//! Reading the answer of a `profile` step out of a finished harness run.
//!
//! The agent's text is untrusted: the object is found with the session's
//! bounded scanners. An answer without a usable object is a format error
//! the engine answers with a reprompt (R13). A single entry of the wrong
//! shape is not: it is kept as [`ProposedEntry::Malformed`] and skipped with
//! a note (R6).

use serde_json::{Map, Value};

use crate::domain::ActionError;
use crate::engine::actions::agent::MISSING_OUTPUT_LABEL;
use crate::engine::actions::answer::{has_profile_entries, PROFILE_ENTRIES_KEY};
use crate::engine::actions::harness::{tail, HarnessRun};
use crate::engine::actions::session::response::{json_objects, structured_then_text};

/// The most entries of one answer the engine reads; the rest are dropped.
/// A workflow has far fewer steps.
pub const MAX_PROFILE_ENTRIES: usize = 200;

/// How much of the agent's answer a rejection quotes back.
const FAILED_READ_QUOTE_BYTES: usize = 2 * 1024;

/// What the profiling agent answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileAnswer {
    /// The entries, in the order the agent gave them, at most
    /// [`MAX_PROFILE_ENTRIES`].
    pub entries: Vec<ProposedEntry>,
    /// How many entries were dropped past [`MAX_PROFILE_ENTRIES`].
    pub dropped: usize,
    /// The agent's summary in one line. Empty when it gave none.
    pub reason: String,
}

/// One entry of the answer, as the agent wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposedEntry {
    /// An object naming a step.
    Proposed(ProposedSetting),
    /// Anything else: not an object, or no string `step`.
    Malformed {
        /// Why the entry cannot be read.
        problem: &'static str,
    },
}

/// What the agent asked for one step.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProposedSetting {
    /// The step the entry is for.
    pub step: String,
    /// The model tier asked for. A value that is not a string is kept as its
    /// JSON text, so that it fails the tier check with a note.
    pub model: Option<String>,
    /// The effort asked for, kept like `model`.
    pub effort: Option<String>,
    /// Why, in the agent's words. Not sanitized yet.
    pub rationale: String,
    /// Whether the agent is sure. A missing or non-boolean value is `false`.
    pub confident: bool,
}

/// Reads the answer of a finished run: its `structured_output` first, when
/// the harness reported one with entries, then the last object with an
/// `entries` array in its text.
pub fn read_profile_answer(run: &HarnessRun) -> Result<ProfileAnswer, ActionError> {
    structured_then_text(
        run,
        |value| {
            value
                .as_object()
                .filter(|object| has_profile_entries(object))
                .map(answer_of)
                .ok_or_else(|| {
                    no_profile_error(
                        "the structured output has no 'entries' array",
                        &value.to_string(),
                    )
                })
        },
        |text| {
            json_objects(text)
                .iter()
                .rev()
                .filter_map(Value::as_object)
                .find(|object| has_profile_entries(object))
                .map(answer_of)
                .ok_or_else(|| {
                    no_profile_error(
                        "the agent's answer contains no JSON object with an 'entries' array",
                        text,
                    )
                })
        },
    )
}

/// The answer `object` (with an `entries` array) stands for.
fn answer_of(object: &Map<String, Value>) -> ProfileAnswer {
    let entries = object
        .get(PROFILE_ENTRIES_KEY)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    ProfileAnswer {
        entries: entries
            .iter()
            .take(MAX_PROFILE_ENTRIES)
            .map(proposed_entry)
            .collect(),
        dropped: entries.len().saturating_sub(MAX_PROFILE_ENTRIES),
        reason: object
            .get("reason")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_string(),
    }
}

/// One entry of the `entries` array.
fn proposed_entry(value: &Value) -> ProposedEntry {
    let Some(entry) = value.as_object() else {
        return ProposedEntry::Malformed {
            problem: "the entry is not an object",
        };
    };
    let step = match entry.get("step").and_then(Value::as_str).map(str::trim) {
        Some(step) if !step.is_empty() => step.to_string(),
        _ => {
            return ProposedEntry::Malformed {
                problem: "the entry names no step",
            }
        }
    };
    ProposedEntry::Proposed(ProposedSetting {
        step,
        model: requested_value(entry.get("model")),
        effort: requested_value(entry.get("effort")),
        rationale: entry
            .get("rationale")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_string(),
        confident: entry.get("confident").and_then(Value::as_bool) == Some(true),
    })
}

/// A requested model or effort: `None` when the agent named none.
fn requested_value(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Null => None,
        Value::String(text) => Some(text.trim().to_string()).filter(|text| !text.is_empty()),
        other => Some(other.to_string()),
    }
}

/// No profile could be read — a format error, answered with a reprompt.
fn no_profile_error(reason: &str, text: &str) -> ActionError {
    ActionError {
        message: format!(
            "no step profile could be read from the agent's answer: {reason}. \
             The agent must end with a JSON object carrying an 'entries' array"
        ),
        error_type: Some(MISSING_OUTPUT_LABEL.to_string()),
        details: Some(serde_json::json!({
            "answer_tail": tail(text, FAILED_READ_QUOTE_BYTES),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(text: &str) -> HarnessRun {
        HarnessRun {
            text: text.into(),
            ..HarnessRun::default()
        }
    }

    #[test]
    fn the_last_object_with_entries_is_the_answer() {
        let answer = read_profile_answer(&run(
            "Example: {\"entries\": []}\nNow mine:\n```json\n{\"entries\": [{\"step\": \"work\", \
             \"model\": \"large\", \"effort\": \"high\", \"rationale\": \"hard\", \
             \"confident\": true}], \"reason\": \"one step\"}\n```",
        ))
        .expect("an answer");
        assert_eq!(answer.reason, "one step");
        assert_eq!(
            answer.entries,
            vec![ProposedEntry::Proposed(ProposedSetting {
                step: "work".into(),
                model: Some("large".into()),
                effort: Some("high".into()),
                rationale: "hard".into(),
                confident: true,
            })]
        );
    }

    #[test]
    fn an_answer_without_entries_is_a_format_error() {
        let error = read_profile_answer(&run("I think `work` is hard. {\"choice\": \"x\"}"))
            .expect_err("no profile");
        assert_eq!(error.error_type.as_deref(), Some(MISSING_OUTPUT_LABEL));
        assert!(error.message.contains("entries"), "{}", error.message);
        let error = read_profile_answer(&run("no JSON at all")).expect_err("no profile");
        assert_eq!(error.error_type.as_deref(), Some(MISSING_OUTPUT_LABEL));
    }

    #[test]
    fn entries_that_are_not_an_array_are_no_answer() {
        assert!(read_profile_answer(&run("{\"entries\": {\"work\": 1}}")).is_err());
    }

    #[test]
    fn the_structured_output_is_read_first() {
        let mut run = run("{\"entries\": []}");
        run.structured_output = Some(json!({"entries": [{"step": "a", "confident": true}]}));
        let answer = read_profile_answer(&run).expect("an answer");
        assert_eq!(answer.entries.len(), 1);
    }

    #[test]
    fn a_bad_entry_is_kept_as_malformed_and_odd_values_as_text() {
        let answer = read_profile_answer(&run(
            "{\"entries\": [3, {\"model\": \"large\"}, {\"step\": \"a\", \"model\": 5, \
             \"effort\": null, \"confident\": \"yes\"}]}",
        ))
        .expect("an answer");
        assert!(matches!(answer.entries[0], ProposedEntry::Malformed { .. }));
        assert!(matches!(answer.entries[1], ProposedEntry::Malformed { .. }));
        assert_eq!(
            answer.entries[2],
            ProposedEntry::Proposed(ProposedSetting {
                step: "a".into(),
                model: Some("5".into()),
                effort: None,
                rationale: String::new(),
                confident: false,
            })
        );
    }

    #[test]
    fn entries_past_the_limit_are_dropped() {
        let entries: Vec<Value> = (0..MAX_PROFILE_ENTRIES + 3)
            .map(|index| json!({"step": format!("s{index}"), "confident": true}))
            .collect();
        let answer =
            read_profile_answer(&run(&json!({ "entries": entries }).to_string())).expect("answer");
        assert_eq!(answer.entries.len(), MAX_PROFILE_ENTRIES);
        assert_eq!(answer.dropped, 3);
    }
}
