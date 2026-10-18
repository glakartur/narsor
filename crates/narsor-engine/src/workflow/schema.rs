//! YAML deserialization layer for workflow definitions.
//!
//! These structures mirror `WORKFLOW_SCHEMA.md` one to one: field names,
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

/// The whole workflow file (`WORKFLOW_SCHEMA.md` §2).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowFile {
    /// Unique workflow name; the loader also derives the definition id from it.
    pub name: String,

    /// Definition version.
    pub version: u32,

    /// Optional description of the workflow's purpose.
    #[serde(default)]
    pub description: Option<String>,

    /// Inputs required to start an execution.
    #[serde(default)]
    pub inputs: Vec<InputSchema>,

    /// Workflow-wide defaults.
    #[serde(default)]
    pub globals: Option<GlobalsSchema>,

    /// How a task introduces itself: title and links (§3.6).
    #[serde(default)]
    pub display: Option<DisplaySchema>,

    /// The state graph. Order is significant: without an explicit
    /// `initial: true` flag the first state is the entry point, so the mapping
    /// is kept as a vector of pairs instead of a `HashMap`.
    #[serde(deserialize_with = "deserialize_ordered_states")]
    pub states: Vec<(String, StateSchema)>,

    /// States whose entry ends the execution, with the status each one
    /// produces. Accepts both forms of §3.5 — see [`TerminalStatesSchema`].
    #[serde(default)]
    pub terminal_states: TerminalStatesSchema,
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

    /// Default workspace for the execution.
    #[serde(default)]
    pub workspace: Option<WorkspaceSchema>,

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

/// A single state of the graph (§3.3).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateSchema {
    #[serde(default)]
    pub description: Option<String>,

    /// Action executed on entry. Terminal states have none (§4).
    #[serde(default)]
    pub action: Option<ActionSchema>,

    /// Possible output labels, each optionally described — see
    /// [`OutputSchema`].
    #[serde(default)]
    pub outputs: Vec<OutputSchema>,

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
}

/// One entry of a state's `outputs` list (§3.3), in either of its two forms.
///
/// A bare label is the original contract; the object form adds a description
/// the engine shows wherever the label is offered — the agent's output
/// contract, the input form, the transition menu:
///
/// ```yaml
/// outputs:
///   - success                                   # bare label
///   - label: needs_human
///     description: A person has to decide first  # described label
/// ```
///
/// Both compile to the same [`crate::domain::OutputDefinition`]; a bare label
/// simply has no description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputSchema {
    pub label: String,
    pub description: Option<String>,
    pub requires_human_approval: bool,
    pub resume_comment_field: Option<String>,
}

/// The object form of an [`OutputSchema`]. `deny_unknown_fields` keeps a
/// misspelled `descripton:` from being dropped silently.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputObjectSchema {
    label: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    requires_human_approval: bool,
    #[serde(default)]
    resume_comment_field: Option<String>,
}

impl<'de> Deserialize<'de> for OutputSchema {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct OutputVisitor;

        impl<'de> Visitor<'de> for OutputVisitor {
            type Value = OutputSchema;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str(
                    "an output label, or a mapping with 'label' and an optional 'description'",
                )
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(OutputSchema {
                    label: value.to_string(),
                    description: None,
                    requires_human_approval: false,
                    resume_comment_field: None,
                })
            }

            fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let object =
                    OutputObjectSchema::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Ok(OutputSchema {
                    label: object.label,
                    description: object.description,
                    requires_human_approval: object.requires_human_approval,
                    resume_comment_field: object.resume_comment_field,
                })
            }
        }

        deserializer.deserialize_any(OutputVisitor)
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
