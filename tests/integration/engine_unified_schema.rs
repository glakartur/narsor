//! The engine's behavior on workflows in the unified `schema: v2` shape:
//! scope by declaration, `options`, `source: workflow`, `exit` steps, `set`
//! and the label of an earlier step.
//!
//! The agent is scripted in-process; what is asserted is what the engine
//! hands the agent, writes to the context and records as events.

use crate::engine_support;

use std::sync::Arc;

use engine_support::{workflow_file, AgentTurn, Harness, ScriptedAgentExecutor};
use narsor_engine::domain::{EventData, ExecutionStatus, WorkflowExecution, WorkspaceField};
use narsor_engine::engine::{ActionKind, Engine, ManualTransition};
use narsor_engine::workflow::WorkspaceValues;
use serde_json::{json, Value};

fn engine_with(harness: &Harness, agent: &Arc<ScriptedAgentExecutor>) -> Engine {
    harness
        .engine()
        .with_executor(ActionKind::Agent, agent.clone())
}

/// Runs the fixture `file` on `turns`.
async fn run(
    harness: &Harness,
    file: &str,
    turns: Vec<AgentTurn>,
) -> (Arc<ScriptedAgentExecutor>, WorkflowExecution) {
    let agent = Arc::new(ScriptedAgentExecutor::new(turns));
    let execution = engine_with(harness, &agent)
        .start_and_run(&workflow_file(file), json!({}), harness.workspace())
        .await
        .expect("run");
    (agent, execution)
}

fn events(harness: &Harness, execution_id: &str) -> Vec<EventData> {
    harness
        .events(execution_id)
        .into_iter()
        .map(|event| event.data)
        .collect()
}

fn action_failures(harness: &Harness, execution_id: &str) -> Vec<String> {
    events(harness, execution_id)
        .into_iter()
        .filter_map(|data| match data {
            EventData::ActionFailed { error, .. } => Some(error),
            _ => None,
        })
        .collect()
}

fn local_values(execution: &WorkflowExecution, state_id: &str) -> Value {
    execution.metadata["engine"]["step_locals"][state_id].clone()
}

fn variable(execution: &WorkflowExecution, name: &str) -> Option<Value> {
    execution.context.variables.get(name).cloned()
}

/// The transitions taken, as `from -> to (label)`.
fn transitions(harness: &Harness, execution_id: &str) -> Vec<String> {
    events(harness, execution_id)
        .into_iter()
        .filter_map(|data| match data {
            EventData::TransitionTaken {
                from_state,
                to_state,
                output_label,
            } => Some(format!("{from_state} -> {to_state} ({output_label})")),
            _ => None,
        })
        .collect()
}

/// The variables the workflow set, as `(name, value, state, label)`.
fn variables_set(harness: &Harness, execution_id: &str) -> Vec<(String, Value, String, String)> {
    events(harness, execution_id)
        .into_iter()
        .filter_map(|data| match data {
            EventData::VariableSetByWorkflow {
                variable,
                value,
                state_id,
                output_label,
                ..
            } => Some((variable, value, state_id, output_label)),
            _ => None,
        })
        .collect()
}

/// The turn of the exit step `brainstorm`: no label to choose.
fn ideas() -> AgentTurn {
    AgentTurn::with_variables("exit", json!({ "ideas": "a few" }))
}

/// A local variable kept by one entry is still there when a later entry
/// does not give it, and never reaches the context.
#[tokio::test]
async fn a_declared_local_survives_an_entry_that_does_not_give_it() {
    let harness = Harness::new();
    let (agent, execution) = run(
        &harness,
        "v2/engine_scope.yaml",
        vec![
            AgentTurn::with_variables("revise", json!({ "memory": "A" })),
            ideas(),
            AgentTurn::label("revise", "again"),
            ideas(),
            AgentTurn::label("ready", "done"),
        ],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let plan_prompts: Vec<String> = agent
        .prompts()
        .into_iter()
        .filter(|prompt| prompt.starts_with("Plan."))
        .collect();
    assert!(plan_prompts[1].contains("[A]"), "{}", plan_prompts[1]);
    assert!(plan_prompts[2].contains("[A]"), "{}", plan_prompts[2]);
    assert_eq!(local_values(&execution, "plan")["memory"], json!("A"));
    assert_eq!(variable(&execution, "memory"), None);
}

/// A new value of a local variable replaces the old one.
#[tokio::test]
async fn a_new_local_value_replaces_the_old_one() {
    let harness = Harness::new();
    let (agent, execution) = run(
        &harness,
        "v2/engine_scope.yaml",
        vec![
            AgentTurn::with_variables("revise", json!({ "memory": "A" })),
            ideas(),
            AgentTurn::with_variables("revise", json!({ "memory": "B" })),
            ideas(),
            AgentTurn::label("ready", "done"),
        ],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(local_values(&execution, "plan")["memory"], json!("B"));
    assert!(agent.prompts()[4].contains("[B]"), "{}", agent.prompts()[4]);
}

/// A choice that requires a local variable is met by the value an earlier
/// entry kept: no reprompt.
#[tokio::test]
async fn a_required_local_is_met_by_an_earlier_entry() {
    let harness = Harness::new();
    let (agent, execution) = run(
        &harness,
        "v2/engine_scope.yaml",
        vec![
            AgentTurn::with_variables("noted", json!({ "note": "n" })),
            ideas(),
            AgentTurn::label("noted", "no new note"),
            ideas(),
            AgentTurn::label("ready", "done"),
        ],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(agent.calls().len(), 5, "no reprompt was needed");
}

/// A required local variable that no entry ever gave is asked for again.
#[tokio::test]
async fn a_required_local_nobody_gave_is_asked_for_again() {
    let harness = Harness::new();
    let (agent, execution) = run(
        &harness,
        "v2/engine_scope.yaml",
        vec![
            AgentTurn::label("noted", "forgot the note"),
            AgentTurn::with_variables("noted", json!({ "note": "n" })),
            ideas(),
            AgentTurn::label("ready", "done"),
        ],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(agent.calls().len(), 4);
    assert_eq!(local_values(&execution, "plan")["note"], json!("n"));
}

/// A value outside `options` is rejected like a missing variable: the agent
/// is asked again, with the allowed values in the request.
#[tokio::test]
async fn a_value_outside_options_is_asked_for_again_with_the_allowed_values() {
    let harness = Harness::new();
    let (agent, execution) = run(
        &harness,
        "v2/engine_scope.yaml",
        vec![
            AgentTurn::with_variables("judged", json!({ "verdict": "bogus" })),
            AgentTurn::with_variables("judged", json!({ "verdict": "fine" })),
        ],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(variable(&execution, "verdict"), Some(json!("fine")));
    let calls = agent.calls();
    assert_eq!(calls.len(), 2);
    let reprompt = &calls[1].prompt;
    assert!(reprompt.contains("bogus"), "{reprompt}");
    assert!(
        reprompt.contains("fine") && reprompt.contains("poor"),
        "{reprompt}"
    );
    assert!(events(&harness, &execution.id)
        .iter()
        .any(|data| matches!(data, EventData::ActionOutputRejected { .. })));
}

/// A value inside `options` is accepted, and so is a choice that does not
/// give the variable at all.
#[tokio::test]
async fn a_value_inside_options_is_accepted() {
    let harness = Harness::new();
    let (agent, execution) = run(
        &harness,
        "v2/engine_scope.yaml",
        vec![AgentTurn::with_variables(
            "judged",
            json!({ "verdict": "poor" }),
        )],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(agent.calls().len(), 1);
    assert_eq!(variable(&execution, "verdict"), Some(json!("poor")));
}

/// A `source: workflow` variable given by the agent is kept as additional
/// data with a warning and never overwrites the context.
#[tokio::test]
async fn a_workflow_variable_given_by_the_agent_goes_to_additional_data() {
    let harness = Harness::new();
    let (_agent, execution) = run(
        &harness,
        "v2/engine_scope.yaml",
        vec![AgentTurn::with_variables(
            "ready",
            json!({ "autopilot": "on" }),
        )],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(variable(&execution, "autopilot"), None);
    let completed = events(&harness, &execution.id)
        .into_iter()
        .find_map(|data| match data {
            EventData::ActionCompleted {
                additional_data,
                warnings,
                ..
            } => Some((additional_data, warnings)),
            _ => None,
        })
        .expect("the step completed");
    assert_eq!(completed.0.get("autopilot"), Some(&json!("on")));
    assert_eq!(completed.1.len(), 1, "{:?}", completed.1);
}

/// A script has no reprompt: a value outside `options` ends the step with
/// an action error.
#[tokio::test]
async fn a_script_value_outside_options_fails_the_action() {
    let harness = Harness::new();
    let (_agent, execution) = run(&harness, "v2/engine_script_options.yaml", vec![]).await;

    assert_eq!(execution.status, ExecutionStatus::Failed);
    let failures = action_failures(&harness, &execution.id);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("bogus"), "{}", failures[0]);
    assert!(transitions(&harness, &execution.id).is_empty());
}

/// A script step with `exit` goes on when it exits with code 0.
#[tokio::test]
async fn a_script_with_exit_goes_on_after_exit_code_zero() {
    let harness = Harness::new();
    let (_agent, execution) = run(&harness, "v2/engine_script_exit.yaml", vec![]).await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(
        transitions(&harness, &execution.id),
        vec!["build -> done (exit)".to_string()]
    );
}

/// An agent in a step with `exit` goes on to its `next`.
#[tokio::test]
async fn an_agent_in_an_exit_step_goes_on_to_its_next_step() {
    let harness = Harness::new();
    let (_agent, execution) = run(
        &harness,
        "v2/engine_scope.yaml",
        vec![
            AgentTurn::label("revise", "again"),
            ideas(),
            AgentTurn::label("ready", "done"),
        ],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(
        transitions(&harness, &execution.id),
        vec![
            "plan -> brainstorm (revise)".to_string(),
            "brainstorm -> plan (exit)".to_string(),
            "plan -> done (ready)".to_string(),
        ]
    );
}

/// An exit step still requires its variables: without them the agent is
/// asked again.
#[tokio::test]
async fn an_exit_step_without_its_required_variable_is_asked_again() {
    let harness = Harness::new();
    let (agent, execution) = run(
        &harness,
        "v2/engine_scope.yaml",
        vec![
            AgentTurn::label("revise", "again"),
            AgentTurn::label("exit", "no ideas"),
            ideas(),
            AgentTurn::label("ready", "done"),
        ],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(agent.calls().len(), 4);
    assert_eq!(variable(&execution, "ideas"), Some(json!("a few")));
}

/// A step with `choices` that gets no label it declares is still asked
/// again; the reserved exit label does not stand in for a choice.
#[tokio::test]
async fn a_choices_step_does_not_accept_the_exit_label() {
    let harness = Harness::new();
    let (agent, execution) = run(
        &harness,
        "v2/engine_scope.yaml",
        vec![
            AgentTurn::label("exit", "no label"),
            AgentTurn::label("ready", "done"),
        ],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(agent.calls().len(), 2);
}

/// `set` assigns the variables of the choice taken, and nothing for another.
#[tokio::test]
async fn set_assigns_the_variables_of_the_chosen_label_only() {
    let harness = Harness::new();
    let (_agent, approved) = run(
        &harness,
        "v2/engine_set.yaml",
        vec![
            AgentTurn::with_variables("approved", json!({ "summary": "S" })),
            AgentTurn::label("exit", "reported"),
        ],
    )
    .await;
    assert_eq!(approved.status, ExecutionStatus::Completed);
    assert_eq!(variable(&approved, "stage"), Some(json!("reviewed")));
    assert_eq!(variable(&approved, "autopilot"), Some(json!("on")));
    assert_eq!(variable(&approved, "count"), Some(json!(3)));

    let (_agent, plain) = run(
        &harness,
        "v2/engine_set.yaml",
        vec![
            AgentTurn::label("plain", "nothing to set"),
            AgentTurn::label("exit", "reported"),
        ],
    )
    .await;
    assert_eq!(plain.status, ExecutionStatus::Completed);
    assert_eq!(variable(&plain, "stage"), None);
    assert_eq!(variable(&plain, "autopilot"), None);
}

/// A value `set` assigns wins over the one the step returned.
#[tokio::test]
async fn set_overwrites_the_value_the_step_returned() {
    let harness = Harness::new();
    let (_agent, execution) = run(
        &harness,
        "v2/engine_set.yaml",
        vec![
            AgentTurn::with_variables("overwritten", json!({ "summary": "mine" })),
            AgentTurn::label("exit", "reported"),
        ],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(variable(&execution, "summary"), Some(json!("forced")));
}

#[tokio::test]
async fn set_workspace_records_the_pull_request_of_the_named_workspace_only() {
    let harness = Harness::new();
    let (_agent, execution) = run(
        &harness,
        "v2/engine_set_workspace.yaml",
        vec![AgentTurn::with_variables(
            "opened",
            json!({ "pr_url": "https://git.example/g/api/-/merge_requests/42", "pr_number": 42 }),
        )],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let code = &execution.context.workspace_prs["code"];
    assert_eq!(code.pr.as_deref(), Some("42"));
    assert_eq!(
        code.pr_url.as_deref(),
        Some("https://git.example/g/api/-/merge_requests/42")
    );
    assert!(!execution.context.workspace_prs.contains_key("docs"));
}

#[tokio::test]
async fn set_workspace_with_an_empty_value_records_nothing() {
    let harness = Harness::new();
    let (_agent, execution) = run(
        &harness,
        "v2/engine_set_workspace.yaml",
        vec![AgentTurn::label("opened", "no pull request")],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert!(execution.context.workspace_prs.is_empty());
}

#[tokio::test]
async fn set_is_applied_whole_or_not_at_all() {
    let harness = Harness::new();
    let (_agent, execution) = run(
        &harness,
        "v2/engine_set_workspace.yaml",
        vec![AgentTurn::with_variables(
            "rejected",
            json!({ "pr_url": "https://git.example/pr/1" }),
        )],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Failed);
    assert!(execution.context.workspace_prs.is_empty());
    assert_eq!(variable(&execution, "mode"), None);
}

/// `set.local` writes the leaving step's own memory, rendered against the
/// step's result; it never reaches the context.
#[tokio::test]
async fn set_local_writes_the_memory_of_the_step_being_left() {
    let harness = Harness::new();
    let (_agent, execution) = run(
        &harness,
        "v2/engine_set.yaml",
        vec![
            AgentTurn::with_variables("approved", json!({ "summary": "S" })),
            AgentTurn::label("exit", "reported"),
        ],
    )
    .await;

    assert_eq!(
        local_values(&execution, "work")["memo"],
        json!("approved after S")
    );
    assert_eq!(variable(&execution, "memo"), None);
}

/// The event says what was set, on which step and by which label.
#[tokio::test]
async fn setting_a_variable_is_an_event_before_the_transition() {
    let harness = Harness::new();
    let (_agent, execution) = run(
        &harness,
        "v2/engine_set.yaml",
        vec![
            AgentTurn::with_variables("approved", json!({ "summary": "S" })),
            AgentTurn::label("exit", "reported"),
        ],
    )
    .await;

    let set = variables_set(&harness, &execution.id);
    assert!(
        set.contains(&(
            "stage".to_string(),
            json!("reviewed"),
            "work".to_string(),
            "approved".to_string()
        )),
        "{set:?}"
    );
    assert!(
        set.iter().any(|(name, ..)| name == "memo"),
        "a local variable is reported too: {set:?}"
    );
    let all = events(&harness, &execution.id);
    let last_set = all
        .iter()
        .rposition(|data| matches!(data, EventData::VariableSetByWorkflow { .. }))
        .expect("a variable was set");
    let transition = all
        .iter()
        .position(|data| matches!(data, EventData::TransitionTaken { .. }))
        .expect("a transition");
    assert!(last_set < transition, "set comes before the transition");
}

/// A value that renders outside `options` writes nothing from `set`, takes
/// no transition and fails the action.
#[tokio::test]
async fn a_set_value_outside_options_writes_nothing_and_fails_the_action() {
    let harness = Harness::new();
    let (_agent, execution) = run(
        &harness,
        "v2/engine_set.yaml",
        vec![AgentTurn::with_variables(
            "rendered",
            json!({ "summary": "reckless" }),
        )],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Failed);
    assert_eq!(variable(&execution, "stage"), None, "no value was written");
    assert_eq!(variable(&execution, "mode"), None);
    assert!(variables_set(&harness, &execution.id).is_empty());
    assert!(transitions(&harness, &execution.id).is_empty());
    let failures = action_failures(&harness, &execution.id);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("reckless"), "{}", failures[0]);
}

/// A manual, forced jump leaves the step without running its `set`.
#[tokio::test]
async fn a_forced_jump_does_not_run_set() {
    let harness = Harness::new();
    let agent = Arc::new(ScriptedAgentExecutor::new(vec![AgentTurn::Fail]));
    let engine = engine_with(&harness, &agent);
    let definition = workflow_file("v2/engine_set.yaml");
    let failed = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run");
    assert_eq!(failed.status, ExecutionStatus::Failed);

    let transition = ManualTransition::by_operator(None, Some("report".to_string()), true);
    let jumped = engine
        .take_manual_transition(&definition, &failed.id, &transition)
        .expect("take the manual transition");

    assert_eq!(jumped.current_state.as_deref(), Some("report"));
    assert_eq!(variable(&jumped, "stage"), None);
    assert!(variables_set(&harness, &failed.id).is_empty());
}

/// A later step reads the label chosen in an earlier one.
#[tokio::test]
async fn a_step_label_is_readable_by_a_later_prompt() {
    let harness = Harness::new();
    let (agent, execution) = run(
        &harness,
        "v2/engine_set.yaml",
        vec![
            AgentTurn::label("plain", "nothing to set"),
            AgentTurn::label("exit", "reported"),
        ],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert!(
        agent.prompts()[1].contains("Last: plain."),
        "{}",
        agent.prompts()[1]
    );
}

/// The label it reads is the newest one, and set values are visible to the
/// next step's prompt.
#[tokio::test]
async fn the_next_prompt_sees_what_set_wrote() {
    let harness = Harness::new();
    let (agent, _execution) = run(
        &harness,
        "v2/engine_set.yaml",
        vec![
            AgentTurn::with_variables("approved", json!({ "summary": "S" })),
            AgentTurn::label("exit", "reported"),
        ],
    )
    .await;

    let prompt = &agent.prompts()[1];
    assert!(prompt.contains("Last: approved."), "{prompt}");
    assert!(prompt.contains("Stage: [reviewed]."), "{prompt}");
}

/// A part waits for its condition: the first entry has no memory and so no
/// part for it, the next one has, and the reply of the review joins in once
/// the review has answered.
#[tokio::test]
async fn parts_of_a_prompt_appear_when_their_values_do() {
    let harness = Harness::new();
    let (agent, execution) = run(
        &harness,
        "v2/prompt_parts.yaml",
        vec![
            AgentTurn::with_variables("remember", json!({ "memory": "M" })),
            AgentTurn::Label {
                label: "exit",
                reason: "",
                variables: json!({ "remarks": "R" }),
                artifacts: json!({}),
                reply: Some("the review reply"),
                session: true,
            },
            AgentTurn::label("finish", "done"),
        ],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let drafts: Vec<String> = agent
        .prompts()
        .into_iter()
        .filter(|prompt| prompt.starts_with("Draft"))
        .collect();
    assert_eq!(drafts.len(), 2);
    assert!(!drafts[0].contains("MEMORY"), "{}", drafts[0]);
    assert!(!drafts[0].contains("REVIEW"), "{}", drafts[0]);
    assert!(
        drafts[1].contains("\n\nMEMORY[M]\n\nREVIEW[the review reply]"),
        "the parts must follow each other, separated by an empty line: {}",
        drafts[1]
    );
}

/// The values the runner would report for the workspace `code`.
#[derive(Debug)]
struct ReportedWorkspace;

impl WorkspaceValues for ReportedWorkspace {
    fn value(&self, workspace: &str, field: WorkspaceField) -> Option<Value> {
        match (workspace, field) {
            ("code", WorkspaceField::Branch) => Some(json!("feat/x")),
            ("code", WorkspaceField::BaseBranch) => Some(json!("main")),
            ("code", WorkspaceField::Added) => Some(json!(12)),
            _ => None,
        }
    }
}

/// Runs `v2/engine_workspace_read.yaml` through two steps; the first one
/// reports a pull request address.
async fn read_workspace_values(
    engine: Engine,
    agent: &Arc<ScriptedAgentExecutor>,
    harness: &Harness,
) -> WorkflowExecution {
    let engine = engine.with_executor(ActionKind::Agent, agent.clone());
    engine
        .start_and_run(
            &workflow_file("v2/engine_workspace_read.yaml"),
            json!({}),
            harness.workspace(),
        )
        .await
        .expect("run")
}

fn read_turns() -> Vec<AgentTurn> {
    vec![
        AgentTurn::with_variables("opened", json!({ "pr_url": "https://git.example/pr/3" })),
        AgentTurn::label("reviewed", "ok"),
    ]
}

#[tokio::test]
async fn a_step_reads_the_pull_request_the_step_before_recorded_and_the_runners_values() {
    let harness = Harness::new();
    let agent = Arc::new(ScriptedAgentExecutor::new(read_turns()));

    let execution = read_workspace_values(
        harness.engine().with_workspace_values(Arc::new(ReportedWorkspace)),
        &agent,
        &harness,
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(
        agent.prompts()[1],
        "Review https://git.example/pr/3 on [feat/x] with 12 added."
    );
    assert_eq!(variable(&execution, "base"), Some(json!("main")));
}

#[tokio::test]
async fn an_engine_without_workspace_values_renders_the_runners_fields_empty() {
    let harness = Harness::new();
    let agent = Arc::new(ScriptedAgentExecutor::new(read_turns()));

    let execution = read_workspace_values(harness.engine(), &agent, &harness).await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(
        agent.prompts()[1],
        "Review https://git.example/pr/3 on [] with  added."
    );
    assert_eq!(variable(&execution, "base"), Some(json!("")));
}
