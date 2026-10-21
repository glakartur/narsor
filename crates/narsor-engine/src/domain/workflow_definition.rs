//! Workflow Definition model
//!
//! Represents the immutable definition of a workflow (the blueprint).
//!
//! The external contract for this model is `WORKFLOW_SCHEMA.md`; the YAML
//! layer lives in `crate::workflow::schema` and is compiled into these types
//! by `crate::workflow::loader`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::domain::{DisplaySpec, ExecutionStatus, ImageSpec};

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

    /// How a task of this workflow introduces itself: title and links, still
    /// templated (`display` in YAML).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<DisplaySpec>,

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

    /// Base-image override for the whole workflow (`globals.image`). Not
    /// propagated to the states: provenance (R18) has to tell a workflow
    /// override from a step override, so each keeps its own field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageSpec>,
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
        /// Paths captured into a checkpoint even though the repository
        /// ignores them (`preserve_paths` in YAML).
        #[serde(default)]
        preserve_paths: Vec<String>,
    },
    /// Plain local folder, relative to the task directory
    Local {
        /// Path to the folder
        path: String,
        /// Paths captured into a checkpoint even though the repository
        /// ignores them (`preserve_paths` in YAML).
        #[serde(default)]
        preserve_paths: Vec<String>,
    },
}

/// Type of an input parameter
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ParamType {
    #[default]
    String,
    /// Multi-line text: a string whose value is prose, shown as a large field in the TUI.
    Text,
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

    /// Maximum number of workflow entries before operator intervention.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_entries: Option<u32>,

    /// Optional human-readable description
    #[serde(default)]
    pub description: Option<String>,

    /// The action to execute when entering this state.
    ///
    /// Terminal states carry no action — see `WORKFLOW_SCHEMA.md` §4, where
    /// `completed` and `failed` only have a description.
    #[serde(default)]
    pub action: Option<ActionConfig>,

    /// Variables the step's action may set, each with what it means.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variables: Vec<VariableDefinition>,

    /// Artifacts the step's action may produce, each with what it is.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ArtifactDefinition>,

    /// The choices that end this state's action (`choices` in YAML).
    #[serde(default)]
    pub choices: Vec<ChoiceDefinition>,

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

    /// Base-image override for this step (R15): wins over `globals.image`
    /// and over the repository's base image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageSpec>,
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
    /// Which harness to use (e.g., "claude-code", "opencode").
    ///
    /// `None` means "whatever the configuration says", resolved by the
    /// executor rather than the loader so that compiling a definition stays a
    /// pure function of its source (D-F). `serde(default)` reads an execution
    /// stored before the field became optional, where it was always present.
    #[serde(default)]
    pub harness: Option<String>,

    /// One-shot Claude Code allow rules attached by a resolved task
    /// permission request. Never loaded from workflow source or persisted
    /// definitions.
    #[serde(skip)]
    pub permission_grants: Vec<String>,

    /// Model specification (optional)
    #[serde(default)]
    pub model: Option<String>,

    /// Access the agent receives to its execution workspace.
    ///
    /// This is intentionally independent of a particular harness: each
    /// adapter translates it to its native permission mechanism.
    #[serde(default = "legacy_workspace_access")]
    pub workspace_access: Option<WorkspaceAccess>,

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

    /// Whether the engine adds the history of earlier attempts to the prompt
    /// when the step is entered again (`history` in YAML).
    #[serde(default = "default_history")]
    pub history: bool,
}

fn default_history() -> bool {
    true
}

fn legacy_workspace_access() -> Option<WorkspaceAccess> {
    // Records written before omitted access was preserved serialized the
    // schema's read-write default as an explicit value.
    Some(WorkspaceAccess::ReadWrite)
}

/// The workflow-level workspace permission for an agent action.
///
/// New actions are writable unless they explicitly opt into report-only work.
/// `serde(default)` also makes executions stored before this field existed
/// retain that historical effective behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceAccess {
    #[default]
    ReadWrite,
    ReadOnly,
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
    #[serde(default, with = "exit_code_map")]
    pub exit_codes: HashMap<i32, OutputLabel>,

    /// Timeout in seconds
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// JSON object keys are strings, while workflow exit codes are integers.
/// This adapter keeps the ergonomic typed map and makes sealed JSON workflow
/// snapshots round-trip faithfully.
mod exit_code_map {
    use std::collections::HashMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::OutputLabel;

    pub fn serialize<S>(map: &HashMap<i32, OutputLabel>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        map.iter()
            .map(|(code, label)| (code.to_string(), label))
            .collect::<HashMap<_, _>>()
            .serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<HashMap<i32, OutputLabel>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let values = HashMap::<String, OutputLabel>::deserialize(deserializer)?;
        values
            .into_iter()
            .map(|(code, label)| {
                code.parse::<i32>()
                    .map(|code| (code, label))
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

/// A variable a step defines: its name and what its value means.
///
/// A name with the [`LOCAL_VARIABLE_PREFIX`] is the step's own memory and
/// never reaches the shared context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VariableDefinition {
    pub name: String,
    pub description: String,
}

impl VariableDefinition {
    /// Whether this variable is the step's own memory (`local.*`).
    pub fn is_local(&self) -> bool {
        self.name.starts_with(LOCAL_VARIABLE_PREFIX)
    }
}

/// The prefix of a variable that stays with its step (`local.memory`).
pub const LOCAL_VARIABLE_PREFIX: &str = "local.";

/// An artifact a step defines: its name and what the file is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactDefinition {
    pub name: String,
    pub description: String,
}

/// One choice that ends a step, and what it needs to be accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChoiceDefinition {
    pub label: OutputLabel,
    /// Names of step variables that must have a value when this choice is
    /// made.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "deserialize_required_names"
    )]
    pub requires_variables: Vec<String>,
    /// Names of step artifacts that must be given when this choice is made.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires_artifacts: Vec<String>,
    /// What making this choice means, shown wherever the choice is offered
    /// (the agent's output contract, the input form, the transition menu).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Stops an agent result for an explicit operator decision before its
    /// ordinary transition is applied.
    #[serde(default)]
    pub requires_human_approval: bool,
    /// A text input whose value continues the transition target agent's
    /// prior session after this human choice is made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_comment_field: Option<String>,
}

/// Reads `requires_variables` of a stored record (`pending_input.json`) in
/// either shape it was ever written in: plain names, or the
/// `{name, description}` objects of records written before descriptions
/// moved to the step (KTD16: stored engine records stay readable).
fn deserialize_required_names<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StoredName {
        Name(String),
        Described { name: String },
    }

    let stored = Vec::<StoredName>::deserialize(deserializer)?;
    Ok(stored
        .into_iter()
        .map(|entry| match entry {
            StoredName::Name(name) | StoredName::Described { name } => name,
        })
        .collect())
}

impl ChoiceDefinition {
    /// A choice with no description and no requirements.
    pub fn new(label: impl Into<OutputLabel>) -> Self {
        Self {
            label: label.into(),
            requires_variables: Vec::new(),
            requires_artifacts: Vec::new(),
            description: None,
            requires_human_approval: false,
            resume_comment_field: None,
        }
    }
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

fn default_max_attempts() -> u32 {
    1
}
fn default_backoff_secs() -> u64 {
    0
}
fn default_backoff_multiplier() -> f64 {
    2.0
}

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

    #[error("State '{state}' declares choice '{label}' but has no transition for it")]
    ChoiceWithoutTransition { state: StateId, label: OutputLabel },

    #[error("State '{state}' has a transition for label '{label}', which is not declared in its choices")]
    TransitionWithoutChoice { state: StateId, label: OutputLabel },

    #[error("State '{state}' declares choice '{label}' more than once")]
    DuplicateChoice { state: StateId, label: OutputLabel },

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

    #[error("State '{state}' has a {action_type} action with an empty 'choices' list")]
    EmptyChoices {
        state: StateId,
        action_type: &'static str,
    },

    #[error(
        "State '{state}' has invalid max_entries {value}; expected at least 1 on an agent action"
    )]
    InvalidMaxEntries { state: StateId, value: u32 },

    #[error("State '{state}' defines {kind} with invalid name '{name}'")]
    InvalidDefinitionName {
        state: StateId,
        kind: &'static str,
        name: String,
    },

    #[error("Choice '{label}' in state '{state}' requires variable '{name}', which the state does not define")]
    UndefinedRequiredVariable {
        state: StateId,
        label: OutputLabel,
        name: String,
    },

    #[error("Choice '{label}' in state '{state}' requires artifact '{name}', which the state does not define")]
    UndefinedRequiredArtifact {
        state: StateId,
        label: OutputLabel,
        name: String,
    },

    #[error("State '{state}' defines local variable '{name}', but only an agent step keeps local variables")]
    LocalVariableOutsideAgent { state: StateId, name: String },

    #[error("State '{state}' is a {action_type} step and cannot define variables or artifacts; only agent and script steps can")]
    DefinitionsNotAllowed {
        state: StateId,
        action_type: &'static str,
    },

    #[error("State '{state}' field '{field}' cannot reference step replies")]
    StepReplyNotAllowed { state: StateId, field: String },

    #[error("State '{state}' references unknown or non-agent step '{step_id}' in '{field}'")]
    InvalidStepReplySource {
        state: StateId,
        step_id: String,
        field: String,
    },

    #[error("State '{state}' field '{field}' cannot reference script step streams")]
    StepStreamNotAllowed { state: StateId, field: String },

    #[error("State '{state}' references unknown or non-script step '{step_id}' in '{field}'")]
    InvalidStepStreamSource {
        state: StateId,
        step_id: String,
        field: String,
    },

    #[error(
        "State '{state}' field '{field}' references '{reference}'; supported fields are steps.<id>.reply, steps.<id>.stdout and steps.<id>.stderr"
    )]
    UnsupportedStepReference {
        state: StateId,
        field: String,
        reference: String,
    },

    #[error("Choice '{label}' in state '{state}' has invalid approval/resume behavior: {reason}")]
    InvalidChoiceBehavior {
        state: StateId,
        label: OutputLabel,
        reason: String,
    },

    // --- Decision correctness (§5.3) ---
    #[error("State '{state}' uses an agent decision without the required field '{field}'")]
    AgentDecisionMissingField { state: StateId, field: &'static str },

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

    // --- Display (§3.6) ---
    #[error("Field '{field}' references unknown input '{variable}'")]
    UnknownInputReferenceInDisplay { field: String, variable: String },

    #[error("Field '{field}' is invalid: {reason}")]
    InvalidDisplay { field: String, reason: String },

    // --- Environment (R15) ---
    #[error("Field '{field}' has an unusable image: {reason}")]
    InvalidImage { field: String, reason: String },

    #[error("Terminal state '{0}' cannot override the image: it runs nothing")]
    ImageOnTerminalState(StateId),
}
