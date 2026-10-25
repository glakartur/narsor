//! The form of an answer to the agent: its own question, and the approval of
//! its result.

use super::input::*;
use crate::domain::display::without_unsafe_chars;
use crate::tui::chooser::{ChooserField, ChooserItem, Submission};
use crate::tui::input_form::value_as_text;
use narsor_protocol::api::*;
use narsor_protocol::display::NO_REASON;
use serde_json::Value;

impl InputPanel {
    pub(in crate::tui::task_app::panel) fn build_question(
        &mut self,
        pending: &PendingHumanInputView,
    ) {
        let fields: Vec<&HumanInputFieldView> = pending.fields().iter().collect();
        let mut item = declared_fields(
            ChooserItem::new("Odpowiedz agentowi", "Odpowiedź wznawia sesję agenta.")
                .resuming(pending.state_id.to_string()),
            &fields,
            &[],
            None,
        );
        if let Some(answer) = pending.prefilled_answer.as_deref() {
            // The text of an answer a refused resume gave back (2a9c6f2).
            for field in item
                .fields
                .iter_mut()
                .filter(|field| field.name == "answer")
            {
                *field = field.clone().with_value(answer);
            }
        }
        self.push(item, Plan::Answer);
    }

    pub(in crate::tui::task_app::panel) fn answer(
        &self,
        submission: &Submission,
    ) -> Result<HumanInputDeliveryDto, String> {
        let filled = |name: &str| Self::value_of(submission, name).is_some();
        if !filled("answer") && !filled("option") {
            return Err("Wpisz odpowiedź albo wybierz opcję.".to_string());
        }
        let values = submission
            .values
            .iter()
            .map(|(name, value)| (name.clone(), Value::String(value.clone())))
            .collect();
        Ok(HumanInputDeliveryDto {
            values,
            ..HumanInputDeliveryDto::default()
        })
    }

    pub(in crate::tui::task_app::panel) fn build_approval(
        &mut self,
        pending: &PendingHumanInputView,
    ) {
        let mut resumable = false;
        if let Some(approval) = &pending.approval {
            let result = &approval.result;
            let label = result.output_label.clone();
            self.context.push(ContextLine::plain(format!(
                "wybór agenta: {}",
                without_unsafe_chars(&label)
            )));
            if let Some(description) = pending
                .outputs
                .iter()
                .find(|output| output.label == label)
                .and_then(|output| output.description.clone())
            {
                self.context.push(ContextLine::dim(description));
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
            ChooserItem::new("Akceptuj", "Przyjmuje wynik agenta; zadanie idzie dalej."),
            Plan::Approve,
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
            ChooserItem::new(label, description)
                .resuming(pending.state_id.to_string())
                .with_field(ChooserField::multiline(COMMENT_TO_AGENT).required()),
            Plan::Return,
        );
        // Going back is the safe default: accepting moves the work on.
        self.selected = 1;
    }
}
