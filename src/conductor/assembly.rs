//! Putting a conductor together, and bringing it back after a restart.
//!
//! Two processes host a conductor: the daemon, for the executions it drives
//! itself, and the task runner, for the one execution on its storage. Both
//! assemble it the same way — a [`ConductorRegistry`] over their engine,
//! attached to it, with the means to start a pass of its own — and both
//! restore its unfinished work right after their own recovery. That is what
//! lives here, so neither process can drift from the other.

use std::sync::Arc;

use crate::conductor::{ConductorInvoker, ConductorRegistry, ConductorRestore, PassLauncher};
use crate::domain::ExecutionId;
use crate::engine::Engine;

/// A conductor over `engine`, running through `invoker`, attached to the
/// engine so that it is handed what the engine cannot settle itself (F1,
/// F3).
///
/// With a `launcher` the conductor may start the one retry of a failed step
/// (R21) in the process's own registry of passes; without one that retry is
/// only proposed. The engine holds the registry weakly: the caller keeps the
/// returned [`Arc`] for as long as the conductor should live.
pub fn assemble(
    engine: &Arc<Engine>,
    invoker: Arc<dyn ConductorInvoker>,
    launcher: Option<Arc<dyn PassLauncher>>,
) -> Arc<ConductorRegistry> {
    let mut registry = ConductorRegistry::new(Arc::clone(engine), invoker);
    if let Some(launcher) = launcher {
        registry = registry.with_pass_launcher(launcher);
    }
    let registry = Arc::new(registry);
    engine.attach_conductor(&registry);
    registry
}

/// What [`restore_at_startup`] picked up again.
#[derive(Debug, Default)]
pub struct Restoration {
    /// The conductor's own rows: queues served again, label rescues
    /// discarded, automatic requests parked.
    pub restore: ConductorRestore,
    /// Executions whose failed step is diagnosed again.
    pub diagnosed: Vec<ExecutionId>,
}

/// The conductor's rows of the restart matrix, run once after the process's
/// own recovery: unanswered chat messages are served again, interrupted
/// advisory invocations run again, and a failed step the conductor had not
/// concluded about is diagnosed again. Best-effort, like the rest of the
/// startup sweep: what cannot be restored is logged and skipped.
pub fn restore_at_startup(conductor: &Arc<ConductorRegistry>) -> Restoration {
    let mut restoration = Restoration::default();

    match conductor.restore_at_startup() {
        Ok(restore) => {
            for (execution_id, requests) in &restore.requeued {
                tracing::info!(
                    execution_id = %execution_id,
                    requests,
                    "conductor: resumes its queue after the restart"
                );
            }
            for execution_id in &restore.discarded {
                tracing::info!(
                    execution_id = %execution_id,
                    "conductor: discarded a label rescue for an execution that moved on"
                );
            }
            for (execution_id, requests, pending) in &restore.parked {
                tracing::warn!(
                    execution_id = %execution_id,
                    queued = requests.len(),
                    interrupted = pending.is_some(),
                    "conductor: automatic requests were left from before the restart"
                );
            }
            restoration.restore = restore;
        }
        Err(error) => {
            tracing::warn!(error = %error, "conductor: could not be restored");
        }
    }

    match conductor.restore_failures_at_startup() {
        Ok(diagnosed) => {
            for execution_id in &diagnosed {
                tracing::info!(
                    execution_id = %execution_id,
                    "conductor: diagnoses again a failed step it had not concluded about"
                );
            }
            restoration.diagnosed = diagnosed;
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                "conductor: the failed-step diagnoses could not be restored"
            );
        }
    }
    restoration
}
