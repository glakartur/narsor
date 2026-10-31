//! The runner's own decisions, driven in-process (U3, U5, U7, R8, R13, R22).
//!
//! A [`Session`] here is the one [`super::run`] assembles, on a temporary
//! task storage, with three things in-process instead of in a child
//! process: agent actions answer from [`Agent`], the conductor answers from
//! [`Conductor`], and a harness binary is looked for on a `PATH` of empty
//! files ([`HarnessProbe::on_path`]). No credential is mounted, so the
//! session never reaches a control plane: what it would report is read off
//! [`Session::snapshot`], and what a poll answer would carry goes straight
//! to [`Session::follow_default_harness`], [`Session::take_input`] and
//! [`Session::settle_command`], in the order [`Session::absorb`] takes it.
//!
//! What the control plane does with a snapshot, a verdict or a read is
//! tested with the protocol (`tests/controller_protocol.rs`).

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Notify;

use super::conductor::{ConductorHost, HarnessProbe};
use super::storage::{CommandVerdict, StorageLock};
use super::*;
use crate::conductor::{
    ChatAuthor, ConductorAnswer, ConductorInvoker, ConductorView, Invocation, InvocationOutcome,
    ProposalState,
};
use crate::controller_protocol::{
    ConductorReadRequest, DeliveredOperatorCommand, HarnessUsageReport, UsageLimit,
};
use crate::domain::{
    ActionConfig, ActionError, ActionResult, ConductorPurpose, EventData, ResolvedEnvironment,
    ResourceProfile,
};
use crate::engine::{ActionExecutor, ActionKind, ActionOutcome, ActionRequest, UsageReporter};
use crate::orchestration::{AttentionKind, OperatorCommand};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// How long a test waits for in-process work (a pass, the conductor) that
/// it has no handle on.
const PATIENCE: Duration = Duration::from_secs(10);

/// The owner every command here is from.
const OWNER: &str = "owner@example.com";

/// A temporary directory that deletes itself.
struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-session-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create the temporary root");
        Self(path)
    }

    fn dir(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(&path).expect("create a directory");
        path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A `PATH` of one directory holding an empty executable file per
/// `program`: a harness that needs one of them is found, any other is not.
/// Nothing is ever run from it.
fn harness_path(root: &TempRoot, programs: &[&str]) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = root.dir(&format!("bin-{}", uuid::Uuid::new_v4()));
    for program in programs {
        let path = bin.join(program);
        fs::write(&path, "").expect("write a program");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("make it executable");
    }
    bin
}

/// What the in-process agent does on one call.
#[derive(Clone)]
enum Reply {
    /// Answers with this label.
    Label(&'static str),
    /// Answers with this label and these variables.
    Variables(&'static str, Value),
    /// Never answers: the step runs until its pass is dropped.
    Hang,
    /// Answers with this label once the notify is opened.
    After(Arc<Notify>, &'static str),
    /// Reports these usage limits, in order, then answers with this label,
    /// as the agent executor does with what its harness reported.
    Reports(Vec<HarnessUsageReport>, &'static str),
    /// Fails, as a harness that exited with an error.
    Fail,
    /// Answers with text that names no output label.
    Labelless(&'static str),
}

/// One call of the in-process agent.
#[derive(Debug, Clone)]
struct Call {
    prompt: String,
    harness: Option<String>,
}

/// The agent actions of a task, answered in-process: each call takes the
/// next reply, and `fallback` once the list is spent.
struct Agent {
    replies: Mutex<VecDeque<Reply>>,
    fallback: Reply,
    calls: Mutex<Vec<Call>>,
    /// The usage reporter of the last session's engine the agent joined.
    usage: Mutex<Option<Arc<dyn UsageReporter>>>,
}

impl Agent {
    fn new(replies: Vec<Reply>, fallback: Reply) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into()),
            fallback,
            calls: Mutex::new(Vec::new()),
            usage: Mutex::new(None),
        })
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn harnesses(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .map(|call| call.harness.unwrap_or_default())
            .collect()
    }
}

impl ActionExecutor for Agent {
    fn execute<'a>(
        &'a self,
        request: ActionRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ActionOutcome, ActionError>> + Send + 'a>> {
        let ActionConfig::Agent(config) = request.action else {
            panic!("only agent actions run here");
        };
        self.calls.lock().unwrap().push(Call {
            prompt: config.prompt.clone(),
            harness: config.harness.clone(),
        });
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| self.fallback.clone());
        let completed = |label: &str, variables: Value| {
            Ok(ActionOutcome::Completed(ActionResult {
                output_label: label.to_string(),
                data: json!({}),
                variables: variables.as_object().cloned().unwrap_or_default(),
                ..ActionResult::default()
            }))
        };
        Box::pin(async move {
            match reply {
                Reply::Label(label) => completed(label, json!({})),
                Reply::Variables(label, variables) => completed(label, variables),
                Reply::Hang => std::future::pending().await,
                Reply::After(gate, label) => {
                    gate.notified().await;
                    completed(label, json!({}))
                }
                Reply::Reports(reports, label) => {
                    let reporter = self.usage.lock().unwrap().clone();
                    let reporter = reporter.expect("the session's usage reporter");
                    for report in reports {
                        reporter.report(report);
                    }
                    completed(label, json!({}))
                }
                Reply::Fail => Err(ActionError {
                    message: "the build exited with 3".to_string(),
                    error_type: None,
                    details: None,
                }),
                Reply::Labelless(answer) => Err(ActionError {
                    message: "no output label could be extracted from the agent's answer"
                        .to_string(),
                    error_type: Some(
                        crate::engine::actions::agent::MISSING_OUTPUT_LABEL.to_string(),
                    ),
                    details: Some(json!({ "answer_tail": answer })),
                }),
            }
        })
    }
}

/// The conductor of a task, answered in-process: one answer per kind of
/// request, given every time; a request with none fails, as a harness that
/// cannot answer does.
#[derive(Default)]
struct Conductor {
    answers: HashMap<&'static str, Value>,
    calls: Mutex<Vec<Invocation>>,
}

impl Conductor {
    fn answering(answers: &[(&'static str, Value)]) -> Arc<Self> {
        Arc::new(Self {
            answers: answers.iter().cloned().collect(),
            ..Self::default()
        })
    }

    fn silent() -> Arc<Self> {
        Self::answering(&[])
    }

    fn calls_for(&self, purpose: ConductorPurpose) -> Vec<Invocation> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.request.purpose == purpose)
            .cloned()
            .collect()
    }
}

/// Which answer a request takes, by what it asks.
fn answer_key(purpose: ConductorPurpose) -> &'static str {
    match purpose {
        ConductorPurpose::DiagnoseFailure => "diagnose",
        ConductorPurpose::ReplyToChat => "reply",
        ConductorPurpose::SummarizeGate => "annotate",
        ConductorPurpose::GenerateTitle => "title",
        ConductorPurpose::ChooseOutput | ConductorPurpose::ChooseOutputAfterReprompt => "choose",
    }
}

impl ConductorInvoker for Conductor {
    fn invoke<'a>(
        &'a self,
        invocation: &'a Invocation,
    ) -> Pin<Box<dyn Future<Output = InvocationOutcome> + Send + 'a>> {
        self.calls.lock().unwrap().push(invocation.clone());
        let answer = self
            .answers
            .get(answer_key(invocation.request.purpose))
            .cloned();
        Box::pin(async move {
            match answer {
                Some(response) => InvocationOutcome::Answered(ConductorAnswer {
                    message: response
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    response,
                }),
                None => InvocationOutcome::Failed {
                    error: "the conductor has no answer for this".to_string(),
                },
            }
        })
    }
}

/// The launch spec of `yaml`, without a workspace, on `harness`, with a
/// title the operator typed: the conductor is not asked to name the task.
fn spec(yaml: &str, harness: &str) -> TaskLaunchSpec {
    untitled(yaml, harness).with_labels(None, Some("A task".to_string()))
}

/// [`spec`] without a title.
fn untitled(yaml: &str, harness: &str) -> TaskLaunchSpec {
    let definition = crate::workflow::load_from_str(yaml).expect("a valid workflow");
    TaskLaunchSpec::new(
        definition.id.clone(),
        definition.version,
        json!({}),
        TaskWorkspace::None,
        ResolvedEnvironment::harness_only(harness),
        ResourceProfile::new(1000, 2048),
    )
    .with_definition(&definition)
    .expect("snapshot the definition")
}

/// A runner configuration for `storage` whose credential file does not
/// exist: the control plane is never reached.
fn offline_config(root: &TempRoot, storage: &Path, incarnation: &str) -> RunnerConfig {
    RunnerConfig {
        task_id: TaskId::parse("task-1").unwrap(),
        storage: storage.to_path_buf(),
        launch_spec: None,
        control_plane: "http://control-plane.invalid".to_string(),
        credential_file: root.0.join("no-credential"),
        secrets_dir: None,
        harness_home: HarnessHome::default(),
        incarnation: incarnation.to_string(),
        timing: RunnerTiming {
            problem_report_deadline: Duration::ZERO,
            clean_launch_wait: Duration::from_millis(50),
            retry_min: Duration::from_millis(5),
            retry_max: Duration::from_millis(20),
            ..RunnerTiming::default()
        },
        contract: BaseImageContract::default(),
        read_only: false,
        pinned_git_credential_only: false,
    }
}

/// One incarnation of a runner on a task storage, holding its lock.
struct Fixture {
    session: Session,
    agent: Arc<Agent>,
    conductor: Arc<Conductor>,
    spec: TaskLaunchSpec,
    _lock: StorageLock,
}

impl Fixture {
    /// Assembles the session [`super::run`] would on `storage`, after the
    /// storage is initialized for `spec`.
    fn new(
        root: &TempRoot,
        storage: &Path,
        incarnation: &str,
        spec: &TaskLaunchSpec,
        agent: Arc<Agent>,
        conductor: Arc<Conductor>,
        path: &Path,
    ) -> Self {
        let config = offline_config(root, storage, incarnation);
        let storage = TaskStorage::new(storage);
        let lock = storage.lock().expect("lock the storage");
        storage.prepare().expect("prepare the storage");
        initialize(
            &storage,
            &storage.store(),
            &config.task_id,
            spec,
            &MountedSecrets::new(None),
            &crate::workspace::GitAccess::User,
        )
        .expect("initialize the storage");
        let definition = Arc::new(spec.definition().expect("the definition"));
        let link = Link::new(&config);
        let session = assemble_session(
            &config,
            storage,
            link,
            spec,
            definition,
            |engine, reporter| {
                *agent.usage.lock().unwrap() = Some(Arc::clone(reporter));
                engine.with_executor(ActionKind::Agent, agent.clone())
            },
            |engine, _, harness, definition| {
                ConductorHost::assemble_with(
                    engine,
                    harness,
                    definition,
                    conductor.clone(),
                    HarnessProbe::on_path(path.as_os_str()),
                )
            },
        );
        Self {
            session,
            agent,
            conductor,
            spec: spec.clone(),
            _lock: lock,
        }
    }

    /// A fresh storage under `root` for `spec`, on a `PATH` with `claude`.
    fn start(root: &TempRoot, spec: &TaskLaunchSpec, agent: Arc<Agent>) -> Self {
        let path = harness_path(root, &["claude"]);
        Self::new(
            root,
            &root.dir("storage"),
            "pod-1",
            spec,
            agent,
            Conductor::silent(),
            &path,
        )
    }

    /// Decides how to continue and starts the work, as [`Session::begin`]
    /// does before its control loop.
    async fn begin(&mut self) -> Option<RunnerExit> {
        let spec = self.spec.clone();
        self.session
            .start_work(&spec)
            .await
            .expect("start the work")
    }

    fn execution_id(&self) -> String {
        self.session.execution_id.clone().expect("an execution")
    }

    fn execution(&self) -> WorkflowExecution {
        self.session
            .engine
            .store()
            .load_execution(&self.execution_id())
            .expect("load the execution")
    }

    fn status(&self) -> ExecutionStatus {
        self.execution().status
    }

    fn events(&self) -> Vec<EventData> {
        self.session
            .engine
            .store()
            .event_log(&self.execution_id())
            .read_all()
            .expect("read the events")
            .into_iter()
            .map(|event| event.data)
            .collect()
    }

    fn snapshot(&mut self) -> RunnerSnapshot {
        self.session.snapshot(None, false).expect("a snapshot")
    }

    fn attention_kinds(&mut self) -> Vec<AttentionKind> {
        self.snapshot()
            .attention
            .into_iter()
            .map(|item| item.kind)
            .collect()
    }

    /// What an owner's read of the conductor would be answered with.
    fn conductor_view(&self) -> ConductorView {
        reader::conductor_view(
            self.session.engine.store(),
            &self.execution_id(),
            &ConductorReadRequest {
                request_id: "read-1".to_string(),
                after: 0,
                limit: 200,
            },
            self.session.describe_conductor(),
        )
        .expect("read the conductor")
    }

    /// Waits until no pass drives the execution.
    async fn settled(&mut self) {
        if let Some(pass) = self.session.pass.take() {
            let _ = tokio::time::timeout(PATIENCE, pass)
                .await
                .expect("the pass ends");
        }
        let session = &self.session;
        wait_for("the conductor's pass to end", || {
            !session.conductor.passes().active()
        })
        .await;
    }

    /// Waits until the agent has been called `count` times.
    async fn agent_called(&self, count: usize) {
        let agent = &self.agent;
        wait_for("the agent's call", || agent.calls().len() >= count).await;
    }

    async fn command(&mut self, id: &str, command: OperatorCommand) -> CommandVerdict {
        self.session
            .settle_command(DeliveredOperatorCommand {
                command_id: id.to_string(),
                requested_by: OWNER.to_string(),
                command,
            })
            .await
            .expect("settle the command")
    }

    async fn applied(&mut self, id: &str, command: OperatorCommand) {
        let verdict = self.command(id, command).await;
        assert!(verdict.applied, "{id}: {verdict:?}");
    }

    async fn rejected(&mut self, id: &str, command: OperatorCommand) -> String {
        let verdict = self.command(id, command).await;
        assert!(!verdict.applied, "{id}: {verdict:?}");
        verdict.detail.expect("the reason")
    }

    /// The owner's answer to the gate the execution waits at.
    fn answer_gate(&mut self, input_id: &str) -> InputVerdict {
        let pending = self
            .session
            .pending_input_id()
            .expect("read the pending input")
            .expect("a gate");
        self.session
            .take_input(DeliveredInput {
                input_id: input_id.to_string(),
                pending_input_id: pending,
                delivery: narsor_protocol::api::HumanInputDeliveryDto {
                    chosen_label: Some("ok".to_string()),
                    ..Default::default()
                },
            })
            .expect("take the answer")
    }
}

/// Polls `check` until it holds, for at most [`PATIENCE`].
async fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while !check() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn restart(note: Option<&str>, failure_key: Option<&str>) -> OperatorCommand {
    OperatorCommand::RestartStep {
        note: note.map(str::to_string),
        failure_key: failure_key.map(str::to_string),
    }
}

fn interruptions(events: &[EventData]) -> usize {
    events
        .iter()
        .filter(|data| {
            matches!(data, EventData::Error { message, .. } if message.contains("will not be replayed"))
        })
        .count()
}

fn available(harness: &str) -> Option<TaskDefaultHarness> {
    Some(TaskDefaultHarness::Available {
        harness: harness.to_string(),
    })
}

/// One agent step.
const ONE_STEP: &str = r#"
name: one-step
version: 1
states:
  work:
    action:
      type: agent
      prompt: Do the work.
    choices: [done]
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states: [completed]
"#;

/// Two agent steps.
const TWO_STEPS: &str = r#"
name: two-steps
version: 1
states:
  one:
    action:
      type: agent
      prompt: First.
    choices: [done]
    transitions:
      done: two
  two:
    action:
      type: agent
      prompt: Second.
    choices: [done]
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states: [completed]
"#;

/// An agent step, then an owner's gate.
const GATED: &str = r#"
name: gated
version: 1
states:
  think:
    action:
      type: agent
      prompt: Think about it.
    choices: [done]
    transitions:
      done: gate
  gate:
    action:
      type: agent
      prompt: Gate.
    choices: [ok, rejected]
    transitions:
      ok: review
      rejected: failed
    decision:
      type: human
      message: Ship it?
  review:
    action:
      type: agent
      prompt: Review it.
    choices: [done]
    transitions:
      done: completed
  completed:
    description: Done.
  failed:
    description: Rejected.
terminal_states:
  completed: completed
  failed: failed
"#;

// ---------------------------------------------------------------------------
// Offline progress (R5, R14)
// ---------------------------------------------------------------------------

/// Work never waits for the control plane: a runner that cannot reach it
/// runs the task to its end, each step once, and the snapshot it reports
/// once it can is built from storage, at the latest event.
#[tokio::test]
async fn an_offline_runner_keeps_working_and_its_snapshot_has_the_latest_cursor() {
    let root = TempRoot::new();
    let agent = Agent::new(vec![], Reply::Label("done"));
    let mut fixture = Fixture::start(&root, &spec(TWO_STEPS, "claude-code"), agent);
    assert!(fixture.session.link.client().is_none(), "no control plane");

    assert_eq!(fixture.begin().await, None);
    fixture.settled().await;

    assert_eq!(fixture.status(), ExecutionStatus::Completed);
    let prompts: Vec<String> = fixture
        .agent
        .calls()
        .into_iter()
        .map(|call| call.prompt)
        .collect();
    assert_eq!(prompts.len(), 2, "no step ran twice: {prompts:?}");
    let last = fixture
        .session
        .engine
        .store()
        .event_log(&fixture.execution_id())
        .last_sequence()
        .unwrap()
        .expect("events");
    let snapshot = fixture.snapshot();
    assert_eq!(snapshot.execution_status, ExecutionStatus::Completed);
    assert_eq!(snapshot.event_cursor, last);
}

/// A refused credential is read again from its file and renewed before
/// anything else; failures back off, up to the longest delay.
#[test]
fn a_refused_credential_is_renewed_and_failures_back_off() {
    let root = TempRoot::new();
    let mut config = offline_config(&root, &root.dir("storage"), "pod-1");
    config.timing.retry_min = Duration::from_millis(100);
    config.timing.retry_max = Duration::from_millis(250);
    fs::write(&config.credential_file, "first\n").unwrap();
    let mut link = Link::new(&config);
    assert!(link.client().is_some());
    assert!(!link.renew_due(config.timing.renew_margin));

    link.failed(&ClientError::Unauthorized, &config.timing);
    assert!(link.client.is_none(), "the credential is read again");
    assert!(link.renew_due(config.timing.renew_margin), "renewal first");
    let delays: Vec<Duration> = (0..3)
        .map(|_| {
            let before = Instant::now();
            link.failed(&ClientError::Unauthorized, &config.timing);
            link.next_attempt - before
        })
        .collect();
    // Doubled from the shortest, then capped at the longest.
    for (delay, expected) in delays.iter().zip([200, 250, 250]) {
        let expected = Duration::from_millis(expected);
        assert!(
            *delay >= expected && *delay < expected + Duration::from_millis(50),
            "{delays:?}"
        );
    }

    fs::write(&config.credential_file, "reissued\n").unwrap();
    assert!(
        link.client().is_some(),
        "a remounted credential is picked up"
    );
    link.succeeded();
    assert!(link.ready());
    link.renewal_wanted = false;
    link.expires_at = Some(Utc::now() + chrono::Duration::seconds(60));
    assert!(
        link.renew_due(Duration::from_secs(5 * 60)),
        "renewed ahead of its expiry"
    );
}

// ---------------------------------------------------------------------------
// Usage limits (R8)
// ---------------------------------------------------------------------------

/// A report of `claude-code`'s five-hour limit at `utilization`.
fn usage_report(utilization: f64) -> HarnessUsageReport {
    harness_usage_report("claude-code", utilization)
}

/// A report of `harness`'s five-hour limit at `utilization`.
fn harness_usage_report(harness: &str, utilization: f64) -> HarnessUsageReport {
    HarnessUsageReport {
        harness: harness.to_string(),
        observed_at: Utc::now(),
        limits: vec![UsageLimit {
            window: "5h".to_string(),
            utilization,
            resets_at: Utc::now() + chrono::Duration::hours(4),
            status: None,
        }],
    }
}

/// A one-step task whose step reports two usage reports, one after the
/// other, and completes.
fn reporting(root: &TempRoot) -> Fixture {
    let agent = Agent::new(
        vec![Reply::Reports(
            vec![usage_report(0.1), usage_report(0.2)],
            "done",
        )],
        Reply::Label("done"),
    );
    Fixture::start(root, &spec(ONE_STEP, "claude-code"), agent)
}

/// The harness and utilization of each report waiting in the session.
fn waiting_usage(session: &Session) -> Vec<(String, f64)> {
    session
        .usage
        .waiting()
        .into_iter()
        .map(|report| (report.harness, report.limits[0].utilization))
        .collect()
}

/// Waits until the session's usage forwarder can start a send, and starts
/// it with the session's client.
async fn forward_usage(session: &mut Session) -> Option<JoinHandle<()>> {
    tokio::time::timeout(PATIENCE, session.usage.ready())
        .await
        .expect("a report waits and no send is on its way");
    session.usage.forward(session.link.client())
}

/// Covers AE3. The usage limits a step reports leave the control loop
/// once, the latest only, in a task of their own. A control plane that
/// cannot be reached costs the step nothing and leaves the link's backoff
/// as it was: the report is dropped, not retried.
#[tokio::test]
async fn only_the_latest_usage_report_is_sent_once_and_a_failure_leaves_the_link_alone() {
    let root = TempRoot::new();
    let mut fixture = reporting(&root);
    // A credential, for a control plane that is not there.
    fs::write(root.0.join("no-credential"), "credential\n").unwrap();

    assert_eq!(fixture.begin().await, None);
    fixture.settled().await;
    assert_eq!(fixture.status(), ExecutionStatus::Completed);

    let session = &mut fixture.session;
    let (failures, next_attempt) = (session.link.failures, session.link.next_attempt);
    assert_eq!(
        waiting_usage(session),
        [("claude-code".to_string(), 0.2)],
        "only the latest waits"
    );

    let sent = forward_usage(session)
        .await
        .expect("one attempt to send it");
    tokio::time::timeout(PATIENCE, sent)
        .await
        .expect("the attempt ends")
        .expect("the attempt does not panic");

    assert!(
        session.usage.waiting().is_empty(),
        "nothing is left to send again"
    );
    assert!(
        session.usage.forward(session.link.client()).is_none(),
        "no retry"
    );
    assert_eq!(session.link.failures, failures);
    assert_eq!(session.link.next_attempt, next_attempt);
}

/// Without a credential there is no client: the report is dropped and the
/// link is left as it was.
#[tokio::test]
async fn without_a_client_the_usage_report_is_dropped() {
    let root = TempRoot::new();
    let mut fixture = reporting(&root);

    assert_eq!(fixture.begin().await, None);
    fixture.settled().await;
    assert_eq!(fixture.status(), ExecutionStatus::Completed);

    let session = &mut fixture.session;
    let (failures, next_attempt) = (session.link.failures, session.link.next_attempt);
    assert!(
        forward_usage(session).await.is_none(),
        "no client to send with"
    );
    assert!(session.usage.waiting().is_empty());
    assert_eq!(session.link.failures, failures);
    assert_eq!(session.link.next_attempt, next_attempt);
}

/// A control plane on a local port that records the utilization of every
/// usage report it receives, in order, and holds its answer to the first
/// one until `release` is opened.
struct UsageServer {
    url: String,
    received: Arc<Mutex<Vec<f64>>>,
    arrived: Arc<Notify>,
    release: Arc<Notify>,
}

impl UsageServer {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local port");
        let url = format!("http://{}", listener.local_addr().unwrap());
        let received = Arc::new(Mutex::new(Vec::new()));
        let arrived = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (seen, signal, held) = (received.clone(), arrived.clone(), release.clone());
        tokio::spawn(async move {
            while let Ok((connection, _)) = listener.accept().await {
                let (seen, signal, held) = (seen.clone(), signal.clone(), held.clone());
                tokio::spawn(Self::serve(connection, seen, signal, held));
            }
        });
        Self {
            url,
            received,
            arrived,
            release,
        }
    }

    /// Answers the requests of one connection with no content.
    async fn serve(
        mut connection: tokio::net::TcpStream,
        received: Arc<Mutex<Vec<f64>>>,
        arrived: Arc<Notify>,
        release: Arc<Notify>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buffer = Vec::new();
        loop {
            let Some(head_end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else {
                let mut chunk = [0u8; 4096];
                match connection.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                }
                continue;
            };
            let head = String::from_utf8_lossy(&buffer[..head_end]).to_lowercase();
            let length: usize = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .map(|value| value.trim().parse().expect("a length"))
                .unwrap_or(0);
            let body_start = head_end + 4;
            while buffer.len() < body_start + length {
                let mut chunk = [0u8; 4096];
                match connection.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                }
            }
            let report: HarnessUsageReport =
                serde_json::from_slice(&buffer[body_start..body_start + length])
                    .expect("a usage report");
            buffer.drain(..body_start + length);
            let first = {
                let mut received = received.lock().unwrap();
                received.push(report.limits[0].utilization);
                received.len() == 1
            };
            arrived.notify_one();
            if first {
                release.notified().await;
            }
            let answer = b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n";
            if connection.write_all(answer).await.is_err() {
                return;
            }
        }
    }

    fn received(&self) -> Vec<f64> {
        self.received.lock().unwrap().clone()
    }
}

/// The fixture's session pointed at `server`, with the reporter its engine
/// was given.
fn usage_session<'a>(
    fixture: &'a mut Fixture,
    server: &UsageServer,
) -> (&'a mut Session, Arc<dyn UsageReporter>) {
    let reporter = fixture
        .agent
        .usage
        .lock()
        .unwrap()
        .clone()
        .expect("the session's usage reporter");
    let session = &mut fixture.session;
    session.link.base_url = server.url.clone();
    session.link.client = None;
    (session, reporter)
}

/// Waits until `send` ended.
async fn sent(send: Option<JoinHandle<()>>) {
    tokio::time::timeout(PATIENCE, send.expect("a send under way"))
        .await
        .expect("the send ends")
        .expect("the send does not panic");
}

/// A newer usage report waits while an older one is still on its way, and
/// goes out only after it: the control plane, which keeps whichever report
/// came last, ends with the newer one.
#[tokio::test]
async fn a_newer_usage_report_waits_for_the_send_under_way_and_arrives_last() {
    let root = TempRoot::new();
    let mut fixture = reporting(&root);
    let server = UsageServer::start().await;
    fs::write(root.0.join("no-credential"), "credential\n").unwrap();

    assert_eq!(fixture.begin().await, None);
    fixture.settled().await;
    let (session, reporter) = usage_session(&mut fixture, &server);

    let first = forward_usage(session).await;
    assert!(first.is_some(), "the first send starts");
    tokio::time::timeout(PATIENCE, server.arrived.notified())
        .await
        .expect("the first report reaches the control plane");
    assert_eq!(server.received(), vec![0.2]);

    reporter.report(usage_report(0.3));
    assert!(
        session.usage.forward(session.link.client()).is_none(),
        "no second send while the first is on its way"
    );
    assert_eq!(
        waiting_usage(session),
        [("claude-code".to_string(), 0.3)],
        "the newer report waits"
    );

    server.release.notify_one();
    sent(first).await;
    sent(forward_usage(session).await).await;

    assert_eq!(
        server.received(),
        vec![0.2, 0.3],
        "the newer one arrives last"
    );
}

/// Reports of two harnesses made before a send both reach the control
/// plane, one after the other; two reports of one harness are merged, so
/// only the newer goes out.
#[tokio::test]
async fn the_latest_report_of_each_harness_reaches_the_control_plane() {
    let root = TempRoot::new();
    let mut fixture = reporting(&root);
    let server = UsageServer::start().await;
    fs::write(root.0.join("no-credential"), "credential\n").unwrap();

    assert_eq!(fixture.begin().await, None);
    fixture.settled().await;
    let (session, reporter) = usage_session(&mut fixture, &server);
    reporter.report(harness_usage_report("codex", 0.5));
    reporter.report(usage_report(0.6));
    assert_eq!(
        waiting_usage(session),
        [("claude-code".to_string(), 0.6), ("codex".to_string(), 0.5)]
    );

    server.release.notify_one();
    sent(forward_usage(session).await).await;
    sent(forward_usage(session).await).await;

    assert_eq!(server.received(), vec![0.6, 0.5]);
    assert!(session.usage.waiting().is_empty());
}

// ---------------------------------------------------------------------------
// What the snapshot says
// ---------------------------------------------------------------------------

const DISPLAY: &str = r#"
name: shown-task
version: 1
display:
  title: Shown
  links:
    - label: PR
      text: "!{{ context.variables.pr_number }}"
      url: "{{ context.variables.pr_url }}"
states:
  open_pr:
    action:
      type: agent
      prompt: Open the PR.
    variables:
      - { name: pr_number, description: What the step reports as pr_number. }
      - { name: pr_url, description: What the step reports as pr_url. }
    choices: [done]
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states: [completed]
"#;

/// The runner renders `display` against the variables its steps set.
#[tokio::test]
async fn the_snapshot_carries_the_display_rendered_from_context_variables() {
    let root = TempRoot::new();
    let agent = Agent::new(
        vec![Reply::Variables(
            "done",
            json!({"pr_number": 42, "pr_url": "https://git.example/pr/42"}),
        )],
        Reply::Fail,
    );
    let mut fixture = Fixture::start(&root, &spec(DISPLAY, "claude-code"), agent);
    fixture.begin().await;
    fixture.settled().await;

    let display = fixture.snapshot().display.expect("a display");
    assert_eq!(display.title, "Shown");
    assert_eq!(display.links[0].text, "!42");
    assert_eq!(
        display.links[0].url.as_deref(),
        Some("https://git.example/pr/42")
    );
}

// ---------------------------------------------------------------------------
// Base-image contract (R22)
// ---------------------------------------------------------------------------

/// A base image without `git` stops the runner before any work: the problem
/// says so, nothing is cloned and no execution exists.
#[tokio::test]
async fn a_base_image_without_git_fails_before_any_work() {
    let root = TempRoot::new();
    let storage = root.dir("storage");
    let spec = spec(ONE_STEP, "claude-code");
    let spec_file = root.0.join("spec.json");
    fs::write(&spec_file, serde_json::to_string(&spec).unwrap()).unwrap();
    let mut config = offline_config(&root, &storage, "pod-1");
    config.launch_spec = Some(spec_file);
    // Every basic tool but git, as empty programs: none of them is run.
    config.contract.path = Some(
        harness_path(
            &root,
            &["cat", "cp", "env", "grep", "ls", "mkdir", "mv", "rm", "sed"],
        )
        .into_os_string(),
    );

    let exit = run(config, |_| panic!("no environment is applied"))
        .await
        .expect("the runner ends");

    let RunnerExit::Problem(problem) = exit else {
        panic!("a problem: {exit:?}");
    };
    assert_eq!(problem.code, PROBLEM_BASE_IMAGE);
    assert!(
        problem.message.contains("git is not on PATH"),
        "{}",
        problem.message
    );
    assert!(
        !problem.message.contains("basic tools"),
        "{}",
        problem.message
    );
    let storage = TaskStorage::new(&storage);
    assert!(!storage.workspace_dir().exists(), "nothing was cloned");
    assert!(
        storage.marker().unwrap().is_none(),
        "nothing was initialized"
    );
    assert!(storage.store().list_execution_ids().unwrap().is_empty());
}

/// What a runner that cannot work reports waits for the owner as a problem
/// named by its code.
#[test]
fn a_problem_waits_for_the_owner_under_its_code() {
    let problem = RunnerProblem {
        code: PROBLEM_BASE_IMAGE.to_string(),
        message: "git is not on PATH".to_string(),
    };
    let items = snapshot::attention_items(None, Some(&problem), Utc::now());
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind, AttentionKind::Problem);
    assert_eq!(items[0].waiting_id, PROBLEM_BASE_IMAGE);
}

// ---------------------------------------------------------------------------
// Operator commands (U3)
// ---------------------------------------------------------------------------

/// An agent step that allows overrides.
const OVERRIDABLE: &str = r#"
name: agent-task
version: 1
states:
  think:
    allow_manual_override: true
    action:
      type: agent
      prompt: Think about it.
    choices: [done]
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states: [completed]
"#;

/// A runner whose agent hangs in its first call, with the call under way.
async fn hanging(root: &TempRoot, yaml: &str) -> Fixture {
    let agent = Agent::new(vec![Reply::Hang], Reply::Label("done"));
    let mut fixture = Fixture::start(root, &spec(yaml, "claude-code"), agent);
    fixture.begin().await;
    fixture.agent_called(1).await;
    fixture
}

#[tokio::test]
async fn an_interrupted_step_is_dropped_and_restarted_with_the_owners_note() {
    let root = TempRoot::new();
    let mut fixture = hanging(&root, OVERRIDABLE).await;

    // A restart while the step runs is refused by the Engine's own claim.
    let early = fixture.rejected("restart-early", restart(None, None)).await;
    assert!(early.contains("interrupt it first"), "{early}");

    fixture
        .applied("interrupt-1", OperatorCommand::Interrupt)
        .await;
    assert_eq!(fixture.status(), ExecutionStatus::Unknown);
    assert!(!fixture.session.pass_active(), "the pass is dropped");
    assert_eq!(interruptions(&fixture.events()), 1);

    // Nothing is running any more, so a second interrupt has nothing to cut.
    let again = fixture
        .rejected("interrupt-2", OperatorCommand::Interrupt)
        .await;
    assert!(again.contains("nothing to interrupt"), "{again}");

    fixture
        .applied("restart-1", restart(Some("Mind the edge cases."), None))
        .await;
    fixture.settled().await;
    assert_eq!(fixture.status(), ExecutionStatus::Completed);
    let calls = fixture.agent.calls();
    assert_eq!(calls.len(), 2);
    assert!(!calls[0].prompt.contains("Operator recovery note:"));
    assert!(
        calls[1].prompt.contains("Operator recovery note:")
            && calls[1].prompt.contains("Mind the edge cases."),
        "only the restarted attempt got the note: {}",
        calls[1].prompt
    );
}

/// A command delivered again after its acknowledgement was lost is
/// answered from the verdict on storage, not carried out twice.
#[tokio::test]
async fn a_redelivered_command_is_never_carried_out_twice() {
    let root = TempRoot::new();
    let mut fixture = hanging(&root, OVERRIDABLE).await;

    let first = fixture
        .command("interrupt-1", OperatorCommand::Interrupt)
        .await;
    let again = fixture
        .command("interrupt-1", OperatorCommand::Interrupt)
        .await;

    // Carried out again, an interrupt would find nothing to cut and be
    // rejected: the verdict is the first one's.
    assert!(first.applied, "{first:?}");
    assert_eq!(again, first);
    assert_eq!(interruptions(&fixture.events()), 1);
}

/// A command a runner began but never settled (it died) is refused when it
/// comes again, not carried out a second time.
#[tokio::test]
async fn a_command_begun_by_a_runner_that_died_is_refused() {
    let root = TempRoot::new();
    let mut fixture = hanging(&root, OVERRIDABLE).await;
    fixture
        .session
        .storage
        .begin_command("interrupt-1")
        .unwrap();

    let detail = fixture
        .rejected("interrupt-1", OperatorCommand::Interrupt)
        .await;

    assert!(detail.contains("runner stopped"), "{detail}");
    assert!(fixture.session.pass_active(), "the step was not touched");
    assert_eq!(fixture.status(), ExecutionStatus::Running);
}

#[tokio::test]
async fn a_restart_naming_a_failure_the_task_has_moved_on_from_is_rejected_with_the_reason() {
    let root = TempRoot::new();
    let mut fixture = hanging(&root, OVERRIDABLE).await;
    fixture
        .applied("interrupt-1", OperatorCommand::Interrupt)
        .await;
    let events_before = fixture.events().len();

    let detail = fixture
        .rejected(
            "restart-1",
            restart(Some("again"), Some("failure-of-another-time")),
        )
        .await;

    assert!(detail.contains("failure-of-another-time"), "{detail}");
    assert_eq!(
        fixture.status(),
        ExecutionStatus::Unknown,
        "the execution was not touched"
    );
    assert_eq!(fixture.events().len(), events_before);
}

#[tokio::test]
async fn an_override_transition_is_taken_under_the_claim_and_audited_with_its_reason() {
    let root = TempRoot::new();
    let mut fixture = hanging(&root, OVERRIDABLE).await;
    let transition = |reason: &str| OperatorCommand::Transition {
        target: "completed".to_string(),
        forced: true,
        reason: Some(reason.to_string()),
    };

    // While the agent step runs, no transition can be taken.
    let busy = fixture
        .rejected("transition-early", transition("early"))
        .await;
    assert!(busy.contains("interrupt it first"), "{busy}");
    assert_eq!(fixture.status(), ExecutionStatus::Running);

    fixture
        .applied("interrupt-1", OperatorCommand::Interrupt)
        .await;
    fixture
        .applied("transition-1", transition("the ticket was closed upstream"))
        .await;

    assert_eq!(fixture.status(), ExecutionStatus::Completed);
    // The execution's own log records the override with its reason and
    // the owner who asked, under the override provider.
    assert!(fixture.events().iter().any(|data| matches!(
        data,
        EventData::DecisionMade { provider, reasoning: Some(reasoning), .. }
            if provider == "human_override"
                && reasoning.contains("the ticket was closed upstream")
                && reasoning.contains(OWNER)
    )));
}

const AGENT_THEN_AGENT: &str = r#"
name: agent-then-agent
version: 1
states:
  think:
    action:
      type: agent
      prompt: Think about it.
    choices: [done]
    transitions:
      done: next
  next:
    action:
      type: agent
      prompt: Next.
    choices: [done]
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states: [completed]
"#;

#[tokio::test]
async fn a_declared_transition_carries_on_from_the_state_it_leads_to() {
    let root = TempRoot::new();
    let mut fixture = hanging(&root, AGENT_THEN_AGENT).await;
    fixture
        .applied("interrupt-1", OperatorCommand::Interrupt)
        .await;

    // Not an exit of `think`, and no override: refused by the runner,
    // whatever the control plane let through.
    fixture
        .rejected(
            "transition-bad",
            OperatorCommand::Transition {
                target: "completed".to_string(),
                forced: false,
                reason: None,
            },
        )
        .await;
    assert_eq!(fixture.status(), ExecutionStatus::Unknown);

    fixture
        .applied(
            "transition-1",
            OperatorCommand::Transition {
                target: "next".to_string(),
                forced: false,
                reason: None,
            },
        )
        .await;
    fixture.settled().await;

    assert_eq!(fixture.status(), ExecutionStatus::Completed);
    let prompts: Vec<String> = fixture
        .agent
        .calls()
        .into_iter()
        .map(|call| call.prompt)
        .collect();
    assert_eq!(prompts.len(), 2, "{prompts:?}");
    assert!(prompts[1].starts_with("Next."), "{prompts:?}");
    assert!(fixture.events().iter().any(|data| matches!(
        data,
        EventData::DecisionMade { provider, .. } if provider == "human"
    )));
}

// ---------------------------------------------------------------------------
// The conductor in the runner (U5)
// ---------------------------------------------------------------------------

/// A conductor's reply with a proposal.
fn diagnosis() -> Value {
    json!({
        "decision": "diagnose",
        "confidence": "high",
        "rationale": "the step exits with 3 on its first run",
        "message": "The build failed.",
        "diagnosis": "the step exited with 3",
        "proposals": [
            {"kind": "retry", "summary": "Run the build again", "text": "run it again"}
        ],
    })
}

/// A runner on a fresh storage whose one step fails first, with `conductor`
/// and harness programs `programs` on its `PATH`.
fn failing_first(
    root: &TempRoot,
    harness: &str,
    conductor: Arc<Conductor>,
    programs: &[&str],
) -> Fixture {
    let path = harness_path(root, programs);
    Fixture::new(
        root,
        &root.dir("storage"),
        "pod-1",
        &spec(ONE_STEP, harness),
        Agent::new(vec![Reply::Fail], Reply::Label("done")),
        conductor,
        &path,
    )
}

/// A failed step with a conductor: it is diagnosed, the failed step waits
/// for the owner with the diagnosis and its proposals, and confirming the
/// proposal restarts the step and clears the wait.
#[tokio::test]
async fn a_failed_step_is_diagnosed_and_its_confirmed_proposal_restarts_it() {
    let root = TempRoot::new();
    let conductor = Conductor::answering(&[("diagnose", diagnosis())]);
    let mut fixture = failing_first(&root, "claude-code", conductor, &["claude"]);
    fixture.begin().await;
    fixture.settled().await;
    assert_eq!(fixture.status(), ExecutionStatus::Failed);

    let view = {
        let fixture = &fixture;
        let mut view = None;
        wait_for("the diagnosis", || {
            let read = fixture.conductor_view();
            let ready = read.failure.is_some() && !read.proposals.is_empty();
            view = Some(read);
            ready
        })
        .await;
        view.unwrap()
    };
    assert!(view.available, "{view:?}");
    assert_eq!(view.harness.as_deref(), Some("claude-code"));
    let failure = view.failure.as_ref().unwrap();
    assert!(failure.reason.contains("exited with 3"), "{failure:?}");
    let proposal = view.proposals[0].clone();
    assert_eq!(proposal.state, ProposalState::Pending);
    assert_eq!(proposal.in_reply_to, None, "it came with the diagnosis");

    // The failed step waits, named by the key a restart of it names; the
    // diagnosis' proposal is part of it, not a second wait.
    let key = crate::engine::step_failure(&fixture.execution())
        .unwrap()
        .expect("the failure is recorded")
        .key()
        .to_string();
    let snapshot = fixture.snapshot();
    assert_eq!(snapshot.attention.len(), 1, "{:?}", snapshot.attention);
    assert_eq!(snapshot.attention[0].kind, AttentionKind::FailedStep);
    assert_eq!(snapshot.attention[0].waiting_id, key);
    assert_eq!(snapshot.attention[0].state_id.as_deref(), Some("work"));
    assert!(snapshot.conductor_available);
    assert_eq!(snapshot.conductor_unavailable_reason, None);

    fixture
        .applied(
            "confirm-1",
            OperatorCommand::ConfirmProposal {
                proposal_id: proposal.proposal_id.clone(),
            },
        )
        .await;
    fixture.settled().await;
    assert_eq!(fixture.status(), ExecutionStatus::Completed);
    assert!(fixture.attention_kinds().is_empty());
    assert_eq!(
        fixture.conductor_view().proposals[0].state,
        ProposalState::Executed
    );

    // Settled once: confirming it again is refused, with the reason.
    let again = fixture
        .rejected(
            "confirm-2",
            OperatorCommand::ConfirmProposal {
                proposal_id: proposal.proposal_id,
            },
        )
        .await;
    assert!(again.contains("already settled"), "{again}");
}

/// A runner whose harness cannot run here has no conductor and says why: a
/// failed step goes to the owner as it is, without a diagnosis, and a
/// restart naming its key carries it on.
#[tokio::test]
async fn without_a_conductor_a_failed_step_waits_for_the_owner_without_proposals() {
    let root = TempRoot::new();
    let conductor = Conductor::answering(&[("diagnose", diagnosis())]);
    let mut fixture = failing_first(&root, "no-such-harness", conductor, &["claude"]);
    fixture.begin().await;
    fixture.settled().await;

    assert_eq!(fixture.attention_kinds(), vec![AttentionKind::FailedStep]);
    let snapshot = fixture.snapshot();
    assert!(!snapshot.conductor_available);
    assert!(
        snapshot
            .conductor_unavailable_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("no-such-harness")),
        "{:?}",
        snapshot.conductor_unavailable_reason
    );
    let view = fixture.conductor_view();
    assert!(!view.available);
    assert!(view.unavailable_reason.is_some());
    assert!(view.failure.is_none(), "nobody diagnosed it: {view:?}");
    assert!(view.proposals.is_empty());
    assert!(fixture
        .conductor
        .calls_for(ConductorPurpose::DiagnoseFailure)
        .is_empty());

    for (id, command) in [
        (
            "chat-1",
            OperatorCommand::ConductorChat {
                text: "anyone there?".to_string(),
            },
        ),
        (
            "confirm-1",
            OperatorCommand::ConfirmProposal {
                proposal_id: "p".to_string(),
            },
        ),
    ] {
        let detail = fixture.rejected(id, command).await;
        assert!(detail.contains("no conductor"), "{detail}");
    }

    // The owner's own restart, naming the failure the wait names.
    let key = fixture.snapshot().attention[0].waiting_id.clone();
    fixture
        .applied("restart-1", restart(Some("once more"), Some(&key)))
        .await;
    fixture.settled().await;
    assert_eq!(fixture.status(), ExecutionStatus::Completed);
}

/// The owner writes to the conductor while the step runs: the answer is
/// read back, its proposal waits for the owner as a conductor proposal, and
/// rejecting it settles it for good.
#[tokio::test]
async fn a_chat_answer_and_its_proposal_are_read_back_and_a_rejection_settles_it() {
    let root = TempRoot::new();
    let path = harness_path(&root, &["claude"]);
    let conductor = Conductor::answering(&[(
        "reply",
        json!({
            "decision": "reply",
            "confidence": "high",
            "rationale": "it is waiting",
            "message": "The step is still sleeping.",
            "proposals": [{"kind": "abort", "summary": "Cancel the task"}],
        }),
    )]);
    let mut fixture = Fixture::new(
        &root,
        &root.dir("storage"),
        "pod-1",
        &spec(ONE_STEP, "claude-code"),
        Agent::new(vec![], Reply::Hang),
        conductor,
        &path,
    );
    fixture.begin().await;
    fixture.agent_called(1).await;

    fixture
        .applied(
            "chat-1",
            OperatorCommand::ConductorChat {
                text: "  how is it going?  ".to_string(),
            },
        )
        .await;
    let view = {
        let fixture = &fixture;
        let mut view = None;
        wait_for("the answer", || {
            let read = fixture.conductor_view();
            let ready = read.chat.iter().any(|entry| {
                entry.author == ChatAuthor::Conductor && entry.text == "The step is still sleeping."
            }) && !read.proposals.is_empty();
            view = Some(read);
            ready
        })
        .await;
        view.unwrap()
    };
    assert_eq!(view.chat[0].text, "how is it going?", "the text is trimmed");
    assert!(view.unanswered_chat.is_empty(), "{view:?}");
    let proposal = view.proposals[0].clone();
    assert_eq!(proposal.state, ProposalState::Pending);
    assert!(proposal.in_reply_to.is_some(), "it answers the message");

    let waiting: Vec<_> = fixture
        .snapshot()
        .attention
        .into_iter()
        .filter(|item| item.kind == AttentionKind::ConductorProposal)
        .collect();
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].waiting_id, proposal.proposal_id);

    fixture
        .applied(
            "reject-1",
            OperatorCommand::RejectProposal {
                proposal_id: proposal.proposal_id.clone(),
            },
        )
        .await;
    assert!(fixture.attention_kinds().is_empty());
    assert_eq!(
        fixture.conductor_view().proposals[0].state,
        ProposalState::Rejected
    );

    // A rejected proposal can neither be rejected again nor confirmed.
    for (id, command) in [
        (
            "reject-2",
            OperatorCommand::RejectProposal {
                proposal_id: proposal.proposal_id.clone(),
            },
        ),
        (
            "confirm-1",
            OperatorCommand::ConfirmProposal {
                proposal_id: proposal.proposal_id.clone(),
            },
        ),
    ] {
        fixture.rejected(id, command).await;
    }
    assert_eq!(
        fixture.status(),
        ExecutionStatus::Running,
        "nothing was carried out"
    );
}

const LABELLESS: &str = r#"
name: labelless-task
version: 1
states:
  work:
    action:
      type: agent
      prompt: Do the work.
    choices: [implemented]
    transitions:
      implemented: completed
  completed:
    description: Done.
terminal_states: [completed]
"#;

/// The conductor completed a step with its own choice of an output; the
/// owner takes it back and the step is a problem again.
#[tokio::test]
async fn a_chosen_output_is_undone_and_the_step_is_a_problem_again() {
    let root = TempRoot::new();
    let path = harness_path(&root, &["claude"]);
    let conductor = Conductor::answering(&[(
        "choose",
        json!({
            "decision": "choose_output",
            "label": "implemented",
            "data": {},
            "variables": {},
            "confidence": "high",
            "rationale": "the agent said it finished three units",
        }),
    )]);
    let mut fixture = Fixture::new(
        &root,
        &root.dir("storage"),
        "pod-1",
        &spec(LABELLESS, "claude-code"),
        Agent::new(vec![], Reply::Labelless("Done: three units.")),
        conductor,
        &path,
    );
    fixture.begin().await;
    {
        let fixture = &fixture;
        wait_for("the conductor's choice", || {
            fixture.status() == ExecutionStatus::Completed
        })
        .await;
    }
    fixture.settled().await;

    let view = fixture.conductor_view();
    assert_eq!(view.decisions.len(), 1, "{view:?}");
    let decision = view.decisions[0].clone();
    assert_eq!(decision.kind, "choose_output");
    assert!(decision.undoable && !decision.undone);

    fixture
        .applied(
            "undo-1",
            OperatorCommand::UndoDecision {
                decision_id: decision.decision_id.clone(),
            },
        )
        .await;
    assert_eq!(fixture.status(), ExecutionStatus::InputNeeded);
    let view = fixture.conductor_view();
    assert!(view.decisions[0].undone && !view.decisions[0].undoable);
    assert!(
        view.chat
            .iter()
            .any(|entry| entry.author == ChatAuthor::Operator
                && entry.text.contains("Undid the conductor's choice")),
        "the undo is in the history: {:?}",
        view.chat
    );
    assert!(fixture.attention_kinds().contains(&AttentionKind::Problem));

    let again = fixture
        .rejected(
            "undo-2",
            OperatorCommand::UndoDecision {
                decision_id: decision.decision_id,
            },
        )
        .await;
    assert!(again.contains("already undone"), "{again}");
}

/// The conductor's harness is no longer chosen per task: a choice an older
/// version kept on the storage is not read, and a `SetConductorHarness`
/// queued before the update is settled without changing anything.
#[tokio::test]
async fn a_per_task_conductor_harness_from_before_is_ignored() {
    let root = TempRoot::new();
    let storage = root.dir("storage");
    // What an older version wrote when the owner chose `opencode`.
    let remembered = storage.join("narsor/conductor.json");
    fs::create_dir_all(remembered.parent().unwrap()).unwrap();
    fs::write(&remembered, r#"{"version":1,"harness":"opencode"}"#).unwrap();
    let path = harness_path(&root, &["claude", "opencode"]);
    let mut fixture = Fixture::new(
        &root,
        &storage,
        "pod-1",
        &spec(ONE_STEP, "claude-code"),
        Agent::new(vec![], Reply::Hang),
        Conductor::silent(),
        &path,
    );
    fixture.begin().await;

    assert_eq!(
        fixture.conductor_view().harness.as_deref(),
        Some("claude-code")
    );
    let detail = fixture
        .rejected(
            "harness-1",
            OperatorCommand::SetConductorHarness {
                harness: "codex".to_string(),
            },
        )
        .await;
    assert!(detail.contains("no longer chosen per task"), "{detail}");
    assert_eq!(
        fixture.conductor_view().harness.as_deref(),
        Some("claude-code")
    );
    assert_eq!(
        fs::read_to_string(&remembered).unwrap(),
        r#"{"version":1,"harness":"opencode"}"#,
        "the old file is neither read nor removed"
    );
}

// ---------------------------------------------------------------------------
// The default harness running tasks follow (U7)
// ---------------------------------------------------------------------------

/// AE3, R16b: a task created on `codex` follows the installation's default
/// from its first action; a change reaches the next action and the
/// conductor, even when the answer that carries it also carries the input
/// that starts that action.
#[tokio::test]
async fn a_running_task_follows_the_default_harness_from_its_next_action() {
    let root = TempRoot::new();
    let path = harness_path(&root, &["claude", "opencode", "codex"]);
    let mut fixture = Fixture::new(
        &root,
        &root.dir("storage"),
        "pod-1",
        &spec(GATED, "codex"),
        Agent::new(
            vec![Reply::Label("done"), Reply::Label("ok")],
            Reply::Label("done"),
        ),
        Conductor::silent(),
        &path,
    );
    // The first answer of the control plane, before any work.
    fixture
        .session
        .follow_default_harness(available("claude-code"));
    fixture.begin().await;
    fixture.settled().await;
    assert_eq!(fixture.agent.harnesses(), vec!["claude-code"; 2]);

    // One answer: the new default first, then the input it carries.
    fixture
        .session
        .follow_default_harness(available("opencode"));
    assert_eq!(
        fixture.answer_gate("answer-1"),
        InputVerdict::Taken { first: true }
    );
    fixture.settled().await;

    assert_eq!(fixture.status(), ExecutionStatus::Completed);
    assert_eq!(
        fixture.agent.harnesses(),
        vec!["claude-code", "claude-code", "opencode"]
    );
    let view = fixture.conductor_view();
    assert!(view.available, "{view:?}");
    assert_eq!(view.harness.as_deref(), Some("opencode"));
    assert_eq!(fixture.session.harness_unavailable, None);
    assert_eq!(
        fixture
            .session
            .storage
            .applied_harness()
            .unwrap()
            .as_deref(),
        Some("opencode")
    );
}

/// The harness a runner switched to is where the next incarnation on the
/// same storage starts, whatever the launch spec says; a storage that never
/// switched starts on the spec's.
#[test]
fn the_next_incarnation_starts_on_the_harness_switched_to() {
    let root = TempRoot::new();
    let spec = spec(ONE_STEP, "codex");
    let storage = TaskStorage::new(root.dir("storage"));
    storage.prepare().unwrap();
    assert_eq!(starting_harness(&storage, &spec), "codex");

    storage.record_applied_harness("opencode").unwrap();
    assert_eq!(starting_harness(&storage, &spec), "opencode");

    // A record that cannot be read leaves the task's own.
    fs::write(storage.bookkeeping_dir().join("harness.json"), "{").unwrap();
    assert_eq!(starting_harness(&storage, &spec), "codex");
}

const THREE_STEPS: &str = r#"
name: three-steps
version: 1
states:
  a:
    action:
      type: agent
      prompt: First.
    choices: [done]
    transitions:
      done: g1
  g1:
    action:
      type: agent
      prompt: Gate one.
    choices: [ok, rejected]
    transitions:
      ok: b
      rejected: failed
    decision:
      type: human
      message: Go on?
  b:
    action:
      type: agent
      prompt: Second.
    choices: [done]
    transitions:
      done: g2
  g2:
    action:
      type: agent
      prompt: Gate two.
    choices: [ok, rejected]
    transitions:
      ok: c
      rejected: failed
    decision:
      type: human
      message: Go on?
  c:
    action:
      type: agent
      prompt: Third.
    choices: [done]
    transitions:
      done: completed
  completed:
    description: Done.
  failed:
    description: Rejected.
terminal_states:
  completed: completed
  failed: failed
"#;

/// AE6, R16c: a default the backend does not run, or whose binary is not in
/// the task's environment, leaves the next action on the task's harness;
/// the owner sees why, as information (no problem, no failed step), and the
/// reason goes once a default the task can run comes back.
#[tokio::test]
async fn a_default_the_task_cannot_run_keeps_its_harness_and_says_why() {
    let root = TempRoot::new();
    // Only `claude` is on the runner's PATH.
    let path = harness_path(&root, &["claude"]);
    let gate = Reply::Label("ok");
    let done = Reply::Label("done");
    let mut fixture = Fixture::new(
        &root,
        &root.dir("storage"),
        "pod-1",
        &spec(THREE_STEPS, "claude-code"),
        Agent::new(
            vec![done.clone(), gate.clone(), done, gate],
            Reply::Label("done"),
        ),
        Conductor::silent(),
        &path,
    );
    fixture
        .session
        .follow_default_harness(available("claude-code"));
    fixture.begin().await;
    fixture.settled().await;

    // The backend runs only claude-code: the control plane says so.
    fixture
        .session
        .follow_default_harness(Some(TaskDefaultHarness::Unavailable {
            harness: "opencode".to_string(),
            reason: "the backend runs only claude-code".to_string(),
        }));
    let expected = HarnessUnavailable {
        harness: "opencode".to_string(),
        kept: "claude-code".to_string(),
        reason: "the backend runs only claude-code".to_string(),
    };
    assert_eq!(fixture.session.harness_unavailable, Some(expected.clone()));
    assert!(fixture.session.report_forced, "a snapshot must say so");
    let snapshot = fixture.snapshot();
    assert_eq!(snapshot.default_harness_unavailable, Some(expected));
    assert_eq!(snapshot.problem, None, "it is not a problem");
    assert_eq!(fixture.attention_kinds(), vec![AttentionKind::Question]);
    assert_eq!(
        fixture.session.poll_query(None, 0).harness.as_deref(),
        Some("opencode"),
        "the poll names the default turned down, so it is not handed out again"
    );

    fixture.answer_gate("answer-1");
    fixture.settled().await;

    // The backend runs it now, but the binary is not here.
    fixture
        .session
        .follow_default_harness(available("opencode"));
    let unavailable = fixture.session.harness_unavailable.clone().unwrap();
    assert!(
        unavailable.reason.contains("not on PATH"),
        "{unavailable:?}"
    );
    fixture.answer_gate("answer-2");
    fixture.settled().await;

    assert_eq!(fixture.status(), ExecutionStatus::Completed);
    assert_eq!(
        fixture.agent.harnesses(),
        vec!["claude-code"; 5],
        "no step failed or switched"
    );

    // Back to a default the task runs: the reason goes.
    fixture
        .session
        .follow_default_harness(available("claude-code"));
    assert_eq!(fixture.session.harness_unavailable, None);
}

/// A runner that starts on a harness it cannot run has no conductor; once
/// the control plane names one it can, the conductor works.
#[tokio::test]
async fn the_conductor_comes_up_once_the_default_is_a_harness_the_task_can_run() {
    let root = TempRoot::new();
    let path = harness_path(&root, &["claude"]);
    let conductor = Conductor::answering(&[(
        "reply",
        json!({"decision": "reply", "message": "From claude."}),
    )]);
    let mut fixture = Fixture::new(
        &root,
        &root.dir("storage"),
        "pod-1",
        &spec(ONE_STEP, "opencode"),
        Agent::new(vec![], Reply::Hang),
        conductor,
        &path,
    );
    fixture.begin().await;
    assert!(!fixture.snapshot().conductor_available);

    fixture
        .session
        .follow_default_harness(available("claude-code"));
    assert!(fixture.snapshot().conductor_available);
    fixture
        .applied(
            "chat-1",
            OperatorCommand::ConductorChat {
                text: "hello".to_string(),
            },
        )
        .await;
    {
        let fixture = &fixture;
        wait_for("claude's answer", || {
            fixture
                .conductor_view()
                .chat
                .iter()
                .any(|entry| entry.text == "From claude.")
        })
        .await;
    }
    assert_eq!(
        fixture.conductor_view().harness.as_deref(),
        Some("claude-code")
    );
}

// ---------------------------------------------------------------------------
// The task's title (U10)
// ---------------------------------------------------------------------------

const TITLED: &str = r#"
name: titled-task
version: 1
display:
  title: "Przebudowa obslugi zamowien hurtowych"
states:
  work:
    action:
      type: agent
      prompt: Do the work.
    choices: [done]
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states: [completed]
"#;

fn title_calls(fixture: &Fixture) -> usize {
    fixture
        .events()
        .iter()
        .filter(|data| {
            matches!(
                data,
                EventData::ConductorStarted {
                    purpose: ConductorPurpose::GenerateTitle,
                    ..
                }
            )
        })
        .count()
}

fn titling(root: &TempRoot, spec: &TaskLaunchSpec) -> Fixture {
    let path = harness_path(root, &["claude"]);
    Fixture::new(
        root,
        &root.dir("storage"),
        "pod-1",
        spec,
        Agent::new(vec![], Reply::Hang),
        Conductor::answering(&[(
            "title",
            json!({"decision": "title", "title": "Zmiana API zamowien", "confidence": "high"}),
        )]),
        &path,
    )
}

/// A task without a title is named by the conductor from its description,
/// once; the runner reports the title while the step keeps running.
#[tokio::test]
async fn a_task_without_a_title_is_named_by_the_conductor() {
    let root = TempRoot::new();
    let mut fixture = titling(&root, &untitled(TITLED, "claude-code"));
    assert_eq!(fixture.snapshot().generated_title, None);
    fixture.begin().await;

    {
        let session = &mut fixture.session;
        let mut titled = false;
        let deadline = tokio::time::Instant::now() + PATIENCE;
        while !titled {
            assert!(tokio::time::Instant::now() < deadline, "no title came");
            titled = session
                .snapshot(None, false)
                .unwrap()
                .generated_title
                .is_some();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    let snapshot = fixture.snapshot();
    assert_eq!(
        snapshot.generated_title.as_deref(),
        Some("Zmiana API zamowien")
    );
    assert_eq!(snapshot.execution_status, ExecutionStatus::Running);
    assert_eq!(title_calls(&fixture), 1);
    let asked = fixture.conductor.calls_for(ConductorPurpose::GenerateTitle);
    assert!(
        asked[0]
            .request
            .payload
            .to_string()
            .contains("Przebudowa obslugi zamowien hurtowych"),
        "named from the display title: {:?}",
        asked[0].request.payload
    );
}

/// A title the operator typed is never asked for, and never replaced.
#[tokio::test]
async fn a_task_with_an_operator_title_does_not_ask_the_conductor() {
    let root = TempRoot::new();
    let spec = untitled(TITLED, "claude-code")
        .with_labels(Some("NAR-12".to_string()), Some("Moj tytul".to_string()));
    let mut fixture = titling(&root, &spec);
    fixture.begin().await;
    fixture.agent_called(1).await;

    assert_eq!(title_calls(&fixture), 0);
    assert!(fixture
        .conductor
        .calls_for(ConductorPurpose::GenerateTitle)
        .is_empty());
    assert_eq!(fixture.snapshot().generated_title, None);
    assert_eq!(
        fixture.session.engine.task_title().as_deref(),
        Some("Moj tytul")
    );
    assert_eq!(fixture.session.engine.task_ticket(), Some("NAR-12"));
}

// ---------------------------------------------------------------------------
// After a stop: what the next incarnation finds (R8, R13)
// ---------------------------------------------------------------------------

/// A runner that reached `GATED`'s gate and was lost there, without closing
/// its session; the gate it waited at.
async fn lost_at_the_gate(root: &TempRoot, storage: &Path) -> String {
    // No conductor: nothing of it keeps working after the loss.
    let path = harness_path(root, &[]);
    let mut fixture = Fixture::new(
        root,
        storage,
        "pod-1",
        &spec(GATED, "claude-code"),
        Agent::new(vec![Reply::Label("done"), Reply::Label("ok")], Reply::Fail),
        Conductor::silent(),
        &path,
    );
    fixture.begin().await;
    fixture.settled().await;
    fixture
        .session
        .pending_input_id()
        .unwrap()
        .expect("the gate")
}

/// R8: after the resume policy relaunched a lost runner, the task waits at
/// the very gate it waited at before, and nothing ran again.
#[tokio::test]
async fn an_automatic_resume_waits_at_the_same_gate() {
    let root = TempRoot::new();
    let storage = root.dir("storage");
    let gate = lost_at_the_gate(&root, &storage).await;
    let path = harness_path(&root, &["claude"]);
    let agent = Agent::new(vec![], Reply::Fail);
    let mut fixture = Fixture::new(
        &root,
        &storage,
        "pod-2",
        &spec(GATED, "claude-code"),
        agent,
        Conductor::silent(),
        &path,
    );
    fixture.session.link.launch = Some(RunnerLaunchView {
        incarnation: "pod-2".to_string(),
        launch_generation: 2,
        authorized_by: Some(OwnerCommand::Resume),
        authorized_by_actor: Some(CommandActor::Policy),
        resume_message: None,
    });

    assert_eq!(fixture.begin().await, None, "the policy decided");
    fixture.settled().await;

    assert_eq!(
        fixture.session.pending_input_id().unwrap(),
        Some(gate.clone())
    );
    let snapshot = fixture.snapshot();
    assert_eq!(snapshot.pending_input.map(|pending| pending.id), Some(gate));
    assert!(fixture.agent.calls().is_empty(), "nothing ran again");
}

/// R13: the same incarnation started again on a storage left by an
/// unexpected stop has nobody's decision, and changes nothing.
#[tokio::test]
async fn the_same_incarnation_after_an_unexpected_stop_waits_for_a_decision() {
    let root = TempRoot::new();
    let storage = root.dir("storage");
    let gate = lost_at_the_gate(&root, &storage).await;
    let path = harness_path(&root, &["claude"]);
    let mut fixture = Fixture::new(
        &root,
        &storage,
        "pod-1",
        &spec(GATED, "claude-code"),
        Agent::new(vec![], Reply::Fail),
        Conductor::silent(),
        &path,
    );
    fixture.session.link.launch = Some(RunnerLaunchView {
        incarnation: "pod-1".to_string(),
        launch_generation: 1,
        authorized_by: Some(OwnerCommand::Start),
        authorized_by_actor: Some(CommandActor::Owner),
        resume_message: None,
    });

    let exit = fixture.begin().await;

    let Some(RunnerExit::Problem(problem)) = exit else {
        panic!("a problem: {exit:?}");
    };
    assert_eq!(problem.code, PROBLEM_OWNER_DECISION);
    assert!(fixture.session.pass.is_none());
    assert_eq!(fixture.status(), ExecutionStatus::InputNeeded);
    assert_eq!(fixture.session.pending_input_id().unwrap(), Some(gate));
    assert!(
        fixture
            .session
            .storage
            .session()
            .unwrap()
            .unwrap()
            .closed
            .is_none(),
        "the lost session stays open for the next decision"
    );
}

/// A runner on `TWO_STEPS` paused cleanly between its steps; the storage
/// it leaves.
async fn paused_between_steps(root: &TempRoot) -> PathBuf {
    let storage = root.dir("storage");
    // No conductor: nothing of it keeps working after the stop.
    let path = harness_path(root, &[]);
    let gate = Arc::new(Notify::new());
    let mut fixture = Fixture::new(
        root,
        &storage,
        "pod-1",
        &spec(TWO_STEPS, "claude-code"),
        Agent::new(vec![Reply::After(Arc::clone(&gate), "done")], Reply::Fail),
        Conductor::silent(),
        &path,
    );
    fixture.begin().await;
    fixture.agent_called(1).await;
    // The pause directive arrives while the first step runs.
    fixture.session.engine.pause(&fixture.execution_id());
    gate.notify_one();
    fixture.settled().await;
    assert_eq!(fixture.status(), ExecutionStatus::Paused);
    fixture
        .session
        .close_session("paused at a safe point")
        .unwrap();
    storage
}

/// A runner resumed after a pause, with its launch view.
fn resumed(root: &TempRoot, storage: &Path, launch: Option<RunnerLaunchView>) -> Fixture {
    let path = harness_path(root, &["claude"]);
    let mut fixture = Fixture::new(
        root,
        storage,
        "pod-2",
        &spec(TWO_STEPS, "claude-code"),
        Agent::new(vec![], Reply::Label("done")),
        Conductor::silent(),
        &path,
    );
    fixture.session.link.launch = launch;
    fixture
}

fn launch_with_comment(incarnation: &str) -> RunnerLaunchView {
    RunnerLaunchView {
        incarnation: incarnation.to_string(),
        launch_generation: 2,
        authorized_by: Some(OwnerCommand::Resume),
        authorized_by_actor: Some(CommandActor::Owner),
        resume_message: Some(ResumeMessage {
            key: "c-1".to_string(),
            text: "Mind the flaky test.".to_string(),
        }),
    }
}

/// The comment the owner left on the pause reaches the next step once, and
/// the storage remembers it gave it.
#[tokio::test]
async fn the_comment_on_a_pause_reaches_the_next_step_once() {
    let root = TempRoot::new();
    let storage = paused_between_steps(&root).await;
    let mut fixture = resumed(&root, &storage, Some(launch_with_comment("pod-2")));

    assert_eq!(fixture.begin().await, None);
    fixture.settled().await;

    assert_eq!(fixture.status(), ExecutionStatus::Completed);
    let calls = fixture.agent.calls();
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].prompt.contains("Mind the flaky test."),
        "{}",
        calls[0].prompt
    );
    assert_eq!(
        fixture.session.taken_resume_message_key().as_deref(),
        Some("c-1"),
        "the next poll tells the control plane to drop it"
    );
    assert_eq!(fixture.session.untaken_resume_message().unwrap(), None);
}

/// A runner resumed while the control plane is away does not wait for the
/// comment: the work goes on without it, and nothing marks it as given, so
/// the record keeps it for a later launch.
#[tokio::test]
async fn without_the_control_plane_a_resume_goes_on_without_the_comment() {
    let root = TempRoot::new();
    let storage = paused_between_steps(&root).await;
    let mut fixture = resumed(&root, &storage, None);

    assert_eq!(fixture.begin().await, None);
    fixture.settled().await;

    assert_eq!(fixture.status(), ExecutionStatus::Completed);
    let calls = fixture.agent.calls();
    assert_eq!(calls.len(), 1);
    assert!(!calls[0].prompt.contains("Mind the flaky test."));
    // The launch view arrives later: the comment is still to be given.
    fixture.session.link.launch = Some(launch_with_comment("pod-2"));
    assert_eq!(fixture.session.taken_resume_message_key(), None);
    assert!(fixture.session.untaken_resume_message().unwrap().is_some());
}
