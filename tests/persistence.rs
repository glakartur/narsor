//! Integration tests for the filesystem persistence layer (U3).
//!
//! Every test runs against a real temporary directory: the whole value of this
//! layer lies in real file operations, so nothing here is mocked.

use chrono::Utc;
use narsor_engine::domain::{
    Context, Event, EventData, ExecutionStatus, RepositoryInfo, WorkflowExecution, Workspace,
    WorkspaceType,
};
use narsor_engine::persistence::atomic::{read_json, write_json_atomic};
use narsor_engine::persistence::paths::{event_file_name, temp_path_for};
use narsor_engine::persistence::{EventLog, ExecutionStore, PersistenceError};
use std::fs;
use std::path::{Path, PathBuf};

/// A temporary directory that deletes itself when it goes out of scope.
///
/// Stands in for the `tempfile` crate, which is not a dependency of this crate.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("narsor-persistence-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
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

fn sample_execution(id: &str) -> WorkflowExecution {
    let mut context = Context::new();
    context.update_variables(
        serde_json::json!({ "pr_number": 123, "branch": "feature/login" })
            .as_object()
            .unwrap()
            .clone(),
    );

    WorkflowExecution {
        id: id.to_string(),
        workflow_id: "implement-feature".to_string(),
        workflow_version: 3,
        status: ExecutionStatus::Paused,
        current_state: Some("implement".to_string()),
        started_at: Utc::now(),
        completed_at: None,
        inputs: serde_json::json!({ "description": "login" }),
        context,
        workspace: Workspace::git_checkout(
            "/tmp/workspace-42".to_string(),
            RepositoryInfo {
                id: "repo-1".to_string(),
                name: "narsor".to_string(),
                provider: "gitlab".to_string(),
                remote: "git@example.com:team/narsor.git".to_string(),
            },
            Some("main".to_string()),
        ),
        current_action_execution: Some("action-7".to_string()),
        metadata: serde_json::json!({ "triggered_by": "cli" }),
    }
}

#[test]
fn step_replies_are_stored_as_atomic_attempt_files() {
    let root = TempRoot::new();
    let store = root.store();
    store
        .write_step_reply("exec-1", "attempt-2", "review prose")
        .unwrap();
    assert_eq!(
        store.read_step_reply("exec-1", "attempt-2").unwrap(),
        "review prose"
    );
    assert!(!root
        .path()
        .join("executions/exec-1/step-replies/attempt-1.txt")
        .exists());
}

fn sample_event(execution_id: &str, sequence: u64, state_id: &str) -> Event {
    Event::new(
        execution_id.to_string(),
        EventData::StateEntered {
            state_id: state_id.to_string(),
        },
        sequence,
    )
}

// --- Happy paths ---------------------------------------------------------

#[test]
fn saving_and_loading_an_execution_preserves_every_field() {
    let root = TempRoot::new();
    let store = root.store();
    let execution = sample_execution("exec-roundtrip");

    store.save_execution(&execution).expect("save execution");
    let loaded = store.load_execution(&execution.id).expect("load execution");

    assert_eq!(loaded.id, execution.id);
    assert_eq!(loaded.workflow_id, execution.workflow_id);
    assert_eq!(loaded.workflow_version, execution.workflow_version);
    assert_eq!(loaded.status, ExecutionStatus::Paused);
    assert_eq!(loaded.current_state.as_deref(), Some("implement"));
    assert_eq!(loaded.started_at, execution.started_at);
    assert_eq!(loaded.completed_at, None);
    assert_eq!(loaded.inputs, execution.inputs);
    assert_eq!(loaded.metadata, execution.metadata);
    assert_eq!(loaded.current_action_execution.as_deref(), Some("action-7"));

    // Context survives in full.
    assert_eq!(
        loaded.context.get_variable("pr_number"),
        Some(&serde_json::json!(123))
    );
    assert_eq!(
        loaded.context.get_variable("branch"),
        Some(&serde_json::json!("feature/login"))
    );

    // Workspace survives in full.
    assert!(matches!(
        loaded.workspace.workspace_type,
        WorkspaceType::GitCheckout
    ));
    assert_eq!(loaded.workspace.id, execution.workspace.id);
    assert_eq!(loaded.workspace.path, "/tmp/workspace-42");
    assert_eq!(loaded.workspace.git_ref.as_deref(), Some("main"));
    assert!(!loaded.workspace.read_only);
    let repository = loaded.workspace.repository.expect("repository preserved");
    assert_eq!(repository.name, "narsor");
    assert_eq!(repository.provider, "gitlab");
    assert_eq!(repository.remote, "git@example.com:team/narsor.git");
}

#[test]
fn appending_three_events_reads_them_back_in_order() {
    let root = TempRoot::new();
    let store = root.store();
    let log = store.event_log("exec-events");

    log.append(&sample_event("exec-events", 1, "analyze"))
        .expect("append 1");
    log.append(&sample_event("exec-events", 2, "implement"))
        .expect("append 2");
    log.append(&sample_event("exec-events", 3, "test"))
        .expect("append 3");

    let events = log.read_all().expect("read log");
    let sequences: Vec<u64> = events.iter().map(|event| event.sequence).collect();
    assert_eq!(sequences, vec![1, 2, 3]);

    let states: Vec<String> = events
        .iter()
        .map(|event| match &event.data {
            EventData::StateEntered { state_id } => state_id.clone(),
            other => panic!("unexpected event data: {other:?}"),
        })
        .collect();
    assert_eq!(states, vec!["analyze", "implement", "test"]);
    assert_eq!(log.next_sequence().expect("next sequence"), 4);
}

#[test]
fn append_next_allocates_consecutive_sequence_numbers() {
    let root = TempRoot::new();
    let store = root.store();
    let execution_id = "exec-append-next".to_string();
    let log = store.event_log(&execution_id);

    let first = log
        .append_next(&execution_id, EventData::WorkflowResumed)
        .expect("append first");
    let second = log
        .append_next(
            &execution_id,
            EventData::WorkflowPaused {
                reason: "human input".to_string(),
            },
        )
        .expect("append second");

    assert_eq!(first.sequence, 1);
    assert_eq!(second.sequence, 2);
    assert_eq!(log.read_all().expect("read log").len(), 2);
}

/// An event written before `action_execution_id`, `state_id`,
/// `harness_session_id` and `resumed_session` existed on the agent/retry
/// events still deserializes, with every new field defaulting to `None`
/// (U1): old logs on disk must keep working after an upgrade.
#[test]
fn an_old_format_agent_session_event_deserializes_with_the_new_fields_absent() {
    let old_format = serde_json::json!({
        "id": "event-1",
        "workflow_execution_id": "exec-old-format",
        "timestamp": Utc::now().to_rfc3339(),
        "sequence": 1,
        "data": {
            "type": "agent_session_started",
            "session_id": "session-1",
            "harness": "claude-code"
        }
    });

    let event: Event = serde_json::from_value(old_format).expect("deserialize the old event");
    match event.data {
        EventData::AgentSessionStarted {
            session_id,
            harness,
            action_execution_id,
            resumed_session,
        } => {
            assert_eq!(session_id, "session-1");
            assert_eq!(harness, "claude-code");
            assert_eq!(action_execution_id, None);
            assert_eq!(resumed_session, None);
        }
        other => panic!("unexpected event data: {other:?}"),
    }
}

/// Same guarantee for `HumanInputRequested`, which carries the preceding
/// step's reason and required values instead of the agent's question since
/// the agent contract plan's U4 (KTD16).
#[test]
fn an_old_format_human_input_requested_event_deserializes_with_no_reason() {
    let old_format = serde_json::json!({
        "id": "event-3",
        "workflow_execution_id": "exec-old-format",
        "timestamp": Utc::now().to_rfc3339(),
        "sequence": 3,
        "data": {
            "type": "human_input_requested",
            "instruction": "Approve?",
            "question": "Account or user limit?"
        }
    });

    let event: Event = serde_json::from_value(old_format).expect("deserialize the old event");
    match event.data {
        EventData::HumanInputRequested {
            instruction,
            reason,
            required_values,
            answer_tail,
            episode,
        } => {
            assert_eq!(instruction, "Approve?");
            assert_eq!(reason, None);
            assert!(required_values.is_empty());
            assert_eq!(answer_tail, None);
            assert_eq!(episode, None);
        }
        other => panic!("unexpected event data: {other:?}"),
    }
}

/// Same guarantee for `HumanInputReceived`, which says who closed the input
/// and how since the conductor plan's U5.
#[test]
fn an_old_format_human_input_received_event_deserializes_as_a_plain_answer() {
    let old_format = serde_json::json!({
        "id": "event-4",
        "workflow_execution_id": "exec-old-format",
        "timestamp": Utc::now().to_rfc3339(),
        "sequence": 4,
        "data": {
            "type": "human_input_received",
            "input": {"approval_status": "approved"}
        }
    });

    let event: Event = serde_json::from_value(old_format).expect("deserialize the old event");
    match &event.data {
        EventData::HumanInputReceived {
            input,
            answered_by,
            close_reason,
            episode,
            output_label,
            artifacts,
            variables,
            comment,
        } => {
            assert_eq!(input, &serde_json::json!({"approval_status": "approved"}));
            assert!(answered_by.is_none() && close_reason.is_none() && episode.is_none());
            assert!(output_label.is_none() && artifacts.is_none() && variables.is_none());
            assert!(comment.is_none());
        }
        other => panic!("unexpected event data: {other:?}"),
    }
    assert_eq!(
        event.description(),
        "Human input received from operator: approval_status"
    );
}

/// Same guarantee for `ActionRetrying`, whose `state_id` is new in U1.
#[test]
fn an_old_format_action_retrying_event_deserializes_with_no_state_id() {
    let old_format = serde_json::json!({
        "id": "event-2",
        "workflow_execution_id": "exec-old-format",
        "timestamp": Utc::now().to_rfc3339(),
        "sequence": 2,
        "data": {
            "type": "action_retrying",
            "action_execution_id": "action-1",
            "retry_count": 1
        }
    });

    let event: Event = serde_json::from_value(old_format).expect("deserialize the old event");
    match event.data {
        EventData::ActionRetrying {
            action_execution_id,
            retry_count,
            state_id,
        } => {
            assert_eq!(action_execution_id, "action-1");
            assert_eq!(retry_count, 1);
            assert_eq!(state_id, None);
        }
        other => panic!("unexpected event data: {other:?}"),
    }
}

/// A second, independent `EventLog` handle on the same directory — which now
/// legitimately happens when a background TUI run's own error is emitted
/// concurrently with the pass it was rejected in favour of still writing its
/// own events — starts its own next-sequence cache from its own directory
/// listing, so both handles can compute the same "next" number. The retry in
/// `append_next` claims the next free slot instead of failing outright.
#[test]
fn append_next_retries_past_a_sequence_a_second_handle_already_claimed() {
    let root = TempRoot::new();
    let store = root.store();
    let execution_id = "exec-append-next-collision".to_string();

    // Two handles on the same log, exactly as two different `Engine` method
    // calls each get their own via `ExecutionStore::event_log`.
    let first_handle = store.event_log(&execution_id);
    let second_handle = store.event_log(&execution_id);

    let first = first_handle
        .append_next(&execution_id, EventData::WorkflowResumed)
        .expect("append via the first handle");
    assert_eq!(first.sequence, 1);

    // The second handle's cache still thinks 1 is free: it has never
    // written through this handle before.
    let second = second_handle
        .append_next(
            &execution_id,
            EventData::WorkflowPaused {
                reason: "human input".to_string(),
            },
        )
        .expect("append via the second handle, past the collision");
    assert_eq!(second.sequence, 2, "it claimed the next free slot instead");

    let events = first_handle.read_all().expect("read the log");
    assert_eq!(events.len(), 2, "both events landed, none silently lost");
}

#[test]
fn directory_structure_is_created_on_first_write() {
    let root = TempRoot::new();
    let store = root.store();
    let execution = sample_execution("exec-mkdir");

    // No mkdir by the caller: not even the root's `executions/` exists yet.
    assert!(!store.paths().executions_dir().exists());

    store.save_execution(&execution).expect("save execution");

    let paths = store.paths();
    assert!(paths.execution_dir("exec-mkdir").is_dir());
    assert!(paths.state_file("exec-mkdir").is_file());
    assert!(paths.context_file("exec-mkdir").is_file());
    assert!(paths.events_dir("exec-mkdir").is_dir());
    assert!(paths.artifacts_dir("exec-mkdir").is_dir());

    let artifact_dir = store
        .artifact_dir(&execution.id, "artifact-9")
        .expect("artifact dir");
    assert!(artifact_dir.is_dir());
    assert_eq!(
        artifact_dir,
        paths.artifacts_dir("exec-mkdir").join("artifact-9")
    );

    assert_eq!(
        store.list_execution_ids().expect("list"),
        vec!["exec-mkdir".to_string()]
    );
}

// --- Edge cases ----------------------------------------------------------

#[test]
fn reading_an_empty_events_directory_returns_an_empty_list() {
    let root = TempRoot::new();
    let store = root.store();
    store
        .create_execution_dirs("exec-empty")
        .expect("create dirs");

    let log = store.event_log("exec-empty");
    assert!(log.dir().is_dir());
    assert!(log.read_all().expect("read empty log").is_empty());
    assert_eq!(log.last_sequence().expect("last sequence"), None);
    assert_eq!(log.next_sequence().expect("next sequence"), 1);
}

#[test]
fn reading_a_missing_events_directory_returns_an_empty_list() {
    let root = TempRoot::new();
    let log = EventLog::new(root.path().join("executions/absent/events"));
    assert!(log.read_all().expect("read missing log").is_empty());
}

#[test]
fn context_with_empty_variables_object_deserializes_to_an_empty_map() {
    let root = TempRoot::new();
    let store = root.store();
    store
        .create_execution_dirs("exec-empty-ctx")
        .expect("create dirs");

    fs::write(
        store.paths().context_file("exec-empty-ctx"),
        r#"{ "variables": {} }"#,
    )
    .expect("write context");

    let context = store.load_context("exec-empty-ctx").expect("load context");
    assert!(context.variables.is_empty());
    assert!(context.artifacts.is_empty());
    assert!(context.repositories.is_empty());
    assert!(context.task.is_none());
    assert!(context.workspace.is_none());
}

// --- Error paths ---------------------------------------------------------

#[test]
fn appending_an_existing_sequence_fails_and_leaves_the_file_untouched() {
    let root = TempRoot::new();
    let store = root.store();
    let log = store.event_log("exec-collision");

    log.append(&sample_event("exec-collision", 1, "analyze"))
        .expect("append 1");
    log.append(&sample_event("exec-collision", 2, "implement"))
        .expect("append 2");

    let path = log.event_path(2);
    let before = fs::read_to_string(&path).expect("read existing event");

    let colliding = sample_event("exec-collision", 2, "SHOULD-NOT-BE-WRITTEN");
    let error = log
        .append(&colliding)
        .expect_err("collision must be an error");

    match &error {
        PersistenceError::EventAlreadyExists { sequence, path: p } => {
            assert_eq!(*sequence, 2);
            assert_eq!(p, &path);
        }
        other => panic!("unexpected error: {other:?}"),
    }
    assert!(error.to_string().contains(&path.display().to_string()));

    let after = fs::read_to_string(&path).expect("read event again");
    assert_eq!(before, after, "existing event file was modified");

    // And no temporary file was left behind by the rejected append.
    assert!(!temp_path_for(&path).exists());
}

#[test]
fn a_gap_in_the_numbering_is_reported_as_corruption() {
    let root = TempRoot::new();
    let store = root.store();
    let log = store.event_log("exec-gap");

    for sequence in [1_u64, 2, 4] {
        log.append(&sample_event("exec-gap", sequence, "state"))
            .expect("append");
    }

    let error = log.read_all().expect_err("gap must be an error");
    match error {
        PersistenceError::SequenceGap {
            expected, found, ..
        } => {
            assert_eq!(expected, 3);
            assert_eq!(found, 4);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn an_event_file_contradicting_its_name_is_reported() {
    let root = TempRoot::new();
    let store = root.store();
    let log = store.event_log("exec-mismatch");
    log.ensure_dir().expect("create events dir");

    // File 00000001.json declaring sequence 7.
    write_json_atomic(
        &log.event_path(1),
        &sample_event("exec-mismatch", 7, "analyze"),
    )
    .expect("write event");

    let error = log.read_all().expect_err("mismatch must be an error");
    match error {
        PersistenceError::SequenceMismatch {
            expected, declared, ..
        } => {
            assert_eq!(expected, 1);
            assert_eq!(declared, 7);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn a_truncated_state_file_yields_a_readable_error_naming_the_file() {
    let root = TempRoot::new();
    let store = root.store();
    let execution = sample_execution("exec-truncated");
    store.save_execution(&execution).expect("save execution");

    let path = store.paths().state_file("exec-truncated");
    let complete = fs::read_to_string(&path).expect("read state");
    fs::write(&path, &complete[..complete.len() / 2]).expect("truncate state");

    let error = store
        .load_execution("exec-truncated")
        .expect_err("truncated state must be an error");

    match &error {
        PersistenceError::Deserialize { path: p, .. } => assert_eq!(p, &path),
        other => panic!("unexpected error: {other:?}"),
    }
    let message = error.to_string();
    assert!(
        message.contains("state.json"),
        "error message does not name the file: {message}"
    );
}

#[test]
fn loading_a_missing_execution_names_the_expected_path() {
    let root = TempRoot::new();
    let store = root.store();

    let error = store
        .load_execution("nope")
        .expect_err("missing execution must be an error");
    match error {
        PersistenceError::ExecutionNotFound { execution_id, path } => {
            assert_eq!(execution_id, "nope");
            assert_eq!(path, store.paths().state_file("nope"));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

// --- Integration ---------------------------------------------------------

#[test]
fn cleanup_removes_an_orphaned_temp_file_and_leaves_the_real_file_intact() {
    let root = TempRoot::new();
    let store = root.store();
    let execution = sample_execution("exec-orphan");
    store.save_execution(&execution).expect("save execution");

    let state_path = store.paths().state_file("exec-orphan");
    let state_before = fs::read_to_string(&state_path).expect("read state");

    // Simulate a crash between the write and the rename.
    let orphan = temp_path_for(&state_path);
    fs::write(&orphan, b"{\"id\":\"half-written").expect("write orphan");
    let event_orphan =
        store
            .paths()
            .events_dir("exec-orphan")
            .join(format!("{}{}", event_file_name(5), ".tmp"));
    fs::write(&event_orphan, b"{").expect("write event orphan");
    assert!(orphan.exists() && event_orphan.exists());

    let removed = store.cleanup_orphaned_temp_files().expect("cleanup");

    assert!(!orphan.exists(), "orphaned state temp file survived");
    assert!(!event_orphan.exists(), "orphaned event temp file survived");
    assert!(removed.contains(&orphan));
    assert!(removed.contains(&event_orphan));

    assert_eq!(
        fs::read_to_string(&state_path).expect("read state again"),
        state_before,
        "state.json was disturbed by the cleanup"
    );
    assert_eq!(
        store.load_execution("exec-orphan").expect("load").id,
        "exec-orphan"
    );
}

#[test]
fn two_successive_context_writes_leave_no_temp_file_behind() {
    let root = TempRoot::new();
    let store = root.store();
    let mut execution = sample_execution("exec-context");
    store.save_execution(&execution).expect("first save");

    execution.context.update_variables(
        serde_json::json!({ "pr_number": 456 })
            .as_object()
            .unwrap()
            .clone(),
    );
    store
        .save_context(&execution.id, &execution.context)
        .expect("second save");

    let dir = store.execution_dir("exec-context");
    let leftovers: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("read execution dir")
        .map(|entry| entry.expect("dir entry").path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".tmp"))
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {leftovers:?}"
    );

    let context = store.load_context("exec-context").expect("load context");
    assert_eq!(
        context.get_variable("pr_number"),
        Some(&serde_json::json!(456))
    );
}

/// The verification bar of U3: an interrupted write leaves the previous
/// version of the file readable and complete.
#[test]
fn an_interrupted_write_leaves_the_previous_version_complete() {
    let root = TempRoot::new();
    let store = root.store();
    let execution = sample_execution("exec-interrupted");
    store.save_execution(&execution).expect("save execution");

    let state_path = store.paths().state_file("exec-interrupted");

    // A write that got as far as the temp file but never reached the rename.
    let mut half_written = sample_execution("exec-interrupted");
    half_written.status = ExecutionStatus::Completed;
    let temp = temp_path_for(&state_path);
    let serialized = serde_json::to_vec_pretty(&half_written).expect("serialize");
    fs::write(&temp, &serialized[..serialized.len() / 3]).expect("write temp");

    // The previous version is still complete and still the one that is read.
    let loaded: WorkflowExecution = read_json(&state_path).expect("read previous version");
    assert_eq!(loaded.status, ExecutionStatus::Paused);
    assert_eq!(loaded.current_state.as_deref(), Some("implement"));
    assert_eq!(
        loaded.context.get_variable("pr_number"),
        Some(&serde_json::json!(123))
    );

    // And the leftover is garbage that cleanup discards.
    store.cleanup_orphaned_temp_files().expect("cleanup");
    assert!(!temp.exists());
    assert_eq!(
        store
            .load_execution("exec-interrupted")
            .expect("load")
            .status,
        ExecutionStatus::Paused
    );
}

#[test]
fn a_temp_file_in_the_events_directory_is_not_read_as_an_event() {
    let root = TempRoot::new();
    let store = root.store();
    let log = store.event_log("exec-temp-event");
    log.append(&sample_event("exec-temp-event", 1, "analyze"))
        .expect("append");

    fs::write(temp_path_for(&log.event_path(2)), b"{ not json").expect("write temp event");

    let events = log.read_all().expect("read log");
    assert_eq!(events.len(), 1);
    assert_eq!(log.next_sequence().expect("next sequence"), 2);
}

// ---------------------------------------------------------------------------
// The three roots
// ---------------------------------------------------------------------------

/// AE2 — splitting one root into three moves nothing.
///
/// The whole layout, asserted against the literal paths the code produced
/// before the split. This is the test that makes "the change is invisible
/// until someone separates the roots deliberately" a fact rather than an
/// intention, so it is written against string literals on purpose: deriving
/// the expectation from the same constants the implementation uses would
/// assert nothing at all.
#[test]
fn the_three_roots_on_one_directory_reproduce_the_previous_layout() {
    use narsor_engine::persistence::{CatalogPaths, StatePaths, WorkPaths};

    let root = Path::new("/srv/narsor");
    let state = StatePaths::new(root);
    let catalog = CatalogPaths::new(root);
    let work = WorkPaths::new(root);

    assert_eq!(state.executions_dir(), Path::new("/srv/narsor/executions"));
    assert_eq!(
        state.state_file("exec-1"),
        Path::new("/srv/narsor/executions/exec-1/state.json")
    );
    assert_eq!(
        state.context_file("exec-1"),
        Path::new("/srv/narsor/executions/exec-1/context.json")
    );
    assert_eq!(
        state.events_dir("exec-1"),
        Path::new("/srv/narsor/executions/exec-1/events")
    );
    assert_eq!(
        state.event_file("exec-1", 7),
        Path::new("/srv/narsor/executions/exec-1/events/00000007.json")
    );
    assert_eq!(
        state.artifacts_dir("exec-1"),
        Path::new("/srv/narsor/executions/exec-1/artifacts")
    );
    assert_eq!(state.log_file(), Path::new("/srv/narsor/narsor.log"));

    assert_eq!(catalog.workflows_dir(), Path::new("/srv/narsor/workflows"));
    assert_eq!(
        catalog.repositories_dir(),
        Path::new("/srv/narsor/repositories")
    );

    assert_eq!(work.tasks_dir(), Path::new("/srv/narsor/tasks"));
    assert_eq!(
        work.task_dir("exec-1"),
        Path::new("/srv/narsor/tasks/exec-1")
    );
    assert_eq!(
        work.code_dir("exec-1"),
        Path::new("/srv/narsor/tasks/exec-1/code")
    );
    assert_eq!(
        work.checkout_dir("exec-1", "acme-api"),
        Path::new("/srv/narsor/tasks/exec-1/code/acme-api")
    );
}

/// R2 — all three share one built-in value, so an unconfigured run is unmoved.
#[test]
fn all_three_roots_default_to_the_same_directory() {
    use narsor_engine::persistence::{CatalogPaths, StatePaths, WorkPaths, DEFAULT_ROOT};

    assert_eq!(StatePaths::default().root(), Path::new(DEFAULT_ROOT));
    assert_eq!(CatalogPaths::default().root(), Path::new(DEFAULT_ROOT));
    assert_eq!(WorkPaths::default().root(), Path::new(DEFAULT_ROOT));
    assert_eq!(DEFAULT_ROOT, "./.narsor");
}

#[test]
fn workspace_without_a_base_commit_still_reads() {
    // A state.json written before this field existed must keep loading:
    // an execution in flight when the binary is upgraded is the normal case.
    let json = serde_json::json!({
        "id": "ws-1",
        "workspace_type": "local_folder",
        "path": "/tmp/ws",
    });

    let workspace: narsor_engine::domain::Workspace =
        serde_json::from_value(json).expect("deserialize legacy workspace");

    assert_eq!(workspace.base_commit, None);
}

#[test]
fn workspace_round_trips_its_base_commit() {
    let mut workspace = narsor_engine::domain::Workspace::local_folder("/tmp/ws".to_string());
    workspace.base_commit = Some("0123456789abcdef0123456789abcdef01234567".to_string());

    let encoded = serde_json::to_value(&workspace).expect("serialize");
    let decoded: narsor_engine::domain::Workspace =
        serde_json::from_value(encoded).expect("deserialize");

    assert_eq!(
        decoded.base_commit.as_deref(),
        Some("0123456789abcdef0123456789abcdef01234567")
    );
}

fn sample_launch_spec(feature: &str) -> narsor_engine::domain::TaskLaunchSpec {
    narsor_engine::domain::TaskLaunchSpec::new(
        "implement-feature",
        3,
        serde_json::json!({ "feature": feature }),
        narsor_engine::domain::TaskWorkspace::None,
        narsor_engine::domain::ResolvedEnvironment::harness_only("claude-code"),
        narsor_engine::domain::ResourceProfile::new(1000, 2048),
    )
}

#[test]
fn a_launch_spec_initializes_task_storage_exactly_once() {
    let root = TempRoot::new();
    let store = root.store();
    let task_id = narsor_engine::domain::TaskId::parse("task-1").unwrap();
    let spec = sample_launch_spec("remote backends");

    assert_eq!(store.load_task_launch_spec(&task_id).unwrap(), None);
    assert!(store.initialize_task_launch_spec(&task_id, &spec).unwrap());
    assert!(!store.initialize_task_launch_spec(&task_id, &spec).unwrap());
    assert_eq!(store.load_task_launch_spec(&task_id).unwrap(), Some(spec));

    let other = sample_launch_spec("something else");
    assert!(matches!(
        store.initialize_task_launch_spec(&task_id, &other),
        Err(PersistenceError::LaunchSpecConflict { .. })
    ));
}

#[test]
fn a_launch_spec_that_does_not_match_its_digest_is_refused() {
    let root = TempRoot::new();
    let store = root.store();
    let task_id = narsor_engine::domain::TaskId::parse("task-1").unwrap();
    let mut spec = sample_launch_spec("remote backends");
    spec.inputs = serde_json::json!({ "feature": "tampered" });

    assert!(matches!(
        store.initialize_task_launch_spec(&task_id, &spec),
        Err(PersistenceError::LaunchSpecConflict { .. })
    ));
    assert_eq!(store.load_task_launch_spec(&task_id).unwrap(), None);
}

#[test]
fn launch_specs_are_not_executions_and_their_temp_files_are_swept() {
    let root = TempRoot::new();
    let store = root.store();
    let task_id = narsor_engine::domain::TaskId::parse("task-1").unwrap();
    store
        .initialize_task_launch_spec(&task_id, &sample_launch_spec("x"))
        .unwrap();
    let orphan = temp_path_for(&store.paths().task_launch_spec_file("task-2"));
    fs::write(&orphan, b"{\"torn\":").unwrap();

    assert!(store.list_execution_ids().unwrap().is_empty());
    let removed = store.cleanup_orphaned_temp_files().unwrap();
    assert_eq!(removed, vec![orphan.clone()]);
    assert!(!orphan.exists());
    assert!(store.load_task_launch_spec(&task_id).unwrap().is_some());
}
