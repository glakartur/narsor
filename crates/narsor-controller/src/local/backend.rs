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
//! This module is the task lifecycle every kind of runner shares: storage,
//! workload records, creation, observation, removal and archive. What lives
//! on disk is described in `local::workload`; how a runner runs is the
//! business of `ProcessRunners` (`local::process`) and `ContainerRunners`
//! ([`super::container`]). Runners run in their own
//! process group (or container) and survive the controller, like a Pod
//! survives its controller (R14); removing compute kills the group or
//! removes the container.
//!
//! # Which runner
//!
//! - Creating a working runner follows the *configured* mode: a container
//!   in the Docker work mode ([`LocalBackendConfig::container_mode`]), else
//!   a host process.
//! - Observing and removing a workload follow the kind of its *record*,
//!   never the configured mode: a controller restarted in the other mode
//!   still observes and removes what the previous one left.
//! - The read-only runner (D63) is a host process in both modes.
//!
//! # Waking the controller
//!
//! The controller waits on [`ExecutionBackend::changed`] between steps
//! (R4). The first wait starts what wakes it: a `SIGCHLD` listener for the
//! runners this instance starts, and in the Docker mode a follower of the
//! managed containers' events. Both only wake; the next observation tells
//! what changed.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Once};

use narsor_controller_runtime::backend::{
    ArchiveProgress, BackendCapabilities, BackendError, BackendKind, BackendResult,
    ExecutionBackend, Provisioned, ReaderStart, Removal, StorageState, TaskObservation,
    TaskResources, WakeListener, WakeSignal, WorkloadLaunch, WorkloadState,
};
use narsor_controller_runtime::LaunchSpecs;
use narsor_model::domain::{TaskId, TaskLaunchSpec};

use super::container::docker::DockerTransport;
use super::container::{ContainerMode, ContainerRunners};
use super::process::ProcessRunners;
use super::workload::{io_error, Look, Role, WorkloadKind, WorkloadRecord, WorkloadStore};

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
    /// ([`is_user_credential`](super::process::is_user_credential)):
    /// harness logins, Git and forge credentials. Set for a control plane
    /// beyond this machine with a private HOME (R17).
    pub withhold_user_credentials: bool,
    /// How containers are observed and removed, in either mode: a container
    /// a Docker-mode controller left behind is still removed by a
    /// process-mode one. `None` leaves such a container unobservable.
    pub docker: Option<Arc<dyn DockerTransport>>,
    /// The Docker work mode: working runners run in containers. `None` runs
    /// them as host processes.
    pub container_mode: Option<ContainerMode>,
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
    pub(super) config: Arc<LocalBackendConfig>,
    pub(super) store: WorkloadStore,
    /// Host-process runners: every reader, and the working runners of the
    /// process mode.
    pub(super) process: ProcessRunners,
    /// Container runners: the working runners of the Docker mode.
    containers: ContainerRunners,
    /// The launch specs a container waiting for its image starts from,
    /// shared with the controller (KTD4).
    launch_specs: LaunchSpecs,
    /// What a runner exit or a container event wakes (R4).
    wake: WakeSignal,
    listener: Arc<WakeListener>,
    /// Starts what wakes it, once, at the first wait.
    following: Arc<Once>,
}

impl LocalBackend {
    /// An adapter over `config.root`.
    pub fn new(config: LocalBackendConfig) -> Self {
        let config = Arc::new(config);
        let store = WorkloadStore::new(config.root.clone());
        let launch_specs = LaunchSpecs::default();
        let wake = WakeSignal::new();
        Self {
            listener: Arc::new(wake.listen()),
            wake,
            following: Arc::new(Once::new()),
            process: ProcessRunners::new(config.clone(), store.clone()),
            containers: ContainerRunners::new(config.clone(), store.clone(), launch_specs.clone()),
            launch_specs,
            store,
            config,
        }
    }

    /// The pid of the task's runner while it runs as a host process — what
    /// a test kills to model the host losing the workload.
    pub fn runner_pid(&self, task: &TaskResources) -> Option<u32> {
        self.process.live_pid(task, Role::Work)
    }

    /// The pid of the task's read-only runner while it runs.
    pub fn reader_pid(&self, task: &TaskResources) -> Option<u32> {
        self.process.live_pid(task, Role::Read)
    }

    /// Where the task's storage lives.
    pub fn storage_dir(&self, task: &TaskResources) -> PathBuf {
        self.store.storage_dir(task)
    }

    /// Removes the task images no per-task record names and no cached
    /// definition has as its latest (KTD12); nothing outside the Docker
    /// mode. Run at controller start; a failure is only logged.
    pub fn collect_image_garbage(&self) {
        self.containers.collect_image_garbage();
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
                self.process.look(task, role, record, *pid, *start_ticks)
            }
            WorkloadKind::Container { container } => {
                self.containers.look(task, role, record, container)
            }
        }
    }

    fn observe_workload(
        &self,
        task: &TaskResources,
        role: Role,
    ) -> BackendResult<Option<WorkloadState>> {
        let Some(record) = self.store.read_record(task, role)? else {
            return Ok(None);
        };
        let Look { phase, updated } = self.look(task, role, &record)?;
        let record = match updated {
            Some(mut updated) => {
                updated.resource_generation = record.resource_generation + 1;
                if let Err(error) = self.store.write_record(task, role, &updated) {
                    // A concurrent removal took the workload while what it
                    // learned was being written back: it is gone, not
                    // unobservable.
                    if !self.store.workload_dir(task, role).exists() {
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

    pub(super) fn create(
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
        let container = match &self.config.container_mode {
            Some(mode) => Some((mode, self.containers.preflight(mode, &spec)?)),
            None => None,
        };
        if created_storage {
            fs::create_dir_all(&storage)
                .map_err(|error| io_error("create the task storage", error))?;
        }
        let workload = match container {
            Some((mode, image)) => match self.containers.create(mode, task, launch, &image) {
                Ok(workload) => workload,
                Err(error) => {
                    // A refusal leaves no new task storage behind.
                    if created_storage {
                        let _ = fs::remove_dir_all(&storage);
                    }
                    return Err(error);
                }
            },
            None => self.process.start_working(task, launch, &storage)?,
        };
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
        // A host process in both modes (D63).
        let workload = self.process.spawn(task, Role::Read, launch, &storage)?;
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

    pub(super) fn remove_workload(
        &self,
        task: &TaskResources,
        role: Role,
    ) -> BackendResult<Removal> {
        let dir = self.store.workload_dir(task, role);
        let Some(record) = self.store.read_record(task, role)? else {
            // A staging leftover or an empty directory is not a workload.
            let _ = fs::remove_dir_all(dir.with_extension("staging"));
            return Ok(if dir.exists() {
                fs::remove_dir_all(&dir).map_err(|error| io_error("remove the workload", error))?;
                Removal::Removed
            } else {
                Removal::AlreadyAbsent
            });
        };
        let gone = match &record.kind {
            WorkloadKind::Process { pid, start_ticks } => {
                self.process.stop(task, role, &record, *pid, *start_ticks)?
            }
            WorkloadKind::Container { container } => {
                self.containers.remove(container)?;
                true
            }
        };
        if !gone {
            return Ok(Removal::Initiated);
        }
        // The secrets directory of a container goes with the workload.
        fs::remove_dir_all(&dir).map_err(|error| io_error("remove the workload", error))?;
        Ok(Removal::Removed)
    }

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
        self.store
            .remove_task_record(task)
            .map_err(|error| io_error("delete the task record", error))?;
        Ok(ArchiveProgress { workload, storage })
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

    fn launch_specs(&self) -> Option<LaunchSpecs> {
        Some(self.launch_specs.clone())
    }

    async fn changed(&self) {
        self.following.call_once(|| {
            self.process.follow_exits(self.wake.clone());
            self.containers.follow_events(self.wake.clone());
        });
        self.listener.woken().await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::test_support::Scratch;

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
            !scratch
                .backend
                .store
                .workload_dir(&task, Role::Work)
                .exists(),
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
        let reader_dir = scratch.backend.store.workload_dir(&task, Role::Read);
        assert!(reader_dir.starts_with(scratch.root.join("local/readers")));
        assert_ne!(
            reader_dir,
            scratch.backend.store.workload_dir(&task, Role::Work)
        );

        let launch = scratch.launch("task-1-reader-g1");
        let storage = scratch.backend.storage_dir(&task);
        let args = |role| {
            let dir = scratch.backend.store.workload_dir(&task, role);
            scratch
                .backend
                .process
                .runner_args(role, &launch, &dir, &storage)
                .unwrap()
        };
        assert!(args(Role::Read).iter().any(|arg| arg == "--read-only"));
        assert!(!args(Role::Work).iter().any(|arg| arg == "--read-only"));
    }
}
