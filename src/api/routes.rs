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

use axum::extract::{Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::Event;
use crate::engine::{build_timeline, Timeline};
use crate::persistence::{ConversationSource, ExecutionStore, PersistenceError};

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
    store: ExecutionStore,
    token: String,
}

/// Builds the router: three GET endpoints behind the bearer-token gate.
pub fn router(store: ExecutionStore, token: String) -> Router {
    let state = AppState { store, token };
    Router::new()
        .route("/executions/:execution_id/events", get(get_events))
        .route("/executions/:execution_id/timeline", get(get_timeline))
        .route(
            "/executions/:execution_id/attempts/:attempt_id/conversation",
            get(get_conversation),
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
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// An error a handler can return; every variant maps to a status code with
/// no informative body, since the only caller is the TUI/CLI client in this
/// same trust boundary and there is nothing a body would add.
enum ApiError {
    ExecutionNotFound,
    Storage(PersistenceError),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::ExecutionNotFound => StatusCode::NOT_FOUND.into_response(),
            ApiError::Storage(error) => {
                tracing::warn!(error = %error, "api: storage error serving request");
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
