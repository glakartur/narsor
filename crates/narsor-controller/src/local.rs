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
//! flags, then the `NARSOR_LOCAL_*` variables, then the Narsor configuration
//! file — which the daemon's child, started with `--no-config`, never reads.
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
use std::time::{Duration, Instant};

use clap::Args;

use crate::controller::{
    enrollment_error, exit_code, shutdown_signal, ConnectionArgs, ControllerError,
};
use untrusted::UntrustedServer;

mod untrusted;
use narsor_controller_runtime::{Controller, ControllerConfig};
use narsor_engine::backends::{LaunchCheck, LocalBackend, LocalBackendConfig};
use narsor_engine::config::{Flags, HarnessHome, HomeMode, Resolved, SettingKey, Source};
use narsor_engine::controller_protocol::{ClientOptions, ControllerClient};
use narsor_engine::controller_runtime::load_or_enroll_with_options;

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

    /// Read no configuration file: every setting comes from the flags.
    #[arg(long, conflicts_with = "config")]
    pub no_config: bool,

    /// The Narsor configuration file to read the HOME settings from,
    /// instead of searching for one.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// With a control plane beyond this machine: let tasks use your own
    /// HOME (`--home host`, or `local_home: host` in the configuration),
    /// with your SSH keys, tokens and harness logins.
    #[arg(long)]
    pub accept_host_home: bool,

    /// With a control plane beyond this machine: also allow a secret to be
    /// delivered as environment variable NAME (repeatable).
    #[arg(long = "allow-env", value_name = "NAME")]
    pub allow_env: Vec<String>,
}

/// The HOME settings runners are started with (R17, R18; KTD15): each from
/// its flag, else from `config` — its `NARSOR_LOCAL_*` variable, else the
/// configuration file. With a control plane beyond this machine the mode is
/// `private` unless something set it, and `host` needs `--accept-host-home`;
/// so do the harness directories — the user's own logins — which the
/// configuration file then does not give either.
fn resolve_home(
    args: &LocalArgs,
    place: ControlPlanePlace,
    config: Option<&Resolved>,
) -> Result<HarnessHome, ControllerError> {
    let configured = config.map(Resolved::local_harness_home).unwrap_or_default();
    // `local_home` has a built-in value (`host`); only one somebody set
    // counts for a server that is not trusted.
    let mode_set = config
        .is_some_and(|config| !matches!(config.source(SettingKey::LocalHome), Source::BuiltIn));
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

/// The Narsor configuration as a hand-started controller reads it:
/// `--config` or the usual search, then the `NARSOR_*` variables.
fn read_config(args: &LocalArgs) -> Result<Resolved, ControllerError> {
    let flags = Flags {
        root: None,
        config: args.config.clone(),
    };
    let resolved = narsor_engine::config::resolve(
        &flags,
        &narsor_engine::config::resolve::environment_from_process(),
    );
    if !resolved.is_ok() {
        let errors: Vec<String> = resolved.errors().iter().map(ToString::to_string).collect();
        return Err(ControllerError::usage(errors.join("\n")));
    }
    Ok(resolved)
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
    let config = if args.no_config {
        None
    } else {
        Some(read_config(&args)?)
    };
    let home = resolve_home(&args, place, config.as_ref())?;
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
    if place == ControlPlanePlace::Remote {
        backend_config.launch_check = Some(untrusted_check(&args)?);
        if home.mode == HomeMode::Private {
            backend_config
                .runner_args
                .push("--pinned-git-credential-only".into());
            backend_config.withhold_user_credentials = true;
        }
    }
    let backend = LocalBackend::new(backend_config);

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
