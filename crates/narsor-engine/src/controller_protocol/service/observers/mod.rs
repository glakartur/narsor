//! The control plane's side of observers: what the owner decides about an
//! observer (approval, pause and resume; the rest of the owner's
//! commands, by name and owner, in [`owner`]), the schedule that
//! hands runs to the worker ([`schedule`]) and the effects a run's script
//! has through its token ([`effects`]). The worker that claims runs is the
//! server's own: its key lives in the server's memory, not here.
//!
//! Every mutation runs under the observer's lock and saves the record once;
//! the record is the commit point (see [`crate::observers::store`]). Who may
//! call these methods is the HTTP layer's business: they are the owner's
//! (R21), the claim and the result the worker's.
//!
//! The entries of observer queues are the wait episodes of `decision`
//! gates with a `watch` block ([`crate::observers::QueueEntry`]): the
//! runner's snapshot adds and removes them ([`entries`]), and the schedule
//! hands them to the `on_episodes` observers of their queue.

mod effects;
mod entries;
mod owner;
mod schedule;
#[cfg(test)]
mod tests;

pub use effects::MAX_EFFECT_DATA_BYTES;
pub use schedule::{
    retry_delay_secs, ClaimedObserverRun, ObserverPass, RunOutcome, RunReport,
    MAX_CONSECUTIVE_FAILURES, MAX_RETRY_DELAY_SECS, MAX_STORED_LOG_BYTES, RUN_DEADLINE_MARGIN_SECS,
    SCRIPT_TIMEOUT_SECS,
};

use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::ProtocolService;
use crate::observers::{ObserverId, ObserverRecord, ObserverState};

impl ProtocolService {
    /// Approves script version `version`: from now on the schedule runs it,
    /// and only it (R11).
    ///
    /// A draft observer becomes active and is due at once; an observer in
    /// any other state keeps its state, so approving a new version of a
    /// paused observer leaves it paused. Saving a script never approves it
    /// (AE6). The version must be readable and match its digest.
    pub fn approve_observer_version(
        &self,
        observer_id: &ObserverId,
        version: u32,
    ) -> ProtocolResult<ObserverRecord> {
        let lock = self.observers.lock(observer_id)?;
        let mut record = self.observers.load(observer_id)?;
        self.observers.load_script(observer_id, version)?;
        let now = self.control.now();
        record.approved_version = Some(version);
        if record.state == ObserverState::Draft {
            record.state = ObserverState::Active;
            record.next_run_at.get_or_insert(now);
        }
        self.observers.save(&lock, &mut record, now)?;
        Ok(record)
    }

    /// Stops the schedule of an observer (R18). A run in progress finishes;
    /// a scheduled run not claimed yet is dropped. Pausing a paused
    /// observer changes nothing.
    pub fn pause_observer(&self, observer_id: &ObserverId) -> ProtocolResult<ObserverRecord> {
        let lock = self.observers.lock(observer_id)?;
        let mut record = self.observers.load(observer_id)?;
        match record.state {
            ObserverState::Paused => return Ok(record),
            ObserverState::Active | ObserverState::NeedsAttention | ObserverState::Error => {}
            ObserverState::Draft | ObserverState::Unrecognized => {
                return Err(not_schedulable(&record));
            }
        }
        record.state = ObserverState::Paused;
        record.run_due_since = None;
        self.observers
            .save(&lock, &mut record, self.control.now())?;
        Ok(record)
    }

    /// Puts a paused observer, or one waiting for its owner
    /// (`needs_attention`, `error`), back on its schedule, and clears its
    /// count of failed runs and its note.
    ///
    /// An observer the owner had to look at is due at once; a paused one
    /// keeps its next deadline, so a resume right after a pause does not
    /// run it early. Resuming an active observer changes nothing.
    pub fn resume_observer(&self, observer_id: &ObserverId) -> ProtocolResult<ObserverRecord> {
        let lock = self.observers.lock(observer_id)?;
        let mut record = self.observers.load(observer_id)?;
        let now = self.control.now();
        match record.state {
            ObserverState::Active => return Ok(record),
            ObserverState::Paused => {
                record.next_run_at.get_or_insert(now);
            }
            ObserverState::NeedsAttention | ObserverState::Error => {
                record.next_run_at = Some(now);
            }
            ObserverState::Draft | ObserverState::Unrecognized => {
                return Err(not_schedulable(&record));
            }
        }
        record.state = if record.approved_version.is_some() {
            ObserverState::Active
        } else {
            ObserverState::Draft
        };
        record.consecutive_failures = 0;
        record.note = None;
        self.observers.save(&lock, &mut record, now)?;
        Ok(record)
    }
}

/// The refusal of a schedule command for an observer that has no schedule
/// to stop or resume: a draft, or a state from a newer build.
fn not_schedulable(record: &ObserverRecord) -> ProtocolError {
    let name = &record.config.name;
    if record.state == ObserverState::Unrecognized {
        return ProtocolError::conflict(
            "observer_state_unrecognized",
            format!("observer '{name}' is in a state this server does not know"),
        );
    }
    ProtocolError::conflict(
        "observer_not_approved",
        format!("observer '{name}' has no approved script version: approve one first"),
    )
}
