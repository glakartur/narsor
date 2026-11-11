//! The answer object an agent or a script ends a step with (R1, KTD3).
//!
//! ```json
//! {"choice": "<label>", "reason": "<one line>", "variables": {},
//!  "artifacts": {"<name>": "<path>"}, "additional_data": {},
//!  "additional_artifacts": ["<path>"]}
//! ```
//!
//! Only `choice` is needed for the object to be an answer (a step with an
//! `exit` needs no `choice` at all). `reason` is asked
//! for by the contract, but an answer without one is accepted with an empty
//! reason (R3). A field of the wrong shape makes the answer unusable rather
//! than being dropped: a value the step requires would otherwise go missing
//! without anybody being told why.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::domain::{ActionResult, EXIT_LABEL};

/// The key that makes an object an answer, and carries its label.
pub const CHOICE_KEY: &str = "choice";

/// A choice, with everything the action handed in beside it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChoiceAnswer {
    /// The raw label, exactly as written. Whether the state declares it is
    /// the engine's judgement, never the executor's.
    pub choice: String,
    /// Why this choice was made. Empty when the answer gave no reason.
    pub reason: String,
    /// Values for the step's variables.
    pub variables: Map<String, Value>,
    /// The step's named artifacts: name → path.
    pub artifacts: BTreeMap<String, String>,
    /// Anything else, outside the step's definitions.
    pub additional_data: Map<String, Value>,
    /// Paths of further files, outside the step's definitions.
    pub additional_artifacts: Vec<String>,
}

impl ChoiceAnswer {
    /// The action result this answer makes; the executor adds what it
    /// observed itself (exit code, output, reply).
    pub fn into_result(self) -> ActionResult {
        ActionResult {
            output_label: self.choice,
            reason: self.reason,
            variables: self.variables,
            artifacts: self.artifacts,
            additional_data: self.additional_data,
            additional_artifacts: self.additional_artifacts,
            ..ActionResult::default()
        }
    }
}

/// What makes an object the answer of a step.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AnswerShape {
    /// The step has choices: the answer carries a string `choice`.
    #[default]
    Choice,
    /// The step has one `exit`: any object is the answer, and it needs no
    /// `choice` — the step's only label is [`EXIT_LABEL`].
    Exit,
    /// The step is a `profile` step: the answer is an object with an array
    /// under [`PROFILE_ENTRIES_KEY`], and the step's only label is
    /// [`EXIT_LABEL`]. It has no question to ask.
    Profile,
}

impl AnswerShape {
    /// The shape of the answer of a step that has an `exit` or not.
    pub fn of_step(has_exit: bool) -> Self {
        if has_exit {
            Self::Exit
        } else {
            Self::Choice
        }
    }
}

/// The key of the entries a `profile` step answers with.
pub const PROFILE_ENTRIES_KEY: &str = "entries";

/// Whether `object` carries an array of profile entries, which makes it the
/// answer of a `profile` step.
pub fn has_profile_entries(object: &Map<String, Value>) -> bool {
    object.get(PROFILE_ENTRIES_KEY).is_some_and(Value::is_array)
}

/// Whether `object` carries a string `choice`, which makes it an answer.
pub fn has_choice(object: &Map<String, Value>) -> bool {
    object.get(CHOICE_KEY).and_then(Value::as_str).is_some()
}

/// Reads the answer `object` stands for. `None` when it has no string
/// `choice`; an error names the field that has the wrong shape.
pub fn read_choice_answer(
    object: &Map<String, Value>,
) -> Option<Result<ChoiceAnswer, &'static str>> {
    let choice = object.get(CHOICE_KEY)?.as_str()?.to_string();
    Some(answer_with(choice, object))
}

/// Reads the answer `object` stands for in a step of this `shape`. In an
/// `exit` step a missing `choice` is the exit label; one given is kept, so
/// the engine can turn down a label the step does not declare.
pub fn read_answer(
    object: &Map<String, Value>,
    shape: AnswerShape,
) -> Option<Result<ChoiceAnswer, &'static str>> {
    match shape {
        AnswerShape::Choice => read_choice_answer(object),
        AnswerShape::Exit => {
            let choice = object
                .get(CHOICE_KEY)
                .and_then(Value::as_str)
                .unwrap_or(EXIT_LABEL)
                .to_string();
            Some(answer_with(choice, object))
        }
        AnswerShape::Profile => has_profile_entries(object).then(|| {
            Ok(ChoiceAnswer {
                choice: EXIT_LABEL.to_string(),
                reason: reason(object)?,
                ..ChoiceAnswer::default()
            })
        }),
    }
}

fn answer_with(choice: String, object: &Map<String, Value>) -> Result<ChoiceAnswer, &'static str> {
    Ok(ChoiceAnswer {
        choice,
        reason: reason(object)?,
        variables: object_field(object, "variables").ok_or("'variables' must be an object")?,
        artifacts: named_artifacts(object)?,
        additional_data: object_field(object, "additional_data")
            .ok_or("'additional_data' must be an object")?,
        additional_artifacts: additional_artifacts(object)?,
    })
}

fn reason(object: &Map<String, Value>) -> Result<String, &'static str> {
    match object.get("reason") {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(reason)) => Ok(reason.trim().to_string()),
        Some(_) => Err("'reason' must be a string"),
    }
}

/// The object at `key`, empty when the key is absent. `None` when the value
/// is not an object.
fn object_field(object: &Map<String, Value>, key: &str) -> Option<Map<String, Value>> {
    match object.get(key) {
        None | Some(Value::Null) => Some(Map::new()),
        Some(Value::Object(fields)) => Some(fields.clone()),
        Some(_) => None,
    }
}

fn named_artifacts(object: &Map<String, Value>) -> Result<BTreeMap<String, String>, &'static str> {
    const SHAPE: &str = "'artifacts' must be an object of artifact names to paths";
    let entries = object_field(object, "artifacts").ok_or(SHAPE)?;
    entries
        .into_iter()
        .map(|(name, path)| match path {
            Value::String(path) => Ok((name, path)),
            _ => Err(SHAPE),
        })
        .collect()
}

fn additional_artifacts(object: &Map<String, Value>) -> Result<Vec<String>, &'static str> {
    const SHAPE: &str = "'additional_artifacts' must be an array of paths";
    match object.get("additional_artifacts") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(paths)) => paths
            .iter()
            .map(|path| path.as_str().map(ToString::to_string).ok_or(SHAPE))
            .collect(),
        Some(_) => Err(SHAPE),
    }
}
