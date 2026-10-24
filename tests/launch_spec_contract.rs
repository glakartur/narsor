//! The engine's launch spec must pass the controller's validation: both read
//! the same envelope from `narsor-protocol`.

use narsor_controller_runtime::backend::validate_launch_input;
use narsor_engine::domain::{ResolvedEnvironment, ResourceProfile, TaskLaunchSpec, TaskWorkspace};
use narsor_protocol::launch::LaunchSpecEnvelope;

fn plain() -> TaskLaunchSpec {
    TaskLaunchSpec::new(
        "wf",
        1,
        serde_json::json!({}),
        TaskWorkspace::None,
        ResolvedEnvironment::harness_only("claude-code"),
        ResourceProfile::new(1000, 2048),
    )
}

fn labelled() -> TaskLaunchSpec {
    plain().with_labels(Some("NAR-12".into()), Some("Zmiana API".into()))
}

fn with_definition() -> TaskLaunchSpec {
    let mut spec = labelled();
    spec.definition = Some(serde_json::json!({"id": "wf"}));
    spec.digest = spec.compute_digest();
    spec
}

fn accepted(spec: &TaskLaunchSpec) {
    let value = serde_json::to_value(spec).unwrap();
    assert_eq!(validate_launch_input(&value), Ok(()));
}

#[test]
fn a_spec_without_labels_passes_controller_validation() {
    accepted(&plain());
}

#[test]
fn a_spec_with_ticket_and_title_passes_controller_validation() {
    accepted(&labelled());
}

#[test]
fn a_spec_with_a_definition_snapshot_passes_controller_validation() {
    accepted(&with_definition());
}

/// Fails when a field is added to `TaskLaunchSpec` only. The new field must
/// be set to a non-default value in `with_definition` above, or `Serialize`
/// skips it and this test cannot see it.
#[test]
fn every_serialized_key_exists_in_the_envelope() {
    let value = serde_json::to_value(with_definition()).unwrap();
    // `deny_unknown_fields` makes this parse fail on any key the envelope
    // does not know.
    let envelope: LaunchSpecEnvelope = serde_json::from_value(value).unwrap();
    assert!(envelope.ticket.is_some() && envelope.title.is_some() && envelope.definition.is_some());
}
