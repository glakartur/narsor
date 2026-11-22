//! The daemon process lifecycle (U2).
//!
//! # Control-plane recovery
//!
//! The remote-task control plane (`narsor_engine::orchestration`) keeps its own
//! records; [`recover_orchestration_at_startup`] repairs them before the
//! API — whose control-plane routes this daemon serves — accepts anything.
//! The daemon drives no execution itself: every task runs in a
//! `narsor-runner` under a controller, so there is no execution of its own
//! to reconcile or drain.
//!
//! # The observer worker
//!
//! With `server.observer.enabled: true` the daemon starts
//! `narsor-observer-worker` once its API listens, after the local
//! controller, and stops it first at shutdown, while the API still takes
//! its results ([`crate::observer_worker`]). A configuration that cannot
//! start it (no script user and no waiver, network mode without
//! `server.observer.server_url`) is refused by the configuration step
//! ([`crate::settings::configure`]), before anything starts.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::signal::unix::{signal, SignalKind};

use crate::api::{ApiHandle, ApiServices, ServerSecurity};
use crate::auth_gate::NetworkAuth;
use crate::local_controller::{controller_program, LocalController, LocalControllerSettings};
use crate::observer_worker::{
    worker_program, ObserverPlan, ObserverWorker, ObserverWorkerSettings,
};
use crate::settings::ServerSettings;
use narsor_engine::auth::{OidcVerifier, OwnerStore, SecurityPlan, StartupOwnership};
use narsor_engine::config::{local_controller, Resolved, SettingKey};
use narsor_engine::controller_protocol::{AutoResumePass, ProtocolService};
use narsor_engine::engine::HarnessRegistry;
use narsor_engine::environment::{BackendKind, BackendRecord, EnvironmentCatalog};
use narsor_engine::launch::{CatalogLaunchResolver, HarnessDefaults};
use narsor_engine::observers::{stored_observers_warning, ObserverStore};
use narsor_engine::orchestration::ControlPlane;
use narsor_engine::persistence::{DaemonInfo, DefaultHarnessSelector};
use narsor_engine::workflows::WorkflowResolver;

use narsor_shared::cli_error::{CliError, Result};
use narsor_shared::config::old_layout::old_state_root;

/// How many times a starting daemon tries the instance lock, and the pause
/// between tries: about 20 s, longer than a predecessor's shutdown drain.
const INSTANCE_LOCK_ATTEMPTS: u32 = 40;
const INSTANCE_LOCK_PAUSE: Duration = Duration::from_millis(500);

/// How long the daemon waits for [`crate::api::ApiHandle::wait_drained`]
/// (`axum::serve`'s `with_graceful_shutdown`, its connections-close half) to
/// finish before giving up on it and exiting anyway.
///
/// `axum::serve`'s graceful shutdown waits for every open connection to
/// close on its own — normally near-instant, since an HTTP response ends the
/// connection (or it sits idle only briefly). An SSE stream
/// (`GET /executions/:id/stream`/`GET /events/stream`) is the exception: it
/// is meant to stay open for as long as its client cares to hold it, so if
/// that client's *process* dies without the kernel getting a chance to send
/// a clean TCP close (killed by `SIGKILL`, the machine loses network, ...),
/// the connection can sit half-open from this side indefinitely and
/// `wait_drained` never resolves on its own. No test exercises this specific
/// branch end to end — doing so would need a client that opens an SSE stream
/// and is then killed without a clean close, which none of this crate's
/// current SSE tests do (they all disconnect cleanly); the timeout below is
/// reasoned about, not proven by a failing-then-passing test the way the
/// shutdown-ordering fix a few lines down (`api_handle.stop_accepting()`
/// before the drain, not after) is. Bounding this the same way
/// [`drain_active_runs`] bounds its own wait keeps `SIGTERM`/`daemon stop`
/// reliably responsive regardless of what any given client does; the
/// process exiting closes every remaining connection anyway, and nothing
/// about workflow correctness depends on this finishing cleanly, only on it
/// not hanging forever.
const HTTP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// `narsor daemon` — the foreground entry point: recovery, bind, discovery
/// file, serve until `SIGTERM`, graceful shutdown.
///
/// Runs until it receives `SIGTERM` (from `narsor-server stop`, or any other
/// sender — `kill -TERM <pid>` works too, deliberately, since the discovery
/// file already publishes the PID). Never returns early on its own (R3):
/// the only way out is that signal.
///
/// `settings` is what [`crate::settings::configure`] made of `config`:
/// every rule between the settings already held.
pub async fn run_daemon(config: &Resolved, settings: ServerSettings) -> Result<()> {
    let paths = narsor_engine::persistence::ServerStatePaths::new(config.state_dir());
    // Clients find this daemon only through the rendezvous directory (R7).
    let run = narsor_shared::run_paths::RunPaths::new(config.state_dir());
    let daemon_file = run.daemon_file();
    // One daemon per state directory, held until this function returns. A
    // predecessor that is still shutting down is waited for: two daemons
    // must never drive the same tasks.
    let mut instance_lock = crate::instance_lock::FlockFile::new(paths.daemon_instance_lock_file());
    crate::instance_lock::acquire(
        &mut instance_lock,
        INSTANCE_LOCK_ATTEMPTS,
        INSTANCE_LOCK_PAUSE,
    )
    .await
    .map_err(|error| CliError::failure(error.to_string()))?;
    // An installation of the old layout is moved by `narsor-migrate`, never
    // here: refused before anything reads the server's files (KTD10).
    crate::layout_guard::refuse_old_layout(config, &paths).map_err(CliError::failure)?;
    // Stored observers of older versions are never read or moved:
    // automations and queues are catalogue files now (R22). Say so once.
    if let Some(warning) = stored_observers_warning(&old_state_root(config)) {
        tracing::warn!("daemon: {warning}");
    }
    // Refused before anything starts, not after the controller enrolled.
    run.prepare()
        .map_err(|error| CliError::failure(error.to_string()))?;
    let runs_local_controller = settings.runs_local_controller();
    let ServerSettings {
        security,
        observer,
        owner_defaults,
    } = settings;
    let security = server_security(security, &paths)?;

    let control_plane = recover_orchestration_at_startup(ControlPlane::new(paths.clone()));

    if runs_local_controller {
        ensure_local_backend(config.catalog_dir())?;
    } else {
        refuse_declared_local_backend(config.catalog_dir())?;
    }
    // A harness without a model for a tier is a configuration error found
    // here, not a step that fails hours into a task.
    HarnessRegistry::defaults()
        .check_model_tiers()
        .map_err(CliError::failure)?;
    let supported = HarnessRegistry::defaults()
        .supported()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let selector = Arc::new(
        DefaultHarnessSelector::load(paths.clone(), supported, config.agent_harness().to_string())
            .await
            .map_err(|error| {
                CliError::failure(format!("could not load default harness selection: {error}"))
            })?,
    );
    // Tasks resolve against the same catalogue and start on the same live
    // default harness a local harnessless step would use; their runners
    // follow it afterwards, through the control plane's answers (U7).
    if !runs_local_controller && owner_defaults.default_backend == LOCAL_BACKEND_ID {
        let key = SettingKey::DefaultBackend;
        tracing::warn!(
            "daemon: {key} is '{LOCAL_BACKEND_ID}', which a network server does not have; a \
             task that names no backend is refused until {key} names another backend"
        );
    }
    let launch = CatalogLaunchResolver::new(
        config.catalog_dir(),
        WorkflowResolver::new(config.catalog_dir()),
        owner_defaults,
        Arc::clone(&selector) as Arc<dyn HarnessDefaults>,
    );
    // A network server has no local controller, so no built-in `local`
    // backend either: a task sent there is refused at once (R13).
    let launch = Arc::new(if runs_local_controller {
        launch
    } else {
        launch.without_builtin_local()
    });
    // A stopped task is read by a read-only runner its controller starts
    // (D63): the control plane reads no task storage, local backend or not.
    let mut service = ProtocolService::new(control_plane, launch.clone())
        .with_max_concurrent_tasks(config.max_concurrent_tasks());
    // The tasks an `always` queue creates belong to the installation owner
    // (A4): the trusted mode's local owner, or the network owner claimed or
    // configured, read at each use.
    if let ServerSecurity::Network { auth, .. } = &security {
        let auth = Arc::clone(auth);
        service = service.with_installation_owner(Arc::new(move || {
            auth.owners()
                .owner()
                .map(|owner| owner.identity().owner_ref())
        }));
    }
    let control = Arc::new(service);
    let _scheduler = spawn_periodic_admission(Arc::clone(&control));

    let api_handle = crate::api::serve(
        config.catalog_dir().to_path_buf(),
        ApiServices {
            harness_selector: Some(selector),
            control: Some(Arc::clone(&control)),
            local_launch: Some(launch),
            ..ApiServices::default()
        },
        security,
    )
    .await
    .map_err(|error| {
        CliError::failure(format!("could not start the daemon's API server: {error}"))
    })?;

    let observer_worker = observer
        .map(|plan| observer_worker_settings(plan, config, &api_handle))
        .transpose()?;

    let info = DaemonInfo {
        pid: std::process::id(),
        addr: api_handle.addr(),
        token: api_handle.token().to_string(),
        started_at: Utc::now(),
    };
    // The listener must be ready before the controller enrolls: both the
    // controller and the runner it later starts reach the same local control
    // plane URL.  Do not make the daemon discoverable until the controller
    // holds a lease of its own: otherwise a freshly connected local client
    // can create work in the gap before any controller can take it (R7).
    let local_controller = if runs_local_controller {
        let settings = local_controller_settings(config, &api_handle)?;
        Some(
            LocalController::start(settings, Arc::clone(&control))
                .await
                .map_err(CliError::failure)?,
        )
    } else {
        tracing::info!(
            "daemon: network mode serves remote backends only; the built-in local controller does not start"
        );
        None
    };
    let observer_worker = match observer_worker {
        Some(settings) => {
            let control = Arc::clone(&control);
            match ObserverWorker::start(settings, move || control.issue_observer_worker_key()) {
                Ok(worker) => Some(worker),
                Err(message) => {
                    if let Some(local_controller) = local_controller {
                        local_controller.stop().await;
                    }
                    return Err(CliError::failure(message));
                }
            }
        }
        None => None,
    };
    if let Err(error) = info.write(&daemon_file) {
        if let Some(observer_worker) = observer_worker {
            observer_worker.stop().await;
        }
        if let Some(local_controller) = local_controller {
            local_controller.stop().await;
        }
        return Err(CliError::failure(format!(
            "could not write the daemon discovery file: {error}"
        )));
    }
    tracing::info!(
        pid = info.pid,
        addr = %info.addr,
        "daemon: started and discoverable"
    );

    let mut terminate = signal(SignalKind::terminate()).map_err(|error| {
        CliError::failure(format!("could not install the SIGTERM handler: {error}"))
    })?;
    wait_for_sigterm_keeping_discovery(&mut terminate, &daemon_file, &info).await;

    // A worker's claim held open (a remote worker's too) answers "nothing"
    // now, so the drain below does not wait for its limit (U6).
    control.release_observer_claims();

    // The worker first, while the API still takes the result of a run in
    // progress; one it cannot report is lost at its deadline.
    if let Some(observer_worker) = observer_worker {
        observer_worker.stop().await;
    }

    // Stop the local controller while the API still accepts its signed
    // lease release (R8). A successor can then acquire the lease
    // immediately rather than waiting for expiry.
    if let Some(local_controller) = local_controller {
        local_controller.stop().await;
    }

    // An in-flight request still completes normally (`stop_accepting` only
    // refuses *new* connections).
    api_handle.stop_accepting();

    if tokio::time::timeout(HTTP_SHUTDOWN_TIMEOUT, api_handle.wait_drained())
        .await
        .is_err()
    {
        tracing::warn!(
            "daemon: the HTTP server did not finish draining its connections within the \
             shutdown timeout (likely a client that died without closing an SSE stream \
             cleanly); exiting anyway"
        );
    }
    cleanup_discovery_file(&daemon_file);

    Ok(())
}

const LOCAL_BACKEND_ID: &str = BackendRecord::LOCAL_ID;

/// Refuses to start when the catalogue declares a `local` backend that is
/// not a local one. An undeclared `local` backend is built in and is never
/// written to the (declaration-only) catalogue.
fn ensure_local_backend(catalog_dir: &Path) -> Result<()> {
    let record = EnvironmentCatalog::new(catalog_dir)
        .backend(LOCAL_BACKEND_ID)
        .map_err(|error| CliError::failure(format!("could not load local backend: {error}")))?
        .unwrap_or_else(BackendRecord::builtin_local);
    if record.kind == BackendKind::Local {
        Ok(())
    } else {
        Err(CliError::failure(format!(
            "backend '{LOCAL_BACKEND_ID}' is configured as {:?}, not a local backend",
            record.kind
        )))
    }
}

/// Refuses to start a network server whose catalogue declares a backend
/// `local`: the id is the trusted daemon's own (R13).
fn refuse_declared_local_backend(catalog_dir: &Path) -> Result<()> {
    let declared = EnvironmentCatalog::new(catalog_dir)
        .declares_backend(LOCAL_BACKEND_ID)
        .map_err(|error| CliError::failure(format!("could not read the catalogue: {error}")))?;
    if declared {
        return Err(CliError::failure(format!(
            "backends/{LOCAL_BACKEND_ID}.yaml: the id '{LOCAL_BACKEND_ID}' is reserved for the \
             built-in backend of a trusted daemon; give this backend another id (for example \
             'laptop')"
        )));
    }
    Ok(())
}

/// What the daemon hands its local controller (KTD3): the state and work
/// roots and the launch settings it resolved. The controller derives its
/// own directories below the roots (KTD7).
fn local_controller_settings(
    config: &Resolved,
    api: &ApiHandle,
) -> Result<LocalControllerSettings> {
    let program = controller_program().map_err(CliError::failure)?;
    Ok(LocalControllerSettings {
        program,
        control_plane: format!("http://{}", api.addr()),
        state_root: config.state_dir().to_path_buf(),
        work_root: config.work_dir().to_path_buf(),
        // The controller reads its own section (R21) from the file this
        // daemon read, on every start: a restart reads the file again.
        config_args: local_controller::config_args(config.file().path()),
        forwarded_env_prefixes: local_controller::forwarded_prefixes(),
        log_directive: config.log_directive(),
    })
}

/// What the daemon starts its observer worker with: the checked `plan`,
/// with the API's own address when no URL is configured (only a trusted
/// server gets this far without one).
fn observer_worker_settings(
    plan: ObserverPlan,
    config: &Resolved,
    api: &ApiHandle,
) -> Result<ObserverWorkerSettings> {
    Ok(ObserverWorkerSettings {
        program: worker_program().map_err(CliError::failure)?,
        server: plan
            .server
            .unwrap_or_else(|| format!("http://{}", api.addr())),
        secrets_dir: plan.secrets_dir,
        isolation: plan.isolation,
        log_directive: config.log_directive(),
        supervision: narsor_controller_runtime::Supervision::default(),
    })
}

/// How often the daemon re-evaluates queue admission on its own (U10).
const ADMISSION_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Re-evaluates admission periodically, beside the event-driven passes.
///
/// Two inputs change without any request announcing it: a controller's
/// lease running out (its capacity is then withheld and waiting tasks say
/// `controller_offline`) and an operator editing a backend's limit in the
/// catalogue. The resume policy resumes tasks whose workload was lost once
/// their attempt is due, task materials whose time to live ran out are
/// swept (U9), and observers whose run is due are flagged, on the same tick. The handle is dropped with the daemon's
/// runtime.
fn spawn_periodic_admission(control: Arc<ProtocolService>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(ADMISSION_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let control = Arc::clone(&control);
            // File I/O under advisory locks: keep it off the async workers.
            let pass = move || {
                run_periodic_pass(&control);
            };
            if let Err(error) = tokio::task::spawn_blocking(pass).await {
                tracing::warn!(%error, "scheduler: periodic admission pass panicked");
            }
        }
    })
}

/// One tick of the daemon's periodic pass: admission, then the resume
/// policy, then the slot-release policy (D62), then task materials whose
/// time to live ran out (U9), the reader leases that ran idle (D63), and the
/// observers' schedule. Returns what the resume policy did.
pub fn run_periodic_pass(control: &ProtocolService) -> AutoResumePass {
    control.reschedule_all();
    // Asks running tasks to stop while a backend holds more slots than its
    // limit allows, e.g. after the operator edited the backend file.
    control.limit_stop_pass();
    let resumed = control.auto_resume_pass();
    // Stops tasks whose deadline came; the slot itself goes on once the
    // runner reached its safe point, and that report re-runs admission.
    control.slot_release_pass();
    // Ends the leases of read-only runners that ran idle, so the desired
    // state stops asking the controller for them.
    control.reader_lease_pass();
    control.sweep_materials();
    // Flags the observers whose run is due for the worker's claim, and
    // records the runs no worker finished in time as lost.
    control.observer_pass();
    resumed
}

/// The server's security from its checked `plan` (U7).
///
/// In network mode this also opens the installation's ownership records:
/// with no owner configured or recorded, a fresh one-time claim code is
/// printed to stderr (the daemon's log in a container) and stored `0600`
/// beside them for the operator.
fn server_security(
    plan: SecurityPlan,
    paths: &narsor_engine::persistence::ServerStatePaths,
) -> Result<ServerSecurity> {
    let network = match plan {
        SecurityPlan::Trusted { bind, cors } => return Ok(ServerSecurity::Trusted { bind, cors }),
        SecurityPlan::Network(network) => network,
    };
    let issuer = network.oidc.issuer.clone();
    let verifier = OidcVerifier::new(network.oidc)
        .map_err(|error| CliError::failure(format!("{}: {error}", SettingKey::OidcIssuer)))?;
    let (owners, ownership) = OwnerStore::open(paths, &issuer, network.owner_subject.as_deref())
        .map_err(|error| {
            CliError::failure(format!(
                "could not open the installation owner record: {error}"
            ))
        })?;
    match ownership {
        StartupOwnership::Owned(owner) => tracing::info!(
            subject = %owner.subject,
            bound_by = ?owner.bound_by,
            "daemon: network mode; the installation has an owner"
        ),
        StartupOwnership::Unclaimed { code, code_file } => {
            eprintln!(
                "narsor: this installation has no owner yet. The first user to claim it with \
                 this one-time code becomes its owner:\n  {}\n(also stored in {}; a restart \
                 issues a new code)",
                code.expose(),
                code_file.display()
            );
            tracing::warn!("daemon: network mode; the installation is unclaimed");
        }
    }
    Ok(ServerSecurity::Network {
        bind: network.bind,
        auth: Arc::new(NetworkAuth::new(verifier, owners)),
        proxy: network.proxy,
        cors: network.cors,
    })
}

/// Repairs the control plane's own records (U2) — orphaned temporary files
/// and events a crash kept out of a task's log — before the API serves
/// them, so a stream replay never skips one. It judges no task: a task's runner state changes only through its controller's
/// reconciliation, never because this process restarted. The automation
/// store's orphaned temporary files are removed here too, and stored
/// observers of older versions get their one warning. Best-effort: a
/// failure is logged and the daemon still starts.
pub fn recover_orchestration_at_startup(control: ControlPlane) -> ControlPlane {
    match control.recover() {
        Ok(recovery) => {
            for path in &recovery.removed_temp_files {
                tracing::info!(
                    path = %path.display(),
                    "daemon: removed an interrupted control-plane write"
                );
            }
            for (task_id, sequence) in &recovery.republished_events {
                tracing::info!(
                    task_id = %task_id,
                    sequence,
                    "daemon: republished a control-plane event a crash kept out of the log"
                );
            }
            for task_id in &recovery.adopted_losses {
                tracing::info!(
                    task_id = %task_id,
                    "daemon: adopted a workload loss from before the resume policy"
                );
            }
        }
        Err(error) => {
            tracing::warn!(error = %error, "daemon: control-plane recovery failed");
        }
    }
    let observers = ObserverStore::new(control.store().paths().clone());
    match observers.cleanup_orphaned_temp_files() {
        Ok(removed) => {
            for path in &removed {
                tracing::info!(
                    path = %path.display(),
                    "daemon: removed an interrupted observer write"
                );
            }
        }
        Err(error) => {
            tracing::warn!(error = %error, "daemon: observer store recovery failed");
        }
    }
    control
}

/// How often the daemon checks that its discovery file is still there.
const DISCOVERY_WATCH_INTERVAL: Duration = Duration::from_secs(5);

/// What the daemon found at its discovery file.
#[derive(Debug, PartialEq, Eq)]
enum DiscoveryRead {
    /// The file is gone.
    Missing,
    /// The file names this PID.
    Pid(u32),
    /// The file exists but could not be read now. Not a reason to write it.
    Unreadable,
}

impl DiscoveryRead {
    fn from_result(result: narsor_engine::persistence::Result<DaemonInfo>) -> Self {
        use narsor_engine::persistence::PersistenceError;
        match result {
            Ok(info) => Self::Pid(info.pid),
            Err(PersistenceError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Self::Missing
            }
            Err(_) => Self::Unreadable,
        }
    }

    /// `true` when no client can find this daemon: a client removed the
    /// file, or it names another PID. The daemon holds the instance lock,
    /// so it is the only legitimate daemon and writes the record again.
    fn needs_rewrite(&self, own_pid: u32) -> bool {
        match self {
            Self::Missing => true,
            Self::Pid(pid) => *pid != own_pid,
            Self::Unreadable => false,
        }
    }
}

/// Waits for `SIGTERM`. Meanwhile, writes `info` again whenever the
/// discovery file is gone, so a client that removed it by mistake does not
/// leave a running daemon undiscoverable.
async fn wait_for_sigterm_keeping_discovery(
    terminate: &mut tokio::signal::unix::Signal,
    daemon_file: &Path,
    info: &DaemonInfo,
) {
    let mut interval = tokio::time::interval(DISCOVERY_WATCH_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await;
    loop {
        tokio::select! {
            _ = terminate.recv() => {
                tracing::info!("daemon: received SIGTERM, shutting down");
                return;
            }
            _ = interval.tick() => {
                let read = DiscoveryRead::from_result(DaemonInfo::read(daemon_file));
                if read.needs_rewrite(info.pid) {
                    match info.write(daemon_file) {
                        Ok(()) => tracing::warn!(
                            ?read,
                            "daemon: the discovery file was gone or foreign, wrote it again"
                        ),
                        Err(error) => tracing::warn!(
                            error = %error,
                            "daemon: could not write the discovery file again"
                        ),
                    }
                }
            }
        }
    }
}

fn cleanup_discovery_file(path: &Path) {
    if let Err(error) = std::fs::remove_file(path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                error = %error,
                path = %path.display(),
                "daemon: could not remove the discovery file on shutdown"
            );
        }
    }
}

#[cfg(test)]
mod discovery_watch_tests {
    use super::*;
    use narsor_engine::persistence::PersistenceError;
    use std::io;
    use std::path::PathBuf;

    fn io_error(kind: io::ErrorKind) -> PersistenceError {
        PersistenceError::Io {
            path: PathBuf::from("daemon.json"),
            source: io::Error::from(kind),
        }
    }

    #[test]
    fn a_missing_file_is_written_again() {
        let read = DiscoveryRead::from_result(Err(io_error(io::ErrorKind::NotFound)));
        assert_eq!(read, DiscoveryRead::Missing);
        assert!(read.needs_rewrite(10));
    }

    #[test]
    fn a_file_with_another_pid_is_written_again() {
        assert!(DiscoveryRead::Pid(11).needs_rewrite(10));
    }

    #[test]
    fn a_file_with_its_own_pid_is_left_alone() {
        assert!(!DiscoveryRead::Pid(10).needs_rewrite(10));
    }

    #[test]
    fn a_read_failure_other_than_not_found_is_left_alone() {
        let read = DiscoveryRead::from_result(Err(io_error(io::ErrorKind::PermissionDenied)));
        assert_eq!(read, DiscoveryRead::Unreadable);
        assert!(!read.needs_rewrite(10));
    }
}
