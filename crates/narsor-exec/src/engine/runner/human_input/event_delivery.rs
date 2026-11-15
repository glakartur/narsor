//! What ends a `wait` step's wait: a matching external event, delivered by
//! the runner (`engine::actions::wait`).

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
    /// event as its result.
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
        let result = wait::wait_result(&delivery.ending, &decision::permitted_labels(state));
        let flow = self.record_completion(
            definition,
            &mut execution,
            state,
            pending.action_execution_id().to_string(),
            result,
        )?;
        Ok(match flow {
            Flow::Stop => HandleState::Done(execution),
            Flow::Continue => HandleState::Ready(execution),
        })
    }
}
