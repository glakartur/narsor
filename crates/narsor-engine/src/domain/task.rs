//! Identity of a remotely orchestrated task and its immutable launch input.
//!
//! These types are shared by both sides of KTD1's boundary: the control plane
//! stores them in its orchestration records, and the runner finds the same
//! [`TaskLaunchSpec`] on its task storage. They therefore live in the domain
//! rather than in [`crate::orchestration`], which only the control plane uses.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use narsor_protocol::display::GENERATED_TITLE_MAX_CHARS;
pub use narsor_protocol::identifier::{BackendId, InvalidIdentifier, TaskId, MAX_IDENTIFIER_LEN};
pub use narsor_protocol::launch::TASK_LAUNCH_SPEC_VERSION;

use narsor_protocol::launch::LaunchSpecEnvelope;

use super::environment::{canonical_sha256, ResolvedEnvironment, ResourceProfile, TaskWorkspace};
use super::{DisplaySpec, WorkflowDefinition};

/// Longest ticket, in characters.
pub const TICKET_MAX_CHARS: usize = 64;
/// Longest title the operator can type, in characters.
pub const TITLE_MAX_CHARS: usize = 200;

/// Why a ticket or title is refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LabelError {
    /// The text has a line break or another control character.
    #[error("{field} must be one line without control characters")]
    NotOneLine {
        /// Name of the field.
        field: &'static str,
    },
    /// The text is longer than the limit.
    #[error("{field} has more than {max} characters")]
    TooLong {
        /// Name of the field.
        field: &'static str,
        /// The limit.
        max: usize,
    },
}

/// Trims a ticket or title the operator typed.
///
/// Empty text means "none". Text with a control character, a line break
/// included, or over `max` characters is refused, never cut: the operator
/// sees what was wrong.
pub fn normalize_label(
    field: &'static str,
    raw: Option<&str>,
    max: usize,
) -> Result<Option<String>, LabelError> {
    let Some(raw) = raw else { return Ok(None) };
    let text = raw.trim();
    if text.is_empty() {
        return Ok(None);
    }
    if text.chars().any(char::is_control) {
        return Err(LabelError::NotOneLine { field });
    }
    if text.chars().count() > max {
        return Err(LabelError::TooLong { field, max });
    }
    Ok(Some(text.to_owned()))
}

/// Reduces text that came from a conductor or a remote runner to one line of
/// at most `max` characters; `None` when nothing usable is left.
pub fn one_line_label(raw: &str, max: usize) -> Option<String> {
    let line = raw
        .split_whitespace()
        .map(|word| word.chars().filter(|c| !c.is_control()).collect::<String>())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let cut: String = line.chars().take(max).collect();
    let cut = cut.trim_end().to_owned();
    (!cut.is_empty()).then_some(cut)
}

/// A title a model made, safe to render into any template — a script's
/// command line included: letters, digits, spaces and `- _ . , : ( )` stay,
/// everything else (quotes, `$`, backticks, `;`, `|`, `/`, `\`, control
/// characters) is dropped. One line of at most `max` characters; `None` when
/// nothing is left.
pub fn plain_generated_title(raw: &str, max: usize) -> Option<String> {
    let kept: String = raw
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace() || "-_.,:()".contains(*c))
        .collect();
    one_line_label(&kept, max)
}

/// The title a task shows: the operator's, else the one the conductor made.
pub fn effective_title(operator: Option<&str>, generated: Option<&str>) -> Option<String> {
    operator
        .filter(|title| !title.is_empty())
        .or(generated.filter(|title| !title.is_empty()))
        .map(str::to_owned)
}

/// Immutable, versioned input a task is launched from.
///
/// Built once when the task is created and copied to task storage once
/// (R18, KTD5): a resumed or reconnected runner reads this record, never the
/// mutable defaults it was originally resolved from.
///
/// # Older versions fail clearly
///
/// Deserialization reads `version` first and refuses anything but
/// [`TASK_LAUNCH_SPEC_VERSION`] with a message naming both versions. A
/// version-1 record has no environment to run in, so there is nothing
/// faithful to upgrade it to: guessing one from today's configuration would
/// be exactly the silent re-resolution R18 forbids.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TaskLaunchSpec {
    /// Schema version of this record.
    pub version: u32,
    /// Workflow the task runs.
    pub workflow_id: String,
    /// Version of that workflow.
    pub workflow_version: u32,
    /// Workflow inputs, exactly as validated at creation.
    pub inputs: serde_json::Value,
    /// What the task clones into its empty storage (R39).
    pub workspace: TaskWorkspace,
    /// The pinned runner environment (KTD5).
    pub environment: ResolvedEnvironment,
    /// CPU and memory of the task workload, within the backend's limits.
    pub resources: ResourceProfile,
    /// The loaded and validated [`WorkflowDefinition`] the task runs, as
    /// JSON (see [`TaskLaunchSpec::with_definition`]).
    ///
    /// Snapshotted at creation like everything else here (R18): a runner
    /// resumed months later runs exactly the graph its task started with,
    /// never whatever the catalogue now holds under the same name. `None`
    /// only for specs built for orchestration bookkeeping alone; a runner
    /// refuses one ([`TaskLaunchSpec::definition`]).
    pub definition: Option<serde_json::Value>,
    /// Ticket the operator typed, already normalized. Absent in specs sealed
    /// before the field existed, which therefore keep their digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
    /// Title the operator typed, already normalized; see `ticket`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// `sha256:` digest of every other field, see
    /// [`TaskLaunchSpec::compute_digest`].
    pub digest: String,
}

/// Why a launch spec's workflow definition snapshot cannot be used.
#[derive(Debug, Error)]
pub enum LaunchDefinitionError {
    /// The spec was built without a definition snapshot.
    #[error("the launch spec of workflow '{0}' carries no workflow definition snapshot")]
    Missing(String),
    /// The snapshot does not deserialize into a definition.
    #[error("the workflow definition snapshot of '{workflow}' is unreadable: {source}")]
    Unreadable {
        /// Workflow id of the spec.
        workflow: String,
        /// What serde said.
        #[source]
        source: serde_json::Error,
    },
    /// The snapshot is of another workflow than the spec names.
    #[error(
        "the workflow definition snapshot is '{found}' v{found_version}, \
         but the launch spec names '{expected}' v{expected_version}"
    )]
    Mismatch {
        /// Workflow the spec names.
        expected: String,
        /// Its version.
        expected_version: u32,
        /// Workflow the snapshot holds.
        found: String,
        /// Its version.
        found_version: u32,
    },
    /// The snapshot reads, but this build's validation rejects it: it was
    /// sealed by a Narsor build with another workflow schema.
    #[error(
        "the workflow definition snapshot of '{workflow}' is not valid for this build — it was \
         sealed by a Narsor build with another workflow schema; restart the server on this \
         build, archive the task and create it again:\n  {}",
        violations.join("\n  ")
    )]
    Invalid {
        /// Workflow id of the spec.
        workflow: String,
        /// Every violation, as validation reports them.
        violations: Vec<String>,
    },
}

impl<'de> Deserialize<'de> for TaskLaunchSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        let version = value
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| serde::de::Error::custom("task launch spec has no version"))?;
        if version != u64::from(TASK_LAUNCH_SPEC_VERSION) {
            return Err(serde::de::Error::custom(format!(
                "task launch spec version {version} is not supported by this build (expected \
                 {TASK_LAUNCH_SPEC_VERSION}); an older spec predates pinned environments or \
                 the workflow definition snapshot and cannot be upgraded faithfully — archive \
                 the task and create it again"
            )));
        }
        let envelope: LaunchSpecEnvelope =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        // Exhaustive on purpose (no `..`): a field added to the shared
        // envelope stops this reader compiling until it is handled.
        let LaunchSpecEnvelope {
            version,
            workflow_id,
            workflow_version,
            inputs,
            workspace,
            environment,
            resources,
            definition,
            ticket,
            title,
            digest,
        } = envelope;
        Ok(Self {
            version,
            workflow_id,
            workflow_version,
            inputs,
            workspace: typed_part("workspace", workspace)?,
            environment: typed_part("environment", environment)?,
            resources: typed_part("resources", resources)?,
            definition,
            ticket,
            title,
            digest,
        })
    }
}

/// Gives a nested part of the envelope its domain type; the error names it.
fn typed_part<T, E>(field: &'static str, value: serde_json::Value) -> Result<T, E>
where
    T: serde::de::DeserializeOwned,
    E: serde::de::Error,
{
    serde_json::from_value(value).map_err(|error| E::custom(format!("{field}: {error}")))
}

impl TaskLaunchSpec {
    /// Builds a spec at the current schema version and seals it with its
    /// digest. `environment` is sealed too, so its identity always matches.
    pub fn new(
        workflow_id: impl Into<String>,
        workflow_version: u32,
        inputs: serde_json::Value,
        workspace: TaskWorkspace,
        environment: ResolvedEnvironment,
        resources: ResourceProfile,
    ) -> Self {
        let mut spec = Self {
            version: TASK_LAUNCH_SPEC_VERSION,
            workflow_id: workflow_id.into(),
            workflow_version,
            inputs,
            workspace,
            environment: environment.sealed(),
            resources,
            definition: None,
            ticket: None,
            title: None,
            digest: String::new(),
        };
        spec.digest = spec.compute_digest();
        spec
    }

    /// Returns the spec with `definition` snapshotted into it, resealed.
    ///
    /// Stored as JSON rather than as the typed definition so the record stays
    /// a plain, comparable value; [`Self::definition`] turns it back.
    pub fn with_definition(
        mut self,
        definition: &WorkflowDefinition,
    ) -> Result<Self, serde_json::Error> {
        self.definition = Some(serde_json::to_value(definition)?);
        self.digest = self.compute_digest();
        Ok(self)
    }

    /// Returns the spec with the operator's ticket and title set, resealed.
    pub fn with_labels(mut self, ticket: Option<String>, title: Option<String>) -> Self {
        self.ticket = ticket;
        self.title = title;
        self.digest = self.compute_digest();
        self
    }

    /// The workflow definition this task runs, from its snapshot.
    ///
    /// Refuses a spec without one and a snapshot of another workflow than
    /// the spec names: a runner must never fall back to looking the
    /// workflow up by name. Refuses a snapshot this build does not validate
    /// too, so a server and a runner of different builds meet as a reported
    /// problem rather than as a runner that dies before it is ready.
    pub fn definition(&self) -> Result<WorkflowDefinition, LaunchDefinitionError> {
        let value = self
            .definition
            .clone()
            .ok_or_else(|| LaunchDefinitionError::Missing(self.workflow_id.clone()))?;
        let definition: WorkflowDefinition =
            serde_json::from_value(value).map_err(|source| LaunchDefinitionError::Unreadable {
                workflow: self.workflow_id.clone(),
                source,
            })?;
        if definition.id != self.workflow_id || definition.version != self.workflow_version {
            return Err(LaunchDefinitionError::Mismatch {
                expected: self.workflow_id.clone(),
                expected_version: self.workflow_version,
                found: definition.id,
                found_version: definition.version,
            });
        }
        definition
            .validate()
            .map_err(|violations| LaunchDefinitionError::Invalid {
                workflow: self.workflow_id.clone(),
                violations: violations.iter().map(ToString::to_string).collect(),
            })?;
        Ok(definition)
    }

    /// The `display` section of the sealed definition, without reading the
    /// rest of it: every task summary asks, and the display is cosmetic, so
    /// a snapshot that has none or cannot be read simply shows nothing.
    pub fn display_spec(&self) -> Option<DisplaySpec> {
        let display = self.definition.as_ref()?.get("display")?;
        serde_json::from_value(display.clone()).ok()
    }

    /// Recomputes the digest from the content fields.
    ///
    /// SHA-256 over the canonical JSON rendering (see
    /// [`canonical_sha256`]): stable across processes, Rust releases and
    /// key insertion order, and — unlike the FNV-1a of version 1 — not
    /// forgeable by a party that can write the record but wants it to keep
    /// matching the digest a controller already accepted.
    pub fn compute_digest(&self) -> String {
        let mut content = serde_json::json!({
            "version": self.version,
            "workflow_id": self.workflow_id,
            "workflow_version": self.workflow_version,
            "inputs": self.inputs,
            "workspace": self.workspace,
            "environment": self.environment,
            "resources": self.resources,
            "definition": self.definition,
        });
        // Only set labels enter the digest, so a spec sealed without them keeps
        // the digest it already has.
        if let Some(object) = content.as_object_mut() {
            if let Some(ticket) = &self.ticket {
                object.insert("ticket".into(), ticket.clone().into());
            }
            if let Some(title) = &self.title {
                object.insert("title".into(), title.clone().into());
            }
        }
        canonical_sha256(&content)
    }

    /// True when the stored digest matches the content and the environment
    /// identity matches the environment.
    pub fn digest_is_valid(&self) -> bool {
        self.digest == self.compute_digest() && self.environment.identity_is_valid()
    }
}
