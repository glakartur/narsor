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
    ActionConfig, ActionError, AgentActionConfig, Context, EventData, ExecutionStatus, Workspace,
};
use narsor::engine::actions::agent::{AgentEventSink, SESSION_ID_VARIABLE};
use narsor::engine::{
    ActionExecutor, ActionKind, ActionOutcome, ActionRequest, AgentExecutor, ClaudeCodeAdapter,
    Engine, HarnessRegistry, LoggingEventSink, OpenCodeAdapter,
};
use narsor::persistence::ExecutionStore;
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
    fn install(dir: PathBuf) -> Self {
        let source =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/harness/fake_harness.sh");
        let program = dir.join("fake_harness.sh");
        fs::copy(&source, &program).expect("copy the fake harness");
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

/// A `claude --output-format json` result envelope carrying `text`.
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

/// An agent action on the `claude-code` harness.
fn agent_action(prompt: &str) -> AgentActionConfig {
    AgentActionConfig {
        harness: Some("claude-code".to_string()),
        model: None,
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
            .any(|pair| pair[0] == "--output-format" && pair[1] == "json"),
        "{args:?}"
    );
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
async fn the_permission_mode_is_configurable_per_action() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope(r#"{"output":"ready"}"#));

    let mut config = agent_action("look around");
    config
        .parameters
        .insert("permission_mode".to_string(), json!("bypassPermissions"));

    complete(&executor(&fake), config, &root.workspace()).await;

    let args = fake.args(1);
    assert!(
        args.contains(&"--allow-dangerously-skip-permissions".to_string()),
        "the bypass mode needs the CLI's own opt-in flag: {args:?}"
    );
    assert_eq!(args[args.len() - 2], "bypassPermissions");
}

#[tokio::test]
async fn an_unknown_permission_mode_is_rejected_before_anything_runs() {
    let root = TempRoot::new();
    let fake = FakeHarness::install(root.dir("bin"));
    fake.answers(&envelope(r#"{"output":"ready"}"#));

    let mut config = agent_action("look around");
    config
        .parameters
        .insert("permission_mode".to_string(), json!("yolo"));

    let error = fail(&executor(&fake), config, &root.workspace()).await;

    assert_eq!(error.error_type.as_deref(), Some("invalid_parameter"));
    assert!(error.message.contains("acceptEdits"), "{}", error.message);
    assert_eq!(fake.calls(), 0, "nothing may be run on a bad parameter");
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
    config
        .parameters
        .insert("auto_approve".to_string(), json!(true));

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

// ---------------------------------------------------------------------------
// The real harness — deliberately ignored
// ---------------------------------------------------------------------------

/// Drives the actual `claude` binary once, end to end.
///
/// Ignored by default: it costs money, needs credentials and is not
/// deterministic. Run it on purpose with `cargo test -- --ignored`, and read it
/// as the answer to "does the contract in [`AGENT_OUTPUT_CONTRACT`] actually
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
    config
        .parameters
        .insert("permission_mode".to_string(), json!("plan"));

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

/// `permission_mode` is checked against a whitelist, which is the field a
/// reviewer reads before trusting a workflow. Every other value was pushed into
/// a flag-value position raw, so a `model` beginning with `-` became another
/// flag — and `--allow-dangerously-skip-permissions` smuggled through `model`
/// would have escalated straight past that review.
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
