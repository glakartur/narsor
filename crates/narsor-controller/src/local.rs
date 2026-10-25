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
//! `controller.lock` beside the credential file and writes its PID into it;
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

use crate::controller::{exit_code, shutdown_signal, ConnectionArgs, ControllerError};
use narsor_controller_runtime::{Controller, ControllerConfig};
use narsor_engine::backends::{LocalBackend, LocalBackendConfig};
use narsor_engine::config::{HarnessHome, HomeMode};
use narsor_engine::controller_protocol::{ClientOptions, ControllerClient};
use narsor_engine::controller_runtime::load_or_enroll_with_options;

/// Name of the lock file beside the credential file.
pub const LOCK_FILE: &str = "controller.lock";

/// Bound of the lease release on the way out: well inside the daemon's
/// stop timeout (10 s), so a release against a dead control plane cannot
/// get the controller killed (KTD8).
pub const RELEASE_LEASE_TIMEOUT: Duration = Duration::from_secs(5);

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
    #[arg(long)]
    pub no_config: bool,
}

impl LocalArgs {
    /// The HOME mode and harness directories runners are started with.
    pub fn harness_home(&self) -> HarnessHome {
        HarnessHome {
            mode: self.home.unwrap_or_default(),
            codex_home: self.codex_home.clone(),
            claude_config_dir: self.claude_config_dir.clone(),
            claude_profile_source: self.claude_profile_source.clone(),
        }
    }
}

/// Runs `narsor-controller local` until `SIGTERM`, Ctrl-C or the loss of
/// its parent.
pub async fn local_command(args: LocalArgs) -> i32 {
    exit_code(run(args).await)
}

async fn run(args: LocalArgs) -> Result<(), ControllerError> {
    let connection = args.connection.resolve()?;
    let lock_path = lock_path(&connection.credential_file);
    let timeout = Duration::from_secs(args.lock_timeout_secs);
    let _lock = tokio::task::spawn_blocking(move || ControllerLock::acquire(&lock_path, timeout))
        .await
        .map_err(|error| ControllerError::failure(error.to_string()))??;

    let mut backend_config = LocalBackendConfig::new(&args.storage).map_err(|error| {
        ControllerError::failure(format!("cannot configure the local backend: {error}"))
    })?;
    backend_config.runner_args = args.harness_home().runner_args();
    let backend = LocalBackend::new(backend_config);

    let options = ClientOptions {
        release_lease_timeout: Some(RELEASE_LEASE_TIMEOUT),
        ..ClientOptions::default()
    };
    tracing::info!(
        control_plane = %connection.control_plane,
        enrolling = connection.enrollment_code.is_some(),
        "controller: connecting to the control plane"
    );
    let client = load_or_enroll(&connection, &options).await?;
    let mut config = ControllerConfig::new(connection.runner_control_plane);
    config.lease_ttl = connection.lease_ttl;
    tracing::info!(
        storage = %args.storage.display(),
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
    .map_err(|error| {
        if error.is_usage_error() {
            ControllerError::usage(error.message())
        } else {
            ControllerError::failure(error.message())
        }
    })
}

/// The lock file of the controller keeping `credential_file`.
pub fn lock_path(credential_file: &Path) -> PathBuf {
    credential_file
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(LOCK_FILE)
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
pub struct ControllerLock {
    _file: fs::File,
}

impl ControllerLock {
    /// Takes the lock at `path`, waiting up to `timeout` for its holder, and
    /// records this process's PID in it.
    pub fn acquire(path: &Path, timeout: Duration) -> Result<Self, ControllerError> {
        if let Some(parent) = path.parent() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .map_err(|error| {
                    ControllerError::failure(format!("cannot create {}: {error}", parent.display()))
                })?;
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
