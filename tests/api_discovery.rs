//! Integration tests for `ensure_daemon_running` (U5): the discover-or-launch
//! protocol clients use to find a running daemon or spawn one, over the
//! real, compiled `narsor` binary (not the test harness — see
//! [`init_exe_override`] on why that distinction matters here).
//!
//! Every test here spawns a real detached daemon process against its own
//! throwaway `TempRoot`, so each test cleans its daemon up (`SIGTERM`, then
//! waits for the PID to actually die) before its `TempRoot` is dropped —
//! nothing here is meant to outlive the test that started it.

use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::{Duration, Instant};
use std::{fs, io};

use chrono::Utc;
use narsor::api::ensure_daemon_running;
use narsor::config::Flags;
use narsor::daemon::launch::set_exe_override_for_tests;
use narsor::persistence::{DaemonInfo, StatePaths};

/// A temporary directory that deletes itself when it goes out of scope —
/// same stand-in every other integration test in this crate uses in place
/// of the `tempfile` crate.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("narsor-api-discovery-{}", uuid::Uuid::new_v4()));
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
/// `narsor` binary instead of this test binary itself.
///
/// `current_exe()` inside a `cargo test` process resolves to the test
/// harness binary, not `narsor` — without this, every daemon
/// `ensure_daemon_running` "spawns" here would actually re-exec this very
/// test binary with `daemon` as a bare positional argument, which is not a
/// recognised test filter and does nothing resembling a daemon. `Once`
/// because this sets a process-wide environment variable exactly once, no
/// matter how many of this file's tests end up racing each other to be the
/// first to call it — see `set_exe_override_for_tests`'s own doc comment.
fn init_exe_override() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        set_exe_override_for_tests(Path::new(env!("CARGO_BIN_EXE_narsor")));
    });
}

/// The `Flags` `ensure_daemon_running` needs to reproduce this test's own
/// `--root` on the daemon it spawns.
fn flags_for(root: &TempRoot) -> Flags {
    Flags {
        root: Some(root.path().to_path_buf()),
        config: None,
    }
}

fn state_paths(root: &TempRoot) -> StatePaths {
    StatePaths::new(root.path())
}

/// Whether a PID is alive right now (`kill(pid, 0)`), mirroring
/// `tests/daemon_lifecycle.rs`'s own helper of the same shape.
fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 sends nothing; it only probes existence/visibility.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Sends `SIGTERM` to `pid` and waits (bounded) for it to actually die —
/// the daemon this test spawned is detached, so there is no `Child` handle
/// to `wait()` on; polling `pid_alive` is the only way to know it is gone.
fn stop_and_reap(pid: u32) {
    // SAFETY: a plain signal-send to a PID this test's own daemon spawn
    // just reported.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while pid_alive(pid) {
        if Instant::now() >= deadline {
            // Best effort: do not fail the test over a slow shutdown, but
            // do not leave it silently running forever either.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A PID guaranteed not to be alive by the time the caller sees it: spawn
/// and reap a trivial child, mirroring
/// `src/persistence/daemon_info.rs`'s own test of the same shape.
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("spawn `true`");
    let pid = child.id();
    child.wait().expect("reap child");
    pid
}

/// Binds an ephemeral loopback port and immediately releases it — an
/// address nothing is listening on, so a connection to it fails fast
/// (`ECONNREFUSED`) rather than timing out, standing in for "this PID is
/// alive but is not a narsor daemon".
fn unused_loopback_addr() -> io::Result<std::net::SocketAddr> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.local_addr()
}

// ---------------------------------------------------------------------------
// AE2: no daemon running at all
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_daemon_running_spawns_one_and_returns_a_working_client() {
    init_exe_override();
    let root = TempRoot::new();
    let paths = state_paths(&root);
    let flags = flags_for(&root);

    let client = ensure_daemon_running(&paths, &flags)
        .await
        .expect("ensure_daemon_running should spawn a fresh daemon");

    let listing = client
        .list_executions()
        .await
        .expect("the returned client must actually work against the spawned daemon");
    assert!(listing.executions.is_empty());

    let info = DaemonInfo::read(&paths.daemon_file()).expect("daemon.json must exist");
    assert!(info.is_alive());
    assert!(
        !paths.daemon_lock_file().exists(),
        "the lock file must be cleaned up after a successful spawn"
    );

    stop_and_reap(info.pid);
}

// ---------------------------------------------------------------------------
// AE3: discovery file names a dead PID
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_stale_discovery_file_naming_a_dead_pid_is_cleaned_up_and_a_fresh_daemon_spawned() {
    init_exe_override();
    let root = TempRoot::new();
    let paths = state_paths(&root);
    let flags = flags_for(&root);

    let stale = DaemonInfo {
        pid: dead_pid(),
        addr: "127.0.0.1:1".parse().unwrap(),
        token: "stale-token".to_string(),
        started_at: Utc::now(),
    };
    stale
        .write(&paths.daemon_file())
        .expect("seed a stale discovery file");

    let client = ensure_daemon_running(&paths, &flags)
        .await
        .expect("ensure_daemon_running must recover from a stale discovery file");
    let listing = client
        .list_executions()
        .await
        .expect("the returned client must work");
    assert!(listing.executions.is_empty());

    let info = DaemonInfo::read(&paths.daemon_file()).expect("a fresh daemon.json must exist");
    assert_ne!(
        info.pid, stale.pid,
        "a genuinely fresh daemon must have been spawned, not the stale record reused"
    );
    assert!(info.is_alive());

    stop_and_reap(info.pid);
}

// ---------------------------------------------------------------------------
// A live, responding daemon is reused rather than duplicated
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_live_responding_daemon_is_reused_without_spawning_a_duplicate() {
    init_exe_override();
    let root = TempRoot::new();
    let paths = state_paths(&root);
    let flags = flags_for(&root);

    let first_client = ensure_daemon_running(&paths, &flags)
        .await
        .expect("first call spawns the daemon");
    let first_info = DaemonInfo::read(&paths.daemon_file()).expect("daemon.json exists");
    // Prove the first client is real before asking for a second.
    first_client
        .list_executions()
        .await
        .expect("first client works");

    let second_client = ensure_daemon_running(&paths, &flags)
        .await
        .expect("second call must discover the already-running daemon, not fail");
    let second_info = DaemonInfo::read(&paths.daemon_file()).expect("daemon.json still exists");

    assert_eq!(
        first_info.pid, second_info.pid,
        "no second daemon should have been spawned: the discovery file must still name the \
         same process"
    );
    second_client
        .list_executions()
        .await
        .expect("second client must also work, against the same daemon");

    stop_and_reap(first_info.pid);
}

// ---------------------------------------------------------------------------
// AE4-shaped: two concurrent callers racing with no daemon running
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_calls_with_no_daemon_running_spawn_exactly_one_daemon() {
    init_exe_override();
    let root = TempRoot::new();
    let paths = state_paths(&root);
    let flags = flags_for(&root);

    let (first, second) = tokio::join!(
        ensure_daemon_running(&paths, &flags),
        ensure_daemon_running(&paths, &flags)
    );
    let first_client = first.expect("first concurrent caller must succeed");
    let second_client = second.expect("second concurrent caller must succeed");

    // Both callers must have ended up with a client for the very same
    // daemon — if the lock had failed to serialize the spawn, the loser
    // would have raced its own `Engine`/`ExecutionStore` into existence
    // against the winner's, and the two clients would report different
    // addresses (whichever process's `daemon.json` write landed last).
    let listing_a = first_client
        .list_executions()
        .await
        .expect("first client works");
    let listing_b = second_client
        .list_executions()
        .await
        .expect("second client works");
    assert_eq!(listing_a.executions.len(), listing_b.executions.len());

    let info = DaemonInfo::read(&paths.daemon_file()).expect("daemon.json exists");
    assert!(
        !paths.daemon_lock_file().exists(),
        "the lock file must not be left behind by either caller"
    );

    stop_and_reap(info.pid);
}

// ---------------------------------------------------------------------------
// Error path: PID alive, but not a daemon
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_pid_that_is_alive_but_not_a_daemon_is_treated_as_dead() {
    init_exe_override();
    let root = TempRoot::new();
    let paths = state_paths(&root);
    let flags = flags_for(&root);

    // A real, unrelated, long-lived process — alive, but not a narsor
    // daemon and not listening on the address the discovery file claims.
    let mut unrelated = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn an unrelated long-lived process");

    let fake_addr = unused_loopback_addr().expect("grab an unused loopback address");
    let fake = DaemonInfo {
        pid: unrelated.id(),
        addr: fake_addr,
        token: "not-a-real-token".to_string(),
        started_at: Utc::now(),
    };
    fake.write(&paths.daemon_file())
        .expect("seed a discovery file naming a live, unrelated PID");

    let client = ensure_daemon_running(&paths, &flags).await.expect(
        "a PID that is alive but fails the health check must be treated as dead, and a fresh \
         daemon spawned instead of failing outright",
    );
    client
        .list_executions()
        .await
        .expect("the freshly spawned daemon must actually work");

    let info = DaemonInfo::read(&paths.daemon_file()).expect("a fresh daemon.json must exist");
    assert_ne!(
        info.pid, fake.pid,
        "the fresh daemon must not be the unrelated process"
    );
    assert!(info.is_alive());

    stop_and_reap(info.pid);
    let _ = unrelated.kill();
    let _ = unrelated.wait();
}
