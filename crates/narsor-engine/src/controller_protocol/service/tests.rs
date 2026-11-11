use super::operator::check_step_exists;
use super::tasks::{transition_offer, workflow_steps};
use super::*;
use crate::domain::WorkflowDefinition;
use crate::engine::{manual_override_allowed, plan_operator_transition};
use crate::orchestration::{RunnerState, TaskRecord};
use crate::workflow::load_from_str;

const UNIFIED: &str = r#"
schema: v2
name: unified
version: 1
states:
  work:
    action:
      type: script
      command: "true"
    exit: { next: completed }
  completed: {}
terminal_states: { completed: completed }
"#;

const LEGACY_WITH_FIELD: &str = r#"
name: legacy
version: 1
states:
  work:
    allow_manual_override: true
    action:
      type: script
      command: "true"
    choices: [done]
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states: [completed]
"#;

const LEGACY_WITHOUT_FIELD: &str = r#"
name: legacy
version: 1
states:
  work:
    action:
      type: script
      command: "true"
    choices: [done]
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states: [completed]
"#;

const LEGACY_FIELD_OFF: &str = r#"
name: legacy
version: 1
states:
  work:
    allow_manual_override: false
    action:
      type: script
      command: "true"
    choices: [done]
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states: [completed]
"#;

/// Every shape the consent can depend on, with what it must be.
fn cases() -> [(&'static str, &'static str, bool); 4] {
    [
        ("unified", UNIFIED, true),
        ("legacy, field on", LEGACY_WITH_FIELD, true),
        ("legacy, field off", LEGACY_FIELD_OFF, false),
        ("legacy, no field", LEGACY_WITHOUT_FIELD, false),
    ]
}

#[test]
fn the_offer_and_the_runners_verdict_come_from_the_same_consent() {
    for (name, yaml, allowed) in cases() {
        let definition = load_from_str(yaml).expect(name);
        let state = &definition.states["work"];

        let offer = transition_offer(&definition, "work").expect(name);
        let verdict = plan_operator_transition(&definition, state, "completed", true);

        assert_eq!(offer.override_allowed, allowed, "{name}");
        assert_eq!(
            offer.override_allowed,
            manual_override_allowed(&definition, state)
        );
        assert_eq!(verdict.is_ok(), offer.override_allowed, "{name}");
        assert_eq!(!offer.override_targets.is_empty(), allowed, "{name}");
    }
}

#[test]
fn a_terminal_state_of_a_unified_definition_offers_no_override() {
    let definition = load_from_str(UNIFIED).unwrap();

    let offer = transition_offer(&definition, "completed").unwrap();

    assert!(!offer.override_allowed);
}

#[test]
fn a_saved_definition_without_a_shape_reads_as_legacy_and_keeps_its_field() {
    let saved = serde_json::to_value(load_from_str(LEGACY_WITH_FIELD).unwrap()).unwrap();
    assert!(
        saved.get("shape").is_none(),
        "a legacy definition adds nothing"
    );

    let definition: WorkflowDefinition = serde_json::from_value(saved).unwrap();

    assert!(manual_override_allowed(
        &definition,
        &definition.states["work"]
    ));
    let off: WorkflowDefinition = serde_json::from_value(
        serde_json::to_value(load_from_str(LEGACY_FIELD_OFF).unwrap()).unwrap(),
    )
    .unwrap();
    assert!(!manual_override_allowed(&off, &off.states["work"]));
}

#[test]
fn the_unified_shape_survives_being_saved() {
    let saved = serde_json::to_value(load_from_str(UNIFIED).unwrap()).unwrap();

    let definition: WorkflowDefinition = serde_json::from_value(saved).unwrap();

    assert!(manual_override_allowed(
        &definition,
        &definition.states["work"]
    ));
}

const STEPS: &str = r#"
schema: v2
name: steps
version: 1
states:
  work:
    action:
      type: script
      command: "true"
    exit: { next: check }
  check:
    action:
      type: script
      command: "true"
    exit: { next: completed }
  completed: {}
terminal_states: { completed: completed }
"#;

/// A record of a task whose launch spec carries `yaml` as its workflow,
/// stored by a real control plane, in `runner_state`.
fn record_of(yaml: Option<&str>, runner_state: RunnerState) -> TaskRecord {
    use crate::domain::{
        GitIdentity, ResolvedEnvironment, ResourceProfile, TaskId, TaskLaunchSpec, TaskRepository,
        TaskRepositorySource, TaskWorkspace,
    };
    use crate::orchestration::{ControlPlane, NewTask, OwnerRef};
    let root = std::env::temp_dir().join(format!("narsor-service-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let workspace = TaskWorkspace::single(TaskRepository::new(
        "narsor",
        TaskRepositorySource::Git(Box::new(GitIdentity {
            repository: Some("narsor".to_string()),
            remote_url: "https://example.com/narsor.git".to_string(),
            commit: "614c3a8".repeat(5) + "abcde",
            git_ref: None,
            ref_origin: None,
            credential: None,
        })),
    ));
    let mut spec = TaskLaunchSpec::new(
        "steps",
        1,
        serde_json::json!({}),
        workspace,
        ResolvedEnvironment::harness_only("claude-code"),
        ResourceProfile::new(1000, 2048),
    );
    if let Some(yaml) = yaml {
        spec = spec.with_definition(&load_from_str(yaml).unwrap()).unwrap();
    }
    let control = ControlPlane::new(crate::persistence::StatePaths::new(&root));
    let mut record = control
        .create_task(NewTask::new(
            TaskId::parse("task-1").unwrap(),
            OwnerRef::new("oidc:https://issuer|artur"),
            crate::domain::BackendId::parse("k8s").unwrap(),
            spec,
        ))
        .unwrap();
    let _ = std::fs::remove_dir_all(&root);
    record.runner_state = runner_state;
    record
}

#[test]
fn a_step_choice_names_a_step_of_the_workflow() {
    let record = record_of(Some(STEPS), RunnerState::Stopped);

    assert!(check_step_exists(&record, "check").is_ok());
    match check_step_exists(&record, "nowhere") {
        Err(ProtocolError::BadRequest { code, .. }) => assert_eq!(code, "unknown_step"),
        other => panic!("an unknown step: {other:?}"),
    }
    match check_step_exists(&record, "  ") {
        Err(ProtocolError::BadRequest { code, .. }) => assert_eq!(code, "missing_target"),
        other => panic!("an empty target: {other:?}"),
    }
}

#[test]
fn a_step_choice_is_not_checked_without_a_workflow_snapshot() {
    let record = record_of(None, RunnerState::Stopped);

    assert!(check_step_exists(&record, "anything").is_ok());
}

#[test]
fn the_steps_of_a_stopped_task_start_at_the_initial_state_and_name_their_kind() {
    let record = record_of(Some(STEPS), RunnerState::Stopped);
    let initial = record
        .launch_spec
        .definition()
        .unwrap()
        .initial_state
        .to_string();

    let steps = workflow_steps(&record).expect("a stopped task lists its steps");

    assert_eq!(
        steps.first().map(|step| step.id.as_str()),
        Some(initial.as_str())
    );
    let mut rest: Vec<&str> = steps.iter().skip(1).map(|step| step.id.as_str()).collect();
    let sorted = {
        let mut copy = rest.clone();
        copy.sort_unstable();
        copy
    };
    assert_eq!(rest.len(), 2);
    assert_eq!(rest.as_mut_slice(), sorted.as_slice());
    let kind_of = |id: &str| steps.iter().find(|step| step.id == id).unwrap().kind;
    assert_eq!(kind_of("work"), StepKind::Script);
    assert_eq!(kind_of("check"), StepKind::Script);
    assert_eq!(kind_of("completed"), StepKind::Terminal);
}

#[test]
fn a_profile_step_is_listed_with_its_own_kind() {
    let yaml = r#"
schema: v2
name: steps
version: 1
states:
  profile:
    action:
      type: profile
      harness: claude-code
      prompt: Decide the models of the later steps.
    exit: { next: work }
  work:
    action:
      type: agent
      harness: claude-code
      prompt: Do the work.
    exit: { next: completed }
  completed: {}
terminal_states: { completed: completed }
"#;
    let record = record_of(Some(yaml), RunnerState::Stopped);

    let steps = workflow_steps(&record).expect("a stopped task lists its steps");

    let kind_of = |id: &str| steps.iter().find(|step| step.id == id).unwrap().kind;
    assert_eq!(kind_of("profile"), StepKind::Profile);
    assert_eq!(kind_of("work"), StepKind::Agent);
}

#[test]
fn a_task_that_is_not_stopped_lists_no_steps() {
    for state in [RunnerState::Running, RunnerState::PauseRequested] {
        assert!(workflow_steps(&record_of(Some(STEPS), state)).is_none());
    }
}

#[test]
fn a_stopped_task_without_a_workflow_snapshot_lists_no_steps() {
    assert!(workflow_steps(&record_of(None, RunnerState::Stopped)).is_none());
}
