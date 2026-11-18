//! Durable files of observers.
//!
//! ```text
//! {state}/observers/create.lock                     creation and deletion (unique names)
//! {state}/observers/{id}/observer.json              ObserverRecord (atomic replace)
//! {state}/observers/{id}/observer.lock              per-observer mutation lock
//! {state}/observers/{id}/scripts/{n}.sh             script version n (exclusive create, immutable)
//! {state}/observers/{id}/errors/{seq:08}.json       RunFailure of a failed or lost run (atomic
//!                                                   replace; the newest 20 kept, the
//!                                                   newest one per queue entry)
//! {state}/observer-queues/entries.lock              additions and removals of queue entries
//! {state}/observer-queues/entries/{entry}.json      QueueEntry (exclusive create, never changed
//!                                                   unless corrupt)
//! ```
//!
//! # Locks and commit protocol
//!
//! Mirrors [`crate::orchestration::store`]: every read-modify-write runs
//! under the observer's advisory `flock(2)`, which the kernel releases when
//! the holder dies, and every file is replaced atomically (temp + fsync +
//! rename) or created exclusively (hard link, `EEXIST` instead of
//! clobbering). Methods that mutate an observer's files take the
//! [`ObserverLock`] as proof that the caller holds it, and methods that add
//! or remove queue entries take the [`QueueEntriesLock`]. The order is
//! fixed: `create.lock`, then an observer's lock, then `entries.lock`; never
//! the other way round.
//!
//! # Queues
//!
//! A queue is a name in the namespace of an owner (R6, R7), not a file: the
//! observers whose configuration names it and the entries that name it. All
//! observers of one owner's queue have the same mode (R26). The check runs
//! under `create.lock` in [`ObserverStore::create`] and
//! [`ObserverStore::update_config`], the only writes that set a queue or a
//! mode, so two concurrent writes cannot mix the modes of one queue (KTD-7).
//!
//! # Records of an older version
//!
//! A version 1 record (watch lists) is read as version 2 (queues), see
//! [`super::migrate`]; the next save writes version 2.
//!
//! The record is the commit point of a script version: the script file is
//! written first and listed in the record second, so a crash in between
//! leaves an unlisted file that the next save of that version replaces. A
//! deletion removes the record first, so a crash half way through leaves a
//! directory without `observer.json`, which no listing reports.
//!
//! A run in progress lives in the record only (`active_run`). A run leaves
//! a file only when it fails or is lost; a successful run changes the
//! record and nothing else.

use super::migrate::migrate_v1_record;
use super::model::{
    sha256_hex, ObserverConfig, ObserverId, ObserverRecord, QueueEntry, RunFailure,
    MAX_SCRIPT_BYTES, OBSERVER_RECORD_VERSION, QUEUE_ENTRY_VERSION, RUN_FAILURE_VERSION,
};
use super::{ObserverError, Result};
use crate::domain::WorkflowDefinition;
use crate::persistence::atomic::{
    cleanup_orphaned_temp_files, read_json, remove_file_if_exists, write_atomic_exclusive,
    write_json_atomic, write_json_atomic_exclusive,
};
use crate::persistence::lock::{FileLock, FileLockError};
use crate::persistence::paths::{is_temp_file, parse_event_file_name, StatePaths};
use crate::persistence::PersistenceError;
use chrono::{DateTime, Utc};
use narsor_protocol::controller::OwnerRef;
use std::fs;
use std::path::{Path, PathBuf};

/// How many execution errors of an observer the store keeps: writing one
/// more deletes the oldest.
pub const KEPT_RUN_FAILURES: usize = 20;

/// Proof that the caller holds one observer's lock. Released when dropped.
#[derive(Debug)]
pub struct ObserverLock {
    observer_id: ObserverId,
    _lock: FileLock,
}

impl ObserverLock {
    /// The observer this lock is for.
    pub fn observer_id(&self) -> &ObserverId {
        &self.observer_id
    }

    fn require(&self, observer_id: &ObserverId) -> Result<()> {
        if &self.observer_id == observer_id {
            Ok(())
        } else {
            Err(ObserverError::LockMismatch {
                lock: self.observer_id.clone(),
                record: observer_id.clone(),
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
    /// The readable entries of every owner, oldest first.
    pub entries: Vec<QueueEntry>,
    /// Ids of the entry files that exist but cannot be read: corrupt, of a
    /// newer version, or unreadable for the filesystem.
    pub unreadable: Vec<String>,
}

/// The observers' file store. Holds no state beyond paths, so any number of
/// handles in any number of threads are safe.
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

    /// True when a record for `observer_id` exists.
    pub fn exists(&self, observer_id: &ObserverId) -> bool {
        self.paths
            .observer_record_file(observer_id.as_str())
            .exists()
    }

    /// Loads an observer's record.
    pub fn load(&self, observer_id: &ObserverId) -> Result<ObserverRecord> {
        let path = self.paths.observer_record_file(observer_id.as_str());
        if !path.exists() {
            return Err(ObserverError::NotFound(observer_id.clone()));
        }
        let mut value: serde_json::Value = read_json(&path)?;
        let version = value
            .get("record_version")
            .and_then(serde_json::Value::as_u64)
            .and_then(|version| u32::try_from(version).ok())
            .unwrap_or(0);
        check_version(&path, version, OBSERVER_RECORD_VERSION)?;
        if version < OBSERVER_RECORD_VERSION {
            migrate_v1_record(&mut value);
        }
        serde_json::from_value(value).map_err(|source| {
            PersistenceError::Deserialize {
                path: path.clone(),
                source,
            }
            .into()
        })
    }

    /// Every observer, sorted by name.
    ///
    /// A directory without `observer.json` (a crash during creation or
    /// deletion) or whose name is not an observer id is skipped.
    pub fn list(&self) -> Result<Vec<ObserverRecord>> {
        let mut records = Vec::new();
        for observer_id in self.observer_ids()? {
            match self.load(&observer_id) {
                Ok(record) => records.push(record),
                // Deleted between the listing and the read.
                Err(ObserverError::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        records.sort_by(|left, right| left.config.name.cmp(&right.config.name));
        Ok(records)
    }

    /// Every observer that can be read, sorted by name: a record that
    /// cannot (corrupt, or written by a newer build) is logged and skipped,
    /// so it does not stop the others (D7).
    ///
    /// For the schedule only. The owner's views and [`Self::create`] use
    /// the strict [`Self::list`], which reports such a record.
    pub fn list_readable(&self) -> Result<Vec<ObserverRecord>> {
        let mut records = Vec::new();
        for observer_id in self.observer_ids()? {
            match self.load(&observer_id) {
                Ok(record) => records.push(record),
                Err(ObserverError::NotFound(_)) => {}
                Err(error) => {
                    tracing::warn!(observer = %observer_id, %error, "observers: skipping an unreadable observer record");
                }
            }
        }
        records.sort_by(|left, right| left.config.name.cmp(&right.config.name));
        Ok(records)
    }

    /// The observers of `owner`'s queue `queue`, sorted by name (R7). An
    /// unreadable record is skipped, as in [`Self::list_readable`].
    pub fn queue_observers(&self, owner: &OwnerRef, queue: &str) -> Result<Vec<ObserverRecord>> {
        Ok(self
            .list_readable()?
            .into_iter()
            .filter(|record| &record.owner == owner && record.config.queue == queue)
            .collect())
    }

    /// The observer named `name`, if any.
    pub fn find_by_name(&self, name: &str) -> Result<Option<ObserverRecord>> {
        Ok(self
            .list()?
            .into_iter()
            .find(|record| record.config.name.as_str() == name))
    }

    /// Creates a draft observer owned by `owner`.
    ///
    /// `workflow` is the catalogue's definition of the workflow a "create a
    /// task" effect names (`None`: not found); it is ignored for any other
    /// effect. The configuration is validated first; the name is checked for
    /// uniqueness and the mode against the other observers of the owner's
    /// queue (R26) under `create.lock`.
    pub fn create(
        &self,
        owner: OwnerRef,
        config: ObserverConfig,
        workflow: Option<&WorkflowDefinition>,
        now: DateTime<Utc>,
    ) -> Result<ObserverRecord> {
        config.validate()?;
        config.validate_workflow(workflow)?;
        let _create = self.lock_creation()?;
        let records = self.list()?;
        if records
            .iter()
            .any(|record| record.config.name == config.name)
        {
            return Err(ObserverError::NameTaken(config.name));
        }
        check_queue_mode(&records, &owner, &config, None)?;
        let mut record = ObserverRecord::new(ObserverId::generate(), owner, config, now);
        // A random id that is already taken is all but impossible; draw
        // again rather than reuse anything.
        while !write_json_atomic_exclusive(
            &self.paths.observer_record_file(record.observer_id.as_str()),
            &record,
        )? {
            record.observer_id = ObserverId::generate();
        }
        Ok(record)
    }

    /// Replaces the configuration of an observer (its queue, mode or
    /// schedule, say), checked as [`Self::create`] checks a new one: valid,
    /// a unique name, and the mode of the queue it then serves (R26, KTD-7).
    /// A "create a task" effect is checked against `workflow` only when it
    /// changes: the catalogue may have a newer version of the workflow the
    /// observer was made for.
    ///
    /// The rest of the record (state, scripts, runs) is kept; a new queue
    /// or mode ends the current round of entries, and a new schedule of a
    /// scheduled observer counts its next tick from `now`.
    pub fn update_config(
        &self,
        observer_id: &ObserverId,
        config: ObserverConfig,
        workflow: Option<&WorkflowDefinition>,
        now: DateTime<Utc>,
    ) -> Result<ObserverRecord> {
        config.validate()?;
        let _create = self.lock_creation()?;
        let lock = self.lock(observer_id)?;
        let records = self.list()?;
        if records
            .iter()
            .any(|record| &record.observer_id != observer_id && record.config.name == config.name)
        {
            return Err(ObserverError::NameTaken(config.name));
        }
        let mut record = self.load(observer_id)?;
        if record.config.effect != config.effect {
            config.validate_workflow(workflow)?;
        }
        check_queue_mode(&records, &record.owner, &config, Some(observer_id))?;
        if record.config.queue != config.queue || record.config.mode != config.mode {
            // The round holds entries of the old queue, or none is due.
            record.entry_round.clear();
            record.entry_last_runs.clear();
        }
        if record.config.schedule != config.schedule && record.next_run_at.is_some() {
            record.next_run_at = Some(config.schedule.next_after(now));
        }
        record.config = config;
        self.save(&lock, &mut record, now)?;
        Ok(record)
    }

    /// Deletes an observer and every file of it. `Ok(false)` when there was
    /// none.
    pub fn delete(&self, observer_id: &ObserverId) -> Result<bool> {
        let _create = self.lock_creation()?;
        let _lock = match self.lock(observer_id) {
            Ok(lock) => lock,
            Err(ObserverError::NotFound(_)) => return Ok(false),
            Err(error) => return Err(error),
        };
        // The record first: from here on the observer does not exist, even
        // if the rest of the directory outlives a crash.
        let record = self.paths.observer_record_file(observer_id.as_str());
        fs::remove_file(&record).map_err(|source| PersistenceError::io(&record, source))?;
        let dir = self.paths.observer_dir(observer_id.as_str());
        fs::remove_dir_all(&dir).map_err(|source| PersistenceError::io(&dir, source))?;
        Ok(true)
    }

    /// Takes the observer's lock, refusing an observer that does not exist
    /// (before or after waiting for it).
    pub fn lock(&self, observer_id: &ObserverId) -> Result<ObserverLock> {
        if !self.exists(observer_id) {
            return Err(ObserverError::NotFound(observer_id.clone()));
        }
        let lock = FileLock::acquire(self.paths.observer_lock_file(observer_id.as_str()))
            .map_err(lock_error)?;
        // Deleted while this caller waited for the lock.
        if !self.exists(observer_id) {
            return Err(ObserverError::NotFound(observer_id.clone()));
        }
        Ok(ObserverLock {
            observer_id: observer_id.clone(),
            _lock: lock,
        })
    }

    /// Atomically replaces an observer's record at the current schema
    /// version, one revision later.
    pub fn save(
        &self,
        lock: &ObserverLock,
        record: &mut ObserverRecord,
        now: DateTime<Utc>,
    ) -> Result<()> {
        lock.require(&record.observer_id)?;
        let previous = self.load(&record.observer_id)?;
        record.record_version = OBSERVER_RECORD_VERSION;
        record.revision = previous.revision + 1;
        record.updated_at = Some(now);
        let path = self.paths.observer_record_file(record.observer_id.as_str());
        Ok(write_json_atomic(&path, record)?)
    }

    // -- scripts ------------------------------------------------------------

    /// Saves `script` as the observer's next, immutable script version and
    /// returns its number. Never touches `approved_version` (R11).
    pub fn add_script(
        &self,
        observer_id: &ObserverId,
        script: &str,
        now: DateTime<Utc>,
    ) -> Result<u32> {
        if script.trim().is_empty() || script.len() > MAX_SCRIPT_BYTES || script.contains('\0') {
            return Err(ObserverError::invalid(
                "script",
                format!("must be non-empty text of at most {MAX_SCRIPT_BYTES} bytes"),
            ));
        }
        let lock = self.lock(observer_id)?;
        let mut record = self.load(observer_id)?;
        let version = record.latest_script_version().unwrap_or(0) + 1;
        let path = self
            .paths
            .observer_script_file(observer_id.as_str(), version);
        if !write_atomic_exclusive(&path, script.as_bytes())? {
            // A file the record never listed: left by a crash between the
            // two writes, and read by nobody. Replace it.
            fs::remove_file(&path).map_err(|source| PersistenceError::io(&path, source))?;
            if !write_atomic_exclusive(&path, script.as_bytes())? {
                return Err(PersistenceError::io(
                    &path,
                    std::io::Error::from(std::io::ErrorKind::AlreadyExists),
                )
                .into());
            }
        }
        record.scripts.push(super::model::ScriptVersion {
            version,
            created_at: now,
            sha256: script_digest(script.as_bytes()),
        });
        self.save(&lock, &mut record, now)?;
        Ok(version)
    }

    /// Reads script version `version`, checked against its digest.
    pub fn load_script(&self, observer_id: &ObserverId, version: u32) -> Result<String> {
        let record = self.load(observer_id)?;
        let listed =
            record
                .script_version(version)
                .ok_or_else(|| ObserverError::ScriptNotFound {
                    observer_id: observer_id.clone(),
                    version,
                })?;
        let path = self
            .paths
            .observer_script_file(observer_id.as_str(), version);
        let bytes = fs::read(&path).map_err(|source| PersistenceError::io(&path, source))?;
        let corrupt = || ObserverError::ScriptCorrupt {
            observer_id: observer_id.clone(),
            version,
        };
        if script_digest(&bytes) != listed.sha256 {
            return Err(corrupt());
        }
        String::from_utf8(bytes).map_err(|_| corrupt())
    }

    // -- run failures -------------------------------------------------------

    /// Writes the execution error of a failed or lost run, then deletes the
    /// older errors of the same queue entry (a new error replaces the old
    /// one, R23) and the oldest errors beyond [`KEPT_RUN_FAILURES`].
    /// Writing the same run's error again replaces it.
    pub fn record_failure(&self, lock: &ObserverLock, failure: &RunFailure) -> Result<()> {
        lock.require(&failure.observer_id)?;
        let id = failure.observer_id.as_str();
        write_json_atomic(
            &self.paths.observer_error_file(id, failure.sequence),
            failure,
        )?;
        if failure.entry_id.is_some() {
            for older in self.list_failures(&failure.observer_id)? {
                if older.sequence != failure.sequence && older.entry_id == failure.entry_id {
                    remove_file_if_exists(&self.paths.observer_error_file(id, older.sequence))?;
                }
            }
        }
        let sequences = self.failure_sequences(&failure.observer_id)?;
        let surplus = sequences.len().saturating_sub(KEPT_RUN_FAILURES);
        for sequence in &sequences[..surplus] {
            remove_file_if_exists(&self.paths.observer_error_file(id, *sequence))?;
        }
        Ok(())
    }

    /// The kept execution errors of an observer, newest first.
    pub fn list_failures(&self, observer_id: &ObserverId) -> Result<Vec<RunFailure>> {
        let mut failures = Vec::new();
        for sequence in self.failure_sequences(observer_id)?.into_iter().rev() {
            let path = self
                .paths
                .observer_error_file(observer_id.as_str(), sequence);
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

    /// Sequence numbers of an observer's error files, oldest first.
    fn failure_sequences(&self, observer_id: &ObserverId) -> Result<Vec<u64>> {
        let dir = self.paths.observer_errors_dir(observer_id.as_str());
        let mut sequences: Vec<u64> = json_file_stems(&dir)?
            .iter()
            .filter_map(|name| parse_event_file_name(&format!("{name}.json")))
            .collect();
        sequences.sort_unstable();
        Ok(sequences)
    }

    // -- queue entries ------------------------------------------------------

    /// Takes the lock of the queue entries: every addition and removal of
    /// an entry holds it. Taken after an observer's lock, never before.
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
    /// task's corrupt entry does not keep its gate from every observer.
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
    /// skipped, as in [`Self::list_readable`]: all owners share the
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

    /// The entries of `owner`'s queue `queue`, oldest first (R7): an entry
    /// of another owner's queue of the same name is not among them.
    pub fn queue_entries(&self, owner: &OwnerRef, queue: &str) -> Result<Vec<QueueEntry>> {
        Ok(self
            .list_queue_entries()?
            .into_iter()
            .filter(|entry| entry.is_in(owner, queue))
            .collect())
    }

    // -- housekeeping -------------------------------------------------------

    /// Removes the temporary files of interrupted writes in every
    /// observer's directories. Returns the paths removed.
    pub fn cleanup_orphaned_temp_files(&self) -> Result<Vec<PathBuf>> {
        let mut removed = Vec::new();
        for observer_id in self.observer_dirs()? {
            let id = observer_id.as_str();
            for dir in [
                self.paths.observer_dir(id),
                self.paths.observer_scripts_dir(id),
                self.paths.observer_errors_dir(id),
            ] {
                removed.extend(cleanup_orphaned_temp_files(&dir)?);
            }
        }
        removed.extend(cleanup_orphaned_temp_files(
            &self.paths.queue_entries_dir(),
        )?);
        Ok(removed)
    }

    fn lock_creation(&self) -> Result<FileLock> {
        FileLock::acquire(self.paths.observers_create_lock_file()).map_err(lock_error)
    }

    /// Ids of every observer with a record, sorted.
    fn observer_ids(&self) -> Result<Vec<ObserverId>> {
        Ok(self
            .observer_dirs()?
            .into_iter()
            .filter(|observer_id| self.exists(observer_id))
            .collect())
    }

    /// Ids of every directory named like an observer, record or not, sorted.
    fn observer_dirs(&self) -> Result<Vec<ObserverId>> {
        let dir = self.paths.observers_dir();
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
            let Ok(observer_id) = ObserverId::parse(name) else {
                continue;
            };
            if entry.path().is_dir() {
                ids.push(observer_id);
            }
        }
        ids.sort();
        Ok(ids)
    }
}

/// `sha256:` digest of a script's bytes, as [`super::ScriptVersion`] holds it.
fn script_digest(bytes: &[u8]) -> String {
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

/// Refuses `config` when another observer of `owner`'s queue (`except`
/// aside) runs in another mode (R26): the refusal names the queue's mode.
fn check_queue_mode(
    records: &[ObserverRecord],
    owner: &OwnerRef,
    config: &ObserverConfig,
    except: Option<&ObserverId>,
) -> Result<()> {
    let other = records.iter().find(|record| {
        Some(&record.observer_id) != except
            && &record.owner == owner
            && record.config.queue == config.queue
            && record.config.mode != config.mode
    });
    match other {
        Some(other) => Err(ObserverError::QueueModeMismatch {
            queue: config.queue.clone(),
            mode: other.config.mode,
            observer: other.config.name.clone(),
        }),
        None => Ok(()),
    }
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
