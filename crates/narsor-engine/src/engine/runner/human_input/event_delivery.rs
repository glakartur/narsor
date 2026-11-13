//! What ends a `wait` step's wait: a matching external event or the wait's
//! deadline, delivered by the runner (`engine::actions::wait`).

use super::*;

impl Engine {
    /// The external event an execution waits for, if it waits for one.
    pub fn pending_event(
        &self,
        execution_id: &str,
    ) -> Result<Option<PendingExternalEvent>, EngineError> {
        Ok(wait::pending_event(&self.store, execution_id)?)
    }

    /// Ends the wait of an execution waiting for an event and runs it on.
    pub async fn deliver_external_event(
        &self,
        definition: &WorkflowDefinition,
        execution_id: &str,
        delivery: &ExternalEventDelivery,
    ) -> Result<WorkflowExecution, EngineError> {
        self.observed(execution_id, async {
            let _guard = self.begin_run(execution_id)?;
            match self.prepare_deliver_external_event(definition, execution_id, delivery)? {
                HandleState::Done(execution) => Ok(execution),
                HandleState::Ready(execution) => self.run_loop(definition, execution).await,
            }
        })
        .await
    }

    /// Phase 1 of the two-phase [`Engine::deliver_external_event`], like
    /// [`Engine::try_begin_deliver_human_input`]: claims the execution
    /// synchronously ([`EngineError::AlreadyRunning`] at once if another
    /// pass holds it), ends the wait, and hands back the [`RunHandle`]
    /// that drives the workflow on.
    pub fn try_begin_deliver_external_event(
        self: &Arc<Self>,
        definition: &WorkflowDefinition,
        execution_id: &str,
        delivery: &ExternalEventDelivery,
    ) -> Result<RunHandle, EngineError> {
        let claim = self.claim_active_run(execution_id)?;
        match self.prepare_deliver_external_event(definition, execution_id, delivery) {
            Ok(state) => Ok(RunHandle {
                engine: Arc::clone(self),
                execution_id: execution_id.to_string(),
                claim,
                state,
            }),
            Err(error) => {
                self.note_engine_error(execution_id, &error);
                Err(error)
            }
        }
    }

    /// The synchronous half: ends the wait ([`wait::resume`], which refuses
    /// a delivery for another episode), then completes the step with the
    /// event as its result — or, for the deadline, with the `timeout`
    /// label when the step declares it and a failed step otherwise (PO4).
    fn prepare_deliver_external_event(
        &self,
        definition: &WorkflowDefinition,
        execution_id: &str,
        delivery: &ExternalEventDelivery,
    ) -> Result<HandleState, EngineError> {
        let from = self.store.event_log(execution_id).next_sequence()?;
        let (mut execution, pending) = wait::resume(&self.store, execution_id, delivery)?;
        self.publish_from(execution_id, from)?;
        self.lock_pause_requests().remove(execution_id);

        let state = self.state_of(definition, &execution, &pending.state_id)?;
        let flow = match wait::wait_result(
            &pending,
            &delivery.ending,
            &decision::permitted_labels(state),
        ) {
            Ok(result) => self.record_completion(
                definition,
                &mut execution,
                state,
                pending.action_execution_id().to_string(),
                result,
            )?,
            Err(error) => {
                // Nothing ran, so nothing can have acted: a restart of the
                // step waits again.
                self.fail_attempts(
                    &mut execution,
                    pending.action_execution_id(),
                    FailedAction {
                        message: error.message,
                        error_type: error.error_type,
                    },
                    true,
                )?;
                Flow::Stop
            }
        };
        Ok(match flow {
            Flow::Stop => HandleState::Done(execution),
            Flow::Continue => HandleState::Ready(execution),
        })
    }
}
