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

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::routing::post;
use axum::{Extension, Json, Router};
use serde::de::DeserializeOwned;

use super::http::{blocking, HttpProtocolError, ProtocolResult};
use crate::auth_gate::{require_observer_run, Principal};
use narsor_engine::controller_protocol::error::ProtocolError;
use narsor_engine::controller_protocol::service::{ProtocolService, MAX_EFFECT_DATA_BYTES};
use narsor_engine::controller_protocol::wire::{
    ObserverCreateTaskRequest, ObserverEffectOutcome, ObserverEffectResponse, ObserverEventRequest,
};
use narsor_engine::observers::RunId;

type Service = Arc<ProtocolService>;

/// Largest effect request body: the data's own limit plus room for the key
/// and the type. Anything larger is refused before it is read.
const MAX_EFFECT_BODY: usize = MAX_EFFECT_DATA_BYTES + 16 * 1024;

/// The effect routes, behind the observer run gate.
pub fn observer_routes<S>(service: Service) -> Router<S>
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
/// script may not set, with a coded `400` naming what is wrong.
fn parse_body<T: DeserializeOwned>(body: &Bytes) -> ProtocolResult<T> {
    serde_json::from_slice(body).map_err(|error| {
        HttpProtocolError(ProtocolError::bad_request(
            "invalid_request",
            format!("invalid effect request: {error}"),
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
    let request: ObserverCreateTaskRequest = parse_body(&body)?;
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
    let request: ObserverEventRequest = parse_body(&body)?;
    Ok(Json(
        blocking(&service, move |s| {
            s.observer_deliver_event(&run_id, &task_id, request)
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
            let workflow: WorkflowDefinition = serde_json::from_value(json!({
                "id": "triage",
                "name": "triage",
                "version": 1,
                "inputs": [{"name": "ticket", "required": true}],
                "states": {},
                "initial_state": "start",
            }))
            .unwrap();
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
}
