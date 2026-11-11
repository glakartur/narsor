//! One attempt at an action.

use super::*;

impl Engine {
    /// One attempt: expand the templates, hand the action to its executor,
    /// translate what comes back.
    ///
    /// `declared` are the workflow's declared names, which render empty
    /// while unset (KTD6). `pinned_harness` is the action's default harness,
    /// shared by all its attempts: [`Engine::pin_harness`] fills it in at the
    /// first one.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn attempt_action(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
        action: &ActionConfig,
        declared: &DeclaredNames,
        descriptions: &BTreeMap<String, String>,
        action_execution_id: &str,
        timeout: Option<Duration>,
        pinned_harness: &mut Option<String>,
        profile: AttemptProfile,
    ) -> Result<AttemptOutcome, AttemptFailure> {
        let location = format!("state '{}'", state.id);

        // Lazily, on entering the state, so that `{{ context.variables.x }}`
        // picks up the value current at this moment (D31).
        let mut step_replies = HashMap::new();
        for source in action_step_reply_sources(action) {
            if let Some(attempt) = step_reply_attempt(execution, &source) {
                match self.store.read_step_reply(&execution.id, attempt) {
                    Ok(reply) => {
                        step_replies.insert(source, Value::String(reply));
                    }
                    // A pointer without its file is a reply that is not
                    // there: rendering names the step as an unknown variable.
                    Err(PersistenceError::Io { source: error, .. })
                        if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(AttemptFailure::retriable(format!(
                            "could not read the reply of step '{source}' (attempt {attempt}): \
                             {error}"
                        ))
                        .side_effect_free_if(true));
                    }
                }
            }
        }
        let mut step_streams = HashMap::new();
        for (source, field) in action_step_stream_sources(action) {
            if let Some(value) = execution
                .metadata
                .get(METADATA_ENGINE_KEY)
                .and_then(|engine| engine.get(METADATA_STEP_STREAMS_KEY))
                .and_then(|streams| streams.get(&source))
                .and_then(|streams| streams.get(&field))
                .and_then(Value::as_str)
            {
                step_streams.insert(
                    format!("{source}.{field}"),
                    Value::String(value.to_string()),
                );
            }
        }
        let unreadable_memory = |error: EngineError| AttemptFailure::permanent(error.to_string());
        let kept_locals =
            step_memory::step_locals(execution, &state.id).map_err(unreadable_memory)?;
        let history =
            harness_attempt::step_history(execution, state, action).map_err(unreadable_memory)?;
        let step_labels = step_memory::last_choice_labels(execution).map_err(unreadable_memory)?;
        let scope = TemplateScope::from_execution(&execution.inputs, &execution.context)
            .map_err(|error| AttemptFailure::permanent(error.to_string()))?;
        let task_title = self.task_labels.effective_title();
        let scope = self.reading_workspaces(scope);
        let scope = scope
            .with_task_fields(self.task_labels.ticket.as_deref(), task_title.as_deref())
            .with_declared(declared)
            .with_step_replies(&step_replies)
            .with_step_streams(&step_streams)
            .with_step_labels(&step_labels)
            .with_step_memory(StepMemory {
                locals: &kept_locals,
                definitions: &state.variables,
                history: &history,
            });
        let mut rendered = scope
            .render_action(action, &location)
            .map_err(|error| AttemptFailure::permanent(error.to_string()))?;
        // A decision for the operator is asked as a form: the step's
        // variables are its fields. One for the conductor is no action to
        // run: the caller asks the conductor.
        if let ActionConfig::Decision(decision) = &rendered {
            match decision.by {
                DecisionMaker::Human => {
                    rendered = ActionConfig::Human(HumanActionConfig {
                        instruction: decision.message.to_string(),
                        inputs: decision_fields(state),
                        input_schema: None,
                    });
                }
                DecisionMaker::Conductor => {
                    return Ok(AttemptOutcome::AwaitingConductor(
                        decision.message.to_string(),
                    ));
                }
            }
        }

        // A comment is consumed only by the first agent attempt following an
        // explicit recovery. It remains durable until this point, so a daemon
        // restart before the attempt begins cannot silently discard it.
        let mut resumed_input = None;
        let mut author = harness_attempt::prompt_author_of(action);
        if let Some(template) = action.harness_prompt() {
            resumed_input = self.prepare_harness_attempt(
                execution,
                state,
                template,
                &mut rendered,
                &history,
                pinned_harness,
                profile,
            )?;
            if let Some(input) = &resumed_input {
                author = input.author;
            }
        }

        let kind = ActionKind::of(action);
        let outcome = self
            .execute_rendered(
                definition,
                execution,
                state,
                &rendered,
                descriptions,
                action_execution_id,
                PromptPurpose::Step,
                author,
                timeout,
            )
            .await?;

        match outcome {
            Ok(ActionOutcome::Completed(result)) => {
                // A label the state does not declare is a format error of an
                // agent, answered with a reprompt (KTD7); for any other action
                // it stays the permanent failure `record_completion` reports.
                if kind.reprompts_rejected_output() {
                    if let Some(rejection) = OutputRejection::from_result(
                        state,
                        action_execution_id,
                        &result,
                        &kept_locals,
                    ) {
                        return Ok(AttemptOutcome::OutputRejected(rejection));
                    }
                }
                Ok(AttemptOutcome::Completed(result))
            }

            Ok(ActionOutcome::TimedOut(timed_out)) => {
                match timed_out.resolve(&decision::permitted_labels(state)) {
                    Ok(result) => Ok(AttemptOutcome::Completed(result)),
                    Err(error) => Err(AttemptFailure::of_action(error)),
                }
            }

            Ok(ActionOutcome::Suspended(SuspendedAction::AwaitingHumanInput(request))) => {
                Ok(AttemptOutcome::AwaitingHumanInput(request))
            }

            Ok(ActionOutcome::Suspended(SuspendedAction::AwaitingAgentInput(request))) => {
                Ok(AttemptOutcome::AwaitingAgentInput(request))
            }

            Err(error)
                if resumed_input
                    .as_ref()
                    .is_some_and(|input| input.session.is_some())
                    && error.error_type.as_deref() == Some(agent::RESUME_SESSION_REJECTED) =>
            {
                let input = resumed_input.expect("guarded above");
                Ok(AttemptOutcome::AwaitingAgentInput(HumanInputRequest {
                    action_execution_id: action_execution_id.to_string(),
                    instruction: "The saved agent session could not be resumed. Review the message below, then press Enter to start a new session.".to_string(),
                    fields: vec![HumanInputField {
                        name: "answer".to_string(),
                        field_type: HumanInputType::Text,
                        required: true,
                        description: None,
                        options: Vec::new(),
                    }],
                    reason: None,
                    required_values: Vec::new(),
                    answer_tail: None,
                    agent_harness: None,
                    agent_session_id: None,
                    prefilled_answer: Some(input.answer),
                    permission: None,
                }))
            }
            Err(error) => match OutputRejection::from_error(state, action_execution_id, &error) {
                Some(rejection) => Ok(AttemptOutcome::OutputRejected(rejection)),
                None => {
                    let clean = self.attempt_left_no_trace(
                        kind,
                        &execution.id,
                        action_execution_id,
                        &error,
                    );
                    Err(AttemptFailure::of_action(error).side_effect_free_if(clean))
                }
            },
        }
    }

    /// Hands an already rendered action to its executor and closes the
    /// attempt's conversation once it is over.
    ///
    /// The outer `Err` is a failure before anything ran (no executor for the
    /// action type); the inner result is the executor's own.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn execute_rendered(
        &self,
        definition: &WorkflowDefinition,
        execution: &WorkflowExecution,
        state: &State,
        rendered: &ActionConfig,
        descriptions: &BTreeMap<String, String>,
        action_execution_id: &str,
        purpose: PromptPurpose,
        author: PromptAuthor,
        timeout: Option<Duration>,
    ) -> Result<Result<ActionOutcome, ActionError>, AttemptFailure> {
        let kind = ActionKind::of(rendered);
        let executor = self.executors.get(&kind).ok_or_else(|| {
            AttemptFailure::permanent(format!(
                "state '{}' uses a {} action, which is not available in this version \
                 of narsor",
                state.id,
                kind.name()
            ))
        })?;

        // The access a harness session runs with comes from the step's
        // `workspace` section; a run that already carries one (a reprompt,
        // a `profile` step without a section) keeps it.
        let with_access;
        let rendered = match (rendered.harness_step(), state.workspace_access()) {
            (Some(step), Some(access)) if step.workspace_access.is_none() => {
                let mut config = rendered.clone();
                if let Some(step) = config.harness_step_mut() {
                    step.workspace_access = Some(access);
                }
                with_access = config;
                &with_access
            }
            _ => rendered,
        };

        let conversation_store = self.store.conversation_store(&execution.id);
        let scope = StepScope::resolve(state.workspace.as_ref(), &execution.workspace)
            .with_descriptions(descriptions);
        let request = ActionRequest::new(rendered, &execution.workspace, action_execution_id)
            .with_scope(&scope)
            .with_timeout(timeout)
            .with_execution_id(&execution.id)
            .with_conversation_store(&conversation_store)
            .with_contract(StepContract::of(state))
            .with_purpose(purpose)
            .with_author(author)
            .with_workflow(definition, &state.id);

        let outcome = executor.execute(request).await;

        // The attempt is over, one way or another (completed, suspended,
        // timed out, or failed outright): whatever it wrote to the
        // conversation store is final, so this is where it is closed and
        // compressed (R15). A script or human action never opened a writer,
        // so this is a harmless no-op for them. Closing must never fail the
        // attempt itself — same principle as `LoggingEventSink` recording an
        // event: the durable record of what happened is already safe, and a
        // compression failure is not a reason to also fail the workflow.
        if let Err(error) = conversation_store.close_attempt(action_execution_id) {
            tracing::warn!(
                action_execution_id,
                %error,
                "could not close/compress this attempt's conversation file"
            );
        }

        Ok(outcome)
    }
}
