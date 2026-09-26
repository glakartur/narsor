//! The daemon discovery file (`{state}/daemon.json`).
//!
//! A running daemon writes one of these so that clients can find it without a
//! registry: PID, bound address, a bearer token minted for this run, and when
//! it started. The file carries a secret (the token), so it is written under
//! `0600` from the very first byte — see [`write_atomic_with_mode`] for why
//! the ordinary atomic write is not enough here.
//!
//! [`DaemonInfo::is_alive`] only answers "does this PID still exist" — it
//! says nothing about whether the HTTP server behind it is actually
//! answering. That check needs a network round trip, which belongs to the
//! async client side (U6), not this synchronous, dependency-free module.

use super::atomic::write_json_atomic_with_mode;
use super::{read_json, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::Path;

/// Discovery file's permissions: owner read/write only, no group/other.
///
/// The token inside is a bearer credential for the daemon's whole API, so
/// this is not a hardening afterthought — see the module docs.
const DAEMON_FILE_MODE: u32 = 0o600;

/// What a client needs to find and talk to a running daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonInfo {
    /// PID of the daemon process, for [`DaemonInfo::is_alive`] and `stop`.
    pub pid: u32,
    /// Address the daemon's HTTP server is bound to.
    pub addr: SocketAddr,
    /// Bearer token minted fresh for this run, required on every request.
    pub token: String,
    /// When this daemon instance started.
    pub started_at: DateTime<Utc>,
}

impl DaemonInfo {
    /// Writes this record to `path` atomically, under `0600` from the first
    /// byte — never merely tightened after the fact.
    pub fn write(&self, path: &Path) -> Result<()> {
        write_json_atomic_with_mode(path, self, DAEMON_FILE_MODE)
    }

    /// Reads a discovery file back.
    ///
    /// Fails distinguishably: a missing file surfaces as
    /// [`PersistenceError::Io`] (not-found), a present-but-unparseable one as
    /// [`PersistenceError::Deserialize`] — so a caller can tell "no daemon
    /// here" apart from "a daemon left a corrupt file behind" and clean the
    /// latter up rather than silently giving up.
    pub fn read(path: &Path) -> Result<Self> {
        read_json(path)
    }

    /// Whether the process this record names is still alive.
    ///
    /// Checks the PID only — `kill(pid, 0)` on Unix, which sends no signal
    /// and just asks the kernel whether the process exists and is visible to
    /// this user. It says nothing about whether the HTTP server behind that
    /// PID is actually answering; that requires a network round trip and is
    /// the client's job (U6), not this module's.
    #[cfg(unix)]
    pub fn is_alive(&self) -> bool {
        // SAFETY: `kill` with signal 0 sends no signal; it only validates
        // that a process with this PID exists and is signalable by us. Not
        // representable as safe Rust because it is a raw syscall.
        let result = unsafe { libc::kill(self.pid as libc::pid_t, 0) };
        result == 0
    }

    /// Non-Unix fallback: liveness cannot be checked, so assume it is not.
    #[cfg(not(unix))]
    pub fn is_alive(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::PersistenceError;
    use std::fs;
    use std::path::PathBuf;

    /// A temporary directory that deletes itself when it goes out of scope.
    ///
    /// Stands in for the `tempfile` crate, which is not a dependency of this
    /// crate — mirrors the same helper in `tests/persistence.rs`.
    struct TempRoot {
        path: PathBuf,
    }

    impl TempRoot {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("narsor-daemon-info-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).expect("create temp root");
            Self { path }
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn sample(pid: u32) -> DaemonInfo {
        DaemonInfo {
            pid,
            addr: "127.0.0.1:4317".parse().unwrap(),
            token: "test-token".to_string(),
            started_at: Utc::now(),
        }
    }

    #[test]
    fn round_trips_through_write_and_read() {
        let dir = TempRoot::new();
        let path = dir.path.join("daemon.json");
        let info = sample(std::process::id());

        info.write(&path).unwrap();
        let read_back = DaemonInfo::read(&path).unwrap();

        assert_eq!(info, read_back);
    }

    #[test]
    fn write_sets_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempRoot::new();
        let path = dir.path.join("daemon.json");
        sample(std::process::id()).write(&path).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn is_alive_true_for_current_process() {
        let info = sample(std::process::id());
        assert!(info.is_alive());
    }

    #[test]
    fn is_alive_false_for_a_pid_that_does_not_exist() {
        // Spawn and reap a child: its PID is guaranteed not to be reused by
        // the time we check, at least for the duration of this assertion.
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn `true`");
        let pid = child.id();
        child.wait().expect("reap child");

        let info = sample(pid);
        assert!(!info.is_alive());
    }

    #[test]
    fn read_on_missing_file_fails_without_panicking() {
        let dir = TempRoot::new();
        let path = dir.path.join("does-not-exist.json");

        let error = DaemonInfo::read(&path).unwrap_err();
        assert!(matches!(error, PersistenceError::Io { .. }));
    }

    #[test]
    fn read_on_corrupt_file_fails_distinctly_from_missing() {
        let dir = TempRoot::new();
        let path = dir.path.join("daemon.json");
        fs::write(&path, b"not json").unwrap();

        let error = DaemonInfo::read(&path).unwrap_err();
        assert!(matches!(error, PersistenceError::Deserialize { .. }));
    }

    #[test]
    fn write_atomic_exclusive_succeeds_once_then_fails() {
        use crate::persistence::write_atomic_exclusive;

        let dir = TempRoot::new();
        let path = dir.path.join("daemon.lock");

        let first = write_atomic_exclusive(&path, b"lock").unwrap();
        assert!(first, "first writer should acquire the lock");

        let second = write_atomic_exclusive(&path, b"lock").unwrap();
        assert!(!second, "second writer should find the lock already taken");
    }
}
