//! What the daemon's supervised children share: the local controller
//! ([`crate::local_controller`]) and the observer worker
//! ([`crate::observer_worker`]).
//!
//! * [`prepare`] gives a child a clean start: none of the daemon's
//!   `NARSOR_*` variables, `RUST_LOG` after the daemon's level, a process
//!   group of its own, and an end with the daemon — `PR_SET_PDEATHSIG`,
//!   checked against the daemon's pid in case the daemon died first, and
//!   no capability: the daemon's ambient set is cleared
//!   ([`crate::capabilities`]).
//! * [`copy_stderr`] copies the child's stderr, line by line and prefixed,
//!   to the daemon's own stderr, which is the daemon's log.
//! * [`watch`] waits for the child's end or a stop request; a stop is
//!   [`terminate`]: `SIGTERM`, a bounded wait, then `SIGKILL`.
//! * [`RestartDelay`] is the pause before a restart, doubled on each
//!   further failure.
//!
//! `PR_SET_PDEATHSIG` is tied to the thread that forked, so each child is
//! started by a thread that lives as long as its supervision: a pool thread
//! ending after idling would kill a healthy child.

use std::io::{self, BufRead as _, Write as _};
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use narsor_shared::config::ENV_PREFIX;

use crate::capabilities;

/// Longest pause before a restart.
pub const MAX_RESTART_DELAY: Duration = Duration::from_secs(30);

/// A child that ran this long starts the restart pauses over.
pub const RESTART_RESET_AFTER: Duration = Duration::from_secs(60);

/// How often a supervising thread looks for the child's end.
pub const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Gives `command` a clean start as a child of this daemon: no `NARSOR_*`
/// variable of the daemon, `RUST_LOG` set to `log_directive` (or removed),
/// stdin and stdout closed, stderr piped, a process group of its own, and
/// `SIGTERM` when the starting thread ends, and an empty ambient
/// capability set, so that the child gets no capability from the daemon.
/// Add the child's own variables after this call.
pub fn prepare(command: &mut Command, log_directive: Option<&str>) {
    let parent = std::process::id();
    for (name, _) in std::env::vars_os() {
        if name.as_encoded_bytes().starts_with(ENV_PREFIX.as_bytes()) {
            command.env_remove(name);
        }
    }
    match log_directive {
        Some(directive) => command.env("RUST_LOG", directive),
        None => command.env_remove("RUST_LOG"),
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        // Its own group: a Ctrl-C at the daemon's terminal reaches the
        // daemon, which stops the child in order.
        .process_group(0);
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(io::Error::last_os_error());
            }
            // The daemon may have died before the signal was armed.
            if libc::getppid() as u32 != parent {
                return Err(io::Error::other("the daemon is gone"));
            }
            // Only the observer worker gets the daemon's capabilities, and
            // it raises them itself after this hook.
            capabilities::clear_ambient()
        });
    }
}

/// Starts a thread named `thread` that copies `child`'s stderr to the
/// daemon's stderr, each line after `prefix`, and hands each line to
/// `remember`. Kills and reaps the child when the thread cannot start.
pub fn copy_stderr(
    child: &mut Child,
    thread: &str,
    prefix: &'static str,
    mut remember: impl FnMut(String) + Send + 'static,
) -> io::Result<std::thread::JoinHandle<()>> {
    let stderr = child.stderr.take().expect("stderr is piped");
    let copier = std::thread::Builder::new()
        .name(thread.into())
        .spawn(move || {
            // Bytes, not `lines()`: a line that is not UTF-8 must not end
            // the copy, or the child's next write to stderr fails on a
            // closed pipe.
            let mut reader = io::BufReader::new(stderr);
            let mut buffer = Vec::new();
            loop {
                buffer.clear();
                match reader.read_until(b'\n', &mut buffer) {
                    Ok(0) => break,
                    Ok(_) => {
                        let line = String::from_utf8_lossy(&buffer);
                        let line = line.trim_end_matches(['\n', '\r']).to_owned();
                        // Not `eprintln!`: it panics when the write fails,
                        // and the end of this thread closes the pipe.
                        let _ = writeln!(io::stderr().lock(), "{prefix} {line}");
                        remember(line);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => {
                        // Keep the pipe open until the child closes it.
                        let _ = io::copy(&mut reader, &mut io::sink());
                        break;
                    }
                }
            }
        });
    if copier.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    copier
}

/// How one incarnation of a child ended.
#[derive(Debug)]
pub enum Ended {
    /// On request.
    Stopped,
    /// On its own, or it never started.
    Exited(Result<ExitStatus, String>),
}

/// Waits for the child's end or a stop request, whichever comes first; a
/// stop request [`terminate`]s the child within `stop_timeout`.
pub fn watch(
    child: &mut Child,
    stopped: &mpsc::Receiver<()>,
    stop_timeout: Duration,
    name: &str,
) -> Ended {
    loop {
        match stopped.recv_timeout(POLL_INTERVAL) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                terminate(child, stop_timeout, name);
                return Ended::Stopped;
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ended::Exited(Ok(status)),
            Ok(None) => {}
            Err(error) => return Ended::Exited(Err(format!("cannot wait for it: {error}"))),
        }
    }
}

/// `SIGTERM`, up to `timeout` for the child to end in order, then
/// `SIGKILL`.
pub fn terminate(child: &mut Child, timeout: Duration, name: &str) {
    signal(child.id(), libc::SIGTERM);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(Some(_)) | Err(_) = child.try_wait() {
            return;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    tracing::warn!("daemon: {name} did not stop in time; killing it");
    let _ = child.kill();
    let _ = child.wait();
}

fn signal(pid: u32, signal: i32) {
    // SAFETY: signals the child this thread started and has not reaped.
    unsafe {
        libc::kill(pid as libc::pid_t, signal);
    }
}

/// Waits `delay` for a stop request: `true` when one came (or the sender is
/// gone), `false` when the pause ran out and the child may start again.
pub fn stop_requested_within(stopped: &mpsc::Receiver<()>, delay: Duration) -> bool {
    match stopped.recv_timeout(delay) {
        Err(RecvTimeoutError::Timeout) => false,
        Ok(()) | Err(RecvTimeoutError::Disconnected) => true,
    }
}

/// The pauses before restarts: the first one, doubled after every restart
/// up to [`MAX_RESTART_DELAY`], and the first one again after a child that
/// ran [`RESTART_RESET_AFTER`].
#[derive(Debug)]
pub struct RestartDelay {
    first: Duration,
    next: Duration,
}

impl RestartDelay {
    /// Pauses starting at `first`.
    pub fn new(first: Duration) -> Self {
        Self { first, next: first }
    }

    /// The pause before starting again a child that ran for `ran_for`.
    pub fn after(&mut self, ran_for: Duration) -> Duration {
        if ran_for >= RESTART_RESET_AFTER {
            self.next = self.first;
        }
        let delay = self.next;
        self.next = (delay * 2).min(MAX_RESTART_DELAY);
        delay
    }
}

/// The executable `name` next to this executable — in a debug build also
/// the one in Cargo's profile directory, for a test harness under `deps/`.
/// Never one from `PATH` (KTD9).
pub fn sibling_program(name: &str) -> Result<std::path::PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("cannot find the narsor-server executable: {error}"))?;
    let adjacent = exe.with_file_name(name);
    if adjacent.is_file() {
        return Ok(adjacent);
    }
    #[cfg(debug_assertions)]
    if let Some(profile) = exe.parent().and_then(std::path::Path::parent) {
        let built = profile.join(name);
        if built.is_file() {
            return Ok(built);
        }
    }
    Err(format!(
        "{} is missing; install it in the directory of narsor-server",
        adjacent.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_restart_delay_doubles_up_to_thirty_seconds() {
        let mut delays = RestartDelay::new(Duration::from_secs(1));
        let crashed_at_once = Duration::from_millis(10);
        let seconds: Vec<u64> = (0..8)
            .map(|_| delays.after(crashed_at_once).as_secs())
            .collect();
        assert_eq!(seconds, [1, 2, 4, 8, 16, 30, 30, 30]);
    }

    #[test]
    fn a_child_that_ran_a_minute_starts_the_delays_over() {
        let mut delays = RestartDelay::new(Duration::from_secs(1));
        for _ in 0..4 {
            delays.after(Duration::ZERO);
        }
        assert_eq!(
            delays.after(RESTART_RESET_AFTER - Duration::from_secs(1)),
            Duration::from_secs(16)
        );
        assert_eq!(delays.after(RESTART_RESET_AFTER), Duration::from_secs(1));
        assert_eq!(delays.after(Duration::ZERO), Duration::from_secs(2));
    }
}
