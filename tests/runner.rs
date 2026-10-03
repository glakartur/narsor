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
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use chrono::Utc;
use narsor::controller_protocol::http::runner_routes;
use narsor::controller_protocol::{
    AcquireLeaseRequest, ConversationPageQuery, CreateEnrollmentRequest, EnrollRequest,
    OwnerCommandRequest, ProtocolService, ReportRequest, RunnerCredentialRequest,
    SubmitInputRequest, LOCAL_OWNER,
};
use narsor::domain::{
    BackendId, EventData, ExecutionStatus, GitIdentity, ResolvedEnvironment, ResourceProfile,
    SecretDelivery, SecretReference, TaskId, TaskLaunchSpec, TaskWorkspace,
};
use narsor::engine::HumanInputDelivery;
use narsor::orchestration::{
    ControlPlane, NewTask, Observation, ObservedSummary, OwnerCommand, OwnerRef, TaskRecord,
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
        let service = Arc::new(ProtocolService::new(
            control,
            environment_support::seeded_resolver(&state),
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
        }
    }

    /// Starts serving the runner routes on the fixture's address.
    async fn serve(&mut self) {
        let listener = tokio::net::TcpListener::bind(self.addr)
            .await
            .expect("bind the control plane");
        self.serve_on(listener);
    }

    fn serve_on(&mut self, listener: tokio::net::TcpListener) {
        let router: Router = runner_routes(Arc::clone(&self.service));
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
    // task pinned at creation.
    let homes = fs::read_to_string(home.join("claude-homes")).unwrap();
    assert_eq!(
        homes.lines().collect::<Vec<_>>(),
        vec![home.display().to_string(); 2]
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
    assert!(!launch.workspace().exists(), "nothing was cloned");
    assert!(launch.execution_id().is_none(), "no work started");
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
