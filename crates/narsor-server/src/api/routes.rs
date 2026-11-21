//! Handlers for the daemon API and its bearer-token gate.
//!
//! The owner routes of this module start a task (`POST /tasks`) and keep the
//! default-harness setting; the control plane's own owner routes
//! (`/control/v1/…`) serve every task read, the executions included (U8).
//! Nothing here reads a task's storage: an execution is read through its
//! task, from its runner or a read-only runner (D63), whatever its backend.
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

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::Value;

use crate::api::ApiServices;
use crate::auth_gate as gate;
use crate::auth_gate::OwnerAuth;
use crate::controller_protocol::http as protocol_http;
use narsor_engine::api::dto::{
    HarnessSelectionSnapshot, RequestErrorResponse, SetHarnessSelectionRequest, StartTaskRequest,
    StartTaskResponse,
};
use narsor_engine::controller_protocol::ProtocolError;
use narsor_engine::domain::TaskId;
use narsor_engine::environment::LaunchIntent;
use narsor_engine::launch::CatalogLaunchResolver;
use narsor_engine::orchestration::OwnerRef;
use narsor_engine::persistence::{
    DefaultHarnessSelector, HarnessSelectionError,
    HarnessSelectionSnapshot as PersistenceHarnessSelectionSnapshot,
};
use narsor_engine::registry::{BranchLister, GitBranchLister};
use narsor_engine::tasks::{self, TaskError};
use narsor_engine::workflows::WorkflowResolver;

mod control;

#[derive(Clone)]
struct AppState {
    catalog_dir: PathBuf,
    harness_selector: Option<Arc<DefaultHarnessSelector>>,
    control: Option<Arc<narsor_engine::controller_protocol::ProtocolService>>,
    local_launch: Option<Arc<CatalogLaunchResolver>>,
    /// The workflows `GET /control/v1/workflows` lists.
    workflows: WorkflowResolver,
    /// Whether this is the trusted local daemon: only it serves the files
    /// of its own disk to its owner (U6).
    local: bool,
    /// What a client calls this connection.
    connection_name: String,
    /// Lists the branches of a registered repository's remote.
    branch_lister: Arc<dyn BranchLister>,
}

/// Builds the router: `POST /tasks` and the default-harness setting behind
/// the owner gate (`auth`: the process token, or the owner's OIDC token).
/// Every execution lives on a backend's storage and is driven and read by
/// its runner; the daemon drives none and reads none.
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
/// `catalog_dir` is what workflow-reference resolution ([`WorkflowResolver`])
/// needs — the daemon's own catalogue (`config::Resolved::catalog_dir`).
pub fn router(auth: OwnerAuth, catalog_dir: PathBuf, services: ApiServices) -> Router {
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
        harness_selector: services.harness_selector,
        control: control.clone(),
        local_launch: services.local_launch,
        branch_lister: services
            .branch_lister
            .unwrap_or_else(|| Arc::new(GitBranchLister::default())),
    };
    let mut owner = Router::new()
        .route(
            "/settings/agent-harness",
            get(get_harness_selection)
                .put(put_harness_selection)
                .delete(delete_harness_selection),
        )
        .route("/tasks", post(post_start_task));
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
    NotFound,
    /// The request itself is malformed or asks for something the workflow
    /// definition does not allow (an unknown workflow reference, an illegal
    /// transition target). `400`.
    BadRequest(String),
    /// A request error safe to return to a local authenticated client.
    Request {
        code: &'static str,
        message: String,
    },
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
    /// Anything else that stops a handler from completing: `500`.
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::NotFound => StatusCode::NOT_FOUND.into_response(),
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
        ProtocolError::NotFound => ApiError::NotFound,
        ProtocolError::BadRequest { code, message } => ApiError::Request { code, message },
        ProtocolError::Conflict { code, message } => ApiError::Refused { code, message },
        ProtocolError::Unavailable { code, message } => ApiError::Unavailable { code, message },
        ProtocolError::Unauthorized | ProtocolError::Forbidden => ApiError::NotFound,
        ProtocolError::Internal(message) => ApiError::Internal(message),
    }
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
