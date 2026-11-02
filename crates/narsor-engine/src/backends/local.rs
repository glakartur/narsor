//! The local adapter: `narsor-runner` as a child process of the host, task
//! storage as a local directory (U4).
//!
//! This is how the local machine becomes a backend like any other: the same
//! control-plane runner state, the same controller, the same contract suite.
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
//! {root}/readers/{reader}/            the task's read-only runner (D63), same files
//! ```
//!
//! A workload directory exists exactly while the workload does, so the
//! deterministic name *is* the identity: a restarted controller finds the
//! process again through `workload.json` instead of starting a second one.
//! Runners run in their own process group and survive the controller, like
//! a Pod survives its controller (R14); removing compute kills the group.
//!
//! # Killing a group whose runner already died
//!
//! The group outlives its leader while anything the runner started (a
//! harness, a tool) still runs, and such an orphan could keep writing the
//! task storage under the next runner. Its id is the dead runner's pid,
//! though, which the system may hand out again once the group is gone, and
//! a stranger could lead a new group of that id. So the group is signalled
//! only when one of its live members carries the workload's mark: the
//! environment variable [`WORKLOAD_MARK_ENV`] set to the workload's uid,
//! which every runner gets and its descendants inherit. A group is either
//! all ours or all a stranger's, and a pid cannot be reused while a group
//! of that id still has members, so a marked member proves the group ours.
//! A runner started before the mark existed, or descendants that cleared
//! their environment, are left alone once the runner is gone, as before.
//!
//! # The reader
//!
//! A read-only runner (`narsor-runner --read-only`, D63) is a second kind of
//! workload of the same task: its own directory under `readers/`, its own
//! process group and its own mark, so removing a task's working runner never
//! signals the reader and removing the reader never signals a working
//! runner's group. It takes the storage's lock like any runner, so it cannot
//! run beside a working one; the controller sees to that first and the lock
//! is the second guard. It never looks at the process group a dead runner
//! left behind: should one still hold the lock, the reader cannot read and
//! says so.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use narsor_shared::config::{CLAUDE_CONFIG_DIR_ENV, CODEX_HOME_ENV, ENV_PREFIX};
use serde::{Deserialize, Serialize};

use super::{
    ArchiveProgress, BackendError, BackendResult, ExecutionBackend, Provisioned, ReaderStart,
    Removal, StorageState, TaskObservation, TaskResources, WorkloadLaunch, WorkloadPhase,
    WorkloadState,
};
use crate::domain::{TaskId, TaskLaunchSpec};
use crate::environment::{BackendCapabilities, BackendKind};
use crate::workspace::provisioner::USER_GIT_ACCESS_ENVS;

/// How long removing compute waits for a killed runner to disappear before
/// it answers "in progress" and lets the controller come back.
const KILL_WAIT: Duration = Duration::from_secs(2);

/// The environment variable every runner gets, set to its workload's uid:
/// what proves a process group still ours after the runner is gone.
const WORKLOAD_MARK_ENV: &str = "NARSOR_LOCAL_WORKLOAD_UID";

/// Which of a task's two kinds of workload a call is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// The runner that works the task.
    Work,
    /// The read-only runner (D63).
    Read,
}

impl Role {
    fn directory(self) -> &'static str {
        match self {
            Self::Work => "workloads",
            Self::Read => "readers",
        }
    }

    fn name(self, task: &TaskResources) -> &str {
        match self {
            Self::Work => &task.workload,
            Self::Read => &task.reader,
        }
    }
}

/// A check of every launch spec before anything of its task is created:
/// `Err` refuses the task with that problem. What a controller serving a
/// control plane it does not trust sets (R17).
#[derive(Clone)]
pub struct LaunchCheck(Arc<LaunchCheckFn>);

/// The function of a [`LaunchCheck`].
type LaunchCheckFn = dyn Fn(&TaskLaunchSpec) -> Result<(), String> + Send + Sync;

impl LaunchCheck {
    /// A check running `check`.
    pub fn new(
        check: impl Fn(&TaskLaunchSpec) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(check))
    }
}

impl std::fmt::Debug for LaunchCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LaunchCheck")
    }
}

/// Configuration of a [`LocalBackend`].
#[derive(Debug, Clone)]
pub struct LocalBackendConfig {
    /// Directory holding task storages and workload records.
    pub root: PathBuf,
    /// The standalone `narsor-runner` executable.
    pub program: PathBuf,
    /// Extra arguments appended to every `narsor-runner` invocation (timing
    /// knobs, for instance).
    pub runner_args: Vec<OsString>,
    /// Environment variables set for every runner, on top of the
    /// controller's own environment less its `NARSOR_*` settings.
    pub env: Vec<(OsString, OsString)>,
    /// Run on every launch spec before its task's storage or workload is
    /// created; none by default.
    pub launch_check: Option<LaunchCheck>,
    /// Whether runners inherit none of the user's credential variables
    /// ([`is_user_credential`]): harness logins, Git and forge credentials.
    /// Set for a control plane beyond this machine with a private HOME (R17).
    pub withhold_user_credentials: bool,
}

impl LocalBackendConfig {
    /// Runners started from the `narsor-runner` sibling of the current
    /// executable. The server and runner binaries are installed together.
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        let current_exe = std::env::current_exe()?;
        let runner_name = if cfg!(windows) {
            "narsor-runner.exe"
        } else {
            "narsor-runner"
        };
        let parent = current_exe.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "current executable has no parent")
        })?;
        Ok(Self::with_program(root, parent.join(runner_name)))
    }

    /// Runners started from the explicit executable `program`.
    pub fn with_program(root: impl Into<PathBuf>, program: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            program: program.into(),
            runner_args: Vec::new(),
            env: Vec::new(),
            launch_check: None,
            withhold_user_credentials: false,
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
        self.live_pid(task, Role::Work)
    }

    /// The pid of the task's read-only runner while it runs.
    pub fn reader_pid(&self, task: &TaskResources) -> Option<u32> {
        self.live_pid(task, Role::Read)
    }

    fn live_pid(&self, task: &TaskResources, role: Role) -> Option<u32> {
        let record = self.read_record(task, role).ok()??;
        match self.phase(task, role, &record) {
            Ok((phase, _)) if phase.is_alive() => Some(record.pid),
            _ => None,
        }
    }

    /// Where the task's storage lives.
    pub fn storage_dir(&self, task: &TaskResources) -> PathBuf {
        self.config.root.join("storage").join(&task.storage)
    }

    fn workload_dir(&self, task: &TaskResources, role: Role) -> PathBuf {
        self.config
            .root
            .join(role.directory())
            .join(role.name(task))
    }

    fn record_path(&self, task: &TaskResources, role: Role) -> PathBuf {
        self.workload_dir(task, role).join("workload.json")
    }

    fn children(&self) -> MutexGuard<'_, HashMap<String, Child>> {
        self.children.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn read_record(
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

    fn write_record(
        &self,
        task: &TaskResources,
        role: Role,
        record: &WorkloadRecord,
    ) -> BackendResult<()> {
        let path = self.record_path(task, role);
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
        role: Role,
        record: &WorkloadRecord,
    ) -> BackendResult<(WorkloadPhase, Option<ExitRecord>)> {
        // The parent asks its child first: only it can reap the process and
        // learn the exit code, whatever another instance recorded.
        let name = role.name(task);
        let mut children = self.children();
        if let Some(child) = children.get_mut(name) {
            if child.id() == record.pid {
                return match child.try_wait() {
                    Ok(Some(status)) => {
                        children.remove(name);
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

    fn observe_workload(
        &self,
        task: &TaskResources,
        role: Role,
    ) -> BackendResult<Option<WorkloadState>> {
        let Some(mut record) = self.read_record(task, role)? else {
            return Ok(None);
        };
        let (phase, learned) = self.phase(task, role, &record)?;
        if let Some(exit) = learned {
            record.exited = Some(exit);
            record.resource_generation += 1;
            if let Err(error) = self.write_record(task, role, &record) {
                // A concurrent removal took the workload while its exit was
                // being written back: it is gone, not unobservable.
                if !self.workload_dir(task, role).exists() {
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
        self.checked_spec(launch)?;
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
        if let Some(existing) = self.observe_workload(task, Role::Work)? {
            return Ok(Provisioned {
                workload: existing,
                created_workload: false,
                created_storage,
            });
        }
        let workload = self.spawn(task, Role::Work, launch, &storage)?;
        Ok(Provisioned {
            workload,
            created_workload: true,
            created_storage,
        })
    }

    /// Starts the task's read-only runner on its existing storage.
    fn create_reader(
        &self,
        task: &TaskResources,
        launch: &WorkloadLaunch,
    ) -> BackendResult<ReaderStart> {
        self.checked_spec(launch)?;
        let storage = self.storage_dir(task);
        if !storage.is_dir() {
            // A reader never creates storage: there would be nothing to read.
            return Err(BackendError::storage_missing(format!(
                "task storage '{}' does not exist",
                storage.display()
            )));
        }
        if let Some(existing) = self.observe_workload(task, Role::Read)? {
            return Ok(ReaderStart {
                workload: existing,
                created: false,
            });
        }
        let workload = self.spawn(task, Role::Read, launch, &storage)?;
        Ok(ReaderStart {
            workload,
            created: true,
        })
    }

    /// The launch spec, refused when it is malformed or fails the
    /// configured [`LaunchCheck`].
    fn checked_spec(&self, launch: &WorkloadLaunch) -> BackendResult<TaskLaunchSpec> {
        let spec = serde_json::from_value::<TaskLaunchSpec>(launch.launch_spec.clone()).map_err(
            |error| BackendError::rejected(format!("the launch spec is malformed: {error}")),
        )?;
        if let Some(LaunchCheck(check)) = &self.config.launch_check {
            check(&spec).map_err(BackendError::rejected)?;
        }
        Ok(spec)
    }

    fn spawn(
        &self,
        task: &TaskResources,
        role: Role,
        launch: &WorkloadLaunch,
        storage: &Path,
    ) -> BackendResult<WorkloadState> {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::process::CommandExt;

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
        let uid = format!("local-{}", uuid::Uuid::new_v4().simple());
        let mut command = Command::new(&self.config.program);
        command
            .args(["--task-id", launch.task_id.as_str()])
            .arg("--task-storage")
            .arg(storage)
            .arg("--launch-spec")
            .arg(dir.join("launch-spec.json"))
            .args(["--control-plane", &launch.control_plane_url])
            .arg("--credential-file")
            .arg(dir.join("credential"))
            .args(["--incarnation", &launch.incarnation]);
        if role == Role::Read {
            command.arg("--read-only");
        }
        command.args(&self.config.runner_args);
        // Nothing of the controller's own settings — its enrollment code
        // least of all — reaches the runner or the tools it starts (R12);
        // what the adapter sets itself is set below. `spawn` isolates it:
        // the controller's environment stays as it is.
        // Nor, for a server that is not trusted, the user's own logins: a
        // harness directory or a Git credential inherited from the shell.
        let withhold = self.config.withhold_user_credentials;
        for (name, _) in std::env::vars_os() {
            if is_controller_setting(&name) || (withhold && is_user_credential(&name)) {
                command.env_remove(name);
            }
        }
        command
            .envs(self.config.env.iter().map(|(k, v)| (k, v)))
            .env(WORKLOAD_MARK_ENV, &uid)
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
            uid,
            resource_generation: 1,
            incarnation: launch.incarnation.clone(),
            launch_generation: launch.generation,
            pid: child.id(),
            start_ticks: process_start_ticks(child.id()),
            exited: None,
        };
        self.children().insert(role.name(task).to_string(), child);
        self.write_record(task, role, &record)?;
        Ok(WorkloadState {
            uid: record.uid,
            resource_generation: record.resource_generation,
            incarnation: record.incarnation,
            launch_generation: record.launch_generation,
            phase: WorkloadPhase::Running,
        })
    }

    fn remove_workload(&self, task: &TaskResources, role: Role) -> BackendResult<Removal> {
        let dir = self.workload_dir(task, role);
        let Some(record) = self.read_record(task, role)? else {
            // A staging leftover or an empty directory is not a workload.
            let _ = fs::remove_dir_all(dir.with_extension("staging"));
            return Ok(if dir.exists() {
                fs::remove_dir_all(&dir).map_err(|error| io_error("remove the workload", error))?;
                Removal::Removed
            } else {
                Removal::AlreadyAbsent
            });
        };
        let (phase, _) = self.phase(task, role, &record)?;
        if phase.is_alive() {
            // SAFETY: signalling the process group this adapter created;
            // `phase` checked the pid still names that process.
            unsafe {
                libc::kill(-(record.pid as libc::pid_t), libc::SIGKILL);
            }
            if !self.wait_gone(task, role, &record) {
                return Ok(Removal::Initiated);
            }
        }
        // The runner is gone; what it started may not be (see the module
        // docs on why the mark makes the group safe to signal).
        if !kill_marked_group(record.pid, &record.uid) {
            return Ok(Removal::Initiated);
        }
        self.children().remove(role.name(task));
        fs::remove_dir_all(&dir).map_err(|error| io_error("remove the workload", error))?;
        Ok(Removal::Removed)
    }

    /// Waits briefly for a killed runner to go, reaping it when it is ours.
    fn wait_gone(&self, task: &TaskResources, role: Role, record: &WorkloadRecord) -> bool {
        let deadline = Instant::now() + KILL_WAIT;
        loop {
            let gone = match self.children().get_mut(role.name(task)) {
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
            // Another directory, and another name, than the workload's.
            reader: format!("{key}-reader"),
            storage: key.to_string(),
        })
    }

    async fn observe(&self, task: &TaskResources) -> BackendResult<TaskObservation> {
        Ok(TaskObservation {
            workload: self.observe_workload(task, Role::Work)?,
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
        self.remove_workload(task, Role::Work)
    }

    async fn archive(&self, task: &TaskResources) -> BackendResult<ArchiveProgress> {
        let workload = self.remove_workload(task, Role::Work)?;
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

    async fn observe_reader(&self, task: &TaskResources) -> BackendResult<Option<WorkloadState>> {
        self.observe_workload(task, Role::Read)
    }

    async fn start_reader(
        &self,
        task: &TaskResources,
        launch: &WorkloadLaunch,
    ) -> BackendResult<ReaderStart> {
        self.create_reader(task, launch)
    }

    async fn stop_reader(&self, task: &TaskResources) -> BackendResult<Removal> {
        self.remove_workload(task, Role::Read)
    }
}

/// Whether `name` is one of the controller's own `NARSOR_*` variables,
/// which a runner never inherits.
fn is_controller_setting(name: &std::ffi::OsStr) -> bool {
    name.as_encoded_bytes().starts_with(ENV_PREFIX.as_bytes())
        && name != std::ffi::OsStr::new(crate::config::AGENT_ARGS_ENV)
}

/// Variables through which a runner or a harness would reach the user's
/// own logins, beyond those git reads ([`USER_GIT_ACCESS_ENVS`]).
const USER_CREDENTIAL_ENVS: [&str; 13] = [
    CODEX_HOME_ENV,
    CLAUDE_CONFIG_DIR_ENV,
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_AUTH_TOKEN",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "GH_CONFIG_DIR",
    "GITLAB_TOKEN",
    "GL_TOKEN",
    "GLAB_TOKEN",
    "GLAB_CONFIG_DIR",
];

/// The prefix of git's configuration variables (`GIT_CONFIG_GLOBAL`,
/// `GIT_CONFIG_KEY_<n>`, ...), any of which can hand git a credential.
const GIT_CONFIG_ENV_PREFIX: &str = "GIT_CONFIG";

/// Whether `name` is one of the user's credential variables, which a runner
/// serving a server that is not trusted never inherits.
fn is_user_credential(name: &std::ffi::OsStr) -> bool {
    let name = name.as_encoded_bytes();
    name.starts_with(GIT_CONFIG_ENV_PREFIX.as_bytes())
        || USER_GIT_ACCESS_ENVS
            .iter()
            .chain(&USER_CREDENTIAL_ENVS)
            .any(|known| name == known.as_bytes())
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

/// Kills what is left of the process group `pgid` when it is the
/// workload's (a live member carries `uid` as [`WORKLOAD_MARK_ENV`]), and
/// waits briefly for it to go; true when no marked member is left.
fn kill_marked_group(pgid: u32, uid: &str) -> bool {
    let deadline = Instant::now() + KILL_WAIT;
    loop {
        if !group_marked(pgid, uid) {
            return true;
        }
        // SAFETY: a marked live member proves the group ours, and its id
        // cannot be reused while that member lives.
        unsafe {
            libc::kill(-(pgid as libc::pid_t), libc::SIGKILL);
        }
        if Instant::now() >= deadline {
            return !group_marked(pgid, uid);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether a live (not zombie) member of process group `pgid` carries the
/// workload's mark. A process whose environment cannot be read (another
/// user's) does not.
fn group_marked(pgid: u32, uid: &str) -> bool {
    let Ok(entries) = fs::read_dir("/proc") else {
        return false;
    };
    let mark = format!("{WORKLOAD_MARK_ENV}={uid}");
    entries.flatten().any(|entry| {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            return false;
        };
        process_group(pid) == Some(pgid)
            && fs::read(format!("/proc/{pid}/environ")).is_ok_and(|environ| {
                environ
                    .split(|byte| *byte == 0)
                    .any(|entry| entry == mark.as_bytes())
            })
    })
}

/// Field 5 of `/proc/{pid}/stat`: its process group; `None` for a zombie or
/// a process that is gone.
fn process_group(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    if fields.next()? == "Z" {
        return None;
    }
    // `rest` starts at field 3 (state); field 5 is two further on.
    fields.nth(1)?.parse().ok()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    /// A process in a group of its own, carrying `mark` (if any).
    fn group_leader(mark: Option<&str>) -> Child {
        let mut command = Command::new("sleep");
        command.arg("600").process_group(0);
        if let Some(uid) = mark {
            command.env(WORKLOAD_MARK_ENV, uid);
        }
        command.spawn().expect("spawn sleep")
    }

    #[test]
    fn the_controllers_settings_are_not_inherited_by_a_runner() {
        for name in [
            "NARSOR_ENROLLMENT_CODE",
            "NARSOR_CONTROL_PLANE",
            "NARSOR_RUNNER_CONTROL_PLANE",
        ] {
            assert!(is_controller_setting(name.as_ref()), "{name}");
        }
        assert!(!is_controller_setting(
            crate::config::AGENT_ARGS_ENV.as_ref()
        ));
        for name in ["PATH", "HOME", "RUST_LOG", "narsor_lower", "XNARSOR_"] {
            assert!(!is_controller_setting(name.as_ref()), "{name}");
        }
    }

    #[test]
    fn the_users_logins_are_credential_variables() {
        for name in [
            "CODEX_HOME",
            "CLAUDE_CONFIG_DIR",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_AUTH_TOKEN",
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "GH_CONFIG_DIR",
            "GITLAB_TOKEN",
            "GL_TOKEN",
            "GLAB_TOKEN",
            "GLAB_CONFIG_DIR",
            "SSH_AUTH_SOCK",
            "GIT_ASKPASS",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_KEY_0",
            "XDG_CONFIG_HOME",
        ] {
            assert!(is_user_credential(name.as_ref()), "{name}");
        }
        for name in ["PATH", "HOME", "GIT_TERMINAL_PROMPT", "codex_home"] {
            assert!(!is_user_credential(name.as_ref()), "{name}");
        }
    }

    #[test]
    fn a_group_without_the_workloads_mark_is_never_signalled() {
        let mut stranger = group_leader(Some("local-someone-else"));
        let mut unmarked = group_leader(None);

        assert!(kill_marked_group(stranger.id(), "local-ours"));
        assert!(kill_marked_group(unmarked.id(), "local-ours"));

        assert!(stranger.try_wait().unwrap().is_none(), "still running");
        assert!(unmarked.try_wait().unwrap().is_none(), "still running");
        let _ = stranger.kill();
        let _ = unmarked.kill();
        let _ = stranger.wait();
        let _ = unmarked.wait();
    }

    /// A scratch backend whose "runner" is a script that records its
    /// arguments and sleeps; stops everything it started when dropped.
    struct Scratch {
        root: PathBuf,
        backend: LocalBackend,
    }

    impl Scratch {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("narsor-local-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            let program = root.join("fake-runner");
            fs::write(
                &program,
                "#!/bin/sh\necho \"$@\" >> \"$ARGS_FILE\"\nexec sleep 600\n",
            )
            .unwrap();
            fs::set_permissions(
                &program,
                std::os::unix::fs::PermissionsExt::from_mode(0o755),
            )
            .unwrap();
            let mut config = LocalBackendConfig::with_program(root.join("local"), &program);
            config.env = vec![("ARGS_FILE".into(), root.join("args").into())];
            Self {
                root,
                backend: LocalBackend::new(config),
            }
        }

        fn task(&self) -> TaskResources {
            let id = TaskId::parse("task-1").unwrap();
            self.backend.resources(&id, "task-1").unwrap()
        }

        fn launch(&self, incarnation: &str) -> WorkloadLaunch {
            use crate::domain::{ResolvedEnvironment, ResourceProfile, TaskWorkspace};
            let spec = TaskLaunchSpec::new(
                "wf",
                1,
                serde_json::json!({}),
                TaskWorkspace::None,
                ResolvedEnvironment::harness_only("claude-code"),
                ResourceProfile::new(1000, 2048),
            );
            WorkloadLaunch {
                task_id: TaskId::parse("task-1").unwrap(),
                generation: 1,
                incarnation: incarnation.into(),
                lease_epoch: 1,
                launch_spec: serde_json::to_value(spec).unwrap(),
                control_plane_url: "http://127.0.0.1:9".into(),
                credential: crate::backends::RunnerCredential::new("nsr.test"),
            }
        }

        /// The command lines the script was started with, once `count` of
        /// them were written.
        fn invocations(&self, count: usize) -> Vec<String> {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let lines: Vec<String> = fs::read_to_string(self.root.join("args"))
                    .unwrap_or_default()
                    .lines()
                    .map(str::to_string)
                    .collect();
                if lines.len() >= count || Instant::now() >= deadline {
                    return lines;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let task = self.task();
            for role in [Role::Work, Role::Read] {
                let _ = self.backend.remove_workload(&task, role);
            }
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[tokio::test]
    async fn a_launch_the_check_refuses_creates_nothing() {
        let mut scratch = Scratch::new();
        let mut config = (*scratch.backend.config).clone();
        config.launch_check = Some(LaunchCheck::new(|spec| {
            Err(format!(
                "workflow '{}' is not allowed here",
                spec.workflow_id
            ))
        }));
        scratch.backend = LocalBackend::new(config);
        let task = scratch.task();

        let refused = scratch
            .backend
            .provision(&task, &scratch.launch("task-1-g1"))
            .await
            .expect_err("refused by the check");

        assert!(
            refused.to_string().contains("'wf' is not allowed here"),
            "{refused}"
        );
        assert!(!scratch.backend.storage_dir(&task).exists(), "no storage");
        assert!(
            !scratch.backend.workload_dir(&task, Role::Work).exists(),
            "no workload"
        );
        assert!(!scratch.root.join("args").exists(), "no runner");
    }

    #[tokio::test]
    async fn a_reader_needs_storage_and_never_creates_it() {
        let scratch = Scratch::new();
        let task = scratch.task();

        let refused = scratch
            .backend
            .start_reader(&task, &scratch.launch("task-1-reader-g1"))
            .await
            .expect_err("no storage to read");

        assert_eq!(
            refused.kind,
            crate::backends::BackendErrorKind::StorageMissing
        );
        assert!(
            !scratch.backend.storage_dir(&task).exists(),
            "nothing created"
        );
        assert!(scratch
            .backend
            .observe_reader(&task)
            .await
            .unwrap()
            .is_none());
        assert!(!scratch
            .root
            .join("local/readers")
            .join(&task.reader)
            .exists());
    }

    #[tokio::test]
    async fn a_reader_has_its_own_name_directory_group_and_flag() {
        let scratch = Scratch::new();
        let task = scratch.task();
        assert_ne!(task.reader, task.workload);
        fs::create_dir_all(scratch.backend.storage_dir(&task)).unwrap();

        let started = scratch
            .backend
            .start_reader(&task, &scratch.launch("task-1-reader-g1"))
            .await
            .expect("start the reader");
        assert!(started.created);
        let again = scratch
            .backend
            .start_reader(&task, &scratch.launch("task-1-reader-g1-again"))
            .await
            .expect("reconcile");
        assert!(!again.created, "one reader per task");
        assert_eq!(again.workload.uid, started.workload.uid);

        let reader_pid = scratch.backend.reader_pid(&task).expect("a live reader");
        assert_eq!(process_group(reader_pid), Some(reader_pid), "its own group");
        assert!(scratch
            .root
            .join("local/readers")
            .join(&task.reader)
            .is_dir());
        let observed = scratch.backend.observe(&task).await.unwrap();
        assert!(
            observed.workload.is_none(),
            "the reader is not the workload"
        );
        assert!(scratch.invocations(1)[0].contains("--read-only"));

        // A working runner beside it (as the controller never does, but the
        // adapter must still keep them apart): its own group and directory.
        scratch
            .backend
            .provision(&task, &scratch.launch("task-1-g1"))
            .await
            .expect("provision");
        let work_pid = scratch.backend.runner_pid(&task).expect("a live runner");
        assert_ne!(process_group(work_pid), process_group(reader_pid));
        assert!(!scratch.invocations(2)[1].contains("--read-only"));

        // Removing the working runner — group kill, marked-group sweep —
        // leaves the reader alone, and removing the reader leaves the
        // storage alone.
        assert!(scratch
            .backend
            .release_compute(&task)
            .await
            .unwrap()
            .is_complete());
        assert!(scratch.backend.runner_pid(&task).is_none());
        assert_eq!(
            scratch.backend.reader_pid(&task),
            Some(reader_pid),
            "still reading"
        );

        let removal = scratch.backend.stop_reader(&task).await.unwrap();
        assert!(removal.is_complete(), "{removal:?}");
        assert!(scratch.backend.reader_pid(&task).is_none());
        assert!(!scratch
            .root
            .join("local/readers")
            .join(&task.reader)
            .exists());
        assert!(scratch.backend.storage_dir(&task).is_dir(), "storage stays");
        assert!(scratch
            .backend
            .stop_reader(&task)
            .await
            .unwrap()
            .is_complete());
    }

    #[test]
    fn a_group_with_the_workloads_mark_is_killed() {
        let mut ours = group_leader(Some("local-ours"));

        assert!(kill_marked_group(ours.id(), "local-ours"));

        let status = ours.wait().expect("reaped");
        assert!(!status.success(), "killed: {status:?}");
    }
}
