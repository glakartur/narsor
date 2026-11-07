//! The text helpers of the forms and the inputs a workflow declares (the
//! new-task form), and the reason a request stopped waiting.
//!
//! What the answer panel offers for a pending human input is defined by the
//! server (`narsor_protocol::panel`) and shown by
//! [`crate::tui::task_app::panel`]; its names are given again here, so a path
//! through `input_form` still leads to them.

use std::collections::BTreeMap;
use std::iter::Peekable;
use std::str::Chars;

use serde_json::{Map, Value};

use crate::domain::display::is_invisible_format;
use crate::tui::chooser::{ChooserField, FieldKind};
use narsor_protocol::controller::{
    RepositoryView, WorkflowInputType, WorkflowInputView, WorkflowWorkspaceView, WorkspaceRequest,
};
use narsor_protocol::event::{AnsweredBy, EventData, InputCloseReason};
use narsor_protocol::panel::value_as_text;

pub use crate::tui::task_app::panel::input::{
    ContextLine, InputKind, InputPanel, Tone, COMMENT_FIELD, COMMENT_TO_AGENT,
    EXTRA_VARIABLES_FIELD, MESSAGE_ENTRY, MESSAGE_FIELD, PATTERN_FIELD,
};

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
        FieldKind::Text { multiline: false } | FieldKind::Suggest(_) => text.replace('\n', " "),
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

/// Steps `current` by `delta`, wrapping around `length`.
fn step_index(current: usize, delta: isize, length: usize) -> usize {
    ((current as isize + delta).rem_euclid(length as isize)) as usize
}

// ---------------------------------------------------------------------------
// The inputs and workspaces a workflow declares (the new-task form)
// ---------------------------------------------------------------------------
//
// A workflow's `inputs` are typed (`string`, `text`, `integer`, `boolean`);
// the new-task form edits them as fields of the chooser, and the typed values
// become the task's JSON inputs exactly as `narsor run --input` parses them:
// `is_experimental=true` reaches the task as a boolean. A workspace whose
// repository the workflow leaves to be chosen is a repository field followed
// by its branch field.

/// The two values of a `boolean` input.
const BOOLEAN_OPTIONS: [&str; 2] = ["true", "false"];

/// The labels drawn above the inputs the bundled workflows name alike. The
/// name stays the key of the value; an input not listed is drawn under its name.
const INPUT_LABELS: [(&str, &str); 1] = [("description", "Opis")];

const BRANCH_SUFFIX: &str = " · gałąź";

const REPOSITORY_SUFFIX: &str = " · repozytorium";

/// The name of the field that holds the repository of `workspace`; apart
/// from every declared input, which may share the workspace's name.
pub fn repository_field_name(workspace: &str) -> String {
    format!("{workspace}{REPOSITORY_SUFFIX}")
}

/// The name of the field that holds the branch of `workspace`: it stands
/// right after the repository field ([`repository_field_name`]).
pub fn branch_field_name(workspace: &str) -> String {
    format!("{workspace}{BRANCH_SUFFIX}")
}

/// The name of the branch field that belongs to the field `field`, when that
/// is the repository field of a workspace.
pub fn branch_field_of(field: &str) -> Option<String> {
    field.strip_suffix(REPOSITORY_SUFFIX).map(branch_field_name)
}

/// One field per declared input, in declaration order.
///
/// * `boolean` is a choice between `true` and `false`; with no default it
///   starts on `false`, the value that changes nothing, so it is never unset
///   on screen;
/// * `integer` is a number: the chooser refuses anything else next to the
///   field, before anything is sent;
/// * `text` is a field of many lines;
/// * `string` is a free line.
///
/// An input is required when the workflow says so and gives no default; a
/// default is shown as the value the field starts with.
pub fn workflow_input_fields(inputs: &[WorkflowInputView]) -> Vec<ChooserField> {
    inputs.iter().map(input_field).collect()
}

fn input_field(input: &WorkflowInputView) -> ChooserField {
    let mut field = match input.input_type {
        WorkflowInputType::Boolean => ChooserField::select(
            input.name.clone(),
            BOOLEAN_OPTIONS
                .iter()
                .map(|option| option.to_string())
                .collect(),
        )
        .with_value(BOOLEAN_OPTIONS[1]),
        WorkflowInputType::Integer => ChooserField::integer(input.name.clone()),
        WorkflowInputType::Text => ChooserField::multiline(input.name.clone()),
        WorkflowInputType::String => ChooserField::text(input.name.clone()),
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
}

/// Two fields per workspace to choose a repository for, in the order of the
/// file: the repository ([`repository_field_name`]) and its branch
/// ([`branch_field_name`]), which the new-task form draws on one row.
///
/// The repository is required: a choice among the `repositories` the server
/// has registered, starting on the first — with none registered it is a free
/// line. The branch is a free line that starts from the workspace's
/// `base_branch`, else the repository's default branch.
pub fn workspace_fields(
    workspaces: &[WorkflowWorkspaceView],
    repositories: &[RepositoryView],
) -> Vec<ChooserField> {
    workspaces
        .iter()
        .flat_map(|workspace| {
            let repository = repository_field(workspace, repositories);
            let branch = ChooserField::suggest(branch_field_name(&workspace.name), Vec::new())
                .labelled(format!("{}{BRANCH_SUFFIX}", workspace.name))
                .starting_from(suggested_branch(
                    workspace,
                    repositories,
                    repository.value(),
                ));
            [repository, branch]
        })
        .collect()
}

fn repository_field(
    workspace: &WorkflowWorkspaceView,
    repositories: &[RepositoryView],
) -> ChooserField {
    let name = repository_field_name(&workspace.name);
    let field = if repositories.is_empty() {
        ChooserField::text(name)
    } else {
        ChooserField::select(
            name,
            repositories
                .iter()
                .map(|repository| repository.id.clone())
                .collect(),
        )
    };
    let said = workspace
        .description
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());
    field
        .required()
        .labelled(workspace.name.clone())
        .described(match said {
            Some(said) => format!("repozytorium · {said}"),
            None => "repozytorium".to_string(),
        })
}

/// The branch the field of `workspace` starts from when `repository` is
/// chosen: the workspace's `base_branch`, else the repository's default
/// branch.
pub fn suggested_branch(
    workspace: &WorkflowWorkspaceView,
    repositories: &[RepositoryView],
    repository: &str,
) -> String {
    workspace
        .base_branch
        .as_deref()
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| default_branch_of(repositories, repository))
}

fn default_branch_of(repositories: &[RepositoryView], id: &str) -> String {
    repositories
        .iter()
        .find(|repository| repository.id == id)
        .and_then(|repository| repository.default_branch.as_deref())
        .map(str::trim)
        .unwrap_or_default()
        .to_string()
}

/// What the form chose for each workspace (`(name, value)` as the form
/// returned them): the repository and the branch, a blank one left out.
pub fn workspace_choices(
    workspaces: &[WorkflowWorkspaceView],
    values: &[(String, String)],
) -> BTreeMap<String, WorkspaceRequest> {
    let given = |field: &str| {
        values
            .iter()
            .find(|(name, _)| name == field)
            .map(|(_, value)| value.trim())
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    workspaces
        .iter()
        .map(|workspace| {
            let choice = WorkspaceRequest {
                repository: given(&repository_field_name(&workspace.name)),
                branch: given(&branch_field_name(&workspace.name)),
                commit: None,
            };
            (workspace.name.clone(), choice)
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
            WorkflowInputType::String | WorkflowInputType::Text => Value::String(raw.to_string()),
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
