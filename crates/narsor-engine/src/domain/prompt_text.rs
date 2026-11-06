//! The text an agent or a decider is asked: a string, or a list of parts of
//! which each may depend on a value being there.

use std::fmt;

use serde::{Deserialize, Serialize};

/// What separates two parts of a text once they are joined.
const PART_SEPARATOR: &str = "\n\n";

/// A prompt or message: one or more parts, joined by an empty line when the
/// text is used.
///
/// A plain string is a text of one part that always applies, and is stored
/// as a plain string again, so a definition written before parts existed
/// reads back unchanged and an unconditional text stays what it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "PromptTextRepr", into = "PromptTextRepr")]
pub struct PromptText {
    parts: Vec<PromptPart>,
}

/// One part of a [`PromptText`]: templated text and the references that
/// must all be non-empty for the part to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptPart {
    /// The templated text.
    pub text: String,
    /// References such as `local.notes`; empty means the part always applies.
    pub when: Vec<String>,
}

impl PromptPart {
    /// A part that always applies.
    pub fn always(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            when: Vec::new(),
        }
    }

    /// A part that applies when every reference in `when` has a value.
    pub fn when(text: impl Into<String>, when: Vec<String>) -> Self {
        Self {
            text: text.into(),
            when,
        }
    }
}

impl PromptText {
    /// A text of one part that always applies.
    pub fn plain(text: impl Into<String>) -> Self {
        Self::from_parts(vec![PromptPart::always(text)])
    }

    /// A text of `parts`, in order.
    pub fn from_parts(parts: Vec<PromptPart>) -> Self {
        Self { parts }
    }

    /// The parts, in order.
    pub fn parts(&self) -> &[PromptPart] {
        &self.parts
    }

    /// Whether the text is a lone part that always applies, which is what a
    /// plain string is.
    pub fn is_plain(&self) -> bool {
        matches!(self.parts.as_slice(), [part] if part.when.is_empty())
    }

    /// Whether no part has any text.
    pub fn is_blank(&self) -> bool {
        self.parts.iter().all(|part| part.text.trim().is_empty())
    }

    /// Every templated piece the text can hold, as a template: each part's
    /// text, and each `when` reference as a placeholder, so the helpers that
    /// scan templates see what the parts refer to.
    pub fn template_pieces(&self) -> Vec<String> {
        let mut pieces = Vec::new();
        for part in &self.parts {
            pieces.push(part.text.clone());
            pieces.extend(
                part.when
                    .iter()
                    .map(|reference| format!("{{{{ {reference} }}}}")),
            );
        }
        pieces
    }

    /// The part texts only, for the helpers that ask what the text itself
    /// contains.
    pub fn texts(&self) -> impl Iterator<Item = &str> {
        self.parts.iter().map(|part| part.text.as_str())
    }

    /// The text of every part, joined, whatever the conditions say. For a
    /// text already rendered this is the text itself.
    pub fn joined(&self) -> String {
        self.texts().collect::<Vec<_>>().join(PART_SEPARATOR)
    }

    /// Adds `text` to the end of the last part. For a text already
    /// rendered, which is a single part.
    pub fn push_str(&mut self, text: &str) {
        match self.parts.last_mut() {
            Some(part) => part.text.push_str(text),
            None => self.parts.push(PromptPart::always(text)),
        }
    }
}

impl fmt::Display for PromptText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.joined())
    }
}

impl From<String> for PromptText {
    fn from(text: String) -> Self {
        Self::plain(text)
    }
}

impl From<&str> for PromptText {
    fn from(text: &str) -> Self {
        Self::plain(text)
    }
}

/// The two shapes a text is written in: a string, or a list of parts.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum PromptTextRepr {
    Plain(String),
    Parts(Vec<PartRepr>),
}

/// A part in a list: a string, or `{text, when}`.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum PartRepr {
    Plain(String),
    Conditional(ConditionalRepr),
}

/// `{text, when}`, and nothing besides.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConditionalRepr {
    text: String,
    when: WhenRepr,
}

/// `when`: one reference, or a list of them.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum WhenRepr {
    One(String),
    All(Vec<String>),
}

impl TryFrom<PromptTextRepr> for PromptText {
    type Error = String;

    fn try_from(repr: PromptTextRepr) -> Result<Self, Self::Error> {
        match repr {
            PromptTextRepr::Plain(text) => Ok(Self::plain(text)),
            PromptTextRepr::Parts(parts) => parts
                .into_iter()
                .enumerate()
                .map(|(index, part)| match part {
                    PartRepr::Plain(text) => Ok(PromptPart::always(text)),
                    PartRepr::Conditional(ConditionalRepr {
                        when: WhenRepr::All(references),
                        ..
                    }) if references.is_empty() => Err(format!(
                        "part {}: 'when' needs at least one reference",
                        index + 1
                    )),
                    PartRepr::Conditional(ConditionalRepr {
                        text,
                        when: WhenRepr::One(reference),
                    }) => Ok(PromptPart::when(text, vec![reference])),
                    PartRepr::Conditional(ConditionalRepr {
                        text,
                        when: WhenRepr::All(references),
                    }) => Ok(PromptPart::when(text, references)),
                })
                .collect::<Result<_, _>>()
                .map(Self::from_parts),
        }
    }
}

impl From<PromptText> for PromptTextRepr {
    fn from(text: PromptText) -> Self {
        if text.is_plain() {
            let [part] = <[PromptPart; 1]>::try_from(text.parts).expect("a plain text has a part");
            return Self::Plain(part.text);
        }
        Self::Parts(
            text.parts
                .into_iter()
                .map(|part| {
                    if part.when.is_empty() {
                        PartRepr::Plain(part.text)
                    } else {
                        PartRepr::Conditional(ConditionalRepr {
                            text: part.text,
                            when: WhenRepr::All(part.when),
                        })
                    }
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_plain_string_reads_as_one_unconditional_part() {
        let text: PromptText = serde_json::from_value(json!("do it")).unwrap();

        assert!(text.is_plain());
        assert_eq!(text.joined(), "do it");
    }

    #[test]
    fn a_list_reads_strings_and_conditional_parts() {
        let text: PromptText = serde_json::from_value(json!([
            "first",
            {"text": "second", "when": "local.notes"},
            {"text": "third", "when": ["context.a", "context.b"]},
        ]))
        .unwrap();

        assert_eq!(
            text.parts(),
            [
                PromptPart::always("first"),
                PromptPart::when("second", vec!["local.notes".to_string()]),
                PromptPart::when(
                    "third",
                    vec!["context.a".to_string(), "context.b".to_string()]
                ),
            ]
        );
    }

    #[test]
    fn a_plain_text_is_stored_as_a_string_and_parts_as_a_list() {
        let plain = serde_json::to_value(PromptText::plain("do it")).unwrap();
        let parts = serde_json::to_value(PromptText::from_parts(vec![
            PromptPart::always("a"),
            PromptPart::when("b", vec!["inputs.x".to_string()]),
        ]))
        .unwrap();

        assert_eq!(plain, json!("do it"));
        assert_eq!(parts, json!(["a", {"text": "b", "when": ["inputs.x"]}]));
    }

    #[test]
    fn stored_parts_read_back_equal() {
        let text = PromptText::from_parts(vec![
            PromptPart::always("a"),
            PromptPart::when("b", vec!["inputs.x".to_string()]),
        ]);

        let back: PromptText =
            serde_json::from_value(serde_json::to_value(&text).unwrap()).unwrap();

        assert_eq!(back, text);
    }
}
