//! An agent output rejected as a format error (KTD7).

use super::*;

/// An agent attempt whose output was rejected as a format error (KTD7).
///
/// Persisted in `metadata.engine.output_rejection` from just before
/// `ActionOutputRejected` until the reprompt announces itself with
/// `OutputReprompted`. A restart in between finds the attempt closed in the
/// log — reconciliation leaves the execution `Running` — and the next pass
/// picks the reprompt up from this record instead of running the state's
/// action afresh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputRejection {
    /// The state whose action ran.
    pub state_id: StateId,

    /// The attempt whose output was rejected.
    pub action_execution_id: String,

    /// Why it was rejected.
    pub reason: String,

    /// The end of the agent's answer.
    #[serde(default)]
    pub answer_tail: Option<String>,

    /// Full response retained across a format reprompt.
    #[serde(default)]
    pub reply: Option<String>,

    /// The harness the attempt ran on — where its session can be resumed.
    #[serde(default)]
    pub harness: Option<String>,

    /// The harness's own session id, when it reported one.
    #[serde(default)]
    pub harness_session_id: Option<String>,

    /// The step's attempt under `retry_policy` (1-based) whose output was
    /// rejected: where the policy goes on from when a reprompt continued
    /// after a restart is still unusable (R13). Missing in a record from
    /// before it was kept, read as the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
}

impl OutputRejection {
    /// The rejection of an answer no label could be extracted from, when
    /// `error` is one — `None` for any other failure.
    pub(crate) fn from_error(
        state: &State,
        action_execution_id: &str,
        error: &ActionError,
    ) -> Option<Self> {
        if error.error_type.as_deref() != Some(MISSING_OUTPUT_LABEL) {
            return None;
        }
        let detail = |key: &str| {
            error
                .details
                .as_ref()
                .and_then(|details| details.get(key))
                .and_then(Value::as_str)
                .map(ToString::to_string)
        };
        Some(Self {
            state_id: state.id.clone(),
            action_execution_id: action_execution_id.to_string(),
            reason: error.message.clone(),
            answer_tail: detail("answer_tail").filter(|tail| !tail.is_empty()),
            reply: detail("reply"),
            harness: detail("harness"),
            harness_session_id: detail("harness_session_id"),
            attempt: None,
        })
    }

    /// The rejection of a result whose label the state does not declare, or
    /// that lacks what its choice requires (with `kept_locals`, the step's
    /// `local.*` values from earlier attempts) — `None` for a complete one.
    pub(crate) fn from_result(
        state: &State,
        action_execution_id: &str,
        result: &ActionResult,
        kept_locals: &Map<String, Value>,
    ) -> Option<Self> {
        let reason = match decision::check_label(state, &result.output_label) {
            Err(error @ DecisionError::LabelNotDeclared { .. }) => error.to_string(),
            Ok(_) => {
                let choice = state
                    .choices
                    .iter()
                    .find(|choice| choice.label == result.output_label)?;
                unmet_requirements(choice, result, kept_locals)
                    .or_else(|| variable_rules::value_outside_options(state, &result.variables))?
            }
            Err(_) => return None,
        };
        let session = result.data.get(SESSION_DATA_KEY);
        let session_field = |pointer: &str| {
            session
                .and_then(|session| session.pointer(pointer))
                .and_then(Value::as_str)
                .map(ToString::to_string)
        };
        Some(Self {
            state_id: state.id.clone(),
            action_execution_id: action_execution_id.to_string(),
            reason,
            answer_tail: gate_context::answer_tail(&result.stdout),
            reply: result.reply.clone(),
            harness: session_field("/harness"),
            harness_session_id: session_field("/config/harness_session_id"),
            attempt: None,
        })
    }

    /// The rejection of an agent that asked a question instead of giving the
    /// label it was asked for again (KTD7): a failed reprompt, never a
    /// suspension that would restart the step in a fresh session.
    pub(crate) fn from_question(state: &State, action_execution_id: &str, question: &str) -> Self {
        Self {
            state_id: state.id.clone(),
            action_execution_id: action_execution_id.to_string(),
            reason: format!(
                "the agent asked for human input instead of giving its output label: {question}"
            ),
            answer_tail: None,
            reply: None,
            harness: None,
            harness_session_id: None,
            attempt: None,
        }
    }
}

/// What `result` lacks of what `choice` requires, as the reason to ask the
/// agent again — `None` when nothing is missing.
///
/// A `local.*` variable the result does not give is met by the value its
/// step kept from an earlier attempt (`kept_locals`, KTD10).
fn unmet_requirements(
    choice: &ChoiceDefinition,
    result: &ActionResult,
    kept_locals: &Map<String, Value>,
) -> Option<String> {
    let missing_variables: Vec<&str> = choice
        .requires_variables
        .iter()
        .filter(|required| {
            result
                .variables
                .get(required.as_str())
                .or_else(|| kept_locals.get(required.as_str()))
                .is_none_or(human::is_blank)
        })
        .map(String::as_str)
        .collect();
    let missing_artifacts: Vec<&str> = choice
        .requires_artifacts
        .iter()
        .filter(|required| {
            result
                .artifacts
                .get(required.as_str())
                .is_none_or(|path| path.trim().is_empty())
        })
        .map(String::as_str)
        .collect();

    let mut missing = Vec::new();
    if !missing_variables.is_empty() {
        missing.push(format!(
            "is missing required variables: {}",
            missing_variables.join(", ")
        ));
    }
    if !missing_artifacts.is_empty() {
        missing.push(format!(
            "is missing required artifacts: {}",
            missing_artifacts.join(", ")
        ));
    }
    if missing.is_empty() {
        return None;
    }
    Some(format!(
        "output '{}' {}",
        choice.label,
        missing.join(" and ")
    ))
}

/// The rejected agent output `execution` still owes a reprompt at its current
/// state (KTD7), if any.
///
/// For the daemon's startup hook: an execution left `Running` with such a
/// record stopped between the rejection and the reprompt, and a new pass
/// picks the reprompt up rather than replaying the attempt.
pub fn owed_reprompt(
    execution: &WorkflowExecution,
) -> Result<Option<OutputRejection>, EngineError> {
    match execution.current_state.as_deref() {
        Some(state_id) => stored_output_rejection(execution, state_id),
        None => Ok(None),
    }
}

/// The rejected agent output of `execution` still owed its reprompt, when it
/// belongs to `state_id`.
pub(crate) fn stored_output_rejection(
    execution: &WorkflowExecution,
    state_id: &str,
) -> Result<Option<OutputRejection>, EngineError> {
    Ok(
        read_engine_record::<OutputRejection>(execution, METADATA_OUTPUT_REJECTION_KEY)?
            .filter(|rejection| rejection.state_id == state_id),
    )
}

impl Engine {
    /// Closes an attempt whose output was rejected: the record that lets a
    /// later pass continue with the reprompt, then `ActionOutputRejected`.
    ///
    /// In this order so that a crash between the two leaves the attempt open
    /// in the log — `Unknown`, as for any interrupted action — rather than a
    /// closed attempt with nothing to continue from, which the next pass would
    /// replay from the start of the state.
    pub(crate) fn reject_output(
        &self,
        execution: &mut WorkflowExecution,
        rejection: &OutputRejection,
    ) -> Result<(), EngineError> {
        let value = serde_json::to_value(rejection).map_err(|error| {
            EngineError::CorruptEngineMetadata {
                execution_id: execution.id.clone(),
                message: error.to_string(),
            }
        })?;
        engine_metadata_mut(execution).insert(METADATA_OUTPUT_REJECTION_KEY.to_string(), value);
        self.store.save_execution(execution)?;

        self.emit(
            &execution.id,
            EventData::ActionOutputRejected {
                action_execution_id: rejection.action_execution_id.clone(),
                state_id: rejection.state_id.clone(),
                reason: rejection.reason.clone(),
                answer_tail: rejection.answer_tail.clone(),
            },
        )?;
        Ok(())
    }

    /// Continues a step whose pass ended after its output was rejected and
    /// before the reprompt started (a daemon restart in between).
    pub(crate) async fn continue_rejected_output(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
        rejection: OutputRejection,
    ) -> Result<Flow, EngineError> {
        let Some(action) = &state.action else {
            clear_engine_metadata(execution, METADATA_OUTPUT_REJECTION_KEY);
            self.fail_action(
                execution,
                &rejection.action_execution_id,
                format!("state '{}' is not terminal but has no action", state.id),
            )?;
            return Ok(Flow::Stop);
        };
        let kind = ActionKind::of(action);
        let timeout = self
            .timeout_for(kind, state, definition)
            .map(Duration::from_secs);
        let attempt = rejection.attempt.unwrap_or(1);

        let upshot = match self
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
            Ok(upshot) => upshot,
            // A step whose own retries come next (R13): the policy judges
            // the failed reprompt as the attempt it closed, as it would have
            // without the restart.
            Err((failed_attempt, failure)) => {
                self.run_attempts(
                    definition,
                    execution,
                    state,
                    action,
                    AttemptsFrom {
                        attempt,
                        action_execution_id: failed_attempt,
                        failed: Some(failure),
                    },
                )
                .await?
            }
        };
        match upshot {
            ActionUpshot::Stopped => Ok(Flow::Stop),
            ActionUpshot::Transitioned => Ok(Flow::Continue),
            ActionUpshot::Completed {
                action_execution_id,
                result,
            } => self.record_completion(definition, execution, state, action_execution_id, result),
        }
    }
}
