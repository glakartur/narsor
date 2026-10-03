//! Identity of a remotely orchestrated task and its immutable launch input.
//!
//! These types are shared by both sides of KTD1's boundary: the control plane
//! stores them in its orchestration records, and the runner finds the same
//! [`TaskLaunchSpec`] on its task storage. They therefore live in the domain
//! rather than in [`crate::orchestration`], which only the control plane uses.

use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

use super::environment::{canonical_sha256, ResolvedEnvironment, ResourceProfile, TaskWorkspace};
use super::WorkflowDefinition;

/// Longest identifier accepted for a task or a backend.
///
/// Both end up as a single path component under the state root and, later,
/// inside backend resource names, so an unbounded value is never wanted.
pub const MAX_IDENTIFIER_LEN: usize = 128;

/// A task or backend identifier that is not safe to use.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid {kind} identifier '{value}': use 1-{MAX_IDENTIFIER_LEN} characters of [A-Za-z0-9._-], not starting with '.'")]
pub struct InvalidIdentifier {
    /// Which kind of identifier was rejected (`task` or `backend`).
    pub kind: &'static str,
    /// The rejected value.
    pub value: String,
}

/// Checks that `value` can be used as one path component.
///
/// A leading `.` is refused so that neither `..` nor a hidden file can ever be
/// produced; everything else outside the conservative character set is
/// refused rather than escaped, so the identifier is the path component.
fn validate_identifier(kind: &'static str, value: &str) -> Result<(), InvalidIdentifier> {
    let valid = !value.is_empty()
        && value.len() <= MAX_IDENTIFIER_LEN
        && !value.starts_with('.')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(InvalidIdentifier {
            kind,
            value: value.to_string(),
        })
    }
}

macro_rules! identifier {
    ($(#[$meta:meta])* $name:ident, $kind:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Validates and wraps `value`.
            pub fn parse(value: impl Into<String>) -> Result<Self, InvalidIdentifier> {
                let value = value.into();
                validate_identifier($kind, &value)?;
                Ok(Self(value))
            }

            /// The identifier as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidIdentifier;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::parse(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

identifier!(
    /// Stable identifier the control plane gives one logical task.
    ///
    /// Validated on construction *and* on deserialization, so a record read
    /// back from disk or received over the API can never smuggle a path
    /// separator into a later path join.
    TaskId,
    "task"
);

identifier!(
    /// Stable identifier of a registered execution backend (R3).
    BackendId,
    "backend"
);

/// Current schema version of [`TaskLaunchSpec`].
///
/// Version 2 replaced version 1's opaque `workspace` with a typed
/// [`TaskWorkspace`], added the pinned [`ResolvedEnvironment`] and the
/// [`ResourceProfile`], and moved the digest from FNV-1a to SHA-256.
/// Version 3 added the [`TaskLaunchSpec::definition`] snapshot, so a runner
/// never has to find the workflow again in a catalogue that may have changed
/// (or that it cannot even see) since the task was created.
pub const TASK_LAUNCH_SPEC_VERSION: u32 = 3;

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
}

/// The fields of a current-version spec, for the version-checked read.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskLaunchSpecFields {
    version: u32,
    workflow_id: String,
    workflow_version: u32,
    inputs: serde_json::Value,
    workspace: TaskWorkspace,
    environment: ResolvedEnvironment,
    resources: ResourceProfile,
    definition: Option<serde_json::Value>,
    digest: String,
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
        let fields: TaskLaunchSpecFields =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            version: fields.version,
            workflow_id: fields.workflow_id,
            workflow_version: fields.workflow_version,
            inputs: fields.inputs,
            workspace: fields.workspace,
            environment: fields.environment,
            resources: fields.resources,
            definition: fields.definition,
            digest: fields.digest,
        })
    }
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

    /// The workflow definition this task runs, from its snapshot.
    ///
    /// Refuses a spec without one and a snapshot of another workflow than
    /// the spec names: a runner must never fall back to looking the
    /// workflow up by name.
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
        Ok(definition)
    }

    /// Recomputes the digest from the content fields.
    ///
    /// SHA-256 over the canonical JSON rendering (see
    /// [`canonical_sha256`]): stable across processes, Rust releases and
    /// key insertion order, and — unlike the FNV-1a of version 1 — not
    /// forgeable by a party that can write the record but wants it to keep
    /// matching the digest a controller already accepted.
    pub fn compute_digest(&self) -> String {
        let content = serde_json::json!({
            "version": self.version,
            "workflow_id": self.workflow_id,
            "workflow_version": self.workflow_version,
            "inputs": self.inputs,
            "workspace": self.workspace,
            "environment": self.environment,
            "resources": self.resources,
            "definition": self.definition,
        });
        canonical_sha256(&content)
    }

    /// True when the stored digest matches the content and the environment
    /// identity matches the environment.
    pub fn digest_is_valid(&self) -> bool {
        self.digest == self.compute_digest() && self.environment.identity_is_valid()
    }
}
