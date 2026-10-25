//! The form of an action of the workflow: a gate that completes a step and a
//! problem the owner settles by giving the output the agent did not give.

use std::collections::HashSet;

use serde_json::{Map, Value};

use super::{
    declared_fields, Builder, PanelContextLine, PanelEntry, PanelField, PanelPlan, MESSAGE_FIELD,
};
use crate::api::{
    HumanInputFieldView, HumanInputTypeView, OutputDefinitionView, PendingHumanInputView,
};

/// The name of the optional field that takes more `variables` than the label
/// needs, one `key=value` per line.
pub const EXTRA_VARIABLES_FIELD: &str = "variables (klucz=wartość)";

/// A problem's free-text comment, kept in the log with the answer.
pub const COMMENT_FIELD: &str = "komentarz";

/// The label of the entry that talks to the agent instead of settling a
/// problem.
pub const MESSAGE_ENTRY: &str = "Napisz do agenta";

impl Builder {
    pub(super) fn build_gate(
        &mut self,
        pending: &PendingHumanInputView,
        resumed_state: &dyn Fn(&str) -> Option<String>,
    ) {
        let fields = pending.fields();
        let outputs = &pending.outputs;
        let label_field = fields
            .iter()
            .find(|field| offers_exactly(field, outputs))
            .map(|field| field.name.clone());
        let others: Vec<&HumanInputFieldView> = fields
            .iter()
            .filter(|field| Some(&field.name) != label_field.as_ref())
            .collect();

        if outputs.is_empty() {
            let entry = declared_fields(
                PanelEntry::new("Odpowiedz", pending.instruction()),
                &others,
                outputs,
                None,
            );
            self.push(
                entry,
                PanelPlan::Complete {
                    label: None,
                    label_field,
                },
            );
            return;
        }
        if label_field.is_none() && infers_label(fields) {
            let entry = declared_fields(
                PanelEntry::new(
                    "wg wartości pola",
                    "Etykieta wyniku wynika z wybranej wartości pola.",
                ),
                &others,
                outputs,
                None,
            );
            self.push(
                entry,
                PanelPlan::Complete {
                    label: None,
                    label_field: None,
                },
            );
        }
        for output in outputs {
            let mut entry = PanelEntry::new(
                output.label.clone(),
                output.description.clone().unwrap_or_default(),
            );
            if output.resume_comment_field.is_some() {
                let attempt = pending
                    .resume_targets
                    .get(&output.label)
                    .map(|target| target.action_execution_id.clone());
                let state = attempt
                    .as_deref()
                    .and_then(resumed_state)
                    .unwrap_or_default();
                entry = entry.resuming(state);
                entry.resumes_attempt = attempt;
            }
            let entry = declared_fields(entry, &others, outputs, Some(&output.label));
            self.push(
                entry,
                PanelPlan::Complete {
                    label: Some(output.label.clone()),
                    label_field: label_field.clone(),
                },
            );
        }
    }

    pub(super) fn build_problem(&mut self, pending: &PendingHumanInputView) {
        if pending.problem.is_none() {
            return;
        }
        self.build_problem_outputs(pending);
        let message = PanelEntry::new(
            MESSAGE_ENTRY,
            "Wiadomość wznawia ten sam krok agenta; wynik ustawi on sam.",
        )
        .resuming(pending.state_id.to_string())
        .with_field(PanelField::multiline(MESSAGE_FIELD).required());
        self.push(message, PanelPlan::Message);
    }

    /// The problem's context and one entry per output label, without the
    /// message entry.
    pub(super) fn build_problem_outputs(&mut self, pending: &PendingHumanInputView) {
        let Some(problem) = pending.problem.as_ref() else {
            return;
        };
        self.context
            .push(PanelContextLine::problem(problem.reason.clone()));
        for answer in &problem.answers {
            let mut text = format!("odrzucona odpowiedź agenta: {}", answer.reason);
            if let Some(tail) = &answer.answer_tail {
                text.push_str(&format!(" …{tail}"));
            }
            self.context.push(PanelContextLine::dim(text));
        }
        let analysis = problem.analysis.clone().unwrap_or_default();
        if let Some(rationale) = &analysis.rationale {
            self.context.push(PanelContextLine::conductor(format!(
                "◆ dyrygent: {rationale}"
            )));
        }
        let proposed_artifacts: Map<String, Value> = analysis
            .artifacts
            .iter()
            .map(|(name, path)| (name.clone(), Value::String(path.clone())))
            .collect();
        for output in &pending.outputs {
            let proposed = analysis.label.as_deref() == Some(output.label.as_str());
            let mut entry = PanelEntry::new(
                output.label.clone(),
                output.description.clone().unwrap_or_default(),
            );
            let requirement = problem.requirement_for(&output.label);
            let required_artifacts: &[String] =
                requirement.map_or(&[], |need| need.artifacts.as_slice());
            let required_variables: &[String] =
                requirement.map_or(&[], |need| need.variables.as_slice());
            for (map, prefix, required) in [
                (&proposed_artifacts, "artifacts", required_artifacts),
                (&analysis.variables, "variables", required_variables),
            ] {
                let mut keys: Vec<&String> = required.iter().collect();
                if proposed {
                    keys.extend(map.keys().filter(|key| !required.contains(key)));
                }
                for key in keys {
                    let name = format!("{prefix}.{key}");
                    let mut field = PanelField::multiline(name);
                    if prefix == "artifacts" && required.contains(key) {
                        field = field
                            .required()
                            .described("wymagane przez stan, do którego prowadzi etykieta");
                    } else if required.contains(key) {
                        field = field.described("wymagane, chyba że kontekst już ma wartość");
                    }
                    if let Some(value) = proposed.then(|| map.get(key)).flatten() {
                        field = field.proposing(value.clone());
                    }
                    entry = entry.with_field(field);
                }
            }
            entry = entry
                .with_field(PanelField::multiline(EXTRA_VARIABLES_FIELD))
                .with_field(PanelField::multiline(COMMENT_FIELD));
            if proposed {
                entry = entry.recommended();
            }
            self.push(
                entry,
                PanelPlan::Output {
                    label: output.label.clone(),
                },
            );
        }
    }
}

/// Whether a single `select` field lets the engine infer the output label.
fn infers_label(fields: &[HumanInputFieldView]) -> bool {
    fields
        .iter()
        .filter(|field| field.field_type == HumanInputTypeView::Select)
        .count()
        == 1
}

/// Whether `field` is a `select` whose options are exactly `outputs`, in any
/// order (KTD17).
fn offers_exactly(field: &HumanInputFieldView, outputs: &[OutputDefinitionView]) -> bool {
    if field.field_type != HumanInputTypeView::Select
        || outputs.is_empty()
        || field.options.len() != outputs.len()
    {
        return false;
    }
    let options: HashSet<&str> = field.options.iter().map(String::as_str).collect();
    options.len() == outputs.len()
        && outputs
            .iter()
            .all(|output| options.contains(output.label.as_str()))
}
