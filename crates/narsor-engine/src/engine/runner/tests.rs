use super::*;

use crate::domain::TerminalStatus;

fn terminal(id: &str, declared: Option<TerminalStatus>) -> State {
    State {
        id: id.to_string(),
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
        terminal_status: declared,
        allow_manual_override: false,
        workspace: None,
        image: None,
    }
}

#[test]
fn a_terminal_state_ends_the_execution_as_it_declares() {
    assert_eq!(
        terminal_status(&terminal("completed", Some(TerminalStatus::Completed))),
        ExecutionStatus::Completed
    );
    assert_eq!(
        terminal_status(&terminal("failed", Some(TerminalStatus::Failed))),
        ExecutionStatus::Failed
    );
    assert_eq!(
        terminal_status(&terminal("gave_up", Some(TerminalStatus::Cancelled))),
        ExecutionStatus::Cancelled
    );
}

#[test]
fn an_undeclared_terminal_completes_whatever_it_is_called() {
    // The id is never consulted — this is exactly the guessing that was
    // removed, and `failed` here is the deliberate consequence of it.
    assert_eq!(
        terminal_status(&terminal("done", None)),
        ExecutionStatus::Completed
    );
    assert_eq!(
        terminal_status(&terminal("failed", None)),
        ExecutionStatus::Completed
    );
    assert_eq!(
        terminal_status(&terminal("rejected_by_reviewer", None)),
        ExecutionStatus::Completed
    );
}

/// Every action kind, with what the runner checked before it asked the
/// kind's capabilities: `kind == Agent`, `is_agent`, `has_step_contract`
/// and `holds_variables_to_declarations` as they were.
const KINDS: [ActionKind; 6] = [
    ActionKind::Agent,
    ActionKind::Script,
    ActionKind::Human,
    ActionKind::Wait,
    ActionKind::Decision,
    ActionKind::Profile,
];

#[test]
fn only_an_agent_and_a_profile_step_run_on_a_harness() {
    for kind in KINDS {
        assert_eq!(
            kind.runs_harness(),
            matches!(kind, ActionKind::Agent | ActionKind::Profile),
            "{kind:?}"
        );
    }
}

#[test]
fn only_an_agent_and_a_profile_step_are_asked_again_for_a_rejected_output() {
    for kind in KINDS {
        assert_eq!(
            kind.reprompts_rejected_output(),
            matches!(kind, ActionKind::Agent | ActionKind::Profile),
            "{kind:?}"
        );
    }
    assert_eq!(
        ActionKind::Profile.reprompt_text(),
        Some(profile_action::PROFILE_REPROMPT)
    );
}

#[test]
fn only_an_agent_has_its_label_rescued_by_the_conductor() {
    for kind in KINDS {
        assert_eq!(
            kind.rescues_output_label(),
            kind == ActionKind::Agent,
            "{kind:?}"
        );
    }
}

#[test]
fn only_a_profile_step_retries_an_unrepaired_output() {
    for kind in KINDS {
        assert_eq!(
            kind.retries_unrepaired_output(),
            kind == ActionKind::Profile,
            "{kind:?}"
        );
    }
}

/// A `profile` step runs on a harness like an agent and is asked again
/// for an unusable answer, but answers with no step contract: no label
/// to rescue, no variables to hold to declarations.
#[test]
fn a_profile_step_runs_a_harness_without_a_step_contract() {
    let profile = ActionKind::Profile;
    assert_eq!(profile.name(), "profile");
    assert!(profile.runs_harness());
    assert!(profile.reprompts_rejected_output());
    assert!(!profile.rescues_output_label());
    assert!(!profile.answers_with_step_contract());
    assert!(!profile.holds_variables_to_declarations());
}

#[test]
fn a_profile_action_is_of_the_profile_kind() {
    let action: ActionConfig =
        serde_json::from_value(json!({ "type": "profile", "prompt": "judge" }))
            .expect("a profile action reads");
    assert_eq!(ActionKind::of(&action), ActionKind::Profile);
}

#[test]
fn an_agent_and_a_script_answer_with_the_step_contract() {
    for kind in KINDS {
        assert_eq!(
            kind.answers_with_step_contract(),
            matches!(kind, ActionKind::Agent | ActionKind::Script),
            "{kind:?}"
        );
    }
}

#[test]
fn a_decision_is_held_to_its_declarations_too() {
    for kind in KINDS {
        assert_eq!(
            kind.holds_variables_to_declarations(),
            matches!(
                kind,
                ActionKind::Agent | ActionKind::Script | ActionKind::Decision
            ),
            "{kind:?}"
        );
    }
}
