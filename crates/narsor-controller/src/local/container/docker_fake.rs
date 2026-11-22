//! An in-memory [`DockerTransport`] for tests.
//!
//! Production code never uses it. It lives in the library, like
//! `narsor_controller_runtime::testkit`, so that the unit tests of this
//! crate and the integration tests under `tests/` share one implementation.
//!
//! It keeps images and containers in memory, records every call, and can be
//! told to fail: an unavailable or rootless daemon, a daemon that stops
//! answering, failing builds, a hook that runs inside a build and one that
//! runs before every call (to make them slow or to wait on the test). It
//! hands the container events a test emits to whoever follows them, and can
//! end that stream.
//!
//! With [`FakeDocker::run_programs`] a started container also runs its
//! program as a host process, so a test can drive a real (stub) runner
//! through the container lifecycle without a Docker daemon.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use super::docker::{
    BuildRequest, ContainerEvent, ContainerState, DockerError, DockerResult, DockerTransport,
    ImageInfo, RunRequest,
};

/// How often a follower with no event is asked whether it still wants them.
const IDLE_TICK: Duration = Duration::from_millis(20);

/// One call the fake received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockerCall {
    CheckAvailable,
    PullImage(String),
    BuildImage(BuildRequest),
    InspectImage(String),
    ListImages(Vec<String>),
    RemoveImage(String),
    RunContainer(RunRequest),
    InspectContainer(String),
    RemoveContainer(String),
    ContainerLogs(String),
    /// A follow of the events of containers with this label.
    FollowContainerEvents(String),
}

/// What the event stream delivers next.
#[derive(Debug)]
enum FeedItem {
    Event(ContainerEvent),
    End,
}

type BuildHook = Arc<dyn Fn(&BuildRequest) + Send + Sync>;
type CallHook = Arc<dyn Fn(&DockerCall) + Send + Sync>;

/// The in-memory Docker. Cheap to share behind an `Arc`.
#[derive(Default)]
pub struct FakeDocker {
    state: Mutex<FakeState>,
    build_hook: Mutex<Option<BuildHook>>,
    call_hook: Mutex<Option<CallHook>>,
    /// The events and stream ends emitted and not yet delivered.
    feed: Mutex<VecDeque<FeedItem>>,
    fed: Condvar,
    /// How many emitted events the follower has handled (its callback
    /// returned), so a test knows an event was seen before it checks what
    /// the event did not do.
    handled: std::sync::atomic::AtomicUsize,
}

#[derive(Debug, Default)]
struct FakeState {
    /// Image id → labels.
    images: BTreeMap<String, BTreeMap<String, String>>,
    /// Tag → image id.
    tags: BTreeMap<String, String>,
    containers: BTreeMap<String, FakeContainer>,
    calls: Vec<DockerCall>,
    /// The error of the availability check; `None` is a healthy daemon.
    availability: Option<DockerError>,
    /// While set, every call fails as an unreachable daemon.
    unreachable: bool,
    /// While set, builds fail with this error.
    build_failure: Option<DockerError>,
    next_id: u64,
    /// Whether a started container runs its program as a host process.
    run_programs: bool,
    /// The host process of each container that runs one, by name.
    processes: BTreeMap<String, Child>,
}

#[derive(Debug, Clone)]
struct FakeContainer {
    labels: BTreeMap<String, String>,
    state: ContainerState,
    logs: String,
}

const VERSION: &str = "29.0.0-fake";

impl std::fmt::Debug for FakeDocker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeDocker")
            .field("state", &*self.lock())
            .finish_non_exhaustive()
    }
}

impl FakeDocker {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs the call hook, records `call` and fails it when the daemon is
    /// unreachable.
    fn begin(&self, call: DockerCall) -> DockerResult<MutexGuard<'_, FakeState>> {
        let hook = self
            .call_hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(hook) = hook {
            hook(&call);
        }
        let mut state = self.lock();
        state.calls.push(call);
        if state.unreachable {
            return Err(DockerError::Unreachable(
                "Cannot connect to the Docker daemon (fake)".into(),
            ));
        }
        Ok(state)
    }

    // --- scripting ---

    /// The availability check fails as if `docker` were not installed.
    pub fn set_not_installed(&self) {
        self.lock().availability = Some(DockerError::NotInstalled);
    }

    /// The availability check reports a rootless daemon.
    pub fn set_rootless(&self) {
        self.lock().availability = Some(DockerError::Rootless);
    }

    /// Every call fails as an unreachable daemon while `unreachable` is set.
    pub fn set_unreachable(&self, unreachable: bool) {
        self.lock().unreachable = unreachable;
    }

    /// Builds fail with `message` (`None` lets them succeed again).
    pub fn fail_builds(&self, message: Option<&str>) {
        self.fail_builds_with(message.map(|message| DockerError::Failed {
            command: "docker build".into(),
            message: message.to_string(),
        }));
    }

    /// Builds fail with `error` (`None` lets them succeed again).
    pub fn fail_builds_with(&self, error: Option<DockerError>) {
        self.lock().build_failure = error;
    }

    /// Runs `hook` inside every build, outside the fake's lock, before the
    /// image appears: a test can sleep or wait on a channel in it.
    pub fn on_build(&self, hook: impl Fn(&BuildRequest) + Send + Sync + 'static) {
        *self
            .build_hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(hook));
    }

    /// Runs `hook` before every call, outside the fake's lock: a test can
    /// block one kind of call on a channel while others go on.
    pub fn on_call(&self, hook: impl Fn(&DockerCall) + Send + Sync + 'static) {
        *self
            .call_hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(hook));
    }

    /// Adds an image tagged `reference` with `labels`; returns its id.
    pub fn add_image(&self, reference: &str, labels: BTreeMap<String, String>) -> String {
        let mut state = self.lock();
        state.add_image(reference, labels)
    }

    /// Sets the state of an existing container (its process exited, …).
    pub fn set_container_state(&self, name: &str, new_state: ContainerState) {
        if let Some(container) = self.lock().containers.get_mut(name) {
            container.state = new_state;
        }
    }

    /// From now on a started container runs its program as a host process:
    /// the source of the mount whose target is the request's program, with
    /// the request's arguments, only its environment, its working directory
    /// and in a process group of its own. The process ending is the
    /// container exiting with its code (128 + the signal when killed).
    ///
    /// Only same-path mounts work: the process sees the host's paths.
    pub fn run_programs(&self) {
        self.lock().run_programs = true;
    }

    /// Kills the container from outside, as `docker kill`: its process is
    /// killed and the container stays, exited with 137. False when no
    /// container has this name.
    pub fn kill_container(&self, name: &str) -> bool {
        let mut state = self.lock();
        state.stop_process(name);
        match state.containers.get_mut(name) {
            Some(container) => {
                container.state = ContainerState::Exited(137);
                true
            }
            None => false,
        }
    }

    /// Kills the processes of every container, as [`Self::kill_container`].
    pub fn kill_all_processes(&self) {
        let names: Vec<String> = self.lock().processes.keys().cloned().collect();
        for name in names {
            self.kill_container(&name);
        }
    }

    /// Emits a container event to the follower: an event of a container
    /// called `name` with `labels`. Unlike Docker the fake does not filter
    /// the stream by label; the follower sees every event.
    pub fn emit_event(&self, action: &str, name: &str, labels: BTreeMap<String, String>) {
        let mut attributes = labels;
        attributes.insert("name".into(), name.into());
        self.feed_item(FeedItem::Event(ContainerEvent {
            action: action.into(),
            name: name.into(),
            attributes,
        }));
    }

    /// Ends the current event stream, as a restarted daemon would.
    pub fn end_event_stream(&self) {
        self.feed_item(FeedItem::End);
    }

    fn feed_item(&self, item: FeedItem) {
        self.feed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(item);
        self.fed.notify_all();
    }

    /// How many emitted events the follower has handled.
    pub fn events_handled(&self) -> usize {
        self.handled.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// How many times the event stream was followed.
    pub fn event_follows(&self) -> usize {
        self.lock()
            .calls
            .iter()
            .filter(|call| matches!(call, DockerCall::FollowContainerEvents(_)))
            .count()
    }

    /// Sets what an existing container's process wrote.
    pub fn set_container_logs(&self, name: &str, logs: &str) {
        if let Some(container) = self.lock().containers.get_mut(name) {
            container.logs = logs.to_string();
        }
    }

    // --- observation ---

    /// Every call so far, in order.
    pub fn calls(&self) -> Vec<DockerCall> {
        self.lock().calls.clone()
    }

    /// The requests of every `run_container` call, in order.
    pub fn runs(&self) -> Vec<RunRequest> {
        self.lock()
            .calls
            .iter()
            .filter_map(|call| match call {
                DockerCall::RunContainer(request) => Some(request.clone()),
                _ => None,
            })
            .collect()
    }

    /// The requests of every `build_image` call, in order.
    pub fn builds(&self) -> Vec<BuildRequest> {
        self.lock()
            .calls
            .iter()
            .filter_map(|call| match call {
                DockerCall::BuildImage(request) => Some(request.clone()),
                _ => None,
            })
            .collect()
    }

    /// The names of the containers that exist now.
    pub fn container_names(&self) -> Vec<String> {
        self.lock().containers.keys().cloned().collect()
    }

    /// The names of the containers, in any state, that carry `label`
    /// (`key` or `key=value`).
    pub fn list_containers(&self, label: &str) -> Vec<String> {
        self.lock()
            .containers
            .iter()
            .filter(|(_, container)| matches_label(&container.labels, label))
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// The ids of the images that exist now.
    pub fn image_ids(&self) -> Vec<String> {
        self.lock().images.keys().cloned().collect()
    }
}

impl Drop for FakeDocker {
    fn drop(&mut self) {
        self.kill_all_processes();
    }
}

impl FakeState {
    /// Kills the process group of the container's process and reaps it.
    fn stop_process(&mut self, name: &str) {
        if let Some(mut child) = self.processes.remove(name) {
            // SAFETY: signalling the process group this fake created.
            unsafe {
                libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
            }
            let _ = child.wait();
        }
    }

    /// The state of the container's process, when it runs one and it ended.
    fn process_exit(&mut self, name: &str) -> Option<i32> {
        use std::os::unix::process::ExitStatusExt as _;

        let status = self.processes.get_mut(name)?.try_wait().ok()??;
        self.processes.remove(name);
        Some(
            status
                .code()
                .or_else(|| status.signal().map(|signal| 128 + signal))
                .unwrap_or(-1),
        )
    }

    fn fresh_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}{:012}", self.next_id)
    }

    fn add_image(&mut self, reference: &str, labels: BTreeMap<String, String>) -> String {
        let id = self.fresh_id("sha256:fake");
        self.images.insert(id.clone(), labels);
        self.tags.insert(reference.to_string(), id.clone());
        id
    }

    fn resolve_image(&self, reference: &str) -> Option<String> {
        if self.images.contains_key(reference) {
            return Some(reference.to_string());
        }
        self.tags.get(reference).cloned()
    }

    fn image_info(&self, reference: &str) -> Option<ImageInfo> {
        let id = self.resolve_image(reference)?;
        let labels = self.images.get(&id)?.clone();
        Some(ImageInfo { id, labels })
    }
}

fn matches_label(labels: &BTreeMap<String, String>, filter: &str) -> bool {
    match filter.split_once('=') {
        Some((key, value)) => labels.get(key).is_some_and(|actual| actual == value),
        None => labels.contains_key(filter),
    }
}

impl DockerTransport for FakeDocker {
    fn check_available(&self) -> DockerResult<String> {
        let state = self.begin(DockerCall::CheckAvailable)?;
        match &state.availability {
            Some(error) => Err(error.clone()),
            None => Ok(VERSION.to_string()),
        }
    }

    fn pull_image(&self, reference: &str) -> DockerResult<()> {
        let mut state = self.begin(DockerCall::PullImage(reference.to_string()))?;
        if state.resolve_image(reference).is_none() {
            state.add_image(reference, BTreeMap::new());
        }
        Ok(())
    }

    fn build_image(&self, request: &BuildRequest) -> DockerResult<ImageInfo> {
        drop(self.begin(DockerCall::BuildImage(request.clone()))?);
        let hook = self
            .build_hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(hook) = hook {
            hook(request);
        }
        let mut state = self.lock();
        if let Some(error) = &state.build_failure {
            return Err(error.clone());
        }
        let id = state.add_image(&request.tag, request.labels.clone());
        Ok(ImageInfo {
            id,
            labels: request.labels.clone(),
        })
    }

    fn inspect_image(&self, reference: &str) -> DockerResult<Option<ImageInfo>> {
        let state = self.begin(DockerCall::InspectImage(reference.to_string()))?;
        Ok(state.image_info(reference))
    }

    fn list_images(&self, filters: &[String]) -> DockerResult<Vec<String>> {
        let state = self.begin(DockerCall::ListImages(filters.to_vec()))?;
        Ok(state
            .images
            .iter()
            .filter(|(_, labels)| filters.iter().all(|filter| matches_label(labels, filter)))
            .map(|(id, _)| id.clone())
            .collect())
    }

    fn remove_image(&self, reference: &str) -> DockerResult<()> {
        let mut state = self.begin(DockerCall::RemoveImage(reference.to_string()))?;
        if let Some(id) = state.resolve_image(reference) {
            state.images.remove(&id);
            state.tags.retain(|_, tagged| *tagged != id);
        }
        Ok(())
    }

    fn run_container(&self, request: &RunRequest) -> DockerResult<String> {
        let mut state = self.begin(DockerCall::RunContainer(request.clone()))?;
        if state.containers.contains_key(&request.name) {
            return Err(DockerError::Failed {
                command: "docker run".into(),
                message: format!(
                    "Conflict. The container name \"/{}\" is already in use",
                    request.name
                ),
            });
        }
        if state.resolve_image(&request.image).is_none() {
            return Err(DockerError::Missing(format!(
                "No such image: {}",
                request.image
            )));
        }
        if state.run_programs {
            let child = spawn_program(request).map_err(|message| DockerError::Failed {
                command: "docker run".into(),
                message,
            })?;
            state.processes.insert(request.name.clone(), child);
        }
        state.containers.insert(
            request.name.clone(),
            FakeContainer {
                labels: request.labels.clone(),
                state: ContainerState::Running,
                logs: String::new(),
            },
        );
        Ok(state.fresh_id("fakecontainer"))
    }

    fn inspect_container(&self, name: &str) -> DockerResult<ContainerState> {
        let mut state = match self.begin(DockerCall::InspectContainer(name.to_string())) {
            Ok(state) => state,
            Err(DockerError::Unreachable(message)) => {
                return Ok(ContainerState::Unreachable(message))
            }
            Err(error) => return Err(error),
        };
        if let Some(code) = state.process_exit(name) {
            if let Some(container) = state.containers.get_mut(name) {
                container.state = ContainerState::Exited(code);
            }
        }
        Ok(state
            .containers
            .get(name)
            .map_or(ContainerState::Missing, |container| container.state.clone()))
    }

    fn remove_container(&self, name: &str) -> DockerResult<()> {
        let mut state = self.begin(DockerCall::RemoveContainer(name.to_string()))?;
        state.stop_process(name);
        state.containers.remove(name);
        Ok(())
    }

    fn follow_container_events(
        &self,
        label: &str,
        on_event: &mut dyn FnMut(Option<ContainerEvent>) -> bool,
    ) -> DockerResult<()> {
        drop(self.begin(DockerCall::FollowContainerEvents(label.to_string()))?);
        loop {
            let mut feed = self.feed.lock().unwrap_or_else(PoisonError::into_inner);
            if feed.is_empty() {
                feed = self
                    .fed
                    .wait_timeout(feed, IDLE_TICK)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
            let item = feed.pop_front();
            drop(feed);
            let wanted = match item {
                Some(FeedItem::End) => return Ok(()),
                Some(FeedItem::Event(event)) => {
                    let wanted = on_event(Some(event));
                    self.handled
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    wanted
                }
                None => on_event(None),
            };
            if !wanted {
                return Ok(());
            }
        }
    }

    fn container_logs(&self, name: &str, _tail: usize) -> DockerResult<String> {
        let state = self.begin(DockerCall::ContainerLogs(name.to_string()))?;
        state
            .containers
            .get(name)
            .map(|container| container.logs.clone())
            .ok_or_else(|| DockerError::Missing(format!("No such container: {name}")))
    }
}

/// Starts the program of a container as a host process (see
/// [`FakeDocker::run_programs`]).
fn spawn_program(request: &RunRequest) -> Result<Child, String> {
    use std::os::unix::process::CommandExt as _;

    let program = request
        .mounts
        .iter()
        .find(|mount| mount.target == Path::new(&request.program))
        .map(|mount| mount.source.clone())
        .ok_or_else(|| format!("no mount provides the program {}", request.program))?;
    let mut command = Command::new(program);
    command
        .args(&request.args)
        .env_clear()
        .envs(request.env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    if let Some(workdir) = &request.workdir {
        command.current_dir(workdir);
    }
    command
        .spawn()
        .map_err(|error| format!("cannot start {}: {error}", request.program))
}
