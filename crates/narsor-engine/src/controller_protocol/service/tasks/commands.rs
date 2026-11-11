//! Owner commands on a task and moving it to another backend.

use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::ProtocolService;
use crate::controller_protocol::wire::*;
use crate::domain::TaskId;
use crate::orchestration::{CommandActor, CommandRequest, MoveTarget, OwnerRef, RunnerState};

impl ProtocolService {
    /// Applies an owner command.
    pub fn command(
        &self,
        owner: &OwnerRef,
        task_id: &str,
        request: OwnerCommandRequest,
    ) -> ProtocolResult<OwnerCommandResponse> {
        let record = self.owned_task(owner, task_id)?;
        if request.idempotency_key.trim().is_empty() {
            return Err(ProtocolError::bad_request(
                "missing_idempotency_key",
                "an owner command needs an idempotency key",
            ));
        }
        if let Some((prefix, purpose)) = crate::orchestration::RESERVED_KEY_PREFIXES
            .iter()
            .find(|(prefix, _)| request.idempotency_key.starts_with(prefix))
        {
            return Err(ProtocolError::bad_request(
                "reserved_idempotency_key",
                format!("idempotency keys starting with `{prefix}` are reserved for {purpose}"),
            ));
        }
        let command = request.command;
        // A repeated key is answered with the receipt it got, not judged anew.
        let is_retry = record
            .commands
            .iter()
            .any(|receipt| receipt.idempotency_key == request.idempotency_key);
        if !is_retry {
            self.require_owner_command_available(&record, command, request.expected_generation)?;
        }
        let outcome = self.control.command_with_capacity_from(
            &record.task_id,
            CommandRequest {
                command,
                idempotency_key: request.idempotency_key,
                expected_generation: request.expected_generation,
                actor: CommandActor::Owner,
            },
            || self.scheduling_policy(&record.assignment.backend_id),
        )?;
        if command == crate::orchestration::OwnerCommand::Archive {
            // Archive removes the task's materials too (U9) — also on a
            // replay, which finishes a purge a crash may have cut short.
            self.purge_materials(&record.task_id);
        }
        let mut task = outcome.task;
        if !outcome.replayed {
            self.notify(&record.task_id);
            // An archive can release a slot for queued work on any backend.
            self.reschedule_all();
            task = self.control.task(&record.task_id)?;
        }
        let queued_for_slot = outcome.receipt.command == crate::orchestration::OwnerCommand::Resume
            && outcome.receipt.runner_state_after == RunnerState::Queued;
        Ok(OwnerCommandResponse {
            task: self.summarize(&task)?,
            receipt: outcome.receipt,
            replayed: outcome.replayed,
            queued_for_slot,
        })
    }

    /// Moves a queued task within its backend's queue and re-evaluates
    /// the queue (R1). Never stops or reorders anything holding compute.
    pub fn move_task(
        &self,
        owner: &OwnerRef,
        task_id: &str,
        request: MoveTaskRequest,
    ) -> ProtocolResult<TaskSummary> {
        let record = self.owned_task(owner, task_id)?;
        let target = match request.target {
            MoveTaskTarget::Before(anchor) => {
                let anchor = TaskId::parse(anchor).map_err(|error| {
                    ProtocolError::bad_request("invalid_task_id", error.to_string())
                })?;
                // An anchor of another owner is refused like one that does
                // not exist: its existence is not the caller's to learn.
                if self.owned_task(owner, anchor.as_str()).is_err() {
                    return Err(
                        crate::orchestration::OrchestrationError::InvalidMoveAnchor {
                            task_id: record.task_id.clone(),
                            anchor,
                            reason: "the anchor is not one of your tasks",
                        }
                        .into(),
                    );
                }
                MoveTarget::Before(anchor)
            }
            MoveTaskTarget::After(anchor) => {
                let anchor = TaskId::parse(anchor).map_err(|error| {
                    ProtocolError::bad_request("invalid_task_id", error.to_string())
                })?;
                // Keep the same owner-scoped indistinguishability as Before.
                if self.owned_task(owner, anchor.as_str()).is_err() {
                    return Err(
                        crate::orchestration::OrchestrationError::InvalidMoveAnchor {
                            task_id: record.task_id.clone(),
                            anchor,
                            reason: "the anchor is not one of your tasks",
                        }
                        .into(),
                    );
                }
                MoveTarget::After(anchor)
            }
            MoveTaskTarget::Top => MoveTarget::Top,
            MoveTaskTarget::Bottom => MoveTarget::Bottom,
        };
        self.require_movable(&record, request.expected_generation)?;
        let (_, changed) =
            self.control
                .move_task(&record.task_id, &target, request.expected_generation)?;
        if changed {
            self.notify(&record.task_id);
            self.reschedule(&record.assignment.backend_id);
        }
        self.owned_summary(owner, &record.task_id)
    }
}
