//! Handlers for the daemon API and its bearer-token gate.
//!
//! # Why these three shapes, and only these
//!
//! Each endpoint mirrors exactly one already-landed read: events
//! ([`narsor_engine::persistence::EventLog::read_all`], U2/pre-existing), the step
//! timeline ([`narsor_engine::engine::build_timeline`], U4), and a conversation
//! ([`narsor_engine::persistence::ConversationStore::read`], U2). None of them
//! reshapes its answer for the wire: the event contract in the API is
//! identical to the event log's (the plan is explicit that only the
//! transport changes, never the shape), and the timeline's `Serialize` impl
//! lives on the timeline types themselves (`src/engine/timeline.rs`) for the
//! same reason. The one real translation is the conversation response,
//! because [`narsor_engine::persistence::ConversationEntry::raw`] is a `Vec<u8>`
//! that is not always valid UTF-8 (R9's "byte for byte") and JSON strings
//! cannot hold arbitrary bytes; [`ConversationEntryDto`] base64-encodes it,
//! the same trick the store's own on-disk envelope already uses for the
//! same reason.
//!
//! # Why "no conversation record" is `Ok`, not `404`
//!
//! `ConversationStore::read` returning `Ok(None)` means exactly one of: a
//! script step (nothing was ever going to write one), an agent step whose
//! conversation never started, or a step from an old-format log written
//! before U2 existed. None of those is a caller error — the execution and
//! the attempt id are both perfectly real — so the response is a normal
//! `200` carrying [`ConversationResponse::NoRecord`], not an HTTP error. The
//! only thing that *is* a `404` here is an execution the store has never
//! heard of at all.
//!
//! # Why the auth check is hand-rolled instead of `axum-extra`'s typed header
//!
//! `axum-extra`'s `Authorization<Bearer>` extractor is a natural fit for
//! *parsing* the header, but using it as a handler argument means a missing
//! or malformed header fails as an extractor rejection, which axum renders
//! with a short text body describing the problem. The plan requires "no
//! body content" on a refusal — the token itself must not be confirmed or
//! denied by anything the response says beyond the status code — so the
//! check is done by hand, in a `middleware::from_fn_with_state` layer that
//! only ever returns a bare status code. The layer is
//! [`narsor_engine::auth::gate::require_owner`]: the process token in trusted mode,
//! the installation owner's OIDC token in network mode (U7).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;

use crate::api::ApiServices;
use crate::auth_gate as gate;
use crate::auth_gate::OwnerAuth;
use crate::controller_protocol::http as protocol_http;
pub use narsor_engine::api::conversation_response_from_snapshot;
use narsor_engine::api::dto::{
    AcceptedResponse, ActiveProblem, ConductorActivity, ConversationResponse,
    ExecutionListResponse, ExecutionSummary, HarnessSelectionSnapshot, HumanInputDeliveryDto,
    PendingHumanInputView, RequestErrorResponse, SetHarnessSelectionRequest, StartTaskRequest,
    StartTaskResponse,
};
use narsor_engine::conductor::{annotation_of, ConductorStore};
use narsor_engine::controller_protocol::ProtocolError;
use narsor_engine::domain::TaskId;
use narsor_engine::domain::{Event, ExecutionStatus};
use narsor_engine::engine::{current_timeline, EngineError, Timeline};
use narsor_engine::environment::LaunchIntent;
use narsor_engine::launch::CatalogLaunchResolver;
use narsor_engine::orchestration::OwnerRef;
use narsor_engine::persistence::{
    DefaultHarnessSelector, ExecutionStore, HarnessSelectionError,
    HarnessSelectionSnapshot as PersistenceHarnessSelectionSnapshot, PersistenceError,
};
use narsor_engine::registry::{BranchLister, GitBranchLister};
use narsor_engine::tasks::{self, TaskError};
use narsor_engine::workflows::WorkflowResolver;

/// Adapts an in-flight conductor invocation to its protocol summary.
fn activity_of_pending(pending: &narsor_engine::conductor::PendingInvocation) -> ConductorActivity {
    ConductorActivity {
        purpose: pending.request.purpose.into(),
        trigger: pending.request.trigger.into(),
        episode: pending.request.episode.clone(),
        started_at: pending.started_at,
    }
}

/// Adapts a queued conductor request to its protocol summary.
fn activity_of_queued(queued: &narsor_engine::conductor::QueuedRequest) -> ConductorActivity {
    ConductorActivity {
        purpose: queued.purpose.into(),
        trigger: queued.trigger.into(),
        episode: queued.episode.clone(),
        started_at: queued.queued_at,
    }
}

/// Builds the protocol summary for a pending output problem.
fn problem_of_pending(pending: &narsor_engine::engine::PendingHumanInput) -> Option<ActiveProblem> {
    if pending.continuation != narsor_engine::engine::InputContinuation::ResolveOutput {
        return None;
    }
    Some(ActiveProblem {
        kind: narsor_engine::domain::ProblemKind::Output.into(),
        reason: pending
            .problem
            .as_ref()
            .map(|problem| problem.reason.clone())
            .unwrap_or_else(|| pending.instruction().to_string()),
        episode: Some(pending.episode_key().to_string()),
    })
}

/// Projects the durable engine record into the independent client wire view.
fn pending_input_view(
    pending: &narsor_engine::engine::PendingHumanInput,
) -> Result<PendingHumanInputView, ApiError> {
    let serialized = serde_json::to_value(pending)
        .map_err(|error| ApiError::Internal(format!("could not encode pending input: {error}")))?;
    serde_json::from_value(serialized)
        .map_err(|error| ApiError::Internal(format!("invalid pending-input wire view: {error}")))
}

/// Builds the protocol summary for a conductor diagnosis.
fn problem_of_failure(report: &narsor_engine::conductor::FailureReport) -> ActiveProblem {
    ActiveProblem {
        kind: narsor_engine::domain::ProblemKind::Failure.into(),
        reason: report.reason.clone(),
        episode: Some(report.key.clone()),
    }
}

/// Builds activity for a failure diagnosis that the conductor has not started.
fn activity_of_owed_diagnosis(
    failure: &narsor_engine::engine::StepFailure,
    execution: &narsor_engine::domain::WorkflowExecution,
) -> ConductorActivity {
    ConductorActivity {
        purpose: narsor_engine::domain::ConductorPurpose::DiagnoseFailure.into(),
        trigger: narsor_engine::domain::ConductorTrigger::StepFailure.into(),
        episode: Some(failure.key().to_string()),
        started_at: execution.completed_at.unwrap_or_else(chrono::Utc::now),
    }
}

mod control;

/// Default byte budget for a conversation read when the caller does not ask
/// for a specific one.
///
/// Sized to comfortably hold a normal run of harness lines in one response
/// while still bounding it — the plan defers the exact number to
/// implementation time, to be picked from real observed sizes; 1 MiB is a
/// generous starting point a caller can override per request via
/// `byte_budget`, and it is only ever a *soft* cap (the first entry is
/// always returned whole, per `ConversationStore::read`).
const DEFAULT_BYTE_BUDGET: usize = 1024 * 1024;

#[derive(Clone)]
struct AppState {
    catalog_dir: PathBuf,
    work_dir: PathBuf,
    harness_selector: Option<Arc<DefaultHarnessSelector>>,
    control: Option<Arc<narsor_engine::controller_protocol::ProtocolService>>,
    local_launch: Option<Arc<CatalogLaunchResolver>>,
    /// The workflows `GET /control/v1/workflows` lists.
    workflows: WorkflowResolver,
    /// Whether this is the trusted local daemon: only it serves the files
    /// and the executions of its own disk to its owner (U6).
    local: bool,
    /// What a client calls this connection.
    connection_name: String,
    /// Lists the branches of a registered repository's remote.
    branch_lister: Arc<dyn BranchLister>,
}

/// Builds the router: the execution reads (events, timeline, conversation,
/// `GET /executions`), `POST /tasks` and the answers a waiting task takes
/// (`input`, `permission`) — all behind the same owner gate (`auth`: the
/// process token, or the owner's OIDC token). Every execution lives on a
/// backend's storage and is driven by its runner; the daemon drives none.
///
/// The identity routes (`/auth/v1/…`) sit outside it: a caller who is not
/// the owner yet must be able to claim the installation.
///
/// With a control plane in `services` (U2), its owner routes
/// (`/control/v1/…`) join the gated set, while its controller
/// (`/controller/v1/…`) and runner (`/runner/v1/…`) routes are merged after
/// the gate: they authenticate with their own credentials
/// ([`crate::controller_protocol::http`]), never with the owner token.
///
/// `catalog_dir`/`work_dir` are what [`narsor_engine::tasks::start_task`] and
/// workflow-reference resolution ([`WorkflowResolver`]) need — the daemon's
/// own catalogue and work roots (`config::Resolved::catalog_dir`/
/// `work_dir`), now read by the server that runs a task instead of by a CLI
/// process that used to hold them directly.
pub fn router(
    auth: OwnerAuth,
    catalog_dir: PathBuf,
    work_dir: PathBuf,
    services: ApiServices,
) -> Router {
    let control = services.control.clone();
    let local = matches!(auth, OwnerAuth::Trusted { .. });
    let state = AppState {
        workflows: services
            .workflows
            .unwrap_or_else(|| WorkflowResolver::new(catalog_dir.clone())),
        local,
        connection_name: services
            .connection_name
            .unwrap_or_else(|| if local { "local" } else { "remote" }.to_string()),
        catalog_dir,
        work_dir,
        harness_selector: services.harness_selector,
        control: control.clone(),
        local_launch: services.local_launch,
        branch_lister: services
            .branch_lister
            .unwrap_or_else(|| Arc::new(GitBranchLister::default())),
    };
    let mut owner = Router::new()
        .route("/executions/{execution_id}/events", get(get_events))
        .route("/executions/{execution_id}/timeline", get(get_timeline))
        .route(
            "/executions/{execution_id}/attempts/{attempt_id}/conversation",
            get(get_conversation),
        )
        .route("/executions", get(get_executions))
        .route(
            "/settings/agent-harness",
            get(get_harness_selection)
                .put(put_harness_selection)
                .delete(delete_harness_selection),
        )
        .route("/tasks", post(post_start_task))
        .route("/executions/{execution_id}/input", post(post_input))
        .route(
            "/executions/{execution_id}/permission",
            post(post_permission_decision),
        );
    if let Some(control) = &control {
        owner = owner
            .merge(protocol_http::owner_routes(Arc::clone(control)))
            .merge(control::routes());
    }
    // The owner gate covers everything above; controllers and runners are
    // not owners and pass their own credential gates instead (U2).
    let mut router = owner
        .layer(middleware::from_fn_with_state(
            auth.clone(),
            gate::require_owner,
        ))
        .merge(gate::auth_routes(auth));
    if let Some(control) = control {
        router = router
            .merge(protocol_http::controller_routes(Arc::clone(&control)))
            .merge(protocol_http::observer_routes(Arc::clone(&control)))
            .merge(protocol_http::runner_routes(control));
    }
    router.with_state(state)
}

/// Reads the daemon's authoritative default-harness setting.
async fn get_harness_selection(
    State(state): State<AppState>,
) -> Result<Json<HarnessSelectionSnapshot>, ApiError> {
    let selector = require_harness_selector(&state)?;
    Ok(Json(harness_selection_view(selector.snapshot().await)))
}

/// Persists a new default-harness override and returns the post-commit view.
async fn put_harness_selection(
    State(state): State<AppState>,
    Json(request): Json<SetHarnessSelectionRequest>,
) -> Result<Json<HarnessSelectionSnapshot>, ApiError> {
    let selector = require_harness_selector(&state)?;
    Ok(Json(harness_selection_view(
        selector.set(request.harness).await?,
    )))
}

/// Removes the durable override and returns the configured fallback view.
async fn delete_harness_selection(
    State(state): State<AppState>,
) -> Result<Json<HarnessSelectionSnapshot>, ApiError> {
    let selector = require_harness_selector(&state)?;
    Ok(Json(harness_selection_view(selector.reset().await?)))
}

fn require_harness_selector(state: &AppState) -> Result<Arc<DefaultHarnessSelector>, ApiError> {
    state.harness_selector.clone().ok_or_else(|| {
        ApiError::Internal("default harness selector was not configured for this API server".into())
    })
}

/// An error a handler can return; every variant maps to a status code with
/// no informative body, since the only caller is the TUI/CLI client in this
/// same trust boundary and there is nothing a body would add.
enum ApiError {
    ExecutionNotFound,
    /// The request itself is malformed or asks for something the workflow
    /// definition does not allow (an unknown workflow reference, an illegal
    /// transition target). `400`.
    BadRequest(String),
    /// A request error safe to return to a local authenticated client.
    Request {
        code: &'static str,
        message: String,
    },
    /// The request is well-formed but conflicts with the execution's current
    /// state (already being driven, or being transitioned by hand while
    /// active). `409`.
    Conflict(String),
    /// A conflict the operator is told the reason of — a proposal about a
    /// moment that has passed (KTD9), a decision already undone. `409`
    /// with a [`RequestErrorResponse`] body.
    Refused {
        code: &'static str,
        message: String,
    },
    /// Something that should have answered did not: `503` with a reason.
    Unavailable {
        code: &'static str,
        message: String,
    },
    Storage(PersistenceError),
    /// Anything else that stops a handler from completing — mirrors
    /// `Storage`'s `500`, kept distinct only so the log line names the right
    /// source.
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::ExecutionNotFound => StatusCode::NOT_FOUND.into_response(),
            ApiError::BadRequest(message) => {
                tracing::debug!(message, "api: bad request");
                StatusCode::BAD_REQUEST.into_response()
            }
            ApiError::Request { code, message } => (
                StatusCode::BAD_REQUEST,
                Json(RequestErrorResponse {
                    code: code.to_owned(),
                    message,
                }),
            )
                .into_response(),
            ApiError::Conflict(message) => {
                tracing::debug!(message, "api: conflict");
                StatusCode::CONFLICT.into_response()
            }
            ApiError::Refused { code, message } => (
                StatusCode::CONFLICT,
                Json(RequestErrorResponse {
                    code: code.to_owned(),
                    message,
                }),
            )
                .into_response(),
            ApiError::Unavailable { code, message } => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(RequestErrorResponse {
                    code: code.to_owned(),
                    message,
                }),
            )
                .into_response(),
            ApiError::Storage(error) => {
                tracing::warn!(error = %error, "api: storage error serving request");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
            ApiError::Internal(message) => {
                tracing::warn!(message, "api: internal error serving request");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
    }
}

impl From<ProtocolError> for ApiError {
    fn from(error: ProtocolError) -> Self {
        protocol_error_as_api(error)
    }
}

impl From<HarnessSelectionError> for ApiError {
    fn from(error: HarnessSelectionError) -> Self {
        match error {
            HarnessSelectionError::UnsupportedHarness { .. } => ApiError::Request {
                code: "unsupported_harness",
                message: error.to_string(),
            },
            other => ApiError::Internal(other.to_string()),
        }
    }
}

fn harness_selection_view(
    snapshot: PersistenceHarnessSelectionSnapshot,
) -> HarnessSelectionSnapshot {
    HarnessSelectionSnapshot {
        supported: snapshot.supported,
        configured: snapshot.configured,
        override_harness: snapshot.override_harness,
        effective: snapshot.effective,
        revision: snapshot.revision,
    }
}

impl From<PersistenceError> for ApiError {
    fn from(error: PersistenceError) -> Self {
        ApiError::Storage(error)
    }
}

/// Maps an [`EngineError`] the way the plan's U3 Approach specifies:
/// [`EngineError::AlreadyRunning`] → `409`, "the execution does not exist" →
/// `404`, everything else → `500` — refined for human input (U5): an answer
/// that came too late is `409`, one that is invalid is `400` with the reason. Every other variant (`NotRunnable`,
/// `AwaitingHumanInput`, `Decision`, `InvalidDefinition`, ...) is an engine or
/// workflow-author problem the plan does not ask this endpoint surface to
/// distinguish further — it lands on `Internal`, same as a storage failure.
impl From<EngineError> for ApiError {
    fn from(error: EngineError) -> Self {
        match error {
            EngineError::AlreadyRunning { .. }
            | EngineError::NotRunnable { .. }
            | EngineError::AwaitingHumanInput { .. }
            // Somebody acted on the failure first (KTD9).
            | EngineError::StaleEpisode { .. }
            | EngineError::EpisodeChanged { .. } => ApiError::Conflict(error.to_string()),
            EngineError::InvalidTransition { message } => ApiError::BadRequest(message),
            // Too late: the request was answered or closed meanwhile (KTD9).
            EngineError::HumanInput(ref human) if human.is_stale() => {
                ApiError::Conflict(error.to_string())
            }
            // The answer itself is wrong, and the person who sent it can fix
            // it: said with the reason, e.g. the key a label still needs.
            EngineError::HumanInput(ref human) if human.is_invalid_delivery() => {
                ApiError::Request {
                    code: "invalid_input",
                    message: error.to_string(),
                }
            }
            EngineError::Persistence(PersistenceError::ExecutionNotFound { .. }) => {
                ApiError::ExecutionNotFound
            }
            other => {
                tracing::warn!(error = %other, "api: engine error serving request");
                ApiError::Internal(other.to_string())
            }
        }
    }
}

/// Maps a [`TaskError`] (starting a task, `POST /tasks`): a definition that
/// fails static validation is the caller's fault (`400`); everything else
/// (a rejected workspace, a failed clone, a storage error) is `500`.
impl From<TaskError> for ApiError {
    fn from(error: TaskError) -> Self {
        match error {
            TaskError::InvalidDefinition { violations } => {
                ApiError::BadRequest(violations.join("; "))
            }
            other => ApiError::Internal(other.to_string()),
        }
    }
}

fn protocol_error_as_api(error: ProtocolError) -> ApiError {
    match error {
        ProtocolError::NotFound => ApiError::ExecutionNotFound,
        ProtocolError::BadRequest { code, message } => ApiError::Request { code, message },
        ProtocolError::Conflict { code, message } => ApiError::Refused { code, message },
        ProtocolError::Unavailable { code, message } => ApiError::Unavailable { code, message },
        ProtocolError::Unauthorized | ProtocolError::Forbidden => ApiError::ExecutionNotFound,
        ProtocolError::Internal(message) => ApiError::Internal(message),
    }
}

/// Where the daemon-owned local backend keeps its task storages — under the
/// configured work root, which is where task workspaces have always lived.
fn local_storage_root(state: &AppState) -> PathBuf {
    state.work_dir.join("local-controller").join("storage")
}

/// Resolves an execution visible to one owner.  Runner storage is never
/// enumerated as a tenant-visible namespace: the task projection is the
/// authority linking an execution id to its backend-owned storage.
fn owner_execution_store_for(
    state: &AppState,
    owner: &OwnerRef,
    execution_id: &str,
) -> Result<ExecutionStore, ApiError> {
    if !is_safe_id(execution_id) {
        return Err(ApiError::ExecutionNotFound);
    }
    let mut matches = Vec::new();
    let Some(control) = &state.control else {
        return Err(ApiError::ExecutionNotFound);
    };
    let tasks = control.list_tasks(owner).map_err(|error| {
        ApiError::Internal(format!("could not resolve task projection: {error}"))
    })?;
    for task in tasks.tasks {
        let is_execution = task
            .runner
            .as_ref()
            .and_then(|runner| runner.execution_cursor.execution_id.as_deref())
            == Some(execution_id);
        // The embedded local API can read only the daemon-owned local
        // backend. Other backends keep their storage behind their own API.
        if is_execution && task.backend_id.as_str() == "local" {
            let store = ExecutionStore::new(
                local_storage_root(state)
                    .join(task.task_id.as_str())
                    .join("state"),
            );
            if store.exists(execution_id) {
                matches.push(store);
            }
        }
    }
    match matches.len() {
        0 => Err(ApiError::ExecutionNotFound),
        1 => Ok(matches.pop().expect("one owner-visible execution store")),
        _ => Err(ApiError::Conflict(format!(
            "execution '{execution_id}' is ambiguous across backend storage"
        ))),
    }
}

/// Whether `id` is safe to join onto a filesystem path: every id this API
/// ever hands out (execution ids, attempt ids) is one of narsor's own
/// generated identifiers, never a path fragment, so this rejects anything a
/// caller could use to escape the intended directory (`..`, `/`, a bare
/// `.`) before it ever reaches [`narsor_engine::persistence::paths::StatePaths`]'s
/// path-joining. A request with an id that fails this check is treated the
/// same as one for an id that simply does not exist — there is nothing an
/// attacker should learn from the distinction.
fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    #[serde(default)]
    after: u64,
}

/// `GET /executions/{execution_id}/events?after=<sequence>`
///
/// Events with `sequence > after`, in the same shape as the event log
/// itself — the transport changes, the contract does not.
async fn get_events(
    State(state): State<AppState>,
    Extension(owner): Extension<OwnerRef>,
    Path(execution_id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> Result<Json<Vec<Event>>, ApiError> {
    let events = run_blocking(move || {
        let store = owner_execution_store_for(&state, &owner, &execution_id)?;
        read_events(&store, &execution_id, query.after)
    })
    .await?;
    Ok(Json(events))
}

/// The events of `execution_id` with `sequence > after` — shared by
/// `/executions/{id}/events` and the history route of the control API.
fn read_events(
    store: &ExecutionStore,
    execution_id: &str,
    after: u64,
) -> Result<Vec<Event>, ApiError> {
    Ok(store
        .event_log(execution_id)
        .read_after(after, usize::MAX)?
        .0)
}

/// `GET /executions/{execution_id}/timeline`
///
/// The step timeline ([`narsor_engine::engine::build_timeline`]) computed from the
/// execution's event log and its conversation store's presence — the real
/// wiring U4's pure function was left waiting for.
async fn get_timeline(
    State(state): State<AppState>,
    Extension(owner): Extension<OwnerRef>,
    Path(execution_id): Path<String>,
) -> Result<Json<Timeline>, ApiError> {
    let timeline = run_blocking(move || {
        let store = owner_execution_store_for(&state, &owner, &execution_id)?;
        read_timeline(&store, &execution_id)
    })
    .await?;
    Ok(Json(timeline))
}

/// The step timeline of `execution_id`, computed from its own store.
fn read_timeline(store: &ExecutionStore, execution_id: &str) -> Result<Timeline, ApiError> {
    let execution = store.load_execution(execution_id)?;
    let events = store.event_log(execution_id).read_all()?;
    let presence = conversation_presence(store, execution_id)?;
    Ok(current_timeline(
        store,
        execution_id,
        &events,
        execution.status,
        &presence,
    ))
}

/// Every attempt id with a conversation record on disk, open or closed —
/// exactly what [`narsor_engine::engine::build_timeline`] needs as its
/// `conversation_presence` set. Delegates to
/// [`narsor_engine::persistence::ConversationStore::recorded_attempt_ids`] so this
/// handler and the `steps` CLI command can never compute a different answer
/// for the same execution.
fn conversation_presence(
    store: &ExecutionStore,
    execution_id: &str,
) -> Result<HashSet<String>, ApiError> {
    store
        .conversation_store(execution_id)
        .recorded_attempt_ids()
        .map_err(ApiError::Storage)
}

#[derive(Debug, Deserialize)]
struct ConversationQuery {
    #[serde(default = "default_from")]
    from: u64,
    #[serde(default = "default_byte_budget")]
    byte_budget: usize,
}

fn default_from() -> u64 {
    1
}

fn default_byte_budget() -> usize {
    DEFAULT_BYTE_BUDGET
}

/// `GET /executions/{execution_id}/attempts/{attempt_id}/conversation?from=<n>&byte_budget=<bytes>`
async fn get_conversation(
    State(state): State<AppState>,
    Extension(owner): Extension<OwnerRef>,
    Path((execution_id, attempt_id)): Path<(String, String)>,
    Query(query): Query<ConversationQuery>,
) -> Result<Json<ConversationResponse>, ApiError> {
    let execution_store = owner_execution_store_for(&state, &owner, &execution_id)?;
    Ok(Json(read_conversation(
        &execution_store,
        &execution_id,
        &attempt_id,
        query.from,
        query.byte_budget,
    )?))
}

/// One attempt's conversation from `from` on, within `byte_budget` — shared
/// by `/executions/…/conversation` and the control API's history route.
fn read_conversation(
    execution_store: &ExecutionStore,
    execution_id: &str,
    attempt_id: &str,
    from: u64,
    byte_budget: usize,
) -> Result<ConversationResponse, ApiError> {
    if !is_safe_id(attempt_id) {
        return Ok(ConversationResponse::NoRecord);
    }
    let store = execution_store.conversation_store(execution_id);
    let snapshot = store.read(attempt_id, from, byte_budget)?;
    Ok(conversation_response_from_snapshot(snapshot))
}

// ---------------------------------------------------------------------------
// Mutating endpoints (U3)
// ---------------------------------------------------------------------------

/// `POST /tasks` — starts a new execution and drives it in the background.
///
/// Mirrors `narsor run`'s own two steps
/// ([`tasks::start_task`] then [`Engine::run`]), except the run itself is
/// hop-spawned rather than awaited: the plan calls for `202 Accepted` with
/// the new `execution_id` the moment the execution exists, not once it
/// finishes — a workflow can run for as long as its longest human gate.
async fn post_start_task(
    State(state): State<AppState>,
    Extension(owner): Extension<OwnerRef>,
    Json(body): Json<StartTaskRequest>,
) -> Result<(StatusCode, Json<StartTaskResponse>), ApiError> {
    let control = state.control.as_ref().ok_or_else(|| {
        ProtocolError::Internal("local task scheduler is not configured".to_string())
    })?;
    let launch = state.local_launch.as_ref().ok_or_else(|| {
        ProtocolError::Internal("local task resolver is not configured".to_string())
    })?;
    let task_id = TaskId::parse(&body.task_id).map_err(|error| ProtocolError::BadRequest {
        code: "invalid_identifier",
        message: error.to_string(),
    })?;
    let digest = narsor_engine::domain::canonical_sha256(
        &serde_json::to_value(&body).map_err(|error| ProtocolError::Internal(error.to_string()))?,
    );
    if let Some(task) = control.replay_sealed_task(&owner, &task_id, &digest)? {
        return Ok((StatusCode::ACCEPTED, Json(StartTaskResponse { task })));
    }
    let resolved = WorkflowResolver::new(state.catalog_dir.clone())
        .resolve(&body.workflow)
        .map_err(|error| ProtocolError::BadRequest {
            code: "invalid_workflow",
            message: error.to_string(),
        })?;
    let definition =
        tasks::load_resolved(&resolved).map_err(|error| ProtocolError::BadRequest {
            code: "invalid_workflow",
            message: error.to_string(),
        })?;
    let inputs = Value::Object(body.inputs.clone());
    let label_refusal = |error: narsor_engine::domain::LabelError| ProtocolError::BadRequest {
        code: "invalid_task_label",
        message: error.to_string(),
    };
    let ticket = narsor_engine::domain::normalize_label(
        "ticket",
        body.ticket.as_deref(),
        narsor_engine::domain::TICKET_MAX_CHARS,
    )
    .map_err(label_refusal)?;
    let title = narsor_engine::domain::normalize_label(
        "title",
        body.title.as_deref(),
        narsor_engine::domain::TITLE_MAX_CHARS,
    )
    .map_err(label_refusal)?;
    let sealed = launch
        .resolve_definition(
            &LaunchIntent {
                workflow_id: definition.id.clone(),
                workflow_version: definition.version,
                inputs,
                workspaces: body.workspaces.clone(),
                ticket,
                title,
                ..LaunchIntent::default()
            },
            definition,
        )
        .map_err(|error| ProtocolError::BadRequest {
            code: error.code(),
            message: error.to_string(),
        })?;
    let task = control.create_sealed_task(&owner, task_id, digest, sealed)?;

    Ok((StatusCode::ACCEPTED, Json(StartTaskResponse { task })))
}

/// `POST /executions/{execution_id}/input` — delivers a person's answer to
/// an execution that waits for one.
///
/// Whoever calls this is the operator: `answered_by` is not on the wire. The
/// answer goes to the execution's task through the control plane, and its
/// runner takes it on its next poll. An execution no task of the owner runs
/// is `404`; one that waits for nothing is `409`.
async fn post_input(
    State(state): State<AppState>,
    Extension(owner): Extension<OwnerRef>,
    Path(execution_id): Path<String>,
    Json(delivery): Json<HumanInputDeliveryDto>,
) -> Result<(StatusCode, Json<AcceptedResponse>), ApiError> {
    if delivery.permission_decision.is_some() {
        return Err(ApiError::BadRequest(
            "permission decisions must use the dedicated /permission endpoint".into(),
        ));
    }
    submit_to_task(&state, &owner, execution_id, delivery, "input")
}

/// `POST /executions/{execution_id}/permission` — resolves one durable
/// Claude Code permission request, through the execution's task like
/// [`post_input`].
async fn post_permission_decision(
    State(state): State<AppState>,
    Extension(owner): Extension<OwnerRef>,
    Path(execution_id): Path<String>,
    Json(dto): Json<narsor_engine::api::PermissionDecisionRequestDto>,
) -> Result<(StatusCode, Json<AcceptedResponse>), ApiError> {
    let delivery = HumanInputDeliveryDto {
        episode: Some(dto.episode),
        permission_decision: Some(dto.permission_decision),
        ..Default::default()
    };
    submit_to_task(&state, &owner, execution_id, delivery, "permission")
}

/// Hands `delivery` to the task whose runner drives `execution_id`, as the
/// answer to its pending input.
fn submit_to_task(
    state: &AppState,
    owner: &OwnerRef,
    execution_id: String,
    delivery: HumanInputDeliveryDto,
    waiting_for: &str,
) -> Result<(StatusCode, Json<AcceptedResponse>), ApiError> {
    let control = state.control.as_ref().ok_or(ApiError::ExecutionNotFound)?;
    let tasks = control.list_tasks(owner).map_err(protocol_error_as_api)?;
    let runner = tasks
        .tasks
        .into_iter()
        .find_map(|task| {
            let runner = task.runner?;
            (runner.execution_cursor.execution_id.as_deref() == Some(execution_id.as_str()))
                .then_some((task.task_id, runner))
        })
        .ok_or(ApiError::ExecutionNotFound)?;
    let (task_id, runner) = runner;
    let pending_input_id = runner
        .pending_input
        .map(|input| input.id)
        .ok_or_else(|| ApiError::Conflict(format!("execution is not waiting for {waiting_for}")))?;
    control
        .submit_input(
            owner,
            task_id.as_str(),
            narsor_engine::controller_protocol::SubmitInputRequest {
                pending_input_id,
                idempotency_key: uuid::Uuid::new_v4().to_string(),
                delivery,
            },
        )
        .map_err(protocol_error_as_api)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(AcceptedResponse { execution_id }),
    ))
}

/// `GET /executions` — every stored execution, summarised.
///
/// Builds each row with [`build_summary`] rather than the plain
/// `ExecutionSummary::from(&execution)` `post_transition` uses: this is the
/// one read the TUI's list-and-select path (U7) relies on for everything it
/// used to read off `Arc<Engine>`/`ExecutionStore` directly, so it is also
/// the one place `variables`/`pending_input` have to be their freshest,
/// authoritative selves.
async fn get_executions(
    State(state): State<AppState>,
    Extension(owner): Extension<OwnerRef>,
) -> Result<Json<ExecutionListResponse>, ApiError> {
    Ok(Json(
        run_blocking(move || list_executions(&state, &owner)).await?,
    ))
}

/// Runs blocking store reads off the async workers.
async fn run_blocking<T, F>(work: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ApiError> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ApiError::Internal(error.to_string()))?
}

fn list_executions(state: &AppState, owner: &OwnerRef) -> Result<ExecutionListResponse, ApiError> {
    let mut executions = std::collections::BTreeMap::new();
    // An execution URL names no backend. Do not advertise an ID for which a
    // later direct read would be ambiguous.
    let mut ambiguous = std::collections::BTreeSet::new();
    if let Some(control) = &state.control {
        let tasks = control.list_tasks(owner).map_err(|error| {
            ApiError::Internal(format!("could not resolve task projection: {error}"))
        })?;
        for task in tasks.tasks {
            let Some(execution_id) = task
                .runner
                .as_ref()
                .and_then(|runner| runner.execution_cursor.execution_id.as_deref())
            else {
                continue;
            };
            if task.backend_id.as_str() != "local" || !ambiguous.insert(execution_id.to_string()) {
                executions.remove(execution_id);
                continue;
            }
            let store = ExecutionStore::new(
                local_storage_root(state)
                    .join(task.task_id.as_str())
                    .join("state"),
            );
            if !store.exists(execution_id) {
                continue;
            }
            let execution = store.load_execution(execution_id)?;
            executions.insert(execution_id.to_string(), build_summary(&store, &execution)?);
        }
    }
    Ok(ExecutionListResponse {
        executions: executions.into_values().collect(),
    })
}

/// An [`ExecutionSummary`] carrying the freshest context variables
/// ([`ExecutionStore::load_context`], falling back to the execution's own
/// embedded copy exactly as `narsor show` does) and the real pending-input
/// record ([`Engine::pending_input`]'s own store read,
/// `ExecutionStore::load_pending_input`), rather than the bare
/// `ExecutionSummary::from(&execution)` conversion. See
/// [`ExecutionSummary`]'s own doc for why a client with neither an `Engine`
/// nor an `ExecutionStore` (the TUI, since U7) needs both of these on every
/// row of `GET /executions` and not just the fields the wire shape started
/// with.
fn build_summary(
    store: &ExecutionStore,
    execution: &narsor_engine::domain::WorkflowExecution,
) -> Result<ExecutionSummary, ApiError> {
    let mut summary = ExecutionSummary::from(execution);
    summary.variables = store
        .load_context(&execution.id)
        .map(|context| context.variables)
        .unwrap_or_else(|_| execution.context.variables.clone());
    let pending_input = if execution.status == ExecutionStatus::InputNeeded {
        store
            .load_pending_input::<narsor_engine::engine::PendingHumanInput>(&execution.id)
            .unwrap_or(None)
    } else {
        None
    };
    summary.annotation = pending_input
        .as_ref()
        .and_then(|pending| annotation_of(store, &execution.id, pending))
        .map(Into::into);
    summary.problem = match &pending_input {
        Some(pending) => problem_of_pending(pending),
        // A failed step stays `Failed` (KTD4): its problem is the
        // conductor's report, while the execution is still failed on it.
        None => narsor_engine::conductor::active_failure_problem(store, execution)
            .as_ref()
            .map(problem_of_failure),
    };
    summary.pending_input = pending_input.as_ref().map(pending_input_view).transpose()?;
    summary.conductor = conductor_activity(store, execution);
    Ok(summary)
}

/// What the conductor of `execution` is working on (R11): the invocation
/// under way, else the first request waiting its turn, else the diagnosis
/// a step failure still owes — the engine records the failure before the
/// conductor's own marker is written, so a list read in between still
/// shows that the conductor takes it up. `None` when it has nothing to do.
fn conductor_activity(
    store: &ExecutionStore,
    execution: &narsor_engine::domain::WorkflowExecution,
) -> Option<ConductorActivity> {
    let conductor = ConductorStore::new(store.paths(), &execution.id);
    let state = if conductor.exists() {
        Some(conductor.load_state().ok()?)
    } else {
        None
    };
    if let Some(state) = &state {
        if let Some(pending) = &state.pending {
            return Some(activity_of_pending(pending));
        }
        if let Some(queued) = state.queue.first() {
            return Some(activity_of_queued(queued));
        }
    }
    let failure = narsor_engine::engine::step_failure(execution)
        .ok()
        .flatten()?;
    let concluded = state
        .as_ref()
        .and_then(|state| state.failure.as_ref())
        .is_some_and(|report| report.key == failure.key());
    (!concluded).then(|| activity_of_owed_diagnosis(&failure, execution))
}
