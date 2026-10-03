//! "Discover a running daemon, or launch one" (U5): the client-side half of
//! the sequence documented in the plan's "High-Level Technical Design"
//! (AE2/AE3/AE4).
//!
//! [`ensure_daemon_running`] is the one entry point every client (CLI, TUI —
//! U6/U7, not built yet) is meant to call before it ever talks to the API:
//! it hands back a working [`ApiClient`], spawning a fresh daemon behind the
//! scenes if (and only if) none was already running.
//!
//! # Why liveness alone ([`DaemonInfo::is_alive`]) is not enough
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
//! `src/persistence/daemon_info.rs`'s module doc and U1's Approach).
//! `{state}/daemon.lock`, acquired via
//! [`crate::persistence::write_atomic_exclusive`]'s `O_EXCL`-like semantics,
//! makes "decide who spawns" atomic: exactly one caller ever observes
//! `Ok(true)` for a given lock file. The loser does not spawn its own
//! daemon — it polls [`paths.daemon_file()`](crate::persistence::StatePaths::daemon_file)
//! the same way it would if it had found a live daemon on its very first
//! read, on the theory that "someone else is already spawning" and "someone
//! else has already spawned" are the same wait from this caller's point of
//! view.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::config::Flags;
use crate::daemon::launch::spawn_detached;
use crate::persistence::{
    pid_is_alive, write_atomic_exclusive, DaemonInfo, PersistenceError, StatePaths,
};

use super::client::ApiClient;

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
    /// A filesystem operation (reading, writing, or removing one of the
    /// discovery/lock files) failed for a reason other than the file simply
    /// not being there yet.
    Persistence(PersistenceError),
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
    /// another caller's spawn.
    Timeout,
}

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
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
            DiscoveryError::Timeout => write!(
                f,
                "timed out waiting for the narsor daemon to start and answer"
            ),
        }
    }
}

impl std::error::Error for DiscoveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DiscoveryError::Persistence(error) => Some(error),
            DiscoveryError::Spawn(error) => Some(error),
            DiscoveryError::Unprotected(_)
            | DiscoveryError::NetworkMode
            | DiscoveryError::Timeout => None,
        }
    }
}

impl From<PersistenceError> for DiscoveryError {
    fn from(error: PersistenceError) -> Self {
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
/// `narsor daemon` would default to.
pub async fn ensure_daemon_running(
    paths: &StatePaths,
    flags: &Flags,
) -> Result<ApiClient, DiscoveryError> {
    let daemon_file = paths.daemon_file();

    if let Some(info) = read_protected(&daemon_file)? {
        if info.is_alive() && info.token.is_empty() {
            return Err(DiscoveryError::NetworkMode);
        }
        if info.is_alive() && health_check(&info).await {
            return Ok(client_for(&info));
        }
        // Stale entry (dead PID — AE3) or a PID that answers to nothing
        // narsor understands (a reused PID, or some unrelated process):
        // clean it up and fall through to the spawn/lock path below, same
        // as if no discovery file had ever existed.
        let _ = std::fs::remove_file(&daemon_file);
    }

    let lock_file = paths.daemon_lock_file();
    let won_the_lock = try_acquire_lock(&lock_file)?;

    if won_the_lock {
        let outcome = spawn_and_wait(paths, flags, &daemon_file).await;
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
        let info = poll_until_ready(&daemon_file, DISCOVERY_TIMEOUT).await?;
        Ok(client_for(&info))
    }
}

/// Builds the client [`ensure_daemon_running`] hands back, from a
/// discovery record already proven live by [`health_check`].
fn client_for(info: &DaemonInfo) -> ApiClient {
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
fn try_acquire_lock(lock_file: &Path) -> Result<bool, DiscoveryError> {
    for _ in 0..MAX_LOCK_REAP_ATTEMPTS {
        if write_atomic_exclusive(lock_file, current_pid_bytes().as_bytes())? {
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
    paths: &StatePaths,
    flags: &Flags,
    daemon_file: &std::path::Path,
) -> Result<DaemonInfo, DiscoveryError> {
    let args = daemon_launch_args(flags);
    spawn_detached(args, &paths.log_file()).map_err(DiscoveryError::Spawn)?;
    poll_until_ready(daemon_file, DISCOVERY_TIMEOUT).await
}

/// Builds the argv a spawned `narsor daemon` needs to resolve the same
/// configuration as this process — see [`ensure_daemon_running`]'s doc on
/// why this is spelled out explicitly rather than left to inherited
/// environment.
fn daemon_launch_args(flags: &Flags) -> Vec<String> {
    let mut args = vec!["daemon".to_string()];
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

/// Polls `daemon_file` until it both exists and names a daemon that answers
/// an authenticated health check, or `timeout` elapses.
///
/// Used identically by the lock-winning branch (waiting on its own spawn)
/// and the lock-losing branch (waiting on someone else's) — from this
/// caller's point of view "I am waiting for a daemon to become ready" is
/// the same operation either way.
async fn poll_until_ready(
    daemon_file: &std::path::Path,
    timeout: Duration,
) -> Result<DaemonInfo, DiscoveryError> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(info) = read_protected(daemon_file)? {
            if health_check(&info).await {
                return Ok(info);
            }
        }
        if Instant::now() >= deadline {
            return Err(DiscoveryError::Timeout);
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
fn read_protected(path: &Path) -> Result<Option<DaemonInfo>, DiscoveryError> {
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
    Ok(DaemonInfo::read(path).ok())
}

/// A real, authenticated request against the address/token a discovery
/// record names — the only way to tell "a live process happens to hold
/// this PID" from "the narsor daemon is actually listening and accepting
/// this token" (see the module doc). There is no unauthenticated `/health`
/// endpoint (every route in this plan sits behind `require_token`), so this
/// reuses an ordinary read endpoint, `GET /executions`, exactly as an
/// ordinary [`ApiClient`] caller would.
async fn health_check(info: &DaemonInfo) -> bool {
    let client = client_for(info);
    client.list_executions().await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn daemon_launch_args_reappends_root_and_config_flags() {
        let flags = Flags {
            root: Some(PathBuf::from("/tmp/some-root")),
            config: Some(PathBuf::from("/tmp/some.toml")),
        };
        assert_eq!(
            daemon_launch_args(&flags),
            vec![
                "daemon",
                "--root",
                "/tmp/some-root",
                "--config",
                "/tmp/some.toml",
            ]
        );
    }

    #[test]
    fn daemon_launch_args_omits_flags_that_were_not_given() {
        let flags = Flags::default();
        assert_eq!(daemon_launch_args(&flags), vec!["daemon"]);
    }
}
