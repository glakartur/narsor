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
//! The daemon creates a short-lived one-time enrollment code in process for
//! every start of the child, notes the `epoch` of the backend's current
//! lease, starts the child and waits — up to [`READY_TIMEOUT`] —
//! for a lease of a larger epoch. Only a lease of the new incarnation
//! proves the child works: after a `kill -9` of the previous daemon its
//! lease is still valid (R7). A child that ends first, a missing binary or
//! the timeout fail the start, with the end of the child's stderr.
//!
//! # What the child gets
//!
//! Its connection and the state and work roots (`--state-root`,
//! `--work-root`) as flags — the controller appends its own directory name
//! to each, so the daemon never constructs the controller's paths or looks
//! at its files (KTD7) — and its configuration the way the
//! daemon got it (R21): `--config <file>` naming the file that won the
//! daemon's cascade, or `--no-config` when none won, so the child cannot
//! find a file that appeared later. The child reads its own `controller`
//! section from it, on every start: a supervised restart reads the file
//! again. Of the daemon's `NARSOR_*` variables only those of the
//! controller's sections (`NARSOR_GLOBAL__*`, `NARSOR_CONTROLLER__*`) reach
//! it; the others are removed from its environment (R6). The enrollment
//! code is the one other `NARSOR_*` variable it gets — never as an argument
//! (R12). The child uses it only when its own credential does not exist,
//! so a controller whose state was wiped enrolls again; an unused code
//! simply expires. The daemon checks the controller's
//! configuration before it binds its API
//! ([`crate::settings::configure`]). `RUST_LOG` follows the daemon's log
//! level; the child's stderr is copied, line by line and prefixed, to the
//! daemon's own stderr, which is the daemon's log (R4).
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
//! [`child_process::RESTART_RESET_AFTER`] (R9). A restart gets a fresh code
//! too. [`child_process`] holds what this supervision
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
use narsor_engine::config::local_controller;
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

/// Longest wait for the child's lease when it may first move its files to
/// the new layout (`--legacy-dir`, KTD10): a copy of the task storage across
/// filesystems can take long, and a child killed on the way would start the
/// copy again at every start. Clients stop waiting long before; they find
/// the daemon once the move is done.
const MIGRATION_READY_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How long the child waits for a predecessor holding its credential lock.
const LOCK_TIMEOUT: Duration = Duration::from_secs(7);

/// How long the enrollment code of one start stays valid: enough for the
/// child's wait on its lock and its first request, short enough that an
/// unused code is worth nothing soon after.
const ENROLLMENT_TTL: Duration = Duration::from_secs(120);

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
    /// The state root (`server.state`), handed on as `--state-root`.
    pub state_root: PathBuf,
    /// The work root (`server.work`), handed on as `--work-root`.
    pub work_root: PathBuf,
    /// The controller's old directories (state side, work side) while they
    /// exist, handed on as `--legacy-dir` / `--legacy-work-dir` so the
    /// controller moves its own files (KTD10).
    pub legacy_dirs: Option<(PathBuf, PathBuf)>,
    /// The arguments naming the child's configuration file:
    /// `--config <file>` or `--no-config`.
    pub config_args: Vec<OsString>,
    /// The prefixes of the daemon's `NARSOR_*` variables the child gets.
    pub forwarded_env_prefixes: Vec<String>,
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
    /// Starts the child with a fresh enrollment code and waits for its
    /// lease.
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
        let timing = StartTiming::of(&settings);
        if settings.legacy_dirs.is_some() {
            tracing::info!(
                wait_mins = timing.ready.as_secs() / 60,
                "daemon: the local controller may first move its files to the new layout; waiting for it"
            );
        }
        // The first code is made here, so a control plane that cannot make
        // one fails the start at once rather than in the supervising thread.
        let code = enrollment_code(&control, timing.enrollment_ttl)?;

        let shared = Arc::new(Shared {
            phase: Mutex::new(Phase::Starting),
            stderr: Mutex::new(VecDeque::new()),
        });
        let (stop, stopped) = mpsc::channel();
        let thread = {
            let shared = Arc::clone(&shared);
            let control_for_thread = Arc::clone(&control);
            std::thread::Builder::new()
                .name("local-controller".into())
                .spawn(move || supervise(&settings, &control_for_thread, code, &shared, &stopped))
                .map_err(|error| {
                    format!("cannot start the local controller's supervisor: {error}")
                })?
        };
        let controller = Self { stop, thread };

        let deadline = Instant::now() + timing.ready;
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
                    timing.ready.as_secs(),
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

/// How long the first start of the child may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StartTiming {
    /// The longest wait for the child's lease.
    ready: Duration,
    /// How long the first enrollment code stays valid: past the wait, as
    /// the child uses it only after its migration and its lock.
    enrollment_ttl: Duration,
}

impl StartTiming {
    fn of(settings: &LocalControllerSettings) -> Self {
        if settings.legacy_dirs.is_some() {
            Self {
                ready: MIGRATION_READY_TIMEOUT,
                enrollment_ttl: MIGRATION_READY_TIMEOUT + ENROLLMENT_TTL,
            }
        } else {
            Self {
                ready: READY_TIMEOUT,
                enrollment_ttl: ENROLLMENT_TTL,
            }
        }
    }
}

/// A short-lived one-time code enrolling the child of one start, used by
/// the child only when it has no credential.
fn enrollment_code(control: &ProtocolService, ttl: Duration) -> Result<String, String> {
    control
        .create_builtin_local_enrollment(ttl)
        .map(|response| response.enrollment_code)
        .map_err(|error| {
            format!("could not create the local controller's enrollment code: {error}")
        })
}

/// The supervising thread: starts the child, restarts it after the start,
/// stops it on request. Every start but the first gets a fresh code.
fn supervise(
    settings: &LocalControllerSettings,
    control: &ProtocolService,
    first_code: String,
    shared: &Arc<Shared>,
    stopped: &mpsc::Receiver<()>,
) {
    let policy = Supervision::default();
    let mut delays = RestartDelay::new(policy.restart_after);
    let mut code = Some(first_code);
    loop {
        let started = Instant::now();
        let start_code = match code.take() {
            Some(code) => Some(code),
            None => enrollment_code(control, ENROLLMENT_TTL)
                .inspect_err(|error| tracing::warn!(%error, "daemon: starting the local controller without one"))
                .ok(),
        };
        let outcome = match spawn(settings, start_code, shared) {
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
        .args(local_controller::root_args(
            &settings.state_root,
            Some(&settings.work_root),
        ))
        .args(
            settings
                .legacy_dirs
                .as_ref()
                .map(|(state, work)| local_controller::legacy_args(state, work))
                .unwrap_or_default(),
        )
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
        .args(&settings.config_args);
    child_process::prepare(
        &mut command,
        settings.log_directive.as_deref(),
        &settings.forwarded_env_prefixes,
    );
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

    /// A start that may migrate waits much longer than the clients, and its
    /// code outlives the wait either way.
    #[test]
    fn a_start_that_may_migrate_waits_long_for_the_lease() {
        for (legacy_dirs, ready) in [
            (None, READY_TIMEOUT),
            (
                Some((PathBuf::from("/old/state"), PathBuf::from("/old/work"))),
                MIGRATION_READY_TIMEOUT,
            ),
        ] {
            let settings = LocalControllerSettings {
                program: PathBuf::from("narsor-controller"),
                control_plane: "http://127.0.0.1:1".into(),
                state_root: PathBuf::from("/state"),
                work_root: PathBuf::from("/work"),
                legacy_dirs,
                config_args: Vec::new(),
                forwarded_env_prefixes: Vec::new(),
                log_directive: None,
            };

            let timing = StartTiming::of(&settings);

            assert_eq!(timing.ready, ready);
            assert!(timing.enrollment_ttl > timing.ready, "{timing:?}");
        }
    }

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
