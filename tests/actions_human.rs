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

use narsor_engine::domain::{
    ActionConfig, Event, EventData, ExecutionStatus, HumanActionConfig, HumanInputField,
    HumanInputType, OutputDefinition, WorkflowExecution, Workspace,
};
use narsor_engine::engine::actions::human::{
    self, HumanActionError, HumanInputRequest, PendingHumanInput, PermissionDecision,
    PermissionDecisionKind, PermissionDenialDetails, PermissionGrantScope,
    PermissionRequestDetails, ResumedHumanAction,
};
use narsor_engine::engine::{
    ActionExecutor, ActionOutcome, ActionRequest, HumanExecutor, HumanInputDelivery,
    SuspendedAction,
};
use narsor_engine::persistence::ExecutionStore;
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
        let path = std::env::temp_dir().join(format!("narsor-human-{}", uuid::Uuid::new_v4()));
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

/// Outputs without descriptions, as a state declaring bare labels has them.
fn labels(labels: &[&str]) -> Vec<OutputDefinition> {
    labels.iter().copied().map(OutputDefinition::new).collect()
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
        context: narsor_engine::domain::Context::new(),
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
        request_for(action, &action_execution_id).await
    else {
        panic!("a human action must request human input");
    };

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
        EventData::HumanInputReceived { input, .. } => Some(input.clone()),
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
    let SuspendedAction::AwaitingHumanInput(request) = request_for(&action, "action-1").await
    else {
        panic!("a human action must request human input");
    };

    assert_eq!(request.action_execution_id, "action-1");
    assert!(request.instruction.contains("doprecyzować"));
    assert_eq!(request.fields.len(), 1);
    assert_eq!(request.fields[0].name, "clarification_text");
}

#[tokio::test]
async fn executor_rejects_an_action_of_another_type() {
    let action = ActionConfig::Script(narsor_engine::domain::ScriptActionConfig {
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
    assert_eq!(execution.status, ExecutionStatus::InputNeeded);
    let stored = store.load_execution(&execution.id).expect("load");
    assert_eq!(stored.status, ExecutionStatus::InputNeeded);
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

#[test]
fn permission_wait_survives_reload_and_rejects_ordinary_input() {
    let root = TempRoot::new();
    let store = root.store();
    let mut execution = start_execution(&store, root.path());
    let request = HumanInputRequest {
        action_execution_id: "attempt-permission".to_string(),
        instruction: "Claude Code needs a permission decision.".to_string(),
        fields: Vec::new(),
        question: None,
        answer_tail: Some("The command was blocked.".to_string()),
        agent_harness: Some("claude-code".to_string()),
        agent_session_id: Some("session-permission".to_string()),
        prefilled_answer: None,
        permission: Some(PermissionRequestDetails {
            denials: vec![PermissionDenialDetails {
                tool_name: "Bash".to_string(),
                tool_use_id: "tool-permission".to_string(),
                input: json!({ "command": "cargo build" }),
                cwd: root.path().to_string_lossy().into_owned(),
                reason_type: Some("subcommandResults".to_string()),
                reason: Some("This command requires approval.".to_string()),
            }],
        }),
    };
    let pending = PendingHumanInput::for_permission(
        execution.id.clone(),
        execution.current_state.clone().unwrap_or_default(),
        request,
    );
    human::suspend(&store, &mut execution, &pending).expect("persist permission wait");

    let reloaded: PendingHumanInput = store
        .load_pending_input(&execution.id)
        .expect("read pending")
        .expect("permission wait persisted");
    assert_eq!(
        reloaded.continuation,
        narsor_engine::engine::InputContinuation::ResolvePermission
    );
    assert_eq!(
        reloaded.request.permission.as_ref().unwrap().denials[0].tool_use_id,
        "tool-permission"
    );
    assert_eq!(
        store.load_execution(&execution.id).unwrap().status,
        ExecutionStatus::InputNeeded
    );

    let error = human::resume(&store, &execution.id, &HumanInputDelivery::default())
        .expect_err("ordinary input cannot resolve a permission request");
    assert!(matches!(
        error,
        HumanActionError::PermissionDecisionRequired
    ));
    assert!(store.has_pending_input(&execution.id));

    let resumed = human::resolve_permission(
        &store,
        &execution.id,
        PermissionDecision {
            denial_index: Some(0),
            decision: PermissionDecisionKind::Allow,
            scope: Some(PermissionGrantScope::ExactOperation),
        },
        reloaded.episode_key(),
    )
    .expect("the dedicated operation accepts a supported exact grant");
    assert_eq!(resumed.execution.status, ExecutionStatus::Running);
    assert!(resumed.permission_decision.is_some());
    assert!(!store.has_pending_input(&execution.id));
}

#[test]
fn permission_grant_rules_name_each_part_and_refuse_what_they_cannot_pin_down() {
    let denial = PermissionDenialDetails {
        tool_name: "Bash".into(),
        tool_use_id: "tool-1".into(),
        input: json!({"command":"cargo test"}),
        cwd: "/tmp/task".into(),
        reason_type: Some("subcommandResults".into()),
        reason: None,
    };
    let rules =
        |denial: &PermissionDenialDetails, scope| human::claude_permission_rules(denial, scope);
    assert_eq!(
        rules(&denial, PermissionGrantScope::ExactOperation),
        Some(vec!["Bash(cargo test)".to_string()])
    );
    assert_eq!(
        rules(&denial, PermissionGrantScope::CommandGroup),
        Some(vec!["Bash(cargo test *)".to_string()])
    );
    // Claude Code allows a compound command part by part.
    let mut compound = denial.clone();
    compound.input = json!({"command":"cargo test 2>&1 | tail -20 && curl example.com"});
    assert_eq!(
        rules(&compound, PermissionGrantScope::ExactOperation),
        Some(vec![
            "Bash(cargo test)".to_string(),
            "Bash(tail -20)".to_string(),
            "Bash(curl example.com)".to_string(),
        ])
    );
    let mut substituted = denial.clone();
    substituted.input = json!({"command":"cargo test $(cat flags)"});
    assert!(rules(&substituted, PermissionGrantScope::ExactOperation).is_none());
    // What `--permission-prompts none` reports for a command nobody could
    // approve (Claude Code 2.1.3xx).
    let mut unattended = denial.clone();
    unattended.reason_type = Some("asyncAgent".into());
    assert!(rules(&unattended, PermissionGrantScope::ExactOperation).is_some());
    for reason in ["other", "rule", "hook"] {
        let mut policy = denial.clone();
        policy.reason_type = Some(reason.into());
        assert!(
            rules(&policy, PermissionGrantScope::ExactOperation).is_none(),
            "{reason}"
        );
    }
}

#[test]
fn a_decision_without_an_index_grants_every_denial_or_none() {
    let denial = |command: &str, reason: &str| PermissionDenialDetails {
        tool_name: "Bash".into(),
        tool_use_id: format!("tool-{command}"),
        input: json!({ "command": command }),
        cwd: "/tmp/task".into(),
        reason_type: Some(reason.into()),
        reason: None,
    };
    let request = |denials| HumanInputRequest {
        action_execution_id: "attempt".to_string(),
        instruction: String::new(),
        fields: Vec::new(),
        question: None,
        answer_tail: None,
        agent_harness: Some("claude-code".to_string()),
        agent_session_id: Some("session".to_string()),
        prefilled_answer: None,
        permission: Some(PermissionRequestDetails { denials }),
    };
    let allow_all = PermissionDecision {
        denial_index: None,
        decision: PermissionDecisionKind::Allow,
        scope: Some(PermissionGrantScope::ExactOperation),
    };
    let both = request(vec![
        denial("cargo fmt --check && cargo test", "asyncAgent"),
        denial("cargo test | tail -5", "asyncAgent"),
    ]);
    assert_eq!(
        human::granted_rules(&both, &allow_all).unwrap(),
        vec![
            "Bash(cargo fmt --check)".to_string(),
            "Bash(cargo test)".to_string(),
            "Bash(tail -5)".to_string(),
        ],
        "every part once"
    );
    let one_blocked = request(vec![
        denial("cargo test", "asyncAgent"),
        denial("rm -rf build", "rule"),
    ]);
    assert!(
        human::granted_rules(&one_blocked, &allow_all).is_err(),
        "a policy block is never granted with the rest"
    );
    let only_first = PermissionDecision {
        denial_index: Some(0),
        ..allow_all
    };
    assert_eq!(
        human::granted_rules(&one_blocked, &only_first).unwrap(),
        vec!["Bash(cargo test)".to_string()]
    );
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
async fn an_omitted_optional_text_field_becomes_an_empty_string() {
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
    // An empty answer, not a missing one: a later `{{ context.comments }}`
    // renders as nothing instead of failing as an unknown variable.
    assert_eq!(resumed.result.variables.get("comments"), Some(&json!("")));
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
        ExecutionStatus::InputNeeded
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
        ExecutionStatus::InputNeeded
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
        assert_eq!(execution.status, ExecutionStatus::InputNeeded);
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

/// Output descriptions are part of the durable record, so a form rebuilt
/// after a restart can still say what each label means.
#[tokio::test]
async fn output_descriptions_survive_in_the_pending_record() {
    let root = TempRoot::new();
    let store = root.store();
    let mut execution = start_execution(&store, root.path());
    let SuspendedAction::AwaitingHumanInput(request) =
        request_for(&no_fields_action(), "action-1").await
    else {
        panic!("a human action must request human input");
    };
    let outputs = vec![
        OutputDefinition {
            label: "ignore".to_string(),
            description: Some("The failures are known and harmless.".to_string()),
            requires_human_approval: false,
            resume_comment_field: None,
        },
        OutputDefinition::new("fix_it"),
    ];
    let pending = PendingHumanInput::new(
        execution.id.clone(),
        "human_review_tests",
        request,
        outputs.clone(),
    );
    human::suspend(&store, &mut execution, &pending).expect("suspend");

    let reread = human::pending_input(&ExecutionStore::new(root.path()), &execution.id)
        .expect("read pending")
        .expect("the request is pending");
    assert_eq!(reread.outputs, outputs);
    assert_eq!(reread.output_labels(), ["ignore", "fix_it"]);
}

/// A `pending_input.json` written before outputs had descriptions — a plain
/// list of labels — still loads, and is still answerable.
#[tokio::test]
async fn a_pending_record_with_bare_output_labels_still_loads() {
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

    // Rewrite the record in the old shape, the way an older narsor left it.
    let path = store.pending_input_file(&execution.id);
    let mut record: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("read the record")).expect("json");
    record["outputs"] = json!(["ignore", "fix_it"]);
    fs::write(&path, record.to_string()).expect("write the old shape");

    let pending = human::pending_input(&store, &execution.id)
        .expect("the old shape still reads")
        .expect("the request is pending");
    assert_eq!(pending.outputs, labels(&["ignore", "fix_it"]));

    let resumed = human::resume(&store, &execution.id, &HumanInputDelivery::label("fix_it"))
        .expect("the old record is still answerable");
    assert_eq!(resumed.result.output_label, "fix_it");
}

/// A `pending_input.json` written before requests carried the preceding
/// agent's question still loads, with neither the question nor the tail.
#[tokio::test]
async fn a_pending_record_without_an_agent_question_still_loads() {
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

    let path = store.pending_input_file(&execution.id);
    let mut record: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("read the record")).expect("json");
    let object = record.as_object_mut().expect("an object");
    object.remove("question");
    object.remove("answer_tail");
    fs::write(&path, record.to_string()).expect("write the old shape");

    let pending = human::pending_input(&store, &execution.id)
        .expect("the old shape still reads")
        .expect("the request is pending");
    assert_eq!(pending.request.question, None);
    assert_eq!(pending.request.answer_tail, None);
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

// ---------------------------------------------------------------------------
// Problems: the `ResolveOutput` request (U5)
// ---------------------------------------------------------------------------

use narsor_engine::domain::{AnsweredBy, InputCloseReason};
use narsor_engine::engine::actions::human::{
    InputContinuation, OutputProblem, OutputRequirement, RejectedAnswer,
};

/// Suspends `execution` on a problem of state `work`: labels `implemented`
/// (needs `variables.summary`) and `needs_human_decision` (needs
/// `data.question`), after one rejected agent answer.
fn raise_problem(store: &ExecutionStore, execution: &mut WorkflowExecution) -> PendingHumanInput {
    let request = HumanInputRequest {
        action_execution_id: "attempt-1".to_string(),
        instruction: "Choose the label the step ends with.".to_string(),
        fields: Vec::new(),
        question: None,
        answer_tail: Some("All done, tests pass.".to_string()),
        agent_harness: None,
        agent_session_id: None,
        prefilled_answer: None,
        permission: None,
    };
    let problem = OutputProblem {
        reason: "no output label".to_string(),
        answers: vec![RejectedAnswer {
            action_execution_id: "attempt-1".to_string(),
            reason: "no output label".to_string(),
            answer_tail: Some("All done, tests pass.".to_string()),
        }],
        requirements: vec![
            OutputRequirement {
                label: "implemented".to_string(),
                data: Vec::new(),
                variables: vec!["summary".to_string()],
            },
            OutputRequirement {
                label: "needs_human_decision".to_string(),
                data: vec!["question".to_string()],
                variables: Vec::new(),
            },
        ],
        analysis: None,
    };
    let pending = PendingHumanInput::for_problem(
        execution.id.clone(),
        "work".to_string(),
        request,
        labels(&["implemented", "needs_human_decision"]),
        problem,
    );
    human::suspend(store, execution, &pending).expect("suspend");
    pending
}

fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().expect("an object")
}

#[test]
fn a_problem_is_answered_with_a_label_its_data_and_its_variables() {
    let root = TempRoot::new();
    let store = root.store();
    let mut execution = start_execution(&store, root.path());
    let pending = raise_problem(&store, &mut execution);

    let delivery = HumanInputDelivery::label("implemented")
        .with_data(object(json!({ "units_completed": 3 })))
        .with_variables(object(json!({ "summary": "login works" })))
        .with_comment("  read the answer myself  ")
        .for_episode(pending.episode_key());
    let resumed = human::resume(&store, &execution.id, &delivery).expect("resume");

    assert_eq!(
        resumed.pending.continuation,
        InputContinuation::ResolveOutput
    );
    assert_eq!(resumed.result.output_label, "implemented");
    assert_eq!(resumed.result.data, json!({ "units_completed": 3 }));
    assert_eq!(
        resumed.result.variables,
        object(json!({ "summary": "login works" }))
    );
    assert_eq!(
        store.load_execution(&execution.id).expect("reload").status,
        ExecutionStatus::Running
    );
    assert!(!store.has_pending_input(&execution.id));

    let received = events(&store, &execution.id)
        .into_iter()
        .find_map(|event| match event.data {
            data @ EventData::HumanInputReceived { .. } => Some(data),
            _ => None,
        })
        .expect("HumanInputReceived");
    let EventData::HumanInputReceived {
        answered_by,
        close_reason,
        episode,
        output_label,
        data,
        variables,
        comment,
        ..
    } = received
    else {
        unreachable!()
    };
    assert_eq!(answered_by, Some(AnsweredBy::Operator));
    assert_eq!(close_reason, Some(InputCloseReason::Answered));
    assert_eq!(episode.as_deref(), Some(pending.episode_key()));
    assert_eq!(output_label.as_deref(), Some("implemented"));
    assert_eq!(data, Some(json!({ "units_completed": 3 })));
    assert_eq!(variables, Some(json!({ "summary": "login works" })));
    assert_eq!(comment.as_deref(), Some("read the answer myself"));
}

#[test]
fn a_problem_can_be_returned_to_the_agent_without_choosing_an_output() {
    let root = TempRoot::new();
    let store = root.store();
    let mut execution = start_execution(&store, root.path());
    let pending = raise_problem(&store, &mut execution);

    let resumed = human::resume(
        &store,
        &execution.id,
        &HumanInputDelivery::default()
            .with_agent_message("Please use the documented API version.")
            .for_episode(pending.episode_key()),
    )
    .expect("message returns to the agent");

    assert_eq!(resumed.result.output_label, "__resume_agent__");
    assert_eq!(
        resumed.result.variables.get("answer"),
        Some(&json!("Please use the documented API version."))
    );
}

#[test]
fn a_problem_answer_without_a_required_key_names_the_key_and_changes_nothing() {
    let root = TempRoot::new();
    let store = root.store();
    let mut execution = start_execution(&store, root.path());
    raise_problem(&store, &mut execution);

    let error = human::resume(
        &store,
        &execution.id,
        &HumanInputDelivery::label("needs_human_decision"),
    )
    .expect_err("the gate needs a question");
    assert!(
        matches!(&error, HumanActionError::MissingRequiredKey { key, .. } if key == "data.question"),
        "{error:?}"
    );
    assert!(error.is_invalid_delivery());

    // A blank value is no value.
    let error = human::resume(
        &store,
        &execution.id,
        &HumanInputDelivery::label("implemented")
            .with_variables(object(json!({ "summary": "   " }))),
    )
    .expect_err("a blank summary is missing");
    assert!(
        matches!(&error, HumanActionError::MissingRequiredKey { key, .. } if key == "variables.summary"),
        "{error:?}"
    );

    assert_eq!(
        store.load_execution(&execution.id).expect("reload").status,
        ExecutionStatus::InputNeeded
    );
    assert!(store.has_pending_input(&execution.id));
    assert!(received_input(&events(&store, &execution.id)).is_none());
}

#[test]
fn a_required_variable_the_context_already_holds_need_not_be_delivered() {
    let root = TempRoot::new();
    let store = root.store();
    let mut execution = start_execution(&store, root.path());
    execution
        .context
        .variables
        .insert("summary".to_string(), json!("from an earlier step"));
    store.save_execution(&execution).expect("save context");
    raise_problem(&store, &mut execution);

    let resumed = human::resume(
        &store,
        &execution.id,
        &HumanInputDelivery::label("implemented"),
    )
    .expect("the context supplies the summary");
    assert_eq!(resumed.result.output_label, "implemented");
}

#[test]
fn a_problem_answer_needs_a_declared_label() {
    let root = TempRoot::new();
    let store = root.store();
    let mut execution = start_execution(&store, root.path());
    raise_problem(&store, &mut execution);

    let error =
        human::resume(&store, &execution.id, &HumanInputDelivery::default()).expect_err("no label");
    assert!(
        matches!(error, HumanActionError::NoLabelChosen { .. }),
        "{error:?}"
    );

    let error = human::resume(&store, &execution.id, &HumanInputDelivery::label("done"))
        .expect_err("not a declared label");
    assert!(
        matches!(error, HumanActionError::LabelNotDeclared { .. }),
        "{error:?}"
    );

    // A problem has no fields: a field value is undeclared.
    let error = human::resume(
        &store,
        &execution.id,
        &HumanInputDelivery::with_values(object(json!({ "answer": "x" }))).choosing("implemented"),
    )
    .expect_err("no fields");
    assert!(
        matches!(error, HumanActionError::UndeclaredField { .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn data_and_variables_are_refused_by_a_request_that_is_not_a_problem() {
    let root = TempRoot::new();
    let store = root.store();
    let mut execution = start_execution(&store, root.path());
    enter_human_state(
        &store,
        &mut execution,
        "review",
        &no_fields_action(),
        &["ignore", "fix_it"],
    )
    .await;

    let error = human::resume(
        &store,
        &execution.id,
        &HumanInputDelivery::label("ignore").with_data(object(json!({ "x": 1 }))),
    )
    .expect_err("nothing would read the data");
    assert!(
        matches!(error, HumanActionError::UnexpectedStepValues),
        "{error:?}"
    );
    assert!(store.has_pending_input(&execution.id));
}

#[tokio::test]
async fn a_delivery_for_another_request_is_stale_and_changes_nothing() {
    let root = TempRoot::new();
    let store = root.store();
    let mut execution = start_execution(&store, root.path());
    let pending = enter_human_state(
        &store,
        &mut execution,
        "review",
        &no_fields_action(),
        &["ignore", "fix_it"],
    )
    .await;
    assert!(!pending.episode.is_empty(), "every new request has a key");

    let error = human::resume(
        &store,
        &execution.id,
        &HumanInputDelivery::label("ignore").for_episode("an-earlier-request"),
    )
    .expect_err("a stale key is refused");
    assert!(
        matches!(&error, HumanActionError::StaleEpisode { current, .. } if current == pending.episode_key()),
        "{error:?}"
    );
    assert!(error.is_stale());
    assert!(store.has_pending_input(&execution.id));

    // Without a key (an older client) the answer is still taken.
    human::resume(&store, &execution.id, &HumanInputDelivery::label("ignore"))
        .expect("an unkeyed answer still resumes");
}

#[test]
fn a_pending_record_without_a_key_is_named_by_its_attempt() {
    let old_format = json!({
        "execution_id": "exec-1",
        "state_id": "review",
        "action_execution_id": "attempt-7",
        "instruction": "Approve?",
        "outputs": ["approve"],
        "requested_at": chrono::Utc::now().to_rfc3339(),
    });
    let pending: PendingHumanInput =
        serde_json::from_value(old_format).expect("an old record reads");
    assert_eq!(pending.episode_key(), "attempt-7");
    assert_eq!(pending.continuation, InputContinuation::CompleteStep);
    assert!(pending.problem.is_none());
}

#[tokio::test]
async fn closing_a_request_records_why_and_removes_it() {
    let root = TempRoot::new();
    let store = root.store();
    let mut execution = start_execution(&store, root.path());
    let pending = enter_human_state(
        &store,
        &mut execution,
        "review",
        &no_fields_action(),
        &["ignore"],
    )
    .await;

    let closed = human::close_pending(
        &store,
        &execution.id,
        InputCloseReason::Recover,
        AnsweredBy::Operator,
    )
    .expect("close")
    .expect("there was a request");
    assert_eq!(closed.episode_key(), pending.episode_key());
    assert!(!store.has_pending_input(&execution.id));

    let last = events(&store, &execution.id)
        .pop()
        .expect("the closing event");
    assert!(
        matches!(
            &last.data,
            EventData::HumanInputReceived {
                close_reason: Some(InputCloseReason::Recover),
                answered_by: Some(AnsweredBy::Operator),
                episode: Some(episode),
                ..
            } if episode == pending.episode_key()
        ),
        "{:?}",
        last.data
    );
    assert_eq!(
        last.description(),
        "Human input closed by recover (operator)"
    );

    // Nothing left: closing again is a no-op.
    assert!(human::close_pending(
        &store,
        &execution.id,
        InputCloseReason::Recover,
        AnsweredBy::Operator
    )
    .expect("close again")
    .is_none());
}

#[test]
fn a_delivery_never_reads_who_answers_from_the_wire() {
    let delivery: HumanInputDelivery =
        serde_json::from_value(json!({ "chosen_label": "x", "answered_by": "conductor" }))
            .expect("parse");
    assert_eq!(delivery.answered_by, AnsweredBy::Operator);
    let wire =
        serde_json::to_value(HumanInputDelivery::label("x").answered_by(AnsweredBy::Conductor))
            .expect("serialize");
    assert!(wire.get("answered_by").is_none(), "{wire}");
}
