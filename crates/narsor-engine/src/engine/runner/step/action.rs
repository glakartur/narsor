//! The action of a state, with its attempts under `retry_policy`.

use super::*;

impl Engine {
    /// The time limit of one attempt.
    ///
    /// The state's own value wins, then the workflow's global one — which is
    /// what R13 asks for: a setting is a default, never an instruction the
    /// workflow cannot override. Only when neither declares anything does the
    /// configured default apply, and only to an `agent` action (D-C).
    pub(crate) fn timeout_for(
        &self,
        kind: ActionKind,
        state: &State,
        definition: &WorkflowDefinition,
    ) -> Option<u64> {
        let declared = state.timeout_secs.or(definition.globals.timeout_secs);
        match kind {
            ActionKind::Agent | ActionKind::Profile => declared.or(self.agent_timeout_secs),
            // Unchanged, bit for bit: a script or human state that declares no
            // limit still has none. Spelled out rather than caught by `_`, so
            // that a fifth action kind has to decide rather than inherit.
            ActionKind::Script | ActionKind::Human | ActionKind::Wait | ActionKind::Decision => {
                declared
            }
        }
    }

    /// Runs the action of a state, retrying it as the policy allows (D21).
    ///
    /// A failure that survived every attempt is already recorded as
    /// `ActionFailed`, and a suspension is already persisted, so both come back
    /// as [`ActionUpshot::Stopped`].
    pub(crate) async fn run_action(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
        action: &ActionConfig,
    ) -> Result<ActionUpshot, EngineError> {
        let action_execution_id = Uuid::new_v4().to_string();
        execution.current_action_execution = Some(action_execution_id.clone());
        self.emit(
            &execution.id,
            EventData::ActionStarted {
                action_execution_id: action_execution_id.clone(),
                state_id: state.id.clone(),
                action_type: ActionKind::of(action).name().to_string(),
                decision_by: match action {
                    ActionConfig::Decision(decision) => Some(decision.by.name().to_string()),
                    _ => None,
                },
            },
        )?;

        self.run_attempts(
            definition,
            execution,
            state,
            action,
            AttemptsFrom {
                attempt: 1,
                action_execution_id,
                failed: None,
            },
        )
        .await
    }

    /// The attempts of a started action, from `from` on, as the policy
    /// allows (D21).
    pub(crate) async fn run_attempts(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
        action: &ActionConfig,
        from: AttemptsFrom,
    ) -> Result<ActionUpshot, EngineError> {
        let schedule = RetrySchedule::resolve(state, &definition.globals);
        let kind = ActionKind::of(action);
        let timeout = self
            .timeout_for(kind, state, definition)
            .map(Duration::from_secs);

        // The conductor's retry is one attempt, never a new round of the
        // policy (KTD14).
        let single_attempt = failure::conductor_retry_of(execution, &state.id)?.is_some();
        // What earlier attempts left is not known after a restart.
        let mut side_effect_free = from.failed.is_none();

        let AttemptsFrom {
            mut attempt,
            mut action_execution_id,
            mut failed,
        } = from;
        // The default harness this action runs on when it names none: read
        // at its first attempt, kept for every retry (R16a).
        let mut pinned_harness = None;
        let declared = DeclaredNames::of(definition);
        let descriptions = definition.repository_descriptions();

        loop {
            let attempted = match failed.take() {
                Some(failure) => Err(failure),
                None => {
                    self.attempt_action(
                        definition,
                        execution,
                        state,
                        action,
                        &declared,
                        &descriptions,
                        &action_execution_id,
                        timeout,
                        &mut pinned_harness,
                        AttemptProfile {
                            failures: attempt - 1,
                            announce: true,
                        },
                    )
                    .await
                }
            };

            let failure = match attempted {
                Ok(AttemptOutcome::AwaitingHumanInput(mut request)) => {
                    if let Some(record) = preceding_agent_step(execution, state) {
                        request.reason = record.reason;
                        request.required_values = record.required_values;
                        request.answer_tail = record.answer_tail;
                    }
                    self.suspend_for_input(
                        execution,
                        state,
                        request,
                        decision::permitted_outputs(state),
                    )?;
                    return Ok(ActionUpshot::Stopped);
                }
                Ok(AttemptOutcome::AwaitingAgentInput(request)) => {
                    let flow = self.route_agent_question(execution, state, request).await?;
                    return Ok(ActionUpshot::settled(flow));
                }
                Ok(AttemptOutcome::AwaitingConductor(message)) => {
                    let flow = self
                        .decide_by_conductor(
                            definition,
                            execution,
                            state,
                            &action_execution_id,
                            message,
                        )
                        .await?;
                    return Ok(ActionUpshot::settled(flow));
                }
                Ok(AttemptOutcome::OutputRejected(mut rejection)) => {
                    // Outside the retry schedule on purpose (R15): a format
                    // error is repaired in the same session, not re-run.
                    rejection.attempt = Some(attempt);
                    self.reject_output(execution, &rejection)?;
                    // Unless the step's own retries come next (R13): the
                    // reprompt attempt that is still unusable is the one
                    // that fails.
                    match self
                        .reprompt(
                            definition,
                            execution,
                            state,
                            action,
                            rejection,
                            timeout,
                            Unrepaired::of(kind),
                        )
                        .await?
                    {
                        Ok(upshot) => return Ok(upshot),
                        Err((failed_attempt, failure)) => {
                            action_execution_id = failed_attempt;
                            failure
                        }
                    }
                }
                Ok(AttemptOutcome::Completed(result)) => {
                    return Ok(ActionUpshot::Completed {
                        action_execution_id,
                        result,
                    })
                }
                Err(failure) => failure,
            };

            side_effect_free &= failure.side_effect_free;
            if !failure.retriable || single_attempt || !schedule.permits_another(attempt) {
                self.fail_attempts(execution, &action_execution_id, failure, side_effect_free)?;
                return Ok(ActionUpshot::Stopped);
            }

            let next_attempt = attempt + 1;
            let delay = schedule.delay_before(next_attempt);

            // Announced before the backoff sleep (R4, R7, R11): an operator
            // watching the log sees the failure the moment it happens, not
            // only once the delay has elapsed. The last attempt never reaches
            // here — it fails through the branch above, as `ActionFailed`.
            self.emit(
                &execution.id,
                EventData::ActionAttemptFailed {
                    action_execution_id: action_execution_id.clone(),
                    state_id: state.id.clone(),
                    error: failure.message,
                    retry_in_ms: delay.as_millis() as u64,
                },
            )?;

            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }

            attempt = next_attempt;

            // A fresh idempotency key per attempt (D24), announced in the event.
            action_execution_id = Uuid::new_v4().to_string();
            execution.current_action_execution = Some(action_execution_id.clone());
            self.emit(
                &execution.id,
                EventData::ActionRetrying {
                    action_execution_id: action_execution_id.clone(),
                    retry_count: RetrySchedule::retry_count(attempt),
                    state_id: Some(state.id.clone()),
                },
            )?;
        }
    }
}
