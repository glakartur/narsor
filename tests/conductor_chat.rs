//! Integration tests for the conductor's chat, its proposals, the summaries
//! of `human` gates and the undo of its decisions (U12).
//!
//! The agent and the conductor are both scripted in-process — the agent by
//! the executors of `engine_support`, the conductor by an invoker behind the
//! real registry and actor — so no harness runs, least of all the real
//! `claude`. What is under test is what the operator can rely on: nothing
//! happens before a proposal is confirmed, a confirmation is carried out
//! once and never on a moment that has passed, a gate always waits for the
//! operator, a failed chat changes nothing, and an undone choice leaves the
//! step a problem again without running its agent.

mod engine_support;

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use engine_support::{
    workflow, AgentTurn, Harness, LabellessAgentExecutor, QuestioningAgentExecutor,
    ScriptedAgentExecutor,
};
use narsor::tui::input_form::InputClosure;
use narsor_engine::conductor::assembly::assemble;
use narsor_engine::conductor::{
    annotation_of, ChatAuthor, ChatEntry, ChatEntryKind, ConductorAnswer, ConductorInvoker,
    ConductorRegistry, ConductorStore, Invocation, InvocationOutcome, ProposalError,
    ProposalRecord, ProposalStatus, UndoError,
};
use narsor_engine::domain::{
    AnsweredBy, ConductorDecisionKind, ConductorPurpose, ConductorTrigger, Confidence, EventData,
    ExecutionStatus, InputCloseReason, ProblemKind, WorkflowDefinition, WorkflowExecution,
};
use narsor_engine::engine::{
    ActionExecutor, ActionKind, Engine, EngineError, HumanInputDelivery, InputContinuation,
    PendingHumanInput,
};
use serde_json::{json, Value};
use tokio::sync::Notify;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// One outcome of the scripted conductor.
enum Script {
    /// It answers with this response.
    Answer(Value),
    /// It fails.
    Fail,
    /// It answers with this response once `gate` is opened.
    Gated(Arc<Notify>, Value),
}

/// A conductor that answers each purpose from a list of its own, and
/// records every invocation. A purpose with nothing scripted fails — the
/// way a conductor that cannot answer does.
#[derive(Default)]
struct ScriptedConductor {
    scripts: Mutex<HashMap<ConductorPurpose, VecDeque<Script>>>,
    calls: Mutex<Vec<Invocation>>,
    started: Notify,
}

impl ScriptedConductor {
    fn new(scripts: Vec<(ConductorPurpose, Script)>) -> Self {
        let mut by_purpose: HashMap<ConductorPurpose, VecDeque<Script>> = HashMap::new();
        for (purpose, script) in scripts {
            by_purpose.entry(purpose).or_default().push_back(script);
        }
        Self {
            scripts: Mutex::new(by_purpose),
            ..Self::default()
        }
    }

    fn calls(&self) -> Vec<Invocation> {
        self.calls.lock().unwrap().clone()
    }
}

fn answered(response: Value) -> InvocationOutcome {
    InvocationOutcome::Answered(ConductorAnswer {
        message: response
            .get("message")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        response,
    })
}

impl ConductorInvoker for ScriptedConductor {
    fn invoke<'a>(
        &'a self,
        invocation: &'a Invocation,
    ) -> Pin<Box<dyn Future<Output = InvocationOutcome> + Send + 'a>> {
        self.calls.lock().unwrap().push(invocation.clone());
        let script = self
            .scripts
            .lock()
            .unwrap()
            .get_mut(&invocation.request.purpose)
            .and_then(VecDeque::pop_front);
        self.started.notify_one();
        Box::pin(async move {
            match script {
                Some(Script::Answer(response)) => answered(response),
                Some(Script::Gated(gate, response)) => {
                    gate.notified().await;
                    answered(response)
                }
                Some(Script::Fail) | None => InvocationOutcome::Failed {
                    error: "the scripted conductor failed".to_string(),
                },
            }
        })
    }
}

/// An engine over a temporary store, with the scripted conductor attached.
struct Fixture {
    harness: Harness,
    engine: Arc<Engine>,
    registry: Arc<ConductorRegistry>,
    conductor: Arc<ScriptedConductor>,
    definition: WorkflowDefinition,
}

impl Fixture {
    fn new(
        workflow_name: &str,
        agent: Option<Arc<dyn ActionExecutor>>,
        scripts: Vec<(ConductorPurpose, Script)>,
    ) -> Self {
        let harness = Harness::new();
        let mut engine = harness.engine();
        if let Some(agent) = agent {
            engine = engine.with_executor(ActionKind::Agent, agent);
        }
        let engine = Arc::new(engine);
        let conductor = Arc::new(ScriptedConductor::new(scripts));
        let registry = assemble(&engine, conductor.clone(), None);
        Self {
            harness,
            engine,
            registry,
            conductor,
            definition: workflow(workflow_name),
        }
    }

    async fn run(&self, inputs: Value) -> WorkflowExecution {
        self.engine
            .start_and_run(&self.definition, inputs, self.harness.workspace())
            .await
            .expect("run the workflow")
    }

    fn load(&self, execution_id: &str) -> WorkflowExecution {
        self.harness
            .store()
            .load_execution(execution_id)
            .expect("load the execution")
    }

    fn pending(&self, execution_id: &str) -> PendingHumanInput {
        self.harness
            .store()
            .load_pending_input(execution_id)
            .expect("read the pending input")
            .expect("a pending input")
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

    fn chat(&self, execution_id: &str) -> Vec<ChatEntry> {
        self.conductor_store(execution_id)
            .read_chat()
            .expect("read the chat")
    }

    /// Writes `text` to the chat and waits until the conductor answered it;
    /// returns the entries of the answer (the reply, then its proposals).
    async fn ask(&self, execution_id: &str, text: &str) -> Vec<ChatEntry> {
        let message = self
            .registry
            .post_chat(execution_id, text)
            .expect("post the message");
        wait_until("the conductor answered the chat", || {
            self.chat(execution_id)
                .iter()
                .any(|entry| entry.in_reply_to.as_deref() == Some(message.id.as_str()))
        })
        .await;
        self.chat(execution_id)
            .into_iter()
            .filter(|entry| entry.in_reply_to.as_deref() == Some(message.id.as_str()))
            .collect()
    }

    /// Waits until every invocation the conductor started has finished.
    async fn wait_until_idle(&self, execution_id: &str) {
        wait_until("the conductor finished its invocations", || {
            let events = self.events(execution_id);
            count(&events, |data| {
                matches!(data, EventData::ConductorStarted { .. })
            }) == count(&events, |data| {
                matches!(data, EventData::ConductorFinished { .. })
            }) && self
                .conductor_store(execution_id)
                .load_state()
                .map(|state| state.pending.is_none() && state.queue.is_empty())
                .unwrap_or(true)
        })
        .await;
    }
}

async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !condition() {
        if Instant::now() >= deadline {
            panic!("timed out waiting until {what}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn count<F: Fn(&EventData) -> bool>(events: &[EventData], matches: F) -> usize {
    events.iter().filter(|data| matches(data)).count()
}

fn answers_received(events: &[EventData]) -> Vec<(Option<AnsweredBy>, Option<String>, Value)> {
    events
        .iter()
        .filter_map(|data| match data {
            EventData::HumanInputReceived {
                answered_by,
                close_reason: Some(InputCloseReason::Answered),
                episode,
                input,
                ..
            } => Some((*answered_by, episode.clone(), input.clone())),
            _ => None,
        })
        .collect()
}

fn executed_proposals(events: &[EventData]) -> Vec<(String, Confidence, Option<String>)> {
    events
        .iter()
        .filter_map(|data| match data {
            EventData::ConductorDecision {
                decision_id,
                kind: ConductorDecisionKind::ExecuteProposal,
                confidence,
                rationale,
                ..
            } => Some((decision_id.clone(), *confidence, rationale.clone())),
            _ => None,
        })
        .collect()
}

/// A chat reply proposing `proposal`.
fn reply_proposing(message: &str, proposal: Value) -> Script {
    Script::Answer(json!({
        "decision": "reply",
        "message": message,
        "confidence": "high",
        "rationale": "The operator asked for exactly this.",
        "proposals": [proposal],
    }))
}

/// The chat reply of the human-decision gate: a proposal to answer it with
/// `label`.
fn propose_answer(label: &str) -> (ConductorPurpose, Script) {
    (
        ConductorPurpose::ReplyToChat,
        reply_proposing(
            &format!("I would answer '{label}'."),
            json!({"kind": "answer_input", "label": label, "summary": format!("Answer '{label}'")}),
        ),
    )
}

/// Runs the `human_decision` workflow to its gate.
async fn at_the_gate(fixture: &Fixture) -> (WorkflowExecution, PendingHumanInput) {
    let execution = fixture.run(json!({})).await;
    assert_eq!(execution.status, ExecutionStatus::InputNeeded);
    let pending = fixture.pending(&execution.id);
    assert_eq!(pending.continuation, InputContinuation::CompleteStep);
    (execution, pending)
}

fn proposal_of(entries: &[ChatEntry]) -> (ChatEntry, ProposalRecord) {
    let entry = entries
        .iter()
        .find(|entry| entry.kind == ChatEntryKind::Proposal)
        .cloned()
        .expect("a proposal in the answer");
    let record: ProposalRecord =
        serde_json::from_value(entry.payload.clone().expect("a payload")).expect("a record");
    (entry, record)
}

// ---------------------------------------------------------------------------
// AE17: a proposal to answer the agent, carried out only once confirmed
// ---------------------------------------------------------------------------

/// AE17 (R28, F4): the operator asks the conductor to answer the agent's
/// question. The conductor proposes it; nothing happens until the operator
/// confirms, and then the answer reaches the agent, on record as the
/// conductor's with its rationale and confidence.
#[tokio::test]
async fn a_proposal_to_answer_the_agent_is_carried_out_only_once_confirmed() {
    let agent = Arc::new(QuestioningAgentExecutor::new());
    let fixture = Fixture::new(
        "agent_question",
        Some(agent.clone()),
        vec![(
            ConductorPurpose::ReplyToChat,
            reply_proposing(
                "I will tell the agent to use variant B once you confirm.",
                json!({
                    "kind": "message_agent",
                    "summary": "Tell the agent to use variant B",
                    "text": "Use variant B."
                }),
            ),
        )],
    );
    let execution = fixture.run(json!({})).await;
    assert_eq!(execution.status, ExecutionStatus::InputNeeded);
    let pending = fixture.pending(&execution.id);
    assert_eq!(pending.continuation, InputContinuation::ResumeAgent);

    let answer = fixture
        .ask(&execution.id, "odpowiedz agentowi, że wariant B")
        .await;
    assert_eq!(answer[0].kind, ChatEntryKind::Message);
    assert_eq!(answer[0].author, ChatAuthor::Conductor);
    let (proposal, record) = proposal_of(&answer);
    assert_eq!(proposal.text, "Tell the agent to use variant B");
    assert_eq!(record.episode.as_deref(), Some(pending.episode_key()));
    assert_eq!(record.state_id.as_deref(), Some("implement"));
    assert_eq!(record.confidence, Some(Confidence::High));

    let calls = fixture.conductor.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].request.trigger, ConductorTrigger::Chat);
    assert_eq!(calls[0].request.purpose, ConductorPurpose::ReplyToChat);

    // Nothing happened yet: the question still waits, unanswered.
    fixture.wait_until_idle(&execution.id).await;
    assert_eq!(
        fixture.load(&execution.id).status,
        ExecutionStatus::InputNeeded
    );
    assert!(answers_received(&fixture.events(&execution.id)).is_empty());
    assert_eq!(agent.prompts().len(), 1);

    let confirmed = fixture
        .registry
        .confirm_proposal(&fixture.definition, &execution.id, &proposal.id)
        .expect("confirm the proposal");
    assert_eq!(confirmed.outcome.status, ProposalStatus::Executed);
    assert!(!confirmed.outcome.repeated);
    let finished = confirmed
        .pass
        .expect("answering the agent runs it on")
        .run_to_completion(&fixture.definition)
        .await
        .expect("run on");
    assert_eq!(finished.status, ExecutionStatus::Completed);

    // The agent got the answer, from the conductor.
    let prompts = agent.prompts();
    assert_eq!(prompts.len(), 2);
    assert!(prompts[1].contains("Use variant B."), "{}", prompts[1]);
    let events = fixture.events(&execution.id);
    assert_eq!(
        answers_received(&events),
        vec![(
            Some(AnsweredBy::Conductor),
            Some(pending.episode_key().to_string()),
            json!({"answer": "Use variant B."}),
        )]
    );
    let decisions = executed_proposals(&events);
    assert_eq!(
        decisions,
        vec![(
            confirmed
                .outcome
                .decision_id
                .clone()
                .expect("a decision id"),
            Confidence::High,
            Some("The operator asked for exactly this.".to_string()),
        )]
    );

    // The chat says what was done, answering the proposal.
    let decision = fixture
        .chat(&execution.id)
        .into_iter()
        .find(|entry| entry.kind == ChatEntryKind::Decision)
        .expect("a decision entry");
    assert_eq!(decision.author, ChatAuthor::Conductor);
    assert_eq!(decision.in_reply_to.as_deref(), Some(proposal.id.as_str()));
    assert_eq!(
        decision.payload.as_ref().expect("a payload")["decision_id"],
        json!(confirmed.outcome.decision_id)
    );

    // A carried-out proposal is not a choice of the conductor's to undo.
    let refused = fixture
        .registry
        .undo_decision(
            &fixture.definition,
            &execution.id,
            confirmed.outcome.decision_id.as_deref().unwrap(),
            || {},
        )
        .await
        .expect_err("not undoable");
    assert!(matches!(refused, UndoError::NotUndoable(_)), "{refused:?}");
}

// ---------------------------------------------------------------------------
// AE16 (R30): a gate is summarized, and still waits for the operator
// ---------------------------------------------------------------------------

/// AE16: entering a `human` gate, the conductor summarizes it and
/// recommends an answer; the gate goes on waiting for the operator.
#[tokio::test]
async fn a_human_gate_is_summarized_and_still_waits_for_the_operator() {
    let fixture = Fixture::new(
        "human_decision",
        None,
        vec![(
            ConductorPurpose::SummarizeGate,
            Script::Answer(json!({
                "decision": "annotate",
                "summary": "The check passed; decide whether to ship.",
                "recommendation": "ok",
                "confidence": "high",
                "rationale": "The check reported ok.",
            })),
        )],
    );
    let (execution, pending) = at_the_gate(&fixture).await;

    let store = fixture.harness.store();
    wait_until("the gate was summarized", || {
        annotation_of(&store, &execution.id, &pending).is_some()
    })
    .await;
    let annotation = annotation_of(&store, &execution.id, &pending).expect("an annotation");
    assert_eq!(annotation.key, pending.episode_key());
    assert_eq!(
        annotation.summary,
        "The check passed; decide whether to ship."
    );
    assert_eq!(annotation.recommendation.as_deref(), Some("ok"));

    let calls = fixture.conductor.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].request.trigger, ConductorTrigger::Annotation);
    assert_eq!(calls[0].request.purpose, ConductorPurpose::SummarizeGate);
    assert_eq!(
        calls[0].request.episode.as_deref(),
        Some(pending.episode_key())
    );
    assert_eq!(calls[0].request.payload["state_id"], "check");
    assert_eq!(
        calls[0].request.payload["instruction"],
        "Is this good enough to ship?"
    );

    // The gate still waits, on the same input, and nobody answered it.
    fixture.wait_until_idle(&execution.id).await;
    let now = fixture.load(&execution.id);
    assert_eq!(now.status, ExecutionStatus::InputNeeded);
    assert_eq!(
        fixture.pending(&execution.id).episode_key(),
        pending.episode_key()
    );
    let events = fixture.events(&execution.id);
    assert!(answers_received(&events).is_empty());
    assert_eq!(
        count(&events, |data| matches!(
            data,
            EventData::ConductorDecision { .. }
        )),
        0
    );
    assert!(
        fixture.chat(&execution.id).is_empty(),
        "no chat for a summary"
    );
}

/// R30: a summary that arrives after the operator answered the gate is
/// dropped, and never reopens the input.
#[tokio::test]
async fn a_summary_that_arrives_after_the_answer_is_dropped() {
    let gate = Arc::new(Notify::new());
    let fixture = Fixture::new(
        "human_decision",
        None,
        vec![(
            ConductorPurpose::SummarizeGate,
            Script::Gated(
                Arc::clone(&gate),
                json!({"decision": "annotate", "summary": "Too late.", "recommendation": "ok"}),
            ),
        )],
    );
    let (execution, pending) = at_the_gate(&fixture).await;
    wait_until("the summary started", || {
        !fixture.conductor.calls().is_empty()
    })
    .await;

    // The operator answers first.
    let answered = fixture
        .engine
        .deliver_human_input(
            &fixture.definition,
            &execution.id,
            &HumanInputDelivery::label("ok").for_episode(pending.episode_key()),
        )
        .await
        .expect("the operator answers");
    assert_eq!(answered.status, ExecutionStatus::Completed);

    gate.notify_one();
    fixture.wait_until_idle(&execution.id).await;

    let conductor = fixture.conductor_store(&execution.id);
    assert_eq!(
        conductor
            .load_annotation(pending.episode_key())
            .expect("read the annotation"),
        None
    );
    let now = fixture.load(&execution.id);
    assert_eq!(now.status, ExecutionStatus::Completed);
    assert!(!fixture.harness.store().has_pending_input(&execution.id));
    assert_eq!(answers_received(&fixture.events(&execution.id)).len(), 1);
}

/// R30: a summary still queued when the operator answers its gate is never
/// run — no invocation is spent on a gate nobody waits on.
#[tokio::test]
async fn a_summary_queued_for_an_answered_gate_is_not_run() {
    let chat_gate = Arc::new(Notify::new());
    let fixture = Fixture::new(
        "human_decision",
        None,
        vec![
            (
                ConductorPurpose::ReplyToChat,
                Script::Gated(
                    Arc::clone(&chat_gate),
                    json!({"decision": "reply", "message": "Hello.", "confidence": "high"}),
                ),
            ),
            (
                ConductorPurpose::SummarizeGate,
                Script::Answer(json!({"decision": "annotate", "summary": "Too late."})),
            ),
        ],
    );
    let execution = fixture
        .engine
        .start(&fixture.definition, json!({}), fixture.harness.workspace())
        .expect("start");
    // The operator's chat runs first, so the gate's summary queues behind it.
    fixture
        .registry
        .post_chat(&execution.id, "hello")
        .expect("post");
    wait_until("the chat is under way", || {
        fixture.conductor.calls().len() == 1
    })
    .await;
    let at_gate = fixture
        .engine
        .run(&fixture.definition, &execution.id)
        .await
        .expect("run to the gate");
    assert_eq!(at_gate.status, ExecutionStatus::InputNeeded);
    let pending = fixture.pending(&execution.id);
    let actor = fixture.registry.actor(&execution.id).expect("the actor");
    assert_eq!(actor.queued(), 1, "the summary waits behind the chat");

    fixture
        .engine
        .deliver_human_input(
            &fixture.definition,
            &execution.id,
            &HumanInputDelivery::label("ok").for_episode(pending.episode_key()),
        )
        .await
        .expect("the operator answers");
    chat_gate.notify_one();
    fixture.wait_until_idle(&execution.id).await;

    let calls = fixture.conductor.calls();
    assert_eq!(calls.len(), 1, "only the chat ran: {calls:?}");
    assert_eq!(calls[0].request.purpose, ConductorPurpose::ReplyToChat);
    assert!(!fixture.events(&execution.id).iter().any(|data| matches!(
        data,
        EventData::ConductorStarted {
            trigger: ConductorTrigger::Annotation,
            ..
        }
    )));
}

/// R30: a summary under way when the operator answers its gate is
/// interrupted rather than left to run to its end.
#[tokio::test]
async fn a_running_summary_is_interrupted_when_its_gate_is_answered() {
    let never = Arc::new(Notify::new());
    let fixture = Fixture::new(
        "human_decision",
        None,
        vec![(
            ConductorPurpose::SummarizeGate,
            Script::Gated(
                Arc::clone(&never),
                json!({"decision": "annotate", "summary": "Never."}),
            ),
        )],
    );
    let (execution, pending) = at_the_gate(&fixture).await;
    wait_until("the summary started", || {
        !fixture.conductor.calls().is_empty()
    })
    .await;

    fixture
        .engine
        .deliver_human_input(
            &fixture.definition,
            &execution.id,
            &HumanInputDelivery::label("ok").for_episode(pending.episode_key()),
        )
        .await
        .expect("the operator answers");
    fixture.wait_until_idle(&execution.id).await;

    assert!(fixture.events(&execution.id).iter().any(|data| matches!(
        data,
        EventData::ConductorFinished {
            outcome: narsor_engine::domain::ConductorOutcome::Interrupted,
            ..
        }
    )));
    assert_eq!(
        fixture
            .conductor_store(&execution.id)
            .load_annotation(pending.episode_key())
            .expect("read the annotation"),
        None
    );
}

/// KTD12: a summary the conductor cannot make is an error in the chat, and
/// nothing else.
#[tokio::test]
async fn a_summary_that_fails_leaves_an_error_in_the_chat_only() {
    let fixture = Fixture::new(
        "human_decision",
        None,
        vec![(ConductorPurpose::SummarizeGate, Script::Fail)],
    );
    let (execution, pending) = at_the_gate(&fixture).await;
    wait_until("the failure reached the chat", || {
        !fixture.chat(&execution.id).is_empty()
    })
    .await;
    fixture.wait_until_idle(&execution.id).await;

    let chat = fixture.chat(&execution.id);
    assert_eq!(chat.len(), 1, "{chat:?}");
    assert_eq!(chat[0].kind, ChatEntryKind::Error);
    assert!(chat[0].text.contains("scripted conductor failed"));
    assert_eq!(
        fixture.pending(&execution.id).episode_key(),
        pending.episode_key()
    );
    let events = fixture.events(&execution.id);
    assert_eq!(
        count(&events, |data| matches!(
            data,
            EventData::ProblemRaised { .. }
        )),
        0
    );
    let state = fixture
        .conductor_store(&execution.id)
        .load_state()
        .expect("read the state");
    assert!(state.invocation_attempts.is_empty(), "{state:?}");
    assert_eq!(state.failure, None);
}

// ---------------------------------------------------------------------------
// AE13: the conductor's answer closes the form another window holds
// ---------------------------------------------------------------------------

/// AE13: a confirmed proposal answers the gate a second window has a form
/// open on. That form learns the conductor answered, and its own answer is
/// refused as late.
#[tokio::test]
async fn a_confirmed_answer_closes_the_form_of_another_window() {
    let fixture = Fixture::new("human_decision", None, vec![propose_answer("ok")]);
    let (execution, pending) = at_the_gate(&fixture).await;
    // The second window's form: the request it shows, and its key.
    let held = pending.episode_key().to_string();

    let answer = fixture.ask(&execution.id, "ship it for me").await;
    let (proposal, record) = proposal_of(&answer);
    assert_eq!(record.episode.as_deref(), Some(held.as_str()));

    let confirmed = fixture
        .registry
        .confirm_proposal(&fixture.definition, &execution.id, &proposal.id)
        .expect("confirm");
    let finished = confirmed
        .pass
        .expect("a pass")
        .run_to_completion(&fixture.definition)
        .await
        .expect("run on");
    assert_eq!(finished.status, ExecutionStatus::Completed);

    // What the form reads from the stream: answered, by the conductor.
    let events = fixture.events(&execution.id);
    let closure = events
        .iter()
        .find_map(InputClosure::from_event)
        .expect("the input was closed");
    assert_eq!(closure.0, Some(held.as_str()));
    assert_eq!(closure.1.answered_by, Some(AnsweredBy::Conductor));
    assert!(
        closure.1.message().contains("the conductor"),
        "{}",
        closure.1.message()
    );
    let decided = events
        .iter()
        .find_map(|data| match data {
            EventData::DecisionMade {
                output_label,
                provider,
                ..
            } => Some((output_label.clone(), provider.clone())),
            _ => None,
        })
        .expect("a decision");
    assert_eq!(decided.0, "ok");

    // The form's own answer comes too late.
    let late = fixture
        .engine
        .try_begin_deliver_human_input(
            &fixture.definition,
            &execution.id,
            &HumanInputDelivery::label("rejected").for_episode(held),
        )
        .err()
        .expect("refused");
    assert!(
        matches!(late, EngineError::HumanInput(ref human) if human.is_stale()),
        "{late:?}"
    );
}

/// KTD9: two windows confirm the same proposal at once; it is carried out
/// once, and the second confirmation answers with the first one's result.
#[tokio::test]
async fn two_windows_confirming_one_proposal_carry_it_out_once() {
    let fixture = Fixture::new("human_decision", None, vec![propose_answer("ok")]);
    let (execution, _) = at_the_gate(&fixture).await;
    let answer = fixture.ask(&execution.id, "ship it").await;
    let (proposal, _) = proposal_of(&answer);

    let (first, second) = std::thread::scope(|scope| {
        let confirm = || {
            fixture
                .registry
                .confirm_proposal(&fixture.definition, &execution.id, &proposal.id)
                .expect("confirm")
        };
        let first = scope.spawn(confirm);
        let second = scope.spawn(confirm);
        (first.join().unwrap(), second.join().unwrap())
    });

    let outcomes = [&first.outcome, &second.outcome];
    assert_eq!(
        outcomes.iter().filter(|outcome| outcome.repeated).count(),
        1,
        "{outcomes:?}"
    );
    assert_eq!(first.outcome.decision_id, second.outcome.decision_id);
    assert_eq!(first.outcome.message, second.outcome.message);
    let passes: Vec<_> = [first.pass, second.pass].into_iter().flatten().collect();
    assert_eq!(passes.len(), 1, "one pass runs the answer on");
    for pass in passes {
        pass.run_to_completion(&fixture.definition)
            .await
            .expect("run on");
    }

    let events = fixture.events(&execution.id);
    assert_eq!(answers_received(&events).len(), 1);
    assert_eq!(executed_proposals(&events).len(), 1);
    assert_eq!(
        fixture.load(&execution.id).status,
        ExecutionStatus::Completed
    );

    // A third, later confirmation still answers the same, and does nothing.
    let again = fixture
        .registry
        .confirm_proposal(&fixture.definition, &execution.id, &proposal.id)
        .expect("confirm again");
    assert!(again.outcome.repeated);
    assert!(again.pass.is_none());
    assert_eq!(executed_proposals(&fixture.events(&execution.id)).len(), 1);
}

/// KTD9: a proposal about an input the operator answered meanwhile is
/// refused with a readable reason, and nothing is recorded as done.
#[tokio::test]
async fn a_proposal_about_a_moment_that_passed_is_refused() {
    let fixture = Fixture::new("human_decision", None, vec![propose_answer("ok")]);
    let (execution, pending) = at_the_gate(&fixture).await;
    let answer = fixture.ask(&execution.id, "ship it").await;
    let (proposal, _) = proposal_of(&answer);

    let answered = fixture
        .engine
        .deliver_human_input(
            &fixture.definition,
            &execution.id,
            &HumanInputDelivery::label("rejected").for_episode(pending.episode_key()),
        )
        .await
        .expect("the operator answers first");
    assert_eq!(answered.status, ExecutionStatus::Failed);
    let events_before = fixture.events(&execution.id).len();

    let refused = fixture
        .registry
        .confirm_proposal(&fixture.definition, &execution.id, &proposal.id)
        .err()
        .expect("refused");
    let ProposalError::Stale(message) = &refused else {
        panic!("expected a stale proposal, got {refused:?}");
    };
    assert!(message.contains("moved on"), "{message}");
    assert!(message.contains("Answer 'ok'"), "{message}");

    assert_eq!(fixture.events(&execution.id).len(), events_before);
    assert_eq!(fixture.load(&execution.id).status, ExecutionStatus::Failed);
    assert!(!fixture
        .chat(&execution.id)
        .iter()
        .any(|entry| entry.kind == ChatEntryKind::Decision));

    // Rejected, it can no longer be confirmed either.
    let rejected = fixture
        .registry
        .reject_proposal(&execution.id, &proposal.id)
        .expect("reject");
    assert_eq!(rejected.status, ProposalStatus::Rejected);
    let again = fixture
        .registry
        .reject_proposal(&execution.id, &proposal.id)
        .expect("reject again");
    assert!(again.repeated);
    assert!(matches!(
        fixture
            .registry
            .confirm_proposal(&fixture.definition, &execution.id, &proposal.id)
            .err(),
        Some(ProposalError::Settled(_))
    ));
}

// ---------------------------------------------------------------------------
// KTD12: a chat the conductor cannot answer changes nothing
// ---------------------------------------------------------------------------

/// KTD12: a chat invocation that fails is an error entry answering the
/// message, and nothing else — no problem, no counter, no change to the
/// execution.
#[tokio::test]
async fn a_failed_chat_leaves_an_error_entry_and_changes_nothing() {
    let fixture = Fixture::new(
        "linear",
        None,
        vec![(ConductorPurpose::ReplyToChat, Script::Fail)],
    );
    let execution = fixture.run(json!({"subject": "chat"})).await;
    assert_eq!(execution.status, ExecutionStatus::Completed);
    let before = fixture.load(&execution.id);

    let answer = fixture.ask(&execution.id, "what happened?").await;
    assert_eq!(answer.len(), 1, "{answer:?}");
    assert_eq!(answer[0].kind, ChatEntryKind::Error);
    assert_eq!(answer[0].author, ChatAuthor::Conductor);
    assert!(answer[0].text.contains("scripted conductor failed"));
    fixture.wait_until_idle(&execution.id).await;

    let after = fixture.load(&execution.id);
    assert_eq!(after.status, before.status);
    assert_eq!(after.current_state, before.current_state);
    assert_eq!(after.context.variables, before.context.variables);
    assert_eq!(after.metadata, before.metadata);
    let events = fixture.events(&execution.id);
    assert_eq!(
        count(&events, |data| matches!(
            data,
            EventData::ProblemRaised { .. } | EventData::ConductorDecision { .. }
        )),
        0
    );
    let state = fixture
        .conductor_store(&execution.id)
        .load_state()
        .expect("read the state");
    assert!(state.invocation_attempts.is_empty(), "{state:?}");
    assert_eq!(fixture.conductor.calls().len(), 1, "never repeated (R24)");
}

// ---------------------------------------------------------------------------
// KTD13: undoing the conductor's choice of an output
// ---------------------------------------------------------------------------

/// KTD13: the conductor completed a step with its choice (a) and the
/// workflow ran on. Undone, the context from before it is back and the step
/// is a problem again at its state — its agent is not run again — with a
/// warning that the step after it had already run.
#[tokio::test]
async fn undoing_a_chosen_output_makes_the_step_a_problem_again() {
    let fixture = Fixture::new(
        "agent_problem",
        Some(Arc::new(LabellessAgentExecutor::answering(
            "Done: three units.",
        ))),
        vec![(
            ConductorPurpose::ChooseOutputAfterReprompt,
            Script::Answer(json!({
                "decision": "choose_output",
                "label": "implemented",
                "data": {"units_completed": 3},
                "variables": {"summary": "three units"},
                "confidence": "high",
                "rationale": "the agent said it finished three units",
            })),
        )],
    );
    let execution = fixture.run(json!({})).await;
    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(
        execution.context.variables.get("summary"),
        Some(&json!("three units"))
    );
    let events = fixture.events(&execution.id);
    let work_attempts = |events: &[EventData]| {
        count(
            events,
            |data| matches!(data, EventData::ActionStarted { state_id, .. } if state_id == "work"),
        )
    };
    assert_eq!(work_attempts(&events), 1);
    let decision_id = events
        .iter()
        .find_map(|data| match data {
            EventData::ConductorDecision {
                decision_id,
                kind: ConductorDecisionKind::ChooseOutput,
                ..
            } => Some(decision_id.clone()),
            _ => None,
        })
        .expect("the conductor chose the output");

    let undone = fixture
        .registry
        .undo_decision(&fixture.definition, &execution.id, &decision_id, || {})
        .await
        .expect("undo the decision");
    assert_eq!(undone.decision_id, decision_id);
    assert_eq!(undone.state_id, "work");
    let warning = undone.warning.clone().expect("a warning");
    assert!(warning.contains("'report'"), "{warning}");

    // Back at the step, with the context from before the decision.
    let now = fixture.load(&execution.id);
    assert_eq!(now.status, ExecutionStatus::InputNeeded);
    assert_eq!(now.current_state.as_deref(), Some("work"));
    assert_eq!(now.completed_at, None);
    assert_eq!(now.context.variables.get("summary"), None);
    let pending = fixture.pending(&execution.id);
    assert_eq!(pending.continuation, InputContinuation::ResolveOutput);
    assert_eq!(pending.episode_key(), undone.episode);
    let problem = pending.problem.clone().expect("a problem");
    assert_eq!(problem.answers.len(), 1);
    assert_eq!(
        problem.answers[0].answer_tail.as_deref(),
        Some("Done: three units.")
    );
    let analysis = problem.analysis.expect("the undone choice as the analysis");
    assert_eq!(analysis.label.as_deref(), Some("implemented"));
    assert_eq!(
        analysis.variables.get("summary"),
        Some(&json!("three units"))
    );

    // On record, and the agent was not run again.
    let events = fixture.events(&execution.id);
    assert_eq!(work_attempts(&events), 1);
    let undone_at = events
        .iter()
        .position(|data| {
            matches!(
                data,
                EventData::ConductorDecisionUndone { decision_id: id, state_id, .. }
                    if *id == decision_id && state_id == "work"
            )
        })
        .expect("ConductorDecisionUndone");
    assert!(events[undone_at..].iter().any(|data| matches!(
        data,
        EventData::ProblemRaised { kind: ProblemKind::Output, state_id, .. } if state_id == "work"
    )));
    let chat = fixture.chat(&execution.id);
    let entry = chat.last().expect("the undo in the chat");
    assert_eq!(entry.kind, ChatEntryKind::Decision);
    assert_eq!(
        entry.payload.as_ref().expect("a payload")["decision_id"],
        json!(decision_id)
    );

    // Undone once only.
    let again = fixture
        .registry
        .undo_decision(&fixture.definition, &execution.id, &decision_id, || {})
        .await
        .expect_err("already undone");
    assert!(matches!(again, UndoError::AlreadyUndone(_)), "{again:?}");
    let unknown = fixture
        .registry
        .undo_decision(
            &fixture.definition,
            &execution.id,
            "no-such-decision",
            || {},
        )
        .await
        .expect_err("unknown");
    assert!(matches!(unknown, UndoError::NotFound(_)), "{unknown:?}");

    // The problem is a real one: the operator settles it.
    let settled = fixture
        .engine
        .deliver_human_input(
            &fixture.definition,
            &execution.id,
            &HumanInputDelivery::default()
                .choosing("implemented")
                .with_variables(serde_json::Map::from_iter([(
                    "summary".to_string(),
                    json!("settled by hand"),
                )]))
                .for_episode(pending.episode_key()),
        )
        .await
        .expect("settle the problem");
    assert_eq!(settled.status, ExecutionStatus::Completed);
    assert_eq!(
        settled.context.variables.get("summary"),
        Some(&json!("settled by hand"))
    );
}

/// KTD13, R3, R10: an undone choice raises the problem again with the
/// step's reply, so settling it hands the original prose on, and the entry
/// its transition counted into the next step is taken back.
#[tokio::test]
async fn undoing_a_chosen_output_keeps_the_reply_and_takes_back_the_entry() {
    let agent = Arc::new(ScriptedAgentExecutor::new(vec![
        AgentTurn::NoLabel {
            reply: Some("The original prose."),
            session: true,
        },
        AgentTurn::NoLabel {
            reply: None,
            session: true,
        },
        AgentTurn::label("fixed", "Fixed once."),
        AgentTurn::label("fixed", "Fixed again."),
    ]));
    let fixture = Fixture::new(
        "step_reply_problem",
        Some(agent.clone()),
        vec![(
            ConductorPurpose::ChooseOutput,
            Script::Answer(json!({
                "decision": "choose_output",
                "label": "done",
                "confidence": "high",
            })),
        )],
    );
    let execution = fixture.run(json!({})).await;
    assert_eq!(execution.status, ExecutionStatus::Completed);
    let entries = |execution: &WorkflowExecution| {
        execution.metadata["engine"]["state_entries"]["fix"].clone()
    };
    assert_eq!(entries(&execution), json!(1));
    let decision_id = fixture
        .events(&execution.id)
        .iter()
        .find_map(|data| match data {
            EventData::ConductorDecision {
                decision_id,
                kind: ConductorDecisionKind::ChooseOutput,
                ..
            } => Some(decision_id.clone()),
            _ => None,
        })
        .expect("the conductor chose the output");

    fixture
        .registry
        .undo_decision(&fixture.definition, &execution.id, &decision_id, || {})
        .await
        .expect("undo the decision");
    let now = fixture.load(&execution.id);
    assert_eq!(entries(&now), Value::Null, "the entry is taken back");
    let pending = fixture.pending(&execution.id);
    let problem = pending.problem.clone().expect("a problem");
    assert_eq!(problem.reply.as_deref(), Some("The original prose."));

    let settled = fixture
        .engine
        .deliver_human_input(
            &fixture.definition,
            &execution.id,
            &HumanInputDelivery::default()
                .choosing("done")
                .for_episode(pending.episode_key()),
        )
        .await
        .expect("settle the problem");
    assert_eq!(settled.status, ExecutionStatus::Completed);
    assert_eq!(entries(&settled), json!(1));
    let prompts = agent.prompts();
    assert_eq!(prompts.len(), 4, "{prompts:?}");
    assert!(prompts[3].contains("The original prose."), "{}", prompts[3]);
}

/// The `agent_problem` fixture whose conductor completes the step with its
/// choice (a); runs it and returns the execution with the decision's id.
async fn completed_by_a_chosen_output() -> (Fixture, WorkflowExecution, String) {
    let fixture = Fixture::new(
        "agent_problem",
        Some(Arc::new(LabellessAgentExecutor::answering("Done."))),
        vec![(
            ConductorPurpose::ChooseOutputAfterReprompt,
            Script::Answer(json!({
                "decision": "choose_output",
                "label": "implemented",
                "data": {"units_completed": 3},
                "variables": {"summary": "three units"},
                "confidence": "high",
            })),
        )],
    );
    let execution = fixture.run(json!({})).await;
    assert_eq!(execution.status, ExecutionStatus::Completed);
    let decision_id = fixture
        .events(&execution.id)
        .iter()
        .find_map(|data| match data {
            EventData::ConductorDecision {
                decision_id,
                kind: ConductorDecisionKind::ChooseOutput,
                ..
            } => Some(decision_id.clone()),
            _ => None,
        })
        .expect("the conductor chose the output");
    (fixture, execution, decision_id)
}

/// Writes a later choice of the conductor's, `later-choice`, after the
/// decision `first` — a copy of it, as a looping workflow would make.
fn note_a_later_choice(fixture: &Fixture, execution_id: &str, first: &str) {
    let saved: narsor_engine::conductor::SavedDecision = fixture
        .chat(execution_id)
        .iter()
        .find(|entry| {
            entry.kind == ChatEntryKind::Decision
                && entry
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("decision_id"))
                    == Some(&json!(first))
        })
        .and_then(|entry| serde_json::from_value(entry.payload.clone()?).ok())
        .expect("the saved decision");
    let later = narsor_engine::conductor::SavedDecision {
        decision_id: "later-choice".to_string(),
        ..saved
    };
    fixture
        .registry
        .note_decision(execution_id, &later, None)
        .expect("note a later choice");
}

/// KTD13: only the latest choice can be undone. One followed by a later
/// choice of the conductor's is superseded and refused, and nothing changes.
#[tokio::test]
async fn an_older_chosen_output_cannot_be_undone() {
    let (fixture, execution, first) = completed_by_a_chosen_output().await;
    note_a_later_choice(&fixture, &execution.id, &first);

    let refused = fixture
        .registry
        .undo_decision(&fixture.definition, &execution.id, &first, || {})
        .await
        .expect_err("superseded");
    assert!(matches!(refused, UndoError::Superseded(_)), "{refused:?}");
    let now = fixture.load(&execution.id);
    assert_eq!(now.status, ExecutionStatus::Completed);
    assert!(!fixture
        .events(&execution.id)
        .iter()
        .any(|data| matches!(data, EventData::ConductorDecisionUndone { .. })));
}

/// KTD13: once a later decision was undone, an earlier one cannot be
/// undone either — it would restore a context from before the operator's
/// own correction.
#[tokio::test]
async fn a_choice_before_an_undo_cannot_be_undone() {
    let (fixture, execution, first) = completed_by_a_chosen_output().await;
    note_a_later_choice(&fixture, &execution.id, &first);
    fixture
        .registry
        .undo_decision(&fixture.definition, &execution.id, "later-choice", || {})
        .await
        .expect("the latest choice is undoable");

    let refused = fixture
        .registry
        .undo_decision(&fixture.definition, &execution.id, &first, || {})
        .await
        .expect_err("superseded by the undo");
    assert!(matches!(refused, UndoError::Superseded(_)), "{refused:?}");
}

/// A cancelled execution stays cancelled: undoing a decision of it is
/// refused, and nothing changes.
#[tokio::test]
async fn a_decision_of_a_cancelled_execution_cannot_be_undone() {
    let (fixture, execution, decision_id) = completed_by_a_chosen_output().await;
    let mut cancelled = fixture.load(&execution.id);
    cancelled.status = ExecutionStatus::Cancelled;
    fixture
        .harness
        .store()
        .save_execution(&cancelled)
        .expect("save the execution");

    let refused = fixture
        .registry
        .undo_decision(&fixture.definition, &execution.id, &decision_id, || {})
        .await
        .expect_err("cancelled");
    assert!(matches!(refused, UndoError::Cancelled(_)), "{refused:?}");
    assert_eq!(
        fixture.load(&execution.id).status,
        ExecutionStatus::Cancelled
    );
}

/// A workflow whose agent step is followed by a step that runs for long:
/// the conductor's choice leads into it, and it is still running when the
/// choice is undone.
const SLOW_AFTER_AGENT: &str = r#"
name: slow-after-agent
version: 1
states:
  work:
    action:
      type: agent
      prompt: Implement the feature.
    choices:
      - implemented
    transitions:
      implemented: slow
  slow:
    action:
      type: script
      command: sleep
      args:
        - "30"
    choices:
      - ok
    transitions:
      ok: finished
  finished:
    description: Done.
terminal_states:
  finished: completed
"#;

/// KTD13: undoing a decision while a pass still drives the execution — the
/// step after it is running — interrupts that pass first, waits for its
/// claim, and warns that the step it started is not undone.
#[tokio::test]
async fn an_undo_interrupts_the_pass_that_drives_the_execution() {
    let mut fixture = Fixture::new(
        "agent_problem",
        Some(Arc::new(LabellessAgentExecutor::answering("Done."))),
        vec![(
            ConductorPurpose::ChooseOutputAfterReprompt,
            Script::Answer(json!({
                "decision": "choose_output",
                "label": "implemented",
                "confidence": "high",
            })),
        )],
    );
    let path = fixture.harness.root().join("slow_after_agent.yaml");
    std::fs::write(&path, SLOW_AFTER_AGENT).expect("write the workflow");
    fixture.definition = narsor_engine::workflow::load_from_file(&path).expect("load the workflow");

    let execution = fixture
        .engine
        .start(&fixture.definition, json!({}), fixture.harness.workspace())
        .expect("start");
    let engine = Arc::clone(&fixture.engine);
    let definition = fixture.definition.clone();
    let id = execution.id.clone();
    let pass = tokio::spawn(async move {
        let _ = engine.run(&definition, &id).await;
    });
    wait_until("the step after the decision started", || {
        fixture.events(&execution.id).iter().any(
            |data| matches!(data, EventData::ActionStarted { state_id, .. } if state_id == "slow"),
        )
    })
    .await;
    assert!(fixture.engine.is_active(&execution.id));
    let decision_id = fixture
        .events(&execution.id)
        .iter()
        .find_map(|data| match data {
            EventData::ConductorDecision { decision_id, .. } => Some(decision_id.clone()),
            _ => None,
        })
        .expect("a decision");

    let abort = pass.abort_handle();
    let undone = fixture
        .registry
        .undo_decision(
            &fixture.definition,
            &execution.id,
            &decision_id,
            move || abort.abort(),
        )
        .await
        .expect("undo");
    assert!(pass.await.unwrap_err().is_cancelled());

    assert_eq!(undone.state_id, "work");
    let warning = undone.warning.expect("a warning");
    assert!(warning.contains("'slow'"), "{warning}");
    let now = fixture.load(&execution.id);
    assert_eq!(now.status, ExecutionStatus::InputNeeded);
    assert_eq!(now.current_state.as_deref(), Some("work"));
    assert!(!fixture.engine.is_active(&execution.id));
}

// ---------------------------------------------------------------------------
// The other kinds of proposal
// ---------------------------------------------------------------------------

/// R28: a confirmed transition is the manual transition of its label, made
/// by the conductor; it closes the gate it leaves.
#[tokio::test]
async fn a_confirmed_transition_takes_the_proposed_label() {
    let fixture = Fixture::new(
        "human_decision",
        None,
        vec![(
            ConductorPurpose::ReplyToChat,
            reply_proposing("Take 'ok'.", json!({"kind": "transition", "label": "ok"})),
        )],
    );
    let (execution, pending) = at_the_gate(&fixture).await;
    let (proposal, _) = proposal_of(&fixture.ask(&execution.id, "move on").await);
    assert_eq!(proposal.text, "Take the transition of output 'ok'");

    let confirmed = fixture
        .registry
        .confirm_proposal(&fixture.definition, &execution.id, &proposal.id)
        .expect("confirm");
    assert!(confirmed.pass.is_none(), "a terminal target runs nothing");
    let now = fixture.load(&execution.id);
    assert_eq!(now.status, ExecutionStatus::Completed);
    assert_eq!(now.current_state.as_deref(), Some("completed"));

    let events = fixture.events(&execution.id);
    assert!(events.iter().any(|data| matches!(
        data,
        EventData::HumanInputReceived {
            close_reason: Some(InputCloseReason::Transition),
            answered_by: Some(AnsweredBy::Conductor),
            episode: Some(episode),
            ..
        } if episode == pending.episode_key()
    )));
    assert!(events.iter().any(|data| matches!(
        data,
        EventData::DecisionMade { provider, output_label, .. }
            if provider == "conductor" && output_label == "ok"
    )));
    assert_eq!(executed_proposals(&events).len(), 1);
}

/// R28: a confirmed abort cancels the execution.
#[tokio::test]
async fn a_confirmed_abort_cancels_the_execution() {
    let fixture = Fixture::new(
        "human_decision",
        None,
        vec![(
            ConductorPurpose::ReplyToChat,
            reply_proposing("Cancel it.", json!({"kind": "abort"})),
        )],
    );
    let (execution, _) = at_the_gate(&fixture).await;
    let (proposal, _) = proposal_of(&fixture.ask(&execution.id, "stop this").await);

    fixture
        .registry
        .confirm_proposal(&fixture.definition, &execution.id, &proposal.id)
        .expect("confirm");
    let now = fixture.load(&execution.id);
    assert_eq!(now.status, ExecutionStatus::Cancelled);
    assert!(!fixture.harness.store().has_pending_input(&execution.id));
    let events = fixture.events(&execution.id);
    assert!(events.iter().any(|data| matches!(
        data,
        EventData::HumanInputReceived {
            close_reason: Some(InputCloseReason::Cancel),
            ..
        }
    )));
    assert!(events
        .iter()
        .any(|data| matches!(data, EventData::WorkflowCancelled { .. })));
}

/// R28: a confirmed retry of a failed step is a recovery of it with the
/// proposal's note, under the failure's key.
#[tokio::test]
async fn a_confirmed_retry_recovers_the_failed_step() {
    let fixture = Fixture::new(
        "missing_script",
        None,
        vec![(
            ConductorPurpose::ReplyToChat,
            reply_proposing(
                "Try it once more.",
                json!({"kind": "retry", "text": "The script is back."}),
            ),
        )],
    );
    let execution = fixture.run(json!({})).await;
    assert_eq!(execution.status, ExecutionStatus::Failed);
    let failure = narsor_engine::engine::step_failure(&execution)
        .expect("read")
        .expect("the failure is recorded");
    // The diagnosis (nothing scripted: it fails) is over before the chat.
    wait_until("the failure was raised as a problem", || {
        fixture
            .events(&execution.id)
            .iter()
            .any(|data| matches!(data, EventData::ProblemRaised { .. }))
    })
    .await;

    let (proposal, record) = proposal_of(&fixture.ask(&execution.id, "retry it").await);
    assert_eq!(record.episode.as_deref(), Some(failure.key()));
    let confirmed = fixture
        .registry
        .confirm_proposal(&fixture.definition, &execution.id, &proposal.id)
        .expect("confirm");
    let finished = confirmed
        .pass
        .expect("the recovery runs")
        .run_to_completion(&fixture.definition)
        .await
        .expect("run");
    assert_eq!(
        finished.status,
        ExecutionStatus::Failed,
        "the script is still missing"
    );

    let events = fixture.events(&execution.id);
    assert_eq!(
        count(&events, |data| matches!(data, EventData::WorkflowResumed)),
        1
    );
    assert_eq!(
        count(&events, |data| matches!(
            data,
            EventData::ActionStarted { .. }
        )),
        2
    );
    assert_eq!(executed_proposals(&events).len(), 1);
}

/// A failed step whose record is gone (a failure from before the feature, a
/// forced terminal), or an execution a restart left `Unknown`, has no
/// failure key; a retry confirmed on it is carried out, not refused as
/// stale.
async fn retry_confirmed_without_a_failure_record(status: ExecutionStatus) {
    let fixture = Fixture::new(
        "missing_script",
        None,
        vec![(
            ConductorPurpose::ReplyToChat,
            reply_proposing(
                "Try it once more.",
                json!({"kind": "retry", "text": "The script is back."}),
            ),
        )],
    );
    let execution = fixture.run(json!({})).await;
    assert_eq!(execution.status, ExecutionStatus::Failed);
    wait_until("the failure was raised as a problem", || {
        fixture
            .events(&execution.id)
            .iter()
            .any(|data| matches!(data, EventData::ProblemRaised { .. }))
    })
    .await;
    fixture.wait_until_idle(&execution.id).await;

    let mut stripped = fixture.load(&execution.id);
    stripped.status = status;
    if let Some(engine) = stripped
        .metadata
        .get_mut("engine")
        .and_then(Value::as_object_mut)
    {
        engine.remove("step_failure");
    }
    fixture
        .harness
        .store()
        .save_execution(&stripped)
        .expect("save the execution");
    assert!(narsor_engine::engine::step_failure(&stripped)
        .expect("read")
        .is_none());

    let (proposal, record) = proposal_of(&fixture.ask(&execution.id, "retry it").await);
    assert!(record.episode.is_some(), "the proposal names its moment");
    let confirmed = fixture
        .registry
        .confirm_proposal(&fixture.definition, &execution.id, &proposal.id)
        .expect("a retry without a failure record is carried out, not refused as stale");
    confirmed
        .pass
        .expect("the recovery runs")
        .run_to_completion(&fixture.definition)
        .await
        .expect("run");

    let events = fixture.events(&execution.id);
    assert_eq!(
        count(&events, |data| matches!(data, EventData::WorkflowResumed)),
        1
    );
    assert_eq!(executed_proposals(&events).len(), 1);
}

#[tokio::test]
async fn a_confirmed_retry_recovers_a_failed_step_without_a_failure_record() {
    retry_confirmed_without_a_failure_record(ExecutionStatus::Failed).await;
}

#[tokio::test]
async fn a_confirmed_retry_recovers_an_unknown_execution() {
    retry_confirmed_without_a_failure_record(ExecutionStatus::Unknown).await;
}

// ---------------------------------------------------------------------------
// What an owner reads of the conductor (U5)
// ---------------------------------------------------------------------------

/// The owner's view of the conductor tells each proposal's fate: waiting,
/// rejected, carried out — and stale, once the execution has moved on from
/// the moment it is about. Only an unsettled proposal of the chat counts as
/// waiting.
#[tokio::test]
async fn the_view_tells_what_became_of_every_proposal() {
    use narsor_engine::conductor::{pending_chat_proposals, ConductorView, ProposalState};

    let fixture = Fixture::new(
        "human_decision",
        None,
        vec![
            propose_answer("ok"),
            propose_answer("rejected"),
            propose_answer("ok"),
        ],
    );
    let (execution, _) = at_the_gate(&fixture).await;
    let first = proposal_of(&fixture.ask(&execution.id, "answer ok?").await).0;
    let second = proposal_of(&fixture.ask(&execution.id, "or reject it?").await).0;
    let third = proposal_of(&fixture.ask(&execution.id, "ok again?").await).0;
    fixture.wait_until_idle(&execution.id).await;
    let store = fixture.harness.store();
    let state_of = |view: &ConductorView, id: &str| {
        view.proposals
            .iter()
            .find(|proposal| proposal.proposal_id == id)
            .map(|proposal| proposal.state)
            .expect("the proposal in the view")
    };

    let view = ConductorView::read(&store, &execution.id, 0, 100).expect("read the view");
    for entry in [&first, &second, &third] {
        assert_eq!(state_of(&view, &entry.id), ProposalState::Pending);
    }
    assert!(view.proposals.iter().all(|p| p.in_reply_to.is_some()));
    assert_eq!(
        pending_chat_proposals(&store, &execution.id)
            .expect("the waiting ones")
            .len(),
        3
    );

    fixture
        .registry
        .reject_proposal(&execution.id, &second.id)
        .expect("reject the second");
    let confirmed = fixture
        .registry
        .confirm_proposal(&fixture.definition, &execution.id, &first.id)
        .expect("confirm the first");
    let finished = confirmed
        .pass
        .expect("answering the gate runs it on")
        .run_to_completion(&fixture.definition)
        .await
        .expect("run on");
    assert_eq!(finished.status, ExecutionStatus::Completed);

    let view = ConductorView::read(&store, &execution.id, 0, 100).expect("read the view");
    assert_eq!(state_of(&view, &first.id), ProposalState::Executed);
    assert_eq!(state_of(&view, &second.id), ProposalState::Rejected);
    assert_eq!(
        state_of(&view, &third.id),
        ProposalState::Stale,
        "the gate it is about was answered"
    );
    assert!(pending_chat_proposals(&store, &execution.id)
        .expect("the waiting ones")
        .is_empty());
}

/// The view pages the chat, and knows which messages wait for an answer
/// without the caller reading the whole history.
#[tokio::test]
async fn the_view_pages_the_chat_and_names_the_unanswered_messages() {
    use narsor_engine::conductor::ConductorView;

    let fixture = Fixture::new("human_decision", None, Vec::new());
    let (execution, _) = at_the_gate(&fixture).await;
    let store = fixture.harness.store();
    let conductor = fixture.conductor_store(&execution.id);
    let mut messages = Vec::new();
    for text in ["one", "two", "three"] {
        messages.push(
            conductor
                .append_chat(ChatEntry::new(
                    ChatAuthor::Operator,
                    ChatEntryKind::Message,
                    text,
                ))
                .expect("append"),
        );
    }

    let first = ConductorView::read(&store, &execution.id, 0, 2).expect("first page");
    let texts: Vec<_> = first.chat.iter().map(|entry| entry.text.as_str()).collect();
    assert_eq!(texts, vec!["one", "two"]);
    assert!(first.chat_has_more);
    let rest =
        ConductorView::read(&store, &execution.id, first.chat[1].sequence, 2).expect("second page");
    assert_eq!(rest.chat.len(), 1);
    assert!(!rest.chat_has_more);
    let ids: Vec<_> = messages.iter().map(|entry| entry.id.clone()).collect();
    assert_eq!(first.unanswered_chat, ids);

    // An execution the conductor never worked on has the empty view.
    let empty = ConductorView::read(&store, "no-such-execution", 0, 10).expect("an empty view");
    assert!(empty.chat.is_empty() && empty.proposals.is_empty() && empty.failure.is_none());
}

/// A decision of the conductor's is shown with whether the owner may still
/// take it back; once taken back, that is shown too.
#[tokio::test]
async fn the_view_tells_whether_a_decision_can_still_be_undone() {
    use narsor_engine::conductor::ConductorView;

    let fixture = Fixture::new(
        "agent_problem",
        Some(Arc::new(LabellessAgentExecutor::answering(
            "Done: three units.",
        ))),
        vec![(
            ConductorPurpose::ChooseOutputAfterReprompt,
            Script::Answer(json!({
                "decision": "choose_output",
                "label": "implemented",
                "data": {"units_completed": 3},
                "variables": {"summary": "three units"},
                "confidence": "high",
                "rationale": "the agent said it finished three units",
            })),
        )],
    );
    let execution = fixture.run(json!({})).await;
    assert_eq!(execution.status, ExecutionStatus::Completed);
    let store = fixture.harness.store();

    let view = ConductorView::read(&store, &execution.id, 0, 100).expect("read the view");
    assert_eq!(view.decisions.len(), 1, "{view:?}");
    let decision = &view.decisions[0];
    assert_eq!(decision.kind, "choose_output");
    assert_eq!(decision.state_id.as_deref(), Some("work"));
    assert_eq!(decision.confidence, Some(Confidence::High));
    assert!(decision.undoable && !decision.undone);

    fixture
        .registry
        .undo_decision(
            &fixture.definition,
            &execution.id,
            &decision.decision_id,
            || {},
        )
        .await
        .expect("undo the decision");
    let view = ConductorView::read(&store, &execution.id, 0, 100).expect("read the view");
    assert!(view.decisions[0].undone && !view.decisions[0].undoable);
}

/// What a terminal must not be handed is stripped from every text of a
/// view; the line breaks of a diagnosis stay.
#[test]
fn a_cleaned_view_keeps_line_breaks_and_loses_what_a_terminal_acts_on() {
    use narsor_engine::conductor::ConductorView;

    let mut view = ConductorView::unavailable("no\u{1b}[2J harness");
    let mut entry = ChatEntry::new(
        ChatAuthor::Conductor,
        ChatEntryKind::Message,
        "one\ntwo\u{202e}\u{7}\tthree\r",
    );
    entry.payload = Some(json!({"diagnosis": "a\u{1b}[31mb\nc", "n": 3}));
    view.chat.push(entry);

    let cleaned = view.cleaned();
    assert_eq!(cleaned.unavailable_reason.as_deref(), Some("no[2J harness"));
    assert_eq!(cleaned.chat[0].text, "one\ntwo three");
    assert_eq!(
        cleaned.chat[0].payload,
        Some(json!({"diagnosis": "a[31mb\nc", "n": 3}))
    );
}
