//! A `decision` step decided by the operator, in the unified `schema: v2`
//! shape: the step's variables are the form, the choices are the menu, and
//! each choice has its own requirements.
//!
//! The agent is scripted in-process; what is asserted is what the engine
//! asks the operator, what it refuses and where the answer leads.

use crate::engine_support;

use std::sync::Arc;

use engine_support::{workflow_file, AgentTurn, Harness, ScriptedAgentExecutor};
use narsor_engine::domain::{
    ActionConfig, EventData, ExecutionStatus, WorkflowDefinition, WorkflowExecution,
};
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

// ---------------------------------------------------------------------------
// A gate on an observer queue (`watch`, U2)
// ---------------------------------------------------------------------------

/// `review_work` on the queue `mr-review`: the merge request comes from the
/// inputs, the label is fixed; `params` is the template of the params.
fn watched_review(params: &str) -> WorkflowDefinition {
    narsor_engine::workflow::load_from_str(&format!(
        r#"
schema: v2
name: watched-review
version: 1
inputs:
  - name: mr_url
    type: string
    required: true
states:
  review_work:
    action:
      type: decision
      by: human
      message: Review {{{{ inputs.mr_url }}}}.
      watch:
        queue: mr-review
        params: {params}
    variables:
      - name: comment
        type: text
        description: What to change.
      - name: severity
        type: select
        options: [low, high]
      - name: stage
        source: workflow
    choices:
      check_feedback:
        description: Read the review.
        next: done
      revise_plan:
        requires_variables: [comment]
        next: done
      abandon:
        requires_variables: [comment, severity]
        next: cancelled
  done:
    description: Finished.
  cancelled:
    description: Dropped.
terminal_states:
  done: completed
  cancelled: cancelled
"#
    ))
    .expect("the workflow loads")
}

const MR_URL: &str = "https://git.example.com/a/-/merge_requests/17";

/// Runs `definition` until its gate asks; the engine and the pending input.
async fn at_the_watched_gate(
    harness: &Harness,
    definition: &WorkflowDefinition,
) -> (WorkflowExecution, PendingHumanInput) {
    let execution = harness
        .engine()
        .start_and_run(definition, json!({ "mr_url": MR_URL }), harness.workspace())
        .await
        .expect("run");
    assert_eq!(execution.status, ExecutionStatus::InputNeeded);
    let pending = harness
        .store()
        .load_pending_input::<PendingHumanInput>(&execution.id)
        .expect("read the pending input")
        .expect("a pending input");
    (execution, pending)
}

#[tokio::test]
async fn entering_a_watched_gate_renders_its_params_and_keeps_them_with_the_request() {
    let harness = Harness::new();
    let definition = watched_review(r#"{ mr_url: "{{ inputs.mr_url }}", label: "w CR" }"#);

    let (_, pending) = at_the_watched_gate(&harness, &definition).await;

    let watch = pending.request.gate_watch.expect("the gate has a watch");
    assert_eq!(watch.step, "review_work");
    assert_eq!(watch.queue, "mr-review");
    assert_eq!(watch.params["mr_url"], MR_URL);
    assert_eq!(watch.params["label"], "w CR");
    let choices: Vec<(&str, &[String])> = watch
        .choices
        .iter()
        .map(|choice| (choice.name.as_str(), choice.requires_variables.as_slice()))
        .collect();
    assert_eq!(
        choices,
        [
            ("check_feedback", &[][..]),
            ("revise_plan", &["comment".to_string()][..]),
            (
                "abandon",
                &["comment".to_string(), "severity".to_string()][..]
            ),
        ]
    );
    assert_eq!(
        watch.choices[0].description.as_deref(),
        Some("Read the review.")
    );
    // The variables an answer may give: not the one the workflow sets.
    let variables: Vec<(&str, Option<&str>)> = watch
        .variables
        .iter()
        .map(|variable| (variable.name.as_str(), variable.variable_type.as_deref()))
        .collect();
    assert_eq!(
        variables,
        [("comment", Some("text")), ("severity", Some("select"))]
    );
    assert_eq!(watch.variables[1].options, ["low", "high"]);
    // R4: the gate's own form is unchanged.
    let labels: Vec<&str> = pending
        .outputs
        .iter()
        .map(|output| output.label.as_str())
        .collect();
    assert_eq!(labels, ["check_feedback", "revise_plan", "abandon"]);
}

#[tokio::test]
async fn a_gate_without_a_watch_has_none_on_its_request() {
    let run = Run::until_the_decision(vec![work_done()]).await;

    assert_eq!(run.pending().request.gate_watch, None);
    let stored = serde_json::to_value(run.pending()).unwrap();
    assert!(stored.get("gate_watch").is_none(), "{stored}");
}

#[tokio::test]
async fn a_param_that_fails_to_render_fails_the_step_like_a_message_that_does() {
    let failure = |definition: WorkflowDefinition| async move {
        let harness = Harness::new();
        let execution = harness
            .engine()
            .start_and_run(
                &definition,
                json!({ "mr_url": MR_URL }),
                harness.workspace(),
            )
            .await
            .expect("run");
        let error = harness
            .events(&execution.id)
            .into_iter()
            .find_map(|event| match event.data {
                EventData::ActionFailed { error, .. } => Some(error),
                _ => None,
            })
            .expect("the step failed");
        (execution.status, error)
    };
    let in_params = watched_review(r#"{ mr_url: "{{ context.nowhere }}" }"#);
    let mut in_message = watched_review("{}");
    if let Some(ActionConfig::Decision(decision)) = &mut in_message
        .states
        .get_mut("review_work")
        .expect("the gate")
        .action
    {
        decision.message = "Review {{ context.nowhere }}.".to_string().into();
    }

    let (params_status, params_error) = failure(in_params).await;
    let (message_status, message_error) = failure(in_message).await;

    assert_eq!(params_status, message_status);
    assert!(
        params_error.contains("unknown template variable 'context.nowhere'")
            && params_error.contains("watch.params.mr_url"),
        "{params_error}"
    );
    assert!(
        message_error.contains("unknown template variable 'context.nowhere'"),
        "{message_error}"
    );
}

#[tokio::test]
async fn a_watched_gate_is_still_answered_by_hand_with_every_choice() {
    for (label, values, end) in [
        ("check_feedback", vec![], ExecutionStatus::Completed),
        (
            "revise_plan",
            vec![("comment", json!("smaller"))],
            ExecutionStatus::Completed,
        ),
        (
            "abandon",
            vec![("comment", json!("no")), ("severity", json!("high"))],
            ExecutionStatus::Cancelled,
        ),
    ] {
        let harness = Harness::new();
        let definition = watched_review(r#"{ mr_url: "{{ inputs.mr_url }}" }"#);
        let (execution, pending) = at_the_watched_gate(&harness, &definition).await;

        let execution = harness
            .engine()
            .deliver_human_input(
                &definition,
                &execution.id,
                &pick(label, &values).for_episode(pending.episode_key().to_string()),
            )
            .await
            .unwrap_or_else(|error| panic!("{label}: {error}"));

        assert_eq!(execution.status, end, "{label}");
    }
}
