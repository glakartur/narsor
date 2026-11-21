//! The Docker work mode of the local adapter (D81): working runners in
//! containers on the local, rootful Docker daemon.
//!
//! In this mode ([`ContainerMode`]) `provision` and `resume` check what the
//! container needs, write a container record and ask the image build job
//! for the task image, and return at once; `observe` reports `Starting`
//! while the image builds, then copies the task's secrets into the workload
//! directory and starts the container (KTD5–KTD7, KTD10, KTD12). The launch
//! spec it starts the container from is kept in memory ([`LaunchSpecs`]);
//! after a controller restart the controller fetches it again. The runner
//! fetches the spec itself (R5) with a credential issued when the container
//! starts — an image build can outlast the one issued at creation — which
//! the controller hands over through the same cache. There is no
//! fallback to a host process (R4). The read-only runner (D63) stays a host
//! process in both modes.
//!
//! A container record is observed and removed through Docker whatever mode
//! the controller runs in now ([`LocalBackendConfig::docker`]): a
//! controller restarted in the process mode still removes what a Docker-mode
//! one left. A container needs none of the care a process group needs: its
//! name is the workload's uid, and removing it takes everything in it.
//!
//! - [`docker`]: the `docker` CLI behind the [`DockerTransport`] trait.
//! - [`docker_fake`]: an in-memory Docker for tests.
//! - [`image`]: the task image build jobs and their garbage collection.
//! - [`secrets`]: the controller's token store (KTD10).

pub mod docker;
pub mod docker_fake;
pub mod image;
pub mod secrets;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use narsor_controller_runtime::backend::{
    BackendError, BackendResult, TaskResources, WorkloadLaunch, WorkloadPhase, WorkloadState,
};
use narsor_controller_runtime::LaunchSpecs;
use narsor_model::domain::{SecretDelivery, TaskImage, TaskLaunchSpec, TaskRepositorySource};

use super::backend::LocalBackendConfig;
use super::workload::{
    io_error, task_args, write_credential, ContainerRecord, ExitRecord, LastRunner, Look, Role,
    TaskRecord, WorkloadKind, WorkloadRecord, WorkloadStore, WORKLOAD_MARK_ENV,
};
use docker::{ContainerState, DockerError, DockerTransport, Mount, RunRequest, UserIds};
use image::{definition_hash, BuildState, TaskImageBuilds, DEFINITION_LABEL, MANAGED_LABEL};
use secrets::{SecretError, SecretStore};

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

/// How long after its start a container's failure is checked for a runner
/// that cannot execute in the image.
pub const DEFAULT_START_WINDOW: Duration = Duration::from_secs(15);

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

/// What a container of a task gets from the host, checked before its
/// workload is created and again when the container starts.
struct ContainerInputs {
    /// Local workspace sources, mounted read-only.
    local_sources: Vec<PathBuf>,
    /// The home sources: the host path, the mount point and the target.
    home_sources: Vec<(PathBuf, String, String)>,
    /// The token store entries the task needs.
    secret_names: Vec<String>,
    /// The secrets the task image (itself or its components) names, as
    /// `--deliver-secret` values: the runner knows nothing of task images.
    secret_grants: Vec<String>,
}

/// The runners of the adapter that are containers, and the task images they
/// run on.
#[derive(Debug, Clone)]
pub(super) struct ContainerRunners {
    config: Arc<LocalBackendConfig>,
    store: WorkloadStore,
    /// The launch specs of the workloads created, until their container
    /// starts (shared with the controller).
    specs: LaunchSpecs,
}

impl ContainerRunners {
    /// Containers run as `config` says, recorded in `store`, started from
    /// the launch specs in `specs`.
    pub(super) fn new(
        config: Arc<LocalBackendConfig>,
        store: WorkloadStore,
        specs: LaunchSpecs,
    ) -> Self {
        Self {
            config,
            store,
            specs,
        }
    }

    fn docker(&self) -> BackendResult<&Arc<dyn DockerTransport>> {
        self.config.docker.as_ref().ok_or_else(|| {
            BackendError::transient("this controller has no Docker transport to reach containers")
        })
    }

    /// Removes the task images no per-task record names and no cached
    /// definition has as its latest (KTD12); nothing outside the Docker
    /// mode. A failure is only logged.
    pub(super) fn collect_image_garbage(&self) {
        let Some(mode) = &self.config.container_mode else {
            return;
        };
        match mode.builds.collect_garbage(&self.referenced_images()) {
            Ok(0) => {}
            Ok(removed) => tracing::info!(removed, "local backend: removed unused task images"),
            Err(error) => tracing::warn!(%error, "local backend: cannot collect task images"),
        }
    }

    /// Every image a per-task record names, and every image a container
    /// workload that has not exited was given (it may not have started yet,
    /// so no per-task record names it): what garbage collection keeps.
    fn referenced_images(&self) -> HashSet<String> {
        let mut images: HashSet<String> = self
            .store
            .task_records()
            .filter_map(|record| record.image)
            .collect();
        for record in self.store.work_records() {
            if let (WorkloadKind::Container { container }, None) = (record.kind, record.exited) {
                images.extend(container.image);
            }
        }
        images
    }

    /// Refuses a task the Docker mode cannot run, naming the fix: no sealed
    /// task image, step images, Docker missing, unreachable or rootless, a
    /// host path it cannot mount, a secret missing from the store. Returns
    /// the task image.
    pub(super) fn preflight(
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
            container_inputs(mode, spec, self.store.root()).map_err(BackendError::rejected)?;
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
    pub(super) fn create(
        &self,
        mode: &ContainerMode,
        task: &TaskResources,
        launch: &WorkloadLaunch,
        image: &TaskImage,
    ) -> BackendResult<WorkloadState> {
        let hash = definition_hash(image);
        let docker = self.docker()?;
        let recorded = self
            .store
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
        let dir = self.store.prepare_workload_dir(task, Role::Work, launch)?;
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
        if let Err(error) = self.store.write_record(task, Role::Work, &record) {
            let _ = fs::remove_dir_all(&dir);
            return Err(error);
        }
        self.specs
            .insert(&task.task_id, launch.generation, launch.launch_spec.clone());
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
    pub(super) fn look(
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
            return self.state(task, record, container);
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
        // The spec the workload was created with, kept in memory; after a
        // controller restart it is asked for, and the controller fetches it
        // at its next observation of the task (KTD4). Until then the
        // container waits as for its image.
        let Some(spec) = self
            .specs
            .get_or_request(&task.task_id, record.launch_generation)
        else {
            return Ok(Look::phase(WorkloadPhase::Starting));
        };
        let spec = match serde_json::from_value::<TaskLaunchSpec>(spec) {
            Ok(spec) => spec,
            Err(error) => {
                let problem = format!("the launch spec is malformed: {error}");
                return Ok(ended(task, record, None, problem));
            }
        };
        let dir = self.store.workload_dir(task, role);
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
        self.start(task, mode, record, container, &spec, &dir, image_id)
    }

    /// Prepares the secrets directory and starts the container on
    /// `image_id`; reports its state.
    #[allow(clippy::too_many_arguments)]
    fn start(
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
                let mut look = self.state(task, &started, container)?;
                look.updated.get_or_insert(started);
                return Ok(look);
            }
            Err(error) => return Err(docker_error(&error)),
        }
        // The credential issued at creation may have run out while the image
        // built: the runner starts with a fresh one, issued for the same
        // incarnation at the controller's next observation of the task.
        let Some(credential) = self.specs.take_credential_or_request(
            &task.task_id,
            record.launch_generation,
            &record.incarnation,
        ) else {
            return Ok(Look::phase(WorkloadPhase::Starting));
        };
        write_credential(dir, &credential)
            .map_err(|error| io_error("write the runner credential", error))?;
        let request = match self.run_request(mode, task, record, container, spec, dir, &image_id) {
            Ok(request) => request,
            Err(problem) => return Ok(ended(task, record, None, problem)),
        };
        let task_record = TaskRecord {
            kind: LastRunner::Container,
            image: Some(image_id),
        };
        self.store
            .write_task_record(task, &task_record)
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
        let mut look = self.state(task, &started, container)?;
        look.updated.get_or_insert(started);
        Ok(look)
    }

    /// The phase of a started container, from Docker. An unreachable daemon
    /// is "cannot tell", never a loss.
    fn state(
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
        let inputs = container_inputs(mode, spec, self.store.root())?;
        let storage = self.store.storage_dir(task);
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
            &spec.compute_digest(),
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
            for grant in &inputs.secret_grants {
                args.push("--deliver-secret".into());
                args.push(grant.into());
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

    /// Removes the container of a record, whatever the mode now: the record
    /// says a container.
    pub(super) fn remove(&self, container: &ContainerRecord) -> BackendResult<()> {
        self.docker()?
            .remove_container(&container.name)
            .map_err(|error| docker_error(&error))
    }
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
    let mut secret_grants = Vec::new();
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
        secret_names.insert(secret.name.clone());
        secret_grants.push(match &secret.delivery {
            SecretDelivery::Env(variable) => format!("{}=env:{variable}", secret.name),
            SecretDelivery::File(path) => format!("{}=file:{path}", secret.name),
        });
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
        secret_grants,
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

fn mount(path: &Path, read_only: bool) -> Mount {
    Mount {
        source: path.to_path_buf(),
        target: path.to_path_buf(),
        read_only,
    }
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
