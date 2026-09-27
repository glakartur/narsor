//! U8: the end-to-end proof of AE1 — "closing the client that started an
//! action does not kill the action or the daemon".
//!
//! Every other test that touches the daemon/client boundary (`tests/api.rs`,
//! `tests/api_discovery.rs`, `tests/daemon_lifecycle.rs`, `tests/tui_state.rs`)
//! proves one slice of this: that `POST /tasks` really starts a background
//! run, that `ensure_daemon_running` discovers or spawns a detached daemon,
//! that a killed *daemon* gets reconciled by D23 on its next start. None of
//! them puts all three together with a real client *process* — as opposed to
//! an in-process `ApiClient` value that simply goes out of scope — dying
//! mid-action. This file is that missing link.
//!
//! # What plays the role of "the client that started it"
//!
//! The plan's Approach text allows either an in-process `ApiClient` that is
//! simply never held past the call that starts the action, or "the
//! triggering step run in its own process, then killed". This test takes the
//! second, stronger option: it spawns a real `narsor run <workflow>`
//! subprocess — the actual CLI entry point an operator would type at a
//! terminal — and `SIGKILL`s it while the workflow's one action is still
//! sleeping. `narsor run` (U6) discovers-or-launches the daemon itself,
//! `POST`s the task, and then blocks polling for the run to settle before it
//! ever prints anything; killing it captures exactly the failure mode AE1
//! guards against (a terminal closed, a shell session ended, a client
//! crashed) without this test having to fake any of that by hand.
//!
//! Everything this test itself needs to observe the daemon afterwards (is it
//! still alive, is the event log still growing, did the execution finish) is
//! a second, independent `ApiClient` this test builds for itself — standing
//! in for a different operator re-attaching later, or the TUI, or `narsor
//! list`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Once;
use std::time::{Duration, Instant};

use narsor::api::{ensure_daemon_running, ApiClient};
use narsor::config::Flags;
use narsor::daemon::launch::set_exe_override_for_tests;
use narsor::domain::{Event, EventData, ExecutionStatus};
use narsor::persistence::{DaemonInfo, StatePaths};

// ---------------------------------------------------------------------------
// harness (same shapes as `tests/api_discovery.rs`/`tests/tui_state.rs`)
// ---------------------------------------------------------------------------

/// A temporary directory that deletes itself when it goes out of scope.
///
/// Stands in for the `tempfile` crate, which is not a dependency of this
/// crate — the same shape every other integration test in this crate uses.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-daemon-e2e-{}", uuid::Uuid::new_v4()));
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

/// Points [`narsor::daemon::launch::spawn_detached`] at the real, compiled
/// `narsor` binary instead of this test harness binary — see
/// `tests/api_discovery.rs`'s own copy of this helper for why this is
/// needed at all. `set_exe_override_for_tests` sets a process-wide
/// environment variable, which every subprocess this test spawns (including
/// the `narsor run` client, which itself calls `ensure_daemon_running` and
/// may need to spawn the daemon) inherits automatically.
fn init_exe_override() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        set_exe_override_for_tests(Path::new(env!("CARGO_BIN_EXE_narsor")));
    });
}

fn state_paths(root: &TempRoot) -> StatePaths {
    StatePaths::new(root.path())
}

fn flags_for(root: &TempRoot) -> Flags {
    Flags {
        root: Some(root.path().to_path_buf()),
        config: None,
    }
}

/// Whether a PID is alive right now (`kill(pid, 0)`), mirroring every other
/// integration test file's own copy of this check.
///
/// # Why this also reaps
///
/// This test, unlike every other integration test that spawns a detached
/// daemon, races its own [`observer_client`] against a *subprocess*
/// (`client_process`) to be the one whose `ensure_daemon_running` actually
/// wins the spawn — whichever one does becomes this test binary's own
/// direct child (`spawn_detached`'s `Child` handle is dropped without
/// `wait()`ing on it, by design: see its own doc comment). If this test's
/// own process turns out to be the winner, the daemon is this test's child
/// in the OS's bookkeeping sense too, and once it exits it is a zombie —
/// still reported "alive" by `kill(pid, 0)` — until something calls
/// `waitpid` on it. Nothing else in this process ever will, so this reaps
/// it (best-effort; `ECHILD` just means some other process is that pid's
/// real parent, which is fine) before answering the question a real
/// operator's shell would answer correctly for free.
fn pid_alive(pid: u32) -> bool {
    // SAFETY: `WNOHANG` never blocks; a failure (this pid is not our child,
    // or already reaped) is an expected, harmless outcome, not one that
    // needs propagating.
    unsafe {
        let mut status = 0;
        libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG);
    }
    // SAFETY: signal 0 sends nothing; it only probes existence/visibility.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// A client for whatever daemon is (or becomes) reachable over `root` —
/// spawning one fresh if none is running yet. Stands in for "a different
/// operator's process attaching to the daemon", never for the client whose
/// death this test is simulating.
async fn observer_client(root: &TempRoot) -> ApiClient {
    ensure_daemon_running(&state_paths(root), &flags_for(root))
        .await
        .expect("ensure_daemon_running finds/spawns the daemon")
}

// ---------------------------------------------------------------------------
// fixture: one script action slow enough to still be running when the
// client that started it gets killed
// ---------------------------------------------------------------------------

/// `tests/fixtures/scripts/sleep_then_emit.sh <seconds> <json>`: sleeps, then
/// prints the JSON payload the script executor reads as its output — the
/// same fixture `tests/fixtures/engine/slow_script.yaml` and
/// `tests/fixtures/engine/pause.yaml` already use for exactly this shape of
/// scenario, reused here by absolute path so this workflow needs no
/// `globals.workspace` of its own.
fn sleep_then_emit_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scripts/sleep_then_emit.sh")
}

/// How long the workflow's one action sleeps — long enough that this test
/// can reliably observe it still running (kill the client, check the daemon
/// and the event log) before it finishes, short enough not to slow the
/// suite down unreasonably.
const ACTION_SLEEP_SECONDS: u64 = 4;

/// Writes a workflow with a single script action that sleeps
/// [`ACTION_SLEEP_SECONDS`] before completing, and returns its absolute
/// path — passed straight to `narsor run` as the workflow argument, the same
/// way `tests/cli_smoke.rs`'s fixtures are.
fn write_slow_workflow(root: &TempRoot) -> PathBuf {
    let yaml = format!(
        r#"
name: daemon-e2e-slow
version: 1
description: >
  One script action that runs long enough for a test to kill the client that
  started it and still observe the action finishing on its own, entirely
  inside the daemon (AE1).

states:
  slow:
    action:
      type: script
      command: "{script}"
      args:
        - "{seconds}"
        - '{{"output": "done"}}'
    outputs:
      - done
    transitions:
      done: completed

  completed:
    description: Finished on its own time, long after the client died.

terminal_states:
  - completed
"#,
        script = sleep_then_emit_script().display(),
        seconds = ACTION_SLEEP_SECONDS,
    );
    let path = root.path().join("slow.yaml");
    fs::write(&path, yaml).expect("write the workflow definition");
    path
}

// ---------------------------------------------------------------------------
// polling helpers
// ---------------------------------------------------------------------------

async fn wait_for_execution(client: &ApiClient, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let listing = client
            .list_executions()
            .await
            .expect("list executions while waiting for one to appear");
        if let Some(execution) = listing.executions.first() {
            return execution.id.clone();
        }
        assert!(
            Instant::now() < deadline,
            "no execution ever appeared within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_event(
    client: &ApiClient,
    execution_id: &str,
    timeout: Duration,
    mut predicate: impl FnMut(&EventData) -> bool,
) -> Vec<Event> {
    let deadline = Instant::now() + timeout;
    loop {
        let events = client
            .events(execution_id, 0)
            .await
            .expect("read the event log while waiting");
        if events.iter().any(|event| predicate(&event.data)) {
            return events;
        }
        assert!(
            Instant::now() < deadline,
            "the expected event never appeared for '{execution_id}' within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn event_kind(data: &EventData) -> &'static str {
    match data {
        EventData::WorkflowStarted { .. } => "workflow_started",
        EventData::WorkflowPaused { .. } => "workflow_paused",
        EventData::WorkflowResumed => "workflow_resumed",
        EventData::WorkflowCompleted { .. } => "workflow_completed",
        EventData::WorkflowFailed { .. } => "workflow_failed",
        EventData::WorkflowCancelled { .. } => "workflow_cancelled",
        EventData::StateEntered { .. } => "state_entered",
        EventData::StateExited { .. } => "state_exited",
        EventData::ActionStarted { .. } => "action_started",
        EventData::ActionCompleted { .. } => "action_completed",
        EventData::ActionFailed { .. } => "action_failed",
        _ => "other",
    }
}

// ---------------------------------------------------------------------------
// AE1
// ---------------------------------------------------------------------------

#[tokio::test]
async fn closing_the_client_that_started_an_action_does_not_kill_the_daemon_or_the_action() {
    init_exe_override();
    let root = TempRoot::new();
    let workflow_path = write_slow_workflow(&root);

    // (1)/(2): the real CLI client, as an operator would type it. `narsor
    // run` discovers-or-launches the daemon (there is none yet for this
    // root, so it spawns one), `POST`s the task, then blocks polling for the
    // run to settle before printing anything — exactly the client this test
    // needs to kill mid-action.
    let mut client_process = Command::new(env!("CARGO_BIN_EXE_narsor"))
        .arg("--root")
        .arg(root.path())
        .arg("run")
        .arg(workflow_path.display().to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn `narsor run` as the starting client");

    // A second, independent client — never the one whose death is being
    // simulated — to observe the daemon this `run` invocation spawned.
    let observer = observer_client(&root).await;
    let daemon_info_path = state_paths(&root).daemon_file();
    let daemon_pid = DaemonInfo::read(&daemon_info_path)
        .expect("daemon.json must exist once a daemon is up")
        .pid;
    assert!(pid_alive(daemon_pid), "the freshly spawned daemon is alive");

    let execution_id = wait_for_execution(&observer, Duration::from_secs(10)).await;

    // Let the action actually start (not just the execution exist) before
    // killing the client — the interesting case is a client dying while an
    // action is genuinely in flight, not while the execution is still being
    // created.
    wait_for_event(&observer, &execution_id, Duration::from_secs(10), |data| {
        matches!(data, EventData::ActionStarted { .. })
    })
    .await;

    // (3): simulate the client's process dying outright — not a graceful
    // exit, a `SIGKILL`, the same as a terminal being closed or a session
    // being cut. The action the client started is, by this point, running
    // entirely inside the daemon's own `tokio::spawn(engine.run(...))` (U3);
    // nothing about it depends on this process still existing.
    client_process.kill().expect("SIGKILL the client process");
    let client_exit = client_process.wait().expect("reap the killed client");
    assert!(
        !client_exit.success(),
        "the client must have died, not exited normally"
    );

    let events_right_after_kill = observer
        .events(&execution_id, 0)
        .await
        .expect("read events right after killing the client");

    // (4): the daemon process (and, implicitly, the action it is still
    // running) survived the client's death — same PID as before, still
    // alive, discovery file untouched.
    let info_after_kill =
        DaemonInfo::read(&daemon_info_path).expect("daemon.json still exists after the kill");
    assert_eq!(
        info_after_kill.pid, daemon_pid,
        "the daemon must not have restarted — same process throughout"
    );
    assert!(
        pid_alive(daemon_pid),
        "the daemon must still be alive after its starting client was killed"
    );

    // (5): the conversation/event log keeps growing after the simulated
    // client death — proof the action itself is still progressing, not just
    // that the daemon process happens to still exist. Polling
    // `GET /executions/:id/events` (via `ApiClient::events`), exactly as the
    // plan's Approach specifies.
    let events_after_completion = wait_for_event(
        &observer,
        &execution_id,
        Duration::from_secs(ACTION_SLEEP_SECONDS + 10),
        |data| matches!(data, EventData::WorkflowCompleted { .. }),
    )
    .await;
    assert!(
        events_after_completion.len() > events_right_after_kill.len(),
        "the event log must have grown after the client died: had {}, now {}",
        events_right_after_kill.len(),
        events_after_completion.len()
    );
    let kinds: Vec<&str> = events_after_completion
        .iter()
        .map(|event| event_kind(&event.data))
        .collect();
    assert!(
        kinds.contains(&"action_completed"),
        "the action must have finished on its own, entirely inside the daemon: {kinds:?}"
    );
    assert!(
        kinds.contains(&"workflow_completed"),
        "the workflow must have run to completion after the client died: {kinds:?}"
    );

    let listing = observer
        .list_executions()
        .await
        .expect("list executions after completion");
    let summary = listing
        .executions
        .iter()
        .find(|execution| execution.id == execution_id)
        .expect("the execution is still listed");
    assert_eq!(summary.status, ExecutionStatus::Completed);

    // (6): clean up — `narsor daemon stop`, the same real CLI entry point an
    // operator would use, then confirm the process is actually gone before
    // this test's `TempRoot` removes the directory out from under it.
    let stop = Command::new(env!("CARGO_BIN_EXE_narsor"))
        .arg("--root")
        .arg(root.path())
        .arg("daemon")
        .arg("stop")
        .output()
        .expect("run `narsor daemon stop`");
    assert!(
        stop.status.success(),
        "daemon stop failed: {}",
        String::from_utf8_lossy(&stop.stderr)
    );

    let deadline = Instant::now() + Duration::from_secs(15);
    while pid_alive(daemon_pid) {
        assert!(
            Instant::now() < deadline,
            "the daemon did not exit after `daemon stop`; log:\n{}",
            fs::read_to_string(state_paths(&root).log_file()).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!daemon_info_path.exists());
}
