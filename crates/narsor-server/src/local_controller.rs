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
//! doubled on each further failure up to [`MAX_RESTART_DELAY`] and reset
//! once one ran for [`RESTART_RESET_AFTER`] (R9). A restart enrolls nothing:
//! the credential is there.
//!
//! # Stopping
//!
//! `SIGTERM`, then up to [`Supervision`]'s stop timeout for the child to
//! release its lease, then `SIGKILL` — while the API still accepts the
//! release (R8).

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{self, BufRead as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use narsor_controller_runtime::{ControllerConfig, Supervision};
use narsor_engine::controller_protocol::ProtocolService;
use narsor_engine::domain::BackendId;
use narsor_engine::environment::BackendRecord;

/// The controller binary, installed next to `narsor-server`.
pub const CONTROLLER_PROGRAM: &str = "narsor-controller";

/// Longest wait for the child's lease, counted from its start: less than
/// the clients' discovery timeout (15 s), with room for the child's own
/// wait on a predecessor's lock ([`LOCK_TIMEOUT`]).
pub const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the child waits for a predecessor holding its credential lock.
pub const LOCK_TIMEOUT: Duration = Duration::from_secs(7);

/// Longest pause before a restart.
pub const MAX_RESTART_DELAY: Duration = Duration::from_secs(30);

/// A child that ran this long starts the restart pauses over.
pub const RESTART_RESET_AFTER: Duration = Duration::from_secs(60);

/// How many of the child's last stderr lines a start failure quotes.
const STDERR_TAIL: usize = 20;

/// How often a starting daemon looks for the child's lease, and the
/// supervising thread for the child's end.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// The prefix of the daemon's own settings, none of which the child gets.
const NARSOR_ENV_PREFIX: &str = "NARSOR_";

/// The variable the child reads its enrollment code from.
const ENROLLMENT_CODE_ENV: &str = "NARSOR_ENROLLMENT_CODE";

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

/// The `narsor-controller` next to this executable — in a debug build also
/// the one in Cargo's profile directory, for a test harness under `deps/`.
/// Never one from `PATH` (KTD9).
pub fn controller_program() -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("cannot find the narsor-server executable: {error}"))?;
    let adjacent = exe.with_file_name(CONTROLLER_PROGRAM);
    if adjacent.is_file() {
        return Ok(adjacent);
    }
    #[cfg(debug_assertions)]
    if let Some(profile) = exe.parent().and_then(Path::parent) {
        let built = profile.join(CONTROLLER_PROGRAM);
        if built.is_file() {
            return Ok(built);
        }
    }
    Err(format!(
        "{} is missing; narsor-server, narsor-controller and narsor-runner must be installed \
         in one directory",
        adjacent.display()
    ))
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
    let mut delay = policy.restart_after;
    loop {
        let started = Instant::now();
        let outcome = match spawn(settings, code.take(), shared) {
            Ok((mut child, stderr)) => {
                let outcome = watch(&mut child, stopped, policy.stop_timeout);
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
        if started.elapsed() >= RESTART_RESET_AFTER {
            delay = policy.restart_after;
        }
        tracing::warn!(
            reason = %reason,
            restart_in_secs = delay.as_secs(),
            "daemon: the local controller ended; starting it again"
        );
        match stopped.recv_timeout(delay) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
        }
        delay = (delay * 2).min(MAX_RESTART_DELAY);
    }
}

/// How one incarnation ended.
enum Ended {
    /// On request.
    Stopped,
    /// On its own, or it never started.
    Exited(Result<ExitStatus, String>),
}

/// Waits for the child's end or a stop request, whichever comes first.
fn watch(child: &mut Child, stopped: &mpsc::Receiver<()>, stop_timeout: Duration) -> Ended {
    loop {
        match stopped.recv_timeout(POLL_INTERVAL) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                terminate(child, stop_timeout);
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

/// `SIGTERM`, up to `timeout` for the child to release its lease and end,
/// then `SIGKILL`.
fn terminate(child: &mut Child, timeout: Duration) {
    signal(child.id(), libc::SIGTERM);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(Some(_)) | Err(_) = child.try_wait() {
            return;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    tracing::warn!("daemon: the local controller did not stop in time; killing it");
    let _ = child.kill();
    let _ = child.wait();
}

fn signal(pid: u32, signal: i32) {
    // SAFETY: signals the child this thread started and has not reaped.
    unsafe {
        libc::kill(pid as libc::pid_t, signal);
    }
}

/// Starts one incarnation, with a thread copying its stderr to the log.
fn spawn(
    settings: &LocalControllerSettings,
    code: Option<String>,
    shared: &Arc<Shared>,
) -> io::Result<(Child, std::thread::JoinHandle<()>)> {
    use std::os::unix::process::CommandExt;

    let parent = std::process::id();
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
        .arg(parent.to_string())
        .arg("--no-config");
    for (name, _) in std::env::vars_os() {
        if name
            .as_encoded_bytes()
            .starts_with(NARSOR_ENV_PREFIX.as_bytes())
        {
            command.env_remove(name);
        }
    }
    match &settings.log_directive {
        Some(directive) => command.env("RUST_LOG", directive),
        None => command.env_remove("RUST_LOG"),
    };
    if let Some(code) = code {
        command.env(ENROLLMENT_CODE_ENV, code);
    }
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
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let stderr = child.stderr.take().expect("stderr is piped");
    let shared = Arc::clone(shared);
    let copier = std::thread::Builder::new()
        .name("local-controller-log".into())
        .spawn(move || {
            for line in io::BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                eprintln!("{LOG_PREFIX} {line}");
                shared.remember(line);
            }
        });
    match copier {
        Ok(copier) => Ok((child, copier)),
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(error)
        }
    }
}
