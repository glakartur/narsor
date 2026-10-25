//! The form of an answer to the agent: its own question, and the approval of
//! its result.

use super::{
    declared_fields, value_as_text, Builder, PanelContextLine, PanelEntry, PanelField, PanelPlan,
    COMMENT_TO_AGENT,
};
use crate::api::{HumanInputFieldView, PendingHumanInputView};
use crate::display::{without_unsafe_chars, NO_REASON};

impl Builder {
    pub(super) fn build_question(&mut self, pending: &PendingHumanInputView) {
        let fields: Vec<&HumanInputFieldView> = pending.fields().iter().collect();
        let mut entry = declared_fields(
            PanelEntry::new("Odpowiedz agentowi", "Odpowiedź wznawia sesję agenta.")
                .resuming(pending.state_id.to_string()),
            &fields,
            &[],
            None,
        );
        if let Some(answer) = pending.prefilled_answer.as_deref() {
            // The text of an answer a refused resume gave back (2a9c6f2).
            for field in entry
                .fields
                .iter_mut()
                .filter(|field| field.name == "answer")
            {
                field.value = Some(answer.to_string());
            }
        }
        self.push(entry, PanelPlan::Answer);
    }

    pub(super) fn build_approval(&mut self, pending: &PendingHumanInputView) {
        let mut resumable = false;
        if let Some(approval) = &pending.approval {
            let result = &approval.result;
            let label = result.output_label.clone();
            self.context.push(PanelContextLine::plain(format!(
                "wybór agenta: {}",
                without_unsafe_chars(&label)
            )));
            if let Some(description) = pending
                .outputs
                .iter()
                .find(|output| output.label == label)
                .and_then(|output| output.description.clone())
            {
                self.context.push(PanelContextLine::dim(description));
            }
            let reason = result.reason.trim();
            self.push_reason(if reason.is_empty() { NO_REASON } else { reason });
            for (name, value) in &result.variables {
                self.push_named_value(name, &value_as_text(value));
            }
            for (name, path) in &result.artifacts {
                self.push_named_value(name, path);
            }
            resumable = approval.agent_session_id.is_some();
            self.approval_label = Some(label);
        }
        self.push(
            PanelEntry::new("Akceptuj", "Przyjmuje wynik agenta; zadanie idzie dalej."),
            PanelPlan::Approve,
        );
        let (label, description) = if resumable {
            (
                "Zwróć agentowi",
                "Agent wznowi swoją sesję z Twoim komentarzem i przygotuje nowy wynik.",
            )
        } else {
            (
                "Nowa sesja",
                "Sesji agenta nie da się wznowić: komentarz zaczyna nową sesję i nowy wynik.",
            )
        };
        self.push(
            PanelEntry::new(label, description)
                .resuming(pending.state_id.to_string())
                .with_field(PanelField::multiline(COMMENT_TO_AGENT).required()),
            PanelPlan::Return,
        );
    }
}
