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

fn gate() -> Value {
    json!({
        "execution_id": "exec-1",
        "state_id": "review_scope",
        "action_execution_id": "act-7",
        "instruction": "Przejrzyj pracę.",
        "fields": [],
        "outputs": [{ "label": "approve" }, { "label": "reject" }],
        "episode": "ep-g",
        "requested_at": "2026-10-09T11:54:00Z"
    })
}

fn problem() -> Value {
    json!({
        "execution_id": "exec-1",
        "state_id": "implement",
        "action_execution_id": "act-9",
        "instruction": "Krok nie dał etykiety",
        "outputs": [{ "label": "implemented" }, { "label": "needs_human_decision" }],
        "continuation": "resolve_output",
        "problem": {
            "reason": "Odpowiedź agenta nie zawiera etykiety.",
            "answers": [],
            "requirements": [
                { "label": "implemented", "variables": [], "artifacts": [] },
                { "label": "needs_human_decision", "variables": [], "artifacts": [] }
            ]
        },
        "episode": "ep-p",
        "requested_at": "2026-10-09T11:54:00Z"
    })
}

#[test]
fn a_gate_offers_its_outputs_and_completes_the_step_with_the_chosen_one() {
    let panel = panel(gate());
    assert_eq!(labels(&panel), ["approve", "reject"]);
    let delivery = choose(&panel, 1, &[]);
    assert_eq!(delivery.chosen_label.as_deref(), Some("reject"));
    assert_eq!(delivery.episode.as_deref(), Some("ep-g"));
}

#[test]
fn a_problem_offers_its_outputs_and_a_message_to_the_agent() {
    let panel = panel(problem());
    let labels = labels(&panel);
    assert!(labels.contains(&"implemented".to_string()), "{labels:?}");
    assert!(
        labels.contains(&"needs_human_decision".to_string()),
        "{labels:?}"
    );
    let delivery = choose(&panel, 0, &[]);
    assert_eq!(delivery.chosen_label.as_deref(), Some("implemented"));
    let position = labels
        .iter()
        .position(|label| label == "needs_human_decision")
        .expect("the second output is offered");
    let delivery = choose(&panel, position, &[]);
    assert_eq!(
        delivery.chosen_label.as_deref(),
        Some("needs_human_decision")
    );
    assert_eq!(delivery.episode.as_deref(), Some("ep-p"));
}
