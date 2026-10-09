//! Operator commands the control plane hands the runner: interrupt a step,
//! take a transition, restart a failed step (U3), and the conductor's
//! chat, proposals, undo and harness (U5, see [`super::conductor`]).
//!
//! The runner is the only writer of its execution, so it is also where a
//! command is decided. Every operation goes through the Engine's own claim
//! on the execution — the one a pass holds while it drives it — so a command
//! can never race the pass or another command: what it checks, it checks on
//! the read it then writes from, and a command that finds the execution
//! somewhere it was not meant for is refused with the reason. The control
//! plane's earlier checks only spare the owner a round trip.
//!
//! A command is carried out at most once per storage: its progress is
//! written before the Engine is touched and its verdict before the control
//! plane hears of it, so a redelivery after a lost acknowledgement — or a
//! restart of the runner — is answered from there.

use super::storage::{CommandProgress, CommandVerdict};
use super::{RunnerError, Session};
use crate::controller_protocol::DeliveredOperatorCommand;
use crate::domain::ExecutionStatus;
use crate::engine::{EngineError, OperatorTransition, Recovery};
use crate::orchestration::OperatorCommand;

impl Session {
    /// Carries out one delivered command, once, and acknowledges it.
    pub(super) async fn carry_out(
        &mut self,
        delivered: DeliveredOperatorCommand,
    ) -> Result<(), RunnerError> {
        let Some(client) = self.link.client() else {
            return Ok(());
        };
        let id = delivered.command_id.clone();
        let verdict = match self.storage.command_progress(&id)? {
            CommandProgress::Done(verdict) => verdict,
            CommandProgress::Begun => {
                let verdict = CommandVerdict::rejected(
                    "the runner stopped while it was carrying this command out; check the \
                     task before sending it again",
                );
                self.storage.finish_command(&id, &verdict)?;
                verdict
            }
            CommandProgress::Unseen => {
                self.storage.begin_command(&id)?;
                let verdict = self.decide(delivered).await;
                self.storage.finish_command(&id, &verdict)?;
                verdict
            }
        };
        if let Err(error) = client
            .ack_command(&id, verdict.applied, verdict.detail)
            .await
        {
            // Redelivered later and answered from the recorded verdict.
            self.link.failed(&error, &self.timing);
        }
        Ok(())
    }

    async fn decide(&mut self, delivered: DeliveredOperatorCommand) -> CommandVerdict {
        let Some(execution_id) = self.execution_id.clone() else {
            return CommandVerdict::rejected("the task has no execution yet");
        };
        tracing::info!(
            command = %delivered.command_id,
            requested_by = %delivered.requested_by,
            kind = delivered.command.describe(),
            "carrying out an operator command"
        );
        let outcome = match delivered.command {
            OperatorCommand::Interrupt => self.interrupt(&execution_id).await,
            OperatorCommand::Transition {
                target,
                forced,
                reason,
            } => self.transition(
                &execution_id,
                OperatorTransition {
                    target,
                    forced,
                    reason,
                    requested_by: delivered.requested_by,
                },
            ),
            OperatorCommand::RestartStep { note, failure_key } => {
                self.restart_step(&execution_id, note, failure_key)
            }
            OperatorCommand::ConductorChat { text } => self.conductor_chat(&execution_id, &text),
            OperatorCommand::ConfirmProposal { proposal_id } => {
                self.confirm_proposal(&execution_id, &proposal_id)
            }
            OperatorCommand::RejectProposal { proposal_id } => {
                self.reject_proposal(&execution_id, &proposal_id)
            }
            OperatorCommand::UndoDecision { decision_id } => {
                self.undo_decision(&execution_id, &decision_id).await
            }
            OperatorCommand::SetConductorHarness { harness } => {
                self.set_conductor_harness(&harness)
            }
        };
        match outcome {
            Ok(()) => CommandVerdict::applied(),
            Err(reason) => CommandVerdict::rejected(reason),
        }
    }

    /// Drops the pass that is driving the step — its harness is killed with
    /// it — and leaves the execution recoverable.
    async fn interrupt(&mut self, execution_id: &str) -> Result<(), String> {
        if !self.pass_active() {
            return Err("no step is running, so there is nothing to interrupt".to_string());
        }
        // The runner's own pass and one the conductor started both drive
        // the execution; whichever runs is dropped, its harness with it.
        let own = self.pass.take();
        let conductor = self.conductor.passes().take();
        if let Some(pass) = &own {
            pass.abort();
        }
        if let Some(pass) = &conductor {
            pass.abort();
        }
        // The claim goes with the dropped pass; settling takes it.
        let own_finished =
            joined(own, "the pass finished before the interrupt reached it").await;
        let conductor_finished = joined(
            conductor,
            "the conductor's pass finished before the interrupt reached it",
        )
        .await;
        let finished = own_finished || conductor_finished;
        if finished {
            return Err("the step finished before it could be interrupted".to_string());
        }
        self.engine
            .settle_interrupted(execution_id)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    /// Takes the transition and, when it leads into a state that still has
    /// work to do, carries on from there: the owner chose where the task
    /// goes, and nothing in a running task asks for a second "resume".
    fn transition(
        &mut self,
        execution_id: &str,
        transition: OperatorTransition,
    ) -> Result<(), String> {
        let execution = self
            .engine
            .take_operator_transition(&self.definition, execution_id, &transition)
            .map_err(|error| refusal(&error))?;
        if execution.status == ExecutionStatus::Paused {
            self.resume_pass().map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Starts the failed or interrupted step again: a failed or unknown
    /// execution is recovered, a paused one (cut between steps) resumed,
    /// each with the owner's note for the agent's next attempt.
    fn restart_step(
        &mut self,
        execution_id: &str,
        note: Option<String>,
        failure_key: Option<String>,
    ) -> Result<(), String> {
        if self.pass_active() {
            return Err("a step is running; interrupt it first".to_string());
        }
        let status = self
            .engine
            .store()
            .load_execution(execution_id)
            .map_err(|error| error.to_string())?
            .status;
        let note = note.filter(|note| !note.trim().is_empty());
        let handle = match status {
            ExecutionStatus::Failed | ExecutionStatus::Unknown => self
                .engine
                .try_begin_recover_with(execution_id, Recovery::by_operator(note, failure_key)),
            ExecutionStatus::Running | ExecutionStatus::Paused => {
                if let Some(note) = &note {
                    self.engine
                        .leave_operator_note(execution_id, note, failure_key.as_deref())
                        .map_err(|error| refusal(&error))?;
                }
                self.engine.try_begin_resume(execution_id)
            }
            other => {
                return Err(format!(
                    "the execution is {}: there is no step to restart",
                    other.as_str()
                ))
            }
        }
        .map_err(|error| refusal(&error))?;
        self.spawn_pass(handle);
        Ok(())
    }
}

/// An Engine refusal as the owner reads it.
fn refusal(error: &EngineError) -> String {
    match error {
        EngineError::AlreadyRunning { .. } => "a step is running; interrupt it first".to_string(),
        other => other.to_string(),
    }
}

/// Waits for an aborted pass to let go. Whether it ran to its end instead of
/// being cancelled: the interrupt came too late for it.
async fn joined<T>(pass: Option<tokio::task::JoinHandle<T>>, finished_message: &str) -> bool {
    let Some(pass) = pass else {
        return false;
    };
    let finished = pass.await.is_ok();
    if finished {
        tracing::info!("{finished_message}");
    }
    finished
}
