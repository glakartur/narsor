//! Tests for the step timeline (U4): a pure, in-memory function over a
//! hand-built event log, an execution status and a conversation-presence
//! set. No file I/O, no engine, no real child processes — every scenario
//! here is built the same way `tests/engine_recovery.rs` builds its
//! crash-simulation logs, just without a real store behind it.

use std::collections::HashSet;

use narsor::domain::{Event, EventData, ExecutionStatus, RepromptedBy};
use narsor::engine::{build_timeline, ConversationState, EventLink, PositionStatus};
use serde_json::json;

const EXEC: &str = "exec-1";

/// Builds one event with the next sequence number, mirroring
/// `Event::new`'s signature so fixtures read like the real emission order.
struct Log {
    sequence: u64,
    events: Vec<Event>,
}

impl Log {
    fn new() -> Self {
        Self {
            sequence: 0,
            events: Vec::new(),
        }
    }

    fn push(&mut self, data: EventData) -> u64 {
        self.sequence += 1;
        let event = Event::new(EXEC.to_string(), data, self.sequence);
        let seq = event.sequence;
        self.events.push(event);
        seq
    }
}

fn state_entered(state_id: &str) -> EventData {
    EventData::StateEntered {
        state_id: state_id.to_string(),
    }
}

fn action_started(id: &str, state_id: &str, action_type: &str) -> EventData {
    EventData::ActionStarted {
        action_execution_id: id.to_string(),
        state_id: state_id.to_string(),
        action_type: action_type.to_string(),
    }
}

fn action_completed(id: &str, label: &str) -> EventData {
    EventData::ActionCompleted {
        action_execution_id: id.to_string(),
        output_label: label.to_string(),
    }
}

fn action_failed(id: &str, error: &str) -> EventData {
    EventData::ActionFailed {
        action_execution_id: id.to_string(),
        error: error.to_string(),
    }
}

fn action_attempt_failed(id: &str, state_id: &str, error: &str) -> EventData {
    EventData::ActionAttemptFailed {
        action_execution_id: id.to_string(),
        state_id: state_id.to_string(),
        error: error.to_string(),
        retry_in_ms: 500,
    }
}

fn action_retrying(id: &str, state_id: &str) -> EventData {
    EventData::ActionRetrying {
        action_execution_id: id.to_string(),
        retry_count: 1,
        state_id: Some(state_id.to_string()),
    }
}

fn decision_made(label: &str) -> EventData {
    EventData::DecisionMade {
        provider: "rules".to_string(),
        output_label: label.to_string(),
        reasoning: None,
    }
}

fn transition_taken(from: &str, to: &str, label: &str) -> EventData {
    EventData::TransitionTaken {
        from_state: from.to_string(),
        to_state: to.to_string(),
        output_label: label.to_string(),
    }
}

fn presence(ids: &[&str]) -> HashSet<String> {
    ids.iter().map(|id| id.to_string()).collect()
}

// ---------------------------------------------------------------------------
// Happy paths
// ---------------------------------------------------------------------------

/// Covers AE7: a failed first attempt followed by a successful second one
/// produces two independently-tracked positions.
#[test]
fn a_failed_attempt_then_a_successful_retry_gives_two_positions() {
    let mut log = Log::new();
    log.push(state_entered("plan"));
    log.push(action_started("a1", "plan", "agent"));
    log.push(action_attempt_failed("a1", "plan", "the harness crashed"));
    log.push(action_retrying("a2", "plan"));
    log.push(action_completed("a2", "success"));
    log.push(decision_made("success"));
    log.push(transition_taken("plan", "done", "success"));
    log.push(state_entered("done"));
    log.push(EventData::WorkflowCompleted { result: json!({}) });

    let timeline = build_timeline(&log.events, ExecutionStatus::Completed, &HashSet::new());

    assert_eq!(timeline.positions.len(), 2);

    let first = &timeline.positions[0];
    assert_eq!(first.label, "plan #1");
    assert_eq!(first.action_execution_id.as_deref(), Some("a1"));
    assert_eq!(first.status, PositionStatus::Failed);
    assert_eq!(first.error.as_deref(), Some("the harness crashed"));

    let second = &timeline.positions[1];
    assert_eq!(second.label, "plan #2");
    assert_eq!(second.action_execution_id.as_deref(), Some("a2"));
    assert_eq!(second.status, PositionStatus::Succeeded);
}

/// KTD7: a reprompt is an attempt of its own, with its own conversation,
/// next to the attempt whose output was rejected — which shows as failed
/// with the reason, never as a pending retry.
#[test]
fn a_rejected_output_and_its_reprompt_are_two_positions() {
    let mut log = Log::new();
    log.push(state_entered("work"));
    log.push(action_started("a1", "work", "agent"));
    let rejected_seq = log.push(EventData::ActionOutputRejected {
        action_execution_id: "a1".to_string(),
        state_id: "work".to_string(),
        reason: "no JSON object".to_string(),
        answer_tail: None,
    });
    let reprompted_seq = log.push(EventData::OutputReprompted {
        action_execution_id: "a2".to_string(),
        previous_attempt: "a1".to_string(),
        state_id: "work".to_string(),
        by: RepromptedBy::Engine,
    });
    log.push(action_completed("a2", "implemented"));
    log.push(decision_made("implemented"));
    log.push(transition_taken("work", "done", "implemented"));

    let timeline = build_timeline(
        &log.events,
        ExecutionStatus::Running,
        &presence(&["a1", "a2"]),
    );

    assert_eq!(timeline.positions.len(), 2);
    let rejected = &timeline.positions[0];
    assert_eq!(rejected.label, "work #1");
    assert_eq!(rejected.status, PositionStatus::Failed);
    assert!(!rejected.retry_pending);
    assert_eq!(
        rejected.error.as_deref(),
        Some("output rejected: no JSON object")
    );

    let reprompt = &timeline.positions[1];
    assert_eq!(reprompt.label, "work #2");
    assert_eq!(reprompt.action_execution_id.as_deref(), Some("a2"));
    assert_eq!(reprompt.action_type.as_deref(), Some("agent"));
    assert_eq!(reprompt.status, PositionStatus::Succeeded);
    assert_eq!(reprompt.conversation, ConversationState::Recorded);

    assert_eq!(
        timeline.link(rejected_seq),
        Some(&EventLink::Position("work #1".to_string()))
    );
    assert_eq!(
        timeline.link(reprompted_seq),
        Some(&EventLink::Position("work #2".to_string()))
    );
}

/// Covers AE2: the final `ActionFailed` (no more retries) links to the
/// attempt it belongs to, not the first one.
#[test]
fn action_failed_of_the_second_attempt_links_to_its_own_position() {
    let mut log = Log::new();
    log.push(state_entered("plan"));
    log.push(action_started("a1", "plan", "agent"));
    log.push(action_attempt_failed("a1", "plan", "first try failed"));
    log.push(action_retrying("a2", "plan"));
    let failed_seq = log.push(action_failed("a2", "second try failed too"));

    let timeline = build_timeline(&log.events, ExecutionStatus::Unknown, &HashSet::new());

    assert_eq!(
        timeline.link(failed_seq),
        Some(&EventLink::Position("plan #2".to_string()))
    );
    assert_eq!(timeline.positions[1].status, PositionStatus::Failed);
}

/// A loop: `plan` visited twice, the second visit retried once. Numbering is
/// global across the execution, and a label assigned to a position never
/// changes once later events are appended (safe to use as a selection key).
#[test]
fn a_loop_numbers_attempts_globally_and_labels_stay_stable() {
    let mut log = Log::new();
    log.push(state_entered("plan"));
    log.push(action_started("a1", "plan", "agent"));
    log.push(action_completed("a1", "retry"));
    log.push(decision_made("retry"));
    let after_first_visit = log.push(transition_taken("plan", "plan", "retry"));

    let partial_timeline = build_timeline(&log.events, ExecutionStatus::Running, &HashSet::new());
    assert_eq!(
        partial_timeline
            .positions
            .iter()
            .map(|p| p.label.clone())
            .collect::<Vec<_>>(),
        vec!["plan #1"]
    );
    let _ = after_first_visit;

    log.push(state_entered("plan"));
    log.push(action_started("a2", "plan", "agent"));
    log.push(action_attempt_failed("a2", "plan", "flaky"));
    log.push(action_retrying("a3", "plan"));
    log.push(action_completed("a3", "success"));
    log.push(decision_made("success"));
    log.push(transition_taken("plan", "done", "success"));
    log.push(state_entered("done"));
    log.push(EventData::WorkflowCompleted { result: json!({}) });

    let full_timeline = build_timeline(&log.events, ExecutionStatus::Completed, &HashSet::new());

    let labels: Vec<&str> = full_timeline
        .positions
        .iter()
        .map(|p| p.label.as_str())
        .collect();
    assert_eq!(labels, vec!["plan #1", "plan #2", "plan #3"]);

    let visits: Vec<u32> = full_timeline.positions.iter().map(|p| p.visit).collect();
    assert_eq!(visits, vec![1, 2, 2]);

    // Appending more events did not change the first position's label.
    assert_eq!(
        full_timeline.positions[0].label,
        partial_timeline.positions[0].label
    );
}

/// Happy path: default selection picks the currently-running attempt.
#[test]
fn default_selection_is_the_running_attempt() {
    let mut log = Log::new();
    log.push(state_entered("plan"));
    log.push(action_started("a1", "plan", "agent"));

    let timeline = build_timeline(&log.events, ExecutionStatus::Running, &HashSet::new());

    assert_eq!(timeline.positions[0].status, PositionStatus::InProgress);
    assert_eq!(timeline.default_position.as_deref(), Some("plan #1"));
}

/// Happy path: with nothing running, default selection falls back to the
/// last position.
#[test]
fn default_selection_falls_back_to_the_last_position() {
    let mut log = Log::new();
    log.push(state_entered("plan"));
    log.push(action_started("a1", "plan", "script"));
    log.push(action_completed("a1", "success"));
    log.push(decision_made("success"));
    log.push(transition_taken("plan", "done", "success"));
    log.push(state_entered("done"));
    log.push(EventData::WorkflowCompleted { result: json!({}) });

    let timeline = build_timeline(&log.events, ExecutionStatus::Completed, &HashSet::new());

    assert_eq!(timeline.default_position.as_deref(), Some("plan #1"));
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

/// A backoff pause: the failed attempt has no `ActionRetrying` yet, so it is
/// `Failed` with `retry_pending` set.
#[test]
fn a_backoff_pause_is_failed_with_retry_pending() {
    let mut log = Log::new();
    log.push(state_entered("plan"));
    log.push(action_started("a1", "plan", "agent"));
    log.push(action_attempt_failed("a1", "plan", "timed out"));

    let timeline = build_timeline(&log.events, ExecutionStatus::Running, &HashSet::new());

    let position = &timeline.positions[0];
    assert_eq!(position.status, PositionStatus::Failed);
    assert!(position.retry_pending);
}

/// A recovery `Error` naming the interrupted attempt marks it `Interrupted`,
/// not `Failed`.
#[test]
fn a_recovery_error_interrupts_the_open_attempt() {
    let mut log = Log::new();
    log.push(state_entered("first"));
    log.push(action_started("a1", "first", "script"));
    log.push(EventData::Error {
        message: "action 'a1' of state 'first' was still running when the narsor stopped"
            .to_string(),
        context: json!({
            "state": "first",
            "action_execution_id": "a1",
            "reconciliation": "interrupted_action",
        }),
    });

    let timeline = build_timeline(&log.events, ExecutionStatus::Unknown, &HashSet::new());

    let position = &timeline.positions[0];
    assert_eq!(position.status, PositionStatus::Interrupted);
    assert!(position.error.as_ref().unwrap().contains("still running"));
}

/// A manual transition while an attempt is still open interrupts it — it
/// must not be reported as merely "in progress".
#[test]
fn a_manual_transition_interrupts_the_open_attempt_instead_of_running() {
    let mut log = Log::new();
    log.push(state_entered("first"));
    log.push(action_started("a1", "first", "agent"));
    log.push(EventData::HumanTransitionSelected {
        from_state: "first".to_string(),
        to_state: "second".to_string(),
    });
    log.push(state_entered("second"));

    let timeline = build_timeline(&log.events, ExecutionStatus::Running, &HashSet::new());

    assert_eq!(timeline.positions.len(), 1);
    assert_eq!(timeline.positions[0].status, PositionStatus::Interrupted);
}

/// A timeout the executor turned into a label keeps the attempt
/// `Succeeded`, but flags that the harness session itself was interrupted.
#[test]
fn a_salvaged_timeout_stays_succeeded_but_flags_the_session() {
    let mut log = Log::new();
    log.push(state_entered("agent_step"));
    log.push(action_started("a1", "agent_step", "agent"));
    log.push(EventData::AgentSessionStarted {
        session_id: "s1".to_string(),
        harness: "claude-code".to_string(),
        action_execution_id: Some("a1".to_string()),
        resumed_session: None,
    });
    log.push(EventData::AgentSessionFailed {
        session_id: "s1".to_string(),
        error: "timed out".to_string(),
        action_execution_id: Some("a1".to_string()),
        harness_session_id: None,
    });
    log.push(action_completed("a1", "partial_success"));

    let timeline = build_timeline(&log.events, ExecutionStatus::Running, &HashSet::new());

    let position = &timeline.positions[0];
    assert_eq!(position.status, PositionStatus::Succeeded);
    assert!(position.harness_session_interrupted);
}

/// An agent's action completed, but the execution parked itself awaiting a
/// human decision before transitioning: the position shows "awaiting
/// decision", not a plain "succeeded".
#[test]
fn a_completed_action_awaiting_a_human_decision_is_flagged() {
    let mut log = Log::new();
    log.push(state_entered("review"));
    log.push(action_started("a1", "review", "agent"));
    log.push(action_completed("a1", "checked"));
    log.push(EventData::WorkflowPaused {
        reason: "waiting for a human decision".to_string(),
    });

    let timeline = build_timeline(&log.events, ExecutionStatus::InputNeeded, &HashSet::new());

    assert_eq!(
        timeline.positions[0].status,
        PositionStatus::AwaitingDecision
    );
}

/// Covers AE6: an old-format log whose agent events predate
/// `action_execution_id` still attributes every event to the right
/// position — only the conversation itself cannot be found.
#[test]
fn an_old_format_log_still_attributes_events_but_has_no_conversation() {
    let mut log = Log::new();
    log.push(state_entered("plan"));
    log.push(action_started("a1", "plan", "agent"));
    let agent_started_seq = log.push(EventData::AgentSessionStarted {
        session_id: "s1".to_string(),
        harness: "claude-code".to_string(),
        action_execution_id: None,
        resumed_session: None,
    });
    let agent_message_seq = log.push(EventData::AgentMessage {
        session_id: "s1".to_string(),
        content: "hello".to_string(),
        role: "assistant".to_string(),
        action_execution_id: None,
    });
    log.push(action_completed("a1", "success"));

    // Even a real conversation store's presence set cannot help here — the
    // id needed to look it up was never recorded.
    let timeline = build_timeline(&log.events, ExecutionStatus::Completed, &presence(&["a1"]));

    assert_eq!(
        timeline.positions[0].conversation,
        ConversationState::NoRecord
    );
    assert_eq!(
        timeline.link(agent_started_seq),
        Some(&EventLink::Position("plan #1".to_string()))
    );
    assert_eq!(
        timeline.link(agent_message_seq),
        Some(&EventLink::Position("plan #1".to_string()))
    );
}

/// A template error before the executor ever ran: no attempt exists yet, so
/// a position is synthesized to carry the error, marked "conversation not
/// created".
#[test]
fn a_template_error_before_the_executor_ran_synthesizes_a_position() {
    let mut log = Log::new();
    log.push(state_entered("plan"));
    log.push(EventData::Error {
        message: "template error: unknown variable 'subject'".to_string(),
        context: json!({}),
    });

    let timeline = build_timeline(&log.events, ExecutionStatus::Failed, &HashSet::new());

    assert_eq!(timeline.positions.len(), 1);
    let position = &timeline.positions[0];
    assert_eq!(position.label, "plan #1");
    assert!(position.action_execution_id.is_none());
    assert_eq!(position.status, PositionStatus::Failed);
    assert_eq!(
        position.conversation,
        ConversationState::NotCreated {
            reason: "template error: unknown variable 'subject'".to_string()
        }
    );
}

/// Covers AE3: workflow lifecycle events and human input never belong to an
/// agent conversation; a script step's own events still get a position (R7
/// asks for every attempt on the axis), but that position itself carries no
/// conversation.
#[test]
fn workflow_and_human_events_have_no_link_and_script_steps_have_no_conversation() {
    let mut log = Log::new();
    let started_seq = log.push(EventData::WorkflowStarted {
        workflow_id: "wf".to_string(),
        workflow_version: 1,
        inputs: json!({}),
    });
    log.push(state_entered("gather"));
    let human_input_seq = log.push(EventData::human_input_received(json!({})));
    log.push(state_entered("build"));
    log.push(action_started("a1", "build", "script"));
    log.push(action_completed("a1", "success"));

    let timeline = build_timeline(&log.events, ExecutionStatus::Running, &HashSet::new());

    assert!(matches!(
        timeline.link(started_seq),
        Some(EventLink::NoLink { .. })
    ));
    assert!(matches!(
        timeline.link(human_input_seq),
        Some(EventLink::NoLink { .. })
    ));

    let script_position = timeline
        .position("build #1")
        .expect("the script position exists");
    assert!(matches!(
        script_position.conversation,
        ConversationState::NotCreated { .. }
    ));
}

/// A terminal state's `StateEntered` never links to anything — there is no
/// attempt in it to attribute the event to.
#[test]
fn state_entered_of_a_terminal_state_has_no_link() {
    let mut log = Log::new();
    log.push(state_entered("plan"));
    log.push(action_started("a1", "plan", "script"));
    log.push(action_completed("a1", "success"));
    log.push(decision_made("success"));
    log.push(transition_taken("plan", "done", "success"));
    let terminal_seq = log.push(state_entered("done"));
    log.push(EventData::WorkflowCompleted { result: json!({}) });

    let timeline = build_timeline(&log.events, ExecutionStatus::Completed, &HashSet::new());

    assert!(matches!(
        timeline.link(terminal_seq),
        Some(EventLink::NoLink { .. })
    ));
}

/// Happy path: a normal, new-format attempt whose conversation is present
/// in the caller's presence set reads as `Recorded`.
#[test]
fn a_present_conversation_reads_as_recorded() {
    let mut log = Log::new();
    log.push(state_entered("plan"));
    log.push(action_started("a1", "plan", "agent"));
    log.push(action_completed("a1", "success"));

    let timeline = build_timeline(&log.events, ExecutionStatus::Running, &presence(&["a1"]));

    assert_eq!(
        timeline.positions[0].conversation,
        ConversationState::Recorded
    );
}
