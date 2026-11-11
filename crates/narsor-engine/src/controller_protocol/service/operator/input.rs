//! Human input the owner submits for a waiting task.

use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::ProtocolService;
use crate::controller_protocol::wire::*;
use crate::domain::AnsweredBy;
use crate::engine::HumanInputDelivery;
use crate::orchestration::{
    CommandActor, CommandRequest, InputRequest, OwnerRef, RunnerState, TaskRecord,
};

impl ProtocolService {
    /// Queues an owner answer for the runner's pending input request.
    pub fn submit_input(
        &self,
        owner: &OwnerRef,
        task_id: &str,
        request: SubmitInputRequest,
    ) -> ProtocolResult<SubmitInputResponse> {
        let record = self.owned_task(owner, task_id)?;
        if request.idempotency_key.trim().is_empty() {
            return Err(ProtocolError::bad_request(
                "missing_idempotency_key",
                "an input needs an idempotency key",
            ));
        }
        let is_retry = record
            .inputs
            .iter()
            .any(|queued| queued.idempotency_key == request.idempotency_key);
        if !is_retry {
            self.require_input_available(&record, &request)?;
        }
        let outcome = self.control.queue_input(
            &record.task_id,
            InputRequest {
                pending_input_id: request.pending_input_id,
                idempotency_key: request.idempotency_key,
                delivery: engine_delivery(request.delivery),
            },
        )?;
        if !outcome.replayed {
            self.notify(&record.task_id);
        }
        if !outcome.input.status.is_settled() {
            self.resume_for_work(&record, &outcome.input.idempotency_key)?;
        }
        Ok(SubmitInputResponse {
            input: InputView::from(&outcome.input),
            replayed: outcome.replayed,
        })
    }

    /// Work the owner sent (an answer, an operator command) was stored for a
    /// task that may be paused: resumes it, as the owner's resume would, so
    /// the work reaches the runner of the next start (R13, R14).
    ///
    /// With no free slot the task goes to the front of the queue instead
    /// and starts when a slot frees (R20): the work never starts an agent
    /// without one. The resume's key derives from the work's own key, so a
    /// retry of the work resumes the task once. A task that is not paused
    /// (running, or already resumed or queued) needs nothing. Called for a
    /// retry too: a first call that stored the work and failed before the
    /// resume is repaired by it.
    pub(crate) fn resume_for_work(
        &self,
        record: &TaskRecord,
        work_key: &str,
    ) -> ProtocolResult<()> {
        let current = self.control.task(&record.task_id)?;
        if current.runner_state != RunnerState::Stopped {
            return Ok(());
        }
        let outcome = self.control.command_with_capacity_from(
            &current.task_id,
            CommandRequest {
                command: crate::orchestration::OwnerCommand::Resume,
                idempotency_key: crate::orchestration::work_resume_key(work_key),
                expected_generation: None,
                actor: CommandActor::Owner,
            },
            || self.scheduling_policy(&current.assignment.backend_id),
        );
        match outcome {
            Ok(outcome) => {
                if !outcome.replayed {
                    self.notify(&current.task_id);
                    self.reschedule(&current.assignment.backend_id);
                }
                Ok(())
            }
            // Someone else moved the task first (resumed it, archived it):
            // the work is stored and goes the way the task goes.
            Err(
                crate::orchestration::OrchestrationError::StaleGeneration { .. }
                | crate::orchestration::OrchestrationError::IllegalCommand { .. },
            ) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

fn engine_delivery(delivery: narsor_protocol::api::HumanInputDeliveryDto) -> HumanInputDelivery {
    let permission_decision =
        delivery
            .permission_decision
            .map(|decision| crate::engine::actions::PermissionDecision {
                denial_index: decision.denial_index,
                decision: match decision.decision {
                    narsor_protocol::api::PermissionDecisionKindDto::Deny => {
                        crate::engine::actions::PermissionDecisionKind::Deny
                    }
                    narsor_protocol::api::PermissionDecisionKindDto::Allow => {
                        crate::engine::actions::PermissionDecisionKind::Allow
                    }
                },
                scope: decision.scope.map(|scope| match scope {
                    narsor_protocol::api::PermissionGrantScopeDto::ExactOperation => {
                        crate::engine::actions::PermissionGrantScope::ExactOperation
                    }
                    narsor_protocol::api::PermissionGrantScopeDto::CommandGroup => {
                        crate::engine::actions::PermissionGrantScope::CommandGroup
                    }
                    narsor_protocol::api::PermissionGrantScopeDto::Pattern => {
                        crate::engine::actions::PermissionGrantScope::Pattern
                    }
                }),
                pattern: decision.pattern,
                message: decision.message,
            });
    HumanInputDelivery {
        values: delivery.values,
        chosen_label: delivery.chosen_label,
        artifacts: delivery.artifacts,
        variables: delivery.variables,
        comment: delivery.comment,
        agent_message: delivery.agent_message,
        action: delivery.action,
        episode: delivery.episode,
        permission_decision,
        answered_by: AnsweredBy::Operator,
    }
}

pub(crate) fn protocol_delivery(
    delivery: &HumanInputDelivery,
) -> narsor_protocol::api::HumanInputDeliveryDto {
    narsor_protocol::api::HumanInputDeliveryDto {
        values: delivery.values.clone(),
        chosen_label: delivery.chosen_label.clone(),
        artifacts: delivery.artifacts.clone(),
        variables: delivery.variables.clone(),
        comment: delivery.comment.clone(),
        agent_message: delivery.agent_message.clone(),
        action: delivery.action,
        episode: delivery.episode.clone(),
        permission_decision: delivery.permission_decision.as_ref().map(|decision| {
            narsor_protocol::api::PermissionDecisionDto {
                denial_index: decision.denial_index,
                decision: match decision.decision {
                    crate::engine::actions::PermissionDecisionKind::Deny => {
                        narsor_protocol::api::PermissionDecisionKindDto::Deny
                    }
                    crate::engine::actions::PermissionDecisionKind::Allow => {
                        narsor_protocol::api::PermissionDecisionKindDto::Allow
                    }
                },
                scope: decision.scope.map(|scope| match scope {
                    crate::engine::actions::PermissionGrantScope::ExactOperation => {
                        narsor_protocol::api::PermissionGrantScopeDto::ExactOperation
                    }
                    crate::engine::actions::PermissionGrantScope::CommandGroup => {
                        narsor_protocol::api::PermissionGrantScopeDto::CommandGroup
                    }
                    crate::engine::actions::PermissionGrantScope::Pattern => {
                        narsor_protocol::api::PermissionGrantScopeDto::Pattern
                    }
                }),
                pattern: decision.pattern.clone(),
                message: decision.message.clone(),
            }
        }),
    }
}
