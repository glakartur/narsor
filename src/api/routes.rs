//! Handlers for the daemon API and its bearer-token gate.
//!
//! # Why these three shapes, and only these
//!
//! Each endpoint mirrors exactly one already-landed read: events
//! ([`crate::persistence::EventLog::read_all`], U2/pre-existing), the step
//! timeline ([`crate::engine::build_timeline`], U4), and a conversation
//! ([`crate::persistence::ConversationStore::read`], U2). None of them
//! reshapes its answer for the wire: the event contract in the API is
//! identical to the event log's (the plan is explicit that only the
//! transport changes, never the shape), and the timeline's `Serialize` impl
//! lives on the timeline types themselves (`src/engine/timeline.rs`) for the
//! same reason. The one real translation is the conversation response,
//! because [`crate::persistence::ConversationEntry::raw`] is a `Vec<u8>`
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
//! [`crate::auth::gate::require_owner`]: the process token in trusted mode,
//! the installation owner's OIDC token in network mode (U7).

use std::collections::HashSet;
use std::convert::Infallible;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::broadcast;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};

use crate::api::active::ActiveTasks;
use crate::api::dto::{
    AcceptedResponse, ActiveProblem, ChatMessageRequest, ConductorActivity, ConductorChatResponse,
    ConductorStateResponse, ExecutionListResponse, ExecutionSummary, HarnessSelectionSnapshot,
    OperatorNoteRequest, RecoveryRequest, RequestErrorResponse, SetHarnessSelectionRequest,
    StartTaskRequest, StartTaskResponse, TransitionRequest,
};
use crate::api::ApiServices;
use crate::auth::gate::{self, OwnerAuth};
use crate::conductor::{
    annotation_of, ChatEntry, ConductorRegistry, ConductorStore, ProposalError, ProposalOutcome,
    UndoError, UndoOutcome,
};
use crate::controller_protocol::http as protocol_http;
use crate::controller_protocol::ProtocolError;
use crate::domain::TaskId;
use crate::domain::{Event, ExecutionStatus};
use crate::engine::{
    build_timeline, Engine, EngineError, HumanInputDelivery, ManualTransition, Recovery, Timeline,
};
use crate::environment::{CatalogLaunchResolver, LaunchIntent};
use crate::orchestration::OwnerRef;
use crate::persistence::{
    ConversationSource, DefaultHarnessSelector, ExecutionStore, HarnessSelectionError,
    HarnessSelectionSnapshot as PersistenceHarnessSelectionSnapshot, PersistenceError,
};
use crate::tasks::{self, TaskError};
use crate::workflows::WorkflowResolver;

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
    engine: Arc<Engine>,
    store: ExecutionStore,
    catalog_dir: PathBuf,
    work_dir: PathBuf,
    harness_selector: Option<Arc<DefaultHarnessSelector>>,
    active_tasks: ActiveTasks,
    conductor: Option<Arc<ConductorRegistry>>,
    control: Option<Arc<crate::controller_protocol::ProtocolService>>,
    local_launch: Option<Arc<CatalogLaunchResolver>>,
}

/// Builds the router: the three original read-only endpoints (U5/U2) plus
/// the mutating surface U3 adds — `POST /tasks`, `POST
/// /executions/:id/resume`, `POST /executions/:id/input`, `POST
/// /executions/:id/transition` and `GET /executions` — all behind the same
/// owner gate (`auth`: the process token, or the owner's OIDC token).
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
/// `catalog_dir`/`work_dir` are what [`crate::tasks::start_task`] and
/// workflow-reference resolution ([`WorkflowResolver`]) need — the daemon's
/// own catalogue and work roots (`config::Resolved::catalog_dir`/
/// `work_dir`), now read by the server that runs a task instead of by a CLI
/// process that used to hold them directly.
pub fn router(
    engine: Arc<Engine>,
    store: ExecutionStore,
    auth: OwnerAuth,
    catalog_dir: PathBuf,
    work_dir: PathBuf,
    services: ApiServices,
) -> Router {
    let control = services.control.clone();
    let state = AppState {
        engine,
        store,
        catalog_dir,
        work_dir,
        harness_selector: services.harness_selector,
        active_tasks: services.active_tasks,
        conductor: services.conductor,
        control: control.clone(),
        local_launch: services.local_launch,
    };
    let mut owner = Router::new()
        .route("/executions/:execution_id/events", get(get_events))
        .route("/executions/:execution_id/timeline", get(get_timeline))
        .route(
            "/executions/:execution_id/attempts/:attempt_id/conversation",
            get(get_conversation),
        )
        .route("/executions", get(get_executions))
        .route(
            "/executions/:execution_id/conductor",
            get(get_conductor_state),
        )
        .route(
            "/executions/:execution_id/conductor/chat",
            get(get_conductor_chat).post(post_conductor_chat),
        )
        .route(
            "/executions/:execution_id/conductor/proposals/:proposal_id/confirm",
            post(post_confirm_proposal),
        )
        .route(
            "/executions/:execution_id/conductor/proposals/:proposal_id/reject",
            post(post_reject_proposal),
        )
        .route(
            "/executions/:execution_id/conductor/decisions/:decision_id/undo",
            post(post_undo_decision),
        )
        .route(
            "/settings/agent-harness",
            get(get_harness_selection)
                .put(put_harness_selection)
                .delete(delete_harness_selection),
        )
        .route(
            "/executions/:execution_id/stream",
            get(get_execution_stream),
        )
        .route("/events/stream", get(get_global_stream))
        .route("/tasks", post(post_start_task))
        .route("/executions/:execution_id/resume", post(post_resume))
        .route("/executions/:execution_id/recover", post(post_recover))
        .route(
            "/executions/:execution_id/operator-note",
            post(post_operator_note),
        )
        .route("/executions/:execution_id/interrupt", post(post_interrupt))
        .route("/executions/:execution_id/input", post(post_input))
        .route(
            "/executions/:execution_id/transition",
            post(post_transition),
        );
    if let Some(control) = &control {
        owner = owner.merge(protocol_http::owner_routes(Arc::clone(control)));
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
            .merge(protocol_http::runner_routes(control));
    }
    router.with_state(state)
}

/// Reads the daemon's authoritative default-harness setting.
async fn get_harness_selection(
    State(state): State<AppState>,
) -> Result<Json<HarnessSelectionSnapshot>, ApiError> {
    let selector = require_harness_selector(&state)?;
    Ok(Json(selector.snapshot().await.into()))
}

/// Persists a new default-harness override and returns the post-commit view.
async fn put_harness_selection(
    State(state): State<AppState>,
    Json(request): Json<SetHarnessSelectionRequest>,
) -> Result<Json<HarnessSelectionSnapshot>, ApiError> {
    let selector = require_harness_selector(&state)?;
    Ok(Json(selector.set(request.harness).await?.into()))
}

/// Removes the durable override and returns the configured fallback view.
async fn delete_harness_selection(
    State(state): State<AppState>,
) -> Result<Json<HarnessSelectionSnapshot>, ApiError> {
    let selector = require_harness_selector(&state)?;
    Ok(Json(selector.reset().await?.into()))
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

impl From<PersistenceHarnessSelectionSnapshot> for HarnessSelectionSnapshot {
    fn from(snapshot: PersistenceHarnessSelectionSnapshot) -> Self {
        Self {
            supported: snapshot.supported,
            configured: snapshot.configured,
            override_harness: snapshot.override_harness,
            effective: snapshot.effective,
            revision: snapshot.revision,
        }
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
        ProtocolError::Unauthorized | ProtocolError::Forbidden => ApiError::ExecutionNotFound,
        ProtocolError::Internal(message) => ApiError::Internal(message),
    }
}

/// Where the daemon-owned local backend keeps its task storages — under the
/// configured work root, which is where task workspaces have always lived.
fn local_storage_root(state: &AppState) -> PathBuf {
    state.work_dir.join("local-controller").join("storage")
}

fn execution_store_for(state: &AppState, execution_id: &str) -> Result<ExecutionStore, ApiError> {
    if !is_safe_id(execution_id) {
        return Err(ApiError::ExecutionNotFound);
    }
    let matches = execution_stores(state)?
        .into_iter()
        .filter(|store| store.exists(execution_id))
        .collect::<Vec<_>>();
    match matches.len() {
        0 => Err(ApiError::ExecutionNotFound),
        1 => Ok(matches
            .into_iter()
            .next()
            .expect("one execution store match")),
        _ => Err(ApiError::Conflict(format!(
            "execution '{execution_id}' is ambiguous across backend storage"
        ))),
    }
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
    if state.store.exists(execution_id) {
        matches.push(state.store.clone());
    }
    let Some(control) = &state.control else {
        return match matches.len() {
            0 => Err(ApiError::ExecutionNotFound),
            1 => Ok(matches.pop().expect("one legacy execution store")),
            _ => unreachable!("one legacy store can contribute at most once"),
        };
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
            "execution '{execution_id}' is ambiguous across legacy and backend storage"
        ))),
    }
}

/// Every local execution store the compatibility routes may inspect.
///
/// Runner storage is deliberately discovered from the backend's durable root:
/// a queued task has no execution storage yet.  A missing local backend is
/// normal for a fresh installation; any other directory-read error is not
/// silently mistaken for a missing execution.
fn execution_stores(state: &AppState) -> Result<Vec<ExecutionStore>, ApiError> {
    let mut stores = vec![state.store.clone()];
    let storage_root = local_storage_root(state);
    let entries = match std::fs::read_dir(&storage_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(stores),
        Err(error) => {
            return Err(ApiError::Internal(format!(
                "could not read runner storage '{}': {error}",
                storage_root.display()
            )))
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| {
            ApiError::Internal(format!(
                "could not enumerate runner storage '{}': {error}",
                storage_root.display()
            ))
        })?;
        stores.push(ExecutionStore::new(entry.path().join("state")));
    }
    Ok(stores)
}

fn require_execution(state: &AppState, execution_id: &str) -> Result<(), ApiError> {
    execution_store_for(state, execution_id).map(|_| ())
}

/// Legacy execution mutations are driven by the daemon's in-process engine.
/// Runner-owned executions must instead be controlled through their task
/// protocol; accepting them here would read a different store and fail later.
fn require_legacy_execution(state: &AppState, execution_id: &str) -> Result<(), ApiError> {
    if is_safe_id(execution_id) && state.store.exists(execution_id) {
        Ok(())
    } else if execution_store_for(state, execution_id).is_ok() {
        Err(ApiError::Conflict(format!(
            "execution '{execution_id}' is runner-owned; use its task lifecycle operation"
        )))
    } else {
        Err(ApiError::ExecutionNotFound)
    }
}

/// Whether `id` is safe to join onto a filesystem path: every id this API
/// ever hands out (execution ids, attempt ids) is one of narsor's own
/// generated identifiers, never a path fragment, so this rejects anything a
/// caller could use to escape the intended directory (`..`, `/`, a bare
/// `.`) before it ever reaches [`crate::persistence::paths::StatePaths`]'s
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

/// `GET /executions/:execution_id/events?after=<sequence>`
///
/// Events with `sequence > after`, in the same shape as the event log
/// itself — the transport changes, the contract does not.
async fn get_events(
    State(state): State<AppState>,
    Extension(owner): Extension<OwnerRef>,
    Path(execution_id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> Result<Json<Vec<Event>>, ApiError> {
    let store = owner_execution_store_for(&state, &owner, &execution_id)?;
    let events = store.event_log(&execution_id).read_all()?;
    let events = events
        .into_iter()
        .filter(|event| event.sequence > query.after)
        .collect();
    Ok(Json(events))
}

/// `GET /executions/:execution_id/conductor`
///
/// Read straight from the conductor's store (D55): its state file is
/// replaced atomically and its chat is append-only, so a reader needs no
/// part in the actor's serialization. An execution without a `conductor/`
/// directory answers with the empty state.
async fn get_conductor_state(
    State(state): State<AppState>,
    Path(execution_id): Path<String>,
) -> Result<Json<ConductorStateResponse>, ApiError> {
    require_legacy_execution(&state, &execution_id)?;
    let conductor = ConductorStore::new(state.store.paths(), &execution_id);
    let unanswered = conductor
        .unanswered_chat()?
        .into_iter()
        .map(|entry| entry.id)
        .collect();
    Ok(Json(ConductorStateResponse::new(
        conductor.load_state()?,
        unanswered,
    )))
}

/// `GET /executions/:execution_id/conductor/chat?after=<sequence>`
///
/// The chat history (R26) after `after`, in the order it was written.
async fn get_conductor_chat(
    State(state): State<AppState>,
    Path(execution_id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> Result<Json<ConductorChatResponse>, ApiError> {
    require_legacy_execution(&state, &execution_id)?;
    let conductor = ConductorStore::new(state.store.paths(), &execution_id);
    Ok(Json(ConductorChatResponse {
        entries: conductor.read_chat_after(query.after)?,
    }))
}

/// The conductor of this daemon, for the endpoints only it can serve.
fn require_conductor(state: &AppState) -> Result<Arc<ConductorRegistry>, ApiError> {
    state
        .conductor
        .clone()
        .ok_or_else(|| ApiError::Internal("no conductor runs behind this API server".into()))
}

/// `POST /executions/:execution_id/conductor/chat` — the operator writes to
/// the conductor (F4).
///
/// Answers `202` with the entry as written, at once: the conductor's answer
/// comes later, as further entries of the same history (`GET .../chat`),
/// whichever client wrote the message.
async fn post_conductor_chat(
    State(state): State<AppState>,
    Path(execution_id): Path<String>,
    Json(body): Json<ChatMessageRequest>,
) -> Result<(StatusCode, Json<ChatEntry>), ApiError> {
    require_legacy_execution(&state, &execution_id)?;
    let conductor = require_conductor(&state)?;
    let text = body.text.trim();
    if text.is_empty() {
        return Err(ApiError::Request {
            code: "empty_message",
            message: "a chat message cannot be empty".to_string(),
        });
    }
    let entry = conductor.post_chat(&execution_id, text)?;
    Ok((StatusCode::ACCEPTED, Json(entry)))
}

/// `POST /executions/:execution_id/conductor/proposals/:proposal_id/confirm`
/// — carries out a proposal of the conductor's (R28).
///
/// Idempotent: a second confirmation answers the first one's outcome with
/// `repeated: true` and does nothing. A proposal about a moment the
/// execution has moved on from is `409` with the reason (KTD9). When
/// carrying it out starts a pass, the pass runs in the background like any
/// other and can be interrupted.
async fn post_confirm_proposal(
    State(state): State<AppState>,
    Path((execution_id, proposal_id)): Path<(String, String)>,
) -> Result<Json<ProposalOutcome>, ApiError> {
    require_legacy_execution(&state, &execution_id)?;
    let conductor = require_conductor(&state)?;
    let execution = state.store.load_execution(&execution_id)?;
    let definition = load_definition_for(&state, &execution)?;

    let confirmed = conductor.confirm_proposal(&definition, &execution_id, &proposal_id)?;
    if let Some(pass) = confirmed.pass {
        spawn_active(&state, execution_id.clone(), async move {
            let _ = pass.run_to_completion(&definition).await;
        });
    }
    Ok(Json(confirmed.outcome))
}

/// `POST /executions/:execution_id/conductor/proposals/:proposal_id/reject`
/// — the operator turns a proposal down; it can no longer be confirmed.
async fn post_reject_proposal(
    State(state): State<AppState>,
    Path((execution_id, proposal_id)): Path<(String, String)>,
) -> Result<Json<ProposalOutcome>, ApiError> {
    require_legacy_execution(&state, &execution_id)?;
    let conductor = require_conductor(&state)?;
    Ok(Json(
        conductor.reject_proposal(&execution_id, &proposal_id)?,
    ))
}

/// `POST /executions/:execution_id/conductor/decisions/:decision_id/undo` —
/// undoes a chosen output of the conductor's (KTD13).
///
/// A pass driving the execution is interrupted first; the answer carries a
/// warning when a step after the decision had already started. Anything but
/// a chosen output is `400`; a decision already undone is `409`.
async fn post_undo_decision(
    State(state): State<AppState>,
    Path((execution_id, decision_id)): Path<(String, String)>,
) -> Result<Json<UndoOutcome>, ApiError> {
    require_legacy_execution(&state, &execution_id)?;
    let conductor = require_conductor(&state)?;
    let execution = state.store.load_execution(&execution_id)?;
    let definition = load_definition_for(&state, &execution)?;

    let active_tasks = state.active_tasks.clone();
    let interrupted = execution_id.clone();
    let outcome = conductor
        .undo_decision(&definition, &execution_id, &decision_id, move || {
            if let Some(handle) = active_tasks.get(&interrupted) {
                handle.abort();
            }
        })
        .await?;
    Ok(Json(outcome))
}

impl From<ProposalError> for ApiError {
    fn from(error: ProposalError) -> Self {
        match error {
            ProposalError::NotFound(_) => ApiError::ExecutionNotFound,
            ProposalError::Stale(message) => ApiError::Refused {
                code: "stale_proposal",
                message,
            },
            ProposalError::Settled(message) => ApiError::Refused {
                code: "proposal_settled",
                message,
            },
            ProposalError::Busy(message) => ApiError::Refused {
                code: "execution_busy",
                message,
            },
            ProposalError::Invalid(message) => ApiError::Request {
                code: "invalid_proposal",
                message,
            },
            ProposalError::Failed(message) => ApiError::Internal(message),
        }
    }
}

impl From<UndoError> for ApiError {
    fn from(error: UndoError) -> Self {
        match error {
            UndoError::NotFound(_) => ApiError::ExecutionNotFound,
            UndoError::NotUndoable(message) => ApiError::Request {
                code: "not_undoable",
                message,
            },
            UndoError::AlreadyUndone(message) => ApiError::Refused {
                code: "already_undone",
                message,
            },
            UndoError::Superseded(message) => ApiError::Refused {
                code: "decision_superseded",
                message,
            },
            UndoError::Cancelled(message) => ApiError::Refused {
                code: "execution_cancelled",
                message,
            },
            UndoError::Busy(message) => ApiError::Refused {
                code: "execution_busy",
                message,
            },
            UndoError::Failed(message) => ApiError::Internal(message),
        }
    }
}

/// `GET /executions/:execution_id/timeline`
///
/// The step timeline ([`crate::engine::build_timeline`]) computed from the
/// execution's event log and its conversation store's presence — the real
/// wiring U4's pure function was left waiting for.
async fn get_timeline(
    State(state): State<AppState>,
    Extension(owner): Extension<OwnerRef>,
    Path(execution_id): Path<String>,
) -> Result<Json<Timeline>, ApiError> {
    let store = owner_execution_store_for(&state, &owner, &execution_id)?;
    let execution = store.load_execution(&execution_id)?;
    let events = store.event_log(&execution_id).read_all()?;
    let presence = conversation_presence(&store, &execution_id)?;
    let timeline = build_timeline(&events, execution.status, &presence);
    Ok(Json(timeline))
}

/// Every attempt id with a conversation record on disk, open or closed —
/// exactly what [`crate::engine::build_timeline`] needs as its
/// `conversation_presence` set. Delegates to
/// [`crate::persistence::ConversationStore::recorded_attempt_ids`] so this
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

/// One decoded conversation entry, wire shape.
///
/// `raw` is base64 rather than a JSON string of the decoded bytes: the raw
/// harness line is preserved byte for byte (R9) and is not guaranteed to be
/// valid UTF-8, so it cannot be spliced into a JSON string unescaped — the
/// same reasoning the on-disk envelope already follows
/// (`src/persistence/conversation.rs`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationEntryDto {
    /// Sequential entry number, starting at 1.
    pub entry_number: u64,
    /// When narsor observed (or produced) this line.
    pub timestamp: DateTime<Utc>,
    /// Which stream this line came from.
    pub source: ConversationSource,
    /// Base64 of the raw bytes.
    pub raw_base64: String,
}

/// The wire response of the conversation endpoint.
///
/// `NoRecord` is a normal, successful answer (see the module doc) rather
/// than an error variant, precisely so a caller cannot mistake "this step
/// has no conversation" for "something went wrong".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ConversationResponse {
    /// A conversation record exists — possibly still being written.
    Recorded {
        /// Entries from the requested cursor onward, in order.
        entries: Vec<ConversationEntryDto>,
        /// The cursor to pass on the next read to continue exactly here.
        next_entry: u64,
        /// Whether the attempt is closed (compressed).
        closed: bool,
        /// Whether the on-disk file's last line was incomplete when read.
        truncated_tail: bool,
    },
    /// No conversation record exists for this attempt: a non-agent step, an
    /// agent step whose conversation never started, or an old-format log.
    NoRecord,
}

/// `GET /executions/:execution_id/attempts/:attempt_id/conversation?from=<n>&byte_budget=<bytes>`
async fn get_conversation(
    State(state): State<AppState>,
    Extension(owner): Extension<OwnerRef>,
    Path((execution_id, attempt_id)): Path<(String, String)>,
    Query(query): Query<ConversationQuery>,
) -> Result<Json<ConversationResponse>, ApiError> {
    let execution_store = owner_execution_store_for(&state, &owner, &execution_id)?;
    if !is_safe_id(&attempt_id) {
        return Ok(Json(ConversationResponse::NoRecord));
    }
    let store = execution_store.conversation_store(&execution_id);
    let snapshot = store.read(&attempt_id, query.from, query.byte_budget)?;

    let response = match snapshot {
        Some(snapshot) => ConversationResponse::Recorded {
            entries: snapshot
                .entries
                .into_iter()
                .map(|entry| ConversationEntryDto {
                    entry_number: entry.entry_number,
                    timestamp: entry.timestamp,
                    source: entry.source,
                    raw_base64: base64_encode(&entry.raw),
                })
                .collect(),
            next_entry: snapshot.next_entry,
            closed: snapshot.closed,
            truncated_tail: snapshot.truncated_tail,
        },
        None => ConversationResponse::NoRecord,
    };

    Ok(Json(response))
}

fn base64_encode(raw: &[u8]) -> String {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    STANDARD.encode(raw)
}

// ---------------------------------------------------------------------------
// Mutating endpoints (U3)
// ---------------------------------------------------------------------------

/// The definition an execution was started from, re-resolved the way `narsor
/// resume`/`input` already do it ([`tasks::resolve_definition`]): from the
/// path or workflow name pinned in the execution's own metadata, never from a
/// caller-supplied override — there is no REST equivalent of the CLI's
/// `--file` here, since a client of this API has no local filesystem
/// relationship to the daemon's catalogue to point at.
fn load_definition_for(
    state: &AppState,
    execution: &crate::domain::WorkflowExecution,
) -> Result<crate::domain::WorkflowDefinition, ApiError> {
    let resolved = tasks::resolve_definition(&state.catalog_dir, state.store.paths(), execution)
        .map_err(|error| ApiError::Internal(error.to_string()))?;
    resolved
        .load()
        .map_err(|error| ApiError::Internal(error.to_string()))
}

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
) -> Result<(StatusCode, Json<StartTaskResponse>), ProtocolError> {
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
    let digest = crate::domain::canonical_sha256(
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
    let sealed = launch
        .resolve_definition(
            &LaunchIntent {
                workflow_id: definition.id.clone(),
                workflow_version: definition.version,
                inputs,
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

/// `POST /executions/:execution_id/resume` — resumes a paused execution.
///
/// Phase 1 ([`Engine::try_begin_resume`]) runs synchronously: if another pass
/// already holds this execution, this returns `409` immediately, with
/// nothing spawned. On success, phase 2
/// ([`crate::engine::RunHandle::run_to_completion`]) is handed to
/// `tokio::spawn` and the handler answers `202` right away.
async fn post_resume(
    State(state): State<AppState>,
    Path(execution_id): Path<String>,
) -> Result<(StatusCode, Json<AcceptedResponse>), ApiError> {
    require_legacy_execution(&state, &execution_id)?;
    let execution = state.store.load_execution(&execution_id)?;
    let definition = load_definition_for(&state, &execution)?;

    let handle = state.engine.try_begin_resume(&execution_id)?;
    spawn_active(&state, execution_id.clone(), async move {
        let _ = handle.run_to_completion(&definition).await;
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(AcceptedResponse { execution_id }),
    ))
}

/// `POST /executions/:execution_id/recover` — explicitly starts a new attempt
/// from the failed or interrupted state. This is separate from `/resume` so a
/// crash is never replayed merely because the daemon restarted.
async fn post_recover(
    State(state): State<AppState>,
    Path(execution_id): Path<String>,
    Json(request): Json<RecoveryRequest>,
) -> Result<(StatusCode, Json<AcceptedResponse>), ApiError> {
    require_legacy_execution(&state, &execution_id)?;
    let execution = state.store.load_execution(&execution_id)?;
    let definition = load_definition_for(&state, &execution)?;

    let handle = state.engine.try_begin_recover_with(
        &execution_id,
        Recovery::by_operator(request.comment, request.episode),
    )?;
    spawn_active(&state, execution_id.clone(), async move {
        let _ = handle.run_to_completion(&definition).await;
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(AcceptedResponse { execution_id }),
    ))
}

/// Stores an operator instruction for the next agent attempt. Terminal
/// executions are valid targets; an active run must be interrupted first.
async fn post_operator_note(
    State(state): State<AppState>,
    Path(execution_id): Path<String>,
    Json(body): Json<OperatorNoteRequest>,
) -> Result<(StatusCode, Json<AcceptedResponse>), ApiError> {
    require_legacy_execution(&state, &execution_id)?;
    if state.engine.is_active(&execution_id) {
        return Err(ApiError::Conflict(format!(
            "execution '{execution_id}' is being driven in the background right now"
        )));
    }
    if body.note.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "operator note cannot be empty".to_string(),
        ));
    }
    let mut execution = state.store.load_execution(&execution_id)?;
    let metadata = execution
        .metadata
        .as_object_mut()
        .ok_or_else(|| ApiError::Internal("execution metadata is not an object".to_string()))?;
    let engine = metadata
        .entry("engine")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| ApiError::Internal("engine metadata is not an object".to_string()))?;
    engine.insert(
        "operator_recovery_note".to_string(),
        Value::String(body.note),
    );
    state.store.save_execution(&execution)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(AcceptedResponse { execution_id }),
    ))
}

/// Aborts the daemon task that owns an active workflow pass. The executor's
/// child-process guard kills an in-flight harness when that task is dropped.
async fn post_interrupt(
    State(state): State<AppState>,
    Path(execution_id): Path<String>,
) -> Result<Json<AcceptedResponse>, ApiError> {
    require_legacy_execution(&state, &execution_id)?;
    if !state.engine.is_active(&execution_id) {
        return Err(ApiError::Conflict(format!(
            "execution '{execution_id}' is not being processed"
        )));
    }
    let handle = state.active_tasks.get(&execution_id).ok_or_else(|| {
        ApiError::Conflict(format!("execution '{execution_id}' cannot be interrupted"))
    })?;
    handle.abort();
    Ok(Json(AcceptedResponse { execution_id }))
}

fn spawn_active<F>(state: &AppState, execution_id: String, future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    state.active_tasks.spawn(execution_id, future);
}

/// `POST /executions/:execution_id/input` — delivers a person's answer to a
/// paused execution.
///
/// The body is a [`HumanInputDelivery`] verbatim — it already derives
/// `Serialize`/`Deserialize` for exactly this reason, so no separate DTO
/// duplicates its shape. Same two-phase split as [`post_resume`].
///
/// Whoever calls this is the operator: `answered_by` is not on the wire.
/// A delivery naming the `episode` of a request that is no longer pending
/// is `409`, checked by `human::resume` on the same read it answers from, so
/// of two concurrent answers to one request exactly one is accepted. An
/// invalid answer is `400` with the reason.
async fn post_input(
    State(state): State<AppState>,
    Extension(owner): Extension<OwnerRef>,
    Path(execution_id): Path<String>,
    Json(delivery): Json<HumanInputDelivery>,
) -> Result<(StatusCode, Json<AcceptedResponse>), ApiError> {
    // Runner-owned executions live on backend storage. Route their answer
    // through the task protocol so the runner consumes it on its next poll;
    // the legacy in-process engine path below remains for historic work.
    if let Some(control) = &state.control {
        let tasks = control.list_tasks(&owner).map_err(protocol_error_as_api)?;
        if let Some(task) = tasks.tasks.into_iter().find(|task| {
            task.runner
                .as_ref()
                .and_then(|runner| runner.execution_cursor.execution_id.as_deref())
                == Some(execution_id.as_str())
        }) {
            let pending_input_id = task
                .runner
                .as_ref()
                .and_then(|runner| runner.pending_input.as_ref())
                .map(|input| input.id.clone())
                .ok_or_else(|| ApiError::Conflict("execution is not waiting for input".into()))?;
            control
                .submit_input(
                    &owner,
                    task.task_id.as_str(),
                    crate::controller_protocol::SubmitInputRequest {
                        pending_input_id,
                        idempotency_key: uuid::Uuid::new_v4().to_string(),
                        delivery,
                    },
                )
                .map_err(protocol_error_as_api)?;
            return Ok((
                StatusCode::ACCEPTED,
                Json(AcceptedResponse { execution_id }),
            ));
        }
    }
    require_legacy_execution(&state, &execution_id)?;
    let execution = state.store.load_execution(&execution_id)?;
    let definition = load_definition_for(&state, &execution)?;

    let handle =
        state
            .engine
            .try_begin_deliver_human_input(&definition, &execution_id, &delivery)?;
    spawn_active(&state, execution_id.clone(), async move {
        let _ = handle.run_to_completion(&definition).await;
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(AcceptedResponse { execution_id }),
    ))
}

/// `POST /executions/:execution_id/transition` — a manual or forced
/// transition, server-side.
///
/// Reimplements `src/tui/app.rs`'s `write_transition` (reached today only
/// through `manual_transition`/`force_transition`, both fed from a menu the
/// TUI already built out of *legal* choices), now in
/// [`Engine::take_manual_transition`], which takes the execution's claim — a
/// concurrently-driven execution is `409` — and — the one check the TUI
/// never needed, because its menu could not offer an illegal choice in the
/// first place — the requested transition is validated against the
/// workflow's own declared transitions before anything is written:
///
/// * ordinary (`force: false`): `label` must be one of the current state's
///   declared `transitions`; anything else is `400`.
/// * forced (`force: true`, D30): the current state must declare
///   `allow_manual_override: true`, and `target` must name a state the
///   workflow actually has; either failing is `400`.
///
/// Once validated, the event sequence and status update are byte-for-byte
/// the ones `write_transition` writes, in the same order.
async fn post_transition(
    State(state): State<AppState>,
    Path(execution_id): Path<String>,
    Json(body): Json<TransitionRequest>,
) -> Result<Json<ExecutionSummary>, ApiError> {
    require_legacy_execution(&state, &execution_id)?;
    let execution = state.store.load_execution(&execution_id)?;
    let definition = load_definition_for(&state, &execution)?;

    // The claim the engine takes for the transition is what keeps it from
    // racing a background pass's own appends and saves: an execution being
    // driven right now is `409`.
    let execution = state.engine.take_manual_transition(
        &definition,
        &execution_id,
        &ManualTransition::by_operator(body.label, body.target, body.force),
    )?;

    Ok(Json(ExecutionSummary::from(&execution)))
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
    let mut executions = std::collections::BTreeMap::new();
    let mut ambiguous = std::collections::BTreeSet::new();
    for store in std::iter::once(state.store.clone()) {
        for id in store.list_execution_ids()? {
            // An execution URL names no backend. Do not advertise an ID for
            // which a later direct read would be ambiguous.
            if !ambiguous.insert(id.clone()) {
                executions.remove(&id);
                continue;
            }
            let execution = store.load_execution(&id)?;
            executions.insert(id, build_summary(&store, &execution)?);
        }
    }
    if let Some(control) = &state.control {
        let tasks = control.list_tasks(&owner).map_err(|error| {
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
                local_storage_root(&state)
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
    Ok(Json(ExecutionListResponse {
        executions: executions.into_values().collect(),
    }))
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
    execution: &crate::domain::WorkflowExecution,
) -> Result<ExecutionSummary, ApiError> {
    let mut summary = ExecutionSummary::from(execution);
    summary.variables = store
        .load_context(&execution.id)
        .map(|context| context.variables)
        .unwrap_or_else(|_| execution.context.variables.clone());
    summary.pending_input = if execution.status == ExecutionStatus::InputNeeded {
        store
            .load_pending_input::<crate::engine::PendingHumanInput>(&execution.id)
            .unwrap_or(None)
    } else {
        None
    };
    summary.annotation = summary
        .pending_input
        .as_ref()
        .and_then(|pending| annotation_of(store, &execution.id, pending));
    summary.problem = match &summary.pending_input {
        Some(pending) => ActiveProblem::of_pending(pending),
        // A failed step stays `Failed` (KTD4): its problem is the
        // conductor's report, while the execution is still failed on it.
        None => crate::conductor::active_failure_problem(store, execution)
            .as_ref()
            .map(ActiveProblem::of_failure),
    };
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
    execution: &crate::domain::WorkflowExecution,
) -> Option<ConductorActivity> {
    let conductor = ConductorStore::new(store.paths(), &execution.id);
    let state = if conductor.exists() {
        Some(conductor.load_state().ok()?)
    } else {
        None
    };
    if let Some(state) = &state {
        if let Some(pending) = &state.pending {
            return Some(ConductorActivity::of_pending(pending));
        }
        if let Some(queued) = state.queue.first() {
            return Some(ConductorActivity::of_queued(queued));
        }
    }
    let failure = crate::engine::step_failure(execution).ok().flatten()?;
    let concluded = state
        .as_ref()
        .and_then(|state| state.failure.as_ref())
        .is_some_and(|report| report.key == failure.key());
    (!concluded).then(|| ConductorActivity::owed_diagnosis(&failure, execution))
}

// ---------------------------------------------------------------------------
// SSE endpoints (U4)
// ---------------------------------------------------------------------------

/// How often the connection sends a keep-alive comment frame while no real
/// event fires — long enough not to spam an idle connection, short enough
/// that a client (or an intermediary proxy) never mistakes a quiet execution
/// for a dead one.
const SSE_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(15);

fn sse_keep_alive() -> KeepAlive {
    KeepAlive::new()
        .interval(SSE_KEEP_ALIVE_INTERVAL)
        .text("keep-alive")
}

/// Adapts [`Engine::subscribe`]'s broadcast receiver into the stream an axum
/// [`Sse`] response wants: `Result<sse::Event, Infallible>`.
///
/// This is the one seam both SSE endpoints share — the only difference
/// between "one execution's events" and "every execution's events"
/// (`filter_execution_id`) is whether a `None` filter lets everything
/// through.
///
/// Two failure-shaped inputs are deliberately swallowed as `None` (skip this
/// item, keep the stream open) rather than turned into a stream `Err`,
/// because `Sse`'s item type here is `Infallible` — surfacing either as an
/// `Err` would have no representable value to construct and, more to the
/// point, would end the whole SSE stream for one bad event, which is exactly
/// what the plan says must not happen:
///
/// * [`BroadcastStreamRecvError::Lagged`] — this receiver fell behind the
///   engine's broadcast buffer and missed `n` events irretrievably (the
///   comment on [`Engine::subscribe`] already documents that a slow reader
///   never blocks or fails the engine). Logged as a warning so the operator
///   can see it happened, then the stream simply continues from the next
///   event the buffer still holds.
/// * A `json_data` serialization failure — [`Event`] always serializes in
///   practice (every field is a plain, already-`Serialize` domain type), so
///   this is defense in depth, not a path any test expects to exercise.
fn sse_event_stream(
    rx: broadcast::Receiver<Event>,
    filter_execution_id: Option<String>,
) -> impl Stream<Item = Result<SseEvent, Infallible>> {
    BroadcastStream::new(rx).filter_map(move |item| {
        let event = match item {
            Ok(event) => event,
            Err(BroadcastStreamRecvError::Lagged(skipped)) => {
                tracing::warn!(
                    skipped,
                    execution_id = filter_execution_id.as_deref().unwrap_or("*"),
                    "api: SSE client lagged behind the engine's event broadcast; \
                     some events were not delivered to it"
                );
                return None;
            }
        };

        if let Some(wanted) = &filter_execution_id {
            if &event.workflow_execution_id != wanted {
                return None;
            }
        }

        match SseEvent::default().event("engine").json_data(&event) {
            Ok(sse_event) => Some(Ok(sse_event)),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    execution_id = %event.workflow_execution_id,
                    "api: failed to encode an engine event as SSE data; dropping it"
                );
                None
            }
        }
    })
}

/// `GET /executions/:execution_id/stream` — an SSE stream of every event the
/// engine publishes for this one execution (AE5), filtered from the same
/// broadcast every other subscriber (including the global stream below) also
/// reads from.
async fn get_execution_stream(
    State(state): State<AppState>,
    Path(execution_id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    require_execution(&state, &execution_id)?;
    let rx = state.engine.subscribe();
    let stream = sse_event_stream(rx, Some(execution_id));
    Ok(Sse::new(stream).keep_alive(sse_keep_alive()))
}

/// `GET /events/stream` — an SSE stream of every event from every execution
/// the engine runs, unfiltered.
async fn get_global_stream(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = state.engine.subscribe();
    let stream = sse_event_stream(rx, None);
    Sse::new(stream).keep_alive(sse_keep_alive())
}

#[cfg(test)]
mod sse_stream_tests {
    //! Unit-level coverage of [`sse_event_stream`]'s `Lagged` handling.
    //!
    //! This is deliberately not an HTTP-level test (unlike the rest of U4's
    //! test list, in `tests/sse.rs`): reproducing a real `Lagged` over an
    //! actual loopback connection would mean overrunning
    //! [`crate::engine::runner::EVENT_CHANNEL_CAPACITY`] (1024) faster than
    //! the kernel's socket buffers and axum's own body-polling ever drain
    //! it — which depends on OS buffer sizing and scheduling, not on
    //! anything this code controls, and would make the test flaky. Building
    //! a small broadcast channel directly and driving
    //! [`sse_event_stream`] — the exact function both SSE handlers call —
    //! exercises the same `Err(Lagged(_))` branch deterministically.
    use super::*;
    use crate::domain::EventData;

    #[tokio::test]
    async fn lagged_receiver_skips_missed_events_but_keeps_streaming() {
        // Capacity 2: sending 5 events before anyone reads guarantees the
        // receiver is behind by more than the buffer holds.
        let (tx, rx) = broadcast::channel(2);
        let stream = sse_event_stream(rx, None);
        tokio::pin!(stream);

        for sequence in 1..=5u64 {
            let _ = tx.send(sample_event("exec-1", sequence));
        }
        drop(tx);

        let mut received = Vec::new();
        while let Some(item) = stream.next().await {
            // The stream's item type is `Result<SseEvent, Infallible>` —
            // asserting it is always `Ok` is the point: a lagged receiver
            // must never surface as a stream error.
            let event = item.expect("a lagged receiver must not close the stream with an error");
            received.push(event);
        }

        // Only the last two of the five sends still fit in a
        // capacity-2 channel; the rest were overwritten before this
        // receiver ever read them. What matters here is not exactly which
        // sequence numbers survive (that's the broadcast channel's own
        // contract) but that the stream degraded gracefully: it kept
        // producing well-formed events after the loss instead of ending.
        assert_eq!(
            received.len(),
            2,
            "the receiver should see exactly the events still held in the buffer"
        );
    }

    fn sample_event(execution_id: &str, sequence: u64) -> Event {
        Event {
            id: format!("event-{sequence}"),
            workflow_execution_id: execution_id.to_string(),
            timestamp: Utc::now(),
            data: EventData::StateEntered {
                state_id: "plan".to_string(),
            },
            sequence,
        }
    }
}
