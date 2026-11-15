//! The record of a step whose output the conductor rescues (F1, U10).

use super::*;

/// A step whose agent gave no usable output label, even when asked again,
/// handed to the conductor (F1, U10).
///
/// Persisted in `metadata.engine.output_rescue` from the moment the step is
/// handed over — before the event that closes the last attempt — until its
/// transition is taken or it becomes a problem. A pass that finds it (after
/// a restart, or an interrupt) carries the rescue on instead of entering
/// the state afresh: nothing the conductor does can have a side effect, so
/// this is the one place a pass continues rather than replays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputRescue {
    /// The state whose output is rescued.
    pub state_id: StateId,

    /// The agent's answers without a label, oldest first. The last one's
    /// attempt is the rescue's episode (KTD9) and the attempt the step
    /// completes under.
    pub answers: Vec<RejectedAnswer>,

    /// Why the step has no label yet.
    pub reason: String,

    /// The harness the agent's session ran on, when it said.
    #[serde(default)]
    pub harness: Option<String>,

    /// The agent's session, to ask it again in (the conductor's (b)).
    #[serde(default)]
    pub harness_session_id: Option<String>,

    /// Whether the conductor's one reprompt was used (R17).
    #[serde(default)]
    pub conductor_reprompted: bool,

    /// The step's reply from the rejected attempts (R3): the first that had
    /// one, kept for whatever settles the label.
    #[serde(default)]
    pub reply: Option<String>,
}

impl OutputRescue {
    /// The rescue of the step whose first answer was `rejection`.
    pub(crate) fn after(rejection: &OutputRejection) -> Self {
        Self {
            state_id: rejection.state_id.clone(),
            answers: vec![RejectedAnswer::from(rejection)],
            reason: rejection.reason.clone(),
            harness: rejection.harness.clone(),
            harness_session_id: rejection.harness_session_id.clone(),
            conductor_reprompted: false,
            reply: non_blank(rejection.reply.as_deref()),
        }
    }

    /// Adds the answer of a reprompt that gave no label.
    pub(crate) fn record(&mut self, failure: &UnlabelledAnswer) {
        self.answers.push(failure.answer.clone());
        self.reason = failure.reason.clone();
        // A reprompt's text counts only when no earlier attempt had any (R3).
        if self.reply.is_none() {
            self.reply = non_blank(failure.reply.as_deref());
        }
        if failure.harness_session_id.is_some() {
            self.harness = failure.harness.clone().or_else(|| self.harness.clone());
            self.harness_session_id = failure.harness_session_id.clone();
        }
    }

    /// The episode of the rescue (KTD9): the attempt of the last answer.
    pub fn episode(&self) -> &str {
        self.answers
            .last()
            .map(|answer| answer.action_execution_id.as_str())
            .unwrap_or_default()
    }

    /// Whether the conductor may still ask the agent again: once, and only
    /// in a session there is to resume (R17).
    pub fn can_reprompt(&self) -> bool {
        !self.conductor_reprompted && self.harness.is_some() && self.harness_session_id.is_some()
    }
}

impl From<&OutputRejection> for RejectedAnswer {
    fn from(rejection: &OutputRejection) -> Self {
        Self {
            action_execution_id: rejection.action_execution_id.clone(),
            reason: rejection.reason.clone(),
            answer_tail: rejection.answer_tail.clone(),
        }
    }
}

/// The rescue of an output label (F1) `execution` still owes at its current
/// state, if any.
///
/// For the daemon's startup hook: an execution left `Running` with such a
/// record stopped while the conductor worked on its step — before it was
/// asked, while it was invoked, or after its decision was saved — and a new
/// pass carries the rescue on.
pub fn owed_output_rescue(
    execution: &WorkflowExecution,
) -> Result<Option<OutputRescue>, EngineError> {
    match execution.current_state.as_deref() {
        Some(state_id) => stored_output_rescue(execution, state_id),
        None => Ok(None),
    }
}

/// The rescue of `execution`'s output label under way, when it belongs to
/// `state_id`.
pub(crate) fn stored_output_rescue(
    execution: &WorkflowExecution,
    state_id: &str,
) -> Result<Option<OutputRescue>, EngineError> {
    Ok(
        read_engine_record::<OutputRescue>(execution, METADATA_OUTPUT_RESCUE_KEY)?
            .filter(|rescue| rescue.state_id == state_id),
    )
}

impl Engine {
    /// Records `rescue` as the step's rescue under way, replacing a rejected
    /// output still owed its reprompt, and saves the execution.
    pub(crate) fn begin_rescue(
        &self,
        execution: &mut WorkflowExecution,
        rescue: &OutputRescue,
    ) -> Result<(), EngineError> {
        let value =
            serde_json::to_value(rescue).map_err(|error| EngineError::CorruptEngineMetadata {
                execution_id: execution.id.clone(),
                message: error.to_string(),
            })?;
        clear_engine_metadata(execution, METADATA_OUTPUT_REJECTION_KEY);
        engine_metadata_mut(execution).insert(METADATA_OUTPUT_RESCUE_KEY.to_string(), value);
        self.store.save_execution(execution)?;
        Ok(())
    }
}
