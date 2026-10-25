use serde_json::{json, Value};

use crate::tui::chooser::Submission;
use crate::tui::task_app::panel::input::InputPanel;
use narsor_protocol::api::PendingHumanInputView;

fn panel(form: Value) -> InputPanel {
    let pending: PendingHumanInputView = serde_json::from_value(form).expect("a pending input");
    InputPanel::new(&pending, &|_| None)
}

fn labels(panel: &InputPanel) -> Vec<String> {
    panel
        .items()
        .iter()
        .map(|item| item.label.clone())
        .collect()
}

fn choose(
    panel: &InputPanel,
    index: usize,
    values: &[(&str, &str)],
) -> narsor_protocol::api::HumanInputDeliveryDto {
    panel
        .delivery(&Submission {
            index,
            label: panel.items()[index].label.clone(),
            values: values
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
        })
        .expect("a delivery")
}

fn request(command: &str) -> Value {
    json!({
        "execution_id": "exec-1",
        "state_id": "implement",
        "action_execution_id": "act-8",
        "instruction": "Claude Code needs permission.",
        "fields": [],
        "continuation": "resolve_permission",
        "permission": { "denials": [{
            "tool_name": "Bash",
            "tool_use_id": "t1",
            "input": { "command": command },
            "cwd": "/tmp/task",
            "reason_type": "asyncAgent",
            "reason": "This command requires approval."
        }] },
        "agent_harness": "claude-code",
        "agent_session_id": "s1",
        "episode": "ep-8",
        "requested_at": "2026-10-09T11:54:00Z"
    })
}

#[test]
fn a_resolvable_bash_denial_offers_scoped_allows_and_a_refusal() {
    let panel = panel(request("cargo test"));
    let all = labels(&panel);
    assert_eq!(all[0], "Odmów");
    assert!(all.len() >= 2, "{all:?}");
    let refusal = choose(&panel, 0, &[]);
    let decision = refusal.permission_decision.expect("a decision");
    assert_eq!(
        decision.decision,
        narsor_protocol::api::PermissionDecisionKindDto::Deny
    );
}

#[test]
fn a_pattern_without_a_rule_is_refused_at_the_delivery() {
    let panel = panel(request("cargo test"));
    let pattern = (0..panel.items().len()).find(|&index| {
        panel
            .delivery(&Submission {
                index,
                label: String::new(),
                values: vec![("Wzorzec polecenia".to_string(), String::new())],
            })
            .is_err()
    });
    assert!(pattern.is_some(), "an empty pattern is refused");
}
