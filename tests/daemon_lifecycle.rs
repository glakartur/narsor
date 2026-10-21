//! Integration tests for the daemon process and `daemon stop` (U2).
//!
//! # What is, and is not, covered here
//!
//! `POST /tasks` and every other mutating REST endpoint are U3; SSE is U4.
//! Both are out of scope for the daemon process itself in this unit, which
//! is why the "AE1-shape" test here proves only the part of AE1 that is
//! actually U2's to prove: that the daemon detaches into a session of its
//! own (`setsid()`) and survives independently of whatever spawned it — the
//! mechanism the plan's External References calls out as the actual
//! guarantee behind AE1 (a killed client must not take the daemon down with
//! it). The full AE1 acceptance example — an action running *through* the
//! daemon, driven by a client that starts it over the network and then
//! dies, with the conversation log observably still growing — needs
//! `POST /tasks` (U3) to start that action against the daemon's own `Engine`
//! from outside the process; it is deferred to U8's end-to-end test, per
//! this unit's own scope note.
//!
//! Similarly, the "timeout exceeded" branch of the graceful-stop drain
//! (`cli::daemon::drain_active_runs` giving up and warning rather than
//! draining) is not exercised by a dedicated test here: doing so needs a
//! script action engineered to never finish, which would leak a live child
//! process past the end of the test unless it were reaped by hand — the
//! "drains within the timeout" branch below already exercises the same
//! pause/poll loop end to end.

mod engine_support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use narsor_engine::conductor::{
    ChatAuthor, ChatEntry, ChatEntryKind, ConductorState, ConductorStore, PendingInvocation,
    QueuedRequest,
};
use narsor_engine::domain::{
    ConductorOutcome, ConductorPurpose, ConductorTrigger, EventData, ExecutionStatus, Workspace,
};
use narsor_engine::engine::{Engine, OutputRejection};
use narsor_engine::persistence::{DaemonInfo, ExecutionStore};

/// A temporary directory that deletes itself when it goes out of scope.
///
/// Stands in for the `tempfile` crate, which is not a dependency of this
/// crate (mirrors `tests/cli_smoke.rs`, `tests/api.rs`, ...).
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-daemon-{}", uuid::Uuid::new_v4()));
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

fn daemon_file(root: &Path) -> PathBuf {
    root.join("daemon.json")
}

/// A spawned daemon that never outlives its test: when the guard is dropped
/// — also while an assertion unwinds — the daemon is killed and reaped, and
/// the runners under its root (which outlive a daemon by design) are
/// stopped. It derefs to the [`Child`].
struct DaemonGuard {
    child: Child,
    _processes: ProcessesUnder,
}

impl DaemonGuard {
    fn new(root: &Path, child: Child) -> Self {
        Self {
            child,
            _processes: ProcessesUnder {
                root: root.to_path_buf(),
            },
        }
    }
}

impl std::ops::Deref for DaemonGuard {
    type Target = Child;

    fn deref(&self) -> &Child {
        &self.child
    }
}

impl std::ops::DerefMut for DaemonGuard {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawns `narsor-server --root <root>` as a plain (non-detached) child, so
/// the test itself can `wait()` on it directly.
fn spawn_daemon(root: &Path) -> DaemonGuard {
    DaemonGuard::new(
        root,
        Command::new(engine_support::workspace_binary("narsor-server"))
            .arg("--root")
            .arg(root)
            .env("PATH", engine_support::fake_claude_first_on_path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the daemon"),
    )
}

/// Runs `narsor --root <root> daemon stop` to completion; returns its exit
/// code and combined stdout+stderr.
fn stop_daemon_cli(root: &Path) -> (i32, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_narsor-cli"))
        .arg("--root")
        .arg(root)
        .arg("daemon")
        .arg("stop")
        .env(
            "NARSOR_TEST_DAEMON_EXE",
            engine_support::workspace_binary("narsor-server"),
        )
        .output()
        .expect("run `daemon stop`");
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.code().expect("normal exit"), combined)
}

/// Polls for the discovery file to appear, up to `timeout`.
fn wait_for_daemon_info(root: &Path, timeout: Duration) -> DaemonInfo {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(info) = DaemonInfo::read(&daemon_file(root)) {
            return info;
        }
        if Instant::now() >= deadline {
            panic!(
                "daemon.json never appeared under {} within {:?}",
                root.display(),
                timeout
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether a PID is alive right now (`kill(pid, 0)`), same check as
/// [`DaemonInfo::is_alive`] but usable on a bare PID.
fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 sends nothing; it only probes existence/visibility.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
        return false;
    }
    // An exited daemon this process spawned and never reaped stays a zombie,
    // which `kill(pid, 0)` still sees. A zombie has stopped running.
    !std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(')')
                .map(|(_, rest)| rest.trim_start().starts_with('Z'))
        })
        .unwrap_or(false)
}

fn send_sigterm(pid: u32) {
    // SAFETY: a plain signal-send to a PID this test just spawned.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

/// Waits for a spawned child to exit, up to `timeout`; panics (killing the
/// child first) if it does not.
fn wait_for_exit(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("poll child status") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("process did not exit within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------
// Happy path: starts, discoverable, serves a request, stops on SIGTERM
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "starts a narsor daemon; run with --ignored"]
async fn daemon_starts_writes_discovery_file_and_serves_a_request() {
    let root = TempRoot::new();
    let mut child = spawn_daemon(root.path());

    let info = wait_for_daemon_info(root.path(), Duration::from_secs(10));
    assert_eq!(info.pid, child.id());
    assert!(pid_alive(info.pid));

    // A simple authenticated GET against the pre-existing, still-minimal
    // read-only surface (mutating endpoints are U3) proves the HTTP server
    // itself is up and wired to the token the discovery file names — a
    // made-up execution id is expected to 404, not to fail to connect at
    // all.
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let response = http
        .get(format!(
            "http://{}/executions/no-such-execution/events",
            info.addr
        ))
        .bearer_auth(&info.token)
        .send()
        .await
        .expect("the daemon's HTTP server answers");
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);

    // An unauthenticated request is refused, not silently allowed.
    let unauthenticated = http
        .get(format!(
            "http://{}/executions/no-such-execution/events",
            info.addr
        ))
        .send()
        .await
        .expect("the daemon's HTTP server answers");
    assert_eq!(unauthenticated.status(), reqwest::StatusCode::UNAUTHORIZED);

    send_sigterm(child.id());
    let status = wait_for_exit(&mut child, Duration::from_secs(10));
    assert!(status.success(), "daemon should exit cleanly on SIGTERM");
    assert!(
        !daemon_file(root.path()).exists(),
        "the discovery file must be removed on a graceful shutdown"
    );
}

// ---------------------------------------------------------------------------
// The daemon-owned local controller (U2, KTD4)
// ---------------------------------------------------------------------------

/// The `local` backend's lease as the control plane stores it, read from the
/// outside the way an operator's tooling would.
fn local_lease(root: &Path) -> Option<narsor_engine::orchestration::BackendLease> {
    narsor_engine::orchestration::ControlPlane::new(narsor_engine::persistence::StatePaths::new(
        root,
    ))
    .lease(&narsor_engine::domain::BackendId::parse("local").unwrap())
    .expect("read the lease")
}

fn wait_for_lease(
    root: &Path,
    what: &str,
    mut accept: impl FnMut(&Option<narsor_engine::orchestration::BackendLease>) -> bool,
) -> Option<narsor_engine::orchestration::BackendLease> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let lease = local_lease(root);
        if accept(&lease) {
            return lease;
        }
        assert!(Instant::now() < deadline, "the local lease never {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A fresh daemon enrolls a controller for its built-in `local` backend and
/// takes the lease before it is discoverable; a graceful stop releases the
/// lease at once, and the next daemon reuses the same identity, taking the
/// lease at a later epoch.
#[test]
#[ignore = "starts a narsor daemon; run with --ignored"]
fn the_local_controller_holds_the_lease_and_a_restart_reuses_its_identity() {
    let root = TempRoot::new();
    let credential = root.path().join("local-controller/credential");

    let mut first = spawn_daemon(root.path());
    wait_for_daemon_info(root.path(), Duration::from_secs(15));
    assert!(
        credential.is_file(),
        "the controller was enrolled at startup"
    );
    let enrolled = fs::read_to_string(&credential).expect("read the credential");
    let held = wait_for_lease(root.path(), "became active", |lease| {
        lease
            .as_ref()
            .is_some_and(|lease| lease.expires_at > chrono::Utc::now())
    })
    .expect("a lease");

    send_sigterm(first.id());
    assert!(wait_for_exit(&mut first, Duration::from_secs(20)).success());
    let released = local_lease(root.path());
    assert!(
        released
            .as_ref()
            .is_none_or(|lease| lease.expires_at <= chrono::Utc::now()),
        "the lease is given up on a clean shutdown, not left to expire: {released:?}"
    );

    let mut second = spawn_daemon(root.path());
    wait_for_daemon_info(root.path(), Duration::from_secs(15));
    let reacquired = wait_for_lease(root.path(), "was reacquired", |lease| {
        lease
            .as_ref()
            .is_some_and(|lease| lease.expires_at > chrono::Utc::now())
    })
    .expect("a lease");
    assert_eq!(
        fs::read_to_string(&credential).expect("read the credential"),
        enrolled,
        "the restarted daemon reuses its controller credential"
    );
    assert_eq!(reacquired.controller_id, held.controller_id);
    assert!(
        reacquired.epoch > held.epoch,
        "a new epoch fences the old one"
    );

    send_sigterm(second.id());
    assert!(wait_for_exit(&mut second, Duration::from_secs(20)).success());
}

/// Queued and admitted local tasks survive a daemon restart without a second
/// copy of anything: the same task ids, one storage each, and no task that
/// the first daemon accepted is created again.
#[tokio::test]
#[ignore = "starts a narsor daemon; run with --ignored"]
async fn a_restart_keeps_queued_and_admitted_tasks_without_duplicating_them() {
    let root = TempRoot::new();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cli");
    let workflow = fixtures.join("greet.yaml").display().to_string();
    let script = fixtures.join("announce.sh").display().to_string();
    let http = reqwest::Client::new();

    let mut first = spawn_daemon(root.path());
    let info = wait_for_daemon_info(root.path(), Duration::from_secs(15));
    // The workflow stops at a human gate, so the first task keeps the only
    // slot and the second one stays queued behind it.
    for task_id in ["restart-one", "restart-two"] {
        let response = http
            .post(format!("http://{}/tasks", info.addr))
            .bearer_auth(&info.token)
            .json(&serde_json::json!({
                "task_id": task_id,
                "workflow": workflow,
                "inputs": { "greeting": "hello", "script_path": script }
            }))
            .send()
            .await
            .expect("create a task");
        assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    }

    let storage = root.path().join("local-controller/storage");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !storage.join("restart-one/state").is_dir() {
        assert!(
            Instant::now() < deadline,
            "the first task was never admitted"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    send_sigterm(first.id());
    assert!(wait_for_exit(&mut first, Duration::from_secs(30)).success());

    let mut second = spawn_daemon(root.path());
    let info = wait_for_daemon_info(root.path(), Duration::from_secs(15));
    let client = narsor::api::ApiClient::new(info.addr, info.token.clone());
    let tasks = client.control_tasks().await.expect("list tasks").tasks;
    let mut ids = tasks
        .iter()
        .map(|task| task.task_id.to_string())
        .collect::<Vec<_>>();
    ids.sort();
    assert_eq!(
        ids,
        ["restart-one", "restart-two"],
        "no task appeared or vanished"
    );

    // Replaying a create the first daemon already answered returns that task.
    let replay = http
        .post(format!("http://{}/tasks", info.addr))
        .bearer_auth(&info.token)
        .json(&serde_json::json!({
            "task_id": "restart-one",
            "workflow": workflow,
            "inputs": { "greeting": "hello", "script_path": script }
        }))
        .send()
        .await
        .expect("replay the create");
    assert_eq!(replay.status(), reqwest::StatusCode::ACCEPTED);
    let after = client.control_tasks().await.expect("list tasks").tasks;
    assert_eq!(after.len(), 2, "the replay created nothing");

    // One storage per task that was ever admitted — never two for one task.
    let mut storages = fs::read_dir(&storage)
        .expect("task storages")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    storages.sort();
    assert!(
        storages.iter().all(|name| name.starts_with("restart-")) && storages.len() <= 2,
        "unexpected task storages: {storages:?}"
    );

    send_sigterm(second.id());
    assert!(wait_for_exit(&mut second, Duration::from_secs(30)).success());
}

// ---------------------------------------------------------------------------
// `daemon stop`
// ---------------------------------------------------------------------------

#[test]
#[ignore = "starts a narsor daemon; run with --ignored"]
fn daemon_stop_signals_the_running_daemon_and_it_exits() {
    let root = TempRoot::new();
    let mut child = spawn_daemon(root.path());
    let info = wait_for_daemon_info(root.path(), Duration::from_secs(10));
    assert!(pid_alive(info.pid));

    let (code, output) = stop_daemon_cli(root.path());
    assert_eq!(code, 0, "output:\n{output}");
    assert!(output.contains("Sent SIGTERM"), "output:\n{output}");

    let status = wait_for_exit(&mut child, Duration::from_secs(10));
    assert!(status.success());
    assert!(!daemon_file(root.path()).exists());
}

#[test]
fn daemon_stop_with_no_discovery_file_reports_and_exits_nonzero() {
    let root = TempRoot::new();

    let (code, output) = stop_daemon_cli(root.path());

    assert_ne!(code, 0);
    assert!(output.contains("No daemon is running"), "output:\n{output}");
}

#[test]
fn daemon_stop_with_a_stale_pid_cleans_up_and_exits_nonzero() {
    let root = TempRoot::new();

    // A PID guaranteed to be dead: spawn and reap `true`.
    let mut short_lived = Command::new("true").spawn().expect("spawn `true`");
    let dead_pid = short_lived.id();
    short_lived.wait().expect("reap it");

    let info = DaemonInfo {
        pid: dead_pid,
        addr: "127.0.0.1:1".parse().unwrap(),
        token: "stale-token".to_string(),
        started_at: chrono::Utc::now(),
    };
    info.write(&daemon_file(root.path()))
        .expect("write the stale discovery file");

    let (code, output) = stop_daemon_cli(root.path());

    assert_ne!(code, 0);
    assert!(output.contains("stale"), "output:\n{output}");
    assert!(
        !daemon_file(root.path()).exists(),
        "the stale discovery file must be cleaned up"
    );
}

// ---------------------------------------------------------------------------
// Startup error: nothing half-written (stands in for a bind failure — see
// module doc for why binding 127.0.0.1:0 itself cannot be forced to fail
// deterministically here)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "starts a narsor daemon; run with --ignored"]
fn a_startup_error_leaves_no_half_written_discovery_file() {
    let root = TempRoot::new();
    // Occupy the discovery file's path with a directory, so `DaemonInfo::write`
    // fails cleanly (a file cannot be renamed onto a directory) after the
    // server has already bound successfully — the closest reproducible
    // "clean startup error" this process can trigger on demand.
    fs::create_dir_all(daemon_file(root.path())).expect("occupy the discovery file's path");

    let mut child = spawn_daemon(root.path());
    let status = wait_for_exit(&mut child, Duration::from_secs(10));

    assert!(!status.success(), "startup must fail cleanly, not hang");
    assert!(
        daemon_file(root.path()).is_dir(),
        "the path this test occupied must be untouched, not replaced with a half-written file"
    );
    let leftovers: Vec<_> = fs::read_dir(root.path())
        .expect("read the root")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name())
        .collect();
    assert!(
        !leftovers
            .iter()
            .any(|name| name.to_string_lossy().ends_with(".tmp")),
        "no orphaned temp file left behind: {leftovers:?}"
    );
}

// ---------------------------------------------------------------------------
// AE6: SIGKILL, then a fresh daemon start reconciles via D23
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "starts a narsor daemon; run with --ignored"]
async fn a_fresh_daemon_reconciles_an_execution_left_running_by_a_killed_predecessor() {
    let root = TempRoot::new();
    fs::create_dir_all(root.path().join("work")).expect("create a work dir");

    // Seed the store exactly as `tests/engine_recovery.rs` does for the
    // engine-level version of this scenario: a `state.json` that still says
    // `Running`, with the event log stopping mid-action — what a killed
    // process (this unit's daemon, once it exists) would have left behind.
    let store = ExecutionStore::new(root.path());
    let engine = Engine::new(store.clone());
    let definition = engine_support::workflow("two_steps");
    let workspace = Workspace::local_folder(root.path().join("work").display().to_string());
    let execution = engine
        .start(&definition, serde_json::json!({}), workspace)
        .expect("seed the execution");

    let log = store.event_log(&execution.id);
    log.append_next(
        &execution.id,
        EventData::StateEntered {
            state_id: "first".to_string(),
        },
    )
    .expect("append StateEntered");
    log.append_next(
        &execution.id,
        EventData::ActionStarted {
            action_execution_id: "action-under-way".to_string(),
            state_id: "first".to_string(),
            action_type: "script".to_string(),
        },
    )
    .expect("append ActionStarted");

    // A real daemon process, started fresh over this same store: its
    // startup recovery (moved here from `tui::mod::run` in this unit, R8)
    // must mark the execution `Unknown` before the discovery file appears —
    // `run_daemon` runs recovery before binding/writing it.
    let mut child = spawn_daemon(root.path());
    let _info = wait_for_daemon_info(root.path(), Duration::from_secs(10));

    let stored = store
        .load_execution(&execution.id)
        .expect("load the execution");
    assert_eq!(
        stored.status,
        ExecutionStatus::Unknown,
        "D23 recovery must have run at daemon startup"
    );

    let events = log.read_all().expect("read the event log");
    assert!(
        events
            .iter()
            .any(|event| matches!(event.data, EventData::Error { .. })),
        "recovery must have appended an Error event: {events:?}"
    );

    send_sigterm(child.id());
    wait_for_exit(&mut child, Duration::from_secs(10));
}

// ---------------------------------------------------------------------------
// Graceful stop: `Engine::pause` is called and the wait drains
// ---------------------------------------------------------------------------

#[tokio::test]
async fn drain_active_runs_pauses_the_active_run_and_waits_for_it_to_finish() {
    let root = TempRoot::new();
    let work = root.path().join("work");
    fs::create_dir_all(&work).expect("create a work dir");
    for entry in
        fs::read_dir(engine_support::fixtures().join("scripts")).expect("read fixture scripts")
    {
        let entry = entry.expect("fixture script");
        fs::copy(entry.path(), work.join(entry.file_name())).expect("copy fixture script");
    }
    let store = ExecutionStore::new(root.path());
    let engine = std::sync::Arc::new(Engine::new(store));
    // `pause` (D34): two script states, the first slow enough to still be
    // running when the drain call arrives — the same fixture and pattern
    // `tests/engine_execution.rs`'s
    // `pausing_lets_the_running_action_finish_and_stops_the_next_one` uses.
    let definition = std::sync::Arc::new(engine_support::workflow("pause"));
    let workspace = Workspace::local_folder(work.display().to_string());

    let execution = engine
        .start(&definition, serde_json::json!({}), workspace)
        .expect("start the workflow");

    let running = {
        let engine = std::sync::Arc::clone(&engine);
        let definition = std::sync::Arc::clone(&definition);
        let execution_id = execution.id.clone();
        tokio::spawn(async move { engine.run(&definition, &execution_id).await })
    };

    // Give the first (slow) action time to actually start before draining.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        engine.is_active(&execution.id),
        "the run must still be in flight for this test to mean anything"
    );

    let drained = narsor_server::runtime::drain_active_runs(&engine, Duration::from_secs(5)).await;

    assert!(
        drained,
        "the single slow action should finish well inside the timeout"
    );
    assert!(engine.active_run_ids().is_empty());

    let execution = running.await.expect("join the run").expect("run cleanly");
    assert_eq!(
        execution.status,
        ExecutionStatus::Paused,
        "Engine::pause was honoured at the next step boundary (D34)"
    );
}

// ---------------------------------------------------------------------------
// Regression: the HTTP server must stop accepting new mutating requests
// before the active-run drain, not after (found in code review, U8 follow-up)
// ---------------------------------------------------------------------------

/// Writes a workflow with one script action that sleeps `sleep_seconds`
/// before succeeding, and returns its absolute path — passed straight to
/// `POST /tasks` as the `workflow` field, exactly as `narsor run
/// <path-to-file>` would (`WorkflowResolver::resolve` accepts an existing
/// file path directly, regardless of the daemon's own catalog directory).
fn write_sleeping_workflow(root: &Path, sleep_seconds: u64) -> PathBuf {
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scripts/sleep_then_emit.sh");
    let yaml = format!(
        r#"
name: daemon-shutdown-race-fixture
version: 1
description: One script action that sleeps before succeeding.

states:
  slow:
    action:
      type: script
      command: "{command}"
      args:
        - "{seconds}"
        - '{{"choice": "done"}}'
    choices:
      - done
    transitions:
      done: completed

  completed:
    description: Terminal.

terminal_states:
  - completed
"#,
        command = script.display(),
        seconds = sleep_seconds,
    );
    let path = root.join(format!("sleeping-{sleep_seconds}.yaml"));
    fs::write(&path, yaml).expect("write the workflow definition");
    path
}

/// Regression test for a real bug found in code review: `run_daemon` used to
/// call `drain_active_runs` — which can legitimately take up to
/// `SHUTDOWN_DRAIN_TIMEOUT` (several seconds) — *before* telling the HTTP
/// server to stop accepting new connections. Every mutating endpoint stayed
/// reachable throughout that whole window, so a fresh `POST /tasks` landing
/// mid-drain would start a background run that the drain's own snapshot
/// (already taken) never asked to pause — silently defeating the point of
/// draining at all.
///
/// This starts one long-running action (to give the drain real time to
/// work with), sends `SIGTERM`, and — comfortably after the near-instant
/// `stop_accepting()` call but comfortably before the multi-second drain
/// finishes — asserts a second `POST /tasks` is refused at the connection
/// level, not accepted.
#[tokio::test]
#[ignore = "starts a narsor daemon; run with --ignored"]
async fn new_mutating_requests_are_refused_once_shutdown_begins_not_only_after_the_drain() {
    let root = TempRoot::new();
    let mut child = spawn_daemon(root.path());
    let info = wait_for_daemon_info(root.path(), Duration::from_secs(10));

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();

    let long_workflow = write_sleeping_workflow(root.path(), 5);
    let start = http
        .post(format!("http://{}/tasks", info.addr))
        .bearer_auth(&info.token)
        .json(&serde_json::json!({
            "task_id": "shutdown-long-task",
            "workflow": long_workflow.display().to_string(),
            "inputs": {}
        }))
        .send()
        .await
        .expect("start the long-running task");
    assert_eq!(start.status(), reqwest::StatusCode::ACCEPTED);

    // Let the action actually begin before shutting down, so
    // `drain_active_runs` has a real, multi-second wait ahead of it.
    tokio::time::sleep(Duration::from_millis(300)).await;

    send_sigterm(child.id());

    // Comfortably longer than the near-instant `stop_accepting()` call this
    // signal triggers, comfortably shorter than the 5-second action the
    // drain is waiting on — squarely inside the window the bug this test
    // guards against would have kept accepting new connections in.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let short_workflow = write_sleeping_workflow(root.path(), 0);
    let second = http
        .post(format!("http://{}/tasks", info.addr))
        .bearer_auth(&info.token)
        .json(&serde_json::json!({
            "task_id": "shutdown-short-task",
            "workflow": short_workflow.display().to_string(),
            "inputs": {}
        }))
        .send()
        .await;

    match second {
        Err(error) => assert!(
            error.is_connect() || error.is_request() || error.is_timeout(),
            "expected a connection-level refusal, got a different reqwest error: {error}"
        ),
        Ok(response) => panic!(
            "a new mutating request must be refused once shutdown has begun \
             (drain_active_runs took its snapshot already) — instead got {}",
            response.status()
        ),
    }

    let status = wait_for_exit(&mut child, Duration::from_secs(15));
    assert!(status.success(), "daemon should still exit cleanly");
}

// ---------------------------------------------------------------------------
// The startup hook (D55): what recovery found safely stopped is carried on
// ---------------------------------------------------------------------------

/// Seeds a store with a started `two_steps` execution and returns it.
fn seed_two_steps(root: &Path) -> (ExecutionStore, narsor_engine::domain::WorkflowExecution) {
    fs::create_dir_all(root.join("work")).expect("create a work dir");
    let store = ExecutionStore::new(root);
    let engine = Engine::new(store.clone());
    let definition = engine_support::workflow("two_steps");
    let workspace = Workspace::local_folder(root.join("work").display().to_string());
    let execution = engine
        .start(&definition, serde_json::json!({}), workspace)
        .expect("seed the execution");
    (store, execution)
}

/// Polls `condition` until it holds, up to `timeout`.
async fn wait_for(what: &str, timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        if Instant::now() >= deadline {
            panic!("timed out after {timeout:?} waiting until {what}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Installs the fake harness of `tests/fixtures/harness` as `claude` in a
/// directory of its own, answering every call with `text`, and returns that
/// directory — to put first on a daemon's `PATH`, so the conductor never
/// reaches a real harness.
fn fake_claude(dir: &Path, text: &str) -> PathBuf {
    fs::create_dir_all(dir).expect("create the harness directory");
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/harness/fake_harness.sh");
    let temp = dir.join(format!(".claude.{}.tmp", uuid::Uuid::new_v4()));
    fs::copy(&source, &temp).expect("copy the fake harness");
    fs::rename(&temp, dir.join("claude")).expect("move the fake harness into place");
    let answer = serde_json::json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "session_id": "conductor-session",
        "result": text,
    });
    fs::write(dir.join("stdout"), format!("{answer}\n")).expect("write the answer");
    dir.to_path_buf()
}

/// Restart matrix: a chat message the conductor had not answered when the
/// daemon stopped is answered after the next start, by the conductor run on
/// the (fake) default harness.
#[tokio::test]
#[ignore = "starts a narsor daemon; run with --ignored"]
async fn a_fresh_daemon_answers_a_chat_message_left_unanswered() {
    let root = TempRoot::new();
    let bin = fake_claude(
        &root.path().join("bin"),
        r#"{"decision": "reply", "message": "It is about to run its first step."}"#,
    );
    let (store, execution) = seed_two_steps(root.path());
    let conductor = ConductorStore::new(store.paths(), &execution.id);
    let message = conductor
        .append_chat(ChatEntry::new(
            ChatAuthor::Operator,
            ChatEntryKind::Message,
            "what is the task doing?",
        ))
        .expect("seed an unanswered message");

    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = Command::new(engine_support::workspace_binary("narsor-server"))
        .arg("--root")
        .arg(root.path())
        .env("PATH", path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the daemon");
    let info = wait_for_daemon_info(root.path(), Duration::from_secs(10));
    let client = narsor::api::ApiClient::new(info.addr, &info.token);

    let deadline = Instant::now() + Duration::from_secs(10);
    let chat = loop {
        let chat = client
            .conductor_chat(&execution.id, 0)
            .await
            .expect("read the chat over the API");
        if chat.entries.len() >= 2 || Instant::now() >= deadline {
            break chat;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(chat.entries.len(), 2, "{:?}", chat.entries);
    assert_eq!(chat.entries[0], message.clone().into());
    let answer = &chat.entries[1];
    assert_eq!(answer.in_reply_to.as_deref(), Some(message.id.as_str()));
    assert_eq!(answer.author, narsor::api::ChatAuthorDto::Conductor);
    assert_eq!(answer.kind, narsor::api::ChatEntryKindDto::Message);
    assert_eq!(answer.text, "It is about to run its first step.");

    let log = store.event_log(&execution.id);
    wait_for(
        "the invocation was recorded",
        Duration::from_secs(5),
        || {
            log.read_all().expect("read the log").iter().any(|event| {
                matches!(
                    event.data,
                    EventData::ConductorFinished {
                        outcome: ConductorOutcome::Completed,
                        ..
                    }
                )
            })
        },
    )
    .await;
    assert!(log
        .read_all()
        .expect("read the log")
        .iter()
        .any(|event| matches!(
            event.data,
            EventData::ConductorStarted {
                trigger: ConductorTrigger::Chat,
                purpose: ConductorPurpose::ReplyToChat,
                ..
            }
        )));

    let state = client
        .conductor_state(&execution.id)
        .await
        .expect("read the conductor's state over the API");
    assert!(state.unanswered_chat.is_empty(), "{state:?}");
    assert_eq!(state.pending, None);

    send_sigterm(child.id());
    wait_for_exit(&mut child, Duration::from_secs(10));
}

/// Restart matrix: `Running` with the conductor's F1 marker and no open
/// attempt stopped in the conductor, not in an action — a fresh daemon
/// leaves it `Running`, not `Unknown`.
#[tokio::test]
#[ignore = "starts a narsor daemon; run with --ignored"]
async fn a_fresh_daemon_does_not_mark_an_execution_stopped_in_the_conductor_unknown() {
    let root = TempRoot::new();
    let (store, execution) = seed_two_steps(root.path());
    let log = store.event_log(&execution.id);
    for data in [
        EventData::StateEntered {
            state_id: "first".to_string(),
        },
        EventData::ActionStarted {
            action_execution_id: "attempt-rejected".to_string(),
            state_id: "first".to_string(),
            action_type: "agent".to_string(),
        },
        EventData::ActionOutputRejected {
            action_execution_id: "attempt-rejected".to_string(),
            state_id: "first".to_string(),
            reason: "no output label".to_string(),
            answer_tail: None,
        },
    ] {
        log.append_next(&execution.id, data).expect("append");
    }
    let conductor = ConductorStore::new(store.paths(), &execution.id);
    let request = QueuedRequest::new(
        ConductorTrigger::OutputRescue,
        ConductorPurpose::ChooseOutput,
    )
    .with_episode("attempt-rejected");
    let mut state = ConductorState {
        pending: Some(PendingInvocation {
            invocation_id: "invocation-cut-short".to_string(),
            request: request.clone(),
            started_at: chrono::Utc::now(),
        }),
        ..ConductorState::default()
    };
    state.queue.push(request);
    conductor.save_state(&state).expect("seed the marker");

    let mut child = spawn_daemon(root.path());
    let _info = wait_for_daemon_info(root.path(), Duration::from_secs(10));

    let stored = store.load_execution(&execution.id).expect("load");
    assert_eq!(stored.status, ExecutionStatus::Running);
    assert!(
        !log.read_all()
            .expect("read the log")
            .iter()
            .any(|event| matches!(event.data, EventData::Error { .. })),
        "no interrupted-action error was recorded"
    );
    // The execution recorded no rescue of its own (the marker alone is not
    // enough to carry a step on), so there is nothing to invoke again: the
    // marker is stale and discarded, and the conductor never runs.
    let left = conductor.load_state().expect("load the conductor's state");
    assert_eq!(left.pending, None, "{left:?}");
    assert!(left.queue.is_empty(), "{left:?}");
    assert!(
        !log.read_all()
            .expect("read the log")
            .iter()
            .any(|event| matches!(event.data, EventData::ConductorStarted { .. })),
        "the conductor was not invoked"
    );

    send_sigterm(child.id());
    wait_for_exit(&mut child, Duration::from_secs(10));
}

/// Installs the fake harness as `claude` in `bin`, so a daemon whose `PATH`
/// starts there runs it for `claude-code`. Copied under a temporary name and
/// renamed into place (see `FakeHarness::install` in `tests/actions_agent.rs`
/// for the ETXTBSY reason).
fn install_fake_claude(bin: &Path) {
    fs::create_dir_all(bin).expect("create the bin dir");
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/harness/fake_harness.sh");
    let temp = bin.join(format!(".claude.{}.tmp", uuid::Uuid::new_v4()));
    fs::copy(&source, &temp).expect("copy the fake harness");
    fs::rename(&temp, bin.join("claude")).expect("move the fake harness into place");
}

const REPROMPT_WORKFLOW: &str = r#"
name: daemon-reprompt
version: 1
states:
  work:
    action:
      type: agent
      harness: claude-code
      prompt: "Implement the feature."
    choices:
      - label: implemented
        description: The feature is implemented.
      - label: needs_human_decision
        description: A person has to decide first.
    transitions:
      implemented: done
      needs_human_decision: stopped
  done:
    description: Success.
  stopped:
    description: Failure.
terminal_states:
  done: completed
  stopped: failed
"#;

/// Restart matrix (U4): a daemon that stopped between a rejected output and
/// its reprompt left the attempt closed and the execution `Running`; the
/// next start spawns a pass that asks the same session again, read-only,
/// instead of replaying the attempt.
#[tokio::test]
#[ignore = "starts a narsor daemon; run with --ignored"]
async fn a_fresh_daemon_continues_a_reprompt_the_restart_interrupted() {
    let root = TempRoot::new();
    let bin = root.path().join("bin");
    install_fake_claude(&bin);
    fs::write(
        bin.join("stdout.1"),
        serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "session_id": "fake-session-1",
            "result": r#"{"choice":"implemented"}"#,
        })
        .to_string(),
    )
    .expect("write the harness answer");

    let work = root.path().join("work");
    fs::create_dir_all(&work).expect("create a work dir");
    let definition_path = root.path().join("reprompt.yaml");
    fs::write(&definition_path, REPROMPT_WORKFLOW).expect("write the workflow");
    let definition = narsor_engine::workflow::load_from_file(&definition_path).expect("load it");
    let store = ExecutionStore::new(root.path());
    let engine = Engine::new(store.clone());
    let mut execution = engine
        .start(
            &definition,
            serde_json::json!({}),
            Workspace::local_folder(work.display().to_string()),
        )
        .expect("seed the execution");
    fs::write(
        store.paths().definition_file(&execution.id),
        REPROMPT_WORKFLOW,
    )
    .expect("pin the definition");

    let log = store.event_log(&execution.id);
    for data in [
        EventData::StateEntered {
            state_id: "work".to_string(),
        },
        EventData::ActionStarted {
            action_execution_id: "attempt-rejected".to_string(),
            state_id: "work".to_string(),
            action_type: "agent".to_string(),
        },
        EventData::ActionOutputRejected {
            action_execution_id: "attempt-rejected".to_string(),
            state_id: "work".to_string(),
            reason: "the answer has no 'output'".to_string(),
            answer_tail: Some("Done.".to_string()),
        },
    ] {
        log.append_next(&execution.id, data).expect("append");
    }
    let rejection = OutputRejection {
        state_id: "work".to_string(),
        action_execution_id: "attempt-rejected".to_string(),
        reason: "the answer has no 'output'".to_string(),
        answer_tail: Some("Done.".to_string()),
        reply: None,
        harness: Some("claude-code".to_string()),
        harness_session_id: Some("fake-session-1".to_string()),
    };
    execution
        .metadata
        .as_object_mut()
        .expect("metadata is an object")
        .insert(
            "engine".to_string(),
            serde_json::json!({ "output_rejection": rejection }),
        );
    execution.current_state = Some("work".to_string());
    store
        .save_execution(&execution)
        .expect("save the execution");

    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = Command::new(engine_support::workspace_binary("narsor-server"))
        .arg("--root")
        .arg(root.path())
        .env("PATH", path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the daemon");
    let _info = wait_for_daemon_info(root.path(), Duration::from_secs(10));

    wait_for(
        "the reprompted execution completed",
        Duration::from_secs(15),
        || {
            store
                .load_execution(&execution.id)
                .map(|stored| stored.status == ExecutionStatus::Completed)
                .unwrap_or(false)
        },
    )
    .await;

    let count = fs::read_to_string(bin.join("count")).expect("the harness ran");
    assert_eq!(count.trim(), "1", "one reprompt, no replay of the attempt");
    let args = fs::read_to_string(bin.join("args.1")).expect("the harness's arguments");
    assert!(
        args.contains("--resume\n--%ARG%--\nfake-session-1"),
        "{args}"
    );
    let events = log.read_all().expect("read the log");
    assert!(events.iter().any(|event| matches!(
        event.data,
        EventData::OutputReprompted {
            by: narsor_engine::domain::RepromptedBy::Engine,
            ..
        }
    )));

    send_sigterm(child.id());
    wait_for_exit(&mut child, Duration::from_secs(10));
}

// ---------------------------------------------------------------------------
// The conductor's label rescue (F1, U10) across a daemon restart
// ---------------------------------------------------------------------------

/// Seeds `REPROMPT_WORKFLOW` stopped during the rescue of its `work` state's
/// output: the rejected attempt closed in the log, the rescue recorded in
/// the execution, `Running`. The conductor's own leftovers are the test's.
fn seed_rescue(root: &Path) -> (ExecutionStore, narsor_engine::domain::WorkflowExecution) {
    let work = root.join("work");
    fs::create_dir_all(&work).expect("create a work dir");
    let definition_path = root.join("reprompt.yaml");
    fs::write(&definition_path, REPROMPT_WORKFLOW).expect("write the workflow");
    let definition = narsor_engine::workflow::load_from_file(&definition_path).expect("load it");
    let store = ExecutionStore::new(root);
    let engine = Engine::new(store.clone());
    let mut execution = engine
        .start(
            &definition,
            serde_json::json!({}),
            Workspace::local_folder(work.display().to_string()),
        )
        .expect("seed the execution");
    fs::write(
        store.paths().definition_file(&execution.id),
        REPROMPT_WORKFLOW,
    )
    .expect("pin the definition");

    let log = store.event_log(&execution.id);
    for data in [
        EventData::StateEntered {
            state_id: "work".to_string(),
        },
        EventData::ActionStarted {
            action_execution_id: "attempt-rejected".to_string(),
            state_id: "work".to_string(),
            action_type: "agent".to_string(),
        },
        EventData::ActionOutputRejected {
            action_execution_id: "attempt-rejected".to_string(),
            state_id: "work".to_string(),
            reason: "the answer has no 'output'".to_string(),
            answer_tail: Some("Done, all tests pass.".to_string()),
        },
    ] {
        log.append_next(&execution.id, data).expect("append");
    }
    execution.current_state = Some("work".to_string());
    execution.current_action_execution = Some("attempt-rejected".to_string());
    execution.metadata = serde_json::json!({
        "engine": {
            "output_rescue": {
                "state_id": "work",
                "answers": [{
                    "action_execution_id": "attempt-rejected",
                    "reason": "the answer has no 'output'",
                    "answer_tail": "Done, all tests pass.",
                }],
                "reason": "the answer has no 'output'",
            }
        }
    });
    store
        .save_execution(&execution)
        .expect("save the execution");
    (store, execution)
}

/// Spawns a daemon over `root` whose `PATH` starts with `bin`.
fn spawn_daemon_with_path(root: &Path, bin: &Path) -> DaemonGuard {
    DaemonGuard::new(root, daemon_with_path(root, bin))
}

/// `narsor --root <root> daemon` with `bin` first on its `PATH`, spawned
/// bare: the caller makes sure it does not outlive the test. Its output is
/// appended to `<root>/daemon.log`, so a failing test can be read back.
fn daemon_with_path(root: &Path, bin: &Path) -> Child {
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("daemon.log"))
        .expect("open the daemon's log");
    Command::new(engine_support::workspace_binary("narsor-server"))
        .arg("--root")
        .arg(root)
        .env("PATH", path)
        .stdout(log.try_clone().expect("share the daemon's log"))
        .stderr(log)
        .spawn()
        .expect("spawn the daemon")
}

/// Leaves the conductor's marker of the rescue invocation a restart cut
/// short, counted as the episode's first try.
fn leave_rescue_marker(store: &ExecutionStore, execution_id: &str) -> ConductorStore {
    let conductor = ConductorStore::new(store.paths(), execution_id);
    let request = QueuedRequest::new(
        ConductorTrigger::OutputRescue,
        ConductorPurpose::ChooseOutputAfterReprompt,
    )
    .with_episode("attempt-rejected");
    let mut state = ConductorState {
        pending: Some(PendingInvocation {
            invocation_id: "invocation-cut-short".to_string(),
            request: request.clone(),
            started_at: chrono::Utc::now(),
        }),
        ..ConductorState::default()
    };
    state.queue.push(request);
    state
        .invocation_attempts
        .insert("attempt-rejected".to_string(), 1);
    conductor.save_state(&state).expect("seed the marker");
    conductor
}

fn wait_for_status(store: &ExecutionStore, execution_id: &str, status: ExecutionStatus) -> bool {
    store
        .load_execution(execution_id)
        .map(|stored| stored.status == status)
        .unwrap_or(false)
}

/// Restart matrix: a daemon that stopped during the conductor's rescue
/// invocation invokes it once more at the next start — R24's one repeat,
/// counted before it runs — and applies what it chose.
#[tokio::test]
#[ignore = "starts a narsor daemon; run with --ignored"]
async fn a_fresh_daemon_repeats_a_rescue_invocation_the_restart_cut_short() {
    let root = TempRoot::new();
    let bin = fake_claude(
        &root.path().join("bin"),
        r#"{"decision": "choose_output", "label": "implemented", "confidence": "high",
            "rationale": "the agent said all tests pass"}"#,
    );
    let (store, execution) = seed_rescue(root.path());
    let conductor = leave_rescue_marker(&store, &execution.id);

    let mut child = spawn_daemon_with_path(root.path(), &bin);
    let _info = wait_for_daemon_info(root.path(), Duration::from_secs(10));
    wait_for(
        "the rescued execution completed",
        Duration::from_secs(15),
        || wait_for_status(&store, &execution.id, ExecutionStatus::Completed),
    )
    .await;

    let count = fs::read_to_string(bin.join("count")).expect("the conductor ran");
    assert_eq!(count.trim(), "1", "invoked once more, and only once");
    let state = conductor.load_state().expect("load the conductor's state");
    assert_eq!(state.attempts("attempt-rejected"), 2);
    assert_eq!(state.pending, None);
    assert!(state.queue.is_empty(), "{state:?}");
    let events = store.event_log(&execution.id).read_all().expect("read");
    assert!(events.iter().any(|event| matches!(
        &event.data,
        EventData::DecisionMade { provider, output_label, .. }
            if provider == "conductor" && output_label == "implemented"
    )));

    send_sigterm(child.id());
    wait_for_exit(&mut child, Duration::from_secs(10));
}

/// Restart matrix: when that one repeat fails too, the step is a problem
/// with the agent's raw answer — never a third invocation.
#[tokio::test]
#[ignore = "starts a narsor daemon; run with --ignored"]
async fn a_fresh_daemon_makes_a_problem_when_the_repeated_rescue_fails() {
    let root = TempRoot::new();
    let bin = fake_claude(&root.path().join("bin"), "no JSON at all");
    fs::write(bin.join("exit"), "1").expect("make the harness fail");
    let (store, execution) = seed_rescue(root.path());
    leave_rescue_marker(&store, &execution.id);

    let mut child = spawn_daemon_with_path(root.path(), &bin);
    let _info = wait_for_daemon_info(root.path(), Duration::from_secs(10));
    wait_for("the step became a problem", Duration::from_secs(15), || {
        wait_for_status(&store, &execution.id, ExecutionStatus::InputNeeded)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let count = fs::read_to_string(bin.join("count")).expect("the conductor ran");
    assert_eq!(count.trim(), "1", "no repeat after the repeat");
    let pending: narsor_engine::engine::PendingHumanInput = store
        .load_pending_input(&execution.id)
        .expect("read the pending input")
        .expect("a problem");
    let problem = pending.problem.expect("a problem");
    assert_eq!(problem.analysis, None);
    assert_eq!(
        problem.answers[0].answer_tail.as_deref(),
        Some("Done, all tests pass.")
    );

    send_sigterm(child.id());
    wait_for_exit(&mut child, Duration::from_secs(10));
}

/// Restart matrix: a choice the conductor saved before the restart is
/// applied at the next start without invoking it again.
#[tokio::test]
#[ignore = "starts a narsor daemon; run with --ignored"]
async fn a_fresh_daemon_applies_a_saved_choice_without_invoking_the_conductor() {
    let root = TempRoot::new();
    // Never to be run; there only so a mistake cannot reach a real harness.
    let bin = fake_claude(&root.path().join("bin"), "{}");
    let (store, execution) = seed_rescue(root.path());
    let conductor = ConductorStore::new(store.paths(), &execution.id);
    conductor
        .save_state(&ConductorState {
            unapplied_decisions: vec![narsor_engine::conductor::SavedDecision {
                decision_id: "decision-saved".to_string(),
                kind: narsor_engine::domain::ConductorDecisionKind::ChooseOutput,
                episode: Some("attempt-rejected".to_string()),
                state_id: Some("work".to_string()),
                output_label: Some("implemented".to_string()),
                artifacts: Default::default(),
                variables: serde_json::json!({}),
                confidence: narsor_engine::domain::Confidence::High,
                rationale: Some("the agent said all tests pass".to_string()),
                context_before: serde_json::json!({}),
                answers: Vec::new(),
                reason: None,
                reply: None,
                state_entries_before: serde_json::Value::Null,
                step_memory_before: serde_json::Value::Null,
                saved_at: chrono::Utc::now(),
            }],
            ..ConductorState::default()
        })
        .expect("save the decision");

    let mut child = spawn_daemon_with_path(root.path(), &bin);
    let _info = wait_for_daemon_info(root.path(), Duration::from_secs(10));
    wait_for(
        "the saved choice was applied",
        Duration::from_secs(15),
        || wait_for_status(&store, &execution.id, ExecutionStatus::Completed),
    )
    .await;

    assert!(!bin.join("count").exists(), "the conductor was not invoked");
    let state = conductor.load_state().expect("load the conductor's state");
    assert!(state.unapplied_decisions.is_empty(), "{state:?}");

    send_sigterm(child.id());
    wait_for_exit(&mut child, Duration::from_secs(10));
}

// ---------------------------------------------------------------------------
// The conductor of a task, hosted by its runner (U5)
// ---------------------------------------------------------------------------

/// Stops whatever a test left running under its root: the daemon, and the
/// runner of a task, which outlives it by design.
struct ProcessesUnder {
    root: PathBuf,
}

impl Drop for ProcessesUnder {
    fn drop(&mut self) {
        let root = self.root.display().to_string();
        let Ok(entries) = fs::read_dir("/proc") else {
            return;
        };
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            let command = fs::read(entry.path().join("cmdline")).unwrap_or_default();
            let command = String::from_utf8_lossy(&command).replace('\0', " ");
            if pid != std::process::id() && command.contains("narsor") && command.contains(&root) {
                // SAFETY: a process of this test's own root; a runner leads
                // its process group, which takes its harness with it.
                unsafe {
                    libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
            }
        }
    }
}

/// `narsor --root <root> <args>` run to completion, as an operator's
/// shell would.
fn narsor(root: &Path, bin: &Path, args: &[&str]) -> (i32, String) {
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_narsor-cli"))
        .current_dir(root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("HOME", root)
        .env("PATH", path)
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("run the narsor binary");
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.code().expect("normal exit"), text)
}

const GATE_WORKFLOW: &str = r#"
name: gate
version: 1
states:
  gate:
    action:
      type: script
      command: sh
      args: [-c, "printf '{\"choice\": \"ok\"}'"]
    choices: [ok, rejected]
    transitions:
      ok: completed
      rejected: failed
    decision:
      type: human
      message: Ship it?
  completed:
    description: Done.
  failed:
    description: Rejected.
terminal_states:
  completed: completed
  failed: failed
"#;

/// Restart matrix, for a task: a chat message its conductor had not
/// answered when the runner stopped is answered after the runner is started
/// again — by the real `narsor-runner` the daemon's local backend starts on
/// `task resume`, on the (fake) harness of its environment.
#[tokio::test]
#[ignore = "starts a narsor daemon; run with --ignored"]
async fn a_restarted_runner_answers_a_chat_message_left_unanswered() {
    let root = TempRoot::new();
    let _processes = ProcessesUnder {
        root: root.path().to_path_buf(),
    };
    // The first call is the summary of the human gate; every later one
    // answers the chat.
    let bin = fake_claude(
        &root.path().join("bin"),
        r#"{"decision": "reply", "message": "It waits at the gate."}"#,
    );
    fs::write(
        bin.join("stdout.1"),
        format!(
            "{}\n",
            serde_json::json!({
                "type": "result", "subtype": "success", "is_error": false,
                "session_id": "gate-summary",
                "result": r#"{"decision": "annotate", "summary": "Ship it?"}"#,
            })
        ),
    )
    .expect("write the summary of the gate");
    fs::create_dir_all(root.path().join("workflows")).unwrap();
    fs::write(root.path().join("workflows/gate.yaml"), GATE_WORKFLOW).unwrap();

    let mut daemon = spawn_daemon_with_path(root.path(), &bin);
    wait_for_daemon_info(root.path(), Duration::from_secs(10));
    let (code, text) = narsor(
        root.path(),
        &bin,
        &["task", "create", "gate", "--task-id", "t-gate"],
    );
    assert_eq!(code, 0, "{text}");

    // The runner is its own process; its storage is where the daemon's
    // local backend keeps it.
    let state = root.path().join("local-controller/storage/t-gate/state");
    let store = ExecutionStore::new(&state);
    let execution_id = {
        let mut id = None;
        wait_for("the task's execution", Duration::from_secs(30), || {
            id = store
                .list_execution_ids()
                .ok()
                .and_then(|ids| ids.into_iter().next());
            id.is_some()
        })
        .await;
        id.unwrap()
    };
    wait_for("the gate", Duration::from_secs(30), || {
        wait_for_status(&store, &execution_id, ExecutionStatus::InputNeeded)
    })
    .await;

    wait_for("the task to be running", Duration::from_secs(30), || {
        let (_, shown) = narsor(root.path(), &bin, &["task", "show", "t-gate"]);
        shown.contains("Lifecycle  running")
    })
    .await;

    // The runner stops at a safe point; what it left is a message nobody
    // answered, as a runner killed mid-answer leaves it.
    let (code, text) = narsor(root.path(), &bin, &["task", "pause", "t-gate"]);
    assert_eq!(code, 0, "{text}");
    wait_for("the task to be paused", Duration::from_secs(30), || {
        let (_, shown) = narsor(root.path(), &bin, &["task", "show", "t-gate"]);
        shown.contains("Lifecycle  paused")
    })
    .await;
    let conductor = ConductorStore::new(store.paths(), &execution_id);
    let message = conductor
        .append_chat(ChatEntry::new(
            ChatAuthor::Operator,
            ChatEntryKind::Message,
            "what is the task doing?",
        ))
        .expect("leave an unanswered message");

    let (code, text) = narsor(root.path(), &bin, &["task", "resume", "t-gate"]);
    assert_eq!(code, 0, "{text}");
    wait_for(
        "the restarted runner's answer",
        Duration::from_secs(30),
        || {
            conductor.read_chat().is_ok_and(|chat| {
                chat.iter().any(|entry| {
                    entry.author == ChatAuthor::Conductor
                        && entry.in_reply_to.as_deref() == Some(message.id.as_str())
                })
            })
        },
    )
    .await;
    let chat = conductor.read_chat().expect("read the chat");
    let answer = chat
        .iter()
        .find(|entry| entry.in_reply_to.as_deref() == Some(message.id.as_str()))
        .expect("an answer");
    assert_eq!(answer.kind, ChatEntryKind::Message, "{answer:?}");
    assert_eq!(answer.text, "It waits at the gate.");
    assert!(conductor.unanswered_chat().unwrap().is_empty());

    // The operator reads the same through the control plane.
    let (code, text) = narsor(
        root.path(),
        &bin,
        &["task", "conductor", "history", "t-gate"],
    );
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("Conductor: running on claude-code"), "{text}");
    assert!(text.contains("what is the task doing?"), "{text}");
    assert!(text.contains("It waits at the gate."), "{text}");

    send_sigterm(daemon.id());
    wait_for_exit(&mut daemon, Duration::from_secs(10));
}

// ---------------------------------------------------------------------------
// Automatic resume across daemon restarts
// ---------------------------------------------------------------------------

/// The task every restart scenario below runs: the gate workflow, which
/// waits on its owner's approval.
const RESUMED_TASK: &str = "t-resumed";

/// How long a task may take to come back after a restart: the scheduler's
/// 5 s tick, the controller's idle long-poll and a fresh runner's recovery.
const RESUME_TIMEOUT: Duration = Duration::from_secs(90);

/// A daemon over a root of its own whose task waits on an approval, which a
/// test stops and starts again. Runners outlive a graceful stop by design,
/// so whether the task's runner survives a restart is the test's choice.
/// Dropping it kills the daemon and every process left under its root.
struct RestartedDaemon {
    daemon: Child,
    bin: PathBuf,
    _processes: ProcessesUnder,
    root: TempRoot,
}

impl RestartedDaemon {
    /// Starts the daemon and creates the task.
    fn start() -> Self {
        let root = TempRoot::new();
        let bin = fake_claude(
            &root.path().join("bin"),
            r#"{"decision": "annotate", "summary": "Ship it?"}"#,
        );
        fs::create_dir_all(root.path().join("workflows")).unwrap();
        fs::write(root.path().join("workflows/gate.yaml"), GATE_WORKFLOW).unwrap();
        let daemon = daemon_with_path(root.path(), &bin);
        let started = Self {
            daemon,
            bin,
            _processes: ProcessesUnder {
                root: root.path().to_path_buf(),
            },
            root,
        };
        wait_for_daemon_info(started.root(), Duration::from_secs(15));
        let (code, text) = narsor(
            started.root(),
            &started.bin,
            &["task", "create", "gate", "--task-id", RESUMED_TASK],
        );
        assert_eq!(code, 0, "{text}");
        started
    }

    fn root(&self) -> &Path {
        self.root.path()
    }

    /// Stops the daemon gracefully, as `daemon stop` does.
    fn stop(&mut self) {
        send_sigterm(self.daemon.id());
        assert!(
            wait_for_exit(&mut self.daemon, Duration::from_secs(30)).success(),
            "the daemon stopped cleanly"
        );
        // A discovery file left behind would name the stopped daemon.
        let _ = fs::remove_file(daemon_file(self.root()));
    }

    /// Starts the daemon again over the same root.
    fn start_again(&mut self) {
        self.daemon = daemon_with_path(self.root(), &self.bin);
        wait_for_daemon_info(self.root(), Duration::from_secs(15));
    }

    fn client(&self) -> narsor::api::ApiClient {
        let info = DaemonInfo::read(&daemon_file(self.root())).expect("read daemon.json");
        narsor::api::ApiClient::new(info.addr, info.token)
    }

    async fn task(&self) -> narsor_engine::controller_protocol::TaskSummary {
        self.client()
            .control_task(RESUMED_TASK)
            .await
            .expect("read the task")
    }

    async fn events(&self) -> Vec<narsor_engine::orchestration::TaskEvent> {
        self.client()
            .control_events(RESUMED_TASK, 0)
            .await
            .expect("read the task's events")
    }

    /// The automatic resumes in the task's history: (attempt, limit) each.
    async fn auto_resumes(&self) -> Vec<(u32, u32)> {
        self.events()
            .await
            .into_iter()
            .filter_map(|event| match event.kind {
                narsor_engine::orchestration::TaskEventKind::AutoResumed {
                    attempt, limit, ..
                } => Some((attempt, limit)),
                _ => None,
            })
            .collect()
    }

    /// Waits until a running task's runner — one other than `replaced`,
    /// when given — reports the approval it waits on; returns that runner's
    /// incarnation and the approval's waiting id.
    async fn wait_at_approval(&self, replaced: Option<&str>) -> (String, String) {
        let deadline = Instant::now() + RESUME_TIMEOUT;
        loop {
            let task = self.task().await;
            let runner = task
                .runner
                .as_ref()
                .filter(|runner| Some(runner.incarnation.as_str()) != replaced);
            // The gate's human decision is the owner's answer to a question.
            let approval = task.attention.iter().find(|item| {
                item.kind == narsor_engine::orchestration::AttentionKind::Question
                    && item.state_id.as_deref() == Some("gate")
            });
            if let (
                narsor_engine::orchestration::RunnerState::Running,
                Some(runner),
                Some(approval),
            ) = (task.runner_state, runner, approval)
            {
                return (runner.incarnation.clone(), approval.waiting_id.clone());
            }
            assert!(
                Instant::now() < deadline,
                "the task never waited at its approval again (replacing {replaced:?}): \
                 {task:#?}\n{}",
                fs::read_to_string(self.root().join("daemon.log")).unwrap_or_default()
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// The pid of the task's runner, read from the local controller's
    /// workload record the way the controller finds it after a restart.
    fn runner_pid(&self) -> u32 {
        let workloads = self.root().join("local-controller/workloads");
        let pids = fs::read_dir(&workloads)
            .expect("the task's workloads")
            .filter_map(Result::ok)
            .filter_map(|entry| fs::read(entry.path().join("workload.json")).ok())
            .map(|bytes| {
                let record: serde_json::Value =
                    serde_json::from_slice(&bytes).expect("a workload record");
                u32::try_from(record["pid"].as_u64().expect("a runner pid")).unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(pids.len(), 1, "one workload for the one task: {pids:?}");
        pids[0]
    }

    /// Kills the task's runner — its process group, harness included — the
    /// way a host loses it, and waits until it is gone.
    fn kill_runner(&self) {
        let pid = self.runner_pid();
        assert!(pid_alive(pid), "the runner survived the daemon's stop");
        // SAFETY: the runner of this test's task leads its process group.
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while pid_alive(pid) {
            assert!(
                Instant::now() < deadline,
                "the killed runner never went away"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// One restart that loses the runner: stop, kill it, start again.
    fn restart_losing_the_runner(&mut self) {
        self.stop();
        self.kill_runner();
        self.start_again();
    }
}

impl Drop for RestartedDaemon {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

/// AE1: a task waiting on its owner's approval whose runner a daemon
/// restart lost returns, without its owner, to the very same approval — and
/// its history says it was resumed automatically.
#[tokio::test]
#[ignore = "starts a narsor daemon; run with --ignored"]
async fn a_task_waiting_on_an_approval_returns_to_it_after_a_restart_lost_its_runner() {
    let mut daemon = RestartedDaemon::start();
    let (runner, waiting_id) = daemon.wait_at_approval(None).await;

    daemon.restart_losing_the_runner();

    let (resumed, waiting_again) = daemon.wait_at_approval(Some(&runner)).await;
    assert_ne!(resumed, runner, "a new runner took over");
    assert_eq!(waiting_again, waiting_id, "it waits on the same approval");
    let resumes = daemon.auto_resumes().await;
    assert_eq!(resumes.len(), 1, "resumed automatically once: {resumes:?}");
    let task = daemon.task().await;
    assert!(task.resume.exhausted.is_none(), "{:?}", task.resume);
}

/// The restart rule: a loss a restarted daemon finds says nothing about
/// whether the task can run, so six restarts in a row — each losing the
/// runner well within the stability window — never use up the attempts.
/// Only the policy's limit of them (five) go uncounted, though: the sixth
/// is counted as attempt 1, so a crash-looping daemon ends in an
/// exhaustion rather than resuming forever.
#[tokio::test]
#[ignore = "starts a narsor daemon; run with --ignored"]
async fn six_restarts_that_each_lose_the_runner_do_not_exhaust_the_resume() {
    let mut daemon = RestartedDaemon::start();
    let (mut runner, waiting_id) = daemon.wait_at_approval(None).await;

    for restart in 1..=6 {
        daemon.restart_losing_the_runner();
        let (resumed, waiting_again) = daemon.wait_at_approval(Some(&runner)).await;
        assert_eq!(waiting_again, waiting_id, "restart {restart}");
        runner = resumed;
    }

    let resumes = daemon.auto_resumes().await;
    assert_eq!(resumes.len(), 6, "{resumes:?}");
    let attempts: Vec<u32> = resumes.iter().map(|(attempt, _)| *attempt).collect();
    assert_eq!(
        attempts,
        [0, 0, 0, 0, 0, 1],
        "five uncounted restarts, then the first counted attempt: {resumes:?}"
    );
    let task = daemon.task().await;
    assert_eq!(task.resume.consecutive_losses, 1, "{:?}", task.resume);
    assert!(task.resume.exhausted.is_none(), "{:?}", task.resume);
}

/// A runner that outlives the restart, as runners do by design, keeps its
/// task: still running on the same process, and nothing resumed.
#[tokio::test]
#[ignore = "starts a narsor daemon; run with --ignored"]
async fn a_runner_that_survives_a_restart_keeps_its_task_without_a_resume() {
    let mut daemon = RestartedDaemon::start();
    daemon.wait_at_approval(None).await;
    let pid = daemon.runner_pid();

    daemon.stop();
    let restarted_at = chrono::Utc::now();
    daemon.start_again();

    // The restarted controller has looked at the workload...
    let deadline = Instant::now() + RESUME_TIMEOUT;
    loop {
        let task = daemon.task().await;
        if task
            .observation
            .as_ref()
            .is_some_and(|observation| observation.received_at > restarted_at)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the restarted controller never reported the task: {task:#?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // ...and the resume policy had more than one tick to act on it.
    tokio::time::sleep(Duration::from_secs(12)).await;

    let task = daemon.task().await;
    assert_eq!(
        task.runner_state,
        narsor_engine::orchestration::RunnerState::Running,
        "{task:#?}"
    );
    assert_eq!(daemon.runner_pid(), pid, "the same runner");
    assert!(pid_alive(pid), "the runner still runs");
    assert!(daemon.auto_resumes().await.is_empty());
    assert!(task.resume.is_empty(), "{:?}", task.resume);
}

/// A task a daemon before the resume policy left `Interrupted` — its record
/// without any resume state — is resumed once a daemon with the policy
/// starts, back to the approval it waited on.
#[tokio::test]
#[ignore = "starts a narsor daemon; run with --ignored"]
async fn a_task_interrupted_before_the_resume_policy_is_resumed_at_start() {
    let mut daemon = RestartedDaemon::start();
    let (runner, waiting_id) = daemon.wait_at_approval(None).await;

    daemon.stop();
    daemon.kill_runner();
    interrupt_as_before_the_policy(daemon.root(), RESUMED_TASK);
    daemon.start_again();

    let (_, waiting_again) = daemon.wait_at_approval(Some(&runner)).await;
    assert_eq!(waiting_again, waiting_id);
    let resumes = daemon.auto_resumes().await;
    assert_eq!(resumes.len(), 1, "{resumes:?}");
    assert_eq!(resumes[0].0, 1, "the adopted loss is the first attempt");
}

/// Rewrites a stopped daemon's record of a running task the way a daemon
/// that predates the resume policy left a lost one: `Interrupted` by the
/// controller's report of the loss, committed as a new revision whose event
/// is not published yet (startup recovery publishes it), and none of the
/// fields the policy added.
fn interrupt_as_before_the_policy(root: &Path, task_id: &str) {
    use narsor_engine::orchestration::{
        Observation, RunnerState, TaskEvent, TaskEventKind, TaskRecord,
    };

    let path = narsor_engine::persistence::StatePaths::new(root).orchestration_task_file(task_id);
    let mut record: TaskRecord =
        serde_json::from_slice(&fs::read(&path).expect("read the task record"))
            .expect("parse the task record");
    assert_eq!(record.runner_state, RunnerState::Running);
    record.runner_state = RunnerState::Interrupted;
    record.revision += 1;
    record.last_event = TaskEvent {
        sequence: record.revision,
        task_id: record.task_id.clone(),
        at: chrono::Utc::now(),
        runner_state: RunnerState::Interrupted,
        generation: record.generation,
        kind: TaskEventKind::ControllerReported {
            observation: Observation::WorkloadLost { state_known: true },
            lease_epoch: record.last_report_epoch.expect("a controller reported"),
        },
    };
    let mut stored = serde_json::to_value(&record).expect("serialize the task record");
    let fields = stored.as_object_mut().expect("a record object");
    for added in ["resume", "workload_epoch", "running_since"] {
        assert!(fields.remove(added).is_some(), "the record has {added}");
    }
    fs::write(&path, serde_json::to_vec_pretty(&stored).unwrap()).expect("write the task record");
}
