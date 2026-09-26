//! Handlers for the three read-only endpoints, and the bearer-token gate in
//! front of all of them.
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
//! only ever returns a bare status code.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::api::dto::{
    AcceptedResponse, ExecutionListResponse, ExecutionSummary, StartTaskRequest, StartTaskResponse,
    TransitionRequest,
};
use crate::domain::{Event, EventData, ExecutionStatus};
use crate::engine::{
    build_timeline, terminal_status, Engine, EngineError, HumanInputDelivery, Timeline,
};
use crate::persistence::{ConversationSource, ExecutionStore, PersistenceError};
use crate::tasks::{self, TaskError};
use crate::workflows::WorkflowResolver;

/// The decision provider recorded for an operator's ordinary manual choice.
///
/// Deliberately the same literal `src/tui/app.rs`'s `MANUAL_PROVIDER` writes:
/// both are alternative front doors to the same append-only event log, and an
/// operator reading it later must not be able to tell which one wrote a given
/// manual transition from the provider string alone.
const MANUAL_PROVIDER: &str = "human";

/// The decision provider recorded for a forced override (D30). See
/// `src/tui/app.rs`'s `OVERRIDE_PROVIDER` for why it is deliberately distinct
/// from [`MANUAL_PROVIDER`].
const OVERRIDE_PROVIDER: &str = "human_override";

/// The reason recorded on a forced transition — same text as
/// `src/tui/app.rs`'s `OVERRIDE_REASON`.
const OVERRIDE_REASON: &str =
    "Forced manual transition (override): the target state was picked by the operator.";

/// The reason recorded on an ordinary manual transition — same text as
/// `src/tui/app.rs`'s `MANUAL_REASON`.
const MANUAL_REASON: &str = "Manual transition picked by the operator.";

/// The label recorded for a forced transition, which by definition has none
/// — same value as `src/tui/app.rs`'s `OVERRIDE_LABEL`.
const OVERRIDE_LABEL: &str = "override";

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
    token: String,
    catalog_dir: PathBuf,
    work_dir: PathBuf,
}

/// Builds the router: the three original read-only endpoints (U5/U2) plus
/// the mutating surface U3 adds — `POST /tasks`, `POST
/// /executions/:id/resume`, `POST /executions/:id/input`, `POST
/// /executions/:id/transition` and `GET /executions` — all behind the same
/// bearer-token gate.
///
/// `catalog_dir`/`work_dir` are what [`crate::tasks::start_task`] and
/// workflow-reference resolution ([`WorkflowResolver`]) need — the daemon's
/// own catalogue and work roots (`config::Resolved::catalog_dir`/
/// `work_dir`), now read by the server that runs a task instead of by a CLI
/// process that used to hold them directly.
pub fn router(
    engine: Arc<Engine>,
    store: ExecutionStore,
    token: String,
    catalog_dir: PathBuf,
    work_dir: PathBuf,
) -> Router {
    let state = AppState {
        engine,
        store,
        token,
        catalog_dir,
        work_dir,
    };
    Router::new()
        .route("/executions/:execution_id/events", get(get_events))
        .route("/executions/:execution_id/timeline", get(get_timeline))
        .route(
            "/executions/:execution_id/attempts/:attempt_id/conversation",
            get(get_conversation),
        )
        .route("/executions", get(get_executions))
        .route("/tasks", post(post_start_task))
        .route("/executions/:execution_id/resume", post(post_resume))
        .route("/executions/:execution_id/input", post(post_input))
        .route(
            "/executions/:execution_id/transition",
            post(post_transition),
        )
        .layer(middleware::from_fn_with_state(state.clone(), require_token))
        .with_state(state)
}

/// Refuses any request whose `Authorization` header is missing or does not
/// match the process token, with no response body either way — a caller
/// cannot distinguish "wrong token" from "no token" from the response
/// itself, which is the point.
async fn require_token(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let expected = format!("Bearer {}", state.token);
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());

    match presented {
        Some(value) if constant_time_eq(value.as_bytes(), expected.as_bytes()) => {
            next.run(request).await
        }
        _ => StatusCode::UNAUTHORIZED.into_response(),
    }
}

/// Compares two byte strings in time that depends only on their lengths, not
/// their contents — the token is a bearer secret, so a length-leaking or
/// early-exit comparison would give a caller on the same loopback interface
/// a timing side channel to brute-force it byte by byte.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
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
    /// The request is well-formed but conflicts with the execution's current
    /// state (already being driven, or being transitioned by hand while
    /// active). `409`.
    Conflict(String),
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
            ApiError::Conflict(message) => {
                tracing::debug!(message, "api: conflict");
                StatusCode::CONFLICT.into_response()
            }
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

impl From<PersistenceError> for ApiError {
    fn from(error: PersistenceError) -> Self {
        ApiError::Storage(error)
    }
}

/// Maps an [`EngineError`] the way the plan's U3 Approach specifies:
/// [`EngineError::AlreadyRunning`] → `409`, "the execution does not exist" →
/// `404`, everything else → `500`. Every other variant (`NotRunnable`,
/// `AwaitingHumanInput`, `Decision`, `InvalidDefinition`, ...) is an engine or
/// workflow-author problem the plan does not ask this endpoint surface to
/// distinguish further — it lands on `Internal`, same as a storage failure.
impl From<EngineError> for ApiError {
    fn from(error: EngineError) -> Self {
        match error {
            EngineError::AlreadyRunning { .. } => ApiError::Conflict(error.to_string()),
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

fn require_execution(state: &AppState, execution_id: &str) -> Result<(), ApiError> {
    if is_safe_id(execution_id) && state.store.exists(execution_id) {
        Ok(())
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
    Path(execution_id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> Result<Json<Vec<Event>>, ApiError> {
    require_execution(&state, &execution_id)?;
    let events = state.store.event_log(&execution_id).read_all()?;
    let events = events
        .into_iter()
        .filter(|event| event.sequence > query.after)
        .collect();
    Ok(Json(events))
}

/// `GET /executions/:execution_id/timeline`
///
/// The step timeline ([`crate::engine::build_timeline`]) computed from the
/// execution's event log and its conversation store's presence — the real
/// wiring U4's pure function was left waiting for.
async fn get_timeline(
    State(state): State<AppState>,
    Path(execution_id): Path<String>,
) -> Result<Json<Timeline>, ApiError> {
    require_execution(&state, &execution_id)?;
    let execution = state.store.load_execution(&execution_id)?;
    let events = state.store.event_log(&execution_id).read_all()?;
    let presence = conversation_presence(&state.store, &execution_id)?;
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
    Path((execution_id, attempt_id)): Path<(String, String)>,
    Query(query): Query<ConversationQuery>,
) -> Result<Json<ConversationResponse>, ApiError> {
    require_execution(&state, &execution_id)?;
    if !is_safe_id(&attempt_id) {
        return Ok(Json(ConversationResponse::NoRecord));
    }
    let store = state.store.conversation_store(&execution_id);
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
    Json(body): Json<StartTaskRequest>,
) -> Result<(StatusCode, Json<StartTaskResponse>), ApiError> {
    let resolved = WorkflowResolver::new(state.catalog_dir.clone())
        .resolve(&body.workflow)
        .map_err(|error| ApiError::BadRequest(error.to_string()))?;
    let definition = tasks::load_resolved(&resolved)?;

    let execution = tasks::start_task(
        &state.catalog_dir,
        &state.work_dir,
        &state.engine,
        &definition,
        &resolved,
        body.inputs,
    )?;

    let execution_id = execution.id.clone();
    let engine = Arc::clone(&state.engine);
    tokio::spawn(async move {
        let _ = engine.run(&definition, &execution_id).await;
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(StartTaskResponse {
            execution_id: execution.id,
        }),
    ))
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
    require_execution(&state, &execution_id)?;
    let execution = state.store.load_execution(&execution_id)?;
    let definition = load_definition_for(&state, &execution)?;

    let handle = state.engine.try_begin_resume(&execution_id)?;
    tokio::spawn(async move {
        let _ = handle.run_to_completion(&definition).await;
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(AcceptedResponse { execution_id }),
    ))
}

/// `POST /executions/:execution_id/input` — delivers a person's answer to a
/// paused execution.
///
/// The body is a [`HumanInputDelivery`] verbatim — it already derives
/// `Serialize`/`Deserialize` for exactly this reason, so no separate DTO
/// duplicates its shape. Same two-phase split as [`post_resume`].
async fn post_input(
    State(state): State<AppState>,
    Path(execution_id): Path<String>,
    Json(delivery): Json<HumanInputDelivery>,
) -> Result<(StatusCode, Json<AcceptedResponse>), ApiError> {
    require_execution(&state, &execution_id)?;
    let execution = state.store.load_execution(&execution_id)?;
    let definition = load_definition_for(&state, &execution)?;

    let handle =
        state
            .engine
            .try_begin_deliver_human_input(&definition, &execution_id, &delivery)?;
    tokio::spawn(async move {
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
/// TUI already built out of *legal* choices): `engine.is_active` refuses a
/// concurrently-driven execution first (`409`), and — the one check the TUI
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
    require_execution(&state, &execution_id)?;

    // A background pass may be advancing this execution right now — this
    // check is what keeps a hand-written transition from racing that pass's
    // own appends and its own `save_execution` (see `write_transition`'s own
    // comment on the same check).
    if state.engine.is_active(&execution_id) {
        return Err(ApiError::Conflict(format!(
            "execution '{execution_id}' is being driven in the background right now"
        )));
    }

    let mut execution = state.store.load_execution(&execution_id)?;
    let definition = load_definition_for(&state, &execution)?;

    let from_state_id = execution
        .current_state
        .clone()
        .unwrap_or_else(|| definition.initial_state.clone());
    let from_state = definition.states.get(&from_state_id).ok_or_else(|| {
        ApiError::Internal(format!(
            "execution '{execution_id}' is at state '{from_state_id}', which its workflow does \
             not define"
        ))
    })?;

    let (target, label, provider, reason) = if body.force {
        if !from_state.allow_manual_override {
            return Err(ApiError::BadRequest(format!(
                "state '{from_state_id}' does not allow a forced transition \
                 (allow_manual_override)"
            )));
        }
        let target = body.target.clone().ok_or_else(|| {
            ApiError::BadRequest("a forced transition requires a target state".to_string())
        })?;
        if !definition.states.contains_key(&target) {
            return Err(ApiError::BadRequest(format!(
                "the workflow has no state '{target}'"
            )));
        }
        (
            target,
            OVERRIDE_LABEL.to_string(),
            OVERRIDE_PROVIDER,
            OVERRIDE_REASON,
        )
    } else {
        let label = body.label.clone().ok_or_else(|| {
            ApiError::BadRequest("label is required unless force is set".to_string())
        })?;
        let target = from_state.transitions.get(&label).cloned().ok_or_else(|| {
            ApiError::BadRequest(format!(
                "state '{from_state_id}' has no transition for label '{label}' — this is not a \
                 legal transition from the current state"
            ))
        })?;
        (target, label, MANUAL_PROVIDER, MANUAL_REASON)
    };

    // Guaranteed to exist: the forced branch checked it above, and the
    // ordinary branch's target came from the current state's own
    // `transitions` map, which the loader already validates points at a
    // declared state (`WORKFLOW_SCHEMA.md` §5).
    let target_state = definition
        .states
        .get(&target)
        .cloned()
        .ok_or_else(|| ApiError::Internal(format!("the workflow has no state '{target}'")))?;

    let log = state.store.event_log(&execution_id);
    let writes: Vec<EventData> = vec![
        EventData::DecisionMade {
            provider: provider.to_string(),
            output_label: label.clone(),
            reasoning: Some(reason.to_string()),
        },
        EventData::HumanTransitionSelected {
            from_state: from_state_id.clone(),
            to_state: target.clone(),
        },
        EventData::StateExited {
            state_id: from_state_id.clone(),
            output_label: label.clone(),
        },
        EventData::TransitionTaken {
            from_state: from_state_id.clone(),
            to_state: target.clone(),
            output_label: label.clone(),
        },
    ];
    for data in writes {
        log.append_next(&execution_id, data)?;
    }

    execution.current_state = Some(target.clone());
    execution.current_action_execution = None;

    if target_state.terminal {
        let status = terminal_status(&target_state);
        execution.status = status.clone();
        execution.completed_at = Some(Utc::now());

        let closing = match status {
            ExecutionStatus::Failed => EventData::WorkflowFailed {
                error: format!("Forced transition into terminal state '{target}'."),
            },
            ExecutionStatus::Cancelled => EventData::WorkflowCancelled {
                reason: format!("Forced transition into terminal state '{target}'."),
            },
            _ => EventData::WorkflowCompleted {
                result: Value::Object(Map::new()),
            },
        };
        let entered = EventData::StateEntered {
            state_id: target.clone(),
        };
        for data in [entered, closing] {
            log.append_next(&execution_id, data)?;
        }
    } else {
        execution.status = ExecutionStatus::Paused;
        log.append_next(
            &execution_id,
            EventData::WorkflowPaused {
                reason: format!("Manual transition into state '{target}'; waiting to be resumed."),
            },
        )?;
    }

    // A question asked in the state just left can no longer be answered.
    state.store.clear_pending_input(&execution_id)?;
    state.store.save_execution(&execution)?;

    Ok(Json(ExecutionSummary::from(&execution)))
}

/// `GET /executions` — every stored execution, summarised.
async fn get_executions(
    State(state): State<AppState>,
) -> Result<Json<ExecutionListResponse>, ApiError> {
    let ids = state.store.list_execution_ids()?;
    let mut executions = Vec::with_capacity(ids.len());
    for id in ids {
        let execution = state.store.load_execution(&id)?;
        executions.push(ExecutionSummary::from(&execution));
    }
    Ok(Json(ExecutionListResponse { executions }))
}
