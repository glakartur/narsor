//! The control plane's material store.
//!
//! ```text
//! {state}/materials/{task}/{material}/material.json   MaterialRecord
//! {state}/materials/{task}/{material}/blobs/{sha256}  content, by digest
//! {state}/materials/.staging/{uuid}/                  an upload under way,
//!                                                     or a removal
//! {state}/materials/.locks/{task}.lock                per-task advisory lock
//! ```
//!
//! # Commit protocol
//!
//! An upload is received into its own staging directory: blobs first, the
//! record last. Only then, under the task's lock, is the directory renamed
//! to `{task}/{material}` — one `rename(2)`, so a material either exists
//! complete or not at all. A refused upload drops its staging directory
//! ([`Staging`]'s `Drop`); one a crash interrupted is removed by
//! [`MaterialStore::recover`]. Removal runs the other way round: the
//! material is renamed *into* staging, then deleted, so a crash half-way
//! through a deletion never leaves half a material visible.
//!
//! The lock files live outside the task directories, because archive
//! deletes those: a lock file deleted while held would let a second holder
//! lock a fresh inode under the same name.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use super::{
    is_material_id, is_sha256_hex, MaterialDelivery, MaterialError, MaterialLimits, MaterialRecord,
    MaterialResult,
};
use crate::domain::TaskId;
use crate::orchestration::store::FileLock;
use crate::persistence::atomic::sync_parent_dir;
use crate::persistence::paths::{MATERIAL_BLOBS_DIR, MATERIAL_RECORD_FILE};
use crate::persistence::{read_json, write_json_atomic, StatePaths};

/// Material files on the state root.
#[derive(Debug, Clone)]
pub struct MaterialStore {
    paths: StatePaths,
    limits: MaterialLimits,
}

/// An upload directory that is discarded unless committed.
#[derive(Debug)]
pub struct Staging {
    dir: PathBuf,
    committed: bool,
}

impl Staging {
    /// Where the upload's blobs go.
    pub fn blobs_dir(&self) -> PathBuf {
        self.dir.join(MATERIAL_BLOBS_DIR)
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.committed {
            if let Err(error) = fs::remove_dir_all(&self.dir) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(dir = %self.dir.display(), %error, "could not discard a refused material upload");
                }
            }
        }
    }
}

impl MaterialStore {
    /// The store below the state root of `paths`, bounded by `limits`.
    pub fn new(paths: &StatePaths, limits: MaterialLimits) -> Self {
        Self {
            paths: paths.clone(),
            limits,
        }
    }

    /// The bounds in force.
    pub fn limits(&self) -> &MaterialLimits {
        &self.limits
    }

    /// A fresh staging directory for one upload.
    pub fn begin(&self) -> MaterialResult<Staging> {
        let dir = self
            .paths
            .materials_staging_dir()
            .join(uuid::Uuid::new_v4().simple().to_string());
        fs::create_dir_all(&dir).map_err(|source| MaterialError::io(&dir, source))?;
        Ok(Staging {
            dir,
            committed: false,
        })
    }

    fn lock(&self, task_id: &TaskId) -> MaterialResult<FileLock> {
        FileLock::acquire(self.paths.task_materials_lock_file(task_id.as_str()))
            .map_err(|error| MaterialError::Internal(error.to_string()))
    }

    /// Makes the staged upload the material `record` describes.
    ///
    /// Under the task's lock: an existing material with the same id is the
    /// answer when its manifest is identical (`replayed`), a refusal
    /// otherwise; `admit` then sees the task's current materials and may
    /// refuse (an archived task, an exceeded quota); only then is the upload
    /// renamed into place.
    pub fn commit(
        &self,
        mut staging: Staging,
        record: MaterialRecord,
        admit: impl FnOnce(&[MaterialRecord]) -> MaterialResult<()>,
    ) -> MaterialResult<(MaterialRecord, bool)> {
        let _lock = self.lock(&record.task_id)?;
        let existing = self.list_unlocked(&record.task_id)?;
        if let Some(previous) = existing
            .iter()
            .find(|material| material.material_id == record.material_id)
        {
            return if previous.manifest_digest == record.manifest_digest {
                Ok((previous.clone(), true))
            } else {
                Err(MaterialError::IdReused(record.material_id))
            };
        }
        admit(&existing)?;
        write_json_atomic(&staging.dir.join(MATERIAL_RECORD_FILE), &record)?;
        let task_dir = self.paths.task_materials_dir(record.task_id.as_str());
        fs::create_dir_all(&task_dir).map_err(|source| MaterialError::io(&task_dir, source))?;
        let target = self
            .paths
            .material_dir(record.task_id.as_str(), &record.material_id);
        fs::rename(&staging.dir, &target).map_err(|source| MaterialError::io(&target, source))?;
        staging.committed = true;
        sync_parent_dir(&target);
        Ok((record, false))
    }

    /// The task's materials, oldest first.
    pub fn list(&self, task_id: &TaskId) -> MaterialResult<Vec<MaterialRecord>> {
        let _lock = self.lock(task_id)?;
        self.list_unlocked(task_id)
    }

    fn list_unlocked(&self, task_id: &TaskId) -> MaterialResult<Vec<MaterialRecord>> {
        let dir = self.paths.task_materials_dir(task_id.as_str());
        let mut records = Vec::new();
        for name in subdirectory_names(&dir)? {
            if !is_material_id(&name) {
                continue;
            }
            let file = self.paths.material_record_file(task_id.as_str(), &name);
            if file.is_file() {
                records.push(read_json::<MaterialRecord>(&file)?);
            }
        }
        records.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.material_id.cmp(&b.material_id))
        });
        Ok(records)
    }

    /// One material of the task.
    pub fn get(&self, task_id: &TaskId, material_id: &str) -> MaterialResult<MaterialRecord> {
        if !is_material_id(material_id) {
            return Err(MaterialError::NotFound);
        }
        let file = self
            .paths
            .material_record_file(task_id.as_str(), material_id);
        if !file.is_file() {
            return Err(MaterialError::NotFound);
        }
        Ok(read_json(&file)?)
    }

    /// The stored blob `sha256` of `record`; only a digest its manifest
    /// names is ever served.
    pub fn blob_file(&self, record: &MaterialRecord, sha256: &str) -> MaterialResult<PathBuf> {
        if !is_sha256_hex(sha256) || !record.files.iter().any(|file| file.sha256 == sha256) {
            return Err(MaterialError::NotFound);
        }
        Ok(self
            .paths
            .material_blob_file(record.task_id.as_str(), &record.material_id, sha256))
    }

    /// Records the runner's acknowledgement of a material whose manifest
    /// digest it names; `changed` is false for a repeated acknowledgement.
    pub fn mark_delivered(
        &self,
        task_id: &TaskId,
        material_id: &str,
        manifest_digest: &str,
        incarnation: &str,
        now: DateTime<Utc>,
    ) -> MaterialResult<(MaterialRecord, bool)> {
        let _lock = self.lock(task_id)?;
        let mut record = self.get(task_id, material_id)?;
        if record.manifest_digest != manifest_digest {
            return Err(MaterialError::DigestMismatch(format!(
                "material '{material_id}' has manifest {}, not {manifest_digest}",
                record.manifest_digest
            )));
        }
        if record.delivered.is_some() {
            return Ok((record, false));
        }
        record.delivered = Some(MaterialDelivery {
            at: now,
            incarnation: incarnation.to_string(),
        });
        write_json_atomic(
            &self
                .paths
                .material_record_file(task_id.as_str(), material_id),
            &record,
        )?;
        Ok((record, true))
    }

    /// Removes one material; `None` when there was none.
    pub fn delete(
        &self,
        task_id: &TaskId,
        material_id: &str,
    ) -> MaterialResult<Option<MaterialRecord>> {
        let _lock = self.lock(task_id)?;
        let record = match self.get(task_id, material_id) {
            Ok(record) => record,
            Err(MaterialError::NotFound) => return Ok(None),
            Err(error) => return Err(error),
        };
        self.remove_dir(&self.paths.material_dir(task_id.as_str(), material_id))?;
        Ok(Some(record))
    }

    /// Removes every material of the task (archive).
    pub fn purge_task(&self, task_id: &TaskId) -> MaterialResult<()> {
        let _lock = self.lock(task_id)?;
        self.remove_dir(&self.paths.task_materials_dir(task_id.as_str()))
    }

    /// Removes the task's materials whose time to live ran out by `now`,
    /// returning them.
    pub fn expire(
        &self,
        task_id: &TaskId,
        now: DateTime<Utc>,
    ) -> MaterialResult<Vec<MaterialRecord>> {
        let _lock = self.lock(task_id)?;
        let mut expired = Vec::new();
        for record in self.list_unlocked(task_id)? {
            if record.expires_at <= now {
                self.remove_dir(
                    &self
                        .paths
                        .material_dir(task_id.as_str(), &record.material_id),
                )?;
                expired.push(record);
            }
        }
        Ok(expired)
    }

    /// Every task that has a material directory.
    pub fn task_ids(&self) -> MaterialResult<Vec<TaskId>> {
        Ok(subdirectory_names(&self.paths.materials_dir())?
            .into_iter()
            .filter_map(|name| TaskId::parse(name).ok())
            .collect())
    }

    /// Startup repair: drops every staged upload or removal a crash left
    /// behind, and the materials of every task `archived` says is archived
    /// (archive committed, its purge did not). Returns what was removed.
    pub fn recover(&self, archived: impl Fn(&TaskId) -> bool) -> MaterialResult<Vec<PathBuf>> {
        let mut removed = Vec::new();
        let staging = self.paths.materials_staging_dir();
        for name in subdirectory_names(&staging)? {
            let dir = staging.join(name);
            fs::remove_dir_all(&dir).map_err(|source| MaterialError::io(&dir, source))?;
            removed.push(dir);
        }
        for task_id in self.task_ids()? {
            if archived(&task_id) {
                let dir = self.paths.task_materials_dir(task_id.as_str());
                self.purge_task(&task_id)?;
                removed.push(dir);
            }
        }
        Ok(removed)
    }

    /// Moves `dir` into staging, then deletes it. Missing is fine.
    fn remove_dir(&self, dir: &Path) -> MaterialResult<()> {
        if !dir.exists() {
            return Ok(());
        }
        let graveyard = self.paths.materials_staging_dir();
        fs::create_dir_all(&graveyard).map_err(|source| MaterialError::io(&graveyard, source))?;
        let doomed = graveyard.join(format!("removed-{}", uuid::Uuid::new_v4().simple()));
        fs::rename(dir, &doomed).map_err(|source| MaterialError::io(dir, source))?;
        sync_parent_dir(dir);
        fs::remove_dir_all(&doomed).map_err(|source| MaterialError::io(&doomed, source))
    }
}

/// Names of the sub-directories of `dir`; none when it does not exist.
fn subdirectory_names(dir: &Path) -> MaterialResult<Vec<String>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(MaterialError::io(dir, source)),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| MaterialError::io(dir, source))?;
        let is_dir = entry
            .file_type()
            .map_err(|source| MaterialError::io(entry.path(), source))?
            .is_dir();
        if let (true, Some(name)) = (is_dir, entry.file_name().to_str()) {
            names.push(name.to_string());
        }
    }
    names.sort();
    Ok(names)
}
