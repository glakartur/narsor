//! The remote tasks mode of the terminal interface (U8, KTD4).
//!
//! A parallel mode, not a replacement: the local `/executions` interface
//! ([`super::app`]) keeps driving the local daemon's engine as before, while
//! this mode shows the control plane's remote tasks — on any backend, the
//! `local` one included — through the owner API (`/control/v1`) only. It
//! never learns, and never sends, a backend address, a runner endpoint or a
//! backend credential: every read and every command names a task id, and
//! the control plane relays the rest (R1, R41).
//!
//! # What it shows, and what it refuses to invent
//!
//! Everything the control plane only *observed* is shown with the age of
//! the observation and whether its source still vouches for it
//! ([`Freshness`]). A disconnected controller or an unreachable runner turns
//! the line stale — it does not turn the task into some other state (R14).
//! Once a task is paused, interrupted or archived its compute is gone, so the
//! conversation is shown as explicitly unavailable with the last safe summary
//! instead of being fetched from nowhere.
//!
//! # Layers
//!
//! Like [`super::app`]: [`RemoteApp`] holds what the mode knows and turns keys
//! into a [`RemoteRequest`]; [`RemoteApp::perform`] carries a request out
//! against the API; [`super::remote_views`] draws. The text the views show
//! comes from the pure functions here, which is what `tests/tui_state.rs`
//! asserts on.

use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::api::{ApiClient, ClientError, ConversationResponse as PageResponse};
use crate::controller_protocol::{
    ControlEventEnvelope, ConversationPageQuery, ConversationResponse,
    ConversationUnavailableReason, DefaultBackendView, Freshness, OwnerCommandRequest,
    OwnerCommandResponse, QueueView, SetDefaultBackendRequest, SetPriorityRequest,
    SubmitInputRequest, TaskSummary, TimelineResponse,
};
use crate::orchestration::{
    ConversationAttemptSummary, InputStatus, OwnerCommand, PendingInputRef, QueueReason,
    TaskEventKind, TaskLifecycle,
};

use super::app::agent_transcript_entry;
use super::conversation::{classify, TranscriptEvent};
use super::input_form::{FormAction, InputForm};
use super::transcript::{TranscriptId, TranscriptState};

/// Soft cap on the raw bytes of one relayed conversation page.
pub const PAGE_BYTE_BUDGET: u64 = 64 * 1024;

/// How a line should look; the views map it to colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Plain text.
    Normal,
    /// Secondary detail.
    Muted,
    /// Progressing as it should.
    Good,
    /// Needs a look, but nothing failed.
    Warning,
    /// Cannot proceed without the owner.
    Error,
}

/// One line of text with its tone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToneLine {
    /// What is shown.
    pub text: String,
    /// How it looks.
    pub tone: Tone,
}

impl ToneLine {
    fn new(text: impl Into<String>, tone: Tone) -> Self {
        Self {
            text: text.into(),
            tone,
        }
    }
}

// ---------------------------------------------------------------------------
// Pure presentation
// ---------------------------------------------------------------------------

/// A lifecycle as people read it.
pub fn lifecycle_name(lifecycle: TaskLifecycle) -> &'static str {
    match lifecycle {
        TaskLifecycle::Queued => "queued",
        TaskLifecycle::Provisioning => "provisioning",
        TaskLifecycle::Running => "running",
        TaskLifecycle::PauseRequested => "pausing",
        TaskLifecycle::Paused => "paused",
        TaskLifecycle::Interrupted => "interrupted",
        TaskLifecycle::Unknown => "unknown",
        TaskLifecycle::Archiving => "archiving",
        TaskLifecycle::Archived => "archived",
    }
}

/// The tone of a lifecycle on its own.
pub fn lifecycle_tone(lifecycle: TaskLifecycle) -> Tone {
    match lifecycle {
        TaskLifecycle::Running | TaskLifecycle::Provisioning => Tone::Good,
        TaskLifecycle::Queued | TaskLifecycle::PauseRequested | TaskLifecycle::Paused => {
            Tone::Normal
        }
        TaskLifecycle::Interrupted | TaskLifecycle::Unknown => Tone::Error,
        TaskLifecycle::Archiving | TaskLifecycle::Archived => Tone::Muted,
    }
}

/// The short badge of a queued task's scheduler verdict (U10).
pub fn queue_badge(queue: &QueueView) -> ToneLine {
    match &queue.reason {
        QueueReason::Admitted => ToneLine::new("starting…", Tone::Good),
        QueueReason::WaitingForCapacity {
            position,
            running,
            limit,
        } => ToneLine::new(format!("#{position} · {running}/{limit}"), Tone::Normal),
        QueueReason::ControllerOffline => ToneLine::new("controller offline", Tone::Warning),
        QueueReason::BackendUnknown { detail } => {
            ToneLine::new(format!("backend unknown: {detail}"), Tone::Error)
        }
    }
}

/// `at` as an age relative to `now`: `12s`, `3m`, `2h`, `4d`.
pub fn age(now: DateTime<Utc>, at: DateTime<Utc>) -> String {
    let secs = (now - at).num_seconds().max(0);
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// The one-line status of a task in the list: its lifecycle, then the most
/// useful detail — the queue verdict, a pending question, a runner problem,
/// or the runner's status — with staleness marked.
pub fn status_badge(task: &TaskSummary) -> ToneLine {
    let lifecycle = lifecycle_name(task.lifecycle);
    if task.lifecycle == TaskLifecycle::Queued {
        if let Some(queue) = &task.queue {
            let badge = queue_badge(queue);
            return ToneLine::new(format!("{lifecycle} · {}", badge.text), badge.tone);
        }
    }
    if let Some(runner) = &task.runner {
        if let Some(problem) = &runner.problem {
            return ToneLine::new(format!("{lifecycle} · {}", problem.code), Tone::Error);
        }
        if awaiting_input(task).is_some() {
            return ToneLine::new(format!("{lifecycle} · awaiting input"), Tone::Warning);
        }
        if runner.freshness == Freshness::Stale && is_live(task.lifecycle) {
            return ToneLine::new(
                format!(
                    "{lifecycle} · stale ({} ago)",
                    age(
                        task.as_of,
                        runner.last_contact_at.unwrap_or(runner.received_at)
                    )
                ),
                Tone::Warning,
            );
        }
    }
    ToneLine::new(lifecycle, lifecycle_tone(task.lifecycle))
}

/// Lifecycles in which a runner is expected to report.
fn is_live(lifecycle: TaskLifecycle) -> bool {
    matches!(
        lifecycle,
        TaskLifecycle::Provisioning | TaskLifecycle::Running | TaskLifecycle::PauseRequested
    )
}

/// What the owner can do about the task now, in words.
pub fn lifecycle_hint(task: &TaskSummary) -> Option<ToneLine> {
    let hint = match task.lifecycle {
        TaskLifecycle::Queued => {
            let queue = task.queue.as_ref()?;
            let tone = queue_badge(queue).tone;
            return Some(ToneLine::new(queue.message.clone(), tone));
        }
        TaskLifecycle::Provisioning => ToneLine::new(
            "the backend is preparing storage and the workload",
            Tone::Normal,
        ),
        TaskLifecycle::Running => return None,
        TaskLifecycle::PauseRequested => ToneLine::new(
            "pausing: waiting for the runner to reach a safe point",
            Tone::Normal,
        ),
        TaskLifecycle::Paused => ToneLine::new(
            "paused at a safe point; storage is kept and no slot is held — resume (u) or archive (a)",
            Tone::Normal,
        ),
        TaskLifecycle::Interrupted | TaskLifecycle::Unknown => ToneLine::new(
            "the workload was lost and is not restarted on its own; it still holds its backend \
             slot — resume (u), start afresh (s) or archive (a)",
            Tone::Error,
        ),
        TaskLifecycle::Archiving => ToneLine::new(
            "archiving: deleting the workload and its storage",
            Tone::Muted,
        ),
        TaskLifecycle::Archived => ToneLine::new(
            "archived: the workload and its storage were deleted",
            Tone::Muted,
        ),
    };
    Some(hint)
}

/// The input request the runner waits on, unless the owner already
/// answered it (an answer queued, delivered or consumed for that request).
pub fn awaiting_input(task: &TaskSummary) -> Option<&PendingInputRef> {
    if task.lifecycle.is_archival() {
        return None;
    }
    let pending = task.runner.as_ref()?.pending_input.as_ref()?;
    let answered = task.inputs.iter().any(|input| {
        input.pending_input_id == pending.id
            && matches!(
                input.status,
                InputStatus::Queued | InputStatus::Delivered | InputStatus::Consumed
            )
    });
    (!answered).then_some(pending)
}

/// Every detail line of a task: identity, observations with their age,
/// inputs, timeline summary and conversation availability.
pub fn detail_lines(
    task: &TaskSummary,
    timeline: Option<&TimelineResponse>,
    conversation: Option<&ConversationResponse>,
) -> Vec<ToneLine> {
    let now = task.as_of;
    let mut lines = vec![
        ToneLine::new(format!("Task       {}", task.task_id), Tone::Normal),
        ToneLine::new(format!("Backend    {}", task.backend_id), Tone::Normal),
        ToneLine::new(
            format!(
                "Lifecycle  {} (generation {})",
                lifecycle_name(task.lifecycle),
                task.generation
            ),
            lifecycle_tone(task.lifecycle),
        ),
        ToneLine::new(
            format!(
                "Workflow   {} v{} · priority {}",
                task.workflow_id, task.workflow_version, task.priority
            ),
            Tone::Normal,
        ),
    ];
    if let Some(hint) = lifecycle_hint(task) {
        lines.push(hint);
    }

    match &task.controller {
        Some(controller) if controller.freshness == Freshness::Live => lines.push(ToneLine::new(
            format!("Controller {} online", controller.controller_id),
            Tone::Good,
        )),
        Some(controller) => lines.push(ToneLine::new(
            format!(
                "Controller {} offline (last heartbeat {})",
                controller.controller_id,
                controller
                    .last_heartbeat_at
                    .map(|at| format!("{} ago", age(now, at)))
                    .unwrap_or_else(|| "never".into())
            ),
            Tone::Warning,
        )),
        None => lines.push(ToneLine::new(
            "Controller none holds this backend's lease",
            Tone::Warning,
        )),
    }

    match &task.observation {
        Some(observation) => {
            let mut text = format!(
                "Observed   {:?}, workload {}, storage {} — {} ago",
                observation.observation,
                presence(observation.summary.workload_present),
                presence(observation.summary.storage_present),
                age(now, observation.observed_at)
            );
            if let Some(detail) = &observation.summary.detail {
                text.push_str(&format!(" ({detail})"));
            }
            let tone = match observation.freshness {
                Freshness::Live => Tone::Normal,
                Freshness::Stale => {
                    text.push_str(" · stale: last known state, not current");
                    Tone::Warning
                }
            };
            lines.push(ToneLine::new(text, tone));
        }
        None => lines.push(ToneLine::new("Observed   nothing yet", Tone::Muted)),
    }

    match &task.runner {
        Some(runner) => {
            let mut text = format!(
                "Runner     {} in {} — observed {} ago",
                runner.execution_status.as_str(),
                runner
                    .execution_cursor
                    .current_state
                    .as_deref()
                    .unwrap_or("-"),
                age(now, runner.observed_at)
            );
            let tone = if runner.freshness == Freshness::Stale {
                text.push_str(" · stale");
                Tone::Warning
            } else {
                Tone::Normal
            };
            lines.push(ToneLine::new(text, tone));
            if let Some(problem) = &runner.problem {
                lines.push(ToneLine::new(
                    format!("Problem    {}: {}", problem.code, problem.message),
                    Tone::Error,
                ));
            }
            if let Some(pending) = awaiting_input(task) {
                lines.push(ToneLine::new(
                    format!(
                        "Awaiting   input {} — {} (press i to answer)",
                        pending.id,
                        pending.form.instruction()
                    ),
                    Tone::Warning,
                ));
            }
        }
        None => lines.push(ToneLine::new(
            "Runner     has not reported yet",
            Tone::Muted,
        )),
    }

    for input in &task.inputs {
        let tone = match input.status {
            InputStatus::Rejected => Tone::Error,
            InputStatus::Obsolete => Tone::Muted,
            _ => Tone::Normal,
        };
        let mut text = format!(
            "Input      {} for {}: {:?}",
            input.input_id, input.pending_input_id, input.status
        );
        if let Some(detail) = &input.detail {
            text.push_str(&format!(" ({detail})"));
        }
        lines.push(ToneLine::new(text, tone));
    }

    if let Some(timeline) = timeline {
        match &timeline.timeline {
            Some(steps) => {
                let mut text = format!("Timeline   {} step(s)", steps.positions.len());
                if let Some(position) = &steps.default_position {
                    text.push_str(&format!(", at {position}"));
                }
                if let Some(at) = timeline.observed_at {
                    text.push_str(&format!(" — reported {} ago", age(now, at)));
                }
                let tone = if timeline.freshness == Freshness::Stale {
                    text.push_str(" · stale");
                    Tone::Warning
                } else {
                    Tone::Normal
                };
                lines.push(ToneLine::new(text, tone));
            }
            None => lines.push(ToneLine::new("Timeline   not reported yet", Tone::Muted)),
        }
    }

    if let Some(conversation) = conversation {
        lines.extend(conversation_lines(now, conversation));
    }
    lines
}

fn presence(present: bool) -> &'static str {
    if present {
        "present"
    } else {
        "absent"
    }
}

/// Why a conversation is unavailable, for people.
pub fn unavailable_reason(reason: ConversationUnavailableReason) -> &'static str {
    match reason {
        ConversationUnavailableReason::NotStarted => "the runner has not reported yet",
        ConversationUnavailableReason::Paused => "the task is paused; its compute is gone",
        ConversationUnavailableReason::Interrupted => "the workload was lost",
        ConversationUnavailableReason::Archived => "the task is archived",
        ConversationUnavailableReason::RunnerUnreachable => {
            "the runner has not been heard from lately"
        }
        ConversationUnavailableReason::RunnerReported => "the runner cannot serve it right now",
    }
}

/// The conversation summary: its attempts when the runner can serve them,
/// otherwise an explicit "unavailable" with the last safe summary (KTD4).
pub fn conversation_lines(
    now: DateTime<Utc>,
    conversation: &ConversationResponse,
) -> Vec<ToneLine> {
    let mut lines = Vec::new();
    match conversation {
        ConversationResponse::Available {
            attempts,
            observed_at,
        } => {
            lines.push(ToneLine::new(
                format!(
                    "Conversation live — {} attempt(s), reported {} ago (c: read)",
                    attempts.len(),
                    age(now, *observed_at)
                ),
                Tone::Good,
            ));
            lines.extend(attempts.iter().map(attempt_line));
        }
        ConversationResponse::Unavailable {
            reason,
            last_attempts,
            observed_at,
        } => {
            let since = observed_at
                .map(|at| format!("; last safe summary from {} ago", age(now, at)))
                .unwrap_or_default();
            lines.push(ToneLine::new(
                format!(
                    "Conversation unavailable: {}{since}",
                    unavailable_reason(*reason)
                ),
                Tone::Warning,
            ));
            lines.extend(last_attempts.iter().map(attempt_line));
        }
    }
    lines
}

fn attempt_line(attempt: &ConversationAttemptSummary) -> ToneLine {
    ToneLine::new(
        format!(
            "  attempt {} ({}) — {} entries{}",
            attempt.attempt_id,
            attempt.state_id.as_deref().unwrap_or("-"),
            attempt.entries,
            if attempt.closed { ", closed" } else { "" }
        ),
        Tone::Muted,
    )
}

/// The outcome of an owner command, as the notice shows it.
///
/// `backend_at_capacity` on a resume or start is "the backend is full",
/// not a failure of the task: it keeps its state and the owner retries once
/// a slot frees up (U10, R11).
pub fn command_outcome(
    command: OwnerCommand,
    result: &Result<OwnerCommandResponse, ClientError>,
) -> ToneLine {
    let verb = command_name(command);
    match result {
        Ok(response) => {
            let replay = if response.replayed {
                " (already accepted earlier)"
            } else {
                ""
            };
            ToneLine::new(
                format!(
                    "{verb} accepted{replay}: task {} is now {} (generation {})",
                    response.task.task_id,
                    lifecycle_name(response.task.lifecycle),
                    response.task.generation
                ),
                Tone::Good,
            )
        }
        Err(ClientError::Refused { code, message }) if code == "backend_at_capacity" => {
            ToneLine::new(
                format!(
                    "Backend full — {verb} not applied, the task keeps its state: {message}. \
                     Try again once a slot frees up."
                ),
                Tone::Warning,
            )
        }
        Err(ClientError::Refused { code, message }) => {
            ToneLine::new(format!("{verb} refused ({code}): {message}"), Tone::Error)
        }
        Err(error) => ToneLine::new(format!("{verb} failed: {error}"), Tone::Error),
    }
}

/// An owner command as a word.
pub fn command_name(command: OwnerCommand) -> &'static str {
    match command {
        OwnerCommand::Start => "start",
        OwnerCommand::Pause => "pause",
        OwnerCommand::Resume => "resume",
        OwnerCommand::Archive => "archive",
    }
}

/// The candidates of the default-backend picker: the effective, configured
/// and overriding backends and every backend a task runs on, without
/// repetition, effective first.
pub fn backend_choices(view: &DefaultBackendView, tasks: &[TaskSummary]) -> Vec<String> {
    let mut choices = vec![view.effective.clone(), view.configured.clone()];
    if let Some(backend) = &view.override_backend {
        choices.push(backend.to_string());
    }
    choices.extend(tasks.iter().map(|task| task.backend_id.to_string()));
    let mut seen = std::collections::BTreeSet::new();
    choices.retain(|choice| seen.insert(choice.clone()));
    choices
}

/// The default-backend line of the header.
pub fn default_backend_line(view: &DefaultBackendView) -> String {
    match &view.override_backend {
        Some(backend) => format!(
            "default backend: {backend} (override; configured {})",
            view.configured
        ),
        None => format!("default backend: {} (configured)", view.effective),
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Which screen is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteScreen {
    /// The task list with the selected task's detail beside it.
    List,
    /// The selected task's detail and conversation, full width.
    Detail,
}

/// One loaded page of a relayed conversation.
#[derive(Debug, Clone)]
pub struct PageView {
    /// Attempt the page belongs to.
    pub attempt_id: String,
    /// Entries, classified for display.
    pub entries: Vec<TranscriptEvent>,
    /// Entry number to continue from.
    pub next_entry: u64,
    /// Whether the attempt's record is closed.
    pub closed: bool,
    /// The runner holds no record for the attempt.
    pub no_record: bool,
    /// The runner reported an unfinished final record, so the displayed
    /// buffer must not claim to be the complete diagnostic history.
    pub truncated_tail: bool,
    /// Every readable forward page was collected before this page was made
    /// visible.
    pub complete: bool,
    /// Shared logical-line reader state for this attempt.
    pub transcript: TranscriptState,
}

/// What is loaded about the selected task beyond its summary.
#[derive(Debug, Clone, Default)]
pub struct TaskDetail {
    /// The task these belong to.
    pub task_id: String,
    /// Its last reported timeline.
    pub timeline: Option<TimelineResponse>,
    /// Its conversation summary or unavailability.
    pub conversation: Option<ConversationResponse>,
    /// The last page read through the relay.
    pub page: Option<PageView>,
}

/// An answer being composed for a remote task's pending input.
#[derive(Debug, Clone)]
pub struct RemoteInput {
    /// The task.
    pub task_id: String,
    /// The runner request it answers.
    pub pending_input_id: String,
    /// The form.
    pub form: InputForm,
}

/// The default-backend picker.
#[derive(Debug, Clone)]
pub struct BackendPicker {
    /// What it showed when opened.
    pub view: DefaultBackendView,
    /// Backends to pick from; index 0 is "reset to configured".
    pub choices: Vec<String>,
    /// The highlighted row: 0 resets, `n` picks `choices[n - 1]`.
    pub index: usize,
    /// A backend id typed by hand; wins over the highlighted row.
    pub typed: String,
}

/// A centred overlay.
#[derive(Debug, Clone)]
pub enum RemoteModal {
    /// Nothing open.
    None,
    /// Answering a pending input.
    Input(Box<RemoteInput>),
    /// Archive deletes storage: asked once more.
    ConfirmArchive {
        /// The task.
        task_id: String,
    },
    /// Editing a queued task's priority.
    Priority {
        /// The task.
        task_id: String,
        /// The generation the edit was opened at.
        generation: u64,
        /// What was typed.
        value: String,
    },
    /// Choosing the default backend of new tasks.
    DefaultBackend(BackendPicker),
    /// The key list.
    Help,
}

/// The state of the control-plane event stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteStream {
    /// Not connected yet.
    Connecting,
    /// Events arrive.
    Live,
    /// The connection failed; the client reconnects from its cursor.
    Reconnecting(String),
    /// The stream ended for good (the token was refused).
    Ended(String),
}

/// What a key asked for that needs the API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteRequest {
    /// Nothing.
    None,
    /// Re-read the tasks (and the open detail).
    Refresh,
    /// Load the selected task's detail.
    OpenDetail,
    /// Send an owner command to the selected task.
    Command(OwnerCommand),
    /// Send the open input form.
    SubmitInput,
    /// Apply the priority being edited.
    SetPriority,
    /// Load the default backend and open the picker.
    OpenBackendPicker,
    /// Apply the picker's choice.
    ApplyDefaultBackend,
    /// Read a conversation page of the selected attempt; `next` continues
    /// the page shown.
    LoadConversation {
        /// Continue after the page shown.
        next: bool,
    },
}

/// Everything the remote mode knows.
pub struct RemoteApp {
    client: ApiClient,
    tasks: Vec<TaskSummary>,
    selected: usize,
    cursor: Option<String>,
    default_backend: Option<DefaultBackendView>,
    screen: RemoteScreen,
    detail: Option<TaskDetail>,
    attempt_index: usize,
    modal: RemoteModal,
    notice: Option<ToneLine>,
    stream: RemoteStream,
    refresh_wanted: bool,
    quit: bool,
}

impl RemoteApp {
    /// A mode over `client`; nothing is read until [`Self::refresh`].
    pub fn new(client: ApiClient) -> Self {
        Self {
            client,
            tasks: Vec::new(),
            selected: 0,
            cursor: None,
            default_backend: None,
            screen: RemoteScreen::List,
            detail: None,
            attempt_index: 0,
            modal: RemoteModal::None,
            notice: None,
            stream: RemoteStream::Connecting,
            refresh_wanted: false,
            quit: false,
        }
    }

    /// The client, for opening the event stream.
    pub fn client(&self) -> &ApiClient {
        &self.client
    }

    /// The tasks, in creation order.
    pub fn tasks(&self) -> &[TaskSummary] {
        &self.tasks
    }

    /// Index of the selected task.
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// The selected task.
    pub fn selected(&self) -> Option<&TaskSummary> {
        self.tasks.get(self.selected)
    }

    /// Selects the task with `task_id`, if listed.
    pub fn select(&mut self, task_id: &str) -> bool {
        match self
            .tasks
            .iter()
            .position(|task| task.task_id.as_str() == task_id)
        {
            Some(index) => {
                self.selected = index;
                true
            }
            None => false,
        }
    }

    /// The stream cursor of the state shown.
    pub fn cursor(&self) -> Option<&str> {
        self.cursor.as_deref()
    }

    /// The default backend, once read.
    pub fn default_backend(&self) -> Option<&DefaultBackendView> {
        self.default_backend.as_ref()
    }

    /// The screen shown.
    pub fn screen(&self) -> RemoteScreen {
        self.screen
    }

    /// Records the resolved conversation pane height for logical navigation.
    pub fn set_conversation_viewport(&mut self, height: usize) {
        if let Some(page) = self.detail.as_mut().and_then(|detail| detail.page.as_mut()) {
            page.transcript.set_viewport(height);
        }
    }

    /// The loaded detail of the selected task.
    pub fn detail(&self) -> Option<&TaskDetail> {
        self.detail
            .as_ref()
            .filter(|detail| Some(detail.task_id.as_str()) == self.selected_id().as_deref())
    }

    /// The open overlay.
    pub fn modal(&self) -> &RemoteModal {
        &self.modal
    }

    /// The last outcome or error.
    pub fn notice(&self) -> Option<&ToneLine> {
        self.notice.as_ref()
    }

    /// The event stream's state.
    pub fn stream(&self) -> &RemoteStream {
        &self.stream
    }

    /// Whether the operator asked to quit.
    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// Whether a live event asked for a re-read.
    pub fn refresh_wanted(&self) -> bool {
        self.refresh_wanted
    }

    /// The detail lines of the selected task.
    pub fn selected_lines(&self) -> Vec<ToneLine> {
        let Some(task) = self.selected() else {
            return vec![ToneLine::new("No remote tasks.", Tone::Muted)];
        };
        let detail = self.detail();
        detail_lines(
            task,
            detail.and_then(|detail| detail.timeline.as_ref()),
            detail.and_then(|detail| detail.conversation.as_ref()),
        )
    }

    /// The shortcuts that apply right now.
    pub fn shortcuts(&self) -> Vec<(&'static str, &'static str)> {
        let mut keys = vec![("j/k", "move"), ("r", "refresh")];
        if self.screen == RemoteScreen::List {
            keys.push(("Enter", "open"));
        } else {
            keys.push(("Esc", "back"));
            keys.push(("c", "conversation"));
            if self
                .detail()
                .and_then(|detail| detail.page.as_ref())
                .is_some()
            {
                keys.extend([
                    ("j/k", "scroll history"),
                    ("PgUp/PgDn", "page history"),
                    ("g/G", "history top/bottom"),
                    ("e", "detail"),
                    ("v", "raw/rendered"),
                    ("[/]", "other attempt"),
                ]);
            }
        }
        if let Some(task) = self.selected() {
            match task.lifecycle {
                TaskLifecycle::Running => keys.push(("p", "pause")),
                TaskLifecycle::Paused => keys.push(("u", "resume")),
                TaskLifecycle::Interrupted | TaskLifecycle::Unknown => {
                    keys.push(("u", "resume"));
                    keys.push(("s", "start afresh"));
                }
                TaskLifecycle::Queued => keys.push(("P", "priority")),
                _ => {}
            }
            if !task.lifecycle.is_archival() {
                keys.push(("a", "archive"));
            }
            if awaiting_input(task).is_some() {
                keys.push(("i", "answer"));
            }
        }
        keys.push(("b", "default backend"));
        keys.push(("?", "help"));
        keys.push(("q", "quit"));
        keys
    }

    fn selected_id(&self) -> Option<String> {
        self.selected().map(|task| task.task_id.to_string())
    }

    fn set_notice(&mut self, line: ToneLine) {
        self.notice = Some(line);
    }

    fn error(&mut self, what: &str, error: &ClientError) {
        self.set_notice(ToneLine::new(format!("{what}: {error}"), Tone::Error));
    }

    // -- keys -------------------------------------------------------------

    /// Folds a key into the state; returns what needs the API.
    pub fn on_key(&mut self, key: KeyEvent) -> RemoteRequest {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return RemoteRequest::None;
        }
        match &mut self.modal {
            RemoteModal::None => {}
            RemoteModal::Help => {
                self.modal = RemoteModal::None;
                return RemoteRequest::None;
            }
            RemoteModal::Input(input) => {
                return match input.form.on_key(key) {
                    FormAction::None => RemoteRequest::None,
                    FormAction::Submit => RemoteRequest::SubmitInput,
                    FormAction::Cancel => {
                        self.modal = RemoteModal::None;
                        RemoteRequest::None
                    }
                };
            }
            RemoteModal::ConfirmArchive { .. } => {
                let confirmed = matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'));
                self.modal = RemoteModal::None;
                return if confirmed {
                    RemoteRequest::Command(OwnerCommand::Archive)
                } else {
                    self.set_notice(ToneLine::new("Archive cancelled.", Tone::Muted));
                    RemoteRequest::None
                };
            }
            RemoteModal::Priority { value, .. } => {
                match key.code {
                    KeyCode::Esc => self.modal = RemoteModal::None,
                    KeyCode::Enter => return RemoteRequest::SetPriority,
                    KeyCode::Backspace => {
                        value.pop();
                    }
                    KeyCode::Char(c) if c.is_ascii_digit() || (c == '-' && value.is_empty()) => {
                        value.push(c)
                    }
                    _ => {}
                }
                return RemoteRequest::None;
            }
            RemoteModal::DefaultBackend(picker) => {
                match key.code {
                    KeyCode::Esc => self.modal = RemoteModal::None,
                    KeyCode::Enter => return RemoteRequest::ApplyDefaultBackend,
                    KeyCode::Down => picker.index = (picker.index + 1) % (picker.choices.len() + 1),
                    KeyCode::Up => {
                        picker.index =
                            (picker.index + picker.choices.len()) % (picker.choices.len() + 1)
                    }
                    KeyCode::Backspace => {
                        picker.typed.pop();
                    }
                    KeyCode::Char(c) if !c.is_control() => picker.typed.push(c),
                    _ => {}
                }
                return RemoteRequest::None;
            }
        }

        if self.screen == RemoteScreen::Detail
            && self
                .detail()
                .and_then(|detail| detail.page.as_ref())
                .is_some()
        {
            let transcript = &mut self
                .detail
                .as_mut()
                .expect("detail checked")
                .page
                .as_mut()
                .expect("page checked")
                .transcript;
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    transcript.line_down();
                    return RemoteRequest::None;
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    transcript.line_up();
                    return RemoteRequest::None;
                }
                KeyCode::PageDown => {
                    transcript.page_down();
                    return RemoteRequest::None;
                }
                KeyCode::PageUp => {
                    transcript.page_up();
                    return RemoteRequest::None;
                }
                KeyCode::Char('g') | KeyCode::Home => {
                    transcript.to_top();
                    return RemoteRequest::None;
                }
                KeyCode::Char('G') | KeyCode::End => {
                    transcript.to_bottom();
                    return RemoteRequest::None;
                }
                KeyCode::Char('v') => {
                    transcript.toggle_mode();
                    return RemoteRequest::None;
                }
                KeyCode::Char('e') | KeyCode::Enter | KeyCode::Char(' ') => {
                    if let Some(anchor) = transcript.anchor() {
                        transcript.toggle_expanded(&anchor.entry_id);
                    }
                    return RemoteRequest::None;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Char('q') => {
                self.quit = true;
                RemoteRequest::None
            }
            KeyCode::Char('?') => {
                self.modal = RemoteModal::Help;
                RemoteRequest::None
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('g') | KeyCode::Home => {
                self.selected = 0;
                self.after_move()
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.selected = self.tasks.len().saturating_sub(1);
                self.after_move()
            }
            KeyCode::Char('r') => RemoteRequest::Refresh,
            KeyCode::Enter | KeyCode::Char('l') if self.screen == RemoteScreen::List => {
                self.screen = RemoteScreen::Detail;
                RemoteRequest::OpenDetail
            }
            KeyCode::Esc | KeyCode::Char('h') if self.screen == RemoteScreen::Detail => {
                self.screen = RemoteScreen::List;
                RemoteRequest::None
            }
            KeyCode::Char('p') => self.command_if(OwnerCommand::Pause),
            KeyCode::Char('u') => self.command_if(OwnerCommand::Resume),
            KeyCode::Char('s') => self.command_if(OwnerCommand::Start),
            KeyCode::Char('a') => {
                if let Some(task) = self.selected().filter(|task| !task.lifecycle.is_archival()) {
                    self.modal = RemoteModal::ConfirmArchive {
                        task_id: task.task_id.to_string(),
                    };
                }
                RemoteRequest::None
            }
            KeyCode::Char('P') => {
                self.open_priority();
                RemoteRequest::None
            }
            KeyCode::Char('i') => {
                self.open_input();
                RemoteRequest::None
            }
            KeyCode::Char('b') => RemoteRequest::OpenBackendPicker,
            KeyCode::Char('c') => RemoteRequest::LoadConversation { next: false },
            KeyCode::Char('n') => RemoteRequest::LoadConversation { next: true },
            KeyCode::Char(']') => self.cycle_attempt(1),
            KeyCode::Char('[') => self.cycle_attempt(-1),
            _ => RemoteRequest::None,
        }
    }

    fn move_selection(&mut self, by: isize) -> RemoteRequest {
        if self.tasks.is_empty() {
            return RemoteRequest::None;
        }
        let last = self.tasks.len() - 1;
        self.selected = self.selected.saturating_add_signed(by).min(last);
        self.after_move()
    }

    fn after_move(&mut self) -> RemoteRequest {
        self.attempt_index = 0;
        if self.screen == RemoteScreen::Detail {
            RemoteRequest::OpenDetail
        } else {
            RemoteRequest::None
        }
    }

    /// A command, unless the task's lifecycle makes it pointless — the
    /// control plane would refuse it, and the notice says why up front.
    fn command_if(&mut self, command: OwnerCommand) -> RemoteRequest {
        let Some(lifecycle) = self.selected().map(|task| task.lifecycle) else {
            return RemoteRequest::None;
        };
        let allowed = match command {
            OwnerCommand::Pause => lifecycle == TaskLifecycle::Running,
            OwnerCommand::Resume => matches!(
                lifecycle,
                TaskLifecycle::Paused | TaskLifecycle::Interrupted | TaskLifecycle::Unknown
            ),
            OwnerCommand::Start => {
                matches!(
                    lifecycle,
                    TaskLifecycle::Interrupted | TaskLifecycle::Unknown
                )
            }
            OwnerCommand::Archive => !lifecycle.is_archival(),
        };
        if allowed {
            RemoteRequest::Command(command)
        } else {
            self.set_notice(ToneLine::new(
                format!(
                    "Cannot {} a {} task.",
                    command_name(command),
                    lifecycle_name(lifecycle)
                ),
                Tone::Muted,
            ));
            RemoteRequest::None
        }
    }

    fn open_priority(&mut self) {
        let Some(task) = self.selected() else {
            return;
        };
        if task.lifecycle != TaskLifecycle::Queued {
            let line = ToneLine::new(
                "Priority orders waiting tasks only; this task is not queued.",
                Tone::Muted,
            );
            self.set_notice(line);
            return;
        }
        self.modal = RemoteModal::Priority {
            task_id: task.task_id.to_string(),
            generation: task.generation,
            value: task.priority.to_string(),
        };
    }

    /// Opens the form for the selected task's pending input, if it waits.
    pub fn open_input(&mut self) {
        let Some(task) = self.selected() else {
            return;
        };
        let Some(pending) = awaiting_input(task) else {
            let line = ToneLine::new("This task is not waiting for input.", Tone::Muted);
            self.set_notice(line);
            return;
        };
        self.modal = RemoteModal::Input(Box::new(RemoteInput {
            task_id: task.task_id.to_string(),
            pending_input_id: pending.id.clone(),
            form: InputForm::new(&pending.form, &[]),
        }));
    }

    fn cycle_attempt(&mut self, by: isize) -> RemoteRequest {
        let count = self.attempts().len();
        if count == 0 {
            return RemoteRequest::None;
        }
        self.attempt_index = (self.attempt_index as isize + by).rem_euclid(count as isize) as usize;
        RemoteRequest::LoadConversation { next: false }
    }

    /// The attempts the conversation summary lists, newest last.
    fn attempts(&self) -> Vec<ConversationAttemptSummary> {
        match self
            .detail()
            .and_then(|detail| detail.conversation.as_ref())
        {
            Some(ConversationResponse::Available { attempts, .. }) => attempts.clone(),
            Some(ConversationResponse::Unavailable { last_attempts, .. }) => last_attempts.clone(),
            None => Vec::new(),
        }
    }

    /// The attempt the conversation keys act on: counted back from the
    /// newest, so a fresh detail starts on the latest attempt.
    pub fn current_attempt(&self) -> Option<String> {
        let attempts = self.attempts();
        let index = attempts.len().checked_sub(1 + self.attempt_index)?;
        attempts
            .get(index)
            .map(|attempt| attempt.attempt_id.clone())
    }

    // -- the API ------------------------------------------------------------

    /// Carries out `request`.
    pub async fn perform(&mut self, request: RemoteRequest) {
        match request {
            RemoteRequest::None => {}
            RemoteRequest::Refresh => self.refresh().await,
            RemoteRequest::OpenDetail => self.load_detail().await,
            RemoteRequest::Command(command) => self.command(command).await,
            RemoteRequest::SubmitInput => self.submit_input().await,
            RemoteRequest::SetPriority => self.set_priority().await,
            RemoteRequest::OpenBackendPicker => self.open_backend_picker().await,
            RemoteRequest::ApplyDefaultBackend => self.apply_default_backend().await,
            RemoteRequest::LoadConversation { next } => self.load_conversation(next).await,
        }
    }

    /// Re-reads the task list, the default backend and the open detail.
    /// The selection follows its task, not its row.
    pub async fn refresh(&mut self) {
        self.refresh_wanted = false;
        let keep = self.selected_id();
        match self.client.control_tasks().await {
            Ok(listing) => {
                self.tasks = listing.tasks;
                self.cursor = Some(listing.cursor);
                if !keep.as_deref().is_some_and(|id| self.select(id)) {
                    self.selected = self.selected.min(self.tasks.len().saturating_sub(1));
                }
            }
            Err(error) => {
                self.error("Could not read the remote tasks", &error);
                return;
            }
        }
        match self.client.control_default_backend().await {
            Ok(view) => self.default_backend = Some(view),
            Err(error) => self.error("Could not read the default backend", &error),
        }
        if self.screen == RemoteScreen::Detail || self.detail.is_some() {
            self.load_detail().await;
        }
    }

    /// Loads the selected task's timeline and conversation summary, keeping
    /// a page already read for it.
    pub async fn load_detail(&mut self) {
        let Some(task_id) = self.selected_id() else {
            self.detail = None;
            return;
        };
        let timeline = match self.client.control_timeline(&task_id).await {
            Ok(timeline) => Some(timeline),
            Err(error) => {
                self.error("Could not read the timeline", &error);
                None
            }
        };
        let conversation = match self.client.control_conversation(&task_id).await {
            Ok(conversation) => Some(conversation),
            Err(error) => {
                self.error("Could not read the conversation summary", &error);
                None
            }
        };
        let page = self
            .detail
            .take()
            .filter(|detail| detail.task_id == task_id)
            .and_then(|detail| detail.page);
        self.detail = Some(TaskDetail {
            task_id,
            timeline,
            conversation,
            page,
        });
    }

    /// Reads a page of the current attempt through the control plane's
    /// relay — or, when the runner cannot serve it, says so and keeps the
    /// last safe summary on screen.
    pub async fn load_conversation(&mut self, next: bool) {
        if self.detail().is_none() {
            self.load_detail().await;
        }
        let Some(task_id) = self.selected_id() else {
            return;
        };
        if let Some(ConversationResponse::Unavailable { reason, .. }) = self
            .detail()
            .and_then(|detail| detail.conversation.as_ref())
        {
            let line = ToneLine::new(
                format!(
                    "Conversation unavailable: {} — showing the last safe summary.",
                    unavailable_reason(*reason)
                ),
                Tone::Warning,
            );
            self.set_notice(line);
            return;
        }
        let Some(attempt_id) = self.current_attempt() else {
            self.set_notice(ToneLine::new("No conversation recorded yet.", Tone::Muted));
            return;
        };
        // The relay is byte-paged too.  Build the complete readable prefix
        // off-screen so opening a remote conversation has the same newest-end
        // semantics as local history and does not present a first page as an
        // end of record.
        let _ = next;
        let mut from = 0;
        let mut entries = Vec::new();
        let page = loop {
            let query = ConversationPageQuery {
                from,
                byte_budget: Some(PAGE_BYTE_BUDGET),
            };
            match self
                .client
                .control_conversation_page(&task_id, &attempt_id, &query)
                .await
            {
                Ok(PageResponse::Recorded {
                    entries: page_entries,
                    next_entry,
                    closed: page_closed,
                    truncated_tail: page_truncated_tail,
                }) => {
                    if page_entries.is_empty() {
                        break PageView {
                            attempt_id: attempt_id.clone(),
                            entries,
                            next_entry,
                            closed: page_closed,
                            no_record: false,
                            truncated_tail: page_truncated_tail,
                            complete: !page_truncated_tail,
                            transcript: TranscriptState::new(TranscriptId::from(
                                attempt_id.clone(),
                            )),
                        };
                    }
                    entries.extend(page_entries.iter().map(classify));
                    if page_closed || page_truncated_tail || next_entry <= from {
                        break PageView {
                            attempt_id: attempt_id.clone(),
                            entries,
                            next_entry,
                            closed: page_closed,
                            no_record: false,
                            truncated_tail: page_truncated_tail,
                            complete: page_closed && !page_truncated_tail,
                            transcript: TranscriptState::new(TranscriptId::from(
                                attempt_id.clone(),
                            )),
                        };
                    }
                    from = next_entry;
                }
                Ok(PageResponse::NoRecord) => {
                    break PageView {
                        attempt_id: attempt_id.clone(),
                        entries: Vec::new(),
                        next_entry: 0,
                        closed: true,
                        no_record: true,
                        truncated_tail: false,
                        complete: true,
                        transcript: TranscriptState::new(TranscriptId::from(attempt_id.clone())),
                    }
                }
                Err(ClientError::Refused { code, message })
                    if code == "conversation_unavailable" =>
                {
                    self.set_notice(ToneLine::new(
                        format!("Conversation unavailable: {message}"),
                        Tone::Warning,
                    ));
                    // The summary may have turned unavailable meanwhile.
                    self.load_detail().await;
                    return;
                }
                Err(error) => {
                    self.error("Could not read the conversation", &error);
                    return;
                }
            }
        };
        if let Some(detail) = self.detail.as_mut() {
            let mut page = page;
            let entries = page.entries.iter().map(agent_transcript_entry).collect();
            if let Some(previous) = detail
                .page
                .as_ref()
                .filter(|previous| previous.attempt_id == page.attempt_id)
            {
                page.transcript = previous.transcript.clone();
            }
            page.transcript.replace_entries(entries);
            detail.page = Some(page);
        }
    }

    /// Sends `command` for the selected task, bound to the generation shown
    /// so a stale screen cannot act on a task that moved on.
    pub async fn command(&mut self, command: OwnerCommand) {
        let Some(task) = self.selected() else {
            return;
        };
        let task_id = task.task_id.to_string();
        let request = OwnerCommandRequest {
            command,
            idempotency_key: uuid::Uuid::new_v4().to_string(),
            expected_generation: Some(task.generation),
        };
        let result = self.client.control_command(&task_id, &request).await;
        self.set_notice(command_outcome(command, &result));
        self.refresh().await;
    }

    /// Sends the open input form's answer to the control plane, which hands
    /// it to the runner waiting on exactly that request.
    pub async fn submit_input(&mut self) {
        let RemoteModal::Input(input) = &mut self.modal else {
            return;
        };
        let delivery = match input.form.validate() {
            Ok(delivery) => delivery.for_episode(input.pending_input_id.clone()),
            Err(error) => {
                let message = error.message.clone();
                input.form.fail(error);
                self.set_notice(ToneLine::new(message, Tone::Error));
                return;
            }
        };
        let task_id = input.task_id.clone();
        let request = SubmitInputRequest {
            pending_input_id: input.pending_input_id.clone(),
            idempotency_key: uuid::Uuid::new_v4().to_string(),
            delivery,
        };
        match self.client.control_input(&task_id, &request).await {
            Ok(response) => {
                self.modal = RemoteModal::None;
                self.set_notice(ToneLine::new(
                    format!(
                        "Answer {} queued for the runner of task {task_id}.",
                        response.input.input_id
                    ),
                    Tone::Good,
                ));
                self.refresh().await;
            }
            Err(error) => {
                let message = match &error {
                    ClientError::Refused { code, message } => format!("{code}: {message}"),
                    other => other.to_string(),
                };
                if let RemoteModal::Input(input) = &mut self.modal {
                    input.form.error = Some(message.clone());
                }
                self.set_notice(ToneLine::new(
                    format!("The answer was not accepted: {message}"),
                    Tone::Error,
                ));
            }
        }
    }

    /// Applies the edited priority of a queued task.
    pub async fn set_priority(&mut self) {
        let RemoteModal::Priority {
            task_id,
            generation,
            value,
        } = &self.modal
        else {
            return;
        };
        let Ok(priority) = value.parse::<i32>() else {
            self.set_notice(ToneLine::new("A priority is a whole number.", Tone::Error));
            return;
        };
        let task_id = task_id.clone();
        let request = SetPriorityRequest {
            priority,
            expected_generation: Some(*generation),
        };
        self.modal = RemoteModal::None;
        match self.client.control_set_priority(&task_id, &request).await {
            Ok(task) => self.set_notice(ToneLine::new(
                format!("Priority of {} is now {}.", task.task_id, task.priority),
                Tone::Good,
            )),
            Err(ClientError::Refused { message, .. }) => {
                self.set_notice(ToneLine::new(
                    format!("Priority not changed: {message}"),
                    Tone::Error,
                ));
            }
            Err(error) => self.error("Priority not changed", &error),
        }
        self.refresh().await;
    }

    /// Reads the default backend and opens the picker on it.
    pub async fn open_backend_picker(&mut self) {
        match self.client.control_default_backend().await {
            Ok(view) => {
                let choices = backend_choices(&view, &self.tasks);
                let index = choices
                    .iter()
                    .position(|choice| *choice == view.effective)
                    .map_or(0, |index| index + 1);
                self.default_backend = Some(view.clone());
                self.modal = RemoteModal::DefaultBackend(BackendPicker {
                    view,
                    choices,
                    index,
                    typed: String::new(),
                });
            }
            Err(error) => self.error("Could not read the default backend", &error),
        }
    }

    /// Applies the picker: a typed id, the highlighted backend, or a reset
    /// to the configured value. New tasks only — existing ones never move
    /// (R10).
    pub async fn apply_default_backend(&mut self) {
        let RemoteModal::DefaultBackend(picker) = &self.modal else {
            return;
        };
        let backend_id = if !picker.typed.trim().is_empty() {
            Some(picker.typed.trim().to_string())
        } else if picker.index == 0 {
            None
        } else {
            picker.choices.get(picker.index - 1).cloned()
        };
        self.modal = RemoteModal::None;
        let request = SetDefaultBackendRequest { backend_id };
        match self.client.control_set_default_backend(&request).await {
            Ok(view) => {
                self.set_notice(ToneLine::new(
                    format!(
                        "New tasks will run on {}; existing tasks stay where they are.",
                        view.effective
                    ),
                    Tone::Good,
                ));
                self.default_backend = Some(view);
            }
            Err(ClientError::Request { message, .. } | ClientError::Refused { message, .. }) => {
                self.set_notice(ToneLine::new(
                    format!("Default backend not changed: {message}"),
                    Tone::Error,
                ))
            }
            Err(error) => self.error("Default backend not changed", &error),
        }
    }

    // -- the stream -----------------------------------------------------------

    /// Folds one item of the control-plane stream in.
    ///
    /// Lifecycle, generation, priority and queue verdict are applied to the
    /// listed summary at once — that is what the list shows. Anything else
    /// (a runner or controller report, an input changing status, a task this
    /// list does not know) asks for a re-read, which the loop debounces.
    pub fn on_stream(&mut self, item: Option<Result<ControlEventEnvelope, ClientError>>) {
        match item {
            Some(Ok(envelope)) => {
                self.stream = RemoteStream::Live;
                self.cursor = Some(envelope.cursor.clone());
                let event = envelope.event;
                let Some(task) = self
                    .tasks
                    .iter_mut()
                    .find(|task| task.task_id == event.task_id)
                else {
                    self.refresh_wanted = true;
                    return;
                };
                task.lifecycle = event.lifecycle;
                task.generation = event.generation;
                task.revision = task.revision.max(event.sequence);
                if task.lifecycle != TaskLifecycle::Queued {
                    task.queue = None;
                }
                match event.kind {
                    TaskEventKind::QueueChanged { reason } => {
                        if task.lifecycle == TaskLifecycle::Queued {
                            task.queue = Some(QueueView {
                                message: reason.describe(),
                                reason,
                                since: event.at,
                            });
                        }
                    }
                    TaskEventKind::PriorityChanged { priority } => task.priority = priority,
                    _ => self.refresh_wanted = true,
                }
            }
            Some(Err(error @ (ClientError::Unavailable(_) | ClientError::Server(_)))) => {
                self.stream = RemoteStream::Reconnecting(error.to_string());
            }
            Some(Err(error)) => self.stream = RemoteStream::Ended(error.to_string()),
            None => self.stream = RemoteStream::Ended("the event stream ended".into()),
        }
    }
}
