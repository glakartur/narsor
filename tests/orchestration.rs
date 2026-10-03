//! Integration tests for remote task orchestration (U1).
//!
//! Like the persistence tests, everything runs against a real temporary
//! state root: the guarantees under test — atomic commits, per-task
//! serialization, recovery after partial writes — live in file operations.

use chrono::{DateTime, Duration, TimeZone, Utc};
use narsor::domain::{BackendId, TaskId, TaskLaunchSpec};
use narsor::orchestration::{
    BackendLease, Clock, CommandRequest, ControlPlane, ControllerReport, NewTask, Observation,
    ObservedSummary, OrchestrationError, OwnerCommand, OwnerRef, ResourceIdentity, TaskEventKind,
    TaskLifecycle, TaskRecord,
};
use narsor::persistence::paths::{temp_path_for, StatePaths};
use narsor::persistence::ExecutionStore;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A temporary directory that deletes itself when it goes out of scope.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("narsor-orchestration-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn paths(&self) -> StatePaths {
        StatePaths::new(&self.path)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A clock the test moves by hand, so lease expiry is deterministic.
#[derive(Debug)]
struct ManualClock(Mutex<DateTime<Utc>>);

impl ManualClock {
    fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(
            Utc.with_ymd_and_hms(2026, 10, 3, 12, 0, 0).unwrap(),
        )))
    }

    fn advance(&self, by: Duration) {
        *self.0.lock().unwrap() += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

/// A control plane plus the clock driving it.
struct Fixture {
    root: TempRoot,
    clock: Arc<ManualClock>,
    control: ControlPlane,
}

impl Fixture {
    fn new() -> Self {
        let root = TempRoot::new();
        let clock = ManualClock::new();
        let control = ControlPlane::with_clock(root.paths(), clock.clone());
        Self {
            root,
            clock,
            control,
        }
    }

    /// A second, independent handle on the same state root.
    fn reopen(&self) -> ControlPlane {
        ControlPlane::with_clock(self.root.paths(), self.clock.clone())
    }

    fn create(&self, id: &str) -> TaskRecord {
        self.control
            .create_task(new_task(id, "k8s"))
            .expect("create task")
    }

    fn command(&self, id: &str, command: OwnerCommand, key: &str) -> TaskRecord {
        self.control
            .command(&task(id), CommandRequest::new(command, key))
            .expect("owner command")
            .task
    }

    fn lease(&self, backend: &str, controller: &str) -> BackendLease {
        self.control
            .acquire_lease(&backend_id(backend), controller, Duration::minutes(5))
            .expect("acquire lease")
    }

    /// Reports `observation` at the task's current generation.
    fn observe(
        &self,
        id: &str,
        lease: &BackendLease,
        observation: Observation,
    ) -> Result<TaskRecord, OrchestrationError> {
        let generation = self.control.task(&task(id)).unwrap().generation;
        self.control
            .report(report(id, lease, generation, observation, None))
            .map(|outcome| outcome.task)
    }

    /// Drives a fresh task to `Running` through its controller.
    fn running(&self, id: &str, lease: &BackendLease) -> TaskRecord {
        self.create(id);
        self.observe(id, lease, Observation::StartAccepted)
            .expect("start accepted");
        self.observe(id, lease, Observation::RunnerReady)
            .expect("runner ready")
    }
}

fn task(id: &str) -> TaskId {
    TaskId::parse(id).expect("valid task id")
}

fn backend_id(id: &str) -> BackendId {
    BackendId::parse(id).expect("valid backend id")
}

fn launch_spec() -> TaskLaunchSpec {
    TaskLaunchSpec::new(
        "implement-feature",
        3,
        serde_json::json!({ "feature": "remote backends" }),
        serde_json::json!({ "repository": "narsor", "commit": "614c3a8" }),
    )
}

fn new_task(id: &str, backend: &str) -> NewTask {
    NewTask::new(
        task(id),
        OwnerRef::new("oidc:https://issuer|artur"),
        backend_id(backend),
        launch_spec(),
    )
}

fn report(
    id: &str,
    lease: &BackendLease,
    generation: u64,
    observation: Observation,
    resource: Option<ResourceIdentity>,
) -> ControllerReport {
    ControllerReport {
        task_id: task(id),
        lease: lease.token(),
        desired_generation: generation,
        observation,
        resource,
        summary: ObservedSummary {
            workload_present: true,
            storage_present: true,
            runner_status: None,
            detail: None,
        },
        observed_at: Utc.with_ymd_and_hms(2026, 10, 3, 12, 0, 0).unwrap(),
    }
}

fn resource(uid: &str, generation: u64) -> Option<ResourceIdentity> {
    Some(ResourceIdentity {
        uid: uid.to_string(),
        resource_generation: generation,
    })
}

#[test]
fn creating_a_task_records_a_queued_assignment_with_generation_and_queue_order() {
    let fx = Fixture::new();
    let first = fx.create("task-a");
    let second = fx
        .control
        .create_task(new_task("task-b", "k8s").with_priority(5))
        .unwrap();

    assert_eq!(first.lifecycle, TaskLifecycle::Queued);
    assert_eq!(first.generation, 1);
    assert_eq!(first.revision, 1);
    assert_eq!(first.priority, 0, "priority defaults to 0");
    assert_eq!(first.assignment.backend_id, backend_id("k8s"));
    assert_eq!(first.assignment.resource_key, "task-a");
    assert_eq!(first.owner, OwnerRef::new("oidc:https://issuer|artur"));
    assert!(first.observed.is_none());
    assert_eq!(second.priority, 5);
    assert!(second.queue_sequence > first.queue_sequence);

    let events = fx.control.events_since(&task("task-a"), 0).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, TaskEventKind::Created);
}

#[test]
fn desired_generation_grows_monotonically_through_the_whole_lifecycle() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    let running = fx.running("task-1", &lease);
    assert_eq!(running.lifecycle, TaskLifecycle::Running);
    assert_eq!(running.generation, 1, "observations never bump generation");

    let pausing = fx.command("task-1", OwnerCommand::Pause, "pause-1");
    assert_eq!(pausing.lifecycle, TaskLifecycle::PauseRequested);
    assert_eq!(pausing.generation, 2);

    let paused = fx
        .observe("task-1", &lease, Observation::PausedAtSafePoint)
        .unwrap();
    assert_eq!(paused.lifecycle, TaskLifecycle::Paused);

    let resumed = fx.command("task-1", OwnerCommand::Resume, "resume-1");
    assert_eq!(resumed.lifecycle, TaskLifecycle::Provisioning);
    assert_eq!(resumed.generation, 3);
    let running = fx
        .observe("task-1", &lease, Observation::RunnerReady)
        .unwrap();
    assert_eq!(running.lifecycle, TaskLifecycle::Running);

    let archiving = fx.command("task-1", OwnerCommand::Archive, "archive-1");
    assert_eq!(archiving.lifecycle, TaskLifecycle::Archiving);
    assert_eq!(archiving.generation, 4);
    let archived = fx
        .observe("task-1", &lease, Observation::ResourcesDeleted)
        .unwrap();
    assert_eq!(archived.lifecycle, TaskLifecycle::Archived);
    let tombstone = archived.archive.expect("tombstone");
    assert!(tombstone.completed_at.is_some());
    assert!(!tombstone.resources_never_created);
}

#[test]
fn owner_commands_need_no_controller_lease() {
    let fx = Fixture::new();
    fx.create("task-1");
    assert!(fx.control.lease(&backend_id("k8s")).unwrap().is_none());

    let archived = fx.command("task-1", OwnerCommand::Archive, "archive-1");
    assert_eq!(archived.lifecycle, TaskLifecycle::Archived);
    assert!(
        fx.control.lease(&backend_id("k8s")).unwrap().is_none(),
        "an owner command neither needs nor creates a lease"
    );
}

#[test]
fn repeating_a_command_with_its_idempotency_key_leaves_one_receipt_and_one_state() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    fx.running("task-1", &lease);

    let request = CommandRequest::new(OwnerCommand::Pause, "pause-1");
    let first = fx
        .control
        .command(&task("task-1"), request.clone())
        .unwrap();
    let again = fx.control.command(&task("task-1"), request).unwrap();
    assert!(!first.replayed);
    assert!(again.replayed);
    assert_eq!(again.receipt, first.receipt);
    assert_eq!(again.task, first.task, "a replay writes nothing");

    fx.observe("task-1", &lease, Observation::PausedAtSafePoint)
        .unwrap();
    let resume = CommandRequest::new(OwnerCommand::Resume, "resume-1");
    let resumed = fx.control.command(&task("task-1"), resume.clone()).unwrap();
    // A redelivered resume after the runner is back up must not resume twice.
    fx.observe("task-1", &lease, Observation::RunnerReady)
        .unwrap();
    let replay = fx.control.command(&task("task-1"), resume).unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.receipt, resumed.receipt);
    assert_eq!(replay.task.lifecycle, TaskLifecycle::Running);

    let archive = CommandRequest::new(OwnerCommand::Archive, "archive-1");
    fx.control
        .command(&task("task-1"), archive.clone())
        .unwrap();
    fx.control.command(&task("task-1"), archive).unwrap();

    let record = fx.control.task(&task("task-1")).unwrap();
    let keys: Vec<_> = record
        .commands
        .iter()
        .map(|receipt| receipt.idempotency_key.as_str())
        .collect();
    assert_eq!(keys, ["pause-1", "resume-1", "archive-1"]);
    assert_eq!(record.generation, 4);
}

#[test]
fn reusing_an_idempotency_key_for_a_different_command_is_refused() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    fx.running("task-1", &lease);
    fx.command("task-1", OwnerCommand::Pause, "key-1");

    let error = fx
        .control
        .command(
            &task("task-1"),
            CommandRequest::new(OwnerCommand::Archive, "key-1"),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        OrchestrationError::IdempotencyKeyReused {
            original: OwnerCommand::Pause,
            requested: OwnerCommand::Archive,
            ..
        }
    ));
    assert_eq!(
        fx.control.task(&task("task-1")).unwrap().lifecycle,
        TaskLifecycle::PauseRequested
    );
}

#[test]
fn a_command_expecting_an_older_generation_cannot_overwrite_newer_intent() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    fx.running("task-1", &lease);
    let archived = fx.command("task-1", OwnerCommand::Archive, "archive-1");

    let error = fx
        .control
        .command(
            &task("task-1"),
            CommandRequest::new(OwnerCommand::Pause, "pause-old").expecting_generation(1),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        OrchestrationError::StaleGeneration {
            current: 2,
            presented: 1,
            ..
        }
    ));
    assert_eq!(fx.control.task(&task("task-1")).unwrap(), archived);
}

#[test]
fn illegal_transitions_are_rejected_with_a_typed_error_and_change_nothing() {
    let fx = Fixture::new();
    let queued = fx.create("task-1");
    let lease = fx.lease("k8s", "controller-a");

    for command in [
        OwnerCommand::Pause,
        OwnerCommand::Resume,
        OwnerCommand::Start,
    ] {
        let error = fx
            .control
            .command(&task("task-1"), CommandRequest::new(command, "illegal"))
            .unwrap_err();
        assert!(matches!(
            error,
            OrchestrationError::IllegalCommand {
                lifecycle: TaskLifecycle::Queued,
                ..
            }
        ));
    }
    let error = fx
        .observe("task-1", &lease, Observation::RunnerReady)
        .unwrap_err();
    assert!(matches!(
        error,
        OrchestrationError::IllegalObservation {
            observation: Observation::RunnerReady,
            lifecycle: TaskLifecycle::Queued,
            ..
        }
    ));
    assert_eq!(fx.control.task(&task("task-1")).unwrap(), queued);
}

#[test]
fn a_stale_backend_lease_epoch_cannot_report() {
    let fx = Fixture::new();
    let old = fx.lease("k8s", "controller-a");
    fx.create("task-1");

    let held = fx
        .control
        .acquire_lease(&backend_id("k8s"), "controller-b", Duration::minutes(5))
        .unwrap_err();
    assert!(matches!(held, OrchestrationError::LeaseHeld { .. }));

    fx.clock.advance(Duration::minutes(6));
    let expired = fx
        .observe("task-1", &old, Observation::StartAccepted)
        .unwrap_err();
    assert!(matches!(expired, OrchestrationError::LeaseExpired { .. }));

    let new = fx.lease("k8s", "controller-b");
    assert_eq!(new.epoch, old.epoch + 1);
    fx.observe("task-1", &new, Observation::StartAccepted)
        .unwrap();

    let fenced = fx
        .observe("task-1", &old, Observation::RunnerReady)
        .unwrap_err();
    assert!(matches!(
        fenced,
        OrchestrationError::StaleLease {
            presented: 1,
            current: Some(2),
            ..
        }
    ));
    let renew = fx
        .control
        .renew_lease(&old.token(), Duration::minutes(5))
        .unwrap_err();
    assert!(matches!(renew, OrchestrationError::StaleLease { .. }));
    assert_eq!(
        fx.control.task(&task("task-1")).unwrap().lifecycle,
        TaskLifecycle::Provisioning
    );
}

#[test]
fn an_expired_lease_nobody_took_over_can_be_renewed_with_the_same_epoch() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    fx.clock.advance(Duration::minutes(10));
    let renewed = fx
        .control
        .renew_lease(&lease.token(), Duration::minutes(5))
        .unwrap();
    assert_eq!(renewed.epoch, lease.epoch);
    fx.create("task-1");
    fx.observe("task-1", &renewed, Observation::StartAccepted)
        .unwrap();
}

#[test]
fn a_controller_cannot_report_on_another_backends_task() {
    let fx = Fixture::new();
    fx.create("task-1");
    let other = fx.lease("docker", "controller-x");
    let error = fx
        .observe("task-1", &other, Observation::StartAccepted)
        .unwrap_err();
    assert!(matches!(error, OrchestrationError::WrongBackend { .. }));
}

#[test]
fn an_older_generation_report_cannot_overwrite_an_archival_intent() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    fx.create("task-1");
    // The controller fetched the start at generation 1, the owner archives
    // before it claims it: the late claim is fenced.
    let archived = fx.command("task-1", OwnerCommand::Archive, "archive-1");
    assert_eq!(archived.lifecycle, TaskLifecycle::Archived);

    let error = fx
        .control
        .report(report(
            "task-1",
            &lease,
            1,
            Observation::StartAccepted,
            None,
        ))
        .unwrap_err();
    assert!(matches!(
        error,
        OrchestrationError::StaleGeneration {
            current: 2,
            presented: 1,
            ..
        }
    ));
    assert_eq!(fx.control.task(&task("task-1")).unwrap(), archived);
}

#[test]
fn an_out_of_order_report_for_an_older_resource_generation_is_ignored() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    fx.running("task-1", &lease);

    let newer = report(
        "task-1",
        &lease,
        1,
        Observation::Status,
        resource("pod-1", 7),
    );
    let applied = fx.control.report(newer.clone()).unwrap();
    assert!(applied.applied);

    let older = report(
        "task-1",
        &lease,
        1,
        Observation::Status,
        resource("pod-1", 3),
    );
    let error = fx.control.report(older).unwrap_err();
    assert!(matches!(
        error,
        OrchestrationError::StaleObservation {
            accepted: 7,
            presented: 3,
            ..
        }
    ));

    let duplicate = fx.control.report(newer).unwrap();
    assert!(!duplicate.applied, "a redelivery is absorbed");
    assert_eq!(duplicate.task, applied.task);
    let observed = duplicate.task.observed.expect("observation");
    assert_eq!(observed.resource, resource("pod-1", 7));
    assert_eq!(observed.lease_epoch, lease.epoch);
    assert_eq!(observed.received_at, fx.clock.now());
}

#[test]
fn an_unexpected_workload_loss_interrupts_without_creating_a_replacement() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    let running = fx.running("task-1", &lease);

    let lost = fx
        .observe(
            "task-1",
            &lease,
            Observation::WorkloadLost { state_known: true },
        )
        .unwrap();
    assert_eq!(lost.lifecycle, TaskLifecycle::Interrupted);
    assert_eq!(lost.generation, running.generation, "no new desired work");

    // Nothing but the owner leads out of Interrupted.
    for observation in [Observation::StartAccepted, Observation::RunnerReady] {
        let error = fx.observe("task-1", &lease, observation).unwrap_err();
        assert!(matches!(
            error,
            OrchestrationError::IllegalObservation {
                lifecycle: TaskLifecycle::Interrupted,
                ..
            }
        ));
    }

    let retried = fx.command("task-1", OwnerCommand::Start, "retry-1");
    assert_eq!(retried.lifecycle, TaskLifecycle::Provisioning);
    assert_eq!(retried.generation, running.generation + 1);
}

#[test]
fn an_uncertain_workload_loss_becomes_unknown_and_waits_for_resume() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    fx.running("task-1", &lease);
    let unknown = fx
        .observe(
            "task-1",
            &lease,
            Observation::WorkloadLost { state_known: false },
        )
        .unwrap();
    assert_eq!(unknown.lifecycle, TaskLifecycle::Unknown);
    let resumed = fx.command("task-1", OwnerCommand::Resume, "resume-1");
    assert_eq!(resumed.lifecycle, TaskLifecycle::Provisioning);
}

#[test]
fn archiving_queued_provisioning_or_pause_requested_work_fences_it() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");

    // Queued: nothing was ever created, so it is clean at once.
    fx.create("queued");
    let queued = fx.command("queued", OwnerCommand::Archive, "archive");
    assert_eq!(queued.lifecycle, TaskLifecycle::Archived);
    assert!(queued.archive.as_ref().unwrap().resources_never_created);

    // Provisioning: resources may exist, so archive waits for deletion and
    // the pending readiness of the old generation is fenced.
    fx.create("provisioning");
    fx.observe("provisioning", &lease, Observation::StartAccepted)
        .unwrap();
    let archiving = fx.command("provisioning", OwnerCommand::Archive, "archive");
    assert_eq!(archiving.lifecycle, TaskLifecycle::Archiving);
    let late = fx
        .control
        .report(report(
            "provisioning",
            &lease,
            1,
            Observation::RunnerReady,
            None,
        ))
        .unwrap_err();
    assert!(matches!(late, OrchestrationError::StaleGeneration { .. }));
    let illegal = fx
        .observe("provisioning", &lease, Observation::RunnerReady)
        .unwrap_err();
    assert!(matches!(
        illegal,
        OrchestrationError::IllegalObservation { .. }
    ));
    // Absent resources acknowledged as deleted count as clean.
    let clean = fx
        .observe("provisioning", &lease, Observation::ResourcesDeleted)
        .unwrap();
    assert_eq!(clean.lifecycle, TaskLifecycle::Archived);

    // PauseRequested: the pending safe point of the pause is fenced.
    fx.running("pausing", &lease);
    let pausing = fx.command("pausing", OwnerCommand::Pause, "pause");
    fx.command("pausing", OwnerCommand::Archive, "archive");
    let late = fx
        .control
        .report(report(
            "pausing",
            &lease,
            pausing.generation,
            Observation::PausedAtSafePoint,
            None,
        ))
        .unwrap_err();
    assert!(matches!(late, OrchestrationError::StaleGeneration { .. }));
    assert_eq!(
        fx.control.task(&task("pausing")).unwrap().lifecycle,
        TaskLifecycle::Archiving
    );
}

#[test]
fn repeating_archive_converges_on_the_tombstone_and_keeps_the_audit_trail() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    fx.running("task-1", &lease);
    let first = fx.command("task-1", OwnerCommand::Archive, "archive-1");
    let tombstone = first.archive.clone().expect("tombstone");

    let second = fx.command("task-1", OwnerCommand::Archive, "archive-2");
    assert_eq!(second.lifecycle, TaskLifecycle::Archiving);
    assert_eq!(second.generation, first.generation);
    assert_eq!(second.archive, Some(tombstone.clone()));

    fx.observe("task-1", &lease, Observation::ResourcesDeleted)
        .unwrap();
    let third = fx.command("task-1", OwnerCommand::Archive, "archive-3");
    assert_eq!(third.lifecycle, TaskLifecycle::Archived);
    assert_eq!(third.archive.unwrap().idempotency_key, "archive-1");
    let redelivered = fx
        .observe("task-1", &lease, Observation::ResourcesDeleted)
        .unwrap();
    assert_eq!(redelivered.lifecycle, TaskLifecycle::Archived);

    let resume = fx
        .control
        .command(
            &task("task-1"),
            CommandRequest::new(OwnerCommand::Resume, "resume-after-archive"),
        )
        .unwrap_err();
    assert!(matches!(
        resume,
        OrchestrationError::IllegalCommand {
            lifecycle: TaskLifecycle::Archived,
            ..
        }
    ));

    let record = fx.control.task(&task("task-1")).unwrap();
    assert_eq!(record.commands.len(), 3, "every archive stays on record");
    let events = fx.control.events_since(&task("task-1"), 0).unwrap();
    assert_eq!(events.len() as u64, record.revision);
    assert_eq!(events[0].kind, TaskEventKind::Created);
}

#[test]
fn creating_a_task_is_idempotent_but_a_different_request_conflicts() {
    let fx = Fixture::new();
    let created = fx.create("task-1");
    assert_eq!(fx.create("task-1"), created);

    let different_spec = NewTask {
        launch_spec: TaskLaunchSpec::new(
            "implement-feature",
            3,
            serde_json::json!({ "feature": "something else" }),
            serde_json::json!({ "repository": "narsor", "commit": "614c3a8" }),
        ),
        ..new_task("task-1", "k8s")
    };
    let conflicting = [
        different_spec,
        new_task("task-1", "docker"),
        new_task("task-1", "k8s").with_priority(9),
        NewTask {
            owner: OwnerRef::new("someone-else"),
            ..new_task("task-1", "k8s")
        },
    ];
    for request in conflicting {
        let error = fx.control.create_task(request).unwrap_err();
        assert!(matches!(error, OrchestrationError::TaskConflict(_)));
    }

    let mut tampered = new_task("task-2", "k8s");
    tampered.launch_spec.inputs = serde_json::json!({ "feature": "tampered" });
    let error = fx.control.create_task(tampered).unwrap_err();
    assert!(matches!(error, OrchestrationError::InvalidLaunchSpec(_)));
    assert_eq!(fx.control.list_tasks().unwrap(), vec![created]);
}

#[test]
fn unsafe_identifiers_are_refused_before_any_path_is_built() {
    for bad in ["", "..", ".hidden", "a/b", "a b", &"x".repeat(129)] {
        assert!(TaskId::parse(bad).is_err(), "{bad:?} must be refused");
    }
    let parsed: Result<TaskId, _> = serde_json::from_str("\"../escape\"");
    assert!(parsed.is_err(), "deserialization validates too");
}

#[test]
fn a_reconnect_cannot_bootstrap_a_second_runner_on_initialized_task_storage() {
    let fx = Fixture::new();
    fx.create("task-1");
    let storage_root = TempRoot::new();
    let task_storage = ExecutionStore::new(storage_root.path());

    assert!(fx
        .control
        .bootstrap_task_storage(&task("task-1"), &task_storage)
        .unwrap());
    assert!(
        !fx.control
            .bootstrap_task_storage(&task("task-1"), &task_storage)
            .unwrap(),
        "the identical spec is already there: reconcile, do not bootstrap"
    );
    assert_eq!(
        task_storage.load_task_launch_spec(&task("task-1")).unwrap(),
        Some(launch_spec())
    );

    fx.command("task-1", OwnerCommand::Archive, "archive");
    let error = fx
        .control
        .bootstrap_task_storage(&task("task-1"), &task_storage)
        .unwrap_err();
    assert!(matches!(error, OrchestrationError::IllegalCommand { .. }));
}

#[test]
fn concurrent_mutations_from_threads_serialize_without_lost_updates() {
    let fx = Fixture::new();
    fx.create("task-1");
    fx.command("task-1", OwnerCommand::Archive, "archive-0");

    const THREADS: usize = 8;
    const PER_THREAD: usize = 10;
    std::thread::scope(|scope| {
        for thread in 0..THREADS {
            // Each thread its own handle, as separate requests would have.
            let control = fx.reopen();
            scope.spawn(move || {
                for index in 0..PER_THREAD {
                    control
                        .command(
                            &task("task-1"),
                            CommandRequest::new(
                                OwnerCommand::Archive,
                                format!("archive-{thread}-{index}"),
                            ),
                        )
                        .expect("concurrent archive");
                }
            });
        }
    });

    let record = fx.control.task(&task("task-1")).unwrap();
    assert_eq!(record.commands.len(), 1 + THREADS * PER_THREAD);
    assert_eq!(record.revision, 2 + (THREADS * PER_THREAD) as u64);
    let events = fx.control.events_since(&task("task-1"), 0).unwrap();
    let sequences: Vec<u64> = events.iter().map(|event| event.sequence).collect();
    assert_eq!(sequences, (1..=record.revision).collect::<Vec<_>>());
}

#[test]
fn concurrent_creations_get_distinct_queue_sequences() {
    let fx = Fixture::new();
    std::thread::scope(|scope| {
        for index in 0..8 {
            let control = fx.reopen();
            scope.spawn(move || {
                control
                    .create_task(new_task(&format!("task-{index}"), "k8s"))
                    .expect("create");
            });
        }
    });
    let sequences: Vec<u64> = fx
        .control
        .list_tasks()
        .unwrap()
        .iter()
        .map(|task| task.queue_sequence)
        .collect();
    assert_eq!(sequences, (1..=8).collect::<Vec<_>>());
}

#[test]
fn listing_returns_every_task_in_creation_order_including_tombstones() {
    let fx = Fixture::new();
    for id in ["zeta", "alpha", "mid"] {
        fx.create(id);
    }
    fx.command("alpha", OwnerCommand::Archive, "archive");
    // A directory without a record is debris, not a task.
    fs::create_dir_all(fx.root.paths().orchestration_task_dir("debris")).unwrap();

    let listed: Vec<(String, TaskLifecycle)> = fx
        .control
        .list_tasks()
        .unwrap()
        .into_iter()
        .map(|task| (task.task_id.to_string(), task.lifecycle))
        .collect();
    assert_eq!(
        listed,
        [
            ("zeta".to_string(), TaskLifecycle::Queued),
            ("alpha".to_string(), TaskLifecycle::Archived),
            ("mid".to_string(), TaskLifecycle::Queued),
        ]
    );
}

#[test]
fn the_event_log_is_monotonic_and_survives_reopening() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    fx.running("task-1", &lease);
    fx.command("task-1", OwnerCommand::Pause, "pause-1");

    let reopened = fx.reopen();
    let all = reopened.events_since(&task("task-1"), 0).unwrap();
    let sequences: Vec<u64> = all.iter().map(|event| event.sequence).collect();
    assert_eq!(sequences, [1, 2, 3, 4]);
    assert_eq!(
        all.last().unwrap().kind,
        TaskEventKind::OwnerCommandAccepted {
            command: OwnerCommand::Pause,
            idempotency_key: "pause-1".to_string(),
        }
    );

    let tail = reopened.events_since(&task("task-1"), 2).unwrap();
    assert_eq!(tail, all[2..]);
    assert!(reopened
        .events_since(&task("task-1"), 4)
        .unwrap()
        .is_empty());

    let generation = reopened.task(&task("task-1")).unwrap().generation;
    reopened
        .report(report(
            "task-1",
            &lease,
            generation,
            Observation::PausedAtSafePoint,
            None,
        ))
        .expect("a reopened handle keeps appending");
    let next = fx.control.events_since(&task("task-1"), 4).unwrap();
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].sequence, 5);
}

#[test]
fn recovery_repairs_an_unpublished_event_and_removes_orphaned_temp_files() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    fx.running("task-1", &lease);
    let paths = fx.root.paths();

    // Crash after the record committed revision 3 but before its event was
    // appended: the log is one behind.
    let record = fx.control.task(&task("task-1")).unwrap();
    fs::remove_file(paths.orchestration_task_event_file("task-1", record.revision)).unwrap();
    let replayed = fx.control.events_since(&task("task-1"), 0).unwrap();
    assert_eq!(
        replayed.len() as u64,
        record.revision,
        "readers never skip it"
    );

    // Crash mid-write: torn temporary files next to every kind of record.
    let torn = [
        temp_path_for(&paths.orchestration_task_file("task-1")),
        temp_path_for(&paths.orchestration_task_event_file("task-1", 9)),
        temp_path_for(&paths.backend_lease_file("k8s")),
    ];
    for path in &torn {
        fs::write(path, b"{\"half\":").unwrap();
    }
    assert_eq!(fx.control.task(&task("task-1")).unwrap(), record);

    let recovery = fx.reopen().recover().unwrap();
    assert_eq!(recovery.removed_temp_files.len(), torn.len());
    assert_eq!(
        recovery.republished_events,
        vec![(task("task-1"), record.revision)]
    );
    for path in &torn {
        assert!(!path.exists());
    }
    assert!(paths
        .orchestration_task_event_file("task-1", record.revision)
        .exists());
    assert_eq!(fx.control.recover().unwrap(), Default::default());
}

#[test]
fn a_mutation_after_an_interrupted_publish_keeps_the_log_gapless() {
    let fx = Fixture::new();
    fx.create("task-1");
    let lease = fx.lease("k8s", "controller-a");
    fx.observe("task-1", &lease, Observation::StartAccepted)
        .unwrap();
    fs::remove_file(fx.root.paths().orchestration_task_event_file("task-1", 2)).unwrap();

    fx.observe("task-1", &lease, Observation::RunnerReady)
        .unwrap();
    let sequences: Vec<u64> = fx
        .control
        .events_since(&task("task-1"), 0)
        .unwrap()
        .iter()
        .map(|event| event.sequence)
        .collect();
    assert_eq!(sequences, [1, 2, 3]);
}

#[test]
fn local_crash_recovery_never_touches_orchestration_records() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    let running = fx.running("task-1", &lease);
    let store = ExecutionStore::new(fx.root.path());
    fx.control
        .bootstrap_task_storage(&task("task-1"), &store)
        .unwrap();
    let orphan = temp_path_for(&fx.root.paths().orchestration_task_file("task-1"));
    fs::write(&orphan, b"torn").unwrap();

    let report = narsor::persistence::recover(&store).expect("local recovery");
    assert!(report.reconciled.is_empty(), "no remote task is reconciled");
    assert!(
        orphan.exists(),
        "orchestration debris is left to ControlPlane::recover"
    );
    assert!(
        store.list_execution_ids().unwrap().is_empty(),
        "a launch spec is not an execution"
    );
    assert_eq!(fx.control.task(&task("task-1")).unwrap(), running);
}

#[test]
fn an_unknown_task_is_reported_as_not_found_without_creating_debris() {
    let fx = Fixture::new();
    let error = fx
        .control
        .command(
            &task("missing"),
            CommandRequest::new(OwnerCommand::Archive, "archive"),
        )
        .unwrap_err();
    assert!(matches!(error, OrchestrationError::TaskNotFound(_)));
    assert!(!fx.root.paths().orchestration_task_dir("missing").exists());
    let error = fx.control.events_since(&task("missing"), 0).unwrap_err();
    assert!(matches!(error, OrchestrationError::TaskNotFound(_)));
}

#[test]
fn reading_the_event_log_while_a_writer_appends_never_sees_a_gap() {
    let fx = Fixture::new();
    fx.create("task-1");
    fx.command("task-1", OwnerCommand::Archive, "archive-0");

    std::thread::scope(|scope| {
        let writer = fx.reopen();
        scope.spawn(move || {
            for index in 0..50 {
                writer
                    .command(
                        &task("task-1"),
                        CommandRequest::new(OwnerCommand::Archive, format!("concurrent-{index}")),
                    )
                    .expect("archive");
            }
        });
        let reader = fx.reopen();
        scope.spawn(move || {
            let mut cursor = 0;
            while cursor < 52 {
                for event in reader.events_since(&task("task-1"), cursor).expect("read") {
                    assert_eq!(event.sequence, cursor + 1, "cursor replay is gapless");
                    cursor = event.sequence;
                }
            }
        });
    });
}
