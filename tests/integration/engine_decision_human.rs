//! A `decision` step decided by the operator, in the unified `schema: v2`
//! shape: the step's variables are the form, the choices are the menu, and
//! each choice has its own requirements.
//!
//! The agent is scripted in-process; what is asserted is what the engine
//! asks the operator, what it refuses and where the answer leads.

use crate::engine_support;

use std::sync::Arc;

use engine_support::{workflow_file, AgentTurn, Harness, ScriptedAgentExecutor};
use narsor_engine::domain::{EventData, ExecutionStatus, WorkflowDefinition, WorkflowExecution};
use narsor_engine::engine::actions::human::HumanActionError;
use narsor_engine::engine::{
    ActionKind, Engine, EngineError, HumanInputDelivery, InputContinuation, PendingHumanInput,
};
use serde_json::{json, Map, Value};

const FIXTURE: &str = "v2/engine_decision_human.yaml";

struct Run {
    harness: Harness,
    agent: Arc<ScriptedAgentExecutor>,
    definition: WorkflowDefinition,
    execution: WorkflowExecution,
}

impl Run {
    /// Runs the fixture until the decision asks the operator, with `turns`
    /// as the agent's answers.
    async fn until_the_decision(turns: Vec<AgentTurn>) -> Self {
        let harness = Harness::new();
        let agent = Arc::new(ScriptedAgentExecutor::new(turns));
        let definition = workflow_file(FIXTURE);
        let execution = engine_with(&harness, &agent)
            .start_and_run(&definition, json!({}), harness.workspace())
            .await
            .expect("run");
        assert_eq!(execution.status, ExecutionStatus::InputNeeded);
        Self {
            harness,
            agent,
            definition,
            execution,
        }
    }

    fn pending(&self) -> PendingHumanInput {
        self.harness
            .store()
            .load_pending_input::<PendingHumanInput>(&self.execution.id)
            .expect("read the pending input")
            .expect("a pending input")
    }

    /// Delivers `delivery`, naming the request it answers.
    async fn deliver(&mut self, delivery: HumanInputDelivery) -> Result<(), EngineError> {
        let episode = self.pending().episode_key().to_string();
        self.execution = engine_with(&self.harness, &self.agent)
            .deliver_human_input(
                &self.definition,
                &self.execution.id,
                &delivery.for_episode(episode),
            )
            .await?;
        Ok(())
    }

    fn events(&self) -> Vec<EventData> {
        self.harness
            .events(&self.execution.id)
            .into_iter()
            .map(|event| event.data)
            .collect()
    }
}

fn engine_with(harness: &Harness, agent: &Arc<ScriptedAgentExecutor>) -> Engine {
    harness
        .engine()
        .with_executor(ActionKind::Agent, agent.clone())
}

fn values(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_string(), value.clone()))
        .collect()
}

/// The turns that bring the fixture to its decision.
fn work_done() -> AgentTurn {
    AgentTurn::Label {
        label: "exit",
        reason: "",
        variables: json!({ "summary": "s" }),
        artifacts: json!({}),
        reply: Some("all done"),
        session: true,
    }
}

/// The delivery that chooses `label` from the menu with these field values.
fn pick(label: &str, pairs: &[(&str, Value)]) -> HumanInputDelivery {
    HumanInputDelivery::with_values(values(pairs)).choosing(label)
}

fn invalid_delivery(error: EngineError) -> HumanActionError {
    match error {
        EngineError::HumanInput(error) => {
            assert!(error.is_invalid_delivery(), "{error}");
            error
        }
        other => panic!("expected a refused delivery, got {other}"),
    }
}

#[tokio::test]
async fn the_form_is_the_variables_of_the_step_and_the_menu_is_its_choices() {
    let run = Run::until_the_decision(vec![work_done()]).await;
    let pending = run.pending();

    assert_eq!(pending.continuation, InputContinuation::CompleteStep);
    assert!(pending.label_from_menu);
    let fields: Vec<&str> = pending
        .fields()
        .iter()
        .map(|field| field.name.as_str())
        .collect();
    assert_eq!(fields, ["comment", "severity", "notes"]);
    let labels: Vec<&str> = pending
        .outputs
        .iter()
        .map(|output| output.label.as_str())
        .collect();
    assert_eq!(labels, ["approve", "revise", "reject"]);
    let reject = &pending.outputs[2];
    assert_eq!(reject.requires_variables, ["comment", "severity"]);
}

#[tokio::test]
async fn the_message_is_rendered_with_the_reply_of_the_step_before() {
    let run = Run::until_the_decision(vec![work_done()]).await;

    assert!(
        run.pending().instruction().contains("all done"),
        "{}",
        run.pending().instruction()
    );
}

#[tokio::test]
async fn a_choice_without_requirements_takes_an_empty_answer() {
    let mut run = Run::until_the_decision(vec![work_done()]).await;

    run.deliver(HumanInputDelivery::label("approve"))
        .await
        .expect("approve");

    assert_eq!(run.execution.status, ExecutionStatus::Completed);
    assert_eq!(
        run.execution.context.variables.get("stage"),
        Some(&json!("closed"))
    );
}

#[tokio::test]
async fn a_choice_missing_a_required_variable_is_refused_and_stays_answerable() {
    let mut run = Run::until_the_decision(vec![work_done()]).await;

    let error = run
        .deliver(pick("reject", &[("severity", json!("low"))]))
        .await
        .expect_err("the comment is required to reject");

    assert!(matches!(
        invalid_delivery(error),
        HumanActionError::MissingRequiredField { field } if field == "comment"
    ));
    assert_eq!(run.execution.status, ExecutionStatus::InputNeeded);

    run.deliver(pick(
        "reject",
        &[("severity", json!("low")), ("comment", json!("no"))],
    ))
    .await
    .expect("with both values the choice is accepted");
    assert_eq!(run.execution.status, ExecutionStatus::Cancelled);
}

#[tokio::test]
async fn a_blank_comment_does_not_meet_a_requirement() {
    let mut run = Run::until_the_decision(vec![work_done()]).await;

    let error = run
        .deliver(pick("revise", &[("comment", json!("  "))]))
        .await
        .expect_err("a blank comment is none");

    assert!(matches!(
        invalid_delivery(error),
        HumanActionError::MissingRequiredField { field } if field == "comment"
    ));
}

#[tokio::test]
async fn a_value_outside_the_options_of_a_variable_is_refused() {
    let mut run = Run::until_the_decision(vec![work_done()]).await;

    let error = run
        .deliver(pick("approve", &[("severity", json!("urgent"))]))
        .await
        .expect_err("urgent is not an option");

    assert!(matches!(
        invalid_delivery(error),
        HumanActionError::ValueNotPermitted { field, .. } if field == "severity"
    ));
}

#[tokio::test]
async fn a_single_select_variable_does_not_choose_the_label() {
    let mut run = Run::until_the_decision(vec![work_done()]).await;

    let error = run
        .deliver(HumanInputDelivery::with_values(values(&[(
            "severity",
            json!("low"),
        )])))
        .await
        .expect_err("the menu chooses the label");

    assert!(matches!(
        invalid_delivery(error),
        HumanActionError::NoLabelChosen { .. }
    ));
}

#[tokio::test]
async fn a_label_the_step_does_not_declare_is_refused() {
    let mut run = Run::until_the_decision(vec![work_done()]).await;

    let error = run
        .deliver(HumanInputDelivery::label("postpone"))
        .await
        .expect_err("no such choice");

    assert!(matches!(
        invalid_delivery(error),
        HumanActionError::LabelNotDeclared { .. }
    ));
}

#[tokio::test]
async fn the_comment_of_a_returning_choice_goes_to_the_session_of_the_target_step() {
    let mut run = Run::until_the_decision(vec![work_done(), work_done()]).await;
    assert!(run.pending().resume_targets.contains_key("revise"));

    run.deliver(pick("revise", &[("comment", json!("cover the edge case"))]))
        .await
        .expect("revise");

    let calls = run.agent.calls();
    assert_eq!(calls.len(), 2);
    assert!(calls[1].resume_session.is_some());
    assert!(calls[1].prompt.contains("cover the edge case"));
}

#[tokio::test]
async fn the_answer_of_the_operator_is_recorded_with_the_label() {
    let mut run = Run::until_the_decision(vec![work_done()]).await;

    run.deliver(pick(
        "reject",
        &[("severity", json!("high")), ("comment", json!("no"))],
    ))
    .await
    .expect("reject");

    assert_eq!(
        run.execution.context.variables.get("severity"),
        Some(&json!("high"))
    );
    assert!(run.events().iter().any(|event| matches!(
        event,
        EventData::HumanInputReceived { output_label: Some(label), .. } if label == "reject"
    )));
}

#[tokio::test]
async fn a_local_variable_of_the_decision_is_kept_and_shown_the_next_time() {
    let mut run = Run::until_the_decision(vec![work_done(), work_done()]).await;
    assert!(run.pending().local_values.is_empty());

    run.deliver(pick(
        "revise",
        &[
            ("comment", json!("once more")),
            ("notes", json!("first pass")),
        ],
    ))
    .await
    .expect("revise");

    assert_eq!(run.execution.status, ExecutionStatus::InputNeeded);
    assert_eq!(run.execution.context.variables.get("notes"), None);
    let shown: Vec<(String, String)> = run
        .pending()
        .local_values
        .iter()
        .map(|local| (local.name.clone(), local.value.clone()))
        .collect();
    assert_eq!(shown, [("notes".to_string(), "first pass".to_string())]);
    assert!(run.pending().instruction().contains("first pass"));
}

#[tokio::test]
async fn a_pending_record_written_before_decisions_had_forms_still_loads() {
    let run = Run::until_the_decision(vec![work_done()]).await;
    let mut stored = serde_json::to_value(run.pending()).unwrap();
    let record = stored.as_object_mut().unwrap();
    record.remove("label_from_menu");
    record.remove("local_values");

    let loaded: PendingHumanInput = serde_json::from_value(stored).expect("an old record loads");

    assert!(!loaded.label_from_menu);
    assert!(loaded.local_values.is_empty());
}
