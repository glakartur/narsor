//! Integration tests for the invocation of the conductor (U9, KTD5, KTD6).
//!
//! The conductor runs on the fake harness of `tests/fixtures/harness`, so
//! what is under test is everything narsor does around a real process: the
//! harness chosen at each invocation, the read-only arguments and the
//! working directory, the snapshot and the prompt, the checking of the
//! answer, and the one repeat of R24 the actor adds on top.

mod engine_support;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use engine_support::{fixtures, workflow, Harness};
use narsor_engine::conductor::snapshot::{render, ConversationTail, SnapshotBudget, SnapshotInput};
use narsor_engine::conductor::{
    prompt::conductor_prompt, ChatAuthor, ChatEntry, ChatEntryKind, ConductorAccess,
    ConductorRegistry, ConductorResponse, ConductorStore, Decision, DecisionKind, HarnessInvoker,
    Invocation, InvocationOutcome, LiveHarness, QueuedRequest, ResponseError,
};
use narsor_engine::domain::{
    ArtifactReference, ConductorOutcome, ConductorPurpose, ConductorTrigger, Confidence, Event,
    EventData, ChoiceDefinition,
};
use narsor_engine::engine::actions::harness::{
    ClaudeCodeAdapter, CodexAdapter, HarnessRegistry, OpenCodeAdapter,
};
use narsor_engine::engine::{DefaultHarnessSource, Engine};
use narsor_engine::persistence::DefaultHarnessSelector;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// The fake harness CLI, installed in a directory of its own (see
/// `tests/actions_agent.rs` for why it is renamed into place).
struct FakeHarness {
    dir: PathBuf,
    program: PathBuf,
}

impl FakeHarness {
    fn install(dir: PathBuf) -> Self {
        fs::create_dir_all(&dir).expect("create the harness directory");
        let source = fixtures().join("harness/fake_harness.sh");
        let program = dir.join("fake_harness.sh");
        let temp = dir.join(format!(".fake_harness.sh.{}.tmp", uuid::Uuid::new_v4()));
        fs::copy(&source, &temp).expect("copy the fake harness");
        fs::rename(&temp, &program).expect("move the fake harness into place");
        Self { dir, program }
    }

    fn program(&self) -> String {
        self.program.to_string_lossy().into_owned()
    }

    fn answers(&self, stdout: &str) -> &Self {
        fs::write(self.dir.join("stdout"), stdout).expect("write the answer");
        self
    }

    fn answers_on_call(&self, nth: usize, stdout: &str) -> &Self {
        fs::write(self.dir.join(format!("stdout.{nth}")), stdout).expect("write the answer");
        self
    }

    fn fails(&self, code: i32, stderr: &str) -> &Self {
        fs::write(self.dir.join("exit"), code.to_string()).expect("write the exit code");
        fs::write(self.dir.join("stderr"), stderr).expect("write the stderr");
        self
    }

    fn fails_on_call(&self, nth: usize, code: i32, stderr: &str) -> &Self {
        fs::write(self.dir.join(format!("exit.{nth}")), code.to_string())
            .expect("write the exit code");
        fs::write(self.dir.join(format!("stderr.{nth}")), stderr).expect("write the stderr");
        self
    }

    fn sleeps(&self, seconds: u64) -> &Self {
        fs::write(self.dir.join("sleep"), seconds.to_string()).expect("write the sleep");
        self
    }

    fn args(&self, nth: usize) -> Vec<String> {
        let raw = fs::read_to_string(self.dir.join(format!("args.{nth}")))
            .expect("the harness recorded its arguments");
        raw.split("\n--%ARG%--\n")
            .filter(|arg| !arg.is_empty())
            .map(str::to_string)
            .collect()
    }

    fn cwd(&self, nth: usize) -> PathBuf {
        PathBuf::from(read_line(&self.dir.join(format!("cwd.{nth}"))))
    }

    fn action_id(&self, nth: usize) -> String {
        read_line(&self.dir.join(format!("action_id.{nth}")))
    }

    fn calls(&self) -> usize {
        fs::read_to_string(self.dir.join("count"))
            .map(|raw| raw.trim().parse().expect("the count is a number"))
            .unwrap_or(0)
    }
}

fn read_line(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .trim_end()
        .to_string()
}

/// A Claude Code `result` line carrying `text`.
fn claude_answer(text: &str) -> String {
    format!(
        "{}\n",
        json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "session_id": "conductor-session",
            "result": text,
        })
    )
}

/// An opencode stream whose final text is `text`.
fn opencode_answer(text: &str) -> String {
    format!(
        "{}\n{}\n",
        json!({ "type": "text", "sessionID": "conductor-session", "text": text }),
        json!({ "type": "step_finish", "sessionID": "conductor-session" }),
    )
}

/// A codex stream whose final message is `text`.
fn codex_answer(text: &str) -> String {
    format!(
        "{}\n{}\n{}\n",
        json!({ "type": "thread.started", "thread_id": "conductor-thread" }),
        json!({ "type": "item.completed", "item": { "type": "agent_message", "text": text } }),
        json!({ "type": "turn.completed" }),
    )
}

const CHOOSE_DONE: &str = r#"The agent ran the step to the end; its last message says the work is done.
{"decision": "choose_output", "label": "done", "artifacts": {"report": "docs/report.md"}, "variables": {"ok": true},
 "confidence": "high", "rationale": "The conversation ends with the work finished."}"#;

/// A started execution of `two_steps`, with its definition pinned, and the
/// fakes of all three harnesses.
struct Fixture {
    harness: Harness,
    engine: Arc<Engine>,
    execution_id: String,
    claude: FakeHarness,
    opencode: FakeHarness,
    codex: FakeHarness,
}

impl Fixture {
    fn new() -> Self {
        let harness = Harness::new();
        let engine = Arc::new(harness.engine());
        let execution = engine
            .start(&workflow("two_steps"), json!({}), harness.workspace())
            .expect("start an execution");
        let yaml = fs::read_to_string(fixtures().join("engine/two_steps.yaml"))
            .expect("read the definition");
        fs::write(harness.store().paths().definition_file(&execution.id), yaml)
            .expect("pin the definition");

        let claude = FakeHarness::install(harness.root().join("claude"));
        let opencode = FakeHarness::install(harness.root().join("opencode"));
        let codex = FakeHarness::install(harness.root().join("codex"));
        Self {
            harness,
            engine,
            execution_id: execution.id,
            claude,
            opencode,
            codex,
        }
    }

    fn registry(&self) -> HarnessRegistry {
        HarnessRegistry::defaults()
            .with_adapter(Arc::new(ClaudeCodeAdapter::with_program(
                self.claude.program(),
            )))
            .with_adapter(Arc::new(OpenCodeAdapter::with_program(
                self.opencode.program(),
            )))
            .with_adapter(Arc::new(CodexAdapter::with_program(self.codex.program())))
    }

    fn work_dir(&self) -> PathBuf {
        self.harness.root().join("work")
    }

    /// An invoker on `harness` over the fakes.
    fn invoker(&self, harness: &str) -> HarnessInvoker {
        HarnessInvoker::new(
            self.harness.store(),
            self.harness.root().join("catalog"),
            self.work_dir(),
        )
        .with_harnesses(self.registry())
        .with_default_harness_source(Arc::new(harness.to_string()))
    }

    fn invocation(&self, trigger: ConductorTrigger, purpose: ConductorPurpose) -> Invocation {
        Invocation {
            invocation_id: "invocation-1".to_string(),
            execution_id: self.execution_id.clone(),
            request: QueuedRequest::new(trigger, purpose).with_episode("episode-1"),
            chat_message: None,
            attempt: 1,
        }
    }

    fn rescue(&self) -> Invocation {
        self.invocation(
            ConductorTrigger::OutputRescue,
            ConductorPurpose::ChooseOutput,
        )
    }

    fn events(&self) -> Vec<EventData> {
        self.harness
            .events(&self.execution_id)
            .into_iter()
            .map(|event| event.data)
            .collect()
    }

    fn conductor_store(&self) -> ConductorStore {
        ConductorStore::new(self.harness.store().paths(), &self.execution_id)
    }
}

fn answered(outcome: InvocationOutcome, purpose: ConductorPurpose) -> ConductorResponse {
    match outcome {
        InvocationOutcome::Answered(answer) => {
            ConductorResponse::from_value(answer.response, purpose).expect("a checked response")
        }
        other => panic!("expected an answer, got {other:?}"),
    }
}

fn failed(outcome: InvocationOutcome) -> String {
    match outcome {
        InvocationOutcome::Failed { error } => error,
        other => panic!("expected a failure, got {other:?}"),
    }
}

fn finished_outcomes(events: &[EventData]) -> Vec<ConductorOutcome> {
    events
        .iter()
        .filter_map(|event| match event {
            EventData::ConductorFinished { outcome, .. } => Some(*outcome),
            _ => None,
        })
        .collect()
}

fn started_count(events: &[EventData]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, EventData::ConductorStarted { .. }))
        .count()
}

// ---------------------------------------------------------------------------
// One invocation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_valid_answer_parses_to_a_decision_with_high_confidence_and_a_rationale() {
    let fixture = Fixture::new();
    fixture.claude.answers(&claude_answer(CHOOSE_DONE));

    let outcome = fixture
        .invoker("claude-code")
        .invoke_once(&fixture.rescue())
        .await;
    let response = answered(outcome, ConductorPurpose::ChooseOutput);

    assert_eq!(
        response.decision,
        Decision::ChooseOutput {
            label: "done".to_string(),
            artifacts: [("report".to_string(), "docs/report.md".to_string())].into(),
            variables: json!({"ok": true}).as_object().cloned().unwrap(),
        }
    );
    assert_eq!(response.confidence, Confidence::High);
    assert_eq!(
        response.rationale.as_deref(),
        Some("The conversation ends with the work finished.")
    );
}

#[tokio::test]
async fn claude_runs_read_only_in_the_workspace_without_an_action_attempt() {
    let fixture = Fixture::new();
    fixture.claude.answers(&claude_answer(CHOOSE_DONE));
    let before = fixture.events().len();

    fixture
        .invoker("claude-code")
        .invoke_once(&fixture.rescue())
        .await;

    let args = fixture.claude.args(1);
    let mode = args
        .iter()
        .position(|arg| arg == "--permission-mode")
        .expect("a permission mode");
    assert_eq!(args[mode + 1], "plan");
    assert_eq!(
        fixture.claude.cwd(1).canonicalize().unwrap(),
        fixture.harness.workspace_dir().canonicalize().unwrap()
    );
    assert_eq!(fixture.claude.action_id(1), "", "no action attempt id");
    assert_eq!(
        fixture.events().len(),
        before,
        "the invoker itself records nothing: no ActionStarted, no session"
    );
    let recorded = fixture
        .harness
        .store()
        .conversation_store(&fixture.execution_id)
        .recorded_attempt_ids()
        .expect("list the conversations");
    assert!(
        recorded.is_empty(),
        "no conversation is written: {recorded:?}"
    );
    assert!(!fixture.work_dir().exists(), "no scratch directory is made");
}

#[tokio::test]
async fn codex_runs_in_its_read_only_sandbox() {
    let fixture = Fixture::new();
    fixture.codex.answers(&codex_answer(CHOOSE_DONE));

    let outcome = fixture
        .invoker("codex")
        .invoke_once(&fixture.rescue())
        .await;
    answered(outcome, ConductorPurpose::ChooseOutput);

    let args = fixture.codex.args(1);
    let sandbox = args
        .iter()
        .position(|arg| arg == "--sandbox")
        .expect("a sandbox");
    assert_eq!(args[sandbox + 1], "read-only");
}

#[tokio::test]
async fn opencode_runs_in_an_empty_scratch_directory_and_is_shown_the_artifacts() {
    let fixture = Fixture::new();
    fixture.opencode.answers(&opencode_answer(CHOOSE_DONE));

    // An artifact of the workflow, in the workspace the conductor cannot see.
    fs::write(
        fixture.harness.workspace_dir().join("plan.md"),
        "# The plan\n\nUnit 1: write the parser.\n",
    )
    .expect("write the plan");
    let store = fixture.harness.store();
    let mut execution = store
        .load_execution(&fixture.execution_id)
        .expect("load the execution");
    execution.context.artifacts.insert(
        "plan".to_string(),
        ArtifactReference {
            id: "plan".to_string(),
            name: Some("plan".to_string()),
            location: "plan.md".to_string(),
            metadata: Value::Null,
        },
    );
    store
        .save_execution(&execution)
        .expect("save the execution");

    // Whatever an earlier invocation left in the scratch directory is gone.
    let invoker = fixture.invoker("opencode");
    let scratch = invoker.scratch_dir(&fixture.execution_id);
    fs::create_dir_all(&scratch).expect("create the scratch directory");
    fs::write(scratch.join("leftover.txt"), "old").expect("write a leftover");

    let outcome = invoker.invoke_once(&fixture.rescue()).await;
    answered(outcome, ConductorPurpose::ChooseOutput);

    assert_eq!(
        fixture.opencode.cwd(1).canonicalize().unwrap(),
        scratch.canonicalize().unwrap()
    );
    assert_eq!(
        scratch,
        fixture
            .work_dir()
            .join("tasks")
            .join(&fixture.execution_id)
            .join("conductor/scratch")
    );
    assert!(!scratch.join("leftover.txt").exists());

    let args = fixture.opencode.args(1);
    assert!(!args.contains(&"--auto".to_string()), "never auto-approved");
    let prompt = args.last().expect("the prompt");
    assert!(prompt.contains("Unit 1: write the parser."), "{prompt}");
    assert!(prompt.contains("empty scratch directory"));
}

#[tokio::test]
async fn a_sandboxed_conductor_is_not_shown_the_artifact_files() {
    let fixture = Fixture::new();
    fixture.claude.answers(&claude_answer(CHOOSE_DONE));
    fs::write(
        fixture.harness.workspace_dir().join("plan.md"),
        "Unit 1: write the parser.",
    )
    .expect("write the plan");
    let store = fixture.harness.store();
    let mut execution = store.load_execution(&fixture.execution_id).unwrap();
    execution.context.artifacts.insert(
        "plan".to_string(),
        ArtifactReference {
            id: "plan".to_string(),
            name: None,
            location: "plan.md".to_string(),
            metadata: Value::Null,
        },
    );
    store.save_execution(&execution).unwrap();

    fixture
        .invoker("claude-code")
        .invoke_once(&fixture.rescue())
        .await;

    let prompt = fixture.claude.args(1).last().cloned().unwrap();
    assert!(!prompt.contains("Unit 1: write the parser."));
    assert!(prompt.contains("opened read-only"));
    assert!(prompt.contains("name: two-steps"), "the pinned definition");
}

#[tokio::test]
async fn the_harness_is_taken_from_the_selector_at_every_invocation() {
    let fixture = Fixture::new();
    fixture.claude.answers(&claude_answer(CHOOSE_DONE));
    fixture.opencode.answers(&opencode_answer(CHOOSE_DONE));
    let selector = Arc::new(
        DefaultHarnessSelector::load(
            fixture.harness.store().paths().clone(),
            vec![
                "claude-code".to_string(),
                "codex".to_string(),
                "opencode".to_string(),
            ],
            "claude-code",
        )
        .await
        .expect("load the selector"),
    );
    let invoker = fixture
        .invoker("codex")
        .with_default_harness_source(selector.clone());

    invoker.invoke_once(&fixture.rescue()).await;
    selector.set("opencode").await.expect("switch the default");
    invoker.invoke_once(&fixture.rescue()).await;

    assert_eq!(fixture.claude.calls(), 1);
    assert_eq!(fixture.opencode.calls(), 1);
    assert_eq!(fixture.codex.calls(), 0);
}

/// A task runner has no selector: its conductor runs on the live harness
/// the control plane last handed it (U7), read at every invocation, and the
/// same handle answers the Engine's default for harnessless actions.
#[tokio::test]
async fn a_runners_live_harness_is_read_at_every_invocation() {
    let fixture = Fixture::new();
    fixture.claude.answers(&claude_answer(CHOOSE_DONE));
    fixture.opencode.answers(&opencode_answer(CHOOSE_DONE));
    let live = Arc::new(LiveHarness::new("claude-code"));
    let invoker = fixture
        .invoker("codex")
        .with_default_harness_source(live.clone());

    invoker.invoke_once(&fixture.rescue()).await;
    live.set("opencode");
    assert_eq!(live.current_harness(), "opencode");
    invoker.invoke_once(&fixture.rescue()).await;

    assert_eq!(fixture.claude.calls(), 1);
    assert_eq!(fixture.opencode.calls(), 1);
    assert_eq!(fixture.codex.calls(), 0);
}

#[tokio::test]
async fn an_answer_without_json_fails() {
    let fixture = Fixture::new();
    fixture
        .claude
        .answers(&claude_answer("I think the agent is done."));

    let error = failed(
        fixture
            .invoker("claude-code")
            .invoke_once(&fixture.rescue())
            .await,
    );
    assert!(error.contains("no JSON object"), "{error}");
}

#[tokio::test]
async fn an_answer_off_the_schema_fails() {
    let fixture = Fixture::new();
    fixture.claude.answers(&claude_answer(
        r#"{"decision": "choose_output", "confidence": "high"}"#,
    ));

    let error = failed(
        fixture
            .invoker("claude-code")
            .invoke_once(&fixture.rescue())
            .await,
    );
    assert!(error.contains("label"), "{error}");
}

#[tokio::test]
async fn a_decision_the_purpose_does_not_allow_fails() {
    let fixture = Fixture::new();
    // A diagnosis may never choose the way on (R23).
    fixture.claude.answers(&claude_answer(CHOOSE_DONE));
    let invocation = fixture.invocation(
        ConductorTrigger::StepFailure,
        ConductorPurpose::DiagnoseFailure,
    );

    let error = failed(
        fixture
            .invoker("claude-code")
            .invoke_once(&invocation)
            .await,
    );
    assert!(error.contains("not allowed"), "{error}");
    assert!(error.contains("diagnose_failure"), "{error}");
}

#[tokio::test]
async fn a_process_error_and_a_timeout_fail() {
    let fixture = Fixture::new();
    fixture.claude.fails(1, "authentication failed");
    let error = failed(
        fixture
            .invoker("claude-code")
            .invoke_once(&fixture.rescue())
            .await,
    );
    assert!(error.contains("authentication failed"), "{error}");

    let fixture = Fixture::new();
    fixture
        .claude
        .answers(&claude_answer(CHOOSE_DONE))
        .sleeps(5);
    let error = failed(
        fixture
            .invoker("claude-code")
            .with_timeout(Duration::from_millis(300))
            .invoke_once(&fixture.rescue())
            .await,
    );
    assert!(error.contains("still running"), "{error}");
}

// ---------------------------------------------------------------------------
// The one repeat (R24), through the actor
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_failed_automatic_invocation_is_repeated_once_and_the_repeat_answers() {
    let fixture = Fixture::new();
    fixture
        .claude
        .fails_on_call(1, 1, "authentication failed")
        .answers_on_call(2, &claude_answer(CHOOSE_DONE));
    let registry = ConductorRegistry::new(
        Arc::clone(&fixture.engine),
        Arc::new(fixture.invoker("claude-code")),
    );
    let actor = registry.actor(&fixture.execution_id).expect("the actor");

    let outcome = actor
        .invoke(
            QueuedRequest::new(
                ConductorTrigger::OutputRescue,
                ConductorPurpose::ChooseOutput,
            )
            .with_episode("episode-1"),
        )
        .await
        .expect("invoke");

    let response = answered(outcome, ConductorPurpose::ChooseOutput);
    assert_eq!(response.kind(), DecisionKind::ChooseOutput);
    assert_eq!(fixture.claude.calls(), 2, "one repeat");

    let events = fixture.events();
    assert_eq!(started_count(&events), 2);
    assert_eq!(
        finished_outcomes(&events),
        vec![ConductorOutcome::Failed, ConductorOutcome::Completed]
    );
    let state = actor.state();
    assert_eq!(state.attempts("episode-1"), 2, "both tries were counted");
    assert!(state.pending.is_none());
    assert!(state.queue.is_empty());
}

#[tokio::test]
async fn two_failures_are_a_conductor_failure_and_raise_no_further_invocation() {
    let fixture = Fixture::new();
    fixture.claude.fails(1, "authentication failed");
    let registry = ConductorRegistry::new(
        Arc::clone(&fixture.engine),
        Arc::new(fixture.invoker("claude-code")),
    );
    let actor = registry.actor(&fixture.execution_id).expect("the actor");

    let outcome = actor
        .invoke(
            QueuedRequest::new(
                ConductorTrigger::StepFailure,
                ConductorPurpose::DiagnoseFailure,
            )
            .with_episode("episode-1"),
        )
        .await
        .expect("invoke");
    let error = failed(outcome);
    assert!(error.contains("authentication failed"), "{error}");

    // Nothing is queued in its wake (R25).
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fixture.claude.calls(), 2);
    let events = fixture.events();
    assert_eq!(started_count(&events), 2);
    assert_eq!(
        finished_outcomes(&events),
        vec![ConductorOutcome::Failed, ConductorOutcome::Failed]
    );
    assert_eq!(actor.queued(), 0);
    let state = actor.state();
    assert!(state.queue.is_empty());
    assert!(state.pending.is_none());
}

#[tokio::test]
async fn an_episode_whose_repeat_was_already_counted_is_not_repeated_again() {
    let fixture = Fixture::new();
    fixture.claude.fails(1, "authentication failed");
    let registry = ConductorRegistry::new(
        Arc::clone(&fixture.engine),
        Arc::new(fixture.invoker("claude-code")),
    );
    let actor = registry.actor(&fixture.execution_id).expect("the actor");
    // A restart cut the first try short: it was counted, it did not end.
    actor
        .update_state(|state| {
            state.invocation_attempts.insert("episode-1".to_string(), 1);
        })
        .expect("count the first try");

    let outcome = actor
        .invoke(
            QueuedRequest::new(
                ConductorTrigger::OutputRescue,
                ConductorPurpose::ChooseOutput,
            )
            .with_episode("episode-1"),
        )
        .await
        .expect("invoke");

    failed(outcome);
    assert_eq!(fixture.claude.calls(), 1, "the repeat was this try");
    assert_eq!(actor.state().attempts("episode-1"), 2);
}

#[tokio::test]
async fn a_failed_chat_reply_is_one_error_entry_and_no_repeat() {
    let fixture = Fixture::new();
    fixture.claude.fails(1, "authentication failed");
    let registry = ConductorRegistry::new(
        Arc::clone(&fixture.engine),
        Arc::new(fixture.invoker("claude-code")),
    );
    let actor = registry.actor(&fixture.execution_id).expect("the actor");

    let message = actor.post_chat("what is going on?").expect("post");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let reply = loop {
        let chat = fixture
            .conductor_store()
            .read_chat()
            .expect("read the chat");
        if let Some(reply) = chat
            .iter()
            .find(|entry| entry.in_reply_to.as_deref() == Some(message.id.as_str()))
        {
            break reply.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the chat was never answered"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    assert_eq!(reply.kind, ChatEntryKind::Error);
    assert!(reply.text.contains("authentication failed"));
    assert_eq!(fixture.claude.calls(), 1, "KTD12: no repeat for a chat");
}

#[tokio::test]
async fn a_chat_reply_carries_the_conductors_message() {
    let fixture = Fixture::new();
    fixture.claude.answers(&claude_answer(
        r#"{"decision": "reply", "message": "The first step is running.",
            "proposals": [{"kind": "abort", "summary": "Stop it"}]}"#,
    ));
    let registry = ConductorRegistry::new(
        Arc::clone(&fixture.engine),
        Arc::new(fixture.invoker("claude-code")),
    );
    let actor = registry.actor(&fixture.execution_id).expect("the actor");

    let message = actor.post_chat("status?").expect("post");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let chat = fixture
            .conductor_store()
            .read_chat()
            .expect("read the chat");
        if let Some(reply) = chat
            .iter()
            .find(|entry| entry.in_reply_to.as_deref() == Some(message.id.as_str()))
        {
            assert_eq!(reply.kind, ChatEntryKind::Message);
            assert_eq!(reply.text, "The first step is running.");
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let prompt = fixture.claude.args(1).last().cloned().unwrap();
    assert!(prompt.contains("status?"), "the message answered is shown");
}

// ---------------------------------------------------------------------------
// The answer as a JSON Schema (U5)
// ---------------------------------------------------------------------------

/// A Claude Code `result` line with `structured_output`, as
/// `claude --json-schema` prints it.
fn claude_structured_answer(text: &str, structured: Value) -> String {
    format!(
        "{}\n",
        json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "session_id": "conductor-session",
            "result": text,
            "structured_output": structured,
        })
    )
}

/// The schema passed with `--json-schema`, and where the flag stands.
fn json_schema_arg(args: &[String]) -> (usize, Value) {
    let flag = args
        .iter()
        .position(|arg| arg == "--json-schema")
        .unwrap_or_else(|| panic!("no --json-schema in {args:?}"));
    let schema = serde_json::from_str(&args[flag + 1]).expect("the schema is JSON");
    (flag, schema)
}

#[tokio::test]
async fn a_claude_conductor_gets_the_schema_of_its_purpose() {
    let fixture = Fixture::new();
    fixture.claude.answers(&claude_answer(CHOOSE_DONE));
    fixture
        .invoker("claude-code")
        .invoke_once(&fixture.rescue())
        .await;
    fixture.claude.answers_on_call(
        2,
        &claude_answer(r#"{"decision": "diagnose", "diagnosis": "The disk is full."}"#),
    );
    fixture
        .invoker("claude-code")
        .invoke_once(&fixture.invocation(
            ConductorTrigger::StepFailure,
            ConductorPurpose::DiagnoseFailure,
        ))
        .await;

    let rescue_args = fixture.claude.args(1);
    let (flag, rescue) = json_schema_arg(&rescue_args);
    let mode = rescue_args
        .iter()
        .position(|arg| arg == "--permission-mode")
        .expect("a permission mode");
    assert!(flag < mode, "{rescue_args:?}");
    assert_eq!(rescue["type"], "object");
    assert_eq!(rescue["required"], json!(["decision"]));
    assert_eq!(rescue["additionalProperties"], false);
    assert!(rescue.get("anyOf").is_none(), "{rescue}");
    assert_eq!(
        rescue["properties"]["decision"]["enum"],
        json!(["choose_output", "reprompt", "escalate"])
    );
    for field in ["label", "variables", "artifacts", "prompt"] {
        assert!(
            rescue["properties"].get(field).is_some(),
            "{field}: {rescue}"
        );
    }
    assert!(rescue["properties"].get("data").is_none(), "{rescue}");
    assert!(
        rescue["properties"].get("proposals").is_none(),
        "a label rescue proposes nothing: {rescue}"
    );

    let (_, diagnosis) = json_schema_arg(&fixture.claude.args(2));
    assert_eq!(
        diagnosis["properties"]["decision"]["enum"],
        json!(["diagnose", "retry"])
    );
    assert!(
        diagnosis["properties"].get("proposals").is_some(),
        "{diagnosis}"
    );
    assert!(
        diagnosis["properties"].get("label").is_none(),
        "{diagnosis}"
    );
}

#[tokio::test]
async fn a_claude_conductors_answer_is_read_from_its_structured_output() {
    let fixture = Fixture::new();
    fixture.claude.answers(&claude_structured_answer(
        "The agent finished the work.",
        json!({
            "decision": "choose_output",
            "label": "done",
            "artifacts": { "report": "docs/report.md" },
            "confidence": "high",
            "rationale": "The conversation ends with the work finished.",
        }),
    ));

    let outcome = fixture
        .invoker("claude-code")
        .invoke_once(&fixture.rescue())
        .await;
    let response = answered(outcome, ConductorPurpose::ChooseOutput);

    assert_eq!(
        response.decision,
        Decision::ChooseOutput {
            label: "done".to_string(),
            artifacts: [("report".to_string(), "docs/report.md".to_string())].into(),
            variables: Default::default(),
        }
    );
    assert_eq!(response.confidence, Confidence::High);
}

#[tokio::test]
async fn an_unusable_structured_answer_falls_back_to_the_text() {
    let fixture = Fixture::new();
    // A diagnosis may not choose an output; the text holds an allowed one.
    fixture.claude.answers(&claude_structured_answer(
        r#"{"decision": "diagnose", "diagnosis": "The disk is full."}"#,
        json!({ "decision": "choose_output", "label": "done" }),
    ));

    let outcome = fixture
        .invoker("claude-code")
        .invoke_once(&fixture.invocation(
            ConductorTrigger::StepFailure,
            ConductorPurpose::DiagnoseFailure,
        ))
        .await;
    let response = answered(outcome, ConductorPurpose::DiagnoseFailure);

    assert_eq!(response.kind(), DecisionKind::Diagnose);
}

#[tokio::test]
async fn an_opencode_conductor_gets_no_schema() {
    let fixture = Fixture::new();
    fixture.opencode.answers(&opencode_answer(CHOOSE_DONE));

    let outcome = fixture
        .invoker("opencode")
        .invoke_once(&fixture.rescue())
        .await;
    answered(outcome, ConductorPurpose::ChooseOutput);

    let args = fixture.opencode.args(1);
    assert!(
        !args
            .iter()
            .any(|arg| arg == "--json-schema" || arg.starts_with('{')),
        "{args:?}"
    );
}

// ---------------------------------------------------------------------------
// The response
// ---------------------------------------------------------------------------

#[test]
fn a_response_without_confidence_is_low() {
    let response = ConductorResponse::parse(
        r#"Escalating. {"decision": "escalate", "rationale": "unclear"}"#,
        ConductorPurpose::ChooseOutput,
    )
    .expect("parse");
    assert_eq!(response.confidence, Confidence::Low);
    assert_eq!(response.kind(), DecisionKind::Escalate);
}

#[test]
fn the_decisions_a_purpose_allows_are_enforced() {
    let reprompt = r#"{"decision": "reprompt", "prompt": "Which label?"}"#;
    assert!(ConductorResponse::parse(reprompt, ConductorPurpose::ChooseOutput).is_ok());
    assert_eq!(
        ConductorResponse::parse(reprompt, ConductorPurpose::ChooseOutputAfterReprompt),
        Err(ResponseError::Disallowed {
            decision: DecisionKind::Reprompt,
            purpose: ConductorPurpose::ChooseOutputAfterReprompt,
        })
    );

    let retry = r#"{"decision": "retry", "note": "The network was down."}"#;
    assert!(ConductorResponse::parse(retry, ConductorPurpose::DiagnoseFailure).is_ok());
    assert!(ConductorResponse::parse(retry, ConductorPurpose::ChooseOutput).is_err());

    // A transition can be proposed after a failure, never chosen (R23).
    let proposed = r#"{"decision": "diagnose", "diagnosis": "The tests fail.",
        "proposals": [{"kind": "transition", "label": "failed"}]}"#;
    assert!(ConductorResponse::parse(proposed, ConductorPurpose::DiagnoseFailure).is_ok());
    let chosen = r#"{"decision": "choose_output", "label": "failed"}"#;
    assert!(matches!(
        ConductorResponse::parse(chosen, ConductorPurpose::DiagnoseFailure),
        Err(ResponseError::Disallowed { .. })
    ));
    let unknown = r#"{"decision": "transition", "label": "failed"}"#;
    assert!(matches!(
        ConductorResponse::parse(unknown, ConductorPurpose::DiagnoseFailure),
        Err(ResponseError::Schema(_))
    ));

    let annotate = r#"{"decision": "annotate", "summary": "Approve the plan?",
        "recommendation": "approved"}"#;
    assert!(ConductorResponse::parse(annotate, ConductorPurpose::SummarizeGate).is_ok());
    let proposing = r#"{"decision": "annotate", "summary": "Approve the plan?",
        "proposals": [{"kind": "abort"}]}"#;
    assert!(matches!(
        ConductorResponse::parse(proposing, ConductorPurpose::SummarizeGate),
        Err(ResponseError::Schema(_))
    ));

    let silent_reply = r#"{"decision": "reply"}"#;
    assert!(matches!(
        ConductorResponse::parse(silent_reply, ConductorPurpose::ReplyToChat),
        Err(ResponseError::Schema(_))
    ));
}

#[test]
fn a_response_reads_back_from_its_value() {
    let response =
        ConductorResponse::parse(CHOOSE_DONE, ConductorPurpose::ChooseOutput).expect("parse");
    let again = ConductorResponse::from_value(response.to_value(), ConductorPurpose::ChooseOutput)
        .expect("read back");
    assert_eq!(again, response);
}

// ---------------------------------------------------------------------------
// The snapshot
// ---------------------------------------------------------------------------

fn event(sequence: u64, data: EventData) -> Event {
    let mut event = Event::new("execution-1".to_string(), data, sequence);
    event.timestamp = Utc::now();
    event
}

#[test]
fn a_long_tasks_snapshot_fits_its_budget_with_the_definition_and_pending_input_whole() {
    let definition = format!(
        "name: long\nstates:\n{}",
        "  # a comment line\n".repeat(200)
    );
    let pending = json!({
        "execution_id": "execution-1",
        "state_id": "implement",
        "instruction": "Settle the output. ".repeat(100),
        "continuation": "resolve_output",
    });
    let events = (1..=5_000)
        .map(|sequence| {
            event(
                sequence,
                EventData::Error {
                    message: "x".repeat(2_000),
                    context: Value::Null,
                },
            )
        })
        .collect();
    let conversations = (0..20)
        .map(|index| ConversationTail {
            state_id: format!("state-{}", index % 7),
            attempt_id: format!("attempt-{index}"),
            current: index < 3,
            text: "[stdout] a long line of tool output\n".repeat(5_000),
        })
        .collect();
    let chat = (1..=500)
        .map(|sequence| {
            let mut entry = ChatEntry::new(
                ChatAuthor::Operator,
                ChatEntryKind::Message,
                "y".repeat(5_000),
            );
            entry.sequence = sequence;
            entry
        })
        .collect();
    let input = SnapshotInput {
        execution_id: "execution-1".to_string(),
        workflow_id: "long".to_string(),
        status: "input_needed".to_string(),
        current_state: Some("implement".to_string()),
        state_outputs: vec![ChoiceDefinition {
            label: "implemented".to_string(),
            requires_variables: Vec::new(),
            requires_artifacts: Vec::new(),
            description: Some("The work is done.".to_string()),
            requires_human_approval: false,
            resume_comment_field: None,
        }],
        definition: Ok(definition.clone()),
        pending_input: Some(pending.clone()),
        variables: json!({ "big": "z".repeat(100_000) }),
        events,
        conversations,
        chat,
        ..SnapshotInput::default()
    };
    let budget = SnapshotBudget {
        total_bytes: 64 * 1024,
        ..SnapshotBudget::default()
    };

    let snapshot = render(&input, &budget);

    assert!(
        snapshot.text.len() <= budget.total_bytes,
        "{} bytes",
        snapshot.text.len()
    );
    assert!(snapshot.level.is_some(), "shortened, not left out");
    assert!(snapshot.text.contains(definition.trim_end()));
    assert!(snapshot
        .text
        .contains(&serde_json::to_string_pretty(&pending).unwrap()));
    assert!(snapshot
        .text
        .contains("- `implemented` — The work is done."));
    assert!(snapshot.text.contains("#5000"), "the latest event is shown");
    assert!(snapshot.text.contains("## Agent conversations"));
    assert!(snapshot.text.contains("## Recent chat"));
}

#[test]
fn whole_sections_over_the_budget_leave_the_rest_out_rather_than_being_cut() {
    let definition = "d".repeat(10_000);
    let input = SnapshotInput {
        definition: Ok(definition.clone()),
        events: vec![event(
            1,
            EventData::Error {
                message: "boom".to_string(),
                context: Value::Null,
            },
        )],
        ..SnapshotInput::default()
    };
    let budget = SnapshotBudget {
        total_bytes: 1_000,
        ..SnapshotBudget::default()
    };

    let snapshot = render(&input, &budget);
    assert_eq!(snapshot.level, None);
    assert!(snapshot.text.contains(&definition));
    assert!(!snapshot.text.contains("boom"));
}

// ---------------------------------------------------------------------------
// The prompt
// ---------------------------------------------------------------------------

/// Compares `actual` with the snapshot file `name`, or rewrites the file
/// when `NARSOR_UPDATE_SNAPSHOTS` is set.
fn assert_snapshot(name: &str, actual: &str) {
    let path = fixtures().join("conductor").join(name);
    if std::env::var_os("NARSOR_UPDATE_SNAPSHOTS").is_some() {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, actual).unwrap();
        return;
    }
    let expected = fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "read {}: {error} (run with NARSOR_UPDATE_SNAPSHOTS=1 to write it)",
            path.display()
        )
    });
    assert_eq!(
        actual, expected,
        "the prompt changed; if on purpose, rerun with NARSOR_UPDATE_SNAPSHOTS=1"
    );
}

#[test]
fn the_label_rescue_prompt_reads_as_recorded() {
    let prompt = conductor_prompt(
        ConductorPurpose::ChooseOutput,
        ConductorAccess::ReadOnlyWorkspace,
        "## Execution\n\n- execution: execution-1\n",
    );
    assert_snapshot("prompt_choose_output.txt", &prompt);
}

#[test]
fn the_diagnosis_prompt_reads_as_recorded() {
    let prompt = conductor_prompt(
        ConductorPurpose::DiagnoseFailure,
        ConductorAccess::Isolated,
        "## Execution\n\n- execution: execution-1\n",
    );
    assert_snapshot("prompt_diagnose_failure_isolated.txt", &prompt);
}

#[test]
fn every_purpose_offers_exactly_the_decisions_it_allows() {
    let purposes = [
        ConductorPurpose::ChooseOutput,
        ConductorPurpose::ChooseOutputAfterReprompt,
        ConductorPurpose::DiagnoseFailure,
        ConductorPurpose::ReplyToChat,
        ConductorPurpose::SummarizeGate,
    ];
    let all = [
        DecisionKind::ChooseOutput,
        DecisionKind::Reprompt,
        DecisionKind::Escalate,
        DecisionKind::Retry,
        DecisionKind::Diagnose,
        DecisionKind::Reply,
        DecisionKind::Annotate,
    ];
    let mut offered = HashMap::new();
    for purpose in purposes {
        let prompt = conductor_prompt(purpose, ConductorAccess::Isolated, "");
        let contract = prompt
            .split("`decision` must be exactly one of:")
            .nth(1)
            .and_then(|rest| rest.split("`confidence` is").next())
            .unwrap();
        for kind in all {
            let listed = contract.contains(&format!("- `{}` —", kind.name()));
            assert_eq!(
                listed,
                narsor_engine::conductor::allowed_decisions(purpose).contains(&kind),
                "{} offered for {}",
                kind.name(),
                purpose.name()
            );
            offered.insert((purpose.name(), kind.name()), listed);
        }
    }
    assert!(!offered[&("diagnose_failure", "choose_output")]);
}
