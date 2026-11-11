//! The "already running" claim a pass holds on its execution.

use super::*;

/// Holds the "already running" claim on one execution for as long as a pass is
/// driving it.
///
/// Acquired by [`Engine::begin_run`], which does the check-and-insert under one
/// lock so two attempts can never both succeed; released on `Drop`, so every
/// exit path — a normal return, an early `?`, or a panic unwinding through it —
/// frees the execution the same way.
pub(crate) struct RunGuard<'a> {
    engine: &'a Engine,
    execution_id: ExecutionId,
}

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        self.engine.lock_active_runs().remove(&self.execution_id);
    }
}

/// An owned equivalent of [`RunGuard`]: holds the same "already running"
/// claim, over the same `active_runs` set, but through an [`Arc<Engine>`]
/// rather than a borrow — which is what lets it live inside a
/// `tokio::spawn`-ed task instead of a stack frame that ends when an HTTP
/// handler returns (U3). [`Engine::begin_run`] and this claim both operate on
/// the one `active_runs` set, so a plain [`Engine::run`]/[`Engine::resume`]
/// and a [`RunHandle`]-driven resume can never both succeed on the same
/// execution.
pub(crate) struct ActiveRunClaim {
    engine: Arc<Engine>,
    execution_id: ExecutionId,
}

impl Drop for ActiveRunClaim {
    fn drop(&mut self) {
        self.engine.lock_active_runs().remove(&self.execution_id);
    }
}

impl Engine {
    /// Claims the right to drive `execution_id`, or refuses if another pass
    /// already holds it.
    ///
    /// The check and the insert happen under the one lock this returns from
    /// holding, which is what makes two concurrent callers unable to both
    /// succeed (R7): a separate check-then-insert would leave a gap between
    /// the two a second caller could land in.
    pub(crate) fn begin_run(&self, execution_id: &str) -> Result<RunGuard<'_>, EngineError> {
        if !self.lock_active_runs().insert(execution_id.to_string()) {
            return Err(EngineError::AlreadyRunning {
                execution_id: execution_id.to_string(),
            });
        }
        Ok(RunGuard {
            engine: self,
            execution_id: execution_id.to_string(),
        })
    }

    /// The owned equivalent of [`Engine::begin_run`]: claims the right to
    /// drive `execution_id` through an [`ActiveRunClaim`] that lives inside
    /// an [`Arc<Engine>`] rather than borrowing `&self`, so it can be moved
    /// into a `tokio::spawn`-ed task. Operates on the exact same
    /// `active_runs` set as [`Engine::begin_run`], so the two can never both
    /// succeed for the same execution.
    pub(crate) fn claim_active_run(
        self: &Arc<Self>,
        execution_id: &str,
    ) -> Result<ActiveRunClaim, EngineError> {
        if !self.lock_active_runs().insert(execution_id.to_string()) {
            return Err(EngineError::AlreadyRunning {
                execution_id: execution_id.to_string(),
            });
        }
        Ok(ActiveRunClaim {
            engine: Arc::clone(self),
            execution_id: execution_id.to_string(),
        })
    }

    /// Locks the active-run set, recovering from a poisoned mutex: a panic
    /// elsewhere is no reason to make every future run impossible.
    pub(crate) fn lock_active_runs(&self) -> std::sync::MutexGuard<'_, HashSet<ExecutionId>> {
        self.active_runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
