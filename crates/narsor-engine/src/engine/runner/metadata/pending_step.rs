//! The result of a finished action, kept until its transition is taken.

use super::*;

/// The result of a finished action, kept until its transition has been taken.
///
/// Persisted in `metadata.engine.pending_step` so that a step interrupted
/// between `ActionCompleted` and `TransitionTaken` resumes at the decision
/// instead of re-running the action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingStep {
    /// The state whose action produced the result.
    pub state_id: StateId,

    /// The attempt that produced it (D24 idempotency key).
    pub action_execution_id: String,

    /// What the action returned.
    pub result: ActionResult,

    /// Artifacts the action reported, already recorded as events.
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
}

/// The mid-step state of `execution`, when it belongs to `state_id`.
pub(crate) fn stored_pending_step(
    execution: &WorkflowExecution,
    state_id: &str,
) -> Result<Option<PendingStep>, EngineError> {
    Ok(
        read_engine_record::<PendingStep>(execution, METADATA_PENDING_STEP_KEY)?
            .filter(|pending| pending.state_id == state_id),
    )
}

pub(crate) fn agent_session_provenance(pending: &PendingStep) -> Option<AgentSessionProvenance> {
    let session = pending.result.data.get(SESSION_DATA_KEY)?;
    let harness = session.pointer("/harness")?.as_str()?.to_string();
    let session_id = session
        .pointer("/config/harness_session_id")?
        .as_str()?
        .to_string();
    Some(AgentSessionProvenance {
        action_execution_id: pending.action_execution_id.clone(),
        harness,
        session_id,
    })
}

impl Engine {
    /// The mid-step state of this execution, when it belongs to `state_id`.
    pub(crate) fn pending_step(
        &self,
        execution: &WorkflowExecution,
        state_id: &str,
    ) -> Result<Option<PendingStep>, EngineError> {
        stored_pending_step(execution, state_id)
    }

    /// Persists the mid-step state inside the execution record.
    pub(crate) fn write_pending_step(
        &self,
        execution: &mut WorkflowExecution,
        pending: &PendingStep,
    ) -> Result<(), EngineError> {
        write_engine_record(execution, METADATA_PENDING_STEP_KEY, pending)
    }

    /// Drops the mid-step state.
    pub(crate) fn clear_pending_step(&self, execution: &mut WorkflowExecution) {
        clear_engine_metadata(execution, METADATA_PENDING_STEP_KEY);
    }
}
