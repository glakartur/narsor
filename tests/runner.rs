//! Integration tests for the portable, resumable task runner (U3).
//!
//! Every runner here is the real `narsor runner` binary in a child process,
//! talking over real HTTP to the real [`ProtocolService`] served on
//! loopback — so a lost workload is a killed process group, a lost control
//! plane is a port nobody listens on, and a resumed workload is a second
//! process on the same task storage. The controller's side (enrollment,
//! lease, reports, runner credentials) and the owner's side (commands,
//! input, conversation pages) are driven in-process through the same
//! service. Workspaces are cloned from a local bare repository; agent steps
//! run a fake `claude` put first on the runner's `PATH`.

mod environment_support;

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Router;
use chrono::Utc;
use narsor::controller_protocol::http::runner_routes;
use narsor::controller_protocol::{
    AcquireLeaseRequest, ConductorReadQuery, ConductorReadResponse, ConversationPageQuery,
    CreateEnrollmentRequest, EnrollRequest, OwnerCommandRequest, ProtocolService, ReportRequest,
    RunnerCredentialRequest, SubmitInputRequest, SubmitOperatorCommandRequest, LOCAL_OWNER,
};
use narsor::domain::{
    BackendId, EventData, ExecutionStatus, GitIdentity, ResolvedEnvironment, ResourceProfile,
    SecretDelivery, SecretReference, TaskId, TaskLaunchSpec, TaskWorkspace,
};
use narsor::engine::HumanInputDelivery;
use narsor::environment::HarnessDefaults;
use narsor::orchestration::{
    AttentionKind, ControlPlane, NewTask, Observation, ObservedSummary, OperatorCommand,
    OperatorCommandRequest, OperatorCommandStatus, OwnerCommand, OwnerRef, QueuedOperatorCommand,
    TaskEventKind, TaskRecord,
};
use narsor::persistence::paths::StatePaths;
use narsor::persistence::ExecutionStore;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const BACKEND: &str = "k8s";

/// A temporary directory that deletes itself when it goes out of scope.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-runner-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn dir(&self, name: &str) -> PathBuf {
        let path = self.path.join(name);
        fs::create_dir_all(&path).expect("create directory");
        path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// The runner contract refuses root (R22); a suite run as root cannot
/// exercise it and says so instead of failing.
fn running_as_root() -> bool {
    // SAFETY: `geteuid` has no preconditions.
    let root = unsafe { libc::geteuid() } == 0;
    if root {
        eprintln!("skipped: the runner refuses to run as root");
    }
    root
}

/// A control plane, a controller holding the backend's lease, and the
/// loopback address the runner routes are (or will be) served on.
struct Fixture {
    root: TempRoot,
    service: Arc<ProtocolService>,
    controller: narsor::controller_protocol::ControllerIdentity,
    epoch: u64,
    addr: SocketAddr,
    server: Option<tokio::task::JoinHandle<()>>,
    runners: Vec<Child>,
    /// While set, the served routes refuse every operator command
    /// acknowledgement, as a network that drops the answers would.
    block_command_acks: Arc<AtomicBool>,
    /// The installation's default harness running tasks follow (U7).
    default: Arc<LiveDefault>,
}

/// A default harness a test changes by hand, the way the daemon's selector
/// changes — announcing each change to the waiting runner polls the same
/// way; `None` is a control plane with no default to hand out.
#[derive(Debug, Default)]
struct LiveDefault {
    harness: std::sync::Mutex<Option<String>>,
    changes: tokio::sync::watch::Sender<u64>,
}

impl LiveDefault {
    fn set(&self, harness: Option<&str>) {
        *self.harness.lock().unwrap() = harness.map(str::to_string);
        self.changes.send_modify(|revision| *revision += 1);
    }
}

impl HarnessDefaults for LiveDefault {
    fn supported(&self) -> Vec<String> {
        vec!["claude-code".into(), "codex".into(), "opencode".into()]
    }

    fn default_harness(&self) -> String {
        self.live_default()
            .unwrap_or_else(|| "claude-code".to_string())
    }

    fn live_default(&self) -> Option<String> {
        self.harness.lock().unwrap().clone()
    }

    fn live_changes(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        Some(self.changes.subscribe())
    }
}

impl Fixture {
    /// A fixture whose runner routes are served right away.
    async fn served() -> Self {
        // Bind before anyone learns the address: reserving a port and
        // re-binding it later lets a parallel test grab it in between.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the control plane");
        let mut fixture = Self::unserved();
        fixture.addr = listener.local_addr().expect("address");
        fixture.serve_on(listener);
        fixture
    }

    /// A fixture whose address nobody listens on yet: a control plane the
    /// runner cannot reach.
    fn unserved() -> Self {
        let root = TempRoot::new();
        let state = root.dir("control");
        let control = ControlPlane::new(StatePaths::new(&state));
        let default = Arc::new(LiveDefault::default());
        let service = Arc::new(ProtocolService::new(
            control,
            environment_support::seeded_catalog_resolver_with(
                &state,
                Arc::clone(&default) as Arc<dyn HarnessDefaults>,
            ),
        ));
        let code = service
            .create_enrollment(BACKEND, CreateEnrollmentRequest::default())
            .expect("enrollment");
        let enrolled = service
            .enroll(EnrollRequest::signed(
                &code.enrollment_code,
                &narsor::auth::ControllerKey::generate(),
            ))
            .expect("enroll");
        // The identity the controller gate would prove; this fixture calls
        // the service in-process, past the HTTP gate.
        let controller = narsor::controller_protocol::ControllerIdentity {
            controller_id: enrolled.controller_id,
            backend_id: enrolled.backend_id,
        };
        let epoch = service
            .acquire_lease(
                &controller,
                AcquireLeaseRequest {
                    ttl_secs: Some(600),
                },
            )
            .expect("lease")
            .epoch;
        let reserved = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve a port");
        let addr = reserved.local_addr().expect("address");
        drop(reserved);
        Self {
            root,
            service,
            controller,
            epoch,
            addr,
            server: None,
            runners: Vec::new(),
            block_command_acks: Arc::new(AtomicBool::new(false)),
            default,
        }
    }

    /// Changes the installation's default harness, which tells the waiting
    /// runners, as the daemon's selector does.
    fn set_default(&self, harness: Option<&str>) {
        self.default.set(harness);
    }

    /// Lets the task backend run only `harnesses` (all of them when empty).
    fn restrict_harnesses(&self, harnesses: &[&str]) {
        environment_support::restrict_harnesses(
            &self.root.path.join("control"),
            BACKEND,
            harnesses,
        );
    }

    /// Starts serving the runner routes on the fixture's address.
    async fn serve(&mut self) {
        let listener = tokio::net::TcpListener::bind(self.addr)
            .await
            .expect("bind the control plane");
        self.serve_on(listener);
    }

    fn serve_on(&mut self, listener: tokio::net::TcpListener) {
        let block = Arc::clone(&self.block_command_acks);
        let router: Router =
            runner_routes(Arc::clone(&self.service)).layer(axum::middleware::from_fn(
                move |request: axum::extract::Request, next: axum::middleware::Next| {
                    let block = Arc::clone(&block);
                    async move {
                        let ack = request.uri().path().starts_with("/runner/v1/commands/");
                        if ack && block.load(Ordering::SeqCst) {
                            return StatusCode::SERVICE_UNAVAILABLE.into_response();
                        }
                        next.run(request).await
                    }
                },
            ));
        self.server = Some(tokio::spawn(async move {
            axum::serve(listener, router).await.expect("serve");
        }));
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn owner(&self) -> OwnerRef {
        OwnerRef::new(LOCAL_OWNER)
    }

    fn create(&self, task: &str, spec: TaskLaunchSpec) {
        self.service
            .control_plane()
            .create_task(NewTask::new(
                TaskId::parse(task).unwrap(),
                self.owner(),
                BackendId::parse(BACKEND).unwrap(),
                spec,
            ))
            .expect("create the task");
    }

    fn record(&self, task: &str) -> TaskRecord {
        self.service
            .control_plane()
            .task(&TaskId::parse(task).unwrap())
            .expect("task record")
    }

    fn report(&self, task: &str, observation: Observation) {
        let generation = self.record(task).generation;
        self.service
            .report(
                &self.controller,
                ReportRequest {
                    task_id: task.to_string(),
                    lease_epoch: self.epoch,
                    desired_generation: generation,
                    observation,
                    resource: None,
                    summary: ObservedSummary::default(),
                    observed_at: Utc::now(),
                },
            )
            .unwrap_or_else(|error| panic!("report {observation:?}: {error}"));
    }

    fn command(&self, task: &str, command: OwnerCommand, key: &str) {
        self.service
            .command(
                &self.owner(),
                task,
                OwnerCommandRequest {
                    command,
                    idempotency_key: key.to_string(),
                    expected_generation: None,
                },
            )
            .unwrap_or_else(|error| panic!("{command:?}: {error}"));
    }

    /// Issues `incarnation`'s credential for the task's current generation
    /// and writes it to a file, the way a controller mounts it.
    fn credential(&self, task: &str, incarnation: &str) -> PathBuf {
        let issued = self
            .service
            .issue_runner_credential(
                &self.controller,
                task,
                RunnerCredentialRequest {
                    lease_epoch: self.epoch,
                    generation: self.record(task).generation,
                    incarnation: incarnation.to_string(),
                },
            )
            .expect("runner credential");
        let path = self
            .root
            .dir("credentials")
            .join(format!("{task}-{incarnation}"));
        fs::write(&path, issued.credential).expect("write the credential");
        path
    }

    /// Starts `narsor runner` for `task` on `storage`.
    fn spawn(&mut self, launch: &Launch, incarnation: &str, credential: &Path) -> usize {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new(env!("CARGO_BIN_EXE_narsor"));
        command
            .args(["runner", "--task-id", &launch.task])
            .arg("--task-storage")
            .arg(&launch.storage)
            .arg("--launch-spec")
            .arg(&launch.spec_file)
            .args(["--control-plane", &self.url()])
            .arg("--credential-file")
            .arg(credential)
            .args(["--incarnation", incarnation])
            .args(["--poll-wait-secs", "2"])
            .args(["--retry-min-ms", "50", "--retry-max-ms", "300"])
            .args(["--report-interval-ms", "50", "--problem-report-secs", "10"])
            .env("PATH", &launch.path)
            .env("RUST_LOG", "narsor=info")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            // Its own process group, so a "lost workload" takes its
            // children with it, as a deleted Pod would.
            .process_group(0);
        if let Some(secrets) = &launch.secrets {
            command.arg("--secrets-dir").arg(secrets);
        }
        command
            .args(&launch.args)
            .envs(launch.env.iter().map(|(name, value)| (name, value)));
        self.runners
            .push(command.spawn().expect("start the runner process"));
        self.runners.len() - 1
    }

    /// Kills runner `index` and everything it started.
    fn kill(&mut self, index: usize) {
        let child = &mut self.runners[index];
        // SAFETY: signalling a process group we created.
        unsafe {
            libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
        }
        let _ = child.wait();
    }

    fn wait_exit(&mut self, index: usize, within: Duration) -> ExitStatus {
        let deadline = Instant::now() + within;
        loop {
            if let Some(status) = self.runners[index].try_wait().expect("poll the runner") {
                return status;
            }
            assert!(Instant::now() < deadline, "the runner did not exit in time");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for index in 0..self.runners.len() {
            self.kill(index);
        }
        if let Some(server) = self.server.take() {
            server.abort();
        }
    }
}

/// One task's launch: its spec on disk, its storage and the runner's PATH.
struct Launch {
    task: String,
    spec_file: PathBuf,
    storage: PathBuf,
    path: String,
    secrets: Option<PathBuf>,
    /// Extra `narsor runner` arguments.
    args: Vec<String>,
    /// Extra variables of the runner process.
    env: Vec<(String, String)>,
}

impl Launch {
    fn new(fixture: &Fixture, task: &str, spec: &TaskLaunchSpec) -> Self {
        let spec_file = fixture.root.dir("specs").join(format!("{task}.json"));
        fs::write(&spec_file, serde_json::to_string(spec).unwrap()).expect("write the spec");
        Self {
            task: task.to_string(),
            spec_file,
            storage: fixture.root.dir(&format!("storage-{task}")),
            path: std::env::var("PATH").unwrap_or_default(),
            secrets: None,
            args: Vec::new(),
            env: Vec::new(),
        }
    }

    fn with_bin_first(mut self, bin: &Path) -> Self {
        self.path = format!("{}:{}", bin.display(), self.path);
        self
    }

    fn store(&self) -> ExecutionStore {
        ExecutionStore::new(self.storage.join("state"))
    }

    fn execution_id(&self) -> Option<String> {
        self.store()
            .list_execution_ids()
            .ok()
            .and_then(|ids| ids.into_iter().next())
    }

    fn status(&self) -> Option<ExecutionStatus> {
        let id = self.execution_id()?;
        self.store().load_execution(&id).ok().map(|e| e.status)
    }

    fn events(&self) -> Vec<narsor::domain::Event> {
        let id = self.execution_id().expect("an execution");
        self.store().event_log(&id).read_all().expect("events")
    }

    fn home(&self) -> PathBuf {
        self.storage.join("home")
    }

    fn workspace(&self) -> PathBuf {
        self.storage.join("workspace")
    }
}

fn wait_until(within: Duration, what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

async fn wait_until_async(within: Duration, what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Runner Test",
            "-c",
            "user.email=runner@example.com",
        ])
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A bare "remote" with one commit, and that commit's id.
fn origin(root: &TempRoot) -> (PathBuf, String) {
    let work = root.dir("origin-work");
    git(&work, &["-c", "init.defaultBranch=main", "init", "-q"]);
    fs::write(work.join("README.md"), "pinned\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "pinned"]);
    let bare = root.path.join("origin.git");
    let status = Command::new("git")
        .args(["clone", "-q", "--bare"])
        .arg(&work)
        .arg(&bare)
        .status()
        .unwrap();
    assert!(status.success());
    (bare, git(&work, &["rev-parse", "HEAD"]))
}

/// Moves the remote's branch on, as other people's pushes would.
fn advance_origin(root: &TempRoot, bare: &Path) {
    let work = root.dir("origin-work");
    fs::write(work.join("README.md"), "moved on\n").unwrap();
    git(&work, &["commit", "-q", "-am", "moved on"]);
    git(
        &work,
        &["push", "-q", &bare.display().to_string(), "HEAD:main"],
    );
}

/// A fake `claude` that records the HOME it ran with and answers `done`.
fn fake_claude(root: &TempRoot) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = root.dir("bin");
    let script = "#!/bin/sh\n\
        printf '%s\\n' \"$HOME\" >> \"$HOME/claude-homes\"\n\
        printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\
        \"session_id\":\"fake\",\"result\":\"Done.\\n{\\\"output\\\": \\\"done\\\"}\"}'\n";
    let temp = bin.join(".claude.tmp");
    fs::write(&temp, script).unwrap();
    fs::set_permissions(&temp, fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(&temp, bin.join("claude")).unwrap();
    bin
}

fn spec(yaml: &str, workspace: TaskWorkspace, environment: ResolvedEnvironment) -> TaskLaunchSpec {
    let definition = narsor::workflow::load_from_str(yaml).expect("a valid workflow");
    TaskLaunchSpec::new(
        definition.id.clone(),
        definition.version,
        serde_json::json!({}),
        workspace,
        environment,
        ResourceProfile::new(1000, 2048),
    )
    .with_definition(&definition)
    .expect("snapshot the definition")
}

fn runner_status(fixture: &Fixture, task: &str) -> Option<ExecutionStatus> {
    fixture
        .record(task)
        .runner
        .map(|runner| runner.snapshot.execution_status)
}

const GATED_WORKFLOW: &str = r#"
name: remote-task
version: 1
states:
  think:
    action:
      type: agent
      prompt: Think about it.
    outputs: [done]
    transitions:
      done: first
  first:
    action:
      type: script
      command: sh
      args:
        - -c
        - |
          echo first >> steps.log
          printf '%s' "$HOME" > "$HOME/home-seen-by-first"
          printf '{"output": "done"}'
    outputs: [done]
    transitions:
      done: gate
  gate:
    action:
      type: script
      command: sh
      args: [-c, "printf '{\"output\": \"ok\"}'"]
    outputs: [ok, rejected]
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
    outputs: [done]
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
// Pause, resume, input, conversation relay (R12, R17, R39, R41, R42)
// ---------------------------------------------------------------------------

/// A workspace left unpinned at creation is pinned by the admitted clone: the
/// runner resolves the remote's HEAD once, and that commit is the workspace.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unpinned_workspace_is_pinned_by_the_admitted_clone() {
    if running_as_root() {
        return;
    }
    let mut fixture = Fixture::served().await;
    let (bare, head) = origin(&fixture.root);
    let bin = fake_claude(&fixture.root);
    let spec = spec(
        GATED_WORKFLOW,
        TaskWorkspace::Git(GitIdentity {
            repository: Some("demo".to_string()),
            remote_url: bare.display().to_string(),
            commit: String::new(),
            git_ref: None,
            credential: None,
        }),
        ResolvedEnvironment::harness_only("claude-code"),
    );
    fixture.create("task-1", spec.clone());
    fixture.report("task-1", Observation::StartAccepted);
    let launch = Launch::new(&fixture, "task-1", &spec).with_bin_first(&bin);
    let credential = fixture.credential("task-1", "pod-1");
    let _runner = fixture.spawn(&launch, "pod-1", &credential);

    wait_until_async(Duration::from_secs(30), "the gate", || {
        fixture.record("task-1").pending_input_id().is_some()
    })
    .await;

    assert_eq!(git(&launch.workspace(), &["rev-parse", "HEAD"]), head);
    let marker = fs::read_to_string(launch.storage.join("narsor/initialized.json")).unwrap();
    assert!(
        marker.contains(&head),
        "the marker records the commit pinned at admission: {marker}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paused_task_resumes_on_its_own_storage_without_recloning() {
    if running_as_root() {
        return;
    }
    let mut fixture = Fixture::served().await;
    let (bare, pinned) = origin(&fixture.root);
    let bin = fake_claude(&fixture.root);
    let spec = spec(
        GATED_WORKFLOW,
        TaskWorkspace::Git(GitIdentity {
            repository: Some("demo".to_string()),
            remote_url: bare.display().to_string(),
            commit: pinned.clone(),
            git_ref: None,
            credential: None,
        }),
        ResolvedEnvironment::harness_only("claude-code"),
    );
    fixture.create("task-1", spec.clone());
    fixture.report("task-1", Observation::StartAccepted);
    let launch = Launch::new(&fixture, "task-1", &spec).with_bin_first(&bin);
    let credential = fixture.credential("task-1", "pod-1");
    let first = fixture.spawn(&launch, "pod-1", &credential);

    // The runner clones, runs the agent and the script, and stops at the
    // human gate, reporting the request it waits on.
    wait_until_async(Duration::from_secs(30), "the gate", || {
        fixture.record("task-1").pending_input_id().is_some()
    })
    .await;
    let record = fixture.record("task-1");
    let snapshot = &record.runner.as_ref().unwrap().snapshot;
    assert_eq!(snapshot.execution_status, ExecutionStatus::InputNeeded);
    let execution_id = snapshot.execution_cursor.execution_id.clone().unwrap();
    let cursor_at_gate = snapshot.event_cursor;

    // One clone of exactly the pinned commit, initialized once.
    assert_eq!(
        git(&launch.workspace(), &["rev-parse", "HEAD"]),
        pinned,
        "the workspace is the pinned commit"
    );
    assert!(launch.storage.join("narsor/initialized.json").is_file());
    let home = launch.home();
    assert_eq!(
        fs::read_to_string(home.join("home-seen-by-first")).unwrap(),
        home.display().to_string(),
        "actions run with the private HOME on task storage"
    );

    // The owner reads a raw conversation page through the control plane.
    let attempt = match &snapshot.conversation {
        narsor::orchestration::ConversationAvailability::Live { attempts } => attempts
            .first()
            .expect("the agent's attempt")
            .attempt_id
            .clone(),
        other => panic!("conversation not live: {other:?}"),
    };
    let page = fixture
        .service
        .conversation_page(
            &fixture.owner(),
            "task-1",
            &attempt,
            ConversationPageQuery::default(),
        )
        .await
        .expect("a relayed page");
    match page {
        narsor::api::ConversationResponse::Recorded { entries, .. } => {
            assert!(!entries.is_empty())
        }
        other => panic!("no record: {other:?}"),
    }

    // The gate is a wait on the owner, and the raw events come through the
    // same relay from the real runner.
    let summary = fixture
        .service
        .task(&fixture.owner(), "task-1")
        .expect("summary");
    assert_eq!(summary.attention.len(), 1, "{:?}", summary.attention);
    assert_eq!(
        summary.attention[0].kind,
        narsor::orchestration::AttentionKind::Question
    );
    assert_eq!(
        Some(summary.attention[0].waiting_id.as_str()),
        record.pending_input_id()
    );
    match fixture
        .service
        .execution_events(
            &fixture.owner(),
            "task-1",
            narsor::controller_protocol::ExecutionEventsQuery::default(),
        )
        .await
        .expect("events from the runner")
    {
        narsor::controller_protocol::ExecutionEventsResponse::Available {
            events, source, ..
        } => {
            assert_eq!(
                source,
                narsor::controller_protocol::ExecutionEventsSource::Runner
            );
            assert!(events.len() as u64 >= 1);
            assert_eq!(events.last().unwrap().sequence, cursor_at_gate);
        }
        other => panic!("expected events, got {other:?}"),
    }

    // A runner that cannot read its own log answers "unavailable" at once;
    // it does not leave the request pending until the relay gives up.
    let log = launch.store().event_log(&execution_id);
    let gap = log.event_path(cursor_at_gate + 2);
    fs::copy(log.event_path(cursor_at_gate), &gap).unwrap();
    let started = std::time::Instant::now();
    let refused = fixture
        .service
        .execution_events(
            &fixture.owner(),
            "task-1",
            narsor::controller_protocol::ExecutionEventsQuery::default(),
        )
        .await
        .expect_err("the runner cannot read its log");
    assert!(
        refused.to_string().contains("runner_read_failed"),
        "{refused}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "answered at once, not at the relay's timeout: {:?}",
        started.elapsed()
    );
    fs::remove_file(&gap).unwrap();

    // Work in the workspace, and a remote that moves on: neither may be
    // touched by a resume.
    fs::write(launch.workspace().join("agent-work.txt"), "keep me").unwrap();
    advance_origin(&fixture.root, &bare);

    // Pause: the runner reports a safe point and stops on its own.
    fixture.report("task-1", Observation::RunnerReady);
    fixture.command("task-1", OwnerCommand::Pause, "pause-1");
    let status = fixture.wait_exit(first, Duration::from_secs(30));
    assert!(
        status.success(),
        "a paused runner exits cleanly: {status:?}"
    );
    let session: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(launch.storage.join("narsor/session.json")).unwrap(),
    )
    .unwrap();
    assert!(
        !session["closed"].is_null(),
        "the stop is recorded as clean: {session}"
    );
    let record = fixture.record("task-1");
    assert_eq!(
        record.runner.as_ref().unwrap().safe_point_generation,
        Some(record.generation),
        "the safe point was acknowledged for the pause"
    );
    fixture.report("task-1", Observation::PausedAtSafePoint);
    // With its compute gone, the transcript is explicitly unavailable;
    // nothing was replicated to serve it from.
    let refused = fixture
        .service
        .conversation_page(
            &fixture.owner(),
            "task-1",
            &attempt,
            ConversationPageQuery::default(),
        )
        .await
        .expect_err("no live runner to relay from");
    assert!(
        refused.to_string().contains("conversation_unavailable"),
        "{refused}"
    );

    // Resume: a new incarnation on the same storage.
    fixture.command("task-1", OwnerCommand::Resume, "resume-1");
    let credential = fixture.credential("task-1", "pod-2");
    fixture.spawn(&launch, "pod-2", &credential);
    wait_until_async(Duration::from_secs(30), "the resumed runner", || {
        fixture
            .record("task-1")
            .runner
            .is_some_and(|runner| runner.incarnation == "pod-2")
    })
    .await;
    let record = fixture.record("task-1");
    let pending = record
        .pending_input_id()
        .expect("still at the gate")
        .to_string();
    fixture
        .service
        .submit_input(
            &fixture.owner(),
            "task-1",
            SubmitInputRequest {
                pending_input_id: pending,
                idempotency_key: "answer-1".to_string(),
                delivery: HumanInputDelivery::label("ok"),
            },
        )
        .expect("queue the answer");
    wait_until_async(Duration::from_secs(30), "completion", || {
        runner_status(&fixture, "task-1") == Some(ExecutionStatus::Completed)
    })
    .await;

    let record = fixture.record("task-1");
    let snapshot = &record.runner.as_ref().unwrap().snapshot;
    assert_eq!(
        snapshot.execution_cursor.execution_id.as_deref(),
        Some(execution_id.as_str()),
        "the same execution went on"
    );
    assert!(snapshot.event_cursor > cursor_at_gate);
    assert_eq!(
        record.inputs[0].status,
        narsor::orchestration::InputStatus::Consumed
    );

    // Nothing was cloned again and nothing was replayed.
    assert_eq!(git(&launch.workspace(), &["rev-parse", "HEAD"]), pinned);
    assert_eq!(
        fs::read_to_string(launch.workspace().join("agent-work.txt")).unwrap(),
        "keep me"
    );
    assert_eq!(
        fs::read_to_string(launch.workspace().join("steps.log")).unwrap(),
        "first\n"
    );
    // Both agent steps ran with the same private HOME and the harness the
    // task pinned at creation. The conductor, which runs on that harness
    // too, may have been asked beside them (it summarizes the human gate),
    // and it shares the HOME as well.
    let homes = fs::read_to_string(home.join("claude-homes")).unwrap();
    assert!(homes.lines().count() >= 2, "{homes}");
    assert!(
        homes.lines().all(|line| line == home.display().to_string()),
        "{homes}"
    );
    let harnesses: Vec<String> = launch
        .events()
        .into_iter()
        .filter_map(|event| match event.data {
            EventData::AgentSessionStarted { harness, .. } => Some(harness),
            _ => None,
        })
        .collect();
    assert_eq!(harnesses, vec!["claude-code", "claude-code"]);
}

// ---------------------------------------------------------------------------
// Unexpected loss needs the owner (R13)
// ---------------------------------------------------------------------------

const SLOW_WORKFLOW: &str = r#"
name: slow-task
version: 1
states:
  slow:
    action:
      type: script
      command: sh
      args:
        - -c
        - |
          tries=$(cat "$HOME/attempts" 2>/dev/null | wc -l)
          echo try >> "$HOME/attempts"
          if [ "$tries" -eq 0 ]; then sleep 60; fi
          printf '{"output": "done"}'
    outputs: [done]
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states: [completed]
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unexpected_loss_is_recovered_only_after_the_owner_decides() {
    if running_as_root() {
        return;
    }
    let mut fixture = Fixture::served().await;
    let spec = spec(
        SLOW_WORKFLOW,
        TaskWorkspace::None,
        ResolvedEnvironment::harness_only("claude-code"),
    );
    fixture.create("task-2", spec.clone());
    fixture.report("task-2", Observation::StartAccepted);
    let launch = Launch::new(&fixture, "task-2", &spec);
    let credential = fixture.credential("task-2", "pod-1");
    let first = fixture.spawn(&launch, "pod-1", &credential);

    let attempts = launch.home().join("attempts");
    wait_until_async(Duration::from_secs(30), "the first attempt", || {
        attempts.is_file()
    })
    .await;
    wait_until_async(
        Duration::from_secs(10),
        "a report of the running pass",
        || {
            fixture
                .record("task-2")
                .runner
                .is_some_and(|runner| runner.snapshot.event_cursor > 0)
        },
    )
    .await;
    let cursor_before = fixture
        .record("task-2")
        .runner
        .unwrap()
        .snapshot
        .event_cursor;

    // The workload dies in the middle of the action.
    fixture.kill(first);
    fixture.report("task-2", Observation::WorkloadLost { state_known: true });
    let events_before = launch.events().len();

    // The same incarnation coming back is not an owner decision: it
    // changes nothing and says why.
    let again = fixture.spawn(&launch, "pod-1", &credential);
    let status = fixture.wait_exit(again, Duration::from_secs(30));
    assert_eq!(status.code(), Some(4), "owner decision required");
    assert_eq!(launch.status(), Some(ExecutionStatus::Running));
    assert_eq!(
        launch.events().len(),
        events_before,
        "not one event written"
    );
    assert_eq!(fs::read_to_string(&attempts).unwrap().lines().count(), 1);
    let record = fixture.record("task-2");
    let snapshot = &record.runner.as_ref().unwrap().snapshot;
    assert_eq!(
        snapshot
            .problem
            .as_ref()
            .map(|problem| problem.code.as_str()),
        Some("owner_decision_required")
    );
    assert!(
        snapshot.event_cursor >= cursor_before,
        "the cursor never goes back"
    );

    // The owner resumes: a new incarnation reconciles and runs the
    // interrupted action again, once.
    fixture.command("task-2", OwnerCommand::Resume, "resume-after-loss");
    let credential = fixture.credential("task-2", "pod-2");
    fixture.spawn(&launch, "pod-2", &credential);
    wait_until_async(Duration::from_secs(30), "the recovered run", || {
        runner_status(&fixture, "task-2") == Some(ExecutionStatus::Completed)
    })
    .await;
    assert_eq!(fs::read_to_string(&attempts).unwrap().lines().count(), 2);
    let events = launch.events();
    let starts = events
        .iter()
        .filter(|event| matches!(event.data, EventData::ActionStarted { .. }))
        .count();
    assert_eq!(starts, 2, "the interrupted action ran exactly once more");
    assert!(events.iter().any(|event| matches!(
        &event.data,
        EventData::Error { message, .. } if message.contains("will not be replayed")
    )));
}

/// The resume policy's resume authorizes the recovery like the owner's: the
/// new incarnation reconciles, runs the interrupted action again once, and
/// the history says the resume was automatic (R1, R2, AE2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_automatic_resume_recovers_the_interrupted_action_and_says_so() {
    if running_as_root() {
        return;
    }
    let mut fixture = Fixture::served().await;
    let spec = spec(
        SLOW_WORKFLOW,
        TaskWorkspace::None,
        ResolvedEnvironment::harness_only("claude-code"),
    );
    fixture.create("task-2", spec.clone());
    fixture.report("task-2", Observation::StartAccepted);
    let launch = Launch::new(&fixture, "task-2", &spec);
    let credential = fixture.credential("task-2", "pod-1");
    let first = fixture.spawn(&launch, "pod-1", &credential);
    let attempts = launch.home().join("attempts");
    wait_until_async(Duration::from_secs(30), "the first attempt", || {
        attempts.is_file()
    })
    .await;

    fixture.kill(first);
    fixture.report("task-2", Observation::WorkloadLost { state_known: true });
    let pass = fixture.service.auto_resume_pass();
    assert_eq!(pass.resumed, vec![TaskId::parse("task-2").unwrap()]);
    assert!(matches!(
        fixture.record("task-2").last_event.kind,
        TaskEventKind::AutoResumed { attempt: 1, .. }
    ));

    let credential = fixture.credential("task-2", "pod-2");
    fixture.spawn(&launch, "pod-2", &credential);
    wait_until_async(Duration::from_secs(60), "the recovered run", || {
        runner_status(&fixture, "task-2") == Some(ExecutionStatus::Completed)
    })
    .await;
    assert_eq!(fs::read_to_string(&attempts).unwrap().lines().count(), 2);
    let record = fixture.record("task-2");
    assert_eq!(
        record
            .runner
            .unwrap()
            .snapshot
            .problem
            .map(|problem| problem.code),
        None,
        "no owner decision was needed"
    );
    let events = launch.events();
    let starts = events
        .iter()
        .filter(|event| matches!(event.data, EventData::ActionStarted { .. }))
        .count();
    assert_eq!(starts, 2, "the interrupted action ran exactly once more");
    // A script step leaves the recovery's note unread, where the next
    // agent attempt would find it.
    let execution = launch
        .store()
        .load_execution(&launch.execution_id().unwrap())
        .unwrap();
    let note = execution.metadata.to_string();
    assert!(
        note.contains("resumed automatically after workload loss"),
        "the automatic resume is noted: {note}"
    );
    assert!(!note.contains("owner resume"), "{note}");
}

/// A task waiting for an approval when its workload was lost waits for the
/// same approval after the automatic resume: the owner's draft keeps its
/// place (R8).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_automatically_resumed_task_waits_for_the_same_approval() {
    if running_as_root() {
        return;
    }
    let mut fixture = Fixture::served().await;
    let (bare, pinned) = origin(&fixture.root);
    let bin = fake_claude(&fixture.root);
    let spec = spec(
        GATED_WORKFLOW,
        TaskWorkspace::Git(GitIdentity {
            repository: Some("demo".to_string()),
            remote_url: bare.display().to_string(),
            commit: pinned,
            git_ref: None,
            credential: None,
        }),
        ResolvedEnvironment::harness_only("claude-code"),
    );
    fixture.create("task-1", spec.clone());
    fixture.report("task-1", Observation::StartAccepted);
    let launch = Launch::new(&fixture, "task-1", &spec).with_bin_first(&bin);
    let credential = fixture.credential("task-1", "pod-1");
    let first = fixture.spawn(&launch, "pod-1", &credential);
    let waiting = |fixture: &Fixture| -> Vec<String> {
        fixture
            .service
            .task(&fixture.owner(), "task-1")
            .expect("summary")
            .attention
            .into_iter()
            .map(|item| item.waiting_id)
            .collect()
    };
    wait_until_async(Duration::from_secs(30), "the gate", || {
        !waiting(&fixture).is_empty()
    })
    .await;
    let before = waiting(&fixture);

    fixture.kill(first);
    fixture.report("task-1", Observation::WorkloadLost { state_known: true });
    assert_eq!(fixture.service.auto_resume_pass().resumed.len(), 1);
    let generation = fixture.record("task-1").generation;
    let credential = fixture.credential("task-1", "pod-2");
    fixture.spawn(&launch, "pod-2", &credential);
    wait_until_async(
        Duration::from_secs(60),
        "the new runner at the gate",
        || {
            fixture.record("task-1").runner.is_some_and(|runner| {
                runner.incarnation == "pod-2"
                    && runner.launch_generation == generation
                    && runner.snapshot.pending_input.is_some()
            })
        },
    )
    .await;
    assert_eq!(waiting(&fixture), before, "the same wait, by the same id");
}

// ---------------------------------------------------------------------------
// Offline progress (R5, R14)
// ---------------------------------------------------------------------------

const TWO_STEP_WORKFLOW: &str = r#"
name: offline-task
version: 1
states:
  one:
    action:
      type: script
      command: sh
      args: [-c, "echo one >> \"$HOME/steps\"; printf '{\"output\": \"done\"}'"]
    outputs: [done]
    transitions:
      done: two
  two:
    action:
      type: script
      command: sh
      args: [-c, "echo two >> \"$HOME/steps\"; printf '{\"output\": \"done\"}'"]
    outputs: [done]
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states: [completed]
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_offline_runner_keeps_working_and_reports_the_latest_cursor_later() {
    if running_as_root() {
        return;
    }
    let mut fixture = Fixture::unserved();
    let spec = spec(
        TWO_STEP_WORKFLOW,
        TaskWorkspace::None,
        ResolvedEnvironment::harness_only("claude-code"),
    );
    fixture.create("task-3", spec.clone());
    fixture.report("task-3", Observation::StartAccepted);
    let launch = Launch::new(&fixture, "task-3", &spec);
    let credential = fixture.credential("task-3", "pod-1");
    fixture.spawn(&launch, "pod-1", &credential);

    wait_until(Duration::from_secs(30), "offline completion", || {
        launch.status() == Some(ExecutionStatus::Completed)
    });
    assert!(
        fixture.record("task-3").runner.is_none(),
        "nothing reported yet"
    );

    fixture.serve().await;
    let last = launch.events().last().unwrap().sequence;
    wait_until_async(Duration::from_secs(30), "the reconciled report", || {
        fixture.record("task-3").runner.is_some_and(|runner| {
            runner.snapshot.execution_status == ExecutionStatus::Completed
                && runner.snapshot.event_cursor == last
        })
    })
    .await;
    assert_eq!(
        fs::read_to_string(launch.home().join("steps")).unwrap(),
        "one\ntwo\n",
        "no step ran twice"
    );
}

const DISPLAY_WORKFLOW: &str = r#"
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
      type: script
      command: sh
      args:
        - -c
        - |
          printf '{"output": "done", "variables": {"pr_number": 42, "pr_url": "https://git.example/pr/42"}}'
    outputs: [done]
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states: [completed]
"#;

/// The runner renders `display` against the variables its steps set and
/// reports it; the control plane serves that rendering.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_runner_reports_its_display_rendered_from_context_variables() {
    if running_as_root() {
        return;
    }
    let mut fixture = Fixture::served().await;
    let spec = spec(
        DISPLAY_WORKFLOW,
        TaskWorkspace::None,
        ResolvedEnvironment::harness_only("claude-code"),
    );
    fixture.create("task-d", spec.clone());
    fixture.report("task-d", Observation::StartAccepted);
    let launch = Launch::new(&fixture, "task-d", &spec);
    let credential = fixture.credential("task-d", "pod-1");
    fixture.spawn(&launch, "pod-1", &credential);

    wait_until_async(Duration::from_secs(30), "the display report", || {
        fixture
            .record("task-d")
            .runner
            .and_then(|runner| runner.snapshot.display)
            .is_some_and(|display| display.links[0].url.is_some())
    })
    .await;

    let display = fixture
        .record("task-d")
        .runner
        .and_then(|runner| runner.snapshot.display)
        .unwrap();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_base_image_without_git_fails_before_any_work() {
    if running_as_root() {
        return;
    }
    let mut fixture = Fixture::served().await;
    let (bare, pinned) = origin(&fixture.root);
    let spec = spec(
        TWO_STEP_WORKFLOW,
        TaskWorkspace::Git(GitIdentity {
            repository: None,
            remote_url: bare.display().to_string(),
            commit: pinned,
            git_ref: None,
            credential: None,
        }),
        ResolvedEnvironment::harness_only("claude-code"),
    );
    fixture.create("task-4", spec.clone());
    fixture.report("task-4", Observation::StartAccepted);

    // An "image" whose PATH has every basic tool but git.
    let bin = fixture.root.dir("no-git-bin");
    for tool in ["cat", "cp", "env", "grep", "ls", "mkdir", "mv", "rm", "sed"] {
        let found = Command::new("sh")
            .args(["-c", &format!("command -v {tool}")])
            .output()
            .unwrap();
        let target = String::from_utf8_lossy(&found.stdout).trim().to_string();
        std::os::unix::fs::symlink(target, bin.join(tool)).unwrap();
    }
    let mut launch = Launch::new(&fixture, "task-4", &spec);
    launch.path = bin.display().to_string();
    let credential = fixture.credential("task-4", "pod-1");
    let runner = fixture.spawn(&launch, "pod-1", &credential);

    let status = fixture.wait_exit(runner, Duration::from_secs(30));
    assert_eq!(status.code(), Some(3), "a reported problem");
    let record = fixture.record("task-4");
    let problem = record
        .runner
        .and_then(|runner| runner.snapshot.problem)
        .expect("the problem reached the control plane");
    assert_eq!(problem.code, "base_image_contract");
    assert!(problem.message.contains("git"), "{}", problem.message);
    let waiting = fixture
        .service
        .task(&fixture.owner(), "task-4")
        .expect("summary")
        .attention;
    assert_eq!(waiting.len(), 1);
    assert_eq!(
        waiting[0].kind,
        narsor::orchestration::AttentionKind::Problem
    );
    assert_eq!(waiting[0].waiting_id, "base_image_contract");
    assert!(!launch.workspace().exists(), "nothing was cloned");
    assert!(launch.execution_id().is_none(), "no work started");
}

// ---------------------------------------------------------------------------
// The HOME a task's tools see
// ---------------------------------------------------------------------------

const HOME_WORKFLOW: &str = r#"
name: home-task
version: 1
states:
  look:
    action:
      type: script
      command: sh
      args:
        - -c
        - |
          {
            printf 'home=%s\n' "$HOME"
            printf 'codex=%s\n' "$CODEX_HOME"
            printf 'claude=%s\n' "$CLAUDE_CONFIG_DIR"
            printf 'anthropic=%s\n' "${ANTHROPIC_API_KEY-unset}"
            printf 'openai=%s\n' "${OPENAI_API_KEY-unset}"
          } > "$NARSOR_OUTBOX_DIR/../seen"
          printf '{"output": "done"}'
    outputs: [done]
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states: [completed]
"#;

/// Runs [`HOME_WORKFLOW`] with the HOME `host_home`, the extra runner
/// `args`, and API keys in the runner's environment; returns what the
/// script saw and the private HOME of its storage.
async fn seen_by_the_tools(task: &str, host_home: &Path, args: &[&str]) -> (String, PathBuf) {
    let mut fixture = Fixture::served().await;
    let spec = spec(
        HOME_WORKFLOW,
        TaskWorkspace::None,
        ResolvedEnvironment::harness_only("codex"),
    );
    fixture.create(task, spec.clone());
    fixture.report(task, Observation::StartAccepted);
    let mut launch = Launch::new(&fixture, task, &spec);
    launch.args = args.iter().map(|arg| arg.to_string()).collect();
    launch.env = vec![
        ("HOME".into(), host_home.display().to_string()),
        ("ANTHROPIC_API_KEY".into(), "sk-ant".into()),
        ("OPENAI_API_KEY".into(), "sk-oai".into()),
    ];
    let credential = fixture.credential(task, "pod-1");
    fixture.spawn(&launch, "pod-1", &credential);

    wait_until_async(Duration::from_secs(30), "completion", || {
        runner_status(&fixture, task) == Some(ExecutionStatus::Completed)
    })
    .await;
    let seen = fs::read_to_string(launch.storage.join("seen")).expect("the script ran");
    (seen, launch.home())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_home_runs_the_tools_with_the_hosts_logins_and_no_api_keys() {
    if running_as_root() {
        return;
    }
    let root = TempRoot::new();
    let host_home = root.dir("host-home");
    let (seen, _) = seen_by_the_tools(
        "task-home-host",
        &host_home,
        &[
            "--home",
            "host",
            "--codex-home",
            "/srv/codex",
            "--claude-config-dir",
            "/srv/claude",
        ],
    )
    .await;
    assert_eq!(
        seen,
        format!(
            "home={}\ncodex=/srv/codex\nclaude=/srv/claude\nanthropic=unset\nopenai=unset\n",
            host_home.display()
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_private_home_is_the_default_and_drops_inherited_api_keys_too() {
    if running_as_root() {
        return;
    }
    let root = TempRoot::new();
    let host_home = root.dir("host-home");
    let (seen, private_home) = seen_by_the_tools("task-home-private", &host_home, &[]).await;
    assert_eq!(
        seen,
        format!(
            "home={}\ncodex=\nclaude=\nanthropic=unset\nopenai=unset\n",
            private_home.display()
        )
    );
}

// ---------------------------------------------------------------------------
// Secrets by reference (R19)
// ---------------------------------------------------------------------------

const SECRET_WORKFLOW: &str = r#"
name: secret-task
version: 1
states:
  check:
    action:
      type: script
      command: sh
      args:
        - -c
        - |
          printf '%s' "$GH_TOKEN" | sha256sum > "$HOME/token.sha256"
          sha256sum < "$HOME/.config/gh/hosts.yml" > "$HOME/hosts.sha256"
          printf '{"output": "done"}'
    outputs: [done]
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states: [completed]
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secrets_reach_their_tools_without_being_copied_onto_task_storage() {
    if running_as_root() {
        return;
    }
    let mut fixture = Fixture::served().await;
    let secrets = fixture.root.dir("secrets");
    fs::write(secrets.join("github-token"), "tok-123-secret\n").unwrap();
    fs::write(secrets.join("gh-hosts"), "hosts-secret-xyz").unwrap();
    let mut environment = ResolvedEnvironment::harness_only("claude-code");
    environment.secrets = vec![
        SecretReference {
            backend: BackendId::parse(BACKEND).unwrap(),
            name: "github-token".to_string(),
            tool: "gh".to_string(),
            key: "narsor-github/token".to_string(),
            delivery: SecretDelivery::Env("GH_TOKEN".to_string()),
        },
        SecretReference {
            backend: BackendId::parse(BACKEND).unwrap(),
            name: "gh-hosts".to_string(),
            tool: "gh".to_string(),
            key: "narsor-github/hosts".to_string(),
            delivery: SecretDelivery::File(".config/gh/hosts.yml".to_string()),
        },
    ];
    let spec = spec(SECRET_WORKFLOW, TaskWorkspace::None, environment.sealed());
    fixture.create("task-5", spec.clone());
    fixture.report("task-5", Observation::StartAccepted);
    let mut launch = Launch::new(&fixture, "task-5", &spec);
    launch.secrets = Some(secrets);
    let credential = fixture.credential("task-5", "pod-1");
    fixture.spawn(&launch, "pod-1", &credential);

    wait_until_async(Duration::from_secs(30), "completion", || {
        runner_status(&fixture, "task-5") == Some(ExecutionStatus::Completed)
    })
    .await;
    // The tools saw the values (compared by digest, so the workflow itself
    // need not carry them).
    let digest = |value: &str| {
        use sha2::Digest;
        format!("{:x}", sha2::Sha256::digest(value.as_bytes()))
    };
    let seen = |file: &str| {
        fs::read_to_string(launch.home().join(file))
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_string()
    };
    assert_eq!(seen("token.sha256"), digest("tok-123-secret"));
    assert_eq!(seen("hosts.sha256"), digest("hosts-secret-xyz"));
    assert!(
        fs::symlink_metadata(launch.home().join(".config/gh/hosts.yml"))
            .unwrap()
            .file_type()
            .is_symlink()
    );

    // No regular file on task storage holds either value.
    let mut stack = vec![launch.storage.clone()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                stack.push(entry.path());
            } else if kind.is_file() {
                let bytes = fs::read(entry.path()).unwrap();
                let text = String::from_utf8_lossy(&bytes);
                assert!(
                    !text.contains("tok-123-secret") && !text.contains("hosts-secret-xyz"),
                    "a secret leaked into {}",
                    entry.path().display()
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// One storage, one task (R39)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_storage_that_is_not_empty_is_never_initialized() {
    if running_as_root() {
        return;
    }
    let mut fixture = Fixture::served().await;
    let spec = spec(
        TWO_STEP_WORKFLOW,
        TaskWorkspace::None,
        ResolvedEnvironment::harness_only("claude-code"),
    );
    fixture.create("task-6", spec.clone());
    fixture.report("task-6", Observation::StartAccepted);
    let launch = Launch::new(&fixture, "task-6", &spec);
    fs::write(launch.storage.join("someone-elses-file"), "data").unwrap();
    let credential = fixture.credential("task-6", "pod-1");
    let runner = fixture.spawn(&launch, "pod-1", &credential);

    let status = fixture.wait_exit(runner, Duration::from_secs(30));
    assert_eq!(status.code(), Some(3));
    let problem = fixture
        .record("task-6")
        .runner
        .and_then(|runner| runner.snapshot.problem)
        .expect("reported");
    assert_eq!(problem.code, "initialization_failed");
    assert!(
        problem.message.contains("someone-elses-file"),
        "{}",
        problem.message
    );
    assert!(launch.execution_id().is_none());
}

// ---------------------------------------------------------------------------
// Operator commands (U3)
// ---------------------------------------------------------------------------

/// A one-step agent workflow whose step allows overrides.
const AGENT_WORKFLOW: &str = r#"
name: agent-task
version: 1
states:
  think:
    allow_manual_override: true
    action:
      type: agent
      prompt: Think about it.
    outputs: [done]
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states: [completed]
"#;

/// A fake `claude` that hangs on its first invocation and answers `done` on
/// every later one, recording its pid and its arguments (the prompt, with
/// any operator note) under `$HOME`.
fn hanging_claude(root: &TempRoot) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = root.dir("bin");
    let script = "#!/bin/sh\n\
        tries=$(cat \"$HOME/claude-tries\" 2>/dev/null | wc -l)\n\
        echo try >> \"$HOME/claude-tries\"\n\
        echo $$ >> \"$HOME/claude-pids\"\n\
        printf '%s\\n' \"$@\" >> \"$HOME/claude-args\"\n\
        if [ \"$tries\" -eq 0 ]; then sleep 60; fi\n\
        printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\
        \"session_id\":\"fake\",\"result\":\"Done.\\n{\\\"output\\\": \\\"done\\\"}\"}'\n";
    let temp = bin.join(".claude.tmp");
    fs::write(&temp, script).unwrap();
    fs::set_permissions(&temp, fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(&temp, bin.join("claude")).unwrap();
    bin
}

/// Starts a runner for `task` and brings the task to `Running` once the
/// runner has reported, the way a controller would.
async fn running_runner(fixture: &mut Fixture, task: &str, launch: &Launch) -> usize {
    fixture.report(task, Observation::StartAccepted);
    let credential = fixture.credential(task, "pod-1");
    let runner = fixture.spawn(launch, "pod-1", &credential);
    wait_until_async(Duration::from_secs(30), "the runner's first report", || {
        fixture.record(task).runner.is_some()
    })
    .await;
    fixture.report(task, Observation::RunnerReady);
    runner
}

fn submit(fixture: &Fixture, task: &str, command: OperatorCommand, key: &str) -> String {
    fixture
        .service
        .submit_operator_command(
            &fixture.owner(),
            task,
            SubmitOperatorCommandRequest {
                command,
                idempotency_key: key.to_string(),
                expected_generation: None,
            },
        )
        .unwrap_or_else(|error| panic!("submit {key}: {error}"))
        .command
        .command_id
}

async fn settled(fixture: &Fixture, task: &str, command_id: &str) -> QueuedOperatorCommand {
    wait_until_async(Duration::from_secs(30), "the runner's verdict", || {
        fixture
            .record(task)
            .operator_command(command_id)
            .is_some_and(|command| command.status.is_settled())
    })
    .await;
    fixture
        .record(task)
        .operator_command(command_id)
        .cloned()
        .expect("the command")
}

fn process_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only probes for the process.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interrupted_step_is_dropped_and_restarted_with_the_owners_note() {
    if running_as_root() {
        return;
    }
    let mut fixture = Fixture::served().await;
    let bin = hanging_claude(&fixture.root);
    let spec = spec(
        AGENT_WORKFLOW,
        TaskWorkspace::None,
        ResolvedEnvironment::harness_only("claude-code"),
    );
    fixture.create("task-1", spec.clone());
    let launch = Launch::new(&fixture, "task-1", &spec).with_bin_first(&bin);
    running_runner(&mut fixture, "task-1", &launch).await;
    let pids = launch.home().join("claude-pids");
    wait_until_async(Duration::from_secs(30), "the hanging agent", || {
        pids.is_file()
    })
    .await;
    let hanging: u32 = fs::read_to_string(&pids)
        .unwrap()
        .trim()
        .parse()
        .expect("the agent's pid");

    // A restart while the step runs is refused by the Engine's own claim.
    let early = submit(&fixture, "task-1", restart(None, None), "restart-early");
    let early = settled(&fixture, "task-1", &early).await;
    assert_eq!(early.status, OperatorCommandStatus::Rejected);
    assert!(
        early
            .detail
            .as_deref()
            .is_some_and(|d| d.contains("interrupt it first")),
        "{early:?}"
    );

    let interrupt = submit(
        &fixture,
        "task-1",
        OperatorCommand::Interrupt,
        "interrupt-1",
    );
    let interrupt = settled(&fixture, "task-1", &interrupt).await;
    assert_eq!(
        interrupt.status,
        OperatorCommandStatus::Applied,
        "{interrupt:?}"
    );
    assert_eq!(launch.status(), Some(ExecutionStatus::Unknown));
    wait_until_async(Duration::from_secs(10), "the hanging agent to die", || {
        !process_alive(hanging)
    })
    .await;
    assert!(launch.events().iter().any(|event| matches!(
        &event.data,
        EventData::Error { message, .. } if message.contains("will not be replayed")
    )));

    // Nothing is running any more, so a second interrupt has nothing to cut.
    let again = submit(
        &fixture,
        "task-1",
        OperatorCommand::Interrupt,
        "interrupt-2",
    );
    let again = settled(&fixture, "task-1", &again).await;
    assert_eq!(again.status, OperatorCommandStatus::Rejected);

    let restart_id = submit(
        &fixture,
        "task-1",
        restart(Some("Mind the edge cases."), None),
        "restart-1",
    );
    let restarted = settled(&fixture, "task-1", &restart_id).await;
    assert_eq!(
        restarted.status,
        OperatorCommandStatus::Applied,
        "{restarted:?}"
    );
    wait_until_async(Duration::from_secs(30), "the restarted step", || {
        runner_status(&fixture, "task-1") == Some(ExecutionStatus::Completed)
    })
    .await;
    let args = fs::read_to_string(launch.home().join("claude-args")).unwrap();
    assert_eq!(
        args.matches("Operator recovery note:").count(),
        1,
        "only the restarted attempt got the note: {args}"
    );
    assert!(args.contains("Mind the edge cases."));

    // The control plane's log tells the whole story in order.
    let verdicts: Vec<_> = fixture
        .service
        .events(&fixture.owner(), "task-1", 0)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.kind {
            TaskEventKind::OperatorCommandQueued { command_id, .. } => Some((command_id, "queued")),
            TaskEventKind::OperatorCommandResolved { command_id, .. } => {
                Some((command_id, "resolved"))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        verdicts.len(),
        8,
        "four commands, queued and resolved: {verdicts:?}"
    );
}

fn restart(note: Option<&str>, failure_key: Option<&str>) -> OperatorCommand {
    OperatorCommand::RestartStep {
        note: note.map(str::to_string),
        failure_key: failure_key.map(str::to_string),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_acknowledgement_never_carries_a_command_out_twice() {
    if running_as_root() {
        return;
    }
    let mut fixture = Fixture::served().await;
    let bin = hanging_claude(&fixture.root);
    let spec = spec(
        AGENT_WORKFLOW,
        TaskWorkspace::None,
        ResolvedEnvironment::harness_only("claude-code"),
    );
    fixture.create("task-1", spec.clone());
    let launch = Launch::new(&fixture, "task-1", &spec).with_bin_first(&bin);
    running_runner(&mut fixture, "task-1", &launch).await;
    wait_until_async(Duration::from_secs(30), "the hanging agent", || {
        launch.home().join("claude-pids").is_file()
    })
    .await;

    fixture.block_command_acks.store(true, Ordering::SeqCst);
    let command_id = submit(
        &fixture,
        "task-1",
        OperatorCommand::Interrupt,
        "interrupt-1",
    );
    wait_until_async(
        Duration::from_secs(30),
        "the interrupt to be carried out",
        || launch.status() == Some(ExecutionStatus::Unknown),
    )
    .await;
    // Redelivered on every poll, still unacknowledged.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        fixture
            .record("task-1")
            .operator_command(&command_id)
            .unwrap()
            .status,
        OperatorCommandStatus::Queued
    );

    fixture.block_command_acks.store(false, Ordering::SeqCst);
    let command = settled(&fixture, "task-1", &command_id).await;

    // Carried out again, an interrupt would find nothing to cut and be
    // rejected: the verdict is the first one's, from the runner's record.
    assert_eq!(
        command.status,
        OperatorCommandStatus::Applied,
        "{command:?}"
    );
    let interruptions = launch
        .events()
        .iter()
        .filter(|event| {
            matches!(
                &event.data,
                EventData::Error { message, .. } if message.contains("will not be replayed")
            )
        })
        .count();
    assert_eq!(interruptions, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_naming_a_failure_the_task_has_moved_on_from_is_rejected_with_the_reason() {
    if running_as_root() {
        return;
    }
    let mut fixture = Fixture::served().await;
    let bin = hanging_claude(&fixture.root);
    let spec = spec(
        AGENT_WORKFLOW,
        TaskWorkspace::None,
        ResolvedEnvironment::harness_only("claude-code"),
    );
    fixture.create("task-1", spec.clone());
    let launch = Launch::new(&fixture, "task-1", &spec).with_bin_first(&bin);
    running_runner(&mut fixture, "task-1", &launch).await;
    wait_until_async(Duration::from_secs(30), "the hanging agent", || {
        launch.home().join("claude-pids").is_file()
    })
    .await;
    let interrupt = submit(
        &fixture,
        "task-1",
        OperatorCommand::Interrupt,
        "interrupt-1",
    );
    settled(&fixture, "task-1", &interrupt).await;
    let execution_id = launch.execution_id().unwrap();
    let events_before = launch.events().len();

    let id = submit(
        &fixture,
        "task-1",
        restart(Some("again"), Some("failure-of-another-time")),
        "restart-1",
    );
    let rejected = settled(&fixture, "task-1", &id).await;

    assert_eq!(rejected.status, OperatorCommandStatus::Rejected);
    assert!(
        rejected
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("failure-of-another-time")),
        "{rejected:?}"
    );
    assert_eq!(
        launch.store().load_execution(&execution_id).unwrap().status,
        ExecutionStatus::Unknown,
        "the execution was not touched"
    );
    assert_eq!(launch.events().len(), events_before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_override_transition_is_taken_under_the_claim_and_audited_with_its_reason() {
    if running_as_root() {
        return;
    }
    let mut fixture = Fixture::served().await;
    let bin = hanging_claude(&fixture.root);
    let spec = spec(
        AGENT_WORKFLOW,
        TaskWorkspace::None,
        ResolvedEnvironment::harness_only("claude-code"),
    );
    fixture.create("task-1", spec.clone());
    let launch = Launch::new(&fixture, "task-1", &spec).with_bin_first(&bin);
    running_runner(&mut fixture, "task-1", &launch).await;
    wait_until_async(Duration::from_secs(30), "the hanging agent", || {
        launch.home().join("claude-pids").is_file()
    })
    .await;

    // While the agent step runs, no transition can be taken.
    let busy = submit(
        &fixture,
        "task-1",
        OperatorCommand::Transition {
            target: "completed".to_string(),
            forced: true,
            reason: Some("early".to_string()),
        },
        "transition-early",
    );
    let busy = settled(&fixture, "task-1", &busy).await;
    assert_eq!(busy.status, OperatorCommandStatus::Rejected);
    assert!(busy
        .detail
        .as_deref()
        .is_some_and(|d| d.contains("interrupt it first")));
    assert_eq!(launch.status(), Some(ExecutionStatus::Running));

    let interrupt = submit(
        &fixture,
        "task-1",
        OperatorCommand::Interrupt,
        "interrupt-1",
    );
    settled(&fixture, "task-1", &interrupt).await;
    let id = submit(
        &fixture,
        "task-1",
        OperatorCommand::Transition {
            target: "completed".to_string(),
            forced: true,
            reason: Some("the ticket was closed upstream".to_string()),
        },
        "transition-1",
    );
    let taken = settled(&fixture, "task-1", &id).await;

    assert_eq!(taken.status, OperatorCommandStatus::Applied, "{taken:?}");
    assert_eq!(launch.status(), Some(ExecutionStatus::Completed));
    // The execution's own log records the override with its reason and
    // the owner who asked, under the override provider.
    assert!(launch.events().iter().any(|event| matches!(
        &event.data,
        EventData::DecisionMade { provider, reasoning: Some(reasoning), .. }
            if provider == "human_override"
                && reasoning.contains("the ticket was closed upstream")
                && reasoning.contains(LOCAL_OWNER)
    )));
}

const TWO_STATE_AGENT_WORKFLOW: &str = r#"
name: agent-then-script
version: 1
states:
  think:
    action:
      type: agent
      prompt: Think about it.
    outputs: [done]
    transitions:
      done: next
  next:
    action:
      type: script
      command: sh
      args: [-c, "echo ran >> \"$HOME/next-ran\"; printf '{\"output\": \"done\"}'"]
    outputs: [done]
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states: [completed]
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_declared_transition_carries_on_from_the_state_it_leads_to() {
    if running_as_root() {
        return;
    }
    let mut fixture = Fixture::served().await;
    let bin = hanging_claude(&fixture.root);
    let spec = spec(
        TWO_STATE_AGENT_WORKFLOW,
        TaskWorkspace::None,
        ResolvedEnvironment::harness_only("claude-code"),
    );
    fixture.create("task-1", spec.clone());
    let launch = Launch::new(&fixture, "task-1", &spec).with_bin_first(&bin);
    running_runner(&mut fixture, "task-1", &launch).await;
    wait_until_async(Duration::from_secs(30), "the hanging agent", || {
        launch.home().join("claude-pids").is_file()
    })
    .await;
    let interrupt = submit(
        &fixture,
        "task-1",
        OperatorCommand::Interrupt,
        "interrupt-1",
    );
    settled(&fixture, "task-1", &interrupt).await;

    // Not an exit of `think`, and no override: refused by the runner too,
    // whatever the control plane let through.
    // (Queued past the control plane's own check, as a stale view of the
    // task's state would.)
    let id = fixture
        .service
        .control_plane()
        .queue_operator_command(
            &TaskId::parse("task-1").unwrap(),
            narsor::orchestration::OperatorCommandRequest {
                command: OperatorCommand::Transition {
                    target: "completed".to_string(),
                    forced: false,
                    reason: None,
                },
                idempotency_key: "transition-bad".to_string(),
                expected_generation: None,
                requested_by: fixture.owner(),
            },
        )
        .expect("queued")
        .command
        .command_id;
    let refused = settled(&fixture, "task-1", &id).await;
    assert_eq!(refused.status, OperatorCommandStatus::Rejected);
    assert_eq!(launch.status(), Some(ExecutionStatus::Unknown));

    let id = submit(
        &fixture,
        "task-1",
        OperatorCommand::Transition {
            target: "next".to_string(),
            forced: false,
            reason: None,
        },
        "transition-1",
    );
    let taken = settled(&fixture, "task-1", &id).await;

    assert_eq!(taken.status, OperatorCommandStatus::Applied, "{taken:?}");
    wait_until_async(Duration::from_secs(30), "the next state's work", || {
        runner_status(&fixture, "task-1") == Some(ExecutionStatus::Completed)
    })
    .await;
    assert_eq!(
        fs::read_to_string(launch.home().join("next-ran")).unwrap(),
        "ran\n"
    );
    assert!(launch.events().iter().any(|event| matches!(
        &event.data,
        EventData::DecisionMade { provider, .. } if provider == "human"
    )));
}

// ---------------------------------------------------------------------------
// The conductor in the runner (U5)
// ---------------------------------------------------------------------------

/// A fake `claude` that answers by what it is asked: an agent step with
/// `agent`, the conductor by its purpose (`diagnose`, `reply`, `annotate`,
/// `choose`). Each answer is the reply text; the fake wraps it the way the
/// harness does. It records the HOME of every call and which answer it gave.
fn conductor_claude(root: &TempRoot, answers: &[(&str, String)]) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = root.dir("bin");
    let dir = root.dir("claude-answers");
    for (key, text) in answers {
        let line = serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "session_id": "fake",
            "result": text,
        });
        fs::write(dir.join(key), format!("{line}\n")).unwrap();
    }
    let script = format!(
        "#!/bin/sh\n\
         printf '%s\\n' \"$HOME\" >> \"$HOME/claude-homes\"\n\
         case \"$*\" in\n\
         *'You are the conductor of a narsor'*)\n\
           case \"$*\" in\n\
           *'Diagnose why'*) key=diagnose ;;\n\
           *\"Answer the operator's message\"*) key=reply ;;\n\
           *'Summarize what the operator'*) key=annotate ;;\n\
           *) key=choose ;;\n\
           esac ;;\n\
         *) key=agent ;;\n\
         esac\n\
         echo \"$key\" >> \"$HOME/claude-calls\"\n\
         cat '{}'/\"$key\"\n",
        dir.display()
    );
    let temp = bin.join(".claude.tmp");
    fs::write(&temp, script).unwrap();
    fs::set_permissions(&temp, fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(&temp, bin.join("claude")).unwrap();
    bin
}

/// A conductor's reply: some words, then the JSON object it ends with.
fn conductor_says(answer: serde_json::Value) -> String {
    format!("Understood.\n{answer}")
}

/// A script step that fails on its first run and succeeds on the next.
const FLAKY_WORKFLOW: &str = r#"
name: flaky-task
version: 1
states:
  build:
    action:
      type: script
      command: sh
      args:
        - -c
        - |
          if [ -f retried ]; then printf '{"output": "done"}'; else touch retried; echo boom >&2; exit 3; fi
    outputs: [done]
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states: [completed]
"#;

/// A script step that takes long enough for a test to talk to the
/// conductor meanwhile.
const LONG_STEP_WORKFLOW: &str = r#"
name: long-task
version: 1
states:
  wait:
    action:
      type: script
      command: sh
      args: [-c, "sleep 60; printf '{\"output\": \"done\"}'"]
    outputs: [done]
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states: [completed]
"#;

/// An agent step that answers without its output label.
const LABELLESS_WORKFLOW: &str = r#"
name: labelless-task
version: 1
states:
  work:
    action:
      type: agent
      prompt: Do the work.
    outputs: [implemented]
    transitions:
      implemented: completed
  completed:
    description: Done.
terminal_states: [completed]
"#;

/// What the owner reads of the task's conductor, once the runner answers.
async fn conductor_view(fixture: &Fixture, task: &str) -> narsor::conductor::ConductorView {
    match fixture
        .service
        .conductor(&fixture.owner(), task, ConductorReadQuery::default())
        .await
        .expect("read the conductor")
    {
        ConductorReadResponse::Available { conductor, .. } => *conductor,
        ConductorReadResponse::Unavailable { message, .. } => {
            panic!("the conductor is not readable: {message}")
        }
    }
}

async fn wait_for_view(
    fixture: &Fixture,
    task: &str,
    what: &str,
    mut ready: impl FnMut(&narsor::conductor::ConductorView) -> bool,
) -> narsor::conductor::ConductorView {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let view = conductor_view(fixture, task).await;
        if ready(&view) {
            return view;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn attention_of(fixture: &Fixture, task: &str) -> Vec<narsor::orchestration::AttentionItem> {
    fixture
        .record(task)
        .runner
        .map(|runner| runner.snapshot.attention)
        .unwrap_or_default()
}

fn attention_kinds(fixture: &Fixture, task: &str) -> Vec<AttentionKind> {
    attention_of(fixture, task)
        .into_iter()
        .map(|item| item.kind)
        .collect()
}

async fn applied(fixture: &Fixture, task: &str, command: OperatorCommand, key: &str) {
    let id = submit(fixture, task, command, key);
    let settled = settled(fixture, task, &id).await;
    assert_eq!(
        settled.status,
        OperatorCommandStatus::Applied,
        "{settled:?}"
    );
}

async fn rejected(
    fixture: &Fixture,
    task: &str,
    command: OperatorCommand,
    key: &str,
) -> QueuedOperatorCommand {
    let id = submit(fixture, task, command, key);
    let settled = settled(fixture, task, &id).await;
    assert_eq!(
        settled.status,
        OperatorCommandStatus::Rejected,
        "{settled:?}"
    );
    settled
}

/// A failed step with a conductor: it is diagnosed, the failed step waits
/// for the owner with the diagnosis and its proposals, and confirming the
/// proposal restarts the step and clears the wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_step_is_diagnosed_and_its_confirmed_proposal_restarts_it() {
    if running_as_root() {
        return;
    }
    let mut fixture = Fixture::served().await;
    let bin = conductor_claude(
        &fixture.root,
        &[(
            "diagnose",
            conductor_says(serde_json::json!({
                "decision": "diagnose",
                "confidence": "high",
                "rationale": "the script exits with 3 on its first run",
                "message": "The build failed.",
                "diagnosis": "the script exited with 3",
                "proposals": [
                    {"kind": "retry", "summary": "Run the build again", "text": "run it again"}
                ],
            })),
        )],
    );
    let spec = spec(
        FLAKY_WORKFLOW,
        TaskWorkspace::None,
        ResolvedEnvironment::harness_only("claude-code"),
    );
    fixture.create("task-1", spec.clone());
    let launch = Launch::new(&fixture, "task-1", &spec).with_bin_first(&bin);
    running_runner(&mut fixture, "task-1", &launch).await;

    let view = wait_for_view(&fixture, "task-1", "the diagnosis", |view| {
        view.failure.is_some() && !view.proposals.is_empty()
    })
    .await;
    assert!(view.available, "{view:?}");
    assert_eq!(view.harness.as_deref(), Some("claude-code"));
    let failure = view.failure.as_ref().unwrap();
    assert!(failure.reason.contains("exited with 3"), "{failure:?}");
    let proposal = &view.proposals[0];
    assert_eq!(proposal.state, narsor::conductor::ProposalState::Pending);
    assert_eq!(proposal.in_reply_to, None, "it came with the diagnosis");

    // The failed step waits, named by the key a restart of it names; the
    // diagnosis' proposal is part of it, not a second wait.
    let execution = launch
        .store()
        .load_execution(&launch.execution_id().unwrap())
        .unwrap();
    let key = narsor::engine::step_failure(&execution)
        .unwrap()
        .expect("the failure is recorded")
        .key()
        .to_string();
    wait_until_async(
        Duration::from_secs(30),
        "the failed step's attention",
        || !attention_of(&fixture, "task-1").is_empty(),
    )
    .await;
    let attention = attention_of(&fixture, "task-1");
    assert_eq!(attention.len(), 1, "{attention:?}");
    assert_eq!(attention[0].kind, AttentionKind::FailedStep);
    assert_eq!(attention[0].waiting_id, key);
    assert_eq!(attention[0].state_id.as_deref(), Some("build"));
    let summary = fixture.service.task(&fixture.owner(), "task-1").unwrap();
    let runner = summary.runner.expect("the runner reported");
    assert!(runner.conductor_available);
    assert_eq!(runner.conductor_unavailable_reason, None);

    applied(
        &fixture,
        "task-1",
        OperatorCommand::ConfirmProposal {
            proposal_id: proposal.proposal_id.clone(),
        },
        "confirm-1",
    )
    .await;
    wait_until_async(Duration::from_secs(30), "completion", || {
        runner_status(&fixture, "task-1") == Some(ExecutionStatus::Completed)
    })
    .await;
    wait_until_async(Duration::from_secs(30), "the wait to clear", || {
        attention_of(&fixture, "task-1").is_empty()
    })
    .await;
    let view = conductor_view(&fixture, "task-1").await;
    assert_eq!(
        view.proposals[0].state,
        narsor::conductor::ProposalState::Executed
    );

    // Settled once: confirming it again is refused, with the reason.
    let again = rejected(
        &fixture,
        "task-1",
        OperatorCommand::ConfirmProposal {
            proposal_id: proposal.proposal_id.clone(),
        },
        "confirm-2",
    )
    .await;
    assert!(
        again
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("already settled")),
        "{again:?}"
    );
}

/// A runner whose harness cannot run here has no conductor and says why: a
/// failed step goes to the owner as it is, without a diagnosis, and a
/// restart naming its key carries it on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_conductor_a_failed_step_waits_for_the_owner_without_proposals() {
    if running_as_root() {
        return;
    }
    let mut fixture = Fixture::served().await;
    let spec = spec(
        FLAKY_WORKFLOW,
        TaskWorkspace::None,
        ResolvedEnvironment::harness_only("no-such-harness"),
    );
    fixture.create("task-1", spec.clone());
    let launch = Launch::new(&fixture, "task-1", &spec);
    running_runner(&mut fixture, "task-1", &launch).await;

    wait_until_async(
        Duration::from_secs(30),
        "the failed step's attention",
        || attention_kinds(&fixture, "task-1") == vec![AttentionKind::FailedStep],
    )
    .await;
    let summary = fixture.service.task(&fixture.owner(), "task-1").unwrap();
    let runner = summary.runner.expect("the runner reported");
    assert!(!runner.conductor_available);
    assert!(
        runner
            .conductor_unavailable_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("no-such-harness")),
        "{:?}",
        runner.conductor_unavailable_reason
    );

    let view = conductor_view(&fixture, "task-1").await;
    assert!(!view.available);
    assert!(view.unavailable_reason.is_some());
    assert!(view.failure.is_none(), "nobody diagnosed it: {view:?}");
    assert!(view.proposals.is_empty());

    for (command, key) in [
        (
            OperatorCommand::ConductorChat {
                text: "anyone there?".to_string(),
            },
            "chat-1",
        ),
        (
            OperatorCommand::ConfirmProposal {
                proposal_id: "p".to_string(),
            },
            "confirm-1",
        ),
    ] {
        let refused = rejected(&fixture, "task-1", command, key).await;
        assert!(
            refused
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("no conductor")),
            "{refused:?}"
        );
    }

    // The owner's own restart, naming the failure the wait names.
    let key = attention_of(&fixture, "task-1")[0].waiting_id.clone();
    applied(
        &fixture,
        "task-1",
        restart(Some("once more"), Some(&key)),
        "restart-1",
    )
    .await;
    wait_until_async(Duration::from_secs(30), "completion", || {
        runner_status(&fixture, "task-1") == Some(ExecutionStatus::Completed)
    })
    .await;
}

/// The owner writes to the conductor while the step runs: the answer is
/// read back, its proposal waits for the owner as a conductor proposal, and
/// rejecting it settles it for good.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chat_answer_and_its_proposal_are_read_back_and_a_rejection_settles_it() {
    if running_as_root() {
        return;
    }
    let mut fixture = Fixture::served().await;
    let bin = conductor_claude(
        &fixture.root,
        &[(
            "reply",
            conductor_says(serde_json::json!({
                "decision": "reply",
                "confidence": "high",
                "rationale": "it is waiting",
                "message": "The step is still sleeping.",
                "proposals": [{"kind": "abort", "summary": "Cancel the task"}],
            })),
        )],
    );
    let spec = spec(
        LONG_STEP_WORKFLOW,
        TaskWorkspace::None,
        ResolvedEnvironment::harness_only("claude-code"),
    );
    fixture.create("task-1", spec.clone());
    let launch = Launch::new(&fixture, "task-1", &spec).with_bin_first(&bin);
    running_runner(&mut fixture, "task-1", &launch).await;

    applied(
        &fixture,
        "task-1",
        OperatorCommand::ConductorChat {
            text: "  how is it going?  ".to_string(),
        },
        "chat-1",
    )
    .await;
    let view = wait_for_view(&fixture, "task-1", "the answer", |view| {
        view.chat.iter().any(|entry| {
            entry.author == narsor::conductor::ChatAuthor::Conductor
                && entry.text == "The step is still sleeping."
        })
    })
    .await;
    assert_eq!(view.chat[0].text, "how is it going?", "the text is trimmed");
    assert!(view.unanswered_chat.is_empty(), "{view:?}");
    let proposal = view.proposals[0].clone();
    assert_eq!(proposal.state, narsor::conductor::ProposalState::Pending);
    assert!(proposal.in_reply_to.is_some(), "it answers the message");

    wait_until_async(Duration::from_secs(30), "the proposal's attention", || {
        attention_of(&fixture, "task-1")
            .iter()
            .any(|item| item.kind == AttentionKind::ConductorProposal)
    })
    .await;
    let waiting: Vec<_> = attention_of(&fixture, "task-1")
        .into_iter()
        .filter(|item| item.kind == AttentionKind::ConductorProposal)
        .collect();
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].waiting_id, proposal.proposal_id);

    applied(
        &fixture,
        "task-1",
        OperatorCommand::RejectProposal {
            proposal_id: proposal.proposal_id.clone(),
        },
        "reject-1",
    )
    .await;
    wait_until_async(Duration::from_secs(30), "the wait to clear", || {
        attention_kinds(&fixture, "task-1").is_empty()
    })
    .await;
    let view = conductor_view(&fixture, "task-1").await;
    assert_eq!(
        view.proposals[0].state,
        narsor::conductor::ProposalState::Rejected
    );

    // A rejected proposal can neither be rejected again nor confirmed.
    for (command, key) in [
        (
            OperatorCommand::RejectProposal {
                proposal_id: proposal.proposal_id.clone(),
            },
            "reject-2",
        ),
        (
            OperatorCommand::ConfirmProposal {
                proposal_id: proposal.proposal_id.clone(),
            },
            "confirm-1",
        ),
    ] {
        rejected(&fixture, "task-1", command, key).await;
    }
    assert_eq!(
        runner_status(&fixture, "task-1"),
        Some(ExecutionStatus::Running),
        "nothing was carried out"
    );
}

/// The conductor completed a step with its own choice of an output; the
/// owner takes it back and the step is a problem again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chosen_output_is_undone_and_the_step_is_a_problem_again() {
    if running_as_root() {
        return;
    }
    let mut fixture = Fixture::served().await;
    let bin = conductor_claude(
        &fixture.root,
        &[
            ("agent", "Done: three units.".to_string()),
            (
                "choose",
                conductor_says(serde_json::json!({
                    "decision": "choose_output",
                    "label": "implemented",
                    "data": {},
                    "variables": {},
                    "confidence": "high",
                    "rationale": "the agent said it finished three units",
                })),
            ),
        ],
    );
    let spec = spec(
        LABELLESS_WORKFLOW,
        TaskWorkspace::None,
        ResolvedEnvironment::harness_only("claude-code"),
    );
    fixture.create("task-1", spec.clone());
    let launch = Launch::new(&fixture, "task-1", &spec).with_bin_first(&bin);
    running_runner(&mut fixture, "task-1", &launch).await;
    wait_until_async(Duration::from_secs(60), "the conductor's choice", || {
        runner_status(&fixture, "task-1") == Some(ExecutionStatus::Completed)
    })
    .await;

    let view = conductor_view(&fixture, "task-1").await;
    assert_eq!(view.decisions.len(), 1, "{view:?}");
    let decision = &view.decisions[0];
    assert_eq!(decision.kind, "choose_output");
    assert!(decision.undoable && !decision.undone);

    applied(
        &fixture,
        "task-1",
        OperatorCommand::UndoDecision {
            decision_id: decision.decision_id.clone(),
        },
        "undo-1",
    )
    .await;
    wait_until_async(Duration::from_secs(30), "the problem to come back", || {
        runner_status(&fixture, "task-1") == Some(ExecutionStatus::InputNeeded)
    })
    .await;
    let view = conductor_view(&fixture, "task-1").await;
    assert!(view.decisions[0].undone && !view.decisions[0].undoable);
    assert!(
        view.chat.iter().any(
            |entry| entry.author == narsor::conductor::ChatAuthor::Operator
                && entry.text.contains("Undid the conductor's choice")
        ),
        "the undo is in the history: {:?}",
        view.chat
    );
    wait_until_async(Duration::from_secs(30), "the problem's attention", || {
        attention_kinds(&fixture, "task-1").contains(&AttentionKind::Problem)
    })
    .await;

    let again = rejected(
        &fixture,
        "task-1",
        OperatorCommand::UndoDecision {
            decision_id: decision.decision_id.clone(),
        },
        "undo-2",
    )
    .await;
    assert!(
        again
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("already undone")),
        "{again:?}"
    );
}

/// The conductor's harness is no longer chosen per task: a choice an older
/// version kept on the storage is not read, and a `SetConductorHarness`
/// queued before the update is settled without changing anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_per_task_conductor_harness_from_before_is_ignored() {
    if running_as_root() {
        return;
    }
    let mut fixture = Fixture::served().await;
    let bin = conductor_claude(
        &fixture.root,
        &[(
            "reply",
            conductor_says(serde_json::json!({
                "decision": "reply", "message": "From claude.",
            })),
        )],
    );
    let spec = spec(
        LONG_STEP_WORKFLOW,
        TaskWorkspace::None,
        ResolvedEnvironment::harness_only("claude-code"),
    );
    fixture.create("task-1", spec.clone());
    let launch = Launch::new(&fixture, "task-1", &spec).with_bin_first(&bin);
    // What an older version wrote when the owner chose `opencode`.
    let remembered = launch.storage.join("narsor/conductor.json");
    fs::create_dir_all(remembered.parent().unwrap()).unwrap();
    fs::write(&remembered, r#"{"version":1,"harness":"opencode"}"#).unwrap();
    running_runner(&mut fixture, "task-1", &launch).await;

    let view = wait_for_view(&fixture, "task-1", "the conductor", |view| view.available).await;
    assert_eq!(view.harness.as_deref(), Some("claude-code"));

    // Queued before the control plane stopped taking it.
    let historical = fixture
        .service
        .control_plane()
        .queue_operator_command(
            &TaskId::parse("task-1").unwrap(),
            OperatorCommandRequest {
                command: OperatorCommand::SetConductorHarness {
                    harness: "codex".to_string(),
                },
                idempotency_key: "harness-1".to_string(),
                expected_generation: None,
                requested_by: fixture.owner(),
            },
        )
        .expect("queue the historical command")
        .command
        .command_id;
    // A command through the service wakes the runner's poll.
    let chat = submit(
        &fixture,
        "task-1",
        OperatorCommand::ConductorChat {
            text: "hello".to_string(),
        },
        "chat-1",
    );
    let settled_harness = settled(&fixture, "task-1", &historical).await;
    assert_eq!(
        settled_harness.status,
        OperatorCommandStatus::Rejected,
        "{settled_harness:?}"
    );
    assert!(
        settled_harness
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("no longer chosen per task")),
        "{settled_harness:?}"
    );
    settled(&fixture, "task-1", &chat).await;
    let view = wait_for_view(&fixture, "task-1", "claude's answer", |view| {
        view.chat.iter().any(|entry| entry.text == "From claude.")
    })
    .await;
    assert_eq!(view.harness.as_deref(), Some("claude-code"));
    assert_eq!(
        fs::read_to_string(&remembered).unwrap(),
        r#"{"version":1,"harness":"opencode"}"#,
        "the old file is neither read nor removed"
    );
}

// ---------------------------------------------------------------------------
// The default harness running tasks follow (U7)
// ---------------------------------------------------------------------------

/// Fake harness binaries under one `bin`: each of `programs` (`claude`,
/// `opencode`, `codex`) answers an agent step with the `done` label in its
/// own stream format.
fn fake_harnesses(root: &TempRoot, programs: &[&str]) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = root.dir("bin");
    let answers = root.dir("harness-answers");
    let text = "Done.\n{\"output\": \"done\"}";
    for program in programs {
        let lines: Vec<serde_json::Value> = match *program {
            "claude" => vec![serde_json::json!({
                "type": "result", "subtype": "success", "is_error": false,
                "session_id": "fake-claude", "result": text,
            })],
            "opencode" => vec![
                serde_json::json!({ "type": "step_start", "sessionID": "fake-opencode" }),
                serde_json::json!({ "type": "text", "sessionID": "fake-opencode", "text": text }),
                serde_json::json!({ "type": "step_finish", "sessionID": "fake-opencode" }),
            ],
            "codex" => vec![
                serde_json::json!({ "type": "thread.started", "thread_id": "fake-codex" }),
                serde_json::json!({
                    "type": "item.completed",
                    "item": { "type": "agent_message", "text": text },
                }),
                serde_json::json!({ "type": "turn.completed" }),
            ],
            other => panic!("no fake for {other}"),
        };
        let answer = answers.join(program);
        let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
        fs::write(&answer, body).unwrap();
        let temp = bin.join(format!(".{program}.tmp"));
        fs::write(&temp, format!("#!/bin/sh\ncat '{}'\n", answer.display())).unwrap();
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&temp, bin.join(program)).unwrap();
    }
    bin
}

/// A PATH of `bin` alone, with the basic tools a task needs linked into it:
/// no harness the test did not fake can be found (a developer's machine may
/// have the real ones).
fn only_on_path(bin: &Path) -> String {
    for tool in [
        "sh", "cat", "cp", "git", "sleep", "env", "grep", "ls", "mkdir", "mv", "rm", "sed",
    ] {
        let found = Command::new("sh")
            .args(["-c", &format!("command -v {tool}")])
            .output()
            .unwrap();
        let target = String::from_utf8_lossy(&found.stdout).trim().to_string();
        let link = bin.join(tool);
        if !link.exists() {
            std::os::unix::fs::symlink(target, link).unwrap();
        }
    }
    bin.display().to_string()
}

/// Three agent steps with an owner's gate between each two, so a test
/// decides when the next one starts.
const THREE_STEP_WORKFLOW: &str = r#"
name: three-steps
version: 1
states:
  a:
    action:
      type: agent
      prompt: First.
    outputs: [done]
    transitions:
      done: g1
  g1:
    action:
      type: script
      command: sh
      args: [-c, "printf '{\"output\": \"ok\"}'"]
    outputs: [ok, rejected]
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
    outputs: [done]
    transitions:
      done: g2
  g2:
    action:
      type: script
      command: sh
      args: [-c, "printf '{\"output\": \"ok\"}'"]
    outputs: [ok, rejected]
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
    outputs: [done]
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

/// The harness every agent session of the task started on, in order.
fn agent_harnesses(launch: &Launch) -> Vec<String> {
    launch
        .events()
        .into_iter()
        .filter_map(|event| match event.data {
            EventData::AgentSessionStarted { harness, .. } => Some(harness),
            _ => None,
        })
        .collect()
}

/// Waits for the task to stand at a gate other than `after`, and returns it.
async fn gate_after(fixture: &Fixture, task: &str, after: Option<&str>) -> String {
    wait_until_async(Duration::from_secs(30), "a gate", || {
        fixture
            .record(task)
            .pending_input_id()
            .is_some_and(|pending| Some(pending) != after)
    })
    .await;
    fixture
        .record(task)
        .pending_input_id()
        .expect("a gate")
        .to_string()
}

fn answer_gate(fixture: &Fixture, task: &str, pending: &str, key: &str) {
    fixture
        .service
        .submit_input(
            &fixture.owner(),
            task,
            SubmitInputRequest {
                pending_input_id: pending.to_string(),
                idempotency_key: key.to_string(),
                delivery: HumanInputDelivery::label("ok"),
            },
        )
        .expect("queue the answer");
}

/// Why the runner says the task keeps its harness, as last reported.
fn harness_unavailable(
    fixture: &Fixture,
    task: &str,
) -> Option<narsor::orchestration::HarnessUnavailable> {
    fixture
        .record(task)
        .runner
        .and_then(|runner| runner.snapshot.default_harness_unavailable)
}

/// AE3, R16b: a task created on `codex` follows the installation's default
/// from its first action; a change reaches the next action and the
/// conductor — even when the answer that carries it also carries the input
/// that starts that action.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_task_follows_the_default_harness_from_its_next_action() {
    if running_as_root() {
        return;
    }
    let mut fixture = Fixture::served().await;
    fixture.set_default(Some("claude-code"));
    let bin = fake_harnesses(&fixture.root, &["claude", "opencode", "codex"]);
    let spec = spec(
        GATED_WORKFLOW,
        TaskWorkspace::None,
        ResolvedEnvironment::harness_only("codex"),
    );
    fixture.create("task-1", spec.clone());
    let launch = Launch::new(&fixture, "task-1", &spec).with_bin_first(&bin);
    running_runner(&mut fixture, "task-1", &launch).await;
    let gate = gate_after(&fixture, "task-1", None).await;
    assert_eq!(agent_harnesses(&launch), vec!["claude-code"]);

    // Changed without telling the runner: the answer that wakes it for the
    // owner's input is the one that brings the new default as well.
    fixture.default.set(Some("opencode"));
    answer_gate(&fixture, "task-1", &gate, "answer-1");
    wait_until_async(Duration::from_secs(30), "completion", || {
        runner_status(&fixture, "task-1") == Some(ExecutionStatus::Completed)
    })
    .await;
    assert_eq!(agent_harnesses(&launch), vec!["claude-code", "opencode"]);

    let view = wait_for_view(&fixture, "task-1", "the conductor on opencode", |view| {
        view.harness.as_deref() == Some("opencode")
    })
    .await;
    assert!(view.available, "{view:?}");
    assert_eq!(harness_unavailable(&fixture, "task-1"), None);
    let applied = fs::read_to_string(launch.storage.join("narsor/harness.json")).unwrap();
    assert!(applied.contains("opencode"), "{applied}");
}

/// AE6, R16c: a default the backend does not run, or whose binary is not in
/// the task's environment, leaves the next action on the task's harness;
/// the owner sees why, as information — no problem, no failed step — and
/// the reason goes once a default the task can run comes back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_default_the_task_cannot_run_keeps_its_harness_and_says_why() {
    if running_as_root() {
        return;
    }
    let mut fixture = Fixture::served().await;
    fixture.set_default(Some("claude-code"));
    let bin = fake_harnesses(&fixture.root, &["claude"]);
    let spec = spec(
        THREE_STEP_WORKFLOW,
        TaskWorkspace::None,
        ResolvedEnvironment::harness_only("claude-code"),
    );
    fixture.create("task-1", spec.clone());
    // Only `claude` is on the runner's PATH.
    let mut launch = Launch::new(&fixture, "task-1", &spec);
    launch.path = only_on_path(&bin);
    running_runner(&mut fixture, "task-1", &launch).await;
    let first = gate_after(&fixture, "task-1", None).await;

    // The backend runs only claude-code.
    fixture.restrict_harnesses(&["claude-code"]);
    fixture.set_default(Some("opencode"));
    wait_until_async(Duration::from_secs(30), "the reason", || {
        harness_unavailable(&fixture, "task-1").is_some()
    })
    .await;
    let unavailable = harness_unavailable(&fixture, "task-1").unwrap();
    assert_eq!(unavailable.harness, "opencode");
    assert_eq!(unavailable.kept, "claude-code");
    assert!(
        unavailable.reason.contains("runs only claude-code"),
        "{unavailable:?}"
    );
    let summary = fixture.service.task(&fixture.owner(), "task-1").unwrap();
    let runner = summary.runner.expect("the runner reported");
    assert_eq!(runner.default_harness_unavailable, Some(unavailable));
    assert_eq!(runner.problem, None, "it is not a problem");
    assert_eq!(
        attention_kinds(&fixture, "task-1"),
        vec![AttentionKind::Question]
    );

    answer_gate(&fixture, "task-1", &first, "answer-1");
    let second = gate_after(&fixture, "task-1", Some(&first)).await;
    assert_eq!(agent_harnesses(&launch), vec!["claude-code", "claude-code"]);

    // The backend runs it now, but the binary is not here.
    fixture.restrict_harnesses(&[]);
    fixture.set_default(Some("opencode"));
    wait_until_async(Duration::from_secs(30), "the missing binary", || {
        harness_unavailable(&fixture, "task-1")
            .is_some_and(|unavailable| unavailable.reason.contains("not on PATH"))
    })
    .await;
    answer_gate(&fixture, "task-1", &second, "answer-2");
    wait_until_async(Duration::from_secs(30), "completion", || {
        runner_status(&fixture, "task-1") == Some(ExecutionStatus::Completed)
    })
    .await;
    assert_eq!(
        agent_harnesses(&launch),
        vec!["claude-code", "claude-code", "claude-code"],
        "no step failed or switched"
    );

    // Back to a default the task runs: the reason goes.
    fixture.set_default(Some("claude-code"));
    wait_until_async(Duration::from_secs(30), "the reason to go", || {
        harness_unavailable(&fixture, "task-1").is_none()
    })
    .await;
}

/// A task that starts — or resumes after a pause — while the default is
/// another than its own takes it from its first action; what it switched
/// to survives the pause, even when the default meanwhile became one its
/// backend does not run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_harness_switched_to_survives_a_pause_and_reaches_the_first_action() {
    if running_as_root() {
        return;
    }
    let mut fixture = Fixture::served().await;
    fixture.set_default(Some("opencode"));
    let bin = fake_harnesses(&fixture.root, &["claude", "opencode", "codex"]);
    let spec = spec(
        THREE_STEP_WORKFLOW,
        TaskWorkspace::None,
        ResolvedEnvironment::harness_only("claude-code"),
    );
    fixture.create("task-1", spec.clone());
    let launch = Launch::new(&fixture, "task-1", &spec).with_bin_first(&bin);
    let first_runner = running_runner(&mut fixture, "task-1", &launch).await;
    let first = gate_after(&fixture, "task-1", None).await;
    assert_eq!(
        agent_harnesses(&launch),
        vec!["opencode"],
        "the first action did not start on the fallback"
    );

    fixture.restrict_harnesses(&["claude-code", "opencode"]);
    fixture.set_default(Some("codex"));
    wait_until_async(Duration::from_secs(30), "the reason", || {
        harness_unavailable(&fixture, "task-1").is_some()
    })
    .await;

    fixture.report("task-1", Observation::RunnerReady);
    fixture.command("task-1", OwnerCommand::Pause, "pause-1");
    let status = fixture.wait_exit(first_runner, Duration::from_secs(30));
    assert!(status.success(), "{status:?}");
    fixture.report("task-1", Observation::PausedAtSafePoint);
    fixture.command("task-1", OwnerCommand::Resume, "resume-1");
    let credential = fixture.credential("task-1", "pod-2");
    fixture.spawn(&launch, "pod-2", &credential);
    wait_until_async(Duration::from_secs(30), "the resumed runner", || {
        fixture
            .record("task-1")
            .runner
            .is_some_and(|runner| runner.incarnation == "pod-2")
    })
    .await;

    answer_gate(&fixture, "task-1", &first, "answer-1");
    gate_after(&fixture, "task-1", Some(&first)).await;
    assert_eq!(agent_harnesses(&launch), vec!["opencode", "opencode"]);
    let unavailable = harness_unavailable(&fixture, "task-1").expect("still says why");
    assert_eq!(unavailable.harness, "codex");
    assert_eq!(unavailable.kept, "opencode");
}

/// A runner that starts on a harness it cannot run has no conductor; once
/// the control plane names one it can, the conductor works.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_conductor_comes_up_once_the_default_is_a_harness_the_task_can_run() {
    if running_as_root() {
        return;
    }
    let mut fixture = Fixture::served().await;
    let bin = conductor_claude(
        &fixture.root,
        &[(
            "reply",
            conductor_says(serde_json::json!({
                "decision": "reply", "message": "From claude.",
            })),
        )],
    );
    let spec = spec(
        LONG_STEP_WORKFLOW,
        TaskWorkspace::None,
        ResolvedEnvironment::harness_only("opencode"),
    );
    fixture.create("task-1", spec.clone());
    let mut launch = Launch::new(&fixture, "task-1", &spec);
    launch.path = only_on_path(&bin);
    running_runner(&mut fixture, "task-1", &launch).await;

    // No default to follow: the task stays on opencode, which is not here.
    let summary = fixture.service.task(&fixture.owner(), "task-1").unwrap();
    let runner = summary.runner.expect("the runner reported");
    assert!(!runner.conductor_available, "{runner:?}");

    fixture.set_default(Some("claude-code"));
    wait_until_async(Duration::from_secs(30), "the conductor", || {
        fixture
            .record("task-1")
            .runner
            .is_some_and(|runner| runner.snapshot.conductor_available)
    })
    .await;
    applied(
        &fixture,
        "task-1",
        OperatorCommand::ConductorChat {
            text: "hello".to_string(),
        },
        "chat-1",
    )
    .await;
    let view = wait_for_view(&fixture, "task-1", "claude's answer", |view| {
        view.chat.iter().any(|entry| entry.text == "From claude.")
    })
    .await;
    assert_eq!(view.harness.as_deref(), Some("claude-code"));
}
