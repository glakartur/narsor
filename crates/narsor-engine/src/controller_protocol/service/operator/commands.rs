//! Operator commands (interrupt, transition, restart, conductor chat) for a
//! running task.

use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::ProtocolService;
use crate::controller_protocol::wire::*;
use crate::domain::LaunchDefinitionError;
use crate::engine::{plan_operator_transition, EngineError};
use crate::orchestration::{
    CommandActor, CommandRequest, OperatorCommand, OperatorCommandRequest, OwnerRef, RunnerState,
    TaskRecord, RETIRED_CONDUCTOR_HARNESS,
};

impl ProtocolService {
    /// Queues an operator command (interrupt, transition, restart) for the
    /// runner of a running task.
    ///
    /// Refused at once, with the reason and nothing written, when the task
    /// is not running, its runner is gone or silent, the generation moved
    /// on, or the command can be seen to be illegal from the runner's last
    /// snapshot (an override without a reason or out of a state that does
    /// not allow one, a target no declared transition leads to).
    /// The runner checks the same again under the Engine's own claim, on
    /// what it then finds: this early refusal only spares the owner a
    /// round trip.
    pub fn submit_operator_command(
        &self,
        owner: &OwnerRef,
        task_id: &str,
        request: SubmitOperatorCommandRequest,
    ) -> ProtocolResult<SubmitOperatorCommandResponse> {
        let record = self.owned_task(owner, task_id)?;
        if request.idempotency_key.trim().is_empty() {
            return Err(ProtocolError::bad_request(
                "missing_idempotency_key",
                "an operator command needs an idempotency key",
            ));
        }
        let is_retry = record
            .operator_commands
            .iter()
            .any(|queued| queued.idempotency_key == request.idempotency_key);
        if !is_retry {
            self.check_operator_command(&record, &request.command)?;
            if let Some(action) = operator_action(&request.command) {
                self.require_available(&record, action, request.expected_generation)?;
            }
        }
        // A stop is a pause first, then the cut: in this order, so that a
        // runner lost while cutting counts as lost from `PauseRequested`
        // (nothing resumes it by itself), not as a lost task.
        let mut expected_generation = request.expected_generation;
        let mut paused_for_stop = false;
        if matches!(request.command, OperatorCommand::StopStep)
            && record.runner_state == RunnerState::Running
        {
            self.control.command_with_capacity_from(
                &record.task_id,
                CommandRequest {
                    command: crate::orchestration::OwnerCommand::Pause,
                    idempotency_key: format!("stop-pause:{}", request.idempotency_key),
                    expected_generation,
                    actor: CommandActor::Owner,
                },
                || self.scheduling_policy(&record.assignment.backend_id),
            )?;
            self.notify(&record.task_id);
            // The pause moved the generation the owner saw.
            expected_generation = None;
            paused_for_stop = true;
        }
        let queued = self.control.queue_operator_command(
            &record.task_id,
            OperatorCommandRequest {
                command: request.command.clone(),
                idempotency_key: request.idempotency_key,
                expected_generation,
                requested_by: owner.clone(),
            },
        );
        let outcome = match queued {
            Ok(outcome) => outcome,
            // The runner reached its safe point between the two writes: the
            // task is stopped, which is what the owner asked for.
            Err(crate::orchestration::OrchestrationError::OperatorCommandRefused { .. })
                if paused_for_stop
                    && self.control.task(&record.task_id)?.runner_state == RunnerState::Stopped =>
            {
                let now = self.control.now();
                return Ok(SubmitOperatorCommandResponse {
                    command: OperatorCommandView {
                        command_id: "stopped-before-the-cut".to_string(),
                        command: request.command,
                        status: narsor_protocol::controller::OperatorCommandStatus::Applied,
                        submitted_at: now,
                        updated_at: now,
                        detail: Some(
                            "the task reached its safe point before the cut; it is stopped"
                                .to_string(),
                        ),
                    },
                    replayed: false,
                });
            }
            Err(error) => return Err(error.into()),
        };
        if !outcome.replayed {
            self.notify(&record.task_id);
        }
        if !outcome.command.status.is_settled() {
            self.resume_for_work(&record, &outcome.command.idempotency_key)?;
        }
        Ok(SubmitOperatorCommandResponse {
            command: OperatorCommandView::from(&outcome.command),
            replayed: outcome.replayed,
        })
    }

    /// What the control plane can tell about `command` without the runner.
    fn check_operator_command(
        &self,
        record: &TaskRecord,
        command: &OperatorCommand,
    ) -> ProtocolResult<()> {
        if matches!(command, OperatorCommand::SetConductorHarness { .. }) {
            return Err(ProtocolError::bad_request(
                "unsupported_command",
                RETIRED_CONDUCTOR_HARNESS,
            ));
        }
        if let Some(reason) = record.operator_command_refusal(command) {
            return Err(ProtocolError::conflict("operator_command_refused", reason));
        }
        let runner = record.runner.as_ref().ok_or(ProtocolError::NotFound)?;
        // A paused or resumed task has no live runner to be silent: its
        // command waits for the next start.
        let live = record.runner_state == RunnerState::Running
            && record.awaited_launch_generation().is_none();
        if live && self.runner_freshness(record, runner, self.control.now())? == Freshness::Stale {
            return Err(ProtocolError::conflict(
                "operator_command_refused",
                "the task's runner has not been heard from lately; the command would wait for \
                 a runner that may be gone",
            ));
        }
        if let OperatorCommand::Transition {
            target,
            forced,
            reason,
        } = command
        {
            if target.trim().is_empty() {
                return Err(ProtocolError::bad_request(
                    "missing_target",
                    "a transition needs a target state",
                ));
            }
            if *forced
                && reason
                    .as_deref()
                    .is_none_or(|reason| reason.trim().is_empty())
            {
                return Err(ProtocolError::bad_request(
                    "override_reason_required",
                    "an override needs a reason; it is recorded in the task's audit trail",
                ));
            }
            self.check_transition_legal(record, target, *forced)?;
        }
        if let OperatorCommand::SelectStep { target, .. } = command {
            check_step_exists(record, target)?;
        }
        check_conductor_command(command)
    }

    /// Checks a transition against the workflow snapshot and the state the
    /// runner last reported; silent when the runner has reported no state
    /// yet — it decides then.
    fn check_transition_legal(
        &self,
        record: &TaskRecord,
        target: &str,
        forced: bool,
    ) -> ProtocolResult<()> {
        let Some(current) = record
            .runner
            .as_ref()
            .and_then(|runner| runner.snapshot.execution_cursor.current_state.as_deref())
        else {
            return Ok(());
        };
        let definition = match record.launch_spec.definition() {
            Ok(definition) => definition,
            // Bookkeeping-only spec: there is nothing to check against.
            Err(LaunchDefinitionError::Missing(_)) => return Ok(()),
            Err(error) => {
                return Err(ProtocolError::Internal(format!(
                    "the task's workflow snapshot is unusable: {error}"
                )))
            }
        };
        let Some(state) = definition.states.get(current) else {
            return Ok(());
        };
        plan_operator_transition(&definition, state, target, forced)
            .map(|_| ())
            .map_err(|error| match error {
                EngineError::InvalidTransition { message } => {
                    let code = if forced {
                        "override_not_allowed"
                    } else {
                        "illegal_transition"
                    };
                    ProtocolError::bad_request(code, message)
                }
                other => ProtocolError::Internal(other.to_string()),
            })
    }
}

/// Most characters of one chat message to the conductor: the text is kept
/// in the task's event stream, which is not the place for a document.
const MAX_CHAT_CHARS: usize = 4_000;

/// The action of the offer an operator command is, if it is one of them: the
/// conductor's commands are not.
fn operator_action(command: &OperatorCommand) -> Option<TaskActionId> {
    match command {
        OperatorCommand::Interrupt => Some(TaskActionId::Interrupt),
        OperatorCommand::StopStep => Some(TaskActionId::Stop),
        OperatorCommand::SelectStep { .. } => Some(TaskActionId::SelectStep),
        OperatorCommand::Transition { .. } => Some(TaskActionId::Transition),
        _ => None,
    }
}

/// What can be said about a conductor command without the runner: that it
/// names what it needs.
fn check_conductor_command(command: &OperatorCommand) -> ProtocolResult<()> {
    let blank = |text: &str| text.trim().is_empty();
    match command {
        OperatorCommand::ConductorChat { text } if blank(text) => Err(ProtocolError::bad_request(
            "empty_message",
            "a chat message cannot be empty",
        )),
        OperatorCommand::ConductorChat { text } if text.chars().count() > MAX_CHAT_CHARS => {
            Err(ProtocolError::bad_request(
                "message_too_long",
                format!("a chat message holds at most {MAX_CHAT_CHARS} characters"),
            ))
        }
        OperatorCommand::ConfirmProposal { proposal_id }
        | OperatorCommand::RejectProposal { proposal_id }
            if blank(proposal_id) =>
        {
            Err(ProtocolError::bad_request(
                "missing_proposal",
                "a proposal command needs the proposal's id",
            ))
        }
        OperatorCommand::UndoDecision { decision_id } if blank(decision_id) => {
            Err(ProtocolError::bad_request(
                "missing_decision",
                "undoing a decision needs the decision's id",
            ))
        }
        _ => Ok(()),
    }
}

/// Checks the step an owner chose against the workflow snapshot; silent
/// when the task has none to check against.
pub(crate) fn check_step_exists(record: &TaskRecord, target: &str) -> ProtocolResult<()> {
    if target.trim().is_empty() {
        return Err(ProtocolError::bad_request(
            "missing_target",
            "a step choice needs a step",
        ));
    }
    match record.launch_spec.definition() {
        Ok(definition) if !definition.states.contains_key(target) => {
            Err(ProtocolError::bad_request(
                "unknown_step",
                format!("the workflow has no step '{target}'"),
            ))
        }
        Ok(_) | Err(LaunchDefinitionError::Missing(_)) => Ok(()),
        Err(error) => Err(ProtocolError::Internal(format!(
            "the task's workflow snapshot is unusable: {error}"
        ))),
    }
}
