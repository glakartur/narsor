//! Context model
//!
//! Represents the shared state of a workflow execution.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Workflow context - shared state across all steps
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Context {
    /// Task-related information
    #[serde(default)]
    pub task: Option<TaskInfo>,

    /// User-defined variables
    #[serde(default)]
    pub variables: HashMap<String, serde_json::Value>,

    /// References to artifacts
    #[serde(default)]
    pub artifacts: HashMap<String, ArtifactReference>,

    /// Repository references
    #[serde(default)]
    pub repositories: HashMap<String, RepositoryReference>,

    /// Workspace reference
    #[serde(default)]
    pub workspace: Option<WorkspaceInfo>,
}

/// Task information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskInfo {
    pub prompt: String,
    #[serde(default)]
    pub parameters: HashMap<String, serde_json::Value>,
}

/// Reference to an artifact
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactReference {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub location: String,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// Reference to a repository
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryReference {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub remote: String,
    #[serde(default)]
    pub default_branch: Option<String>,
}

/// Workspace information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub id: String,
    pub path: String,
    #[serde(default)]
    pub workspace_type: String,
}

impl Context {
    /// Creates a new empty context
    pub fn new() -> Self {
        Self::default()
    }

    /// Updates variables in the context
    pub fn update_variables(&mut self, variables: serde_json::Map<String, serde_json::Value>) {
        for (key, value) in variables {
            self.variables.insert(key, value);
        }
    }

    /// Adds an artifact reference
    pub fn add_artifact(&mut self, key: String, artifact: ArtifactReference) {
        self.artifacts.insert(key, artifact);
    }

    /// Gets a variable by key
    pub fn get_variable(&self, key: &str) -> Option<&serde_json::Value> {
        self.variables.get(key)
    }
}
