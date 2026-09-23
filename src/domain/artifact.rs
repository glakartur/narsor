//! Artifact model
//!
//! Represents files or data produced during workflow execution.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Unique identifier for an artifact
pub type ArtifactId = String;

/// An artifact produced during workflow execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    /// Unique identifier
    pub id: ArtifactId,

    /// Type of artifact (e.g., "file", "log", "report", "commit")
    pub artifact_type: String,

    /// Location (path or URL)
    pub location: String,

    /// Optional name
    #[serde(default)]
    pub name: Option<String>,

    /// Metadata about the artifact
    #[serde(default)]
    pub metadata: serde_json::Value,

    /// ID of the producer (action execution)
    pub producer: String,

    /// When the artifact was created
    pub created_at: DateTime<Utc>,

    /// Size in bytes (if known)
    #[serde(default)]
    pub size_bytes: Option<u64>,
}

impl Artifact {
    /// Creates a new artifact
    pub fn new(artifact_type: String, location: String, producer: String) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            artifact_type,
            location,
            name: None,
            metadata: serde_json::json!({}),
            producer,
            created_at: Utc::now(),
            size_bytes: None,
        }
    }

    /// Creates a new artifact with name
    pub fn with_name(
        artifact_type: String,
        location: String,
        name: String,
        producer: String,
    ) -> Self {
        let mut artifact = Self::new(artifact_type, location, producer);
        artifact.name = Some(name);
        artifact
    }
}
