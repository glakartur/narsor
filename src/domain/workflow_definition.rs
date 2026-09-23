//! Workflow Definition model
//!
//! Represents the immutable definition of a workflow (the blueprint).
//!
//! The external contract for this model is `WORKFLOW_SCHEMA.md`; the YAML
//! layer lives in `crate::workflow::schema` and is compiled into these types
//! by `crate::workflow::loader`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::domain::ExecutionStatus;

/// Unique identifier for a workflow definition
pub type WorkflowDefinitionId = String;

/// Version number for workflow definitions
pub type Version = u32;

/// The main workflow definition structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowDefinition {
    /// Unique identifier for this workflow.
    ///
    /// The YAML contract has no `id` field, so the loader derives it from
    /// `name`, which `WORKFLOW_SCHEMA.md` §2 declares to be unique.
    pub id: WorkflowDefinitionId,

    /// Human-readable name
    pub name: String,

    /// Version of this workflow definition
    pub version: Version,

    /// Optional description
    #[serde(default)]
    pub description: Option<String>,

    /// Input parameters required to start the workflow
    #[serde(default)]
    pub inputs: Vec<InputParameter>,

    /// Workflow-wide defaults (`globals` in YAML), already propagated to the
    /// individual states but kept here so the original intent is not lost.
    #[serde(default)]
    pub globals: GlobalsConfig,

    /// Default workspace for every execution of this workflow
    /// (`globals.workspace` in YAML). Workspaces are per execution, never
    /// per state.
    #[serde(default)]
    pub workspace: Option<WorkspaceSpec>,

    /// States in the workflow graph
    pub states: HashMap<StateId, State>,

    /// The entry point state
    pub initial_state: StateId,
}

/// Workflow-wide defaults from the `globals` section
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GlobalsConfig {
    /// Default timeout for a state, in seconds
    #[serde(default)]
    pub timeout_secs: Option<u64>,

    /// Default retry policy for actions
    #[serde(default)]
    pub retry_policy: Option<RetryPolicy>,
}

/// Default workspace configuration for an execution
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkspaceSpec {
    /// Git checkout of a registered repository
    Git {
        /// Repository id from the registry (may still contain a `{{ }}` template)
        repository: String,
        /// Branch, tag or commit
        #[serde(default, rename = "ref")]
        git_ref: Option<String>,
    },
    /// Plain local folder, relative to the task directory
    Local {
        /// Path to the folder
        path: String,
    },
}

/// Type of an input parameter
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ParamType {
    #[default]
    String,
    Integer,
    Boolean,
    Repository,
}


/// Input parameter definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputParameter {
    pub name: String,

    /// Declared type of the parameter (`type` in YAML)
    #[serde(default, rename = "type")]
    pub param_type: ParamType,

    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<serde_json::Value>,
    #[serde(default)]
    pub description: Option<String>,
}

/// Identifier for a state within a workflow
pub type StateId = String;

/// The execution status a terminal state produces when it is entered.
///
/// This is the explicit marker of `WORKFLOW_SCHEMA.md` §3.5: a workflow says
/// what reaching a terminal means instead of leaving the engine to guess from
/// the state's name. The variants mirror the terminal variants of
/// [`crate::domain::ExecutionStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum TerminalStatus {
    #[default]
    Completed,
    Failed,
    Cancelled,
}


impl From<TerminalStatus> for ExecutionStatus {
    fn from(status: TerminalStatus) -> Self {
        match status {
            TerminalStatus::Completed => ExecutionStatus::Completed,
            TerminalStatus::Failed => ExecutionStatus::Failed,
            TerminalStatus::Cancelled => ExecutionStatus::Cancelled,
        }
    }
}

/// A state in the workflow graph
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    /// Unique identifier within the workflow
    pub id: StateId,

    /// Optional human-readable description
    #[serde(default)]
    pub description: Option<String>,

    /// The action to execute when entering this state.
    ///
    /// Terminal states carry no action — see `WORKFLOW_SCHEMA.md` §4, where
    /// `completed` and `failed` only have a description.
    #[serde(default)]
    pub action: Option<ActionConfig>,

    /// Possible outputs from this state's action
    #[serde(default)]
    pub outputs: Vec<OutputDefinition>,

    /// Transitions from this state based on decision results
    #[serde(default)]
    pub transitions: HashMap<OutputLabel, StateId>,

    /// Decision provider configuration (optional, defaults to rules)
    #[serde(default)]
    pub decision: Option<DecisionConfig>,

    /// Timeout in seconds (optional)
    #[serde(default)]
    pub timeout_secs: Option<u64>,

    /// Retry policy (optional)
    #[serde(default)]
    pub retry_policy: Option<RetryPolicy>,

    /// Whether this is a terminal state
    #[serde(default)]
    pub terminal: bool,

    /// How the execution ends when this terminal state is entered.
    ///
    /// Declared explicitly in the YAML contract (`WORKFLOW_SCHEMA.md` §3.5,
    /// the mapping form of `terminal_states`). `None` means the workflow did
    /// not declare one — the list form of `terminal_states`, or a `state.json`
    /// written before this field existed — and the engine then treats the
    /// terminal as a success.
    #[serde(default)]
    pub terminal_status: Option<TerminalStatus>,

    /// Whether manual transition override is allowed
    #[serde(default)]
    pub allow_manual_override: bool,
}

/// Configuration for an action
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActionConfig {
    Agent(AgentActionConfig),
    Script(ScriptActionConfig),
    Human(HumanActionConfig),
    Wait(WaitActionConfig),
}

/// Agent action configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentActionConfig {
    /// Which harness to use (e.g., "claude-code", "opencode")
    pub harness: String,

    /// Model specification (optional)
    #[serde(default)]
    pub model: Option<String>,

    /// Prompt to send to the agent
    pub prompt: String,

    /// Workspace configuration (optional, defaults to current)
    #[serde(default)]
    pub workspace: Option<WorkspaceRef>,

    /// Additional parameters
    #[serde(default)]
    pub parameters: HashMap<String, serde_json::Value>,

    /// Extra paths mounted read-only, relative to the workspace
    #[serde(default)]
    pub read_only_paths: Vec<String>,
}

/// Script action configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptActionConfig {
    /// Command to execute
    pub command: String,

    /// Arguments passed to the command, each one separately
    #[serde(default)]
    pub args: Vec<String>,

    /// Working directory (optional)
    #[serde(default)]
    pub working_dir: Option<String>,

    /// Environment variables
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Mapping of process exit codes to output labels.
    /// Empty means the default mapping: 0 -> success, anything else -> failure.
    #[serde(default)]
    pub exit_codes: HashMap<i32, OutputLabel>,

    /// Timeout in seconds
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// Human action configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HumanActionConfig {
    /// Description of what the human needs to do (`message` in YAML)
    pub instruction: String,

    /// Form fields the human is asked to fill in
    #[serde(default)]
    pub inputs: Vec<HumanInputField>,

    /// Expected input schema (optional)
    #[serde(default)]
    pub input_schema: Option<serde_json::Value>,
}

/// Type of a form field presented to a human
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanInputType {
    /// Free-form text
    Text,
    /// One value out of `options`
    Select,
}

/// A single form field of a `HumanAction`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HumanInputField {
    pub name: String,

    /// Kind of the field (`type` in YAML)
    #[serde(rename = "type")]
    pub field_type: HumanInputType,

    /// Allowed values, meaningful for `select` fields
    #[serde(default)]
    pub options: Vec<String>,

    #[serde(default)]
    pub required: bool,

    #[serde(default)]
    pub description: Option<String>,
}

/// Wait action configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitActionConfig {
    /// Event type to wait for (`event_type` in YAML)
    pub event: String,

    /// Filter criteria (optional)
    #[serde(default)]
    pub filter: Option<serde_json::Value>,

    /// Timeout in seconds
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// Reference to a workspace
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceRef {
    Current,
    Repository(String),
    Path(String),
}

/// Definition of a possible output from an action
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputDefinition {
    pub label: OutputLabel,
    #[serde(default)]
    pub description: Option<String>,
}

/// Label identifying an output/transition
pub type OutputLabel = String;

/// Decision provider configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DecisionConfig {
    Rules,
    Agent(AgentDecisionConfig),
    Human(HumanDecisionConfig),
}

/// Agent-based decision configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDecisionConfig {
    #[serde(default)]
    pub harness: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
}

/// Human-based decision configuration
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HumanDecisionConfig {
    /// Message shown to the operator when picking the transition
    #[serde(default)]
    pub message: Option<String>,
}

/// Retry policy configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Total number of attempts, including the first one (`max_attempts` in YAML)
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,

    /// Delay between attempts in seconds (`backoff` in YAML)
    #[serde(default = "default_backoff_secs")]
    pub backoff_secs: u64,

    /// Backoff multiplier applied between attempts.
    /// Not part of the YAML contract; kept for the engine (D21).
    #[serde(default = "default_backoff_multiplier")]
    pub backoff_multiplier: f64,
}

fn default_max_attempts() -> u32 { 1 }
fn default_backoff_secs() -> u64 { 0 }
fn default_backoff_multiplier() -> f64 { 2.0 }

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            backoff_secs: default_backoff_secs(),
            backoff_multiplier: default_backoff_multiplier(),
        }
    }
}

impl WorkflowDefinition {
    /// Validates the workflow definition against `WORKFLOW_SCHEMA.md` §5.
    ///
    /// All violations are reported at once, so that the author of a workflow
    /// does not have to fix them one file save at a time. The rules themselves
    /// live in [`crate::workflow::validator`], which also offers entry points
    /// that see the YAML layer (`terminal_states` naming a state that does not
    /// exist disappears during compilation).
    pub fn validate(&self) -> Result<(), Vec<ValidationError>> {
        crate::workflow::validator::validate(self)
    }
}

/// Errors that can occur during workflow validation
#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    // --- Graph consistency (§5.1) ---
    #[error("Initial state '{0}' not found")]
    InitialStateNotFound(StateId),

    #[error("State '{0}' has transition '{1}' to unknown state '{2}'")]
    TransitionToUnknownState(StateId, OutputLabel, StateId),

    #[error("Terminal state '{0}' cannot have transitions")]
    TerminalStateWithTransitions(StateId),

    #[error("Workflow must have at least one terminal state")]
    NoTerminalState,

    #[error("State '{state}' declares output '{label}' but has no transition for it")]
    OutputWithoutTransition { state: StateId, label: OutputLabel },

    #[error("State '{state}' has a transition for label '{label}', which is not declared in its outputs")]
    TransitionWithoutOutput { state: StateId, label: OutputLabel },

    #[error("State '{0}' is not reachable from the initial state")]
    UnreachableState(StateId),

    #[error("State '{0}' has no path to any terminal state (deadlock)")]
    NoPathToTerminal(StateId),

    #[error("Non-terminal state '{0}' has no action")]
    NonTerminalStateWithoutAction(StateId),

    #[error("'terminal_states' names state '{0}', which is not defined")]
    UnknownTerminalState(StateId),

    // --- Action correctness (§5.2) ---
    #[error("State '{state}' has a {action_type} action without the required field '{field}'")]
    MissingActionField {
        state: StateId,
        action_type: &'static str,
        field: &'static str,
    },

    #[error("State '{state}' has a {action_type} action with an empty 'outputs' list")]
    EmptyOutputs {
        state: StateId,
        action_type: &'static str,
    },

    // --- Decision correctness (§5.3) ---
    #[error("State '{state}' uses an agent decision without the required field '{field}'")]
    AgentDecisionMissingField {
        state: StateId,
        field: &'static str,
    },

    // --- Variables and context (§5.4) ---
    #[error("State '{state}' references unknown input '{variable}' in field '{field}'")]
    UnknownInputReference {
        state: StateId,
        field: String,
        variable: String,
    },

    #[error("Field '{field}' references unknown input '{variable}'")]
    UnknownInputReferenceInGlobals {
        field: &'static str,
        variable: String,
    },
}
