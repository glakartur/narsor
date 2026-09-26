//! Integration tests for U4's two SSE endpoints (`GET
//! /executions/:id/stream` and `GET /events/stream`), over the same real
//! embedded server (`src/api/mod.rs`) and real HTTP that `tests/api.rs`
//! already exercises for the read/mutating endpoints.
//!
//! # Why the "slow client / Lagged" scenario is not here
//!
//! It is covered instead by a unit test next to the code it exercises
//! (`src/api/routes.rs`, `sse_stream_tests`). Reproducing a real `Lagged`
//! over an actual loopback connection would require overrunning the
//! engine's 1024-capacity broadcast buffer faster than the OS's own socket
//! buffers and axum's body-polling drain it — which depends on kernel
//! buffer sizing, not on anything this crate controls, and would make the
//! test flaky (or simply never lag, if the buffers happen to be generous).
//! The unit test drives the exact same `sse_event_stream` combinator both
//! handlers here call, over a deliberately tiny broadcast channel, which
//! exercises the `Err(Lagged(_))` branch deterministically instead.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use narsor::api;
use narsor::domain::{Event, Workspace};
use narsor::engine::Engine;
use narsor::persistence::ExecutionStore;
use narsor::workflow::load_from_file;

/// A temporary directory that deletes itself when it goes out of scope —
/// same stand-in `tests/api.rs` uses in place of the `tempfile` crate.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-sse-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    fn store(&self) -> ExecutionStore {
        ExecutionStore::new(&self.path)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Writes (and loads) the smallest workflow that validates: one script state
/// that always succeeds, straight into a terminal — mirrors
/// `tests/api.rs`'s `write_script_to_done_workflow`. Running it end to end
/// through [`Engine::run`] emits, in order: `ActionStarted`,
/// `ActionCompleted`, `StateExited`, `TransitionTaken`, `StateEntered`,
/// `WorkflowCompleted` — six events per run, enough to tell a real sequence
/// apart from an empty or truncated one.
fn write_script_to_done_workflow(
    root: &TempRoot,
    name: &str,
) -> narsor::domain::WorkflowDefinition {
    let script_path = root.join(&format!("{name}-succeed.sh"));
    fs::write(&script_path, "#!/bin/sh\nexit 0\n").expect("write script");
    let mut permissions = fs::metadata(&script_path)
        .expect("stat script")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script_path, permissions).expect("chmod script");

    let yaml = format!(
        r#"
name: sse-{name}-fixture
version: 1
description: Minimal script-to-terminal fixture for the SSE endpoint tests.

states:
  start:
    action:
      type: script
      command: "{command}"
    outputs:
      - success
    transitions:
      success: done

  done:
    description: Terminal.

terminal_states:
  done: completed
"#,
        command = script_path.display()
    );

    let path = root.join(&format!("sse-{name}-fixture.yaml"));
    fs::write(&path, yaml).expect("write definition");
    load_from_file(&path).expect("load definition")
}

async fn start_server_with_engine(
    engine: Arc<Engine>,
    catalog_dir: PathBuf,
    work_dir: PathBuf,
) -> api::ApiHandle {
    api::start(engine, catalog_dir, work_dir)
        .await
        .expect("server starts")
}

fn url(handle: &api::ApiHandle, path: &str) -> String {
    format!("http://{}{path}", handle.addr())
}

/// Reads whole SSE frames (`"...\n\n"`-terminated) off a streaming response,
/// parsing each `data:` line as an [`Event`], until either `expected` events
/// have been collected or `deadline` elapses — whichever comes first. An SSE
/// connection never closes on its own (`KeepAlive`), so a fixed deadline
/// rather than "read until EOF" is the only sound way to bound this.
async fn collect_events(
    mut response: reqwest::Response,
    expected: usize,
    deadline: Duration,
) -> Vec<Event> {
    let mut buf = String::new();
    let mut events = Vec::new();
    let started = tokio::time::Instant::now();

    while events.len() < expected {
        let remaining = deadline.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        let chunk = match tokio::time::timeout(remaining, response.chunk()).await {
            Ok(Ok(Some(chunk))) => chunk,
            _ => break,
        };
        buf.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(idx) = buf.find("\n\n") {
            let frame = buf[..idx].to_string();
            buf.drain(..=idx + 1);
            for line in frame.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    if let Ok(event) = serde_json::from_str::<Event>(data) {
                        events.push(event);
                    }
                }
            }
        }
    }

    events
}

// --- AE5: identical, real-time sequence to two clients ---------------------

#[tokio::test]
async fn two_clients_on_the_same_execution_see_an_identical_sequence() {
    let root = TempRoot::new();
    let store = root.store();
    let engine = Arc::new(Engine::new(store.clone()));

    let definition = write_script_to_done_workflow(&root, "two-clients");
    let workspace_path = root.join("workspace");
    fs::create_dir_all(&workspace_path).expect("create workspace dir");
    let execution = engine
        .start(
            &definition,
            serde_json::json!({}),
            Workspace::local_folder(workspace_path.display().to_string()),
        )
        .expect("start execution");

    let handle = start_server_with_engine(
        Arc::clone(&engine),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .await;
    let raw = reqwest::Client::new();
    let stream_url = url(&handle, &format!("/executions/{}/stream", execution.id));

    // Both clients connect (and so both subscribe on the engine's broadcast)
    // before the run happens, so every event the run emits reaches both.
    let response_a = raw
        .get(&stream_url)
        .bearer_auth(handle.token())
        .send()
        .await
        .expect("client A connects");
    assert_eq!(response_a.status(), reqwest::StatusCode::OK);
    let response_b = raw
        .get(&stream_url)
        .bearer_auth(handle.token())
        .send()
        .await
        .expect("client B connects");
    assert_eq!(response_b.status(), reqwest::StatusCode::OK);

    let reader_a = tokio::spawn(collect_events(response_a, 6, Duration::from_secs(5)));
    let reader_b = tokio::spawn(collect_events(response_b, 6, Duration::from_secs(5)));

    // Give both readers a moment to actually be polling before the run
    // starts producing events, then drive the execution to completion.
    tokio::time::sleep(Duration::from_millis(50)).await;
    engine
        .run(&definition, &execution.id)
        .await
        .expect("run to completion");

    let events_a = reader_a.await.expect("reader A task");
    let events_b = reader_b.await.expect("reader B task");

    assert!(
        events_a.len() >= 6,
        "client A should have seen every event the run emitted: {events_a:?}"
    );
    assert_eq!(
        events_a.len(),
        events_b.len(),
        "both clients on the same execution's stream must see the same number of events"
    );

    let sequences_a: Vec<u64> = events_a.iter().map(|event| event.sequence).collect();
    let sequences_b: Vec<u64> = events_b.iter().map(|event| event.sequence).collect();
    assert_eq!(
        sequences_a, sequences_b,
        "both clients must see an identical, ordered sequence of events"
    );
    assert!(events_a
        .iter()
        .all(|event| event.workflow_execution_id == execution.id));
}

// --- Global stream sees events from multiple executions --------------------

#[tokio::test]
async fn global_stream_sees_events_from_multiple_executions() {
    let root = TempRoot::new();
    let store = root.store();
    let engine = Arc::new(Engine::new(store.clone()));

    let definition_one = write_script_to_done_workflow(&root, "global-one");
    let definition_two = write_script_to_done_workflow(&root, "global-two");

    let workspace_one = root.join("workspace-one");
    let workspace_two = root.join("workspace-two");
    fs::create_dir_all(&workspace_one).expect("create workspace one");
    fs::create_dir_all(&workspace_two).expect("create workspace two");

    let execution_one = engine
        .start(
            &definition_one,
            serde_json::json!({}),
            Workspace::local_folder(workspace_one.display().to_string()),
        )
        .expect("start execution one");
    let execution_two = engine
        .start(
            &definition_two,
            serde_json::json!({}),
            Workspace::local_folder(workspace_two.display().to_string()),
        )
        .expect("start execution two");

    let handle = start_server_with_engine(
        Arc::clone(&engine),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .await;
    let raw = reqwest::Client::new();

    let response = raw
        .get(url(&handle, "/events/stream"))
        .bearer_auth(handle.token())
        .send()
        .await
        .expect("connect to the global stream");
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    // 6 events per run, from two independent executions.
    let reader = tokio::spawn(collect_events(response, 12, Duration::from_secs(5)));
    tokio::time::sleep(Duration::from_millis(50)).await;

    engine
        .run(&definition_one, &execution_one.id)
        .await
        .expect("run execution one to completion");
    engine
        .run(&definition_two, &execution_two.id)
        .await
        .expect("run execution two to completion");

    let events = reader.await.expect("reader task");

    let ids: std::collections::HashSet<&str> = events
        .iter()
        .map(|event| event.workflow_execution_id.as_str())
        .collect();
    assert!(
        ids.contains(execution_one.id.as_str()),
        "the global stream must carry execution one's events: {events:?}"
    );
    assert!(
        ids.contains(execution_two.id.as_str()),
        "the global stream must carry execution two's events: {events:?}"
    );
}

// --- A client disconnecting mid-stream never panics the server -------------

#[tokio::test]
async fn a_disconnecting_client_does_not_disrupt_the_server() {
    let root = TempRoot::new();
    let store = root.store();
    let engine = Arc::new(Engine::new(store.clone()));

    let definition = write_script_to_done_workflow(&root, "disconnect");
    let workspace_path = root.join("workspace");
    fs::create_dir_all(&workspace_path).expect("create workspace dir");
    let execution = engine
        .start(
            &definition,
            serde_json::json!({}),
            Workspace::local_folder(workspace_path.display().to_string()),
        )
        .expect("start execution");

    let handle = start_server_with_engine(
        Arc::clone(&engine),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .await;
    let raw = reqwest::Client::new();

    let response = raw
        .get(url(
            &handle,
            &format!("/executions/{}/stream", execution.id),
        ))
        .bearer_auth(handle.token())
        .send()
        .await
        .expect("client connects");
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    // Simulate a disconnect: drop the response (and so the connection)
    // without ever reading a single event from it, then drive the run that
    // publishes to the now-orphaned broadcast receiver — the comment on
    // `Engine::subscribe` promises `events.send`'s result is ignored, so
    // this must neither panic nor fail the run.
    drop(response);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let outcome = engine.run(&definition, &execution.id).await;
    assert!(
        outcome.is_ok(),
        "a run must complete normally even with a dropped SSE subscriber: {outcome:?}"
    );

    // The server itself must still be answering ordinary requests.
    let listing = raw
        .get(url(&handle, "/executions"))
        .bearer_auth(handle.token())
        .send()
        .await
        .expect("get /executions after the disconnect");
    assert_eq!(listing.status(), reqwest::StatusCode::OK);
}

// --- Auth: no/wrong bearer token is refused before the stream opens ---------

#[tokio::test]
async fn sse_endpoints_refuse_a_missing_or_wrong_token_before_streaming() {
    let root = TempRoot::new();
    let store = root.store();
    let engine = Arc::new(Engine::new(store.clone()));
    let handle = start_server_with_engine(
        Arc::clone(&engine),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .await;
    let raw = reqwest::Client::new();

    for path in [
        "/executions/does-not-exist/stream".to_string(),
        "/events/stream".to_string(),
    ] {
        let no_token = raw
            .get(url(&handle, &path))
            .send()
            .await
            .expect("request without a token");
        assert_eq!(
            no_token.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "path {path} without a token"
        );
        let body = no_token.text().await.expect("read body");
        assert!(body.is_empty(), "a refusal must carry no body content");

        let wrong_token = raw
            .get(url(&handle, &path))
            .bearer_auth("not-the-real-token")
            .send()
            .await
            .expect("request with the wrong token");
        assert_eq!(
            wrong_token.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "path {path} with the wrong token"
        );
    }
}

#[tokio::test]
async fn execution_stream_reports_an_unknown_execution_as_not_found() {
    let root = TempRoot::new();
    let store = root.store();
    let engine = Arc::new(Engine::new(store.clone()));
    let handle = start_server_with_engine(
        Arc::clone(&engine),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .await;
    let raw = reqwest::Client::new();

    let response = raw
        .get(url(&handle, "/executions/does-not-exist/stream"))
        .bearer_auth(handle.token())
        .send()
        .await
        .expect("request the stream for an unknown execution");
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
}
