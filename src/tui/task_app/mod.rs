//! The new task-based application (U8): the list, the header, the event
//! stream, refreshing and moving tasks.
//!
//! It is built next to the old [`super::app`] and [`super::remote`] and takes
//! the module name over in U14, when `tui::run` switches to it. The state it
//! holds is what [`super::remote::RemoteApp`] held — the tasks, a selection
//! that follows a `task_id`, the stream cursor — but the connection is the
//! [`super::connection::Connection`] of U7 (live / retrying / down, the age of
//! the data, the gate on mutations) and every key goes through
//! [`super::keymap`], every colour through [`super::style`].
//!
//! # The pieces
//!
//! * [`TaskApp`] — all it knows, and what a key or an event does to it. It
//!   never touches a terminal, which is why `tests/tui_state.rs` drives it.
//! * [`list`] — the tasks in server order, how events patch a row, how a row
//!   reads, the move mode.
//! * [`layout`] — the three layouts and the frame (header, details, bar).
//! * [`detail`] and [`tabs`] — the details of the selected task: the `display`
//!   header and the tabs, what they remember per task and how they read.
//! * [`notify`] — the notification on the bar.
//! * [`EventFeed`] — the control-plane event stream, resumed from the cursor
//!   of the last event applied.
//!
//! # Keys → request → perform
//!
//! Like the old modes: [`TaskApp::on_key`] folds a key into the state and says
//! what needs the API ([`Request`]); [`TaskApp::perform`] does it. A loop,
//! given a terminal, is this and nothing more:
//!
//! ```text
//! select! {
//!     key   => { let request = app.on_key(key); app.perform(request).await; }
//!     event = feed.next() => app.on_stream(event),
//!     _ = tick => {}
//! }
//! feed.ensure_open(&app);            // once a read gave it a cursor
//! app.refresh_if_due(Instant::now()).await;
//! app.sync_detail().await;           // what the visible tab of the selected task lacks
//! terminal.draw(|frame| layout::render(frame, &mut app, &styles, Instant::now()))
//! ```
//!
//! # For U9–U13
//!
//! * **Tabs and details (U9)** are in: [`detail::render`] draws into
//!   [`layout::Areas::detail`], the selected task is
//!   [`TaskApp::list`]`.selected()`, and the tab and what each task's tabs
//!   remember (cursors, the followed step, scroll, what was read) are in
//!   [`DetailState`] ([`DetailState::view`]). Keys reach the tabs by the
//!   `Context::Details` arm of [`TaskApp::on_key`]. *Dyrygent* (U12) is a tab
//!   that says it is coming: give it a state in [`detail::TaskDetail`], a
//!   module in [`tabs`], and replace its arm in [`detail::render`], its arm in
//!   [`detail::TaskDetail::on_action`] and its read in
//!   [`TaskApp::sync_detail`].
//! * **The answer panel (U10)** is a part of the details; the list already
//!   leads to it (`Space` → [`list::TaskList::select_next_waiting`], `Enter`
//!   → [`Focus::Details`]). A draft belongs in [`DetailState::set_draft`]. The
//!   panel's place is [`detail::areas`]`(area, panel_height).panel`: pass its
//!   height from [`detail::render`] and the tab content gives up the rows; the
//!   keys of [`keymap::Context::Panel`] apply while it has the focus (`Tab` is
//!   [`Action::SwitchFocus`], ignored until then).
//! * **The action menu (U11)** is an overlay like the help: a new field next
//!   to `help_open`, a new arm in [`TaskApp::key_context`]. A mutation must
//!   first pass [`TaskApp::mutation_allowed`], and send its own
//!   [`idempotency_key`].
//! * **The new-task form (U13)** takes [`layout::Areas::detail`]; a task it
//!   sent shows as an [`list::StateIcon::Sending`] row.

pub mod detail;
pub mod layout;
pub mod list;
pub mod notify;
pub mod tabs;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crossterm::event::KeyEvent;
use futures::stream::{BoxStream, StreamExt as _};

use crate::api::{ApiClient, ClientError};
use crate::controller_protocol::{
    CapabilitiesView, ControlEventEnvelope, MoveTaskRequest, TaskListResponse,
};
use crate::orchestration::{MoveTarget, TaskLifecycle};

use super::connection::Connection;
use super::keymap::{self, Action, Context};
use super::refresh::RefreshSchedule;
use super::scroll::sanitize;
use detail::TaskDetail;
use list::{title_of, MoveRequest, Removal, TaskList};
use notify::{Notifier, Severity};

/// Where the keyboard is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// On the task list.
    List,
    /// In the details of the selected task.
    Details,
}

/// The tabs of the details (origin §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailTab {
    /// The step timeline and the selected step.
    Progress,
    /// The agent's conversation.
    Conversation,
    /// The raw event log.
    Events,
    /// The conductor's chat and decisions.
    Conductor,
    /// Sealed inputs, workflow, workspace, runner, controller.
    Info,
}

impl DetailTab {
    /// Every tab, left to right.
    pub const ALL: [DetailTab; 5] = [
        DetailTab::Progress,
        DetailTab::Conversation,
        DetailTab::Events,
        DetailTab::Conductor,
        DetailTab::Info,
    ];

    /// The name on the tab.
    pub fn title(self) -> &'static str {
        match self {
            Self::Progress => "Przebieg",
            Self::Conversation => "Rozmowa",
            Self::Events => "Zdarzenia",
            Self::Conductor => "Dyrygent",
            Self::Info => "Info",
        }
    }

    /// The tab `by` places along, wrapping round.
    pub fn shifted(self, by: isize) -> Self {
        let at = Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0);
        let count = Self::ALL.len() as isize;
        Self::ALL[(at as isize + by).rem_euclid(count) as usize]
    }
}

/// What the details remember, which a refresh must not touch: the tab, and a
/// draft per task.
#[derive(Debug, Clone)]
pub struct DetailState {
    tab: DetailTab,
    drafts: BTreeMap<String, String>,
    views: BTreeMap<String, TaskDetail>,
    hyperlinks: bool,
}

impl DetailState {
    fn new() -> Self {
        Self {
            tab: DetailTab::Progress,
            drafts: BTreeMap::new(),
            views: BTreeMap::new(),
            hyperlinks: false,
        }
    }

    /// What the details of `task_id` remember, once they were shown or read.
    pub fn view(&self, task_id: &str) -> Option<&TaskDetail> {
        self.views.get(task_id)
    }

    /// What the details of `task_id` remember, created empty when there is
    /// nothing yet. The cursors, the tab state and the following of every task
    /// live here, so they come back with the task.
    pub fn view_mut(&mut self, task_id: &str) -> &mut TaskDetail {
        self.views
            .entry(task_id.to_string())
            .or_insert_with(|| TaskDetail::new(task_id))
    }

    /// Whether links in the header are written as OSC 8 hyperlinks.
    pub fn hyperlinks(&self) -> bool {
        self.hyperlinks
    }

    /// Writes the header's links as OSC 8 hyperlinks (or stops doing it). Off
    /// until the loop has asked the terminal's environment
    /// ([`detail::hyperlinks_supported`]).
    pub fn set_hyperlinks(&mut self, enabled: bool) {
        self.hyperlinks = enabled;
    }

    /// Makes what every task's details hold due for a new read; what is shown,
    /// and every cursor, stays.
    fn invalidate(&mut self) {
        for view in self.views.values_mut() {
            view.invalidate();
        }
    }

    /// Forgets the details of tasks that are no longer on the list.
    fn retain(&mut self, tasks: &[crate::controller_protocol::TaskSummary]) {
        self.views
            .retain(|id, _| tasks.iter().any(|task| task.task_id.as_str() == id));
    }

    /// The tab shown.
    pub fn tab(&self) -> DetailTab {
        self.tab
    }

    /// Shows `tab`.
    pub fn set_tab(&mut self, tab: DetailTab) {
        self.tab = tab;
    }

    /// What the operator has typed for `task_id` and not sent.
    pub fn draft(&self, task_id: &str) -> Option<&str> {
        self.drafts.get(task_id).map(String::as_str)
    }

    /// Keeps `text` as the draft for `task_id`; an empty one forgets it.
    pub fn set_draft(&mut self, task_id: &str, text: impl Into<String>) {
        let text = text.into();
        if text.is_empty() {
            self.drafts.remove(task_id);
        } else {
            self.drafts.insert(task_id.to_string(), text);
        }
    }

    /// Whether any draft is unsent: quitting then asks first.
    pub fn has_drafts(&self) -> bool {
        !self.drafts.is_empty()
    }
}

/// What drawing the details needs, borrowed from the application.
pub(super) struct DetailView<'a> {
    pub(super) task: &'a crate::controller_protocol::TaskSummary,
    pub(super) queue_place: Option<usize>,
    pub(super) tab: DetailTab,
    pub(super) detail: &'a mut TaskDetail,
}

/// What a key asked for that needs the API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Nothing.
    None,
    /// Put a queued task elsewhere in the queue.
    Move(MoveRequest),
}

/// A fresh key for a command that must not run twice (an owner command, an
/// answer): the server treats a repeat of a key as the same command.
pub fn idempotency_key() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Everything the interface knows.
pub struct TaskApp {
    client: ApiClient,
    list: TaskList,
    connection: Connection,
    notifier: Notifier,
    schedule: RefreshSchedule,
    cursor: Option<String>,
    capabilities: Option<CapabilitiesView>,
    focus: Focus,
    detail: DetailState,
    help_open: bool,
    quit: bool,
    /// Failed reconnects in a row, to estimate the countdown in the header.
    stream_failures: u32,
    /// The stream ended for good: no reconnect will come.
    stream_ended: bool,
}

impl TaskApp {
    /// An application over `client`, called `connection_name` until the
    /// server says its own. Nothing is read until [`Self::refresh`].
    pub fn new(
        client: ApiClient,
        connection_name: impl Into<String>,
        refresh_interval: Option<Duration>,
    ) -> Self {
        Self {
            client,
            list: TaskList::new(),
            connection: Connection::new(clean(&connection_name.into())),
            notifier: Notifier::default(),
            schedule: RefreshSchedule::new(refresh_interval, Instant::now()),
            cursor: None,
            capabilities: None,
            focus: Focus::List,
            detail: DetailState::new(),
            help_open: false,
            quit: false,
            stream_failures: 0,
            stream_ended: false,
        }
    }

    /// The client, for opening the event stream.
    pub fn client(&self) -> &ApiClient {
        &self.client
    }

    /// The task list.
    pub fn list(&self) -> &TaskList {
        &self.list
    }

    /// The task list, for drawing.
    pub fn list_mut(&mut self) -> &mut TaskList {
        &mut self.list
    }

    /// The connection to the backend.
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// The notification on the bar.
    pub fn notifier(&self) -> &Notifier {
        &self.notifier
    }

    /// What the details remember.
    pub fn detail(&self) -> &DetailState {
        &self.detail
    }

    /// What the details remember, to change it.
    pub fn detail_mut(&mut self) -> &mut DetailState {
        &mut self.detail
    }

    /// What the server offers, once read.
    pub fn capabilities(&self) -> Option<&CapabilitiesView> {
        self.capabilities.as_ref()
    }

    /// Where the keyboard is.
    pub fn focus(&self) -> Focus {
        self.focus
    }

    /// The stream cursor of the state shown: the last event applied, or the
    /// last read. A stream resumes from it.
    pub fn cursor(&self) -> Option<&str> {
        self.cursor.as_deref()
    }

    /// Whether the stream ended for good.
    pub fn stream_ended(&self) -> bool {
        self.stream_ended
    }

    /// Whether the interface should be left.
    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// Whether the `?` help is open.
    pub fn is_help_open(&self) -> bool {
        self.help_open
    }

    /// Whether an event asked for a read that has not run yet.
    pub fn refresh_wanted(&self) -> bool {
        self.schedule.is_requested()
    }

    /// When the next read is due: the debounce after an event, the period, or
    /// the retry after a failure.
    pub fn next_refresh_due(&self) -> Option<Instant> {
        self.schedule.next_due()
    }

    /// The context whose keys apply now.
    pub fn key_context(&self) -> Context {
        layout::context_of(self.focus, &self.list)
    }

    /// Whether an action that changes state may run now; otherwise the reason
    /// goes to the bar. Every mutation asks this first (R18).
    pub fn mutation_allowed(&mut self) -> bool {
        match self.connection.mutation_gate(Instant::now()) {
            Ok(()) => true,
            Err(blocked) => {
                self.notify(Severity::Warning, blocked.reason);
                false
            }
        }
    }

    fn notify(&mut self, severity: Severity, text: impl AsRef<str>) {
        self.notifier.push(severity, text, Instant::now());
    }

    // -----------------------------------------------------------------
    // Keys
    // -----------------------------------------------------------------

    /// Folds a key into the state. Returns what needs the API.
    pub fn on_key(&mut self, key: KeyEvent) -> Request {
        if self.help_open {
            self.help_open = false;
            return Request::None;
        }
        let context = self.key_context();
        let Some(action) = keymap::lookup(context, &key) else {
            return Request::None;
        };
        match action {
            Action::Help => self.help_open = true,
            Action::Quit => self.quit = true,
            Action::Down | Action::Up | Action::Top | Action::Bottom => {
                self.on_cursor(context, action)
            }
            Action::PageDown
            | Action::PageUp
            | Action::Expand
            | Action::RawEntry
            | Action::RawAll
            | Action::Follow
                if context == Context::Details =>
            {
                self.on_detail_action(action)
            }
            Action::NextWaiting => {
                if !self.list.select_next_waiting() {
                    self.notify(Severity::Warning, "żadne zadanie nie czeka na Ciebie");
                }
            }
            Action::Open => {
                if self.list.selected().is_some() {
                    self.focus = Focus::Details;
                }
            }
            Action::Back => self.focus = Focus::List,
            Action::ToggleDone => self.list.toggle_hide_done(),
            Action::PrevTab => self.detail.tab = self.detail.tab.shifted(-1),
            Action::NextTab => self.detail.tab = self.detail.tab.shifted(1),
            Action::MoveUp => return self.step_move(true),
            Action::MoveDown => return self.step_move(false),
            Action::MoveMode => self.start_move_mode(),
            Action::Confirm if context == Context::Move => return self.finish_move_mode(),
            Action::Cancel if context == Context::Move => self.list.cancel_move(),
            // Keys of the units to come: the menu, the panel, the forms.
            _ => {}
        }
        Request::None
    }

    fn on_cursor(&mut self, context: Context, action: Action) {
        match (context, action) {
            (Context::Move, Action::Down) => self.list.step_move(1),
            (Context::Move, Action::Up) => self.list.step_move(-1),
            (Context::List, Action::Down) => self.list.step(1),
            (Context::List, Action::Up) => self.list.step(-1),
            (Context::List, Action::Top) => self.list.first(),
            (Context::List, Action::Bottom) => self.list.last(),
            (Context::Details, _) => self.on_detail_action(action),
            _ => {}
        }
    }

    /// A key in the details: handed to the tab shown, with the task's own
    /// remembered state.
    fn on_detail_action(&mut self, action: Action) {
        let Some(id) = self.list.selected_id().map(str::to_string) else {
            return;
        };
        let tab = self.detail.tab;
        self.detail.view_mut(&id).on_action(tab, action);
    }

    /// The selected task with the state its details remember, split so that
    /// drawing can change the one while it reads the other.
    pub(super) fn detail_view(&mut self) -> Option<DetailView<'_>> {
        let task = self.list.selected()?;
        let queue_place = self.list.queue_position(task.task_id.as_str());
        let tab = self.detail.tab;
        let detail = self
            .detail
            .views
            .entry(task.task_id.to_string())
            .or_insert_with(|| TaskDetail::new(task.task_id.as_str()));
        Some(DetailView {
            task,
            queue_place,
            tab,
            detail,
        })
    }

    /// Whether the selected task can be moved; otherwise says why.
    fn movable_selection(&mut self) -> bool {
        let Some(task) = self.list.selected() else {
            return false;
        };
        if task.lifecycle != TaskLifecycle::Queued {
            self.notify(Severity::Warning, "przesuń: zadanie nie jest w kolejce");
            return false;
        }
        self.mutation_allowed()
    }

    /// `K` / `J`: one place up or down, relative to the neighbouring row.
    fn step_move(&mut self, up: bool) -> Request {
        if !self.movable_selection() {
            return Request::None;
        }
        let Some(neighbour) = self.list.neighbour(up) else {
            let which = if up { "pierwsze" } else { "ostatnie" };
            self.notify(
                Severity::Warning,
                format!("przesuń: to zadanie jest już {which} w kolejce"),
            );
            return Request::None;
        };
        let anchor = neighbour.task_id.clone();
        let anchor_title = title_of(neighbour);
        let task = self.list.selected().expect("a neighbour implies a selection");
        Request::Move(MoveRequest {
            task_id: task.task_id.to_string(),
            title: title_of(task),
            generation: task.generation,
            target: if up {
                MoveTarget::Before(anchor)
            } else {
                MoveTarget::After(anchor)
            },
            anchor_title,
        })
    }

    /// `m`: previews a place with `j`/`k`.
    fn start_move_mode(&mut self) {
        if self.movable_selection() && !self.list.start_move() {
            self.notify(Severity::Warning, "przesuń: zadanie nie jest na liście");
        }
    }

    /// `Enter` in the move mode.
    fn finish_move_mode(&mut self) -> Request {
        if !self.mutation_allowed() {
            self.list.cancel_move();
            return Request::None;
        }
        match self.list.finish_move() {
            Some(request) => Request::Move(request),
            None => Request::None,
        }
    }

    // -----------------------------------------------------------------
    // API
    // -----------------------------------------------------------------

    /// Carries a request out.
    pub async fn perform(&mut self, request: Request) {
        match request {
            Request::None => {}
            Request::Move(request) => self.move_task(request).await,
        }
    }

    /// Sends a move. The new order is not applied here: the server announces
    /// it as a `Moved` event, which patches the list. A move the server
    /// refuses (another client moved, the task was admitted, the revision
    /// changed) reloads the list and says why.
    async fn move_task(&mut self, request: MoveRequest) {
        let body = MoveTaskRequest {
            target: request.target.clone(),
            expected_generation: Some(request.generation),
        };
        match self.client.control_move_task(&request.task_id, &body).await {
            Ok(_) => {
                let how = match &request.target {
                    MoveTarget::Before(_) => "przed",
                    MoveTarget::After(_) => "za",
                    MoveTarget::Top => "na początek",
                    MoveTarget::Bottom => "na koniec",
                };
                let anchor = match &request.target {
                    MoveTarget::Before(_) | MoveTarget::After(_) => {
                        format!(" {}", clean(&request.anchor_title))
                    }
                    _ => String::new(),
                };
                self.notify(
                    Severity::Success,
                    format!("{}: przesunięto {how}{anchor}", clean(&request.title)),
                );
            }
            Err(error) => {
                self.note_transport(&error);
                let reloaded = !is_transport(&error);
                if reloaded {
                    // First the read, so the reason is the last word on the
                    // bar, not the notice of a row that left the list.
                    self.refresh().await;
                }
                let tail = if reloaded { "; lista wczytana na nowo" } else { "" };
                self.notify(
                    Severity::Error,
                    format!(
                        "{}: przesunięcie odrzucone — {}{tail}",
                        clean(&request.title),
                        move_failure_reason(&error)
                    ),
                );
            }
        }
    }

    /// Re-reads the task list (and the server's name, once). The selection,
    /// the focus, the tab, the drafts and the move mode are not touched; the
    /// selection stays on its `task_id`.
    pub async fn refresh(&mut self) {
        let now = Instant::now();
        match self.client.control_tasks_archived(false).await {
            Ok(listing) => {
                self.load(listing);
                if self.capabilities.is_none() {
                    self.read_capabilities().await;
                }
            }
            Err(error) => self.on_read_failure(&error, now),
        }
    }

    /// Reads a due refresh. Returns whether one ran.
    pub async fn refresh_if_due(&mut self, now: Instant) -> bool {
        if !self.schedule.is_due(now) {
            return false;
        }
        self.refresh().await;
        true
    }

    async fn read_capabilities(&mut self) {
        if let Ok(capabilities) = self.client.control_capabilities().await {
            self.connection.rename(clean(&capabilities.connection_name));
            self.capabilities = Some(capabilities);
        }
    }

    /// Takes a read of the task list in: the tasks, the cursor to stream
    /// from, a live connection.
    pub fn load(&mut self, listing: TaskListResponse) {
        let now = Instant::now();
        self.cursor = Some(listing.cursor);
        self.connection.on_data(now);
        self.schedule.done(now);
        self.stream_failures = 0;
        let removal = self.list.replace(listing.tasks);
        self.detail.retain(self.list.tasks());
        self.detail.invalidate();
        self.announce(removal);
    }

    fn announce(&mut self, removal: Option<Removal>) {
        let Some(removal) = removal else {
            return;
        };
        let moved_to = removal
            .now_selected
            .as_deref()
            .and_then(|id| {
                self.list
                    .tasks()
                    .iter()
                    .find(|task| task.task_id.as_str() == id)
            })
            .map(|task| format!(" — zaznaczono {}", clean(&title_of(task))))
            .unwrap_or_default();
        self.notify(
            Severity::Warning,
            format!(
                "{}: zniknęło z listy (zarchiwizowane){moved_to}",
                clean(&removal.title)
            ),
        );
    }

    /// A read failed. A server that has never answered keeps the "łączę z …"
    /// state; one that has, shows the retry countdown; a refusal a retry will
    /// not fix shows as no connection.
    fn on_read_failure(&mut self, error: &ClientError, now: Instant) {
        let delay = self.schedule.fail(now);
        self.mark_failure(error, now, delay);
    }

    fn mark_failure(&mut self, error: &ClientError, now: Instant, delay: Duration) {
        if !is_transport(error) {
            self.connection.on_failure(now, None);
        } else if self.connection.data_age(now).is_some() {
            self.connection.on_failure(now, Some(delay));
        }
    }

    /// A mutation that failed to reach the server counts as a failed read.
    fn note_transport(&mut self, error: &ClientError) {
        if is_transport(error) {
            let now = Instant::now();
            let delay = self.schedule.fail(now);
            self.mark_failure(error, now, delay);
        }
    }

    /// Takes one item of the event stream in.
    ///
    /// An event patches the list at once ([`TaskList::apply_event`]) and moves
    /// the cursor; what it cannot say asks for a read, which waits
    /// [`super::refresh::DEBOUNCE`] so a burst costs one. An error is the
    /// stream dropping — it reconnects by itself from its cursor — and the
    /// header says so; one a retry will not fix ends the stream.
    pub fn on_stream(&mut self, item: Option<Result<ControlEventEnvelope, ClientError>>) {
        let now = Instant::now();
        match item {
            Some(Ok(envelope)) => {
                self.connection.on_data(now);
                self.stream_failures = 0;
                let effect = self.list.apply_event(&envelope.event);
                if effect.applied {
                    self.cursor = Some(envelope.cursor);
                }
                if effect.refresh {
                    self.schedule.request(now);
                }
                self.announce(effect.removal);
            }
            Some(Err(error)) if is_transport(&error) => {
                self.stream_failures = self.stream_failures.saturating_add(1);
                let delay = reconnect_estimate(self.stream_failures);
                self.mark_failure(&error, now, delay);
            }
            Some(Err(error)) => {
                self.stream_ended = true;
                self.connection.on_failure(now, None);
                self.notify(Severity::Error, format!("strumień zdarzeń zakończony: {error}"));
            }
            None => {
                self.stream_ended = true;
                self.connection.on_failure(now, None);
            }
        }
    }
}

/// The delay the client's own reconnect backoff has after `failures` failed
/// attempts, for the header's countdown. Mirrors the client's schedule
/// (250ms doubling to 15s); the stream does not report its own.
fn reconnect_estimate(failures: u32) -> Duration {
    let factor = 2u32.saturating_pow(failures.saturating_sub(1).min(16));
    Duration::from_millis(250)
        .saturating_mul(factor)
        .min(Duration::from_secs(15))
}

/// Whether `error` is the server being unreachable or unwell, which a retry
/// can fix, as opposed to a refusal.
fn is_transport(error: &ClientError) -> bool {
    matches!(
        error,
        ClientError::Unavailable(_) | ClientError::Server(_) | ClientError::Degraded { .. }
    )
}

/// Why the server refused a move, in words for the notification.
pub fn move_failure_reason(error: &ClientError) -> String {
    match error {
        ClientError::Refused { message, .. }
        | ClientError::Request { message, .. }
        | ClientError::Degraded { message, .. } => clean(message),
        ClientError::Conflict => "zadanie zmieniło się w międzyczasie (409)".to_string(),
        ClientError::NotFound => "nie ma takiego zadania".to_string(),
        ClientError::Unavailable(_) => "brak połączenia z serwerem".to_string(),
        other => clean(&other.to_string()),
    }
}

/// Server text on one line, safe for a terminal.
fn clean(text: &str) -> String {
    sanitize(text).replace('\n', " ")
}

/// The control-plane event stream, resumed from the cursor of the last event
/// the application applied.
///
/// Held by the loop, apart from [`TaskApp`], so that waiting for the next
/// event does not borrow the application. It opens once the application has a
/// cursor — an unreachable server at start means no read, so no cursor, so no
/// stream yet — and [`Self::close`] drops it, as a broken connection would: the
/// next [`Self::ensure_open`] resumes from the application's cursor, so no
/// event (a `Moved` among them) is lost. Reconnecting after a drop *inside* the
/// stream is [`ApiClient::follow_control_events`]'s own business.
pub struct EventFeed {
    client: ApiClient,
    stream: Option<BoxStream<'static, Result<ControlEventEnvelope, ClientError>>>,
}

impl EventFeed {
    /// A feed over `client`, not yet open.
    pub fn new(client: ApiClient) -> Self {
        Self {
            client,
            stream: None,
        }
    }

    /// Opens the stream from the application's cursor when it is closed, the
    /// application has a cursor and the stream did not end for good.
    pub fn ensure_open(&mut self, app: &TaskApp) {
        if self.stream.is_some() || app.stream_ended() {
            return;
        }
        if let Some(cursor) = app.cursor() {
            self.stream = Some(
                self.client
                    .follow_control_events(Some(cursor.to_string()), None)
                    .boxed(),
            );
        }
    }

    /// Whether the stream is open.
    pub fn is_open(&self) -> bool {
        self.stream.is_some()
    }

    /// Drops the stream, as a broken connection does.
    pub fn close(&mut self) {
        self.stream = None;
    }

    /// The next item. Never resolves while the stream is closed, so it can sit
    /// in a `select!` arm.
    pub async fn next(&mut self) -> Option<Result<ControlEventEnvelope, ClientError>> {
        match self.stream.as_mut() {
            Some(stream) => stream.next().await,
            None => std::future::pending().await,
        }
    }
}
