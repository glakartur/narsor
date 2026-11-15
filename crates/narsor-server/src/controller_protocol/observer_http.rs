//! The observers' HTTP routes outside the owner gate, and the owner's routes
//! for the worker credential.
//!
//! The effect routes of an observer run's script (U3), behind the observer
//! run gate ([`crate::auth_gate::require_observer_run`]):
//!
//! | route | body | answer |
//! | --- | --- | --- |
//! | `POST /observer/v1/runs/{run}/tasks` | [`ObserverCreateTaskRequest`] | `201` created, `200` duplicate or dry run |
//! | `POST /observer/v1/runs/{run}/tasks/{task_id}/events` | [`ObserverEventRequest`] | `200` |
//!
//! Both answer an [`ObserverEffectResponse`]. They are merged outside the
//! owner gate, which never accepts a run token; the run gate in turn
//! accepts nothing but the token of a run in progress before its deadline,
//! and only for that run's own path. Refusals of the gate are a bare `401`;
//! a refused effect is a coded `400` or `409` the script can print.
//!
//! The worker routes (U6), behind the observer worker gate
//! ([`crate::auth_gate::require_observer_worker`]), for the observers of
//! the credential's owner only:
//!
//! | route | body | answer |
//! | --- | --- | --- |
//! | `POST /observer/v1/worker/claim` | none | `200` [`ObserverClaimResponse`] |
//! | `POST /observer/v1/worker/runs/{run}/result` | [`ObserverRunReport`] | `200` [`ObserverRunResultResponse`], `404` another owner's run, `409` ended |
//!
//! The owner's routes for the worker credential ([`observer_owner_routes`],
//! behind the owner gate):
//!
//! | route | body | answer |
//! | --- | --- | --- |
//! | `GET /control/v1/observer-workers` | none | [`ObserverWorkerListResponse`] |
//! | `POST /control/v1/observer-workers` | [`CreateObserverWorkerRequest`] | `201` [`ObserverWorkerCreated`] |
//! | `POST /control/v1/observer-workers/{worker_id}/revoke` | none | [`ObserverWorkerView`] |
//!
//! The owner's routes for the observers themselves (U7, R21), behind the
//! owner gate as well; `{observer}` is a name or an id, and another owner's
//! observer is a bare `404`:
//!
//! | route | body | answer |
//! | --- | --- | --- |
//! | `GET /control/v1/observers` | none | [`ObserverListResponse`] |
//! | `POST /control/v1/observers` | [`CreateObserverRequest`] | `201` [`ObserverView`] |
//! | `GET /control/v1/observers/{observer}` | none | [`ObserverDetailView`] |
//! | `DELETE /control/v1/observers/{observer}` | none | [`ObserverView`] |
//! | `POST /control/v1/observers/{observer}/scripts` | [`SetObserverScriptRequest`] | `201` [`ObserverScriptView`] |
//! | `POST /control/v1/observers/{observer}/dry-run` | [`ObserverDryRunRequest`] | `202` [`ObserverDryRunView`] |
//! | `POST /control/v1/observers/{observer}/approve` | [`ApproveObserverRequest`] | [`ObserverView`] |
//! | `POST /control/v1/observers/{observer}/pause` | none | [`ObserverView`] |
//! | `POST /control/v1/observers/{observer}/resume` | none | [`ObserverView`] |
//! | `GET /control/v1/observers/{observer}/watches` | none | [`ObserverWatchListResponse`] |
//! | `POST /control/v1/observers/{observer}/watches` | [`AddObserverWatchRequest`] | `201` new, `200` existing [`ObserverWatchView`] |
//! | `DELETE /control/v1/observers/{observer}/watches/{entry_id}` | none | [`ObserverWatchView`] |
//!
//! Like the worker credential routes, they need the owner gate's
//! [`OwnerRef`] and [`Operator`]: anyone else gets a bare `403`.
//!
//! Each credential opens its own routes only: a run token is refused on the
//! worker routes, a worker credential on the effect routes and the owner's.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use serde::de::DeserializeOwned;

use super::http::{blocking, HttpProtocolError, ProtocolResult};
use crate::auth_gate::{require_observer_run, require_observer_worker, Operator, Principal};
use narsor_engine::controller_protocol::error::ProtocolError;
use narsor_engine::controller_protocol::service::{
    ProtocolService, MAX_EFFECT_DATA_BYTES, MAX_STORED_LOG_BYTES,
};
use narsor_engine::controller_protocol::wire::{
    AddObserverWatchRequest, ApproveObserverRequest, CreateObserverRequest,
    CreateObserverWorkerRequest, ObserverClaimResponse, ObserverCreateTaskRequest,
    ObserverDetailView, ObserverDryRunRequest, ObserverDryRunView, ObserverEffectOutcome,
    ObserverEffectResponse, ObserverEventRequest, ObserverListResponse, ObserverRunReport,
    ObserverRunResultResponse, ObserverScriptView, ObserverView, ObserverWatchListResponse,
    ObserverWatchView, ObserverWorkerCreated, ObserverWorkerListResponse, ObserverWorkerView,
    SetObserverScriptRequest,
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

/// The owner's routes for the worker credential; they expect the owner
/// gate's [`OwnerRef`] and [`Operator`] in the request extensions.
pub fn observer_owner_routes<S>(service: Service) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route(
            "/control/v1/observer-workers",
            get(list_workers).post(post_worker),
        )
        .route(
            "/control/v1/observer-workers/{worker_id}/revoke",
            post(post_revoke_worker),
        )
        .route(
            "/control/v1/observers",
            get(list_observers).post(post_observer),
        )
        .route(
            "/control/v1/observers/{observer}",
            get(get_observer).delete(delete_observer),
        )
        .route(
            "/control/v1/observers/{observer}/scripts",
            post(post_script),
        )
        .route(
            "/control/v1/observers/{observer}/dry-run",
            post(post_dry_run),
        )
        .route(
            "/control/v1/observers/{observer}/approve",
            post(post_approve),
        )
        .route("/control/v1/observers/{observer}/pause", post(post_pause))
        .route("/control/v1/observers/{observer}/resume", post(post_resume))
        .route(
            "/control/v1/observers/{observer}/watches",
            get(list_watches).post(post_watch),
        )
        .route(
            "/control/v1/observers/{observer}/watches/{entry_id}",
            delete(delete_watch),
        )
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
        .route(
            "/observer/v1/runs/{run}/tasks/{task_id}/events",
            post(post_event),
        )
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
    let status = if response.outcome == ObserverEffectOutcome::Created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(response)))
}

async fn post_event(
    State(service): State<Service>,
    Extension(principal): Extension<Principal>,
    Path((run, task_id)): Path<(String, String)>,
    body: Bytes,
) -> ProtocolResult<Json<ObserverEffectResponse>> {
    let run_id = token_run(&principal, &run)?;
    let request: ObserverEventRequest = parse_json(&body, "effect request")?;
    Ok(Json(
        blocking(&service, move |s| {
            s.observer_deliver_event(&run_id, &task_id, request)
        })
        .await?,
    ))
}

/// The owner a worker credential acts for (a bare `401` for any other
/// principal, which the worker gate never lets through anyway).
fn worker_owner(principal: &Principal) -> ProtocolResult<OwnerRef> {
    match principal {
        Principal::ObserverWorker { owner, .. } => Ok(owner.clone()),
        _ => Err(HttpProtocolError(ProtocolError::Unauthorized)),
    }
}

async fn post_claim(
    State(service): State<Service>,
    Extension(principal): Extension<Principal>,
) -> ProtocolResult<Json<ObserverClaimResponse>> {
    let owner = worker_owner(&principal)?;
    let claimed = blocking(&service, move |s| s.claim_observer_run_for(&owner)).await?;
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
    let owner = worker_owner(&principal)?;
    let run_id = RunId::parse(&run).map_err(|_| HttpProtocolError(ProtocolError::NotFound))?;
    let report: ObserverRunReport = parse_json(&body, "run result")?;
    let run = blocking(&service, move |s| {
        s.finish_observer_run_for(&owner, &run_id, report)
    })
    .await?;
    let status = serde_json::to_value(run.status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default();
    Ok(Json(ObserverRunResultResponse {
        run_id: run.run_id().to_string(),
        status,
        error: run.error,
    }))
}

/// The owner and operator a worker credential route needs (a bare `403`
/// without them).
fn require_owner_operator(
    owner: Option<Extension<OwnerRef>>,
    operator: Option<Extension<Operator>>,
) -> ProtocolResult<OwnerRef> {
    match (owner, operator) {
        (Some(Extension(owner)), Some(_)) => Ok(owner),
        _ => Err(HttpProtocolError(ProtocolError::Forbidden)),
    }
}

async fn list_workers(
    State(service): State<Service>,
    owner: Option<Extension<OwnerRef>>,
    operator: Option<Extension<Operator>>,
) -> ProtocolResult<Json<ObserverWorkerListResponse>> {
    let owner = require_owner_operator(owner, operator)?;
    Ok(Json(
        blocking(&service, move |s| s.observer_workers(&owner)).await?,
    ))
}

async fn post_worker(
    State(service): State<Service>,
    owner: Option<Extension<OwnerRef>>,
    operator: Option<Extension<Operator>>,
    body: Bytes,
) -> ProtocolResult<(StatusCode, Json<ObserverWorkerCreated>)> {
    let owner = require_owner_operator(owner, operator)?;
    let request: CreateObserverWorkerRequest = if body.is_empty() {
        CreateObserverWorkerRequest::default()
    } else {
        parse_json(&body, "worker request")?
    };
    let created = blocking(&service, move |s| s.create_observer_worker(&owner, request)).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

async fn post_revoke_worker(
    State(service): State<Service>,
    owner: Option<Extension<OwnerRef>>,
    operator: Option<Extension<Operator>>,
    Path(worker_id): Path<String>,
) -> ProtocolResult<Json<ObserverWorkerView>> {
    let owner = require_owner_operator(owner, operator)?;
    Ok(Json(
        blocking(&service, move |s| {
            s.revoke_observer_worker(&owner, &worker_id)
        })
        .await?,
    ))
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

async fn post_dry_run(
    State(service): State<Service>,
    owner: Option<Extension<OwnerRef>>,
    operator: Option<Extension<Operator>>,
    Path(observer): Path<String>,
    body: Bytes,
) -> ProtocolResult<(StatusCode, Json<ObserverDryRunView>)> {
    let owner = require_owner_operator(owner, operator)?;
    let request: ObserverDryRunRequest = if body.is_empty() {
        ObserverDryRunRequest::default()
    } else {
        parse_json(&body, "observer request")?
    };
    let requested = blocking(&service, move |s| {
        s.request_observer_dry_run_for(&owner, &observer, request)
    })
    .await?;
    Ok((StatusCode::ACCEPTED, Json(requested)))
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

async fn list_watches(
    State(service): State<Service>,
    owner: Option<Extension<OwnerRef>>,
    operator: Option<Extension<Operator>>,
    Path(observer): Path<String>,
) -> ProtocolResult<Json<ObserverWatchListResponse>> {
    let owner = require_owner_operator(owner, operator)?;
    Ok(Json(
        blocking(&service, move |s| s.observer_watches(&owner, &observer)).await?,
    ))
}

async fn post_watch(
    State(service): State<Service>,
    owner: Option<Extension<OwnerRef>>,
    operator: Option<Extension<Operator>>,
    Path(observer): Path<String>,
    body: Bytes,
) -> ProtocolResult<(StatusCode, Json<ObserverWatchView>)> {
    let owner = require_owner_operator(owner, operator)?;
    let request: AddObserverWatchRequest = parse_json(&body, "observer request")?;
    let (entry, created) = blocking(&service, move |s| {
        s.add_observer_watch(&owner, &observer, request)
    })
    .await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(entry)))
}

async fn delete_watch(
    State(service): State<Service>,
    owner: Option<Extension<OwnerRef>>,
    operator: Option<Extension<Operator>>,
    Path((observer, entry_id)): Path<(String, String)>,
) -> ProtocolResult<Json<ObserverWatchView>> {
    let owner = require_owner_operator(owner, operator)?;
    Ok(Json(
        blocking(&service, move |s| {
            s.remove_observer_watch(&owner, &observer, &entry_id)
        })
        .await?,
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
    use narsor_engine::domain::{
        BackendId, GitIdentity, ResolvedEnvironment, ResourceProfile, TaskId, TaskLaunchSpec,
        TaskRepository, TaskRepositorySource, TaskWorkspace, WorkflowDefinition,
    };
    use narsor_engine::environment::{
        BackendRecord, LaunchError, LaunchIntent, LaunchResolver, ResolvedLaunch,
        SlotReleaseTimings,
    };
    use narsor_engine::observers::{
        CreateTaskEffect, ObserverConfig, ObserverEffect, ObserverId, ObserverName, WatchEntry,
        WatchMode, WatchObjectPolicy, WatchOrigin,
    };
    use narsor_engine::orchestration::{Clock, ControlPlane, NewTask, OwnerRef, TaskEventKind};
    use narsor_engine::persistence::StatePaths;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use super::*;
    use crate::auth_gate::{require_owner, OwnerAuth};
    use crate::controller_protocol::http::owner_routes;

    const OWNER_TOKEN: &str = "owner-token";
    const PREFIX: &str = "https://git.example.com/";

    #[derive(Debug)]
    struct ManualClock(Mutex<DateTime<Utc>>);

    impl Clock for ManualClock {
        fn now(&self) -> DateTime<Utc> {
            *self.0.lock().unwrap()
        }
    }

    /// Launches nothing: these routes are tested on events and dry runs.
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
        fn observer(&self, name: &str, effect: ObserverEffect, mode: WatchMode) -> ObserverId {
            let mut config =
                ObserverConfig::new(ObserverName::parse(name).unwrap(), "gitlab", effect);
            config.watch_mode = mode;
            config.watch_objects = WatchObjectPolicy {
                allowed_prefixes: vec![PREFIX.into()],
                ..WatchObjectPolicy::default()
            };
            let workflow = triage();
            let store = self.service.observers();
            let id = store
                .create(owner(), config, Some(&workflow), self.now())
                .unwrap()
                .observer_id;
            store
                .add_script(&id, "echo '{\"state\": {}}'\n", self.now())
                .unwrap();
            self.service.approve_observer_version(&id, 1).unwrap();
            id
        }

        /// A watch-list observer's run and the task on its list.
        fn event_run(&self) -> (ClaimedObserverRun, TaskId) {
            let id = self.observer(
                "mr-review",
                ObserverEffect::DeliverEvent,
                WatchMode::WatchList,
            );
            let task = self.task("task-1");
            let entry = WatchEntry::new(
                task.clone(),
                None,
                format!("{PREFIX}a/-/merge_requests/17"),
                vec!["approved".into()],
                WatchOrigin::Operator,
                self.now(),
            );
            let lock = self.service.observers().lock(&id).unwrap();
            self.service
                .observers()
                .create_watch(&lock, &entry)
                .unwrap();
            drop(lock);
            self.service.observer_pass();
            (self.service.claim_observer_run().unwrap().unwrap(), task)
        }

        fn task(&self, id: &str) -> TaskId {
            let workspace = TaskWorkspace::single(TaskRepository::new(
                "narsor",
                TaskRepositorySource::Git(Box::new(GitIdentity {
                    repository: Some("narsor".to_string()),
                    remote_url: "https://example.com/narsor.git".to_string(),
                    commit: "614c3a8".repeat(5) + "abcde",
                    git_ref: None,
                    ref_origin: None,
                    credential: None,
                })),
            ));
            let spec = TaskLaunchSpec::new(
                "steps",
                1,
                json!({}),
                workspace,
                ResolvedEnvironment::harness_only("claude-code"),
                ResourceProfile::new(1000, 2048),
            );
            let task_id = TaskId::parse(id).unwrap();
            self.service
                .control_plane()
                .create_task(NewTask::new(
                    task_id.clone(),
                    owner(),
                    BackendId::parse("k8s").unwrap(),
                    spec,
                ))
                .unwrap();
            task_id
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

    fn events_uri(run: &ClaimedObserverRun, task: &TaskId) -> String {
        format!("/observer/v1/runs/{}/tasks/{task}/events", run.run_id)
    }

    fn event(key: &str) -> Value {
        json!({"effect_key": key, "event_type": "approved", "data": {"mr": 17}})
    }

    #[tokio::test]
    async fn an_event_goes_to_the_history_of_the_watched_task() {
        let fx = Fixture::new();
        let (run, task) = fx.event_run();

        let response = fx
            .send(
                "POST",
                &events_uri(&run, &task),
                run.token.expose(),
                event("MR!17:approved"),
            )
            .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["outcome"], "recorded");
        assert_eq!(body["effect_key"], "MR!17:approved");
        assert_eq!(body["task_id"], "task-1");
        let again = fx
            .send(
                "POST",
                &events_uri(&run, &task),
                run.token.expose(),
                event("MR!17:approved"),
            )
            .await;
        assert_eq!(json_body(again).await["outcome"], "duplicate");
        let events = fx.service.control_plane().events_since(&task, 0).unwrap();
        let external = events
            .iter()
            .filter(|event| matches!(event.kind, TaskEventKind::ExternalEventReceived { .. }))
            .count();
        assert_eq!(external, 1);
    }

    #[tokio::test]
    async fn the_gate_refuses_no_token_a_wrong_one_and_another_runs_path() {
        let fx = Fixture::new();
        let (run, task) = fx.event_run();
        let uri = events_uri(&run, &task);

        let request = Request::post(&uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(event("k").to_string()))
            .unwrap();
        let no_token = fx.router().oneshot(request).await.unwrap();
        assert_eq!(no_token.status(), StatusCode::UNAUTHORIZED);

        let forged = format!("{}x", run.token.expose());
        let wrong = fx.send("POST", &uri, &forged, event("k")).await;
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
        let owner = fx.send("POST", &uri, OWNER_TOKEN, event("k")).await;
        assert_eq!(owner.status(), StatusCode::UNAUTHORIZED);

        let other_run = format!(
            "/observer/v1/runs/{}.{}/tasks/{task}/events",
            run.run_id.observer_id,
            run.run_id.sequence + 1
        );
        let elsewhere = fx
            .send("POST", &other_run, run.token.expose(), event("k"))
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
    async fn a_run_token_opens_no_owner_route() {
        let fx = Fixture::new();
        let (run, _task) = fx.event_run();

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
        let (run, task) = fx.event_run();
        fx.service
            .finish_observer_run(
                &run.run_id,
                RunReport {
                    outcome: RunOutcome::Exited { exit_code: 0 },
                    stdout: r#"{"state": {}}"#.into(),
                    stderr: String::new(),
                },
            )
            .unwrap();
        let ended = fx
            .send(
                "POST",
                &events_uri(&run, &task),
                run.token.expose(),
                event("k"),
            )
            .await;
        assert_eq!(ended.status(), StatusCode::UNAUTHORIZED);

        // The next run, left past its deadline without a result.
        *fx.clock.0.lock().unwrap() += Duration::seconds(300);
        fx.service.observer_pass();
        let next = fx.service.claim_observer_run().unwrap().unwrap();
        *fx.clock.0.lock().unwrap() +=
            Duration::seconds(i64::try_from(next.timeout_secs).unwrap() + 3600);
        let late = fx
            .send(
                "POST",
                &events_uri(&next, &task),
                next.token.expose(),
                event("k"),
            )
            .await;
        assert_eq!(late.status(), StatusCode::UNAUTHORIZED);
        let external = fx
            .service
            .control_plane()
            .events_since(&task, 0)
            .unwrap()
            .iter()
            .filter(|event| matches!(event.kind, TaskEventKind::ExternalEventReceived { .. }))
            .count();
        assert_eq!(external, 0);
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
            WatchMode::All,
        );
        fx.service
            .request_observer_dry_run(&id, Some(1), false)
            .unwrap();
        let run = fx.service.claim_observer_run().unwrap().unwrap();
        let uri = format!("/observer/v1/runs/{}/tasks", run.run_id);

        for field in ["workflow_id", "backend_id", "inputs"] {
            let mut body = json!({"effect_key": "ABC-1", "data": {"key": "ABC-1"}});
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

        let would = fx
            .send(
                "POST",
                &uri,
                run.token.expose(),
                json!({"effect_key": "ABC-1", "data": {"key": "ABC-1"}}),
            )
            .await;
        assert_eq!(would.status(), StatusCode::OK);
        assert_eq!(json_body(would).await["outcome"], "would_create");
        assert!(fx.service.control_plane().list_tasks().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_oversized_effect_body_is_refused_before_it_is_read() {
        let fx = Fixture::new();
        let (run, task) = fx.event_run();
        let body = json!({
            "effect_key": "k",
            "event_type": "approved",
            "data": {"text": "x".repeat(MAX_EFFECT_BODY)},
        });

        let refused = fx
            .send("POST", &events_uri(&run, &task), run.token.expose(), body)
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
            WatchMode::All,
        )
    }

    /// A worker credential of the owner, created through the owner route.
    async fn worker_credential(fx: &Fixture) -> (String, String) {
        let created = fx
            .send(
                "POST",
                "/control/v1/observer-workers",
                OWNER_TOKEN,
                json!({"name": "host-1"}),
            )
            .await;
        assert_eq!(created.status(), StatusCode::CREATED);
        let body = json_body(created).await;
        assert_eq!(body["worker"]["name"], "host-1");
        (
            body["worker"]["worker_id"].as_str().unwrap().to_string(),
            body["credential"].as_str().unwrap().to_string(),
        )
    }

    #[tokio::test]
    async fn a_worker_claims_a_run_and_reports_its_result() {
        let fx = Fixture::new();
        let (_worker_id, credential) = worker_credential(&fx).await;
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
    async fn each_credential_opens_only_its_own_routes() {
        let fx = Fixture::new();
        let (_worker_id, credential) = worker_credential(&fx).await;
        let (run, task) = fx.event_run();

        for (method, uri) in [
            ("GET", "/control/v1/tasks"),
            ("GET", "/control/v1/observer-workers"),
            ("POST", "/control/v1/observer-workers"),
            ("GET", "/control/v1/observers"),
            ("POST", "/control/v1/observers"),
            ("POST", "/control/v1/observers/mr-review/approve"),
        ] {
            let refused = fx.send(method, uri, &credential, json!({})).await;
            assert_eq!(refused.status(), StatusCode::UNAUTHORIZED, "{uri}");
        }
        let effect = fx
            .send("POST", &events_uri(&run, &task), &credential, event("k"))
            .await;
        assert_eq!(effect.status(), StatusCode::UNAUTHORIZED);

        let result_uri = format!("/observer/v1/worker/runs/{}/result", run.run_id);
        for token in [run.token.expose(), OWNER_TOKEN, "nsw.wkr-1.forged"] {
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
    async fn a_revoked_worker_credential_is_refused() {
        let fx = Fixture::new();
        let (worker_id, credential) = worker_credential(&fx).await;
        let listed = fx
            .send(
                "GET",
                "/control/v1/observer-workers",
                OWNER_TOKEN,
                Value::Null,
            )
            .await;
        assert_eq!(
            json_body(listed).await["workers"][0]["worker_id"],
            worker_id.as_str()
        );

        let revoked = fx
            .send(
                "POST",
                &format!("/control/v1/observer-workers/{worker_id}/revoke"),
                OWNER_TOKEN,
                Value::Null,
            )
            .await;

        assert_eq!(revoked.status(), StatusCode::OK);
        assert!(json_body(revoked).await["revoked_at"].is_string());
        let claim = fx
            .send(
                "POST",
                "/observer/v1/worker/claim",
                &credential,
                Value::Null,
            )
            .await;
        assert_eq!(claim.status(), StatusCode::UNAUTHORIZED);
        let unknown = fx
            .send(
                "POST",
                "/control/v1/observer-workers/wkr-0/revoke",
                OWNER_TOKEN,
                Value::Null,
            )
            .await;
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_worker_of_another_owner_neither_claims_nor_reports_the_run() {
        let fx = Fixture::new();
        let stranger = fx
            .service
            .create_observer_worker(&OwnerRef::new("someone-else"), Default::default())
            .unwrap()
            .credential;
        jira_observer(&fx);
        fx.service.observer_pass();

        let claim = fx
            .send("POST", "/observer/v1/worker/claim", &stranger, Value::Null)
            .await;
        assert_eq!(json_body(claim).await["run"], Value::Null);

        let run = fx.service.claim_observer_run().unwrap().unwrap();
        let result = fx
            .send(
                "POST",
                &format!("/observer/v1/worker/runs/{}/result", run.run_id),
                &stranger,
                json!({"outcome": {"kind": "exited", "exit_code": 0}, "stdout": "{\"state\": {}}"}),
            )
            .await;
        assert_eq!(result.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            fx.service.observers().load_run(&run.run_id).unwrap().status,
            narsor_engine::observers::RunStatus::Running
        );
    }

    #[tokio::test]
    async fn the_credential_record_keeps_only_a_digest() {
        let fx = Fixture::new();
        let (worker_id, credential) = worker_credential(&fx).await;
        let path = StatePaths::new(&fx.root).observer_worker_file(&worker_id);

        let stored = std::fs::read_to_string(&path).unwrap();

        let secret = credential.rsplit_once('.').unwrap().1;
        assert!(!stored.contains(secret), "{stored}");
        assert!(stored.contains("sha256:"), "{stored}");
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // -- the owner's observer routes ----------------------------------------

    /// The body of a "deliver events" observer named `name`.
    fn event_observer_body(name: &str) -> Value {
        json!({
            "name": name,
            "source": "gitlab",
            "effect": {"kind": "deliver_event"},
            "watch_list": true,
            "allowed_object_prefixes": [PREFIX],
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
                event_observer_body("mr-review"),
            )
            .await;
        assert_eq!(created.status(), StatusCode::CREATED);
        let created = json_body(created).await;
        assert_eq!(created["state"], "draft");
        assert_eq!(created["effect"], "deliver_event");

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
                "source": "jira",
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
    async fn a_dry_run_request_is_pending_until_a_worker_claims_it() {
        let fx = Fixture::new();
        fx.observer(
            "mr-review",
            ObserverEffect::DeliverEvent,
            WatchMode::WatchList,
        );

        let requested = fx
            .send(
                "POST",
                "/control/v1/observers/mr-review/dry-run",
                OWNER_TOKEN,
                json!({"with_secrets": true}),
            )
            .await;

        assert_eq!(requested.status(), StatusCode::ACCEPTED);
        let requested = json_body(requested).await;
        assert_eq!(requested["version"], 1);
        assert_eq!(requested["with_secrets"], true);
        let shown = fx
            .send(
                "GET",
                "/control/v1/observers/mr-review",
                OWNER_TOKEN,
                Value::Null,
            )
            .await;
        let shown = json_body(shown).await;
        assert_eq!(shown["observer"]["pending_dry_run"]["version"], 1);
        let run = fx.service.claim_observer_run().unwrap().unwrap();
        assert!(run.dry_run);
    }

    #[tokio::test]
    async fn the_owner_adds_lists_and_removes_a_watch_entry() {
        let fx = Fixture::new();
        fx.observer(
            "mr-review",
            ObserverEffect::DeliverEvent,
            WatchMode::WatchList,
        );
        let task = fx.task("task-1");
        let body = |object: &str| json!({"task_id": task.to_string(), "object": object, "event_types": ["approved"]});
        let uri = "/control/v1/observers/mr-review/watches";
        let object = format!("{PREFIX}a/-/merge_requests/17");

        let added = fx.send("POST", uri, OWNER_TOKEN, body(&object)).await;
        assert_eq!(added.status(), StatusCode::CREATED);
        let entry = json_body(added).await;
        assert_eq!(entry["origin"], "operator");
        let again = fx.send("POST", uri, OWNER_TOKEN, body(&object)).await;
        assert_eq!(again.status(), StatusCode::OK);

        let foreign = fx
            .send("POST", uri, OWNER_TOKEN, body("https://evil.example.org/x"))
            .await;
        assert_eq!(foreign.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json_body(foreign).await["code"], "invalid_watch_entry");
        let unknown_task = fx
            .send(
                "POST",
                uri,
                OWNER_TOKEN,
                json!({"task_id": "task-9", "object": object, "event_types": ["approved"]}),
            )
            .await;
        assert_eq!(json_body(unknown_task).await["code"], "unknown_task");

        let listed = fx.send("GET", uri, OWNER_TOKEN, Value::Null).await;
        let listed = json_body(listed).await;
        assert_eq!(listed["watches"].as_array().unwrap().len(), 1);
        assert_eq!(listed["watches"][0]["object"], object.as_str());

        let entry_id = entry["entry_id"].as_str().unwrap();
        let removed = fx
            .send(
                "DELETE",
                &format!("{uri}/{entry_id}"),
                OWNER_TOKEN,
                Value::Null,
            )
            .await;
        assert_eq!(removed.status(), StatusCode::OK);
        let listed = fx.send("GET", uri, OWNER_TOKEN, Value::Null).await;
        assert!(json_body(listed).await["watches"]
            .as_array()
            .unwrap()
            .is_empty());
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
        fx.observer(
            "mr-review",
            ObserverEffect::DeliverEvent,
            WatchMode::WatchList,
        );

        let stranger = router_for(&fx, "someone-else", false);
        let create = call(
            stranger.clone(),
            "POST",
            "/control/v1/observers",
            event_observer_body("mine"),
        )
        .await;
        assert_eq!(create.status(), StatusCode::FORBIDDEN);
        assert!(json_body_is_empty(create).await);
        let list = call(stranger, "GET", "/control/v1/observers", Value::Null).await;
        assert_eq!(list.status(), StatusCode::FORBIDDEN);

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
        for (method, uri, body) in [
            ("GET", "/control/v1/observers/mr-review", Value::Null),
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
