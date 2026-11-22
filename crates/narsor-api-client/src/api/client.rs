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
//!
//! # Remote control planes (U8)
//!
//! [`ApiClient::remote`] talks to a control plane at a URL instead of a
//! loopback port: `https://…` behind the TLS-terminating proxy of a network
//! deployment, or plain `http://` to a loopback address only (a trusted
//! daemon, a local tunnel). Its timeout is longer, because a network hop
//! legitimately takes longer than a loopback one, and its token comes from a
//! [`TokenSource`] that is re-read after a `401` — see [`super::token`] for
//! why a `401` and a `403` are told apart and only the first is retried.
//!
//! Nothing a request carries names a backend address, a runner or a
//! credential other than the caller's own bearer token: the control plane is
//! the only endpoint a client ever talks to (R1, R41).

use std::collections::{BTreeMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::future::FutureExt as _;
use futures::stream::{self, Stream, StreamExt as _};
use narsor_shared::api_error::handle_json_response;
pub use narsor_shared::api_error::ClientError;
use reqwest::StatusCode;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;

use narsor_protocol::api::{
    ClaimRequest, ConversationResponse as ConversationPage, ExecutionLookupResponse, MaterialFile,
    MaterialListResponse, MaterialSummary, MaterialUploadResponse, MaterialView, OwnerView,
    WhoAmIResponse, BUNDLE_CONTENT_TYPE,
};
use narsor_protocol::controller::{
    ControlEventEnvelope, ConversationResponse as RemoteConversationResponse, MoveTaskRequest,
    OwnerCommandResponse, RefreshEnvironmentRequest, RefreshEnvironmentResponse,
    SubmitCommentRequest, SubmitCommentResponse, SubmitInputResponse,
    SubmitOperatorCommandResponse, TaskEvent, TaskListResponse, TaskSummary, TimelineResponse,
};

use super::dto::{
    ExecutionListResponse, HarnessSelectionSnapshot, RequestErrorResponse,
    SetHarnessSelectionRequest, StartTaskRequest, StartTaskResponse,
};
use crate::controller_protocol::ConductorReadResponse;
use narsor_protocol::controller::{
    CapabilitiesView, ConcurrencyLimitView, ConductorReadQuery, ControllerListResponse,
    ControllerView, ConversationPageQuery, CreateEnrollmentRequest, CreateRepositoryRequest,
    CreateTaskRequest, DefaultBackendView, EnrollmentResponse, ExecutionDetailsResponse,
    ExecutionEventsQuery, ExecutionEventsResponse, ExecutionStateResponse, HarnessUsageResponse,
    ObserverClaimResponse, ObserverCreateTaskRequest, ObserverDecideRequest,
    ObserverEffectResponse, ObserverRunReport, ObserverRunResultResponse, ObserverWorkerRun,
    OwnerCommandRequest, RepositoryBranchesResponse, RepositoryListResponse, RepositoryView,
    SetConcurrencyLimitRequest, SetDefaultBackendRequest, SubmitInputRequest,
    SubmitOperatorCommandRequest, ValidateWorkflowRequest, ValidateWorkflowResponse,
    WorkflowListResponse, WorkflowSourceRequest, WorkflowSourceResponse,
};
use narsor_protocol::controller::{ObserverQueueListResponse, ObserverQueueView};

use super::token::{TokenCell, TokenSource};

/// How long a single request may take before it is treated as the server
/// being unavailable — and, since every call is awaited on the TUI's
/// key-handling path, the worst-case stall an operator can see from a single
/// slow request. See the module doc for why this is short rather than
/// generous.
const REQUEST_TIMEOUT: Duration = Duration::from_millis(800);

/// The request timeout of a client built by [`ApiClient::remote`]: a proxy,
/// TLS and a real network sit between it and the control plane, so the
/// loopback bound would turn ordinary latency into "unavailable".
const REMOTE_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a relayed conversation page may take: the control plane waits
/// for the runner's next poll before it can answer.
const CONVERSATION_PAGE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long listing the branches of a remote may take: the server lets
/// `git ls-remote` run for 20 seconds, so the client waits a little longer
/// and the server's own answer is the failure signal.
const BRANCH_LISTING_TIMEOUT: Duration = Duration::from_secs(25);

/// How long the observer worker's claim may take: the control plane holds
/// it up to 25 s while no run is due (a long-poll), so the client waits
/// longer than that.
const OBSERVER_CLAIM_TIMEOUT: Duration = Duration::from_secs(40);

/// How long one material upload or blob download may take: tens of
/// megabytes over a real network, far beyond a control request's bound.
const MATERIAL_TRANSFER_TIMEOUT: Duration = Duration::from_secs(600);

/// First delay before [`ApiClient::follow_control_events`] reconnects after
/// a failure; doubled per consecutive failure up to
/// [`MAX_RECONNECT_DELAY`].
const FIRST_RECONNECT_DELAY: Duration = Duration::from_millis(250);

/// Upper bound of the reconnect delay.
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(15);

/// Whose token a forwarded request carries ([`ApiClient::forward`]).
#[derive(Debug, Clone)]
pub enum ForwardAuth {
    /// This client's own token.
    Own,
    /// The `Authorization` header of the request being forwarded.
    Header(reqwest::header::HeaderValue),
}

/// A client bound to one running server: a loopback daemon
/// ([`ApiClient::new`]) or a control plane at a URL ([`ApiClient::remote`]).
///
/// Cheap to clone: clones share their connection pools and their token, so
/// a token refreshed by one serves the others.
#[derive(Clone)]
pub struct ApiClient {
    base_url: String,
    token: Arc<TokenCell>,
    http: reqwest::Client,
    /// A second client, deliberately built with no request timeout, used
    /// only by the SSE streams ([`ApiClient::stream_control_events`]).
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

impl std::fmt::Debug for ApiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiClient")
            .field("base_url", &self.base_url)
            .field("token", &self.token)
            .finish_non_exhaustive()
    }
}

impl ApiClient {
    /// Builds a client for the server at `addr`, presenting `token` on every
    /// request.
    pub fn new(addr: SocketAddr, token: impl Into<String>) -> Self {
        Self::build(
            format!("http://{addr}"),
            TokenSource::Static(token.into()),
            REQUEST_TIMEOUT,
        )
    }

    /// Like [`ApiClient::new`], with a request timeout of the caller's
    /// choice, for calls that are not awaited on the TUI's key path (the
    /// daemon health check).
    pub fn with_timeout(addr: SocketAddr, token: impl Into<String>, timeout: Duration) -> Self {
        Self::build(
            format!("http://{addr}"),
            TokenSource::Static(token.into()),
            timeout,
        )
    }

    /// Builds a client for the control plane at `base_url`, presenting the
    /// token `source` yields.
    ///
    /// `base_url` must be `https://…`; plain `http://` is accepted for a
    /// loopback host only, so a token is never sent in clear text over a
    /// network. A path prefix (a proxy mounting the API under `/narsor`) is
    /// kept; a query, a fragment or credentials in the URL are refused.
    pub fn remote(base_url: &str, source: TokenSource) -> Result<Self, ClientError> {
        let base_url = normalize_base_url(base_url).map_err(|message| ClientError::Request {
            code: "invalid_server_url".to_string(),
            message,
        })?;
        Ok(Self::build(base_url, source, REMOTE_REQUEST_TIMEOUT))
    }

    fn build(base_url: String, source: TokenSource, timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("reqwest client with only a timeout configured never fails to build");
        let stream_http = reqwest::Client::builder()
            .build()
            .expect("reqwest client with no special configuration never fails to build");
        Self {
            base_url,
            token: Arc::new(TokenCell::new(source)),
            http,
            stream_http,
        }
    }

    /// Sends one request of another client on to this server (the proxy of
    /// `narsor-web`): `method` on `path_and_query` (which starts with `/`),
    /// with `headers` and `body` as they came, and returns the answer
    /// unread, so a stream (SSE) flows through as it arrives.
    ///
    /// `auth` says whose token the request carries: this client's own
    /// ([`ForwardAuth::Own`], sent once more after a `401` with a re-read
    /// token, like every other call of this client) or the browser's header
    /// ([`ForwardAuth::Header`], sent as it is). There is no default, so a
    /// request without a header of its own can never borrow this client's
    /// token by accident.
    pub async fn forward(
        &self,
        method: reqwest::Method,
        path_and_query: &str,
        headers: reqwest::header::HeaderMap,
        body: Bytes,
        auth: ForwardAuth,
    ) -> Result<reqwest::Response, ClientError> {
        let url = format!("{}{path_and_query}", self.base_url);
        let build = |http: &reqwest::Client| {
            http.request(method.clone(), &url)
                .headers(headers.clone())
                .body(body.clone())
        };
        match auth {
            ForwardAuth::Header(value) => build(&self.stream_http)
                .header(reqwest::header::AUTHORIZATION, value)
                .send()
                .await
                .map_err(|error| ClientError::Unavailable(error.to_string())),
            ForwardAuth::Own => {
                self.send(&self.stream_http, |http, token| {
                    build(http).bearer_auth(token)
                })
                .await
            }
        }
    }

    /// The server's base URL, without a trailing slash.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    // -----------------------------------------------------------------
    // Mutating calls (U5)
    // -----------------------------------------------------------------

    /// Queues `workflow` with `inputs` — `POST /tasks`.
    ///
    /// `task_id` belongs to the caller's submission lifetime.  In
    /// particular, a caller retrying after an uncertain transport failure
    /// must reuse it with the same body so the control plane can replay the
    /// durable acknowledgement instead of enqueueing another task.
    pub async fn start_task(
        &self,
        task_id: &str,
        workflow: &str,
        inputs: Map<String, Value>,
    ) -> Result<StartTaskResponse, ClientError> {
        self.start_task_request(&StartTaskRequest {
            task_id: task_id.to_string(),
            workflow: workflow.to_string(),
            inputs,
            workspaces: Default::default(),
            ticket: None,
            title: None,
        })
        .await
    }

    /// [`Self::start_task`] with everything the operator chose: the
    /// workspaces, the ticket and the title, passed on as typed — the server
    /// trims and validates them.
    pub async fn start_task_request(
        &self,
        body: &StartTaskRequest,
    ) -> Result<StartTaskResponse, ClientError> {
        let url = format!("{}/tasks", self.base_url);
        self.post_json(&url, body).await
    }

    /// The execution of every task of the owner, as the task records hold
    /// them — `GET /control/v1/executions`.
    pub async fn control_executions(&self) -> Result<ExecutionListResponse, ClientError> {
        let url = format!("{}/control/v1/executions", self.base_url);
        self.get_json(&url).await
    }

    /// One execution of the owner and the task that runs it — `GET
    /// /control/v1/executions/:execution_id`.
    pub async fn control_execution(
        &self,
        execution_id: &str,
    ) -> Result<ExecutionLookupResponse, ClientError> {
        let url = format!("{}/control/v1/executions/{execution_id}", self.base_url);
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

    /// Only the archived tasks, newest archive first (the Historia), or
    /// only the others when `archived` is `false`.
    pub async fn control_tasks_archived(
        &self,
        archived: bool,
    ) -> Result<TaskListResponse, ClientError> {
        let url = format!("{}/control/v1/tasks?archived={archived}", self.base_url);
        self.get_json(&url).await
    }

    /// What the server offers: path validation, old executions, the name of
    /// the connection (U6).
    pub async fn control_capabilities(&self) -> Result<CapabilitiesView, ClientError> {
        let url = format!("{}/control/v1/capabilities", self.base_url);
        self.get_json(&url).await
    }

    /// The workflows a task can be created from.
    pub async fn control_workflows(&self) -> Result<WorkflowListResponse, ClientError> {
        let url = format!("{}/control/v1/workflows", self.base_url);
        self.get_json(&url).await
    }

    /// Resolve one workflow definition, or fetch one embedded built-in.
    pub async fn control_workflow_source(
        &self,
        name: &str,
        builtin: bool,
    ) -> Result<WorkflowSourceResponse, ClientError> {
        let url = format!("{}/control/v1/workflows/source", self.base_url);
        self.post_json(
            &url,
            &WorkflowSourceRequest {
                name: name.to_string(),
                builtin,
            },
        )
        .await
    }

    /// Every violation of a workflow file on the server's disk; refused
    /// (`400`) by a server whose capabilities say `workflow_paths: false`.
    pub async fn control_validate_workflow(
        &self,
        path: &str,
    ) -> Result<ValidateWorkflowResponse, ClientError> {
        let url = format!("{}/control/v1/workflows/validate", self.base_url);
        self.post_json(
            &url,
            &ValidateWorkflowRequest {
                path: path.to_string(),
            },
        )
        .await
    }

    /// The registered repositories.
    pub async fn control_repositories(&self) -> Result<RepositoryListResponse, ClientError> {
        let url = format!("{}/control/v1/repositories", self.base_url);
        self.get_json(&url).await
    }

    /// The branches of a registered repository on its remote;
    /// [`ClientError::NotFound`] for an unknown id, and an error with the
    /// reason when the remote cannot be listed.
    pub async fn control_repository_branches(
        &self,
        id: &str,
    ) -> Result<RepositoryBranchesResponse, ClientError> {
        let url = format!("{}/control/v1/repositories/{id}/branches", self.base_url);
        let response = self
            .send(&self.stream_http, |http, token| {
                http.get(&url)
                    .bearer_auth(token)
                    .timeout(BRANCH_LISTING_TIMEOUT)
            })
            .await?;
        handle_json_response(response).await
    }

    /// Registers a repository.
    pub async fn control_add_repository(
        &self,
        request: &CreateRepositoryRequest,
    ) -> Result<RepositoryView, ClientError> {
        let url = format!("{}/control/v1/repositories", self.base_url);
        self.post_json(&url, request).await
    }

    /// Removes a repository; [`ClientError::NotFound`] for an unknown id.
    pub async fn control_remove_repository(&self, id: &str) -> Result<RepositoryView, ClientError> {
        let url = format!("{}/control/v1/repositories/{id}", self.base_url);
        self.delete_json(&url).await
    }

    /// One remote task's normalized summary.
    pub async fn control_task(&self, task_id: &str) -> Result<TaskSummary, ClientError> {
        let url = format!("{}/control/v1/tasks/{task_id}", self.base_url);
        self.get_json(&url).await
    }

    /// One remote task's summary with how its stored environment stands
    /// against the current catalogue. It makes the server read the
    /// catalogue, so pollers use [`Self::control_task`].
    pub async fn control_task_with_drift(&self, task_id: &str) -> Result<TaskSummary, ClientError> {
        let url = format!("{}/control/v1/tasks/{task_id}?drift=true", self.base_url);
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

    /// Moves a queued remote task within its backend's queue (R1);
    /// repeating a move that already holds is a no-op.
    pub async fn control_move_task(
        &self,
        task_id: &str,
        request: &MoveTaskRequest,
    ) -> Result<TaskSummary, ClientError> {
        let url = format!("{}/control/v1/tasks/{task_id}/move", self.base_url);
        self.post_json(&url, request).await
    }

    /// Re-resolves a stopped remote task's environment from the current
    /// catalogue; says whether anything changed.
    pub async fn control_refresh_environment(
        &self,
        task_id: &str,
        request: &RefreshEnvironmentRequest,
    ) -> Result<RefreshEnvironmentResponse, ClientError> {
        let url = format!(
            "{}/control/v1/tasks/{task_id}/refresh-environment",
            self.base_url
        );
        self.post_json(&url, request).await
    }

    /// The default backend of new remote tasks (R10).
    pub async fn control_default_backend(&self) -> Result<DefaultBackendView, ClientError> {
        let url = format!("{}/control/v1/default-backend", self.base_url);
        self.get_json(&url).await
    }

    /// Chooses (or with `backend_id: None` clears) the default backend of
    /// new remote tasks; existing tasks never move.
    pub async fn control_set_default_backend(
        &self,
        request: &SetDefaultBackendRequest,
    ) -> Result<DefaultBackendView, ClientError> {
        let url = format!("{}/control/v1/default-backend", self.base_url);
        self.put_json(&url, request).await
    }

    /// The limit of tasks processed at once and what the tasks hold.
    pub async fn control_concurrency_limit(&self) -> Result<ConcurrencyLimitView, ClientError> {
        let url = format!("{}/control/v1/concurrency-limit", self.base_url);
        self.get_json(&url).await
    }

    /// Changes the limit of tasks processed at once; it lasts until the
    /// control plane restarts.
    pub async fn control_set_concurrency_limit(
        &self,
        limit: u32,
    ) -> Result<ConcurrencyLimitView, ClientError> {
        let url = format!("{}/control/v1/concurrency-limit", self.base_url);
        self.put_json(&url, &SetConcurrencyLimitRequest { limit })
            .await
    }

    /// The latest usage limits of the harnesses' subscriptions, as the
    /// runners reported them: known harnesses in their fixed order, the
    /// shortest window first. Empty when no fresh report is held.
    pub async fn control_usage(&self) -> Result<HarnessUsageResponse, ClientError> {
        let url = format!("{}/control/v1/usage", self.base_url);
        self.get_json(&url).await
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
        self.get_relayed(&url, query).await
    }

    /// A `GET` that the control plane answers through a runner's next poll,
    /// so it may take as long as [`CONVERSATION_PAGE_TIMEOUT`].
    async fn get_relayed<Q, R>(&self, url: &str, query: &Q) -> Result<R, ClientError>
    where
        Q: serde::Serialize + ?Sized,
        R: serde::de::DeserializeOwned,
    {
        let response = self
            .send(&self.stream_http, |http, token| {
                http.get(url)
                    .bearer_auth(token)
                    .query(query)
                    .timeout(CONVERSATION_PAGE_TIMEOUT)
            })
            .await?;
        handle_json_response(response).await
    }

    /// The raw events of a task's execution after `query.after`: from the
    /// live runner through the relay, or from the task's storage. Waits for
    /// the runner like a conversation page, so it shares its timeout.
    pub async fn control_execution_events(
        &self,
        task_id: &str,
        query: &ExecutionEventsQuery,
    ) -> Result<ExecutionEventsResponse, ClientError> {
        let url = format!(
            "{}/control/v1/tasks/{task_id}/execution-events",
            self.base_url
        );
        self.get_relayed(&url, query).await
    }

    /// The details of a task's execution that only its storage holds — the
    /// context variables, the conductor's summary and activity, a failed
    /// step's problem — from the live runner or, for a stopped task, its
    /// read-only runner, through the relay. Waits for the runner like an
    /// events page, so it shares its timeout.
    pub async fn control_execution_details(
        &self,
        task_id: &str,
    ) -> Result<ExecutionDetailsResponse, ClientError> {
        let url = format!(
            "{}/control/v1/tasks/{task_id}/execution-details",
            self.base_url
        );
        self.get_relayed(&url, &()).await
    }

    /// The complete state of a task's execution — context, the local values
    /// of each step and the conductor's state — through the relay. Waits for
    /// the runner like an events page, so it shares its timeout.
    pub async fn control_execution_state(
        &self,
        task_id: &str,
    ) -> Result<ExecutionStateResponse, ClientError> {
        let url = format!(
            "{}/control/v1/tasks/{task_id}/execution-state",
            self.base_url
        );
        self.get_relayed(&url, &()).await
    }

    /// The state of a task's conductor — chat after `query.after`,
    /// decisions, proposals, the diagnosis of a failed step: from the live
    /// runner through the relay, or from the task's storage. Waits for the
    /// runner like an events page, so it shares its timeout.
    pub async fn control_conductor(
        &self,
        task_id: &str,
        query: &ConductorReadQuery,
    ) -> Result<ConductorReadResponse, ClientError> {
        let url = format!("{}/control/v1/tasks/{task_id}/conductor", self.base_url);
        self.get_relayed(&url, query).await
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

    /// Queues an operator command (interrupt, transition, restart) for the
    /// remote task's runner; its verdict follows in the task's events.
    pub async fn control_operator_command(
        &self,
        task_id: &str,
        request: &SubmitOperatorCommandRequest,
    ) -> Result<SubmitOperatorCommandResponse, ClientError> {
        let url = format!(
            "{}/control/v1/tasks/{task_id}/operator-commands",
            self.base_url
        );
        self.post_json(&url, request).await
    }

    /// Sends the owner's comment to a remote task; the server picks what it
    /// does by the way the task is stopped.
    pub async fn control_comment(
        &self,
        task_id: &str,
        request: &SubmitCommentRequest,
    ) -> Result<SubmitCommentResponse, ClientError> {
        let url = format!("{}/control/v1/tasks/{task_id}/comment", self.base_url);
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

    /// Uploads an encoded material bundle to a remote task (U9) —
    /// `POST /control/v1/tasks/:task_id/materials`. Repeating an upload
    /// whose manifest names its `material_id` is safe.
    pub async fn upload_material(
        &self,
        task_id: &str,
        bundle: Bytes,
    ) -> Result<MaterialUploadResponse, ClientError> {
        let url = format!("{}/control/v1/tasks/{task_id}/materials", self.base_url);
        let response = self
            .send(&self.stream_http, |http, token| {
                http.post(&url)
                    .bearer_auth(token)
                    .header(reqwest::header::CONTENT_TYPE, BUNDLE_CONTENT_TYPE)
                    .body(bundle.clone())
                    .timeout(MATERIAL_TRANSFER_TIMEOUT)
            })
            .await?;
        handle_json_response(response).await
    }

    /// The remote task's materials.
    pub async fn materials(&self, task_id: &str) -> Result<MaterialListResponse, ClientError> {
        let url = format!("{}/control/v1/tasks/{task_id}/materials", self.base_url);
        self.get_json(&url).await
    }

    /// One material of the remote task, with its manifest.
    pub async fn material(
        &self,
        task_id: &str,
        material_id: &str,
    ) -> Result<MaterialView, ClientError> {
        let url = format!(
            "{}/control/v1/tasks/{task_id}/materials/{material_id}",
            self.base_url
        );
        self.get_json(&url).await
    }

    /// Deletes one material at the control plane.
    pub async fn delete_material(
        &self,
        task_id: &str,
        material_id: &str,
    ) -> Result<MaterialSummary, ClientError> {
        let url = format!(
            "{}/control/v1/tasks/{task_id}/materials/{material_id}",
            self.base_url
        );
        self.delete_json(&url).await
    }

    /// Downloads one material: its manifest and every blob, each checked
    /// against its digest and size before it is returned. Nothing is
    /// written anywhere; [`crate::materials::write_files`] does that.
    pub async fn fetch_material(
        &self,
        task_id: &str,
        material_id: &str,
    ) -> Result<(MaterialView, BTreeMap<String, Bytes>), ClientError> {
        let view = self.material(task_id, material_id).await?;
        let mut blobs = BTreeMap::new();
        for (sha256, size) in material_blob_order(&view.files) {
            let url = format!(
                "{}/control/v1/tasks/{task_id}/materials/{material_id}/blobs/{sha256}",
                self.base_url
            );
            let response = self
                .send(&self.stream_http, |http, token| {
                    http.get(&url)
                        .bearer_auth(token)
                        .timeout(MATERIAL_TRANSFER_TIMEOUT)
                })
                .await?;
            if response.status() != StatusCode::OK {
                return Err(
                    match handle_json_response::<serde_json::Value>(response).await {
                        Err(error) => error,
                        Ok(_) => ClientError::Server("unexpected answer to a blob download".into()),
                    },
                );
            }
            let bytes = response
                .bytes()
                .await
                .map_err(|error| ClientError::Unavailable(error.to_string()))?;
            if bytes.len() as u64 != size || material_sha256_hex(&bytes) != sha256 {
                return Err(ClientError::Server(format!(
                    "blob {sha256} of material '{material_id}' does not match its manifest"
                )));
            }
            blobs.insert(sha256, bytes);
        }
        Ok((view, blobs))
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
    // The observer worker (U6)
    // -----------------------------------------------------------------

    /// Claims the next due observer run, for a client holding the worker
    /// key — `POST /observer/v1/worker/claim`. `None` when no run became
    /// due while the control plane held the request.
    pub async fn claim_observer_run(&self) -> Result<Option<ObserverWorkerRun>, ClientError> {
        let url = format!("{}/observer/v1/worker/claim", self.base_url);
        let response = self
            .send(&self.stream_http, |http, token| {
                http.post(url.as_str())
                    .bearer_auth(token)
                    .timeout(OBSERVER_CLAIM_TIMEOUT)
            })
            .await?;
        let response: ObserverClaimResponse = handle_json_response(response).await?;
        Ok(response.run)
    }

    /// Reports the result of a claimed observer run —
    /// `POST /observer/v1/worker/runs/{run}/result`.
    pub async fn report_observer_run(
        &self,
        run_id: &str,
        report: &ObserverRunReport,
    ) -> Result<ObserverRunResultResponse, ClientError> {
        let url = format!("{}/observer/v1/worker/runs/{run_id}/result", self.base_url);
        self.post_json(&url, report).await
    }

    // -----------------------------------------------------------------
    // Queues: the owner's read-only views, then a run's effect routes
    // -----------------------------------------------------------------

    /// The catalogue's queues with the caller's entry counts, and the
    /// catalogue files that failed to load — `GET /control/v1/observer-queues`.
    pub async fn observer_queues(&self) -> Result<ObserverQueueListResponse, ClientError> {
        let url = format!("{}/control/v1/observer-queues", self.base_url);
        self.get_json(&url).await
    }

    /// One queue with its automations, the caller's entries and the
    /// problems of its file — `GET /control/v1/observer-queues/{queue}`.
    pub async fn observer_queue(&self, queue: &str) -> Result<ObserverQueueView, ClientError> {
        let queue = path_segment("queue", queue)?;
        let url = format!("{}/control/v1/observer-queues/{queue}", self.base_url);
        self.get_json(&url).await
    }

    /// Creates a task from effect data, for a client holding a run token
    /// — `POST /observer/v1/runs/{run}/tasks`.
    pub async fn observer_create_task(
        &self,
        run_id: &str,
        request: &ObserverCreateTaskRequest,
    ) -> Result<ObserverEffectResponse, ClientError> {
        let run_id = path_segment("run", run_id)?;
        let url = format!("{}/observer/v1/runs/{run_id}/tasks", self.base_url);
        self.post_json(&url, request).await
    }

    /// Answers the gate of the run's queue entry, for a client holding a
    /// run token — `POST /observer/v1/runs/{run}/decide`.
    pub async fn observer_decide(
        &self,
        run_id: &str,
        request: &ObserverDecideRequest,
    ) -> Result<ObserverEffectResponse, ClientError> {
        let run_id = path_segment("run", run_id)?;
        let url = format!("{}/observer/v1/runs/{run_id}/decide", self.base_url);
        self.post_json(&url, request).await
    }

    // -----------------------------------------------------------------
    // Identity (U7, U8)
    // -----------------------------------------------------------------

    /// What the server makes of this client's token — `GET /auth/v1/whoami`.
    pub async fn whoami(&self) -> Result<WhoAmIResponse, ClientError> {
        let url = format!("{}/auth/v1/whoami", self.base_url);
        self.get_json(&url).await
    }

    /// Claims an unowned network-mode installation with the one-time code
    /// its daemon printed (R38) — `POST /auth/v1/claim`. A wrong or spent
    /// code, or an installation owned by somebody else, is
    /// [`ClientError::Forbidden`]; a trusted-mode daemon, which has no owner
    /// to claim, answers [`ClientError::NotFound`].
    pub async fn claim(&self, claim_code: &str) -> Result<OwnerView, ClientError> {
        let url = format!("{}/auth/v1/claim", self.base_url);
        self.post_json(
            &url,
            &ClaimRequest {
                claim_code: claim_code.to_string(),
            },
        )
        .await
    }

    // -----------------------------------------------------------------
    // SSE (U5)
    // -----------------------------------------------------------------

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

    /// [`ApiClient::stream_control_events`] that outlives its connections.
    ///
    /// A control-plane stream ends for ordinary reasons: the access token it
    /// was opened with expires (network mode ends the stream at exactly that
    /// moment), a proxy recycles the connection, the daemon restarts. This
    /// stream reconnects after each of them from the cursor of the last
    /// event it yielded, so nothing is missed or repeated, re-reading the
    /// token when the reconnect draws a `401`.
    ///
    /// A failed (re)connect is yielded as an `Err` item — a client shows it
    /// as "reconnecting" — and retried after a growing delay
    /// ([`FIRST_RECONNECT_DELAY`] … [`MAX_RECONNECT_DELAY`]) while it is a
    /// transport or server failure. A refusal a retry cannot fix
    /// ([`ClientError::Unauthorized`] after re-reading the token,
    /// [`ClientError::Forbidden`], [`ClientError::NotFound`], an invalid
    /// cursor) is yielded once and ends the stream.
    pub fn follow_control_events(
        &self,
        cursor: Option<String>,
        task_id: Option<String>,
    ) -> impl Stream<Item = Result<ControlEventEnvelope, ClientError>> {
        let state = FollowState {
            client: self.clone(),
            cursor,
            task_id,
            connection: None,
            delivered: false,
            failures: 0,
            done: false,
        };
        stream::unfold(state, follow_step)
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
        let client = self.clone();

        async move {
            let response = match client
                .send(&client.stream_http, |http, token| {
                    http.get(&url).bearer_auth(token)
                })
                .await
            {
                Ok(response) => response,
                Err(error) => return single_error_stream(error),
            };

            match response.status() {
                StatusCode::OK => sse_events(response, decode).boxed(),
                StatusCode::UNAUTHORIZED => single_error_stream(ClientError::Unauthorized),
                StatusCode::FORBIDDEN => single_error_stream(ClientError::Forbidden),
                StatusCode::NOT_FOUND => single_error_stream(ClientError::NotFound),
                StatusCode::BAD_REQUEST => match response.json::<RequestErrorResponse>().await {
                    Ok(error) => single_error_stream(ClientError::Request {
                        code: error.code,
                        message: error.message,
                    }),
                    Err(error) => single_error_stream(ClientError::Server(error.to_string())),
                },
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

    /// Sends the request `build` makes with the current token; after a
    /// `401`, re-reads the token and — when that yields another one — sends
    /// it once more. Every other answer is returned as it is.
    ///
    /// Retrying after a `401` is safe for every method: the gate refuses a
    /// request before any handler runs, so nothing of the first attempt took
    /// effect.
    async fn send(
        &self,
        http: &reqwest::Client,
        build: impl Fn(&reqwest::Client, &str) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, ClientError> {
        let token = self.token.current().await?;
        let response = build(http, &token)
            .send()
            .await
            .map_err(|error| ClientError::Unavailable(error.to_string()))?;
        if response.status() != StatusCode::UNAUTHORIZED || !self.token.refresh(&token).await? {
            return Ok(response);
        }
        let token = self.token.current().await?;
        build(http, &token)
            .send()
            .await
            .map_err(|error| ClientError::Unavailable(error.to_string()))
    }

    async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T, ClientError> {
        let response = self
            .send(&self.http, |http, token| http.get(url).bearer_auth(token))
            .await?;
        handle_json_response(response).await
    }

    /// `POST url` with a JSON body, decoding the response as `T`.
    async fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        url: &str,
        body: &B,
    ) -> Result<T, ClientError> {
        let response = self
            .send(&self.http, |http, token| {
                http.post(url).bearer_auth(token).json(body)
            })
            .await?;
        handle_json_response(response).await
    }

    /// `POST url` with no body, decoding the response as `T`.
    async fn post_empty<T: DeserializeOwned>(&self, url: &str) -> Result<T, ClientError> {
        let response = self
            .send(&self.http, |http, token| http.post(url).bearer_auth(token))
            .await?;
        handle_json_response(response).await
    }

    async fn put_json<B: Serialize, T: DeserializeOwned>(
        &self,
        url: &str,
        body: &B,
    ) -> Result<T, ClientError> {
        let response = self
            .send(&self.http, |http, token| {
                http.put(url).bearer_auth(token).json(body)
            })
            .await?;
        handle_json_response(response).await
    }

    async fn delete_json<T: DeserializeOwned>(&self, url: &str) -> Result<T, ClientError> {
        let response = self
            .send(&self.http, |http, token| {
                http.delete(url).bearer_auth(token)
            })
            .await?;
        handle_json_response(response).await
    }
}

/// `value` as one path segment: refused when it is empty or could leave
/// its segment (`/`, `?`, `#`, `%`, `..`, whitespace or control characters).
fn path_segment<'a>(what: &str, value: &'a str) -> Result<&'a str, ClientError> {
    let unsafe_char =
        |c: char| matches!(c, '/' | '?' | '#' | '%' | '\\') || c.is_whitespace() || c.is_control();
    if value.is_empty() || value == "." || value == ".." || value.chars().any(unsafe_char) {
        return Err(ClientError::Request {
            code: "invalid_identifier".to_string(),
            message: format!(
                "not a valid {what} name or id: it must not be empty or hold '/', '?', '#', \
                 '%', '\\', whitespace or control characters"
            ),
        });
    }
    Ok(value)
}

/// Checks and normalizes the base URL of [`ApiClient::remote`].
pub(super) fn normalize_base_url(raw: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(raw.trim())
        .map_err(|error| format!("'{raw}' is not a URL: {error}"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("the server URL must not carry credentials".to_string());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("the server URL must not have a query or a fragment".to_string());
    }
    match url.scheme() {
        "https" => {}
        "http" if is_loopback_host(&url) => {}
        "http" => {
            return Err(format!(
                "'{raw}' is plain http to a non-loopback host; the control plane must be \
                 reached over https so the token is never sent in clear text"
            ))
        }
        other => return Err(format!("unsupported URL scheme '{other}'")),
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

fn is_loopback_host(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// What [`ApiClient::follow_control_events`] unfolds over.
struct FollowState {
    client: ApiClient,
    /// Cursor after the last event yielded; the next connection starts here.
    cursor: Option<String>,
    task_id: Option<String>,
    connection:
        Option<futures::stream::BoxStream<'static, Result<ControlEventEnvelope, ClientError>>>,
    /// Whether the current connection yielded an event yet.
    delivered: bool,
    /// Consecutive failed or empty connections; sets the reconnect delay.
    failures: u32,
    done: bool,
}

/// The delay before reconnect attempt number `failures` (0: at once).
pub fn reconnect_delay(failures: u32) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    let factor = 2u32.saturating_pow(failures.saturating_sub(1).min(16));
    FIRST_RECONNECT_DELAY
        .saturating_mul(factor)
        .min(MAX_RECONNECT_DELAY)
}

async fn follow_step(
    mut state: FollowState,
) -> Option<(Result<ControlEventEnvelope, ClientError>, FollowState)> {
    loop {
        if state.done {
            return None;
        }
        if state.connection.is_none() {
            tokio::time::sleep(reconnect_delay(state.failures)).await;
            state.connection = Some(
                state
                    .client
                    .stream_control_events(state.cursor.as_deref(), state.task_id.as_deref())
                    .boxed(),
            );
            state.delivered = false;
        }
        let next = state.connection.as_mut()?.next().await;
        match next {
            Some(Ok(envelope)) => {
                state.cursor = Some(envelope.cursor.clone());
                state.delivered = true;
                state.failures = 0;
                return Some((Ok(envelope), state));
            }
            Some(Err(error @ (ClientError::Unavailable(_) | ClientError::Server(_)))) => {
                state.connection = None;
                state.failures = state.failures.saturating_add(1);
                return Some((Err(error), state));
            }
            Some(Err(error)) => {
                state.done = true;
                return Some((Err(error), state));
            }
            // The server ended the stream (token expiry, a recycled
            // connection): reconnect from the cursor. Immediately after a
            // connection that delivered something, with a growing delay
            // after one that ended without delivering anything, so a server
            // that keeps closing at once is not hammered.
            None => {
                state.connection = None;
                if state.delivered {
                    state.failures = 0;
                } else {
                    state.failures = state.failures.saturating_add(1);
                }
            }
        }
    }
}

/// Maps a response's status to a [`ClientError`] the same way for every
/// call this client makes, `2xx` aside: `401` → [`ClientError::Unauthorized`],
/// `403` → [`ClientError::Forbidden`],
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

/// Distinct material blobs in manifest order, matching the bundle layout.
fn material_blob_order(files: &[MaterialFile]) -> Vec<(String, u64)> {
    let mut seen = std::collections::BTreeSet::new();
    files
        .iter()
        .filter(|file| seen.insert(file.sha256.clone()))
        .map(|file| (file.sha256.clone(), file.size))
        .collect()
}

/// Lowercase hexadecimal SHA-256 used to verify downloaded material blobs.
fn material_sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    use std::fmt::Write as _;

    sha2::Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// One read of the usage limits, ready to be awaited anywhere.
pub type UsageRead = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<HarnessUsageResponse, ClientError>> + Send>,
>;

/// Where the usage limits of the harnesses are read from: the control plane
/// through an [`ApiClient`], or a test double.
pub trait UsageSource: Send + Sync {
    /// Reads the usage limits as [`ApiClient::control_usage`] does.
    fn read_usage(&self) -> UsageRead;
}

impl UsageSource for ApiClient {
    fn read_usage(&self) -> UsageRead {
        let client = self.clone();
        Box::pin(async move { client.control_usage().await })
    }
}

/// One read of the branches of a registered repository, ready to be awaited
/// anywhere.
pub type BranchRead =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<String>, ClientError>> + Send>>;

/// Where the branches of a registered repository are read from: the control
/// plane through an [`ApiClient`], or a test double.
pub trait BranchSource: Send + Sync {
    /// Reads the branches of the repository `id` as
    /// [`ApiClient::control_repository_branches`] does.
    fn read_branches(&self, id: &str) -> BranchRead;
}

impl BranchSource for ApiClient {
    fn read_branches(&self, id: &str) -> BranchRead {
        let (client, id) = (self.clone(), id.to_string());
        Box::pin(async move {
            client
                .control_repository_branches(&id)
                .await
                .map(|response| response.branches)
        })
    }
}

#[cfg(test)]
mod material_transfer_tests {
    use super::{material_blob_order, material_sha256_hex};
    use narsor_protocol::api::MaterialFile;

    #[test]
    fn blob_order_keeps_first_manifest_occurrence() {
        let files = vec![
            MaterialFile {
                path: "a".into(),
                size: 2,
                sha256: "first".into(),
            },
            MaterialFile {
                path: "b".into(),
                size: 3,
                sha256: "second".into(),
            },
            MaterialFile {
                path: "c".into(),
                size: 2,
                sha256: "first".into(),
            },
        ];

        assert_eq!(
            material_blob_order(&files),
            vec![("first".into(), 2), ("second".into(), 3)]
        );
    }

    #[test]
    fn downloaded_blob_digest_uses_lowercase_sha256() {
        assert_eq!(
            material_sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
