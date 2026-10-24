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
pub const TASK_LAUNCH_SPEC_VERSION: u32 = 3;

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
            "version": 3,
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
    fn full_envelope_parses_with_ticket_and_title() {
        let mut value = base();
        value["definition"] = json!({"id": "wf"});
        value["ticket"] = json!("NAR-001");
        value["title"] = json!("Poprawa formularza");

        let envelope: LaunchSpecEnvelope = serde_json::from_value(value).unwrap();

        assert_eq!(envelope.version, TASK_LAUNCH_SPEC_VERSION);
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
