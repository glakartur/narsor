//! Integration tests for the controller and runner protocol (U2).
//!
//! Everything goes over real HTTP against the real API server, through the
//! real typed clients: an owner (`ApiClient`), controllers enrolled with
//! one-time codes (`ControllerClient`) and runners holding incarnation
//! credentials (`RunnerClient`). The control plane runs on a hand-moved
//! clock, so lease expiry and staleness are deterministic; long-polls use
//! real time but are only ever asked to wait briefly.

mod environment_support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Duration, TimeZone, Utc};
use narsor::api::{self, ApiClient, ApiServices, ClientError};
use narsor::controller_protocol::{
    CommandAck, ControllerClient, ConversationResponse, ConversationUnavailableReason,
    CreateEnrollmentRequest, CreateTaskRequest, DesiredAction, Freshness, LaunchRequest,
    OwnerCommandRequest, ProtocolService, ReportRequest, RunnerClient, RunnerCredentialRequest,
    RunnerDirective, SubmitInputRequest, TaskSummary,
};
use narsor::domain::{ExecutionStatus, TaskId};
use narsor::engine::{Engine, HumanInputDelivery, PendingHumanInput, Timeline};
use narsor::orchestration::{
    Clock, CommandRequest, ControlPlane, ConversationAttemptSummary, ConversationAvailability,
    ExecutionCursor, InputStatus, NewTask, Observation, ObservedSummary, OwnerCommand, OwnerRef,
    PendingInputRef, ResourceIdentity, RunnerSnapshot, TaskLifecycle,
};
use narsor::persistence::paths::{temp_path_for, StatePaths};
use narsor::persistence::{DaemonInfo, ExecutionStore};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A temporary directory that deletes itself when it goes out of scope.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-protocol-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A clock the test moves by hand.
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

/// A served control plane plus an owner client.
struct Harness {
    root: TempRoot,
    clock: Arc<ManualClock>,
    handle: api::ApiHandle,
    owner: ApiClient,
}

/// A task brought to `Running` by a controller, with its runner.
struct Running {
    controller: ControllerClient,
    epoch: u64,
    runner: RunnerClient,
}

impl Harness {
    async fn start() -> Self {
        let root = TempRoot::new();
        let clock = ManualClock::new();
        let control = ControlPlane::with_clock(StatePaths::new(root.path()), clock.clone());
        let services = ApiServices {
            control: Some(Arc::new(ProtocolService::new(
                control,
                environment_support::seeded_resolver(root.path()),
            ))),
            ..ApiServices::default()
        };
        let engine = Arc::new(Engine::new(ExecutionStore::new(root.path())));
        let handle = api::start_with_services(
            engine,
            root.path().to_path_buf(),
            root.path().to_path_buf(),
            services,
        )
        .await
        .expect("server starts");
        let owner = handle.client();
        Self {
            root,
            clock,
            handle,
            owner,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.handle.addr())
    }

    /// A second control-plane handle on the server's state root and clock,
    /// for setting up what no owner of this server could create.
    fn control_plane(&self) -> ControlPlane {
        ControlPlane::with_clock(StatePaths::new(self.root.path()), self.clock.clone())
    }

    /// A newly enrolled controller and its credential.
    async fn enroll_with_credential(&self, backend: &str) -> (ControllerClient, String) {
        let code = self
            .owner
            .create_enrollment(backend, &CreateEnrollmentRequest::default())
            .await
            .expect("enrollment code");
        let (client, enrolled) = ControllerClient::enroll(self.base_url(), &code.enrollment_code)
            .await
            .expect("enroll");
        (client, enrolled.credential)
    }

    async fn enroll(&self, backend: &str) -> ControllerClient {
        self.enroll_with_credential(backend).await.0
    }

    /// An enrolled controller holding its backend's lease.
    async fn controller(&self, backend: &str) -> (ControllerClient, u64) {
        let controller = self.enroll(backend).await;
        let lease = controller.acquire_lease(None).await.expect("lease");
        (controller, lease.epoch)
    }

    async fn create(&self, task: &str, backend: &str) -> TaskSummary {
        self.owner
            .create_control_task(&create_request(task, backend))
            .await
            .expect("create task")
    }

    async fn command(&self, task: &str, command: OwnerCommand, key: &str) -> TaskSummary {
        self.owner
            .control_command(
                task,
                &OwnerCommandRequest {
                    command,
                    idempotency_key: key.to_string(),
                    expected_generation: None,
                },
            )
            .await
            .expect("owner command")
            .task
    }

    async fn task(&self, task: &str) -> TaskSummary {
        self.owner.control_task(task).await.expect("task summary")
    }

    /// Creates `task` on `backend` and drives it to `Running` the way a
    /// controller must: accept the start, issue the incarnation's
    /// credential, let the runner report, report it ready.
    async fn running(&self, task: &str, backend: &str) -> Running {
        self.create(task, backend).await;
        let (controller, epoch) = self.controller(backend).await;
        controller
            .report(&report(task, epoch, 1, Observation::StartAccepted, None))
            .await
            .expect("start accepted");
        let credential = controller
            .issue_runner_credential(
                task,
                &RunnerCredentialRequest {
                    lease_epoch: epoch,
                    generation: 1,
                    incarnation: "pod-1".to_string(),
                },
            )
            .await
            .expect("runner credential");
        let runner = RunnerClient::new(self.base_url(), credential.credential);
        runner
            .snapshot(&snapshot(1, ExecutionStatus::Running, 3, None))
            .await
            .expect("first snapshot");
        controller
            .report(&report(task, epoch, 1, Observation::RunnerReady, Some(1)))
            .await
            .expect("runner ready");
        Running {
            controller,
            epoch,
            runner,
        }
    }
}

fn create_request(task: &str, backend: &str) -> CreateTaskRequest {
    CreateTaskRequest {
        task_id: Some(task.to_string()),
        backend_id: Some(backend.to_string()),
        priority: 0,
        launch: LaunchRequest {
            workflow_id: "implement-feature".to_string(),
            workflow_version: 1,
            inputs: serde_json::json!({ "ticket": "NAR-1" }),
            workspace: None,
            profile: None,
            harness: None,
            resources: None,
        },
    }
}

/// A report about `pod-1` at resource generation `resource_generation`
/// (`None`: no resource exists yet).
fn report(
    task: &str,
    epoch: u64,
    generation: u64,
    observation: Observation,
    resource_generation: Option<u64>,
) -> ReportRequest {
    ReportRequest {
        task_id: task.to_string(),
        lease_epoch: epoch,
        desired_generation: generation,
        observation,
        resource: resource_generation.map(|resource_generation| ResourceIdentity {
            uid: "pod-1".to_string(),
            resource_generation,
        }),
        summary: ObservedSummary::default(),
        observed_at: Utc::now(),
    }
}

fn pending(id: &str) -> PendingInputRef {
    let form: PendingHumanInput = serde_json::from_value(serde_json::json!({
        "execution_id": "exec-1",
        "state_id": "review",
        "action_execution_id": "act-7",
        "instruction": "Approve the plan?",
        "fields": [],
        "outputs": [],
        "episode": id,
        "requested_at": "2026-10-03T12:00:00Z",
    }))
    .expect("pending input form");
    PendingInputRef {
        id: id.to_string(),
        attempt_id: Some("act-7".to_string()),
        form,
    }
}

fn snapshot(
    sequence: u64,
    status: ExecutionStatus,
    event_cursor: u64,
    pending_input: Option<&str>,
) -> RunnerSnapshot {
    RunnerSnapshot {
        snapshot_sequence: sequence,
        execution_status: status,
        execution_cursor: ExecutionCursor {
            execution_id: Some("exec-1".to_string()),
            current_state: Some("review".to_string()),
            action_execution_id: Some("act-7".to_string()),
        },
        event_cursor,
        pending_input: pending_input.map(pending),
        conversation: ConversationAvailability::Live {
            attempts: vec![ConversationAttemptSummary {
                attempt_id: "act-6".to_string(),
                state_id: Some("plan".to_string()),
                entries: 42,
                closed: true,
                last_entry_at: None,
            }],
        },
        timeline: Some(Timeline {
            default_position: Some("plan#1".to_string()),
            ..Timeline::default()
        }),
        safe_point: false,
        observed_at: Utc.with_ymd_and_hms(2026, 10, 3, 12, 0, 5).unwrap(),
    }
}

fn delivery(comment: &str) -> HumanInputDelivery {
    HumanInputDelivery {
        comment: Some(comment.to_string()),
        ..HumanInputDelivery::default()
    }
}

fn input_request(pending_input_id: &str, key: &str) -> SubmitInputRequest {
    SubmitInputRequest {
        pending_input_id: pending_input_id.to_string(),
        idempotency_key: key.to_string(),
        delivery: delivery("looks good"),
    }
}

fn refused_code(result: Result<impl std::fmt::Debug, ClientError>) -> String {
    match result {
        Err(ClientError::Refused { code, .. }) => code,
        other => panic!("expected a 409 with a reason, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Authentication boundaries
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unauthenticated_requests_get_bare_status_codes() {
    let harness = Harness::start().await;
    let raw = reqwest::Client::new();
    let base = harness.base_url();

    for (method, path) in [
        ("GET", "/control/v1/tasks"),
        ("GET", "/controller/v1/commands"),
        ("POST", "/controller/v1/heartbeat"),
        ("GET", "/runner/v1/poll"),
        ("POST", "/runner/v1/renew"),
    ] {
        for token in [
            None,
            Some("nsc.ctl-0.deadbeef"),
            Some("nsr.task-1.deadbeef"),
        ] {
            let mut request = match method {
                "GET" => raw.get(format!("{base}{path}")),
                _ => raw
                    .post(format!("{base}{path}"))
                    .json(&serde_json::json!({})),
            };
            if let Some(token) = token {
                request = request.bearer_auth(token);
            }
            let response = request.send().await.expect("request");
            assert_eq!(
                response.status(),
                reqwest::StatusCode::UNAUTHORIZED,
                "{method} {path} with {token:?}"
            );
            assert!(response.text().await.unwrap().is_empty(), "bare 401");
        }
    }

    let bad_code = raw
        .post(format!("{base}/controller/v1/enroll"))
        .json(&serde_json::json!({ "enrollment_code": "nse.enr-0.deadbeef" }))
        .send()
        .await
        .expect("enroll request");
    assert_eq!(bad_code.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert!(bad_code.text().await.unwrap().is_empty());

    // A controller credential is not an owner credential.
    let controller = harness.enroll("k8s").await;
    let as_owner = ApiClient::new(harness.handle.addr(), "nsc.anything.secret");
    assert!(matches!(
        as_owner.control_tasks().await,
        Err(ClientError::Unauthorized)
    ));
    controller
        .heartbeat()
        .await
        .expect("controller credential works");
}

#[tokio::test]
async fn an_enrollment_code_enrolls_once_and_expires() {
    let harness = Harness::start().await;
    let code = harness
        .owner
        .create_enrollment(
            "k8s",
            &CreateEnrollmentRequest {
                name: Some("cluster-a".to_string()),
                ttl_secs: Some(60),
            },
        )
        .await
        .expect("code");

    let (controller, enrolled) =
        ControllerClient::enroll(harness.base_url(), &code.enrollment_code)
            .await
            .expect("first redemption");
    assert_eq!(enrolled.backend_id.as_str(), "k8s");
    assert!(matches!(
        ControllerClient::enroll(harness.base_url(), &code.enrollment_code).await,
        Err(ClientError::Unauthorized)
    ));
    let heartbeat = controller.heartbeat().await.expect("heartbeat");
    assert_eq!(heartbeat.controller_id, enrolled.controller_id);

    let late = harness
        .owner
        .create_enrollment("k8s", &CreateEnrollmentRequest::default())
        .await
        .expect("second code");
    harness.clock.advance(Duration::hours(1));
    assert!(matches!(
        ControllerClient::enroll(harness.base_url(), &late.enrollment_code).await,
        Err(ClientError::Unauthorized)
    ));
}

#[tokio::test]
async fn a_controller_scoped_to_one_backend_cannot_see_or_touch_another() {
    let harness = Harness::start().await;
    harness.create("task-a", "backend-a").await;
    harness.create("task-b", "backend-b").await;
    let (controller, epoch) = harness.controller("backend-a").await;

    let commands = controller.commands(0, true).await.expect("commands");
    let ids: Vec<_> = commands
        .commands
        .iter()
        .map(|command| command.task_id.as_str())
        .collect();
    assert_eq!(ids, ["task-a"]);

    assert!(matches!(
        controller
            .report(&report(
                "task-b",
                epoch,
                1,
                Observation::StartAccepted,
                None
            ))
            .await,
        Err(ClientError::NotFound)
    ));
    assert!(matches!(
        controller
            .acknowledge(vec![CommandAck {
                task_id: "task-b".to_string(),
                generation: 1,
            }])
            .await,
        Err(ClientError::NotFound)
    ));
    assert!(matches!(
        controller
            .issue_runner_credential(
                "task-b",
                &RunnerCredentialRequest {
                    lease_epoch: epoch,
                    generation: 1,
                    incarnation: "pod-x".to_string(),
                },
            )
            .await,
        Err(ClientError::NotFound)
    ));
    assert_eq!(
        harness.task("task-b").await.lifecycle,
        TaskLifecycle::Queued
    );

    // Its lease is its own backend's, and leaves backend-b free.
    let (_, other_epoch) = harness.controller("backend-b").await;
    assert_eq!(other_epoch, 1);
}

#[tokio::test]
async fn a_revoked_controller_is_refused_and_its_lease_fenced() {
    let harness = Harness::start().await;
    harness.create("task-1", "k8s").await;
    let (controller, epoch) = harness.controller("k8s").await;
    controller
        .report(&report(
            "task-1",
            epoch,
            1,
            Observation::StartAccepted,
            None,
        ))
        .await
        .expect("accepted before revocation");

    let listed = harness.owner.controllers().await.expect("controllers");
    let controller_id = listed.controllers[0].controller_id.clone();
    assert!(listed.controllers[0].holds_lease);
    let revoked = harness
        .owner
        .revoke_controller(&controller_id)
        .await
        .expect("revoke");
    assert!(revoked.revoked_at.is_some());
    assert!(!revoked.holds_lease);

    assert!(matches!(
        controller.heartbeat().await,
        Err(ClientError::Unauthorized)
    ));
    assert!(matches!(
        controller.commands(0, true).await,
        Err(ClientError::Unauthorized)
    ));

    // A replacement takes the backend over at once, with a higher epoch.
    let (replacement, new_epoch) = harness.controller("k8s").await;
    assert_eq!(new_epoch, epoch + 1);
    replacement
        .report(&report(
            "task-1",
            new_epoch,
            1,
            Observation::Status,
            Some(1),
        ))
        .await
        .expect("replacement reports");
}

#[tokio::test]
async fn a_stale_lease_epoch_report_is_refused_over_http() {
    let harness = Harness::start().await;
    harness.create("task-1", "k8s").await;
    let (old, old_epoch) = harness.controller("k8s").await;

    harness.clock.advance(Duration::minutes(5));
    let (new, new_epoch) = harness.controller("k8s").await;
    assert!(new_epoch > old_epoch);

    let code = refused_code(
        old.report(&report(
            "task-1",
            old_epoch,
            1,
            Observation::StartAccepted,
            None,
        ))
        .await,
    );
    assert_eq!(code, "stale_lease");
    assert_eq!(
        refused_code(old.commands(0, true).await),
        "lease_not_held",
        "a fenced controller is not sent commands either"
    );

    let code = refused_code(
        new.report(&report(
            "task-1",
            new_epoch,
            7,
            Observation::StartAccepted,
            None,
        ))
        .await,
    );
    assert_eq!(code, "stale_generation");

    new.report(&report(
        "task-1",
        new_epoch,
        1,
        Observation::StartAccepted,
        None,
    ))
    .await
    .expect("current lease and generation");
    assert_eq!(
        harness.task("task-1").await.lifecycle,
        TaskLifecycle::Provisioning
    );
}

// ---------------------------------------------------------------------------
// Desired commands and acknowledgements
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_reconnecting_controller_receives_only_commands_newer_than_its_acknowledgement() {
    let harness = Harness::start().await;
    harness.create("task-1", "k8s").await;
    harness.create("task-2", "k8s").await;
    let (controller, credential) = harness.enroll_with_credential("k8s").await;
    controller.acquire_lease(None).await.expect("lease");

    let first = controller.commands(0, false).await.expect("commands");
    assert_eq!(first.commands.len(), 2);
    assert!(first
        .commands
        .iter()
        .all(|command| command.action == DesiredAction::Start && command.generation == 1));

    let acks: Vec<_> = first
        .commands
        .iter()
        .map(|command| CommandAck {
            task_id: command.task_id.to_string(),
            generation: command.generation,
        })
        .collect();
    controller.acknowledge(acks.clone()).await.expect("ack");
    let again = controller.acknowledge(acks).await.expect("duplicate ack");
    assert!(again.acknowledged.iter().all(|ack| ack.generation == 1));
    assert!(controller
        .commands(0, false)
        .await
        .expect("nothing new")
        .commands
        .is_empty());

    harness
        .command("task-1", OwnerCommand::Archive, "archive-1")
        .await;

    // Reconnect: a fresh client on the same identity, a new lease epoch.
    let reconnected = ControllerClient::new(harness.base_url(), credential);
    let lease = reconnected.acquire_lease(None).await.expect("reacquire");
    assert_eq!(lease.epoch, 2);
    let newer = reconnected.commands(0, false).await.expect("commands");
    assert_eq!(newer.lease_epoch, 2);
    assert_eq!(
        newer.commands.len(),
        0,
        "an unaccepted task archives at once"
    );
    assert_eq!(
        harness.task("task-1").await.lifecycle,
        TaskLifecycle::Archived
    );

    // An acknowledgement older than the one recorded never lowers it.
    let lowered = reconnected
        .acknowledge(vec![CommandAck {
            task_id: "task-2".to_string(),
            generation: 0,
        }])
        .await
        .expect("old ack");
    assert_eq!(lowered.acknowledged[0].generation, 1);
    let full = reconnected.commands(0, true).await.expect("full resync");
    let ids: Vec<_> = full
        .commands
        .iter()
        .map(|command| command.task_id.as_str())
        .collect();
    assert_eq!(ids, ["task-2"], "archived tasks are never sent again");
}

#[tokio::test]
async fn a_long_poll_answers_as_soon_as_a_command_arrives() {
    let harness = Harness::start().await;
    let (controller, _) = harness.controller("k8s").await;
    let waiting = tokio::spawn(async move { controller.commands(5, false).await });
    tokio::time::sleep(StdDuration::from_millis(200)).await;
    let started = Instant::now();
    harness.create("task-1", "k8s").await;
    let response = waiting.await.expect("join").expect("commands");
    assert_eq!(response.commands.len(), 1);
    assert!(started.elapsed() < StdDuration::from_secs(4));
}

#[tokio::test]
async fn archive_racing_start_refuses_the_late_start_acceptance() {
    let harness = Harness::start().await;
    harness.create("task-1", "k8s").await;
    let (controller, epoch) = harness.controller("k8s").await;
    let commands = controller.commands(0, false).await.expect("commands");
    assert_eq!(commands.commands[0].action, DesiredAction::Start);

    let archived = harness
        .command("task-1", OwnerCommand::Archive, "archive-1")
        .await;
    assert_eq!(archived.lifecycle, TaskLifecycle::Archived);

    // The controller had not reported StartAccepted yet, so it must not
    // create anything: its late report is refused.
    let code = refused_code(
        controller
            .report(&report(
                "task-1",
                epoch,
                1,
                Observation::StartAccepted,
                None,
            ))
            .await,
    );
    assert_eq!(code, "stale_generation");
    let code = refused_code(
        controller
            .report(&report(
                "task-1",
                epoch,
                2,
                Observation::StartAccepted,
                None,
            ))
            .await,
    );
    assert_eq!(code, "illegal_observation");
}

// ---------------------------------------------------------------------------
// Runner channel
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_runner_with_an_old_incarnation_is_refused() {
    let harness = Harness::start().await;
    let Running {
        controller,
        epoch,
        runner,
    } = harness.running("task-1", "k8s").await;

    let second = controller
        .issue_runner_credential(
            "task-1",
            &RunnerCredentialRequest {
                lease_epoch: epoch,
                generation: 1,
                incarnation: "pod-2".to_string(),
            },
        )
        .await
        .expect("second incarnation");
    let replacement = RunnerClient::new(harness.base_url(), second.credential);

    assert!(matches!(
        runner
            .snapshot(&snapshot(2, ExecutionStatus::Running, 4, None))
            .await,
        Err(ClientError::Unauthorized)
    ));
    assert!(matches!(
        runner.poll(None, 0).await,
        Err(ClientError::Unauthorized)
    ));
    let accepted = replacement
        .snapshot(&snapshot(1, ExecutionStatus::Running, 4, None))
        .await
        .expect("current incarnation");
    assert!(accepted.applied);
    assert_eq!(
        harness.task("task-1").await.runner.unwrap().incarnation,
        "pod-2"
    );
}

#[tokio::test]
async fn a_resume_supersedes_the_paused_runners_credential() {
    let harness = Harness::start().await;
    let Running {
        controller,
        epoch,
        runner,
    } = harness.running("task-1", "k8s").await;
    harness
        .command("task-1", OwnerCommand::Pause, "pause-1")
        .await;
    let mut at_safe_point = snapshot(2, ExecutionStatus::Running, 5, None);
    at_safe_point.safe_point = true;
    runner.snapshot(&at_safe_point).await.expect("safe point");
    controller
        .report(&report(
            "task-1",
            epoch,
            2,
            Observation::PausedAtSafePoint,
            Some(2),
        ))
        .await
        .expect("paused");
    harness
        .command("task-1", OwnerCommand::Resume, "resume-1")
        .await;

    assert!(matches!(
        runner.renew().await,
        Err(ClientError::Unauthorized)
    ));
}

#[tokio::test]
async fn duplicate_snapshots_are_absorbed_and_regressing_cursors_refused() {
    let harness = Harness::start().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    let revision = harness.task("task-1").await.revision;

    let duplicate = runner
        .snapshot(&snapshot(1, ExecutionStatus::Running, 3, None))
        .await
        .expect("redelivery");
    assert!(!duplicate.applied);
    assert_eq!(harness.task("task-1").await.revision, revision);

    let older = refused_code(
        runner
            .snapshot(&snapshot(2, ExecutionStatus::Running, 2, None))
            .await,
    );
    assert_eq!(older, "stale_snapshot");
}

#[tokio::test]
async fn an_offline_runner_leaves_an_explicitly_stale_last_observation() {
    let harness = Harness::start().await;
    harness.running("task-1", "k8s").await;
    let live = harness.task("task-1").await;
    assert_eq!(live.runner.as_ref().unwrap().freshness, Freshness::Live);
    assert_eq!(live.controller.as_ref().unwrap().freshness, Freshness::Live);

    // Runner and controller go quiet; nothing reports anything.
    harness.clock.advance(Duration::minutes(10));
    let stale = harness.task("task-1").await;
    let runner = stale.runner.expect("last runner observation kept");
    assert_eq!(
        stale.lifecycle,
        TaskLifecycle::Running,
        "no fabricated state"
    );
    assert_eq!(runner.execution_status, ExecutionStatus::Running);
    assert_eq!(runner.freshness, Freshness::Stale);
    assert_eq!(
        runner.observed_at,
        snapshot(1, ExecutionStatus::Running, 3, None).observed_at
    );
    assert_eq!(stale.observation.unwrap().freshness, Freshness::Stale);
    assert_eq!(stale.controller.unwrap().freshness, Freshness::Stale);
    assert_eq!(stale.as_of - runner.received_at, Duration::minutes(10));

    match harness
        .owner
        .control_conversation("task-1")
        .await
        .expect("conversation")
    {
        ConversationResponse::Unavailable {
            reason,
            last_attempts,
            observed_at,
        } => {
            assert_eq!(reason, ConversationUnavailableReason::RunnerUnreachable);
            assert_eq!(last_attempts.len(), 1);
            assert!(observed_at.is_some());
        }
        other => panic!("expected unavailable, got {other:?}"),
    }
}

#[tokio::test]
async fn input_for_an_obsolete_pending_input_is_refused_without_reaching_the_runner() {
    let harness = Harness::start().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    runner
        .snapshot(&snapshot(2, ExecutionStatus::InputNeeded, 4, Some("ep-1")))
        .await
        .expect("waiting on ep-1");
    let revision = harness.task("task-1").await.revision;

    let code = refused_code(
        harness
            .owner
            .control_input("task-1", &input_request("ep-0", "answer-0"))
            .await,
    );
    assert_eq!(code, "obsolete_input");
    let after = harness.task("task-1").await;
    assert_eq!(after.revision, revision, "nothing was written");
    assert!(after.inputs.is_empty());
    assert!(runner
        .poll(Some("ep-1"), 0)
        .await
        .expect("poll")
        .inputs
        .is_empty());

    // An answer queued for ep-1 becomes obsolete, undelivered, once the
    // runner reports it moved on to ep-2.
    harness
        .owner
        .control_input("task-1", &input_request("ep-1", "answer-1"))
        .await
        .expect("queued");
    runner
        .snapshot(&snapshot(3, ExecutionStatus::InputNeeded, 6, Some("ep-2")))
        .await
        .expect("moved on");
    let polled = runner.poll(Some("ep-2"), 0).await.expect("poll");
    assert!(polled.inputs.is_empty());
    let inputs = harness.task("task-1").await.inputs;
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].status, InputStatus::Obsolete);

    // Likewise when the runner's own poll says it waits on another request
    // than the one an answer was queued for.
    harness
        .owner
        .control_input("task-1", &input_request("ep-2", "answer-2"))
        .await
        .expect("queued");
    let polled = runner.poll(Some("ep-3"), 0).await.expect("poll");
    assert!(polled.inputs.is_empty(), "never handed to another request");
    let inputs = harness.task("task-1").await.inputs;
    assert_eq!(inputs[1].status, InputStatus::Obsolete);
}

#[tokio::test]
async fn input_is_delivered_only_for_the_matching_pending_input_and_settled_once() {
    let harness = Harness::start().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    runner
        .snapshot(&snapshot(2, ExecutionStatus::InputNeeded, 4, Some("ep-1")))
        .await
        .expect("waiting on ep-1");

    let submitted = harness
        .owner
        .control_input("task-1", &input_request("ep-1", "answer-1"))
        .await
        .expect("queued");
    let retried = harness
        .owner
        .control_input("task-1", &input_request("ep-1", "answer-1"))
        .await
        .expect("idempotent retry");
    assert!(retried.replayed);
    assert_eq!(retried.input.input_id, submitted.input.input_id);
    assert_eq!(
        refused_code(
            harness
                .owner
                .control_input("task-1", &input_request("ep-1", "answer-2"))
                .await
        ),
        "input_already_queued"
    );

    let polled = runner.poll(Some("ep-1"), 0).await.expect("poll");
    assert_eq!(polled.inputs.len(), 1);
    assert_eq!(polled.inputs[0].delivery.episode.as_deref(), Some("ep-1"));
    let redelivered = runner.poll(Some("ep-1"), 0).await.expect("redelivery");
    assert_eq!(
        redelivered.inputs.len(),
        1,
        "redelivered until acknowledged"
    );

    let input_id = polled.inputs[0].input_id.clone();
    let acked = runner.ack_input(&input_id, true, None).await.expect("ack");
    assert_eq!(acked.input.status, InputStatus::Consumed);
    runner
        .ack_input(&input_id, true, None)
        .await
        .expect("duplicate ack is harmless");
    assert_eq!(
        refused_code(runner.ack_input(&input_id, false, None).await),
        "input_already_settled"
    );
    assert!(runner
        .poll(Some("ep-1"), 0)
        .await
        .expect("poll")
        .inputs
        .is_empty());
}

/// AE12 over the control plane: the client reads the timeline and the
/// conversation summary, answers the runner, pauses it at a safe point and
/// then sees the conversation explicitly unavailable — without ever
/// addressing the runner or the backend.
#[tokio::test]
async fn ae12_client_reads_and_answers_a_remote_task_through_the_control_plane() {
    let harness = Harness::start().await;
    let Running {
        controller,
        epoch,
        runner,
    } = harness.running("task-1", "k8s").await;
    runner
        .snapshot(&snapshot(2, ExecutionStatus::InputNeeded, 4, Some("ep-1")))
        .await
        .expect("waiting on ep-1");

    let timeline = harness
        .owner
        .control_timeline("task-1")
        .await
        .expect("timeline");
    assert_eq!(timeline.freshness, Freshness::Live);
    assert_eq!(
        timeline.timeline.unwrap().default_position.as_deref(),
        Some("plan#1")
    );
    match harness
        .owner
        .control_conversation("task-1")
        .await
        .expect("conversation")
    {
        ConversationResponse::Available { attempts, .. } => {
            assert_eq!(attempts[0].attempt_id, "act-6");
            assert_eq!(attempts[0].entries, 42);
        }
        other => panic!("expected available, got {other:?}"),
    }
    let summary = harness.task("task-1").await;
    let waiting = summary.runner.unwrap().pending_input.unwrap();
    assert_eq!(waiting.form.request.instruction, "Approve the plan?");

    // The answer travels owner → control plane → runner long-poll.
    let poll = {
        let runner = runner.clone();
        tokio::spawn(async move { runner.poll(Some("ep-1"), 5).await })
    };
    tokio::time::sleep(StdDuration::from_millis(200)).await;
    harness
        .owner
        .control_input("task-1", &input_request(&waiting.id, "answer-1"))
        .await
        .expect("submit");
    let delivered = poll.await.expect("join").expect("poll");
    assert_eq!(delivered.directive, RunnerDirective::Continue);
    assert_eq!(
        delivered.inputs[0].delivery.comment.as_deref(),
        Some("looks good")
    );
    runner
        .ack_input(&delivered.inputs[0].input_id, true, None)
        .await
        .expect("ack");
    runner
        .snapshot(&snapshot(3, ExecutionStatus::Running, 6, None))
        .await
        .expect("answered");

    // Pause: the controller may not remove compute before the safe point.
    let paused = harness
        .command("task-1", OwnerCommand::Pause, "pause-1")
        .await;
    assert_eq!(paused.lifecycle, TaskLifecycle::PauseRequested);
    assert_eq!(
        refused_code(
            controller
                .report(&report(
                    "task-1",
                    epoch,
                    2,
                    Observation::PausedAtSafePoint,
                    Some(2)
                ))
                .await
        ),
        "safe_point_not_acknowledged"
    );
    let directive = runner.poll(None, 0).await.expect("poll").directive;
    assert_eq!(directive, RunnerDirective::PauseAtSafePoint);
    let mut at_safe_point = snapshot(4, ExecutionStatus::Running, 7, None);
    at_safe_point.safe_point = true;
    runner.snapshot(&at_safe_point).await.expect("safe point");
    let command = controller
        .commands(0, true)
        .await
        .expect("commands")
        .commands;
    assert_eq!(command[0].action, DesiredAction::PauseAtSafePoint);
    assert!(command[0].runner.as_ref().unwrap().safe_point_acknowledged);
    controller
        .report(&report(
            "task-1",
            epoch,
            2,
            Observation::PausedAtSafePoint,
            Some(2),
        ))
        .await
        .expect("paused at safe point");

    match harness
        .owner
        .control_conversation("task-1")
        .await
        .expect("conversation")
    {
        ConversationResponse::Unavailable {
            reason,
            last_attempts,
            ..
        } => {
            assert_eq!(reason, ConversationUnavailableReason::Paused);
            assert_eq!(last_attempts[0].attempt_id, "act-6", "last safe summary");
        }
        other => panic!("expected unavailable, got {other:?}"),
    }
    let summary = harness.task("task-1").await;
    assert_eq!(summary.lifecycle, TaskLifecycle::Paused);
    assert_eq!(summary.inputs[0].status, InputStatus::Consumed);
    assert_eq!(summary.runner.unwrap().freshness, Freshness::Stale);
}

#[tokio::test]
async fn owner_scope_hides_other_owners_tasks() {
    let harness = Harness::start().await;
    let foreign = TaskId::parse("foreign").unwrap();
    let control = harness.control_plane();
    control
        .create_task(NewTask::new(
            foreign.clone(),
            OwnerRef::new("someone-else"),
            narsor::domain::BackendId::parse("k8s").unwrap(),
            narsor::domain::TaskLaunchSpec::new(
                "wf",
                1,
                serde_json::json!({}),
                narsor::domain::TaskWorkspace::None,
                narsor::domain::ResolvedEnvironment::harness_only("claude-code"),
                narsor::domain::ResourceProfile::new(1000, 2048),
            ),
        ))
        .expect("foreign task");

    let listed = harness.owner.control_tasks().await.expect("list");
    assert!(listed.tasks.is_empty());
    assert!(listed.cursor.is_empty());
    assert!(matches!(
        harness.owner.control_task("foreign").await,
        Err(ClientError::NotFound)
    ));
    assert!(matches!(
        harness.owner.control_events("foreign", 0).await,
        Err(ClientError::NotFound)
    ));
    assert!(matches!(
        harness
            .owner
            .control_command(
                "foreign",
                &OwnerCommandRequest {
                    command: OwnerCommand::Archive,
                    idempotency_key: "a".to_string(),
                    expected_generation: None,
                },
            )
            .await,
        Err(ClientError::NotFound)
    ));
    assert_eq!(
        control.task(&foreign).unwrap().lifecycle,
        TaskLifecycle::Queued
    );
}

// ---------------------------------------------------------------------------
// Daemon startup
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_daemon_recovers_the_control_plane_at_startup_and_serves_it() {
    let root = TempRoot::new();
    let paths = StatePaths::new(root.path());
    let control = ControlPlane::new(paths.clone());
    let task = TaskId::parse("task-1").unwrap();
    control
        .create_task(NewTask::new(
            task.clone(),
            OwnerRef::new(narsor::controller_protocol::LOCAL_OWNER),
            narsor::domain::BackendId::parse("k8s").unwrap(),
            narsor::domain::TaskLaunchSpec::new(
                "wf",
                1,
                serde_json::json!({}),
                narsor::domain::TaskWorkspace::None,
                narsor::domain::ResolvedEnvironment::harness_only("claude-code"),
                narsor::domain::ResourceProfile::new(1000, 2048),
            ),
        ))
        .expect("create");
    control
        .command(&task, CommandRequest::new(OwnerCommand::Archive, "a"))
        .expect("archive");

    // A crash after committing revision 2 but before appending its event,
    // plus an interrupted write.
    let event_file = paths.orchestration_task_event_file("task-1", 2);
    fs::remove_file(&event_file).expect("drop event 2");
    let orphan = temp_path_for(&paths.orchestration_task_file("task-1"));
    fs::write(&orphan, b"{ torn").expect("orphaned temp file");

    let mut daemon = spawn_daemon(root.path());
    let info = wait_for_daemon_info(root.path(), StdDuration::from_secs(10));

    assert!(event_file.exists(), "recover() republished the event");
    assert!(!orphan.exists(), "recover() removed the orphan");
    let client = ApiClient::new(info.addr, info.token.clone());
    let events = client.control_events("task-1", 0).await.expect("events");
    assert_eq!(
        events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        [1, 2]
    );

    // SAFETY: a plain signal to the child this test spawned.
    unsafe {
        libc::kill(info.pid as libc::pid_t, libc::SIGTERM);
    }
    wait_for_exit(&mut daemon, StdDuration::from_secs(20));
}

fn spawn_daemon(root: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_narsor"))
        .arg("--root")
        .arg(root)
        .arg("daemon")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the daemon")
}

fn wait_for_daemon_info(root: &Path, timeout: StdDuration) -> DaemonInfo {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(info) = DaemonInfo::read(&root.join("daemon.json")) {
            return info;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon never became discoverable"
        );
        std::thread::sleep(StdDuration::from_millis(20));
    }
}

fn wait_for_exit(child: &mut Child, timeout: StdDuration) {
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait().expect("poll child").is_some() {
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("the daemon did not exit");
        }
        std::thread::sleep(StdDuration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------
// Launch resolution on the create path (U6)
// ---------------------------------------------------------------------------

/// The `400` reason of a refused request.
fn request_code(result: Result<impl std::fmt::Debug, ClientError>) -> String {
    match result {
        Err(ClientError::Request { code, .. }) => code,
        other => panic!("expected a 400 with a reason, got {other:?}"),
    }
}

/// Every file under `dir`, recursively.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(files_under(&path));
        } else {
            files.push(path);
        }
    }
    files
}

#[tokio::test]
async fn a_launch_that_does_not_resolve_is_refused_and_stores_nothing() {
    let harness = Harness::start().await;
    let catalog = narsor::environment::EnvironmentCatalog::new(harness.root.path());
    let mut bad_limits = environment_support::backend("backend-a");
    bad_limits.limits.max_concurrent_tasks = 0;
    fs::write(
        harness.root.path().join("backends/backend-a.yaml"),
        serde_yaml::to_string(&bad_limits).unwrap(),
    )
    .unwrap();
    catalog
        .save_profile(&narsor::environment::ProfileRecord {
            id: "unregistered".to_string(),
            version: 1,
            secrets: vec!["aws-key".to_string()],
            ..Default::default()
        })
        .unwrap();
    fs::write(
        harness.root.path().join("workflows/no-image.yaml"),
        environment_support::workflow_yaml("no-image", None),
    )
    .unwrap();

    let mut cases = Vec::new();
    let mut request = create_request("task-1", "nowhere");
    cases.push(("unknown_backend", request.clone()));
    request.backend_id = Some("backend-a".to_string());
    cases.push(("invalid_backend", request.clone()));
    request = create_request("task-1", "k8s");
    request.launch.profile = Some("unregistered".to_string());
    cases.push(("unregistered_secret", request.clone()));
    request = create_request("task-1", "k8s");
    request.launch.harness = Some("nonesuch".to_string());
    cases.push(("unsupported_harness", request.clone()));
    request = create_request("task-1", "k8s");
    request.launch.workflow_id = "no-image".to_string();
    cases.push(("missing_base_image", request.clone()));

    for (code, request) in cases {
        assert_eq!(
            request_code(harness.owner.create_control_task(&request).await),
            code
        );
    }

    let listed = harness.owner.control_tasks().await.expect("list");
    assert!(listed.tasks.is_empty(), "no partial assignment");
    let state = StatePaths::new(harness.root.path());
    assert!(
        files_under(&state.orchestration_dir()).is_empty(),
        "no task record, event or lock was written: {:?}",
        files_under(&state.orchestration_dir())
    );
}

#[tokio::test]
async fn the_create_path_uses_the_owner_default_backend_unless_overridden() {
    let harness = Harness::start().await;
    let mut request = create_request("task-1", "k8s");
    request.backend_id = None;
    let by_default = harness
        .owner
        .create_control_task(&request)
        .await
        .expect("create");
    assert_eq!(by_default.backend_id.as_str(), "k8s");

    let explicit = harness.create("task-2", "backend-b").await;
    assert_eq!(explicit.backend_id.as_str(), "backend-b");
    assert_eq!(
        by_default.environment_identity, explicit.environment_identity,
        "the same images on two backends of the same kind"
    );
}

#[tokio::test]
async fn an_existing_task_keeps_its_resolved_environment_after_configuration_changes() {
    let harness = Harness::start().await;
    let first = harness.create("task-1", "k8s").await;

    // The operator moves the workflow to a new image afterwards.
    fs::write(
        harness.root.path().join("workflows/implement-feature.yaml"),
        environment_support::workflow_yaml(
            "implement-feature",
            Some(&format!(
                "ghcr.io/acme/dev@{}",
                environment_support::digest('7')
            )),
        ),
    )
    .unwrap();
    let second = harness.create("task-2", "k8s").await;
    assert_ne!(second.environment_identity, first.environment_identity);

    let again = harness.task("task-1").await;
    assert_eq!(again.environment_identity, first.environment_identity);
    assert_eq!(again.launch_digest, first.launch_digest);

    let (controller, _) = harness.controller("k8s").await;
    let commands = controller.commands(0, true).await.expect("commands");
    let spec = &commands
        .commands
        .iter()
        .find(|command| command.task_id.as_str() == "task-1")
        .expect("task-1 command")
        .launch_spec;
    assert_eq!(
        spec.environment.base_image.as_ref().unwrap().pinned,
        environment_support::workflow_image(),
        "the controller still receives the digest pinned at creation"
    );
}

#[tokio::test]
async fn secret_values_never_reach_records_responses_or_events() {
    // A credential value as the backend's own store would hold it. The
    // control plane is only ever told where it lives.
    const SENTINEL: &str = "SENTINEL-s3cr3t-value-6f1c";
    let value = narsor::domain::SecretValue::new(SENTINEL);
    assert!(!format!("{value:?}").contains(SENTINEL));

    let harness = Harness::start().await;
    narsor::environment::EnvironmentCatalog::new(harness.root.path())
        .save_profile(&narsor::environment::ProfileRecord {
            id: "default".to_string(),
            version: 1,
            secrets: vec!["github-token".to_string()],
            ..Default::default()
        })
        .unwrap();
    let summary = harness.create("task-1", "k8s").await;
    let (controller, _) = harness.controller("k8s").await;
    let commands = controller.commands(0, true).await.expect("commands");
    let events = harness
        .owner
        .control_events("task-1", 0)
        .await
        .expect("events");

    let spec = &commands.commands[0].launch_spec;
    assert_eq!(spec.environment.secrets.len(), 1, "the reference is there");
    let mut serialized = vec![
        serde_json::to_string(&summary).unwrap(),
        serde_json::to_string(&commands).unwrap(),
        serde_json::to_string(&events).unwrap(),
        serde_json::to_string(spec).unwrap(),
    ];
    for path in files_under(harness.root.path()) {
        if let Ok(text) = fs::read_to_string(&path) {
            serialized.push(text);
        }
    }
    assert!(serialized
        .iter()
        .any(|text| text.contains("narsor-github/token")));
    for text in serialized {
        assert!(!text.contains(SENTINEL), "a secret value leaked: {text}");
    }
}

#[tokio::test]
async fn a_launch_request_with_an_unknown_field_is_refused() {
    let harness = Harness::start().await;
    let mut body = serde_json::to_value(create_request("task-1", "k8s")).unwrap();
    body["launch"]["secret_value"] = serde_json::json!("hunter2");
    let response = reqwest::Client::new()
        .post(format!("{}/control/v1/tasks", harness.base_url()))
        .bearer_auth(harness.handle.token())
        .json(&body)
        .send()
        .await
        .expect("request");
    assert!(response.status().is_client_error(), "{}", response.status());
    assert!(harness
        .owner
        .control_tasks()
        .await
        .unwrap()
        .tasks
        .is_empty());
}
