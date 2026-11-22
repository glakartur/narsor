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
        self.wait_for_or_until(wait, only, || {
            Ok(match check()? {
                Some(found) => Checked::Found(found),
                None => Checked::NotYet { recheck_at: None },
            })
        })
        .await
    }

    /// [`Self::wait_for`] for a check that can also tell when what it looks
    /// at changes without a notification (a lease running out): it is run
    /// again then, too.
    pub(super) async fn wait_for_or_until<T>(
        &self,
        wait: std::time::Duration,
        only: Option<&TaskId>,
        mut check: impl FnMut() -> ProtocolResult<Checked<T>>,
    ) -> ProtocolResult<Option<T>> {
        let mut changes = self.changes.subscribe();
        // The installation's default harness, which every task sees (U7):
        // a runner hears of a change now rather than when its long-poll
        // runs out.
        let mut harness_changes = self.launch.default_harness_changes();
        let deadline = tokio::time::Instant::now() + wait;
        let mut recheck_at = None;
        let mut relevant = true;
        loop {
            if relevant {
                match check()? {
                    Checked::Found(found) => return Ok(Some(found)),
                    Checked::NotYet { recheck_at: at } => recheck_at = at,
                }
            }
            let wake_at = recheck_at.map_or(deadline, |at: tokio::time::Instant| at.min(deadline));
            relevant = tokio::select! {
                received = tokio::time::timeout_at(wake_at, changes.recv()) => match received {
                    Err(_elapsed) if wake_at >= deadline => return Ok(None),
                    Err(_elapsed) => true,
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

/// What one check of a long-poll found.
pub(super) enum Checked<T> {
    /// Something to answer with.
    Found(T),
    /// Nothing yet; look again at the next notification, or at `recheck_at`
    /// if that comes first.
    NotYet {
        recheck_at: Option<tokio::time::Instant>,
    },
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
