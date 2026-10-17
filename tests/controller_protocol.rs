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
    RunnerCredentialRequest, RunnerDirective, SubmitInputRequest, SubmitOperatorCommandRequest,
    TaskDefaultHarness, TaskSummary, LOCAL_OWNER,
};
use narsor_engine::domain::{
    BackendId, DisplayLink, Event, EventData, ExecutionStatus, TaskDisplay, TaskId,
    WorkflowExecution, Workspace,
};
use narsor_engine::engine::{Engine, PendingHumanInput, Timeline};
use narsor_engine::orchestration::{
    AttentionItem, AttentionKind, Clock, CommandRequest, ControlPlane, ConversationAttemptSummary,
    ConversationAvailability, ExecutionCursor, InputStatus, NewTask, Observation, ObservedSummary,
    OperatorCommand, OperatorCommandStatus, OwnerCommand, OwnerRef, PendingInputRef,
    ResourceIdentity, ResumePolicy, RunnerSnapshot, TaskEventKind, TaskLifecycle,
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
        Self::start_with(ProtocolConfig::default(), None).await
    }

    /// A harness with its own timing policy, and optionally a backend whose
    /// task storages this host holds under the given root.
    async fn start_with(config: ProtocolConfig, local: Option<(&str, PathBuf)>) -> Self {
        Self::start_inner(config, local, false).await
    }

    /// A harness whose daemon has a default-harness selector (configured
    /// `claude-code`) that running tasks follow (U7).
    async fn start_with_default_harness() -> Self {
        Self::start_inner(ProtocolConfig::default(), None, true).await
    }

    async fn start_inner(
        config: ProtocolConfig,
        local: Option<(&str, PathBuf)>,
        with_selector: bool,
    ) -> Self {
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
        let mut service = ProtocolService::with_config(control, resolver, config);
        if let Some((backend, storage_root)) = local {
            service = service
                .with_local_storage(BackendId::parse(backend).expect("backend id"), storage_root);
        }
        let service = Arc::new(service);
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
        control.task(&foreign).unwrap().lifecycle,
        TaskLifecycle::Queued
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
         command: \"true\"\n    outputs: [success]\n    transitions:\n      success: locked\n  \
         locked:\n    action:\n      type: script\n      command: \"true\"\n    \
         outputs: [success]\n    transitions:\n      success: done\n  \
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
    assert_eq!(summary.lifecycle, TaskLifecycle::Interrupted);
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
    let harness = Harness::start_with(config, None).await;
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
async fn a_remote_task_without_a_running_runner_answers_unavailable_not_an_error() {
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

#[tokio::test]
async fn a_paused_local_task_is_read_from_its_own_storage() {
    let storage = TempRoot::new();
    let harness = Harness::start_with(
        ProtocolConfig::default(),
        Some(("k8s", storage.path().to_path_buf())),
    )
    .await;
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

    // No storage yet for the execution the runner named.
    assert!(matches!(
        harness
            .owner
            .control_execution_events("task-1", &ExecutionEventsQuery::default())
            .await
            .expect("an answer"),
        ExecutionEventsResponse::Unavailable { .. }
    ));

    let store = ExecutionStore::new(storage.path().join("task-1").join("state"));
    store
        .save_execution(&WorkflowExecution {
            id: "exec-1".to_string(),
            workflow_id: "wf".to_string(),
            workflow_version: 1,
            status: ExecutionStatus::Running,
            current_state: Some("review".to_string()),
            started_at: Utc::now(),
            completed_at: None,
            inputs: serde_json::json!({}),
            context: narsor_engine::domain::Context::new(),
            workspace: Workspace::local_folder("/tmp/none".to_string()),
            current_action_execution: None,
            metadata: serde_json::json!({}),
        })
        .expect("execution");
    for sequence in 1..=5 {
        store
            .event_log("exec-1")
            .append(&execution_event(sequence))
            .expect("event");
    }
    let page = |after, limit| {
        let owner = harness.owner.clone();
        async move {
            owner
                .control_execution_events(
                    "task-1",
                    &ExecutionEventsQuery {
                        after,
                        limit: Some(limit),
                    },
                )
                .await
                .expect("events")
        }
    };
    let (first, more, source) = sequences(page(0, 3).await);
    assert_eq!((first, more), (vec![1, 2, 3], true));
    assert_eq!(source, ExecutionEventsSource::Storage);
    let (rest, more, _) = sequences(page(3, 3).await);
    assert_eq!((rest, more), (vec![4, 5], false));
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
    let harness = Harness::start_with(config, None).await;
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
async fn a_remote_task_without_a_running_runner_has_no_conductor_to_read() {
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
        ConductorReadResponse::Unavailable { reason, message } => {
            assert_eq!(reason, ConversationUnavailableReason::Interrupted);
            assert!(message.contains("runner is not running"), "{message}");
        }
        other => panic!("expected unavailable, got {other:?}"),
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
async fn the_conductor_of_a_stopped_local_task_is_read_from_its_own_storage() {
    let storage = TempRoot::new();
    let harness = Harness::start_with(
        ProtocolConfig::default(),
        Some(("k8s", storage.path().to_path_buf())),
    )
    .await;
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

    let store = ExecutionStore::new(storage.path().join("task-1").join("state"));
    store
        .save_execution(&WorkflowExecution {
            id: "exec-1".to_string(),
            workflow_id: "wf".to_string(),
            workflow_version: 1,
            status: ExecutionStatus::Running,
            current_state: Some("review".to_string()),
            started_at: Utc::now(),
            completed_at: None,
            inputs: serde_json::json!({}),
            context: narsor_engine::domain::Context::new(),
            workspace: Workspace::local_folder("/tmp/none".to_string()),
            current_action_execution: None,
            metadata: serde_json::json!({}),
        })
        .expect("execution");
    let conductor = narsor_engine::conductor::ConductorStore::new(store.paths(), "exec-1");
    for text in ["first", "second\u{1b}[2J", "third"] {
        conductor
            .append_chat(conductor_entry(
                0,
                narsor_engine::conductor::ChatAuthor::Operator,
                text,
            ))
            .expect("chat");
    }

    let read = |after| {
        let owner = harness.owner.clone();
        async move {
            match owner
                .control_conductor(
                    "task-1",
                    &ConductorReadQuery {
                        after,
                        limit: Some(2),
                    },
                )
                .await
                .expect("the conductor")
            {
                ConductorReadResponse::Available { conductor, source } => (*conductor, source),
                other => panic!("expected the conductor, got {other:?}"),
            }
        }
    };
    let (first, source) = read(0).await;
    assert_eq!(source, ExecutionEventsSource::Storage);
    assert!(!first.available, "nothing runs it now");
    assert!(
        first
            .unavailable_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("runner is not running")),
        "{first:?}"
    );
    let texts: Vec<_> = first.chat.iter().map(|entry| entry.text.as_str()).collect();
    assert_eq!(texts, vec!["first", "second[2J"], "cleaned and bounded");
    assert!(first.chat_has_more);
    assert_eq!(first.unanswered_chat.len(), 3);
    let (rest, _) = read(2).await;
    assert_eq!(rest.chat.len(), 1);
    assert!(!rest.chat_has_more);
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
    assert_eq!(record.lifecycle, TaskLifecycle::Provisioning);
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
    assert_eq!(before.lifecycle, TaskLifecycle::Interrupted);

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
    assert_eq!(record.lifecycle, TaskLifecycle::Provisioning);
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
    assert_eq!(record.lifecycle, TaskLifecycle::Interrupted);
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
    assert_eq!(record.lifecycle, TaskLifecycle::Provisioning);
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
        harness.task("task-1").await.lifecycle,
        TaskLifecycle::Paused
    );
    assert_eq!(
        harness.task("task-2").await.lifecycle,
        TaskLifecycle::Interrupted
    );
    assert_eq!(auto_resumed(&task_events(&harness, "task-1")), 0);
    assert_eq!(auto_resumed(&task_events(&harness, "task-2")), 0);
}

#[tokio::test]
async fn an_exhausted_task_is_announced_exactly_once() {
    let harness = Harness::start_with(
        ProtocolConfig {
            resume: ResumePolicy {
                attempt_delays: vec![Duration::zero()],
                stability_window: Duration::minutes(10),
            },
            ..ProtocolConfig::default()
        },
        None,
    )
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
    assert_eq!(record.lifecycle, TaskLifecycle::Interrupted);
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
        harness.task("task-1").await.lifecycle,
        TaskLifecycle::Queued
    );
}

// ---------------------------------------------------------------------------
// The conversation of a local task without a live runner (U6)
// ---------------------------------------------------------------------------

/// A harness whose `k8s` backend keeps its task storages on this host.
async fn local_harness(storage: &TempRoot) -> Harness {
    Harness::start_with(
        ProtocolConfig::default(),
        Some(("k8s", storage.path().to_path_buf())),
    )
    .await
}

/// The task's own storage, as the daemon holding it reads it.
fn local_store(storage: &TempRoot, task: &str) -> ExecutionStore {
    ExecutionStore::new(storage.path().join(task).join("state"))
}

/// `exec-1` — the execution the test runner names — with its attempt
/// `act-1` of `review`, open, holding `entries` lines.
fn record_attempt(store: &ExecutionStore, entries: usize) {
    store
        .save_execution(&WorkflowExecution {
            id: "exec-1".to_string(),
            workflow_id: "wf".to_string(),
            workflow_version: 1,
            status: ExecutionStatus::Running,
            current_state: Some("review".to_string()),
            started_at: Utc::now(),
            completed_at: None,
            inputs: serde_json::json!({}),
            context: narsor_engine::domain::Context::new(),
            workspace: Workspace::local_folder("/tmp/none".to_string()),
            current_action_execution: None,
            metadata: serde_json::json!({}),
        })
        .expect("execution");
    store
        .event_log("exec-1")
        .append_next(
            &"exec-1".to_string(),
            EventData::ActionStarted {
                action_execution_id: "act-1".to_string(),
                state_id: "review".to_string(),
                action_type: "agent".to_string(),
            },
        )
        .expect("event");
    let mut writer = store
        .conversation_store("exec-1")
        .writer("act-1")
        .expect("writer");
    for entry in 1..=entries {
        writer
            .append_narsor(&format!("entry {entry}"), &[])
            .expect("line");
    }
    writer.finish().expect("finish");
}

/// Drives a running task to `Paused` at a safe point.
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

/// Entry numbers, the cursor, the source and the record's time of a page.
fn page_of(
    page: PageResponse,
) -> (
    Vec<u64>,
    u64,
    Option<ExecutionEventsSource>,
    Option<DateTime<Utc>>,
) {
    match page {
        PageResponse::Recorded {
            entries,
            next_entry,
            source,
            recorded_at,
            ..
        } => (
            entries.iter().map(|entry| entry.entry_number).collect(),
            next_entry,
            source,
            recorded_at,
        ),
        PageResponse::NoRecord => panic!("expected a record"),
    }
}

/// The attempts and the record's time of a summary read from storage.
fn stored_summary(response: ConversationResponse) -> (Vec<(String, u64)>, Option<DateTime<Utc>>) {
    match response {
        ConversationResponse::Available {
            attempts,
            source,
            recorded_at,
            ..
        } => {
            assert_eq!(source, ExecutionEventsSource::Storage);
            (
                attempts
                    .into_iter()
                    .map(|attempt| (attempt.attempt_id, attempt.entries))
                    .collect(),
                recorded_at,
            )
        }
        other => panic!("expected a summary from storage, got {other:?}"),
    }
}

/// The runner side of one relayed page, read from the storage exactly as
/// the runner reads it.
async fn answer_page_from(runner: &RunnerClient, store: &ExecutionStore) {
    loop {
        let poll = runner.poll(None, 5).await.expect("poll");
        if let Some(request) = poll.conversation_pages.first() {
            let read = store
                .conversation_store("exec-1")
                .read(
                    &request.attempt_id,
                    request.from_entry,
                    request.byte_budget as usize,
                )
                .expect("read");
            let accepted = runner
                .upload_conversation_page(
                    &request.request_id,
                    &narsor_engine::api::conversation_response_from_snapshot(read),
                )
                .await
                .expect("upload");
            assert!(accepted.delivered);
            return;
        }
    }
}

/// AE5: a paused local task's conversation is read from its record, marked
/// as such and dated.
#[tokio::test]
async fn ae5_a_paused_local_task_reads_its_conversation_from_its_record() {
    let storage = TempRoot::new();
    let harness = local_harness(&storage).await;
    let running = harness.running("task-1", "k8s").await;
    record_attempt(&local_store(&storage, "task-1"), 3);
    pause(&harness, &running).await;
    assert_eq!(
        harness.task("task-1").await.lifecycle,
        TaskLifecycle::Paused
    );

    let (attempts, summary_at) = stored_summary(
        harness
            .owner
            .control_conversation("task-1")
            .await
            .expect("conversation"),
    );
    assert_eq!(
        attempts,
        vec![("act-1".to_string(), 3)],
        "the record, not the last report"
    );
    assert!(summary_at.is_some());

    let (entries, next, source, page_at) =
        page_of(page_from(&harness, "act-1", 0).await.expect("a page"));
    assert_eq!((entries, next), (vec![1, 2, 3], 4));
    assert_eq!(source, Some(ExecutionEventsSource::Storage));
    assert_eq!(page_at, summary_at);
}

#[tokio::test]
async fn interrupted_and_provisioning_local_tasks_read_their_record() {
    let storage = TempRoot::new();
    let harness = local_harness(&storage).await;
    let running = harness.running("task-1", "k8s").await;
    record_attempt(&local_store(&storage, "task-1"), 2);
    lose(&running).await;
    assert_eq!(
        harness.task("task-1").await.lifecycle,
        TaskLifecycle::Interrupted
    );
    let read = |expected: TaskLifecycle| {
        let harness = &harness;
        async move {
            assert_eq!(harness.task("task-1").await.lifecycle, expected);
            let (attempts, _) = stored_summary(
                harness
                    .owner
                    .control_conversation("task-1")
                    .await
                    .expect("conversation"),
            );
            assert_eq!(attempts, vec![("act-1".to_string(), 2)], "{expected:?}");
            let (entries, _, source, _) =
                page_of(page_from(harness, "act-1", 0).await.expect("a page"));
            assert_eq!(entries, vec![1, 2], "{expected:?}");
            assert_eq!(source, Some(ExecutionEventsSource::Storage));
        }
    };
    read(TaskLifecycle::Interrupted).await;

    // Restarted: the old runner's report no longer speaks for the task.
    harness
        .command("task-1", OwnerCommand::Start, "start-again")
        .await;
    read(TaskLifecycle::Provisioning).await;
}

#[tokio::test]
async fn a_provisioning_local_task_without_a_report_has_an_empty_record() {
    let storage = TempRoot::new();
    let harness = local_harness(&storage).await;
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
        .expect("start accepted");
    assert_eq!(
        harness.task("task-1").await.lifecycle,
        TaskLifecycle::Provisioning
    );

    let (attempts, recorded_at) = stored_summary(
        harness
            .owner
            .control_conversation("task-1")
            .await
            .expect("conversation"),
    );
    assert!(attempts.is_empty());
    assert_eq!(recorded_at, None);
    assert!(matches!(
        page_from(&harness, "act-1", 0).await.expect("an answer"),
        PageResponse::NoRecord
    ));
}

/// AE6: a remote task keeps today's answer.
#[tokio::test]
async fn ae6_a_remote_interrupted_task_cannot_be_read_right_now() {
    let storage = TempRoot::new();
    // The daemon holds the storages of another backend only.
    let harness = Harness::start_with(
        ProtocolConfig::default(),
        Some(("local", storage.path().to_path_buf())),
    )
    .await;
    let running = harness.running("task-1", "k8s").await;
    record_attempt(&local_store(&storage, "task-1"), 2);
    lose(&running).await;

    match harness
        .owner
        .control_conversation("task-1")
        .await
        .expect("conversation")
    {
        ConversationResponse::Unavailable {
            reason,
            last_attempts,
            message,
            ..
        } => {
            assert_eq!(reason, ConversationUnavailableReason::Interrupted);
            assert_eq!(last_attempts[0].attempt_id, "act-6", "the last report");
            assert_eq!(message, None);
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
}

#[tokio::test]
async fn an_archived_local_task_cannot_be_read_right_now() {
    let storage = TempRoot::new();
    let harness = local_harness(&storage).await;
    harness.create("task-1", "k8s").await;
    record_attempt(&local_store(&storage, "task-1"), 2);
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
}

#[tokio::test]
async fn an_unreadable_record_says_why_instead_of_being_unavailable() {
    let storage = TempRoot::new();
    let harness = local_harness(&storage).await;
    let running = harness.running("task-1", "k8s").await;
    let store = local_store(&storage, "task-1");
    record_attempt(&store, 1);
    // A line in the middle that is not an entry: the record is damaged,
    // not merely being written.
    let file = store.conversation_store("exec-1").dir().join("act-1.jsonl");
    let mut bytes = fs::read(&file).unwrap();
    bytes.extend_from_slice(b"not an entry\n");
    bytes.extend_from_slice(&fs::read(&file).unwrap());
    fs::write(&file, bytes).unwrap();
    lose(&running).await;

    match harness
        .owner
        .control_conversation("task-1")
        .await
        .expect("conversation")
    {
        ConversationResponse::Unavailable {
            reason, message, ..
        } => {
            assert_eq!(reason, ConversationUnavailableReason::StorageUnreadable);
            let message = message.expect("why");
            assert!(message.contains("could not be read"), "{message}");
            assert!(message.contains("act-1.jsonl"), "{message}");
        }
        other => panic!("expected unreadable, got {other:?}"),
    }
    match page_from(&harness, "act-1", 0).await {
        Err(ClientError::Refused { code, message }) => {
            assert_eq!(code, "conversation_record_unreadable");
            assert!(message.contains("could not be read"), "{message}");
            assert!(!message.contains("right now"), "{message}");
        }
        other => panic!("expected the read error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_record_being_written_is_read_up_to_its_last_whole_line() {
    let storage = TempRoot::new();
    let harness = local_harness(&storage).await;
    let running = harness.running("task-1", "k8s").await;
    let store = local_store(&storage, "task-1");
    record_attempt(&store, 2);
    let file = store.conversation_store("exec-1").dir().join("act-1.jsonl");
    let mut bytes = fs::read(&file).unwrap();
    bytes.extend_from_slice(br#"{"n":3,"ts":"2026-10-03T12:00:00Z","source":"stdout","raw_"#);
    fs::write(&file, bytes).unwrap();
    lose(&running).await;

    let (attempts, _) = stored_summary(
        harness
            .owner
            .control_conversation("task-1")
            .await
            .expect("conversation"),
    );
    assert_eq!(attempts, vec![("act-1".to_string(), 2)]);
    match page_from(&harness, "act-1", 0).await.expect("a page") {
        PageResponse::Recorded {
            entries,
            next_entry,
            truncated_tail,
            ..
        } => {
            assert_eq!(entries.len(), 2);
            assert_eq!(next_entry, 3);
            assert!(truncated_tail);
        }
        other => panic!("expected a record, got {other:?}"),
    }
}

#[tokio::test]
async fn an_attempt_id_that_would_walk_the_path_is_refused() {
    let storage = TempRoot::new();
    let harness = local_harness(&storage).await;
    let running = harness.running("task-1", "k8s").await;
    record_attempt(&local_store(&storage, "task-1"), 1);
    lose(&running).await;

    let owner = OwnerRef::new(LOCAL_OWNER);
    for attempt in ["..", "../act-1", "act/1", "a..b", ".hidden"] {
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

/// The record and the live runner number entries alike: a reader that
/// continues from `next_entry` across the switch neither loses nor repeats
/// an entry.
#[tokio::test]
async fn reading_the_record_then_the_runner_from_the_cursor_loses_and_repeats_nothing() {
    let storage = TempRoot::new();
    let harness = local_harness(&storage).await;
    let running = harness.running("task-1", "k8s").await;
    let store = local_store(&storage, "task-1");
    record_attempt(&store, 5);

    // The runner goes quiet: the record is read, a small page at a time —
    // a budget of exactly three stored lines.
    let record =
        std::fs::read_to_string(store.conversation_store("exec-1").dir().join("act-1.jsonl"))
            .expect("the open record");
    let three_lines: usize = record.lines().take(3).map(str::len).sum();
    harness.clock.advance(Duration::minutes(2));
    let small = harness
        .owner
        .control_conversation_page(
            "task-1",
            "act-1",
            &ConversationPageQuery {
                from: 0,
                byte_budget: Some(three_lines as u64),
            },
        )
        .await
        .expect("a page");
    let (first, next, source, _) = page_of(small);
    assert_eq!((first.clone(), next), (vec![1, 2, 3], 4));
    assert_eq!(source, Some(ExecutionEventsSource::Storage));
    let (whole_record, record_next, _, _) =
        page_of(page_from(&harness, "act-1", 0).await.expect("a page"));

    // It speaks again: the reader continues live from the cursor.
    running
        .runner
        .snapshot(&snapshot(2, ExecutionStatus::Running, 4, None))
        .await
        .expect("back");
    let continued = {
        let owner = harness.owner.clone();
        tokio::spawn(async move {
            owner
                .control_conversation_page(
                    "task-1",
                    "act-1",
                    &ConversationPageQuery {
                        from: next,
                        byte_budget: None,
                    },
                )
                .await
        })
    };
    answer_page_from(&running.runner, &store).await;
    let (rest, _, source, recorded_at) = page_of(continued.await.unwrap().expect("live"));
    assert_eq!(source, Some(ExecutionEventsSource::Runner));
    assert_eq!(recorded_at, None);
    let mut read: Vec<u64> = first;
    read.extend(rest);
    assert_eq!(read, vec![1, 2, 3, 4, 5]);

    // The same attempt read whole from either side is the same.
    let live = {
        let owner = harness.owner.clone();
        tokio::spawn(async move {
            owner
                .control_conversation_page("task-1", "act-1", &ConversationPageQuery::default())
                .await
        })
    };
    answer_page_from(&running.runner, &store).await;
    let (live_entries, live_next, _, _) = page_of(live.await.unwrap().expect("live"));
    assert_eq!((live_entries, live_next), (whole_record, record_next));
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
                "lifecycle": "interrupted",
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
