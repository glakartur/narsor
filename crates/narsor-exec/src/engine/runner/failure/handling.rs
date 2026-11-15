//! Failing an action, a step or an execution, and noting engine errors.

use super::*;

impl Engine {
    /// Records a failed action and the failed execution that follows from it.
    ///
    /// For a failure met after the action ran — a label, a decision — so it
    /// counts as one that may have had side effects.
    pub(crate) fn fail_action(
        &self,
        execution: &mut WorkflowExecution,
        action_execution_id: &str,
        message: impl Into<String>,
    ) -> Result<(), EngineError> {
        let failure = FailedAction {
            message: message.into(),
            error_type: None,
        };
        self.fail_attempts(execution, action_execution_id, failure, false)
    }

    /// Records the failed action whose last attempt was `action_execution_id`
    /// and the failed execution that follows; `side_effect_free` says whether
    /// none of its attempts can have acted (KTD14).
    pub(crate) fn fail_attempts(
        &self,
        execution: &mut WorkflowExecution,
        action_execution_id: &str,
        failure: impl Into<FailedAction>,
        side_effect_free: bool,
    ) -> Result<(), EngineError> {
        let failure: FailedAction = failure.into();
        if let Some(state_id) = execution.current_state.clone() {
            step_memory::record_failure(execution, &state_id, failure.error_type.as_deref())?;
        }
        self.emit(
            &execution.id,
            EventData::ActionFailed {
                action_execution_id: action_execution_id.to_string(),
                error: failure.message.clone(),
                error_type: failure.error_type,
            },
        )?;
        self.fail_step(
            execution,
            Some(action_execution_id),
            failure.message,
            side_effect_free,
        )
    }

    /// Ends the execution as `Failed`, with no transition taken.
    fn fail_execution(
        &self,
        execution: &mut WorkflowExecution,
        error: String,
    ) -> Result<(), EngineError> {
        self.fail_step(execution, None, error, false)
    }

    /// Ends the execution as `Failed` on its step's attempt `failed_attempt`,
    /// and hands the failure to the conductor (F3) — in the same save that
    /// makes it `Failed`, the record the conductor reads, then the trigger
    /// once it is durable.
    fn fail_step(
        &self,
        execution: &mut WorkflowExecution,
        failed_attempt: Option<&str>,
        error: String,
        side_effect_free: bool,
    ) -> Result<(), EngineError> {
        self.emit(
            &execution.id,
            EventData::WorkflowFailed {
                error: error.clone(),
            },
        )?;
        execution.status = ExecutionStatus::Failed;
        execution.completed_at = Some(Utc::now());
        let conductor =
            self.record_step_failure(execution, failed_attempt, &error, side_effect_free)?;
        self.clear_pending_step(execution);
        clear_engine_metadata(execution, METADATA_OUTPUT_REJECTION_KEY);
        clear_engine_metadata(execution, METADATA_OUTPUT_RESCUE_KEY);
        self.store.save_execution(execution)?;
        if let Some(conductor) = conductor {
            conductor.step_failed(&execution.id);
        }
        Ok(())
    }

    /// Records an engine-level problem and fails the execution on it.
    pub(crate) fn record_engine_error(
        &self,
        execution: &mut WorkflowExecution,
        message: &str,
    ) -> Result<(), EngineError> {
        self.emit(
            &execution.id,
            EventData::Error {
                message: message.to_string(),
                context: json!({ "state": execution.current_state }),
            },
        )?;
        self.fail_execution(execution, message.to_string())
    }

    /// Runs `future`, one of the three public entry points, and makes sure any
    /// `Err` it returns is also noted as an event (R5) before it reaches the
    /// caller — which is what makes the failure observable to a caller that,
    /// like the TUI's background spawns, discards the `Result` itself.
    pub(crate) async fn observed<F>(
        &self,
        execution_id: &str,
        future: F,
    ) -> Result<WorkflowExecution, EngineError>
    where
        F: std::future::Future<Output = Result<WorkflowExecution, EngineError>>,
    {
        let outcome = future.await;
        if let Err(error) = &outcome {
            self.note_engine_error(execution_id, error);
        }
        outcome
    }

    /// Emits an [`EventData::Error`] against `execution_id`, best-effort.
    ///
    /// Used to make an [`EngineError`] that would otherwise only reach the
    /// caller's `Result` observable through the event stream as well (R5), for
    /// callers — background TUI spawns — that discard the `Result`. Skips
    /// emitting for the one case where `execution_id` never resolved to
    /// anything on disk (there is no log to identify), rather than inventing
    /// one; otherwise, a failure to emit never masks or replaces the original
    /// error — it is only ever logged, not returned.
    pub(crate) fn note_engine_error(&self, execution_id: &str, error: &EngineError) {
        if matches!(
            error,
            EngineError::Persistence(PersistenceError::ExecutionNotFound { .. })
        ) {
            return;
        }
        if let Err(emit_error) = self.emit(
            execution_id,
            EventData::Error {
                message: error.to_string(),
                context: json!({}),
            },
        ) {
            tracing::warn!(
                execution_id,
                %error,
                %emit_error,
                "could not record an engine-level error as an event"
            );
        }
    }

    /// The error for an operation the execution's status does not allow.
    pub(crate) fn not_runnable(
        &self,
        execution: &WorkflowExecution,
        operation: &'static str,
    ) -> EngineError {
        if execution.status == ExecutionStatus::InputNeeded
            && self.store.has_pending_input(&execution.id)
        {
            return EngineError::AwaitingHumanInput {
                execution_id: execution.id.clone(),
            };
        }
        EngineError::NotRunnable {
            execution_id: execution.id.clone(),
            status: execution.status.as_str().to_string(),
            operation,
        }
    }
}
