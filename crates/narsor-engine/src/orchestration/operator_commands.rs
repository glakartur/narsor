//! Operator commands: what an owner asks the runner of a running task to
//! do — interrupt a step, take a transition, restart a failed step (U3).
//!
//! The mechanism is the one owner inputs use (see [`super::runner_state`]):
//! the owner's request becomes a durable event on the task record, the
//! runner receives it with its long-poll — again and again until it
//! acknowledges — and its verdict becomes a second event the owner's stream
//! shows. The control plane never executes anything: it validates what it
//! can know (runner_state, generation, idempotency) and relays; the runner
//! decides under the Engine's own claim on the execution.
//!
//! A command is meant for one runner launch. When the task stops being
//! `Running`, or a newer launch takes over, the commands nobody settled are
//! rejected by the next poll instead of reaching a runner they were never
//! meant for.
//!
//! A task without a runner (paused, or resumed and waiting for a slot or its
//! runner) keeps the work sent to it for the launch that comes next (R13,
//! R14): the command is bound to that launch's generation (see
//! [`TaskRecord::operator_command_launch`]), the poll of any other runner
//! neither gets it nor settles it, and the poll of that launch's runner —
//! also while the task still provisions — takes it.

use super::control_plane::{ControlPlane, Mutation};
use super::error::{OrchestrationError, Result};
use super::model::{
    OperatorCommand, OperatorCommandOutcome, OperatorCommandRequest, OperatorCommandStatus,
    QueuedOperatorCommand, RunnerState, TaskEventKind, TaskRecord,
};
use super::runner_state::fence_runner;
use crate::domain::display::bounded_detail;
use crate::domain::TaskId;

/// Settled commands a record keeps, newest last; older ones are forgotten.
/// Open commands are never forgotten.
const SETTLED_COMMANDS_KEPT: usize = 20;

/// Commands that may wait for a verdict at once; a new one is refused
/// beyond it, so a silent runner cannot make the record grow.
const MAX_OPEN_COMMANDS: usize = 20;

/// Keys of forgotten commands a record remembers.
const RETIRED_KEYS_KEPT: usize = 256;

impl ControlPlane {
    /// Queues an operator command for the runner of a running task.
    ///
    /// A retry with the same key and command returns the original, whatever
    /// became of it meanwhile; the same key for another command is refused.
    /// A new command is refused — without touching the record — unless the
    /// task is `Running` under a runner of its current launch, and when
    /// `expected_generation` no longer holds or too many commands already
    /// wait for the runner.
    ///
    /// A record keeps its open commands and the last few settled ones,
    /// the text of a settled conductor message dropped (it is the owner's
    /// words, and the conductor has answered). A retry of a command that
    /// was forgotten meanwhile is refused as a reused key — never carried
    /// out again — for as long as its key is among the last
    /// [`RETIRED_KEYS_KEPT`] forgotten; a retry that late is not something a
    /// client does.
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
                if !same_request(existing, &request.command) {
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
            if record
                .retired_command_keys
                .contains(&request.idempotency_key)
            {
                return Err(OrchestrationError::OperatorCommandKeyReused {
                    task_id: record.task_id.clone(),
                    key: request.idempotency_key.clone(),
                });
            }
            if let Some(expected) = request.expected_generation {
                record.check_generation(expected)?;
            }
            if let Some(reason) = record.operator_command_refusal(&request.command) {
                return Err(OrchestrationError::OperatorCommandRefused {
                    task_id: record.task_id.clone(),
                    reason,
                });
            }
            if open_commands(record).len() >= MAX_OPEN_COMMANDS {
                return Err(OrchestrationError::OperatorCommandRefused {
                    task_id: record.task_id.clone(),
                    reason: format!(
                        "{MAX_OPEN_COMMANDS} commands already wait for the runner; \
                         wait for it to take them"
                    ),
                });
            }
            // The runner of the current launch, or the launch the task
            // waits for: a command of a paused or queued task is carried
            // out after the next start.
            let launch_generation = record
                .operator_command_launch()
                .or_else(|| {
                    record
                        .runner
                        .as_ref()
                        .map(|runner| runner.launch_generation)
                })
                .unwrap_or(record.generation);
            let queued = QueuedOperatorCommand {
                command_id: next_command_id(record),
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
            record.operator_command_counter = command_number(&queued.command_id);
            record.operator_commands.push(queued.clone());
            forget_old_commands(record);
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
        // Every wake of a runner's long-poll lands here, and almost always
        // there is nothing to reject: look first, without a lock or a write.
        let record = self.task(task_id)?;
        fence_runner(&record, incarnation, launch_generation)?;
        if !record.operator_commands.iter().any(|command| {
            !command.status.is_settled()
                && obsolete_reason(command, record.runner_state, launch_generation).is_some()
        }) {
            return Ok((commands_for(&record, launch_generation), false));
        }
        let mut changed = false;
        loop {
            let (record, rejected) = self.mutate(task_id, |record, now| {
                fence_runner(record, incarnation, launch_generation)?;
                let runner_state = record.runner_state;
                let Some(command) = record.operator_commands.iter_mut().find(|command| {
                    !command.status.is_settled()
                        && obsolete_reason(command, runner_state, launch_generation).is_some()
                }) else {
                    return Ok(Mutation::Unchanged(false));
                };
                let detail = obsolete_reason(command, runner_state, launch_generation);
                command.status = OperatorCommandStatus::Rejected;
                command.updated_at = now;
                command.detail = detail.clone();
                command.forget_text();
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
                return Ok((commands_for(&record, launch_generation), changed));
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
            command.forget_text();
            let settled = command.clone();
            forget_old_commands(record);
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

/// Whether `existing` is what a retry offering `requested` repeats. The text
/// of a settled conductor message is gone, so only the kind can be compared.
fn same_request(existing: &QueuedOperatorCommand, requested: &OperatorCommand) -> bool {
    match (&existing.command, requested) {
        (OperatorCommand::ConductorChat { text }, OperatorCommand::ConductorChat { .. })
            if text.is_empty() && existing.status.is_settled() =>
        {
            true
        }
        (command, requested) => command == requested,
    }
}

/// The number in a `command-<n>` id; 0 for anything else.
fn command_number(command_id: &str) -> u64 {
    command_id
        .strip_prefix("command-")
        .and_then(|number| number.parse().ok())
        .unwrap_or(0)
}

/// The next id: above every one ever given, whether the counter knew it or
/// only a command still on the record does (a record from before the
/// counter).
fn next_command_id(record: &TaskRecord) -> String {
    let highest = record
        .operator_commands
        .iter()
        .map(|command| command_number(&command.command_id))
        .fold(record.operator_command_counter, u64::max);
    format!("command-{}", highest + 1)
}

/// Drops all but the last [`SETTLED_COMMANDS_KEPT`] settled commands,
/// remembering their keys.
fn forget_old_commands(record: &mut TaskRecord) {
    let settled = record
        .operator_commands
        .iter()
        .filter(|command| command.status.is_settled())
        .count();
    let mut forget = settled.saturating_sub(SETTLED_COMMANDS_KEPT);
    if forget == 0 {
        return;
    }
    let mut retired = Vec::new();
    record.operator_commands.retain(|command| {
        let drop = forget > 0 && command.status.is_settled();
        if drop {
            forget -= 1;
            retired.push(command.idempotency_key.clone());
        }
        !drop
    });
    record.retired_command_keys.extend(retired);
    let excess = record
        .retired_command_keys
        .len()
        .saturating_sub(RETIRED_KEYS_KEPT);
    record.retired_command_keys.drain(..excess);
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

/// The open commands meant for the runner of `launch_generation`: one
/// accepted for a later launch (a paused task's work) waits for it.
fn commands_for(record: &TaskRecord, launch_generation: u64) -> Vec<QueuedOperatorCommand> {
    open_commands(record)
        .into_iter()
        .filter(|command| command.launch_generation == launch_generation)
        .collect()
}

/// Why an unsettled command can no longer be carried out by the runner of
/// `launch_generation`, if it cannot. A command for a later launch is not
/// obsolete: it waits for that launch's runner.
fn obsolete_reason(
    command: &QueuedOperatorCommand,
    runner_state: RunnerState,
    launch_generation: u64,
) -> Option<String> {
    if command.launch_generation > launch_generation {
        return None;
    }
    // A runner polls from its start, while the task is still provisioning.
    if !matches!(
        runner_state,
        RunnerState::Running | RunnerState::Provisioning
    ) {
        return Some(format!(
            "the task stopped running before the runner took the command (runner state: {runner_state:?})"
        ));
    }
    (command.launch_generation != launch_generation)
        .then(|| "the runner it was meant for was replaced".to_string())
}
