//! Task materials through the protocol (U9).
//!
//! Owners upload, list, download and delete a task's materials; the task's
//! runner is told about undelivered ones in its poll answer, fetches them
//! with its own credential, acknowledges them and publishes its own. Every
//! request is scoped exactly like the rest of the protocol: an owner reaches
//! only its own tasks' materials (another owner's are `NotFound`), a runner
//! only its own task's.
//!
//! The only trace a material leaves in the task's record is a
//! [`MaterialChanged`](crate::orchestration::TaskEventKind::MaterialChanged)
//! event with its id and counts; manifests and bytes stay in the
//! [`MaterialStore`](crate::materials::MaterialStore).

use std::path::PathBuf;

use bytes::Bytes;
use futures::Stream;

use super::{bounded_ttl, ProtocolService};
use crate::controller_protocol::credentials::RunnerIdentity;
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::domain::TaskId;
use crate::materials::{
    bundle, manifest_digest, new_material_id, MaterialAckRequest, MaterialAckResponse,
    MaterialAnnouncement, MaterialError, MaterialListResponse, MaterialOrigin, MaterialRecord,
    MaterialResult, MaterialSummary, MaterialUploadResponse, MaterialView, MATERIAL_RECORD_VERSION,
};
use crate::orchestration::{MaterialChange, OrchestrationError, OwnerRef};

impl ProtocolService {
    // -----------------------------------------------------------------
    // Owner side
    // -----------------------------------------------------------------

    /// Receives a material the owner uploads to one of its tasks.
    pub async fn upload_material<S, E>(
        &self,
        owner: &OwnerRef,
        task_id: &str,
        body: S,
    ) -> ProtocolResult<MaterialUploadResponse>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin,
        E: std::fmt::Display,
    {
        let record = self.owned_task(owner, task_id)?;
        self.receive_material(&record.task_id, MaterialOrigin::Owner, body)
            .await
    }

    /// The task's materials.
    pub fn list_materials(
        &self,
        owner: &OwnerRef,
        task_id: &str,
    ) -> ProtocolResult<MaterialListResponse> {
        let record = self.owned_task(owner, task_id)?;
        self.expire_materials(&record.task_id);
        Ok(MaterialListResponse {
            materials: self
                .materials
                .list(&record.task_id)?
                .iter()
                .map(MaterialRecord::summary)
                .collect(),
        })
    }

    /// One material with its manifest.
    pub fn material(
        &self,
        owner: &OwnerRef,
        task_id: &str,
        material_id: &str,
    ) -> ProtocolResult<MaterialView> {
        let record = self.owned_task(owner, task_id)?;
        self.expire_materials(&record.task_id);
        Ok(self.materials.get(&record.task_id, material_id)?.view())
    }

    /// The stored file of one blob of a material.
    pub fn material_blob(
        &self,
        owner: &OwnerRef,
        task_id: &str,
        material_id: &str,
        sha256: &str,
    ) -> ProtocolResult<PathBuf> {
        let record = self.owned_task(owner, task_id)?;
        self.expire_materials(&record.task_id);
        let material = self.materials.get(&record.task_id, material_id)?;
        Ok(self.materials.blob_file(&material, sha256)?)
    }

    /// Deletes a material at the control plane.
    pub fn delete_material(
        &self,
        owner: &OwnerRef,
        task_id: &str,
        material_id: &str,
    ) -> ProtocolResult<MaterialSummary> {
        let record = self.owned_task(owner, task_id)?;
        let removed = self
            .materials
            .delete(&record.task_id, material_id)?
            .ok_or(ProtocolError::NotFound)?;
        self.record_material_event(&removed, MaterialChange::Deleted);
        Ok(removed.summary())
    }

    // -----------------------------------------------------------------
    // Runner side
    // -----------------------------------------------------------------

    /// Receives a material the runner publishes from its task's outbox.
    pub async fn runner_publish_material<S, E>(
        &self,
        identity: &RunnerIdentity,
        body: S,
    ) -> ProtocolResult<MaterialUploadResponse>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin,
        E: std::fmt::Display,
    {
        self.credentials
            .touch_runner(identity, None)?
            .ok_or(ProtocolError::Unauthorized)?;
        self.receive_material(&identity.task_id, MaterialOrigin::Runner, body)
            .await
    }

    /// A material of the runner's own task, with its manifest.
    pub fn runner_material(
        &self,
        identity: &RunnerIdentity,
        material_id: &str,
    ) -> ProtocolResult<MaterialView> {
        Ok(self.materials.get(&identity.task_id, material_id)?.view())
    }

    /// The stored file of one blob of a material of the runner's task.
    pub fn runner_material_blob(
        &self,
        identity: &RunnerIdentity,
        material_id: &str,
        sha256: &str,
    ) -> ProtocolResult<PathBuf> {
        let material = self.materials.get(&identity.task_id, material_id)?;
        Ok(self.materials.blob_file(&material, sha256)?)
    }

    /// The runner's acknowledgement that a material is on task storage.
    /// Repeating it (a lost answer) changes nothing.
    pub fn runner_ack_material(
        &self,
        identity: &RunnerIdentity,
        material_id: &str,
        request: MaterialAckRequest,
    ) -> ProtocolResult<MaterialAckResponse> {
        let (record, changed) = self.materials.mark_delivered(
            &identity.task_id,
            material_id,
            &request.manifest_digest,
            &identity.incarnation,
            self.control.now(),
        )?;
        if changed {
            self.record_material_event(&record, MaterialChange::Delivered);
        }
        let delivered_at = record
            .delivered
            .as_ref()
            .map(|delivery| delivery.at)
            .ok_or_else(|| ProtocolError::Internal("an acknowledged material".to_string()))?;
        Ok(MaterialAckResponse {
            material_id: record.material_id,
            delivered_at,
        })
    }

    /// Owner materials of the task its runner has not acknowledged yet.
    pub(super) fn material_announcements(&self, task_id: &TaskId) -> Vec<MaterialAnnouncement> {
        let now = self.control.now();
        match self.materials.list(task_id) {
            Ok(records) => records
                .iter()
                .filter(|record| record.awaits_delivery() && record.expires_at > now)
                .map(MaterialRecord::announcement)
                .collect(),
            Err(error) => {
                tracing::warn!(task_id = %task_id, %error, "materials: cannot list for the runner");
                Vec::new()
            }
        }
    }

    // -----------------------------------------------------------------
    // Housekeeping
    // -----------------------------------------------------------------

    /// Removes every expired material of every task. The daemon calls it
    /// periodically; reads expire lazily as well.
    pub fn sweep_materials(&self) {
        match self.materials.task_ids() {
            Ok(task_ids) => {
                for task_id in task_ids {
                    self.expire_materials(&task_id);
                }
            }
            Err(error) => tracing::warn!(%error, "materials: cannot list tasks to expire"),
        }
    }

    /// Removes the task's materials (archive). Logged, never failed: the
    /// archive itself was committed, and startup recovery finishes a purge
    /// that did not.
    pub(super) fn purge_materials(&self, task_id: &TaskId) {
        if let Err(error) = self.materials.purge_task(task_id) {
            tracing::warn!(task_id = %task_id, %error, "materials: purge after archive failed");
        }
    }

    fn expire_materials(&self, task_id: &TaskId) {
        match self.materials.expire(task_id, self.control.now()) {
            Ok(expired) => {
                for record in expired {
                    self.record_material_event(&record, MaterialChange::Expired);
                }
            }
            Err(error) => tracing::warn!(task_id = %task_id, %error, "materials: expiry failed"),
        }
    }

    /// Receives, verifies and commits one bundle for `task_id`.
    async fn receive_material<S, E>(
        &self,
        task_id: &TaskId,
        origin: MaterialOrigin,
        body: S,
    ) -> ProtocolResult<MaterialUploadResponse>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin,
        E: std::fmt::Display,
    {
        self.expire_materials(task_id);
        // Refused before a single byte is staged; checked again at commit.
        self.admit_material(task_id)?;
        let limits = self.materials.limits().clone();
        let max_total = match origin {
            MaterialOrigin::Owner => limits.max_total_bytes,
            MaterialOrigin::Runner => limits.max_published_bytes,
        };
        let staging = self.materials.begin()?;
        let manifest = bundle::receive(body, &limits, max_total, &staging.blobs_dir()).await?;

        let now = self.control.now();
        let ttl = bounded_ttl(manifest.ttl_secs, limits.default_ttl, limits.max_ttl);
        let mut files = manifest.files;
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let total_bytes: u64 = files.iter().map(|file| file.size).sum();
        let record = MaterialRecord {
            version: MATERIAL_RECORD_VERSION,
            material_id: manifest.material_id.unwrap_or_else(new_material_id),
            task_id: task_id.clone(),
            origin,
            label: manifest.label,
            created_at: now,
            expires_at: now + ttl,
            file_count: u32::try_from(files.len()).unwrap_or(u32::MAX),
            total_bytes,
            manifest_digest: manifest_digest(&files),
            files,
            delivered: None,
        };
        let (record, replayed) = self.materials.commit(staging, record, |existing| {
            // Under the store's task lock, which archive's purge takes too
            // after archive committed: an upload either lands before the
            // purge (and is purged) or sees the archived task here.
            self.admit_material(task_id)?;
            let limits = self.materials.limits();
            if existing.len() >= limits.max_materials_per_task {
                return Err(MaterialError::QuotaExceeded(format!(
                    "the task holds {} materials, at most {} are allowed",
                    existing.len(),
                    limits.max_materials_per_task
                )));
            }
            let held: u64 = existing.iter().map(|material| material.total_bytes).sum();
            if held.saturating_add(total_bytes) > limits.max_task_bytes {
                return Err(MaterialError::QuotaExceeded(format!(
                    "the task's materials would hold more than {} bytes",
                    limits.max_task_bytes
                )));
            }
            Ok(())
        })?;
        if !replayed {
            let change = match origin {
                MaterialOrigin::Owner => MaterialChange::Uploaded,
                MaterialOrigin::Runner => MaterialChange::Published,
            };
            self.record_material_event(&record, change);
        }
        Ok(MaterialUploadResponse {
            material: record.summary(),
            replayed,
        })
    }

    /// Refuses materials for a task that is archived (or being archived).
    fn admit_material(&self, task_id: &TaskId) -> MaterialResult<()> {
        match self.control.task(task_id) {
            Ok(record) if record.runner_state.is_archival() => {
                Err(MaterialError::TaskArchived(task_id.clone()))
            }
            Ok(_) => Ok(()),
            Err(OrchestrationError::TaskNotFound(_)) => Err(MaterialError::NotFound),
            Err(error) => Err(MaterialError::Internal(error.to_string())),
        }
    }

    /// Appends the event of a material change and wakes listeners. The
    /// change itself is committed already, so a failure here is logged.
    fn record_material_event(&self, record: &MaterialRecord, change: MaterialChange) {
        match self.control.record_material_change(
            &record.task_id,
            &record.material_id,
            change,
            record.file_count,
            record.total_bytes,
        ) {
            Ok(_) => self.notify(&record.task_id),
            Err(error) => {
                tracing::warn!(task_id = %record.task_id, material = %record.material_id, %error, "materials: could not record the change")
            }
        }
    }
}
