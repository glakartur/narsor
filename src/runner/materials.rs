//! The runner's half of task materials (U9).
//!
//! **Delivery.** A poll answer announces owner materials the control plane
//! has not seen acknowledged. For each, the runner fetches the manifest and
//! every blob with its own incarnation-bound credential — the only channel
//! it has, never a mount of anybody's machine (R23) — checks every digest,
//! writes the files into `narsor/materials.partial/{id}` and renames that to
//! `materials/{id}`. The rename is the commit: a `materials/{id}` directory
//! is complete by construction, so a redelivery after a lost acknowledgement
//! only acknowledges again, and a delivery cut short leaves nothing but a
//! partial directory the next attempt starts over.
//!
//! **Publication.** Each folder the task leaves in `outbox/` is published as
//! a material once no engine pass runs (nothing is still writing it), with
//! an id derived from its name and content so a retry after a lost answer
//! is recognized. Published folders are removed; a folder the control plane
//! refuses (too large, a name a material cannot carry) is renamed to
//! `<name>.rejected` and left for the owner to inspect.
//!
//! Neither directory is inside the workspace, so none of this ever touches
//! the product repository (R30, AE7).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::api::ClientError;
use crate::controller_protocol::RunnerClient;
use crate::materials::{
    blob_order, is_material_id, manifest_digest, sha256_hex, snapshot_folder, write_files,
    MaterialAnnouncement, MaterialError, MaterialLimits, WriteMode,
};

use super::storage::TaskStorage;
use super::Session;

/// Why one delivery did not complete.
#[derive(Debug)]
enum DeliveryError {
    /// The control plane did not answer as needed.
    Client(ClientError),
    /// The material or the storage was refused.
    Material(MaterialError),
}

impl std::fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(error) => error.fmt(f),
            Self::Material(error) => error.fmt(f),
        }
    }
}

impl From<ClientError> for DeliveryError {
    fn from(error: ClientError) -> Self {
        Self::Client(error)
    }
}

impl From<MaterialError> for DeliveryError {
    fn from(error: MaterialError) -> Self {
        Self::Material(error)
    }
}

impl Session {
    /// Whether materials are being backed off after a failed delivery.
    pub(super) fn materials_deferred(&self) -> bool {
        self.material_retry_at
            .is_some_and(|at| tokio::time::Instant::now() < at)
    }

    /// Writes the announced materials onto task storage and acknowledges
    /// each.
    ///
    /// A material that cannot be delivered right now — the control plane
    /// unreachable, a blob that does not match its manifest, a full disk —
    /// is retried after a growing delay, during which the poll defers
    /// materials, instead of in a tight loop of immediate poll answers.
    pub(super) async fn receive_materials(&mut self, announcements: Vec<MaterialAnnouncement>) {
        let mut failed = false;
        for announcement in announcements {
            let Some(client) = self.link.client() else {
                return;
            };
            let outcome = match deliver(&self.storage, &client, &announcement).await {
                Ok(()) => client
                    .ack_material(&announcement.material_id, &announcement.manifest_digest)
                    .await
                    .map(|_| ())
                    .map_err(DeliveryError::Client),
                Err(error) => Err(error),
            };
            match outcome {
                Ok(()) => tracing::info!(
                    material = %announcement.material_id,
                    files = announcement.file_count,
                    "material delivered onto task storage"
                ),
                Err(error) => {
                    failed = true;
                    tracing::warn!(material = %announcement.material_id, %error, "material not delivered");
                    if let DeliveryError::Client(error @ ClientError::Unauthorized) = &error {
                        self.link.failed(error, &self.timing);
                    }
                }
            }
        }
        if failed {
            self.material_failures = self.material_failures.saturating_add(1);
            let factor = 2u32.saturating_pow(self.material_failures.saturating_sub(1).min(16));
            let delay = self
                .timing
                .retry_min
                .saturating_mul(factor)
                .min(self.timing.retry_max);
            self.material_retry_at = Some(tokio::time::Instant::now() + delay);
        } else {
            self.material_failures = 0;
            self.material_retry_at = None;
        }
    }

    /// Publishes every finished folder of the outbox.
    pub(super) async fn publish_outbox(&mut self) {
        self.outbox_due = false;
        let entries = match outbox_entries(&self.storage.outbox_dir()) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(%error, "cannot read the outbox");
                return;
            }
        };
        for (name, dir) in entries {
            let Some(client) = self.link.client() else {
                self.outbox_due = true;
                return;
            };
            let limits = MaterialLimits::default();
            let snapshot = match snapshot_folder(&dir, &limits, limits.max_published_bytes) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    reject(&dir, &error.to_string());
                    continue;
                }
            };
            for skipped in &snapshot.skipped {
                tracing::warn!(path = %skipped.path.display(), reason = %skipped.reason, "outbox entry not published");
            }
            let mut bundle = snapshot.bundle;
            let digest = manifest_digest(&bundle.manifest.files);
            bundle.manifest.material_id = Some(outbox_material_id(&name, &digest));
            bundle.manifest.label = Some(name.clone());
            let body = match bundle.encode() {
                Ok(body) => body,
                Err(error) => {
                    reject(&dir, &error.to_string());
                    continue;
                }
            };
            match client.publish_material(body).await {
                Ok(response) => {
                    tracing::info!(
                        material = %response.material.material_id,
                        outbox = %name,
                        "published an outbox folder as a material"
                    );
                    if let Err(error) = fs::remove_dir_all(&dir) {
                        tracing::warn!(dir = %dir.display(), %error, "cannot clear a published outbox folder");
                    }
                }
                Err(ClientError::Request { message, .. }) => reject(&dir, &message),
                Err(error) => {
                    self.link.failed(&error, &self.timing);
                    self.outbox_due = true;
                    return;
                }
            }
        }
    }
}

/// Fetches, verifies and writes one material; nothing to do when it is on
/// storage already.
async fn deliver(
    storage: &TaskStorage,
    client: &RunnerClient,
    announcement: &MaterialAnnouncement,
) -> Result<(), DeliveryError> {
    let material_id = &announcement.material_id;
    if !is_material_id(material_id) {
        return Err(MaterialError::InvalidManifest(format!("material id '{material_id}'")).into());
    }
    let target = storage.materials_dir().join(material_id);
    if fs::symlink_metadata(&target).is_ok_and(|metadata| metadata.is_dir()) {
        // Written by an earlier attempt whose acknowledgement was lost.
        return Ok(());
    }
    let view = client.material(material_id).await?;
    if view.summary.manifest_digest != announcement.manifest_digest
        || manifest_digest(&view.files) != announcement.manifest_digest
    {
        return Err(MaterialError::DigestMismatch(format!(
            "the manifest of '{material_id}' is not the one announced"
        ))
        .into());
    }
    let mut blobs = BTreeMap::new();
    for (sha256, size) in blob_order(&view.files) {
        let bytes = client.material_blob(material_id, &sha256).await?;
        if bytes.len() as u64 != size || sha256_hex(&bytes) != sha256 {
            return Err(MaterialError::DigestMismatch(format!(
                "blob {sha256} of '{material_id}' does not match its manifest"
            ))
            .into());
        }
        blobs.insert(sha256, bytes);
    }

    let partial = storage.partial_materials_dir().join(material_id);
    remove_if_present(&partial)?;
    fs::create_dir_all(&partial).map_err(|source| MaterialError::io(&partial, source))?;
    write_files(&partial, &view.files, &blobs, WriteMode::Refuse)?;
    let materials = storage.materials_dir();
    fs::create_dir_all(&materials).map_err(|source| MaterialError::io(&materials, source))?;
    fs::rename(&partial, &target).map_err(|source| MaterialError::io(&target, source))?;
    Ok(())
}

fn remove_if_present(dir: &Path) -> Result<(), MaterialError> {
    match fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(MaterialError::io(dir, source)),
    }
}

/// Folders of the outbox waiting to be published, by name: hidden folders
/// and rejected ones are left alone, and anything that is not a real folder
/// is ignored.
fn outbox_entries(outbox: &Path) -> std::io::Result<Vec<(String, PathBuf)>> {
    let entries = match fs::read_dir(outbox) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut folders = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name.starts_with('.') || name.ends_with(REJECTED_SUFFIX) {
            continue;
        }
        if entry.file_type()?.is_dir() {
            folders.push((name, entry.path()));
        }
    }
    folders.sort();
    Ok(folders)
}

const REJECTED_SUFFIX: &str = ".rejected";

/// The id of an outbox folder's material: the same name and content always
/// give the same id, so a publication repeated after a lost answer is
/// recognized by the control plane instead of stored twice.
fn outbox_material_id(name: &str, digest: &str) -> String {
    format!(
        "out-{}",
        &sha256_hex(format!("{name}\n{digest}").as_bytes())[..24]
    )
}

/// Sets a refused outbox folder aside, so it is not offered again.
fn reject(dir: &Path, reason: &str) {
    tracing::warn!(dir = %dir.display(), reason, "outbox folder refused; renamed to *.rejected");
    let name = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut target = dir.with_file_name(format!("{name}{REJECTED_SUFFIX}"));
    if target.exists() {
        target = dir.with_file_name(format!(
            "{name}-{}{REJECTED_SUFFIX}",
            uuid::Uuid::new_v4().simple()
        ));
    }
    if let Err(error) = fs::rename(dir, &target) {
        tracing::warn!(dir = %dir.display(), %error, "cannot set a refused outbox folder aside");
    }
}
