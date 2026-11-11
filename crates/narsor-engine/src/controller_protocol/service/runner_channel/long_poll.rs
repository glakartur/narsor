//! Waiting for change notifications on behalf of a long-poll.

use tokio::sync::broadcast::error::RecvError;
use tokio::sync::watch;

use crate::controller_protocol::error::ProtocolResult;
use crate::controller_protocol::service::ProtocolService;
use crate::domain::TaskId;

impl ProtocolService {
    pub(super) fn long_poll(&self, requested: Option<u64>) -> std::time::Duration {
        std::time::Duration::from_secs(requested.unwrap_or(0)).min(self.config.max_long_poll)
    }

    /// Re-runs `check` after every change notification until it finds
    /// something or `wait` elapses. With `only`, a notification of another
    /// task is ignored: the check reads that one task, so another's change
    /// cannot have given it anything to find.
    ///
    /// Subscribes *before* the first check, so a change landing between the
    /// check and the wait is never missed.
    pub(super) async fn wait_for<T>(
        &self,
        wait: std::time::Duration,
        only: Option<&TaskId>,
        mut check: impl FnMut() -> ProtocolResult<Option<T>>,
    ) -> ProtocolResult<Option<T>> {
        let mut changes = self.changes.subscribe();
        // The installation's default harness, which every task sees (U7):
        // a runner hears of a change now rather than when its long-poll
        // runs out.
        let mut harness_changes = self.launch.default_harness_changes();
        let deadline = tokio::time::Instant::now() + wait;
        let mut relevant = true;
        loop {
            if relevant {
                if let Some(found) = check()? {
                    return Ok(Some(found));
                }
            }
            relevant = tokio::select! {
                received = tokio::time::timeout_at(deadline, changes.recv()) => match received {
                    Err(_elapsed) => return Ok(None),
                    Ok(Ok(changed)) => only.is_none_or(|task_id| *task_id == changed),
                    // Some notifications were missed: any of them may matter.
                    Ok(Err(RecvError::Lagged(_))) => true,
                    Ok(Err(RecvError::Closed)) => return Ok(None),
                },
                Ok(()) = harness_changed(&mut harness_changes) => true,
            };
        }
    }
}

/// Resolves when the default harness `changes` watches changes; never
/// without one.
async fn harness_changed(
    changes: &mut Option<watch::Receiver<u64>>,
) -> Result<(), watch::error::RecvError> {
    match changes {
        Some(changes) => changes.changed().await,
        None => std::future::pending().await,
    }
}
