//! The answer to an agent's permission request.

use super::*;

impl Engine {
    /// Applies a permission decision through the dedicated path and resumes
    /// the same agent session. Generic human input cannot call this operation.
    pub async fn resolve_permission(
        &self,
        definition: &WorkflowDefinition,
        execution_id: &str,
        decision: crate::engine::actions::PermissionDecision,
        episode: &str,
    ) -> Result<WorkflowExecution, EngineError> {
        self.observed(execution_id, async {
            let _guard = self.begin_run(execution_id)?;
            match self.prepare_resolve_permission(definition, execution_id, decision, episode)? {
                HandleState::Done(execution) => Ok(execution),
                HandleState::Ready(execution) => self.run_loop(definition, execution).await,
            }
        })
        .await
    }

    fn prepare_resolve_permission(
        &self,
        definition: &WorkflowDefinition,
        execution_id: &str,
        decision: crate::engine::actions::PermissionDecision,
        episode: &str,
    ) -> Result<HandleState, EngineError> {
        let from = self.store.event_log(execution_id).next_sequence()?;
        let resumed = human::resolve_permission(&self.store, execution_id, decision, episode)?;
        self.publish_from(execution_id, from)?;
        self.lock_pause_requests().remove(execution_id);
        let mut execution = resumed.execution;
        let state_id = self.current_state_id(&execution)?;
        let _state = self.state_of(definition, &execution, &state_id)?;
        let answer = agent_reply(&resumed.result.variables).ok_or_else(|| {
            EngineError::HumanInput(HumanActionError::MissingRequiredField {
                field: "permission decision".to_string(),
            })
        })?;
        let permission_rules = match resumed.permission_decision.as_ref() {
            Some(decision) => {
                crate::engine::actions::human::granted_rules(&resumed.pending.request, decision)?
            }
            None => Vec::new(),
        };
        self.store_agent_input(
            &mut execution,
            &state_id,
            &answer,
            resumed.pending.request.agent_harness.as_deref(),
            resumed.pending.request.agent_session_id.as_deref(),
            permission_rules,
            PromptAuthor::User,
        )?;
        Ok(HandleState::Ready(execution))
    }

    /// Two-phase claim for the dedicated permission decision operation.
    pub fn try_begin_resolve_permission(
        self: &Arc<Self>,
        definition: &WorkflowDefinition,
        execution_id: &str,
        decision: crate::engine::actions::PermissionDecision,
        episode: &str,
    ) -> Result<RunHandle, EngineError> {
        let claim = self.claim_active_run(execution_id)?;
        match self.prepare_resolve_permission(definition, execution_id, decision, episode) {
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
}
