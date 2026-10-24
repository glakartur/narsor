//! The backend-neutral contract suite (U4, AE2): one set of runner_state
//! scenarios every execution adapter must pass unchanged.
//!
//! # Plugging an adapter in
//!
//! Implement [`ContractTarget`] for it and expand [`contract_suite!`]:
//!
//! ```ignore
//! mod environment_support;
//! #[macro_use]
//! mod backend_contract_support;
//! use backend_contract_support::ContractTarget;
//!
//! struct MyTarget { /* … */ }
//! impl ContractTarget for MyTarget { /* … */ }
//!
//! contract_suite!(my_adapter, MyTarget);
//! ```
//!
//! # What runs
//!
//! Each scenario serves a real control plane ([`ProtocolService`] with the
//! controller and runner routes) on loopback, starts one or more real
//! [`Controller`]s that reach it over HTTP through [`SeverableLink`]s, and
//! puts the target's adapter behind a [`Probed`] that counts what it really
//! did and injects faults. The owner acts in-process through the same
//! service. Runners are whatever the adapter starts: the fake's simulated
//! runner, or — for the adapters that start processes (the local one, a
//! mock cluster's Pods) — the [`stub_runner`], which speaks the runner
//! protocol with the same simulated runner. No scenario starts the real
//! `narsor-runner`: what is under test is the adapter and the controller.
//! The simulated work runs until the owner moves it to its terminal state
//! ([`Harness::finish`]), so a task is `Running` for as long as a scenario
//! wants and the owner's commands pass the owner's action offer as they
//! would for any running task. Nothing here names a platform type.
//!
//! The resume policy runs as it does in the daemon, on a short tick and with
//! short delays, so a lost workload comes back without the owner.
//! [`Harness::without_auto_resume`] leaves it out, for scenarios where the
//! owner must be the one who decides.

#![allow(dead_code)]

pub mod stub_runner;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use narsor_controller_runtime::testkit::{ProbeCounters, Probed, SeverableLink};
use narsor_controller_runtime::{Controller, ControllerConfig};
use narsor_engine::auth::{ControllerCredential, ControllerKey};
use narsor_engine::backends::{
    BackendErrorKind, ExecutionBackend, TaskObservation, TaskResources, WorkloadPhase,
    WorkloadState,
};
use narsor_engine::controller_protocol::{
    ControllerClient, CreateEnrollmentRequest, OwnerCommandRequest, OwnerCommandResponse,
    ProtocolConfig, ProtocolError, ProtocolService, SubmitOperatorCommandRequest, LOCAL_OWNER,
};
use narsor_engine::domain::{BackendId, TaskId, TaskLaunchSpec};
use narsor_engine::orchestration::{
    CommandActor, NewTask, Observation, OperatorCommand, OperatorCommandStatus, OwnerCommand,
    OwnerRef, ResumePolicy, RunnerState, TaskEventKind, TaskRecord,
};
use narsor_engine::persistence::paths::StatePaths;
use narsor_server::controller_protocol::http::{controller_routes, runner_routes};
use tokio::sync::oneshot;

/// The backend every contract task is assigned to.
pub const BACKEND: &str = "contract";

/// One adapter under contract test.
pub trait ContractTarget: Send + Sync + Sized + 'static {
    /// The adapter.
    type Backend: ExecutionBackend + Clone;

    /// Builds the target with `scratch` as its private directory, or says
    /// why it cannot run here (`None` skips every scenario, noisily).
    fn setup(scratch: &Path) -> Option<Self>;

    /// A new adapter instance on the target's platform: what a freshly
    /// started controller process would hold. State the platform keeps
    /// (workloads, storage) must be visible through every instance.
    fn backend(&self) -> Self::Backend;

    /// A launch spec the target's runners can run, with [`WORKFLOW`] as its
    /// workflow.
    fn launch_spec(&self) -> TaskLaunchSpec;

    /// The platform loses the task's workload unexpectedly.
    fn kill_workload(&self, backend: &Self::Backend, task: &TaskResources);

    /// Where the task's storage lives as a directory the test may read, if
    /// the target has one (the fake keeps none).
    fn storage_dir(&self, _backend: &Self::Backend, _task: &TaskResources) -> Option<PathBuf> {
        None
    }

    /// The platform loses the task's read-only runner unexpectedly. False
    /// when the target cannot (its scenario then fails, so none may not).
    fn kill_reader(&self, _backend: &Self::Backend, _task: &TaskResources) -> bool {
        false
    }

    /// Stops everything the target started (runner processes, mock
    /// servers). Called when a scenario ends, pass or fail.
    fn cleanup(&self, _backend: &Self::Backend) {}

    /// How long one runner_state transition may take on this target.
    fn patience(&self) -> Duration {
        Duration::from_secs(30)
    }
}

/// The workflow of every contract task: one state whose way out leads to
/// the terminal one. The simulated runner stands in `work` until the owner
/// moves it on ([`Harness::finish`]).
pub const WORKFLOW: &str = r#"
name: contract-task
version: 1
states:
  work:
    action:
      type: script
      command: sh
      args: [-c, "printf '{\"choice\": \"done\"}'"]
    choices: [done]
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states: [completed]
"#;

/// [`WORKFLOW`], parsed.
pub fn workflow() -> narsor_engine::domain::WorkflowDefinition {
    narsor_engine::workflow::load_from_str(WORKFLOW).expect("a valid workflow")
}

/// How often the harness runs the resume policy (the daemon: every 5 s).
const POLICY_TICK: Duration = Duration::from_millis(50);

/// The resume policy of every scenario: the daemon's shape, in milliseconds.
fn test_resume_policy() -> ResumePolicy {
    ResumePolicy {
        attempt_delays: [0, 100, 200, 300, 400]
            .into_iter()
            .map(chrono::Duration::milliseconds)
            .collect(),
        stability_window: chrono::Duration::minutes(10),
    }
}

/// A directory that deletes itself.
struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-contract-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self(path)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// One running controller.
pub struct ControllerHandle {
    /// Its connection to the control plane, severable.
    pub link: SeverableLink<ControllerClient>,
    task: tokio::task::JoinHandle<()>,
    shutdown: Option<oneshot::Sender<()>>,
}

/// A served control plane, the target's adapter behind a probe, and the
/// controllers driving it.
pub struct Harness<T: ContractTarget> {
    /// The adapter under test.
    pub target: T,
    /// Counters and faults, shared by every controller's adapter instance.
    pub probe: Probed<T::Backend>,
    /// The control plane's protocol service.
    pub service: Arc<ProtocolService>,
    /// Base URL of the served control plane.
    pub url: String,
    controllers: Vec<ControllerHandle>,
    server: tokio::task::JoinHandle<()>,
    policy: Option<tokio::task::JoinHandle<()>>,
    root: TempRoot,
}

impl<T: ContractTarget> Harness<T> {
    /// Serves a control plane with its resume policy running and sets the
    /// target up; `None` skips.
    pub async fn new() -> Option<Self> {
        Self::serve(true).await
    }

    /// [`Self::new`] without the resume policy: a lost workload waits for
    /// the owner.
    pub async fn without_auto_resume() -> Option<Self> {
        Self::serve(false).await
    }

    async fn serve(auto_resume: bool) -> Option<Self> {
        let root = TempRoot::new();
        let Some(target) = T::setup(&root.0.join("target")) else {
            eprintln!("skipped: the contract target cannot run here");
            return None;
        };
        let state = root.0.join("control");
        fs::create_dir_all(&state).expect("state root");
        let launch = crate::environment_support::seeded_resolver(&state);
        // The scheduler admits a task only on a registered backend, with
        // its limit read from the catalogue (U10).
        narsor_engine::environment::EnvironmentCatalog::new(&state)
            .save_backend(&crate::environment_support::backend(BACKEND))
            .expect("register the contract backend");
        let service = Arc::new(ProtocolService::with_config(
            narsor_engine::orchestration::ControlPlane::new(StatePaths::new(&state)),
            launch,
            ProtocolConfig {
                resume: test_resume_policy(),
                // A finished task keeps its runner here (AE9's scenario
                // archives it), so the policy that would stop it is off; it
                // has its own tests. Reading a stopped task is the reader's
                // contract (U10).
                slot_release: false,
                ..ProtocolConfig::default()
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the control plane");
        let url = format!("http://{}", listener.local_addr().expect("address"));
        let router: axum::Router =
            controller_routes(Arc::clone(&service)).merge(runner_routes(Arc::clone(&service)));
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.expect("serve");
        });
        let policy = auto_resume.then(|| spawn_policy(Arc::clone(&service)));
        let probe = Probed::new(target.backend());
        Some(Self {
            target,
            probe,
            service,
            url,
            controllers: Vec::new(),
            server,
            policy,
            root,
        })
    }

    // -- controllers --------------------------------------------------------

    /// Enrolls a new controller identity for the backend with a fresh key;
    /// its persistable credential.
    pub fn enroll(&self) -> String {
        let code = self
            .service
            .create_enrollment(BACKEND, CreateEnrollmentRequest::default())
            .expect("enrollment code");
        let key = ControllerKey::generate();
        let enrolled = self
            .service
            .enroll(narsor_engine::auth::pop::signed_enrollment_request(
                &code.enrollment_code,
                &key,
            ))
            .expect("enroll");
        ControllerCredential::new(enrolled.controller_id, key).expose()
    }

    /// Starts a controller with a new identity and the default lease.
    pub fn start_controller(&mut self) -> usize {
        let credential = self.enroll();
        self.start_controller_as(&credential, Duration::from_secs(30))
    }

    /// Starts a controller presenting `credential`, on a fresh adapter
    /// instance, with a lease valid for `lease_ttl`.
    pub fn start_controller_as(&mut self, credential: &str, lease_ttl: Duration) -> usize {
        let link = SeverableLink::new(ControllerClient::new(&self.url, credential));
        let backend = self.probe.sharing(self.target.backend());
        let mut config = ControllerConfig::new(&self.url);
        config.lease_ttl = lease_ttl;
        config.poll_wait = Duration::from_secs(1);
        config.settle_interval = Duration::from_millis(50);
        config.retry_interval = Duration::from_millis(100);
        config.heartbeat_interval = Duration::from_secs(1);
        config.observe_failures_before_unknown = 2;
        config.reader_restart_interval = Duration::from_millis(200);
        let controller = Controller::new(backend, link.clone(), config);
        let (shutdown, stop) = oneshot::channel();
        let task = tokio::spawn(controller.run(async {
            let _ = stop.await;
        }));
        self.controllers.push(ControllerHandle {
            link,
            task,
            shutdown: Some(shutdown),
        });
        self.controllers.len() - 1
    }

    /// Kills controller `index` without a goodbye: its lease lapses.
    pub fn crash_controller(&mut self, index: usize) {
        self.controllers[index].task.abort();
        self.controllers[index].shutdown = None;
    }

    /// Cuts controller `index` off the control plane.
    pub fn sever(&self, index: usize) {
        self.controllers[index].link.sever();
    }

    /// Reconnects controller `index`.
    pub fn restore(&self, index: usize) {
        self.controllers[index].link.restore();
    }

    /// Changes the backend's `max_concurrent_tasks` in the catalogue; the
    /// scheduler reads it fresh at its next decision (U10).
    pub fn set_limit(&self, limit: u32) {
        let mut record = crate::environment_support::backend(BACKEND);
        record.limits.max_concurrent_tasks = limit;
        narsor_engine::environment::EnvironmentCatalog::new(self.root.0.join("control"))
            .save_backend(&record)
            .expect("save the backend");
    }

    /// The controller currently holding the backend's lease, if any.
    pub fn lease_holder(&self) -> Option<(String, u64)> {
        self.service
            .control_plane()
            .lease(&BackendId::parse(BACKEND).unwrap())
            .expect("lease")
            .map(|lease| (lease.controller_id, lease.epoch))
    }

    // -- owner -------------------------------------------------------------

    /// Creates and queues a task, announcing it the way every in-process
    /// writer must.
    pub fn create(&self, task: &str) {
        self.create_with(task, self.target.launch_spec());
    }

    /// [`Self::create`] with a launch spec of the scenario's own.
    pub fn create_with(&self, task: &str, spec: TaskLaunchSpec) {
        let task_id = TaskId::parse(task).unwrap();
        self.service
            .control_plane()
            .create_task(NewTask::new(
                task_id.clone(),
                OwnerRef::new(LOCAL_OWNER),
                BackendId::parse(BACKEND).unwrap(),
                spec,
            ))
            .expect("create the task");
        self.service.notify(&task_id);
    }

    /// An owner command, as the owner's API would issue it.
    pub fn command(
        &self,
        task: &str,
        command: OwnerCommand,
        key: &str,
    ) -> Result<OwnerCommandResponse, ProtocolError> {
        self.service.command(
            &OwnerRef::new(LOCAL_OWNER),
            task,
            OwnerCommandRequest {
                command,
                idempotency_key: key.to_string(),
                expected_generation: None,
            },
        )
    }

    /// An owner command that must be accepted.
    pub fn must(&self, task: &str, command: OwnerCommand, key: &str) {
        if let Err(error) = self.command(task, command, key) {
            panic!("{command:?} of {task}: {error}");
        }
    }

    /// The owner moves the task's work to the terminal state of
    /// [`WORKFLOW`], through the owner's offer, and waits until the runner
    /// reports it completed.
    pub async fn finish(&self, task: &str) {
        self.service
            .submit_operator_command(
                &OwnerRef::new(LOCAL_OWNER),
                task,
                SubmitOperatorCommandRequest {
                    command: OperatorCommand::Transition {
                        target: "completed".to_string(),
                        forced: false,
                        reason: None,
                    },
                    idempotency_key: format!("finish-{task}"),
                    expected_generation: None,
                },
            )
            .unwrap_or_else(|error| panic!("finish {task}: {error}"));
        self.wait_for("the workflow to finish", |h| {
            h.record(task).runner.is_some_and(|runner| {
                runner.snapshot.execution_status
                    == narsor_engine::domain::ExecutionStatus::Completed
            })
        })
        .await;
    }

    /// An owner starts reading the task: leases a reader for `idle_secs`
    /// and wakes the controller, as a read of a stopped task does (D63).
    pub fn lease_reader(&self, task: &str, idle_secs: i64) {
        let task_id = TaskId::parse(task).unwrap();
        let leased = self
            .service
            .control_plane()
            .lease_reader(&task_id, chrono::Duration::seconds(idle_secs))
            .expect("lease a reader");
        assert!(leased.is_some(), "{task} is not in a state a reader reads");
        self.service.notify(&task_id);
    }

    /// The task's read-only runner as the platform shows it, uncounted.
    pub async fn reader(&self, task: &str) -> Option<WorkloadState> {
        self.probe
            .inner()
            .observe_reader(&self.resources(task))
            .await
            .expect("observe the reader")
    }

    /// Waits for the task's reader to satisfy `check` (`None`: no reader).
    pub async fn wait_reader(
        &self,
        task: &str,
        what: &str,
        check: impl Fn(Option<&WorkloadState>) -> bool,
    ) {
        let deadline = Instant::now() + self.target.patience();
        loop {
            let reader = self.reader(task).await;
            if check(reader.as_ref()) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{task}: timed out waiting for {what}; the reader is {reader:?}; counters {:?}",
                self.counters()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Waits for a running reader and returns it.
    pub async fn running_reader(&self, task: &str) -> WorkloadState {
        self.wait_reader(task, "a running reader", |reader| {
            reader.is_some_and(|r| r.phase == WorkloadPhase::Running)
        })
        .await;
        self.reader(task).await.expect("the reader")
    }

    /// A digest of everything under the task's storage — paths, sizes,
    /// modification times and contents — if the target shows it.
    pub fn storage_digest(&self, task: &str) -> Option<String> {
        let dir = self
            .target
            .storage_dir(self.probe.inner(), &self.resources(task))?;
        let mut lines = Vec::new();
        digest_tree(&dir, &dir, &mut lines);
        lines.sort();
        Some(lines.join("\n"))
    }

    /// Runs the task and pauses it: it is `Stopped`, with its storage kept.
    pub async fn stopped(&self, task: &str) {
        self.running(task).await;
        self.must(task, OwnerCommand::Pause, "pause-for-reading");
        self.wait_runner_state(task, RunnerState::Stopped).await;
    }

    // -- observation --------------------------------------------------------

    /// The task's control-plane record.
    pub fn record(&self, task: &str) -> TaskRecord {
        self.service
            .control_plane()
            .task(&TaskId::parse(task).unwrap())
            .expect("task record")
    }

    /// The task's runner_state.
    pub fn runner_state(&self, task: &str) -> RunnerState {
        self.record(task).runner_state
    }

    /// The task's resource names.
    pub fn resources(&self, task: &str) -> TaskResources {
        self.probe
            .resources(&TaskId::parse(task).unwrap(), task)
            .expect("resource names")
    }

    /// What the platform holds for the task, without counting it.
    pub async fn observe(&self, task: &str) -> TaskObservation {
        self.probe
            .inner()
            .observe(&self.resources(task))
            .await
            .expect("observe the platform")
    }

    /// What the probe counted.
    pub fn counters(&self) -> ProbeCounters {
        self.probe.counters()
    }

    /// The platform loses the task's workload.
    pub fn kill_workload(&self, task: &str) {
        self.target
            .kill_workload(self.probe.inner(), &self.resources(task));
    }

    /// Observations the control plane accepted for the task, in order.
    pub fn reported(&self, task: &str) -> Vec<Observation> {
        self.service
            .control_plane()
            .events_since(&TaskId::parse(task).unwrap(), 0)
            .expect("events")
            .into_iter()
            .filter_map(|event| match event.kind {
                TaskEventKind::ControllerReported { observation, .. } => Some(observation),
                _ => None,
            })
            .collect()
    }

    /// Every event kind in the task's control-plane log.
    pub fn events(&self, task: &str) -> Vec<TaskEventKind> {
        self.service
            .control_plane()
            .events_since(&TaskId::parse(task).unwrap(), 0)
            .expect("events")
            .into_iter()
            .map(|event| event.kind)
            .collect()
    }

    /// Waits until `check` holds, failing with `what` after the target's
    /// patience.
    pub async fn wait_for(&self, what: &str, mut check: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + self.target.patience();
        while !check(self) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; counters {:?}",
                self.counters()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Waits for the task to reach `runner_state`.
    pub async fn wait_runner_state(&self, task: &str, runner_state: RunnerState) {
        let deadline = Instant::now() + self.target.patience();
        loop {
            let record = self.record(task);
            if record.runner_state == runner_state {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{task} did not reach {runner_state:?}: it is {:?} (observed {:?}, runner {:?}); counters {:?}",
                record.runner_state,
                record.observed,
                record.runner.as_ref().map(|r| (&r.incarnation, r.safe_point_generation)),
                self.counters()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Waits for the platform's view of the task to satisfy `check`.
    pub async fn wait_platform(
        &self,
        task: &str,
        what: &str,
        check: impl Fn(&TaskObservation) -> bool,
    ) {
        let deadline = Instant::now() + self.target.patience();
        loop {
            let observed = self.observe(task).await;
            if check(&observed) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{task}: timed out waiting for {what}; platform shows {observed:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Waits until the platform was observed `count` more times: as many
    /// controller passes over a task that observes on each, at the idle
    /// pace.
    pub async fn observed_again(&self, count: u64) {
        let target = self.counters().observe_calls + count;
        self.wait_for("more observations", |h| {
            h.counters().observe_calls >= target
        })
        .await;
    }

    /// Lets controllers run a few rounds, for "nothing happens" checks.
    pub async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(600)).await;
    }

    /// Creates a task and waits until it runs.
    pub async fn running(&self, task: &str) {
        self.create(task);
        self.wait_runner_state(task, RunnerState::Running).await;
    }
}

impl<T: ContractTarget> Drop for Harness<T> {
    fn drop(&mut self) {
        for controller in &mut self.controllers {
            controller.task.abort();
            if let Some(shutdown) = controller.shutdown.take() {
                let _ = shutdown.send(());
            }
        }
        if let Some(policy) = self.policy.take() {
            policy.abort();
        }
        self.target.cleanup(self.probe.inner());
        self.server.abort();
        let _ = &self.root;
    }
}

/// Runs the resume policy on a tick, the way the daemon's scheduler does.
fn spawn_policy(service: Arc<ProtocolService>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(POLICY_TICK).await;
            let service = Arc::clone(&service);
            let _ = tokio::task::spawn_blocking(move || {
                service.auto_resume_pass();
                service.reader_lease_pass();
            })
            .await;
        }
    })
}

/// One line per file under `dir`: path, length, modification time (to the
/// nanosecond) and contents.
fn digest_tree(root: &Path, dir: &Path, lines: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap().display().to_string();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            lines.push(format!("{relative}/"));
            digest_tree(root, &path, lines);
        } else {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |since| since.as_nanos());
            let contents = fs::read(&path).unwrap_or_default();
            lines.push(format!(
                "{relative} {} {modified} {}",
                metadata.len(),
                String::from_utf8_lossy(&contents)
            ));
        }
    }
}

/// The automatic resumes in a task's log: `(attempt, limit)`.
pub fn auto_resumes(events: &[TaskEventKind]) -> Vec<(u32, u32)> {
    events
        .iter()
        .filter_map(|kind| match kind {
            TaskEventKind::AutoResumed { attempt, limit, .. } => Some((*attempt, *limit)),
            _ => None,
        })
        .collect()
}

fn alive(observed: &TaskObservation) -> bool {
    observed
        .workload
        .as_ref()
        .is_some_and(|workload| workload.phase.is_alive())
}

fn incarnation(record: &TaskRecord) -> Option<String> {
    record
        .runner
        .as_ref()
        .map(|runner| runner.incarnation.clone())
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// Start → pause → resume → archive: one storage for the whole life, one
/// workload per launch, compute removed only at the acknowledged safe
/// point, everything removed by archive (R7, R9, R12, R34, R42).
pub async fn lifecycle_round_trip<T: ContractTarget>() {
    let Some(mut h) = Harness::<T>::new().await else {
        return;
    };
    h.start_controller();
    h.running("task-1").await;
    assert!(alive(&h.observe("task-1").await));
    let first = h.record("task-1");
    let first_incarnation = incarnation(&first).expect("the runner reported");
    let cursor_before = first.runner.as_ref().unwrap().snapshot.event_cursor;

    h.must("task-1", OwnerCommand::Pause, "pause-1");
    h.wait_runner_state("task-1", RunnerState::Stopped).await;
    let paused = h.record("task-1");
    assert_eq!(
        paused.runner.as_ref().unwrap().safe_point_generation,
        Some(paused.generation),
        "the pause completed on the runner's acknowledgement"
    );
    let observed = h.observe("task-1").await;
    assert!(observed.workload.is_none(), "compute removed: {observed:?}");
    assert!(observed.storage.exists(), "storage kept through the pause");

    h.must("task-1", OwnerCommand::Resume, "resume-1");
    h.wait_runner_state("task-1", RunnerState::Running).await;
    let resumed = h.record("task-1");
    let runner = resumed.runner.as_ref().unwrap();
    assert_ne!(runner.incarnation, first_incarnation, "a new incarnation");
    assert_eq!(runner.launch_generation, resumed.generation);
    assert!(
        runner.snapshot.event_cursor >= cursor_before,
        "the resumed runner continues the same storage's work"
    );

    h.must("task-1", OwnerCommand::Archive, "archive-1");
    h.wait_runner_state("task-1", RunnerState::Archived).await;
    let observed = h.observe("task-1").await;
    assert!(
        observed.workload.is_none() && !observed.storage.exists(),
        "{observed:?}"
    );

    let counters = h.counters();
    assert_eq!(counters.workloads_created, 2, "{counters:?}");
    assert_eq!(counters.storages_created, 1, "{counters:?}");
    assert_eq!(counters.storage_deletions, 1, "{counters:?}");
    assert_eq!(counters.launches_over_existing, 0, "{counters:?}");
}

/// AE8: a controller cut off from the control plane holds still — no task
/// is declared lost because *it* lost contact — and after reconnecting, or
/// after a restart, it finds the existing workload by identity instead of
/// creating a second one (R14).
pub async fn reconnect_reconciles_instead_of_duplicating<T: ContractTarget>() {
    let Some(mut h) = Harness::<T>::new().await else {
        return;
    };
    let credential = h.enroll();
    let first = h.start_controller_as(&credential, Duration::from_secs(30));
    h.running("task-1").await;

    h.sever(first);
    h.settle().await;
    let before = h.counters();
    h.settle().await;
    assert_eq!(h.runner_state("task-1"), RunnerState::Running);
    assert!(
        alive(&h.observe("task-1").await),
        "the workload keeps running"
    );
    assert_eq!(
        h.counters().observe_calls,
        before.observe_calls,
        "a severed controller does not act"
    );

    h.restore(first);
    h.settle().await;
    assert_eq!(h.runner_state("task-1"), RunnerState::Running);

    // A crash and a restart with the same identity: a new epoch, a full
    // resync, and still the one workload.
    let epoch = h.lease_holder().expect("lease").1;
    h.crash_controller(first);
    h.start_controller_as(&credential, Duration::from_secs(30));
    h.wait_for("the restarted controller's lease", |h| {
        h.lease_holder().is_some_and(|(_, e)| e > epoch)
    })
    .await;
    h.settle().await;
    assert_eq!(h.runner_state("task-1"), RunnerState::Running);
    assert!(alive(&h.observe("task-1").await));
    let counters = h.counters();
    assert_eq!(counters.workloads_created, 1, "{counters:?}");
    assert_eq!(counters.workload_deletions, 0, "{counters:?}");
    assert!(
        !h.reported("task-1")
            .iter()
            .any(|o| matches!(o, Observation::WorkloadLost { .. })),
        "connection loss is not workload loss"
    );

    // The restarted controller still drives the task.
    h.must("task-1", OwnerCommand::Pause, "pause-1");
    h.wait_runner_state("task-1", RunnerState::Stopped).await;
}

/// A create whose answer was lost is found by identity on the next round,
/// not repeated (R7).
pub async fn a_lost_launch_answer_is_reconciled_not_duplicated<T: ContractTarget>() {
    let Some(mut h) = Harness::<T>::new().await else {
        return;
    };
    h.probe.lose_launch_answers(1);
    h.start_controller();
    h.running("task-1").await;
    let counters = h.counters();
    assert_eq!(counters.workloads_created, 1, "{counters:?}");
    assert_eq!(counters.provision_calls, 1, "{counters:?}");
    assert_eq!(counters.launches_over_existing, 0, "{counters:?}");
}

/// AE4: a workload that dies is reported Interrupted and the resume policy
/// launches it again on the same storage, without the owner; the dead one
/// is cleared — even when its deletion is slow — before the replacement is
/// created (R1, R7, R13).
pub async fn a_dead_workload_is_resumed_automatically<T: ContractTarget>() {
    let Some(mut h) = Harness::<T>::new().await else {
        return;
    };
    h.start_controller();
    h.running("task-1").await;

    h.probe.delay_deletions(8);
    h.kill_workload("task-1");
    h.wait_for("the replacement workload", |h| {
        h.counters().workloads_created == 2 && h.runner_state("task-1") == RunnerState::Running
    })
    .await;
    let counters = h.counters();
    assert_eq!(
        counters.workload_deletions, 1,
        "the dead workload went first"
    );
    assert_eq!(counters.launches_over_existing, 0, "{counters:?}");
    assert_eq!(counters.storages_created, 1, "the same storage");
    assert!(
        h.reported("task-1")
            .contains(&Observation::WorkloadLost { state_known: true }),
        "the loss itself is known"
    );
    let events = h.events("task-1");
    assert_eq!(auto_resumes(&events), vec![(1, 5)], "{events:?}");
    let record = h.record("task-1");
    let resumes: Vec<_> = record
        .commands
        .iter()
        .filter(|receipt| receipt.command == OwnerCommand::Resume)
        .collect();
    assert_eq!(resumes.len(), 1, "{:?}", record.commands);
    assert_eq!(resumes[0].actor, CommandActor::Policy);
    assert_eq!(
        record.runner.map(|runner| runner.launch_generation),
        Some(record.generation),
        "the runner of the automatic resume reports"
    );
}

/// Without the resume policy a dead workload waits for the owner, who
/// resumes it on the same storage (R13).
pub async fn workload_death_waits_for_the_owner_without_the_policy<T: ContractTarget>() {
    let Some(mut h) = Harness::<T>::without_auto_resume().await else {
        return;
    };
    h.start_controller();
    h.running("task-1").await;

    h.kill_workload("task-1");
    h.wait_runner_state("task-1", RunnerState::Interrupted)
        .await;
    h.settle().await;
    assert_eq!(h.runner_state("task-1"), RunnerState::Interrupted);
    assert_eq!(h.counters().workloads_created, 1, "nothing replaces it");

    h.must("task-1", OwnerCommand::Resume, "resume-1");
    h.wait_runner_state("task-1", RunnerState::Running).await;
    let counters = h.counters();
    assert_eq!(counters.workloads_created, 2, "{counters:?}");
    assert_eq!(counters.launches_over_existing, 0, "{counters:?}");
    assert_eq!(counters.storages_created, 1, "the same storage");
}

/// A backend that cannot be observed makes the task Unknown, and while its
/// workload is seen alive again nothing replaces it — not the resume policy
/// either, which acts only on a confirmed loss — until the owner decides
/// (R3, R13, AE4).
pub async fn a_failed_observation_reports_unknown_and_waits<T: ContractTarget>() {
    let Some(mut h) = Harness::<T>::new().await else {
        return;
    };
    h.start_controller();
    h.running("task-1").await;

    h.probe.fail_observes(2);
    h.wait_runner_state("task-1", RunnerState::Unknown).await;
    assert!(h
        .reported("task-1")
        .contains(&Observation::WorkloadLost { state_known: false }));
    h.observed_again(3).await;
    assert_eq!(
        h.runner_state("task-1"),
        RunnerState::Unknown,
        "a live workload is no confirmed loss"
    );
    assert_eq!(h.counters().workloads_created, 1);
    assert!(!h.reported("task-1").contains(&Observation::LossConfirmed));
    assert!(auto_resumes(&h.events("task-1")).is_empty());

    // The owner retries: the old launch goes, one new one comes.
    h.must("task-1", OwnerCommand::Start, "start-1");
    h.wait_runner_state("task-1", RunnerState::Running).await;
    let counters = h.counters();
    assert_eq!(counters.workloads_created, 2, "{counters:?}");
    assert_eq!(counters.workload_deletions, 1, "{counters:?}");
    assert_eq!(counters.launches_over_existing, 0, "{counters:?}");
}

/// An Unknown task whose workload is then seen dead, with its storage
/// still there, is a confirmed loss: Interrupted, and the resume policy
/// launches it again on the same storage. While the backend still cannot
/// be observed nothing is concluded and nothing reported again (R1, R3).
pub async fn a_confirmed_loss_of_an_unknown_task_is_resumed<T: ContractTarget>() {
    let Some(mut h) = Harness::<T>::new().await else {
        return;
    };
    h.start_controller();
    h.running("task-1").await;

    h.probe.fail_observes(2);
    h.wait_runner_state("task-1", RunnerState::Unknown).await;
    // The runner dies while the backend still cannot be observed.
    h.probe.fail_observes(u32::MAX / 2);
    h.kill_workload("task-1");
    h.observed_again(2).await;
    assert_eq!(h.runner_state("task-1"), RunnerState::Unknown);
    let losses = |h: &Harness<T>| {
        h.reported("task-1")
            .into_iter()
            .filter(|o| matches!(o, Observation::WorkloadLost { .. }))
            .count()
    };
    assert_eq!(losses(&h), 1, "a failed observation reports nothing more");

    h.probe.clear_faults();
    h.wait_for("the replacement workload", |h| {
        h.counters().workloads_created == 2 && h.runner_state("task-1") == RunnerState::Running
    })
    .await;
    let reported = h.reported("task-1");
    let confirmed = reported
        .iter()
        .position(|o| *o == Observation::LossConfirmed)
        .expect("the loss was confirmed");
    assert!(
        reported[..confirmed].contains(&Observation::WorkloadLost { state_known: false }),
        "{reported:?}"
    );
    assert_eq!(losses(&h), 1, "{reported:?}");
    // Only an Interrupted task is resumed automatically.
    assert_eq!(auto_resumes(&h.events("task-1")), vec![(1, 5)]);
    let counters = h.counters();
    assert_eq!(counters.launches_over_existing, 0, "{counters:?}");
    assert_eq!(counters.storages_created, 1, "the same storage");
}

/// An Unknown task whose storage is gone stays Unknown even with its
/// workload gone too: a resume could only fail on the missing storage
/// again, so no loss is confirmed and nothing loops (R3, R13).
pub async fn an_unknown_task_without_storage_stays_unknown<T: ContractTarget>() {
    let Some(mut h) = Harness::<T>::new().await else {
        return;
    };
    h.start_controller();
    h.running("task-1").await;

    h.probe.fail_observes(2);
    h.wait_runner_state("task-1", RunnerState::Unknown).await;
    // Workload and storage vanish behind the controller's back — unseen
    // until both are gone, since a platform may remove them one by one.
    h.probe.fail_observes(u32::MAX / 2);
    let task = h.resources("task-1");
    let deadline = Instant::now() + h.target.patience();
    loop {
        let gone = h
            .probe
            .inner()
            .archive(&task)
            .await
            .is_ok_and(|progress| progress.is_complete());
        if gone {
            break;
        }
        assert!(Instant::now() < deadline, "the platform kept the task");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    h.probe.clear_faults();

    h.observed_again(3).await;
    assert_eq!(h.runner_state("task-1"), RunnerState::Unknown);
    assert!(!h.reported("task-1").contains(&Observation::LossConfirmed));
    assert!(auto_resumes(&h.events("task-1")).is_empty());
    let counters = h.counters();
    assert_eq!(counters.workloads_created, 1, "{counters:?}");
    assert_eq!(counters.storages_created, 1, "{counters:?}");

    h.must("task-1", OwnerCommand::Archive, "archive-1");
    h.wait_runner_state("task-1", RunnerState::Archived).await;
}

/// Transient launch failures are retried; a refusal interrupts the task,
/// and the resume policy launches it again (R1, R13).
pub async fn provisioning_failures_retry_or_resume_automatically<T: ContractTarget>() {
    let Some(mut h) = Harness::<T>::new().await else {
        return;
    };
    h.probe
        .fail_launches([BackendErrorKind::Transient, BackendErrorKind::Transient]);
    h.start_controller();
    h.running("task-1").await;
    assert!(
        !h.reported("task-1")
            .iter()
            .any(|o| matches!(o, Observation::WorkloadLost { .. })),
        "a transient failure is no loss"
    );

    h.probe.fail_launches([BackendErrorKind::Rejected]);
    h.running("task-2").await;
    assert!(
        h.reported("task-2")
            .contains(&Observation::WorkloadLost { state_known: true }),
        "the refusal interrupted it"
    );
    assert_eq!(auto_resumes(&h.events("task-2")), vec![(1, 5)]);
    let counters = h.counters();
    assert_eq!(counters.workloads_created, 2, "{counters:?}");
}

/// Archive deletes workload and storage exactly once, however often it is
/// delivered, and a restarted controller does not touch the tombstone
/// (R34, AE9).
pub async fn archive_deletes_exactly_once<T: ContractTarget>() {
    let Some(mut h) = Harness::<T>::new().await else {
        return;
    };
    let first = h.start_controller();
    h.running("task-1").await;

    h.must("task-1", OwnerCommand::Archive, "archive-1");
    h.must("task-1", OwnerCommand::Archive, "archive-1");
    h.wait_runner_state("task-1", RunnerState::Archived).await;
    h.must("task-1", OwnerCommand::Archive, "archive-2");
    h.settle().await;

    h.crash_controller(first);
    h.start_controller();
    h.settle().await;
    h.settle().await;
    let counters = h.counters();
    assert_eq!(counters.workload_deletions, 1, "{counters:?}");
    assert_eq!(counters.storage_deletions, 1, "{counters:?}");
    assert_eq!(counters.workloads_created, 1, "{counters:?}");
    let observed = h.observe("task-1").await;
    assert!(observed.workload.is_none() && !observed.storage.exists());
    assert_eq!(h.runner_state("task-1"), RunnerState::Archived);
}

/// AE9: a task whose workflow finished keeps its storage and stays
/// assigned until the owner archives it.
pub async fn a_finished_task_keeps_its_storage_until_archived<T: ContractTarget>() {
    let Some(mut h) = Harness::<T>::new().await else {
        return;
    };
    h.start_controller();
    h.running("task-1").await;
    h.finish("task-1").await;
    h.settle().await;
    assert_eq!(h.runner_state("task-1"), RunnerState::Running);
    assert!(h.observe("task-1").await.storage.exists());
    assert_eq!(h.counters().storage_deletions, 0);

    h.must("task-1", OwnerCommand::Archive, "archive-1");
    h.wait_runner_state("task-1", RunnerState::Archived).await;
    let observed = h.observe("task-1").await;
    assert!(observed.workload.is_none() && !observed.storage.exists());
}

/// A slow deletion keeps the pause from completing, a resume meanwhile is
/// refused, and the replacement comes only after the old compute is gone
/// (KTD2: delayed deletion, resume during pause).
pub async fn delayed_deletion_and_resume_during_pause<T: ContractTarget>() {
    let Some(mut h) = Harness::<T>::new().await else {
        return;
    };
    h.start_controller();
    h.running("task-1").await;

    h.probe.delay_deletions(30);
    h.must("task-1", OwnerCommand::Pause, "pause-1");
    h.wait_platform("task-1", "the deletion to start", |observed| {
        observed
            .workload
            .as_ref()
            .is_none_or(|w| w.phase == WorkloadPhase::Deleting || !w.phase.is_alive())
    })
    .await;
    h.wait_for("the safe point", |h| {
        let record = h.record("task-1");
        record
            .runner
            .as_ref()
            .is_some_and(|runner| runner.safe_point_generation == Some(record.generation))
    })
    .await;
    assert_eq!(
        h.runner_state("task-1"),
        RunnerState::PauseRequested,
        "the pause completes only once compute is gone"
    );
    let refused = h
        .command("task-1", OwnerCommand::Resume, "resume-early")
        .expect_err("no resume while the pause is under way");
    assert!(
        matches!(
            refused,
            ProtocolError::Conflict {
                code: "illegal_command",
                ..
            }
        ),
        "{refused:?}"
    );

    h.wait_runner_state("task-1", RunnerState::Stopped).await;
    assert!(h.observe("task-1").await.workload.is_none());
    h.must("task-1", OwnerCommand::Resume, "resume-1");
    h.wait_runner_state("task-1", RunnerState::Running).await;
    let counters = h.counters();
    assert_eq!(counters.workloads_created, 2, "{counters:?}");
    assert_eq!(counters.workload_deletions, 1, "{counters:?}");
    assert_eq!(counters.launches_over_existing, 0, "{counters:?}");
}

/// Lease handoff: a partitioned controller's lease lapses, another one
/// takes over and reconciles the same workload; the old one, back again,
/// is fenced and touches nothing (KTD3).
pub async fn lease_handoff_between_controllers<T: ContractTarget>() {
    let Some(mut h) = Harness::<T>::new().await else {
        return;
    };
    let first_credential = h.enroll();
    let first = h.start_controller_as(&first_credential, Duration::from_secs(2));
    h.running("task-1").await;
    let (first_id, first_epoch) = h.lease_holder().expect("first lease");

    h.sever(first);
    let second_credential = h.enroll();
    h.start_controller_as(&second_credential, Duration::from_secs(30));
    h.wait_for("the takeover", |h| {
        h.lease_holder()
            .is_some_and(|(holder, epoch)| holder != first_id && epoch > first_epoch)
    })
    .await;
    h.must("task-1", OwnerCommand::Pause, "pause-1");
    h.wait_runner_state("task-1", RunnerState::Stopped).await;
    let counters = h.counters();
    assert_eq!(
        counters.workloads_created, 1,
        "the takeover found the workload: {counters:?}"
    );

    // The old controller comes back to a fenced lease.
    let before = h.counters();
    h.restore(first);
    h.settle().await;
    h.settle().await;
    let after = h.counters();
    assert_eq!(after.workloads_created, before.workloads_created);
    assert_eq!(after.workload_deletions, before.workload_deletions);
    assert_eq!(after.storage_deletions, before.storage_deletions);
    assert_ne!(h.lease_holder().unwrap().0, first_id, "the takeover holds");
    assert_eq!(h.runner_state("task-1"), RunnerState::Stopped);

    h.must("task-1", OwnerCommand::Resume, "resume-1");
    h.wait_runner_state("task-1", RunnerState::Running).await;
    assert_eq!(h.counters().workloads_created, 2);
}

/// Archive racing start: an archive before any controller accepted the
/// task completes with nothing created; an archive during a provisioning
/// that keeps failing ends it, and nothing is created afterwards (R34).
pub async fn archive_racing_start_creates_nothing<T: ContractTarget>() {
    let Some(mut h) = Harness::<T>::new().await else {
        return;
    };
    h.create("early");
    h.must("early", OwnerCommand::Archive, "archive-early");
    assert_eq!(h.runner_state("early"), RunnerState::Archived);

    h.probe
        .fail_launches(std::iter::repeat_n(BackendErrorKind::Transient, 100_000));
    h.start_controller();
    h.create("racing");
    h.wait_runner_state("racing", RunnerState::Provisioning)
        .await;
    h.must("racing", OwnerCommand::Archive, "archive-racing");
    h.wait_runner_state("racing", RunnerState::Archived).await;
    h.probe.clear_faults();
    h.settle().await;
    h.settle().await;

    let counters = h.counters();
    assert_eq!(counters.workloads_created, 0, "{counters:?}");
    assert_eq!(counters.storages_created, 0, "{counters:?}");
    for task in ["early", "racing"] {
        let observed = h.observe(task).await;
        assert!(
            observed.workload.is_none() && !observed.storage.exists(),
            "{task}: {observed:?}"
        );
        assert_eq!(h.runner_state(task), RunnerState::Archived);
    }
}

/// AE1: two tasks run side by side on separate storage; pausing one
/// removes only its compute.
pub async fn parallel_tasks_are_isolated<T: ContractTarget>() {
    let Some(mut h) = Harness::<T>::new().await else {
        return;
    };
    h.start_controller();
    h.create("task-a");
    h.create("task-b");
    h.wait_runner_state("task-a", RunnerState::Running).await;
    h.wait_runner_state("task-b", RunnerState::Running).await;
    let (a, b) = (h.resources("task-a"), h.resources("task-b"));
    assert_ne!(a.storage, b.storage, "separate storage");
    assert_ne!(a.workload, b.workload, "separate workloads");

    h.must("task-a", OwnerCommand::Pause, "pause-a");
    h.wait_runner_state("task-a", RunnerState::Stopped).await;
    let paused = h.observe("task-a").await;
    assert!(paused.workload.is_none() && paused.storage.exists());
    assert_eq!(h.runner_state("task-b"), RunnerState::Running);
    assert!(
        alive(&h.observe("task-b").await),
        "the other task keeps running"
    );
    let counters = h.counters();
    assert_eq!(counters.workloads_created, 2, "{counters:?}");
    assert_eq!(counters.storages_created, 2, "{counters:?}");
}

/// U10: the controller is offered — and can only get accepted — as many
/// starts as the backend's limit allows. A waiting task says why it
/// waits, a pause releases the slot to it, and a resume finding the
/// backend full is refused rather than stopping anything (R11).
pub async fn the_controller_never_starts_more_than_the_limit<T: ContractTarget>() {
    let Some(mut h) = Harness::<T>::new().await else {
        return;
    };
    h.set_limit(1);
    h.start_controller();
    h.create("task-1");
    h.wait_runner_state("task-1", RunnerState::Running).await;
    h.create("task-2");
    h.settle().await;

    assert_eq!(h.runner_state("task-2"), RunnerState::Queued);
    assert!(
        h.reported("task-2").is_empty(),
        "no start was even accepted"
    );
    assert_eq!(
        h.record("task-2").queue.map(|queue| queue.reason),
        Some(
            narsor_engine::orchestration::QueueReason::WaitingForCapacity {
                position: 1,
                running: 1,
                limit: 1,
            }
        )
    );
    assert_eq!(h.counters().workloads_created, 1, "{:?}", h.counters());

    // Pausing releases the compute and its slot: the waiting task starts.
    h.must("task-1", OwnerCommand::Pause, "pause-1");
    h.wait_runner_state("task-1", RunnerState::Stopped).await;
    h.wait_runner_state("task-2", RunnerState::Running).await;

    // Resuming now would exceed the limit: the task waits for a slot
    // instead, and nothing stops.
    let response = h
        .command("task-1", OwnerCommand::Resume, "resume-1")
        .expect("a resume beyond the limit is queued");
    assert!(response.queued_for_slot);
    h.settle().await;
    assert_eq!(h.runner_state("task-1"), RunnerState::Queued);
    assert_eq!(h.runner_state("task-2"), RunnerState::Running);
    assert!(alive(&h.observe("task-2").await));

    // The slot task-2 frees goes to the waiting resume, which continues on
    // its own storage (`backend.resume`), not to a fresh provision.
    h.must("task-2", OwnerCommand::Archive, "archive-2");
    h.wait_runner_state("task-2", RunnerState::Archived).await;
    h.wait_runner_state("task-1", RunnerState::Running).await;
    let counters = h.counters();
    assert_eq!(counters.workloads_created, 3, "{counters:?}");
    assert_eq!(counters.resume_calls, 1, "{counters:?}");
    assert_eq!(
        counters.storages_created, 2,
        "no second clone: {counters:?}"
    );
}

/// U3: an operator command queued for a running task reaches its runner
/// through the poll the runner already makes and comes back with the
/// runner's verdict; a retry of its key is the same command, and a command
/// for a task whose compute is gone is refused at once.
pub async fn operator_commands_reach_the_runner_and_come_back_with_a_verdict<T: ContractTarget>() {
    let Some(mut h) = Harness::<T>::new().await else {
        return;
    };
    h.start_controller();
    h.running("task-1").await;

    let request = SubmitOperatorCommandRequest {
        command: OperatorCommand::Interrupt,
        idempotency_key: "interrupt-1".to_string(),
        expected_generation: None,
    };
    let owner = OwnerRef::new(LOCAL_OWNER);
    let submitted = h
        .service
        .submit_operator_command(&owner, "task-1", request.clone())
        .expect("an interrupt of a running task");
    assert!(!submitted.replayed);
    let command_id = submitted.command.command_id;
    let retried = h
        .service
        .submit_operator_command(&owner, "task-1", request)
        .expect("a retry of the key");
    assert!(retried.replayed);
    assert_eq!(retried.command.command_id, command_id);

    h.wait_for("the runner's verdict", |h| {
        h.record("task-1")
            .operator_command(&command_id)
            .is_some_and(|command| command.status.is_settled())
    })
    .await;
    // Either verdict is the runner's own — but a rejection says why.
    let command = h
        .record("task-1")
        .operator_command(&command_id)
        .cloned()
        .unwrap();
    if command.status == OperatorCommandStatus::Rejected {
        assert!(command.detail.is_some(), "{command:?}");
    }
    let events = h.events("task-1");
    assert!(events.iter().any(|kind| matches!(
        kind,
        TaskEventKind::OperatorCommandQueued { command_id: id, .. } if *id == command_id
    )));
    assert!(events.iter().any(|kind| matches!(
        kind,
        TaskEventKind::OperatorCommandResolved { command_id: id, .. } if *id == command_id
    )));

    h.must("task-1", OwnerCommand::Pause, "pause-1");
    h.wait_runner_state("task-1", RunnerState::Stopped).await;
    match h.service.submit_operator_command(
        &owner,
        "task-1",
        SubmitOperatorCommandRequest {
            command: OperatorCommand::Interrupt,
            idempotency_key: "interrupt-2".to_string(),
            expected_generation: None,
        },
    ) {
        Err(ProtocolError::Conflict { code, .. }) => assert_eq!(code, "operator_command_refused"),
        other => panic!("a paused task has no runner to command: {other:?}"),
    }
}

/// AE5: the owner reads a stopped task. The controller starts a read-only
/// runner beside nothing — no workload, no slot, storage untouched — keeps
/// it through the owner's lease, and removes it when the lease runs out; the
/// task stays `Stopped` throughout and nothing is reported lost (R8-R11).
pub async fn a_reader_reads_a_stopped_task_and_leaves_with_its_lease<T: ContractTarget>() {
    let Some(mut h) = Harness::<T>::new().await else {
        return;
    };
    h.start_controller();
    h.stopped("task-1").await;
    let before = h.storage_digest("task-1");
    let created = h.counters().workloads_created;

    h.lease_reader("task-1", 3);
    let reader = h.running_reader("task-1").await;
    assert_eq!(reader.launch_generation, h.record("task-1").generation);
    let observed = h.observe("task-1").await;
    assert!(
        observed.workload.is_none(),
        "no working runner: {observed:?}"
    );
    assert!(observed.storage.exists());
    assert_eq!(h.runner_state("task-1"), RunnerState::Stopped);

    // It stays for as long as the owner is reading, and the task's
    // keep-paused reconciliation does not take it for compute to remove.
    h.settle().await;
    assert!(h
        .reader("task-1")
        .await
        .is_some_and(|r| r.uid == reader.uid));

    h.wait_reader("task-1", "the lease to end the reader", |r| r.is_none())
        .await;
    assert_eq!(h.runner_state("task-1"), RunnerState::Stopped);
    assert_eq!(
        h.storage_digest("task-1"),
        before,
        "a reader writes nothing"
    );
    let counters = h.counters();
    assert_eq!(counters.readers_created, 1, "{counters:?}");
    assert_eq!(counters.reader_deletions, 1, "{counters:?}");
    assert_eq!(counters.workloads_created, created, "{counters:?}");
    assert!(counters.never_two_runners_on_one_storage(), "{counters:?}");
    assert!(
        !h.reported("task-1")
            .iter()
            .any(|o| matches!(o, Observation::WorkloadLost { .. })),
        "a reader is not the task's workload"
    );
}

/// AE9: the owner resumes a task a reader is reading. The controller removes
/// the reader — and waits for it to be gone, however slowly the platform
/// deletes it — before the working runner is created; the platform never
/// shows two runners on the storage (R15).
pub async fn a_resume_removes_the_reader_before_the_work_starts<T: ContractTarget>() {
    let Some(mut h) = Harness::<T>::new().await else {
        return;
    };
    h.start_controller();
    h.stopped("task-1").await;
    h.lease_reader("task-1", 60);
    h.running_reader("task-1").await;

    h.probe.delay_deletions(6);
    h.must("task-1", OwnerCommand::Resume, "resume-1");
    h.wait_runner_state("task-1", RunnerState::Running).await;

    h.wait_reader("task-1", "the reader to be gone", |r| r.is_none())
        .await;
    let counters = h.counters();
    assert_eq!(counters.workloads_created, 2, "{counters:?}");
    assert_eq!(counters.readers_created, 1, "{counters:?}");
    assert_eq!(counters.reader_deletions, 1, "{counters:?}");
    assert!(counters.never_two_runners_on_one_storage(), "{counters:?}");
    assert!(alive(&h.observe("task-1").await));

    // And it is paused and read again the same way.
    h.probe.clear_faults();
    h.must("task-1", OwnerCommand::Pause, "pause-2");
    h.wait_runner_state("task-1", RunnerState::Stopped).await;
    h.lease_reader("task-1", 60);
    h.running_reader("task-1").await;
    assert!(h.counters().never_two_runners_on_one_storage());
}

/// A task that was interrupted — its workload gone, its owner to decide —
/// is read like a stopped one, and its resume clears the reader first
/// (AE7's state, AE9's rule).
pub async fn an_interrupted_task_is_read_and_resumed_without_a_second_runner<T: ContractTarget>() {
    let Some(mut h) = Harness::<T>::without_auto_resume().await else {
        return;
    };
    h.start_controller();
    h.running("task-1").await;
    h.kill_workload("task-1");
    h.wait_runner_state("task-1", RunnerState::Interrupted)
        .await;

    h.lease_reader("task-1", 60);
    h.running_reader("task-1").await;
    assert_eq!(
        h.runner_state("task-1"),
        RunnerState::Interrupted,
        "reading changes nothing"
    );
    h.settle().await;
    assert_eq!(h.runner_state("task-1"), RunnerState::Interrupted);
    assert_eq!(h.counters().workloads_created, 1, "nothing replaces it");

    h.probe.delay_deletions(4);
    h.must("task-1", OwnerCommand::Resume, "resume-1");
    h.wait_runner_state("task-1", RunnerState::Running).await;
    h.wait_reader("task-1", "the reader to be gone", |r| r.is_none())
        .await;
    let counters = h.counters();
    assert_eq!(counters.workloads_created, 2, "{counters:?}");
    assert!(counters.never_two_runners_on_one_storage(), "{counters:?}");
}

/// Archiving a task a reader is reading removes the reader first, then the
/// workload and the storage under it (R15, R34).
pub async fn an_archive_removes_the_reader_before_the_storage<T: ContractTarget>() {
    let Some(mut h) = Harness::<T>::new().await else {
        return;
    };
    h.start_controller();
    h.stopped("task-1").await;
    h.lease_reader("task-1", 60);
    h.running_reader("task-1").await;

    h.probe.delay_deletions(4);
    h.must("task-1", OwnerCommand::Archive, "archive-1");
    h.wait_runner_state("task-1", RunnerState::Archived).await;

    assert!(h.reader("task-1").await.is_none());
    let observed = h.observe("task-1").await;
    assert!(
        observed.workload.is_none() && !observed.storage.exists(),
        "{observed:?}"
    );
    let counters = h.counters();
    assert_eq!(counters.reader_deletions, 1, "{counters:?}");
    assert_eq!(counters.storage_deletions, 1, "{counters:?}");
    assert_eq!(counters.archives_over_reader, 0, "{counters:?}");
}

/// A reader that cannot start changes nothing about the task: no loss is
/// reported, no state moves, and the controller tries again later while the
/// owner is still reading (R12).
pub async fn a_reader_that_cannot_start_leaves_the_task_alone<T: ContractTarget>() {
    let Some(mut h) = Harness::<T>::new().await else {
        return;
    };
    h.start_controller();
    h.stopped("task-1").await;
    h.probe
        .fail_reader_starts([BackendErrorKind::Rejected, BackendErrorKind::Transient]);

    h.lease_reader("task-1", 60);
    h.wait_for("the first reader start to fail", |h| {
        h.counters().reader_start_calls >= 1
    })
    .await;
    assert!(h.reader("task-1").await.is_none());
    assert_eq!(h.runner_state("task-1"), RunnerState::Stopped);

    // Later attempts get through.
    h.running_reader("task-1").await;
    let counters = h.counters();
    assert!(counters.reader_start_calls >= 3, "{counters:?}");
    assert_eq!(counters.readers_created, 1, "{counters:?}");
    assert_eq!(h.runner_state("task-1"), RunnerState::Stopped);
    assert!(
        !h.reported("task-1")
            .iter()
            .any(|o| matches!(o, Observation::WorkloadLost { .. })),
        "a failed reader is no lost workload"
    );
}

/// A reader that dies while the owner still reads is replaced — silently:
/// its death is not the task's (R11).
pub async fn a_reader_that_dies_is_replaced_while_it_is_wanted<T: ContractTarget>() {
    let Some(mut h) = Harness::<T>::new().await else {
        return;
    };
    h.start_controller();
    h.stopped("task-1").await;
    h.lease_reader("task-1", 60);
    let first = h.running_reader("task-1").await;

    assert!(
        h.target
            .kill_reader(h.probe.inner(), &h.resources("task-1")),
        "a reader to kill"
    );
    h.wait_reader("task-1", "a replacement reader", |r| {
        r.is_some_and(|r| r.uid != first.uid && r.phase.is_alive())
    })
    .await;
    assert_eq!(h.runner_state("task-1"), RunnerState::Stopped);
    assert!(
        !h.reported("task-1")
            .iter()
            .any(|o| matches!(o, Observation::WorkloadLost { .. })),
        "the death of a reader is not reported"
    );
    let counters = h.counters();
    assert_eq!(counters.readers_created, 2, "{counters:?}");
    assert!(counters.never_two_runners_on_one_storage(), "{counters:?}");
}

/// Expands the whole suite for one target into a module of tests.
macro_rules! contract_suite {
    ($module:ident, $target:ty) => {
        mod $module {
            #[allow(unused_imports)]
            use super::*;
            contract_suite!(@tests $target;
                lifecycle_round_trip,
                reconnect_reconciles_instead_of_duplicating,
                a_lost_launch_answer_is_reconciled_not_duplicated,
                a_dead_workload_is_resumed_automatically,
                workload_death_waits_for_the_owner_without_the_policy,
                a_failed_observation_reports_unknown_and_waits,
                a_confirmed_loss_of_an_unknown_task_is_resumed,
                an_unknown_task_without_storage_stays_unknown,
                provisioning_failures_retry_or_resume_automatically,
                archive_deletes_exactly_once,
                a_finished_task_keeps_its_storage_until_archived,
                delayed_deletion_and_resume_during_pause,
                lease_handoff_between_controllers,
                archive_racing_start_creates_nothing,
                parallel_tasks_are_isolated,
                the_controller_never_starts_more_than_the_limit,
                operator_commands_reach_the_runner_and_come_back_with_a_verdict,
                a_reader_reads_a_stopped_task_and_leaves_with_its_lease,
                a_resume_removes_the_reader_before_the_work_starts,
                an_interrupted_task_is_read_and_resumed_without_a_second_runner,
                an_archive_removes_the_reader_before_the_storage,
                a_reader_that_cannot_start_leaves_the_task_alone,
                a_reader_that_dies_is_replaced_while_it_is_wanted);
        }
    };
    (@tests $target:ty; $($scenario:ident),* $(,)?) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $scenario() {
                crate::backend_contract_support::$scenario::<$target>().await;
            }
        )*
    };
}
