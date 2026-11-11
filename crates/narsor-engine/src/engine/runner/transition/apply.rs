//! A decision applied: the transition taken and what its label assigns.

use super::*;
use crate::domain::WorkspaceField;

/// Writes what `set` assigned: shared variables into the context, local
/// ones into the memory of `state_id`, pull request values into the
/// context's workspaces.
fn write_assignments(
    execution: &mut WorkflowExecution,
    state_id: &str,
    assigned: &RenderedSet,
) -> Result<(), EngineError> {
    for assignment in &assigned.workspaces {
        let pr = execution
            .context
            .workspace_prs
            .entry(assignment.workspace.clone())
            .or_default();
        let value = Some(assignment.value.clone());
        match assignment.field {
            WorkspaceField::Pr => pr.pr = value,
            WorkspaceField::PrUrl => pr.pr_url = value,
            read_only => {
                return Err(EngineError::InvalidDefinition {
                    violations: vec![format!(
                        "workspace field '{}' is read-only and cannot be written",
                        read_only.key()
                    )],
                })
            }
        }
    }
    let (local, global): (Vec<&Assignment>, Vec<&Assignment>) = assigned
        .variables
        .iter()
        .partition(|assignment| assignment.local);
    for assignment in global {
        execution
            .context
            .variables
            .insert(assignment.name.clone(), assignment.value.clone());
    }
    let local_values = local
        .into_iter()
        .map(|assignment| (assignment.name.clone(), assignment.value.clone()))
        .collect();
    step_memory::write_locals(execution, state_id, local_values)
}

/// Moves the variables of an agent, script or decision `result` that its step does not
/// let it write into its `additional_data` (KTD5): kept, but never written
/// to the context. That is a variable the step does not define, and one
/// only the workflow sets (`source: workflow`). Returns a warning for each
/// one moved.
///
/// The engine's own [`agent::SESSION_ID_VARIABLE`] stays where it is, and so
/// does whatever a gate or a person gave: only a step that can define
/// variables is held to its definitions.
pub(crate) fn set_aside_undeclared_variables(
    state: &State,
    result: &mut ActionResult,
) -> Vec<String> {
    if !holds_variables_to_declarations(state) {
        return Vec::new();
    }
    let is_kept = |name: &String| {
        name == agent::SESSION_ID_VARIABLE
            || state
                .variable(name)
                .is_some_and(|variable| variable.source == VariableSource::Step)
    };
    let (kept, set_aside): (Map<String, Value>, Map<String, Value>) =
        std::mem::take(&mut result.variables)
            .into_iter()
            .partition(|(name, _)| is_kept(name));
    result.variables = kept;
    set_aside
        .into_iter()
        .map(|(name, value)| {
            let warning = if state.variable(&name).is_some() {
                format!(
                    "variable '{name}' is set by the workflow alone on step '{}'; it was kept \
                     as additional data and not written to the context",
                    state.id
                )
            } else {
                format!(
                    "variable '{name}' is not defined on step '{}'; it was kept as additional \
                     data and not written to the context",
                    state.id
                )
            };
            result.additional_data.entry(name).or_insert(value);
            warning
        })
        .collect()
}

/// The variables of a result that belong in the shared context: all but the
/// step's own local memory, which never leaves its step (R12).
fn shared_variables(state: &State, variables: &Map<String, Value>) -> Map<String, Value> {
    variables
        .iter()
        .filter(|(name, _)| !state.is_local_variable(name))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

impl Engine {
    /// `DecisionMade` → context (D10) → `TransitionTaken` → `state.json`.
    pub(crate) fn apply_decision(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
        pending: PendingStep,
        outcome: DecisionOutcome,
    ) -> Result<Flow, EngineError> {
        let target = match decision::check_label(state, &outcome.label) {
            Ok(target) => target.clone(),
            Err(error) => {
                self.fail_action(execution, &pending.action_execution_id, error.to_string())?;
                return Ok(Flow::Stop);
            }
        };
        let continues_target_session = state
            .choices
            .iter()
            .any(|output| output.label == outcome.label && output.resume_comment_field.is_some());
        // Rendered and checked before anything is written: a `set` that
        // cannot be applied whole is not applied at all.
        let assignments =
            match self.render_assignments(definition, execution, state, &pending, &outcome.label) {
                Ok(assignments) => assignments,
                Err(message) => {
                    self.fail_action(execution, &pending.action_execution_id, message)?;
                    return Ok(Flow::Stop);
                }
            };

        // Before the step goes on, and again when a restart completes it
        // again: the same file, written whole.
        if let Err(message) = self.store_step_profile(execution, state, &pending) {
            self.fail_action(execution, &pending.action_execution_id, message)?;
            return Ok(Flow::Stop);
        }

        self.emit(
            &execution.id,
            EventData::DecisionMade {
                provider: outcome.provider_name().to_string(),
                output_label: outcome.label.clone(),
                reasoning: outcome.reasoning.clone(),
            },
        )?;

        // The one place the context is written (D10). `save_execution` writes
        // `state.json` as well, because it embeds the context and is the
        // authority; writing `context.json` alone would leave a stale copy.
        execution
            .context
            .update_variables(shared_variables(state, &pending.result.variables));
        step_memory::keep_locals(execution, state, &pending.result.variables)?;
        step_memory::record_choice(execution, &state.id, &outcome.label, &pending.result.reason)?;
        if matches!(state.action, Some(ActionConfig::Decision(_))) {
            let by_conductor = outcome.provider == DecisionProviderType::Conductor;
            step_memory::count_decision(execution, &state.id, by_conductor)?;
        }
        // After the step's own result, so what `set` assigns wins over it.
        write_assignments(execution, &state.id, &assignments)?;
        if pending
            .result
            .reply
            .as_deref()
            .is_some_and(|reply| !reply.trim().is_empty())
            && workflow_step_reply_sources(definition).contains(&state.id)
        {
            let reply = pending.result.reply.as_deref().expect("checked above");
            self.store
                .write_step_reply(&execution.id, &pending.action_execution_id, reply)?;
            let attempts = engine_metadata_mut(execution)
                .entry(METADATA_STEP_REPLIES_KEY.to_string())
                .or_insert_with(|| json!({}));
            if !attempts.is_object() {
                *attempts = json!({});
            }
            attempts
                .as_object_mut()
                .expect("reply pointers are an object")
                .insert(
                    state.id.clone(),
                    Value::String(pending.action_execution_id.clone()),
                );
        }
        if matches!(state.action.as_ref(), Some(ActionConfig::Script(_)))
            && workflow_step_stream_sources(definition).contains(&state.id)
        {
            let streams = engine_metadata_mut(execution)
                .entry(METADATA_STEP_STREAMS_KEY.to_string())
                .or_insert_with(|| json!({}));
            if !streams.is_object() {
                *streams = json!({});
            }
            streams
                .as_object_mut()
                .expect("streams are an object")
                .insert(
                    state.id.clone(),
                    json!({
                        "stdout": pending.result.stdout,
                        "stderr": pending.result.stderr,
                    }),
                );
        }
        for artifact in &pending.artifacts {
            let key = artifact.name.clone().unwrap_or_else(|| artifact.id.clone());
            execution.context.add_artifact(
                key,
                ArtifactReference {
                    id: artifact.id.clone(),
                    name: artifact.name.clone(),
                    location: artifact.location.clone(),
                    metadata: artifact.metadata.clone(),
                },
            );
        }
        // Before `pending_step` goes: it holds the answer the record is made
        // of. Written from the same `save_execution` as the context, so the
        // record and the transition cannot disagree after a crash.
        let kept_locals = step_memory::step_locals(execution, &state.id)?;
        let record = LastAgentStep::from_result(
            state,
            target.clone(),
            pending.action_execution_id.clone(),
            &pending.result,
            &kept_locals,
        );
        set_last_agent_step(execution, record.as_ref())?;
        self.clear_pending_step(execution);
        // A rescued step is over once its transition is taken (F1), and so
        // is the conductor's retry of a failed one (F3).
        clear_engine_metadata(execution, METADATA_OUTPUT_RESCUE_KEY);
        clear_engine_metadata(execution, METADATA_CONDUCTOR_RETRY_KEY);

        // No save until the target is set: a `state.json` without the pending
        // step but still at this state would run its action again after a
        // crash, over the result and the `set` already applied. A crash
        // before the one save below leaves the pending step, and the step
        // resumes at its decision.
        for assignment in assignments.variables {
            self.emit(
                &execution.id,
                EventData::VariableSetByWorkflow {
                    variable: assignment.name,
                    value: assignment.value,
                    state_id: state.id.clone(),
                    output_label: outcome.label.clone(),
                    local: assignment.local,
                },
            )?;
        }
        self.emit(
            &execution.id,
            EventData::TransitionTaken {
                from_state: state.id.clone(),
                to_state: target.clone(),
                output_label: outcome.label,
            },
        )?;

        step_memory::count_step_entry(execution, &target)?;
        execution.current_state = Some(target);
        execution.current_action_execution = Some(pending.action_execution_id);
        let target_state = definition
            .states
            .get(execution.current_state.as_deref().expect("target was set"))
            .expect("validated transition target exists");
        if runs_harness(target_state) && !continues_target_session {
            if let Some(max_entries) = target_state.max_entries {
                let entries = state_entry_count(execution, &target_state.id)?;
                if entries >= max_entries {
                    // `suspend` saves the execution once, after the pending
                    // input exists: a crash before then cannot start the step
                    // past its limit.
                    self.wait_on_entry_limit(
                        definition,
                        execution,
                        target_state,
                        entries,
                        max_entries,
                    )?;
                    return Ok(Flow::Stop);
                }
                count_entry(execution, &target_state.id)?;
            }
        }
        self.store.save_execution(execution)?;

        self.write_checkpoint(definition, execution);

        Ok(Flow::Continue)
    }

    /// What the `set` of `label` assigns on leaving `state`, rendered in the
    /// scope of that step: the context as the step's own result leaves it,
    /// its local memory, and the label each step last chose (this one's
    /// included). Reads only; nothing is written.
    pub(crate) fn render_assignments(
        &self,
        definition: &WorkflowDefinition,
        execution: &WorkflowExecution,
        state: &State,
        pending: &PendingStep,
        label: &str,
    ) -> Result<RenderedSet, String> {
        let sets_nothing = state
            .choices
            .iter()
            .all(|choice| choice.label != label || choice.set.is_empty());
        if sets_nothing {
            return Ok(RenderedSet::default());
        }
        let unreadable = |error: EngineError| error.to_string();
        let mut context = execution.context.clone();
        context.update_variables(shared_variables(state, &pending.result.variables));
        let mut locals = step_memory::step_locals(execution, &state.id).map_err(unreadable)?;
        locals.extend(
            pending
                .result
                .variables
                .iter()
                .filter(|(name, _)| state.is_local_variable(name))
                .map(|(name, value)| (name.clone(), value.clone())),
        );
        let mut labels = step_memory::last_choice_labels(execution).map_err(unreadable)?;
        labels.insert(state.id.clone(), Value::String(label.to_string()));
        let declared = DeclaredNames::of(definition);
        let task_title = self.task_labels.effective_title();
        let scope = TemplateScope::from_execution(&execution.inputs, &context)
            .map_err(|error| error.to_string())?;
        let scope = self
            .reading_workspaces(scope)
            .with_task_fields(self.task_labels.ticket.as_deref(), task_title.as_deref())
            .with_declared(&declared)
            .with_step_labels(&labels)
            .with_step_memory(StepMemory {
                locals: &locals,
                definitions: &state.variables,
                history: "",
            });
        variable_rules::render_assignments(state, label, &scope)
    }
}
