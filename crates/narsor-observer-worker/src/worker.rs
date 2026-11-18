//! The observer worker's loop: claim, run the script, report (U6).
//!
//! `narsor-server` starts `narsor-observer-worker` and supervises it when its configuration enables
//! the worker, and hands it a key it makes at every start of the worker in
//! [`OBSERVER_WORKER_KEY_ENV`] (D5). The key claims the runs of every
//! (queue, automation) pair and reports their results, nothing else.
//!
//! The control plane decides when an automation's script runs; the worker
//! only takes what is due and runs it:
//!
//! ```text
//! claim (worker key), held by the server up to 25 s until a run is due
//!                                                 → nothing due: claim again
//!                                                   at once (at most one
//!                                                   claim per CLAIM_PAUSE)
//!              run dir (0700, removed afterwards)
//!              sh script.sh, stdin = the input, env = the list below only
//!              stdout/stderr read with limits, time limit, process group
//!              result (secrets and token masked)  → claim again at once
//! ```
//!
//! # What the script gets
//!
//! An empty environment plus exactly: `PATH`, `HOME` (the run directory),
//! `NARSOR_SERVER`, `NARSOR_TOKEN` (the run's token, valid on the effect
//! routes of this one run only), `NARSOR_OBSERVER_RUN` and one variable per
//! secret the automation declares. Nothing of the worker's own environment
//! reaches it — neither the worker key nor a `GH_TOKEN` the worker was
//! started with.
//!
//! # Secrets
//!
//! The installation has one owner (A4), so the secrets directory is flat:
//! `{secrets-dir}/{name}`, one file per secret name. A script gets the
//! secrets its automation declares (`secrets: [{name, env}]`), and no
//! other. A declared secret the directory lacks fails the run before the
//! script starts.
//!
//! With `--script-user` the script runs as that system user, which must
//! not be able to read the worker's secrets directory or its environment
//! (`/proc/<pid>/environ` of another user is closed); the worker refuses to
//! start when the directory is open to anyone but its owner. The child also
//! drops what it could inherit: ambient capabilities are cleared and
//! `no_new_privs` is set before `exec`.
//!
//! # Ending the script
//!
//! The script leads a process group of its own. At its time limit the group
//! gets `SIGTERM`, then `SIGKILL` after a grace period. When the script
//! exits by itself whatever it left running in the group is killed at once:
//! nothing of a run outlives it, and a background process holding stdout
//! cannot keep the worker waiting (see
//! `docs/solutions/runtime-errors/agent-step-hangs-on-orphaned-background-process-2026-10-26.md`).
//! A process can leave the group (`setsid`). With `--script-user` every
//! process of that user is therefore killed after each run too, and the
//! worker claims no run while one of them still lives: all runs share the
//! user, so a process left behind could read the environment of the next
//! run (`/proc/<pid>/environ`) — its secrets and token. The script user
//! must therefore be a dedicated user that runs nothing else. The pipes are
//! drained for a bounded time only.
//!
//! # What the server gets
//!
//! stdout up to [`MAX_STDOUT_BYTES`] (more is a format error), stderr up to
//! [`MAX_STDERR_BYTES`]; both with control characters removed and every
//! occurrence of a secret value and of the run's token replaced by
//! [`MASK`] — before anything leaves the worker, error messages included.
//! Masking is the second line of defence: the first is that the script has
//! no reason to print them.
//!
//! The control plane and the process are behind two seams, [`WorkerApi`]
//! and [`ScriptRunner`], so the worker is tested without a server and,
//! where the process itself is not the subject, without `sh`.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use futures::future::BoxFuture;
#[cfg(doc)]
use narsor_protocol::controller::OBSERVER_WORKER_KEY_ENV;
use narsor_protocol::controller::{
    ObserverRunOutcome, ObserverRunReport, ObserverRunResultResponse, ObserverSecretRef,
    ObserverWorkerRun,
};
use narsor_protocol::display::without_unsafe_chars_multiline;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};

use narsor_api_client::api::{ApiClient, ClientError};

/// The shortest time between the starts of two claims that found no run,
/// and the pause after a failed claim. A claim the server held (its
/// long-poll) is followed by the next one at once; one answered at once
/// (an older server, a server that stops) waits out the rest of it.
pub const CLAIM_PAUSE: Duration = Duration::from_secs(2);

/// Most bytes of stdout a script may print: the script contract's limit
/// (`MAX_SCRIPT_OUTPUT_BYTES` of the control plane). More is a format
/// error, whatever the exit code.
pub const MAX_STDOUT_BYTES: usize = 1024 * 1024;

/// Most bytes of stderr sent to the control plane, which keeps no more of
/// it (`MAX_STORED_LOG_BYTES`). The rest is read and dropped.
pub const MAX_STDERR_BYTES: usize = 64 * 1024;

/// Largest secret file the worker reads.
pub const MAX_SECRET_BYTES: u64 = 64 * 1024;

/// What replaces a secret value or the run's token in the script's output.
pub const MASK: &str = "[masked]";

/// What ends stderr that was cut at [`MAX_STDERR_BYTES`].
pub const STDERR_TRUNCATED: &str = "\n[stderr truncated]";

/// Time between `SIGTERM` and `SIGKILL` at the script's time limit.
pub const KILL_GRACE: Duration = Duration::from_secs(5);

/// How long the pipes are drained after the process group is gone. Only a
/// process that left the group can hold them open longer.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Time kept before the run's deadline for killing the script and sending
/// the result: the script's limit is cut so that both fit.
pub const REPORT_MARGIN: Duration = Duration::from_secs(15);

/// Attempts to send a result when the control plane cannot be reached.
const REPORT_ATTEMPTS: u32 = 3;

/// Time between two attempts to send a result.
const REPORT_RETRY_DELAY: Duration = Duration::from_secs(2);

/// The shell the script runs in.
pub const SHELL: &str = "/bin/sh";

/// The file the script is written to, in the run directory.
pub const SCRIPT_FILE: &str = "script.sh";

/// `PATH` of the script after the directory of the worker program, where
/// `narsor-cli` is installed too.
pub const SYSTEM_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// Variables the worker sets itself; a secret may not take their name.
const RESERVED_ENVS: [&str; 2] = ["PATH", "HOME"];

// ---------------------------------------------------------------------------
// Seams
// ---------------------------------------------------------------------------

/// The worker's side of the control plane: claim a run, report its result.
pub trait WorkerApi: Send + Sync {
    /// The next due run, `None` when none is due.
    fn claim(&self) -> BoxFuture<'_, Result<Option<ObserverWorkerRun>, ClientError>>;

    /// Reports the result of a claimed run.
    fn report<'a>(
        &'a self,
        run_id: &'a str,
        report: &'a ObserverRunReport,
    ) -> BoxFuture<'a, Result<ObserverRunResultResponse, ClientError>>;
}

impl WorkerApi for ApiClient {
    fn claim(&self) -> BoxFuture<'_, Result<Option<ObserverWorkerRun>, ClientError>> {
        Box::pin(self.claim_observer_run())
    }

    fn report<'a>(
        &'a self,
        run_id: &'a str,
        report: &'a ObserverRunReport,
    ) -> BoxFuture<'a, Result<ObserverRunResultResponse, ClientError>> {
        Box::pin(self.report_observer_run(run_id, report))
    }
}

impl<T: WorkerApi + ?Sized> WorkerApi for Arc<T> {
    fn claim(&self) -> BoxFuture<'_, Result<Option<ObserverWorkerRun>, ClientError>> {
        (**self).claim()
    }

    fn report<'a>(
        &'a self,
        run_id: &'a str,
        report: &'a ObserverRunReport,
    ) -> BoxFuture<'a, Result<ObserverRunResultResponse, ClientError>> {
        (**self).report(run_id, report)
    }
}

/// The system user a script runs as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScriptUser {
    /// Its user id.
    pub uid: u32,
    /// Its primary group id.
    pub gid: u32,
}

impl ScriptUser {
    /// The user named `name` in the system's user database. Root is refused: it would undo the isolation.
    pub fn lookup(name: &str) -> io::Result<Self> {
        let user = lookup_user(name)?;
        if user.uid == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("the script user '{name}' is root; pick an unprivileged system user"),
            ));
        }
        Ok(user)
    }
}

/// How to start one script.
#[derive(Clone)]
pub struct ScriptSpec {
    /// The program (`/bin/sh`).
    pub program: PathBuf,
    /// Its arguments (the script file).
    pub args: Vec<String>,
    /// The whole environment of the process: nothing else is inherited.
    pub env: Vec<(String, String)>,
    /// Its working directory (the run directory).
    pub cwd: PathBuf,
    /// Written to its stdin, which is then closed.
    pub stdin: Vec<u8>,
    /// How long it may run.
    pub timeout: Duration,
    /// Time between `SIGTERM` and `SIGKILL` at the limit.
    pub kill_grace: Duration,
    /// The user it runs as; `None` for the worker's own.
    pub user: Option<ScriptUser>,
    /// Most bytes of stdout kept; the rest is read and counted.
    pub stdout_limit: usize,
    /// Most bytes of stderr kept; the rest is read and counted.
    pub stderr_limit: usize,
}

impl std::fmt::Debug for ScriptSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The environment holds the run's token and the secrets.
        let names: Vec<&str> = self.env.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("ScriptSpec")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("env", &names)
            .field("cwd", &self.cwd)
            .field("timeout", &self.timeout)
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

/// How a script ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptEnd {
    /// It exited with this code.
    Exited(i32),
    /// A signal it did not expect killed it.
    Signaled(i32),
    /// It ran past its time limit and was killed.
    TimedOut,
}

/// What a script left: how it ended and what it printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptExit {
    /// How it ended.
    pub end: ScriptEnd,
    /// The first [`ScriptSpec::stdout_limit`] bytes of stdout.
    pub stdout: Vec<u8>,
    /// How many bytes it printed on stdout in all.
    pub stdout_total: u64,
    /// The first [`ScriptSpec::stderr_limit`] bytes of stderr.
    pub stderr: Vec<u8>,
    /// How many bytes it printed on stderr in all.
    pub stderr_total: u64,
}

/// Runs a script process.
pub trait ScriptRunner: Send + Sync {
    /// Starts `spec`, waits for its end within its time limit and returns
    /// what it printed. An error means it could not be started.
    fn run(&self, spec: ScriptSpec) -> BoxFuture<'_, io::Result<ScriptExit>>;
}

impl<T: ScriptRunner + ?Sized> ScriptRunner for Arc<T> {
    fn run(&self, spec: ScriptSpec) -> BoxFuture<'_, io::Result<ScriptExit>> {
        (**self).run(spec)
    }
}

// ---------------------------------------------------------------------------
// The worker
// ---------------------------------------------------------------------------

/// How the worker runs scripts.
#[derive(Debug, Clone)]
pub struct WorkerOptions {
    /// The control plane's URL, the script's `NARSOR_SERVER`.
    pub server: String,
    /// One file per secret reference name; `None`: no secrets available.
    pub secrets_dir: Option<PathBuf>,
    /// The user scripts run as; `None` for the worker's own.
    pub script_user: Option<ScriptUser>,
    /// Where run directories are made.
    pub work_dir: PathBuf,
    /// The script's `PATH`.
    pub path: String,
    /// The shell (`/bin/sh`).
    pub shell: PathBuf,
    /// Time between `SIGTERM` and `SIGKILL` at the time limit.
    pub kill_grace: Duration,
}

impl WorkerOptions {
    /// Options with the defaults: no secrets, the worker's own user, the
    /// system's temporary directory, [`default_script_path`], [`SHELL`].
    pub fn new(server: impl Into<String>) -> Self {
        Self {
            server: server.into(),
            secrets_dir: None,
            script_user: None,
            work_dir: std::env::temp_dir(),
            path: default_script_path(),
            shell: PathBuf::from(SHELL),
            kill_grace: KILL_GRACE,
        }
    }
}

/// The script's default `PATH`: the directory of this program, where
/// `narsor-cli` is installed too, so the script finds it, then
/// [`SYSTEM_PATH`].
pub fn default_script_path() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .and_then(|dir| dir.to_str().map(str::to_string))
        .filter(|dir| !dir.is_empty() && !dir.contains(':'))
        .map_or_else(
            || SYSTEM_PATH.to_string(),
            |dir| format!("{dir}:{SYSTEM_PATH}"),
        )
}

/// One run the worker handled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandledRun {
    /// The run.
    pub run_id: String,
    /// What the worker sent.
    pub report: ObserverRunReport,
    /// What the control plane answered, when it took the result.
    pub accepted: Option<ObserverRunResultResponse>,
}

/// Why the worker stopped.
#[derive(Debug)]
pub enum WorkerStop {
    /// The control plane refused the worker's key: one of an earlier start,
    /// or no worker key at all. Polling on would not change that.
    CredentialRefused(ClientError),
    /// A claim failed (with `--once`).
    Claim(ClientError),
}

impl std::fmt::Display for WorkerStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkerStop::CredentialRefused(error) => write!(
                f,
                "the control plane refused the worker key (one of an earlier start of the \
                 worker?): {error}"
            ),
            WorkerStop::Claim(error) => write!(f, "could not claim a run: {error}"),
        }
    }
}

/// The observer worker.
pub struct Worker<A, R> {
    api: A,
    runner: R,
    options: WorkerOptions,
}

impl<A: WorkerApi, R: ScriptRunner> Worker<A, R> {
    /// A worker claiming through `api` and running scripts with `runner`.
    pub fn new(api: A, runner: R, options: WorkerOptions) -> Self {
        Self {
            api,
            runner,
            options,
        }
    }

    /// Claims one run and handles it. `Ok(None)` when none was due.
    ///
    /// With a script user, claims nothing while a process of that user
    /// runs (one that escaped an earlier run, say) and cannot be killed:
    /// it could read the environment — secrets and token — of the next run.
    pub async fn run_once(&self) -> Result<Option<HandledRun>, ClientError> {
        if let Some(user) = self.options.script_user {
            if let Err(left) = clear_processes_of(user).await {
                tracing::error!(uid = user.uid, processes = ?left, "observer worker: the script user has processes that cannot be killed; not claiming a run");
                return Ok(None);
            }
        }
        let Some(run) = self.api.claim().await? else {
            return Ok(None);
        };
        tracing::info!(run = %run.run_id, phase = %run.phase, script = %run.script_digest, "observer worker: claimed a run");
        let report = self.execute(&run).await;
        let accepted = self.send_report(&run.run_id, &report).await;
        Ok(Some(HandledRun {
            run_id: run.run_id,
            report,
            accepted,
        }))
    }

    /// Claims and handles runs until `shutdown` resolves: a due run is
    /// followed by the next claim at once, an empty claim by the next one
    /// once `pause` passed since it started (at once after a claim the
    /// server held that long), a failed claim by `pause`. With `once`,
    /// stops after the first claim, due or not.
    pub async fn serve(
        &self,
        once: bool,
        pause: Duration,
        shutdown: impl std::future::Future<Output = ()>,
    ) -> Result<(), WorkerStop> {
        tokio::pin!(shutdown);
        loop {
            let started = tokio::time::Instant::now();
            let claimed = tokio::select! {
                // Dropping a run in progress kills its process group.
                () = &mut shutdown => return Ok(()),
                claimed = self.run_once() => claimed,
            };
            let wait = match claimed {
                Ok(Some(_)) if once => return Ok(()),
                Ok(Some(_)) => continue,
                Ok(None) if once => return Ok(()),
                Ok(None) => pause.saturating_sub(started.elapsed()),
                Err(error @ (ClientError::Unauthorized | ClientError::Forbidden)) => {
                    return Err(WorkerStop::CredentialRefused(error));
                }
                Err(error) if once => return Err(WorkerStop::Claim(error)),
                Err(error) => {
                    tracing::warn!(%error, "observer worker: claim failed; trying again");
                    pause
                }
            };
            tokio::select! {
                () = &mut shutdown => return Ok(()),
                () = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// Runs the script of `run` and builds the result to send: never
    /// fails, a problem of the worker is a [`ObserverRunOutcome::WorkerError`].
    pub async fn execute(&self, run: &ObserverWorkerRun) -> ObserverRunReport {
        let mut masker = Masker::new([run.token.as_str()]);
        let secrets = match self.read_secrets(&run.secrets) {
            Ok(secrets) => secrets,
            Err(message) => return worker_error(&masker, &message),
        };
        for (_, value) in &secrets {
            masker.add(value);
        }
        let run_dir = match RunDir::create(&self.options, &run.run_id, &run.script) {
            Ok(run_dir) => run_dir,
            Err(error) => {
                return worker_error(
                    &masker,
                    &format!("cannot prepare the run directory: {error}"),
                )
            }
        };
        let spec = self.spec(run, &run_dir, secrets, &masker);
        let exit = self.runner.run(spec).await;
        drop(run_dir);
        match exit {
            Ok(exit) => report_of(exit, &masker),
            Err(error) => worker_error(&masker, &format!("cannot start the script: {error}")),
        }
    }

    /// The secret values of `references`, as `(variable, value)`, from the
    /// secrets directory. Fails, naming the reference only, when one cannot
    /// be read.
    fn read_secrets(
        &self,
        references: &[ObserverSecretRef],
    ) -> Result<Vec<(String, String)>, String> {
        let mut secrets = Vec::new();
        for reference in references {
            let name = &reference.name;
            if !is_plain_name(name) {
                return Err(format!(
                    "the secret reference '{}' is not one plain file name",
                    without_unsafe_chars_multiline(name)
                ));
            }
            if !is_valid_env(&reference.env) {
                return Err(format!(
                    "the secret reference '{name}' names the variable '{}', which the worker \
                     does not hand to a script",
                    without_unsafe_chars_multiline(&reference.env)
                ));
            }
            let Some(dir) = &self.options.secrets_dir else {
                return Err(format!(
                    "the automation needs the secret '{name}', but the worker has no secrets \
                     directory (--secrets-dir)"
                ));
            };
            let value = read_secret(&dir.join(name))
                .map_err(|problem| format!("the secret '{name}' cannot be used: {problem}"))?;
            secrets.push((reference.env.clone(), value));
        }
        Ok(secrets)
    }

    /// The process of `run`: its environment is the explicit list and
    /// nothing else.
    fn spec(
        &self,
        run: &ObserverWorkerRun,
        run_dir: &RunDir,
        secrets: Vec<(String, String)>,
        masker: &Masker,
    ) -> ScriptSpec {
        let home = run_dir.path().to_string_lossy().into_owned();
        let mut env = vec![
            ("PATH".to_string(), self.options.path.clone()),
            ("HOME".to_string(), home),
            ("NARSOR_SERVER".to_string(), self.options.server.clone()),
            ("NARSOR_TOKEN".to_string(), run.token.clone()),
            ("NARSOR_OBSERVER_RUN".to_string(), run.run_id.clone()),
        ];
        env.extend(secrets);
        // A secret cut in two by the limit is still masked whole: keep
        // enough beyond it to see the rest.
        let slack = masker.longest();
        ScriptSpec {
            program: self.options.shell.clone(),
            args: vec![SCRIPT_FILE.to_string()],
            env,
            cwd: run_dir.path().to_path_buf(),
            stdin: serde_json::to_vec(&run.input).unwrap_or_default(),
            timeout: script_timeout(run, self.options.kill_grace),
            kill_grace: self.options.kill_grace,
            user: self.options.script_user,
            stdout_limit: MAX_STDOUT_BYTES + slack,
            stderr_limit: MAX_STDERR_BYTES + slack,
        }
    }

    /// Sends `report`, again when the control plane cannot be reached. A
    /// refusal is final: the run ended (lost at its deadline, say).
    async fn send_report(
        &self,
        run_id: &str,
        report: &ObserverRunReport,
    ) -> Option<ObserverRunResultResponse> {
        for attempt in 1..=REPORT_ATTEMPTS {
            match self.api.report(run_id, report).await {
                Ok(accepted) => {
                    tracing::info!(run = %run_id, status = %accepted.status, "observer worker: result accepted");
                    return Some(accepted);
                }
                Err(error @ (ClientError::Unavailable(_) | ClientError::Server(_)))
                    if attempt < REPORT_ATTEMPTS =>
                {
                    tracing::warn!(run = %run_id, %error, "observer worker: cannot send the result; trying again");
                    tokio::time::sleep(REPORT_RETRY_DELAY).await;
                }
                Err(error) => {
                    tracing::warn!(run = %run_id, %error, "observer worker: the result was not accepted");
                    return None;
                }
            }
        }
        None
    }
}

/// The script's time limit: the run's, cut so that killing the script and
/// sending the result fit before the run's deadline. At least one second.
fn script_timeout(run: &ObserverWorkerRun, kill_grace: Duration) -> Duration {
    let limit = Duration::from_secs(run.timeout_secs.max(1));
    let left = (run.deadline - Utc::now())
        .to_std()
        .unwrap_or_default()
        .saturating_sub(kill_grace + REPORT_MARGIN);
    limit.min(left).max(Duration::from_secs(1))
}

/// A worker error, masked like everything else the server gets.
fn worker_error(masker: &Masker, message: &str) -> ObserverRunReport {
    tracing::warn!(message = %masker.mask(message), "observer worker: the run failed before the script ran");
    ObserverRunReport {
        outcome: ObserverRunOutcome::WorkerError {
            message: masker.mask(message),
        },
        stdout: String::new(),
        stderr: String::new(),
    }
}

/// The result of a script that ran, masked and bounded.
fn report_of(exit: ScriptExit, masker: &Masker) -> ObserverRunReport {
    let stdout = masker.mask(&String::from_utf8_lossy(&exit.stdout));
    let too_large = exit.stdout_total > MAX_STDOUT_BYTES as u64 || stdout.len() > MAX_STDOUT_BYTES;
    let mut stderr = masker.mask(&String::from_utf8_lossy(&exit.stderr));
    if exit.stderr_total > MAX_STDERR_BYTES as u64 || stderr.len() > MAX_STDERR_BYTES {
        stderr = cut(&stderr, MAX_STDERR_BYTES - STDERR_TRUNCATED.len()).to_string();
        stderr.push_str(STDERR_TRUNCATED);
    }
    let exit_code = match exit.end {
        ScriptEnd::Exited(code) => Some(code),
        ScriptEnd::Signaled(signal) => Some(128 + signal),
        ScriptEnd::TimedOut => None,
    };
    let (outcome, stdout) = match exit_code {
        None => (
            ObserverRunOutcome::TimedOut,
            cut(&stdout, MAX_STDERR_BYTES).to_string(),
        ),
        Some(exit_code) if too_large => (
            ObserverRunOutcome::OutputTooLarge { exit_code },
            // The control plane keeps no more of it than of stderr.
            cut(&stdout, MAX_STDERR_BYTES).to_string(),
        ),
        Some(exit_code) => (ObserverRunOutcome::Exited { exit_code }, stdout),
    };
    ObserverRunReport {
        outcome,
        stdout,
        stderr,
    }
}

/// `text` cut to at most `max` bytes, on a character boundary.
fn cut(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

// ---------------------------------------------------------------------------
// Masking
// ---------------------------------------------------------------------------

/// Replaces secret values in text bound for the control plane.
#[derive(Debug, Clone, Default)]
pub struct Masker {
    /// Longest first, so a secret that contains another is masked whole.
    needles: Vec<String>,
}

impl Masker {
    /// A masker of `values` (empty ones are ignored).
    pub fn new<'a>(values: impl IntoIterator<Item = &'a str>) -> Self {
        let mut masker = Self::default();
        for value in values {
            masker.add(value);
        }
        masker
    }

    /// Masks `value` too, as given and as it reads once its control
    /// characters are gone (text is cleaned before it is masked).
    pub fn add(&mut self, value: &str) {
        for needle in [value.to_string(), without_unsafe_chars_multiline(value)] {
            if !needle.is_empty() && !self.needles.contains(&needle) {
                self.needles.push(needle);
            }
        }
        self.needles
            .sort_by(|left, right| right.len().cmp(&left.len()).then(left.cmp(right)));
    }

    /// The length of the longest value, in bytes.
    pub fn longest(&self) -> usize {
        self.needles.first().map_or(0, String::len)
    }

    /// `text` without control characters (line feeds stay, tabs become
    /// spaces) and with every occurrence of a value replaced by [`MASK`].
    pub fn mask(&self, text: &str) -> String {
        let mut text = without_unsafe_chars_multiline(text);
        for needle in &self.needles {
            if text.contains(needle.as_str()) {
                text = text.replace(needle.as_str(), MASK);
            }
        }
        text
    }
}

// ---------------------------------------------------------------------------
// Secrets and the run directory
// ---------------------------------------------------------------------------

/// A secret's value: the file's text without one trailing line feed.
fn read_secret(path: &Path) -> Result<String, String> {
    let metadata = fs::metadata(path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => "it is not in the secrets directory".to_string(),
        _ => format!("it cannot be read ({})", error.kind()),
    })?;
    if !metadata.is_file() {
        return Err("it is not a file in the secrets directory".to_string());
    }
    if metadata.len() > MAX_SECRET_BYTES {
        return Err(format!("it is larger than {MAX_SECRET_BYTES} bytes"));
    }
    let bytes = fs::read(path).map_err(|error| format!("it cannot be read ({})", error.kind()))?;
    let value = String::from_utf8(bytes).map_err(|_| "it is not UTF-8 text".to_string())?;
    let value = value.strip_suffix('\n').unwrap_or(&value);
    if value.contains('\0') {
        return Err("it holds a NUL byte".to_string());
    }
    Ok(value.to_string())
}

/// One path component, not hidden, no separator.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.contains('/')
        && !name.contains('\0')
        && name.len() <= 255
}

/// `[A-Z_][A-Z0-9_]*`, and none the worker sets itself.
fn is_valid_env(env: &str) -> bool {
    let bytes = env.as_bytes();
    !bytes.is_empty()
        && (bytes[0].is_ascii_uppercase() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_')
        && !RESERVED_ENVS.contains(&env)
        && !env.starts_with("NARSOR_")
}

/// The run's working directory and `HOME`, mode `0700`, owned by the
/// script's user; removed when dropped.
struct RunDir {
    path: PathBuf,
    user: Option<ScriptUser>,
}

impl RunDir {
    /// Creates the directory with the script in it.
    fn create(options: &WorkerOptions, run_id: &str, script: &str) -> io::Result<Self> {
        let safe_id: String = run_id
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '.' | '_'))
            .take(80)
            .collect();
        let path = options.work_dir.join(format!(
            "narsor-observer-run-{safe_id}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        let run_dir = Self {
            path,
            user: options.script_user,
        };
        write_script(&run_dir.path, script, run_dir.user)?;
        Ok(run_dir)
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

/// Writes the script into the run directory `dir` and gives the file and
/// the directory to `user`, through file descriptors only. The directory is
/// in a temp directory that other processes of the worker's uid share: one
/// of them can put a symlink in place of `dir` or of the script between two
/// calls, and a `chown` by path follows it — with `CAP_CHOWN` that gives
/// any host file to the script user. `O_NOFOLLOW` refuses a symlink, and
/// `O_EXCL` a script file that is already there.
fn write_script(dir: &Path, script: &str, user: Option<ScriptUser>) -> io::Result<()> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    let dir = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir)?;
    let name = CString::new(SCRIPT_FILE).expect("the script file name has no NUL byte");
    // SAFETY: `dir` is an open directory and `name` a NUL-terminated string;
    // both live past the call.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o400 as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `openat` returned a new descriptor that nothing else owns.
    let mut file = unsafe { fs::File::from_raw_fd(fd) };
    io::Write::write_all(&mut file, script.as_bytes())?;
    if let Some(user) = user {
        std::os::unix::fs::fchown(&file, Some(user.uid), Some(user.gid))?;
        std::os::unix::fs::fchown(&dir, Some(user.uid), Some(user.gid))?;
    }
    Ok(())
}

impl Drop for RunDir {
    fn drop(&mut self) {
        if fs::remove_dir_all(&self.path).is_ok() || !self.path.exists() {
            return;
        }
        // What the script user made in its own directory, the worker may
        // not be allowed to remove; the script user is.
        if let Some(user) = self.user {
            let _ = std::process::Command::new("rm")
                .arg("-rf")
                .arg("--")
                .arg(&self.path)
                .env_clear()
                .env("PATH", SYSTEM_PATH)
                .uid(user.uid)
                .gid(user.gid)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            let _ = fs::remove_dir_all(&self.path);
        }
        if self.path.exists() {
            tracing::warn!(path = %self.path.display(), "observer worker: cannot remove a run directory");
        }
    }
}

// ---------------------------------------------------------------------------
// Startup checks
// ---------------------------------------------------------------------------

/// Refuses a secrets directory anyone but its owner can open.
pub fn check_secrets_dir(path: &Path) -> Result<(), String> {
    let metadata = fs::metadata(path).map_err(|error| {
        format!(
            "cannot read the secrets directory {}: {error}",
            path.display()
        )
    })?;
    if !metadata.is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "the secrets directory {} is open to other users; make it accessible to its owner \
             only (chmod 700)",
            path.display()
        ));
    }
    Ok(())
}

/// The user `name` of the user database.
fn lookup_user(name: &str) -> io::Result<ScriptUser> {
    let not_found = || {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no system user named '{name}'"),
        )
    };
    let cname = CString::new(name).map_err(|_| not_found())?;
    let mut buffer = vec![0u8; 16 * 1024];
    // SAFETY: `passwd` is plain data; zeroed is a valid value to be
    // overwritten by `getpwnam_r`.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is valid for the call and the buffer's length
    // is its own; the strings in `entry` point into `buffer`, which we do
    // not read.
    let code = unsafe {
        libc::getpwnam_r(
            cname.as_ptr(),
            &mut entry,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut found,
        )
    };
    if code != 0 {
        return Err(io::Error::from_raw_os_error(code));
    }
    if found.is_null() {
        return Err(not_found());
    }
    Ok(ScriptUser {
        uid: entry.pw_uid,
        gid: entry.pw_gid,
    })
}

/// The worker's own user id.
pub fn current_uid() -> u32 {
    // SAFETY: `getuid` takes nothing and cannot fail.
    unsafe { libc::getuid() }
}

// ---------------------------------------------------------------------------
// The process
// ---------------------------------------------------------------------------

/// Runs scripts as real processes, each the leader of its own group.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessRunner;

impl ScriptRunner for ProcessRunner {
    fn run(&self, spec: ScriptSpec) -> BoxFuture<'_, io::Result<ScriptExit>> {
        Box::pin(run_process(spec))
    }
}

/// Kills the group, and with a script user every process of that user,
/// when dropped while armed: a run dropped half-way (the worker stopping)
/// leaves nothing behind.
struct GroupGuard {
    pgid: Option<i32>,
    user: Option<ScriptUser>,
}

impl GroupGuard {
    fn kill(&self, signal: i32) {
        if let Some(pgid) = self.pgid {
            // SAFETY: `killpg` takes two integers and touches no memory.
            unsafe {
                libc::killpg(pgid, signal);
            }
        }
    }

    /// Kills what is left of the run and disarms the guard. A process that
    /// left the group (`setsid`) still runs as the script user: with one,
    /// every process of that user is killed too, so nothing of this run can
    /// read the environment of the next one.
    fn finish(&mut self) {
        self.kill(libc::SIGKILL);
        self.pgid = None;
        if let Some(user) = self.user.take() {
            if let Err(error) = kill_all_processes_of(user) {
                tracing::warn!(%error, uid = user.uid, "observer worker: cannot kill the script user's processes");
            }
        }
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Sends `SIGKILL` to every process of `user`: a helper process switches
/// to that user, drops every capability, and calls `kill(-1)`, which
/// reaches every process the user may signal — its own — except the helper
/// itself. It cannot reach a process of anyone else.
///
/// A switch from one non-zero user id to another keeps the capabilities
/// (a worker that is not root holds `CAP_KILL` in its ambient set), so the
/// helper clears them itself and does not call `kill(-1)` if `CAP_KILL`
/// is still there.
pub fn kill_all_processes_of(user: ScriptUser) -> io::Result<()> {
    let mut command = std::process::Command::new(SHELL);
    command
        .args(["-c", ":"])
        .env_clear()
        .env("PATH", SYSTEM_PATH)
        .current_dir("/")
        .uid(user.uid)
        .gid(user.gid)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: the hook only calls `prctl`, `capset`, `capget` and `kill`,
    // all plain system calls, async-signal-safe. It runs after the switch
    // to `user`.
    unsafe {
        command.pre_exec(|| {
            #[cfg(target_os = "linux")]
            {
                drop_inherited_privileges()?;
                drop_all_capabilities()?;
            }
            // `ESRCH` (nothing to kill) is not a failure.
            libc::kill(-1, libc::SIGKILL);
            Ok(())
        });
    }
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("the helper ended with {status}")))
    }
}

/// The processes running as `uid` (real, effective, saved or file-system
/// user id), zombies and this process excepted. Empty where `/proc` is not
/// readable.
pub fn processes_of(uid: u32) -> Vec<i32> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    let own = std::process::id();
    let uid = uid.to_string();
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| *pid != own)
        .filter(|pid| {
            let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) else {
                return false;
            };
            let field = |name: &str| {
                status
                    .lines()
                    .find_map(|line| line.strip_prefix(name))
                    .map(str::trim)
            };
            let zombie = field("State:").is_some_and(|state| state.starts_with('Z'));
            let ours = field("Uid:").is_some_and(|ids| ids.split_whitespace().any(|id| id == uid));
            ours && !zombie
        })
        .filter_map(|pid| i32::try_from(pid).ok())
        .collect()
}

/// How long the worker waits for the script user's processes to end
/// before it refuses to claim.
const LEFTOVER_WAIT: Duration = Duration::from_secs(2);

/// Makes sure no process of `user` runs: kills what is left and waits for
/// it to end. `Err` lists what still runs.
pub async fn clear_processes_of(user: ScriptUser) -> Result<(), Vec<i32>> {
    let mut left = processes_of(user.uid);
    if left.is_empty() {
        return Ok(());
    }
    tracing::warn!(uid = user.uid, processes = ?left, "observer worker: the script user still has processes; killing them");
    let started = std::time::Instant::now();
    while started.elapsed() < LEFTOVER_WAIT {
        if let Err(error) = kill_all_processes_of(user) {
            tracing::warn!(%error, uid = user.uid, "observer worker: cannot kill the script user's processes");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        left = processes_of(user.uid);
        if left.is_empty() {
            return Ok(());
        }
    }
    Err(left)
}

/// What one pipe gave so far, shared with its reader.
#[derive(Debug, Default)]
struct Captured {
    kept: Vec<u8>,
    total: u64,
}

async fn run_process(spec: ScriptSpec) -> io::Result<ScriptExit> {
    let mut command = tokio::process::Command::new(&spec.program);
    command
        .args(&spec.args)
        .env_clear()
        .envs(spec.env.iter().map(|(name, value)| (name, value)))
        .current_dir(&spec.cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    if let Some(user) = spec.user {
        command.uid(user.uid).gid(user.gid);
    }
    #[cfg(target_os = "linux")]
    // SAFETY: the hook only calls `prctl`, which is async-signal-safe.
    unsafe {
        command.pre_exec(drop_inherited_privileges);
    }
    let mut child = command.spawn()?;
    let mut guard = GroupGuard {
        pgid: child.id().and_then(|pid| i32::try_from(pid).ok()),
        user: spec.user,
    };

    let stdin = child.stdin.take();
    let input = spec.stdin;
    let writer = tokio::spawn(async move {
        if let Some(mut stdin) = stdin {
            // A script that does not read its input closes the pipe early.
            let _ = stdin.write_all(&input).await;
        }
    });
    let stdout = Arc::new(Mutex::new(Captured::default()));
    let stderr = Arc::new(Mutex::new(Captured::default()));
    let stdout_reader = child
        .stdout
        .take()
        .map(|pipe| tokio::spawn(read_bounded(pipe, spec.stdout_limit, Arc::clone(&stdout))));
    let stderr_reader = child
        .stderr
        .take()
        .map(|pipe| tokio::spawn(read_bounded(pipe, spec.stderr_limit, Arc::clone(&stderr))));

    let end = match tokio::time::timeout(spec.timeout, child.wait()).await {
        Ok(status) => end_of(status?),
        Err(_elapsed) => {
            guard.kill(libc::SIGTERM);
            if tokio::time::timeout(spec.kill_grace, child.wait())
                .await
                .is_err()
            {
                let _ = child.kill().await;
            }
            ScriptEnd::TimedOut
        }
    };
    // Whatever the script left running ends with it: its group, and with a
    // script user every process of that user.
    guard.finish();
    writer.abort();
    for mut reader in [stdout_reader, stderr_reader].into_iter().flatten() {
        if tokio::time::timeout(DRAIN_TIMEOUT, &mut reader)
            .await
            .is_err()
        {
            tracing::warn!("observer worker: a process outside the script's group holds its output; not waiting for it");
            reader.abort();
        }
    }
    let take = |captured: &Arc<Mutex<Captured>>| {
        let mut captured = captured.lock().unwrap_or_else(|poison| poison.into_inner());
        (std::mem::take(&mut captured.kept), captured.total)
    };
    let (stdout, stdout_total) = take(&stdout);
    let (stderr, stderr_total) = take(&stderr);
    Ok(ScriptExit {
        end,
        stdout,
        stdout_total,
        stderr,
        stderr_total,
    })
}

/// Reads `pipe` to its end, keeping the first `limit` bytes and counting
/// the rest.
async fn read_bounded(mut pipe: impl AsyncRead + Unpin, limit: usize, into: Arc<Mutex<Captured>>) {
    let mut buffer = [0u8; 8192];
    loop {
        let read = match pipe.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(read) => read,
        };
        let mut captured = into.lock().unwrap_or_else(|poison| poison.into_inner());
        captured.total += read as u64;
        let room = limit.saturating_sub(captured.kept.len());
        captured.kept.extend_from_slice(&buffer[..read.min(room)]);
    }
}

fn end_of(status: std::process::ExitStatus) -> ScriptEnd {
    use std::os::unix::process::ExitStatusExt as _;
    match (status.code(), status.signal()) {
        (Some(code), _) => ScriptEnd::Exited(code),
        (None, Some(signal)) => ScriptEnd::Signaled(signal),
        (None, None) => ScriptEnd::Exited(-1),
    }
}

/// `_LINUX_CAPABILITY_VERSION_3`: two 32-bit words per set.
#[cfg(target_os = "linux")]
const CAPABILITY_VERSION_3: u32 = 0x2008_0522;

/// `struct __user_cap_header_struct`.
#[cfg(target_os = "linux")]
#[repr(C)]
struct CapabilityHeader {
    version: u32,
    pid: libc::c_int,
}

/// `struct __user_cap_data_struct`.
#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CapabilityData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// Clears the effective, permitted and inheritable capability sets of the
/// calling thread and checks with `capget` that they are empty. `Err` when
/// a set is not empty: the caller must then not use a privilege it meant
/// to give up. Runs between `fork` and `exec`, so it allocates nothing.
#[cfg(target_os = "linux")]
fn drop_all_capabilities() -> io::Result<()> {
    let mut header = CapabilityHeader {
        version: CAPABILITY_VERSION_3,
        pid: 0,
    };
    let empty = [CapabilityData::default(); 2];
    // SAFETY: `capset` reads a version-3 header and two data structs, both
    // valid for the duration of the call.
    if unsafe { libc::syscall(libc::SYS_capset, &mut header, empty.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut left = [CapabilityData::default(); 2];
    // SAFETY: `capget` writes two data structs into `left`, which holds two.
    if unsafe { libc::syscall(libc::SYS_capget, &mut header, left.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let held = left
        .iter()
        .any(|set| set.effective | set.permitted | set.inheritable != 0);
    if held {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    Ok(())
}

/// Runs in the child between `fork` and `exec`, after the switch to the
/// script's user: no capability the worker holds survives `exec`, and no
/// set-user-id program can give the script new ones.
#[cfg(target_os = "linux")]
fn drop_inherited_privileges() -> io::Result<()> {
    // SAFETY: `prctl` with integer arguments only.
    unsafe {
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(io::Error::last_os_error());
        }
        // Fails only on kernels without ambient capabilities: then there is
        // nothing to clear.
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL,
            0,
            0,
            0,
        );
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// The child clears its capability sets, and `capget` confirms it, or
    /// the spawn fails. Run inside `unshare -r` too: there the child starts
    /// with every capability and the check is not trivial.
    #[test]
    fn a_child_can_drop_every_capability_before_exec() {
        let mut command = std::process::Command::new(SHELL);
        command.args(["-c", ":"]);
        // SAFETY: the hook only calls `capset` and `capget`.
        unsafe {
            command.pre_exec(drop_all_capabilities);
        }
        assert!(command.status().unwrap().success());
    }

    /// A scratch directory, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "narsor-write-script-test-{}",
                uuid::Uuid::new_v4().simple()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The test's own uid with a group that is not its primary group when
    /// it has one: a change of group is then visible without `CAP_CHOWN`
    /// (a process may give its files to any of its groups).
    fn script_user() -> ScriptUser {
        // SAFETY: plain getters.
        let (uid, primary) = unsafe { (libc::getuid(), libc::getgid()) };
        let mut groups = vec![0 as libc::gid_t; 256];
        // SAFETY: the buffer holds `groups.len()` entries.
        let count = unsafe { libc::getgroups(groups.len() as libc::c_int, groups.as_mut_ptr()) };
        let other = groups[..count.max(0) as usize]
            .iter()
            .copied()
            .find(|gid| *gid != primary);
        ScriptUser {
            uid,
            gid: other.unwrap_or(primary),
        }
    }

    fn group_of(path: &Path) -> u32 {
        use std::os::unix::fs::MetadataExt as _;
        fs::symlink_metadata(path).unwrap().gid()
    }

    #[test]
    fn the_script_and_the_run_dir_go_to_the_script_user() {
        use std::os::unix::fs::MetadataExt as _;
        let scratch = Scratch::new();
        let user = script_user();
        write_script(&scratch.0, "echo hi\n", Some(user)).unwrap();
        let script = scratch.0.join(SCRIPT_FILE);
        assert_eq!(fs::read_to_string(&script).unwrap(), "echo hi\n");
        let meta = fs::metadata(&script).unwrap();
        assert_eq!(meta.mode() & 0o777, 0o400);
        assert_eq!((meta.uid(), meta.gid()), (user.uid, user.gid));
        let dir = fs::metadata(&scratch.0).unwrap();
        assert_eq!((dir.uid(), dir.gid()), (user.uid, user.gid));
    }

    /// Another process of the worker's uid puts a symlink to a host file in
    /// place of the script: the worker refuses, and the file keeps its
    /// content and its owner.
    #[test]
    fn a_symlink_in_place_of_the_script_does_not_give_its_target_away() {
        let scratch = Scratch::new();
        let target = scratch.0.join("host-file");
        fs::write(&target, "host").unwrap();
        let run_dir = scratch.0.join("run");
        fs::create_dir(&run_dir).unwrap();
        std::os::unix::fs::symlink(&target, run_dir.join(SCRIPT_FILE)).unwrap();
        let group = group_of(&target);

        let user = script_user();
        assert!(write_script(&run_dir, "echo hi\n", Some(user)).is_err());

        assert_eq!(fs::read_to_string(&target).unwrap(), "host");
        assert_eq!(group_of(&target), group);
    }

    /// The same with a symlink in place of the run directory.
    #[test]
    fn a_symlink_in_place_of_the_run_dir_does_not_give_its_target_away() {
        let scratch = Scratch::new();
        let target = scratch.0.join("host-dir");
        fs::create_dir(&target).unwrap();
        let run_dir = scratch.0.join("run");
        std::os::unix::fs::symlink(&target, &run_dir).unwrap();
        let group = group_of(&target);

        let user = script_user();
        let error = write_script(&run_dir, "echo hi\n", Some(user)).unwrap_err();

        // ENOTDIR or ELOOP, as the kernel chooses for O_DIRECTORY | O_NOFOLLOW.
        assert!(
            matches!(error.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP)),
            "{error}"
        );
        assert!(!target.join(SCRIPT_FILE).exists());
        assert_eq!(group_of(&target), group);
    }
}
