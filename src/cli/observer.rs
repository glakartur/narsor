//! `narsor-cli observer …`: observers of external sources.
//!
//! | command | who | what |
//! | --- | --- | --- |
//! | `observer worker` | the worker's system user | claims due runs and executes their scripts ([`super::observer_worker`]) |
//! | `observer worker-token create\|list\|revoke` | the owner | manages the worker's credential |
//!
//! The worker talks to the control plane with its own credential, never
//! the owner's token (PO5): the credential claims runs and reports their
//! results, nothing else. It is shown once, at `create`; with `--output`
//! it goes straight into a new file of mode `0600`.

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use clap::Subcommand;

use super::commands::{CliError, Result};
use super::observer_worker::{
    check_credential_file, check_secrets_dir, current_uid, ProcessRunner, ScriptUser, Worker,
    WorkerOptions, WorkerStop, POLL_INTERVAL,
};
use super::remote::failure;
use super::task_text::age;
use crate::api::{ApiClient, ServerTarget, TokenSource};
use crate::config::Flags;
use crate::controller_protocol::CreateObserverWorkerRequest;
use crate::domain::without_unsafe_chars;

/// What `narsor-cli observer` can be asked to do.
#[derive(Debug, Subcommand)]
pub enum ObserverCommand {
    /// Runs the observer worker: claims the runs the control plane has due
    /// and executes their scripts in a clean environment. Needs --server.
    Worker(WorkerArgs),

    /// Creates, lists and revokes the credential of observer workers.
    WorkerToken {
        /// What to do.
        #[command(subcommand)]
        command: WorkerTokenCommand,
    },
}

impl ObserverCommand {
    /// Whether the command runs on a configuration file that does not hold
    /// up: the worker reads none, only its flags.
    pub fn runs_on_broken_config(&self) -> bool {
        matches!(self, ObserverCommand::Worker(_))
    }
}

/// `narsor-cli observer worker`.
#[derive(Debug, clap::Args)]
pub struct WorkerArgs {
    /// The file holding the worker credential (`observer worker-token
    /// create --output`). Must be readable by its owner only.
    #[arg(long, value_name = "FILE")]
    pub credential_file: PathBuf,

    /// The directory of the observers' secrets: one file per secret
    /// reference name. Must be accessible to its owner only.
    #[arg(long, value_name = "DIRECTORY")]
    pub secrets_dir: Option<PathBuf>,

    /// Run scripts as this system user, which must not be able to read the
    /// credential file or the secrets directory.
    #[arg(long, value_name = "USER")]
    pub script_user: Option<String>,

    /// Where to make the run directories (the system's temporary directory
    /// by default).
    #[arg(long, value_name = "DIRECTORY")]
    pub work_dir: Option<PathBuf>,

    /// The `PATH` of scripts (the directory of narsor-cli, then the
    /// system's directories, by default).
    #[arg(long, value_name = "PATH")]
    pub script_path: Option<String>,

    /// Claim once, handle what was due, and exit.
    #[arg(long)]
    pub once: bool,
}

/// `narsor-cli observer worker-token …`.
#[derive(Debug, Subcommand)]
pub enum WorkerTokenCommand {
    /// Creates a worker credential, shown once.
    Create {
        /// A label for the worker (a host name, say).
        #[arg(long, value_name = "NAME")]
        name: Option<String>,

        /// Write the credential to this new file (mode 0600) instead of
        /// printing it.
        #[arg(long, value_name = "FILE")]
        output: Option<PathBuf>,
    },

    /// Lists the worker credentials, revoked ones included.
    List,

    /// Revokes a worker credential: the worker is refused from now on.
    Revoke {
        /// The credential's id (`wkr-…`).
        #[arg(value_name = "WORKER_ID")]
        worker_id: String,
    },
}

/// Runs one `narsor-cli observer` command.
pub async fn observer(
    command: ObserverCommand,
    target: &ServerTarget,
    state: &Path,
    flags: &Flags,
) -> Result<()> {
    match command {
        ObserverCommand::Worker(args) => worker(args, target).await,
        ObserverCommand::WorkerToken { command } => {
            let client = crate::api::connect(target, state, flags)
                .await
                .map_err(CliError::failure)?;
            worker_token(command, &client).await
        }
    }
}

async fn worker_token(command: WorkerTokenCommand, client: &ApiClient) -> Result<()> {
    match command {
        WorkerTokenCommand::Create { name, output } => {
            if let Some(path) = &output {
                if path.exists() {
                    return Err(CliError::usage(format!(
                        "{} already exists; the credential goes into a new file only",
                        path.display()
                    )));
                }
            }
            let created = client
                .create_observer_worker(&CreateObserverWorkerRequest { name })
                .await
                .map_err(|error| failure("Could not create a worker credential", error))?;
            println!("Worker: {}", created.worker.worker_id);
            match output {
                Some(path) => {
                    write_new_secret_file(&path, &created.credential).map_err(|error| {
                        CliError::failure(format!(
                            "Could not write {}: {error}; revoke {} and create another",
                            path.display(),
                            created.worker.worker_id
                        ))
                    })?;
                    println!("Credential written to {} (mode 0600).", path.display());
                }
                None => {
                    println!("Credential: {}", created.credential);
                    eprintln!(
                        "Shown once. Put it in a file of mode 0600 readable by the worker's \
                         user only, and pass it with --credential-file."
                    );
                }
            }
            Ok(())
        }
        WorkerTokenCommand::List => {
            let listing = client
                .observer_workers()
                .await
                .map_err(|error| failure("Could not list the worker credentials", error))?;
            if listing.workers.is_empty() {
                println!("No worker credentials.");
            }
            let now = chrono::Utc::now();
            for worker in &listing.workers {
                let state = worker.revoked_at.map_or("active".to_string(), |at| {
                    format!("revoked {} ago", age(now, at))
                });
                println!(
                    "{:<40} {:<24} created {} ago, {state}",
                    worker.worker_id,
                    without_unsafe_chars(worker.name.as_deref().unwrap_or("-")),
                    age(now, worker.created_at)
                );
            }
            Ok(())
        }
        WorkerTokenCommand::Revoke { worker_id } => {
            let worker = client
                .revoke_observer_worker(&worker_id)
                .await
                .map_err(|error| failure("Could not revoke the worker credential", error))?;
            println!("Revoked worker credential {}.", worker.worker_id);
            Ok(())
        }
    }
}

/// Writes `secret` to a file that must not exist yet, mode `0600`.
fn write_new_secret_file(path: &Path, secret: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    writeln!(file, "{secret}")
}

async fn worker(args: WorkerArgs, target: &ServerTarget) -> Result<()> {
    let Some(server) = target.server.clone() else {
        return Err(CliError::usage(
            "narsor-cli observer worker needs the control plane's URL: --server or NARSOR_SERVER",
        ));
    };
    check_credential_file(&args.credential_file).map_err(CliError::failure)?;
    if let Some(dir) = &args.secrets_dir {
        check_secrets_dir(dir).map_err(CliError::failure)?;
    }
    let mut options = WorkerOptions::new(server.clone());
    options.secrets_dir = args.secrets_dir;
    if let Some(name) = &args.script_user {
        let user = ScriptUser::lookup(name).map_err(|error| {
            CliError::failure(format!("Cannot run scripts as '{name}': {error}"))
        })?;
        if user.uid == current_uid() {
            return Err(CliError::failure(format!(
                "Cannot run scripts as '{name}': it is the worker's own user, which can read \
                 the credential and the secrets"
            )));
        }
        options.script_user = Some(user);
    }
    if let Some(dir) = args.work_dir {
        options.work_dir = dir;
    }
    if let Some(path) = args.script_path {
        options.path = path;
    }
    let client = ApiClient::remote(&server, TokenSource::File(args.credential_file))
        .map_err(|error| failure("Cannot reach the control plane", error))?;
    let worker = Worker::new(client, ProcessRunner, options);
    tracing::info!(server = %server, "observer worker: started");
    worker
        .serve(args.once, POLL_INTERVAL, shutdown_signal())
        .await
        .map_err(|stop: WorkerStop| CliError::failure(stop.to_string()))
}

/// Resolves at Ctrl-C or `SIGTERM` (systemd's stop).
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
