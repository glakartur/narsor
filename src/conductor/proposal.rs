//! Carrying out what the conductor proposed, once the operator confirms it
//! (R28, U12).
//!
//! A proposal is a chat entry ([`ChatEntryKind::Proposal`]) with a
//! [`ProposalRecord`]: what to do, and the key of the moment it is about
//! (KTD9). Nothing happens until the operator confirms it; then it goes
//! through the same public [`crate::engine::Engine`] operations the API
//! routes use, under the execution's ordinary claim (KTD10):
//!
//! * **answer the input** — a delivery to the pending input, made by the
//!   conductor (`answered_by: conductor`);
//! * **tell the agent** — the answer to the agent's pending question, or,
//!   when it asks none, the operator note its next attempt reads;
//! * **retry** — a recovery of the failed step with the proposal's note;
//! * **transition** — the manual transition of the proposed label, after
//!   which the execution runs on;
//! * **abort** — the execution is cancelled.
//!
//! Each carried-out proposal is recorded as `ConductorDecision { kind:
//! execute_proposal }` with the rationale and confidence of the answer that
//! proposed it, and as a decision entry of the chat, which is also what
//! makes a confirmation idempotent: a second confirmation of the same
//! proposal — another window, a repeated command — returns the first one's
//! result and does nothing. A proposal about a moment the execution has
//! moved on from is refused ([`ProposalError::Stale`]) and changes nothing.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::conductor::chat::ProposalRecord;
use crate::conductor::response::{ProposalKind, ProposedAction};
use crate::conductor::store::{ChatAuthor, ChatEntry, ChatEntryKind};
use crate::conductor::ConductorRegistry;
use crate::domain::{
    AnsweredBy, ConductorDecisionKind, EventData, ExecutionStatus, WorkflowDefinition,
};
use crate::engine::{
    execution_episode, step_failure, Engine, EngineError, HumanInputDelivery, InputContinuation,
    ManualTransition, PendingHumanInput, Recovery, RunHandle,
};

/// The `kind` of a chat decision entry that records a carried-out proposal.
pub(super) const EXECUTED: &str = "execute_proposal";

/// The `kind` of a chat entry that records a rejected proposal.
pub(super) const REJECTED: &str = "proposal_rejected";

/// The provider `DecisionMade` records for a transition the conductor took
/// on a confirmed proposal.
const CONDUCTOR_PROVIDER: &str = "conductor";

/// What became of a proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    /// It was carried out.
    Executed,
    /// The operator rejected it.
    Rejected,
}

/// The answer to a confirmation or a rejection of a proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalOutcome {
    /// The proposal: the id of its chat entry.
    pub proposal_id: String,
    /// What became of it.
    pub status: ProposalStatus,
    /// The `ConductorDecision` that carried it out, when it was.
    #[serde(default)]
    pub decision_id: Option<String>,
    /// What was done, for the operator.
    pub message: String,
    /// Whether this answers a repeated request: the proposal had already
    /// been settled, and nothing was done now.
    #[serde(default)]
    pub repeated: bool,
}

/// A confirmed proposal, and the engine pass it started, if it started one.
///
/// The pass is the caller's to drive (`RunHandle::run_to_completion`),
/// typically in the background the API keeps its passes in, so that it can
/// be interrupted like any other.
pub struct ConfirmedProposal {
    /// What became of the proposal.
    pub outcome: ProposalOutcome,
    /// The pass to drive, when carrying the proposal out needs one.
    pub pass: Option<RunHandle>,
}

/// Why a proposal was not carried out; nothing changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposalError {
    /// There is no such proposal in the execution's chat.
    NotFound(String),
    /// The execution moved on from the moment the proposal is about (KTD9).
    Stale(String),
    /// The proposal was already settled the other way.
    Settled(String),
    /// Another pass drives the execution right now.
    Busy(String),
    /// The proposal cannot be carried out as it stands.
    Invalid(String),
    /// Something broke on the way.
    Failed(String),
}

impl std::fmt::Display for ProposalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(message)
            | Self::Stale(message)
            | Self::Settled(message)
            | Self::Busy(message)
            | Self::Invalid(message)
            | Self::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ProposalError {}

impl From<crate::persistence::PersistenceError> for ProposalError {
    fn from(error: crate::persistence::PersistenceError) -> Self {
        Self::Failed(error.to_string())
    }
}

/// How an earlier request settled a proposal.
enum Settlement {
    Executed(ProposalOutcome),
    Rejected(ProposalOutcome),
}

impl ConductorRegistry {
    /// Carries out the proposal `proposal_id` of `execution_id`'s chat, as
    /// the operator confirmed it (see the module documentation).
    ///
    /// `definition` is the one the execution runs.
    pub fn confirm_proposal(
        &self,
        definition: &WorkflowDefinition,
        execution_id: &str,
        proposal_id: &str,
    ) -> Result<ConfirmedProposal, ProposalError> {
        let _settling = self.lock_settling();
        let (entry, record) = self.find_proposal(execution_id, proposal_id)?;
        match self.settlement(execution_id, proposal_id)? {
            Some(Settlement::Executed(outcome)) => {
                return Ok(ConfirmedProposal {
                    outcome,
                    pass: None,
                })
            }
            Some(Settlement::Rejected(_)) => {
                return Err(ProposalError::Settled(format!(
                    "The proposal '{}' was rejected; it can no longer be carried out.",
                    entry.text
                )))
            }
            None => {}
        }

        let Some(episode) = record.episode.clone() else {
            return Err(ProposalError::Invalid(
                "The proposal does not say which moment of the execution it is about, so it \
                 cannot be checked against where the execution is now."
                    .to_string(),
            ));
        };
        let (message, pass) = self
            .carry_out(definition, execution_id, &record, &episode)
            .map_err(|error| refusal(&entry, error))?;

        let decision_id = Uuid::new_v4().to_string();
        let confidence = record.confidence.unwrap_or_default();
        let rationale = record
            .rationale
            .clone()
            .or_else(|| record.proposal.summary.clone());
        let outcome = ProposalOutcome {
            proposal_id: proposal_id.to_string(),
            status: ProposalStatus::Executed,
            decision_id: Some(decision_id.clone()),
            message: message.clone(),
            repeated: false,
        };

        if let Err(error) = self.engine.record_event(
            execution_id,
            EventData::ConductorDecision {
                decision_id: decision_id.clone(),
                kind: ConductorDecisionKind::ExecuteProposal,
                state_id: record.state_id.clone(),
                output_label: record.proposal.label.clone(),
                episode: Some(episode.clone()),
                confidence,
                rationale: rationale.clone(),
            },
        ) {
            tracing::warn!(execution_id, %error, "conductor: could not record a decision");
        }
        let mut decision = ChatEntry::new(ChatAuthor::Conductor, ChatEntryKind::Decision, message)
            .replying_to(proposal_id);
        decision.payload = Some(json!({
            "decision_id": decision_id,
            "kind": EXECUTED,
            "proposal_id": proposal_id,
            "proposal": record.proposal,
            "episode": episode,
            "state_id": record.state_id,
            "confidence": confidence,
            "rationale": rationale,
        }));
        self.actor(execution_id)?.append_chat(decision)?;

        Ok(ConfirmedProposal { outcome, pass })
    }

    /// Rejects the proposal `proposal_id`: it is marked so in the chat and
    /// can no longer be confirmed. Rejecting it again answers the same.
    pub fn reject_proposal(
        &self,
        execution_id: &str,
        proposal_id: &str,
    ) -> Result<ProposalOutcome, ProposalError> {
        let _settling = self.lock_settling();
        let (entry, _) = self.find_proposal(execution_id, proposal_id)?;
        match self.settlement(execution_id, proposal_id)? {
            Some(Settlement::Rejected(outcome)) => return Ok(outcome),
            Some(Settlement::Executed(_)) => {
                return Err(ProposalError::Settled(format!(
                    "The proposal '{}' was already carried out; it cannot be rejected.",
                    entry.text
                )))
            }
            None => {}
        }

        let message = format!("Rejected the proposal: {}", entry.text);
        let mut rejection = ChatEntry::new(
            ChatAuthor::Operator,
            ChatEntryKind::Decision,
            message.clone(),
        )
        .replying_to(proposal_id);
        rejection.payload = Some(json!({
            "kind": REJECTED,
            "proposal_id": proposal_id,
        }));
        self.actor(execution_id)?.append_chat(rejection)?;
        Ok(ProposalOutcome {
            proposal_id: proposal_id.to_string(),
            status: ProposalStatus::Rejected,
            decision_id: None,
            message,
            repeated: false,
        })
    }

    /// The proposal entry `proposal_id` and its record.
    fn find_proposal(
        &self,
        execution_id: &str,
        proposal_id: &str,
    ) -> Result<(ChatEntry, ProposalRecord), ProposalError> {
        let not_found = || {
            ProposalError::NotFound(format!(
                "Execution '{execution_id}' has no proposal '{proposal_id}'."
            ))
        };
        let entry = self
            .store(execution_id)
            .read_chat()?
            .into_iter()
            .find(|entry| entry.id == proposal_id && entry.kind == ChatEntryKind::Proposal)
            .ok_or_else(not_found)?;
        let record = entry
            .payload
            .clone()
            .and_then(|payload| serde_json::from_value::<ProposalRecord>(payload).ok())
            .ok_or_else(|| {
                ProposalError::Invalid(format!(
                    "The proposal '{proposal_id}' is unreadable and cannot be carried out."
                ))
            })?;
        Ok((entry, record))
    }

    /// How an earlier request settled `proposal_id`, if one did.
    fn settlement(
        &self,
        execution_id: &str,
        proposal_id: &str,
    ) -> Result<Option<Settlement>, ProposalError> {
        let chat = self.store(execution_id).read_chat()?;
        Ok(chat.into_iter().find_map(|entry| {
            let payload = entry.payload.as_ref()?;
            if entry.kind != ChatEntryKind::Decision
                || payload.get("proposal_id").and_then(Value::as_str) != Some(proposal_id)
            {
                return None;
            }
            match payload.get("kind").and_then(Value::as_str) {
                Some(EXECUTED) => Some(Settlement::Executed(ProposalOutcome {
                    proposal_id: proposal_id.to_string(),
                    status: ProposalStatus::Executed,
                    decision_id: payload
                        .get("decision_id")
                        .and_then(Value::as_str)
                        .map(ToString::to_string),
                    message: entry.text.clone(),
                    repeated: true,
                })),
                Some(REJECTED) => Some(Settlement::Rejected(ProposalOutcome {
                    proposal_id: proposal_id.to_string(),
                    status: ProposalStatus::Rejected,
                    decision_id: None,
                    message: entry.text.clone(),
                    repeated: true,
                })),
                _ => None,
            }
        }))
    }

    /// Carries `record` out through the engine, at `episode`. Returns what
    /// was done, and the pass to drive when it needs one.
    fn carry_out(
        &self,
        definition: &WorkflowDefinition,
        execution_id: &str,
        record: &ProposalRecord,
        episode: &str,
    ) -> Result<(String, Option<RunHandle>), EngineError> {
        let engine: &Arc<Engine> = &self.engine;
        let proposal = &record.proposal;
        let execution = engine.store().load_execution(execution_id)?;
        // Checked up front, so a late confirmation leaves no trace; each
        // engine operation checks it again under the claim, on the read it
        // writes from (KTD9).
        let current = execution_episode(engine.store(), &execution);
        if current.as_deref() != Some(episode) {
            return Err(moved_on(execution_id, episode, current));
        }
        let pending = if execution.status == ExecutionStatus::InputNeeded {
            engine
                .store()
                .load_pending_input::<PendingHumanInput>(execution_id)?
        } else {
            None
        };

        match proposal.kind {
            ProposalKind::AnswerInput => {
                let pending = pending.ok_or_else(|| moved_on(execution_id, episode, None))?;
                let delivery = answer_delivery(&pending, proposal, episode, record)?;
                let pass =
                    engine.try_begin_deliver_human_input(definition, execution_id, &delivery)?;
                Ok(("Answered the pending input.".to_string(), Some(pass)))
            }
            ProposalKind::MessageAgent => {
                let text = proposal.text.clone().unwrap_or_default();
                match pending {
                    Some(pending) if pending.continuation == InputContinuation::ResumeAgent => {
                        let delivery = HumanInputDelivery::with_values(Map::from_iter([(
                            "answer".to_string(),
                            Value::String(text.clone()),
                        )]))
                        .for_episode(episode)
                        .answered_by(AnsweredBy::Conductor);
                        let pass = engine.try_begin_deliver_human_input(
                            definition,
                            execution_id,
                            &delivery,
                        )?;
                        Ok((format!("Answered the agent's question: {text}"), Some(pass)))
                    }
                    _ => {
                        engine.leave_operator_note(execution_id, &text, Some(episode))?;
                        Ok((
                            format!("Left a note for the agent's next attempt: {text}"),
                            None,
                        ))
                    }
                }
            }
            ProposalKind::Retry => {
                // Bound to the failure's key only when the execution failed
                // on a recorded step failure: an `Unknown` execution, or a
                // failed one without a record, has no such key to compare
                // against, and the check above already refused a late
                // confirmation.
                let failure_key = step_failure(&execution)
                    .ok()
                    .flatten()
                    .map(|failure| failure.key().to_string());
                let pass = engine.try_begin_recover_with(
                    execution_id,
                    Recovery {
                        comment: proposal.text.clone(),
                        episode: failure_key,
                        conductor_retry: false,
                    },
                )?;
                Ok(("Restarted the failed step.".to_string(), Some(pass)))
            }
            ProposalKind::Transition => {
                let label = proposal.label.clone().unwrap_or_default();
                let moved = engine.take_manual_transition(
                    definition,
                    execution_id,
                    &ManualTransition {
                        label: Some(label.clone()),
                        target: None,
                        force: false,
                        provider: CONDUCTOR_PROVIDER.to_string(),
                        reason: record
                            .rationale
                            .clone()
                            .or_else(|| proposal.summary.clone())
                            .unwrap_or_else(|| {
                                "A proposal of the conductor's, confirmed by the operator."
                                    .to_string()
                            }),
                        by: AnsweredBy::Conductor,
                        episode: Some(episode.to_string()),
                    },
                )?;
                let target = moved.current_state.clone().unwrap_or_default();
                if moved.status != ExecutionStatus::Paused {
                    return Ok((
                        format!("Took the transition of output '{label}' into '{target}'."),
                        None,
                    ));
                }
                // Confirmed to move the execution on: it runs on from there.
                let pass = engine.try_begin_resume(execution_id)?;
                Ok((
                    format!("Took the transition of output '{label}' into '{target}' and resumed."),
                    Some(pass),
                ))
            }
            ProposalKind::Abort => {
                engine.cancel_execution(
                    execution_id,
                    "Cancelled on a proposal of the conductor's, confirmed by the operator.",
                    AnsweredBy::Conductor,
                    Some(episode),
                )?;
                Ok(("Cancelled the execution.".to_string(), None))
            }
        }
    }

    fn lock_settling(&self) -> std::sync::MutexGuard<'_, ()> {
        self.settling
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The delivery that answers `pending` as `proposal` says, made by the
/// conductor at `episode`.
fn answer_delivery(
    pending: &PendingHumanInput,
    proposal: &ProposedAction,
    episode: &str,
    record: &ProposalRecord,
) -> Result<HumanInputDelivery, EngineError> {
    let values = proposal.values.clone();
    let mut delivery = match pending.continuation {
        InputContinuation::ResolveOutput => {
            // A problem's answer is the step's output: `values` holds its
            // `data` and `variables`, or is the `data` itself.
            let object = |key: &str| match values.get(key) {
                Some(Value::Object(map)) => Some(map.clone()),
                _ => None,
            };
            let (data, variables) = match (object("data"), object("variables")) {
                (None, None) => (values.clone(), Map::new()),
                (data, variables) => (data.unwrap_or_default(), variables.unwrap_or_default()),
            };
            HumanInputDelivery::default()
                .with_data(data)
                .with_variables(variables)
        }
        InputContinuation::ResumeAgent if values.is_empty() => {
            let text = proposal.text.clone().unwrap_or_default();
            HumanInputDelivery::with_values(Map::from_iter([(
                "answer".to_string(),
                Value::String(text),
            )]))
        }
        InputContinuation::CompleteStep | InputContinuation::ResumeAgent => {
            HumanInputDelivery::with_values(values)
        }
        // Approval is an operator-only commitment; a proposal may describe it
        // but never selects accept/resume on the operator's behalf.
        InputContinuation::ApproveOutput => {
            return Err(EngineError::HumanInput(
                crate::engine::actions::human::HumanActionError::UnexpectedStepValues,
            ))
        }
    };
    if pending.continuation != InputContinuation::ResumeAgent {
        if let Some(label) = &proposal.label {
            delivery = delivery.choosing(label.clone());
        }
    }
    if let Some(rationale) = record.rationale.as_deref().or(proposal.summary.as_deref()) {
        delivery = delivery.with_comment(rationale);
    }
    Ok(delivery
        .for_episode(episode)
        .answered_by(AnsweredBy::Conductor))
}

/// The error of a proposal about `episode` of an execution that no longer
/// stands there.
fn moved_on(execution_id: &str, episode: &str, current: Option<String>) -> EngineError {
    EngineError::EpisodeChanged {
        execution_id: execution_id.to_string(),
        expected: episode.to_string(),
        current,
    }
}

/// Why the proposal `entry` was not carried out, in the operator's terms.
fn refusal(entry: &ChatEntry, error: EngineError) -> ProposalError {
    let what = &entry.text;
    match error {
        EngineError::StaleEpisode { .. }
        | EngineError::EpisodeChanged { .. }
        | EngineError::NotRunnable { .. }
        | EngineError::AwaitingHumanInput { .. } => ProposalError::Stale(format!(
            "The proposal '{what}' is about a moment the execution has moved on from, so it was \
             not carried out ({error})."
        )),
        EngineError::HumanInput(ref human) if human.is_stale() => ProposalError::Stale(format!(
            "The proposal '{what}' answers an input that is no longer pending, so it was not \
             carried out ({error})."
        )),
        EngineError::HumanInput(ref human) if human.is_invalid_delivery() => {
            ProposalError::Invalid(format!(
                "The proposal '{what}' cannot be carried out as it stands: {error}"
            ))
        }
        EngineError::InvalidTransition { .. } => ProposalError::Invalid(format!(
            "The proposal '{what}' cannot be carried out as it stands: {error}"
        )),
        EngineError::AlreadyRunning { .. } => ProposalError::Busy(format!(
            "The execution is being driven right now; the proposal '{what}' was not carried out. \
             Try again once it stops."
        )),
        other => ProposalError::Failed(format!(
            "The proposal '{what}' could not be carried out: {other}"
        )),
    }
}
