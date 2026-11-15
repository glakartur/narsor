//! Tests of the `narsor-observer-worker` program (D20): the binary itself,
//! started as the server starts it, against a stand-in for the worker
//! routes of the control plane over real HTTP.
//!
//! They live in this crate so that `cargo test` builds the program and
//! gives its path in `CARGO_BIN_EXE_narsor-observer-worker`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use chrono::Utc;
use narsor_observer_worker::worker::MASK;
use narsor_protocol::controller::{
    ObserverRunOutcome, ObserverRunReport, ObserverRunResultResponse, ObserverWorkerRun,
    OBSERVER_WORKER_KEY_ENV,
};
use serde_json::json;

/// A temporary work directory for the program, removed when it goes away.
struct WorkDir(PathBuf);

impl WorkDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "narsor-obs-worker-program-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What the stand-in control plane saw from the program.
#[derive(Debug, Default)]
struct SeenByServer {
    claims: Vec<Option<String>>,
    reports: Vec<(String, ObserverRunReport)>,
}

/// A stand-in for the worker routes of the control plane over real HTTP:
/// the first claim gets `run`, every later claim nothing; each request's
/// `Authorization` header and each result is recorded.
async fn serve_worker_routes(
    run: ObserverWorkerRun,
) -> (
    String,
    Arc<Mutex<SeenByServer>>,
    tokio::task::JoinHandle<()>,
) {
    use axum::extract::{Path as RoutePath, State};
    use axum::http::HeaderMap;
    use axum::routing::post;
    use axum::{Json, Router};

    type Shared = (
        Arc<Mutex<SeenByServer>>,
        Arc<Mutex<Option<ObserverWorkerRun>>>,
    );

    fn bearer(headers: &HeaderMap) -> Option<String> {
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    }

    async fn claim(
        State((seen, due)): State<Shared>,
        headers: HeaderMap,
    ) -> Json<serde_json::Value> {
        seen.lock().unwrap().claims.push(bearer(&headers));
        let run = due.lock().unwrap().take();
        Json(json!({ "run": run }))
    }

    async fn result(
        State((seen, _)): State<Shared>,
        RoutePath(run): RoutePath<String>,
        Json(report): Json<ObserverRunReport>,
    ) -> Json<ObserverRunResultResponse> {
        seen.lock().unwrap().reports.push((run.clone(), report));
        Json(ObserverRunResultResponse {
            run_id: run,
            status: "succeeded".to_string(),
            error: None,
        })
    }

    let seen = Arc::new(Mutex::new(SeenByServer::default()));
    let shared: Shared = (Arc::clone(&seen), Arc::new(Mutex::new(Some(run))));
    let app = Router::new()
        .route("/observer/v1/worker/claim", post(claim))
        .route("/observer/v1/worker/runs/{run}/result", post(result))
        .with_state(shared);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, seen, server)
}

fn program_run(script: &str) -> ObserverWorkerRun {
    ObserverWorkerRun {
        run_id: "obs-1.1".to_string(),
        owner: "owner".to_string(),
        token: "nso.run.tok3n-SENTINEL".to_string(),
        script_version: 1,
        script: script.to_string(),
        input: json!({ "state": { "cursor": 4 } }),
        secrets: Vec::new(),
        timeout_secs: 30,
        deadline: Utc::now() + chrono::Duration::minutes(2),
    }
}

/// `narsor-observer-worker` started as the server starts it: only the key
/// in its environment, the rest as flags.
fn worker_program(server: &str, work: &Path, key: Option<&str>) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_narsor-observer-worker"));
    command
        .env_clear()
        .args(["--server", server, "--insecure-same-user", "--once"])
        .arg("--work-dir")
        .arg(work)
        .kill_on_drop(true);
    if let Some(key) = key {
        command.env(OBSERVER_WORKER_KEY_ENV, key);
    }
    command
}

#[tokio::test]
async fn the_program_claims_with_its_key_runs_the_script_and_reports_over_http() {
    let work = WorkDir::new();
    let script = "read -r input; printf '{\"state\":{\"seen\":true}}'; \
                  echo \"token=$NARSOR_TOKEN input=$input\" >&2";
    let (server, seen, handle) = serve_worker_routes(program_run(script)).await;

    let output = tokio::time::timeout(
        StdDuration::from_secs(60),
        worker_program(&server, &work.0, Some("nsw.server.k3y")).output(),
    )
    .await
    .expect("the program ends after one claim with --once")
    .unwrap();
    handle.abort();

    assert!(
        output.status.success(),
        "{:?}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let seen = seen.lock().unwrap();
    assert!(!seen.claims.is_empty());
    for header in &seen.claims {
        assert_eq!(header.as_deref(), Some("Bearer nsw.server.k3y"));
    }
    let [(run, report)] = seen.reports.as_slice() else {
        panic!("one result expected: {:?}", seen.reports);
    };
    assert_eq!(run, "obs-1.1");
    assert_eq!(report.outcome, ObserverRunOutcome::Exited { exit_code: 0 });
    assert_eq!(report.stdout, "{\"state\":{\"seen\":true}}");
    // The run's token never leaves the worker; the input reached stdin.
    assert!(
        !report.stderr.contains("tok3n-SENTINEL"),
        "{}",
        report.stderr
    );
    assert!(report.stderr.contains(MASK), "{}", report.stderr);
    assert!(report.stderr.contains("cursor"), "{}", report.stderr);
}

#[tokio::test]
async fn the_program_without_its_key_is_a_usage_error_and_claims_nothing() {
    let work = WorkDir::new();
    let (server, seen, handle) = serve_worker_routes(program_run("exit 0")).await;

    let output = worker_program(&server, &work.0, None)
        .output()
        .await
        .unwrap();
    handle.abort();

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains(OBSERVER_WORKER_KEY_ENV));
    assert!(seen.lock().unwrap().claims.is_empty());
}
