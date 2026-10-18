//! Registered execution backends (R3, R10, R19, R37; KTD6).
//!
//! A backend record is what the owner configures once per execution
//! environment: what kind of platform it is, what it can do, how much of it
//! one task and all tasks together may use, which credentials exist there and
//! how they reach a tool. It is stored as `{catalog}/backends/{id}.yaml`; the
//! file name is the [`BackendId`], exactly like a repository entry.
//!
//! ```yaml
//! kind: kubernetes
//! description: Team cluster, namespace narsor-tasks
//! trust_scope: dedicated
//! limits:
//!   max_concurrent_tasks: 3
//!   default_resources: { cpu_millicores: 2000, memory_mib: 4096 }
//!   max_resources: { cpu_millicores: 4000, memory_mib: 8192 }
//! harnesses: [claude-code, codex]
//! secret_delivery: platform
//! secrets:
//!   - name: github-token
//!     tool: gh
//!     key: narsor-github/token
//!     delivery: { env: GH_TOKEN }
//! ```
//!
//! Secrets are *references* only: `key` names a value the backend's own
//! store holds (for Kubernetes a `Secret` and key in the backend's trust
//! namespace). Nothing in this file is a credential, which is why the record
//! may be stored, listed and shown freely.

use std::collections::BTreeSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::domain::{BackendId, ResourceProfile, SecretDelivery, SecretReference};

/// Most concurrent tasks a backend record may declare. A sanity bound, not a
/// scheduling policy: Narsor targets a handful of parallel tasks (R6).
pub const MAX_CONCURRENT_TASKS_LIMIT: u32 = 1024;

/// Default wait before the control plane frees the slot of a task that waits
/// in a transient state (`slot_release_grace_secs`).
pub const DEFAULT_SLOT_RELEASE_GRACE_SECS: u64 = 300;
/// Default extension of that wait when the owner reads the task's details
/// (`slot_release_read_extension_secs`).
pub const DEFAULT_SLOT_RELEASE_READ_EXTENSION_SECS: u64 = 180;
/// Default idle time after which a reader no longer extends the wait
/// (`slot_reader_idle_secs`).
pub const DEFAULT_SLOT_READER_IDLE_SECS: u64 = 120;
/// Longest period a slot-release setting may have: one day.
pub const MAX_SLOT_PERIOD_SECS: u64 = 86_400;

pub use narsor_controller_runtime::backend::{BackendCapabilities, BackendKind};

/// How far the operator trusts the place a backend runs (R3, KTD6).
///
/// Recorded, not enforced here: U5's admission policy and U7's controller
/// identities decide what a scope permits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustScope {
    /// The owner's own machine.
    Workstation,
    /// Infrastructure reserved for this owner, e.g. a dedicated namespace.
    Dedicated,
    /// Infrastructure other workloads share.
    Shared,
}

/// How the controller materializes the credentials it is given references to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretDeliveryMode {
    /// The backend cannot deliver credentials; any secret reference is
    /// refused.
    #[default]
    None,
    /// The backend's native secret store (a Kubernetes `Secret` mounted or
    /// projected into the task Pod), read-only and outside task storage.
    Platform,
}

/// Per-backend limits (R3, R6; consumed by U10's scheduler).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendLimits {
    /// Tasks that may hold compute at once.
    pub max_concurrent_tasks: u32,
    /// What a task gets when it asks for nothing.
    pub default_resources: ResourceProfile,
    /// The most one task may ask for; `default_resources` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_resources: Option<ResourceProfile>,
    /// Seconds a task waits in a transient state before its slot is freed.
    #[serde(default = "default_slot_release_grace_secs")]
    pub slot_release_grace_secs: u64,
    /// Seconds a read of the task's details adds to that wait.
    #[serde(default = "default_slot_release_read_extension_secs")]
    pub slot_release_read_extension_secs: u64,
    /// Seconds without a read after which a reader stops extending the wait.
    #[serde(default = "default_slot_reader_idle_secs")]
    pub slot_reader_idle_secs: u64,
}

fn default_slot_release_grace_secs() -> u64 {
    DEFAULT_SLOT_RELEASE_GRACE_SECS
}
fn default_slot_release_read_extension_secs() -> u64 {
    DEFAULT_SLOT_RELEASE_READ_EXTENSION_SECS
}
fn default_slot_reader_idle_secs() -> u64 {
    DEFAULT_SLOT_READER_IDLE_SECS
}

/// The periods that decide when a slot is freed, as read from one backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotReleaseTimings {
    /// Wait before a task in a transient state frees its slot.
    pub grace: Duration,
    /// Time a read of the task's details adds to that wait.
    pub read_extension: Duration,
    /// Idle time after which a reader no longer extends the wait.
    pub reader_idle: Duration,
}

impl Default for SlotReleaseTimings {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(DEFAULT_SLOT_RELEASE_GRACE_SECS),
            read_extension: Duration::from_secs(DEFAULT_SLOT_RELEASE_READ_EXTENSION_SECS),
            reader_idle: Duration::from_secs(DEFAULT_SLOT_READER_IDLE_SECS),
        }
    }
}

impl BackendLimits {
    /// Limits with the given capacity and resources and default slot periods.
    pub fn new(
        max_concurrent_tasks: u32,
        default_resources: ResourceProfile,
        max_resources: Option<ResourceProfile>,
    ) -> Self {
        Self {
            max_concurrent_tasks,
            default_resources,
            max_resources,
            slot_release_grace_secs: DEFAULT_SLOT_RELEASE_GRACE_SECS,
            slot_release_read_extension_secs: DEFAULT_SLOT_RELEASE_READ_EXTENSION_SECS,
            slot_reader_idle_secs: DEFAULT_SLOT_READER_IDLE_SECS,
        }
    }

    /// The slot-release periods.
    pub fn slot_release_timings(&self) -> SlotReleaseTimings {
        SlotReleaseTimings {
            grace: Duration::from_secs(self.slot_release_grace_secs),
            read_extension: Duration::from_secs(self.slot_release_read_extension_secs),
            reader_idle: Duration::from_secs(self.slot_reader_idle_secs),
        }
    }

    /// The most one task may ask for.
    pub fn max_resources(&self) -> ResourceProfile {
        self.max_resources.unwrap_or(self.default_resources)
    }
}

/// One credential the backend can deliver, by reference (R19).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendSecret {
    /// Name profiles and repositories refer to it by.
    pub name: String,
    /// Tool it is for.
    pub tool: String,
    /// Backend-native locator of the value — never the value.
    pub key: String,
    /// How the controller hands it to the tool.
    pub delivery: SecretDelivery,
}

/// One registered execution backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendRecord {
    /// Identifier, carried by the file name rather than the file (see
    /// [`crate::registry::RepositoryEntry`] for why).
    #[serde(skip)]
    pub id: String,
    /// Platform.
    pub kind: BackendKind,
    /// Free-form note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Trust scope.
    pub trust_scope: TrustScope,
    /// Supported runner_state operations.
    #[serde(default)]
    pub capabilities: BackendCapabilities,
    /// Limits.
    pub limits: BackendLimits,
    /// Harnesses this backend's runtime can run; empty means every harness
    /// this build registers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub harnesses: Vec<String>,
    /// How credentials are delivered.
    #[serde(default)]
    pub secret_delivery: SecretDeliveryMode,
    /// The credentials that may be referenced on this backend — the allow
    /// list every secret reference is validated against.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<BackendSecret>,
}

impl BackendRecord {
    /// The id of the backend the local daemon owns.
    pub const LOCAL_ID: &'static str = "local";

    /// The local backend as the daemon provides it when the catalogue does
    /// not declare one: one task at a time on this workstation.
    pub fn builtin_local() -> Self {
        Self::new(
            Self::LOCAL_ID,
            BackendKind::Local,
            TrustScope::Workstation,
            BackendLimits::new(1, ResourceProfile::new(1000, 2048), None),
        )
    }

    /// A record with the given limits and nothing else configured.
    pub fn new(
        id: impl Into<String>,
        kind: BackendKind,
        trust_scope: TrustScope,
        limits: BackendLimits,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            description: None,
            trust_scope,
            capabilities: BackendCapabilities::default(),
            limits,
            harnesses: Vec::new(),
            secret_delivery: SecretDeliveryMode::None,
            secrets: Vec::new(),
        }
    }

    /// Every problem with the record, empty when it is usable.
    ///
    /// All of them at once, like the workflow validator: an operator fixing
    /// a backend file should not have to do it one error per attempt.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if let Err(error) = BackendId::parse(self.id.clone()) {
            problems.push(error.to_string());
        }

        let limits = &self.limits;
        if limits.max_concurrent_tasks == 0
            || limits.max_concurrent_tasks > MAX_CONCURRENT_TASKS_LIMIT
        {
            problems.push(format!(
                "limits.max_concurrent_tasks must be between 1 and {MAX_CONCURRENT_TASKS_LIMIT}, \
                 not {}",
                limits.max_concurrent_tasks
            ));
        }
        for (name, secs) in [
            ("slot_release_grace_secs", limits.slot_release_grace_secs),
            (
                "slot_release_read_extension_secs",
                limits.slot_release_read_extension_secs,
            ),
            ("slot_reader_idle_secs", limits.slot_reader_idle_secs),
        ] {
            if secs == 0 || secs > MAX_SLOT_PERIOD_SECS {
                problems.push(format!(
                    "limits.{name} must be between 1 and {MAX_SLOT_PERIOD_SECS} seconds, \
                     not {secs}"
                ));
            }
        }
        for problem in limits.default_resources.problems() {
            problems.push(format!("limits.default_resources: {problem}"));
        }
        if let Some(max) = &limits.max_resources {
            for problem in max.problems() {
                problems.push(format!("limits.max_resources: {problem}"));
            }
            if !limits.default_resources.fits_within(max) {
                problems.push(format!(
                    "limits.default_resources ({}) exceed limits.max_resources ({max})",
                    limits.default_resources
                ));
            }
        }

        let mut harnesses = BTreeSet::new();
        for harness in &self.harnesses {
            if harness.trim().is_empty() {
                problems.push("harnesses: a harness name is blank".to_string());
            } else if !harnesses.insert(harness) {
                problems.push(format!("harnesses: '{harness}' is listed twice"));
            }
        }

        if self.secret_delivery == SecretDeliveryMode::None && !self.secrets.is_empty() {
            problems.push(
                "secrets are listed but secret_delivery is 'none'; set it to 'platform' or \
                 remove them"
                    .to_string(),
            );
        }
        let mut names = BTreeSet::new();
        let mut targets = BTreeSet::new();
        for secret in &self.secrets {
            if !is_reference_name(&secret.name) {
                problems.push(format!(
                    "secrets: name '{}' must be [A-Za-z0-9._-], not starting with '.'",
                    secret.name
                ));
            } else if !names.insert(&secret.name) {
                problems.push(format!("secrets: '{}' is defined twice", secret.name));
            }
            if secret.tool.trim().is_empty() {
                problems.push(format!("secrets: '{}' names no tool", secret.name));
            }
            if secret.key.trim().is_empty() || secret.key.chars().any(char::is_whitespace) {
                problems.push(format!(
                    "secrets: '{}' needs a backend-native key without whitespace",
                    secret.name
                ));
            }
            if let Some(problem) = secret.delivery.problem() {
                problems.push(format!("secrets: '{}': {problem}", secret.name));
            }
            if !targets.insert(&secret.delivery) {
                problems.push(format!(
                    "secrets: '{}' is delivered to {} which another secret already uses",
                    secret.name, secret.delivery
                ));
            }
        }
        problems
    }

    /// The allow-listed credential `name` as a reference, if it is listed.
    pub fn secret_reference(&self, backend: &BackendId, name: &str) -> Option<SecretReference> {
        self.secrets
            .iter()
            .find(|secret| secret.name == name)
            .map(|secret| SecretReference {
                backend: backend.clone(),
                name: secret.name.clone(),
                tool: secret.tool.clone(),
                key: secret.key.clone(),
                delivery: secret.delivery.clone(),
            })
    }

    /// Whether this backend's runtime can run `harness`.
    pub fn allows_harness(&self, harness: &str) -> bool {
        self.harnesses.is_empty() || self.harnesses.iter().any(|known| known == harness)
    }

    /// Why this backend (`backend`) cannot run `harness`, or `None` when it
    /// can.
    pub fn harness_refusal(&self, backend: &BackendId, harness: &str) -> Option<String> {
        (!self.allows_harness(harness)).then(|| {
            format!(
                "backend '{backend}' runs only {}",
                self.harnesses.join(", ")
            )
        })
    }
}

/// Whether `name` is usable as a secret, tool or skill reference: the same
/// conservative character set identifiers use.
pub(crate) fn is_reference_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= crate::domain::MAX_IDENTIFIER_LEN
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "kind: kubernetes\ntrust_scope: dedicated\nlimits:\n  max_concurrent_tasks: 2\n  default_resources: { cpu_millicores: 1000, memory_mib: 2048 }\n";

    fn parse(extra: &str) -> Result<BackendRecord, serde_yaml::Error> {
        let mut record: BackendRecord = serde_yaml::from_str(&format!("{BASE}{extra}"))?;
        record.id = "k8s".to_string();
        Ok(record)
    }

    #[test]
    fn a_config_without_the_new_fields_gets_the_defaults() {
        let record = parse("").unwrap();
        assert!(record.problems().is_empty());
        let timings = record.limits.slot_release_timings();
        assert_eq!(timings.grace, Duration::from_secs(300));
        assert_eq!(timings.read_extension, Duration::from_secs(180));
        assert_eq!(timings.reader_idle, Duration::from_secs(120));
    }

    #[test]
    fn a_config_with_the_fields_uses_them() {
        let record = parse(
            "  slot_release_grace_secs: 60\n  slot_release_read_extension_secs: 30\n  slot_reader_idle_secs: 10\n",
        )
        .unwrap();
        assert!(record.problems().is_empty());
        let timings = record.limits.slot_release_timings();
        assert_eq!(timings.grace, Duration::from_secs(60));
        assert_eq!(timings.read_extension, Duration::from_secs(30));
        assert_eq!(timings.reader_idle, Duration::from_secs(10));
    }

    #[test]
    fn a_negative_or_non_numeric_period_is_a_config_error() {
        for bad in ["-5", "soon", "1.5"] {
            let error = parse(&format!("  slot_release_grace_secs: {bad}\n")).unwrap_err();
            assert!(
                error.to_string().contains("slot_release_grace_secs")
                    || error.to_string().contains("invalid"),
                "{bad}: {error}"
            );
        }
    }

    #[test]
    fn a_zero_or_huge_period_is_a_problem() {
        for bad in [0, MAX_SLOT_PERIOD_SECS + 1] {
            let record = parse(&format!("  slot_reader_idle_secs: {bad}\n")).unwrap();
            let problems = record.problems();
            assert_eq!(problems.len(), 1, "{problems:?}");
            assert!(problems[0].contains("limits.slot_reader_idle_secs"));
        }
    }
}
