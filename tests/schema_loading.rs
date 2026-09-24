//! Loading of YAML workflow definitions into the domain model (U1).
//!
//! The reference fixture is `tests/fixtures/implement_feature.yaml`, copied
//! verbatim from `WORKFLOW_SCHEMA.md` §4.

use narsor::domain::{
    ActionConfig, ExecutionStatus, HumanInputType, ParamType, State, TerminalStatus,
    WorkflowDefinition, WorkspaceSpec,
};
use narsor::engine::terminal_status;
use narsor::workflow::{load_from_file, load_from_str};

fn fixture_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn load_reference() -> WorkflowDefinition {
    load_from_file(fixture_path("implement_feature.yaml"))
        .expect("the §4 example must load without modification")
}

fn state<'a>(definition: &'a WorkflowDefinition, id: &str) -> &'a State {
    definition
        .states
        .get(id)
        .unwrap_or_else(|| panic!("state '{id}' is missing"))
}

#[test]
fn reference_example_loads_with_all_states() {
    let definition = load_reference();

    assert_eq!(definition.name, "implement-feature");
    assert_eq!(definition.id, "implement-feature");
    assert_eq!(definition.version, 1);
    // §4 declares nine states: analyze, human_clarification, implement, test,
    // fix_code, human_review_tests, create_pr, completed, failed.
    assert_eq!(definition.states.len(), 9);
    assert_eq!(definition.initial_state, "analyze");

    assert!(state(&definition, "completed").terminal);
    assert!(state(&definition, "failed").terminal);
    assert!(!state(&definition, "analyze").terminal);
}

#[test]
fn terminal_states_have_no_action() {
    let definition = load_reference();

    assert!(state(&definition, "completed").action.is_none());
    assert!(state(&definition, "failed").action.is_none());
    assert_eq!(
        state(&definition, "completed").description.as_deref(),
        Some("Sukces.")
    );
}

#[test]
fn inputs_keep_their_declared_types() {
    let definition = load_reference();

    let feature = &definition.inputs[0];
    assert_eq!(feature.name, "feature_description");
    assert_eq!(feature.param_type, ParamType::String);
    assert!(feature.required);

    let repo = &definition.inputs[1];
    assert_eq!(repo.name, "target_repo");
    assert_eq!(repo.param_type, ParamType::Repository);
}

#[test]
fn global_workspace_becomes_a_definition_level_spec() {
    let definition = load_reference();

    match definition
        .workspace
        .expect("globals.workspace must be kept")
    {
        WorkspaceSpec::Git {
            repository,
            git_ref,
        } => {
            assert_eq!(repository, "{{ inputs.target_repo }}");
            assert_eq!(git_ref.as_deref(), Some("main"));
        }
        other => panic!("expected a git workspace, got {other:?}"),
    }
}

#[test]
fn global_retry_policy_is_propagated_and_can_be_overridden() {
    let yaml = r#"
name: retry-demo
version: 1
globals:
  retry_policy:
    max_attempts: 2
    backoff: 10s
states:
  inherits:
    action:
      type: script
      command: ./a.sh
    outputs: [success]
    transitions:
      success: overrides
  overrides:
    action:
      type: script
      command: ./b.sh
    retry_policy:
      max_attempts: 3
    outputs: [success]
    transitions:
      success: done
  done:
    description: end
terminal_states:
  - done
"#;

    let definition = load_from_str(yaml).unwrap();

    let inherited = state(&definition, "inherits")
        .retry_policy
        .as_ref()
        .expect("the global policy must reach a state without its own policy");
    assert_eq!(inherited.max_attempts, 2);
    assert_eq!(inherited.backoff_secs, 10);

    let overridden = state(&definition, "overrides")
        .retry_policy
        .as_ref()
        .expect("the state keeps its own policy");
    assert_eq!(overridden.max_attempts, 3);
    assert_eq!(overridden.backoff_secs, 0);

    assert_eq!(definition.globals.retry_policy.unwrap().max_attempts, 2);
}

#[test]
fn global_timeout_compiles_to_seconds_and_reaches_every_state() {
    let definition = load_reference();

    assert_eq!(definition.globals.timeout_secs, Some(7200));
    assert_eq!(state(&definition, "analyze").timeout_secs, Some(7200));
    assert_eq!(state(&definition, "test").timeout_secs, Some(7200));
}

#[test]
fn exit_codes_round_trip_into_the_domain() {
    let definition = load_reference();

    let action = state(&definition, "test")
        .action
        .as_ref()
        .expect("the test state has an action");
    let script = match action {
        ActionConfig::Script(script) => script,
        other => panic!("expected a script action, got {other:?}"),
    };

    assert_eq!(script.command, "./run_tests.sh");
    assert_eq!(script.exit_codes.len(), 3);
    assert_eq!(script.exit_codes[&0], "success");
    assert_eq!(script.exit_codes[&1], "failure");
    assert_eq!(script.exit_codes[&2], "partial_failure");
}

#[test]
fn script_args_stay_separate_arguments() {
    let definition = load_reference();

    let action = state(&definition, "create_pr").action.as_ref().unwrap();
    let script = match action {
        ActionConfig::Script(script) => script,
        other => panic!("expected a script action, got {other:?}"),
    };

    assert_eq!(
        script.args,
        vec![
            "--title".to_string(),
            "Feature: {{ inputs.feature_description }}".to_string(),
        ]
    );
}

#[test]
fn human_action_inputs_are_loaded() {
    let definition = load_reference();

    let action = state(&definition, "human_clarification")
        .action
        .as_ref()
        .unwrap();
    let human = match action {
        ActionConfig::Human(human) => human,
        other => panic!("expected a human action, got {other:?}"),
    };

    assert_eq!(
        human.instruction,
        "Agent ma wątpliwości. Proszę doprecyzować wymagania."
    );
    assert_eq!(human.inputs.len(), 1);
    let field = &human.inputs[0];
    assert_eq!(field.name, "clarification_text");
    assert_eq!(field.field_type, HumanInputType::Text);
    assert!(field.required);

    // A human action without a form still loads (human_review_tests in §4).
    let action = state(&definition, "human_review_tests")
        .action
        .as_ref()
        .unwrap();
    match action {
        ActionConfig::Human(human) => assert!(human.inputs.is_empty()),
        other => panic!("expected a human action, got {other:?}"),
    }
}

#[test]
fn human_select_field_keeps_its_options() {
    let yaml = r#"
name: review
version: 1
states:
  review:
    action:
      type: human
      message: "Proszę o review zmian."
      inputs:
        - name: approval_status
          type: select
          options: [approved, changes_requested, rejected]
          required: true
        - name: comments
          type: text
          required: false
    outputs: [approved, changes_requested, rejected]
    transitions:
      approved: done
      changes_requested: done
      rejected: done
  done:
    description: end
terminal_states: [done]
"#;

    let definition = load_from_str(yaml).unwrap();
    let human = match state(&definition, "review").action.as_ref().unwrap() {
        ActionConfig::Human(human) => human,
        other => panic!("expected a human action, got {other:?}"),
    };

    assert_eq!(human.inputs.len(), 2);
    assert_eq!(human.inputs[0].field_type, HumanInputType::Select);
    assert_eq!(
        human.inputs[0].options,
        vec!["approved", "changes_requested", "rejected"]
    );
    assert!(!human.inputs[1].required);
}

#[test]
fn explicit_initial_flag_beats_file_order() {
    let yaml = r#"
name: flagged-initial
version: 1
states:
  first_in_file:
    action:
      type: script
      command: ./a.sh
    outputs: [success]
    transitions:
      success: done
  real_start:
    initial: true
    action:
      type: script
      command: ./b.sh
    outputs: [success]
    transitions:
      success: first_in_file
  done:
    description: end
terminal_states: [done]
"#;

    let definition = load_from_str(yaml).unwrap();
    assert_eq!(definition.initial_state, "real_start");
}

#[test]
fn workflow_without_globals_has_no_per_state_defaults() {
    let yaml = r#"
name: bare
version: 1
states:
  only:
    action:
      type: script
      command: ./a.sh
    outputs: [success]
    transitions:
      success: done
  done:
    description: end
terminal_states: [done]
"#;

    let definition = load_from_str(yaml).unwrap();

    assert!(definition.inputs.is_empty());
    assert!(definition.workspace.is_none());
    assert!(definition.globals.timeout_secs.is_none());
    assert!(definition.globals.retry_policy.is_none());
    assert!(state(&definition, "only").timeout_secs.is_none());
    assert!(state(&definition, "only").retry_policy.is_none());
}

#[test]
fn wait_action_keeps_event_type_filter_and_timeout() {
    let yaml = r#"
name: waiting
version: 1
states:
  wait_for_review:
    action:
      type: wait
      event_type: github.pull_request.reviewed
      filter:
        pr_number: "{{ context.pr_number }}"
      timeout: 24h
    outputs: [received, timeout]
    transitions:
      received: done
      timeout: done
  done:
    description: end
terminal_states: [done]
"#;

    let definition = load_from_str(yaml).unwrap();
    let wait = match state(&definition, "wait_for_review")
        .action
        .as_ref()
        .unwrap()
    {
        ActionConfig::Wait(wait) => wait,
        other => panic!("expected a wait action, got {other:?}"),
    };

    assert_eq!(wait.event, "github.pull_request.reviewed");
    assert_eq!(wait.timeout_secs, Some(86_400));
    assert_eq!(
        wait.filter.as_ref().unwrap()["pr_number"],
        serde_json::json!("{{ context.pr_number }}")
    );
}

#[test]
fn agent_action_keeps_read_only_paths_and_parameters() {
    let yaml = r#"
name: agent-demo
version: 1
states:
  scan:
    action:
      type: agent
      harness: claude-code
      model: sonnet-3.5
      prompt: |
        Analizuj kod.
      parameters:
        max_tokens: 4096
        temperature: 0.7
      read_only_paths:
        - ../other-repo/src
    outputs: [success]
    transitions:
      success: done
  done:
    description: end
terminal_states: [done]
"#;

    let definition = load_from_str(yaml).unwrap();
    let agent = match state(&definition, "scan").action.as_ref().unwrap() {
        ActionConfig::Agent(agent) => agent,
        other => panic!("expected an agent action, got {other:?}"),
    };

    assert_eq!(agent.harness.as_deref(), Some("claude-code"));
    assert_eq!(agent.model.as_deref(), Some("sonnet-3.5"));
    assert_eq!(agent.read_only_paths, vec!["../other-repo/src"]);
    assert_eq!(agent.parameters["max_tokens"], serde_json::json!(4096));
}

#[test]
fn human_decision_message_is_not_lost() {
    use narsor::domain::DecisionConfig;

    let yaml = r#"
name: decision-demo
version: 1
states:
  pick:
    action:
      type: script
      command: ./a.sh
    decision:
      type: human
      message: "Wybierz kolejny krok na podstawie wyników testów."
    outputs: [success]
    transitions:
      success: done
  done:
    description: end
terminal_states: [done]
"#;

    let definition = load_from_str(yaml).unwrap();
    match state(&definition, "pick").decision.as_ref().unwrap() {
        DecisionConfig::Human(human) => assert_eq!(
            human.message.as_deref(),
            Some("Wybierz kolejny krok na podstawie wyników testów.")
        ),
        other => panic!("expected a human decision, got {other:?}"),
    }
}

#[test]
fn rules_and_agent_decisions_load() {
    use narsor::domain::DecisionConfig;

    let yaml = r#"
name: decisions
version: 1
states:
  plain:
    action:
      type: script
      command: ./a.sh
    decision:
      type: rules
    outputs: [success]
    transitions:
      success: judged
  judged:
    action:
      type: script
      command: ./b.sh
    decision:
      type: agent
      harness: claude-code
      prompt: |
        Czy zmiany są bezpieczne?
    outputs: [safe_to_merge]
    transitions:
      safe_to_merge: done
  done:
    description: end
terminal_states: [done]
"#;

    let definition = load_from_str(yaml).unwrap();
    assert!(matches!(
        state(&definition, "plain").decision,
        Some(DecisionConfig::Rules)
    ));
    match state(&definition, "judged").decision.as_ref().unwrap() {
        DecisionConfig::Agent(agent) => {
            assert_eq!(agent.harness.as_deref(), Some("claude-code"));
            assert!(agent.prompt.as_ref().unwrap().contains("bezpieczne"));
        }
        other => panic!("expected an agent decision, got {other:?}"),
    }
}

#[test]
fn unknown_key_is_reported_with_its_name() {
    let yaml = r#"
name: typo
version: 1
states:
  broken:
    action:
      type: script
      commnad: ./a.sh
    outputs: [success]
    transitions:
      success: done
  done:
    description: end
terminal_states: [done]
"#;

    let error = load_from_str(yaml).expect_err("a misspelled key must be rejected");
    let message = error.to_string();
    assert!(
        message.contains("commnad"),
        "error should name the offending key, got: {message}"
    );
}

#[test]
fn unsupported_action_type_is_reported() {
    let yaml = r#"
name: docker
version: 1
states:
  broken:
    action:
      type: docker
      image: alpine
    outputs: [success]
    transitions:
      success: done
  done:
    description: end
terminal_states: [done]
"#;

    let error = load_from_str(yaml).expect_err("an unknown action type must be rejected");
    let message = error.to_string();
    assert!(
        message.contains("docker"),
        "error should name the unknown type, got: {message}"
    );
}

#[test]
fn unparseable_duration_is_an_error_not_a_panic() {
    let yaml = r#"
name: bad-duration
version: 1
globals:
  timeout: "2 hours"
states:
  only:
    description: end
terminal_states: [only]
"#;

    let error = load_from_str(yaml).expect_err("an invalid duration must be rejected");
    let message = error.to_string();
    assert!(
        message.contains("hours"),
        "error should quote the bad duration, got: {message}"
    );
}

#[test]
fn terminal_state_naming_a_missing_state_still_loads() {
    // U1 only has to carry the information through; rejecting it is U2's job.
    let yaml = r#"
name: dangling-terminal
version: 1
states:
  only:
    description: end
terminal_states:
  - only
  - cancelled
"#;

    let definition = load_from_str(yaml).expect("loading must not fail here");
    assert!(state(&definition, "only").terminal);
    assert!(!definition.states.contains_key("cancelled"));
}

// ---------------------------------------------------------------------------
// `terminal_states`: the explicit status marker of §3.5
// ---------------------------------------------------------------------------

#[test]
fn the_reference_example_declares_its_failure_terminal_explicitly() {
    let definition = load_reference();

    assert_eq!(
        state(&definition, "completed").terminal_status,
        Some(TerminalStatus::Completed)
    );
    assert_eq!(
        state(&definition, "failed").terminal_status,
        Some(TerminalStatus::Failed)
    );
}

#[test]
fn the_mapping_form_gives_each_terminal_its_declared_status() {
    let yaml = r#"
name: mapped-terminals
version: 1
states:
  start:
    action:
      type: script
      command: /bin/true
    outputs: [ok]
    transitions:
      ok: completed
  completed:
    description: done
  failed:
    description: not done
  gave_up:
    description: abandoned
terminal_states:
  completed: completed
  failed: failed
  gave_up: cancelled
"#;

    let definition = load_from_str(yaml).expect("the mapping form must load");

    for id in ["completed", "failed", "gave_up"] {
        assert!(state(&definition, id).terminal, "{id} must be terminal");
    }
    assert_eq!(
        state(&definition, "completed").terminal_status,
        Some(TerminalStatus::Completed)
    );
    assert_eq!(
        state(&definition, "failed").terminal_status,
        Some(TerminalStatus::Failed)
    );
    assert_eq!(
        state(&definition, "gave_up").terminal_status,
        Some(TerminalStatus::Cancelled)
    );
    assert!(state(&definition, "start").terminal_status.is_none());
}

#[test]
fn the_list_form_still_loads_and_every_entry_means_completed() {
    // Backwards compatibility: an existing file keeps working, and a state
    // called `failed` in the list form completes the execution — the list
    // carries no failure information and nothing guesses from the name.
    let yaml = r#"
name: listed-terminals
version: 1
states:
  start:
    action:
      type: script
      command: /bin/true
    outputs: [ok]
    transitions:
      ok: completed
  completed:
    description: done
  failed:
    description: not done
terminal_states:
  - completed
  - failed
"#;

    let definition = load_from_str(yaml).expect("the list form must keep working");

    for id in ["completed", "failed"] {
        let terminal = state(&definition, id);
        assert!(terminal.terminal, "{id} must be terminal");
        assert!(
            terminal.terminal_status.is_none(),
            "the list form declares no status for {id}"
        );
        assert_eq!(
            terminal_status(terminal),
            ExecutionStatus::Completed,
            "every list entry ends the execution as completed"
        );
    }
}

#[test]
fn an_unknown_terminal_status_names_the_state_the_value_and_the_permitted_set() {
    let yaml = r#"
name: bad-terminal-status
version: 1
states:
  only:
    description: end
terminal_states:
  only: kaput
"#;

    let error = load_from_str(yaml).expect_err("an unknown status must be rejected");
    let message = error.to_string();
    assert!(message.contains("only"), "names the state: {message}");
    assert!(message.contains("kaput"), "names the bad value: {message}");
    for permitted in ["completed", "failed", "cancelled"] {
        assert!(
            message.contains(permitted),
            "names '{permitted}' among the permitted values: {message}"
        );
    }
}

#[test]
fn the_mapping_form_carries_a_missing_state_through_to_the_validator() {
    // Same contract as the list form: the loader does not reject it, the
    // validator's `UnknownTerminalState` rule does (U2).
    let yaml = r#"
name: dangling-mapped-terminal
version: 1
states:
  only:
    description: end
terminal_states:
  only: completed
  cancelled: cancelled
"#;

    let definition = load_from_str(yaml).expect("loading must not fail here");
    assert!(state(&definition, "only").terminal);
    assert!(!definition.states.contains_key("cancelled"));
}
