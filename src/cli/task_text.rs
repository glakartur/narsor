//! The text `narsor task …` prints about a task: badges, ages, the detail
//! block, the conversation summary and the outcome of an owner command.
//!
//! Pure functions of what the control plane returned; the commands in
//! [`super::remote`] decide what to print and these decide how it reads.
//! Everything the control plane only *observed* carries the age of the
//! observation and whether its source still vouches for it ([`Freshness`]): a
//! disconnected controller or an unreachable runner turns the line stale, it
//! does not turn the task into some other state (R14).

use chrono::{DateTime, Utc};

use serde::Serialize;

use crate::api::{ClientError, HarnessSelectionSnapshot};
use crate::controller_protocol::{
    ConversationResponse, ConversationUnavailableReason, DefaultBackendView, ExecutionEventsSource,
    Freshness, OwnerCommandResponse, QueueView, TaskSummary, TimelineResponse,
    CONVERSATION_RECORD_UNREADABLE, CONVERSATION_RECORD_UNREADABLE_PREFIX,
};
use crate::domain::{without_unsafe_chars, without_unsafe_chars_json};
use crate::tui::task_app::list::{attention_of, Attention};
use narsor_protocol::controller::PendingInputRefView;
use narsor_protocol::controller::{
    AttentionItem, AttentionKind, ConversationAttemptSummary, InputStatus, OperatorCommand,
    OwnerCommand, QueueReason, RunnerState, SlotReleaseReason, TaskEvent, TaskEventKind, TaskState,
};
use narsor_protocol::display::{exhaustion_loss, loss_state, LossState};

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
    /// Everything a line shows may come from a runner or an agent, so the
    /// characters a terminal would act on (escape sequences, bidi
    /// overrides, line breaks) never get into it.
    pub(super) fn new(text: impl Into<String>, tone: Tone) -> Self {
        Self {
            text: without_unsafe_chars(&text.into()),
            tone,
        }
    }
}

// ---------------------------------------------------------------------------
// Pure presentation
// ---------------------------------------------------------------------------

/// A runner state as people read it.
pub fn runner_state_name(runner_state: RunnerState) -> &'static str {
    match runner_state {
        RunnerState::Queued => "queued",
        RunnerState::Provisioning => "provisioning",
        RunnerState::Running => "running",
        RunnerState::PauseRequested => "pausing",
        RunnerState::Stopped => "paused",
        RunnerState::Interrupted => "interrupted",
        RunnerState::Unknown => "unknown",
        RunnerState::Archiving => "archiving",
        RunnerState::Archived => "archived",
    }
}

/// A [`TaskState`] as people read it: the one word every list, header and
/// detail uses for what the owner sees.
pub fn task_state_name(state: TaskState) -> &'static str {
    match state {
        TaskState::Queued => "queued",
        TaskState::Running => "running",
        TaskState::HumanNeeded => "awaiting input",
        TaskState::Paused => "paused",
        TaskState::Finished => "done",
        TaskState::Error => "failed",
        TaskState::Archived => "archived",
        TaskState::Unrecognized => "unknown",
    }
}

/// The tone of a [`TaskState`] on its own.
pub fn task_state_tone(state: TaskState) -> Tone {
    match state {
        TaskState::Running => Tone::Good,
        TaskState::Queued | TaskState::Paused | TaskState::Finished => Tone::Normal,
        TaskState::HumanNeeded => Tone::Warning,
        TaskState::Error => Tone::Error,
        TaskState::Archived | TaskState::Unrecognized => Tone::Muted,
    }
}

/// Whether the task is in a state that does not need a slot to be in and
/// holds none: the owner sees "no slot" next to it. A task that is pausing
/// still holds its slot, so it has no such note.
pub fn without_slot(task: &TaskSummary) -> bool {
    !task.holds_slot
        && matches!(
            task.state,
            TaskState::Finished | TaskState::HumanNeeded | TaskState::Error | TaskState::Paused
        )
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
    short_age((now - at).num_seconds())
}

/// `45s`, `4m`, `3h`, `2d`: `seconds` in the largest whole unit (a negative
/// span reads as zero).
pub fn short_age(seconds: i64) -> String {
    let seconds = seconds.max(0);
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86_399 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// How old a record is, in the coarse steps people read its age in: under
/// a minute it is "just now", then whole minutes, hours and days. The TUI
/// and the CLI word the same bucket in their own language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordAge {
    /// Less than a minute.
    JustNow,
    /// Whole minutes, under an hour.
    Minutes(i64),
    /// Whole hours, under a day.
    Hours(i64),
    /// Whole days.
    Days(i64),
}

impl RecordAge {
    /// The bucket of a record written `seconds` ago (a negative span — a
    /// clock behind the writer's — reads as just now).
    pub fn of_seconds(seconds: i64) -> Self {
        match seconds.max(0) {
            0..=59 => Self::JustNow,
            seconds @ 60..=3599 => Self::Minutes(seconds / 60),
            seconds @ 3600..=86_399 => Self::Hours(seconds / 3600),
            seconds => Self::Days(seconds / 86_400),
        }
    }

    /// The bucket of a record written `at`, seen `now`.
    pub fn between(now: DateTime<Utc>, at: DateTime<Utc>) -> Self {
        Self::of_seconds((now - at).num_seconds())
    }

    /// The age in English: `just now`, `5 min`, `3 h`, `2 d`.
    pub fn words(self) -> String {
        match self {
            Self::JustNow => "just now".to_string(),
            Self::Minutes(minutes) => format!("{minutes} min"),
            Self::Hours(hours) => format!("{hours} h"),
            Self::Days(days) => format!("{days} d"),
        }
    }
}

/// Where a conversation was read from, as the first words of its summary
/// or page: `Conversation live` from the runner, `Conversation from record,
/// 5 min old` from the task's storage (without the age when the record
/// holds no entry yet).
pub fn conversation_origin(
    now: DateTime<Utc>,
    source: ExecutionEventsSource,
    recorded_at: Option<DateTime<Utc>>,
) -> String {
    match (source, recorded_at.map(|at| RecordAge::between(now, at))) {
        (ExecutionEventsSource::Runner, _) => "Conversation live".to_string(),
        (ExecutionEventsSource::Storage, None) => "Conversation from record".to_string(),
        (ExecutionEventsSource::Storage, Some(RecordAge::JustNow)) => {
            "Conversation from record, written just now".to_string()
        }
        (ExecutionEventsSource::Storage, Some(age)) => {
            format!("Conversation from record, {} old", age.words())
        }
    }
}

/// What went wrong reading the task's own conversation record, when that is
/// why the control plane refused a page; `None` for any other failure.
pub fn record_unreadable_detail(error: &ClientError) -> Option<String> {
    match error {
        ClientError::Refused { code, message } if code == CONVERSATION_RECORD_UNREADABLE => {
            let detail = message
                .strip_prefix(CONVERSATION_RECORD_UNREADABLE_PREFIX)
                .unwrap_or(message);
            Some(without_unsafe_chars(detail))
        }
        _ => None,
    }
}

/// The one-line status of a task in the list: its state, then the most
/// useful detail — the queue verdict, a runner problem, a missing slot, or
/// staleness.
pub fn status_badge(task: &TaskSummary) -> ToneLine {
    let state = task_state_name(task.state);
    let tone = task_state_tone(task.state);
    if task.state == TaskState::Queued {
        if let Some(queue) = &task.queue {
            let badge = queue_badge(queue);
            return ToneLine::new(format!("{state} · {}", badge.text), badge.tone);
        }
    }
    if let Some(runner) = &task.runner {
        if let Some(problem) = &runner.problem {
            return ToneLine::new(format!("{state} · {}", problem.code), Tone::Error);
        }
        if runner.freshness == Freshness::Stale && is_live(task) {
            return ToneLine::new(
                format!(
                    "{state} · stale ({} ago)",
                    age(
                        task.as_of,
                        runner.last_contact_at.unwrap_or(runner.received_at)
                    )
                ),
                Tone::Warning,
            );
        }
    }
    if without_slot(task) {
        return ToneLine::new(format!("{state} · no slot"), tone);
    }
    ToneLine::new(state, tone)
}

/// One row of `narsor task list`: id, backend, queue place, the attention
/// mark (`?` asks, `!` is wrong), the status and — when the workflow declares
/// one — the title.
pub fn list_line(task: &TaskSummary) -> String {
    let title = display_title(task);
    let title = if title.is_empty() {
        String::new()
    } else {
        format!(" — {title}")
    };
    without_unsafe_chars(&format!(
        "{:<28} {:<14} {:<10} {} {}{title}",
        task.task_id,
        task.backend_id,
        format!("#{}", task.rank),
        attention_of(task).glyph(),
        status_badge(task).text
    ))
}

/// The task's title from its `display`, made safe to print.
fn display_title(task: &TaskSummary) -> String {
    without_unsafe_chars(task.display.title.trim())
}

/// What a kind of waiting is called.
pub fn attention_kind_name(kind: AttentionKind) -> &'static str {
    match kind {
        AttentionKind::Question => "question",
        AttentionKind::Approval => "approval",
        AttentionKind::Problem => "problem",
        AttentionKind::EntryLimit => "entry limit",
        AttentionKind::FailedStep => "failed step",
        AttentionKind::ConductorProposal => "conductor proposal",
        AttentionKind::ProvisioningFailed => "provisioning failed",
        AttentionKind::Unrecognized => "unsupported wait",
    }
}

/// The title and the links of the task's `display`; nothing when the
/// workflow declares none. A link without a value reads `—`.
pub fn display_lines(task: &TaskSummary) -> Vec<ToneLine> {
    // A runner reports its own display: it is cleaned (and a URL that is not
    // `http(s)` dropped) before anything is printed.
    let display = task.display.clone().sanitized();
    let mut lines = Vec::new();
    if !display.title.trim().is_empty() {
        lines.push(ToneLine::new(
            format!("Title      {}", display.title.trim()),
            Tone::Normal,
        ));
    }
    for link in &display.links {
        let text = if link.text.trim().is_empty() {
            crate::domain::EMPTY_LINK_TEXT
        } else {
            link.text.trim()
        };
        let url = link
            .url
            .as_deref()
            .map(|url| format!(" <{url}>"))
            .unwrap_or_default();
        lines.push(ToneLine::new(
            format!("Link       {}: {text}{url}", link.label),
            Tone::Normal,
        ));
    }
    lines
}

/// What the task waits on its owner for, oldest first: the kind, the state
/// it concerns, since when, and the id an answer names (`--waiting-id`).
pub fn attention_lines(task: &TaskSummary) -> Vec<ToneLine> {
    task.attention
        .iter()
        .map(|item| {
            let tone = match attention_glyph(item.kind) {
                '?' => Tone::Warning,
                _ => Tone::Error,
            };
            let state = item
                .state_id
                .as_deref()
                .map(|state| format!(" in '{state}'"))
                .unwrap_or_default();
            ToneLine::new(
                format!(
                    "Waiting    {} {}{state} for {} (id {})",
                    attention_glyph(item.kind),
                    attention_kind_name(item.kind),
                    age(task.as_of, item.since),
                    item.waiting_id
                ),
                tone,
            )
        })
        .collect()
}

fn attention_glyph(kind: AttentionKind) -> char {
    match kind {
        AttentionKind::Question | AttentionKind::Approval | AttentionKind::ConductorProposal => '?',
        AttentionKind::Problem | AttentionKind::FailedStep | AttentionKind::ProvisioningFailed => {
            '!'
        }
        AttentionKind::EntryLimit => '?',
        AttentionKind::Unrecognized => '?',
    }
}

/// The next task after `current` that waits on its owner, wrapping round
/// the list (the TUI's `Space`); `None` when no other task waits.
pub fn next_waiting<'a>(tasks: &'a [TaskSummary], current: &str) -> Option<&'a TaskSummary> {
    let start = tasks
        .iter()
        .position(|task| task.task_id.as_str() == current)
        .map_or(0, |index| index + 1);
    (0..tasks.len())
        .map(|offset| &tasks[(start + offset) % tasks.len()])
        .find(|task| task.task_id.as_str() != current && attention_of(task) != Attention::None)
}

/// The ids an owner answer can name: what the task waits on, as `kind id`.
pub fn waiting_summary(attention: &[AttentionItem]) -> String {
    if attention.is_empty() {
        return "nothing".to_string();
    }
    attention
        .iter()
        .map(|item| format!("{} {}", attention_kind_name(item.kind), item.waiting_id))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `value` as pretty JSON for a machine to read. Strings the runner or an
/// agent wrote are cleaned of what a terminal would act on (bidi marks,
/// zero-width characters); serde escapes the control characters.
pub fn safe_json<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let mut value = serde_json::to_value(value)?;
    without_unsafe_chars_json(&mut value);
    serde_json::to_string_pretty(&value)
}

/// Whether a runner that holds a slot is expected to report.
fn is_live(task: &TaskSummary) -> bool {
    task.holds_slot && matches!(task.state, TaskState::Running | TaskState::Paused)
}

/// What the owner can do about the task now, in words.
pub fn state_hint(task: &TaskSummary) -> Option<ToneLine> {
    let hint = match task.state {
        TaskState::Queued => {
            let queue = task.queue.as_ref()?;
            let tone = queue_badge(queue).tone;
            return Some(ToneLine::new(queue.message.clone(), tone));
        }
        TaskState::Running => {
            // A lost workload that the policy still tries to resume looks
            // like work to the owner, and says what it waits for.
            if task.runner_state != RunnerState::Interrupted {
                return None;
            }
            interrupted_hint(task)
        }
        TaskState::HumanNeeded if task.holds_slot => ToneLine::new(
            "the task waits for your answer",
            Tone::Warning,
        ),
        TaskState::HumanNeeded => ToneLine::new(
            "the task waits for your answer and holds no slot; answering resumes it, and it \
             queues first when the backend is full",
            Tone::Warning,
        ),
        // Lost while pausing: no attempt is planned, the owner decides.
        TaskState::Paused if task.runner_state == RunnerState::Interrupted => {
            interrupted_hint(task)
        }
        TaskState::Paused if task.holds_slot => ToneLine::new(
            "pausing: waiting for the runner to reach a safe point",
            Tone::Normal,
        ),
        TaskState::Paused => ToneLine::new(
            "paused at a safe point; storage is kept and no slot is held — resume (u) or archive (a)",
            Tone::Normal,
        ),
        TaskState::Finished if task.holds_slot => ToneLine::new(
            "the work ended; the slot is freed shortly — archive (a)",
            Tone::Normal,
        ),
        TaskState::Finished => ToneLine::new(
            "the work ended; no slot is held — resume (u) or archive (a)",
            Tone::Normal,
        ),
        TaskState::Error if loss_state(task.runner_state, &task.resume).is_some() => {
            interrupted_hint(task)
        }
        TaskState::Error => ToneLine::new(
            "the task ended with an error; start afresh (s), resume (u) or archive (a)",
            Tone::Error,
        ),
        TaskState::Archived => ToneLine::new(
            "archived: the workload and its storage were deleted",
            Tone::Muted,
        ),
        TaskState::Unrecognized => return None,
    };
    Some(hint)
}

/// What an interrupted task waits for — the automatic resume or its owner —
/// as the TUI tells it ([`loss_state`]).
fn interrupted_hint(task: &TaskSummary) -> ToneLine {
    const OWNER_CHOICES: &str =
        "it still holds its backend slot — resume (u), start afresh (s) or archive (a)";
    match loss_state(task.runner_state, &task.resume) {
        Some(LossState::AutoResume { attempt, limit }) => ToneLine::new(
            format!(
                "the workload was lost; it is resumed automatically (attempt {attempt}/{limit}) \
                 — or start afresh (s) or archive (a)"
            ),
            Tone::Warning,
        ),
        Some(LossState::Exhausted(reason)) => {
            ToneLine::new(format!("{reason}; {OWNER_CHOICES}"), Tone::Error)
        }
        Some(LossState::LostWhilePausing) => ToneLine::new(
            format!("the workload was lost while pausing and waits for you; {OWNER_CHOICES}"),
            Tone::Error,
        ),
        None => ToneLine::new(
            format!("the workload was lost; {OWNER_CHOICES}"),
            Tone::Error,
        ),
    }
}

/// The input request the runner waits on, unless the owner already
/// answered it (an answer queued, delivered or consumed for that request).
pub fn awaiting_input(task: &TaskSummary) -> Option<&PendingInputRefView> {
    if matches!(task.state, TaskState::Archived | TaskState::Finished) {
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
                "State      {}{} (generation {})",
                task_state_name(task.state),
                if without_slot(task) {
                    " · no slot"
                } else if task.holds_slot {
                    " · slot held"
                } else {
                    ""
                },
                task.generation
            ),
            task_state_tone(task.state),
        ),
        ToneLine::new(
            format!(
                "Workflow   {} v{} · #{}",
                task.workflow_id, task.workflow_version, task.rank
            ),
            Tone::Normal,
        ),
    ];
    lines.splice(1..1, display_lines(task));
    if let Some(hint) = state_hint(task) {
        lines.push(hint);
    }
    lines.extend(attention_lines(task));

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
            if let Some(unavailable) = &runner.default_harness_unavailable {
                lines.push(ToneLine::new(
                    format!(
                        "Harness    {} unavailable here, keeping {}: {}",
                        without_unsafe_chars(&unavailable.harness),
                        without_unsafe_chars(&unavailable.kept),
                        without_unsafe_chars(&unavailable.reason)
                    ),
                    Tone::Warning,
                ));
            }
            if let Some(pending) = awaiting_input(task) {
                lines.push(ToneLine::new(
                    format!(
                        "Awaiting   input {} — {} (press i to answer)",
                        pending.id,
                        pending
                            .form
                            .get("instruction")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("input details unavailable")
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
        ConversationUnavailableReason::StorageUnreadable => "its record could not be read",
        ConversationUnavailableReason::ReaderStarting => {
            "the task is stopped; its read-only runner is starting"
        }
        ConversationUnavailableReason::Unrecognized => {
            "the server gave a reason this version does not know"
        }
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
            source: ExecutionEventsSource::Runner,
            ..
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
        ConversationResponse::Available {
            attempts,
            source: source @ ExecutionEventsSource::Storage,
            recorded_at,
            ..
        } => {
            let origin = conversation_origin(now, *source, *recorded_at);
            lines.push(if attempts.is_empty() {
                ToneLine::new(format!("{origin} — nothing was recorded"), Tone::Muted)
            } else {
                ToneLine::new(
                    format!("{origin} — {} attempt(s) (c: read)", attempts.len()),
                    Tone::Normal,
                )
            });
            lines.extend(attempts.iter().map(attempt_line));
        }
        ConversationResponse::Unavailable {
            reason,
            last_attempts,
            observed_at,
            message,
        } => {
            let since = observed_at
                .map(|at| format!("; last safe summary from {} ago", age(now, at)))
                .unwrap_or_default();
            let detail = message
                .as_deref()
                .map(|message| {
                    let message = message
                        .strip_prefix(CONVERSATION_RECORD_UNREADABLE_PREFIX)
                        .unwrap_or(message);
                    format!(": {message}")
                })
                .unwrap_or_default();
            lines.push(ToneLine::new(
                format!(
                    "Conversation unavailable: {}{detail}{since}",
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

/// One line of `narsor task events`: sequence, time, task, runner state and
/// what happened.
pub fn event_line(event: &TaskEvent) -> String {
    let kind = match &event.kind {
        TaskEventKind::Created => "created".to_string(),
        TaskEventKind::OwnerCommandAccepted { command, .. } => {
            format!("owner command {command:?}")
        }
        TaskEventKind::ControllerReported { observation, .. } => {
            format!("controller reported {observation:?}")
        }
        TaskEventKind::RunnerReported {
            execution_status,
            pending_input_id,
            ..
        } => match pending_input_id {
            Some(pending) => format!(
                "runner reported {} (awaiting input {pending})",
                execution_status.as_str()
            ),
            None => format!("runner reported {}", execution_status.as_str()),
        },
        TaskEventKind::InputQueued { input_id, .. } => format!("input {input_id} queued"),
        TaskEventKind::OperatorCommandQueued {
            command_id,
            command,
            requested_by,
        } => format!(
            "operator command {command_id} queued by {}: {}",
            requested_by.0,
            describe_operator_command(command)
        ),
        TaskEventKind::OperatorCommandResolved {
            command_id,
            status,
            detail,
        } => match detail {
            Some(detail) => format!("operator command {command_id} {status:?}: {detail}"),
            None => format!("operator command {command_id} {status:?}"),
        },
        TaskEventKind::PriorityChanged { priority } => format!("priority {priority} (obsolete)"),
        TaskEventKind::Moved { rank, .. } => format!("moved to #{rank}"),
        TaskEventKind::QueueChanged { reason } => format!("queue: {}", reason.describe()),
        TaskEventKind::InputsUpdated { updates } => format!("{} input(s) updated", updates.len()),
        TaskEventKind::MaterialChanged {
            material_id,
            change,
            files,
            total_bytes,
        } => format!("material {material_id} {change:?} ({files} file(s), {total_bytes} bytes)"),
        TaskEventKind::AutoResumed {
            attempt: 0, limit, ..
        } => format!(
            "resumed automatically after a controller restart lost the runner \
             (not counted toward the {limit} attempts)"
        ),
        TaskEventKind::AutoResumed { attempt, limit, .. } => {
            format!("resumed automatically after runner loss (attempt {attempt}/{limit})")
        }
        TaskEventKind::AutoResumeExhausted { attempts, reason } => format!(
            "automatic resume gave up after {attempts} {}: {}",
            if *attempts == 1 {
                "attempt"
            } else {
                "attempts"
            },
            exhaustion_loss(reason)
        ),
        TaskEventKind::SlotReleaseScheduled { due_at } => format!(
            "the slot is freed at {} unless the task gets work before",
            due_at.format("%H:%M:%S")
        ),
        TaskEventKind::SlotReleased { reason } => match reason {
            SlotReleaseReason::Terminal => {
                "slot freed: the task's work ended; the runner is asked to stop".to_string()
            }
            SlotReleaseReason::GraceElapsed => {
                "slot freed: the task waited for its owner too long; the runner is asked to stop"
                    .to_string()
            }
            SlotReleaseReason::Unrecognized => {
                "slot freed for a reason this version does not know".to_string()
            }
        },
        TaskEventKind::ReaderLeased { until } => format!(
            "a read-only runner is wanted for the stopped task until {}",
            until.format("%H:%M:%S")
        ),
        TaskEventKind::ReaderLeaseEnded => {
            "the read-only runner ran idle and is no longer wanted".to_string()
        }
        TaskEventKind::Unrecognized => "an event this version does not know".to_string(),
    };
    // Notes, reasons and details come from operators and runners: nothing a
    // terminal would act on gets through.
    let kind = without_unsafe_chars(&kind);
    format!(
        "{:>4}  {}  {:<24} {:<12} {kind}",
        event.sequence,
        event.at.to_rfc3339(),
        event.task_id,
        runner_state_name(event.runner_state)
    )
}

/// An operator command with its parameters, for the event log — an
/// override with the reason it was audited under.
fn describe_operator_command(command: &OperatorCommand) -> String {
    match command {
        OperatorCommand::Interrupt => "interrupt".to_string(),
        OperatorCommand::Transition {
            target,
            forced: false,
            ..
        } => format!("transition to '{target}'"),
        OperatorCommand::Transition {
            target,
            forced: true,
            reason,
        } => format!(
            "override transition to '{target}' (reason: {})",
            reason.as_deref().unwrap_or("none")
        ),
        OperatorCommand::RestartStep { note, failure_key } => format!(
            "restart step{}{}",
            if note.is_some() { " with a note" } else { "" },
            failure_key
                .as_deref()
                .map(|key| format!(" (failure {key})"))
                .unwrap_or_default()
        ),
        // The chat text is the owner's own words and the event log is no
        // place for them twice: only the length is told.
        OperatorCommand::ConductorChat { text } => {
            format!("conductor chat ({} characters)", text.chars().count())
        }
        OperatorCommand::ConfirmProposal { proposal_id } => {
            format!("confirm proposal {proposal_id}")
        }
        OperatorCommand::RejectProposal { proposal_id } => {
            format!("reject proposal {proposal_id}")
        }
        OperatorCommand::UndoDecision { decision_id } => format!("undo decision {decision_id}"),
        // Historical: no longer accepted, but still in old event logs.
        OperatorCommand::SetConductorHarness { harness } => {
            format!("conductor harness {harness}")
        }
    }
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
            let queued = if response.queued_for_slot {
                " and waits first in the queue for a slot"
            } else {
                ""
            };
            ToneLine::new(
                format!(
                    "{verb} accepted{replay}: task {} is now {}{queued} (generation {})",
                    response.task.task_id,
                    task_state_name(response.task.state),
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

/// The one-line state of the daemon's default agent harness: the effective
/// value and where it comes from, like [`default_backend_line`].
pub fn default_harness_line(selection: &HarnessSelectionSnapshot) -> String {
    match &selection.override_harness {
        Some(harness) => format!(
            "default harness: {} (override; configured {})",
            without_unsafe_chars(harness),
            without_unsafe_chars(&selection.configured)
        ),
        None => format!(
            "default harness: {} (configured)",
            without_unsafe_chars(&selection.effective)
        ),
    }
}
