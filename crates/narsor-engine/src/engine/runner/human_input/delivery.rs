//! The input a person or the conductor delivers to a suspended step.

use super::*;

/// Converts the agent-response form into the text injected into the next
/// harness turn. A choice is clearly distinguished from arbitrary prose, and
/// an optional comment stays attached to that choice.
pub(crate) fn agent_reply(values: &Map<String, Value>) -> Option<String> {
    let text = |name: &str| {
        values
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    if let Some(answer) = text("answer") {
        return Some(answer.to_string());
    }
    let option = text("option")?;
    let mut reply = format!("Selected option: {option}");
    if let Some(comment) = text("comment") {
        reply.push_str("\nComment: ");
        reply.push_str(comment);
    }
    Some(reply)
}

impl Engine {
    /// The question an execution is waiting on, if it is waiting on one.
    pub fn pending_input(
        &self,
        execution_id: &str,
    ) -> Result<Option<PendingHumanInput>, EngineError> {
        Ok(human::pending_input(&self.store, execution_id)?)
    }

    /// Delivers an answer to an execution needing input and runs it on.
    ///
    /// Serves both places an execution can wait for a person: a `human` action,
    /// where the answer *is* the action's result, and a `human` decision, where
    /// the action already finished and the answer only picks the transition.
    /// The two are told apart by the persisted mid-step state: a decision is
    /// pending exactly when the action's result is already recorded.
    pub async fn deliver_human_input(
        &self,
        definition: &WorkflowDefinition,
        execution_id: &str,
        delivery: &HumanInputDelivery,
    ) -> Result<WorkflowExecution, EngineError> {
        self.observed(execution_id, async {
            let _guard = self.begin_run(execution_id)?;
            match self.prepare_deliver_human_input(definition, execution_id, delivery)? {
                HandleState::Done(execution) => Ok(execution),
                HandleState::Ready(execution) => self.run_loop(definition, execution).await,
            }
        })
        .await
    }

    /// The synchronous half of [`Engine::deliver_human_input`]: records the
    /// answer, applies it (as a decision or as an action's own result,
    /// depending on what was pending), and reports whether the step already
    /// stopped or still needs `run_loop`. Shared between the borrowed
    /// (`deliver_human_input`) and owned
    /// ([`Engine::try_begin_deliver_human_input`]) entry points.
    fn prepare_deliver_human_input(
        &self,
        definition: &WorkflowDefinition,
        execution_id: &str,
        delivery: &HumanInputDelivery,
    ) -> Result<HandleState, EngineError> {
        let from = self.store.event_log(execution_id).next_sequence()?;
        let resumed = human::resume(&self.store, execution_id, delivery)?;
        self.publish_from(execution_id, from)?;

        let mut execution = resumed.execution;
        self.lock_pause_requests().remove(execution_id);

        let state_id = self.current_state_id(&execution)?;
        let state = self.state_of(definition, &execution, &state_id)?;

        if resumed.pending.continuation == InputContinuation::ApproveOutput {
            let candidate = resumed.pending.approval.ok_or_else(|| {
                EngineError::HumanInput(HumanActionError::NoPendingRequest {
                    execution_id: execution_id.to_string(),
                })
            })?;
            if resumed.result.output_label == APPROVE_OUTPUT_LABEL {
                let output_label = candidate.result.output_label.clone();
                let pending = PendingStep {
                    state_id: state_id.clone(),
                    action_execution_id: candidate.action_execution_id,
                    result: candidate.result,
                    artifacts: candidate.artifacts,
                };
                let flow = self.apply_decision(
                    definition,
                    &mut execution,
                    state,
                    pending,
                    DecisionOutcome {
                        label: output_label,
                        provider: DecisionProviderType::Human,
                        reasoning: None,
                    },
                )?;
                return Ok(match flow {
                    Flow::Stop => HandleState::Done(execution),
                    Flow::Continue => HandleState::Ready(execution),
                });
            }
            let answer = agent_reply(&resumed.result.variables).ok_or_else(|| {
                EngineError::HumanInput(HumanActionError::MissingRequiredField {
                    field: "comment".to_string(),
                })
            })?;
            // The held result must not be completed on the next loop pass:
            // this delivery explicitly rejects it and re-enters the agent.
            self.clear_pending_step(&mut execution);
            clear_engine_metadata(&mut execution, METADATA_OUTPUT_REJECTION_KEY);
            clear_engine_metadata(&mut execution, METADATA_OUTPUT_RESCUE_KEY);
            self.store_agent_input(
                &mut execution,
                &state_id,
                &answer,
                candidate.agent_harness.as_deref(),
                candidate.agent_session_id.as_deref(),
                Vec::new(),
                PromptAuthor::User,
            )?;
            return Ok(HandleState::Ready(execution));
        }

        if resumed.pending.continuation == InputContinuation::EntryLimit {
            if resumed.result.output_label == ENTRY_LIMIT_CONTINUE_LABEL
                || resumed.result.output_label == ENTRY_LIMIT_RESET_LABEL
            {
                return Ok(HandleState::Ready(execution));
            }

            let outcome = DecisionOutcome {
                label: resumed.result.output_label.clone(),
                provider: DecisionProviderType::Human,
                reasoning: delivery.comment.clone(),
            };
            let flow = self.apply_decision(
                definition,
                &mut execution,
                state,
                PendingStep {
                    state_id: state_id.clone(),
                    action_execution_id: resumed.pending.action_execution_id().to_string(),
                    result: resumed.result,
                    artifacts: Vec::new(),
                },
                outcome,
            )?;
            return Ok(match flow {
                Flow::Stop => HandleState::Done(execution),
                Flow::Continue => HandleState::Ready(execution),
            });
        }

        if resumed.pending.continuation == InputContinuation::ResumeAgent {
            let answer = agent_reply(&resumed.result.variables).ok_or_else(|| {
                EngineError::HumanInput(HumanActionError::MissingRequiredField {
                    field: "answer or option".to_string(),
                })
            })?;
            self.store_agent_input(
                &mut execution,
                &state_id,
                &answer,
                resumed.pending.request.agent_harness.as_deref(),
                resumed.pending.request.agent_session_id.as_deref(),
                Vec::new(),
                prompt_author(delivery.answered_by),
            )?;
            return Ok(HandleState::Ready(execution));
        }

        // A problem form can explicitly return to the agent instead of
        // settling its output. This is a fresh runner state entry: a previously
        // exhausted reprompt or conductor rescue must not be inherited by the
        // next attempt.
        if resumed.pending.continuation == InputContinuation::ResolveOutput
            && resumed.result.output_label == RESUME_AGENT_LABEL
        {
            let answer = agent_reply(&resumed.result.variables).ok_or_else(|| {
                EngineError::HumanInput(HumanActionError::MissingRequiredField {
                    field: "agent_message".to_string(),
                })
            })?;
            clear_engine_metadata(&mut execution, METADATA_OUTPUT_REJECTION_KEY);
            clear_engine_metadata(&mut execution, METADATA_OUTPUT_RESCUE_KEY);
            self.store_agent_input(
                &mut execution,
                &state_id,
                &answer,
                resumed.pending.request.agent_harness.as_deref(),
                resumed.pending.request.agent_session_id.as_deref(),
                Vec::new(),
                prompt_author(delivery.answered_by),
            )?;
            return Ok(HandleState::Ready(execution));
        }

        if resumed.pending.continuation == InputContinuation::ResolveOutput {
            // The answer is the output the agent did not give: recorded as
            // the step's result, and the label it names is the decision,
            // made by whoever answered rather than by the state's provider.
            let outcome = DecisionOutcome {
                label: resumed.result.output_label.clone(),
                provider: match delivery.answered_by {
                    AnsweredBy::Operator => DecisionProviderType::Human,
                    AnsweredBy::Conductor => DecisionProviderType::Conductor,
                },
                reasoning: delivery
                    .comment
                    .as_deref()
                    .map(str::trim)
                    .filter(|comment| !comment.is_empty())
                    .map(ToString::to_string),
            };
            let mut result = resumed.result;
            // What the agent last said, for a gate the label may lead to: it
            // is the answer the operator settled, and the gate shows it
            // (KTD8).
            result.stdout = resumed
                .pending
                .problem
                .as_ref()
                .and_then(|problem| {
                    problem
                        .answers
                        .iter()
                        .rev()
                        .find_map(|answer| answer.answer_tail.clone())
                })
                .unwrap_or_default();
            // The reply of the rejected attempts: the operator settles the
            // label, not what the agent said (R3).
            result.reply = resumed
                .pending
                .problem
                .as_ref()
                .and_then(|problem| problem.reply.clone());
            let flow = match self.record_result(
                &mut execution,
                state,
                resumed.pending.action_execution_id().to_string(),
                result,
            )? {
                Some(pending) => {
                    self.apply_decision(definition, &mut execution, state, pending, outcome)?
                }
                None => Flow::Stop,
            };
            return Ok(match flow {
                Flow::Stop => HandleState::Done(execution),
                Flow::Continue => HandleState::Ready(execution),
            });
        }

        // A marked human output still completes its own step and records the
        // declared transition.  The only difference is how the target agent
        // starts: its required text field becomes one-shot continuation input
        // instead of an ordinary fresh prompt with no operator context.
        if resumed.pending.continuation == InputContinuation::CompleteStep {
            if let Some(field) = state
                .choices
                .iter()
                .find(|output| output.label == resumed.result.output_label)
                .and_then(|output| output.resume_comment_field.as_deref())
            {
                let comment = resumed
                    .result
                    .variables
                    .get(field)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let target = state
                    .transitions
                    .get(&resumed.result.output_label)
                    .cloned()
                    .ok_or_else(|| {
                        EngineError::HumanInput(HumanActionError::NoLabelChosen {
                            outputs: state
                                .choices
                                .iter()
                                .map(|output| output.label.clone())
                                .collect(),
                        })
                    })?;
                let provenance = resumed
                    .pending
                    .resume_targets
                    .get(&resumed.result.output_label)
                    .cloned();
                let flow = self.record_completion(
                    definition,
                    &mut execution,
                    state,
                    resumed.pending.action_execution_id().to_string(),
                    resumed.result,
                )?;
                if matches!(flow, Flow::Continue) {
                    if provenance.is_none() {
                        let request = HumanInputRequest {
                            action_execution_id: execution
                                .current_action_execution
                                .clone()
                                .unwrap_or_default(),
                            instruction: "The saved agent session is unavailable. Review the preserved comment, then press Enter to start a new session.".to_string(),
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
                            prefilled_answer: Some(comment),
                            permission: None,
                        };
                        let target_state = self.state_of(definition, &execution, &target)?;
                        self.suspend_for_agent_input(&mut execution, target_state, request)?;
                        return Ok(HandleState::Done(execution));
                    }
                    self.store_agent_input(
                        &mut execution,
                        &target,
                        &comment,
                        provenance.as_ref().map(|session| session.harness.as_str()),
                        provenance
                            .as_ref()
                            .map(|session| session.session_id.as_str()),
                        Vec::new(),
                        PromptAuthor::User,
                    )?;
                    return Ok(HandleState::Ready(execution));
                }
                return Ok(HandleState::Done(execution));
            }
        }

        let flow = match self.pending_step(&execution, &state_id)? {
            Some(pending) => {
                // The action had already finished: this answer is the decision.
                let outcome = DecisionOutcome {
                    label: resumed.result.output_label.clone(),
                    provider: DecisionProviderType::Human,
                    reasoning: None,
                };
                self.apply_decision(definition, &mut execution, state, pending, outcome)?
            }
            None => {
                // The answer is the human action's own result.
                self.record_completion(
                    definition,
                    &mut execution,
                    state,
                    resumed.pending.action_execution_id().to_string(),
                    resumed.result,
                )?
            }
        };

        Ok(match flow {
            Flow::Stop => HandleState::Done(execution),
            Flow::Continue => HandleState::Ready(execution),
        })
    }

    /// Phase 1 of the two-phase `deliver_human_input` (U3): claims the
    /// execution synchronously — returning [`EngineError::AlreadyRunning`]
    /// immediately, with no `.await`, if another pass already holds it — and
    /// does the same synchronous recording/decision work
    /// [`Engine::deliver_human_input`] does. On success, hand the returned
    /// [`RunHandle`] to [`RunHandle::run_to_completion`], typically via
    /// `tokio::spawn`, to drive the workflow the rest of the way.
    pub fn try_begin_deliver_human_input(
        self: &Arc<Self>,
        definition: &WorkflowDefinition,
        execution_id: &str,
        delivery: &HumanInputDelivery,
    ) -> Result<RunHandle, EngineError> {
        let claim = self.claim_active_run(execution_id)?;
        match self.prepare_deliver_human_input(definition, execution_id, delivery) {
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
