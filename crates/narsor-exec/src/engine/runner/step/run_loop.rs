//! The loop of a pass: one state after the other.

use super::*;

impl Engine {
    /// Steps until the execution stops being runnable.
    pub(crate) async fn run_loop(
        &self,
        definition: &WorkflowDefinition,
        execution: WorkflowExecution,
    ) -> Result<WorkflowExecution, EngineError> {
        self.step_loop(definition, execution, None).await
    }

    /// Steps until the execution stops being runnable or, when `limit` is
    /// given, until that many step boundaries have been crossed.
    pub(crate) async fn step_loop(
        &self,
        definition: &WorkflowDefinition,
        mut execution: WorkflowExecution,
        limit: Option<usize>,
    ) -> Result<WorkflowExecution, EngineError> {
        let mut crossed = 0;
        loop {
            if execution.status != ExecutionStatus::Running {
                break;
            }
            if limit.is_some_and(|limit| crossed >= limit) {
                break;
            }

            if self.take_pause_request(&execution.id) {
                self.pause_execution(&mut execution, "paused on operator request")?;
                break;
            }

            let state_id = self.current_state_id(&execution)?;
            let state = self.state_of(definition, &execution, &state_id)?;

            let flow = if let Some(rescue) = stored_output_rescue(&execution, &state_id)? {
                // A pass that ended while the conductor was rescuing the
                // step's output (a restart, an interrupt): the rescue goes on
                // from its record, never from the start of the state.
                self.continue_output_rescue(definition, &mut execution, state, rescue)
                    .await?
            } else {
                match self.pending_step(&execution, &state_id)? {
                    Some(pending) => {
                        self.complete_step(definition, &mut execution, state, pending)?
                    }
                    None => match stored_output_rejection(&execution, &state_id)? {
                        // A pass that ended between a rejected output and its
                        // reprompt: the attempt is closed, so it continues with
                        // the reprompt instead of running the action afresh.
                        Some(rejection) => {
                            self.continue_rejected_output(
                                definition,
                                &mut execution,
                                state,
                                rejection,
                            )
                            .await?
                        }
                        None => self.enter_state(definition, &mut execution, state).await?,
                    },
                }
            };

            if flow == Flow::Stop {
                break;
            }
            crossed += 1;
        }

        Ok(execution)
    }

    /// Enters a state: `StateEntered`, then either the end of the workflow or
    /// the state's action.
    pub(crate) async fn enter_state(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
    ) -> Result<Flow, EngineError> {
        execution.current_state = Some(state.id.clone());
        self.emit(
            &execution.id,
            EventData::StateEntered {
                state_id: state.id.clone(),
            },
        )?;

        if state.terminal {
            self.finish(execution, state)?;
            return Ok(Flow::Stop);
        }

        let Some(action) = &state.action else {
            // Static validation rejects this, so reaching it means the
            // definition was changed under a running execution.
            let id = Uuid::new_v4().to_string();
            self.emit(
                &execution.id,
                EventData::ActionStarted {
                    action_execution_id: id.clone(),
                    state_id: state.id.clone(),
                    action_type: "none".to_string(),
                    decision_by: None,
                },
            )?;
            self.fail_action(
                execution,
                &id,
                format!("state '{}' is not terminal but has no action", state.id),
            )?;
            return Ok(Flow::Stop);
        };

        match self
            .run_action(definition, execution, state, action)
            .await?
        {
            ActionUpshot::Stopped => Ok(Flow::Stop),
            ActionUpshot::Transitioned => Ok(Flow::Continue),
            ActionUpshot::Completed {
                action_execution_id,
                result,
            } => self.record_completion(definition, execution, state, action_execution_id, result),
        }
    }

    pub(crate) fn wait_on_entry_limit(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
        entries: u32,
        max_entries: u32,
    ) -> Result<(), EngineError> {
        let attempt_id = Uuid::new_v4().to_string();
        let request = HumanInputRequest {
            action_execution_id: attempt_id,
            instruction: format!(
                "Krok '{}' osiągnął limit wejść: {entries}/{max_entries}. Wybierz kontynuację albo wyjście.",
                state.id
            ),
            fields: vec![HumanInputField {
                name: human::ENTRY_LIMIT_DECISION_FIELD.to_string(),
                field_type: HumanInputType::Select,
                options: vec![
                    human::ENTRY_LIMIT_CONTINUE.to_string(),
                    human::ENTRY_LIMIT_RESET_COUNTER.to_string(),
                ],
                required: false,
                description: Some("Kontynuuj albo rozpocznij licznik od nowa".to_string()),
            }],
            reason: None,
            required_values: Vec::new(),
            answer_tail: None,
            agent_harness: None,
            agent_session_id: None,
            prefilled_answer: None,
            permission: None,
        };
        let requirements = output_requirements(definition, execution, state);
        let problem = OutputProblem {
            reason: format!("entry limit reached ({entries}/{max_entries})"),
            answers: Vec::new(),
            requirements,
            analysis: None,
            reply: None,
        };
        let pending = PendingHumanInput::for_entry_limit(
            execution.id.clone(),
            state.id.clone(),
            request,
            state.choices.clone(),
            problem,
            EntryLimitDetails {
                entries,
                max_entries,
            },
        );
        human::suspend(&self.store, execution, &pending)?;
        self.emit(
            &execution.id,
            EventData::EntryLimitReached {
                state_id: state.id.clone(),
                entries,
                max_entries,
            },
        )?;
        Ok(())
    }
}
