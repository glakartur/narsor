//! Identity of a remotely orchestrated task and its immutable launch input.
//!
//! These types are shared by both sides of KTD1's boundary: the control plane
//! stores them in its orchestration records, and the runner finds the same
//! [`TaskLaunchSpec`] on its task storage. They therefore live in the domain
//! rather than in [`crate::orchestration`], which only the control plane uses.

use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

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
pub const TASK_LAUNCH_SPEC_VERSION: u32 = 1;

/// Immutable, versioned input a task is launched from.
///
/// Built once when the task is created and copied to task storage once
/// (R18, KTD5): a resumed or reconnected runner reads this record, never the
/// mutable defaults it was originally resolved from. `workspace` is kept
/// opaque here on purpose — U3/U6 give the Git identity and the resolved
/// environment their own types and bump [`TASK_LAUNCH_SPEC_VERSION`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskLaunchSpec {
    /// Schema version of this record.
    pub version: u32,
    /// Workflow the task runs.
    pub workflow_id: String,
    /// Version of that workflow.
    pub workflow_version: u32,
    /// Workflow inputs, exactly as validated at creation.
    pub inputs: serde_json::Value,
    /// Workspace source (repository, pinned commit…), opaque until U3.
    pub workspace: serde_json::Value,
    /// Digest of every other field, see [`TaskLaunchSpec::compute_digest`].
    pub digest: String,
}

impl TaskLaunchSpec {
    /// Builds a spec at the current schema version and seals it with its
    /// digest.
    pub fn new(
        workflow_id: impl Into<String>,
        workflow_version: u32,
        inputs: serde_json::Value,
        workspace: serde_json::Value,
    ) -> Self {
        let mut spec = Self {
            version: TASK_LAUNCH_SPEC_VERSION,
            workflow_id: workflow_id.into(),
            workflow_version,
            inputs,
            workspace,
            digest: String::new(),
        };
        spec.digest = spec.compute_digest();
        spec
    }

    /// Recomputes the digest from the content fields.
    ///
    /// The content is serialized through `serde_json::Value`, whose objects
    /// are key-sorted maps in this build, so the bytes — and the digest — do
    /// not depend on the order the caller inserted keys in. FNV-1a is used
    /// rather than `DefaultHasher` because the value is persisted and must be
    /// identical across processes and Rust releases. It is an integrity and
    /// identity check against accidents, not a cryptographic commitment;
    /// equality checks below compare full content as well.
    pub fn compute_digest(&self) -> String {
        let content = serde_json::json!({
            "version": self.version,
            "workflow_id": self.workflow_id,
            "workflow_version": self.workflow_version,
            "inputs": self.inputs,
            "workspace": self.workspace,
        });
        let bytes = serde_json::to_vec(&content).expect("a JSON value always serializes");
        fnv1a64(&bytes)
    }

    /// True when the stored digest matches the content.
    pub fn digest_is_valid(&self) -> bool {
        self.digest == self.compute_digest()
    }
}

fn fnv1a64(bytes: &[u8]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
    }
    format!("fnv1a64-{hash:016x}")
}
