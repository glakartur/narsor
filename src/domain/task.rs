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
pub const TASK_LAUNCH_SPEC_VERSION: u32 = 2;

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
    /// `sha256:` digest of every other field, see
    /// [`TaskLaunchSpec::compute_digest`].
    pub digest: String,
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
                 {TASK_LAUNCH_SPEC_VERSION}); a version-1 spec predates pinned environments \
                 and cannot be upgraded — archive the task and create it again"
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
            digest: String::new(),
        };
        spec.digest = spec.compute_digest();
        spec
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
        });
        canonical_sha256(&content)
    }

    /// True when the stored digest matches the content and the environment
    /// identity matches the environment.
    pub fn digest_is_valid(&self) -> bool {
        self.digest == self.compute_digest() && self.environment.identity_is_valid()
    }
}
