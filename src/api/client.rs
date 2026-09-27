//! Typed HTTP client for the embedded API (U5), the one thing U7's task view
//! is meant to depend on for events, the timeline and conversations.
//!
//! # Why a distinct "API unavailable" error
//!
//! The TUI's task view (U7) reads by calling this client on every manual
//! refresh (R8: no auto-refresh) — a request can fail for reasons that call
//! for very different messages to the operator: the *server* is simply gone
//! (the loopback port refused the connection, or the request timed out —
//! [`ClientError::Unavailable`]) versus the *answer* being a well-formed
//! `401` or `404` from a server that is very much up. Collapsing all of
//! that into one generic "request failed" would make "is my API server even
//! running" indistinguishable from "this execution id is wrong", which is
//! exactly the distinction an operator needs when the TUI is the process
//! that started the server in the first place.
//!
//! # Why a timeout
//!
//! The server only ever reads local files, so a slow response almost never
//! means "give it more time" — it means the process is gone, wedged, or the
//! wrong port. A bounded timeout turns that into a prompt, readable
//! [`ClientError::Unavailable`] instead of a TUI that appears to hang.
//!
//! The bound is short (not the generous timeout a client talking to a real
//! network service would want) because every call this client makes is
//! awaited directly inside the TUI's key-handling loop
//! (`crate::tui::run`'s `select!`), so the timeout is also the worst-case
//! delay before the interface redraws or responds to another key — including
//! quit. A loopback, same-process, read-only server has no legitimate reason
//! to take seconds to answer; if it ever does, failing fast matters more
//! here than it would for a client with no such coupling to input latency.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use futures::future::FutureExt as _;
use futures::stream::{self, Stream, StreamExt as _};
use reqwest::StatusCode;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;

use crate::domain::{Event, EventData};
use crate::engine::{HumanInputDelivery, Timeline};

use super::dto::{
    AcceptedResponse, ExecutionListResponse, ExecutionSummary, HarnessSelectionSnapshot,
    RequestErrorResponse, SetHarnessSelectionRequest, StartTaskRequest, StartTaskResponse,
    TransitionRequest,
};
use super::routes::ConversationResponse;

/// How long a single request may take before it is treated as the server
/// being unavailable — and, since every call is awaited on the TUI's
/// key-handling path, the worst-case stall an operator can see from a single
/// slow request. See the module doc for why this is short rather than
/// generous.
const REQUEST_TIMEOUT: Duration = Duration::from_millis(800);

/// A distinguishable failure of a client call.
#[derive(Debug)]
pub enum ClientError {
    /// The request never got a well-formed answer at all: connection
    /// refused, timed out, or some other transport-level failure. This is
    /// the "API unavailable" case the plan calls for — distinct from a
    /// `401`/`404` that a live server chose to send.
    Unavailable(String),
    /// The token was missing or wrong.
    Unauthorized,
    /// The execution (or, for conversations, the enclosing execution) is
    /// not known to the server.
    NotFound,
    /// The request is well-formed but conflicts with the execution's current
    /// state (`409`) — for instance a `resume`/`input`/`transition` racing
    /// another driver of the same execution, or a hand-written transition
    /// while a background pass is active. Distinct from [`ClientError::Server`]
    /// so a caller (the CLI, the TUI) can tell "try again, something else is
    /// driving this" from "the server broke".
    Conflict,
    /// The server rejected a validly decoded request and supplied a safe,
    /// typed reason suitable for an operator-facing UI.
    Request { code: String, message: String },
    /// The server answered with an unexpected status or a body this client
    /// could not parse.
    Server(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Unavailable(detail) => write!(f, "API unavailable: {detail}"),
            ClientError::Unauthorized => write!(f, "API rejected the request: bad token"),
            ClientError::NotFound => write!(f, "API: not found"),
            ClientError::Conflict => write!(f, "API: conflict (409)"),
            ClientError::Request { code, message } => {
                write!(f, "API request rejected ({code}): {message}")
            }
            ClientError::Server(detail) => write!(f, "API error: {detail}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// A client bound to one running server, over `http://127.0.0.1:<port>`.
pub struct ApiClient {
    base_url: String,
    token: String,
    http: reqwest::Client,
    /// A second client, deliberately built with no request timeout, used
    /// only by [`ApiClient::stream_execution`]/[`ApiClient::stream_all`].
    ///
    /// [`REQUEST_TIMEOUT`] exists because every other call this client makes
    /// is a bounded, one-shot read/write against a loopback server that
    /// never legitimately takes long to answer. An SSE connection is the
    /// opposite by design: it is meant to stay open, silent but for
    /// keep-alives, for as long as the execution runs — which can be far
    /// longer than 800ms and is not a sign anything is wrong. Reusing `http`
    /// here would have `reqwest` tear the connection down mid-stream the
    /// moment an execution paused for a human gate.
    stream_http: reqwest::Client,
}

impl ApiClient {
    /// Builds a client for the server at `addr`, presenting `token` on every
    /// request.
    pub fn new(addr: SocketAddr, token: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("reqwest client with only a timeout configured never fails to build");
        let stream_http = reqwest::Client::builder()
            .build()
            .expect("reqwest client with no special configuration never fails to build");
        Self {
            base_url: format!("http://{addr}"),
            token: token.into(),
            http,
            stream_http,
        }
    }

    /// Events of `execution_id` with `sequence > after`.
    pub async fn events(&self, execution_id: &str, after: u64) -> Result<Vec<Event>, ClientError> {
        let url = format!(
            "{}/executions/{execution_id}/events?after={after}",
            self.base_url
        );
        self.get_json(&url).await
    }

    /// The step timeline of `execution_id`.
    pub async fn timeline(&self, execution_id: &str) -> Result<Timeline, ClientError> {
        let url = format!("{}/executions/{execution_id}/timeline", self.base_url);
        self.get_json(&url).await
    }

    /// Conversation entries of one attempt, from entry `from` onward, up to
    /// `byte_budget` bytes.
    pub async fn conversation(
        &self,
        execution_id: &str,
        attempt_id: &str,
        from: u64,
        byte_budget: usize,
    ) -> Result<ConversationResponse, ClientError> {
        let url = format!(
            "{}/executions/{execution_id}/attempts/{attempt_id}/conversation?from={from}&byte_budget={byte_budget}",
            self.base_url
        );
        self.get_json(&url).await
    }

    // -----------------------------------------------------------------
    // Mutating calls (U5)
    // -----------------------------------------------------------------

    /// Starts a new execution of `workflow` with `inputs` — `POST /tasks`.
    pub async fn start_task(
        &self,
        workflow: &str,
        inputs: Map<String, Value>,
    ) -> Result<StartTaskResponse, ClientError> {
        let url = format!("{}/tasks", self.base_url);
        let body = StartTaskRequest {
            workflow: workflow.to_string(),
            inputs,
        };
        self.post_json(&url, &body).await
    }

    /// Resumes a paused execution — `POST /executions/:id/resume`.
    pub async fn resume(&self, execution_id: &str) -> Result<AcceptedResponse, ClientError> {
        let url = format!("{}/executions/{execution_id}/resume", self.base_url);
        self.post_empty(&url).await
    }

    /// Delivers a person's answer to a paused execution — `POST
    /// /executions/:id/input`.
    pub async fn deliver_input(
        &self,
        execution_id: &str,
        delivery: HumanInputDelivery,
    ) -> Result<AcceptedResponse, ClientError> {
        let url = format!("{}/executions/{execution_id}/input", self.base_url);
        self.post_json(&url, &delivery).await
    }

    /// A manual or forced transition — `POST /executions/:id/transition`.
    pub async fn transition(
        &self,
        execution_id: &str,
        request: TransitionRequest,
    ) -> Result<ExecutionSummary, ClientError> {
        let url = format!("{}/executions/{execution_id}/transition", self.base_url);
        self.post_json(&url, &request).await
    }

    /// Every stored execution, summarised — `GET /executions`.
    pub async fn list_executions(&self) -> Result<ExecutionListResponse, ClientError> {
        let url = format!("{}/executions", self.base_url);
        self.get_json(&url).await
    }

    /// Reads supported harnesses and the daemon's current default selection.
    pub async fn harness_selection(&self) -> Result<HarnessSelectionSnapshot, ClientError> {
        let url = format!("{}/settings/agent-harness", self.base_url);
        self.get_json(&url).await
    }

    /// Persists `harness` as the daemon's default and returns its confirmed
    /// post-commit snapshot.
    pub async fn set_harness_selection(
        &self,
        harness: &str,
    ) -> Result<HarnessSelectionSnapshot, ClientError> {
        let url = format!("{}/settings/agent-harness", self.base_url);
        self.put_json(
            &url,
            &SetHarnessSelectionRequest {
                harness: harness.to_owned(),
            },
        )
        .await
    }

    /// Removes the durable override and returns the configured fallback.
    pub async fn reset_harness_selection(&self) -> Result<HarnessSelectionSnapshot, ClientError> {
        let url = format!("{}/settings/agent-harness", self.base_url);
        self.delete_json(&url).await
    }

    // -----------------------------------------------------------------
    // SSE (U5)
    // -----------------------------------------------------------------

    /// An SSE stream of every event published for one execution (AE5).
    ///
    /// The stream ends itself — the caller sees `None` from `.next()` — the
    /// moment it has yielded a terminal event
    /// ([`EventData::WorkflowCompleted`]/[`EventData::WorkflowFailed`]/
    /// [`EventData::WorkflowCancelled`]), so a caller that wants to block
    /// until an execution settles can simply drain this stream to
    /// completion rather than inspecting every event itself to decide when
    /// to stop.
    pub fn stream_execution(
        &self,
        execution_id: &str,
    ) -> impl Stream<Item = Result<Event, ClientError>> {
        let url = format!("{}/executions/{execution_id}/stream", self.base_url);
        self.sse_stream(url, Some(execution_id.to_string()))
    }

    /// An SSE stream of every event from every execution the daemon runs.
    ///
    /// Unlike [`ApiClient::stream_execution`], there is no single execution
    /// whose terminal event would end this stream — it keeps running until
    /// the connection itself ends (the server stops, or the caller drops
    /// it). This is enforced by passing no `terminal_scope` to
    /// [`ApiClient::sse_stream`]: `extract_sse_events` only ever treats a
    /// terminal event as stream-ending when it is scoped to a specific
    /// execution id, which this call does not have.
    pub fn stream_all(&self) -> impl Stream<Item = Result<Event, ClientError>> {
        let url = format!("{}/events/stream", self.base_url);
        self.sse_stream(url, None)
    }

    /// Connects to `url` over [`ApiClient::stream_http`] (no request
    /// timeout — see its doc comment) and turns the response into a stream
    /// of parsed [`Event`]s, or a single [`ClientError`] item when the
    /// connection itself could not be established or the server refused it
    /// before ever opening the stream.
    ///
    /// `terminal_scope` is the execution id whose terminal event should end
    /// the stream, or `None` for a stream — [`ApiClient::stream_all`] — that
    /// must never self-terminate on content: passed straight through to
    /// [`sse_events`]/[`extract_sse_events`], which is the single parsing
    /// pipeline both [`ApiClient::stream_execution`] and
    /// [`ApiClient::stream_all`] share.
    fn sse_stream(
        &self,
        url: String,
        terminal_scope: Option<String>,
    ) -> impl Stream<Item = Result<Event, ClientError>> {
        let http = self.stream_http.clone();
        let token = self.token.clone();

        async move {
            let response = match http.get(&url).bearer_auth(&token).send().await {
                Ok(response) => response,
                Err(error) => {
                    return single_error_stream(ClientError::Unavailable(error.to_string()));
                }
            };

            match response.status() {
                StatusCode::OK => sse_events(response, terminal_scope).boxed(),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                    single_error_stream(ClientError::Unauthorized)
                }
                StatusCode::NOT_FOUND => single_error_stream(ClientError::NotFound),
                status => {
                    single_error_stream(ClientError::Server(format!("unexpected status {status}")))
                }
            }
        }
        .flatten_stream()
    }

    // -----------------------------------------------------------------
    // Request helpers
    // -----------------------------------------------------------------

    async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T, ClientError> {
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|error| ClientError::Unavailable(error.to_string()))?;
        handle_json_response(response).await
    }

    /// `POST url` with a JSON body, decoding the response as `T`.
    async fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        url: &str,
        body: &B,
    ) -> Result<T, ClientError> {
        let response = self
            .http
            .post(url)
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(|error| ClientError::Unavailable(error.to_string()))?;
        handle_json_response(response).await
    }

    /// `POST url` with no body, decoding the response as `T`.
    async fn post_empty<T: DeserializeOwned>(&self, url: &str) -> Result<T, ClientError> {
        let response = self
            .http
            .post(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|error| ClientError::Unavailable(error.to_string()))?;
        handle_json_response(response).await
    }

    async fn put_json<B: Serialize, T: DeserializeOwned>(
        &self,
        url: &str,
        body: &B,
    ) -> Result<T, ClientError> {
        let response = self
            .http
            .put(url)
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(|error| ClientError::Unavailable(error.to_string()))?;
        handle_json_response(response).await
    }

    async fn delete_json<T: DeserializeOwned>(&self, url: &str) -> Result<T, ClientError> {
        let response = self
            .http
            .delete(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|error| ClientError::Unavailable(error.to_string()))?;
        handle_json_response(response).await
    }
}

/// Maps a response's status to a [`ClientError`] the same way for every
/// call this client makes, `2xx` aside: `401`/`403` → [`ClientError::Unauthorized`],
/// `404` → [`ClientError::NotFound`], `409` → [`ClientError::Conflict`] (the
/// one status [`ApiClient::get_json`] never actually sees, since nothing it
/// calls mutates anything — kept in the shared helper anyway so a future
/// read endpoint that gains a conflict case does not have to duplicate this
/// match), anything else → [`ClientError::Server`].
///
/// `200` and `202` both count as success: every mutating endpoint in this
/// plan answers one or the other (`202 Accepted` for a call that only
/// kicks off a background pass, `200 OK` for `transition`, which answers
/// synchronously) and the body is always the type the caller asked to
/// decode.
async fn handle_json_response<T: DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, ClientError> {
    match response.status() {
        StatusCode::OK | StatusCode::ACCEPTED => response
            .json::<T>()
            .await
            .map_err(|error| ClientError::Server(error.to_string())),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(ClientError::Unauthorized),
        StatusCode::NOT_FOUND => Err(ClientError::NotFound),
        StatusCode::CONFLICT => Err(ClientError::Conflict),
        StatusCode::BAD_REQUEST => {
            let error = response
                .json::<RequestErrorResponse>()
                .await
                .map_err(|error| {
                    ClientError::Server(format!("invalid bad-request response: {error}"))
                })?;
            Err(ClientError::Request {
                code: error.code,
                message: error.message,
            })
        }
        status => Err(ClientError::Server(format!("unexpected status {status}"))),
    }
}

/// A boxed stream yielding exactly one `Err(error)` then ending — the shape
/// every early-exit branch of [`ApiClient::sse_stream`] needs, so that every
/// branch of its `match` produces the same concrete (boxed) stream type.
fn single_error_stream(
    error: ClientError,
) -> futures::stream::BoxStream<'static, Result<Event, ClientError>> {
    stream::once(async move { Err(error) }).boxed()
}

/// State threaded through [`stream::unfold`] to turn a raw byte stream (the
/// response body) into parsed [`Event`]s.
///
/// # Why a hand-rolled parser rather than a library like `eventsource-stream`
///
/// SSE's wire format here is about as simple as it gets — this server only
/// ever emits `event: engine\ndata: <json>\n\n` frames
/// (`src/api/routes.rs`'s `sse_event_stream`), with axum's own `KeepAlive`
/// comments the only other thing on the wire. `tests/sse.rs` already proved
/// out exactly this parsing approach (frame-split on `"\n\n"`, then a
/// `data: ` line per frame) against the real server before this unit existed,
/// so reusing it here needs no new dependency and no translation between a
/// library's event type and [`Event`] — a `serde_json::from_str::<Event>`
/// straight off the `data: ` line is the whole job. A library would only pay
/// for itself if this client had to interoperate with an SSE producer it did
/// not also control, which is not the case: the only server behind this
/// client is `src/api/routes.rs`, in the same crate.
struct SseState<S> {
    /// The response body's byte stream.
    inner: S,
    /// Bytes read so far that do not yet form a complete `"\n\n"`-terminated
    /// frame.
    buffer: String,
    /// Parsed items not yet handed back to the caller — a single chunk off
    /// the wire can contain more than one complete frame.
    queue: VecDeque<Result<Event, ClientError>>,
    /// Set once a terminal event has been queued, or the underlying stream
    /// ended/errored — once `true` and `queue` is empty, the stream is over.
    done: bool,
    /// The execution id whose terminal event ends this stream, or `None` for
    /// a stream that must never self-terminate on content
    /// ([`ApiClient::stream_all`]). See [`extract_sse_events`].
    terminal_scope: Option<String>,
}

/// Turns a successful (`200 OK`) SSE response into a stream of parsed
/// [`Event`]s, ending after `terminal_scope`'s terminal event (or never
/// self-terminating, when `terminal_scope` is `None`) — see [`SseState`].
fn sse_events(
    response: reqwest::Response,
    terminal_scope: Option<String>,
) -> impl Stream<Item = Result<Event, ClientError>> {
    let state = SseState {
        inner: response.bytes_stream(),
        buffer: String::new(),
        queue: VecDeque::new(),
        done: false,
        terminal_scope,
    };
    stream::unfold(state, |state| sse_unfold_step(state))
}

/// One step of [`stream::unfold`] over [`SseState`]: drains any already
/// parsed item first, otherwise reads and parses another chunk off the wire,
/// looping until there is an item to hand back or the stream is over.
async fn sse_unfold_step<S>(
    mut state: SseState<S>,
) -> Option<(Result<Event, ClientError>, SseState<S>)>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    loop {
        if let Some(item) = state.queue.pop_front() {
            return Some((item, state));
        }
        if state.done {
            return None;
        }

        match state.inner.next().await {
            Some(Ok(bytes)) => {
                state.buffer.push_str(&String::from_utf8_lossy(&bytes));
                extract_sse_events(
                    &mut state.buffer,
                    &mut state.queue,
                    &mut state.done,
                    state.terminal_scope.as_deref(),
                );
            }
            Some(Err(error)) => {
                state
                    .queue
                    .push_back(Err(ClientError::Unavailable(error.to_string())));
                state.done = true;
            }
            None => {
                state.done = true;
            }
        }
    }
}

/// Extracts every complete `"\n\n"`-terminated frame currently in `buffer`,
/// parses each one's `data: ` line as an [`Event`], and appends it to
/// `queue` — silently skipping a frame that has no `data: ` line (axum's
/// `KeepAlive` comment frames) or whose `data: ` line does not parse as an
/// `Event` (defence in depth; every real frame this server sends does).
///
/// Sets `*done = true` the moment a terminal event *belonging to
/// `terminal_scope`* is queued — per [`ApiClient::stream_execution`]'s
/// contract when `terminal_scope` is `Some(execution_id)`. When
/// `terminal_scope` is `None` ([`ApiClient::stream_all`]), a terminal event
/// never sets `done`: this is the one piece of state that makes the two
/// streams' contracts different despite sharing this parser. Getting this
/// scoping wrong is not hypothetical — an earlier version of this function
/// ended on *any* terminal event regardless of whose it was, which silently
/// killed the global stream the moment the first of several concurrently
/// running executions finished.
fn extract_sse_events(
    buffer: &mut String,
    queue: &mut VecDeque<Result<Event, ClientError>>,
    done: &mut bool,
    terminal_scope: Option<&str>,
) {
    while let Some(idx) = buffer.find("\n\n") {
        let frame = buffer[..idx].to_string();
        buffer.drain(..=idx + 1);

        for line in frame.lines() {
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            let Ok(event) = serde_json::from_str::<Event>(data) else {
                continue;
            };
            let terminal = is_terminal_event(&event.data)
                && terminal_scope == Some(event.workflow_execution_id.as_str());
            queue.push_back(Ok(event));
            if terminal {
                *done = true;
            }
        }
    }
}

/// Whether `data` is one of the three events that end a workflow execution
/// — see [`ApiClient::stream_execution`]'s contract.
fn is_terminal_event(data: &EventData) -> bool {
    matches!(
        data,
        EventData::WorkflowCompleted { .. }
            | EventData::WorkflowFailed { .. }
            | EventData::WorkflowCancelled { .. }
    )
}
