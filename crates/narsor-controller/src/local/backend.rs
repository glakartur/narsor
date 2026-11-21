//! The local adapter: `narsor-runner` as a child process of the host, or in
//! a Docker container (the Docker work mode), task storage as a local
//! directory (U4).
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
//!     workload.json                   incarnation, generation, pid or container
//!     credential                      the runner credential, 0600, outside storage
//!     launch-spec.json                the immutable launch spec, outside storage
//!     runner.log                      the runner's stdout and stderr (process)
//!     secrets/                        the task's secrets (container), 0700
//! {root}/readers/{reader}/            the task's read-only runner (D63), same files
//! {root}/tasks/{storage}.json         the per-task record (KTD14)
//! ```
//!
//! A workload directory exists exactly while the workload does, so the
//! deterministic name *is* the identity: a restarted controller finds the
//! process or container again through `workload.json` instead of starting a
//! second one. Runners run in their own process group (or container) and
//! survive the controller, like a Pod survives its controller (R14);
//! removing compute kills the group or removes the container.
//!
//! # Two kinds of workload
//!
//! A `workload.json` records a process (pid and start time) or a container
//! (its name, its image once known, when it was started, a problem that
//! ended it). A record written before containers existed has no container
//! and reads as a process. Observing and removing a workload follow the
//! kind of its *record*, never the configured mode: a controller restarted
//! in the other mode still observes and removes what the previous one left.
//!
//! In the Docker work mode ([`ContainerMode`]) `provision` and `resume`
//! check what the container needs, write a container record and ask the
//! image build job for the task image, and return at once; `observe`
//! reports `Starting` while the image builds, then copies the task's secrets
//! into the workload directory and starts the container (KTD5–KTD7, KTD10,
//! KTD12). There is no fallback to a host process (R4). The read-only runner
//! (D63) stays a host process in both modes.
//!
//! # The per-task record
//!
//! `{root}/tasks/{storage}.json` keeps the kind of the task's last working
//! runner and the image it last ran on (KTD14). It is a controller file
//! beside the workloads, not a file on task storage: the agent in the
//! container writes task storage and must not choose the image of its next
//! start or hide it from garbage collection. It outlives the workload
//! directory (pause, release, loss) and goes with the storage on archive.
//! It gives the image a resumed task runs on (R11), the warning when a task
//! that ran in a container starts as a host process (R3) and the images
//! garbage collection keeps.
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
//! A container needs none of this: its name is the workload's uid, and
//! removing it takes everything in it.
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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use narsor_engine::config::{CLAUDE_CONFIG_DIR_ENV, CODEX_HOME_ENV, ENV_PREFIX};
use serde::{Deserialize, Serialize};

use narsor_controller_runtime::backend::{
    ArchiveProgress, BackendError, BackendResult, ExecutionBackend, Provisioned, ReaderStart,
    Removal, StorageState, TaskObservation, TaskResources, WorkloadLaunch, WorkloadPhase,
    WorkloadState,
};
use narsor_engine::domain::{TaskId, TaskImage, TaskLaunchSpec, TaskRepositorySource};
use narsor_engine::environment::{BackendCapabilities, BackendKind};
use narsor_engine::workspace::provisioner::USER_GIT_ACCESS_ENVS;

use super::docker::{ContainerState, DockerError, DockerTransport, Mount, RunRequest, UserIds};
use super::image::{definition_hash, BuildState, TaskImageBuilds, DEFINITION_LABEL, MANAGED_LABEL};
use super::secrets::{SecretError, SecretStore};

/// How long removing compute waits for a killed runner to disappear before
/// it answers "in progress" and lets the controller come back.
const KILL_WAIT: Duration = Duration::from_secs(2);

/// The environment variable every runner gets, set to its workload's uid:
/// what proves a process group still ours after the runner is gone.
const WORKLOAD_MARK_ENV: &str = "NARSOR_LOCAL_WORKLOAD_UID";

/// Where the runner binary is mounted in a container.
pub const RUNNER_IN_CONTAINER: &str = "/opt/narsor/bin/narsor-runner";

/// Where the home sources are mounted in a container, one directory each.
const HOME_SOURCES_IN_CONTAINER: &str = "/run/narsor/home";

/// The label with the workload uid a container belongs to.
pub const WORKLOAD_LABEL: &str = "io.narsor.workload";

/// The label with the task a container works for.
pub const TASK_LABEL: &str = "io.narsor.task";

/// The variables of the controller's environment a container gets, besides
/// every `LC_*` one (KTD7). Nothing else of that environment reaches it.
const PASS_THROUGH_ENV: [&str; 9] = [
    "TERM",
    "LANG",
    "TZ",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
];

/// How many lines of a container's output tell why it could not start.
const LOG_TAIL: usize = 50;

/// The directory of the per-task records under the adapter's root.
const TASKS_DIR: &str = "tasks";

/// How long after its start a container's failure is checked for a runner
/// that cannot execute in the image.
pub const DEFAULT_START_WINDOW: Duration = Duration::from_secs(15);

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
    /// Extra arguments appended to every `narsor-runner` process invocation
    /// (timing knobs, for instance). A container runner gets
    /// [`ContainerMode::runner_args`] instead.
    pub runner_args: Vec<OsString>,
    /// Environment variables set for every runner, on top of the
    /// controller's own environment less its `NARSOR_*` settings; a
    /// container gets them on top of a short allowlist (KTD7).
    pub env: Vec<(OsString, OsString)>,
    /// Run on every launch spec before its task's storage or workload is
    /// created; none by default.
    pub launch_check: Option<LaunchCheck>,
    /// Whether runners inherit none of the user's credential variables
    /// ([`is_user_credential`]): harness logins, Git and forge credentials.
    /// Set for a control plane beyond this machine with a private HOME (R17).
    pub withhold_user_credentials: bool,
    /// How containers are observed and removed, in either mode: a container
    /// a Docker-mode controller left behind is still removed by a
    /// process-mode one. `None` leaves such a container unobservable.
    pub docker: Option<Arc<dyn DockerTransport>>,
    /// The Docker work mode: working runners run in containers. `None` runs
    /// them as host processes.
    pub container_mode: Option<ContainerMode>,
}

/// What the Docker work mode needs beside the transport.
#[derive(Debug, Clone)]
pub struct ContainerMode {
    /// The task image build jobs (KTD12).
    pub builds: TaskImageBuilds,
    /// The controller's token store (KTD10).
    pub secrets: SecretStore,
    /// The host user and group the container runs as (KTD5).
    pub user: UserIds,
    /// The controller user's HOME, which a leading `~` of a home source
    /// stands for.
    pub host_home: Option<PathBuf>,
    /// The configuration file the runner reads, mounted read-only.
    pub config_file: Option<PathBuf>,
    /// Extra arguments of every container runner (timing knobs, for
    /// instance).
    pub runner_args: Vec<OsString>,
    /// How long after its start a failing container is checked for a runner
    /// that cannot execute in its image.
    pub start_window: Duration,
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
            root: absolute_root(root.into()),
            program: program.into(),
            runner_args: Vec::new(),
            env: Vec::new(),
            launch_check: None,
            withhold_user_credentials: false,
            docker: None,
            container_mode: None,
        }
    }
}

/// `root` made absolute against the current directory, once: container
/// mounts and workdirs need absolute host paths. An absolute root, or one
/// the current directory cannot resolve, stays as it is.
fn absolute_root(root: PathBuf) -> PathBuf {
    if root.is_absolute() {
        return root;
    }
    std::path::absolute(&root).unwrap_or(root)
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
    /// A process or a container.
    #[serde(flatten)]
    kind: WorkloadKind,
    /// Set once the runner was seen to terminate.
    #[serde(default)]
    exited: Option<ExitRecord>,
}

/// The two kinds of workload. A record without a `container` is a process:
/// what every record written before the Docker work mode is.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
enum WorkloadKind {
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
struct ContainerRecord {
    /// The container's name, also its [`WORKLOAD_LABEL`]: the workload uid.
    name: String,
    /// The image it runs, once known; `None` while the image builds.
    #[serde(default)]
    image: Option<String>,
    /// When the container was created, in ms since the epoch; `None` before.
    #[serde(default)]
    started_at_ms: Option<u64>,
    /// The control plane the runner reaches.
    control_plane: String,
    /// Why the workload ended without its runner, when it did.
    #[serde(default)]
    problem: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
struct ExitRecord {
    code: Option<i32>,
}

/// The per-task record (KTD14): what a task's last working runner was.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct TaskRecord {
    kind: LastRunner,
    /// The image the task last ran on, kept across a process run so a
    /// return to the Docker mode finds it again.
    #[serde(default)]
    image: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum LastRunner {
    Process,
    Container,
}

/// What a look at a workload found: its phase (`None`: it is gone), and the
/// record to write back when the look learned something.
struct Look {
    phase: Option<WorkloadPhase>,
    updated: Option<WorkloadRecord>,
}

impl Look {
    fn phase(phase: WorkloadPhase) -> Self {
        Self {
            phase: Some(phase),
            updated: None,
        }
    }
}

/// What a container of a task gets from the host, checked before its
/// workload is created and again when the container starts.
struct ContainerInputs {
    /// Local workspace sources, mounted read-only.
    local_sources: Vec<PathBuf>,
    /// The home sources: the host path, the mount point and the target.
    home_sources: Vec<(PathBuf, String, String)>,
    /// The token store entries the task needs.
    secret_names: Vec<String>,
    /// Whether the task image (itself or its components) names secrets.
    task_image_secrets: bool,
}

impl LocalBackend {
    /// An adapter over `config.root`.
    pub fn new(config: LocalBackendConfig) -> Self {
        Self {
            config: Arc::new(config),
            children: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The pid of the task's runner while it runs as a host process — what
    /// a test kills to model the host losing the workload.
    pub fn runner_pid(&self, task: &TaskResources) -> Option<u32> {
        self.live_pid(task, Role::Work)
    }

    /// The pid of the task's read-only runner while it runs.
    pub fn reader_pid(&self, task: &TaskResources) -> Option<u32> {
        self.live_pid(task, Role::Read)
    }

    fn live_pid(&self, task: &TaskResources, role: Role) -> Option<u32> {
        let record = self.read_record(task, role).ok()??;
        let WorkloadKind::Process { pid, .. } = record.kind else {
            return None;
        };
        match self.look(task, role, &record) {
            Ok(Look {
                phase: Some(phase), ..
            }) if phase.is_alive() => Some(pid),
            _ => None,
        }
    }

    /// Where the task's storage lives.
    pub fn storage_dir(&self, task: &TaskResources) -> PathBuf {
        self.config.root.join("storage").join(&task.storage)
    }

    /// Removes the task images no per-task record names and no cached
    /// definition has as its latest (KTD12); nothing outside the Docker
    /// mode. Run at controller start; a failure is only logged.
    pub fn collect_image_garbage(&self) {
        let Some(mode) = &self.config.container_mode else {
            return;
        };
        match mode.builds.collect_garbage(&self.referenced_images()) {
            Ok(0) => {}
            Ok(removed) => tracing::info!(removed, "local backend: removed unused task images"),
            Err(error) => tracing::warn!(%error, "local backend: cannot collect task images"),
        }
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

    fn task_record_path(&self, task: &TaskResources) -> PathBuf {
        self.config
            .root
            .join(TASKS_DIR)
            .join(format!("{}.json", task.storage))
    }

    fn children(&self) -> MutexGuard<'_, HashMap<String, Child>> {
        self.children.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn docker(&self) -> BackendResult<&Arc<dyn DockerTransport>> {
        self.config.docker.as_ref().ok_or_else(|| {
            BackendError::transient("this controller has no Docker transport to reach containers")
        })
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
        let bytes = serde_json::to_vec_pretty(record)
            .map_err(|error| BackendError::transient(error.to_string()))?;
        write_atomically(&path, &bytes)
            .map_err(|error| io_error("write the workload record", error))
    }

    /// The task's per-task record; one that cannot be read counts as none.
    fn read_task_record(&self, task: &TaskResources) -> Option<TaskRecord> {
        let path = self.task_record_path(task);
        let bytes = fs::read(&path).ok()?;
        serde_json::from_slice(&bytes)
            .inspect_err(|error| {
                tracing::warn!(path = %path.display(), %error, "local backend: ignoring an unreadable task record");
            })
            .ok()
    }

    fn write_task_record(&self, task: &TaskResources, record: &TaskRecord) -> io::Result<()> {
        let path = self.task_record_path(task);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_atomically(
            &path,
            &serde_json::to_vec_pretty(record).map_err(io::Error::other)?,
        )
    }

    /// Every image a per-task record names, and every image a container
    /// workload that has not exited was given (it may not have started yet,
    /// so no per-task record names it): what garbage collection keeps.
    fn referenced_images(&self) -> HashSet<String> {
        let mut images: HashSet<String> = json_files(&self.config.root.join(TASKS_DIR))
            .filter_map(|bytes| serde_json::from_slice::<TaskRecord>(&bytes).ok())
            .filter_map(|record| record.image)
            .collect();
        let workloads = self.config.root.join(Role::Work.directory());
        let records = fs::read_dir(workloads)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| fs::read(entry.path().join("workload.json")).ok())
            .filter_map(|bytes| serde_json::from_slice::<WorkloadRecord>(&bytes).ok());
        for record in records {
            if let (WorkloadKind::Container { container }, None) = (record.kind, record.exited) {
                images.extend(container.image);
            }
        }
        images
    }

    /// The workload's phase, following the kind of its record.
    fn look(
        &self,
        task: &TaskResources,
        role: Role,
        record: &WorkloadRecord,
    ) -> BackendResult<Look> {
        match &record.kind {
            WorkloadKind::Process { pid, start_ticks } => {
                let (phase, exit) =
                    self.process_phase(task, role, *pid, *start_ticks, record.exited)?;
                Ok(Look {
                    phase: Some(phase),
                    updated: exit.map(|exit| WorkloadRecord {
                        exited: Some(exit),
                        ..record.clone()
                    }),
                })
            }
            WorkloadKind::Container { container } => {
                self.container_look(task, role, record, container)
            }
        }
    }

    /// A process's phase, and whether the record learned something new (an
    /// exit) that must be written back.
    fn process_phase(
        &self,
        task: &TaskResources,
        role: Role,
        pid: u32,
        start_ticks: Option<u64>,
        exited: Option<ExitRecord>,
    ) -> BackendResult<(WorkloadPhase, Option<ExitRecord>)> {
        // The parent asks its child first: only it can reap the process and
        // learn the exit code, whatever another instance recorded.
        let name = role.name(task);
        let mut children = self.children();
        if let Some(child) = children.get_mut(name) {
            if child.id() == pid {
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
        if let Some(exit) = exited {
            return Ok((WorkloadPhase::Exited { code: exit.code }, None));
        }
        // Started by an earlier controller: alive is all that can be told;
        // its exit code went to whoever reaped it.
        if process_alive(pid, start_ticks) {
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
        let Some(record) = self.read_record(task, role)? else {
            return Ok(None);
        };
        let Look { phase, updated } = self.look(task, role, &record)?;
        let record = match updated {
            Some(mut updated) => {
                updated.resource_generation = record.resource_generation + 1;
                if let Err(error) = self.write_record(task, role, &updated) {
                    // A concurrent removal took the workload while what it
                    // learned was being written back: it is gone, not
                    // unobservable.
                    if !self.workload_dir(task, role).exists() {
                        return Ok(None);
                    }
                    return Err(error);
                }
                updated
            }
            None => record,
        };
        Ok(phase.map(|phase| WorkloadState {
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
        let spec = self.checked_spec(launch)?;
        let storage = self.storage_dir(task);
        let created_storage = !storage.is_dir();
        if created_storage && !create_storage {
            return Err(BackendError::storage_missing(format!(
                "task storage '{}' does not exist",
                storage.display()
            )));
        }
        if !created_storage {
            if let Some(existing) = self.observe_workload(task, Role::Work)? {
                return Ok(Provisioned {
                    workload: existing,
                    created_workload: false,
                    created_storage,
                });
            }
        }
        // Everything the Docker mode can refuse is refused before anything
        // of the task is created; there is no fallback to a host process.
        let container_image = match &self.config.container_mode {
            Some(mode) => Some(self.container_preflight(mode, &spec)?),
            None => None,
        };
        if created_storage {
            fs::create_dir_all(&storage)
                .map_err(|error| io_error("create the task storage", error))?;
        }
        let workload = match container_image {
            Some(image) => match self.create_container(task, launch, &image) {
                Ok(workload) => workload,
                Err(error) => {
                    // A refusal leaves no new task storage behind.
                    if created_storage {
                        let _ = fs::remove_dir_all(&storage);
                    }
                    return Err(error);
                }
            },
            None => {
                let previous = self.note_process_start(task);
                let workload = self.spawn(task, Role::Work, launch, &storage)?;
                if let Some(previous) = previous {
                    self.remember_process_start(task, previous);
                }
                workload
            }
        };
        Ok(Provisioned {
            workload,
            created_workload: true,
            created_storage,
        })
    }

    /// R3: warns when a task whose last runner was a container now starts
    /// as a host process; returns its per-task record then.
    fn note_process_start(&self, task: &TaskResources) -> Option<TaskRecord> {
        let previous = self
            .read_task_record(task)
            .filter(|record| record.kind == LastRunner::Container)?;
        tracing::warn!(
            task = %task.task_id,
            "local backend: task {} last ran in a Docker container and now starts as a host \
             process (controller.local.mode is 'process'): it runs as your user with your HOME \
             settings, and tools installed only in its image are missing",
            task.task_id
        );
        Some(previous)
    }

    /// Records that the task's runner is a host process now.
    fn remember_process_start(&self, task: &TaskResources, previous: TaskRecord) {
        let record = TaskRecord {
            kind: LastRunner::Process,
            ..previous
        };
        if let Err(error) = self.write_task_record(task, &record) {
            tracing::warn!(task = %task.task_id, %error, "local backend: cannot update the task record");
        }
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

    /// The command line of a runner process: a reader gets `--read-only`.
    fn runner_args(
        &self,
        role: Role,
        launch: &WorkloadLaunch,
        dir: &Path,
        storage: &Path,
    ) -> Vec<OsString> {
        let mut args = task_args(
            launch.task_id.as_str(),
            storage,
            dir,
            &launch.control_plane_url,
            &launch.incarnation,
        );
        if role == Role::Read {
            args.push("--read-only".into());
        }
        args.extend(self.config.runner_args.iter().map(Into::into));
        args
    }

    /// Creates the workload directory with the credential and the launch
    /// spec; a half-made one is removed again.
    fn prepare_workload_dir(
        &self,
        task: &TaskResources,
        role: Role,
        launch: &WorkloadLaunch,
    ) -> BackendResult<PathBuf> {
        use std::os::unix::fs::OpenOptionsExt;

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
        Ok(dir)
    }

    fn spawn(
        &self,
        task: &TaskResources,
        role: Role,
        launch: &WorkloadLaunch,
        storage: &Path,
    ) -> BackendResult<WorkloadState> {
        use std::os::unix::process::CommandExt;

        let dir = self.prepare_workload_dir(task, role, launch)?;
        let log = fs::File::create(dir.join("runner.log"))
            .map_err(|error| io_error("create the runner log", error))?;
        let log_err = log
            .try_clone()
            .map_err(|error| io_error("create the runner log", error))?;
        let uid = format!("local-{}", uuid::Uuid::new_v4().simple());
        let mut command = Command::new(&self.config.program);
        command.args(self.runner_args(role, launch, &dir, storage));
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
            kind: WorkloadKind::Process {
                pid: child.id(),
                start_ticks: process_start_ticks(child.id()),
            },
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

    // --- the Docker work mode ---

    /// Refuses a task the Docker mode cannot run, naming the fix: no sealed
    /// task image, step images, Docker missing, unreachable or rootless, a
    /// host path it cannot mount, a secret missing from the store. Returns
    /// the task image.
    fn container_preflight(
        &self,
        mode: &ContainerMode,
        spec: &TaskLaunchSpec,
    ) -> BackendResult<TaskImage> {
        let image = spec.environment.task_image.clone().ok_or_else(|| {
            BackendError::rejected(
                "the docker work mode needs a task image, and this task has none: define \
                 `task_image` in runtime.yaml (a task created before that has none sealed and \
                 must be created again), or set controller.local.mode to 'process'",
            )
        })?;
        if !spec.environment.step_images.is_empty() {
            let steps: Vec<String> = spec
                .environment
                .step_images
                .keys()
                .map(|step| format!("'{step}'"))
                .collect();
            return Err(BackendError::rejected(format!(
                "the workflow sets its own image for step {} (states.<step>.image), which the \
                 docker work mode does not support: a task runs in one container from \
                 runtime.yaml task_image. Remove the step image or set controller.local.mode to \
                 'process'",
                steps.join(", ")
            )));
        }
        let docker = self
            .docker()
            .map_err(|error| BackendError::rejected(error.message))?;
        // Docker missing or rootless needs the operator; a daemon that does
        // not answer now may answer at the next attempt.
        docker.check_available().map_err(|error| match error {
            DockerError::NotInstalled | DockerError::Rootless => BackendError::rejected(format!(
                "the docker work mode cannot start the task: {error}"
            )),
            DockerError::Unreachable(_) | DockerError::Missing(_) | DockerError::Failed { .. } => {
                docker_error(&error)
            }
        })?;
        let inputs =
            container_inputs(mode, spec, &self.config.root).map_err(BackendError::rejected)?;
        let mut missing = Vec::new();
        for name in &inputs.secret_names {
            match mode.secrets.read(name) {
                Ok(Some(_)) => {}
                Ok(None) => missing.push(name.clone()),
                Err(error) => return Err(BackendError::rejected(error.to_string())),
            }
        }
        if !missing.is_empty() {
            return Err(BackendError::rejected(
                SecretError::Missing(missing).to_string(),
            ));
        }
        Ok(image)
    }

    /// Writes a container record and asks for the image: the one the
    /// per-task record names while it exists (R11), else the build job's.
    /// Never waits for a build.
    fn create_container(
        &self,
        task: &TaskResources,
        launch: &WorkloadLaunch,
        image: &TaskImage,
    ) -> BackendResult<WorkloadState> {
        let mode = self
            .config
            .container_mode
            .as_ref()
            .expect("only the Docker mode creates containers");
        let hash = definition_hash(image);
        let docker = self.docker()?;
        let recorded = self
            .read_task_record(task)
            .and_then(|record| record.image)
            .filter(|id| {
                matches!(
                    docker.inspect_image(id),
                    Ok(Some(info)) if info.labels.get(DEFINITION_LABEL) == Some(&hash)
                )
            });
        let image_id = match recorded {
            Some(id) => Some(id),
            None => match mode.builds.request(image) {
                // A retryable failure is retried by the build job; the
                // workload waits as for a build.
                BuildState::Building | BuildState::Retrying(_) => None,
                BuildState::Ready(id) => Some(id),
                BuildState::Failed(problem) => return Err(BackendError::rejected(problem)),
            },
        };
        let dir = self.prepare_workload_dir(task, Role::Work, launch)?;
        let uid = format!("narsor-local-{}", uuid::Uuid::new_v4().simple());
        let record = WorkloadRecord {
            uid: uid.clone(),
            resource_generation: 1,
            incarnation: launch.incarnation.clone(),
            launch_generation: launch.generation,
            kind: WorkloadKind::Container {
                container: ContainerRecord {
                    name: uid,
                    image: image_id,
                    started_at_ms: None,
                    control_plane: launch.control_plane_url.clone(),
                    problem: None,
                },
            },
            exited: None,
        };
        if let Err(error) = self.write_record(task, Role::Work, &record) {
            let _ = fs::remove_dir_all(&dir);
            return Err(error);
        }
        Ok(WorkloadState {
            uid: record.uid,
            resource_generation: record.resource_generation,
            incarnation: record.incarnation,
            launch_generation: record.launch_generation,
            phase: WorkloadPhase::Starting,
        })
    }

    /// A container workload's phase: `Starting` while its image builds,
    /// then what Docker says. Starts the container once the image is ready.
    fn container_look(
        &self,
        task: &TaskResources,
        role: Role,
        record: &WorkloadRecord,
        container: &ContainerRecord,
    ) -> BackendResult<Look> {
        if let Some(exit) = record.exited {
            return Ok(Look::phase(WorkloadPhase::Exited { code: exit.code }));
        }
        if container.started_at_ms.is_some() {
            return self.container_state(task, record, container);
        }
        let Some(mode) = &self.config.container_mode else {
            return Ok(ended(
                task,
                record,
                None,
                "the controller no longer runs in the docker work mode, so the task's container \
                 was never started"
                    .to_string(),
            ));
        };
        let dir = self.workload_dir(task, role);
        let spec = match read_spec(&dir) {
            Ok(spec) => spec,
            Err(problem) => return Ok(ended(task, record, None, problem)),
        };
        let image_id = match &container.image {
            Some(id) => id.clone(),
            None => {
                let Some(image) = &spec.environment.task_image else {
                    let problem = "the task has no task image".to_string();
                    return Ok(ended(task, record, None, problem));
                };
                match mode.builds.poll(image) {
                    BuildState::Building | BuildState::Retrying(_) => {
                        return Ok(Look::phase(WorkloadPhase::Starting))
                    }
                    BuildState::Failed(problem) => return Ok(ended(task, record, None, problem)),
                    BuildState::Ready(id) => id,
                }
            }
        };
        self.start_container(task, mode, record, container, &spec, &dir, image_id)
    }

    /// Prepares the secrets directory and starts the container on
    /// `image_id`; reports its state.
    #[allow(clippy::too_many_arguments)]
    fn start_container(
        &self,
        task: &TaskResources,
        mode: &ContainerMode,
        record: &WorkloadRecord,
        container: &ContainerRecord,
        spec: &TaskLaunchSpec,
        dir: &Path,
        image_id: String,
    ) -> BackendResult<Look> {
        let docker = self.docker()?;
        let started = WorkloadRecord {
            kind: WorkloadKind::Container {
                container: ContainerRecord {
                    image: Some(image_id.clone()),
                    started_at_ms: Some(now_ms()),
                    ..container.clone()
                },
            },
            ..record.clone()
        };
        // A container this controller started before it could record so is
        // the workload's: its name is the workload's uid.
        match docker.inspect_container(&container.name) {
            Ok(ContainerState::Missing) => {}
            Ok(ContainerState::Unreachable(message)) => return Err(unreachable(&message)),
            Ok(_) => {
                let WorkloadKind::Container { container } = &started.kind else {
                    unreachable!("a container record")
                };
                let mut look = self.container_state(task, &started, container)?;
                look.updated.get_or_insert(started);
                return Ok(look);
            }
            Err(error) => return Err(docker_error(&error)),
        }
        let request = match self.run_request(mode, task, record, container, spec, dir, &image_id) {
            Ok(request) => request,
            Err(problem) => return Ok(ended(task, record, None, problem)),
        };
        let task_record = TaskRecord {
            kind: LastRunner::Container,
            image: Some(image_id),
        };
        self.write_task_record(task, &task_record)
            .map_err(|error| io_error("write the task record", error))?;
        match docker.run_container(&request) {
            Ok(_) => {}
            Err(DockerError::Unreachable(message)) => return Err(unreachable(&message)),
            Err(error) => {
                let problem = format!("cannot start the task's container: {error}");
                return Ok(ended(task, record, None, problem));
            }
        }
        // The new image is named by a per-task record now; older ones may go.
        if let Err(error) = mode.builds.collect_garbage(&self.referenced_images()) {
            tracing::warn!(%error, "local backend: cannot collect task images");
        }
        let WorkloadKind::Container { container } = &started.kind else {
            unreachable!("a container record")
        };
        let mut look = self.container_state(task, &started, container)?;
        look.updated.get_or_insert(started);
        Ok(look)
    }

    /// The phase of a started container, from Docker. An unreachable daemon
    /// is "cannot tell", never a loss.
    fn container_state(
        &self,
        task: &TaskResources,
        record: &WorkloadRecord,
        container: &ContainerRecord,
    ) -> BackendResult<Look> {
        let docker = self.docker()?;
        let state = docker
            .inspect_container(&container.name)
            .map_err(|error| docker_error(&error))?;
        match state {
            ContainerState::Running => Ok(Look::phase(WorkloadPhase::Running)),
            ContainerState::Starting => Ok(Look::phase(WorkloadPhase::Starting)),
            ContainerState::Missing => Ok(Look {
                phase: None,
                updated: None,
            }),
            ContainerState::Unreachable(message) => Err(unreachable(&message)),
            ContainerState::Exited(code) => {
                let early = container.started_at_ms.is_some_and(|started| {
                    let window = self
                        .config
                        .container_mode
                        .as_ref()
                        .map_or(DEFAULT_START_WINDOW, |mode| mode.start_window);
                    now_ms().saturating_sub(started) <= duration_ms(window)
                });
                let problem = (code != 0 && early)
                    .then(|| docker.container_logs(&container.name, LOG_TAIL).ok())
                    .flatten()
                    .and_then(|logs| loader_problem(&logs));
                match problem {
                    Some(problem) => Ok(ended(task, record, Some(code), problem)),
                    None => Ok(Look {
                        phase: Some(WorkloadPhase::Exited { code: Some(code) }),
                        updated: Some(WorkloadRecord {
                            exited: Some(ExitRecord { code: Some(code) }),
                            ..record.clone()
                        }),
                    }),
                }
            }
        }
    }

    /// The `docker run` of the task's container (KTD5–KTD7): host paths
    /// mounted at the same paths, the runner binary at a fixed one, a
    /// private HOME, the secrets copied into the workload directory.
    #[allow(clippy::too_many_arguments)]
    fn run_request(
        &self,
        mode: &ContainerMode,
        task: &TaskResources,
        record: &WorkloadRecord,
        container: &ContainerRecord,
        spec: &TaskLaunchSpec,
        dir: &Path,
        image_id: &str,
    ) -> Result<RunRequest, String> {
        let inputs = container_inputs(mode, spec, &self.config.root)?;
        let storage = self.storage_dir(task);
        let mut mounts = vec![
            mount(&storage, false),
            mount(dir, true),
            Mount {
                source: self.config.program.clone(),
                target: RUNNER_IN_CONTAINER.into(),
                read_only: true,
            },
        ];
        let mut args = task_args(
            task.task_id.as_str(),
            &storage,
            dir,
            &container.control_plane,
            &record.incarnation,
        );
        args.extend(["--home".into(), "private".into()]);
        if let Some(file) = &mode.config_file {
            mounts.push(mount(file, true));
            args.extend(["--config".into(), file.into()]);
        }
        if !inputs.secret_names.is_empty() {
            let secrets_dir = dir.join("secrets");
            let _ = fs::remove_dir_all(&secrets_dir);
            mode.secrets
                .copy_into(inputs.secret_names.iter().map(String::as_str), &secrets_dir)
                .map_err(|error| error.to_string())?;
            mounts.push(mount(&secrets_dir, true));
            args.extend(["--secrets-dir".into(), secrets_dir.into()]);
            if inputs.task_image_secrets {
                args.push("--task-image-secrets".into());
            }
        }
        for source in &inputs.local_sources {
            mounts.push(mount(source, true));
        }
        for (host, target, to) in &inputs.home_sources {
            mounts.push(Mount {
                source: host.clone(),
                target: target.into(),
                read_only: true,
            });
            args.push("--home-source".into());
            args.push(format!("{target}={to}").into());
        }
        args.extend(mode.runner_args.iter().cloned());
        let args = args
            .into_iter()
            .map(|arg| {
                arg.into_string()
                    .map_err(|arg| format!("the runner argument {arg:?} is not UTF-8"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(RunRequest {
            name: container.name.clone(),
            image: image_id.to_string(),
            labels: BTreeMap::from([
                (MANAGED_LABEL.to_string(), "true".to_string()),
                (WORKLOAD_LABEL.to_string(), container.name.clone()),
                (TASK_LABEL.to_string(), task.task_id.to_string()),
            ]),
            user: mode.user,
            mounts,
            env: container_env(std::env::vars_os(), &self.config.env, &record.uid),
            workdir: Some(storage),
            program: RUNNER_IN_CONTAINER.to_string(),
            args,
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
        match &record.kind {
            WorkloadKind::Process { pid, start_ticks } => {
                let (phase, _) =
                    self.process_phase(task, role, *pid, *start_ticks, record.exited)?;
                if phase.is_alive() {
                    // SAFETY: signalling the process group this adapter
                    // created; `process_phase` checked the pid still names
                    // that process.
                    unsafe {
                        libc::kill(-(*pid as libc::pid_t), libc::SIGKILL);
                    }
                    if !self.wait_gone(task, role, *pid, *start_ticks) {
                        return Ok(Removal::Initiated);
                    }
                }
                // The runner is gone; what it started may not be (see the
                // module docs on why the mark makes the group safe to
                // signal).
                if !kill_marked_group(*pid, &record.uid) {
                    return Ok(Removal::Initiated);
                }
                self.children().remove(role.name(task));
            }
            WorkloadKind::Container { container } => {
                // Whatever the mode now: the record says a container.
                self.docker()?
                    .remove_container(&container.name)
                    .map_err(|error| docker_error(&error))?;
            }
        }
        // The secrets directory of a container goes with the workload.
        fs::remove_dir_all(&dir).map_err(|error| io_error("remove the workload", error))?;
        Ok(Removal::Removed)
    }

    /// Waits briefly for a killed runner to go, reaping it when it is ours.
    fn wait_gone(
        &self,
        task: &TaskResources,
        role: Role,
        pid: u32,
        start_ticks: Option<u64>,
    ) -> bool {
        let deadline = Instant::now() + KILL_WAIT;
        loop {
            let gone = match self.children().get_mut(role.name(task)) {
                Some(child) => child.try_wait().map(|s| s.is_some()).unwrap_or(true),
                None => !process_alive(pid, start_ticks),
            };
            if gone || Instant::now() >= deadline {
                return gone;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Runs `call` on a blocking thread: a workload call may wait for a
    /// killed process or call the `docker` CLI, which must not hold up the
    /// async runtime.
    async fn off_thread<T: Send + 'static>(
        &self,
        call: impl FnOnce(&Self) -> BackendResult<T> + Send + 'static,
    ) -> BackendResult<T> {
        let backend = self.clone();
        tokio::task::spawn_blocking(move || call(&backend))
            .await
            .map_err(|error| {
                BackendError::transient(format!("a local backend call failed: {error}"))
            })?
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
        let task = task.clone();
        self.off_thread(move |backend| {
            Ok(TaskObservation {
                workload: backend.observe_workload(&task, Role::Work)?,
                storage: backend.storage_state(&task),
            })
        })
        .await
    }

    async fn provision(
        &self,
        task: &TaskResources,
        launch: &WorkloadLaunch,
    ) -> BackendResult<Provisioned> {
        let (task, launch) = (task.clone(), launch.clone());
        self.off_thread(move |backend| backend.create(&task, &launch, true))
            .await
    }

    async fn resume(
        &self,
        task: &TaskResources,
        launch: &WorkloadLaunch,
    ) -> BackendResult<Provisioned> {
        let (task, launch) = (task.clone(), launch.clone());
        self.off_thread(move |backend| backend.create(&task, &launch, false))
            .await
    }

    async fn release_compute(&self, task: &TaskResources) -> BackendResult<Removal> {
        let task = task.clone();
        self.off_thread(move |backend| backend.remove_workload(&task, Role::Work))
            .await
    }

    async fn archive(&self, task: &TaskResources) -> BackendResult<ArchiveProgress> {
        let task = task.clone();
        self.off_thread(move |backend| backend.archive_task(&task))
            .await
    }

    async fn observe_reader(&self, task: &TaskResources) -> BackendResult<Option<WorkloadState>> {
        let task = task.clone();
        self.off_thread(move |backend| backend.observe_workload(&task, Role::Read))
            .await
    }

    async fn start_reader(
        &self,
        task: &TaskResources,
        launch: &WorkloadLaunch,
    ) -> BackendResult<ReaderStart> {
        let (task, launch) = (task.clone(), launch.clone());
        self.off_thread(move |backend| backend.create_reader(&task, &launch))
            .await
    }

    async fn stop_reader(&self, task: &TaskResources) -> BackendResult<Removal> {
        let task = task.clone();
        self.off_thread(move |backend| backend.remove_workload(&task, Role::Read))
            .await
    }
}

impl LocalBackend {
    fn archive_task(&self, task: &TaskResources) -> BackendResult<ArchiveProgress> {
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
        // The per-task record lives as long as the storage (KTD14).
        match fs::remove_file(self.task_record_path(task)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error("delete the task record", error)),
        }
        Ok(ArchiveProgress { workload, storage })
    }
}

/// The arguments every runner of a task gets.
fn task_args(
    task_id: &str,
    storage: &Path,
    dir: &Path,
    control_plane: &str,
    incarnation: &str,
) -> Vec<OsString> {
    vec![
        "--task-id".into(),
        task_id.into(),
        "--task-storage".into(),
        storage.into(),
        "--launch-spec".into(),
        dir.join("launch-spec.json").into(),
        "--control-plane".into(),
        control_plane.into(),
        "--credential-file".into(),
        dir.join("credential").into(),
        "--incarnation".into(),
        incarnation.into(),
    ]
}

/// What a container of `spec` gets from the host, checked: the local
/// workspace sources and the home sources (a leading `~` is the
/// controller's HOME; each must exist), none of which may overlap a
/// [`protected_dirs`] entry, and the secrets of the store it needs.
fn container_inputs(
    mode: &ContainerMode,
    spec: &TaskLaunchSpec,
    root: &Path,
) -> Result<ContainerInputs, String> {
    let protected = protected_dirs(mode, root);
    let mut local_sources = Vec::new();
    for repository in &spec.workspace.repositories {
        if let TaskRepositorySource::Local { path } = &repository.source {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err(format!(
                    "the local workspace source '{}' is not an absolute path, so it cannot be \
                     mounted into the task's container",
                    path.display()
                ));
            }
            if let Some(dir) = overlapping(&protected, &path) {
                return Err(format!(
                    "the local workspace source '{}' overlaps the controller's own directory {}, \
                     so it cannot be mounted into a task's container",
                    path.display(),
                    dir.display()
                ));
            }
            local_sources.push(path);
        }
    }
    let mut home_sources = Vec::new();
    let mut secret_names = BTreeSet::new();
    let mut task_image_secrets = false;
    let components = spec
        .environment
        .task_image
        .iter()
        .flat_map(|image| &image.components);
    for component in components {
        for (index, source) in component.home.iter().enumerate() {
            let host = expand_home(&source.from, mode.host_home.as_deref()).ok_or_else(|| {
                format!(
                    "the home source '{}' of component '{}' must be an absolute path or start \
                     with '~/' (with HOME set)",
                    source.from, component.name
                )
            })?;
            if !host.exists() {
                return Err(format!(
                    "the home source '{}' of component '{}' does not exist on this host ({})",
                    source.from,
                    component.name,
                    host.display()
                ));
            }
            if let Some(protected) = overlapping(&protected, &host) {
                return Err(format!(
                    "the home source '{}' of component '{}' overlaps the controller's own \
                     directory {}, so it cannot be mounted into a task's container",
                    source.from,
                    component.name,
                    protected.display()
                ));
            }
            if source.to.contains('=') {
                return Err(format!(
                    "the home source target '{}' of component '{}' must not contain '='",
                    source.to, component.name
                ));
            }
            let target = format!("{HOME_SOURCES_IN_CONTAINER}/{}-{index}", component.name);
            home_sources.push((host, target, source.to.clone()));
        }
    }
    for secret in spec
        .environment
        .task_image
        .iter()
        .flat_map(TaskImage::all_secrets)
    {
        task_image_secrets = true;
        secret_names.insert(secret.name.clone());
    }
    secret_names.extend(
        spec.environment
            .secrets
            .iter()
            .map(|secret| secret.name.clone()),
    );
    Ok(ContainerInputs {
        local_sources,
        home_sources,
        secret_names: secret_names.into_iter().collect(),
        task_image_secrets,
    })
}

/// The directories no host path mounted into a container may overlap: the
/// controller's directory (the token store and the controller credential),
/// the directory above it (the daemon state directory, with `daemon.json`
/// and the owner token of the local API) and the backend `root` (the
/// storage and credentials of other tasks). All canonical. The directory
/// above the controller's is left out when it is `/` or holds the HOME
/// (a controller directory placed by hand), as it then is no state
/// directory and would refuse every source.
fn protected_dirs(mode: &ContainerMode, root: &Path) -> Vec<PathBuf> {
    let canonical = |path: &Path| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let controller = mode.secrets.dir().parent().map(canonical);
    let state = controller
        .as_deref()
        .and_then(Path::parent)
        .filter(|state| state.parent().is_some())
        .filter(|state| {
            !mode
                .host_home
                .as_deref()
                .is_some_and(|home| canonical(home).starts_with(state))
        })
        .map(Path::to_path_buf);
    [controller, state, Some(canonical(root))]
        .into_iter()
        .flatten()
        .collect()
}

/// The protected directory `path` (canonicalized) is in or holds, if any.
fn overlapping<'a>(protected: &'a [PathBuf], path: &Path) -> Option<&'a PathBuf> {
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    protected
        .iter()
        .find(|dir| resolved.starts_with(dir) || dir.starts_with(&resolved))
}

/// `from` with a leading `~` replaced by `home`; `None` when it is neither
/// absolute nor `~`-prefixed, or `~` cannot be expanded.
fn expand_home(from: &str, home: Option<&Path>) -> Option<PathBuf> {
    if from == "~" {
        return home.map(Path::to_path_buf);
    }
    if let Some(rest) = from.strip_prefix("~/") {
        return home.map(|home| home.join(rest));
    }
    let path = PathBuf::from(from);
    path.is_absolute().then_some(path)
}

/// The environment of a container (KTD7): the pass-through variables of
/// `process`, then the configured ones, then the workload's mark. Never
/// `PATH`, `HOME`, `USER`, `XDG_*`, `NARSOR_*` or an API key.
fn container_env(
    process: impl IntoIterator<Item = (OsString, OsString)>,
    configured: &[(OsString, OsString)],
    uid: &str,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = process
        .into_iter()
        .filter(|(name, _)| passes_through(name))
        .chain(configured.iter().cloned())
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect();
    env.push((WORKLOAD_MARK_ENV.to_string(), uid.to_string()));
    env
}

/// Whether the controller's variable `name` reaches a container.
fn passes_through(name: &OsStr) -> bool {
    let name = name.as_encoded_bytes();
    name.starts_with(b"LC_")
        || PASS_THROUGH_ENV
            .iter()
            .any(|known| name == known.as_bytes())
}

/// The problem of a runner that cannot execute in its image, from what the
/// container wrote; `None` when the output says nothing of the kind.
fn loader_problem(logs: &str) -> Option<String> {
    const LOADER: [&str; 3] = [
        "glibc_",
        "exec format error",
        "error while loading shared libraries",
    ];
    let line = logs.lines().find(|line| {
        let lower = line.to_lowercase();
        LOADER.iter().any(|marker| lower.contains(marker))
            || (lower.contains("no such file or directory") && line.contains(RUNNER_IN_CONTAINER))
    })?;
    Some(format!(
        "the runner cannot run in this image (needs glibc 2.39 or newer on the host's \
         architecture; use another task_image base): {}",
        line.trim()
    ))
}

/// The record of a workload that ended without its runner, with `problem`
/// for whoever looks; the problem is also logged.
fn ended(
    task: &TaskResources,
    record: &WorkloadRecord,
    code: Option<i32>,
    problem: String,
) -> Look {
    tracing::warn!(task = %task.task_id, %problem, "local backend: the task's container ended");
    let mut updated = record.clone();
    updated.exited = Some(ExitRecord { code });
    if let WorkloadKind::Container { container } = &mut updated.kind {
        container.problem = Some(problem);
    }
    Look {
        phase: Some(WorkloadPhase::Exited { code }),
        updated: Some(updated),
    }
}

/// The launch spec in a workload directory; a problem when it cannot be
/// read.
fn read_spec(dir: &Path) -> Result<TaskLaunchSpec, String> {
    let path = dir.join("launch-spec.json");
    let bytes =
        fs::read(&path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("the launch spec is malformed: {error}"))
}

fn mount(path: &Path, read_only: bool) -> Mount {
    Mount {
        source: path.to_path_buf(),
        target: path.to_path_buf(),
        read_only,
    }
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

/// "Cannot tell": the daemon does not answer, which is never a loss.
fn unreachable(message: &str) -> BackendError {
    BackendError::transient(format!(
        "the Docker daemon cannot be reached, so the container's state is unknown: {message}"
    ))
}

fn docker_error(error: &DockerError) -> BackendError {
    BackendError::transient(error.to_string())
}

fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    fs::write(&temp, bytes)?;
    fs::rename(&temp, path)
}

fn now_ms() -> u64 {
    duration_ms(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default(),
    )
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Whether `name` is one of the controller's own `NARSOR_*` variables,
/// which a runner never inherits.
fn is_controller_setting(name: &std::ffi::OsStr) -> bool {
    name.as_encoded_bytes().starts_with(ENV_PREFIX.as_bytes())
        && name != std::ffi::OsStr::new(narsor_engine::config::AGENT_ARGS_ENV)
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
        let environ = fs::read(format!("/proc/{pid}/environ")).ok();
        member_marked(process_group(pid), pgid, environ.as_deref(), &mark)
    })
}

/// Whether a process — in group `member_group` (`None` for a zombie or one
/// that is gone), with environment `environ` (`None` when unreadable) —
/// is a live member of group `pgid` carrying `mark`.
fn member_marked(member_group: Option<u32>, pgid: u32, environ: Option<&[u8]>, mark: &str) -> bool {
    member_group == Some(pgid) && environ.is_some_and(|environ| environ_has(environ, mark))
}

/// Whether the NUL-separated `environ` block holds exactly the entry `mark`.
fn environ_has(environ: &[u8], mark: &str) -> bool {
    environ
        .split(|byte| *byte == 0)
        .any(|entry| entry == mark.as_bytes())
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
            narsor_engine::config::AGENT_ARGS_ENV.as_ref()
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
            use narsor_engine::domain::{ResolvedEnvironment, ResourceProfile, TaskWorkspace};
            let spec = TaskLaunchSpec::new(
                "wf",
                1,
                serde_json::json!({}),
                TaskWorkspace::none(),
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
                credential: narsor_controller_runtime::backend::RunnerCredential::new("nsr.test"),
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
            narsor_controller_runtime::backend::BackendErrorKind::StorageMissing
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

    #[test]
    fn a_reader_has_its_own_name_directory_and_flag() {
        let scratch = Scratch::new();
        let task = scratch.task();
        assert_ne!(task.reader, task.workload);
        let reader_dir = scratch.backend.workload_dir(&task, Role::Read);
        assert!(reader_dir.starts_with(scratch.root.join("local/readers")));
        assert_ne!(reader_dir, scratch.backend.workload_dir(&task, Role::Work));

        let launch = scratch.launch("task-1-reader-g1");
        let storage = scratch.backend.storage_dir(&task);
        let args = |role| {
            let dir = scratch.backend.workload_dir(&task, role);
            scratch.backend.runner_args(role, &launch, &dir, &storage)
        };
        assert!(args(Role::Read).iter().any(|arg| arg == "--read-only"));
        assert!(!args(Role::Work).iter().any(|arg| arg == "--read-only"));
    }

    #[test]
    fn only_a_live_member_with_the_mark_belongs_to_the_group() {
        let mark = format!("{WORKLOAD_MARK_ENV}=local-ours");
        let ours = mark.as_bytes();
        let stranger = format!("{WORKLOAD_MARK_ENV}=local-someone-else");

        assert!(member_marked(Some(7), 7, Some(ours), &mark));
        assert!(!member_marked(Some(8), 7, Some(ours), &mark), "other group");
        assert!(!member_marked(None, 7, Some(ours), &mark), "zombie or gone");
        assert!(!member_marked(Some(7), 7, None, &mark), "unreadable");
        assert!(!member_marked(Some(7), 7, Some(stranger.as_bytes()), &mark));
        assert!(!member_marked(Some(7), 7, Some(b"A=1"), &mark), "unmarked");
    }

    #[test]
    fn only_an_exact_entry_is_the_workloads_mark() {
        let mark = format!("{WORKLOAD_MARK_ENV}=local-ours");
        let environ = |entries: &[&str]| entries.join("\0").into_bytes();

        assert!(environ_has(&environ(&["A=1", &mark, "B=2"]), &mark));
        assert!(!environ_has(&environ(&["A=1"]), &mark));
        assert!(!environ_has(&environ(&[]), &mark));
        let other = format!("{WORKLOAD_MARK_ENV}=local-someone-else");
        assert!(!environ_has(&environ(&[&other]), &mark));
        let longer = format!("{mark}-2");
        assert!(!environ_has(&environ(&[&longer]), &mark));
    }

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

    /// KTD7: a container gets the pass-through variables, the configured
    /// ones and the mark; nothing else of the controller's environment.
    #[test]
    fn a_containers_environment_is_exactly_the_allowlist() {
        let process: Vec<(OsString, OsString)> = [
            ("PATH", "/usr/bin"),
            ("HOME", "/home/me"),
            ("USER", "me"),
            ("XDG_CONFIG_HOME", "/home/me/.config"),
            ("NARSOR_ENROLLMENT_CODE", "secret"),
            ("ANTHROPIC_API_KEY", "sk-ant"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "token"),
            ("SSH_AUTH_SOCK", "/run/ssh"),
            ("TERM", "xterm"),
            ("LANG", "pl_PL.UTF-8"),
            ("LC_ALL", "C"),
            ("TZ", "Europe/Warsaw"),
            ("https_proxy", "http://proxy:3128"),
            ("NO_PROXY", "localhost"),
        ]
        .into_iter()
        .map(|(name, value)| (name.into(), value.into()))
        .collect();
        let configured = vec![("RUST_LOG".into(), "debug".into())];

        let env = container_env(process, &configured, "narsor-local-1");

        let names: Vec<&str> = env.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "TERM",
                "LANG",
                "LC_ALL",
                "TZ",
                "https_proxy",
                "NO_PROXY",
                "RUST_LOG",
                WORKLOAD_MARK_ENV
            ]
        );
        assert_eq!(env.last().unwrap().1, "narsor-local-1");
    }

    #[test]
    fn only_a_loader_failure_reads_as_a_runner_that_cannot_run_in_the_image() {
        let cases = [
            (
                "/opt/narsor/bin/narsor-runner: /lib/x86_64-linux-gnu/libc.so.6: version \
                 `GLIBC_2.39' not found (required by /opt/narsor/bin/narsor-runner)",
                true,
            ),
            (
                "[FATAL tini (7)] exec /opt/narsor/bin/narsor-runner failed: No such file or \
                 directory",
                true,
            ),
            (
                "exec /opt/narsor/bin/narsor-runner: exec format error",
                true,
            ),
            (
                "runner failed: cannot read /x/launch-spec.json: No such file or directory",
                false,
            ),
            (
                "runner problem (secret): 'gitlab-token' is not mounted",
                false,
            ),
        ];
        for (logs, expected) in cases {
            let problem = loader_problem(&format!("starting\n{logs}\n"));
            assert_eq!(problem.is_some(), expected, "{logs}");
            if let Some(problem) = problem {
                assert!(problem.contains("glibc 2.39"), "{problem}");
            }
        }
    }

    /// The Docker work mode against the in-memory Docker.
    mod containers {
        use std::sync::mpsc;

        use super::*;
        use crate::local::docker_fake::{DockerCall, FakeDocker};
        use crate::local::image::{BuildSettings, TaskImageBuilds};
        use narsor_engine::domain::{
            ComponentSecret, HomeSource, ImageDigest, ImageSource, ImageSpec, ResolvedEnvironment,
            ResolvedImage, ResourceProfile, SecretDelivery, TaskImageComponent, TaskRepository,
            TaskWorkspace,
        };

        const USER: UserIds = UserIds {
            uid: 1000,
            gid: 100,
        };

        /// A scratch backend in the Docker mode over a fake Docker, with a
        /// home source, a local workspace, a configuration file and a
        /// token store.
        struct Fixture {
            scratch: Scratch,
            docker: Arc<FakeDocker>,
            version_file: PathBuf,
            /// How long a checked image stays current: zero checks the
            /// component versions at every start.
            update_interval: Duration,
        }

        impl Fixture {
            fn new() -> Self {
                Self::with_update_interval(Duration::ZERO)
            }

            fn with_update_interval(update_interval: Duration) -> Self {
                let scratch = Scratch::new();
                for dir in ["home/.claude/skills", "project"] {
                    fs::create_dir_all(scratch.root.join(dir)).unwrap();
                }
                fs::write(scratch.root.join("config.yaml"), "{}\n").unwrap();
                let version_file = scratch.root.join("version");
                fs::write(&version_file, "1.0\n").unwrap();
                let mut fixture = Self {
                    scratch,
                    docker: Arc::new(FakeDocker::new()),
                    version_file,
                    update_interval,
                };
                fixture.use_backend(true);
                fixture
            }

            fn root(&self) -> &Path {
                &self.scratch.root
            }

            /// The controller's directory, inside a state directory as the
            /// server lays it out.
            fn controller_dir(&self) -> PathBuf {
                self.root().join("state/controller")
            }

            fn store(&self) -> SecretStore {
                SecretStore::beside_credential(&self.controller_dir().join("credential"))
            }

            /// A backend over the same root: in the Docker mode, or in the
            /// process mode with the transport.
            fn backend(&self, docker_mode: bool) -> LocalBackend {
                let mut config = (*self.scratch.backend.config).clone();
                config.docker = Some(self.docker.clone());
                config.container_mode = docker_mode.then(|| ContainerMode {
                    builds: TaskImageBuilds::new(
                        self.docker.clone(),
                        &self.controller_dir(),
                        BuildSettings {
                            update_interval: Some(self.update_interval),
                            version_timeout: Duration::from_secs(5),
                            version_output_cap: 64,
                            user: USER,
                            retry_delay: Duration::from_millis(100),
                        },
                    ),
                    secrets: self.store(),
                    user: USER,
                    host_home: Some(self.root().join("home")),
                    config_file: Some(self.root().join("config.yaml")),
                    runner_args: Vec::new(),
                    start_window: DEFAULT_START_WINDOW,
                });
                LocalBackend::new(config)
            }

            fn use_backend(&mut self, docker_mode: bool) {
                self.scratch.backend = self.backend(docker_mode);
            }

            fn backend_ref(&self) -> &LocalBackend {
                &self.scratch.backend
            }

            fn task(&self, key: &str) -> TaskResources {
                let id = TaskId::parse(key).unwrap();
                self.backend_ref().resources(&id, key).unwrap()
            }

            fn image(&self) -> TaskImage {
                TaskImage {
                    base: ImageSpec::pinned("debian:trixie-slim"),
                    components: vec![
                        TaskImageComponent {
                            name: "claude-code".into(),
                            install: "echo claude\n".into(),
                            version: Some(format!("cat '{}'", self.version_file.display())),
                            secrets: vec![ComponentSecret {
                                name: "claude-token".into(),
                                delivery: SecretDelivery::Env("CLAUDE_CODE_OAUTH_TOKEN".into()),
                            }],
                            home: vec![HomeSource {
                                from: "~/.claude/skills".into(),
                                to: ".claude/skills".into(),
                            }],
                        },
                        TaskImageComponent {
                            name: "glab".into(),
                            install: "echo glab\n".into(),
                            version: None,
                            secrets: Vec::new(),
                            home: Vec::new(),
                        },
                    ],
                    secrets: Vec::new(),
                }
            }

            fn environment(&self) -> ResolvedEnvironment {
                let mut environment = ResolvedEnvironment::harness_only("claude-code");
                environment.task_image = Some(self.image());
                environment
            }

            fn launch(&self, key: &str, environment: ResolvedEnvironment) -> WorkloadLaunch {
                self.launch_from(key, environment, &self.root().join("project"))
            }

            /// A launch whose local workspace source is `project`.
            fn launch_from(
                &self,
                key: &str,
                environment: ResolvedEnvironment,
                project: &Path,
            ) -> WorkloadLaunch {
                let workspace = TaskWorkspace::single(TaskRepository::new(
                    "project",
                    TaskRepositorySource::Local {
                        path: project.display().to_string(),
                    },
                ));
                let spec = TaskLaunchSpec::new(
                    "wf",
                    1,
                    serde_json::json!({}),
                    workspace,
                    environment,
                    ResourceProfile::new(1000, 2048),
                );
                WorkloadLaunch {
                    task_id: TaskId::parse(key).unwrap(),
                    generation: 1,
                    incarnation: format!("{key}-g1"),
                    lease_epoch: 1,
                    launch_spec: serde_json::to_value(spec).unwrap(),
                    control_plane_url: "http://127.0.0.1:9".into(),
                    credential: narsor_controller_runtime::backend::RunnerCredential::new(
                        "nsr.test",
                    ),
                }
            }

            fn default_launch(&self, key: &str) -> WorkloadLaunch {
                self.launch(key, self.environment())
            }

            /// Observes until the workload leaves `Starting`.
            async fn settled(&self, task: &TaskResources) -> Option<WorkloadState> {
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    let observed = self.backend_ref().observe(task).await.unwrap().workload;
                    match &observed {
                        Some(state) if state.phase == WorkloadPhase::Starting => {
                            assert!(Instant::now() < deadline, "still starting");
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        _ => return observed,
                    }
                }
            }

            /// Provisions the task and waits for its running container.
            async fn running(&self, key: &str) -> (TaskResources, RunRequest) {
                let task = self.task(key);
                self.backend_ref()
                    .provision(&task, &self.default_launch(key))
                    .await
                    .unwrap();
                let state = self.settled(&task).await.unwrap();
                assert_eq!(state.phase, WorkloadPhase::Running);
                let run = self
                    .docker
                    .runs()
                    .into_iter()
                    .rev()
                    .find(|run| run.labels[TASK_LABEL] == key)
                    .expect("a container ran");
                (task, run)
            }

            fn task_record(&self, task: &TaskResources) -> Option<TaskRecord> {
                self.backend_ref().read_task_record(task)
            }
        }

        fn arg_after<'a>(run: &'a RunRequest, flag: &str) -> Vec<&'a str> {
            run.args
                .windows(2)
                .filter(|pair| pair[0] == flag)
                .map(|pair| pair[1].as_str())
                .collect()
        }

        /// KTD6: every path the runner is told about is inside a mount.
        fn assert_paths_mounted(run: &RunRequest) {
            let mounted = |path: &Path| {
                run.mounts
                    .iter()
                    .any(|mount| path.starts_with(&mount.target))
            };
            assert!(mounted(Path::new(&run.program)), "the runner binary");
            assert!(mounted(run.workdir.as_deref().unwrap()), "the workdir");
            for arg in run.args.iter().filter(|arg| arg.starts_with('/')) {
                // A home source names its mount before the last '='.
                let path = arg.rsplit_once('=').map_or(arg.as_str(), |(path, _)| path);
                assert!(mounted(Path::new(path)), "{arg} is not mounted");
            }
        }

        #[tokio::test]
        async fn a_container_starts_once_its_image_is_built_with_only_what_the_task_needs() {
            let fixture = Fixture::new();
            fixture.store().set("claude-token", "oauth-secret").unwrap();
            fixture.store().set("db-password", "db-secret").unwrap();
            let mut environment = fixture.environment();
            environment.task_image.as_mut().unwrap().secrets = vec![ComponentSecret {
                name: "db-password".into(),
                delivery: SecretDelivery::Env("DATABASE_PASSWORD".into()),
            }];
            let launch = fixture.launch("task-1", environment);
            let (release, wait) = mpsc::channel::<()>();
            let wait = Mutex::new(wait);
            fixture.docker.on_build(move |_| {
                let _ = wait.lock().unwrap().recv();
            });
            let task = fixture.task("task-1");

            let started = Instant::now();
            let provisioned = fixture
                .backend_ref()
                .provision(&task, &launch)
                .await
                .unwrap();

            assert!(
                started.elapsed() < Duration::from_secs(2),
                "no wait for the build"
            );
            assert_eq!(provisioned.workload.phase, WorkloadPhase::Starting);
            let observed = fixture.backend_ref().observe(&task).await.unwrap();
            assert_eq!(observed.workload.unwrap().phase, WorkloadPhase::Starting);
            assert!(fixture.docker.runs().is_empty());

            release.send(()).unwrap();
            let state = fixture.settled(&task).await.unwrap();
            assert_eq!(state.phase, WorkloadPhase::Running);
            let runs = fixture.docker.runs();
            assert_eq!(runs.len(), 1);
            let run = &runs[0];

            // The image just built, the workload's name and labels, the host
            // user (the hardening flags are fixed by the transport).
            assert_eq!(run.name, state.uid);
            assert_eq!(run.labels[WORKLOAD_LABEL], state.uid);
            assert_eq!(run.labels[MANAGED_LABEL], "true");
            assert_eq!(
                fixture.task_record(&task).unwrap(),
                TaskRecord {
                    kind: LastRunner::Container,
                    image: Some(run.image.clone())
                }
            );
            assert_eq!(run.user, USER);
            assert_eq!(run.program, RUNNER_IN_CONTAINER);

            let backend = fixture.backend_ref();
            let storage = backend.storage_dir(&task);
            let dir = backend.workload_dir(&task, Role::Work);
            let secrets = dir.join("secrets");
            let mounts: Vec<(PathBuf, PathBuf, bool)> = run
                .mounts
                .iter()
                .map(|m| (m.source.clone(), m.target.clone(), m.read_only))
                .collect();
            let same = |path: PathBuf, read_only| (path.clone(), path, read_only);
            assert_eq!(
                mounts,
                [
                    same(storage.clone(), false),
                    same(dir.clone(), true),
                    (
                        backend.config.program.clone(),
                        RUNNER_IN_CONTAINER.into(),
                        true
                    ),
                    same(fixture.root().join("config.yaml"), true),
                    same(secrets.clone(), true),
                    same(fixture.root().join("project"), true),
                    (
                        fixture.root().join("home/.claude/skills"),
                        "/run/narsor/home/claude-code-0".into(),
                        true
                    ),
                ]
            );
            assert_eq!(run.workdir.as_deref(), Some(storage.as_path()));
            assert_eq!(arg_after(run, "--home"), ["private"]);
            assert_eq!(arg_after(run, "--secrets-dir"), [secrets.to_str().unwrap()]);
            assert!(run.args.iter().any(|arg| arg == "--task-image-secrets"));
            assert_eq!(
                arg_after(run, "--home-source"),
                ["/run/narsor/home/claude-code-0=.claude/skills"]
            );
            assert_eq!(
                fs::read_to_string(secrets.join("claude-token")).unwrap(),
                "oauth-secret"
            );
            assert_eq!(
                fs::read_to_string(secrets.join("db-password")).unwrap(),
                "db-secret",
                "an image-level secret is copied like a component's"
            );

            // AE5: no host harness directory, no host HOME, keys or socket.
            for flag in [
                "--codex-home",
                "--claude-config-dir",
                "--claude-profile-source",
                "--read-only",
            ] {
                assert!(!run.args.iter().any(|arg| arg == flag), "{flag}");
            }
            for mount in &run.mounts {
                let source = mount.source.display().to_string();
                assert_ne!(mount.source, fixture.root().join("home"));
                assert!(
                    !source.contains(".ssh") && !source.contains("docker.sock"),
                    "{source}"
                );
            }
            for (name, _) in &run.env {
                assert!(
                    name == WORKLOAD_MARK_ENV || !name.starts_with("NARSOR_"),
                    "{name}"
                );
                for forbidden in [
                    "PATH",
                    "HOME",
                    "USER",
                    "ANTHROPIC_API_KEY",
                    "CODEX_API_KEY",
                    "OPENAI_API_KEY",
                ] {
                    assert_ne!(name, forbidden);
                }
                assert!(!name.starts_with("XDG_"), "{name}");
            }
            assert!(run
                .env
                .iter()
                .any(|(name, value)| name == WORKLOAD_MARK_ENV && *value == state.uid));
            assert!(
                run.env.iter().any(|(name, _)| name == "ARGS_FILE"),
                "configured"
            );
            assert!(!run
                .env
                .iter()
                .any(|(_, value)| value.contains("oauth-secret")));
            assert_paths_mounted(run);
        }

        /// AE2 and the refusals of KTD8, KTD10 and per-step images: each a
        /// readable problem, with nothing created and no host runner. A
        /// daemon that does not answer is the one transient refusal.
        #[tokio::test]
        async fn the_docker_mode_refuses_what_it_cannot_run_without_a_host_fallback() {
            use narsor_controller_runtime::backend::BackendErrorKind::{Rejected, Transient};
            type Setup = fn(&Fixture);
            let healthy: Setup = |fixture| fixture.store().set("claude-token", "t").unwrap();
            let step_image = {
                let digest = ImageDigest::parse(format!("sha256:{}", "a".repeat(64))).unwrap();
                ResolvedImage {
                    reference: "node:22".into(),
                    pinned: format!("node@{digest}"),
                    digest,
                    tag_resolved: false,
                    source: ImageSource::Step {
                        workflow: "wf".into(),
                        state: "review".into(),
                    },
                }
            };
            type Launch = Box<dyn Fn(&Fixture) -> WorkloadLaunch>;
            let home_source = |from: fn(&Fixture) -> PathBuf| -> Launch {
                Box::new(move |fixture| {
                    let mut environment = fixture.environment();
                    let image = environment.task_image.as_mut().unwrap();
                    image.components[0].home.push(HomeSource {
                        from: from(fixture).display().to_string(),
                        to: ".narsor".into(),
                    });
                    fixture.launch("task-1", environment)
                })
            };
            let default: fn() -> Launch = || Box::new(|fixture| fixture.default_launch("task-1"));
            let cases: Vec<(&str, Setup, Launch, &str, _)> = vec![
                (
                    "docker not installed",
                    |fixture| {
                        fixture.docker.set_not_installed();
                        fixture.store().set("claude-token", "t").unwrap();
                    },
                    default(),
                    "docker not found",
                    Rejected,
                ),
                (
                    "rootless docker",
                    |fixture| {
                        fixture.docker.set_rootless();
                        fixture.store().set("claude-token", "t").unwrap();
                    },
                    default(),
                    "rootless",
                    Rejected,
                ),
                (
                    "unreachable daemon",
                    |fixture| {
                        fixture.docker.set_unreachable(true);
                        fixture.store().set("claude-token", "t").unwrap();
                    },
                    default(),
                    "cannot be reached",
                    Transient,
                ),
                (
                    "no sealed task image",
                    healthy,
                    Box::new(|fixture| {
                        fixture.launch("task-1", ResolvedEnvironment::harness_only("claude-code"))
                    }),
                    "runtime.yaml",
                    Rejected,
                ),
                (
                    "a step image",
                    healthy,
                    Box::new(move |fixture| {
                        let mut environment = fixture.environment();
                        environment
                            .step_images
                            .insert("review".into(), step_image.clone());
                        fixture.launch("task-1", environment)
                    }),
                    "step 'review'",
                    Rejected,
                ),
                (
                    "a secret missing from the store",
                    |_| {},
                    default(),
                    "narsor-cli secret set claude-token",
                    Rejected,
                ),
                (
                    "a home source over the controller's token store",
                    healthy,
                    home_source(|fixture| fixture.store().dir().to_path_buf()),
                    "overlaps the controller's own directory",
                    Rejected,
                ),
                (
                    "a home source over the daemon state directory",
                    |fixture| {
                        fixture.store().set("claude-token", "t").unwrap();
                        fs::write(fixture.root().join("state/daemon.json"), "{}").unwrap();
                    },
                    home_source(|fixture| fixture.root().join("state/daemon.json")),
                    "overlaps the controller's own directory",
                    Rejected,
                ),
                (
                    "a local workspace source over the controller's directory",
                    healthy,
                    Box::new(|fixture| {
                        let environment = fixture.environment();
                        fixture.launch_from("task-1", environment, &fixture.root().join("state"))
                    }),
                    "the local workspace source",
                    Rejected,
                ),
            ];
            for (case, setup, launch, expected, kind) in cases {
                let fixture = Fixture::new();
                setup(&fixture);
                let task = fixture.task("task-1");
                let launch = launch(&fixture);

                let refused = fixture
                    .backend_ref()
                    .provision(&task, &launch)
                    .await
                    .expect_err(case);

                assert_eq!(refused.kind, kind, "{case}");
                assert!(refused.message.contains(expected), "{case}: {refused}");
                let backend = fixture.backend_ref();
                assert!(!backend.storage_dir(&task).exists(), "{case}: no storage");
                assert!(!backend.workload_dir(&task, Role::Work).exists(), "{case}");
                assert!(
                    !fixture.root().join("args").exists(),
                    "{case}: no host runner"
                );
                assert!(fixture.docker.runs().is_empty(), "{case}: no container");
            }
        }

        /// AE1 and R3: a task paused in a container, resumed by a
        /// controller restarted in the process mode, runs as a host process
        /// with a warning that names it; the per-task record survived.
        #[tokio::test]
        async fn a_task_that_ran_in_a_container_resumes_as_a_host_process_with_a_warning() {
            let mut fixture = Fixture::new();
            fixture.store().set("claude-token", "t").unwrap();
            let (task, run) = fixture.running("task-1").await;

            let released = fixture.backend_ref().release_compute(&task).await.unwrap();
            assert_eq!(released, Removal::Removed);
            assert!(fixture.docker.container_names().is_empty());
            assert_eq!(
                fixture.task_record(&task).map(|record| record.kind),
                Some(LastRunner::Container),
                "the per-task record survives the release"
            );

            fixture.use_backend(false);
            let logs = Arc::new(Mutex::new(Vec::<u8>::new()));
            let writer = logs.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_writer(move || LogWriter(writer.clone()))
                .with_ansi(false)
                .finish();
            let launch = fixture.default_launch("task-1");
            let resumed = tracing::subscriber::with_default(subscriber, || {
                fixture.backend_ref().create(&task, &launch, false)
            })
            .unwrap();

            assert_eq!(resumed.workload.phase, WorkloadPhase::Running);
            assert!(
                fixture.backend_ref().runner_pid(&task).is_some(),
                "a host runner"
            );
            let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
            assert!(logs.contains("WARN"), "{logs}");
            assert!(
                logs.contains("task-1 last ran in a Docker container"),
                "{logs}"
            );
            assert_eq!(
                fixture.task_record(&task).unwrap(),
                TaskRecord {
                    kind: LastRunner::Process,
                    image: Some(run.image),
                }
            );
        }

        struct LogWriter(Arc<Mutex<Vec<u8>>>);

        impl io::Write for LogWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        /// R11: a resumed task runs on the image it started on, even when
        /// its definition has a newer build.
        #[tokio::test]
        async fn a_resumed_task_runs_on_its_recorded_image_after_a_newer_build() {
            let fixture = Fixture::new();
            fixture.store().set("claude-token", "t").unwrap();
            let (task, first) = fixture.running("task-1").await;
            fixture.backend_ref().release_compute(&task).await.unwrap();

            fs::write(&fixture.version_file, "2.0\n").unwrap();
            let (_, newer) = fixture.running("task-2").await;
            assert_ne!(newer.image, first.image, "a new build");

            fixture
                .backend_ref()
                .resume(&task, &fixture.default_launch("task-1"))
                .await
                .unwrap();
            let state = fixture.settled(&task).await.unwrap();

            assert_eq!(state.phase, WorkloadPhase::Running);
            let resumed = fixture.docker.runs().pop().unwrap();
            assert_eq!(resumed.labels[TASK_LABEL], "task-1");
            assert_eq!(resumed.image, first.image);
            assert!(fixture.docker.image_ids().contains(&first.image));
        }

        /// D58: an unreachable daemon is "cannot tell", never a loss; a
        /// container that is gone, with its storage, is absent; an early
        /// exit with a loader error records why.
        #[tokio::test]
        async fn a_started_container_is_observed_without_guessing() {
            let fixture = Fixture::new();
            fixture.store().set("claude-token", "t").unwrap();
            let (task, run) = fixture.running("task-1").await;
            let backend = fixture.backend_ref();

            fixture.docker.set_unreachable(true);
            let unknown = backend.observe(&task).await.expect_err("cannot tell");
            assert_eq!(
                unknown.kind,
                narsor_controller_runtime::backend::BackendErrorKind::Transient
            );
            fixture.docker.set_unreachable(false);

            fixture
                .docker
                .set_container_state(&run.name, ContainerState::Exited(127));
            fixture.docker.set_container_logs(
                &run.name,
                "/opt/narsor/bin/narsor-runner: /lib/libc.so.6: version `GLIBC_2.39' not found",
            );
            let observed = backend.observe(&task).await.unwrap();
            assert_eq!(
                observed.workload.unwrap().phase,
                WorkloadPhase::Exited { code: Some(127) }
            );
            let record = backend.read_record(&task, Role::Work).unwrap().unwrap();
            let WorkloadKind::Container { container } = record.kind else {
                panic!("a container record");
            };
            assert!(
                container
                    .problem
                    .unwrap()
                    .contains("cannot run in this image"),
                "the loader failure is recorded"
            );

            let (other, other_run) = fixture.running("task-2").await;
            fixture.docker.remove_container(&other_run.name).unwrap();
            let observed = backend.observe(&other).await.unwrap();
            assert_eq!(observed.workload, None);
            assert_eq!(observed.storage, StorageState::Present);
        }

        /// KTD1: the record's kind decides how a workload is removed, not
        /// the mode the controller runs in now.
        #[tokio::test]
        async fn a_workload_is_removed_the_way_its_record_says() {
            let mut fixture = Fixture::new();
            fixture.store().set("claude-token", "t").unwrap();

            // A container left by a Docker-mode controller, removed by a
            // process-mode one.
            let (task, run) = fixture.running("task-1").await;
            fixture.use_backend(false);
            let removed = fixture.backend_ref().release_compute(&task).await.unwrap();
            assert_eq!(removed, Removal::Removed);
            assert!(!fixture.docker.container_names().contains(&run.name));

            // A host runner left by a process-mode controller, removed by a
            // Docker-mode one.
            let other = fixture.task("task-2");
            fixture
                .backend_ref()
                .provision(&other, &fixture.default_launch("task-2"))
                .await
                .unwrap();
            let pid = fixture
                .backend_ref()
                .runner_pid(&other)
                .expect("a host runner");
            fixture.use_backend(true);
            let calls_before = fixture.docker.calls().len();
            let removed = fixture.backend_ref().release_compute(&other).await.unwrap();

            assert_eq!(removed, Removal::Removed);
            assert!(!process_alive(pid, None), "the process group was killed");
            assert!(!fixture.docker.calls()[calls_before..]
                .iter()
                .any(|call| matches!(call, DockerCall::RemoveContainer(_))));
        }

        /// A start refused because the image build already failed leaves no
        /// new task storage behind.
        #[tokio::test]
        async fn a_start_refused_for_a_failed_build_leaves_no_task_storage() {
            let fixture = Fixture::with_update_interval(Duration::from_secs(3600));
            fixture.store().set("claude-token", "t").unwrap();
            fixture.docker.fail_builds(Some("exit code: 1"));
            let first = fixture.task("task-1");
            fixture
                .backend_ref()
                .provision(&first, &fixture.default_launch("task-1"))
                .await
                .unwrap();
            fixture.settled(&first).await.unwrap();
            let task = fixture.task("task-2");

            let refused = fixture
                .backend_ref()
                .provision(&task, &fixture.default_launch("task-2"))
                .await
                .expect_err("the build already failed");

            assert_eq!(
                refused.kind,
                narsor_controller_runtime::backend::BackendErrorKind::Rejected
            );
            assert!(!fixture.backend_ref().storage_dir(&task).exists());
        }

        /// A build that cannot reach Docker is tried again after a short
        /// delay: its task waits as for a build and a new task is not
        /// refused meanwhile.
        #[tokio::test]
        async fn a_task_waits_while_its_image_build_cannot_reach_docker() {
            let fixture = Fixture::with_update_interval(Duration::from_secs(3600));
            fixture.store().set("claude-token", "t").unwrap();
            fixture
                .docker
                .fail_builds_with(Some(DockerError::Unreachable("restarting".into())));
            let task = fixture.task("task-1");
            fixture
                .backend_ref()
                .provision(&task, &fixture.default_launch("task-1"))
                .await
                .unwrap();

            let waited_until = Instant::now() + Duration::from_millis(400);
            while Instant::now() < waited_until {
                let observed = fixture.backend_ref().observe(&task).await.unwrap();
                assert_eq!(observed.workload.unwrap().phase, WorkloadPhase::Starting);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let other = fixture.task("task-2");
            let accepted = fixture
                .backend_ref()
                .provision(&other, &fixture.default_launch("task-2"))
                .await
                .unwrap();
            assert_eq!(accepted.workload.phase, WorkloadPhase::Starting);
            assert!(fixture.docker.builds().len() >= 2, "the build was retried");

            fixture.docker.fail_builds_with(None);
            let state = fixture.settled(&task).await.unwrap();
            assert_eq!(state.phase, WorkloadPhase::Running);
        }

        /// A relative storage root (the server's default layout) still gives
        /// the container absolute mount sources and workdir.
        #[tokio::test]
        async fn a_relative_storage_root_gives_the_container_absolute_paths() {
            let mut fixture = Fixture::new();
            fixture.store().set("claude-token", "t").unwrap();
            let mut config = (*fixture.backend(true).config).clone();
            let up = std::env::current_dir()
                .unwrap()
                .components()
                .skip(1)
                .map(|_| "..")
                .collect::<PathBuf>();
            let relative = up.join(config.root.strip_prefix("/").unwrap());
            assert!(relative.is_relative());
            config.root = LocalBackendConfig::with_program(relative, &config.program).root;
            fixture.scratch.backend = LocalBackend::new(config);

            let (_, run) = fixture.running("task-1").await;

            for mount in &run.mounts {
                assert!(mount.source.is_absolute(), "{}", mount.source.display());
            }
            assert!(run.workdir.unwrap().is_absolute());
        }

        /// KTD12: a failed build with no previous image ends the workload
        /// with the failing component named, and the next start says so.
        #[tokio::test]
        async fn a_failed_build_ends_the_workload_naming_the_component() {
            let fixture = Fixture::with_update_interval(Duration::from_secs(3600));
            fixture.store().set("claude-token", "t").unwrap();
            fixture.docker.fail_builds(Some(
                "process \"/bin/sh -c sh /opt/narsor/components/glab.sh\" did not complete \
                 successfully: exit code: 1",
            ));
            let task = fixture.task("task-1");
            let launch = fixture.default_launch("task-1");
            fixture
                .backend_ref()
                .provision(&task, &launch)
                .await
                .unwrap();

            let state = fixture.settled(&task).await.unwrap();

            assert_eq!(state.phase, WorkloadPhase::Exited { code: None });
            assert!(fixture.docker.runs().is_empty());
            fixture.backend_ref().release_compute(&task).await.unwrap();
            let refused = fixture
                .backend_ref()
                .resume(&task, &launch)
                .await
                .expect_err("the build still fails");
            assert!(refused.message.contains("component 'glab'"), "{refused}");
        }
    }
}
