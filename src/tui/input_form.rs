//! The text helpers of the forms and the inputs a workflow declares (the
//! new-task form), and the reason a request stopped waiting.
//!
//! What the answer panel offers for a pending human input moved to
//! [`crate::tui::task_app::panel`], one directory for each kind of form; its
//! names are given again here, so a path through `input_form` still leads to
//! them.

use std::iter::Peekable;
use std::str::Chars;

use serde_json::{Map, Value};

use crate::domain::display::is_invisible_format;
use crate::tui::chooser::{ChooserField, FieldKind};
use narsor_protocol::controller::{WorkflowInputType, WorkflowInputView};
use narsor_protocol::event::{AnsweredBy, EventData, InputCloseReason};

pub use crate::tui::task_app::panel::action::{
    COMMENT_FIELD, EXTRA_VARIABLES_FIELD, MESSAGE_ENTRY,
};
pub use crate::tui::task_app::panel::input::{
    ContextLine, InputKind, InputPanel, Tone, COMMENT_TO_AGENT, MESSAGE_FIELD,
};
pub use crate::tui::task_app::panel::permission::PATTERN_FIELD;

/// How the request an open form answers stopped waiting — someone else's
/// answer, a transition, an interrupt, a recovery (R5, AE13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputClosure {
    /// Who closed it, when the log says.
    pub answered_by: Option<AnsweredBy>,
    /// How it was closed, when the log says.
    pub reason: Option<InputCloseReason>,
    /// The answer that closed it, summarised on one line.
    pub answer: Option<String>,
}

impl InputClosure {
    /// A closure nothing explains: the request is simply no longer pending.
    pub fn unexplained() -> Self {
        Self {
            answered_by: None,
            reason: None,
            answer: None,
        }
    }

    /// The closure a `HumanInputReceived` records, with the episode it
    /// closed; `None` for any other event.
    pub fn from_event(data: &EventData) -> Option<(Option<&str>, Self)> {
        let EventData::HumanInputReceived {
            input,
            answered_by,
            close_reason,
            episode,
            output_label,
            artifacts,
            variables,
            comment,
        } = data
        else {
            return None;
        };
        let mut parts = Vec::new();
        if let Some(label) = output_label {
            parts.push(format!("'{label}'"));
        }
        for values in [Some(input), artifacts.as_ref(), variables.as_ref()]
            .into_iter()
            .flatten()
        {
            if let Some(map) = values.as_object() {
                parts.extend(
                    map.iter()
                        .map(|(key, value)| format!("{key}={}", value_as_text(value))),
                );
            }
        }
        if let Some(comment) = comment.as_deref().filter(|comment| !comment.is_empty()) {
            parts.push(format!("comment: {comment}"));
        }
        let closure = Self {
            answered_by: *answered_by,
            reason: close_reason.or(Some(InputCloseReason::Answered)),
            answer: (!parts.is_empty()).then(|| parts.join(", ")),
        };
        Some((episode.as_deref(), closure))
    }

    /// What the form says about it.
    pub fn message(&self) -> String {
        let by = self.answered_by.map(|by| match by {
            AnsweredBy::Operator => "an operator",
            AnsweredBy::Conductor => "the conductor",
        });
        match self.reason {
            Some(InputCloseReason::Transition) => match by {
                Some(by) => format!("Closed by a transition ({by})."),
                None => "Closed by a transition.".to_string(),
            },
            Some(InputCloseReason::Recover) => {
                "Closed: the step was recovered and starts afresh.".to_string()
            }
            Some(InputCloseReason::Cancel) => "Closed: the execution was cancelled.".to_string(),
            Some(InputCloseReason::Undo) => {
                "Closed: a decision of the conductor's was undone.".to_string()
            }
            Some(InputCloseReason::Answered) => {
                let by = by.unwrap_or("someone else");
                match &self.answer {
                    Some(answer) => format!("Already answered by {by}: {answer}"),
                    None => format!("Already answered by {by}."),
                }
            }
            None => "This input is no longer pending: it was closed elsewhere.".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// What a field takes from a paste or a key, besides the text buffer
// ---------------------------------------------------------------------------
//
// Editing of the text itself is `super::form::buffer::TextBuffer`; here are
// the rules about what a paste may carry and how a `select` steps.

/// What a paste puts into a field of `kind`: nothing for a `select`, and for
/// the others only what could have been typed — no escape sequence, no
/// control character but `\n` and `\t`, no invisible format character the
/// field would not show, every line break one `\n`. A one-line field gets a
/// space for each line break, a number field only its digits and `-`.
pub fn pasted_text(kind: &FieldKind, pasted: &str) -> Option<String> {
    if matches!(kind, FieldKind::Select(_)) {
        return None;
    }
    let mut text = String::with_capacity(pasted.len());
    let mut chars = pasted.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\u{1b}' => skip_pasted_escape(&mut chars),
            '\r' => {
                chars.next_if_eq(&'\n');
                text.push('\n');
            }
            // NEL, the line and the paragraph separator break a line too.
            '\n' | '\u{85}' | '\u{2028}' | '\u{2029}' => text.push('\n'),
            '\t' => text.push(character),
            c if c.is_control() => {}
            // The field does not draw these (see `scroll::sanitize`), so the
            // value would differ from what the operator sees and sends.
            c if is_invisible_format(c) || is_unicode_tag(c) => {}
            c => text.push(c),
        }
    }
    Some(match kind {
        FieldKind::Text { multiline: true } => text,
        FieldKind::Text { multiline: false } => text.replace('\n', " "),
        _ => text
            .chars()
            .filter(|character| character.is_ascii_digit() || *character == '-')
            .collect(),
    })
}

/// Skips the escape sequence a pasted ESC starts, and never more: pasted
/// text is the operator's data, not display output, so a lone or cut-off
/// sequence must not eat the text after it.
///
/// A CSI ends at its first byte that is not a parameter or an intermediate
/// byte, and only a final byte is taken with it. An OSC ends at BEL, at ST
/// (`ESC \`) or before a line break. Any other ESC takes only a following
/// `@`..=`_` byte, the one-character escapes.
fn skip_pasted_escape(chars: &mut Peekable<Chars<'_>>) {
    match chars.peek() {
        Some('[') => {
            chars.next();
            while chars.next_if(|c| (' '..='?').contains(c)).is_some() {}
            chars.next_if(|c| ('@'..='~').contains(c));
        }
        Some(']') => {
            chars.next();
            while let Some(c) = chars.next_if(|c| !matches!(c, '\n' | '\r')) {
                match c {
                    '\u{7}' => break,
                    '\u{1b}' => {
                        chars.next_if_eq(&'\\');
                        break;
                    }
                    _ => {}
                }
            }
        }
        _ => {
            chars.next_if(|c| ('@'..='_').contains(c));
        }
    }
}

/// A character of the Unicode tag block: invisible, and a known way to hide
/// text inside text.
fn is_unicode_tag(character: char) -> bool {
    ('\u{E0000}'..='\u{E007F}').contains(&character)
}

/// The option `delta` steps from `current`, wrapping around. A `current` that
/// is not an option starts at the first (forward) or last (backward) one.
/// `None` when there are no options.
pub fn cycle_option(options: &[String], current: &str, delta: isize) -> Option<String> {
    if options.is_empty() {
        return None;
    }
    let next = match options.iter().position(|option| option == current) {
        Some(index) => step_index(index, delta, options.len()),
        None if delta >= 0 => 0,
        None => options.len() - 1,
    };
    Some(options[next].clone())
}

/// A proposed value as editable text: a string as itself, anything else as
/// JSON.
pub(crate) fn value_as_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Steps `current` by `delta`, wrapping around `length`.
fn step_index(current: usize, delta: isize, length: usize) -> usize {
    ((current as isize + delta).rem_euclid(length as isize)) as usize
}

// ---------------------------------------------------------------------------
// The inputs a workflow declares (the new-task form)
// ---------------------------------------------------------------------------
//
// A workflow's `inputs` are typed (`string`, `integer`, `boolean`,
// `repository`); the new-task form edits them as fields of the chooser, and
// the typed values become the task's JSON inputs exactly as `narsor run
// --input` parses them: `is_experimental=true` reaches the task as a boolean.

/// The two values of a `boolean` input.
const BOOLEAN_OPTIONS: [&str; 2] = ["true", "false"];

/// The labels drawn above the inputs the bundled workflows name alike. The
/// name stays the key of the value; an input not listed is drawn under its name.
const INPUT_LABELS: [(&str, &str); 2] = [("target_repo", "Repozytorium"), ("description", "Opis")];

/// One field per declared input, in declaration order.
///
/// * `boolean` is a choice between `true` and `false`; with no default it
///   starts on `false`, the value that changes nothing, so it is never unset
///   on screen;
/// * `repository` is a choice among the `repositories` the server has
///   registered, starting on the default or the first one — with none
///   registered it is a free line, so a remote the resolver still accepts can
///   be typed;
/// * `integer` is a number: the chooser refuses anything else next to the
///   field, before anything is sent;
/// * `string` is a free line.
///
/// An input is required when the workflow says so and gives no default; a
/// default is shown as the value the field starts with.
pub fn workflow_input_fields(
    inputs: &[WorkflowInputView],
    repositories: &[String],
) -> Vec<ChooserField> {
    inputs
        .iter()
        .map(|input| {
            let mut field = match input.input_type {
                WorkflowInputType::Boolean => ChooserField::select(
                    input.name.clone(),
                    BOOLEAN_OPTIONS
                        .iter()
                        .map(|option| option.to_string())
                        .collect(),
                )
                .with_value(BOOLEAN_OPTIONS[1]),
                WorkflowInputType::Repository if !repositories.is_empty() => {
                    ChooserField::select(input.name.clone(), repositories.to_vec())
                }
                WorkflowInputType::Integer => ChooserField::integer(input.name.clone()),
                WorkflowInputType::Text => ChooserField::multiline(input.name.clone()),
                WorkflowInputType::String | WorkflowInputType::Repository => {
                    ChooserField::text(input.name.clone())
                }
            };
            if let Some(default) = &input.default {
                field = field.with_value(value_as_text(default));
            }
            if input.required && input.default.is_none() {
                field = field.required();
            }
            if let Some((_, label)) = INPUT_LABELS.iter().find(|(name, _)| *name == input.name) {
                field = field.labelled(*label);
            }
            match input_description(input) {
                Some(description) => field.described(description),
                None => field,
            }
        })
        .collect()
}

/// What the current field is for: its type when that is not obvious, then
/// what the workflow says of it.
fn input_description(input: &WorkflowInputView) -> Option<String> {
    let kind = match input.input_type {
        WorkflowInputType::String | WorkflowInputType::Text => None,
        WorkflowInputType::Integer => Some("liczba całkowita"),
        WorkflowInputType::Boolean => Some("true / false"),
        WorkflowInputType::Repository => Some("repozytorium"),
    };
    let said = input
        .description
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());
    match (kind, said) {
        (Some(kind), Some(said)) => Some(format!("{kind} · {said}")),
        (Some(kind), None) => Some(kind.to_string()),
        (None, said) => said.map(str::to_string),
    }
}

/// A value of a workflow input that cannot be turned into its declared type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputProblem {
    /// The input it is about.
    pub field: String,
    /// What is wrong, in words for the field.
    pub message: String,
}

/// The task's inputs from what the form returned (`(name, value)`, a blank
/// optional field left out): every declared input that has a value or a
/// default, typed as declared.
pub fn workflow_input_values(
    inputs: &[WorkflowInputView],
    values: &[(String, String)],
) -> Result<Map<String, Value>, InputProblem> {
    let mut typed = Map::new();
    for input in inputs {
        let given = values
            .iter()
            .find(|(name, _)| *name == input.name)
            .map(|(_, value)| value.trim())
            .filter(|value| !value.is_empty());
        let Some(raw) = given else {
            if let Some(default) = &input.default {
                typed.insert(input.name.clone(), default.clone());
            }
            continue;
        };
        let value = match input.input_type {
            WorkflowInputType::String | WorkflowInputType::Text | WorkflowInputType::Repository => {
                Value::String(raw.to_string())
            }
            WorkflowInputType::Integer => {
                raw.parse::<i64>()
                    .map(Value::from)
                    .map_err(|_| InputProblem {
                        field: input.name.clone(),
                        message: format!("Pole '{}' wymaga liczby całkowitej.", input.name),
                    })?
            }
            WorkflowInputType::Boolean => match raw.to_ascii_lowercase().as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                other => {
                    return Err(InputProblem {
                        field: input.name.clone(),
                        message: format!(
                            "Pole '{}' wymaga true albo false, nie '{other}'.",
                            input.name
                        ),
                    })
                }
            },
        };
        typed.insert(input.name.clone(), value);
    }
    Ok(typed)
}
