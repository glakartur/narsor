//! A `decision` step declared `by: conductor`.
//!
//! The conductor always decides such a step, whatever `autopilot` says, and
//! inline, under the claim of the pass that waits for it, like the rescue of
//! an output ([`super::rescue`]). A choice it is sure of — a label the state
//! declares, with every value the label requires — is saved and applied as
//! the `conductor` provider's decision, so `set` and an undo work as for any
//! other. Anything else hands the step to the operator, with the
//! conductor's proposal prefilled in the form.

use super::rescue::{check_choice, chosen_output_decision, unsaved_decision, Choice, Settled};
use super::*;
use crate::conductor::{
    ConductorResponse, Decision, InvocationOutcome, QueuedRequest, SavedDecision,
};
use crate::domain::{ConductorDecisionKind, ConductorPurpose, ConductorTrigger};

impl Engine {
    /// Asks the conductor to decide the step `state` and carries out its
    /// answer. `message` is what the step asks, as rendered.
    ///
    /// Without a conductor attached, the operator decides.
    pub(super) async fn decide_by_conductor(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
        action_execution_id: &str,
        message: String,
    ) -> Result<Flow, EngineError> {
        let Some(conductor) = self.conductor() else {
            return self.hand_decision_to_operator(
                definition,
                execution,
                state,
                action_execution_id,
                "no conductor is attached".to_string(),
                None,
            );
        };

        let request =
            QueuedRequest::new(ConductorTrigger::DecisionStep, ConductorPurpose::DecideFork)
                .with_episode(action_execution_id)
                .with_payload(json!({
                    "state_id": state.id,
                    "message": message,
                    "outputs": state.choices,
                    "requirements": output_requirements(definition, execution, state),
                }));
        let answered = match conductor.choose_output(&execution.id, request).await? {
            InvocationOutcome::Answered(answer) => {
                ConductorResponse::from_value(answer.response, ConductorPurpose::DecideFork)
                    .map_err(|error| error.to_string())
            }
            InvocationOutcome::Failed { error } => Err(error),
            InvocationOutcome::Interrupted => {
                Err("the conductor was stopped before it answered".to_string())
            }
        };
        let response = match answered {
            Ok(response) => response,
            Err(error) => {
                return self.hand_decision_to_operator(
                    definition,
                    execution,
                    state,
                    action_execution_id,
                    format!("the conductor could not settle it: {error}"),
                    None,
                );
            }
        };

        let (choice, proposed_only) = match response.decision.clone() {
            Decision::ChooseOutput {
                label,
                artifacts,
                variables,
            } => (
                Choice {
                    label,
                    artifacts,
                    variables,
                },
                false,
            ),
            Decision::Escalate {
                label,
                artifacts,
                variables,
            } => (
                Choice {
                    label: label.unwrap_or_default(),
                    artifacts,
                    variables,
                },
                true,
            ),
            // Refused by the purpose's check before it gets here.
            other => {
                return self.hand_decision_to_operator(
                    definition,
                    execution,
                    state,
                    action_execution_id,
                    format!(
                        "the conductor answered with a decision a fork does not take: {}",
                        other.kind().name()
                    ),
                    None,
                );
            }
        };

        let checked = if proposed_only {
            Err(response
                .rationale
                .clone()
                .unwrap_or_else(|| "it gave no reason".to_string()))
        } else {
            check_choice(definition, execution, state, &response, &choice)
        };
        match checked {
            Ok(()) => {
                let decision = SavedDecision {
                    reason: Some(format!(
                        "State '{}' is a decision the conductor settled.",
                        state.id
                    )),
                    ..chosen_output_decision(
                        execution,
                        state,
                        action_execution_id,
                        &response,
                        choice,
                    )?
                };
                conductor.save_decision(&execution.id, &decision, response.message.as_deref())?;
                self.apply_conductor_output(
                    definition,
                    execution,
                    state,
                    Settled {
                        episode: action_execution_id,
                        stdout: String::new(),
                        reply: None,
                    },
                    decision,
                )
            }
            Err(why) => {
                let declared = decision::check_label(state, &choice.label).is_ok();
                let proposal = Choice {
                    label: if declared {
                        choice.label
                    } else {
                        String::new()
                    },
                    ..choice
                };
                self.escalate_decision(
                    definition,
                    execution,
                    state,
                    action_execution_id,
                    &response,
                    proposal,
                    why,
                )
            }
        }
    }

    /// Records that the conductor hands the decision to the operator, with
    /// what it would have chosen, and raises the problem.
    #[allow(clippy::too_many_arguments)]
    fn escalate_decision(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
        action_execution_id: &str,
        response: &ConductorResponse,
        proposal: Choice,
        why: String,
    ) -> Result<Flow, EngineError> {
        let rationale = match &response.rationale {
            Some(rationale) if *rationale != why => format!("{why}. {rationale}"),
            _ => why.clone(),
        };
        let analysis = ConductorAnalysis {
            label: (!proposal.label.is_empty()).then(|| proposal.label.clone()),
            artifacts: proposal.artifacts.clone(),
            variables: proposal.variables.clone(),
            rationale: Some(rationale.clone()),
        };
        if let Some(conductor) = self.conductor() {
            let decision = unsaved_decision(
                ConductorDecisionKind::Escalate,
                state,
                action_execution_id,
                response,
                proposal,
                Some(rationale),
            );
            self.note_decision(&conductor, execution, &decision, response);
        }
        self.hand_decision_to_operator(
            definition,
            execution,
            state,
            action_execution_id,
            why,
            Some(analysis),
        )
    }

    /// Makes the decision a problem for the operator: the form of the
    /// step's labels, prefilled with the conductor's `analysis`, if it gave
    /// one.
    fn hand_decision_to_operator(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
        action_execution_id: &str,
        why: String,
        analysis: Option<ConductorAnalysis>,
    ) -> Result<Flow, EngineError> {
        execution.current_action_execution = Some(action_execution_id.to_string());
        let reason = format!(
            "The decision of state '{}' is the conductor's, which handed it to the operator: {why}",
            state.id
        );
        self.raise_output_problem(definition, execution, state, Vec::new(), reason, analysis)?;
        Ok(Flow::Stop)
    }
}
