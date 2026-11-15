//! The trusted daemon's local controller: `narsor-controller local` as a
//! child process the daemon starts, enrolls and supervises (R6-R12; KTD3,
//! KTD5, KTD6, KTD9).
//!
//! The `local` backend is then like every other backend: its controller is
//! a process of its own that enrolls and talks `/controller/v1` over HTTP.
//! For the user nothing changes: the daemon still brings it up before it
//! becomes discoverable, and takes it down when it stops.
//!
//! # Starting
//!
//! The daemon creates a one-time enrollment code in process when the
//! credential does not exist yet, notes the `epoch` of the backend's
//! current lease, starts the child and waits — up to [`READY_TIMEOUT`] —
//! for a lease of a larger epoch. Only a lease of the new incarnation
//! proves the child works: after a `kill -9` of the previous daemon its
//! lease is still valid (R7). A child that ends first, a missing binary or
//! the timeout fail the start, with the end of the child's stderr.
//!
//! # What the child gets
//!
//! Every setting as a flag, already resolved, and `--no-config`: the child
//! reads no configuration file and none of the daemon's `NARSOR_*`
//! variables, which are all removed from its environment (R6). The
//! enrollment code is the one `NARSOR_*` variable it gets, and only when
//! there is one — never as an argument (R12). `RUST_LOG` follows the
//! daemon's log level; the child's stderr is copied, line by line and
//! prefixed, to the daemon's own stderr, which is the daemon's log (R4).
//!
//! # Supervision
//!
//! One thread starts every incarnation and lives as long as the
//! supervision does: `PR_SET_PDEATHSIG` is tied to the thread that forked,
//! so a pool thread ending after idling would kill a healthy child. The
//! child also checks its parent itself (`--parent-pid`), the guard for a
//! daemon killed with `SIGKILL` before the signal could be set up. A child
//! that ends after the start is started again after [`Supervision`]'s pause,
//! doubled on each further failure up to
//! [`child_process::MAX_RESTART_DELAY`] and reset once one ran for
//! [`child_process::RESTART_RESET_AFTER`] (R9). A restart enrolls nothing:
//! the credential is there. [`child_process`] holds what this supervision
//! shares with the observer worker's.
//!
//! # Stopping
//!
//! `SIGTERM`, then up to [`Supervision`]'s stop timeout for the child to
//! release its lease, then `SIGKILL` — while the API still accepts the
//! release (R8).

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use narsor_controller_runtime::{ControllerConfig, Supervision, ENROLLMENT_CODE_ENV};
use narsor_engine::controller_protocol::ProtocolService;
use narsor_engine::domain::BackendId;
use narsor_engine::environment::BackendRecord;

use crate::child_process::{self, Ended, RestartDelay, POLL_INTERVAL};

/// The controller binary, installed next to `narsor-server`.
const CONTROLLER_PROGRAM: &str = "narsor-controller";

/// Longest wait for the child's lease, counted from its start: less than
/// the clients' discovery timeout (15 s), with room for the child's own
/// wait on a predecessor's lock ([`LOCK_TIMEOUT`]).
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the child waits for a predecessor holding its credential lock.
const LOCK_TIMEOUT: Duration = Duration::from_secs(7);

/// How many of the child's last stderr lines a start failure quotes.
const STDERR_TAIL: usize = 20;

/// Prefix of the child's lines in the daemon's log.
const LOG_PREFIX: &str = "narsor-controller:";

/// Everything the daemon resolved for its local controller.
#[derive(Debug, Clone)]
pub struct LocalControllerSettings {
    /// The `narsor-controller` executable.
    pub program: PathBuf,
    /// The API's URL, for the controller and for its runners.
    pub control_plane: String,
    /// The controller's credential.
    pub credential_file: PathBuf,
    /// The backend's storage: task storages and runner records.
    pub storage: PathBuf,
    /// The HOME and harness arguments runners are started with.
    pub harness_args: Vec<OsString>,
    /// The `tracing` filter of the daemon, handed on as `RUST_LOG`.
    pub log_directive: Option<String>,
}

/// The `narsor-controller` next to this executable (KTD9), see
/// [`child_process::sibling_program`].
pub fn controller_program() -> Result<PathBuf, String> {
    child_process::sibling_program(CONTROLLER_PROGRAM)
}

/// How far the child got, shared by the starting daemon and the thread.
#[derive(Debug)]
enum Phase {
    /// Started; no lease of its own yet.
    Starting,
    /// Held a lease of its own: from now on, an end means a restart.
    Ready,
    /// Ended, or could not be started, before it was ready.
    Failed(String),
}

#[derive(Debug)]
struct Shared {
    phase: Mutex<Phase>,
    stderr: Mutex<VecDeque<String>>,
}

impl Shared {
    fn phase(&self) -> std::sync::MutexGuard<'_, Phase> {
        self.phase.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The last lines the child wrote, ready to append to a message.
    fn stderr_tail(&self) -> String {
        let lines = self.stderr.lock().unwrap_or_else(PoisonError::into_inner);
        if lines.is_empty() {
            return String::new();
        }
        let mut tail = String::from("\nnarsor-controller wrote:");
        for line in lines.iter() {
            tail.push_str("\n  ");
            tail.push_str(line);
        }
        tail
    }

    fn remember(&self, line: String) {
        let mut lines = self.stderr.lock().unwrap_or_else(PoisonError::into_inner);
        if lines.len() == STDERR_TAIL {
            lines.pop_front();
        }
        lines.push_back(line);
    }
}

/// The running local controller, supervised until [`Self::stop`].
#[derive(Debug)]
pub struct LocalController {
    stop: mpsc::Sender<()>,
    thread: std::thread::JoinHandle<()>,
}

impl LocalController {
    /// Enrolls when needed, starts the child and waits for its lease.
    pub async fn start(
        settings: LocalControllerSettings,
        control: Arc<ProtocolService>,
    ) -> Result<Self, String> {
        let backend = BackendId::parse(BackendRecord::LOCAL_ID).expect("a valid backend id");
        let epoch = |control: &ProtocolService| -> Result<Option<u64>, String> {
            control
                .control_plane()
                .lease(&backend)
                .map(|lease| lease.map(|lease| lease.epoch))
                .map_err(|error| format!("cannot read the local backend's lease: {error}"))
        };
        let before = epoch(&control)?;
        let code = if settings.credential_file.exists() {
            None
        } else {
            Some(
                control
                    .create_builtin_local_enrollment()
                    .map_err(|error| format!("could not enroll the local controller: {error}"))?
                    .enrollment_code,
            )
        };

        let shared = Arc::new(Shared {
            phase: Mutex::new(Phase::Starting),
            stderr: Mutex::new(VecDeque::new()),
        });
        let (stop, stopped) = mpsc::channel();
        let thread = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("local-controller".into())
                .spawn(move || supervise(&settings, code, &shared, &stopped))
                .map_err(|error| {
                    format!("cannot start the local controller's supervisor: {error}")
                })?
        };
        let controller = Self { stop, thread };

        let deadline = Instant::now() + READY_TIMEOUT;
        let failure = loop {
            // Judged under the lock the thread takes when the child ends, so
            // an end is either a failed start or a restart, never neither.
            let outcome = {
                let mut phase = shared.phase();
                match &*phase {
                    Phase::Failed(reason) => Some(Err(format!(
                        "the local controller did not start: {reason}{}",
                        shared.stderr_tail()
                    ))),
                    _ => match epoch(&control) {
                        Err(error) => Some(Err(error)),
                        Ok(epoch) if epoch > before => {
                            *phase = Phase::Ready;
                            Some(Ok(()))
                        }
                        Ok(_) => None,
                    },
                }
            };
            match outcome {
                Some(Ok(())) => {
                    tracing::info!("daemon: the local controller holds its lease");
                    return Ok(controller);
                }
                Some(Err(message)) => break message,
                None => {}
            }
            if Instant::now() >= deadline {
                break format!(
                    "the local controller did not take the local backend's lease within {} s{}",
                    READY_TIMEOUT.as_secs(),
                    shared.stderr_tail()
                );
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        };
        controller.stop().await;
        Err(failure)
    }

    /// Stops the child — `SIGTERM`, a bounded wait, then `SIGKILL` — and the
    /// supervision with it.
    pub async fn stop(self) {
        let _ = self.stop.send(());
        let thread = self.thread;
        if let Err(error) = tokio::task::spawn_blocking(move || thread.join()).await {
            tracing::warn!(%error, "daemon: could not wait for the local controller to stop");
        }
    }
}

/// The supervising thread: starts the child, restarts it after the start,
/// stops it on request.
fn supervise(
    settings: &LocalControllerSettings,
    mut code: Option<String>,
    shared: &Arc<Shared>,
    stopped: &mpsc::Receiver<()>,
) {
    let policy = Supervision::default();
    let mut delays = RestartDelay::new(policy.restart_after);
    loop {
        let started = Instant::now();
        let outcome = match spawn(settings, code.take(), shared) {
            Ok((mut child, stderr)) => {
                let outcome = child_process::watch(
                    &mut child,
                    stopped,
                    policy.stop_timeout,
                    "the local controller",
                );
                // Every line in before the end is judged.
                let _ = stderr.join();
                outcome
            }
            Err(error) => Ended::Exited(Err(format!(
                "cannot start {}: {error}",
                settings.program.display()
            ))),
        };
        let reason = match outcome {
            Ended::Stopped => return,
            Ended::Exited(Ok(status)) => format!("narsor-controller exited ({status})"),
            Ended::Exited(Err(reason)) => reason,
        };
        {
            let mut phase = shared.phase();
            if matches!(*phase, Phase::Starting) {
                *phase = Phase::Failed(reason);
                return;
            }
        }
        let delay = delays.after(started.elapsed());
        tracing::warn!(
            reason = %reason,
            restart_in_secs = delay.as_secs(),
            "daemon: the local controller ended; starting it again"
        );
        if child_process::stop_requested_within(stopped, delay) {
            return;
        }
    }
}

/// Starts one incarnation, with a thread copying its stderr to the log.
fn spawn(
    settings: &LocalControllerSettings,
    code: Option<String>,
    shared: &Arc<Shared>,
) -> io::Result<(Child, std::thread::JoinHandle<()>)> {
    let mut command = Command::new(&settings.program);
    command
        .arg("local")
        .arg("--control-plane")
        .arg(&settings.control_plane)
        .arg("--credential-file")
        .arg(&settings.credential_file)
        .arg("--storage")
        .arg(&settings.storage)
        .args(&settings.harness_args)
        .arg("--lease-ttl-secs")
        .arg(
            ControllerConfig::new(String::new())
                .lease_ttl
                .as_secs()
                .to_string(),
        )
        .arg("--lock-timeout-secs")
        .arg(LOCK_TIMEOUT.as_secs().to_string())
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .arg("--no-config");
    child_process::prepare(&mut command, settings.log_directive.as_deref());
    if let Some(code) = code {
        command.env(ENROLLMENT_CODE_ENV, code);
    }
    let mut child = command.spawn()?;
    let shared = Arc::clone(shared);
    let copier = child_process::copy_stderr(&mut child, "local-controller-log", LOG_PREFIX, {
        move |line| shared.remember(line)
    })?;
    Ok((child, copier))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_start_failure_quotes_the_last_twenty_lines() {
        let shared = Shared {
            phase: Mutex::new(Phase::Starting),
            stderr: Mutex::new(VecDeque::new()),
        };
        assert_eq!(shared.stderr_tail(), "");
        for index in 0..25 {
            shared.remember(format!("line {index}"));
        }
        let tail = shared.stderr_tail();
        // An empty line, the heading, then the lines.
        let quoted: Vec<&str> = tail.lines().skip(2).map(str::trim).collect();
        let expected: Vec<String> = (5..25).map(|index| format!("line {index}")).collect();
        assert_eq!(quoted, expected);
        assert!(tail.starts_with("\nnarsor-controller wrote:"), "{tail}");
    }
}
