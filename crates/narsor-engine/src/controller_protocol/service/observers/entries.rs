//! The entries of observer queues, kept in step with the wait episodes of
//! `decision` gates with a `watch` block (R16–R19, KTD-5), and what the
//! task view reads of them (R21).
//!
//! ```text
//! runner snapshot  watched episode ended        → its entry removed
//!                  watched episode began        → its entry added, after the removal,
//!                                                 under the same lock
//!                    added, and the catalogue has no usable on_episodes queue
//!                    of that name                → warning in the task's history
//!                    refused or not stored      → warning in the task's history
//!                  same episode (pause, Interrupted, auto-resume, runner restart)
//!                                               → nothing (R17)
//! owner's archive  the entry of the task's episode removed (R19)
//! schedule         an entry whose task no longer waits in its episode, or ended,
//!                  was archived or is gone      → removed, never run (R18, R19)
//!                  an entry whose task waits, but whose runner reports a problem
//!                  or is starting (no pending input)
//!                                               → kept, not run (R17)
//!                  a task of the queue that waits in its watched episode with
//!                  no entry, or with an entry file that cannot be parsed
//!                                               → entry added back (or rewritten), no
//!                                                 warning
//!                  an unreadable entry file of an episode no task waits in
//!                                               → removed
//! ```
//!
//! The task's record and the entry are two writes: a control plane that
//! stops between them leaves a waiting task without its entry, and no later
//! snapshot adds it (the episode is known). The tick of a pair of the
//! `on_episodes` queue adds it back ([`ProtocolService::restore_queue_entries`]).
//!
//! The warning is written once per episode: only the snapshot that begins
//! the episode adds the entry, and an entry id is the task's and the
//! episode's, so an entry is created at most once per episode
//! ([`EntryWrite::Created`]). A redelivered or a later snapshot of the same
//! episode finds the episode known and changes nothing.

use std::collections::HashSet;

use crate::controller_protocol::service::ProtocolService;
use crate::domain::TaskId;
use crate::observers::{queue_entry_id, EntryWrite, ObserverError, ObserverMode, QueueEntry};
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
    /// The task could not be read, or its runner cannot take an answer now
    /// (it reports a problem, or is starting): the entry is kept, but not
    /// run now.
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
                if !waits_in(&task, &entry.owner, &entry.episode) {
                    EntryWait::Gone
                } else if gate_takes_answer(&task, &entry.episode) {
                    EntryWait::Waits
                } else {
                    tracing::debug!(task = %entry.task_id, episode = %entry.episode, "observers: the runner of a waiting task cannot take an answer now; its queue entry is not run");
                    EntryWait::Unknown
                }
            }
            Err(OrchestrationError::TaskNotFound(_)) => EntryWait::Gone,
            Err(error) => {
                tracing::warn!(task = %entry.task_id, %error, "observers: cannot read the task of a queue entry");
                EntryWait::Unknown
            }
        }
    }

    /// Adds back the entries of queue `queue` that a task waiting in its
    /// watched episode has none of, or whose file cannot be parsed; returns
    /// how many. Called at the tick of a pair of the `on_episodes` queue, so
    /// its cost (one read of the task records) comes at most once per tick
    /// and pair.
    ///
    /// Each entry is added under the lock of the queue entries after the
    /// task is read again, so it never comes back for an episode a snapshot
    /// ended meanwhile. No warning: the snapshot that began the episode
    /// wrote the one there is. Failures are logged; the next tick tries
    /// again.
    pub(super) fn restore_queue_entries(&self, queue: &str) -> usize {
        let tasks = match self.control.list_tasks() {
            Ok(tasks) => tasks,
            Err(error) => {
                tracing::warn!(%error, "observers: cannot list the tasks for the queue entries");
                return 0;
            }
        };
        let missing: Vec<TaskRecord> = tasks
            .into_iter()
            .filter(|task| {
                task.watched_gate.as_ref().is_some_and(|gate| {
                    gate.watch.queue == queue && waits_in(task, &task.owner, &gate.episode)
                })
            })
            .filter(|task| self.entry_needs_write(task))
            .collect();
        let mut restored = 0;
        for task in missing {
            match self.restore_queue_entry(&task.task_id) {
                Ok(true) => restored += 1,
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(task = %task.task_id, %error, "observers: cannot add back a queue entry");
                }
            }
        }
        restored
    }

    /// Whether the entry of the episode `task` waits in must be written: it
    /// is missing, or its file cannot be parsed. A file of a newer version,
    /// or one the filesystem cannot read, is left alone: a write at every
    /// tick would fail the same way.
    fn entry_needs_write(&self, task: &TaskRecord) -> bool {
        let Some(gate) = &task.watched_gate else {
            return false;
        };
        match self
            .observers
            .queue_entry(&queue_entry_id(&task.task_id, &gate.episode))
        {
            Ok(entry) => entry.is_none(),
            Err(error) => error.is_corrupt_file(),
        }
    }

    /// Removes the unreadable entry files `entry_ids` of the episodes no
    /// task waits in now; returns how many went. Such a file names no task
    /// a read can see, so its id is matched against the entry id of the
    /// episode of every waiting task, of every owner. The file of a task
    /// that waits stays ([`Self::restore_queue_entries`] rewrites a corrupt
    /// one; one of a newer version is another build's). Removing one loses
    /// nothing: should its task wait after all, a tick adds its entry back.
    pub(super) fn prune_unreadable_entries(&self, entry_ids: &[String]) -> usize {
        if entry_ids.is_empty() {
            return 0;
        }
        let lock = match self.observers.lock_queue_entries() {
            Ok(lock) => lock,
            Err(error) => {
                tracing::warn!(%error, "observers: cannot lock the queue entries to remove unreadable ones");
                return 0;
            }
        };
        let tasks = match self.control.list_tasks() {
            Ok(tasks) => tasks,
            Err(error) => {
                tracing::warn!(%error, "observers: cannot list the tasks for the unreadable queue entries");
                return 0;
            }
        };
        let waiting: HashSet<String> = tasks
            .iter()
            .filter_map(|task| {
                let gate = task.watched_gate.as_ref()?;
                waits_in(task, &task.owner, &gate.episode)
                    .then(|| queue_entry_id(&task.task_id, &gate.episode))
            })
            .collect();
        let mut removed = 0;
        for entry_id in entry_ids.iter().filter(|id| !waiting.contains(*id)) {
            // Read again under the lock: a file rewritten meanwhile is kept.
            if self.observers.queue_entry(entry_id).is_ok() {
                continue;
            }
            match self.observers.remove_queue_entry(&lock, entry_id) {
                Ok(true) => {
                    removed += 1;
                    tracing::warn!(entry = %entry_id, "observers: an unreadable queue entry of an episode no task waits in is removed");
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(entry = %entry_id, %error, "observers: cannot remove an unreadable queue entry");
                }
            }
        }
        removed
    }

    /// Adds the entry of the episode `task_id` waits in now, if it has
    /// none, or rewrites it if its file cannot be parsed; `true` when it
    /// was written.
    fn restore_queue_entry(&self, task_id: &TaskId) -> Result<bool, ObserverError> {
        let lock = self.observers.lock_queue_entries()?;
        let task = match self.control.task(task_id) {
            Ok(task) => task,
            Err(OrchestrationError::TaskNotFound(_)) => return Ok(false),
            Err(error) => {
                tracing::warn!(task = %task_id, %error, "observers: cannot read a task for its queue entry");
                return Ok(false);
            }
        };
        let Some(gate) = task
            .watched_gate
            .as_ref()
            .filter(|gate| waits_in(&task, &task.owner, &gate.episode))
        else {
            return Ok(false);
        };
        let entry = QueueEntry::from_gate(
            task.owner.clone(),
            task.task_id.clone(),
            gate.episode.clone(),
            &gate.watch,
            gate.began_at.unwrap_or_else(|| self.control.now()),
        );
        if entry.validate().is_err() {
            return Ok(false);
        }
        match self.observers.add_queue_entry(&lock, &entry) {
            Ok(EntryWrite::Created) => {
                tracing::info!(task = %task.task_id, queue = %gate.watch.queue, episode = %gate.episode, "observers: a waiting task had no queue entry; it is added back");
                Ok(true)
            }
            Ok(EntryWrite::Exists(_)) => Ok(false),
            Err(error) if error.is_corrupt_file() => {
                let replaced = self.observers.replace_corrupt_queue_entry(&lock, &entry)?;
                if replaced {
                    tracing::warn!(task = %task.task_id, queue = %gate.watch.queue, episode = %gate.episode, %error, "observers: the queue entry of a waiting task could not be parsed; it is replaced by one rebuilt from the task");
                }
                Ok(replaced)
            }
            Err(error) => Err(error),
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
            began.began_at.unwrap_or_else(|| self.control.now()),
        );
        if let Err(error) = entry.validate() {
            return Ok(Some(format!("the queue entry was refused: {error}")));
        }
        match self.observers.add_queue_entry(&lock, &entry)? {
            EntryWrite::Exists(_) => Ok(None),
            EntryWrite::Created => {
                drop(lock);
                tracing::info!(task = %task.task_id, queue = %began.watch.queue, episode = %began.episode, "observers: a gate added a queue entry");
                Ok(self.queue_unserved(began))
            }
        }
    }

    /// Why no automation of the queue of `gate` can answer it, `None` when
    /// the catalogue has a usable `on_episodes` queue of that name (R9,
    /// R27).
    fn queue_unserved(&self, gate: &WatchedGate) -> Option<String> {
        let queue = &gate.watch.queue;
        let catalog = self.automation_catalog();
        let reason = match catalog.queues.get(queue) {
            Some(found) if found.mode == ObserverMode::OnEpisodes => return None,
            Some(_) => format!(
                "queue '{queue}' runs its automations in mode always, which cannot answer a \
                 gate; the gate waits for a person"
            ),
            None if catalog
                .problems
                .iter()
                .any(|problem| problem_names_queue(&problem.file, queue)) =>
            {
                format!(
                    "the catalogue file of queue '{queue}' cannot be used; the gate waits for a \
                     person"
                )
            }
            None => format!("queue '{queue}' is not in the catalogue; the gate waits for a person"),
        };
        Some(reason)
    }
}

/// Whether the catalogue file `file` (relative to the catalogue root) is the
/// file of queue `queue`.
pub(super) fn problem_names_queue(file: &std::path::Path, queue: &str) -> bool {
    file.parent() == Some(std::path::Path::new("queues"))
        && file.file_stem().and_then(|stem| stem.to_str()) == Some(queue)
}

/// Whether `task` of `owner` still waits at its watched gate in `episode`:
/// not ended, and the episode is the one its last runner snapshot shows.
pub(super) fn waits_in(task: &TaskRecord, owner: &OwnerRef, episode: &str) -> bool {
    !task_ended(task)
        && &task.owner == owner
        && task
            .watched_gate
            .as_ref()
            .is_some_and(|gate| gate.episode == episode)
}

/// Whether the runner of `task` can take an answer in `episode` now: its
/// last snapshot shows the episode's pending input. A runner that reports
/// a problem, or that is still starting, shows none while the episode goes
/// on (R17).
pub(super) fn gate_takes_answer(task: &TaskRecord, episode: &str) -> bool {
    task.runner
        .as_ref()
        .and_then(|runner| runner.snapshot.pending_input.as_ref())
        .is_some_and(|pending| pending.id == episode)
}

/// Whether `task` can take no more answers: finished, archived (or being
/// archived).
fn task_ended(task: &TaskRecord) -> bool {
    task.archive.is_some()
        || task.runner_state.is_archival()
        || task.task_state() == TaskState::Finished
}
