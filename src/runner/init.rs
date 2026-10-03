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

use crate::domain::{TaskId, TaskLaunchSpec, TaskWorkspace};
use crate::persistence::ExecutionStore;
use crate::workspace::{checked_out_commit, clone_pinned_commit};

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
        TaskWorkspace::Git(identity) => {
            let partial = storage.partial_clone_dir();
            if partial.exists() {
                fs::remove_dir_all(&partial).map_err(|source| RunnerError::io(&partial, source))?;
            }
            if workspace.exists() {
                // A clone that finished but whose marker was not written.
                if checked_out_commit(&workspace).as_deref() != Some(identity.commit.as_str()) {
                    return Err(RunnerError::ForeignStorage(format!(
                        "'{}' exists without an initialization marker and is not a checkout \
                         of {}",
                        workspace.display(),
                        identity.commit
                    )));
                }
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
                clone_pinned_commit(
                    &repository,
                    &identity.remote_url,
                    &identity.commit,
                    &partial,
                    credential.as_deref(),
                )?;
                fs::rename(&partial, &workspace)
                    .map_err(|source| RunnerError::io(&workspace, source))?;
            }
            Some(identity.commit.clone())
        }
    };

    storage.write_marker(&InitializationMarker::new(
        task_id.as_str(),
        &spec.digest,
        commit,
    ))?;
    Ok(Initialization::Fresh)
}
