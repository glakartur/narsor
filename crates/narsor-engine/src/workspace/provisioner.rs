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
//!
//! # The `git` command-line tool is a runtime prerequisite
//!
//! Every git operation here shells out to `git`, found on `PATH`. No linked
//! git library is used: libgit ignores `~/.ssh/config` host aliases and
//! `IdentityFile`, git credential helpers and `url.*.insteadOf` rewrites, so
//! it cannot clone the private repositories the user can clone by hand.
//! Running the user's own `git` makes their existing authentication simply
//! work, and lets every failure be reported in git's own words.

use crate::domain::{RefOrigin, RepositoryInfo, Workspace};
use crate::persistence::paths::WorkPaths;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use thiserror::Error;

pub use crate::persistence::paths::{CODE_DIR, TASKS_DIR};

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
    #[error(
        "git ref '{git_ref}'{origin} not found in repository '{repository}' ({detail})",
        origin = origin_note(*.origin)
    )]
    RefNotFound {
        /// The branch, tag or commit that was requested.
        git_ref: String,
        /// Where the name came from, when it is known.
        origin: Option<RefOrigin>,
        /// The repository the reference was looked up in.
        repository: String,
        /// What `git` said on stderr about the failed lookup.
        detail: String,
    },

    /// A `git` invocation failed for a reason other than a missing reference.
    ///
    /// Carries the whole story of the failure: which command was run, how it
    /// ended, and everything `git` printed on stderr. The stderr is the point
    /// — an authentication or ref-lookup failure is diagnosed by reading what
    /// git itself said about it.
    #[error(
        "git error on repository '{repository}': `git {}` failed with {}: {stderr}",
        .args.join(" "),
        .status.map(|code| format!("exit status {code}")).unwrap_or_else(|| "a signal".to_string())
    )]
    Git {
        /// The repository being operated on.
        repository: String,
        /// The arguments `git` was invoked with, without the `git` itself.
        args: Vec<String>,
        /// The exit status, or `None` when git was killed by a signal or
        /// could not be started at all.
        status: Option<i32>,
        /// Everything `git` wrote to stderr, trimmed.
        stderr: String,
    },

    /// A git remote that `git` would not read as a plain URL.
    #[error("repository '{repository}' has an unusable remote '{remote}': {reason}")]
    UnsafeRemote {
        /// The repository the remote belongs to.
        repository: String,
        /// The rejected remote.
        remote: String,
        /// Why it was rejected.
        reason: String,
    },

    /// A git reference that `git` would not read as a plain reference.
    #[error("repository '{repository}' has an unusable ref '{git_ref}': {reason}")]
    UnsafeRef {
        /// The repository the reference belongs to.
        repository: String,
        /// The rejected reference.
        git_ref: String,
        /// Why it was rejected.
        reason: String,
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

    /// Wraps a failed `git` invocation together with the repository it
    /// happened on, keeping git's own stderr verbatim.
    fn git(
        repository: impl Into<String>,
        args: &[&str],
        status: Option<i32>,
        stderr: impl Into<String>,
    ) -> Self {
        Self::Git {
            repository: repository.into(),
            args: args.iter().map(|arg| redact_credentials(arg)).collect(),
            status,
            stderr: redact_credentials(stderr.into().trim()),
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
        /// Where `git_ref` came from. Only used to explain a failed checkout.
        ref_origin: Option<RefOrigin>,
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
                ref_origin: None,
            },
        }
    }

    /// Records where the git ref came from, for the error of a failed checkout.
    pub fn with_ref_origin(mut self, origin: Option<RefOrigin>) -> Self {
        if let RepositorySource::Git { ref_origin, .. } = &mut self.source {
            *ref_origin = origin;
        }
        self
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
/// nothing here ever reaches for a global location. It is a [`WorkPaths`] and
/// not a bare path so that this layer can name the directories it owns and no
/// others: there is no way from here to a workflow definition or to an
/// execution's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceProvisioner {
    paths: WorkPaths,
}

impl WorkspaceProvisioner {
    /// Creates a provisioner rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            paths: WorkPaths::new(root),
        }
    }

    /// The layout this provisioner writes into.
    pub fn paths(&self) -> &WorkPaths {
        &self.paths
    }

    /// The workspace root itself.
    pub fn root(&self) -> &Path {
        self.paths.root()
    }

    /// `{root}/tasks/{execution_id}`
    pub fn task_dir(&self, execution_id: &str) -> PathBuf {
        self.paths.task_dir(execution_id)
    }

    /// `{root}/tasks/{execution_id}/code`
    pub fn code_dir(&self, execution_id: &str) -> PathBuf {
        self.paths.code_dir(execution_id)
    }

    /// `{root}/tasks/{execution_id}/code/{repository_name}`
    pub fn repository_dir(&self, execution_id: &str, repository_name: &str) -> PathBuf {
        self.paths.checkout_dir(execution_id, repository_name)
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

            if let RepositorySource::Git {
                remote, git_ref, ..
            } = &spec.source
            {
                check_remote(&spec.name, remote)?;
                if let Some(git_ref) = git_ref {
                    check_ref(&spec.name, git_ref)?;
                }
            }

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
                RepositorySource::Git {
                    remote,
                    git_ref,
                    ref_origin,
                } => {
                    clone_repository(remote, git_ref.as_deref(), *ref_origin, &spec.name, &target)?;
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

        let primary = &repositories[0].path;
        let primary_path = primary.display().to_string();
        let workspace = match &request.primary.source {
            RepositorySource::Git { .. } => {
                let mut workspace = Workspace::git_checkout(
                    primary_path,
                    request.primary.repository_info(),
                    request.primary.git_ref(),
                );
                workspace.base_commit = head_commit(primary, &request.primary.name)?;
                workspace
            }
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

/// How much of the user's own git setup a clone may use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum GitAccess {
    /// All of it: `~/.gitconfig`, credential helpers, `url.*.insteadOf`
    /// rewrites, SSH keys and agent — what makes the user's private
    /// repositories simply clone.
    #[default]
    User,
    /// None of it, for a clone a control plane beyond this machine asked
    /// for (R17): HOME is `home`, git reads no global or system
    /// configuration, has no SSH agent or askpass helper, speaks `https`
    /// only and follows no redirect. The pinned credential is the only one
    /// it has.
    PinnedOnly {
        /// The HOME git runs with: one holding nothing of the user's.
        home: PathBuf,
    },
}

/// The variables through which git would reach the user's own credentials
/// or configuration, none of which a [`GitAccess::PinnedOnly`] clone sees.
pub(crate) const USER_GIT_ACCESS_ENVS: [&str; 9] = [
    "SSH_AUTH_SOCK",
    "SSH_ASKPASS",
    "GIT_ASKPASS",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "XDG_CONFIG_HOME",
];

/// Clones `remote` into `target` and leaves it on `commit`, detached (R39).
///
/// The remote-task counterpart of [`WorkspaceProvisioner::provision`]: a task
/// pins a full commit id rather than a moving ref, so what lands in the work
/// tree is exactly what the owner pinned, whenever the clone happens. The
/// checkout is verified afterwards — a commit that is not in the remote's
/// history is an error, never a silent fallback to the default branch.
///
/// `credential_file` is a read-only file holding an HTTPS token (mounted
/// outside task storage, R19). It reaches `git` only through a one-shot
/// credential helper given on the command line, which reads the file
/// itself: the token is never an argument, an environment variable or a
/// line of `.git/config`, so nothing about it survives into the work tree.
/// The helper answers with the user name `x-access-token`, which GitHub and
/// GitLab both accept for a token.
///
/// `target` must not exist yet; the caller decides where a half-finished
/// clone may live (see `crate::runner::storage`).
///
/// `access` says how much of the user's own git setup the clone and its
/// checkout may use.
fn clone_without_checkout(
    repository: &str,
    remote: &str,
    target: &Path,
    credential_file: Option<&Path>,
    access: &GitAccess,
) -> Result<String> {
    check_remote(repository, remote)?;
    if target.exists() {
        return Err(WorkspaceError::io(
            target,
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a pinned clone only ever goes into a directory that does not exist yet",
            ),
        ));
    }

    let helper = match credential_file {
        Some(file) => {
            let file = file.to_string_lossy();
            if file.contains('\'') || file.contains('\n') {
                return Err(WorkspaceError::io(
                    file.as_ref(),
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "a credential file path may not contain a quote or a newline",
                    ),
                ));
            }
            Some(format!(
                "credential.helper=!f() {{ test \"$1\" = get || exit 0; \
                 printf 'username=x-access-token\\n'; \
                 printf 'password=%s\\n' \"$(cat '{file}')\"; }}; f"
            ))
        }
        None => None,
    };

    let target_arg = target.to_string_lossy().into_owned();
    let mut clone: Vec<&str> = Vec::new();
    if let Some(helper) = &helper {
        // The empty value first drops every helper the image configured, so
        // the pinned credential is the only one git asks.
        clone.extend(["-c", "credential.helper=", "-c", helper.as_str()]);
    }
    clone.extend([
        "clone",
        "--no-checkout",
        "--quiet",
        "--",
        remote,
        &target_arg,
    ]);
    run_git_with(repository, &clone, access)?;
    Ok(target_arg)
}

pub fn clone_pinned_commit(
    repository: &str,
    remote: &str,
    commit: &str,
    target: &Path,
    credential_file: Option<&Path>,
    access: &GitAccess,
) -> Result<()> {
    check_ref(repository, commit)?;
    let target_arg = clone_without_checkout(repository, remote, target, credential_file, access)?;
    run_git_with(
        repository,
        &[
            "-C",
            &target_arg,
            "checkout",
            "--quiet",
            "--detach",
            &format!("{commit}^{{commit}}"),
        ],
        access,
    )
    .map_err(|error| match error {
        WorkspaceError::Git { stderr, .. } => WorkspaceError::RefNotFound {
            git_ref: commit.to_string(),
            origin: None,
            repository: repository.to_string(),
            detail: stderr,
        },
        other => other,
    })?;

    match head_commit(target, repository)? {
        Some(head) if head == commit => Ok(()),
        head => Err(WorkspaceError::RefNotFound {
            git_ref: commit.to_string(),
            origin: None,
            repository: repository.to_string(),
            detail: format!(
                "the checkout is at {} instead",
                head.as_deref().unwrap_or("no commit")
            ),
        }),
    }
}

/// Clones `remote` into `target` and leaves it detached on whatever
/// `git_ref` (a branch or a tag; the remote's `HEAD` when `None`) names right
/// now, returning that commit.
///
/// The admission-time counterpart of [`clone_pinned_commit`]: the commit is
/// not known when the task is created, so it is pinned here, by the one
/// clone that provisions the workspace. The caller records the returned
/// commit so a resumed task never resolves the ref again.
pub fn clone_resolving_ref(
    repository: &str,
    remote: &str,
    git_ref: Option<&str>,
    ref_origin: Option<RefOrigin>,
    target: &Path,
    credential_file: Option<&Path>,
    access: &GitAccess,
) -> Result<String> {
    if let Some(git_ref) = git_ref {
        check_ref(repository, git_ref)?;
    }
    let target_arg = clone_without_checkout(repository, remote, target, credential_file, access)?;
    let wanted = git_ref.unwrap_or("HEAD");
    // A fresh clone has only the default branch locally; any other branch is
    // reachable as `origin/<branch>`, and a tag by its own name.
    let candidates = if git_ref.is_some() {
        vec![wanted.to_string(), format!("origin/{wanted}")]
    } else {
        vec!["HEAD".to_string()]
    };
    let mut last_error = None;
    for candidate in candidates {
        match run_git_with(
            repository,
            &[
                "-C",
                &target_arg,
                "checkout",
                "--quiet",
                "--detach",
                &format!("{candidate}^{{commit}}"),
            ],
            access,
        ) {
            Ok(_) => {
                return head_commit(target, repository)?.ok_or_else(|| {
                    WorkspaceError::RefNotFound {
                        git_ref: wanted.to_string(),
                        origin: ref_origin.filter(|_| git_ref.is_some()),
                        repository: repository.to_string(),
                        detail: "the checkout has no commit".to_string(),
                    }
                })
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(match last_error {
        Some(WorkspaceError::Git { stderr, .. }) => WorkspaceError::RefNotFound {
            git_ref: wanted.to_string(),
            origin: ref_origin.filter(|_| git_ref.is_some()),
            repository: repository.to_string(),
            detail: stderr,
        },
        Some(other) => other,
        None => unreachable!("at least one candidate is always tried"),
    })
}

/// The commit `path` is checked out at, when it is a git work tree with one.
///
/// What a resumed runner uses to recognise its own finished clone.
pub fn checked_out_commit(path: &Path) -> Option<String> {
    let dir = path.to_string_lossy().into_owned();
    git_stdout(&["-C", &dir, "rev-parse", "--verify", "HEAD^{commit}"])
}

/// One completed `git` invocation, with both streams already decoded.
struct GitRun {
    /// Whether git exited with status 0.
    ok: bool,
    /// The exit code, or `None` when git was killed by a signal.
    code: Option<i32>,
    /// Standard output, trimmed of surrounding whitespace.
    stdout: String,
    /// Standard error, trimmed of surrounding whitespace.
    stderr: String,
}

/// Replaces the password of any `scheme://user:password@host` URL with `***`.
///
/// `git` echoes the remote back in most of its error messages, so a remote
/// carrying an embedded token would otherwise write that token into
/// `state.json` and into the log the moment a clone fails. Redaction happens
/// where the error is built, so no call site can forget it.
fn redact_credentials(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(marker) = rest.find("://") {
        let (before, after) = rest.split_at(marker + 3);
        out.push_str(before);

        // The authority ends at the first '/', or at whitespace when the URL
        // carries no path at all.
        let end = after
            .find(|c: char| c == '/' || c.is_whitespace())
            .unwrap_or(after.len());
        let (authority, tail) = after.split_at(end);

        match (authority.find('@'), authority.find(':')) {
            (Some(at), Some(colon)) if colon < at => {
                out.push_str(&authority[..=colon]);
                out.push_str("***");
                out.push_str(&authority[at..]);
            }
            _ => out.push_str(authority),
        }
        rest = tail;
    }

    out.push_str(rest);
    out
}

/// Rejects a remote `git` would not read as a plain URL.
///
/// Two cases matter. A value starting with `-` is read as an option, and the
/// `ext::` transport makes `git` run an arbitrary command. Neither is reachable
/// from a hand-written workflow by accident, but a remote can come from
/// `{{ inputs.* }}`, which is supplied when the workflow is *run* rather than
/// when it is authored — a lower trust level than the file itself.
pub(crate) fn check_remote(repository: &str, remote: &str) -> Result<()> {
    let reason = if remote.trim().is_empty() {
        Some("it is empty")
    } else if remote.starts_with('-') {
        Some("a value starting with '-' is read by git as an option")
    } else if remote.len() >= 5 && remote[..5].eq_ignore_ascii_case("ext::") {
        Some("the ext:: transport makes git execute an arbitrary command")
    } else {
        None
    };

    match reason {
        Some(reason) => Err(WorkspaceError::UnsafeRemote {
            repository: repository.to_string(),
            remote: redact_credentials(remote),
            reason: reason.to_string(),
        }),
        None => Ok(()),
    }
}

/// The words that say where a ref name came from; empty when that is unknown.
fn origin_note(origin: Option<RefOrigin>) -> String {
    origin
        .map(|origin| format!(" ({})", origin.describe()))
        .unwrap_or_default()
}

/// Rejects a reference `git` would read as an option rather than a revision.
fn check_ref(repository: &str, git_ref: &str) -> Result<()> {
    let reason = if git_ref.trim().is_empty() {
        Some("it is empty")
    } else if git_ref.starts_with('-') {
        Some("a value starting with '-' is read by git as an option")
    } else {
        None
    };

    match reason {
        Some(reason) => Err(WorkspaceError::UnsafeRef {
            repository: repository.to_string(),
            git_ref: git_ref.to_string(),
            reason: reason.to_string(),
        }),
        None => Ok(()),
    }
}

/// Runs the `git` command-line tool and captures both of its streams.
///
/// `git` is looked up by name, so the `PATH` of the running process decides
/// which installation is used — never a path hardcoded here. The narsor
/// therefore requires `git` to be installed and on `PATH` at runtime; see the
/// module documentation.
///
/// `GIT_TERMINAL_PROMPT=0` keeps every invocation non-interactive: a missing
/// credential fails immediately with a message on stderr instead of blocking
/// an unattended execution on a password prompt. Everything else about the
/// user's environment is left alone on purpose — `~/.ssh/config` aliases,
/// credential helpers and `url.*.insteadOf` rewrites are exactly what shelling
/// out to `git` buys us — unless `access` takes them away.
fn git_run(args: &[&str]) -> io::Result<GitRun> {
    git_run_with(args, &GitAccess::User)
}

fn git_run_with(args: &[&str], access: &GitAccess) -> io::Result<GitRun> {
    let mut command = Command::new("git");
    command.args(args).env("GIT_TERMINAL_PROMPT", "0");
    restrict_git_access(&mut command, access);
    let output = command.output()?;
    Ok(GitRun {
        ok: output.status.success(),
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

/// Takes from `command` what `access` does not allow of the user's own git
/// setup. A [`GitAccess::PinnedOnly`] git also follows no HTTP redirect, so
/// a public host cannot send the clone on to a private network.
fn restrict_git_access(command: &mut Command, access: &GitAccess) {
    if let GitAccess::PinnedOnly { home } = access {
        for name in USER_GIT_ACCESS_ENVS {
            command.env_remove(name);
        }
        command
            .env("HOME", home)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.followRedirects")
            .env("GIT_CONFIG_VALUE_0", "false")
            .env("GIT_ALLOW_PROTOCOL", "https");
    }
}

/// Runs `git` and returns its stdout, turning any failure into
/// [`WorkspaceError::Git`] with git's own stderr attached.
fn run_git(repository: &str, args: &[&str]) -> Result<String> {
    run_git_with(repository, args, &GitAccess::User)
}

fn run_git_with(repository: &str, args: &[&str], access: &GitAccess) -> Result<String> {
    let run = git_run_with(args, access).map_err(|error| {
        WorkspaceError::git(
            repository,
            args,
            None,
            format!("could not run git: {error}"),
        )
    })?;
    if !run.ok {
        return Err(WorkspaceError::git(repository, args, run.code, run.stderr));
    }
    Ok(run.stdout)
}

/// Runs `git` where a failure is an answer rather than an error: `None` means
/// "git said no", which is how the absence of an upstream or of a remote is
/// detected.
fn git_stdout(args: &[&str]) -> Option<String> {
    let run = git_run(args).ok()?;
    run.ok.then_some(run.stdout)
}

/// Whether `git` exits cleanly, discarding its output.
fn git_succeeds(args: &[&str]) -> bool {
    git_run(args).map(|run| run.ok).unwrap_or(false)
}

/// Clones `remote` into `target` and checks out `git_ref`.
fn clone_repository(
    remote: &str,
    git_ref: Option<&str>,
    ref_origin: Option<RefOrigin>,
    repository: &str,
    target: &Path,
) -> Result<()> {
    let target_arg = target.to_string_lossy().into_owned();
    run_git(repository, &["clone", "--", remote, &target_arg])?;

    let Some(git_ref) = git_ref else {
        return Ok(());
    };
    checkout_ref(target, git_ref, ref_origin, repository)
}

/// The commit a freshly provisioned checkout sits at, or `None` when the
/// checkout has no commits yet (an unborn `HEAD` — e.g. a brand new
/// repository whose branch tip does not exist).
///
/// Recorded once, as the workspace's base commit: every checkpoint delta is
/// measured against it, and a takeover checks exactly this commit out again —
/// not whatever the branch has moved on to since. An empty repository simply
/// has no base commit to pin; that is not an error, so it must not fail
/// provisioning for everyone else just because checkpointing might one day
/// want a base to diff against.
fn head_commit(path: &Path, repository: &str) -> Result<Option<String>> {
    let dir = path.to_string_lossy().into_owned();
    let args = ["-C", dir.as_str(), "rev-parse", "--verify", "HEAD^{commit}"];
    let run = git_run(&args).map_err(|error| {
        WorkspaceError::git(
            repository,
            &args,
            None,
            format!("could not run git: {error}"),
        )
    })?;
    if run.ok {
        return Ok(Some(run.stdout));
    }

    // `rev-parse` failed. That is expected on an unborn HEAD (a checkout with
    // zero commits), and a real error otherwise. `rev-list --all` distin-
    // guishes the two: it lists every commit reachable from any ref, so an
    // empty, successful result means the repository genuinely has no
    // commits at all — the failure above can only be the unborn-HEAD case.
    // Any other outcome (a non-empty history, or `rev-list` itself failing)
    // means the original failure was a genuine, unexpected git error, which
    // must still propagate.
    let history = git_run(&["-C", &dir, "rev-list", "--all", "-n", "1"]).map_err(|error| {
        WorkspaceError::git(
            repository,
            &args,
            None,
            format!("could not run git: {error}"),
        )
    })?;
    if history.ok && history.stdout.is_empty() {
        return Ok(None);
    }
    Err(WorkspaceError::git(repository, &args, run.code, run.stderr))
}

/// Checks `git_ref` out in an already cloned repository.
///
/// A plain branch name is tried first, then the same name below `origin/`, so
/// that a branch that exists only as a remote-tracking ref after the clone
/// still resolves. A remote branch is materialised as a local branch tracking
/// it, which is what makes the "unpushed commits" check meaningful later;
/// anything else (a tag or a commit) lands on a detached HEAD.
fn checkout_ref(
    path: &Path,
    git_ref: &str,
    ref_origin: Option<RefOrigin>,
    repository: &str,
) -> Result<()> {
    let dir = path.to_string_lossy().into_owned();
    let candidates = [git_ref.to_string(), format!("origin/{git_ref}")];

    let mut detail = String::new();
    let mut resolved = None;
    for candidate in &candidates {
        // `<rev>^{commit}` refuses anything that is not a commit-ish, which is
        // the same bar `revparse_ext` used to apply.
        let run = git_run(&[
            "-C",
            &dir,
            "rev-parse",
            "--verify",
            &format!("{candidate}^{{commit}}"),
        ])
        .map_err(|error| {
            WorkspaceError::git(
                repository,
                &["rev-parse", "--verify", candidate],
                None,
                format!("could not run git: {error}"),
            )
        })?;
        if run.ok {
            resolved = Some(candidate.clone());
            break;
        }
        detail = run.stderr;
    }

    let Some(resolved) = resolved else {
        return Err(WorkspaceError::RefNotFound {
            git_ref: git_ref.to_string(),
            origin: ref_origin,
            repository: repository.to_string(),
            detail,
        });
    };

    if git_succeeds(&[
        "-C",
        &dir,
        "show-ref",
        "--verify",
        "--quiet",
        &format!("refs/heads/{resolved}"),
    ]) {
        run_git(repository, &["-C", &dir, "checkout", "--force", &resolved])?;
        return Ok(());
    }

    if git_succeeds(&[
        "-C",
        &dir,
        "show-ref",
        "--verify",
        "--quiet",
        &format!("refs/remotes/{resolved}"),
    ]) {
        // A remote-tracking ref becomes a local branch of the same short name,
        // tracking the ref it came from.
        let short = resolved.rsplit('/').next().unwrap_or(&resolved).to_string();
        run_git(
            repository,
            &["-C", &dir, "checkout", "--force", "-B", &short, &resolved],
        )?;
        run_git(
            repository,
            &[
                "-C",
                &dir,
                "branch",
                &format!("--set-upstream-to={resolved}"),
                &short,
            ],
        )?;
        return Ok(());
    }

    run_git(
        repository,
        &["-C", &dir, "checkout", "--force", "--detach", &resolved],
    )?;
    Ok(())
}

/// Whether `path` is itself the root of a git repository.
///
/// `git rev-parse` walks up into parent directories, which would make every
/// directory inside a checked-out repository look like one; comparing the
/// reported top level against `path` keeps the answer about `path` alone.
fn is_git_repository(path: &Path) -> bool {
    let dir = path.to_string_lossy().into_owned();
    let Some(top_level) = git_stdout(&["-C", &dir, "rev-parse", "--show-toplevel"]) else {
        return false;
    };
    match (fs::canonicalize(top_level), fs::canonicalize(path)) {
        (Ok(top_level), Ok(path)) => top_level == path,
        _ => false,
    }
}

/// Reports uncommitted changes or unpushed commits in one repository.
///
/// Returns `None` for a directory that is not a git repository at all.
fn unsaved_work_in_repository(path: &Path, name: &str) -> Result<Option<String>> {
    if !is_git_repository(path) {
        return Ok(None);
    }
    let dir = path.to_string_lossy().into_owned();

    // `--porcelain` is stable across git versions, configurations and locales,
    // and lists untracked files (but not ignored ones) by default.
    let status = run_git(name, &["-C", &dir, "status", "--porcelain"])?;
    let changes = status
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    if changes > 0 {
        return Ok(Some(format!(
            "repository '{name}' has {changes} uncommitted change(s)"
        )));
    }

    Ok(unpushed_commits(path, name))
}

/// Reports commits that exist only in this checkout.
///
/// A branch with an upstream is compared against it. A branch without an
/// upstream counts as unpushed only when the repository has a remote at all:
/// a repository with no remote has nowhere to push to, so its commits are as
/// safe as they will ever get and must not block cleanup forever.
fn unpushed_commits(path: &Path, name: &str) -> Option<String> {
    let dir = path.to_string_lossy().into_owned();

    // A detached HEAD is not a branch, and an unborn one has no commits.
    let shorthand = git_stdout(&["-C", &dir, "symbolic-ref", "--short", "HEAD"])?;
    git_stdout(&["-C", &dir, "rev-parse", "--verify", "HEAD"])?;

    match git_stdout(&[
        "-C",
        &dir,
        "rev-parse",
        "--abbrev-ref",
        "--symbolic-full-name",
        "@{upstream}",
    ]) {
        Some(upstream) => {
            let ahead: usize = git_stdout(&[
                "-C",
                &dir,
                "rev-list",
                "--count",
                &format!("{upstream}..HEAD"),
            ])?
            .parse()
            .ok()?;
            (ahead > 0).then(|| {
                format!("repository '{name}' has {ahead} commit(s) not pushed to its upstream")
            })
        }
        None => {
            let has_remote = git_stdout(&["-C", &dir, "remote"])
                .map(|remotes| !remotes.is_empty())
                .unwrap_or(false);
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
        let metadata = fs::metadata(&from).map_err(|error| WorkspaceError::io(&from, error))?;
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
    let metadata = fs::symlink_metadata(path).map_err(|error| WorkspaceError::io(path, error))?;
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
pub(crate) fn is_usable_directory_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `program` started from the user's shell, with their own git setup:
    /// a `~/.gitconfig`, a global configuration, an SSH agent and an
    /// askpass helper.
    fn as_the_user(root: &Path, program: &str, args: &[&str]) -> Command {
        let home = root.join("users-home");
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join(".gitconfig"),
            "[credential]\n\thelper = users-home-helper\n",
        )
        .unwrap();
        let global = root.join("users-global");
        fs::write(&global, "[credential]\n\thelper = users-global-helper\n").unwrap();
        let mut command = Command::new(program);
        command
            .args(args)
            .env("HOME", &home)
            .env("GIT_CONFIG_GLOBAL", &global)
            .env("SSH_AUTH_SOCK", root.join("agent.sock"))
            .env("GIT_ASKPASS", "users-askpass");
        command
    }

    fn stdout(mut command: Command) -> String {
        let output = command.output().expect("run the command");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn restricted(mut command: Command, access: &GitAccess) -> Command {
        restrict_git_access(&mut command, access);
        command
    }

    #[test]
    fn a_pinned_only_git_uses_none_of_the_users_own_setup() {
        let root = std::env::temp_dir().join(format!("narsor-git-access-{}", uuid::Uuid::new_v4()));
        let private = root.join("private-home");
        fs::create_dir_all(&private).unwrap();
        let pinned = GitAccess::PinnedOnly {
            home: private.clone(),
        };
        let helpers = ["config", "--get-all", "credential.helper"];

        let user = as_the_user(&root, "git", &helpers);
        assert_eq!(
            stdout(restricted(user, &GitAccess::User)),
            "users-global-helper"
        );

        let pinned_git = as_the_user(&root, "git", &helpers);
        assert_eq!(stdout(restricted(pinned_git, &pinned)), "");

        let redirects = as_the_user(&root, "git", &["config", "--get", "http.followRedirects"]);
        assert_eq!(stdout(restricted(redirects, &pinned)), "false");

        let env = stdout(restricted(as_the_user(&root, "env", &[]), &pinned));
        let lines: Vec<&str> = env.lines().collect();
        assert!(
            lines.contains(&format!("HOME={}", private.display()).as_str()),
            "{env}"
        );
        assert!(lines.contains(&"GIT_CONFIG_GLOBAL=/dev/null"), "{env}");
        assert!(!env.contains("SSH_AUTH_SOCK="), "{env}");
        assert!(!env.contains("GIT_ASKPASS="), "{env}");

        let _ = fs::remove_dir_all(&root);
    }
}
