//! Detach-spawn: turning a live client process into "narsor-server, running
//! in a session of its own".
//!
//! # Why re-exec + `setsid()`, not a double fork
//!
//! A daemon fully detached from the terminal that discovered it missing
//! needs its own session, so that closing that terminal (which sends
//! `SIGHUP` to every process in its session) never reaches the daemon.
//! `Command::process_group(0)` only gives the child its own *process
//! group*, not a new *session* — `SIGHUP` from the closing terminal would
//! still be delivered. The classic double-fork dodges this by forking twice
//! and calling `setsid()` in the middle child, but that forks a live
//! process — and this process may already be running the tokio runtime by
//! the time it decides to spawn a daemon (U5's `ensure_daemon_running`),
//! and forking a multi-threaded async runtime is exactly the kind of thing
//! that corrupts it (locks held by threads that do not exist in the child,
//! I/O drivers registered against an epoll fd the child no longer owns,
//! ...). A `pre_exec` closure runs after `fork` but *before* `exec`, in a
//! child that is about to replace its whole image anyway — nothing from the
//! parent's runtime survives to be corrupted, so `setsid()` there is safe
//! where a raw `fork()` mid-runtime would not be.
//!
//! `daemonize`-style crates are avoided for the same reason: they do a raw
//! `fork()` in the live process, which is the exact hazard above.
//!
//! See `docs/plans/2026-09-27-001-feat-daemon-lokalny-plan.md`, "External
//! References", for the fuller rationale this module implements.

use std::ffi::OsStr;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// Spawns a fully detached copy of the current binary.
///
/// `args` become the child's argv (after the binary path itself) — the
/// caller decides the subcommand and flags (typically `["daemon"]`, plus
/// any `--root`/`--config` the caller was itself invoked with, re-appended
/// *explicitly*: `exec` does not lose environment variables, but relying on
/// inherited environment alone is fragile here, since the point of this
/// call is precisely that the child may resolve a different default state
/// root than the caller expects unless told otherwise on its own argv).
///
/// `log_file` is opened for appending, created under owner-only `0600`
/// permissions before a single byte is written to it — a mutating request's
/// `Authorization` header or body could in principle reach this file
/// through `tracing` or panic output, so it must never be group- or
/// world-readable even for the short window before anything sensitive is
/// actually logged. Both the child's stdout and stderr are redirected
/// there; its stdin is `/dev/null` — a detached process has no terminal to
/// read from.
///
/// Returns the spawned child's PID as soon as it exists. The `Child` handle
/// itself is deliberately dropped without `wait()`: this process is not
/// responsible for the daemon's lifetime, and blocking here would defeat
/// the entire point of detaching. A process whose parent exits without
/// `wait`ing on it is simply reparented (to `init` or the nearest
/// subreaper) by the kernel — it does not become a zombie or get killed.
pub fn spawn_detached<I, S>(args: I, log_file: &Path) -> io::Result<u32>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let current_exe = resolve_exe()?;
    let log_out = open_log_file(log_file)?;
    let log_err = log_out.try_clone()?;

    let mut command = Command::new(current_exe);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_out))
        .stderr(Stdio::from(log_err));

    // SAFETY: `setsid()` is async-signal-safe and this closure runs in the
    // forked child after `fork` and before `exec` — exactly where POSIX
    // permits calling it. It touches no Rust-managed state shared with the
    // parent (no allocation beyond what libc itself may do internally for
    // the syscall, no locks), so it cannot deadlock against a parent thread
    // frozen mid-allocation by the fork, which is the usual danger of doing
    // more than this in a `pre_exec` closure.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = command.spawn()?;
    let pid = child.id();
    drop(child);
    Ok(pid)
}

/// Environment variable [`set_exe_override_for_tests`] writes to, and
/// [`resolve_exe`] checks before falling back to the sibling `narsor-server`.
///
/// Only ever read in a `debug_assertions` build (see [`resolve_exe`]): a
/// release binary must never let *any* environment variable redirect which
/// executable a daemon spawn re-execs, since that would hand anyone able to
/// set env vars for this process a way to make it launch an arbitrary
/// binary in its place — a strictly worse position than "no override
/// exists at all" that a shipped build should never carry.
#[cfg(debug_assertions)]
const EXE_OVERRIDE_ENV: &str = "NARSOR_TEST_DAEMON_EXE";

/// Resolves the executable [`spawn_detached`] re-execs: in a
/// `debug_assertions` build, the override set by
/// [`set_exe_override_for_tests`] if one was set, otherwise
/// the sibling `narsor-server`. A release build only ever resolves that sibling — the
/// override does not exist in that build at all (see [`EXE_OVERRIDE_ENV`]).
fn resolve_exe() -> io::Result<std::path::PathBuf> {
    #[cfg(debug_assertions)]
    if let Ok(path) = std::env::var(EXE_OVERRIDE_ENV) {
        return Ok(std::path::PathBuf::from(path));
    }
    let client_exe = std::env::current_exe()?;
    let server_name = if cfg!(windows) {
        "narsor-server.exe"
    } else {
        "narsor-server"
    };
    let adjacent = client_exe.with_file_name(server_name);
    if adjacent.is_file() {
        return Ok(adjacent);
    }
    // Cargo integration tests run from `target/<profile>/deps`; workspace
    // binaries under test are one directory above their test harnesses.
    if let Some(profile_dir) = client_exe.parent().and_then(|deps| deps.parent()) {
        let workspace_binary = profile_dir.join(server_name);
        if workspace_binary.is_file() {
            return Ok(workspace_binary);
        }
    }
    Ok(adjacent)
}

/// Path of the sibling server executable used by explicit client lifecycle
/// commands as well as detached discovery startup.
pub fn server_executable() -> io::Result<std::path::PathBuf> {
    resolve_exe()
}

/// Test-only seam: makes [`spawn_detached`] re-exec `path` instead of
/// `current_exe()`.
///
/// `cargo test`'s own harness binary is not the `narsor-server` binary — `current_exe()`
/// inside a test process resolves to the compiled test binary, not to
/// `narsor-server` itself (see this module's doc comment on why a raw re-exec of
/// `current_exe()` cannot be exercised directly from this crate's own unit
/// tests). An integration test that wants `ensure_daemon_running`
/// (`src/api/discovery.rs`, U5) to actually spawn the real daemon calls this
/// once, with the workspace path to `narsor-server`, before any test in the binary
/// calls through to [`spawn_detached`].
///
/// `#[cfg(debug_assertions)]`, not `#[cfg(test)]`: an integration test under
/// `tests/` is compiled as a separate crate that only sees this crate's
/// public API, never its `cfg(test)` items, so this has to be a plain,
/// exported function to be reachable from there at all — but it must still
/// not exist in a release build (see [`EXE_OVERRIDE_ENV`]), and every
/// ordinary `cargo test` run already builds in a `debug_assertions`
/// profile, so gating on that rather than `cfg(test)` costs nothing here.
///
/// Sets a process-wide environment variable, so a test binary that calls
/// this must do so once, before spawning any threads that call
/// [`spawn_detached`] concurrently — never from inside a `#[tokio::test]`
/// itself, where `cargo test`'s own test runner may already be running
/// other tests on other threads in the same process.
#[cfg(debug_assertions)]
pub fn set_exe_override_for_tests(path: &Path) {
    std::env::set_var(EXE_OVERRIDE_ENV, path);
}

/// Opens `path` for appending, ensuring `0600` permissions whether the file
/// is created fresh or already existed from a previous run.
fn open_log_file(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    // `mode()` only governs a freshly *created* file; an already-existing
    // log from a previous daemon run keeps whatever permissions it had. Set
    // them explicitly so a stale, more permissive file is tightened too.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TempRoot {
        path: std::path::PathBuf,
    }

    impl TempRoot {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("narsor-launch-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).expect("create temp root");
            Self { path }
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// Spawning `true` (via re-exec of the *test* binary would run this
    /// test binary itself, which is not what we want here) is not exercised
    /// through `spawn_detached` directly, because that always re-execs
    /// `current_exe()` — this crate's test harness binary, not `narsor`.
    /// The real binary is exercised end to end in
    /// `tests/daemon_lifecycle.rs`, which spawns the actual `narsor`
    /// executable and does not go through this function's `current_exe`
    /// resolution at all; this unit test only checks the log file's
    /// permission handling in isolation, which does not require exec.
    #[test]
    fn open_log_file_sets_owner_only_permissions_even_on_a_preexisting_file() {
        let dir = TempRoot::new();
        let path = dir.path.join("daemon.log");
        fs::write(&path, b"stale").expect("seed a pre-existing log");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))
            .expect("widen permissions to simulate a stale, loosely-permissioned file");

        let file = open_log_file(&path).expect("open the log file");
        let mode = file.metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
