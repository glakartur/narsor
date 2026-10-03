//! The backend-neutral contract suite (U4, AE2): one set of lifecycle
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
//! runner, the local adapter's `narsor runner` processes, a mock cluster's
//! Pods. Nothing here names a platform type.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use narsor::auth::{ControllerCredential, ControllerKey};
use narsor::backends::testkit::{ProbeCounters, Probed, SeverableLink};
use narsor::backends::{
    BackendErrorKind, Controller, ControllerConfig, ExecutionBackend, TaskObservation,
    TaskResources, WorkloadPhase,
};
use narsor::controller_protocol::http::{controller_routes, runner_routes};
use narsor::controller_protocol::{
    ControllerClient, CreateEnrollmentRequest, EnrollRequest, OwnerCommandRequest,
    OwnerCommandResponse, ProtocolError, ProtocolService, LOCAL_OWNER,
};
use narsor::domain::{BackendId, TaskId, TaskLaunchSpec};
use narsor::orchestration::{
    NewTask, Observation, OwnerCommand, OwnerRef, TaskEventKind, TaskLifecycle, TaskRecord,
};
use narsor::persistence::paths::StatePaths;
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

    /// A launch spec the target's runners can run. Its workflow finishes
    /// on its own; the runner then idles until told otherwise.
    fn launch_spec(&self) -> TaskLaunchSpec;

    /// The platform loses the task's workload unexpectedly.
    fn kill_workload(&self, backend: &Self::Backend, task: &TaskResources);

    /// Stops everything the target started (runner processes, mock
    /// servers). Called when a scenario ends, pass or fail.
    fn cleanup(&self, _backend: &Self::Backend) {}

    /// How long one lifecycle transition may take on this target.
    fn patience(&self) -> Duration {
        Duration::from_secs(30)
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
    root: TempRoot,
}

impl<T: ContractTarget> Harness<T> {
    /// Serves a control plane and sets the target up; `None` skips.
    pub async fn new() -> Option<Self> {
        let root = TempRoot::new();
        let Some(target) = T::setup(&root.0.join("target")) else {
            eprintln!("skipped: the contract target cannot run here");
            return None;
        };
        let state = root.0.join("control");
        fs::create_dir_all(&state).expect("state root");
        let service = Arc::new(ProtocolService::new(
            narsor::orchestration::ControlPlane::new(StatePaths::new(&state)),
            crate::environment_support::seeded_resolver(&state),
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
        let probe = Probed::new(target.backend());
        Some(Self {
            target,
            probe,
            service,
            url,
            controllers: Vec::new(),
            server,
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
            .enroll(EnrollRequest::signed(&code.enrollment_code, &key))
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
        let task_id = TaskId::parse(task).unwrap();
        self.service
            .control_plane()
            .create_task(NewTask::new(
                task_id.clone(),
                OwnerRef::new(LOCAL_OWNER),
                BackendId::parse(BACKEND).unwrap(),
                self.target.launch_spec(),
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

    // -- observation --------------------------------------------------------

    /// The task's control-plane record.
    pub fn record(&self, task: &str) -> TaskRecord {
        self.service
            .control_plane()
            .task(&TaskId::parse(task).unwrap())
            .expect("task record")
    }

    /// The task's lifecycle.
    pub fn lifecycle(&self, task: &str) -> TaskLifecycle {
        self.record(task).lifecycle
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

    /// Waits for the task to reach `lifecycle`.
    pub async fn wait_lifecycle(&self, task: &str, lifecycle: TaskLifecycle) {
        let deadline = Instant::now() + self.target.patience();
        loop {
            let record = self.record(task);
            if record.lifecycle == lifecycle {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{task} did not reach {lifecycle:?}: it is {:?} (observed {:?}, runner {:?}); counters {:?}",
                record.lifecycle,
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

    /// Lets controllers run a few rounds, for "nothing happens" checks.
    pub async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(600)).await;
    }

    /// Creates a task and waits until it runs.
    pub async fn running(&self, task: &str) {
        self.create(task);
        self.wait_lifecycle(task, TaskLifecycle::Running).await;
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
        self.target.cleanup(self.probe.inner());
        self.server.abort();
        let _ = &self.root;
    }
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
    h.wait_lifecycle("task-1", TaskLifecycle::Paused).await;
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
    h.wait_lifecycle("task-1", TaskLifecycle::Running).await;
    let resumed = h.record("task-1");
    let runner = resumed.runner.as_ref().unwrap();
    assert_ne!(runner.incarnation, first_incarnation, "a new incarnation");
    assert_eq!(runner.launch_generation, resumed.generation);
    assert!(
        runner.snapshot.event_cursor >= cursor_before,
        "the resumed runner continues the same storage's work"
    );

    h.must("task-1", OwnerCommand::Archive, "archive-1");
    h.wait_lifecycle("task-1", TaskLifecycle::Archived).await;
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
    assert_eq!(h.lifecycle("task-1"), TaskLifecycle::Running);
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
    assert_eq!(h.lifecycle("task-1"), TaskLifecycle::Running);

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
    assert_eq!(h.lifecycle("task-1"), TaskLifecycle::Running);
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
    h.wait_lifecycle("task-1", TaskLifecycle::Paused).await;
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

/// AE4: a workload that dies is reported Interrupted and never replaced
/// until the owner resumes; the dead one is cleared — even when its
/// deletion is slow — before the replacement is created (R7, R13).
pub async fn workload_death_waits_for_the_owner<T: ContractTarget>() {
    let Some(mut h) = Harness::<T>::new().await else {
        return;
    };
    h.start_controller();
    h.running("task-1").await;

    h.kill_workload("task-1");
    h.wait_lifecycle("task-1", TaskLifecycle::Interrupted).await;
    h.settle().await;
    h.settle().await;
    assert_eq!(h.lifecycle("task-1"), TaskLifecycle::Interrupted);
    assert_eq!(
        h.counters().workloads_created,
        1,
        "no automatic replacement"
    );
    assert!(
        h.reported("task-1")
            .contains(&Observation::WorkloadLost { state_known: true }),
        "the loss itself is known"
    );

    h.probe.delay_deletions(8);
    h.must("task-1", OwnerCommand::Resume, "resume-1");
    h.wait_lifecycle("task-1", TaskLifecycle::Running).await;
    let counters = h.counters();
    assert_eq!(counters.workloads_created, 2, "{counters:?}");
    assert_eq!(
        counters.workload_deletions, 1,
        "the dead workload went first"
    );
    assert_eq!(counters.launches_over_existing, 0, "{counters:?}");
    assert_eq!(counters.storages_created, 1, "the same storage");
}

/// A backend that cannot be observed makes the task Unknown, and nothing
/// replaces its workload until the owner decides (R13).
pub async fn a_failed_observation_reports_unknown_and_waits<T: ContractTarget>() {
    let Some(mut h) = Harness::<T>::new().await else {
        return;
    };
    h.start_controller();
    h.running("task-1").await;

    h.probe.fail_observes(2);
    h.wait_lifecycle("task-1", TaskLifecycle::Unknown).await;
    assert!(h
        .reported("task-1")
        .contains(&Observation::WorkloadLost { state_known: false }));
    h.settle().await;
    assert_eq!(
        h.lifecycle("task-1"),
        TaskLifecycle::Unknown,
        "observing again changes nothing"
    );
    assert_eq!(h.counters().workloads_created, 1);

    // The owner retries: the old launch goes, one new one comes.
    h.must("task-1", OwnerCommand::Start, "start-1");
    h.wait_lifecycle("task-1", TaskLifecycle::Running).await;
    let counters = h.counters();
    assert_eq!(counters.workloads_created, 2, "{counters:?}");
    assert_eq!(counters.workload_deletions, 1, "{counters:?}");
    assert_eq!(counters.launches_over_existing, 0, "{counters:?}");
}

/// Transient launch failures are retried; a refusal interrupts the task
/// until the owner starts it again (R13).
pub async fn provisioning_failures_retry_or_wait_for_the_owner<T: ContractTarget>() {
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
    h.create("task-2");
    h.wait_lifecycle("task-2", TaskLifecycle::Interrupted).await;
    h.settle().await;
    assert_eq!(h.lifecycle("task-2"), TaskLifecycle::Interrupted);
    assert!(h.observe("task-2").await.workload.is_none());

    h.must("task-2", OwnerCommand::Start, "start-1");
    h.wait_lifecycle("task-2", TaskLifecycle::Running).await;
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
    h.wait_lifecycle("task-1", TaskLifecycle::Archived).await;
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
    assert_eq!(h.lifecycle("task-1"), TaskLifecycle::Archived);
}

/// AE9: a task whose workflow finished keeps its storage and stays
/// assigned until the owner archives it.
pub async fn a_finished_task_keeps_its_storage_until_archived<T: ContractTarget>() {
    let Some(mut h) = Harness::<T>::new().await else {
        return;
    };
    h.start_controller();
    h.running("task-1").await;
    h.wait_for("the workflow to finish", |h| {
        h.record("task-1").runner.is_some_and(|runner| {
            runner.snapshot.execution_status == narsor::domain::ExecutionStatus::Completed
        })
    })
    .await;
    h.settle().await;
    assert_eq!(h.lifecycle("task-1"), TaskLifecycle::Running);
    assert!(h.observe("task-1").await.storage.exists());
    assert_eq!(h.counters().storage_deletions, 0);

    h.must("task-1", OwnerCommand::Archive, "archive-1");
    h.wait_lifecycle("task-1", TaskLifecycle::Archived).await;
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
        h.lifecycle("task-1"),
        TaskLifecycle::PauseRequested,
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

    h.wait_lifecycle("task-1", TaskLifecycle::Paused).await;
    assert!(h.observe("task-1").await.workload.is_none());
    h.must("task-1", OwnerCommand::Resume, "resume-1");
    h.wait_lifecycle("task-1", TaskLifecycle::Running).await;
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
    h.wait_lifecycle("task-1", TaskLifecycle::Paused).await;
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
    assert_eq!(h.lifecycle("task-1"), TaskLifecycle::Paused);

    h.must("task-1", OwnerCommand::Resume, "resume-1");
    h.wait_lifecycle("task-1", TaskLifecycle::Running).await;
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
    assert_eq!(h.lifecycle("early"), TaskLifecycle::Archived);

    h.probe
        .fail_launches(std::iter::repeat_n(BackendErrorKind::Transient, 100_000));
    h.start_controller();
    h.create("racing");
    h.wait_lifecycle("racing", TaskLifecycle::Provisioning)
        .await;
    h.must("racing", OwnerCommand::Archive, "archive-racing");
    h.wait_lifecycle("racing", TaskLifecycle::Archived).await;
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
        assert_eq!(h.lifecycle(task), TaskLifecycle::Archived);
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
    h.wait_lifecycle("task-a", TaskLifecycle::Running).await;
    h.wait_lifecycle("task-b", TaskLifecycle::Running).await;
    let (a, b) = (h.resources("task-a"), h.resources("task-b"));
    assert_ne!(a.storage, b.storage, "separate storage");
    assert_ne!(a.workload, b.workload, "separate workloads");

    h.must("task-a", OwnerCommand::Pause, "pause-a");
    h.wait_lifecycle("task-a", TaskLifecycle::Paused).await;
    let paused = h.observe("task-a").await;
    assert!(paused.workload.is_none() && paused.storage.exists());
    assert_eq!(h.lifecycle("task-b"), TaskLifecycle::Running);
    assert!(
        alive(&h.observe("task-b").await),
        "the other task keeps running"
    );
    let counters = h.counters();
    assert_eq!(counters.workloads_created, 2, "{counters:?}");
    assert_eq!(counters.storages_created, 2, "{counters:?}");
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
                workload_death_waits_for_the_owner,
                a_failed_observation_reports_unknown_and_waits,
                provisioning_failures_retry_or_wait_for_the_owner,
                archive_deletes_exactly_once,
                a_finished_task_keeps_its_storage_until_archived,
                delayed_deletion_and_resume_during_pause,
                lease_handoff_between_controllers,
                archive_racing_start_creates_nothing,
                parallel_tasks_are_isolated);
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
