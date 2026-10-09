//! How an event of an old execution reads: one line for a list and every field
//! for a detail window.
//!
//! Pure functions of one [`Event`]; the *Zdarzenia* tab of
//! [`super::task_app`] draws what they return.

use serde_json::Value;

use crate::domain::{Event, EventData, InputCloseReason};
use crate::engine::actions::script::TRUNCATION_MARKER;

use super::scroll::sanitize;

/// A one-line description of an event, for the stream panel.
pub fn event_line(event: &Event) -> String {
    let detail = match &event.data {
        EventData::WorkflowStarted { workflow_id, .. } => {
            format!("workflow_started  {workflow_id}")
        }
        EventData::WorkflowPaused { reason } => format!("workflow_paused  {reason}"),
        EventData::WorkflowResumed => "workflow_resumed".to_string(),
        EventData::WorkflowCompleted { .. } => "workflow_completed".to_string(),
        EventData::WorkflowFailed { error } => format!("workflow_failed  {error}"),
        EventData::WorkflowCancelled { reason } => format!("workflow_cancelled  {reason}"),
        EventData::StateEntered { state_id } => format!("state_entered  {state_id}"),
        EventData::StateExited {
            state_id,
            output_label,
        } => format!("state_exited  {state_id} → {output_label}"),
        EventData::ActionStarted {
            state_id,
            action_type,
            ..
        } => format!("action_started  {state_id} ({action_type})"),
        EventData::ActionCompleted { output_label, .. } => {
            format!("action_completed  {output_label}")
        }
        EventData::ActionFailed { error, .. } => format!("action_failed  {error}"),
        EventData::ActionAttemptFailed {
            error, retry_in_ms, ..
        } => {
            format!("action_attempt_failed  {error} (retrying in {retry_in_ms}ms)")
        }
        EventData::ActionRetrying { retry_count, .. } => {
            format!("action_retrying  attempt {retry_count}")
        }
        EventData::ActionOutputRejected {
            action_execution_id,
            reason,
            ..
        } => format!(
            "action_output_rejected  attempt {}: {}",
            short_id(Some(action_execution_id)),
            first_line(reason)
        ),
        EventData::OutputReprompted {
            action_execution_id,
            previous_attempt,
            by,
            ..
        } => format!(
            "output_reprompted  by {} (attempt {}, after {})",
            by.name(),
            short_id(Some(action_execution_id)),
            short_id(Some(previous_attempt))
        ),
        EventData::DecisionRequested { provider } => format!("decision_requested  {provider}"),
        EventData::DecisionMade {
            provider,
            output_label,
            ..
        } => format!("decision_made  {provider} → {output_label}"),
        EventData::HumanInputRequested { instruction, .. } => {
            format!("human_input_requested  {instruction}")
        }
        EventData::HumanInputReceived {
            answered_by,
            close_reason,
            output_label,
            ..
        } => {
            let by = answered_by.unwrap_or_default().name();
            match (close_reason, output_label) {
                (Some(reason), _) if *reason != InputCloseReason::Answered => {
                    format!("human_input_received  closed by {} ({by})", reason.name())
                }
                (_, Some(label)) => format!("human_input_received  {by} → {label}"),
                _ => format!("human_input_received  {by}"),
            }
        }
        EventData::ProblemRaised {
            state_id,
            kind,
            reason,
            ..
        } => format!(
            "problem_raised  {} in {state_id}: {}",
            kind.name(),
            first_line(reason)
        ),
        EventData::ConductorStarted {
            trigger, purpose, ..
        } => format!("conductor_started  {} ({})", purpose.name(), trigger.name()),
        EventData::ConductorFinished { outcome, error, .. } => match error {
            Some(error) => format!(
                "conductor_finished  {}: {}",
                outcome.name(),
                first_line(error)
            ),
            None => format!("conductor_finished  {}", outcome.name()),
        },
        EventData::ConductorDecision {
            kind,
            output_label,
            confidence,
            ..
        } => match output_label {
            Some(label) => format!(
                "conductor_decision  {} → {label} ({})",
                kind.name(),
                confidence.name()
            ),
            None => format!(
                "conductor_decision  {} ({})",
                kind.name(),
                confidence.name()
            ),
        },
        EventData::ConductorDecisionUndone {
            decision_id,
            state_id,
            ..
        } => format!("conductor_decision_undone  {decision_id} → {state_id}"),
        EventData::HumanTransitionSelected {
            from_state,
            to_state,
        } => format!("human_transition_selected  {from_state} → {to_state}"),
        EventData::TransitionTaken {
            from_state,
            to_state,
            output_label,
        } => format!("transition_taken  {from_state} → {to_state} ({output_label})"),
        EventData::ArtifactCreated { artifact_id, .. } => {
            format!("artifact_created  {artifact_id}")
        }
        EventData::ExternalEventReceived { event_type, .. } => {
            format!("external_event_received  {event_type}")
        }
        EventData::Error { message, .. } => format!("error  {message}"),
        EventData::AgentSessionStarted {
            harness,
            action_execution_id,
            resumed_session,
            ..
        } => match resumed_session {
            Some(session) => format!(
                "agent_session_started  {harness} (attempt {}, resuming {})",
                short_id(action_execution_id.as_deref()),
                short_id(Some(session))
            ),
            None => format!(
                "agent_session_started  {harness} (attempt {})",
                short_id(action_execution_id.as_deref())
            ),
        },
        EventData::AgentSessionCompleted {
            action_execution_id,
            harness_session_id,
            ..
        } => format!(
            "agent_session_completed  attempt {} (session {})",
            short_id(action_execution_id.as_deref()),
            short_id(harness_session_id.as_deref())
        ),
        EventData::AgentSessionFailed {
            error,
            action_execution_id,
            ..
        } => format!(
            "agent_session_failed  {error} (attempt {})",
            short_id(action_execution_id.as_deref())
        ),
        EventData::AgentMessage {
            role,
            content,
            action_execution_id,
            ..
        } => format!(
            "agent_message  ({role}) attempt {}: {}",
            short_id(action_execution_id.as_deref()),
            first_line(content)
        ),
    };

    format!(
        "{:>4}  {}  {}",
        event.sequence,
        event.timestamp.format("%H:%M:%S"),
        detail
    )
}

/// The first line of a multi-line string, trimmed and elided, so a long
/// agent message cannot break the one-line-per-event stream panel.
fn first_line(text: &str) -> String {
    const LIMIT: usize = 80;
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() <= LIMIT {
        return line.to_string();
    }
    let cut: String = line.chars().take(LIMIT).collect();
    format!("{cut}…")
}

/// A short fragment of an id, for a one-line summary where the full UUID
/// would swamp everything else on the line. The full value is always in the
/// detail window ([`event_detail_lines`]).
fn short_id(id: Option<&str>) -> String {
    match id {
        Some(id) => id.chars().take(8).collect(),
        None => "unknown".to_string(),
    }
}

/// Every field of an event, for the detail window (R1).
///
/// Structured values are shown as indented JSON: the whole event is
/// serialized once and pretty-printed, which covers every current and future
/// [`EventData`] variant without a second exhaustive match to keep in step
/// with [`event_line`] — that one stays hand-written because its whole point
/// is a compiler-enforced one-liner per variant (R2), which a generic dump
/// cannot give.
pub fn event_detail_lines(event: &Event) -> Vec<String> {
    let mut lines = vec![sanitize(&event_line(event)), String::new()];

    if let EventData::AgentMessage { content, .. } = &event.data {
        if content.ends_with(TRUNCATION_MARKER) {
            lines.push(sanitize(
                "This message was truncated by the executor — the full text is in the conversation tab.",
            ));
            lines.push(String::new());
        }
    }

    let value = serde_json::to_value(event).unwrap_or(Value::Null);
    let pretty = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    lines.extend(pretty.lines().map(sanitize));

    lines
}
