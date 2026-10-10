//! The details of the selected task (U9): a two-line header made of the
//! workflow's `display` (three lines while the task cannot follow the
//! installation's default harness), the tabs, and what the active tab shows.
//!
//! ```text
//! NAR-12  Zmiana API zamówień                             implement v3
//! Jira NAR-12 ↗   PR: —   ● running · krok 3/5 · 14m
//!  Przebieg   Rozmowa   Zdarzenia   Dyrygent   Info          podąża
//!                     ● review_api · w toku · agent · 2m
//!   ✓ plan ok         …
//!   ━━━━━━━━━━━━━━
//! ┃ ● review_api w toku
//!   ━━━━━━━━━━━━━━
//! ```
//!
//! # What it remembers
//!
//! [`TaskDetail`] is the state of one task's details — a tab's cursor, the
//! step the timeline follows, the transcript position, what was last read —
//! kept in [`super::DetailState`] under the task's id. Moving to another task
//! and back, or a refresh, finds it as it was left; that is requirement R18.
//!
//! # Reading
//!
//! [`TaskApp::sync_detail`] reads what the selected task's visible tab needs
//! and has not got: the timeline always (the header's `krok n/m` and the
//! following need it), the attempts of the conversation for *Przebieg* and
//! *Rozmowa*, the events for *Zdarzenia*. A task whose row moved to a newer
//! revision, or a list that was read again, makes the data due; nothing is
//! read twice for the same revision. Every answer goes through
//! [`TaskApp::on_detail_read`], so a test can hand over any answer — a relay
//! error included — without a server. A failed read stays in its tab; only an
//! unreachable server counts against the connection.
//!
//! The conductor's tab is read with the same answer as the answer panel's
//! ([`DetailRead::Conductor`]); see [`super::tabs::conductor`].
//!
//! # The answer panel (U10)
//!
//! [`areas`] takes the height of the panel and returns where it goes; the tab
//! content gives up that many rows at the bottom. [`render`] asks
//! [`super::waiting::WaitingPanel::preferred_height`] for it and draws the
//! panel there; the conductor's state, which a failed step and a proposal are
//! made of, is read with the other tabs' data ([`DetailRead::Conductor`]).

use std::num::NonZeroU16;

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::api::ClientError;
use crate::cli::task_text::lifecycle_name;
pub use crate::cli::task_text::short_age;
use crate::controller_protocol::{
    ConductorReadResponse, ExecutionEventsQuery, ExecutionEventsResponse, TaskSummary,
    TimelineResponse,
};
use crate::domain::display::{safe_link_url, DisplayLink, EMPTY_LINK_TEXT};
use crate::domain::ExecutionStatus;
use crate::engine::ConversationState;
use crate::orchestration::{QueueReason, TaskLifecycle};

use super::super::keymap::Action;
use super::super::scroll::one_line;
use super::super::style::{Role, Styles};
use super::list::{fit, state_icon, title_of};
use super::notify::Severity;
use super::tabs::conductor::{self, Conductor};
use super::tabs::conversation::{
    self, transcript_action, Attempt, AttemptRead, Attempts, Conversation,
};
use super::tabs::events::{self, Events};
use super::tabs::info::{self, Info};
use super::tabs::progress::{self, Progress};
use super::waiting::WaitingPanel;
use super::{failure_reason, DetailTab, DetailView, Focus, StepFocus, TaskApp};

/// Everything the details of one task remember.
#[derive(Debug, Clone)]
pub struct TaskDetail {
    pub(super) progress: Progress,
    pub(super) attempts: Attempts,
    pub(super) conversation: Conversation,
    pub(super) events: Events,
    info: Info,
    pub(super) conductor: Conductor,
    /// The answer panel at the bottom (U10).
    pub(super) waiting: WaitingPanel,
}

impl TaskDetail {
    /// Details of `task_id` with nothing read.
    pub fn new(task_id: &str) -> Self {
        Self {
            progress: Progress::default(),
            attempts: Attempts::default(),
            conversation: Conversation::new(task_id),
            events: Events::default(),
            info: Info::default(),
            conductor: Conductor::new(task_id),
            waiting: WaitingPanel::default(),
        }
    }

    /// The answer panel.
    pub fn waiting(&self) -> &WaitingPanel {
        &self.waiting
    }

    /// *Przebieg*.
    pub fn progress(&self) -> &Progress {
        &self.progress
    }

    /// *Rozmowa*.
    pub fn conversation(&self) -> &Conversation {
        &self.conversation
    }

    /// The attempts read so far.
    pub fn attempts(&self) -> &Attempts {
        &self.attempts
    }

    /// *Zdarzenia*.
    pub fn events(&self) -> &Events {
        &self.events
    }

    /// *Info*.
    pub fn info(&self) -> &Info {
        &self.info
    }

    /// *Dyrygent*.
    pub fn conductor(&self) -> &Conductor {
        &self.conductor
    }

    /// What the operator wrote and has not sent: in the answer panel and in
    /// the text box of *Dyrygent*.
    pub fn draft_text(&self) -> String {
        let chat = self.conductor.draft();
        let panel = self.waiting.draft_text();
        match (panel.is_empty(), chat.is_empty()) {
            (_, true) => panel,
            (true, false) => chat.to_string(),
            (false, false) => format!("{panel}\n{chat}"),
        }
    }

    /// Makes everything due for a new read, keeping what is shown.
    pub fn invalidate(&mut self) {
        self.invalidate_task_reads();
        self.invalidate_conductor_reads();
    }

    /// Makes what the task's own revision tells apart (the timeline, the
    /// attempts' conversations, the events) due for a new read.
    pub fn invalidate_task_reads(&mut self) {
        self.progress.invalidate();
        self.attempts.invalidate();
        self.events.invalidate();
    }

    /// Makes the conductor's state due for a new read: its chat grows without
    /// the task's revision moving.
    pub fn invalidate_conductor_reads(&mut self) {
        self.conductor.invalidate();
        self.waiting.invalidate();
    }

    /// What a key does in `tab`; on *Przebieg* `step` says whether the keys
    /// pick the step or read its conversation. Cursors, scroll positions and
    /// modes live here and survive anything but the task being gone.
    pub fn on_action(&mut self, tab: DetailTab, step: StepFocus, action: Action) {
        match tab {
            DetailTab::Progress => self.on_progress_action(step, action),
            DetailTab::Conversation => {
                transcript_action(self.conversation.transcript_mut(), action);
            }
            DetailTab::Events => self.events.on_action(action),
            DetailTab::Info => self.info.on_action(action),
            DetailTab::Conductor => self.conductor.on_action(action),
        }
    }

    /// On the timeline `j` / `k` / `g` / `G` pick the step and `f` follows;
    /// in the step's conversation the reading keys read it. Entering and
    /// leaving are the application's: they move the focus.
    fn on_progress_action(&mut self, step: StepFocus, action: Action) {
        match (step, action) {
            (StepFocus::Axis, Action::Down) => self.progress.step(1),
            (StepFocus::Axis, Action::Up) => self.progress.step(-1),
            (StepFocus::Axis, Action::Top) => self.progress.first(),
            (StepFocus::Axis, Action::Bottom) => self.progress.last(),
            (StepFocus::Axis, Action::Follow) => self.progress.toggle_follow(),
            (StepFocus::Axis, _)
            | (StepFocus::Conversation, Action::Follow | Action::EnterStep | Action::LeaveStep) => {
            }
            (StepFocus::Conversation, other) => {
                let attempt = self
                    .progress
                    .selected_position()
                    .and_then(|position| position.action_execution_id.clone());
                if let Some(attempt) = attempt.and_then(|id| self.attempts.get_mut(&id)) {
                    transcript_action(attempt.transcript_mut(), other);
                }
            }
        }
    }

    /// The selected step's attempt when it has a conversation to show (R2):
    /// recorded under an attempt's id, read, with a record and at least one
    /// entry. A banner of a failed read over entries read before does not
    /// take it away.
    pub fn step_conversation(&self) -> Option<&Attempt> {
        let attempt = self
            .progress
            .selected_position()
            .and_then(recorded_attempt)?;
        self.attempts
            .get(&attempt)
            .filter(|attempt| attempt.has_conversation())
    }

    /// `Enter` / `→` on the timeline: whether the selected step's
    /// conversation can take the focus. When it can, the timeline holds the
    /// step until [`Self::leave_step`].
    pub(super) fn enter_step(&mut self) -> bool {
        if self.step_conversation().is_none() {
            return false;
        }
        self.progress.pause();
        true
    }

    /// Back on the timeline: it lets the step go, and with the follow on
    /// picks the running one up.
    pub(super) fn leave_step(&mut self) {
        self.progress.resume();
    }

    /// After a read, while the operator is in the step's conversation:
    /// `None` when it can stay; otherwise the timeline lets the step go and
    /// the answer says why the focus has to fall back on it.
    pub(super) fn recheck_step(&mut self) -> Option<StepDrop> {
        if !self.progress.is_paused() {
            return Some(StepDrop::Gone);
        }
        if self.step_conversation().is_some() {
            return None;
        }
        self.leave_step();
        Some(StepDrop::Unavailable)
    }
}

/// Why the focus fell from a step's conversation back on the timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StepDrop {
    /// The conversation has nothing to show any more (no record): quietly.
    Unavailable,
    /// The step is no longer in the run: the operator is told.
    Gone,
}

/// What the bar says when the step whose conversation was read left the run.
pub(super) fn step_gone_notice(label: &str) -> String {
    format!(
        "{}: krok zniknął z przebiegu — fokus wrócił na oś",
        one_line(label)
    )
}

/// An answer to one of the reads the details make.
#[derive(Debug)]
pub enum DetailRead {
    /// The timeline.
    Timeline(Result<TimelineResponse, ClientError>),
    /// One page of events after `after`.
    Events {
        /// The sequence the page continued after.
        after: u64,
        /// The answer.
        result: Result<ExecutionEventsResponse, ClientError>,
    },
    /// The conversation of an attempt, from entry `from`.
    Attempt {
        /// The attempt.
        attempt_id: String,
        /// The entry the read started from.
        from: u64,
        /// The answer.
        result: Result<AttemptRead, ClientError>,
    },
    /// The conductor's state: a failed step's diagnosis, the proposals.
    Conductor(Result<ConductorReadResponse, ClientError>),
}

/// Where the details of a task can be read from at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// The task has (or had) an execution to read.
    Started,
    /// No execution yet: queued, or provisioning without a runner.
    NotStarted,
    /// Archived: the storage went away with the archiving.
    Archived,
}

/// Whether there is an execution to read for `task`.
pub fn readiness(task: &TaskSummary) -> Readiness {
    if task.lifecycle.is_archival() {
        Readiness::Archived
    } else if task.runner.is_none()
        && matches!(
            task.lifecycle,
            TaskLifecycle::Queued | TaskLifecycle::Provisioning
        )
    {
        Readiness::NotStarted
    } else {
        Readiness::Started
    }
}

/// What *Przebieg*, *Rozmowa* and *Zdarzenia* say when there is nothing to
/// show, or `None` when there is.
pub fn unstarted_message(task: &TaskSummary, queue_place: Option<usize>) -> Option<String> {
    match readiness(task) {
        Readiness::Started => None,
        Readiness::Archived => Some("storage usunięty przy archiwizacji".to_string()),
        Readiness::NotStarted => {
            let place = match (task.lifecycle, queue_place) {
                (TaskLifecycle::Queued, Some(place)) => format!("#{place} w kolejce"),
                (TaskLifecycle::Queued, None) => "w kolejce".to_string(),
                (other, _) => lifecycle_name(other).to_string(),
            };
            Some(format!("jeszcze nie wystartowało, {place}"))
        }
    }
}

/// Whether the terminal named by the environment draws OSC 8 hyperlinks.
/// `var` reads one variable; passing it in keeps the environment out of this
/// module, so a test says what the terminal is.
pub fn hyperlinks_supported(var: impl Fn(&str) -> Option<String>) -> bool {
    let has = |name: &str| var(name).is_some_and(|value| !value.is_empty());
    if var("TERM").is_some_and(|term| term == "dumb" || term.starts_with("linux")) {
        return false;
    }
    if has("WT_SESSION") || has("KITTY_WINDOW_ID") || has("WEZTERM_EXECUTABLE") {
        return true;
    }
    if var("TERM_PROGRAM").is_some_and(|program| {
        matches!(
            program.as_str(),
            "iTerm.app" | "WezTerm" | "vscode" | "ghostty" | "Hyper"
        )
    }) {
        return true;
    }
    if var("VTE_VERSION")
        .and_then(|version| version.parse::<u32>().ok())
        .is_some_and(|version| version >= 5000)
    {
        return true;
    }
    var("TERM").is_some_and(|term| {
        term.contains("kitty") || term.contains("foot") || term.contains("ghostty")
    })
}

/// Where the parts of the details go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Areas {
    /// The header lines: two, and a third when the task has a notice
    /// ([`notice`]).
    pub header: Rect,
    /// The tab titles.
    pub tabs: Rect,
    /// What the tab shows.
    pub content: Rect,
    /// The answer panel at the bottom, when there is one.
    pub panel: Option<Rect>,
}

/// The lines of the header of a task without a notice.
const HEADER_LINES: u16 = 2;

/// Splits the details `area`: a header `header_lines` high — one more than
/// [`HEADER_LINES`] while the task has a notice, which moves the tabs down —
/// the tabs, the content and — when `panel_height` is not zero — a panel at
/// the bottom. The content never goes below one row.
pub fn areas(area: Rect, header_lines: u16, panel_height: u16) -> Areas {
    let header = Rect::new(area.x, area.y, area.width, header_lines.min(area.height));
    let tabs = Rect::new(
        area.x,
        area.y + header.height,
        area.width,
        1.min(area.height - header.height),
    );
    let used = header.height + tabs.height;
    let remaining = area.height - used;
    let panel_height = panel_height.min(remaining.saturating_sub(1));
    let content = Rect::new(area.x, area.y + used, area.width, remaining - panel_height);
    let panel = (panel_height > 0)
        .then(|| Rect::new(area.x, content.y + content.height, area.width, panel_height));
    Areas {
        header,
        tabs,
        content,
        panel,
    }
}

// ---------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------

/// How long the loop waits for a read the runner has to answer (the
/// conversation, the events, the conductor) before it gives the tab a note
/// instead: the runner answers on its next poll, and a silent one would
/// otherwise freeze keys and redraw for as long as the server's relay waits.
pub const RELAYED_READ_LIMIT: std::time::Duration = std::time::Duration::from_secs(3);

/// The failure a read gets when the runner did not answer in time: a note
/// in the tab, like the relay's own `503`, and no mark against the
/// connection.
pub fn runner_not_answering() -> ClientError {
    ClientError::Degraded {
        code: "runner_timeout".to_string(),
        message: "runner nie odpowiada — spróbuję ponownie przy następnej zmianie".to_string(),
    }
}

/// `read`, but no longer than `limit`: past it, [`runner_not_answering`].
pub async fn bounded_read<T>(
    limit: std::time::Duration,
    read: impl std::future::Future<Output = Result<T, ClientError>>,
) -> Result<T, ClientError> {
    tokio::time::timeout(limit, read)
        .await
        .unwrap_or_else(|_| Err(runner_not_answering()))
}

impl TaskApp {
    /// Takes an answer of a read of `task_id`'s details in. `revision` is the
    /// revision of the row the read was made for. A failure stays in its tab;
    /// one that says the server is unreachable also counts against the
    /// connection, a `503` with a reason (the relay missing its deadline) does
    /// not.
    pub fn on_detail_read(&mut self, task_id: &str, revision: u64, read: DetailRead) {
        let held = self.held_step(task_id);
        match read {
            DetailRead::Timeline(result) => {
                let result = self.settle(result);
                self.detail
                    .view_mut(task_id)
                    .progress
                    .apply(revision, result);
                self.reread_waiting(task_id);
            }
            DetailRead::Events { after, result } => {
                let result = self.settle(result);
                self.detail
                    .view_mut(task_id)
                    .events
                    .apply(revision, after, result);
            }
            DetailRead::Attempt {
                attempt_id,
                from,
                result,
            } => {
                let result = self.settle(result);
                self.detail
                    .view_mut(task_id)
                    .attempts
                    .apply(&attempt_id, revision, from, result);
            }
            DetailRead::Conductor(result) => self.apply_conductor_read(task_id, revision, result),
        }
        if let Some(label) = held {
            self.recheck_step(task_id, &label);
        }
    }

    /// The step whose conversation has the focus, when it is `task_id`'s.
    fn held_step(&self, task_id: &str) -> Option<String> {
        if self.step_focus != StepFocus::Conversation || self.list.selected_id() != Some(task_id) {
            return None;
        }
        let label = self.detail.view(task_id)?.progress.selected_label()?;
        Some(label.to_string())
    }

    /// After a read of `task_id`, whose step `label` the operator reads: back
    /// on the timeline when its conversation cannot stay, with a word on the
    /// bar when the step left the run.
    fn recheck_step(&mut self, task_id: &str, label: &str) {
        match self.detail.view_mut(task_id).recheck_step() {
            None => {}
            Some(StepDrop::Unavailable) => self.back_to_axis(),
            Some(StepDrop::Gone) => {
                self.back_to_axis();
                self.notify(Severity::Warning, step_gone_notice(label));
            }
        }
    }

    /// A failed read as the words of its tab. Only a server that cannot be
    /// reached counts against the connection: a `503` with a reason is the
    /// relay missing its deadline (the connection is fine), and a refusal is
    /// an answer.
    pub(super) fn settle<T>(&mut self, result: Result<T, ClientError>) -> Result<T, String> {
        result.map_err(|error| {
            if matches!(error, ClientError::Unavailable(_) | ClientError::Server(_)) {
                self.note_transport(&error);
            }
            failure_reason(&error)
        })
    }

    /// Reads what the selected task's visible tab needs and has not got. Cheap
    /// when nothing is due: no request is made. Call it after a key, an event
    /// and a refresh.
    pub async fn sync_detail(&mut self) {
        if matches!(self.view, super::View::History(_)) {
            self.sync_history().await;
            return;
        }
        let Some(task) = self.list.selected() else {
            return;
        };
        if readiness(task) != Readiness::Started {
            return;
        }
        let id = task.task_id.to_string();
        let revision = task.revision;
        let client = self.client.clone();
        let tab = self.detail.tab();

        if self.detail.view_mut(&id).progress.needs_read(revision) {
            let result = client.control_timeline(&id).await;
            self.on_detail_read(&id, revision, DetailRead::Timeline(result));
        }
        self.sync_conductor(&id, revision).await;
        match tab {
            DetailTab::Progress => {
                let attempt = self
                    .detail
                    .view_mut(&id)
                    .progress
                    .selected_position()
                    .and_then(recorded_attempt);
                if let Some(attempt) = attempt {
                    self.sync_attempt(&id, revision, &attempt).await;
                }
            }
            DetailTab::Conversation => self.sync_conversation(&id, revision).await,
            DetailTab::Events => self.sync_events(&id, revision).await,
            DetailTab::Conductor | DetailTab::Info => {}
        }
    }

    /// Reads one attempt's conversation when it is due. Returns whether a read
    /// was made.
    async fn sync_attempt(&mut self, id: &str, revision: u64, attempt_id: &str) -> bool {
        let view = self.detail.view_mut(id);
        if !view.attempts.needs_read(attempt_id, revision) {
            return false;
        }
        let from = view.attempts.from(attempt_id);
        let client = self.client.clone();
        let result = conversation::read_attempt(&client, id, attempt_id, from).await;
        self.on_detail_read(
            id,
            revision,
            DetailRead::Attempt {
                attempt_id: attempt_id.to_string(),
                from,
                result,
            },
        );
        true
    }

    async fn sync_conversation(&mut self, id: &str, revision: u64) {
        let attempts: Vec<String> = self
            .detail
            .view_mut(id)
            .progress
            .positions()
            .iter()
            .filter_map(recorded_attempt)
            .collect();
        let mut read = false;
        for attempt in &attempts {
            read |= self.sync_attempt(id, revision, attempt).await;
        }
        let view = self.detail.view_mut(id);
        if read || !view.conversation.is_built() {
            let positions = view.progress.positions().to_vec();
            view.conversation.rebuild(&positions, &view.attempts);
        }
    }

    async fn sync_events(&mut self, id: &str, revision: u64) {
        if !self.detail.view_mut(id).events.needs_read(revision) {
            return;
        }
        let client = self.client.clone();
        for _ in 0..events::MAX_PAGES {
            let after = self.detail.view_mut(id).events.after();
            let query = ExecutionEventsQuery {
                after,
                limit: Some(events::PAGE_LIMIT),
            };
            let result = bounded_read(
                RELAYED_READ_LIMIT,
                client.control_execution_events(id, &query),
            )
            .await;
            let more = matches!(
                &result,
                Ok(ExecutionEventsResponse::Available { events, has_more: true, .. })
                    if !events.is_empty()
            );
            self.on_detail_read(id, revision, DetailRead::Events { after, result });
            if !more {
                break;
            }
        }
    }
}

/// The attempt of a position whose conversation can be read.
pub(super) fn recorded_attempt(position: &crate::engine::Position) -> Option<String> {
    match (&position.conversation, &position.action_execution_id) {
        (ConversationState::Recorded, Some(id)) => Some(id.clone()),
        _ => None,
    }
}

// ---------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------

/// A link that may be made clickable: where it starts in the line, how wide it
/// is, and where it points.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LinkSpot {
    column: u16,
    width: u16,
    url: String,
}

/// The title of a panel showing `title`, cut to fit a frame `width` wide.
pub(super) fn panel_title(title: &str, width: u16) -> String {
    format!(
        " {} ",
        fit(&one_line(title), usize::from(width).saturating_sub(4))
    )
}

/// Draws the details of the selected task into `area`: a frame named after
/// the task, lit while the keys are in the details — in the answer panel too
/// (R4) — and what it holds inside it.
pub fn render(frame: &mut Frame, area: Rect, app: &mut TaskApp, styles: &Styles, stale: bool) {
    let panel_focused = app.panel_focused();
    let frame_focused = app.focus() == Focus::Details;
    let focused = frame_focused && !panel_focused;
    let panel_has_keys = frame_focused && panel_focused;
    let hyperlinks = app.detail().hyperlinks();
    let step = app.step_focus();
    // A task created here that the server has not confirmed has no summary
    // yet: its row says where it stands (the new-task form).
    if let Some(pending) = app.list().selected_pending().cloned() {
        let inner = styles.render_panel(
            frame,
            area,
            panel_title(&pending.title, area.width),
            frame_focused,
        );
        super::new_task::render_pending(
            frame,
            inner,
            &pending,
            app.send_data(&pending.task_id),
            styles,
        );
        return;
    }
    let Some(view) = app.detail_view() else {
        let inner = styles.render_panel(frame, area, " Szczegóły ", frame_focused);
        frame.render_widget(
            Paragraph::new(Line::styled(
                "brak zaznaczonego zadania",
                styles.of(Role::Done),
            )),
            inner,
        );
        return;
    };
    let inner = styles.render_panel(
        frame,
        area,
        panel_title(&title_of(view.task), area.width),
        frame_focused,
    );
    let options = ViewOptions {
        stale,
        focused,
        step,
        panel_focused: panel_has_keys,
        hyperlinks,
    };
    render_view(frame, inner, view, styles, options);
}

/// How the details are drawn, apart from what they show.
#[derive(Debug, Clone, Copy)]
pub(super) struct ViewOptions {
    /// No connection: everything is grey.
    pub(super) stale: bool,
    /// The keys are in the tab's content.
    pub(super) focused: bool,
    /// The level of *Przebieg* the keys are on.
    pub(super) step: StepFocus,
    /// The keys are in the answer panel.
    pub(super) panel_focused: bool,
    /// Links are written as OSC 8 hyperlinks.
    pub(super) hyperlinks: bool,
}

/// Draws `view` — the details of a task, or of an entry of the history —
/// into `area`.
pub(super) fn render_view(
    frame: &mut Frame,
    area: Rect,
    view: DetailView<'_>,
    styles: &Styles,
    options: ViewOptions,
) {
    let ViewOptions {
        stale,
        focused,
        step,
        panel_focused,
        hyperlinks,
    } = options;
    let panel_height = view
        .detail
        .waiting
        .preferred_height(area.width, area.height);
    let notice = notice(view.task, styles, stale, area.width);
    let header_lines = HEADER_LINES + u16::from(notice.is_some());
    let parts = areas(area, header_lines, panel_height);

    let (mut lines, spots) = header(
        view.task,
        view.queue_place,
        &view.detail.progress,
        styles,
        stale,
        area.width,
    );
    lines.extend(notice);
    frame.render_widget(Paragraph::new(lines), parts.header);
    if hyperlinks {
        link_spots(frame.buffer_mut(), parts.header, &spots);
    }
    frame.render_widget(
        Paragraph::new(tab_bar(view.tab, &view.detail.progress, focused, styles)),
        parts.tabs,
    );

    let content = parts.content;
    let unstarted = unstarted_message(view.task, view.queue_place);
    match (view.tab, unstarted) {
        (
            DetailTab::Progress
            | DetailTab::Conversation
            | DetailTab::Events
            | DetailTab::Conductor,
            Some(message),
        ) => {
            frame.render_widget(
                Paragraph::new(Line::styled(message, styles.of(Role::Done))),
                content,
            );
        }
        (DetailTab::Progress, None) => progress::render(
            frame,
            content,
            styles,
            &mut view.detail.progress,
            &mut view.detail.attempts,
            stale,
            focused.then_some(step),
        ),
        (DetailTab::Conversation, None) => {
            conversation::render(
                frame,
                content,
                styles,
                &mut view.detail.conversation,
                stale,
                focused,
            );
        }
        (DetailTab::Events, None) => {
            events::render(
                frame,
                content,
                styles,
                &mut view.detail.events,
                stale,
                focused,
            );
        }
        (DetailTab::Conductor, None) => {
            conductor::render(
                frame,
                content,
                styles,
                &mut view.detail.conductor,
                view.task,
                stale,
                focused,
            );
        }
        (DetailTab::Info, _) => {
            info::render(
                frame,
                content,
                styles,
                &mut view.detail.info,
                view.task,
                stale,
            );
        }
    }
    if let Some(panel) = parts.panel {
        view.detail
            .waiting
            .render(frame, panel, styles, stale, panel_focused, view.task.as_of);
    }
}

/// The tab titles, the active one in the focus role while the details have
/// the keyboard.
fn tab_bar(
    active: DetailTab,
    progress: &Progress,
    focused: bool,
    styles: &Styles,
) -> Line<'static> {
    let mut spans = Vec::new();
    for tab in DetailTab::ALL {
        let style = if tab == active {
            let role = if focused { Role::Focus } else { Role::Calm };
            styles.selected_tab(role)
        } else {
            styles.of(Role::Done)
        };
        spans.push(Span::styled(format!(" {} ", tab.title()), style));
        spans.push(Span::raw(" "));
    }
    if active == DetailTab::Progress && !progress.positions().is_empty() {
        let (text, role) = if progress.follows() {
            ("↧ podąża", Role::Live)
        } else {
            ("· ręcznie", Role::Done)
        };
        spans.push(Span::styled(text, styles.of(role)));
    }
    Line::from(spans)
}

/// The two header lines of `task`, and where its links are on the second.
fn header(
    task: &TaskSummary,
    queue_place: Option<usize>,
    progress: &Progress,
    styles: &Styles,
    stale: bool,
    width: u16,
) -> (Vec<Line<'static>>, Vec<LinkSpot>) {
    let grey = |role: Role| if stale { Role::Done } else { role };
    let width = usize::from(width);

    // Line 1: the title, what the workflow calls it, the workflow.
    let workflow = format!("{} v{}", one_line(&task.workflow_id), task.workflow_version);
    let title = title_of(task);
    let identity = task.task_id.to_string();
    let room = width.saturating_sub(workflow.chars().count() + 3);
    let mut first = vec![Span::styled(
        fit(&title, room),
        styles.strong(grey(Role::Calm)),
    )];
    let mut used = first[0].width();
    if title != identity && used + 2 + identity.chars().count() <= room {
        first.push(Span::styled(format!("  {identity}"), styles.of(Role::Done)));
        used = first.iter().map(Span::width).sum();
    }
    let padding = width.saturating_sub(used + workflow.chars().count());
    first.push(Span::raw(" ".repeat(padding)));
    first.push(Span::styled(workflow, styles.of(Role::Done)));

    // Line 2: links, the state, the step or the queue, the time.
    let mut second: Vec<Span<'static>> = Vec::new();
    let mut spots = Vec::new();
    let mut column = 0usize;
    for link in &task.display.links {
        let (text, url) = link_text(link);
        let role = if url.is_some() {
            Role::Link
        } else {
            Role::Done
        };
        let span = Span::styled(text, styles.of(if stale { Role::Done } else { role }));
        if let Some(url) = url {
            if let (Ok(column), Ok(width)) = (u16::try_from(column), u16::try_from(span.width())) {
                spots.push(LinkSpot { column, width, url });
            }
        }
        column += span.width() + 3;
        second.push(span);
        second.push(Span::raw("   "));
    }
    let icon = state_icon(task);
    second.push(Span::styled(
        format!("{} {}", icon.glyph(), status_word(task)),
        styles.of(grey(icon.role())),
    ));
    for (text, role) in status_parts(task, queue_place, progress) {
        second.push(Span::styled(" · ", styles.of(Role::Done)));
        second.push(Span::styled(text, styles.of(grey(role))));
    }
    (vec![Line::from(first), Line::from(second)], spots)
}

/// The one line under the header that says the installation's default
/// harness cannot be used by this task, and which one it keeps (R16c), cut
/// to `width`; `None` while the task can follow the default. The full reason
/// is in *Info*.
fn notice(task: &TaskSummary, styles: &Styles, stale: bool, width: u16) -> Option<Line<'static>> {
    let unavailable = task.runner.as_ref()?.default_harness_unavailable.as_ref()?;
    let text = format!(
        "{} niedostępny tutaj — zostaje {}",
        one_line(&unavailable.harness),
        one_line(&unavailable.kept)
    );
    Some(Line::styled(
        fit(&text, usize::from(width)),
        styles.of(if stale { Role::Done } else { Role::Attention }),
    ))
}

/// The visible text of a link and, when it can be followed, its target.
/// A link without a value is `label: —`.
fn link_text(link: &DisplayLink) -> (String, Option<String>) {
    let label = one_line(&link.label);
    let text = one_line(&link.text);
    if text.trim().is_empty() || text == EMPTY_LINK_TEXT {
        return (format!("{label}: {EMPTY_LINK_TEXT}"), None);
    }
    match link.url.as_deref().and_then(safe_link_url) {
        Some(url) => (format!("{label} {text} ↗"), Some(url)),
        None => (format!("{label} {text}"), None),
    }
}

/// The lifecycle in a word, except that a runner that finished says how.
fn status_word(task: &TaskSummary) -> &'static str {
    match task.runner.as_ref().map(|runner| &runner.execution_status) {
        Some(ExecutionStatus::Completed) => "completed",
        Some(ExecutionStatus::Failed) => "failed",
        Some(ExecutionStatus::Cancelled) => "cancelled",
        _ => lifecycle_name(task.lifecycle),
    }
}

/// What follows the state on line 2: the queue, or the step, the time and a
/// controller that is not there.
fn status_parts(
    task: &TaskSummary,
    queue_place: Option<usize>,
    progress: &Progress,
) -> Vec<(String, Role)> {
    let mut parts = Vec::new();
    if task.lifecycle == TaskLifecycle::Queued {
        parts.push((queue_text(task, queue_place), Role::Calm));
        return parts;
    }
    if let Some((at, of)) = progress.step_count() {
        parts.push((format!("krok {at}/{of}"), Role::Calm));
    } else if let Some(step) = task
        .runner
        .as_ref()
        .and_then(|runner| runner.execution_cursor.current_state.as_deref())
        .map(one_line)
        .filter(|step| !step.trim().is_empty())
    {
        parts.push((format!("krok {step}"), Role::Calm));
    }
    if let Some(seconds) = elapsed(task) {
        parts.push((short_age(seconds), Role::Done));
    }
    if matches!(
        task.lifecycle,
        TaskLifecycle::Provisioning | TaskLifecycle::Running
    ) {
        if let Some(offline) = controller_offline(task) {
            parts.push((offline, Role::Attention));
        }
    }
    parts
}

/// The whole life of a task so far, or what it took: it runs until the
/// runner's last word when the runner finished.
fn elapsed(task: &TaskSummary) -> Option<i64> {
    let finished = task
        .runner
        .as_ref()
        .is_some_and(|runner| runner.execution_status.is_terminal());
    let until = match (&task.runner, finished) {
        (Some(runner), true) => runner.observed_at,
        _ => task.as_of,
    };
    Some((until - task.created_at).num_seconds())
}

/// `kontroler offline 4m` when no live controller holds the task's backend.
fn controller_offline(task: &TaskSummary) -> Option<String> {
    use crate::controller_protocol::Freshness;
    match &task.controller {
        Some(controller) if controller.freshness == Freshness::Live => None,
        Some(controller) => Some(match controller.last_heartbeat_at {
            Some(at) => format!(
                "kontroler offline {}",
                short_age((task.as_of - at).num_seconds())
            ),
            None => "kontroler offline".to_string(),
        }),
        None => Some("brak kontrolera".to_string()),
    }
}

/// What line 2 says of a queued task: its place and why it waits.
pub fn queue_text(task: &TaskSummary, queue_place: Option<usize>) -> String {
    let place = |fallback: Option<u32>| {
        queue_place
            .map(|place| format!("#{place}"))
            .or_else(|| fallback.map(|place| format!("#{place}")))
    };
    let joined = |place: Option<String>, text: String| match place {
        Some(place) => format!("{place} · {text}"),
        None => text,
    };
    match task.queue.as_ref() {
        Some(queue) => match &queue.reason {
            QueueReason::WaitingForCapacity {
                position,
                running,
                limit,
            } => joined(
                place(Some(*position)),
                format!("czeka na slot ({running}/{limit})"),
            ),
            QueueReason::ControllerOffline => format!(
                "kontroler offline {}",
                short_age((task.as_of - queue.since).num_seconds())
            ),
            QueueReason::Admitted => joined(place(None), "przyjęte, czeka na start".to_string()),
            QueueReason::BackendUnknown { .. } => "backend nieznany".to_string(),
        },
        None => joined(place(None), "w kolejce".to_string()),
    }
}

/// Whether `url` may be embedded in an OSC 8 sequence: `http(s)` and nothing
/// but printable ASCII, which rules out ESC, BEL, C1 controls, spaces and
/// bidirectional overrides that could end the sequence or disguise the target.
fn is_safe_link_target(url: &str) -> bool {
    (url.starts_with("http://") || url.starts_with("https://"))
        && url.chars().all(|c| c.is_ascii_graphic())
}

/// Makes the links at `spots` clickable by writing OSC 8 into the buffer.
///
/// Each link becomes one cell holding the escape sequences and the whole
/// visible text, told to occupy its true width ([`CellDiffOption::ForcedWidth`])
/// so the terminal backend neither counts the escape bytes as columns nor
/// draws over the cells the text spans. A link that does not fit on the line is
/// left as plain text.
fn link_spots(buffer: &mut Buffer, header: Rect, spots: &[LinkSpot]) {
    if header.height < 2 {
        return;
    }
    let y = header.y + 1;
    for spot in spots {
        let end = u32::from(spot.column) + u32::from(spot.width);
        let (Some(width), true) = (NonZeroU16::new(spot.width), end <= u32::from(header.width))
        else {
            continue;
        };
        if !is_safe_link_target(&spot.url) {
            continue;
        }
        let x = header.x + spot.column;
        let text: String = (0..spot.width)
            .map(|offset| buffer[(x + offset, y)].symbol().to_string())
            .collect();
        // Last line of defence: nothing but the text itself may sit between the sequences.
        if one_line(&text) != text {
            continue;
        }
        let cell = &mut buffer[(x, y)];
        cell.set_symbol(&format!("\x1b]8;;{}\x1b\\{text}\x1b]8;;\x1b\\", spot.url));
        cell.set_diff_option(CellDiffOption::ForcedWidth(width));
    }
}
