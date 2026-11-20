//! The daemon's observer worker: `narsor-observer-worker` as a child
//! process the daemon starts and supervises when its configuration enables
//! it (`server.observer.enabled: true`; D5, PO5, U6).
//!
//! # The key
//!
//! Before every start of the child the daemon makes a new random key
//! ([`ProtocolService::issue_observer_worker_key`]) and keeps only its
//! SHA-256 digest, in memory. The child gets the key in
//! [`OBSERVER_WORKER_KEY_ENV`] — never as an argument, which `ps` shows. A
//! new start, or a new daemon, makes every earlier key worthless. Nobody
//! else makes or holds such a key, and it opens the worker routes only:
//! claim a run of any (queue, automation) pair, report its result.
//!
//! # What the child gets
//!
//! `--server` (the configured `server.observer.server_url`, or the daemon's own
//! address), the secrets directory and the script user or the waiver, each
//! as a flag. None of the daemon's `NARSOR_*` variables reach it; the key
//! is the one it gets. `RUST_LOG` follows the daemon's log level; the
//! child's stderr is copied, prefixed, to the daemon's log. With a script
//! user, the child gets exactly [`capabilities::WORKER_CAPABILITIES`] as
//! ambient capabilities, when the daemon holds them; every other child of
//! the daemon gets none ([`crate::capabilities`]).
//!
//! # Supervision
//!
//! Like the local controller's ([`crate::child_process`]): one thread
//! starts every incarnation; a child that ends is started again after a
//! pause, doubled on each further failure up to
//! [`child_process::MAX_RESTART_DELAY`]; the daemon's stop ends the child
//! with `SIGTERM`, then `SIGKILL`. There is no readiness to wait for: a
//! worker that cannot reach the control plane only claims nothing, and
//! `observer list` then warns that no worker claims due runs.

use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::time::Instant;

use narsor_controller_runtime::Supervision;
use narsor_engine::config::{Resolved, SettingKey};
use narsor_engine::controller_protocol::credentials::BearerToken;
use narsor_engine::controller_protocol::wire::OBSERVER_WORKER_KEY_ENV;
#[cfg(doc)]
use narsor_engine::controller_protocol::ProtocolService;

use crate::capabilities;
use crate::child_process::{self, Ended, RestartDelay};

/// The program the worker is, installed next to `narsor-server`.
const WORKER_PROGRAM: &str = "narsor-observer-worker";

/// Prefix of the child's lines in the daemon's log.
const LOG_PREFIX: &str = "narsor-observer-worker:";

/// Everything the daemon resolved for its observer worker.
#[derive(Debug, Clone)]
pub struct ObserverWorkerSettings {
    /// The `narsor-observer-worker` executable.
    pub program: PathBuf,
    /// The API's URL for the worker and its scripts.
    pub server: String,
    /// The secrets directory: one file per secret name, flat; a run gets
    /// only the secrets its automation declares.
    pub secrets_dir: Option<PathBuf>,
    /// How scripts are isolated from the worker.
    pub isolation: ScriptIsolation,
    /// The `tracing` filter of the daemon, handed on as `RUST_LOG`.
    pub log_directive: Option<String>,
    /// The pauses before a restart and the time a stopping child gets.
    pub supervision: Supervision,
}

/// Which user scripts run as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptIsolation {
    /// This system user, not the daemon's (`--script-user`).
    User(String),
    /// The daemon's own user: a trial only (`--insecure-same-user`).
    SameUser,
}

/// The observer worker as the configuration asks for it, checked before
/// anything starts; what needs the running API (its address, when no URL
/// is configured) is added at start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObserverPlan {
    /// The configured URL of the API, `None` for the daemon's own address.
    pub server: Option<String>,
    /// The secrets directory, `None` for none.
    pub secrets_dir: Option<PathBuf>,
    /// How scripts are isolated from the worker.
    pub isolation: ScriptIsolation,
}

/// The observer worker the configuration enables (`server.observer.*`),
/// `None` when it enables none, or every reason it could not run.
///
/// `network` is the server's mode: a network server's own address is not
/// one the worker may use, its requests must come through the TLS proxy.
pub fn observer_plan(
    config: &Resolved,
    network: bool,
) -> Result<Option<ObserverPlan>, Vec<String>> {
    let Some(worker) = config.observer_worker() else {
        return Ok(None);
    };
    let enabled = SettingKey::ObserverEnabled.key();
    let script_user = SettingKey::ObserverScriptUser.key();
    let same_user = SettingKey::ObserverInsecureSameUser.key();
    let server_url = SettingKey::ObserverServerUrl.key();
    let mut problems = Vec::new();
    let isolation = match (worker.script_user, worker.insecure_same_user) {
        (Some(user), false) => Some(ScriptIsolation::User(user)),
        (None, true) => Some(ScriptIsolation::SameUser),
        (None, false) => {
            problems.push(format!(
                "{enabled}: true needs {script_user}, the system user scripts run as (or \
                 {same_user}: true, for a trial only)"
            ));
            None
        }
        (Some(_), true) => {
            problems.push(format!(
                "{script_user} and {same_user}: true contradict each other; set one of them"
            ));
            None
        }
    };
    if network && worker.server.is_none() {
        problems.push(format!(
            "{enabled}: true in network mode needs {server_url}: the URL clients reach the \
             API at, through the TLS proxy"
        ));
    }
    match isolation {
        Some(isolation) if problems.is_empty() => Ok(Some(ObserverPlan {
            server: worker.server,
            secrets_dir: worker.secrets_dir,
            isolation,
        })),
        _ => Err(problems),
    }
}

impl ObserverWorkerSettings {
    /// The child's command line and environment, with `key`. Only the key
    /// is a variable of the child's own; the rest are flags.
    pub fn command(&self, key: &str) -> Command {
        let mut command = Command::new(&self.program);
        command.arg("--server").arg(&self.server);
        if let Some(dir) = &self.secrets_dir {
            command.arg("--secrets-dir").arg(dir);
        }
        match &self.isolation {
            ScriptIsolation::User(user) => command.arg("--script-user").arg(user),
            ScriptIsolation::SameUser => command.arg("--insecure-same-user"),
        };
        child_process::prepare(&mut command, self.log_directive.as_deref(), &[]);
        if matches!(self.isolation, ScriptIsolation::User(_)) {
            // SAFETY: the hook only calls `prctl`. It runs after the hook
            // of `prepare`, which cleared the ambient set.
            unsafe {
                command.pre_exec(capabilities::raise_worker_ambient);
            }
        }
        command.env(OBSERVER_WORKER_KEY_ENV, key);
        command
    }
}

/// The `narsor-observer-worker` next to this executable (KTD9), see
/// [`child_process::sibling_program`].
pub fn worker_program() -> Result<PathBuf, String> {
    child_process::sibling_program(WORKER_PROGRAM)
}

/// The running observer worker, supervised until [`Self::stop`].
#[derive(Debug)]
pub struct ObserverWorker {
    stop: mpsc::Sender<()>,
    thread: std::thread::JoinHandle<()>,
}

impl ObserverWorker {
    /// Starts the supervising thread, which starts the child at once with
    /// a key from `issue_key` — one new key for every start
    /// ([`ProtocolService::issue_observer_worker_key`]).
    pub fn start(
        settings: ObserverWorkerSettings,
        issue_key: impl Fn() -> BearerToken + Send + 'static,
    ) -> Result<Self, String> {
        let (stop, stopped) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("observer-worker".into())
            .spawn(move || supervise(&settings, &issue_key, &stopped))
            .map_err(|error| format!("cannot start the observer worker's supervisor: {error}"))?;
        tracing::info!("daemon: the observer worker is started");
        Ok(Self { stop, thread })
    }

    /// Stops the child — `SIGTERM`, a bounded wait, then `SIGKILL` — and
    /// the supervision with it.
    pub async fn stop(self) {
        let _ = self.stop.send(());
        let thread = self.thread;
        if let Err(error) = tokio::task::spawn_blocking(move || thread.join()).await {
            tracing::warn!(%error, "daemon: could not wait for the observer worker to stop");
        }
    }
}

/// The supervising thread: starts the child with a new key, starts it
/// again when it ends, stops it on request.
fn supervise(
    settings: &ObserverWorkerSettings,
    issue_key: &dyn Fn() -> BearerToken,
    stopped: &mpsc::Receiver<()>,
) {
    let mut delays = RestartDelay::new(settings.supervision.restart_after);
    loop {
        let started = Instant::now();
        // The key of the incarnation before stops working here.
        let key = issue_key();
        let outcome = match settings.command(key.expose()).spawn() {
            Ok(mut child) => match child_process::copy_stderr(
                &mut child,
                "observer-worker-log",
                LOG_PREFIX,
                |_| {},
            ) {
                Ok(stderr) => {
                    let outcome = child_process::watch(
                        &mut child,
                        stopped,
                        settings.supervision.stop_timeout,
                        "the observer worker",
                    );
                    let _ = stderr.join();
                    outcome
                }
                Err(error) => Ended::Exited(Err(format!("cannot read its stderr: {error}"))),
            },
            Err(error) => Ended::Exited(Err(format!(
                "cannot start {}: {error}",
                settings.program.display()
            ))),
        };
        drop(key);
        let reason = match outcome {
            Ended::Stopped => return,
            Ended::Exited(Ok(status)) => format!("narsor-observer-worker exited ({status})"),
            Ended::Exited(Err(reason)) => reason,
        };
        let delay = delays.after(started.elapsed());
        tracing::warn!(
            reason = %reason,
            restart_in_secs = delay.as_secs_f64(),
            "daemon: the observer worker ended; starting it again"
        );
        if child_process::stop_requested_within(stopped, delay) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::Arc;
    use std::time::Duration;

    use narsor_engine::controller_protocol::credentials::ObserverWorkerKey;

    use super::*;

    /// How long a test waits for something the supervision does.
    const TEST_TIMEOUT: Duration = Duration::from_secs(10);

    fn settings(program: PathBuf) -> ObserverWorkerSettings {
        ObserverWorkerSettings {
            program,
            server: "http://127.0.0.1:4100".to_string(),
            secrets_dir: Some(PathBuf::from("/etc/narsor/observer-secrets")),
            isolation: ScriptIsolation::User("narsor-observer-script".to_string()),
            log_directive: Some("info".to_string()),
            supervision: Supervision {
                restart_after: Duration::from_millis(50),
                stop_timeout: Duration::from_secs(2),
            },
        }
    }

    #[test]
    fn the_key_is_in_the_environment_and_never_an_argument() {
        let command = settings(PathBuf::from("/opt/narsor/narsor-observer-worker"))
            .command("nsw.server.s3cr3t");

        let args: Vec<&OsStr> = command.get_args().collect();
        assert_eq!(
            args,
            [
                "--server",
                "http://127.0.0.1:4100",
                "--secrets-dir",
                "/etc/narsor/observer-secrets",
                "--script-user",
                "narsor-observer-script",
            ]
        );
        let set: Vec<(&OsStr, Option<&OsStr>)> = command.get_envs().collect();
        assert!(set.contains(&(
            OsStr::new(OBSERVER_WORKER_KEY_ENV),
            Some(OsStr::new("nsw.server.s3cr3t"))
        )));
        assert!(set.contains(&(OsStr::new("RUST_LOG"), Some(OsStr::new("info")))));
        // Nothing else of the daemon's settings goes in as a variable.
        for (name, value) in set {
            if name.to_string_lossy().starts_with("NARSOR_") {
                assert!(
                    name == OBSERVER_WORKER_KEY_ENV || value.is_none(),
                    "{name:?} is handed on"
                );
            }
        }
    }

    #[test]
    fn the_waiver_replaces_the_script_user() {
        let mut settings = settings(PathBuf::from("narsor-observer-worker"));
        settings.isolation = ScriptIsolation::SameUser;
        settings.secrets_dir = None;

        let command = settings.command("key");

        let args: Vec<&OsStr> = command.get_args().collect();
        assert_eq!(
            args,
            ["--server", "http://127.0.0.1:4100", "--insecure-same-user"]
        );
    }

    /// A stand-in for `narsor-observer-worker` that records its key and arguments, then
    /// exits at once.
    fn recording_program(dir: &std::path::Path) -> PathBuf {
        let program = dir.join("fake-narsor-observer-worker");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\necho \"${OBSERVER_WORKER_KEY_ENV} $*\" >> {}\n",
                dir.join("starts").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        program
    }

    #[tokio::test]
    async fn a_worker_that_ends_is_started_again_with_a_new_key() {
        let dir =
            std::env::temp_dir().join(format!("narsor-observer-worker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let keys = Arc::new(ObserverWorkerKey::default());
        let worker = ObserverWorker::start(settings(recording_program(&dir)), {
            let keys = Arc::clone(&keys);
            move || keys.issue()
        })
        .unwrap();

        let deadline = Instant::now() + TEST_TIMEOUT;
        let starts = loop {
            let starts = std::fs::read_to_string(dir.join("starts")).unwrap_or_default();
            if starts.lines().count() >= 3 || Instant::now() > deadline {
                break starts;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        worker.stop().await;

        let given: Vec<&str> = starts
            .lines()
            .map(|line| line.split_once(' ').unwrap().0)
            .collect();
        assert!(given.len() >= 3, "{starts}");
        for line in starts.lines() {
            assert!(line.contains(" --server "), "{line}");
        }
        assert!(
            given.iter().all(|key| key.starts_with("nsw.server.")),
            "{starts}"
        );
        assert_ne!(given[0], given[1], "each start has its own key");
        // A later start ended every key before the last one.
        for key in &given[..given.len() - 1] {
            assert!(!keys.matches(key));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
