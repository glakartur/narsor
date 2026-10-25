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

fn limit() -> Value {
    json!({
        "execution_id": "exec-1",
        "state_id": "code_review",
        "action_execution_id": "act-limit",
        "instruction": "Krok osiągnął limit wejść.",
        "fields": [{
            "name": "entry_limit_decision",
            "type": "select",
            "options": ["continue", "reset_counter"],
            "required": false
        }],
        "outputs": [{ "label": "clean" }, { "label": "findings" }],
        "continuation": "entry_limit",
        "problem": { "reason": "entry limit reached (5/5)", "requirements": [] },
        "entry_limit": { "entries": 5, "max_entries": 5 },
        "episode": "ep-limit",
        "requested_at": "2026-10-18T12:00:00Z"
    })
}

#[test]
fn the_decision_at_the_limit_is_sent_as_its_field() {
    let panel = panel(limit());
    let all = labels(&panel);
    assert_eq!(all, ["clean", "findings", "Kontynuuj", "Wyzeruj licznik"]);
    let delivery = choose(&panel, 2, &[]);
    assert_eq!(
        delivery.values.get("entry_limit_decision"),
        Some(&Value::String("continue".to_string()))
    );
    let reset = choose(&panel, 3, &[]);
    assert_eq!(
        reset.values.get("entry_limit_decision"),
        Some(&Value::String("reset_counter".to_string()))
    );
}
