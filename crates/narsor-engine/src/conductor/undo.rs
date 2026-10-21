//! Undoing a decision of the conductor's (R31, KTD13, U12).
//!
//! Only a chosen output (a) can be undone: it is the one decision that
//! settles a step in the operator's place, and the one saved with the
//! execution's context from before it (see [`super::rescue`]). A reprompt
//! (b) or an escalation (c) leaves nothing to take back, and a carried-out
//! proposal was the operator's own confirmation.
//!
//! Undoing is a dedicated operation, not a transition — it needs no
//! `allow_manual_override`: a pass still driving the execution is
//! interrupted, the claim it held is waited for, and the engine restores
//! the context and raises the step's problem again at the same state
//! ([`crate::engine::Engine::undo_output_decision`]). The agent step is not
//! run again. What a step after the decision already did is not undone; the
//! answer warns about it.

use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::conductor::store::{ChatAuthor, ChatEntry, ChatEntryKind, SavedDecision};
use crate::conductor::ConductorRegistry;
use crate::domain::{ConductorDecisionKind, ExecutionStatus, WorkflowDefinition};
use crate::engine::{ConductorAnalysis, EngineError, OutputDecisionUndo};

pub use narsor_protocol::api::UndoOutcome;

/// The `kind` of the chat entry that records an undone decision.
pub(super) const UNDONE: &str = "decision_undone";

/// How long an undo waits for an interrupted pass to let go of the claim.
const CLAIM_WAIT: Duration = Duration::from_secs(10);

/// How often it looks again meanwhile.
const CLAIM_POLL: Duration = Duration::from_millis(25);

/// Why a decision was not undone; nothing changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoError {
    /// The execution's chat holds no such decision.
    NotFound(String),
    /// The decision is not a chosen output of the conductor's.
    NotUndoable(String),
    /// It was undone already.
    AlreadyUndone(String),
    /// A later choice of the conductor's, or an undo, came after it: only
    /// the latest choice can be undone.
    Superseded(String),
    /// The execution was cancelled: an undo does not bring it back.
    Cancelled(String),
    /// The execution could not be taken from the pass driving it.
    Busy(String),
    /// Something broke on the way.
    Failed(String),
}

impl std::fmt::Display for UndoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(message)
            | Self::NotUndoable(message)
            | Self::AlreadyUndone(message)
            | Self::Superseded(message)
            | Self::Cancelled(message)
            | Self::Busy(message)
            | Self::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for UndoError {}

impl From<crate::persistence::PersistenceError> for UndoError {
    fn from(error: crate::persistence::PersistenceError) -> Self {
        Self::Failed(error.to_string())
    }
}

impl ConductorRegistry {
    /// Undoes the chosen output `decision_id` of `execution_id` (see the
    /// module documentation).
    ///
    /// `interrupt` stops the pass driving the execution, if one does — the
    /// daemon's registry of background passes is the caller's; the undo
    /// then waits for the pass to let go of the claim.
    pub async fn undo_decision(
        &self,
        definition: &WorkflowDefinition,
        execution_id: &str,
        decision_id: &str,
        interrupt: impl FnOnce(),
    ) -> Result<UndoOutcome, UndoError> {
        let _undoing = self.undoing.lock().await;
        let decision = self.undoable_decision(execution_id, decision_id)?;
        if self.engine.store().load_execution(execution_id)?.status == ExecutionStatus::Cancelled {
            return Err(cancelled(execution_id));
        }
        let state_id = decision.state_id.clone().ok_or_else(|| {
            UndoError::NotUndoable(format!(
                "The decision '{decision_id}' does not say which state it settled."
            ))
        })?;

        if self.engine.is_active(execution_id) {
            interrupt();
            let deadline = Instant::now() + CLAIM_WAIT;
            while self.engine.is_active(execution_id) {
                if Instant::now() >= deadline {
                    return Err(UndoError::Busy(format!(
                        "Execution '{execution_id}' is still being driven; the decision was not \
                         undone. Interrupt it and try again."
                    )));
                }
                tokio::time::sleep(CLAIM_POLL).await;
            }
        }

        let undo = OutputDecisionUndo {
            decision_id: decision.decision_id.clone(),
            state_id: state_id.clone(),
            context_before: decision.context_before.clone(),
            answers: decision.answers.clone(),
            reason: decision.reason.clone(),
            reply: decision.reply.clone(),
            state_entries_before: decision.state_entries_before.clone(),
            step_memory_before: decision.step_memory_before.clone(),
            analysis: ConductorAnalysis {
                label: decision.output_label.clone(),
                data: object_of(&decision.data),
                variables: object_of(&decision.variables),
                rationale: Some(match &decision.rationale {
                    Some(rationale) => {
                        format!("The conductor chose this, and the operator undid it: {rationale}")
                    }
                    None => "The conductor chose this, and the operator undid it.".to_string(),
                }),
            },
        };
        let undone = self
            .engine
            .undo_output_decision(definition, execution_id, &undo)
            .map_err(|error| match error {
                EngineError::NotRunnable { status, .. } if status == "cancelled" => {
                    cancelled(execution_id)
                }
                EngineError::AlreadyRunning { .. } => UndoError::Busy(format!(
                    "Execution '{execution_id}' is being driven again; the decision was not \
                     undone. Try again once it stops."
                )),
                other => UndoError::Failed(format!(
                    "The decision '{decision_id}' could not be undone: {other}"
                )),
            })?;

        let actor = self.actor(execution_id)?;
        actor.update_state(|state| {
            state
                .unapplied_decisions
                .retain(|saved| saved.decision_id != decision.decision_id);
        })?;
        let mut message = format!(
            "Undid the conductor's choice of the output '{}' at '{state_id}': the context from \
             before it is back, and the step is a problem again.",
            decision.output_label.as_deref().unwrap_or_default()
        );
        if let Some(warning) = &undone.warning {
            message.push(' ');
            message.push_str(warning);
        }
        let mut entry = ChatEntry::new(ChatAuthor::Operator, ChatEntryKind::Decision, &message);
        entry.payload = Some(json!({
            "kind": UNDONE,
            "decision_id": decision.decision_id,
            "state_id": state_id,
            "episode": undone.episode,
            "warning": undone.warning,
        }));
        actor.append_chat(entry)?;

        Ok(UndoOutcome {
            decision_id: decision.decision_id,
            state_id,
            episode: undone.episode,
            warning: undone.warning,
            message,
        })
    }

    /// The saved decision `decision_id`, if it is one an undo takes back:
    /// a chosen output of the conductor's, not undone yet, and the latest
    /// one of the execution.
    ///
    /// Only the latest: undoing an older choice would throw away everything
    /// since it — later choices included — and leave those later choices
    /// undoable, so contexts could be swapped back and forth. A decision
    /// followed by another choice, or by an undo, is superseded.
    fn undoable_decision(
        &self,
        execution_id: &str,
        decision_id: &str,
    ) -> Result<SavedDecision, UndoError> {
        let chat = self.store(execution_id).read_chat()?;
        undoable_in(&chat, execution_id, decision_id)
    }
}

/// The saved decision `decision_id` of `chat`, if it is one an undo takes
/// back (see [`ConductorRegistry::undo_decision`]).
pub(super) fn undoable_in(
    chat: &[ChatEntry],
    execution_id: &str,
    decision_id: &str,
) -> Result<SavedDecision, UndoError> {
    let names = |entry: &ChatEntry| {
        entry.kind == ChatEntryKind::Decision
            && payload_str(entry, "decision_id") == Some(decision_id)
    };
    if chat.iter().any(|entry| names(entry) && is_undo(entry)) {
        return Err(UndoError::AlreadyUndone(format!(
            "The decision '{decision_id}' was already undone."
        )));
    }
    let position = chat
        .iter()
        .position(|entry| names(entry) && entry.author == ChatAuthor::Conductor)
        .ok_or_else(|| {
            UndoError::NotFound(format!(
                "Execution '{execution_id}' has no decision '{decision_id}' of the \
                 conductor's."
            ))
        })?;
    let entry = &chat[position];
    let not_undoable = || {
        UndoError::NotUndoable(format!(
            "The decision '{decision_id}' cannot be undone: only the conductor's choice of \
             an output completes a step in the operator's place and can be taken back."
        ))
    };
    let decision = entry
        .payload
        .clone()
        .and_then(|payload| serde_json::from_value::<SavedDecision>(payload).ok())
        .ok_or_else(not_undoable)?;
    if decision.kind != ConductorDecisionKind::ChooseOutput || !decision.context_before.is_object()
    {
        return Err(not_undoable());
    }
    let superseded = chat[position + 1..]
        .iter()
        .filter(|later| later.kind == ChatEntryKind::Decision)
        .any(|later| is_undo(later) || is_chosen_output(later));
    if superseded {
        return Err(UndoError::Superseded(format!(
            "The decision '{decision_id}' cannot be undone any more: a later choice of the \
             conductor's, or an undo, came after it. Only the latest choice can be taken \
             back."
        )));
    }
    Ok(decision)
}

/// The string `key` of `entry`'s payload, if it has one.
fn payload_str<'a>(entry: &'a ChatEntry, key: &str) -> Option<&'a str> {
    entry.payload.as_ref()?.get(key)?.as_str()
}

/// Whether `entry` records an undone decision.
fn is_undo(entry: &ChatEntry) -> bool {
    payload_str(entry, "kind") == Some(UNDONE)
}

/// Whether `entry` records a chosen output of the conductor's.
fn is_chosen_output(entry: &ChatEntry) -> bool {
    entry.author == ChatAuthor::Conductor
        && payload_str(entry, "kind") == Some(ConductorDecisionKind::ChooseOutput.name())
}

/// The refusal to undo a decision of the cancelled `execution_id`.
fn cancelled(execution_id: &str) -> UndoError {
    UndoError::Cancelled(format!(
        "Execution '{execution_id}' was cancelled; undoing a decision does not bring it back."
    ))
}

/// `value` as an object; anything else is no values at all.
fn object_of(value: &Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map.clone(),
        _ => Map::new(),
    }
}
