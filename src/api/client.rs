//! Typed HTTP client for the daemon API, the one thing U7's task view
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

use crate::conductor::{ChatEntry, ProposalOutcome, UndoOutcome};

use super::dto::{
    AcceptedResponse, ChatMessageRequest, ConductorChatResponse, ConductorStateResponse,
    ExecutionListResponse, ExecutionSummary, HarnessSelectionSnapshot, OperatorNoteRequest,
    RecoveryRequest, RequestErrorResponse, SetHarnessSelectionRequest, StartTaskRequest,
    StartTaskResponse, TransitionRequest,
};
use super::routes::ConversationResponse;
use crate::controller_protocol::{
    ControlEventEnvelope, ControllerListResponse, ControllerView, ConversationPage,
    ConversationPageQuery, ConversationResponse as RemoteConversationResponse,
    CreateEnrollmentRequest, CreateTaskRequest, EnrollmentResponse, OwnerCommandRequest,
    OwnerCommandResponse, SubmitInputRequest, SubmitInputResponse, TaskListResponse, TaskSummary,
    TimelineResponse,
};
use crate::orchestration::TaskEvent;

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
    /// A `409` the server gave its reason for — a proposal of the
    /// conductor's about a moment the execution has moved on from (KTD9), a
    /// decision already undone: the request conflicts with the execution's
    /// state, and `message` says how, for the operator to read.
    Refused { code: String, message: String },
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
            ClientError::Refused { code, message } => {
                write!(f, "API refused the request ({code}): {message}")
            }
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

    /// What the conductor of `execution_id` is doing and has kept (D55).
    pub async fn conductor_state(
        &self,
        execution_id: &str,
    ) -> Result<ConductorStateResponse, ClientError> {
        let url = format!("{}/executions/{execution_id}/conductor", self.base_url);
        self.get_json(&url).await
    }

    /// The conductor's chat history of `execution_id` after sequence
    /// `after`.
    pub async fn conductor_chat(
        &self,
        execution_id: &str,
        after: u64,
    ) -> Result<ConductorChatResponse, ClientError> {
        let url = format!(
            "{}/executions/{execution_id}/conductor/chat?after={after}",
            self.base_url
        );
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

    /// Explicitly starts a fresh attempt after a failed or interrupted run.
    pub async fn recover(
        &self,
        execution_id: &str,
        comment: Option<String>,
    ) -> Result<AcceptedResponse, ClientError> {
        let url = format!("{}/executions/{execution_id}/recover", self.base_url);
        self.post_json(
            &url,
            &RecoveryRequest {
                comment,
                episode: None,
            },
        )
        .await
    }

    /// Recovers an execution from the step failure `episode` names (KTD9):
    /// refused with `409` once it is no longer failed on it.
    pub async fn recover_failure(
        &self,
        execution_id: &str,
        episode: &str,
        comment: Option<String>,
    ) -> Result<AcceptedResponse, ClientError> {
        let url = format!("{}/executions/{execution_id}/recover", self.base_url);
        self.post_json(
            &url,
            &RecoveryRequest {
                comment,
                episode: Some(episode.to_string()),
            },
        )
        .await
    }

    /// Stores a one-shot operator instruction for the next agent attempt.
    pub async fn operator_note(
        &self,
        execution_id: &str,
        note: String,
    ) -> Result<AcceptedResponse, ClientError> {
        let url = format!("{}/executions/{execution_id}/operator-note", self.base_url);
        self.post_json(&url, &OperatorNoteRequest { note }).await
    }

    /// Immediately interrupts the daemon task driving an execution.
    pub async fn interrupt(&self, execution_id: &str) -> Result<AcceptedResponse, ClientError> {
        let url = format!("{}/executions/{execution_id}/interrupt", self.base_url);
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

    /// Writes `text` to the conductor of `execution_id` (F4) — `POST
    /// /executions/:id/conductor/chat`. Returns the entry as written; the
    /// answer follows in the chat history.
    pub async fn post_chat(
        &self,
        execution_id: &str,
        text: &str,
    ) -> Result<ChatEntry, ClientError> {
        let url = format!("{}/executions/{execution_id}/conductor/chat", self.base_url);
        self.post_json(
            &url,
            &ChatMessageRequest {
                text: text.to_string(),
            },
        )
        .await
    }

    /// Carries out the conductor's proposal `proposal_id` (R28). A second
    /// confirmation answers the first one's outcome; one about a moment the
    /// execution has moved on from is [`ClientError::Refused`].
    pub async fn confirm_proposal(
        &self,
        execution_id: &str,
        proposal_id: &str,
    ) -> Result<ProposalOutcome, ClientError> {
        let url = format!(
            "{}/executions/{execution_id}/conductor/proposals/{proposal_id}/confirm",
            self.base_url
        );
        self.post_empty(&url).await
    }

    /// Rejects the conductor's proposal `proposal_id`.
    pub async fn reject_proposal(
        &self,
        execution_id: &str,
        proposal_id: &str,
    ) -> Result<ProposalOutcome, ClientError> {
        let url = format!(
            "{}/executions/{execution_id}/conductor/proposals/{proposal_id}/reject",
            self.base_url
        );
        self.post_empty(&url).await
    }

    /// Undoes the conductor's chosen output `decision_id` (KTD13): the
    /// step is a problem again at its state.
    pub async fn undo_decision(
        &self,
        execution_id: &str,
        decision_id: &str,
    ) -> Result<UndoOutcome, ClientError> {
        let url = format!(
            "{}/executions/{execution_id}/conductor/decisions/{decision_id}/undo",
            self.base_url
        );
        self.post_empty(&url).await
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
    // Remote-task control plane (U2, KTD4)
    // -----------------------------------------------------------------

    /// Creates a remote task — `POST /control/v1/tasks`. Idempotent when
    /// the request names its `task_id`.
    pub async fn create_control_task(
        &self,
        request: &CreateTaskRequest,
    ) -> Result<TaskSummary, ClientError> {
        let url = format!("{}/control/v1/tasks", self.base_url);
        self.post_json(&url, request).await
    }

    /// The caller's remote tasks and the stream cursor at that state.
    pub async fn control_tasks(&self) -> Result<TaskListResponse, ClientError> {
        let url = format!("{}/control/v1/tasks", self.base_url);
        self.get_json(&url).await
    }

    /// One remote task's normalized summary.
    pub async fn control_task(&self, task_id: &str) -> Result<TaskSummary, ClientError> {
        let url = format!("{}/control/v1/tasks/{task_id}", self.base_url);
        self.get_json(&url).await
    }

    /// An owner command (pause, resume, start, archive) on a remote task.
    pub async fn control_command(
        &self,
        task_id: &str,
        request: &OwnerCommandRequest,
    ) -> Result<OwnerCommandResponse, ClientError> {
        let url = format!("{}/control/v1/tasks/{task_id}/commands", self.base_url);
        self.post_json(&url, request).await
    }

    /// The last timeline the remote task's runner reported.
    pub async fn control_timeline(&self, task_id: &str) -> Result<TimelineResponse, ClientError> {
        let url = format!("{}/control/v1/tasks/{task_id}/timeline", self.base_url);
        self.get_json(&url).await
    }

    /// The remote task's conversation summary, or why it is unavailable.
    pub async fn control_conversation(
        &self,
        task_id: &str,
    ) -> Result<RemoteConversationResponse, ClientError> {
        let url = format!("{}/control/v1/tasks/{task_id}/conversation", self.base_url);
        self.get_json(&url).await
    }

    /// One page of an attempt's raw conversation, relayed from the live
    /// runner (KTD4). Waits for the runner, so it uses the client without
    /// the short loopback timeout, bounded by its own.
    pub async fn control_conversation_page(
        &self,
        task_id: &str,
        attempt_id: &str,
        query: &ConversationPageQuery,
    ) -> Result<ConversationPage, ClientError> {
        let url = format!(
            "{}/control/v1/tasks/{task_id}/attempts/{attempt_id}/conversation",
            self.base_url
        );
        let response = self
            .stream_http
            .get(url)
            .bearer_auth(&self.token)
            .query(query)
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await
            .map_err(|error| ClientError::Unavailable(error.to_string()))?;
        handle_json_response(response).await
    }

    /// Submits an answer to the remote runner's pending input request.
    pub async fn control_input(
        &self,
        task_id: &str,
        request: &SubmitInputRequest,
    ) -> Result<SubmitInputResponse, ClientError> {
        let url = format!("{}/control/v1/tasks/{task_id}/input", self.base_url);
        self.post_json(&url, request).await
    }

    /// The remote task's control-plane events after `after`.
    pub async fn control_events(
        &self,
        task_id: &str,
        after: u64,
    ) -> Result<Vec<TaskEvent>, ClientError> {
        let url = format!(
            "{}/control/v1/tasks/{task_id}/events?after={after}",
            self.base_url
        );
        self.get_json(&url).await
    }

    /// Creates a one-time code that enrolls one controller for `backend_id`.
    pub async fn create_enrollment(
        &self,
        backend_id: &str,
        request: &CreateEnrollmentRequest,
    ) -> Result<EnrollmentResponse, ClientError> {
        let url = format!(
            "{}/control/v1/backends/{backend_id}/enrollments",
            self.base_url
        );
        self.post_json(&url, request).await
    }

    /// Every controller identity.
    pub async fn controllers(&self) -> Result<ControllerListResponse, ClientError> {
        let url = format!("{}/control/v1/controllers", self.base_url);
        self.get_json(&url).await
    }

    /// Revokes a controller identity and fences its lease.
    pub async fn revoke_controller(
        &self,
        controller_id: &str,
    ) -> Result<ControllerView, ClientError> {
        let url = format!(
            "{}/control/v1/controllers/{controller_id}/revoke",
            self.base_url
        );
        self.post_empty(&url).await
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
        self.sse_stream(url, engine_event_decoder(Some(execution_id.to_string())))
    }

    /// An SSE stream of every event from every execution the daemon runs.
    ///
    /// Unlike [`ApiClient::stream_execution`], there is no single execution
    /// whose terminal event would end this stream — it keeps running until
    /// the connection itself ends (the server stops, or the caller drops
    /// it). This is enforced by giving [`engine_event_decoder`] no
    /// `terminal_scope`: it only ever treats a terminal event as
    /// stream-ending when it is scoped to a specific execution id, which
    /// this call does not have.
    pub fn stream_all(&self) -> impl Stream<Item = Result<Event, ClientError>> {
        let url = format!("{}/events/stream", self.base_url);
        self.sse_stream(url, engine_event_decoder(None))
    }

    /// The control plane's durable event stream (U2): every event after
    /// `cursor` (all of them when `None`) exactly once, then live ones.
    ///
    /// Each item carries the cursor *after* it; reconnecting with the last
    /// one seen resumes without a gap or a repeat. `task_id` restricts the
    /// stream to one task. The stream never ends on content.
    pub fn stream_control_events(
        &self,
        cursor: Option<&str>,
        task_id: Option<&str>,
    ) -> impl Stream<Item = Result<ControlEventEnvelope, ClientError>> {
        let mut params = Vec::new();
        if let Some(cursor) = cursor {
            params.push(("cursor", cursor));
        }
        if let Some(task_id) = task_id {
            params.push(("task_id", task_id));
        }
        let url = reqwest::Url::parse_with_params(
            &format!("{}/control/v1/events/stream", self.base_url),
            &params,
        )
        .map(String::from)
        .unwrap_or_else(|_| format!("{}/control/v1/events/stream", self.base_url));
        let decode: FrameDecoder<ControlEventEnvelope> =
            Box::new(|data| serde_json::from_str(data).ok().map(|item| (item, false)));
        self.sse_stream(url, decode)
    }

    /// Connects to `url` over [`ApiClient::stream_http`] (no request
    /// timeout — see its doc comment) and turns the response into a stream
    /// of items `decode` produces, or a single [`ClientError`] item when the
    /// connection itself could not be established or the server refused it
    /// before ever opening the stream.
    ///
    /// `decode` is the only thing that differs between the streams this
    /// client reads: it parses one frame's `data:` payload and says whether
    /// that item ends the stream (see [`engine_event_decoder`]).
    fn sse_stream<T: Send + 'static>(
        &self,
        url: String,
        decode: FrameDecoder<T>,
    ) -> impl Stream<Item = Result<T, ClientError>> {
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
                StatusCode::OK => sse_events(response, decode).boxed(),
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
/// `200`, `201` and `202` all count as success: every mutating endpoint
/// answers one of them (`202 Accepted` for a call that only kicks off a
/// background pass, `201 Created` for a control-plane record that now
/// exists, `200 OK` for everything answered synchronously) and the body is
/// always the type the caller asked to decode.
pub(crate) async fn handle_json_response<T: DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, ClientError> {
    match response.status() {
        StatusCode::OK | StatusCode::CREATED | StatusCode::ACCEPTED => response
            .json::<T>()
            .await
            .map_err(|error| ClientError::Server(error.to_string())),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(ClientError::Unauthorized),
        StatusCode::NOT_FOUND => Err(ClientError::NotFound),
        StatusCode::CONFLICT => match response.json::<RequestErrorResponse>().await {
            Ok(error) => Err(ClientError::Refused {
                code: error.code,
                message: error.message,
            }),
            Err(_) => Err(ClientError::Conflict),
        },
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
fn single_error_stream<T: Send + 'static>(
    error: ClientError,
) -> futures::stream::BoxStream<'static, Result<T, ClientError>> {
    stream::once(async move { Err(error) }).boxed()
}

/// Decodes one SSE frame's `data:` payload into an item and says whether
/// that item ends the stream; `None` skips the frame (a payload this
/// stream does not understand).
type FrameDecoder<T> = Box<dyn FnMut(&str) -> Option<(T, bool)> + Send>;

/// The decoder of the engine's event streams.
///
/// A terminal event ends the stream only when it belongs to
/// `terminal_scope` — per [`ApiClient::stream_execution`]'s contract when
/// that is `Some(execution_id)`. When it is `None`
/// ([`ApiClient::stream_all`]), a terminal event never ends it: this is the
/// one piece of state that makes the two streams' contracts different
/// despite sharing one parser. Getting this scoping wrong is not
/// hypothetical — an earlier version ended on *any* terminal event
/// regardless of whose it was, which silently killed the global stream the
/// moment the first of several concurrently running executions finished.
fn engine_event_decoder(terminal_scope: Option<String>) -> FrameDecoder<Event> {
    Box::new(move |data| {
        let event = serde_json::from_str::<Event>(data).ok()?;
        let terminal = is_terminal_event(&event.data)
            && terminal_scope.as_deref() == Some(event.workflow_execution_id.as_str());
        Some((event, terminal))
    })
}

/// State threaded through [`stream::unfold`] to turn a raw byte stream (the
/// response body) into parsed items.
///
/// # Why a hand-rolled parser rather than a library like `eventsource-stream`
///
/// SSE's wire format here is about as simple as it gets — this server only
/// ever emits `event: <kind>\nid: <cursor>\ndata: <json>\n\n` frames
/// (`src/api/routes.rs`'s `sse_event_stream`, the control plane's stream in
/// `src/controller_protocol/http.rs`), with axum's own `KeepAlive` comments
/// the only other thing on the wire. `tests/sse.rs` already proved out
/// exactly this parsing approach (frame-split on `"\n\n"`, then a `data: `
/// line per frame) against the real server, so reusing it needs no new
/// dependency and no translation between a library's event type and ours —
/// a `serde_json::from_str` straight off the `data: ` line is the whole job.
/// The control stream's cursor travels inside the JSON as well as in `id:`,
/// so the parser never needs the `id:` line. A library would only pay for
/// itself if this client had to interoperate with an SSE producer it did
/// not also control, which is not the case.
struct SseState<S, T> {
    /// The response body's byte stream.
    inner: S,
    /// Bytes read so far that do not yet form a complete `"\n\n"`-terminated
    /// frame.
    buffer: String,
    /// Parsed items not yet handed back to the caller — a single chunk off
    /// the wire can contain more than one complete frame.
    queue: VecDeque<Result<T, ClientError>>,
    /// Set once a stream-ending item has been queued, or the underlying
    /// stream ended/errored — once `true` and `queue` is empty, the stream
    /// is over.
    done: bool,
    /// Turns a frame's payload into an item.
    decode: FrameDecoder<T>,
}

/// Turns a successful (`200 OK`) SSE response into a stream of decoded
/// items, ending after an item `decode` marks as final — see [`SseState`].
fn sse_events<T: Send + 'static>(
    response: reqwest::Response,
    decode: FrameDecoder<T>,
) -> impl Stream<Item = Result<T, ClientError>> {
    let state = SseState {
        inner: response.bytes_stream(),
        buffer: String::new(),
        queue: VecDeque::new(),
        done: false,
        decode,
    };
    stream::unfold(state, |state| sse_unfold_step(state))
}

/// One step of [`stream::unfold`] over [`SseState`]: drains any already
/// parsed item first, otherwise reads and parses another chunk off the wire,
/// looping until there is an item to hand back or the stream is over.
async fn sse_unfold_step<S, T>(
    mut state: SseState<S, T>,
) -> Option<(Result<T, ClientError>, SseState<S, T>)>
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
                    &mut state.decode,
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
/// decodes each one's `data: ` line, and appends it to `queue` — silently
/// skipping a frame that has no `data: ` line (axum's `KeepAlive` comment
/// frames) or whose payload `decode` does not accept (defence in depth;
/// every real frame this server sends decodes).
///
/// Sets `*done = true` the moment `decode` marks an item as the last one.
fn extract_sse_events<T>(
    buffer: &mut String,
    queue: &mut VecDeque<Result<T, ClientError>>,
    done: &mut bool,
    decode: &mut FrameDecoder<T>,
) {
    while let Some(idx) = buffer.find("\n\n") {
        let frame = buffer[..idx].to_string();
        buffer.drain(..=idx + 1);

        for line in frame.lines() {
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            let Some((item, last)) = decode(data) else {
                continue;
            };
            queue.push_back(Ok(item));
            if last {
                *done = true;
            }
        }
    }
}

/// Whether `data` is one of the three events that end a workflow execution
/// — see [`ApiClient::stream_execution`]'s contract.
///
/// The conductor's events (`ConductorStarted`/`Finished`/`Decision`) never
/// end a stream, even when they follow `WorkflowFailed` (F3): the
/// conductor works beside an execution, it does not change how it ends.
fn is_terminal_event(data: &EventData) -> bool {
    match data {
        EventData::WorkflowCompleted { .. }
        | EventData::WorkflowFailed { .. }
        | EventData::WorkflowCancelled { .. } => true,
        EventData::ConductorStarted { .. }
        | EventData::ConductorFinished { .. }
        | EventData::ConductorDecision { .. }
        | EventData::ConductorDecisionUndone { .. } => false,
        _ => false,
    }
}
