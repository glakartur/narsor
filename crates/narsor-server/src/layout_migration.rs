//! The server's half of the one-time move to the per-binary layout (R10,
//! KTD10).
//!
//! Before every binary had a directory of its own, the server kept its
//! files directly in the state root, which by default was `global.root`
//! itself, next to the catalogue. At the first start of the new server,
//! before it reads anything, [`migrate`]:
//!
//! 1. does nothing when its marker
//!    ([`MARKER_FILE`](narsor_shared::layout_move::MARKER_FILE) in
//!    `state/narsor-server/`) is there;
//! 2. refuses while an old daemon runs: it takes the old
//!    `daemon.instance.lock` without waiting;
//! 3. refuses while a runner of the old local controller lives: it reads
//!    the old workload records (pid plus start time, or a container). This
//!    read of another binary's files is the one documented exception to the
//!    ownership rule (R6 c);
//! 4. moves its own entries, by allowlist only, to `state/narsor-server/`
//!    ([`SERVER_ENTRIES`]); deletes the stale discovery and autostart files
//!    of the old daemon and its instance lock;
//! 5. writes its marker.
//!
//! Nothing is moved when a refusal applies. A move that fails stops the
//! start and names the entry; a repeat finishes what a crash left.
//!
//! The controller moves its own files when the server starts it with the
//! old directories ([`legacy_controller_dirs`]). The old `executions/`,
//! `launch-specs/` and `observers/` stay where they are for the operator to
//! delete (`docs/operations/updates.md`).

use std::fmt;
use std::fs;
use std::io;
use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use narsor_engine::persistence::ServerStatePaths;
use narsor_shared::config::old_layout::OldRoots;
use narsor_shared::layout_move::{self, Mover};
use narsor_shared::process_liveness::process_alive;

/// The old entries of the state root that are the server's, and their names
/// in `state/narsor-server/`.
pub const SERVER_ENTRIES: [(&str, &str); 6] = [
    ("auth", "auth"),
    ("orchestration", "orchestration"),
    ("materials", "materials"),
    ("observer-queues", "observer-queues"),
    ("agent-harness.json", "agent-harness.json"),
    // The old log: kept under its name, nobody writes it any more.
    ("narsor.log", "narsor.log"),
];

/// Files of the old daemon and its clients that a stopped installation
/// does not need: deleted, never moved.
pub const STALE_FILES: [&str; 4] = [
    "daemon.json",
    "daemon.lock",
    "daemon.lock.guard",
    "daemon.instance.lock",
];

/// The old instance lock, held by a running old daemon.
const OLD_INSTANCE_LOCK: &str = "daemon.instance.lock";

/// The environment mark of every local runner process.
const WORKLOAD_MARK_ENV: &str = "NARSOR_LOCAL_WORKLOAD_UID";

/// The label of every local runner container.
const WORKLOAD_LABEL: &str = "io.narsor.workload";

/// Why the migration did not run or did not finish.
#[derive(Debug)]
pub struct MigrationError(String);

impl fmt::Display for MigrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MigrationError {}

/// What a look at a container name found: running or not.
pub trait ContainerProbe {
    /// Whether the container `name` runs.
    fn running(&self, name: &str) -> bool;
}

/// Asks the local Docker daemon. No `docker`, no daemon or no such
/// container: not running.
#[derive(Debug, Default)]
pub struct DockerProbe;

impl ContainerProbe for DockerProbe {
    fn running(&self, name: &str) -> bool {
        std::process::Command::new("docker")
            .args(["inspect", "--format", "{{.State.Running}}", name])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .is_ok_and(|output| {
                output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true"
            })
    }
}

/// What [`migrate`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The marker was there: an earlier start migrated.
    AlreadyMigrated,
    /// Migrated now; the old entries that moved.
    Migrated { moved: Vec<String> },
}

/// Moves the server's old files to `paths` (see the module documentation).
pub fn migrate(
    old: &OldRoots,
    paths: &ServerStatePaths,
    probe: &dyn ContainerProbe,
    mover: Mover,
) -> Result<Outcome, MigrationError> {
    if layout_move::marker_present(paths.dir()) {
        return Ok(Outcome::AlreadyMigrated);
    }
    // Held until the end: an old daemon cannot start in the middle either.
    let _old_daemon = lock_old_instance(&old.state.join(OLD_INSTANCE_LOCK))?;
    refuse_live_workloads(old, probe)?;

    let mut moved = Vec::new();
    for (name, new_name) in SERVER_ENTRIES {
        let source = old.state.join(name);
        let target = paths.dir().join(new_name);
        match mover.move_entry(&source, &target) {
            Ok(layout_move::Moved::Moved) => moved.push(name.to_string()),
            Ok(_) => {}
            Err(error) => {
                return Err(MigrationError(format!(
                    "the move to the new layout stopped: {error}"
                )))
            }
        }
    }
    for name in STALE_FILES {
        let path = old.state.join(name);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "migration: cannot delete an old file; leaving it");
            }
        }
    }
    layout_move::write_marker(paths.dir()).map_err(|error| {
        MigrationError(format!(
            "cannot write the migration marker in {}: {error}",
            paths.dir().display()
        ))
    })?;
    if !moved.is_empty() {
        tracing::info!(
            from = %old.state.display(),
            to = %paths.dir().display(),
            entries = %moved.join(", "),
            "migration: moved the server's files to the new layout"
        );
    }
    Ok(Outcome::Migrated { moved })
}

/// The old controller directories to hand the local controller
/// (`--legacy-dir`, `--legacy-work-dir`): `None` when neither exists.
pub fn legacy_controller_dirs(old: &OldRoots) -> Option<(PathBuf, PathBuf)> {
    let state = old.local_controller_state();
    let work = old.local_controller_work();
    (state.exists() || work.exists()).then_some((state, work))
}

/// Takes the old instance lock without waiting; `None` when the file does
/// not exist (no old daemon ever ran there).
fn lock_old_instance(path: &Path) -> Result<Option<fs::File>, MigrationError> {
    let file = match fs::OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(MigrationError(format!(
                "cannot open the old daemon's instance lock {}: {error}",
                path.display()
            )))
        }
    };
    // SAFETY: flock on a descriptor this function owns.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::WouldBlock {
        return Err(MigrationError(format!(
            "an old narsor daemon holds {}; stop it with the old `narsor-server stop`, then \
             start the new server again: nothing was moved",
            path.display()
        )));
    }
    Err(MigrationError(format!(
        "cannot lock {}: {error}",
        path.display()
    )))
}

/// The fields of an old `workload.json` that tell whether it lives.
#[derive(Debug, Deserialize)]
struct OldWorkload {
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    start_ticks: Option<u64>,
    #[serde(default)]
    container: Option<OldContainer>,
}

#[derive(Debug, Deserialize)]
struct OldContainer {
    name: String,
}

/// Refuses while a workload of the old local controller lives. An
/// unreadable record is logged and skipped: it names nothing to check.
fn refuse_live_workloads(old: &OldRoots, probe: &dyn ContainerProbe) -> Result<(), MigrationError> {
    let base = old.local_controller_work();
    let mut live = Vec::new();
    for kind in ["workloads", "readers"] {
        let Ok(entries) = fs::read_dir(base.join(kind)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path().join("workload.json");
            let record = match fs::read(&path) {
                Ok(bytes) => serde_json::from_slice::<OldWorkload>(&bytes),
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "migration: skipping an unreadable workload record");
                    continue;
                }
            };
            let record = match record {
                Ok(record) => record,
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "migration: skipping an unreadable workload record");
                    continue;
                }
            };
            if let Some(container) = record.container {
                if probe.running(&container.name) {
                    live.push(format!("container {} ({})", container.name, path.display()));
                }
            } else if let Some(pid) = record.pid {
                if process_alive(pid, record.start_ticks) {
                    live.push(format!("process {pid} ({})", path.display()));
                }
            }
        }
    }
    if live.is_empty() {
        return Ok(());
    }
    Err(MigrationError(format!(
        "cannot move the installation to the new layout while tasks run under the old local \
         controller; nothing was moved. Still running:\n  - {}\n\
         Stop all tasks first (stop the old server and every task), then start the new server \
         again. If a runner outlived its server: kill the processes whose environment holds \
         {WORKLOAD_MARK_ENV}, and remove the containers labelled {WORKLOAD_LABEL} \
         (`docker rm -f $(docker ps -q --filter label={WORKLOAD_LABEL})`); after the update \
         the controller reports them lost and resumes them. See docs/operations/updates.md.",
        live.join("\n  - ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Temp(PathBuf);

    impl Temp {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "narsor-server-migration-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Answers from a list, remembering what it was asked.
    struct Probe {
        running: Vec<&'static str>,
        asked: RefCell<Vec<String>>,
    }

    impl ContainerProbe for Probe {
        fn running(&self, name: &str) -> bool {
            self.asked.borrow_mut().push(name.to_string());
            self.running.contains(&name)
        }
    }

    fn idle() -> Probe {
        Probe {
            running: Vec::new(),
            asked: RefCell::new(Vec::new()),
        }
    }

    /// An old installation with the default layout: one directory with
    /// the catalogue, the server's files and the controller's.
    fn old_installation(root: &Path) -> OldRoots {
        for dir in [
            "workflows",
            "auth",
            "orchestration/tasks",
            "local-controller/workloads",
        ] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(root.join("workflows/flow.yaml"), "name: flow").unwrap();
        fs::write(root.join("auth/owner.json"), "{}").unwrap();
        fs::write(root.join("agent-harness.json"), "{}").unwrap();
        fs::write(root.join("daemon.json"), "{}").unwrap();
        fs::write(root.join("daemon.instance.lock"), "").unwrap();
        OldRoots {
            state: root.to_path_buf(),
            work: root.to_path_buf(),
        }
    }

    fn write_workload(old: &OldRoots, name: &str, record: serde_json::Value) {
        let dir = old.local_controller_work().join("workloads").join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("workload.json"), record.to_string()).unwrap();
    }

    #[test]
    fn moves_the_allowlist_once_and_leaves_the_catalogue() {
        let temp = Temp::new();
        let old = old_installation(&temp.0);
        let paths = ServerStatePaths::new(temp.0.join("state"));

        let first = migrate(&old, &paths, &idle(), Mover::new()).unwrap();

        assert_eq!(
            first,
            Outcome::Migrated {
                moved: vec![
                    "auth".into(),
                    "orchestration".into(),
                    "agent-harness.json".into()
                ]
            }
        );
        assert!(paths.owner_file().is_file());
        assert!(paths.orchestration_tasks_dir().is_dir());
        assert!(
            temp.0.join("workflows/flow.yaml").is_file(),
            "catalogue untouched"
        );
        assert!(
            temp.0.join("local-controller").is_dir(),
            "the controller's own"
        );
        assert!(
            !temp.0.join("daemon.json").exists(),
            "stale discovery file deleted"
        );
        assert!(!temp.0.join("auth").exists());
        assert!(layout_move::marker_present(paths.dir()));

        // A file appearing at the old place later is no longer the server's.
        fs::write(temp.0.join("agent-harness.json"), "{}").unwrap();
        let second = migrate(&old, &paths, &idle(), Mover::new()).unwrap();
        assert_eq!(second, Outcome::AlreadyMigrated);
        assert!(temp.0.join("agent-harness.json").exists());
    }

    #[test]
    fn a_held_old_instance_lock_refuses_before_anything_moves() {
        let temp = Temp::new();
        let old = old_installation(&temp.0);
        let paths = ServerStatePaths::new(temp.0.join("state"));
        let _old_daemon = lock_old_instance(&temp.0.join(OLD_INSTANCE_LOCK))
            .unwrap()
            .unwrap();
        // flock locks belong to the open file: a second open conflicts even
        // in one process.

        let error = migrate(&old, &paths, &idle(), Mover::new()).unwrap_err();

        assert!(
            error.to_string().contains("daemon.instance.lock"),
            "{error}"
        );
        assert!(temp.0.join("auth/owner.json").is_file(), "nothing moved");
        assert!(!layout_move::marker_present(paths.dir()));
    }

    #[test]
    fn a_live_runner_refuses_before_anything_moves() {
        let own = std::process::id();
        let ticks = narsor_shared::process_liveness::process_start_ticks(own).unwrap();
        let cases = [
            (
                serde_json::json!({ "pid": own, "start_ticks": ticks }),
                "process",
            ),
            (
                serde_json::json!({ "pid": 1, "container": { "name": "narsor-local-1" } }),
                "container narsor-local-1",
            ),
        ];
        for (record, named) in cases {
            let temp = Temp::new();
            let old = old_installation(&temp.0);
            let paths = ServerStatePaths::new(temp.0.join("state"));
            write_workload(&old, "task-1", record);
            // A dead runner and an unreadable record next to it change nothing.
            write_workload(
                &old,
                "task-2",
                serde_json::json!({ "pid": own, "start_ticks": ticks + 1 }),
            );
            let broken = old.local_controller_work().join("workloads/task-3");
            fs::create_dir_all(&broken).unwrap();
            fs::write(broken.join("workload.json"), "{not json").unwrap();
            let probe = Probe {
                running: vec!["narsor-local-1"],
                asked: RefCell::new(Vec::new()),
            };

            let error = migrate(&old, &paths, &probe, Mover::new()).unwrap_err();

            let message = error.to_string();
            assert!(message.contains(named), "{message}");
            assert!(message.contains("Stop all tasks first"), "{message}");
            assert!(!message.contains("task-2"), "{message}");
            assert!(temp.0.join("auth/owner.json").is_file(), "nothing moved");
            assert!(!layout_move::marker_present(paths.dir()));
        }
    }

    #[test]
    fn a_failed_move_stops_naming_the_entry() {
        if unsafe { libc::geteuid() } == 0 {
            return; // root ignores the permissions this test relies on
        }
        use std::os::unix::fs::PermissionsExt as _;
        let temp = Temp::new();
        let old = old_installation(&temp.0);
        let paths = ServerStatePaths::new(temp.0.join("state"));
        // The new directory cannot take entries.
        fs::create_dir_all(paths.dir()).unwrap();
        fs::set_permissions(paths.dir(), fs::Permissions::from_mode(0o500)).unwrap();

        let error = migrate(&old, &paths, &idle(), Mover::new()).unwrap_err();

        fs::set_permissions(paths.dir(), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            error
                .to_string()
                .contains(&*temp.0.join("auth").to_string_lossy()),
            "{error}"
        );
        assert!(!layout_move::marker_present(paths.dir()));
        // Once the cause is gone, the next start finishes.
        migrate(&old, &paths, &idle(), Mover::new()).unwrap();
        assert!(paths.owner_file().is_file());
    }
}
