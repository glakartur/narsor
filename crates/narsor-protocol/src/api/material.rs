//! Material bundle request and response DTOs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Media type of an encoded material bundle.
pub const BUNDLE_CONTENT_TYPE: &str = "application/vnd.narsor.material-bundle";

/// One file in a material manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MaterialFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// Upload manifest for a material bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UploadManifest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub material_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
    pub files: Vec<MaterialFile>,
}

/// Who created a material.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MaterialOrigin {
    Owner,
    Runner,
}

/// Summary of one material.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaterialSummary {
    pub material_id: String,
    pub task_id: String,
    pub origin: MaterialOrigin,
    pub label: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub file_count: u32,
    pub total_bytes: u64,
    pub manifest_digest: String,
    pub delivered_at: Option<DateTime<Utc>>,
}

/// Material summary and its file manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaterialView {
    #[serde(flatten)]
    pub summary: MaterialSummary,
    pub files: Vec<MaterialFile>,
}

/// List of materials attached to one task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaterialListResponse {
    pub materials: Vec<MaterialSummary>,
}

/// Result of uploading a material bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaterialUploadResponse {
    pub material: MaterialSummary,
    pub replayed: bool,
}

/// Material announcement included in a runner poll response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaterialAnnouncement {
    pub material_id: String,
    pub manifest_digest: String,
    pub file_count: u32,
    pub total_bytes: u64,
}
