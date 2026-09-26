//! Integration tests for the embedded API server (U5): the server, over a
//! real temporary store and a real bound loopback port, answering the three
//! read-only endpoints through the real `ApiClient`.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use chrono::Utc;
use narsor::api;
use narsor::api::ClientError;
use narsor::domain::{
    Context, Event, EventData, ExecutionStatus, RepositoryInfo, WorkflowExecution, Workspace,
};
use narsor::engine::{ConversationState, PositionStatus};
use narsor::persistence::conversation::ConversationSource;
use narsor::persistence::ExecutionStore;

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
    first.append_narsor("prompt: try the thing").expect("append prompt 1");
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

async fn start_server(store: ExecutionStore) -> api::ApiHandle {
    api::start(store).await.expect("server starts")
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
    assert_eq!(timeline.positions[0].conversation, ConversationState::Recorded);
    assert_eq!(timeline.positions[1].conversation, ConversationState::Recorded);
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

    let url = format!(
        "http://{}/executions/exec-1/timeline",
        handle.addr()
    );

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
