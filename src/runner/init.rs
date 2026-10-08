//! One-time initialization of a task storage (R18, R39).
//!
//! Initialization happens exactly once in a storage's life and is finished by
//! the [`InitializationMarker`]. In order:
//!
//! 1. the launch spec is copied onto the storage
//!    ([`ExecutionStore::initialize_task_launch_spec`]); a different spec for
//!    the same task is refused for good;
//! 2. the pinned commit is cloned — into `narsor/workspace.partial`, then
//!    renamed to `workspace`, so a `workspace` directory only ever exists
//!    complete;
//! 3. the marker is written.
//!
//! A storage with the marker is a *resumed* storage: it is checked to belong
//! to this task and launch, and nothing is cloned again, whatever the
//! workspace now holds — it may hold an agent's work. A storage without the
//! marker never ran any work, so whatever an interrupted initialization left
//! behind is its own debris: a partial clone is removed and redone, and a
//! complete clone whose marker was never written is recognised by its
//! commit and adopted. Anything that is neither ours nor the filesystem's
//! means the storage was not empty, which is refused rather than cloned
//! into.

use std::fs;
use std::path::Path;

use crate::domain::{TaskId, TaskLaunchSpec, TaskWorkspace};
use crate::persistence::ExecutionStore;
use crate::workspace::{checked_out_commit, clone_pinned_commit, clone_resolving_ref};

use super::secrets::MountedSecrets;
use super::storage::{InitializationMarker, TaskStorage};
use super::RunnerError;

/// What [`initialize`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Initialization {
    /// The storage was empty and is initialized now.
    Fresh,
    /// The storage was initialized before; nothing was cloned.
    Resumed,
}

/// Initializes `storage` for `task_id` from `spec`, once.
pub fn initialize(
    storage: &TaskStorage,
    store: &ExecutionStore,
    task_id: &TaskId,
    spec: &TaskLaunchSpec,
    secrets: &MountedSecrets,
) -> Result<Initialization, RunnerError> {
    if let Some(marker) = storage.marker()? {
        if marker.task_id != task_id.as_str() || marker.launch_digest != spec.digest {
            return Err(RunnerError::ForeignStorage(format!(
                "it was initialized for task '{}' with launch {}, not task '{}' with launch {}",
                marker.task_id, marker.launch_digest, task_id, spec.digest
            )));
        }
        // Still checked: a spec file replaced on the storage is a conflict.
        store.initialize_task_launch_spec(task_id, spec)?;
        return Ok(Initialization::Resumed);
    }

    let foreign = storage.foreign_entries()?;
    if !foreign.is_empty() {
        return Err(RunnerError::ForeignStorage(format!(
            "it is not empty and was never initialized: {}",
            foreign.join(", ")
        )));
    }
    storage.prepare()?;
    store.initialize_task_launch_spec(task_id, spec)?;

    let workspace = storage.workspace_dir();
    let commit = match &spec.workspace {
        TaskWorkspace::None => {
            fs::create_dir_all(&workspace).map_err(|source| RunnerError::io(&workspace, source))?;
            None
        }
        TaskWorkspace::Local { path } => {
            copy_local_directory(Path::new(path), &workspace)?;
            None
        }
        TaskWorkspace::Git(identity) => {
            let partial = storage.partial_clone_dir();
            if partial.exists() {
                fs::remove_dir_all(&partial).map_err(|source| RunnerError::io(&partial, source))?;
            }
            let pinned = if workspace.exists() {
                // A clone that finished but whose marker was not written.
                let found = checked_out_commit(&workspace);
                let matches = if identity.is_pinned() {
                    found.as_deref() == Some(identity.commit.as_str())
                } else {
                    // Pinned at admission by that earlier clone: adopt it.
                    found.is_some()
                };
                if !matches {
                    return Err(RunnerError::ForeignStorage(format!(
                        "'{}' exists without an initialization marker and is not a checkout \
                         of {}",
                        workspace.display(),
                        if identity.is_pinned() {
                            identity.commit.as_str()
                        } else {
                            "a git repository"
                        }
                    )));
                }
                found.expect("a matching checkout has a commit")
            } else {
                let credential = identity
                    .credential
                    .as_ref()
                    .map(|reference| secrets.file(reference))
                    .transpose()?;
                let repository = identity
                    .repository
                    .clone()
                    .unwrap_or_else(|| crate::tasks::repository_name(&identity.remote_url));
                let pinned = if identity.is_pinned() {
                    clone_pinned_commit(
                        &repository,
                        &identity.remote_url,
                        &identity.commit,
                        &partial,
                        credential.as_deref(),
                    )?;
                    identity.commit.clone()
                } else {
                    clone_resolving_ref(
                        &repository,
                        &identity.remote_url,
                        identity.git_ref.as_deref(),
                        &partial,
                        credential.as_deref(),
                    )?
                };
                fs::rename(&partial, &workspace)
                    .map_err(|source| RunnerError::io(&workspace, source))?;
                pinned
            };
            Some(pinned)
        }
    };

    storage.write_marker(&InitializationMarker::new(
        task_id.as_str(),
        &spec.digest,
        commit,
    ))?;
    Ok(Initialization::Fresh)
}

/// Copies a sealed host-local workspace only after admission.  This lives at
/// the runner boundary so create acknowledgement never touches the source or
/// creates task storage.
///
/// Symlinks are recreated as symlinks, never followed: a link inside the
/// workspace must not turn a file outside it into content of the copy. A link
/// that resolves outside the source tree, or that cannot be resolved, is
/// refused.
fn copy_local_directory(source: &Path, target: &Path) -> Result<(), RunnerError> {
    let root = source
        .canonicalize()
        .map_err(|error| RunnerError::io(source, error))?;
    copy_tree(&root, &root, target)
}

fn copy_tree(root: &Path, source: &Path, target: &Path) -> Result<(), RunnerError> {
    if !source.is_dir() {
        return Err(RunnerError::ForeignStorage(format!(
            "local workspace '{}' is not a directory",
            source.display()
        )));
    }
    fs::create_dir_all(target).map_err(|error| RunnerError::io(target, error))?;
    for entry in fs::read_dir(source).map_err(|error| RunnerError::io(source, error))? {
        let entry = entry.map_err(|error| RunnerError::io(source, error))?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(|error| RunnerError::io(&from, error))?;
        if kind.is_symlink() {
            let resolved = from
                .canonicalize()
                .map_err(|error| RunnerError::io(&from, error))?;
            if !resolved.starts_with(root) {
                return Err(RunnerError::ForeignStorage(format!(
                    "symlink '{}' leads outside the local workspace",
                    from.display()
                )));
            }
            let link = fs::read_link(&from).map_err(|error| RunnerError::io(&from, error))?;
            std::os::unix::fs::symlink(&link, &to).map_err(|error| RunnerError::io(&to, error))?;
        } else if kind.is_dir() {
            copy_tree(root, &from, &to)?;
        } else {
            fs::copy(&from, &to).map_err(|error| RunnerError::io(&to, error))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("narsor-copy-{name}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn a_local_workspace_is_copied_with_its_internal_symlinks_kept_as_links() {
        let base = temp("inside");
        let source = base.join("source");
        fs::create_dir_all(source.join("dir")).unwrap();
        fs::write(source.join("dir/file.txt"), "content\n").unwrap();
        std::os::unix::fs::symlink("dir/file.txt", source.join("link")).unwrap();

        let target = base.join("target");
        copy_local_directory(&source, &target).unwrap();

        assert_eq!(
            fs::read_to_string(target.join("dir/file.txt")).unwrap(),
            "content\n"
        );
        assert!(
            fs::symlink_metadata(target.join("link"))
                .unwrap()
                .is_symlink(),
            "an internal link stays a link instead of becoming a copy of its target"
        );
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn a_symlink_leading_outside_the_workspace_is_refused_not_dereferenced() {
        let base = temp("escape");
        let source = base.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(base.join("secret.txt"), "secret\n").unwrap();
        std::os::unix::fs::symlink(base.join("secret.txt"), source.join("secret")).unwrap();

        let target = base.join("target");
        let error = copy_local_directory(&source, &target).unwrap_err();

        assert!(
            error.to_string().contains("outside the local workspace"),
            "{error}"
        );
        assert!(
            !target.join("secret").exists(),
            "nothing of the target was copied"
        );
        let _ = fs::remove_dir_all(base);
    }
}
