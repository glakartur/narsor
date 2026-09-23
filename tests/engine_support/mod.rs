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

use orchestrator::domain::{
    ActionConfig, ActionError, ActionResult, Event, ExecutionStatus, WorkflowDefinition, Workspace,
};
use orchestrator::engine::{ActionExecutor, ActionOutcome, ActionRequest, Engine};
use orchestrator::persistence::ExecutionStore;
use orchestrator::workflow::load_from_file;
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
        let root =
            std::env::temp_dir().join(format!("orchestrator-engine-{}", uuid::Uuid::new_v4()));
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
