//! The local controller's half of the one-time move to the per-binary
//! layout (R10, KTD10).
//!
//! The old controller kept everything in one `local-controller/` directory
//! of the old state root (credential side) and of the old work root
//! (storage side), one directory with the defaults. `narsor-server` checks
//! that no old daemon and no old runner lives, moves its own files and then
//! starts this controller with `--legacy-dir` (and `--legacy-work-dir`).
//! Before it takes its own credential lock, [`migrate`]:
//!
//! 1. does nothing when its marker is in its state directory;
//! 2. takes the old `credential.lock` (an old controller still stopping is
//!    waited for);
//! 3. records the `controller-id` from the old directory (KTD8), before
//!    anything moves, so the images built earlier stay this controller's;
//! 4. moves its entries ([`STATE_ENTRIES`], [`WORK_ENTRIES`]), splitting
//!    them between its state and work directories;
//!    `secrets/` and `images/` merge into a target a newer `narsor-cli`
//!    already created ([`MERGED_ENTRIES`]);
//! 5. renames the Claude Code session directories of every task, keyed by
//!    the old workspace path, to the key of the new one ([`claude_project_key`]),
//!    and rewrites the workspace paths each stored execution recorded below
//!    the old storage directory ([`rebase_workspace_paths`]);
//! 6. writes its marker, then removes the old directories when empty.
//!
//! A repeat finishes what a crash left: every move and rename is done per
//! entry and skipped when done.

use std::fs;
use std::io;
use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use narsor_model::domain::WorkflowExecution;
use narsor_shared::layout_move::{self, Mover};
use narsor_store::paths::EXECUTIONS_DIR;
use narsor_store::ExecutionStore;

use super::container::image;
use super::paths::{ControllerStatePaths, ControllerWorkPaths};

/// The old entries of the credential side that go to the state directory.
pub const STATE_ENTRIES: [&str; 3] = ["credential", "secrets", "images"];

/// The old entries of the storage side, and whether each goes to the state
/// directory (`true`) or to the work directory.
pub const WORK_ENTRIES: [(&str, bool); 4] = [
    ("storage", false),
    ("workloads", false),
    ("readers", false),
    ("tasks", true),
];

/// The state entries a newer `narsor-cli` may create before the first start
/// of the new server (`secret set`, `image refresh`): an existing target
/// is merged into, entry by entry, instead of refused.
pub const MERGED_ENTRIES: [&str; 2] = ["secrets", "images"];

/// The old credential lock: moved last.
const CREDENTIAL_LOCK: &str = "credential.lock";

/// Where Claude Code keeps its sessions, inside a task's private HOME on
/// its storage.
const CLAUDE_PROJECTS: &str = "home/.claude/projects";

/// The Engine state root inside a task storage (the runner's
/// `TaskStorage::state_dir`).
const TASK_STATE_DIR: &str = "state";

/// The longest key Claude Code uses as it is; a longer one is cut and gets
/// a hash this migration cannot reproduce.
const CLAUDE_KEY_LIMIT: usize = 200;

/// How long the old credential lock is waited for between tries.
const LOCK_RETRY: Duration = Duration::from_millis(100);

/// The controller's old directories (`--legacy-dir`, `--legacy-work-dir`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyDirs {
    /// The old `{state}/local-controller`: credential, secrets, images.
    pub state: PathBuf,
    /// The old `{work}/local-controller`: storage, workloads, readers,
    /// tasks.
    pub work: PathBuf,
}

/// What [`migrate`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The marker was there: an earlier start migrated.
    AlreadyMigrated,
    /// Migrated now: the entries that moved, the Claude session
    /// directories renamed and the stored executions whose workspace paths
    /// were rewritten.
    Migrated {
        moved: Vec<String>,
        sessions: usize,
        executions: usize,
    },
}

/// Moves the controller's old files (see the module documentation).
pub fn migrate(
    legacy: &LegacyDirs,
    state: &ControllerStatePaths,
    work: &ControllerWorkPaths,
    mover: Mover,
    lock_timeout: Duration,
) -> Result<Outcome, String> {
    if layout_move::marker_present(state.dir()) {
        return Ok(Outcome::AlreadyMigrated);
    }
    let lock = lock_old_credential(&legacy.state.join(CREDENTIAL_LOCK), lock_timeout)?;
    if legacy.state.exists() {
        image::seed_controller_id(state, &legacy.state).map_err(|error| {
            format!(
                "cannot record the controller id in {}: {error}",
                state.controller_id_file().display()
            )
        })?;
    }
    // The old storage path as the runners saw it — physical for a process
    // (`getcwd`), as given for a container (mounted at the same path). The
    // old directory stays until the marker is written, so a repeat sees
    // the same paths.
    let old_storage =
        [physical(&legacy.work), absolute(&legacy.work)].map(|dir| dir.join("storage"));

    let mut moved = Vec::new();
    let mut move_one = |from: &Path, name: &str, to: &Path| -> Result<(), String> {
        let merge = MERGED_ENTRIES.contains(&name)
            && is_real_dir(&from.join(name))
            && is_real_dir(&to.join(name));
        let outcome = if merge {
            merge_dir(mover, &from.join(name), &to.join(name))
        } else {
            mover.move_entry(&from.join(name), &to.join(name))
        };
        match outcome {
            Ok(layout_move::Moved::Moved) => {
                moved.push(name.to_string());
                Ok(())
            }
            Ok(_) => Ok(()),
            Err(error) => Err(format!("the move to the new layout stopped: {error}")),
        }
    };
    for name in STATE_ENTRIES {
        move_one(&legacy.state, name, state.dir())?;
    }
    for (name, to_state) in WORK_ENTRIES {
        let to = if to_state { state.dir() } else { work.dir() };
        move_one(&legacy.work, name, to)?;
    }
    move_one(&legacy.state, CREDENTIAL_LOCK, state.dir())?;

    let sessions = rename_claude_sessions(&old_storage, &work.storage_dir());
    let executions = rebase_workspace_paths(&old_storage, &work.storage_dir());
    layout_move::write_marker(state.dir()).map_err(|error| {
        format!(
            "cannot write the migration marker in {}: {error}",
            state.dir().display()
        )
    })?;
    drop(lock);
    // Only empty ones go; anything left stays for the operator.
    let _ = fs::remove_dir(&legacy.state);
    let _ = fs::remove_dir(&legacy.work);
    if !moved.is_empty() {
        tracing::info!(
            state = %state.dir().display(),
            work = %work.dir().display(),
            entries = %moved.join(", "),
            sessions,
            executions,
            "migration: moved the controller's files to the new layout"
        );
    }
    Ok(Outcome::Migrated {
        moved,
        sessions,
        executions,
    })
}

/// Moves every entry of the old directory `from` into the existing `to`,
/// one by one, so a repeat finishes what a crash left. An entry whose name
/// `to` already holds was written after the update and is newer: it stays,
/// and the old one is left beside the old directory with a warning.
fn merge_dir(
    mover: Mover,
    from: &Path,
    to: &Path,
) -> Result<layout_move::Moved, layout_move::MoveError> {
    let fail = |reason: String| layout_move::MoveError {
        source: from.to_path_buf(),
        target: to.to_path_buf(),
        reason,
    };
    let mut names = fs::read_dir(from)
        .map_err(|error| fail(error.to_string()))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<Vec<_>>>()
        .map_err(|error| fail(error.to_string()))?;
    // A copy across filesystems a crash left complete is renamed aside
    // (`<name>.narsor-moved`); `move_entry` of `<name>` finishes it.
    for name in &mut names {
        if let Some(original) = name
            .to_str()
            .and_then(|name| name.strip_suffix(layout_move::MOVED_SUFFIX))
        {
            *name = original.into();
        }
    }
    names.sort();
    names.dedup();
    let mut outcome = layout_move::Moved::AlreadyThere;
    for name in names {
        let (source, target) = (from.join(&name), to.join(&name));
        let kept = fs::symlink_metadata(&source).is_ok() && fs::symlink_metadata(&target).is_ok();
        if kept {
            tracing::warn!(old = %source.display(), kept = %target.display(), "migration: a newer entry of the same name is already in the new layout; keeping it and leaving the old one");
            continue;
        }
        if mover.move_entry(&source, &target)? == layout_move::Moved::Moved {
            outcome = layout_move::Moved::Moved;
        }
    }
    // Only an empty one goes; a kept conflict stays for the operator.
    let _ = fs::remove_dir(from);
    Ok(outcome)
}

/// Whether `path` is a directory itself, not a link to one.
fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// The key Claude Code files the sessions of the working directory `path`
/// under (`~/.claude/projects/<key>`): every character but an ASCII letter
/// or digit becomes `-`, one per UTF-16 unit, as its JavaScript replace
/// does (`/home/a/x.y` → `-home-a-x-y`).
pub fn claude_project_key(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .flat_map(|character| {
            let dashes = if character.is_ascii_alphanumeric() {
                0
            } else {
                character.len_utf16()
            };
            std::iter::repeat_n('-', dashes).chain((dashes == 0).then_some(character))
        })
        .collect()
}

/// Renames, in every task storage below `new_storage`, the Claude Code
/// session directories keyed by the task's old storage path to the key of
/// its new path. `old_storage` is the old storage directory physical and as
/// given, `new_storage` is matched the same way. A task that cannot be
/// read, a key too long to rebuild or a key already taken is skipped with
/// a warning. Returns how many were renamed.
fn rename_claude_sessions(old_storage: &[PathBuf; 2], new_storage: &Path) -> usize {
    let Ok(tasks) = fs::read_dir(new_storage) else {
        return 0;
    };
    let new_storage = [physical(new_storage), absolute(new_storage)];
    let mut renamed = 0;
    for task in tasks.flatten() {
        let Some(projects) = claude_projects_dir(&task.path()) else {
            continue;
        };
        for (old, new) in old_storage.iter().zip(&new_storage) {
            let old_prefix = claude_project_key(&old.join(task.file_name()));
            let new_prefix = claude_project_key(&new.join(task.file_name()));
            if old_prefix != new_prefix {
                renamed += rename_keys(&projects, &old_prefix, &new_prefix);
            }
        }
    }
    renamed
}

/// The Claude Code projects directory of the task storage `task`, when
/// every directory on the way is a real one: a link (to anywhere outside
/// the storage) is not followed, with a warning.
fn claude_projects_dir(task: &Path) -> Option<PathBuf> {
    real_dir_below(task, Path::new(CLAUDE_PROJECTS), "Claude sessions path")
}

/// `base` joined with `relative` when `base` and every directory on the way
/// are real directories, `None` when one is missing or is not a directory.
/// A task writes its own storage: a symbolic link it planted must not send
/// the migration's reads or writes outside it, so one is refused with a
/// warning naming `what`.
fn real_dir_below(base: &Path, relative: &Path, what: &str) -> Option<PathBuf> {
    let mut path = base.to_path_buf();
    let components = relative.components();
    for component in std::iter::once(None).chain(components.map(Some)) {
        if let Some(component) = component {
            path.push(component);
        }
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                tracing::warn!(path = %path.display(), "migration: a symbolic link in a task's {what}; leaving it as it is");
                return None;
            }
            Ok(metadata) if metadata.is_dir() => {}
            _ => return None,
        }
    }
    Some(path)
}

/// Rewrites, in the executions stored in every task storage below
/// `new_storage`, the workspace paths recorded below the task's old
/// storage path to the same place below its new one: a paused task resumes
/// where its work tree is now. `old_storage` and `new_storage` pair the
/// physical and the given form, as for the Claude keys; a path below
/// neither old form is left. A task whose execution cannot be read or
/// written is skipped with a warning. Returns how many executions changed.
fn rebase_workspace_paths(old_storage: &[PathBuf; 2], new_storage: &Path) -> usize {
    let Ok(tasks) = fs::read_dir(new_storage) else {
        return 0;
    };
    let new_storage = [physical(new_storage), absolute(new_storage)];
    let mut rebased = 0;
    for task in tasks.flatten() {
        if !task.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let moves: Vec<(PathBuf, PathBuf)> = old_storage
            .iter()
            .zip(&new_storage)
            .map(|(old, new)| (old.join(task.file_name()), new.join(task.file_name())))
            .collect();
        // Every directory the store reads or writes below the task must be
        // real: the files themselves are replaced by a rename, never followed.
        let executions = Path::new(TASK_STATE_DIR).join(EXECUTIONS_DIR);
        if real_dir_below(&task.path(), &executions, "stored executions").is_none() {
            continue;
        }
        let store = ExecutionStore::new(task.path().join(TASK_STATE_DIR));
        let ids = match store.list_execution_ids() {
            Ok(ids) => ids,
            Err(error) => {
                tracing::warn!(task = %task.path().display(), %error, "migration: cannot list a task's executions; its workspace paths stay as they were");
                continue;
            }
        };
        for id in ids {
            if real_dir_below(
                &task.path(),
                &executions.join(id.as_str()),
                "stored executions",
            )
            .is_none()
            {
                continue;
            }
            let mut execution = match store.load_execution(&id) {
                Ok(execution) => execution,
                Err(error) => {
                    tracing::warn!(task = %task.path().display(), execution = %id, %error, "migration: cannot read a stored execution; its workspace paths stay as they were");
                    continue;
                }
            };
            if !rebase_execution(&mut execution, &moves) {
                continue;
            }
            match store.save_execution(&execution) {
                Ok(()) => rebased += 1,
                Err(error) => {
                    tracing::warn!(task = %task.path().display(), execution = %id, %error, "migration: cannot write a stored execution; its workspace paths stay as they were");
                }
            }
        }
    }
    rebased
}

/// Rewrites every workspace path of `execution` below an old path of
/// `moves` to the paired new one. Whether anything changed.
fn rebase_execution(execution: &mut WorkflowExecution, moves: &[(PathBuf, PathBuf)]) -> bool {
    let workspace = &mut execution.workspace;
    let mut paths: Vec<&mut String> = vec![&mut workspace.path];
    paths.extend(workspace.root.as_mut());
    paths.extend(
        workspace
            .repositories
            .iter_mut()
            .map(|repository| &mut repository.path),
    );
    paths.extend(
        execution
            .context
            .workspace
            .as_mut()
            .map(|info| &mut info.path),
    );
    let mut changed = false;
    for path in paths {
        if let Some(rebased) = rebase_path(path, moves) {
            *path = rebased;
            changed = true;
        }
    }
    changed
}

/// `path` moved from below the first old path of `moves` it lies below.
fn rebase_path(path: &str, moves: &[(PathBuf, PathBuf)]) -> Option<String> {
    moves.iter().find_map(|(old, new)| {
        let rest = Path::new(path).strip_prefix(old).ok()?;
        let rebased = if rest.as_os_str().is_empty() {
            new.clone()
        } else {
            new.join(rest)
        };
        Some(rebased.display().to_string())
    })
}

/// Renames the session directories in `projects` whose key is `old_prefix`
/// or starts with it followed by a path separator's dash.
fn rename_keys(projects: &Path, old_prefix: &str, new_prefix: &str) -> usize {
    let Ok(keys) = fs::read_dir(projects) else {
        return 0;
    };
    let mut renamed = 0;
    for key in keys.flatten() {
        let Some(old_key) = key.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(rest) = old_key.strip_prefix(old_prefix) else {
            continue;
        };
        if !(rest.is_empty() || rest.starts_with('-')) {
            continue;
        }
        let new_key = format!("{new_prefix}{rest}");
        let target = projects.join(&new_key);
        if old_key.len() > CLAUDE_KEY_LIMIT || new_key.len() > CLAUDE_KEY_LIMIT {
            tracing::warn!(session = %key.path().display(), "migration: a Claude session key is too long to rebuild; the task cannot resume this session");
            continue;
        }
        if target.exists() {
            tracing::warn!(session = %key.path().display(), target = %target.display(), "migration: the new Claude session key is taken; leaving the old one");
            continue;
        }
        match fs::rename(key.path(), &target) {
            Ok(()) => renamed += 1,
            Err(error) => {
                tracing::warn!(session = %key.path().display(), %error, "migration: cannot rename a Claude session directory; skipping it");
            }
        }
    }
    renamed
}

/// `path` as `getcwd` reports it below it: links resolved where it exists,
/// made absolute otherwise.
fn physical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| absolute(path))
}

/// `path` made absolute without resolving links, as the controller hands
/// it to a runner.
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Takes the old credential lock, waiting up to `timeout` for an old
/// controller to stop; `None` when there is no such file.
fn lock_old_credential(path: &Path, timeout: Duration) -> Result<Option<fs::File>, String> {
    let file = match fs::OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot open {}: {error}", path.display())),
    };
    let deadline = Instant::now() + timeout;
    loop {
        // SAFETY: flock on a descriptor this function owns.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(file));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::WouldBlock {
            return Err(format!("cannot lock {}: {error}", path.display()));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "an old narsor-controller holds {}; stop it, then start again: nothing was moved",
                path.display()
            ));
        }
        std::thread::sleep(LOCK_RETRY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    struct Temp(PathBuf);

    impl Temp {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "narsor-controller-migration-{}",
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(fs::canonicalize(&dir).unwrap())
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_claude_key_replaces_every_other_character_with_a_dash() {
        for (path, key) in [
            ("/home/a/x.y", "-home-a-x-y"),
            ("/srv/state_1/task-a b", "-srv-state-1-task-a-b"),
            ("/tmp/zażółć", "-tmp-za----"),
            ("/tmp/🦀", "-tmp---"),
        ] {
            assert_eq!(claude_project_key(Path::new(path)), key, "{path}");
        }
    }

    fn old_controller(dir: &Path) -> LegacyDirs {
        fs::create_dir_all(dir.join("secrets")).unwrap();
        fs::write(dir.join("credential"), "token").unwrap();
        fs::set_permissions(dir.join("credential"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(dir.join("credential.lock"), "").unwrap();
        fs::create_dir_all(dir.join("storage/task-1/workspace")).unwrap();
        fs::create_dir_all(dir.join("tasks")).unwrap();
        fs::write(dir.join("tasks/task-1.json"), "{}").unwrap();
        LegacyDirs {
            state: dir.to_path_buf(),
            work: dir.to_path_buf(),
        }
    }

    #[test]
    fn splits_the_old_directory_and_records_the_old_controller_id() {
        let temp = Temp::new();
        let legacy = old_controller(&temp.0.join("local-controller"));
        let old_id = {
            let old_paths = ControllerStatePaths::new(temp.0.join("scratch"));
            image::seed_controller_id(&old_paths, &legacy.state).unwrap();
            fs::read_to_string(old_paths.controller_id_file()).unwrap()
        };
        let state = ControllerStatePaths::new(temp.0.join("state"));
        let work = ControllerWorkPaths::new(temp.0.join("work"));

        let outcome = migrate(&legacy, &state, &work, Mover::new(), Duration::ZERO).unwrap();

        assert!(matches!(outcome, Outcome::Migrated { .. }), "{outcome:?}");
        assert_eq!(
            fs::read_to_string(state.credential_file()).unwrap(),
            "token"
        );
        assert!(state.secrets_dir().is_dir());
        assert!(state.tasks_dir().join("task-1.json").is_file());
        assert!(state.credential_lock_file().is_file());
        assert!(work.storage_dir().join("task-1/workspace").is_dir());
        assert!(!legacy.state.exists(), "the emptied old directory goes");
        assert_eq!(
            image::controller_id(&state),
            old_id.trim(),
            "images built before the move stay this controller's"
        );
        assert_eq!(
            migrate(&legacy, &state, &work, Mover::new(), Duration::ZERO).unwrap(),
            Outcome::AlreadyMigrated
        );
    }

    #[test]
    fn a_held_old_credential_lock_refuses_before_anything_moves() {
        let temp = Temp::new();
        let legacy = old_controller(&temp.0.join("local-controller"));
        let _old = lock_old_credential(&legacy.state.join(CREDENTIAL_LOCK), Duration::ZERO)
            .unwrap()
            .unwrap();
        let state = ControllerStatePaths::new(temp.0.join("state"));
        let work = ControllerWorkPaths::new(temp.0.join("work"));

        let error = migrate(&legacy, &state, &work, Mover::new(), Duration::ZERO).unwrap_err();

        assert!(error.contains("credential.lock"), "{error}");
        assert!(legacy.state.join("credential").is_file());
        assert!(!state.controller_id_file().exists());
    }

    fn paused_execution(workspace_dir: &Path, elsewhere: &str) -> WorkflowExecution {
        use narsor_model::domain::{
            Context, ExecutionStatus, Workspace, WorkspaceInfo, WorkspaceRepository,
        };
        let repository = |name: &str, path: String, primary| WorkspaceRepository {
            name: name.into(),
            path,
            primary,
            branch: None,
            repository: None,
            base_commit: None,
        };
        let primary = workspace_dir.join("app").display().to_string();
        let mut workspace = Workspace::local_folder(primary.clone());
        workspace.root = Some(workspace_dir.display().to_string());
        workspace.repositories = vec![
            repository("app", primary.clone(), true),
            repository("docs", elsewhere.into(), false),
        ];
        WorkflowExecution {
            id: "exec-1".into(),
            workflow_id: "flow".into(),
            workflow_version: 1,
            status: ExecutionStatus::Paused,
            current_state: Some("work".into()),
            started_at: chrono::Utc::now(),
            completed_at: None,
            inputs: serde_json::json!({}),
            context: Context {
                workspace: Some(WorkspaceInfo {
                    id: workspace.id.clone(),
                    path: primary,
                    workspace_type: "local_folder".into(),
                }),
                ..Context::default()
            },
            workspace,
            current_action_execution: None,
            metadata: serde_json::json!({}),
        }
    }

    /// A paused task resumes from its stored execution, whose workspace
    /// paths must follow its storage; a path outside it stays.
    #[test]
    fn stored_workspace_paths_follow_the_moved_task_storage() {
        let temp = Temp::new();
        let legacy = old_controller(&temp.0.join("local-controller"));
        let task = legacy.work.join("storage/task-1");
        let elsewhere = "/srv/elsewhere/docs";
        ExecutionStore::new(task.join(TASK_STATE_DIR))
            .save_execution(&paused_execution(&task.join("workspace"), elsewhere))
            .unwrap();
        let state = ControllerStatePaths::new(temp.0.join("state"));
        let work = ControllerWorkPaths::new(temp.0.join("work"));

        let outcome = migrate(&legacy, &state, &work, Mover::new(), Duration::ZERO).unwrap();

        assert!(
            matches!(outcome, Outcome::Migrated { executions: 1, .. }),
            "{outcome:?}"
        );
        let workspace = work.storage_dir().join("task-1/workspace");
        let primary = workspace.join("app").display().to_string();
        let store = ExecutionStore::new(work.storage_dir().join("task-1").join(TASK_STATE_DIR));
        let execution = store.load_execution("exec-1").unwrap();
        assert_eq!(execution.workspace.path, primary);
        assert_eq!(
            execution.workspace.root.as_deref(),
            Some(workspace.display().to_string().as_str())
        );
        let paths: Vec<_> = execution
            .workspace
            .repositories
            .iter()
            .map(|repository| repository.path.as_str())
            .collect();
        assert_eq!(paths, [primary.as_str(), elsewhere]);
        assert_eq!(execution.context.workspace.unwrap().path, primary);
        assert_eq!(
            store
                .load_context("exec-1")
                .unwrap()
                .workspace
                .unwrap()
                .path,
            primary,
            "the context projection follows too"
        );
        let old_storage = [legacy.work.join("storage"), legacy.work.join("storage")];
        assert_eq!(
            rebase_workspace_paths(&old_storage, &work.storage_dir()),
            0,
            "a repeat after a crash changes nothing"
        );
    }

    /// A task writes its own storage: a `state` it turned into a link to
    /// another directory must not get that directory's executions rewritten.
    #[test]
    fn a_linked_task_state_directory_is_left_alone() {
        let temp = Temp::new();
        let old_storage = temp.0.join("old/storage");
        let new_storage = temp.0.join("new/storage");
        let outside = temp.0.join("outside");
        let planted = paused_execution(&old_storage.join("task-1/workspace"), "/srv/x");
        ExecutionStore::new(&outside)
            .save_execution(&planted)
            .unwrap();
        fs::create_dir_all(new_storage.join("task-1")).unwrap();
        std::os::unix::fs::symlink(&outside, new_storage.join("task-1").join(TASK_STATE_DIR))
            .unwrap();

        let rebased = rebase_workspace_paths(&[old_storage.clone(), old_storage], &new_storage);

        assert_eq!(rebased, 0);
        let kept = ExecutionStore::new(&outside)
            .load_execution("exec-1")
            .unwrap();
        assert_eq!(kept.workspace.path, planted.workspace.path);
    }

    /// `narsor-cli secret set` run after the update and before the first
    /// start of the new server creates the new `secrets/` already.
    #[test]
    fn old_secrets_merge_into_a_newer_secrets_directory_keeping_the_newer_value() {
        let temp = Temp::new();
        let legacy = old_controller(&temp.0.join("local-controller"));
        fs::write(legacy.state.join("secrets/token"), "old").unwrap();
        fs::write(legacy.state.join("secrets/key"), "old key").unwrap();
        let state = ControllerStatePaths::new(temp.0.join("state"));
        fs::create_dir_all(state.secrets_dir()).unwrap();
        fs::write(state.secrets_dir().join("token"), "new").unwrap();
        let work = ControllerWorkPaths::new(temp.0.join("work"));

        migrate(&legacy, &state, &work, Mover::copying(), Duration::ZERO).unwrap();

        assert_eq!(
            fs::read_to_string(state.secrets_dir().join("token")).unwrap(),
            "new"
        );
        assert_eq!(
            fs::read_to_string(state.secrets_dir().join("key")).unwrap(),
            "old key"
        );
        assert!(layout_move::marker_present(state.dir()));
        assert!(
            legacy.state.join("secrets/token").is_file(),
            "the older value stays for the operator"
        );
    }

    /// Behind a linked root a process keys its sessions by the physical
    /// path and a container by the path as given: both follow the move.
    #[test]
    fn sessions_keyed_by_the_physical_and_the_given_path_both_follow_the_move() {
        let temp = Temp::new();
        fs::create_dir_all(temp.0.join("real")).unwrap();
        std::os::unix::fs::symlink(temp.0.join("real"), temp.0.join("link")).unwrap();
        let legacy = old_controller(&temp.0.join("link/local-controller"));
        let given = temp
            .0
            .join("link/local-controller/storage/task-1/workspace");
        let real = temp
            .0
            .join("real/local-controller/storage/task-1/workspace");
        let projects = legacy.work.join("storage/task-1").join(CLAUDE_PROJECTS);
        for (path, session) in [(&given, "container"), (&real, "process")] {
            let dir = projects.join(claude_project_key(path));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("s.jsonl"), session).unwrap();
        }
        let state = ControllerStatePaths::new(temp.0.join("link/state"));
        let work = ControllerWorkPaths::new(temp.0.join("link/work"));

        migrate(&legacy, &state, &work, Mover::new(), Duration::ZERO).unwrap();

        let storage = work.storage_dir().join("task-1");
        let projects = storage.join(CLAUDE_PROJECTS);
        for (path, session) in [
            (storage.join("workspace"), "container"),
            (physical(&storage).join("workspace"), "process"),
        ] {
            let file = projects.join(claude_project_key(&path)).join("s.jsonl");
            assert_eq!(
                fs::read_to_string(&file).unwrap(),
                session,
                "{}",
                file.display()
            );
        }
    }
}
