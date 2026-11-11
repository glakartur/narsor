//! Workflow listing, source and validation DTOs.

use serde::{Deserialize, Serialize};

/// A workflow input's declared value type in the owner API.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowInputType {
    #[default]
    String,
    Text,
    Integer,
    Boolean,
}

/// One workspace whose repository the owner chooses when creating a task
/// (`globals.workspaces.<name>` without `repository` or `path`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowWorkspaceView {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The branch the form suggests; the repository's default branch when
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
}

/// One declared workflow input, reduced to fields an owner form needs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowInputView {
    pub name: String,
    #[serde(rename = "type")]
    pub input_type: WorkflowInputType,
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
}

/// Where the selected copy of a workflow comes from.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowSource {
    BuiltIn,
    Catalog,
    User,
}

/// One workflow a task may be created from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowView {
    pub name: String,
    pub source: WorkflowSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    #[serde(default)]
    pub state_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_state: Option<String>,
    pub inputs: Vec<WorkflowInputView>,
    /// The workspaces to choose a repository for, in the order of the file.
    #[serde(default)]
    pub workspaces: Vec<WorkflowWorkspaceView>,
    pub valid: bool,
    #[serde(default)]
    pub violations: Vec<String>,
    pub overrides: bool,
    /// The resolved origin name shown by the CLI (for example `root`).
    #[serde(default)]
    pub origin_label: String,
    /// Lower-priority copies hidden by this selected workflow.
    #[serde(default)]
    pub shadow_labels: Vec<String>,
    /// Whether a built-in definition exists under this name. `None` means
    /// the server predates this field, so clients must not infer absence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_builtin: Option<bool>,
}

/// Request to retrieve exactly one selected or built-in workflow definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowSourceRequest {
    pub name: String,
    #[serde(default)]
    pub builtin: bool,
}

/// One workflow definition returned on demand by the owner API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowSourceResponse {
    pub name: String,
    pub origin_label: String,
    pub content: String,
}

/// Workflows available to the control-plane owner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowListResponse {
    pub workflows: Vec<WorkflowView>,
}

/// Request to validate a workflow path on the server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValidateWorkflowRequest {
    pub path: String,
}

/// Validation result with every reported violation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ValidateWorkflowResponse {
    pub valid: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    #[serde(default)]
    pub state_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_state: Option<String>,
    #[serde(default)]
    pub inputs: Vec<WorkflowInputView>,
    #[serde(default)]
    pub workspaces: Vec<WorkflowWorkspaceView>,
    #[serde(default)]
    pub violations: Vec<String>,
}
