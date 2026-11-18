//! The automations' HTTP routes outside the owner gate, and the owner's
//! read-only queue views.
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
//! the queue entry the run was started for, and no other (KTD-6), and only
//! from a `check` run. They are merged outside the
//! owner gate, which never accepts a run token; the run gate in turn
//! accepts nothing but the token of a run in progress before its deadline,
//! and only for that run's own path. Refusals of the gate are a bare `401`;
//! a refused effect is a coded `400` or `409` the script can print.
//!
//! The worker routes (U6), behind the observer worker gate
//! ([`crate::auth_gate::require_observer_worker`]). The worker is the
//! server's own, started with a key the server makes at each of its starts
//! (D5); it claims the runs of every (queue, automation) pair:
//!
//! | route | body | answer |
//! | --- | --- | --- |
//! | `POST /observer/v1/worker/claim` | none | `200` [`ObserverClaimResponse`] |
//! | `POST /observer/v1/worker/runs/{run}/result` | [`ObserverRunReport`] | `200` [`ObserverRunResultResponse`], `404` no such run, `409` ended |
//!
//! The owner's read-only queue views (R21), behind the owner gate
//! ([`observer_owner_routes`]). Queues and automations are catalogue files:
//! no route creates, changes or deletes them, and the routes of the stored
//! observers of older versions (`/control/v1/observers…`) are gone (a bare
//! `404`, R22):
//!
//! | route | body | answer |
//! | --- | --- | --- |
//! | `GET /control/v1/observer-queues` | none | [`ObserverQueueListResponse`] |
//! | `GET /control/v1/observer-queues/{queue}` | none | [`ObserverQueueView`] |
//!
//! A queue view shows the caller's entries only.
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
    ObserverClaimResponse, ObserverCreateTaskRequest, ObserverDecideRequest,
    ObserverEffectResponse, ObserverQueueListResponse, ObserverQueueView, ObserverRunReport,
    ObserverRunResultResponse,
};
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
        // Only the matched routes: a path no route serves (a removed
        // observer management route, say) stays a plain `404`.
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&service),
            require_observer_worker,
        ))
        .with_state(service)
}

/// The owner's read-only queue routes; they expect the owner gate's
/// [`OwnerRef`] and [`Operator`] in the request extensions.
pub fn observer_owner_routes<S>(service: Service) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/control/v1/observer-queues", get(list_queues))
        .route("/control/v1/observer-queues/{queue}", get(get_queue))
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
        // Only the matched routes: a path no route serves (a removed
        // observer management route, say) stays a plain `404`.
        .route_layer(middleware::from_fn_with_state(
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
// The owner's queue views
// ---------------------------------------------------------------------------

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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use axum::body::Body;
    use axum::extract::Request;
    use axum::http::header;
    use axum::response::Response;
    use chrono::{DateTime, Duration, TimeZone, Utc};
    use narsor_engine::automations::{
        AutomationCatalog, AutomationCatalogError, AutomationCatalogLoad, AutomationScript,
        HookPhase,
    };
    use narsor_engine::controller_protocol::{ClaimedObserverRun, RunOutcome, RunReport};
    use narsor_engine::domain::{BackendId, WorkflowDefinition};
    use narsor_engine::environment::{
        BackendRecord, LaunchError, LaunchIntent, LaunchResolver, ResolvedLaunch,
        SlotReleaseTimings,
    };
    use narsor_engine::observers::AutomationKey;
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

    /// Launches nothing, and serves the automation catalogue under
    /// `catalog`: these routes are tested on their gates and refusals.
    struct NoLaunches {
        catalog: PathBuf,
    }

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

        fn automation_catalog(&self) -> AutomationCatalogLoad {
            AutomationCatalog::new(&self.catalog).load(|workflow| {
                self.workflow_definition(workflow)
                    .map_err(|error| error.to_string())
            })
        }

        fn automation_script(
            &self,
            automation: &str,
            phase: HookPhase,
        ) -> Result<AutomationScript, AutomationCatalogError> {
            AutomationCatalog::new(&self.catalog).script(automation, phase)
        }
    }

    /// The `triage` workflow the `jira-intake` queue creates tasks of.
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

    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("narsor-observer-http-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let clock = Arc::new(ManualClock(Mutex::new(
                Utc.with_ymd_and_hms(2026, 11, 13, 12, 0, 0).unwrap(),
            )));
            let service = Self::service(&root, &clock);
            Self {
                root,
                clock,
                service,
            }
        }

        fn service(root: &std::path::Path, clock: &Arc<ManualClock>) -> Service {
            Arc::new(ProtocolService::new(
                ControlPlane::with_clock(StatePaths::new(root.join("state")), clock.clone()),
                Arc::new(NoLaunches {
                    catalog: root.join("catalog"),
                }),
            ))
        }

        fn write(&self, relative: &str, text: &str) {
            let path = self.root.join("catalog").join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        /// The `always` queue `jira-intake` of the automation `jira-new`,
        /// which creates `triage` tasks: due once the pass runs.
        fn jira_queue(&self) {
            self.write(
                "automations/jira-new/automation.yaml",
                "description: new Jira issues\n",
            );
            self.write("automations/jira-new/check.sh", "echo '{\"state\": {}}'\n");
            self.write(
                "queues/jira-intake.yaml",
                "automations: [jira-new]\nmode: always\nschedule: {interval_secs: 300}\n\
                 tasks:\n  workflow_id: triage\n  mapping: {ticket: key}\n",
            );
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

        /// The claimed run of the `jira-intake` queue.
        fn effect_run(&self) -> ClaimedObserverRun {
            self.jira_queue();
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

    async fn json_body_is_empty(response: Response) -> bool {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .is_empty()
    }

    fn tasks_uri(run: &ClaimedObserverRun) -> String {
        format!("/observer/v1/runs/{}/tasks", run.run_id)
    }

    fn effect() -> Value {
        json!({"data": {"key": "ABC-1"}})
    }

    /// The worker key, as the server makes it when it starts the worker.
    fn worker_key(fx: &Fixture) -> String {
        fx.service.issue_observer_worker_key().expose().to_string()
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
            "/observer/v1/runs/{}~{}~{}/tasks",
            run.run_id.key.queue,
            run.run_id.key.automation,
            run.run_id.sequence + 1
        );
        let elsewhere = fx
            .send("POST", &other_run, run.token.expose(), effect())
            .await;
        assert_eq!(elsewhere.status(), StatusCode::UNAUTHORIZED);
        assert!(json_body_is_empty(elsewhere).await);
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
        let run = fx.effect_run();

        for field in ["workflow_id", "backend_id", "inputs"] {
            let mut body = effect();
            body[field] = json!("other");
            let refused = fx
                .send("POST", &tasks_uri(&run), run.token.expose(), body)
                .await;
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

    #[tokio::test]
    async fn a_worker_claims_a_catalogue_run_and_reports_its_result() {
        let fx = Fixture::new();
        let credential = worker_key(&fx);
        fx.jira_queue();
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
        assert_eq!(run_id, "jira-intake~jira-new~1");
        assert!(run["token"].as_str().unwrap().starts_with("nso."));
        assert_eq!(run["phase"], "check");
        assert_eq!(run["queue"], "jira-intake");
        assert_eq!(run["automation"], "jira-new");
        assert!(run["script_digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:"));
        assert_eq!(run["input"]["contract_version"], 3);
        assert_eq!(run["input"]["phase"], "check");
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
        let key = AutomationKey::new("jira-intake", "jira-new").unwrap();
        let record = fx.service.observers().load(&key).unwrap().unwrap();
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
        let credential = worker_key(&fx);
        let run = fx.effect_run();

        for (method, uri) in [
            ("GET", "/control/v1/tasks"),
            ("GET", "/control/v1/observer-queues"),
        ] {
            let refused = fx.send(method, uri, &credential, json!({})).await;
            assert_eq!(refused.status(), StatusCode::UNAUTHORIZED, "{uri}");
            let refused = fx.send(method, uri, run.token.expose(), json!({})).await;
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
    async fn a_restarted_server_refuses_the_key_of_its_predecessor() {
        let fx = Fixture::new();
        let earlier = worker_key(&fx);
        let current = worker_key(&fx);
        let refused = fx
            .send("POST", "/observer/v1/worker/claim", &earlier, Value::Null)
            .await;
        assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
        let accepted = fx
            .send("POST", "/observer/v1/worker/claim", &current, Value::Null)
            .await;
        assert_eq!(accepted.status(), StatusCode::OK);

        // The same state root, a new process: the digest lived in memory.
        let restarted = Fixture {
            root: fx.root.join("unused"),
            clock: Arc::clone(&fx.clock),
            service: Fixture::service(&fx.root, &fx.clock),
        };
        let claim = restarted
            .send("POST", "/observer/v1/worker/claim", &current, Value::Null)
            .await;
        assert_eq!(claim.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_worker_key_is_written_nowhere() {
        let fx = Fixture::new();
        fx.jira_queue();
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

    // -- the owner's queue views --------------------------------------------

    #[tokio::test]
    async fn the_queue_views_are_read_only_and_list_the_bad_files() {
        let fx = Fixture::new();
        fx.jira_queue();
        fx.write(
            "queues/broken.yaml",
            "automations: [missing]\nmode: on_episodes\nschedule: {interval_secs: 300}\n",
        );

        let list = fx
            .send(
                "GET",
                "/control/v1/observer-queues",
                OWNER_TOKEN,
                Value::Null,
            )
            .await;
        assert_eq!(list.status(), StatusCode::OK);
        let list = json_body(list).await;
        assert_eq!(list["queues"][0]["queue"], "broken");
        assert!(list["queues"][0]["problem"]
            .as_str()
            .unwrap()
            .contains("missing"));
        assert_eq!(list["queues"][1]["queue"], "jira-intake");
        assert_eq!(list["queues"][1]["automations"], json!(["jira-new"]));
        assert_eq!(list["problems"][0]["file"], "queues/broken.yaml");

        let shown = fx
            .send(
                "GET",
                "/control/v1/observer-queues/jira-intake",
                OWNER_TOKEN,
                Value::Null,
            )
            .await;
        assert_eq!(shown.status(), StatusCode::OK);
        let shown = json_body(shown).await;
        assert_eq!(shown["automations"][0]["scripts"], json!(["check"]));
        assert_eq!(shown["workflow_id"], "triage");

        // R22: the routes that managed stored observers are gone.
        for (method, uri) in [
            ("POST", "/control/v1/observers"),
            ("GET", "/control/v1/observers"),
            ("PATCH", "/control/v1/observers/jira-new"),
            ("POST", "/control/v1/observers/jira-new/approve"),
            ("POST", "/control/v1/observers/jira-new/pause"),
            ("DELETE", "/control/v1/observers/jira-new"),
        ] {
            let gone = fx
                .send(method, uri, OWNER_TOKEN, json!({"name": "jira-new"}))
                .await;
            assert_eq!(gone.status(), StatusCode::NOT_FOUND, "{method} {uri}");
        }
        let unknown = fx
            .send(
                "GET",
                "/control/v1/observer-queues/nothing",
                OWNER_TOKEN,
                Value::Null,
            )
            .await;
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
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

    #[tokio::test]
    async fn only_the_owner_reads_the_queues() {
        // R21: a user who is not the installation's owner is refused with
        // a bare 403.
        let fx = Fixture::new();
        fx.jira_queue();
        let stranger = router_for(&fx, "someone-else", false);
        for uri in [
            "/control/v1/observer-queues",
            "/control/v1/observer-queues/jira-intake",
        ] {
            let request = Request::get(uri).body(Body::empty()).unwrap();
            let refused = stranger.clone().oneshot(request).await.unwrap();
            assert_eq!(refused.status(), StatusCode::FORBIDDEN, "{uri}");
            assert!(json_body_is_empty(refused).await);
        }
    }
}
