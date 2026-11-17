//! The entries of observer queues, kept in step with the wait episodes of
//! `decision` gates with a `watch` block (R16–R19, KTD-5), and what the
//! task view reads of them (R21).
//!
//! ```text
//! runner snapshot  watched episode ended        → its entry removed
//!                  watched episode began        → its entry added, after the removal,
//!                                                 under the same lock
//!                    added, and no active on_episodes observer of the queue
//!                                               → warning in the task's history
//!                    refused or not stored      → warning in the task's history
//!                  same episode (pause, Interrupted, auto-resume, runner restart)
//!                                               → nothing (R17)
//! owner's archive  the entry of the task's episode removed (R19)
//! schedule         an entry whose task no longer waits in its episode, or ended,
//!                  was archived or is gone      → removed, never run (R18, R19)
//! ```
//!
//! The warning is written once per episode: only the snapshot that begins
//! the episode adds the entry, and an entry id is the task's and the
//! episode's, so an entry is created at most once per episode
//! ([`EntryWrite::Created`]). A redelivered or a later snapshot of the same
//! episode finds the episode known and changes nothing.

use crate::controller_protocol::service::ProtocolService;
use crate::observers::{
    queue_entry_id, EntryWrite, ObserverError, ObserverMode, ObserverState, QueueEntry,
};
use crate::orchestration::{
    GateChange, OrchestrationError, OwnerRef, TaskRecord, TaskState, WatchedGate,
};

/// Whether a queue entry's task still waits in the entry's episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EntryWait {
    /// It does: a script may answer.
    Waits,
    /// It does not: the gate exited, or the task ended, was archived or is
    /// gone. The entry can never matter again.
    Gone,
    /// The task could not be read: the entry is kept, but not run now.
    Unknown,
}

impl ProtocolService {
    /// Applies the watched gate episodes a runner snapshot of `task` ended
    /// and began to the queue entries (R16): the ended episode's entry is
    /// removed first, then the new episode's entry is added, under one lock
    /// of the queue entries. A snapshot that changed no episode changes
    /// nothing (R17).
    ///
    /// Nothing here refuses the snapshot: a failure is logged, and for a new
    /// episode written to the task's history as a warning, like a queue that
    /// no active `on_episodes` observer serves (R9, R27). The gate waits for
    /// a person either way.
    pub fn follow_gate_episode(&self, task: &TaskRecord, change: &GateChange) {
        if change.is_empty() {
            return;
        }
        let warning = match self.swap_queue_entries(task, change) {
            Ok(warning) => warning,
            Err(error) => {
                tracing::warn!(task = %task.task_id, %error, "observers: cannot update the queue entries of a gate");
                // The error names server paths; the history gets the fact.
                Some("the queue entry could not be stored; the server log has the cause".into())
            }
        };
        let (Some(reason), Some(gate)) = (warning, change.began.as_ref()) else {
            return;
        };
        match self
            .control
            .record_gate_queue_warning(&task.task_id, gate, &reason)
        {
            Ok(()) => self.notify(&task.task_id),
            Err(error) => {
                tracing::warn!(task = %task.task_id, %error, "observers: cannot record a queue warning");
            }
        }
    }

    /// Removes the queue entry of the task's watched episode, if it has
    /// one: an archived task's gate can never be answered (R19).
    pub(crate) fn drop_queue_entry_of(&self, task: &TaskRecord) {
        let Some(gate) = &task.watched_gate else {
            return;
        };
        let entry_id = queue_entry_id(&task.task_id, &gate.episode);
        let removed = self
            .observers
            .lock_queue_entries()
            .and_then(|lock| self.observers.remove_queue_entry(&lock, &entry_id));
        if let Err(error) = removed {
            tracing::warn!(task = %task.task_id, %error, "observers: cannot remove the queue entry of an archived task");
        }
    }

    /// The queue entry of the episode `task` waits in now, if the control
    /// plane has one: the source of the task view's queue mark (R21). One
    /// read of one entry, and none for a task at no watched gate. `None`
    /// for an ended or archived task, whose entry is about to go.
    pub fn task_queue_entry(&self, task: &TaskRecord) -> Option<QueueEntry> {
        let gate = task.watched_gate.as_ref()?;
        if task_ended(task) {
            return None;
        }
        let entry_id = queue_entry_id(&task.task_id, &gate.episode);
        match self.observers.queue_entry(&entry_id) {
            Ok(entry) => entry.filter(|entry| entry.owner == task.owner),
            Err(error) => {
                tracing::warn!(task = %task.task_id, %error, "observers: cannot read the queue entry of a task");
                None
            }
        }
    }

    /// Whether `entry`'s task still waits in the entry's episode (R18).
    pub(super) fn entry_wait(&self, entry: &QueueEntry) -> EntryWait {
        match self.control.task(&entry.task_id) {
            Ok(task) => {
                let waits = !task_ended(&task)
                    && task.owner == entry.owner
                    && task
                        .watched_gate
                        .as_ref()
                        .is_some_and(|gate| gate.episode == entry.episode);
                if waits {
                    EntryWait::Waits
                } else {
                    EntryWait::Gone
                }
            }
            Err(OrchestrationError::TaskNotFound(_)) => EntryWait::Gone,
            Err(error) => {
                tracing::warn!(task = %entry.task_id, %error, "observers: cannot read the task of a queue entry");
                EntryWait::Unknown
            }
        }
    }

    /// The removal and the addition of [`Self::follow_gate_episode`];
    /// `Ok(Some(reason))` when the new episode deserves a warning.
    fn swap_queue_entries(
        &self,
        task: &TaskRecord,
        change: &GateChange,
    ) -> Result<Option<String>, ObserverError> {
        let lock = self.observers.lock_queue_entries()?;
        if let Some(ended) = &change.ended {
            let entry_id = queue_entry_id(&task.task_id, &ended.episode);
            match self.observers.remove_queue_entry(&lock, &entry_id) {
                Ok(true) => {
                    tracing::info!(task = %task.task_id, queue = %ended.watch.queue, episode = %ended.episode, "observers: the gate exited; its queue entry is removed");
                }
                Ok(false) => {}
                // The new episode's entry is still worth adding: the old
                // one can never be run, the guard before a run drops it.
                Err(error) => {
                    tracing::warn!(task = %task.task_id, %error, "observers: cannot remove the queue entry of an ended episode");
                }
            }
        }
        let Some(began) = &change.began else {
            return Ok(None);
        };
        let entry = QueueEntry::from_gate(
            task.owner.clone(),
            task.task_id.clone(),
            began.episode.clone(),
            &began.watch,
            self.control.now(),
        );
        if let Err(error) = entry.validate() {
            return Ok(Some(format!("the queue entry was refused: {error}")));
        }
        match self.observers.add_queue_entry(&lock, &entry)? {
            EntryWrite::Exists(_) => Ok(None),
            EntryWrite::Created => {
                drop(lock);
                tracing::info!(task = %task.task_id, queue = %began.watch.queue, episode = %began.episode, "observers: a gate added a queue entry");
                self.queue_unserved(&task.owner, began)
            }
        }
    }

    /// Why no observer of `owner`'s queue of `gate` can answer it, `None`
    /// when an active `on_episodes` observer serves the queue (R9, R27).
    fn queue_unserved(
        &self,
        owner: &OwnerRef,
        gate: &WatchedGate,
    ) -> Result<Option<String>, ObserverError> {
        let queue = &gate.watch.queue;
        let observers = self.observers.queue_observers(owner, queue)?;
        let on_episodes = || {
            observers
                .iter()
                .filter(|record| record.config.mode == ObserverMode::OnEpisodes)
        };
        let reason = if on_episodes().any(|record| record.state == ObserverState::Active) {
            return Ok(None);
        } else if observers.is_empty() {
            format!("queue '{queue}' has no observer; the gate waits for a person")
        } else if on_episodes().next().is_none() {
            format!(
                "the observers of queue '{queue}' run in mode always and cannot answer a gate; the gate waits for a person"
            )
        } else {
            format!("no observer of queue '{queue}' is active; the gate waits for a person")
        };
        Ok(Some(reason))
    }
}

/// Whether `task` can take no more answers: finished, archived (or being
/// archived).
fn task_ended(task: &TaskRecord) -> bool {
    task.archive.is_some()
        || task.runner_state.is_archival()
        || task.task_state() == TaskState::Finished
}
