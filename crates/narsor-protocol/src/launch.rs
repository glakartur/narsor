//! Top-level shape of a task launch spec.
//!
//! The engine owns the typed launch spec; the controller must check the same
//! top-level fields without importing the engine's domain model. This module
//! is the one definition of that list, so a field cannot be added on one side
//! only. The nested parts stay opaque [`Value`]s here.

use serde::Deserialize;
use serde_json::Value;

/// Current schema version of a launch spec.
///
/// Version 2 replaced version 1's opaque `workspace` with a typed workspace,
/// added the pinned environment and the resource profile, and moved the
/// digest from FNV-1a to SHA-256.
/// Version 3 added the `definition` snapshot, so a runner never has to find
/// the workflow again in a catalogue that may have changed (or that it cannot
/// even see) since the task was created.
/// Version 4 replaced the single `workspace` source with a list of named
/// repositories, each with its branch, for workflows of several repositories.
/// Version 5 is a version 4 spec whose definition snapshot is in the unified
/// state shape (`schema: v2` without legacy markers). Only such a spec is
/// written as version 5, so a runner older than the shape refuses it by its
/// version instead of failing on the snapshot, and still runs the rest.
pub const TASK_LAUNCH_SPEC_VERSION: u32 = 5;

/// The version of a spec whose definition is in the legacy state shape, or
/// that has none. Still written, and read, beside [`TASK_LAUNCH_SPEC_VERSION`].
pub const LEGACY_SHAPE_LAUNCH_SPEC_VERSION: u32 = 4;

/// Why this build cannot read a spec of `version`, or `None` when it can.
pub fn unsupported_launch_spec_version(version: u64) -> Option<String> {
    let readable =
        u64::from(LEGACY_SHAPE_LAUNCH_SPEC_VERSION)..=u64::from(TASK_LAUNCH_SPEC_VERSION);
    if readable.contains(&version) {
        return None;
    }
    let reads = format!(
        "this build reads {LEGACY_SHAPE_LAUNCH_SPEC_VERSION} to {TASK_LAUNCH_SPEC_VERSION}"
    );
    Some(if version > u64::from(TASK_LAUNCH_SPEC_VERSION) {
        format!(
            "task launch spec version {version} was written by a newer Narsor build ({reads}); \
             update this runner and controller to the server's build"
        )
    } else {
        format!(
            "task launch spec version {version} is not supported ({reads}); an older spec \
             predates pinned environments, the workflow definition snapshot or the repository \
             list and cannot be upgraded faithfully — archive the task and create it again"
        )
    })
}

/// Every top-level field of a current-version launch spec.
///
/// Fields are public so that a reader can unpack the envelope with an
/// exhaustive pattern: adding a field here then stops that reader compiling.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchSpecEnvelope {
    /// Schema version of the record.
    pub version: u32,
    /// Workflow the task runs.
    pub workflow_id: String,
    /// Version of that workflow.
    pub workflow_version: u32,
    /// Workflow inputs.
    pub inputs: Value,
    /// What the task clones into its empty storage.
    pub workspace: Value,
    /// The pinned runner environment.
    pub environment: Value,
    /// CPU and memory of the task workload.
    pub resources: Value,
    /// Snapshot of the workflow definition; absent for bookkeeping-only specs.
    #[serde(default)]
    pub definition: Option<Value>,
    /// Ticket the operator typed; absent in specs sealed before the field.
    #[serde(default)]
    pub ticket: Option<String>,
    /// Title the operator typed; absent in specs sealed before the field.
    #[serde(default)]
    pub title: Option<String>,
    /// `sha256:` digest of every other field.
    pub digest: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base() -> Value {
        json!({
            "version": 4,
            "workflow_id": "wf",
            "workflow_version": 1,
            "inputs": {},
            "workspace": {},
            "environment": {},
            "resources": {},
            "digest": "sha256:abc",
        })
    }

    #[test]
    fn both_written_versions_are_readable() {
        assert_eq!(unsupported_launch_spec_version(4), None);
        assert_eq!(unsupported_launch_spec_version(5), None);
    }

    #[test]
    fn a_newer_version_asks_for_an_update() {
        let message = unsupported_launch_spec_version(6).expect("refused");
        assert!(message.contains("newer Narsor build"), "{message}");
        assert!(message.contains("update"), "{message}");
    }

    #[test]
    fn an_older_version_asks_to_create_the_task_again() {
        let message = unsupported_launch_spec_version(3).expect("refused");
        assert!(message.contains("create it again"), "{message}");
    }

    #[test]
    fn full_envelope_parses_with_ticket_and_title() {
        let mut value = base();
        value["definition"] = json!({"id": "wf"});
        value["ticket"] = json!("NAR-001");
        value["title"] = json!("Poprawa formularza");

        let envelope: LaunchSpecEnvelope = serde_json::from_value(value).unwrap();

        assert_eq!(envelope.version, LEGACY_SHAPE_LAUNCH_SPEC_VERSION);
        assert_eq!(envelope.ticket.as_deref(), Some("NAR-001"));
        assert_eq!(envelope.title.as_deref(), Some("Poprawa formularza"));
        assert_eq!(envelope.definition, Some(json!({"id": "wf"})));
    }

    #[test]
    fn envelope_without_optional_fields_parses() {
        let envelope: LaunchSpecEnvelope = serde_json::from_value(base()).unwrap();

        assert_eq!(envelope.ticket, None);
        assert_eq!(envelope.title, None);
        assert_eq!(envelope.definition, None);
    }

    #[test]
    fn unknown_top_level_field_is_rejected_by_name() {
        let mut value = base();
        value["surprise"] = json!(1);

        let error = serde_json::from_value::<LaunchSpecEnvelope>(value).unwrap_err();

        assert!(error.to_string().contains("surprise"), "{error}");
    }

    #[test]
    fn missing_digest_is_rejected() {
        let mut value = base();
        value.as_object_mut().unwrap().remove("digest");

        let error = serde_json::from_value::<LaunchSpecEnvelope>(value).unwrap_err();

        assert!(error.to_string().contains("digest"), "{error}");
    }
}
