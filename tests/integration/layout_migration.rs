//! The one-time move of an installation to the per-binary layout (R10,
//! KTD10) by `narsor-migrate`, on fixtures of the old layout, and the
//! server's refusal to start on an old layout that was not moved.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use narsor_controller::local::container::image;
use narsor_controller::local::paths::{ControllerStatePaths, ControllerWorkPaths};
use narsor_engine::persistence::ServerStatePaths;
use narsor_migrate::controller::claude_project_key;
use narsor_migrate::mover::marker_present;
use narsor_migrate::{
    BackupChoice, ContainerProbe, Failure, Installation, Mover, Options, Outcome, Target,
};
use narsor_shared::config::resolve::{resolve_with, Environment, Flags};
use narsor_shared::config::{ConfigCascade, Resolved, Scope};
use sha2::Digest as _;

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

/// Answers Docker questions from a fixed answer.
struct Docker(Result<bool, String>);

impl ContainerProbe for Docker {
    fn running(&self, _name: &str) -> Result<bool, String> {
        self.0.clone()
    }
}

fn no_containers() -> Docker {
    Docker(Ok(false))
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

/// The new directories of the installation `config` resolves.
struct NewLayout {
    server: ServerStatePaths,
    state: ControllerStatePaths,
    work: ControllerWorkPaths,
}

fn new_layout(config: &Resolved) -> NewLayout {
    NewLayout {
        server: ServerStatePaths::new(config.state_dir()),
        state: ControllerStatePaths::new(config.state_dir()),
        work: ControllerWorkPaths::new(config.work_dir()),
    }
}

/// What the installer does with `--yes`.
fn migrate(config: &Resolved, options: Options, docker: &Docker) -> Result<Outcome, Failure> {
    let target = Target::Installation(Installation::of(config));
    let mut out = Vec::new();
    narsor_migrate::run(&target, &options, docker, &mut || true, &mut out)
}

fn without_backup(mover: Mover) -> Options {
    Options {
        backup: BackupChoice::Skip,
        mover,
        ..Options::default()
    }
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
    file(old_state.join("web-session"), "stale");
    file(old_state.join("executions/e1/state.json"), "old execution");
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

/// Every old entry is at its new place.
fn assert_migrated(new: &NewLayout) {
    let NewLayout {
        server,
        state,
        work,
    } = new;
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

/// The image label the old controller computed from its directory.
fn old_controller_id(old_dir: &Path) -> String {
    let digest = sha2::Sha256::digest(
        fs::canonicalize(old_dir)
            .unwrap()
            .as_os_str()
            .as_encoded_bytes(),
    );
    digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn a_default_installation_moves_below_state_and_work_and_leaves_the_catalogue() {
    let temp = TempDir::new();
    let root = temp.0.join("narsor");
    old_installation(&root, &root);
    fs::create_dir_all(root.join("workflows")).unwrap();
    fs::write(root.join("workflows/flow.yaml"), "name: flow").unwrap();
    let config = resolved(&[("NARSOR_GLOBAL__ROOT", &root)]);
    let new = new_layout(&config);
    let old_id = old_controller_id(&root.join("local-controller"));

    let dry = migrate(
        &config,
        Options {
            dry_run: true,
            ..without_backup(Mover::new())
        },
        &no_containers(),
    );
    assert!(matches!(dry, Ok(Outcome::DryRun(_))), "{dry:?}");
    assert!(
        root.join("auth/owner.json").is_file(),
        "a dry run moves nothing"
    );

    let outcome = migrate(&config, without_backup(Mover::new()), &no_containers());

    assert!(matches!(outcome, Ok(Outcome::Migrated(_))), "{outcome:?}");
    assert_eq!(new.server.dir(), root.join("state/narsor-server"));
    assert_eq!(new.state.dir(), root.join("state/narsor-controller-local"));
    assert_eq!(new.work.dir(), root.join("work/narsor-controller-local"));
    assert_migrated(&new);
    assert_eq!(
        image::controller_id(&new.state),
        old_id,
        "images built before the move stay this controller's"
    );
    assert!(
        root.join("workflows/flow.yaml").is_file(),
        "catalogue untouched"
    );
    assert!(!root.join("daemon.json").exists(), "stale file deleted");
    assert!(
        !root.join("web-session").exists(),
        "old web session deleted"
    );
    assert!(
        root.join("executions/e1/state.json").is_file(),
        "left for the operator"
    );
    assert!(!root.join("local-controller").exists());

    // The next run finds the marker and moves nothing.
    fs::write(root.join("agent-harness.json"), "new").unwrap();
    let again = migrate(&config, without_backup(Mover::new()), &no_containers());
    assert_eq!(again, Ok(Outcome::NothingToMigrate));
    assert!(root.join("agent-harness.json").is_file());
    assert_eq!(
        fs::read_to_string(new.server.agent_harness_selection_file()).unwrap(),
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
    let new = new_layout(&config);

    migrate(&config, without_backup(Mover::copying()), &no_containers()).unwrap();

    assert_eq!(new.server.dir(), state_root.join("narsor-server"));
    assert_eq!(new.work.dir(), work_root.join("narsor-controller-local"));
    assert_migrated(&new);
    assert!(!state_root.join("local-controller").exists());
    assert!(!work_root.join("local-controller").exists());
}

/// A run that stopped halfway — some entries moved, one copy unfinished,
/// no marker — is finished by the next run.
#[test]
fn a_run_after_a_crash_halfway_finishes_the_move() {
    let temp = TempDir::new();
    let root = temp.0.join("narsor");
    old_installation(&root, &root);
    let config = resolved(&[("NARSOR_GLOBAL__ROOT", &root)]);
    let new = new_layout(&config);
    // Moved before the crash.
    fs::create_dir_all(new.server.dir()).unwrap();
    fs::rename(root.join("auth"), new.server.auth_dir()).unwrap();
    fs::create_dir_all(new.state.dir()).unwrap();
    fs::rename(
        root.join("local-controller/secrets"),
        new.state.secrets_dir(),
    )
    .unwrap();
    // A copy across filesystems the crash interrupted.
    let staging = new.server.dir().join("orchestration.narsor-moving");
    fs::create_dir_all(&staging).unwrap();
    fs::write(staging.join("partial"), "").unwrap();

    migrate(&config, without_backup(Mover::new()), &no_containers()).unwrap();

    assert_migrated(&new);
    assert!(!staging.exists());
    assert!(!new.server.orchestration_dir().join("partial").exists());
}

/// Every file below `dir`, by path relative to it, with its bytes and mode.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (Vec<u8>, u32)> {
    fn walk(base: &Path, dir: &Path, found: &mut BTreeMap<PathBuf, (Vec<u8>, u32)>) {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, found);
            } else {
                let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
                found.insert(
                    path.strip_prefix(base).unwrap().to_path_buf(),
                    (fs::read(&path).unwrap(), mode),
                );
            }
        }
    }
    let mut found = BTreeMap::new();
    walk(dir, dir, &mut found);
    found
}

/// The backup holds the old entries, not the catalogue, and extracting it
/// at `/` (here: at a scratch directory) gives the old files back.
#[test]
fn the_backup_holds_the_old_entries_and_gives_them_back() {
    let temp = TempDir::new();
    let root = temp.0.join("narsor");
    old_installation(&root, &root);
    fs::create_dir_all(root.join("workflows")).unwrap();
    fs::write(root.join("workflows/flow.yaml"), "name: flow").unwrap();
    let before = snapshot(&root);
    let config = resolved(&[("NARSOR_GLOBAL__ROOT", &root)]);

    let outcome = migrate(
        &config,
        Options {
            mover: Mover::new(),
            ..Options::default()
        },
        &no_containers(),
    );

    let Ok(Outcome::Migrated(summary)) = outcome else {
        panic!("{outcome:?}");
    };
    let archive = summary.backup.expect("a backup").path;
    assert_eq!(archive.parent().unwrap(), root.join("backups"));
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&root.join("backups")), 0o700);
    assert_eq!(mode(&archive), 0o600);

    let restored = temp.0.join("restored");
    let decoder = zstd::stream::read::Decoder::new(fs::File::open(&archive).unwrap()).unwrap();
    let mut unpack = tar::Archive::new(decoder);
    unpack.set_preserve_permissions(true);
    unpack.unpack(&restored).unwrap();
    let restored_root = restored.join(root.strip_prefix("/").unwrap());
    let mut expected = before;
    expected.retain(|path, _| !path.starts_with("workflows"));
    assert_eq!(snapshot(&restored_root), expected);
}

/// Every refusal comes before anything is backed up or moved.
#[test]
fn a_running_program_or_an_unanswered_docker_refuses_before_anything_changes() {
    let own = std::process::id();
    let ticks = narsor_shared::process_liveness::process_start_ticks(own).unwrap();
    type Arrange = Box<dyn Fn(&Path) -> Option<fs::File>>;
    let workload = |record: serde_json::Value| -> Arrange {
        Box::new(move |root: &Path| {
            let dir = root.join("local-controller/workloads/task-2");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("workload.json"), record.to_string()).unwrap();
            None
        })
    };
    let container = serde_json::json!({ "pid": 1, "container": { "name": "narsor-local-1" } });
    let cases: Vec<(&str, Arrange, Docker, bool, Option<&str>)> = vec![
        (
            "old daemon",
            Box::new(|root: &Path| {
                let path = root.join("daemon.instance.lock");
                fs::write(&path, "").unwrap();
                let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
                use std::os::fd::AsRawFd as _;
                assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) }, 0);
                Some(file)
            }),
            no_containers(),
            false,
            Some("daemon.instance.lock"),
        ),
        (
            "live runner process",
            workload(serde_json::json!({ "pid": own, "start_ticks": ticks })),
            no_containers(),
            false,
            Some("Stop all tasks first"),
        ),
        (
            "running container",
            workload(container.clone()),
            Docker(Ok(true)),
            false,
            Some("container narsor-local-1"),
        ),
        (
            "Docker without an answer",
            workload(container.clone()),
            Docker(Err("cannot run docker".into())),
            false,
            Some("--assume-containers-stopped"),
        ),
        (
            "Docker without an answer, checked by hand",
            workload(container),
            Docker(Err("cannot run docker".into())),
            true,
            None,
        ),
    ];
    for (name, arrange, docker, assume_stopped, refusal) in cases {
        let temp = TempDir::new();
        let root = temp.0.join("narsor");
        old_installation(&root, &root);
        let _held = arrange(&root);
        let config = resolved(&[("NARSOR_GLOBAL__ROOT", &root)]);

        let outcome = migrate(
            &config,
            Options {
                assume_containers_stopped: assume_stopped,
                ..Options::default()
            },
            &docker,
        );

        match refusal {
            Some(text) => {
                let Err(Failure::Refused(message)) = &outcome else {
                    panic!("{name}: {outcome:?}");
                };
                assert!(message.contains(text), "{name}: {message}");
                assert!(
                    root.join("auth/owner.json").is_file(),
                    "{name}: nothing moved"
                );
                assert!(!root.join("backups").exists(), "{name}: no backup");
            }
            None => assert!(
                matches!(outcome, Ok(Outcome::Migrated(_))),
                "{name}: {outcome:?}"
            ),
        }
    }
}

/// The server moves nothing: it refuses an old layout that was not moved
/// and starts on a fresh or a moved one.
#[test]
fn the_server_refuses_an_old_layout_until_it_is_moved() {
    type Arrange = fn(&Path);
    let cases: [(&str, Arrange, bool); 3] = [
        ("old layout", |root| old_installation(root, root), false),
        ("fresh root", |root| fs::create_dir_all(root).unwrap(), true),
        (
            "moved",
            |root| {
                old_installation(root, root);
                let config = resolved(&[("NARSOR_GLOBAL__ROOT", root)]);
                migrate(&config, without_backup(Mover::new()), &no_containers()).unwrap();
                // An old file appearing after the move changes nothing.
                fs::create_dir_all(root.join("auth")).unwrap();
            },
            true,
        ),
    ];
    for (name, arrange, starts) in cases {
        let temp = TempDir::new();
        let root = temp.0.join("narsor");
        arrange(&root);
        let config = resolved(&[("NARSOR_GLOBAL__ROOT", &root)]);

        let result = narsor_server::layout_guard::refuse_old_layout(
            &config,
            &ServerStatePaths::new(config.state_dir()),
        );

        assert_eq!(result.is_ok(), starts, "{name}: {result:?}");
        if let Err(message) = result {
            assert!(message.contains("narsor-migrate"), "{message}");
            assert!(
                message.contains(&*root.join("auth").to_string_lossy()),
                "{message}"
            );
        }
    }
}
