//! The engine's launch spec must pass the controller's validation: both read
//! the same envelope from `narsor-protocol`.

use narsor_controller_runtime::backend::validate_launch_input;
use narsor_engine::domain::{ResolvedEnvironment, ResourceProfile, TaskLaunchSpec, TaskWorkspace};
use narsor_engine::workflow::load_from_str;
use narsor_protocol::controller::LaunchRequest;
use narsor_protocol::launch::{
    LaunchSpecEnvelope, LEGACY_SHAPE_LAUNCH_SPEC_VERSION, TASK_LAUNCH_SPEC_VERSION,
};

fn plain() -> TaskLaunchSpec {
    TaskLaunchSpec::new(
        "wf",
        1,
        serde_json::json!({}),
        TaskWorkspace::none(),
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

fn launch_request(workspaces: serde_json::Value) -> serde_json::Value {
    let mut value = serde_json::json!({
        "workflow_id": "wf",
        "workflow_version": 1,
        "inputs": {"code": "app"},
    });
    if !workspaces.is_null() {
        value["workspaces"] = workspaces;
    }
    value
}

#[test]
fn a_launch_request_without_workspaces_serializes_exactly_as_before() {
    let plain = launch_request(serde_json::Value::Null);
    let request: LaunchRequest = serde_json::from_value(plain.clone()).unwrap();
    assert!(request.workspaces.is_empty());
    assert_eq!(serde_json::to_value(&request).unwrap(), plain);
}

#[test]
fn a_launch_request_carries_a_choice_per_workspace() {
    let with =
        launch_request(serde_json::json!({"code": {"repository": "app", "branch": "develop"}}));
    let request: LaunchRequest = serde_json::from_value(with.clone()).unwrap();
    assert_eq!(
        request.workspaces["code"].branch.as_deref(),
        Some("develop")
    );
    assert_eq!(serde_json::to_value(&request).unwrap(), with);
}

#[test]
fn a_launch_request_with_the_former_branches_key_is_refused() {
    let former = launch_request(serde_json::Value::Null);
    let mut former = former;
    former["branches"] = serde_json::json!({"code": "develop"});
    assert!(serde_json::from_value::<LaunchRequest>(former).is_err());
}

fn sealed_with(definition_yaml: &str) -> TaskLaunchSpec {
    let definition = load_from_str(definition_yaml).expect("the definition loads");
    TaskLaunchSpec::new(
        definition.id.clone(),
        definition.version,
        serde_json::json!({}),
        TaskWorkspace::none(),
        ResolvedEnvironment::harness_only("claude-code"),
        ResourceProfile::new(1000, 2048),
    )
    .with_definition(&definition)
    .expect("the definition serializes")
}

/// A runner from before the unified shape reads only version 4, so it
/// refuses a unified snapshot by its version, and still runs a legacy one.
#[test]
fn only_a_unified_definition_raises_the_spec_version() {
    let unified = sealed_with(include_str!("../fixtures/v2/exit.yaml"));
    let legacy = sealed_with(include_str!("../fixtures/implement_feature.yaml"));

    assert_eq!(unified.version, TASK_LAUNCH_SPEC_VERSION);
    assert_eq!(legacy.version, LEGACY_SHAPE_LAUNCH_SPEC_VERSION);
    accepted(&unified);
    accepted(&legacy);
    for spec in [unified, legacy] {
        let value = serde_json::to_value(&spec).unwrap();
        let read: TaskLaunchSpec = serde_json::from_value(value).expect("this build reads it");
        assert_eq!(read, spec);
        assert!(
            read.digest_is_valid(),
            "the version is sealed in the digest"
        );
    }
}
