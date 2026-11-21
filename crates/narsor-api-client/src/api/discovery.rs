//! "Discover a running daemon, or launch one" (U5): the client-side half of
//! the sequence documented in the plan's "High-Level Technical Design"
//! (AE2/AE3/AE4).
//!
//! [`ensure_daemon_running`] is the one entry point every client (CLI, TUI —
//! U6/U7, not built yet) is meant to call before it ever talks to the API:
//! it hands back a working [`ApiClient`], spawning a fresh daemon behind the
//! scenes if (and only if) none was already running.
//!
//! # Why liveness alone (a PID check) is not enough
//!
//! A PID existing only means *some* process holds it — not that it is the
//! daemon this crate started, still less that its HTTP server is actually
//! answering. A PID can be reused by an unrelated process after the daemon
//! that owned it died (AE3's "dead PID" is the easy case; the harder one,
//! also covered here, is a *live* PID that answers to nothing narsor
//! understands). So every branch below that is about to hand back a client
//! for an *existing* discovery file first does a real, authenticated
//! request — [`health_check`] — and only trusts the file if that succeeds
//! too. A stale or wrong entry is treated exactly like no daemon at all:
//! cleaned up, then handled by the spawn/lock path below.
//!
//! # Why a lock file, not just "check-then-spawn"
//!
//! Two clients discovering no daemon at (almost) the same moment must not
//! both spawn one — two daemons would open the same on-disk store for write
//! at once, exactly the hazard this whole plan exists to close off (see
//! the daemon discovery record's module docs and U1's Approach).
//! `{state}/run/daemon.lock`, acquired via
//! [`write_exclusive`]'s atomic exclusive-create semantics,
//! makes "decide who spawns" atomic: exactly one caller ever observes
//! `Ok(true)` for a given lock file. The loser does not spawn its own
//! daemon — it polls `{state}/run/daemon.json`
//! the same way it would if it had found a live daemon on its very first
//! read, on the theory that "someone else is already spawning" and "someone
//! else has already spawned" are the same wait from this caller's point of
//! view.
//!
//! Every file of this protocol lives in the rendezvous directory
//! `{state}/run/` ([`RunPaths`]), checked by [`RunPaths::prepare`] before
//! anything is read or written there.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use narsor_protocol::api::DaemonDiscoveryRecord;
use narsor_shared::run_paths::{RunDirError, RunPaths};

use crate::config::Flags;

use super::client::ApiClient;
use super::launch::spawn_detached;

/// How often the discovery poll (waiting for `daemon.json` to appear and
/// answer) re-checks.
///
/// Short enough that a caller waiting on a daemon that starts in well under
/// a second (the common case: bind a loopback socket, construct an
/// in-memory `Engine`) does not feel it, long enough not to spin a busy
/// loop against the filesystem.
const DISCOVERY_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long [`ensure_daemon_running`] waits, in total, for a daemon (its own
/// spawn, or one another racing caller is spawning) to become discoverable
/// and answer a health check before giving up.
///
/// Generous relative to how long a daemon actually takes to start (well
/// under a second in every observed run) because the plan's Verification
/// asks for this to hold even under a concurrent-spawn race
/// ([`ensure_daemon_running`]'s AE4-shaped scenario), where the losing
/// caller is waiting on a whole extra process's worth of startup — `exec`,
/// recovery sweep, socket bind — that its own path never pays for.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);

/// How long one health check may wait for a loopback daemon's answer.
///
/// A busy daemon (several tasks running) can need more than the TUI's
/// 800 ms request bound. A slow answer is not a dead daemon.
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// How many health checks a live PID gets before the daemon is called
/// unresponsive.
const HEALTH_CHECK_ATTEMPTS: u32 = 3;

/// Pause between two health checks of one live PID.
const HEALTH_CHECK_PAUSE: Duration = Duration::from_secs(1);

/// How many of the last lines of the server log a start failure shows.
const LOG_TAIL_LINES: usize = 20;

/// How many times [`try_acquire_lock`] will reap a stale lock and retry
/// before giving up and treating the lock as held by a live racer.
///
/// One reap is normally enough (the lock's owner is either dead, in which
/// case removing it once clears the way, or alive, in which case the very
/// next attempt observes that and stops immediately) — a small bound beyond
/// one only guards against pathological back-to-back staleness, not a
/// realistic steady state.
const MAX_LOCK_REAP_ATTEMPTS: u32 = 5;

/// A failure of the discover-or-launch protocol.
#[derive(Debug)]
pub enum DiscoveryError {
    /// The rendezvous directory `{state}/run/` is refused or cannot be
    /// created.
    RunDir(RunDirError),
    /// A filesystem operation (reading, writing, or removing one of the
    /// discovery/lock files) failed for a reason other than the file simply
    /// not being there yet.
    Persistence(std::io::Error),
    /// Detach-spawning a fresh daemon process failed at the OS level (e.g.
    /// `current_exe()`, opening the log file, or the `fork`/`exec` itself).
    Spawn(std::io::Error),
    /// The discovery file is readable or writable by someone other than
    /// this user, or belongs to another user: it could point this client —
    /// and the token it would send — at somebody else's server, so it is
    /// not trusted, and not removed either (it is not this user's to remove).
    Unprotected(std::path::PathBuf),
    /// The daemon on this state root runs in network mode: it has no local
    /// token, and a client authenticates with an OIDC token instead.
    NetworkMode,
    /// Waited [`DISCOVERY_TIMEOUT`] without ever seeing a discovery file
    /// that both names a live PID and answers an authenticated health
    /// check — whether this caller was the one spawning or was waiting on
    /// another caller's spawn. `log` is where a daemon this client spawns
    /// writes, the reason it did not start included.
    Timeout {
        /// The daemon's log, `{state}/run/narsor-server.log`.
        log: PathBuf,
        /// What the daemon this client spawned wrote there; empty when
        /// another client spawned it.
        tail: String,
    },
    /// The daemon this client spawned exited before it answered, for
    /// example because it refused its configuration.
    Exited {
        /// The daemon's log, `{state}/run/narsor-server.log`.
        log: PathBuf,
        /// The last lines the daemon wrote there.
        tail: String,
    },
}

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiscoveryError::RunDir(error) => write!(f, "{error}"),
            DiscoveryError::Persistence(error) => {
                write!(f, "could not read/write daemon discovery state: {error}")
            }
            DiscoveryError::Spawn(error) => {
                write!(f, "could not launch the narsor daemon: {error}")
            }
            DiscoveryError::Unprotected(path) => write!(
                f,
                "refusing the daemon discovery file '{}': it must belong to you and be \
                 readable by you only (mode 0600)",
                path.display()
            ),
            DiscoveryError::NetworkMode => write!(
                f,
                "the narsor daemon on this state directory runs in network mode; connect \
                 to it with an OIDC token instead of the local discovery file"
            ),
            DiscoveryError::Timeout { log, tail } => {
                write!(
                    f,
                    "timed out waiting for the narsor daemon to start and answer; see its log, {}",
                    log.display()
                )?;
                write_tail(f, tail)
            }
            DiscoveryError::Exited { log, tail } => {
                write!(
                    f,
                    "the narsor daemon stopped before it answered; see its log, {}",
                    log.display()
                )?;
                write_tail(f, tail)
            }
        }
    }
}

/// Appends the end of the daemon's log to a start failure, when there is one.
fn write_tail(f: &mut std::fmt::Formatter<'_>, tail: &str) -> std::fmt::Result {
    if tail.is_empty() {
        return Ok(());
    }
    write!(f, "; it ends with:\n{tail}")
}

impl std::error::Error for DiscoveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DiscoveryError::RunDir(error) => Some(error),
            DiscoveryError::Persistence(error) => Some(error),
            DiscoveryError::Spawn(error) => Some(error),
            DiscoveryError::Unprotected(_)
            | DiscoveryError::NetworkMode
            | DiscoveryError::Timeout { .. }
            | DiscoveryError::Exited { .. } => None,
        }
    }
}

impl From<std::io::Error> for DiscoveryError {
    fn from(error: std::io::Error) -> Self {
        DiscoveryError::Persistence(error)
    }
}

/// Ensures a daemon is running against `paths`' state root and returns a
/// client already configured to talk to it — discovering an existing one,
/// or launching a fresh one, per the plan's sequence diagram.
///
/// `flags` supplies the `--root`/`--config` this process was itself invoked
/// with, re-appended verbatim to the spawned daemon's own argv: a detached
/// re-exec of the current binary does not lose the parent's environment,
/// but relying on inherited environment alone here would be fragile, since
/// the whole point of this call is that the spawned daemon must resolve
/// exactly the same state root this caller did — not whatever a bare
/// `narsor-server` would default to.
pub async fn ensure_daemon_running(
    state_root: impl AsRef<Path>,
    flags: &Flags,
) -> Result<ApiClient, DiscoveryError> {
    let run = RunPaths::new(state_root);
    run.prepare().map_err(DiscoveryError::RunDir)?;
    let daemon_file = run.daemon_file();

    if let Some(info) = read_protected(&daemon_file)? {
        if pid_is_alive(info.pid) && info.token.is_empty() {
            return Err(DiscoveryError::NetworkMode);
        }
        if pid_is_alive(info.pid)
            && is_responsive(HEALTH_CHECK_ATTEMPTS, HEALTH_CHECK_PAUSE, || {
                health_check(&info)
            })
            .await
        {
            return Ok(client_for(&info));
        }
        // Stale entry (dead PID — AE3) or a PID that answers to nothing
        // narsor understands (a reused PID, or some unrelated process):
        // clean it up and fall through to the spawn/lock path below, same
        // as if no discovery file had ever existed.
        let _ = std::fs::remove_file(&daemon_file);
    }

    let lock_file = run.daemon_lock_file();
    let won_the_lock = try_acquire_lock(&lock_file, &run.daemon_lock_guard_file())?;

    if won_the_lock {
        let outcome = spawn_and_wait(&run, flags).await;
        // Cleaned up on every path out of this branch — success and
        // failure alike — so a spawn that times out never leaves a
        // permanent deadlock behind for the next caller (see U1's Approach
        // on this same point).
        let _ = std::fs::remove_file(&lock_file);
        Ok(client_for(&outcome?))
    } else {
        // Someone else already won the lock and is spawning (or has just
        // spawned) — this caller must not spawn its own daemon (the
        // AE4-shaped race the plan calls out). Wait for their daemon
        // exactly as it would wait for its own.
        let info = poll_until_ready(&run, None, DISCOVERY_TIMEOUT).await?;
        Ok(client_for(&info))
    }
}

/// Builds the client [`ensure_daemon_running`] hands back, from a
/// discovery record already proven live by [`health_check`].
fn client_for(info: &DaemonDiscoveryRecord) -> ApiClient {
    ApiClient::new(info.addr, info.token.clone())
}

/// Attempts to acquire `lock_file`, reaping it first if it names a PID that
/// is no longer alive.
///
/// A plain `write_atomic_exclusive` call would deadlock every future caller
/// forever behind a lock whose winner died — killed, OOM, a closing
/// terminal — after acquiring it but before its own `remove_file` ran (see
/// [`ensure_daemon_running`]'s cleanup comment: that cleanup only runs on the
/// winner's own path, so it never runs at all if that path is cut short).
/// The lock file's content is the winner's own PID for exactly this reason —
/// [`LOCK_FILE_CONTENTS`] used to be a fixed, unparseable string; recording
/// the PID is what makes staleness checkable at all, the same way
/// `daemon.json` already lets [`DaemonInfo::is_alive`] tell a dead entry from
/// a live one.
///
/// Bounded by [`MAX_LOCK_REAP_ATTEMPTS`]: each attempt either wins outright,
/// or finds a live owner (a genuine, not stale, racing caller) and stops
/// immediately — the bound only guards against back-to-back staleness, which
/// should not happen in steady state.
fn try_acquire_lock(lock_file: &Path, guard_file: &Path) -> Result<bool, DiscoveryError> {
    // Keep the guard inode permanently in place: unlinking a flocked file
    // would let another process lock a different inode at the same path.
    // The kernel releases the advisory lock if a process exits or is killed,
    // so stale-lock recovery remains automatic after a crash.
    let _guard = lock_acquisition_guard(guard_file)?;
    for _ in 0..MAX_LOCK_REAP_ATTEMPTS {
        if write_exclusive(lock_file, current_pid_bytes().as_bytes())? {
            return Ok(true);
        }
        match read_lock_owner(lock_file) {
            Some(pid) if !pid_is_alive(pid) => {
                // The lock's owner is dead: reap the stale lock and try
                // again. A concurrent racer could win the re-acquire
                // instead of us — that is not a bug, just this caller
                // losing the race to a live one, exactly as an ordinary
                // `write_atomic_exclusive` failure would be handled below.
                let _ = std::fs::remove_file(lock_file);
            }
            // A live owner, or the lock file could not be read/parsed
            // (already removed by whoever reaped it, or mid-write by its
            // creator) — either way, not stale from this caller's point of
            // view. Fall through to the poll-for-someone-else's-daemon path,
            // same as an ordinary lost race.
            _ => return Ok(false),
        }
    }
    Ok(false)
}

/// Serializes stale-lock inspection/removal and lock creation across
/// processes. `daemon.lock` itself cannot provide this serialization: two
/// reapers can both read the same dead PID, then the later `remove_file`
/// could unlink a fresh lock created by the first reaper.
fn lock_acquisition_guard(guard_path: &Path) -> Result<std::fs::File, DiscoveryError> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(guard_path)?;
    // SAFETY: flock operates on this live file descriptor and has no pointer
    // arguments. The File owns the descriptor until after this function's
    // caller completes acquisition; dropping it releases LOCK_EX.
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(file)
}

/// Atomically creates a lock file without overwriting a concurrent owner's
/// file. Its directory is the prepared rendezvous directory.
fn write_exclusive(path: &Path, bytes: &[u8]) -> Result<bool, DiscoveryError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let temp = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("lock"),
        uuid::Uuid::new_v4()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = options.open(&temp)?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = std::fs::remove_file(&temp);
        return Err(error.into());
    }
    drop(file);
    let linked = match std::fs::hard_link(&temp, path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            return Err(error.into());
        }
    };
    std::fs::remove_file(&temp)?;
    Ok(linked)
}

#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only asks the OS whether the PID exists and is
    // signalable; it sends no signal to the process.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(not(unix))]
fn pid_is_alive(_pid: u32) -> bool {
    false
}

/// The current process's PID, as the exact bytes [`try_acquire_lock`] writes
/// into a freshly acquired `daemon.lock`.
fn current_pid_bytes() -> String {
    std::process::id().to_string()
}

/// Reads `lock_file`'s content back as the PID that acquired it, or `None`
/// if the file is gone, unreadable, or does not parse as one — any of which
/// means "not a lock this caller can judge the staleness of," not "stale."
fn read_lock_owner(lock_file: &Path) -> Option<u32> {
    std::fs::read_to_string(lock_file).ok()?.trim().parse().ok()
}

/// The winning branch: launch a detached daemon, then wait for it to
/// publish a discovery file that also answers a health check.
async fn spawn_and_wait(
    run: &RunPaths,
    flags: &Flags,
) -> Result<DaemonDiscoveryRecord, DiscoveryError> {
    let log = run.server_log_file();
    // Only what this daemon writes explains why it did not start; the log
    // keeps the output of earlier daemons before it.
    let from = std::fs::metadata(&log)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let child = spawn_detached(daemon_launch_args(flags), &log).map_err(DiscoveryError::Spawn)?;
    let mut spawned = Spawned { child, log, from };
    poll_until_ready(run, Some(&mut spawned), DISCOVERY_TIMEOUT).await
}

/// The daemon this client launched, and where its output starts in the log.
struct Spawned {
    child: std::process::Child,
    log: PathBuf,
    from: u64,
}

impl Spawned {
    fn tail(&self) -> String {
        log_tail(&self.log, self.from, LOG_TAIL_LINES)
    }
}

/// The last `lines` lines of `log` after byte `from`; empty when there is
/// nothing to read.
fn log_tail(log: &Path, from: u64, lines: usize) -> String {
    use std::io::{Read, Seek, SeekFrom};

    let mut text = Vec::new();
    let read = std::fs::File::open(log).and_then(|mut file| {
        file.seek(SeekFrom::Start(from))?;
        file.read_to_end(&mut text)
    });
    if read.is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&text);
    let all: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Builds the argv a spawned `narsor-server` needs to resolve the same
/// configuration as this process — see [`ensure_daemon_running`]'s doc on
/// why this is spelled out explicitly rather than left to inherited
/// environment.
fn daemon_launch_args(flags: &Flags) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(root) = &flags.root {
        args.push("--root".to_string());
        args.push(path_to_arg(root));
    }
    if let Some(config) = &flags.config {
        args.push("--config".to_string());
        args.push(path_to_arg(config));
    }
    args
}

fn path_to_arg(path: &std::path::Path) -> String {
    path.display().to_string()
}

/// Polls the discovery file until it both exists and names a daemon that answers
/// an authenticated health check, or `timeout` elapses.
///
/// Used identically by the lock-winning branch (waiting on its own spawn)
/// and the lock-losing branch (waiting on someone else's) — from this
/// caller's point of view "I am waiting for a daemon to become ready" is
/// the same operation either way. A daemon this caller spawned
/// (`spawned`) that exits stops the wait at once, with the end of its log.
async fn poll_until_ready(
    run: &RunPaths,
    mut spawned: Option<&mut Spawned>,
    timeout: Duration,
) -> Result<DaemonDiscoveryRecord, DiscoveryError> {
    let daemon_file = run.daemon_file();
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(info) = read_protected(&daemon_file)? {
            if health_check(&info).await {
                return Ok(info);
            }
        }
        if let Some(spawned) = spawned.as_deref_mut() {
            if matches!(spawned.child.try_wait(), Ok(Some(_))) {
                return Err(DiscoveryError::Exited {
                    log: spawned.log.clone(),
                    tail: spawned.tail(),
                });
            }
        }
        if Instant::now() >= deadline {
            return Err(DiscoveryError::Timeout {
                log: run.server_log_file(),
                tail: spawned.map(|spawned| spawned.tail()).unwrap_or_default(),
            });
        }
        tokio::time::sleep(DISCOVERY_POLL_INTERVAL).await;
    }
}

/// Reads the discovery file, but only trusts one that is protected: a
/// regular file owned by this user with no group or other permission bits
/// — the way the daemon writes it (`0600` from the first byte). Trusted
/// mode's whole security rests on that file being private (R36).
///
/// `Ok(None)` when there is no usable file (absent, not a regular file,
/// unparseable): the same as no daemon, as before.
fn read_protected(path: &Path) -> Result<Option<DaemonDiscoveryRecord>, DiscoveryError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata,
        _ => return Ok(None),
    };
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    if metadata.uid() != me || metadata.mode() & 0o077 != 0 {
        return Err(DiscoveryError::Unprotected(path.to_path_buf()));
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    Ok(serde_json::from_slice(&bytes).ok())
}

/// A real, authenticated request against the address/token a discovery
/// record names — the only way to tell "a live process happens to hold
/// this PID" from "the narsor daemon is actually listening and accepting
/// this token" (see the module doc). There is no unauthenticated `/health`
/// endpoint (every route in this plan sits behind `require_token`), so this
/// reuses an ordinary read endpoint, `GET /executions`, exactly as an
/// ordinary [`ApiClient`] caller would.
async fn health_check(info: &DaemonDiscoveryRecord) -> bool {
    let client = ApiClient::with_timeout(info.addr, info.token.clone(), HEALTH_CHECK_TIMEOUT);
    client.control_capabilities().await.is_ok()
}

/// Whether `probe` succeeds within `attempts` tries, `pause` apart. Stops
/// at the first success.
async fn is_responsive<F, Fut>(attempts: u32, pause: Duration, mut probe: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for attempt in 0..attempts {
        if attempt > 0 {
            tokio::time::sleep(pause).await;
        }
        if probe().await {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn run_responsive(results: Vec<bool>) -> (bool, usize) {
        let calls = std::cell::Cell::new(0usize);
        let attempts = results.len() as u32;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let verdict = runtime.block_on(is_responsive(attempts, Duration::ZERO, || {
            let answer = results[calls.get()];
            calls.set(calls.get() + 1);
            async move { answer }
        }));
        (verdict, calls.get())
    }

    #[test]
    fn a_daemon_that_answers_late_is_responsive() {
        assert_eq!(run_responsive(vec![false, false, true]), (true, 3));
    }

    #[test]
    fn a_daemon_that_answers_at_once_is_probed_once() {
        assert_eq!(run_responsive(vec![true, false, false]), (true, 1));
    }

    #[test]
    fn a_daemon_that_never_answers_is_unresponsive_after_all_attempts() {
        assert_eq!(run_responsive(vec![false, false, false]), (false, 3));
    }

    #[test]
    fn daemon_launch_args_reappends_root_and_config_flags() {
        let flags = Flags {
            root: Some(PathBuf::from("/tmp/some-root")),
            config: Some(PathBuf::from("/tmp/some.toml")),
        };
        assert_eq!(
            daemon_launch_args(&flags),
            vec!["--root", "/tmp/some-root", "--config", "/tmp/some.toml",]
        );
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn a_timeout_points_at_the_server_log_in_the_run_dir() {
        let state =
            std::env::temp_dir().join(format!("narsor-discovery-timeout-{}", std::process::id()));
        let run = RunPaths::new(&state);

        let error = block_on(poll_until_ready(&run, None, Duration::from_millis(1)))
            .expect_err("no daemon answers");

        let message = error.to_string();
        let log = state.join("run").join("narsor-server.log");
        assert!(message.contains(&log.display().to_string()), "{message}");
    }

    #[test]
    fn a_spawned_daemon_that_exits_reports_what_it_wrote_to_its_log() {
        let state =
            std::env::temp_dir().join(format!("narsor-discovery-exited-{}", uuid::Uuid::new_v4()));
        let run = RunPaths::new(&state);
        run.prepare().expect("prepare the run directory");
        let log = run.server_log_file();
        std::fs::write(&log, "an earlier daemon's line\n").unwrap();
        let from = std::fs::metadata(&log).unwrap().len();
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "echo 'narsor-server: the configuration does not hold up:' >> '{0}'; \
                 echo '  server.mode: unknown value' >> '{0}'; exit 1",
                log.display()
            ))
            .spawn()
            .expect("spawn a daemon stand-in");
        let mut spawned = Spawned { child, log, from };

        let error = block_on(poll_until_ready(
            &run,
            Some(&mut spawned),
            Duration::from_secs(10),
        ))
        .expect_err("the daemon exited");

        let message = error.to_string();
        assert!(matches!(error, DiscoveryError::Exited { .. }), "{message}");
        assert!(message.contains("server.mode: unknown value"), "{message}");
        assert!(!message.contains("an earlier daemon"), "{message}");
        std::fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn daemon_launch_args_omits_flags_that_were_not_given() {
        let flags = Flags::default();
        assert!(daemon_launch_args(&flags).is_empty());
    }

    #[test]
    fn concurrent_stale_lock_reapers_leave_the_winner_lock_intact() {
        let root =
            std::env::temp_dir().join(format!("narsor-lock-reaper-{}", uuid::Uuid::new_v4()));
        let run = RunPaths::new(&root);
        run.prepare().expect("prepare the run directory");
        let lock = run.daemon_lock_file();
        let guard = run.daemon_lock_guard_file();
        let mut exited = std::process::Command::new("true")
            .spawn()
            .expect("spawn process to obtain a dead PID");
        let dead_pid = exited.id();
        exited.wait().expect("reap dead-PID helper");
        std::fs::write(&lock, dead_pid.to_string()).expect("seed dead-owner lock");

        let start = std::sync::Arc::new(std::sync::Barrier::new(8));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let lock = lock.clone();
                let guard = guard.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    try_acquire_lock(&lock, &guard).expect("acquire or observe winner")
                })
            })
            .collect();
        let winners = workers
            .into_iter()
            .map(|worker| worker.join().expect("worker succeeds"))
            .filter(|won| *won)
            .count();

        assert_eq!(winners, 1, "only one process may win stale-lock recovery");
        assert_eq!(
            read_lock_owner(&lock),
            Some(std::process::id()),
            "a losing reaper must not unlink the fresh winner's lock"
        );
        std::fs::remove_dir_all(root).expect("remove test state root");
    }
}
