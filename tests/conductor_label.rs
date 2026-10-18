//! Integration tests for the conductor's rescue of an output label (F1,
//! U10).
//!
//! The agent and the conductor are both scripted in-process: the agent by an
//! executor that answers without a label (or with one) from a list, the
//! conductor by an invoker behind the real registry and actor. No harness
//! runs — least of all the real `claude`. What is under test is the engine's
//! side: which path each answer takes, what is recorded, and what the
//! operator is left with.

mod engine_support;

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use engine_support::{workflow, AgentTurn, Harness, ScriptedAgentExecutor};
use narsor_engine::conductor::assembly::assemble;
use narsor_engine::conductor::{
    ChatEntryKind, ConductorAnswer, ConductorInvoker, ConductorRegistry, ConductorStore,
    Invocation, InvocationOutcome, SavedDecision,
};
use narsor_engine::domain::{
    ActionConfig, ActionError, ActionResult, AnsweredBy, ConductorDecisionKind, ConductorPurpose,
    ConductorTrigger, Confidence, EventData, ExecutionStatus, ProblemKind, RepromptedBy,
    WorkflowExecution,
};
use narsor_engine::engine::actions::agent::MISSING_OUTPUT_LABEL;
use narsor_engine::engine::{
    owed_output_rescue, ActionExecutor, ActionKind, ActionOutcome, ActionRequest, Engine,
    HumanInputDelivery, InputContinuation, OutputProblem, PendingHumanInput,
};
use serde_json::{json, Map, Value};
use tokio::sync::Notify;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// One answer of the scripted agent.
#[derive(Clone)]
enum Reply {
    /// No output label; `session` says whether the harness reported one.
    NoLabel { tail: &'static str, session: bool },
    /// This label, with `data`.
    Label(&'static str),
}

/// What the scripted agent was handed on one call.
#[derive(Debug, Clone)]
struct AgentCall {
    prompt: String,
    resume_session: Option<String>,
    action_execution_id: String,
}

/// An agent that answers from a list, and records what it was asked.
struct ScriptedAgent {
    replies: Mutex<VecDeque<Reply>>,
    calls: Mutex<Vec<AgentCall>>,
}

impl ScriptedAgent {
    fn new(replies: Vec<Reply>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<AgentCall> {
        self.calls.lock().unwrap().clone()
    }
}

impl ActionExecutor for ScriptedAgent {
    fn execute<'a>(
        &'a self,
        request: ActionRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ActionOutcome, ActionError>> + Send + 'a>> {
        let ActionConfig::Agent(config) = request.action else {
            panic!("the scripted agent only runs agent actions");
        };
        self.calls.lock().unwrap().push(AgentCall {
            prompt: config.prompt.clone(),
            resume_session: config
                .parameters
                .get("resume_session")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            action_execution_id: request.action_execution_id.to_string(),
        });
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("the agent was called more often than scripted");
        Box::pin(async move {
            match reply {
                Reply::NoLabel { tail, session } => {
                    let mut details = json!({ "answer_tail": tail });
                    if session {
                        details["harness"] = json!("claude-code");
                        details["harness_session_id"] = json!("agent-session");
                    }
                    Err(ActionError {
                        message: "no output label could be extracted from the agent's answer"
                            .to_string(),
                        error_type: Some(MISSING_OUTPUT_LABEL.to_string()),
                        details: Some(details),
                    })
                }
                Reply::Label(label) => Ok(ActionOutcome::Completed(ActionResult {
                    output_label: label.to_string(),
                    data: json!({}),
                    variables: Map::from_iter([("summary".to_string(), json!("by the agent"))]),
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    reply: None,
                })),
            }
        })
    }
}

/// One outcome of the scripted conductor.
enum Script {
    /// It answers with this response.
    Answer(Value),
    /// It fails.
    Fail,
    /// It never answers by itself.
    Hang,
    /// It fails once `gate` is opened.
    FailAfter(Arc<Notify>),
}

/// A conductor that answers from a list, and records every invocation.
#[derive(Default)]
struct ScriptedConductor {
    scripts: Mutex<VecDeque<Script>>,
    calls: Mutex<Vec<Invocation>>,
    started: Notify,
    dropped: Arc<AtomicBool>,
}

impl ScriptedConductor {
    fn new(scripts: Vec<Script>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into()),
            ..Self::default()
        }
    }

    fn calls(&self) -> Vec<Invocation> {
        self.calls.lock().unwrap().clone()
    }

    fn purposes(&self) -> Vec<(ConductorPurpose, u32)> {
        self.calls()
            .iter()
            .map(|call| (call.request.purpose, call.attempt))
            .collect()
    }
}

/// Sets its flag when the invocation holding it is dropped.
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
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
                Script::Hang => {
                    let _flag = DropFlag(Arc::clone(&self.dropped));
                    self.started.notify_one();
                    std::future::pending::<InvocationOutcome>().await
                }
                Script::FailAfter(gate) => {
                    gate.notified().await;
                    InvocationOutcome::Failed {
                        error: "the scripted conductor failed".to_string(),
                    }
                }
            }
        })
    }
}

/// An engine with the scripted agent, and the conductor attached.
struct Fixture {
    harness: Harness,
    engine: Arc<Engine>,
    registry: Arc<ConductorRegistry>,
    agent: Arc<ScriptedAgent>,
    conductor: Arc<ScriptedConductor>,
}

impl Fixture {
    fn new(replies: Vec<Reply>, scripts: Vec<Script>) -> Self {
        let harness = Harness::new();
        let agent = Arc::new(ScriptedAgent::new(replies));
        let conductor = Arc::new(ScriptedConductor::new(scripts));
        let engine = Arc::new(
            harness
                .engine()
                .with_executor(ActionKind::Agent, agent.clone()),
        );
        let registry = assemble(&engine, conductor.clone(), None);
        Self {
            harness,
            engine,
            registry,
            agent,
            conductor,
        }
    }

    async fn run(&self) -> WorkflowExecution {
        self.engine
            .start_and_run(
                &workflow("agent_problem"),
                json!({}),
                self.harness.workspace(),
            )
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

    fn conductor_store(&self, execution_id: &str) -> ConductorStore {
        ConductorStore::new(self.harness.store().paths(), execution_id)
    }

    fn problem(&self, execution: &WorkflowExecution) -> (PendingHumanInput, OutputProblem) {
        assert_eq!(execution.status, ExecutionStatus::InputNeeded);
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

/// The agent's first answer and its answer to the engine's reprompt, both
/// without a label, in a session the conductor could resume.
fn two_answers_without_label() -> Vec<Reply> {
    vec![
        Reply::NoLabel {
            tail: r#"Done. {"data": {"units_completed": 3}}"#,
            session: true,
        },
        Reply::NoLabel {
            tail: "gotowe, wszystko działa",
            session: true,
        },
    ]
}

fn choose(label: &str, confidence: &str, data: Value, variables: Value) -> Script {
    Script::Answer(json!({
        "decision": "choose_output",
        "label": label,
        "data": data,
        "variables": variables,
        "confidence": confidence,
        "rationale": "the agent reported three units done and working",
    }))
}

fn decisions(events: &[EventData]) -> Vec<(ConductorDecisionKind, Option<String>, Confidence)> {
    events
        .iter()
        .filter_map(|data| match data {
            EventData::ConductorDecision {
                kind,
                output_label,
                confidence,
                ..
            } => Some((*kind, output_label.clone(), *confidence)),
            _ => None,
        })
        .collect()
}

fn count<F: Fn(&EventData) -> bool>(events: &[EventData], matches: F) -> usize {
    events.iter().filter(|data| matches(data)).count()
}

// ---------------------------------------------------------------------------
// (a): the conductor chooses the output
// ---------------------------------------------------------------------------

/// AE3: an agent that says it is done but gives no label, even when asked
/// again, is completed by the conductor's sure choice — with its values,
/// on record with its rationale, and without the operator.
#[tokio::test]
async fn a_sure_choice_completes_the_step_on_the_conductors_word() {
    let fixture = Fixture::new(
        two_answers_without_label(),
        vec![choose(
            "implemented",
            "high",
            json!({ "units_completed": 3 }),
            json!({ "summary": "three units implemented" }),
        )],
    );
    let execution = fixture.run().await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(fixture.agent.calls().len(), 2, "one attempt, one reprompt");
    assert_eq!(
        fixture.conductor.purposes(),
        vec![(ConductorPurpose::ChooseOutput, 1)]
    );
    let call = &fixture.conductor.calls()[0];
    assert_eq!(call.request.trigger, ConductorTrigger::OutputRescue);
    let second_attempt = fixture.agent.calls()[1].action_execution_id.clone();
    assert_eq!(
        call.request.episode.as_deref(),
        Some(second_attempt.as_str())
    );
    assert_eq!(call.request.payload["state_id"], "work");
    assert_eq!(call.request.payload["answers"].as_array().unwrap().len(), 2);

    let events = fixture.events(&execution.id);
    assert_eq!(
        decisions(&events),
        vec![(
            ConductorDecisionKind::ChooseOutput,
            Some("implemented".to_string()),
            Confidence::High
        )]
    );
    let rationale = events.iter().find_map(|data| match data {
        EventData::ConductorDecision { rationale, .. } => rationale.clone(),
        _ => None,
    });
    assert_eq!(
        rationale.as_deref(),
        Some("the agent reported three units done and working")
    );
    let decided = events
        .iter()
        .find_map(|data| match data {
            EventData::DecisionMade {
                provider,
                output_label,
                reasoning,
            } => Some((provider.clone(), output_label.clone(), reasoning.clone())),
            _ => None,
        })
        .expect("a DecisionMade");
    assert_eq!(decided.0, "conductor");
    assert_eq!(decided.1, "implemented");
    assert!(decided.2.is_some());
    assert!(events.iter().any(|data| matches!(
        data,
        EventData::ActionCompleted { action_execution_id, output_label }
            if *action_execution_id == second_attempt && output_label == "implemented"
    )));
    assert!(events.iter().any(|data| matches!(
        data,
        EventData::TransitionTaken { from_state, to_state, .. }
            if from_state == "work" && to_state == "report"
    )));
    assert_eq!(
        count(&events, |data| matches!(
            data,
            EventData::ProblemRaised { .. }
        )),
        0
    );

    // The conductor's values reached the context.
    assert_eq!(
        execution.context.variables.get("summary"),
        Some(&json!("three units implemented"))
    );

    // The decision is on record for an undo, and no longer waits to apply.
    let store = fixture.conductor_store(&execution.id);
    let state = store.load_state().expect("load the conductor's state");
    assert!(state.unapplied_decisions.is_empty(), "{state:?}");
    assert_eq!(state.pending, None);
    assert!(state.queue.is_empty());
    let entry = store
        .read_chat()
        .expect("read the chat")
        .into_iter()
        .find(|entry| entry.kind == ChatEntryKind::Decision)
        .expect("a decision in the chat");
    let saved: SavedDecision =
        serde_json::from_value(entry.payload.expect("a payload")).expect("a saved decision");
    assert_eq!(saved.kind, ConductorDecisionKind::ChooseOutput);
    assert_eq!(saved.output_label.as_deref(), Some("implemented"));
    assert_eq!(saved.state_id.as_deref(), Some("work"));
    assert!(saved.context_before.get("variables").is_some());
    assert_eq!(
        saved.context_before["variables"].get("summary"),
        None,
        "the context from before the decision"
    );
    assert!(owed_output_rescue(&execution).expect("read").is_none());
}

/// A choice the conductor is not sure of is not taken: the step becomes a
/// problem, with the choice as the analysis.
#[tokio::test]
async fn a_choice_with_low_confidence_is_an_escalation() {
    let fixture = Fixture::new(
        two_answers_without_label(),
        vec![choose(
            "implemented",
            "low",
            json!({}),
            json!({ "summary": "maybe done" }),
        )],
    );
    let execution = fixture.run().await;

    let (_, problem) = fixture.problem(&execution);
    let analysis = problem.analysis.expect("an analysis");
    assert_eq!(analysis.label.as_deref(), Some("implemented"));
    assert_eq!(
        analysis.variables.get("summary"),
        Some(&json!("maybe done"))
    );
    let rationale = analysis.rationale.expect("a rationale");
    assert!(rationale.contains("low confidence"), "{rationale}");
    assert!(
        problem.reason.contains("low confidence"),
        "{}",
        problem.reason
    );
    assert_eq!(
        decisions(&fixture.events(&execution.id)),
        vec![(
            ConductorDecisionKind::Escalate,
            Some("implemented".to_string()),
            Confidence::Low
        )]
    );
}

/// A label the state does not declare is not taken either.
#[tokio::test]
async fn a_choice_of_an_undeclared_label_is_an_escalation() {
    let fixture = Fixture::new(
        two_answers_without_label(),
        vec![choose("shipped", "high", json!({}), json!({}))],
    );
    let execution = fixture.run().await;

    let (_, problem) = fixture.problem(&execution);
    let analysis = problem.analysis.expect("an analysis");
    assert_eq!(analysis.label, None, "an undeclared label is not prefilled");
    assert!(
        analysis
            .rationale
            .as_deref()
            .is_some_and(|rationale| rationale.contains("'shipped'")),
        "{analysis:?}"
    );
}

/// AE6: the conductor chooses `needs_human_decision` without the question
/// the gate it leads to shows (KTD15): an escalation, saying what is
/// missing.
#[tokio::test]
async fn a_choice_without_a_value_the_next_step_needs_is_an_escalation() {
    let fixture = Fixture::new(
        two_answers_without_label(),
        vec![choose("needs_human_decision", "high", json!({}), json!({}))],
    );
    let execution = fixture.run().await;

    let (_, problem) = fixture.problem(&execution);
    let analysis = problem.analysis.expect("an analysis");
    assert_eq!(analysis.label.as_deref(), Some("needs_human_decision"));
    assert!(
        analysis
            .rationale
            .as_deref()
            .is_some_and(|rationale| rationale.contains("data.question")),
        "{analysis:?}"
    );
    let events = fixture.events(&execution.id);
    assert_eq!(
        count(&events, |data| matches!(
            data,
            EventData::DecisionMade { .. }
        )),
        0,
        "the choice was not applied"
    );
}

// ---------------------------------------------------------------------------
// (c): the conductor escalates
// ---------------------------------------------------------------------------

/// AE4 (the engine's part): an escalation is a problem carrying both of the
/// agent's answers, the conductor's analysis, and every label with its
/// description.
#[tokio::test]
async fn an_escalation_is_a_problem_with_both_answers_and_the_analysis() {
    let fixture = Fixture::new(
        two_answers_without_label(),
        vec![Script::Answer(json!({
            "decision": "escalate",
            "label": "implemented",
            "variables": { "summary": "probably three units" },
            "confidence": "low",
            "rationale": "the answers do not say whether the tests pass",
            "message": "I could not tell whether the work is done.",
        }))],
    );
    let execution = fixture.run().await;

    let (pending, problem) = fixture.problem(&execution);
    assert_eq!(problem.answers.len(), 2);
    assert!(problem.answers[0]
        .answer_tail
        .as_deref()
        .is_some_and(|tail| tail.contains("units_completed")));
    assert_eq!(
        problem.answers[1].answer_tail.as_deref(),
        Some("gotowe, wszystko działa")
    );
    let analysis = problem.analysis.expect("an analysis");
    assert_eq!(analysis.label.as_deref(), Some("implemented"));
    assert_eq!(
        analysis.variables.get("summary"),
        Some(&json!("probably three units"))
    );
    assert_eq!(
        analysis.rationale.as_deref(),
        Some("the answers do not say whether the tests pass")
    );
    let outputs: Vec<(String, Option<String>)> = pending
        .outputs
        .iter()
        .map(|output| (output.label.clone(), output.description.clone()))
        .collect();
    assert_eq!(
        outputs,
        vec![
            (
                "implemented".to_string(),
                Some("The feature is implemented.".to_string())
            ),
            (
                "needs_human_decision".to_string(),
                Some("A person has to decide first.".to_string())
            ),
        ]
    );

    let events = fixture.events(&execution.id);
    assert_eq!(
        decisions(&events),
        vec![(
            ConductorDecisionKind::Escalate,
            Some("implemented".to_string()),
            Confidence::Low
        )]
    );
    assert!(events.iter().any(|data| matches!(
        data,
        EventData::ProblemRaised {
            kind: ProblemKind::Output,
            ..
        }
    )));
    let chat = fixture
        .conductor_store(&execution.id)
        .read_chat()
        .expect("read the chat");
    assert!(chat
        .iter()
        .any(|entry| entry.kind == ChatEntryKind::Decision
            && entry.text == "I could not tell whether the work is done."));
    assert!(owed_output_rescue(&execution).expect("read").is_none());
}

// ---------------------------------------------------------------------------
// (b): the conductor asks the agent again
// ---------------------------------------------------------------------------

/// AE5: the conductor asks once more in its own words; the agent still
/// gives no label, and the conductor, asked again, can no longer choose
/// (b). The agent was asked again twice in all.
#[tokio::test]
async fn after_its_own_reprompt_the_conductor_can_only_choose_or_escalate() {
    let mut replies = two_answers_without_label();
    replies.push(Reply::NoLabel {
        tail: "still done",
        session: true,
    });
    let fixture = Fixture::new(
        replies,
        vec![
            Script::Answer(json!({
                "decision": "reprompt",
                "prompt": "Did the tests pass? Answer with the output object.",
                "confidence": "low",
                "rationale": "the tests are not mentioned",
            })),
            choose(
                "implemented",
                "high",
                json!({}),
                json!({ "summary": "done, as the agent repeated" }),
            ),
        ],
    );
    let execution = fixture.run().await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let calls = fixture.agent.calls();
    assert_eq!(calls.len(), 3, "one attempt and two reprompts");
    assert!(calls[2]
        .prompt
        .starts_with("Did the tests pass? Answer with the output object."));
    assert_eq!(calls[2].resume_session.as_deref(), Some("agent-session"));
    assert_eq!(
        fixture.conductor.purposes(),
        vec![
            (ConductorPurpose::ChooseOutput, 1),
            (ConductorPurpose::ChooseOutputAfterReprompt, 1),
        ]
    );

    let events = fixture.events(&execution.id);
    let reprompts: Vec<(RepromptedBy, String)> = events
        .iter()
        .filter_map(|data| match data {
            EventData::OutputReprompted {
                by,
                previous_attempt,
                ..
            } => Some((*by, previous_attempt.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        reprompts,
        vec![
            (RepromptedBy::Engine, calls[0].action_execution_id.clone()),
            (
                RepromptedBy::Conductor,
                calls[1].action_execution_id.clone()
            ),
        ]
    );
    assert_eq!(
        count(&events, |data| matches!(
            data,
            EventData::ActionOutputRejected { .. }
        )),
        3
    );
    let kinds: Vec<ConductorDecisionKind> = decisions(&events)
        .into_iter()
        .map(|(kind, ..)| kind)
        .collect();
    assert_eq!(
        kinds,
        vec![
            ConductorDecisionKind::Reprompt,
            ConductorDecisionKind::ChooseOutput
        ]
    );
    let completed = events.iter().find_map(|data| match data {
        EventData::ActionCompleted {
            action_execution_id,
            ..
        } => Some(action_execution_id.clone()),
        _ => None,
    });
    assert_eq!(
        completed.as_deref(),
        Some(calls[2].action_execution_id.as_str()),
        "the step completes under its last attempt"
    );
}

/// The conductor's reprompt that gets the label completes the step as the
/// agent would have: the conductor is not asked again.
#[tokio::test]
async fn a_conductor_reprompt_that_gets_the_label_completes_the_step() {
    let mut replies = two_answers_without_label();
    replies.push(Reply::Label("implemented"));
    let fixture = Fixture::new(
        replies,
        vec![Script::Answer(json!({
            "decision": "reprompt",
            "prompt": "Which label describes the work?",
        }))],
    );
    let execution = fixture.run().await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(fixture.conductor.calls().len(), 1);
    let events = fixture.events(&execution.id);
    let provider = events.iter().find_map(|data| match data {
        EventData::DecisionMade { provider, .. } => Some(provider.clone()),
        _ => None,
    });
    assert_eq!(provider.as_deref(), Some("rules"), "the agent's own label");
    assert!(owed_output_rescue(&execution).expect("read").is_none());
}

/// Without a session to resume there is no reprompt at all: the conductor
/// is asked straight away, and may not choose (b).
#[tokio::test]
async fn without_a_session_the_conductor_is_asked_without_reprompt() {
    let fixture = Fixture::new(
        vec![Reply::NoLabel {
            tail: "done",
            session: false,
        }],
        vec![Script::Answer(json!({
            "decision": "escalate",
            "rationale": "nothing to go on",
        }))],
    );
    let execution = fixture.run().await;

    assert_eq!(fixture.agent.calls().len(), 1);
    assert_eq!(
        fixture.conductor.purposes(),
        vec![(ConductorPurpose::ChooseOutputAfterReprompt, 1)]
    );
    let (_, problem) = fixture.problem(&execution);
    assert_eq!(problem.answers.len(), 1);
    assert!(
        problem.reason.contains("no session to resume"),
        "{}",
        problem.reason
    );
}

// ---------------------------------------------------------------------------
// The conductor fails
// ---------------------------------------------------------------------------

/// AE10: two failed invocations (the first and R24's one repeat) make the
/// step a problem with the agent's raw answers and no analysis — and the
/// conductor is not invoked about its own failure.
#[tokio::test]
async fn a_conductor_that_fails_twice_leaves_a_problem_with_the_raw_answers() {
    let fixture = Fixture::new(
        two_answers_without_label(),
        vec![Script::Fail, Script::Fail],
    );
    let execution = fixture.run().await;

    assert_eq!(
        fixture.conductor.purposes(),
        vec![
            (ConductorPurpose::ChooseOutput, 1),
            (ConductorPurpose::ChooseOutput, 2)
        ]
    );
    let (_, problem) = fixture.problem(&execution);
    assert_eq!(problem.analysis, None);
    assert_eq!(problem.answers.len(), 2);
    assert!(
        problem.reason.contains("the scripted conductor failed"),
        "{}",
        problem.reason
    );
    let events = fixture.events(&execution.id);
    assert!(decisions(&events).is_empty());
    assert_eq!(
        count(&events, |data| matches!(
            data,
            EventData::WorkflowFailed { .. }
        )),
        0,
        "a malformed answer never fails the workflow"
    );

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fixture.conductor.calls().len(), 2, "no further invocation");
}

/// An answer the purpose does not allow counts as a failure, not a decision.
#[tokio::test]
async fn an_answer_off_the_purpose_is_a_failure() {
    let fixture = Fixture::new(
        two_answers_without_label(),
        vec![Script::Answer(json!({
            "decision": "diagnose",
            "diagnosis": "wrong purpose",
        }))],
    );
    let execution = fixture.run().await;

    let (_, problem) = fixture.problem(&execution);
    assert_eq!(problem.analysis, None);
    assert_eq!(fixture.conductor.calls().len(), 1);
}

// ---------------------------------------------------------------------------
// Interrupts and restarts
// ---------------------------------------------------------------------------

/// Interrupting the pass during the inline invocation stops the conductor;
/// the execution stays where it was, and does not go on by itself. A new
/// pass carries the rescue on, as R24's one repeat.
#[tokio::test]
async fn an_interrupt_during_the_inline_invocation_stops_the_conductor() {
    let fixture = Fixture::new(
        two_answers_without_label(),
        vec![
            Script::Hang,
            choose(
                "implemented",
                "high",
                json!({}),
                json!({ "summary": "done" }),
            ),
        ],
    );
    let execution = fixture
        .engine
        .start(
            &workflow("agent_problem"),
            json!({}),
            fixture.harness.workspace(),
        )
        .expect("start");
    let definition = workflow("agent_problem");

    let engine = Arc::clone(&fixture.engine);
    let id = execution.id.clone();
    let pass = tokio::spawn(async move {
        let definition = workflow("agent_problem");
        let _ = engine.run(&definition, &id).await;
    });
    tokio::time::timeout(Duration::from_secs(5), fixture.conductor.started.notified())
        .await
        .expect("the conductor was invoked");
    pass.abort();
    let _ = pass.await;

    let store = fixture.conductor_store(&execution.id);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !fixture.conductor.dropped.load(Ordering::SeqCst) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the invocation was not stopped"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    let stored = fixture
        .harness
        .store()
        .load_execution(&execution.id)
        .expect("load");
    assert_eq!(stored.status, ExecutionStatus::Running);
    assert_eq!(stored.current_state.as_deref(), Some("work"));
    assert!(owed_output_rescue(&stored).expect("read").is_some());
    let state = store.load_state().expect("load the conductor's state");
    assert!(
        state.pending.is_some(),
        "the marker is kept for the startup hook: {state:?}"
    );
    assert!(!fixture.engine.is_active(&execution.id));
    assert_eq!(fixture.conductor.calls().len(), 1);

    // Carried on by a new pass: the conductor is invoked again, as the
    // second try.
    let resumed = fixture
        .engine
        .run(&definition, &execution.id)
        .await
        .expect("carry the rescue on");
    assert_eq!(resumed.status, ExecutionStatus::Completed);
    assert_eq!(
        fixture.conductor.purposes(),
        vec![
            (ConductorPurpose::ChooseOutput, 1),
            (ConductorPurpose::ChooseOutput, 2)
        ]
    );
    assert_eq!(
        fixture.agent.calls().len(),
        2,
        "the agent was not run again"
    );
    let state = store.load_state().expect("load the conductor's state");
    assert_eq!(state.pending, None);
    assert!(state.queue.is_empty(), "{state:?}");
}

/// Waits until `condition` holds, for at most five seconds.
async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting until {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Starts `agent_problem` and runs a pass of it in the background until the
/// conductor's inline invocation is under way (`started`), then interrupts
/// the pass. Returns the execution.
async fn interrupted_during_the_rescue(fixture: &Fixture) -> WorkflowExecution {
    let execution = fixture
        .engine
        .start(
            &workflow("agent_problem"),
            json!({}),
            fixture.harness.workspace(),
        )
        .expect("start");
    let engine = Arc::clone(&fixture.engine);
    let id = execution.id.clone();
    let pass = tokio::spawn(async move {
        let _ = engine.run(&workflow("agent_problem"), &id).await;
    });
    tokio::time::timeout(Duration::from_secs(5), fixture.conductor.started.notified())
        .await
        .expect("the conductor was invoked");
    pass.abort();
    let _ = pass.await;
    wait_until("the invocation was stopped", || {
        fixture.conductor.dropped.load(Ordering::SeqCst)
    })
    .await;
    wait_until("the engine let go of the execution", || {
        !fixture.engine.is_active(&execution.id)
    })
    .await;
    execution
}

/// A cancelled execution owes no rescue: what an interrupted rescue left
/// in the conductor's store goes with the cancellation, so no phantom
/// conductor activity is shown for it.
#[tokio::test]
async fn cancelling_after_an_interrupted_rescue_drops_its_marker() {
    let fixture = Fixture::new(two_answers_without_label(), vec![Script::Hang]);
    let execution = interrupted_during_the_rescue(&fixture).await;
    let store = fixture.conductor_store(&execution.id);
    wait_until("the interrupted invocation finished", || {
        store
            .load_state()
            .is_ok_and(|state| state.pending.is_some() && state.queue.is_empty())
    })
    .await;

    fixture
        .engine
        .cancel_execution(&execution.id, "not needed", AnsweredBy::Operator, None)
        .expect("cancel");

    let state = store.load_state().expect("load the conductor's state");
    assert_eq!(state.pending, None, "{state:?}");
    assert!(state.queue.is_empty(), "{state:?}");
    let actor = fixture.registry.actor(&execution.id).expect("the actor");
    assert_eq!(actor.state().pending, None);
}

/// A manual transition after an interrupted rescue moves the step on: the
/// rescue's marker goes with it.
#[tokio::test]
async fn a_manual_transition_after_an_interrupted_rescue_drops_its_marker() {
    let fixture = Fixture::new(two_answers_without_label(), vec![Script::Hang]);
    let execution = interrupted_during_the_rescue(&fixture).await;
    let store = fixture.conductor_store(&execution.id);
    wait_until("the interrupted invocation finished", || {
        store
            .load_state()
            .is_ok_and(|state| state.pending.is_some() && state.queue.is_empty())
    })
    .await;

    fixture
        .engine
        .take_manual_transition(
            &workflow("agent_problem"),
            &execution.id,
            &narsor_engine::engine::ManualTransition {
                label: Some("implemented".to_string()),
                target: None,
                force: false,
                provider: "human".to_string(),
                reason: "done by hand".to_string(),
                by: AnsweredBy::Operator,
                episode: None,
            },
        )
        .expect("transition");

    let state = store.load_state().expect("load the conductor's state");
    assert_eq!(state.pending, None, "{state:?}");
}

/// A rescue request whose pass was interrupted while it waited behind
/// another invocation is never served: no attempt is counted for its
/// episode, so the pass that carries the rescue on still has R24's repeat.
#[tokio::test]
async fn a_rescue_abandoned_in_the_queue_does_not_spend_its_attempts() {
    let chat_gate = Arc::new(Notify::new());
    let fixture = Fixture::new(
        two_answers_without_label(),
        vec![
            Script::FailAfter(Arc::clone(&chat_gate)),
            Script::Fail,
            choose(
                "implemented",
                "high",
                json!({}),
                json!({ "summary": "done" }),
            ),
        ],
    );
    let definition = workflow("agent_problem");
    let execution = fixture
        .engine
        .start(&definition, json!({}), fixture.harness.workspace())
        .expect("start");
    // The operator's chat runs first, and the rescue queues behind it.
    fixture
        .registry
        .post_chat(&execution.id, "what is going on?")
        .expect("post");
    wait_until("the chat is answered", || {
        fixture.conductor.calls().len() == 1
    })
    .await;
    let actor = fixture.registry.actor(&execution.id).expect("the actor");
    let engine = Arc::clone(&fixture.engine);
    let id = execution.id.clone();
    let pass = tokio::spawn(async move {
        let _ = engine.run(&workflow("agent_problem"), &id).await;
    });
    wait_until("the rescue is queued", || actor.queued() == 1).await;

    // Interrupted while queued, then the chat finishes.
    pass.abort();
    let _ = pass.await;
    chat_gate.notify_one();
    wait_until("the queue is drained", || {
        let state = actor.state();
        actor.queued() == 0 && state.pending.is_none() && state.queue.is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        fixture.conductor.calls().len(),
        1,
        "the rescue was not served"
    );
    let state = actor.state();
    assert!(state.invocation_attempts.is_empty(), "{state:?}");

    // The pass that carries the rescue on gets both tries.
    let resumed = fixture
        .engine
        .run(&definition, &execution.id)
        .await
        .expect("carry the rescue on");
    assert_eq!(resumed.status, ExecutionStatus::Completed);
    assert_eq!(
        fixture.conductor.purposes(),
        vec![
            (ConductorPurpose::ReplyToChat, 1),
            (ConductorPurpose::ChooseOutput, 1),
            (ConductorPurpose::ChooseOutput, 2)
        ]
    );
}

/// Without a conductor attached, a step the reprompt did not settle is a
/// problem at once — as before the conductor existed.
#[tokio::test]
async fn without_a_conductor_the_step_is_a_problem_at_once() {
    let fixture = Fixture::new(two_answers_without_label(), Vec::new());
    drop(fixture.registry);
    let execution = fixture
        .engine
        .start_and_run(
            &workflow("agent_problem"),
            json!({}),
            fixture.harness.workspace(),
        )
        .await
        .expect("run");

    assert_eq!(execution.status, ExecutionStatus::InputNeeded);
    assert!(fixture.conductor.calls().is_empty());
}

// ---------------------------------------------------------------------------
// The step's reply survives the rescue (R3)
// ---------------------------------------------------------------------------

const REVIEW_REPORT: &str = "P1: validate the request body";

/// `step_reply` (`review` → `fix`, which reads `steps.review.reply`) on the
/// scripted `turns`, with the scripted conductor attached when `scripts` is
/// given.
async fn run_step_reply(
    harness: &Harness,
    turns: Vec<AgentTurn>,
    scripts: Option<Vec<Script>>,
) -> (Arc<Engine>, Arc<ScriptedAgentExecutor>, WorkflowExecution) {
    run_scripted(harness, "step_reply", turns, scripts).await
}

/// `name` on the scripted `turns`, with the scripted conductor attached when
/// `scripts` is given.
async fn run_scripted(
    harness: &Harness,
    name: &str,
    turns: Vec<AgentTurn>,
    scripts: Option<Vec<Script>>,
) -> (Arc<Engine>, Arc<ScriptedAgentExecutor>, WorkflowExecution) {
    let agent = Arc::new(ScriptedAgentExecutor::new(turns));
    let engine = Arc::new(
        harness
            .engine()
            .with_executor(ActionKind::Agent, agent.clone()),
    );
    let registry =
        scripts.map(|scripts| assemble(&engine, Arc::new(ScriptedConductor::new(scripts)), None));
    let execution = engine
        .start_and_run(&workflow(name), json!({}), harness.workspace())
        .await
        .expect("run the workflow");
    drop(registry);
    (engine, agent, execution)
}

/// The rejected attempt's report, then a reprompt with nothing to say.
fn report_without_label() -> Vec<AgentTurn> {
    vec![
        AgentTurn::NoLabel {
            reply: Some(REVIEW_REPORT),
            session: true,
        },
        AgentTurn::NoLabel {
            reply: None,
            session: true,
        },
    ]
}

#[tokio::test]
async fn a_label_the_conductor_chooses_keeps_the_rejected_attempts_reply() {
    let harness = Harness::new();
    let mut turns = report_without_label();
    turns.push(AgentTurn::label("done", "fixed"));
    let (_engine, agent, execution) = run_step_reply(
        &harness,
        turns,
        Some(vec![choose("findings", "high", json!({}), json!({}))]),
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let prompts = agent.prompts();
    assert_eq!(prompts.len(), 3);
    assert!(prompts[2].contains(REVIEW_REPORT), "{}", prompts[2]);
}

#[tokio::test]
async fn a_label_the_operator_settles_keeps_the_rejected_attempts_reply() {
    let harness = Harness::new();
    let mut turns = report_without_label();
    turns.push(AgentTurn::label("done", "fixed"));
    let (engine, agent, execution) = run_step_reply(&harness, turns, None).await;

    assert_eq!(execution.status, ExecutionStatus::InputNeeded);
    let pending: PendingHumanInput = harness
        .store()
        .load_pending_input(&execution.id)
        .expect("read the pending input")
        .expect("a problem");
    assert_eq!(pending.continuation, InputContinuation::ResolveOutput);
    let delivery = HumanInputDelivery::label("findings").for_episode(pending.episode_key());
    let execution = engine
        .deliver_human_input(&workflow("step_reply"), &execution.id, &delivery)
        .await
        .expect("settle the problem");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let prompts = agent.prompts();
    assert_eq!(prompts.len(), 3);
    assert!(prompts[2].contains(REVIEW_REPORT), "{}", prompts[2]);
}

#[tokio::test]
async fn a_conductor_reprompt_keeps_the_earlier_reply() {
    let harness = Harness::new();
    let mut turns = report_without_label();
    turns.push(AgentTurn::label("findings", "only the output object"));
    turns.push(AgentTurn::label("done", "fixed"));
    let (_engine, agent, execution) = run_step_reply(
        &harness,
        turns,
        Some(vec![Script::Answer(json!({
            "decision": "reprompt",
            "prompt": "Which label describes the review?",
        }))]),
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let calls = agent.calls();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[2].resume_session.as_deref(), Some("agent-session"));
    assert!(
        calls[3].prompt.contains(REVIEW_REPORT),
        "{}",
        calls[3].prompt
    );
    assert!(!calls[3].prompt.contains("only the output object"));
}

/// A label still without its required variable after the engine's reprompt
/// is handed to the conductor (U5), never failed.
#[tokio::test]
async fn a_required_variable_missing_twice_goes_to_the_conductor() {
    let harness = Harness::new();
    let (_engine, agent, execution) = run_scripted(
        &harness,
        "required_variable",
        vec![
            AgentTurn::with_variables("plan_ready", json!({})),
            AgentTurn::with_variables("plan_ready", json!({})),
        ],
        Some(vec![choose(
            "plan_ready",
            "high",
            json!({}),
            json!({ "plan_doc": "docs/plan.md" }),
        )]),
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(agent.calls().len(), 2);
    let events: Vec<EventData> = harness
        .events(&execution.id)
        .into_iter()
        .map(|event| event.data)
        .collect();
    assert_eq!(
        decisions(&events),
        vec![(
            ConductorDecisionKind::ChooseOutput,
            Some("plan_ready".to_string()),
            Confidence::High
        )]
    );
    assert_eq!(
        count(&events, |data| matches!(
            data,
            EventData::ActionFailed { .. }
        )),
        0
    );
    assert_eq!(
        execution.context.variables.get("plan_doc"),
        Some(&json!("docs/plan.md"))
    );
}
