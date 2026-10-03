//! Integration tests for remote task orchestration (U1).
//!
//! Like the persistence tests, everything runs against a real temporary
//! state root: the guarantees under test — atomic commits, per-task
//! serialization, recovery after partial writes — live in file operations.

use chrono::{DateTime, Duration, TimeZone, Utc};
use narsor::domain::{BackendId, ExecutionStatus, TaskId, TaskLaunchSpec};
use narsor::orchestration::{
    BackendCapacity, BackendLease, Clock, CommandRequest, ControlPlane, ControllerReport, NewTask,
    Observation, ObservedSummary, OrchestrationError, OwnerCommand, OwnerRef, QueueReason,
    ResourceIdentity, RunnerSnapshot, TaskEventKind, TaskLifecycle, TaskRecord,
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
        if observation == Observation::PausedAtSafePoint {
            self.acknowledge_safe_point(id);
        }
        if observation == Observation::StartAccepted {
            self.admit(id);
        }
        let generation = self.control.task(&task(id)).unwrap().generation;
        self.control
            .report(report(id, lease, generation, observation, None))
            .map(|outcome| outcome.task)
    }

    /// Lets the scheduler admit the task's backend queue, as the control
    /// plane does before offering a start (U10). A no-op for a task that is
    /// not queued, or whose backend has no live controller.
    fn admit(&self, id: &str) {
        let record = self.control.task(&task(id)).unwrap();
        self.control
            .schedule(&record.assignment.backend_id, &BackendCapacity::Limit(16))
            .expect("admission pass");
    }

    /// Lets the task's runner acknowledge a safe point, when a pause waits
    /// for one: the control plane completes a pause only after that (KTD2).
    fn acknowledge_safe_point(&self, id: &str) {
        let record = self.control.task(&task(id)).unwrap();
        if record.lifecycle != TaskLifecycle::PauseRequested {
            return;
        }
        let launch_generation = record
            .commands
            .iter()
            .filter(|receipt| matches!(receipt.command, OwnerCommand::Start | OwnerCommand::Resume))
            .map(|receipt| receipt.generation_after)
            .max()
            .unwrap_or(1);
        let sequence = record
            .runner
            .as_ref()
            .map_or(1, |runner| runner.snapshot.snapshot_sequence + 1);
        self.control
            .record_runner_snapshot(
                &task(id),
                &format!("pod-g{launch_generation}"),
                launch_generation,
                safe_point_snapshot(sequence),
            )
            .expect("safe-point snapshot");
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

fn git_workspace() -> narsor::domain::TaskWorkspace {
    narsor::domain::TaskWorkspace::Git(narsor::domain::GitIdentity {
        repository: Some("narsor".to_string()),
        remote_url: "https://example.com/narsor.git".to_string(),
        commit: "614c3a8".repeat(5) + "abcde",
        credential: None,
    })
}

fn launch_spec() -> TaskLaunchSpec {
    TaskLaunchSpec::new(
        "implement-feature",
        3,
        serde_json::json!({ "feature": "remote backends" }),
        git_workspace(),
        narsor::domain::ResolvedEnvironment::harness_only("claude-code"),
        narsor::domain::ResourceProfile::new(1000, 2048),
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

fn safe_point_snapshot(sequence: u64) -> RunnerSnapshot {
    RunnerSnapshot {
        snapshot_sequence: sequence,
        execution_status: ExecutionStatus::Running,
        execution_cursor: Default::default(),
        event_cursor: 0,
        pending_input: None,
        conversation: Default::default(),
        timeline: None,
        safe_point: true,
        problem: None,
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
            git_workspace(),
            narsor::domain::ResolvedEnvironment::harness_only("claude-code"),
            narsor::domain::ResourceProfile::new(1000, 2048),
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
    // Created, admitted (U10), start accepted, runner ready, pause.
    assert_eq!(sequences, [1, 2, 3, 4, 5]);
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
        .events_since(&task("task-1"), 5)
        .unwrap()
        .is_empty());

    fx.acknowledge_safe_point("task-1");
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
    // The runner's safe point is sequence 6, the pause completing 7.
    let next = fx.control.events_since(&task("task-1"), 5).unwrap();
    let sequences: Vec<u64> = next.iter().map(|event| event.sequence).collect();
    assert_eq!(sequences, [6, 7]);
}

#[test]
fn a_pause_completes_only_after_the_runner_acknowledged_a_safe_point() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    fx.running("task-1", &lease);
    let pausing = fx.command("task-1", OwnerCommand::Pause, "pause-1");

    // Straight to the control plane, without the protocol service in front:
    // an in-process adapter is held to the same rule (KTD2).
    let refused = fx
        .control
        .report(report(
            "task-1",
            &lease,
            pausing.generation,
            Observation::PausedAtSafePoint,
            None,
        ))
        .unwrap_err();
    assert!(
        matches!(refused, OrchestrationError::SafePointNotAcknowledged { generation, .. } if generation == pausing.generation),
        "{refused:?}"
    );
    assert_eq!(
        fx.control.task(&task("task-1")).unwrap().lifecycle,
        TaskLifecycle::PauseRequested
    );

    fx.acknowledge_safe_point("task-1");
    let paused = fx
        .control
        .report(report(
            "task-1",
            &lease,
            pausing.generation,
            Observation::PausedAtSafePoint,
            None,
        ))
        .expect("acknowledged pause");
    assert_eq!(paused.task.lifecycle, TaskLifecycle::Paused);
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
    // Created (1), admitted (2), start accepted (3): lose the last publish.
    fs::remove_file(fx.root.paths().orchestration_task_event_file("task-1", 3)).unwrap();

    fx.observe("task-1", &lease, Observation::RunnerReady)
        .unwrap();
    let sequences: Vec<u64> = fx
        .control
        .events_since(&task("task-1"), 0)
        .unwrap()
        .iter()
        .map(|event| event.sequence)
        .collect();
    assert_eq!(sequences, [1, 2, 3, 4]);
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

// ---------------------------------------------------------------------------
// Admission by backend capacity (U10)
// ---------------------------------------------------------------------------

fn k8s() -> BackendId {
    backend_id("k8s")
}

fn limit(slots: u32) -> BackendCapacity {
    BackendCapacity::Limit(slots)
}

fn reason(fx: &Fixture, id: &str) -> Option<QueueReason> {
    fx.control
        .task(&task(id))
        .unwrap()
        .queue
        .map(|queue| queue.reason)
}

fn create_with_priority(fx: &Fixture, id: &str, priority: i32) {
    fx.control
        .create_task(new_task(id, "k8s").with_priority(priority))
        .expect("create task");
}

/// Reports straight to the control plane, without the fixture's admission.
fn report_now(
    fx: &Fixture,
    id: &str,
    lease: &BackendLease,
    observation: Observation,
) -> Result<TaskRecord, OrchestrationError> {
    let generation = fx.control.task(&task(id)).unwrap().generation;
    fx.control
        .report(report(id, lease, generation, observation, None))
        .map(|outcome| outcome.task)
}

#[test]
fn a_controller_cannot_start_a_task_the_scheduler_did_not_admit() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    fx.create("task-1");

    let refused = report_now(&fx, "task-1", &lease, Observation::StartAccepted).unwrap_err();
    assert!(matches!(refused, OrchestrationError::NotAdmitted(_)));
    assert_eq!(fx.control.task(&task("task-1")).unwrap().revision, 1);

    assert_eq!(
        fx.control.schedule(&k8s(), &limit(1)).unwrap(),
        [task("task-1")]
    );
    assert_eq!(reason(&fx, "task-1"), Some(QueueReason::Admitted));
    let started = report_now(&fx, "task-1", &lease, Observation::StartAccepted).unwrap();
    assert_eq!(started.lifecycle, TaskLifecycle::Provisioning);
    // Leaving the queue drops the reservation: the lifecycle holds the slot.
    assert_eq!(started.reservation, None);
    assert_eq!(started.queue, None);
    assert_eq!(fx.control.capacity_in_use(&k8s()).unwrap(), 1);
}

#[test]
fn at_capacity_priority_orders_the_waiting_and_fifo_breaks_ties_without_preemption() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    fx.create("running");
    fx.control.schedule(&k8s(), &limit(1)).unwrap();
    report_now(&fx, "running", &lease, Observation::StartAccepted).unwrap();
    report_now(&fx, "running", &lease, Observation::RunnerReady).unwrap();

    create_with_priority(&fx, "low", 0);
    create_with_priority(&fx, "high-first", 5);
    create_with_priority(&fx, "high-second", 5);
    fx.control.schedule(&k8s(), &limit(1)).unwrap();

    let waiting = |position| QueueReason::WaitingForCapacity {
        position,
        running: 1,
        limit: 1,
    };
    assert_eq!(reason(&fx, "high-first"), Some(waiting(1)));
    assert_eq!(reason(&fx, "high-second"), Some(waiting(2)));
    assert_eq!(reason(&fx, "low"), Some(waiting(3)));
    // A full backend never stops what runs (R11).
    let running = fx.control.task(&task("running")).unwrap();
    assert_eq!(running.lifecycle, TaskLifecycle::Running);
    assert_eq!(running.generation, 1);

    // A repeated pass over an unchanged backend writes nothing.
    assert!(fx.control.schedule(&k8s(), &limit(1)).unwrap().is_empty());

    // Raising the low task above the others moves it to the front.
    let (_, changed) = fx
        .control
        .set_priority(&task("low"), 9, None)
        .expect("set priority");
    assert!(changed);
    fx.control.schedule(&k8s(), &limit(1)).unwrap();
    assert_eq!(reason(&fx, "low"), Some(waiting(1)));
    assert_eq!(reason(&fx, "high-first"), Some(waiting(2)));

    // One more slot: exactly the first in line is admitted.
    let admitted = fx.control.schedule(&k8s(), &limit(2)).unwrap();
    assert!(admitted.contains(&task("low")));
    assert_eq!(reason(&fx, "low"), Some(QueueReason::Admitted));
    assert_eq!(
        reason(&fx, "high-first"),
        Some(QueueReason::WaitingForCapacity {
            position: 1,
            running: 2,
            limit: 2,
        })
    );

    // Lowering the limit again takes no reservation back.
    fx.control.schedule(&k8s(), &limit(1)).unwrap();
    assert_eq!(reason(&fx, "low"), Some(QueueReason::Admitted));
    assert_eq!(fx.control.capacity_in_use(&k8s()).unwrap(), 2);
    assert!(matches!(
        report_now(&fx, "high-first", &lease, Observation::StartAccepted),
        Err(OrchestrationError::NotAdmitted(_))
    ));
}

#[test]
fn an_offline_controller_withholds_capacity_but_keeps_granted_reservations() {
    let fx = Fixture::new();
    fx.create("task-1");
    fx.control.schedule(&k8s(), &limit(2)).unwrap();
    assert_eq!(reason(&fx, "task-1"), Some(QueueReason::ControllerOffline));

    let lease = fx.lease("k8s", "controller-a");
    fx.control.schedule(&k8s(), &limit(2)).unwrap();
    assert_eq!(reason(&fx, "task-1"), Some(QueueReason::Admitted));

    // Revoking the controller expires its lease: the backend is offline.
    fx.control
        .revoke_lease(&k8s(), "controller-a")
        .unwrap()
        .expect("held lease");
    fx.create("task-2");
    fx.control.schedule(&k8s(), &limit(2)).unwrap();
    assert_eq!(reason(&fx, "task-2"), Some(QueueReason::ControllerOffline));
    assert_eq!(reason(&fx, "task-1"), Some(QueueReason::Admitted));
    assert!(report_now(&fx, "task-1", &lease, Observation::StartAccepted).is_err());

    // A lease that merely ran out withholds capacity the same way.
    fx.lease("k8s", "controller-b");
    fx.clock.advance(Duration::minutes(10));
    fx.control.schedule(&k8s(), &limit(2)).unwrap();
    assert_eq!(reason(&fx, "task-2"), Some(QueueReason::ControllerOffline));

    let lease = fx.lease("k8s", "controller-b");
    fx.control.schedule(&k8s(), &limit(2)).unwrap();
    assert_eq!(reason(&fx, "task-2"), Some(QueueReason::Admitted));
    report_now(&fx, "task-1", &lease, Observation::StartAccepted).expect("first in line starts");
}

#[test]
fn an_unknown_backend_admits_nothing_and_says_so() {
    let fx = Fixture::new();
    fx.lease("k8s", "controller-a");
    fx.create("task-1");
    fx.control
        .schedule(
            &k8s(),
            &BackendCapacity::Unavailable("backend 'k8s' is not registered".into()),
        )
        .unwrap();
    assert_eq!(
        reason(&fx, "task-1"),
        Some(QueueReason::BackendUnknown {
            detail: "backend 'k8s' is not registered".into()
        })
    );
    assert_eq!(fx.control.task(&task("task-1")).unwrap().reservation, None);
}

#[test]
fn which_lifecycles_hold_capacity() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    let in_use = || fx.control.capacity_in_use(&k8s()).unwrap();

    fx.create("task-1");
    assert_eq!(in_use(), 0, "an unadmitted task holds nothing");
    fx.running("task-1", &lease);
    assert_eq!(in_use(), 1);

    fx.command("task-1", OwnerCommand::Pause, "pause-1");
    assert_eq!(in_use(), 1, "pause requested: compute still exists");
    fx.observe("task-1", &lease, Observation::PausedAtSafePoint)
        .unwrap();
    assert_eq!(in_use(), 0, "paused: compute removed at the safe point");

    fx.command("task-1", OwnerCommand::Resume, "resume-1");
    fx.observe(
        "task-1",
        &lease,
        Observation::WorkloadLost { state_known: true },
    )
    .unwrap();
    assert_eq!(in_use(), 1, "interrupted: a workload may still exist");
    fx.command("task-1", OwnerCommand::Archive, "archive-1");
    assert_eq!(in_use(), 1, "archiving: deletion not confirmed yet");
    fx.observe("task-1", &lease, Observation::ResourcesDeleted)
        .unwrap();
    assert_eq!(in_use(), 0, "archived");
}

#[test]
fn archive_releases_an_admitted_or_provisioning_task_deterministically() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    for id in ["admitted", "provisioning", "waiting"] {
        fx.create(id);
    }
    fx.control.schedule(&k8s(), &limit(2)).unwrap();
    report_now(&fx, "provisioning", &lease, Observation::StartAccepted).unwrap();
    assert_eq!(
        reason(&fx, "waiting"),
        Some(QueueReason::WaitingForCapacity {
            position: 1,
            running: 2,
            limit: 2,
        })
    );

    // An admitted task never accepted by a controller: archived at once,
    // its slot free for the next pass.
    let archived = fx.command("admitted", OwnerCommand::Archive, "archive-a");
    assert_eq!(archived.lifecycle, TaskLifecycle::Archived);
    assert_eq!(archived.reservation, None);
    fx.control.schedule(&k8s(), &limit(2)).unwrap();
    assert_eq!(reason(&fx, "waiting"), Some(QueueReason::Admitted));

    // Cancelled provisioning holds its slot until deletion is confirmed.
    fx.command("provisioning", OwnerCommand::Archive, "archive-p");
    fx.create("next");
    fx.control.schedule(&k8s(), &limit(2)).unwrap();
    assert!(matches!(
        reason(&fx, "next"),
        Some(QueueReason::WaitingForCapacity { running: 2, .. })
    ));
    fx.observe("provisioning", &lease, Observation::ResourcesDeleted)
        .unwrap();
    fx.control.schedule(&k8s(), &limit(2)).unwrap();
    assert_eq!(reason(&fx, "next"), Some(QueueReason::Admitted));
}

#[test]
fn concurrent_creation_and_admission_never_exceed_the_limit() {
    let fx = Fixture::new();
    fx.lease("k8s", "controller-a");
    std::thread::scope(|scope| {
        for index in 0..12 {
            let control = fx.reopen();
            scope.spawn(move || {
                control
                    .create_task(new_task(&format!("task-{index}"), "k8s"))
                    .expect("create");
                control.schedule(&k8s(), &limit(3)).expect("schedule");
            });
        }
    });
    let tasks = fx.control.list_tasks().unwrap();
    let admitted: Vec<u64> = tasks
        .iter()
        .filter(|task| task.reservation.is_some())
        .map(|task| task.queue_sequence)
        .collect();
    assert_eq!(admitted.len(), 3, "{admitted:?}");
    assert_eq!(fx.control.capacity_in_use(&k8s()).unwrap(), 3);
}

#[test]
fn a_priority_change_is_idempotent_generation_checked_and_only_for_the_queued() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    fx.create("queued");

    let stale = fx
        .control
        .set_priority(&task("queued"), 3, Some(7))
        .unwrap_err();
    assert!(matches!(stale, OrchestrationError::StaleGeneration { .. }));

    let (record, changed) = fx
        .control
        .set_priority(&task("queued"), 3, Some(1))
        .unwrap();
    assert!(changed);
    assert_eq!((record.priority, record.generation), (3, 1));
    assert_eq!(
        record.last_event.kind,
        TaskEventKind::PriorityChanged { priority: 3 }
    );
    let (again, changed) = fx
        .control
        .set_priority(&task("queued"), 3, Some(1))
        .unwrap();
    assert!(!changed);
    assert_eq!(again.revision, record.revision);

    // AE3: a running task is never reordered, let alone stopped.
    let running = fx.running("running", &lease);
    let refused = fx
        .control
        .set_priority(&task("running"), 10, None)
        .unwrap_err();
    assert!(matches!(
        refused,
        OrchestrationError::TaskNotQueued {
            lifecycle: TaskLifecycle::Running,
            ..
        }
    ));
    assert_eq!(fx.control.task(&task("running")).unwrap(), running);
}

#[test]
fn resuming_a_paused_task_needs_a_free_slot() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    fx.running("paused", &lease);
    fx.command("paused", OwnerCommand::Pause, "pause-1");
    fx.observe("paused", &lease, Observation::PausedAtSafePoint)
        .unwrap();
    fx.running("other", &lease);

    let resume = CommandRequest::new(OwnerCommand::Resume, "resume-1");
    let refused = fx
        .control
        .command_with_capacity(&task("paused"), resume.clone(), &limit(1))
        .unwrap_err();
    assert!(matches!(
        refused,
        OrchestrationError::BackendAtCapacity {
            running: 1,
            limit: 1,
            ..
        }
    ));
    assert_eq!(
        fx.control.task(&task("paused")).unwrap().lifecycle,
        TaskLifecycle::Paused
    );

    let resumed = fx
        .control
        .command_with_capacity(&task("paused"), resume.clone(), &limit(2))
        .expect("a free slot");
    assert_eq!(resumed.task.lifecycle, TaskLifecycle::Provisioning);
    // A retry is answered from its receipt, whatever the capacity now.
    let retried = fx
        .control
        .command_with_capacity(&task("paused"), resume, &limit(1))
        .expect("retry");
    assert!(retried.replayed);

    // Retrying an interrupted task keeps the slot it holds: no check.
    fx.observe(
        "other",
        &lease,
        Observation::WorkloadLost { state_known: true },
    )
    .unwrap();
    fx.control
        .command_with_capacity(
            &task("other"),
            CommandRequest::new(OwnerCommand::Start, "start-1"),
            &limit(2),
        )
        .expect("an interrupted task already holds its slot");
}

#[test]
fn the_default_backend_override_is_durable_and_idempotent() {
    let fx = Fixture::new();
    assert_eq!(fx.control.default_backend().unwrap(), None);
    let first = fx.control.set_default_backend(Some(k8s())).unwrap();
    assert_eq!((first.revision, first.backend.clone()), (1, Some(k8s())));
    assert_eq!(fx.control.set_default_backend(Some(k8s())).unwrap(), first);

    let reopened = fx.reopen();
    assert_eq!(reopened.default_backend().unwrap(), Some(first));
    let cleared = reopened.set_default_backend(None).unwrap();
    assert_eq!((cleared.revision, cleared.backend), (2, None));
}

#[test]
fn an_identical_create_request_returns_the_task_by_its_digest() {
    let fx = Fixture::new();
    let created = fx
        .control
        .create_task(new_task("task-1", "k8s").with_request_digest("sha256:request"))
        .unwrap();
    // The configuration changed: the same request now resolves elsewhere.
    let retried = fx
        .control
        .create_task(new_task("task-1", "backend-b").with_request_digest("sha256:request"))
        .unwrap();
    assert_eq!(retried, created);
    let conflict = fx
        .control
        .create_task(new_task("task-1", "backend-b").with_request_digest("sha256:other"))
        .unwrap_err();
    assert!(matches!(conflict, OrchestrationError::TaskConflict(_)));
}
