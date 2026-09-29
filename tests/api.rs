//! Integration tests for the embedded API server (U5): the server, over a
//! real temporary store and a real bound loopback port, answering the three
//! read-only endpoints through the real `ApiClient`.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use narsor::api;
use narsor::api::{ClientError, ExecutionListResponse, ExecutionSummary, StartTaskResponse};
use narsor::domain::{
    Context, Event, EventData, ExecutionStatus, RepositoryInfo, WorkflowDefinition,
    WorkflowExecution, Workspace,
};
use narsor::engine::{ConversationState, Engine, HumanInputDelivery, PositionStatus};
use narsor::persistence::conversation::ConversationSource;
use narsor::persistence::{DefaultHarnessSelector, ExecutionStore, StatePaths};
use narsor::workflow::load_from_file;

/// A temporary directory that deletes itself when it goes out of scope.
///
/// Stands in for the `tempfile` crate, which is not a dependency of this
/// crate (mirrors `tests/conversation_store.rs`).
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-api-{}", uuid::Uuid::new_v4()));
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

fn sample_execution(id: &str, status: ExecutionStatus) -> WorkflowExecution {
    WorkflowExecution {
        id: id.to_string(),
        workflow_id: "implement-feature".to_string(),
        workflow_version: 1,
        status,
        current_state: Some("plan".to_string()),
        started_at: Utc::now(),
        completed_at: None,
        inputs: serde_json::json!({}),
        context: Context::new(),
        workspace: Workspace::git_checkout(
            "/tmp/workspace".to_string(),
            RepositoryInfo {
                id: "repo-1".to_string(),
                name: "narsor".to_string(),
                provider: "gitlab".to_string(),
                remote: "git@example.com:team/narsor.git".to_string(),
            },
            Some("main".to_string()),
        ),
        current_action_execution: None,
        metadata: serde_json::json!({}),
    }
}

/// Writes a workflow with a retried `plan` step into `store`: attempt-1
/// fails, attempt-2 succeeds — the timeline should read back as two
/// positions. Each attempt also gets a conversation record.
fn seed_execution_with_retry(store: &ExecutionStore, execution_id: &str) {
    let execution = sample_execution(execution_id, ExecutionStatus::Completed);
    store.save_execution(&execution).expect("save execution");

    let log = store.event_log(execution_id);
    log.append_next(
        &execution_id.to_string(),
        EventData::WorkflowStarted {
            workflow_id: "implement-feature".to_string(),
            workflow_version: 1,
            inputs: serde_json::json!({}),
        },
    )
    .expect("append WorkflowStarted");
    log.append_next(
        &execution_id.to_string(),
        EventData::StateEntered {
            state_id: "plan".to_string(),
        },
    )
    .expect("append StateEntered");
    log.append_next(
        &execution_id.to_string(),
        EventData::ActionStarted {
            action_execution_id: "attempt-1".to_string(),
            state_id: "plan".to_string(),
            action_type: "agent".to_string(),
        },
    )
    .expect("append ActionStarted");
    log.append_next(
        &execution_id.to_string(),
        EventData::ActionAttemptFailed {
            action_execution_id: "attempt-1".to_string(),
            state_id: "plan".to_string(),
            error: "boom".to_string(),
            retry_in_ms: 100,
        },
    )
    .expect("append ActionAttemptFailed");
    log.append_next(
        &execution_id.to_string(),
        EventData::ActionRetrying {
            action_execution_id: "attempt-2".to_string(),
            retry_count: 1,
            state_id: Some("plan".to_string()),
        },
    )
    .expect("append ActionRetrying");
    log.append_next(
        &execution_id.to_string(),
        EventData::ActionCompleted {
            action_execution_id: "attempt-2".to_string(),
            output_label: "success".to_string(),
        },
    )
    .expect("append ActionCompleted");

    let conversations = store.conversation_store(execution_id);
    let mut first = conversations.writer("attempt-1").expect("open writer 1");
    first
        .append_narsor("prompt: try the thing")
        .expect("append prompt 1");
    first
        .append(ConversationSource::Stdout, br#"{"type":"error"}"#)
        .expect("append stdout 1");
    first.finish().expect("finish writer 1");
    conversations
        .close_attempt("attempt-1")
        .expect("close attempt 1");

    let mut second = conversations.writer("attempt-2").expect("open writer 2");
    second
        .append_narsor("prompt: try again")
        .expect("append prompt 2");
    second
        .append(ConversationSource::Stdout, br#"{"type":"assistant"}"#)
        .expect("append stdout 2");
    second.finish().expect("finish writer 2");
    conversations
        .close_attempt("attempt-2")
        .expect("close attempt 2");
}

/// Starts the server over a fresh, otherwise unused engine — for the
/// original read-only endpoint tests, which never drive anything and so
/// don't care what the catalog/work roots are.
async fn start_server(store: ExecutionStore) -> api::ApiHandle {
    let root = std::env::temp_dir();
    start_server_with_engine(Arc::new(Engine::new(store)), root.clone(), root).await
}

/// Starts the server over a caller-supplied, already-`Arc`-wrapped engine —
/// for the mutating endpoint tests (U3), which need to keep driving that
/// same engine directly (to seed a paused execution, or to hold the
/// "already running" claim while asserting a conflict) alongside what goes
/// over HTTP.
async fn start_server_with_engine(
    engine: Arc<Engine>,
    catalog_dir: PathBuf,
    work_dir: PathBuf,
) -> api::ApiHandle {
    api::start(engine, catalog_dir, work_dir)
        .await
        .expect("server starts")
}

async fn start_server_with_harness_selector(
    root: &TempRoot,
    selector: Arc<DefaultHarnessSelector>,
) -> api::ApiHandle {
    api::start_with_harness_selector(
        Arc::new(Engine::new(root.store())),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
        Some(selector),
    )
    .await
    .expect("server starts with selector")
}

#[tokio::test]
async fn harness_settings_are_authenticated_and_return_authoritative_snapshots() {
    let root = TempRoot::new();
    let selector = Arc::new(
        DefaultHarnessSelector::load(
            StatePaths::new(root.path()),
            vec!["claude-code".into(), "codex".into(), "opencode".into()],
            "claude-code",
        )
        .await
        .expect("load selector"),
    );
    let server = start_server_with_harness_selector(&root, Arc::clone(&selector)).await;
    let client = server.client();

    let initial = client.harness_selection().await.expect("authenticated GET");
    assert_eq!(initial.effective, "claude-code");
    assert_eq!(initial.override_harness, None);
    assert_eq!(initial.revision, 0);

    let changed = client
        .set_harness_selection("codex")
        .await
        .expect("authenticated PUT");
    assert_eq!(changed.override_harness.as_deref(), Some("codex"));
    assert_eq!(changed.effective, "codex");
    assert_eq!(changed.revision, 1);

    let reset = client
        .reset_harness_selection()
        .await
        .expect("authenticated DELETE");
    assert_eq!(reset.override_harness, None);
    assert_eq!(reset.effective, "claude-code");
    assert_eq!(reset.revision, 2);

    let unauthenticated = api::ApiClient::new(server.addr(), "wrong-token");
    assert!(matches!(
        unauthenticated.harness_selection().await,
        Err(ClientError::Unauthorized)
    ));
    server.shutdown().await;
}

#[tokio::test]
async fn invalid_harness_setting_is_typed_and_does_not_change_the_snapshot() {
    let root = TempRoot::new();
    let selector = Arc::new(
        DefaultHarnessSelector::load(
            StatePaths::new(root.path()),
            vec!["claude-code".into(), "codex".into(), "opencode".into()],
            "claude-code",
        )
        .await
        .expect("load selector"),
    );
    let server = start_server_with_harness_selector(&root, selector).await;
    let client = server.client();

    let error = client
        .set_harness_selection("missing")
        .await
        .expect_err("unknown harness is rejected");
    assert!(matches!(
        error,
        ClientError::Request { ref code, .. } if code == "unsupported_harness"
    ));
    let current = client
        .harness_selection()
        .await
        .expect("state is unchanged");
    assert_eq!(current.effective, "claude-code");
    assert_eq!(current.override_harness, None);
    assert_eq!(current.revision, 0);
    server.shutdown().await;
}

/// The directory the TUI's own fixtures live in — reused here rather than
/// duplicated, since `pipeline.yaml` (one script state, one human state, two
/// terminals) is exactly the shape these tests need too.
fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tui")
}

fn pipeline_definition_path() -> PathBuf {
    fixtures().join("pipeline.yaml")
}

fn prepare_script_path() -> PathBuf {
    fixtures().join("prepare.sh")
}

/// Writes (and loads) the smallest workflow that validates: one script state
/// that always succeeds, straight into a terminal. For tests that need an
/// execution to run to completion without any human gate in the way.
fn write_script_to_done_workflow(root: &TempRoot) -> (WorkflowDefinition, PathBuf) {
    let script_path = root.join("succeed.sh");
    fs::write(&script_path, "#!/bin/sh\nexit 0\n").expect("write script");
    let mut permissions = fs::metadata(&script_path)
        .expect("stat script")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script_path, permissions).expect("chmod script");

    let yaml = format!(
        r#"
name: api-resume-fixture
version: 1
description: Minimal script-to-terminal fixture for the resume endpoint test.

states:
  start:
    description: Runs a script that always succeeds.
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

    let path = root.join("api-resume-fixture.yaml");
    fs::write(&path, yaml).expect("write definition");
    let definition = load_from_file(&path).expect("load definition");
    (definition, path)
}

/// Writes (and loads) a workflow with a non-terminal `waiting` state whose
/// declared transitions are exactly `approve -> done` and `reject ->
/// aborted` — for the transition endpoint tests, which need a state with a
/// real transition graph to validate a label against but never actually run
/// the state's own action.
fn write_transition_fixture(root: &TempRoot) -> (WorkflowDefinition, PathBuf) {
    let yaml = r#"
name: api-transition-fixture
version: 1
description: Fixture with a declared transition graph, for the transition endpoint tests.

states:
  waiting:
    description: A person decides, by hand, whether to approve or reject.
    allow_manual_override: true
    action:
      type: human
      message: "Approve?"
      inputs:
        - name: approval_status
          type: select
          required: true
          options:
            - approved
            - rejected
    outputs:
      - approve
      - reject
    transitions:
      approve: done
      reject: aborted

  done:
    description: Approved.

  aborted:
    description: Rejected.

terminal_states:
  done: completed
  aborted: failed
"#;

    let path = root.join("api-transition-fixture.yaml");
    fs::write(&path, yaml).expect("write definition");
    let definition = load_from_file(&path).expect("load definition");
    (definition, path)
}

/// Records where `definition_path` lives in `execution`'s own metadata, the
/// way `tasks::start_task` does — so the server's own
/// `tasks::resolve_definition` can find the workflow again for
/// resume/input/transition, exactly as `narsor resume`/`input` already rely
/// on it to.
fn pin_definition_path(store: &ExecutionStore, execution_id: &str, definition_path: &Path) {
    let mut execution = store
        .load_execution(execution_id)
        .expect("reload execution");
    execution.metadata = serde_json::json!({
        "cli": { "definition_path": definition_path.display().to_string() }
    });
    store
        .save_execution(&execution)
        .expect("save execution with pinned definition path");
}

/// Polls the store until `execution_id` reaches `expected`, or panics once
/// `timeout` elapses.
async fn wait_for_status(
    store: &ExecutionStore,
    execution_id: &str,
    expected: ExecutionStatus,
    timeout: Duration,
) -> WorkflowExecution {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let execution = store.load_execution(execution_id).expect("load execution");
        if execution.status == expected {
            return execution;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "execution '{execution_id}' did not reach {expected:?} within {timeout:?} \
                 (last status: {:?})",
                execution.status
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// --- Happy paths -------------------------------------------------------

#[tokio::test]
async fn timeline_endpoint_reports_two_positions_after_a_retry() {
    let root = TempRoot::new();
    let store = root.store();
    seed_execution_with_retry(&store, "exec-1");

    let handle = start_server(store).await;
    let client = handle.client();

    let timeline = client.timeline("exec-1").await.expect("timeline request");

    assert_eq!(timeline.positions.len(), 2);
    assert_eq!(timeline.positions[0].label, "plan #1");
    assert_eq!(timeline.positions[0].status, PositionStatus::Failed);
    assert_eq!(timeline.positions[1].label, "plan #2");
    assert_eq!(timeline.positions[1].status, PositionStatus::Succeeded);
    assert_eq!(
        timeline.positions[0].conversation,
        ConversationState::Recorded
    );
    assert_eq!(
        timeline.positions[1].conversation,
        ConversationState::Recorded
    );
}

#[tokio::test]
async fn conversation_endpoint_returns_each_attempts_entries() {
    let root = TempRoot::new();
    let store = root.store();
    seed_execution_with_retry(&store, "exec-1");

    let handle = start_server(store).await;
    let client = handle.client();

    let first = client
        .conversation("exec-1", "attempt-1", 0, 1 << 20)
        .await
        .expect("conversation request 1");
    let second = client
        .conversation("exec-1", "attempt-2", 0, 1 << 20)
        .await
        .expect("conversation request 2");

    let names = |response: &narsor::api::ConversationResponse| match response {
        narsor::api::ConversationResponse::Recorded { entries, .. } => entries.len(),
        narsor::api::ConversationResponse::NoRecord => 0,
    };
    assert_eq!(names(&first), 2);
    assert_eq!(names(&second), 2);
}

/// Covers AE4: reading a running attempt's conversation, appending more
/// entries, then reading again with the returned cursor yields only the new
/// entries.
#[tokio::test]
async fn conversation_endpoint_cursor_returns_only_new_entries_for_a_running_attempt() {
    let root = TempRoot::new();
    let store = root.store();
    let execution = sample_execution("exec-running", ExecutionStatus::Running);
    store.save_execution(&execution).expect("save execution");

    // Keep the writer open across the whole test, mirroring a harness
    // process still appending to it — `ConversationWriter` holds its own
    // file handle to the path, independent of the `ExecutionStore` moved
    // into the server below.
    let conversations = store.conversation_store("exec-running");
    let mut writer = conversations.writer("attempt-live").expect("open writer");
    writer.append_narsor("prompt: go").expect("append prompt");
    writer
        .append(ConversationSource::Stdout, b"line one")
        .expect("append line one");

    let handle = start_server(store).await;
    let client = handle.client();

    let first = client
        .conversation("exec-running", "attempt-live", 0, 1 << 20)
        .await
        .expect("first conversation request");
    let (first_entries, next_cursor, closed) = match first {
        narsor::api::ConversationResponse::Recorded {
            entries,
            next_entry,
            closed,
            ..
        } => (entries, next_entry, closed),
        narsor::api::ConversationResponse::NoRecord => panic!("expected a record"),
    };
    assert_eq!(first_entries.len(), 2);
    assert!(!closed, "an open attempt is not closed");

    // Simulate the still-running harness producing more output, then read
    // again from the cursor the first response handed back.
    writer
        .append(ConversationSource::Stdout, b"line two")
        .expect("append line two");
    writer
        .append_narsor("exit code: 0")
        .expect("append completion");

    let second = client
        .conversation("exec-running", "attempt-live", next_cursor, 1 << 20)
        .await
        .expect("second conversation request");
    let second_entries = match second {
        narsor::api::ConversationResponse::Recorded { entries, .. } => entries,
        narsor::api::ConversationResponse::NoRecord => panic!("expected a record"),
    };
    assert_eq!(second_entries.len(), 2, "only the newly appended entries");
    assert_eq!(second_entries[0].entry_number, 3);
    assert_eq!(second_entries[1].entry_number, 4);
}

// --- Edge cases ----------------------------------------------------------

#[tokio::test]
async fn conversation_endpoint_returns_a_single_oversized_entry_whole() {
    let root = TempRoot::new();
    let store = root.store();
    let execution = sample_execution("exec-big", ExecutionStatus::Completed);
    store.save_execution(&execution).expect("save execution");

    let conversations = store.conversation_store("exec-big");
    let mut writer = conversations.writer("attempt-big").expect("open writer");
    let huge = vec![b'x'; 4096];
    writer
        .append(ConversationSource::Stdout, &huge)
        .expect("append huge entry");
    writer
        .append(ConversationSource::Stdout, b"small")
        .expect("append small entry");
    writer.finish().expect("finish writer");
    conversations
        .close_attempt("attempt-big")
        .expect("close attempt");

    let handle = start_server(store).await;
    let client = handle.client();

    // A byte budget far smaller than the first entry: it must still come
    // back whole, alone, with the cursor past it.
    let response = client
        .conversation("exec-big", "attempt-big", 0, 16)
        .await
        .expect("conversation request");

    match response {
        narsor::api::ConversationResponse::Recorded {
            entries,
            next_entry,
            ..
        } => {
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].entry_number, 1);
            assert_eq!(next_entry, 2);
        }
        narsor::api::ConversationResponse::NoRecord => panic!("expected a record"),
    }
}

#[tokio::test]
async fn events_endpoint_only_returns_events_after_the_given_sequence() {
    let root = TempRoot::new();
    let store = root.store();
    let execution = sample_execution("exec-events", ExecutionStatus::Running);
    store.save_execution(&execution).expect("save execution");

    let log = store.event_log("exec-events");
    for _ in 0..3 {
        log.append_next(
            &"exec-events".to_string(),
            EventData::StateEntered {
                state_id: "plan".to_string(),
            },
        )
        .expect("append event");
    }

    let handle = start_server(store).await;
    let client = handle.client();

    let events: Vec<Event> = client
        .events("exec-events", 1)
        .await
        .expect("events request");

    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|event| event.sequence > 1));
}

// --- Error paths -----------------------------------------------------------

#[tokio::test]
async fn missing_or_wrong_token_is_refused() {
    let root = TempRoot::new();
    let store = root.store();
    seed_execution_with_retry(&store, "exec-1");

    let handle = start_server(store).await;
    let raw = reqwest::Client::new();

    let url = format!("http://{}/executions/exec-1/timeline", handle.addr());

    let no_token = raw.get(&url).send().await.expect("request without token");
    assert_eq!(no_token.status(), reqwest::StatusCode::UNAUTHORIZED);
    let body = no_token.text().await.expect("read body");
    assert!(body.is_empty(), "refusal must carry no body content");

    let wrong_token = raw
        .get(&url)
        .bearer_auth("not-the-real-token")
        .send()
        .await
        .expect("request with wrong token");
    assert_eq!(wrong_token.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unknown_execution_is_reported_as_not_found() {
    let root = TempRoot::new();
    let store = root.store();

    let handle = start_server(store).await;
    let client = handle.client();

    let result = client.timeline("does-not-exist").await;
    assert!(matches!(result, Err(ClientError::NotFound)));
}

#[tokio::test]
async fn an_attempt_without_a_conversation_record_is_not_an_error() {
    let root = TempRoot::new();
    let store = root.store();
    let execution = sample_execution("exec-no-record", ExecutionStatus::Completed);
    store.save_execution(&execution).expect("save execution");

    let handle = start_server(store).await;
    let client = handle.client();

    let response = client
        .conversation("exec-no-record", "never-written", 0, 1 << 20)
        .await
        .expect("conversation request must not error");

    assert!(matches!(
        response,
        narsor::api::ConversationResponse::NoRecord
    ));
}

// --- Integration ------------------------------------------------------------

/// A client against a running server on a random port walking the full path
/// event → timeline → conversation.
#[tokio::test]
async fn client_walks_event_to_timeline_to_conversation() {
    let root = TempRoot::new();
    let store = root.store();
    seed_execution_with_retry(&store, "exec-1");

    let handle = start_server(store).await;
    let client = handle.client();

    let events = client.events("exec-1", 0).await.expect("events request");
    assert!(!events.is_empty());

    let timeline = client.timeline("exec-1").await.expect("timeline request");
    let mut by_label: HashMap<String, &str> = HashMap::new();
    for position in &timeline.positions {
        let attempt_id = position
            .action_execution_id
            .as_deref()
            .expect("every position here has an attempt id");
        by_label.insert(position.label.clone(), attempt_id);
    }
    let attempt_id = by_label.get("plan #2").expect("plan #2 position exists");

    let conversation = client
        .conversation("exec-1", attempt_id, 0, 1 << 20)
        .await
        .expect("conversation request");
    match conversation {
        narsor::api::ConversationResponse::Recorded { entries, .. } => {
            assert!(!entries.is_empty());
        }
        narsor::api::ConversationResponse::NoRecord => panic!("expected a record"),
    }
}

// ---------------------------------------------------------------------------
// U3: mutating endpoints
// ---------------------------------------------------------------------------

fn raw_client() -> reqwest::Client {
    reqwest::Client::new()
}

fn url(handle: &api::ApiHandle, path: &str) -> String {
    format!("http://{}{path}", handle.addr())
}

#[tokio::test]
async fn post_tasks_creates_an_execution_and_it_appears_in_the_list() {
    let root = TempRoot::new();
    let store = root.store();
    let engine = Arc::new(Engine::new(store.clone()));
    let handle = start_server_with_engine(
        Arc::clone(&engine),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .await;
    let raw = raw_client();

    let body = serde_json::json!({
        "workflow": pipeline_definition_path().display().to_string(),
        "inputs": {
            "script_path": prepare_script_path().display().to_string(),
            "ticket": "ORC-1",
        }
    });

    let response = raw
        .post(url(&handle, "/tasks"))
        .bearer_auth(handle.token())
        .json(&body)
        .send()
        .await
        .expect("post /tasks");
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    let parsed: StartTaskResponse = response.json().await.expect("parse start-task response");
    let execution_id = parsed.execution_id;
    assert!(store.exists(&execution_id));

    let listing = raw
        .get(url(&handle, "/executions"))
        .bearer_auth(handle.token())
        .send()
        .await
        .expect("get /executions");
    assert_eq!(listing.status(), reqwest::StatusCode::OK);
    let listing: ExecutionListResponse = listing.json().await.expect("parse listing");
    assert!(listing
        .executions
        .iter()
        .any(|execution| execution.id == execution_id));

    // The workflow's script state always succeeds, so the run reaches the
    // human gate at `review` and stops there — proof the background
    // `tokio::spawn(engine.run(...))` actually ran.
    wait_for_status(
        &store,
        &execution_id,
        ExecutionStatus::InputNeeded,
        Duration::from_secs(5),
    )
    .await;
}

/// U8: proves `POST /tasks` is not a divergent reimplementation of
/// `tasks::start_task` but a thin wrapper calling the very same function.
///
/// The handler's source shows a single call to `tasks::start_task` (see
/// `src/api/routes.rs::post_start_task`), but that alone would not catch a
/// future edit that swapped in a lookalike. This test instead proves it
/// behaviourally, by relying on a side effect only the real `start_task`
/// performs and that no other test in this file lets happen on its own:
/// recording `metadata.cli.definition_path` (`record_definition_source`).
/// Every other paused-execution test here calls the `pin_definition_path`
/// test helper to fake that recording by hand; this test never does, and
/// instead lets `POST /tasks` alone create the execution, then drives it
/// straight through the human gate (`POST /input`) to completion. If the
/// handler ever stopped calling the real `start_task` — or called a
/// reimplementation that skipped pinning the definition source — resolving
/// the definition for `/input` would fail with `DefinitionLookupError` and
/// this test would fail closed.
#[tokio::test]
async fn post_tasks_calls_the_real_start_task_and_records_a_resolvable_definition() {
    let root = TempRoot::new();
    let store = root.store();
    let engine = Arc::new(Engine::new(store.clone()));
    let handle = start_server_with_engine(
        Arc::clone(&engine),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .await;
    let raw = raw_client();

    let body = serde_json::json!({
        "workflow": pipeline_definition_path().display().to_string(),
        "inputs": {
            "script_path": prepare_script_path().display().to_string(),
            "ticket": "ORC-3",
        }
    });

    let response = raw
        .post(url(&handle, "/tasks"))
        .bearer_auth(handle.token())
        .json(&body)
        .send()
        .await
        .expect("post /tasks");
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    let execution_id = response
        .json::<StartTaskResponse>()
        .await
        .expect("parse start-task response")
        .execution_id;

    wait_for_status(
        &store,
        &execution_id,
        ExecutionStatus::InputNeeded,
        Duration::from_secs(5),
    )
    .await;

    // No `pin_definition_path` call anywhere above: the definition path this
    // resolves through below can only have come from `start_task` itself.
    let delivery = HumanInputDelivery::with_values(
        serde_json::json!({
            "approval_status": "approved",
            "comment": "confirms start_task recorded the definition source",
        })
        .as_object()
        .cloned()
        .expect("object"),
    );

    let response = raw
        .post(url(&handle, &format!("/executions/{execution_id}/input")))
        .bearer_auth(handle.token())
        .json(&delivery)
        .send()
        .await
        .expect("post input");
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);

    wait_for_status(
        &store,
        &execution_id,
        ExecutionStatus::Completed,
        Duration::from_secs(5),
    )
    .await;
}

#[tokio::test]
async fn post_resume_drives_a_paused_execution_to_a_terminal_state() {
    let root = TempRoot::new();
    let store = root.store();
    let engine = Arc::new(Engine::new(store.clone()));

    let (definition, definition_path) = write_script_to_done_workflow(&root);
    let workspace_path = root.join("workspace");
    fs::create_dir_all(&workspace_path).expect("create workspace dir");
    let execution = engine
        .start(
            &definition,
            serde_json::json!({}),
            Workspace::local_folder(workspace_path.display().to_string()),
        )
        .expect("start execution");
    engine
        .pause_now(&execution.id, "paused for the resume test")
        .expect("pause the execution");
    pin_definition_path(&store, &execution.id, &definition_path);

    let handle = start_server_with_engine(
        Arc::clone(&engine),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .await;
    let raw = raw_client();

    let response = raw
        .post(url(
            &handle,
            &format!("/executions/{}/resume", execution.id),
        ))
        .bearer_auth(handle.token())
        .send()
        .await
        .expect("post resume");
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);

    wait_for_status(
        &store,
        &execution.id,
        ExecutionStatus::Completed,
        Duration::from_secs(5),
    )
    .await;
}

#[tokio::test]
async fn post_input_delivers_an_answer_to_a_waiting_execution() {
    let root = TempRoot::new();
    let store = root.store();
    let engine = Arc::new(Engine::new(store.clone()));

    let definition = load_from_file(pipeline_definition_path()).expect("load pipeline fixture");
    let workspace_path = root.join("workspace");
    fs::create_dir_all(&workspace_path).expect("create workspace dir");
    let execution = engine
        .start(
            &definition,
            serde_json::json!({
                "script_path": prepare_script_path().display().to_string(),
                "ticket": "ORC-2",
            }),
            Workspace::local_folder(workspace_path.display().to_string()),
        )
        .expect("start execution");
    pin_definition_path(&store, &execution.id, &pipeline_definition_path());

    // Drives the execution to the human gate directly through the engine —
    // exactly what `POST /tasks` does in the background — before the server
    // (and its own claim on the execution) is even started.
    engine
        .run(&definition, &execution.id)
        .await
        .expect("run to the human gate");
    assert_eq!(
        store.load_execution(&execution.id).expect("reload").status,
        ExecutionStatus::InputNeeded
    );
    assert!(store.has_pending_input(&execution.id));

    let handle = start_server_with_engine(
        Arc::clone(&engine),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .await;
    let raw = raw_client();

    let delivery = HumanInputDelivery::with_values(
        serde_json::json!({
            "approval_status": "approved",
            "comment": "looks good",
        })
        .as_object()
        .cloned()
        .expect("object"),
    );

    let response = raw
        .post(url(&handle, &format!("/executions/{}/input", execution.id)))
        .bearer_auth(handle.token())
        .json(&delivery)
        .send()
        .await
        .expect("post input");
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);

    wait_for_status(
        &store,
        &execution.id,
        ExecutionStatus::Completed,
        Duration::from_secs(5),
    )
    .await;
}

#[tokio::test]
async fn post_transition_happy_path_writes_the_expected_events_and_state() {
    let root = TempRoot::new();
    let store = root.store();
    let engine = Arc::new(Engine::new(store.clone()));

    let (definition, definition_path) = write_transition_fixture(&root);
    let workspace_path = root.join("workspace");
    fs::create_dir_all(&workspace_path).expect("create workspace dir");
    let mut execution = engine
        .start(
            &definition,
            serde_json::json!({}),
            Workspace::local_folder(workspace_path.display().to_string()),
        )
        .expect("start execution");
    execution.current_state = Some("waiting".to_string());
    execution.status = ExecutionStatus::Paused;
    store.save_execution(&execution).expect("save execution");
    pin_definition_path(&store, &execution.id, &definition_path);

    let handle = start_server_with_engine(
        Arc::clone(&engine),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .await;
    let raw = raw_client();

    let body = TransitionRequestBody {
        label: Some("approve".to_string()),
        target: None,
        force: false,
    };

    let response = raw
        .post(url(
            &handle,
            &format!("/executions/{}/transition", execution.id),
        ))
        .bearer_auth(handle.token())
        .json(&body)
        .send()
        .await
        .expect("post transition");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let summary: ExecutionSummary = response.json().await.expect("parse summary");
    assert_eq!(summary.current_state.as_deref(), Some("done"));
    assert_eq!(summary.status, ExecutionStatus::Completed);

    let events = store
        .event_log(&execution.id)
        .read_all()
        .expect("read events");
    let kinds: Vec<&str> = events
        .iter()
        .map(|event| match &event.data {
            EventData::WorkflowStarted { .. } => "workflow_started",
            EventData::DecisionMade { .. } => "decision_made",
            EventData::HumanTransitionSelected { .. } => "human_transition_selected",
            EventData::StateExited { .. } => "state_exited",
            EventData::TransitionTaken { .. } => "transition_taken",
            EventData::StateEntered { .. } => "state_entered",
            EventData::WorkflowCompleted { .. } => "workflow_completed",
            other => panic!("unexpected event in this fixture's log: {other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "workflow_started",
            "decision_made",
            "human_transition_selected",
            "state_exited",
            "transition_taken",
            "state_entered",
            "workflow_completed",
        ]
    );

    let reloaded = store.load_execution(&execution.id).expect("reload");
    assert_eq!(reloaded.status, ExecutionStatus::Completed);
    assert_eq!(reloaded.current_state.as_deref(), Some("done"));
}

#[tokio::test]
async fn post_transition_to_an_illegal_label_is_rejected_with_no_write() {
    let root = TempRoot::new();
    let store = root.store();
    let engine = Arc::new(Engine::new(store.clone()));

    let (definition, definition_path) = write_transition_fixture(&root);
    let workspace_path = root.join("workspace");
    fs::create_dir_all(&workspace_path).expect("create workspace dir");
    let mut execution = engine
        .start(
            &definition,
            serde_json::json!({}),
            Workspace::local_folder(workspace_path.display().to_string()),
        )
        .expect("start execution");
    execution.current_state = Some("waiting".to_string());
    execution.status = ExecutionStatus::Paused;
    store.save_execution(&execution).expect("save execution");
    pin_definition_path(&store, &execution.id, &definition_path);

    let events_before = store
        .event_log(&execution.id)
        .read_all()
        .expect("read events");

    let handle = start_server_with_engine(
        Arc::clone(&engine),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .await;
    let raw = raw_client();

    let body = TransitionRequestBody {
        label: Some("not-a-declared-label".to_string()),
        target: None,
        force: false,
    };

    let response = raw
        .post(url(
            &handle,
            &format!("/executions/{}/transition", execution.id),
        ))
        .bearer_auth(handle.token())
        .json(&body)
        .send()
        .await
        .expect("post transition");
    assert!(
        response.status() == reqwest::StatusCode::BAD_REQUEST
            || response.status() == reqwest::StatusCode::CONFLICT
    );

    let events_after = store
        .event_log(&execution.id)
        .read_all()
        .expect("read events");
    assert_eq!(
        events_before.len(),
        events_after.len(),
        "no event was written"
    );
    let reloaded = store.load_execution(&execution.id).expect("reload");
    assert_eq!(reloaded.status, ExecutionStatus::Paused, "no status change");
    assert_eq!(reloaded.current_state.as_deref(), Some("waiting"));
}

#[tokio::test]
async fn post_transition_on_an_active_execution_is_a_conflict_with_no_write() {
    let root = TempRoot::new();
    let store = root.store();
    let engine = Arc::new(Engine::new(store.clone()));

    // A script that sleeps briefly, so the execution is observably "active"
    // (`Engine::is_active`) for long enough to race the transition against.
    let script_path = root.join("slow.sh");
    fs::write(&script_path, "#!/bin/sh\nsleep 1\nexit 0\n").expect("write script");
    let mut permissions = fs::metadata(&script_path).expect("stat").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script_path, permissions).expect("chmod");

    let yaml = format!(
        r#"
name: api-active-conflict-fixture
version: 1
description: A slow script state, for the "active execution" conflict test.

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
    let definition_path = root.join("api-active-conflict-fixture.yaml");
    fs::write(&definition_path, yaml).expect("write definition");
    let definition = load_from_file(&definition_path).expect("load definition");

    let workspace_path = root.join("workspace");
    fs::create_dir_all(&workspace_path).expect("create workspace dir");
    let execution = engine
        .start(
            &definition,
            serde_json::json!({}),
            Workspace::local_folder(workspace_path.display().to_string()),
        )
        .expect("start execution");
    pin_definition_path(&store, &execution.id, &definition_path);

    // Drives the execution directly through the engine (not through the
    // server) — a background pass exactly like `POST /tasks`'s
    // `tokio::spawn(engine.run(...))` would leave running.
    let running_engine = Arc::clone(&engine);
    let running_definition = definition.clone();
    let execution_id = execution.id.clone();
    let run_task = tokio::spawn(async move {
        let _ = running_engine.run(&running_definition, &execution_id).await;
    });

    // Wait until the engine actually claims the execution before racing the
    // transition against it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while !engine.is_active(&execution.id) {
        if tokio::time::Instant::now() >= deadline {
            panic!("the background run never became active");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let handle = start_server_with_engine(
        Arc::clone(&engine),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .await;
    let raw = raw_client();

    let body = TransitionRequestBody {
        label: Some("success".to_string()),
        target: None,
        force: false,
    };
    let response = raw
        .post(url(
            &handle,
            &format!("/executions/{}/transition", execution.id),
        ))
        .bearer_auth(handle.token())
        .json(&body)
        .send()
        .await
        .expect("post transition");
    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);

    run_task.await.expect("background run task");
    wait_for_status(
        &store,
        &execution.id,
        ExecutionStatus::Completed,
        Duration::from_secs(5),
    )
    .await;
}

/// Covers AE4: two concurrent `POST /executions/:id/resume` for the same id
/// — exactly one `202`, the other `409`.
#[tokio::test]
async fn concurrent_resume_yields_exactly_one_accepted_and_one_conflict() {
    let root = TempRoot::new();
    let store = root.store();
    let engine = Arc::new(Engine::new(store.clone()));

    let script_path = root.join("slow.sh");
    fs::write(&script_path, "#!/bin/sh\nsleep 1\nexit 0\n").expect("write script");
    let mut permissions = fs::metadata(&script_path).expect("stat").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script_path, permissions).expect("chmod");

    let yaml = format!(
        r#"
name: api-concurrent-resume-fixture
version: 1
description: A slow script state, for the concurrent-resume (AE4) test.

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
    let definition_path = root.join("api-concurrent-resume-fixture.yaml");
    fs::write(&definition_path, yaml).expect("write definition");
    let definition = load_from_file(&definition_path).expect("load definition");

    let workspace_path = root.join("workspace");
    fs::create_dir_all(&workspace_path).expect("create workspace dir");
    let execution = engine
        .start(
            &definition,
            serde_json::json!({}),
            Workspace::local_folder(workspace_path.display().to_string()),
        )
        .expect("start execution");
    engine
        .pause_now(&execution.id, "paused for the concurrent-resume test")
        .expect("pause");
    pin_definition_path(&store, &execution.id, &definition_path);

    let handle = start_server_with_engine(
        Arc::clone(&engine),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .await;
    let raw = raw_client();

    let request = || {
        raw.post(url(
            &handle,
            &format!("/executions/{}/resume", execution.id),
        ))
        .bearer_auth(handle.token())
        .send()
    };

    let (first, second) = tokio::join!(request(), request());
    let statuses = [
        first.expect("first resume request").status(),
        second.expect("second resume request").status(),
    ];

    let accepted = statuses
        .iter()
        .filter(|status| **status == reqwest::StatusCode::ACCEPTED)
        .count();
    let conflicts = statuses
        .iter()
        .filter(|status| **status == reqwest::StatusCode::CONFLICT)
        .count();
    assert_eq!(
        accepted, 1,
        "exactly one request must be accepted: {statuses:?}"
    );
    assert_eq!(
        conflicts, 1,
        "exactly one request must conflict: {statuses:?}"
    );

    wait_for_status(
        &store,
        &execution.id,
        ExecutionStatus::Completed,
        Duration::from_secs(5),
    )
    .await;
}

#[tokio::test]
async fn unknown_execution_id_is_not_found_on_every_mutating_endpoint() {
    let root = TempRoot::new();
    let store = root.store();
    let engine = Arc::new(Engine::new(store.clone()));
    let handle = start_server_with_engine(
        Arc::clone(&engine),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .await;
    let raw = raw_client();

    let resume = raw
        .post(url(&handle, "/executions/does-not-exist/resume"))
        .bearer_auth(handle.token())
        .send()
        .await
        .expect("post resume");
    assert_eq!(resume.status(), reqwest::StatusCode::NOT_FOUND);

    let input = raw
        .post(url(&handle, "/executions/does-not-exist/input"))
        .bearer_auth(handle.token())
        .json(&HumanInputDelivery::label("approved"))
        .send()
        .await
        .expect("post input");
    assert_eq!(input.status(), reqwest::StatusCode::NOT_FOUND);

    let transition = raw
        .post(url(&handle, "/executions/does-not-exist/transition"))
        .bearer_auth(handle.token())
        .json(&TransitionRequestBody {
            label: Some("approve".to_string()),
            target: None,
            force: false,
        })
        .send()
        .await
        .expect("post transition");
    assert_eq!(transition.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn mutating_endpoints_refuse_a_missing_or_wrong_token() {
    let root = TempRoot::new();
    let store = root.store();
    let engine = Arc::new(Engine::new(store.clone()));
    let handle = start_server_with_engine(
        Arc::clone(&engine),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .await;
    let raw = raw_client();

    let no_token = raw
        .post(url(&handle, "/tasks"))
        .json(&serde_json::json!({ "workflow": "does-not-matter", "inputs": {} }))
        .send()
        .await
        .expect("post tasks without a token");
    assert_eq!(no_token.status(), reqwest::StatusCode::UNAUTHORIZED);
    let body = no_token.text().await.expect("read body");
    assert!(body.is_empty(), "refusal must carry no body content");

    let wrong_token = raw
        .get(url(&handle, "/executions"))
        .bearer_auth("not-the-real-token")
        .send()
        .await
        .expect("get executions with the wrong token");
    assert_eq!(wrong_token.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_unsafe_execution_id_on_transition_is_rejected_like_not_found() {
    let root = TempRoot::new();
    let store = root.store();
    let engine = Arc::new(Engine::new(store.clone()));
    let handle = start_server_with_engine(
        Arc::clone(&engine),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .await;
    let raw = raw_client();

    // `../../etc` URL-encoded, so it survives as a single path segment
    // rather than being collapsed by the HTTP client before it ever reaches
    // the server; `is_safe_id` must still reject it.
    let response = raw
        .post(url(&handle, "/executions/..%2F..%2Fetc/transition"))
        .bearer_auth(handle.token())
        .json(&TransitionRequestBody {
            label: Some("approve".to_string()),
            target: None,
            force: false,
        })
        .send()
        .await
        .expect("post transition with an unsafe id");
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
}

/// A local stand-in for [`TransitionRequest`].
///
/// `TransitionRequest` itself gained `Serialize` in U5 (the client's own
/// `transition` method needs it to send a request body), but these tests
/// predate that and are left using their own copy rather than churning every
/// call site.
#[derive(serde::Serialize)]
struct TransitionRequestBody {
    label: Option<String>,
    target: Option<String>,
    force: bool,
}

// ---------------------------------------------------------------------------
// U5: `ApiClient`'s own mutating methods and SSE streaming
// ---------------------------------------------------------------------------

use futures::StreamExt as _;
use narsor::api::TransitionRequest;

#[tokio::test]
async fn client_start_task_creates_an_execution_that_reaches_the_human_gate() {
    let root = TempRoot::new();
    let store = root.store();
    let engine = Arc::new(Engine::new(store.clone()));
    let handle = start_server_with_engine(
        Arc::clone(&engine),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .await;
    let client = handle.client();

    let inputs = serde_json::json!({
        "script_path": prepare_script_path().display().to_string(),
        "ticket": "ORC-CLIENT-1",
    })
    .as_object()
    .cloned()
    .expect("object");

    let response = client
        .start_task(&pipeline_definition_path().display().to_string(), inputs)
        .await
        .expect("start_task");
    assert!(store.exists(&response.execution_id));

    wait_for_status(
        &store,
        &response.execution_id,
        ExecutionStatus::InputNeeded,
        Duration::from_secs(5),
    )
    .await;
}

#[tokio::test]
async fn client_resume_drives_a_paused_execution_to_completion() {
    let root = TempRoot::new();
    let store = root.store();
    let engine = Arc::new(Engine::new(store.clone()));

    let (definition, definition_path) = write_script_to_done_workflow(&root);
    let workspace_path = root.join("workspace");
    fs::create_dir_all(&workspace_path).expect("create workspace dir");
    let execution = engine
        .start(
            &definition,
            serde_json::json!({}),
            Workspace::local_folder(workspace_path.display().to_string()),
        )
        .expect("start execution");
    engine
        .pause_now(&execution.id, "paused for the client resume test")
        .expect("pause the execution");
    pin_definition_path(&store, &execution.id, &definition_path);

    let handle = start_server_with_engine(
        Arc::clone(&engine),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .await;
    let client = handle.client();

    let response = client.resume(&execution.id).await.expect("client resume");
    assert_eq!(response.execution_id, execution.id);

    wait_for_status(
        &store,
        &execution.id,
        ExecutionStatus::Completed,
        Duration::from_secs(5),
    )
    .await;
}

#[tokio::test]
async fn client_deliver_input_answers_a_waiting_execution() {
    let root = TempRoot::new();
    let store = root.store();
    let engine = Arc::new(Engine::new(store.clone()));

    let definition = load_from_file(pipeline_definition_path()).expect("load pipeline fixture");
    let workspace_path = root.join("workspace");
    fs::create_dir_all(&workspace_path).expect("create workspace dir");
    let execution = engine
        .start(
            &definition,
            serde_json::json!({
                "script_path": prepare_script_path().display().to_string(),
                "ticket": "ORC-CLIENT-2",
            }),
            Workspace::local_folder(workspace_path.display().to_string()),
        )
        .expect("start execution");
    pin_definition_path(&store, &execution.id, &pipeline_definition_path());

    engine
        .run(&definition, &execution.id)
        .await
        .expect("run to the human gate");
    assert!(store.has_pending_input(&execution.id));

    let handle = start_server_with_engine(
        Arc::clone(&engine),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .await;
    let client = handle.client();

    let delivery = HumanInputDelivery::with_values(
        serde_json::json!({
            "approval_status": "approved",
            "comment": "looks good",
        })
        .as_object()
        .cloned()
        .expect("object"),
    );

    let response = client
        .deliver_input(&execution.id, delivery)
        .await
        .expect("client deliver_input");
    assert_eq!(response.execution_id, execution.id);

    wait_for_status(
        &store,
        &execution.id,
        ExecutionStatus::Completed,
        Duration::from_secs(5),
    )
    .await;
}

#[tokio::test]
async fn client_transition_moves_a_waiting_execution_to_the_declared_target() {
    let root = TempRoot::new();
    let store = root.store();
    let engine = Arc::new(Engine::new(store.clone()));

    let (definition, definition_path) = write_transition_fixture(&root);
    let workspace_path = root.join("workspace");
    fs::create_dir_all(&workspace_path).expect("create workspace dir");
    let mut execution = engine
        .start(
            &definition,
            serde_json::json!({}),
            Workspace::local_folder(workspace_path.display().to_string()),
        )
        .expect("start execution");
    execution.current_state = Some("waiting".to_string());
    execution.status = ExecutionStatus::Paused;
    store.save_execution(&execution).expect("save execution");
    pin_definition_path(&store, &execution.id, &definition_path);

    let handle = start_server_with_engine(
        Arc::clone(&engine),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .await;
    let client = handle.client();

    let summary = client
        .transition(
            &execution.id,
            TransitionRequest {
                label: Some("approve".to_string()),
                target: None,
                force: false,
            },
        )
        .await
        .expect("client transition");
    assert_eq!(summary.current_state.as_deref(), Some("done"));
    assert_eq!(summary.status, ExecutionStatus::Completed);
}

#[tokio::test]
async fn client_list_executions_reports_every_stored_execution() {
    let root = TempRoot::new();
    let store = root.store();
    seed_execution_with_retry(&store, "exec-list-1");
    let execution_two = sample_execution("exec-list-2", ExecutionStatus::Paused);
    store
        .save_execution(&execution_two)
        .expect("save execution two");

    let handle = start_server(store).await;
    let client = handle.client();

    let listing = client.list_executions().await.expect("list_executions");
    let ids: Vec<&str> = listing
        .executions
        .iter()
        .map(|execution| execution.id.as_str())
        .collect();
    assert!(ids.contains(&"exec-list-1"));
    assert!(ids.contains(&"exec-list-2"));
}

/// A `409` from a mutating endpoint maps to [`ClientError::Conflict`], not a
/// generic [`ClientError::Server`] — covers the plan's explicit "not a
/// generic Server error" test scenario, driven this time through the client
/// method itself rather than a raw request.
#[tokio::test]
async fn client_resume_on_an_already_running_execution_maps_to_conflict() {
    let root = TempRoot::new();
    let store = root.store();
    let engine = Arc::new(Engine::new(store.clone()));

    let script_path = root.join("slow.sh");
    fs::write(&script_path, "#!/bin/sh\nsleep 1\nexit 0\n").expect("write script");
    let mut permissions = fs::metadata(&script_path).expect("stat").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script_path, permissions).expect("chmod");

    let yaml = format!(
        r#"
name: api-client-conflict-fixture
version: 1
description: A slow script state, for the client-side 409-mapping test.

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
    let definition_path = root.join("api-client-conflict-fixture.yaml");
    fs::write(&definition_path, yaml).expect("write definition");
    let definition = load_from_file(&definition_path).expect("load definition");

    let workspace_path = root.join("workspace");
    fs::create_dir_all(&workspace_path).expect("create workspace dir");
    let execution = engine
        .start(
            &definition,
            serde_json::json!({}),
            Workspace::local_folder(workspace_path.display().to_string()),
        )
        .expect("start execution");
    engine
        .pause_now(&execution.id, "paused for the client 409 test")
        .expect("pause");
    pin_definition_path(&store, &execution.id, &definition_path);

    let handle = start_server_with_engine(
        Arc::clone(&engine),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .await;
    let client = handle.client();

    let (first, second) = tokio::join!(client.resume(&execution.id), client.resume(&execution.id));
    let results = [first, second];

    let accepted = results.iter().filter(|result| result.is_ok()).count();
    let conflicts = results
        .iter()
        .filter(|result| matches!(result, Err(ClientError::Conflict)))
        .count();
    assert_eq!(
        accepted, 1,
        "exactly one resume must be accepted: {results:?}"
    );
    assert_eq!(
        conflicts, 1,
        "exactly one resume must map to ClientError::Conflict: {results:?}"
    );

    wait_for_status(
        &store,
        &execution.id,
        ExecutionStatus::Completed,
        Duration::from_secs(5),
    )
    .await;
}

/// AE5 from the client's own `stream_execution`: it yields every event up to
/// and including the terminal one, and then ends on its own — `.next()`
/// returns `None` — without the caller having to inspect events itself to
/// know when to stop.
#[tokio::test]
async fn client_stream_execution_ends_itself_after_the_terminal_event() {
    let root = TempRoot::new();
    let store = root.store();
    let engine = Arc::new(Engine::new(store.clone()));

    let (definition, _definition_path) = write_script_to_done_workflow(&root);
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
    let client = handle.client();

    let stream = client.stream_execution(&execution.id);
    tokio::pin!(stream);

    // Give the stream a moment to actually connect and start polling before
    // the run happens, mirroring `tests/sse.rs`'s own pattern.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let engine_for_run = Arc::clone(&engine);
    let definition_for_run = definition.clone();
    let execution_id = execution.id.clone();
    let run_task = tokio::spawn(async move {
        engine_for_run
            .run(&definition_for_run, &execution_id)
            .await
            .expect("run to completion")
    });

    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("stream_execution did not end within the deadline: {events:?}");
        }
        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(item)) => events.push(item.expect("stream item must not be an error")),
            Ok(None) => break,
            Err(_) => panic!("timed out waiting for the next stream item: {events:?}"),
        }
    }

    run_task.await.expect("background run task");

    assert!(
        !events.is_empty(),
        "the stream must have yielded at least the terminal event"
    );
    assert!(matches!(
        events.last().expect("at least one event").data,
        narsor::domain::EventData::WorkflowCompleted { .. }
    ));
    assert!(events
        .iter()
        .all(|event| event.workflow_execution_id == execution.id));
}

/// A script-to-terminal fixture whose one action sleeps for a bit before
/// succeeding — long enough that a test can observe it still `Running` while
/// a sibling execution using [`write_script_to_done_workflow`] has already
/// reached a terminal state.
fn write_slow_script_workflow(
    root: &TempRoot,
    sleep_seconds: u64,
) -> (WorkflowDefinition, PathBuf) {
    let script_path = root.join(&format!("slow-{sleep_seconds}.sh"));
    fs::write(
        &script_path,
        format!("#!/bin/sh\nsleep {sleep_seconds}\nexit 0\n"),
    )
    .expect("write script");
    let mut permissions = fs::metadata(&script_path)
        .expect("stat script")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script_path, permissions).expect("chmod script");

    let yaml = format!(
        r#"
name: api-slow-fixture
version: 1
description: A single script action that sleeps before succeeding.

states:
  start:
    description: Runs a script that sleeps, then succeeds.
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

    let path = root.join(&format!("api-slow-fixture-{sleep_seconds}.yaml"));
    fs::write(&path, yaml).expect("write definition");
    let definition = load_from_file(&path).expect("load definition");
    (definition, path)
}

/// Regression test for a real bug found in code review (U8 follow-up): the
/// SSE parser both [`ApiClient::stream_execution`] and
/// [`ApiClient::stream_all`] share used to set `done = true` on *any*
/// terminal event, with no check that it belonged to the execution the
/// stream cared about. `stream_execution` never noticed, since it only ever
/// watches one execution — but `stream_all`, which by contract "keeps
/// running until the connection itself ends," would silently end the moment
/// the *first* of several concurrently running executions finished. A TUI
/// relying on `stream_all` for its whole live-update lifetime would then
/// falsely report the connection as dropped the first time any workflow
/// completed while another was still in flight.
///
/// This drives two executions concurrently — one that finishes almost
/// immediately, one that is still sleeping — and asserts `stream_all` keeps
/// yielding events (in particular, the *second* execution's own terminal
/// event) well after the first execution's terminal event has already gone
/// by.
#[tokio::test]
async fn client_stream_all_does_not_end_when_only_one_of_several_executions_finishes() {
    let root = TempRoot::new();
    let store = root.store();
    let engine = Arc::new(Engine::new(store.clone()));

    let (fast_definition, _fast_path) = write_script_to_done_workflow(&root);
    let (slow_definition, _slow_path) = write_slow_script_workflow(&root, 2);
    let fast_workspace_path = root.join("workspace-fast");
    let slow_workspace_path = root.join("workspace-slow");
    fs::create_dir_all(&fast_workspace_path).expect("create fast workspace dir");
    fs::create_dir_all(&slow_workspace_path).expect("create slow workspace dir");

    let fast_execution = engine
        .start(
            &fast_definition,
            serde_json::json!({}),
            Workspace::local_folder(fast_workspace_path.display().to_string()),
        )
        .expect("start fast execution");
    let slow_execution = engine
        .start(
            &slow_definition,
            serde_json::json!({}),
            Workspace::local_folder(slow_workspace_path.display().to_string()),
        )
        .expect("start slow execution");

    let handle = start_server_with_engine(
        Arc::clone(&engine),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .await;
    let client = handle.client();

    // `stream_all()`'s connection is lazy — it only opens on the first
    // `.next()` poll (see `src/tui/mod.rs`'s module doc on this same gap for
    // `App`'s own event loop). Polling it once, in its own task, before
    // either run is spawned is what actually opens the SSE connection early
    // enough to see the fast execution's own terminal event; the sleep
    // afterwards is a generous margin for the connection to be fully
    // established server-side, not a race the test depends on being exact.
    let stream = client.stream_all();
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    let stream_task = tokio::spawn(async move {
        tokio::pin!(stream);
        while let Some(item) = stream.next().await {
            if events_tx.send(item).is_err() {
                break;
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let engine_for_fast = Arc::clone(&engine);
    let fast_definition_for_run = fast_definition.clone();
    let fast_id = fast_execution.id.clone();
    let fast_task = tokio::spawn(async move {
        engine_for_fast
            .run(&fast_definition_for_run, &fast_id)
            .await
            .expect("fast run to completion")
    });

    let engine_for_slow = Arc::clone(&engine);
    let slow_definition_for_run = slow_definition.clone();
    let slow_id = slow_execution.id.clone();
    let slow_task = tokio::spawn(async move {
        engine_for_slow
            .run(&slow_definition_for_run, &slow_id)
            .await
            .expect("slow run to completion")
    });

    let mut saw_fast_terminal = false;
    let mut saw_slow_terminal = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !saw_slow_terminal {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "stream_all ended or stalled before the slow execution's terminal event \
             (saw_fast_terminal={saw_fast_terminal})"
        );
        let item = match tokio::time::timeout(remaining, events_rx.recv()).await {
            Ok(Some(item)) => item.expect("stream item must not be an error"),
            Ok(None) => panic!(
                "stream_all ended after the fast execution finished, before the slow one did \
                 (saw_fast_terminal={saw_fast_terminal}) — this is exactly the bug this test \
                 guards against"
            ),
            Err(_) => panic!("timed out waiting for the next stream_all item"),
        };
        if item.workflow_execution_id == fast_execution.id
            && matches!(
                item.data,
                narsor::domain::EventData::WorkflowCompleted { .. }
            )
        {
            saw_fast_terminal = true;
        }
        if item.workflow_execution_id == slow_execution.id
            && matches!(
                item.data,
                narsor::domain::EventData::WorkflowCompleted { .. }
            )
        {
            saw_slow_terminal = true;
        }
    }

    fast_task.await.expect("fast run task");
    slow_task.await.expect("slow run task");
    stream_task.abort();
    assert!(
        saw_fast_terminal,
        "must have observed the fast execution's terminal event too"
    );
}
