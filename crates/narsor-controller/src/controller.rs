//! `narsor-controller`: the environment controller of one backend, run next
//! to it (R3-R5, R37; KTD1, KTD6).
//!
//! ```text
//! narsor-controller --control-plane <url> --credential-file <file> \
//!                   [--enrollment-code <code>] --kubernetes-config <file> \
//!                   [--namespace <ns>] [--api-server <url> --token-file <file> --ca-file <file>]
//!
//! narsor-controller edit-claude-profile [--claude-command <program>] [--yes] …
//! ```
//!
//! # Editing the Claude profile
//!
//! `edit-claude-profile` is the one way Narsor writes the Claude profile
//! Secret (R15, KTD7). Run it in the controller's Pod with a terminal —
//! `kubectl exec -it deploy/narsor-controller -- narsor-controller
//! edit-claude-profile` — and it lays the Secret out in a private session
//! directory, runs Claude Code on it, and, once Claude Code is closed,
//! shows what changed and writes the changed keys back; see
//! [`crate::profile_edit`]. The harness itself never gets the controller's
//! Kubernetes access.
//!
//! Every flag can also come from the environment (`NARSOR_…`, see `--help`),
//! which is how the Deployment in `deploy/kubernetes/` configures it.
//!
//! # Identity
//!
//! The controller's identity is the `nsk.…` credential in
//! `--credential-file`: its id and an Ed25519 key it generated itself
//! ([`narsor_engine::auth::pop`]). On the first start the file does not exist yet;
//! the controller then redeems the one-time `--enrollment-code`, binding a
//! freshly generated key, and persists the credential `0600`. Every later
//! start loads it and ignores a code still lying around in the environment,
//! so a restarted Pod does not try to re-enroll with a spent code.
//!
//! # Cluster access
//!
//! In-cluster by default (service-account token and CA bundle, the Pod's
//! namespace); `--api-server` with `--token-file`/`--ca-file` reaches a
//! cluster from outside, without any kubeconfig parsing.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, ValueEnum};

use crate::kubernetes::client::{ClusterAccess, TokenSource};
use crate::kubernetes::{KubernetesBackend, KubernetesBackendConfig};
use crate::profile_edit::{EditSession, ProfileChanges};
use narsor_controller_runtime::{Controller, ControllerConfig};
use narsor_engine::config::CLAUDE_CONFIG_DIR_ENV;
use narsor_engine::controller_protocol::ControllerClient;
use narsor_engine::controller_runtime::load_or_enroll as load_controller_identity;

const EXIT_SUCCESS: i32 = 0;
const EXIT_FAILURE: i32 = 1;
const EXIT_USAGE: i32 = 2;

/// The variables of the controller's environment a profile edit session's
/// harness inherits: what a terminal program needs, nothing else.
const HARNESS_ENVS: [&str; 7] = [
    "PATH",
    "TERM",
    "COLORTERM",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
];

/// Platforms `narsor-controller` can drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ControllerBackend {
    /// One Pod and one PVC per task.
    Kubernetes,
}

/// Arguments of `narsor-controller`.
#[derive(Debug, Clone, Args)]
pub struct ControllerArgs {
    /// The control plane's base URL, as the controller reaches it.
    #[arg(long, env = "NARSOR_CONTROL_PLANE", value_name = "URL")]
    pub control_plane: String,

    /// The control plane's base URL as runners reach it from their Pods;
    /// defaults to `--control-plane`.
    #[arg(long, env = "NARSOR_RUNNER_CONTROL_PLANE", value_name = "URL")]
    pub runner_control_plane: Option<String>,

    /// Where the controller credential is kept (written on enrollment).
    #[arg(long, env = "NARSOR_CONTROLLER_CREDENTIAL_FILE", value_name = "FILE")]
    pub credential_file: PathBuf,

    /// One-time enrollment code, redeemed when the credential file does
    /// not exist yet.
    #[arg(
        long,
        env = "NARSOR_ENROLLMENT_CODE",
        hide_env_values = true,
        value_name = "CODE"
    )]
    pub enrollment_code: Option<String>,

    /// The platform.
    #[arg(long, value_enum, default_value = "kubernetes")]
    pub backend: ControllerBackend,

    /// The Kubernetes backend configuration (YAML).
    #[arg(long, env = "NARSOR_KUBERNETES_CONFIG", value_name = "FILE")]
    pub kubernetes_config: PathBuf,

    /// How the cluster is reached.
    #[command(flatten)]
    pub cluster: ClusterArgs,

    /// Validity of the backend lease, in seconds.
    #[arg(long, default_value_t = 60, value_name = "SECONDS")]
    pub lease_ttl_secs: u64,
}

/// How a `narsor-controller` command reaches the cluster.
#[derive(Debug, Clone, Args)]
pub struct ClusterArgs {
    /// Namespace of the task objects; overrides the configuration and the
    /// in-cluster default.
    #[arg(long, env = "NARSOR_KUBERNETES_NAMESPACE", value_name = "NAMESPACE")]
    pub namespace: Option<String>,

    /// API server URL, to run outside the cluster.
    #[arg(long, env = "NARSOR_KUBERNETES_API_SERVER", value_name = "URL")]
    pub api_server: Option<String>,

    /// Bearer token file for `--api-server` (re-read on every request).
    #[arg(long, env = "NARSOR_KUBERNETES_TOKEN_FILE", value_name = "FILE")]
    pub token_file: Option<PathBuf>,

    /// CA bundle (PEM) for `--api-server`.
    #[arg(long, env = "NARSOR_KUBERNETES_CA_FILE", value_name = "FILE")]
    pub ca_file: Option<PathBuf>,
}

/// Arguments of `narsor-controller edit-claude-profile`.
#[derive(Debug, Clone, Args)]
pub struct EditProfileArgs {
    /// The Kubernetes backend configuration (YAML), naming the profile
    /// Secret and its `key=path` mappings.
    #[arg(long, env = "NARSOR_KUBERNETES_CONFIG", value_name = "FILE")]
    pub kubernetes_config: PathBuf,

    /// How the cluster is reached.
    #[command(flatten)]
    pub cluster: ClusterArgs,

    /// The Claude Code program to run.
    #[arg(
        long,
        env = "NARSOR_CLAUDE_COMMAND",
        default_value = "claude",
        value_name = "PROGRAM"
    )]
    pub claude_command: PathBuf,

    /// Where the session directory is created; the system's temporary
    /// directory by default.
    #[arg(long, env = "NARSOR_PROFILE_EDIT_DIR", value_name = "DIRECTORY")]
    pub session_parent: Option<PathBuf>,

    /// Save the changes without asking.
    #[arg(long)]
    pub yes: bool,
}

/// A failure of the controller command, with its exit code.
#[derive(Debug)]
pub struct ControllerError {
    /// What went wrong.
    pub message: String,
    /// The exit code.
    pub code: i32,
}

impl ControllerError {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: EXIT_USAGE,
        }
    }

    fn failure(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: EXIT_FAILURE,
        }
    }
}

/// Runs the controller until `SIGTERM` or Ctrl-C.
pub async fn run_command(args: ControllerArgs) -> i32 {
    match run(args).await {
        Ok(()) => EXIT_SUCCESS,
        Err(error) => {
            eprintln!("{}", error.message);
            error.code
        }
    }
}

async fn run(args: ControllerArgs) -> Result<(), ControllerError> {
    let ControllerBackend::Kubernetes = args.backend;
    let mut config = read_backend_config(&args.kubernetes_config)?;
    if args.cluster.namespace.is_some() {
        config.namespace = args.cluster.namespace.clone();
    }
    let access = cluster_access(&args.cluster, config.namespace.clone())?;
    let backend = KubernetesBackend::new(&access, config).map_err(ControllerError::usage)?;
    backend.preflight().await.map_err(|error| {
        ControllerError::failure(format!("the Kubernetes backend is not usable: {error}"))
    })?;

    let client = load_or_enroll(
        &args.control_plane,
        &args.credential_file,
        args.enrollment_code.as_deref(),
    )
    .await?;
    let runner_url = args
        .runner_control_plane
        .clone()
        .unwrap_or_else(|| args.control_plane.clone());
    let mut controller_config = ControllerConfig::new(runner_url);
    controller_config.lease_ttl = Duration::from_secs(args.lease_ttl_secs.max(1));
    tracing::info!(namespace = %backend.namespace(), "controller: driving the Kubernetes backend");
    Controller::new(backend, client, controller_config)
        .run(shutdown_signal())
        .await;
    Ok(())
}

/// Runs one Claude profile edit session.
pub async fn edit_profile_command(args: EditProfileArgs) -> i32 {
    match edit_profile(args).await {
        Ok(()) => EXIT_SUCCESS,
        Err(error) => {
            eprintln!("{}", error.message);
            error.code
        }
    }
}

/// One edit session, its failure unprinted.
pub async fn edit_profile(args: EditProfileArgs) -> Result<(), ControllerError> {
    let mut config = read_backend_config(&args.kubernetes_config)?;
    if args.cluster.namespace.is_some() {
        config.namespace = args.cluster.namespace.clone();
    }
    let Some(secret) = config.claude_profile_secret.clone() else {
        return Err(ControllerError::usage(format!(
            "{} configures no claude_profile_secret",
            args.kubernetes_config.display()
        )));
    };
    let access = cluster_access(&args.cluster, config.namespace.clone())?;
    let backend = KubernetesBackend::new(&access, config).map_err(ControllerError::usage)?;
    let current = backend
        .read_claude_profile()
        .await
        .map_err(|error| ControllerError::failure(error.to_string()))?;
    let parent = args
        .session_parent
        .clone()
        .unwrap_or_else(std::env::temp_dir);
    let session = EditSession::open(
        &parent,
        backend.config().claude_profile_mappings(),
        &current,
    )
    .map_err(ControllerError::failure)?;

    eprintln!("Starting Claude Code on the profile of Secret '{secret}'. Close it to continue.");
    let status = run_claude(&args.claude_command, &session).await?;
    if !status.success() {
        eprintln!("Claude Code exited with {status}; its changes can still be saved.");
    }
    let changes = session
        .changes(&current)
        .map_err(ControllerError::failure)?;
    let mappings = backend.config().claude_profile_mappings();
    let executing = report_changes(&changes, &current, &mappings);
    if changes.is_empty() {
        eprintln!("Nothing to save.");
        return Ok(());
    }
    // A change that makes Claude Code run something — in every task — is
    // never saved unseen: `--yes` does not cover it.
    if args.yes && executing {
        return Err(ControllerError::usage(
            "the changes make Claude Code run programs; review and save them without --yes",
        ));
    }
    if !args.yes && !confirm(&format!("Save to Secret '{secret}'? [y/N] "))? {
        eprintln!("Not saved.");
        return Ok(());
    }
    backend
        .write_claude_profile(&changes.updated)
        .await
        .map_err(|error| ControllerError::failure(error.to_string()))?;
    eprintln!(
        "Saved {} key(s) to Secret '{secret}'; task Pods take them at their next start.",
        changes.updated.len()
    );
    Ok(())
}

/// Runs Claude Code on the session in this terminal, with only the
/// session's directories and no API key, until the user closes it.
async fn run_claude(
    program: &Path,
    session: &EditSession,
) -> Result<std::process::ExitStatus, ControllerError> {
    // Ctrl-C belongs to Claude Code: without a handler of its own, this
    // process would die with it and leave the session's login on disk.
    let _interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(|error| ControllerError::failure(format!("cannot handle Ctrl-C: {error}")))?;
    // Nothing of the controller's environment but the terminal's: no API
    // key, no NARSOR_* setting, no enrollment code.
    let mut command = std::process::Command::new(program);
    command
        .env_clear()
        .envs(
            HARNESS_ENVS
                .iter()
                .filter_map(|name| std::env::var_os(name).map(|value| (*name, value))),
        )
        .env(CLAUDE_CONFIG_DIR_ENV, session.config_dir())
        .env("HOME", session.home_dir())
        .current_dir(session.home_dir());
    tokio::task::spawn_blocking(move || command.status())
        .await
        .map_err(|error| ControllerError::failure(error.to_string()))?
        .map_err(|error| {
            ControllerError::failure(format!("cannot start {}: {error}", program.display()))
        })
}

/// Shows the user what saving would write: a diff per changed key (the
/// login's content hidden), warnings for changes that make Claude Code run
/// programs, and what cannot be saved. Returns whether there was such a
/// warning.
fn report_changes(
    changes: &ProfileChanges,
    current: &std::collections::BTreeMap<String, Vec<u8>>,
    mappings: &[(String, String)],
) -> bool {
    let reviews = changes.review(current, mappings);
    for review in &reviews {
        eprintln!("changed: {} (key {})", review.path, review.key);
        eprint!("{}", review.diff);
    }
    for review in &reviews {
        for warning in &review.warnings {
            eprintln!("WARNING: {warning}");
        }
    }
    for path in &changes.missing {
        eprintln!("removed: {path} — kept in the Secret, task Pods mount every mapped key");
    }
    for path in &changes.unmapped {
        eprintln!("not saved: {path} — add a 'key={path}' entry to claude_profile_files");
    }
    reviews.iter().any(|review| !review.warnings.is_empty())
}

fn confirm(prompt: &str) -> Result<bool, ControllerError> {
    use std::io::Write as _;
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|error| ControllerError::failure(format!("cannot read the answer: {error}")))?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

/// Reads and checks the backend configuration file.
pub fn read_backend_config(path: &Path) -> Result<KubernetesBackendConfig, ControllerError> {
    let text = fs::read_to_string(path).map_err(|error| {
        ControllerError::usage(format!("cannot read {}: {error}", path.display()))
    })?;
    let config: KubernetesBackendConfig = serde_yaml::from_str(&text).map_err(|error| {
        ControllerError::usage(format!(
            "{} is not a valid configuration: {error}",
            path.display()
        ))
    })?;
    let problems = config.problems();
    if !problems.is_empty() {
        return Err(ControllerError::usage(format!(
            "{}: {}",
            path.display(),
            problems.join("; ")
        )));
    }
    Ok(config)
}

fn cluster_access(
    args: &ClusterArgs,
    namespace: Option<String>,
) -> Result<ClusterAccess, ControllerError> {
    let Some(api_server) = &args.api_server else {
        return ClusterAccess::in_cluster(namespace).map_err(ControllerError::usage);
    };
    let namespace = namespace
        .ok_or_else(|| ControllerError::usage("--api-server needs a namespace (--namespace)"))?;
    let mut access = ClusterAccess::explicit(api_server, namespace);
    if let Some(token) = &args.token_file {
        access.token = TokenSource::File(token.clone());
    }
    if let Some(ca) = &args.ca_file {
        access.ca_pem = Some(fs::read(ca).map_err(|error| {
            ControllerError::usage(format!("cannot read {}: {error}", ca.display()))
        })?);
    }
    Ok(access)
}

/// The controller's identity: the persisted credential when there is one,
/// else a fresh enrollment with `code`, persisted `0600` before first use.
pub async fn load_or_enroll(
    control_plane: &str,
    credential_file: &Path,
    code: Option<&str>,
) -> Result<ControllerClient, ControllerError> {
    load_controller_identity(control_plane, credential_file, code)
        .await
        .map_err(|error| {
            let message = error.message();
            if error.is_usage_error() {
                ControllerError::usage(message)
            } else {
                ControllerError::failure(message)
            }
        })
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    match signal(SignalKind::terminate()) {
        Ok(mut terminate) => {
            tokio::select! {
                _ = terminate.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        }
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}
