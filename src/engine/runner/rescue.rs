//! The conductor's rescue of a step without a usable output label (F1, U10).
//!
//! When the agent of a step gives no label the state accepts, even after the
//! engine's one reprompt — or cannot be asked again because its harness
//! reported no session — the engine hands the step to the conductor, inline,
//! under the claim of the pass that runs it (KTD10): an operator's `/input`
//! or `/transition` is refused meanwhile, and interrupting the pass stops the
//! conductor with it. The conductor answers with one of three paths (R17):
//!
//! * **(a) choose the output**: the step completes with the conductor's
//!   label, `data` and `variables`, as if the agent had given them —
//!   `ActionCompleted` on the last attempt, then `DecisionMade` with the
//!   `conductor` provider. Only a sure choice (`high` confidence) of a label
//!   the state declares, with every value the next step needs (KTD15), is
//!   taken; any other becomes (c), with the reason in the rationale. The
//!   decision is saved before it is applied, with the context from before it
//!   (KTD13; see [`crate::conductor::rescue`]).
//! * **(b) ask the agent again**, once, in the conductor's words: a new
//!   attempt announced by `OutputReprompted { by: conductor }`. When it too
//!   gives no label, the conductor is asked once more, and may then only
//!   choose (a) or (c). At most two reprompts in all: the engine's and the
//!   conductor's.
//! * **(c) escalate**: the step becomes a problem (`InputNeeded` on a
//!   `ResolveOutput` request, `ProblemRaised`) carrying the agent's answers
//!   and the conductor's analysis.
//!
//! A conductor that cannot answer — after the one repeat R24 allows — makes
//! the step a problem with the agent's raw answers and no analysis, and
//! never triggers the conductor again (R25).
//!
//! # What survives a restart
//!
//! The rescue is recorded in the execution ([`OutputRescue`]) before the
//! event that closes the last attempt, so a restart at any point from there
//! finds the execution `Running` with nothing open, and the startup hook
//! spawns a pass that carries the rescue on ([`Engine::continue_output_rescue`]):
//!
//! * before the conductor was invoked, or while it was — the conductor is
//!   invoked (again); a restart during an invocation spends R24's one repeat;
//! * after a chosen output was saved — the saved decision is applied without
//!   asking again;
//! * during the conductor's reprompt — the attempt is open, the execution is
//!   `Unknown` like after any interrupted attempt, and a recovery drops the
//!   record.

use super::*;
use crate::conductor::{
    ConductorResponse, Decision, InvocationOutcome, QueuedRequest, SavedDecision,
};
use crate::domain::{ConductorDecisionKind, ConductorPurpose, ConductorTrigger, Confidence};

impl Engine {
    /// Carries on the rescue a pass found recorded for the current state (a
    /// restart, an interrupted pass).
    pub(super) async fn continue_output_rescue(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
        rescue: OutputRescue,
    ) -> Result<Flow, EngineError> {
        let timeout = state
            .action
            .as_ref()
            .and_then(|action| self.timeout_for(ActionKind::of(action), state, definition))
            .map(Duration::from_secs);
        self.rescue_output(definition, execution, state, rescue, timeout)
            .await
    }

    /// Hands the step `rescue` records to the conductor and carries out what
    /// it decides (see the module documentation).
    ///
    /// `rescue` is already persisted in the execution. Without a conductor
    /// attached, the step becomes a problem straight away.
    pub(super) async fn rescue_output(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
        mut rescue: OutputRescue,
        timeout: Option<Duration>,
    ) -> Result<Flow, EngineError> {
        loop {
            let conductor = self.conductor();

            if let Some(conductor) = &conductor {
                // Decided before a restart: applied, never asked again.
                if let Some(decision) =
                    conductor.unapplied_decision(&execution.id, rescue.episode())?
                {
                    return self
                        .apply_conductor_output(definition, execution, state, &rescue, decision);
                }
            }
            if let Some(pending) = self.pending_step(execution, &state.id)? {
                // The step already has its result: only the decision is left.
                return self.complete_step(definition, execution, state, pending);
            }
            let Some(conductor) = conductor else {
                // No conductor to hand the step to: the operator settles it.
                self.raise_output_problem(
                    definition,
                    execution,
                    state,
                    rescue.answers,
                    rescue.reason,
                    None,
                )?;
                return Ok(Flow::Stop);
            };

            let purpose = if rescue.can_reprompt() {
                ConductorPurpose::ChooseOutput
            } else {
                ConductorPurpose::ChooseOutputAfterReprompt
            };
            let request = QueuedRequest::new(ConductorTrigger::OutputRescue, purpose)
                .with_episode(rescue.episode())
                .with_payload(rescue_payload(definition, execution, state, &rescue));
            let outcome = conductor.choose_output(&execution.id, request).await?;

            let answered = match outcome {
                InvocationOutcome::Answered(answer) => {
                    ConductorResponse::from_value(answer.response, purpose)
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
                    // R24's repeat is already spent by the actor: the
                    // operator gets the raw answers, and a failure of the
                    // conductor never asks the conductor again (R25).
                    let reason = format!(
                        "{} The conductor could not settle the output: {error}",
                        rescue.reason
                    );
                    self.raise_output_problem(
                        definition,
                        execution,
                        state,
                        rescue.answers,
                        reason,
                        None,
                    )?;
                    return Ok(Flow::Stop);
                }
            };

            match response.decision.clone() {
                Decision::ChooseOutput {
                    label,
                    data,
                    variables,
                } => {
                    let choice = Choice {
                        label,
                        data: object_of(data),
                        variables,
                    };
                    return match check_choice(definition, execution, state, &response, &choice) {
                        Ok(()) => {
                            let decision = SavedDecision {
                                decision_id: Uuid::new_v4().to_string(),
                                kind: ConductorDecisionKind::ChooseOutput,
                                episode: Some(rescue.episode().to_string()),
                                state_id: Some(state.id.clone()),
                                output_label: Some(choice.label),
                                data: Value::Object(choice.data),
                                variables: Value::Object(choice.variables),
                                confidence: response.confidence,
                                rationale: response.rationale.clone(),
                                context_before: serde_json::to_value(&execution.context)
                                    .unwrap_or(Value::Null),
                                answers: rescue.answers.clone(),
                                reason: Some(rescue.reason.clone()),
                                saved_at: Utc::now(),
                            };
                            conductor.save_decision(
                                &execution.id,
                                &decision,
                                response.message.as_deref(),
                            )?;
                            self.apply_conductor_output(
                                definition, execution, state, &rescue, decision,
                            )
                        }
                        Err(why) => {
                            let declared = decision::check_label(state, &choice.label).is_ok();
                            self.escalate_output(
                                definition,
                                execution,
                                state,
                                rescue,
                                &response,
                                Choice {
                                    label: if declared {
                                        choice.label
                                    } else {
                                        String::new()
                                    },
                                    ..choice
                                },
                                why,
                            )
                        }
                    };
                }
                Decision::Reprompt { prompt } => {
                    let config = match (&state.action, &rescue.harness, &rescue.harness_session_id)
                    {
                        (Some(ActionConfig::Agent(config)), Some(harness), Some(session))
                            if rescue.can_reprompt() =>
                        {
                            agent::conductor_reprompt_config(config, harness, session, &prompt)
                        }
                        // The purpose did not offer (b): a response that
                        // takes it anyway was refused before it got here.
                        _ => {
                            return self.escalate_output(
                                definition,
                                execution,
                                state,
                                rescue,
                                &response,
                                Choice::default(),
                                "the conductor asked to repeat the question, but the agent \
                                 cannot be asked again"
                                    .to_string(),
                            );
                        }
                    };

                    let decision = unsaved_decision(
                        ConductorDecisionKind::Reprompt,
                        state,
                        &rescue,
                        &response,
                        Choice::default(),
                        response.rationale.clone(),
                    );
                    self.note_decision(&conductor, execution, &decision, &response);

                    // Written by the attempt's own save, after the event that
                    // opens it: a restart before that asks the conductor
                    // again, one during it leaves the attempt open.
                    rescue.conductor_reprompted = true;
                    set_output_rescue(execution, &rescue)?;
                    let previous = rescue.episode().to_string();
                    match self
                        .reprompt_attempt(
                            execution,
                            state,
                            config,
                            &previous,
                            RepromptedBy::Conductor,
                            timeout,
                        )
                        .await?
                    {
                        RepromptEnd::Labelled {
                            action_execution_id,
                            result,
                        } => {
                            // The agent gave its label after all: the step
                            // completes as it would have, and the rescue is
                            // over — dropped by the result's own save.
                            clear_engine_metadata(execution, METADATA_OUTPUT_RESCUE_KEY);
                            return self.record_completion(
                                definition,
                                execution,
                                state,
                                action_execution_id,
                                result,
                            );
                        }
                        RepromptEnd::Stopped => return Ok(Flow::Stop),
                        RepromptEnd::Unlabelled(failure) => {
                            rescue.record(&failure);
                            self.begin_rescue(execution, &rescue)?;
                            self.close_unlabelled(execution, state, &failure.answer)?;
                            // Asked once more, without (b) this time.
                            continue;
                        }
                    }
                }
                Decision::Escalate {
                    label,
                    data,
                    variables,
                } => {
                    let why = response
                        .rationale
                        .clone()
                        .unwrap_or_else(|| "it gave no reason".to_string());
                    let choice = Choice {
                        label: label
                            .filter(|label| decision::check_label(state, label).is_ok())
                            .unwrap_or_default(),
                        data: object_of(data),
                        variables,
                    };
                    return self.escalate_output(
                        definition, execution, state, rescue, &response, choice, why,
                    );
                }
                other => {
                    // Refused by the purpose's check before it gets here.
                    let reason = format!(
                        "{} The conductor answered with a decision a label rescue does not \
                         take: {}",
                        rescue.reason,
                        other.kind().name()
                    );
                    self.raise_output_problem(
                        definition,
                        execution,
                        state,
                        rescue.answers,
                        reason,
                        None,
                    )?;
                    return Ok(Flow::Stop);
                }
            }
        }
    }

    /// Applies the conductor's saved choice (a): `ConductorDecision`, then
    /// the step's result on its last attempt (`ActionCompleted`) and the
    /// decision with the `conductor` provider — which takes the transition
    /// and drops the rescue — and last the saved decision.
    ///
    /// A result already recorded before a restart is not recorded again.
    fn apply_conductor_output(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
        rescue: &OutputRescue,
        decision: SavedDecision,
    ) -> Result<Flow, EngineError> {
        let label = decision.output_label.clone().unwrap_or_default();
        let outcome = DecisionOutcome {
            label: label.clone(),
            provider: DecisionProviderType::Conductor,
            reasoning: decision.rationale.clone(),
        };

        let flow = match self.pending_step(execution, &state.id)? {
            Some(pending) => self.apply_decision(definition, execution, state, pending, outcome)?,
            None => {
                self.emit(&execution.id, decision_event(&decision))?;
                let result = ActionResult {
                    output_label: label,
                    data: match &decision.data {
                        Value::Object(data) => Value::Object(data.clone()),
                        _ => json!({}),
                    },
                    variables: match &decision.variables {
                        Value::Object(variables) => variables.clone(),
                        _ => Map::new(),
                    },
                    exit_code: None,
                    // What the agent last said: a gate the label leads to
                    // shows it (KTD8).
                    stdout: rescue
                        .answers
                        .iter()
                        .rev()
                        .find_map(|answer| answer.answer_tail.clone())
                        .unwrap_or_default(),
                    stderr: String::new(),
                };
                match self.record_result(execution, state, rescue.episode().to_string(), result)? {
                    Some(pending) => {
                        self.apply_decision(definition, execution, state, pending, outcome)?
                    }
                    None => Flow::Stop,
                }
            }
        };

        if let Some(conductor) = self.conductor() {
            if let Err(error) = conductor.decision_applied(&execution.id, &decision.decision_id) {
                // Harmless: the startup hook drops a decision for a step that
                // moved on.
                tracing::warn!(
                    execution_id = %execution.id,
                    %error,
                    "could not mark the conductor's decision as applied"
                );
            }
        }
        Ok(flow)
    }

    /// (c): records the escalation and makes the step a problem, with the
    /// conductor's reading of it as the analysis the form prefills.
    #[allow(clippy::too_many_arguments)]
    fn escalate_output(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
        rescue: OutputRescue,
        response: &ConductorResponse,
        choice: Choice,
        why: String,
    ) -> Result<Flow, EngineError> {
        let rationale = match &response.rationale {
            Some(rationale) if *rationale != why => format!("{why}. {rationale}"),
            _ => why.clone(),
        };
        let analysis = ConductorAnalysis {
            label: (!choice.label.is_empty()).then(|| choice.label.clone()),
            data: choice.data.clone(),
            variables: choice.variables.clone(),
            rationale: Some(rationale.clone()),
        };
        if let Some(conductor) = self.conductor() {
            let decision = unsaved_decision(
                ConductorDecisionKind::Escalate,
                state,
                &rescue,
                response,
                choice,
                Some(rationale),
            );
            self.note_decision(&conductor, execution, &decision, response);
        }

        let reason = format!(
            "{} The conductor handed the step to the operator: {why}",
            rescue.reason
        );
        self.raise_output_problem(
            definition,
            execution,
            state,
            rescue.answers,
            reason,
            Some(analysis),
        )?;
        Ok(Flow::Stop)
    }

    /// Records a decision carried out right away ((b), (c)): in the chat and
    /// as `ConductorDecision`. Best-effort — the decision is carried out
    /// whether or not its record could be written.
    fn note_decision(
        &self,
        conductor: &ConductorRegistry,
        execution: &WorkflowExecution,
        decision: &SavedDecision,
        response: &ConductorResponse,
    ) {
        if let Err(error) =
            conductor.note_decision(&execution.id, decision, response.message.as_deref())
        {
            tracing::warn!(
                execution_id = %execution.id,
                %error,
                "could not write the conductor's decision to its chat"
            );
        }
        if let Err(error) = self.emit(&execution.id, decision_event(decision)) {
            tracing::warn!(
                execution_id = %execution.id,
                %error,
                "could not record the conductor's decision"
            );
        }
    }
}

/// A label and the values that go with it, as the conductor gave them.
#[derive(Debug, Default)]
struct Choice {
    /// The label; empty for none.
    label: String,
    /// The step's `data`.
    data: Map<String, Value>,
    /// The step's `variables`.
    variables: Map<String, Value>,
}

/// Whether the conductor's choice (a) can be taken as it is (R18, KTD15):
/// a sure choice of a label the state declares, with every value the state
/// it leads to needs. The reason it cannot, otherwise.
fn check_choice(
    definition: &WorkflowDefinition,
    execution: &WorkflowExecution,
    state: &State,
    response: &ConductorResponse,
    choice: &Choice,
) -> Result<(), String> {
    let label = &choice.label;
    if decision::check_label(state, label).is_err() {
        return Err(format!(
            "the conductor chose the output '{label}', which state '{}' does not declare",
            state.id
        ));
    }
    if response.confidence != Confidence::High {
        return Err(format!(
            "the conductor chose the output '{label}' without being sure of it (low confidence)"
        ));
    }

    let requirements = output_requirements(definition, execution, state);
    let Some(requirement) = requirements
        .iter()
        .find(|requirement| requirement.label == *label)
    else {
        return Ok(());
    };
    let held = |map: &Map<String, Value>, key: &str| {
        map.get(key).is_some_and(|value| !human::is_blank(value))
    };
    if let Some(key) = requirement.data.iter().find(|key| !held(&choice.data, key)) {
        return Err(format!(
            "the conductor chose the output '{label}' without data.{key}, which the next step \
             needs"
        ));
    }
    if let Some(key) = requirement.variables.iter().find(|key| {
        !held(&choice.variables, key)
            && !execution
                .context
                .variables
                .get(key.as_str())
                .is_some_and(|value| !human::is_blank(value))
    }) {
        return Err(format!(
            "the conductor chose the output '{label}' without variables.{key}, which the next \
             step needs"
        ));
    }
    Ok(())
}

/// The record of a decision that is carried out rather than saved.
fn unsaved_decision(
    kind: ConductorDecisionKind,
    state: &State,
    rescue: &OutputRescue,
    response: &ConductorResponse,
    choice: Choice,
    rationale: Option<String>,
) -> SavedDecision {
    SavedDecision {
        decision_id: Uuid::new_v4().to_string(),
        kind,
        episode: Some(rescue.episode().to_string()),
        state_id: Some(state.id.clone()),
        output_label: (!choice.label.is_empty()).then_some(choice.label),
        data: Value::Object(choice.data),
        variables: Value::Object(choice.variables),
        confidence: response.confidence,
        rationale,
        context_before: Value::Null,
        answers: Vec::new(),
        reason: None,
        saved_at: Utc::now(),
    }
}

/// The `ConductorDecision` event of `decision`.
fn decision_event(decision: &SavedDecision) -> EventData {
    EventData::ConductorDecision {
        decision_id: decision.decision_id.clone(),
        kind: decision.kind,
        state_id: decision.state_id.clone(),
        output_label: decision.output_label.clone(),
        episode: decision.episode.clone(),
        confidence: decision.confidence,
        rationale: decision.rationale.clone(),
    }
}

/// What the conductor is told about the step it rescues, beside its
/// snapshot: the agent's answers, the labels with what each one needs, and
/// whether the agent can still be asked again.
fn rescue_payload(
    definition: &WorkflowDefinition,
    execution: &WorkflowExecution,
    state: &State,
    rescue: &OutputRescue,
) -> Value {
    json!({
        "state_id": state.id,
        "reason": rescue.reason,
        "answers": rescue.answers,
        "outputs": state.outputs,
        "requirements": output_requirements(definition, execution, state),
        "agent_can_be_asked_again": rescue.can_reprompt(),
    })
}

/// `value` as an object; anything else is no values at all.
fn object_of(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// Writes `rescue` into the execution's record, without saving it.
fn set_output_rescue(
    execution: &mut WorkflowExecution,
    rescue: &OutputRescue,
) -> Result<(), EngineError> {
    let value =
        serde_json::to_value(rescue).map_err(|error| EngineError::CorruptEngineMetadata {
            execution_id: execution.id.clone(),
            message: error.to_string(),
        })?;
    engine_metadata_mut(execution).insert(METADATA_OUTPUT_RESCUE_KEY.to_string(), value);
    Ok(())
}
