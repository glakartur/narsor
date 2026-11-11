//! YAML deserialization layer for workflow definitions.
//!
//! These structures mirror `docs/dev/workflow-schema.md` one to one: field names,
//! nesting and optionality are the external contract, and every struct uses
//! `#[serde(deny_unknown_fields)]` so that a typo in a key is reported instead
//! of being silently ignored.
//!
//! The structures here are deliberately dumb — no defaults are propagated and
//! no graph rules are checked. Compiling them into `crate::domain` types is the
//! job of [`crate::workflow::loader`], validating the result is the job of the
//! validator.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

pub use crate::config::duration::{parse_duration, DurationParseError};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::domain::{
    DecisionMaker, HarnessStepConfig, PromptText, SchemaVersion, SetDefinition, VariableScope,
    VariableSource, VariableType, EXIT_LABEL, LOCAL_VARIABLE_PREFIX,
};

/// The whole workflow file (`docs/dev/workflow-schema.md` §2).
///
/// Two shapes of state exist, see [`StateShape`]. Both are read into the same
/// [`StateSchema`], so everything past parsing is shape-agnostic.
///
/// Which schema the file is written in is read from `schema` and enforced
/// while parsing: a `v2` file that still uses `globals.workspace` or an
/// agent's `workspace_access`, and a `v1` file that uses `globals.workspaces`
/// or a step's `workspace`, is rejected with an error naming the field.
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "RawWorkflowFile")]
pub struct WorkflowFile {
    /// Schema the file is written in; a missing `schema` means `v1`.
    pub schema: SchemaVersion,

    /// Unique workflow name; the loader also derives the definition id from it.
    pub name: String,

    /// Definition version.
    pub version: u32,

    /// Optional description of the workflow's purpose.
    pub description: Option<String>,

    /// Inputs required to start an execution.
    pub inputs: Vec<InputSchema>,

    /// Workflow-wide defaults.
    pub globals: Option<GlobalsSchema>,

    /// The retired `display` section as it was written. It is read only so
    /// that the loader can report that it is ignored.
    pub ignored_display: Option<serde_yaml::Value>,

    /// The state graph. Order is significant: without an explicit
    /// `initial: true` flag the first state is the entry point, so the mapping
    /// is kept as a vector of pairs instead of a `HashMap`.
    pub states: Vec<(String, StateSchema)>,

    /// States whose entry ends the execution, with the status each one
    /// produces. Accepts both forms of §3.5 — see [`TerminalStatesSchema`].
    pub terminal_states: TerminalStatesSchema,

    /// The shape the states were written in.
    pub shape: StateShape,
}

impl WorkflowFile {
    /// Whether the file is `schema: v2` but still written in the legacy
    /// state shape, which only exists so that earlier files keep loading.
    pub fn uses_deprecated_state_shape(&self) -> bool {
        self.schema == SchemaVersion::V2 && self.shape == StateShape::Legacy
    }
}

/// How the states of a file are written.
///
/// * `Legacy`: `choices` is a list and `transitions` a separate map; gates
///   use `inputs`, forks may use a `decision` block. Every `v1` file, and
///   any `v2` file that keeps this form.
/// * `Unified`: `choices` is a mapping from label to exit object with its own
///   `next`, a linear step has `exit`, and a fork decided by someone else is a
///   `decision` action. Only in `schema: v2`.
///
/// The shape is read from the content, not declared: a `v2` file whose states
/// carry a legacy marker (see [`legacy_marker`]) is `Legacy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateShape {
    Legacy,
    Unified,
}

/// The file as it is written, before the schema rules are applied.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkflowFile {
    #[serde(default)]
    schema: SchemaVersion,
    name: String,
    version: u32,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    inputs: Vec<InputSchema>,
    #[serde(default)]
    globals: Option<GlobalsSchema>,
    #[serde(default)]
    display: Option<serde_yaml::Value>,
    #[serde(deserialize_with = "deserialize_ordered_states")]
    states: Vec<(String, serde_yaml::Value)>,
    #[serde(default)]
    terminal_states: TerminalStatesSchema,
}

impl TryFrom<RawWorkflowFile> for WorkflowFile {
    type Error = String;

    fn try_from(raw: RawWorkflowFile) -> Result<Self, Self::Error> {
        let marker = match raw.schema {
            SchemaVersion::V1 => None,
            SchemaVersion::V2 => legacy_marker(&raw.states, &raw.terminal_states),
        };
        let shape = if raw.schema == SchemaVersion::V2 && marker.is_none() {
            StateShape::Unified
        } else {
            StateShape::Legacy
        };
        let states = raw
            .states
            .into_iter()
            .map(|(id, value)| {
                parse_state(shape, &id, value)
                    .map(|state| (id, state))
                    .map_err(|error| match &marker {
                        Some(reason) => format!(
                            "{error} (the file is read in the legacy state shape because \
                             {reason})"
                        ),
                        None => error,
                    })
            })
            .collect::<Result<Vec<_>, String>>()?;

        let globals = raw.globals.as_ref();
        match raw.schema {
            SchemaVersion::V1 => {
                if globals.is_some_and(|globals| globals.workspaces.is_some()) {
                    return Err("'globals.workspaces' requires 'schema: v2'".to_string());
                }
                if let Some((id, _)) = states.iter().find(|(_, state)| state.workspace.is_some()) {
                    return Err(format!("'states.{id}.workspaces' requires 'schema: v2'"));
                }
            }
            SchemaVersion::V2 => {
                if globals.is_some_and(|globals| globals.workspace.is_some()) {
                    return Err(
                        "'globals.workspace' is not allowed in 'schema: v2'; declare \
                         'globals.workspaces'"
                            .to_string(),
                    );
                }
                let with_access = states.iter().find(|(_, state)| {
                    matches!(
                        &state.action,
                        Some(ActionSchema::Agent(agent)) if agent.workspace_access.is_some()
                    )
                });
                if let Some((id, _)) = with_access {
                    return Err(format!(
                        "'states.{id}.action.workspace_access' is not allowed in 'schema: v2'; \
                         use the step's 'workspaces' section"
                    ));
                }
            }
        }
        if shape == StateShape::Unified {
            check_unified_file(&states, &raw.terminal_states)?;
        } else if let Some((id, _)) = states
            .iter()
            .find(|(_, state)| matches!(state.action, Some(ActionSchema::Decision(_))))
        {
            return Err(format!(
                "'states.{id}.action.type' is 'decision', which needs a 'schema: v2' file \
                 in the unified state shape"
            ));
        }
        Ok(Self {
            schema: raw.schema,
            name: raw.name,
            version: raw.version,
            description: raw.description,
            inputs: raw.inputs,
            globals: raw.globals,
            ignored_display: raw.display,
            states,
            terminal_states: raw.terminal_states,
            shape,
        })
    }
}

/// Why a `v2` file is in the legacy shape, if it is: the first thing in it
/// that only that shape has.
///
/// `transitions` and a `decision` block are the legacy shape's own; so is a
/// `choices` written as a list, `allow_manual_override` and `terminal_states`
/// written as a list. A `v2` file with none of them is read in the unified
/// shape.
fn legacy_marker(
    states: &[(String, serde_yaml::Value)],
    terminal_states: &TerminalStatesSchema,
) -> Option<String> {
    let in_a_state = states.iter().find_map(|(id, value)| {
        let state = value.as_mapping()?;
        let has = |key: &str| state.contains_key(key);
        let marker = if has("transitions") {
            "transitions"
        } else if has("decision") {
            "decision"
        } else if has("allow_manual_override") {
            "allow_manual_override"
        } else if state
            .get("choices")
            .is_some_and(serde_yaml::Value::is_sequence)
        {
            "choices as a list"
        } else {
            return None;
        };
        Some(format!("'states.{id}' has '{marker}'"))
    });
    in_a_state.or_else(|| {
        terminal_states
            .is_list_form()
            .then(|| "'terminal_states' is a list".to_string())
    })
}

fn parse_state(
    shape: StateShape,
    id: &str,
    value: serde_yaml::Value,
) -> Result<StateSchema, String> {
    let failed = |error: serde_yaml::Error| format!("states.{id}: {error}");
    let state = match shape {
        StateShape::Legacy => serde_yaml::from_value::<LegacyStateSchema>(value)
            .map(StateSchema::from)
            .map_err(failed),
        StateShape::Unified => serde_yaml::from_value::<UnifiedStateSchema>(value)
            .map_err(failed)
            .and_then(|state| {
                state
                    .into_schema()
                    .map_err(|error| format!("states.{id}: {error}"))
            }),
    }?;
    if state
        .variables
        .iter()
        .any(|variable| variable.name == RESERVED_WORKSPACE_NAME)
    {
        return Err(format!(
            "states.{id}: variable '{RESERVED_WORKSPACE_NAME}' is reserved: \
             'context.{RESERVED_WORKSPACE_NAME}' holds the workspaces of the task"
        ));
    }
    Ok(state)
}

const RESERVED_WORKSPACE_NAME: &str = "workspace";

/// The rules of the unified shape that need more than one state: one form of
/// `terminal_states`, no gate with `inputs`, and no exit on a terminal state.
fn check_unified_file(
    states: &[(String, StateSchema)],
    terminal_states: &TerminalStatesSchema,
) -> Result<(), String> {
    if terminal_states.is_list_form() {
        return Err(
            "'terminal_states' must be a mapping from state id to status in the unified \
                    state shape"
                .to_string(),
        );
    }
    for (id, state) in states {
        if matches!(state.action, Some(ActionSchema::Human(_))) {
            return Err(format!(
                "'states.{id}.action.type' is 'human', which the unified state shape does not \
                 have; use 'type: decision' with 'by: human'"
            ));
        }
        if terminal_states.contains(id) && (state.exit.is_some() || !state.choices.is_empty()) {
            return Err(format!(
                "terminal state '{id}' cannot have 'exit' or 'choices'"
            ));
        }
    }
    Ok(())
}

/// The `terminal_states` section (§3.5), in either of its two forms.
///
/// The contract accepts a plain list of state ids and a mapping from state id
/// to the resulting execution status:
///
/// ```yaml
/// terminal_states:          # mapping form — explicit
///   completed: completed
///   failed: failed
///
/// terminal_states:          # list form — every entry means `completed`
///   - completed
///   - failed
/// ```
///
/// Both compile to the same shape: an ordered list of pairs, where the list
/// form leaves the status undeclared. Order is preserved so that error reports
/// follow the file.
#[derive(Debug, Clone, Default)]
pub struct TerminalStatesSchema {
    entries: Vec<(String, Option<TerminalStatusSchema>)>,
}

impl TerminalStatesSchema {
    /// Builds the section from already-parsed entries.
    pub fn new(entries: Vec<(String, Option<TerminalStatusSchema>)>) -> Self {
        Self { entries }
    }

    /// Whether the list form was used, which declares no status.
    pub fn is_list_form(&self) -> bool {
        self.entries.iter().any(|(_, status)| status.is_none())
    }

    /// The declared state ids, in file order.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(id, _)| id.as_str())
    }

    /// Whether `id` is declared terminal.
    pub fn contains(&self, id: &str) -> bool {
        self.entries.iter().any(|(entry, _)| entry == id)
    }

    /// The status declared for `id`, if the mapping form was used.
    pub fn status_of(&self, id: &str) -> Option<TerminalStatusSchema> {
        self.entries
            .iter()
            .find(|(entry, _)| entry == id)
            .and_then(|(_, status)| *status)
    }
}

/// The status a terminal state produces, as written in the YAML contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalStatusSchema {
    Completed,
    Failed,
    Cancelled,
}

/// The values `terminal_states` accepts, for error messages.
pub const TERMINAL_STATUS_VALUES: &[&str] = &["completed", "failed", "cancelled"];

impl TerminalStatusSchema {
    /// Parses one declared status value.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for TerminalStatesSchema {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct TerminalStatesVisitor;

        impl<'de> Visitor<'de> for TerminalStatesVisitor {
            type Value = TerminalStatesSchema;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str(
                    "a list of terminal state ids, or a mapping from state id to one of: completed, failed, cancelled",
                )
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                // The list form declares no status: every entry is a plain
                // terminal, which the engine ends as `completed`.
                let mut entries = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(id) = seq.next_element::<String>()? {
                    entries.push((id, None));
                }
                Ok(TerminalStatesSchema::new(entries))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(0));
                while let Some((id, value)) = map.next_entry::<String, String>()? {
                    let status = TerminalStatusSchema::parse(value.trim()).ok_or_else(|| {
                        de::Error::custom(format!(
                            "terminal state '{id}' declares unknown status '{value}'; permitted values are {}",
                            TERMINAL_STATUS_VALUES.join(", ")
                        ))
                    })?;
                    entries.push((id, Some(status)));
                }
                Ok(TerminalStatesSchema::new(entries))
            }
        }

        deserializer.deserialize_any(TerminalStatesVisitor)
    }
}

/// One entry of the `inputs` list (§3.1).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputSchema {
    pub name: String,

    /// `string`, `integer`, `boolean` or `repository`.
    #[serde(default, rename = "type")]
    pub param_type: ParamTypeSchema,

    #[serde(default)]
    pub required: bool,

    #[serde(default)]
    pub default: Option<serde_json::Value>,

    #[serde(default)]
    pub description: Option<String>,
}

/// Declared type of an input parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ParamTypeSchema {
    #[default]
    String,
    Text,
    Integer,
    Boolean,
}

/// The `globals` section (§3.2).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalsSchema {
    /// Default timeout for a state.
    #[serde(default)]
    pub timeout: Option<DurationSecs>,

    /// Default retry policy for actions.
    #[serde(default)]
    pub retry_policy: Option<RetryPolicySchema>,

    /// Default workspace for the execution (`schema: v1` only).
    #[serde(default)]
    pub workspace: Option<WorkspaceSchema>,

    /// Workspaces of the workflow, one repository each, in file order
    /// (`schema: v2` only).
    #[serde(default, deserialize_with = "deserialize_ordered_workspaces")]
    pub workspaces: Option<Vec<RepositorySchema>>,

    /// Base image for every step of a remote task, overriding the
    /// repository's own (R15). A reference string, or a mapping with
    /// `reference` and `digest_policy` — see [`crate::domain::ImageSpec`].
    #[serde(default)]
    pub image: Option<crate::domain::ImageSpec>,

    /// The model tier of an agent step that names none: `small`, `medium`,
    /// `large` or `max`. The step's own `model` wins.
    #[serde(default)]
    pub default_model: Option<String>,

    /// The effort of an agent step that names none. The step's own `effort`
    /// wins.
    #[serde(default)]
    pub default_effort: Option<String>,

    /// The general ceiling: the most a step without its own ceiling may be
    /// raised to. A step's own `ceiling` field wins field by field.
    #[serde(default)]
    pub ceiling: Option<crate::domain::StepCeiling>,
}

/// Retry policy, both globally and per state (§3.2, §3.3).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryPolicySchema {
    /// Number of attempts, including the first one. Defaults to 1.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,

    /// Delay between attempts.
    #[serde(default)]
    pub backoff: Option<DurationSecs>,
}

fn default_max_attempts() -> u32 {
    1
}

/// The `globals.workspace` section (§3.2).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkspaceSchema {
    /// Git checkout; needs `repository` and usually `ref`.
    Git(GitWorkspaceSchema),
    /// Local folder; needs `path`.
    Local(LocalWorkspaceSchema),
}

/// `globals.workspace` with `type: git`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitWorkspaceSchema {
    /// Repository id from the registry.
    pub repository: String,

    /// Branch, tag or commit.
    #[serde(default, rename = "ref")]
    pub git_ref: Option<String>,

    /// Paths captured into a checkpoint even though the repository ignores them.
    ///
    /// The one exception to "ignored files do not travel": a file a step
    /// generates, the repository ignores, and a later step needs — a
    /// generated lockfile being the usual one. Everything else is rebuilt by
    /// the worker that takes the execution over.
    ///
    /// A preserved path travels into the checkpoint bundle un-encrypted, so it
    /// must never name a file carrying secrets or credentials (e.g. `.env`) —
    /// R33 requires that nothing in the record or checkpoint carries
    /// credentials.
    #[serde(default)]
    pub preserve_paths: Vec<String>,
}

/// `globals.workspace` with `type: local`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalWorkspaceSchema {
    /// Path to the folder, relative to the task directory.
    pub path: String,

    /// Paths captured into a checkpoint even though the repository ignores them.
    ///
    /// The one exception to "ignored files do not travel": a file a step
    /// generates, the repository ignores, and a later step needs — a
    /// generated lockfile being the usual one. Everything else is rebuilt by
    /// the worker that takes the execution over.
    ///
    /// A preserved path travels into the checkpoint bundle un-encrypted, so it
    /// must never name a file carrying secrets or credentials (e.g. `.env`) —
    /// R33 requires that nothing in the record or checkpoint carries
    /// credentials.
    #[serde(default)]
    pub preserve_paths: Vec<String>,
}

/// One entry of `globals.workspaces` (`schema: v2`).
///
/// At most one of `repository` and `path` names the source; with neither,
/// the owner chooses the repository when creating the task.
#[derive(Debug, Clone)]
pub struct RepositorySchema {
    /// The key of the entry: the label of the repository and the name of its
    /// folder in the task directory.
    pub name: String,

    /// Marks the primary repository; exactly one repository is primary.
    pub primary: bool,

    /// Branch a new task starts from (`base_branch`).
    pub base_branch: Option<String>,

    /// What the repository is for, in words for the new-task form.
    pub description: Option<String>,

    /// Where the repository comes from.
    pub source: RepositorySourceSchema,

    /// Paths captured into a checkpoint even though the repository ignores
    /// them; the same caveats as [`GitWorkspaceSchema::preserve_paths`].
    pub preserve_paths: Vec<String>,
}

/// The source of a `globals.workspaces` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepositorySourceSchema {
    /// Repository id from the registry.
    Repository(String),
    /// Local folder, relative to the task directory.
    Path(String),
    /// Chosen by the owner when the task is created.
    Chosen,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRepositorySchema {
    #[serde(default)]
    primary: bool,
    #[serde(default)]
    base_branch: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    preserve_paths: Vec<String>,
}

impl RawRepositorySchema {
    fn into_schema(self, name: String) -> Result<RepositorySchema, String> {
        let raw = self;
        let source = match (raw.repository, raw.path) {
            (Some(repository), None) => RepositorySourceSchema::Repository(repository),
            (None, Some(path)) => RepositorySourceSchema::Path(path),
            (None, None) => RepositorySourceSchema::Chosen,
            (Some(_), Some(_)) => {
                return Err(format!(
                    "workspace '{name}' takes 'repository' or 'path', not both"
                ))
            }
        };
        Ok(RepositorySchema {
            name,
            primary: raw.primary,
            base_branch: raw.base_branch,
            description: raw.description,
            source,
            preserve_paths: raw.preserve_paths,
        })
    }
}

/// The `workspaces` section of a step (`schema: v2`): which repositories the
/// step gets and where the agent starts.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepWorkspaceSchema {
    /// A repository name, or `.` for the directory holding all of them. The
    /// repository is writable unless `read` lists it.
    pub root: String,

    /// Further repositories the step may change.
    #[serde(default)]
    pub write: Vec<String>,

    /// Repositories the step may only read.
    #[serde(default)]
    pub read: Vec<String>,
}

/// A single state of the graph (§3.3), whichever shape it was written in.
///
/// Built from a [`LegacyStateSchema`] or an [`UnifiedStateSchema`]; the
/// loader compiles this one. A unified state has no `transitions` of its own:
/// the `next` of each choice is carried over into `transitions`, and `exit`
/// stays separate until the loader turns it into the reserved choice.
#[derive(Debug, Clone)]
pub struct StateSchema {
    /// Maximum number of entries before operator intervention.
    pub max_entries: Option<u32>,
    pub description: Option<String>,

    /// Action executed on entry. Terminal states have none (§4).
    pub action: Option<ActionSchema>,

    /// Variables the step's action may set.
    pub variables: Vec<VariableSchema>,

    /// Artifacts the step's action may produce (agent and script steps
    /// only).
    pub artifacts: Vec<DefinitionSchema>,

    /// Possible choices, each optionally described — see [`ChoiceSchema`].
    pub choices: Vec<ChoiceSchema>,

    /// The only way out of a linear step (unified shape only).
    pub exit: Option<ExitSchema>,

    /// Mapping from output label to target state.
    pub transitions: HashMap<String, String>,

    /// Decision provider; defaults to `rules` (legacy shape only).
    pub decision: Option<DecisionSchema>,

    /// Overrides `globals.timeout`.
    pub timeout: Option<DurationSecs>,

    /// Overrides `globals.retry_policy`.
    pub retry_policy: Option<RetryPolicySchema>,

    /// Marks the entry point explicitly, overriding file order (§5.1).
    pub initial: bool,

    /// Allows the operator to force an arbitrary transition out of this state
    /// (legacy shape only).
    pub allow_manual_override: bool,

    /// Base image for this step only, overriding `globals.image` and the
    /// repository's (R15).
    pub image: Option<crate::domain::ImageSpec>,

    /// Repositories the step gets (`schema: v2` only). Without it the step
    /// gets all of them, the primary one writable.
    pub workspace: Option<StepWorkspaceSchema>,
}

/// A state in the legacy shape, as it is written (§3.3).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyStateSchema {
    #[serde(default)]
    max_entries: Option<u32>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    action: Option<ActionSchema>,
    #[serde(default)]
    variables: Vec<DefinitionSchema>,
    #[serde(default)]
    artifacts: Vec<DefinitionSchema>,
    #[serde(default)]
    choices: Vec<ChoiceSchema>,
    #[serde(default)]
    transitions: HashMap<String, String>,
    #[serde(default)]
    decision: Option<DecisionSchema>,
    #[serde(default)]
    timeout: Option<DurationSecs>,
    #[serde(default)]
    retry_policy: Option<RetryPolicySchema>,
    #[serde(default)]
    initial: bool,
    #[serde(default)]
    allow_manual_override: bool,
    #[serde(default)]
    image: Option<crate::domain::ImageSpec>,
    #[serde(default, rename = "workspaces")]
    workspace: Option<StepWorkspaceSchema>,
}

impl From<LegacyStateSchema> for StateSchema {
    fn from(state: LegacyStateSchema) -> Self {
        StateSchema {
            max_entries: state.max_entries,
            description: state.description,
            action: state.action,
            variables: state
                .variables
                .into_iter()
                .map(VariableSchema::from)
                .collect(),
            artifacts: state.artifacts,
            choices: state.choices,
            exit: None,
            transitions: state.transitions,
            decision: state.decision,
            timeout: state.timeout,
            retry_policy: state.retry_policy,
            initial: state.initial,
            allow_manual_override: state.allow_manual_override,
            image: state.image,
            workspace: state.workspace,
        }
    }
}

/// A state in the unified shape, as it is written.
///
/// `choices` is a mapping from label to exit object, `exit` the single exit
/// of a linear step; a step has one of them. There is no `transitions`, no
/// `decision` block and no `allow_manual_override`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnifiedStateSchema {
    #[serde(default)]
    max_entries: Option<u32>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    action: Option<ActionSchema>,
    #[serde(default)]
    variables: Vec<VariableSchema>,
    #[serde(default)]
    artifacts: Vec<DefinitionSchema>,
    #[serde(default, deserialize_with = "deserialize_ordered_choices")]
    choices: Vec<(String, ExitObjectSchema)>,
    #[serde(default)]
    exit: Option<ExitObjectSchema>,
    #[serde(default)]
    timeout: Option<DurationSecs>,
    #[serde(default)]
    retry_policy: Option<RetryPolicySchema>,
    #[serde(default)]
    initial: bool,
    #[serde(default)]
    image: Option<crate::domain::ImageSpec>,
    #[serde(default, rename = "workspaces")]
    workspace: Option<StepWorkspaceSchema>,
}

impl UnifiedStateSchema {
    /// Applies the rules of a single unified state and converts it.
    fn into_schema(self) -> Result<StateSchema, String> {
        if self.exit.is_some() && !self.choices.is_empty() {
            return Err("a step has 'exit' or 'choices', never both".to_string());
        }
        if let Some(variable) = self
            .variables
            .iter()
            .find(|variable| variable.name.starts_with(LOCAL_VARIABLE_PREFIX))
        {
            return Err(format!(
                "variable '{}' cannot be named with '{LOCAL_VARIABLE_PREFIX}'; declare it with \
                 'scope: local'",
                variable.name
            ));
        }

        let mut choices = Vec::with_capacity(self.choices.len());
        let mut transitions = HashMap::with_capacity(self.choices.len());
        for (label, object) in self.choices {
            if label == EXIT_LABEL {
                return Err(format!(
                    "'{EXIT_LABEL}' is reserved for the step's 'exit' and cannot be a choice label"
                ));
            }
            let (choice, next) = object.into_choice(label.clone())?;
            transitions.insert(label, next);
            choices.push(choice);
        }
        let exit = self
            .exit
            .map(|object| {
                object
                    .into_choice(EXIT_LABEL.to_string())
                    .map(|(choice, next)| ExitSchema { choice, next })
            })
            .transpose()?;

        Ok(StateSchema {
            max_entries: self.max_entries,
            description: self.description,
            action: self.action,
            variables: self.variables,
            artifacts: self.artifacts,
            choices,
            exit,
            transitions,
            decision: None,
            timeout: self.timeout,
            retry_policy: self.retry_policy,
            initial: self.initial,
            allow_manual_override: false,
            image: self.image,
            workspace: self.workspace,
        })
    }
}

/// The `exit` of a unified linear step: its only choice and where it leads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitSchema {
    /// The choice, labelled [`EXIT_LABEL`].
    pub choice: ChoiceSchema,
    /// The state the step leads to.
    pub next: String,
}

/// An exit object as written under `choices.<label>` or `exit`: what the
/// choice requires, what it sets, and where it leads.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExitObjectSchema {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    requires_variables: Vec<String>,
    #[serde(default)]
    requires_artifacts: Vec<String>,
    #[serde(default)]
    requires_human_approval: bool,
    #[serde(default)]
    resume_comment_field: Option<String>,
    #[serde(default)]
    set: BTreeMap<String, serde_json::Value>,
    next: String,
}

impl ExitObjectSchema {
    fn into_choice(self, label: String) -> Result<(ChoiceSchema, String), String> {
        let set = parse_set(self.set).map_err(|error| format!("choice '{label}': {error}"))?;
        let choice = ChoiceSchema {
            label,
            description: self.description,
            requires_variables: self.requires_variables,
            requires_artifacts: self.requires_artifacts,
            requires_human_approval: self.requires_human_approval,
            resume_comment_field: self.resume_comment_field,
            set,
        };
        Ok((choice, self.next))
    }
}

/// Reads a `set` mapping: a name per shared variable, the reserved key
/// `local` holding a mapping of the step's own variables, and the reserved
/// key `workspace` holding, per workspace name, a mapping of the pull
/// request values to set. Every value is text, a number or a boolean.
fn parse_set(raw: BTreeMap<String, serde_json::Value>) -> Result<SetDefinition, String> {
    let mut set = SetDefinition::default();
    for (name, value) in raw {
        if name == "local" {
            let serde_json::Value::Object(locals) = value else {
                return Err("'set.local' must be a mapping of variable name to value".to_string());
            };
            for (local_name, local_value) in locals {
                check_set_value(&format!("set.local.{local_name}"), &local_value)?;
                set.local.insert(local_name, local_value);
            }
        } else if name == RESERVED_WORKSPACE_NAME {
            set.workspace = parse_set_workspace(value)?;
        } else {
            check_set_value(&format!("set.{name}"), &value)?;
            set.global.insert(name, value);
        }
    }
    Ok(set)
}

/// Reads `set.workspace`: a mapping from workspace name to a mapping of
/// value name to value. The validator checks the names.
fn parse_set_workspace(
    value: serde_json::Value,
) -> Result<BTreeMap<String, BTreeMap<String, serde_json::Value>>, String> {
    const SHAPE: &str = "'set.workspace' must be a mapping of workspace name to a mapping of \
                         value name to value";
    let serde_json::Value::Object(workspaces) = value else {
        return Err(SHAPE.to_string());
    };
    let mut parsed = BTreeMap::new();
    for (workspace, values) in workspaces {
        let serde_json::Value::Object(values) = values else {
            return Err(SHAPE.to_string());
        };
        let mut assigned = BTreeMap::new();
        for (key, value) in values {
            check_set_value(&format!("set.workspace.{workspace}.{key}"), &value)?;
            assigned.insert(key, value);
        }
        parsed.insert(workspace, assigned);
    }
    Ok(parsed)
}

fn check_set_value(field: &str, value: &serde_json::Value) -> Result<(), String> {
    match value {
        serde_json::Value::String(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::Bool(_) => Ok(()),
        _ => Err(format!("'{field}' must be text, a number or true/false")),
    }
}

/// One entry of a state's `variables` list.
///
/// A legacy entry has a name and a description; a unified one may also say
/// where the value lives (`scope`), who supplies it (`source`), what type it
/// has and which values it accepts.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariableSchema {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub scope: VariableScope,
    #[serde(default)]
    pub source: VariableSource,
    #[serde(default, rename = "type")]
    pub variable_type: Option<VariableType>,
    #[serde(default)]
    pub options: Vec<String>,
}

impl From<DefinitionSchema> for VariableSchema {
    fn from(definition: DefinitionSchema) -> Self {
        VariableSchema {
            name: definition.name,
            description: Some(definition.description),
            scope: VariableScope::Global,
            source: VariableSource::Step,
            variable_type: None,
            options: Vec::new(),
        }
    }
}

/// One entry of a state's `variables` or `artifacts` list: a name and what
/// it means.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefinitionSchema {
    pub name: String,
    pub description: String,
}

/// One entry of a state's `choices` list (§3.3), in either of its two forms.
///
/// A bare label is the short form; the object form adds a description the
/// engine shows wherever the label is offered — the agent's output contract,
/// the input form, the transition menu — and what the choice requires:
///
/// ```yaml
/// choices:
///   - success                                   # bare label
///   - label: needs_human
///     description: A person has to decide first  # described label
///     requires_variables: [summary]
///     requires_artifacts: [plan_doc]
/// ```
///
/// Both compile to the same [`crate::domain::ChoiceDefinition`]; a bare label
/// simply has no description and no requirements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceSchema {
    pub label: String,
    pub description: Option<String>,
    pub requires_variables: Vec<String>,
    pub requires_artifacts: Vec<String>,
    pub requires_human_approval: bool,
    pub resume_comment_field: Option<String>,
    /// What taking the choice sets (unified shape only).
    pub set: SetDefinition,
}

/// The object form of a [`ChoiceSchema`]. `deny_unknown_fields` keeps a
/// misspelled `descripton:` from being dropped silently.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChoiceObjectSchema {
    label: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    requires_variables: Vec<String>,
    #[serde(default)]
    requires_artifacts: Vec<String>,
    #[serde(default)]
    requires_human_approval: bool,
    #[serde(default)]
    resume_comment_field: Option<String>,
}

impl ChoiceSchema {
    fn bare(label: &str) -> Self {
        ChoiceSchema {
            label: label.to_string(),
            description: None,
            requires_variables: Vec::new(),
            requires_artifacts: Vec::new(),
            requires_human_approval: false,
            resume_comment_field: None,
            set: SetDefinition::default(),
        }
    }
}

impl From<ChoiceObjectSchema> for ChoiceSchema {
    fn from(object: ChoiceObjectSchema) -> Self {
        ChoiceSchema {
            label: object.label,
            description: object.description,
            requires_variables: object.requires_variables,
            requires_artifacts: object.requires_artifacts,
            requires_human_approval: object.requires_human_approval,
            resume_comment_field: object.resume_comment_field,
            set: SetDefinition::default(),
        }
    }
}

/// A hand-written visitor rather than `#[serde(untagged)]`: an untagged enum
/// replaces the object form's own error (a misspelled key, a missing label)
/// with a generic one.
impl<'de> Deserialize<'de> for ChoiceSchema {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ChoiceVisitor;

        impl<'de> Visitor<'de> for ChoiceVisitor {
            type Value = ChoiceSchema;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str(
                    "a choice label, or a mapping with 'label' and an optional 'description'",
                )
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(ChoiceSchema::bare(value))
            }

            fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                ChoiceObjectSchema::deserialize(de::value::MapAccessDeserializer::new(map))
                    .map(ChoiceSchema::from)
            }
        }

        deserializer.deserialize_any(ChoiceVisitor)
    }
}

/// Action configuration (§3.3.A–D).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActionSchema {
    Agent(AgentActionSchema),
    Script(ScriptActionSchema),
    Human(HumanActionSchema),
    Wait(WaitActionSchema),
    /// Unified shape only.
    Decision(DecisionActionSchema),
}

/// `type: decision`: a fork someone other than the step decides.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionActionSchema {
    /// `conductor` or `human`.
    pub by: DecisionMaker,

    /// What the decider is asked; templated like an agent prompt.
    pub message: PromptText,
}

/// `type: agent` (§3.3.A).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentActionSchema {
    /// Optional: an action that names no harness uses the configured default,
    /// and failing that the built-in one (D-F).
    ///
    /// `deny_unknown_fields` still catches a misspelled `harnes:`; what
    /// changes is only the status of the field being absent.
    #[serde(default)]
    pub harness: Option<String>,

    #[serde(default)]
    pub model: Option<String>,

    /// Default effort: `low`, `medium`, `high` or `max`.
    #[serde(default)]
    pub effort: Option<String>,

    /// The most the conductor or an escalation may raise the step to.
    #[serde(default)]
    pub ceiling: Option<crate::domain::StepCeiling>,

    /// Marks the plan step: after it succeeds the conductor profiles the
    /// steps that have not started.
    #[serde(default)]
    pub plan: bool,

    /// Workspace permission, translated to native harness permissions at run
    /// time. Omission leaves the harness's own configuration in effect.
    #[serde(default)]
    pub workspace_access: Option<crate::domain::WorkspaceAccess>,

    pub prompt: PromptText,

    #[serde(default)]
    pub parameters: HashMap<String, serde_json::Value>,

    /// Extra folders mounted read-only, relative to the workspace.
    #[serde(default)]
    pub read_only_paths: Vec<String>,

    /// Whether the engine adds earlier attempts to the prompt when the step
    /// is entered again. On unless turned off.
    #[serde(default = "default_history")]
    pub history: bool,
}

fn default_history() -> bool {
    true
}

impl AgentActionSchema {
    /// The harness session settings the step declares, before `globals` fill
    /// what it leaves out. The YAML contract has no per-action workspace (§6
    /// lists it as a future extension) and no permission grants, so both stay
    /// unset.
    pub fn harness_step(&self) -> HarnessStepConfig {
        HarnessStepConfig {
            harness: self.harness.clone(),
            permission_grants: Vec::new(),
            model: self.model.clone(),
            effort: self.effort.clone(),
            ceiling: self.ceiling.clone(),
            workspace_access: self.workspace_access,
            workspace: None,
            parameters: self.parameters.clone(),
            read_only_paths: self.read_only_paths.clone(),
        }
    }
}

/// `type: script` (§3.3.B).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptActionSchema {
    pub command: String,

    /// Arguments, each one a separate list entry.
    #[serde(default)]
    pub args: Vec<String>,

    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Mapping from process exit code to output label.
    #[serde(default)]
    pub exit_codes: HashMap<i32, String>,

    #[serde(default)]
    pub working_dir: Option<String>,

    #[serde(default)]
    pub timeout: Option<DurationSecs>,
}

/// `type: human` (§3.3.C).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanActionSchema {
    /// Message shown in the UI; the domain calls this field `instruction`.
    pub message: String,

    /// Form fields the operator fills in.
    #[serde(default)]
    pub inputs: Vec<HumanInputSchema>,
}

/// One form field of a human action (§3.3.C).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanInputSchema {
    pub name: String,

    #[serde(rename = "type")]
    pub field_type: HumanInputTypeSchema,

    /// Allowed values for a `select` field.
    #[serde(default)]
    pub options: Vec<String>,

    #[serde(default)]
    pub required: bool,

    #[serde(default)]
    pub description: Option<String>,
}

/// Kind of a human form field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanInputTypeSchema {
    Text,
    Select,
}

/// `type: wait` (§3.3.D).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitActionSchema {
    /// The domain calls this field `event`.
    pub event_type: String,

    #[serde(default)]
    pub filter: Option<serde_json::Value>,

    #[serde(default)]
    pub timeout: Option<DurationSecs>,
}

/// Decision provider configuration (§3.4).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DecisionSchema {
    /// Static label mapping — the default.
    Rules,
    Agent(AgentDecisionSchema),
    Human(HumanDecisionSchema),
}

/// `decision: { type: agent }` (§3.4.B).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDecisionSchema {
    #[serde(default)]
    pub harness: Option<String>,

    #[serde(default)]
    pub prompt: Option<String>,
}

/// `decision: { type: human }` (§3.4.C).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanDecisionSchema {
    #[serde(default)]
    pub message: Option<String>,
}

// ---------------------------------------------------------------------------
// Durations
// ---------------------------------------------------------------------------

/// A duration from the YAML contract, normalized to whole seconds.
///
/// Accepts a bare integer (seconds) or a string built from `<number><unit>`
/// groups, where the unit is `s`, `m`, `h` or `d` — for example `10s`, `30m`,
/// `2h` or `1h30m`. The domain model only ever stores seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurationSecs(pub u64);

impl DurationSecs {
    /// Returns the duration in whole seconds.
    pub fn as_secs(self) -> u64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for DurationSecs {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct DurationVisitor;

        impl<'de> Visitor<'de> for DurationVisitor {
            type Value = DurationSecs;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter
                    .write_str("a duration such as \"10s\", \"30m\", \"2h\" or a number of seconds")
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(DurationSecs(value))
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                if value < 0 {
                    return Err(E::custom(format!("duration must not be negative: {value}")));
                }
                Ok(DurationSecs(value as u64))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                parse_duration(value)
                    .map(DurationSecs)
                    .map_err(|err| E::custom(err.to_string()))
            }
        }

        deserializer.deserialize_any(DurationVisitor)
    }
}

// ---------------------------------------------------------------------------
// Order-preserving state mapping
// ---------------------------------------------------------------------------

/// Deserializes `globals.workspaces`, a mapping of name to repository, keeping
/// the order of the file.
fn deserialize_ordered_workspaces<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<RepositorySchema>>, D::Error>
where
    D: Deserializer<'de>,
{
    struct WorkspacesVisitor;

    impl<'de> Visitor<'de> for WorkspacesVisitor {
        type Value = Option<Vec<RepositorySchema>>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a mapping of workspace name to repository definition")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut workspaces = Vec::with_capacity(map.size_hint().unwrap_or(0));
            while let Some((name, raw)) = map.next_entry::<String, RawRepositorySchema>()? {
                if workspaces
                    .iter()
                    .any(|entry: &RepositorySchema| entry.name == name)
                {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate workspace '{name}'"
                    )));
                }
                workspaces.push(raw.into_schema(name).map_err(serde::de::Error::custom)?);
            }
            Ok(Some(workspaces))
        }
    }

    deserializer.deserialize_map(WorkspacesVisitor)
}

/// Deserializes the `states` mapping while preserving the order of the file.
///
/// `HashMap` would lose that order, and the order decides which state is the
/// entry point when no state is flagged `initial: true`. Each state is kept
/// as raw YAML here, because which shape it is parsed as depends on the whole
/// file (see [`StateShape`]).
fn deserialize_ordered_states<'de, D>(
    deserializer: D,
) -> Result<Vec<(String, serde_yaml::Value)>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_ordered_map(deserializer, "a mapping of state id to state definition")
}

/// Deserializes the `choices` mapping of a unified state, keeping the order
/// of the file: the order is how choices are listed to the agent and the
/// operator.
fn deserialize_ordered_choices<'de, D>(
    deserializer: D,
) -> Result<Vec<(String, ExitObjectSchema)>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_ordered_map(
        deserializer,
        "a mapping of choice label to exit object (a legacy list of labels is not allowed here)",
    )
}

fn deserialize_ordered_map<'de, D, V>(
    deserializer: D,
    expecting: &'static str,
) -> Result<Vec<(String, V)>, D::Error>
where
    D: Deserializer<'de>,
    V: Deserialize<'de>,
{
    struct OrderedMapVisitor<V> {
        expecting: &'static str,
        value: std::marker::PhantomData<V>,
    }

    impl<'de, V: Deserialize<'de>> Visitor<'de> for OrderedMapVisitor<V> {
        type Value = Vec<(String, V)>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str(self.expecting)
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(0));
            while let Some((key, value)) = map.next_entry::<String, V>()? {
                if entries.iter().any(|(existing, _)| existing == &key) {
                    return Err(de::Error::custom(format!("duplicate entry '{key}'")));
                }
                entries.push((key, value));
            }
            Ok(entries)
        }
    }

    deserializer.deserialize_map(OrderedMapVisitor {
        expecting,
        value: std::marker::PhantomData,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_seconds_minutes_and_hours() {
        assert_eq!(parse_duration("10s").unwrap(), 10);
        assert_eq!(parse_duration("30m").unwrap(), 1800);
        assert_eq!(parse_duration("2h").unwrap(), 7200);
        assert_eq!(parse_duration("1d").unwrap(), 86_400);
    }

    #[test]
    fn parses_bare_integer_as_seconds() {
        assert_eq!(parse_duration("45").unwrap(), 45);
    }

    #[test]
    fn parses_compound_duration() {
        assert_eq!(parse_duration("1h30m").unwrap(), 5400);
    }

    #[test]
    fn ignores_surrounding_whitespace() {
        assert_eq!(parse_duration("  2h  ").unwrap(), 7200);
    }

    #[test]
    fn rejects_unknown_unit() {
        let err = parse_duration("2 hours").unwrap_err();
        assert!(matches!(err, DurationParseError::UnknownUnit { .. }));
        assert!(err.to_string().contains("hours"));
    }

    #[test]
    fn rejects_missing_number() {
        let err = parse_duration("h").unwrap_err();
        assert!(matches!(err, DurationParseError::Invalid { .. }));
    }

    #[test]
    fn rejects_empty_input() {
        assert!(matches!(
            parse_duration("   ").unwrap_err(),
            DurationParseError::Empty(_)
        ));
    }

    #[test]
    fn deserializes_duration_from_string_and_integer() {
        let from_string: DurationSecs = serde_yaml::from_str("\"30m\"").unwrap();
        assert_eq!(from_string.as_secs(), 1800);

        let from_int: DurationSecs = serde_yaml::from_str("120").unwrap();
        assert_eq!(from_int.as_secs(), 120);
    }

    #[test]
    fn deserialization_of_bad_duration_is_an_error_not_a_panic() {
        let result: Result<DurationSecs, _> = serde_yaml::from_str("\"2 hours\"");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("hours"), "unexpected message: {err}");
    }

    #[test]
    fn preserves_state_order() {
        let yaml = r#"
name: ordered
version: 1
states:
  first:
    description: a
  second:
    description: b
  third:
    description: c
"#;
        let file: WorkflowFile = serde_yaml::from_str(yaml).unwrap();
        let ids: Vec<&str> = file.states.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["first", "second", "third"]);
    }
}
