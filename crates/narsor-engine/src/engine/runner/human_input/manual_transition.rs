//! Recording an operator's manual transition.

use super::*;

/// Records the step behind a manual transition from `from` into `to`, as
/// [`Engine`] does for every transition it takes itself (KTD8, KTD11).
///
/// A manual transition takes no decision, so the step's answer is wherever
/// the state left it: in `pending_step` when the step finished and waited
/// for a decision, or, for an agent, in `pending_input` when it asked a
/// question of its own (then the question is the reason, and there is no
/// answer to quote) or when its step is a problem (then there is an answer
/// but no choice, so no reason). Leaving any other state drops the record.
pub fn record_manual_transition(
    execution: &mut WorkflowExecution,
    from: &State,
    to: &str,
    pending_input: Option<&PendingHumanInput>,
) -> Result<(), EngineError> {
    let record = match stored_pending_step(execution, &from.id)? {
        Some(pending) => {
            let kept_locals = step_memory::step_locals(execution, &from.id)?;
            LastAgentStep::from_result(
                from,
                to.to_string(),
                pending.action_execution_id,
                &pending.result,
                &kept_locals,
            )
        }
        None if runs_harness(from) => pending_input
            .filter(|input| input.state_id == from.id)
            .and_then(|input| agent_input_record(from, to, input)),
        None => None,
    };
    // A manual transition abandons a rejected output still owed a reprompt,
    // a rescue the conductor had not finished, and a failure it diagnoses.
    clear_engine_metadata(execution, METADATA_OUTPUT_REJECTION_KEY);
    clear_engine_metadata(execution, METADATA_OUTPUT_RESCUE_KEY);
    clear_engine_metadata(execution, METADATA_STEP_FAILURE_KEY);
    clear_engine_metadata(execution, METADATA_CONDUCTOR_RETRY_KEY);
    set_last_agent_step(execution, record.as_ref())
}

/// The record of an agent step left by hand while it waited on `input`.
fn agent_input_record(from: &State, to: &str, input: &PendingHumanInput) -> Option<LastAgentStep> {
    let (reason, answer_tail) = match input.continuation {
        InputContinuation::ResumeAgent => {
            (Some(gate_context::gate_reason(input.instruction())), None)
        }
        InputContinuation::ResolveOutput => (None, input.request.answer_tail.clone()),
        InputContinuation::CompleteStep
        | InputContinuation::ApproveOutput
        | InputContinuation::ResolvePermission
        | InputContinuation::EntryLimit => return None,
    };
    Some(LastAgentStep {
        state_id: from.id.clone(),
        to_state: to.to_string(),
        action_execution_id: input.action_execution_id().to_string(),
        reason,
        required_values: Vec::new(),
        answer_tail,
    })
}
