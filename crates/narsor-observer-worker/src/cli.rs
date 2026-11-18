//! The flags of `narsor-observer-worker` and its start.
//!
//! The worker talks to the control plane with the key the server hands it
//! in [`OBSERVER_WORKER_KEY_ENV`] when it starts the worker, never with an
//! owner's token (D5, PO5): the key claims runs and reports their results,
//! nothing else. Nobody else makes or holds such a key. The worker reads no
//! configuration file: its flags and the key are all it needs.

use std::path::PathBuf;

use clap::Parser;
use narsor_api_client::api::{ApiClient, ClientError, TokenSource};
use narsor_protocol::controller::OBSERVER_WORKER_KEY_ENV;

use crate::worker::{
    check_secrets_dir, current_uid, ProcessRunner, ScriptUser, Worker, WorkerOptions, CLAIM_PAUSE,
};

/// The program's exit code on success.
pub const EXIT_SUCCESS: i32 = 0;
/// The exit code of a failure while running.
pub const EXIT_FAILURE: i32 = 1;
/// The exit code of a wrong call (flags, a missing key).
pub const EXIT_USAGE: i32 = 2;

/// `narsor-observer-worker`.
#[derive(Debug, Parser)]
#[command(
    name = "narsor-observer-worker",
    version = narsor_protocol::VERSION,
    about = "Claims the observer runs the control plane has due and executes their scripts \
             in a clean environment. narsor-server starts it, with --server and the key in \
             NARSOR_OBSERVER_WORKER_KEY."
)]
pub struct WorkerArgs {
    /// The control plane's URL, for the worker and its scripts.
    #[arg(long, value_name = "URL")]
    pub server: String,

    /// The directory of the automations' secrets: one file per secret name,
    /// flat. Must be accessible to its owner only.
    #[arg(long, value_name = "DIRECTORY")]
    pub secrets_dir: Option<PathBuf>,

    /// Run scripts as this system user, which must not be able to read the
    /// secrets directory or the worker's environment. Required unless
    /// `--insecure-same-user` is given: isolation is the default.
    #[arg(
        long,
        value_name = "USER",
        required_unless_present = "insecure_same_user"
    )]
    pub script_user: Option<String>,

    /// Run scripts as the worker's own user. A script can then read the
    /// worker key and every observer's secrets: for a trial on a
    /// developer machine only, never in production.
    #[arg(long, conflicts_with = "script_user")]
    pub insecure_same_user: bool,

    /// Where to make the run directories (the system's temporary directory
    /// by default).
    #[arg(long, value_name = "DIRECTORY")]
    pub work_dir: Option<PathBuf>,

    /// The `PATH` of scripts (the directory of this program, where
    /// narsor-cli is installed too, then the system's directories, by
    /// default).
    #[arg(long, value_name = "PATH")]
    pub script_path: Option<String>,

    /// Claim once, handle what was due, and exit.
    #[arg(long)]
    pub once: bool,
}

/// Why the worker did not start or stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartError {
    /// A wrong call: [`EXIT_USAGE`].
    Usage(String),
    /// A failure: [`EXIT_FAILURE`].
    Failure(String),
}

impl StartError {
    /// The exit code of the error.
    pub fn exit_code(&self) -> i32 {
        match self {
            StartError::Usage(_) => EXIT_USAGE,
            StartError::Failure(_) => EXIT_FAILURE,
        }
    }

    fn message(&self) -> &str {
        match self {
            StartError::Usage(message) | StartError::Failure(message) => message,
        }
    }
}

/// Runs the worker until its stop and returns the exit code; prints the
/// reason of a failure on stderr.
pub async fn run(args: WorkerArgs) -> i32 {
    match serve(args).await {
        Ok(()) => EXIT_SUCCESS,
        Err(error) => {
            eprintln!("{}", error.message());
            error.exit_code()
        }
    }
}

async fn serve(args: WorkerArgs) -> Result<(), StartError> {
    let key = worker_key(std::env::var(OBSERVER_WORKER_KEY_ENV).ok())?;
    let options = worker_options(&args)?;
    let client = ApiClient::remote(&args.server, TokenSource::Static(key))
        .map_err(|error| failure("Cannot reach the control plane", error))?;
    let worker = Worker::new(client, ProcessRunner, options);
    tracing::info!(server = %args.server, "observer worker: started");
    worker
        .serve(args.once, CLAIM_PAUSE, shutdown_signal())
        .await
        .map_err(|stop| StartError::Failure(stop.to_string()))
}

/// The worker's options from its flags: checks the secrets directory and
/// the script user before anything runs.
pub fn worker_options(args: &WorkerArgs) -> Result<WorkerOptions, StartError> {
    if let Some(dir) = &args.secrets_dir {
        check_secrets_dir(dir).map_err(StartError::Failure)?;
    }
    let mut options = WorkerOptions::new(args.server.clone());
    options.secrets_dir = args.secrets_dir.clone();
    if let Some(name) = &args.script_user {
        let user = ScriptUser::lookup(name).map_err(|error| {
            StartError::Failure(format!("Cannot run scripts as '{name}': {error}"))
        })?;
        if user.uid == current_uid() {
            return Err(StartError::Failure(format!(
                "Cannot run scripts as '{name}': it is the worker's own user, which holds \
                 the key and can read the secrets"
            )));
        }
        options.script_user = Some(user);
    } else if args.insecure_same_user {
        tracing::warn!(
            "observer worker: --insecure-same-user: scripts run as the worker's own user and \
             can read the worker key and every observer's secrets; use --script-user outside \
             a trial"
        );
        eprintln!(
            "Warning: scripts run as the worker's own user (--insecure-same-user) and can read \
             the worker key and the secrets. Use --script-user in production."
        );
    } else {
        // clap requires one of the two; refuse rather than run unisolated.
        return Err(StartError::Usage(
            "narsor-observer-worker needs --script-user USER (or --insecure-same-user for a \
             trial)"
                .to_string(),
        ));
    }
    if let Some(dir) = &args.work_dir {
        options.work_dir = dir.clone();
    }
    if let Some(path) = &args.script_path {
        options.path = path.clone();
    }
    Ok(options)
}

/// The key `narsor-server` handed the worker in [`OBSERVER_WORKER_KEY_ENV`]
/// (`value`). A variable, not a flag: an argument would show in `ps`. The
/// worker never passes it on: a script's environment is an explicit list.
pub fn worker_key(value: Option<String>) -> Result<String, StartError> {
    match value {
        Some(key) if !key.trim().is_empty() => Ok(key.trim().to_string()),
        _ => Err(StartError::Usage(format!(
            "narsor-observer-worker needs the key narsor-server gives it in \
             {OBSERVER_WORKER_KEY_ENV}; enable the worker in the server's configuration \
             (observer_worker: on) instead of starting it by hand"
        ))),
    }
}

fn failure(what: &str, error: ClientError) -> StartError {
    StartError::Failure(format!("{what}: {error}"))
}

/// Resolves at `SIGTERM` (the server's stop) or Ctrl-C.
async fn shutdown_signal() {
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = terminate => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: [&str; 3] = [
        "narsor-observer-worker",
        "--server",
        "http://127.0.0.1:4100",
    ];

    #[test]
    fn the_worker_needs_a_script_user_unless_isolation_is_waived() {
        // Isolation is the default: no script user and no waiver is refused.
        assert!(WorkerArgs::try_parse_from(SERVER).is_err());
        let insecure =
            WorkerArgs::try_parse_from(SERVER.iter().copied().chain(["--insecure-same-user"]));
        assert!(insecure.is_ok());
        // The waiver and a script user contradict each other.
        let both = WorkerArgs::try_parse_from(SERVER.iter().copied().chain([
            "--insecure-same-user",
            "--script-user",
            "narsor-observer-script",
        ]));
        assert!(both.is_err());
    }

    #[test]
    fn the_worker_needs_the_server_url() {
        assert!(
            WorkerArgs::try_parse_from(["narsor-observer-worker", "--insecure-same-user"]).is_err()
        );
    }

    #[test]
    fn the_key_comes_from_the_server_never_from_a_flag() {
        assert!(WorkerArgs::try_parse_from(SERVER.iter().copied().chain([
            "--insecure-same-user",
            "--credential-file",
            "/etc/narsor/credential",
        ]))
        .is_err());
        assert_eq!(
            worker_key(Some(" nsw.server.k ".to_string())),
            Ok("nsw.server.k".to_string())
        );
        for missing in [None, Some(String::new()), Some("  ".to_string())] {
            let error = worker_key(missing).unwrap_err();
            assert_eq!(error.exit_code(), EXIT_USAGE);
            assert!(error.message().contains(OBSERVER_WORKER_KEY_ENV));
        }
    }

    #[test]
    fn the_flags_become_the_worker_options() {
        let args = WorkerArgs::try_parse_from(SERVER.iter().copied().chain([
            "--insecure-same-user",
            "--work-dir",
            "/var/tmp/obs",
            "--script-path",
            "/opt/bin:/bin",
        ]))
        .unwrap();

        let options = worker_options(&args).unwrap();

        assert_eq!(options.server, "http://127.0.0.1:4100");
        assert_eq!(options.work_dir, PathBuf::from("/var/tmp/obs"));
        assert_eq!(options.path, "/opt/bin:/bin");
        assert!(options.script_user.is_none());
    }

    #[test]
    fn the_worker_refuses_a_script_user_that_does_not_exist() {
        let args = WorkerArgs::try_parse_from(
            SERVER
                .iter()
                .copied()
                .chain(["--script-user", "narsor-no-such-user-7f3a"]),
        )
        .unwrap();

        let error = worker_options(&args).unwrap_err();

        assert_eq!(error.exit_code(), EXIT_FAILURE);
        assert!(error.message().contains("narsor-no-such-user-7f3a"));
    }
}
