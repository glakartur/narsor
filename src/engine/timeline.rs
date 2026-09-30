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
//! One [`Position`] is one action attempt: an `ActionStarted` or
//! `ActionRetrying` event and everything that happens to that attempt after
//! it, keyed by `action_execution_id` (D24's idempotency key, already a
//! fresh id per attempt). A state visited twice, or retried once, produces
//! two or three positions, never one position mutated in place — R7 asks for
//! every attempt and every re-entry to show up on its own.
//!
//! # Why the label is the identity
//!
//! A position's label is `"<state> #n"`, where `n` counts attempts of that
//! state across the *whole* execution, not per visit — so a loop's second
//! visit continues the count rather than restarting it. Because `n` only
//! grows, a label assigned to a position never changes as later events are
//! appended, which is exactly what lets a caller hold onto a label as a
//! selection across a refresh (R6). There is deliberately no separate
//! position id: the label already is one.
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
//! lifecycle events, human input, external events, and `StateEntered` itself
//! are never part of an agent conversation, and the link map says why.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::{Event, EventData, ExecutionStatus};

/// One action attempt on the timeline.
///
/// `Serialize`/`Deserialize` are derived here, not bolted on in `src/api`,
/// because this is the API's actual response shape (U5): the contract is
/// "the same fold this module already computes", not a second copy of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    /// `"<state> #n"`, stable identity of this position (see module docs).
    pub label: String,
    /// The state this attempt belongs to.
    pub state_id: String,
    /// Which visit of `state_id` this attempt belongs to, 1-based. Several
    /// positions can share a visit (a retried attempt); a loop's second visit
    /// starts a new one.
    pub visit: u32,
    /// The attempt's idempotency key (D24), absent only for the synthesized
    /// "template error before the executor ran" position.
    pub action_execution_id: Option<String>,
    /// The action type (`"agent"`, `"script"`, `"human"`, ...) from
    /// `ActionStarted`, when known.
    pub action_type: Option<String>,
    /// The status, per the precedence rule in the module docs.
    pub status: PositionStatus,
    /// When the attempt started (`ActionStarted` / `ActionRetrying`).
    pub started_at: Option<DateTime<Utc>>,
    /// When the attempt was resolved one way or another. `None` while it is
    /// still open.
    pub ended_at: Option<DateTime<Utc>>,
    /// The error, when the status carries one (`Failed`, `Interrupted`, or a
    /// template error before the executor ran).
    pub error: Option<String>,
    /// The harness session this attempt resumed, if any (`AgentSessionStarted.resumed_session`, D18).
    pub resumed_harness_session: Option<String>,
    /// Set when the harness session itself failed (e.g. a timeout) even
    /// though the attempt's status is still `Succeeded` because the executor
    /// salvaged a label from partial output.
    pub harness_session_interrupted: bool,
    /// Set when this is a `Failed` attempt (via `ActionAttemptFailed`) that
    /// has retries left and no `ActionRetrying` has started the next one yet
    /// — the backoff pause is still in progress.
    pub retry_pending: bool,
    /// Whether — and why not — this attempt's conversation can be read.
    pub conversation: ConversationState,
}

/// The status of one [`Position`], per the precedence rule in the module
/// docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionStatus {
    /// `ActionCompleted` fired for this attempt.
    Succeeded,
    /// `ActionFailed` or `ActionAttemptFailed` fired for this attempt.
    Failed,
    /// Cut short: a recovery error named it, a manual transition or a new
    /// visit started while it was still open.
    Interrupted,
    /// Still open, and the execution is `Running`.
    InProgress,
    /// Its action completed, but the state hasn't transitioned out yet and
    /// the execution is `InputNeeded` — waiting on a human decision.
    AwaitingDecision,
}

/// Whether a position's conversation can be read, and why not when it
/// cannot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConversationState {
    /// The attempt has a conversation record.
    Recorded,
    /// No conversation exists for this attempt, with the reason: a
    /// non-agent action, or an agent attempt whose conversation never
    /// started.
    NotCreated {
        /// Why there is nothing to read.
        reason: String,
    },
    /// The attempt predates `action_execution_id` on agent session events
    /// (U1) — an old-format log. The position itself is still correctly
    /// attributed; only its conversation cannot be located.
    NoRecord,
}

/// Where one event belongs on the timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "link", content = "value", rename_all = "snake_case")]
pub enum EventLink {
    /// The event belongs to this position, named by its label.
    Position(String),
    /// The event does not belong to any attempt's conversation.
    NoLink {
        /// Why not.
        reason: String,
    },
}

/// The result of [`build_timeline`]: every position, in order, plus the
/// event → position map and the position to select by default (R6).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Timeline {
    /// Every position, in the order their attempts started.
    pub positions: Vec<Position>,
    /// Event `sequence` → where it belongs.
    pub links: BTreeMap<u64, EventLink>,
    /// The label to select by default: the position currently in progress,
    /// or — when none is — the last position. `None` only when there are no
    /// positions at all.
    pub default_position: Option<String>,
}

impl Timeline {
    /// The position with this label, if any.
    pub fn position(&self, label: &str) -> Option<&Position> {
        self.positions
            .iter()
            .find(|position| position.label == label)
    }

    /// Where the event with this `sequence` belongs, if it was in the log
    /// this timeline was built from.
    pub fn link(&self, sequence: u64) -> Option<&EventLink> {
        self.links.get(&sequence)
    }
}

/// Reasons used for [`EventLink::NoLink`], centralized so the wording stays
/// consistent.
mod reasons {
    pub const WORKFLOW_LIFECYCLE: &str = "does not belong to an agent conversation";
    pub const STATE_ENTERED: &str = "entering a state is not itself part of an agent conversation";
    pub const HUMAN_OR_MANUAL: &str = "does not belong to an agent conversation";
    pub const NO_ATTEMPT_YET: &str = "no attempt has started in this visit yet";
}

/// One attempt under construction while folding the log.
struct Draft {
    label: String,
    state_id: String,
    visit: u32,
    action_execution_id: Option<String>,
    action_type: Option<String>,
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
    let mut state_attempt_counts: HashMap<String, u32> = HashMap::new();
    let mut state_visit_counts: HashMap<String, u32> = HashMap::new();
    let mut current_visit_state: Option<String> = None;
    let mut current_visit_last_position: Option<usize> = None;
    let mut open_position: Option<usize> = None;
    let mut links: BTreeMap<u64, EventLink> = BTreeMap::new();

    for event in events {
        let seq = event.sequence;

        match &event.data {
            EventData::WorkflowStarted { .. }
            | EventData::WorkflowPaused { .. }
            | EventData::WorkflowResumed
            | EventData::WorkflowCompleted { .. }
            | EventData::WorkflowFailed { .. }
            | EventData::WorkflowCancelled { .. } => {
                links.insert(
                    seq,
                    EventLink::NoLink {
                        reason: reasons::WORKFLOW_LIFECYCLE.to_string(),
                    },
                );
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
                links.insert(
                    seq,
                    EventLink::NoLink {
                        reason: reasons::STATE_ENTERED.to_string(),
                    },
                );
            }

            EventData::ActionStarted {
                action_execution_id,
                state_id,
                action_type,
            } => {
                let idx = push_attempt(
                    &mut drafts,
                    &mut state_attempt_counts,
                    &state_visit_counts,
                    state_id.clone(),
                    Some(action_execution_id.clone()),
                    Some(action_type.clone()),
                    event.timestamp,
                );
                by_attempt.insert(action_execution_id.clone(), idx);
                current_visit_last_position = Some(idx);
                open_position = Some(idx);
                links.insert(seq, EventLink::Position(drafts[idx].label.clone()));
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
                    links.insert(
                        seq,
                        EventLink::NoLink {
                            reason: reasons::NO_ATTEMPT_YET.to_string(),
                        },
                    );
                    continue;
                };
                let inherited_action_type =
                    current_visit_last_position.and_then(|idx| drafts[idx].action_type.clone());
                let idx = push_attempt(
                    &mut drafts,
                    &mut state_attempt_counts,
                    &state_visit_counts,
                    state_id,
                    Some(action_execution_id.clone()),
                    inherited_action_type,
                    event.timestamp,
                );
                by_attempt.insert(action_execution_id.clone(), idx);
                current_visit_last_position = Some(idx);
                open_position = Some(idx);
                links.insert(seq, EventLink::Position(drafts[idx].label.clone()));
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
                let idx = push_attempt(
                    &mut drafts,
                    &mut state_attempt_counts,
                    &state_visit_counts,
                    state_id.clone(),
                    Some(action_execution_id.clone()),
                    inherited_action_type,
                    event.timestamp,
                );
                by_attempt.insert(action_execution_id.clone(), idx);
                current_visit_last_position = Some(idx);
                open_position = Some(idx);
                links.insert(seq, EventLink::Position(drafts[idx].label.clone()));
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
                    links.insert(seq, EventLink::Position(drafts[idx].label.clone()));
                } else {
                    links.insert(
                        seq,
                        EventLink::NoLink {
                            reason: reasons::NO_ATTEMPT_YET.to_string(),
                        },
                    );
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
                    links.insert(seq, EventLink::Position(drafts[idx].label.clone()));
                } else {
                    links.insert(
                        seq,
                        EventLink::NoLink {
                            reason: reasons::NO_ATTEMPT_YET.to_string(),
                        },
                    );
                }
            }

            EventData::ActionFailed {
                action_execution_id,
                error,
            } => {
                if let Some(&idx) = by_attempt.get(action_execution_id) {
                    drafts[idx].outcome = Some(Outcome::Failed);
                    drafts[idx].ended_at = Some(event.timestamp);
                    drafts[idx].error = Some(error.clone());
                    if open_position == Some(idx) {
                        open_position = None;
                    }
                    links.insert(seq, EventLink::Position(drafts[idx].label.clone()));
                } else {
                    links.insert(
                        seq,
                        EventLink::NoLink {
                            reason: reasons::NO_ATTEMPT_YET.to_string(),
                        },
                    );
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
                    links.insert(seq, EventLink::Position(drafts[idx].label.clone()));
                } else {
                    links.insert(
                        seq,
                        EventLink::NoLink {
                            reason: reasons::NO_ATTEMPT_YET.to_string(),
                        },
                    );
                }
            }

            EventData::AgentSessionStarted {
                action_execution_id,
                resumed_session,
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
                    },
                );
            }

            EventData::AgentSessionCompleted {
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

            EventData::AgentSessionFailed {
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
                    |draft| draft.harness_session_interrupted = true,
                );
            }

            EventData::AgentMessage {
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
                links.insert(
                    seq,
                    EventLink::NoLink {
                        reason: reasons::WORKFLOW_LIFECYCLE.to_string(),
                    },
                );
            }

            // The conductor's work is not an attempt of its own (D55): it is
            // pinned to the visit it happened in, like a decision.
            EventData::DecisionMade { .. }
            | EventData::ArtifactCreated { .. }
            | EventData::ProblemRaised { .. }
            | EventData::ConductorStarted { .. }
            | EventData::ConductorFinished { .. }
            | EventData::ConductorDecision { .. } => {
                link_positional(current_visit_last_position, &drafts, &mut links, seq);
            }

            EventData::TransitionTaken { .. } => {
                if let Some(idx) = current_visit_last_position {
                    drafts[idx].visit_closed = true;
                    links.insert(seq, EventLink::Position(drafts[idx].label.clone()));
                } else {
                    links.insert(
                        seq,
                        EventLink::NoLink {
                            reason: reasons::NO_ATTEMPT_YET.to_string(),
                        },
                    );
                }
            }

            EventData::StateExited { .. } => {
                if let Some(idx) = current_visit_last_position {
                    drafts[idx].visit_closed = true;
                    links.insert(seq, EventLink::Position(drafts[idx].label.clone()));
                } else {
                    links.insert(
                        seq,
                        EventLink::NoLink {
                            reason: reasons::NO_ATTEMPT_YET.to_string(),
                        },
                    );
                }
            }

            EventData::HumanInputRequested { .. } | EventData::HumanInputReceived { .. } => {
                links.insert(
                    seq,
                    EventLink::NoLink {
                        reason: reasons::HUMAN_OR_MANUAL.to_string(),
                    },
                );
            }

            EventData::HumanTransitionSelected { .. } => {
                if let Some(idx) = open_position.take() {
                    drafts[idx].interrupted = true;
                }
                links.insert(
                    seq,
                    EventLink::NoLink {
                        reason: reasons::HUMAN_OR_MANUAL.to_string(),
                    },
                );
            }

            EventData::ExternalEventReceived { .. } => {
                links.insert(
                    seq,
                    EventLink::NoLink {
                        reason: reasons::WORKFLOW_LIFECYCLE.to_string(),
                    },
                );
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
                        links.insert(seq, EventLink::Position(drafts[idx].label.clone()));
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
                        &mut state_attempt_counts,
                        &state_visit_counts,
                        state_id,
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
                    links.insert(seq, EventLink::Position(drafts[idx].label.clone()));
                } else {
                    link_positional(current_visit_last_position, &drafts, &mut links, seq);
                }
            }
        }
    }

    let last_index = drafts.len().checked_sub(1);
    let positions: Vec<Position> = drafts
        .into_iter()
        .enumerate()
        .map(|(idx, draft)| {
            finalize(
                draft,
                execution_status.clone(),
                Some(idx) == last_index,
                conversation_presence,
            )
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

/// Creates a new draft attempt, assigning it the next `#n` for `state_id`
/// (global across the execution) and its current visit number, and returns
/// its index.
fn push_attempt(
    drafts: &mut Vec<Draft>,
    state_attempt_counts: &mut HashMap<String, u32>,
    state_visit_counts: &HashMap<String, u32>,
    state_id: String,
    action_execution_id: Option<String>,
    action_type: Option<String>,
    started_at: DateTime<Utc>,
) -> usize {
    let n = *state_attempt_counts
        .entry(state_id.clone())
        .and_modify(|count| *count += 1)
        .or_insert(1);
    let visit = *state_visit_counts.get(&state_id).unwrap_or(&1);
    let label = format!("{state_id} #{n}");
    let idx = drafts.len();
    drafts.push(Draft {
        label,
        state_id,
        visit,
        action_execution_id,
        action_type,
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
    links: &mut BTreeMap<u64, EventLink>,
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
            links.insert(seq, EventLink::Position(drafts[idx].label.clone()));
        }
        None => {
            links.insert(
                seq,
                EventLink::NoLink {
                    reason: reasons::NO_ATTEMPT_YET.to_string(),
                },
            );
        }
    }
}

/// Links an event positionally, to the last attempt of the current visit.
fn link_positional(
    current_visit_last_position: Option<usize>,
    drafts: &[Draft],
    links: &mut BTreeMap<u64, EventLink>,
    seq: u64,
) {
    match current_visit_last_position {
        Some(idx) => {
            links.insert(seq, EventLink::Position(drafts[idx].label.clone()));
        }
        None => {
            links.insert(
                seq,
                EventLink::NoLink {
                    reason: reasons::NO_ATTEMPT_YET.to_string(),
                },
            );
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
        if action_type != "agent" {
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
        label: draft.label,
        state_id: draft.state_id,
        visit: draft.visit,
        action_execution_id: draft.action_execution_id,
        action_type: draft.action_type,
        status,
        started_at: draft.started_at,
        ended_at: draft.ended_at,
        error: draft.error,
        resumed_harness_session: draft.resumed_harness_session,
        harness_session_interrupted: draft.harness_session_interrupted,
        retry_pending,
        conversation,
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
