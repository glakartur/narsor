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

use narsor::domain::{
    ActionConfig, ActionError, ActionResult, Event, ExecutionStatus, HumanInputField,
    HumanInputType, WorkflowDefinition, Workspace,
};
use narsor::engine::actions::{HumanInputRequest, SuspendedAction};
use narsor::engine::{ActionExecutor, ActionOutcome, ActionRequest, Engine};
use narsor::persistence::ExecutionStore;
use narsor::workflow::load_from_file;
use narsor::workspace::WorkspaceProvisioner;
use serde_json::{json, Map, Value};

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
/// in Phase 3), but the reference workflow of `WORKFLOW_SCHEMA.md` §4 has three
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
                .push(config.prompt.clone());

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
                variables: Map::new(),
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
            }))
        })
    }
}

/// An `agent` executor that answers from a fixed list of replies: each one a
/// label together with the `data` and the answer text a real agent returns.
pub struct AnsweringAgentExecutor {
    replies: Mutex<VecDeque<(String, Value, String)>>,
}

impl AnsweringAgentExecutor {
    /// An executor that returns these `(label, data, answer)` replies, in order.
    pub fn new(replies: Vec<(&str, Value, &str)>) -> Self {
        Self {
            replies: Mutex::new(
                replies
                    .into_iter()
                    .map(|(label, data, answer)| (label.to_string(), data, answer.to_string()))
                    .collect(),
            ),
        }
    }
}

impl ActionExecutor for AnsweringAgentExecutor {
    fn execute<'a>(
        &'a self,
        _request: ActionRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ActionOutcome, ActionError>> + Send + 'a>> {
        Box::pin(async move {
            let (output_label, data, stdout) = self
                .replies
                .lock()
                .expect("replies")
                .pop_front()
                .unwrap_or_else(|| ("unexpected".to_string(), json!({}), String::new()));

            Ok(ActionOutcome::Completed(ActionResult {
                output_label,
                data,
                variables: Map::new(),
                exit_code: Some(0),
                stdout,
                stderr: String::new(),
            }))
        })
    }
}

/// An agent that asks once, then completes after the engine reruns the same
/// state with the person's answer included in its prompt.
pub struct QuestioningAgentExecutor {
    calls: Mutex<usize>,
    prompts: Mutex<Vec<String>>,
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
            ActionConfig::Agent(config) => config.prompt.clone(),
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
                        question: None,
                        answer_tail: None,
                    }),
                ));
            }
            Ok(ActionOutcome::Completed(ActionResult {
                output_label: "done".to_string(),
                data: json!({}),
                variables: Map::new(),
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
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
                error_type: Some(narsor::engine::actions::agent::MISSING_OUTPUT_LABEL.to_string()),
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
                variables: Map::new(),
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
            }))
        })
    }
}
