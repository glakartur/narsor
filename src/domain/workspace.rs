//! Workspace model
//! 
//! Represents the execution environment for a workflow.

use serde::{Deserialize, Serialize};

/// Unique identifier for a workspace
pub type WorkspaceId = String;

/// The workspace where actions are executed
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    /// Unique identifier
    pub id: WorkspaceId,
    
    /// Type of workspace
    pub workspace_type: WorkspaceType,
    
    /// Path to the workspace
    pub path: String,
    
    /// Repository reference (if applicable)
    #[serde(default)]
    pub repository: Option<RepositoryInfo>,
    
    /// Git reference (branch, tag, commit)
    #[serde(default)]
    pub git_ref: Option<String>,
    
    /// Whether this workspace is read-only
    #[serde(default)]
    pub read_only: bool,
}

/// Type of workspace
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceType {
    /// Local folder (not necessarily a git repo)
    LocalFolder,
    /// Git repository checkout
    GitCheckout,
    /// Git worktree
    GitWorktree,
    /// Temporary/isolated workspace
    Temporary,
}

/// Repository information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryInfo {
    /// Repository ID
    pub id: String,
    
    /// Repository name
    pub name: String,
    
    /// Provider (e.g., "github", "gitlab")
    pub provider: String,
    
    /// Remote URL
    pub remote: String,
}

impl Workspace {
    /// Creates a new local folder workspace
    pub fn local_folder(path: String) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            workspace_type: WorkspaceType::LocalFolder,
            path,
            repository: None,
            git_ref: None,
            read_only: false,
        }
    }
    
    /// Creates a new git checkout workspace
    pub fn git_checkout(
        path: String,
        repository: RepositoryInfo,
        git_ref: Option<String>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            workspace_type: WorkspaceType::GitCheckout,
            path,
            repository: Some(repository),
            git_ref,
            read_only: false,
        }
    }
    
    /// Creates a new git worktree workspace
    pub fn git_worktree(
        path: String,
        repository: RepositoryInfo,
        git_ref: String,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            workspace_type: WorkspaceType::GitWorktree,
            path,
            repository: Some(repository),
            git_ref: Some(git_ref),
            read_only: false,
        }
    }
}
