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

fn question() -> Value {
    json!({
        "execution_id": "exec-1",
        "state_id": "review_scope",
        "action_execution_id": "act-7",
        "instruction": "Która wersja API?",
        "fields": [{ "name": "answer", "type": "text", "required": true }],
        "continuation": "resume_agent",
        "episode": "ep-q",
        "requested_at": "2026-10-09T11:54:00Z"
    })
}

fn approval() -> Value {
    json!({
        "execution_id": "exec-1",
        "state_id": "review_scope",
        "action_execution_id": "act-7",
        "instruction": "Zatwierdź wynik",
        "outputs": [{ "label": "reviewed" }],
        "continuation": "approve_output",
        "approval": {
            "action_execution_id": "act-7",
            "result": { "output_label": "reviewed", "data": {}, "variables": {}, "exit_code": null, "stdout": "", "stderr": "" },
            "agent_session_id": "sess-1"
        },
        "episode": "ep-a",
        "requested_at": "2026-10-09T11:54:00Z"
    })
}

#[test]
fn a_question_is_answered_with_the_typed_text() {
    let panel = panel(question());
    let delivery = choose(&panel, 0, &[("answer", "v2")]);
    assert_eq!(delivery.episode.as_deref(), Some("ep-q"));
    assert!(
        delivery
            .values
            .get("answer")
            .is_some_and(|value| value == "v2"),
        "{delivery:?}"
    );
}

#[test]
fn an_approval_is_accepted_or_returned_to_the_agent() {
    let panel = panel(approval());
    assert_eq!(labels(&panel).len(), 2);
    let accepted = choose(&panel, 0, &[]);
    assert_eq!(
        accepted.action,
        Some(narsor_protocol::api::HumanInputActionDto::Approve)
    );
    let returned = choose(&panel, 1, &[("Komentarz dla agenta", "popraw")]);
    assert_eq!(
        returned.action,
        Some(narsor_protocol::api::HumanInputActionDto::Resume)
    );
    assert_eq!(returned.comment.as_deref(), Some("popraw"));
}
