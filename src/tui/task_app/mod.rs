//! The new task-based application (U8): the list, the header, the event
//! stream, refreshing and moving tasks; the action menu (U11).
//!
//! The only interface of `narsor tui` (U14): the tasks of the control plane
//! (`/control/v1`) of the backend the configuration names, a selection that
//! follows a `task_id`, the stream cursor, and the
//! [`super::connection::Connection`] (live / retrying / down, the age of the
//! data, the gate on mutations). Every key goes through [`super::keymap`],
//! every colour through [`super::style`].
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
//! [`TaskApp::on_key`] folds a key into the state and says
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
//! app.retry_outgoing_if_due(Instant::now()).await; // a new task whose answer was lost
//! app.sync_detail().await;           // what the visible tab of the selected task lacks
//! // the tick also wakes at `app.next_refresh_due()` and `app.next_outgoing_due()`
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
//!   `Context::Details` arm of [`TaskApp::on_key`].
//! * **The answer panel (U10)** is a part of the details; the list already
//!   leads to it (`Space` → [`list::TaskList::select_next_waiting`], `Enter`
//!   → [`Focus::Details`]). A draft belongs in [`DetailState::set_draft`]. The
//!   panel's place is
//!   [`detail::areas`]`(area, header_lines, panel_height).panel`: pass its
//!   height from [`detail::render`] and the tab content gives up the rows; the
//!   keys of [`keymap::Context::Panel`] apply while it has the focus (`Tab` is
//!   [`Action::SwitchFocus`], ignored until then).
//! * **The action menu (U11)** is in: [`actions`]. `.` opens it, every action
//!   has a shortcut, and [`actions::availability`] decides — for the menu, the
//!   shortcuts and the bar — whether an action can be done and why not. What
//!   sits over the screen is an [`Overlay`] in the field next to `help_open`;
//!   it takes the keys first ([`TaskApp::key_context`], [`TaskApp::on_key`]).
//!   An action `!` waits in the `y` / `n` [`actions::Confirmation`]. A new
//!   mutation must first pass [`TaskApp::mutation_allowed`], and send its own
//!   [`idempotency_key`]; the verdict of an operator command comes back as an
//!   event and ends on the bar ([`list::CommandResolved`]).
//! * **The *Dyrygent* tab (U12)** is in: [`tabs::conductor`]. Its chat, its
//!   decisions and its harness go through [`Request::Act`] like any operator
//!   command (a message is [`actions::ActionSend::Operator`] with
//!   `ConductorChat`), so they pass [`TaskApp::mutation_allowed`] and carry an
//!   [`idempotency_key`]. What is typed in its text box is a draft
//!   ([`detail::TaskDetail::draft_text`]), and while typing
//!   [`TaskApp::key_context`] is [`Context::Form`].
//! * **The new-task form, the history and the repositories (U13)** are in:
//!   [`new_task`], [`history`], [`repositories`]. `n`, `H` and `R` ask for a
//!   read ([`Request::OpenNewTask`], [`Request::OpenHistory`],
//!   [`Request::OpenRepositories`]) and what they open is the application's
//!   [`View`]: while one is open it takes the keys ([`TaskApp::key_context`]),
//!   the details area (the history: the list as well) and `Esc` gives the task
//!   list back as it was. A task the form sent is a [`list::PendingRow`] —
//!   `↻ wysyłanie…`, `✗ nie wysłano` — until the server lists it; the loop sends
//!   again what a lost answer left ([`TaskApp::retry_outgoing_if_due`],
//!   [`TaskApp::next_outgoing_due`]). A mutation of these views passes
//!   [`TaskApp::mutation_allowed`] like every other, and what the operator
//!   typed is a draft ([`DetailState::set_draft`]), so `q` asks first.

pub mod actions;
pub mod detail;
pub mod harness;
pub mod history;
pub mod layout;
pub mod list;
pub mod new_task;
pub mod notify;
pub mod repositories;
pub mod tabs;
pub mod waiting;

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use crossterm::event::KeyEvent;
use futures::stream::{BoxStream, StreamExt as _};

use crate::api::{reconnect_delay, ApiClient, ClientError, HarnessSelectionSnapshot};
use crate::controller_protocol::{
    CapabilitiesView, ControlEventEnvelope, CreateRepositoryRequest, MoveTaskRequest,
    TaskListResponse,
};
use narsor_protocol::controller::MoveTaskTarget;

use super::connection::Connection;
use super::keymap::{self, Action, Context, Stack};
use super::refresh::RefreshSchedule;
use super::scroll::one_line;
use actions::{availability, ActionRequest, Availability, LinkRequest, Overlay, TaskAction};
use detail::{StepLevel, TaskDetail};
use history::History;
use list::{title_of, MoveRequest, Removal, ResumeNotice, ResumeNoticeKind, TaskList};
use new_task::{NewTaskForm, Outgoing};
use notify::{Notifier, Severity};
use repositories::Repositories;
use waiting::AnswerRequest;

/// Where the keyboard is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// On the task list.
    List,
    /// In the details of the selected task.
    Details,
}

/// Where the keys go on *Przebieg* (R1): the timeline, or the conversation of
/// the step it picked. Every other tab has one level, so this is
/// [`StepFocus::Axis`] there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StepFocus {
    /// The timeline: the keys pick the step.
    #[default]
    Axis,
    /// The selected step's conversation: the keys read it, and the timeline
    /// holds its step ([`tabs::progress::Progress::is_paused`]).
    Conversation,
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

/// The keys over the details `base` (the task's or the history's) while `tab`
/// is shown: on *Przebieg* the timeline's or the step conversation's, as
/// `step` says; reading on every other tab.
fn detail_keys(base: Context, tab: DetailTab, step: StepFocus) -> Stack {
    let layer = match (tab, step, base) {
        (DetailTab::Progress, StepFocus::Conversation, _) => Context::StepConversation,
        (DetailTab::Progress, StepFocus::Axis, Context::HistoryDetails) => Context::HistoryAxis,
        (DetailTab::Progress, StepFocus::Axis, _) => Context::Axis,
        (DetailTab::Conversation, _, _) => Context::AgentConversation,
        _ => Context::Reading,
    };
    Stack::layered(layer, base)
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

    /// Makes the details of every task due for a new read, keeping what is
    /// shown and every cursor. The task's own reads (timeline, conversation,
    /// events) are made due only when its revision differs from the one in
    /// `before`; the conductor's chat can grow without it, so it is always read
    /// again.
    fn invalidate_changed(
        &mut self,
        tasks: &[crate::controller_protocol::TaskSummary],
        before: &HashMap<String, u64>,
    ) {
        for task in tasks {
            let id = task.task_id.as_str();
            let Some(view) = self.views.get_mut(id) else {
                continue;
            };
            if before.get(id) != Some(&task.revision) {
                view.invalidate_task_reads();
            }
            view.invalidate_conductor_reads();
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

/// What the details area shows, apart from the selected task: the screens
/// `n`, `H` and `R` open. Each takes the keys while it is open and `Esc` gives
/// the list of tasks back as it was.
#[derive(Debug)]
pub enum View {
    /// The list of tasks and the details of the selected one.
    Tasks,
    /// The new-task form, over the details.
    NewTask(Box<NewTaskForm>),
    /// The history: its own list and its read-only details.
    History(Box<History>),
    /// The registered repositories, over the details.
    Repositories(Box<Repositories>),
}

/// Which [`View`] is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewKind {
    /// The tasks.
    Tasks,
    /// The new-task form.
    NewTask,
    /// The history.
    History,
    /// The repositories.
    Repositories,
}

/// What a key asked for that needs the API.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// Nothing.
    None,
    /// `n`: read the catalogue and open the new-task form.
    OpenNewTask,
    /// `H`: read the archived tasks and old executions and open the history.
    OpenHistory,
    /// `R`: read the registry and open the repositories.
    OpenRepositories,
    /// Send the task being created under this id (the first attempt, or by
    /// hand once it was not sent).
    SendTask(String),
    /// Check this absolute path of a workflow file with the server.
    ValidatePath(String),
    /// Recover this old execution of the history.
    Recover(String),
    /// Register a repository.
    AddRepository(Box<CreateRepositoryRequest>),
    /// Remove the repository with this id.
    RemoveRepository(String),
    /// Put a queued task elsewhere in the queue.
    Move(MoveRequest),
    /// Send what the answer panel's operator chose.
    Answer(Box<AnswerRequest>),
    /// Send an action of the menu, confirmed where it asks to be.
    Act(Box<ActionRequest>),
    /// Point the browser at a link of the task.
    OpenLink(LinkRequest),
    /// `h`: read the default harness and open the list to change it.
    OpenHarness,
    /// Make this harness the installation's default; `None` goes back to
    /// the configured one.
    SetHarness(Option<String>),
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
    /// The daemon's default harness, as last read (U5).
    harness_selection: Option<HarnessSelectionSnapshot>,
    /// The harness a change sent and the daemon has not answered yet.
    harness_pending: Option<String>,
    focus: Focus,
    detail: DetailState,
    help_open: bool,
    /// What sits over the screen and takes the keys: the action menu, a list
    /// of exits or links, the `y` / `n` window.
    overlay: Option<Overlay>,
    /// The keys of actions that got no answer, by decision: a retry of the
    /// same decision reuses its key.
    action_keys: BTreeMap<String, String>,
    /// The keyboard is on the answer panel at the bottom of the details.
    panel_focus: bool,
    /// The level of the focus on *Przebieg*, kept while the panel, a menu or
    /// the help take the keys and back to the timeline whenever the details
    /// show something else ([`Self::back_to_axis`]).
    step_focus: StepLevel,
    quit: bool,
    /// `q` found an unsent draft and asked; the next `q` leaves.
    quit_confirm: bool,
    /// What the details area shows (U13).
    view: View,
    /// Where the keyboard was when a view opened, to give back on `Esc`.
    previous_focus: Focus,
    /// What was typed in the new-task form and left with `Esc`.
    new_task_memory: Option<NewTaskForm>,
    /// Tasks being created: what each attempt sends, under the task's id.
    outbox: Vec<Outgoing>,
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
            connection: Connection::new(one_line(&connection_name.into())),
            notifier: Notifier::default(),
            schedule: RefreshSchedule::new(refresh_interval, Instant::now()),
            cursor: None,
            capabilities: None,
            harness_selection: None,
            harness_pending: None,
            focus: Focus::List,
            detail: DetailState::new(),
            help_open: false,
            overlay: None,
            action_keys: BTreeMap::new(),
            panel_focus: false,
            step_focus: StepLevel::default(),
            quit: false,
            quit_confirm: false,
            view: View::Tasks,
            previous_focus: Focus::List,
            new_task_memory: None,
            outbox: Vec::new(),
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

    /// Takes what the server offers in, as the first read does: the name of
    /// the connection and what the new-task form may offer.
    pub fn set_capabilities(&mut self, capabilities: CapabilitiesView) {
        self.connection
            .rename(one_line(&capabilities.connection_name));
        self.capabilities = Some(capabilities);
    }

    /// Where the keyboard is.
    pub fn focus(&self) -> Focus {
        self.focus
    }

    /// Whether the keys work the task list: it has the focus, or the move
    /// mode is on — that moves the list's row whichever side started it.
    pub fn keys_on_list(&self) -> bool {
        self.focus == Focus::List || self.list.moving().is_some()
    }

    /// The level of the focus on *Przebieg* of the task details: the timeline
    /// or the selected step's conversation. What draws the timeline and the
    /// step's transcript asks it which of them has the keys.
    pub fn step_focus(&self) -> StepFocus {
        self.step_focus.get()
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

    /// Which view is open.
    pub fn view_kind(&self) -> ViewKind {
        match self.view {
            View::Tasks => ViewKind::Tasks,
            View::NewTask(_) => ViewKind::NewTask,
            View::History(_) => ViewKind::History,
            View::Repositories(_) => ViewKind::Repositories,
        }
    }

    /// The keys that apply now: the context, and over the details the layer
    /// of the tab shown.
    pub fn key_context(&self) -> Stack {
        if let Some(overlay) = &self.overlay {
            return overlay.context().into();
        }
        if let Some(context) = self
            .new_task_context()
            .or_else(|| self.repositories_context())
        {
            return context.into();
        }
        if let Some(stack) = self.history_context() {
            return stack;
        }
        if self.panel_focused() {
            // Typing into a field: `q` and `?` are text, not commands.
            return if self.panel_typing() {
                Context::Form
            } else {
                Context::Panel
            }
            .into();
        }
        if self.conductor_typing() {
            return Context::Form.into();
        }
        match layout::context_of(self.focus, &self.list) {
            Context::Details => detail_keys(Context::Details, self.detail.tab, self.step_focus()),
            context => context.into(),
        }
    }

    /// The transcript projection whose shortcuts are active, when focus is
    /// on an agent conversation reader.
    pub fn active_agent_transcript_mode(&self) -> Option<crate::tui::transcript::DisplayMode> {
        use crate::tui::keymap::Context;
        use crate::tui::transcript::DisplayMode;

        let context = self.key_context().layer?;
        let transcript = match (&self.view, context) {
            (View::Tasks, Context::AgentConversation) => {
                let task_id = self.list.selected()?.task_id.as_str();
                self.detail.view(task_id)?.conversation().transcript()
            }
            (View::Tasks, Context::StepConversation) => {
                let task_id = self.list.selected()?.task_id.as_str();
                self.detail.view(task_id)?.step_conversation()?.transcript()
            }
            (View::History(history), Context::AgentConversation) => {
                history.selected_detail()?.conversation().transcript()
            }
            (View::History(history), Context::StepConversation) => {
                history.selected_detail()?.step_conversation()?.transcript()
            }
            _ => return None,
        };
        (transcript.mode() == DisplayMode::Raw).then_some(DisplayMode::Raw)
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
        let confirming_quit = std::mem::take(&mut self.quit_confirm);
        if self.overlay.is_some() {
            return self.on_overlay_key(key, confirming_quit);
        }
        match self.view {
            View::Tasks => {}
            View::NewTask(_) => return self.on_new_task_key(key, confirming_quit),
            View::History(_) => return self.on_history_key(key, confirming_quit),
            View::Repositories(_) => return self.on_repositories_key(key, confirming_quit),
        }
        if self.panel_focused() {
            return self.on_panel_key(key, confirming_quit);
        }
        if self.conductor_typing() {
            return self.on_conductor_typing_key(key);
        }
        let keys = self.key_context();
        let Some(action) = keymap::lookup(keys, &key) else {
            return Request::None;
        };
        if self.active_agent_transcript_mode() == Some(crate::tui::transcript::DisplayMode::Raw)
            && matches!(action, Action::Expand | Action::RawEntry | Action::RawAll)
        {
            return Request::None;
        }
        let context = keys.base;
        match action {
            Action::Help => self.help_open = true,
            Action::Quit => self.on_quit(confirming_quit),
            Action::Down | Action::Up | Action::Top | Action::Bottom => {
                self.on_cursor(context, action)
            }
            Action::PageDown
            | Action::PageUp
            | Action::Expand
            | Action::RawEntry
            | Action::RawAll
            | Action::NextConversationView
            | Action::Follow
                if context == Context::Details =>
            {
                self.on_detail_action(action)
            }
            Action::EnterStep if context == Context::Details => self.enter_step(),
            Action::LeaveStep if context == Context::Details => self.back_to_axis(),
            Action::Compose | Action::UndoDecision | Action::LoadMore
                if context == Context::Details =>
            {
                return self.on_conductor_action(action);
            }
            Action::NextWaiting => {
                if !self.change_list(TaskList::select_next_waiting) {
                    self.notify(Severity::Warning, "żadne zadanie nie czeka na Ciebie");
                }
            }
            Action::Open => self.open_selected(),
            Action::SwitchFocus if context == Context::Details => self.focus_panel(),
            Action::Back => {
                self.focus = Focus::List;
                self.panel_focus = false;
                self.back_to_axis();
            }
            Action::ToggleDone => self.change_list(TaskList::toggle_hide_done),
            Action::PrevTab => self.shift_tab(-1),
            Action::NextTab => self.shift_tab(1),
            Action::MoveUp => return self.step_move(true),
            Action::MoveDown => return self.step_move(false),
            Action::MoveMode => self.start_move_mode(),
            Action::Confirm if context == Context::Move => return self.finish_move_mode(),
            Action::Cancel if context == Context::Move => self.list.cancel_move(),
            Action::ActionMenu => self.open_menu(),
            Action::NewTask => return Request::OpenNewTask,
            Action::History => return Request::OpenHistory,
            Action::Repositories => return Request::OpenRepositories,
            Action::ChooseHarness => return Request::OpenHarness,
            Action::RetrySend => return self.retry_selected(),
            other => {
                if let Some(action) = TaskAction::of_key(other) {
                    return self.invoke(action);
                }
            }
        }
        Request::None
    }

    /// `q`: leaves, unless an unsent draft asks first — then a second `q`
    /// leaves and any other key stays.
    fn on_quit(&mut self, confirmed: bool) {
        if confirmed || !self.detail.has_drafts() {
            self.quit = true;
        } else {
            self.quit_confirm = true;
            self.notify(
                Severity::Warning,
                "niewysłany szkic — q ponownie wychodzi, inny klawisz zostaje",
            );
        }
    }

    fn on_cursor(&mut self, context: Context, action: Action) {
        match (context, action) {
            (Context::Move, Action::Down) => self.list.step_move(1),
            (Context::Move, Action::Up) => self.list.step_move(-1),
            (Context::List, Action::Down) => self.change_list(|list| list.step(1)),
            (Context::List, Action::Up) => self.change_list(|list| list.step(-1)),
            (Context::List, Action::Top) => self.change_list(TaskList::first),
            (Context::List, Action::Bottom) => self.change_list(TaskList::last),
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
        let step = self.step_focus();
        self.detail.view_mut(&id).on_action(tab, step, action);
    }

    /// `[` / `]`: the tab `by` places along, on its timeline.
    fn shift_tab(&mut self, by: isize) {
        self.detail.tab = self.detail.tab.shifted(by);
        self.back_to_axis();
    }

    /// `Enter` / `→` on the timeline: into the selected step's conversation,
    /// when it has one to show; otherwise nothing happens (AE2).
    fn enter_step(&mut self) {
        let Some(id) = self.list.selected_id().map(str::to_string) else {
            return;
        };
        self.step_focus.enter(self.detail.view_mut(&id));
    }

    /// Back on the timeline: `Esc` / `←` in the step's conversation, and
    /// whenever the details stop showing it (another tab, another task, the
    /// list). Every timeline lets its step go, so none stays held by a task
    /// the operator has left.
    fn back_to_axis(&mut self) {
        self.step_focus.back_to_axis(&mut self.detail.views);
    }

    /// Every change of the list that can move its selection goes through
    /// here: another selected task than before starts its details on the
    /// timeline, so the level of the focus never outlives the task it was on.
    fn change_list<T>(&mut self, change: impl FnOnce(&mut TaskList) -> T) -> T {
        let before = self.list.selected_id().map(str::to_string);
        let answer = change(&mut self.list);
        if self.list.selected_id() != before.as_deref() {
            self.back_to_axis();
        }
        answer
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
        if let Availability::Unavailable(reason) = availability(task, TaskAction::Move) {
            let word = actions::verb(task, TaskAction::Move);
            self.notify(Severity::Warning, format!("{word}: {reason}"));
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
        let task = self
            .list
            .selected()
            .expect("a neighbour implies a selection");
        Request::Move(MoveRequest {
            task_id: task.task_id.to_string(),
            title: title_of(task),
            generation: task.generation,
            target: if up {
                MoveTaskTarget::Before(anchor.to_string())
            } else {
                MoveTaskTarget::After(anchor.to_string())
            },
            anchor_title,
        })
    }

    /// `m`: previews a place with `j`/`k`.
    fn start_move_mode(&mut self) {
        if self.movable_selection() {
            self.begin_move();
        }
    }

    /// Enters the move mode on the selected task, which must be movable.
    fn begin_move(&mut self) {
        if !self.list.start_move() {
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
            Request::OpenNewTask => self.read_and_open_new_task().await,
            Request::OpenHistory => self.read_and_open_history().await,
            Request::OpenRepositories => self.read_and_open_repositories().await,
            Request::SendTask(id) => self.send_task(&id).await,
            Request::ValidatePath(path) => self.validate_path(&path).await,
            Request::Recover(id) => self.recover_execution(&id).await,
            Request::AddRepository(request) => self.add_repository(&request).await,
            Request::RemoveRepository(id) => self.remove_repository(&id).await,
            Request::Move(request) => self.move_task(request).await,
            Request::Answer(request) => self.answer(*request).await,
            Request::Act(request) => self.act(*request).await,
            Request::OpenLink(link) => self.open_link(link),
            Request::OpenHarness => self.read_and_open_harness().await,
            Request::SetHarness(harness) => self.change_harness(harness).await,
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
                    MoveTaskTarget::Before(_) => "przed",
                    MoveTaskTarget::After(_) => "za",
                    MoveTaskTarget::Top => "na początek",
                    MoveTaskTarget::Bottom => "na koniec",
                };
                let anchor = match &request.target {
                    MoveTaskTarget::Before(_) | MoveTaskTarget::After(_) => {
                        format!(" {}", request.anchor_title)
                    }
                    _ => String::new(),
                };
                self.notify(
                    Severity::Success,
                    format!("{}: przesunięto {how}{anchor}", request.title),
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
                let tail = if reloaded {
                    "; lista wczytana na nowo"
                } else {
                    ""
                };
                self.notify(
                    Severity::Error,
                    format!(
                        "{}: przesunięcie odrzucone — {}{tail}",
                        request.title,
                        failure_reason(&error)
                    ),
                );
            }
        }
    }

    /// Re-reads the task list (and the server's name, once) and the default
    /// harness, both at once. The selection, the focus, the tab, the drafts
    /// and the move mode are not touched; the selection stays on its
    /// `task_id`. The harness is taken in only with the list, and a failed
    /// read of it keeps the last value known.
    pub async fn refresh(&mut self) {
        let now = Instant::now();
        let (listing, harness) = tokio::join!(
            self.client.control_tasks_archived(false),
            self.client.harness_selection()
        );
        match listing {
            Ok(listing) => {
                self.load(listing);
                if self.capabilities.is_none() {
                    self.read_capabilities().await;
                }
                if let Ok(snapshot) = harness {
                    self.set_harness_selection(snapshot);
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
            self.set_capabilities(capabilities);
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
        // A read that worked is a reason to follow the stream again, if it
        // ended: the feed reopens from the cursor just taken.
        self.stream_ended = false;
        let revisions: HashMap<String, u64> = self
            .list
            .tasks()
            .iter()
            .map(|task| (task.task_id.to_string(), task.revision))
            .collect();
        let removal = self.change_list(|list| list.replace(listing.tasks));
        self.detail.retain(self.list.tasks());
        self.detail
            .invalidate_changed(self.list.tasks(), &revisions);
        self.sync_waiting();
        self.sync_overlay();
        self.settle_outbox();
        self.announce(removal);
    }

    /// Shows `view` with `focus`, closing any overlay. The focus to return to
    /// is remembered unless the same kind of view was already open.
    fn enter_view(&mut self, view: View, focus: Focus) {
        if std::mem::discriminant(&self.view) != std::mem::discriminant(&view) {
            self.previous_focus = self.focus;
        }
        self.focus = focus;
        self.panel_focus = false;
        self.overlay = None;
        self.view = view;
    }

    /// Back to the task list, with the focus the view was entered from.
    /// Returns the view that was left.
    fn leave_view(&mut self) -> View {
        let left = std::mem::replace(&mut self.view, View::Tasks);
        self.focus = self.previous_focus;
        self.panel_focus = false;
        left
    }

    /// Tells what the resume policy did on the bar (R7): a resume as a
    /// warning — several in a row counted in one — and giving up as an
    /// error.
    fn notify_resume(&mut self, resume: &ResumeNotice, now: Instant) {
        match &resume.kind {
            ResumeNoticeKind::Resumed { attempt, limit } => self.notifier.push_counted(
                Severity::Warning,
                "auto-resume",
                if *attempt == 0 {
                    format!(
                        "{}: wznowiono automatycznie po restarcie kontrolera (poza limitem {limit} prób)",
                        resume.title
                    )
                } else {
                    format!(
                        "{}: wznowiono automatycznie po utracie runnera (próba {attempt}/{limit})",
                        resume.title
                    )
                },
                |count| format!("wznowiono automatycznie {count} {}", tasks_word(count)),
                now,
            ),
            ResumeNoticeKind::Exhausted(detail) => self.notifier.push(
                Severity::Error,
                format!(
                    "{}: nie udało się wznowić automatycznie: {detail}",
                    resume.title
                ),
                now,
            ),
        }
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
            .map(|task| format!(" — zaznaczono {}", title_of(task)))
            .unwrap_or_default();
        self.notify(
            Severity::Warning,
            format!(
                "{}: zniknęło z listy (zarchiwizowane){moved_to}",
                removal.title
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
                let effect = self.change_list(|list| list.apply_event(&envelope.event));
                if effect.applied {
                    self.cursor = Some(envelope.cursor);
                }
                if effect.refresh {
                    self.schedule.request(now);
                }
                self.sync_waiting();
                self.sync_overlay();
                if let Some(resolved) = &effect.resolved {
                    self.notify_resolved(resolved);
                }
                if let Some(resume) = &effect.resume {
                    self.notify_resume(resume, now);
                }
                self.announce(effect.removal);
            }
            Some(Err(error)) if is_transport(&error) => {
                self.stream_failures = self.stream_failures.saturating_add(1);
                let delay = reconnect_delay(self.stream_failures);
                self.mark_failure(&error, now, delay);
            }
            Some(Err(error)) => {
                self.stream_ended = true;
                self.connection.on_failure(now, None);
                self.notify(
                    Severity::Error,
                    format!("strumień zdarzeń zakończony: {error}"),
                );
            }
            None => {
                self.stream_ended = true;
                self.connection.on_failure(now, None);
            }
        }
    }
}

/// "zadanie" in the form that follows the number `count`.
fn tasks_word(count: usize) -> &'static str {
    match (count % 10, count % 100) {
        _ if count == 1 => "zadanie",
        (2..=4, tens) if !(12..=14).contains(&tens) => "zadania",
        _ => "zadań",
    }
}

/// Whether `error` is the server being unreachable or unwell, which a retry
/// can fix, as opposed to a refusal.
fn is_transport(error: &ClientError) -> bool {
    matches!(
        error,
        ClientError::Unavailable(_) | ClientError::Server(_) | ClientError::Degraded { .. }
    )
}

/// Why a call to the server failed, in words for a notification or a tab —
/// the one mapping of client errors the interface has ([`actions::refusal_reason`]
/// only adds the codes of operator commands).
pub fn failure_reason(error: &ClientError) -> String {
    match error {
        ClientError::Refused { message, .. }
        | ClientError::Request { message, .. }
        | ClientError::Degraded { message, .. } => one_line(message),
        ClientError::Conflict => "zadanie zmieniło się w międzyczasie (409)".to_string(),
        ClientError::NotFound => "nie ma takiego zadania".to_string(),
        ClientError::Unavailable(_) => "brak połączenia z serwerem".to_string(),
        other => one_line(&other.to_string()),
    }
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
        if app.stream_ended() {
            // Whatever is left of an ended stream yields nothing more.
            self.stream = None;
            return;
        }
        if self.stream.is_some() {
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
