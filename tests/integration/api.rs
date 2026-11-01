//! Integration tests for the embedded API server (U5): the server, over a
//! real temporary store and a real bound loopback port, answering the three
//! read-only endpoints through the real `ApiClient`.

use crate::environment_support;

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use narsor_engine::conductor::assembly::assemble;
use narsor_engine::conductor::{
    ChatAuthor, ChatEntry, ChatEntryKind, ConductorState, ConductorStore, PendingInvocation,
    QueuedRequest,
};
use narsor_engine::domain::{
    Context, Event, EventData, ExecutionStatus, RepositoryInfo, WorkflowDefinition,
    WorkflowExecution, Workspace,
};
use narsor_engine::engine::actions::human::{
    self as human, HumanInputRequest, PendingHumanInput, PermissionDenialDetails,
    PermissionRequestDetails,
};
use narsor_engine::engine::{ConversationState, Engine, HumanInputDelivery, PositionStatus};
use narsor_engine::persistence::conversation::ConversationSource;
use narsor_engine::persistence::{DefaultHarnessSelector, ExecutionStore, StatePaths};
use narsor_engine::workflow::load_from_file;
use narsor_protocol::api::HumanInputDeliveryDto;
use narsor_protocol::chat::{ChatElement, TechnicalCategory, UnclassifiedReason};
use narsor_server::api;
use narsor_server::api::{ClientError, ExecutionSummary, StartTaskResponse};

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

fn wire_delivery(delivery: HumanInputDelivery) -> HumanInputDeliveryDto {
    HumanInputDeliveryDto {
        values: delivery.values,
        chosen_label: delivery.chosen_label,
        artifacts: delivery.artifacts,
        variables: delivery.variables,
        comment: delivery.comment,
        agent_message: delivery.agent_message,
        action: delivery.action,
        episode: delivery.episode,
        permission_decision: delivery.permission_decision.map(|decision| {
            narsor_protocol::api::PermissionDecisionDto {
                denial_index: decision.denial_index,
                decision: match decision.decision {
                    narsor_engine::engine::PermissionDecisionKind::Deny => {
                        narsor_protocol::api::PermissionDecisionKindDto::Deny
                    }
                    narsor_engine::engine::PermissionDecisionKind::Allow => {
                        narsor_protocol::api::PermissionDecisionKindDto::Allow
                    }
                },
                scope: decision.scope.map(|scope| match scope {
                    narsor_engine::engine::PermissionGrantScope::ExactOperation => {
                        narsor_protocol::api::PermissionGrantScopeDto::ExactOperation
                    }
                    narsor_engine::engine::PermissionGrantScope::CommandGroup => {
                        narsor_protocol::api::PermissionGrantScopeDto::CommandGroup
                    }
                    narsor_engine::engine::PermissionGrantScope::Pattern => {
                        narsor_protocol::api::PermissionGrantScopeDto::Pattern
                    }
                }),
                pattern: decision.pattern.clone(),
                message: decision.message.clone(),
            }
        }),
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
            reason: None,
            additional_data: Default::default(),
            warnings: Vec::new(),
        },
    )
    .expect("append ActionCompleted");

    let conversations = store.conversation_store(execution_id);
    let mut first = conversations.writer("attempt-1").expect("open writer 1");
    first
        .append_narsor("prompt: try the thing", &[])
        .expect("append prompt 1");
    first
        .append(ConversationSource::Stdout, br#"{"type":"error"}"#, &[])
        .expect("append stdout 1");
    first.finish().expect("finish writer 1");
    conversations
        .close_attempt("attempt-1")
        .expect("close attempt 1");

    let mut second = conversations.writer("attempt-2").expect("open writer 2");
    second
        .append_narsor("prompt: try again", &[])
        .expect("append prompt 2");
    second
        .append(ConversationSource::Stdout, br#"{"type":"assistant"}"#, &[])
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
            vec![
                "claude-code".into(),
                "codex".into(),
                "github-copilot".into(),
                "opencode".into(),
            ],
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
            vec![
                "claude-code".into(),
                "codex".into(),
                "github-copilot".into(),
                "opencode".into(),
            ],
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
    choices:
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
    choices:
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
    writer
        .append_narsor("prompt: go", &[])
        .expect("append prompt");
    writer
        .append(ConversationSource::Stdout, b"line one", &[])
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
        .append(ConversationSource::Stdout, b"line two", &[])
        .expect("append line two");
    writer
        .append_narsor("exit code: 0", &[])
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
        .append(ConversationSource::Stdout, &huge, &[])
        .expect("append huge entry");
    writer
        .append(ConversationSource::Stdout, b"small", &[])
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

/// A record written before elements existed still reads, and every entry
/// reaches the client with an element: stdout and narsor lines as
/// not-recorded, stderr as what it is.
#[tokio::test]
async fn conversation_endpoint_gives_old_entries_a_stand_in_element() {
    let root = TempRoot::new();
    let store = root.store();
    let execution = sample_execution("exec-old", ExecutionStatus::Completed);
    store.save_execution(&execution).expect("save execution");

    let conversations = store.conversation_store("exec-old");
    fs::create_dir_all(conversations.dir()).expect("create the directory");
    fs::write(
        conversations.dir().join("attempt-old.jsonl"),
        concat!(
            r#"{"n":1,"ts":"2026-01-02T03:04:05Z","source":"narsor","raw_b64":"cHJvbXB0"}"#,
            "\n",
            r#"{"n":2,"ts":"2026-01-02T03:04:06Z","source":"stdout","raw_b64":"e30="}"#,
            "\n",
            r#"{"n":3,"ts":"2026-01-02T03:04:07Z","source":"stderr","raw_b64":"b29wcw=="}"#,
            "\n",
        ),
    )
    .expect("write an old record");

    let handle = start_server(store).await;
    let response = handle
        .client()
        .conversation("exec-old", "attempt-old", 0, 1 << 20)
        .await
        .expect("conversation request");

    let narsor::api::ConversationResponse::Recorded { entries, .. } = response else {
        panic!("expected a record");
    };
    let not_recorded = vec![ChatElement::Unclassified {
        reason: UnclassifiedReason::NotRecorded,
    }];
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].elements, not_recorded);
    assert_eq!(entries[1].elements, not_recorded);
    assert_eq!(
        entries[2].elements,
        [ChatElement::Technical {
            category: TechnicalCategory::Stderr,
            summary: "oops".to_string(),
            data: None,
        }]
    );
}

/// Elements reach the client as stored, and the page budget counts them:
/// a page of tiny raw lines with bulky elements ends early, and its cursor
/// continues at the next entry.
#[tokio::test]
async fn conversation_endpoint_pages_by_raw_bytes_and_elements() {
    let root = TempRoot::new();
    let store = root.store();
    let execution = sample_execution("exec-elements", ExecutionStatus::Completed);
    store.save_execution(&execution).expect("save execution");

    let bulky = |n: usize| {
        vec![ChatElement::AgentText {
            text: format!("{n}").repeat(300),
        }]
    };
    let conversations = store.conversation_store("exec-elements");
    let mut writer = conversations.writer("attempt-1").expect("open writer");
    for n in 1..=3 {
        writer
            .append(ConversationSource::Stdout, b"{}", &bulky(n))
            .expect("append");
    }
    writer.finish().expect("finish writer");

    let handle = start_server(store).await;
    let client = handle.client();

    let page = |response: narsor::api::ConversationResponse| match response {
        narsor::api::ConversationResponse::Recorded {
            entries,
            next_entry,
            ..
        } => (entries, next_entry),
        narsor::api::ConversationResponse::NoRecord => panic!("expected a record"),
    };
    // 6 raw bytes in all, but over 900 bytes of elements: each stored
    // line is over 300 bytes, so two of them spend a 600-byte budget.
    let (first, next) = page(
        client
            .conversation("exec-elements", "attempt-1", 0, 600)
            .await
            .expect("first page"),
    );
    assert_eq!(first.len(), 2, "the budget is spent after two entries");
    assert_eq!(first[0].elements, bulky(1));
    assert_eq!(first[1].elements, bulky(2));
    assert_eq!(next, 3);

    let (rest, next) = page(
        client
            .conversation("exec-elements", "attempt-1", next, 600)
            .await
            .expect("second page"),
    );
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].entry_number, 3);
    assert_eq!(rest[0].elements, bulky(3));
    assert_eq!(next, 4);
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

/// An execution the conductor never worked on has an empty conductor state
/// and an empty chat — not an error (U8).
#[tokio::test]
async fn conductor_endpoints_answer_an_old_execution_with_an_empty_state() {
    let root = TempRoot::new();
    let store = root.store();
    store
        .save_execution(&sample_execution("exec-old", ExecutionStatus::Completed))
        .expect("save execution");

    let handle = start_server(store).await;
    let client = handle.client();

    let state = client
        .conductor_state("exec-old")
        .await
        .expect("conductor state");
    assert_eq!(state, api::ConductorStateResponse::default());
    let chat = client.conductor_chat("exec-old", 0).await.expect("chat");
    assert!(chat.entries.is_empty());

    assert!(matches!(
        client.conductor_state("does-not-exist").await,
        Err(ClientError::NotFound)
    ));
    assert!(matches!(
        client.conductor_chat("does-not-exist", 0).await,
        Err(ClientError::NotFound)
    ));
}

/// The conductor's state and chat history are read from its store: what is
/// under way, what waits for an answer, and the chat after a sequence.
#[tokio::test]
async fn conductor_endpoints_return_the_state_and_the_chat_after_a_sequence() {
    let root = TempRoot::new();
    let store = root.store();
    store
        .save_execution(&sample_execution("exec-chat", ExecutionStatus::Running))
        .expect("save execution");
    let conductor = ConductorStore::new(store.paths(), "exec-chat");
    let answered = conductor
        .append_chat(ChatEntry::new(
            ChatAuthor::Operator,
            ChatEntryKind::Message,
            "first",
        ))
        .expect("append");
    let reply = conductor
        .append_chat(
            ChatEntry::new(ChatAuthor::Conductor, ChatEntryKind::Message, "noted")
                .replying_to(&answered.id),
        )
        .expect("append");
    let open = conductor
        .append_chat(ChatEntry::new(
            ChatAuthor::Operator,
            ChatEntryKind::Message,
            "second",
        ))
        .expect("append");
    let request = QueuedRequest::for_chat(&open);
    let mut state = ConductorState {
        pending: Some(PendingInvocation {
            invocation_id: "invocation-1".to_string(),
            request,
            started_at: Utc::now(),
        }),
        ..ConductorState::default()
    };
    state.used_retries.insert("episode-1".to_string());
    conductor.save_state(&state).expect("save the state");

    let handle = start_server(store).await;
    let client = handle.client();

    let response = client
        .conductor_state("exec-chat")
        .await
        .expect("conductor state");
    assert_eq!(
        serde_json::to_value(response.pending).expect("serialize API pending invocation"),
        serde_json::to_value(state.pending).expect("serialize stored pending invocation")
    );
    assert_eq!(response.unanswered_chat, vec![open.id.clone()]);
    assert!(response.used_retries.contains("episode-1"));

    let chat = client.conductor_chat("exec-chat", 1).await.expect("chat");
    assert_eq!(chat.entries, vec![reply.into(), open.into()]);
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

/// U7: trusted mode answers who the caller is only with the process token,
/// and has no owner to claim.
#[tokio::test]
async fn trusted_mode_names_itself_to_the_token_holder_and_has_nothing_to_claim() {
    let root = TempRoot::new();
    let handle = start_server(root.store()).await;
    let raw = reqwest::Client::new();

    let whoami: serde_json::Value = raw
        .get(url(&handle, "/auth/v1/whoami"))
        .bearer_auth(handle.token())
        .send()
        .await
        .expect("whoami")
        .json()
        .await
        .expect("json");
    assert_eq!(whoami["mode"], "trusted");
    assert_eq!(whoami["owner"], true);

    let stranger = raw
        .get(url(&handle, "/auth/v1/whoami"))
        .bearer_auth("not-the-token")
        .send()
        .await
        .expect("whoami");
    assert_eq!(stranger.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert!(stranger.text().await.unwrap().is_empty());

    let claim = raw
        .post(url(&handle, "/auth/v1/claim"))
        .bearer_auth(handle.token())
        .json(&serde_json::json!({ "claim_code": "nsclaim-x" }))
        .send()
        .await
        .expect("claim");
    assert_eq!(claim.status(), reqwest::StatusCode::NOT_FOUND);
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
async fn post_tasks_creates_a_queued_task_without_an_execution() {
    let root = TempRoot::new();
    let (handle, service) = start_scheduling_server(&root).await;
    let raw = raw_client();

    let body = serde_json::json!({
        "task_id": "local-create-contract",
        "workflow": "implement-feature",
        "inputs": {}
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
    assert_eq!(parsed.task.task_id.as_str(), "local-create-contract");
    assert!(root.store().list_execution_ids().unwrap().is_empty());
    assert_eq!(service.control_plane().list_tasks().unwrap().len(), 1);
}

/// A task holds its workflow definition snapshot before a runner exists.
#[tokio::test]
async fn post_tasks_seals_a_resolvable_definition_before_admission() {
    let root = TempRoot::new();
    let (handle, service) = start_scheduling_server(&root).await;
    let raw = raw_client();

    let body = serde_json::json!({
        "task_id": "local-sealed-definition",
        "workflow": "implement-feature",
        "inputs": {}
    });

    let response = raw
        .post(url(&handle, "/tasks"))
        .bearer_auth(handle.token())
        .json(&body)
        .send()
        .await
        .expect("post /tasks");
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    let task_id = response
        .json::<StartTaskResponse>()
        .await
        .expect("parse start-task response")
        .task
        .task_id;
    let record = service.control_plane().task(&task_id).unwrap();
    assert_eq!(
        record.launch_spec.definition().unwrap().id,
        "implement-feature"
    );
    assert!(root.store().list_execution_ids().unwrap().is_empty());
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
async fn ordinary_input_cannot_resolve_permission_and_unapprovable_denial_stays_pending() {
    let root = TempRoot::new();
    let store = root.store();
    let mut execution = sample_execution("permission-api", ExecutionStatus::Running);
    execution.current_state = Some("review".to_string());
    store.save_execution(&execution).expect("save execution");
    pin_definition_path(&store, &execution.id, &pipeline_definition_path());
    execution = store.load_execution(&execution.id).expect("reload");
    let request = HumanInputRequest {
        action_execution_id: "attempt-permission".to_string(),
        instruction: "Claude Code needs a permission decision.".to_string(),
        fields: Vec::new(),
        reason: None,
        required_values: Vec::new(),
        answer_tail: None,
        agent_harness: Some("claude-code".to_string()),
        agent_session_id: Some("session-permission".to_string()),
        prefilled_answer: None,
        permission: Some(PermissionRequestDetails {
            denials: vec![PermissionDenialDetails {
                tool_name: "Bash".to_string(),
                tool_use_id: "tool-permission".to_string(),
                input: serde_json::json!({"command":"cargo build"}),
                cwd: "/tmp/task".to_string(),
                reason_type: Some("other".to_string()),
                reason: Some("A policy rule blocked this action.".to_string()),
            }],
        }),
    };
    let pending = PendingHumanInput::for_permission(execution.id.clone(), "review", request);
    human::suspend(&store, &mut execution, &pending).expect("persist permission wait");
    let server = start_server_with_engine(
        Arc::new(Engine::new(store.clone())),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .await;
    let client = server.client();
    let pending = store
        .load_pending_input::<PendingHumanInput>(&execution.id)
        .expect("load pending")
        .expect("permission wait");
    let decision = narsor_protocol::api::PermissionDecisionDto {
        denial_index: Some(0),
        decision: narsor_protocol::api::PermissionDecisionKindDto::Allow,
        scope: Some(narsor_protocol::api::PermissionGrantScopeDto::ExactOperation),
        pattern: None,
        message: None,
    };
    assert!(client
        .resolve_permission(
            &execution.id,
            narsor_protocol::api::PermissionDecisionRequestDto {
                episode: pending.episode_key().to_string(),
                permission_decision: decision.clone(),
            }
        )
        .await
        .is_err());
    assert!(client
        .deliver_input(
            &execution.id,
            HumanInputDeliveryDto {
                episode: Some(pending.episode_key().to_string()),
                permission_decision: Some(decision),
                ..HumanInputDeliveryDto::default()
            }
        )
        .await
        .is_err());
    assert!(store.has_pending_input(&execution.id));
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
    assert_eq!(summary.status, narsor::api::ExecutionStatusDto::Completed);

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
    choices:
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
    choices:
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
async fn client_start_task_returns_a_queued_task() {
    let root = TempRoot::new();
    let (handle, service) = start_scheduling_server(&root).await;
    let client = handle.client();

    let response = client
        .start_task("task-client-start", "implement-feature", Default::default())
        .await
        .expect("start_task");
    assert_eq!(
        response.task.runner_state,
        narsor_engine::orchestration::RunnerState::Queued
    );
    assert!(root.store().list_execution_ids().unwrap().is_empty());
    assert_eq!(service.control_plane().list_tasks().unwrap().len(), 1);
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
        .deliver_input(&execution.id, wire_delivery(delivery))
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
    assert_eq!(summary.status, narsor::api::ExecutionStatusDto::Completed);
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

/// A server without a control plane has no owner-scoped task projection, so
/// runner storage must not become visible merely because a directory exists.
/// It continues to serve only its historic root-store execution.
#[tokio::test]
async fn runner_storage_is_not_discovered_without_a_control_plane_projection() {
    let root = TempRoot::new();
    let legacy = root.store();
    seed_execution_with_retry(&legacy, "exec-collision");
    let runner = ExecutionStore::new(root.join("local-controller/storage/task-other/state"));
    seed_execution_with_retry(&runner, "exec-collision");

    let handle = start_server(legacy).await;
    let client = handle.client();

    let listing = client.list_executions().await.expect("list executions");
    assert_eq!(
        listing
            .executions
            .iter()
            .filter(|execution| execution.id == "exec-collision")
            .count(),
        1,
        "the historic root execution remains visible"
    );
    assert!(client.events("exec-collision", 0).await.is_ok());
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
    choices:
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
        narsor_engine::domain::EventData::WorkflowCompleted { .. }
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
    choices:
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
                narsor_engine::domain::EventData::WorkflowCompleted { .. }
            )
        {
            saw_fast_terminal = true;
        }
        if item.workflow_execution_id == slow_execution.id
            && matches!(
                item.data,
                narsor_engine::domain::EventData::WorkflowCompleted { .. }
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

// ---------------------------------------------------------------------------
// A manual transition carries the agent's reason to the gate after it (U4)
// ---------------------------------------------------------------------------

use narsor_engine::domain::{ActionError, ActionResult};
use narsor_engine::engine::{
    ActionExecutor, ActionKind, ActionOutcome, ActionRequest, SuspendedAction,
};

/// An agent that either finishes with a reason and a value its choice
/// requires, or stops to ask a question of its own (`awaiting_input`).
enum AgentReply {
    Finish,
    Ask,
}

impl ActionExecutor for AgentReply {
    fn execute<'a>(
        &'a self,
        request: ActionRequest<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ActionOutcome, ActionError>> + Send + 'a>,
    > {
        let action_execution_id = request.action_execution_id.to_string();
        Box::pin(async move {
            Ok(match self {
                Self::Finish => ActionOutcome::Completed(ActionResult {
                    output_label: "done".to_string(),
                    reason: "the limit scope is open".to_string(),
                    variables: serde_json::Map::from_iter([(
                        "limit_scope".to_string(),
                        serde_json::json!("account or user?"),
                    )]),
                    exit_code: Some(0),
                    stdout: "the agent's answer".to_string(),
                    reply: Some("the agent's answer".to_string()),
                    ..Default::default()
                }),
                Self::Ask => ActionOutcome::Suspended(SuspendedAction::AwaitingAgentInput(
                    HumanInputRequest {
                        action_execution_id,
                        instruction: "Which API version?".to_string(),
                        fields: Vec::new(),
                        reason: None,
                        required_values: Vec::new(),
                        answer_tail: None,
                        agent_harness: None,
                        agent_session_id: None,
                        prefilled_answer: None,
                        permission: None,
                    },
                )),
            })
        })
    }
}

/// Runs an agent state up to where a person has to act, moves it by hand to
/// the `resolve` gate and runs the gate; returns the gate's pending request.
async fn manual_transition_to_gate(reply: AgentReply) -> HumanInputRequest {
    let root = TempRoot::new();
    let store = root.store();
    let engine =
        Arc::new(Engine::new(store.clone()).with_executor(ActionKind::Agent, Arc::new(reply)));

    let yaml = r#"
name: api-agent-gate-fixture
version: 1
states:
  work:
    action:
      type: agent
      prompt: Do the work.
    variables:
      - { name: limit_scope, description: Which limit the work applies. }
    choices:
      - label: done
        requires_variables: [limit_scope]
      - needs_human_decision
    transitions:
      done: finished
      needs_human_decision: resolve
    decision:
      type: human
      message: Accept the work?
  resolve:
    action:
      type: human
      message: Decide.
    choices:
      - go
    transitions:
      go: finished
  finished:
    description: Done.
terminal_states:
  finished: completed
"#;
    let definition_path = root.join("api-agent-gate-fixture.yaml");
    fs::write(&definition_path, yaml).expect("write definition");
    let definition = load_from_file(&definition_path).expect("load definition");

    let workspace_path = root.join("workspace");
    fs::create_dir_all(&workspace_path).expect("create workspace dir");
    let execution = engine
        .start_and_run(
            &definition,
            serde_json::json!({}),
            Workspace::local_folder(workspace_path.display().to_string()),
        )
        .await
        .expect("run up to the person");
    assert_eq!(execution.status, ExecutionStatus::InputNeeded);
    assert_eq!(execution.current_state.as_deref(), Some("work"));
    // As `pin_definition_path` does, but keeping the engine's own metadata:
    // the finished agent's answer waits there for the transition.
    let mut execution = store.load_execution(&execution.id).expect("reload");
    execution.metadata["cli"] =
        serde_json::json!({ "definition_path": definition_path.display().to_string() });
    store
        .save_execution(&execution)
        .expect("pin definition path");

    let handle = start_server_with_engine(
        Arc::clone(&engine),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
    )
    .await;
    let response = raw_client()
        .post(url(
            &handle,
            &format!("/executions/{}/transition", execution.id),
        ))
        .bearer_auth(handle.token())
        .json(&TransitionRequestBody {
            label: Some("needs_human_decision".to_string()),
            target: None,
            force: false,
        })
        .send()
        .await
        .expect("post transition");
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let execution = engine
        .resume(&definition, &execution.id)
        .await
        .expect("run the gate");
    assert_eq!(execution.status, ExecutionStatus::InputNeeded);
    assert_eq!(execution.current_state.as_deref(), Some("resolve"));
    engine
        .pending_input(&execution.id)
        .expect("read pending input")
        .expect("the gate is pending")
        .request
}

#[tokio::test]
async fn post_transition_from_a_finished_agent_carries_its_reason() {
    let request = manual_transition_to_gate(AgentReply::Finish).await;

    assert_eq!(request.reason.as_deref(), Some("the limit scope is open"));
    assert_eq!(
        request.required_values,
        vec![narsor_engine::engine::actions::human::RequiredValue {
            name: "limit_scope".to_string(),
            kind: narsor_engine::engine::actions::human::RequiredValueKind::Variable,
            value: "account or user?".to_string(),
        }]
    );
    assert_eq!(request.answer_tail.as_deref(), Some("the agent's answer"));
}

#[tokio::test]
async fn post_transition_from_an_asking_agent_carries_its_question_as_the_reason() {
    let request = manual_transition_to_gate(AgentReply::Ask).await;

    assert_eq!(request.reason.as_deref(), Some("Which API version?"));
    assert!(request.required_values.is_empty());
    assert_eq!(request.answer_tail, None);
}

// ---------------------------------------------------------------------------
// Conductor plan U5: problems over `POST /input`, stale answers, closing
// ---------------------------------------------------------------------------

use crate::engine_support;

mod problems {
    use super::*;

    use narsor_engine::domain::{AnsweredBy, InputCloseReason};
    use narsor_engine::engine::{ActionKind, InputContinuation, PendingHumanInput};

    /// An execution of `agent_problem` stopped on its problem, and a server
    /// over the engine that stopped it.
    struct Problem {
        _root: TempRoot,
        store: ExecutionStore,
        handle: api::ApiHandle,
        execution_id: String,
        pending: PendingHumanInput,
    }

    async fn problem_over_http() -> Problem {
        let root = TempRoot::new();
        let store = root.store();
        let engine = Arc::new(Engine::new(store.clone()).with_executor(
            ActionKind::Agent,
            Arc::new(engine_support::LabellessAgentExecutor::answering(
                "Done, all tests pass.",
            )),
        ));
        let definition_path = engine_support::fixtures().join("engine/agent_problem.yaml");
        let definition = load_from_file(&definition_path).expect("load the fixture");

        let workspace_path = root.join("workspace");
        fs::create_dir_all(&workspace_path).expect("create workspace dir");
        fs::copy(
            engine_support::fixtures().join("scripts/print_json.sh"),
            workspace_path.join("print_json.sh"),
        )
        .expect("copy the fixture script");

        let execution = engine
            .start(
                &definition,
                serde_json::json!({}),
                Workspace::local_folder(workspace_path.display().to_string()),
            )
            .expect("start execution");
        pin_definition_path(&store, &execution.id, &definition_path);
        engine
            .run(&definition, &execution.id)
            .await
            .expect("run to the problem");
        let pending: PendingHumanInput = store
            .load_pending_input(&execution.id)
            .expect("read")
            .expect("the problem is pending");
        assert_eq!(pending.continuation, InputContinuation::ResolveOutput);

        let handle = start_server_with_engine(
            Arc::clone(&engine),
            root.path().to_path_buf(),
            root.path().to_path_buf(),
        )
        .await;
        Problem {
            _root: root,
            store,
            handle,
            execution_id: execution.id,
            pending,
        }
    }

    fn post_input(
        problem: &Problem,
        body: &serde_json::Value,
    ) -> impl std::future::Future<Output = reqwest::Result<reqwest::Response>> {
        raw_client()
            .post(url(
                &problem.handle,
                &format!("/executions/{}/input", problem.execution_id),
            ))
            .bearer_auth(problem.handle.token())
            .json(body)
            .send()
    }

    fn events(problem: &Problem) -> Vec<Event> {
        problem
            .store
            .event_log(&problem.execution_id)
            .read_all()
            .expect("read events")
    }

    #[tokio::test]
    async fn post_input_resolves_a_problem_with_label_artifacts_and_variables() {
        let problem = problem_over_http().await;

        let response = post_input(
            &problem,
            &serde_json::json!({
                "chosen_label": "implemented",
                "artifacts": { "report": "docs/report.md" },
                "variables": { "summary": "login works" },
                "episode": problem.pending.episode_key(),
                // Not on the wire: whoever calls the API is the operator.
                "answered_by": "conductor",
            }),
        )
        .await
        .expect("post input");
        assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);

        wait_for_status(
            &problem.store,
            &problem.execution_id,
            ExecutionStatus::Completed,
            Duration::from_secs(5),
        )
        .await;
        let events = events(&problem);
        assert!(events.iter().any(|event| matches!(
            &event.data,
            EventData::DecisionMade { provider, output_label, .. }
                if provider == "human" && output_label == "implemented"
        )));
        assert!(events.iter().any(|event| matches!(
            &event.data,
            EventData::ArtifactCreated { location, .. } if location == "docs/report.md"
        )));
        assert!(events.iter().any(|event| matches!(
            &event.data,
            EventData::HumanInputReceived {
                answered_by: Some(AnsweredBy::Operator),
                ..
            }
        )));
    }

    #[tokio::test]
    async fn a_problem_answer_missing_a_required_key_is_a_bad_request_naming_it() {
        let problem = problem_over_http().await;
        let client = api::ApiClient::new(problem.handle.addr(), problem.handle.token());
        let before = events(&problem).len();

        let error = client
            .deliver_input(
                &problem.execution_id,
                wire_delivery(
                    HumanInputDelivery::label("implemented")
                        .for_episode(problem.pending.episode_key()),
                ),
            )
            .await
            .expect_err("implemented needs variables.summary");
        match error {
            ClientError::Request { code, message } => {
                assert_eq!(code, "invalid_input");
                assert!(message.contains("variables.summary"), "{message}");
            }
            other => panic!("expected a 400 with a reason, got {other:?}"),
        }

        let execution = problem
            .store
            .load_execution(&problem.execution_id)
            .expect("reload");
        assert_eq!(execution.status, ExecutionStatus::InputNeeded);
        assert!(problem.store.has_pending_input(&problem.execution_id));
        // Only the error note the engine records for any refused call.
        assert!(events(&problem)[before..]
            .iter()
            .all(|event| matches!(event.data, EventData::Error { .. })));
    }

    #[tokio::test]
    async fn an_answer_with_a_stale_episode_is_a_conflict_and_changes_nothing() {
        let problem = problem_over_http().await;

        let response = post_input(
            &problem,
            &serde_json::json!({
                "chosen_label": "implemented",
                "variables": { "summary": "x" },
                "episode": "a-request-that-is-gone",
            }),
        )
        .await
        .expect("post input");
        assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);

        let execution = problem
            .store
            .load_execution(&problem.execution_id)
            .expect("reload");
        assert_eq!(execution.status, ExecutionStatus::InputNeeded);
        let still: PendingHumanInput = problem
            .store
            .load_pending_input(&problem.execution_id)
            .expect("read")
            .expect("still pending");
        assert_eq!(still.episode_key(), problem.pending.episode_key());
        assert!(!events(&problem)
            .iter()
            .any(|event| matches!(event.data, EventData::HumanInputReceived { .. })));
    }

    /// Two answers to the same request at once: exactly one is taken. The
    /// other either finds the execution claimed or, once the first has moved
    /// it on to the gate, finds a newer request than the one it answers.
    #[tokio::test]
    async fn two_concurrent_answers_to_one_problem_yield_exactly_one_accepted() {
        let problem = problem_over_http().await;
        let body = serde_json::json!({
            "chosen_label": "needs_human_decision",
            "episode": problem.pending.episode_key(),
        });

        let (first, second) =
            tokio::join!(post_input(&problem, &body), post_input(&problem, &body));
        let statuses = [
            first.expect("first answer").status(),
            second.expect("second answer").status(),
        ];
        let accepted = statuses
            .iter()
            .filter(|status| **status == reqwest::StatusCode::ACCEPTED)
            .count();
        let conflicts = statuses
            .iter()
            .filter(|status| **status == reqwest::StatusCode::CONFLICT)
            .count();
        assert_eq!((accepted, conflicts), (1, 1), "{statuses:?}");

        let execution = wait_for_status(
            &problem.store,
            &problem.execution_id,
            ExecutionStatus::InputNeeded,
            Duration::from_secs(5),
        )
        .await;
        // Settled on the gate, answered once.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while problem
            .store
            .load_execution(&problem.execution_id)
            .expect("reload")
            .current_state
            .as_deref()
            != Some("resolve_ambiguity")
        {
            assert!(tokio::time::Instant::now() < deadline, "{execution:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let answers = events(&problem)
            .iter()
            .filter(|event| matches!(event.data, EventData::HumanInputReceived { .. }))
            .count();
        assert_eq!(answers, 1);

        // A late retry of the same answer names a request that is gone.
        let late = post_input(&problem, &body).await.expect("late answer");
        assert_eq!(late.status(), reqwest::StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn a_transition_closes_the_pending_problem_and_says_so() {
        let problem = problem_over_http().await;

        let response = raw_client()
            .post(url(
                &problem.handle,
                &format!("/executions/{}/transition", problem.execution_id),
            ))
            .bearer_auth(problem.handle.token())
            .json(&TransitionRequestBody {
                label: Some("implemented".to_string()),
                target: None,
                force: false,
            })
            .send()
            .await
            .expect("post transition");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert!(!problem.store.has_pending_input(&problem.execution_id));

        let events = events(&problem);
        let closed = events
            .iter()
            .position(|event| {
                matches!(
                    &event.data,
                    EventData::HumanInputReceived {
                        close_reason: Some(InputCloseReason::Transition),
                        answered_by: Some(AnsweredBy::Operator),
                        episode: Some(episode),
                        ..
                    } if episode == problem.pending.episode_key()
                )
            })
            .expect("the problem is closed by the transition");
        let decided = events
            .iter()
            .position(|event| matches!(event.data, EventData::DecisionMade { .. }))
            .expect("the transition's decision");
        assert!(closed < decided, "closed first");

        // What the agent did say travels on, as for any agent step left (KTD8).
        let execution = problem
            .store
            .load_execution(&problem.execution_id)
            .expect("reload");
        assert_eq!(
            execution.metadata["engine"]["last_agent_step"]["answer_tail"],
            serde_json::json!("Done, all tests pass.")
        );
    }
}

// ---------------------------------------------------------------------------
// Conductor plan U11: a failed step's problem, and recoveries that name it
// ---------------------------------------------------------------------------

mod failures {
    use super::*;

    use narsor::api::ProblemKindDto;
    use narsor_engine::conductor::{FailureOutcome, FailureReport};
    use narsor_engine::engine::StepFailure;

    /// An execution of `two_steps` failed on `first` with a step failure
    /// the conductor diagnosed, and a server over it.
    struct Failed {
        _root: TempRoot,
        store: ExecutionStore,
        handle: api::ApiHandle,
        execution_id: String,
    }

    async fn failed_over_http() -> Failed {
        let root = TempRoot::new();
        let store = root.store();
        let engine = Arc::new(Engine::new(store.clone()));
        let definition_path = engine_support::fixtures().join("engine/two_steps.yaml");
        let definition = load_from_file(&definition_path).expect("load the fixture");
        let workspace_path = root.join("workspace");
        fs::create_dir_all(&workspace_path).expect("create workspace dir");
        fs::copy(
            engine_support::fixtures().join("scripts/print_json.sh"),
            workspace_path.join("print_json.sh"),
        )
        .expect("copy the fixture script");

        let mut execution = engine
            .start(
                &definition,
                serde_json::json!({}),
                Workspace::local_folder(workspace_path.display().to_string()),
            )
            .expect("start execution");
        let failure = StepFailure {
            state_id: "first".to_string(),
            action_execution_id: "attempt-2".to_string(),
            episode: "attempt-2".to_string(),
            error: "the build broke".to_string(),
            side_effect_free: false,
            conductor_retry: false,
        };
        execution.status = ExecutionStatus::Failed;
        execution.current_state = Some("first".to_string());
        execution.metadata = serde_json::json!({
            "cli": { "definition_path": definition_path.display().to_string() },
            "engine": { "step_failure": failure },
        });
        store.save_execution(&execution).expect("save the failure");

        let conductor = ConductorStore::new(store.paths(), &execution.id);
        conductor
            .save_state(&ConductorState {
                failure: Some(FailureReport {
                    key: "attempt-2".to_string(),
                    episode: "attempt-2".to_string(),
                    state_id: "first".to_string(),
                    reason: "A dependency is missing.".to_string(),
                    outcome: FailureOutcome::Diagnosed {
                        diagnosis: "A dependency is missing.".to_string(),
                        proposals: Vec::new(),
                    },
                    concluded_at: Utc::now(),
                }),
                ..ConductorState::default()
            })
            .expect("save the conductor's report");

        // With a control plane, so `/control/v1/executions` (the Historia)
        // is served beside the legacy routes.
        let handle = api::start_with_services(
            Arc::clone(&engine),
            root.path().to_path_buf(),
            root.path().to_path_buf(),
            api::ApiServices {
                control: Some(Arc::new(
                    narsor_engine::controller_protocol::ProtocolService::new(
                        narsor_engine::orchestration::ControlPlane::new(StatePaths::new(
                            root.path(),
                        )),
                        environment_support::seeded_resolver(root.path()),
                    ),
                )),
                ..api::ApiServices::default()
            },
        )
        .await
        .expect("server starts");
        Failed {
            _root: root,
            store,
            handle,
            execution_id: execution.id,
        }
    }

    fn post_recover(
        failed: &Failed,
        body: &serde_json::Value,
    ) -> impl std::future::Future<Output = reqwest::Result<reqwest::Response>> {
        raw_client()
            .post(url(
                &failed.handle,
                &format!("/executions/{}/recover", failed.execution_id),
            ))
            .bearer_auth(failed.handle.token())
            .json(body)
            .send()
    }

    /// KTD4: a failed step stays `Failed`, and the list shows the
    /// conductor's diagnosis as its problem, keyed by the failure.
    #[tokio::test]
    async fn the_list_shows_a_diagnosed_failure_as_the_executions_problem() {
        let failed = failed_over_http().await;
        let list = failed
            .handle
            .client()
            .list_executions()
            .await
            .expect("list executions");
        let summary = list
            .executions
            .iter()
            .find(|summary| summary.id == failed.execution_id)
            .expect("listed");
        assert_eq!(summary.status, narsor::api::ExecutionStatusDto::Failed);
        let problem = summary.problem.clone().expect("a problem");
        assert_eq!(problem.kind, ProblemKindDto::Failure);
        assert_eq!(problem.reason, "A dependency is missing.");
        assert_eq!(problem.episode.as_deref(), Some("attempt-2"));

        let state = failed
            .handle
            .client()
            .conductor_state(&failed.execution_id)
            .await
            .expect("the conductor's state");
        assert_eq!(
            state.failure.map(|report| report.key).as_deref(),
            Some("attempt-2")
        );
    }

    /// U6: the Historia shows a failed old execution as recoverable, with
    /// its problem, and recovering it through `/control/v1` starts it again.
    #[tokio::test]
    async fn history_recovers_a_failed_execution() {
        let failed = failed_over_http().await;
        let client = failed.handle.client();

        let history = client.control_history().await.expect("history");
        let row = history
            .executions
            .iter()
            .find(|row| row.id == failed.execution_id)
            .expect("listed");
        assert_eq!(row.status, ExecutionStatus::Failed);
        assert!(row.recoverable, "{row:?}");
        assert_eq!(
            row.problem.as_ref().map(|problem| problem.reason.as_str()),
            Some("A dependency is missing.")
        );
        let capabilities = client.control_capabilities().await.expect("capabilities");
        assert!(capabilities.legacy_executions);

        let accepted = client
            .control_recover_execution(&failed.execution_id, None)
            .await
            .expect("recovered");
        assert_eq!(accepted.execution_id, failed.execution_id);
        let stored = failed
            .store
            .load_execution(&failed.execution_id)
            .expect("load");
        assert!(stored.metadata["engine"].get("step_failure").is_none());
    }

    /// KTD9: a recovery naming a failure the execution is no longer failed
    /// on is `409` and changes nothing; one naming the current failure is
    /// accepted, and the problem is gone with the failure record.
    #[tokio::test]
    async fn a_recovery_naming_a_stale_failure_is_a_conflict() {
        let failed = failed_over_http().await;

        let stale = post_recover(
            &failed,
            &serde_json::json!({ "episode": "attempt-1", "comment": "again" }),
        )
        .await
        .expect("send");
        assert_eq!(stale.status(), reqwest::StatusCode::CONFLICT);
        let stored = failed
            .store
            .load_execution(&failed.execution_id)
            .expect("load");
        assert_eq!(stored.status, ExecutionStatus::Failed);
        assert!(stored.metadata["engine"].get("step_failure").is_some());

        let accepted = failed
            .handle
            .client()
            .recover_failure(&failed.execution_id, "attempt-2", None)
            .await
            .expect("the current failure is recovered");
        assert_eq!(accepted.execution_id, failed.execution_id);
        let stored = failed
            .store
            .load_execution(&failed.execution_id)
            .expect("load");
        assert!(stored.metadata["engine"].get("step_failure").is_none());
        let list = failed
            .handle
            .client()
            .list_executions()
            .await
            .expect("list executions");
        let summary = list
            .executions
            .iter()
            .find(|summary| summary.id == failed.execution_id)
            .expect("listed");
        assert_eq!(summary.problem, None);
    }
}

// ---------------------------------------------------------------------------
// Conductor plan U12: the chat, proposals, gate summaries and undo
// ---------------------------------------------------------------------------

mod conductor_chat {
    use super::*;

    use std::collections::{HashMap, VecDeque};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;

    use narsor_engine::conductor::{
        ConductorAnswer, ConductorInvoker, ConductorRegistry, Invocation, InvocationOutcome,
        ProposalStatus,
    };
    use narsor_engine::domain::{AnsweredBy, ConductorPurpose, InputCloseReason};
    use narsor_engine::engine::{ActionExecutor, ActionKind};
    use narsor_server::api::ApiServices;

    /// A conductor answering each purpose from a list of its own; anything
    /// unscripted fails.
    struct ScriptedConductor {
        answers: Mutex<HashMap<ConductorPurpose, VecDeque<serde_json::Value>>>,
    }

    impl ConductorInvoker for ScriptedConductor {
        fn invoke<'a>(
            &'a self,
            invocation: &'a Invocation,
        ) -> Pin<Box<dyn Future<Output = InvocationOutcome> + Send + 'a>> {
            let answer = self
                .answers
                .lock()
                .unwrap()
                .get_mut(&invocation.request.purpose)
                .and_then(VecDeque::pop_front);
            Box::pin(async move {
                match answer {
                    Some(response) => InvocationOutcome::Answered(ConductorAnswer {
                        message: response
                            .get("message")
                            .and_then(serde_json::Value::as_str)
                            .map(ToString::to_string),
                        response,
                    }),
                    None => InvocationOutcome::Failed {
                        error: "nothing scripted".to_string(),
                    },
                }
            })
        }
    }

    /// An execution stopped by `definition_path`'s workflow, and a server
    /// with a scripted conductor over the engine that stopped it.
    struct Served {
        _root: TempRoot,
        store: ExecutionStore,
        handle: api::ApiHandle,
        execution_id: String,
        _registry: Arc<ConductorRegistry>,
    }

    async fn served(
        definition_path: PathBuf,
        inputs: serde_json::Value,
        agent: Option<Arc<dyn ActionExecutor>>,
        answers: Vec<(ConductorPurpose, serde_json::Value)>,
    ) -> Served {
        let root = TempRoot::new();
        let store = root.store();
        let mut engine = Engine::new(store.clone());
        if let Some(agent) = agent {
            engine = engine.with_executor(ActionKind::Agent, agent);
        }
        let engine = Arc::new(engine);
        let mut by_purpose: HashMap<ConductorPurpose, VecDeque<serde_json::Value>> = HashMap::new();
        for (purpose, answer) in answers {
            by_purpose.entry(purpose).or_default().push_back(answer);
        }
        let registry = assemble(
            &engine,
            Arc::new(ScriptedConductor {
                answers: Mutex::new(by_purpose),
            }),
            None,
        );

        let definition = load_from_file(&definition_path).expect("load the fixture");
        let workspace_path = root.join("workspace");
        fs::create_dir_all(&workspace_path).expect("create workspace dir");
        fs::copy(
            engine_support::fixtures().join("scripts/print_json.sh"),
            workspace_path.join("print_json.sh"),
        )
        .expect("copy the fixture script");
        let execution = engine
            .start(
                &definition,
                inputs,
                Workspace::local_folder(workspace_path.display().to_string()),
            )
            .expect("start execution");
        pin_definition_path(&store, &execution.id, &definition_path);
        engine
            .run(&definition, &execution.id)
            .await
            .expect("run until it stops");

        let handle = api::start_with_services(
            engine,
            root.path().to_path_buf(),
            root.path().to_path_buf(),
            ApiServices {
                conductor: Some(Arc::clone(&registry)),
                ..ApiServices::default()
            },
        )
        .await
        .expect("server starts");
        Served {
            _root: root,
            store,
            handle,
            execution_id: execution.id,
            _registry: registry,
        }
    }

    /// The pipeline, stopped at its `review` gate.
    async fn at_the_review(answers: Vec<(ConductorPurpose, serde_json::Value)>) -> Served {
        served(
            pipeline_definition_path(),
            serde_json::json!({
                "script_path": prepare_script_path().display().to_string(),
                "ticket": "ORC-12",
            }),
            None,
            answers,
        )
        .await
    }

    fn approve_proposal() -> (ConductorPurpose, serde_json::Value) {
        (
            ConductorPurpose::ReplyToChat,
            serde_json::json!({
                "decision": "reply",
                "message": "I would approve it.",
                "confidence": "high",
                "rationale": "The preparation succeeded.",
                "proposals": [{
                    "kind": "answer_input",
                    "summary": "Approve the change",
                    "values": {"approval_status": "approved", "comment": "approved by the conductor"}
                }],
            }),
        )
    }

    /// Posts `text` and waits for the proposal the conductor answers with.
    async fn proposal_for(served: &Served, text: &str) -> String {
        let client = served.handle.client();
        let posted = client
            .post_chat(&served.execution_id, text)
            .await
            .expect("post the message");
        assert_eq!(posted.text, text);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let chat = client
                .conductor_chat(&served.execution_id, posted.sequence)
                .await
                .expect("read the chat");
            if let Some(proposal) = chat
                .entries
                .iter()
                .find(|entry| entry.kind == narsor::api::ChatEntryKindDto::Proposal)
            {
                return proposal.id.clone();
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no proposal came: {:?}",
                chat.entries
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn summary(served: &Served) -> ExecutionSummary {
        served
            .handle
            .client()
            .list_executions()
            .await
            .expect("list")
            .executions
            .into_iter()
            .find(|summary| summary.id == served.execution_id)
            .expect("the execution is listed")
    }

    /// R30, AE16: the gate's summary rides on the pending input's read, and
    /// the gate waits; R28: a proposal confirmed over the API answers it —
    /// once, however often it is confirmed.
    #[tokio::test]
    async fn the_chat_proposes_and_a_confirmation_over_the_api_answers_once() {
        let served = at_the_review(vec![
            (
                ConductorPurpose::SummarizeGate,
                serde_json::json!({
                    "decision": "annotate",
                    "summary": "The change was prepared for ORC-12.",
                    "recommendation": "approved",
                }),
            ),
            approve_proposal(),
        ])
        .await;
        let client = served.handle.client();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let listed = loop {
            let listed = summary(&served).await;
            if listed.annotation.is_some() || tokio::time::Instant::now() >= deadline {
                break listed;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(listed.status, narsor::api::ExecutionStatusDto::InputNeeded);
        let pending: narsor_protocol::api::PendingHumanInputView =
            listed.pending_input.clone().expect("the gate");
        let annotation = listed.annotation.expect("the gate's summary");
        assert_eq!(annotation.key, pending.episode_key());
        assert_eq!(annotation.recommendation.as_deref(), Some("approved"));

        let proposal = proposal_for(&served, "approve it for me").await;
        assert_eq!(
            summary(&served).await.status,
            narsor::api::ExecutionStatusDto::InputNeeded,
            "nothing happens before the confirmation"
        );

        let first = client
            .confirm_proposal(&served.execution_id, &proposal)
            .await
            .expect("confirm");
        assert_eq!(first.status, ProposalStatus::Executed);
        assert!(!first.repeated);
        wait_for_status(
            &served.store,
            &served.execution_id,
            ExecutionStatus::Completed,
            Duration::from_secs(5),
        )
        .await;
        let second = client
            .confirm_proposal(&served.execution_id, &proposal)
            .await
            .expect("confirm again");
        assert!(second.repeated);
        assert_eq!(second.decision_id, first.decision_id);

        let events = client
            .events(&served.execution_id, 0)
            .await
            .expect("events");
        let answers: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.data {
                EventData::HumanInputReceived {
                    answered_by,
                    close_reason: Some(InputCloseReason::Answered),
                    episode,
                    ..
                } => Some((*answered_by, episode.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            answers,
            vec![(
                Some(AnsweredBy::Conductor),
                Some(pending.episode_key().to_string())
            )]
        );
    }

    /// KTD9: a proposal confirmed after the operator answered the gate is a
    /// `409` whose body says why; an unknown proposal is `404`.
    #[tokio::test]
    async fn a_stale_confirmation_is_a_conflict_with_its_reason() {
        let served = at_the_review(vec![approve_proposal()]).await;
        let client = served.handle.client();
        let proposal = proposal_for(&served, "approve it").await;

        let pending = summary(&served).await.pending_input.expect("the gate");
        client
            .deliver_input(
                &served.execution_id,
                wire_delivery(
                    HumanInputDelivery::with_values(
                        serde_json::json!({"approval_status": "rejected", "comment": "no"})
                            .as_object()
                            .cloned()
                            .unwrap(),
                    )
                    .for_episode(pending.episode_key()),
                ),
            )
            .await
            .expect("the operator answers first");
        wait_for_status(
            &served.store,
            &served.execution_id,
            ExecutionStatus::Failed,
            Duration::from_secs(5),
        )
        .await;

        match client
            .confirm_proposal(&served.execution_id, &proposal)
            .await
        {
            Err(ClientError::Refused { code, message }) => {
                assert_eq!(code, "stale_proposal");
                assert!(message.contains("moved on"), "{message}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(matches!(
            client
                .confirm_proposal(&served.execution_id, "no-such-proposal")
                .await,
            Err(ClientError::NotFound)
        ));
        assert_eq!(
            served
                .store
                .load_execution(&served.execution_id)
                .unwrap()
                .status,
            ExecutionStatus::Failed
        );
    }

    /// KTD13 over the API: the conductor's choice is undone, once; anything
    /// that is not one is refused.
    #[tokio::test]
    async fn a_chosen_output_is_undone_over_the_api() {
        let served = served(
            engine_support::fixtures().join("engine/agent_problem.yaml"),
            serde_json::json!({}),
            Some(Arc::new(engine_support::LabellessAgentExecutor::answering(
                "Done, all tests pass.",
            ))),
            vec![(
                ConductorPurpose::ChooseOutputAfterReprompt,
                serde_json::json!({
                    "decision": "choose_output",
                    "label": "implemented",
                    "variables": {"summary": "all tests pass"},
                    "confidence": "high",
                }),
            )],
        )
        .await;
        let client = served.handle.client();
        assert_eq!(
            summary(&served).await.status,
            narsor::api::ExecutionStatusDto::Completed
        );
        let decision = client
            .conductor_chat(&served.execution_id, 0)
            .await
            .expect("chat")
            .entries
            .into_iter()
            .find(|entry| entry.kind == narsor::api::ChatEntryKindDto::Decision)
            .and_then(|entry| {
                entry
                    .payload?
                    .get("decision_id")?
                    .as_str()
                    .map(String::from)
            })
            .expect("the decision in the chat");

        let undone = client
            .undo_decision(&served.execution_id, &decision)
            .await
            .expect("undo");
        assert_eq!(undone.state_id, "work");
        assert!(undone.warning.is_some());
        let listed = summary(&served).await;
        assert_eq!(listed.status, narsor::api::ExecutionStatusDto::InputNeeded);
        assert_eq!(
            listed.problem.and_then(|problem| problem.episode),
            Some(undone.episode)
        );

        assert!(matches!(
            client.undo_decision(&served.execution_id, &decision).await,
            Err(ClientError::Refused { ref code, .. }) if code == "already_undone"
        ));
        assert!(matches!(
            client.undo_decision(&served.execution_id, "no-such").await,
            Err(ClientError::NotFound)
        ));
    }

    /// Without a conductor behind the server, the chat cannot be written.
    #[tokio::test]
    async fn without_a_conductor_the_chat_is_not_served() {
        let root = TempRoot::new();
        let store = root.store();
        store
            .save_execution(&sample_execution("exec-chat", ExecutionStatus::Paused))
            .expect("save");
        let handle = start_server(store).await;
        assert!(matches!(
            handle.client().post_chat("exec-chat", "hello").await,
            Err(ClientError::Server(_))
        ));
    }
}

// ---------------------------------------------------------------------------
// Remote-task control plane routes (U2)
// ---------------------------------------------------------------------------

async fn start_server_with_control_plane(root: &TempRoot) -> api::ApiHandle {
    let control = narsor_engine::orchestration::ControlPlane::new(StatePaths::new(root.path()));
    api::start_with_services(
        Arc::new(Engine::new(root.store())),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
        api::ApiServices {
            control: Some(Arc::new(
                narsor_engine::controller_protocol::ProtocolService::new(
                    control,
                    environment_support::seeded_resolver(root.path()),
                ),
            )),
            ..api::ApiServices::default()
        },
    )
    .await
    .expect("server starts")
}

fn remote_task(
    task_id: &str,
    backend_id: &str,
) -> narsor_engine::controller_protocol::CreateTaskRequest {
    narsor_engine::controller_protocol::CreateTaskRequest {
        task_id: Some(task_id.to_string()),
        backend_id: Some(backend_id.to_string()),
        launch: narsor_engine::controller_protocol::LaunchRequest {
            workflow_id: "implement-feature".to_string(),
            workflow_version: 1,
            inputs: serde_json::json!({}),
            workspace: None,
            profile: None,
            harness: None,
            resources: None,
            ticket: None,
            title: None,
        },
    }
}

#[tokio::test]
async fn control_plane_routes_sit_behind_the_owner_token_beside_the_local_ones() {
    let root = TempRoot::new();
    seed_execution_with_retry(&root.store(), "exec-1");
    let handle = start_server_with_control_plane(&root).await;
    let raw = raw_client();

    let refused = raw
        .get(url(&handle, "/control/v1/tasks"))
        .send()
        .await
        .expect("request without token");
    assert_eq!(refused.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert!(refused.text().await.unwrap().is_empty());

    let client = handle.client();
    let created = client
        .create_control_task(&remote_task("task-1", "k8s"))
        .await
        .expect("create");
    assert_eq!(
        created.runner_state,
        narsor_engine::orchestration::RunnerState::Queued
    );
    assert_eq!(created.generation, 1);
    assert!(created.runner.is_none() && created.observation.is_none());
    let again = client
        .create_control_task(&remote_task("task-1", "k8s"))
        .await
        .expect("idempotent create");
    assert_eq!(again.revision, created.revision);

    // The local daemon path is untouched.
    let local = client.list_executions().await.expect("local executions");
    assert_eq!(local.executions.len(), 1);
    assert_eq!(
        client
            .timeline("exec-1")
            .await
            .expect("timeline")
            .positions
            .len(),
        2
    );
}

#[tokio::test]
async fn control_plane_refusals_are_typed() {
    let root = TempRoot::new();
    let handle = start_server_with_control_plane(&root).await;
    let client = handle.client();

    match client
        .create_control_task(&remote_task("../escape", "k8s"))
        .await
    {
        Err(ClientError::Request { code, .. }) => assert_eq!(code, "invalid_identifier"),
        other => panic!("expected a typed 400, got {other:?}"),
    }

    client
        .create_control_task(&remote_task("task-1", "k8s"))
        .await
        .expect("create");
    match client
        .create_control_task(&remote_task("task-1", "other-backend"))
        .await
    {
        Err(ClientError::Refused { code, .. }) => assert_eq!(code, "task_conflict"),
        other => panic!("expected a typed 409, got {other:?}"),
    }

    let pause = narsor_engine::controller_protocol::OwnerCommandRequest {
        command: narsor_engine::orchestration::OwnerCommand::Pause,
        idempotency_key: "pause-1".to_string(),
        expected_generation: None,
    };
    match client.control_command("task-1", &pause).await {
        Err(ClientError::Refused { code, .. }) => assert_eq!(code, "illegal_command"),
        other => panic!("a queued task cannot pause, got {other:?}"),
    }
    let stale = narsor_engine::controller_protocol::OwnerCommandRequest {
        command: narsor_engine::orchestration::OwnerCommand::Archive,
        idempotency_key: "archive-1".to_string(),
        expected_generation: Some(9),
    };
    match client.control_command("task-1", &stale).await {
        Err(ClientError::Refused { code, .. }) => assert_eq!(code, "stale_generation"),
        other => panic!("expected stale generation, got {other:?}"),
    }
    assert!(matches!(
        client.control_task("no-such-task").await,
        Err(ClientError::NotFound)
    ));
}

#[tokio::test]
async fn control_plane_routes_are_absent_without_a_control_plane() {
    let root = TempRoot::new();
    let handle = start_server(root.store()).await;
    let response = raw_client()
        .get(url(&handle, "/control/v1/tasks"))
        .bearer_auth(handle.token())
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Default backend, priority and queue admission (U10)
// ---------------------------------------------------------------------------

/// A served control plane plus the in-process service, so a test can play
/// the controller of a backend without an HTTP controller gate.
async fn start_scheduling_server(
    root: &TempRoot,
) -> (
    api::ApiHandle,
    Arc<narsor_engine::controller_protocol::ProtocolService>,
) {
    let control = narsor_engine::orchestration::ControlPlane::new(StatePaths::new(root.path()));
    let launch = environment_support::seeded_catalog_resolver(root.path());
    let service = Arc::new(narsor_engine::controller_protocol::ProtocolService::new(
        control,
        launch.clone(),
    ));
    let handle = api::start_with_services(
        Arc::new(Engine::new(root.store())),
        root.path().to_path_buf(),
        root.path().to_path_buf(),
        api::ApiServices {
            control: Some(Arc::clone(&service)),
            local_launch: Some(launch),
            ..api::ApiServices::default()
        },
    )
    .await
    .expect("server starts");
    (handle, service)
}

#[tokio::test]
async fn post_tasks_acknowledges_a_queued_task_without_creating_an_execution() {
    use narsor_engine::orchestration::RunnerState;

    let root = TempRoot::new();
    let (handle, _service) = start_scheduling_server(&root).await;
    let response = raw_client()
        .post(url(&handle, "/tasks"))
        .bearer_auth(handle.token())
        .json(&serde_json::json!({
            "task_id": "local-create-1",
            "workflow": "implement-feature",
            "inputs": {}
        }))
        .send()
        .await
        .expect("post task");

    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    let created: StartTaskResponse = response.json().await.expect("task acknowledgement");
    assert_eq!(created.task.task_id.as_str(), "local-create-1");
    assert_eq!(created.task.runner_state, RunnerState::Queued);
    assert!(root.store().list_execution_ids().unwrap().is_empty());
}

#[tokio::test]
async fn post_tasks_replays_the_same_submission_without_a_second_task() {
    let root = TempRoot::new();
    let (handle, service) = start_scheduling_server(&root).await;
    let body = serde_json::json!({
        "task_id": "local-replay-1",
        "workflow": "implement-feature",
        "inputs": {}
    });

    let submit = || async {
        raw_client()
            .post(url(&handle, "/tasks"))
            .bearer_auth(handle.token())
            .json(&body)
            .send()
            .await
            .expect("post task")
    };
    let first = submit().await;
    let second = submit().await;
    assert_eq!(first.status(), reqwest::StatusCode::ACCEPTED);
    assert_eq!(second.status(), reqwest::StatusCode::ACCEPTED);
    let first: StartTaskResponse = first.json().await.expect("first acknowledgement");
    let second: StartTaskResponse = second.json().await.expect("replay acknowledgement");
    assert_eq!(first.task.task_id, second.task.task_id);
    assert_eq!(first.task.revision, second.task.revision);
    assert_eq!(service.control_plane().list_tasks().unwrap().len(), 1);
}

#[tokio::test]
async fn post_tasks_refuses_a_reused_submission_id_with_different_intent() {
    let root = TempRoot::new();
    let (handle, service) = start_scheduling_server(&root).await;
    let post = |body: serde_json::Value| {
        let handle = &handle;
        async move {
            raw_client()
                .post(url(handle, "/tasks"))
                .bearer_auth(handle.token())
                .json(&body)
                .send()
                .await
                .expect("post task")
        }
    };

    let first = post(serde_json::json!({
        "task_id": "local-conflict-1",
        "workflow": "implement-feature",
        "inputs": {}
    }))
    .await;
    assert_eq!(first.status(), reqwest::StatusCode::ACCEPTED);

    let divergent = post(serde_json::json!({
        "task_id": "local-conflict-1",
        "workflow": "implement-feature",
        "inputs": { "note": "a different intent" }
    }))
    .await;
    assert_eq!(divergent.status(), reqwest::StatusCode::CONFLICT);
    assert_eq!(
        service.control_plane().list_tasks().unwrap().len(),
        1,
        "the conflicting reuse must not queue a second task"
    );
}

#[tokio::test]
async fn post_tasks_rejects_an_invalid_submission_without_queueing_anything() {
    let root = TempRoot::new();
    let (handle, service) = start_scheduling_server(&root).await;
    let post = |body: serde_json::Value| {
        let handle = &handle;
        async move {
            raw_client()
                .post(url(handle, "/tasks"))
                .bearer_auth(handle.token())
                .json(&body)
                .send()
                .await
                .expect("post task")
        }
    };

    let malformed_id = post(serde_json::json!({
        "task_id": "../not an id",
        "workflow": "implement-feature",
        "inputs": {}
    }))
    .await;
    assert_eq!(malformed_id.status(), reqwest::StatusCode::BAD_REQUEST);

    let unknown_workflow = post(serde_json::json!({
        "task_id": "local-invalid-1",
        "workflow": "no-such-workflow",
        "inputs": {}
    }))
    .await;
    assert_eq!(unknown_workflow.status(), reqwest::StatusCode::BAD_REQUEST);

    assert!(service.control_plane().list_tasks().unwrap().is_empty());
}

/// One enrolled controller holding `backend`'s lease, called in-process.
struct InProcessController {
    service: Arc<narsor_engine::controller_protocol::ProtocolService>,
    identity: narsor_engine::controller_protocol::ControllerIdentity,
    epoch: u64,
}

impl InProcessController {
    fn online(
        service: &Arc<narsor_engine::controller_protocol::ProtocolService>,
        backend: &str,
    ) -> Self {
        use narsor_engine::controller_protocol::{AcquireLeaseRequest, CreateEnrollmentRequest};
        let code = service
            .create_enrollment(backend, CreateEnrollmentRequest::default())
            .expect("enrollment");
        let enrolled = service
            .enroll(narsor_engine::auth::pop::signed_enrollment_request(
                &code.enrollment_code,
                &narsor_engine::auth::ControllerKey::generate(),
            ))
            .expect("enroll");
        let identity = narsor_engine::controller_protocol::ControllerIdentity {
            controller_id: enrolled.controller_id,
            backend_id: enrolled.backend_id,
        };
        let epoch = service
            .acquire_lease(
                &identity,
                AcquireLeaseRequest {
                    ttl_secs: Some(600),
                },
            )
            .expect("lease")
            .epoch;
        Self {
            service: Arc::clone(service),
            identity,
            epoch,
        }
    }

    /// The tasks this controller is offered a start for.
    fn offered_starts(&self) -> Vec<String> {
        self.service
            .commands(&self.identity, true)
            .expect("commands")
            .commands
            .into_iter()
            .filter(|command| {
                command.action == narsor_engine::controller_protocol::DesiredAction::Start
            })
            .map(|command| command.task_id.to_string())
            .collect()
    }

    fn report(
        &self,
        task: &str,
        observation: narsor_engine::orchestration::Observation,
    ) -> Result<(), narsor_engine::controller_protocol::ProtocolError> {
        let generation = self
            .service
            .control_plane()
            .task(&narsor_engine::domain::TaskId::parse(task).unwrap())
            .unwrap()
            .generation;
        self.service
            .report(
                &self.identity,
                narsor_engine::controller_protocol::ReportRequest {
                    task_id: task.to_string(),
                    lease_epoch: self.epoch,
                    desired_generation: generation,
                    observation,
                    resource: None,
                    summary: Default::default(),
                    observed_at: Utc::now(),
                },
            )
            .map(|_| ())
    }

    fn run(&self, task: &str) {
        use narsor_engine::orchestration::Observation;
        self.report(task, Observation::StartAccepted)
            .expect("start accepted");
        self.report(task, Observation::RunnerReady)
            .expect("runner ready");
    }
}

/// AE2 across entry points: the local `/tasks` facade and the control-plane
/// create share one queue — arrival order until the owner moves a task — and a
/// task submitted while no controller holds the lease waits with its offline
/// reason, with no execution behind it.
#[tokio::test]
async fn local_and_control_plane_creates_share_one_ranked_queue() {
    use narsor_engine::controller_protocol::MoveTaskRequest;
    use narsor_engine::domain::TaskId;
    use narsor_engine::orchestration::{MoveTarget, Observation, QueueReason, RunnerState};

    let root = TempRoot::new();
    let (handle, service) = start_scheduling_server(&root).await;
    set_backend_limit(&root, "k8s", 1);
    let client = handle.client();
    let reason = |summary: narsor_engine::controller_protocol::TaskSummary| {
        summary.queue.map(|queue| queue.reason)
    };

    let submit_local = |task_id: &'static str| {
        let handle = &handle;
        async move {
            let response = raw_client()
                .post(url(handle, "/tasks"))
                .bearer_auth(handle.token())
                .json(&serde_json::json!({
                    "task_id": task_id,
                    "workflow": "implement-feature",
                    "inputs": {}
                }))
                .send()
                .await
                .expect("post task");
            assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
            let created: StartTaskResponse = response.json().await.expect("acknowledgement");
            created.task
        }
    };

    // No controller yet: queued, offline, and nothing was started.
    let first = submit_local("first").await;
    assert_eq!(first.runner_state, RunnerState::Queued);
    assert_eq!(first.rank, 1, "a local task joins the end of the queue");
    assert_eq!(
        reason(first.clone()),
        Some(QueueReason::ControllerOffline),
        "an unadmitted local task says why it waits"
    );
    assert!(
        first.runner.is_none(),
        "no execution exists before admission"
    );

    let controller = InProcessController::online(&service, "k8s");
    assert_eq!(controller.offered_starts(), ["first"]);
    controller.run("first");

    // Three more, from both surfaces, while the only slot is taken.
    submit_local("local-b").await;
    client
        .create_control_task(&remote_task("remote-c", "k8s"))
        .await
        .expect("create");
    client
        .create_control_task(&remote_task("remote-d", "k8s"))
        .await
        .expect("create");
    submit_local("local-e").await;
    // The owner moves the fourth arrival to the head of the waiting.
    client
        .control_move_task(
            "remote-d",
            &MoveTaskRequest {
                target: MoveTarget::Before(TaskId::parse("local-b").expect("id")).into(),
                expected_generation: Some(1),
            },
        )
        .await
        .expect("move");

    let position = |id: &'static str| {
        let client = &client;
        async move {
            match reason(client.control_task(id).await.unwrap()) {
                Some(QueueReason::WaitingForCapacity { position, .. }) => position,
                other => panic!("{id} should wait for capacity, got {other:?}"),
            }
        }
    };
    assert_eq!(position("remote-d").await, 1, "moved to the head");
    assert_eq!(position("local-b").await, 2, "then arrival order");
    assert_eq!(position("remote-c").await, 3);
    assert_eq!(position("local-e").await, 4);
    assert!(controller.offered_starts().is_empty(), "no preemption");

    // Freeing the slot admits exactly the head of that single queue.
    client
        .control_command(
            "first",
            &narsor_engine::controller_protocol::OwnerCommandRequest {
                command: narsor_engine::orchestration::OwnerCommand::Archive,
                idempotency_key: "archive-first".to_string(),
                expected_generation: None,
            },
        )
        .await
        .expect("archive");
    controller
        .report("first", Observation::ResourcesDeleted)
        .expect("deleted");
    assert_eq!(controller.offered_starts(), ["remote-d"]);
}

fn set_backend_limit(root: &TempRoot, backend: &str, limit: u32) {
    let mut record = environment_support::backend(backend);
    record.limits.max_concurrent_tasks = limit;
    narsor_engine::environment::EnvironmentCatalog::new(root.path())
        .save_backend(&record)
        .expect("save backend");
}

fn inheriting_task(task_id: &str) -> narsor_engine::controller_protocol::CreateTaskRequest {
    narsor_engine::controller_protocol::CreateTaskRequest {
        backend_id: None,
        ..remote_task(task_id, "unused")
    }
}

#[tokio::test]
async fn new_tasks_inherit_the_owner_default_backend_and_existing_tasks_never_move() {
    use narsor_engine::controller_protocol::SetDefaultBackendRequest;
    use narsor_engine::orchestration::RunnerState;
    let root = TempRoot::new();
    let (handle, service) = start_scheduling_server(&root).await;
    let client = handle.client();

    let initial = client.control_default_backend().await.expect("default");
    assert_eq!(
        (initial.configured.as_str(), initial.effective.as_str()),
        ("k8s", "k8s")
    );
    assert_eq!((initial.override_backend, initial.revision), (None, 0));

    let chosen = client
        .control_set_default_backend(&SetDefaultBackendRequest {
            backend_id: Some("backend-a".to_string()),
        })
        .await
        .expect("choose backend-a");
    assert_eq!(chosen.effective, "backend-a");
    assert_eq!(chosen.revision, 1);

    let inherited = client
        .create_control_task(&inheriting_task("inherits"))
        .await
        .expect("create inheriting");
    assert_eq!(inherited.backend_id.as_str(), "backend-a");
    let explicit = client
        .create_control_task(&remote_task("explicit", "k8s"))
        .await
        .expect("create explicit");
    assert_eq!(explicit.backend_id.as_str(), "k8s");

    let controller = InProcessController::online(&service, "backend-a");
    controller.run("inherits");
    let running = client.control_task("inherits").await.expect("task");
    assert_eq!(running.runner_state, RunnerState::Running);

    // AE3: changing the default moves nothing and stops nothing.
    client
        .control_set_default_backend(&SetDefaultBackendRequest {
            backend_id: Some("backend-b".to_string()),
        })
        .await
        .expect("choose backend-b");
    let after = client.control_task("inherits").await.expect("task");
    assert_eq!(after.backend_id.as_str(), "backend-a");
    assert_eq!(after.runner_state, RunnerState::Running);
    assert_eq!(after.generation, running.generation);

    // An identical create retry returns the existing task instead of
    // resolving the changed default into a conflict.
    let retried = client
        .create_control_task(&inheriting_task("inherits"))
        .await
        .expect("idempotent retry after a default change");
    assert_eq!(retried.backend_id.as_str(), "backend-a");
    assert_eq!(retried.revision, after.revision);

    let later = client
        .create_control_task(&inheriting_task("later"))
        .await
        .expect("create later");
    assert_eq!(later.backend_id.as_str(), "backend-b");

    match client
        .control_set_default_backend(&SetDefaultBackendRequest {
            backend_id: Some("nowhere".to_string()),
        })
        .await
    {
        Err(ClientError::Request { code, .. }) => assert_eq!(code, "unknown_backend"),
        other => panic!("an unregistered default must be refused, got {other:?}"),
    }
    let reset = client
        .control_set_default_backend(&SetDefaultBackendRequest { backend_id: None })
        .await
        .expect("reset");
    assert_eq!(reset.effective, "k8s");
    assert_eq!(reset.override_backend, None);
}

#[tokio::test]
async fn queued_tasks_wait_for_capacity_in_rank_order_with_a_visible_reason() {
    use narsor_engine::controller_protocol::MoveTaskRequest;
    use narsor_engine::orchestration::{
        MoveTarget, Observation, QueueReason, RunnerState, TaskEventKind,
    };
    let root = TempRoot::new();
    let (handle, service) = start_scheduling_server(&root).await;
    // Read fresh by every decision: no restart needed.
    set_backend_limit(&root, "k8s", 1);
    let client = handle.client();

    // Nobody can start anything yet: the controller is offline.
    let first = client
        .create_control_task(&remote_task("first", "k8s"))
        .await
        .expect("create");
    let queue = first.queue.expect("a queue verdict");
    assert_eq!(queue.reason, QueueReason::ControllerOffline);
    assert!(queue.message.contains("controller"), "{}", queue.message);

    let controller = InProcessController::online(&service, "k8s");
    assert_eq!(controller.offered_starts(), ["first"]);
    controller.run("first");

    for id in ["high", "low"] {
        client
            .create_control_task(&remote_task(id, "k8s"))
            .await
            .expect("create");
    }
    let reason = |summary: narsor_engine::controller_protocol::TaskSummary| {
        summary.queue.map(|queue| queue.reason)
    };
    assert_eq!(
        reason(client.control_task("high").await.unwrap()),
        Some(QueueReason::WaitingForCapacity {
            position: 1,
            running: 1,
            limit: 1
        })
    );
    assert!(
        controller.offered_starts().is_empty(),
        "the limit is reached"
    );
    match controller.report("high", Observation::StartAccepted) {
        Err(narsor_engine::controller_protocol::ProtocolError::Conflict { code, .. }) => {
            assert_eq!(code, "not_admitted")
        }
        other => panic!("an unadmitted start must be refused, got {other:?}"),
    }

    // The owner reorders the waiting; the running task is untouched (AE3).
    let raised = client
        .control_move_task(
            "low",
            &MoveTaskRequest {
                target: MoveTarget::Top.into(),
                expected_generation: Some(1),
            },
        )
        .await
        .expect("move to the top");
    assert_eq!(raised.rank, 1);
    assert_eq!(
        reason(raised),
        Some(QueueReason::WaitingForCapacity {
            position: 1,
            running: 1,
            limit: 1
        })
    );
    let events = client.control_events("low", 0).await.expect("events");
    assert!(events.iter().any(|event| matches!(
        event.kind,
        TaskEventKind::QueueChanged {
            reason: QueueReason::WaitingForCapacity { position: 1, .. }
        }
    )));
    match client
        .control_move_task(
            "first",
            &MoveTaskRequest {
                target: MoveTarget::Bottom.into(),
                expected_generation: None,
            },
        )
        .await
    {
        Err(ClientError::Refused { code, .. }) => assert_eq!(code, "task_not_queued"),
        other => panic!("a running task cannot be moved, got {other:?}"),
    }
    match client
        .control_move_task(
            "low",
            &MoveTaskRequest {
                target: MoveTarget::Bottom.into(),
                expected_generation: Some(4),
            },
        )
        .await
    {
        Err(ClientError::Refused { code, .. }) => assert_eq!(code, "stale_generation"),
        other => panic!("expected a stale generation, got {other:?}"),
    }
    let running = client.control_task("first").await.unwrap();
    assert_eq!(
        (running.runner_state, running.generation),
        (RunnerState::Running, 1)
    );

    // Archiving the running task frees the slot for the first in line.
    client
        .control_command(
            "first",
            &narsor_engine::controller_protocol::OwnerCommandRequest {
                command: narsor_engine::orchestration::OwnerCommand::Archive,
                idempotency_key: "archive-first".to_string(),
                expected_generation: None,
            },
        )
        .await
        .expect("archive");
    controller
        .report("first", Observation::ResourcesDeleted)
        .expect("deleted");
    assert_eq!(
        reason(client.control_task("low").await.unwrap()),
        Some(QueueReason::Admitted)
    );
    assert_eq!(controller.offered_starts(), ["low"]);

    // A revoked controller withholds the backend's capacity.
    service
        .revoke_controller(&controller.identity.controller_id)
        .expect("revoke");
    assert_eq!(
        reason(client.control_task("high").await.unwrap()),
        Some(QueueReason::ControllerOffline)
    );
    assert_eq!(
        reason(client.control_task("low").await.unwrap()),
        Some(QueueReason::Admitted),
        "an admitted task keeps its reservation"
    );
}

// ---------------------------------------------------------------------------
// Catalogue, repositories, history and capabilities (U6)
// ---------------------------------------------------------------------------

mod control_catalog {
    use super::*;

    use narsor_engine::controller_protocol::{
        ConversationResponse, CreateRepositoryRequest, OwnerCommandRequest, WorkflowSource,
    };
    use narsor_engine::orchestration::{ControlPlane, OwnerCommand, RunnerState};
    use narsor_engine::registry::RepositoryRegistry;
    use narsor_engine::workflows::WorkflowResolver;

    async fn start(root: &TempRoot) -> api::ApiHandle {
        api::start_with_services(
            Arc::new(Engine::new(root.store())),
            root.path().to_path_buf(),
            root.path().to_path_buf(),
            api::ApiServices {
                control: Some(Arc::new(
                    narsor_engine::controller_protocol::ProtocolService::new(
                        ControlPlane::new(StatePaths::new(root.path())),
                        environment_support::seeded_resolver(root.path()),
                    ),
                )),
                workflows: Some(WorkflowResolver::with_user_dir(root.path(), None)),
                ..api::ApiServices::default()
            },
        )
        .await
        .expect("server starts")
    }

    fn all_fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    fn repository(id: &str, remote: &str) -> CreateRepositoryRequest {
        CreateRepositoryRequest {
            id: id.to_string(),
            remote: remote.to_string(),
            name: None,
            provider: None,
            default_branch: None,
            description: None,
        }
    }

    #[tokio::test]
    async fn the_catalogue_lists_built_ins_with_description_and_inputs() {
        let root = TempRoot::new();
        let handle = start(&root).await;
        let list = handle.client().control_workflows().await.expect("list");
        let workflow = list
            .workflows
            .iter()
            .find(|workflow| workflow.name == "template")
            .expect("built-in listed");
        assert_eq!(workflow.source, WorkflowSource::BuiltIn);
        assert!(workflow.valid, "{:?}", workflow.violations);
        assert_eq!(workflow.version, Some(1));
        assert!(workflow
            .description
            .as_deref()
            .is_some_and(|text| text.starts_with("A safe, runnable tour")));
        let input = workflow
            .inputs
            .iter()
            .find(|input| input.name == "example_text")
            .expect("input declared");
        assert!(input.required);
        assert!(input.description.is_some());
        assert_eq!(workflow.inputs.len(), 1);
        // Never a path of the server.
        assert!(!serde_json::to_string(&list)
            .unwrap()
            .contains(root.path().to_str().unwrap()));
    }

    #[tokio::test]
    async fn a_catalogue_workflow_that_does_not_load_stays_listed_with_its_violations() {
        let root = TempRoot::new();
        fs::create_dir_all(root.join("workflows")).unwrap();
        fs::copy(
            all_fixtures().join("invalid/five_violations.yaml"),
            root.join("workflows/broken.yaml"),
        )
        .unwrap();
        let handle = start(&root).await;
        let list = handle.client().control_workflows().await.expect("list");
        let broken = list
            .workflows
            .iter()
            .find(|workflow| workflow.name == "broken")
            .expect("listed");
        assert_eq!(broken.source, WorkflowSource::Catalog);
        assert!(!broken.valid);
        assert_eq!(broken.violations.len(), 5);
        assert!(broken.inputs.is_empty());
    }

    #[tokio::test]
    async fn validating_a_path_reports_every_violation() {
        let root = TempRoot::new();
        let handle = start(&root).await;
        let client = handle.client();
        let verdict = client
            .control_validate_workflow(
                all_fixtures()
                    .join("invalid/five_violations.yaml")
                    .to_str()
                    .unwrap(),
            )
            .await
            .expect("verdict");
        assert!(!verdict.valid);
        assert_eq!(verdict.violations.len(), 5, "{:?}", verdict.violations);

        let valid = client
            .control_validate_workflow(
                all_fixtures()
                    .join("implement_feature.yaml")
                    .to_str()
                    .unwrap(),
            )
            .await
            .expect("verdict");
        assert!(valid.valid, "{:?}", valid.violations);
        assert!(valid.name.is_some());

        for (path, code) in [
            (root.join("missing.yaml"), "workflow_not_found"),
            (root.path().to_path_buf(), "workflow_not_yaml"),
            (root.join("notes.txt"), "workflow_not_yaml"),
        ] {
            match client
                .control_validate_workflow(path.to_str().unwrap())
                .await
            {
                Err(ClientError::Request { code: got, .. }) => assert_eq!(got, code),
                other => panic!("{path:?}: {other:?}"),
            }
        }
        fs::create_dir_all(root.join("dir.yaml")).unwrap();
        match client
            .control_validate_workflow(root.join("dir.yaml").to_str().unwrap())
            .await
        {
            Err(ClientError::Request { code, .. }) => assert_eq!(code, "workflow_not_a_file"),
            other => panic!("{other:?}"),
        }
        fs::write(root.join("big.yaml"), vec![b' '; 2 * 1024 * 1024]).unwrap();
        match client
            .control_validate_workflow(root.join("big.yaml").to_str().unwrap())
            .await
        {
            Err(ClientError::Request { code, .. }) => assert_eq!(code, "workflow_too_large"),
            other => panic!("{other:?}"),
        }
        fs::write(root.join("garbage.yaml"), "not: [a workflow").unwrap();
        let garbage = client
            .control_validate_workflow(root.join("garbage.yaml").to_str().unwrap())
            .await
            .expect("a verdict, not an error");
        assert!(!garbage.valid && garbage.violations.len() == 1);
    }

    #[tokio::test]
    async fn repositories_are_added_listed_and_removed_through_the_api() {
        let root = TempRoot::new();
        let handle = start(&root).await;
        let client = handle.client();
        assert!(client
            .control_repositories()
            .await
            .unwrap()
            .repositories
            .is_empty());

        let added = client
            .control_add_repository(&repository(
                "acme",
                "https://user:secret@example.com/acme.git",
            ))
            .await
            .expect("added");
        assert_eq!(added.provider, "git");
        assert_eq!(
            added.remote, "https://example.com/acme.git",
            "no credentials"
        );
        client
            .control_add_repository(&repository("local-one", "/srv/git/local.git"))
            .await
            .expect("a path is fine on the local daemon");

        // What `narsor repo list` reads.
        let registered = RepositoryRegistry::new(root.path()).list().unwrap();
        assert_eq!(registered.entries.len(), 2);
        let listed = client.control_repositories().await.unwrap();
        assert_eq!(
            listed
                .repositories
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["acme", "local-one"]
        );

        match client
            .control_add_repository(&repository("acme", "https://example.com/other.git"))
            .await
        {
            Err(ClientError::Refused { code, .. }) => assert_eq!(code, "repository_exists"),
            other => panic!("{other:?}"),
        }
        for bad in [
            repository("bad", "--upload-pack=evil"),
            repository("bad", "ext::sh -c evil"),
            repository("../escape", "https://example.com/x.git"),
            repository("bad\u{1b}", "https://example.com/x.git"),
        ] {
            assert!(
                matches!(
                    client.control_add_repository(&bad).await,
                    Err(ClientError::Request { .. })
                ),
                "{bad:?}"
            );
        }

        let removed = client.control_remove_repository("acme").await.unwrap();
        assert_eq!(removed.id, "acme");
        assert!(matches!(
            client.control_remove_repository("acme").await,
            Err(ClientError::NotFound)
        ));
        assert!(matches!(
            client.control_remove_repository("never-registered").await,
            Err(ClientError::NotFound)
        ));
        assert_eq!(
            RepositoryRegistry::new(root.path())
                .list()
                .unwrap()
                .entries
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn a_corrupt_registry_file_does_not_hide_the_other_repositories() {
        let root = TempRoot::new();
        let handle = start(&root).await;
        let client = handle.client();
        client
            .control_add_repository(&repository("fine", "https://example.com/f.git"))
            .await
            .unwrap();
        fs::write(root.join("repositories/broken.yaml"), ": : :").unwrap();
        let listed = client.control_repositories().await.unwrap();
        assert_eq!(listed.repositories.len(), 1);
        assert_eq!(listed.unreadable.len(), 1);
        assert!(listed.unreadable[0].starts_with("broken.yaml"));
    }

    #[tokio::test]
    async fn history_lists_archived_tasks_newest_first_and_says_their_storage_is_gone() {
        let root = TempRoot::new();
        let handle = start(&root).await;
        let client = handle.client();
        assert!(client
            .control_tasks_archived(true)
            .await
            .unwrap()
            .tasks
            .is_empty());

        for id in ["task-a", "task-b", "task-live"] {
            client
                .create_control_task(&remote_task(id, "k8s"))
                .await
                .expect("created");
        }
        for id in ["task-a", "task-b"] {
            client
                .control_command(
                    id,
                    &OwnerCommandRequest {
                        command: OwnerCommand::Archive,
                        idempotency_key: format!("archive-{id}"),
                        expected_generation: None,
                    },
                )
                .await
                .expect("archived");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let archived = client.control_tasks_archived(true).await.unwrap();
        let ids: Vec<_> = archived.tasks.iter().map(|t| t.task_id.as_str()).collect();
        assert_eq!(ids, ["task-b", "task-a"], "newest archive first");
        assert!(archived
            .tasks
            .iter()
            .all(|task| task.runner_state == RunnerState::Archived));
        let rest = client.control_tasks_archived(false).await.unwrap();
        assert_eq!(rest.tasks.len(), 1);
        assert_eq!(rest.tasks[0].task_id.as_str(), "task-live");
        assert_eq!(
            client.control_tasks().await.unwrap().tasks.len(),
            3,
            "unfiltered as before"
        );

        // The record and the control-plane log remain; the storage-backed
        // tabs say why they are empty.
        assert!(!client.control_events("task-a", 0).await.unwrap().is_empty());
        match client.control_conversation("task-a").await.unwrap() {
            ConversationResponse::Unavailable { reason, .. } => {
                assert_eq!(
                    reason,
                    narsor_engine::controller_protocol::ConversationUnavailableReason::Archived
                )
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn history_reads_an_old_execution_and_refuses_to_recover_a_finished_one() {
        let root = TempRoot::new();
        seed_execution_with_retry(&root.store(), "exec-old");
        let handle = start(&root).await;
        let client = handle.client();

        let capabilities = client.control_capabilities().await.unwrap();
        assert_eq!(capabilities.connection_name, "local");
        assert!(capabilities.workflow_paths);
        assert!(capabilities.workflow_paths_reason.is_none());
        assert!(capabilities.legacy_executions);
        assert!(capabilities.local_repository_paths);

        let history = client.control_history().await.unwrap();
        assert_eq!(history.executions.len(), 1);
        let row = client.control_history_execution("exec-old").await.unwrap();
        assert_eq!(row.status, ExecutionStatus::Completed);
        assert!(!row.recoverable);
        let json = serde_json::to_string(&history).unwrap();
        assert!(!json.contains("definition_path") && !json.contains("workspace"));

        assert_eq!(
            client
                .control_history_events("exec-old", 4)
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(!client
            .control_history_timeline("exec-old")
            .await
            .unwrap()
            .positions
            .is_empty());
        assert!(matches!(
            client
                .control_history_conversation("exec-old", "attempt-1", 1)
                .await
                .unwrap(),
            narsor::api::ConversationResponse::Recorded { .. }
        ));
        assert!(client.control_history_conductor("exec-old").await.is_ok());
        assert!(client
            .control_history_conductor_chat("exec-old", 0)
            .await
            .unwrap()
            .entries
            .is_empty());

        match client.control_recover_execution("exec-old", None).await {
            Err(ClientError::Refused { code, message }) => {
                assert_eq!(code, "not_recoverable");
                assert!(message.contains("completed"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        for unknown in ["nope", "exec-old-x"] {
            assert!(matches!(
                client.control_history_execution(unknown).await,
                Err(ClientError::NotFound)
            ));
            assert!(matches!(
                client.control_recover_execution(unknown, None).await,
                Err(ClientError::NotFound)
            ));
        }
    }

    #[tokio::test]
    async fn an_empty_history_is_an_empty_list() {
        let root = TempRoot::new();
        let handle = start(&root).await;
        let client = handle.client();
        assert!(client
            .control_history()
            .await
            .unwrap()
            .executions
            .is_empty());
        assert!(
            !client
                .control_capabilities()
                .await
                .unwrap()
                .legacy_executions
        );
    }

    #[tokio::test]
    async fn the_new_routes_refuse_a_missing_or_wrong_token() {
        let root = TempRoot::new();
        let handle = start(&root).await;
        let raw = raw_client();
        for path in [
            "/control/v1/capabilities",
            "/control/v1/workflows",
            "/control/v1/repositories",
            "/control/v1/executions",
            "/control/v1/tasks?archived=true",
        ] {
            let bare = raw.get(url(&handle, path)).send().await.unwrap();
            assert_eq!(bare.status(), reqwest::StatusCode::UNAUTHORIZED, "{path}");
            let wrong = raw
                .get(url(&handle, path))
                .bearer_auth("wrong")
                .send()
                .await
                .unwrap();
            assert_eq!(wrong.status(), reqwest::StatusCode::UNAUTHORIZED, "{path}");
        }

        for token in [None, Some("wrong")] {
            let mut request = raw
                .post(url(&handle, "/control/v1/workflows/source"))
                .json(&serde_json::json!({ "name": "example", "builtin": false }));
            if let Some(token) = token {
                request = request.bearer_auth(token);
            }
            let response = request.send().await.unwrap();
            assert_eq!(
                response.status(),
                reqwest::StatusCode::UNAUTHORIZED,
                "workflow source token={token:?}"
            );
        }
    }
}
