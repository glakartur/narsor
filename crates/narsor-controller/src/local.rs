//! `narsor-controller local`: the controller of a `local` backend — one
//! `narsor-runner` process per task, on this machine (R5).
//!
//! ```text
//! narsor-controller local --control-plane <url> --credential-file <file> \
//!                   --storage <dir> [--home private|host] [--codex-home <dir>] …
//! ```
//!
//! The daemon of a trusted installation starts it as its child, with every
//! setting resolved and passed as a flag (R6); it is the same controller
//! loop over the same [`LocalBackend`] the daemon used to run in its own
//! process, so task storage and the controller's identity stay where they
//! were (R10).
//!
//! # One controller per credential
//!
//! Two controllers on one credential would fight over the lease and over
//! the runners. At start the controller takes an exclusive `flock` on
//! `<credential file>.lock` beside the credential file and writes its PID
//! into it — one lock per credential, so controllers whose credentials share
//! a directory do not wait for each other;
//! while another process holds it, it waits up to `--lock-timeout-secs`,
//! naming the holder. The kernel releases the lock with the process, even
//! after `SIGKILL` (R8).
//!
//! # A gone parent
//!
//! Started by the daemon with `--parent-pid`, the controller also stops when
//! its parent is gone — the second guard beside the parent-death signal the
//! daemon sets up, which a daemon thread ending could otherwise fire early
//! or never (KTD6).
//!
//! # A control plane beyond this machine
//!
//! Started by hand, the same controller serves a remote control plane
//! (R16-R18). It reaches it only over `https://` with the certificate
//! checked and follows no redirect ([`check_transport`]); it treats what the
//! server sends as untrusted ([`untrusted`]); tasks get a private HOME, no
//! harness login of the user's and a Git clone with none of the user's git
//! credentials unless the user accepts their own with `--accept-host-home`;
//! and it warns about all of this at start. Its HOME settings come from the
//! flags, then the `NARSOR_CONTROLLER__LOCAL__*` variables, then the
//! `controller` section of the Narsor configuration file.
//!
//! # Started by the daemon
//!
//! The daemon hands its child the file that won its own cascade
//! (`--config <file>`) or `--no-config` when none won, and only its
//! `NARSOR_GLOBAL__*` and `NARSOR_CONTROLLER__*` variables (R21). The child
//! reads its own section from them, the same way
//! [`narsor_engine::config::local_controller::resolve`] does for the
//! daemon's check before it starts.
//!
//! # Stopping
//!
//! On `SIGTERM` or Ctrl-C the loop releases the lease, bounded by
//! [`RELEASE_LEASE_TIMEOUT`] so that a dead control plane cannot keep the
//! process past its supervisor's stop timeout (KTD8). Runners run in their
//! own process groups and outlive it, like Pods outlive their controller.

use std::fs;
use std::io::{Read as _, Seek as _, Write as _};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Args;

use crate::controller::{
    enrollment_error, exit_code, shutdown_signal, ConnectionArgs, ControllerError,
};
use untrusted::UntrustedServer;

pub mod backend;
pub mod docker;
pub mod docker_fake;
pub mod image;
pub mod secrets;
mod untrusted;
use backend::{ContainerMode, LaunchCheck, LocalBackend, LocalBackendConfig};
use docker::{DockerCli, DockerTransport};
use narsor_controller_client::controller_runtime::load_or_enroll_with_options;
use narsor_controller_client::{ClientOptions, ControllerClient};
use narsor_controller_runtime::{Controller, ControllerConfig};
use narsor_engine::config::local_controller::{self, LocalMode};
use narsor_engine::config::{Flags, HarnessHome, HomeMode, Resolved, SettingKey, Source};

/// What the lock file's name adds to the credential file's.
const LOCK_SUFFIX: &str = ".lock";

/// Bound of the lease release on the way out: well inside the daemon's
/// stop timeout (10 s), so a release against a dead control plane cannot
/// get the controller killed (KTD8).
const RELEASE_LEASE_TIMEOUT: Duration = Duration::from_secs(5);

/// How often a held lock is tried again.
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// How often the parent named by `--parent-pid` is checked.
const PARENT_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// Arguments of `narsor-controller local`.
#[derive(Debug, Clone, Args)]
pub struct LocalArgs {
    /// How the control plane is reached.
    #[command(flatten)]
    pub connection: ConnectionArgs,

    /// Directory holding task storages and runner records.
    #[arg(long, value_name = "DIRECTORY")]
    pub storage: PathBuf,

    /// Where HOME points for the tools of a task.
    #[arg(long, value_enum, value_name = "MODE")]
    pub home: Option<HomeMode>,

    /// `codex`'s configuration directory, when not `$HOME/.codex`.
    #[arg(long, value_name = "DIRECTORY")]
    pub codex_home: Option<PathBuf>,

    /// Claude Code's configuration directory, when not `$HOME/.claude`.
    #[arg(long, value_name = "DIRECTORY")]
    pub claude_config_dir: Option<PathBuf>,

    /// The Claude Code profile the configuration directory is brought up to
    /// date with before every start.
    #[arg(long, value_name = "DIRECTORY")]
    pub claude_profile_source: Option<PathBuf>,

    /// How long to wait for another controller holding the same credential
    /// to stop.
    #[arg(long, default_value_t = 10, value_name = "SECONDS")]
    pub lock_timeout_secs: u64,

    /// Stop when this process is no longer the parent (set by the daemon).
    #[arg(long, hide = true, value_name = "PID")]
    pub parent_pid: Option<u32>,

    /// Read no configuration file: every setting comes from the flags and
    /// the `NARSOR_CONTROLLER__*` variables.
    #[arg(long, conflicts_with = "config")]
    pub no_config: bool,

    /// The Narsor configuration file to read the HOME settings from,
    /// instead of searching for one.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// With a control plane beyond this machine: let tasks use your own
    /// HOME (`--home host`, or `controller.local.home: host` in the configuration),
    /// with your SSH keys, tokens and harness logins.
    #[arg(long)]
    pub accept_host_home: bool,

    /// With a control plane beyond this machine: also allow a secret to be
    /// delivered as environment variable NAME (repeatable).
    #[arg(long = "allow-env", value_name = "NAME")]
    pub allow_env: Vec<String>,
}

/// The hidden helper commands of `narsor-controller local`, which
/// `narsor-cli` runs for the operator.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum LocalCommand {
    /// Manages the controller's token store.
    #[command(hide = true)]
    Secret {
        /// What to do with the store.
        #[command(subcommand)]
        command: SecretCommand,
    },
    /// Manages the task images of the docker work mode.
    #[command(hide = true)]
    Image {
        /// What to do with the images.
        #[command(subcommand)]
        command: ImageCommand,
    },
}

/// What `narsor-controller local image` can do.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum ImageCommand {
    /// Asks for a version check of every task image at its next use, even
    /// inside the update interval: writes the refresh marker.
    Refresh(ImageRefreshArgs),
}

/// Arguments of `narsor-controller local image refresh`.
#[derive(Debug, Clone, Args)]
pub struct ImageRefreshArgs {
    /// The credential file of the controller whose images these are; the
    /// marker is in the `images` directory beside it. Defaults to the
    /// credential of a controller started by hand.
    #[arg(long, env = "NARSOR_CONTROLLER_CREDENTIAL_FILE", value_name = "FILE")]
    pub credential_file: Option<PathBuf>,
}

/// What `narsor-controller local secret` can do.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum SecretCommand {
    /// Stores the value read from stdin under NAME (one trailing newline is
    /// dropped). The value is never taken from the command line.
    Set(SecretSetArgs),
}

/// Arguments of `narsor-controller local secret set`.
#[derive(Debug, Clone, Args)]
pub struct SecretSetArgs {
    /// The secret's name, as a component of the task image names it.
    #[arg(value_name = "NAME")]
    pub name: String,

    /// The credential file of the controller whose store this is; the store
    /// is the `secrets` directory beside it. Defaults to the credential of
    /// a controller started by hand.
    #[arg(long, env = "NARSOR_CONTROLLER_CREDENTIAL_FILE", value_name = "FILE")]
    pub credential_file: Option<PathBuf>,
}

/// Runs a hidden helper command of `narsor-controller local`.
pub fn local_helper_command(command: LocalCommand) -> i32 {
    match command {
        LocalCommand::Secret {
            command: SecretCommand::Set(args),
        } => exit_code(secret_set(&args, &mut std::io::stdin().lock())),
        LocalCommand::Image {
            command: ImageCommand::Refresh(args),
        } => exit_code(image_refresh(&args)),
    }
}

/// The credential file a helper command names, else the default one.
fn helper_credential_file(given: Option<&Path>) -> Result<PathBuf, ControllerError> {
    given
        .map(Path::to_path_buf)
        .or_else(default_credential_file)
        .ok_or_else(|| {
            crate::controller::missing_argument(
                "--credential-file <FILE>",
                "NARSOR_CONTROLLER_CREDENTIAL_FILE",
            )
        })
}

/// `local image refresh`: writes the refresh marker beside the credential.
fn image_refresh(args: &ImageRefreshArgs) -> Result<(), ControllerError> {
    let credential_file = helper_credential_file(args.credential_file.as_deref())?;
    let controller_dir = credential_file
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let marker = image::request_refresh(controller_dir).map_err(|error| {
        ControllerError::failure(format!("cannot write the image refresh marker: {error}"))
    })?;
    eprintln!(
        "narsor-controller: task images are checked again at their next use ({})",
        marker.display()
    );
    Ok(())
}

/// `local secret set`: the value from `input`, stored beside the credential.
fn secret_set(args: &SecretSetArgs, input: &mut impl std::io::Read) -> Result<(), ControllerError> {
    let credential_file = helper_credential_file(args.credential_file.as_deref())?;
    let mut text = String::new();
    input.read_to_string(&mut text).map_err(|error| {
        ControllerError::failure(format!("cannot read the value from stdin: {error}"))
    })?;
    let store = secrets::SecretStore::beside_credential(&credential_file);
    store
        .set(&args.name, secrets::value_from_input(&text))
        .map_err(|error| match error {
            secrets::SecretError::Io { .. } => ControllerError::failure(error.to_string()),
            _ => ControllerError::usage(error.to_string()),
        })?;
    eprintln!(
        "narsor-controller: stored secret '{}' in {}",
        args.name,
        store.dir().display()
    );
    Ok(())
}

/// What the controller's configuration step settles before anything starts:
/// how runners run and which HOME they get.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSetup {
    /// How runners run (`controller.local.mode`).
    pub mode: LocalMode,
    /// The HOME settings runners are started with.
    pub home: HarnessHome,
    /// How long a built task image is used before its component versions
    /// are checked again (Docker mode); `None` never checks again.
    pub docker_update_interval: Option<Duration>,
}

/// The controller's configuration step: the work mode and the HOME
/// settings, with the rules between them and the control plane.
///
/// Docker mode needs the trusted local control plane (KTD13): a sealed
/// task image from a server beyond this machine could otherwise run
/// scripts on this host. A container always gets a private HOME (KTD5):
/// a `host` HOME somebody set is refused, the built-in one is replaced.
fn configure(
    args: &LocalArgs,
    place: ControlPlanePlace,
    config: &Resolved,
) -> Result<LocalSetup, ControllerError> {
    let mode = config.local_mode();
    if mode == LocalMode::Docker {
        if place == ControlPlanePlace::Remote {
            return Err(ControllerError::usage(format!(
                "{} is 'docker', which needs the local daemon as the control plane: a control \
                 plane beyond this machine could have this computer build and run any image. \
                 Use mode 'process' with it",
                SettingKey::LocalMode
            )));
        }
        if args.home == Some(HomeMode::Host) {
            return Err(ControllerError::usage(format!(
                "--home host with {} 'docker': a container always runs with a private HOME on \
                 task storage. Leave --home out or use --home private",
                SettingKey::LocalMode
            )));
        }
    }
    if let Some(problem) = local_controller::problems(config).into_iter().next() {
        return Err(ControllerError::usage(problem.to_string()));
    }
    let mut home = resolve_home(args, place, config)?;
    if mode == LocalMode::Docker {
        home.mode = HomeMode::Private;
    }
    Ok(LocalSetup {
        mode,
        home,
        docker_update_interval: config
            .local_docker_update_interval()
            .map(Duration::from_secs),
    })
}

/// The HOME settings runners are started with (R17, R18; KTD15): each from
/// its flag, else from `config` — its `NARSOR_CONTROLLER__LOCAL__*` variable, else the
/// configuration file. With a control plane beyond this machine the mode is
/// `private` unless something set it, and `host` needs `--accept-host-home`;
/// so do the harness directories — the user's own logins — which the
/// configuration file then does not give either.
fn resolve_home(
    args: &LocalArgs,
    place: ControlPlanePlace,
    config: &Resolved,
) -> Result<HarnessHome, ControllerError> {
    let configured = config.local_harness_home();
    // `controller.local.home` has a built-in value (`host`); only one somebody set
    // counts for a server that is not trusted.
    let mode_set = !matches!(config.source(SettingKey::LocalHome), Source::BuiltIn);
    let mode = match (args.home, place) {
        (Some(mode), _) => mode,
        (None, ControlPlanePlace::Remote) if !mode_set => HomeMode::Private,
        (None, _) => configured.mode,
    };
    if place == ControlPlanePlace::Remote && mode == HomeMode::Host && !args.accept_host_home {
        return Err(ControllerError::usage(format!(
            "HOME mode 'host' (from {}) with a control plane beyond this machine would give \
             every task it sends your own HOME: your SSH keys, tokens and harness logins. Use \
             --home private, or accept this with --accept-host-home",
            if args.home.is_some() {
                "--home"
            } else {
                "the configuration"
            }
        )));
    }
    let untrusted = place == ControlPlanePlace::Remote && !args.accept_host_home;
    if untrusted {
        if let Some(flag) = harness_directory_flag(args) {
            return Err(ControllerError::usage(format!(
                "{flag} with a control plane beyond this machine would give every task it sends \
                 your own harness login. Leave it out, or accept this with --accept-host-home"
            )));
        }
    }
    let configured = if untrusted {
        HarnessHome {
            mode: configured.mode,
            ..HarnessHome::default()
        }
    } else {
        configured
    };
    Ok(HarnessHome {
        mode,
        codex_home: args.codex_home.clone().or(configured.codex_home),
        claude_config_dir: args
            .claude_config_dir
            .clone()
            .or(configured.claude_config_dir),
        claude_profile_source: args
            .claude_profile_source
            .clone()
            .or(configured.claude_profile_source),
    })
}

/// The first harness directory flag given, by name.
fn harness_directory_flag(args: &LocalArgs) -> Option<&'static str> {
    [
        ("--codex-home", &args.codex_home),
        ("--claude-config-dir", &args.claude_config_dir),
        ("--claude-profile-source", &args.claude_profile_source),
    ]
    .into_iter()
    .find_map(|(flag, dir)| dir.is_some().then_some(flag))
}

/// The controller's section of the Narsor configuration: from `--config`,
/// from no file at all with `--no-config`, else from the usual search; the
/// `NARSOR_GLOBAL__*` and `NARSOR_CONTROLLER__*` variables win over the file.
fn read_config(args: &LocalArgs) -> Result<Resolved, ControllerError> {
    let resolved = resolve_config(args.config.as_deref(), args.no_config);
    if !resolved.is_ok() {
        let errors: Vec<String> = resolved.errors().iter().map(ToString::to_string).collect();
        return Err(ControllerError::usage(errors.join("\n")));
    }
    Ok(resolved)
}

/// The `local` scope of the configuration: from `file`, from no file at
/// all with `no_config`, else from the cascade; and the process
/// environment.
fn resolve_config(file: Option<&Path>, no_config: bool) -> Resolved {
    let environment = narsor_engine::config::resolve::environment_from_process();
    if no_config || file.is_some() {
        local_controller::resolve(file, &environment)
    } else {
        narsor_engine::config::resolve(&Flags::default(), &environment, local_controller::SCOPE)
    }
}

/// `narsor-controller --check-config [--config <file>]`: checks the `local`
/// scope of the configuration, prints the report as JSON and exits 0 (no
/// problem) or 1, without starting anything (`narsor-cli config show`).
pub fn check_command(file: Option<&Path>) -> ! {
    let resolved = resolve_config(file, false);
    let mut problems = narsor_engine::config::Problem::of_resolution(&resolved);
    if resolved.is_ok() {
        problems.extend(local_controller::problems(&resolved));
    }
    narsor_engine::config::CheckReport::build("narsor-controller", &resolved, &problems)
        .print_and_exit()
}

/// Tells the user, before anything else happens, what serving a control
/// plane beyond this machine means.
fn warn_untrusted(control_plane: &str, home: &HarnessHome) {
    eprintln!(
        "narsor-controller: this computer is a backend of the control plane at {control_plane}."
    );
    eprintln!(
        "WARNING: that server can run any program on this computer as your user, without a \
         container; whoever takes the server over can read every file you can read. Tasks run \
         with HOME mode '{}'.",
        home.mode.as_str()
    );
    if home.mode == HomeMode::Host {
        eprintln!(
            "WARNING: --accept-host-home: tasks use your own HOME, with your SSH keys, tokens \
             and harness logins."
        );
    }
    for (name, dir) in home.variables() {
        eprintln!("WARNING: --accept-host-home: tasks use {name}={dir}, with its harness login.");
    }
}

/// Runs `narsor-controller local` until `SIGTERM`, Ctrl-C or the loss of
/// its parent.
pub async fn local_command(args: LocalArgs) -> i32 {
    exit_code(run(args).await)
}

async fn run(args: LocalArgs) -> Result<(), ControllerError> {
    let connection = args
        .connection
        .resolve_with_default_credential(default_credential_file())?;
    let place = check_transport(
        "--control-plane",
        &connection.control_plane,
        PLAIN_HTTP_BEYOND_LOOPBACK,
    )?;
    check_transport(
        "--runner-control-plane",
        &connection.runner_control_plane,
        PLAIN_HTTP_BEYOND_LOOPBACK,
    )?;
    let config = read_config(&args)?;
    // R26: an un-migrated configuration runs on built-in values; say so.
    let environment = narsor_engine::config::resolve::environment_from_process();
    if let Some(warning) = narsor_engine::config::legacy::warning(&config, &environment) {
        eprintln!("narsor-controller: warning: {warning}");
    }
    let setup = configure(&args, place, &config)?;
    let home = setup.home;
    if place == ControlPlanePlace::Remote {
        warn_untrusted(&connection.control_plane, &home);
    }
    let lock_path = lock_path(&connection.credential_file);
    let timeout = Duration::from_secs(args.lock_timeout_secs);
    let _lock = tokio::task::spawn_blocking(move || ControllerLock::acquire(&lock_path, timeout))
        .await
        .map_err(|error| ControllerError::failure(error.to_string()))??;

    let mut backend_config = LocalBackendConfig::new(&args.storage).map_err(|error| {
        ControllerError::failure(format!("cannot configure the local backend: {error}"))
    })?;
    backend_config.runner_args = home.runner_args();
    if let Some(path) = config.file().path().map(Path::to_path_buf) {
        backend_config.runner_args.push("--config".into());
        backend_config.runner_args.push(path.as_os_str().into());
    }
    if place == ControlPlanePlace::Remote {
        backend_config.launch_check = Some(untrusted_check(&args)?);
        if home.mode == HomeMode::Private {
            backend_config
                .runner_args
                .push("--pinned-git-credential-only".into());
            backend_config.withhold_user_credentials = true;
        }
    }
    // Containers are observed and removed in either mode: a process-mode
    // controller still removes what a Docker-mode one left (KTD1).
    let docker: Arc<dyn DockerTransport> = Arc::new(DockerCli::default());
    backend_config.docker = Some(Arc::clone(&docker));
    if setup.mode == LocalMode::Docker {
        backend_config.container_mode = Some(container_mode(
            docker,
            &connection.credential_file,
            setup.docker_update_interval,
            config.file().path().map(Path::to_path_buf),
        ));
    }
    let backend = LocalBackend::new(backend_config);
    if setup.mode == LocalMode::Docker {
        let collector = backend.clone();
        // Images no task names any more go at start (KTD12); a failure is
        // only logged.
        let _ = tokio::task::spawn_blocking(move || collector.collect_image_garbage()).await;
    }

    let options = ClientOptions {
        release_lease_timeout: Some(RELEASE_LEASE_TIMEOUT),
        refuse_redirects: true,
    };
    tracing::info!(
        control_plane = %connection.control_plane,
        enrolling = connection.enrollment_code.is_some(),
        "controller: connecting to the control plane"
    );
    let client = load_or_enroll(&connection, &options).await?;
    if place == ControlPlanePlace::Remote {
        match client.heartbeat().await {
            Ok(heartbeat) => eprintln!(
                "narsor-controller: serving backend '{}' as controller {}.",
                heartbeat.backend_id, heartbeat.controller_id
            ),
            Err(error) => tracing::warn!(%error, "controller: the first heartbeat failed"),
        }
    }
    let mut config = ControllerConfig::new(connection.runner_control_plane);
    config.lease_ttl = connection.lease_ttl;
    tracing::info!(
        storage = %args.storage.display(),
        remote = place == ControlPlanePlace::Remote,
        mode = setup.mode.as_str(),
        "controller: driving the local backend"
    );
    let parent = args.parent_pid;
    Controller::new(backend, client, config)
        .run(async move {
            tokio::select! {
                _ = shutdown_signal() => {}
                _ = parent_gone(parent) => {
                    tracing::warn!("controller: the parent process is gone; stopping");
                }
            }
        })
        .await;
    Ok(())
}

/// What the Docker work mode needs: the image build jobs and the token
/// store of the controller keeping `credential_file`, and the host user.
fn container_mode(
    docker: Arc<dyn DockerTransport>,
    credential_file: &Path,
    update_interval: Option<Duration>,
    config_file: Option<PathBuf>,
) -> ContainerMode {
    let controller_dir = credential_file
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let settings = image::BuildSettings::new(update_interval);
    let user = settings.user;
    ContainerMode {
        builds: image::TaskImageBuilds::new(docker, controller_dir, settings),
        secrets: secrets::SecretStore::beside_credential(credential_file),
        user,
        host_home: std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from),
        config_file,
        runner_args: Vec::new(),
        start_window: backend::DEFAULT_START_WINDOW,
    }
}

/// The checks of every launch spec from a control plane beyond this
/// machine, over the controller's storage (R17).
fn untrusted_check(args: &LocalArgs) -> Result<LaunchCheck, ControllerError> {
    create_private_dir(&args.storage)?;
    let checks = UntrustedServer::new(&args.storage, &args.allow_env).map_err(|error| {
        ControllerError::failure(format!("cannot use {}: {error}", args.storage.display()))
    })?;
    Ok(LaunchCheck::new(move |spec| checks.check(spec)))
}

fn create_private_dir(path: &Path) -> Result<(), ControllerError> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|error| {
            ControllerError::failure(format!("cannot create {}: {error}", path.display()))
        })
}

async fn load_or_enroll(
    connection: &crate::controller::Connection,
    options: &ClientOptions,
) -> Result<ControllerClient, ControllerError> {
    load_or_enroll_with_options(
        &connection.control_plane,
        &connection.credential_file,
        connection.enrollment_code.as_deref(),
        options,
    )
    .await
    .map_err(enrollment_error)
}

/// Whether plain HTTP is accepted for a control plane beyond loopback: in a
/// debug build only, for tests (R16).
const PLAIN_HTTP_BEYOND_LOOPBACK: bool = cfg!(debug_assertions);

/// Where a control plane is, as far as trusting it goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlPlanePlace {
    /// On this machine: the daemon of a trusted installation.
    Loopback,
    /// Anywhere else: its commands are not trusted (R17).
    Remote,
}

/// Checks the transport to the control plane at `url`, given as `flag`
/// (R16): beyond loopback only `https://` — with the certificate checked,
/// there is no way to turn that off — unless `plain_http_beyond_loopback`.
/// Loopback is a loopback IP address or exactly `localhost`, never a name
/// looked up in DNS.
pub fn check_transport(
    flag: &str,
    url: &str,
    plain_http_beyond_loopback: bool,
) -> Result<ControlPlanePlace, ControllerError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|error| ControllerError::usage(format!("{flag} '{url}' is not a URL: {error}")))?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(ControllerError::usage(format!(
            "{flag} '{url}' must not carry user information"
        )));
    }
    let place = if narsor_engine::config::is_loopback_host(&parsed) {
        ControlPlanePlace::Loopback
    } else {
        ControlPlanePlace::Remote
    };
    match parsed.scheme() {
        "https" => Ok(place),
        "http" if place == ControlPlanePlace::Loopback || plain_http_beyond_loopback => Ok(place),
        "http" => Err(ControllerError::usage(format!(
            "{flag} '{url}' uses plain HTTP: a control plane beyond this machine must be reached \
             over https://, so that nobody on the way can read or change what it sends"
        ))),
        other => Err(ControllerError::usage(format!(
            "{flag} '{url}' uses the unsupported scheme '{other}'; use https://"
        ))),
    }
}

/// The credential file of a controller started by hand:
/// `$XDG_STATE_HOME/narsor/workstation-controller/credential`, else under
/// `~/.local/state`.
fn default_credential_file() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|dir| !dir.is_empty())
                .map(|home| PathBuf::from(home).join(".local/state"))
        })?;
    Some(state.join("narsor/workstation-controller/credential"))
}

/// The lock file of the controller keeping `credential_file`:
/// `<credential_file>.lock`.
fn lock_path(credential_file: &Path) -> PathBuf {
    let mut path = credential_file.as_os_str().to_owned();
    path.push(LOCK_SUFFIX);
    PathBuf::from(path)
}

/// Resolves when `parent` is set and this process is no longer its child;
/// never otherwise.
async fn parent_gone(parent: Option<u32>) {
    let Some(parent) = parent else {
        return std::future::pending().await;
    };
    let mut interval = tokio::time::interval(PARENT_CHECK_INTERVAL);
    loop {
        interval.tick().await;
        // SAFETY: getppid has no preconditions and cannot fail.
        if unsafe { libc::getppid() } as u32 != parent {
            return;
        }
    }
}

/// The exclusive lock on a controller credential, held while it lives.
#[derive(Debug)]
struct ControllerLock {
    _file: fs::File,
}

impl ControllerLock {
    /// Takes the lock at `path`, waiting up to `timeout` for its holder, and
    /// records this process's PID in it.
    fn acquire(path: &Path, timeout: Duration) -> Result<Self, ControllerError> {
        if let Some(parent) = path.parent() {
            create_private_dir(parent)?;
        }
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .map_err(|error| {
                ControllerError::failure(format!("cannot open {}: {error}", path.display()))
            })?;
        let deadline = Instant::now() + timeout;
        let mut reported = false;
        loop {
            // SAFETY: flock on a descriptor this function owns.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(ControllerError::failure(format!(
                    "cannot lock {}: {error}",
                    path.display()
                )));
            }
            let holder = holder_pid(&mut file);
            if !reported {
                tracing::warn!(
                    pid = %holder,
                    lock = %path.display(),
                    "controller: another controller holds this credential; waiting for it to stop"
                );
                reported = true;
            }
            if Instant::now() >= deadline {
                return Err(ControllerError::failure(format!(
                    "another narsor-controller (pid {holder}) holds {}; it did not stop within \
                     {} s",
                    path.display(),
                    timeout.as_secs()
                )));
            }
            std::thread::sleep(LOCK_RETRY_INTERVAL);
        }
        let recorded = file
            .set_len(0)
            .and_then(|()| file.rewind())
            .and_then(|()| writeln!(file, "{}", std::process::id()));
        if let Err(error) = recorded {
            tracing::warn!(%error, "controller: could not record its pid in the lock file");
        }
        Ok(Self { _file: file })
    }
}

/// The PID the holder of the lock recorded, as written, or `unknown`.
fn holder_pid(file: &mut fs::File) -> String {
    let mut text = String::new();
    let read = file.rewind().and_then(|()| file.read_to_string(&mut text));
    match read {
        Ok(_) if !text.trim().is_empty() => text.trim().to_string(),
        _ => "unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::ConnectionArgs;
    use narsor_engine::config::Environment;

    fn args() -> LocalArgs {
        LocalArgs {
            connection: ConnectionArgs::default(),
            storage: PathBuf::from("/storage"),
            home: None,
            codex_home: None,
            claude_config_dir: None,
            claude_profile_source: None,
            lock_timeout_secs: 10,
            parent_pid: None,
            no_config: false,
            config: None,
            accept_host_home: false,
            allow_env: Vec::new(),
        }
    }

    /// The controller's configuration from a file holding `controller`
    /// (none: no file) and `env`, as the daemon hands it over.
    fn configuration(controller: Option<&str>, env: &[(&str, &str)]) -> Resolved {
        let dir = std::env::temp_dir().join(format!(
            "narsor-controller-home-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&dir).expect("create the directory");
        let file = controller.map(|text| {
            let path = dir.join("config.yaml");
            fs::write(&path, format!("controller:\n  local:\n{text}")).expect("write the file");
            path
        });
        let environment: Environment = env
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        let resolved = local_controller::resolve(file.as_deref(), &environment);
        let _ = fs::remove_dir_all(&dir);
        assert!(resolved.is_ok(), "{:?}", resolved.errors());
        resolved
    }

    #[test]
    fn a_remote_control_plane_gets_a_private_home_unless_the_configuration_set_one() {
        let remote = ControlPlanePlace::Remote;
        // KTD15: the built-in `host` does not count for a remote server.
        let home = resolve_home(&args(), remote, &configuration(None, &[])).unwrap();
        assert_eq!(home.mode, HomeMode::Private);

        let home = resolve_home(
            &args(),
            remote,
            &configuration(Some("    home: private\n"), &[]),
        )
        .unwrap();
        assert_eq!(home.mode, HomeMode::Private);

        // A `host` somebody set counts, so it needs the acceptance.
        let set_host = configuration(
            Some("    home: private\n"),
            &[("NARSOR_CONTROLLER__LOCAL__HOME", "host")],
        );
        let refused = resolve_home(&args(), remote, &set_host).unwrap_err();
        assert!(
            refused.message.contains("the configuration"),
            "{}",
            refused.message
        );
        let accepted = LocalArgs {
            accept_host_home: true,
            ..args()
        };
        assert_eq!(
            resolve_home(&accepted, remote, &set_host).unwrap().mode,
            HomeMode::Host
        );
    }

    #[test]
    fn docker_mode_runs_with_a_private_home_on_the_local_daemon_only() {
        let loopback = ControlPlanePlace::Loopback;
        let docker = configuration(Some("    mode: docker\n"), &[]);
        let setup = configure(&args(), loopback, &docker).unwrap();
        assert_eq!(setup.mode, LocalMode::Docker);
        // KTD5: the built-in `host` gives way to a private HOME.
        assert_eq!(setup.home.mode, HomeMode::Private);
        assert_eq!(
            setup.docker_update_interval,
            Some(Duration::from_secs(24 * 60 * 60))
        );

        let explicit_host = configuration(Some("    mode: docker\n    home: host\n"), &[]);
        let host_flag = LocalArgs {
            home: Some(HomeMode::Host),
            ..args()
        };
        for (case, args, place, config, expected) in [
            (
                "home: host set in the file",
                args(),
                loopback,
                &explicit_host,
                vec!["controller.local.mode", "controller.local.home"],
            ),
            (
                "--home host",
                host_flag,
                loopback,
                &docker,
                vec!["--home host", "controller.local.mode"],
            ),
            (
                "a remote control plane",
                args(),
                ControlPlanePlace::Remote,
                &docker,
                vec!["controller.local.mode", "local daemon"],
            ),
        ] {
            let refused = configure(&args, place, config).unwrap_err();
            for needle in expected {
                assert!(
                    refused.message.contains(needle),
                    "{case}: '{needle}' missing from: {}",
                    refused.message
                );
            }
        }
    }
}
