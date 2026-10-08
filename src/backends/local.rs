//! The local adapter: `narsor runner` as a child process of the host, task
//! storage as a local directory (U4).
//!
//! This is how the local machine becomes a backend like any other: the same
//! control-plane lifecycle, the same controller, the same contract suite.
//! The existing daemon's `/executions` path is untouched — it keeps running
//! the Engine in-process with its own recovery; this adapter is the
//! lifecycle-managed route to the very same Engine, through the portable
//! runner (U3).
//!
//! # Layout under the adapter's root
//!
//! ```text
//! {root}/storage/{storage}/           task storage (R9): state, workspace, HOME
//! {root}/workloads/{workload}/        one live "workload" at most per task
//!     workload.json                   incarnation, generation, pid, uid
//!     credential                      the runner credential, 0600, outside storage
//!     launch-spec.json                the immutable launch spec, outside storage
//!     runner.log                      the runner's stdout and stderr
//! ```
//!
//! A workload directory exists exactly while the workload does, so the
//! deterministic name *is* the identity: a restarted controller finds the
//! process again through `workload.json` instead of starting a second one.
//! Runners run in their own process group and survive the controller, like
//! a Pod survives its controller (R14); removing compute kills the group.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{
    ArchiveProgress, BackendError, BackendResult, ExecutionBackend, Provisioned, Removal,
    StorageState, TaskObservation, TaskResources, WorkloadLaunch, WorkloadPhase, WorkloadState,
};
use crate::domain::TaskId;
use crate::environment::{BackendCapabilities, BackendKind};

/// How long removing compute waits for a killed runner to disappear before
/// it answers "in progress" and lets the controller come back.
const KILL_WAIT: Duration = Duration::from_secs(2);

/// Configuration of a [`LocalBackend`].
#[derive(Debug, Clone)]
pub struct LocalBackendConfig {
    /// Directory holding task storages and workload records.
    pub root: PathBuf,
    /// The `narsor` executable that provides `narsor runner`.
    pub program: PathBuf,
    /// Extra arguments appended to every `narsor runner` invocation (timing
    /// knobs, for instance).
    pub runner_args: Vec<OsString>,
    /// Environment variables set for every runner, on top of the
    /// controller's own environment.
    pub env: Vec<(OsString, OsString)>,
}

impl LocalBackendConfig {
    /// Runners started from the currently running executable.
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        Ok(Self::with_program(root, std::env::current_exe()?))
    }

    /// Runners started from `program`.
    pub fn with_program(root: impl Into<PathBuf>, program: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            program: program.into(),
            runner_args: Vec::new(),
            env: Vec::new(),
        }
    }
}

/// The local adapter. Cloning shares the process table.
#[derive(Debug, Clone)]
pub struct LocalBackend {
    config: Arc<LocalBackendConfig>,
    /// Runners this instance started: only a parent can learn an exit code
    /// and reap its child. A runner started by a previous controller is
    /// found through its `workload.json` instead.
    children: Arc<Mutex<HashMap<String, Child>>>,
}

/// What `workload.json` records about the one workload of a task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WorkloadRecord {
    uid: String,
    resource_generation: u64,
    incarnation: String,
    launch_generation: u64,
    pid: u32,
    /// The process's start time in clock ticks since boot, so a reused pid
    /// is never mistaken for the runner.
    start_ticks: Option<u64>,
    /// Set once the runner was seen to terminate.
    #[serde(default)]
    exited: Option<ExitRecord>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
struct ExitRecord {
    code: Option<i32>,
}

impl LocalBackend {
    /// An adapter over `config.root`.
    pub fn new(config: LocalBackendConfig) -> Self {
        Self {
            config: Arc::new(config),
            children: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The pid of the task's runner while it runs — what a test kills to
    /// model the host losing the workload.
    pub fn runner_pid(&self, task: &TaskResources) -> Option<u32> {
        let record = self.read_record(task).ok()??;
        match self.phase(task, &record) {
            Ok((phase, _)) if phase.is_alive() => Some(record.pid),
            _ => None,
        }
    }

    /// Where the task's storage lives.
    pub fn storage_dir(&self, task: &TaskResources) -> PathBuf {
        self.config.root.join("storage").join(&task.storage)
    }

    fn workload_dir(&self, task: &TaskResources) -> PathBuf {
        self.config.root.join("workloads").join(&task.workload)
    }

    fn record_path(&self, task: &TaskResources) -> PathBuf {
        self.workload_dir(task).join("workload.json")
    }

    fn children(&self) -> MutexGuard<'_, HashMap<String, Child>> {
        self.children.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn read_record(&self, task: &TaskResources) -> BackendResult<Option<WorkloadRecord>> {
        match fs::read(self.record_path(task)) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
                BackendError::transient(format!("unreadable workload record: {error}"))
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io_error("read the workload record", error)),
        }
    }

    fn write_record(&self, task: &TaskResources, record: &WorkloadRecord) -> BackendResult<()> {
        let path = self.record_path(task);
        let temp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(record)
            .map_err(|error| BackendError::transient(error.to_string()))?;
        fs::write(&temp, bytes).map_err(|error| io_error("write the workload record", error))?;
        fs::rename(&temp, &path).map_err(|error| io_error("commit the workload record", error))
    }

    /// The workload's phase, and whether the record learned something new
    /// (an exit) that must be written back.
    fn phase(
        &self,
        task: &TaskResources,
        record: &WorkloadRecord,
    ) -> BackendResult<(WorkloadPhase, Option<ExitRecord>)> {
        // The parent asks its child first: only it can reap the process and
        // learn the exit code, whatever another instance recorded.
        let mut children = self.children();
        if let Some(child) = children.get_mut(&task.workload) {
            if child.id() == record.pid {
                return match child.try_wait() {
                    Ok(Some(status)) => {
                        children.remove(&task.workload);
                        let exit = ExitRecord {
                            code: status.code(),
                        };
                        Ok((WorkloadPhase::Exited { code: exit.code }, Some(exit)))
                    }
                    Ok(None) => Ok((WorkloadPhase::Running, None)),
                    Err(error) => Err(io_error("wait for the runner", error)),
                };
            }
        }
        drop(children);
        if let Some(exit) = record.exited {
            return Ok((WorkloadPhase::Exited { code: exit.code }, None));
        }
        // Started by an earlier controller: alive is all that can be told;
        // its exit code went to whoever reaped it.
        if process_alive(record.pid, record.start_ticks) {
            Ok((WorkloadPhase::Running, None))
        } else {
            let exit = ExitRecord { code: None };
            Ok((WorkloadPhase::Exited { code: None }, Some(exit)))
        }
    }

    fn observe_workload(&self, task: &TaskResources) -> BackendResult<Option<WorkloadState>> {
        let Some(mut record) = self.read_record(task)? else {
            return Ok(None);
        };
        let (phase, learned) = self.phase(task, &record)?;
        if let Some(exit) = learned {
            record.exited = Some(exit);
            record.resource_generation += 1;
            if let Err(error) = self.write_record(task, &record) {
                // A concurrent removal took the workload while its exit was
                // being written back: it is gone, not unobservable.
                if !self.workload_dir(task).exists() {
                    return Ok(None);
                }
                return Err(error);
            }
        }
        Ok(Some(WorkloadState {
            uid: record.uid,
            resource_generation: record.resource_generation,
            incarnation: record.incarnation,
            launch_generation: record.launch_generation,
            phase,
        }))
    }

    fn storage_state(&self, task: &TaskResources) -> StorageState {
        if self.storage_dir(task).is_dir() {
            StorageState::Present
        } else {
            StorageState::Absent
        }
    }

    fn create(
        &self,
        task: &TaskResources,
        launch: &WorkloadLaunch,
        create_storage: bool,
    ) -> BackendResult<Provisioned> {
        let storage = self.storage_dir(task);
        let created_storage = !storage.is_dir();
        if created_storage {
            if !create_storage {
                return Err(BackendError::storage_missing(format!(
                    "task storage '{}' does not exist",
                    storage.display()
                )));
            }
            fs::create_dir_all(&storage)
                .map_err(|error| io_error("create the task storage", error))?;
        }
        if let Some(existing) = self.observe_workload(task)? {
            return Ok(Provisioned {
                workload: existing,
                created_workload: false,
                created_storage,
            });
        }
        let workload = self.spawn(task, launch, &storage)?;
        Ok(Provisioned {
            workload,
            created_workload: true,
            created_storage,
        })
    }

    fn spawn(
        &self,
        task: &TaskResources,
        launch: &WorkloadLaunch,
        storage: &Path,
    ) -> BackendResult<WorkloadState> {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::process::CommandExt;

        // Written first and removed on failure: a half-made workload
        // directory without a record must not linger as "something exists".
        let dir = self.workload_dir(task);
        let staging = dir.with_extension("staging");
        let _ = fs::remove_dir_all(&staging);
        // A directory without a record is what a crash between committing
        // the directory and recording the process leaves: no workload.
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&staging).map_err(|error| io_error("create the workload", error))?;
        let made = (|| -> io::Result<()> {
            let mut credential = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(staging.join("credential"))?;
            io::Write::write_all(&mut credential, launch.credential.expose().as_bytes())?;
            fs::write(
                staging.join("launch-spec.json"),
                serde_json::to_vec(&launch.launch_spec).map_err(io::Error::other)?,
            )?;
            Ok(())
        })();
        if let Err(error) = made {
            let _ = fs::remove_dir_all(&staging);
            return Err(io_error("prepare the workload", error));
        }
        fs::rename(&staging, &dir).map_err(|error| io_error("commit the workload", error))?;

        let log = fs::File::create(dir.join("runner.log"))
            .map_err(|error| io_error("create the runner log", error))?;
        let log_err = log
            .try_clone()
            .map_err(|error| io_error("create the runner log", error))?;
        let mut command = Command::new(&self.config.program);
        command
            .arg("runner")
            .args(["--task-id", launch.task_id.as_str()])
            .arg("--task-storage")
            .arg(storage)
            .arg("--launch-spec")
            .arg(dir.join("launch-spec.json"))
            .args(["--control-plane", &launch.control_plane_url])
            .arg("--credential-file")
            .arg(dir.join("credential"))
            .args(["--incarnation", &launch.incarnation])
            .args(&self.config.runner_args)
            .envs(self.config.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(log_err)
            // Its own group: removing the workload takes everything the
            // runner started with it, and the controller's own signals
            // (a Ctrl-C) do not reach it.
            .process_group(0);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = fs::remove_dir_all(&dir);
                return Err(BackendError::rejected(format!(
                    "cannot start '{}': {error}",
                    self.config.program.display()
                )));
            }
        };
        let record = WorkloadRecord {
            uid: format!("local-{}", uuid::Uuid::new_v4().simple()),
            resource_generation: 1,
            incarnation: launch.incarnation.clone(),
            launch_generation: launch.generation,
            pid: child.id(),
            start_ticks: process_start_ticks(child.id()),
            exited: None,
        };
        self.children().insert(task.workload.clone(), child);
        self.write_record(task, &record)?;
        Ok(WorkloadState {
            uid: record.uid,
            resource_generation: record.resource_generation,
            incarnation: record.incarnation,
            launch_generation: record.launch_generation,
            phase: WorkloadPhase::Running,
        })
    }

    fn remove_workload(&self, task: &TaskResources) -> BackendResult<Removal> {
        let dir = self.workload_dir(task);
        let Some(record) = self.read_record(task)? else {
            // A staging leftover or an empty directory is not a workload.
            let _ = fs::remove_dir_all(dir.with_extension("staging"));
            return Ok(if dir.exists() {
                fs::remove_dir_all(&dir).map_err(|error| io_error("remove the workload", error))?;
                Removal::Removed
            } else {
                Removal::AlreadyAbsent
            });
        };
        let (phase, _) = self.phase(task, &record)?;
        if phase.is_alive() {
            // SAFETY: signalling the process group this adapter created;
            // `phase` checked the pid still names that process.
            unsafe {
                libc::kill(-(record.pid as libc::pid_t), libc::SIGKILL);
            }
            if !self.wait_gone(task, &record) {
                return Ok(Removal::Initiated);
            }
        }
        self.children().remove(&task.workload);
        fs::remove_dir_all(&dir).map_err(|error| io_error("remove the workload", error))?;
        Ok(Removal::Removed)
    }

    /// Waits briefly for a killed runner to go, reaping it when it is ours.
    fn wait_gone(&self, task: &TaskResources, record: &WorkloadRecord) -> bool {
        let deadline = Instant::now() + KILL_WAIT;
        loop {
            let gone = match self.children().get_mut(&task.workload) {
                Some(child) => child.try_wait().map(|s| s.is_some()).unwrap_or(true),
                None => !process_alive(record.pid, record.start_ticks),
            };
            if gone || Instant::now() >= deadline {
                return gone;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl ExecutionBackend for LocalBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Local
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            pause_resume: true,
            persistent_storage: true,
            archive: true,
        }
    }

    fn resources(&self, task_id: &TaskId, resource_key: &str) -> BackendResult<TaskResources> {
        // A task id is one safe path component; nothing else is accepted.
        let key = TaskId::parse(resource_key)
            .map_err(|error| BackendError::rejected(error.to_string()))?;
        Ok(TaskResources {
            task_id: task_id.clone(),
            resource_key: resource_key.to_string(),
            workload: key.to_string(),
            storage: key.to_string(),
        })
    }

    async fn observe(&self, task: &TaskResources) -> BackendResult<TaskObservation> {
        Ok(TaskObservation {
            workload: self.observe_workload(task)?,
            storage: self.storage_state(task),
        })
    }

    async fn provision(
        &self,
        task: &TaskResources,
        launch: &WorkloadLaunch,
    ) -> BackendResult<Provisioned> {
        self.create(task, launch, true)
    }

    async fn resume(
        &self,
        task: &TaskResources,
        launch: &WorkloadLaunch,
    ) -> BackendResult<Provisioned> {
        self.create(task, launch, false)
    }

    async fn release_compute(&self, task: &TaskResources) -> BackendResult<Removal> {
        self.remove_workload(task)
    }

    async fn archive(&self, task: &TaskResources) -> BackendResult<ArchiveProgress> {
        let workload = self.remove_workload(task)?;
        if !workload.is_complete() {
            // Never delete storage under a runner that may still write it.
            return Ok(ArchiveProgress {
                workload,
                storage: Removal::InProgress,
            });
        }
        let dir = self.storage_dir(task);
        let storage = if dir.exists() {
            fs::remove_dir_all(&dir).map_err(|error| io_error("delete the task storage", error))?;
            Removal::Removed
        } else {
            Removal::AlreadyAbsent
        };
        Ok(ArchiveProgress { workload, storage })
    }
}

fn io_error(what: &str, error: io::Error) -> BackendError {
    BackendError::transient(format!("cannot {what}: {error}"))
}

/// Field 22 of `/proc/{pid}/stat`: when the process started.
fn process_start_ticks(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may hold spaces and parentheses; fields resume after
    // the last ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let state = fields.next()?;
    if state == "Z" {
        return None;
    }
    // `rest` starts at field 3 (state); field 22 is 19 further on.
    fields.nth(18)?.parse().ok()
}

/// Whether `pid` still names the process that started at `start_ticks`
/// and has not terminated (a zombie counts as terminated).
fn process_alive(pid: u32, start_ticks: Option<u64>) -> bool {
    match (process_start_ticks(pid), start_ticks) {
        (Some(now), Some(then)) => now == then,
        (Some(_), None) => true,
        (None, _) => false,
    }
}
