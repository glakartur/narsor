//! Durable files of observers.
//!
//! ```text
//! {state}/observers/create.lock                     creation and deletion (unique names)
//! {state}/observers/{id}/observer.json              ObserverRecord (atomic replace)
//! {state}/observers/{id}/observer.lock              per-observer mutation lock
//! {state}/observers/{id}/scripts/{n}.sh             script version n (exclusive create, immutable)
//! {state}/observers/{id}/errors/{seq:08}.json       RunFailure of a failed or lost run (atomic
//!                                                   replace; the newest 20 kept)
//! {state}/observers/{id}/watches/{entry}.json       WatchEntry (exclusive create)
//! ```
//!
//! # Locks and commit protocol
//!
//! Mirrors [`crate::orchestration::store`]: every read-modify-write runs
//! under the observer's advisory `flock(2)`, which the kernel releases when
//! the holder dies, and every file is replaced atomically (temp + fsync +
//! rename) or created exclusively (hard link, `EEXIST` instead of
//! clobbering). Methods that mutate an observer's files take the
//! [`ObserverLock`] as proof that the caller holds it. `create.lock` is
//! always taken before an observer's lock, never after.
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

use super::model::{
    sha256_hex, ObserverConfig, ObserverId, ObserverRecord, RunFailure, WatchEntry,
    MAX_SCRIPT_BYTES, OBSERVER_RECORD_VERSION, RUN_FAILURE_VERSION, WATCH_ENTRY_VERSION,
};
use super::{ObserverError, Result};
use crate::domain::WorkflowDefinition;
use crate::orchestration::store::FileLock;
use crate::orchestration::{OrchestrationError, OwnerRef};
use crate::persistence::atomic::{
    cleanup_orphaned_temp_files, read_json, remove_file_if_exists, write_atomic_exclusive,
    write_json_atomic, write_json_atomic_exclusive,
};
use crate::persistence::paths::{is_temp_file, parse_event_file_name, StatePaths};
use crate::persistence::PersistenceError;
use chrono::{DateTime, Utc};
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

/// What an exclusive write of a watch entry found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchWrite {
    /// The entry is new.
    Created,
    /// The entry already existed and was left untouched.
    Exists(WatchEntry),
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
        let record: ObserverRecord = read_json(&path)?;
        check_version(&path, record.record_version, OBSERVER_RECORD_VERSION)?;
        Ok(record)
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
    /// effect. The configuration is validated first, and the name is checked
    /// for uniqueness under `create.lock`.
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
        if self
            .list()?
            .iter()
            .any(|record| record.config.name == config.name)
        {
            return Err(ObserverError::NameTaken(config.name));
        }
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
    /// oldest errors beyond [`KEPT_RUN_FAILURES`]. Writing the same run's
    /// error again replaces it.
    pub fn record_failure(&self, lock: &ObserverLock, failure: &RunFailure) -> Result<()> {
        lock.require(&failure.observer_id)?;
        let id = failure.observer_id.as_str();
        write_json_atomic(
            &self.paths.observer_error_file(id, failure.sequence),
            failure,
        )?;
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

    // -- watches ------------------------------------------------------------

    /// Adds a watch entry, idempotently by task, step and object: an entry
    /// already there is answered as it is and left untouched.
    ///
    /// Checks the entry itself, not the observer's object policy: the
    /// caller applies [`super::WatchObjectPolicy::check`] before.
    pub fn create_watch(&self, lock: &ObserverLock, entry: &WatchEntry) -> Result<WatchWrite> {
        entry.validate()?;
        let path = self
            .paths
            .observer_watch_file(lock.observer_id().as_str(), &entry.entry_id);
        if write_json_atomic_exclusive(&path, entry)? {
            return Ok(WatchWrite::Created);
        }
        Ok(WatchWrite::Exists(read_watch(&path)?))
    }

    /// Every watch entry of an observer, oldest first.
    pub fn list_watches(&self, observer_id: &ObserverId) -> Result<Vec<WatchEntry>> {
        let dir = self.paths.observer_watches_dir(observer_id.as_str());
        let mut entries = Vec::new();
        for stem in json_file_stems(&dir)? {
            if !is_entry_id(&stem) {
                continue;
            }
            match read_watch(&dir.join(format!("{stem}.json"))) {
                Ok(entry) => entries.push(entry),
                // Removed between the listing and the read.
                Err(ObserverError::Persistence(PersistenceError::Io { source, .. }))
                    if source.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        entries.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.entry_id.cmp(&right.entry_id))
        });
        Ok(entries)
    }

    /// Removes a watch entry. `Ok(false)` when there was none.
    pub fn remove_watch(&self, lock: &ObserverLock, entry_id: &str) -> Result<bool> {
        if !is_entry_id(entry_id) {
            return Err(ObserverError::invalid(
                "watch entry id",
                format!("'{entry_id}' is not a watch entry id"),
            ));
        }
        Ok(remove_file_if_exists(&self.paths.observer_watch_file(
            lock.observer_id().as_str(),
            entry_id,
        ))?)
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
                self.paths.observer_watches_dir(id),
            ] {
                removed.extend(cleanup_orphaned_temp_files(&dir)?);
            }
        }
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

fn read_watch(path: &Path) -> Result<WatchEntry> {
    let entry: WatchEntry = read_json(path)?;
    check_version(path, entry.record_version, WATCH_ENTRY_VERSION)?;
    Ok(entry)
}

/// A watch entry id is a hex SHA-256: anything else is not a file name this
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

fn lock_error(error: OrchestrationError) -> ObserverError {
    match error {
        OrchestrationError::Lock { path, source } => ObserverError::Lock { path, source },
        OrchestrationError::Persistence(error) => ObserverError::Persistence(error),
        other => ObserverError::Persistence(PersistenceError::io(
            "observer lock",
            std::io::Error::other(other.to_string()),
        )),
    }
}
