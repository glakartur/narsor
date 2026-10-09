//! Operator commands: what an owner asks the runner of a running task to
//! do — interrupt a step, take a transition, restart a failed step (U3).
//!
//! The mechanism is the one owner inputs use (see [`super::runner_state`]):
//! the owner's request becomes a durable event on the task record, the
//! runner receives it with its long-poll — again and again until it
//! acknowledges — and its verdict becomes a second event the owner's stream
//! shows. The control plane never executes anything: it validates what it
//! can know (lifecycle, generation, idempotency) and relays; the runner
//! decides under the Engine's own claim on the execution.
//!
//! A command is meant for one runner launch. When the task stops being
//! `Running`, or a newer launch takes over, the commands nobody settled are
//! rejected by the next poll instead of reaching a runner they were never
//! meant for.

use super::control_plane::{ControlPlane, Mutation};
use super::error::{OrchestrationError, Result};
use super::model::{
    OperatorCommandOutcome, OperatorCommandRequest, OperatorCommandStatus, QueuedOperatorCommand,
    TaskEventKind, TaskLifecycle, TaskRecord,
};
use super::runner_state::fence_runner;
use crate::domain::display::bounded_detail;
use crate::domain::TaskId;

impl ControlPlane {
    /// Queues an operator command for the runner of a running task.
    ///
    /// A retry with the same key and command returns the original, whatever
    /// became of it meanwhile; the same key for another command is refused.
    /// A new command is refused — without touching the record — unless the
    /// task is `Running` under a runner of its current launch, and when
    /// `expected_generation` no longer holds.
    pub fn queue_operator_command(
        &self,
        task_id: &TaskId,
        request: OperatorCommandRequest,
    ) -> Result<OperatorCommandOutcome> {
        let (_, outcome) = self.mutate(task_id, |record, now| {
            if let Some(existing) = record
                .operator_commands
                .iter()
                .find(|queued| queued.idempotency_key == request.idempotency_key)
            {
                if existing.command != request.command {
                    return Err(OrchestrationError::OperatorCommandKeyReused {
                        task_id: record.task_id.clone(),
                        key: request.idempotency_key.clone(),
                    });
                }
                return Ok(Mutation::Unchanged(OperatorCommandOutcome {
                    command: existing.clone(),
                    replayed: true,
                }));
            }
            if let Some(expected) = request.expected_generation {
                record.check_generation(expected)?;
            }
            if let Some(reason) = record.operator_command_refusal() {
                return Err(OrchestrationError::OperatorCommandRefused {
                    task_id: record.task_id.clone(),
                    reason,
                });
            }
            let launch_generation = record
                .runner
                .as_ref()
                .map_or(record.generation, |runner| runner.launch_generation);
            let queued = QueuedOperatorCommand {
                command_id: format!("command-{}", record.operator_commands.len() + 1),
                idempotency_key: request.idempotency_key.clone(),
                command: request.command.clone(),
                requested_by: request.requested_by.clone(),
                expected_generation: request.expected_generation,
                launch_generation,
                status: OperatorCommandStatus::Queued,
                submitted_at: now,
                updated_at: now,
                detail: None,
            };
            record.operator_commands.push(queued.clone());
            Ok(Mutation::Changed(
                TaskEventKind::OperatorCommandQueued {
                    command_id: queued.command_id.clone(),
                    command: queued.command.clone(),
                    requested_by: queued.requested_by.clone(),
                },
                OperatorCommandOutcome {
                    command: queued,
                    replayed: false,
                },
            ))
        })?;
        Ok(outcome)
    }

    /// The commands to hand a polling runner: every one of its launch that
    /// no verdict settled yet (at-least-once; the runner deduplicates by
    /// id). Commands that can no longer be carried out are rejected first.
    ///
    /// Returns the commands and whether the record changed (so the caller
    /// knows whether there is a new event to announce).
    pub fn deliver_operator_commands(
        &self,
        task_id: &TaskId,
        incarnation: &str,
        launch_generation: u64,
    ) -> Result<(Vec<QueuedOperatorCommand>, bool)> {
        let mut changed = false;
        loop {
            let (record, rejected) = self.mutate(task_id, |record, now| {
                fence_runner(record, incarnation, launch_generation)?;
                let lifecycle = record.lifecycle;
                let Some(command) = record.operator_commands.iter_mut().find(|command| {
                    !command.status.is_settled()
                        && obsolete_reason(command, lifecycle, launch_generation).is_some()
                }) else {
                    return Ok(Mutation::Unchanged(false));
                };
                let detail = obsolete_reason(command, lifecycle, launch_generation);
                command.status = OperatorCommandStatus::Rejected;
                command.updated_at = now;
                command.detail = detail.clone();
                Ok(Mutation::Changed(
                    TaskEventKind::OperatorCommandResolved {
                        command_id: command.command_id.clone(),
                        status: OperatorCommandStatus::Rejected,
                        detail,
                    },
                    true,
                ))
            })?;
            if !rejected {
                return Ok((open_commands(&record), changed));
            }
            changed = true;
        }
    }

    /// The runner's verdict on a delivered command. Repeating the same
    /// verdict is harmless (`changed: false`); contradicting a settled one
    /// is refused.
    pub fn settle_operator_command(
        &self,
        task_id: &TaskId,
        incarnation: &str,
        launch_generation: u64,
        command_id: &str,
        applied: bool,
        detail: Option<String>,
    ) -> Result<(QueuedOperatorCommand, bool)> {
        let detail = detail.map(|detail| bounded_detail(&detail));
        let (_, settled) = self.mutate(task_id, |record, now| {
            fence_runner(record, incarnation, launch_generation)?;
            let target = if applied {
                OperatorCommandStatus::Applied
            } else {
                OperatorCommandStatus::Rejected
            };
            let task_id = record.task_id.clone();
            let command = record
                .operator_commands
                .iter_mut()
                .find(|command| command.command_id == command_id)
                .ok_or_else(|| OrchestrationError::OperatorCommandNotFound {
                    task_id: task_id.clone(),
                    command_id: command_id.to_string(),
                })?;
            if command.status == target {
                return Ok(Mutation::Unchanged((command.clone(), false)));
            }
            if command.status.is_settled() {
                return Err(OrchestrationError::OperatorCommandAlreadySettled {
                    task_id,
                    command_id: command_id.to_string(),
                    status: command.status,
                });
            }
            let detail = detail.filter(|detail| !detail.trim().is_empty());
            command.status = target;
            command.updated_at = now;
            command.detail = detail.clone();
            let settled = command.clone();
            Ok(Mutation::Changed(
                TaskEventKind::OperatorCommandResolved {
                    command_id: settled.command_id.clone(),
                    status: target,
                    detail,
                },
                (settled, true),
            ))
        })?;
        Ok(settled)
    }
}

/// Every command still waiting for a verdict.
fn open_commands(record: &TaskRecord) -> Vec<QueuedOperatorCommand> {
    record
        .operator_commands
        .iter()
        .filter(|command| !command.status.is_settled())
        .cloned()
        .collect()
}

/// Why an unsettled command can no longer be carried out by the runner of
/// `launch_generation`, if it cannot.
fn obsolete_reason(
    command: &QueuedOperatorCommand,
    lifecycle: TaskLifecycle,
    launch_generation: u64,
) -> Option<String> {
    if lifecycle != TaskLifecycle::Running {
        return Some(format!(
            "the task stopped running before the runner took the command (lifecycle: {lifecycle:?})"
        ));
    }
    (command.launch_generation != launch_generation)
        .then(|| "the runner it was meant for was replaced".to_string())
}
