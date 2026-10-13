//! Bounded task materials (U9: R23, R30, AE7).
//!
//! A *material* is a one-off, size-bounded set of regular files attached to
//! a task: input the owner uploads (a few files, a snapshot of a local
//! folder) and small documents the task produces for the owner to download.
//! It is the plan's answer to "give the agent these files" that is neither a
//! live mount of the owner's machine (R23) nor a commit in the product
//! repository (R30).
//!
//! # Shape
//!
//! A material is a **manifest** — normalized relative paths, sizes and
//! SHA-256 digests, regular files only ([`MaterialFile`]) — plus **opaque,
//! content-addressed blobs**. Nothing else exists in the format: no
//! directories, links, devices, modes or owners, so nothing but plain files
//! can ever be recreated from one ([`path`] holds the rules a path must
//! pass). Every size is bounded ([`MaterialLimits`]).
//!
//! # Where the bytes live, and where they never go
//!
//! * At the control plane, under `{state}/materials/{task}/{material}/`
//!   ([`store`]): outside any workspace, outside the task's orchestration
//!   record and its event log. An upload is staged and renamed into place
//!   only when complete and verified, so a refused or interrupted upload
//!   leaves neither blobs nor a manifest behind.
//! * On task storage, under `{storage}/materials/{material}/`, written by
//!   the runner after it downloaded and verified every blob over its own
//!   runner channel ([`crate::runner`]). That directory is a sibling of the
//!   Git workspace, never inside it, so delivering materials can never
//!   produce a commit or even a dirty file in the product repository (AE7).
//! * Never in an event, an SSE frame, a task summary or a conversation
//!   record: those carry the material id, the file count and the byte total
//!   ([`crate::orchestration::TaskEventKind::MaterialChanged`]) and nothing
//!   else (KTD4).
//!
//! # Flow
//!
//! 1. The owner uploads a [`bundle`] to `POST /control/v1/tasks/:task/materials`.
//! 2. The runner's next long-poll announces it ([`MaterialAnnouncement`]); the
//!    runner fetches manifest and blobs with its incarnation-bound
//!    credential, verifies each digest, writes the files atomically and
//!    acknowledges. A redelivery after a lost acknowledgement finds the
//!    material already on storage and only acknowledges again.
//! 3. The runner publishes what the task leaves in `{storage}/outbox/<name>/`
//!    as a material of its own, bounded the same way but smaller.
//! 4. The owner lists, downloads ([`local`]) or deletes materials; each also
//!    expires after its time to live. Archiving the task removes them all.
//!
//! Deleting or expiring a material removes the control plane's copy; a
//! copy already delivered to task storage stays there until the task is
//! archived, which destroys the storage.

pub mod bundle;
pub mod local;
pub mod path;
pub mod snapshot;
pub mod store;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::TaskId;
use crate::persistence::PersistenceError;

pub use bundle::Bundle;
pub use local::{write_files, WriteMode};
pub use narsor_protocol::api::{
    MaterialAnnouncement, MaterialFile, MaterialListResponse, MaterialOrigin, MaterialSummary,
    MaterialUploadResponse, MaterialView, UploadManifest, BUNDLE_CONTENT_TYPE,
};
pub use path::validate_path;
pub use snapshot::{snapshot_folder, snapshot_paths, SkippedEntry, Snapshot};
pub use store::{MaterialStore, Staging};

/// Schema version of a stored [`MaterialRecord`].
pub const MATERIAL_RECORD_VERSION: u32 = 1;

/// Size bounds of materials.
///
/// The defaults keep a material well inside what one HTTP request, one
/// in-memory verification on the runner and the control plane's single
/// small volume (KTD's "first install stays small") comfortably hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterialLimits {
    /// Largest single file.
    pub max_file_bytes: u64,
    /// Largest material uploaded by the owner, all files together.
    pub max_total_bytes: u64,
    /// Largest material a runner publishes ("small" temporary documents).
    pub max_published_bytes: u64,
    /// Most files in one material.
    pub max_files: usize,
    /// Largest encoded manifest in a bundle.
    pub max_manifest_bytes: usize,
    /// Most materials one task holds at once.
    pub max_materials_per_task: usize,
    /// Most bytes one task's materials hold at once.
    pub max_task_bytes: u64,
    /// Time to live of a material that asks for none.
    pub default_ttl: Duration,
    /// Longest time to live granted.
    pub max_ttl: Duration,
}

impl Default for MaterialLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 16 * MIB,
            max_total_bytes: 64 * MIB,
            max_published_bytes: 8 * MIB,
            max_files: 1000,
            max_manifest_bytes: 1024 * 1024,
            max_materials_per_task: 32,
            max_task_bytes: 256 * MIB,
            default_ttl: Duration::days(7),
            max_ttl: Duration::days(30),
        }
    }
}

const MIB: u64 = 1024 * 1024;

/// The runner's acknowledgement of an owner material.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaterialDelivery {
    /// When it was acknowledged.
    pub at: DateTime<Utc>,
    /// The runner incarnation that wrote it onto task storage.
    pub incarnation: String,
}

/// A stored material: its manifest and its delivery state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaterialRecord {
    /// Schema version.
    pub version: u32,
    /// Id, unique within the task.
    pub material_id: String,
    /// The task it belongs to.
    pub task_id: TaskId,
    /// Who created it.
    pub origin: MaterialOrigin,
    /// Its label.
    pub label: Option<String>,
    /// When it was stored.
    pub created_at: DateTime<Utc>,
    /// When it expires.
    pub expires_at: DateTime<Utc>,
    /// The files, sorted by path.
    pub files: Vec<MaterialFile>,
    /// Number of files.
    pub file_count: u32,
    /// Total size of the files.
    pub total_bytes: u64,
    /// Digest of the manifest ([`manifest_digest`]).
    pub manifest_digest: String,
    /// When the runner acknowledged it (owner materials only).
    #[serde(default)]
    pub delivered: Option<MaterialDelivery>,
}

impl MaterialRecord {
    /// The record without its file list.
    pub fn summary(&self) -> MaterialSummary {
        MaterialSummary {
            material_id: self.material_id.clone(),
            task_id: self.task_id.to_string(),
            origin: self.origin,
            label: self.label.clone(),
            created_at: self.created_at,
            expires_at: self.expires_at,
            file_count: self.file_count,
            total_bytes: self.total_bytes,
            manifest_digest: self.manifest_digest.clone(),
            delivered_at: self.delivered.as_ref().map(|delivery| delivery.at),
        }
    }

    /// The record as an owner or runner reads it.
    pub fn view(&self) -> MaterialView {
        MaterialView {
            summary: self.summary(),
            files: self.files.clone(),
        }
    }

    /// Whether a runner still has to receive it.
    pub fn awaits_delivery(&self) -> bool {
        self.origin == MaterialOrigin::Owner && self.delivered.is_none()
    }

    /// The announcement of it in a runner poll.
    pub fn announcement(&self) -> MaterialAnnouncement {
        MaterialAnnouncement {
            material_id: self.material_id.clone(),
            manifest_digest: self.manifest_digest.clone(),
            file_count: self.file_count,
            total_bytes: self.total_bytes,
        }
    }
}

/// `POST /runner/v1/materials/:material_id/ack`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaterialAckRequest {
    /// The digest of the manifest the runner wrote.
    pub manifest_digest: String,
}

/// `POST /runner/v1/materials/:material_id/ack` response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaterialAckResponse {
    /// The material.
    pub material_id: String,
    /// When it was first acknowledged.
    pub delivered_at: DateTime<Utc>,
}

/// Why a material operation was refused.
#[derive(Debug, thiserror::Error)]
pub enum MaterialError {
    /// A path is not a normalized relative path of a regular file.
    #[error("invalid material path '{path}': {reason}")]
    InvalidPath {
        /// The path, as given.
        path: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// The manifest is malformed.
    #[error("invalid material manifest: {0}")]
    InvalidManifest(String),
    /// A size bound was exceeded.
    #[error("material too large: {0}")]
    TooLarge(String),
    /// Content does not hash to what the manifest declares.
    #[error("material content does not match its manifest: {0}")]
    DigestMismatch(String),
    /// The upload ended early, carried extra bytes or could not be read.
    #[error("material upload incomplete: {0}")]
    Incomplete(String),
    /// The task already holds as many materials or bytes as it may.
    #[error("material quota exceeded: {0}")]
    QuotaExceeded(String),
    /// The id is taken by a material with other files.
    #[error("material '{0}' already exists with different files")]
    IdReused(String),
    /// The task is archived; it takes no materials any more.
    #[error("task '{0}' is archived")]
    TaskArchived(TaskId),
    /// No such material (for this caller).
    #[error("material not found")]
    NotFound,
    /// A local file or directory was refused (a link, a device, an existing
    /// file not to be overwritten).
    #[error("{0}")]
    Refused(String),
    /// The store failed.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    /// A filesystem operation failed.
    #[error("I/O error on '{path}': {source}")]
    Io {
        /// Where.
        path: std::path::PathBuf,
        /// What the OS said.
        #[source]
        source: std::io::Error,
    },
    /// Anything else the server got wrong.
    #[error("{0}")]
    Internal(String),
}

impl MaterialError {
    pub fn io(path: impl AsRef<std::path::Path>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.as_ref().to_path_buf(),
            source,
        }
    }
}

/// Result of material operations.
pub type MaterialResult<T> = Result<T, MaterialError>;

/// Digest of a manifest's files — what identifies a material's content.
///
/// Computed over the files sorted by path, so the order a client listed
/// them in does not matter.
pub fn manifest_digest(files: &[MaterialFile]) -> String {
    let mut sorted: Vec<&MaterialFile> = files.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let value = serde_json::json!({ "files": sorted });
    crate::domain::canonical_sha256(&value)
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    hex(&sha2::Sha256::digest(bytes))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// Whether `value` is a lowercase hex SHA-256.
pub fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Whether `value` can be a material id: 1-64 characters of `[a-z0-9-]`,
/// starting with a letter or digit — one safe path component everywhere.
pub fn is_material_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// A fresh material id.
pub fn new_material_id() -> String {
    format!("mat-{}", uuid::Uuid::new_v4().simple())
}

/// Longest label accepted.
const MAX_LABEL_LEN: usize = 200;

/// Checks a whole manifest against `limits`, with `max_total` bytes for the
/// files together, and returns their total size.
///
/// Every path must pass [`path::validate_path`]; no two may be equal when
/// compared case-insensitively (a case-insensitive filesystem would merge
/// them); no path may be a directory of another; the same digest must
/// always declare the same size.
pub fn validate_manifest(
    manifest: &UploadManifest,
    limits: &MaterialLimits,
    max_total: u64,
) -> MaterialResult<u64> {
    if let Some(id) = &manifest.material_id {
        if !is_material_id(id) {
            return Err(MaterialError::InvalidManifest(format!(
                "material id '{id}' must be 1-64 characters of [a-z0-9-], starting with a letter \
                 or digit"
            )));
        }
    }
    if let Some(label) = &manifest.label {
        if label.len() > MAX_LABEL_LEN || label.chars().any(char::is_control) {
            return Err(MaterialError::InvalidManifest(format!(
                "a label is at most {MAX_LABEL_LEN} bytes without control characters"
            )));
        }
    }
    if manifest.files.is_empty() {
        return Err(MaterialError::InvalidManifest(
            "a material holds at least one file".to_string(),
        ));
    }
    if manifest.files.len() > limits.max_files {
        return Err(MaterialError::TooLarge(format!(
            "{} files, at most {} are allowed",
            manifest.files.len(),
            limits.max_files
        )));
    }
    let mut folded = std::collections::BTreeSet::new();
    let mut sizes = std::collections::BTreeMap::new();
    let mut total: u64 = 0;
    for file in &manifest.files {
        validate_path(&file.path)?;
        if !is_sha256_hex(&file.sha256) {
            return Err(MaterialError::InvalidManifest(format!(
                "'{}': sha256 must be 64 lowercase hex characters",
                file.path
            )));
        }
        if file.size > limits.max_file_bytes {
            return Err(MaterialError::TooLarge(format!(
                "'{}' has {} bytes, at most {} are allowed per file",
                file.path, file.size, limits.max_file_bytes
            )));
        }
        if *sizes.entry(file.sha256.as_str()).or_insert(file.size) != file.size {
            return Err(MaterialError::InvalidManifest(format!(
                "'{}': the same digest is declared with two sizes",
                file.path
            )));
        }
        total = total.saturating_add(file.size);
        if total > max_total {
            return Err(MaterialError::TooLarge(format!(
                "more than {max_total} bytes in total"
            )));
        }
        if !folded.insert(file.path.to_lowercase()) {
            return Err(MaterialError::InvalidPath {
                path: file.path.clone(),
                reason: "collides with another path (paths are compared case-insensitively)",
            });
        }
    }
    // A path that is a directory of another cannot also be a file.
    for file in &manifest.files {
        let lower = file.path.to_lowercase();
        let mut prefix = String::new();
        for component in lower.split('/').take(lower.split('/').count() - 1) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            if folded.contains(&prefix) {
                return Err(MaterialError::InvalidPath {
                    path: file.path.clone(),
                    reason: "lies below a path that is a file",
                });
            }
        }
    }
    Ok(total)
}

/// The distinct blobs of `files` in order of first appearance, each with
/// its size — the order their bytes follow the manifest in a [`bundle`].
pub fn blob_order(files: &[MaterialFile]) -> Vec<(String, u64)> {
    let mut seen = std::collections::BTreeSet::new();
    files
        .iter()
        .filter(|file| seen.insert(file.sha256.clone()))
        .map(|file| (file.sha256.clone(), file.size))
        .collect()
}
