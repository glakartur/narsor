//! The daemon process lifecycle (U2).
//!
//! # Why recovery moved here
//!
//! `persistence::recovery::recover` (D23) used to run at the top of
//! `tui::mod::run`, because the TUI was, until this unit, the process that
//! opened the store for write. Now that role belongs to the daemon (R1, R8):
//! whichever process holds the store when it starts is the one responsible
//! for reconciling whatever a previous, differently-killed holder left
//! behind, and after this unit that is always the daemon, never the TUI.
//!
//! # Control-plane recovery
//!
//! The remote-task control plane (`narsor_engine::orchestration`) keeps its own
//! records beside the executions; [`recover_orchestration_at_startup`]
//! repairs them right after local recovery and before the API — whose
//! control-plane routes this daemon serves — accepts anything.
//!
//! # The startup hook
//!
//! Right after recovery, and before the API accepts anything,
//! [`continue_at_startup`] carries on what recovery found safely stopped
//! (D55's restart matrix): a reprompt a restart cut short between the
//! rejection and its start gets a new pass, so does a label rescue (F1) the
//! restart cut short, the conductor serves its unanswered chat messages
//! again, and diagnoses again a failed step (F3) it had not concluded about.
//! It is the daemon's only replay, and it only ever continues what cannot
//! repeat a side effect.
//!
//! # Shutdown during the conductor's label rescue
//!
//! The rescue (F1) runs inline in a pass, and a pause is honoured only at
//! the next step boundary — so a `SIGTERM` during the conductor's
//! invocation does not stop it early: the drain below waits for it like
//! for any running action, up to its timeout, and the process then exits
//! anyway, taking the invocation with it. That is deliberate. The rescue is
//! recorded before the conductor is invoked and the invocation is counted
//! before it runs, so the next start repeats it (R24's one repeat) instead
//! of losing it; interrupting it earlier would only spend that repeat
//! sooner.
//!
//! # Why shutdown is a poll, not a wait
//!
//! `Engine::pause` (D34) only ever takes effect at the next step boundary —
//! it does not interrupt an action already running. So there is no single
//! future to `.await` for "every active run has stopped"; the shutdown path
//! asks each one to pause and then polls [`Engine::active_run_ids`] on a
//! short interval until it is empty or a bounded timeout elapses. Exceeding
//! the timeout is not treated as failure to shut down: the process exits
//! anyway, and whatever was interrupted mid-action is exactly what
//! `persistence::recovery::recover` is for, at the next start (AE6).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use tokio::signal::unix::{signal, SignalKind};

use crate::api::{ActiveTasks, ApiHandle, ApiServices, ServerSecurity};
use crate::auth_gate::NetworkAuth;
use crate::local_controller::{controller_program, LocalController, LocalControllerSettings};
use narsor_engine::auth::{
    OidcVerifier, OwnerStore, SecurityPlan, SecuritySettings, StartupOwnership,
};
use narsor_engine::conductor::assembly::{assemble, restore_at_startup};
use narsor_engine::conductor::{ConductorRegistry, HarnessInvoker, PassLauncher};
use narsor_engine::config::Resolved;
use narsor_engine::controller_protocol::{AutoResumePass, ProtocolService};
use narsor_engine::domain::{ExecutionStatus, WorkflowDefinition, WorkflowExecution};
use narsor_engine::engine::{owed_output_rescue, owed_reprompt, Engine, HarnessRegistry};
use narsor_engine::environment::{
    BackendKind, BackendRecord, CatalogLaunchResolver, EnvironmentCatalog, HarnessDefaults,
    OwnerDefaults,
};
use narsor_engine::orchestration::ControlPlane;
use narsor_engine::persistence::{
    DaemonInfo, DefaultHarnessSelector, ExecutionStore, ReconciliationOutcome, RecoveryReport,
};
use narsor_engine::workflows::WorkflowResolver;

use narsor_shared::cli_error::{CliError, Result};

/// How long the daemon waits, after asking every active run to pause, for
/// [`Engine::active_run_ids`] to empty before giving up and exiting anyway.
///
/// The plan leaves the exact figure to be settled empirically at
/// implementation time (U2's Open Questions) rather than fixed by the
/// architecture; a handful of seconds is a reasonable starting point for a
/// single local operator's workflows, comfortably longer than one step
/// boundary and short enough that `daemon stop` still feels responsive.
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the drain loop re-checks [`Engine::active_run_ids`].
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

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
/// process exiting closes every remaining connection anyway; the *engine's*
/// own runs are already safely drained by [`drain_active_runs`] before this
/// ever runs, so nothing about workflow correctness depends on this
/// finishing cleanly, only on it not hanging forever.
const HTTP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// `narsor daemon` — the foreground entry point: recovery, bind, discovery
/// file, serve until `SIGTERM`, graceful shutdown.
///
/// Runs until it receives `SIGTERM` (from `narsor-server stop`, or any other
/// sender — `kill -TERM <pid>` works too, deliberately, since the discovery
/// file already publishes the PID). Never returns early on its own (R3):
/// the only way out is that signal.
pub async fn run_daemon(config: &Resolved) -> Result<()> {
    let paths = narsor_engine::persistence::StatePaths::new(config.state_dir());
    // Before anything is touched: a security configuration that does not
    // hold up must stop the daemon, not leave it half started (U7).
    let security = server_security(config, &paths)?;
    let runs_local_controller = security.runs_local_controller();
    let store = ExecutionStore::new(config.state_dir());

    let recovery = recover_at_startup(&store);
    let control_plane = recover_orchestration_at_startup(ControlPlane::new(paths.clone()));

    if runs_local_controller {
        ensure_local_backend(config.catalog_dir())?;
    } else {
        refuse_declared_local_backend(config.catalog_dir())?;
    }
    let defaults = config.agent_defaults();
    let supported = HarnessRegistry::defaults()
        .supported()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let selector = Arc::new(
        DefaultHarnessSelector::load(paths.clone(), supported, defaults.harness.clone())
            .await
            .map_err(|error| {
                CliError::failure(format!("could not load default harness selection: {error}"))
            })?,
    );
    // Tasks resolve against the same catalogue and start on the same live
    // default harness a local harnessless step would use; their runners
    // follow it afterwards, through the control plane's answers (U7).
    let owner_defaults = OwnerDefaults::from_config(config);
    if !runs_local_controller && owner_defaults.default_backend == LOCAL_BACKEND_ID {
        tracing::warn!(
            "daemon: default_backend is '{LOCAL_BACKEND_ID}', which a network server does not \
             have; a task that names no backend is refused until default_backend names another \
             backend"
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
    let control = Arc::new(ProtocolService::new(control_plane, launch.clone()));
    let _scheduler = spawn_periodic_admission(Arc::clone(&control));
    let agent_timeout_secs = defaults.timeout_secs;
    let engine = Arc::new(
        Engine::new(store.clone())
            .with_agent_timeout(defaults.timeout_secs)
            .with_default_harness_source(selector.clone()),
    );

    // One registry of background passes for the API and the startup hook,
    // so a pass the hook spawns can be interrupted like any other.
    let active_tasks = ActiveTasks::new();
    // The conductor runs on the same live default harness as a harnessless
    // agent step, read-only (KTD5), bounded by the agent timeout.
    let invoker = HarnessInvoker::new(
        store.clone(),
        config.catalog_dir().to_path_buf(),
        config.work_dir().to_path_buf(),
    )
    .with_default_harness_source(selector.clone())
    .with_agent_timeout(agent_timeout_secs);
    // Held here for the daemon's whole life: the engine only holds it
    // weakly, and hands a step without a usable output label to it (F1).
    // A pass the conductor starts — its one retry of a failed step (F3) —
    // is driven like one the API starts, and interrupted the same way.
    let conductor = assemble(
        &engine,
        Arc::new(invoker),
        Some(Arc::new(DaemonPasses {
            store: store.clone(),
            catalog_dir: config.catalog_dir().to_path_buf(),
            active_tasks: active_tasks.clone(),
        })),
    );
    if let Some(recovery) = &recovery {
        continue_at_startup(
            &engine,
            &conductor,
            &active_tasks,
            config.catalog_dir(),
            recovery,
        );
    }

    // The engine and API share one selector, so a confirmed setting affects
    // later harnessless attempts without restarting the daemon.
    let api_handle = crate::api::serve(
        Arc::clone(&engine),
        config.catalog_dir().to_path_buf(),
        config.work_dir().to_path_buf(),
        ApiServices {
            harness_selector: Some(selector),
            active_tasks,
            conductor: Some(Arc::clone(&conductor)),
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
    if let Err(error) = info.write(&paths.daemon_file()) {
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
    terminate.recv().await;
    tracing::info!("daemon: received SIGTERM, shutting down");

    // Stop the local controller while the API still accepts its signed
    // lease release (R8). A successor can then acquire the lease
    // immediately rather than waiting for expiry.
    if let Some(local_controller) = local_controller {
        local_controller.stop().await;
    }

    // Stop accepting new connections *before* draining active runs, not
    // after: `Engine::active_run_ids()` is a snapshot the instant
    // `drain_active_runs` takes it, and a fresh `POST /tasks`/`/resume`/
    // `/input` landing anywhere in the drain window would start a new
    // background run this snapshot never asked to pause — silently
    // defeating the whole point of draining. An in-flight request that
    // arrived a moment before this call still completes normally
    // (`stop_accepting` only refuses *new* connections); everything after
    // this point is refused at the TCP level before it ever reaches a
    // handler.
    api_handle.stop_accepting();

    if !drain_active_runs(&engine, SHUTDOWN_DRAIN_TIMEOUT).await {
        tracing::warn!(
            "daemon: active runs did not drain within the shutdown timeout; exiting anyway \
             (D23-recovery will reconcile them at the next start)"
        );
    }

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
    cleanup_discovery_file(&paths.daemon_file());

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

/// What the daemon hands its local controller (KTD3): the paths and
/// launch settings it resolved, the controller's storage and credential
/// where the in-process controller kept them (R10).
fn local_controller_settings(
    config: &Resolved,
    api: &ApiHandle,
) -> Result<LocalControllerSettings> {
    let program = controller_program().map_err(CliError::failure)?;
    Ok(LocalControllerSettings {
        program,
        control_plane: format!("http://{}", api.addr()),
        credential_file: config
            .state_dir()
            .join("local-controller")
            .join("credential"),
        storage: config.work_dir().join("local-controller"),
        // Every runner the controller starts reads it anew: a launch
        // setting of the backend, not part of the task.
        harness_args: config.local_harness_home().runner_args(),
        log_directive: config.log_directive(),
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
/// their attempt is due, and task materials whose time to live ran out are
/// swept (U9), on the same tick. The handle is dropped with the daemon's
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
/// time to live ran out (U9), and the reader leases that ran idle (D63). Returns what the resume policy did.
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
    resumed
}

/// The server's security mode from the configuration (U7), or a failure
/// naming every problem.
///
/// In network mode this also opens the installation's ownership records:
/// with no owner configured or recorded, a fresh one-time claim code is
/// printed to stderr (the daemon's log in a container) and stored `0600`
/// beside them for the operator.
fn server_security(
    config: &Resolved,
    paths: &narsor_engine::persistence::StatePaths,
) -> Result<ServerSecurity> {
    let plan = SecuritySettings::from_config(config)
        .validate()
        .map_err(|error| CliError::failure(error.to_string()))?;
    let network = match plan {
        SecurityPlan::Trusted { bind, cors } => return Ok(ServerSecurity::Trusted { bind, cors }),
        SecurityPlan::Network(network) => network,
    };
    let issuer = network.oidc.issuer.clone();
    let verifier = OidcVerifier::new(network.oidc)
        .map_err(|error| CliError::failure(format!("oidc_issuer: {error}")))?;
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

/// Reconciles every stored execution against its event log (D23) — moved
/// here, verbatim in behaviour, from `tui::mod::run`. Best-effort: a failure
/// here must not stop the daemon from starting, and leaves no report for
/// the startup hook to act on.
fn recover_at_startup(store: &ExecutionStore) -> Option<RecoveryReport> {
    match narsor_engine::persistence::recovery::recover(store) {
        Ok(report) => {
            for reconciliation in report.needing_attention() {
                tracing::warn!(
                    execution_id = %reconciliation.execution_id,
                    "daemon: startup recovery found an execution left running with an \
                     interrupted action; it was marked Unknown and needs attention"
                );
            }
            Some(report)
        }
        Err(error) => {
            tracing::warn!(error = %error, "daemon: startup recovery sweep failed");
            None
        }
    }
}

/// Repairs the control plane's own records (U2) — orphaned temporary files
/// and events a crash kept out of a task's log — before the API serves
/// them, so a stream replay never skips one. Unlike local recovery it judges
/// no task: a remote task's runner state changes only through its controller's
/// reconciliation, never because this process restarted. Best-effort like
/// [`recover_at_startup`]: a failure is logged and the daemon still starts.
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
    control
}

/// The daemon's startup hook (D55): carries on, after `recover`, what the
/// restart matrix says may be carried on. Best-effort per execution, like
/// recovery itself.
///
/// * `Running`, consistent, with a rejected output still owed its reprompt
///   (KTD7): the attempt is closed, so a new pass — spawned exactly as
///   `POST /executions/:id/resume` spawns one — asks the agent again instead
///   of replaying anything.
/// * `Running` with a label rescue still owed (F1): a new pass carries it on
///   — it invokes the conductor, or applies a decision the conductor saved
///   before the restart without invoking it. When the conductor's invocation
///   was under way (its marker is left, `ConductorInterrupted`), invoking
///   it again is R24's one repeat, counted before it runs.
/// * The conductor's own rows: unanswered chat messages and interrupted
///   advisory invocations are served again, and what a rescue left for an
///   execution that no longer owes one is discarded
///   ([`restore_at_startup`]).
/// * `Failed` with a step failure (F3) the conductor had not concluded
///   about: it is diagnosed again — R24's one repeat when the restart cut
///   the invocation short (also [`restore_at_startup`]).
pub fn continue_at_startup(
    engine: &Arc<Engine>,
    conductor: &Arc<ConductorRegistry>,
    active_tasks: &ActiveTasks,
    catalog_dir: &Path,
    recovery: &RecoveryReport,
) {
    for reconciliation in &recovery.reconciled {
        if reconciliation.status != ExecutionStatus::Running {
            continue;
        }
        match &reconciliation.outcome {
            ReconciliationOutcome::Consistent => {
                continue_owed_step(
                    engine,
                    active_tasks,
                    catalog_dir,
                    &reconciliation.execution_id,
                );
            }
            ReconciliationOutcome::ConductorInterrupted { trigger, .. } => {
                tracing::info!(
                    execution_id = %reconciliation.execution_id,
                    trigger = trigger.name(),
                    "daemon: the conductor was interrupted while it worked on this \
                     execution; invoking it again"
                );
                continue_owed_step(
                    engine,
                    active_tasks,
                    catalog_dir,
                    &reconciliation.execution_id,
                );
            }
            _ => {}
        }
    }

    restore_at_startup(conductor);
}

/// The daemon's [`PassLauncher`]: a pass the conductor starts resolves its
/// definition as the API does and runs in the registry of background
/// passes, so `POST /executions/:id/interrupt` stops it like any other.
struct DaemonPasses {
    store: ExecutionStore,
    catalog_dir: PathBuf,
    active_tasks: ActiveTasks,
}

impl PassLauncher for DaemonPasses {
    fn definition(
        &self,
        execution: &WorkflowExecution,
    ) -> std::result::Result<WorkflowDefinition, String> {
        narsor_engine::tasks::resolve_definition(&self.catalog_dir, self.store.paths(), execution)
            .map_err(|error| error.to_string())?
            .load()
            .map_err(|error| error.to_string())
    }

    fn launch(&self, execution_id: &str, pass: Pin<Box<dyn Future<Output = ()> + Send>>) {
        self.active_tasks.spawn(execution_id.to_string(), pass);
    }
}

/// Spawns a pass for `execution_id` when it stopped between a rejected
/// output and its reprompt, or during the conductor's rescue of its output
/// label, and logs what happened.
fn continue_owed_step(
    engine: &Arc<Engine>,
    active_tasks: &ActiveTasks,
    catalog_dir: &Path,
    execution_id: &str,
) {
    match spawn_owed_step(engine, active_tasks, catalog_dir, execution_id) {
        Ok(true) => tracing::info!(
            execution_id,
            "daemon: continuing a step the restart interrupted"
        ),
        Ok(false) => {}
        Err(error) => tracing::warn!(
            execution_id,
            error = %error,
            "daemon: could not continue a step the restart interrupted"
        ),
    }
}

/// Whether `execution_id` owes a reprompt or a label rescue — and if it
/// does, claims it and spawns its pass.
fn spawn_owed_step(
    engine: &Arc<Engine>,
    active_tasks: &ActiveTasks,
    catalog_dir: &Path,
    execution_id: &str,
) -> std::result::Result<bool, String> {
    let execution = engine
        .store()
        .load_execution(execution_id)
        .map_err(|error| error.to_string())?;
    let owes_reprompt = owed_reprompt(&execution)
        .map_err(|error| error.to_string())?
        .is_some();
    let owes_rescue = owed_output_rescue(&execution)
        .map_err(|error| error.to_string())?
        .is_some();
    if !owes_reprompt && !owes_rescue {
        return Ok(false);
    }
    let definition =
        narsor_engine::tasks::resolve_definition(catalog_dir, engine.store().paths(), &execution)
            .map_err(|error| error.to_string())?
            .load()
            .map_err(|error| error.to_string())?;
    let handle = engine
        .try_begin_resume(execution_id)
        .map_err(|error| error.to_string())?;
    active_tasks.spawn(execution_id.to_string(), async move {
        let _ = handle.run_to_completion(&definition).await;
    });
    Ok(true)
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

/// Asks every execution a pass is currently driving to pause (D34), then
/// waits — bounded by `timeout` — for [`Engine::active_run_ids`] to empty.
///
/// Returns `true` when everything drained in time, `false` when the timeout
/// elapsed first. A free function taking `&Engine` rather than a method on
/// `Engine` itself: the polling/timeout policy is a daemon lifecycle
/// concern (U2), not something every engine consumer needs — and keeping it
/// here lets `tests/daemon_lifecycle.rs` exercise exactly this logic
/// in-process, against a real `Engine` driving a real long-running action,
/// without needing a real `SIGTERM`/subprocess round trip for every
/// assertion about it.
pub async fn drain_active_runs(engine: &Engine, timeout: Duration) -> bool {
    for execution_id in engine.active_run_ids() {
        engine.pause(&execution_id);
    }

    let deadline = Instant::now() + timeout;
    loop {
        if engine.active_run_ids().is_empty() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(SHUTDOWN_POLL_INTERVAL).await;
    }
}
