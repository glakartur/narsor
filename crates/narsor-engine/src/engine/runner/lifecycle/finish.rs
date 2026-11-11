//! How an execution ends in a terminal state.

use super::*;

/// How an execution ends when it enters the terminal state `state`.
///
/// The answer comes from the workflow's own declaration (`docs/dev/workflow-schema.md`
/// §3.5) and from nowhere else; a terminal that declares nothing completes the
/// execution.
pub fn terminal_status(state: &State) -> ExecutionStatus {
    state
        .terminal_status
        .map(ExecutionStatus::from)
        .unwrap_or(ExecutionStatus::Completed)
}

impl Engine {
    /// Ends the execution in a terminal state (`docs/dev/domain-model.md` §4.4).
    pub(crate) fn finish(
        &self,
        execution: &mut WorkflowExecution,
        state: &State,
    ) -> Result<(), EngineError> {
        let status = terminal_status(state);

        let ending = match status {
            ExecutionStatus::Cancelled => EventData::WorkflowCancelled {
                reason: format!("workflow ended in terminal state '{}'", state.id),
            },
            ExecutionStatus::Failed => EventData::WorkflowFailed {
                error: format!("workflow ended in failure terminal state '{}'", state.id),
            },
            _ => EventData::WorkflowCompleted {
                result: json!({
                    "terminal_state": state.id,
                    "variables": execution.context.variables,
                }),
            },
        };
        self.emit(&execution.id, ending)?;

        execution.status = status;
        execution.completed_at = Some(Utc::now());
        self.clear_pending_step(execution);
        clear_engine_metadata(execution, METADATA_CONDUCTOR_RETRY_KEY);
        self.store.save_execution(execution)?;
        Ok(())
    }
}
