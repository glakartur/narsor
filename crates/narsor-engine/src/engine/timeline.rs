//! The step timeline: a deterministic view of an execution's attempts,
//! computed from the event log alone.
//!
//! Placed in `src/engine` rather than `src/domain` (deferred to
//! implementation time by the plan) because it lives next to
//! [`crate::engine::decision`] and [`crate::engine::retry`], the engine's
//! other pure functions over domain types: `domain` holds data definitions,
//! while judgement calls that mirror the runner's own emission order —
//! exactly what this module makes — belong with the engine that produces
//! that order. [`crate::persistence::recovery`] is the closest existing
//! precedent for "fold over the event log to reconstruct what happened", and
//! its `unfinished_action` is the pattern this module's fold follows.
//!
//! # What a position is
//!
//! One [`Position`] is one entry of the timeline: the attempts of one
//! agent session on a step, or one script or human attempt. The fold first
//! follows every attempt (an `ActionStarted`, `ActionRetrying` or
//! `OutputReprompted` event and what happens to it, keyed by
//! `action_execution_id`) and then groups them:
//!
//! - an attempt that resumes a harness session of its step, when nothing
//!   else came after that session's entry (the agent asked and was
//!   answered, or was reprompted), joins that entry: same position, with
//!   the attempt added to [`Position::attempts`] and its status that of the
//!   latest attempt, so it is active again;
//! - one that resumes a session of its step after other entries (another
//!   step, or another session) is an entry of its own, numbered like the
//!   session it returns to;
//! - any other attempt starts a session (or a script run) and takes the
//!   next number of its step.
//!
//! # Why the label is the identity
//!
//! A label is `"<state> #<session number>"`, with `".k"` after the number on
//! the `k`th entry (from the second) of one session. Numbers only grow and a
//! label is unique, so a caller can hold onto it as a selection across a
//! refresh (R6). It changes only when a later attempt joins an entry that
//! was shown alone for a moment (the resumed session is known once its
//! start is logged). What the agents used is not in the log: see
//! [`crate::engine::timeline_file`].
//!
//! # The status precedence rule
//!
//! Checked in this exact order, first match wins for the *base outcome*:
//!
//! 1. `ActionCompleted` → [`PositionStatus::Succeeded`].
//! 2. `ActionFailed` / `ActionAttemptFailed` → [`PositionStatus::Failed`].
//! 3. A recovery `Error` naming this attempt, a manual transition while this
//!    attempt was still open, or a new visit starting while it was still
//!    open → [`PositionStatus::Interrupted`] (this overrides 1 and 2: an
//!    interrupted attempt is never shown as merely succeeded or failed).
//! 4. Still open (no completion event at all) in a `Running` execution →
//!    [`PositionStatus::InProgress`].
//! 5. Already `Succeeded` (1 fired), but the state hasn't transitioned out of
//!    yet and the execution is `InputNeeded` → [`PositionStatus::AwaitingDecision`].
//!    This is the one place a later fact (the execution parked itself
//!    waiting on a human) is allowed to relabel an already-resolved outcome:
//!    the action truly did complete, but nothing has acted on that yet.
//!
//! A timed-out harness session whose executor still salvaged a label keeps
//! `Succeeded` (rule 1), but [`Position::harness_session_interrupted`] is set
//! so the distinction is not lost. A failed attempt with retries left keeps
//! `Failed` (rule 2) but [`Position::retry_pending`] is set when nothing has
//! picked it up yet.
//!
//! # Conversation state
//!
//! Every position carries a [`ConversationState`]:
//! - [`ConversationState::Recorded`]: the caller's presence set contains this
//!   attempt's `action_execution_id`.
//! - [`ConversationState::NotCreated`]: a script action (nothing was ever
//!   going to write one), or an agent attempt whose conversation genuinely
//!   never started — a template error before the executor ran being the
//!   sharpest case, where there is no `action_execution_id` at all yet and
//!   this module synthesizes a position for the visit anyway, so the error
//!   has somewhere to attach.
//! - [`ConversationState::NoRecord`]: an old-format log whose agent session
//!   events predate `action_execution_id` (U1). The position is still
//!   correctly attributed — the fold falls back to positional linking for
//!   exactly this case — only the conversation itself cannot be found.
//!
//! # Event links
//!
//! Every event either belongs to a position (by its own
//! `action_execution_id` when the variant carries one, or positionally to
//! the last attempt of the current visit otherwise) or it does not — workflow
//! runner state events, human input, external events, and `StateEntered` itself
//! are never part of an agent conversation, and the link map says why.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Utc};

use crate::domain::{Event, EventData, ExecutionStatus};

pub use narsor_protocol::api::{ConversationState, EventLink, Position, PositionStatus, Timeline};

/// Reasons used for [`EventLink::NoLink`], centralized so the wording stays
/// consistent.
mod reasons {
    pub const WORKFLOW_LIFECYCLE: &str = "does not belong to an agent conversation";
    pub const STATE_ENTERED: &str = "entering a state is not itself part of an agent conversation";
    pub const HUMAN_OR_MANUAL: &str = "does not belong to an agent conversation";
    pub const NO_ATTEMPT_YET: &str = "no attempt has started in this visit yet";
}

/// An event's link while folding: positions are not labelled until attempts
/// have been grouped into entries.
enum Link {
    Position(usize),
    NoLink(String),
}

/// One attempt under construction while folding the log.
struct Draft {
    state_id: String,
    visit: u32,
    action_execution_id: Option<String>,
    action_type: Option<String>,
    decision_by: Option<String>,
    outcome: Option<Outcome>,
    interrupted: bool,
    resolved_via_attempt_failed: bool,
    visit_closed: bool,
    started_at: Option<DateTime<Utc>>,
    ended_at: Option<DateTime<Utc>>,
    error: Option<String>,
    resumed_harness_session: Option<String>,
    harness_session_interrupted: bool,
    has_agent_event_missing_id: bool,
    harness: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    harness_session_id: Option<String>,
    /// Set only for the synthesized "template error before the executor
    /// ran" position, which already knows its final conversation state and
    /// outcome at creation time.
    conversation_override: Option<ConversationState>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Succeeded,
    Failed,
}

/// Builds the step timeline from an execution's event log.
///
/// Pure and in-memory: no file I/O happens here. `conversation_presence` is
/// the set of `action_execution_id`s the caller already knows have a
/// conversation record (from [`crate::persistence::ConversationStore`], or
/// any equivalent — this module only needs the answer to "does attempt X
/// have one", not how it was found).
///
/// `events` is assumed sorted by `sequence`, as every event log in this
/// codebase is.
pub fn build_timeline(
    events: &[Event],
    execution_status: ExecutionStatus,
    conversation_presence: &HashSet<String>,
) -> Timeline {
    let mut drafts: Vec<Draft> = Vec::new();
    let mut by_attempt: HashMap<String, usize> = HashMap::new();
    let mut state_visit_counts: HashMap<String, u32> = HashMap::new();
    let mut current_visit_state: Option<String> = None;
    let mut current_visit_last_position: Option<usize> = None;
    let mut open_position: Option<usize> = None;
    let mut links: BTreeMap<u64, Link> = BTreeMap::new();

    for event in events {
        let seq = event.sequence;

        match &event.data {
            EventData::Unrecognized => {}
            EventData::WorkflowStarted { .. }
            | EventData::WorkflowPaused { .. }
            | EventData::WorkflowResumed
            | EventData::WorkflowCompleted { .. }
            | EventData::WorkflowFailed { .. }
            | EventData::WorkflowCancelled { .. } => {
                links.insert(seq, Link::NoLink(reasons::WORKFLOW_LIFECYCLE.to_string()));
            }

            EventData::StateEntered { state_id } => {
                // A new visit starting while the previous attempt was still
                // open cuts it short (rule 3).
                if let Some(idx) = open_position.take() {
                    drafts[idx].interrupted = true;
                }
                state_visit_counts
                    .entry(state_id.clone())
                    .and_modify(|count| *count += 1)
                    .or_insert(1);
                current_visit_state = Some(state_id.clone());
                current_visit_last_position = None;
                links.insert(seq, Link::NoLink(reasons::STATE_ENTERED.to_string()));
            }

            EventData::ActionStarted {
                action_execution_id,
                state_id,
                action_type,
                decision_by,
            } => {
                let idx = push_attempt(
                    &mut drafts,
                    &state_visit_counts,
                    state_id.clone(),
                    Some(action_execution_id.clone()),
                    Some(action_type.clone()),
                    decision_by.clone(),
                    event.timestamp,
                );
                by_attempt.insert(action_execution_id.clone(), idx);
                current_visit_last_position = Some(idx);
                open_position = Some(idx);
                links.insert(seq, Link::Position(idx));
            }

            EventData::ActionRetrying {
                action_execution_id,
                state_id,
                ..
            } => {
                let state_id = state_id.clone().or_else(|| current_visit_state.clone());
                let Some(state_id) = state_id else {
                    // No idea which state this belongs to (an ancient log
                    // with neither field) — nothing sensible to attach it to.
                    links.insert(seq, Link::NoLink(reasons::NO_ATTEMPT_YET.to_string()));
                    continue;
                };
                let inherited_action_type =
                    current_visit_last_position.and_then(|idx| drafts[idx].action_type.clone());
                let inherited_decision_by =
                    current_visit_last_position.and_then(|idx| drafts[idx].decision_by.clone());
                let idx = push_attempt(
                    &mut drafts,
                    &state_visit_counts,
                    state_id,
                    Some(action_execution_id.clone()),
                    inherited_action_type,
                    inherited_decision_by,
                    event.timestamp,
                );
                by_attempt.insert(action_execution_id.clone(), idx);
                current_visit_last_position = Some(idx);
                open_position = Some(idx);
                links.insert(seq, Link::Position(idx));
            }

            EventData::OutputReprompted {
                action_execution_id,
                state_id,
                ..
            } => {
                // A reprompt is a new attempt of the same visit, exactly like
                // a retry, and runs the same (agent) action.
                let inherited_action_type =
                    current_visit_last_position.and_then(|idx| drafts[idx].action_type.clone());
                let inherited_decision_by =
                    current_visit_last_position.and_then(|idx| drafts[idx].decision_by.clone());
                let idx = push_attempt(
                    &mut drafts,
                    &state_visit_counts,
                    state_id.clone(),
                    Some(action_execution_id.clone()),
                    inherited_action_type,
                    inherited_decision_by,
                    event.timestamp,
                );
                by_attempt.insert(action_execution_id.clone(), idx);
                current_visit_last_position = Some(idx);
                open_position = Some(idx);
                links.insert(seq, Link::Position(idx));
            }

            EventData::ActionOutputRejected {
                action_execution_id,
                reason,
                ..
            } => {
                // The attempt is over without a usable label: shown as failed
                // with the reason, but never as a pending `retry_policy` retry.
                if let Some(&idx) = by_attempt.get(action_execution_id) {
                    drafts[idx].outcome = Some(Outcome::Failed);
                    drafts[idx].ended_at = Some(event.timestamp);
                    drafts[idx].error = Some(format!("output rejected: {reason}"));
                    if open_position == Some(idx) {
                        open_position = None;
                    }
                    links.insert(seq, Link::Position(idx));
                } else {
                    links.insert(seq, Link::NoLink(reasons::NO_ATTEMPT_YET.to_string()));
                }
            }

            EventData::ActionCompleted {
                action_execution_id,
                ..
            } => {
                if let Some(&idx) = by_attempt.get(action_execution_id) {
                    drafts[idx].outcome = Some(Outcome::Succeeded);
                    drafts[idx].ended_at = Some(event.timestamp);
                    if open_position == Some(idx) {
                        open_position = None;
                    }
                    links.insert(seq, Link::Position(idx));
                } else {
                    links.insert(seq, Link::NoLink(reasons::NO_ATTEMPT_YET.to_string()));
                }
            }

            EventData::ActionFailed {
                action_execution_id,
                error,
                ..
            } => {
                if let Some(&idx) = by_attempt.get(action_execution_id) {
                    drafts[idx].outcome = Some(Outcome::Failed);
                    drafts[idx].ended_at = Some(event.timestamp);
                    drafts[idx].error = Some(error.clone());
                    if open_position == Some(idx) {
                        open_position = None;
                    }
                    links.insert(seq, Link::Position(idx));
                } else {
                    links.insert(seq, Link::NoLink(reasons::NO_ATTEMPT_YET.to_string()));
                }
            }

            EventData::ActionAttemptFailed {
                action_execution_id,
                error,
                ..
            } => {
                if let Some(&idx) = by_attempt.get(action_execution_id) {
                    drafts[idx].outcome = Some(Outcome::Failed);
                    drafts[idx].resolved_via_attempt_failed = true;
                    drafts[idx].ended_at = Some(event.timestamp);
                    drafts[idx].error = Some(error.clone());
                    if open_position == Some(idx) {
                        open_position = None;
                    }
                    links.insert(seq, Link::Position(idx));
                } else {
                    links.insert(seq, Link::NoLink(reasons::NO_ATTEMPT_YET.to_string()));
                }
            }

            EventData::AgentSessionStarted {
                action_execution_id,
                resumed_session,
                harness,
                model,
                effort,
                ..
            } => {
                link_agent_event(
                    &mut drafts,
                    &by_attempt,
                    current_visit_last_position,
                    action_execution_id,
                    &mut links,
                    seq,
                    |draft| {
                        if let Some(resumed) = resumed_session {
                            draft.resumed_harness_session = Some(resumed.clone());
                        }
                        draft.harness = Some(harness.clone());
                        draft.model = model.clone();
                        draft.effort = effort.clone();
                    },
                );
            }

            EventData::AgentSessionCompleted {
                action_execution_id,
                harness_session_id,
                ..
            } => {
                link_agent_event(
                    &mut drafts,
                    &by_attempt,
                    current_visit_last_position,
                    action_execution_id,
                    &mut links,
                    seq,
                    |draft| {
                        if harness_session_id.is_some() {
                            draft.harness_session_id = harness_session_id.clone();
                        }
                    },
                );
            }

            EventData::AgentSessionFailed {
                action_execution_id,
                harness_session_id,
                ..
            } => {
                link_agent_event(
                    &mut drafts,
                    &by_attempt,
                    current_visit_last_position,
                    action_execution_id,
                    &mut links,
                    seq,
                    |draft| {
                        draft.harness_session_interrupted = true;
                        if harness_session_id.is_some() {
                            draft.harness_session_id = harness_session_id.clone();
                        }
                    },
                );
            }

            EventData::AgentMessage {
                action_execution_id,
                ..
            }
            | EventData::PermissionDeniedIgnored {
                action_execution_id,
                ..
            } => {
                link_agent_event(
                    &mut drafts,
                    &by_attempt,
                    current_visit_last_position,
                    action_execution_id,
                    &mut links,
                    seq,
                    |_draft| {},
                );
            }

            EventData::DecisionRequested { .. } => {
                links.insert(seq, Link::NoLink(reasons::WORKFLOW_LIFECYCLE.to_string()));
            }

            EventData::EntryLimitReached { .. }
            | EventData::EntryLimitDecision { .. }
            | EventData::EntryLimitCounterReset { .. } => {
                links.insert(seq, Link::NoLink(reasons::WORKFLOW_LIFECYCLE.to_string()));
            }

            // The conductor's work is not an attempt of its own (D55): it is
            // pinned to the visit it happened in, like a decision.
            EventData::DecisionMade { .. }
            | EventData::VariableSetByWorkflow { .. }
            | EventData::ArtifactCreated { .. }
            | EventData::ProblemRaised { .. }
            | EventData::ConductorStarted { .. }
            | EventData::ConductorFinished { .. }
            | EventData::ConductorDecision { .. }
            | EventData::ConductorDecisionUndone { .. } => {
                link_positional(current_visit_last_position, &mut links, seq);
            }

            EventData::TransitionTaken { .. } => {
                if let Some(idx) = current_visit_last_position {
                    drafts[idx].visit_closed = true;
                    links.insert(seq, Link::Position(idx));
                } else {
                    links.insert(seq, Link::NoLink(reasons::NO_ATTEMPT_YET.to_string()));
                }
            }

            EventData::StateExited { .. } => {
                if let Some(idx) = current_visit_last_position {
                    drafts[idx].visit_closed = true;
                    links.insert(seq, Link::Position(idx));
                } else {
                    links.insert(seq, Link::NoLink(reasons::NO_ATTEMPT_YET.to_string()));
                }
            }

            EventData::HumanInputRequested { .. } | EventData::HumanInputReceived { .. } => {
                links.insert(seq, Link::NoLink(reasons::HUMAN_OR_MANUAL.to_string()));
            }

            EventData::HumanTransitionSelected { .. } => {
                if let Some(idx) = open_position.take() {
                    drafts[idx].interrupted = true;
                }
                links.insert(seq, Link::NoLink(reasons::HUMAN_OR_MANUAL.to_string()));
            }

            EventData::ExternalEventReceived { .. } => {
                links.insert(seq, Link::NoLink(reasons::WORKFLOW_LIFECYCLE.to_string()));
            }

            EventData::Error { message, context } => {
                let reconciliation_target = context
                    .get("reconciliation")
                    .and_then(|value| value.as_str())
                    .filter(|value| *value == "interrupted_action")
                    .and_then(|_| context.get("action_execution_id"))
                    .and_then(|value| value.as_str());

                if let Some(id) = reconciliation_target {
                    if let Some(&idx) = by_attempt.get(id) {
                        drafts[idx].interrupted = true;
                        drafts[idx].error = Some(message.clone());
                        if open_position == Some(idx) {
                            open_position = None;
                        }
                        links.insert(seq, Link::Position(idx));
                        continue;
                    }
                }

                if current_visit_last_position.is_none() {
                    // A template error before the executor ever ran: no
                    // attempt exists for this visit yet, so synthesize a
                    // position that carries the error, per the plan's Open
                    // Questions ("conversation not created" case).
                    let state_id = current_visit_state
                        .clone()
                        .unwrap_or_else(|| "unknown".to_string());
                    let idx = push_attempt(
                        &mut drafts,
                        &state_visit_counts,
                        state_id,
                        None,
                        None,
                        None,
                        event.timestamp,
                    );
                    drafts[idx].outcome = Some(Outcome::Failed);
                    drafts[idx].ended_at = Some(event.timestamp);
                    drafts[idx].error = Some(message.clone());
                    drafts[idx].conversation_override = Some(ConversationState::NotCreated {
                        reason: message.clone(),
                    });
                    current_visit_last_position = Some(idx);
                    links.insert(seq, Link::Position(idx));
                } else {
                    link_positional(current_visit_last_position, &mut links, seq);
                }
            }
        }
    }

    let entries = group_entries(&drafts);
    let mut entry_of_draft = vec![0usize; drafts.len()];
    for (entry_idx, entry) in entries.iter().enumerate() {
        for &draft_idx in &entry.drafts {
            entry_of_draft[draft_idx] = entry_idx;
        }
    }

    let last_entry = entries.len().checked_sub(1);
    let attempt_ids: Vec<Vec<Option<String>>> = entries
        .iter()
        .map(|entry| {
            entry
                .drafts
                .iter()
                .map(|&idx| drafts[idx].action_execution_id.clone())
                .collect()
        })
        .collect();
    let mut slots: Vec<Option<Draft>> = drafts.into_iter().map(Some).collect();
    let mut positions: Vec<Position> = Vec::with_capacity(entries.len());
    for (entry_idx, entry) in entries.iter().enumerate() {
        let mut members: Vec<Draft> = entry
            .drafts
            .iter()
            .map(|&idx| slots[idx].take().expect("a draft belongs to one entry"))
            .collect();
        let mut latest = members.pop().expect("an entry has an attempt");
        let first = members.first();
        if let Some(first) = first {
            latest.started_at = first.started_at;
            latest.resumed_harness_session = first
                .resumed_harness_session
                .clone()
                .or(latest.resumed_harness_session);
        }
        latest.has_agent_event_missing_id |= members.iter().any(|m| m.has_agent_event_missing_id);
        latest.harness = latest
            .harness
            .or_else(|| members.iter().rev().find_map(|m| m.harness.clone()));
        latest.model = latest
            .model
            .or_else(|| members.iter().rev().find_map(|m| m.model.clone()));
        latest.effort = latest
            .effort
            .or_else(|| members.iter().rev().find_map(|m| m.effort.clone()));
        latest.harness_session_id = latest.harness_session_id.or_else(|| {
            members
                .iter()
                .rev()
                .find_map(|m| m.harness_session_id.clone())
        });
        let earlier_recorded = members.iter().any(|m| {
            m.action_execution_id
                .as_ref()
                .is_some_and(|id| conversation_presence.contains(id))
        });
        let mut position = finalize(
            latest,
            execution_status,
            Some(entry_idx) == last_entry,
            conversation_presence,
        );
        if earlier_recorded && position.conversation != ConversationState::Recorded {
            position.conversation = ConversationState::Recorded;
        }
        position.session_number = Some(entry.session_number);
        position.label = entry.label(&position.state_id);
        position.attempts = attempt_ids[entry_idx].iter().flatten().cloned().collect();
        positions.push(position);
    }

    let links: BTreeMap<u64, EventLink> = links
        .into_iter()
        .map(|(seq, link)| {
            let link = match link {
                Link::Position(idx) => {
                    EventLink::Position(positions[entry_of_draft[idx]].label.clone())
                }
                Link::NoLink(reason) => EventLink::NoLink { reason },
            };
            (seq, link)
        })
        .collect();

    let default_position = positions
        .iter()
        .find(|position| position.status == PositionStatus::InProgress)
        .or_else(|| positions.last())
        .map(|position| position.label.clone());

    Timeline {
        positions,
        links,
        default_position,
    }
}

/// The attempts that make up one entry of the timeline.
struct Entry {
    drafts: Vec<usize>,
    session_number: u32,
    /// Which entry of its session this is, from 1.
    ordinal: u32,
}

impl Entry {
    fn label(&self, state_id: &str) -> String {
        if self.ordinal <= 1 {
            format!("{state_id} #{}", self.session_number)
        } else {
            format!("{state_id} #{}.{}", self.session_number, self.ordinal)
        }
    }
}

/// Groups attempts into timeline entries and numbers their sessions.
///
/// - An attempt that resumes a session of the step the workflow has not left
///   since (nothing else came after that session's entry) continues that
///   entry: the agent was asked again, or answered, in the same session.
/// - One that resumes a session of the step after other entries came in
///   between is a new entry numbered like the session it returns to.
/// - Anything else is a new session, with the next number of its step.
fn group_entries(drafts: &[Draft]) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut next_number: HashMap<&str, u32> = HashMap::new();
    for (idx, draft) in drafts.iter().enumerate() {
        let session_of = |entry: &Entry| -> Option<&str> {
            entry
                .drafts
                .iter()
                .rev()
                .find_map(|&d| drafts[d].harness_session_id.as_deref())
        };
        let state_of = |entry: &Entry| drafts[entry.drafts[0]].state_id.as_str();
        let resumed = draft.resumed_harness_session.as_deref();
        let origin = resumed.and_then(|resumed| {
            entries.iter().rposition(|entry| {
                state_of(entry) == draft.state_id
                    && session_of(entry) == Some(resumed)
                    && match (&drafts[entry.drafts[0]].harness, &draft.harness) {
                        (Some(a), Some(b)) => a == b,
                        _ => true,
                    }
            })
        });
        match origin {
            Some(pos) if pos + 1 == entries.len() => entries[pos].drafts.push(idx),
            Some(pos) => {
                let session_number = entries[pos].session_number;
                let ordinal = entries
                    .iter()
                    .filter(|entry| {
                        state_of(entry) == draft.state_id && entry.session_number == session_number
                    })
                    .count() as u32
                    + 1;
                entries.push(Entry {
                    drafts: vec![idx],
                    session_number,
                    ordinal,
                });
            }
            None => {
                let number = next_number.entry(draft.state_id.as_str()).or_insert(0);
                *number += 1;
                entries.push(Entry {
                    drafts: vec![idx],
                    session_number: *number,
                    ordinal: 1,
                });
            }
        }
    }
    entries
}

/// Creates a new draft attempt with its state's current visit number and
/// returns its index.
fn push_attempt(
    drafts: &mut Vec<Draft>,
    state_visit_counts: &HashMap<String, u32>,
    state_id: String,
    action_execution_id: Option<String>,
    action_type: Option<String>,
    decision_by: Option<String>,
    started_at: DateTime<Utc>,
) -> usize {
    let visit = *state_visit_counts.get(&state_id).unwrap_or(&1);
    let idx = drafts.len();
    drafts.push(Draft {
        state_id,
        visit,
        action_execution_id,
        action_type,
        decision_by,
        outcome: None,
        interrupted: false,
        resolved_via_attempt_failed: false,
        visit_closed: false,
        started_at: Some(started_at),
        ended_at: None,
        error: None,
        resumed_harness_session: None,
        harness_session_interrupted: false,
        has_agent_event_missing_id: false,
        harness: None,
        model: None,
        effort: None,
        harness_session_id: None,
        conversation_override: None,
    });
    idx
}

/// Links an agent-session event by its own `action_execution_id` when
/// present, falling back to the last attempt of the current visit
/// otherwise — the old-format-log case (U1), which also flags the position
/// as having no conversation record.
fn link_agent_event(
    drafts: &mut [Draft],
    by_attempt: &HashMap<String, usize>,
    current_visit_last_position: Option<usize>,
    action_execution_id: &Option<String>,
    links: &mut BTreeMap<u64, Link>,
    seq: u64,
    apply: impl FnOnce(&mut Draft),
) {
    let idx = match action_execution_id {
        Some(id) => by_attempt.get(id).copied(),
        None => {
            if let Some(idx) = current_visit_last_position {
                drafts[idx].has_agent_event_missing_id = true;
            }
            current_visit_last_position
        }
    };

    match idx {
        Some(idx) => {
            apply(&mut drafts[idx]);
            links.insert(seq, Link::Position(idx));
        }
        None => {
            links.insert(seq, Link::NoLink(reasons::NO_ATTEMPT_YET.to_string()));
        }
    }
}

/// Links an event positionally, to the last attempt of the current visit.
fn link_positional(
    current_visit_last_position: Option<usize>,
    links: &mut BTreeMap<u64, Link>,
    seq: u64,
) {
    match current_visit_last_position {
        Some(idx) => {
            links.insert(seq, Link::Position(idx));
        }
        None => {
            links.insert(seq, Link::NoLink(reasons::NO_ATTEMPT_YET.to_string()));
        }
    }
}

/// Turns one draft into its public [`Position`], applying the status
/// precedence rule and computing `retry_pending` and `conversation`.
fn finalize(
    draft: Draft,
    execution_status: ExecutionStatus,
    is_last: bool,
    conversation_presence: &HashSet<String>,
) -> Position {
    let status = if draft.interrupted {
        PositionStatus::Interrupted
    } else {
        match draft.outcome {
            Some(Outcome::Failed) => PositionStatus::Failed,
            Some(Outcome::Succeeded) => {
                if draft.visit_closed {
                    PositionStatus::Succeeded
                } else if execution_status == ExecutionStatus::InputNeeded {
                    PositionStatus::AwaitingDecision
                } else {
                    PositionStatus::Succeeded
                }
            }
            None => {
                if execution_status == ExecutionStatus::Running
                    || execution_status == ExecutionStatus::Paused
                    || execution_status == ExecutionStatus::InputNeeded
                {
                    PositionStatus::InProgress
                } else {
                    // The execution ended without ever resolving this
                    // attempt and nothing marked it interrupted explicitly —
                    // treat it the same way, since nothing else fits.
                    PositionStatus::Interrupted
                }
            }
        }
    };

    let retry_pending = is_last
        && !draft.interrupted
        && draft.resolved_via_attempt_failed
        && status == PositionStatus::Failed;

    let conversation = if let Some(state) = draft.conversation_override {
        state
    } else if draft.has_agent_event_missing_id {
        ConversationState::NoRecord
    } else if let Some(action_type) = &draft.action_type {
        if !matches!(action_type.as_str(), "agent" | "profile") {
            ConversationState::NotCreated {
                reason: "this action does not use an agent conversation".to_string(),
            }
        } else {
            conversation_from_presence(&draft.action_execution_id, conversation_presence)
        }
    } else {
        conversation_from_presence(&draft.action_execution_id, conversation_presence)
    };

    Position {
        label: String::new(),
        state_id: draft.state_id,
        visit: draft.visit,
        action_execution_id: draft.action_execution_id,
        action_type: draft.action_type,
        decision_by: draft.decision_by,
        status,
        started_at: draft.started_at,
        ended_at: draft.ended_at,
        error: draft.error,
        resumed_harness_session: draft.resumed_harness_session,
        harness_session_interrupted: draft.harness_session_interrupted,
        retry_pending,
        conversation,
        session_number: None,
        attempts: Vec::new(),
        harness: draft.harness,
        model: draft.model,
        effort: draft.effort,
        harness_session_id: draft.harness_session_id,
        usage: None,
    }
}

fn conversation_from_presence(
    action_execution_id: &Option<String>,
    conversation_presence: &HashSet<String>,
) -> ConversationState {
    match action_execution_id {
        Some(id) if conversation_presence.contains(id) => ConversationState::Recorded,
        Some(_) => ConversationState::NotCreated {
            reason: "no conversation record for this attempt".to_string(),
        },
        None => ConversationState::NoRecord,
    }
}
