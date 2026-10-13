//! Integration tests for the conductor at a failed step (F3, U11).
//!
//! The agent runs on the fake harness of `tests/fixtures/harness` (never the
//! real `claude`), scripts run for real, and the conductor is scripted
//! in-process behind the real registry and actor. What is under test is
//! what the failure leaves: the diagnosis and its proposals, the one retry
//! the conductor may take by itself, and what it never does — choose a
//! transition, retry a step that may have acted, or diagnose its own
//! failure.
//!
//! The daemon restart row (`Failed` with the F3 record) is driven through a
//! real daemon, with the fake harness first on its `PATH` as `claude`.

mod engine_support;

use std::collections::VecDeque;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use engine_support::{workflow, Harness};
use narsor_engine::conductor::assembly::assemble;
use narsor_engine::conductor::{
    active_failure_problem, ChatEntry, ChatEntryKind, ConductorAnswer, ConductorInvoker,
    ConductorRegistry, ConductorState, ConductorStore, FailureOutcome, Invocation,
    InvocationOutcome, PassLauncher, PendingInvocation, ProposalKind, QueuedRequest,
};
use narsor_engine::domain::{
    ConductorDecisionKind, ConductorPurpose, ConductorTrigger, EventData, ExecutionStatus,
    ProblemKind, WorkflowDefinition, WorkflowExecution, Workspace,
};
use narsor_engine::engine::{
    step_failure, ActionKind, AgentExecutor, ClaudeCodeAdapter, Engine, EngineError,
    HarnessRegistry, LoggingEventSink, Recovery, StepFailure,
};
use narsor_engine::persistence::{sweep_orphaned_workspaces, DaemonInfo, ExecutionStore};
use narsor_engine::workspace::{CleanupPolicy, WorkspaceProvisioner};
use serde_json::{json, Value};
use tokio::sync::Notify;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// The fake harness CLI, installed in a directory of its own (copied under a
/// temporary name and renamed into place: see `FakeHarness::install` in
/// `tests/actions_agent.rs` for the ETXTBSY reason).
struct FakeHarness {
    dir: PathBuf,
    program: PathBuf,
}

impl FakeHarness {
    fn install(dir: PathBuf, name: &str) -> Self {
        fs::create_dir_all(&dir).expect("create the harness directory");
        let source = engine_support::fixtures().join("harness/fake_harness.sh");
        let program = dir.join(name);
        let temp = dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
        fs::copy(&source, &temp).expect("copy the fake harness");
        fs::rename(&temp, &program).expect("move the fake harness into place");
        Self { dir, program }
    }

    fn program(&self) -> String {
        self.program.to_string_lossy().into_owned()
    }

    /// Every call fails with `code` and `stderr`, printing nothing on stdout
    /// — a harness that dies before it starts a session.
    fn fails(&self, code: i32, stderr: &str) -> &Self {
        fs::write(self.dir.join("exit"), code.to_string()).expect("write the exit code");
        fs::write(self.dir.join("stderr"), stderr).expect("write the stderr");
        self
    }

    /// Every call prints `stdout` first.
    fn prints(&self, stdout: &str) -> &Self {
        fs::write(self.dir.join("stdout"), stdout).expect("write the stdout");
        self
    }

    /// The `nth` call (1-based) prints `stdout` and exits with `code`.
    fn on_call(&self, nth: usize, code: i32, stdout: &str) -> &Self {
        fs::write(self.dir.join(format!("stdout.{nth}")), stdout).expect("write the stdout");
        fs::write(self.dir.join(format!("exit.{nth}")), code.to_string()).expect("write the exit");
        self
    }

    fn calls(&self) -> usize {
        fs::read_to_string(self.dir.join("count"))
            .map(|raw| raw.trim().parse().expect("the count is a number"))
            .unwrap_or(0)
    }

    fn args(&self, nth: usize) -> String {
        fs::read_to_string(self.dir.join(format!("args.{nth}"))).expect("the call's arguments")
    }
}

/// A `stream-json` `result` line carrying `text`.
fn envelope(text: &str) -> String {
    format!(
        "{}\n",
        json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "session_id": "agent-session",
            "result": text,
        })
    )
}

/// One outcome of the scripted conductor.
enum Script {
    /// It answers with this response.
    Answer(Value),
    /// It fails.
    Fail,
    /// It answers with this response once `gate` is opened.
    Gated(Arc<Notify>, Value),
}

/// A conductor that answers from a list, and records every invocation.
#[derive(Default)]
struct ScriptedConductor {
    scripts: Mutex<VecDeque<Script>>,
    calls: Mutex<Vec<Invocation>>,
    started: Notify,
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
        let script = self.scripts.lock().unwrap().pop_front();
        self.started.notify_one();
        Box::pin(async move {
            match script.expect("the conductor was invoked more often than scripted") {
                Script::Answer(response) => answered(response),
                Script::Fail => InvocationOutcome::Failed {
                    error: "the scripted conductor failed".to_string(),
                },
                Script::Gated(gate, response) => {
                    gate.notified().await;
                    answered(response)
                }
            }
        })
    }
}

/// Starts the conductor's passes the way the daemon does, minus the
/// registry of background passes.
struct Passes {
    definition: WorkflowDefinition,
    launched: Mutex<usize>,
}

impl PassLauncher for Passes {
    fn definition(&self, _execution: &WorkflowExecution) -> Result<WorkflowDefinition, String> {
        Ok(self.definition.clone())
    }

    fn launch(&self, _execution_id: &str, pass: Pin<Box<dyn Future<Output = ()> + Send>>) {
        *self.launched.lock().unwrap() += 1;
        tokio::spawn(pass);
    }
}

/// An engine whose agent runs on the fake harness, with the scripted
/// conductor attached.
struct Fixture {
    harness: Harness,
    fake: FakeHarness,
    engine: Arc<Engine>,
    registry: Arc<ConductorRegistry>,
    conductor: Arc<ScriptedConductor>,
    passes: Arc<Passes>,
    definition: WorkflowDefinition,
}

impl Fixture {
    fn new(workflow_name: &str, scripts: Vec<Script>) -> Self {
        let harness = Harness::new();
        let fake = FakeHarness::install(harness.root().join("bin"), "fake_harness.sh");
        let store = harness.store();
        let agent = AgentExecutor::with_harnesses(
            HarnessRegistry::defaults()
                .with_adapter(Arc::new(ClaudeCodeAdapter::with_program(fake.program()))),
        )
        .with_event_sink(Arc::new(LoggingEventSink::new(store.clone(), None)));
        let engine = Arc::new(Engine::new(store).with_executor(ActionKind::Agent, Arc::new(agent)));
        let definition = workflow(workflow_name);
        let conductor = Arc::new(ScriptedConductor::new(scripts));
        let passes = Arc::new(Passes {
            definition: definition.clone(),
            launched: Mutex::new(0),
        });
        let registry = assemble(&engine, conductor.clone(), Some(passes.clone()));
        Self {
            harness,
            fake,
            engine,
            registry,
            conductor,
            passes,
            definition,
        }
    }

    async fn run(&self) -> WorkflowExecution {
        self.engine
            .start_and_run(&self.definition, json!({}), self.harness.workspace())
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

    fn load(&self, execution_id: &str) -> WorkflowExecution {
        self.harness
            .store()
            .load_execution(execution_id)
            .expect("load the execution")
    }

    fn conductor_store(&self, execution_id: &str) -> ConductorStore {
        ConductorStore::new(self.harness.store().paths(), execution_id)
    }

    fn chat(&self, execution_id: &str) -> Vec<ChatEntry> {
        self.conductor_store(execution_id)
            .read_chat()
            .expect("read the chat")
    }

    fn launched(&self) -> usize {
        *self.passes.launched.lock().unwrap()
    }

    /// Waits until the execution's log holds `count` events matching `what`.
    async fn wait_for_events(
        &self,
        execution_id: &str,
        what: &str,
        count: usize,
        matches: impl Fn(&EventData) -> bool,
    ) {
        wait_until(what, || {
            self.events(execution_id)
                .iter()
                .filter(|data| matches(data))
                .count()
                >= count
        })
        .await;
    }

    /// Waits until `count` problems were raised for the execution.
    async fn wait_for_problems(&self, execution_id: &str, count: usize) {
        self.wait_for_events(execution_id, "a problem was raised", count, |data| {
            matches!(data, EventData::ProblemRaised { .. })
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

fn failure_problems(events: &[EventData]) -> Vec<(String, String, Option<String>)> {
    events
        .iter()
        .filter_map(|data| match data {
            EventData::ProblemRaised {
                kind: ProblemKind::Failure,
                action_execution_id,
                reason,
                episode,
                ..
            } => Some((action_execution_id.clone(), reason.clone(), episode.clone())),
            _ => None,
        })
        .collect()
}

fn attempts_started(events: &[EventData]) -> usize {
    count(events, |data| {
        matches!(
            data,
            EventData::ActionStarted { .. } | EventData::ActionRetrying { .. }
        )
    })
}

fn retry_answer(note: &str) -> Value {
    json!({
        "decision": "retry",
        "note": note,
        "diagnosis": "The harness died before it started a session.",
        "confidence": "high",
        "rationale": "Nothing ran, so one more try is safe.",
    })
}

fn diagnosis_with_transition() -> Value {
    json!({
        "decision": "diagnose",
        "diagnosis": "The harness keeps dying before it starts a session: it cannot connect.",
        "rationale": "Two runs failed the same way.",
        "message": "The harness cannot connect; check its credentials, or move on by hand.",
        "proposals": [
            {"kind": "transition", "label": "implemented", "summary": "Mark the step implemented"},
            {"kind": "retry", "text": "The connection is back."}
        ],
    })
}

const HARNESS_DOWN: &str = "error: could not connect to the API";

// ---------------------------------------------------------------------------
// AE8: a failure with no side effects is retried once, then only diagnosed
// ---------------------------------------------------------------------------

/// AE8 (R20–R23): the harness dies before a session, twice, and the policy
/// runs out. The conductor restarts the step once, by itself; the retry is
/// one attempt, not a new round of the policy. When it fails the same way,
/// the conductor only diagnoses, and proposes a transition it does not take.
#[tokio::test]
async fn a_failure_without_side_effects_is_retried_once_then_only_diagnosed() {
    let fixture = Fixture::new(
        "agent_failure",
        vec![
            Script::Answer(retry_answer("The connection may be back; try once more.")),
            Script::Answer(diagnosis_with_transition()),
        ],
    );
    fixture.fake.fails(1, HARNESS_DOWN);

    let execution = fixture.run().await;
    assert_eq!(execution.status, ExecutionStatus::Failed);
    let first = step_failure(&execution)
        .expect("read the record")
        .expect("the failure is recorded for the conductor");
    assert!(first.side_effect_free, "{first:?}");
    assert!(!first.conductor_retry);
    assert_eq!(first.state_id, "work");

    fixture.wait_for_problems(&execution.id, 1).await;
    let events = fixture.events(&execution.id);

    // Two attempts of the policy, one of the conductor's retry.
    assert_eq!(fixture.fake.calls(), 3);
    assert_eq!(attempts_started(&events), 3);
    assert_eq!(fixture.launched(), 1);
    assert!(
        fixture.fake.args(3).contains("try once more"),
        "the retry carries the conductor's note"
    );

    // Two diagnoses: the first may retry, the second may not.
    let calls = fixture.conductor.calls();
    assert_eq!(calls.len(), 2);
    for call in &calls {
        assert_eq!(call.request.trigger, ConductorTrigger::StepFailure);
        assert_eq!(call.request.purpose, ConductorPurpose::DiagnoseFailure);
        assert_eq!(call.attempt, 1);
    }
    assert_eq!(calls[0].request.episode.as_deref(), Some(first.key()));
    assert_eq!(calls[0].request.payload["retry_allowed"], json!(true));
    assert_eq!(calls[1].request.payload["retry_allowed"], json!(false));
    assert_eq!(
        calls[1].request.payload["retried_by_conductor"],
        json!(true)
    );

    // The retry is on record as the conductor's decision.
    let retries: Vec<_> = events
        .iter()
        .filter_map(|data| match data {
            EventData::ConductorDecision {
                kind: ConductorDecisionKind::Retry,
                episode,
                ..
            } => Some(episode.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(retries, vec![Some(first.key().to_string())]);

    // No transition was taken: the execution stays failed, on the retry's
    // failure, in the same episode.
    assert_eq!(
        count(&events, |data| matches!(
            data,
            EventData::TransitionTaken { .. } | EventData::DecisionMade { .. }
        )),
        0
    );
    let stopped = fixture.load(&execution.id);
    assert_eq!(stopped.status, ExecutionStatus::Failed);
    let second = step_failure(&stopped).expect("read").expect("recorded");
    assert_ne!(second.key(), first.key());
    assert_eq!(second.episode, first.episode);
    assert!(second.conductor_retry);

    // The problem names the retry's failure and carries the diagnosis.
    let problems = failure_problems(&events);
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].0, second.key());
    assert_eq!(problems[0].2.as_deref(), Some(second.key()));
    assert!(problems[0].1.contains("cannot connect"), "{problems:?}");

    // The chat: the retry, then the diagnosis and one entry per proposal.
    let chat = fixture.chat(&execution.id);
    let kinds: Vec<ChatEntryKind> = chat.iter().map(|entry| entry.kind).collect();
    assert_eq!(
        kinds,
        vec![
            ChatEntryKind::Decision,
            ChatEntryKind::Message,
            ChatEntryKind::Proposal,
            ChatEntryKind::Proposal,
        ]
    );
    assert_eq!(
        chat[1].text,
        "The harness cannot connect; check its credentials, or move on by hand."
    );
    let proposal = &chat[2].payload.as_ref().expect("a proposal payload");
    assert_eq!(proposal["proposal"]["kind"], "transition");
    assert_eq!(proposal["proposal"]["label"], "implemented");
    assert_eq!(proposal["episode"], second.key());

    // The conductor's state and what a client sees as the problem.
    let state = fixture
        .conductor_store(&execution.id)
        .load_state()
        .expect("load the conductor's state");
    assert!(state.retry_used(&first.episode));
    let report = state.failure.clone().expect("a report");
    assert_eq!(report.key, second.key());
    assert!(matches!(
        report.outcome,
        FailureOutcome::Diagnosed { ref proposals, .. } if proposals.len() == 2
    ));
    assert_eq!(state.pending, None);
    assert!(state.queue.is_empty());
    let problem = active_failure_problem(&fixture.harness.store(), &stopped)
        .expect("the failure is the operator's problem");
    assert_eq!(problem.key, second.key());
}

/// The conductor's retry is one attempt: when it succeeds, the step moves
/// on as if it had never failed.
#[tokio::test]
async fn the_conductors_retry_is_one_attempt_that_can_complete_the_step() {
    let fixture = Fixture::new(
        "agent_failure",
        vec![Script::Answer(retry_answer("Try once more."))],
    );
    fixture
        .fake
        .fails(1, HARNESS_DOWN)
        .on_call(3, 0, &envelope(r#"{"output": "implemented"}"#));

    let execution = fixture.run().await;
    assert_eq!(execution.status, ExecutionStatus::Failed);

    wait_until("the retried execution completed", || {
        fixture.load(&execution.id).status == ExecutionStatus::Completed
    })
    .await;
    let done = fixture.load(&execution.id);
    assert_eq!(fixture.fake.calls(), 3);
    assert_eq!(step_failure(&done).expect("read"), None);
    assert!(
        done.metadata["engine"].get("conductor_retry").is_none(),
        "the retry is over once its transition is taken: {}",
        done.metadata
    );
    let events = fixture.events(&execution.id);
    assert_eq!(failure_problems(&events).len(), 0);
    assert!(events.iter().any(|data| matches!(
        data,
        EventData::TransitionTaken { from_state, output_label, .. }
            if from_state == "work" && output_label == "implemented"
    )));
    assert_eq!(
        active_failure_problem(&fixture.harness.store(), &done),
        None
    );
}

// ---------------------------------------------------------------------------
// No retry after side effects (AE9, KTD14)
// ---------------------------------------------------------------------------

/// AE9 (R21): a script that changed a file and then died is not retried by
/// the conductor, even when it asks to: the retry becomes a proposal.
#[tokio::test]
async fn a_script_that_changed_files_is_not_retried_but_proposed() {
    let fixture = Fixture::new(
        "retry_exhausted",
        vec![Script::Answer(retry_answer("Run the build again."))],
    );

    let execution = fixture.run().await;
    assert_eq!(execution.status, ExecutionStatus::Failed);
    let failure = step_failure(&execution).expect("read").expect("recorded");
    assert!(!failure.side_effect_free, "the script ran: {failure:?}");

    fixture.wait_for_problems(&execution.id, 1).await;
    // Give a wrongly started retry the time to show.
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(
        fs::read_to_string(fixture.harness.workspace_file("attempts.txt")).expect("read"),
        "2",
        "the policy's two attempts, and no third"
    );
    assert_eq!(fixture.launched(), 0);
    let calls = fixture.conductor.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].request.payload["retry_allowed"], json!(false));
    assert_eq!(calls[0].request.payload["side_effect_free"], json!(false));

    let events = fixture.events(&execution.id);
    assert_eq!(
        count(&events, |data| matches!(
            data,
            EventData::ConductorDecision { .. }
        )),
        0
    );
    let chat = fixture.chat(&execution.id);
    let proposal = chat
        .iter()
        .find(|entry| entry.kind == ChatEntryKind::Proposal)
        .expect("the retry is proposed");
    let payload = proposal.payload.as_ref().expect("a payload");
    assert_eq!(payload["proposal"]["kind"], "retry");
    assert_eq!(payload["proposal"]["text"], "Run the build again.");
    assert_eq!(payload["episode"], failure.key());
    assert_eq!(fixture.load(&execution.id).status, ExecutionStatus::Failed);
}

/// KTD14: an agent that failed after its harness reported a session may
/// have done anything in it — no retry by the conductor, only a proposal.
#[tokio::test]
async fn an_agent_that_failed_after_its_session_started_is_not_retried() {
    let fixture = Fixture::new(
        "agent_failure",
        vec![Script::Answer(retry_answer("Try once more."))],
    );
    fixture
        .fake
        .fails(1, "error: the session was cut off")
        .prints(&format!(
            "{}\n",
            json!({"type": "system", "subtype": "init", "session_id": "agent-session-1"})
        ));

    let execution = fixture.run().await;
    let failure = step_failure(&execution).expect("read").expect("recorded");
    assert!(!failure.side_effect_free, "{failure:?}");

    fixture.wait_for_problems(&execution.id, 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fixture.fake.calls(), 2, "no retry beyond the policy");
    assert_eq!(fixture.launched(), 0);
    let report = fixture
        .conductor_store(&execution.id)
        .load_state()
        .expect("load")
        .failure
        .expect("a report");
    match report.outcome {
        FailureOutcome::Diagnosed {
            diagnosis,
            proposals,
        } => {
            assert!(diagnosis.contains("side effects"), "{diagnosis}");
            assert_eq!(proposals[0].kind, ProposalKind::Retry);
        }
        other => panic!("expected a diagnosis, got {other:?}"),
    }
}

/// KTD14: a script whose process never started had no side effect, and
/// the conductor may retry it.
#[tokio::test]
async fn a_script_that_never_started_can_be_retried_by_the_conductor() {
    let fixture = Fixture::new(
        "missing_script",
        vec![
            Script::Answer(retry_answer("Try once more.")),
            Script::Answer(json!({"decision": "diagnose", "diagnosis": "The script is missing."})),
        ],
    );

    let execution = fixture.run().await;
    let failure = step_failure(&execution).expect("read").expect("recorded");
    assert!(failure.side_effect_free, "{failure:?}");

    fixture.wait_for_problems(&execution.id, 1).await;
    assert_eq!(fixture.launched(), 1);
    let events = fixture.events(&execution.id);
    assert_eq!(attempts_started(&events), 3, "two of the policy, one retry");
}

// ---------------------------------------------------------------------------
// Races, conductor failures, R23, R25
// ---------------------------------------------------------------------------

/// KTD9, KTD10: an operator who recovers the execution while the conductor
/// still diagnoses it wins; the conductor's retry is refused and runs
/// nothing a second time.
#[tokio::test]
async fn an_operators_recovery_before_the_conductors_retry_leaves_it_stale() {
    let gate = Arc::new(Notify::new());
    let fixture = Fixture::new(
        "agent_failure",
        vec![Script::Gated(
            Arc::clone(&gate),
            retry_answer("Try once more."),
        )],
    );
    fixture
        .fake
        .fails(1, HARNESS_DOWN)
        .on_call(3, 0, &envelope(r#"{"output": "implemented"}"#));

    let started = fixture.conductor.started.notified();
    let execution = fixture.run().await;
    let failure = step_failure(&execution).expect("read").expect("recorded");
    started.await;

    // The operator recovers first, and the step completes.
    let handle = fixture
        .engine
        .try_begin_recover(&execution.id, None)
        .expect("the operator's recovery");
    let done = handle
        .run_to_completion(&fixture.definition)
        .await
        .expect("run the recovered execution");
    assert_eq!(done.status, ExecutionStatus::Completed);
    assert_eq!(fixture.fake.calls(), 3);

    // Now the conductor answers with its retry.
    gate.notify_one();
    wait_until("the conductor noted the change", || {
        fixture
            .chat(&execution.id)
            .iter()
            .any(|entry| entry.kind == ChatEntryKind::Message && entry.text.contains("changed"))
    })
    .await;

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fixture.fake.calls(), 3, "no second run");
    assert_eq!(fixture.launched(), 0);
    assert_eq!(
        fixture.load(&execution.id).status,
        ExecutionStatus::Completed
    );
    let events = fixture.events(&execution.id);
    assert_eq!(failure_problems(&events).len(), 0);
    assert_eq!(
        count(&events, |data| matches!(data, EventData::Error { .. })),
        0,
        "a stale retry is no engine error"
    );
    // The retry was spent on the episode all the same (KTD14).
    let state = fixture
        .conductor_store(&execution.id)
        .load_state()
        .expect("load");
    assert!(state.retry_used(&failure.episode));
}

/// R24, R25: two failed invocations leave the raw error as the problem,
/// with no analysis — and no further invocation about the conductor's own
/// failure.
#[tokio::test]
async fn two_conductor_failures_raise_the_raw_error() {
    let fixture = Fixture::new("agent_failure", vec![Script::Fail, Script::Fail]);
    fixture.fake.fails(1, HARNESS_DOWN);

    let execution = fixture.run().await;
    fixture.wait_for_problems(&execution.id, 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let calls = fixture.conductor.calls();
    assert_eq!(
        calls.iter().map(|call| call.attempt).collect::<Vec<_>>(),
        vec![1, 2],
        "one repeat, then nothing"
    );
    let events = fixture.events(&execution.id);
    assert_eq!(
        count(&events, |data| matches!(
            data,
            EventData::ConductorStarted { .. }
        )),
        2
    );
    let problems = failure_problems(&events);
    assert_eq!(problems.len(), 1);
    assert!(problems[0].1.contains(HARNESS_DOWN), "{problems:?}");
    assert!(
        problems[0].1.contains("the scripted conductor failed"),
        "{problems:?}"
    );
    assert!(
        fixture.chat(&execution.id).is_empty(),
        "no analysis in the chat"
    );
    let report = fixture
        .conductor_store(&execution.id)
        .load_state()
        .expect("load")
        .failure
        .expect("a report");
    assert!(matches!(report.outcome, FailureOutcome::Unanswered { .. }));
    assert_eq!(fixture.fake.calls(), 2);
}

/// R23: a diagnosis that chooses the step's output — the way on — is
/// refused; the execution stays failed and the operator gets the raw error.
#[tokio::test]
async fn a_diagnosis_that_chooses_a_transition_is_refused() {
    let fixture = Fixture::new(
        "agent_failure",
        vec![Script::Answer(json!({
            "decision": "choose_output",
            "label": "implemented",
            "confidence": "high",
            "rationale": "It is probably done.",
        }))],
    );
    fixture.fake.fails(1, HARNESS_DOWN);

    let execution = fixture.run().await;
    fixture.wait_for_problems(&execution.id, 1).await;

    let events = fixture.events(&execution.id);
    assert_eq!(
        count(&events, |data| matches!(
            data,
            EventData::TransitionTaken { .. }
                | EventData::DecisionMade { .. }
                | EventData::ActionCompleted { .. }
                | EventData::ConductorDecision { .. }
        )),
        0
    );
    let problems = failure_problems(&events);
    assert!(problems[0].1.contains("not allowed"), "{problems:?}");
    let stopped = fixture.load(&execution.id);
    assert_eq!(stopped.status, ExecutionStatus::Failed);
    assert_eq!(stopped.current_state.as_deref(), Some("work"));
    assert_eq!(fixture.launched(), 0);
}

/// A workflow that ends in a terminal state declared as failed did not
/// fail a step: nothing for the conductor.
#[tokio::test]
async fn a_failure_terminal_is_not_a_step_failure() {
    let fixture = Fixture::new("failure_terminal", Vec::new());
    let execution = fixture.run().await;
    assert_eq!(execution.status, ExecutionStatus::Failed);
    assert_eq!(step_failure(&execution).expect("read"), None);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(fixture.conductor.calls().is_empty());
}

/// Without a conductor attached the failure is recorded all the same — it
/// is the key an operator's restart names, and what a task runner reports
/// as a failed step waiting for its owner — and handed to nobody.
#[tokio::test]
async fn without_a_conductor_the_failure_is_recorded_and_keys_a_restart() {
    let harness = Harness::new();
    let engine = Arc::new(harness.engine());
    let execution = engine
        .start_and_run(&workflow("retry_exhausted"), json!({}), harness.workspace())
        .await
        .expect("run");
    assert_eq!(execution.status, ExecutionStatus::Failed);
    let failure = step_failure(&execution)
        .expect("read the record")
        .expect("the failure is recorded");
    assert_eq!(failure.state_id, "build");
    assert!(!failure.conductor_retry);
    assert!(!harness
        .store()
        .paths()
        .conductor_dir(&execution.id)
        .exists());

    // A restart naming another failure is refused, the one naming this is not.
    let stale = engine
        .try_begin_recover_with(
            &execution.id,
            Recovery::by_operator(None, Some("another-failure".to_string())),
        )
        .err()
        .expect("a stale key is refused");
    assert!(
        matches!(stale, EngineError::StaleEpisode { .. }),
        "{stale:?}"
    );
    engine
        .try_begin_recover_with(
            &execution.id,
            Recovery::by_operator(None, Some(failure.key().to_string())),
        )
        .expect("the failure's own key is accepted");
}

// ---------------------------------------------------------------------------
// Restart (the daemon restart matrix)
// ---------------------------------------------------------------------------

/// Leaves `execution` failed on `failure`, as the pass that failed saved it.
fn fail_on(store: &ExecutionStore, execution: &mut WorkflowExecution, failure: &StepFailure) {
    let log = store.event_log(&execution.id);
    for data in [
        EventData::StateEntered {
            state_id: failure.state_id.clone(),
        },
        EventData::ActionStarted {
            action_execution_id: failure.action_execution_id.clone(),
            state_id: failure.state_id.clone(),
            action_type: "script".to_string(),
        },
        EventData::ActionFailed {
            action_execution_id: failure.action_execution_id.clone(),
            error: failure.error.clone(),
        },
        EventData::WorkflowFailed {
            error: failure.error.clone(),
        },
    ] {
        log.append_next(&execution.id, data).expect("append");
    }
    execution.status = ExecutionStatus::Failed;
    execution.current_state = Some(failure.state_id.clone());
    execution.current_action_execution = Some(failure.action_execution_id.clone());
    execution.metadata = json!({ "engine": { "step_failure": failure } });
    store.save_execution(execution).expect("save the execution");
}

fn a_failure(key: &str) -> StepFailure {
    StepFailure {
        state_id: "first".to_string(),
        action_execution_id: key.to_string(),
        episode: key.to_string(),
        error: "the build broke".to_string(),
        side_effect_free: false,
        conductor_retry: false,
    }
}

/// The marker of a diagnosis of `key` a restart cut short, counted once.
fn cut_short(conductor: &ConductorStore, key: &str) {
    let request = QueuedRequest::new(
        ConductorTrigger::StepFailure,
        ConductorPurpose::DiagnoseFailure,
    )
    .with_episode(key);
    let mut state = ConductorState {
        pending: Some(PendingInvocation {
            invocation_id: "cut-short".to_string(),
            request: request.clone(),
            started_at: chrono::Utc::now(),
        }),
        ..ConductorState::default()
    };
    state.queue.push(request);
    state.invocation_attempts.insert(key.to_string(), 1);
    conductor.save_state(&state).expect("seed the marker");
}

/// Restart matrix, in process: `Failed` with the F3 record and a diagnosis
/// cut short is diagnosed again — R24's one repeat — and what the cut left
/// behind is dropped.
#[tokio::test]
async fn a_diagnosis_a_restart_cut_short_runs_again_once() {
    let fixture = Fixture::new(
        "two_steps",
        vec![Script::Answer(
            json!({"decision": "diagnose", "diagnosis": "The build broke."}),
        )],
    );
    let store = fixture.harness.store();
    let mut execution = fixture
        .engine
        .start(&fixture.definition, json!({}), fixture.harness.workspace())
        .expect("start");
    fail_on(&store, &mut execution, &a_failure("attempt-1"));
    cut_short(&fixture.conductor_store(&execution.id), "attempt-1");

    let diagnosed = fixture
        .registry
        .restore_failures_at_startup()
        .expect("restore");
    assert_eq!(diagnosed, vec![execution.id.clone()]);
    fixture.wait_for_problems(&execution.id, 1).await;

    let calls = fixture.conductor.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].attempt, 2, "the repeat R24 allows");
    let state = fixture
        .conductor_store(&execution.id)
        .load_state()
        .expect("load");
    assert_eq!(state.pending, None);
    assert!(state.queue.is_empty(), "{state:?}");
    assert!(state.failure.is_some());

    // Concluded: a second start diagnoses nothing.
    let again = fixture
        .registry
        .restore_failures_at_startup()
        .expect("restore again");
    assert!(again.is_empty());
}

/// Restart matrix: a diagnosis whose repeat was spent already is not run
/// again — the operator gets the raw error.
#[tokio::test]
async fn a_diagnosis_with_its_repeat_spent_raises_the_raw_error_at_startup() {
    let fixture = Fixture::new("two_steps", Vec::new());
    let store = fixture.harness.store();
    let mut execution = fixture
        .engine
        .start(&fixture.definition, json!({}), fixture.harness.workspace())
        .expect("start");
    fail_on(&store, &mut execution, &a_failure("attempt-1"));
    let conductor = fixture.conductor_store(&execution.id);
    cut_short(&conductor, "attempt-1");
    let mut state = conductor.load_state().expect("load");
    state.invocation_attempts.insert("attempt-1".to_string(), 2);
    conductor.save_state(&state).expect("save");

    fixture
        .registry
        .restore_failures_at_startup()
        .expect("restore");
    fixture.wait_for_problems(&execution.id, 1).await;

    assert!(fixture.conductor.calls().is_empty());
    let problems = failure_problems(&fixture.events(&execution.id));
    assert!(problems[0].1.contains("the build broke"), "{problems:?}");
}

/// Restart matrix, through a real daemon: `Failed` with the F3 record and a
/// diagnosis cut short. The startup hook diagnoses it once (on the fake
/// harness, first on the daemon's `PATH` as `claude`), the problem shows in
/// `GET /executions`, and the workspace sweep keeps the task's workspace.
#[tokio::test]
#[ignore = "starts narsor-server; run with --ignored"]
async fn a_fresh_daemon_diagnoses_a_failed_step_again_and_the_sweep_keeps_its_workspace() {
    let harness = Harness::new();
    let root = harness.root().join("daemon");
    fs::create_dir_all(root.join("work")).expect("create the work dir");
    let bin = root.join("bin");
    let fake = FakeHarness::install(bin.clone(), "claude");
    let answer = json!({
        "decision": "diagnose",
        "diagnosis": "The build broke on a missing dependency.",
        "proposals": [{"kind": "retry", "text": "Install the dependency first."}],
    });
    fake.prints(&format!(
        "{}\n",
        json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "session_id": "conductor-session",
            "result": answer.to_string(),
        })
    ));

    let store = ExecutionStore::new(&root);
    let engine = Engine::new(store.clone());
    let mut execution = engine
        .start(
            &workflow("two_steps"),
            json!({}),
            Workspace::local_folder(root.join("work").display().to_string()),
        )
        .expect("seed the execution");
    fail_on(&store, &mut execution, &a_failure("attempt-1"));
    let conductor = ConductorStore::new(store.paths(), &execution.id);
    cut_short(&conductor, "attempt-1");

    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = Command::new(engine_support::workspace_binary("narsor-server"))
        .arg("--root")
        .arg(&root)
        .env("PATH", path)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn the daemon");
    let info = wait_for_daemon_info(&root).await;
    let client = narsor::api::ApiClient::new(info.addr, &info.token);

    let log = store.event_log(&execution.id);
    let problem_raised = || {
        log.read_all()
            .expect("read the log")
            .iter()
            .any(|event| matches!(event.data, EventData::ProblemRaised { .. }))
    };
    wait_until("the daemon diagnosed the failure", problem_raised).await;

    let events: Vec<EventData> = log
        .read_all()
        .expect("read the log")
        .into_iter()
        .map(|event| event.data)
        .collect();
    assert_eq!(
        count(&events, |data| matches!(
            data,
            EventData::ConductorStarted {
                trigger: ConductorTrigger::StepFailure,
                ..
            }
        )),
        1,
        "the hook diagnoses once"
    );
    assert_eq!(fake.calls(), 1);
    let state = conductor.load_state().expect("load");
    assert_eq!(state.attempts("attempt-1"), 2);
    assert_eq!(state.pending, None);
    assert!(state.queue.is_empty());

    let list = client.list_executions().await.expect("list over the API");
    let summary = list
        .executions
        .iter()
        .find(|summary| summary.id == execution.id)
        .expect("the execution is listed");
    assert_eq!(summary.status, narsor::api::ExecutionStatusDto::Failed);
    let problem = summary.problem.clone().expect("a problem");
    assert_eq!(problem.kind, narsor::api::ProblemKindDto::Failure);
    assert_eq!(problem.episode.as_deref(), Some("attempt-1"));
    assert_eq!(problem.reason, "The build broke on a missing dependency.");

    stop(&mut child, &root);

    // The sweep keeps the workspace of a failure the conductor took up.
    let provisioner = WorkspaceProvisioner::new(root.join("work"));
    let task_dir = provisioner.task_dir(&execution.id);
    fs::create_dir_all(&task_dir).expect("create the task's workspace");
    let orphans =
        sweep_orphaned_workspaces(&store, &provisioner, CleanupPolicy::Remove).expect("sweep");
    assert!(
        orphans
            .iter()
            .all(|orphan| orphan.execution_id != execution.id),
        "{orphans:?}"
    );
    assert!(task_dir.exists());
}

async fn wait_for_daemon_info(root: &Path) -> DaemonInfo {
    let file = narsor_engine::persistence::StatePaths::new(root).daemon_file();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(info) = DaemonInfo::read(&file) {
            return info;
        }
        if Instant::now() >= deadline {
            panic!("the daemon never became discoverable");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn stop(child: &mut std::process::Child, root: &Path) {
    // SAFETY: a plain signal to the child this test spawned.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait().expect("poll the daemon").is_some() {
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("the daemon at {} did not stop", root.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
