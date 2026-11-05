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

use std::collections::HashMap;
use std::fmt;

pub use crate::config::duration::{parse_duration, DurationParseError};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::domain::SchemaVersion;

/// The whole workflow file (`docs/dev/workflow-schema.md` §2).
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

    /// How a task introduces itself: title and links (§3.6).
    pub display: Option<DisplaySchema>,

    /// The state graph. Order is significant: without an explicit
    /// `initial: true` flag the first state is the entry point, so the mapping
    /// is kept as a vector of pairs instead of a `HashMap`.
    pub states: Vec<(String, StateSchema)>,

    /// States whose entry ends the execution, with the status each one
    /// produces. Accepts both forms of §3.5 — see [`TerminalStatesSchema`].
    pub terminal_states: TerminalStatesSchema,
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
    display: Option<DisplaySchema>,
    #[serde(deserialize_with = "deserialize_ordered_states")]
    states: Vec<(String, StateSchema)>,
    #[serde(default)]
    terminal_states: TerminalStatesSchema,
}

impl TryFrom<RawWorkflowFile> for WorkflowFile {
    type Error = String;

    fn try_from(raw: RawWorkflowFile) -> Result<Self, Self::Error> {
        let globals = raw.globals.as_ref();
        match raw.schema {
            SchemaVersion::V1 => {
                if globals.is_some_and(|globals| globals.workspaces.is_some()) {
                    return Err("'globals.workspaces' requires 'schema: v2'".to_string());
                }
                if let Some((id, _)) = raw
                    .states
                    .iter()
                    .find(|(_, state)| state.workspace.is_some())
                {
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
                let with_access = raw.states.iter().find(|(_, state)| {
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
        Ok(Self {
            schema: raw.schema,
            name: raw.name,
            version: raw.version,
            description: raw.description,
            inputs: raw.inputs,
            globals: raw.globals,
            display: raw.display,
            states: raw.states,
            terminal_states: raw.terminal_states,
        })
    }
}

/// The `display` section (§3.6): templates for a task's title and links.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisplaySchema {
    /// Template of the task title.
    #[serde(default)]
    pub title: Option<String>,

    /// Links shown next to the title.
    #[serde(default)]
    pub links: Vec<DisplayLinkSchema>,
}

/// One entry of `display.links`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisplayLinkSchema {
    /// What the link is; unique within the section.
    pub label: String,

    /// Template of the visible text.
    #[serde(default)]
    pub text: Option<String>,

    /// Template of the target; rendered values other than `http(s)` URLs are
    /// dropped.
    pub url: String,
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
    Repository,
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
    /// Repository id from the registry; may be a `{{ }}` template.
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
/// Exactly one of `repository` and `path` names the source.
#[derive(Debug, Clone)]
pub struct RepositorySchema {
    /// The key of the entry: the label of the repository and the name of its
    /// folder in the task directory.
    pub name: String,

    /// Marks the primary repository; exactly one repository is primary.
    pub primary: bool,

    /// Branch a new task starts from (`base_branch`).
    pub base_branch: Option<String>,

    /// Where the repository comes from.
    pub source: RepositorySourceSchema,

    /// Paths captured into a checkpoint even though the repository ignores
    /// them; the same caveats as [`GitWorkspaceSchema::preserve_paths`].
    pub preserve_paths: Vec<String>,
}

/// The source of a `globals.workspaces` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepositorySourceSchema {
    /// Repository id from the registry; may be a `{{ }}` template.
    Repository(String),
    /// Local folder, relative to the task directory.
    Path(String),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRepositorySchema {
    #[serde(default)]
    primary: bool,
    #[serde(default)]
    base_branch: Option<String>,
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
            _ => {
                return Err(format!(
                    "workspace '{name}' needs exactly one of 'repository' and 'path'"
                ))
            }
        };
        Ok(RepositorySchema {
            name,
            primary: raw.primary,
            base_branch: raw.base_branch,
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

/// A single state of the graph (§3.3).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateSchema {
    /// Maximum number of entries before operator intervention.
    #[serde(default)]
    pub max_entries: Option<u32>,
    #[serde(default)]
    pub description: Option<String>,

    /// Action executed on entry. Terminal states have none (§4).
    #[serde(default)]
    pub action: Option<ActionSchema>,

    /// Variables the step's action may set (agent and script steps only).
    #[serde(default)]
    pub variables: Vec<DefinitionSchema>,

    /// Artifacts the step's action may produce (agent and script steps
    /// only).
    #[serde(default)]
    pub artifacts: Vec<DefinitionSchema>,

    /// Possible choices, each optionally described — see [`ChoiceSchema`].
    #[serde(default)]
    pub choices: Vec<ChoiceSchema>,

    /// Mapping from output label to target state.
    #[serde(default)]
    pub transitions: HashMap<String, String>,

    /// Decision provider; defaults to `rules`.
    #[serde(default)]
    pub decision: Option<DecisionSchema>,

    /// Overrides `globals.timeout`.
    #[serde(default)]
    pub timeout: Option<DurationSecs>,

    /// Overrides `globals.retry_policy`.
    #[serde(default)]
    pub retry_policy: Option<RetryPolicySchema>,

    /// Marks the entry point explicitly, overriding file order (§5.1).
    #[serde(default)]
    pub initial: bool,

    /// Allows the operator to force an arbitrary transition out of this state.
    #[serde(default)]
    pub allow_manual_override: bool,

    /// Base image for this step only, overriding `globals.image` and the
    /// repository's (R15).
    #[serde(default)]
    pub image: Option<crate::domain::ImageSpec>,

    /// Repositories the step gets (`schema: v2` only). Without it the step
    /// gets all of them, the primary one writable.
    #[serde(default, rename = "workspaces")]
    pub workspace: Option<StepWorkspaceSchema>,
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

    /// Workspace permission, translated to native harness permissions at run
    /// time. Omission leaves the harness's own configuration in effect.
    #[serde(default)]
    pub workspace_access: Option<crate::domain::WorkspaceAccess>,

    pub prompt: String,

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
/// entry point when no state is flagged `initial: true`.
fn deserialize_ordered_states<'de, D>(
    deserializer: D,
) -> Result<Vec<(String, StateSchema)>, D::Error>
where
    D: Deserializer<'de>,
{
    struct StatesVisitor;

    impl<'de> Visitor<'de> for StatesVisitor {
        type Value = Vec<(String, StateSchema)>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a mapping of state id to state definition")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut states = Vec::with_capacity(map.size_hint().unwrap_or(0));
            while let Some((id, state)) = map.next_entry::<String, StateSchema>()? {
                states.push((id, state));
            }
            Ok(states)
        }
    }

    deserializer.deserialize_map(StatesVisitor)
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
