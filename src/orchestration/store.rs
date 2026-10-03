//! Durable files of the control plane: task records, their event logs and
//! backend leases.
//!
//! ```text
//! {state}/orchestration/tasks/create.lock
//! {state}/orchestration/tasks/{task}/task.json         TaskRecord (atomic replace)
//! {state}/orchestration/tasks/{task}/task.lock         per-task mutation lock
//! {state}/orchestration/tasks/{task}/events/{seq}.json TaskEvent (exclusive create)
//! {state}/orchestration/backends/{backend}/lease.json  BackendLease
//! {state}/orchestration/backends/{backend}/lease.lock
//! ```
//!
//! # Commit protocol
//!
//! The record is the commit point. A mutation replaces `task.json`
//! atomically (temp + fsync + rename) with the new revision *and* that
//! revision's event embedded as `last_event`, then appends the same event to
//! the log with an exclusive create. A crash between the two leaves a record
//! one event ahead of its log, which [`OrchestrationStore::publish_last_event`]
//! repairs from the record itself — before the next mutation of that task and
//! during [`super::ControlPlane::recover`]. A crash before the rename leaves
//! the previous record complete plus an orphaned `.tmp` file, so a torn
//! record is never half-applied. Readers of the log see the missing event
//! too, synthesized from the record, so an SSE replay never skips it.

use super::error::{OrchestrationError, Result};
use super::model::{
    BackendLease, TaskEvent, TaskRecord, BACKEND_LEASE_VERSION, TASK_RECORD_VERSION,
};
use crate::domain::{BackendId, TaskId};
use crate::persistence::atomic::{
    cleanup_orphaned_temp_files, read_json, write_json_atomic, write_json_atomic_exclusive,
};
use crate::persistence::paths::{parse_event_file_name, StatePaths};
use crate::persistence::PersistenceError;
use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

/// The control plane's file store. Holds no state of its own beyond paths, so
/// any number of handles — in any number of threads — are safe: every
/// read-modify-write runs under an advisory lock.
#[derive(Debug, Clone)]
pub struct OrchestrationStore {
    paths: StatePaths,
}

impl OrchestrationStore {
    /// A store below the state root of `paths`.
    pub fn new(paths: StatePaths) -> Self {
        Self { paths }
    }

    /// The path layout.
    pub fn paths(&self) -> &StatePaths {
        &self.paths
    }

    /// True when a record for `task_id` exists.
    pub fn task_exists(&self, task_id: &TaskId) -> bool {
        self.paths
            .orchestration_task_file(task_id.as_str())
            .exists()
    }

    /// Loads a task record.
    pub fn load_task(&self, task_id: &TaskId) -> Result<TaskRecord> {
        let path = self.paths.orchestration_task_file(task_id.as_str());
        if !path.exists() {
            return Err(OrchestrationError::TaskNotFound(task_id.clone()));
        }
        let record: TaskRecord = read_json(&path)?;
        if record.record_version > TASK_RECORD_VERSION {
            return Err(OrchestrationError::UnsupportedRecordVersion {
                what: path.display().to_string(),
                version: record.record_version,
            });
        }
        Ok(record)
    }

    /// Writes a new record only if none exists. `Ok(false)` leaves the
    /// existing one untouched.
    pub(crate) fn create_task(&self, record: &TaskRecord) -> Result<bool> {
        let path = self.paths.orchestration_task_file(record.task_id.as_str());
        Ok(write_json_atomic_exclusive(&path, record)?)
    }

    /// Atomically replaces a record. Call only under the task's lock.
    pub(crate) fn save_task(&self, record: &TaskRecord) -> Result<()> {
        let path = self.paths.orchestration_task_file(record.task_id.as_str());
        Ok(write_json_atomic(&path, record)?)
    }

    /// Appends `record.last_event` to the log if it is not there yet.
    ///
    /// Returns whether it had to be written. An event file already present
    /// with *different* content means two writers disagree about history,
    /// which is refused rather than overwritten.
    pub(crate) fn publish_last_event(&self, record: &TaskRecord) -> Result<bool> {
        let event = &record.last_event;
        let path = self
            .paths
            .orchestration_task_event_file(record.task_id.as_str(), event.sequence);
        if write_json_atomic_exclusive(&path, event)? {
            return Ok(true);
        }
        let existing: TaskEvent = read_json(&path)?;
        if existing == *event {
            Ok(false)
        } else {
            Err(PersistenceError::EventAlreadyExists {
                sequence: event.sequence,
                path,
            }
            .into())
        }
    }

    /// Identifiers of every task with a record, sorted.
    ///
    /// A directory without `task.json` (a crash between creating the
    /// directory and linking the record in) or with a name that is not a
    /// valid identifier is not a task and is skipped.
    pub fn list_task_ids(&self) -> Result<Vec<TaskId>> {
        let dir = self.paths.orchestration_tasks_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(PersistenceError::io(&dir, source).into()),
        };
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| PersistenceError::io(&dir, source))?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Ok(task_id) = TaskId::parse(name) else {
                continue;
            };
            if entry.path().is_dir() && self.task_exists(&task_id) {
                ids.push(task_id);
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// Every task record, in creation order (`queue_sequence`).
    pub fn list_tasks(&self) -> Result<Vec<TaskRecord>> {
        let mut tasks = self
            .list_task_ids()?
            .iter()
            .map(|task_id| self.load_task(task_id))
            .collect::<Result<Vec<_>>>()?;
        tasks.sort_by_key(|task| task.queue_sequence);
        Ok(tasks)
    }

    /// The events of a task with a sequence greater than `after`, in order.
    ///
    /// `after = 0` reads the whole log, which must be gapless from 1. Takes
    /// no lock: the log is listed *before* the record is read, and the record
    /// is always committed before its event, so a concurrent mutation can
    /// only make the record newer than the listing — never the reverse. When
    /// the record is exactly one revision ahead (an interrupted publish, or
    /// one in flight) that event is taken from the record, without writing;
    /// anything newer still is simply returned to the next call of a
    /// cursor-following reader.
    pub fn events_since(&self, task_id: &TaskId, after: u64) -> Result<Vec<TaskEvent>> {
        if !self.task_exists(task_id) {
            return Err(OrchestrationError::TaskNotFound(task_id.clone()));
        }
        let dir = self.paths.orchestration_task_events_dir(task_id.as_str());
        let sequences = event_sequences(&dir)?;
        let record = self.load_task(task_id)?;
        for (index, sequence) in sequences.iter().copied().enumerate() {
            let expected = index as u64 + 1;
            if sequence != expected {
                return Err(PersistenceError::SequenceGap {
                    path: dir,
                    expected,
                    found: sequence,
                }
                .into());
            }
        }
        let on_disk = sequences.last().copied().unwrap_or(0);
        if on_disk > record.revision {
            // An event the record never committed: two writers disagree.
            return Err(PersistenceError::SequenceGap {
                path: dir,
                expected: record.revision,
                found: on_disk,
            }
            .into());
        }

        let mut events = Vec::new();
        for sequence in sequences.into_iter().filter(|sequence| *sequence > after) {
            let path = self
                .paths
                .orchestration_task_event_file(task_id.as_str(), sequence);
            let event: TaskEvent = read_json(&path)?;
            if event.sequence != sequence {
                return Err(PersistenceError::SequenceMismatch {
                    path,
                    expected: sequence,
                    declared: event.sequence,
                }
                .into());
            }
            events.push(event);
        }
        if record.revision == on_disk + 1 && record.revision > after {
            events.push(record.last_event);
        }
        Ok(events)
    }

    /// Loads the lease of a backend, if one was ever acquired.
    pub fn load_lease(&self, backend_id: &BackendId) -> Result<Option<BackendLease>> {
        let path = self.paths.backend_lease_file(backend_id.as_str());
        if !path.exists() {
            return Ok(None);
        }
        let lease: BackendLease = read_json(&path)?;
        if lease.record_version > BACKEND_LEASE_VERSION {
            return Err(OrchestrationError::UnsupportedRecordVersion {
                what: path.display().to_string(),
                version: lease.record_version,
            });
        }
        Ok(Some(lease))
    }

    /// Atomically replaces a backend's lease. Call only under its lock.
    pub(crate) fn save_lease(&self, lease: &BackendLease) -> Result<()> {
        let path = self.paths.backend_lease_file(lease.backend_id.as_str());
        Ok(write_json_atomic(&path, lease)?)
    }

    /// Removes orphaned `.tmp` files of interrupted writes anywhere below
    /// `orchestration/`. Each is the trace of a crash before a rename; the
    /// target still holds its last complete version.
    pub fn cleanup_orphaned_temp_files(&self) -> Result<Vec<PathBuf>> {
        let mut removed = cleanup_orphaned_temp_files(&self.paths.orchestration_tasks_dir())?;
        for task_dir in subdirectories(&self.paths.orchestration_tasks_dir())? {
            removed.extend(cleanup_orphaned_temp_files(&task_dir)?);
            removed.extend(cleanup_orphaned_temp_files(
                &task_dir.join(crate::persistence::paths::EVENTS_DIR),
            )?);
        }
        for backend_dir in subdirectories(&self.paths.orchestration_backends_dir())? {
            removed.extend(cleanup_orphaned_temp_files(&backend_dir)?);
        }
        Ok(removed)
    }

    /// Exclusive lock for mutating one existing task.
    pub(crate) fn lock_task(&self, task_id: &TaskId) -> Result<FileLock> {
        FileLock::acquire(self.paths.orchestration_task_lock_file(task_id.as_str()))
    }

    /// Exclusive lock for creating tasks.
    pub(crate) fn lock_creation(&self) -> Result<FileLock> {
        FileLock::acquire(self.paths.orchestration_create_lock_file())
    }

    /// Exclusive lock for one backend's lease.
    pub(crate) fn lock_backend(&self, backend_id: &BackendId) -> Result<FileLock> {
        FileLock::acquire(self.paths.backend_lease_lock_file(backend_id.as_str()))
    }
}

/// Sequence numbers of the event files in `dir`, ascending; `.tmp` leftovers
/// and foreign files are ignored.
fn event_sequences(dir: &Path) -> Result<Vec<u64>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(PersistenceError::io(dir, source).into()),
    };
    let mut sequences = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| PersistenceError::io(dir, source))?;
        if let Some(sequence) = entry.file_name().to_str().and_then(parse_event_file_name) {
            sequences.push(sequence);
        }
    }
    sequences.sort_unstable();
    Ok(sequences)
}

fn subdirectories(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(PersistenceError::io(dir, source).into()),
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| PersistenceError::io(dir, source))?;
        if entry.path().is_dir() {
            dirs.push(entry.path());
        }
    }
    Ok(dirs)
}

/// An exclusive `flock(2)` held until dropped.
///
/// Advisory locks rather than create-new marker files: the kernel releases
/// them when the holder dies, so a crashed control plane can never strand a
/// task. Each acquisition opens its own file description, so the lock also
/// serializes threads of one process, not only processes.
#[derive(Debug)]
pub(crate) struct FileLock {
    file: fs::File,
}

impl FileLock {
    fn acquire(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| PersistenceError::io(parent, source))?;
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| PersistenceError::io(&path, source))?;
        loop {
            // SAFETY: `file` owns a valid descriptor for the duration of the call.
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if result == 0 {
                return Ok(Self { file });
            }
            let source = std::io::Error::last_os_error();
            if source.kind() != std::io::ErrorKind::Interrupted {
                return Err(OrchestrationError::Lock { path, source });
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: the descriptor is still owned by `self.file`. Closing it
        // would release the lock anyway; unlocking first just makes it prompt.
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}
