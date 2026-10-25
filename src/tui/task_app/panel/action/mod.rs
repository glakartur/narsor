//! The form of an action of the workflow: a gate that completes a step and a
//! problem the owner settles by giving the output the agent did not give.

#[cfg(test)]
mod tests;

use super::input::*;
use crate::tui::chooser::{ChooserField, ChooserItem, Submission};
use crate::tui::input_form::value_as_text;
use narsor_protocol::api::*;
use serde_json::{Map, Value};
use std::collections::HashSet;

/// The name of the optional field that takes more `variables` than the label
/// needs, one `key=value` per line.
pub const EXTRA_VARIABLES_FIELD: &str = "variables (klucz=wartość)";

/// A problem's free-text comment, kept in the log with the answer.
pub const COMMENT_FIELD: &str = "komentarz";

/// The label of the entry that talks to the agent instead of settling a
/// problem.
pub const MESSAGE_ENTRY: &str = "Napisz do agenta";

impl InputPanel {
    pub(in crate::tui::task_app::panel) fn build_gate(
        &mut self,
        pending: &PendingHumanInputView,
        resumed_state: &dyn Fn(&str) -> Option<String>,
    ) {
        let fields = pending.fields();
        let outputs = &pending.outputs;
        self.label_field = fields
            .iter()
            .find(|field| offers_exactly(field, outputs))
            .map(|field| field.name.clone());
        let others: Vec<&HumanInputFieldView> = fields
            .iter()
            .filter(|field| Some(&field.name) != self.label_field.as_ref())
            .collect();

        if outputs.is_empty() {
            let item = declared_fields(
                ChooserItem::new("Odpowiedz", pending.instruction()),
                &others,
                outputs,
                None,
            );
            self.push(item, Plan::Complete(None));
            return;
        }
        if self.label_field.is_none() && infers_label(fields) {
            let item = declared_fields(
                ChooserItem::new(
                    "wg wartości pola",
                    "Etykieta wyniku wynika z wybranej wartości pola.",
                ),
                &others,
                outputs,
                None,
            );
            self.push(item, Plan::Complete(None));
        }
        for output in outputs {
            let mut item = ChooserItem::new(
                output.label.clone(),
                output.description.clone().unwrap_or_default(),
            );
            if output.resume_comment_field.is_some() {
                let state = pending
                    .resume_targets
                    .get(&output.label)
                    .and_then(|target| resumed_state(&target.action_execution_id))
                    .unwrap_or_default();
                item = item.resuming(state);
            }
            let item = declared_fields(item, &others, outputs, Some(&output.label));
            self.push(item, Plan::Complete(Some(output.label.clone())));
        }
        // Reversible first: an entry that returns the work to the agent.
        self.selected = self
            .items
            .iter()
            .position(|item| item.resumes_agent.is_some())
            .unwrap_or(0);
    }

    pub(in crate::tui::task_app::panel) fn complete(
        &self,
        label: Option<&str>,
        submission: &Submission,
    ) -> HumanInputDeliveryDto {
        let mut values = Map::new();
        for (name, value) in &submission.values {
            values.insert(name.clone(), Value::String(value.clone()));
        }
        if let (Some(field), Some(label)) = (&self.label_field, label) {
            values.insert(field.clone(), Value::String(label.to_string()));
        }
        HumanInputDeliveryDto {
            values,
            chosen_label: label.map(str::to_string),
            ..HumanInputDeliveryDto::default()
        }
    }

    pub(in crate::tui::task_app::panel) fn build_problem(
        &mut self,
        pending: &PendingHumanInputView,
    ) {
        if pending.problem.is_none() {
            return;
        }
        let recommended = self.build_problem_outputs(pending);
        let message = ChooserItem::new(
            MESSAGE_ENTRY,
            "Wiadomość wznawia ten sam krok agenta; wynik ustawi on sam.",
        )
        .resuming(pending.state_id.to_string())
        .with_field(ChooserField::multiline(MESSAGE_FIELD).required());
        self.push(message, Plan::Message);
        // The conductor's label, or — without one — talking to the agent.
        self.selected = recommended.unwrap_or(self.items.len() - 1);
    }

    /// The problem's context and one item per output label, without the
    /// message entry; returns the index of the recommended item.
    pub(in crate::tui::task_app::panel) fn build_problem_outputs(
        &mut self,
        pending: &PendingHumanInputView,
    ) -> Option<usize> {
        let problem = pending.problem.as_ref()?;
        self.context
            .push(ContextLine::problem(problem.reason.clone()));
        for answer in &problem.answers {
            let mut text = format!("odrzucona odpowiedź agenta: {}", answer.reason);
            if let Some(tail) = &answer.answer_tail {
                text.push_str(&format!(" …{tail}"));
            }
            self.context.push(ContextLine::dim(text));
        }
        let analysis = problem.analysis.clone().unwrap_or_default();
        if let Some(rationale) = &analysis.rationale {
            self.context
                .push(ContextLine::conductor(format!("◆ dyrygent: {rationale}")));
        }
        let mut recommended = None;
        for output in &pending.outputs {
            let proposed = analysis.label.as_deref() == Some(output.label.as_str());
            let index = self.items.len();
            let mut item = ChooserItem::new(
                output.label.clone(),
                output.description.clone().unwrap_or_default(),
            );
            let requirement = problem.requirement_for(&output.label);
            let required_artifacts: &[String] =
                requirement.map_or(&[], |need| need.artifacts.as_slice());
            let required_variables: &[String] =
                requirement.map_or(&[], |need| need.variables.as_slice());
            let proposed_artifacts: Map<String, Value> = analysis
                .artifacts
                .iter()
                .map(|(name, path)| (name.clone(), Value::String(path.clone())))
                .collect();
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
                    let mut field = ChooserField::multiline(name.clone());
                    if prefix == "artifacts" && required.contains(key) {
                        field = field
                            .required()
                            .described("wymagane przez stan, do którego prowadzi etykieta");
                    } else if required.contains(key) {
                        field = field.described("wymagane, chyba że kontekst już ma wartość");
                    }
                    if let Some(value) = proposed.then(|| map.get(key)).flatten() {
                        field = field.proposed(value_as_text(value));
                        self.originals.push((index, name, value.clone()));
                    }
                    item = item.with_field(field);
                }
            }
            item = item
                .with_field(ChooserField::multiline(EXTRA_VARIABLES_FIELD))
                .with_field(ChooserField::multiline(COMMENT_FIELD));
            if proposed {
                item = item.recommended();
                recommended = Some(index);
            }
            self.push(item, Plan::Output(output.label.clone()));
        }
        recommended
    }

    pub(in crate::tui::task_app::panel) fn output(
        &self,
        label: &str,
        submission: &Submission,
    ) -> Result<HumanInputDeliveryDto, String> {
        let mut maps = [Map::new(), Map::new()];
        for (name, text) in &submission.values {
            for (slot, prefix) in ["artifacts.", "variables."].into_iter().enumerate() {
                if let Some(key) = name.strip_prefix(prefix) {
                    let value = self.delivered_value(submission.index, name, text);
                    insert_once(&mut maps[slot], &prefix[..prefix.len() - 1], key, value)?;
                }
            }
        }
        let [artifacts, mut variables] = maps;
        for line in Self::value_of(submission, EXTRA_VARIABLES_FIELD)
            .unwrap_or_default()
            .lines()
        {
            if line.trim().is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(format!(
                    "Linia '{line}' w variables nie ma postaci klucz=wartość."
                ));
            };
            if key.trim().is_empty() {
                return Err(format!("Linia '{line}' w variables nie ma klucza."));
            }
            insert_once(
                &mut variables,
                "variables",
                key.trim(),
                Value::String(value.trim().to_string()),
            )?;
        }
        Ok(HumanInputDeliveryDto {
            chosen_label: Some(label.to_string()),
            artifacts: artifacts
                .into_iter()
                .map(|(name, path)| (name, value_as_text(&path)))
                .collect(),
            variables,
            comment: Self::value_of(submission, COMMENT_FIELD)
                .map(str::trim)
                .map(str::to_string),
            ..HumanInputDeliveryDto::default()
        })
    }

    /// The proposal itself while its text is untouched, the typed text
    /// otherwise.
    pub(in crate::tui::task_app::panel) fn delivered_value(
        &self,
        index: usize,
        name: &str,
        text: &str,
    ) -> Value {
        match self
            .originals
            .iter()
            .find(|(item, field, _)| *item == index && field == name)
        {
            Some((_, _, original)) if value_as_text(original) == text => original.clone(),
            _ => Value::String(text.to_string()),
        }
    }
}

/// Puts `value` under `key` in `map`, refusing a second one.
fn insert_once(
    map: &mut Map<String, Value>,
    name: &str,
    key: &str,
    value: Value,
) -> Result<(), String> {
    if map.insert(key.to_string(), value).is_some() {
        return Err(format!("Klucz '{name}.{key}' jest podany dwa razy."));
    }
    Ok(())
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
