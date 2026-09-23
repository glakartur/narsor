//! Provisioning and teardown of per-execution workspaces.
//!
//! Every execution gets its own directory tree, laid out as described in
//! `DOMAIN_MODEL.md` §2.8:
//!
//! ```text
//! {root}/tasks/{execution_id}/code/
//!     {primary_repo}/     -- read-write, this is where the agent works
//!     {secondary_repo}/   -- read-only, mounted for reference only
//! ```
//!
//! # Read-only is a guard rail, not a security boundary
//!
//! "Read-only" here means the write bits are cleared on every file and
//! directory of a secondary repository (D15). That stops an action from
//! *accidentally* editing a repository it was only supposed to read. It stops
//! nothing that is actively hostile: the process runs as the same user that
//! owns the files, so it can simply `chmod` them back. Hard isolation is
//! containerisation (D14) and is deliberately deferred — do not read this
//! module as a sandbox.
//!
//! # Local sources are copied, never symlinked
//!
//! A `local` source is copied into the workspace (D39). A symlink would point
//! at the user's real working directory, so an agent's writes would land in
//! their actual repository — precisely the accident the workspace exists to
//! prevent. Copying costs disk space; a symlink costs the user their work.

use crate::domain::{RepositoryInfo, Workspace};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Directory holding every execution workspace, directly under the root.
pub const TASKS_DIR: &str = "tasks";

/// Directory holding the checked-out repositories of one execution.
pub const CODE_DIR: &str = "code";

/// Result alias for every fallible workspace operation.
pub type Result<T> = std::result::Result<T, WorkspaceError>;

/// Errors produced while provisioning or cleaning up a workspace.
///
/// Every variant names the offending path or reference: like the persistence
/// layer, a workspace failure is diagnosed by looking at something on disk.
#[derive(Debug, Error)]
pub enum WorkspaceError {
    /// A filesystem operation failed.
    #[error("I/O error on '{path}': {source}")]
    Io {
        /// The path the operation was performed on.
        path: PathBuf,
        /// The underlying OS error.
        #[source]
        source: io::Error,
    },

    /// A `local` source path does not exist.
    ///
    /// Raised by [`WorkspaceProvisioner::validate`], i.e. at execution-creation
    /// time, never in the middle of an action.
    #[error("source path '{path}' for repository '{repository}' does not exist")]
    SourceNotFound {
        /// The path that was configured.
        path: PathBuf,
        /// The repository the path belongs to.
        repository: String,
    },

    /// A `local` source path exists but is not a directory.
    #[error("source path '{path}' for repository '{repository}' is not a directory")]
    SourceNotADirectory {
        /// The offending path.
        path: PathBuf,
        /// The repository the path belongs to.
        repository: String,
    },

    /// The requested git reference does not exist in the cloned repository.
    #[error("git ref '{git_ref}' not found in repository '{repository}' ({detail})")]
    RefNotFound {
        /// The branch, tag or commit that was requested.
        git_ref: String,
        /// The repository the reference was looked up in.
        repository: String,
        /// What `git2` said about the failed lookup.
        detail: String,
    },

    /// A git operation failed for a reason other than a missing reference.
    #[error("git error on repository '{repository}': {source}")]
    Git {
        /// The repository being operated on.
        repository: String,
        /// The underlying `git2` error.
        #[source]
        source: git2::Error,
    },

    /// Two repositories in one request share a name, so they would collide.
    #[error("repository name '{name}' is used more than once in the same workspace")]
    DuplicateRepositoryName {
        /// The colliding name.
        name: String,
    },

    /// A repository name cannot be used as a directory name.
    #[error("repository name '{name}' is not a usable directory name")]
    InvalidRepositoryName {
        /// The offending name.
        name: String,
    },

    /// A workspace directory for this execution already exists.
    ///
    /// Never resolved by overwriting: the existing directory may hold an
    /// agent's work.
    #[error("workspace for execution '{execution_id}' already exists at '{path}'")]
    WorkspaceExists {
        /// The execution whose workspace collided.
        execution_id: String,
        /// The directory that already exists.
        path: PathBuf,
    },
}

impl WorkspaceError {
    /// Wraps an [`io::Error`] together with the path it happened on.
    fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// Wraps a [`git2::Error`] together with the repository it happened on.
    fn git(repository: impl Into<String>, source: git2::Error) -> Self {
        Self::Git {
            repository: repository.into(),
            source,
        }
    }
}

/// Where the content of one repository in a workspace comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepositorySource {
    /// A git repository, cloned and checked out at `git_ref`.
    Git {
        /// Anything `git clone` accepts: an URL or a local path.
        remote: String,
        /// Branch, tag or commit to check out. `None` keeps the clone default.
        git_ref: Option<String>,
    },
    /// A directory on this machine, copied into the workspace.
    Local {
        /// The directory whose contents are copied.
        path: PathBuf,
    },
}

/// One repository to make available inside a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositorySpec {
    /// Directory name below `code/`, and the identity used in errors.
    pub name: String,
    /// Where the content comes from.
    pub source: RepositorySource,
}

impl RepositorySpec {
    /// A git repository checked out at `git_ref`.
    pub fn git(
        name: impl Into<String>,
        remote: impl Into<String>,
        git_ref: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            source: RepositorySource::Git {
                remote: remote.into(),
                git_ref,
            },
        }
    }

    /// A local directory copied into the workspace.
    pub fn local(name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            source: RepositorySource::Local { path: path.into() },
        }
    }

    /// The domain-level description of this repository.
    fn repository_info(&self) -> RepositoryInfo {
        let (provider, remote) = match &self.source {
            RepositorySource::Git { remote, .. } => ("git", remote.clone()),
            RepositorySource::Local { path } => ("local", path.display().to_string()),
        };
        RepositoryInfo {
            id: self.name.clone(),
            name: self.name.clone(),
            provider: provider.to_string(),
            remote,
        }
    }

    /// The git reference this repository is pinned to, if any.
    fn git_ref(&self) -> Option<String> {
        match &self.source {
            RepositorySource::Git { git_ref, .. } => git_ref.clone(),
            RepositorySource::Local { .. } => None,
        }
    }
}

/// Everything a workspace needs: one writable repository plus read-only ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRequest {
    /// The repository the execution works in. Provisioned read-write.
    pub primary: RepositorySpec,
    /// Repositories mounted for reference. Provisioned read-only.
    pub secondary: Vec<RepositorySpec>,
}

impl WorkspaceRequest {
    /// A workspace with a single read-write repository.
    pub fn new(primary: RepositorySpec) -> Self {
        Self {
            primary,
            secondary: Vec::new(),
        }
    }

    /// Adds a read-only repository to the workspace.
    pub fn with_secondary(mut self, repository: RepositorySpec) -> Self {
        self.secondary.push(repository);
        self
    }

    /// Every repository of this request, primary first.
    fn all(&self) -> impl Iterator<Item = (&RepositorySpec, bool)> {
        std::iter::once((&self.primary, false))
            .chain(self.secondary.iter().map(|spec| (spec, true)))
    }
}

/// One repository as it exists on disk after provisioning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionedRepository {
    /// Directory name below `code/`.
    pub name: String,
    /// Absolute-or-root-relative path of the checkout.
    pub path: PathBuf,
    /// Whether the write bits were cleared on this tree.
    pub read_only: bool,
}

/// A provisioned workspace: the domain record plus the concrete checkouts.
#[derive(Debug, Clone)]
pub struct ProvisionedWorkspace {
    /// The record to store in [`crate::domain::WorkflowExecution::workspace`].
    pub workspace: Workspace,
    /// `{root}/tasks/{execution_id}` — the whole tree owned by the execution.
    pub task_dir: PathBuf,
    /// Every repository, primary first.
    pub repositories: Vec<ProvisionedRepository>,
}

impl ProvisionedWorkspace {
    /// The read-write repository the execution works in.
    pub fn primary(&self) -> &ProvisionedRepository {
        &self.repositories[0]
    }

    /// Looks a repository up by its directory name.
    pub fn repository(&self, name: &str) -> Option<&ProvisionedRepository> {
        self.repositories.iter().find(|repo| repo.name == name)
    }
}

/// What [`WorkspaceProvisioner::cleanup`] did, or refused to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupOutcome {
    /// The workspace directory was deleted.
    Removed {
        /// The directory that was deleted.
        path: PathBuf,
    },
    /// The workspace was kept because the caller asked for it.
    Preserved {
        /// The directory that was kept.
        path: PathBuf,
    },
    /// There was nothing to clean up.
    ///
    /// Expected for an execution that died before its workspace existed.
    NotPresent {
        /// The directory that was looked for.
        path: PathBuf,
    },
    /// The workspace was kept because deleting it would have lost work.
    Blocked {
        /// The directory that was kept.
        path: PathBuf,
        /// Why deletion was refused, in the same words as the logged warning.
        reason: String,
    },
}

impl CleanupOutcome {
    /// Whether the workspace directory is gone after this outcome.
    pub fn is_removed(&self) -> bool {
        matches!(self, Self::Removed { .. })
    }
}

/// Whether a finished execution's workspace may be deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupPolicy {
    /// Delete the workspace unless it holds unsaved work (D13).
    Remove,
    /// Keep the workspace, whatever it holds.
    Preserve,
}

/// Creates and removes the per-execution workspaces below a configurable root.
///
/// The root is configurable so that tests point it at a temporary directory;
/// nothing here ever reaches for a global location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceProvisioner {
    root: PathBuf,
}

impl WorkspaceProvisioner {
    /// Creates a provisioner rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The workspace root itself.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `{root}/tasks/{execution_id}`
    pub fn task_dir(&self, execution_id: &str) -> PathBuf {
        self.root.join(TASKS_DIR).join(execution_id)
    }

    /// `{root}/tasks/{execution_id}/code`
    pub fn code_dir(&self, execution_id: &str) -> PathBuf {
        self.task_dir(execution_id).join(CODE_DIR)
    }

    /// `{root}/tasks/{execution_id}/code/{repository_name}`
    pub fn repository_dir(&self, execution_id: &str, repository_name: &str) -> PathBuf {
        self.code_dir(execution_id).join(repository_name)
    }

    /// Checks everything that can be checked without touching the workspace.
    ///
    /// Called by [`Self::provision`], and callable on its own so that an
    /// unusable configuration is rejected while the execution is still being
    /// created rather than halfway through the first action.
    pub fn validate(&self, request: &WorkspaceRequest) -> Result<()> {
        let mut seen: Vec<&str> = Vec::new();
        for (spec, _) in request.all() {
            if !is_usable_directory_name(&spec.name) {
                return Err(WorkspaceError::InvalidRepositoryName {
                    name: spec.name.clone(),
                });
            }
            if seen.contains(&spec.name.as_str()) {
                return Err(WorkspaceError::DuplicateRepositoryName {
                    name: spec.name.clone(),
                });
            }
            seen.push(&spec.name);

            if let RepositorySource::Local { path } = &spec.source {
                if !path.exists() {
                    return Err(WorkspaceError::SourceNotFound {
                        path: path.clone(),
                        repository: spec.name.clone(),
                    });
                }
                if !path.is_dir() {
                    return Err(WorkspaceError::SourceNotADirectory {
                        path: path.clone(),
                        repository: spec.name.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Materialises the workspace of `execution_id` on disk.
    ///
    /// The primary repository is left writable; every secondary one has its
    /// write bits cleared. On any failure the partially created tree is
    /// removed, so a failed provisioning never leaves a half-workspace behind.
    pub fn provision(
        &self,
        execution_id: &str,
        request: &WorkspaceRequest,
    ) -> Result<ProvisionedWorkspace> {
        self.validate(request)?;

        let task_dir = self.task_dir(execution_id);
        if task_dir.exists() {
            return Err(WorkspaceError::WorkspaceExists {
                execution_id: execution_id.to_string(),
                path: task_dir,
            });
        }

        let code_dir = self.code_dir(execution_id);
        fs::create_dir_all(&code_dir).map_err(|source| WorkspaceError::io(&code_dir, source))?;

        match self.materialize(execution_id, request) {
            Ok(provisioned) => {
                tracing::info!(
                    execution_id,
                    path = %provisioned.task_dir.display(),
                    repositories = provisioned.repositories.len(),
                    "workspace provisioned"
                );
                Ok(provisioned)
            }
            Err(error) => {
                // A half-provisioned workspace holds nothing worth keeping and
                // would block a retry with `WorkspaceExists`.
                let _ = force_remove_dir_all(&task_dir);
                Err(error)
            }
        }
    }

    /// Provisions every repository. Split out so that [`Self::provision`] can
    /// roll the whole tree back on the first failure.
    fn materialize(
        &self,
        execution_id: &str,
        request: &WorkspaceRequest,
    ) -> Result<ProvisionedWorkspace> {
        let mut repositories = Vec::new();
        for (spec, read_only) in request.all() {
            let target = self.repository_dir(execution_id, &spec.name);
            match &spec.source {
                RepositorySource::Git { remote, git_ref } => {
                    clone_repository(remote, git_ref.as_deref(), &spec.name, &target)?;
                }
                RepositorySource::Local { path } => {
                    copy_dir_recursive(path, &target)?;
                }
            }
            if read_only {
                set_read_only_recursive(&target)?;
            }
            repositories.push(ProvisionedRepository {
                name: spec.name.clone(),
                path: target,
                read_only,
            });
        }

        let primary_path = repositories[0].path.display().to_string();
        let workspace = match &request.primary.source {
            RepositorySource::Git { .. } => Workspace::git_checkout(
                primary_path,
                request.primary.repository_info(),
                request.primary.git_ref(),
            ),
            RepositorySource::Local { .. } => Workspace::local_folder(primary_path),
        };

        Ok(ProvisionedWorkspace {
            workspace,
            task_dir: self.task_dir(execution_id),
            repositories,
        })
    }

    /// Removes the workspace of a finished execution (D13).
    ///
    /// Four things can happen, and all of them are normal:
    ///
    /// * the directory is gone already — an execution that died before its
    ///   workspace existed cleans up successfully and silently;
    /// * `policy` is [`CleanupPolicy::Preserve`] — nothing is touched;
    /// * the read-write repository holds uncommitted changes or unpushed
    ///   commits — deletion is refused and a warning is logged, because losing
    ///   an agent's work costs far more than the disk space;
    /// * otherwise the whole `{root}/tasks/{execution_id}` tree is deleted.
    pub fn cleanup(&self, execution_id: &str, policy: CleanupPolicy) -> Result<CleanupOutcome> {
        let task_dir = self.task_dir(execution_id);
        if !task_dir.exists() {
            tracing::debug!(
                execution_id,
                path = %task_dir.display(),
                "no workspace to clean up"
            );
            return Ok(CleanupOutcome::NotPresent { path: task_dir });
        }

        if policy == CleanupPolicy::Preserve {
            tracing::info!(
                execution_id,
                path = %task_dir.display(),
                "workspace preserved by policy"
            );
            return Ok(CleanupOutcome::Preserved { path: task_dir });
        }

        if let Some(reason) = self.unsaved_work(execution_id)? {
            tracing::warn!(
                execution_id,
                path = %task_dir.display(),
                reason = %reason,
                "workspace cleanup blocked: {reason}"
            );
            return Ok(CleanupOutcome::Blocked {
                path: task_dir,
                reason,
            });
        }

        force_remove_dir_all(&task_dir)?;
        tracing::info!(
            execution_id,
            path = %task_dir.display(),
            "workspace removed"
        );
        Ok(CleanupOutcome::Removed { path: task_dir })
    }

    /// Describes the unsaved work in the workspace, if there is any.
    ///
    /// Every writable git repository in `code/` is inspected; read-only ones
    /// cannot have been written to by definition. A directory that is not a
    /// git repository holds nothing that could be committed or pushed, so it
    /// never blocks cleanup — the `local` workspace of a plain folder is
    /// disposable by construction.
    fn unsaved_work(&self, execution_id: &str) -> Result<Option<String>> {
        let code_dir = self.code_dir(execution_id);
        if !code_dir.is_dir() {
            return Ok(None);
        }
        let entries =
            fs::read_dir(&code_dir).map_err(|source| WorkspaceError::io(&code_dir, source))?;
        for entry in entries {
            let entry = entry.map_err(|source| WorkspaceError::io(&code_dir, source))?;
            let path = entry.path();
            if !path.is_dir() || is_read_only_dir(&path) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(reason) = unsaved_work_in_repository(&path, &name)? {
                return Ok(Some(reason));
            }
        }
        Ok(None)
    }
}

/// Clones `remote` into `target` and checks out `git_ref`.
fn clone_repository(
    remote: &str,
    git_ref: Option<&str>,
    repository: &str,
    target: &Path,
) -> Result<()> {
    let repo = git2::Repository::clone(remote, target)
        .map_err(|source| WorkspaceError::git(repository, source))?;

    let Some(git_ref) = git_ref else {
        return Ok(());
    };
    checkout_ref(&repo, git_ref, repository)
}

/// Checks `git_ref` out in an already cloned repository.
///
/// A plain branch name is tried first, then the same name below `origin/`, so
/// that a branch that exists only as a remote-tracking ref after the clone
/// still resolves. A remote branch is materialised as a local branch tracking
/// it, which is what makes the "unpushed commits" check meaningful later;
/// anything else (a tag or a commit) lands on a detached HEAD.
fn checkout_ref(repo: &git2::Repository, git_ref: &str, repository: &str) -> Result<()> {
    let (object, reference) = repo
        .revparse_ext(git_ref)
        .or_else(|_| repo.revparse_ext(&format!("origin/{git_ref}")))
        .map_err(|source| WorkspaceError::RefNotFound {
            git_ref: git_ref.to_string(),
            repository: repository.to_string(),
            detail: source.message().to_string(),
        })?;

    let mut checkout = git2::build::CheckoutBuilder::new();
    checkout.force();
    repo.checkout_tree(&object, Some(&mut checkout))
        .map_err(|source| WorkspaceError::git(repository, source))?;

    match reference {
        Some(reference) if reference.is_branch() => {
            let name = reference
                .name()
                .ok_or_else(|| WorkspaceError::RefNotFound {
                    git_ref: git_ref.to_string(),
                    repository: repository.to_string(),
                    detail: "reference name is not valid UTF-8".to_string(),
                })?
                .to_string();
            repo.set_head(&name)
                .map_err(|source| WorkspaceError::git(repository, source))?;
        }
        Some(reference) if reference.is_remote() => {
            let commit = object
                .peel_to_commit()
                .map_err(|source| WorkspaceError::git(repository, source))?;
            let short = git_ref.rsplit('/').next().unwrap_or(git_ref);
            let mut branch = repo
                .branch(short, &commit, true)
                .map_err(|source| WorkspaceError::git(repository, source))?;
            if let Some(upstream) = reference.shorthand() {
                branch
                    .set_upstream(Some(upstream))
                    .map_err(|source| WorkspaceError::git(repository, source))?;
            }
            repo.set_head(&format!("refs/heads/{short}"))
                .map_err(|source| WorkspaceError::git(repository, source))?;
        }
        _ => {
            repo.set_head_detached(object.id())
                .map_err(|source| WorkspaceError::git(repository, source))?;
        }
    }
    Ok(())
}

/// Reports uncommitted changes or unpushed commits in one repository.
///
/// Returns `None` for a directory that is not a git repository at all.
fn unsaved_work_in_repository(path: &Path, name: &str) -> Result<Option<String>> {
    let Ok(repo) = git2::Repository::open(path) else {
        return Ok(None);
    };

    let mut options = git2::StatusOptions::new();
    options.include_untracked(true).include_ignored(false);
    let statuses = repo
        .statuses(Some(&mut options))
        .map_err(|source| WorkspaceError::git(name, source))?;
    if !statuses.is_empty() {
        return Ok(Some(format!(
            "repository '{name}' has {} uncommitted change(s)",
            statuses.len()
        )));
    }

    Ok(unpushed_commits(&repo, name))
}

/// Reports commits that exist only in this checkout.
///
/// A branch with an upstream is compared against it. A branch without an
/// upstream counts as unpushed only when the repository has a remote at all:
/// a repository with no remote has nowhere to push to, so its commits are as
/// safe as they will ever get and must not block cleanup forever.
fn unpushed_commits(repo: &git2::Repository, name: &str) -> Option<String> {
    let head = repo.head().ok()?;
    if !head.is_branch() {
        return None;
    }
    let local_oid = head.target()?;
    let shorthand = head.shorthand().unwrap_or("HEAD").to_string();

    let branch = git2::Branch::wrap(head);
    match branch.upstream() {
        Ok(upstream) => {
            let upstream_oid = upstream.get().target()?;
            let (ahead, _behind) = repo.graph_ahead_behind(local_oid, upstream_oid).ok()?;
            (ahead > 0).then(|| {
                format!("repository '{name}' has {ahead} commit(s) not pushed to its upstream")
            })
        }
        Err(_) => {
            let has_remote = repo.remotes().map(|list| !list.is_empty()).unwrap_or(false);
            has_remote.then(|| {
                format!("repository '{name}' is on branch '{shorthand}', which has no upstream")
            })
        }
    }
}

/// Copies a directory tree, following the rationale in the module docs.
///
/// Symlinks inside the source are copied as the files they point at, because a
/// preserved symlink could reach back out of the workspace.
fn copy_dir_recursive(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(target).map_err(|error| WorkspaceError::io(target, error))?;
    let entries = fs::read_dir(source).map_err(|error| WorkspaceError::io(source, error))?;
    for entry in entries {
        let entry = entry.map_err(|error| WorkspaceError::io(source, error))?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        let metadata =
            fs::metadata(&from).map_err(|error| WorkspaceError::io(&from, error))?;
        if metadata.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            fs::copy(&from, &to).map_err(|error| WorkspaceError::io(&from, error))?;
        }
    }
    Ok(())
}

/// Clears the write bits on a whole tree (D15).
///
/// Directories lose their write bit too: without that, files could still be
/// created, renamed and deleted inside them.
#[cfg(unix)]
fn set_read_only_recursive(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    for entry in walk(path)? {
        let metadata =
            fs::symlink_metadata(&entry).map_err(|error| WorkspaceError::io(&entry, error))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        let mut permissions = metadata.permissions();
        permissions.set_mode(permissions.mode() & !0o222);
        fs::set_permissions(&entry, permissions)
            .map_err(|error| WorkspaceError::io(&entry, error))?;
    }
    Ok(())
}

/// On non-Unix targets there is no equivalent guard rail; say so loudly.
#[cfg(not(unix))]
fn set_read_only_recursive(path: &Path) -> Result<()> {
    tracing::warn!(
        path = %path.display(),
        "read-only repositories are not enforced on this platform"
    );
    Ok(())
}

/// Restores the write bits on a whole tree so that it can be deleted.
#[cfg(unix)]
fn set_writable_recursive(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    for entry in walk(path)? {
        let Ok(metadata) = fs::symlink_metadata(&entry) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        let mut permissions = metadata.permissions();
        permissions.set_mode(permissions.mode() | 0o200);
        let _ = fs::set_permissions(&entry, permissions);
    }
    Ok(())
}

/// No-op counterpart of the Unix implementation.
#[cfg(not(unix))]
fn set_writable_recursive(_path: &Path) -> Result<()> {
    Ok(())
}

/// Deletes a tree that may contain read-only directories.
///
/// `remove_dir_all` cannot unlink entries inside a directory whose write bit is
/// cleared, and secondary repositories are exactly that, so the permissions are
/// restored first.
fn force_remove_dir_all(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    set_writable_recursive(path)?;
    fs::remove_dir_all(path).map_err(|error| WorkspaceError::io(path, error))
}

/// Collects a tree depth-first, deepest entries first.
///
/// The order matters for permission changes: a directory must keep its write
/// bit until everything below it has been visited.
#[cfg(unix)]
fn walk(path: &Path) -> Result<Vec<PathBuf>> {
    let mut collected = Vec::new();
    let metadata =
        fs::symlink_metadata(path).map_err(|error| WorkspaceError::io(path, error))?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        let entries = fs::read_dir(path).map_err(|error| WorkspaceError::io(path, error))?;
        for entry in entries {
            let entry = entry.map_err(|error| WorkspaceError::io(path, error))?;
            collected.extend(walk(&entry.path())?);
        }
    }
    collected.push(path.to_path_buf());
    Ok(collected)
}

/// Whether a directory has its owner write bit cleared.
#[cfg(unix)]
fn is_read_only_dir(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o200 == 0)
        .unwrap_or(false)
}

/// Without permission bits to inspect, nothing is treated as read-only.
#[cfg(not(unix))]
fn is_read_only_dir(_path: &Path) -> bool {
    false
}

/// Rejects names that would escape `code/` or collide with path syntax.
fn is_usable_directory_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}
