//! Asking the agent again for the label it did not give (KTD7).

use super::*;

/// Where an output the engine's reprompt did not repair goes.
#[derive(Clone, Copy)]
pub(crate) enum Unrepaired {
    /// To the conductor's rescue (F1).
    Rescued,
    /// Back as a failed attempt, for `retry_policy` (R13).
    Retried,
}

impl Unrepaired {
    /// Where an unrepaired output of a `kind` action goes.
    pub(crate) fn of(kind: ActionKind) -> Self {
        if kind.retries_unrepaired_output() {
            Self::Retried
        } else {
            Self::Rescued
        }
    }
}

/// How one attempt asking the agent again for its label ended.
pub(crate) enum RepromptEnd {
    /// The agent gave a label the state accepts.
    Labelled {
        /// The reprompt attempt.
        action_execution_id: String,
        /// Its result.
        result: ActionResult,
    },
    /// It gave none; the attempt is still to be closed.
    Unlabelled(UnlabelledAnswer),
    /// It asked a question instead; the attempt stays open until answered.
    Asked(HumanInputRequest),
    /// The execution failed, and that is already persisted.
    Stopped,
}

/// A reprompt attempt that gave no usable label.
pub(crate) struct UnlabelledAnswer {
    /// The answer, as a problem shows it.
    pub(crate) answer: RejectedAnswer,
    /// Why the step still has no label.
    pub(crate) reason: String,
    /// The harness the attempt ran on, when it said.
    pub(crate) harness: Option<String>,
    /// The session it ran in, when it said.
    pub(crate) harness_session_id: Option<String>,
    /// The attempt's reply, when it had one.
    pub(crate) reply: Option<String>,
}

/// How a reprompt ended without a label the state accepts.
enum RepromptFailure {
    /// Another format error — including a question instead of the label.
    Rejected(OutputRejection),
    /// The run itself failed or timed out.
    Failed(String),
}

/// `result` of a format reprompt, with the earlier attempt's reply in place
/// of its own when there was one: a reprompt never replaces it (R3).
fn keep_previous_reply(mut result: ActionResult, previous_reply: Option<&str>) -> ActionResult {
    if let Some(previous) = non_blank(previous_reply) {
        result.reply = Some(previous);
    }
    result
}

impl Engine {
    /// The engine's one reprompt (R14): a new attempt resuming the rejected
    /// attempt's harness session, read-only, on the harness it ran on, asking
    /// only for the output object.
    ///
    /// Not a `retry_policy` retry (R15): it is announced by `OutputReprompted`,
    /// never `ActionRetrying`. Whatever it ends with but a usable answer —
    /// another format error, a failure — and a rejected attempt with no
    /// session to resume, goes where `unrepaired` says: to the conductor
    /// (F1, [`Engine::rescue_output`]), or back as the failure of the still
    /// open attempt that gave it, for `retry_policy` to judge (R13).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn reprompt(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
        action: &ActionConfig,
        rejection: OutputRejection,
        timeout: Option<Duration>,
        unrepaired: Unrepaired,
    ) -> Result<Result<ActionUpshot, (String, AttemptFailure)>, EngineError> {
        let Some(config) = self.reprompt_config(execution, state, action, &rejection) else {
            return match unrepaired {
                Unrepaired::Rescued => {
                    let mut rescue = OutputRescue::after(&rejection);
                    rescue.reason = format!(
                        "{} The agent could not be asked again for its output label: its \
                         harness reported no session to resume.",
                        rejection.reason
                    );
                    self.begin_rescue(execution, &rescue)?;
                    let flow = self
                        .rescue_output(definition, execution, state, rescue, timeout)
                        .await?;
                    Ok(Ok(ActionUpshot::settled(flow)))
                }
                Unrepaired::Retried => {
                    // Nothing is owed any more: the step's next attempt
                    // starts afresh.
                    clear_engine_metadata(execution, METADATA_OUTPUT_REJECTION_KEY);
                    self.store.save_execution(execution)?;
                    Ok(Err((
                        rejection.action_execution_id,
                        AttemptFailure::retriable(format!(
                            "{} It could not be asked again: its harness reported no session \
                             to resume.",
                            rejection.reason
                        )),
                    )))
                }
            };
        };

        let end = self
            .reprompt_attempt(
                definition,
                execution,
                state,
                config,
                &rejection.action_execution_id,
                rejection.reply.as_deref(),
                RepromptedBy::Engine,
                timeout,
            )
            .await?;
        Ok(Ok(match end {
            RepromptEnd::Labelled {
                action_execution_id,
                result,
            } => ActionUpshot::Completed {
                action_execution_id,
                result,
            },
            RepromptEnd::Stopped => ActionUpshot::Stopped,
            RepromptEnd::Asked(request) => {
                let flow = self.route_agent_question(execution, state, request).await?;
                ActionUpshot::settled(flow)
            }
            RepromptEnd::Unlabelled(failure) => match unrepaired {
                Unrepaired::Rescued => {
                    let mut rescue = OutputRescue::after(&rejection);
                    rescue.record(&failure);
                    // The record before the event that closes the attempt: a
                    // crash in between leaves the attempt open (`Unknown`),
                    // and one after it leaves the rescue to continue from (F1).
                    self.begin_rescue(execution, &rescue)?;
                    self.close_unlabelled(execution, state, &failure.answer)?;
                    let flow = self
                        .rescue_output(definition, execution, state, rescue, timeout)
                        .await?;
                    ActionUpshot::settled(flow)
                }
                Unrepaired::Retried => {
                    return Ok(Err((
                        failure.answer.action_execution_id,
                        AttemptFailure::retriable(failure.reason),
                    )))
                }
            },
        }))
    }

    /// The configuration of the engine's reprompt of `rejection`: its
    /// harness session resumed, read-only, asking only for the output.
    /// `None` when the action cannot be asked again, or its harness
    /// reported no session to resume.
    pub(crate) fn reprompt_config(
        &self,
        execution: &mut WorkflowExecution,
        state: &State,
        action: &ActionConfig,
        rejection: &OutputRejection,
    ) -> Option<ActionConfig> {
        let (Some(harness), Some(session)) = (&rejection.harness, &rejection.harness_session_id)
        else {
            return None;
        };
        let text = ActionKind::of(action).reprompt_text()?;
        let message = format!("{text}\n\nWhy it was rejected: {}", rejection.reason);
        agent::reprompt_config(
            self.profiled_action(execution, state, action),
            harness,
            session,
            &message,
        )
    }

    /// One attempt asking the agent of `previous_attempt` again for its
    /// output label, resuming its session with `config` (KTD7) — the engine's
    /// reprompt or the conductor's (F1).
    ///
    /// An attempt that ends without a label is returned as it is, still
    /// open: closing it with `ActionOutputRejected` is the caller's, after
    /// it recorded what comes next.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn reprompt_attempt(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
        rendered: ActionConfig,
        previous_attempt: &str,
        previous_reply: Option<&str>,
        by: RepromptedBy,
        timeout: Option<Duration>,
    ) -> Result<RepromptEnd, EngineError> {
        // Announced before the record goes: a crash in between leaves this
        // attempt open in the log (`Unknown`), never a closed attempt with
        // nothing left to continue from.
        let action_execution_id = Uuid::new_v4().to_string();
        execution.current_action_execution = Some(action_execution_id.clone());
        self.emit(
            &execution.id,
            EventData::OutputReprompted {
                action_execution_id: action_execution_id.clone(),
                previous_attempt: previous_attempt.to_string(),
                state_id: state.id.clone(),
                by,
            },
        )?;
        clear_engine_metadata(execution, METADATA_OUTPUT_REJECTION_KEY);
        self.store.save_execution(execution)?;

        let outcome = match self
            .execute_rendered(
                definition,
                execution,
                state,
                &rendered,
                // A reprompt resumes the agent's session, which was told about
                // the repositories when it began: no block, no descriptions.
                &BTreeMap::new(),
                &action_execution_id,
                PromptPurpose::Reprompt,
                match by {
                    RepromptedBy::Engine => PromptAuthor::Engine,
                    RepromptedBy::Conductor => PromptAuthor::Conductor,
                },
                timeout,
            )
            .await
        {
            Ok(outcome) => outcome,
            Err(failure) => {
                self.fail_attempts(execution, &action_execution_id, failure, false)?;
                return Ok(RepromptEnd::Stopped);
            }
        };

        let kept_locals = step_memory::step_locals(execution, &state.id)?;
        let failure = match outcome {
            Ok(ActionOutcome::Completed(result)) => {
                match OutputRejection::from_result(
                    state,
                    &action_execution_id,
                    &result,
                    &kept_locals,
                ) {
                    None => {
                        return Ok(RepromptEnd::Labelled {
                            action_execution_id,
                            result: keep_previous_reply(result, previous_reply),
                        });
                    }
                    Some(rejection) => RepromptFailure::Rejected(rejection),
                }
            }
            Ok(ActionOutcome::TimedOut(timed_out)) => {
                match timed_out.resolve(&decision::permitted_labels(state)) {
                    Ok(result) => {
                        return Ok(RepromptEnd::Labelled {
                            action_execution_id,
                            result: keep_previous_reply(result, previous_reply),
                        })
                    }
                    Err(error) => RepromptFailure::Failed(error.message),
                }
            }
            // A question is the agent's to ask, here as in any step: it is
            // answered and the agent goes on in its own session.
            Ok(ActionOutcome::Suspended(SuspendedAction::AwaitingAgentInput(request))) => {
                return Ok(RepromptEnd::Asked(request));
            }
            Ok(ActionOutcome::Suspended(SuspendedAction::AwaitingHumanInput(request))) => {
                RepromptFailure::Rejected(OutputRejection::from_question(
                    state,
                    &action_execution_id,
                    &request.instruction,
                ))
            }
            Err(error) => match OutputRejection::from_error(state, &action_execution_id, &error) {
                Some(rejection) => RepromptFailure::Rejected(rejection),
                None => RepromptFailure::Failed(error.message),
            },
        };

        // Either way the attempt is over without a label. A reprompt that
        // failed outright ends the same way — it is a failed reprompt, not an
        // action failure, and `retry_policy` does not apply to it.
        Ok(RepromptEnd::Unlabelled(match failure {
            RepromptFailure::Rejected(rejection) => UnlabelledAnswer {
                answer: RejectedAnswer::from(&rejection),
                reason: format!(
                    "the agent did not give a usable output label even when asked again: {}",
                    rejection.reason
                ),
                harness: rejection.harness,
                harness_session_id: rejection.harness_session_id,
                reply: rejection.reply,
            },
            RepromptFailure::Failed(message) => {
                let reason =
                    format!("asking the agent again for its output label failed: {message}");
                UnlabelledAnswer {
                    answer: RejectedAnswer {
                        action_execution_id: action_execution_id.clone(),
                        reason: reason.clone(),
                        answer_tail: None,
                    },
                    reason,
                    harness: None,
                    harness_session_id: None,
                    reply: None,
                }
            }
        }))
    }

    /// Closes a reprompt attempt that gave no label: `ActionOutputRejected`.
    pub(crate) fn close_unlabelled(
        &self,
        execution: &WorkflowExecution,
        state: &State,
        answer: &RejectedAnswer,
    ) -> Result<(), EngineError> {
        self.emit(
            &execution.id,
            EventData::ActionOutputRejected {
                action_execution_id: answer.action_execution_id.clone(),
                state_id: state.id.clone(),
                reason: answer.reason.clone(),
                answer_tail: answer.answer_tail.clone(),
            },
        )?;
        Ok(())
    }
}
