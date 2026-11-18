//! The observer worker's long-poll claim (U6): an entry, exit or `check`
//! script starts as soon as it is due, not at the worker's next poll.
//!
//! ```text
//! claim     a run is due                    → that run, at once (hook runs
//!                                             before `check` runs)
//!           none is due                     → wait for a wake or the limit
//!                                             (ProtocolConfig::observer_claim_wait),
//!                                             then look again
//!           the limit passed, none due      → nothing
//!           waiting claims released         → nothing, at once (shutdown)
//! wake      a task's gate was entered or    → its entry or exit scripts
//!           left, an exit was held (an        may be due
//!           answer, an operator command),
//!           or it was archived
//!           an entry or exit script attempt → the task's next one may be due
//!           ended
//!           a run's result was recorded     → its pair is free again
//!           the periodic pass ended         → ticks it flagged, runs it lost,
//!                                             retry times and deadlines
//!                                             that came meanwhile
//! ```
//!
//! The wake is a `watch` channel, not a `Notify`: a claim subscribes before
//! its first look, so a wake that lands while it looks is never lost, and
//! the channel's value says whether claims were released for good. Wakes
//! coalesce: many changes during one look cost one more look.
//!
//! Time-based due points (the next tick, a hook's retry time, a run's
//! deadline) wake no claim by themselves: the periodic pass wakes it within
//! its interval (5 s). A tick becomes claimable only through the pass
//! anyway, and finding the earliest retry time would cost as much as a
//! claim.

use std::sync::Arc;

use super::schedule::ClaimedObserverRun;
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::ProtocolService;

impl ProtocolService {
    /// Claims the next due run like [`ProtocolService::claim_observer_run`],
    /// waiting for one up to [`ProtocolConfig::observer_claim_wait`]; `None`
    /// when none became due by then, or when waiting claims were released.
    ///
    /// Each look runs off the async workers: it reads files under locks.
    ///
    /// [`ProtocolConfig::observer_claim_wait`]: crate::controller_protocol::service::ProtocolConfig::observer_claim_wait
    pub async fn claim_observer_run_waiting(
        self: Arc<Self>,
    ) -> ProtocolResult<Option<ClaimedObserverRun>> {
        // Before the first look: a wake during a look is seen after it.
        let mut wakes = self.observer_claims.subscribe();
        let deadline = tokio::time::Instant::now() + self.config.observer_claim_wait;
        loop {
            if *wakes.borrow_and_update() {
                return Ok(None);
            }
            let service = Arc::clone(&self);
            let claimed = tokio::task::spawn_blocking(move || service.claim_observer_run())
                .await
                .map_err(|error| ProtocolError::Internal(error.to_string()))??;
            if claimed.is_some() || tokio::time::Instant::now() >= deadline {
                return Ok(claimed);
            }
            tokio::select! {
                changed = wakes.changed() => {
                    if changed.is_err() {
                        return Ok(None);
                    }
                }
                () = tokio::time::sleep_until(deadline) => {}
            }
        }
    }

    /// Wakes waiting claims: a run may have become due.
    pub fn wake_observer_claims(&self) {
        self.observer_claims.send_modify(|_| {});
    }

    /// Releases waiting claims at once, and every later claim answers at
    /// once: the server stops.
    pub fn release_observer_claims(&self) {
        self.observer_claims.send_replace(true);
    }
}
