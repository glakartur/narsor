//! Repository registry DTOs.

use serde::{Deserialize, Serialize};

/// A repository registered for workflow task creation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepositoryView {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub remote: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Repositories available to the control-plane owner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepositoryListResponse {
    pub repositories: Vec<RepositoryView>,
    #[serde(default)]
    pub unreadable: Vec<String>,
}

/// The branches a registered repository has on its remote: names only, no
/// tags and no commits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepositoryBranchesResponse {
    pub branches: Vec<String>,
}

/// Request to register a repository.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CreateRepositoryRequest {
    pub id: String,
    pub remote: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}
