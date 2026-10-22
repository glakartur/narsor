//! Integration tests for the controller and runner protocol (U2).
//!
//! Everything goes over real HTTP against the real API server, through the
//! real typed clients: an owner (`ApiClient`), controllers enrolled with
//! one-time codes (`ControllerClient`) and runners holding incarnation
//! credentials (`RunnerClient`). The control plane runs on a hand-moved
//! clock, so lease expiry and staleness are deterministic; long-polls use
//! real time but are only ever asked to wait briefly.

#[path = "engine_support/mod.rs"]
mod engine_support;
mod environment_support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Duration, TimeZone, Utc};
use narsor_api_client::api::{ApiClient, ClientError};
use narsor_engine::controller_protocol::{
    CommandAck, ConductorReadQuery, ConductorReadResponse, ControllerClient, ConversationPageQuery,
    ConversationResponse, ConversationUnavailableReason, CreateEnrollmentRequest,
    CreateTaskRequest, DesiredAction, ExecutionEventsPage, ExecutionEventsQuery,
    ExecutionEventsResponse, ExecutionEventsSource, Freshness, LaunchRequest, OwnerCommandRequest,
    ProtocolConfig, ProtocolService, RelayFailure, ReportRequest, RunnerClient,
    RunnerCredentialRequest, RunnerDirective, SubmitCommentRequest, SubmitInputRequest,
    SubmitOperatorCommandRequest, TaskActionId, TaskActionOffer, TaskDefaultHarness, TaskState,
    TaskSummary, LOCAL_OWNER,
};
use narsor_engine::domain::{
    BackendId, DisplayLink, Event, EventData, ExecutionStatus, TaskDisplay, TaskId,
};
use narsor_engine::engine::{Engine, PendingHumanInput, Timeline};
use narsor_engine::orchestration::{
    AttentionItem, AttentionKind, Clock, CommandRequest, ControlPlane, ConversationAttemptSummary,
    ConversationAvailability, ExecutionCursor, InputStatus, NewTask, Observation, ObservedSummary,
    OperatorCommand, OperatorCommandStatus, OwnerCommand, OwnerRef, PendingInputRef,
    ResourceIdentity, ResumePolicy, RunnerSnapshot, RunnerState, TaskEventKind,
};
use narsor_engine::persistence::paths::{temp_path_for, StatePaths};
use narsor_engine::persistence::{DaemonInfo, DefaultHarnessSelector, ExecutionStore};
use narsor_server::api::{self, ApiServices, ConversationResponse as PageResponse};

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

/// The policy the tests run under: the default, but a read that finds no
/// reader connected does not wait for one long (D63).
fn test_config() -> ProtocolConfig {
    ProtocolConfig {
        reader_connect_wait: StdDuration::from_millis(50),
        ..ProtocolConfig::default()
    }
}

/// A served control plane plus an owner client.
struct Harness {
    root: TempRoot,
    clock: Arc<ManualClock>,
    handle: api::ApiHandle,
    owner: ApiClient,
    /// The served protocol service, for what the daemon drives in-process
    /// (the resume policy's pass).
    service: Arc<ProtocolService>,
}

/// A task brought to `Running` by a controller, with its runner.
struct Running {
    controller: ControllerClient,
    epoch: u64,
    runner: RunnerClient,
}

impl Harness {
    async fn start() -> Self {
        Self::start_with(test_config()).await
    }

    /// A harness with its own timing policy.
    async fn start_with(config: ProtocolConfig) -> Self {
        Self::start_inner(config, false).await
    }

    /// A harness whose daemon has a default-harness selector (configured
    /// `claude-code`) that running tasks follow (U7).
    async fn start_with_default_harness() -> Self {
        Self::start_inner(test_config(), true).await
    }

    async fn start_inner(config: ProtocolConfig, with_selector: bool) -> Self {
        let root = TempRoot::new();
        let clock = ManualClock::new();
        let control = ControlPlane::with_clock(StatePaths::new(root.path()), clock.clone());
        let selector = if with_selector {
            Some(Arc::new(
                DefaultHarnessSelector::load(
                    StatePaths::new(root.path()),
                    vec!["claude-code".into(), "codex".into(), "opencode".into()],
                    "claude-code",
                )
                .await
                .expect("load the harness selector"),
            ))
        } else {
            None
        };
        let resolver: Arc<dyn narsor_engine::environment::LaunchResolver> = match &selector {
            Some(selector) => environment_support::seeded_catalog_resolver_with(
                root.path(),
                Arc::clone(selector) as Arc<dyn narsor_engine::environment::HarnessDefaults>,
            ),
            None => environment_support::seeded_resolver(root.path()),
        };
        let service = Arc::new(ProtocolService::with_config(control, resolver, config));
        let services = ApiServices {
            control: Some(Arc::clone(&service)),
            harness_selector: selector,
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
            service,
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
        let (client, _) = ControllerClient::enroll(self.base_url(), &code.enrollment_code)
            .await
            .expect("enroll");
        let credential = client.credential().expect("a signing credential");
        (client, credential)
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
        problem: None,
        attention: Vec::new(),
        conductor_available: false,
        conductor_unavailable_reason: None,
        default_harness_unavailable: None,
        display: None,
        observed_at: Utc.with_ymd_and_hms(2026, 10, 3, 12, 0, 5).unwrap(),
    }
}

fn delivery(comment: &str) -> narsor_protocol::api::HumanInputDeliveryDto {
    narsor_protocol::api::HumanInputDeliveryDto {
        comment: Some(comment.to_string()),
        ..narsor_protocol::api::HumanInputDeliveryDto::default()
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
        harness.task("task-b").await.runner_state,
        RunnerState::Queued
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
        harness.task("task-1").await.runner_state,
        RunnerState::Provisioning
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
        harness.task("task-1").await.runner_state,
        RunnerState::Archived
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
    assert_eq!(archived.runner_state, RunnerState::Archived);

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
async fn the_summary_tells_the_owner_the_task_state_and_whether_it_holds_a_slot() {
    let harness = Harness::start().await;
    let queued = harness.create("task-1", "k8s").await;
    assert_eq!(queued.state, TaskState::Queued);

    let Running {
        controller,
        epoch,
        runner,
    } = harness.running("task-1", "k8s").await;
    let running = harness.task("task-1").await;
    assert_eq!(running.state, TaskState::Running);
    assert!(running.holds_slot);

    runner
        .snapshot(&snapshot(
            2,
            ExecutionStatus::InputNeeded,
            4,
            Some("input-1"),
        ))
        .await
        .expect("input needed");
    let waiting = harness.task("task-1").await;
    assert_eq!(waiting.state, TaskState::HumanNeeded);
    assert!(waiting.holds_slot);

    // A finished run is stopped at once; the task holds its slot until the
    // runner reached its safe point.
    runner
        .snapshot(&snapshot(3, ExecutionStatus::Completed, 5, None))
        .await
        .expect("completed");
    let finished = harness.task("task-1").await;
    assert_eq!(finished.runner_state, RunnerState::PauseRequested);
    assert_eq!(finished.state, TaskState::Finished);
    assert!(finished.holds_slot);

    // Stopped at a safe point, the task is still finished and holds nothing.
    let mut at_safe_point = snapshot(4, ExecutionStatus::Completed, 6, None);
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
    let stopped = harness.task("task-1").await;
    assert_eq!(stopped.runner_state, RunnerState::Stopped);
    assert_eq!(stopped.state, TaskState::Finished);
    assert!(!stopped.holds_slot);
}

#[test]
fn a_summary_from_before_state_and_slot_were_sent_still_loads() {
    let at = Utc.with_ymd_and_hms(2026, 10, 9, 12, 0, 0).unwrap();
    let summary: TaskSummary = serde_json::from_value(serde_json::json!({
        "task_id": "task-1",
        "backend_id": "k8s",
        "runner_state": "running",
        "generation": 1,
        "revision": 1,
        "rank": 1,
        "created_at": at,
        "queue_sequence": 1,
        "workflow_id": "implement-feature",
        "workflow_version": 3,
        "launch_digest": "digest",
        "environment_identity": "env",
        "archive": null,
        "observation": null,
        "runner": null,
        "controller": null,
        "inputs": [],
        "as_of": at,
    }))
    .expect("an old summary");
    assert_eq!(summary.state, TaskState::Unrecognized);
    assert!(!summary.holds_slot);

    let unknown: TaskState =
        serde_json::from_value(serde_json::json!("from_the_future")).expect("an unknown state");
    assert_eq!(unknown, TaskState::Unrecognized);
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
        stale.runner_state,
        RunnerState::Running,
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
            message,
        } => {
            assert_eq!(reason, ConversationUnavailableReason::RunnerUnreachable);
            assert_eq!(message, None);
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
    assert_eq!(waiting.form["instruction"], "Approve the plan?");

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
    assert_eq!(paused.runner_state, RunnerState::PauseRequested);
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
    assert_eq!(summary.runner_state, RunnerState::Stopped);
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
            narsor_engine::domain::BackendId::parse("k8s").unwrap(),
            narsor_engine::domain::TaskLaunchSpec::new(
                "wf",
                1,
                serde_json::json!({}),
                narsor_engine::domain::TaskWorkspace::None,
                narsor_engine::domain::ResolvedEnvironment::harness_only("claude-code"),
                narsor_engine::domain::ResourceProfile::new(1000, 2048),
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
        control.task(&foreign).unwrap().runner_state,
        RunnerState::Queued
    );
}

// ---------------------------------------------------------------------------
// Daemon startup
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "starts a narsor daemon; run with --ignored"]
async fn the_daemon_recovers_the_control_plane_at_startup_and_serves_it() {
    let root = TempRoot::new();
    let paths = StatePaths::new(root.path());
    let control = ControlPlane::new(paths.clone());
    let task = TaskId::parse("task-1").unwrap();
    control
        .create_task(NewTask::new(
            task.clone(),
            OwnerRef::new(narsor_engine::controller_protocol::LOCAL_OWNER),
            narsor_engine::domain::BackendId::parse("k8s").unwrap(),
            narsor_engine::domain::TaskLaunchSpec::new(
                "wf",
                1,
                serde_json::json!({}),
                narsor_engine::domain::TaskWorkspace::None,
                narsor_engine::domain::ResolvedEnvironment::harness_only("claude-code"),
                narsor_engine::domain::ResourceProfile::new(1000, 2048),
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
    Command::new(engine_support::workspace_binary("narsor-server"))
        .arg("--root")
        .arg(root)
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
    let catalog = narsor_engine::environment::EnvironmentCatalog::new(harness.root.path());
    let mut bad_limits = environment_support::backend("backend-a");
    bad_limits.limits.max_concurrent_tasks = 0;
    fs::write(
        harness.root.path().join("backends/backend-a.yaml"),
        serde_yaml::to_string(&bad_limits).unwrap(),
    )
    .unwrap();
    catalog
        .save_profile(&narsor_engine::environment::ProfileRecord {
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
    let spec = commands
        .commands
        .iter()
        .find(|command| command.task_id.as_str() == "task-1")
        .expect("task-1 command")
        .launch_spec
        .clone();
    let spec: narsor_engine::domain::TaskLaunchSpec =
        serde_json::from_value(spec).expect("decode protocol launch spec");
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
    let value = narsor_engine::domain::SecretValue::new(SENTINEL);
    assert!(!format!("{value:?}").contains(SENTINEL));

    let harness = Harness::start().await;
    narsor_engine::environment::EnvironmentCatalog::new(harness.root.path())
        .save_profile(&narsor_engine::environment::ProfileRecord {
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

    let spec: narsor_engine::domain::TaskLaunchSpec =
        serde_json::from_value(commands.commands[0].launch_spec.clone())
            .expect("decode protocol launch spec");
    assert_eq!(spec.environment.secrets.len(), 1, "the reference is there");
    let mut serialized = vec![
        serde_json::to_string(&summary).unwrap(),
        serde_json::to_string(&commands).unwrap(),
        serde_json::to_string(&events).unwrap(),
        serde_json::to_string(&spec).unwrap(),
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

// ---------------------------------------------------------------------------
// Task display (U2)
// ---------------------------------------------------------------------------

/// Replaces the seeded `implement-feature` with one that declares a display.
fn write_display_workflow(harness: &Harness) {
    let yaml = environment_support::workflow_yaml(
        "implement-feature",
        Some(&environment_support::workflow_image()),
    )
    .replacen(
        "states:",
        "inputs:\n  - name: ticket\n    type: string\n    required: true\n\
         display:\n  title: \"{{ inputs.ticket }}\"\n  links:\n    - label: Jira\n      \
         text: \"{{ inputs.ticket }}\"\n      url: \"https://jira.example/browse/{{ inputs.ticket }}\"\n    \
         - label: PR\n      text: \"!{{ context.variables.pr_number }}\"\n      \
         url: \"{{ context.variables.pr_url }}\"\nstates:",
        1,
    );
    fs::write(
        harness.root.path().join("workflows/implement-feature.yaml"),
        yaml,
    )
    .unwrap();
}

#[tokio::test]
async fn a_new_task_shows_the_display_rendered_from_its_sealed_inputs() {
    let harness = Harness::start().await;
    write_display_workflow(&harness);

    let task = harness.create("task-1", "k8s").await;

    assert_eq!(task.display.title, "NAR-1");
    assert_eq!(task.display.links.len(), 2);
    assert_eq!(
        task.display.links[0].url.as_deref(),
        Some("https://jira.example/browse/NAR-1")
    );
    assert_eq!(task.display.links[1].text, "—");
    assert_eq!(task.display.links[1].url, None, "no PR yet");
}

#[tokio::test]
async fn a_task_of_a_workflow_without_display_has_an_empty_one() {
    let harness = Harness::start().await;

    let task = harness.create("task-1", "k8s").await;

    assert_eq!(task.display, TaskDisplay::default());
}

#[tokio::test]
async fn the_runners_display_report_replaces_the_creation_rendering_and_is_sanitised() {
    let harness = Harness::start().await;
    write_display_workflow(&harness);
    let running = harness.running("task-1", "k8s").await;
    // The runner's first snapshot carried no display: creation's stays.
    assert_eq!(harness.task("task-1").await.display.title, "NAR-1");

    let mut reported = snapshot(2, ExecutionStatus::Running, 4, None);
    reported.display = Some(TaskDisplay {
        title: "NAR-1 \u{1b}]8;;https://evil.example\u{7}click\u{1b}]8;;\u{7}".to_string(),
        links: vec![
            DisplayLink {
                label: "PR\u{1b}[31m".to_string(),
                text: "!42".to_string(),
                url: Some("https://git.example/pr/42".to_string()),
            },
            DisplayLink {
                label: "Evil".to_string(),
                text: "x\u{1b}[2J".to_string(),
                url: Some("javascript:alert(1)".to_string()),
            },
            DisplayLink {
                label: "File".to_string(),
                text: "f".to_string(),
                url: Some("file:///etc/passwd".to_string()),
            },
        ],
    });
    running.runner.snapshot(&reported).await.expect("snapshot");

    let display = harness.task("task-1").await.display;
    assert_eq!(display.title, "NAR-1 ]8;;https://evil.exampleclick]8;;");
    assert!(!display.title.chars().any(char::is_control));
    assert_eq!(display.links[0].label, "PR[31m");
    assert_eq!(
        display.links[0].url.as_deref(),
        Some("https://git.example/pr/42")
    );
    assert_eq!(display.links[1].url, None, "javascript: is dropped");
    assert_eq!(display.links[1].text, "x[2J");
    assert_eq!(display.links[2].url, None, "file: is dropped");
}

#[tokio::test]
async fn a_title_with_an_escape_sequence_from_the_inputs_is_stripped() {
    let harness = Harness::start().await;
    write_display_workflow(&harness);
    let mut request = create_request("task-1", "k8s");
    request.launch.inputs = serde_json::json!({ "ticket": "\u{1b}[31mNAR-9\u{1b}[0m" });

    let task = harness
        .owner
        .create_control_task(&request)
        .await
        .expect("create task");

    assert_eq!(task.display.title, "[31mNAR-9[0m");
    assert!(!task.display.links[0].text.chars().any(char::is_control));
}

// ---------------------------------------------------------------------------
// Operator commands (U3)
// ---------------------------------------------------------------------------

fn operator_request(command: OperatorCommand, key: &str) -> SubmitOperatorCommandRequest {
    SubmitOperatorCommandRequest {
        command,
        idempotency_key: key.to_string(),
        expected_generation: None,
    }
}

fn transition(target: &str, forced: bool, reason: Option<&str>) -> OperatorCommand {
    OperatorCommand::Transition {
        target: target.to_string(),
        forced,
        reason: reason.map(str::to_string),
    }
}

fn restart(note: Option<&str>, failure_key: Option<&str>) -> OperatorCommand {
    OperatorCommand::RestartStep {
        note: note.map(str::to_string),
        failure_key: failure_key.map(str::to_string),
    }
}

/// A workflow whose `work` state allows overrides and whose `locked` state
/// does not, written over the seeded `implement-feature`.
fn write_override_workflow(harness: &Harness) {
    let yaml = format!(
        "name: implement-feature\nversion: 1\nglobals:\n  image: \"{}\"\nstates:\n  \
         work:\n    allow_manual_override: true\n    action:\n      type: script\n      \
         command: \"true\"\n    choices: [success]\n    transitions:\n      success: locked\n  \
         locked:\n    action:\n      type: script\n      command: \"true\"\n    \
         choices: [success]\n    transitions:\n      success: done\n  \
         done:\n    description: finished\nterminal_states: [done]\n",
        environment_support::workflow_image()
    );
    fs::write(
        harness.root.path().join("workflows/implement-feature.yaml"),
        yaml,
    )
    .unwrap();
}

/// A snapshot of an execution standing in `state`.
fn snapshot_in(sequence: u64, state: &str, event_cursor: u64) -> RunnerSnapshot {
    let mut snapshot = snapshot(sequence, ExecutionStatus::Running, event_cursor, None);
    snapshot.execution_cursor.current_state = Some(state.to_string());
    snapshot
}

fn bad_request_code(result: Result<impl std::fmt::Debug, ClientError>) -> String {
    match result {
        Err(ClientError::Request { code, .. }) => code,
        other => panic!("expected a 400 with a code, got {other:?}"),
    }
}

async fn operator_events(harness: &Harness, task: &str) -> Vec<TaskEventKind> {
    harness
        .owner
        .control_events(task, 0)
        .await
        .expect("events")
        .into_iter()
        .map(|event| event.kind)
        .collect()
}

#[tokio::test]
async fn an_operator_command_is_queued_once_and_redelivered_until_acknowledged() {
    let harness = Harness::start().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;

    let queued = harness
        .owner
        .control_operator_command(
            "task-1",
            &operator_request(OperatorCommand::Interrupt, "interrupt-1"),
        )
        .await
        .expect("queued");
    assert!(!queued.replayed);
    assert_eq!(queued.command.status, OperatorCommandStatus::Queued);
    let retried = harness
        .owner
        .control_operator_command(
            "task-1",
            &operator_request(OperatorCommand::Interrupt, "interrupt-1"),
        )
        .await
        .expect("idempotent retry");
    assert!(retried.replayed);
    assert_eq!(retried.command.command_id, queued.command.command_id);
    assert_eq!(harness.task("task-1").await.operator_commands.len(), 1);
    assert_eq!(
        refused_code(
            harness
                .owner
                .control_operator_command(
                    "task-1",
                    &operator_request(restart(None, None), "interrupt-1"),
                )
                .await
        ),
        "idempotency_key_reused"
    );

    // Handed over by the long-poll, again and again until acknowledged.
    let polled = runner.poll(None, 0).await.expect("poll");
    assert_eq!(polled.commands.len(), 1);
    assert_eq!(polled.commands[0].command, OperatorCommand::Interrupt);
    assert_eq!(polled.commands[0].requested_by, "local");
    let redelivered = runner.poll(None, 0).await.expect("redelivery");
    assert_eq!(redelivered.commands, polled.commands);

    let command_id = polled.commands[0].command_id.clone();
    let acked = runner
        .ack_command(&command_id, true, None)
        .await
        .expect("ack");
    assert_eq!(acked.command.status, OperatorCommandStatus::Applied);
    runner
        .ack_command(&command_id, true, None)
        .await
        .expect("a repeated verdict is harmless");
    assert_eq!(
        refused_code(
            runner
                .ack_command(&command_id, false, Some("late".to_string()))
                .await
        ),
        "operator_command_already_settled"
    );
    assert!(runner
        .poll(None, 0)
        .await
        .expect("poll")
        .commands
        .is_empty());
    assert!(matches!(
        runner.ack_command("command-99", true, None).await,
        Err(ClientError::NotFound)
    ));

    // A settled command still answers its key, whatever became of it.
    let again = harness
        .owner
        .control_operator_command(
            "task-1",
            &operator_request(OperatorCommand::Interrupt, "interrupt-1"),
        )
        .await
        .expect("retry after settling");
    assert!(again.replayed);
    assert_eq!(again.command.status, OperatorCommandStatus::Applied);

    // Both halves are in the audit log, in order.
    let kinds = operator_events(&harness, "task-1").await;
    let queued_at = kinds
        .iter()
        .position(|kind| {
            matches!(
                kind,
                TaskEventKind::OperatorCommandQueued { command_id: id, requested_by, .. }
                    if *id == command_id && requested_by.0 == "local"
            )
        })
        .expect("a queued event");
    let resolved_at = kinds
        .iter()
        .position(|kind| {
            matches!(
                kind,
                TaskEventKind::OperatorCommandResolved {
                    status: OperatorCommandStatus::Applied,
                    ..
                }
            )
        })
        .expect("a resolved event");
    assert!(queued_at < resolved_at);
}

#[tokio::test]
async fn a_rejection_by_the_runner_carries_its_reason() {
    let harness = Harness::start().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    harness
        .owner
        .control_operator_command(
            "task-1",
            &operator_request(restart(Some("try again"), Some("failure-9")), "restart-1"),
        )
        .await
        .expect("queued");
    let polled = runner.poll(None, 0).await.expect("poll");
    assert_eq!(
        polled.commands[0].command,
        restart(Some("try again"), Some("failure-9"))
    );

    runner
        .ack_command(
            &polled.commands[0].command_id,
            false,
            Some("the execution has moved on from that failure".to_string()),
        )
        .await
        .expect("ack");

    let command = harness.task("task-1").await.operator_commands.remove(0);
    assert_eq!(command.status, OperatorCommandStatus::Rejected);
    assert_eq!(
        command.detail.as_deref(),
        Some("the execution has moved on from that failure")
    );
    assert!(operator_events(&harness, "task-1")
        .await
        .iter()
        .any(|kind| matches!(
            kind,
            TaskEventKind::OperatorCommandResolved {
                status: OperatorCommandStatus::Rejected,
                detail: Some(detail),
                ..
            } if detail.contains("moved on")
        )));
}

#[tokio::test]
async fn a_command_for_a_task_that_is_not_running_is_refused_at_once_with_the_reason() {
    let harness = Harness::start().await;
    harness.create("task-1", "k8s").await;
    let revision = harness.task("task-1").await.revision;

    let refusal = harness
        .owner
        .control_operator_command(
            "task-1",
            &operator_request(OperatorCommand::Interrupt, "interrupt-1"),
        )
        .await;
    match refusal {
        Err(ClientError::Refused { code, message }) => {
            assert_eq!(code, "operator_command_refused");
            assert!(message.contains("not started yet"), "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    let task = harness.task("task-1").await;
    assert_eq!(task.revision, revision, "nothing was written");
    assert!(task.operator_commands.is_empty());

    // An interrupted task has no runner to carry anything out, and the
    // answer says how to get one.
    let Running {
        controller, epoch, ..
    } = harness.running("task-2", "k8s").await;
    controller
        .report(&report(
            "task-2",
            epoch,
            1,
            Observation::WorkloadLost { state_known: true },
            Some(1),
        ))
        .await
        .expect("lost");
    match harness
        .owner
        .control_operator_command(
            "task-2",
            &operator_request(restart(None, None), "restart-1"),
        )
        .await
    {
        Err(ClientError::Refused { code, message }) => {
            assert_eq!(code, "operator_command_refused");
            assert!(message.contains("start or resume"), "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn a_command_for_a_runner_that_went_silent_is_refused() {
    let harness = Harness::start().await;
    harness.running("task-1", "k8s").await;
    harness.clock.advance(Duration::minutes(10));

    match harness
        .owner
        .control_operator_command(
            "task-1",
            &operator_request(OperatorCommand::Interrupt, "interrupt-1"),
        )
        .await
    {
        Err(ClientError::Refused { code, message }) => {
            assert_eq!(code, "operator_command_refused");
            assert!(message.contains("heard from"), "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(harness.task("task-1").await.operator_commands.is_empty());
}

#[tokio::test]
async fn a_command_expecting_another_generation_is_refused() {
    let harness = Harness::start().await;
    harness.running("task-1", "k8s").await;
    let generation = harness.task("task-1").await.generation;
    let mut request = operator_request(OperatorCommand::Interrupt, "interrupt-1");
    request.expected_generation = Some(generation + 1);

    assert_eq!(
        refused_code(
            harness
                .owner
                .control_operator_command("task-1", &request)
                .await
        ),
        "stale_generation"
    );

    request.expected_generation = Some(generation);
    harness
        .owner
        .control_operator_command("task-1", &request)
        .await
        .expect("the current generation is accepted");
}

#[tokio::test]
async fn an_override_needs_a_reason_and_a_state_that_allows_it() {
    let harness = Harness::start().await;
    write_override_workflow(&harness);
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    runner
        .snapshot(&snapshot_in(2, "locked", 5))
        .await
        .expect("in a state that allows no override");

    let revision = harness.task("task-1").await.revision;
    assert_eq!(
        bad_request_code(
            harness
                .owner
                .control_operator_command(
                    "task-1",
                    &operator_request(transition("done", true, Some("  ")), "t-1"),
                )
                .await
        ),
        "override_reason_required"
    );
    assert_eq!(
        bad_request_code(
            harness
                .owner
                .control_operator_command(
                    "task-1",
                    &operator_request(transition("done", true, None), "t-2"),
                )
                .await
        ),
        "override_reason_required"
    );
    assert_eq!(
        bad_request_code(
            harness
                .owner
                .control_operator_command(
                    "task-1",
                    &operator_request(transition("work", true, Some("go back")), "t-3"),
                )
                .await
        ),
        "override_not_allowed"
    );
    // Not an exit of the state either, and not a state at all.
    assert_eq!(
        bad_request_code(
            harness
                .owner
                .control_operator_command(
                    "task-1",
                    &operator_request(transition("work", false, None), "t-4"),
                )
                .await
        ),
        "illegal_transition"
    );
    assert_eq!(
        bad_request_code(
            harness
                .owner
                .control_operator_command(
                    "task-1",
                    &operator_request(transition(" ", false, None), "t-5")
                )
                .await
        ),
        "missing_target"
    );
    assert_eq!(
        harness.task("task-1").await.revision,
        revision,
        "none of them was written"
    );

    // A declared exit of the state is fine without a reason.
    harness
        .owner
        .control_operator_command(
            "task-1",
            &operator_request(transition("done", false, None), "t-6"),
        )
        .await
        .expect("a declared exit");

    // In the state that allows overrides, a reason is all it takes, and it
    // is part of the audit trail with the owner who gave it.
    runner
        .snapshot(&snapshot_in(3, "work", 6))
        .await
        .expect("in a state that allows an override");
    harness
        .owner
        .control_operator_command(
            "task-1",
            &operator_request(
                transition("done", true, Some("the ticket was closed")),
                "t-7",
            ),
        )
        .await
        .expect("an override with a reason");
    let audit = operator_events(&harness, "task-1").await;
    assert!(audit.iter().any(|kind| matches!(
        kind,
        TaskEventKind::OperatorCommandQueued {
            command: OperatorCommand::Transition { forced: true, reason: Some(reason), .. },
            requested_by,
            ..
        } if reason == "the ticket was closed" && requested_by.0 == "local"
    )));
}

#[tokio::test]
async fn commands_nobody_took_are_rejected_once_the_task_stops_running() {
    let harness = Harness::start().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    harness
        .owner
        .control_operator_command(
            "task-1",
            &operator_request(OperatorCommand::Interrupt, "interrupt-1"),
        )
        .await
        .expect("queued");
    harness
        .command("task-1", OwnerCommand::Pause, "pause-1")
        .await;

    let polled = runner.poll(None, 0).await.expect("poll");

    assert!(
        polled.commands.is_empty(),
        "never handed to a pausing runner"
    );
    let command = harness.task("task-1").await.operator_commands.remove(0);
    assert_eq!(command.status, OperatorCommandStatus::Rejected);
    assert!(
        command
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("stopped running")),
        "{command:?}"
    );
    assert!(operator_events(&harness, "task-1")
        .await
        .iter()
        .any(|kind| matches!(
            kind,
            TaskEventKind::OperatorCommandResolved {
                status: OperatorCommandStatus::Rejected,
                ..
            }
        )));
}

#[tokio::test]
async fn an_unauthenticated_request_queues_no_operator_command() {
    let harness = Harness::start().await;
    harness.running("task-1", "k8s").await;
    let raw = reqwest::Client::new()
        .post(format!(
            "{}/control/v1/tasks/task-1/operator-commands",
            harness.base_url()
        ))
        .json(&operator_request(OperatorCommand::Interrupt, "interrupt-1"))
        .send()
        .await
        .expect("request");
    assert_eq!(raw.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert!(harness.task("task-1").await.operator_commands.is_empty());
}

#[test]
fn operator_commands_keep_their_wire_shape() {
    let request: SubmitOperatorCommandRequest = serde_json::from_value(serde_json::json!({
        "kind": "transition",
        "target": "done",
        "override": true,
        "reason": "closed",
        "idempotency_key": "k",
        "expected_generation": 3,
    }))
    .expect("the documented body");
    assert_eq!(request.command, transition("done", true, Some("closed")));
    assert_eq!(request.expected_generation, Some(3));
    let minimal: SubmitOperatorCommandRequest =
        serde_json::from_value(serde_json::json!({ "kind": "interrupt", "idempotency_key": "k" }))
            .expect("an interrupt");
    assert_eq!(minimal.command, OperatorCommand::Interrupt);
    assert_eq!(minimal.expected_generation, None);
}

// ---------------------------------------------------------------------------
// U4: attention summary and on-demand execution reads
// ---------------------------------------------------------------------------

fn attention(kind: AttentionKind, waiting_id: &str, state: Option<&str>) -> AttentionItem {
    AttentionItem {
        kind,
        state_id: state.map(str::to_string),
        since: Utc.with_ymd_and_hms(2026, 10, 3, 12, 0, 1).unwrap(),
        waiting_id: waiting_id.to_string(),
    }
}

fn execution_event(sequence: u64) -> Event {
    Event::new(
        "exec-1".to_string(),
        EventData::StateEntered {
            state_id: format!("state-{sequence}"),
        },
        sequence,
    )
}

fn sequences(response: ExecutionEventsResponse) -> (Vec<u64>, bool, ExecutionEventsSource) {
    match response {
        ExecutionEventsResponse::Available {
            events,
            has_more,
            source,
        } => (
            events.iter().map(|e| e.sequence).collect(),
            has_more,
            source,
        ),
        other => panic!("expected events, got {other:?}"),
    }
}

#[tokio::test]
async fn the_summary_carries_what_the_runner_says_waits_and_drops_what_was_answered() {
    let harness = Harness::start().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    assert!(harness.task("task-1").await.attention.is_empty());

    let mut waiting = snapshot(2, ExecutionStatus::InputNeeded, 4, Some("ep-1"));
    waiting.attention = vec![attention(AttentionKind::Approval, "ep-1", Some("review"))];
    runner.snapshot(&waiting).await.expect("snapshot");
    let summary = harness.task("task-1").await;
    assert_eq!(
        summary.attention,
        vec![attention(AttentionKind::Approval, "ep-1", Some("review"))]
    );

    // The event announces the same list, so a client patches its row.
    let events = operator_events(&harness, "task-1").await;
    assert!(events.iter().any(|kind| matches!(
        kind,
        TaskEventKind::RunnerReported { attention, .. } if attention.len() == 1
    )));

    // Once the owner's answer is queued the row stops asking for it.
    harness
        .owner
        .control_input("task-1", &input_request("ep-1", "answer-1"))
        .await
        .expect("answer");
    assert!(harness.task("task-1").await.attention.is_empty());
}

#[tokio::test]
async fn several_waits_come_oldest_first_and_a_problem_is_one_of_them() {
    let harness = Harness::start().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    let mut older = attention(AttentionKind::Problem, "base_image_contract", None);
    older.since = Utc.with_ymd_and_hms(2026, 10, 3, 11, 0, 0).unwrap();
    let mut report = snapshot(2, ExecutionStatus::Running, 4, None);
    report.attention = vec![
        attention(AttentionKind::Question, "ep-2", Some("ask")),
        older.clone(),
    ];
    runner.snapshot(&report).await.expect("snapshot");
    let kinds: Vec<_> = harness
        .task("task-1")
        .await
        .attention
        .iter()
        .map(|item| item.kind)
        .collect();
    assert_eq!(kinds, vec![AttentionKind::Problem, AttentionKind::Question]);
}

#[tokio::test]
async fn attention_text_from_a_runner_is_cleaned_before_it_is_stored() {
    let harness = Harness::start().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    let mut report = snapshot(2, ExecutionStatus::Running, 4, None);
    report.attention = vec![attention(
        AttentionKind::Question,
        "ep\u{1b}[2J-1\u{202e}",
        Some("re\u{1b}]8;;x\u{7}view\u{200b}"),
    )];
    runner.snapshot(&report).await.expect("snapshot");

    let item = harness.task("task-1").await.attention.remove(0);
    assert_eq!(item.waiting_id, "ep[2J-1");
    assert_eq!(item.state_id.as_deref(), Some("re]8;;xview"));
    // Nor does the announcing event carry the raw text.
    let announced = operator_events(&harness, "task-1").await;
    let raw = serde_json::to_string(&announced).unwrap();
    assert!(
        !raw.contains("\\u001b") && !raw.contains("\\u202e"),
        "{raw}"
    );
}

#[tokio::test]
async fn a_task_the_backend_could_not_provision_asks_for_the_owner() {
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
        .expect("accepted");
    controller
        .report(&report(
            "task-1",
            epoch,
            1,
            Observation::WorkloadLost { state_known: true },
            None,
        ))
        .await
        .expect("lost");
    let summary = harness.task("task-1").await;
    assert_eq!(summary.runner_state, RunnerState::Interrupted);
    assert!(summary.runner.is_none(), "no execution ever existed");
    let kinds: Vec<_> = summary.attention.iter().map(|item| item.kind).collect();
    assert_eq!(kinds, vec![AttentionKind::ProvisioningFailed]);
}

#[tokio::test]
async fn the_timeline_names_the_status_state_and_attempt_under_way() {
    let harness = Harness::start().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    let mut report = snapshot(2, ExecutionStatus::Running, 4, None);
    report.timeline = Some(narsor_engine::engine::Timeline {
        positions: vec![narsor_engine::engine::Position {
            label: "review#1".to_string(),
            state_id: "review".to_string(),
            visit: 1,
            action_execution_id: Some("act-7".to_string()),
            action_type: Some("agent".to_string()),
            status: narsor_engine::engine::PositionStatus::InProgress,
            started_at: None,
            ended_at: None,
            error: None,
            resumed_harness_session: None,
            harness_session_interrupted: false,
            retry_pending: false,
            conversation: narsor_engine::engine::ConversationState::NoRecord,
        }],
        ..Default::default()
    });
    runner.snapshot(&report).await.expect("snapshot");
    let timeline = harness
        .owner
        .control_timeline("task-1")
        .await
        .expect("timeline");
    assert_eq!(
        timeline.execution_status,
        Some(narsor_protocol::api::ExecutionStatusDto::Running)
    );
    assert_eq!(timeline.current_state.as_deref(), Some("review"));
    assert_eq!(timeline.current_position.as_deref(), Some("review#1"));
}

/// The runner side of one relayed read: wait for the request, upload.
async fn answer_events(runner: &RunnerClient, events: Vec<Event>, has_more: bool) {
    loop {
        let poll = runner.poll(None, 5).await.expect("poll");
        if let Some(request) = poll.execution_events.first() {
            let page = ExecutionEventsPage { events, has_more };
            let accepted = runner
                .upload_execution_events(&request.request_id, &page)
                .await
                .expect("upload");
            assert!(accepted.delivered);
            return;
        }
    }
}

#[tokio::test]
async fn execution_events_of_a_live_runner_travel_through_the_relay_and_are_bounded() {
    let harness = Harness::start().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    let read = {
        let owner = harness.owner.clone();
        tokio::spawn(async move {
            owner
                .control_execution_events(
                    "task-1",
                    &ExecutionEventsQuery {
                        after: 2,
                        limit: Some(2),
                    },
                )
                .await
        })
    };
    // The runner over-delivers; the control plane cuts the page to the ask.
    answer_events(&runner, (3..8).map(execution_event).collect(), false).await;
    let (found, has_more, source) = sequences(read.await.unwrap().expect("events"));
    assert_eq!(found, vec![3, 4]);
    assert!(has_more, "events were cut off");
    assert_eq!(source, ExecutionEventsSource::Runner);
}

#[tokio::test]
async fn a_runner_that_does_not_answer_in_time_is_a_503_with_the_reason() {
    let config = ProtocolConfig {
        conversation_page_timeout: StdDuration::from_millis(200),
        ..ProtocolConfig::default()
    };
    let harness = Harness::start_with(config).await;
    harness.running("task-1", "k8s").await;
    match harness
        .owner
        .control_execution_events("task-1", &ExecutionEventsQuery::default())
        .await
    {
        Err(ClientError::Degraded { code, message }) => {
            assert_eq!(code, "runner_timeout");
            assert!(message.contains("did not answer"), "{message}");
        }
        other => panic!("expected a 503, got {other:?}"),
    }
}

#[tokio::test]
async fn a_runner_that_cannot_read_fails_the_request_at_once_and_it_stops_being_offered() {
    let harness = Harness::start().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    let started = std::time::Instant::now();
    let read = {
        let owner = harness.owner.clone();
        tokio::spawn(async move {
            owner
                .control_execution_events("task-1", &ExecutionEventsQuery::default())
                .await
        })
    };
    let request_id = loop {
        let poll = runner.poll(None, 5).await.expect("poll");
        if let Some(request) = poll.execution_events.first() {
            break request.request_id.clone();
        }
    };
    let failure = RelayFailure {
        reason: "the log is unreadable".into(),
    };
    assert!(
        runner
            .upload_relay_failure(&request_id, &failure)
            .await
            .expect("failure")
            .delivered
    );
    match read.await.unwrap() {
        Err(ClientError::Degraded { code, .. }) => assert_eq!(code, "runner_read_failed"),
        other => panic!("expected a 503, got {other:?}"),
    }
    assert!(started.elapsed() < StdDuration::from_secs(10));
    let poll = runner.poll(None, 0).await.expect("poll");
    assert!(
        poll.execution_events.is_empty(),
        "a failed request is not offered again"
    );
    // Failing a request twice, or one that is not there, is harmless.
    assert!(
        !runner
            .upload_relay_failure(&request_id, &failure)
            .await
            .expect("failure")
            .delivered
    );
}

#[tokio::test]
async fn a_task_without_a_runner_answers_unavailable_or_a_starting_reader_not_an_error() {
    let harness = Harness::start().await;
    // Before any runner: nothing to read.
    harness.create("task-0", "k8s").await;
    assert!(matches!(
        harness
            .owner
            .control_execution_events("task-0", &ExecutionEventsQuery::default())
            .await
            .expect("an answer"),
        ExecutionEventsResponse::Unavailable {
            reason: ConversationUnavailableReason::NotStarted,
            ..
        }
    ));

    let Running {
        controller, epoch, ..
    } = harness.running("task-1", "k8s").await;
    controller
        .report(&report(
            "task-1",
            epoch,
            1,
            Observation::WorkloadLost { state_known: true },
            Some(1),
        ))
        .await
        .expect("lost");
    // Interrupted: its workload is known to be gone, a reader may read it.
    match harness
        .owner
        .control_execution_events("task-1", &ExecutionEventsQuery::default())
        .await
        .expect("an answer")
    {
        ExecutionEventsResponse::Unavailable { reason, .. } => {
            assert_eq!(reason, ConversationUnavailableReason::ReaderStarting);
        }
        other => panic!("expected a starting reader, got {other:?}"),
    }
}

/// `Unknown` is not read: its previous runner may still be alive and hold
/// the storage. The answer is today's, and no reader is leased.
#[tokio::test]
async fn an_unknown_task_is_unavailable_and_leases_no_reader() {
    let harness = Harness::start().await;
    let Running {
        controller, epoch, ..
    } = harness.running("task-1", "k8s").await;
    controller
        .report(&report(
            "task-1",
            epoch,
            1,
            Observation::WorkloadLost { state_known: false },
            Some(1),
        ))
        .await
        .expect("lost");
    assert_eq!(
        harness.task("task-1").await.runner_state,
        RunnerState::Unknown
    );
    match harness
        .owner
        .control_execution_events("task-1", &ExecutionEventsQuery::default())
        .await
        .expect("an answer")
    {
        ExecutionEventsResponse::Unavailable { reason, message } => {
            assert_eq!(reason, ConversationUnavailableReason::Interrupted);
            assert!(message.contains("runner is not running"), "{message}");
        }
        other => panic!("expected unavailable, got {other:?}"),
    }
    match harness
        .owner
        .control_conductor("task-1", &ConductorReadQuery::default())
        .await
        .expect("an answer")
    {
        ConductorReadResponse::Unavailable { reason, .. } => {
            assert_eq!(reason, ConversationUnavailableReason::Interrupted)
        }
        other => panic!("expected unavailable, got {other:?}"),
    }
    assert!(control_task(&harness, "task-1").reader_lease.is_none());
    assert!(!desired(&controller, "task-1").await.reader_needed);
}

#[tokio::test]
async fn another_owners_task_has_no_execution_events_to_read() {
    let harness = Harness::start().await;
    harness
        .control_plane()
        .create_task(NewTask::new(
            TaskId::parse("foreign").unwrap(),
            OwnerRef::new("someone-else"),
            BackendId::parse("k8s").unwrap(),
            narsor_engine::domain::TaskLaunchSpec::new(
                "wf",
                1,
                serde_json::json!({}),
                narsor_engine::domain::TaskWorkspace::None,
                narsor_engine::domain::ResolvedEnvironment::harness_only("claude-code"),
                narsor_engine::domain::ResourceProfile::new(1000, 2048),
            ),
        ))
        .expect("foreign task");
    for task in ["foreign", "nobody"] {
        assert!(matches!(
            harness
                .owner
                .control_execution_events(task, &ExecutionEventsQuery::default())
                .await,
            Err(ClientError::NotFound)
        ));
    }
}

/// AE6 (D63): a stopped task is read through a read-only runner the control
/// plane has the controller start; it reads no storage itself.
#[tokio::test]
async fn ae6_a_stopped_tasks_events_are_read_through_a_reader() {
    let harness = Harness::start().await;
    let running = harness.running("task-1", "k8s").await;
    pause(&harness, &running).await;
    assert_eq!(
        harness.task("task-1").await.runner_state,
        RunnerState::Stopped
    );
    assert!(control_task(&harness, "task-1").reader_lease.is_none());

    // The first read leases a reader and says it is starting.
    match harness
        .owner
        .control_execution_events("task-1", &ExecutionEventsQuery::default())
        .await
        .expect("an answer")
    {
        ExecutionEventsResponse::Unavailable { reason, .. } => {
            assert_eq!(reason, ConversationUnavailableReason::ReaderStarting)
        }
        other => panic!("expected the reader to be starting, got {other:?}"),
    }
    assert!(control_task(&harness, "task-1").reader_lease_active(harness.clock.now()));
    assert!(
        desired(&running.controller, "task-1").await.reader_needed,
        "the controller is told a reader is wanted"
    );

    // The controller starts it; once it polls, the same read is answered.
    let reader = start_reader(&harness, &running, "reader-1").await;
    let read = {
        let owner = harness.owner.clone();
        tokio::spawn(async move {
            owner
                .control_execution_events(
                    "task-1",
                    &ExecutionEventsQuery {
                        after: 0,
                        limit: Some(3),
                    },
                )
                .await
        })
    };
    answer_events(&reader, (1..=3).map(execution_event).collect(), true).await;
    let (found, more, source) = sequences(read.await.unwrap().expect("events"));
    assert_eq!((found, more), (vec![1, 2, 3], true));
    assert_eq!(
        source,
        ExecutionEventsSource::Runner,
        "a reader is a runner; the control plane read nothing"
    );
}

#[tokio::test]
async fn timeline_text_from_a_runner_is_cleaned_before_it_is_stored() {
    let harness = Harness::start().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    let mut report = snapshot(2, ExecutionStatus::Running, 4, None);
    report.timeline = Some(narsor_engine::engine::Timeline {
        positions: vec![narsor_engine::engine::Position {
            label: "re\u{1b}[31mview#1\u{202e}".to_string(),
            state_id: "review".to_string(),
            visit: 1,
            action_execution_id: Some("act\u{7}-7".to_string()),
            action_type: Some("ag\u{1b}]8;;x\u{7}ent".to_string()),
            status: narsor_engine::engine::PositionStatus::Failed,
            started_at: None,
            ended_at: None,
            error: Some("bad\u{1b}[2J".to_string()),
            resumed_harness_session: None,
            harness_session_interrupted: false,
            retry_pending: false,
            conversation: narsor_engine::engine::ConversationState::NotCreated {
                reason: "why\u{200b}".to_string(),
            },
        }],
        default_position: Some("re\u{1b}[31mview#1\u{202e}".to_string()),
        ..Default::default()
    });
    runner.snapshot(&report).await.expect("snapshot");
    let timeline = harness
        .owner
        .control_timeline("task-1")
        .await
        .expect("timeline")
        .timeline
        .expect("a timeline");
    let position = &timeline.positions[0];
    assert_eq!(position.label, "re[31mview#1");
    assert_eq!(position.action_type.as_deref(), Some("ag]8;;xent"));
    assert_eq!(position.action_execution_id.as_deref(), Some("act-7"));
    assert_eq!(position.error.as_deref(), Some("bad[2J"));
    assert_eq!(timeline.default_position.as_deref(), Some("re[31mview#1"));
    let raw = serde_json::to_string(&timeline).unwrap();
    assert!(
        !raw.contains("\\u001b") && !raw.contains("\\u202e"),
        "{raw}"
    );
}

// ---------------------------------------------------------------------------
// The conductor's state and commands (U5)
// ---------------------------------------------------------------------------

fn conductor_entry(
    sequence: u64,
    author: narsor_engine::conductor::ChatAuthor,
    text: &str,
) -> narsor_engine::conductor::ChatEntry {
    let mut entry = narsor_engine::conductor::ChatEntry::new(
        author,
        narsor_engine::conductor::ChatEntryKind::Message,
        text,
    );
    entry.sequence = sequence;
    entry
}

/// The runner side of one relayed conductor read: wait for the request,
/// upload `view`.
async fn answer_conductor(runner: &RunnerClient, view: narsor_engine::conductor::ConductorView) {
    loop {
        let poll = runner.poll(None, 5).await.expect("poll");
        if let Some(request) = poll.conductor_reads.first() {
            let accepted = runner
                .upload_conductor_read(&request.request_id, &view)
                .await
                .expect("upload");
            assert!(accepted.delivered);
            return;
        }
    }
}

#[tokio::test]
async fn the_conductor_of_a_live_runner_travels_through_the_relay_and_is_cleaned() {
    let harness = Harness::start().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    let read = {
        let owner = harness.owner.clone();
        tokio::spawn(async move {
            owner
                .control_conductor(
                    "task-1",
                    &ConductorReadQuery {
                        after: 4,
                        limit: Some(10),
                    },
                )
                .await
        })
    };
    let mut view =
        narsor_engine::conductor::ConductorView::unavailable("unused").available_on("codex");
    view.chat = vec![conductor_entry(
        5,
        narsor_engine::conductor::ChatAuthor::Conductor,
        "line one\nline\u{1b}[31m two\u{202e}",
    )];
    view.unavailable_reason = None;
    answer_conductor(&runner, view).await;

    match read.await.unwrap().expect("the conductor") {
        ConductorReadResponse::Available { conductor, source } => {
            assert_eq!(source, ExecutionEventsSource::Runner);
            assert!(conductor.available);
            assert_eq!(conductor.harness.as_deref(), Some("codex"));
            assert_eq!(
                conductor.chat[0].text, "line one\nline[31m two",
                "line breaks stay, what a terminal acts on goes"
            );
        }
        other => panic!("expected the conductor, got {other:?}"),
    }
}

#[tokio::test]
async fn a_runner_that_does_not_answer_the_conductor_request_is_a_503() {
    let config = ProtocolConfig {
        conversation_page_timeout: StdDuration::from_millis(200),
        ..ProtocolConfig::default()
    };
    let harness = Harness::start_with(config).await;
    harness.running("task-1", "k8s").await;
    match harness
        .owner
        .control_conductor("task-1", &ConductorReadQuery::default())
        .await
    {
        Err(ClientError::Degraded { code, message }) => {
            assert_eq!(code, "runner_timeout");
            assert!(message.contains("conductor"), "{message}");
        }
        other => panic!("expected a 503, got {other:?}"),
    }
}

#[tokio::test]
async fn a_task_without_a_running_runner_has_no_conductor_to_read_until_a_reader_starts() {
    let harness = Harness::start().await;
    harness.create("task-0", "k8s").await;
    assert!(matches!(
        harness
            .owner
            .control_conductor("task-0", &ConductorReadQuery::default())
            .await
            .expect("an answer"),
        ConductorReadResponse::Unavailable {
            reason: ConversationUnavailableReason::NotStarted,
            ..
        }
    ));
    let Running {
        controller, epoch, ..
    } = harness.running("task-1", "k8s").await;
    controller
        .report(&report(
            "task-1",
            epoch,
            1,
            Observation::WorkloadLost { state_known: true },
            Some(1),
        ))
        .await
        .expect("lost");
    match harness
        .owner
        .control_conductor("task-1", &ConductorReadQuery::default())
        .await
        .expect("an answer")
    {
        ConductorReadResponse::Unavailable { reason, .. } => {
            assert_eq!(reason, ConversationUnavailableReason::ReaderStarting);
        }
        other => panic!("expected a starting reader, got {other:?}"),
    }
    assert!(matches!(
        harness
            .owner
            .control_conductor("nobody", &ConductorReadQuery::default())
            .await,
        Err(ClientError::NotFound)
    ));
}

#[tokio::test]
async fn the_conductor_of_a_stopped_task_is_read_through_a_reader() {
    let harness = Harness::start().await;
    let running = harness.running("task-1", "k8s").await;
    pause(&harness, &running).await;

    match harness
        .owner
        .control_conductor("task-1", &ConductorReadQuery::default())
        .await
        .expect("an answer")
    {
        ConductorReadResponse::Unavailable { reason, .. } => {
            assert_eq!(reason, ConversationUnavailableReason::ReaderStarting)
        }
        other => panic!("expected a starting reader, got {other:?}"),
    }
    let reader = start_reader(&harness, &running, "reader-1").await;
    let read = {
        let owner = harness.owner.clone();
        tokio::spawn(async move {
            owner
                .control_conductor(
                    "task-1",
                    &ConductorReadQuery {
                        after: 0,
                        limit: Some(2),
                    },
                )
                .await
        })
    };
    // What the reader reads from storage: the conductor, not running now.
    let mut view = narsor_engine::conductor::ConductorView::unavailable("the task is stopped");
    view.chat = vec![conductor_entry(
        1,
        narsor_engine::conductor::ChatAuthor::Operator,
        "second\u{1b}[2J",
    )];
    answer_conductor(&reader, view).await;
    match read.await.unwrap().expect("the conductor") {
        ConductorReadResponse::Available { conductor, source } => {
            assert_eq!(source, ExecutionEventsSource::Runner);
            assert!(!conductor.available, "nothing runs it now");
            assert_eq!(conductor.chat[0].text, "second[2J", "cleaned on the way in");
        }
        other => panic!("expected the conductor, got {other:?}"),
    }
}

#[tokio::test]
async fn conductor_commands_are_checked_for_what_they_name() {
    let harness = Harness::start().await;
    harness.running("task-1", "k8s").await;
    let refused = |command: OperatorCommand, key: &'static str| {
        let owner = harness.owner.clone();
        async move {
            bad_request_code(
                owner
                    .control_operator_command("task-1", &operator_request(command, key))
                    .await,
            )
        }
    };
    assert_eq!(
        refused(
            OperatorCommand::ConductorChat {
                text: "   ".to_string()
            },
            "c-1"
        )
        .await,
        "empty_message"
    );
    assert_eq!(
        refused(
            OperatorCommand::ConductorChat {
                text: "x".repeat(4_001)
            },
            "c-2"
        )
        .await,
        "message_too_long"
    );
    assert_eq!(
        refused(
            OperatorCommand::ConfirmProposal {
                proposal_id: " ".to_string()
            },
            "c-3"
        )
        .await,
        "missing_proposal"
    );
    assert_eq!(
        refused(
            OperatorCommand::RejectProposal {
                proposal_id: String::new()
            },
            "c-4"
        )
        .await,
        "missing_proposal"
    );
    assert_eq!(
        refused(
            OperatorCommand::UndoDecision {
                decision_id: String::new()
            },
            "c-5"
        )
        .await,
        "missing_decision"
    );
    // The conductor's harness is no longer chosen per task: refused, even
    // for a harness that exists.
    assert_eq!(
        refused(
            OperatorCommand::SetConductorHarness {
                harness: "codex".to_string()
            },
            "c-6"
        )
        .await,
        "unsupported_command"
    );
    assert!(
        !operator_events(&harness, "task-1")
            .await
            .iter()
            .any(|kind| matches!(kind, TaskEventKind::OperatorCommandQueued { .. })),
        "nothing refused was queued"
    );

    // A well-formed one is queued for the runner, like any other.
    let accepted = harness
        .owner
        .control_operator_command(
            "task-1",
            &operator_request(
                OperatorCommand::ConductorChat {
                    text: "hello".to_string(),
                },
                "c-ok",
            ),
        )
        .await
        .expect("queued");
    assert_eq!(accepted.command.status, OperatorCommandStatus::Queued);
}

#[tokio::test]
async fn what_a_runner_says_of_its_problem_and_its_conductor_is_cleaned() {
    let harness = Harness::start().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    let mut report = snapshot(2, ExecutionStatus::Running, 4, None);
    report.problem = Some(narsor_engine::orchestration::RunnerProblem {
        code: "base\u{1b}_image".to_string(),
        message: "no git\u{1b}[2J\u{202e} here".to_string(),
    });
    report.conductor_unavailable_reason = Some("harness\u{7} missing".to_string());
    runner.snapshot(&report).await.expect("snapshot");

    let summary = harness.task("task-1").await;
    let runner = summary.runner.expect("a runner view");
    let problem = runner.problem.expect("the problem");
    assert_eq!(problem.code, "base_image");
    assert_eq!(problem.message, "no git[2J here");
    assert!(!runner.conductor_available);
    assert_eq!(
        runner.conductor_unavailable_reason.as_deref(),
        Some("harness missing")
    );
}

// ---------------------------------------------------------------------------
// The default harness running tasks follow (U7)
// ---------------------------------------------------------------------------

fn available(harness: &str) -> Option<TaskDefaultHarness> {
    Some(TaskDefaultHarness::Available {
        harness: harness.to_string(),
    })
}

/// AE3: every answer to a runner carries the installation's default as it
/// applies to its task, and a change reaches the next answer.
#[tokio::test]
async fn every_runner_answer_carries_the_current_default_harness() {
    let harness = Harness::start_with_default_harness().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;

    let poll = runner.poll(None, 0).await.expect("poll");
    assert_eq!(poll.default_harness, available("claude-code"));

    harness
        .owner
        .set_harness_selection("opencode")
        .await
        .expect("change the default");
    let poll = runner.poll(None, 0).await.expect("poll");
    assert_eq!(poll.default_harness, available("opencode"));
    let answer = runner
        .snapshot(&snapshot(2, ExecutionStatus::Running, 4, None))
        .await
        .expect("snapshot");
    assert_eq!(answer.default_harness, available("opencode"));

    harness
        .owner
        .reset_harness_selection()
        .await
        .expect("reset the default");
    let poll = runner.poll(None, 0).await.expect("poll");
    assert_eq!(poll.default_harness, available("claude-code"));
}

/// AE6: a backend that does not run the default gets the reason instead of
/// the name; what its runner then reports reaches the owner as information,
/// not as a problem or something waiting on them.
#[tokio::test]
async fn a_backend_that_does_not_run_the_default_hears_why_and_the_owner_sees_it() {
    let harness = Harness::start_with_default_harness().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    environment_support::restrict_harnesses(harness.root.path(), "k8s", &["claude-code"]);
    harness
        .owner
        .set_harness_selection("opencode")
        .await
        .expect("change the default");

    let poll = runner.poll(None, 0).await.expect("poll");
    match poll.default_harness {
        Some(TaskDefaultHarness::Unavailable { harness, reason }) => {
            assert_eq!(harness, "opencode");
            assert!(reason.contains("runs only claude-code"), "{reason}");
        }
        other => panic!("expected the reason, got {other:?}"),
    }

    let mut report = snapshot(2, ExecutionStatus::Running, 4, None);
    report.default_harness_unavailable = Some(narsor_engine::orchestration::HarnessUnavailable {
        harness: "opencode".to_string(),
        kept: "claude-code".to_string(),
        reason: "backend 'k8s' runs only\u{1b}[2J claude-code".to_string(),
    });
    runner.snapshot(&report).await.expect("snapshot");
    let view = harness.task("task-1").await.runner.expect("a runner view");
    let unavailable = view
        .default_harness_unavailable
        .expect("the owner sees why the task keeps its harness");
    assert_eq!(unavailable.harness, "opencode");
    assert_eq!(unavailable.kept, "claude-code");
    assert_eq!(unavailable.reason, "backend 'k8s' runs only[2J claude-code");
    assert_eq!(view.problem, None, "it is not a problem");
    assert!(harness.task("task-1").await.attention.is_empty());

    // Back to a harness the backend runs: the name again.
    harness
        .owner
        .set_harness_selection("claude-code")
        .await
        .expect("change the default");
    let poll = runner.poll(None, 0).await.expect("poll");
    assert_eq!(poll.default_harness, available("claude-code"));
}

/// A change of the default answers a runner's open long-poll at once.
#[tokio::test]
async fn a_change_of_the_default_answers_an_open_long_poll_at_once() {
    let harness = Harness::start_with_default_harness().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;

    let started = Instant::now();
    let waiting = tokio::spawn(async move { runner.poll(None, 20).await });
    tokio::time::sleep(StdDuration::from_millis(300)).await;
    harness
        .owner
        .set_harness_selection("codex")
        .await
        .expect("change the default");
    let answer = tokio::time::timeout(StdDuration::from_secs(5), waiting)
        .await
        .expect("answered before the long-poll ran out")
        .expect("the poll task")
        .expect("poll");
    assert_eq!(answer.default_harness, available("codex"));
    assert!(started.elapsed() < StdDuration::from_secs(10));
}

/// A long-poll of a runner that says it runs on `on`.
fn poll_on(on: &str, wait_secs: u64) -> narsor_engine::controller_protocol::RunnerPollQuery {
    narsor_engine::controller_protocol::RunnerPollQuery {
        wait_secs: Some(wait_secs),
        harness: Some(on.to_string()),
        ..Default::default()
    }
}

/// A default that changed before the runner's poll began — between two
/// polls, or hidden by a stale answer the runner applied — answers the
/// poll at once when the runner says it is still on another harness.
#[tokio::test]
async fn a_runner_behind_the_default_is_answered_at_once() {
    let harness = Harness::start_with_default_harness().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    harness
        .owner
        .set_harness_selection("opencode")
        .await
        .expect("change the default");

    let answer = tokio::time::timeout(
        StdDuration::from_secs(5),
        runner.poll_with(&poll_on("claude-code", 20)),
    )
    .await
    .expect("answered before the long-poll ran out")
    .expect("poll");
    assert_eq!(answer.default_harness, available("opencode"));
}

/// A runner already on the default waits out its long-poll as before.
#[tokio::test]
async fn a_runner_on_the_default_keeps_waiting() {
    let harness = Harness::start_with_default_harness().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;

    let waited = tokio::time::timeout(
        StdDuration::from_secs(1),
        runner.poll_with(&poll_on("claude-code", 20)),
    )
    .await;
    assert!(waited.is_err(), "answered at once: {waited:?}");
}

/// A default the task's backend cannot run is not something the runner
/// can switch to: it does not answer the poll at once, whichever harness
/// the runner names.
#[tokio::test]
async fn an_unavailable_default_does_not_answer_the_poll_at_once() {
    let harness = Harness::start_with_default_harness().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    environment_support::restrict_harnesses(harness.root.path(), "k8s", &["claude-code"]);
    harness
        .owner
        .set_harness_selection("opencode")
        .await
        .expect("change the default");

    for on in ["claude-code", "opencode"] {
        let waited = tokio::time::timeout(
            StdDuration::from_secs(1),
            runner.poll_with(&poll_on(on, 20)),
        )
        .await;
        assert!(waited.is_err(), "answered at once on {on}: {waited:?}");
    }
}

/// A poll of an older runner, without the harness it runs on, still
/// reads; one without it does not send the field.
#[test]
fn a_poll_without_the_runners_harness_still_reads() {
    let query: narsor_engine::controller_protocol::RunnerPollQuery =
        serde_json::from_value(serde_json::json!({ "wait_secs": 5 })).expect("an old poll");
    assert_eq!(query.harness, None);
    assert_eq!(query.wait_secs, Some(5));
    let encoded = serde_json::to_value(&query).unwrap();
    assert!(encoded.get("harness").is_none(), "{encoded}");
}

/// Without a selector there is no default to follow: the answers carry
/// nothing, and a runner keeps its harness.
#[tokio::test]
async fn without_a_selector_the_answers_carry_no_default_harness() {
    let harness = Harness::start().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    let poll = runner.poll(None, 0).await.expect("poll");
    assert_eq!(poll.default_harness, None);
    let answer = runner
        .snapshot(&snapshot(2, ExecutionStatus::Running, 4, None))
        .await
        .expect("snapshot");
    assert_eq!(answer.default_harness, None);
}

/// Answers and views from before U7 still read.
#[test]
fn answers_and_views_without_the_default_harness_still_read() {
    let poll: narsor_engine::controller_protocol::RunnerPollResponse =
        serde_json::from_value(serde_json::json!({
            "directive": "continue",
            "generation": 1,
            "lifecycle": "running",
            "inputs": [],
            "credential_expires_at": "2026-10-03T12:15:00Z",
            "launch": { "incarnation": "pod-1", "launch_generation": 1, "authorized_by": null },
        }))
        .expect("an old poll answer");
    assert_eq!(poll.default_harness, None);
    let snapshot: RunnerSnapshot = serde_json::from_value(serde_json::json!({
        "snapshot_sequence": 1,
        "execution_status": "running",
        "event_cursor": 0,
        "observed_at": "2026-10-03T12:00:00Z",
    }))
    .expect("an old snapshot");
    assert_eq!(snapshot.default_harness_unavailable, None);
    let encoded = serde_json::to_value(&snapshot).unwrap();
    assert!(
        encoded.get("default_harness_unavailable").is_none(),
        "an old reader is not shown the new field: {encoded}"
    );
}

#[test]
fn the_protocol_config_sets_how_the_control_plane_counts_losses() {
    let root = TempRoot::new();
    let policy = ResumePolicy {
        attempt_delays: vec![Duration::zero(), Duration::seconds(1)],
        stability_window: Duration::seconds(5),
    };
    assert_eq!(ProtocolConfig::default().resume, ResumePolicy::default());

    let service = ProtocolService::with_config(
        ControlPlane::new(StatePaths::new(root.path())),
        environment_support::seeded_resolver(root.path()),
        ProtocolConfig {
            resume: policy.clone(),
            ..ProtocolConfig::default()
        },
    );

    assert_eq!(service.control_plane().resume_policy(), &policy);
}

// ---------------------------------------------------------------------------
// The resume policy's pass (R1, R4, R7)
// ---------------------------------------------------------------------------

/// A report that the workload of `task` at `generation` was lost, saying why.
fn lost(task: &str, epoch: u64, generation: u64, detail: &str) -> ReportRequest {
    let mut request = report(
        task,
        epoch,
        generation,
        Observation::WorkloadLost { state_known: true },
        None,
    );
    request.summary.detail = Some(detail.to_string());
    request
}

/// The task's control-plane events, oldest first.
fn task_events(harness: &Harness, task: &str) -> Vec<TaskEventKind> {
    harness
        .service
        .control_plane()
        .events_since(&TaskId::parse(task).unwrap(), 0)
        .expect("events")
        .into_iter()
        .map(|event| event.kind)
        .collect()
}

fn auto_resumed(events: &[TaskEventKind]) -> usize {
    events
        .iter()
        .filter(|kind| matches!(kind, TaskEventKind::AutoResumed { .. }))
        .count()
}

/// The daemon's tick goes through the same function this test calls, so a
/// due task is resumed without a daemon process.
#[tokio::test]
async fn the_daemons_periodic_pass_resumes_a_due_interrupted_task() {
    let harness = Harness::start().await;
    let Running {
        controller, epoch, ..
    } = harness.running("task-1", "k8s").await;
    controller
        .report(&lost("task-1", epoch, 1, "exited"))
        .await
        .expect("loss");

    let pass = narsor_server::runtime::run_periodic_pass(&harness.service);

    assert_eq!(pass.resumed, vec![TaskId::parse("task-1").unwrap()]);
    let record = harness
        .service
        .control_plane()
        .task(&TaskId::parse("task-1").unwrap())
        .unwrap();
    assert_eq!(record.runner_state, RunnerState::Provisioning);
    assert_eq!(auto_resumed(&task_events(&harness, "task-1")), 1);
}

#[tokio::test]
async fn the_resume_policy_resumes_a_lost_task_once_in_one_mutation() {
    let harness = Harness::start().await;
    let Running {
        controller, epoch, ..
    } = harness.running("task-1", "k8s").await;
    controller
        .report(&lost(
            "task-1",
            epoch,
            1,
            "the runner's workload disappeared",
        ))
        .await
        .expect("loss");
    let before = harness
        .service
        .control_plane()
        .task(&TaskId::parse("task-1").unwrap())
        .unwrap();
    assert_eq!(before.runner_state, RunnerState::Interrupted);

    let pass = harness.service.auto_resume_pass();
    assert_eq!(pass.resumed, vec![TaskId::parse("task-1").unwrap()]);
    // A second tick before the controller acted: the task is no longer
    // due, and repeating the policy's own command replays its receipt.
    assert!(harness.service.auto_resume_pass().resumed.is_empty());
    let replay = harness
        .service
        .control_plane()
        .auto_resume(&TaskId::parse("task-1").unwrap(), 1)
        .expect("a replay");
    assert!(replay.replayed);

    let record = harness
        .service
        .control_plane()
        .task(&TaskId::parse("task-1").unwrap())
        .unwrap();
    assert_eq!(record.runner_state, RunnerState::Provisioning);
    assert_eq!(record.generation, 2);
    assert_eq!(record.revision, before.revision + 1, "one mutation");
    let resumes: Vec<_> = record
        .commands
        .iter()
        .filter(|receipt| receipt.command == OwnerCommand::Resume)
        .collect();
    assert_eq!(resumes.len(), 1);
    assert_eq!(
        resumes[0].actor,
        narsor_engine::orchestration::CommandActor::Policy
    );
    assert_eq!(resumes[0].idempotency_key, "auto-resume:1");
    assert_eq!(record.resume.consecutive_losses, 1, "the count stays");
    assert_eq!(record.resume.next_attempt_at, None, "the attempt is used");
    assert_eq!(
        record.last_event.kind,
        TaskEventKind::AutoResumed {
            attempt: 1,
            limit: 5,
            loss_reason: Some("the runner's workload disappeared".to_string()),
        }
    );
    assert_eq!(auto_resumed(&task_events(&harness, "task-1")), 1);

    // The controller sees an ordinary resume, and the new runner learns
    // that the policy decided.
    let credential = controller
        .issue_runner_credential(
            "task-1",
            &RunnerCredentialRequest {
                lease_epoch: epoch,
                generation: 2,
                incarnation: "pod-2".to_string(),
            },
        )
        .await
        .expect("runner credential");
    let runner = RunnerClient::new(harness.base_url(), credential.credential);
    let answer = runner
        .snapshot(&snapshot(1, ExecutionStatus::Running, 3, None))
        .await
        .expect("snapshot");
    assert_eq!(answer.launch.authorized_by, Some(OwnerCommand::Resume));
    assert_eq!(
        answer.launch.authorized_by_actor,
        Some(narsor_engine::orchestration::CommandActor::Policy)
    );
}

#[tokio::test]
async fn the_resume_policy_waits_for_a_controller_to_launch_the_workload() {
    let harness = Harness::start().await;
    let Running {
        controller, epoch, ..
    } = harness.running("task-1", "k8s").await;
    controller
        .report(&lost("task-1", epoch, 1, "gone"))
        .await
        .expect("loss");
    // The controller's lease runs out: nobody could launch anything.
    harness.clock.advance(Duration::minutes(5));
    let pass = harness.service.auto_resume_pass();
    assert!(pass.resumed.is_empty());
    let record = harness
        .service
        .control_plane()
        .task(&TaskId::parse("task-1").unwrap())
        .unwrap();
    assert_eq!(record.runner_state, RunnerState::Interrupted);
    assert_eq!(record.resume.consecutive_losses, 1);
    assert!(record.resume.next_attempt_at.is_some(), "still planned");
    assert_eq!(auto_resumed(&task_events(&harness, "task-1")), 0);

    controller.acquire_lease(None).await.expect("back online");
    assert_eq!(harness.service.auto_resume_pass().resumed.len(), 1);
}

#[tokio::test]
async fn an_owner_command_first_makes_the_policy_step_aside() {
    let harness = Harness::start().await;
    let Running {
        controller, epoch, ..
    } = harness.running("task-1", "k8s").await;
    controller
        .report(&lost("task-1", epoch, 1, "gone"))
        .await
        .expect("loss");
    // The pass read the task at generation 1; the owner starts it first.
    harness
        .command("task-1", OwnerCommand::Start, "start-1")
        .await;
    let refused = harness
        .service
        .control_plane()
        .auto_resume(&TaskId::parse("task-1").unwrap(), 1);
    assert!(
        matches!(
            refused,
            Err(narsor_engine::orchestration::OrchestrationError::StaleGeneration { .. })
        ),
        "{refused:?}"
    );
    assert!(harness.service.auto_resume_pass().resumed.is_empty());
    let record = harness
        .service
        .control_plane()
        .task(&TaskId::parse("task-1").unwrap())
        .unwrap();
    assert_eq!(record.runner_state, RunnerState::Provisioning);
    assert_eq!(record.generation, 2, "one launch");
    assert_eq!(auto_resumed(&task_events(&harness, "task-1")), 0);
}

#[tokio::test]
async fn paused_tasks_and_losses_while_pausing_wait_for_the_owner() {
    let harness = Harness::start().await;
    // A paused task released its compute on purpose (R4).
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

    // A workload lost before its safe point: the owner wanted a pause.
    harness.create("task-2", "k8s").await;
    controller
        .report(&report(
            "task-2",
            epoch,
            1,
            Observation::StartAccepted,
            None,
        ))
        .await
        .expect("start accepted");
    controller
        .report(&report(
            "task-2",
            epoch,
            1,
            Observation::RunnerReady,
            Some(1),
        ))
        .await
        .expect("runner ready");
    harness
        .command("task-2", OwnerCommand::Pause, "pause-2")
        .await;
    controller
        .report(&lost("task-2", epoch, 2, "gone while pausing"))
        .await
        .expect("loss");

    for _ in 0..3 {
        assert!(harness.service.auto_resume_pass().resumed.is_empty());
        harness.clock.advance(Duration::seconds(30));
        controller.renew_lease(epoch, None).await.expect("renew");
    }
    assert_eq!(
        harness.task("task-1").await.runner_state,
        RunnerState::Stopped
    );
    assert_eq!(
        harness.task("task-2").await.runner_state,
        RunnerState::Interrupted
    );
    assert_eq!(auto_resumed(&task_events(&harness, "task-1")), 0);
    assert_eq!(auto_resumed(&task_events(&harness, "task-2")), 0);
}

#[tokio::test]
async fn an_exhausted_task_is_announced_exactly_once() {
    let harness = Harness::start_with(ProtocolConfig {
        resume: ResumePolicy {
            attempt_delays: vec![Duration::zero()],
            stability_window: Duration::minutes(10),
        },
        ..ProtocolConfig::default()
    })
    .await;
    let Running {
        controller, epoch, ..
    } = harness.running("task-1", "k8s").await;
    controller
        .report(&lost("task-1", epoch, 1, "gone"))
        .await
        .expect("first loss");
    assert_eq!(harness.service.auto_resume_pass().resumed.len(), 1);
    controller
        .report(&lost(
            "task-1",
            epoch,
            2,
            "the backend refused the workload",
        ))
        .await
        .expect("second loss");

    let pass = harness.service.auto_resume_pass();
    assert!(pass.resumed.is_empty());
    assert_eq!(pass.exhausted, vec![TaskId::parse("task-1").unwrap()]);
    assert!(harness.service.auto_resume_pass().exhausted.is_empty());

    let exhausted: Vec<_> = task_events(&harness, "task-1")
        .into_iter()
        .filter_map(|kind| match kind {
            TaskEventKind::AutoResumeExhausted { attempts, reason } => Some((attempts, reason)),
            _ => None,
        })
        .collect();
    assert_eq!(
        exhausted,
        vec![(
            1,
            "could not resume automatically: the backend refused the workload".to_string()
        )]
    );
    let record = harness
        .service
        .control_plane()
        .task(&TaskId::parse("task-1").unwrap())
        .unwrap();
    assert_eq!(record.runner_state, RunnerState::Interrupted);
    assert!(record.resume.exhausted.unwrap().announced);
}

#[tokio::test]
async fn an_owner_key_in_the_policys_namespace_is_refused() {
    let harness = Harness::start().await;
    harness.create("task-1", "k8s").await;
    let refused = harness
        .owner
        .control_command(
            "task-1",
            &OwnerCommandRequest {
                command: OwnerCommand::Archive,
                idempotency_key: "auto-resume:1".to_string(),
                expected_generation: None,
            },
        )
        .await;
    assert_eq!(bad_request_code(refused), "reserved_idempotency_key");
    assert_eq!(
        harness.task("task-1").await.runner_state,
        RunnerState::Queued
    );
}

// ---------------------------------------------------------------------------
// A stopped task's conversation is read through a reader (D63, U9)
// ---------------------------------------------------------------------------

/// Drives a running task to `Stopped` at a safe point.
async fn pause(harness: &Harness, running: &Running) {
    harness
        .command("task-1", OwnerCommand::Pause, "pause-1")
        .await;
    let mut at_safe_point = snapshot(2, ExecutionStatus::Running, 5, None);
    at_safe_point.safe_point = true;
    running
        .runner
        .snapshot(&at_safe_point)
        .await
        .expect("safe point");
    running
        .controller
        .report(&report(
            "task-1",
            running.epoch,
            2,
            Observation::PausedAtSafePoint,
            Some(2),
        ))
        .await
        .expect("paused");
}

async fn lose(running: &Running) {
    running
        .controller
        .report(&report(
            "task-1",
            running.epoch,
            1,
            Observation::WorkloadLost { state_known: true },
            Some(1),
        ))
        .await
        .expect("lost");
}

/// The desired state of `task` as a controller reads it.
async fn desired(
    controller: &ControllerClient,
    task: &str,
) -> narsor_engine::controller_protocol::DesiredCommand {
    controller
        .commands(0, true)
        .await
        .expect("commands")
        .commands
        .into_iter()
        .find(|command| command.task_id.as_str() == task)
        .expect("the task's desired state")
}

/// How a reader polls: for reads only.
fn reader_poll(wait_secs: u64) -> narsor_engine::controller_protocol::RunnerPollQuery {
    narsor_engine::controller_protocol::RunnerPollQuery {
        wait_secs: Some(wait_secs),
        defer_materials: true,
        ..Default::default()
    }
}

/// The controller's side of starting a read-only runner for `task-1`: it
/// asks for the reader's credential at the task's current generation, the
/// reader's first poll then tells the control plane it is connected.
async fn start_reader(harness: &Harness, running: &Running, incarnation: &str) -> RunnerClient {
    let generation = harness.task("task-1").await.generation;
    let issued = running
        .controller
        .issue_reader_credential(
            "task-1",
            &RunnerCredentialRequest {
                lease_epoch: running.epoch,
                generation,
                incarnation: incarnation.to_string(),
            },
        )
        .await
        .expect("a reader credential");
    let reader = RunnerClient::new(harness.base_url(), issued.credential);
    reader
        .poll_with(&reader_poll(0))
        .await
        .expect("the reader connects");
    reader
}

async fn page_from(
    harness: &Harness,
    attempt: &str,
    from: u64,
) -> Result<PageResponse, ClientError> {
    harness
        .owner
        .control_conversation_page(
            "task-1",
            attempt,
            &ConversationPageQuery {
                from,
                byte_budget: None,
            },
        )
        .await
}

/// A page of `entries` lines of one attempt, as a runner reads it.
fn recorded_page(entries: std::ops::RangeInclusive<u64>) -> PageResponse {
    PageResponse::Recorded {
        entries: entries
            .clone()
            .map(|number| narsor_engine::api::ConversationEntryDto {
                entry_number: number,
                timestamp: Utc::now(),
                source: narsor_engine::api::ConversationEntrySource::Narsor,
                raw_base64: String::new(),
                elements: Vec::new(),
            })
            .collect(),
        next_entry: entries.end() + 1,
        closed: false,
        truncated_tail: false,
        source: None,
        recorded_at: None,
    }
}

/// The runner side of one relayed page: wait for the request, upload `page`.
async fn answer_page(runner: &RunnerClient, page: &PageResponse) {
    loop {
        let poll = runner.poll(None, 5).await.expect("poll");
        if let Some(request) = poll.conversation_pages.first() {
            let accepted = runner
                .upload_conversation_page(&request.request_id, page)
                .await
                .expect("upload");
            assert!(accepted.delivered);
            return;
        }
    }
}

fn page_entries(page: PageResponse) -> (Vec<u64>, Option<ExecutionEventsSource>) {
    match page {
        PageResponse::Recorded {
            entries, source, ..
        } => (
            entries.iter().map(|entry| entry.entry_number).collect(),
            source,
        ),
        PageResponse::NoRecord => panic!("expected a record"),
    }
}

#[tokio::test]
async fn a_stopped_tasks_pages_wait_for_a_reader_and_then_come_from_it() {
    let harness = Harness::start().await;
    let running = harness.running("task-1", "k8s").await;
    pause(&harness, &running).await;

    // The summary is the last the runner reported, and says why it is not
    // live; it starts nothing.
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
            assert_eq!(last_attempts[0].attempt_id, "act-6", "the last report");
        }
        other => panic!("expected unavailable, got {other:?}"),
    }
    assert!(control_task(&harness, "task-1").reader_lease.is_none());

    // A page leases a reader and is refused with the retryable code.
    match page_from(&harness, "act-1", 0).await {
        Err(ClientError::Refused { code, .. }) => {
            assert_eq!(code, "conversation_reader_starting")
        }
        other => panic!("expected a 409, got {other:?}"),
    }
    assert!(control_task(&harness, "task-1").reader_lease.is_some());

    let reader = start_reader(&harness, &running, "reader-1").await;
    let read = {
        let owner = harness.owner.clone();
        tokio::spawn(async move {
            owner
                .control_conversation_page("task-1", "act-1", &ConversationPageQuery::default())
                .await
        })
    };
    answer_page(&reader, &recorded_page(1..=3)).await;
    let (entries, source) = page_entries(read.await.unwrap().expect("a page"));
    assert_eq!(entries, vec![1, 2, 3]);
    assert_eq!(source, Some(ExecutionEventsSource::Runner));
}

#[tokio::test]
async fn an_interrupted_task_is_read_through_a_reader_too() {
    let harness = Harness::start().await;
    let running = harness.running("task-1", "k8s").await;
    lose(&running).await;
    assert_eq!(
        harness.task("task-1").await.runner_state,
        RunnerState::Interrupted
    );
    match page_from(&harness, "act-1", 0).await {
        Err(ClientError::Refused { code, .. }) => {
            assert_eq!(code, "conversation_reader_starting")
        }
        other => panic!("expected a 409, got {other:?}"),
    }
    assert!(desired(&running.controller, "task-1").await.reader_needed);
    let reader = start_reader(&harness, &running, "reader-1").await;
    let read = {
        let owner = harness.owner.clone();
        tokio::spawn(async move {
            owner
                .control_conversation_page("task-1", "act-1", &ConversationPageQuery::default())
                .await
        })
    };
    answer_page(&reader, &recorded_page(1..=2)).await;
    assert_eq!(
        page_entries(read.await.unwrap().expect("a page")).0,
        vec![1, 2]
    );
}

#[tokio::test]
async fn an_archived_task_cannot_be_read_right_now() {
    let harness = Harness::start().await;
    harness.create("task-1", "k8s").await;
    harness
        .command("task-1", OwnerCommand::Archive, "archive-1")
        .await;

    match harness
        .owner
        .control_conversation("task-1")
        .await
        .expect("conversation")
    {
        ConversationResponse::Unavailable { reason, .. } => {
            assert_eq!(reason, ConversationUnavailableReason::Archived)
        }
        other => panic!("expected unavailable, got {other:?}"),
    }
    match page_from(&harness, "act-1", 0).await {
        Err(ClientError::Refused { code, message }) => {
            assert_eq!(code, "conversation_unavailable");
            assert!(message.contains("cannot be read right now"), "{message}");
        }
        other => panic!("expected a 409, got {other:?}"),
    }
    assert!(control_task(&harness, "task-1").reader_lease.is_none());
}

#[tokio::test]
async fn an_attempt_id_that_would_walk_the_path_is_refused() {
    let harness = Harness::start().await;
    let running = harness.running("task-1", "k8s").await;
    lose(&running).await;

    let owner = OwnerRef::new(LOCAL_OWNER);
    for attempt in ["..", "../act-1", "act/1", ".hidden"] {
        let refused = harness
            .service
            .conversation_page(&owner, "task-1", attempt, ConversationPageQuery::default())
            .await
            .expect_err(attempt);
        assert!(
            refused.to_string().starts_with("invalid_attempt"),
            "{attempt}: {refused}"
        );
    }
    // Over HTTP too, once the path is decoded.
    assert_eq!(
        bad_request_code(page_from(&harness, "..%2Fact-1", 0).await),
        "invalid_attempt"
    );
}

/// The lease is the control plane's want of a reader; the controller reads
/// it from the desired state, which follows the clock without a pass.
#[tokio::test]
async fn a_reader_lease_runs_out_when_nobody_reads_and_the_desired_state_says_so() {
    let harness = Harness::start().await;
    let running = harness.running("task-1", "k8s").await;
    pause(&harness, &running).await;
    assert!(!desired(&running.controller, "task-1").await.reader_needed);

    let _ = page_from(&harness, "act-1", 0).await;
    let record = control_task(&harness, "task-1");
    let lease = record.reader_lease.expect("leased");
    assert_eq!(
        lease.until - lease.since,
        Duration::seconds(120),
        "the backend's idle period"
    );
    assert!(desired(&running.controller, "task-1").await.reader_needed);

    // The controller keeps its own lease (a minute) alive meanwhile.
    harness.clock.advance(Duration::seconds(59));
    running
        .controller
        .renew_lease(running.epoch, None)
        .await
        .expect("renew");
    harness.clock.advance(Duration::seconds(60));
    running
        .controller
        .renew_lease(running.epoch, None)
        .await
        .expect("renew");
    assert!(desired(&running.controller, "task-1").await.reader_needed);
    harness.clock.advance(Duration::seconds(2));
    running
        .controller
        .renew_lease(running.epoch, None)
        .await
        .expect("renew");
    assert!(
        !desired(&running.controller, "task-1").await.reader_needed,
        "not wanted any more, before any pass removed the lease"
    );
    assert!(control_task(&harness, "task-1").reader_lease.is_some());

    // The daemon's pass removes it and leaves a trace.
    narsor_server::runtime::run_periodic_pass(&harness.service);
    assert!(control_task(&harness, "task-1").reader_lease.is_none());
    assert_eq!(
        task_events(&harness, "task-1")
            .into_iter()
            .filter(|kind| matches!(kind, TaskEventKind::ReaderLeaseEnded))
            .count(),
        1
    );
    narsor_server::runtime::run_periodic_pass(&harness.service);
    assert_eq!(
        task_events(&harness, "task-1")
            .into_iter()
            .filter(|kind| matches!(kind, TaskEventKind::ReaderLeaseEnded))
            .count(),
        1,
        "a second pass adds nothing"
    );
}

/// R7's counterpart for the reader: reads move the lease later, but a
/// client refreshing every few seconds does not write the record each time.
#[tokio::test]
async fn reads_move_the_lease_later_but_do_not_write_the_record_each_time() {
    let harness = Harness::start().await;
    let running = harness.running("task-1", "k8s").await;
    pause(&harness, &running).await;

    let _ = page_from(&harness, "act-1", 0).await;
    let first = control_task(&harness, "task-1");
    harness.clock.advance(Duration::seconds(10));
    let _ = page_from(&harness, "act-1", 0).await;
    let second = control_task(&harness, "task-1");
    assert_eq!(
        second.revision, first.revision,
        "10 s is under the threshold"
    );

    harness.clock.advance(Duration::seconds(60));
    let _ = page_from(&harness, "act-1", 0).await;
    let third = control_task(&harness, "task-1");
    assert!(third.revision > second.revision);
    let (before, after) = (first.reader_lease.unwrap(), third.reader_lease.unwrap());
    assert_eq!(after.since, before.since, "the same reading");
    assert!(after.until > before.until);
}

/// The reader's directive follows its lease: `Continue` while an owner
/// reads, `Stop` when it ran idle — where `directive(Stopped)` alone would
/// have sent a reader away the moment it started.
#[tokio::test]
async fn a_readers_directive_follows_its_lease() {
    let harness = Harness::start().await;
    let running = harness.running("task-1", "k8s").await;
    pause(&harness, &running).await;
    let _ = page_from(&harness, "act-1", 0).await;
    let reader = start_reader(&harness, &running, "reader-1").await;

    let poll = reader.poll_with(&reader_poll(0)).await.expect("poll");
    assert_eq!(poll.directive, RunnerDirective::Continue);
    assert_eq!(poll.runner_state, RunnerState::Stopped);
    assert_eq!(poll.launch.authorized_by, None, "a reader is no launch");

    harness.clock.advance(Duration::seconds(121));
    let poll = reader.poll_with(&reader_poll(0)).await.expect("poll");
    assert_eq!(poll.directive, RunnerDirective::Stop);
    // The working runner of the same task is told what it always was.
    let worker = running.runner.poll(None, 0).await.expect("poll");
    assert_eq!(worker.directive, RunnerDirective::Stop);
}

#[tokio::test]
async fn a_reader_may_not_write_anything() {
    let harness = Harness::start().await;
    let running = harness.running("task-1", "k8s").await;
    harness.task("task-1").await;
    pause(&harness, &running).await;
    let _ = page_from(&harness, "act-1", 0).await;
    let reader = start_reader(&harness, &running, "reader-1").await;
    let before = control_task(&harness, "task-1");

    assert!(matches!(
        reader
            .snapshot(&snapshot(9, ExecutionStatus::Running, 9, None))
            .await,
        Err(ClientError::Forbidden)
    ));
    assert!(matches!(
        reader.ack_input("input-1", true, None).await,
        Err(ClientError::Forbidden)
    ));
    assert!(matches!(
        reader.ack_command("command-1", true, None).await,
        Err(ClientError::Forbidden)
    ));
    assert!(matches!(
        reader.material("material-1").await,
        Err(ClientError::Forbidden)
    ));
    assert!(matches!(
        reader.material_blob("material-1", "sha256:x").await,
        Err(ClientError::Forbidden)
    ));
    assert!(matches!(
        reader.ack_material("material-1", "sha256:x").await,
        Err(ClientError::Forbidden)
    ));
    assert!(matches!(
        reader.publish_material(vec![0; 4]).await,
        Err(ClientError::Forbidden)
    ));
    let after = control_task(&harness, "task-1");
    assert_eq!(after.revision, before.revision, "nothing was written");
    assert_eq!(after.runner, before.runner);

    // Reading and renewing are what a reader does.
    reader.renew().await.expect("renew");
    reader.poll_with(&reader_poll(0)).await.expect("poll");
}

/// A reader is handed no answer and no command, whatever its query says:
/// the credential's role decides, and no request naming another pending
/// input makes the answer obsolete.
#[tokio::test]
async fn a_readers_poll_is_read_only_whatever_it_asks() {
    let harness = Harness::start().await;
    let running = harness.running("task-1", "k8s").await;
    running
        .runner
        .snapshot(&snapshot(2, ExecutionStatus::InputNeeded, 4, Some("ep-1")))
        .await
        .expect("waiting");
    harness
        .owner
        .control_input("task-1", &input_request("ep-1", "answer-1"))
        .await
        .expect("queued");
    // The answer waits for the working runner, which then stops: the task
    // is stopped with an unsettled answer.
    harness
        .command("task-1", OwnerCommand::Pause, "pause-1")
        .await;
    let mut at_safe_point = snapshot(3, ExecutionStatus::InputNeeded, 5, Some("ep-1"));
    at_safe_point.safe_point = true;
    running
        .runner
        .snapshot(&at_safe_point)
        .await
        .expect("safe point");
    running
        .controller
        .report(&report(
            "task-1",
            running.epoch,
            2,
            Observation::PausedAtSafePoint,
            Some(2),
        ))
        .await
        .expect("paused");
    let _ = page_from(&harness, "act-1", 0).await;
    let reader = start_reader(&harness, &running, "reader-1").await;

    let asks_for_everything = narsor_engine::controller_protocol::RunnerPollQuery {
        pending_input_id: Some("ep-1".into()),
        wait_secs: Some(0),
        ..Default::default()
    };
    let poll = reader.poll_with(&asks_for_everything).await.expect("poll");
    assert!(poll.inputs.is_empty());
    assert!(poll.commands.is_empty());
    assert!(poll.materials.is_empty());
    let record = control_task(&harness, "task-1");
    assert!(
        record
            .inputs
            .iter()
            .all(|input| input.status == InputStatus::Queued),
        "the answer is still waiting for its runner"
    );
}

/// R15: the command that resumes the task ends the reader's lease in its
/// own mutation, and its credential stops working with it.
#[tokio::test]
async fn a_resume_ends_the_reader_lease_in_the_same_mutation() {
    let harness = Harness::start().await;
    let running = harness.running("task-1", "k8s").await;
    pause(&harness, &running).await;
    let _ = page_from(&harness, "act-1", 0).await;
    let reader = start_reader(&harness, &running, "reader-1").await;
    let before = control_task(&harness, "task-1");
    assert!(before.reader_lease_active(harness.clock.now()));

    let resumed = harness
        .command("task-1", OwnerCommand::Resume, "resume-1")
        .await;
    assert_eq!(resumed.runner_state, RunnerState::Provisioning);
    let after = control_task(&harness, "task-1");
    assert!(after.reader_lease.is_none());
    assert_eq!(
        after.revision,
        before.revision + 1,
        "one mutation, one revision"
    );
    assert!(
        !task_events(&harness, "task-1")
            .iter()
            .any(|kind| matches!(kind, TaskEventKind::ReaderLeaseEnded)),
        "it ended with the resume, not in a mutation of its own"
    );

    // The controller is no longer asked for a reader, and the one running
    // is turned away.
    assert!(!desired(&running.controller, "task-1").await.reader_needed);
    assert!(matches!(
        reader.poll_with(&reader_poll(0)).await,
        Err(ClientError::Unauthorized)
    ));
    // A reader credential is not issued for a task nobody reads.
    match running
        .controller
        .issue_reader_credential(
            "task-1",
            &RunnerCredentialRequest {
                lease_epoch: running.epoch,
                generation: after.generation,
                incarnation: "reader-2".into(),
            },
        )
        .await
    {
        Err(ClientError::Refused { code, .. }) => assert_eq!(code, "reader_not_wanted"),
        other => panic!("expected a 409, got {other:?}"),
    }
}

#[tokio::test]
async fn an_archive_ends_the_reader_lease_too() {
    let harness = Harness::start().await;
    let running = harness.running("task-1", "k8s").await;
    pause(&harness, &running).await;
    let _ = page_from(&harness, "act-1", 0).await;
    assert!(control_task(&harness, "task-1").reader_lease.is_some());
    harness
        .command("task-1", OwnerCommand::Archive, "archive-1")
        .await;
    assert!(control_task(&harness, "task-1").reader_lease.is_none());
}

/// A reader credential is a session of its own: issuing either kind leaves
/// the other as it was, and neither touches the task's record.
#[tokio::test]
async fn a_readers_credential_and_a_runners_do_not_invalidate_each_other() {
    use narsor_engine::controller_protocol::RunnerRole;
    let harness = Harness::start().await;
    let running = harness.running("task-1", "k8s").await;
    pause(&harness, &running).await;
    let _ = page_from(&harness, "act-1", 0).await;
    let task = TaskId::parse("task-1").unwrap();
    let ttl = Duration::minutes(15);
    let credentials = harness.service.credentials();
    let revision = control_task(&harness, "task-1").revision;

    let (_, worker_1) = credentials
        .issue_runner_session(RunnerRole::Worker, &task, "worker-1", 2, 1, ttl)
        .expect("worker");
    let (_, reader_1) = credentials
        .issue_runner_session(RunnerRole::Reader, &task, "reader-1", 2, 1, ttl)
        .expect("reader");
    let role_of = |token: &narsor_engine::controller_protocol::credentials::BearerToken| {
        harness
            .service
            .authenticate_runner(token.expose())
            .map(|identity| (identity.role, identity.incarnation))
    };
    assert_eq!(
        role_of(&worker_1),
        Some((RunnerRole::Worker, "worker-1".into()))
    );
    assert_eq!(
        role_of(&reader_1),
        Some((RunnerRole::Reader, "reader-1".into()))
    );

    // A newer reader replaces the older reader only.
    let (_, reader_2) = credentials
        .issue_runner_session(RunnerRole::Reader, &task, "reader-2", 2, 1, ttl)
        .expect("reader");
    assert_eq!(role_of(&reader_1), None);
    assert_eq!(
        role_of(&reader_2),
        Some((RunnerRole::Reader, "reader-2".into()))
    );
    assert_eq!(
        role_of(&worker_1),
        Some((RunnerRole::Worker, "worker-1".into()))
    );

    // A newer worker replaces the older worker only.
    let (_, worker_2) = credentials
        .issue_runner_session(RunnerRole::Worker, &task, "worker-2", 2, 1, ttl)
        .expect("worker");
    assert_eq!(role_of(&worker_1), None);
    assert_eq!(
        role_of(&worker_2),
        Some((RunnerRole::Worker, "worker-2".into()))
    );
    assert_eq!(
        role_of(&reader_2),
        Some((RunnerRole::Reader, "reader-2".into()))
    );

    assert_eq!(
        control_task(&harness, "task-1").revision,
        revision,
        "the record is not touched"
    );
}

/// AE5, the control plane's part: a running reader is no slot and no
/// runner of the task's record.
#[tokio::test]
async fn ae5_a_reader_takes_no_slot_and_changes_nothing_the_owner_sees_of_the_runner() {
    let harness = Harness::start().await;
    let running = harness.running("task-1", "k8s").await;
    pause(&harness, &running).await;
    let stopped = harness.task("task-1").await;
    let _ = page_from(&harness, "act-1", 0).await;
    let _reader = start_reader(&harness, &running, "reader-1").await;

    let reading = harness.task("task-1").await;
    assert!(!reading.holds_slot, "a reader holds no slot");
    assert_eq!(reading.runner_state, RunnerState::Stopped);
    assert_eq!(
        reading
            .runner
            .as_ref()
            .map(|runner| runner.incarnation.clone()),
        stopped
            .runner
            .as_ref()
            .map(|runner| runner.incarnation.clone()),
        "the task's runner is still the working one"
    );
    assert_eq!(reading.generation, stopped.generation);
}

/// The controller's long-poll wakes when a reader becomes wanted, though no
/// generation moved: otherwise a reader would start only on the controller's
/// next full pass.
#[tokio::test]
async fn a_reader_becoming_wanted_wakes_the_controllers_long_poll() {
    let harness = Harness::start().await;
    let running = harness.running("task-1", "k8s").await;
    pause(&harness, &running).await;
    // Everything so far acknowledged: nothing new by generation.
    let all = running
        .controller
        .commands(0, true)
        .await
        .expect("commands");
    running
        .controller
        .acknowledge(
            all.commands
                .iter()
                .map(|command| CommandAck {
                    task_id: command.task_id.to_string(),
                    generation: command.generation,
                })
                .collect(),
        )
        .await
        .expect("ack");
    assert!(running
        .controller
        .commands(0, false)
        .await
        .expect("commands")
        .commands
        .is_empty());

    let waiting = {
        let controller = running.controller.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let response = controller.commands(20, false).await.expect("long-poll");
            (started.elapsed(), response)
        })
    };
    tokio::time::sleep(StdDuration::from_millis(200)).await;
    let _ = page_from(&harness, "act-1", 0).await;
    let (waited, response) = waiting.await.unwrap();
    assert!(
        waited < StdDuration::from_secs(10),
        "woken, not timed out: {waited:?}"
    );
    assert!(
        response.commands.is_empty(),
        "it only wakes the controller; the desired state is read as usual"
    );
}

// ---------------------------------------------------------------------------
// Wire compatibility across versions (D58): an older server's answers decode
// with defaults, a newer server's unknown kinds decode as `Unrecognized`
// ---------------------------------------------------------------------------

#[test]
fn a_task_summary_from_a_server_before_the_resume_policy_decodes_empty() {
    let summary: TaskSummary = serde_json::from_value(serde_json::json!({
        "task_id": "task-1",
        "backend_id": "k8s",
        "lifecycle": "interrupted",
        "generation": 2,
        "revision": 4,
        "rank": 1,
        "created_at": "2026-10-10T12:00:00Z",
        "queue_sequence": 1,
        "workflow_id": "implement-feature",
        "workflow_version": 3,
        "launch_digest": "digest",
        "environment_identity": "env",
        "archive": null,
        "observation": null,
        "runner": null,
        "controller": null,
        "inputs": [],
        "as_of": "2026-10-10T12:00:00Z"
    }))
    .expect("an older summary decodes");

    assert!(summary.resume.is_empty());
}

/// A server from before the rename spells the runner state `lifecycle` and
/// the stopped one `paused`; a newer client must still read it.
#[test]
fn a_task_summary_from_a_server_before_the_rename_reads_its_lifecycle() {
    let summary: TaskSummary = serde_json::from_value(serde_json::json!({
        "task_id": "task-1",
        "backend_id": "k8s",
        "lifecycle": "paused",
        "generation": 2,
        "revision": 4,
        "rank": 1,
        "created_at": "2026-10-10T12:00:00Z",
        "queue_sequence": 1,
        "workflow_id": "implement-feature",
        "workflow_version": 3,
        "launch_digest": "digest",
        "environment_identity": "env",
        "archive": null,
        "observation": null,
        "runner": null,
        "controller": null,
        "inputs": [],
        "as_of": "2026-10-10T12:00:00Z"
    }))
    .expect("an older summary decodes");

    assert_eq!(summary.runner_state, RunnerState::Stopped);
    let written = serde_json::to_value(&summary).unwrap();
    assert_eq!(written["runner_state"], "stopped");
    assert!(written.get("lifecycle").is_none());
}

#[test]
fn a_runner_launch_from_a_control_plane_before_the_policy_has_no_actor() {
    let launch: narsor_engine::controller_protocol::RunnerLaunchView =
        serde_json::from_value(serde_json::json!({
            "incarnation": "inc-1",
            "launch_generation": 2,
            "authorized_by": "resume"
        }))
        .expect("an older launch decodes");

    assert_eq!(launch.authorized_by, Some(OwnerCommand::Resume));
    assert_eq!(launch.authorized_by_actor, None, "read as the owner");
}

#[test]
fn a_conversation_from_a_server_before_storage_reads_comes_from_the_runner() {
    let conversation: ConversationResponse = serde_json::from_value(serde_json::json!({
        "status": "available",
        "attempts": [],
        "observed_at": "2026-10-10T12:00:00Z"
    }))
    .expect("an older conversation decodes");

    let ConversationResponse::Available {
        source,
        recorded_at,
        ..
    } = conversation
    else {
        panic!("available: {conversation:?}");
    };
    assert_eq!(source, ExecutionEventsSource::Runner);
    assert_eq!(recorded_at, None);
}

#[test]
fn a_desired_command_from_a_server_before_the_reader_wants_no_reader() {
    use narsor_engine::controller_protocol::DesiredCommand;
    let command = DesiredCommand {
        task_id: TaskId::parse("task-1").unwrap(),
        generation: 2,
        runner_state: RunnerState::Stopped,
        action: DesiredAction::KeepPaused,
        resource_key: "task-1".into(),
        launch_spec: serde_json::json!({}),
        runner: None,
        reader_needed: false,
    };
    let json = serde_json::to_value(&command).unwrap();
    assert!(
        json.get("reader_needed").is_none(),
        "an older controller is sent what it knows"
    );
    assert!(
        !serde_json::from_value::<DesiredCommand>(json)
            .unwrap()
            .reader_needed
    );
    let wanted = DesiredCommand {
        reader_needed: true,
        ..command
    };
    let json = serde_json::to_value(&wanted).unwrap();
    assert_eq!(json["reader_needed"], true);
    assert!(
        serde_json::from_value::<DesiredCommand>(json)
            .unwrap()
            .reader_needed
    );
}

#[test]
fn the_reader_starting_reason_round_trips_and_an_unknown_one_stays_unrecognized() {
    let reason: ConversationUnavailableReason =
        serde_json::from_value(serde_json::json!("reader_starting")).unwrap();
    assert_eq!(reason, ConversationUnavailableReason::ReaderStarting);
    let reason: ConversationUnavailableReason =
        serde_json::from_value(serde_json::json!("reader_on_fire")).unwrap();
    assert_eq!(reason, ConversationUnavailableReason::Unrecognized);
}

#[test]
fn an_unavailable_reason_of_a_newer_server_decodes_as_unrecognized() {
    let conversation: ConversationResponse = serde_json::from_value(serde_json::json!({
        "status": "unavailable",
        "reason": "runner_on_vacation",
        "last_attempts": [],
        "observed_at": null
    }))
    .expect("a newer reason decodes");

    let ConversationResponse::Unavailable { reason, .. } = conversation else {
        panic!("unavailable: {conversation:?}");
    };
    assert_eq!(reason, ConversationUnavailableReason::Unrecognized);
    assert_eq!(
        narsor::cli::task_text::unavailable_reason(reason),
        "the server gave a reason this version does not know"
    );
}

#[test]
fn a_newer_servers_event_and_observation_kinds_decode_as_unrecognized() {
    let envelope: narsor_engine::controller_protocol::ControlEventEnvelope =
        serde_json::from_value(serde_json::json!({
            "cursor": "task-1:9",
            "event": {
                "sequence": 9,
                "task_id": "task-1",
                "at": "2026-10-10T12:00:00Z",
                "runner_state": "interrupted",
                "generation": 2,
                "kind": { "type": "resumed_by_magic" }
            }
        }))
        .expect("a newer event decodes");
    assert_eq!(envelope.event.kind, TaskEventKind::Unrecognized);

    let observation: Observation =
        serde_json::from_value(serde_json::json!({ "kind": "workload_teleported" }))
            .expect("a newer observation decodes");
    assert_eq!(observation, Observation::Unrecognized);
}

// ---------------------------------------------------------------------------
// The slot-release policy (U4, D62)
// ---------------------------------------------------------------------------

fn control_task(harness: &Harness, task: &str) -> narsor_engine::orchestration::TaskRecord {
    harness
        .service
        .control_plane()
        .task(&TaskId::parse(task).unwrap())
        .expect("task record")
}

fn slot_releases(harness: &Harness, task: &str) -> Vec<TaskEventKind> {
    task_events(harness, task)
        .into_iter()
        .filter(|kind| matches!(kind, TaskEventKind::SlotReleased { .. }))
        .collect()
}

#[tokio::test]
async fn the_snapshot_of_a_finished_task_stops_its_runner_in_the_same_request() {
    let harness = Harness::start().await;
    let Running {
        controller,
        epoch,
        runner,
    } = harness.running("task-1", "k8s").await;

    let answer = runner
        .snapshot(&snapshot(2, ExecutionStatus::Completed, 4, None))
        .await
        .expect("completed snapshot");

    assert_eq!(answer.directive, RunnerDirective::PauseAtSafePoint);
    assert_eq!(answer.runner_state, RunnerState::PauseRequested);
    let record = control_task(&harness, "task-1");
    assert!(record.holds_capacity(), "the slot goes at the safe point");
    assert_eq!(
        slot_releases(&harness, "task-1"),
        vec![TaskEventKind::SlotReleased {
            reason: narsor_engine::orchestration::SlotReleaseReason::Terminal
        }]
    );
    // The runner stops at a safe point and the controller reports it.
    let mut at_safe_point = snapshot(3, ExecutionStatus::Completed, 4, None);
    at_safe_point.safe_point = true;
    runner.snapshot(&at_safe_point).await.expect("safe point");
    controller
        .report(&report(
            "task-1",
            epoch,
            record.generation,
            Observation::PausedAtSafePoint,
            Some(2),
        ))
        .await
        .expect("paused");
    let stopped = harness.task("task-1").await;
    assert_eq!(stopped.runner_state, RunnerState::Stopped);
    assert!(!stopped.holds_slot);
    assert_eq!(
        stopped.state,
        narsor_engine::orchestration::TaskState::Finished
    );
}

#[tokio::test]
async fn the_periodic_pass_frees_a_waiting_tasks_slot_once_the_wait_ran_out() {
    let harness = Harness::start().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    runner
        .snapshot(&snapshot(2, ExecutionStatus::InputNeeded, 4, Some("ask-1")))
        .await
        .expect("input needed");
    assert_eq!(
        control_task(&harness, "task-1").runner_state,
        RunnerState::Running
    );

    harness.clock.advance(Duration::seconds(299));
    narsor_server::runtime::run_periodic_pass(&harness.service);
    assert_eq!(
        control_task(&harness, "task-1").runner_state,
        RunnerState::Running
    );
    assert!(slot_releases(&harness, "task-1").is_empty());

    harness.clock.advance(Duration::seconds(2));
    narsor_server::runtime::run_periodic_pass(&harness.service);
    narsor_server::runtime::run_periodic_pass(&harness.service);

    assert_eq!(
        control_task(&harness, "task-1").runner_state,
        RunnerState::PauseRequested
    );
    assert_eq!(
        slot_releases(&harness, "task-1"),
        vec![TaskEventKind::SlotReleased {
            reason: narsor_engine::orchestration::SlotReleaseReason::GraceElapsed
        }],
        "a second pass adds nothing"
    );
}

#[tokio::test]
async fn a_failed_task_that_runs_again_is_not_stopped_by_the_old_wait() {
    let harness = Harness::start().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    runner
        .snapshot(&snapshot(2, ExecutionStatus::Failed, 4, None))
        .await
        .expect("failed");
    harness.clock.advance(Duration::minutes(2));
    runner
        .snapshot(&snapshot(3, ExecutionStatus::Running, 5, None))
        .await
        .expect("restarted step");

    harness.clock.advance(Duration::minutes(4));
    narsor_server::runtime::run_periodic_pass(&harness.service);

    assert_eq!(
        control_task(&harness, "task-1").runner_state,
        RunnerState::Running
    );
}

#[tokio::test]
async fn reading_the_conversation_holds_the_slot_longer_and_writes_the_record_rarely() {
    let harness = Harness::start().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    runner
        .snapshot(&snapshot(2, ExecutionStatus::InputNeeded, 4, Some("ask-1")))
        .await
        .expect("input needed");
    harness.clock.advance(Duration::minutes(4));

    for _ in 0..5 {
        // The runner's relay is not served in this test: the read answers
        // or fails, and either way it is a read of the details.
        let _ = harness.owner.control_conversation("task-1").await;
    }
    let after_reads = control_task(&harness, "task-1");
    let moved_to = after_reads.slot_release_due_at.expect("a deadline");
    assert_eq!(moved_to, harness.clock.now() + Duration::seconds(180));
    let scheduled = task_events(&harness, "task-1")
        .into_iter()
        .filter(|kind| matches!(kind, TaskEventKind::SlotReleaseScheduled { .. }))
        .count();
    assert_eq!(scheduled, 1, "five reads wrote the record once");

    harness.clock.advance(Duration::seconds(90));
    narsor_server::runtime::run_periodic_pass(&harness.service);
    assert_eq!(
        control_task(&harness, "task-1").runner_state,
        RunnerState::Running
    );
}

#[tokio::test]
async fn an_answer_in_the_queue_keeps_the_slot_when_the_wait_runs_out() {
    let harness = Harness::start().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    runner
        .snapshot(&snapshot(2, ExecutionStatus::InputNeeded, 4, Some("ask-1")))
        .await
        .expect("input needed");
    harness.clock.advance(Duration::seconds(299));
    harness
        .owner
        .control_input("task-1", &input_request("ask-1", "answer-1"))
        .await
        .expect("the owner answers");

    harness.clock.advance(Duration::seconds(2));
    narsor_server::runtime::run_periodic_pass(&harness.service);

    assert_eq!(
        control_task(&harness, "task-1").runner_state,
        RunnerState::Running
    );
    assert!(slot_releases(&harness, "task-1").is_empty());
}

#[tokio::test]
async fn a_daemon_that_starts_after_the_deadline_frees_the_slot_on_its_first_pass() {
    let harness = Harness::start().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    runner
        .snapshot(&snapshot(2, ExecutionStatus::InputNeeded, 4, Some("ask-1")))
        .await
        .expect("input needed");
    harness.clock.advance(Duration::minutes(30));
    // A new daemon: another service on the same state root, with no memory.
    let restarted = ProtocolService::with_config(
        harness.control_plane(),
        environment_support::seeded_resolver(harness.root.path()),
        ProtocolConfig::default(),
    );

    let pass = restarted.slot_release_pass();

    assert_eq!(pass.released, vec![TaskId::parse("task-1").unwrap()]);
    assert_eq!(
        control_task(&harness, "task-1").runner_state,
        RunnerState::PauseRequested
    );
}

#[tokio::test]
async fn an_owner_cannot_use_the_slot_release_keys() {
    let harness = Harness::start().await;
    harness.running("task-1", "k8s").await;

    let refused = harness
        .owner
        .control_command(
            "task-1",
            &OwnerCommandRequest {
                command: OwnerCommand::Pause,
                idempotency_key: "slot-release:1".to_string(),
                expected_generation: None,
            },
        )
        .await;

    assert_eq!(bad_request_code(refused), "reserved_idempotency_key");
}

// ---------------------------------------------------------------------------
// Work sent to a stopped task (U6, F4, R13, R14)
// ---------------------------------------------------------------------------

/// Makes `limit` the backend's `max_concurrent_tasks`; read fresh.
fn set_backend_limit(harness: &Harness, limit: u32) {
    let mut record = environment_support::backend("k8s");
    record.limits.max_concurrent_tasks = limit;
    narsor_engine::environment::EnvironmentCatalog::new(harness.root.path())
        .save_backend(&record)
        .expect("save the backend");
}

/// Creates `task` and brings it to `Running` under the controller that
/// already holds the lease, as [`Harness::running`] does under a new one.
async fn run_beside(
    harness: &Harness,
    running: &Running,
    task: &str,
    incarnation: &str,
) -> RunnerClient {
    harness.create(task, "k8s").await;
    running
        .controller
        .report(&report(
            task,
            running.epoch,
            1,
            Observation::StartAccepted,
            None,
        ))
        .await
        .expect("start accepted");
    let credential = running
        .controller
        .issue_runner_credential(
            task,
            &RunnerCredentialRequest {
                lease_epoch: running.epoch,
                generation: 1,
                incarnation: incarnation.to_string(),
            },
        )
        .await
        .expect("runner credential");
    let runner = RunnerClient::new(harness.base_url(), credential.credential);
    runner
        .snapshot(&snapshot(1, ExecutionStatus::Running, 3, None))
        .await
        .expect("first snapshot");
    running
        .controller
        .report(&report(
            task,
            running.epoch,
            1,
            Observation::RunnerReady,
            Some(1),
        ))
        .await
        .expect("runner ready");
    runner
}

/// Stops the running `task-1` the way the slot policy does once its
/// execution reached `status` (and waits on `pending`): the wait runs out,
/// the runner stops at a safe point, the controller reports it.
async fn stop_by_policy(
    harness: &Harness,
    running: &Running,
    status: ExecutionStatus,
    pending: Option<&str>,
) {
    running
        .runner
        .snapshot(&snapshot(2, status, 4, pending))
        .await
        .expect("the execution's state");
    harness.clock.advance(Duration::minutes(6));
    running
        .controller
        .renew_lease(running.epoch, None)
        .await
        .expect("renew the lease");
    running.runner.renew().await.expect("renew the runner");
    narsor_server::runtime::run_periodic_pass(&harness.service);
    let record = control_task(harness, "task-1");
    assert_eq!(record.runner_state, RunnerState::PauseRequested);
    let mut at_safe_point = snapshot(3, status, 4, pending);
    at_safe_point.safe_point = true;
    running
        .runner
        .snapshot(&at_safe_point)
        .await
        .expect("safe point");
    running
        .controller
        .report(&report(
            "task-1",
            running.epoch,
            record.generation,
            Observation::PausedAtSafePoint,
            Some(2),
        ))
        .await
        .expect("paused");
    assert_eq!(
        control_task(harness, "task-1").runner_state,
        RunnerState::Stopped
    );
}

/// The runner of the launch `generation` of `task-1`, as the controller
/// brings it up: its credential, and its first snapshot.
async fn start_runner_of(
    harness: &Harness,
    running: &Running,
    generation: u64,
    status: ExecutionStatus,
    pending: Option<&str>,
) -> RunnerClient {
    let credential = running
        .controller
        .issue_runner_credential(
            "task-1",
            &RunnerCredentialRequest {
                lease_epoch: running.epoch,
                generation,
                incarnation: format!("pod-g{generation}"),
            },
        )
        .await
        .expect("runner credential");
    let runner = RunnerClient::new(harness.base_url(), credential.credential);
    runner
        .snapshot(&snapshot(1, status, 4, pending))
        .await
        .expect("the new runner's first snapshot");
    runner
}

fn resumes_of(record: &narsor_engine::orchestration::TaskRecord) -> usize {
    record
        .commands
        .iter()
        .filter(|receipt| receipt.command == OwnerCommand::Resume)
        .count()
}

/// AE8: an answer to a stopped task on a full backend is kept, the task
/// waits at the front of the queue, and the runner gets the answer once a
/// slot frees and it starts.
#[tokio::test]
async fn ae8_an_answer_to_a_stopped_task_waits_for_a_slot_and_reaches_the_runner_after_the_start() {
    let harness = Harness::start().await;
    set_backend_limit(&harness, 1);
    let running = harness.running("task-1", "k8s").await;
    stop_by_policy(
        &harness,
        &running,
        ExecutionStatus::InputNeeded,
        Some("ask-1"),
    )
    .await;
    let other = run_beside(&harness, &running, "task-2", "pod-b").await;
    assert_eq!(
        harness.task("task-1").await.state,
        narsor_engine::orchestration::TaskState::HumanNeeded
    );

    let answered = harness
        .owner
        .control_input("task-1", &input_request("ask-1", "answer-1"))
        .await
        .expect("an answer to a stopped task is kept");
    assert!(!answered.replayed);
    let queued = harness.task("task-1").await;
    assert_eq!(queued.runner_state, RunnerState::Queued);
    assert!(!queued.holds_slot, "it never starts without a slot");
    assert_eq!(queued.inputs[0].status, InputStatus::Queued);

    // A retry resumes nothing more.
    let retried = harness
        .owner
        .control_input("task-1", &input_request("ask-1", "answer-1"))
        .await
        .expect("retry");
    assert!(retried.replayed);
    let record = control_task(&harness, "task-1");
    assert_eq!(resumes_of(&record), 1);
    assert_eq!(record.generation, queued.generation);

    // The other task stops; its slot goes to the answered one.
    harness
        .command("task-2", OwnerCommand::Pause, "pause-2")
        .await;
    let mut at_safe_point = snapshot(2, ExecutionStatus::Running, 5, None);
    at_safe_point.safe_point = true;
    other.snapshot(&at_safe_point).await.expect("safe point");
    running
        .controller
        .report(&report(
            "task-2",
            running.epoch,
            2,
            Observation::PausedAtSafePoint,
            Some(2),
        ))
        .await
        .expect("paused");
    let admitted = control_task(&harness, "task-1");
    assert!(admitted.reservation.is_some(), "the slot went to it");
    running
        .controller
        .report(&report(
            "task-1",
            running.epoch,
            admitted.generation,
            Observation::StartAccepted,
            None,
        ))
        .await
        .expect("start accepted");
    let runner = start_runner_of(
        &harness,
        &running,
        admitted.generation,
        ExecutionStatus::InputNeeded,
        Some("ask-1"),
    )
    .await;

    let polled = runner.poll(Some("ask-1"), 0).await.expect("poll");
    assert_eq!(polled.inputs.len(), 1);
    assert_eq!(
        polled.inputs[0].delivery.comment.as_deref(),
        Some("looks good")
    );
    let acked = runner
        .ack_input(&polled.inputs[0].input_id, true, None)
        .await
        .expect("ack");
    assert_eq!(acked.input.status, InputStatus::Consumed);
}

/// The first half of [`stop_by_policy`]: the wait of the waiting
/// `task-1` ran out and the policy's pause waits for the safe point.
async fn request_pause_by_policy(harness: &Harness, running: &Running, pending: &str) {
    running
        .runner
        .snapshot(&snapshot(2, ExecutionStatus::InputNeeded, 4, Some(pending)))
        .await
        .expect("input needed");
    harness.clock.advance(Duration::minutes(6));
    running
        .controller
        .renew_lease(running.epoch, None)
        .await
        .expect("renew the lease");
    running.runner.renew().await.expect("renew the runner");
    narsor_server::runtime::run_periodic_pass(&harness.service);
    assert_eq!(
        control_task(harness, "task-1").runner_state,
        RunnerState::PauseRequested
    );
}

/// The runner reaches the safe point and the controller reports it.
async fn confirm_safe_point(harness: &Harness, running: &Running, pending: &str) {
    let generation = control_task(harness, "task-1").generation;
    let mut at_safe_point = snapshot(3, ExecutionStatus::InputNeeded, 4, Some(pending));
    at_safe_point.safe_point = true;
    running
        .runner
        .snapshot(&at_safe_point)
        .await
        .expect("safe point");
    running
        .controller
        .report(&report(
            "task-1",
            running.epoch,
            generation,
            Observation::PausedAtSafePoint,
            Some(2),
        ))
        .await
        .expect("paused");
}

/// An answer accepted while the policy's pause waits for its safe point is
/// not stranded: the safe point resumes the task, which starts and hands
/// the answer to its runner.
#[tokio::test]
async fn an_answer_taken_while_the_policy_pause_waits_resumes_the_task_at_the_safe_point() {
    let harness = Harness::start().await;
    let running = harness.running("task-1", "k8s").await;
    request_pause_by_policy(&harness, &running, "ask-1").await;
    harness
        .owner
        .control_input("task-1", &input_request("ask-1", "answer-1"))
        .await
        .expect("an answer is taken while the pause is pending");
    assert_eq!(
        control_task(&harness, "task-1").runner_state,
        RunnerState::PauseRequested,
        "nothing to resume yet"
    );

    confirm_safe_point(&harness, &running, "ask-1").await;

    let resumed = control_task(&harness, "task-1");
    assert_eq!(resumed.runner_state, RunnerState::Provisioning);
    assert_eq!(resumes_of(&resumed), 1);
    // A retry of the answer resumes nothing more.
    let retried = harness
        .owner
        .control_input("task-1", &input_request("ask-1", "answer-1"))
        .await
        .expect("retry");
    assert!(retried.replayed);
    let record = control_task(&harness, "task-1");
    assert_eq!(resumes_of(&record), 1);
    assert_eq!(record.generation, resumed.generation);

    let runner = start_runner_of(
        &harness,
        &running,
        record.generation,
        ExecutionStatus::InputNeeded,
        Some("ask-1"),
    )
    .await;
    let polled = runner.poll(Some("ask-1"), 0).await.expect("poll");
    assert_eq!(polled.inputs.len(), 1);
    assert_eq!(
        polled.inputs[0].delivery.comment.as_deref(),
        Some("looks good")
    );
}

/// The same on a full backend: the task waits at the front of the queue.
#[tokio::test]
async fn an_answer_taken_while_the_policy_pause_waits_queues_the_task_on_a_full_backend() {
    let harness = Harness::start().await;
    set_backend_limit(&harness, 2);
    let running = harness.running("task-1", "k8s").await;
    run_beside(&harness, &running, "task-2", "pod-b").await;
    request_pause_by_policy(&harness, &running, "ask-1").await;
    harness
        .owner
        .control_input("task-1", &input_request("ask-1", "answer-1"))
        .await
        .expect("an answer is taken while the pause is pending");
    // The other task keeps the only slot that is left.
    set_backend_limit(&harness, 1);

    confirm_safe_point(&harness, &running, "ask-1").await;

    let queued = control_task(&harness, "task-1");
    assert_eq!(queued.runner_state, RunnerState::Queued);
    assert!(
        queued.reservation.is_none(),
        "it never starts without a slot"
    );
    assert_eq!(resumes_of(&queued), 1);
    assert_eq!(queued.inputs[0].status, InputStatus::Queued);
}

/// A finished task that was stopped, on a backend with room.
async fn finished_and_stopped(harness: &Harness) -> Running {
    let running = harness.running("task-1", "k8s").await;
    stop_by_policy(harness, &running, ExecutionStatus::Completed, None).await;
    running
}

#[tokio::test]
async fn a_message_to_the_conductor_of_a_finished_stopped_task_resumes_it_and_is_carried_out_after_the_start(
) {
    let harness = Harness::start().await;
    let running = finished_and_stopped(&harness).await;
    let chat = |text: &str| OperatorCommand::ConductorChat {
        text: text.to_string(),
    };

    let first = harness
        .owner
        .control_operator_command("task-1", &operator_request(chat("first"), "chat-1"))
        .await
        .expect("work for a stopped task is kept");
    assert_eq!(first.command.status, OperatorCommandStatus::Queued);
    let resumed = control_task(&harness, "task-1");
    assert_eq!(resumed.runner_state, RunnerState::Provisioning);
    // The snapshot is of the launch before: the task starts, it is not
    // "finished" until the new runner reports.
    assert_eq!(
        resumed.task_state(),
        narsor_engine::orchestration::TaskState::Queued
    );
    assert_eq!(
        resumed.operator_commands[0].launch_generation,
        resumed.generation
    );

    // The same key again, and a second message before the runner is up.
    let retried = harness
        .owner
        .control_operator_command("task-1", &operator_request(chat("first"), "chat-1"))
        .await
        .expect("retry");
    assert!(retried.replayed);
    harness
        .owner
        .control_operator_command("task-1", &operator_request(chat("second"), "chat-2"))
        .await
        .expect("a second message while the task starts");
    let record = control_task(&harness, "task-1");
    assert_eq!(resumes_of(&record), 1, "one resume for all of it");
    assert_eq!(record.operator_commands.len(), 2);

    let runner = start_runner_of(
        &harness,
        &running,
        record.generation,
        ExecutionStatus::Completed,
        None,
    )
    .await;
    let polled = runner.poll(None, 0).await.expect("poll");
    assert_eq!(
        polled
            .commands
            .iter()
            .map(|command| command.command.clone())
            .collect::<Vec<_>>(),
        vec![chat("first"), chat("second")],
        "both, in the order they were sent"
    );
    for command in &polled.commands {
        runner
            .ack_command(&command.command_id, true, None)
            .await
            .expect("ack");
    }
    // The old runner never saw them.
    assert!(running
        .runner
        .poll(None, 0)
        .await
        .map(|poll| poll.commands.is_empty())
        .unwrap_or(true));
}

#[tokio::test]
async fn undoing_a_decision_of_a_stopped_task_resumes_it_and_waits_for_the_runner() {
    let harness = Harness::start().await;
    let running = finished_and_stopped(&harness).await;

    harness
        .owner
        .control_operator_command(
            "task-1",
            &operator_request(
                OperatorCommand::UndoDecision {
                    decision_id: "decision-1".to_string(),
                },
                "undo-1",
            ),
        )
        .await
        .expect("an undo for a stopped task");

    let record = control_task(&harness, "task-1");
    assert_eq!(record.runner_state, RunnerState::Provisioning);
    let runner = start_runner_of(
        &harness,
        &running,
        record.generation,
        ExecutionStatus::Completed,
        None,
    )
    .await;
    let polled = runner.poll(None, 0).await.expect("poll");
    assert_eq!(
        polled.commands[0].command,
        OperatorCommand::UndoDecision {
            decision_id: "decision-1".to_string()
        }
    );
}

#[tokio::test]
async fn a_command_for_a_stopped_task_on_a_full_backend_queues_it_and_both_commands_survive() {
    let harness = Harness::start().await;
    set_backend_limit(&harness, 1);
    let running = finished_and_stopped(&harness).await;
    run_beside(&harness, &running, "task-2", "pod-b").await;

    for (key, text) in [("chat-1", "first"), ("chat-2", "second")] {
        harness
            .owner
            .control_operator_command(
                "task-1",
                &operator_request(
                    OperatorCommand::ConductorChat {
                        text: text.to_string(),
                    },
                    key,
                ),
            )
            .await
            .expect("kept while the task waits for a slot");
    }

    let record = control_task(&harness, "task-1");
    assert_eq!(record.runner_state, RunnerState::Queued);
    assert_eq!(resumes_of(&record), 1);
    let open: Vec<_> = record
        .operator_commands
        .iter()
        .map(|command| (command.status, command.launch_generation))
        .collect();
    assert_eq!(
        open,
        vec![
            (OperatorCommandStatus::Queued, record.generation),
            (OperatorCommandStatus::Queued, record.generation)
        ],
        "both are bound to the launch the queue leads to"
    );
}

#[tokio::test]
async fn an_answer_to_another_request_than_the_last_snapshots_is_refused_for_a_stopped_task() {
    let harness = Harness::start().await;
    let running = harness.running("task-1", "k8s").await;
    stop_by_policy(
        &harness,
        &running,
        ExecutionStatus::InputNeeded,
        Some("ask-1"),
    )
    .await;
    let revision = harness.task("task-1").await.revision;

    let code = refused_code(
        harness
            .owner
            .control_input("task-1", &input_request("ask-0", "answer-0"))
            .await,
    );

    assert_eq!(code, "obsolete_input");
    let after = harness.task("task-1").await;
    assert_eq!(after.revision, revision, "nothing was written");
    assert_eq!(after.runner_state, RunnerState::Stopped, "and no resume");
}

#[tokio::test]
async fn a_command_that_needs_a_live_runner_is_refused_for_a_stopped_task() {
    let harness = Harness::start().await;
    finished_and_stopped(&harness).await;
    let revision = harness.task("task-1").await.revision;

    for (command, key) in [
        (OperatorCommand::Interrupt, "interrupt-1"),
        (
            OperatorCommand::ConfirmProposal {
                proposal_id: "p-1".to_string(),
            },
            "confirm-1",
        ),
        (
            OperatorCommand::RejectProposal {
                proposal_id: "p-1".to_string(),
            },
            "reject-1",
        ),
    ] {
        match harness
            .owner
            .control_operator_command("task-1", &operator_request(command, key))
            .await
        {
            Err(ClientError::Refused { code, message }) => {
                assert_eq!(code, "operator_command_refused");
                assert!(message.contains("no running runner"), "{message}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
    let task = harness.task("task-1").await;
    assert_eq!(
        task.revision, revision,
        "nothing was written, nothing resumed"
    );
    assert_eq!(task.runner_state, RunnerState::Stopped);
}

#[tokio::test]
async fn an_answer_in_the_wait_period_reaches_the_runner_without_a_resume_and_clears_the_deadline()
{
    let harness = Harness::start().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    runner
        .snapshot(&snapshot(2, ExecutionStatus::InputNeeded, 4, Some("ask-1")))
        .await
        .expect("input needed");
    assert!(control_task(&harness, "task-1")
        .slot_release_due_at
        .is_some());

    harness
        .owner
        .control_input("task-1", &input_request("ask-1", "answer-1"))
        .await
        .expect("the owner answers");
    let polled = runner.poll(Some("ask-1"), 0).await.expect("poll");
    assert_eq!(polled.inputs.len(), 1);
    runner
        .ack_input(&polled.inputs[0].input_id, true, None)
        .await
        .expect("ack");
    runner
        .snapshot(&snapshot(3, ExecutionStatus::Running, 5, None))
        .await
        .expect("it works again");

    let record = control_task(&harness, "task-1");
    assert_eq!(record.runner_state, RunnerState::Running);
    assert_eq!(resumes_of(&record), 0, "nothing was resumed");
    assert_eq!(record.slot_release_due_at, None);
}

#[tokio::test]
async fn an_owner_cannot_use_the_work_resume_keys() {
    let harness = Harness::start().await;
    finished_and_stopped(&harness).await;

    let refused = harness
        .owner
        .control_command(
            "task-1",
            &OwnerCommandRequest {
                command: OwnerCommand::Resume,
                idempotency_key: "work-resume:chat-1".to_string(),
                expected_generation: None,
            },
        )
        .await;

    assert_eq!(bad_request_code(refused), "reserved_idempotency_key");
}

#[tokio::test]
async fn a_runner_whose_credential_ran_out_may_still_renew_it_and_only_that() {
    let harness = Harness::start().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;

    // The host sleeps past the credential's validity: nothing renews it.
    harness.clock.advance(Duration::minutes(20));
    assert!(
        matches!(
            runner
                .snapshot(&snapshot(2, ExecutionStatus::Running, 4, None))
                .await,
            Err(ClientError::Unauthorized)
        ),
        "an expired credential reports nothing"
    );

    let renewed = runner.renew().await.expect("renewal within the grace");
    assert_eq!(
        renewed.expires_at,
        harness.clock.now() + ProtocolConfig::default().runner_credential_ttl
    );
    runner
        .snapshot(&snapshot(2, ExecutionStatus::Running, 4, None))
        .await
        .expect("the runner is heard again");
}

#[tokio::test]
async fn a_credential_expired_past_the_grace_cannot_be_renewed() {
    let harness = Harness::start().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;

    harness.clock.advance(Duration::hours(25));

    assert!(matches!(
        runner.renew().await,
        Err(ClientError::Unauthorized)
    ));
}

// ---------------------------------------------------------------------------
// The limit of concurrent tasks, changed in the running control plane
// ---------------------------------------------------------------------------

/// A runner at a safe point: its session is closed and it waits for the
/// control plane's directive.
async fn at_safe_point(
    runner: &RunnerClient,
    sequence: u64,
) -> narsor_engine::controller_protocol::RunnerSnapshotResponse {
    let mut report = snapshot(sequence, ExecutionStatus::Running, 3, None);
    report.safe_point = true;
    runner.snapshot(&report).await.expect("safe point report")
}

#[tokio::test]
async fn an_ad_hoc_limit_has_priority_over_the_file_until_it_is_cleared() {
    let harness = Harness::start().await;
    set_backend_limit(&harness, 3);

    let view = harness
        .owner
        .control_set_backend_limit("k8s", 5)
        .await
        .expect("set the limit");
    assert_eq!(view.effective, 5);
    assert_eq!(view.file_value, 3);
    assert_eq!(view.override_value, Some(5));
    assert_eq!(
        view.source,
        narsor_protocol::controller::BackendLimitSource::AdHoc
    );

    let view = harness
        .owner
        .control_clear_backend_limit("k8s")
        .await
        .expect("clear the limit");
    assert_eq!(view.effective, 3);
    assert_eq!(view.override_value, None);
    assert_eq!(
        view.source,
        narsor_protocol::controller::BackendLimitSource::File
    );
}

#[tokio::test]
async fn a_limit_outside_the_range_is_refused_and_changes_nothing() {
    let harness = Harness::start().await;
    set_backend_limit(&harness, 2);

    for bad in [0, 2000] {
        let refused = harness.owner.control_set_backend_limit("k8s", bad).await;
        assert_eq!(bad_request_code(refused), "invalid_backend_limit");
    }
    for good in [1, 1024] {
        harness
            .owner
            .control_set_backend_limit("k8s", good)
            .await
            .expect("a limit in range");
    }
    harness
        .owner
        .control_clear_backend_limit("k8s")
        .await
        .expect("clear");
    let view = harness
        .owner
        .control_backend_limit("k8s")
        .await
        .expect("view");
    assert_eq!(view.effective, 2);
    assert_eq!(view.override_value, None);
}

#[tokio::test]
async fn a_limit_of_an_unknown_backend_is_not_found() {
    let harness = Harness::start().await;
    let refused = harness.owner.control_set_backend_limit("nowhere", 2).await;
    assert!(matches!(refused, Err(ClientError::NotFound)), "{refused:?}");
    assert!(harness
        .owner
        .control_backend_limit("nowhere")
        .await
        .is_err());
}

#[tokio::test]
async fn an_ad_hoc_limit_does_not_outlive_the_control_plane() {
    let harness = Harness::start().await;
    set_backend_limit(&harness, 3);
    harness
        .owner
        .control_set_backend_limit("k8s", 5)
        .await
        .expect("set");

    // A new service on the same state: the file is as it was, and the
    // override is gone.
    let fresh = ProtocolService::with_config(
        harness.control_plane(),
        environment_support::seeded_resolver(harness.root.path()),
        test_config(),
    );
    let view = fresh.backend_limit_view("k8s").expect("view");
    assert_eq!(view.effective, 3);
    assert_eq!(view.override_value, None);
}

#[tokio::test]
async fn a_higher_limit_starts_a_waiting_task_at_once() {
    let harness = Harness::start().await;
    set_backend_limit(&harness, 1);
    let first = harness.running("task-1", "k8s").await;
    let _ = &first;
    harness.create("task-2", "k8s").await;
    assert!(!control_task(&harness, "task-2").holds_capacity());

    harness
        .owner
        .control_set_backend_limit("k8s", 2)
        .await
        .expect("raise");

    // No periodic pass ran.
    let waiting = control_task(&harness, "task-2");
    assert!(waiting.holds_capacity(), "the reservation is granted");
    assert_eq!(
        control_task(&harness, "task-1").runner_state,
        RunnerState::Running
    );
}

#[tokio::test]
async fn two_changes_of_the_limit_at_once_end_in_one_committed_value() {
    let harness = Harness::start().await;
    set_backend_limit(&harness, 3);
    let _first = harness.running("task-1", "k8s").await;
    harness.create("task-2", "k8s").await;
    harness.create("task-3", "k8s").await;

    let (lower, higher) = tokio::join!(
        harness.owner.control_set_backend_limit("k8s", 1),
        harness.owner.control_set_backend_limit("k8s", 2),
    );
    lower.expect("lower");
    higher.expect("higher");

    // The value that was stored last is the one the tasks follow.
    let view = harness
        .owner
        .control_backend_limit("k8s")
        .await
        .expect("view");
    assert_eq!(Some(view.effective), view.override_value);
    assert!([1, 2].contains(&view.effective), "{view:?}");
    let held = ["task-1", "task-2", "task-3"]
        .into_iter()
        .filter(|task| control_task(&harness, task).holds_capacity())
        .count();
    assert_eq!(held as u32, view.effective, "{view:?}");
}

#[tokio::test]
async fn a_lower_limit_takes_back_a_reservation_over_it() {
    let harness = Harness::start().await;
    set_backend_limit(&harness, 2);
    let _first = harness.running("task-1", "k8s").await;
    harness.create("task-2", "k8s").await;
    assert!(control_task(&harness, "task-2").holds_capacity());

    harness
        .owner
        .control_set_backend_limit("k8s", 1)
        .await
        .expect("lower");

    let waiting = control_task(&harness, "task-2");
    assert!(!waiting.holds_capacity(), "nothing started on it");
    assert_eq!(waiting.runner_state, RunnerState::Queued);
    // The running task is not over the limit, so nobody is asked to stop.
    assert_eq!(
        control_task(&harness, "task-1").runner_state,
        RunnerState::Running
    );
}

#[tokio::test]
async fn a_lowered_limit_stops_the_first_task_at_a_safe_point_and_queues_it_again() {
    let harness = Harness::start().await;
    set_backend_limit(&harness, 2);
    let first = harness.running("task-1", "k8s").await;
    let second_runner = run_beside(&harness, &first, "task-2", "pod-2").await;

    let view = harness
        .owner
        .control_set_backend_limit("k8s", 1)
        .await
        .expect("lower");
    assert_eq!(view.overshoot, 1);

    // Both are asked, and neither looks paused to its owner.
    for task in ["task-1", "task-2"] {
        let record = control_task(&harness, task);
        assert_eq!(record.runner_state, RunnerState::PauseRequested, "{task}");
        assert!(record.pause_requested_by_limit(), "{task}");
        assert_eq!(
            harness.task(task).await.state,
            narsor_engine::orchestration::TaskState::Running
        );
    }
    let generation = control_task(&harness, "task-1").generation;
    let rank = control_task(&harness, "task-1").rank;

    // The first runner at a safe point stops; the second is told to go on.
    let answer = at_safe_point(&first.runner, 2).await;
    assert_eq!(answer.directive, RunnerDirective::PauseAtSafePoint);
    let answer = at_safe_point(&second_runner, 2).await;
    assert_eq!(answer.directive, RunnerDirective::Continue);
    assert_eq!(answer.runner_state, RunnerState::Running);
    assert!(task_events(&harness, "task-2")
        .iter()
        .any(|kind| matches!(kind, TaskEventKind::LimitStopWithdrawn)));

    // Its compute goes: the task is queued again on its own rank.
    first
        .controller
        .report(&report(
            "task-1",
            first.epoch,
            generation,
            Observation::PausedAtSafePoint,
            Some(2),
        ))
        .await
        .expect("paused at the safe point");
    let queued = control_task(&harness, "task-1");
    assert_eq!(queued.runner_state, RunnerState::Queued);
    assert_eq!(queued.rank, rank);
    assert!(!queued.holds_capacity());
    assert_eq!(
        harness.task("task-1").await.state,
        narsor_engine::orchestration::TaskState::Queued
    );
    assert_eq!(
        slot_releases(&harness, "task-1"),
        vec![TaskEventKind::SlotReleased {
            reason: narsor_engine::orchestration::SlotReleaseReason::LimitLowered
        }]
    );
    assert_eq!(
        control_task(&harness, "task-2").runner_state,
        RunnerState::Running
    );
    let view = harness
        .owner
        .control_backend_limit("k8s")
        .await
        .expect("view");
    assert_eq!(view.overshoot, 0);
    assert_eq!(view.slots_held, 1);
}

#[tokio::test]
async fn a_stop_for_the_limit_is_taken_back_once_the_overshoot_is_gone() {
    let harness = Harness::start().await;
    set_backend_limit(&harness, 2);
    let first = harness.running("task-1", "k8s").await;
    let _second = run_beside(&harness, &first, "task-2", "pod-2").await;
    harness
        .owner
        .control_set_backend_limit("k8s", 1)
        .await
        .expect("lower");
    assert!(control_task(&harness, "task-1").pause_requested_by_limit());

    // The operator raises the limit again before any safe point.
    harness
        .owner
        .control_set_backend_limit("k8s", 2)
        .await
        .expect("raise");
    let answer = at_safe_point(&first.runner, 2).await;

    assert_eq!(answer.directive, RunnerDirective::Continue);
    assert_eq!(
        control_task(&harness, "task-1").runner_state,
        RunnerState::Running
    );
}

#[tokio::test]
async fn a_key_of_the_limit_policy_is_reserved() {
    let harness = Harness::start().await;
    let _running = harness.running("task-1", "k8s").await;
    let refused = harness
        .owner
        .control_command(
            "task-1",
            &OwnerCommandRequest {
                command: OwnerCommand::Pause,
                idempotency_key: "limit-stop:1".to_string(),
                expected_generation: None,
            },
        )
        .await;
    assert_eq!(bad_request_code(refused), "reserved_idempotency_key");
}

#[tokio::test]
async fn a_task_the_limit_stopped_gets_the_next_free_slot_by_its_rank() {
    let harness = Harness::start().await;
    set_backend_limit(&harness, 2);
    let first = harness.running("task-1", "k8s").await;
    let _second = run_beside(&harness, &first, "task-2", "pod-2").await;
    harness
        .owner
        .control_set_backend_limit("k8s", 1)
        .await
        .expect("lower");
    at_safe_point(&first.runner, 2).await;
    first
        .controller
        .report(&report(
            "task-1",
            first.epoch,
            control_task(&harness, "task-1").generation,
            Observation::PausedAtSafePoint,
            Some(2),
        ))
        .await
        .expect("paused at the safe point");
    // A task created later waits behind the stopped one.
    harness.create("task-3", "k8s").await;
    assert!(!control_task(&harness, "task-1").holds_capacity());

    harness
        .owner
        .control_set_backend_limit("k8s", 2)
        .await
        .expect("raise");

    assert!(control_task(&harness, "task-1").holds_capacity());
    assert!(!control_task(&harness, "task-3").holds_capacity());
}

// ---------------------------------------------------------------------------
// The actions of a task (U1-U3)
// ---------------------------------------------------------------------------

/// The summary of a task as a client of an older or newer server sends it.
fn summary_json(extra: serde_json::Value) -> serde_json::Value {
    let at = Utc.with_ymd_and_hms(2026, 10, 22, 12, 0, 0).unwrap();
    let mut summary = serde_json::json!({
        "task_id": "task-1",
        "backend_id": "k8s",
        "runner_state": "running",
        "generation": 1,
        "revision": 1,
        "rank": 1,
        "created_at": at,
        "queue_sequence": 1,
        "workflow_id": "implement-feature",
        "workflow_version": 3,
        "launch_digest": "digest",
        "environment_identity": "env",
        "archive": null,
        "observation": null,
        "runner": null,
        "controller": null,
        "inputs": [],
        "as_of": at,
    });
    summary
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    summary
}

#[test]
fn the_actions_of_a_summary_round_trip() {
    let summary: TaskSummary = serde_json::from_value(summary_json(serde_json::json!({
        "actions": [
            { "id": "comment", "available": true },
            {
                "id": "archive",
                "available": false,
                "reason": "zadanie działa — archiwizować można zakończone lub przerwane",
            },
            {
                "id": "answer",
                "available": true,
                "options": [{ "label": "go", "description": "carry on" }, { "label": "stop" }],
            },
        ],
    })))
    .expect("a summary with actions");
    let actions = summary.actions.clone().expect("the actions");
    assert_eq!(actions.len(), 3);
    assert_eq!(actions[1].id, TaskActionId::Archive);
    assert!(!actions[1].available);
    assert_eq!(
        actions[2].options[0].description.as_deref(),
        Some("carry on")
    );

    let again: TaskSummary =
        serde_json::from_value(serde_json::to_value(&summary).unwrap()).expect("round trip");
    assert_eq!(again, summary);
}

#[test]
fn a_summary_without_actions_reads_as_none_and_writes_none() {
    let summary: TaskSummary =
        serde_json::from_value(summary_json(serde_json::json!({}))).expect("an old summary");
    assert_eq!(summary.actions, None);
    assert!(serde_json::to_value(&summary)
        .unwrap()
        .get("actions")
        .is_none());
}

#[test]
fn an_action_of_a_newer_server_decodes_as_unrecognized_and_the_rest_reads() {
    let summary: TaskSummary = serde_json::from_value(summary_json(serde_json::json!({
        "actions": [
            { "id": "teleport", "available": true },
            { "id": "pause", "available": true },
        ],
    })))
    .expect("a summary with a newer action");
    let ids: Vec<_> = summary.actions.unwrap().into_iter().map(|a| a.id).collect();
    assert_eq!(ids, vec![TaskActionId::Unrecognized, TaskActionId::Pause]);
}

#[test]
fn a_comment_without_an_idempotency_key_is_refused_when_read() {
    let without_key = serde_json::json!({ "text": "keep going" });
    assert!(serde_json::from_value::<SubmitCommentRequest>(without_key).is_err());
    let request: SubmitCommentRequest = serde_json::from_value(serde_json::json!({
        "text": "keep going",
        "idempotency_key": "comment-1",
    }))
    .expect("a comment with a key");
    assert_eq!(request.expected_generation, None);
}

/// The offer of `id`.
fn action(task: &TaskSummary, id: TaskActionId) -> &TaskActionOffer {
    task.actions
        .as_ref()
        .expect("the server offers actions")
        .iter()
        .find(|offer| offer.id == id)
        .unwrap_or_else(|| panic!("{id:?} is not offered"))
}

/// Asserts `id` is available.
fn assert_available(task: &TaskSummary, id: TaskActionId) {
    let offer = action(task, id);
    assert!(offer.available, "{id:?}: {:?}", offer.reason);
    assert_eq!(offer.reason, None, "an available action has no reason");
}

/// Asserts `id` is unavailable, with a reason that contains `reason`.
fn assert_unavailable(task: &TaskSummary, id: TaskActionId, reason: &str) {
    let offer = action(task, id);
    assert!(!offer.available, "{id:?} should not be available");
    let said = offer.reason.as_deref().unwrap_or_default();
    assert!(said.contains(reason), "{id:?}: {said}");
}

/// A snapshot of a task that waits on `id`, which continues as `continuation`.
fn snapshot_waiting(
    sequence: u64,
    event_cursor: u64,
    id: &str,
    continuation: &str,
    kind: AttentionKind,
) -> RunnerSnapshot {
    let mut waiting = snapshot(sequence, ExecutionStatus::InputNeeded, event_cursor, None);
    let form: PendingHumanInput = serde_json::from_value(serde_json::json!({
        "execution_id": "exec-1",
        "state_id": "review",
        "action_execution_id": "act-7",
        "instruction": "What now?",
        "fields": [],
        "outputs": [
            { "label": "go", "description": "carry on" },
            { "label": "stop" },
        ],
        "continuation": continuation,
        "episode": id,
        "requested_at": "2026-10-03T12:00:00Z",
    }))
    .expect("pending input form");
    waiting.pending_input = Some(PendingInputRef {
        id: id.to_string(),
        attempt_id: Some("act-7".to_string()),
        form,
    });
    waiting.attention = vec![attention(kind, id, Some("review"))];
    waiting
}

/// A task of `harness` that waits on a request of `continuation`.
async fn waiting_on_request(harness: &Harness, continuation: &str, kind: AttentionKind) -> Running {
    let running = harness.running("task-1", "k8s").await;
    running
        .runner
        .snapshot(&snapshot_waiting(2, 4, "ep-1", continuation, kind))
        .await
        .expect("input needed");
    running
}

#[tokio::test]
async fn a_finished_task_offers_to_archive_and_not_to_interrupt() {
    let harness = Harness::start().await;
    let running = harness.running("task-1", "k8s").await;
    running
        .runner
        .snapshot(&snapshot(2, ExecutionStatus::Completed, 4, None))
        .await
        .expect("completed");
    let mut at_safe_point = snapshot(3, ExecutionStatus::Completed, 5, None);
    at_safe_point.safe_point = true;
    running.runner.snapshot(&at_safe_point).await.expect("safe");
    running
        .controller
        .report(&report(
            "task-1",
            running.epoch,
            2,
            Observation::PausedAtSafePoint,
            Some(2),
        ))
        .await
        .expect("paused");

    let task = harness.task("task-1").await;
    assert_eq!(task.state, TaskState::Finished);
    assert_available(&task, TaskActionId::Archive);
    assert_unavailable(&task, TaskActionId::Interrupt, "zadanie jest zakończone");
    assert_available(&task, TaskActionId::Resume);
    assert_unavailable(&task, TaskActionId::Comment, "zadanie jest zakończone");
}

#[tokio::test]
async fn an_archived_task_has_nothing_left_to_archive() {
    let harness = Harness::start().await;
    harness.running("task-1", "k8s").await;
    harness
        .command("task-1", OwnerCommand::Archive, "archive-1")
        .await;

    let task = harness.task("task-1").await;
    assert_unavailable(&task, TaskActionId::Archive, "zadanie jest archiwizowane");
}

#[tokio::test]
async fn a_running_task_offers_the_archive_the_control_plane_takes() {
    let harness = Harness::start().await;
    harness.running("task-1", "k8s").await;

    let task = harness.task("task-1").await;
    assert_available(&task, TaskActionId::Archive);
    assert_available(&task, TaskActionId::Interrupt);
    assert_available(&task, TaskActionId::Pause);
    assert_unavailable(&task, TaskActionId::Resume, "zadanie działa");
    assert_unavailable(&task, TaskActionId::Restart, "jest co przerwać");
    assert_unavailable(&task, TaskActionId::Answer, "nie czeka na odpowiedź");
}

#[tokio::test]
async fn a_question_of_the_agent_offers_an_answer_with_its_options_and_a_comment() {
    let harness = Harness::start().await;
    waiting_on_request(&harness, "resolve_output", AttentionKind::Problem).await;

    let task = harness.task("task-1").await;
    assert_available(&task, TaskActionId::Answer);
    let labels: Vec<_> = action(&task, TaskActionId::Answer)
        .options
        .iter()
        .map(|option| option.label.as_str())
        .collect();
    assert_eq!(labels, ["go", "stop"]);
    assert_available(&task, TaskActionId::Comment);
    assert_unavailable(&task, TaskActionId::Approve, "nie czeka na zatwierdzenie");
    assert_unavailable(&task, TaskActionId::AllowPermission, "nie czeka na zgodę");
}

#[tokio::test]
async fn a_permission_request_offers_allow_deny_and_a_comment() {
    let harness = Harness::start().await;
    waiting_on_request(&harness, "resolve_permission", AttentionKind::Problem).await;

    let task = harness.task("task-1").await;
    assert_available(&task, TaskActionId::AllowPermission);
    assert_available(&task, TaskActionId::DenyPermission);
    assert_available(&task, TaskActionId::Comment);
    assert_unavailable(&task, TaskActionId::Answer, "nie czeka na odpowiedź");
}

#[tokio::test]
async fn an_approval_offers_approve_reject_and_a_comment() {
    let harness = Harness::start().await;
    waiting_on_request(&harness, "approve_output", AttentionKind::Approval).await;

    let task = harness.task("task-1").await;
    assert_available(&task, TaskActionId::Approve);
    assert_available(&task, TaskActionId::Reject);
    assert_available(&task, TaskActionId::Comment);
}

#[tokio::test]
async fn a_request_that_takes_no_message_has_no_comment_and_says_so() {
    let harness = Harness::start().await;
    waiting_on_request(&harness, "complete_step", AttentionKind::Question).await;

    let task = harness.task("task-1").await;
    assert_available(&task, TaskActionId::Answer);
    assert_unavailable(&task, TaskActionId::Comment, "nie przyjmuje komentarza");
}

#[tokio::test]
async fn a_task_paused_by_the_operator_offers_a_comment_and_resume() {
    let harness = Harness::start().await;
    let running = harness.running("task-1", "k8s").await;
    pause(&harness, &running).await;

    let task = harness.task("task-1").await;
    assert_eq!(task.runner_state, RunnerState::Stopped);
    assert_eq!(task.state, TaskState::Paused);
    assert_available(&task, TaskActionId::Comment);
    assert_available(&task, TaskActionId::Resume);
    assert_unavailable(&task, TaskActionId::Pause, "zadanie jest wstrzymane");
}

#[tokio::test]
async fn a_queued_task_has_no_comment_for_it_has_no_session() {
    let harness = Harness::start().await;
    let created = harness.create("task-1", "k8s").await;

    assert_unavailable(&created, TaskActionId::Comment, "jest w kolejce");
    assert_unavailable(&created, TaskActionId::Pause, "zadanie jest w kolejce");
}

#[tokio::test]
async fn a_task_that_failed_to_provision_has_no_comment() {
    let harness = Harness::start().await;
    harness.create("task-1", "k8s").await;
    let (controller, epoch) = harness.controller("k8s").await;
    for observation in [
        Observation::StartAccepted,
        Observation::WorkloadLost { state_known: true },
    ] {
        controller
            .report(&report("task-1", epoch, 1, observation, None))
            .await
            .expect("report");
    }

    let task = harness.task("task-1").await;
    assert_unavailable(&task, TaskActionId::Comment, "komentarz można dodać przy");
    assert_available(&task, TaskActionId::Restart);
    assert_available(&task, TaskActionId::Archive);
}

#[tokio::test]
async fn a_task_with_a_failed_step_has_no_comment() {
    let harness = Harness::start().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    let mut failed = snapshot(2, ExecutionStatus::Running, 4, None);
    failed.attention = vec![attention(
        AttentionKind::FailedStep,
        "fail-1",
        Some("review"),
    )];
    runner.snapshot(&failed).await.expect("snapshot");

    let task = harness.task("task-1").await;
    assert_unavailable(&task, TaskActionId::Comment, "komentarz można dodać przy");
}

#[tokio::test]
async fn a_silent_runner_cannot_be_interrupted_or_given_a_transition() {
    let harness = Harness::start().await;
    harness.running("task-1", "k8s").await;
    harness.clock.advance(Duration::minutes(10));

    let task = harness.task("task-1").await;
    assert_unavailable(
        &task,
        TaskActionId::Interrupt,
        "runner nie odpowiada od dłuższego czasu",
    );
    assert_unavailable(
        &task,
        TaskActionId::Transition,
        "runner nie odpowiada od dłuższego czasu",
    );
}

#[tokio::test]
async fn a_transition_needs_the_exits_of_the_state_the_runner_reported() {
    let harness = Harness::start().await;
    write_override_workflow(&harness);
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    // The state of the seeded snapshot is not one of this workflow's.
    let task = harness.task("task-1").await;
    assert!(task.manual_transitions.is_none());
    assert_unavailable(
        &task,
        TaskActionId::Transition,
        "serwer nie podał wyjść bieżącego stanu",
    );

    runner
        .snapshot(&snapshot_in(2, "work", 4))
        .await
        .expect("in work");
    let task = harness.task("task-1").await;
    assert_available(&task, TaskActionId::Transition);
    let exits: Vec<_> = action(&task, TaskActionId::Transition)
        .options
        .iter()
        .map(|option| option.label.as_str())
        .collect();
    assert_eq!(exits, ["success → locked"]);
}

#[tokio::test]
async fn the_list_carries_the_same_actions_as_the_task() {
    let harness = Harness::start().await;
    harness.running("task-1", "k8s").await;
    harness.create("task-2", "k8s").await;

    let listed = harness.owner.control_tasks().await.expect("list").tasks;
    for task in listed {
        let single = harness.task(task.task_id.as_str()).await;
        assert_eq!(task.actions, single.actions, "{}", task.task_id);
        assert!(task.actions.is_some());
    }
}

#[tokio::test]
async fn a_transition_the_offer_does_not_allow_is_refused_with_its_reason() {
    let harness = Harness::start().await;
    write_override_workflow(&harness);
    harness.running("task-1", "k8s").await;
    let before = harness.task("task-1").await;
    let reason = action(&before, TaskActionId::Transition)
        .reason
        .clone()
        .expect("the offer says why");

    match harness
        .owner
        .control_operator_command(
            "task-1",
            &operator_request(transition("locked", false, None), "transition-1"),
        )
        .await
    {
        Err(ClientError::Refused { code, message }) => {
            assert_eq!(code, "action_unavailable");
            assert_eq!(message, reason, "the reason the offer showed a moment ago");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    let after = harness.task("task-1").await;
    assert_eq!(after.revision, before.revision, "nothing was written");
    assert!(after.operator_commands.is_empty());
}

#[tokio::test]
async fn a_pause_of_a_task_whose_step_ended_is_refused_with_the_reason() {
    let harness = Harness::start().await;
    let Running { runner, .. } = harness.running("task-1", "k8s").await;
    runner
        .snapshot(&snapshot(2, ExecutionStatus::Failed, 4, None))
        .await
        .expect("failed");
    let task = harness.task("task-1").await;
    assert_eq!(task.runner_state, RunnerState::Running);
    assert_eq!(task.state, TaskState::Error);

    let refusal = harness
        .owner
        .control_command(
            "task-1",
            &OwnerCommandRequest {
                command: OwnerCommand::Pause,
                idempotency_key: "pause-1".to_string(),
                expected_generation: None,
            },
        )
        .await;
    assert_eq!(refused_code(refusal), "action_unavailable");
    assert_eq!(harness.task("task-1").await.revision, task.revision);
}

#[tokio::test]
async fn an_interrupt_of_a_task_that_stands_is_refused_with_the_reason() {
    let harness = Harness::start().await;
    let running = harness.running("task-1", "k8s").await;
    pause(&harness, &running).await;
    let before = harness.task("task-1").await;

    match harness
        .owner
        .control_operator_command(
            "task-1",
            &operator_request(OperatorCommand::Interrupt, "interrupt-1"),
        )
        .await
    {
        Err(ClientError::Refused { code, message }) => {
            assert_eq!(code, "operator_command_refused", "the code clients know");
            assert!(!message.is_empty());
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(action(&before, TaskActionId::Interrupt).reason.is_some());
}

#[tokio::test]
async fn resuming_a_running_task_is_refused_with_a_reason() {
    let harness = Harness::start().await;
    harness.running("task-1", "k8s").await;

    let refusal = harness
        .owner
        .control_command(
            "task-1",
            &OwnerCommandRequest {
                command: OwnerCommand::Resume,
                idempotency_key: "resume-1".to_string(),
                expected_generation: None,
            },
        )
        .await;
    assert_eq!(refused_code(refusal), "illegal_command");
}

#[tokio::test]
async fn a_stale_generation_is_still_refused_with_its_own_code() {
    let harness = Harness::start().await;
    harness.running("task-1", "k8s").await;
    let generation = harness.task("task-1").await.generation;

    // Interrupting a task that runs is allowed; the stale generation says no.
    let mut request = operator_request(OperatorCommand::Interrupt, "interrupt-1");
    request.expected_generation = Some(generation + 1);
    assert_eq!(
        refused_code(
            harness
                .owner
                .control_operator_command("task-1", &request)
                .await
        ),
        "stale_generation"
    );

    // A move of a task that is not queued: the generation comes first too.
    let stale_move = narsor_engine::controller_protocol::MoveTaskRequest {
        target: narsor_engine::orchestration::MoveTarget::Top.into(),
        expected_generation: Some(generation + 1),
    };
    assert_eq!(
        refused_code(harness.owner.control_move_task("task-1", &stale_move).await),
        "stale_generation"
    );
}

#[tokio::test]
async fn a_move_of_a_task_that_is_not_queued_is_refused_with_the_reason() {
    let harness = Harness::start().await;
    harness.running("task-1", "k8s").await;

    let request = narsor_engine::controller_protocol::MoveTaskRequest {
        target: narsor_engine::orchestration::MoveTarget::Top.into(),
        expected_generation: None,
    };
    match harness.owner.control_move_task("task-1", &request).await {
        Err(ClientError::Refused { code, message }) => {
            assert_eq!(code, "task_not_queued");
            assert_eq!(message, "zadanie nie jest w kolejce");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn repeating_a_key_after_success_returns_the_first_answer_not_an_availability_error() {
    let harness = Harness::start().await;
    harness.running("task-1", "k8s").await;
    let request = OwnerCommandRequest {
        command: OwnerCommand::Pause,
        idempotency_key: "pause-1".to_string(),
        expected_generation: None,
    };
    let first = harness
        .owner
        .control_command("task-1", &request)
        .await
        .expect("pause");
    assert!(!first.replayed);

    // The task is pausing now, which the offer would refuse as a new pause.
    assert_unavailable(
        &harness.task("task-1").await,
        TaskActionId::Pause,
        "zadanie jest już wstrzymywane",
    );
    let second = harness
        .owner
        .control_command("task-1", &request)
        .await
        .expect("the same key");
    assert!(second.replayed);
}

#[tokio::test]
async fn an_answer_to_the_pending_question_is_still_taken_and_an_obsolete_one_keeps_its_code() {
    let harness = Harness::start().await;
    waiting_on_request(&harness, "resolve_output", AttentionKind::Problem).await;

    assert_eq!(
        refused_code(
            harness
                .owner
                .control_input("task-1", &input_request("ep-0", "answer-0"))
                .await
        ),
        "obsolete_input"
    );
    harness
        .owner
        .control_input("task-1", &input_request("ep-1", "answer-1"))
        .await
        .expect("the answer to the pending request");
}

#[tokio::test]
async fn a_permission_decision_is_judged_as_the_decision_it_is() {
    let harness = Harness::start().await;
    waiting_on_request(&harness, "resolve_permission", AttentionKind::Problem).await;
    let before = harness.task("task-1").await;
    assert_available(&before, TaskActionId::AllowPermission);

    let mut deny = input_request("ep-1", "deny-1");
    deny.delivery = narsor_protocol::api::HumanInputDeliveryDto {
        episode: Some("ep-1".to_string()),
        permission_decision: Some(narsor_protocol::api::PermissionDecisionDto {
            denial_index: None,
            decision: narsor_protocol::api::PermissionDecisionKindDto::Deny,
            scope: None,
            pattern: None,
            message: Some("not that".to_string()),
        }),
        ..narsor_protocol::api::HumanInputDeliveryDto::default()
    };
    harness
        .owner
        .control_input("task-1", &deny)
        .await
        .expect("a denial of a pending permission");
}

// ---------------------------------------------------------------------------
// The comment (U4)
// ---------------------------------------------------------------------------

fn comment_request(text: &str, key: &str) -> SubmitCommentRequest {
    SubmitCommentRequest {
        text: text.to_string(),
        idempotency_key: key.to_string(),
        expected_generation: None,
    }
}

#[tokio::test]
async fn a_comment_on_a_permission_denies_with_the_text_and_never_allows() {
    let harness = Harness::start().await;
    let Running { runner, .. } =
        waiting_on_request(&harness, "resolve_permission", AttentionKind::Problem).await;

    let response = harness
        .owner
        .control_comment("task-1", &comment_request("use the read-only tool", "c-1"))
        .await
        .expect("comment");
    assert!(!response.replayed);

    let polled = runner.poll(Some("ep-1"), 0).await.expect("poll");
    let decision = polled.inputs[0]
        .delivery
        .permission_decision
        .as_ref()
        .expect("a permission decision");
    assert_eq!(decision.decision, narsor_protocol::api::PermissionDecisionKindDto::Deny);
    assert_eq!(decision.denial_index, None);
    assert_eq!(decision.message.as_deref(), Some("use the read-only tool"));

    // The request is answered: the task offers no decision on it any more.
    assert_unavailable(
        &response.task,
        TaskActionId::AllowPermission,
        "nie czeka na zgodę",
    );
    assert_unavailable(
        &response.task,
        TaskActionId::DenyPermission,
        "nie czeka na zgodę",
    );
}

#[tokio::test]
async fn a_comment_on_an_approval_returns_the_output_with_the_note_and_does_not_approve() {
    let harness = Harness::start().await;
    let Running { runner, .. } =
        waiting_on_request(&harness, "approve_output", AttentionKind::Approval).await;

    harness
        .owner
        .control_comment("task-1", &comment_request("tests are missing", "c-1"))
        .await
        .expect("comment");

    let polled = runner.poll(Some("ep-1"), 0).await.expect("poll");
    let delivery = &polled.inputs[0].delivery;
    assert_eq!(delivery.action, Some(narsor_protocol::api::HumanInputActionDto::Resume));
    assert_eq!(delivery.comment.as_deref(), Some("tests are missing"));
}

#[tokio::test]
async fn a_comment_on_a_question_reaches_the_agent_as_the_reply() {
    let harness = Harness::start().await;
    let Running { runner, .. } =
        waiting_on_request(&harness, "resume_agent", AttentionKind::Question).await;

    harness
        .owner
        .control_comment("task-1", &comment_request("take the second option", "c-1"))
        .await
        .expect("comment");

    let polled = runner.poll(Some("ep-1"), 0).await.expect("poll");
    let delivery = &polled.inputs[0].delivery;
    assert_eq!(
        delivery.values.get("answer").and_then(|v| v.as_str()),
        Some("take the second option")
    );
    assert_eq!(delivery.episode.as_deref(), Some("ep-1"));
}

#[tokio::test]
async fn a_comment_on_a_problem_is_a_message_to_the_agent() {
    let harness = Harness::start().await;
    let Running { runner, .. } =
        waiting_on_request(&harness, "resolve_output", AttentionKind::Problem).await;

    harness
        .owner
        .control_comment("task-1", &comment_request("try again", "c-1"))
        .await
        .expect("comment");

    let polled = runner.poll(Some("ep-1"), 0).await.expect("poll");
    assert_eq!(
        polled.inputs[0].delivery.agent_message.as_deref(),
        Some("try again")
    );
}

#[tokio::test]
async fn a_comment_key_is_taken_once() {
    let harness = Harness::start().await;
    let Running { runner, .. } =
        waiting_on_request(&harness, "resume_agent", AttentionKind::Question).await;

    let first = harness
        .owner
        .control_comment("task-1", &comment_request("one", "c-1"))
        .await
        .expect("comment");
    let again = harness
        .owner
        .control_comment("task-1", &comment_request("one", "c-1"))
        .await
        .expect("retry");
    assert!(!first.replayed);
    assert!(again.replayed);
    let polled = runner.poll(Some("ep-1"), 0).await.expect("poll");
    assert_eq!(polled.inputs.len(), 1);
}

#[tokio::test]
async fn a_blank_comment_and_a_comment_on_a_running_task_are_refused() {
    let harness = Harness::start().await;
    harness.running("task-1", "k8s").await;

    let task = harness.task("task-1").await;
    assert_unavailable(&task, TaskActionId::Comment, "zadanie działa");
    assert_eq!(
        refused_code(
            harness
                .owner
                .control_comment("task-1", &comment_request("hello", "c-1"))
                .await
        ),
        "action_unavailable"
    );
    let blank = harness
        .owner
        .control_comment("task-1", &comment_request("   ", "c-2"))
        .await;
    assert!(blank.is_err(), "a blank comment is refused: {blank:?}");
}

#[tokio::test]
async fn a_comment_on_an_older_generation_is_refused() {
    let harness = Harness::start().await;
    waiting_on_request(&harness, "resume_agent", AttentionKind::Question).await;
    let task = harness.task("task-1").await;

    let mut request = comment_request("late", "c-1");
    request.expected_generation = Some(task.generation + 1);
    assert_eq!(
        refused_code(harness.owner.control_comment("task-1", &request).await),
        "stale_generation"
    );
}

#[tokio::test]
async fn a_comment_on_a_pause_waits_for_the_next_runner_and_resumes_the_task() {
    let harness = Harness::start().await;
    let running = harness.running("task-1", "k8s").await;
    pause(&harness, &running).await;

    let response = harness
        .owner
        .control_comment("task-1", &comment_request("look at the tests first", "c-1"))
        .await
        .expect("comment");
    assert!(!response.replayed);
    assert_ne!(response.task.runner_state, RunnerState::Stopped);

    let again = harness
        .owner
        .control_comment("task-1", &comment_request("look at the tests first", "c-1"))
        .await
        .expect("retry");
    assert!(again.replayed);

    let record = harness
        .service
        .control_plane()
        .task(&TaskId::parse("task-1").unwrap())
        .unwrap();
    let resumes = record
        .commands
        .iter()
        .filter(|receipt| receipt.command == OwnerCommand::Resume)
        .count();
    assert_eq!(resumes, 1, "one resume for the comment, however often sent");
    assert_eq!(
        record.resume_message.as_ref().map(|m| m.text.as_str()),
        Some("look at the tests first")
    );

    let credential = running
        .controller
        .issue_runner_credential(
            "task-1",
            &RunnerCredentialRequest {
                lease_epoch: running.epoch,
                generation: record.generation,
                incarnation: "pod-2".to_string(),
            },
        )
        .await
        .expect("runner credential");
    let runner = RunnerClient::new(harness.base_url(), credential.credential);
    let answer = runner
        .snapshot(&snapshot(1, ExecutionStatus::Running, 9, None))
        .await
        .expect("snapshot");
    let message = answer.launch.resume_message.expect("the launch carries it");
    assert_eq!(message.key, "c-1");
    assert_eq!(message.text, "look at the tests first");
}

#[tokio::test]
async fn a_blank_comment_on_a_pause_is_refused_and_the_task_stays_paused() {
    let harness = Harness::start().await;
    let running = harness.running("task-1", "k8s").await;
    pause(&harness, &running).await;

    assert!(harness
        .owner
        .control_comment("task-1", &comment_request(" ", "c-1"))
        .await
        .is_err());
    assert_eq!(
        harness.task("task-1").await.runner_state,
        RunnerState::Stopped
    );
}
