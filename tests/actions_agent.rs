//! Integration tests for the `AgentAction` executor (D17–D20).
//!
//! **No real harness is ever run here.** `claude` and `opencode` cost money,
//! need credentials and answer differently every time, which is the opposite of
//! what a test suite wants. Instead the adapters are pointed at
//! `tests/fixtures/harness/fake_harness.sh` through
//! [`ClaudeCodeAdapter::with_program`] — the same seam an operator uses for a
//! harness that is not on `PATH`, not a test-only hatch. Everything else is
//! real: real child processes, real temporary directories, the real engine.
//!
//! The one test that does drive `claude` is `#[ignore]`d and skips itself when
//! the binary is missing; run it deliberately with `cargo test -- --ignored`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use narsor::domain::{
    ActionConfig, ActionError, AgentActionConfig, Context, EventData, ExecutionStatus,
    OutputDefinition, RepromptedBy, Workspace, WorkspaceAccess,
};
use narsor::engine::actions::agent::{output_contract, AgentEventSink, SESSION_ID_VARIABLE};
use narsor::engine::{
    ActionExecutor, ActionKind, ActionOutcome, ActionRequest, AgentExecutor, ClaudeCodeAdapter,
    CodexAdapter, Engine, HarnessRegistry, InputContinuation, LoggingEventSink, OpenCodeAdapter,
    OutputProblem, OutputRejection, PendingHumanInput, METADATA_ENGINE_KEY,
    METADATA_OUTPUT_REJECTION_KEY,
};
use narsor::persistence::{
    recover, ConversationEntry, ConversationSource, ConversationStore, DefaultHarnessSelector,
    ExecutionStore, ReconciliationOutcome, StatePaths,
};
use narsor::workflow::{load_from_file, TemplateScope};
use serde_json::{json, Value};

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
        let path = std::env::temp_dir().join(format!("narsor-agent-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// A workspace whose primary repository is this directory.
    fn workspace(&self) -> Workspace {
        Workspace::local_folder(self.path.to_string_lossy().into_owned())
    }

    /// A subdirectory of the root, created.
    fn dir(&self, name: &str) -> PathBuf {
        let path = self.path.join(name);
        fs::create_dir_all(&path).expect("create directory");
        path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// The fake harness CLI, installed in a directory of its own.
struct FakeHarness {
    dir: PathBuf,
    program: PathBuf,
}

impl FakeHarness {
    /// Copies the fixture script into `dir` and returns a handle to it.
    ///
    /// Copies to a temporary name in the same directory and renames it into
    /// place, rather than `fs::copy`-ing straight to `program`: writing
    /// directly to the final name and then, moments later, `execve`-ing it
    /// (once this test's harness runs) has been an intermittent ETXTBSY
    /// flake under `cargo test`'s parallel test threads — many threads
    /// copying and exec'ing fake-harness binaries at once occasionally hits
    /// the kernel's "text file busy" window before a fresh file's write
    /// handle is fully torn down. The temp-name-then-rename here is the same
    /// atomic-swap idiom `src/persistence/atomic.rs` uses for durability: the
    /// final path is only ever created by a single `rename`, never opened for
    /// writing itself, so there is nothing left for a concurrent `execve` to
    /// collide with.
    fn install(dir: PathBuf) -> Self {
        let source =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/harness/fake_harness.sh");
        let program = dir.join("fake_harness.sh");
        let temp = dir.join(format!(".fake_harness.sh.{}.tmp", uuid::Uuid::new_v4()));
        fs::copy(&source, &temp).expect("copy the fake harness");
        fs::rename(&temp, &program).expect("move the fake harness into place");
        Self { dir, program }
    }

    /// The program an adapter is built with.
    fn program(&self) -> String {
        self.program.to_string_lossy().into_owned()
    }

    /// What the harness prints on stdout, on every call.
    fn answers(&self, stdout: &str) -> &Self {
        fs::write(self.dir.join("stdout"), stdout).expect("write the harness answer");
        self
    }

    /// What the harness prints on the `nth` call (1-based).
    fn answers_on_call(&self, nth: usize, stdout: &str) -> &Self {
        fs::write(self.dir.join(format!("stdout.{nth}")), stdout).expect("write the answer");
        self
    }

    /// Makes the harness sleep before answering.
    fn sleeps(&self, seconds: u64) -> &Self {
        fs::write(self.dir.join("sleep"), seconds.to_string()).expect("write the sleep");
        self
    }

    /// Makes the `nth` call (1-based) sleep after answering.
    fn sleeps_on_call(&self, nth: usize, seconds: u64) -> &Self {
        fs::write(self.dir.join(format!("sleep.{nth}")), seconds.to_string())
            .expect("write the sleep");
        self
    }

    /// Makes the harness fail with an exit code and a message on stderr.
    fn fails(&self, code: i32, stderr: &str) -> &Self {
        fs::write(self.dir.join("exit"), code.to_string()).expect("write the exit code");
        fs::write(self.dir.join("stderr"), stderr).expect("write the stderr");
        self
    }

    /// The arguments of the `nth` call.
    fn args(&self, nth: usize) -> Vec<String> {
        let raw = fs::read_to_string(self.dir.join(format!("args.{nth}")))
            .expect("the harness recorded its arguments");
        raw.split("\n--%ARG%--\n")
            .filter(|arg| !arg.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// The working directory of the `nth` call.
    fn cwd(&self, nth: usize) -> String {
        read_line(&self.dir.join(format!("cwd.{nth}")))
    }

    /// The `ORCHESTRATOR_ACTION_ID` the `nth` call saw.
    fn action_id(&self, nth: usize) -> String {
        read_line(&self.dir.join(format!("action_id.{nth}")))
    }

    /// The process id of the `nth` call.
    fn pid(&self, nth: usize) -> u32 {
        read_line(&self.dir.join(format!("pid.{nth}")))
            .parse()
            .expect("the pid is a number")
    }

    /// How many times the harness was called.
    fn calls(&self) -> usize {
        fs::read_to_string(self.dir.join("count"))
            .map(|raw| raw.trim().parse().expect("the count is a number"))
            .unwrap_or(0)
    }
}

/// The first line of a file the harness wrote.
fn read_line(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .trim_end()
        .to_string()
}

/// A single `stream-json` `result` line carrying `text`.
///
/// A single line is still a well-formed (if minimal) NDJSON stream, so this
/// keeps working as the stand-in for "the harness answered" in every test
/// that does not care about the lines leading up to it.
fn envelope(text: &str) -> String {
    json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "session_id": "fake-session-1",
        "result": text,
    })
    .to_string()
}

/// A single `stream-json` `result` line reporting a failed run.
fn failed_envelope(message: &str) -> String {
    json!({
        "type": "result",
        "subtype": "error_during_execution",
        "is_error": true,
        "session_id": "fake-session-1",
        "result": message,
    })
    .to_string()
}

/// Joins NDJSON lines the way a streaming harness actually prints them: one
/// JSON value per line, each one terminated by `\n` — including the last
/// one, which is what a real harness does (it flushes each line as it is
/// produced, well before it has anything else to say, let alone before it
/// exits) and which matters for a line that arrives right before the process
/// is killed: only a `\n`-terminated line is guaranteed to be read as a whole
/// line rather than lost as an incomplete trailing chunk.
fn ndjson(lines: &[Value]) -> String {
    lines
        .iter()
        .map(|line| format!("{line}\n"))
        .collect::<String>()
}

/// A full `claude --output-format stream-json` run: `system`/`init`, an
/// `assistant` message using a tool, the `user` tool result, and the final
/// `result` line.
fn claude_stream(session_id: &str, output_json: &str) -> String {
    ndjson(&[
        json!({ "type": "system", "subtype": "init", "session_id": session_id }),
        json!({
            "type": "assistant",
            "message": {
                "id": "msg-1",
                "content": [{ "type": "tool_use", "name": "Read", "input": { "file_path": "a.txt" } }],
            },
        }),
        json!({
            "type": "user",
            "message": { "content": [{ "type": "tool_result", "content": "file contents" }] },
        }),
        json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "session_id": session_id,
            "result": output_json,
        }),
    ])
}

/// A full `opencode run --format json` run: `step_start`, a `tool_use`, the
/// final `text`, then `step_finish`.
fn opencode_stream(session_id: &str, text: &str) -> String {
    ndjson(&[
        json!({ "type": "step_start", "sessionID": session_id }),
        json!({
            "type": "tool_use",
            "sessionID": session_id,
            "state": { "input": {}, "output": "ok" },
        }),
        json!({ "type": "text", "sessionID": session_id, "text": text }),
        json!({ "type": "step_finish", "sessionID": session_id }),
    ])
}

/// A complete successful `codex exec --json` stream.  Codex's success is more
/// than an assistant message: it must identify the thread and subsequently
/// complete the turn.
fn codex_stream(thread_id: &str, text: &str) -> String {
    ndjson(&[
        json!({ "type": "thread.started", "thread_id": thread_id }),
        json!({
            "type": "item.completed",
            "item": { "type": "agent_message", "text": text },
        }),
        json!({ "type": "turn.completed" }),
    ])
}

/// A store, scoped to one execution, that a test can wire into an
/// [`ActionRequest`] and later read the conversation back from.
struct ConversationFixture {
    store: ConversationStore,
}

impl ConversationFixture {
    /// A fresh store under `root`, scoped to `EXECUTION_ID`.
    fn new(root: &TempRoot) -> Self {
        let execution_store = ExecutionStore::new(root.dir("conversations"));
        Self {
            store: execution_store.conversation_store(EXECUTION_ID),
        }
    }

    /// Every entry of `attempt_id`'s conversation, from the beginning.
    fn entries(&self, attempt_id: &str) -> Vec<ConversationEntry> {
        self.store
            .read(attempt_id, 1, usize::MAX)
            .expect("read the conversation")
            .unwrap_or_else(|| panic!("attempt '{attempt_id}' has no conversation file"))
            .entries
    }

    /// Whether the attempt was closed (compressed).
    fn is_closed(&self, attempt_id: &str) -> bool {
        self.store
            .read(attempt_id, 1, usize::MAX)
            .expect("read the conversation")
            .map(|snapshot| snapshot.closed)
            .unwrap_or(false)
    }
}

/// The raw bytes of an entry, decoded as UTF-8 (lossily — tests that care
/// about exact bytes compare `entry.raw` directly instead).
fn entry_text(entry: &ConversationEntry) -> String {
    String::from_utf8_lossy(&entry.raw).into_owned()
}

/// The fixed ids every test in this file runs its agent action under.
const EXECUTION_ID: &str = "execution-1";
const ACTION_EXECUTION_ID: &str = "action-exec-1";

/// Runs an agent action with a conversation store wired in.
async fn execute_with_conversation(
    executor: &AgentExecutor,
    config: AgentActionConfig,
    workspace: &Workspace,
    conversation: &ConversationFixture,
    timeout: Option<Duration>,
) -> Result<ActionOutcome, ActionError> {
    let action = ActionConfig::Agent(config);
    executor
        .execute(
            ActionRequest::new(&action, workspace, ACTION_EXECUTION_ID)
                .with_timeout(timeout)
                .with_execution_id(EXECUTION_ID)
                .with_conversation_store(&conversation.store),
        )
        .await
}

/// An agent action on the `claude-code` harness.
fn agent_action(prompt: &str) -> AgentActionConfig {
    AgentActionConfig {
        harness: Some("claude-code".to_string()),
        model: None,
        workspace_access: WorkspaceAccess::ReadWrite,
        prompt: prompt.to_string(),
        workspace: None,
        parameters: HashMap::new(),
        read_only_paths: Vec::new(),
    }
}

/// An executor whose `claude-code` adapter runs the fake harness.
fn executor(fake: &FakeHarness) -> AgentExecutor {
    AgentExecutor::with_harnesses(
        HarnessRegistry::defaults()
            .with_adapter(Arc::new(ClaudeCodeAdapter::with_program(fake.program()))),
    )
}

/// Runs an agent action and returns the outcome.
async fn execute(
    executor: &AgentExecutor,
    config: AgentActionConfig,
    workspace: &Workspace,
    timeout: Option<Duration>,
) -> Result<ActionOutcome, ActionError> {
    let action = ActionConfig::Agent(config);

    executor
        .execute(
            ActionRequest::new(&action, workspace, "action-exec-1")
                .with_timeout(timeout)
                .with_execution_id("execution-1"),
        )
        .await
}

/// Runs an agent action expected to complete, and returns its result.
async fn complete(
    executor: &AgentExecutor,
    config: AgentActionConfig,
    workspace: &Workspace,
) -> narsor::domain::ActionResult {
    match execute(executor, config, workspace, None).await {
        Ok(ActionOutcome::Completed(result)) => result,
        Ok(other) => panic!("expected a completed action, got {other:?}"),
        Err(error) => panic!("action failed: {error:?}"),
    }
}

/// Runs an agent action expected to fail, and returns the error.
async fn fail(
    executor: &AgentExecutor,
    config: AgentActionConfig,
    workspace: &Workspace,
) -> ActionError {
    match execute(executor, config, workspace, None).await {
        Err(error) => error,
        Ok(outcome) => panic!("expected a failure, got {outcome:?}"),
    }
}

/// An event sink that keeps what it was given.
#[derive(Debug, Default)]
struct RecordingSink {
    events: Mutex<Vec<(String, EventData)>>,
}

impl RecordingSink {
    /// The `type` tag of every recorded event, in order.
    fn types(&self) -> Vec<String> {
        self.events
            .lock()
            .expect("events")
            .iter()
            .map(|(_, data)| event_type(data))
            .collect()
    }

    /// Every recorded event.
    fn events(&self) -> Vec<(String, EventData)> {
        self.events.lock().expect("events").clone()
    }
}

impl AgentEventSink for RecordingSink {
    fn emit(&self, execution_id: &str, data: EventData) {
        self.events
            .lock()
            .expect("events")
            .push((execution_id.to_string(), data));
    }
}

/// The `type` tag serde writes for an event.
fn event_type(data: &EventData) -> String {
    serde_json::to_value(data)
        .expect("serialize event data")
        .get("type")
        .and_then(Value::as_str)
        .expect("every event data variant is tagged")
        .to_string()
}

// ---------------------------------------------------------------------------
// Reading the agent's answer (D19/D20)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_json_object_gives_the_label_and_its_variables() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope(
        r#"{"output":"ready","variables":{"plan":"plan.md"}}"#,
    ));

    let result = complete(
        &executor(&fake),
        agent_action("look around"),
        &root.workspace(),
    )
    .await;

    assert_eq!(result.output_label, "ready");
    assert_eq!(result.variables.get("plan"), Some(&json!("plan.md")));
}

#[tokio::test]
async fn an_agent_question_suspends_the_same_action_for_human_input() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope(
        r#"{"status":"awaiting_input","question":"Which API version should I target?"}"#,
    ));

    let outcome = execute(
        &executor(&fake),
        agent_action("implement the change"),
        &root.workspace(),
        None,
    )
    .await
    .expect("the question is a suspension, not an action failure");

    let ActionOutcome::Suspended(narsor::engine::actions::SuspendedAction::AwaitingAgentInput(
        request,
    )) = outcome
    else {
        panic!("expected an agent-input suspension");
    };
    assert_eq!(request.instruction, "Which API version should I target?");
    assert_eq!(request.fields[0].name, "answer");
    assert_eq!(request.agent_harness.as_deref(), Some("claude-code"));
    assert_eq!(request.agent_session_id.as_deref(), Some("fake-session-1"));
}

#[tokio::test]
async fn an_agent_question_can_offer_choices_with_an_optional_comment() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope(
        r#"{"status":"awaiting_input","question":"Choose a version","options":["v1","v2"]}"#,
    ));

    let outcome = execute(
        &executor(&fake),
        agent_action("implement the change"),
        &root.workspace(),
        None,
    )
    .await
    .expect("the menu is a suspension, not an action failure");

    let ActionOutcome::Suspended(narsor::engine::actions::SuspendedAction::AwaitingAgentInput(
        request,
    )) = outcome
    else {
        panic!("expected an agent-input suspension");
    };
    assert_eq!(request.fields[0].name, "option");
    assert_eq!(request.fields[0].options, vec!["v1", "v2"]);
    assert_eq!(request.fields[1].name, "comment");
    assert_eq!(request.fields[2].name, "answer");
}

#[tokio::test]
async fn a_fenced_json_block_is_extracted() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope(
        "Here is what I found.\n\n```json\n{\"output\": \"ready\", \
         \"variables\": {\"plan\": \"plan.md\"}}\n```\n",
    ));

    let result = complete(
        &executor(&fake),
        agent_action("look around"),
        &root.workspace(),
    )
    .await;

    assert_eq!(result.output_label, "ready");
    assert_eq!(result.variables.get("plan"), Some(&json!("plan.md")));
}

#[tokio::test]
async fn the_last_json_object_wins_when_the_agent_rambled_first() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope(
        "I considered answering {\"output\": \"ambiguous\"} but then I read \
         plan.md — a config file holds {\"unrelated\": true} — and settled on:\n\
         {\"output\": \"ready\", \"data\": {\"files\": 3}}",
    ));

    let result = complete(
        &executor(&fake),
        agent_action("look around"),
        &root.workspace(),
    )
    .await;

    assert_eq!(result.output_label, "ready");
    assert_eq!(result.data.get("files"), Some(&json!(3)));
}

#[tokio::test]
async fn an_answer_without_any_json_is_an_error_and_never_a_guessed_label() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope(
        "I had a look around and everything seems fine to me.",
    ));

    let error = fail(
        &executor(&fake),
        agent_action("look around"),
        &root.workspace(),
    )
    .await;

    assert_eq!(error.error_type.as_deref(), Some("missing_output_label"));
    assert!(
        error.message.contains("no output label could be extracted"),
        "{}",
        error.message
    );
    assert!(
        !error.message.contains("success"),
        "no label may be invented: {}",
        error.message
    );
}

#[tokio::test]
async fn json_without_an_output_key_is_an_error() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope(
        "Done.\n{\"status\": \"ok\", \"variables\": {\"plan\": \"plan.md\"}}",
    ));

    let error = fail(
        &executor(&fake),
        agent_action("look around"),
        &root.workspace(),
    )
    .await;

    assert_eq!(error.error_type.as_deref(), Some("missing_output_label"));
    assert!(
        error.message.contains("'output'"),
        "the message names the missing key: {}",
        error.message
    );

    // What the engine needs to ask the same session again (KTD7).
    let details = error.details.expect("details");
    assert_eq!(details["harness"], json!("claude-code"));
    assert_eq!(details["harness_session_id"], json!("fake-session-1"));
}

/// The session config records the harness the attempt ran on, even when the
/// action named none, so the session can be resumed where it lives (KTD7).
#[tokio::test]
async fn the_session_config_records_the_harness_that_ran() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope(r#"{"output":"ready"}"#));

    let mut config = agent_action("do the thing");
    config.harness = None;
    let result = complete(&executor(&fake), config, &root.workspace()).await;

    let session = &result.data["agent_session"];
    assert_eq!(session["config"]["harness"], json!("claude-code"));
    assert_eq!(
        session["config"]["harness_session_id"],
        json!("fake-session-1")
    );
}

#[tokio::test]
async fn a_failing_harness_is_an_error_naming_its_stderr() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope(r#"{"output":"ready"}"#))
        .fails(2, "credit balance too low\n");

    let error = fail(
        &executor(&fake),
        agent_action("look around"),
        &root.workspace(),
    )
    .await;

    assert_eq!(error.error_type.as_deref(), Some("harness_failed"));
    assert!(
        error.message.contains("credit balance too low"),
        "{}",
        error.message
    );
}

// ---------------------------------------------------------------------------
// Sessions (D18/D19)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_session_events_are_emitted_and_the_harness_session_id_is_captured() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope(r#"{"output":"ready"}"#));

    let sink = Arc::new(RecordingSink::default());
    let executor = executor(&fake).with_event_sink(sink.clone());

    let result = complete(&executor, agent_action("look around"), &root.workspace()).await;

    assert_eq!(
        sink.types(),
        vec![
            "agent_session_started",
            "agent_message",
            "agent_session_completed"
        ]
    );

    let events = sink.events();
    let (execution_id, started) = &events[0];
    assert_eq!(execution_id, "execution-1");
    let EventData::AgentSessionStarted {
        session_id,
        harness,
        action_execution_id,
        resumed_session,
    } = started
    else {
        panic!("the first event is the session start: {started:?}");
    };
    assert_eq!(harness, "claude-code");
    assert_eq!(action_execution_id.as_deref(), Some("action-exec-1"));
    assert_eq!(resumed_session, &None);

    // Every event of this attempt carries the same `action_execution_id`.
    let EventData::AgentMessage {
        action_execution_id: message_action_id,
        ..
    } = &events[1].1
    else {
        panic!("the middle event is the agent message");
    };
    assert_eq!(message_action_id.as_deref(), Some("action-exec-1"));

    // The session is the same one throughout.
    let EventData::AgentSessionCompleted {
        session_id: completed,
        action_execution_id: completed_action_id,
        harness_session_id,
    } = &events[2].1
    else {
        panic!("the last event is the session completion");
    };
    assert_eq!(completed, session_id);
    assert_eq!(completed_action_id.as_deref(), Some("action-exec-1"));
    assert_eq!(harness_session_id.as_deref(), Some("fake-session-1"));

    // The harness's own session id is captured, and reachable for a later state.
    assert_eq!(
        result.variables.get(SESSION_ID_VARIABLE),
        Some(&json!("fake-session-1"))
    );
    assert_eq!(
        result.data["agent_session"]["config"]["harness_session_id"],
        json!("fake-session-1")
    );
    assert_eq!(
        result.data["agent_session"]["harness"],
        json!("claude-code")
    );
}

#[tokio::test]
async fn a_failed_run_ends_the_session_with_a_failure_event() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope("no JSON here at all"));

    let sink = Arc::new(RecordingSink::default());
    let executor = executor(&fake).with_event_sink(sink.clone());

    let _ = fail(&executor, agent_action("look around"), &root.workspace()).await;

    assert_eq!(
        sink.types(),
        vec![
            "agent_session_started",
            "agent_message",
            "agent_session_failed"
        ]
    );
}

#[tokio::test]
async fn a_session_to_resume_reaches_the_argv() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope(r#"{"output":"ready"}"#));

    let mut config = agent_action("carry on");
    config
        .parameters
        .insert("resume_session".to_string(), json!("session-42"));

    let sink = Arc::new(RecordingSink::default());
    let executor = executor(&fake).with_event_sink(sink.clone());

    complete(&executor, config, &root.workspace()).await;

    let args = fake.args(1);
    assert!(
        args.windows(2)
            .any(|pair| pair[0] == "--resume" && pair[1] == "session-42"),
        "{args:?}"
    );

    let (_, started) = &sink.events()[0];
    let EventData::AgentSessionStarted {
        resumed_session, ..
    } = started
    else {
        panic!("the first event is the session start: {started:?}");
    };
    assert_eq!(resumed_session.as_deref(), Some("session-42"));
}

// ---------------------------------------------------------------------------
// The command line (D17)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_model_and_the_read_only_paths_reach_the_argv() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope(r#"{"output":"ready"}"#));

    let mut config = agent_action("look around");
    config.model = Some("opus".to_string());
    config.read_only_paths = vec!["../other-repo/src".to_string()];

    complete(&executor(&fake), config, &root.workspace()).await;

    let args = fake.args(1);

    assert!(args.contains(&"--print".to_string()), "{args:?}");
    assert!(
        args.windows(2)
            .any(|pair| pair[0] == "--output-format" && pair[1] == "stream-json"),
        "{args:?}"
    );
    assert!(args.contains(&"--verbose".to_string()), "{args:?}");
    assert!(
        args.windows(2)
            .any(|pair| pair[0] == "--model" && pair[1] == "opus"),
        "{args:?}"
    );

    let add_dir = args
        .iter()
        .position(|arg| arg == "--add-dir")
        .expect("read_only_paths reach --add-dir");
    assert_eq!(
        args[add_dir + 1],
        root.path()
            .join("../other-repo/src")
            .to_string_lossy()
            .into_owned()
    );

    // The permission mode is the conservative default, and the prompt is last —
    // after a flag that takes exactly one value, so it cannot be swallowed.
    let mode = args
        .iter()
        .position(|arg| arg == "--permission-mode")
        .expect("a permission mode is always passed");
    assert_eq!(args[mode + 1], "acceptEdits");
    assert_eq!(mode + 2, args.len() - 1);
    assert!(args.last().expect("a prompt").starts_with("look around"));
}

#[tokio::test]
async fn claude_rejects_harness_specific_workspace_permissions() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope(r#"{"output":"ready"}"#));

    let mut config = agent_action("look around");
    config
        .parameters
        .insert("permission_mode".to_string(), json!("bypassPermissions"));

    let error = fail(&executor(&fake), config, &root.workspace()).await;
    assert_eq!(error.error_type.as_deref(), Some("invalid_parameter"));
    assert!(
        error.message.contains("workspace_access"),
        "{}",
        error.message
    );
    assert_eq!(fake.calls(), 0);
}

#[tokio::test]
async fn claude_maps_read_only_workspace_access_to_plan_mode() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope(r#"{"output":"ready"}"#));

    let mut config = agent_action("look around");
    config.workspace_access = WorkspaceAccess::ReadOnly;

    complete(&executor(&fake), config, &root.workspace()).await;

    let args = fake.args(1);
    let mode = args
        .iter()
        .position(|arg| arg == "--permission-mode")
        .unwrap();
    assert_eq!(args[mode + 1], "plan");
}

#[tokio::test]
async fn the_opencode_adapter_builds_its_own_command_line() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&json!({ "sessionID": "ses_7", "text": "{\"output\":\"ready\"}" }).to_string());

    let executor = AgentExecutor::with_harnesses(
        HarnessRegistry::defaults()
            .with_adapter(Arc::new(OpenCodeAdapter::with_program(fake.program()))),
    );

    let mut config = agent_action("look around");
    config.harness = Some("opencode".to_string());
    config.model = Some("anthropic/claude-sonnet-4".to_string());
    let result = complete(&executor, config, &root.workspace()).await;

    assert_eq!(result.output_label, "ready");
    assert_eq!(
        result.variables.get(SESSION_ID_VARIABLE),
        Some(&json!("ses_7"))
    );

    let args = fake.args(1);
    assert_eq!(args[0], "run");
    assert!(
        args.windows(2)
            .any(|pair| pair[0] == "--model" && pair[1] == "anthropic/claude-sonnet-4"),
        "{args:?}"
    );
    assert!(args.contains(&"--auto".to_string()), "{args:?}");
    assert_eq!(args[args.len() - 3], "--format");
    assert_eq!(args[args.len() - 2], "json");
}

#[tokio::test]
async fn the_codex_adapter_builds_a_safe_fresh_command_line_and_records_its_thread() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&codex_stream("thread-7", r#"{"output":"ready"}"#));

    let executor = AgentExecutor::with_harnesses(
        HarnessRegistry::defaults()
            .with_adapter(Arc::new(CodexAdapter::with_program(fake.program()))),
    );
    let mut config = agent_action("look around");
    config.harness = Some("codex".to_string());
    config.model = Some("gpt-5-codex".to_string());
    config.read_only_paths = vec!["../reference".to_string()];

    let result = complete(&executor, config, &root.workspace()).await;

    assert_eq!(result.output_label, "ready");
    assert_eq!(
        result.variables.get(SESSION_ID_VARIABLE),
        Some(&json!("thread-7"))
    );
    let args = fake.args(1);
    assert_eq!(args[0], "exec");
    assert!(
        args.windows(2)
            .any(|pair| pair == ["--model", "gpt-5-codex"]),
        "{args:?}"
    );
    assert!(
        args.windows(2)
            .any(|pair| pair == ["--sandbox", "workspace-write"]),
        "{args:?}"
    );
    assert!(
        args.windows(2)
            .any(|pair| pair == ["--config", "approval_policy=never"]),
        "{args:?}"
    );
    assert!(args.contains(&"--json".to_string()), "{args:?}");
    assert!(!args.contains(&"--add-dir".to_string()), "{args:?}");
    assert!(
        args.last()
            .is_some_and(|prompt| prompt.contains("readable but must not be modified")),
        "{args:?}"
    );
}

#[tokio::test]
async fn codex_resume_uses_the_exec_resume_grammar() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&codex_stream("thread-42", r#"{"output":"ready"}"#));
    let executor = AgentExecutor::with_harnesses(
        HarnessRegistry::defaults()
            .with_adapter(Arc::new(CodexAdapter::with_program(fake.program()))),
    );
    let mut config = agent_action("continue");
    config.harness = Some("codex".to_string());
    config
        .parameters
        .insert("resume_session".to_string(), json!("thread-42"));

    let result = complete(&executor, config, &root.workspace()).await;

    assert_eq!(
        result.variables.get(SESSION_ID_VARIABLE),
        Some(&json!("thread-42"))
    );
    let args = fake.args(1);
    assert_eq!(args[0], "exec");
    let resume = args
        .iter()
        .position(|arg| arg == "resume")
        .expect("resume subcommand");
    assert_eq!(args[resume + 1], "thread-42");
    assert!(
        args.last()
            .is_some_and(|prompt| prompt.starts_with("continue")),
        "{args:?}"
    );
}

#[tokio::test]
async fn codex_rejects_unsafe_sandbox_before_spawn() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    let executor = AgentExecutor::with_harnesses(
        HarnessRegistry::defaults()
            .with_adapter(Arc::new(CodexAdapter::with_program(fake.program()))),
    );
    let mut config = agent_action("do the thing");
    config.harness = Some("codex".to_string());
    config
        .parameters
        .insert("sandbox".to_string(), json!("danger-full-access"));

    let error = fail(&executor, config, &root.workspace()).await;
    assert_eq!(error.error_type.as_deref(), Some("invalid_parameter"));
    assert_eq!(fake.calls(), 0);
}

#[tokio::test]
async fn codex_maps_read_only_workspace_access_to_its_native_sandbox() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&codex_stream("thread-7", r#"{"output":"ready"}"#));
    let executor = AgentExecutor::with_harnesses(
        HarnessRegistry::defaults()
            .with_adapter(Arc::new(CodexAdapter::with_program(fake.program()))),
    );
    let mut config = agent_action("look around");
    config.harness = Some("codex".to_string());
    config.workspace_access = WorkspaceAccess::ReadOnly;

    complete(&executor, config, &root.workspace()).await;

    assert!(fake
        .args(1)
        .windows(2)
        .any(|pair| pair == ["--sandbox", "read-only"]));
}

#[tokio::test]
async fn codex_requires_a_completed_turn_after_its_agent_message() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&ndjson(&[
        json!({ "type": "thread.started", "thread_id": "thread-7" }),
        json!({ "type": "item.completed", "item": { "type": "agent_message", "text": "{\\\"output\\\":\\\"ready\\\"}" } }),
    ]));
    let executor = AgentExecutor::with_harnesses(
        HarnessRegistry::defaults()
            .with_adapter(Arc::new(CodexAdapter::with_program(fake.program()))),
    );
    let mut config = agent_action("do the thing");
    config.harness = Some("codex".to_string());

    let error = fail(&executor, config, &root.workspace()).await;
    assert_eq!(error.error_type.as_deref(), Some("harness_failed"));
    assert!(
        error.message.contains("turn.completed"),
        "{}",
        error.message
    );
}

#[tokio::test]
async fn codex_terminal_failure_dominates_an_earlier_agent_message() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&ndjson(&[
        json!({ "type": "thread.started", "thread_id": "thread-7" }),
        json!({ "type": "item.completed", "item": { "type": "agent_message", "text": "{\\\"output\\\":\\\"ready\\\"}" } }),
        json!({ "type": "turn.failed", "error": { "message": "permission denied" } }),
    ]));
    let executor = AgentExecutor::with_harnesses(
        HarnessRegistry::defaults()
            .with_adapter(Arc::new(CodexAdapter::with_program(fake.program()))),
    );
    let mut config = agent_action("do the thing");
    config.harness = Some("codex".to_string());

    let error = fail(&executor, config, &root.workspace()).await;
    assert_eq!(error.error_type.as_deref(), Some("harness_failed"));
    assert!(
        error.message.contains("permission denied"),
        "{}",
        error.message
    );
}

// ---------------------------------------------------------------------------
// The prompt (U4 / §3.3.A)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_harness_receives_the_expanded_prompt_and_the_output_contract() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope(r#"{"output":"ready"}"#));

    // What the engine does before it calls an executor (U4): expand once.
    let action = ActionConfig::Agent(agent_action(
        "Implement {{ inputs.feature_description }} in {{ context.variables.module }}.",
    ));
    let mut context = Context::new();
    context.update_variables(
        serde_json::json!({ "module": "auth" })
            .as_object()
            .expect("an object")
            .clone(),
    );
    let inputs = json!({ "feature_description": "login" });
    let scope = TemplateScope::from_execution(&inputs, &context).expect("build the scope");
    let rendered = scope
        .render_action(&action, "state 'analyze'")
        .expect("expand the prompt");

    let ActionConfig::Agent(rendered) = rendered else {
        panic!("rendering an agent action gives an agent action");
    };

    complete(&executor(&fake), rendered, &root.workspace()).await;

    let prompt = fake.args(1).last().expect("a prompt").clone();
    assert!(
        prompt.starts_with("Implement login in auth."),
        "the executor forwards the expanded text: {prompt}"
    );
    assert!(!prompt.contains("{{"), "{prompt}");
    assert!(
        prompt.contains("\"output\""),
        "the output contract is appended: {prompt}"
    );
}

/// The prompt a request with `outputs` sends to the fake harness.
async fn prompt_sent_with_outputs(outputs: &[OutputDefinition]) -> String {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope(r#"{"output":"ready"}"#));

    let action = ActionConfig::Agent(agent_action("do the thing"));
    let workspace = root.workspace();
    executor(&fake)
        .execute(ActionRequest::new(&action, &workspace, ACTION_EXECUTION_ID).with_outputs(outputs))
        .await
        .expect("the action runs");

    fake.args(1).last().expect("a prompt").clone()
}

/// The declared labels are listed by the engine, in declaration order, each
/// with its description where the workflow gives one.
#[tokio::test]
async fn the_output_contract_lists_the_declared_labels_with_their_descriptions() {
    let outputs = [
        OutputDefinition {
            label: "ready".to_string(),
            description: Some("The plan is written and nothing is unclear.".to_string()),
            requires_human_approval: false,
            resume_comment_field: None,
        },
        OutputDefinition::new("ambiguous"),
    ];

    let prompt = prompt_sent_with_outputs(&outputs).await;

    let ready = prompt
        .find("- `ready` — The plan is written and nothing is unclear.\n")
        .unwrap_or_else(|| panic!("the described label is listed: {prompt}"));
    let ambiguous = prompt
        .find("- `ambiguous`\n")
        .unwrap_or_else(|| panic!("the bare label is listed: {prompt}"));
    assert!(ready < ambiguous, "declaration order is kept: {prompt}");
    assert!(
        !prompt.contains("listed above"),
        "the contract no longer relies on the prompt's author: {prompt}"
    );
}

/// A state without descriptions gets a plain list — no dangling dashes.
#[test]
fn a_contract_without_descriptions_lists_bare_labels_only() {
    let contract = output_contract(&[
        OutputDefinition::new("approved"),
        OutputDefinition {
            label: "rejected".to_string(),
            // Blank is treated as no description at all.
            description: Some("   ".to_string()),
            requires_human_approval: false,
            resume_comment_field: None,
        },
    ]);

    assert!(contract.contains("- `approved`\n"), "{contract}");
    assert!(contract.contains("- `rejected`\n"), "{contract}");
    assert!(!contract.contains(" — "), "{contract}");
}

/// With no outputs to list, the agent is pointed at the label the task names.
#[tokio::test]
async fn a_request_without_outputs_asks_for_the_label_the_task_names() {
    let prompt = prompt_sent_with_outputs(&[]).await;

    assert!(
        prompt.contains("the label the task asks for"),
        "the fallback wording is used: {prompt}"
    );
    assert!(
        !prompt.contains("- `"),
        "no label list is rendered: {prompt}"
    );
}

// ---------------------------------------------------------------------------
// The process (U5's rules, applied to a harness)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_harness_runs_in_the_workspace_with_the_action_id_in_its_environment() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope(r#"{"output":"ready"}"#));

    complete(
        &executor(&fake),
        agent_action("look around"),
        &root.workspace(),
    )
    .await;

    assert_eq!(
        fs::canonicalize(fake.cwd(1)).expect("canonicalize the cwd"),
        fs::canonicalize(root.path()).expect("canonicalize the workspace")
    );
    assert_eq!(fake.action_id(1), "action-exec-1");
}

#[tokio::test]
async fn a_timeout_kills_the_harness_and_is_reported_as_a_timeout() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope(r#"{"output":"ready"}"#)).sleeps(30);

    let outcome = execute(
        &executor(&fake),
        agent_action("think for a long time"),
        &root.workspace(),
        Some(Duration::from_millis(300)),
    )
    .await
    .expect("a timeout is an outcome, not a failure");

    let ActionOutcome::TimedOut(timed_out) = outcome else {
        panic!("expected a timeout, got {outcome:?}");
    };
    assert!(timed_out.subject.contains("claude-code"), "{timed_out:?}");

    // The process is really gone, not merely no longer awaited.
    let pid = fake.pid(1);
    assert!(!process_is_alive(pid), "process {pid} survived the timeout");
}

/// Whether a process id still exists. `/proc` is authoritative on Linux and the
/// child has already been reaped, so a surviving entry means a surviving
/// process.
fn process_is_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[tokio::test]
async fn a_missing_harness_binary_names_it_and_says_it_must_be_on_path() {
    let root = TempRoot::new();

    let executor =
        AgentExecutor::with_harnesses(HarnessRegistry::defaults().with_adapter(Arc::new(
            ClaudeCodeAdapter::with_program("claude-that-is-not-installed"),
        )));

    let error = fail(&executor, agent_action("look around"), &root.workspace()).await;

    assert_eq!(error.error_type.as_deref(), Some("harness_not_found"));
    assert!(
        error.message.contains("claude-that-is-not-installed"),
        "{}",
        error.message
    );
    assert!(error.message.contains("PATH"), "{}", error.message);
}

#[tokio::test]
async fn an_unknown_harness_names_the_supported_ones() {
    let root = TempRoot::new();

    let mut config = agent_action("look around");
    config.harness = Some("aider".to_string());

    let error = fail(&AgentExecutor::new(), config, &root.workspace()).await;

    assert_eq!(error.error_type.as_deref(), Some("unknown_harness"));
    assert!(error.message.contains("claude-code"), "{}", error.message);
    assert!(error.message.contains("opencode"), "{}", error.message);
}

// ---------------------------------------------------------------------------
// Through the engine
// ---------------------------------------------------------------------------

/// A store, a workspace and an engine whose agent executor runs `fake`.
struct EngineFixture {
    root: TempRoot,
    fake: FakeHarness,
}

impl EngineFixture {
    fn new() -> Self {
        let root = TempRoot::new();
        let fake = FakeHarness::install(root.dir("bin"));
        root.dir("store");
        root.dir("workspace");
        Self { root, fake }
    }

    fn store(&self) -> ExecutionStore {
        ExecutionStore::new(self.root.path().join("store"))
    }

    fn workspace(&self) -> Workspace {
        Workspace::local_folder(
            self.root
                .path()
                .join("workspace")
                .to_string_lossy()
                .into_owned(),
        )
    }

    /// Copies the §4 fixture scripts into the workspace, the way a real
    /// repository would already contain them.
    fn install_scripts(&self) {
        let scripts = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scripts");
        for entry in fs::read_dir(scripts).expect("read fixture scripts") {
            let entry = entry.expect("fixture script");
            fs::copy(
                entry.path(),
                self.root.path().join("workspace").join(entry.file_name()),
            )
            .expect("copy fixture script");
        }
    }

    /// An engine whose `agent` executor is the fake harness, logging its
    /// session events into the same store as everything else.
    fn engine(&self) -> Engine {
        let store = self.store();
        let executor = AgentExecutor::with_harnesses(HarnessRegistry::defaults().with_adapter(
            Arc::new(ClaudeCodeAdapter::with_program(self.fake.program())),
        ))
        .with_event_sink(Arc::new(LoggingEventSink::new(store.clone(), None)));

        Engine::new(store).with_executor(ActionKind::Agent, Arc::new(executor))
    }

    /// The `type` tag of every event of an execution, in order.
    fn event_types(&self, execution_id: &str) -> Vec<String> {
        self.store()
            .event_log(execution_id)
            .read_all()
            .expect("read the event log")
            .iter()
            .map(|event| event_type(&event.data))
            .collect()
    }

    /// Writes a workflow definition into the root and loads it.
    fn workflow(&self, yaml: &str) -> narsor::domain::WorkflowDefinition {
        let path = self.root.path().join("workflow.yaml");
        fs::write(&path, yaml).expect("write the workflow");
        load_from_file(&path).expect("load the workflow")
    }
}

#[tokio::test]
async fn an_agent_state_transitions_on_the_label_the_agent_returned() {
    let fixture = EngineFixture::new();
    fixture.fake.answers(&envelope(
        r#"{"output":"ready","variables":{"plan":"plan.md"}}"#,
    ));

    let definition = fixture.workflow(
        r#"
name: agent-two-states
version: 1
states:
  analyze:
    action:
      type: agent
      harness: claude-code
      prompt: "Analyse {{ inputs.subject }}"
    outputs: [ready, ambiguous]
    transitions:
      ready: done
      ambiguous: stopped
  done:
    description: Success.
  stopped:
    description: Failure.
terminal_states:
  done: completed
  stopped: failed
inputs:
  - name: subject
    type: string
    required: true
"#,
    );

    let execution = fixture
        .engine()
        .start_and_run(
            &definition,
            json!({ "subject": "the login module" }),
            fixture.workspace(),
        )
        .await
        .expect("run the workflow");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(execution.current_state.as_deref(), Some("done"));
    assert_eq!(
        execution.context.variables.get("plan"),
        Some(&json!("plan.md"))
    );

    // The session events are in the same log as the engine's own.
    let types = fixture.event_types(&execution.id);
    assert!(
        types.contains(&"agent_session_started".to_string()),
        "{types:?}"
    );
    assert!(
        types.contains(&"agent_session_completed".to_string()),
        "{types:?}"
    );

    // The prompt reached the harness expanded.
    let prompt = fixture.fake.args(1).last().expect("a prompt").clone();
    assert!(prompt.starts_with("Analyse the login module"), "{prompt}");
}

/// The engine hands the state's outputs to the executor: the prompt of an
/// agent state lists every declared label and its description, without the
/// workflow's prompt naming a single one (AE15).
#[tokio::test]
async fn the_engine_appends_the_states_labels_and_descriptions_to_the_prompt() {
    let fixture = EngineFixture::new();
    fixture.fake.answers(&envelope(r#"{"output":"ready"}"#));

    let definition = fixture.workflow(
        r#"
name: agent-described-outputs
version: 1
states:
  analyze:
    action:
      type: agent
      harness: claude-code
      prompt: "Analyse the module."
    outputs:
      - label: ready
        description: The analysis is complete.
      - ambiguous
      - label: needs_human_decision
        description: A person has to choose between the options first.
    transitions:
      ready: done
      ambiguous: stopped
      needs_human_decision: stopped
  done:
    description: Success.
  stopped:
    description: Failure.
terminal_states:
  done: completed
  stopped: failed
"#,
    );

    let execution = fixture
        .engine()
        .start_and_run(&definition, json!({}), fixture.workspace())
        .await
        .expect("run the workflow");
    assert_eq!(execution.status, ExecutionStatus::Completed);

    let prompt = fixture.fake.args(1).last().expect("a prompt").clone();
    for line in [
        "- `ready` — The analysis is complete.\n",
        "- `ambiguous`\n",
        "- `needs_human_decision` — A person has to choose between the options first.\n",
    ] {
        assert!(prompt.contains(line), "missing {line:?} in: {prompt}");
    }
}

#[tokio::test]
async fn the_reference_workflow_runs_end_to_end_on_the_fake_harness() {
    let fixture = EngineFixture::new();
    fixture.install_scripts();
    fixture
        .fake
        .answers_on_call(1, &envelope(r#"{"output":"ready"}"#))
        .answers_on_call(2, &envelope(r#"{"output":"implemented"}"#));

    let definition = load_from_file(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/implement_feature.yaml"),
    )
    .expect("load the reference workflow");

    let execution = fixture
        .engine()
        .start_and_run(
            &definition,
            json!({ "feature_description": "add login", "target_repo": "acme/web" }),
            fixture.workspace(),
        )
        .await
        .expect("run the reference workflow");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(execution.current_state.as_deref(), Some("completed"));
    assert_eq!(fixture.fake.calls(), 2, "analyze and implement ran");

    // The script half of the workflow still did its work.
    assert_eq!(
        execution.context.variables.get("pr_number"),
        Some(&json!(123))
    );
}

/// Every `action_execution_id` an `ActionStarted`/`ActionRetrying` event of
/// `execution_id` introduced, in order — one per attempt.
fn attempt_ids(fixture: &EngineFixture, execution_id: &str) -> Vec<String> {
    fixture
        .store()
        .event_log(execution_id)
        .read_all()
        .expect("read the event log")
        .into_iter()
        .filter_map(|event| match event.data {
            EventData::ActionStarted {
                action_execution_id,
                ..
            } => Some(action_execution_id),
            EventData::ActionRetrying {
                action_execution_id,
                ..
            } => Some(action_execution_id),
            _ => None,
        })
        .collect()
}

/// Integration: a retried agent state gets one conversation file per attempt,
/// and both are compressed once the run is over.
#[tokio::test]
async fn a_retried_agent_state_gets_one_compressed_conversation_file_per_attempt() {
    let fixture = EngineFixture::new();
    // A harness failure, not a missing label: a format error is answered
    // with a reprompt rather than a retry (KTD7).
    fixture
        .fake
        .answers_on_call(1, &failed_envelope("the model is overloaded"))
        .answers_on_call(2, &envelope(r#"{"output":"ready"}"#));

    let definition = fixture.workflow(
        r#"
name: agent-retry
version: 1
states:
  analyze:
    action:
      type: agent
      harness: claude-code
      prompt: "Analyse {{ inputs.subject }}"
    outputs: [ready]
    retry_policy:
      max_attempts: 2
      backoff: 0
    transitions:
      ready: done
  done:
    description: Success.
terminal_states:
  done: completed
inputs:
  - name: subject
    type: string
    required: true
"#,
    );

    let execution = fixture
        .engine()
        .start_and_run(
            &definition,
            json!({ "subject": "the login module" }),
            fixture.workspace(),
        )
        .await
        .expect("run the workflow");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(
        fixture.fake.calls(),
        2,
        "the first attempt failed and was retried"
    );

    let attempts = attempt_ids(&fixture, &execution.id);
    assert_eq!(attempts.len(), 2, "one id per attempt: {attempts:?}");
    assert_ne!(
        attempts[0], attempts[1],
        "each attempt has its own id (D24)"
    );

    let conversation_store = fixture.store().conversation_store(&execution.id);
    for attempt in &attempts {
        let snapshot = conversation_store
            .read(attempt, 1, usize::MAX)
            .expect("read the conversation")
            .unwrap_or_else(|| panic!("attempt '{attempt}' has a conversation file"));
        assert!(
            snapshot.closed,
            "attempt '{attempt}' was compressed once its run ended"
        );
        assert!(
            !snapshot.entries.is_empty(),
            "attempt '{attempt}' has at least the prompt and the completion entry"
        );
    }
}

// ---------------------------------------------------------------------------
// Format errors and the engine's reprompt (KTD7)
// ---------------------------------------------------------------------------

/// A two-state agent workflow whose first state allows two attempts, so a
/// test can tell a reprompt from a `retry_policy` retry.
const REPROMPT_WORKFLOW: &str = r#"
name: agent-reprompt
version: 1
states:
  work:
    action:
      type: agent
      harness: claude-code
      prompt: "Implement the feature."
    outputs:
      - label: implemented
        description: The feature is implemented.
      - label: needs_human_decision
        description: A person has to decide first.
    retry_policy:
      max_attempts: 2
      backoff: 0
    transitions:
      implemented: done
      needs_human_decision: stopped
  done:
    description: Success.
  stopped:
    description: Failure.
terminal_states:
  done: completed
  stopped: failed
"#;

/// Every event of an execution, in order.
fn events_of(fixture: &EngineFixture, execution_id: &str) -> Vec<EventData> {
    fixture
        .store()
        .event_log(execution_id)
        .read_all()
        .expect("read the event log")
        .into_iter()
        .map(|event| event.data)
        .collect()
}

/// Whether `args` carries `flag` immediately followed by `value`.
fn has_flag(args: &[String], flag: &str, value: &str) -> bool {
    args.windows(2)
        .any(|pair| pair[0] == flag && pair[1] == value)
}

/// AE7: an answer without `output` is not an action failure. The engine asks
/// the same session again, read-only, as a new attempt — and the state's
/// `retry_policy` never moves.
#[tokio::test]
async fn a_missing_label_is_asked_for_again_in_the_same_read_only_session() {
    let fixture = EngineFixture::new();
    fixture
        .fake
        .answers_on_call(1, &envelope(r#"Done. {"data": {"units_completed": 3}}"#))
        .answers_on_call(2, &envelope(r#"{"output":"implemented"}"#));

    let definition = fixture.workflow(REPROMPT_WORKFLOW);
    let execution = fixture
        .engine()
        .start_and_run(&definition, json!({}), fixture.workspace())
        .await
        .expect("run the workflow");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(fixture.fake.calls(), 2, "one attempt and one reprompt");

    // The reprompt resumes the first call's session, read-only.
    let first = fixture.fake.args(1);
    let second = fixture.fake.args(2);
    assert!(!first.iter().any(|arg| arg == "--resume"), "{first:?}");
    assert!(
        has_flag(&second, "--resume", "fake-session-1"),
        "{second:?}"
    );
    assert!(has_flag(&second, "--permission-mode", "plan"), "{second:?}");

    // The reprompt says what is wrong and lists the labels again.
    let prompt = second.last().expect("a prompt");
    assert!(prompt.contains("output"), "{prompt}");
    assert!(
        prompt.contains("- `implemented` — The feature is implemented.\n"),
        "{prompt}"
    );

    // A new attempt with its own idempotency key.
    assert_ne!(fixture.fake.action_id(1), fixture.fake.action_id(2));

    // Rejection and reprompt are on record; the retry policy is untouched.
    let types = fixture.event_types(&execution.id);
    for absent in ["action_retrying", "action_attempt_failed", "action_failed"] {
        assert!(!types.contains(&absent.to_string()), "{types:?}");
    }
    let events = events_of(&fixture, &execution.id);
    let rejected = events
        .iter()
        .find_map(|data| match data {
            EventData::ActionOutputRejected {
                action_execution_id,
                reason,
                answer_tail,
                ..
            } => Some((
                action_execution_id.clone(),
                reason.clone(),
                answer_tail.clone(),
            )),
            _ => None,
        })
        .expect("an ActionOutputRejected event");
    assert_eq!(rejected.0, fixture.fake.action_id(1));
    assert!(rejected.1.contains("'output'"), "{}", rejected.1);
    assert!(
        rejected
            .2
            .as_deref()
            .is_some_and(|tail| tail.contains("units_completed")),
        "{:?}",
        rejected.2
    );
    let reprompted = events
        .iter()
        .find_map(|data| match data {
            EventData::OutputReprompted {
                action_execution_id,
                previous_attempt,
                by,
                ..
            } => Some((action_execution_id.clone(), previous_attempt.clone(), *by)),
            _ => None,
        })
        .expect("an OutputReprompted event");
    assert_eq!(reprompted.0, fixture.fake.action_id(2));
    assert_eq!(reprompted.1, fixture.fake.action_id(1));
    assert_eq!(reprompted.2, RepromptedBy::Engine);

    // Each attempt has its own conversation file, both closed.
    let conversations = fixture.store().conversation_store(&execution.id);
    for attempt in [fixture.fake.action_id(1), fixture.fake.action_id(2)] {
        let snapshot = conversations
            .read(&attempt, 1, usize::MAX)
            .expect("read the conversation")
            .unwrap_or_else(|| panic!("attempt '{attempt}' has a conversation"));
        assert!(snapshot.closed, "attempt '{attempt}' was compressed");
    }
}

/// Runs [`REPROMPT_WORKFLOW`] on the fixture's engine.
async fn run_reprompt_workflow(fixture: &EngineFixture) -> narsor::domain::WorkflowExecution {
    let definition = fixture.workflow(REPROMPT_WORKFLOW);
    fixture
        .engine()
        .start_and_run(&definition, json!({}), fixture.workspace())
        .await
        .expect("run the workflow")
}

/// The problem the execution waits on (U5): it is `InputNeeded` on a
/// `ResolveOutput` request, announced by `ProblemRaised`, and did not fail.
fn output_problem(
    fixture: &EngineFixture,
    execution: &narsor::domain::WorkflowExecution,
) -> (PendingHumanInput, OutputProblem) {
    assert_eq!(execution.status, ExecutionStatus::InputNeeded);
    let pending: PendingHumanInput = fixture
        .store()
        .load_pending_input(&execution.id)
        .expect("read the pending input")
        .expect("a pending input");
    assert_eq!(pending.continuation, InputContinuation::ResolveOutput);
    let types = fixture.event_types(&execution.id);
    assert!(types.contains(&"problem_raised".to_string()), "{types:?}");
    for absent in ["action_failed", "workflow_failed"] {
        assert!(!types.contains(&absent.to_string()), "{types:?}");
    }
    let problem = pending.problem.clone().expect("the problem record");
    (pending, problem)
}

/// A label the state does not declare is a format error too: asked again,
/// not failed on the spot.
#[tokio::test]
async fn an_undeclared_label_is_asked_for_again_instead_of_failing_at_once() {
    let fixture = EngineFixture::new();
    fixture
        .fake
        .answers_on_call(1, &envelope(r#"{"output":"finished"}"#))
        .answers_on_call(2, &envelope(r#"{"output":"implemented"}"#));

    let execution = run_reprompt_workflow(&fixture).await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(fixture.fake.calls(), 2);
    assert!(has_flag(
        &fixture.fake.args(2),
        "--resume",
        "fake-session-1"
    ));

    let reason = events_of(&fixture, &execution.id)
        .into_iter()
        .find_map(|data| match data {
            EventData::ActionOutputRejected { reason, .. } => Some(reason),
            _ => None,
        })
        .expect("an ActionOutputRejected event");
    assert!(reason.contains("'finished'"), "{reason}");
    let types = fixture.event_types(&execution.id);
    assert!(
        types.contains(&"output_reprompted".to_string()),
        "{types:?}"
    );
    assert!(!types.contains(&"action_retrying".to_string()), "{types:?}");
}

/// A harness that reported no session cannot be asked again: the action fails
/// with the reason, and the retry policy is not used for a format error.
#[tokio::test]
async fn without_a_harness_session_there_is_no_reprompt() {
    let fixture = EngineFixture::new();
    let no_session = json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "result": "All done, nothing to report.",
    })
    .to_string();
    fixture.fake.answers(&no_session);

    let execution = run_reprompt_workflow(&fixture).await;

    assert_eq!(fixture.fake.calls(), 1, "no reprompt and no retry");
    let types = fixture.event_types(&execution.id);
    assert!(
        types.contains(&"action_output_rejected".to_string()),
        "{types:?}"
    );
    for absent in ["output_reprompted", "action_retrying"] {
        assert!(!types.contains(&absent.to_string()), "{types:?}");
    }
    // Phase 2: straight to a problem, with the one answer there was.
    let (_, problem) = output_problem(&fixture, &execution);
    assert!(
        problem.reason.contains("no session to resume"),
        "{}",
        problem.reason
    );
    assert_eq!(problem.answers.len(), 1);
    assert_eq!(
        problem.answers[0].answer_tail.as_deref(),
        Some("All done, nothing to report.")
    );
    assert!(problem.analysis.is_none());
}

/// A reprompt that again yields no label makes the step a problem for the
/// operator (U5) — with both answers — and still no `retry_policy` retry.
#[tokio::test]
async fn a_reprompt_without_a_label_becomes_a_problem_with_both_answers() {
    let fixture = EngineFixture::new();
    fixture
        .fake
        .answers_on_call(1, &envelope("Done, no JSON."))
        .answers_on_call(2, &envelope("I already told you, it is done."));

    let execution = run_reprompt_workflow(&fixture).await;

    assert_eq!(
        fixture.fake.calls(),
        2,
        "one attempt and one reprompt, no more"
    );
    let types = fixture.event_types(&execution.id);
    assert_eq!(
        types
            .iter()
            .filter(|kind| *kind == "action_output_rejected")
            .count(),
        2,
        "{types:?}"
    );
    assert!(!types.contains(&"action_retrying".to_string()), "{types:?}");
    let (pending, problem) = output_problem(&fixture, &execution);
    assert!(
        problem.reason.contains("even when asked again"),
        "{}",
        problem.reason
    );
    let tails: Vec<Option<&str>> = problem
        .answers
        .iter()
        .map(|answer| answer.answer_tail.as_deref())
        .collect();
    assert_eq!(
        tails,
        vec![
            Some("Done, no JSON."),
            Some("I already told you, it is done.")
        ]
    );
    // The request suspends on the reprompt, the last attempt.
    assert_eq!(
        pending.action_execution_id(),
        problem.answers[1].action_execution_id
    );
    assert_eq!(
        pending.request.answer_tail.as_deref(),
        Some("I already told you, it is done.")
    );
    // Both labels are offered with their descriptions.
    let offered: Vec<(&str, Option<&str>)> = pending
        .outputs
        .iter()
        .map(|output| (output.label.as_str(), output.description.as_deref()))
        .collect();
    assert_eq!(
        offered,
        vec![
            ("implemented", Some("The feature is implemented.")),
            (
                "needs_human_decision",
                Some("A person has to decide first.")
            ),
        ]
    );
    let raised = events_of(&fixture, &execution.id)
        .into_iter()
        .find_map(|data| match data {
            EventData::ProblemRaised {
                episode,
                action_execution_id,
                ..
            } => Some((episode, action_execution_id)),
            _ => None,
        })
        .expect("ProblemRaised");
    assert_eq!(raised.0.as_deref(), Some(pending.episode_key()));
    assert_eq!(raised.1, pending.action_execution_id());
}

/// A reprompt whose run itself fails is a failed reprompt too: its attempt
/// is closed and the step becomes a problem, not an action failure.
#[tokio::test]
async fn a_reprompt_that_fails_outright_is_a_problem_too() {
    let fixture = EngineFixture::new();
    fixture
        .fake
        .answers_on_call(1, &envelope("Done, no JSON."))
        .answers_on_call(2, &failed_envelope("the model is overloaded"));

    let execution = run_reprompt_workflow(&fixture).await;

    assert_eq!(fixture.fake.calls(), 2, "no retry of the reprompt");
    let (_, problem) = output_problem(&fixture, &execution);
    assert!(
        problem.reason.contains("asking the agent again"),
        "{}",
        problem.reason
    );
    assert_eq!(problem.answers.len(), 2);
    let rejected = fixture
        .event_types(&execution.id)
        .iter()
        .filter(|kind| *kind == "action_output_rejected")
        .count();
    assert_eq!(rejected, 2, "the reprompt attempt is closed as well");
}

/// KTD7: a question in reply to a reprompt is a failed reprompt, not a
/// suspension that would restart the step in a fresh session.
#[tokio::test]
async fn a_question_in_reply_to_a_reprompt_is_a_failed_reprompt() {
    let fixture = EngineFixture::new();
    fixture
        .fake
        .answers_on_call(1, &envelope("Done, I think."))
        .answers_on_call(
            2,
            &envelope(r#"{"status":"awaiting_input","question":"Which label do you mean?"}"#),
        );

    let execution = run_reprompt_workflow(&fixture).await;

    // Not an agent question (`ResumeAgent`) but a problem about the output.
    let (_, problem) = output_problem(&fixture, &execution);
    assert!(
        problem.reason.contains("Which label do you mean?"),
        "{}",
        problem.reason
    );
}

/// A timeout is an action failure, not a format error: it goes through the
/// state's `retry_policy` exactly as before.
#[tokio::test]
async fn a_timeout_without_a_label_still_uses_the_retry_policy() {
    let fixture = EngineFixture::new();
    fixture
        .fake
        .answers_on_call(1, "")
        .sleeps_on_call(1, 30)
        .answers_on_call(2, &envelope(r#"{"output":"implemented"}"#));

    let definition = fixture.workflow(
        &REPROMPT_WORKFLOW.replace("    retry_policy:\n", "    timeout: 1\n    retry_policy:\n"),
    );
    let execution = fixture
        .engine()
        .start_and_run(&definition, json!({}), fixture.workspace())
        .await
        .expect("run the workflow");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(fixture.fake.calls(), 2);
    assert!(!fixture.fake.args(2).iter().any(|arg| arg == "--resume"));
    let types = fixture.event_types(&execution.id);
    assert!(types.contains(&"action_retrying".to_string()), "{types:?}");
    for absent in ["action_output_rejected", "output_reprompted"] {
        assert!(!types.contains(&absent.to_string()), "{types:?}");
    }
}

/// KTD7: the reprompt resumes the session on the harness that ran the
/// rejected attempt, even when the default harness changed in between.
#[tokio::test]
async fn the_reprompt_runs_on_the_harness_of_the_rejected_attempt() {
    let fixture = EngineFixture::new();
    fixture
        .fake
        .answers_on_call(1, &envelope("Done, no JSON."))
        .sleeps_on_call(1, 1)
        .answers_on_call(2, &envelope(r#"{"output":"implemented"}"#));
    let selector = Arc::new(
        DefaultHarnessSelector::load(
            StatePaths::new(fixture.root.path()),
            vec!["claude-code".into(), "opencode".into()],
            "claude-code",
        )
        .await
        .expect("load selector"),
    );
    let store = fixture.store();
    let executor = AgentExecutor::with_harnesses(
        HarnessRegistry::defaults()
            .with_adapter(Arc::new(ClaudeCodeAdapter::with_program(
                fixture.fake.program(),
            )))
            .with_adapter(Arc::new(OpenCodeAdapter::with_program(
                fixture.fake.program(),
            ))),
    )
    .with_default_harness_selector(Arc::clone(&selector))
    .with_event_sink(Arc::new(LoggingEventSink::new(store.clone(), None)));
    let engine = Engine::new(store).with_executor(ActionKind::Agent, Arc::new(executor));

    let definition =
        fixture.workflow(&REPROMPT_WORKFLOW.replace("      harness: claude-code\n", ""));
    let workspace = fixture.workspace();
    let run = tokio::spawn(async move {
        engine
            .start_and_run(&definition, json!({}), workspace)
            .await
    });
    for _ in 0..100 {
        if fixture.fake.calls() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(fixture.fake.calls(), 1, "the first attempt started");
    selector.set("opencode").await.expect("change the default");

    let execution = run.await.expect("the run joins").expect("run the workflow");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let second = fixture.fake.args(2);
    assert!(second.iter().any(|arg| arg == "--print"), "{second:?}");
    assert!(
        has_flag(&second, "--resume", "fake-session-1"),
        "{second:?}"
    );
}

// ---------------------------------------------------------------------------
// The harness is pinned for the duration of one action (R16a, R16b)
// ---------------------------------------------------------------------------

/// A three-step workflow: `implement` and `review` name no harness,
/// `analyze` names `claude-code`. `implement` may be attempted
/// `implement_attempts` times.
fn pinned_harness_workflow(implement_attempts: u32) -> String {
    format!(
        r#"
name: agent-pinned-harness
version: 1
states:
  implement:
    action:
      type: agent
      prompt: "Implement the feature."
    outputs: [implemented]
    retry_policy:
      max_attempts: {implement_attempts}
      backoff: 0
    transitions:
      implemented: review
  review:
    action:
      type: agent
      prompt: "Review the change."
    outputs: [reviewed]
    transitions:
      reviewed: analyze
  analyze:
    action:
      type: agent
      harness: claude-code
      prompt: "Analyse the result."
    outputs: [analysed]
    transitions:
      analysed: done
  done:
    description: Success.
terminal_states:
  done: completed
"#
    )
}

/// A selector over claude-code, opencode and codex starting on claude-code,
/// and an engine whose three adapters all run the fixture's fake harness.
async fn engine_with_live_default(
    fixture: &EngineFixture,
) -> (Engine, Arc<DefaultHarnessSelector>) {
    let selector = Arc::new(
        DefaultHarnessSelector::load(
            StatePaths::new(fixture.root.path()),
            vec!["claude-code".into(), "opencode".into(), "codex".into()],
            "claude-code",
        )
        .await
        .expect("load selector"),
    );
    let store = fixture.store();
    let executor = AgentExecutor::with_harnesses(
        HarnessRegistry::defaults()
            .with_adapter(Arc::new(ClaudeCodeAdapter::with_program(
                fixture.fake.program(),
            )))
            .with_adapter(Arc::new(OpenCodeAdapter::with_program(
                fixture.fake.program(),
            )))
            .with_adapter(Arc::new(CodexAdapter::with_program(fixture.fake.program()))),
    )
    .with_default_harness_selector(Arc::clone(&selector))
    .with_event_sink(Arc::new(LoggingEventSink::new(store.clone(), None)));
    let engine = Engine::new(store).with_executor(ActionKind::Agent, Arc::new(executor));
    (engine, selector)
}

/// Waits until the fake harness has been called `calls` times.
async fn wait_for_calls(fixture: &EngineFixture, calls: usize) {
    for _ in 0..200 {
        if fixture.fake.calls() >= calls {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(fixture.fake.calls(), calls, "call {calls} started");
}

/// The harness of every `AgentSessionStarted` of an execution, in order.
fn started_harnesses(fixture: &EngineFixture, execution_id: &str) -> Vec<String> {
    events_of(fixture, execution_id)
        .into_iter()
        .filter_map(|data| match data {
            EventData::AgentSessionStarted { harness, .. } => Some(harness),
            _ => None,
        })
        .collect()
}

/// AE5: `implement` starts on claude-code, the default switches to opencode
/// while it runs, the attempt fails and is retried — on claude-code. The
/// next harnessless step, `review`, takes opencode; `analyze`, which names
/// claude-code, stays on it.
#[tokio::test]
async fn covers_ae5_a_retry_stays_on_the_harness_its_action_started_with() {
    let fixture = EngineFixture::new();
    fixture
        .fake
        .answers_on_call(1, &failed_envelope("the model is overloaded"))
        .sleeps_on_call(1, 1)
        .answers_on_call(2, &envelope(r#"{"output":"implemented"}"#))
        .answers_on_call(3, &opencode_stream("oc-1", r#"{"output":"reviewed"}"#))
        .answers_on_call(4, &envelope(r#"{"output":"analysed"}"#));
    let (engine, selector) = engine_with_live_default(&fixture).await;
    let definition = fixture.workflow(&pinned_harness_workflow(2));
    let workspace = fixture.workspace();
    let run = tokio::spawn(async move {
        engine
            .start_and_run(&definition, json!({}), workspace)
            .await
    });

    wait_for_calls(&fixture, 1).await;
    selector.set("opencode").await.expect("change the default");
    let execution = run.await.expect("the run joins").expect("run the workflow");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(
        started_harnesses(&fixture, &execution.id),
        ["claude-code", "claude-code", "opencode", "claude-code"]
    );
    let retry = fixture.fake.args(2);
    assert!(retry.iter().any(|arg| arg == "--print"), "{retry:?}");
    let review = fixture.fake.args(3);
    assert!(review.iter().any(|arg| arg == "run"), "{review:?}");
    let analyze = fixture.fake.args(4);
    assert!(analyze.iter().any(|arg| arg == "--print"), "{analyze:?}");
}

/// R16a: however often the default changes during one action, the action
/// and every one of its retries stay on the harness it started with.
#[tokio::test]
async fn a_default_changed_twice_during_an_action_leaves_its_retries_on_the_original() {
    let fixture = EngineFixture::new();
    fixture
        .fake
        .answers_on_call(1, &failed_envelope("the model is overloaded"))
        .sleeps_on_call(1, 1)
        .answers_on_call(2, &failed_envelope("the model is still overloaded"))
        .sleeps_on_call(2, 1)
        .answers_on_call(3, &envelope(r#"{"output":"implemented"}"#))
        .answers_on_call(4, &codex_stream("thread-1", r#"{"output":"reviewed"}"#))
        .answers_on_call(5, &envelope(r#"{"output":"analysed"}"#));
    let (engine, selector) = engine_with_live_default(&fixture).await;
    let definition = fixture.workflow(&pinned_harness_workflow(3));
    let workspace = fixture.workspace();
    let run = tokio::spawn(async move {
        engine
            .start_and_run(&definition, json!({}), workspace)
            .await
    });

    wait_for_calls(&fixture, 1).await;
    selector.set("opencode").await.expect("change the default");
    wait_for_calls(&fixture, 2).await;
    selector
        .set("codex")
        .await
        .expect("change the default again");
    let execution = run.await.expect("the run joins").expect("run the workflow");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(
        started_harnesses(&fixture, &execution.id),
        [
            "claude-code",
            "claude-code",
            "claude-code",
            "codex",
            "claude-code"
        ]
    );
}

/// Restart matrix: the reprompt continued after a restart runs on the
/// harness the rejected attempt recorded, not on the default current when
/// the daemon came back.
#[tokio::test]
async fn a_reprompt_after_a_restart_runs_on_the_recorded_harness_not_the_new_default() {
    let fixture = EngineFixture::new();
    fixture
        .fake
        .answers(&envelope(r#"{"output":"implemented"}"#));
    let (engine, selector) = engine_with_live_default(&fixture).await;
    let definition =
        fixture.workflow(&REPROMPT_WORKFLOW.replace("      harness: claude-code\n", ""));
    let store = fixture.store();
    let mut execution = engine
        .start(&definition, json!({}), fixture.workspace())
        .expect("start");

    let log = store.event_log(&execution.id);
    for data in [
        EventData::StateEntered {
            state_id: "work".to_string(),
        },
        EventData::ActionStarted {
            action_execution_id: "attempt-1".to_string(),
            state_id: "work".to_string(),
            action_type: "agent".to_string(),
        },
        EventData::ActionOutputRejected {
            action_execution_id: "attempt-1".to_string(),
            state_id: "work".to_string(),
            reason: "no output label".to_string(),
            answer_tail: None,
        },
    ] {
        log.append_next(&execution.id, data).expect("append");
    }
    let rejection = OutputRejection {
        state_id: "work".to_string(),
        action_execution_id: "attempt-1".to_string(),
        reason: "no output label".to_string(),
        answer_tail: None,
        harness: Some("claude-code".to_string()),
        harness_session_id: Some("session-7".to_string()),
    };
    execution.current_state = Some("work".to_string());
    execution.metadata = json!({
        METADATA_ENGINE_KEY: { METADATA_OUTPUT_REJECTION_KEY: rejection },
    });
    store
        .save_execution(&execution)
        .expect("save the execution");
    recover(&store).expect("recover");

    // The operator switched the default while the daemon was down.
    selector.set("opencode").await.expect("change the default");
    let execution = engine
        .resume(&definition, &execution.id)
        .await
        .expect("resume");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let args = fixture.fake.args(1);
    assert!(args.iter().any(|arg| arg == "--print"), "{args:?}");
    assert!(has_flag(&args, "--resume", "session-7"), "{args:?}");
}

/// Restart matrix: a daemon that stopped after `ActionOutputRejected` and
/// before the reprompt started leaves nothing in flight. Startup recovery
/// keeps the execution `Running`, and the next pass continues with the
/// reprompt instead of running the state's action afresh.
#[tokio::test]
async fn a_restart_between_rejection_and_reprompt_continues_with_the_reprompt() {
    let fixture = EngineFixture::new();
    fixture
        .fake
        .answers(&envelope(r#"{"output":"implemented"}"#));
    let store = fixture.store();
    let engine = fixture.engine();
    let definition = fixture.workflow(REPROMPT_WORKFLOW);
    let mut execution = engine
        .start(&definition, json!({}), fixture.workspace())
        .expect("start");

    // What the stopped daemon left behind: the attempt closed by its
    // rejection, and the engine's record of it.
    let log = store.event_log(&execution.id);
    for data in [
        EventData::StateEntered {
            state_id: "work".to_string(),
        },
        EventData::ActionStarted {
            action_execution_id: "attempt-1".to_string(),
            state_id: "work".to_string(),
            action_type: "agent".to_string(),
        },
        EventData::ActionOutputRejected {
            action_execution_id: "attempt-1".to_string(),
            state_id: "work".to_string(),
            reason: "no output label".to_string(),
            answer_tail: None,
        },
    ] {
        log.append_next(&execution.id, data).expect("append");
    }
    let rejection = OutputRejection {
        state_id: "work".to_string(),
        action_execution_id: "attempt-1".to_string(),
        reason: "no output label".to_string(),
        answer_tail: None,
        harness: Some("claude-code".to_string()),
        harness_session_id: Some("session-7".to_string()),
    };
    execution.current_state = Some("work".to_string());
    execution.metadata = json!({
        METADATA_ENGINE_KEY: { METADATA_OUTPUT_REJECTION_KEY: rejection },
    });
    store
        .save_execution(&execution)
        .expect("save the execution");

    let report = recover(&store).expect("recover");
    let verdict = report
        .reconciled
        .iter()
        .find(|entry| entry.execution_id == execution.id)
        .expect("the execution was reconciled");
    assert_eq!(verdict.outcome, ReconciliationOutcome::Consistent);
    assert_eq!(verdict.status, ExecutionStatus::Running);

    let execution = engine
        .resume(&definition, &execution.id)
        .await
        .expect("resume");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(fixture.fake.calls(), 1, "only the reprompt ran");
    let args = fixture.fake.args(1);
    assert!(has_flag(&args, "--resume", "session-7"), "{args:?}");
    assert!(has_flag(&args, "--permission-mode", "plan"), "{args:?}");
    let types = fixture.event_types(&execution.id);
    assert_eq!(
        types
            .iter()
            .filter(|kind| *kind == "action_started")
            .count(),
        1,
        "the state's action was not run afresh: {types:?}"
    );
    assert!(
        types.contains(&"output_reprompted".to_string()),
        "{types:?}"
    );
}

// ---------------------------------------------------------------------------
// Streaming capture (U3): every line reaches the conversation file
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_claude_code_stream_is_captured_in_order_with_the_prompt_and_a_completion_entry() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    let conversation = ConversationFixture::new(&root);
    fake.answers(&claude_stream("fake-session-1", r#"{"output":"ready"}"#));

    let result = execute_with_conversation(
        &executor(&fake),
        agent_action("look around"),
        &root.workspace(),
        &conversation,
        None,
    )
    .await
    .expect("the run completes");

    let ActionOutcome::Completed(result) = result else {
        panic!("expected a completed action");
    };
    assert_eq!(result.output_label, "ready");

    let entries = conversation.entries(ACTION_EXECUTION_ID);
    // Prompt, four stdout lines, one completion entry.
    assert_eq!(entries.len(), 6, "{entries:?}");
    assert_eq!(entries[0].source, ConversationSource::Narsor);
    assert!(entry_text(&entries[0]).contains("look around"));

    for entry in &entries[1..5] {
        assert_eq!(entry.source, ConversationSource::Stdout);
    }
    assert!(
        entry_text(&entries[1]).contains("\"init\""),
        "the first stdout line is the system/init line: {}",
        entry_text(&entries[1])
    );
    assert!(
        entry_text(&entries[4]).contains("\"result\""),
        "the last stdout line is the result line: {}",
        entry_text(&entries[4])
    );

    let last = entries.last().expect("a completion entry");
    assert_eq!(last.source, ConversationSource::Narsor);
    assert!(
        entry_text(last).contains("exited with code 0"),
        "{}",
        entry_text(last)
    );

    // Closing (and compressing) the attempt is the runner's job, done once
    // the whole attempt — executor included — is over (U3's Approach); a
    // bare executor call like this one never closes it itself. See
    // `a_retried_agent_state_gets_one_compressed_conversation_file_per_attempt`
    // for the runner actually doing it.
    assert!(!conversation.is_closed(ACTION_EXECUTION_ID));
}

#[tokio::test]
async fn an_opencode_stream_is_captured_and_the_label_comes_from_its_text_line() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    let conversation = ConversationFixture::new(&root);
    fake.answers(&opencode_stream("ses_7", r#"{"output":"ready"}"#));

    let executor = AgentExecutor::with_harnesses(
        HarnessRegistry::defaults()
            .with_adapter(Arc::new(OpenCodeAdapter::with_program(fake.program()))),
    );

    let mut config = agent_action("look around");
    config.harness = Some("opencode".to_string());

    let result =
        execute_with_conversation(&executor, config, &root.workspace(), &conversation, None)
            .await
            .expect("the run completes");

    let ActionOutcome::Completed(result) = result else {
        panic!("expected a completed action");
    };
    assert_eq!(result.output_label, "ready");
    assert_eq!(
        result.variables.get(SESSION_ID_VARIABLE),
        Some(&json!("ses_7"))
    );

    let entries = conversation.entries(ACTION_EXECUTION_ID);
    // Prompt, four stdout lines, one completion entry.
    assert_eq!(entries.len(), 6, "{entries:?}");
    assert!(!conversation.is_closed(ACTION_EXECUTION_ID));
}

#[tokio::test]
async fn covers_ae5_two_lines_then_a_timeout_are_both_captured_plus_a_timeout_entry() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    let conversation = ConversationFixture::new(&root);
    fake.answers(&ndjson(&[
        json!({ "type": "system", "subtype": "init", "session_id": "s1" }),
        json!({ "type": "assistant", "message": { "content": [{ "type": "text", "text": "thinking" }] } }),
    ]))
    .sleeps(30);

    let sink = Arc::new(RecordingSink::default());
    let executor = executor(&fake).with_event_sink(sink.clone());

    let outcome = execute_with_conversation(
        &executor,
        agent_action("think for a long time"),
        &root.workspace(),
        &conversation,
        Some(Duration::from_millis(300)),
    )
    .await
    .expect("a timeout is an outcome, not a failure");

    assert!(matches!(outcome, ActionOutcome::TimedOut(_)));

    let failed = sink
        .events()
        .into_iter()
        .find_map(|(_, data)| match data {
            EventData::AgentSessionFailed {
                action_execution_id,
                ..
            } => Some(action_execution_id),
            _ => None,
        })
        .expect("a session failure was recorded");
    assert_eq!(failed.as_deref(), Some(ACTION_EXECUTION_ID));

    let entries = conversation.entries(ACTION_EXECUTION_ID);
    // Prompt, two stdout lines, one "timed out" completion entry.
    assert_eq!(entries.len(), 4, "{entries:?}");
    assert!(entry_text(&entries[1]).contains("\"init\""));
    assert!(entry_text(&entries[2]).contains("thinking"));
    assert!(
        entry_text(entries.last().expect("a completion entry")).contains("timed out"),
        "{}",
        entry_text(entries.last().unwrap())
    );
}

#[tokio::test]
async fn a_stream_with_no_result_line_is_an_error() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&ndjson(&[
        json!({ "type": "system", "subtype": "init", "session_id": "s1" }),
        json!({ "type": "assistant", "message": { "content": [{ "type": "text", "text": "done, I think" }] } }),
    ]));

    let error = fail(
        &executor(&fake),
        agent_action("look around"),
        &root.workspace(),
    )
    .await;

    assert_eq!(error.error_type.as_deref(), Some("harness_failed"));
    assert!(
        error.message.contains("finished without a result"),
        "{}",
        error.message
    );
}

#[tokio::test]
async fn an_is_error_result_at_subtype_success_is_a_session_error() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(
        &json!({
            "type": "result",
            "subtype": "success",
            "is_error": true,
            "session_id": "s1",
            "result": "something went wrong internally",
        })
        .to_string(),
    );

    let error = fail(
        &executor(&fake),
        agent_action("look around"),
        &root.workspace(),
    )
    .await;

    assert_eq!(error.error_type.as_deref(), Some("harness_failed"));
    assert!(
        error.message.contains("something went wrong internally"),
        "{}",
        error.message
    );
}

#[tokio::test]
async fn error_max_turns_builds_its_message_from_the_errors_array() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(
        &json!({
            "type": "result",
            "subtype": "error_max_turns",
            "session_id": "s1",
            "errors": [{ "message": "exceeded the maximum number of turns" }],
        })
        .to_string(),
    );

    let error = fail(
        &executor(&fake),
        agent_action("look around"),
        &root.workspace(),
    )
    .await;

    assert_eq!(error.error_type.as_deref(), Some("harness_failed"));
    assert!(
        error
            .message
            .contains("exceeded the maximum number of turns"),
        "{}",
        error.message
    );
}

#[tokio::test]
async fn a_missing_binary_still_leaves_a_conversation_file_with_the_prompt() {
    let root = TempRoot::new();
    let conversation = ConversationFixture::new(&root);

    let executor =
        AgentExecutor::with_harnesses(HarnessRegistry::defaults().with_adapter(Arc::new(
            ClaudeCodeAdapter::with_program("claude-that-is-not-installed"),
        )));

    let outcome = execute_with_conversation(
        &executor,
        agent_action("look around"),
        &root.workspace(),
        &conversation,
        None,
    )
    .await;
    assert!(outcome.is_err());

    let entries = conversation.entries(ACTION_EXECUTION_ID);
    assert_eq!(entries.len(), 2, "{entries:?}");
    assert!(entry_text(&entries[0]).contains("look around"));
    assert!(
        entry_text(&entries[1]).contains("harness did not start"),
        "{}",
        entry_text(&entries[1])
    );
}

#[tokio::test]
async fn an_oversized_tool_result_line_does_not_truncate_the_capture_or_break_the_label() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    let conversation = ConversationFixture::new(&root);

    // Comfortably past the 8 MiB the old whole-buffer capture used to cap at.
    let huge_content = "x".repeat(9 * 1024 * 1024);
    let huge_line = json!({
        "type": "user",
        "message": { "content": [{ "type": "tool_result", "content": huge_content }] },
    })
    .to_string();
    let result_line = envelope(r#"{"output":"ready"}"#);
    fake.answers(&format!("{huge_line}\n{result_line}"));

    let result = execute_with_conversation(
        &executor(&fake),
        agent_action("look around"),
        &root.workspace(),
        &conversation,
        None,
    )
    .await
    .expect("the run completes");

    let ActionOutcome::Completed(result) = result else {
        panic!("expected a completed action");
    };
    assert_eq!(result.output_label, "ready");

    let entries = conversation.entries(ACTION_EXECUTION_ID);
    let big_entry = entries
        .iter()
        .find(|entry| entry.raw.len() > 9 * 1024 * 1024)
        .expect("the oversized line was captured in full");
    assert_eq!(big_entry.raw.len(), huge_line.len());
}

#[tokio::test]
async fn a_non_json_line_is_captured_raw_and_does_not_abort_parsing() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    let conversation = ConversationFixture::new(&root);
    fake.answers(&format!(
        "this is not JSON at all\n{}",
        envelope(r#"{"output":"ready"}"#)
    ));

    let result = execute_with_conversation(
        &executor(&fake),
        agent_action("look around"),
        &root.workspace(),
        &conversation,
        None,
    )
    .await
    .expect("the run completes");

    let ActionOutcome::Completed(result) = result else {
        panic!("expected a completed action");
    };
    assert_eq!(result.output_label, "ready");

    let entries = conversation.entries(ACTION_EXECUTION_ID);
    assert!(
        entries
            .iter()
            .any(|entry| entry_text(entry) == "this is not JSON at all"),
        "{entries:?}"
    );
}

#[tokio::test]
async fn stderr_on_a_successful_run_is_captured_without_affecting_the_result() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    let conversation = ConversationFixture::new(&root);
    fake.answers(&envelope(r#"{"output":"ready"}"#))
        .fails(0, "a warning the harness printed on stderr\n");

    let result = execute_with_conversation(
        &executor(&fake),
        agent_action("look around"),
        &root.workspace(),
        &conversation,
        None,
    )
    .await
    .expect("the run completes");

    let ActionOutcome::Completed(result) = result else {
        panic!("expected a completed action");
    };
    assert_eq!(result.output_label, "ready");

    let entries = conversation.entries(ACTION_EXECUTION_ID);
    let stderr_entry = entries
        .iter()
        .find(|entry| entry.source == ConversationSource::Stderr)
        .expect("a stderr entry was captured");
    assert!(
        entry_text(stderr_entry).contains("a warning the harness printed"),
        "{}",
        entry_text(stderr_entry)
    );
}

#[tokio::test]
async fn a_resumed_session_reaches_the_argv_and_the_prompt_entry() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    let conversation = ConversationFixture::new(&root);
    fake.answers(&envelope(r#"{"output":"ready"}"#));

    let mut config = agent_action("carry on");
    config
        .parameters
        .insert("resume_session".to_string(), json!("session-42"));

    let result = execute_with_conversation(
        &executor(&fake),
        config,
        &root.workspace(),
        &conversation,
        None,
    )
    .await
    .expect("the run completes");
    assert!(matches!(result, ActionOutcome::Completed(_)));

    let args = fake.args(1);
    assert!(
        args.windows(2)
            .any(|pair| pair[0] == "--resume" && pair[1] == "session-42"),
        "{args:?}"
    );

    let entries = conversation.entries(ACTION_EXECUTION_ID);
    assert!(
        entry_text(&entries[0]).contains("session-42"),
        "the prompt entry names the resumed session: {}",
        entry_text(&entries[0])
    );
}

// ---------------------------------------------------------------------------
// The real harness — deliberately ignored
// ---------------------------------------------------------------------------

/// Drives the actual `claude` binary once, end to end.
///
/// Ignored by default: it costs money, needs credentials and is not
/// deterministic. Run it on purpose with `cargo test -- --ignored`, and read it
/// as the answer to "does the contract built by [`output_contract`] actually
/// work on a real model?".
#[tokio::test]
#[ignore = "drives the real `claude` binary: costs money, needs credentials"]
async fn the_real_claude_binary_honours_the_output_contract() {
    if which("claude").is_none() {
        eprintln!("skipping: `claude` is not on PATH");
        return;
    }

    let root = TempRoot::new();
    let mut config = agent_action(
        "Do not use any tools. Answer with the label 'ready' and nothing else \
         beyond the required JSON object.",
    );
    config.workspace_access = WorkspaceAccess::ReadOnly;

    let result = complete(&AgentExecutor::new(), config, &root.workspace()).await;

    assert_eq!(result.output_label, "ready");
}

/// The first entry of `PATH` holding an executable called `name`.
fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

// ---------------------------------------------------------------------------
// Argument safety
// ---------------------------------------------------------------------------

/// Every value passed in a flag-value position must reject an option-like
/// spelling, so a `model` beginning with `-` cannot become another flag.
#[tokio::test]
async fn a_model_that_would_be_read_as_a_flag_is_refused() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope(r#"{"output":"ready"}"#));

    let mut config = agent_action("do the thing");
    config.model = Some("--allow-dangerously-skip-permissions".to_string());

    let error = fail(&executor(&fake), config, &root.workspace()).await;

    assert_eq!(error.error_type.as_deref(), Some("unsafe_harness_argument"));
    assert!(
        error.message.contains("model"),
        "the error must name the offending field: {}",
        error.message
    );
}

// ---------------------------------------------------------------------------
// The configured defaults (U7)
// ---------------------------------------------------------------------------

/// An action that names no harness uses the configured default.
#[tokio::test]
async fn an_action_without_a_harness_uses_the_configured_default() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope("{\"output\": \"done\"}"));

    // The default points at `opencode`, and that adapter is the one wired to
    // the fake harness — so a run proves which name was chosen.
    let executor = AgentExecutor::with_harnesses(
        HarnessRegistry::defaults()
            .with_adapter(Arc::new(OpenCodeAdapter::with_program(fake.program()))),
    )
    .with_default_harness("opencode");

    let mut config = agent_action("do the thing");
    config.harness = None;

    let outcome = execute(&executor, config, &root.workspace(), None)
        .await
        .expect("the action runs on the configured harness");

    assert!(matches!(outcome, ActionOutcome::Completed(_)));
    assert_eq!(fake.calls(), 1);
}

/// With nothing configured, an action that names no harness gets the built-in.
#[tokio::test]
async fn an_action_without_a_harness_falls_back_to_the_built_in_one() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope("{\"output\": \"done\"}"));

    // `claude-code` is the built-in default and the adapter wired to the fake.
    let executor = executor(&fake);
    assert_eq!(executor.default_harness(), "claude-code");

    let mut config = agent_action("do the thing");
    config.harness = None;

    let outcome = execute(&executor, config, &root.workspace(), None)
        .await
        .expect("the action runs on the built-in harness");

    assert!(matches!(outcome, ActionOutcome::Completed(_)));
}

/// An action that names a harness keeps it, whatever the configuration says.
#[tokio::test]
async fn the_workflow_wins_over_the_configured_harness() {
    // AE4 / R13: a setting is a default, never an instruction the workflow
    // cannot override.
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope("{\"output\": \"done\"}"));

    let executor = executor(&fake).with_default_harness("opencode");

    // The action says claude-code, which is the adapter pointed at the fake;
    // the configured opencode adapter is the stock one and would not answer.
    let config = agent_action("do the thing");

    let outcome = execute(&executor, config, &root.workspace(), None)
        .await
        .expect("the harness the workflow named is the one that ran");

    assert!(matches!(outcome, ActionOutcome::Completed(_)));
    assert_eq!(fake.calls(), 1);
}

/// An explicit action harness remains authoritative when the daemon default is
/// changed live.
#[tokio::test]
async fn an_explicit_harness_wins_over_the_live_default_selector() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope("{\"output\": \"done\"}"));
    let selector = Arc::new(
        DefaultHarnessSelector::load(
            StatePaths::new(root.path()),
            vec!["claude-code".into(), "opencode".into()],
            "opencode",
        )
        .await
        .expect("load selector"),
    );
    let executor = AgentExecutor::with_harnesses(
        HarnessRegistry::defaults()
            .with_adapter(Arc::new(ClaudeCodeAdapter::with_program(fake.program()))),
    )
    .with_default_harness_selector(selector);

    let outcome = execute(
        &executor,
        agent_action("do the thing"),
        &root.workspace(),
        None,
    )
    .await
    .expect("the explicit Claude harness runs");

    assert!(matches!(outcome, ActionOutcome::Completed(_)));
    assert!(fake.args(1).iter().any(|argument| argument == "--print"));
}

/// The executor reads the selector once as a harnessless action starts: a
/// later mutation affects the next action, never one already launched. (The
/// engine pins that reading for every retry of the action — see
/// `covers_ae5_a_retry_stays_on_the_harness_its_action_started_with`.)
#[tokio::test]
async fn live_default_changes_apply_to_the_next_harnessless_action() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope("{\"output\": \"done\"}")).sleeps(1);
    let selector = Arc::new(
        DefaultHarnessSelector::load(
            StatePaths::new(root.path()),
            vec!["claude-code".into(), "opencode".into()],
            "claude-code",
        )
        .await
        .expect("load selector"),
    );
    let executor = AgentExecutor::with_harnesses(
        HarnessRegistry::defaults()
            .with_adapter(Arc::new(ClaudeCodeAdapter::with_program(fake.program())))
            .with_adapter(Arc::new(OpenCodeAdapter::with_program(fake.program()))),
    )
    .with_default_harness_selector(Arc::clone(&selector));
    let mut config = agent_action("do the thing");
    config.harness = None;

    let first_executor = executor.clone();
    let first_workspace = root.workspace();
    let first_config = config.clone();
    let first = tokio::spawn(async move {
        execute(&first_executor, first_config, &first_workspace, None).await
    });
    for _ in 0..100 {
        if fake.calls() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(fake.calls(), 1, "the first harness process started");
    selector.set("opencode").await.expect("change default");
    first
        .await
        .expect("the first task joins")
        .expect("the old default attempt completes");
    execute(&executor, config, &root.workspace(), None)
        .await
        .expect("the new default runs");

    assert!(fake.args(1).iter().any(|argument| argument == "--print"));
    assert!(fake.args(2).iter().any(|argument| argument == "run"));
}

/// A bare resume id cannot be safely associated with a provider after the
/// default has changed.
#[tokio::test]
async fn resuming_a_session_requires_an_explicit_harness() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    let mut config = agent_action("continue");
    config.harness = None;
    config
        .parameters
        .insert("resume_session".to_string(), json!("thread-42"));

    let error = fail(&executor(&fake), config, &root.workspace()).await;

    assert_eq!(error.error_type.as_deref(), Some("invalid_parameter"));
    assert!(
        error.message.contains("explicit 'harness'"),
        "{}",
        error.message
    );
    assert_eq!(fake.calls(), 0);
}

/// The session event records the harness that actually ran.
#[tokio::test]
async fn the_session_event_records_the_harness_that_ran_not_the_absence_of_one() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope("{\"output\": \"done\"}"));

    let sink = Arc::new(RecordingSink::default());
    let executor = executor(&fake).with_event_sink(Arc::clone(&sink) as Arc<dyn AgentEventSink>);

    let mut config = agent_action("do the thing");
    config.harness = None;

    execute(&executor, config, &root.workspace(), None)
        .await
        .expect("the action runs");

    let started: Vec<String> = sink
        .events()
        .into_iter()
        .filter_map(|(_, data)| match data {
            EventData::AgentSessionStarted { harness, .. } => Some(harness),
            _ => None,
        })
        .collect();
    assert_eq!(
        started,
        vec!["claude-code".to_string()],
        "the event carries the harness that ran, not 'unspecified'"
    );
}

// ---------------------------------------------------------------------------
// Characterization for the portable runner (U3)
// ---------------------------------------------------------------------------

/// A harness process inherits the environment of the process that runs the
/// executor. The remote runner relies on exactly that to give every harness
/// the task's private `HOME` and its `env`-delivered credentials, without
/// the executor knowing about either.
#[tokio::test]
async fn the_harness_inherits_the_environment_of_the_runner_process() {
    let root = TempRoot::new();
    let bin = root.dir("bin");
    let program = bin.join("env_harness.sh");
    let temp = bin.join(format!(".env_harness.{}.tmp", uuid::Uuid::new_v4()));
    fs::write(
        &temp,
        format!(
            "#!/bin/sh\nprintf '%s' \"$HOME\" > '{}'\nprintf '%s\\n' '{}'\n",
            bin.join("home.out").display(),
            envelope("{\"output\": \"done\"}")
        ),
    )
    .expect("write the harness");
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    fs::rename(&temp, &program).expect("move the harness into place");

    let executor =
        AgentExecutor::with_harnesses(HarnessRegistry::defaults().with_adapter(Arc::new(
            ClaudeCodeAdapter::with_program(program.display().to_string()),
        )));
    let outcome = execute(
        &executor,
        agent_action("do the thing"),
        &root.workspace(),
        None,
    )
    .await
    .expect("the harness runs");
    assert!(matches!(outcome, ActionOutcome::Completed(_)));
    assert_eq!(
        fs::read_to_string(bin.join("home.out")).expect("the harness wrote HOME"),
        std::env::var("HOME").unwrap_or_default()
    );
}
