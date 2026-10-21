//! What a gate shows of the agent or script step before it (KTD11).
//!
//! The step's `reason` and the values its choice required. Both are the
//! step's own text, so they are cleaned of what a terminal must never be
//! handed and bounded before any record or event holds them.

use super::*;
use crate::engine::actions::human::{RequiredValue, RequiredValueKind};
use narsor_protocol::display::{without_unsafe_chars, without_unsafe_chars_multiline};

/// What a gate shows for a step that gave no reason.
pub(super) const NO_REASON: &str = "brak uzasadnienia";

/// The longest reason or required value a gate shows, in bytes.
const MAX_SHOWN_BYTES: usize = 2 * 1024;

/// What a gate entered right after `state` shows of `result`: its reason and
/// the values its choice required, or nothing when `state` is no agent or
/// script step.
pub(super) struct GateContext {
    pub(super) reason: Option<String>,
    pub(super) required_values: Vec<RequiredValue>,
    pub(super) answer_tail: Option<String>,
}

impl GateContext {
    /// The context of `result`, produced in `state`. A required `local.*`
    /// the result does not give is read from `kept_locals` (KTD10).
    pub(super) fn of(state: &State, result: &ActionResult, kept_locals: &Map<String, Value>) -> Self {
        if !has_step_contract(state) {
            return Self::none();
        }
        Self {
            reason: Some(gate_reason(&result.reason)),
            required_values: required_values(state, result, kept_locals),
            answer_tail: answer_tail(&result.stdout),
        }
    }

    /// No context: the step before the gate was no agent or script step.
    pub(super) fn none() -> Self {
        Self {
            reason: None,
            required_values: Vec::new(),
            answer_tail: None,
        }
    }
}

/// Whether `state` runs an action that answers with the step contract.
pub(super) fn has_step_contract(state: &State) -> bool {
    matches!(
        state.action,
        Some(ActionConfig::Agent(_)) | Some(ActionConfig::Script(_))
    )
}

/// `reason` as a gate shows it: cleaned and bounded, or the placeholder
/// when there is none.
pub(super) fn gate_reason(reason: &str) -> String {
    let cleaned = without_unsafe_chars(reason.trim());
    if cleaned.trim().is_empty() {
        NO_REASON.to_string()
    } else {
        head(&cleaned, MAX_SHOWN_BYTES)
    }
}

/// The end of a step's answer.
///
/// The answer is `stdout`, which keeps the head of an answer longer than the
/// capture limit; its tail is then the end of what was kept.
pub(super) fn answer_tail(stdout: &str) -> Option<String> {
    Some(harness::tail(stdout, ANSWER_TAIL_BYTES)).filter(|tail| !tail.is_empty())
}

/// The values the choice of `result` required that the step gave, in the
/// order the choice names them: its variables, then its artifacts.
fn required_values(
    state: &State,
    result: &ActionResult,
    kept_locals: &Map<String, Value>,
) -> Vec<RequiredValue> {
    let Some(choice) = state
        .choices
        .iter()
        .find(|choice| choice.label == result.output_label)
    else {
        return Vec::new();
    };
    let variables = choice.requires_variables.iter().filter_map(|name| {
        let value = result.variables.get(name).or_else(|| kept_locals.get(name))?;
        Some(shown(name, RequiredValueKind::Variable, &text_of(value)))
    });
    let artifacts = choice.requires_artifacts.iter().filter_map(|name| {
        let path = result.artifacts.get(name)?;
        Some(shown(name, RequiredValueKind::Artifact, path))
    });
    variables.chain(artifacts).collect()
}

/// A required value as a gate shows it.
fn shown(name: &str, kind: RequiredValueKind, value: &str) -> RequiredValue {
    RequiredValue {
        name: without_unsafe_chars(name),
        kind,
        value: head(&without_unsafe_chars_multiline(value), MAX_SHOWN_BYTES),
    }
}

/// A variable's value as text: a string as it is, anything else as JSON.
fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// At most `max` bytes of the start of `text`, on a character boundary, and
/// marked when cut.
fn head(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mark = '…';
    let mut end = max - mark.len_utf8();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{mark}", &text[..end])
}
