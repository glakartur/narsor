//! Durable files of catalogue automations: the runtime record of each
//! (queue, automation) pair, and the queue entries.
//!
//! ```text
//! {state}/observer-queues/automations/{queue}/{automation}/
//!     state.json          AutomationRecord (atomic replace)
//!     state.lock          per-pair mutation lock
//!     errors/{seq:08}.json
//!                         RunFailure of a failed or lost run (atomic replace;
//!                         the newest 20 kept, the newest one per queue entry)
//! {state}/observer-queues/entries.lock
//!                         additions and removals of queue entries
//! {state}/observer-queues/entries/{entry}.json
//!                         QueueEntry (exclusive create, never changed unless
//!                         corrupt)
//! ```
//!
//! The pair records are not under `{state}/automations/`: by default the
//! catalogue shares the state root, and its `automations/` holds the
//! automation definitions.
//!
//! # Locks and commit protocol
//!
//! Mirrors [`crate::orchestration::store`]: every read-modify-write runs
//! under the pair's advisory `flock(2)`, which the kernel releases when the
//! holder dies, and every file is replaced atomically (temp + fsync +
//! rename) or created exclusively (hard link, `EEXIST` instead of
//! clobbering). Methods that mutate a pair's files take the
//! [`AutomationLock`] as proof that the caller holds it, and methods that
//! add or remove queue entries take the [`QueueEntriesLock`]. The order is
//! fixed: a pair's lock, then `entries.lock`; never the other way round.
//!
//! # Records
//!
//! The configuration of a pair is the catalogue's; the record holds only
//! what the schedule learns at run time. It is created on first need (the
//! first save under the pair's lock) and deleted when the pair is no longer
//! in the catalogue and has no run in progress. A deletion removes the
//! record first, so a crash half way through leaves a directory without
//! `state.json`, which no listing reports and the next save reuses.
//!
//! A run in progress lives in the record only (`active_run`). A run leaves
//! a file only when it fails or is lost; a successful run changes the
//! record and nothing else.
//!
//! The records of the stored observers of older versions
//! (`{state}/observers/`) are never read here ([`super::migrate`]).

use super::model::{
    sha256_hex, AutomationKey, AutomationRecord, QueueEntry, RunFailure, AUTOMATION_RECORD_VERSION,
    QUEUE_ENTRY_VERSION, RUN_FAILURE_VERSION,
};
use super::{ObserverError, Result};
use crate::persistence::atomic::{
    cleanup_orphaned_temp_files, read_json, remove_file_if_exists, subdirectories,
    write_json_atomic, write_json_atomic_exclusive,
};
use crate::persistence::lock::{FileLock, FileLockError};
use crate::persistence::paths::{is_temp_file, parse_event_file_name, StatePaths};
use crate::persistence::PersistenceError;
use chrono::{DateTime, Utc};
use narsor_protocol::controller::OwnerRef;
use std::fs;
use std::path::{Path, PathBuf};

/// How many execution errors of a pair the store keeps: writing one more
/// deletes the oldest.
pub const KEPT_RUN_FAILURES: usize = 20;

/// Proof that the caller holds one pair's lock. Released when dropped.
#[derive(Debug)]
pub struct AutomationLock {
    key: AutomationKey,
    _lock: FileLock,
}

impl AutomationLock {
    /// The pair this lock is for.
    pub fn key(&self) -> &AutomationKey {
        &self.key
    }

    fn require(&self, key: &AutomationKey) -> Result<()> {
        if &self.key == key {
            Ok(())
        } else {
            Err(ObserverError::LockMismatch {
                lock: self.key.clone(),
                record: key.clone(),
            })
        }
    }
}

/// Proof that the caller holds the lock of the queue entries. Released when
/// dropped.
#[derive(Debug)]
pub struct QueueEntriesLock {
    _lock: FileLock,
}

/// What an exclusive write of a queue entry found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryWrite {
    /// The entry is new.
    Created,
    /// The entry already existed and was left untouched.
    Exists(QueueEntry),
}

/// The queue entries a scan of the entries directory read, oldest first,
/// and the ids of the entry files it could not read.
#[derive(Debug, Clone, Default)]
pub struct EntryScan {
    /// The readable entries of every queue, oldest first.
    pub entries: Vec<QueueEntry>,
    /// Ids of the entry files that exist but cannot be read: corrupt, of a
    /// newer version, or unreadable for the filesystem.
    pub unreadable: Vec<String>,
}

/// The automations' file store. Holds no state beyond paths, so any number
/// of handles in any number of threads are safe.
#[derive(Debug, Clone)]
pub struct ObserverStore {
    paths: StatePaths,
}

impl ObserverStore {
    /// A store below the state root of `paths`.
    pub fn new(paths: StatePaths) -> Self {
        Self { paths }
    }

    /// The path layout.
    pub fn paths(&self) -> &StatePaths {
        &self.paths
    }

    // -- records ------------------------------------------------------------

    /// The record of `key`, `None` when the pair has none yet.
    pub fn load(&self, key: &AutomationKey) -> Result<Option<AutomationRecord>> {
        let path = self
            .paths
            .automation_state_file(&key.queue, &key.automation);
        let record: AutomationRecord = match read_json(&path) {
            Ok(record) => record,
            Err(PersistenceError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                return Ok(None)
            }
            Err(error) => return Err(error.into()),
        };
        check_version(&path, record.record_version, AUTOMATION_RECORD_VERSION)?;
        if &record.key != key {
            return Err(ObserverError::LockMismatch {
                lock: key.clone(),
                record: record.key,
            });
        }
        Ok(Some(record))
    }

    /// The record of `key`, or a new one (not saved yet) when the pair has
    /// none.
    pub fn load_or_new(&self, key: &AutomationKey, now: DateTime<Utc>) -> Result<AutomationRecord> {
        Ok(self
            .load(key)?
            .unwrap_or_else(|| AutomationRecord::new(key.clone(), now)))
    }

    /// Every record that can be read, sorted by pair: a record that cannot
    /// (corrupt, or written by a newer build) is logged and skipped, so it
    /// does not stop the others (D7).
    pub fn list_readable(&self) -> Result<Vec<AutomationRecord>> {
        let mut records = Vec::new();
        for key in self.record_keys()? {
            match self.load(&key) {
                Ok(Some(record)) => records.push(record),
                // Deleted between the listing and the read.
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(pair = %key, %error, "automations: skipping an unreadable runtime record");
                }
            }
        }
        Ok(records)
    }

    /// Takes the lock of `key`'s record; the pair need not have a record
    /// yet.
    pub fn lock(&self, key: &AutomationKey) -> Result<AutomationLock> {
        key.validate()?;
        let lock = FileLock::acquire(
            self.paths
                .automation_state_lock_file(&key.queue, &key.automation),
        )
        .map_err(lock_error)?;
        Ok(AutomationLock {
            key: key.clone(),
            _lock: lock,
        })
    }

    /// Atomically replaces a pair's record (or creates it) at the current
    /// schema version, one revision later.
    pub fn save(
        &self,
        lock: &AutomationLock,
        record: &mut AutomationRecord,
        now: DateTime<Utc>,
    ) -> Result<()> {
        lock.require(&record.key)?;
        let previous = self
            .load(&record.key)?
            .map_or(0, |previous| previous.revision);
        record.record_version = AUTOMATION_RECORD_VERSION;
        record.revision = previous + 1;
        record.updated_at = Some(now);
        let path = self
            .paths
            .automation_state_file(&record.key.queue, &record.key.automation);
        Ok(write_json_atomic(&path, record)?)
    }

    /// Deletes a pair's record and its run errors. `Ok(false)` when there
    /// was no record. The caller checked that no run is in progress.
    pub fn delete(&self, lock: &AutomationLock) -> Result<bool> {
        let key = lock.key();
        let record = self
            .paths
            .automation_state_file(&key.queue, &key.automation);
        // The record first: from here on the pair has no state, even if the
        // rest of the directory outlives a crash.
        if !remove_file_if_exists(&record)? {
            return Ok(false);
        }
        let dir = self.paths.automation_state_dir(&key.queue, &key.automation);
        if let Err(source) = fs::remove_dir_all(&dir) {
            if source.kind() != std::io::ErrorKind::NotFound {
                return Err(PersistenceError::io(&dir, source).into());
            }
        }
        // The queue's directory goes with its last pair; another pair's
        // directory keeps it (the removal then fails, which is fine).
        let _ = fs::remove_dir(self.paths.automation_queue_dir(&key.queue));
        Ok(true)
    }

    // -- run failures -------------------------------------------------------

    /// Writes the execution error of a failed or lost run, then deletes the
    /// older errors of the same queue entry (a new error replaces the old
    /// one, R23) and the oldest errors beyond [`KEPT_RUN_FAILURES`].
    /// Writing the same run's error again replaces it.
    pub fn record_failure(&self, lock: &AutomationLock, failure: &RunFailure) -> Result<()> {
        lock.require(&failure.key)?;
        let key = &failure.key;
        let file = |sequence| {
            self.paths
                .automation_error_file(&key.queue, &key.automation, sequence)
        };
        write_json_atomic(&file(failure.sequence), failure)?;
        if failure.entry_id.is_some() {
            for older in self.list_failures(key)? {
                if older.sequence != failure.sequence && older.entry_id == failure.entry_id {
                    remove_file_if_exists(&file(older.sequence))?;
                }
            }
        }
        let sequences = self.failure_sequences(key)?;
        let surplus = sequences.len().saturating_sub(KEPT_RUN_FAILURES);
        for sequence in &sequences[..surplus] {
            remove_file_if_exists(&file(*sequence))?;
        }
        Ok(())
    }

    /// The kept execution errors of a pair, newest first.
    pub fn list_failures(&self, key: &AutomationKey) -> Result<Vec<RunFailure>> {
        let mut failures = Vec::new();
        for sequence in self.failure_sequences(key)?.into_iter().rev() {
            let path = self
                .paths
                .automation_error_file(&key.queue, &key.automation, sequence);
            match read_json::<RunFailure>(&path) {
                Ok(failure) => {
                    check_version(&path, failure.record_version, RUN_FAILURE_VERSION)?;
                    failures.push(failure);
                }
                // Deleted by a newer error between the listing and the read.
                Err(PersistenceError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(failures)
    }

    /// Sequence numbers of a pair's error files, oldest first.
    fn failure_sequences(&self, key: &AutomationKey) -> Result<Vec<u64>> {
        let dir = self
            .paths
            .automation_errors_dir(&key.queue, &key.automation);
        let mut sequences: Vec<u64> = json_file_stems(&dir)?
            .iter()
            .filter_map(|name| parse_event_file_name(&format!("{name}.json")))
            .collect();
        sequences.sort_unstable();
        Ok(sequences)
    }

    // -- queue entries ------------------------------------------------------

    /// Takes the lock of the queue entries: every addition and removal of
    /// an entry holds it. Taken after a pair's lock, never before.
    pub fn lock_queue_entries(&self) -> Result<QueueEntriesLock> {
        let lock = FileLock::acquire(self.paths.queue_entries_lock_file()).map_err(lock_error)?;
        Ok(QueueEntriesLock { _lock: lock })
    }

    /// Adds a queue entry, idempotently by task and episode: an entry
    /// already there is answered as it is and left untouched (a runner that
    /// reports the same episode again, R17).
    pub fn add_queue_entry(
        &self,
        _lock: &QueueEntriesLock,
        entry: &QueueEntry,
    ) -> Result<EntryWrite> {
        entry.validate()?;
        let path = self.paths.queue_entry_file(&entry.entry_id);
        if write_json_atomic_exclusive(&path, entry)? {
            return Ok(EntryWrite::Created);
        }
        Ok(EntryWrite::Exists(read_entry(&path)?))
    }

    /// Replaces the file of `entry` when it cannot be parsed, so a waiting
    /// task's corrupt entry does not keep its gate from every automation.
    /// `Ok(false)`, and nothing written, when the file is missing, readable,
    /// of a newer version (another build's to read) or cannot be read at
    /// all.
    pub fn replace_corrupt_queue_entry(
        &self,
        _lock: &QueueEntriesLock,
        entry: &QueueEntry,
    ) -> Result<bool> {
        entry.validate()?;
        let path = self.paths.queue_entry_file(&entry.entry_id);
        match read_entry(&path) {
            Err(error) if error.is_corrupt_file() => {
                write_json_atomic(&path, entry)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Removes a queue entry. `Ok(false)` when there was none.
    pub fn remove_queue_entry(&self, _lock: &QueueEntriesLock, entry_id: &str) -> Result<bool> {
        if !is_entry_id(entry_id) {
            return Err(ObserverError::invalid(
                "queue entry id",
                format!("'{entry_id}' is not a queue entry id"),
            ));
        }
        Ok(remove_file_if_exists(
            &self.paths.queue_entry_file(entry_id),
        )?)
    }

    /// The queue entry `entry_id`, if it exists.
    pub fn queue_entry(&self, entry_id: &str) -> Result<Option<QueueEntry>> {
        if !is_entry_id(entry_id) {
            return Ok(None);
        }
        match read_entry(&self.paths.queue_entry_file(entry_id)) {
            Ok(entry) => Ok(Some(entry)),
            Err(ObserverError::Persistence(PersistenceError::Io { source, .. }))
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// Every queue entry of every owner, oldest first. An entry file that
    /// cannot be read (corrupt, or of a newer version) is logged and
    /// skipped, as in [`Self::list_readable`]: all queues share the
    /// directory, so one bad file must not stop every queue.
    pub fn list_queue_entries(&self) -> Result<Vec<QueueEntry>> {
        Ok(self.scan_queue_entries()?.entries)
    }

    /// [`Self::list_queue_entries`], and the ids of the entry files it
    /// skipped because they cannot be read.
    pub fn scan_queue_entries(&self) -> Result<EntryScan> {
        let dir = self.paths.queue_entries_dir();
        let mut scan = EntryScan::default();
        for stem in json_file_stems(&dir)? {
            match self.queue_entry(&stem) {
                Ok(Some(entry)) => scan.entries.push(entry),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(entry = %stem, %error, "observers: skipping an unreadable queue entry");
                    scan.unreadable.push(stem);
                }
            }
        }
        scan.entries.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.entry_id.cmp(&right.entry_id))
        });
        Ok(scan)
    }

    /// The entries of `owner`'s tasks on queue `queue`, oldest first: what
    /// the owner's queue view shows.
    pub fn queue_entries(&self, owner: &OwnerRef, queue: &str) -> Result<Vec<QueueEntry>> {
        Ok(self
            .list_queue_entries()?
            .into_iter()
            .filter(|entry| entry.is_in(owner, queue))
            .collect())
    }

    // -- housekeeping -------------------------------------------------------

    /// Removes the temporary files of interrupted writes in every pair's
    /// directories and in the queue entries. Returns the paths removed.
    pub fn cleanup_orphaned_temp_files(&self) -> Result<Vec<PathBuf>> {
        let mut removed = Vec::new();
        for key in self.pair_dirs()? {
            for dir in [
                self.paths.automation_state_dir(&key.queue, &key.automation),
                self.paths
                    .automation_errors_dir(&key.queue, &key.automation),
            ] {
                removed.extend(cleanup_orphaned_temp_files(&dir)?);
            }
        }
        removed.extend(cleanup_orphaned_temp_files(
            &self.paths.queue_entries_dir(),
        )?);
        Ok(removed)
    }

    /// Pairs with a record, sorted.
    fn record_keys(&self) -> Result<Vec<AutomationKey>> {
        Ok(self
            .pair_dirs()?
            .into_iter()
            .filter(|key| {
                self.paths
                    .automation_state_file(&key.queue, &key.automation)
                    .exists()
            })
            .collect())
    }

    /// Every directory named like a pair, record or not, sorted.
    fn pair_dirs(&self) -> Result<Vec<AutomationKey>> {
        let mut keys = Vec::new();
        for queue in sub_dirs(&self.paths.automation_states_dir())? {
            for automation in sub_dirs(&self.paths.automation_queue_dir(&queue))? {
                if let Ok(key) = AutomationKey::new(queue.clone(), automation) {
                    keys.push(key);
                }
            }
        }
        keys.sort();
        Ok(keys)
    }
}

/// The names of the directories directly in `dir`; empty when `dir` does
/// not exist.
fn sub_dirs(dir: &Path) -> Result<Vec<String>> {
    Ok(subdirectories(dir)?
        .iter()
        .filter_map(|path| path.file_name()?.to_str().map(str::to_string))
        .collect())
}

/// `sha256:` digest of a script's bytes, as a run records it.
pub(crate) fn script_digest(bytes: &[u8]) -> String {
    format!("sha256:{}", sha256_hex(bytes))
}

/// Refuses a record whose schema version is newer than this build's.
fn check_version(path: &Path, version: u32, current: u32) -> Result<()> {
    if version > current {
        return Err(ObserverError::UnsupportedRecordVersion {
            what: path.display().to_string(),
            version,
        });
    }
    Ok(())
}

fn read_entry(path: &Path) -> Result<QueueEntry> {
    let entry: QueueEntry = read_json(path)?;
    check_version(path, entry.record_version, QUEUE_ENTRY_VERSION)?;
    Ok(entry)
}

/// A queue entry id is a hex SHA-256: anything else is not a file name this
/// store wrote, and is never joined into a path.
fn is_entry_id(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Stems of the `*.json` files directly in `dir`, temporary files excluded;
/// empty when `dir` does not exist.
fn json_file_stems(dir: &Path) -> Result<Vec<String>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(PersistenceError::io(dir, source).into()),
    };
    let mut stems = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| PersistenceError::io(dir, source))?;
        let path = entry.path();
        if is_temp_file(&path) {
            continue;
        }
        if let Some(stem) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".json"))
        {
            stems.push(stem.to_string());
        }
    }
    Ok(stems)
}

fn lock_error(error: FileLockError) -> ObserverError {
    match error {
        FileLockError::Lock { path, source } => ObserverError::Lock { path, source },
        FileLockError::Persistence(error) => ObserverError::Persistence(error),
    }
}
