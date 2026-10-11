//! Integration tests for remote task orchestration (U1).
//!
//! Like the persistence tests, everything runs against a real temporary
//! state root: the guarantees under test — atomic commits, per-task
//! serialization, recovery after partial writes — live in file operations.

use chrono::{DateTime, Duration, TimeZone, Utc};
use narsor::domain::{BackendId, ExecutionStatus, TaskId, TaskLaunchSpec};
use narsor::orchestration::{
    exhaustion_loss, loss_state, BackendCapacity, BackendLease, Clock, CommandActor,
    CommandRequest, ControlPlane, ControllerReport, LossState, MoveTarget, NewTask, Observation,
    ObservedSummary, OrchestrationError, OwnerCommand, OwnerRef, QueueReason, ResourceIdentity,
    ResumeExhaustion, ResumePolicy, ResumeState, RunnerSnapshot, TaskEventKind, TaskLifecycle,
    TaskRecord, WorkloadLoss,
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

    /// One scheduling pass on the `k8s` backend.
    fn schedule_once(&self) {
        self.control
            .schedule(&backend_id("k8s"), &BackendCapacity::Limit(1))
            .expect("schedule");
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
        git_ref: None,
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
        attention: Vec::new(),
        conductor_available: false,
        conductor_unavailable_reason: None,
        default_harness_unavailable: None,
        display: None,
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
    let second = fx.control.create_task(new_task("task-b", "k8s")).unwrap();

    assert_eq!(first.lifecycle, TaskLifecycle::Queued);
    assert_eq!(first.generation, 1);
    assert_eq!(first.revision, 1);
    assert_eq!(first.rank, 1, "the first task is first in the queue");
    assert_eq!(first.assignment.backend_id, backend_id("k8s"));
    assert_eq!(first.assignment.resource_key, "task-a");
    assert_eq!(first.owner, OwnerRef::new("oidc:https://issuer|artur"));
    assert!(first.observed.is_none());
    assert_eq!(second.rank, 2, "a new task joins the end of the queue");
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

/// The ids of the backend's tasks in queue order, as the control plane lists
/// them.
fn queue_order(fx: &Fixture, backend: &str) -> Vec<String> {
    fx.control
        .list_tasks()
        .unwrap()
        .into_iter()
        .filter(|task| task.assignment.backend_id == backend_id(backend))
        .map(|task| task.task_id.to_string())
        .collect()
}

fn move_to(
    fx: &Fixture,
    id: &str,
    target: MoveTarget,
) -> Result<(TaskRecord, bool), OrchestrationError> {
    fx.control.move_task(&task(id), &target, None)
}

fn before(id: &str) -> MoveTarget {
    MoveTarget::Before(task(id))
}

fn after(id: &str) -> MoveTarget {
    MoveTarget::After(task(id))
}

/// Rewrites a task's record on disk the way a version 1 build stored it: a
/// `priority`, no `rank`.
fn downgrade_to_v1(fx: &Fixture, id: &str, priority: i32) {
    let path = fx.root.paths().orchestration_task_file(id);
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let record = value.as_object_mut().unwrap();
    record.insert("record_version".into(), 1.into());
    record.remove("rank");
    record.insert("priority".into(), priority.into());
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
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
fn at_capacity_rank_orders_the_waiting_and_a_move_reorders_them_without_preemption() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    fx.create("running");
    fx.control.schedule(&k8s(), &limit(1)).unwrap();
    report_now(&fx, "running", &lease, Observation::StartAccepted).unwrap();
    report_now(&fx, "running", &lease, Observation::RunnerReady).unwrap();

    fx.create("first");
    fx.create("second");
    fx.create("third");
    fx.control.schedule(&k8s(), &limit(1)).unwrap();

    let waiting = |position| QueueReason::WaitingForCapacity {
        position,
        running: 1,
        limit: 1,
    };
    assert_eq!(reason(&fx, "first"), Some(waiting(1)));
    assert_eq!(reason(&fx, "second"), Some(waiting(2)));
    assert_eq!(reason(&fx, "third"), Some(waiting(3)));
    // A full backend never stops what runs (R11).
    let running = fx.control.task(&task("running")).unwrap();
    assert_eq!(running.lifecycle, TaskLifecycle::Running);
    assert_eq!(running.generation, 1);

    // A repeated pass over an unchanged backend writes nothing.
    assert!(fx.control.schedule(&k8s(), &limit(1)).unwrap().is_empty());

    // Moving the last task to the top puts it at the front of the waiting.
    let (_, changed) = move_to(&fx, "third", MoveTarget::Top).expect("move");
    assert!(changed);
    fx.control.schedule(&k8s(), &limit(1)).unwrap();
    assert_eq!(reason(&fx, "third"), Some(waiting(1)));
    assert_eq!(reason(&fx, "first"), Some(waiting(2)));
    assert_eq!(reason(&fx, "second"), Some(waiting(3)));

    // One more slot: exactly the first in line is admitted.
    let admitted = fx.control.schedule(&k8s(), &limit(2)).unwrap();
    assert!(admitted.contains(&task("third")));
    assert_eq!(reason(&fx, "third"), Some(QueueReason::Admitted));
    assert_eq!(
        reason(&fx, "first"),
        Some(QueueReason::WaitingForCapacity {
            position: 1,
            running: 2,
            limit: 2,
        })
    );

    // Lowering the limit again takes no reservation back.
    fx.control.schedule(&k8s(), &limit(1)).unwrap();
    assert_eq!(reason(&fx, "third"), Some(QueueReason::Admitted));
    assert_eq!(fx.control.capacity_in_use(&k8s()).unwrap(), 2);
    assert!(matches!(
        report_now(&fx, "first", &lease, Observation::StartAccepted),
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
fn a_move_places_a_queued_task_before_after_at_the_top_or_at_the_bottom() {
    let fx = Fixture::new();
    for id in ["a", "b", "c"] {
        fx.create(id);
    }
    assert_eq!(queue_order(&fx, "k8s"), ["a", "b", "c"]);

    let (record, changed) = move_to(&fx, "c", MoveTarget::Top).unwrap();
    assert!(changed);
    assert_eq!((record.rank, record.generation), (1, 1));
    assert_eq!(queue_order(&fx, "k8s"), ["c", "a", "b"]);
    assert_eq!(
        record.last_event.kind,
        TaskEventKind::Moved {
            rank: 1,
            order: vec![task("c"), task("a"), task("b")],
        },
        "the event carries the whole new order"
    );

    move_to(&fx, "a", after("b")).unwrap();
    assert_eq!(queue_order(&fx, "k8s"), ["c", "b", "a"]);
    move_to(&fx, "a", before("b")).unwrap();
    assert_eq!(queue_order(&fx, "k8s"), ["c", "a", "b"]);
    move_to(&fx, "c", MoveTarget::Bottom).unwrap();
    assert_eq!(queue_order(&fx, "k8s"), ["a", "b", "c"]);

    // Ranks stay dense and agree with the listing.
    let ranks: Vec<u32> = fx
        .control
        .list_tasks()
        .unwrap()
        .iter()
        .map(|task| task.rank)
        .collect();
    assert_eq!(ranks, [1, 2, 3]);
}

#[test]
fn a_move_that_changes_nothing_writes_and_announces_nothing() {
    let fx = Fixture::new();
    fx.create("a");
    fx.create("b");
    let revision = fx.control.task(&task("b")).unwrap().revision;

    for target in [MoveTarget::Bottom, after("a")] {
        let (record, changed) = move_to(&fx, "b", target).unwrap();
        assert!(!changed);
        assert_eq!(record.revision, revision);
    }
    let (_, changed) = move_to(&fx, "a", MoveTarget::Top).unwrap();
    assert!(!changed);
    assert_eq!(fx.control.events_since(&task("b"), 0).unwrap().len(), 1);
}

#[test]
fn a_new_task_joins_the_end_of_its_own_backends_queue() {
    let fx = Fixture::new();
    fx.create("a");
    fx.control.create_task(new_task("other", "docker")).unwrap();
    let b = fx.create("b");

    assert_eq!(b.rank, 2, "ranks count per backend");
    assert_eq!(fx.control.task(&task("other")).unwrap().rank, 1);
    let waiting = |fx: &Fixture, id: &str| match reason(fx, id) {
        Some(QueueReason::WaitingForCapacity { position, .. }) => position,
        other => panic!("{id} should wait, got {other:?}"),
    };
    let _lease = fx.lease("k8s", "controller-a");
    fx.control.schedule(&k8s(), &limit(0)).unwrap();
    assert_eq!(waiting(&fx, "b"), 2, "a new task waits at place #2");
}

#[test]
fn tasks_that_hold_a_slot_or_finished_keep_their_place_when_a_queued_one_passes_them() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    fx.running("running", &lease);
    fx.create("queued");
    fx.create("late");
    fx.command("running", OwnerCommand::Archive, "archive-1");
    fx.observe("running", &lease, Observation::ResourcesDeleted)
        .unwrap();
    assert_eq!(queue_order(&fx, "k8s"), ["running", "queued", "late"]);
    let archived = fx.control.task(&task("running")).unwrap();

    move_to(&fx, "late", MoveTarget::Top).unwrap();
    assert_eq!(queue_order(&fx, "k8s"), ["late", "running", "queued"]);
    move_to(&fx, "late", MoveTarget::Bottom).unwrap();
    assert_eq!(queue_order(&fx, "k8s"), ["running", "queued", "late"]);
    let finished = fx.control.task(&task("running")).unwrap();
    assert_eq!(finished.lifecycle, TaskLifecycle::Archived);
    assert_eq!(
        finished.revision, archived.revision,
        "a task that was passed gets no event"
    );
}

#[test]
fn the_waiting_position_counts_only_queued_tasks() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    fx.running("running", &lease);
    fx.create("queued");
    fx.control.schedule(&k8s(), &limit(1)).unwrap();
    assert_eq!(
        reason(&fx, "queued"),
        Some(QueueReason::WaitingForCapacity {
            position: 1,
            running: 1,
            limit: 1
        }),
        "the running task ranks first but is not in line"
    );
}

#[test]
fn a_move_is_generation_checked_and_only_for_the_queued() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    fx.create("queued");
    fx.create("other");

    let stale = fx
        .control
        .move_task(&task("queued"), &MoveTarget::Bottom, Some(7))
        .unwrap_err();
    assert!(matches!(stale, OrchestrationError::StaleGeneration { .. }));
    assert_eq!(queue_order(&fx, "k8s"), ["queued", "other"]);

    let (record, changed) = fx
        .control
        .move_task(&task("queued"), &MoveTarget::Bottom, Some(1))
        .unwrap();
    assert!(changed);
    assert_eq!(record.generation, 1, "the desired generation does not move");

    // AE3: a running task is never reordered, let alone stopped.
    let running = fx.running("running", &lease);
    let refused = move_to(&fx, "running", MoveTarget::Top).unwrap_err();
    assert!(matches!(
        refused,
        OrchestrationError::TaskNotQueued {
            lifecycle: TaskLifecycle::Running,
            ..
        }
    ));
    assert_eq!(fx.control.task(&task("running")).unwrap(), running);
    assert_eq!(queue_order(&fx, "k8s"), ["other", "queued", "running"]);
}

#[test]
fn a_move_refuses_an_anchor_that_is_missing_foreign_or_the_task_itself() {
    let fx = Fixture::new();
    fx.create("a");
    fx.create("b");
    fx.control
        .create_task(new_task("elsewhere", "docker"))
        .unwrap();

    for anchor in ["missing", "elsewhere", "a"] {
        let error = move_to(&fx, "a", before(anchor)).unwrap_err();
        assert!(
            matches!(error, OrchestrationError::InvalidMoveAnchor { .. }),
            "{anchor}: {error:?}"
        );
    }
    let error = move_to(&fx, "missing", MoveTarget::Top).unwrap_err();
    assert!(matches!(error, OrchestrationError::TaskNotFound(_)));
    assert_eq!(queue_order(&fx, "k8s"), ["a", "b"]);
    assert_eq!(fx.control.task(&task("a")).unwrap().revision, 1);
}

#[test]
fn concurrent_moves_are_serialized_and_leave_one_consistent_order() {
    let fx = Fixture::new();
    for id in ["a", "b", "c", "d"] {
        fx.create(id);
    }
    let handles: Vec<_> = [("d", MoveTarget::Top), ("a", MoveTarget::Bottom)]
        .into_iter()
        .map(|(id, target)| {
            let control = fx.reopen();
            std::thread::spawn(move || control.move_task(&task(id), &target, None).unwrap())
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }

    let order = queue_order(&fx, "k8s");
    assert_eq!(order, ["d", "b", "c", "a"]);
    let mut ranks: Vec<u32> = fx
        .control
        .list_tasks()
        .unwrap()
        .iter()
        .map(|t| t.rank)
        .collect();
    ranks.sort_unstable();
    assert_eq!(ranks, [1, 2, 3, 4], "the ranks stay dense");
    // Each move announced the order it produced; the later one is final.
    let events: Vec<_> = ["d", "a"]
        .iter()
        .map(|id| fx.control.task(&task(id)).unwrap().last_event.kind)
        .collect();
    let finals = events.iter().filter(|kind| {
        matches!(kind, TaskEventKind::Moved { order, .. } if order.iter().map(ToString::to_string).collect::<Vec<_>>() == ["d", "b", "c", "a"])
    });
    assert_eq!(finals.count(), 1, "{events:?}");
}

/// Makes writing `id`'s rank fail: its lock file is a directory.
fn break_task_lock(fx: &Fixture, id: &str) {
    let lock = fx.root.paths().orchestration_task_lock_file(id);
    let _ = fs::remove_file(&lock);
    fs::create_dir(&lock).unwrap();
}

fn stored_ranks(fx: &Fixture) -> Vec<(String, u32)> {
    let mut ranks: Vec<(String, u32)> = fx
        .control
        .list_tasks()
        .unwrap()
        .iter()
        .map(|task| (task.task_id.to_string(), task.rank))
        .collect();
    ranks.sort();
    ranks
}

#[test]
fn a_move_that_fails_after_renumbering_a_neighbour_restores_the_queue() {
    let fx = Fixture::new();
    for id in ["a", "b", "c"] {
        fx.create(id);
    }
    // `a` is renumbered first, then writing `b` fails.
    break_task_lock(&fx, "b");
    move_to(&fx, "c", MoveTarget::Top).unwrap_err();

    assert_eq!(queue_order(&fx, "k8s"), ["a", "b", "c"]);
    assert_eq!(
        stored_ranks(&fx),
        [("a".into(), 1), ("b".into(), 2), ("c".into(), 3)]
    );
    assert_eq!(
        fx.control.events_since(&task("c"), 0).unwrap().len(),
        1,
        "nothing was announced"
    );

    // The same move goes through once the fault is gone.
    fs::remove_dir(fx.root.paths().orchestration_task_lock_file("b")).unwrap();
    move_to(&fx, "c", MoveTarget::Top).unwrap();
    assert_eq!(queue_order(&fx, "k8s"), ["c", "a", "b"]);
}

#[test]
fn equal_ranks_left_by_an_interrupted_renumbering_are_made_dense_by_the_next_pass() {
    let fx = Fixture::new();
    for id in ["a", "b", "c"] {
        fx.create(id);
    }
    // What a crash in the middle of a renumbering leaves: c and b tie.
    for (id, rank) in [("a", 1), ("b", 2), ("c", 2)] {
        let path = fx.root.paths().orchestration_task_file(id);
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["rank"] = rank.into();
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    }
    fx.schedule_once();
    assert_eq!(
        stored_ranks(&fx),
        [("a".into(), 1), ("b".into(), 2), ("c".into(), 3)]
    );
    assert_eq!(queue_order(&fx, "k8s"), ["a", "b", "c"]);
}

#[test]
fn recovery_makes_equal_ranks_dense() {
    let fx = Fixture::new();
    for id in ["a", "b"] {
        fx.create(id);
    }
    let path = fx.root.paths().orchestration_task_file("b");
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["rank"] = 1.into();
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

    fx.reopen().recover().unwrap();
    assert_eq!(stored_ranks(&fx), [("a".into(), 1), ("b".into(), 2)]);
}

#[test]
fn a_read_between_the_neighbour_writes_and_the_commit_never_ends_in_another_order() {
    let fx = Fixture::new();
    for id in ["a", "b", "c"] {
        fx.create(id);
    }
    // The state a reader can meet mid-move: the neighbours renumbered, the
    // moved task not yet committed.
    for (id, rank) in [("a", 2), ("b", 3)] {
        let path = fx.root.paths().orchestration_task_file(id);
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["rank"] = rank.into();
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    }
    let _ = fx.control.list_tasks().unwrap();
    move_to(&fx, "c", MoveTarget::Top).unwrap();
    assert_eq!(queue_order(&fx, "k8s"), ["c", "a", "b"]);
    assert_eq!(
        stored_ranks(&fx),
        [("a".into(), 2), ("b".into(), 3), ("c".into(), 1)]
    );
}

#[test]
fn a_move_event_names_only_the_movers_tasks_but_ranks_the_whole_queue() {
    let fx = Fixture::new();
    fx.create("mine-1");
    fx.control
        .create_task(NewTask {
            owner: OwnerRef::new("someone-else"),
            ..new_task("theirs", "k8s")
        })
        .unwrap();
    fx.create("mine-2");

    let (record, _) = move_to(&fx, "mine-2", MoveTarget::Top).unwrap();
    assert_eq!(
        record.last_event.kind,
        TaskEventKind::Moved {
            rank: 1,
            order: vec![task("mine-2"), task("mine-1")],
        }
    );
    assert_eq!(queue_order(&fx, "k8s"), ["mine-2", "mine-1", "theirs"]);
    let (record, _) = move_to(&fx, "mine-1", MoveTarget::Top).unwrap();
    assert!(matches!(
        record.last_event.kind,
        TaskEventKind::Moved { rank: 1, .. }
    ));
    let (record, _) = move_to(&fx, "mine-2", after("theirs")).unwrap();
    assert!(matches!(
        record.last_event.kind,
        TaskEventKind::Moved { rank: 3, .. }
    ));
}

#[test]
fn version_one_records_are_numbered_once_by_priority_then_arrival() {
    let fx = Fixture::new();
    for id in ["first", "second", "third"] {
        fx.create(id);
    }
    // Priorities 5, 0, 0 become: the high one leads, the rest keep FIFO.
    downgrade_to_v1(&fx, "first", 0);
    downgrade_to_v1(&fx, "second", 0);
    downgrade_to_v1(&fx, "third", 5);

    let reopened = fx.reopen();
    let order: Vec<(String, u32)> = reopened
        .list_tasks()
        .unwrap()
        .iter()
        .map(|task| (task.task_id.to_string(), task.rank))
        .collect();
    assert_eq!(
        order,
        [
            ("third".into(), 1),
            ("first".into(), 2),
            ("second".into(), 3)
        ]
    );
    let record = reopened.task(&task("third")).unwrap();
    assert_eq!(record.record_version, 2);

    // A second read changes nothing, on disk or in memory.
    let path = fx.root.paths().orchestration_task_file("third");
    let stored = fs::read(&path).unwrap();
    assert_eq!(fx.reopen().list_tasks().unwrap().len(), 3);
    assert_eq!(fs::read(&path).unwrap(), stored);
    let json: serde_json::Value = serde_json::from_slice(&stored).unwrap();
    assert!(
        json.get("priority").is_none(),
        "priority is never written back"
    );

    // The migrated queue admits by the new rank, and takes new tasks last.
    let _lease = reopened
        .acquire_lease(&k8s(), "controller-a", Duration::minutes(5))
        .unwrap();
    reopened.schedule(&k8s(), &limit(1)).unwrap();
    assert_eq!(
        reopened.task(&task("third")).unwrap().queue.unwrap().reason,
        QueueReason::Admitted
    );
    assert_eq!(
        reopened
            .create_task(new_task("fourth", "k8s"))
            .unwrap()
            .rank,
        4
    );
}

#[test]
fn resuming_an_interrupted_migration_keeps_the_migrated_tasks_in_front() {
    let fx = Fixture::new();
    for id in ["first", "second", "third"] {
        fx.create(id);
    }
    // The migration numbered the high priority task first and died: it is at
    // version 2 with rank 1, the other two are still version 1.
    downgrade_to_v1(&fx, "first", 0);
    downgrade_to_v1(&fx, "second", 0);
    let path = fx.root.paths().orchestration_task_file("third");
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["rank"] = 1.into();
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

    let reopened = fx.reopen();
    let order: Vec<(String, u32)> = reopened
        .list_tasks()
        .unwrap()
        .iter()
        .map(|task| (task.task_id.to_string(), task.rank))
        .collect();
    assert_eq!(
        order,
        [
            ("third".into(), 1),
            ("first".into(), 2),
            ("second".into(), 3)
        ]
    );
}

#[test]
fn a_version_one_record_that_appears_after_the_backend_was_remembered_does_not_deadlock() {
    let fx = Fixture::new();
    fx.create("a");
    fx.create("b");
    let control = fx.reopen();
    // Remembers the backend as migrated.
    control.schedule(&k8s(), &limit(1)).unwrap();
    downgrade_to_v1(&fx, "b", 0);

    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = control.clone();
    std::thread::spawn(move || {
        let first = worker.schedule(&k8s(), &limit(1)).map(|_| ());
        let second = worker.schedule(&k8s(), &limit(1)).map(|_| ());
        let _ = sender.send((first, second));
    });
    let (first, second) = receiver
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("the pass must not wait on a lock it holds");
    assert!(
        matches!(first, Err(OrchestrationError::LegacyRecordAppeared { .. })),
        "{first:?}"
    );
    second.expect("the retry migrates the backend first");
    assert_eq!(control.task(&task("b")).unwrap().record_version, 2);
}

#[test]
fn a_task_started_under_version_one_is_migrated_before_it_is_mutated() {
    let fx = Fixture::new();
    fx.create("old");
    downgrade_to_v1(&fx, "old", 3);

    let reopened = fx.reopen();
    let record = reopened
        .command(
            &task("old"),
            CommandRequest::new(OwnerCommand::Archive, "archive-1"),
        )
        .unwrap()
        .task;
    assert_eq!((record.record_version, record.rank), (2, 1));
}

#[test]
fn a_priority_changed_event_from_an_old_log_still_reads() {
    let fx = Fixture::new();
    fx.create("old");
    let kind: TaskEventKind =
        serde_json::from_value(serde_json::json!({ "type": "priority_changed", "priority": 3 }))
            .expect("an old event deserializes");
    assert_eq!(kind, TaskEventKind::PriorityChanged { priority: 3 });

    // The same event inside a stored log does not break reading it.
    for path in [
        fx.root.paths().orchestration_task_file("old"),
        fx.root.paths().orchestration_task_event_file("old", 1),
    ] {
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let event = if path.ends_with("task.json") {
            &mut value["last_event"]
        } else {
            &mut value
        };
        event["kind"] = serde_json::json!({ "type": "priority_changed", "priority": 3 });
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    }
    let events = fx.reopen().events_since(&task("old"), 0).unwrap();
    assert_eq!(
        events[0].kind,
        TaskEventKind::PriorityChanged { priority: 3 }
    );
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

// ---------------------------------------------------------------------------
// Operator commands (U3)
// ---------------------------------------------------------------------------

fn operator_request(
    command: narsor::orchestration::OperatorCommand,
    key: &str,
) -> narsor::orchestration::OperatorCommandRequest {
    narsor::orchestration::OperatorCommandRequest {
        command,
        idempotency_key: key.to_string(),
        expected_generation: None,
        requested_by: OwnerRef::new("oidc:https://issuer|artur"),
    }
}

/// A running task whose runner `pod-1` of launch generation 1 has reported.
fn running_with_runner(fx: &Fixture, id: &str, lease: &BackendLease) {
    fx.running(id, lease);
    fx.control
        .record_runner_snapshot(&task(id), "pod-1", 1, safe_point_snapshot(1))
        .expect("the runner's first snapshot");
}

#[test]
fn an_operator_command_is_bound_to_the_launch_it_was_queued_for() {
    use narsor::orchestration::{OperatorCommand, OperatorCommandStatus};
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    running_with_runner(&fx, "task-1", &lease);

    let queued = fx
        .control
        .queue_operator_command(
            &task("task-1"),
            operator_request(OperatorCommand::Interrupt, "i-1"),
        )
        .expect("queued");
    assert_eq!(queued.command.launch_generation, 1);

    // The runner is paused away and a new launch takes over before anyone
    // delivered the command.
    fx.command("task-1", OwnerCommand::Pause, "pause-1");
    fx.observe("task-1", &lease, Observation::PausedAtSafePoint)
        .unwrap();
    fx.command("task-1", OwnerCommand::Resume, "resume-1");
    fx.observe("task-1", &lease, Observation::RunnerReady)
        .unwrap();
    let generation = fx.control.task(&task("task-1")).unwrap().generation;
    assert!(generation > 1);

    let (delivered, changed) = fx
        .control
        .deliver_operator_commands(&task("task-1"), "pod-2", generation)
        .expect("poll of the new launch");

    assert!(delivered.is_empty(), "a command never crosses launches");
    assert!(changed);
    let record = fx.control.task(&task("task-1")).unwrap();
    assert_eq!(
        record.operator_commands[0].status,
        OperatorCommandStatus::Rejected
    );
}

#[test]
fn a_superseded_runner_can_neither_poll_nor_settle_operator_commands() {
    use narsor::orchestration::OperatorCommand;
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    running_with_runner(&fx, "task-1", &lease);
    let queued = fx
        .control
        .queue_operator_command(
            &task("task-1"),
            operator_request(OperatorCommand::Interrupt, "i-1"),
        )
        .expect("queued");
    fx.command("task-1", OwnerCommand::Pause, "pause-1");
    fx.observe("task-1", &lease, Observation::PausedAtSafePoint)
        .unwrap();
    fx.command("task-1", OwnerCommand::Resume, "resume-1");

    let poll = fx
        .control
        .deliver_operator_commands(&task("task-1"), "pod-1", 1);
    let settle = fx.control.settle_operator_command(
        &task("task-1"),
        "pod-1",
        1,
        &queued.command.command_id,
        true,
        None,
    );

    assert!(matches!(
        poll,
        Err(OrchestrationError::RunnerSuperseded { .. })
    ));
    assert!(matches!(
        settle,
        Err(OrchestrationError::RunnerSuperseded { .. })
    ));
}

#[test]
fn a_task_record_written_before_operator_commands_still_reads() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    fx.running("task-1", &lease);
    let path = fx.root.paths().orchestration_task_file("task-1");
    let mut value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .remove("operator_commands")
        .expect("the field is written");
    fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();

    let record = fx
        .control
        .task(&task("task-1"))
        .expect("an older record reads");

    assert!(record.operator_commands.is_empty());
}

#[test]
fn a_runner_detail_is_cleaned_and_bounded_when_a_command_is_settled() {
    use narsor::orchestration::OperatorCommand;
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    running_with_runner(&fx, "task-1", &lease);
    let queued = fx
        .control
        .queue_operator_command(
            &task("task-1"),
            operator_request(OperatorCommand::Interrupt, "i-1"),
        )
        .expect("queued");
    fx.control
        .deliver_operator_commands(&task("task-1"), "pod-1", 1)
        .expect("delivered");
    let hostile = format!("bad\u{1b}[31m\u{202e}news{}", "é".repeat(5_000));

    let (settled, _) = fx
        .control
        .settle_operator_command(
            &task("task-1"),
            "pod-1",
            1,
            &queued.command.command_id,
            false,
            Some(hostile),
        )
        .expect("settled");

    let detail = settled.detail.expect("the detail is kept");
    assert!(detail.starts_with("bad[31mnews"), "{detail}");
    assert!(!detail.contains('\u{1b}') && !detail.contains('\u{202e}'));
    assert_eq!(detail.chars().count(), 2_000);
}

/// A running task whose runner holds the commands of launch generation 1.
fn running_task_for_commands() -> Fixture {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    running_with_runner(&fx, "task-1", &lease);
    fx
}

fn queue_chat(fx: &Fixture, key: &str, text: &str) -> Result<String, OrchestrationError> {
    use narsor::orchestration::OperatorCommand;
    fx.control
        .queue_operator_command(
            &task("task-1"),
            operator_request(
                OperatorCommand::ConductorChat {
                    text: text.to_string(),
                },
                key,
            ),
        )
        .map(|outcome| outcome.command.command_id)
}

fn settle(fx: &Fixture, command_id: &str) {
    fx.control
        .settle_operator_command(&task("task-1"), "pod-1", 1, command_id, true, None)
        .expect("settled");
}

#[test]
fn settled_commands_are_pruned_ids_stay_unique_and_chat_text_is_dropped() {
    let fx = running_task_for_commands();
    let mut ids = Vec::new();
    for number in 1..=30 {
        let id = queue_chat(&fx, &format!("chat-{number}"), "a long message").unwrap();
        settle(&fx, &id);
        ids.push(id);
    }
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), 30, "an id is never given twice: {ids:?}");
    assert_eq!(ids[29], "command-30");

    let record = fx.control.task(&task("task-1")).unwrap();
    assert_eq!(
        record.operator_commands.len(),
        20,
        "the last 20 settled ones"
    );
    assert_eq!(record.operator_commands[0].command_id, "command-11");
    assert!(record.operator_commands.iter().all(|command| matches!(
        &command.command,
        narsor::orchestration::OperatorCommand::ConductorChat { text } if text.is_empty()
    )));

    // A command still open is never forgotten, and the next id goes on.
    let open = queue_chat(&fx, "chat-31", "still waiting").unwrap();
    assert_eq!(open, "command-31");
    for number in 32..=60 {
        let id = queue_chat(&fx, &format!("chat-{number}"), "x").unwrap();
        settle(&fx, &id);
    }
    let record = fx.control.task(&task("task-1")).unwrap();
    assert!(record.operator_command("command-31").is_some());
    assert_eq!(
        record.operator_command("command-31").unwrap().command_id,
        open
    );
}

#[test]
fn retrying_a_command_key_is_safe_whether_the_command_was_kept_or_forgotten() {
    let fx = running_task_for_commands();
    for number in 1..=25 {
        let id = queue_chat(&fx, &format!("chat-{number}"), "hello").unwrap();
        settle(&fx, &id);
    }
    // Kept (settled, its text dropped): the retry gets the original back.
    let again = fx
        .control
        .queue_operator_command(
            &task("task-1"),
            operator_request(
                narsor::orchestration::OperatorCommand::ConductorChat {
                    text: "hello".into(),
                },
                "chat-25",
            ),
        )
        .unwrap();
    assert!(again.replayed);
    assert_eq!(again.command.command_id, "command-25");
    // Forgotten: refused, not carried out a second time.
    let error = queue_chat(&fx, "chat-1", "hello").unwrap_err();
    assert!(
        matches!(error, OrchestrationError::OperatorCommandKeyReused { .. }),
        "{error:?}"
    );
    assert_eq!(
        fx.control
            .task(&task("task-1"))
            .unwrap()
            .operator_commands
            .len(),
        20
    );
}

#[test]
fn too_many_unsettled_commands_refuse_a_new_one() {
    let fx = running_task_for_commands();
    for number in 1..=20 {
        queue_chat(&fx, &format!("chat-{number}"), "hello").unwrap();
    }
    let error = queue_chat(&fx, "chat-21", "hello").unwrap_err();
    assert!(
        matches!(error, OrchestrationError::OperatorCommandRefused { .. }),
        "{error:?}"
    );
    // A retry of one that is queued still gets its answer.
    assert!(queue_chat(&fx, "chat-3", "hello").is_ok());
}

#[test]
fn command_ids_of_a_record_without_a_counter_continue_above_its_highest() {
    let fx = running_task_for_commands();
    for number in 1..=3 {
        let id = queue_chat(&fx, &format!("chat-{number}"), "hi").unwrap();
        settle(&fx, &id);
    }
    // A record written before the counter existed.
    let path = fx.root.paths().orchestration_task_file("task-1");
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let object = value.as_object_mut().unwrap();
    object.remove("operator_command_counter");
    object.remove("retired_command_keys");
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

    assert_eq!(queue_chat(&fx, "chat-4", "hi").unwrap(), "command-4");
}

#[test]
fn a_runner_poll_with_nothing_to_reject_reads_and_never_writes() {
    let fx = running_task_for_commands();
    let id = queue_chat(&fx, "chat-1", "hello").unwrap();
    // A write would need the task's lock, which is made unobtainable: the
    // wake of a runner by someone else's change must not reach for it.
    break_task_lock(&fx, "task-1");
    let revision = fx.control.task(&task("task-1")).unwrap().revision;

    for _ in 0..3 {
        let (commands, changed) = fx
            .control
            .deliver_operator_commands(&task("task-1"), "pod-1", 1)
            .expect("a read-only poll");
        assert!(!changed);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].command_id, id);
    }
    assert_eq!(fx.control.task(&task("task-1")).unwrap().revision, revision);
}

/// The per-task conductor harness is retired, but an event log written while
/// it existed still reads.
#[test]
fn an_old_event_log_with_a_conductor_harness_command_still_decodes() {
    let line = r#"{
        "sequence": 7,
        "task_id": "task-1",
        "at": "2026-10-01T12:00:00Z",
        "lifecycle": "running",
        "generation": 3,
        "kind": {
            "type": "operator_command_queued",
            "command_id": "command-1",
            "command": { "kind": "set_conductor_harness", "harness": "codex" },
            "requested_by": "local"
        }
    }"#;
    let event: narsor::orchestration::TaskEvent =
        serde_json::from_str(line).expect("an old event decodes");
    assert_eq!(
        event.kind,
        TaskEventKind::OperatorCommandQueued {
            command_id: "command-1".to_string(),
            command: narsor::orchestration::OperatorCommand::SetConductorHarness {
                harness: "codex".to_string(),
            },
            requested_by: OwnerRef::new("local"),
        }
    );
}

// ---------------------------------------------------------------------------
// Automatic resume: model and lifecycle
// ---------------------------------------------------------------------------

/// Drives a running task to `Unknown`: its controller lost track of the
/// workload.
fn unknown(fx: &Fixture, id: &str, lease: &BackendLease) -> TaskRecord {
    fx.running(id, lease);
    fx.observe(id, lease, Observation::WorkloadLost { state_known: false })
        .expect("uncertain loss")
}

#[test]
fn a_confirmed_loss_moves_an_unknown_task_to_interrupted() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    let before = unknown(&fx, "task-1", &lease);
    assert_eq!(before.lifecycle, TaskLifecycle::Unknown);

    let confirmed = fx
        .observe("task-1", &lease, Observation::LossConfirmed)
        .expect("a confirmation on Unknown is legal");

    assert_eq!(confirmed.lifecycle, TaskLifecycle::Interrupted);
    assert_eq!(
        confirmed.generation, before.generation,
        "no new desired work"
    );
    assert!(
        confirmed.commands.is_empty(),
        "nothing resumes by itself yet"
    );
    assert_eq!(
        confirmed.last_event.kind,
        TaskEventKind::ControllerReported {
            observation: Observation::LossConfirmed,
            lease_epoch: lease.epoch,
        }
    );
    assert_eq!(
        confirmed
            .observed
            .as_ref()
            .map(|observed| observed.observation),
        Some(Observation::LossConfirmed)
    );
}

#[test]
fn a_confirmed_loss_changes_no_other_lifecycle() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");

    fx.running("running", &lease);

    fx.running("paused", &lease);
    fx.command("paused", OwnerCommand::Pause, "pause-1");
    fx.observe("paused", &lease, Observation::PausedAtSafePoint)
        .expect("paused");

    fx.running("interrupted", &lease);
    fx.observe(
        "interrupted",
        &lease,
        Observation::WorkloadLost { state_known: true },
    )
    .expect("interrupted");

    fx.running("archiving", &lease);
    fx.command("archiving", OwnerCommand::Archive, "archive-1");

    for (id, lifecycle) in [
        ("running", TaskLifecycle::Running),
        ("paused", TaskLifecycle::Paused),
        ("interrupted", TaskLifecycle::Interrupted),
        ("archiving", TaskLifecycle::Archiving),
    ] {
        let before = fx.control.task(&task(id)).unwrap();
        assert_eq!(before.lifecycle, lifecycle, "{id} set up");
        let after = fx
            .observe(id, &lease, Observation::LossConfirmed)
            .unwrap_or_else(|error| panic!("{id}: a late confirmation is not an error: {error}"));
        assert_eq!(after.lifecycle, lifecycle, "{id} stays where it was");
        assert_eq!(after.generation, before.generation, "{id} gets no new work");
        assert_eq!(after.commands, before.commands, "{id} is not resumed");
    }
}

#[test]
fn a_repeated_loss_confirmation_is_absorbed_as_a_redelivery() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    unknown(&fx, "task-1", &lease);
    let first = fx
        .observe("task-1", &lease, Observation::LossConfirmed)
        .unwrap();

    let generation = first.generation;
    let outcome = fx
        .control
        .report(report(
            "task-1",
            &lease,
            generation,
            Observation::LossConfirmed,
            None,
        ))
        .expect("a redelivery is accepted");

    assert!(!outcome.applied);
    assert_eq!(outcome.task.revision, first.revision);
    assert_eq!(outcome.task.lifecycle, TaskLifecycle::Interrupted);
}

#[test]
fn an_observation_this_version_does_not_know_is_refused_in_a_report() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    let before = unknown(&fx, "task-1", &lease);

    let error = fx
        .observe("task-1", &lease, Observation::Unrecognized)
        .unwrap_err();

    assert!(matches!(
        error,
        OrchestrationError::IllegalObservation {
            observation: Observation::Unrecognized,
            lifecycle: TaskLifecycle::Unknown,
            ..
        }
    ));
    let after = fx.control.task(&task("task-1")).unwrap();
    assert_eq!(after.revision, before.revision, "nothing was written");
}

#[test]
fn a_new_task_has_an_empty_resume_state_and_owner_receipts() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    let running = fx.running("task-1", &lease);
    assert!(running.resume.is_empty());
    assert_eq!(running.resume, ResumeState::default());
    assert_eq!(
        running.running_since,
        Some(fx.clock.now()),
        "set when the runner reported ready"
    );

    let paused = fx.command("task-1", OwnerCommand::Pause, "pause-1");
    assert_eq!(paused.commands[0].actor, CommandActor::Owner);
}

#[test]
fn a_receipt_records_the_actor_that_issued_the_command() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    fx.running("task-1", &lease);
    fx.observe(
        "task-1",
        &lease,
        Observation::WorkloadLost { state_known: true },
    )
    .unwrap();

    let outcome = fx
        .control
        .command(
            &task("task-1"),
            CommandRequest::new(OwnerCommand::Resume, "auto-resume:1")
                .expecting_generation(1)
                .issued_by(CommandActor::Policy),
        )
        .expect("a policy resume");

    assert_eq!(outcome.receipt.actor, CommandActor::Policy);
    let stored = fx.reopen().task(&task("task-1")).unwrap();
    assert_eq!(
        stored.receipt("auto-resume:1").unwrap().actor,
        CommandActor::Policy
    );
}

#[test]
fn the_policy_resumes_only_a_task_whose_attempt_is_due() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    fx.running("task-1", &lease);
    let refused = fx.control.auto_resume(&task("task-1"), 1);
    assert!(
        matches!(refused, Err(OrchestrationError::IllegalCommand { .. })),
        "a running task: {refused:?}"
    );

    lose(&fx, "task-1", &lease, true, "gone");
    policy_resume(&fx, "task-1");
    lose(&fx, "task-1", &lease, true, "gone again");
    let record = fx.control.task(&task("task-1")).unwrap();
    assert!(record.resume.next_attempt_at.unwrap() > fx.control.now());
    let early = fx.control.auto_resume(&task("task-1"), record.generation);
    assert!(
        matches!(early, Err(OrchestrationError::IllegalCommand { .. })),
        "before its delay: {early:?}"
    );
    // The owner is never held back by the policy's schedule.
    fx.control
        .command(
            &task("task-1"),
            CommandRequest::new(OwnerCommand::Resume, "resume-1"),
        )
        .expect("an owner resume");
}

#[test]
fn a_task_record_written_before_automatic_resume_still_reads() {
    let fx = Fixture::new();
    let lease = fx.lease("k8s", "controller-a");
    fx.running("task-1", &lease);
    fx.observe(
        "task-1",
        &lease,
        Observation::WorkloadLost { state_known: true },
    )
    .unwrap();
    fx.command("task-1", OwnerCommand::Start, "retry-1");
    let path = fx.root.paths().orchestration_task_file("task-1");
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let object = value.as_object_mut().unwrap();
    object.remove("resume").expect("the field is written");
    object
        .remove("running_since")
        .expect("the field is written");
    for receipt in object["commands"].as_array_mut().unwrap() {
        receipt
            .as_object_mut()
            .unwrap()
            .remove("actor")
            .expect("the field is written");
    }
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

    let record = fx
        .reopen()
        .task(&task("task-1"))
        .expect("an older record reads");

    assert_eq!(record.resume, ResumeState::default());
    assert_eq!(record.running_since, None);
    assert_eq!(record.commands.len(), 1);
    assert_eq!(record.commands[0].actor, CommandActor::Owner);
}

#[test]
fn a_partial_resume_state_fills_the_rest_with_defaults() {
    let state: ResumeState = serde_json::from_value(serde_json::json!({
        "consecutive_losses": 2,
        "last_loss": {
            "from": "running",
            "at": "2026-10-10T12:00:00Z",
            "lease_epoch": 4
        }
    }))
    .expect("a partial resume state reads");

    assert_eq!(state.consecutive_losses, 2);
    assert_eq!(state.next_attempt_at, None);
    assert_eq!(state.exhausted, None);
    assert_eq!(state.uncounted_losses, 0);
    assert!(!state.last_loss_uncounted);
    let loss = state.last_loss.expect("the loss");
    assert_eq!(loss.from, TaskLifecycle::Running);
    assert_eq!(loss.lease_epoch, 4);
    assert_eq!(loss.reason, None);
}

#[test]
fn a_full_resume_state_round_trips() {
    let at = Utc.with_ymd_and_hms(2026, 10, 10, 12, 0, 0).unwrap();
    let state = ResumeState {
        consecutive_losses: 6,
        next_attempt_at: Some(at + Duration::minutes(2)),
        last_loss: Some(WorkloadLoss {
            from: TaskLifecycle::PauseRequested,
            at,
            lease_epoch: 3,
            reason: Some("pod deleted".to_string()),
        }),
        exhausted: Some(ResumeExhaustion {
            reason: "could not resume automatically: pod deleted".to_string(),
            at,
            announced: true,
        }),
        uncounted_losses: 2,
        last_loss_uncounted: true,
    };
    assert!(!state.is_empty());

    let json = serde_json::to_value(&state).unwrap();
    assert_eq!(json["last_loss"]["from"], "pause_requested");
    assert_eq!(json["exhausted"]["announced"], true);
    assert_eq!(json["uncounted_losses"], 2);
    assert_eq!(json["last_loss_uncounted"], true);
    let back: ResumeState = serde_json::from_value(json).unwrap();
    assert_eq!(back, state);
}

#[test]
fn the_automatic_resume_events_round_trip() {
    for kind in [
        TaskEventKind::AutoResumed {
            attempt: 2,
            limit: 5,
            loss_reason: Some("runner process exited".to_string()),
        },
        TaskEventKind::AutoResumed {
            attempt: 1,
            limit: 5,
            loss_reason: None,
        },
        TaskEventKind::AutoResumeExhausted {
            attempts: 5,
            reason: "could not resume automatically: runner process exited".to_string(),
        },
        TaskEventKind::ControllerReported {
            observation: Observation::LossConfirmed,
            lease_epoch: 7,
        },
    ] {
        let json = serde_json::to_value(&kind).unwrap();
        let back: TaskEventKind = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(back, kind, "{json}");
    }

    let json = serde_json::to_value(TaskEventKind::AutoResumed {
        attempt: 3,
        limit: 5,
        loss_reason: None,
    })
    .unwrap();
    assert_eq!(json["type"], "auto_resumed");
    let exhausted = serde_json::to_value(TaskEventKind::AutoResumeExhausted {
        attempts: 5,
        reason: "x".to_string(),
    })
    .unwrap();
    assert_eq!(exhausted["type"], "auto_resume_exhausted");
    let confirmed = serde_json::to_value(Observation::LossConfirmed).unwrap();
    assert_eq!(confirmed, serde_json::json!({ "kind": "loss_confirmed" }));

    // Written without its optional reason, the event still reads.
    let bare: TaskEventKind = serde_json::from_value(serde_json::json!({
        "type": "auto_resumed", "attempt": 1, "limit": 5
    }))
    .unwrap();
    assert_eq!(
        bare,
        TaskEventKind::AutoResumed {
            attempt: 1,
            limit: 5,
            loss_reason: None
        }
    );
}

#[test]
fn a_malformed_automatic_resume_event_is_an_error_not_a_guess() {
    // A known type with missing fields is corrupt, not newer: it must not
    // be swallowed as an unrecognized event.
    let result = serde_json::from_value::<TaskEventKind>(serde_json::json!({
        "type": "auto_resume_exhausted", "attempts": 5
    }));
    assert!(result.is_err());
}

/// A stream frame as a newer server would send it, with `kind` spliced in.
fn newer_frame(kind: serde_json::Value) -> String {
    serde_json::json!({
        "cursor": "task-1:9",
        "event": {
            "sequence": 9,
            "task_id": "task-1",
            "at": "2026-10-10T12:00:00Z",
            "lifecycle": "interrupted",
            "generation": 2,
            "kind": kind
        }
    })
    .to_string()
}

#[test]
fn a_client_keeps_a_stream_frame_whose_event_or_observation_it_does_not_know() {
    use narsor::controller_protocol::ControlEventEnvelope;

    // An event type from a newer version.
    let frame = newer_frame(serde_json::json!({ "type": "resumed_by_magic", "extra": [1, 2] }));
    let envelope: ControlEventEnvelope =
        serde_json::from_str(&frame).expect("a newer event type still decodes");
    assert_eq!(envelope.event.kind, TaskEventKind::Unrecognized);
    assert_eq!(envelope.cursor, "task-1:9", "the cursor still advances");
    assert_eq!(envelope.event.lifecycle, TaskLifecycle::Interrupted);

    // A controller report of an observation from a newer version.
    let frame = newer_frame(serde_json::json!({
        "type": "controller_reported",
        "observation": { "kind": "workload_teleported" },
        "lease_epoch": 3
    }));
    let envelope: ControlEventEnvelope =
        serde_json::from_str(&frame).expect("a newer observation still decodes");
    assert_eq!(
        envelope.event.kind,
        TaskEventKind::ControllerReported {
            observation: Observation::Unrecognized,
            lease_epoch: 3,
        }
    );

    // The confirmation this version introduces decodes as itself.
    let frame = newer_frame(serde_json::json!({
        "type": "controller_reported",
        "observation": { "kind": "loss_confirmed" },
        "lease_epoch": 3
    }));
    let envelope: ControlEventEnvelope = serde_json::from_str(&frame).unwrap();
    assert_eq!(
        envelope.event.kind,
        TaskEventKind::ControllerReported {
            observation: Observation::LossConfirmed,
            lease_epoch: 3,
        }
    );
}

// ---------------------------------------------------------------------------
// Automatic resume: counting losses and the owner's reset (U2)
// ---------------------------------------------------------------------------

/// A lease that outlives every clock advance of these tests.
fn long_lease(fx: &Fixture, controller: &str) -> BackendLease {
    fx.control
        .acquire_lease(&backend_id("k8s"), controller, Duration::days(1))
        .expect("acquire lease")
}

/// Reports a loss of the task's workload, with the controller's reason.
fn lose(
    fx: &Fixture,
    id: &str,
    lease: &BackendLease,
    state_known: bool,
    detail: &str,
) -> TaskRecord {
    let generation = fx.control.task(&task(id)).unwrap().generation;
    let mut loss = report(
        id,
        lease,
        generation,
        Observation::WorkloadLost { state_known },
        None,
    );
    loss.summary.detail = Some(detail.to_string());
    fx.control.report(loss).expect("loss report").task
}

/// Resumes the task the way the resume policy does, once its attempt is
/// due: the clock moves on to it first.
fn policy_resume(fx: &Fixture, id: &str) -> TaskRecord {
    let record = fx.control.task(&task(id)).unwrap();
    if let Some(due) = record.resume.next_attempt_at {
        let wait = due - fx.control.now();
        if wait > Duration::zero() {
            fx.clock.advance(wait);
        }
    }
    fx.control
        .auto_resume(&task(id), record.generation)
        .expect("policy resume")
        .task
}

/// Resumes the task by the policy and loses the new workload before its
/// runner was ready: one more consecutive loss.
fn lose_again_while_provisioning(
    fx: &Fixture,
    id: &str,
    lease: &BackendLease,
    detail: &str,
) -> TaskRecord {
    policy_resume(fx, id);
    lose(fx, id, lease, true, detail)
}

#[test]
fn a_loss_after_a_stable_run_starts_the_count_anew_and_is_due_now() {
    let fx = Fixture::new();
    let lease = long_lease(&fx, "controller-a");
    fx.running("task-1", &lease);
    lose(&fx, "task-1", &lease, true, "first");
    lose_again_while_provisioning(&fx, "task-1", &lease, "second");
    policy_resume(&fx, "task-1");
    fx.observe("task-1", &lease, Observation::RunnerReady)
        .expect("runner ready");
    fx.clock.advance(Duration::minutes(11));

    let lost = lose(&fx, "task-1", &lease, true, "the runner's workload exited");

    assert_eq!(lost.lifecycle, TaskLifecycle::Interrupted);
    assert_eq!(lost.resume.consecutive_losses, 1);
    assert_eq!(lost.resume.next_attempt_at, Some(fx.clock.now()));
    assert_eq!(lost.resume.exhausted, None);
    let loss = lost.resume.last_loss.expect("the loss is remembered");
    assert_eq!(loss.from, TaskLifecycle::Running);
    assert_eq!(loss.at, fx.clock.now());
    assert_eq!(loss.lease_epoch, lease.epoch);
    assert_eq!(loss.reason.as_deref(), Some("the runner's workload exited"));
}

#[test]
fn a_loss_soon_after_the_runner_was_ready_counts_as_consecutive() {
    let fx = Fixture::new();
    let lease = long_lease(&fx, "controller-a");
    let running = fx.running("task-1", &lease);
    assert_eq!(running.running_since, Some(fx.clock.now()));
    fx.clock.advance(Duration::minutes(9));

    let lost = lose(&fx, "task-1", &lease, true, "exited");

    assert_eq!(lost.resume.consecutive_losses, 1);
    assert_eq!(lost.resume.next_attempt_at, Some(fx.clock.now()));

    // The resumed runner is ready again, and lost again within the window.
    policy_resume(&fx, "task-1");
    let ready = fx
        .observe("task-1", &lease, Observation::RunnerReady)
        .unwrap();
    assert_eq!(ready.running_since, Some(fx.clock.now()));
    fx.clock.advance(Duration::minutes(9));
    let lost = lose(&fx, "task-1", &lease, true, "exited");
    assert_eq!(lost.resume.consecutive_losses, 2);
    assert_eq!(
        lost.resume.next_attempt_at,
        Some(fx.clock.now() + Duration::seconds(30))
    );
}

#[test]
fn a_loss_while_provisioning_counts_and_follows_the_delay_table() {
    let fx = Fixture::new();
    let lease = long_lease(&fx, "controller-a");
    fx.create("task-1");
    fx.observe("task-1", &lease, Observation::StartAccepted)
        .unwrap();

    let first = lose(
        &fx,
        "task-1",
        &lease,
        true,
        "the backend refused the workload",
    );
    assert_eq!(first.resume.consecutive_losses, 1);
    assert_eq!(first.resume.next_attempt_at, Some(fx.clock.now()));
    assert_eq!(
        first.resume.last_loss.as_ref().unwrap().from,
        TaskLifecycle::Provisioning
    );

    let expected = [
        (2, Duration::seconds(30)),
        (3, Duration::minutes(2)),
        (4, Duration::minutes(10)),
        (5, Duration::minutes(30)),
    ];
    for (count, delay) in expected {
        fx.clock.advance(Duration::seconds(1));
        let lost = lose_again_while_provisioning(&fx, "task-1", &lease, "refused");
        assert_eq!(lost.resume.consecutive_losses, count);
        assert_eq!(lost.resume.next_attempt_at, Some(fx.clock.now() + delay));
        assert_eq!(lost.resume.exhausted, None, "attempt {count} is planned");
    }
}

#[test]
fn the_sixth_consecutive_loss_exhausts_the_policy_with_the_last_reason() {
    let fx = Fixture::new();
    let lease = long_lease(&fx, "controller-a");
    fx.running("task-1", &lease);
    lose(&fx, "task-1", &lease, true, "exited");
    for _ in 2..=5 {
        lose_again_while_provisioning(&fx, "task-1", &lease, "exited");
    }
    let fifth = fx.control.task(&task("task-1")).unwrap();
    assert_eq!(fifth.resume.consecutive_losses, 5);
    assert_eq!(
        fifth.resume.next_attempt_at,
        Some(fx.clock.now() + Duration::minutes(30))
    );

    fx.clock.advance(Duration::minutes(30));
    let sixth = lose_again_while_provisioning(
        &fx,
        "task-1",
        &lease,
        "the runner's workload failed before it was ready",
    );

    assert_eq!(sixth.lifecycle, TaskLifecycle::Interrupted);
    assert_eq!(sixth.resume.consecutive_losses, 6);
    assert_eq!(sixth.resume.next_attempt_at, None, "no further attempt");
    let exhausted = sixth.resume.exhausted.expect("the policy gave up");
    assert_eq!(
        exhausted.reason,
        "could not resume automatically: the runner's workload failed before it was ready"
    );
    assert_eq!(exhausted.at, fx.clock.now());
    assert!(!exhausted.announced, "the policy announces it later");
}

#[test]
fn a_shorter_policy_exhausts_sooner() {
    let root = TempRoot::new();
    let clock = ManualClock::new();
    let control =
        ControlPlane::with_clock(root.paths(), clock.clone()).with_resume_policy(ResumePolicy {
            attempt_delays: vec![Duration::zero(), Duration::seconds(1)],
            stability_window: Duration::seconds(5),
        });
    assert_eq!(control.resume_policy().limit(), 2);
    let fx = Fixture {
        root,
        clock,
        control,
    };
    let lease = long_lease(&fx, "controller-a");
    fx.running("task-1", &lease);
    lose(&fx, "task-1", &lease, true, "exited");
    let second = lose_again_while_provisioning(&fx, "task-1", &lease, "exited");
    assert_eq!(
        second.resume.next_attempt_at,
        Some(fx.clock.now() + Duration::seconds(1))
    );

    let third = lose_again_while_provisioning(&fx, "task-1", &lease, "exited");
    assert!(third.resume.exhausted.is_some());

    // The window is the policy's too: five seconds of running start anew.
    fx.command("task-1", OwnerCommand::Start, "owner-start");
    fx.observe("task-1", &lease, Observation::RunnerReady)
        .unwrap();
    fx.clock.advance(Duration::seconds(5));
    let stable = lose(&fx, "task-1", &lease, true, "exited");
    assert_eq!(stable.resume.consecutive_losses, 1);
}

#[test]
fn the_default_policy_is_five_attempts_with_a_ten_minute_window() {
    let policy = ResumePolicy::default();
    assert_eq!(policy.limit(), 5);
    assert_eq!(
        policy.attempt_delays,
        vec![
            Duration::zero(),
            Duration::seconds(30),
            Duration::minutes(2),
            Duration::minutes(10),
            Duration::minutes(30),
        ]
    );
    assert_eq!(policy.stability_window, Duration::minutes(10));
}

#[test]
fn a_loss_while_pausing_plans_no_resume() {
    let fx = Fixture::new();
    let lease = long_lease(&fx, "controller-a");
    fx.running("task-1", &lease);
    fx.command("task-1", OwnerCommand::Pause, "pause-1");

    let lost = lose(&fx, "task-1", &lease, true, "exited before a safe point");

    assert_eq!(lost.lifecycle, TaskLifecycle::Interrupted);
    assert_eq!(lost.resume.next_attempt_at, None, "waits for the owner");
    assert_eq!(lost.resume.consecutive_losses, 0);
    assert_eq!(lost.resume.exhausted, None);
    assert_eq!(
        lost.resume.last_loss.unwrap().from,
        TaskLifecycle::PauseRequested
    );
}

#[test]
fn a_loss_while_pausing_confirmed_through_unknown_plans_no_resume() {
    let fx = Fixture::new();
    let lease = long_lease(&fx, "controller-a");
    fx.running("task-1", &lease);
    fx.command("task-1", OwnerCommand::Pause, "pause-1");
    let unknown = lose(&fx, "task-1", &lease, false, "cannot observe");
    assert_eq!(unknown.lifecycle, TaskLifecycle::Unknown);
    assert_eq!(unknown.resume.next_attempt_at, None);
    assert_eq!(
        unknown.resume.last_loss.as_ref().unwrap().from,
        TaskLifecycle::PauseRequested,
        "the origin is remembered before the confirmation"
    );
    fx.clock.advance(Duration::minutes(3));

    let confirmed = fx
        .observe("task-1", &lease, Observation::LossConfirmed)
        .unwrap();

    assert_eq!(confirmed.lifecycle, TaskLifecycle::Interrupted);
    assert_eq!(confirmed.resume.next_attempt_at, None);
    assert_eq!(confirmed.resume.consecutive_losses, 0);
    assert_eq!(
        confirmed.resume.last_loss.unwrap().from,
        TaskLifecycle::PauseRequested
    );
}

#[test]
fn a_confirmed_loss_is_counted_as_of_when_the_workload_was_lost() {
    let fx = Fixture::new();
    let lease = long_lease(&fx, "controller-a");
    fx.running("task-1", &lease);
    fx.clock.advance(Duration::minutes(9));
    let unknown = lose(&fx, "task-1", &lease, false, "cannot observe");
    let lost_at = fx.clock.now();
    assert_eq!(unknown.resume.consecutive_losses, 0, "not counted yet");
    fx.clock.advance(Duration::minutes(5));

    let confirmed = fx
        .observe("task-1", &lease, Observation::LossConfirmed)
        .unwrap();

    // Ready for nine minutes when lost: consecutive, though fourteen have
    // passed by the confirmation.
    assert_eq!(confirmed.resume.consecutive_losses, 1);
    assert_eq!(confirmed.resume.next_attempt_at, Some(fx.clock.now()));
    let loss = confirmed.resume.last_loss.unwrap();
    assert_eq!(loss.from, TaskLifecycle::Running);
    assert_eq!(loss.at, lost_at);
    assert_eq!(loss.reason.as_deref(), Some("cannot observe"));
}

#[test]
fn a_loss_found_under_a_newer_lease_epoch_than_the_launch_keeps_the_count() {
    let fx = Fixture::new();
    let lease = long_lease(&fx, "controller-a");
    fx.running("task-1", &lease);
    lose(&fx, "task-1", &lease, true, "exited");
    let second = lose_again_while_provisioning(&fx, "task-1", &lease, "exited");
    assert_eq!(second.resume.consecutive_losses, 2);
    policy_resume(&fx, "task-1");
    fx.observe("task-1", &lease, Observation::RunnerReady)
        .unwrap();

    // The controller restarts: same identity, next epoch, and the first
    // thing it sees is the workload gone.
    let restarted = long_lease(&fx, "controller-a");
    assert!(restarted.epoch > lease.epoch);
    let lost = lose(
        &fx,
        "task-1",
        &restarted,
        true,
        "the runner's workload disappeared",
    );

    assert_eq!(
        lost.resume.consecutive_losses, 2,
        "a restart costs no attempt"
    );
    assert_eq!(lost.resume.next_attempt_at, Some(fx.clock.now()));
    assert_eq!(lost.resume.exhausted, None);
    assert_eq!(lost.resume.last_loss.unwrap().lease_epoch, restarted.epoch);

    // The resume is ordered under the new epoch, so its loss counts again.
    let again = lose_again_while_provisioning(&fx, "task-1", &restarted, "exited");
    assert_eq!(again.resume.consecutive_losses, 3);
}

#[test]
fn a_restart_loss_after_a_stable_run_starts_the_count_anew() {
    let fx = Fixture::new();
    let lease = long_lease(&fx, "controller-a");
    fx.running("task-1", &lease);
    lose(&fx, "task-1", &lease, true, "exited");
    lose_again_while_provisioning(&fx, "task-1", &lease, "exited");
    policy_resume(&fx, "task-1");
    fx.observe("task-1", &lease, Observation::RunnerReady)
        .unwrap();
    fx.clock.advance(Duration::minutes(11));

    let restarted = long_lease(&fx, "controller-a");
    let lost = lose(&fx, "task-1", &restarted, true, "disappeared");
    assert_eq!(lost.resume.consecutive_losses, 0, "the stable run reset it");
    assert_eq!(lost.resume.next_attempt_at, Some(fx.clock.now()));

    policy_resume(&fx, "task-1");
    fx.observe("task-1", &restarted, Observation::RunnerReady)
        .unwrap();
    fx.clock.advance(Duration::minutes(1));
    let quick = lose(&fx, "task-1", &restarted, true, "exited");

    assert_eq!(quick.resume.consecutive_losses, 1, "attempt 1 again");
    assert_eq!(quick.resume.next_attempt_at, Some(fx.clock.now()));
}

/// Resumes the task by the policy, restarts its controller (the next lease
/// epoch) and loses the workload under it: a loss the policy may leave
/// uncounted.
fn lose_after_a_restart(fx: &Fixture, id: &str) -> (TaskRecord, BackendLease) {
    policy_resume(fx, id);
    let restarted = long_lease(fx, "controller-a");
    let lost = lose(fx, id, &restarted, true, "the controller restarted");
    (lost, restarted)
}

#[test]
fn a_controller_that_keeps_restarting_gets_only_so_many_free_resumes() {
    let fx = Fixture::new();
    let lease = long_lease(&fx, "controller-a");
    fx.running("task-1", &lease);
    lose(&fx, "task-1", &lease, true, "exited");
    let limit = ResumePolicy::default().limit();

    for free in 1..=limit {
        let (lost, _) = lose_after_a_restart(&fx, "task-1");
        assert_eq!(lost.resume.consecutive_losses, 1, "free loss {free}");
        assert_eq!(lost.resume.uncounted_losses, free);
        assert!(lost.resume.last_loss_uncounted);
        assert_eq!(lost.resume.next_attempt_at, Some(fx.clock.now()));
    }

    // Past the limit a restart loss counts like any other.
    let (counted, _) = lose_after_a_restart(&fx, "task-1");
    assert_eq!(counted.resume.consecutive_losses, 2);
    assert!(!counted.resume.last_loss_uncounted);
    assert_eq!(
        counted.resume.next_attempt_at,
        Some(fx.clock.now() + Duration::seconds(30))
    );
    let mut last = counted;
    while last.resume.exhausted.is_none() {
        let (lost, _) = lose_after_a_restart(&fx, "task-1");
        last = lost;
    }
    assert_eq!(last.resume.consecutive_losses, limit + 1);
    assert_eq!(last.resume.next_attempt_at, None, "the crash loop ends");
}

#[test]
fn six_restart_losses_in_a_row_do_not_exhaust_the_policy() {
    let fx = Fixture::new();
    let lease = long_lease(&fx, "controller-a");
    fx.running("task-1", &lease);
    let restarted = long_lease(&fx, "controller-a");
    let mut last = lose(&fx, "task-1", &restarted, true, "restarted");
    for _ in 2..=6 {
        last = lose_after_a_restart(&fx, "task-1").0;
    }

    assert_eq!(last.resume.exhausted, None);
    assert_eq!(last.resume.consecutive_losses, 1, "the sixth is attempt 1");
    assert_eq!(last.resume.next_attempt_at, Some(fx.clock.now()));
}

#[test]
fn a_resume_after_an_uncounted_loss_says_attempt_zero() {
    let fx = Fixture::new();
    let lease = long_lease(&fx, "controller-a");
    fx.running("task-1", &lease);
    lose(&fx, "task-1", &lease, true, "exited");
    lose_again_while_provisioning(&fx, "task-1", &lease, "exited");
    lose_after_a_restart(&fx, "task-1");

    let resumed = policy_resume(&fx, "task-1");

    let attempts: Vec<u32> = fx
        .control
        .events_since(&task("task-1"), 0)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.kind {
            TaskEventKind::AutoResumed { attempt, .. } => Some(attempt),
            _ => None,
        })
        .collect();
    assert_eq!(attempts, vec![1, 2, 0], "no counted attempt is repeated");
    assert_eq!(resumed.resume.consecutive_losses, 2);
}

#[test]
fn an_owner_resume_forgets_the_uncounted_losses() {
    let fx = Fixture::new();
    let lease = long_lease(&fx, "controller-a");
    fx.running("task-1", &lease);
    lose(&fx, "task-1", &lease, true, "exited");
    lose_after_a_restart(&fx, "task-1");

    let resumed = fx.command("task-1", OwnerCommand::Resume, "owner-resume");

    assert!(resumed.resume.is_empty());
}

#[test]
fn a_loss_after_the_new_controller_saw_the_workload_alive_counts() {
    let fx = Fixture::new();
    let lease = long_lease(&fx, "controller-a");
    fx.running("task-1", &lease);
    let restarted = long_lease(&fx, "controller-a");
    let mut alive = report("task-1", &restarted, 1, Observation::Status, None);
    alive.summary.detail = Some("running".to_string());
    fx.control
        .report(alive)
        .expect("status from the new controller");

    let lost = lose(&fx, "task-1", &restarted, true, "exited");

    assert_eq!(lost.resume.consecutive_losses, 1, "counted like any loss");
}

#[test]
fn an_owner_start_or_resume_resets_an_exhausted_resume_state() {
    for command in [OwnerCommand::Start, OwnerCommand::Resume] {
        let fx = Fixture::new();
        let lease = long_lease(&fx, "controller-a");
        fx.running("task-1", &lease);
        lose(&fx, "task-1", &lease, true, "exited");
        for _ in 2..=6 {
            lose_again_while_provisioning(&fx, "task-1", &lease, "exited");
        }
        let exhausted = fx.control.task(&task("task-1")).unwrap();
        assert!(exhausted.resume.exhausted.is_some());

        let resumed = fx.command("task-1", command, "owner-retry");

        assert_eq!(resumed.lifecycle, TaskLifecycle::Provisioning);
        assert!(resumed.resume.is_empty(), "{command:?} resets the count");
        assert_eq!(resumed.running_since, None);
        let lost = lose(&fx, "task-1", &lease, true, "exited");
        assert_eq!(lost.resume.consecutive_losses, 1, "counting starts over");
    }
}

#[test]
fn an_owner_resume_of_an_unknown_task_forgets_its_loss() {
    let fx = Fixture::new();
    let lease = long_lease(&fx, "controller-a");
    fx.running("task-1", &lease);
    lose(&fx, "task-1", &lease, false, "cannot observe");

    let resumed = fx.command("task-1", OwnerCommand::Start, "owner-start");

    assert!(resumed.resume.is_empty());
}

#[test]
fn a_policy_resume_keeps_the_count_and_uses_up_the_planned_attempt() {
    let fx = Fixture::new();
    let lease = long_lease(&fx, "controller-a");
    fx.running("task-1", &lease);
    lose(&fx, "task-1", &lease, true, "exited");
    let second = lose_again_while_provisioning(&fx, "task-1", &lease, "exited");
    assert_eq!(second.resume.consecutive_losses, 2);

    let resumed = policy_resume(&fx, "task-1");

    assert_eq!(resumed.lifecycle, TaskLifecycle::Provisioning);
    assert_eq!(resumed.resume.consecutive_losses, 2);
    assert_eq!(resumed.resume.next_attempt_at, None);
    assert!(resumed.resume.last_loss.is_some());
}

#[test]
fn later_controller_reports_keep_the_exhaustion() {
    let fx = Fixture::new();
    let lease = long_lease(&fx, "controller-a");
    fx.running("task-1", &lease);
    lose(&fx, "task-1", &lease, true, "exited");
    for _ in 2..=6 {
        lose_again_while_provisioning(&fx, "task-1", &lease, "exited");
    }
    let record = fx.control.task(&task("task-1")).unwrap();
    let (exhausted, generation) = (record.resume, record.generation);
    assert!(exhausted.exhausted.is_some());

    let mut status = report("task-1", &lease, generation, Observation::Status, None);
    status.summary.detail = Some("nothing to do".to_string());
    let after_status = fx.control.report(status).unwrap().task;
    let mut repeated = report(
        "task-1",
        &lease,
        generation,
        Observation::WorkloadLost { state_known: true },
        None,
    );
    repeated.summary.detail = Some("still gone".to_string());
    let after_loss = fx.control.report(repeated).unwrap().task;
    let after_confirmation = fx
        .observe("task-1", &lease, Observation::LossConfirmed)
        .unwrap();

    assert_eq!(after_status.resume, exhausted);
    assert_eq!(
        after_loss.resume, exhausted,
        "a repeated loss is not recounted"
    );
    assert_eq!(after_confirmation.resume, exhausted);
}

/// Rewrites the stored record as a version before the resume policy wrote
/// it: no resume state, no running mark, no workload epoch.
fn strip_resume_fields(fx: &Fixture, id: &str) {
    let path = fx.root.paths().orchestration_task_file(id);
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let object = value.as_object_mut().unwrap();
    object.remove("resume");
    object.remove("running_since");
    object.remove("workload_epoch");
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
}

#[test]
fn a_task_interrupted_before_the_policy_is_resumed_unless_lost_while_pausing() {
    let fx = Fixture::new();
    let lease = long_lease(&fx, "controller-a");
    fx.running("running-1", &lease);
    lose(
        &fx,
        "running-1",
        &lease,
        true,
        "the runner's workload exited",
    );
    let lost_at = fx.clock.now();
    fx.running("pausing-1", &lease);
    fx.command("pausing-1", OwnerCommand::Pause, "pause-1");
    lose(&fx, "pausing-1", &lease, true, "exited before a safe point");
    for id in ["running-1", "pausing-1"] {
        strip_resume_fields(&fx, id);
        assert!(fx.control.task(&task(id)).unwrap().resume.is_empty());
    }
    fx.clock.advance(Duration::hours(2));

    let recovery = fx.reopen().recover().expect("recover");

    assert_eq!(recovery.adopted_losses.len(), 2);
    let running = fx.control.task(&task("running-1")).unwrap();
    assert_eq!(running.lifecycle, TaskLifecycle::Interrupted);
    assert_eq!(running.resume.consecutive_losses, 1, "a fresh loss");
    assert_eq!(running.resume.next_attempt_at, Some(fx.clock.now()));
    let loss = running.resume.last_loss.unwrap();
    assert_eq!(loss.from, TaskLifecycle::Running);
    assert_eq!(loss.at, lost_at, "read from the event log");
    assert_eq!(loss.lease_epoch, lease.epoch);
    assert_eq!(loss.reason.as_deref(), Some("the runner's workload exited"));

    let pausing = fx.control.task(&task("pausing-1")).unwrap();
    assert_eq!(pausing.resume.next_attempt_at, None, "waits for the owner");
    assert_eq!(
        pausing.resume.last_loss.unwrap().from,
        TaskLifecycle::PauseRequested
    );

    // Adopted once: a second recovery changes nothing.
    let again = fx.reopen().recover().expect("recover again");
    assert!(again.adopted_losses.is_empty());
}

#[test]
fn a_task_unknown_before_the_policy_is_counted_once_confirmed() {
    let fx = Fixture::new();
    let lease = long_lease(&fx, "controller-a");
    fx.running("task-1", &lease);
    lose(&fx, "task-1", &lease, false, "cannot observe");
    strip_resume_fields(&fx, "task-1");

    let recovery = fx.reopen().recover().expect("recover");

    assert_eq!(recovery.adopted_losses, vec![task("task-1")]);
    let adopted = fx.control.task(&task("task-1")).unwrap();
    assert_eq!(adopted.lifecycle, TaskLifecycle::Unknown);
    assert_eq!(
        adopted.resume.next_attempt_at, None,
        "nothing confirmed yet"
    );
    assert_eq!(
        adopted.resume.last_loss.as_ref().unwrap().from,
        TaskLifecycle::Running
    );

    let confirmed = fx
        .observe("task-1", &lease, Observation::LossConfirmed)
        .unwrap();
    assert_eq!(confirmed.resume.consecutive_losses, 1);
    assert_eq!(confirmed.resume.next_attempt_at, Some(fx.clock.now()));
}

#[test]
fn an_unreadable_legacy_event_log_skips_only_its_task() {
    let fx = Fixture::new();
    let lease = long_lease(&fx, "controller-a");
    for id in ["broken-1", "fine-1"] {
        fx.running(id, &lease);
        lose(&fx, id, &lease, true, "exited");
        strip_resume_fields(&fx, id);
    }
    let first_event = fx.root.paths().orchestration_task_event_file("broken-1", 1);
    fs::write(&first_event, b"not an event").unwrap();

    let recovery = fx.reopen().recover().expect("recovery still succeeds");

    assert_eq!(recovery.adopted_losses, vec![task("fine-1")]);
    assert!(fx
        .control
        .task(&task("broken-1"))
        .unwrap()
        .resume
        .is_empty());
}

/// The daemon's startup goes through the same function this test calls: a
/// task lost before the resume policy is adopted without a daemon process.
#[test]
fn the_daemons_startup_recovery_adopts_a_loss_from_before_the_policy() {
    let fx = Fixture::new();
    let lease = long_lease(&fx, "controller-a");
    fx.running("task-1", &lease);
    lose(&fx, "task-1", &lease, true, "exited");
    strip_resume_fields(&fx, "task-1");

    narsor::cli::daemon::recover_orchestration_at_startup(fx.reopen());

    let adopted = fx.control.task(&task("task-1")).unwrap();
    assert_eq!(adopted.resume.consecutive_losses, 1);
    assert!(adopted.auto_resume_due(fx.clock.now()));
}

#[test]
fn recovery_leaves_tasks_that_were_not_lost_and_current_losses_alone() {
    let fx = Fixture::new();
    let lease = long_lease(&fx, "controller-a");
    fx.running("running-1", &lease);
    fx.running("lost-1", &lease);
    let lost = lose(&fx, "lost-1", &lease, true, "exited");

    let recovery = fx.reopen().recover().expect("recover");

    assert!(recovery.adopted_losses.is_empty());
    assert!(fx
        .control
        .task(&task("running-1"))
        .unwrap()
        .resume
        .is_empty());
    assert_eq!(fx.control.task(&task("lost-1")).unwrap(), lost);
}

#[test]
fn the_loss_state_tells_what_an_interrupted_task_waits_for() {
    let fx = Fixture::new();
    let lease = long_lease(&fx, "controller-a");
    let running = fx.running("task-1", &lease);
    assert_eq!(loss_state(running.lifecycle, &running.resume), None);

    let lost = lose(&fx, "task-1", &lease, true, "exited");
    let state = loss_state(lost.lifecycle, &lost.resume).expect("a loss state");
    assert_eq!(
        state,
        LossState::AutoResume {
            attempt: 1,
            limit: 5
        }
    );
    assert!(!state.needs_owner());

    let exhausted = ResumeState {
        exhausted: Some(ResumeExhaustion {
            reason: "could not resume automatically: pod evicted".to_string(),
            at: fx.clock.now(),
            announced: true,
        }),
        ..ResumeState::default()
    };
    let state = loss_state(TaskLifecycle::Interrupted, &exhausted).unwrap();
    assert!(state.needs_owner());
    let LossState::Exhausted(reason) = state else {
        panic!("exhausted: {state:?}");
    };
    assert_eq!(exhaustion_loss(&reason), "pod evicted");
    assert_eq!(exhaustion_loss("in other words"), "in other words");
}
