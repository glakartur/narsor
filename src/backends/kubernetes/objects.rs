//! Minimal read-side types for the parts of Kubernetes objects the adapter
//! looks at. Everything else of an answer is ignored, so a newer API server
//! adding fields never breaks observation.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// `metadata`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ObjectMeta {
    /// Name.
    pub name: String,
    /// Server-assigned unique id.
    pub uid: String,
    /// Set once deletion was requested.
    pub deletion_timestamp: Option<String>,
    /// When the object was created.
    pub creation_timestamp: Option<DateTime<Utc>>,
    /// Annotations.
    pub annotations: BTreeMap<String, String>,
    /// Labels.
    pub labels: BTreeMap<String, String>,
}

/// A Pod, as far as the adapter reads it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Pod {
    /// Metadata.
    pub metadata: ObjectMeta,
    /// Status.
    pub status: PodStatus,
}

/// `status` of a Pod.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PodStatus {
    /// `Pending`, `Running`, `Succeeded`, `Failed` or `Unknown`.
    pub phase: Option<String>,
    /// Main containers.
    pub container_statuses: Vec<ContainerStatus>,
}

/// One container's status.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ContainerStatus {
    /// Container name.
    pub name: String,
    /// Its state.
    pub state: ContainerState,
}

/// `state` of a container.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ContainerState {
    /// Set once it terminated.
    pub terminated: Option<Terminated>,
}

/// `state.terminated`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Terminated {
    /// Exit code.
    pub exit_code: Option<i32>,
}

/// A PVC, as far as the adapter reads it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PersistentVolumeClaim {
    /// Metadata.
    pub metadata: ObjectMeta,
    /// Spec.
    pub spec: PvcSpec,
}

/// `spec` of a PVC.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PvcSpec {
    /// The storage class it was provisioned from.
    pub storage_class_name: Option<String>,
}

/// A StorageClass, as far as the adapter reads it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StorageClass {
    /// `Delete` (the default) or `Retain`.
    pub reclaim_policy: Option<String>,
}

impl StorageClass {
    /// Whether deleting a claim deletes its backing volume, which archive
    /// promises (R34).
    pub fn deletes_volumes(&self) -> bool {
        self.reclaim_policy.as_deref().unwrap_or("Delete") == "Delete"
    }
}
