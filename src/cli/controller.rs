//! `narsor controller`: the environment controller of one backend, run next
//! to it (R3-R5, R37; KTD1, KTD6).
//!
//! ```text
//! narsor controller --control-plane <url> --credential-file <file> \
//!                   [--enrollment-code <code>] --kubernetes-config <file> \
//!                   [--namespace <ns>] [--api-server <url> --token-file <file> --ca-file <file>]
//! ```
//!
//! Every flag can also come from the environment (`NARSOR_…`, see `--help`),
//! which is how the Deployment in `deploy/kubernetes/` configures it.
//!
//! # Identity
//!
//! The controller's identity is the `nsk.…` credential in
//! `--credential-file`: its id and an Ed25519 key it generated itself
//! ([`crate::auth::pop`]). On the first start the file does not exist yet;
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
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, ValueEnum};

use crate::auth::ControllerCredential;
use crate::backends::kubernetes::client::{ClusterAccess, TokenSource};
use crate::backends::{Controller, ControllerConfig, KubernetesBackend, KubernetesBackendConfig};
use crate::controller_protocol::ControllerClient;

use super::{EXIT_FAILURE, EXIT_SUCCESS, EXIT_USAGE};

/// Platforms `narsor controller` can drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ControllerBackend {
    /// One Pod and one PVC per task.
    Kubernetes,
}

/// Arguments of `narsor controller`.
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

    /// Validity of the backend lease, in seconds.
    #[arg(long, default_value_t = 60, value_name = "SECONDS")]
    pub lease_ttl_secs: u64,
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
    if args.namespace.is_some() {
        config.namespace = args.namespace.clone();
    }
    let access = cluster_access(&args, config.namespace.clone())?;
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
    args: &ControllerArgs,
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
    match fs::read_to_string(credential_file) {
        Ok(text) => {
            if ControllerCredential::parse(&text).is_none() {
                return Err(ControllerError::usage(format!(
                    "{} does not hold a controller credential",
                    credential_file.display()
                )));
            }
            if code.is_some() {
                tracing::info!("controller: credential found; the enrollment code is not used");
            }
            Ok(ControllerClient::new(control_plane, text.trim()))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let code = code.filter(|code| !code.is_empty()).ok_or_else(|| {
                ControllerError::usage(format!(
                    "{} does not exist and no enrollment code was given (--enrollment-code)",
                    credential_file.display()
                ))
            })?;
            let (client, enrolled) = ControllerClient::enroll(control_plane, code)
                .await
                .map_err(|error| ControllerError::failure(format!("enrollment failed: {error}")))?;
            let credential = client
                .credential()
                .ok_or_else(|| ControllerError::failure("enrollment returned no credential"))?;
            persist_credential(credential_file, &credential).map_err(|error| {
                ControllerError::failure(format!(
                    "cannot write {}: {error}",
                    credential_file.display()
                ))
            })?;
            tracing::info!(controller_id = %enrolled.controller_id, backend = %enrolled.backend_id, "controller: enrolled");
            Ok(client)
        }
        Err(error) => Err(ControllerError::failure(format!(
            "cannot read {}: {error}",
            credential_file.display()
        ))),
    }
}

/// Writes the credential readable by its owner only, never over an
/// existing file, and durably: losing it after enrollment would spend the
/// one-time code for nothing.
fn persist_credential(path: &Path, credential: &str) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(credential.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()
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
