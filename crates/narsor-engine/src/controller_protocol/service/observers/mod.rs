//! The control plane's side of catalogue automations: the schedule that
//! hands runs of each (queue, automation) pair to the worker
//! ([`schedule`]), the effects a run's script has through its token
//! ([`effects`]), and the read-only queue views ([`queues`]). The worker
//! that claims runs is the server's own: its key lives in the server's
//! memory, not here.
//!
//! What a queue or an automation does is a catalogue file, read on every
//! pass with no cache (R2); nothing here changes it. Every mutation of a
//! pair's runtime record runs under the pair's lock and saves the record
//! once; the record is the commit point (see [`crate::observers::store`]).
//! Who may call these methods is the HTTP layer's business: the views are
//! the owner's, the claim and the result the worker's.
//!
//! The entries of queues are the wait episodes of `decision` gates with a
//! `watch` block ([`crate::observers::QueueEntry`]): the runner's snapshot
//! adds and removes them ([`entries`]), and the schedule hands them to the
//! pairs of their `on_episodes` queue.

mod effects;
mod entries;
mod queues;
mod schedule;
#[cfg(test)]
mod tests;

pub use effects::MAX_EFFECT_DATA_BYTES;
pub use schedule::{
    retry_delay_secs, ClaimedObserverRun, ObserverPass, RunOutcome, RunReport,
    MAX_CONSECUTIVE_FAILURES, MAX_RETRY_DELAY_SECS, MAX_STORED_LOG_BYTES, RUN_DEADLINE_MARGIN_SECS,
    SCRIPT_TIMEOUT_SECS,
};
