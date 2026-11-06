//! Integration tests for a `decision` step declared `by: conductor`.
//!
//! The agent and the conductor are scripted in-process: the agent by the
//! executors of `engine_support`, the conductor by an invoker behind the
//! real registry and actor. No harness runs. What is under test is the
//! engine's side: which answer settles the step, which one hands it to the
//! operator with the conductor's proposal, and what is recorded.

use crate::engine_support;

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use engine_support::{workflow_file, AgentTurn, Harness, ScriptedAgentExecutor};
use narsor_engine::conductor::assembly::assemble;
use narsor_engine::conductor::{
    ConductorAnswer, ConductorInvoker, ConductorRegistry, ConductorStore, Invocation,
    InvocationOutcome, SnapshotBudget, SnapshotInput,
};
use narsor_engine::domain::{
    ConductorDecisionKind, ConductorPurpose, ConductorTrigger, Confidence, EventData,
    ExecutionStatus, WorkflowDefinition, WorkflowExecution,
};
use narsor_engine::engine::{
    ActionKind, Engine, InputContinuation, OutputProblem, PendingHumanInput, METADATA_ENGINE_KEY,
    METADATA_STEP_LOCALS_KEY,
};
use serde_json::{json, Value};

const FIXTURE: &str = "v2/engine_decision_conductor.yaml";

/// One outcome of the scripted conductor.
enum Script {
    Answer(Value),
    Fail,
}

/// A conductor that answers from a list, and records every invocation.
struct ScriptedConductor {
    scripts: Mutex<VecDeque<Script>>,
    calls: Mutex<Vec<Invocation>>,
}

impl ScriptedConductor {
    fn new(scripts: Vec<Script>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into()),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<Invocation> {
        self.calls.lock().unwrap().clone()
    }
}

impl ConductorInvoker for ScriptedConductor {
    fn invoke<'a>(
        &'a self,
        invocation: &'a Invocation,
    ) -> Pin<Box<dyn Future<Output = InvocationOutcome> + Send + 'a>> {
        self.calls.lock().unwrap().push(invocation.clone());
        let script = self.scripts.lock().unwrap().pop_front();
        Box::pin(async move {
            match script.expect("the conductor was invoked more often than scripted") {
                Script::Answer(response) => InvocationOutcome::Answered(ConductorAnswer {
                    message: response
                        .get("message")
                        .and_then(Value::as_str)
                        .map(ToString::to_string),
                    response,
                }),
                Script::Fail => InvocationOutcome::Failed {
                    error: "the scripted conductor failed".to_string(),
                },
            }
        })
    }
}

struct Fixture {
    harness: Harness,
    engine: Arc<Engine>,
    registry: Option<Arc<ConductorRegistry>>,
    conductor: Arc<ScriptedConductor>,
    definition: WorkflowDefinition,
}

impl Fixture {
    /// An engine whose agent finishes its work and whose conductor answers
    /// with `scripts`.
    fn new(scripts: Vec<Script>) -> Self {
        Self::with_agent(vec![finished_work(); 4], scripts)
    }

    /// An engine whose agent plays `turns` and whose conductor answers with
    /// `scripts`.
    fn with_agent(turns: Vec<AgentTurn>, scripts: Vec<Script>) -> Self {
        let mut fixture = Self::without_conductor_playing(turns);
        fixture.conductor = ScriptedConductor::new(scripts);
        fixture.registry = Some(assemble(&fixture.engine, fixture.conductor.clone(), None));
        fixture
    }

    fn without_conductor() -> Self {
        Self::without_conductor_playing(vec![finished_work(); 4])
    }

    fn without_conductor_playing(turns: Vec<AgentTurn>) -> Self {
        let harness = Harness::new();
        let agent = Arc::new(ScriptedAgentExecutor::new(turns));
        let engine = Arc::new(harness.engine().with_executor(ActionKind::Agent, agent));
        Self {
            harness,
            engine,
            registry: None,
            conductor: ScriptedConductor::new(Vec::new()),
            definition: workflow_file(FIXTURE),
        }
    }

    async fn run(&self, context: Value) -> WorkflowExecution {
        let mut execution = self
            .engine
            .start(&self.definition, json!({}), self.harness.workspace())
            .expect("start the workflow");
        if let Value::Object(variables) = context {
            execution.context.variables.extend(variables);
            self.harness
                .store()
                .save_execution(&execution)
                .expect("save the execution");
        }
        self.engine
            .run(&self.definition, &execution.id)
            .await
            .expect("run the workflow")
    }

    fn events(&self, execution_id: &str) -> Vec<EventData> {
        self.harness
            .events(execution_id)
            .into_iter()
            .map(|event| event.data)
            .collect()
    }

    /// The problem the step waits on.
    fn problem(&self, execution: &WorkflowExecution) -> (PendingHumanInput, OutputProblem) {
        assert_eq!(execution.status, ExecutionStatus::InputNeeded);
        assert_eq!(execution.current_state.as_deref(), Some("pick"));
        let pending: PendingHumanInput = self
            .harness
            .store()
            .load_pending_input(&execution.id)
            .expect("read the pending input")
            .expect("a pending input");
        assert_eq!(pending.continuation, InputContinuation::ResolveOutput);
        let problem = pending.problem.clone().expect("a problem");
        (pending, problem)
    }
}

fn finished_work() -> AgentTurn {
    AgentTurn::Label {
        label: "exit",
        reason: "",
        variables: json!({}),
        artifacts: json!({}),
        reply: Some("all done"),
        session: true,
    }
}

fn choose(label: &str, confidence: &str, variables: Value) -> Script {
    Script::Answer(json!({
        "decision": "choose_output",
        "label": label,
        "variables": variables,
        "confidence": confidence,
        "rationale": "the work is done",
    }))
}

fn made_by_the_conductor(events: &[EventData]) -> Vec<String> {
    events
        .iter()
        .filter_map(|data| match data {
            EventData::DecisionMade {
                provider,
                output_label,
                ..
            } if provider == "conductor" => Some(output_label.clone()),
            _ => None,
        })
        .collect()
}

/// A sure choice of a declared label takes the transition, runs the label's
/// `set` and is recorded as the conductor's decision. The conductor is asked
/// although `autopilot` is off, and what it is asked is rendered.
#[tokio::test]
async fn a_sure_choice_takes_the_transition_and_runs_the_set_of_the_label() {
    let fixture = Fixture::new(vec![choose("approve", "high", json!({}))]);
    let execution = fixture.run(json!({ "autopilot": "off" })).await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(execution.current_state.as_deref(), Some("done"));
    assert_eq!(
        execution.context.variables.get("stage"),
        Some(&json!("closed"))
    );
    let events = fixture.events(&execution.id);
    assert_eq!(made_by_the_conductor(&events), ["approve"]);
    assert!(events.iter().any(|data| matches!(
        data,
        EventData::ConductorDecision {
            kind: ConductorDecisionKind::ChooseOutput,
            confidence: Confidence::High,
            ..
        }
    )));

    let calls = fixture.conductor.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].request.trigger, ConductorTrigger::DecisionStep);
    assert_eq!(calls[0].request.purpose, ConductorPurpose::DecideFork);
    let message = calls[0].request.payload["message"]
        .as_str()
        .expect("the rendered message");
    assert!(message.contains("all done"), "{message}");
    assert!(!message.contains("{{"), "{message}");
}

/// A label that needs a variable is taken when the conductor delivers it.
#[tokio::test]
async fn a_label_with_requirements_is_taken_when_they_are_met() {
    let fixture = Fixture::new(vec![choose(
        "reject",
        "high",
        json!({ "severity": "high" }),
    )]);
    let execution = fixture.run(json!({})).await;

    assert_eq!(execution.status, ExecutionStatus::Cancelled);
    assert_eq!(
        made_by_the_conductor(&fixture.events(&execution.id)),
        ["reject"]
    );
}

/// The conductor writes variables under the same declarations as an agent: a
/// name the step does not declare is kept as additional data, not written.
#[tokio::test]
async fn a_variable_the_step_does_not_declare_is_set_aside() {
    let fixture = Fixture::new(vec![choose("approve", "high", json!({ "invented": "x" }))]);
    let execution = fixture.run(json!({})).await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(execution.context.variables.get("invented"), None);
    assert_set_aside(&fixture.events(&execution.id), "invented");
}

/// A variable only the workflow sets is no more the conductor's to write
/// than an agent's.
#[tokio::test]
async fn a_variable_the_workflow_alone_sets_is_set_aside() {
    let fixture = Fixture::new(vec![choose(
        "reject",
        "high",
        json!({ "severity": "high", "stage": "open" }),
    )]);
    let execution = fixture.run(json!({})).await;

    assert_eq!(execution.status, ExecutionStatus::Cancelled);
    assert_eq!(execution.context.variables.get("stage"), None);
    assert_set_aside(&fixture.events(&execution.id), "stage");
}

/// A value outside the declared `options` is no choice the conductor can
/// settle: the operator decides, with the label still proposed.
#[tokio::test]
async fn a_value_outside_the_declared_options_waits_for_the_operator() {
    let fixture = Fixture::new(vec![choose(
        "reject",
        "high",
        json!({ "severity": "urgent" }),
    )]);
    let execution = fixture.run(json!({})).await;

    let (_, problem) = fixture.problem(&execution);
    let analysis = problem.analysis.expect("the conductor's proposal");
    assert_eq!(analysis.label.as_deref(), Some("reject"));
    assert!(made_by_the_conductor(&fixture.events(&execution.id)).is_empty());
    assert_eq!(execution.context.variables.get("severity"), None);
}

/// Asserts that the last step to complete kept `name` kept as additional data and a
/// warning for it.
fn assert_set_aside(events: &[EventData], name: &str) {
    let (additional_data, warnings) = events
        .iter()
        .rev()
        .find_map(|data| match data {
            EventData::ActionCompleted {
                additional_data,
                warnings,
                ..
            } => Some((additional_data, warnings)),
            _ => None,
        })
        .expect("the step completed");
    assert!(additional_data.contains_key(name), "{additional_data:?}");
    assert!(
        warnings.iter().any(|warning| warning.contains(name)),
        "{warnings:?}"
    );
}

/// An unsure conductor leaves the step to the operator, who gets what the
/// conductor would have chosen.
#[tokio::test]
async fn an_unsure_choice_waits_for_the_operator_with_the_proposal() {
    let fixture = Fixture::new(vec![choose("approve", "low", json!({}))]);
    let execution = fixture.run(json!({})).await;

    let (_, problem) = fixture.problem(&execution);
    let analysis = problem.analysis.expect("the conductor's proposal");
    assert_eq!(analysis.label.as_deref(), Some("approve"));
    assert!(analysis.rationale.is_some());
    let events = fixture.events(&execution.id);
    assert!(made_by_the_conductor(&events).is_empty());
    assert!(events.iter().any(|data| matches!(
        data,
        EventData::ConductorDecision {
            kind: ConductorDecisionKind::Escalate,
            ..
        }
    )));
    assert_eq!(execution.context.variables.get("stage"), None);
}

/// A label the state does not declare is no choice: the operator decides,
/// with no label proposed.
#[tokio::test]
async fn an_undeclared_label_waits_for_the_operator() {
    let fixture = Fixture::new(vec![choose("celebrate", "high", json!({}))]);
    let execution = fixture.run(json!({})).await;

    let (_, problem) = fixture.problem(&execution);
    assert_eq!(problem.analysis.expect("an analysis").label, None);
    assert!(made_by_the_conductor(&fixture.events(&execution.id)).is_empty());
}

/// A label whose required variable the conductor did not deliver is not
/// taken; the label stays proposed.
#[tokio::test]
async fn a_label_missing_its_required_variable_waits_for_the_operator() {
    let fixture = Fixture::new(vec![choose("reject", "high", json!({}))]);
    let execution = fixture.run(json!({})).await;

    let (_, problem) = fixture.problem(&execution);
    assert_eq!(
        problem.analysis.expect("an analysis").label.as_deref(),
        Some("reject")
    );
}

/// The conductor may hand the decision over by itself, proposing a label.
#[tokio::test]
async fn an_escalation_waits_for_the_operator_with_the_proposal() {
    let fixture = Fixture::new(vec![Script::Answer(json!({
        "decision": "escalate",
        "label": "revise",
        "confidence": "high",
        "rationale": "the notes are thin",
    }))]);
    let execution = fixture.run(json!({})).await;

    let (_, problem) = fixture.problem(&execution);
    let analysis = problem.analysis.expect("the proposal");
    assert_eq!(analysis.label.as_deref(), Some("revise"));
    assert!(analysis
        .rationale
        .expect("a rationale")
        .contains("the notes are thin"));
}

#[tokio::test]
async fn a_failing_conductor_leaves_the_step_to_the_operator() {
    let fixture = Fixture::new(vec![Script::Fail, Script::Fail]);
    let execution = fixture.run(json!({})).await;

    let (_, problem) = fixture.problem(&execution);
    assert_eq!(problem.analysis, None);
}

#[tokio::test]
async fn without_a_conductor_the_step_waits_for_the_operator() {
    let fixture = Fixture::without_conductor();
    let execution = fixture.run(json!({})).await;

    let (_, problem) = fixture.problem(&execution);
    assert_eq!(problem.analysis, None);
    assert!(fixture.conductor.calls().is_empty());
}

/// The snapshot carries the values the current step keeps for itself, and
/// no other step's.
#[tokio::test]
async fn the_snapshot_carries_the_local_values_of_the_current_step() {
    let fixture = Fixture::new(Vec::new());
    let mut execution = fixture
        .engine
        .start(&fixture.definition, json!({}), fixture.harness.workspace())
        .expect("start the workflow");
    execution.current_state = Some("pick".to_string());
    execution.metadata[METADATA_ENGINE_KEY] = json!({
        METADATA_STEP_LOCALS_KEY: {
            "pick": { "notes": "checked the tests" },
            "work": { "scratch": "not for the conductor" },
        },
    });

    let store = fixture.harness.store();
    let input = SnapshotInput::collect(
        &store,
        &ConductorStore::new(store.paths(), &execution.id),
        &execution,
        Err("not needed".to_string()),
        &SnapshotBudget::default(),
        false,
    );

    assert_eq!(input.step_locals, json!({ "notes": "checked the tests" }));
}

/// Undone, a decision of the conductor's is a problem again at its step with
/// the context from before it, like any other chosen output.
#[tokio::test]
async fn undoing_a_decision_of_the_conductor_restores_the_state_before_it() {
    let fixture = Fixture::new(vec![choose("approve", "high", json!({}))]);
    let execution = fixture.run(json!({})).await;
    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(
        execution.context.variables.get("stage"),
        Some(&json!("closed"))
    );
    let decision_id = fixture
        .events(&execution.id)
        .into_iter()
        .find_map(|data| match data {
            EventData::ConductorDecision {
                decision_id,
                kind: ConductorDecisionKind::ChooseOutput,
                ..
            } => Some(decision_id),
            _ => None,
        })
        .expect("the conductor decided");

    let undone = fixture
        .registry
        .as_ref()
        .expect("a conductor")
        .undo_decision(&fixture.definition, &execution.id, &decision_id, || {})
        .await
        .expect("undo the decision");
    assert_eq!(undone.state_id, "pick");

    let now = fixture
        .harness
        .store()
        .load_execution(&execution.id)
        .expect("load the execution");
    assert_eq!(now.status, ExecutionStatus::InputNeeded);
    assert_eq!(now.current_state.as_deref(), Some("pick"));
    assert_eq!(now.context.variables.get("stage"), None);
    let (pending, problem) = fixture.problem(&now);
    assert_eq!(pending.episode_key(), undone.episode);
    assert_eq!(
        problem
            .analysis
            .expect("the undone choice")
            .label
            .as_deref(),
        Some("approve")
    );
    assert!(fixture
        .events(&execution.id)
        .iter()
        .any(|data| matches!(data, EventData::ConductorDecisionUndone { .. })));
}

/// The conductor's answers to an agent's questions are counted per entry into
/// the step: the count is kept while the step is asked again, and starts over
/// when the workflow enters the step anew.
#[tokio::test]
async fn the_count_of_answers_starts_over_when_the_step_is_entered_again() {
    let fixture = Fixture::with_agent(
        vec![
            AgentTurn::Ask("Which API version?"),
            finished_work(),
            finished_work(),
        ],
        vec![
            Script::Answer(json!({
                "decision": "reply",
                "confidence": "high",
                "rationale": "the task names it",
                "message": "Use version 2.",
            })),
            choose("revise", "high", json!({})),
            choose("approve", "high", json!({})),
        ],
    );
    let execution = fixture.run(json!({ "autopilot": "on" })).await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let attempts = &execution.metadata[METADATA_ENGINE_KEY]["step_attempts"]["work"];
    assert_eq!(attempts["entries"], json!(2));
    assert_eq!(attempts["conductor_answers"], json!(0));
}

/// A loop only the conductor drives is bounded: after five decisions of the
/// step in a row, the next entry goes to the operator without asking it.
#[tokio::test]
async fn the_operator_decides_after_five_decisions_of_the_conductor_in_a_row() {
    let fixture = Fixture::with_agent(
        vec![finished_work(); 6],
        (0..5)
            .map(|_| choose("revise", "high", json!({})))
            .collect(),
    );
    let execution = fixture.run(json!({})).await;

    let (_, problem) = fixture.problem(&execution);
    assert_eq!(problem.analysis, None);
    assert_eq!(fixture.conductor.calls().len(), 5);
    assert_eq!(
        made_by_the_conductor(&fixture.events(&execution.id)).len(),
        5
    );
    let attempts = &execution.metadata[METADATA_ENGINE_KEY]["step_attempts"]["pick"];
    assert_eq!(attempts["conductor_decisions"], json!(5));
}
