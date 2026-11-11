//! An output nobody repaired, made a problem for the operator.

use super::*;

/// What the operator is asked when the step `state` has no output label of
/// its own to go on with.
fn output_problem_instruction(state: &State) -> String {
    match state.action {
        Some(ActionConfig::Decision(_)) => format!(
            "The conductor did not settle the decision of state '{}'. Choose the label its step \
             ends with, and the variables it needs.",
            state.id
        ),
        Some(ActionConfig::Profile(_)) => format!(
            "The profiling agent of state '{}' did not give a usable step profile. Choose \
             the step's exit to go on without a new profile.",
            state.id
        ),
        _ => format!(
            "The agent of state '{}' did not give a usable output label. Choose the \
             label its step ends with, and the data and variables it would have given.",
            state.id
        ),
    }
}

/// What choosing each label of `state` requires of an answer to a problem
/// (KTD15, R18).
///
/// A label requires the variables and artifacts its choice names, and every
/// `context.NAME` the templates of the state it leads to use that no step or
/// gate declares — a declared one renders as empty text (KTD6). A variable
/// the context, or for a `local.*` the step's kept memory (KTD10), already
/// holds is required of no one.
pub(crate) fn output_requirements(
    definition: &WorkflowDefinition,
    execution: &WorkflowExecution,
    state: &State,
) -> Vec<OutputRequirement> {
    let declared = DeclaredNames::of(definition);
    // Unreadable memory only costs the leniency of KTD10: the operator is
    // asked for a value the step may already keep.
    let kept_locals = step_memory::step_locals(execution, &state.id).unwrap_or_default();
    let held = |name: &String| {
        execution
            .context
            .variables
            .get(name)
            .or_else(|| kept_locals.get(name))
            .is_some_and(|value| !human::is_blank(value))
    };
    state
        .choices
        .iter()
        .map(|choice| {
            let target_action = state
                .transitions
                .get(&choice.label)
                .and_then(|target| definition.states.get(target))
                .and_then(|target| target.action.as_ref());
            let undeclared_references = target_action
                .map(action_context_references)
                .unwrap_or_default()
                .into_iter()
                .filter(|name| !declared.declares_variable(name));
            let mut variables: Vec<String> = choice
                .requires_variables
                .iter()
                .cloned()
                .chain(undeclared_references)
                .filter(|name| !held(name))
                .collect();
            variables.sort();
            variables.dedup();
            OutputRequirement {
                label: choice.label.clone(),
                variables,
                artifacts: choice.requires_artifacts.clone(),
            }
        })
        .collect()
}

impl Engine {
    /// Suspends a step whose agent gave no usable output label as a problem
    /// (KTD3): `InputNeeded` on a [`InputContinuation::ResolveOutput`]
    /// request carrying the agent's `answers`, then `ProblemRaised`.
    ///
    /// The last answer's attempt is the one the request suspends on; an
    /// answer to the request completes the step under that attempt.
    /// `analysis` is the conductor's reading of the answers, when it made
    /// one (U10).
    pub(crate) fn raise_output_problem(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
        answers: Vec<RejectedAnswer>,
        reason: String,
        analysis: Option<ConductorAnalysis>,
    ) -> Result<(), EngineError> {
        // Keep the historical harness/session on the durable input record.
        // `OutputRescue` is about to be cleared, but the operator may choose
        // conversation rather than the manual output that originally ended it.
        let rescue = stored_output_rescue(execution, &state.id)?;
        let reply = rescue.as_ref().and_then(|rescue| rescue.reply.clone());
        let session = rescue.map(|rescue| (rescue.harness, rescue.harness_session_id));
        clear_engine_metadata(execution, METADATA_OUTPUT_REJECTION_KEY);
        clear_engine_metadata(execution, METADATA_OUTPUT_RESCUE_KEY);

        let action_execution_id = answers
            .last()
            .map(|answer| answer.action_execution_id.clone())
            .or_else(|| execution.current_action_execution.clone())
            .unwrap_or_default();
        let answer_tail = answers
            .iter()
            .rev()
            .find_map(|answer| answer.answer_tail.clone());
        let problem = OutputProblem {
            reason: reason.clone(),
            answers,
            requirements: output_requirements(definition, execution, state),
            analysis,
            reply,
        };
        let request = HumanInputRequest {
            action_execution_id: action_execution_id.clone(),
            instruction: output_problem_instruction(state),
            fields: Vec::new(),
            reason: None,
            required_values: Vec::new(),
            answer_tail,
            agent_harness: session.as_ref().and_then(|(harness, _)| harness.clone()),
            agent_session_id: session.and_then(|(_, session)| session),
            prefilled_answer: None,
            permission: None,
        };
        let mut pending = PendingHumanInput::for_problem(
            execution.id.clone(),
            state.id.clone(),
            request,
            decision::permitted_outputs(state),
            problem,
        );
        if matches!(state.action, Some(ActionConfig::Decision(_))) {
            let kept_locals = step_memory::step_locals(execution, &state.id)?;
            pending.local_values = local_values(state, &kept_locals);
        }

        let from = self.store.event_log(&execution.id).next_sequence()?;
        human::suspend(&self.store, execution, &pending)?;
        self.publish_from(&execution.id, from)?;

        self.emit(
            &execution.id,
            EventData::ProblemRaised {
                state_id: state.id.clone(),
                action_execution_id,
                kind: ProblemKind::Output,
                reason,
                episode: Some(pending.episode_key().to_string()),
            },
        )?;
        Ok(())
    }
}
