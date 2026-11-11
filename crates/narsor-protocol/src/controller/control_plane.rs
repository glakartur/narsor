//! Control-plane capabilities and owner settings DTOs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Capabilities a control plane exposes to its owner clients.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilitiesView {
    pub connection_name: String,
    pub workflow_paths: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_paths_reason: Option<String>,
    pub legacy_executions: bool,
    pub local_repository_paths: bool,
}

/// Default backend selection as presented to an owner client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DefaultBackendView {
    pub configured: String,
    #[serde(rename = "override")]
    pub override_backend: Option<String>,
    pub effective: String,
    pub revision: u64,
    pub updated_at: Option<DateTime<Utc>>,
}

/// Request to choose or clear the default backend for newly created tasks.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetDefaultBackendRequest {
    #[serde(default)]
    pub backend_id: Option<String>,
}

/// Most concurrent tasks the `max_concurrent_tasks` setting may allow. A sanity
/// bound, not a scheduling policy: Narsor targets a handful of parallel tasks (R6).
pub const MAX_CONCURRENT_TASKS_LIMIT: u32 = 1024;

/// The limit of tasks processed at once, as presented to an owner client.
/// The control plane computes every field; a client shows them as they are.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConcurrencyLimitView {
    /// The limit the scheduler applies now; `None` for no limit.
    pub limit: Option<u32>,
    /// Slots held by tasks that have, or may have, compute.
    pub slots_held: u32,
    /// How many tasks still have to stop for the limit.
    pub overshoot: u32,
    /// Tasks that wait for a pause a lowered limit asked for, and tasks
    /// whose safe point was accepted and whose compute is not removed yet.
    pub waiting_to_stop: u32,
}

/// Request to change the limit of tasks processed at once.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetConcurrencyLimitRequest {
    pub limit: u32,
}
