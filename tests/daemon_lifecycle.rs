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

use narsor::conductor::{
    ChatAuthor, ChatEntry, ChatEntryKind, ConductorState, ConductorStore, PendingInvocation,
    QueuedRequest, CONDUCTOR_UNAVAILABLE,
};
use narsor::domain::{
    ConductorOutcome, ConductorPurpose, ConductorTrigger, EventData, ExecutionStatus, Workspace,
};
use narsor::engine::{Engine, OutputRejection};
use narsor::persistence::{DaemonInfo, ExecutionStore};

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

/// Spawns `narsor --root <root> daemon` as a plain (non-detached) child, so
/// the test itself can `wait()` on it directly.
fn spawn_daemon(root: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_narsor"))
        .arg("--root")
        .arg(root)
        .arg("daemon")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the daemon")
}

/// Runs `narsor --root <root> daemon stop` to completion; returns its exit
/// code and combined stdout+stderr.
fn stop_daemon_cli(root: &Path) -> (i32, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_narsor"))
        .arg("--root")
        .arg(root)
        .arg("daemon")
        .arg("stop")
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
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
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
// `daemon stop`
// ---------------------------------------------------------------------------

#[test]
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

    let drained = narsor::cli::daemon::drain_active_runs(&engine, Duration::from_secs(5)).await;

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
        - '{{"output": "done"}}'
    outputs:
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
        .json(&serde_json::json!({ "workflow": long_workflow.display().to_string(), "inputs": {} }))
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
        .json(
            &serde_json::json!({ "workflow": short_workflow.display().to_string(), "inputs": {} }),
        )
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
fn seed_two_steps(root: &Path) -> (ExecutionStore, narsor::domain::WorkflowExecution) {
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

/// Restart matrix: a chat message the conductor had not answered when the
/// daemon stopped is answered after the next start — here with the error
/// entry of a daemon that cannot invoke the conductor yet (KTD12).
#[tokio::test]
async fn a_fresh_daemon_answers_a_chat_message_left_unanswered() {
    let root = TempRoot::new();
    let (store, execution) = seed_two_steps(root.path());
    let conductor = ConductorStore::new(store.paths(), &execution.id);
    let message = conductor
        .append_chat(ChatEntry::new(
            ChatAuthor::Operator,
            ChatEntryKind::Message,
            "what is the task doing?",
        ))
        .expect("seed an unanswered message");

    let mut child = spawn_daemon(root.path());
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
    assert_eq!(chat.entries[0], message);
    let answer = &chat.entries[1];
    assert_eq!(answer.in_reply_to.as_deref(), Some(message.id.as_str()));
    assert_eq!(answer.author, ChatAuthor::Conductor);
    assert_eq!(answer.kind, ChatEntryKind::Error);
    assert_eq!(answer.text, CONDUCTOR_UNAVAILABLE);

    let log = store.event_log(&execution.id);
    wait_for(
        "the invocation was recorded",
        Duration::from_secs(5),
        || {
            log.read_all().expect("read the log").iter().any(|event| {
                matches!(
                    event.data,
                    EventData::ConductorFinished {
                        outcome: ConductorOutcome::Failed,
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
    assert_eq!(
        conductor.load_state().expect("load the conductor's state"),
        state,
        "the marker is left for the conductor's own restart row"
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
    outputs:
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
            "result": r#"{"output":"implemented"}"#,
        })
        .to_string(),
    )
    .expect("write the harness answer");

    let work = root.path().join("work");
    fs::create_dir_all(&work).expect("create a work dir");
    let definition_path = root.path().join("reprompt.yaml");
    fs::write(&definition_path, REPROMPT_WORKFLOW).expect("write the workflow");
    let definition = narsor::workflow::load_from_file(&definition_path).expect("load it");
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
    let mut child = Command::new(env!("CARGO_BIN_EXE_narsor"))
        .arg("--root")
        .arg(root.path())
        .arg("daemon")
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
            by: narsor::domain::RepromptedBy::Engine,
            ..
        }
    )));

    send_sigterm(child.id());
    wait_for_exit(&mut child, Duration::from_secs(10));
}
