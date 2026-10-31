//! The details of the selected task (U9): a three-line header made of the
//! workflow's `display` and an optional notice, a separator, the tabs, and
//! what the active tab shows. The separator makes the tabs a distinct strip.
//!
//! ```text
//! NAR-12  Zmiana API zamówień                             implement v3
//! Jira NAR-12 ↗   PR: —   ● running · krok 3/5 · 14m
//! ────────────────────────────────────────────────────────────────────
//!  Przebieg   Rozmowa   Zdarzenia   Dyrygent   Info          podąża
//!   ✓ plan ok         …
//! ────────────────
//! │ ● review_api w toku
//! ────────────────
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
//! unreachable server counts against the connection. A finished task without
//! a runner defers these reads until Enter; once opened, its reads continue in
//! the background if the operator switches to another task.
//!
//! The conductor's tab is read with the same answer as the answer panel's
//! ([`DetailRead::Conductor`]); see [`super::tabs::conductor`].
//!
//! # The answer panel (U10)
//!
//! [`areas`] takes the height of the panel and returns where it goes; the tab
//! content gives up that many rows at the bottom. [`render`] asks
//! [`super::panel::WaitingPanel::preferred_height`] for it and draws the
//! panel there; the conductor's state, which a failed step and a proposal are
//! made of, is read with the other tabs' data ([`DetailRead::Conductor`]).

use std::collections::BTreeMap;
use std::num::NonZeroU16;

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::api::ClientError;
pub use crate::cli::task_text::short_age;
use crate::cli::task_text::{task_state_name, without_slot};
use crate::controller_protocol::{
    ConductorReadResponse, ExecutionEventsQuery, ExecutionEventsResponse, TaskSummary,
    TimelineResponse,
};
use crate::domain::display::{safe_link_url, DisplayLink, EMPTY_LINK_TEXT};
use narsor_protocol::api::ConversationState;
use narsor_protocol::controller::{QueueReason, TaskState};

use super::super::keymap::Action;
use super::super::scroll::one_line;
use super::super::style::{PanelEdge, Role, Styles};
use super::background::{DetailJob, JobKey, Outcome};
use super::list::{describe_loss, fit, state_icon, title_of};
use super::notify::Severity;
use super::panel::WaitingPanel;
use super::tabs::conductor::{self, Conductor};
use super::tabs::conversation::{
    self, transcript_action, Attempt, AttemptRead, Attempts, Conversation, ReadFailure,
};
use super::tabs::events::{self, Events};
use super::tabs::info::{self, Info};
use super::tabs::progress::{self, Progress};
use super::{failure_reason, DetailTab, DetailView, StepFocus, TaskApp};

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
    /// Whether the operator explicitly opened this task's details.
    pub(super) details_opened: bool,
}

/// Finished tasks whose runner is gone need an explicit open before their
/// execution details are fetched.
pub(super) fn requires_explicit_open(task: &TaskSummary) -> bool {
    task.state == TaskState::Finished && task.runner.is_none()
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
            details_opened: false,
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
        match step {
            StepFocus::Axis => self.on_axis_action(action),
            StepFocus::Conversation => self.on_step_conversation_action(action),
        }
    }

    /// A key on the timeline: it picks the step, or switches the following.
    fn on_axis_action(&mut self, action: Action) {
        match action {
            Action::Down => self.progress.step(1),
            Action::Up => self.progress.step(-1),
            Action::Top => self.progress.first(),
            Action::Bottom => self.progress.last(),
            Action::Follow => self.progress.toggle_follow(),
            _ => {}
        }
    }

    /// A key in the selected step's conversation: it reads the transcript.
    /// `f` is the timeline's, and entering and leaving the application's, so
    /// they do not reach it.
    fn on_step_conversation_action(&mut self, action: Action) {
        if matches!(
            action,
            Action::Follow | Action::EnterStep | Action::LeaveStep
        ) {
            return;
        }
        let attempt = self
            .progress
            .selected_position()
            .and_then(|position| position.action_execution_id.clone());
        if let Some(attempt) = attempt.and_then(|id| self.attempts.get_mut(&id)) {
            transcript_action(attempt.transcript_mut(), action);
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
    fn recheck_step(&mut self) -> Option<StepDrop> {
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

/// The level of the focus on *Przebieg* of a set of details — the tasks' or
/// the history's — and the moves between the levels, which keep every
/// timeline's hold on its step in line with it.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct StepLevel {
    level: StepFocus,
}

impl StepLevel {
    /// The level the keys are on.
    pub(super) fn get(self) -> StepFocus {
        self.level
    }

    /// `Enter` / `→` on the timeline of `selected`: into its step's
    /// conversation when it has one to show; otherwise nothing happens (AE2).
    pub(super) fn enter(&mut self, selected: &mut TaskDetail) {
        if selected.enter_step() {
            self.level = StepFocus::Conversation;
        }
    }

    /// Back on the timeline. Every timeline of `details` lets its step go, so
    /// none stays held by details the operator has left.
    pub(super) fn back_to_axis(&mut self, details: &mut BTreeMap<String, TaskDetail>) {
        self.level = StepFocus::Axis;
        for detail in details.values_mut() {
            detail.leave_step();
        }
    }

    /// The step whose conversation has the focus, when it is the details
    /// `key`'s and those are the `selected` ones.
    pub(super) fn held(
        self,
        details: &BTreeMap<String, TaskDetail>,
        selected: Option<&str>,
        key: &str,
    ) -> Option<String> {
        if self.level != StepFocus::Conversation || selected != Some(key) {
            return None;
        }
        let label = details.get(key)?.progress.selected_label()?;
        Some(label.to_string())
    }

    /// After a read of the details `key`, whose step `label` the operator
    /// reads: back on the timeline when its conversation cannot stay. The
    /// answer is the word for the bar when the step left the run.
    pub(super) fn recheck(
        &mut self,
        details: &mut BTreeMap<String, TaskDetail>,
        key: &str,
        label: &str,
    ) -> Option<String> {
        let drop = details.get_mut(key)?.recheck_step()?;
        self.back_to_axis(details);
        (drop == StepDrop::Gone).then(|| step_gone_notice(label))
    }
}

/// Why the focus fell from a step's conversation back on the timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepDrop {
    /// The conversation has nothing to show any more (no record): quietly.
    Unavailable,
    /// The step is no longer in the run: the operator is told.
    Gone,
}

/// What the bar says when the step whose conversation was read left the run.
fn step_gone_notice(label: &str) -> String {
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
    if task.state == TaskState::Archived {
        Readiness::Archived
    } else if task.runner.is_none() && task.state == TaskState::Queued {
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
            let place = match queue_place {
                Some(place) => format!("#{place} w kolejce"),
                None => "w kolejce".to_string(),
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
    /// The three header lines, their separator, and the optional notice
    /// ([`notice`]).
    pub header: Rect,
    /// The tab titles and active-tab marker.
    pub tabs: Rect,
    /// What the tab shows.
    pub content: Rect,
    /// The answer panel at the bottom, when there is one.
    pub panel: Option<Rect>,
}

/// The three lines of task metadata and the quiet row above the tabs.
const HEADER_LINES: u16 = 5;

/// How much of the task id the frame's edge shows: the number the operator
/// reads out loud. It is never cut.
const SHORT_ID_CHARS: usize = 8;

/// Splits the details `area`: a header `header_lines` high — one more than
/// [`HEADER_LINES`] while the task has a notice, which moves the two-row tabs
/// strip down — the content and — when `panel_height` is not zero — a panel at
/// the bottom. The content never goes below one row.
pub fn areas(area: Rect, header_lines: u16, panel_height: u16) -> Areas {
    let header = Rect::new(area.x, area.y, area.width, header_lines.min(area.height));
    let tabs = Rect::new(
        area.x,
        area.y + header.height,
        area.width,
        2.min(area.height - header.height),
    );
    let used = header.height + tabs.height;
    let remaining = area.height - used;
    let panel_height = panel_height.min(remaining.saturating_sub(2));
    let gap = u16::from(panel_height > 0);
    let content = Rect::new(
        area.x,
        area.y + used,
        area.width,
        remaining - panel_height - gap,
    );
    let panel = (panel_height > 0).then(|| {
        Rect::new(
            area.x,
            content.y + content.height + gap,
            area.width,
            panel_height,
        )
    });
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
        let held = self
            .step_focus
            .held(&self.detail.views, self.list.selected_id(), task_id);
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
                let result = self.settle_with(result, ReadFailure::of);
                self.detail
                    .view_mut(task_id)
                    .attempts
                    .apply(&attempt_id, revision, from, result);
            }
            DetailRead::Conductor(result) => self.apply_conductor_read(task_id, revision, result),
        }
        if let Some(label) = held {
            let notice = self
                .step_focus
                .recheck(&mut self.detail.views, task_id, &label);
            if let Some(notice) = notice {
                self.notify(Severity::Warning, notice);
            }
        }
    }

    /// A failed read as the words of its tab. Only a server that cannot be
    /// reached counts against the connection: a `503` with a reason is the
    /// relay missing its deadline (the connection is fine), and a refusal is
    /// an answer.
    pub(super) fn settle<T>(&mut self, result: Result<T, ClientError>) -> Result<T, String> {
        self.settle_with(result, |_, reason| reason)
    }

    /// [`Self::settle`], with the failure made by `failure` from the error
    /// and its words.
    pub(super) fn settle_with<T, E>(
        &mut self,
        result: Result<T, ClientError>,
        failure: impl FnOnce(&ClientError, String) -> E,
    ) -> Result<T, E> {
        result.map_err(|error| {
            if matches!(error, ClientError::Unavailable(_) | ClientError::Server(_)) {
                self.note_transport(&error);
            }
            let reason = failure_reason(&error);
            failure(&error, reason)
        })
    }

    /// Starts the reads the selected task's visible tab needs and has not
    /// got, and waits for them — all of them, the ones the answers make due
    /// too. The loop does not wait: it calls [`Self::start_detail_reads`]
    /// and applies the answers as they come.
    pub async fn sync_detail(&mut self) {
        for _ in 0..SYNC_ROUNDS {
            self.start_detail_reads();
            if !self.has_running() {
                return;
            }
            self.drain_jobs().await;
        }
    }

    /// Starts the reads the selected task's visible tab needs and has not
    /// got. Cheap when nothing is due: no request is made, and a read that
    /// runs is not started again. Call it after a key, an event, a refresh and
    /// an answer: what an answer makes due starts then.
    pub fn start_detail_reads(&mut self) {
        if matches!(self.view, super::View::History(_)) {
            self.start_history_reads();
            return;
        }
        let Some(task) = self.list.selected() else {
            return;
        };
        if requires_explicit_open(task)
            && !self
                .detail
                .views
                .get(task.task_id.as_str())
                .is_some_and(|view| view.details_opened)
        {
            return;
        }
        if readiness(task) != Readiness::Started {
            return;
        }
        let id = task.task_id.to_string();
        let revision = task.revision;
        let tab = self.detail.tab();

        // The timeline first: the other reads are made of what it says.
        if self.detail.view_mut(&id).progress.needs_read(revision) {
            self.start_timeline_read(&id, revision);
            return;
        }
        self.start_conductor_read(&id, revision);
        match tab {
            DetailTab::Progress => {
                let attempt = self
                    .detail
                    .view_mut(&id)
                    .progress
                    .selected_position()
                    .and_then(recorded_attempt);
                if let Some(attempt) = attempt {
                    self.start_attempt_read(&id, revision, &attempt);
                }
            }
            DetailTab::Conversation => {
                self.start_events_read(&id, revision);
                let attempts: Vec<String> = self
                    .detail
                    .view_mut(&id)
                    .progress
                    .positions()
                    .iter()
                    .filter_map(recorded_attempt)
                    .collect();
                for attempt in &attempts {
                    self.start_attempt_read(&id, revision, attempt);
                }
                if !self.background.task_running(&id) {
                    let view = self.detail.view_mut(&id);
                    if !view.conversation.is_built() {
                        rebuild_conversation(view);
                    }
                }
            }
            DetailTab::Events => self.start_events_read(&id, revision),
            DetailTab::Conductor | DetailTab::Info => {}
        }
    }

    /// Whether a read of the selected task's details runs: what a tab shows
    /// the loading marker for while it has nothing yet.
    pub fn detail_loading(&self) -> bool {
        match &self.view {
            super::View::History(_) => self.history_entry_loading(),
            _ => self
                .list
                .selected_id()
                .is_some_and(|id| self.background.task_running(id)),
        }
    }

    fn start_timeline_read(&mut self, id: &str, revision: u64) {
        let client = self.client.clone();
        let task_id = id.to_string();
        self.background.start(
            JobKey::Detail(task_id.clone(), DetailJob::Timeline),
            async move {
                let result = client.control_timeline(&task_id).await;
                Outcome::new(move |app: &mut TaskApp| {
                    app.on_detail_reads(&task_id, revision, vec![DetailRead::Timeline(result)]);
                })
            },
        );
    }

    /// Starts the read of one attempt's conversation when it is due.
    fn start_attempt_read(&mut self, id: &str, revision: u64, attempt_id: &str) {
        let view = self.detail.view_mut(id);
        if !view.attempts.needs_read(attempt_id, revision) {
            return;
        }
        let from = view.attempts.from(attempt_id);
        let client = self.client.clone();
        let (task_id, attempt_id) = (id.to_string(), attempt_id.to_string());
        self.background.start(
            JobKey::Detail(task_id.clone(), DetailJob::Attempt(attempt_id.clone())),
            async move {
                let result = conversation::read_attempt(&client, &task_id, &attempt_id, from).await;
                Outcome::new(move |app: &mut TaskApp| {
                    let read = DetailRead::Attempt {
                        attempt_id,
                        from,
                        result,
                    };
                    app.on_detail_reads(&task_id, revision, vec![read]);
                })
            },
        );
    }

    /// Starts the read of the events when it is due: the pages up to
    /// [`events::MAX_PAGES`] of them, one answer after the other.
    fn start_events_read(&mut self, id: &str, revision: u64) {
        let first = {
            let view = self.detail.view_mut(id);
            if !view.events.needs_read(revision) {
                return;
            }
            view.events.after()
        };
        let client = self.client.clone();
        let task_id = id.to_string();
        self.background.start(
            JobKey::Detail(task_id.clone(), DetailJob::Events),
            async move {
                let mut reads = Vec::new();
                let mut after = first;
                for _ in 0..events::MAX_PAGES {
                    let query = ExecutionEventsQuery {
                        after,
                        limit: Some(events::PAGE_LIMIT),
                    };
                    let result = bounded_read(
                        RELAYED_READ_LIMIT,
                        client.control_execution_events(&task_id, &query),
                    )
                    .await;
                    let next = match &result {
                        Ok(ExecutionEventsResponse::Available {
                            events,
                            has_more: true,
                            ..
                        }) => events.last().map(|event| event.sequence),
                        _ => None,
                    };
                    reads.push(DetailRead::Events { after, result });
                    match next {
                        Some(next) => after = next,
                        None => break,
                    }
                }
                Outcome::new(move |app: &mut TaskApp| {
                    app.on_detail_reads(&task_id, revision, reads)
                })
            },
        );
    }

    /// Takes the answers of one job of `task_id`'s details in, in the order
    /// they were read. A task that left the list has no details to put them in.
    fn on_detail_reads(&mut self, task_id: &str, revision: u64, reads: Vec<DetailRead>) {
        if !self
            .list
            .tasks()
            .iter()
            .any(|task| task.task_id.as_str() == task_id)
        {
            return;
        }
        let rebuild = reads.iter().any(|read| {
            matches!(
                read,
                DetailRead::Timeline(_) | DetailRead::Events { .. } | DetailRead::Attempt { .. }
            )
        });
        for read in reads {
            self.on_detail_read(task_id, revision, read);
        }
        if rebuild
            && self.detail.tab() == DetailTab::Conversation
            && self.list.selected_id() == Some(task_id)
        {
            rebuild_conversation(self.detail.view_mut(task_id));
        }
    }
}

/// How many times [`TaskApp::sync_detail`] starts reads and waits for them: the
/// answers make more due (the timeline, then what it names), and a bound keeps
/// a read that never settles from holding it for ever.
const SYNC_ROUNDS: usize = 64;

/// Puts the conversation together again from what was read.
fn rebuild_conversation(view: &mut TaskDetail) {
    let positions = view.progress.positions().to_vec();
    view.conversation
        .rebuild_with_events(&positions, &view.attempts, view.events.events());
}

/// The attempt of a position whose conversation can be read.
pub(super) fn recorded_attempt(position: &narsor_protocol::api::Position) -> Option<String> {
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

/// The smallest a cut title or ticket gets: one character and `…`.
const MIN_TITLE_CELLS: usize = 2;
const MIN_TICKET_CELLS: usize = 3;

/// What stands on the top edge of the frame of `task`, cut to fit a frame
/// `width` cells wide: the ticket and the title on the left, the short task id
/// on the right. The id is never cut; the title is cut first, then the ticket,
/// then the title and the ticket leave one after the other. Without a ticket
/// and a title the left side is the task's title as `display` renders it.
/// `loading` is the marker of a task being loaded.
pub fn edge_layout(
    task: &TaskSummary,
    width: u16,
    pill_overhead: usize,
    focused: bool,
    loading: Option<&str>,
) -> PanelEdge {
    let plain = |text: &Option<String>| {
        text.as_deref()
            .map(one_line)
            .filter(|text| !text.trim().is_empty())
    };
    let (ticket, title) = (plain(&task.ticket), plain(&task.title));
    let id: String = task
        .task_id
        .to_string()
        .chars()
        .take(SHORT_ID_CHARS)
        .collect();
    // The corners, the id's pill and one cell of line between the two sides.
    let tail = loading.map_or(0, |glyph| 1 + cells(glyph));
    let marker = usize::from(focused);
    let room =
        usize::from(width).saturating_sub(2 + cells(&id) + pill_overhead + 1 + marker + tail);
    let mut edge = PanelEdge {
        right: Some(id),
        tail: loading.map(str::to_string),
        ..PanelEdge::default()
    };

    if ticket.is_none() && title.is_none() {
        let text = title_of(task);
        let text = fit_cells(&one_line(&text), room.saturating_sub(2));
        edge.plain = Some(format!(" {text} "));
        return edge;
    }

    let pill = |text: &str| cells(text) + pill_overhead;
    let whole = |ticket: &Option<String>, title: &Option<String>| {
        let gap = usize::from(ticket.is_some() && title.is_some());
        ticket.as_deref().map_or(0, pill) + title.as_deref().map_or(0, pill) + gap
    };
    let cut_ticket = |ticket: Option<String>, cells_left: usize| {
        ticket.and_then(|text| {
            if cells(&text) <= cells_left {
                Some(text)
            } else if cells_left >= MIN_TICKET_CELLS {
                Some(fit_cells(&text, cells_left))
            } else {
                None
            }
        })
    };
    let (mut ticket, mut title) = (ticket, title);
    if whole(&ticket, &title) > room {
        let ticket_part = ticket.as_deref().map_or(0, |text| pill(text) + 1);
        let title_room = room.saturating_sub(ticket_part + pill_overhead);
        if let Some(text) = title.take() {
            if title_room >= MIN_TITLE_CELLS {
                title = Some(fit_cells(&text, title_room));
            } else if ticket.is_some() {
                // The title at its least, the ticket cut to what is left.
                let ticket_room = room.saturating_sub(2 * pill_overhead + MIN_TITLE_CELLS + 1);
                match cut_ticket(ticket.clone(), ticket_room) {
                    Some(cut) => {
                        ticket = Some(cut);
                        title = Some(fit_cells(&text, MIN_TITLE_CELLS));
                    }
                    None => ticket = cut_ticket(ticket, room.saturating_sub(pill_overhead)),
                }
            }
        } else {
            ticket = cut_ticket(ticket, room.saturating_sub(pill_overhead));
        }
    }
    edge.left = ticket.into_iter().chain(title).collect();
    edge
}

/// Draws the details of the selected task into `area`: a frame named after
/// the task, lit while the keys are in the details — in the answer panel too
/// (R4), but not in the move mode, whose keys move the list's row — and what
/// it holds inside it.
pub fn render(frame: &mut Frame, area: Rect, app: &mut TaskApp, styles: &Styles, stale: bool) {
    let frame_focused = !app.keys_on_list();
    let keys = match (frame_focused, app.panel_focused()) {
        (false, _) => KeysAt::Elsewhere,
        (true, true) => KeysAt::Panel,
        (true, false) => KeysAt::Content(app.step_focus()),
    };
    let hyperlinks = app.detail().hyperlinks();
    let glyph = app.loading_glyph();
    let loading = app.detail_loading();
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
    let edge = edge_layout(
        view.task,
        area.width,
        Styles::PILL_OVERHEAD,
        frame_focused,
        loading.then_some(glyph),
    );
    let inner = styles.render_panel_edge(frame, area, &edge, frame_focused, stale);
    let options = ViewOptions {
        stale,
        keys,
        hyperlinks,
        glyph,
        deferred: view.deferred,
        details_opened: view.details_opened,
        loading,
    };
    render_view(frame, inner, view, styles, options);
}

/// Where the keys are, as the details draw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeysAt {
    /// In the tab's content; on *Przebieg* at this level.
    Content(StepFocus),
    /// In the answer panel.
    Panel,
    /// Outside the details: on the list.
    Elsewhere,
}

/// How the details are drawn, apart from what they show.
#[derive(Debug, Clone, Copy)]
pub(super) struct ViewOptions {
    /// No connection: everything is grey.
    pub(super) stale: bool,
    /// Where the keys are.
    pub(super) keys: KeysAt,
    /// Links are written as OSC 8 hyperlinks.
    pub(super) hyperlinks: bool,
    /// The glyph of the loading marker at the current frame.
    pub(super) glyph: &'static str,
    /// The task is awaiting Enter before showing its execution details.
    pub(super) deferred: bool,
    /// Enter has requested its execution details.
    pub(super) details_opened: bool,
    /// A background read of the selected task is under way.
    pub(super) loading: bool,
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
        keys,
        hyperlinks,
        glyph,
        deferred,
        details_opened,
        loading,
    } = options;
    let content_keys = match keys {
        KeysAt::Content(step) => Some(step),
        KeysAt::Panel | KeysAt::Elsewhere => None,
    };
    let focused = content_keys.is_some();
    let inner_width = area.width.saturating_sub(3);
    let notice = notice(view.task, styles, stale, inner_width);
    let base_lines = HEADER_LINES + u16::from(notice.is_some());
    let panel_height = view
        .detail
        .waiting
        .preferred_height(inner_width, area.height, base_lines);
    let parts = areas(area, base_lines, panel_height);
    let content = Rect::new(
        parts.content.x.saturating_add(2),
        parts.content.y.saturating_add(1),
        parts.content.width.saturating_sub(3),
        parts.content.height.saturating_sub(2),
    );
    let (mut lines, spots) = header(
        view.task,
        view.queue_place,
        &view.detail.progress,
        styles,
        stale,
        inner_width,
    );
    lines.insert(0, Line::default());
    lines.extend(notice);
    lines.push(Line::styled(
        "─".repeat(usize::from(inner_width)),
        styles.task_tabs_divider(),
    ));
    let header_content = Rect::new(
        parts.header.x.saturating_add(2),
        parts.header.y,
        parts.header.width.saturating_sub(3),
        parts.header.height,
    );
    frame.render_widget(Paragraph::new(lines), header_content);
    let header_background = styles.task_header_background().bg;
    for y in parts.header.y..parts.header.y + parts.header.height {
        for x in parts.header.x..parts.header.x + parts.header.width {
            if let (Some(color), Some(cell)) =
                (header_background, frame.buffer_mut().cell_mut((x, y)))
            {
                cell.bg = color;
            }
        }
    }
    if hyperlinks {
        link_spots(frame.buffer_mut(), header_content, &spots);
    }
    if deferred {
        let message = Rect::new(
            area.x.saturating_add(2),
            area.y.saturating_add(parts.header.height),
            area.width.saturating_sub(3),
            area.height.saturating_sub(parts.header.height),
        );
        frame.render_widget(
            Paragraph::new(Line::styled(
                "więcej danych pojawi się po wejściu w zadanie (Enter)",
                styles.of(Role::Attention),
            )),
            message,
        );
        return;
    }
    let header_background = styles.task_header_background().bg;
    if let Some(color) = header_background {
        for y in parts.tabs.y..parts.tabs.y.saturating_add(parts.tabs.height) {
            for x in parts.tabs.x..parts.tabs.x.saturating_add(parts.tabs.width) {
                if let Some(cell) = frame.buffer_mut().cell_mut((x, y)) {
                    cell.bg = color;
                }
            }
        }
    }
    let tabs_content = Rect::new(
        parts.tabs.x.saturating_add(2),
        parts.tabs.y,
        parts.tabs.width.saturating_sub(3),
        parts.tabs.height,
    );
    frame.render_widget(
        Paragraph::new(tab_bar(
            view.tab,
            &view.detail.progress,
            view.detail.conversation.transcript().is_following_end(),
            focused,
            styles,
            tabs_content.width,
        )),
        tabs_content,
    );
    if parts.tabs.height > 1 {
        let underline_y = parts.tabs.y + 1;
        for x in parts.tabs.x..parts.tabs.x.saturating_add(parts.tabs.width) {
            if let Some(cell) = frame.buffer_mut().cell_mut((x, underline_y)) {
                cell.bg = ratatui::style::Color::Reset;
            }
        }
    }
    if requires_explicit_open(view.task)
        && details_opened
        && view.detail.progress.timeline().is_none()
        && loading
    {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "wczytuję dane szczegółowe…",
                styles.of(Role::Done),
            )),
            content,
        );
        return;
    }

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
            content_keys,
            glyph,
        ),
        (DetailTab::Conversation, None) => {
            conversation::render(
                frame,
                content,
                styles,
                &mut view.detail.conversation,
                stale,
                focused,
                glyph,
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
                glyph,
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
                glyph,
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
        view.detail.waiting.render(
            frame,
            panel,
            styles,
            stale,
            keys == KeysAt::Panel,
            view.task.as_of,
        );
    }
}

/// The tab titles, the active one in the focus role while the details have
/// the keyboard.
fn tab_bar(
    active: DetailTab,
    progress: &Progress,
    conversation_follows: bool,
    focused: bool,
    styles: &Styles,
    width: u16,
) -> Vec<Line<'static>> {
    let tabs = DetailTab::ALL;
    let follow = if active == DetailTab::Progress && !progress.positions().is_empty() {
        Some(if progress.follows() {
            ("↧ podąża", Role::Live)
        } else {
            ("· ręcznie", Role::Done)
        })
    } else if active == DetailTab::Conversation {
        Some(if conversation_follows {
            ("↧ śledzi · f wyłącz", Role::Live)
        } else {
            ("⏸ ręcznie · f śledź", Role::Attention)
        })
    } else {
        None
    };
    let names_width: usize = tabs.iter().map(|tab| Span::raw(tab.title()).width()).sum();
    let padded_width = names_width + tabs.len() * 2 + tabs.len().saturating_sub(1);
    let pad = usize::from(padded_width <= usize::from(width));
    let base_width =
        names_width + tabs.len().saturating_mul(pad * 2) + tabs.len().saturating_sub(1);
    let show_follow = follow
        .is_some_and(|(text, _)| base_width + 1 + Span::raw(text).width() <= usize::from(width));
    let mut spans = Vec::new();
    let mut column = 0usize;
    let mut selected_start = 0usize;
    let mut selected_width = 0usize;
    for (index, tab) in tabs.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" "));
            column += 1;
        }
        if pad > 0 {
            spans.push(Span::raw(" "));
            column += 1;
        }
        let style = if tab == active {
            let role = if focused { Role::Focus } else { Role::Calm };
            styles.selected_tab(role)
        } else {
            styles.of(Role::Done)
        };
        let tab_width = Span::raw(tab.title()).width();
        if tab == active {
            selected_start = column;
            selected_width = tab_width;
        }
        spans.push(Span::styled(tab.title().to_string(), style));
        column += tab_width;
        if pad > 0 {
            spans.push(Span::raw(" "));
            column += 1;
        }
    }
    if show_follow {
        if let Some((text, role)) = follow {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(text, styles.of(role)));
        }
    }
    let marker = if focused { '━' } else { '─' };
    vec![
        Line::from(spans),
        Line::from(vec![
            Span::raw(" ".repeat(selected_start)),
            Span::styled(
                marker.to_string().repeat(selected_width),
                styles.of(if focused { Role::Focus } else { Role::Done }),
            ),
        ]),
    ]
}

/// The three header lines of `task`, and where its links are on the third.
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

    // Line 1: title and task identity.
    let workflow = format!("{} v{}", one_line(&task.workflow_id), task.workflow_version);
    let title = title_of(task);
    let identity = task.task_id.to_string();
    let identity_is_leading = title.strip_prefix(&identity).is_some_and(|suffix| {
        suffix.is_empty()
            || suffix
                .chars()
                .next()
                .is_some_and(|ch| !ch.is_alphanumeric() && ch != '-' && ch != '_')
    });
    let first_text = if identity_is_leading {
        fit_cells(&title, width)
    } else {
        let identity_suffix = format!("  {identity}");
        if Span::raw(identity_suffix.as_str()).width() <= width {
            let title_width = width - Span::raw(identity_suffix.as_str()).width();
            format!("{}{}", fit_cells(&title, title_width), identity_suffix)
        } else {
            fit_cells(&identity, width)
        }
    };
    let first = Line::from(Span::styled(first_text, styles.strong(grey(Role::Calm))));

    // Line 2: runner state, warnings, and compact step/queue context.
    let mut second: Vec<Span<'static>> = Vec::new();
    let icon = state_icon(task);
    let state = format!("{} {}", icon.glyph(), status_word(task));
    second.push(Span::styled(state.clone(), styles.of(grey(icon.role()))));
    let mut used = Span::raw(state).width();
    let mut ordered = status_parts(task, queue_place, progress);
    // Preserve warning cues before optional duration and secondary detail.
    ordered.sort_by_key(|(_, role)| match role {
        Role::Attention => 0,
        Role::Calm => 1,
        Role::Done => 2,
        _ => 3,
    });
    for (text, role) in ordered {
        let separator = " · ";
        let remaining = width.saturating_sub(used);
        let needed = Span::raw(separator).width() + Span::raw(text.as_str()).width();
        if used + needed <= width {
            second.push(Span::styled(separator, styles.of(Role::Done)));
            second.push(Span::styled(text.clone(), styles.of(grey(role))));
            used += needed;
        } else if role == Role::Done {
            continue;
        } else if remaining > Span::raw(separator).width() {
            let shortened = fit_cells(&text, remaining - Span::raw(separator).width());
            if !shortened.is_empty() {
                second.push(Span::styled(separator, styles.of(Role::Done)));
                second.push(Span::styled(shortened, styles.of(grey(role))));
            }
            break;
        }
    }

    // Line 3: whole links first, then workflow/version metadata.
    let mut third: Vec<Span<'static>> = Vec::new();
    let mut spots = Vec::new();
    let mut column = 0usize;
    for link in &task.display.links {
        let (text, url) = link_text(link);
        let text_width = Span::raw(text.as_str()).width();
        let gap = if third.is_empty() { 0 } else { 3 };
        if column + gap + text_width > width {
            break;
        }
        if gap > 0 {
            third.push(Span::raw("   "));
            column += gap;
        }
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
        column += span.width();
        third.push(span);
    }
    let workflow = if third.is_empty() {
        workflow
    } else {
        format!("   {workflow}")
    };
    if column + Span::raw(workflow.as_str()).width() <= width {
        third.push(Span::styled(workflow, styles.of(Role::Done)));
    }
    (vec![first, Line::from(second), Line::from(third)], spots)
}

/// Display width of `text` in terminal cells.
fn cells(text: &str) -> usize {
    Span::raw(text).width()
}

fn fit_cells(text: &str, width: usize) -> String {
    if Span::raw(text).width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut output = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let cells = Span::raw(ch.to_string()).width();
        if used + cells > width.saturating_sub(1) {
            break;
        }
        used += cells;
        output.push(ch);
    }
    output.push('…');
    output
}

/// The one line under the header: what an interrupted task waits for — the
/// automatic resume, or its owner (R6, R7) — or else that the
/// installation's default harness cannot be used by this task, and which one
/// it keeps (R16c), cut to `width`; `None` when there is nothing to say. The
/// harness's full reason is in *Info*.
fn notice(task: &TaskSummary, styles: &Styles, stale: bool, width: u16) -> Option<Line<'static>> {
    let grey = |role: Role| if stale { Role::Done } else { role };
    if let Some(loss) = narsor_protocol::display::loss_state(task.runner_state, &task.resume) {
        let role = if loss.needs_owner() {
            Role::Problem
        } else {
            Role::Attention
        };
        return Some(Line::styled(
            fit(&describe_loss(&loss), usize::from(width)),
            styles.of(grey(role)),
        ));
    }
    let unavailable = task.runner.as_ref()?.default_harness_unavailable.as_ref()?;
    let text = format!(
        "{} niedostępny tutaj — zostaje {}",
        one_line(&unavailable.harness),
        one_line(&unavailable.kept)
    );
    Some(Line::styled(
        fit(&text, usize::from(width)),
        styles.of(grey(Role::Attention)),
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

/// The task's state in a word.
fn status_word(task: &TaskSummary) -> &'static str {
    task_state_name(task.state)
}

/// What follows the state on line 2: the queue, or the step, the time and a
/// controller that is not there.
fn status_parts(
    task: &TaskSummary,
    queue_place: Option<usize>,
    progress: &Progress,
) -> Vec<(String, Role)> {
    let mut parts = Vec::new();
    if task.state == TaskState::Queued {
        parts.push((queue_text(task, queue_place), Role::Calm));
        return parts;
    }
    if without_slot(task) {
        parts.push(("bez slotu".to_string(), Role::Calm));
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
    if task.state == TaskState::Running {
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
    // Below the padding row, the identity and the state.
    if header.height < 4 {
        return;
    }
    let y = header.y + 3;
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
