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
    ConversationResponse, ConversationUnavailableReason, DefaultBackendView, Freshness,
    OwnerCommandResponse, QueueView, TaskSummary, TimelineResponse,
};
use crate::domain::{without_unsafe_chars, without_unsafe_chars_json};
use crate::orchestration::{
    AttentionItem, AttentionKind, ConversationAttemptSummary, InputStatus, OwnerCommand,
    PendingInputRef, QueueReason, TaskLifecycle,
};
use crate::tui::task_app::list::{attention_of, Attention};

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
        AttentionKind::FailedStep => "failed step",
        AttentionKind::ConductorProposal => "conductor proposal",
        AttentionKind::ProvisioningFailed => "provisioning failed",
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
                "Workflow   {} v{} · #{}",
                task.workflow_id, task.workflow_version, task.rank
            ),
            Tone::Normal,
        ),
    ];
    lines.splice(1..1, display_lines(task));
    if let Some(hint) = lifecycle_hint(task) {
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
