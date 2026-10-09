//! HTTP binding of the protocol: routers, credential gates and handlers.
//!
//! Three routers, merged into the daemon's API by `crate::api::routes`:
//!
//! | router | gate | prefix |
//! | --- | --- | --- |
//! | [`owner_routes`] | the API's own owner gate (it inserts the caller's [`OwnerRef`]) | `/control/v1` |
//! | [`controller_routes`] | controller credential → [`ControllerIdentity`] | `/controller/v1` |
//! | [`runner_routes`] | runner credential → [`RunnerIdentity`] | `/runner/v1` |
//!
//! Each gate answers every refusal with a bare `401` — the same rule as the
//! API's token gate, so a credential is never confirmed or denied by
//! anything but the status code. `POST /controller/v1/enroll` sits outside
//! the controller gate: the one-time code in its body, signed by the key it
//! binds, is the credential.
//!
//! The controller gate reads the whole (bounded) body before the handler
//! does: a controller's signature covers it ([`crate::auth::pop`]), so a
//! request cannot be replayed with another body.
//!
//! Enrollment codes, the controller list and revocation are the installation
//! operator's: the owner gate marks such a caller with
//! [`crate::auth::Operator`], and these handlers refuse anyone else with a
//! bare `403`.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use futures::stream::{self, Stream};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

use super::credentials::{ControllerIdentity, RunnerIdentity};
use super::error::{ProtocolError, ProtocolResult};
use super::service::ProtocolService;
use super::wire::*;
use crate::auth::pop::SignedRequest;
use crate::auth::Operator;
use crate::domain::TaskId;
use crate::materials::{
    MaterialAckRequest, MaterialAckResponse, MaterialListResponse, MaterialSummary,
    MaterialUploadResponse, MaterialView, BUNDLE_CONTENT_TYPE,
};
use crate::orchestration::{OwnerRef, TaskEvent};

type Service = Arc<ProtocolService>;

/// How often an idle control-plane stream sends a keep-alive comment.
const SSE_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Largest controller request body the gate reads to check its signature.
/// Controllers send small JSON documents; nothing legitimate comes close.
const MAX_CONTROLLER_BODY: usize = 1024 * 1024;

/// Owner-facing routes. They expect the caller's [`OwnerRef`] in the
/// request extensions, put there by the API's owner gate.
pub fn owner_routes<S>(service: Service) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/control/v1/tasks", get(list_tasks).post(create_task))
        .route("/control/v1/tasks/:task_id", get(get_task))
        .route("/control/v1/tasks/:task_id/commands", post(post_command))
        .route("/control/v1/tasks/:task_id/move", post(post_move))
        .route(
            "/control/v1/default-backend",
            get(get_default_backend).put(put_default_backend),
        )
        .route("/control/v1/tasks/:task_id/timeline", get(get_timeline))
        .route(
            "/control/v1/tasks/:task_id/execution-events",
            get(get_execution_events),
        )
        .route("/control/v1/tasks/:task_id/conductor", get(get_conductor))
        .route(
            "/control/v1/tasks/:task_id/conversation",
            get(get_conversation),
        )
        .route(
            "/control/v1/tasks/:task_id/attempts/:attempt_id/conversation",
            get(get_conversation_page),
        )
        .route("/control/v1/tasks/:task_id/input", post(post_input))
        .route(
            "/control/v1/tasks/:task_id/operator-commands",
            post(post_operator_command),
        )
        .route(
            "/control/v1/tasks/:task_id/materials",
            get(list_materials).post(post_material),
        )
        .route(
            "/control/v1/tasks/:task_id/materials/:material_id",
            get(get_material).delete(delete_material),
        )
        .route(
            "/control/v1/tasks/:task_id/materials/:material_id/blobs/:sha256",
            get(get_material_blob),
        )
        .route("/control/v1/tasks/:task_id/events", get(get_events))
        .route("/control/v1/events/stream", get(get_stream))
        .route(
            "/control/v1/backends/:backend_id/enrollments",
            post(post_enrollment),
        )
        .route("/control/v1/controllers", get(list_controllers))
        .route(
            "/control/v1/controllers/:controller_id/revoke",
            post(post_revoke_controller),
        )
        .with_state(service)
}

/// Controller-facing routes, behind the controller credential gate, plus
/// the unauthenticated enrollment.
pub fn controller_routes<S>(service: Service) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let gated = Router::new()
        .route("/controller/v1/heartbeat", post(post_heartbeat))
        .route("/controller/v1/key", post(post_rotate_key))
        .route("/controller/v1/lease", post(post_acquire_lease))
        .route("/controller/v1/lease/renew", post(post_renew_lease))
        .route("/controller/v1/lease/release", post(post_release_lease))
        .route("/controller/v1/commands", get(get_commands))
        .route("/controller/v1/acks", post(post_acks))
        .route("/controller/v1/reports", post(post_report))
        .route(
            "/controller/v1/tasks/:task_id/runner-credentials",
            post(post_runner_credential),
        )
        .layer(middleware::from_fn_with_state(
            Arc::clone(&service),
            require_controller,
        ));
    Router::new()
        .route("/controller/v1/enroll", post(post_enroll))
        .merge(gated)
        .with_state(service)
}

/// Runner-facing routes, behind the runner credential gate.
pub fn runner_routes<S>(service: Service) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/runner/v1/snapshots", post(post_snapshot))
        .route("/runner/v1/poll", get(get_poll))
        .route("/runner/v1/renew", post(post_renew))
        .route("/runner/v1/inputs/:input_id/ack", post(post_input_ack))
        .route(
            "/runner/v1/commands/:command_id/ack",
            post(post_command_ack),
        )
        .route(
            "/runner/v1/conversation-pages/:request_id",
            post(post_conversation_page),
        )
        .route(
            "/runner/v1/execution-events/:request_id",
            post(post_execution_events),
        )
        .route(
            "/runner/v1/conductor-reads/:request_id",
            post(post_conductor_read),
        )
        .route("/runner/v1/materials", post(post_runner_material))
        .route(
            "/runner/v1/materials/:material_id",
            get(get_runner_material),
        )
        .route(
            "/runner/v1/materials/:material_id/blobs/:sha256",
            get(get_runner_material_blob),
        )
        .route(
            "/runner/v1/materials/:material_id/ack",
            post(post_runner_material_ack),
        )
        .layer(middleware::from_fn_with_state(
            Arc::clone(&service),
            require_runner,
        ))
        .with_state(service)
}

fn bearer(request: &Request) -> Option<&str> {
    request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
}

/// The controller gate: a bare `401` for anything but a fresh, unreplayed
/// request signed by the key of an unrevoked controller identity.
async fn require_controller(
    State(service): State<Service>,
    request: Request,
    next: Next,
) -> Response {
    let (parts, body) = request.into_parts();
    let Ok(body) = axum::body::to_bytes(body, MAX_CONTROLLER_BODY).await else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let authorization = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let path_and_query = parts
        .uri
        .path_and_query()
        .map_or(parts.uri.path(), |value| value.as_str());
    let identity = service.authenticate_controller(&SignedRequest {
        authorization,
        method: parts.method.as_str(),
        path_and_query,
        body: &body,
    });
    match identity {
        Some(identity) => {
            let mut request = Request::from_parts(parts, axum::body::Body::from(body));
            request.extensions_mut().insert(identity);
            next.run(request).await
        }
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

/// The installation operator, or a bare `403` (R37: only the operator
/// enrolls, lists and revokes controllers).
fn require_operator(operator: Option<Extension<Operator>>) -> ProtocolResult<()> {
    operator.map(|_| ()).ok_or(ProtocolError::Forbidden)
}

/// The runner credential gate: a bare `401` for anything but the current,
/// unexpired credential of a current launch.
async fn require_runner(
    State(service): State<Service>,
    mut request: Request,
    next: Next,
) -> Response {
    match bearer(&request).and_then(|token| service.authenticate_runner(token)) {
        Some(identity) => {
            request.extensions_mut().insert(identity);
            next.run(request).await
        }
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

// ---------------------------------------------------------------------------
// Owner handlers
// ---------------------------------------------------------------------------

async fn create_task(
    State(service): State<Service>,
    Extension(owner): Extension<OwnerRef>,
    Json(request): Json<CreateTaskRequest>,
) -> ProtocolResult<(StatusCode, Json<TaskSummary>)> {
    let summary = service.create_task(&owner, request)?;
    Ok((StatusCode::CREATED, Json(summary)))
}

async fn list_tasks(
    State(service): State<Service>,
    Extension(owner): Extension<OwnerRef>,
    Query(query): Query<TaskListQuery>,
) -> ProtocolResult<Json<TaskListResponse>> {
    Ok(Json(service.list_tasks_where(&owner, query.archived)?))
}

async fn get_task(
    State(service): State<Service>,
    Extension(owner): Extension<OwnerRef>,
    Path(task_id): Path<String>,
) -> ProtocolResult<Json<TaskSummary>> {
    Ok(Json(service.task(&owner, &task_id)?))
}

async fn post_command(
    State(service): State<Service>,
    Extension(owner): Extension<OwnerRef>,
    Path(task_id): Path<String>,
    Json(request): Json<OwnerCommandRequest>,
) -> ProtocolResult<Json<OwnerCommandResponse>> {
    Ok(Json(service.command(&owner, &task_id, request)?))
}

async fn post_move(
    State(service): State<Service>,
    Extension(owner): Extension<OwnerRef>,
    Path(task_id): Path<String>,
    Json(request): Json<MoveTaskRequest>,
) -> ProtocolResult<Json<TaskSummary>> {
    Ok(Json(service.move_task(&owner, &task_id, request)?))
}

async fn get_default_backend(
    State(service): State<Service>,
    Extension(_owner): Extension<OwnerRef>,
) -> ProtocolResult<Json<DefaultBackendView>> {
    Ok(Json(service.default_backend()?))
}

async fn put_default_backend(
    State(service): State<Service>,
    Extension(_owner): Extension<OwnerRef>,
    Json(request): Json<SetDefaultBackendRequest>,
) -> ProtocolResult<Json<DefaultBackendView>> {
    Ok(Json(service.set_default_backend(request)?))
}

async fn get_timeline(
    State(service): State<Service>,
    Extension(owner): Extension<OwnerRef>,
    Path(task_id): Path<String>,
) -> ProtocolResult<Json<TimelineResponse>> {
    Ok(Json(service.timeline(&owner, &task_id)?))
}

async fn get_execution_events(
    State(service): State<Service>,
    Extension(owner): Extension<OwnerRef>,
    Path(task_id): Path<String>,
    Query(query): Query<ExecutionEventsQuery>,
) -> ProtocolResult<Json<ExecutionEventsResponse>> {
    Ok(Json(
        service.execution_events(&owner, &task_id, query).await?,
    ))
}

async fn get_conductor(
    State(service): State<Service>,
    Extension(owner): Extension<OwnerRef>,
    Path(task_id): Path<String>,
    Query(query): Query<ConductorReadQuery>,
) -> ProtocolResult<Json<ConductorReadResponse>> {
    Ok(Json(service.conductor(&owner, &task_id, query).await?))
}

async fn get_conversation(
    State(service): State<Service>,
    Extension(owner): Extension<OwnerRef>,
    Path(task_id): Path<String>,
) -> ProtocolResult<Json<ConversationResponse>> {
    Ok(Json(service.conversation(&owner, &task_id)?))
}

async fn get_conversation_page(
    State(service): State<Service>,
    Extension(owner): Extension<OwnerRef>,
    Path((task_id, attempt_id)): Path<(String, String)>,
    Query(query): Query<ConversationPageQuery>,
) -> ProtocolResult<Json<ConversationPage>> {
    Ok(Json(
        service
            .conversation_page(&owner, &task_id, &attempt_id, query)
            .await?,
    ))
}

async fn post_input(
    State(service): State<Service>,
    Extension(owner): Extension<OwnerRef>,
    Path(task_id): Path<String>,
    Json(request): Json<SubmitInputRequest>,
) -> ProtocolResult<(StatusCode, Json<SubmitInputResponse>)> {
    let response = service.submit_input(&owner, &task_id, request)?;
    Ok((StatusCode::ACCEPTED, Json(response)))
}

async fn post_operator_command(
    State(service): State<Service>,
    Extension(owner): Extension<OwnerRef>,
    Path(task_id): Path<String>,
    Json(request): Json<SubmitOperatorCommandRequest>,
) -> ProtocolResult<(StatusCode, Json<SubmitOperatorCommandResponse>)> {
    let response = service.submit_operator_command(&owner, &task_id, request)?;
    Ok((StatusCode::ACCEPTED, Json(response)))
}

async fn get_events(
    State(service): State<Service>,
    Extension(owner): Extension<OwnerRef>,
    Path(task_id): Path<String>,
    Query(query): Query<EventsAfterQuery>,
) -> ProtocolResult<Json<Vec<TaskEvent>>> {
    Ok(Json(service.events(&owner, &task_id, query.after)?))
}

async fn post_enrollment(
    State(service): State<Service>,
    operator: Option<Extension<Operator>>,
    Path(backend_id): Path<String>,
    Json(request): Json<CreateEnrollmentRequest>,
) -> ProtocolResult<(StatusCode, Json<EnrollmentResponse>)> {
    require_operator(operator)?;
    let response = service.create_enrollment(&backend_id, request)?;
    Ok((StatusCode::CREATED, Json(response)))
}

async fn list_controllers(
    State(service): State<Service>,
    operator: Option<Extension<Operator>>,
) -> ProtocolResult<Json<ControllerListResponse>> {
    require_operator(operator)?;
    Ok(Json(service.controllers()?))
}

async fn post_revoke_controller(
    State(service): State<Service>,
    operator: Option<Extension<Operator>>,
    Path(controller_id): Path<String>,
) -> ProtocolResult<Json<ControllerView>> {
    require_operator(operator)?;
    Ok(Json(service.revoke_controller(&controller_id)?))
}

// ---------------------------------------------------------------------------
// The durable event stream (KTD4)
// ---------------------------------------------------------------------------

/// What the stream unfolds over.
struct StreamState {
    service: Service,
    owner: OwnerRef,
    only: Option<TaskId>,
    cursor: StreamCursor,
    changes: broadcast::Receiver<TaskId>,
    queue: VecDeque<ControlEventEnvelope>,
}

/// `GET /control/v1/events/stream?cursor=…&task_id=…` — replay, then live.
///
/// The change subscription is taken *before* the replay is read, so a
/// mutation committed during the replay is either in it or announced after
/// it; either way the listener re-reads the durable log from its own cursor
/// and therefore delivers each event exactly once, in each task's order. A
/// listener that lagged behind the notifications rescans every task instead
/// of losing anything. The cursor comes from `cursor`, else from the
/// standard `Last-Event-ID` header an SSE client sends on reconnect.
async fn get_stream(
    State(service): State<Service>,
    Extension(owner): Extension<OwnerRef>,
    Query(query): Query<StreamQuery>,
    headers: HeaderMap,
) -> ProtocolResult<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>> {
    let text = query.cursor.or_else(|| {
        headers
            .get("last-event-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    });
    let mut cursor = match text {
        Some(text) => text
            .parse::<StreamCursor>()
            .map_err(|error| ProtocolError::bad_request("invalid_cursor", error.to_string()))?,
        None => StreamCursor::default(),
    };
    let only = match query.task_id {
        Some(task_id) => {
            let task_id = TaskId::parse(task_id).map_err(|_| ProtocolError::NotFound)?;
            if !service.owns(&owner, &task_id) {
                return Err(ProtocolError::NotFound);
            }
            Some(task_id)
        }
        None => None,
    };

    let changes = service.subscribe();
    let replay = service.catch_up(&owner, only.as_ref(), &mut cursor)?;
    let state = StreamState {
        service,
        owner,
        only,
        cursor,
        changes,
        queue: replay.into(),
    };
    let stream = stream::unfold(state, next_stream_event);
    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(SSE_KEEP_ALIVE_INTERVAL)
            .text("keep-alive"),
    ))
}

async fn next_stream_event(
    mut state: StreamState,
) -> Option<(Result<SseEvent, Infallible>, StreamState)> {
    loop {
        if let Some(envelope) = state.queue.pop_front() {
            let frame = SseEvent::default()
                .event("task")
                .id(envelope.cursor.clone())
                .json_data(&envelope);
            match frame {
                Ok(frame) => return Some((Ok(frame), state)),
                Err(error) => {
                    tracing::warn!(error = %error, "control stream: could not encode an event");
                    continue;
                }
            }
        }
        let only = match state.changes.recv().await {
            Ok(task_id) => {
                if state.only.as_ref().is_some_and(|only| *only != task_id) {
                    continue;
                }
                Some(task_id)
            }
            // Missed notifications: rescan everything this stream covers.
            Err(RecvError::Lagged(_)) => state.only.clone(),
            Err(RecvError::Closed) => return None,
        };
        match state
            .service
            .catch_up(&state.owner, only.as_ref(), &mut state.cursor)
        {
            Ok(events) => state.queue.extend(events),
            Err(error) => {
                tracing::warn!(error = %error, "control stream: could not read new events");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Controller handlers
// ---------------------------------------------------------------------------

async fn post_enroll(
    State(service): State<Service>,
    Json(request): Json<EnrollRequest>,
) -> ProtocolResult<(StatusCode, Json<EnrollResponse>)> {
    let response = service.enroll(request)?;
    Ok((StatusCode::CREATED, Json(response)))
}

async fn post_rotate_key(
    State(service): State<Service>,
    Extension(identity): Extension<ControllerIdentity>,
    Json(request): Json<RotateKeyRequest>,
) -> ProtocolResult<Json<RotateKeyResponse>> {
    Ok(Json(service.rotate_controller_key(&identity, request)?))
}

async fn post_heartbeat(
    State(service): State<Service>,
    Extension(identity): Extension<ControllerIdentity>,
) -> ProtocolResult<Json<HeartbeatResponse>> {
    Ok(Json(service.heartbeat(&identity)?))
}

async fn post_acquire_lease(
    State(service): State<Service>,
    Extension(identity): Extension<ControllerIdentity>,
    Json(request): Json<AcquireLeaseRequest>,
) -> ProtocolResult<Json<LeaseView>> {
    Ok(Json(service.acquire_lease(&identity, request)?))
}

async fn post_renew_lease(
    State(service): State<Service>,
    Extension(identity): Extension<ControllerIdentity>,
    Json(request): Json<LeaseEpochRequest>,
) -> ProtocolResult<Json<LeaseView>> {
    Ok(Json(service.renew_lease(&identity, request)?))
}

async fn post_release_lease(
    State(service): State<Service>,
    Extension(identity): Extension<ControllerIdentity>,
    Json(request): Json<LeaseEpochRequest>,
) -> ProtocolResult<Json<LeaseView>> {
    Ok(Json(service.release_lease(&identity, request)?))
}

async fn get_commands(
    State(service): State<Service>,
    Extension(identity): Extension<ControllerIdentity>,
    Query(query): Query<CommandsQuery>,
) -> ProtocolResult<Json<CommandsResponse>> {
    Ok(Json(service.wait_for_commands(&identity, query).await?))
}

async fn post_acks(
    State(service): State<Service>,
    Extension(identity): Extension<ControllerIdentity>,
    Json(request): Json<AckRequest>,
) -> ProtocolResult<Json<AckResponse>> {
    Ok(Json(service.acknowledge(&identity, request)?))
}

async fn post_report(
    State(service): State<Service>,
    Extension(identity): Extension<ControllerIdentity>,
    Json(request): Json<ReportRequest>,
) -> ProtocolResult<Json<ReportResponse>> {
    Ok(Json(service.report(&identity, request)?))
}

async fn post_runner_credential(
    State(service): State<Service>,
    Extension(identity): Extension<ControllerIdentity>,
    Path(task_id): Path<String>,
    Json(request): Json<RunnerCredentialRequest>,
) -> ProtocolResult<(StatusCode, Json<RunnerCredentialResponse>)> {
    let response = service.issue_runner_credential(&identity, &task_id, request)?;
    Ok((StatusCode::CREATED, Json(response)))
}

// ---------------------------------------------------------------------------
// Runner handlers
// ---------------------------------------------------------------------------

async fn post_snapshot(
    State(service): State<Service>,
    Extension(identity): Extension<RunnerIdentity>,
    Json(snapshot): Json<RunnerSnapshotRequest>,
) -> ProtocolResult<Json<RunnerSnapshotResponse>> {
    Ok(Json(service.runner_snapshot(&identity, snapshot)?))
}

async fn get_poll(
    State(service): State<Service>,
    Extension(identity): Extension<RunnerIdentity>,
    Query(query): Query<RunnerPollQuery>,
) -> ProtocolResult<Json<RunnerPollResponse>> {
    Ok(Json(service.runner_poll(&identity, query).await?))
}

async fn post_renew(
    State(service): State<Service>,
    Extension(identity): Extension<RunnerIdentity>,
) -> ProtocolResult<Json<RunnerRenewResponse>> {
    Ok(Json(service.runner_renew(&identity)?))
}

async fn post_input_ack(
    State(service): State<Service>,
    Extension(identity): Extension<RunnerIdentity>,
    Path(input_id): Path<String>,
    Json(request): Json<InputAckRequest>,
) -> ProtocolResult<Json<InputAckResponse>> {
    Ok(Json(
        service.runner_ack_input(&identity, &input_id, request)?,
    ))
}

async fn post_command_ack(
    State(service): State<Service>,
    Extension(identity): Extension<RunnerIdentity>,
    Path(command_id): Path<String>,
    Json(request): Json<OperatorCommandAckRequest>,
) -> ProtocolResult<Json<OperatorCommandAckResponse>> {
    Ok(Json(service.runner_ack_command(
        &identity,
        &command_id,
        request,
    )?))
}

async fn post_conversation_page(
    State(service): State<Service>,
    Extension(identity): Extension<RunnerIdentity>,
    Path(request_id): Path<String>,
    Json(page): Json<ConversationPage>,
) -> ProtocolResult<Json<ConversationPageAccepted>> {
    Ok(Json(service.runner_conversation_page(
        &identity,
        &request_id,
        page,
    )?))
}

async fn post_conductor_read(
    State(service): State<Service>,
    Extension(identity): Extension<RunnerIdentity>,
    Path(request_id): Path<String>,
    Json(view): Json<crate::conductor::ConductorView>,
) -> ProtocolResult<Json<ConversationPageAccepted>> {
    Ok(Json(service.runner_conductor_read(
        &identity,
        &request_id,
        view,
    )?))
}

async fn post_execution_events(
    State(service): State<Service>,
    Extension(identity): Extension<RunnerIdentity>,
    Path(request_id): Path<String>,
    Json(page): Json<ExecutionEventsPage>,
) -> ProtocolResult<Json<ConversationPageAccepted>> {
    Ok(Json(service.runner_execution_events(
        &identity,
        &request_id,
        page,
    )?))
}

// ---------------------------------------------------------------------------
// Materials (U9)
// ---------------------------------------------------------------------------

/// Refuses an upload body that does not declare itself a material bundle,
/// so nothing else is ever parsed as one.
fn require_bundle(headers: &HeaderMap) -> ProtocolResult<()> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if content_type.split(';').next().map(str::trim) == Some(BUNDLE_CONTENT_TYPE) {
        Ok(())
    } else {
        Err(ProtocolError::bad_request(
            "invalid_content_type",
            format!("a material upload is sent as {BUNDLE_CONTENT_TYPE}"),
        ))
    }
}

/// A stored blob as an opaque octet stream.
async fn blob_response(path: std::path::PathBuf) -> ProtocolResult<Response> {
    let bytes = tokio::fs::read(&path).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            // Deleted or expired between the lookup and the read.
            ProtocolError::NotFound
        } else {
            ProtocolError::Internal(format!("cannot read '{}': {error}", path.display()))
        }
    })?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

async fn post_material(
    State(service): State<Service>,
    Extension(owner): Extension<OwnerRef>,
    Path(task_id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> ProtocolResult<(StatusCode, Json<MaterialUploadResponse>)> {
    require_bundle(&headers)?;
    let response = service
        .upload_material(&owner, &task_id, body.into_data_stream())
        .await?;
    let status = if response.replayed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((status, Json(response)))
}

async fn list_materials(
    State(service): State<Service>,
    Extension(owner): Extension<OwnerRef>,
    Path(task_id): Path<String>,
) -> ProtocolResult<Json<MaterialListResponse>> {
    Ok(Json(service.list_materials(&owner, &task_id)?))
}

async fn get_material(
    State(service): State<Service>,
    Extension(owner): Extension<OwnerRef>,
    Path((task_id, material_id)): Path<(String, String)>,
) -> ProtocolResult<Json<MaterialView>> {
    Ok(Json(service.material(&owner, &task_id, &material_id)?))
}

async fn get_material_blob(
    State(service): State<Service>,
    Extension(owner): Extension<OwnerRef>,
    Path((task_id, material_id, sha256)): Path<(String, String, String)>,
) -> ProtocolResult<Response> {
    blob_response(service.material_blob(&owner, &task_id, &material_id, &sha256)?).await
}

async fn delete_material(
    State(service): State<Service>,
    Extension(owner): Extension<OwnerRef>,
    Path((task_id, material_id)): Path<(String, String)>,
) -> ProtocolResult<Json<MaterialSummary>> {
    Ok(Json(service.delete_material(
        &owner,
        &task_id,
        &material_id,
    )?))
}

async fn post_runner_material(
    State(service): State<Service>,
    Extension(identity): Extension<RunnerIdentity>,
    headers: HeaderMap,
    body: Body,
) -> ProtocolResult<(StatusCode, Json<MaterialUploadResponse>)> {
    require_bundle(&headers)?;
    let response = service
        .runner_publish_material(&identity, body.into_data_stream())
        .await?;
    let status = if response.replayed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((status, Json(response)))
}

async fn get_runner_material(
    State(service): State<Service>,
    Extension(identity): Extension<RunnerIdentity>,
    Path(material_id): Path<String>,
) -> ProtocolResult<Json<MaterialView>> {
    Ok(Json(service.runner_material(&identity, &material_id)?))
}

async fn get_runner_material_blob(
    State(service): State<Service>,
    Extension(identity): Extension<RunnerIdentity>,
    Path((material_id, sha256)): Path<(String, String)>,
) -> ProtocolResult<Response> {
    blob_response(service.runner_material_blob(&identity, &material_id, &sha256)?).await
}

async fn post_runner_material_ack(
    State(service): State<Service>,
    Extension(identity): Extension<RunnerIdentity>,
    Path(material_id): Path<String>,
    Json(request): Json<MaterialAckRequest>,
) -> ProtocolResult<Json<MaterialAckResponse>> {
    Ok(Json(service.runner_ack_material(
        &identity,
        &material_id,
        request,
    )?))
}
