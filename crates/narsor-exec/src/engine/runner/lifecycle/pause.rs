//! Pausing a pass, and asking which passes are active.

use super::*;

impl Engine {
    /// Asks the pass currently running this execution to stop before its next
    /// action (D34).
    ///
    /// Pausing never interrupts a running action: the current one finishes
    /// normally and its transition is taken, and only then does the loop stop.
    /// The request is in-process and is honoured by the next step boundary —
    /// including the first one, if no pass is running yet.
    pub fn pause(&self, execution_id: &str) {
        self.lock_pause_requests().insert(execution_id.to_string());
    }

    /// Whether a pass is currently driving this execution through
    /// [`Engine::run`], [`Engine::resume`] or [`Engine::deliver_human_input`]
    /// (R7).
    ///
    /// For a caller deciding whether it is safe to touch the execution's
    /// record by some *other* means (the TUI's manual/forced transition,
    /// which writes the store directly rather than going through one of
    /// those three) — not for retrying the call itself, since a failed
    /// acquisition already reports [`EngineError::AlreadyRunning`] atomically
    /// at the point that matters.
    pub fn is_active(&self, execution_id: &str) -> bool {
        self.lock_active_runs().contains(execution_id)
    }

    /// Parks the execution: `Paused` plus `WorkflowPaused`.
    pub(crate) fn pause_execution(
        &self,
        execution: &mut WorkflowExecution,
        reason: &str,
    ) -> Result<(), EngineError> {
        execution.status = ExecutionStatus::Paused;
        self.store.save_execution(execution)?;
        self.emit(
            &execution.id,
            EventData::WorkflowPaused {
                reason: reason.to_string(),
            },
        )?;
        Ok(())
    }

    /// Consumes a pending pause request, if there is one.
    pub(crate) fn take_pause_request(&self, execution_id: &str) -> bool {
        self.lock_pause_requests().remove(execution_id)
    }

    /// Locks the pause requests, recovering from a poisoned mutex: a panic
    /// elsewhere is no reason to make pausing impossible.
    pub(crate) fn lock_pause_requests(&self) -> std::sync::MutexGuard<'_, HashSet<ExecutionId>> {
        self.pause_requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
