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
                        identity.ref_origin,
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
/// that resolves outside the source tree, that cannot be resolved, or that is
/// absolute (it would keep pointing into the host's original) is refused, and
/// so is anything that is not a regular file or a directory.
///
/// The sealed path was canonical when the task was created. It must still be
/// exactly that: a path (or one of its parents) turned into a symlink since
/// would redirect the copy to a directory nobody approved.
fn copy_local_directory(source: &Path, target: &Path) -> Result<(), RunnerError> {
    let root = source
        .canonicalize()
        .map_err(|error| RunnerError::io(source, error))?;
    if root != source {
        return Err(RunnerError::ForeignStorage(format!(
            "local workspace '{}' now resolves to '{}'; it changed since the task was created",
            source.display(),
            root.display()
        )));
    }
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
            if link.is_absolute() {
                return Err(RunnerError::ForeignStorage(format!(
                    "symlink '{}' is absolute; it would keep pointing into the original \
                     workspace",
                    from.display()
                )));
            }
            std::os::unix::fs::symlink(&link, &to).map_err(|error| RunnerError::io(&to, error))?;
        } else if kind.is_dir() {
            copy_tree(root, &from, &to)?;
        } else if kind.is_file() {
            fs::copy(&from, &to).map_err(|error| RunnerError::io(&to, error))?;
        } else {
            return Err(RunnerError::ForeignStorage(format!(
                "'{}' is neither a regular file, a directory nor a symlink",
                from.display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{GitIdentity, RefOrigin, ResolvedEnvironment, ResourceProfile};

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

    #[test]
    fn a_workspace_path_turned_into_a_symlink_since_sealing_is_refused() {
        let base = temp("swapped");
        let elsewhere = base.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join("secret.txt"), "secret\n").unwrap();
        let sealed = base.join("sealed");
        std::os::unix::fs::symlink(&elsewhere, &sealed).unwrap();

        let target = base.join("target");
        let error = copy_local_directory(&sealed, &target).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("changed since the task was created"),
            "{error}"
        );
        assert!(!target.exists());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn an_absolute_symlink_and_a_special_file_are_refused() {
        let base = temp("special");
        let source = base.join("source");
        fs::create_dir_all(source.join("dir")).unwrap();
        fs::write(source.join("dir/file.txt"), "x\n").unwrap();
        let source = source.canonicalize().unwrap();
        std::os::unix::fs::symlink(source.join("dir/file.txt"), source.join("abs")).unwrap();
        let error = copy_local_directory(&source, &base.join("t1")).unwrap_err();
        assert!(error.to_string().contains("is absolute"), "{error}");
        fs::remove_file(source.join("abs")).unwrap();

        let fifo = source.join("pipe");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        let error = copy_local_directory(&source, &base.join("t2")).unwrap_err();
        assert!(
            error.to_string().contains("neither a regular file"),
            "{error}"
        );
        let _ = fs::remove_dir_all(base);
    }

    // The admitted clone (R39) and one storage, one task. Cloning is what is
    // under test, and it is done by the `git` program (a runtime
    // prerequisite of the runner): the "remote" is a local bare repository.

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=Runner Test",
                "-c",
                "user.email=runner@example.com",
            ])
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A bare "remote" with one commit, and that commit's id.
    fn origin(base: &Path) -> (std::path::PathBuf, String) {
        let work = base.join("origin-work");
        fs::create_dir_all(&work).unwrap();
        git(&work, &["-c", "init.defaultBranch=main", "init", "-q"]);
        fs::write(work.join("README.md"), "pinned\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "pinned"]);
        let bare = base.join("origin.git");
        git(
            base,
            &[
                "clone",
                "-q",
                "--bare",
                &work.display().to_string(),
                &bare.display().to_string(),
            ],
        );
        let head = git(&work, &["rev-parse", "HEAD"]);
        (bare, head)
    }

    const WORKFLOW: &str = r#"
name: one-step
version: 1
states:
  work:
    action:
      type: agent
      prompt: Do the work.
    choices: [done]
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states: [completed]
"#;

    fn spec(workspace: TaskWorkspace) -> TaskLaunchSpec {
        let definition = crate::workflow::load_from_str(WORKFLOW).unwrap();
        TaskLaunchSpec::new(
            definition.id.clone(),
            definition.version,
            serde_json::json!({}),
            workspace,
            ResolvedEnvironment::harness_only("claude-code"),
            ResourceProfile::new(1000, 2048),
        )
        .with_definition(&definition)
        .unwrap()
    }

    fn unpinned(
        bare: &Path,
        git_ref: Option<&str>,
        ref_origin: Option<RefOrigin>,
    ) -> TaskWorkspace {
        TaskWorkspace::Git(GitIdentity {
            repository: Some("demo".to_string()),
            remote_url: bare.display().to_string(),
            commit: String::new(),
            git_ref: git_ref.map(str::to_string),
            ref_origin,
            credential: None,
        })
    }

    fn task() -> TaskId {
        TaskId::parse("task-1").unwrap()
    }

    /// A workspace left unpinned at creation is pinned by the admitted
    /// clone: the remote's HEAD is resolved once, and that commit is the
    /// workspace, the marker and the Engine's base commit.
    #[test]
    fn an_unpinned_workspace_is_pinned_by_the_admitted_clone() {
        let base = temp("unpinned");
        let (bare, head) = origin(&base);
        let storage = TaskStorage::new(base.join("storage"));
        fs::create_dir_all(storage.root()).unwrap();
        let spec = spec(unpinned(&bare, None, None));

        let initialized = initialize(
            &storage,
            &storage.store(),
            &task(),
            &spec,
            &MountedSecrets::new(None),
        )
        .unwrap();

        assert_eq!(initialized, Initialization::Fresh);
        assert_eq!(git(&storage.workspace_dir(), &["rev-parse", "HEAD"]), head);
        let marker = storage.marker().unwrap().expect("the marker");
        assert_eq!(marker.workspace_commit.as_deref(), Some(head.as_str()));
        let workspace = super::super::workspace(&storage, &spec);
        assert_eq!(workspace.base_commit.as_deref(), Some(head.as_str()));
        assert!(!storage.partial_clone_dir().exists());
        let _ = fs::remove_dir_all(base);
    }

    /// A branch that does not exist fails the admitted clone, and the error
    /// says which setting named it; nothing is left behind.
    #[test]
    fn a_missing_branch_names_the_setting_it_came_from() {
        let base = temp("missing-branch");
        let (bare, _) = origin(&base);
        for (index, (ref_origin, words)) in [
            (Some(RefOrigin::Repository), "z konfiguracji repozytorium"),
            (None, "not found in repository"),
        ]
        .into_iter()
        .enumerate()
        {
            let storage = TaskStorage::new(base.join(format!("storage-{index}")));
            fs::create_dir_all(storage.root()).unwrap();
            let spec = spec(unpinned(&bare, Some("missing"), ref_origin));

            let error = initialize(
                &storage,
                &storage.store(),
                &task(),
                &spec,
                &MountedSecrets::new(None),
            )
            .unwrap_err();

            let message = error.to_string();
            assert!(matches!(error, RunnerError::Workspace(_)), "{message}");
            assert!(message.contains("missing"), "{message}");
            assert!(message.contains(words), "{message}");
            if ref_origin.is_none() {
                assert!(!message.contains("(z "), "{message}");
            }
            assert!(storage.marker().unwrap().is_none());
            assert!(!storage.workspace_dir().exists());
        }
        let _ = fs::remove_dir_all(base);
    }

    /// A storage that holds anything of somebody else's is never
    /// initialized, and the refusal names what it found.
    #[test]
    fn a_storage_that_is_not_empty_is_never_initialized() {
        let base = temp("not-empty");
        let storage = TaskStorage::new(base.join("storage"));
        fs::create_dir_all(storage.root()).unwrap();
        fs::write(storage.root().join("someone-elses-file"), "data").unwrap();
        let spec = spec(TaskWorkspace::None);

        let error = initialize(
            &storage,
            &storage.store(),
            &task(),
            &spec,
            &MountedSecrets::new(None),
        )
        .unwrap_err();

        assert!(matches!(error, RunnerError::ForeignStorage(_)), "{error}");
        assert!(error.to_string().contains("someone-elses-file"), "{error}");
        assert!(storage.marker().unwrap().is_none());
        assert!(storage
            .store()
            .load_task_launch_spec(&task())
            .unwrap()
            .is_none());
        assert!(storage.store().list_execution_ids().unwrap().is_empty());
        let _ = fs::remove_dir_all(base);
    }
}
