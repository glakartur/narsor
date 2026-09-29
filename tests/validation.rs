//! Static validation of workflow definitions (U2, `WORKFLOW_SCHEMA.md` §5).
//!
//! Every rule has its own fixture under `tests/fixtures/invalid/`, so the
//! offending YAML is readable as YAML instead of as struct literals. The two
//! rules that cannot be expressed in a valid-looking file — an `initial_state`
//! that does not exist, which the loader derives from the file itself — are
//! built as domain structures instead.

use std::collections::HashMap;

use narsor::domain::{State, ValidationError, WorkflowDefinition};
use narsor::workflow::validator::validate_path;
use narsor::workflow::{load_from_file, load_from_str};

fn fixture_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Validates an invalid fixture and returns the errors it produced.
fn errors_of(fixture: &str) -> Vec<ValidationError> {
    let path = fixture_path(&format!("invalid/{fixture}"));
    let errors = validate_path(&path).unwrap_or_else(|err| panic!("{fixture} must load: {err}"));
    assert!(
        !errors.is_empty(),
        "{fixture} is meant to be rejected, but validation found nothing"
    );
    errors
}

fn describe(errors: &[ValidationError]) -> String {
    errors
        .iter()
        .map(|err| err.to_string())
        .collect::<Vec<_>>()
        .join("; ")
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

#[test]
fn reference_example_passes_validation() {
    let definition = load_from_file(fixture_path("implement_feature.yaml"))
        .expect("the §4 example must load without modification");

    if let Err(errors) = definition.validate() {
        panic!(
            "the §4 example must validate cleanly, got: {}",
            describe(&errors)
        );
    }
}

#[test]
fn reference_example_passes_validation_through_the_yaml_entry_point() {
    let errors = validate_path(fixture_path("implement_feature.yaml"))
        .expect("the §4 example must load without modification");
    assert!(errors.is_empty(), "got: {}", describe(&errors));
}

#[test]
fn a_loop_back_to_an_earlier_state_is_not_an_error() {
    // `fix_code -> test -> fix_code` is legal because `test` also leads to
    // `create_pr`; the rule is "a path to a terminal exists", not "no cycles".
    let definition = load_from_file(fixture_path("implement_feature.yaml")).unwrap();
    assert_eq!(definition.states["fix_code"].transitions["fixed"], "test");
    assert!(definition.validate().is_ok());
}

// ---------------------------------------------------------------------------
// Group 1: graph consistency
// ---------------------------------------------------------------------------

#[test]
fn transition_to_unknown_state_is_rejected() {
    let errors = errors_of("transition_to_unknown_state.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::TransitionToUnknownState(state, label, target)
                if state == "start" && label == "success" && target == "nonexistent_state"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn output_without_a_transition_is_rejected() {
    let errors = errors_of("output_without_transition.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::OutputWithoutTransition { state, label }
                if state == "start" && label == "partial_failure"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn transition_for_an_undeclared_label_is_rejected() {
    let errors = errors_of("transition_without_output.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::TransitionWithoutOutput { state, label }
                if state == "start" && label == "failure"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn an_output_declared_twice_in_mixed_forms_is_rejected() {
    let errors = errors_of("duplicate_output.yaml");
    let duplicates: Vec<_> = errors
        .iter()
        .filter(|err| matches!(err, ValidationError::DuplicateOutput { .. }))
        .collect();
    assert!(
        matches!(
            duplicates[..],
            [ValidationError::DuplicateOutput { state, label }]
                if state == "start" && label == "success"
        ),
        "exactly one duplicate is reported, got: {}",
        describe(&errors)
    );
}

#[test]
fn described_outputs_pass_validation() {
    let definition = load_from_str(
        r#"
name: described
version: 1
states:
  start:
    action:
      type: script
      command: ./run.sh
    outputs:
      - success
      - label: failure
        description: The run did not finish.
    transitions:
      success: done
      failure: done
  done:
    description: end
terminal_states: [done]
"#,
    )
    .expect("the described form loads");

    if let Err(errors) = definition.validate() {
        panic!("got: {}", describe(&errors));
    }
}

#[test]
fn unreachable_state_is_rejected() {
    let errors = errors_of("unreachable_state.yaml");
    assert!(
        errors.iter().any(
            |err| matches!(err, ValidationError::UnreachableState(state) if state == "orphan")
        ),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn a_loop_without_an_exit_is_rejected_as_a_deadlock() {
    let errors = errors_of("no_path_to_terminal.yaml");
    for state_id in ["a", "b"] {
        assert!(
            errors.iter().any(
                |err| matches!(err, ValidationError::NoPathToTerminal(state) if state == state_id)
            ),
            "expected a deadlock error for '{state_id}', got: {}",
            describe(&errors)
        );
    }
    // `start` can still reach `done`, so it must not be reported.
    assert!(
        !errors
            .iter()
            .any(|err| matches!(err, ValidationError::NoPathToTerminal(state) if state == "start")),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn terminal_state_with_transitions_is_rejected() {
    let errors = errors_of("terminal_state_with_transitions.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::TerminalStateWithTransitions(state) if state == "done"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn workflow_without_a_terminal_state_is_rejected() {
    let errors = errors_of("no_terminal_state.yaml");
    assert!(
        errors
            .iter()
            .any(|err| matches!(err, ValidationError::NoTerminalState)),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn terminal_states_naming_an_unknown_state_is_rejected() {
    let errors = errors_of("unknown_terminal_state.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::UnknownTerminalState(state) if state == "cancelled"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn the_mapping_form_of_terminal_states_is_checked_for_unknown_states_too() {
    // The loader turns `terminal_states` into a flag on the named state, so an
    // entry naming a state that does not exist disappears during compilation.
    // The YAML entry points exist precisely to keep this rule alive — and they
    // must see the mapping form as well as the list form.
    let yaml = r#"
name: unknown-mapped-terminal
version: 1

states:
  start:
    action:
      type: script
      command: ./run.sh
    outputs:
      - success
    transitions:
      success: done

  done:
    description: Koniec.

terminal_states:
  done: completed
  cancelled: cancelled
"#;

    let errors = narsor::workflow::validate_source(yaml).expect("the file must compile");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::UnknownTerminalState(state) if state == "cancelled"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn initial_state_that_does_not_exist_is_rejected() {
    // The loader always derives `initial_state` from the file, so this rule is
    // only reachable for a definition built in code.
    let mut states = HashMap::new();
    states.insert(
        "done".to_string(),
        State {
            id: "done".to_string(),
            description: None,
            action: None,
            outputs: Vec::new(),
            transitions: HashMap::new(),
            decision: None,
            timeout_secs: None,
            retry_policy: None,
            terminal: true,
            terminal_status: None,
            allow_manual_override: false,
        },
    );

    let definition = WorkflowDefinition {
        id: "handmade".to_string(),
        name: "handmade".to_string(),
        version: 1,
        description: None,
        inputs: Vec::new(),
        globals: Default::default(),
        workspace: None,
        states,
        initial_state: "missing".to_string(),
    };

    let errors = definition.validate().unwrap_err();
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::InitialStateNotFound(state) if state == "missing"
        )),
        "got: {}",
        describe(&errors)
    );
}

// ---------------------------------------------------------------------------
// Group 2: action correctness
// ---------------------------------------------------------------------------

#[test]
fn script_action_without_a_command_is_rejected() {
    let errors = errors_of("missing_script_command.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::MissingActionField { state, action_type, field }
                if state == "start" && *action_type == "script" && *field == "command"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn script_action_with_the_command_key_omitted_is_rejected_by_the_yaml_layer() {
    // serde pre-empts the validator here: `command` is a required key, so a
    // file without it never compiles into a definition at all.
    let yaml = r#"
name: no-command-key
version: 1
states:
  start:
    action:
      type: script
    outputs: [success]
    transitions:
      success: done
  done:
    description: Koniec.
terminal_states: [done]
"#;
    let err = load_from_str(yaml).expect_err("a script without `command` must not load");
    assert!(
        err.to_string().contains("command"),
        "the message must name the missing field, got: {err}"
    );
}

#[test]
fn agent_action_with_empty_outputs_is_rejected() {
    let errors = errors_of("empty_agent_outputs.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::EmptyOutputs { state, action_type }
                if state == "work" && *action_type == "agent"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn non_terminal_state_without_an_action_is_rejected() {
    let errors = errors_of("non_terminal_without_action.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::NonTerminalStateWithoutAction(state) if state == "start"
        )),
        "got: {}",
        describe(&errors)
    );
}

// ---------------------------------------------------------------------------
// Group 3: decision correctness
// ---------------------------------------------------------------------------

#[test]
fn agent_decision_without_a_prompt_is_rejected() {
    let errors = errors_of("agent_decision_without_prompt.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::AgentDecisionMissingField { state, field }
                if state == "start" && *field == "prompt"
        )),
        "got: {}",
        describe(&errors)
    );
    // The harness is present, so only the prompt may be reported.
    assert!(
        !errors.iter().any(|err| matches!(
            err,
            ValidationError::AgentDecisionMissingField { field, .. } if *field == "harness"
        )),
        "got: {}",
        describe(&errors)
    );
}

// ---------------------------------------------------------------------------
// Group 4: variables and context
// ---------------------------------------------------------------------------

#[test]
fn unknown_input_in_a_prompt_is_rejected() {
    let errors = errors_of("unknown_input_reference.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::UnknownInputReference { state, field, variable }
                if state == "start" && field == "action.prompt" && variable == "nonexistent"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn context_references_and_filter_expressions_are_left_to_the_runtime() {
    let yaml = r#"
name: runtime-references
version: 1
inputs:
  - name: feature_description
    type: string
states:
  start:
    action:
      type: agent
      harness: claude-code
      prompt: "{{ context.clarification_text | default(inputs.feature_description) }} {{ context.anything }}"
    outputs: [done]
    transitions:
      done: finished
  finished:
    description: Koniec.
terminal_states: [finished]
"#;
    let definition = load_from_str(yaml).unwrap();
    if let Err(errors) = definition.validate() {
        panic!(
            "runtime references must pass validation, got: {}",
            describe(&errors)
        );
    }
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

#[test]
fn a_single_state_that_is_both_initial_and_terminal_passes() {
    let yaml = r#"
name: single-state
version: 1
states:
  only:
    description: Początek i koniec w jednym.
terminal_states: [only]
"#;
    let definition = load_from_str(yaml).unwrap();
    assert_eq!(definition.initial_state, "only");
    if let Err(errors) = definition.validate() {
        panic!(
            "a single terminal state is a valid workflow, got: {}",
            describe(&errors)
        );
    }
}

#[test]
fn five_violations_are_reported_as_five_errors() {
    let errors = errors_of("five_violations.yaml");
    assert_eq!(
        errors.len(),
        5,
        "expected exactly five errors, got: {}",
        describe(&errors)
    );

    let expected: [fn(&ValidationError) -> bool; 5] = [
        |err| {
            matches!(err, ValidationError::UnknownInputReference { state, variable, .. }
            if state == "start" && variable == "missing")
        },
        |err| {
            matches!(err, ValidationError::OutputWithoutTransition { state, label }
            if state == "start" && label == "extra")
        },
        |err| {
            matches!(err, ValidationError::MissingActionField { state, field, .. }
            if state == "middle" && *field == "command")
        },
        |err| {
            matches!(err, ValidationError::TransitionWithoutOutput { state, label }
            if state == "middle" && label == "failure")
        },
        |err| matches!(err, ValidationError::UnreachableState(state) if state == "orphan"),
    ];
    for (index, matches_rule) in expected.iter().enumerate() {
        assert!(
            errors.iter().any(matches_rule),
            "violation {} is missing, got: {}",
            index + 1,
            describe(&errors)
        );
    }
}

/// An agent state that names no harness is valid (D-F).
///
/// Every setting has a value built into the binary, so "no harness anywhere"
/// cannot happen and there is nothing for a static check to report. Validation
/// stays a pure function of the source: it does not consult the environment,
/// and it gives the same verdict wherever it is run.
#[test]
fn an_agent_state_without_a_harness_is_valid() {
    let yaml = r#"
name: portable
version: 1
states:
  think:
    action:
      type: agent
      prompt: Do the thing.
    outputs:
      - done
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states:
  - completed
"#;

    let errors = narsor::workflow::validate_source(yaml).expect("the file must compile");

    assert!(
        errors.is_empty(),
        "a workflow without a harness is portable, not invalid: {errors:?}"
    );
}

/// A misspelled field is still caught.
///
/// What changed is the status of `harness` being *absent*; an unknown key is
/// still a deserialization error, because the workflow schema keeps
/// `deny_unknown_fields`.
#[test]
fn a_misspelled_harness_field_is_still_refused() {
    let yaml = r#"
name: typo
version: 1
states:
  think:
    action:
      type: agent
      harnes: claude-code
      prompt: Do the thing.
    outputs:
      - done
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states:
  - completed
"#;

    assert!(
        narsor::workflow::validate_source(yaml).is_err(),
        "'harnes' is not a field of an agent action"
    );
}

/// A harness that is present but blank is still refused.
///
/// Absent means "use the configured default"; empty means the author wrote
/// something and it came to nothing. Catching it here costs nothing, where
/// catching it at run time costs a cloned workspace and every state that ran
/// before it.
#[test]
fn a_blank_harness_is_still_a_violation() {
    for written in ["\"\"", "\"   \""] {
        let yaml = format!(
            r#"
name: blank
version: 1
states:
  think:
    action:
      type: agent
      harness: {written}
      prompt: Do the thing.
    outputs:
      - done
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states:
  - completed
"#
        );

        let errors = narsor::workflow::validate_source(&yaml).expect("the file must compile");

        assert!(
            errors.iter().any(|error| matches!(
                error,
                ValidationError::MissingActionField { field, .. } if *field == "harness"
            )),
            "harness: {written} must be reported: {errors:?}"
        );
    }
}
