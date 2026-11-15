//! The agent or script step behind the latest transition (KTD8, KTD11).

use super::*;

/// What the agent or script step behind the latest transition said (KTD8,
/// KTD11).
///
/// Persisted in `metadata.engine.last_agent_step` when a transition leaves an
/// agent or script state, and dropped when one leaves any other state, so
/// that a human gate entered right after the step can show its reason and
/// the values its choice required. The action's own result is gone by then:
/// `pending_step` is cleared the moment the transition is taken.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastAgentStep {
    /// The state the transition left.
    pub state_id: StateId,

    /// The state the transition entered — the only one the record speaks to.
    pub to_state: StateId,

    /// The attempt that produced the answer.
    pub action_execution_id: String,

    /// Why the step made its choice, cleaned; the placeholder when it gave
    /// no reason. A record written before reasons were kept has none.
    #[serde(default)]
    pub reason: Option<String>,

    /// The values the choice required, cleaned and bounded.
    #[serde(default)]
    pub required_values: Vec<RequiredValue>,

    /// The end of the step's answer.
    #[serde(default)]
    pub answer_tail: Option<String>,
}

impl LastAgentStep {
    /// The record of `result`, produced in `state` by `action_execution_id`
    /// and followed by a transition into `to_state`; `None` when `state` is
    /// no agent or script step.
    pub(crate) fn from_result(
        state: &State,
        to_state: StateId,
        action_execution_id: String,
        result: &ActionResult,
        kept_locals: &Map<String, Value>,
    ) -> Option<Self> {
        if !has_step_contract(state) {
            return None;
        }
        let context = GateContext::of(state, result, kept_locals);
        Some(Self {
            state_id: state.id.clone(),
            to_state,
            action_execution_id,
            reason: context.reason,
            required_values: context.required_values,
            answer_tail: context.answer_tail,
        })
    }
}

/// Replaces the record of the agent step behind the latest transition, or
/// drops it when that transition did not leave an agent.
pub(crate) fn set_last_agent_step(
    execution: &mut WorkflowExecution,
    record: Option<&LastAgentStep>,
) -> Result<(), EngineError> {
    match record {
        Some(record) => {
            let value = serde_json::to_value(record).map_err(|error| {
                EngineError::CorruptEngineMetadata {
                    execution_id: execution.id.clone(),
                    message: error.to_string(),
                }
            })?;
            engine_metadata_mut(execution).insert(METADATA_LAST_AGENT_STEP_KEY.to_string(), value);
        }
        None => {
            if let Some(engine) = execution
                .metadata
                .get_mut(METADATA_ENGINE_KEY)
                .and_then(Value::as_object_mut)
            {
                engine.remove(METADATA_LAST_AGENT_STEP_KEY);
            }
        }
    }
    Ok(())
}

/// The agent or script step whose transition led straight into `state`, if
/// there is one.
///
/// Every transition rewrites or drops the record, so one naming `state` as
/// its destination describes the transition that entered it — never a
/// reason from an earlier pass through a loop. An unreadable record is
/// only lost context, not a reason to refuse the suspension.
pub(crate) fn preceding_agent_step(
    execution: &WorkflowExecution,
    state: &State,
) -> Option<LastAgentStep> {
    let value = execution
        .metadata
        .get(METADATA_ENGINE_KEY)?
        .get(METADATA_LAST_AGENT_STEP_KEY)?;
    let record: LastAgentStep = match serde_json::from_value(value.clone()) {
        Ok(record) => record,
        Err(error) => {
            tracing::warn!(
                execution_id = %execution.id,
                %error,
                "the record of the last agent step is unreadable; the input request goes without it"
            );
            return None;
        }
    };
    (record.to_state == state.id).then_some(record)
}
