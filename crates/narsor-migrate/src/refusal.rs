//! What must hold before anything is backed up or moved (KTD10): no old or
//! new program holds the installation's locks, no runner of the old local
//! controller lives, and no entry to move is a symbolic link.
//!
//! Reading the old workload records is the one documented read of another
//! binary's files (R6 c). A container named in a record is asked about
//! through `docker`; when Docker cannot answer, the check fails closed: the
//! operator checks `docker ps` and confirms with
//! `--assume-containers-stopped`.

use std::fs;
use std::io;
use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use narsor_shared::process_liveness::process_alive;

/// The environment mark of every local runner process.
const WORKLOAD_MARK_ENV: &str = "NARSOR_LOCAL_WORKLOAD_UID";

/// The label of every local runner container.
const WORKLOAD_LABEL: &str = "io.narsor.workload";

/// Whether a container runs, as far as something can tell.
pub trait ContainerProbe {
    /// `Ok(true)` when the container `name` runs, `Ok(false)` when it does
    /// not or does not exist, `Err` when there is no answer.
    fn running(&self, name: &str) -> Result<bool, String>;
}

/// Asks the local Docker daemon with `docker inspect`.
#[derive(Debug, Default)]
pub struct DockerProbe;

impl ContainerProbe for DockerProbe {
    fn running(&self, name: &str) -> Result<bool, String> {
        let output = std::process::Command::new("docker")
            .args(["inspect", "--format", "{{.State.Running}}", name])
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|error| format!("cannot run docker: {error}"))?;
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).trim() == "true");
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("No such object") || stderr.contains("No such container") {
            return Ok(false);
        }
        Err(format!("docker inspect {name}: {}", stderr.trim()))
    }
}

/// A lock that a running program holds.
#[derive(Debug, Clone)]
pub struct Lock {
    /// The lock file; a missing file is not checked (no such program ran).
    pub path: PathBuf,
    /// Who holds it when it is held, for the message.
    pub holder: &'static str,
}

/// The locks taken by [`check`], held until dropped so that no program
/// starts on the installation while it moves.
#[derive(Debug, Default)]
pub struct HeldLocks(#[allow(dead_code)] Vec<fs::File>);

/// What [`check`] looks at.
#[derive(Debug, Clone, Default)]
pub struct Checks {
    /// Locks no program may hold.
    pub locks: Vec<Lock>,
    /// Directories holding `workloads/` and `readers/` of the old local
    /// controller (old place and, after a crash, the new one).
    pub workload_dirs: Vec<PathBuf>,
    /// Entries to move: none may be a symbolic link.
    pub sources: Vec<PathBuf>,
}

/// Runs every check and takes the locks. `Err` lists every reason to
/// refuse, one per line; nothing was changed.
pub fn check(
    checks: &Checks,
    probe: &dyn ContainerProbe,
    assume_containers_stopped: bool,
) -> Result<HeldLocks, Refusal> {
    let mut reasons = Vec::new();
    let mut only_held_locks = true;
    let mut held = Vec::new();
    for lock in &checks.locks {
        match take_lock(lock) {
            Ok(Some(file)) => held.push(file),
            Ok(None) => {}
            Err(LockError::Held(reason)) => reasons.push(reason),
            Err(LockError::Other(reason)) => {
                only_held_locks = false;
                reasons.push(reason);
            }
        }
    }
    for source in &checks.sources {
        if fs::symlink_metadata(source).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            only_held_locks = false;
            reasons.push(format!(
                "{} is a symbolic link; the migration moves the installation's own files, never \
                 what a link points at. Move what it points at to its place by hand",
                source.display()
            ));
        }
    }
    reasons.extend(live_workloads(
        &checks.workload_dirs,
        probe,
        assume_containers_stopped,
    ));
    if reasons.is_empty() {
        Ok(HeldLocks(held))
    } else {
        Err(Refusal {
            reason: reasons.join("\n"),
            only_held_locks,
        })
    }
}

/// Why `check` refused.
#[derive(Debug)]
pub struct Refusal {
    /// Every reason, one per line.
    pub reason: String,
    /// `true` when every reason is a lock that another program holds. A
    /// lock that cannot be opened or taken is not such a reason.
    pub only_held_locks: bool,
}

/// Why a lock was not taken.
enum LockError {
    /// Another program holds it.
    Held(String),
    /// It cannot be opened or locked.
    Other(String),
}

/// Takes `lock` without waiting; `None` when the file does not exist.
fn take_lock(lock: &Lock) -> Result<Option<fs::File>, LockError> {
    let path = &lock.path;
    let file = match fs::OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(LockError::Other(format!(
                "cannot open {}: {error}",
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
        return Err(LockError::Held(format!(
            "{} holds {}; stop it first",
            lock.holder,
            path.display()
        )));
    }
    Err(LockError::Other(format!(
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

/// The reasons to refuse found in the workload records below `dirs`. An
/// unreadable record is skipped with a warning: it names nothing to check.
fn live_workloads(
    dirs: &[PathBuf],
    probe: &dyn ContainerProbe,
    assume_containers_stopped: bool,
) -> Vec<String> {
    let mut live = Vec::new();
    let mut unknown = Vec::new();
    for record_path in workload_records(dirs) {
        let record = fs::read(&record_path)
            .map_err(|error| error.to_string())
            .and_then(|bytes| {
                serde_json::from_slice::<OldWorkload>(&bytes).map_err(|error| error.to_string())
            });
        let record = match record {
            Ok(record) => record,
            Err(error) => {
                tracing::warn!(path = %record_path.display(), %error, "migration: skipping an unreadable workload record");
                continue;
            }
        };
        if let Some(container) = record.container {
            if assume_containers_stopped {
                continue;
            }
            match probe.running(&container.name) {
                Ok(true) => live.push(format!(
                    "container {} ({})",
                    container.name,
                    record_path.display()
                )),
                Ok(false) => {}
                Err(error) => unknown.push(format!("{} ({error})", container.name)),
            }
        } else if let Some(pid) = record.pid {
            if process_alive(pid, record.start_ticks) {
                live.push(format!("process {pid} ({})", record_path.display()));
            }
        }
    }
    let mut reasons = Vec::new();
    if !live.is_empty() {
        reasons.push(format!(
            "tasks still run under the old local controller:\n  - {}\n\
             Stop all tasks first (stop the old server and every task). If a runner outlived \
             its server: kill the processes whose environment holds {WORKLOAD_MARK_ENV}, and \
             remove the containers labelled {WORKLOAD_LABEL} \
             (`docker rm -f $(docker ps -q --filter label={WORKLOAD_LABEL})`); after the update \
             the controller reports them lost and resumes them.",
            live.join("\n  - ")
        ));
    }
    if !unknown.is_empty() {
        reasons.push(format!(
            "Docker cannot tell whether the old runner containers still run:\n  - {}\n\
             Check with `docker ps --filter label={WORKLOAD_LABEL}` and remove any that run; \
             then run again with --assume-containers-stopped.",
            unknown.join("\n  - ")
        ));
    }
    reasons
}

/// Every `workload.json` below `{dir}/workloads/*/` and `{dir}/readers/*/`.
fn workload_records(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut records = Vec::new();
    for dir in dirs {
        for kind in ["workloads", "readers"] {
            let Ok(entries) = fs::read_dir(dir.join(kind)) else {
                continue;
            };
            records.extend(
                entries
                    .flatten()
                    .map(|entry| entry.path().join("workload.json"))
                    .filter(|path| Path::is_file(path)),
            );
        }
    }
    records
}
