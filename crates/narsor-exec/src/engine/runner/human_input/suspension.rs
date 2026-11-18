//! Suspending a pass until a person answers.

use super::*;

impl Engine {
    /// Suspends the execution on a `human` gate — a `human` action or a
    /// `human` decision — and records it durably.
    pub(crate) fn suspend_for_input(
        &self,
        execution: &mut WorkflowExecution,
        state: &State,
        request: HumanInputRequest,
        outputs: Vec<ChoiceDefinition>,
    ) -> Result<(), EngineError> {
        let mut pending =
            PendingHumanInput::new(execution.id.clone(), state.id.clone(), request, outputs);
        if matches!(state.action, Some(ActionConfig::Decision(_))) {
            let kept_locals = step_memory::step_locals(execution, &state.id)?;
            pending = pending.deciding(local_values(state, &kept_locals));
        }
        let sessions: BTreeMap<String, AgentSessionProvenance> =
            read_engine_record(execution, METADATA_AGENT_SESSIONS_KEY)?.unwrap_or_default();
        for output in &pending.outputs {
            if output.resume_comment_field.is_some() {
                if let Some(target) = state.transitions.get(&output.label) {
                    if let Some(session) = sessions.get(target) {
                        pending
                            .resume_targets
                            .insert(output.label.clone(), session.clone());
                    }
                }
            }
        }
        let from = self.store.event_log(&execution.id).next_sequence()?;
        human::suspend(&self.store, execution, &pending)?;
        self.publish_from(&execution.id, from)?;
        // The conductor summarizes the gate beside it (R30): advisory, after
        // the gate is durable, and never an answer — the gate waits for the
        // operator whatever the summary says.
        if let Some(conductor) = self.conductor() {
            conductor.gate_suspended(&execution.id, &pending);
        }
        Ok(())
    }

    /// Pauses an agent state in place. No output label is produced and no
    /// workflow transition is selected; `deliver_human_input` later records
    /// the answer and runs this same state again.
    pub(crate) fn suspend_for_agent_input(
        &self,
        execution: &mut WorkflowExecution,
        state: &State,
        request: HumanInputRequest,
    ) -> Result<(), EngineError> {
        let pending = if request.permission.is_some() {
            PendingHumanInput::for_permission(execution.id.clone(), state.id.clone(), request)
        } else {
            PendingHumanInput::for_agent(execution.id.clone(), state.id.clone(), request)
        };
        let from = self.store.event_log(&execution.id).next_sequence()?;
        human::suspend(&self.store, execution, &pending)?;
        self.publish_from(&execution.id, from)
    }
}
