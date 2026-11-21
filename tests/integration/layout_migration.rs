//! The one-time move of an installation to the per-binary layout (R10,
//! KTD10): the server's half, then the local controller's, as the server
//! starts them, on fixtures of the old layout.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use narsor_controller::local::layout_migration::{
    self as controller, claude_project_key, LegacyDirs,
};
use narsor_controller::local::paths::{ControllerStatePaths, ControllerWorkPaths};
use narsor_engine::persistence::ServerStatePaths;
use narsor_server::layout_migration::{self as server, ContainerProbe};
use narsor_shared::config::old_layout::OldRoots;
use narsor_shared::config::resolve::{resolve_with, Environment, Flags};
use narsor_shared::config::{ConfigCascade, Resolved, Scope};
use narsor_shared::layout_move::{marker_present, Mover};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("narsor-layout-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        Self(fs::canonicalize(&dir).unwrap())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// No container runs.
struct NoContainers;

impl ContainerProbe for NoContainers {
    fn running(&self, _name: &str) -> bool {
        false
    }
}

fn resolved(variables: &[(&str, &Path)]) -> Resolved {
    let environment: Environment = variables
        .iter()
        .map(|(name, value)| (name.to_string(), value.display().to_string()))
        .collect();
    resolve_with(
        &Flags::default(),
        &environment,
        &ConfigCascade::with_dirs(None, None, None),
        Scope::SERVER,
    )
}

/// What a server start does: its own move, then the controller's with the
/// old directories it hands over.
fn start(
    config: &Resolved,
    mover: Mover,
) -> (ServerStatePaths, ControllerStatePaths, ControllerWorkPaths) {
    let old = OldRoots::of(config);
    let paths = ServerStatePaths::new(config.state_dir());
    server::migrate(&old, &paths, &NoContainers, mover).unwrap();
    let state = ControllerStatePaths::new(config.state_dir());
    let work = ControllerWorkPaths::new(config.work_dir());
    if let Some((old_state, old_work)) = server::legacy_controller_dirs(&old) {
        let legacy = LegacyDirs {
            state: old_state,
            work: old_work,
        };
        controller::migrate(&legacy, &state, &work, mover, Duration::ZERO).unwrap();
    }
    (paths, state, work)
}

/// The old layout: server files in `old_state`, the controller's
/// credential side in `old_state/local-controller`, its storage side in
/// `old_work/local-controller`, with a stopped task whose Claude session
/// is keyed by its old workspace path.
fn old_installation(old_state: &Path, old_work: &Path) {
    let file = |path: PathBuf, text: &str| {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    };
    file(old_state.join("auth/owner.json"), "owner");
    file(old_state.join("orchestration/tasks/t1/task.json"), "task");
    file(old_state.join("materials/m1/material.json"), "material");
    file(old_state.join("observer-queues/entries/e1.json"), "entry");
    file(old_state.join("agent-harness.json"), "harness");
    file(old_state.join("daemon.json"), "stale");
    let controller = old_state.join("local-controller");
    file(controller.join("credential"), "secret-token");
    fs::set_permissions(
        controller.join("credential"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    file(controller.join("secrets/github"), "ghp");
    file(controller.join("images/abc.json"), "{}");
    let storage = old_work.join("local-controller");
    file(storage.join("tasks/task-1.json"), "{}");
    file(
        storage.join("workloads/task-1/workload.json"),
        r#"{"pid": 999999999, "start_ticks": 1}"#,
    );
    let task = storage.join("storage/task-1");
    file(task.join("workspace/code/README.md"), "code");
    let old_key = claude_project_key(&task.join("workspace/code"));
    file(
        task.join(format!("home/.claude/projects/{old_key}/s1.jsonl")),
        "session",
    );
}

/// Every old entry is at its new place, the catalogue is where it was.
fn assert_migrated(
    server: &ServerStatePaths,
    state: &ControllerStatePaths,
    work: &ControllerWorkPaths,
) {
    assert_eq!(fs::read_to_string(server.owner_file()).unwrap(), "owner");
    assert!(server
        .orchestration_tasks_dir()
        .join("t1/task.json")
        .is_file());
    assert!(server.dir().join("materials/m1/material.json").is_file());
    assert!(server
        .dir()
        .join("observer-queues/entries/e1.json")
        .is_file());
    assert!(server.agent_harness_selection_file().is_file());
    assert_eq!(
        fs::read_to_string(state.credential_file()).unwrap(),
        "secret-token"
    );
    let mode = fs::metadata(state.credential_file())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "the credential stays private");
    assert!(state.secrets_dir().join("github").is_file());
    assert!(state.images_dir().join("abc.json").is_file());
    assert!(state.tasks_dir().join("task-1.json").is_file());
    assert!(state.controller_id_file().is_file());
    assert!(work.workloads_dir().join("task-1/workload.json").is_file());
    let task = work.storage_dir().join("task-1");
    assert!(task.join("workspace/code/README.md").is_file());
    let new_key = claude_project_key(&task.join("workspace/code"));
    assert!(
        task.join(format!("home/.claude/projects/{new_key}/s1.jsonl"))
            .is_file(),
        "the stopped task resumes its Claude session from the new workspace path"
    );
    assert!(marker_present(server.dir()));
    assert!(marker_present(state.dir()));
}

#[test]
fn a_default_installation_moves_below_state_and_work_and_leaves_the_catalogue() {
    let temp = TempDir::new();
    let root = temp.0.join("narsor");
    old_installation(&root, &root);
    fs::create_dir_all(root.join("workflows")).unwrap();
    fs::write(root.join("workflows/flow.yaml"), "name: flow").unwrap();
    let config = resolved(&[("NARSOR_GLOBAL__ROOT", &root)]);

    let (server, state, work) = start(&config, Mover::new());

    assert_eq!(server.dir(), root.join("state/narsor-server"));
    assert_eq!(state.dir(), root.join("state/narsor-controller-local"));
    assert_eq!(work.dir(), root.join("work/narsor-controller-local"));
    assert_migrated(&server, &state, &work);
    assert!(
        root.join("workflows/flow.yaml").is_file(),
        "catalogue untouched"
    );
    assert!(
        !root.join("daemon.json").exists(),
        "stale discovery file deleted"
    );
    assert!(!root.join("local-controller").exists());

    // The next start finds both markers and moves nothing.
    fs::write(root.join("agent-harness.json"), "new").unwrap();
    fs::create_dir_all(root.join("local-controller/secrets")).unwrap();
    start(&config, Mover::new());
    assert!(root.join("agent-harness.json").is_file());
    assert!(root.join("local-controller/secrets").is_dir());
    assert_eq!(
        fs::read_to_string(server.agent_harness_selection_file()).unwrap(),
        "harness"
    );
}

/// Explicit roots apart, and on another filesystem as far as the move
/// knows (every entry is copied, compared and the source deleted).
#[test]
fn explicit_roots_move_below_their_own_root_also_by_copy() {
    let temp = TempDir::new();
    let (state_root, work_root) = (temp.0.join("var-state"), temp.0.join("scratch"));
    old_installation(&state_root, &work_root);
    let config = resolved(&[
        ("NARSOR_GLOBAL__ROOT", &temp.0.join("root")),
        ("NARSOR_SERVER__STATE", &state_root),
        ("NARSOR_SERVER__WORK", &work_root),
    ]);

    let (server, state, work) = start(&config, Mover::copying());

    assert_eq!(server.dir(), state_root.join("narsor-server"));
    assert_eq!(work.dir(), work_root.join("narsor-controller-local"));
    assert_migrated(&server, &state, &work);
    assert!(!state_root.join("local-controller").exists());
    assert!(!work_root.join("local-controller").exists());
}

/// A start that stopped halfway — some entries moved, one copy unfinished,
/// no marker — is finished by the next start.
#[test]
fn a_start_after_a_crash_halfway_finishes_the_move() {
    let temp = TempDir::new();
    let root = temp.0.join("narsor");
    old_installation(&root, &root);
    let config = resolved(&[("NARSOR_GLOBAL__ROOT", &root)]);
    let server = ServerStatePaths::new(config.state_dir());
    let state = ControllerStatePaths::new(config.state_dir());
    let work = ControllerWorkPaths::new(config.work_dir());
    // Moved before the crash.
    fs::create_dir_all(server.dir()).unwrap();
    fs::rename(root.join("auth"), server.auth_dir()).unwrap();
    fs::create_dir_all(state.dir()).unwrap();
    fs::rename(root.join("local-controller/secrets"), state.secrets_dir()).unwrap();
    // A copy across filesystems the crash interrupted.
    let staging = server.dir().join("orchestration.narsor-moving");
    fs::create_dir_all(&staging).unwrap();
    fs::write(staging.join("partial"), "").unwrap();

    let (server, state, work_after) = start(&config, Mover::new());

    assert_eq!(work_after, work);
    assert_migrated(&server, &state, &work);
    assert!(!staging.exists());
    assert!(!server.orchestration_dir().join("partial").exists());
}
