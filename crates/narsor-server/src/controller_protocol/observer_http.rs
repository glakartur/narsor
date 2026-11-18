//! The observers' HTTP routes outside the owner gate, and the owner's routes
//! of the observers.
//!
//! The effect routes of an observer run's script (U3), behind the observer
//! run gate ([`crate::auth_gate::require_observer_run`]):
//!
//! | route | body | answer |
//! | --- | --- | --- |
//! | `POST /observer/v1/runs/{run}/tasks` | [`ObserverCreateTaskRequest`] | `201` created |
//! | `POST /observer/v1/runs/{run}/decide` | [`ObserverDecideRequest`] | `200` delivered |
//!
//! Each answers an [`ObserverEffectResponse`]. `decide` answers the gate of
//! the queue entry the run was started for, and no other (KTD-6). They are merged outside the
//! owner gate, which never accepts a run token; the run gate in turn
//! accepts nothing but the token of a run in progress before its deadline,
//! and only for that run's own path. Refusals of the gate are a bare `401`;
//! a refused effect is a coded `400` or `409` the script can print.
//!
//! The worker routes (U6), behind the observer worker gate
//! ([`crate::auth_gate::require_observer_worker`]). The worker is the
//! server's own, started with a key the server makes at each of its starts
//! (D5); it claims the runs of every owner's observers:
//!
//! | route | body | answer |
//! | --- | --- | --- |
//! | `POST /observer/v1/worker/claim` | none | `200` [`ObserverClaimResponse`] |
//! | `POST /observer/v1/worker/runs/{run}/result` | [`ObserverRunReport`] | `200` [`ObserverRunResultResponse`], `404` no such run, `409` ended |
//!
//! The owner's routes for the observers (U7, R21), behind the owner gate
//! ([`observer_owner_routes`]); `{observer}` is a name or an id, and
//! another owner's observer is a bare `404`:
//!
//! | route | body | answer |
//! | --- | --- | --- |
//! | `GET /control/v1/observers` | none | [`ObserverListResponse`] |
//! | `POST /control/v1/observers` | [`CreateObserverRequest`] | `201` [`ObserverView`] |
//! | `GET /control/v1/observers/{observer}` | none | [`ObserverDetailView`] |
//! | `PATCH /control/v1/observers/{observer}` | [`UpdateObserverRequest`] | [`ObserverView`] |
//! | `DELETE /control/v1/observers/{observer}` | none | [`ObserverView`] |
//! | `POST /control/v1/observers/{observer}/scripts` | [`SetObserverScriptRequest`] | `201` [`ObserverScriptView`] |
//! | `GET /control/v1/observers/{observer}/scripts/{version}` | none | [`ObserverScriptTextView`] |
//! | `POST /control/v1/observers/{observer}/approve` | [`ApproveObserverRequest`] | [`ObserverView`] |
//! | `POST /control/v1/observers/{observer}/pause` | none | [`ObserverView`] |
//! | `POST /control/v1/observers/{observer}/resume` | none | [`ObserverView`] |
//! | `GET /control/v1/observer-queues` | none | [`ObserverQueueListResponse`] |
//! | `GET /control/v1/observer-queues/{queue}` | none | [`ObserverQueueView`] |
//!
//! A queue is a name in the owner's namespace (R7): another owner's queue
//! of the same name is a bare `404`, as is a name nothing of the caller's
//! names.
//!
//! They need the owner gate's [`OwnerRef`] and [`Operator`]: anyone else
//! gets a bare `403`.
//!
//! Each credential opens its own routes only: a run token is refused on the
//! worker routes, the worker key on the effect routes and the owner's.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::de::DeserializeOwned;

use super::http::{blocking, HttpProtocolError, ProtocolResult};
use crate::auth_gate::{require_observer_run, require_observer_worker, Operator, Principal};
use narsor_engine::controller_protocol::error::ProtocolError;
use narsor_engine::controller_protocol::service::{
    ProtocolService, MAX_EFFECT_DATA_BYTES, MAX_STORED_LOG_BYTES,
};
use narsor_engine::controller_protocol::wire::{
    ApproveObserverRequest, CreateObserverRequest, ObserverClaimResponse,
    ObserverCreateTaskRequest, ObserverDecideRequest, ObserverDetailView, ObserverEffectResponse,
    ObserverListResponse, ObserverQueueListResponse, ObserverQueueView, ObserverRunReport,
    ObserverRunResultResponse, ObserverScriptTextView, ObserverScriptView, ObserverView,
    SetObserverScriptRequest, UpdateObserverRequest,
};
use narsor_engine::observers::MAX_SCRIPT_BYTES;
use narsor_engine::observers::{RunId, MAX_SCRIPT_OUTPUT_BYTES};
use narsor_engine::orchestration::OwnerRef;

type Service = Arc<ProtocolService>;

/// Largest effect request body: the data's own limit plus room for the key
/// and the type. Anything larger is refused before it is read.
const MAX_EFFECT_BODY: usize = MAX_EFFECT_DATA_BYTES + 16 * 1024;

/// Largest result body a worker may send: the contract's stdout, the
/// stored stderr, each of which JSON escaping may double, and room for the
/// outcome.
const MAX_RESULT_BODY: usize = 2 * (MAX_SCRIPT_OUTPUT_BYTES + MAX_STORED_LOG_BYTES) + 16 * 1024;

/// Largest owner request body: a script, which JSON escaping may double,
/// and room for the rest.
const MAX_OWNER_BODY: usize = 2 * MAX_SCRIPT_BYTES + 64 * 1024;

/// The routes outside the owner gate: the effect routes behind the observer
/// run gate, the worker routes behind the observer worker gate.
pub fn observer_routes<S>(service: Service) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    effect_routes(Arc::clone(&service)).merge(worker_routes(service))
}

/// The worker routes, behind the observer worker gate.
fn worker_routes<S>(service: Service) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/observer/v1/worker/claim", post(post_claim))
        .route("/observer/v1/worker/runs/{run}/result", post(post_result))
        .layer(DefaultBodyLimit::max(MAX_RESULT_BODY))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&service),
            require_observer_worker,
        ))
        .with_state(service)
}

/// The owner's observer routes; they expect the owner gate's [`OwnerRef`]
/// and [`Operator`] in the request extensions.
pub fn observer_owner_routes<S>(service: Service) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route(
            "/control/v1/observers",
            get(list_observers).post(post_observer),
        )
        .route(
            "/control/v1/observers/{observer}",
            get(get_observer)
                .patch(patch_observer)
                .delete(delete_observer),
        )
        .route(
            "/control/v1/observers/{observer}/scripts",
            post(post_script),
        )
        .route(
            "/control/v1/observers/{observer}/scripts/{version}",
            get(get_script),
        )
        .route(
            "/control/v1/observers/{observer}/approve",
            post(post_approve),
        )
        .route("/control/v1/observers/{observer}/pause", post(post_pause))
        .route("/control/v1/observers/{observer}/resume", post(post_resume))
        .route("/control/v1/observer-queues", get(list_queues))
        .route("/control/v1/observer-queues/{queue}", get(get_queue))
        .layer(DefaultBodyLimit::max(MAX_OWNER_BODY))
        .with_state(service)
}

/// The effect routes, behind the observer run gate.
fn effect_routes<S>(service: Service) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/observer/v1/runs/{run}/tasks", post(post_task))
        .route("/observer/v1/runs/{run}/decide", post(post_decide))
        .layer(DefaultBodyLimit::max(MAX_EFFECT_BODY))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&service),
            require_observer_run,
        ))
        .with_state(service)
}

/// The run the gate authenticated, if it is the run the path names: a token
/// of one run never acts for another (a bare `401` otherwise).
fn token_run(principal: &Principal, run: &str) -> ProtocolResult<RunId> {
    match principal {
        Principal::ObserverRun { run_id } if run_id.to_string() == run => Ok(run_id.clone()),
        _ => Err(HttpProtocolError(ProtocolError::Unauthorized)),
    }
}

/// Parses a request body, refusing a malformed one, or one with a field the
/// caller may not set, with a coded `400` naming what is wrong.
fn parse_json<T: DeserializeOwned>(body: &Bytes, what: &str) -> ProtocolResult<T> {
    serde_json::from_slice(body).map_err(|error| {
        HttpProtocolError(ProtocolError::bad_request(
            "invalid_request",
            format!("invalid {what}: {error}"),
        ))
    })
}

async fn post_task(
    State(service): State<Service>,
    Extension(principal): Extension<Principal>,
    Path(run): Path<String>,
    body: Bytes,
) -> ProtocolResult<(StatusCode, Json<ObserverEffectResponse>)> {
    let run_id = token_run(&principal, &run)?;
    let request: ObserverCreateTaskRequest = parse_json(&body, "effect request")?;
    let response = blocking(&service, move |s| s.observer_create_task(&run_id, request)).await?;
    Ok((StatusCode::CREATED, Json(response)))
}

async fn post_decide(
    State(service): State<Service>,
    Extension(principal): Extension<Principal>,
    Path(run): Path<String>,
    body: Bytes,
) -> ProtocolResult<Json<ObserverEffectResponse>> {
    let run_id = token_run(&principal, &run)?;
    let request: ObserverDecideRequest = parse_json(&body, "answer")?;
    let response = blocking(&service, move |s| s.observer_decide(&run_id, request)).await?;
    Ok(Json(response))
}

/// The worker gate's principal (a bare `401` for any other, which the gate
/// never lets through anyway).
fn require_worker(principal: &Principal) -> ProtocolResult<()> {
    match principal {
        Principal::ObserverWorker => Ok(()),
        _ => Err(HttpProtocolError(ProtocolError::Unauthorized)),
    }
}

async fn post_claim(
    State(service): State<Service>,
    Extension(principal): Extension<Principal>,
) -> ProtocolResult<Json<ObserverClaimResponse>> {
    require_worker(&principal)?;
    let claimed = blocking(&service, |s| s.claim_observer_run()).await?;
    Ok(Json(ObserverClaimResponse {
        run: claimed.map(|claimed| claimed.into_wire()),
    }))
}

async fn post_result(
    State(service): State<Service>,
    Extension(principal): Extension<Principal>,
    Path(run): Path<String>,
    body: Bytes,
) -> ProtocolResult<Json<ObserverRunResultResponse>> {
    require_worker(&principal)?;
    let run_id = RunId::parse(&run).map_err(|_| HttpProtocolError(ProtocolError::NotFound))?;
    let report: ObserverRunReport = parse_json(&body, "run result")?;
    let finished_run = run_id.to_string();
    let run = blocking(&service, move |s| s.finish_observer_run(&run_id, report)).await?;
    let status = serde_json::to_value(run.status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default();
    Ok(Json(ObserverRunResultResponse {
        run_id: finished_run,
        status,
        error: run.error,
    }))
}

/// The owner and operator an owner route needs (a bare `403` without
/// them).
fn require_owner_operator(
    owner: Option<Extension<OwnerRef>>,
    operator: Option<Extension<Operator>>,
) -> ProtocolResult<OwnerRef> {
    match (owner, operator) {
        (Some(Extension(owner)), Some(_)) => Ok(owner),
        _ => Err(HttpProtocolError(ProtocolError::Forbidden)),
    }
}

// ---------------------------------------------------------------------------
// The owner's observer routes
// ---------------------------------------------------------------------------

async fn list_observers(
    State(service): State<Service>,
    owner: Option<Extension<OwnerRef>>,
    operator: Option<Extension<Operator>>,
) -> ProtocolResult<Json<ObserverListResponse>> {
    let owner = require_owner_operator(owner, operator)?;
    Ok(Json(
        blocking(&service, move |s| s.observers_of(&owner)).await?,
    ))
}

async fn post_observer(
    State(service): State<Service>,
    owner: Option<Extension<OwnerRef>>,
    operator: Option<Extension<Operator>>,
    body: Bytes,
) -> ProtocolResult<(StatusCode, Json<ObserverView>)> {
    let owner = require_owner_operator(owner, operator)?;
    let request: CreateObserverRequest = parse_json(&body, "observer request")?;
    let created = blocking(&service, move |s| s.create_observer(&owner, request)).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

async fn get_observer(
    State(service): State<Service>,
    owner: Option<Extension<OwnerRef>>,
    operator: Option<Extension<Operator>>,
    Path(observer): Path<String>,
) -> ProtocolResult<Json<ObserverDetailView>> {
    let owner = require_owner_operator(owner, operator)?;
    Ok(Json(
        blocking(&service, move |s| s.observer_detail(&owner, &observer)).await?,
    ))
}

async fn patch_observer(
    State(service): State<Service>,
    owner: Option<Extension<OwnerRef>>,
    operator: Option<Extension<Operator>>,
    Path(observer): Path<String>,
    body: Bytes,
) -> ProtocolResult<Json<ObserverView>> {
    let owner = require_owner_operator(owner, operator)?;
    let request: UpdateObserverRequest = parse_json(&body, "observer update")?;
    Ok(Json(
        blocking(&service, move |s| {
            s.update_observer(&owner, &observer, request)
        })
        .await?,
    ))
}

async fn list_queues(
    State(service): State<Service>,
    owner: Option<Extension<OwnerRef>>,
    operator: Option<Extension<Operator>>,
) -> ProtocolResult<Json<ObserverQueueListResponse>> {
    let owner = require_owner_operator(owner, operator)?;
    Ok(Json(
        blocking(&service, move |s| s.observer_queues_of(&owner)).await?,
    ))
}

async fn get_queue(
    State(service): State<Service>,
    owner: Option<Extension<OwnerRef>>,
    operator: Option<Extension<Operator>>,
    Path(queue): Path<String>,
) -> ProtocolResult<Json<ObserverQueueView>> {
    let owner = require_owner_operator(owner, operator)?;
    Ok(Json(
        blocking(&service, move |s| s.observer_queue(&owner, &queue)).await?,
    ))
}

async fn delete_observer(
    State(service): State<Service>,
    owner: Option<Extension<OwnerRef>>,
    operator: Option<Extension<Operator>>,
    Path(observer): Path<String>,
) -> ProtocolResult<Json<ObserverView>> {
    let owner = require_owner_operator(owner, operator)?;
    Ok(Json(
        blocking(&service, move |s| s.delete_observer(&owner, &observer)).await?,
    ))
}

async fn post_script(
    State(service): State<Service>,
    owner: Option<Extension<OwnerRef>>,
    operator: Option<Extension<Operator>>,
    Path(observer): Path<String>,
    body: Bytes,
) -> ProtocolResult<(StatusCode, Json<ObserverScriptView>)> {
    let owner = require_owner_operator(owner, operator)?;
    let request: SetObserverScriptRequest = parse_json(&body, "observer request")?;
    let saved = blocking(&service, move |s| {
        s.add_observer_script(&owner, &observer, &request.script)
    })
    .await?;
    Ok((StatusCode::CREATED, Json(saved)))
}

async fn get_script(
    State(service): State<Service>,
    owner: Option<Extension<OwnerRef>>,
    operator: Option<Extension<Operator>>,
    Path((observer, version)): Path<(String, u32)>,
) -> ProtocolResult<Json<ObserverScriptTextView>> {
    let owner = require_owner_operator(owner, operator)?;
    Ok(Json(
        blocking(&service, move |s| {
            s.observer_script(&owner, &observer, version)
        })
        .await?,
    ))
}

async fn post_approve(
    State(service): State<Service>,
    owner: Option<Extension<OwnerRef>>,
    operator: Option<Extension<Operator>>,
    Path(observer): Path<String>,
    body: Bytes,
) -> ProtocolResult<Json<ObserverView>> {
    let owner = require_owner_operator(owner, operator)?;
    let request: ApproveObserverRequest = parse_json(&body, "observer request")?;
    Ok(Json(
        blocking(&service, move |s| {
            s.approve_observer_version_for(&owner, &observer, request.version)
        })
        .await?,
    ))
}

async fn post_pause(
    State(service): State<Service>,
    owner: Option<Extension<OwnerRef>>,
    operator: Option<Extension<Operator>>,
    Path(observer): Path<String>,
) -> ProtocolResult<Json<ObserverView>> {
    let owner = require_owner_operator(owner, operator)?;
    Ok(Json(
        blocking(&service, move |s| s.pause_observer_for(&owner, &observer)).await?,
    ))
}

async fn post_resume(
    State(service): State<Service>,
    owner: Option<Extension<OwnerRef>>,
    operator: Option<Extension<Operator>>,
    Path(observer): Path<String>,
) -> ProtocolResult<Json<ObserverView>> {
    let owner = require_owner_operator(owner, operator)?;
    Ok(Json(
        blocking(&service, move |s| s.resume_observer_for(&owner, &observer)).await?,
    ))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use axum::body::Body;
    use axum::extract::Request;
    use axum::http::header;
    use axum::response::Response;
    use chrono::{DateTime, Duration, TimeZone, Utc};
    use narsor_engine::controller_protocol::{ClaimedObserverRun, RunOutcome, RunReport};
    use narsor_engine::domain::{BackendId, WorkflowDefinition};
    use narsor_engine::environment::{
        BackendRecord, LaunchError, LaunchIntent, LaunchResolver, ResolvedLaunch,
        SlotReleaseTimings,
    };
    use narsor_engine::observers::{
        CreateTaskEffect, ObserverConfig, ObserverEffect, ObserverId, ObserverMode, ObserverName,
    };
    use narsor_engine::orchestration::{Clock, ControlPlane, OwnerRef};
    use narsor_engine::persistence::StatePaths;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use super::*;
    use crate::auth_gate::{require_owner, OwnerAuth};
    use crate::controller_protocol::http::owner_routes;

    const OWNER_TOKEN: &str = "owner-token";

    #[derive(Debug)]
    struct ManualClock(Mutex<DateTime<Utc>>);

    impl Clock for ManualClock {
        fn now(&self) -> DateTime<Utc> {
            *self.0.lock().unwrap()
        }
    }

    /// Launches nothing: these routes are tested on their gates and
    /// refusals.
    struct NoLaunches;

    impl LaunchResolver for NoLaunches {
        fn resolve(&self, _intent: &LaunchIntent) -> Result<ResolvedLaunch, LaunchError> {
            Err(LaunchError::UnknownBackend("k8s".to_string()))
        }

        fn check_backend(&self, backend: &BackendId) -> Result<(), LaunchError> {
            Err(LaunchError::UnknownBackend(backend.to_string()))
        }

        fn slot_release_timings(
            &self,
            backend: &BackendId,
        ) -> Result<SlotReleaseTimings, LaunchError> {
            Err(LaunchError::UnknownBackend(backend.to_string()))
        }

        fn configured_default_backend(&self) -> String {
            "k8s".to_string()
        }

        fn backend_record(
            &self,
            _backend: &BackendId,
        ) -> Result<Option<BackendRecord>, LaunchError> {
            Ok(None)
        }

        fn workflow_definition(
            &self,
            workflow_id: &str,
        ) -> Result<WorkflowDefinition, LaunchError> {
            if workflow_id == "triage" {
                Ok(triage())
            } else {
                Err(LaunchError::UnknownWorkflow {
                    workflow: workflow_id.to_string(),
                    message: "not in the catalogue".to_string(),
                })
            }
        }
    }

    /// The `triage` workflow observers create tasks of.
    fn triage() -> WorkflowDefinition {
        serde_json::from_value(json!({
            "id": "triage",
            "name": "triage",
            "version": 1,
            "inputs": [{"name": "ticket", "required": true}],
            "states": {},
            "initial_state": "start",
        }))
        .unwrap()
    }

    /// The daemon's arrangement of the two gates: owner routes behind the
    /// owner gate, the effect routes merged outside it.
    struct Fixture {
        root: PathBuf,
        clock: Arc<ManualClock>,
        service: Service,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn owner() -> OwnerRef {
        OwnerRef::new("local")
    }

    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("narsor-observer-http-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let clock = Arc::new(ManualClock(Mutex::new(
                Utc.with_ymd_and_hms(2026, 11, 13, 12, 0, 0).unwrap(),
            )));
            let service = Arc::new(ProtocolService::new(
                ControlPlane::with_clock(StatePaths::new(&root), clock.clone()),
                Arc::new(NoLaunches),
            ));
            Self {
                root,
                clock,
                service,
            }
        }

        fn now(&self) -> DateTime<Utc> {
            self.clock.now()
        }

        fn router(&self) -> Router {
            owner_routes(Arc::clone(&self.service))
                .layer(middleware::from_fn_with_state(
                    OwnerAuth::Trusted {
                        token: OWNER_TOKEN.to_string(),
                    },
                    require_owner,
                ))
                .merge(observer_routes(Arc::clone(&self.service)))
        }

        /// An active observer of `effect`, approved and due.
        fn observer(&self, name: &str, effect: ObserverEffect, mode: ObserverMode) -> ObserverId {
            self.observer_of(owner(), name, effect, mode)
        }

        /// The same, of `owner`.
        fn observer_of(
            &self,
            owner: OwnerRef,
            name: &str,
            effect: ObserverEffect,
            mode: ObserverMode,
        ) -> ObserverId {
            let config = ObserverConfig::new(ObserverName::parse(name).unwrap(), effect, mode);
            let workflow = triage();
            let store = self.service.observers();
            let id = store
                .create(owner, config, Some(&workflow), self.now())
                .unwrap()
                .observer_id;
            store
                .add_script(&id, "echo '{\"state\": {}}'\n", self.now())
                .unwrap();
            self.service.approve_observer_version(&id, 1).unwrap();
            id
        }

        /// The claimed run of an active "create a task" observer.
        fn effect_run(&self) -> ClaimedObserverRun {
            jira_observer(self);
            self.service.observer_pass();
            self.service.claim_observer_run().unwrap().unwrap()
        }

        async fn send(&self, method: &str, uri: &str, token: &str, body: Value) -> Response {
            let request = Request::builder()
                .method(method)
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap();
            self.router().oneshot(request).await.unwrap()
        }
    }

    async fn json_body(response: Response) -> Value {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn tasks_uri(run: &ClaimedObserverRun) -> String {
        format!("/observer/v1/runs/{}/tasks", run.run_id)
    }

    fn effect() -> Value {
        json!({"data": {"key": "ABC-1"}})
    }

    #[tokio::test]
    async fn the_gate_refuses_no_token_a_wrong_one_and_another_runs_path() {
        let fx = Fixture::new();
        let run = fx.effect_run();
        let uri = tasks_uri(&run);

        let request = Request::post(&uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(effect().to_string()))
            .unwrap();
        let no_token = fx.router().oneshot(request).await.unwrap();
        assert_eq!(no_token.status(), StatusCode::UNAUTHORIZED);

        let forged = format!("{}x", run.token.expose());
        let wrong = fx.send("POST", &uri, &forged, effect()).await;
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
        let owner = fx.send("POST", &uri, OWNER_TOKEN, effect()).await;
        assert_eq!(owner.status(), StatusCode::UNAUTHORIZED);

        let other_run = format!(
            "/observer/v1/runs/{}.{}/tasks",
            run.run_id.observer_id,
            run.run_id.sequence + 1
        );
        let elsewhere = fx
            .send("POST", &other_run, run.token.expose(), effect())
            .await;
        assert_eq!(elsewhere.status(), StatusCode::UNAUTHORIZED);
        assert!(json_body_is_empty(elsewhere).await);
    }

    async fn json_body_is_empty(response: Response) -> bool {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .is_empty()
    }

    #[tokio::test]
    async fn the_decide_route_is_the_runs_and_refuses_a_run_with_no_entry() {
        let fx = Fixture::new();
        let run = fx.effect_run();
        let uri = format!("/observer/v1/runs/{}/decide", run.run_id);
        let answer = json!({
            "task_id": "task-1",
            "episode": "input-1",
            "choice": "check_feedback",
            "reason": "label removed",
        });

        let owner = fx.send("POST", &uri, OWNER_TOKEN, answer.clone()).await;
        assert_eq!(owner.status(), StatusCode::UNAUTHORIZED);
        let no_entry = fx
            .send("POST", &uri, run.token.expose(), answer.clone())
            .await;
        assert_eq!(no_entry.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json_body(no_entry).await["code"], "no_queue_entry");
        let mut artifacts = answer;
        artifacts["artifacts"] = json!({"report": "out.md"});
        let refused = fx.send("POST", &uri, run.token.expose(), artifacts).await;
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json_body(refused).await["code"], "invalid_request");
    }

    #[tokio::test]
    async fn a_run_token_opens_no_owner_route() {
        let fx = Fixture::new();
        let run = fx.effect_run();

        let list = fx
            .send("GET", "/control/v1/tasks", run.token.expose(), Value::Null)
            .await;

        assert_eq!(list.status(), StatusCode::UNAUTHORIZED);
        let owner = fx
            .send("GET", "/control/v1/tasks", OWNER_TOKEN, Value::Null)
            .await;
        assert_eq!(owner.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_token_stops_working_at_the_result_and_at_the_deadline() {
        let fx = Fixture::new();
        let run = fx.effect_run();
        fx.service
            .finish_observer_run(
                &run.run_id,
                RunReport {
                    outcome: RunOutcome::Exited { exit_code: 0 },
                    stdout: "{\"state\": {}}".to_string(),
                    stderr: String::new(),
                },
            )
            .unwrap();
        let ended = fx
            .send("POST", &tasks_uri(&run), run.token.expose(), effect())
            .await;
        assert_eq!(ended.status(), StatusCode::UNAUTHORIZED);

        // The next run, left past its deadline without a result.
        *fx.clock.0.lock().unwrap() += Duration::seconds(300);
        fx.service.observer_pass();
        let next = fx.service.claim_observer_run().unwrap().unwrap();
        *fx.clock.0.lock().unwrap() +=
            Duration::seconds(i64::try_from(next.timeout_secs).unwrap() + 3600);
        let late = fx
            .send("POST", &tasks_uri(&next), next.token.expose(), effect())
            .await;
        assert_eq!(late.status(), StatusCode::UNAUTHORIZED);
        assert!(fx.service.control_plane().list_tasks().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_create_request_naming_a_workflow_or_backend_is_a_coded_400() {
        let fx = Fixture::new();
        let id = fx.observer(
            "jira-new",
            ObserverEffect::CreateTask(CreateTaskEffect {
                workflow_id: "triage".into(),
                workflow_version: 1,
                workspaces: Default::default(),
                backend_id: None,
                profile: None,
                mapping: [("ticket".to_string(), "key".to_string())].into(),
            }),
            ObserverMode::Always,
        );
        assert!(fx.service.observer_pass().due.contains(&id));
        let run = fx.service.claim_observer_run().unwrap().unwrap();
        let uri = format!("/observer/v1/runs/{}/tasks", run.run_id);

        for field in ["workflow_id", "backend_id", "inputs"] {
            let mut body = json!({"data": {"key": "ABC-1"}});
            body[field] = json!("other");
            let refused = fx.send("POST", &uri, run.token.expose(), body).await;
            assert_eq!(refused.status(), StatusCode::BAD_REQUEST, "{field}");
            let error = json_body(refused).await;
            assert_eq!(error["code"], "invalid_request");
            assert!(
                error["message"].as_str().unwrap().contains(field),
                "{error}"
            );
        }
        assert!(fx.service.control_plane().list_tasks().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_oversized_effect_body_is_refused_before_it_is_read() {
        let fx = Fixture::new();
        let run = fx.effect_run();
        let body = json!({"data": {"key": "x".repeat(MAX_EFFECT_BODY)}});

        let refused = fx
            .send("POST", &tasks_uri(&run), run.token.expose(), body)
            .await;

        assert_eq!(refused.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// An active "create a task" observer, due once the pass runs.
    fn jira_observer(fx: &Fixture) -> ObserverId {
        fx.observer(
            "jira-all",
            ObserverEffect::CreateTask(CreateTaskEffect {
                workflow_id: "triage".into(),
                workflow_version: 1,
                workspaces: Default::default(),
                backend_id: None,
                profile: None,
                mapping: [("ticket".to_string(), "key".to_string())].into(),
            }),
            ObserverMode::Always,
        )
    }

    /// The worker key, as the server makes it when it starts the worker.
    fn worker_key(fx: &Fixture) -> String {
        fx.service.issue_observer_worker_key().expose().to_string()
    }

    #[tokio::test]
    async fn a_worker_claims_a_run_and_reports_its_result() {
        let fx = Fixture::new();
        let credential = worker_key(&fx);
        let id = jira_observer(&fx);
        fx.service.observer_pass();

        let claim = fx
            .send(
                "POST",
                "/observer/v1/worker/claim",
                &credential,
                Value::Null,
            )
            .await;

        assert_eq!(claim.status(), StatusCode::OK);
        let run = json_body(claim).await["run"].clone();
        let run_id = run["run_id"].as_str().unwrap().to_string();
        assert!(run_id.starts_with(id.as_str()), "{run_id}");
        assert!(run["token"].as_str().unwrap().starts_with("nso."));
        assert_eq!(run["owner"], "local");
        assert_eq!(run["input"]["observer"]["name"], "jira-all");
        assert_eq!(run["script"], "echo '{\"state\": {}}'\n");
        let empty = fx
            .send(
                "POST",
                "/observer/v1/worker/claim",
                &credential,
                Value::Null,
            )
            .await;
        assert_eq!(json_body(empty).await["run"], Value::Null);

        let result = fx
            .send(
                "POST",
                &format!("/observer/v1/worker/runs/{run_id}/result"),
                &credential,
                json!({"outcome": {"kind": "exited", "exit_code": 0}, "stdout": "{\"state\": {\"cursor\": 5}}"}),
            )
            .await;
        assert_eq!(result.status(), StatusCode::OK);
        let body = json_body(result).await;
        assert_eq!(body["status"], "succeeded");
        assert_eq!(body["run_id"], run_id.as_str());
        let record = fx.service.observers().load(&id).unwrap();
        assert_eq!(record.script_state, Some(json!({"cursor": 5})));

        let again = fx
            .send(
                "POST",
                &format!("/observer/v1/worker/runs/{run_id}/result"),
                &credential,
                json!({"outcome": {"kind": "exited", "exit_code": 0}, "stdout": ""}),
            )
            .await;
        assert_eq!(again.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn the_owner_sees_only_the_errors_of_failed_runs() {
        let fx = Fixture::new();
        let credential = worker_key(&fx);
        jira_observer(&fx);
        for report in [
            json!({"outcome": {"kind": "exited", "exit_code": 0}, "stdout": "{\"state\": {}}"}),
            json!({"outcome": {"kind": "exited", "exit_code": 4}, "stderr": "boom"}),
        ] {
            fx.service.observer_pass();
            let run = fx.service.claim_observer_run().unwrap().unwrap();
            let result = fx
                .send(
                    "POST",
                    &format!("/observer/v1/worker/runs/{}/result", run.run_id),
                    &credential,
                    report,
                )
                .await;
            assert_eq!(result.status(), StatusCode::OK);
            *fx.clock.0.lock().unwrap() += Duration::seconds(3600);
        }

        let detail = fx
            .send(
                "GET",
                "/control/v1/observers/jira-all",
                OWNER_TOKEN,
                Value::Null,
            )
            .await;

        assert_eq!(detail.status(), StatusCode::OK);
        let body = json_body(detail).await;
        let errors = body["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 1, "{body}");
        assert_eq!(errors[0]["sequence"], 2);
        assert_eq!(errors[0]["status"], "failed");
        assert_eq!(errors[0]["exit_code"], 4);
        assert_eq!(errors[0]["stderr"], "boom");
        assert!(body.get("runs").is_none());
        assert_eq!(body["observer"]["last_run"]["status"], "failed");
    }

    #[tokio::test]
    async fn each_credential_opens_only_its_own_routes() {
        let fx = Fixture::new();
        let credential = worker_key(&fx);
        let run = fx.effect_run();

        for (method, uri) in [
            ("GET", "/control/v1/tasks"),
            ("GET", "/control/v1/observers"),
            ("POST", "/control/v1/observers"),
            ("POST", "/control/v1/observers/jira-all/approve"),
        ] {
            let refused = fx.send(method, uri, &credential, json!({})).await;
            assert_eq!(refused.status(), StatusCode::UNAUTHORIZED, "{uri}");
        }
        let refused = fx
            .send("POST", &tasks_uri(&run), &credential, effect())
            .await;
        assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);

        let result_uri = format!("/observer/v1/worker/runs/{}/result", run.run_id);
        for token in [run.token.expose(), OWNER_TOKEN, "nsw.server.forged"] {
            let claim = fx
                .send("POST", "/observer/v1/worker/claim", token, Value::Null)
                .await;
            assert_eq!(claim.status(), StatusCode::UNAUTHORIZED);
            assert!(json_body_is_empty(claim).await);
            let result = fx
                .send(
                    "POST",
                    &result_uri,
                    token,
                    json!({"outcome": {"kind": "timed_out"}}),
                )
                .await;
            assert_eq!(result.status(), StatusCode::UNAUTHORIZED);
        }
    }

    #[tokio::test]
    async fn a_key_of_an_earlier_start_of_the_worker_is_refused() {
        let fx = Fixture::new();
        let earlier = worker_key(&fx);
        let current = worker_key(&fx);

        let refused = fx
            .send("POST", "/observer/v1/worker/claim", &earlier, Value::Null)
            .await;
        let accepted = fx
            .send("POST", "/observer/v1/worker/claim", &current, Value::Null)
            .await;

        assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
        assert!(json_body_is_empty(refused).await);
        assert_eq!(accepted.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_restarted_server_refuses_the_key_of_its_predecessor() {
        let fx = Fixture::new();
        let key = worker_key(&fx);
        // The same state root, a new process: the digest lived in memory.
        let restarted = Fixture {
            root: fx.root.join("unused"),
            clock: Arc::clone(&fx.clock),
            service: Arc::new(ProtocolService::new(
                ControlPlane::with_clock(StatePaths::new(&fx.root), fx.clock.clone()),
                Arc::new(NoLaunches),
            )),
        };

        let claim = restarted
            .send("POST", "/observer/v1/worker/claim", &key, Value::Null)
            .await;

        assert_eq!(claim.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_worker_claims_and_reports_the_runs_of_every_owner() {
        let fx = Fixture::new();
        let key = worker_key(&fx);
        let stranger = OwnerRef::new("someone-else");
        fx.observer_of(
            stranger.clone(),
            "theirs",
            ObserverEffect::CreateTask(CreateTaskEffect {
                workflow_id: "triage".into(),
                workflow_version: 1,
                workspaces: Default::default(),
                backend_id: None,
                profile: None,
                mapping: Default::default(),
            }),
            ObserverMode::Always,
        );
        fx.service.observer_pass();

        let claim = fx
            .send("POST", "/observer/v1/worker/claim", &key, Value::Null)
            .await;

        let run = json_body(claim).await["run"].clone();
        assert_eq!(run["owner"], stranger.0.as_str());
        let result = fx
            .send(
                "POST",
                &format!(
                    "/observer/v1/worker/runs/{}/result",
                    run["run_id"].as_str().unwrap()
                ),
                &key,
                json!({"outcome": {"kind": "exited", "exit_code": 0}, "stdout": "{\"state\": {}}"}),
            )
            .await;
        assert_eq!(result.status(), StatusCode::OK);
        assert_eq!(json_body(result).await["status"], "succeeded");
    }

    #[tokio::test]
    async fn the_worker_key_is_written_nowhere() {
        let fx = Fixture::new();
        jira_observer(&fx);
        let key = worker_key(&fx);
        fx.service.observer_pass();
        let claim = fx
            .send("POST", "/observer/v1/worker/claim", &key, Value::Null)
            .await;
        assert_eq!(claim.status(), StatusCode::OK);

        let secret = key.rsplit_once('.').unwrap().1;
        let mut files = vec![fx.root.clone()];
        while let Some(path) = files.pop() {
            if path.is_dir() {
                files.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
            } else {
                let stored = std::fs::read(&path).unwrap();
                let stored = String::from_utf8_lossy(&stored);
                assert!(!stored.contains(secret), "{}", path.display());
            }
        }
    }

    // -- the owner's observer routes ----------------------------------------

    /// The body of an observer named `name` that answers the gates of the
    /// `mr-review` queue.
    fn queue_observer_body(name: &str) -> Value {
        json!({
            "name": name,
            "queue": "mr-review",
            "cron": "*/5 * * * *",
        })
    }

    #[tokio::test]
    async fn the_owner_creates_scripts_approves_and_deletes_an_observer() {
        let fx = Fixture::new();

        let created = fx
            .send(
                "POST",
                "/control/v1/observers",
                OWNER_TOKEN,
                queue_observer_body("mr-review"),
            )
            .await;
        assert_eq!(created.status(), StatusCode::CREATED);
        let created = json_body(created).await;
        assert_eq!(created["state"], "draft");
        assert_eq!(created["effect"], "none");
        assert_eq!(created["queue"], "mr-review");
        assert_eq!(created["mode"], "on_episodes");
        assert_eq!(created["cron"], "*/5 * * * *");
        assert!(created.get("interval_secs").is_none());

        let script = fx
            .send(
                "POST",
                "/control/v1/observers/mr-review/scripts",
                OWNER_TOKEN,
                json!({"script": "echo '{\"state\": {}}'\n"}),
            )
            .await;
        assert_eq!(script.status(), StatusCode::CREATED);
        assert_eq!(json_body(script).await["version"], 1);
        let approved = fx
            .send(
                "POST",
                "/control/v1/observers/mr-review/approve",
                OWNER_TOKEN,
                json!({"version": 1}),
            )
            .await;
        assert_eq!(approved.status(), StatusCode::OK);
        assert_eq!(json_body(approved).await["state"], "active");

        // Covers AE6: a new version is a draft; version 1 still runs.
        fx.send(
            "POST",
            "/control/v1/observers/mr-review/scripts",
            OWNER_TOKEN,
            json!({"script": "echo '{\"state\": {\"v\": 2}}'\n"}),
        )
        .await;
        let id = created["observer_id"].as_str().unwrap();
        let shown = fx
            .send(
                "GET",
                &format!("/control/v1/observers/{id}"),
                OWNER_TOKEN,
                Value::Null,
            )
            .await;
        assert_eq!(shown.status(), StatusCode::OK);
        let shown = json_body(shown).await;
        assert_eq!(shown["observer"]["approved_version"], 1);
        assert_eq!(shown["observer"]["latest_version"], 2);
        assert_eq!(shown["scripts"].as_array().unwrap().len(), 2);

        let paused = fx
            .send(
                "POST",
                "/control/v1/observers/mr-review/pause",
                OWNER_TOKEN,
                Value::Null,
            )
            .await;
        assert_eq!(json_body(paused).await["state"], "paused");
        let resumed = fx
            .send(
                "POST",
                "/control/v1/observers/mr-review/resume",
                OWNER_TOKEN,
                Value::Null,
            )
            .await;
        assert_eq!(json_body(resumed).await["state"], "active");

        let listed = fx
            .send("GET", "/control/v1/observers", OWNER_TOKEN, Value::Null)
            .await;
        assert_eq!(json_body(listed).await["observers"][0]["name"], "mr-review");

        let deleted = fx
            .send(
                "DELETE",
                "/control/v1/observers/mr-review",
                OWNER_TOKEN,
                Value::Null,
            )
            .await;
        assert_eq!(deleted.status(), StatusCode::OK);
        let gone = fx
            .send(
                "GET",
                "/control/v1/observers/mr-review",
                OWNER_TOKEN,
                Value::Null,
            )
            .await;
        assert_eq!(gone.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_create_task_observer_takes_the_catalogue_version_and_refuses_an_unknown_workflow() {
        let fx = Fixture::new();
        let body = |workflow: &str| {
            json!({
                "name": "jira-new",
                "interval_secs": 300,
                "effect": {"kind": "create_task", "workflow_id": workflow, "mapping": {"ticket": "key"}},
            })
        };

        let unknown = fx
            .send("POST", "/control/v1/observers", OWNER_TOKEN, body("nope"))
            .await;
        assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json_body(unknown).await["code"], "unknown_workflow");

        let created = fx
            .send("POST", "/control/v1/observers", OWNER_TOKEN, body("triage"))
            .await;
        assert_eq!(created.status(), StatusCode::CREATED);
        assert_eq!(json_body(created).await["workflow_id"], "triage");
        let record = fx
            .service
            .observers()
            .find_by_name("jira-new")
            .unwrap()
            .unwrap();
        let ObserverEffect::CreateTask(effect) = record.config.effect else {
            panic!("a create-task observer");
        };
        assert_eq!(effect.workflow_version, 1);

        let taken = fx
            .send("POST", "/control/v1/observers", OWNER_TOKEN, body("triage"))
            .await;
        assert_eq!(taken.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn a_create_request_refuses_a_bad_schedule_and_a_mode_mixed_into_a_queue() {
        // Covers AE8, AE9.
        let fx = Fixture::new();
        let create = |body: Value| fx.send("POST", "/control/v1/observers", OWNER_TOKEN, body);

        for (field, value) in [
            ("cron", json!("*/30 * * * * *")),
            ("cron", json!("@daily")),
            ("interval_secs", json!(10)),
        ] {
            let mut body = queue_observer_body("gitlab-mr");
            body.as_object_mut().unwrap().remove("cron");
            body[field] = value;
            let refused = create(body).await;
            assert_eq!(refused.status(), StatusCode::BAD_REQUEST, "{field}");
            assert_eq!(json_body(refused).await["code"], "invalid_observer");
        }
        let mut both = queue_observer_body("gitlab-mr");
        both["interval_secs"] = json!(300);
        let both = create(both).await;
        assert_eq!(json_body(both).await["code"], "invalid_observer");
        let mut neither = queue_observer_body("gitlab-mr");
        neither.as_object_mut().unwrap().remove("cron");
        let neither = create(neither).await;
        assert_eq!(json_body(neither).await["code"], "invalid_observer");
        let removed = create(json!({
            "name": "old", "interval_secs": 300,
            "watch_list": true, "allowed_object_prefixes": ["https://x/"],
        }))
        .await;
        assert_eq!(json_body(removed).await["code"], "invalid_request");
        // An older client still sends the removed `source` label.
        let mut with_source = queue_observer_body("gitlab-mr");
        with_source["source"] = json!("gitlab");
        let with_source = create(with_source).await;
        assert_eq!(with_source.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json_body(with_source).await["code"], "invalid_request");

        let created = create(queue_observer_body("gitlab-mr")).await;
        assert_eq!(created.status(), StatusCode::CREATED);
        assert!(json_body(created).await.get("source").is_none());
        let mut always = queue_observer_body("jira-intake");
        always["mode"] = json!("always");
        let mixed = create(always).await;
        assert_eq!(mixed.status(), StatusCode::CONFLICT);
        let mixed = json_body(mixed).await;
        assert_eq!(mixed["code"], "queue_mode_mismatch");
        assert!(
            mixed["message"].as_str().unwrap().contains("on_episodes"),
            "{mixed}"
        );
    }

    #[tokio::test]
    async fn the_owner_updates_an_observer_and_reads_its_queues() {
        // R24, R35: the update route and the two queue routes.
        let fx = Fixture::new();
        fx.observer("gitlab-mr", ObserverEffect::None, ObserverMode::OnEpisodes);
        fx.observer("jira-new", ObserverEffect::None, ObserverMode::Always);

        let moved = fx
            .send(
                "PATCH",
                "/control/v1/observers/gitlab-mr",
                OWNER_TOKEN,
                json!({"queue": "mr-review", "cron": "*/5 * * * *"}),
            )
            .await;
        assert_eq!(moved.status(), StatusCode::OK);
        let moved = json_body(moved).await;
        assert_eq!(moved["queue"], "mr-review");
        assert_eq!(moved["cron"], "*/5 * * * *");
        assert!(moved.get("interval_secs").is_none());
        assert_eq!(moved["state"], "active");

        // Covers AE8: the refusal names the queue's mode.
        let mixed = fx
            .send(
                "PATCH",
                "/control/v1/observers/jira-new",
                OWNER_TOKEN,
                json!({"queue": "mr-review"}),
            )
            .await;
        assert_eq!(mixed.status(), StatusCode::CONFLICT);
        let mixed = json_body(mixed).await;
        assert_eq!(mixed["code"], "queue_mode_mismatch");
        assert!(
            mixed["message"].as_str().unwrap().contains("on_episodes"),
            "{mixed}"
        );
        // Covers AE9.
        let seconds = fx
            .send(
                "PATCH",
                "/control/v1/observers/jira-new",
                OWNER_TOKEN,
                json!({"cron": "*/30 * * * * *"}),
            )
            .await;
        assert_eq!(seconds.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json_body(seconds).await["code"], "invalid_observer");
        for body in [json!({"name": "renamed"}), json!({})] {
            let refused = fx
                .send("PATCH", "/control/v1/observers/jira-new", OWNER_TOKEN, body)
                .await;
            assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
            assert_eq!(json_body(refused).await["code"], "invalid_request");
        }

        let listed = fx
            .send(
                "GET",
                "/control/v1/observer-queues",
                OWNER_TOKEN,
                Value::Null,
            )
            .await;
        assert_eq!(listed.status(), StatusCode::OK);
        assert_eq!(
            json_body(listed).await,
            json!({"queues": [
                {"queue": "jira-new", "mode": "always", "observers": 1, "entries": 0},
                {"queue": "mr-review", "mode": "on_episodes", "observers": 1, "entries": 0},
            ]})
        );
        let shown = fx
            .send(
                "GET",
                "/control/v1/observer-queues/mr-review",
                OWNER_TOKEN,
                Value::Null,
            )
            .await;
        assert_eq!(shown.status(), StatusCode::OK);
        let shown = json_body(shown).await;
        assert_eq!(shown["mode"], "on_episodes");
        assert_eq!(shown["observers"][0]["name"], "gitlab-mr");
        assert_eq!(shown["entries"], json!([]));
        let unknown = fx
            .send(
                "GET",
                "/control/v1/observer-queues/gitlab-mr",
                OWNER_TOKEN,
                Value::Null,
            )
            .await;
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
        assert!(json_body_is_empty(unknown).await);
    }

    /// A router whose gate let `owner` through, as the owner (`operator`)
    /// or not: what the network gate hands on for a signed-in user.
    fn router_for(fx: &Fixture, owner: &str, operator: bool) -> Router {
        let owner = OwnerRef::new(owner);
        owner_routes(Arc::clone(&fx.service)).layer(middleware::from_fn(
            move |mut request: Request, next: middleware::Next| {
                let owner = owner.clone();
                async move {
                    request.extensions_mut().insert(owner);
                    if operator {
                        request.extensions_mut().insert(Operator);
                    }
                    next.run(request).await
                }
            },
        ))
    }

    async fn call(router: Router, method: &str, uri: &str, body: Value) -> Response {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        router.oneshot(request).await.unwrap()
    }

    #[tokio::test]
    async fn only_the_owner_manages_observers() {
        // R21: a user who is not the installation's owner is refused with
        // a bare 403; another owner's observer is not found.
        let fx = Fixture::new();
        fx.observer("mr-review", ObserverEffect::None, ObserverMode::OnEpisodes);

        let stranger = router_for(&fx, "someone-else", false);
        let create = call(
            stranger.clone(),
            "POST",
            "/control/v1/observers",
            queue_observer_body("mine"),
        )
        .await;
        assert_eq!(create.status(), StatusCode::FORBIDDEN);
        assert!(json_body_is_empty(create).await);
        let list = call(
            stranger.clone(),
            "GET",
            "/control/v1/observers",
            Value::Null,
        )
        .await;
        assert_eq!(list.status(), StatusCode::FORBIDDEN);
        for (method, uri, body) in [
            (
                "PATCH",
                "/control/v1/observers/mr-review",
                json!({"cron": "*/5 * * * *"}),
            ),
            ("GET", "/control/v1/observer-queues", Value::Null),
            ("GET", "/control/v1/observer-queues/mr-review", Value::Null),
        ] {
            let refused = call(stranger.clone(), method, uri, body).await;
            assert_eq!(refused.status(), StatusCode::FORBIDDEN, "{method} {uri}");
        }

        let other_owner = router_for(&fx, "someone-else", true);
        let listed = call(
            other_owner.clone(),
            "GET",
            "/control/v1/observers",
            Value::Null,
        )
        .await;
        assert!(json_body(listed).await["observers"]
            .as_array()
            .unwrap()
            .is_empty());
        let queues = call(
            other_owner.clone(),
            "GET",
            "/control/v1/observer-queues",
            Value::Null,
        )
        .await;
        assert_eq!(json_body(queues).await, json!({"queues": []}));
        for (method, uri, body) in [
            ("GET", "/control/v1/observers/mr-review", Value::Null),
            (
                "PATCH",
                "/control/v1/observers/mr-review",
                json!({"cron": "*/5 * * * *"}),
            ),
            ("GET", "/control/v1/observer-queues/mr-review", Value::Null),
            (
                "POST",
                "/control/v1/observers/mr-review/approve",
                json!({"version": 1}),
            ),
            ("POST", "/control/v1/observers/mr-review/pause", Value::Null),
            ("DELETE", "/control/v1/observers/mr-review", Value::Null),
        ] {
            let refused = call(other_owner.clone(), method, uri, body).await;
            assert_eq!(refused.status(), StatusCode::NOT_FOUND, "{method} {uri}");
        }
        assert!(fx
            .service
            .observers()
            .find_by_name("mr-review")
            .unwrap()
            .is_some());
    }
}
