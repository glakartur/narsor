//! What is done to an execution outside a pass: by an operator through the
//! API, or by the conductor carrying out a proposal the operator confirmed
//! (R28, U12).
//!
//! Every operation here takes the execution's claim for as long as it
//! writes (KTD10), so it can never race a pass, and — when it is given one —
//! checks the key of where the execution stood when the operation was asked
//! for against where it stands now, on the same read it then writes from
//! (KTD9): whoever acted first wins, and a late request is refused with
//! [`EngineError::EpisodeChanged`] instead of landing somewhere else.
//!
//! # Undoing a decision of the conductor's (KTD13)
//!
//! [`Engine::undo_output_decision`] takes back a chosen output (a): the
//! context from before it is restored, and the step it settled becomes the
//! problem it would have been without the conductor, at the same state and
//! with the same answers of the agent. The agent step is not run again. A
//! step after the decision that had already started is not undone either —
//! the operation says so in its warning.

use super::*;
use crate::domain::Context;

/// Where an execution stands, as a key (KTD9): what a proposal of the
/// conductor's names, so that a proposal about a moment that has passed is
/// refused rather than carried out on whatever came after it.
///
/// * waiting for input: the key of the pending input;
/// * failed on a step the conductor took up: the failure's key;
/// * otherwise: the last attempt, or — before any — the state.
pub fn execution_episode(store: &ExecutionStore, execution: &WorkflowExecution) -> Option<String> {
    if execution.status == ExecutionStatus::InputNeeded {
        if let Ok(Some(pending)) = store.load_pending_input::<PendingHumanInput>(&execution.id) {
            return Some(pending.episode_key().to_string());
        }
    }
    if let Ok(Some(failure)) = step_failure(execution) {
        return Some(failure.key().to_string());
    }
    execution
        .current_action_execution
        .clone()
        .or_else(|| execution.current_state.clone())
}

/// A manual or forced transition, as [`Engine::take_manual_transition`]
/// carries it out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManualTransition {
    /// The declared output label to take; required unless `force` is set.
    pub label: Option<String>,
    /// The state to force a transition into; required when `force` is set.
    pub target: Option<String>,
    /// Whether this is a forced override (D30).
    pub force: bool,
    /// The provider `DecisionMade` records.
    pub provider: String,
    /// The reasoning `DecisionMade` records.
    pub reason: String,
    /// Who closes a pending input the transition leaves behind.
    pub by: AnsweredBy,
    /// Where the execution must still stand (KTD9), if the transition was
    /// asked for at a moment that may have passed.
    pub episode: Option<String>,
}

/// A chosen output of the conductor's to undo (KTD13), as its saved
/// decision records it.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputDecisionUndo {
    /// The decision, as `ConductorDecision` named it.
    pub decision_id: String,
    /// The state whose step it settled.
    pub state_id: StateId,
    /// The context from before it.
    pub context_before: Value,
    /// The agent's answers without a label it settled.
    pub answers: Vec<RejectedAnswer>,
    /// Why the step had no label.
    pub reason: Option<String>,
    /// The choice being undone, shown on the problem as the conductor's
    /// reading of the answers.
    pub analysis: ConductorAnalysis,
}

/// What [`Engine::undo_output_decision`] left.
#[derive(Debug, Clone)]
pub struct UndoneOutputDecision {
    /// The execution, stopped on the problem raised again.
    pub execution: WorkflowExecution,
    /// The key of that problem's pending input.
    pub episode: String,
    /// What undoing could not take back, if anything.
    pub warning: Option<String>,
}

impl Engine {
    /// Stores `note` for the next agent attempt of `execution_id` — the
    /// operator note a recovery also carries — while no pass drives it.
    pub fn leave_operator_note(
        &self,
        execution_id: &str,
        note: &str,
        episode: Option<&str>,
    ) -> Result<WorkflowExecution, EngineError> {
        let _guard = self.begin_run(execution_id)?;
        let mut execution = self.store.load_execution(execution_id)?;
        self.check_episode(&execution, episode)?;
        engine_metadata_mut(&mut execution).insert(
            METADATA_OPERATOR_RECOVERY_NOTE_KEY.to_string(),
            Value::String(note.to_string()),
        );
        self.store.save_execution(&execution)?;
        Ok(execution)
    }

    /// Cancels `execution_id`: a pending input is closed, the execution is
    /// `Cancelled` with `WorkflowCancelled { reason }`. Refused while a pass
    /// drives it, and for an execution already completed or cancelled.
    pub fn cancel_execution(
        &self,
        execution_id: &str,
        reason: &str,
        by: AnsweredBy,
        episode: Option<&str>,
    ) -> Result<WorkflowExecution, EngineError> {
        let _guard = self.begin_run(execution_id)?;
        let mut execution = self.store.load_execution(execution_id)?;
        if matches!(
            execution.status,
            ExecutionStatus::Completed | ExecutionStatus::Cancelled
        ) {
            return Err(self.not_runnable(&execution, "cancelled"));
        }
        self.check_episode(&execution, episode)?;

        let from = self.store.event_log(execution_id).next_sequence()?;
        human::close_pending(&self.store, execution_id, InputCloseReason::Cancel, by)?;
        self.publish_from(execution_id, from)?;

        abandon_step_records(&mut execution);
        self.lock_pause_requests().remove(execution_id);
        execution.status = ExecutionStatus::Cancelled;
        execution.completed_at = Some(Utc::now());
        self.store.save_execution(&execution)?;
        self.emit(
            execution_id,
            EventData::WorkflowCancelled {
                reason: reason.to_string(),
            },
        )?;
        self.abandon_conductor_rescue(execution_id);
        Ok(execution)
    }

    /// Takes a manual or forced transition out of the current state of
    /// `execution_id` while no pass drives it: the declared transition of
    /// `label`, or — forced — straight into `target`.
    ///
    /// Into a terminal state the execution ends as that state declares;
    /// into any other it is `Paused` there, waiting to be resumed. A pending
    /// input is closed first, so a form left open on it can say what closed
    /// it (R5).
    pub fn take_manual_transition(
        &self,
        definition: &WorkflowDefinition,
        execution_id: &str,
        transition: &ManualTransition,
    ) -> Result<WorkflowExecution, EngineError> {
        let _guard = self.begin_run(execution_id)?;
        let mut execution = self.store.load_execution(execution_id)?;
        self.check_episode(&execution, transition.episode.as_deref())?;

        let from_state_id = execution
            .current_state
            .clone()
            .unwrap_or_else(|| definition.initial_state.clone());
        let from_state = self.state_of(definition, &execution, &from_state_id)?;
        let invalid = |message: String| EngineError::InvalidTransition { message };

        let (target, label) = if transition.force {
            let target = transition
                .target
                .clone()
                .ok_or_else(|| invalid("a forced transition requires a target state".into()))?;
            if !definition.states.contains_key(&target) {
                return Err(invalid(format!("the workflow has no state '{target}'")));
            }
            (target, OVERRIDE_LABEL.to_string())
        } else {
            let label = transition
                .label
                .clone()
                .ok_or_else(|| invalid("label is required unless force is set".into()))?;
            let target = from_state.transitions.get(&label).cloned().ok_or_else(|| {
                invalid(format!(
                    "state '{from_state_id}' has no transition for label '{label}' — this is \
                     not a legal transition from the current state"
                ))
            })?;
            (target, label)
        };
        let target_state = self.state_of(definition, &execution, &target)?;

        // What the agent in the state being left said, for a gate the target
        // may be (KTD8). Read before anything is written: the pending request
        // is about to be closed, and it only counts while input is needed.
        let pending_input: Option<PendingHumanInput> =
            if execution.status == ExecutionStatus::InputNeeded {
                self.store.load_pending_input(execution_id)?
            } else {
                None
            };
        record_manual_transition(&mut execution, from_state, &target, pending_input.as_ref())?;

        if let Some(pending) = &pending_input {
            self.emit(
                execution_id,
                human::closing_event(pending, InputCloseReason::Transition, transition.by),
            )?;
        }
        for data in [
            EventData::DecisionMade {
                provider: transition.provider.clone(),
                output_label: label.clone(),
                reasoning: Some(transition.reason.clone()),
            },
            EventData::HumanTransitionSelected {
                from_state: from_state_id.clone(),
                to_state: target.clone(),
            },
            EventData::StateExited {
                state_id: from_state_id.clone(),
                output_label: label.clone(),
            },
            EventData::TransitionTaken {
                from_state: from_state_id.clone(),
                to_state: target.clone(),
                output_label: label.clone(),
            },
        ] {
            self.emit(execution_id, data)?;
        }

        execution.current_state = Some(target.clone());
        execution.current_action_execution = None;
        self.clear_pending_step(&mut execution);

        let closing = if target_state.terminal {
            let status = terminal_status(target_state);
            execution.status = status.clone();
            execution.completed_at = Some(Utc::now());
            let closing = match status {
                ExecutionStatus::Failed => EventData::WorkflowFailed {
                    error: format!("Forced transition into terminal state '{target}'."),
                },
                ExecutionStatus::Cancelled => EventData::WorkflowCancelled {
                    reason: format!("Forced transition into terminal state '{target}'."),
                },
                _ => EventData::WorkflowCompleted {
                    result: Value::Object(Map::new()),
                },
            };
            self.emit(
                execution_id,
                EventData::StateEntered {
                    state_id: target.clone(),
                },
            )?;
            closing
        } else {
            execution.status = ExecutionStatus::Paused;
            EventData::WorkflowPaused {
                reason: format!("Manual transition into state '{target}'; waiting to be resumed."),
            }
        };
        self.emit(execution_id, closing)?;

        // A question asked in the state just left can no longer be answered.
        self.store.clear_pending_input(execution_id)?;
        self.store.save_execution(&execution)?;
        self.abandon_conductor_rescue(execution_id);
        Ok(execution)
    }

    /// Undoes a chosen output (a) of the conductor's (KTD13): restores the
    /// context from before it and raises the step it settled as a problem
    /// again, at the same state, with the same answers — without running
    /// the agent step again.
    ///
    /// Needs the claim: a pass still driving the execution has to be
    /// interrupted first ([`EngineError::AlreadyRunning`] otherwise). What a
    /// step after the decision already did stays done; the returned warning
    /// names it.
    pub fn undo_output_decision(
        &self,
        definition: &WorkflowDefinition,
        execution_id: &str,
        undo: &OutputDecisionUndo,
    ) -> Result<UndoneOutputDecision, EngineError> {
        let _guard = self.begin_run(execution_id)?;
        let mut execution = self.store.load_execution(execution_id)?;
        // A cancellation is the operator's explicit end of the execution;
        // an undo does not revive it.
        if execution.status == ExecutionStatus::Cancelled {
            return Err(self.not_runnable(&execution, "undone"));
        }
        let state = self.state_of(definition, &execution, &undo.state_id)?;
        let context: Context =
            serde_json::from_value(undo.context_before.clone()).map_err(|error| {
                EngineError::CorruptEngineMetadata {
                    execution_id: execution_id.to_string(),
                    message: format!("the context saved with the decision is unreadable: {error}"),
                }
            })?;
        let warning = self.undo_warning(execution_id, undo)?;

        let from = self.store.event_log(execution_id).next_sequence()?;
        human::close_pending(
            &self.store,
            execution_id,
            InputCloseReason::Undo,
            AnsweredBy::Operator,
        )?;
        self.publish_from(execution_id, from)?;

        abandon_step_records(&mut execution);
        self.clear_pending_step(&mut execution);
        clear_engine_metadata(&mut execution, METADATA_AGENT_INPUT_KEY);
        set_last_agent_step(&mut execution, None)?;
        self.lock_pause_requests().remove(execution_id);

        let moved = execution.current_state.as_deref() != Some(undo.state_id.as_str());
        execution.context = context;
        execution.completed_at = None;
        execution.current_state = Some(undo.state_id.clone());
        if let Some(last) = undo.answers.last() {
            execution.current_action_execution = Some(last.action_execution_id.clone());
        }

        self.emit(
            execution_id,
            EventData::ConductorDecisionUndone {
                decision_id: undo.decision_id.clone(),
                state_id: undo.state_id.clone(),
                warning: warning.clone(),
            },
        )?;
        if moved {
            self.emit(
                execution_id,
                EventData::StateEntered {
                    state_id: undo.state_id.clone(),
                },
            )?;
        }
        let reason = format!(
            "{} The conductor's choice of the output was undone by the operator.",
            undo.reason
                .as_deref()
                .unwrap_or("The agent did not give a usable output label.")
        );
        self.raise_output_problem(
            definition,
            &mut execution,
            state,
            undo.answers.clone(),
            reason,
            Some(undo.analysis.clone()),
        )?;

        self.abandon_conductor_rescue(execution_id);
        let episode = self
            .store
            .load_pending_input::<PendingHumanInput>(execution_id)?
            .map(|pending| pending.episode_key().to_string())
            .unwrap_or_default();
        Ok(UndoneOutputDecision {
            execution,
            episode,
            warning,
        })
    }

    /// What an undo of `undo` cannot take back: the steps started after the
    /// decision was recorded.
    fn undo_warning(
        &self,
        execution_id: &str,
        undo: &OutputDecisionUndo,
    ) -> Result<Option<String>, EngineError> {
        let events = self.store.event_log(execution_id).read_all()?;
        let Some(decided) = events.iter().position(|event| {
            matches!(
                &event.data,
                EventData::ConductorDecision { decision_id, .. } if *decision_id == undo.decision_id
            )
        }) else {
            return Ok(None);
        };
        let mut started: Vec<String> = Vec::new();
        for event in &events[decided..] {
            if let EventData::ActionStarted { state_id, .. } = &event.data {
                if !started.contains(state_id) {
                    started.push(state_id.clone());
                }
            }
        }
        Ok((!started.is_empty()).then(|| {
            format!(
                "The step{} started after the decision ({}) {} not undone: whatever {} changed \
                 stays as it is.",
                if started.len() == 1 { "" } else { "s" },
                started
                    .iter()
                    .map(|state| format!("'{state}'"))
                    .collect::<Vec<_>>()
                    .join(", "),
                if started.len() == 1 { "is" } else { "are" },
                if started.len() == 1 { "it" } else { "they" },
            )
        }))
    }

    /// Drops what an interrupted label rescue left in the conductor's store
    /// of `execution_id` — its marker, a queued request, unapplied
    /// decisions — once the step it was about has been abandoned
    /// ([`abandon_step_records`]): without it, the conductor would go on
    /// showing activity for an execution that no longer owes a rescue.
    ///
    /// The execution's own record is already written; a failure here only
    /// leaves a stale marker the startup hook discards, so it is logged
    /// rather than returned.
    fn abandon_conductor_rescue(&self, execution_id: &str) {
        let Some(conductor) = self.conductor() else {
            return;
        };
        if !conductor.store(execution_id).exists() {
            return;
        }
        if let Err(error) = conductor.discard_rescue(execution_id) {
            tracing::warn!(
                execution_id,
                %error,
                "conductor: could not discard an abandoned label rescue"
            );
        }
    }

    /// Refuses a request made at `expected` once the execution has moved on
    /// from there (KTD9).
    fn check_episode(
        &self,
        execution: &WorkflowExecution,
        expected: Option<&str>,
    ) -> Result<(), EngineError> {
        let Some(expected) = expected else {
            return Ok(());
        };
        let current = execution_episode(&self.store, execution);
        if current.as_deref() == Some(expected) {
            return Ok(());
        }
        Err(EngineError::EpisodeChanged {
            execution_id: execution.id.clone(),
            expected: expected.to_string(),
            current,
        })
    }
}

/// The label recorded for a forced transition, which by definition has none.
const OVERRIDE_LABEL: &str = "override";

/// Drops what a step under way left for a pass to carry on: a rejected
/// output owed a reprompt, a rescue, a failure the conductor diagnoses, its
/// retry.
fn abandon_step_records(execution: &mut WorkflowExecution) {
    clear_engine_metadata(execution, METADATA_OUTPUT_REJECTION_KEY);
    clear_engine_metadata(execution, METADATA_OUTPUT_RESCUE_KEY);
    clear_engine_metadata(execution, METADATA_STEP_FAILURE_KEY);
    clear_engine_metadata(execution, METADATA_CONDUCTOR_RETRY_KEY);
}
