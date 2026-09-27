//! The embedded, read-only API server (U5).
//!
//! # Why an API at all, when the TUI already has the store on disk
//!
//! Every reader before this unit — the TUI, the CLI — has opened
//! [`crate::persistence::ExecutionStore`] directly, in-process, against the
//! local filesystem. That stops working the moment a task view wants to read
//! a *step timeline* or a *conversation*: those are computed
//! ([`crate::engine::build_timeline`]) or transparently decompressed
//! ([`crate::persistence::ConversationStore::read`]), not just deserialized,
//! and M2's distributed narsor (`docs/brainstorms/2026-09-25-…`) wants that
//! same computed view available to a caller that is *not* the process
//! holding the filesystem. An HTTP boundary drawn now, even inside the TUI's
//! own process, is the boundary that later moves to a standalone `narsor
//! serve` (M2) without the TUI or CLI changing how they ask for a timeline or
//! a conversation — only where the answer comes from changes.
//!
//! # Why embedded in the TUI process for now, not `narsor serve`
//!
//! A standalone server process, a Unix socket, and caller identity (R29 in
//! the distributed-narsor brainstorm) are all M2 concerns this plan
//! deliberately does not build: see the plan's "Deferred to Follow-Up Work".
//! For M1, the only consumer is the TUI that is about to open a task view in
//! the same process, so the server is started (and stopped, implicitly, on
//! `ApiHandle` drop) as part of that process's lifetime. This also means the
//! embedded server never has to survive the TUI exiting — there is nothing
//! to leak.
//!
//! # Why loopback-only and why a random token
//!
//! The conversation store this API exposes can contain secrets from tool
//! results (the plan's plan-local scope: "we do not filter secrets from tool
//! results"). `127.0.0.1` keeps the socket off any network interface, but a
//! loopback port is still visible to every other local user on a shared
//! machine — hence the bearer token, generated fresh per process with no
//! persistence and no configuration surface. There is nothing to leak if the
//! process never writes the token to disk, and nothing to guess since it is
//! a random UUID valid only for this process's lifetime.
//!
//! # Why read-only, and why that means no `Engine::is_active` check
//!
//! `docs/plans/2026-09-26-001-fix-tui-background-engine-runs-plan.md`
//! established that anything which *drives* an execution — runs it, resumes
//! it, delivers input to it — must go through `spawn_background` and check
//! `Engine::is_active` first, because two independent drivers racing to
//! advance the same execution is exactly the bug that plan fixed. This
//! server does not drive anything: every handler here only reads
//! `ExecutionStore` and `ConversationStore`, both already safe for
//! concurrent read access from multiple processes (that is the whole point
//! of a plain-file, append-only, atomic-rename store). The "list executions,
//! run/input/transition, live event stream over the API" surface — the one
//! that *would* need that check — is explicitly out of scope until M2 (REST
//! and SSE, D37/D38); this unit only moves read paths the TUI already has
//! onto HTTP.
//!
//! # Bind address and startup failure
//!
//! The server binds `127.0.0.1:0` — an OS-assigned port — so that several
//! narsor TUI processes on one machine never collide, and so tests can run
//! this server many times over without any port coordination. If the bind
//! itself fails (no loopback interface, exhausted file descriptors, ...) the
//! plan is explicit: the TUI must not start silently degraded. [`start`]
//! returns an error the caller is expected to treat as fatal.

mod client;
mod discovery;
pub mod dto;
mod routes;

pub use client::{ApiClient, ClientError};
pub use discovery::{ensure_daemon_running, DiscoveryError};
pub use dto::{
    AcceptedResponse, ExecutionListResponse, ExecutionSummary, HarnessSelectionSnapshot,
    RequestErrorResponse, SetHarnessSelectionRequest, StartTaskRequest, StartTaskResponse,
    TransitionRequest,
};
pub use routes::{ConversationEntryDto, ConversationResponse};

use crate::engine::Engine;
use crate::persistence::DefaultHarnessSelector;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Failure to start the embedded API server.
#[derive(Debug)]
pub struct ApiStartError(std::io::Error);

impl fmt::Display for ApiStartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "could not start the embedded API server: {}", self.0)
    }
}

impl std::error::Error for ApiStartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// A running embedded API server.
///
/// Holds the [`JoinHandle`] of the task serving requests and a channel to
/// signal it to stop. Unlike the earlier, embedded-in-the-TUI shape (U5),
/// this server is now owned by the daemon process (U2) and may outlive any
/// one client — so it is no longer torn down implicitly when a handle is
/// dropped (R10). Callers that want the server to stop must call
/// [`ApiHandle::shutdown`] explicitly; simply dropping the handle leaves the
/// server running, exactly like every other resource that isn't RAII-scoped
/// to one client's lifetime.
pub struct ApiHandle {
    addr: SocketAddr,
    token: String,
    task: JoinHandle<()>,
    shutdown: Arc<Notify>,
}

impl ApiHandle {
    /// The address the server is listening on (`127.0.0.1:<port>`).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The bearer token every request must present.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// A client already configured to talk to this server.
    pub fn client(&self) -> ApiClient {
        ApiClient::new(self.addr, self.token.clone())
    }

    /// Gracefully stops the server: in-flight requests are allowed to finish
    /// ([`axum::serve`]'s `with_graceful_shutdown`), then this returns once
    /// the serving task has actually ended.
    ///
    /// This is [`ApiHandle::stop_accepting`] followed by
    /// [`ApiHandle::wait_drained`] — kept as one call for a caller that
    /// doesn't need anything to happen *between* the two (every caller
    /// before the daemon's own shutdown sequence needed only this). A
    /// caller that needs new connections to stop being accepted before some
    /// other work runs (like `narsor daemon`'s own active-run drain — see
    /// `src/cli/daemon.rs::run_daemon`) should call the two steps
    /// separately instead of this one.
    pub async fn shutdown(self) {
        self.stop_accepting();
        self.wait_drained().await;
    }

    /// Stops the server from accepting any *new* connection, immediately —
    /// wakes [`axum::serve`]'s `with_graceful_shutdown` future without
    /// waiting for anything.
    ///
    /// A [`tokio::sync::Notify`], not a `oneshot`: the signal has to be
    /// sendable through `&self` (this method takes a borrow, not `self` by
    /// value), since the caller still needs the handle afterwards to call
    /// [`ApiHandle::wait_drained`] — `oneshot::Sender::send` consumes the
    /// sender and cannot be cloned, so it cannot fire from a shared
    /// reference. `notify_one` is a no-op if the graceful-shutdown future is
    /// not yet awaiting it (a permit is stored and consumed on its next
    /// `.notified().await` instead), so calling this exactly once, before
    /// anything else, is sufficient regardless of scheduling.
    ///
    /// Existing connections (in-flight requests, open SSE streams) are left
    /// running; [`ApiHandle::wait_drained`] is what waits for those. Calling
    /// this *before* any other shutdown-time work — rather than only at the
    /// end, as the pre-U8 single [`ApiHandle::shutdown`] call did — is what
    /// closes the window where a fresh mutating request (`POST /tasks`,
    /// `/resume`, `/input`) could still land and start a new background run
    /// after the daemon has already decided it is shutting down and started
    /// waiting for [`crate::engine::Engine::active_run_ids`] to drain.
    pub fn stop_accepting(&self) {
        self.shutdown.notify_one();
    }

    /// Waits for the serving task to actually end — every connection has
    /// either finished on its own or [`ApiHandle::stop_accepting`] already
    /// asked it to stop accepting new ones.
    pub async fn wait_drained(self) {
        let _ = self.task.await;
    }
}

/// Starts the embedded API server over `engine`, on a random loopback port.
///
/// `catalog_dir`/`work_dir` are the daemon's own catalogue and work roots
/// (U3): `POST /tasks` needs them to resolve a workflow reference and
/// provision a workspace exactly as `narsor run` does, now server-side.
///
/// The returned [`ApiHandle`] carries the bound address and the token every
/// request must present as `Authorization: Bearer <token>`.
pub async fn start(
    engine: Arc<Engine>,
    catalog_dir: PathBuf,
    work_dir: PathBuf,
) -> Result<ApiHandle, ApiStartError> {
    start_with_harness_selector(engine, catalog_dir, work_dir, None).await
}

/// Starts the API server with the daemon's shared default-harness selector.
///
/// The compatibility [`start`] entry point remains useful to older embedded
/// API tests that do not compose an agent selector. Daemons must call this
/// function so the settings resource and executor observe the same `Arc`.
pub async fn start_with_harness_selector(
    engine: Arc<Engine>,
    catalog_dir: PathBuf,
    work_dir: PathBuf,
    harness_selector: Option<Arc<DefaultHarnessSelector>>,
) -> Result<ApiHandle, ApiStartError> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(ApiStartError)?;
    let addr = listener.local_addr().map_err(ApiStartError)?;
    let token = Uuid::new_v4().to_string();

    let store = engine.store().clone();
    let app = routes::router(
        engine,
        store,
        token.clone(),
        catalog_dir,
        work_dir,
        harness_selector,
    );
    let shutdown = Arc::new(Notify::new());
    let shutdown_wait = Arc::clone(&shutdown);
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown_wait.notified().await;
            })
            .await;
    });

    Ok(ApiHandle {
        addr,
        token,
        task,
        shutdown,
    })
}
