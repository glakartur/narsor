//! Integration tests for the context passed between steps: a step's reply
//! read by a later one (U4), required output variables (U5) and the entry
//! limit (U6).
//!
//! The agent is scripted in-process ([`ScriptedAgentExecutor`]); no harness
//! and no daemon run. What is asserted is what the engine hands the agent,
//! what it records, and what the operator is left with.

mod engine_support;

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use engine_support::{workflow, AgentTurn, Harness, ScriptedAgentExecutor};
use narsor_engine::domain::{EventData, ExecutionStatus, WorkflowExecution};
use narsor_engine::engine::{
    ActionKind, Engine, HumanInputDelivery, InputContinuation, ManualTransition, PendingHumanInput,
};
use serde_json::{json, Map, Value};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// An engine on `harness`'s store with `agent` as its agent executor.
fn engine_with(harness: &Harness, agent: &Arc<ScriptedAgentExecutor>) -> Engine {
    harness
        .engine()
        .with_executor(ActionKind::Agent, agent.clone())
}

fn pending(harness: &Harness, execution_id: &str) -> PendingHumanInput {
    harness
        .store()
        .load_pending_input::<PendingHumanInput>(execution_id)
        .expect("read the pending input")
        .expect("a pending input")
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

fn step_replies_dir(harness: &Harness, execution_id: &str) -> PathBuf {
    harness
        .store()
        .paths()
        .step_reply_file(execution_id, "x")
        .parent()
        .expect("a directory")
        .to_path_buf()
}

fn reply_pointer(execution: &WorkflowExecution, state_id: &str) -> Option<String> {
    execution.metadata["engine"]["step_replies"][state_id]
        .as_str()
        .map(ToString::to_string)
}

fn entries(execution: &WorkflowExecution, state_id: &str) -> Value {
    execution.metadata["engine"]["state_entries"][state_id].clone()
}

/// Moves `execution_id` to `target` as the operator, then runs it on.
async fn jump_to(
    engine: &Engine,
    definition: &narsor_engine::domain::WorkflowDefinition,
    execution_id: &str,
    target: &str,
) -> WorkflowExecution {
    let transition = ManualTransition::by_operator(None, Some(target.to_string()), true);
    engine
        .take_manual_transition(definition, execution_id, &transition)
        .expect("take the manual transition");
    engine
        .resume(definition, execution_id)
        .await
        .expect("run after the manual transition")
}

// ---------------------------------------------------------------------------
// U4: the step's reply
// ---------------------------------------------------------------------------

/// AE1: the second round of findings reaches the fix step, not the first.
#[tokio::test]
async fn the_newest_reply_of_a_step_reaches_the_next_prompt() {
    let harness = Harness::new();
    let agent = Arc::new(ScriptedAgentExecutor::new(vec![
        AgentTurn::label("findings", "first report"),
        AgentTurn::label("fixed", "fixed once"),
        AgentTurn::label("findings", "second report"),
        AgentTurn::label("fixed", "fixed twice"),
        AgentTurn::label("clean", "clean"),
    ]));
    let execution = engine_with(&harness, &agent)
        .start_and_run(&workflow("step_reply_loop"), json!({}), harness.workspace())
        .await
        .expect("run");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let prompts = agent.prompts();
    assert!(prompts[1].contains("first report"), "{}", prompts[1]);
    assert!(prompts[3].contains("second report"), "{}", prompts[3]);
    assert!(!prompts[3].contains("first report"), "{}", prompts[3]);
}

/// AE2: an entry that failed keeps the reply of the one before it.
#[tokio::test]
async fn a_failed_entry_keeps_the_earlier_reply() {
    let harness = Harness::new();
    let agent = Arc::new(ScriptedAgentExecutor::new(vec![
        AgentTurn::label("findings", "first report"),
        AgentTurn::label("fixed", "fixed once"),
        AgentTurn::Fail,
        AgentTurn::label("fixed", "fixed again"),
        AgentTurn::label("clean", "clean"),
    ]));
    let engine = engine_with(&harness, &agent);
    let definition = workflow("step_reply_loop");
    let failed = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run");
    assert_eq!(failed.status, ExecutionStatus::Failed);
    assert_eq!(failed.current_state.as_deref(), Some("review"));

    let execution = jump_to(&engine, &definition, &failed.id, "fix").await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let prompts = agent.prompts();
    assert_eq!(prompts.len(), 5);
    assert!(prompts[3].contains("first report"), "{}", prompts[3]);
}

/// AE3: a jump to the fix step before the review ran fails its action with
/// the review named — and a reply file nothing points to is not read.
#[tokio::test]
async fn a_reply_of_a_step_that_never_ran_fails_the_action() {
    let harness = Harness::new();
    let agent = Arc::new(ScriptedAgentExecutor::new(Vec::new()));
    let engine = engine_with(&harness, &agent);
    let definition = workflow("step_reply");
    let started = engine
        .start(&definition, json!({}), harness.workspace())
        .expect("start");
    let orphan = harness
        .store()
        .paths()
        .step_reply_file(&started.id, "orphan");
    fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    fs::write(&orphan, "an orphaned reply").unwrap();

    let execution = jump_to(&engine, &definition, &started.id, "fix").await;

    assert_eq!(execution.status, ExecutionStatus::Failed);
    assert!(agent.calls().is_empty(), "the action never started");
    let failures = action_failures(&harness, &execution.id);
    assert_eq!(failures.len(), 1);
    assert!(
        failures[0].contains("steps.review.reply"),
        "{}",
        failures[0]
    );
}

/// Only a step something refers to keeps its reply.
#[tokio::test]
async fn a_step_nobody_refers_to_stores_no_reply() {
    let harness = Harness::new();
    let agent = Arc::new(ScriptedAgentExecutor::new(vec![
        AgentTurn::label("findings", "P1: validate the request body"),
        AgentTurn::label("done", "fixed"),
    ]));
    let execution = engine_with(&harness, &agent)
        .start_and_run(&workflow("step_reply"), json!({}), harness.workspace())
        .await
        .expect("run");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let files = fs::read_dir(step_replies_dir(&harness, &execution.id))
        .expect("the reply directory")
        .count();
    assert_eq!(files, 1, "only the review's reply");
    assert!(reply_pointer(&execution, "review").is_some());
    assert!(reply_pointer(&execution, "fix").is_none());
}

/// A reply is substituted once: a template in it stays as it is.
#[tokio::test]
async fn a_template_inside_a_reply_is_not_expanded() {
    let harness = Harness::new();
    let agent = Arc::new(ScriptedAgentExecutor::new(vec![
        AgentTurn::label("findings", "use {{ context.x }} here"),
        AgentTurn::label("done", "fixed"),
    ]));
    let execution = engine_with(&harness, &agent)
        .start_and_run(&workflow("step_reply"), json!({}), harness.workspace())
        .await
        .expect("run");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert!(
        agent.prompts()[1].contains("use {{ context.x }} here"),
        "{}",
        agent.prompts()[1]
    );
}

/// A human gate shows the reply, and an engine started afresh on the same
/// store reads the same reply for the next step.
#[tokio::test]
async fn a_gate_shows_the_reply_and_a_new_engine_reads_it_again() {
    let harness = Harness::new();
    let agent = Arc::new(ScriptedAgentExecutor::new(vec![
        AgentTurn::label("findings", "P1: validate the request body"),
        AgentTurn::label("done", "fixed"),
    ]));
    let definition = workflow("step_reply_gate");
    let waiting = engine_with(&harness, &agent)
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run");
    assert_eq!(waiting.status, ExecutionStatus::InputNeeded);
    let gate = pending(&harness, &waiting.id);
    assert!(
        gate.request
            .instruction
            .contains("The review says: P1: validate the request body"),
        "{}",
        gate.request.instruction
    );

    let restarted = engine_with(&harness, &agent);
    let execution = restarted
        .deliver_human_input(
            &definition,
            &waiting.id,
            &HumanInputDelivery::label("fix").for_episode(gate.episode_key()),
        )
        .await
        .expect("answer the gate");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert!(agent.prompts()[1].contains("P1: validate the request body"));
}

/// A pointer whose file is gone is a reply that is not there: the action
/// fails permanently, with the step named.
#[tokio::test]
async fn a_reply_pointer_without_its_file_fails_the_action() {
    let harness = Harness::new();
    let agent = Arc::new(ScriptedAgentExecutor::new(vec![AgentTurn::label(
        "findings",
        "P1: validate the request body",
    )]));
    let definition = workflow("step_reply_gate");
    let waiting = engine_with(&harness, &agent)
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run");
    let attempt = reply_pointer(&waiting, "review").expect("a pointer");
    fs::remove_file(
        harness
            .store()
            .paths()
            .step_reply_file(&waiting.id, &attempt),
    )
    .unwrap();

    let gate = pending(&harness, &waiting.id);
    let execution = engine_with(&harness, &agent)
        .deliver_human_input(
            &definition,
            &waiting.id,
            &HumanInputDelivery::label("fix").for_episode(gate.episode_key()),
        )
        .await
        .expect("answer the gate");

    assert_eq!(execution.status, ExecutionStatus::Failed);
    assert_eq!(agent.calls().len(), 1, "the fix step never started");
    let failures = action_failures(&harness, &execution.id);
    assert!(
        failures.last().unwrap().contains("steps.review.reply"),
        "{failures:?}"
    );
}

/// A reply file that cannot be read for another reason fails the action
/// with the step, the attempt and the path named.
#[tokio::test]
async fn an_unreadable_reply_file_fails_the_action_naming_the_step() {
    let harness = Harness::new();
    let agent = Arc::new(ScriptedAgentExecutor::new(vec![AgentTurn::label(
        "findings",
        "P1: validate the request body",
    )]));
    let definition = workflow("step_reply_gate");
    let waiting = engine_with(&harness, &agent)
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run");
    let attempt = reply_pointer(&waiting, "review").expect("a pointer");
    let file = harness
        .store()
        .paths()
        .step_reply_file(&waiting.id, &attempt);
    fs::remove_file(&file).unwrap();
    fs::create_dir(&file).unwrap();

    let gate = pending(&harness, &waiting.id);
    let execution = engine_with(&harness, &agent)
        .deliver_human_input(
            &definition,
            &waiting.id,
            &HumanInputDelivery::label("fix").for_episode(gate.episode_key()),
        )
        .await
        .expect("answer the gate");

    assert_eq!(execution.status, ExecutionStatus::Failed);
    let failures = action_failures(&harness, &execution.id);
    let message = failures.last().unwrap();
    assert!(message.contains("step 'review'"), "{message}");
    assert!(message.contains(&attempt), "{message}");
    assert!(message.contains(&file.display().to_string()), "{message}");
}

// ---------------------------------------------------------------------------
// U5: required variables
// ---------------------------------------------------------------------------

fn count_events(harness: &Harness, execution_id: &str, matches: fn(&EventData) -> bool) -> usize {
    events(harness, execution_id)
        .iter()
        .filter(|data| matches(data))
        .count()
}

/// Runs `required_variable` on `turns`, without a conductor.
async fn run_required(
    harness: &Harness,
    turns: Vec<AgentTurn>,
) -> (Engine, Arc<ScriptedAgentExecutor>, WorkflowExecution) {
    let agent = Arc::new(ScriptedAgentExecutor::new(turns));
    let engine = engine_with(harness, &agent);
    let execution = engine
        .start_and_run(
            &workflow("required_variable"),
            json!({}),
            harness.workspace(),
        )
        .await
        .expect("run");
    (engine, agent, execution)
}

/// AE4: a label without its required variable is asked for again in the
/// same session, the reason names the variable, and the step's entry count
/// stays as it was.
#[tokio::test]
async fn a_missing_required_variable_is_asked_for_again_in_the_same_session() {
    let harness = Harness::new();
    let (_engine, agent, execution) = run_required(
        &harness,
        vec![
            AgentTurn::with_variables("plan_ready", json!({})),
            AgentTurn::with_variables("plan_ready", json!({ "plan_doc": "docs/plan.md" })),
        ],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(
        execution.context.variables.get("plan_doc"),
        Some(&json!("docs/plan.md"))
    );
    let calls = agent.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].resume_session.as_deref(), Some("agent-session"));
    assert!(calls[1].prompt.contains("plan_doc"), "{}", calls[1].prompt);
    let reasons: Vec<String> = events(&harness, &execution.id)
        .into_iter()
        .filter_map(|data| match data {
            EventData::ActionOutputRejected { reason, .. } => Some(reason),
            _ => None,
        })
        .collect();
    assert_eq!(reasons.len(), 1);
    assert!(reasons[0].contains("plan_doc"), "{}", reasons[0]);
    assert_eq!(entries(&execution, "plan"), json!(1));
}

/// Blank text and `null` are no value; the text `none` is one.
#[tokio::test]
async fn a_blank_or_null_required_variable_is_missing_but_none_is_a_value() {
    let harness = Harness::new();
    let (_engine, agent, execution) = run_required(
        &harness,
        vec![
            AgentTurn::with_variables("plan_ready", json!({ "plan_doc": "  " })),
            AgentTurn::with_variables("plan_ready", json!({ "plan_doc": null })),
        ],
    )
    .await;
    assert_eq!(execution.status, ExecutionStatus::InputNeeded);
    assert_eq!(agent.calls().len(), 2);
    assert_eq!(
        count_events(&harness, &execution.id, |data| matches!(
            data,
            EventData::ActionOutputRejected { .. }
        )),
        2
    );

    let harness = Harness::new();
    let (_engine, agent, execution) = run_required(
        &harness,
        vec![AgentTurn::with_variables(
            "plan_ready",
            json!({ "plan_doc": "none" }),
        )],
    )
    .await;
    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(agent.calls().len(), 1);
}

/// A variable required by one label is not required by another.
#[tokio::test]
async fn a_label_without_required_variables_passes_without_them() {
    let harness = Harness::new();
    let (_engine, agent, execution) = run_required(
        &harness,
        vec![AgentTurn::with_variables("clean", json!({}))],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(agent.calls().len(), 1);
}

/// A second miss is not an action failure: the step goes on to the rescue,
/// which without a conductor is a problem for the operator.
#[tokio::test]
async fn a_second_miss_is_rescued_not_failed() {
    let harness = Harness::new();
    let (_engine, _agent, execution) = run_required(
        &harness,
        vec![
            AgentTurn::with_variables("plan_ready", json!({})),
            AgentTurn::with_variables("plan_ready", json!({})),
        ],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::InputNeeded);
    assert_eq!(
        pending(&harness, &execution.id).continuation,
        InputContinuation::ResolveOutput
    );
    assert_eq!(
        count_events(&harness, &execution.id, |data| matches!(
            data,
            EventData::ProblemRaised { .. }
        )),
        1
    );
    assert_eq!(
        count_events(&harness, &execution.id, |data| matches!(
            data,
            EventData::ActionFailed { .. }
        )),
        0
    );
}

/// A missing label and a missing variable share the engine's one reprompt.
#[tokio::test]
async fn a_missing_label_and_a_missing_variable_share_one_reprompt() {
    let harness = Harness::new();
    let (_engine, agent, execution) = run_required(
        &harness,
        vec![
            AgentTurn::NoLabel {
                reply: Some("the plan is ready"),
                session: true,
            },
            AgentTurn::with_variables("plan_ready", json!({})),
        ],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::InputNeeded);
    assert_eq!(agent.calls().len(), 2);
    assert_eq!(
        count_events(&harness, &execution.id, |data| matches!(
            data,
            EventData::OutputReprompted { .. }
        )),
        1
    );
}

/// The operator settles the problem only with the variable the label needs.
#[tokio::test]
async fn the_operator_must_give_the_required_variable() {
    let harness = Harness::new();
    let (engine, _agent, execution) = run_required(
        &harness,
        vec![
            AgentTurn::with_variables("plan_ready", json!({})),
            AgentTurn::with_variables("plan_ready", json!({})),
        ],
    )
    .await;
    let definition = workflow("required_variable");
    let problem = pending(&harness, &execution.id);

    let error = engine
        .deliver_human_input(
            &definition,
            &execution.id,
            &HumanInputDelivery::label("plan_ready").for_episode(problem.episode_key()),
        )
        .await
        .expect_err("the variable is missing");
    assert!(error.to_string().contains("plan_doc"), "{error}");
    assert_eq!(
        harness
            .store()
            .load_execution(&execution.id)
            .unwrap()
            .status,
        ExecutionStatus::InputNeeded
    );

    let execution = engine
        .deliver_human_input(
            &definition,
            &execution.id,
            &HumanInputDelivery::label("plan_ready")
                .with_variables(Map::from_iter([(
                    "plan_doc".to_string(),
                    json!("docs/plan.md"),
                )]))
                .for_episode(problem.episode_key()),
        )
        .await
        .expect("settle the problem");
    assert_eq!(execution.status, ExecutionStatus::Completed);
}

/// An output that needs approval is checked before the person sees it: no
/// approval is asked for a result without its variable.
#[tokio::test]
async fn an_incomplete_result_is_never_offered_for_approval() {
    let harness = Harness::new();
    let (_engine, agent, execution) = run_required(
        &harness,
        vec![
            AgentTurn::with_variables("approved_plan", json!({})),
            AgentTurn::with_variables("approved_plan", json!({ "plan_doc": "docs/plan.md" })),
        ],
    )
    .await;

    assert_eq!(agent.calls().len(), 2);
    assert_eq!(execution.status, ExecutionStatus::InputNeeded);
    assert_eq!(
        pending(&harness, &execution.id).continuation,
        InputContinuation::ApproveOutput
    );
    let log = events(&harness, &execution.id);
    let reprompt = log
        .iter()
        .position(|data| matches!(data, EventData::OutputReprompted { .. }))
        .expect("a reprompt");
    let requested = log
        .iter()
        .position(|data| matches!(data, EventData::HumanInputRequested { .. }))
        .expect("an approval request");
    assert!(reprompt < requested);
    assert_eq!(
        count_events(&harness, &execution.id, |data| matches!(
            data,
            EventData::HumanInputRequested { .. }
        )),
        1
    );
}

// ---------------------------------------------------------------------------
// The step contract: required artifacts, undeclared variables, new roots
// ---------------------------------------------------------------------------

async fn run_artifact_contract(
    harness: &Harness,
    turns: Vec<AgentTurn>,
) -> (Arc<ScriptedAgentExecutor>, WorkflowExecution) {
    let agent = Arc::new(ScriptedAgentExecutor::new(turns));
    let execution = engine_with(harness, &agent)
        .start_and_run(
            &workflow("artifact_contract"),
            json!({}),
            harness.workspace(),
        )
        .await
        .expect("run");
    (agent, execution)
}

fn output_rejections(harness: &Harness, execution_id: &str) -> Vec<String> {
    events(harness, execution_id)
        .into_iter()
        .filter_map(|data| match data {
            EventData::ActionOutputRejected { reason, .. } => Some(reason),
            _ => None,
        })
        .collect()
}

/// A choice without its required artifact is asked for again, naming it.
#[tokio::test]
async fn a_missing_required_artifact_is_asked_for_again_by_name() {
    let harness = Harness::new();
    let (agent, execution) = run_artifact_contract(
        &harness,
        vec![
            AgentTurn::with_artifacts("ready", json!({})),
            AgentTurn::with_artifacts("ready", json!({ "plan_doc": "docs/plan.md" })),
            AgentTurn::label("done", "worked"),
        ],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let reasons = output_rejections(&harness, &execution.id);
    assert_eq!(reasons.len(), 1, "{reasons:?}");
    assert!(reasons[0].contains("plan_doc"), "{}", reasons[0]);
    let calls = agent.calls();
    assert_eq!(calls[1].resume_session.as_deref(), Some("agent-session"));
    assert!(calls[1].prompt.contains("plan_doc"), "{}", calls[1].prompt);
}

/// `{{ artifacts.NAME }}` is the path the artifact was written to; before
/// any write, a defined artifact and a declared variable render empty.
#[tokio::test]
async fn artifacts_and_declared_variables_reach_the_prompt() {
    let harness = Harness::new();
    let (agent, execution) = run_artifact_contract(
        &harness,
        vec![
            AgentTurn::with_artifacts("ready", json!({ "plan_doc": "docs/plan.md" })),
            AgentTurn::label("done", "worked"),
        ],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let prompts = agent.prompts();
    assert_eq!(prompts[0], "Plan. Previous plan: []. Blocker: [].");
    assert_eq!(prompts[1], "Work from docs/plan.md.");
}

/// A step's `local.*` variable is its own: it never reaches the context.
#[tokio::test]
async fn a_local_variable_stays_out_of_the_context() {
    let harness = Harness::new();
    let (_agent, execution) = run_artifact_contract(
        &harness,
        vec![
            AgentTurn::Label {
                label: "ready",
                reason: "",
                variables: json!({ "local.notes": "tried the short plan" }),
                artifacts: json!({ "plan_doc": "docs/plan.md" }),
                reply: None,
                session: true,
            },
            AgentTurn::label("done", "worked"),
        ],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert!(
        !execution.context.variables.contains_key("local.notes"),
        "{:?}",
        execution.context.variables
    );
}

/// A variable the step does not define is kept as additional data with a
/// warning, never written to the context, and never reprompted for.
#[tokio::test]
async fn an_undeclared_variable_goes_to_additional_data_not_the_context() {
    let harness = Harness::new();
    let (_engine, agent, execution) = run_required(
        &harness,
        vec![AgentTurn::with_variables(
            "clean",
            json!({ "extra": "kept aside" }),
        )],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(agent.calls().len(), 1);
    assert!(!execution.context.variables.contains_key("extra"));
    let (additional_data, warnings) = events(&harness, &execution.id)
        .into_iter()
        .find_map(|data| match data {
            EventData::ActionCompleted {
                additional_data,
                warnings,
                ..
            } => Some((additional_data, warnings)),
            _ => None,
        })
        .expect("the step completes");
    assert_eq!(additional_data.get("extra"), Some(&json!("kept aside")));
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("extra"), "{}", warnings[0]);
}

// ---------------------------------------------------------------------------
// U6: the entry count and the limit
// ---------------------------------------------------------------------------

fn answer(text: &str) -> Map<String, Value> {
    Map::from_iter([("answer".to_string(), json!(text))])
}

fn note(text: &str) -> Map<String, Value> {
    Map::from_iter([("note".to_string(), json!(text))])
}

fn entry_limit_decision(decision: &str) -> HumanInputDelivery {
    HumanInputDelivery::with_values(Map::from_iter([(
        "entry_limit_decision".to_string(),
        json!(decision),
    )]))
}

/// Answers the pending input of `execution_id` with `delivery`.
async fn deliver(
    engine: &Engine,
    harness: &Harness,
    name: &str,
    execution_id: &str,
    delivery: HumanInputDelivery,
) -> WorkflowExecution {
    let episode = pending(harness, execution_id).episode_key().to_string();
    engine
        .deliver_human_input(
            &workflow(name),
            execution_id,
            &delivery.for_episode(episode),
        )
        .await
        .expect("deliver the answer")
}

/// `entry_limit` run until `review` waits on its limit: two rounds of
/// findings and fixes, then the agent of `then`.
async fn at_review_limit(
    harness: &Harness,
    then: Vec<AgentTurn>,
) -> (Engine, Arc<ScriptedAgentExecutor>, WorkflowExecution) {
    let mut turns = vec![
        AgentTurn::label("findings", "first findings"),
        AgentTurn::label("fixed", "fixed once"),
        AgentTurn::label("findings", "second findings"),
        AgentTurn::label("fixed", "fixed twice"),
    ];
    turns.extend(then);
    let agent = Arc::new(ScriptedAgentExecutor::new(turns));
    let engine = engine_with(harness, &agent);
    let execution = engine
        .start_and_run(&workflow("entry_limit"), json!({}), harness.workspace())
        .await
        .expect("run");
    assert_eq!(execution.status, ExecutionStatus::InputNeeded);
    assert_eq!(
        pending(harness, &execution.id).continuation,
        InputContinuation::EntryLimit
    );
    (engine, agent, execution)
}

/// AE5: questions to the person and a permission decision resume the step;
/// none of them is an entry.
#[tokio::test]
async fn answers_and_permission_decisions_are_not_entries() {
    let harness = Harness::new();
    let agent = Arc::new(ScriptedAgentExecutor::new(vec![
        AgentTurn::Ask("Which API version?"),
        AgentTurn::Ask("And which endpoint?"),
        AgentTurn::Permission,
        AgentTurn::label("checked", "checked"),
    ]));
    let engine = engine_with(&harness, &agent);
    let definition = workflow("entry_limit_paths");
    let execution = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run");
    assert_eq!(entries(&execution, "work"), json!(1));

    deliver(
        &engine,
        &harness,
        "entry_limit_paths",
        &execution.id,
        HumanInputDelivery::with_values(answer("v2")),
    )
    .await;
    let waiting = deliver(
        &engine,
        &harness,
        "entry_limit_paths",
        &execution.id,
        HumanInputDelivery::with_values(answer("/users")),
    )
    .await;
    assert_eq!(
        pending(&harness, &waiting.id).continuation,
        InputContinuation::ResolvePermission
    );
    let decision = narsor_engine::engine::PermissionDecision {
        denial_index: Some(0),
        decision: narsor_engine::engine::PermissionDecisionKind::Allow,
        scope: Some(narsor_engine::engine::PermissionGrantScope::ExactOperation),
        pattern: None,
        message: None,
    };
    let episode = pending(&harness, &waiting.id).episode_key().to_string();
    let execution = engine
        .resolve_permission(&definition, &waiting.id, decision, &episode)
        .await
        .expect("resolve the permission");

    assert_eq!(agent.calls().len(), 4);
    assert_eq!(execution.current_state.as_deref(), Some("gate"));
    assert_eq!(entries(&execution, "work"), json!(1));
}

/// AE6: a retry under `retry_policy` is the same entry.
#[tokio::test]
async fn a_retry_is_not_an_entry() {
    let harness = Harness::new();
    let agent = Arc::new(ScriptedAgentExecutor::new(vec![
        AgentTurn::Fail,
        AgentTurn::label("checked", "checked"),
    ]));
    let execution = engine_with(&harness, &agent)
        .start_and_run(
            &workflow("entry_limit_paths"),
            json!({}),
            harness.workspace(),
        )
        .await
        .expect("run");

    assert_eq!(agent.calls().len(), 2);
    assert_eq!(execution.current_state.as_deref(), Some("gate"));
    assert_eq!(
        count_events(&harness, &execution.id, |data| matches!(
            data,
            EventData::ActionRetrying { .. }
        )),
        1
    );
    assert_eq!(entries(&execution, "work"), json!(1));
}

/// An approval sent back to the agent resumes the same entry.
#[tokio::test]
async fn an_approval_sent_back_is_not_an_entry() {
    let harness = Harness::new();
    let agent = Arc::new(ScriptedAgentExecutor::new(vec![
        AgentTurn::label("done", "done"),
        AgentTurn::label("checked", "checked"),
    ]));
    let engine = engine_with(&harness, &agent);
    let execution = engine
        .start_and_run(
            &workflow("entry_limit_paths"),
            json!({}),
            harness.workspace(),
        )
        .await
        .expect("run");
    assert_eq!(
        pending(&harness, &execution.id).continuation,
        InputContinuation::ApproveOutput
    );

    let mut rejection = HumanInputDelivery::default()
        .with_action(narsor_engine::engine::actions::human::HumanInputAction::Resume);
    rejection.comment = Some("Check the tests too.".to_string());
    let execution = deliver(
        &engine,
        &harness,
        "entry_limit_paths",
        &execution.id,
        rejection,
    )
    .await;

    assert_eq!(agent.calls().len(), 2);
    assert_eq!(execution.current_state.as_deref(), Some("gate"));
    assert_eq!(entries(&execution, "work"), json!(1));
}

/// A gate's comment sent back to the agent continues the step, with or
/// without a session to continue in; neither is an entry.
#[tokio::test]
async fn a_comment_sent_back_from_a_gate_is_not_an_entry() {
    for first in [
        AgentTurn::label("checked", "checked"),
        AgentTurn::sessionless("checked"),
    ] {
        let with_session = matches!(first, AgentTurn::Label { session: true, .. });
        let harness = Harness::new();
        let agent = Arc::new(ScriptedAgentExecutor::new(vec![
            first,
            AgentTurn::label("checked", "checked again"),
        ]));
        let engine = engine_with(&harness, &agent);
        let gate = engine
            .start_and_run(
                &workflow("entry_limit_paths"),
                json!({}),
                harness.workspace(),
            )
            .await
            .expect("run");
        assert_eq!(gate.current_state.as_deref(), Some("gate"));

        let mut execution = deliver(
            &engine,
            &harness,
            "entry_limit_paths",
            &gate.id,
            HumanInputDelivery::with_values(note("Rename the flag.")).choosing("revise"),
        )
        .await;
        if !with_session {
            // The saved session is gone: the operator sends the comment to
            // a new one.
            assert_eq!(execution.current_state.as_deref(), Some("work"));
            execution = deliver(
                &engine,
                &harness,
                "entry_limit_paths",
                &gate.id,
                HumanInputDelivery::with_values(answer("Rename the flag.")),
            )
            .await;
        }

        assert_eq!(agent.calls().len(), 2, "with session: {with_session}");
        assert_eq!(execution.current_state.as_deref(), Some("gate"));
        assert_eq!(
            entries(&execution, "work"),
            json!(1),
            "with session: {with_session}"
        );
    }
}

/// AE7: on its limit the step does not start; "continue" counts one more
/// entry and runs it, and the next entry past the limit stops again. An
/// engine started afresh on the same store finds the same wait.
#[tokio::test]
async fn the_limit_stops_before_the_step_and_continue_counts_one_entry() {
    let harness = Harness::new();
    let (_engine, agent, execution) = at_review_limit(
        &harness,
        vec![
            AgentTurn::label("findings", "third findings"),
            AgentTurn::label("fixed", "fixed thrice"),
        ],
    )
    .await;
    assert_eq!(entries(&execution, "review"), json!(2));
    let log = events(&harness, &execution.id);
    let reached = log
        .iter()
        .position(|data| {
            matches!(data, EventData::EntryLimitReached { state_id, entries: 2, max_entries: 2 }
                if state_id == "review")
        })
        .expect("the stop is an event");
    assert!(
        !log[reached..]
            .iter()
            .any(|data| matches!(data, EventData::ActionStarted { .. })),
        "the limited step did not start"
    );
    assert_eq!(agent.calls().len(), 4);

    let restarted = engine_with(&harness, &agent);
    let reloaded = restarted.store().load_execution(&execution.id).unwrap();
    assert_eq!(reloaded.status, ExecutionStatus::InputNeeded);
    assert_eq!(entries(&reloaded, "review"), json!(2));

    let execution = deliver(
        &restarted,
        &harness,
        "entry_limit",
        &execution.id,
        entry_limit_decision("continue"),
    )
    .await;
    // `review` ran once more, and `fix`, on its own limit, stopped.
    assert_eq!(entries(&execution, "review"), json!(3));
    assert_eq!(execution.current_state.as_deref(), Some("fix"));
    assert_eq!(execution.status, ExecutionStatus::InputNeeded);

    let execution = deliver(
        &restarted,
        &harness,
        "entry_limit",
        &execution.id,
        entry_limit_decision("continue"),
    )
    .await;
    assert_eq!(execution.current_state.as_deref(), Some("review"));
    assert_eq!(execution.status, ExecutionStatus::InputNeeded);
    assert_eq!(entries(&execution, "review"), json!(3));
    assert_eq!(agent.calls().len(), 6);
    assert_eq!(
        count_events(&harness, &execution.id, |data| matches!(
            data,
            EventData::EntryLimitReached { state_id, entries: 3, .. } if state_id == "review"
        )),
        1
    );
}

/// AE8: choosing an output of the limited step takes its transition without
/// running it, and leaves its reply as it was.
#[tokio::test]
async fn choosing_an_output_at_the_limit_keeps_the_steps_reply() {
    let harness = Harness::new();
    let (engine, agent, waiting) = at_review_limit(&harness, Vec::new()).await;
    let pointer = reply_pointer(&waiting, "review").expect("a pointer");

    let execution = deliver(
        &engine,
        &harness,
        "entry_limit",
        &waiting.id,
        HumanInputDelivery::label("clean"),
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(agent.calls().len(), 4);
    assert_eq!(reply_pointer(&execution, "review"), Some(pointer));
    assert_eq!(entries(&execution, "review"), json!(2));
}

/// The initial state counts its first entry: with a limit of one, the first
/// transition back to it stops.
#[tokio::test]
async fn the_initial_state_counts_its_first_entry() {
    let harness = Harness::new();
    let agent = Arc::new(ScriptedAgentExecutor::new(vec![AgentTurn::label(
        "again", "again",
    )]));
    let execution = engine_with(&harness, &agent)
        .start_and_run(
            &workflow("entry_limit_initial"),
            json!({}),
            harness.workspace(),
        )
        .await
        .expect("run");

    assert_eq!(agent.calls().len(), 1);
    assert_eq!(execution.status, ExecutionStatus::InputNeeded);
    assert_eq!(
        pending(&harness, &execution.id).continuation,
        InputContinuation::EntryLimit
    );
    assert_eq!(entries(&execution, "review"), json!(1));
}

/// A decision for an earlier wait is refused, and the same decision sent
/// twice changes the count once.
#[tokio::test]
async fn a_stale_or_repeated_decision_changes_nothing() {
    let harness = Harness::new();
    let (engine, _agent, waiting) =
        at_review_limit(&harness, vec![AgentTurn::label("clean", "clean")]).await;
    let definition = workflow("entry_limit");

    let stale = entry_limit_decision("continue").for_episode("an-earlier-wait");
    engine
        .deliver_human_input(&definition, &waiting.id, &stale)
        .await
        .expect_err("a stale episode");
    let reloaded = harness.store().load_execution(&waiting.id).unwrap();
    assert_eq!(reloaded.status, ExecutionStatus::InputNeeded);
    assert_eq!(entries(&reloaded, "review"), json!(2));

    let decision =
        entry_limit_decision("continue").for_episode(pending(&harness, &waiting.id).episode_key());
    let execution = engine
        .deliver_human_input(&definition, &waiting.id, &decision)
        .await
        .expect("the decision");
    assert_eq!(execution.status, ExecutionStatus::Completed);
    engine
        .deliver_human_input(&definition, &waiting.id, &decision)
        .await
        .expect_err("the same decision again");
    let reloaded = harness.store().load_execution(&waiting.id).unwrap();
    assert_eq!(entries(&reloaded, "review"), json!(3));
    assert_eq!(
        count_events(&harness, &waiting.id, |data| matches!(
            data,
            EventData::EntryLimitDecision { .. }
        )),
        1
    );
}

/// A manual transition elsewhere closes the wait on the limit.
#[tokio::test]
async fn a_manual_transition_closes_the_wait_on_the_limit() {
    let harness = Harness::new();
    let (engine, _agent, waiting) = at_review_limit(&harness, Vec::new()).await;

    let transition = ManualTransition::by_operator(None, Some("completed".to_string()), true);
    let moved = engine
        .take_manual_transition(&workflow("entry_limit"), &waiting.id, &transition)
        .expect("take the manual transition");

    assert_eq!(moved.current_state.as_deref(), Some("completed"));
    assert!(harness
        .store()
        .load_pending_input::<PendingHumanInput>(&waiting.id)
        .unwrap()
        .is_none());
    assert!(events(&harness, &waiting.id).iter().any(|data| matches!(
        data,
        EventData::HumanInputReceived {
            close_reason: Some(narsor_engine::domain::InputCloseReason::Transition),
            ..
        }
    )));
}

/// A manual transition to a step on its limit counts the entry and runs
/// the step: the operator chose it (R10a).
#[tokio::test]
async fn a_manual_transition_runs_a_step_on_its_limit() {
    let harness = Harness::new();
    let (engine, agent, waiting) =
        at_review_limit(&harness, vec![AgentTurn::label("clean", "clean after all")]).await;

    let execution = jump_to(&engine, &workflow("entry_limit"), &waiting.id, "review").await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(agent.calls().len(), 5);
    assert_eq!(entries(&execution, "review"), json!(3));
}

// ---------------------------------------------------------------------------
// U5: the step's memory (`local.*`) and the history of its entries
// ---------------------------------------------------------------------------

/// The first line of the history block the engine adds to a prompt.
const HISTORY_HEADING: &str = "Step history (recorded by the workflow";

/// The prompt of `plan` in `step_history` on an entry with no memory.
const PLAN_PROMPT: &str = "Plan. Memory: [].";

/// The choice `label` with `reason` and `variables`.
fn choice(label: &'static str, reason: &str, variables: Value) -> AgentTurn {
    AgentTurn::Label {
        label,
        reason: Box::leak(reason.to_string().into_boxed_str()),
        variables,
        artifacts: json!({}),
        reply: None,
        session: true,
    }
}

/// `plan` sends the work back with `reason`; brainstorm drafts again.
fn revision(reason: &str, variables: Value) -> [AgentTurn; 2] {
    [
        choice("needs_requirements_revision", reason, variables),
        AgentTurn::label("drafted", "drafted"),
    ]
}

/// `step_history`, changed by `change`, run on `turns`.
async fn run_history(
    harness: &Harness,
    change: impl FnOnce(&mut narsor_engine::domain::AgentActionConfig),
    turns: Vec<AgentTurn>,
) -> (Arc<ScriptedAgentExecutor>, WorkflowExecution) {
    let mut definition = workflow("step_history");
    let plan = definition.states.get_mut("plan").expect("a plan step");
    let Some(narsor_engine::domain::ActionConfig::Agent(config)) = plan.action.as_mut() else {
        panic!("plan is an agent step");
    };
    change(config);
    let agent = Arc::new(ScriptedAgentExecutor::new(turns));
    let execution = engine_with(harness, &agent)
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run");
    (agent, execution)
}

/// The prompts `plan` was given, in order.
fn plan_prompts(agent: &ScriptedAgentExecutor) -> Vec<String> {
    agent
        .prompts()
        .into_iter()
        .filter(|prompt| prompt.starts_with("Plan."))
        .collect()
}

fn step_attempts(execution: &WorkflowExecution, state_id: &str) -> Value {
    execution.metadata["engine"]["step_attempts"][state_id].clone()
}

/// Covers AE1. The first entry gets the author's prompt and nothing else;
/// the contract is the executor's to add.
#[tokio::test]
async fn the_first_entry_gets_only_the_authors_prompt() {
    let harness = Harness::new();
    let (agent, execution) = run_history(
        &harness,
        |_| {},
        vec![choice("plan_ready", "the plan is done", json!({}))],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(agent.prompts(), vec![PLAN_PROMPT.to_string()]);
}

/// Covers AE1. The fourth entry after three revisions names its number and
/// lists the three earlier choices with their reasons.
#[tokio::test]
async fn the_fourth_entry_lists_the_three_earlier_choices() {
    let harness = Harness::new();
    let mut turns = Vec::new();
    for reason in ["no API named", "no users named", "no limits named"] {
        turns.extend(revision(reason, json!({})));
    }
    turns.push(choice("plan_ready", "planned", json!({})));
    let (agent, execution) = run_history(&harness, |_| {}, turns).await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let prompts = plan_prompts(&agent);
    assert_eq!(prompts.len(), 4);
    let fourth = &prompts[3];
    assert!(fourth.starts_with(PLAN_PROMPT), "{fourth}");
    assert!(fourth.contains(HISTORY_HEADING), "{fourth}");
    assert!(
        fourth.contains("This is entry 4 into this step."),
        "{fourth}"
    );
    for reason in ["no API named", "no users named", "no limits named"] {
        let line = format!("- needs_requirements_revision — {reason}\n");
        assert!(fourth.contains(&line), "{line:?} in {fourth}");
    }
    assert!(
        prompts[1].contains("This is entry 2 into this step."),
        "{}",
        prompts[1]
    );
}

/// Covers AE2. A note kept by one attempt survives an attempt that does
/// not set it, and a new note replaces it whole. It never reaches the
/// context.
#[tokio::test]
async fn a_local_note_survives_until_it_is_replaced() {
    let harness = Harness::new();
    let mut turns = Vec::new();
    turns.extend(revision("first", json!({ "local.memory": "A" })));
    turns.extend(revision("second", json!({})));
    turns.extend(revision("third", json!({ "local.memory": "B" })));
    turns.push(choice("plan_ready", "planned", json!({})));
    let (agent, execution) = run_history(&harness, |_| {}, turns).await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let prompts = plan_prompts(&agent);
    assert!(
        prompts[1].starts_with("Plan. Memory: [A]."),
        "{}",
        prompts[1]
    );
    assert!(
        prompts[2].starts_with("Plan. Memory: [A]."),
        "{}",
        prompts[2]
    );
    assert!(
        prompts[3].starts_with("Plan. Memory: [B]."),
        "{}",
        prompts[3]
    );
    assert_eq!(
        execution.metadata["engine"]["step_locals"]["plan"],
        json!({ "local.memory": "B" })
    );
}

/// Covers AE3. A declared note with no value yet renders empty, and the
/// action starts.
#[tokio::test]
async fn a_declared_note_without_a_value_renders_empty() {
    let harness = Harness::new();
    let (agent, execution) = run_history(
        &harness,
        |_| {},
        vec![choice("plan_ready", "planned", json!({}))],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(agent.calls().len(), 1);
    assert!(action_failures(&harness, &execution.id).is_empty());
    assert_eq!(agent.prompts()[0], PLAN_PROMPT);
}

/// Covers AE5. A prompt that places `{{ step.history }}` gets the history
/// there, once.
#[tokio::test]
async fn the_history_goes_where_the_prompt_places_it_once() {
    let harness = Harness::new();
    let mut turns = Vec::new();
    turns.extend(revision("no API named", json!({})));
    turns.push(choice("plan_ready", "planned", json!({})));
    let (agent, _execution) = run_history(
        &harness,
        |config| config.prompt = "Plan.\n{{ step.history }}\nEnd of plan.".to_string(),
        turns,
    )
    .await;

    let second = &plan_prompts(&agent)[1];
    assert_eq!(second.matches(HISTORY_HEADING).count(), 1, "{second}");
    assert!(second.starts_with("Plan.\nStep history"), "{second}");
    assert!(second.ends_with("\nEnd of plan."), "{second}");
    assert!(
        second.contains("- needs_requirements_revision — no API named"),
        "{second}"
    );
}

/// `history: false` leaves the second entry's prompt as the author wrote it.
#[tokio::test]
async fn a_step_without_history_gets_none_on_its_second_entry() {
    let harness = Harness::new();
    let mut turns = Vec::new();
    turns.extend(revision("no API named", json!({})));
    turns.push(choice("plan_ready", "planned", json!({})));
    let (agent, _execution) = run_history(&harness, |config| config.history = false, turns).await;

    assert_eq!(plan_prompts(&agent)[1], PLAN_PROMPT);
}

/// An attempt that failed with a timeout is in the history of the next
/// entry as an error of that type; the recovery itself is the same entry.
#[tokio::test]
async fn a_failed_attempt_is_shown_with_its_error_type() {
    let harness = Harness::new();
    let mut turns = vec![AgentTurn::FailOfType("timeout")];
    turns.extend(revision("no API named", json!({})));
    turns.push(choice("plan_ready", "planned", json!({})));
    let agent = Arc::new(ScriptedAgentExecutor::new(turns));
    let engine = Arc::new(engine_with(&harness, &agent));
    let definition = workflow("step_history");
    let failed = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run");
    assert_eq!(failed.status, ExecutionStatus::Failed);

    let execution = engine
        .try_begin_recover(&failed.id, None)
        .expect("recover")
        .run_to_completion(&definition)
        .await
        .expect("run after the recovery");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let prompts = plan_prompts(&agent);
    assert_eq!(
        prompts[1], PLAN_PROMPT,
        "the recovery is the first entry still"
    );
    let second_entry = &prompts[2];
    assert!(
        second_entry.contains("This is entry 2 into this step."),
        "{second_entry}"
    );
    assert!(
        second_entry.contains("- error: timeout\n"),
        "{second_entry}"
    );
    assert!(
        second_entry.contains("- needs_requirements_revision — no API named"),
        "{second_entry}"
    );
}

/// A question answered by the person resumes the agent's session: the
/// resumed prompt carries no history, and the entry number does not move.
#[tokio::test]
async fn an_answered_question_is_not_an_entry_and_gets_no_history() {
    let harness = Harness::new();
    let mut turns = Vec::new();
    turns.extend(revision("no API named", json!({})));
    turns.push(AgentTurn::Ask("Which API?"));
    turns.extend(revision("still no users", json!({})));
    turns.push(choice("plan_ready", "planned", json!({})));
    let agent = Arc::new(ScriptedAgentExecutor::new(turns));
    let engine = engine_with(&harness, &agent);
    let waiting = engine
        .start_and_run(&workflow("step_history"), json!({}), harness.workspace())
        .await
        .expect("run");
    assert_eq!(waiting.status, ExecutionStatus::InputNeeded);

    let execution = deliver(
        &engine,
        &harness,
        "step_history",
        &waiting.id,
        HumanInputDelivery::with_values(answer("The billing API.")),
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let calls = agent.calls();
    assert_eq!(calls[3].resume_session.as_deref(), Some("agent-session"));
    assert!(
        !calls[3].prompt.contains(HISTORY_HEADING),
        "{}",
        calls[3].prompt
    );
    let last = plan_prompts(&agent).pop().expect("a last plan prompt");
    assert!(last.contains("This is entry 3 into this step."), "{last}");
}

/// A reprompt for a missing required note is part of the same attempt: the
/// history has one line for it.
#[tokio::test]
async fn a_reprompt_adds_no_attempt() {
    let harness = Harness::new();
    let (agent, execution) = run_history(
        &harness,
        |_| {},
        vec![
            choice("noted", "kept a note", json!({})),
            choice("noted", "kept a note", json!({ "local.note": "n" })),
            AgentTurn::label("drafted", "drafted"),
            choice("plan_ready", "planned", json!({})),
        ],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(output_rejections(&harness, &execution.id).len(), 1);
    let second_entry = agent.prompts().pop().expect("the second entry");
    assert_eq!(
        second_entry.matches("- noted — kept a note").count(),
        1,
        "{second_entry}"
    );
    assert_eq!(
        step_attempts(&execution, "plan")["attempts"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );
}

/// KTD10: a required note kept from an earlier attempt satisfies the
/// choice, without a reprompt.
#[tokio::test]
async fn a_required_note_kept_from_an_earlier_attempt_satisfies_the_choice() {
    let harness = Harness::new();
    let mut turns = Vec::new();
    turns.extend(revision("no API named", json!({ "local.note": "kept" })));
    turns.push(choice("noted", "the note still holds", json!({})));
    turns.push(AgentTurn::label("drafted", "drafted"));
    turns.push(choice("plan_ready", "planned", json!({})));
    let (agent, execution) = run_history(&harness, |_| {}, turns).await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert!(output_rejections(&harness, &execution.id).is_empty());
    assert_eq!(agent.calls().len(), 5);
}

/// A step's note stays in the step's own record, not in the context.
#[tokio::test]
async fn a_local_note_is_not_a_context_variable() {
    let harness = Harness::new();
    let mut turns = Vec::new();
    turns.extend(revision("no API named", json!({ "local.memory": "A" })));
    turns.push(choice("plan_ready", "planned", json!({})));
    let (_agent, execution) = run_history(&harness, |_| {}, turns).await;

    assert!(
        !execution.context.variables.contains_key("local.memory"),
        "{:?}",
        execution.context.variables
    );
    assert_eq!(
        execution.metadata["engine"]["step_locals"]["plan"]["local.memory"],
        json!("A")
    );
}

/// A reason of 2000 characters with control characters is cut to 300 clean
/// characters, and the whole block stays within 4 KB however many
/// attempts there are.
#[tokio::test]
async fn the_history_is_cleaned_and_bounded() {
    let harness = Harness::new();
    let reason = format!("\u{1b}[31mred\u{7}\nline{}", "x".repeat(2000));
    let mut turns = Vec::new();
    for _ in 0..15 {
        turns.extend(revision(&reason, json!({})));
    }
    turns.push(choice("plan_ready", "planned", json!({})));
    let (agent, execution) = run_history(&harness, |_| {}, turns).await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let last = plan_prompts(&agent).pop().expect("a last plan prompt");
    let block = &last[last.find(HISTORY_HEADING).expect("a history block")..];
    assert!(block.len() <= 4 * 1024, "{} bytes", block.len());
    assert!(
        block.contains("This is entry 16 into this step."),
        "{block}"
    );
    let reasons: Vec<&str> = block
        .lines()
        .filter_map(|line| line.strip_prefix("- needs_requirements_revision — "))
        .collect();
    assert!(!reasons.is_empty(), "{block}");
    for shown in reasons {
        assert!(
            shown.chars().count() <= 300,
            "{} chars",
            shown.chars().count()
        );
        assert!(shown.starts_with("[31mredline"), "{shown}");
        assert!(!shown.chars().any(char::is_control), "{shown:?}");
    }
}
