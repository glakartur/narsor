//! `narsor daemon` and `narsor daemon stop` (U2).
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

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use tokio::signal::unix::{signal, SignalKind};

use crate::config::Resolved;
use crate::engine::{Engine, HarnessRegistry};
use crate::persistence::{DaemonInfo, DefaultHarnessSelector, ExecutionStore};

use super::commands::{CliError, Result};

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
/// Runs until it receives `SIGTERM` (from [`stop_daemon`], or any other
/// sender — `kill -TERM <pid>` works too, deliberately, since the discovery
/// file already publishes the PID). Never returns early on its own (R3):
/// the only way out is that signal.
pub async fn run_daemon(config: &Resolved) -> Result<()> {
    let paths = config.state_paths();
    let store = ExecutionStore::new(config.state_dir());

    recover_at_startup(&store);

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
    let engine =
        Arc::new(Engine::new(store.clone()).with_agent_defaults_and_selector(defaults, selector));

    // The router now also carries the mutating endpoints U3 adds (SSE is
    // still U4, out of scope here); it drives on `engine` directly, sharing
    // the one `Arc<Engine>` this daemon owns. Binding and owning this server
    // is the daemon's job (R1); a failure to bind is fatal and must leave no
    // discovery file behind (checked by `tests/daemon_lifecycle.rs`).
    let api_handle = crate::api::start(
        Arc::clone(&engine),
        config.catalog_dir().to_path_buf(),
        config.work_dir().to_path_buf(),
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
    info.write(&paths.daemon_file()).map_err(|error| {
        CliError::failure(format!(
            "could not write the daemon discovery file: {error}"
        ))
    })?;
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

/// Reconciles every stored execution against its event log (D23) — moved
/// here, verbatim in behaviour, from `tui::mod::run`. Best-effort: a failure
/// here must not stop the daemon from starting.
fn recover_at_startup(store: &ExecutionStore) {
    match crate::persistence::recovery::recover(store) {
        Ok(report) => {
            for reconciliation in report.needing_attention() {
                tracing::warn!(
                    execution_id = %reconciliation.execution_id,
                    "daemon: startup recovery found an execution left running with an \
                     interrupted action; it was marked Unknown and needs attention"
                );
            }
        }
        Err(error) => {
            tracing::warn!(error = %error, "daemon: startup recovery sweep failed");
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

/// `narsor daemon stop` — reads the discovery file and sends `SIGTERM` to
/// the PID it names, or reports that no daemon is running and cleans up a
/// stale entry.
///
/// Deliberately synchronous and near-instant: it only sends a signal and
/// does not wait for the daemon to actually finish shutting down (there is
/// no REST endpoint for this and no reason to add one — see the plan's Key
/// Technical Decisions). An operator who wants to confirm the daemon is
/// gone re-runs `daemon stop` or checks for the discovery file themselves.
pub fn stop_daemon(config: &Resolved) -> Result<()> {
    let paths = config.state_paths();
    let daemon_file = paths.daemon_file();

    let info = match DaemonInfo::read(&daemon_file) {
        Ok(info) => info,
        Err(_) => {
            println!("No daemon is running.");
            return Err(CliError::failure("no daemon is running"));
        }
    };

    if !info.is_alive() {
        println!(
            "No daemon is running (stale discovery file for pid {}); cleaning up.",
            info.pid
        );
        let _ = std::fs::remove_file(&daemon_file);
        return Err(CliError::failure("no daemon is running"));
    }

    // SAFETY: `kill` sends a plain signal to an existing PID; it is not
    // representable in safe Rust only because it is a raw syscall, not
    // because it does anything unsound here.
    let sent = unsafe { libc::kill(info.pid as libc::pid_t, libc::SIGTERM) };
    if sent != 0 {
        let error = std::io::Error::last_os_error();
        return Err(CliError::failure(format!(
            "could not signal daemon (pid {}): {error}",
            info.pid
        )));
    }

    println!("Sent SIGTERM to daemon (pid {}).", info.pid);
    Ok(())
}
