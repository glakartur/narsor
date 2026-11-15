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
//! A `wait` step with a `watch` block adds a watch entry when its wait
//! begins ([`ProtocolService::register_wait_watch`], R8):
//!
//! ```text
//! no observer of that name and the task's owner   → warning in the task's history
//! observer without a watch list (watch_mode: all)  → warning
//! object empty, too long or under no allowed prefix → warning
//! entry for (task, step, object) already there     → left as it is
//! otherwise                                        → entry added (origin: workflow)
//! ```
//!
//! A warning leaves the task waiting: the owner may add the entry by hand.

mod effects;
#[cfg(test)]
mod effects_tests;
mod owner;
mod schedule;
#[cfg(test)]
mod tests;

pub use effects::{MAX_EFFECT_DATA_BYTES, MAX_EVENT_DATA_BYTES};
pub use schedule::{
    retry_delay_secs, ClaimedObserverRun, ObserverPass, RunOutcome, RunReport,
    MAX_CONSECUTIVE_FAILURES, MAX_RETRY_DELAY_SECS, MAX_STORED_LOG_BYTES, RUN_DEADLINE_MARGIN_SECS,
    SCRIPT_TIMEOUT_SECS,
};

use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::ProtocolService;
use crate::domain::TaskId;
use crate::observers::{
    ObserverError, ObserverId, ObserverRecord, ObserverState, WatchEntry, WatchMode, WatchOrigin,
    WatchWrite,
};
use crate::orchestration::{EventWatch, OwnerRef};

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
    /// count of failed runs.
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
        self.observers.save(&lock, &mut record, now)?;
        Ok(record)
    }
}

impl ProtocolService {
    /// Adds the watch entry a `wait` step's `watch` block asks for, when
    /// the task `task_id` of `owner` begins to wait (R8). Idempotent by
    /// task, step and object, so a review loop that waits again keeps one
    /// entry.
    ///
    /// Only an observer of the task's owner may watch for the task: an
    /// event of any other observer could not reach it. An observer of
    /// another owner is reported like a missing one, so a workflow learns
    /// nothing of another owner's observers. Every refusal is a warning in
    /// the task's history; the task keeps waiting.
    pub(crate) fn register_wait_watch(
        &self,
        task_id: &TaskId,
        owner: &OwnerRef,
        watch: &EventWatch,
    ) {
        let refusal = match self.add_wait_watch(task_id, owner, watch) {
            Ok(None) => return,
            Ok(Some(reason)) => reason,
            Err(error) => {
                tracing::warn!(task = %task_id, observer = %watch.observer, %error, "observers: cannot add the watch entry of a wait step");
                // The error names server paths; the history gets no more
                // than the fact.
                "the watch entry could not be stored; the server log has the cause".to_string()
            }
        };
        match self.control.record_watch_refused(task_id, watch, &refusal) {
            Ok(()) => self.notify(task_id),
            Err(error) => {
                tracing::warn!(task = %task_id, %error, "observers: cannot record a refused watch entry");
            }
        }
    }

    /// The entry of [`Self::register_wait_watch`]: `Ok(None)` when it is
    /// there now, `Ok(Some(reason))` when it is refused.
    fn add_wait_watch(
        &self,
        task_id: &TaskId,
        owner: &OwnerRef,
        watch: &EventWatch,
    ) -> Result<Option<String>, ObserverError> {
        let not_found = || {
            Ok(Some(format!(
                "no observer named '{}' belongs to the task's owner",
                watch.observer
            )))
        };
        let Some(found) = self.observers.find_by_name(&watch.observer)? else {
            return not_found();
        };
        let lock = match self.observers.lock(&found.observer_id) {
            Ok(lock) => lock,
            Err(ObserverError::NotFound(_)) => return not_found(),
            Err(error) => return Err(error),
        };
        // Read again under the lock: the observer may have changed or gone.
        let record = match self.observers.load(&found.observer_id) {
            Ok(record) => record,
            Err(ObserverError::NotFound(_)) => return not_found(),
            Err(error) => return Err(error),
        };
        if record.config.name.as_str() != watch.observer || &record.owner != owner {
            return not_found();
        }
        if record.config.watch_mode != WatchMode::WatchList {
            return Ok(Some(format!(
                "observer '{}' keeps no watch list (its watch mode is not watch_list)",
                watch.observer
            )));
        }
        if let Err(error) = record.config.watch_objects.check(&watch.object) {
            return Ok(Some(error.to_string()));
        }
        let entry = WatchEntry::new(
            task_id.clone(),
            Some(watch.step.clone()),
            watch.object.as_str(),
            watch.event_types.clone(),
            WatchOrigin::Workflow,
            self.control.now(),
        );
        if let Err(error) = entry.validate() {
            return Ok(Some(error.to_string()));
        }
        match self.observers.create_watch(&lock, &entry)? {
            WatchWrite::Created => {
                tracing::info!(task = %task_id, observer = %watch.observer, entry = %entry.entry_id, "observers: a wait step added a watch entry");
            }
            WatchWrite::Exists(_) => {}
        }
        Ok(None)
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
