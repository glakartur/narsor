//! Workspace model
//!
//! Represents the execution environment for a workflow.

use serde::{Deserialize, Serialize};

/// Unique identifier for a workspace
pub type WorkspaceId = String;

/// The workspace where actions are executed.
///
/// # Layout of a multi-repository workspace
///
/// A task directory holds every repository of the workflow as a sibling
/// folder of one parent directory:
///
/// ```text
/// {task}/workspace/          <- `root` (what `root: .` of a step means)
///     <name>/                <- one folder per repository, `repositories[i].path`
/// ```
///
/// `path` stays the directory a single-repository task has always worked in:
/// the primary repository itself, so a converted `v1` workflow sees exactly
/// what it saw before. Code that needs the other repositories or the parent
/// uses [`Workspace::root_dir`] and [`Workspace::repository_paths`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    /// Unique identifier
    pub id: WorkspaceId,

    /// Type of workspace
    pub workspace_type: WorkspaceType,

    /// Path to the workspace: the primary repository's directory, or the
    /// only directory of a workspace without repositories.
    pub path: String,

    /// The directory holding every repository folder. `None` for a record that
    /// predates repository lists or whose directory is the only one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,

    /// Every repository, primary first. Empty for a record that predates
    /// repository lists, which then has the single repository described by
    /// the fields of this record (see [`Workspace::repository_entries`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repositories: Vec<WorkspaceRepository>,

    /// Repository reference (if applicable)
    #[serde(default)]
    pub repository: Option<RepositoryInfo>,

    /// Git reference (branch, tag, commit)
    #[serde(default)]
    pub git_ref: Option<String>,

    /// The commit the workspace was materialised at, recorded once when the
    /// execution started.
    ///
    /// This is the base a checkpoint delta is measured against, and it is
    /// deliberately *not* re-read from the checkout: a checkout that has moved
    /// on would silently change what "the delta" means. `None` for a workspace
    /// that is not a git work tree, and for executions written before this
    /// field existed.
    #[serde(default)]
    pub base_commit: Option<String>,

    /// Whether this workspace is read-only
    #[serde(default)]
    pub read_only: bool,
}

/// One repository of a provisioned [`Workspace`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRepository {
    /// Name of the repository: its label in the workflow and its folder.
    pub name: String,

    /// Absolute path of the repository's directory.
    pub path: String,

    /// Whether this is the primary repository.
    pub primary: bool,

    /// Branch the repository started on, when one was named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,

    /// Where a Git repository was cloned from; `None` for a local folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<RepositoryInfo>,

    /// The commit the repository was materialised at, the base of its
    /// checkpoint delta (see [`Workspace::base_commit`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// The directory holding every repository folder (`root: .`); the
    /// workspace's own directory when there is no separate parent.
    pub fn root_dir(&self) -> &str {
        self.root.as_deref().unwrap_or(&self.path)
    }

    /// Every repository of the workspace, primary first, by name.
    ///
    /// A record without a repository list yields its single directory under
    /// the name of its repository (or of its folder).
    pub fn repository_entries(&self) -> Vec<WorkspaceRepository> {
        if !self.repositories.is_empty() {
            return self.repositories.clone();
        }
        let folder = std::path::Path::new(&self.path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        vec![WorkspaceRepository {
            name: self
                .repository
                .as_ref()
                .map_or(folder, |repository| repository.name.clone()),
            path: self.path.clone(),
            primary: true,
            branch: None,
            repository: self.repository.clone(),
            base_commit: self.base_commit.clone(),
        }]
    }

    /// The absolute path of each repository, by name.
    pub fn repository_paths(&self) -> std::collections::BTreeMap<String, std::path::PathBuf> {
        self.repository_entries()
            .into_iter()
            .map(|repository| (repository.name, std::path::PathBuf::from(repository.path)))
            .collect()
    }

    /// Creates a new local folder workspace
    pub fn local_folder(path: String) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            workspace_type: WorkspaceType::LocalFolder,
            path,
            root: None,
            repositories: Vec::new(),
            repository: None,
            git_ref: None,
            base_commit: None,
            read_only: false,
        }
    }

    /// Creates a new git checkout workspace
    pub fn git_checkout(path: String, repository: RepositoryInfo, git_ref: Option<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            workspace_type: WorkspaceType::GitCheckout,
            path,
            root: None,
            repositories: Vec::new(),
            repository: Some(repository),
            git_ref,
            base_commit: None,
            read_only: false,
        }
    }

    /// Creates a new git worktree workspace
    pub fn git_worktree(path: String, repository: RepositoryInfo, git_ref: String) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            workspace_type: WorkspaceType::GitWorktree,
            path,
            root: None,
            repositories: Vec::new(),
            repository: Some(repository),
            git_ref: Some(git_ref),
            base_commit: None,
            read_only: false,
        }
    }
}
