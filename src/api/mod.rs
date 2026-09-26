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
mod routes;

pub use client::{ApiClient, ClientError};
pub use routes::ConversationResponse;

use crate::persistence::ExecutionStore;
use std::fmt;
use std::net::SocketAddr;
use tokio::net::TcpListener;
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
/// Holds the [`JoinHandle`] of the task serving requests: dropping this
/// handle aborts that task, so the server's lifetime is exactly the
/// lifetime of whatever holds the handle (the TUI's `App`, in U7). There is
/// no separate `stop` method because there is no graceful shutdown to speak
/// of — every request is a read, so aborting mid-request loses nothing but
/// that one in-flight response.
pub struct ApiHandle {
    addr: SocketAddr,
    token: String,
    task: JoinHandle<()>,
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
}

impl Drop for ApiHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Starts the embedded API server over `store`, on a random loopback port.
///
/// The returned [`ApiHandle`] carries the bound address and the token every
/// request must present as `Authorization: Bearer <token>`.
pub async fn start(store: ExecutionStore) -> Result<ApiHandle, ApiStartError> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(ApiStartError)?;
    let addr = listener.local_addr().map_err(ApiStartError)?;
    let token = Uuid::new_v4().to_string();

    let app = routes::router(store, token.clone());
    let task = tokio::spawn(async move {
        // A read-only server has no state to save on the way out; letting
        // the future simply end (on abort, or if `axum::serve` itself ever
        // returns) needs no handling beyond dropping the listener.
        let _ = axum::serve(listener, app).await;
    });

    Ok(ApiHandle { addr, token, task })
}
