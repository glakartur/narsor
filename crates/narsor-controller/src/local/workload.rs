//! What the local adapter keeps on disk about a task, whatever runs it: the
//! workload directories with their `workload.json` records in the
//! controller's work directory, and the per-task records in its state
//! directory ([`super::paths`]).
//!
//! # Layout
//!
//! ```text
//! {work}/storage/{storage}/           task storage (R9): state, workspace, HOME
//! {work}/workloads/{workload}/        one live "workload" at most per task
//!     workload.json                   incarnation, generation, pid or container
//!     credential                      the runner credential, 0600, outside storage
//!     runner.log                      the runner's stdout and stderr (process)
//!     secrets/                        the task's secrets (container), 0700
//! {work}/readers/{reader}/            the task's read-only runner (D63), same files
//! {state}/tasks/{storage}.json        the per-task record (KTD14)
//! ```
//!
//! A workload directory exists exactly while the workload does, so the
//! deterministic name *is* the identity: a restarted controller finds the
//! process or container again through `workload.json` instead of starting a
//! second one.
//!
//! No launch spec is written here (R5): the runner gets the spec's digest
//! (`--launch-digest`) and fetches the spec from the control plane itself.
//!
//! # Two kinds of record
//!
//! A `workload.json` records a process (pid and start time) or a container
//! (its name, its image once known, when it was started, a problem that
//! ended it). A record written before containers existed has no container
//! and reads as a process.
//!
//! # The per-task record
//!
//! `{state}/tasks/{storage}.json` keeps the kind of the task's last working
//! runner and the image it last ran on (KTD14). It is a controller file
//! beside the workloads, not a file on task storage: the agent in the
//! container writes task storage and must not choose the image of its next
//! start or hide it from garbage collection. It outlives the workload
//! directory (pause, release, loss) and goes with the storage on archive.
//! It gives the image a resumed task runs on (R11), the warning when a task
//! that ran in a container starts as a host process (R3) and the images
//! garbage collection keeps.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::paths::{ControllerStatePaths, ControllerWorkPaths};
use narsor_controller_runtime::backend::{
    BackendError, BackendResult, RunnerCredential, TaskResources, WorkloadLaunch, WorkloadPhase,
};

/// The environment variable every runner gets, set to its workload's uid:
/// what proves a process group still ours after the runner is gone.
pub(super) const WORKLOAD_MARK_ENV: &str = "NARSOR_LOCAL_WORKLOAD_UID";

/// Which of a task's two kinds of workload a call is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Role {
    /// The runner that works the task.
    Work,
    /// The read-only runner (D63).
    Read,
}

impl Role {
    fn directory(self, work: &ControllerWorkPaths) -> PathBuf {
        match self {
            Self::Work => work.workloads_dir(),
            Self::Read => work.readers_dir(),
        }
    }

    pub(super) fn name(self, task: &TaskResources) -> &str {
        match self {
            Self::Work => &task.workload,
            Self::Read => &task.reader,
        }
    }
}

/// What `workload.json` records about the one workload of a task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct WorkloadRecord {
    pub(super) uid: String,
    pub(super) resource_generation: u64,
    pub(super) incarnation: String,
    pub(super) launch_generation: u64,
    /// A process or a container.
    #[serde(flatten)]
    pub(super) kind: WorkloadKind,
    /// Set once the runner was seen to terminate.
    #[serde(default)]
    pub(super) exited: Option<ExitRecord>,
}

/// The two kinds of workload. A record without a `container` is a process:
/// what every record written before the Docker work mode is.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub(super) enum WorkloadKind {
    /// A runner in a container (the Docker work mode).
    Container { container: ContainerRecord },
    /// A runner process on the host.
    Process {
        pid: u32,
        /// The process's start time in clock ticks since boot, so a reused
        /// pid is never mistaken for the runner.
        start_ticks: Option<u64>,
    },
}

/// What a container record holds beside the common fields.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct ContainerRecord {
    /// The container's name, also its
    /// [`WORKLOAD_LABEL`](super::container::WORKLOAD_LABEL): the workload
    /// uid.
    pub(super) name: String,
    /// The image it runs, once known; `None` while the image builds.
    #[serde(default)]
    pub(super) image: Option<String>,
    /// When the container was created, in ms since the epoch; `None` before.
    #[serde(default)]
    pub(super) started_at_ms: Option<u64>,
    /// The control plane the runner reaches.
    pub(super) control_plane: String,
    /// Why the workload ended without its runner, when it did.
    #[serde(default)]
    pub(super) problem: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct ExitRecord {
    pub(super) code: Option<i32>,
}

/// The per-task record (KTD14): what a task's last working runner was.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct TaskRecord {
    pub(super) kind: LastRunner,
    /// The image the task last ran on, kept across a process run so a
    /// return to the Docker mode finds it again.
    #[serde(default)]
    pub(super) image: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum LastRunner {
    Process,
    Container,
}

/// What a look at a workload found: its phase (`None`: it is gone), and the
/// record to write back when the look learned something.
pub(super) struct Look {
    pub(super) phase: Option<WorkloadPhase>,
    pub(super) updated: Option<WorkloadRecord>,
}

impl Look {
    pub(super) fn phase(phase: WorkloadPhase) -> Self {
        Self {
            phase: Some(phase),
            updated: None,
        }
    }
}

/// The files of the adapter: task storages, workload directories with
/// their records (work), per-task records (state).
#[derive(Debug, Clone)]
pub(super) struct WorkloadStore {
    state: ControllerStatePaths,
    work: ControllerWorkPaths,
}

impl WorkloadStore {
    /// The files under the controller's `state` and `work` directories.
    pub(super) fn new(state: ControllerStatePaths, work: ControllerWorkPaths) -> Self {
        Self { state, work }
    }

    /// The controller's work directory: storages, workloads, readers.
    pub(super) fn work_dir(&self) -> &Path {
        self.work.dir()
    }

    /// Where the task's storage lives.
    pub(super) fn storage_dir(&self, task: &TaskResources) -> PathBuf {
        self.work.storage_dir().join(&task.storage)
    }

    pub(super) fn workload_dir(&self, task: &TaskResources, role: Role) -> PathBuf {
        role.directory(&self.work).join(role.name(task))
    }

    fn record_path(&self, task: &TaskResources, role: Role) -> PathBuf {
        self.workload_dir(task, role).join("workload.json")
    }

    fn task_record_path(&self, task: &TaskResources) -> PathBuf {
        self.state
            .tasks_dir()
            .join(format!("{}.json", task.storage))
    }

    pub(super) fn read_record(
        &self,
        task: &TaskResources,
        role: Role,
    ) -> BackendResult<Option<WorkloadRecord>> {
        match fs::read(self.record_path(task, role)) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
                BackendError::transient(format!("unreadable workload record: {error}"))
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io_error("read the workload record", error)),
        }
    }

    pub(super) fn write_record(
        &self,
        task: &TaskResources,
        role: Role,
        record: &WorkloadRecord,
    ) -> BackendResult<()> {
        let path = self.record_path(task, role);
        let bytes = serde_json::to_vec_pretty(record)
            .map_err(|error| BackendError::transient(error.to_string()))?;
        write_atomically(&path, &bytes)
            .map_err(|error| io_error("write the workload record", error))
    }

    /// The task's per-task record; one that cannot be read counts as none.
    pub(super) fn read_task_record(&self, task: &TaskResources) -> Option<TaskRecord> {
        let path = self.task_record_path(task);
        let bytes = fs::read(&path).ok()?;
        serde_json::from_slice(&bytes)
            .inspect_err(|error| {
                tracing::warn!(path = %path.display(), %error, "local backend: ignoring an unreadable task record");
            })
            .ok()
    }

    pub(super) fn write_task_record(
        &self,
        task: &TaskResources,
        record: &TaskRecord,
    ) -> io::Result<()> {
        let path = self.task_record_path(task);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_atomically(
            &path,
            &serde_json::to_vec_pretty(record).map_err(io::Error::other)?,
        )
    }

    /// Deletes the task's per-task record; one that does not exist is gone
    /// already.
    pub(super) fn remove_task_record(&self, task: &TaskResources) -> io::Result<()> {
        match fs::remove_file(self.task_record_path(task)) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }

    /// Every readable per-task record.
    pub(super) fn task_records(&self) -> impl Iterator<Item = TaskRecord> {
        json_files(&self.state.tasks_dir())
            .filter_map(|bytes| serde_json::from_slice::<TaskRecord>(&bytes).ok())
    }

    /// Every readable record of a working runner (no reader's).
    pub(super) fn work_records(&self) -> impl Iterator<Item = WorkloadRecord> {
        fs::read_dir(Role::Work.directory(&self.work))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| fs::read(entry.path().join("workload.json")).ok())
            .filter_map(|bytes| serde_json::from_slice::<WorkloadRecord>(&bytes).ok())
    }

    /// Creates the workload directory with the credential; a half-made one
    /// is removed again.
    pub(super) fn prepare_workload_dir(
        &self,
        task: &TaskResources,
        role: Role,
        launch: &WorkloadLaunch,
    ) -> BackendResult<PathBuf> {
        // Written first and removed on failure: a half-made workload
        // directory without a record must not linger as "something exists".
        let dir = self.workload_dir(task, role);
        let staging = dir.with_extension("staging");
        if let Some(parent) = dir.parent() {
            fs::create_dir_all(parent).map_err(|error| io_error("create the workload", error))?;
        }
        let _ = fs::remove_dir_all(&staging);
        // A directory without a record is what a crash between committing
        // the directory and recording the process leaves: no workload.
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&staging).map_err(|error| io_error("create the workload", error))?;
        if let Err(error) = write_credential(&staging, &launch.credential) {
            let _ = fs::remove_dir_all(&staging);
            return Err(io_error("prepare the workload", error));
        }
        fs::rename(&staging, &dir).map_err(|error| io_error("commit the workload", error))?;
        Ok(dir)
    }
}

/// Writes `credential` as the `credential` file of the workload directory
/// `dir`, readable by its owner only, replacing one already there.
pub(super) fn write_credential(dir: &Path, credential: &RunnerCredential) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let temp = dir.join("credential.tmp");
    let _ = fs::remove_file(&temp);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    io::Write::write_all(&mut file, credential.expose().as_bytes())?;
    fs::rename(&temp, dir.join("credential"))
}

/// The arguments every runner of a task gets: the credential file of its
/// workload directory `dir`, its storage, the digest of the launch spec it
/// fetches (or finds on storage) and whom it reports to.
pub(super) fn task_args(
    task_id: &str,
    storage: &Path,
    dir: &Path,
    launch_digest: &str,
    control_plane: &str,
    incarnation: &str,
) -> Vec<OsString> {
    vec![
        "--task-id".into(),
        task_id.into(),
        "--task-storage".into(),
        storage.into(),
        "--launch-digest".into(),
        launch_digest.into(),
        "--control-plane".into(),
        control_plane.into(),
        "--credential-file".into(),
        dir.join("credential").into(),
        "--incarnation".into(),
        incarnation.into(),
    ]
}

pub(super) fn io_error(what: &str, error: io::Error) -> BackendError {
    BackendError::transient(format!("cannot {what}: {error}"))
}

fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    fs::write(&temp, bytes)?;
    fs::rename(&temp, path)
}

/// The contents of the `*.json` files directly in `dir`.
fn json_files(dir: &Path) -> impl Iterator<Item = Vec<u8>> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension() == Some(OsStr::new("json")))
        .filter_map(|entry| fs::read(entry.path()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_written_before_containers_reads_as_a_process() {
        let old = serde_json::json!({
            "uid": "local-1",
            "resource_generation": 2,
            "incarnation": "task-1-g1",
            "launch_generation": 1,
            "pid": 42,
            "start_ticks": 7,
            "exited": null,
        });
        let record: WorkloadRecord = serde_json::from_value(old).unwrap();
        assert_eq!(
            record.kind,
            WorkloadKind::Process {
                pid: 42,
                start_ticks: Some(7)
            }
        );

        let container = WorkloadRecord {
            kind: WorkloadKind::Container {
                container: ContainerRecord {
                    name: "narsor-local-1".into(),
                    image: None,
                    started_at_ms: None,
                    control_plane: "http://127.0.0.1:9".into(),
                    problem: None,
                },
            },
            ..record
        };
        let reread: WorkloadRecord =
            serde_json::from_value(serde_json::to_value(&container).unwrap()).unwrap();
        assert_eq!(reread, container);
    }
}
