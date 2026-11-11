//! Task creation request DTOs.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A requested CPU and memory profile, expressed independently of the
/// server's environment policy types.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceProfileRequest {
    /// CPU in millicores (`1000` = one core).
    pub cpu_millicores: u32,
    /// Memory in MiB.
    pub memory_mib: u64,
}

/// What the owner chose for one workspace of the workflow
/// (`globals.workspaces.<name>`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRequest {
    /// Registered repository id. Required for a workspace the workflow leaves
    /// to be chosen; refused for one whose repository the workflow names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// Branch to start from, for a workspace the workflow leaves to be
    /// chosen; its `base_branch`, then the registry's default branch, when
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Full commit id to clone, for any Git workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// What a new task runs, before the control plane resolves it against its
/// current configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LaunchRequest {
    /// Workflow to run.
    pub workflow_id: String,
    /// Version the caller expects.
    pub workflow_version: u32,
    /// Workflow inputs.
    #[serde(default)]
    pub inputs: Value,
    /// What the owner chose for the workspaces of the workflow, by workspace
    /// name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub workspaces: BTreeMap<String, WorkspaceRequest>,
    /// Environment profile; owner default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Harness for agent steps that name none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// CPU and memory; backend default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceProfileRequest>,
    /// Ticket the operator typed; the server trims and validates it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
    /// Title the operator typed; the server trims and validates it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// `POST /control/v1/tasks`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CreateTaskRequest {
    /// Client-chosen id; repeating the identical request is safe.
    #[serde(default)]
    pub task_id: Option<String>,
    /// Backend to pin the task to; the owner's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_id: Option<String>,
    /// What to run.
    pub launch: LaunchRequest,
}
