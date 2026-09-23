//! `AgentAction` executor (`WORKFLOW_SCHEMA.md` §3.3.A, decisions D17–D20).
//!
//! Runs an AI coding agent in the execution's workspace through one of the
//! harness adapters in [`super::harness`], and turns what the agent said into
//! an output label.
//!
//! # The three jobs
//!
//! 1. **Ask properly.** The state's prompt is sent with
//!    [`AGENT_OUTPUT_CONTRACT`] appended, so the agent is told — in the same
//!    breath as the task — that it has to finish with a JSON object carrying
//!    `output`. D19 calls this "forcing structure on the agent"; forcing it
//!    starts with saying it.
//! 2. **Read robustly, guess never.** Agents wrap JSON in prose, in fences, or
//!    in both. [`extract_output`] takes the *last* JSON object anywhere in the
//!    answer that has an `output` key. When there is none, the action fails
//!    ([`ActionError`]) — it never falls back to `success`. In an unattended
//!    pipeline a guessed label is a silently wrong transition, which is a far
//!    worse outcome than a failure an operator can see. D20's tolerance for a
//!    hallucinating agent is served by *retrying* — an [`ActionError`] is
//!    retriable, so the state's `retry_policy` gives the agent another go, and
//!    escalation to a human is the state's own business.
//! 3. **Record the session.** Every run creates an [`AgentSession`] and emits
//!    `AgentSessionStarted` before it and `AgentSessionCompleted` /
//!    `AgentSessionFailed` after it, with the harness's own session id captured
//!    from its output (D18).
//!
//! # Why the executor emits events itself
//!
//! Everything else an executor produces is described by its return value, but a
//! harness session is a thing that exists *during* the run: it starts before
//! there is a result and it can fail without one. So the executor is given an
//! [`AgentEventSink`] — the engine wires in [`LoggingEventSink`], which appends
//! to the execution's log and republishes on the live channel, exactly as the
//! engine's own `emit` does. Without a sink (a direct unit test) the run is
//! unchanged and the events simply go nowhere.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde_json::{Map, Value};
use tokio::process::Command;
use tokio::sync::broadcast;
use uuid::Uuid;

use super::harness::{HarnessAdapter, HarnessRegistry, HarnessRequest, HarnessRun};
use super::script::{read_capped, truncate};
use super::{ActionExecutor, ActionOutcome, ActionRequest, TimedOutAction, ACTION_ID_ENV_VAR};
use crate::domain::{
    ActionConfig, ActionError, ActionResult, AgentActionConfig, AgentSession, AgentSessionStatus,
    Event, EventData, Workspace,
};
use crate::persistence::ExecutionStore;

/// Appended to every prompt, so the agent is told the contract it has to meet.
///
/// Kept deliberately short and imperative: it competes for attention with the
/// state's own prompt, and a long preamble is the first thing a model drops.
pub const AGENT_OUTPUT_CONTRACT: &str = "\n\n\
---\n\
When you are done, end your reply with a single JSON object on its own, and \
nothing after it:\n\
{\"output\": \"<label>\", \"data\": {}, \"variables\": {}, \"artifacts\": []}\n\
`output` is required and must be exactly one of the labels listed above (or, \
if none were listed, the label the task asks for). `data`, `variables` and \
`artifacts` are optional. Do not omit the JSON object: without it the workflow \
cannot continue.";

/// The `parameters` key that continues an existing harness session (D18).
pub const RESUME_SESSION_PARAMETER: &str = "resume_session";

/// The variable the harness session id is published as, when there is one.
pub const SESSION_ID_VARIABLE: &str = "agent_session_id";

/// The `data` key carrying the session record of the run.
pub const SESSION_DATA_KEY: &str = "agent_session";

/// Receives the agent-session events of a run.
///
/// The executor is shared between executions, so the execution the events
/// belong to is passed in per call rather than held.
pub trait AgentEventSink: Send + Sync + std::fmt::Debug {
    /// Records one event of `execution_id`.
    ///
    /// Recording must never fail a run: an event that cannot be written is
    /// logged and dropped, the same way the engine's live channel ignores a
    /// receiver that went away.
    fn emit(&self, execution_id: &str, data: EventData);
}

/// The sink the engine installs: append to the execution's log, then publish.
#[derive(Debug, Clone)]
pub struct LoggingEventSink {
    store: ExecutionStore,
    events: Option<broadcast::Sender<Event>>,
}

impl LoggingEventSink {
    /// A sink writing to `store` and publishing on `events`.
    pub fn new(store: ExecutionStore, events: Option<broadcast::Sender<Event>>) -> Self {
        Self { store, events }
    }
}

impl AgentEventSink for LoggingEventSink {
    fn emit(&self, execution_id: &str, data: EventData) {
        match self
            .store
            .event_log(execution_id)
            .append_next(&execution_id.to_string(), data)
        {
            Ok(event) => {
                if let Some(events) = &self.events {
                    let _ = events.send(event);
                }
            }
            Err(error) => {
                tracing::warn!(
                    execution_id,
                    %error,
                    "an agent session event could not be appended to the log"
                );
            }
        }
    }
}

/// Executes `AgentAction` states.
///
/// Holds the harness registry, so pointing `claude-code` at a different binary
/// — a wrapper, an absolute path, the test suite's fake harness — is a matter
/// of building the executor with another registry.
#[derive(Debug, Clone)]
pub struct AgentExecutor {
    harnesses: HarnessRegistry,
    events: Option<Arc<dyn AgentEventSink>>,
}

impl AgentExecutor {
    /// An executor with the stock harnesses and no event sink.
    pub fn new() -> Self {
        Self {
            harnesses: HarnessRegistry::defaults(),
            events: None,
        }
    }

    /// An executor with a registry of its own.
    pub fn with_harnesses(harnesses: HarnessRegistry) -> Self {
        Self {
            harnesses,
            events: None,
        }
    }

    /// Returns the executor with the session events routed to `sink`.
    pub fn with_event_sink(mut self, sink: Arc<dyn AgentEventSink>) -> Self {
        self.events = Some(sink);
        self
    }

    /// The harnesses this executor can run.
    pub fn harnesses(&self) -> &HarnessRegistry {
        &self.harnesses
    }

    /// Emits an event for `execution_id`, when both are there to emit with.
    fn emit(&self, execution_id: Option<&str>, data: EventData) {
        if let (Some(sink), Some(execution_id)) = (&self.events, execution_id) {
            sink.emit(execution_id, data);
        }
    }
}

impl Default for AgentExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl ActionExecutor for AgentExecutor {
    fn execute<'a>(
        &'a self,
        request: ActionRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ActionOutcome, ActionError>> + Send + 'a>> {
        Box::pin(async move { self.run(request).await })
    }
}

impl AgentExecutor {
    /// One agent run, from the session record to the output label.
    async fn run(&self, request: ActionRequest<'_>) -> Result<ActionOutcome, ActionError> {
        let config = agent_config(request.action)?;
        let adapter = self.harnesses.get(&config.harness)?;

        let mut session = new_session(config, request.execution_id);
        let resume = resume_session(config)?;

        let prompt = format!("{}{AGENT_OUTPUT_CONTRACT}", config.prompt);
        let workspace = Path::new(&request.workspace.path);

        let invocation = adapter.invocation(&HarnessRequest {
            config,
            prompt: &prompt,
            workspace,
            resume_session: resume,
        })?;

        self.emit(
            request.execution_id,
            EventData::AgentSessionStarted {
                session_id: session.id.clone(),
                harness: config.harness.clone(),
            },
        );
        session.status = AgentSessionStatus::Running;

        let outcome = spawn_and_wait(
            &invocation.program,
            &invocation.args,
            request.workspace,
            request.action_execution_id,
            request.timeout,
        )
        .await;

        let finished = match outcome {
            Ok(Some(finished)) => finished,
            Ok(None) => {
                let limit = request.timeout.unwrap_or_default();
                self.fail_session(
                    request.execution_id,
                    &mut session,
                    format!(
                        "the harness was still running after {}s and was killed",
                        limit.as_secs_f64()
                    ),
                );

                return Ok(ActionOutcome::TimedOut(TimedOutAction {
                    timeout: limit,
                    subject: format!("agent harness '{}'", adapter.name()),
                }));
            }
            Err(error) => {
                self.fail_session(request.execution_id, &mut session, error.message.clone());
                return Err(error);
            }
        };

        let run = adapter.parse(&finished.stdout, &finished.stderr, finished.exit_code);

        if let Some(session_id) = &run.session_id {
            session
                .config
                .as_object_mut()
                .expect("the session config is built as an object")
                .insert(
                    "harness_session_id".to_string(),
                    Value::String(session_id.clone()),
                );
        }

        if !run.text.trim().is_empty() {
            self.emit(
                request.execution_id,
                EventData::AgentMessage {
                    session_id: session.id.clone(),
                    content: truncate(&run.text),
                    role: "assistant".to_string(),
                },
            );
        }

        if let Some(failure) = &run.failure {
            self.fail_session(request.execution_id, &mut session, failure.clone());
            return Err(harness_error(adapter.as_ref(), failure, &finished, &run));
        }

        let output = match extract_output(&run.text) {
            Ok(output) => output,
            Err(error) => {
                self.fail_session(request.execution_id, &mut session, error.message.clone());
                return Err(error);
            }
        };

        session.status = AgentSessionStatus::Completed;
        session.ended_at = Some(Utc::now());
        self.emit(
            request.execution_id,
            EventData::AgentSessionCompleted {
                session_id: session.id.clone(),
            },
        );

        Ok(ActionOutcome::Completed(build_result(
            output, &session, &run, &finished,
        )))
    }

    /// Marks the session failed and says so in the log.
    fn fail_session(
        &self,
        execution_id: Option<&str>,
        session: &mut AgentSession,
        error: String,
    ) {
        session.status = AgentSessionStatus::Failed;
        session.ended_at = Some(Utc::now());
        self.emit(
            execution_id,
            EventData::AgentSessionFailed {
                session_id: session.id.clone(),
                error,
            },
        );
    }
}

/// Narrows the action configuration to an agent configuration.
fn agent_config(action: &ActionConfig) -> Result<&AgentActionConfig, ActionError> {
    match action {
        ActionConfig::Agent(config) => Ok(config),
        other => Err(ActionError {
            message: format!("AgentExecutor received a {} action", action_type_name(other)),
            error_type: Some("wrong_action_type".to_string()),
            details: None,
        }),
    }
}

/// The schema name of an action type, for error messages.
fn action_type_name(action: &ActionConfig) -> &'static str {
    match action {
        ActionConfig::Agent(_) => "agent",
        ActionConfig::Script(_) => "script",
        ActionConfig::Human(_) => "human",
        ActionConfig::Wait(_) => "wait",
    }
}

/// The session record of a run, before the harness has said anything.
fn new_session(config: &AgentActionConfig, execution_id: Option<&str>) -> AgentSession {
    AgentSession {
        id: Uuid::new_v4().to_string(),
        workflow_execution_id: execution_id.unwrap_or_default().to_string(),
        harness: config.harness.clone(),
        started_at: Utc::now(),
        ended_at: None,
        status: AgentSessionStatus::Starting,
        model: config.model.clone(),
        config: serde_json::json!({
            "read_only_paths": config.read_only_paths,
            "parameters": config.parameters,
        }),
    }
}

/// The harness session this run continues, if `parameters` names one (D18).
///
/// `parameters` is **not** template-expanded (only `prompt` is, see
/// `TemplateScope::render_action`), so this is a literal session id — enough to
/// pin a run to a known session, not enough to chain one state's session into
/// the next. The captured id is published as [`SESSION_ID_VARIABLE`] so that
/// chaining becomes a matter of expanding `parameters` one day, and nothing
/// else.
fn resume_session(config: &AgentActionConfig) -> Result<Option<&str>, ActionError> {
    match config.parameters.get(RESUME_SESSION_PARAMETER) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(session)) if session.is_empty() => Ok(None),
        Some(Value::String(session)) => Ok(Some(session.as_str())),
        Some(other) => Err(ActionError {
            message: format!(
                "parameter '{RESUME_SESSION_PARAMETER}' must be a session id string, got {other}"
            ),
            error_type: Some("invalid_parameter".to_string()),
            details: None,
        }),
    }
}

/// A harness process that ran to completion.
struct FinishedProcess {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

/// Runs the harness, draining both pipes while it goes.
///
/// `Ok(None)` means the time limit ran out and the process was killed. The
/// draining is [`read_capped`], shared with the script executor: a chatty agent
/// — and every agent is chatty — would otherwise block on a full pipe and look
/// exactly like a hang.
async fn spawn_and_wait(
    program: &str,
    args: &[String],
    workspace: &Workspace,
    action_execution_id: &str,
    limit: Option<Duration>,
) -> Result<Option<FinishedProcess>, ActionError> {
    let working_dir = Path::new(&workspace.path);

    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(working_dir)
        .env(ACTION_ID_ENV_VAR, action_execution_id)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .map_err(|error| spawn_error(program, working_dir, &error))?;

    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");

    let wait = async {
        tokio::join!(
            read_capped(&mut stdout_pipe),
            read_capped(&mut stderr_pipe),
            child.wait(),
        )
    };

    let finished = match limit {
        Some(limit) => tokio::time::timeout(limit, wait).await,
        None => Ok(wait.await),
    };

    let (stdout_read, stderr_read, status) = match finished {
        Ok(finished) => finished,
        Err(_elapsed) => {
            // The waiting future is gone, so the child can be reaped for good.
            let _ = child.kill().await;
            return Ok(None);
        }
    };

    let stdout = stdout_read.map_err(|error| io_error(program, "stdout", &error))?;
    let stderr = stderr_read.map_err(|error| io_error(program, "stderr", &error))?;
    let status = status.map_err(|error| io_error(program, "exit status", &error))?;

    let stdout = String::from_utf8_lossy(&stdout).into_owned();
    let stderr = String::from_utf8_lossy(&stderr).into_owned();

    let Some(exit_code) = status.code() else {
        return Err(ActionError {
            message: format!("the agent harness '{program}' was killed and left no exit code"),
            error_type: Some("killed_by_signal".to_string()),
            details: Some(serde_json::json!({ "program": program, "stderr": truncate(&stderr) })),
        });
    };

    Ok(Some(FinishedProcess {
        stdout,
        stderr,
        exit_code,
    }))
}

/// The harness could not be started.
///
/// A missing binary gets its own message: `No such file or directory` on its
/// own tells an operator nothing about *which* file, and the answer is always
/// the same — install the harness, or put it on `PATH`.
fn spawn_error(program: &str, working_dir: &Path, error: &std::io::Error) -> ActionError {
    let message = if error.kind() == std::io::ErrorKind::NotFound {
        format!(
            "the agent harness binary '{program}' was not found; it is a runtime \
             prerequisite and must be installed and on PATH (or named by an \
             absolute path)"
        )
    } else {
        format!(
            "failed to start the agent harness '{program}' in {}: {error}",
            working_dir.display()
        )
    };

    ActionError {
        message,
        error_type: Some(if error.kind() == std::io::ErrorKind::NotFound {
            "harness_not_found".to_string()
        } else {
            "spawn_failed".to_string()
        }),
        details: Some(serde_json::json!({
            "program": program,
            "working_dir": working_dir.display().to_string(),
            "os_error": error.raw_os_error(),
        })),
    }
}

/// Reading a pipe or reaping the harness failed.
fn io_error(program: &str, what: &str, error: &std::io::Error) -> ActionError {
    ActionError {
        message: format!("failed to read {what} of the agent harness '{program}': {error}"),
        error_type: Some("io_failed".to_string()),
        details: Some(serde_json::json!({ "program": program })),
    }
}

/// The harness itself reported a failure.
fn harness_error(
    adapter: &dyn HarnessAdapter,
    failure: &str,
    finished: &FinishedProcess,
    run: &HarnessRun,
) -> ActionError {
    ActionError {
        message: format!("the agent harness '{}' failed: {failure}", adapter.name()),
        error_type: Some("harness_failed".to_string()),
        details: Some(serde_json::json!({
            "harness": adapter.name(),
            "program": adapter.program(),
            "exit_code": finished.exit_code,
            "session_id": run.session_id,
            "stderr": truncate(&finished.stderr),
        })),
    }
}

/// The §3.3.A payload an agent finished with.
#[derive(Debug, Clone)]
pub struct AgentOutput {
    /// The raw label, exactly as the agent wrote it. Whether the state declares
    /// it is the engine's judgement, never the executor's.
    pub output_label: String,

    /// The `data` object, with `artifacts` folded in the way the script
    /// executor folds it, so the engine registers them the same way.
    pub data: Value,

    /// The `variables` the agent proposes.
    pub variables: Map<String, Value>,
}

/// Extracts the output payload from whatever the agent said (D19/D20).
///
/// Accepts a bare JSON object, a fenced `json` code block, and a JSON object
/// trailing any amount of prose — the last object with an `output` key wins,
/// because an agent that restates its answer restates it at the end.
///
/// Both failures are deliberate errors and not a default label:
///
/// * nothing in the answer parses as a JSON object;
/// * objects are there, but none carries a string `output`.
pub fn extract_output(text: &str) -> Result<AgentOutput, ActionError> {
    let objects = json_objects(text);

    if objects.is_empty() {
        return Err(no_label_error(
            "the agent's answer contains no JSON object at all",
            text,
        ));
    }

    let Some(object) = objects.iter().rev().find_map(|value| {
        let object = value.as_object()?;
        object.get("output").and_then(Value::as_str)?;
        Some(object)
    }) else {
        return Err(no_label_error(
            "the agent's answer contains JSON, but no object with a string 'output' key",
            text,
        ));
    };

    let output_label = object
        .get("output")
        .and_then(Value::as_str)
        .expect("the object was selected for having one")
        .to_string();

    let mut data = object.get("data").cloned().unwrap_or(Value::Null);

    // `ActionResult` has no artifacts field; keep the list where the engine
    // looks for it, exactly as the script executor does.
    if let Some(artifacts) = object.get("artifacts") {
        match &mut data {
            Value::Object(map) => {
                map.insert("artifacts".to_string(), artifacts.clone());
            }
            Value::Null => data = serde_json::json!({ "artifacts": artifacts.clone() }),
            _ => {}
        }
    }

    let variables = object
        .get("variables")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    Ok(AgentOutput {
        output_label,
        data,
        variables,
    })
}

/// How much of the agent's answer an extraction failure quotes back.
const FAILED_EXTRACTION_QUOTE_BYTES: usize = 2 * 1024;

/// No label could be extracted — and none is invented.
fn no_label_error(reason: &str, text: &str) -> ActionError {
    ActionError {
        message: format!(
            "no output label could be extracted from the agent's answer: {reason}. \
             The agent must end with a JSON object carrying an 'output' key \
             (WORKFLOW_SCHEMA.md §3.3.A)"
        ),
        error_type: Some("missing_output_label".to_string()),
        details: Some(serde_json::json!({
            "answer_tail": super::harness::tail(text, FAILED_EXTRACTION_QUOTE_BYTES),
        })),
    }
}

/// Every top-level JSON object in `text`, in the order they appear.
///
/// Scanning for balanced braces — string- and escape-aware, so a `}` inside a
/// string never ends an object — rather than looking for fences: it finds the
/// fenced case too, since the fence markers are outside the braces.
fn json_objects(text: &str) -> Vec<Value> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();

    let mut start = None;
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;

    for (index, byte) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }

        match byte {
            b'"' if depth > 0 => in_string = true,
            b'{' => {
                if depth == 0 {
                    start = Some(index);
                }
                depth += 1;
            }
            b'}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    if let Some(begin) = start.take() {
                        if let Ok(value) = serde_json::from_str::<Value>(&text[begin..=index]) {
                            if value.is_object() {
                                found.push(value);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    found
}

/// Builds the action result of a successful run.
fn build_result(
    output: AgentOutput,
    session: &AgentSession,
    run: &HarnessRun,
    finished: &FinishedProcess,
) -> ActionResult {
    let mut data = output.data;
    let session_record = serde_json::to_value(session).unwrap_or(Value::Null);

    match &mut data {
        Value::Object(map) => {
            map.insert(SESSION_DATA_KEY.to_string(), session_record);
        }
        Value::Null => data = serde_json::json!({ SESSION_DATA_KEY: session_record }),
        _ => {}
    }

    let mut variables = output.variables;
    if let Some(session_id) = &run.session_id {
        // Never over an agent's own variable of that name: the agent's context
        // is the agent's to write (D10 keeps the engine the only writer, and
        // the engine writes what it is given).
        variables
            .entry(SESSION_ID_VARIABLE.to_string())
            .or_insert_with(|| Value::String(session_id.clone()));
    }

    ActionResult {
        output_label: output.output_label,
        data,
        variables,
        exit_code: Some(finished.exit_code),
        stdout: truncate(&run.text),
        stderr: truncate(&finished.stderr),
    }
}
