//! The contract target of the local adapter in the Docker work mode, on the
//! in-memory Docker ([`FakeDocker`]): no Docker daemon is needed.
//!
//! The fake builds task images in memory and, as a started container, runs
//! the container's program — the [`stub_runner`] mounted at the runner's
//! path in the container — as a host process with the container's
//! arguments and only its environment. So every scenario drives the real
//! container path of the adapter (preflight, the background image build,
//! the per-task record, the `docker run` request, observing and removing by
//! container name) with a runner that speaks the protocol. The read-only
//! runner (D63) stays a host process in this mode, as in production.
//!
//! What the fake cannot prove: that the image builds and the runner starts
//! in a real image, the mounts, the user and the hardening of `docker run`.
//! Unit tests check the `docker` argument lists; the rest is checked by
//! hand against a real daemon (see `docs/dev/building-and-testing.md`).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::{stub_runner, ContractTarget};
use narsor_controller::local::backend::{LocalBackend, LocalBackendConfig};
use narsor_controller::local::container::docker::{DockerTransport, UserIds};
use narsor_controller::local::container::docker_fake::FakeDocker;
use narsor_controller::local::container::image::{BuildSettings, TaskImageBuilds};
use narsor_controller::local::container::secrets::SecretStore;
use narsor_controller::local::container::{ContainerMode, DEFAULT_START_WINDOW, TASK_LABEL};
use narsor_controller_runtime::backend::{ExecutionBackend, TaskResources};
use narsor_engine::domain::{
    ImageSpec, ResolvedEnvironment, ResourceProfile, TaskId, TaskImage, TaskLaunchSpec,
    TaskWorkspace,
};

/// The local adapter in the Docker work mode over one shared fake daemon.
pub struct LocalDockerTarget {
    /// The daemon every adapter instance of the target talks to.
    pub docker: Arc<FakeDocker>,
    root: PathBuf,
    program: PathBuf,
    controller_dir: PathBuf,
}

impl LocalDockerTarget {
    /// The name of the task's container, while one exists.
    pub fn container_of(&self, task: &TaskResources) -> Option<String> {
        self.docker
            .list_containers(&format!("{TASK_LABEL}={}", task.task_id))
            .into_iter()
            .next()
    }
}

impl ContractTarget for LocalDockerTarget {
    type Backend = LocalBackend;

    fn setup(scratch: &Path) -> Option<Self> {
        let program = stub_runner::install(&scratch.join("bin"));
        let controller_dir = scratch.join("controller");
        fs::create_dir_all(&controller_dir).expect("the controller directory");
        let docker = Arc::new(FakeDocker::new());
        docker.run_programs();
        Some(Self {
            docker,
            root: scratch.join("local"),
            program,
            controller_dir,
        })
    }

    fn backend(&self) -> LocalBackend {
        // A new instance, with a new build registry over the same cache
        // directory: exactly a restarted controller.
        let user = current_user();
        let transport: Arc<dyn DockerTransport> = self.docker.clone();
        let mut config = LocalBackendConfig::with_program(&self.root, &self.program);
        config.docker = Some(transport.clone());
        config.container_mode = Some(ContainerMode {
            builds: TaskImageBuilds::new(
                transport,
                &self.controller_dir,
                BuildSettings {
                    update_interval: None,
                    version_timeout: Duration::from_secs(5),
                    version_output_cap: 64,
                    user,
                    retry_delay: Duration::from_millis(100),
                },
            ),
            secrets: SecretStore::beside_credential(&self.controller_dir.join("credential")),
            user,
            host_home: None,
            config_file: None,
            runner_args: Vec::new(),
            start_window: DEFAULT_START_WINDOW,
        });
        LocalBackend::new(config)
    }

    fn launch_spec(&self) -> TaskLaunchSpec {
        let definition = super::workflow();
        let mut environment = ResolvedEnvironment::harness_only("claude-code");
        environment.task_image = Some(TaskImage {
            base: ImageSpec::pinned("debian:trixie-slim"),
            components: Vec::new(),
            secrets: Vec::new(),
        });
        TaskLaunchSpec::new(
            definition.id.clone(),
            definition.version,
            serde_json::json!({}),
            TaskWorkspace::none(),
            environment,
            ResourceProfile::new(1000, 2048),
        )
        .with_definition(&definition)
        .expect("snapshot the definition")
    }

    fn kill_workload(&self, _backend: &LocalBackend, task: &TaskResources) {
        // Someone kills the container from outside (`docker kill`).
        let name = self.container_of(task).expect("a container to kill");
        assert!(self.docker.kill_container(&name), "a container to kill");
    }

    fn storage_dir(&self, backend: &LocalBackend, task: &TaskResources) -> Option<PathBuf> {
        Some(backend.storage_dir(task))
    }

    fn kill_reader(&self, backend: &LocalBackend, task: &TaskResources) -> bool {
        let Some(pid) = backend.reader_pid(task) else {
            return false;
        };
        // SAFETY: signalling the reader's own process group.
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
        true
    }

    fn cleanup(&self, backend: &LocalBackend) {
        self.docker.kill_all_processes();
        // Readers are host processes in this mode too.
        let Ok(entries) = fs::read_dir(self.root.join("readers")) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let key = name.strip_suffix("-reader").unwrap_or(&name).to_string();
            let Ok(task_id) = TaskId::parse(&key) else {
                continue;
            };
            let Ok(task) = backend.resources(&task_id, &key) else {
                continue;
            };
            if let Some(pid) = backend.reader_pid(&task) {
                // SAFETY: as above.
                unsafe {
                    libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                }
            }
        }
    }

    fn patience(&self) -> Duration {
        Duration::from_secs(30)
    }
}

/// The user the containers run as: the test's own.
fn current_user() -> UserIds {
    // SAFETY: `getuid` and `getgid` take nothing and cannot fail.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    UserIds { uid, gid }
}
