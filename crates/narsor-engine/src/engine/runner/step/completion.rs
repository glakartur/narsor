//! A finished action: its result recorded, its step completed.

use super::*;

/// Builds the record of a file artifact at `path`, named when it is one of
/// the step's artifacts. `None` for an empty path.
fn artifact_at(name: Option<&str>, path: &str, action_execution_id: &str) -> Option<Artifact> {
    if path.is_empty() {
        return None;
    }
    let mut artifact = Artifact::new(
        "file".to_string(),
        path.to_string(),
        action_execution_id.to_string(),
    );
    artifact.name = name.map(ToString::to_string);
    Some(artifact)
}

impl Engine {
    /// Records a finished action: label check, `ActionCompleted`, artifacts,
    /// and the mid-step state that makes the rest of the step resumable.
    pub(crate) fn record_completion(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
        action_execution_id: String,
        result: ActionResult,
    ) -> Result<Flow, EngineError> {
        match self.record_result(execution, state, action_execution_id, result)? {
            Some(pending) => self.complete_step(definition, execution, state, pending),
            None => Ok(Flow::Stop),
        }
    }

    /// The first half of [`Engine::record_completion`], up to the decision:
    /// returns the persisted mid-step state, or `None` when the label failed
    /// the action.
    pub(crate) fn record_result(
        &self,
        execution: &mut WorkflowExecution,
        state: &State,
        action_execution_id: String,
        mut result: ActionResult,
    ) -> Result<Option<PendingStep>, EngineError> {
        // A label the state does not declare is the engine's call, never the
        // executor's — this is the message the operator sees.
        if let Err(error) = decision::check_label(state, &result.output_label) {
            self.fail_action(execution, &action_execution_id, error.to_string())?;
            return Ok(None);
        }

        let warnings = set_aside_undeclared_variables(state, &mut result);
        // A script cannot be asked again, so a value outside the declared
        // `options` ends its step; an agent was asked again before this.
        let reprompts = state
            .action
            .as_ref()
            .is_some_and(|action| ActionKind::of(action).reprompts_rejected_output());
        if holds_variables_to_declarations(state) && !reprompts {
            if let Some(problem) = variable_rules::value_outside_options(state, &result.variables) {
                self.fail_action(execution, &action_execution_id, problem)?;
                return Ok(None);
            }
        }
        self.emit(
            &execution.id,
            EventData::ActionCompleted {
                action_execution_id: action_execution_id.clone(),
                output_label: result.output_label.clone(),
                reason: non_blank(Some(result.reason.as_str())),
                additional_data: result.additional_data.clone(),
                warnings,
            },
        )?;

        let artifacts = self.register_artifacts(execution, &action_execution_id, &result)?;

        let pending = PendingStep {
            state_id: state.id.clone(),
            action_execution_id,
            result,
            artifacts,
        };
        if runs_harness(state) {
            if let Some(provenance) = agent_session_provenance(&pending) {
                let mut sessions: BTreeMap<String, AgentSessionProvenance> =
                    read_engine_record(execution, METADATA_AGENT_SESSIONS_KEY)?.unwrap_or_default();
                sessions.insert(state.id.clone(), provenance);
                write_engine_record(execution, METADATA_AGENT_SESSIONS_KEY, &sessions)?;
            }
        }
        self.write_pending_step(execution, &pending)?;
        self.store.save_execution(execution)?;

        Ok(Some(pending))
    }

    /// The second half of a step: decide, write the context, transition.
    pub(crate) fn complete_step(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
        pending: PendingStep,
    ) -> Result<Flow, EngineError> {
        if runs_harness(state)
            && state.choices.iter().any(|output| {
                output.label == pending.result.output_label && output.requires_human_approval
            })
        {
            let provenance = agent_session_provenance(&pending);
            let agent_harness = provenance.as_ref().map(|session| session.harness.clone());
            let agent_session_id = provenance
                .as_ref()
                .map(|session| session.session_id.clone());
            let request = HumanInputRequest {
                action_execution_id: pending.action_execution_id.clone(),
                instruction:
                    "Review the agent output: accept it, or return it to the agent with a comment."
                        .to_string(),
                fields: Vec::new(),
                reason: None,
                required_values: Vec::new(),
                answer_tail: Some(pending.result.stdout.clone()),
                agent_harness: agent_harness.clone(),
                agent_session_id: agent_session_id.clone(),
                prefilled_answer: None,
                permission: None,
            };
            let approval = ApprovalCandidate {
                action_execution_id: pending.action_execution_id,
                result: pending.result,
                artifacts: pending.artifacts,
                agent_harness,
                agent_session_id,
            };
            let request = PendingHumanInput::for_approval(
                execution.id.clone(),
                state.id.clone(),
                request,
                approval,
            );
            let from = self.store.event_log(&execution.id).next_sequence()?;
            human::suspend(&self.store, execution, &request)?;
            self.publish_from(&execution.id, from)?;
            return Ok(Flow::Stop);
        }
        let request = match decision::decide(state, &pending.result) {
            Ok(request) => request,
            Err(error) => {
                // A provider this version cannot use is a workflow-author
                // error, not an action failure: say so and stop.
                self.record_engine_error(execution, &error.to_string())?;
                return Err(EngineError::Decision(error));
            }
        };

        let outcome = match request {
            DecisionRequest::Resolved(outcome) => outcome,
            DecisionRequest::AwaitHuman { message } => {
                let kept_locals = step_memory::step_locals(execution, &state.id)?;
                let context = GateContext::of(state, &pending.result, &kept_locals);
                let request = HumanInputRequest {
                    action_execution_id: pending.action_execution_id.clone(),
                    instruction: message,
                    fields: Vec::new(),
                    reason: context.reason,
                    required_values: context.required_values,
                    answer_tail: context.answer_tail,
                    agent_harness: None,
                    agent_session_id: None,
                    prefilled_answer: None,
                    permission: None,
                };
                self.suspend_for_input(
                    execution,
                    state,
                    request,
                    decision::permitted_outputs(state),
                )?;
                return Ok(Flow::Stop);
            }
        };

        self.apply_decision(definition, execution, state, pending, outcome)
    }

    /// Writes the profile a `profile` step's result carries to
    /// `step-profile.json`, replacing the one before; an empty profile
    /// replaces it too. A result without one — the operator's, for a step
    /// whose output was never usable — leaves the file alone.
    pub(crate) fn store_step_profile(
        &self,
        execution: &mut WorkflowExecution,
        state: &State,
        pending: &PendingStep,
    ) -> Result<(), String> {
        // Not the data key alone: a human step's form answer is its result's
        // `data`, and an author may name a field `step_profile`.
        if !matches!(state.action, Some(ActionConfig::Profile(_))) {
            return Ok(());
        }
        let Some(entries) = profile_action::step_profile_of(&pending.result) else {
            return Ok(());
        };
        let entries = entries.map_err(|error| {
            format!(
                "the step profile of state '{}' is damaged: {error}",
                state.id
            )
        })?;
        self.store
            .write_step_profile(&execution.id, &entries)
            .map_err(|error| {
                format!(
                    "the step profile of state '{}' could not be written: {error}",
                    state.id
                )
            })?;
        // A new file: one that cannot be read later is noted again. Saved
        // with the execution after the decision.
        clear_engine_metadata(execution, METADATA_UNREADABLE_STEP_PROFILE_KEY);
        Ok(())
    }

    /// Turns the artifacts a result reports into artifact records: its named
    /// `artifacts` — an action's, or those a person or the conductor
    /// delivered — and an action's `additional_artifacts`.
    ///
    /// Each one becomes an `ArtifactCreated` event here and an entry in
    /// `context.artifacts` when the context is written. An entry that names
    /// no location is skipped rather than failing the action: a malformed
    /// artifact note is not a reason to discard work that succeeded.
    fn register_artifacts(
        &self,
        execution: &WorkflowExecution,
        action_execution_id: &str,
        result: &ActionResult,
    ) -> Result<Vec<Artifact>, EngineError> {
        let named = result
            .artifacts
            .iter()
            .map(|(name, path)| (Some(name.as_str()), path));
        let additional = result.additional_artifacts.iter().map(|path| (None, path));

        let reported = named
            .chain(additional)
            .filter_map(|(name, path)| artifact_at(name, path, action_execution_id));

        let mut artifacts = Vec::new();
        for artifact in reported {
            self.emit(
                &execution.id,
                EventData::ArtifactCreated {
                    artifact_id: artifact.id.clone(),
                    artifact_type: artifact.artifact_type.clone(),
                    location: artifact.location.clone(),
                },
            )?;
            artifacts.push(artifact);
        }
        Ok(artifacts)
    }
}
