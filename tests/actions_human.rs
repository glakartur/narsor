//! Integration tests for the `HumanAction` executor and the suspend/resume
//! mechanism (U6).
//!
//! Everything runs against a real [`ExecutionStore`] in a real temporary
//! directory, because the whole point of a human action is that the request
//! survives the process that raised it. The restart test says so literally:
//! every in-memory object is dropped and a fresh store is opened over the same
//! root.

use std::fs;
use std::path::{Path, PathBuf};

use orchestrator::domain::{
    ActionConfig, Event, EventData, ExecutionStatus, HumanActionConfig, HumanInputField,
    HumanInputType, WorkflowExecution, Workspace,
};
use orchestrator::engine::actions::human::{
    self, HumanActionError, PendingHumanInput, ResumedHumanAction,
};
use orchestrator::engine::{
    ActionExecutor, ActionOutcome, ActionRequest, HumanExecutor, HumanInputDelivery,
    SuspendedAction,
};
use orchestrator::persistence::ExecutionStore;
use serde_json::{json, Map, Value};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A temporary directory that deletes itself when it goes out of scope.
///
/// Stands in for the `tempfile` crate, which is not a dependency of this crate.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("orchestrator-human-{}", uuid::Uuid::new_v4()));
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

/// A text field.
fn text_field(name: &str, required: bool) -> HumanInputField {
    HumanInputField {
        name: name.to_string(),
        field_type: HumanInputType::Text,
        options: Vec::new(),
        required,
        description: None,
    }
}

/// A select field over `options`.
fn select_field(name: &str, options: &[&str], required: bool) -> HumanInputField {
    HumanInputField {
        name: name.to_string(),
        field_type: HumanInputType::Select,
        options: options.iter().map(|option| option.to_string()).collect(),
        required,
        description: None,
    }
}

/// `human_clarification` of `WORKFLOW_SCHEMA.md` §4: one required text field,
/// outputs `clarified` / `abort`.
fn clarification_action() -> ActionConfig {
    ActionConfig::Human(HumanActionConfig {
        instruction: "Agent ma wątpliwości. Proszę doprecyzować wymagania.".to_string(),
        inputs: vec![text_field("clarification_text", true)],
        input_schema: None,
    })
}

/// The §3.3.C review form: a required `select` plus an optional `comments`.
fn review_action() -> ActionConfig {
    ActionConfig::Human(HumanActionConfig {
        instruction: "Proszę o review zmian.".to_string(),
        inputs: vec![
            select_field(
                "approval_status",
                &["approved", "changes_requested", "rejected"],
                true,
            ),
            text_field("comments", false),
        ],
        input_schema: None,
    })
}

/// `human_review_tests` of §4: no fields at all, outputs `ignore` / `fix_it`.
fn no_fields_action() -> ActionConfig {
    ActionConfig::Human(HumanActionConfig {
        instruction: "Część testów nie przeszła. Czy ignorować?".to_string(),
        inputs: Vec::new(),
        input_schema: None,
    })
}

fn labels(labels: &[&str]) -> Vec<String> {
    labels.iter().map(|label| label.to_string()).collect()
}

/// A fresh running execution, saved in the store.
fn start_execution(store: &ExecutionStore, workspace_path: &Path) -> WorkflowExecution {
    let execution = WorkflowExecution {
        id: uuid::Uuid::new_v4().to_string(),
        workflow_id: "implement-feature".to_string(),
        workflow_version: 1,
        status: ExecutionStatus::Running,
        current_state: Some("analyze".to_string()),
        started_at: chrono::Utc::now(),
        completed_at: None,
        inputs: json!({}),
        context: orchestrator::domain::Context::new(),
        workspace: Workspace::local_folder(workspace_path.to_string_lossy().into_owned()),
        current_action_execution: None,
        metadata: json!({}),
    };
    store.save_execution(&execution).expect("save execution");
    execution
}

/// Runs the executor over an action and returns what it suspended on.
async fn request_for(action: &ActionConfig, action_execution_id: &str) -> SuspendedAction {
    let workspace = Workspace::local_folder(".".to_string());
    let request = ActionRequest::new(action, &workspace, action_execution_id);

    let outcome = HumanExecutor::new()
        .execute(request)
        .await
        .expect("human executor never fails on a human action");

    match outcome {
        ActionOutcome::Suspended(suspended) => suspended,
        other => panic!("expected a suspended outcome, got {other:?}"),
    }
}

/// Enters a human state: runs the executor, builds the durable record and
/// suspends the execution. This is what U7's loop will do around the executor.
async fn enter_human_state(
    store: &ExecutionStore,
    execution: &mut WorkflowExecution,
    state_id: &str,
    action: &ActionConfig,
    outputs: &[&str],
) -> PendingHumanInput {
    let action_execution_id = uuid::Uuid::new_v4().to_string();
    let SuspendedAction::AwaitingHumanInput(request) =
        request_for(action, &action_execution_id).await;

    let pending = PendingHumanInput::new(
        execution.id.clone(),
        state_id.to_string(),
        request,
        labels(outputs),
    );
    human::suspend(store, execution, &pending).expect("suspend");
    pending
}

fn events(store: &ExecutionStore, execution_id: &str) -> Vec<Event> {
    store
        .event_log(execution_id)
        .read_all()
        .expect("read events")
}

fn has_input_requested(events: &[Event]) -> bool {
    events
        .iter()
        .any(|event| matches!(event.data, EventData::HumanInputRequested { .. }))
}

fn received_input(events: &[Event]) -> Option<Value> {
    events.iter().find_map(|event| match &event.data {
        EventData::HumanInputReceived { input } => Some(input.clone()),
        _ => None,
    })
}

fn values(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// The executor itself
// ---------------------------------------------------------------------------

#[tokio::test]
async fn executor_suspends_instead_of_running_anything() {
    let action = clarification_action();
    let SuspendedAction::AwaitingHumanInput(request) = request_for(&action, "action-1").await;

    assert_eq!(request.action_execution_id, "action-1");
    assert!(request.instruction.contains("doprecyzować"));
    assert_eq!(request.fields.len(), 1);
    assert_eq!(request.fields[0].name, "clarification_text");
}

#[tokio::test]
async fn executor_rejects_an_action_of_another_type() {
    let action = ActionConfig::Script(orchestrator::domain::ScriptActionConfig {
        command: "true".to_string(),
        args: Vec::new(),
        working_dir: None,
        env: Default::default(),
        exit_codes: Default::default(),
        timeout_secs: None,
    });
    let workspace = Workspace::local_folder(".".to_string());

    let error = HumanExecutor::new()
        .execute(ActionRequest::new(&action, &workspace, "action-1"))
        .await
        .expect_err("a script action is not a human action");

    assert!(error.message.contains("script"), "{}", error.message);
}

// ---------------------------------------------------------------------------
// Entering a human state
// ---------------------------------------------------------------------------

#[tokio::test]
async fn entering_a_human_state_pauses_logs_and_stores_the_request() {
    let root = TempRoot::new();
    let store = root.store();
    let mut execution = start_execution(&store, root.path());

    let pending = enter_human_state(
        &store,
        &mut execution,
        "human_clarification",
        &clarification_action(),
        &["clarified", "abort"],
    )
    .await;

    // In memory and on disk alike.
    assert_eq!(execution.status, ExecutionStatus::Paused);
    let stored = store.load_execution(&execution.id).expect("load");
    assert_eq!(stored.status, ExecutionStatus::Paused);
    assert_eq!(stored.current_state.as_deref(), Some("human_clarification"));
    assert_eq!(
        stored.current_action_execution.as_deref(),
        Some(pending.action_execution_id())
    );

    assert!(has_input_requested(&events(&store, &execution.id)));

    // Durably stored, in its own file, readable without the definition.
    assert!(store.has_pending_input(&execution.id));
    assert!(store.pending_input_file(&execution.id).exists());
    let read: PendingHumanInput = store
        .load_pending_input(&execution.id)
        .expect("load pending")
        .expect("a request is pending");
    assert_eq!(read.state_id, "human_clarification");
    assert_eq!(read.outputs, labels(&["clarified", "abort"]));
    assert_eq!(read.fields()[0].name, "clarification_text");
}

// ---------------------------------------------------------------------------
// Delivering an answer
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delivering_a_select_value_yields_it_as_the_label_and_resumes() {
    let root = TempRoot::new();
    let store = root.store();
    let mut execution = start_execution(&store, root.path());

    enter_human_state(
        &store,
        &mut execution,
        "human_review",
        &review_action(),
        &["approved", "changes_requested", "rejected"],
    )
    .await;

    let delivery =
        HumanInputDelivery::with_values(values(&[("approval_status", json!("approved"))]));
    let resumed: ResumedHumanAction =
        human::resume(&store, &execution.id, &delivery).expect("resume");

    assert_eq!(resumed.result.output_label, "approved");
    assert_eq!(resumed.execution.status, ExecutionStatus::Running);
    assert_eq!(
        store.load_execution(&execution.id).expect("load").status,
        ExecutionStatus::Running
    );

    let log = events(&store, &execution.id);
    assert_eq!(
        received_input(&log),
        Some(json!({ "approval_status": "approved" }))
    );

    // The answered request is gone, so it cannot be answered twice.
    assert!(!store.has_pending_input(&execution.id));
}

#[tokio::test]
async fn an_action_without_fields_takes_the_label_straight_from_outputs() {
    let root = TempRoot::new();
    let store = root.store();
    let mut execution = start_execution(&store, root.path());

    enter_human_state(
        &store,
        &mut execution,
        "human_review_tests",
        &no_fields_action(),
        &["ignore", "fix_it"],
    )
    .await;

    let resumed =
        human::resume(&store, &execution.id, &HumanInputDelivery::label("ignore")).expect("resume");

    assert_eq!(resumed.result.output_label, "ignore");
    assert!(resumed.result.variables.is_empty());
    assert_eq!(resumed.execution.status, ExecutionStatus::Running);
}

#[tokio::test]
async fn an_optional_field_may_be_omitted() {
    let root = TempRoot::new();
    let store = root.store();
    let mut execution = start_execution(&store, root.path());

    enter_human_state(
        &store,
        &mut execution,
        "human_review",
        &review_action(),
        &["approved", "changes_requested", "rejected"],
    )
    .await;

    let delivery =
        HumanInputDelivery::with_values(values(&[("approval_status", json!("changes_requested"))]));
    let resumed = human::resume(&store, &execution.id, &delivery).expect("resume");

    assert_eq!(resumed.result.output_label, "changes_requested");
    assert!(!resumed.result.variables.contains_key("comments"));
}

#[tokio::test]
async fn delivered_values_become_context_variables() {
    let root = TempRoot::new();
    let store = root.store();
    let mut execution = start_execution(&store, root.path());

    enter_human_state(
        &store,
        &mut execution,
        "human_clarification",
        &clarification_action(),
        &["clarified", "abort"],
    )
    .await;

    let delivery = HumanInputDelivery::with_values(values(&[(
        "clarification_text",
        json!("Obsłuż tylko konta firmowe."),
    )]))
    .choosing("clarified");
    let resumed = human::resume(&store, &execution.id, &delivery).expect("resume");

    // The executor hands the values back; per D10 the engine is what writes
    // them into the context, so that last step is performed here.
    assert_eq!(resumed.result.output_label, "clarified");
    assert_eq!(
        resumed.result.variables.get("clarification_text"),
        Some(&json!("Obsłuż tylko konta firmowe."))
    );

    let mut execution = resumed.execution;
    execution
        .context
        .update_variables(resumed.result.variables.clone());
    store.save_execution(&execution).expect("save");

    let context = store.load_context(&execution.id).expect("load context");
    assert_eq!(
        context.get_variable("clarification_text"),
        Some(&json!("Obsłuż tylko konta firmowe."))
    );
}

// ---------------------------------------------------------------------------
// Rejected deliveries
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_missing_required_field_is_rejected_by_name_and_leaves_it_paused() {
    let root = TempRoot::new();
    let store = root.store();
    let mut execution = start_execution(&store, root.path());

    enter_human_state(
        &store,
        &mut execution,
        "human_clarification",
        &clarification_action(),
        &["clarified", "abort"],
    )
    .await;

    let error = human::resume(
        &store,
        &execution.id,
        &HumanInputDelivery::default().choosing("clarified"),
    )
    .expect_err("a required field is missing");

    assert!(
        matches!(&error, HumanActionError::MissingRequiredField { field } if field == "clarification_text"),
        "{error}"
    );
    assert!(error.to_string().contains("clarification_text"));

    // Nothing moved: the request is still there to be answered.
    assert_eq!(
        store.load_execution(&execution.id).expect("load").status,
        ExecutionStatus::Paused
    );
    assert!(store.has_pending_input(&execution.id));
}

#[tokio::test]
async fn a_select_value_outside_its_options_is_rejected_with_the_permitted_values() {
    let root = TempRoot::new();
    let store = root.store();
    let mut execution = start_execution(&store, root.path());

    enter_human_state(
        &store,
        &mut execution,
        "human_review",
        &review_action(),
        &["approved", "changes_requested", "rejected"],
    )
    .await;

    let delivery = HumanInputDelivery::with_values(values(&[("approval_status", json!("maybe"))]));
    let error =
        human::resume(&store, &execution.id, &delivery).expect_err("'maybe' is not allowed");

    let message = error.to_string();
    assert!(message.contains("approval_status"), "{message}");
    assert!(message.contains("approved"), "{message}");
    assert!(message.contains("changes_requested"), "{message}");
    assert!(message.contains("rejected"), "{message}");

    assert_eq!(
        store.load_execution(&execution.id).expect("load").status,
        ExecutionStatus::Paused
    );
}

#[tokio::test]
async fn a_label_outside_the_declared_outputs_is_rejected() {
    let root = TempRoot::new();
    let store = root.store();
    let mut execution = start_execution(&store, root.path());

    enter_human_state(
        &store,
        &mut execution,
        "human_review_tests",
        &no_fields_action(),
        &["ignore", "fix_it"],
    )
    .await;

    let error = human::resume(&store, &execution.id, &HumanInputDelivery::label("retry"))
        .expect_err("'retry' is not an output of this state");

    let message = error.to_string();
    assert!(message.contains("retry"), "{message}");
    assert!(message.contains("ignore"), "{message}");
}

#[tokio::test]
async fn resuming_a_running_execution_is_rejected() {
    let root = TempRoot::new();
    let store = root.store();
    let execution = start_execution(&store, root.path());

    let error = human::resume(&store, &execution.id, &HumanInputDelivery::label("ignore"))
        .expect_err("a running execution has nothing to answer");

    assert!(
        matches!(&error, HumanActionError::NotPaused { status, .. } if status == "running"),
        "{error}"
    );
}

#[tokio::test]
async fn resuming_a_completed_execution_is_rejected() {
    let root = TempRoot::new();
    let store = root.store();
    let mut execution = start_execution(&store, root.path());

    enter_human_state(
        &store,
        &mut execution,
        "human_review_tests",
        &no_fields_action(),
        &["ignore", "fix_it"],
    )
    .await;

    // The execution finished — and a stale pending file is left behind, which
    // is exactly the shape a crash would leave.
    execution.status = ExecutionStatus::Completed;
    execution.completed_at = Some(chrono::Utc::now());
    store.save_execution(&execution).expect("save");
    assert!(store.has_pending_input(&execution.id));

    let error = human::resume(&store, &execution.id, &HumanInputDelivery::label("ignore"))
        .expect_err("a completed execution cannot be resumed");

    assert!(
        matches!(&error, HumanActionError::NotPaused { status, .. } if status == "completed"),
        "{error}"
    );
}

#[tokio::test]
async fn delivering_the_same_answer_twice_is_rejected() {
    let root = TempRoot::new();
    let store = root.store();
    let mut execution = start_execution(&store, root.path());

    enter_human_state(
        &store,
        &mut execution,
        "human_review_tests",
        &no_fields_action(),
        &["ignore", "fix_it"],
    )
    .await;

    human::resume(&store, &execution.id, &HumanInputDelivery::label("ignore")).expect("resume");

    let error = human::resume(&store, &execution.id, &HumanInputDelivery::label("ignore"))
        .expect_err("the answer was already delivered");

    assert!(
        matches!(error, HumanActionError::NotPaused { .. }),
        "{error}"
    );
}

// ---------------------------------------------------------------------------
// Surviving a restart
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_pending_request_survives_a_restart_and_can_still_be_satisfied() {
    let root = TempRoot::new();

    // --- First process: start, enter the human state, pause. ---------------
    let execution_id = {
        let store = root.store();
        let mut execution = start_execution(&store, root.path());
        enter_human_state(
            &store,
            &mut execution,
            "human_clarification",
            &clarification_action(),
            &["clarified", "abort"],
        )
        .await;
        assert_eq!(execution.status, ExecutionStatus::Paused);
        execution.id
    };
    // Every in-memory object of the first "process" is gone at this point:
    // the store, the execution and the pending record were all dropped.

    // --- Second process: a fresh store over the same root. ------------------
    let store = ExecutionStore::new(root.path());

    // The TUI's question — "is this one waiting for me, and what does it
    // want?" — answered by scanning the store alone.
    let waiting: Vec<String> = store
        .list_execution_ids()
        .expect("list")
        .into_iter()
        .filter(|id| store.has_pending_input(id))
        .collect();
    assert_eq!(waiting, vec![execution_id.clone()]);

    let pending = human::pending_input(&store, &execution_id)
        .expect("read pending")
        .expect("the request is still pending");
    assert_eq!(pending.state_id, "human_clarification");
    assert_eq!(pending.fields()[0].name, "clarification_text");
    assert_eq!(pending.outputs, labels(&["clarified", "abort"]));

    // And it is still answerable.
    let delivery =
        HumanInputDelivery::with_values(values(&[("clarification_text", json!("Tylko PLN."))]))
            .choosing("clarified");
    let resumed = human::resume(&store, &execution_id, &delivery).expect("resume after restart");

    assert_eq!(resumed.result.output_label, "clarified");
    assert_eq!(
        resumed.result.variables.get("clarification_text"),
        Some(&json!("Tylko PLN."))
    );
    assert_eq!(resumed.execution.status, ExecutionStatus::Running);
    assert_eq!(
        store.load_execution(&execution_id).expect("load").status,
        ExecutionStatus::Running
    );

    // The whole history is intact across the restart.
    let log = events(&store, &execution_id);
    assert!(has_input_requested(&log));
    assert!(received_input(&log).is_some());
}

#[tokio::test]
async fn a_pending_request_of_a_non_paused_execution_is_not_offered() {
    let root = TempRoot::new();
    let store = root.store();
    let mut execution = start_execution(&store, root.path());

    enter_human_state(
        &store,
        &mut execution,
        "human_review_tests",
        &no_fields_action(),
        &["ignore", "fix_it"],
    )
    .await;

    execution.status = ExecutionStatus::Cancelled;
    store.save_execution(&execution).expect("save");

    // The file is still on disk, but it is not the authority — the status is.
    assert!(store.has_pending_input(&execution.id));
    assert!(human::pending_input(&store, &execution.id)
        .expect("read pending")
        .is_none());
}
