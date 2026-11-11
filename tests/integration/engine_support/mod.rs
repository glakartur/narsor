//! Shared harness for the engine integration tests.
//!
//! Everything runs against a real filesystem and real child processes: a
//! temporary root holding the execution store and the workspace, with the
//! fixture scripts copied into the workspace so that a workflow can refer to
//! them the way a real one does (`./run_tests.sh`).

#![allow(dead_code)]

use std::collections::VecDeque;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Mutex;

use narsor_engine::domain::{
    ActionConfig, ActionError, ActionResult, Event, ExecutionStatus, HumanInputField,
    HumanInputType, WorkflowDefinition, Workspace,
};
use narsor_engine::engine::actions::{HumanInputRequest, SuspendedAction};
use narsor_engine::engine::{ActionExecutor, ActionOutcome, ActionRequest, Engine};
use narsor_engine::persistence::ExecutionStore;
use narsor_engine::workflow::load_from_file;
use narsor_engine::workspace::WorkspaceProvisioner;
use serde_json::{json, Map, Value};

/// Locates a sibling workspace binary from a Cargo integration-test binary.
/// Test executables live under `target/{profile}/deps` and workspace binaries
/// live one directory above it.
pub fn workspace_binary(name: &str) -> PathBuf {
    let test_binary = std::env::current_exe().expect("test executable path");
    let target_profile = test_binary
        .parent()
        .and_then(Path::parent)
        .expect("Cargo test executable is under target/<profile>/deps");
    #[cfg(windows)]
    let name = format!("{name}.exe");
    #[cfg(not(windows))]
    let name = name.to_string();
    let path = target_profile.join(name);
    assert!(
        path.is_file(),
        "workspace binary was not built: {}",
        path.display()
    );
    path
}

/// A temporary root with a store and a workspace, removed when it goes away.
///
/// Stands in for the `tempfile` crate, which is not a dependency of this crate.
pub struct Harness {
    root: PathBuf,
}

impl Harness {
    /// Creates the root, the store directory and a workspace holding every
    /// fixture script.
    pub fn new() -> Self {
        let root = std::env::temp_dir().join(format!("narsor-engine-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("store")).expect("create store root");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");

        for entry in fs::read_dir(fixtures().join("scripts")).expect("read fixture scripts") {
            let entry = entry.expect("fixture script");
            let target = workspace.join(entry.file_name());
            fs::copy(entry.path(), &target).expect("copy fixture script");
        }

        Self { root }
    }

    /// The temporary root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The store every execution is written to.
    pub fn store(&self) -> ExecutionStore {
        ExecutionStore::new(self.root.join("store"))
    }

    /// The directory actions run in.
    pub fn workspace_dir(&self) -> PathBuf {
        self.root.join("workspace")
    }

    /// The workspace record handed to executions.
    pub fn workspace(&self) -> Workspace {
        Workspace::local_folder(self.workspace_dir().to_string_lossy().into_owned())
    }

    /// An engine with the stock executors.
    pub fn engine(&self) -> Engine {
        Engine::new(self.store())
    }

    /// A provisioner over the work tree root, as a fresh process would build
    /// one.
    pub fn provisioner(&self) -> WorkspaceProvisioner {
        WorkspaceProvisioner::new(self.root.join("work"))
    }

    /// The git repository a fixture may use as its product repository.
    pub fn origin_dir(&self) -> PathBuf {
        self.root.join("origin")
    }

    /// Every ref name the product repository holds, as `git ls-remote` lists
    /// them.
    ///
    /// Empty when the fixture uses no git repository at all — which is itself
    /// the answer to "did anything get pushed to it".
    pub fn origin_refs(&self) -> Vec<String> {
        let origin = self.origin_dir();
        if !origin.is_dir() {
            return Vec::new();
        }
        let output = std::process::Command::new("git")
            .arg("ls-remote")
            .arg(&origin)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("run git ls-remote");
        assert!(
            output.status.success(),
            "git ls-remote {} failed: {}",
            origin.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.split_whitespace().nth(1))
            .map(ToString::to_string)
            .collect()
    }

    /// A file inside the workspace.
    pub fn workspace_file(&self, name: &str) -> PathBuf {
        self.workspace_dir().join(name)
    }

    /// Every event of an execution, in sequence order.
    pub fn events(&self, execution_id: &str) -> Vec<Event> {
        self.store()
            .event_log(execution_id)
            .read_all()
            .expect("read event log")
    }

    /// The `type` tag of every event of an execution, in order.
    pub fn event_types(&self, execution_id: &str) -> Vec<String> {
        self.events(execution_id).iter().map(event_type).collect()
    }

    /// The context as it is stored in `context.json`.
    pub fn stored_context(&self, execution_id: &str) -> Value {
        let path = self
            .store()
            .execution_dir(execution_id)
            .join("context.json");
        serde_json::from_slice(&fs::read(path).expect("read context.json")).expect("parse context")
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}

/// The `tests/fixtures` directory.
pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Loads a workflow from `tests/fixtures/engine/{name}.yaml`.
pub fn workflow(name: &str) -> WorkflowDefinition {
    load_from_file(fixtures().join("engine").join(format!("{name}.yaml")))
        .unwrap_or_else(|error| panic!("load fixture '{name}': {error}"))
}

/// Loads a workflow from `tests/fixtures/{file}`.
pub fn workflow_file(file: &str) -> WorkflowDefinition {
    load_from_file(fixtures().join(file))
        .unwrap_or_else(|error| panic!("load fixture '{file}': {error}"))
}

/// The `type` tag serde writes for an event.
pub fn event_type(event: &Event) -> String {
    serde_json::to_value(&event.data)
        .expect("serialize event data")
        .get("type")
        .and_then(Value::as_str)
        .expect("every event data variant is tagged")
        .to_string()
}

/// The schema spelling of a status, for readable assertions.
pub fn status_name(status: &ExecutionStatus) -> &'static str {
    match status {
        ExecutionStatus::Running => "running",
        ExecutionStatus::Paused => "paused",
        ExecutionStatus::InputNeeded => "input_needed",
        ExecutionStatus::WaitingForEvent => "waiting_for_event",
        ExecutionStatus::Completed => "completed",
        ExecutionStatus::Failed => "failed",
        ExecutionStatus::Cancelled => "cancelled",
        ExecutionStatus::Unknown => "unknown",
    }
}

/// An `agent` executor that answers from a fixed list of labels.
///
/// `AgentAction` is out of scope for this plan (D17 puts the harness adapters
/// in Phase 3), but the reference workflow of `docs/dev/workflow-schema.md` §4 has three
/// agent states. This stub stands in for them so the engine loop can be driven
/// end to end over the real fixture, and it records the prompts it was given so
/// a test can check that templates were expanded before the executor saw them.
pub struct StubAgentExecutor {
    labels: Mutex<VecDeque<String>>,
    prompts: Mutex<Vec<String>>,
}

impl StubAgentExecutor {
    /// A "label" that makes the attempt fail as an action failure — unlike
    /// an undeclared label, which is a format error (KTD7).
    pub const FAIL: &'static str = "!fail";

    /// An executor that returns these labels, in order.
    pub fn new(labels: &[&str]) -> Self {
        Self {
            labels: Mutex::new(labels.iter().map(|label| label.to_string()).collect()),
            prompts: Mutex::new(Vec::new()),
        }
    }

    /// The prompts the engine handed over, in order.
    pub fn prompts(&self) -> Vec<String> {
        self.prompts.lock().expect("prompts").clone()
    }
}

impl ActionExecutor for StubAgentExecutor {
    fn execute<'a>(
        &'a self,
        request: ActionRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ActionOutcome, ActionError>> + Send + 'a>> {
        Box::pin(async move {
            let ActionConfig::Agent(config) = request.action else {
                return Err(ActionError {
                    message: "the stub agent executor received a non-agent action".to_string(),
                    error_type: None,
                    details: None,
                });
            };

            self.prompts
                .lock()
                .expect("prompts")
                .push(config.prompt.to_string());

            let label = self
                .labels
                .lock()
                .expect("labels")
                .pop_front()
                .unwrap_or_else(|| "unexpected".to_string());

            if label == StubAgentExecutor::FAIL {
                return Err(ActionError {
                    message: "the stub agent failed on purpose".to_string(),
                    error_type: None,
                    details: None,
                });
            }

            Ok(ActionOutcome::Completed(ActionResult {
                output_label: label,
                data: json!({}),
                ..ActionResult::default()
            }))
        })
    }
}

/// An `agent` executor that answers from a fixed list of results, and records
/// the prompt and the resumed session of every call.
pub struct AnsweringAgentExecutor {
    results: Mutex<VecDeque<ActionResult>>,
    prompts: Mutex<Vec<String>>,
    resumed_sessions: Mutex<Vec<Option<String>>>,
}

impl AnsweringAgentExecutor {
    /// An executor that returns these `(label, data, answer)` replies, in order.
    pub fn new(replies: Vec<(&str, Value, &str)>) -> Self {
        Self::from_results(
            replies
                .into_iter()
                .map(|(label, data, answer)| ActionResult {
                    output_label: label.to_string(),
                    data,
                    exit_code: Some(0),
                    stdout: answer.to_string(),
                    reply: Some(answer.to_string()),
                    ..ActionResult::default()
                })
                .collect(),
        )
    }

    /// An executor that returns these results, in order.
    pub fn from_results(results: Vec<ActionResult>) -> Self {
        Self {
            results: Mutex::new(results.into()),
            prompts: Mutex::new(Vec::new()),
            resumed_sessions: Mutex::new(Vec::new()),
        }
    }

    pub fn prompts(&self) -> Vec<String> {
        self.prompts.lock().expect("prompts").clone()
    }

    /// The `resume_session` each call was handed, in order.
    pub fn resumed_sessions(&self) -> Vec<Option<String>> {
        self.resumed_sessions.lock().expect("sessions").clone()
    }
}

impl ActionExecutor for AnsweringAgentExecutor {
    fn execute<'a>(
        &'a self,
        request: ActionRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ActionOutcome, ActionError>> + Send + 'a>> {
        Box::pin(async move {
            if let narsor_engine::domain::ActionConfig::Agent(config) = request.action {
                self.prompts
                    .lock()
                    .expect("prompts")
                    .push(config.prompt.to_string());
                self.resumed_sessions.lock().expect("sessions").push(
                    config
                        .harness_step
                        .parameters
                        .get("resume_session")
                        .and_then(Value::as_str)
                        .map(ToString::to_string),
                );
            }
            let result = self
                .results
                .lock()
                .expect("results")
                .pop_front()
                .unwrap_or_else(|| ActionResult::new("unexpected"));
            Ok(ActionOutcome::Completed(result))
        })
    }
}

/// An agent that asks once, then completes after the engine reruns the same
/// state with the person's answer included in its prompt.
pub struct QuestioningAgentExecutor {
    calls: Mutex<usize>,
    prompts: Mutex<Vec<String>>,
}

/// Asks once, then reports that its recorded session is gone; the next fresh
/// attempt succeeds. It exercises the runner's operator-facing fallback.
pub struct ResumeRejectingAgentExecutor {
    calls: Mutex<usize>,
}

impl ResumeRejectingAgentExecutor {
    pub fn new() -> Self {
        Self {
            calls: Mutex::new(0),
        }
    }
}

impl ActionExecutor for ResumeRejectingAgentExecutor {
    fn execute<'a>(
        &'a self,
        request: ActionRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ActionOutcome, ActionError>> + Send + 'a>> {
        let call = {
            let mut calls = self.calls.lock().expect("calls");
            *calls += 1;
            *calls
        };
        let action_execution_id = request.action_execution_id.to_string();
        Box::pin(async move {
            match call {
                1 => Ok(ActionOutcome::Suspended(
                    SuspendedAction::AwaitingAgentInput(HumanInputRequest {
                        action_execution_id,
                        instruction: "Which API version?".to_string(),
                        fields: vec![HumanInputField {
                            name: "answer".to_string(),
                            field_type: HumanInputType::Text,
                            required: true,
                            description: None,
                            options: Vec::new(),
                        }],
                        reason: None,
                        required_values: Vec::new(),
                        answer_tail: None,
                        agent_harness: Some("fake".to_string()),
                        agent_session_id: Some("gone-session".to_string()),
                        prefilled_answer: None,
                        permission: None,
                    }),
                )),
                2 => Err(ActionError {
                    message: "saved session does not exist".to_string(),
                    error_type: Some("resume_session_rejected".to_string()),
                    details: None,
                }),
                _ => Ok(ActionOutcome::Completed(ActionResult {
                    output_label: "done".to_string(),
                    data: json!({}),
                    ..ActionResult::default()
                })),
            }
        })
    }
}

impl QuestioningAgentExecutor {
    pub fn new() -> Self {
        Self {
            calls: Mutex::new(0),
            prompts: Mutex::new(Vec::new()),
        }
    }

    pub fn prompts(&self) -> Vec<String> {
        self.prompts.lock().expect("prompts").clone()
    }
}

impl ActionExecutor for QuestioningAgentExecutor {
    fn execute<'a>(
        &'a self,
        request: ActionRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ActionOutcome, ActionError>> + Send + 'a>> {
        let prompt = match request.action {
            ActionConfig::Agent(config) => config.prompt.to_string(),
            _ => {
                return Box::pin(async {
                    Err(ActionError {
                        message: "the questioning agent received a non-agent action".to_string(),
                        error_type: None,
                        details: None,
                    })
                })
            }
        };
        self.prompts.lock().expect("prompts").push(prompt);
        let call = {
            let mut calls = self.calls.lock().expect("calls");
            *calls += 1;
            *calls
        };
        let action_execution_id = request.action_execution_id.to_string();
        Box::pin(async move {
            if call == 1 {
                return Ok(ActionOutcome::Suspended(
                    SuspendedAction::AwaitingAgentInput(HumanInputRequest {
                        action_execution_id,
                        instruction: "Which API version should I use?".to_string(),
                        fields: vec![HumanInputField {
                            name: "answer".to_string(),
                            field_type: HumanInputType::Text,
                            required: true,
                            description: None,
                            options: Vec::new(),
                        }],
                        reason: None,
                        required_values: Vec::new(),
                        answer_tail: None,
                        agent_harness: None,
                        agent_session_id: None,
                        prefilled_answer: None,
                        permission: None,
                    }),
                ));
            }
            Ok(ActionOutcome::Completed(ActionResult {
                output_label: "done".to_string(),
                data: json!({}),
                ..ActionResult::default()
            }))
        })
    }
}

/// An `agent` executor whose answer never carries an output label, and whose
/// harness reports no session to resume — so the engine cannot ask it again
/// and the step becomes a problem straight away (U5).
pub struct LabellessAgentExecutor {
    answer: String,
}

impl LabellessAgentExecutor {
    /// An executor that always answers `answer`, without a label.
    pub fn answering(answer: &str) -> Self {
        Self {
            answer: answer.to_string(),
        }
    }
}

impl ActionExecutor for LabellessAgentExecutor {
    fn execute<'a>(
        &'a self,
        _request: ActionRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ActionOutcome, ActionError>> + Send + 'a>> {
        Box::pin(async move {
            Err(ActionError {
                message: "no output label could be extracted from the agent's answer".to_string(),
                error_type: Some(
                    narsor_engine::engine::actions::agent::MISSING_OUTPUT_LABEL.to_string(),
                ),
                details: Some(json!({ "answer_tail": self.answer })),
            })
        })
    }
}

/// An executor that records the time limit it was handed and then answers.
///
/// What it exists for is the *value*: the engine resolves a limit per action
/// kind, and the only way to see which chain produced it is to look at what
/// reached the executor.
pub struct TimeoutRecorder {
    label: String,
    timeouts: Mutex<Vec<Option<std::time::Duration>>>,
}

impl TimeoutRecorder {
    /// A recorder that always answers with `label`.
    pub fn answering(label: &str) -> Self {
        Self {
            label: label.to_string(),
            timeouts: Mutex::new(Vec::new()),
        }
    }

    /// The limits handed over, in order.
    pub fn timeouts(&self) -> Vec<Option<std::time::Duration>> {
        self.timeouts.lock().expect("timeouts").clone()
    }
}

impl ActionExecutor for TimeoutRecorder {
    fn execute<'a>(
        &'a self,
        request: ActionRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ActionOutcome, ActionError>> + Send + 'a>> {
        self.timeouts
            .lock()
            .expect("timeouts")
            .push(request.timeout);
        let label = self.label.clone();
        Box::pin(async move {
            Ok(ActionOutcome::Completed(ActionResult {
                output_label: label,
                data: json!({}),
                ..ActionResult::default()
            }))
        })
    }
}

/// Kills every `narsor` process whose command line names `root` — the daemon
/// a test started and the runners that outlive it by design — so a test that
/// panics before its own cleanup leaves nothing behind. A runner leads its
/// process group, which takes its harness with it.
pub fn kill_processes_under(root: &Path) {
    let root = root.display().to_string();
    let Ok(entries) = fs::read_dir("/proc") else {
        return;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let command = fs::read(entry.path().join("cmdline")).unwrap_or_default();
        let command = String::from_utf8_lossy(&command).replace('\0', " ");
        if pid != std::process::id() && command.contains("narsor") && command.contains(&root) {
            // SAFETY: a process of a test's own, unique temporary root.
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
}

/// One turn of a [`ScriptedAgentExecutor`].
#[derive(Debug, Clone)]
pub enum AgentTurn {
    /// The output `label`, with these variables and this reply; `session`
    /// says whether the harness reported a session.
    Label {
        label: &'static str,
        reason: &'static str,
        variables: Value,
        artifacts: Value,
        reply: Option<&'static str>,
        session: bool,
    },
    /// No output label (a format error); `session` says whether the harness
    /// reported a session to ask again in.
    NoLabel {
        reply: Option<&'static str>,
        session: bool,
    },
    /// An action failure.
    Fail,
    /// An action failure of this error type (`timeout`, …).
    FailOfType(&'static str),
    /// A question to the person.
    Ask(&'static str),
    /// A permission request for one Bash command.
    Permission,
}

impl AgentTurn {
    /// The output `label` with no variables and `reply`.
    pub fn label(label: &'static str, reply: &'static str) -> Self {
        Self::Label {
            label,
            reason: "",
            variables: json!({}),
            artifacts: json!({}),
            reply: Some(reply),
            session: true,
        }
    }

    /// The output `label` from a harness that reported no session.
    pub fn sessionless(label: &'static str) -> Self {
        Self::Label {
            label,
            reason: "",
            variables: json!({}),
            artifacts: json!({}),
            reply: None,
            session: false,
        }
    }

    /// The output `label` with `variables` and no reply.
    pub fn with_variables(label: &'static str, variables: Value) -> Self {
        Self::Label {
            label,
            reason: "",
            variables,
            artifacts: json!({}),
            reply: None,
            session: true,
        }
    }

    /// The output `label` with these named `artifacts` (name → path).
    pub fn with_artifacts(label: &'static str, artifacts: Value) -> Self {
        Self::Label {
            label,
            reason: "",
            variables: json!({}),
            artifacts,
            reply: None,
            session: true,
        }
    }

    /// The output `label` with this `reason`.
    pub fn with_reason(label: &'static str, reason: &'static str) -> Self {
        Self::Label {
            label,
            reason,
            variables: json!({}),
            artifacts: json!({}),
            reply: None,
            session: true,
        }
    }
}

/// What a [`ScriptedAgentExecutor`] was handed on one call.
#[derive(Debug, Clone)]
pub struct AgentCall {
    pub prompt: String,
    pub resume_session: Option<String>,
    pub action_execution_id: String,
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// An `agent` executor that plays [`AgentTurn`]s in order and records every
/// call.
pub struct ScriptedAgentExecutor {
    turns: Mutex<VecDeque<AgentTurn>>,
    calls: Mutex<Vec<AgentCall>>,
}

impl ScriptedAgentExecutor {
    pub fn new(turns: Vec<AgentTurn>) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
            calls: Mutex::new(Vec::new()),
        }
    }

    pub fn calls(&self) -> Vec<AgentCall> {
        self.calls.lock().expect("calls").clone()
    }

    pub fn prompts(&self) -> Vec<String> {
        self.calls().into_iter().map(|call| call.prompt).collect()
    }
}

impl ActionExecutor for ScriptedAgentExecutor {
    fn execute<'a>(
        &'a self,
        request: ActionRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ActionOutcome, ActionError>> + Send + 'a>> {
        let ActionConfig::Agent(config) = request.action else {
            panic!("the scripted agent only runs agent actions");
        };
        let action_execution_id = request.action_execution_id.to_string();
        self.calls.lock().expect("calls").push(AgentCall {
            prompt: config.prompt.to_string(),
            resume_session: config
                .harness_step
                .parameters
                .get("resume_session")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            action_execution_id: action_execution_id.clone(),
            model: config.harness_step.model.clone(),
            effort: config.harness_step.effort.clone(),
        });
        let turn = self
            .turns
            .lock()
            .expect("turns")
            .pop_front()
            .expect("the agent was called more often than scripted");
        let question = |instruction: &str, permission: Option<_>| HumanInputRequest {
            action_execution_id: action_execution_id.clone(),
            instruction: instruction.to_string(),
            // A permission request is decided, not answered.
            fields: if permission.is_some() {
                Vec::new()
            } else {
                vec![HumanInputField {
                    name: "answer".to_string(),
                    field_type: HumanInputType::Text,
                    required: true,
                    description: None,
                    options: Vec::new(),
                }]
            },
            reason: None,
            required_values: Vec::new(),
            answer_tail: None,
            agent_harness: Some("claude-code".to_string()),
            agent_session_id: Some("agent-session".to_string()),
            prefilled_answer: None,
            permission,
        };
        let outcome = match turn {
            AgentTurn::Label {
                label,
                reason,
                variables,
                artifacts,
                reply,
                session,
            } => Ok(ActionOutcome::Completed(ActionResult {
                output_label: label.to_string(),
                reason: reason.to_string(),
                artifacts: serde_json::from_value(artifacts)
                    .expect("scripted artifacts are a name → path object"),
                // The session a reprompt of a rejected result resumes.
                data: if session {
                    json!({
                        "agent_session": {
                            "harness": "claude-code",
                            "config": { "harness_session_id": "agent-session" },
                        },
                    })
                } else {
                    json!({})
                },
                variables: match variables {
                    Value::Object(variables) => variables,
                    _ => Map::new(),
                },
                stdout: reply.unwrap_or_default().to_string(),
                reply: reply.map(ToString::to_string),
                ..ActionResult::default()
            })),
            AgentTurn::NoLabel { reply, session } => {
                let mut details = json!({ "answer_tail": reply.unwrap_or_default() });
                if let Some(reply) = reply {
                    details["reply"] = json!(reply);
                }
                if session {
                    details["harness"] = json!("claude-code");
                    details["harness_session_id"] = json!("agent-session");
                }
                Err(ActionError {
                    message: "no output label could be extracted from the agent's answer"
                        .to_string(),
                    error_type: Some(
                        narsor_engine::engine::actions::agent::MISSING_OUTPUT_LABEL.to_string(),
                    ),
                    details: Some(details),
                })
            }
            AgentTurn::Fail => Err(ActionError {
                message: "the scripted agent failed on purpose".to_string(),
                error_type: None,
                details: None,
            }),
            AgentTurn::FailOfType(error_type) => Err(ActionError {
                message: format!("the scripted agent failed with {error_type}"),
                error_type: Some(error_type.to_string()),
                details: None,
            }),
            AgentTurn::Ask(instruction) => Ok(ActionOutcome::Suspended(
                SuspendedAction::AwaitingAgentInput(question(instruction, None)),
            )),
            AgentTurn::Permission => Ok(ActionOutcome::Suspended(
                SuspendedAction::AwaitingAgentInput(question(
                    "Claude Code needs a permission decision.",
                    Some(narsor_engine::engine::actions::PermissionRequestDetails {
                        denials: vec![narsor_engine::engine::actions::PermissionDenialDetails {
                            tool_name: "Bash".to_string(),
                            tool_use_id: "tool-permission".to_string(),
                            input: json!({ "command": "cargo build" }),
                            cwd: "/tmp".to_string(),
                            reason_type: Some("asyncAgent".to_string()),
                            reason: Some("The command needs approval.".to_string()),
                        }],
                    }),
                )),
            )),
        };
        Box::pin(async move { outcome })
    }
}

/// The fake harness CLI, installed in a directory of its own.
pub struct FakeHarness {
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
    pub fn install(dir: PathBuf) -> Self {
        fs::create_dir_all(&dir).expect("create the harness directory");
        let source = fixtures().join("harness/fake_harness.sh");
        let program = dir.join("fake_harness.sh");
        let temp = dir.join(format!(".fake_harness.sh.{}.tmp", uuid::Uuid::new_v4()));
        fs::copy(&source, &temp).expect("copy the fake harness");
        fs::rename(&temp, &program).expect("move the fake harness into place");
        Self { dir, program }
    }

    /// The program an adapter is built with.
    pub fn program(&self) -> String {
        self.program.to_string_lossy().into_owned()
    }

    /// What the harness prints on stdout, on every call.
    pub fn answers(&self, stdout: &str) -> &Self {
        fs::write(self.dir.join("stdout"), stdout).expect("write the harness answer");
        self
    }

    /// What the harness prints on the `nth` call (1-based).
    pub fn answers_on_call(&self, nth: usize, stdout: &str) -> &Self {
        fs::write(self.dir.join(format!("stdout.{nth}")), stdout).expect("write the answer");
        self
    }

    /// Makes the harness sleep before answering.
    pub fn sleeps(&self, seconds: u64) -> &Self {
        fs::write(self.dir.join("sleep"), seconds.to_string()).expect("write the sleep");
        self
    }

    /// Makes the `nth` call (1-based) sleep after answering.
    pub fn sleeps_on_call(&self, nth: usize, seconds: u64) -> &Self {
        fs::write(self.dir.join(format!("sleep.{nth}")), seconds.to_string())
            .expect("write the sleep");
        self
    }

    /// Makes the harness fail with an exit code and a message on stderr.
    pub fn fails(&self, code: i32, stderr: &str) -> &Self {
        fs::write(self.dir.join("exit"), code.to_string()).expect("write the exit code");
        fs::write(self.dir.join("stderr"), stderr).expect("write the stderr");
        self
    }

    /// The arguments of the `nth` call.
    pub fn args(&self, nth: usize) -> Vec<String> {
        let raw = fs::read_to_string(self.dir.join(format!("args.{nth}")))
            .expect("the harness recorded its arguments");
        raw.split("\n--%ARG%--\n")
            .filter(|arg| !arg.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// The working directory of the `nth` call.
    pub fn cwd(&self, nth: usize) -> String {
        read_line(&self.dir.join(format!("cwd.{nth}")))
    }

    /// The `ORCHESTRATOR_ACTION_ID` the `nth` call saw.
    pub fn action_id(&self, nth: usize) -> String {
        read_line(&self.dir.join(format!("action_id.{nth}")))
    }

    /// How many times the harness was called.
    pub fn calls(&self) -> usize {
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
pub fn envelope(text: &str) -> String {
    json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "session_id": "fake-session-1",
        "result": text,
    })
    .to_string()
}

/// The `exit` label of a step with one way out.
pub fn exit() -> AgentTurn {
    AgentTurn::label("exit", "done")
}
