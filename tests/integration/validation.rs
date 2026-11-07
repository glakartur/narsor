//! Static validation of workflow definitions (U2, `docs/dev/workflow-schema.md` §5).
//!
//! Every rule has its own fixture under `tests/fixtures/invalid/`, so the
//! offending YAML is readable as YAML instead of as struct literals. The two
//! rules that cannot be expressed in a valid-looking file — an `initial_state`
//! that does not exist, which the loader derives from the file itself — are
//! built as domain structures instead.

use std::collections::HashMap;

use narsor_engine::domain::{State, ValidationError, WorkflowDefinition};
use narsor_engine::workflow::validator::validate_path;
use narsor_engine::workflow::{load_from_file, load_from_str};

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
            ValidationError::ChoiceWithoutTransition { state, label }
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
            ValidationError::TransitionWithoutChoice { state, label }
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
        .filter(|err| matches!(err, ValidationError::DuplicateChoice { .. }))
        .collect();
    assert!(
        matches!(
            duplicates[..],
            [ValidationError::DuplicateChoice { state, label }]
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
    choices:
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
    choices:
      - success
    transitions:
      success: done

  done:
    description: Koniec.

terminal_states:
  done: completed
  cancelled: cancelled
"#;

    let errors = narsor_engine::workflow::validate_source(yaml).expect("the file must compile");
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
            max_entries: None,
            description: None,
            action: None,
            variables: Vec::new(),
            artifacts: Vec::new(),
            choices: Vec::new(),
            transitions: HashMap::new(),
            decision: None,
            timeout_secs: None,
            retry_policy: None,
            terminal: true,
            terminal_status: None,
            allow_manual_override: false,
            workspace: None,
            image: None,
        },
    );

    let definition = WorkflowDefinition {
        id: "handmade".to_string(),
        name: "handmade".to_string(),
        version: 1,
        description: None,
        inputs: Vec::new(),
        globals: Default::default(),
        source_schema: Default::default(),
        shape: Default::default(),
        repositories: Vec::new(),
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

#[test]
fn validates_step_reply_sources_and_agent_entry_limits() {
    let valid = r#"
name: replies
version: 1
states:
  review:
    action: { type: agent, prompt: review }
    choices: [done]
    transitions: { done: fix }
  fix:
    max_entries: 3
    action:
      type: agent
      prompt: "Fix: {{ steps.review.reply }}"
    choices: [done]
    transitions: { done: finished }
  finished: { description: done }
terminal_states: [finished]
"#;
    assert!(load_from_str(valid).unwrap().validate().is_ok());

    let invalid = valid
        .replace("steps.review.reply", "steps.finished.reply")
        .replace("max_entries: 3", "max_entries: 0");
    let errors = load_from_str(&invalid).unwrap().validate().unwrap_err();
    assert!(errors
        .iter()
        .any(|error| error.to_string().contains("max_entries")));
    assert!(errors
        .iter()
        .any(|error| error.to_string().contains("finished")));
}

#[test]
fn a_step_reply_from_a_human_state_is_rejected() {
    let errors = errors_of("step_reply_from_human_state.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::InvalidStepReplySource { state, step_id, field }
                if state == "work" && step_id == "gate" && field == "action.prompt"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn a_step_reply_from_a_script_state_is_rejected() {
    let errors = errors_of("step_reply_from_script_state.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::InvalidStepReplySource { state, step_id, field }
                if state == "work" && step_id == "check" && field == "action.prompt"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn a_step_field_other_than_reply_is_rejected() {
    let errors = errors_of("step_field_other_than_reply.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::UnsupportedStepReference { state, field, reference }
                if state == "work" && field == "action.prompt" && reference == "steps.review.data"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn a_step_reply_in_the_workspace_is_rejected() {
    let errors = errors_of("step_reply_in_workspace.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::InvalidGlobalsField { field, reason }
                if field == "globals.workspace.path" && reason.contains("step values")
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn a_step_reply_in_script_args_is_rejected() {
    let errors = errors_of("step_reply_in_script_args.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::StepReplyNotAllowed { state, field }
                if state == "check" && field == "action.args[0]"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn zero_max_entries_is_rejected() {
    let errors = errors_of("zero_max_entries.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::InvalidMaxEntries { state, value } if state == "work" && *value == 0
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn max_entries_on_a_human_gate_is_rejected() {
    let errors = errors_of("max_entries_on_human_gate.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::InvalidMaxEntries { state, value } if state == "gate" && *value == 3
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn an_invalid_variable_name_is_rejected() {
    let errors = errors_of("invalid_required_variable_name.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::InvalidDefinitionName { state, kind, name }
                if state == "work" && *kind == "variable" && name == "pr-url"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn a_choice_requiring_an_undefined_variable_is_rejected() {
    let errors = errors_of("undefined_required_variable.yaml");
    assert!(
        matches!(
            errors.as_slice(),
            [ValidationError::UndefinedRequiredVariable { state, label, name }]
                if state == "work" && label == "done" && name == "missing"
        ),
        "got: {}",
        describe(&errors)
    );
    let message = errors[0].to_string();
    assert!(
        message.contains("work") && message.contains("missing"),
        "names the step and the variable: {message}"
    );
}

#[test]
fn a_choice_requiring_an_undefined_artifact_is_rejected() {
    let errors = errors_of("undefined_required_artifact.yaml");
    assert!(
        matches!(
            errors.as_slice(),
            [ValidationError::UndefinedRequiredArtifact { state, label, name }]
                if state == "plan" && label == "ready" && name == "plan_doc"
        ),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn an_artifact_reference_no_step_defines_is_rejected() {
    let errors = errors_of("unknown_artifact_reference.yaml");
    assert!(
        matches!(
            errors.as_slice(),
            [ValidationError::UnknownArtifactReference { state, field, artifact }]
                if state == "work" && field == "action.prompt" && artifact == "unknown"
        ),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn a_local_variable_on_a_script_step_is_rejected() {
    let errors = errors_of("local_variable_on_script_step.yaml");
    assert!(
        matches!(
            errors.as_slice(),
            [ValidationError::LocalVariableOutsideAgent { state, name }]
                if state == "check" && name == "local.memory"
        ),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn variables_on_a_human_gate_are_rejected() {
    let errors = errors_of("variables_on_human_gate.yaml");
    assert!(
        matches!(
            errors.as_slice(),
            [ValidationError::DefinitionsNotAllowed { state, action_type }]
                if state == "gate" && *action_type == "human"
        ),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn a_resume_comment_field_may_point_to_an_optional_text_input() {
    let yaml = r#"
name: optional-comment
version: 1
states:
  work:
    action: { type: agent, prompt: do it }
    choices: [done]
    transitions: { done: gate }
  gate:
    action:
      type: human
      message: review
      inputs:
        - { name: comment, type: text }
    choices:
      - approve
      - label: revise
        resume_comment_field: comment
    transitions: { approve: finished, revise: work }
  finished: { description: done }
terminal_states: [finished]
"#;
    let errors = narsor_engine::workflow::validate_source(yaml).expect("the file must compile");
    assert!(errors.is_empty(), "got: {}", describe(&errors));
}

#[test]
fn a_step_reading_its_own_reply_passes() {
    let yaml = r#"
name: self-reply
version: 1
states:
  review:
    action:
      type: agent
      prompt: "Review again. Last time: {{ steps.review.reply }}"
    choices: [again, done]
    transitions: { again: review, done: finished }
  finished: { description: done }
terminal_states: [finished]
"#;
    let definition = load_from_str(yaml).unwrap();
    if let Err(errors) = definition.validate() {
        panic!(
            "a step may read its own earlier reply, got: {}",
            describe(&errors)
        );
    }
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
    choices: [success]
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
            ValidationError::EmptyChoices { state, action_type }
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
fn a_condition_on_an_undeclared_input_is_rejected_and_names_the_part() {
    let errors = errors_of("when_unknown_input.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::UnknownInputReference { state, field, variable }
                if state == "work" && field == "action.prompt[2].when" && variable == "nonexistent"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn a_condition_with_a_filter_is_rejected() {
    let errors = errors_of("when_with_filter.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::InvalidWhen { state, field, .. }
                if state == "work" && field == "action.prompt[2].when"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn a_condition_on_the_reply_of_a_script_step_is_rejected() {
    let errors = errors_of("when_step_result_from_script.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::InvalidStepReplySource { state, step_id, field }
                if state == "work" && step_id == "build" && field == "action.prompt[2].when"
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
  - name: description
    type: string
states:
  start:
    action:
      type: agent
      harness: claude-code
      prompt: "{{ context.clarification_text | default(inputs.description) }} {{ context.anything }}"
    choices: [done]
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
            matches!(err, ValidationError::ChoiceWithoutTransition { state, label }
            if state == "start" && label == "extra")
        },
        |err| {
            matches!(err, ValidationError::MissingActionField { state, field, .. }
            if state == "middle" && *field == "command")
        },
        |err| {
            matches!(err, ValidationError::TransitionWithoutChoice { state, label }
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
    choices:
      - done
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states:
  - completed
"#;

    let errors = narsor_engine::workflow::validate_source(yaml).expect("the file must compile");

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
    choices:
      - done
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states:
  - completed
"#;

    assert!(
        narsor_engine::workflow::validate_source(yaml).is_err(),
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
    choices:
      - done
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states:
  - completed
"#
        );

        let errors =
            narsor_engine::workflow::validate_source(&yaml).expect("the file must compile");

        assert!(
            errors.iter().any(|error| matches!(
                error,
                ValidationError::MissingActionField { field, .. } if *field == "harness"
            )),
            "harness: {written} must be reported: {errors:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Image overrides (U6, R15)
// ---------------------------------------------------------------------------

fn image_workflow(globals_image: &str, step_image: &str, terminal_image: &str) -> String {
    format!(
        r#"
name: images
version: 1
globals:
{globals_image}
states:
  build:
{step_image}
    action:
      type: script
      command: make
    choices: [success]
    transitions:
      success: done
  done:
{terminal_image}
    description: finished
terminal_states: [done]
"#
    )
}

#[test]
fn every_unusable_image_is_reported_at_once() {
    let yaml = image_workflow(
        "  image: ubuntu:24.04",
        "    image: Not/A/Valid:ref",
        "    image: ubuntu@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    );
    let errors = narsor_engine::workflow::validate_source(&yaml).expect("the file loads");
    let report = describe(&errors);

    assert!(
        errors.iter().any(|error| matches!(
            error,
            ValidationError::InvalidImage { field, reason }
                if field == "globals.image" && reason.contains("no digest")
        )),
        "a tag under the default pinned policy is refused: {report}"
    );
    assert!(
        errors.iter().any(|error| matches!(
            error,
            ValidationError::InvalidImage { field, .. } if field == "states.build.image"
        )),
        "{report}"
    );
    assert!(
        errors.iter().any(
            |error| matches!(error, ValidationError::ImageOnTerminalState(state) if state == "done")
        ),
        "{report}"
    );
}

#[test]
fn a_tag_under_the_resolve_policy_is_valid_until_task_creation() {
    let yaml = image_workflow(
        "  image:\n    reference: ubuntu:24.04\n    digest_policy: resolve",
        "    description: builds",
        "",
    );
    let errors = narsor_engine::workflow::validate_source(&yaml).expect("the file loads");
    assert!(errors.is_empty(), "{}", describe(&errors));
}

fn workspace_set_errors(set: &str) -> Vec<ValidationError> {
    let yaml = format!(
        "schema: v2\nname: shown\nversion: 1\nglobals:\n  workspaces:\n    code:\n      \
         primary: true\n    docs: {{}}\nstates:\n  work:\n    action: {{ type: agent, prompt: p }}\n    \
         variables:\n      - {{ name: pr_url }}\n    exit: {{ set: {set}, next: done }}\n  done:\n    \
         description: end\nterminal_states:\n  done: completed\n"
    );
    narsor_engine::workflow::validate_source(&yaml).expect("the file loads")
}

#[test]
fn a_set_of_the_pull_request_of_a_declared_workspace_passes() {
    let errors = workspace_set_errors(
        "{ workspace: { code: { pr_url: \"{{ context.pr_url }}\", pr: \"7\" }, docs: { pr: \"8\" } } }",
    );

    assert!(errors.is_empty(), "got: {}", describe(&errors));
}

#[test]
fn a_set_for_an_unknown_workspace_names_the_step_and_the_workspace() {
    let errors =
        workspace_set_errors("{ workspace: { payments: { pr_url: \"https://x.example\" } } }");

    assert!(
        errors.iter().any(|error| matches!(
            error,
            ValidationError::SetUnknownWorkspace { state, workspace, .. }
                if state == "work" && workspace == "payments"
        )),
        "got: {}",
        describe(&errors)
    );
    let text = describe(&errors);
    assert!(text.contains("work") && text.contains("payments"), "{text}");
}

#[test]
fn a_set_of_a_workspace_value_other_than_pr_and_pr_url_is_a_violation() {
    let errors = workspace_set_errors("{ workspace: { code: { branch: feat } } }");

    assert!(
        errors.iter().any(|error| matches!(
            error,
            ValidationError::SetUnknownWorkspaceValue { workspace, key, .. }
                if workspace == "code" && key == "branch"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn a_set_of_the_pull_request_of_a_local_folder_workspace_is_a_violation() {
    let yaml = "schema: v2\nname: shown\nversion: 1\nglobals:\n  workspaces:\n    code:\n      \
         primary: true\n    notes:\n      path: notes\nstates:\n  work:\n    \
         action: { type: agent, prompt: p }\n    variables:\n      - { name: pr_url }\n    \
         exit: { set: { workspace: { notes: { pr: \"7\" } } }, next: done }\n  done:\n    \
         description: end\nterminal_states:\n  done: completed\n";

    let errors = narsor_engine::workflow::validate_source(yaml).expect("the file loads");

    assert!(
        errors.iter().any(|error| matches!(
            error,
            ValidationError::SetWorkspaceNotGit { state, workspace, .. }
                if state == "work" && workspace == "notes"
        )),
        "got: {}",
        describe(&errors)
    );
}

/// Validates a unified-shape file whose `states` section is `states`; `done`
/// is its terminal state.
fn unified_errors(states: &str) -> Vec<ValidationError> {
    let yaml = format!(
        "schema: v2\nname: unified\nversion: 1\nstates:\n{states}\n  done:\n    description: end\n\
         terminal_states:\n  done: completed\n"
    );
    narsor_engine::workflow::validate_source(&yaml).expect("the file must load")
}

fn valid_fixture(name: &str) {
    let errors = validate_path(fixture_path(name)).unwrap_or_else(|err| panic!("{name}: {err}"));
    assert!(errors.is_empty(), "{name}: {}", describe(&errors));
}

#[test]
fn every_unified_fixture_passes_validation() {
    for name in [
        "v2/exit.yaml",
        "v2/set.yaml",
        "v2/scope.yaml",
        "v2/decision_human.yaml",
        "v2/decision_conductor.yaml",
    ] {
        valid_fixture(name);
    }
}

#[test]
fn exit_codes_beside_an_exit_are_rejected() {
    let errors = errors_of("exit_codes_with_exit.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::ExitCodesWithExit { state, .. } if state == "check"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn an_old_shape_choice_labelled_exit_is_an_ordinary_choice() {
    let errors = validate_path(fixture_path("legacy_exit_label.yaml")).expect("must load");

    assert!(errors.is_empty(), "got: {}", describe(&errors));
}

#[test]
fn set_of_a_variable_the_step_does_not_declare_is_rejected() {
    let errors = errors_of("set_unknown_variable.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::SetUnknownVariable { state, label, name }
                if state == "work" && label == "exit" && name == "sumary"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn set_local_of_a_global_variable_is_rejected() {
    let errors = errors_of("set_local_on_global_variable.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::SetScopeMismatch { state, name, .. }
                if state == "work" && name == "summary"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn a_local_variable_set_without_local_is_rejected() {
    let errors = unified_errors(
        "  work:\n    action: { type: agent, prompt: p }\n    variables:\n      \
         - { name: notes, scope: local }\n    exit: { set: { notes: x }, next: done }",
    );
    assert!(
        errors.iter().any(
            |err| matches!(err, ValidationError::SetScopeMismatch { name, .. } if name == "notes")
        ),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn set_of_a_value_outside_the_options_is_rejected() {
    let errors = errors_of("set_value_outside_options.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::SetValueRejected { name, .. } if name == "autopilot"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn set_of_a_value_of_the_wrong_type_is_rejected_but_a_template_is_not() {
    let work = |value: &str| {
        unified_errors(&format!(
            "  work:\n    action: {{ type: agent, prompt: p }}\n    variables:\n      \
             - {{ name: attempts, type: integer }}\n    exit: {{ set: {{ attempts: {value} }}, next: done }}"
        ))
    };
    assert!(work("\"many\"")
        .iter()
        .any(|err| matches!(err, ValidationError::SetValueRejected { .. })));
    assert!(work("3").is_empty());
    // Whether a rendered template fits is known only when it is rendered.
    let templated = unified_errors(
        "  work:\n    action: { type: agent, prompt: p }\n    variables:\n      \
         - { name: mode, options: [a, b] }\n    exit: { set: { mode: \"{{ inputs.x }}\" }, next: done }",
    );
    assert!(
        !templated
            .iter()
            .any(|err| matches!(err, ValidationError::SetValueRejected { .. })),
        "got: {}",
        describe(&templated)
    );
}

#[test]
fn one_variable_with_different_options_in_two_steps_is_rejected() {
    let errors = errors_of("inconsistent_variable_options.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::InconsistentVariable { name, state, other_state, .. }
                if name == "autopilot" && state == "first" && other_state == "second"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn a_local_variable_may_differ_between_steps() {
    let errors = unified_errors(
        "  first:\n    action: { type: agent, prompt: p }\n    variables:\n      \
         - { name: notes, scope: local, type: text }\n    exit: { next: second }\n  \
         second:\n    action: { type: agent, prompt: p }\n    variables:\n      \
         - { name: notes, scope: local, type: integer }\n    exit: { next: done }",
    );
    assert!(errors.is_empty(), "got: {}", describe(&errors));
}

#[test]
fn a_variable_declared_twice_in_a_step_is_rejected() {
    let errors = errors_of("duplicate_variable.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::DuplicateVariable { state, name }
                if state == "work" && name == "summary"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn type_and_options_of_a_declaration_have_to_agree() {
    for variable in [
        "{ name: v, type: select }",
        "{ name: v, type: integer, options: [a] }",
        "{ name: v, options: [a, a] }",
        "{ name: v, scope: local, source: workflow }",
    ] {
        let errors = unified_errors(&format!(
            "  work:\n    action: {{ type: agent, prompt: p }}\n    variables:\n      - {variable}\n    \
             exit: {{ next: done }}"
        ));
        assert!(
            errors
                .iter()
                .any(|err| matches!(err, ValidationError::InvalidVariableDeclaration { .. })),
            "{variable}: {}",
            describe(&errors)
        );
    }
}

#[test]
fn a_choice_cannot_require_a_variable_only_the_workflow_sets() {
    let errors = errors_of("required_workflow_variable.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::RequiredWorkflowVariable { state, name, .. }
                if state == "work" && name == "autopilot"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn resume_comment_field_has_to_be_a_text_variable_of_the_decision() {
    let errors = errors_of("resume_comment_field_not_text.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::InvalidChoiceBehavior { state, label, .. }
                if state == "approval" && label == "revise"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn an_old_shape_gate_may_still_have_a_label_named_timeout() {
    let yaml = r#"
schema: v2
name: old-shape-timeout-label
version: 1

states:
  gate:
    action:
      type: human
      message: Pick one.
    choices:
      - timeout
      - done
    transitions:
      timeout: done
      done: done
  done:
    description: End.

terminal_states:
  done: completed
"#;

    let errors = narsor_engine::workflow::validate_source(yaml).expect("the file must compile");
    assert!(
        !errors
            .iter()
            .any(|err| matches!(err, ValidationError::TimeoutChoiceNotAllowed { .. })),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn a_timeout_choice_is_only_for_agent_and_script_steps() {
    let errors = errors_of("timeout_choice_on_decision.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::TimeoutChoiceNotAllowed { state } if state == "approval"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn the_label_of_a_terminal_state_cannot_be_referenced() {
    let errors = errors_of("step_label_from_terminal_state.yaml");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ValidationError::InvalidStepLabelSource { state, step_id, .. }
                if state == "approval" && step_id == "done"
        )),
        "got: {}",
        describe(&errors)
    );
}

#[test]
fn a_decision_message_may_read_a_script_stream_and_a_step_label() {
    let errors = unified_errors(
        "  test:\n    action: { type: script, command: ./t.sh }\n    exit: { next: ask }\n  ask:\n    \
         action:\n      type: decision\n      by: conductor\n      message: \"{{ steps.test.stdout }} \
         {{ steps.test.label }}\"\n    choices:\n      go: { next: done }",
    );
    assert!(errors.is_empty(), "got: {}", describe(&errors));
}

#[test]
fn a_decision_needs_a_message_and_choices() {
    let errors =
        unified_errors("  ask:\n    action: { type: decision, by: human, message: \"  \" }");
    assert!(errors.iter().any(|err| matches!(
        err,
        ValidationError::MissingActionField {
            action_type: "decision",
            field: "message",
            ..
        }
    )));
    assert!(errors.iter().any(|err| matches!(
        err,
        ValidationError::EmptyChoices {
            action_type: "decision",
            ..
        }
    )));
}

#[test]
fn a_decision_defines_variables_but_no_artifacts() {
    let with_variables = unified_errors(
        "  ask:\n    action: { type: decision, by: human, message: m }\n    variables:\n      \
         - { name: comment, type: text }\n    choices:\n      go: { next: done }",
    );
    assert!(
        with_variables.is_empty(),
        "got: {}",
        describe(&with_variables)
    );

    let with_artifacts = unified_errors(
        "  ask:\n    action: { type: decision, by: human, message: m }\n    artifacts:\n      \
         - { name: doc, description: d }\n    choices:\n      go: { next: done }",
    );
    assert!(with_artifacts
        .iter()
        .any(|err| matches!(err, ValidationError::DefinitionsNotAllowed { .. })));
}

#[test]
fn a_local_variable_may_be_declared_on_a_script_step_through_its_scope() {
    let errors = unified_errors(
        "  check:\n    action: { type: script, command: ./c.sh }\n    variables:\n      \
         - { name: cursor, scope: local }\n    exit: { next: done }",
    );
    assert!(errors.is_empty(), "got: {}", describe(&errors));
}

#[test]
fn an_exit_to_an_unknown_state_is_rejected() {
    let errors = unified_errors(
        "  work:\n    action: { type: script, command: ./a.sh }\n    exit: { next: nowhere }",
    );
    assert!(errors.iter().any(|err| matches!(
        err,
        ValidationError::TransitionToUnknownState(state, label, target)
            if state == "work" && label == "exit" && target == "nowhere"
    )));
}

#[test]
fn a_loop_of_exits_without_a_way_out_is_a_deadlock() {
    let errors = unified_errors(
        "  first:\n    action: { type: script, command: ./a.sh }\n    exit: { next: second }\n  \
         second:\n    action: { type: script, command: ./a.sh }\n    exit: { next: first }",
    );
    assert!(errors
        .iter()
        .any(|err| matches!(err, ValidationError::NoPathToTerminal(state) if state == "first")));
}
