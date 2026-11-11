//! Workflow Definition model
//!
//! Represents the immutable definition of a workflow (the blueprint).
//!
//! The external contract for this model is `docs/dev/workflow-schema.md`; the YAML
//! layer lives in `crate::workflow::schema` and is compiled into these types
//! by `crate::workflow::loader`.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

use crate::domain::{ExecutionStatus, ImageSpec, PromptText};

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
    /// `name`, which `docs/dev/workflow-schema.md` §2 declares to be unique.
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

    /// The schema the definition was written in. Everything past the loader
    /// sees `v2`; this only lets diagnostics name the fields the author
    /// actually wrote (a converted `v1` definition reports
    /// `globals.workspace`, not `globals.repositories[0]`).
    #[serde(default, skip_serializing_if = "SchemaVersion::is_v2")]
    pub source_schema: SchemaVersion,

    /// The state shape the definition was written in.
    #[serde(default, skip_serializing_if = "DefinitionShape::is_legacy")]
    pub shape: DefinitionShape,

    /// Repositories the workflow works on (`globals.repositories` in YAML,
    /// or the converted `globals.workspace` of a `v1` file). Repositories are
    /// per execution, never per state. Empty for a workflow without any.
    #[serde(default)]
    pub repositories: Vec<RepositoryDefinition>,

    /// States in the workflow graph
    pub states: HashMap<StateId, State>,

    /// The entry point state
    pub initial_state: StateId,
}

impl WorkflowDefinition {
    /// The primary repository, if the workflow declares any.
    pub fn primary_repository(&self) -> Option<&RepositoryDefinition> {
        self.repositories
            .iter()
            .find(|repository| repository.primary)
    }

    /// The primary repository as the single-workspace declaration the
    /// single-repository runtime still consumes.
    ///
    /// `None` when the workflow declares no repository or its primary
    /// repository is chosen when the task is created.
    pub fn primary_workspace_spec(&self) -> Option<WorkspaceSpec> {
        self.primary_repository()
            .and_then(RepositoryDefinition::workspace_spec)
    }

    /// What the workflow says each repository is for, by repository name.
    /// A repository without a description is not in the map.
    pub fn repository_descriptions(&self) -> BTreeMap<String, String> {
        self.repositories
            .iter()
            .filter_map(|repository| {
                Some((repository.name.clone(), repository.description.clone()?))
            })
            .collect()
    }

    /// The repositories chosen when a task is created, in the order of the
    /// file.
    pub fn chosen_repositories(&self) -> impl Iterator<Item = &RepositoryDefinition> {
        self.repositories
            .iter()
            .filter(|repository| repository.source == RepositorySource::Chosen)
    }
}

/// Version of the YAML schema a definition was written in (`schema` in YAML).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SchemaVersion {
    /// The original single-workspace schema; also what a missing `schema`
    /// means.
    #[default]
    V1,
    V2,
}

impl SchemaVersion {
    fn is_v2(&self) -> bool {
        *self == SchemaVersion::V2
    }
}

/// The state shape a definition was written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DefinitionShape {
    /// `v1`, or `v2` with `transitions`, a `decision` block and the like.
    #[default]
    Legacy,
    /// `v2` with `choices` as a mapping and `exit`.
    Unified,
}

impl DefinitionShape {
    fn is_legacy(&self) -> bool {
        *self == DefinitionShape::Legacy
    }
}

/// One repository of a workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryDefinition {
    /// Label of the repository and name of its folder in the task directory.
    pub name: String,

    /// Whether this is the workflow's primary repository. Exactly one
    /// repository is.
    pub primary: bool,

    /// Branch a new task starts on. `None` leaves the choice to the
    /// registry's default branch. For a repository chosen when the task is
    /// created it is only the branch the new-task form suggests.
    #[serde(default)]
    pub base_branch: Option<String>,

    /// What the repository is for, in words for the new-task form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Where the repository comes from.
    pub source: RepositorySource,

    /// Paths captured into a checkpoint even though the repository ignores
    /// them (`preserve_paths` in YAML).
    #[serde(default)]
    pub preserve_paths: Vec<String>,
}

impl RepositoryDefinition {
    /// The repository as the single-workspace declaration of the
    /// single-repository runtime; `base_branch` plays the part of `ref`.
    /// `None` for a repository chosen when the task is created.
    pub fn workspace_spec(&self) -> Option<WorkspaceSpec> {
        let spec = match &self.source {
            RepositorySource::Git { repository } => WorkspaceSpec::Git {
                repository: repository.clone(),
                git_ref: self.base_branch.clone(),
                preserve_paths: self.preserve_paths.clone(),
            },
            RepositorySource::Local { path } => WorkspaceSpec::Local {
                path: path.clone(),
                preserve_paths: self.preserve_paths.clone(),
            },
            RepositorySource::Chosen => return None,
        };
        Some(spec)
    }
}

/// Where a workflow repository comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RepositorySource {
    /// Registered repository, by its registry id.
    Git { repository: String },
    /// Plain local folder, relative to the task directory.
    Local { path: String },
    /// A registered repository the owner chooses when creating the task.
    Chosen,
}

/// What a step may touch: the `workspaces` section of a step.
///
/// `root` is where the harness starts. That repository is writable unless it
/// is listed in `read`; `write` adds further writable repositories. A step
/// without the section gets every repository (the primary one writable). See
/// [`WorkspaceAccess`] for how the section maps onto a harness permission.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepWorkspace {
    /// Where the agent starts: a repository name or [`WORKSPACE_ROOT`].
    pub root: String,

    /// Further repositories the step may change.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write: Vec<String>,

    /// Repositories the step may only read, `root` included when named here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read: Vec<String>,
}

/// The `root` value that means the directory holding every repository folder.
pub const WORKSPACE_ROOT: &str = ".";

impl StepWorkspace {
    /// Whether `root` is a repository the step may change: it is one, and
    /// `read` does not take it away.
    pub fn root_is_writable(&self) -> bool {
        self.root != WORKSPACE_ROOT && !self.read.contains(&self.root)
    }

    /// The coarse permission this section amounts to: writable when it
    /// has a repository to write, read-only otherwise.
    pub fn access(&self) -> WorkspaceAccess {
        if self.write.is_empty() && !self.root_is_writable() {
            WorkspaceAccess::ReadOnly
        } else {
            WorkspaceAccess::ReadWrite
        }
    }
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

    /// Models from the cheapest to the strongest. The ceiling and the
    /// escalation compare only models on this list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_ladder: Vec<String>,
}

/// Single-repository workspace declaration: the shape of a `v1`
/// `globals.workspace`, kept as the view the single-repository runtime has of
/// the primary repository (see [`WorkflowDefinition::primary_workspace_spec`]).
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
    /// Also how a definition saved with the former `repository` type reads:
    /// repositories are now chosen per workspace (`globals.workspaces`).
    #[default]
    #[serde(alias = "repository")]
    String,
    /// Multi-line text: a string whose value is prose, shown as a large field in the TUI.
    Text,
    Integer,
    Boolean,
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
/// This is the explicit marker of `docs/dev/workflow-schema.md` §3.5: a workflow says
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
    /// Terminal states carry no action — see `docs/dev/workflow-schema.md` §4, where
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
    /// Declared explicitly in the YAML contract (`docs/dev/workflow-schema.md` §3.5,
    /// the mapping form of `terminal_states`). `None` means the workflow did
    /// not declare one — the list form of `terminal_states`, or a `state.json`
    /// written before this field existed — and the engine then treats the
    /// terminal as a success.
    #[serde(default)]
    pub terminal_status: Option<TerminalStatus>,

    /// Whether manual transition override is allowed
    #[serde(default)]
    pub allow_manual_override: bool,

    /// Repositories this step may touch (`workspace` in YAML). `None` means
    /// all of them, the primary one writable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<StepWorkspace>,

    /// Base-image override for this step (R15): wins over `globals.image`
    /// and over the repository's base image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageSpec>,
}

impl State {
    /// The permission the step's agent runs with: `None` for a step without a
    /// `workspace` section, which leaves the harness's own configuration in
    /// effect.
    pub fn workspace_access(&self) -> Option<WorkspaceAccess> {
        self.workspace.as_ref().map(StepWorkspace::access)
    }

    /// The declaration of the variable `name` on this step.
    pub fn variable(&self, name: &str) -> Option<&VariableDefinition> {
        self.variables.iter().find(|variable| variable.name == name)
    }

    /// Whether `name` is the step's own memory rather than a shared
    /// variable: the declaration decides, and a name this step does not
    /// declare is local only by the `local.` prefix of the older shape.
    ///
    /// The one place that tells the two scopes apart.
    pub fn is_local_variable(&self, name: &str) -> bool {
        match self.variable(name) {
            Some(variable) => variable.is_local(),
            None => name.starts_with(LOCAL_VARIABLE_PREFIX),
        }
    }

    /// Whether the step has a single `exit` instead of a choice to make.
    pub fn has_exit(&self) -> bool {
        self.choices.iter().any(ChoiceDefinition::is_exit)
    }
}

/// Configuration for an action
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActionConfig {
    Agent(AgentActionConfig),
    Script(ScriptActionConfig),
    Human(HumanActionConfig),
    Wait(WaitActionConfig),
    Decision(DecisionActionConfig),
}

/// Decision action configuration: a fork decided by someone other than the
/// step itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionActionConfig {
    /// Who picks the label.
    pub by: DecisionMaker,

    /// What the decider is asked, still templated.
    pub message: PromptText,
}

/// Who decides a [`DecisionActionConfig`] fork.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionMaker {
    /// The conductor always decides; an unsure conductor hands the step to
    /// the operator.
    Conductor,
    /// The operator is asked through a form with the step's variables.
    Human,
}

impl DecisionMaker {
    /// The name this decider has in the YAML contract and in events.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Conductor => "conductor",
            Self::Human => "human",
        }
    }
}

/// The most a step may be raised to (`ceiling` in YAML).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepCeiling {
    /// Highest model; must be on `globals.model_ladder` when that exists.
    #[serde(default)]
    pub model: Option<String>,
    /// Highest effort.
    #[serde(default)]
    pub effort: Option<String>,
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

    /// Default effort of the step, one of `low`, `medium`, `high`, `max`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,

    /// Upper bound for what the conductor and an escalation may set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ceiling: Option<StepCeiling>,

    /// Marks the step whose success makes the conductor profile the steps
    /// that have not started (`plan: true` in YAML).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub plan: bool,

    /// Access the agent receives to its execution workspace.
    ///
    /// Normally derived from the step's `workspace` section
    /// ([`State::workspace_access`]) when the action runs, or set by the
    /// engine for its own read-only runs (reprompts, the conductor). It is
    /// stored only for definitions persisted before step sections existed and
    /// for a `v1` agent on a workflow without a repository. This is
    /// intentionally independent of a particular harness: each adapter
    /// translates it to its native permission mechanism.
    #[serde(default)]
    pub workspace_access: Option<WorkspaceAccess>,

    /// Prompt to send to the agent
    pub prompt: PromptText,

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

/// The workspace permission an agent action runs with, derived from the
/// step's `workspace` section by [`StepWorkspace::access`].
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

/// A variable a step defines: its name, what its value means and how the
/// engine treats it.
///
/// A name with the [`LOCAL_VARIABLE_PREFIX`] is the step's own memory and
/// never reaches the shared context; a variable declared with
/// `scope: local` is the same, under its plain name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VariableDefinition {
    pub name: String,

    /// What the value means. Optional: a variable the workflow sets on its
    /// own may need no explanation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Whether the value is shared or private to the step.
    #[serde(default, skip_serializing_if = "VariableScope::is_global")]
    pub scope: VariableScope,

    /// Who supplies the value.
    #[serde(default, skip_serializing_if = "VariableSource::is_step")]
    pub source: VariableSource,

    /// Declared type of the value (`type` in YAML).
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub variable_type: Option<VariableType>,

    /// The only values the variable may take; empty means any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}

impl VariableDefinition {
    /// A shared variable the step supplies, with a description and nothing
    /// else declared.
    pub fn described(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: Some(description.into()),
            scope: VariableScope::Global,
            source: VariableSource::Step,
            variable_type: None,
            options: Vec::new(),
        }
    }

    /// Whether this variable is the step's own memory: declared with
    /// `scope: local`, or named `local.*`.
    pub fn is_local(&self) -> bool {
        self.scope == VariableScope::Local || self.name.starts_with(LOCAL_VARIABLE_PREFIX)
    }
}

/// The prefix of a variable that stays with its step (`local.memory`).
pub const LOCAL_VARIABLE_PREFIX: &str = "local.";

/// Where a variable's value lives (`scope` in YAML).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VariableScope {
    /// Shared with the whole execution.
    #[default]
    Global,
    /// Private to the step, kept between its entries.
    Local,
}

impl VariableScope {
    fn is_global(&self) -> bool {
        *self == VariableScope::Global
    }
}

/// Who supplies a variable's value in a step (`source` in YAML).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VariableSource {
    /// The step's agent, script or form fills it in.
    #[default]
    Step,
    /// Only the workflow sets it, through `set`; it stays out of the step's
    /// contract and form.
    Workflow,
}

impl VariableSource {
    fn is_step(&self) -> bool {
        *self == VariableSource::Step
    }
}

/// Declared type of a step variable: the types of a form field and of a
/// workflow input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VariableType {
    String,
    /// Multi-line text.
    Text,
    /// One value out of `options`.
    Select,
    Integer,
    Boolean,
}

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
    /// Whether this is the `exit` of a unified linear step. Set by the
    /// loader only: a label called `exit` in an old-shape workflow is an
    /// ordinary choice.
    #[serde(default, skip_serializing_if = "is_false")]
    pub exit: bool,
    /// Variables the workflow sets when this choice is taken (`set` in YAML).
    #[serde(default, skip_serializing_if = "SetDefinition::is_empty")]
    pub set: SetDefinition,
}

fn is_false(value: &bool) -> bool {
    !value
}

/// The label a linear step's `exit` compiles to: the step's only choice.
pub const EXIT_LABEL: &str = "exit";

/// The output label a timed-out action asks for, when the state allows it.
pub const TIMEOUT_LABEL: &str = "timeout";

/// What a choice sets when it is taken: literal values or templates, for
/// declared variables of the step being left.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetDefinition {
    /// Shared variables, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub global: BTreeMap<String, serde_json::Value>,
    /// The step's own `scope: local` variables, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub local: BTreeMap<String, serde_json::Value>,
    /// The writable values of workspaces (`pr`, `pr_url`), by workspace name
    /// and then by value name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub workspace: BTreeMap<String, BTreeMap<String, serde_json::Value>>,
}

impl SetDefinition {
    /// Whether nothing is set.
    pub fn is_empty(&self) -> bool {
        self.global.is_empty() && self.local.is_empty() && self.workspace.is_empty()
    }
}

/// A value a workspace has, readable as `context.workspace.<name>.<field>`.
///
/// This is the one list of workspace fields: reading, writing, validation
/// and their messages all take their fields from here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceField {
    Repo,
    BaseBranch,
    Branch,
    Added,
    Removed,
    Pr,
    PrUrl,
}

impl WorkspaceField {
    /// Every field, in the order documentation and messages list them.
    pub const ALL: [Self; 7] = [
        Self::Repo,
        Self::BaseBranch,
        Self::Branch,
        Self::Added,
        Self::Removed,
        Self::Pr,
        Self::PrUrl,
    ];

    /// The field a key names (`pr_url`, `branch`, ...).
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|field| field.key() == key)
    }

    /// The name of the field in a reference and in `set.workspace`.
    pub fn key(self) -> &'static str {
        match self {
            Self::Repo => "repo",
            Self::BaseBranch => "base_branch",
            Self::Branch => "branch",
            Self::Added => "added",
            Self::Removed => "removed",
            Self::Pr => "pr",
            Self::PrUrl => "pr_url",
        }
    }

    /// Whether `set.workspace` can assign the field. The runner owns the
    /// others and rewrites them after every step.
    pub fn writable(self) -> bool {
        matches!(self, Self::Pr | Self::PrUrl)
    }

    /// The keys of all fields, comma separated, for messages.
    pub fn all_keys() -> String {
        Self::list_keys(Self::ALL.into_iter())
    }

    /// The keys of the writable fields, comma separated, for messages.
    pub fn writable_keys() -> String {
        Self::list_keys(Self::ALL.into_iter().filter(|field| field.writable()))
    }

    fn list_keys(fields: impl Iterator<Item = Self>) -> String {
        fields
            .map(|field| format!("'{}'", field.key()))
            .collect::<Vec<_>>()
            .join(", ")
    }
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
            exit: false,
            set: SetDefinition::default(),
        }
    }

    /// Whether this is the single exit of a linear step.
    pub fn is_exit(&self) -> bool {
        self.exit
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
    /// Validates the workflow definition against `docs/dev/workflow-schema.md` §5.
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

    #[error("State '{state}' is a {action_type} step and cannot define variables or artifacts; only agent and script steps can, and decision steps may define variables")]
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
        "State '{state}' field '{field}' references '{reference}'; supported fields are steps.<id>.reply, steps.<id>.stdout, steps.<id>.stderr and steps.<id>.label"
    )]
    UnsupportedStepReference {
        state: StateId,
        field: String,
        reference: String,
    },

    #[error("State '{state}' field '{field}' references the label of unknown or terminal step '{step_id}'")]
    InvalidStepLabelSource {
        state: StateId,
        step_id: String,
        field: String,
    },

    #[error("State '{state}' field '{field}' has an unusable condition: {reason}")]
    InvalidWhen {
        state: StateId,
        field: String,
        reason: String,
    },

    #[error("State '{state}' declares variable '{name}' more than once")]
    DuplicateVariable { state: StateId, name: String },

    #[error("State '{state}' declares variable '{name}' incorrectly: {reason}")]
    InvalidVariableDeclaration {
        state: StateId,
        name: String,
        reason: String,
    },

    #[error("Variable '{name}' is declared differently in states '{state}' and '{other_state}': {reason}")]
    InconsistentVariable {
        name: String,
        state: StateId,
        other_state: StateId,
        reason: String,
    },

    #[error("Choice '{label}' in state '{state}' requires variable '{name}', which only the workflow sets ('source: workflow')")]
    RequiredWorkflowVariable {
        state: StateId,
        label: OutputLabel,
        name: String,
    },

    #[error("Choice '{label}' in state '{state}' sets 'workspace.{workspace}', but the workflow has no such workspace")]
    SetUnknownWorkspace {
        state: StateId,
        label: OutputLabel,
        workspace: String,
    },

    #[error("Choice '{label}' in state '{state}' sets 'workspace.{workspace}', which is a local folder; only a git workspace has a pull request")]
    SetWorkspaceNotGit {
        state: StateId,
        label: OutputLabel,
        workspace: String,
    },

    #[error("Choice '{label}' in state '{state}' sets 'workspace.{workspace}.{key}', which is not a workspace field (fields: {fields})")]
    SetUnknownWorkspaceValue {
        state: StateId,
        label: OutputLabel,
        workspace: String,
        key: String,
        fields: String,
    },

    #[error("Choice '{label}' in state '{state}' sets 'workspace.{workspace}.{key}', which is read-only; only {writable} can be set")]
    SetReadOnlyWorkspaceValue {
        state: StateId,
        label: OutputLabel,
        workspace: String,
        key: String,
        writable: String,
    },

    #[error("Choice '{label}' in state '{state}' sets variable '{name}', which the state does not declare")]
    SetUnknownVariable {
        state: StateId,
        label: OutputLabel,
        name: String,
    },

    #[error(
        "Choice '{label}' in state '{state}' sets variable '{name}' in the wrong place: {reason}"
    )]
    SetScopeMismatch {
        state: StateId,
        label: OutputLabel,
        name: String,
        reason: String,
    },

    #[error("Choice '{label}' in state '{state}' sets variable '{name}' to a value it does not accept: {reason}")]
    SetValueRejected {
        state: StateId,
        label: OutputLabel,
        name: String,
        reason: String,
    },

    #[error("State '{state}' is a {action_type} step with an 'exit' and cannot map 'exit_codes'; give it 'choices' instead")]
    ExitCodesWithExit {
        state: StateId,
        action_type: &'static str,
    },

    #[error("State '{state}' declares a 'timeout' choice, which only agent and script steps can produce")]
    TimeoutChoiceNotAllowed { state: StateId },

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

    #[error(
        "State '{state}' references artifact '{artifact}' in field '{field}', \
         which no step defines"
    )]
    UnknownArtifactReference {
        state: StateId,
        field: String,
        artifact: String,
    },

    #[error(
        "State '{state}' reads '{reference}' in field '{field}', which is not a value of a \
         workspace: {reason}"
    )]
    InvalidWorkspaceReference {
        state: StateId,
        field: String,
        reference: String,
        reason: String,
    },

    #[error("Field '{field}' references unknown input '{variable}'")]
    UnknownInputReferenceInGlobals { field: String, variable: String },

    // --- Repositories (schema v2) ---
    #[error("Field '{field}' is invalid: {reason}")]
    InvalidRepository { field: String, reason: String },

    #[error("State '{state}': field 'workspace.{field}' is invalid: {reason}")]
    InvalidStepWorkspace {
        state: StateId,
        field: &'static str,
        reason: String,
    },

    #[error("Field '{field}' is invalid: {reason}")]
    InvalidGlobalsField { field: String, reason: String },

    // --- Environment (R15) ---
    #[error("Field '{field}' has an unusable image: {reason}")]
    InvalidImage { field: String, reason: String },

    #[error("Terminal state '{0}' cannot override the image: it runs nothing")]
    ImageOnTerminalState(StateId),
}

#[cfg(test)]
mod workspace_field_tests {
    use super::WorkspaceField;

    #[test]
    fn every_key_names_its_field_and_only_the_pull_request_values_are_writable() {
        let cases = [
            ("repo", false),
            ("base_branch", false),
            ("branch", false),
            ("added", false),
            ("removed", false),
            ("pr", true),
            ("pr_url", true),
        ];
        for (key, writable) in cases {
            let field = WorkspaceField::from_key(key).expect(key);
            assert_eq!(field.key(), key);
            assert_eq!(field.writable(), writable, "{key}");
        }
        assert_eq!(WorkspaceField::ALL.len(), cases.len());
        assert_eq!(WorkspaceField::from_key("title"), None);
    }
}
