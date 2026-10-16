//! Turns retained conversation entries into the transcript model.
//!
//! The harness stream parser (and the executor, for the lines Narsor writes
//! itself) already classified every entry into harness-independent
//! [`ChatElement`]s, which the API sends next to the raw line. This module
//! reads those elements only. It never inspects the raw line: that stays the
//! lossless diagnostic record, shown byte for byte in raw mode.
//!
//! One entry can hold several elements (an assistant message with text and
//! two tool calls). Every element becomes its own [`TranscriptEvent`] and its
//! own transcript card. The cards of one entry share the entry's raw bytes,
//! and the global raw view shows them once, as one line per entry.
//!
//! Short text comes from [`ChatElement::summary`] and labels from
//! [`ChatCategory::label`], so the CLI can print the same words.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use narsor_protocol::chat::{
    ChatCategory, ChatElement, TechnicalCategory, ToolResultStatus, UnclassifiedReason,
};
use serde_json::Value;

use crate::api::{ConversationEntryDto, ConversationEntrySource};

use super::scroll::{one_line, sanitize};
use super::transcript::{
    utterance_header, TranscriptDetailLine, TranscriptEntry, TranscriptEntryId,
    TranscriptPresentation,
};

/// How many lines of speech a collapsed card shows.
const SPEECH_PREVIEW_LINES: usize = 5;

/// Correlation scope of tool operations (call and result).
const TOOL_SCOPE: &str = "tools";

/// Correlation scope of contiguous technical events of one category.
const TECHNICAL_SCOPE: &str = "technical";

/// The kind of one transcript event, derived from its element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A prompt Narsor sent, or a user message the harness reported.
    SentToAgent,
    /// The agent's reasoning.
    Thinking,
    /// Text the agent wrote.
    AgentText,
    /// A question the agent asked the operator.
    AgentQuestion,
    /// A tool the agent invoked.
    ToolCall,
    /// The outcome of a tool call.
    ToolResult,
    /// The final run result.
    Result,
    /// Session metadata, hooks, rate limits, stderr, the process outcome.
    Technical,
    /// A line the parser could not classify, or an element from a newer
    /// version.
    Unclassified,
}

impl EntryKind {
    /// The kind of `element`.
    pub fn of(element: &ChatElement) -> Self {
        match element {
            ChatElement::Prompt { .. } | ChatElement::UserMessage { .. } => Self::SentToAgent,
            ChatElement::Thinking { .. } => Self::Thinking,
            ChatElement::AgentText { .. } => Self::AgentText,
            ChatElement::AgentQuestion { .. } => Self::AgentQuestion,
            ChatElement::ToolCall { .. } => Self::ToolCall,
            ChatElement::ToolResult { .. } => Self::ToolResult,
            ChatElement::RunResult { .. } => Self::Result,
            ChatElement::SessionStarted { .. } | ChatElement::Technical { .. } => Self::Technical,
            ChatElement::Unclassified { .. } | ChatElement::Unrecognized => Self::Unclassified,
        }
    }

    /// The model's category of this kind.
    pub fn category(self) -> ChatCategory {
        match self {
            Self::SentToAgent => ChatCategory::SentToAgent,
            Self::Thinking => ChatCategory::Thinking,
            Self::AgentText => ChatCategory::AgentText,
            Self::AgentQuestion => ChatCategory::AgentQuestion,
            Self::ToolCall | Self::ToolResult => ChatCategory::Tool,
            Self::Result => ChatCategory::Result,
            Self::Technical => ChatCategory::Technical,
            Self::Unclassified => ChatCategory::Unclassified,
        }
    }

    /// The short label of the kind: the category label, shared with the CLI.
    pub fn label(self) -> &'static str {
        self.category().label()
    }
}

/// One element of one retained entry.
#[derive(Debug, Clone)]
pub struct TranscriptEvent {
    /// Sequential entry number, starting at 1.
    pub entry_number: u64,
    /// Position of the element in its entry, starting at 0.
    pub element_index: usize,
    /// When narsor observed (or produced) the entry.
    pub timestamp: DateTime<Utc>,
    /// Which stream the entry came from.
    pub source: ConversationEntrySource,
    /// The element's kind.
    pub kind: EntryKind,
    /// The element itself, with its full content.
    pub element: ChatElement,
    /// Exact source bytes of the whole entry, shared by all its elements.
    /// Only raw mode shows them.
    pub raw: Vec<u8>,
    /// The element's one-line description ([`ChatElement::summary`]).
    pub summary: String,
}

/// The events of one entry off the wire, one per element.
///
/// An entry without elements (a page from a server older than the element
/// model) gives one unclassified event. Invalid base64 gives empty raw bytes.
pub fn classify(entry: &ConversationEntryDto) -> Vec<TranscriptEvent> {
    let raw = BASE64
        .decode(entry.raw_base64.as_bytes())
        .unwrap_or_default();
    let fallback = [ChatElement::Unclassified {
        reason: UnclassifiedReason::NotRecorded,
    }];
    let elements = if entry.elements.is_empty() {
        &fallback[..]
    } else {
        &entry.elements[..]
    };
    elements
        .iter()
        .enumerate()
        .map(|(element_index, element)| TranscriptEvent {
            entry_number: entry.entry_number,
            element_index,
            timestamp: entry.timestamp,
            source: entry.source,
            kind: EntryKind::of(element),
            element: element.clone(),
            raw: raw.clone(),
            summary: element.summary(),
        })
        .collect()
}

/// The transcript cards of one entry, one per element.
pub fn transcript_entries(entry: &ConversationEntryDto) -> Vec<TranscriptEntry> {
    classify(entry).iter().map(agent_transcript_entry).collect()
}

/// Projects one event into the presentation model shared with the
/// conductor's chat.
pub(crate) fn agent_transcript_entry(event: &TranscriptEvent) -> TranscriptEntry {
    let (presentation, speaker) = voice(&event.element);
    let content = Content::of(event);
    let mut body = content.body;
    let extra_body = if content.speech && body.len() > SPEECH_PREVIEW_LINES {
        body.split_off(SPEECH_PREVIEW_LINES)
    } else {
        Vec::new()
    };
    let details = extra_body
        .into_iter()
        .map(|text| TranscriptDetailLine::new(text, presentation))
        .chain(
            content
                .details
                .into_iter()
                .map(|text| TranscriptDetailLine::new(text, TranscriptPresentation::Technical)),
        )
        .collect();
    let id = if event.element_index == 0 {
        event.entry_number.to_string()
    } else {
        format!("{}.{}", event.entry_number, event.element_index + 1)
    };
    let header = utterance_header(
        presentation,
        &format!("{speaker} · {}", event.kind.label()),
        event.timestamp,
    );
    let mut transcript = TranscriptEntry::new(TranscriptEntryId::from(id), body, event.raw.clone())
        .with_header(header)
        .with_details(details)
        .with_presentation(presentation);
    if event.element_index > 0 {
        transcript = transcript.sharing_previous_raw();
    }
    group(transcript, &event.element)
}

/// The presentation and the speaker of `element`.
fn voice(element: &ChatElement) -> (TranscriptPresentation, &'static str) {
    use TranscriptPresentation as P;
    match element {
        ChatElement::Prompt { .. } => (P::Operator, "Narsor"),
        ChatElement::UserMessage { .. } => (P::Operator, "Operator"),
        ChatElement::AgentText { .. } | ChatElement::AgentQuestion { .. } => (P::Agent, "Agent"),
        ChatElement::RunResult { is_error, .. } => {
            (if *is_error { P::Error } else { P::Agent }, "Agent")
        }
        ChatElement::ToolResult { status, .. } if *status == ToolResultStatus::Error => {
            (P::Error, "Harness")
        }
        ChatElement::Thinking { .. }
        | ChatElement::ToolCall { .. }
        | ChatElement::ToolResult { .. }
        | ChatElement::Unclassified { .. }
        | ChatElement::Unrecognized => (P::Technical, "Harness"),
        ChatElement::SessionStarted { .. } => (P::System, "Harness"),
        ChatElement::Technical { category, .. } => match category {
            TechnicalCategory::Stderr => (P::Technical, "stderr"),
            TechnicalCategory::Process => (P::System, "Narsor"),
            _ => (P::System, "Harness"),
        },
    }
}

/// What a card shows: its body and what expanding it adds.
struct Content {
    /// Logical body lines, already sanitized.
    body: Vec<String>,
    /// Whether the body is speech: a preview of the full text rather than a
    /// one-line summary.
    speech: bool,
    /// Further lines shown only when expanded, already sanitized.
    details: Vec<String>,
}

impl Content {
    fn of(event: &TranscriptEvent) -> Self {
        match &event.element {
            ChatElement::Prompt {
                text,
                resumed_session,
                ..
            } => Self::speech(
                text,
                labelled(&[("session", resumed_session.as_deref().unwrap_or_default())]),
            ),
            ChatElement::UserMessage { text } | ChatElement::AgentText { text } => {
                Self::speech(text, Vec::new())
            }
            ChatElement::AgentQuestion {
                question, options, ..
            } => {
                let mut text = question.clone();
                for option in options {
                    text.push_str("\n• ");
                    text.push_str(option);
                }
                Self::speech(&text, Vec::new())
            }
            ChatElement::RunResult {
                text: Some(text),
                structured_output,
                usage,
                ..
            } if !text.trim().is_empty() => {
                let mut details = vec![one_line(&event.summary)];
                details.extend(json_block("structured output", structured_output.as_ref()));
                details.extend(json_block("usage", usage.as_ref()));
                Self::speech(text, details)
            }
            ChatElement::RunResult {
                structured_output,
                usage,
                ..
            } => Self::summary(
                event,
                json_block("structured output", structured_output.as_ref())
                    .into_iter()
                    .chain(json_block("usage", usage.as_ref()))
                    .collect(),
            ),
            ChatElement::Thinking { text, .. } => Self::summary(event, text_lines(text)),
            ChatElement::ToolCall { input, .. } => {
                Self::summary(event, json_block("input", Some(input)))
            }
            ChatElement::ToolResult {
                content, truncated, ..
            } => {
                let mut details = text_lines(content);
                if *truncated {
                    details.push("(podgląd skrócony, pełna treść w widoku raw)".to_owned());
                }
                Self::summary(event, details)
            }
            ChatElement::SessionStarted {
                cwd,
                permission_mode,
                ..
            } => Self::summary(
                event,
                labelled(&[
                    ("cwd", cwd.as_deref().unwrap_or_default()),
                    (
                        "permission mode",
                        permission_mode.as_deref().unwrap_or_default(),
                    ),
                ]),
            ),
            ChatElement::Technical { data, .. } => {
                Self::summary(event, json_block("data", data.as_ref()))
            }
            ChatElement::Unclassified { .. } | ChatElement::Unrecognized => {
                Self::summary(event, text_lines(&String::from_utf8_lossy(&event.raw)))
            }
        }
    }

    /// The whole `text` as the body, with `details` after it.
    fn speech(text: &str, details: Vec<String>) -> Self {
        Self {
            body: text_lines(text),
            speech: true,
            details,
        }
    }

    /// The event's summary as the one body line, and `details` without the
    /// lines the body already shows.
    fn summary(event: &TranscriptEvent, mut details: Vec<String>) -> Self {
        let body = vec![one_line(&event.summary)];
        if let Some(index) = details.iter().position(|line| *line == body[0]) {
            details.remove(index);
        }
        Self {
            body,
            speech: false,
            details,
        }
    }
}

/// `text` as sanitized logical lines, without trailing blank lines.
fn text_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = sanitize(text).lines().map(str::to_owned).collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines
}

/// `label: value` lines for the non-empty values.
fn labelled(fields: &[(&str, &str)]) -> Vec<String> {
    fields
        .iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(label, value)| format!("{label}: {}", one_line(value)))
        .collect()
}

/// `label:` followed by `value` as pretty JSON; nothing for an absent,
/// null or empty value.
fn json_block(label: &str, value: Option<&Value>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    let empty = match value {
        Value::Null => true,
        Value::Object(fields) => fields.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::String(text) => text.is_empty(),
        _ => false,
    };
    if empty {
        return Vec::new();
    }
    let pretty = serde_json::to_string_pretty(value).unwrap_or_default();
    std::iter::once(format!("{label}:"))
        .chain(text_lines(&pretty))
        .collect()
}

/// Attaches the grouping of `element`: a tool call and its result form one
/// operation card, and contiguous technical events of one category form one
/// summary card. Both stay expandable member by member.
fn group(transcript: TranscriptEntry, element: &ChatElement) -> TranscriptEntry {
    match element {
        ChatElement::ToolCall { call_id, .. } => transcript.with_correlated_operation(
            Some(TOOL_SCOPE),
            Some(&element.summary()),
            Some("started"),
            call_id.as_deref(),
        ),
        ChatElement::ToolResult { call_id, tool, .. } => transcript.with_correlated_operation(
            Some(TOOL_SCOPE),
            Some(tool.as_deref().unwrap_or("tool")),
            Some("completed"),
            call_id.as_deref(),
        ),
        ChatElement::Technical { category, .. }
            if matches!(
                category,
                TechnicalCategory::Hook
                    | TechnicalCategory::RateLimit
                    | TechnicalCategory::Compaction
                    | TechnicalCategory::System
            ) =>
        {
            transcript
                .with_operation_grouping(
                    Some(TECHNICAL_SCOPE),
                    Some(category.label()),
                    None,
                    Some(category.label()),
                )
                .with_group_family("Techniczne")
        }
        _ => transcript,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::transcript::{DisplayMode, TranscriptId, TranscriptState};
    use narsor_protocol::chat::{PromptPurpose, QuestionSource};
    use serde_json::json;

    fn entry(number: u64, raw: &str, elements: Vec<ChatElement>) -> ConversationEntryDto {
        ConversationEntryDto {
            entry_number: number,
            timestamp: Utc::now(),
            source: ConversationEntrySource::Stdout,
            raw_base64: BASE64.encode(raw.as_bytes()),
            elements,
        }
    }

    fn lines(entries: Vec<TranscriptEntry>, mode: DisplayMode, expand: bool) -> Vec<String> {
        let mut transcript = TranscriptState::new(TranscriptId::from("t"));
        let ids: Vec<_> = entries.iter().map(|entry| entry.id.clone()).collect();
        transcript.load(entries);
        transcript.set_mode(mode);
        if expand {
            for id in &ids {
                transcript.toggle_expanded(id);
            }
        }
        transcript
            .logical_lines()
            .into_iter()
            .map(|line| line.text)
            .collect()
    }

    fn prompt(text: &str) -> ChatElement {
        ChatElement::Prompt {
            text: text.to_owned(),
            resumed_session: None,
            purpose: PromptPurpose::Step,
        }
    }

    #[test]
    fn every_element_kind_has_its_category_label() {
        let cases = [
            (prompt("p"), "sent"),
            (
                ChatElement::Thinking {
                    text: "t".into(),
                    redacted: false,
                },
                "thinking",
            ),
            (ChatElement::AgentText { text: "a".into() }, "agent"),
            (
                ChatElement::AgentQuestion {
                    question: "q?".into(),
                    options: vec![],
                    source: QuestionSource::OutputContract,
                    call_id: None,
                },
                "question",
            ),
            (
                ChatElement::ToolCall {
                    call_id: None,
                    tool: "Read".into(),
                    summary: "a.txt".into(),
                    input: json!({}),
                },
                "tool",
            ),
            (
                ChatElement::RunResult {
                    text: None,
                    structured_output: None,
                    stop_reason: None,
                    is_error: false,
                    cost_usd: None,
                    usage: None,
                    turns: None,
                    duration_ms: None,
                },
                "result",
            ),
            (
                ChatElement::Technical {
                    category: TechnicalCategory::Stderr,
                    summary: "warn".into(),
                    data: None,
                },
                "technical",
            ),
            (ChatElement::Unrecognized, "unclassified"),
        ];
        for (element, label) in cases {
            let events = classify(&entry(1, "x", vec![element.clone()]));
            assert_eq!(events[0].kind.label(), label, "{element:?}");
            let header = &agent_transcript_entry(&events[0]).header;
            assert!(header.contains(&format!(" · {label} · ")), "{header}");
        }
    }

    #[test]
    fn an_entry_without_elements_is_one_unclassified_event() {
        let events = classify(&entry(3, "legacy line", Vec::new()));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EntryKind::Unclassified);
        assert_eq!(events[0].summary, "recorded without elements");
        let expanded = lines(
            transcript_entries(&entry(3, "legacy line", Vec::new())),
            DisplayMode::Rendered,
            true,
        );
        assert!(
            expanded.iter().any(|line| line == "legacy line"),
            "{expanded:?}"
        );
    }

    #[test]
    fn a_long_prompt_previews_five_lines_and_expands_the_rest_once() {
        let text = "one\ntwo\nthree\nfour\nfive\nsix\nseven";
        let entries = transcript_entries(&entry(1, "{}", vec![prompt(text)]));
        assert_eq!(
            entries[0].rendered_lines,
            ["one", "two", "three", "four", "five"]
        );
        let expanded = lines(entries, DisplayMode::Rendered, true);
        for expected in ["one", "two", "three", "four", "five", "six", "seven"] {
            assert_eq!(
                expanded.iter().filter(|line| *line == expected).count(),
                1,
                "{expanded:?}"
            );
        }
        assert!(
            expanded[0].contains("USER · Narsor · sent · "),
            "{expanded:?}"
        );
    }

    #[test]
    fn the_elements_of_one_entry_get_distinct_ids_and_share_the_raw_line() {
        let entries = transcript_entries(&entry(
            7,
            "the line",
            vec![
                ChatElement::AgentText { text: "a".into() },
                ChatElement::AgentText { text: "b".into() },
            ],
        ));
        assert_eq!(entries[0].id.as_str(), "7");
        assert_eq!(entries[1].id.as_str(), "7.2");
        assert!(entries.iter().all(|card| card.raw == b"the line"));
    }

    #[test]
    fn a_tool_call_and_its_result_form_one_operation_card() {
        let entries = [
            entry(
                1,
                "call",
                vec![ChatElement::ToolCall {
                    call_id: Some("c1".into()),
                    tool: "Bash".into(),
                    summary: "git status".into(),
                    input: json!({"command": "git status"}),
                }],
            ),
            entry(
                2,
                "result",
                vec![ChatElement::ToolResult {
                    call_id: Some("c1".into()),
                    tool: Some("Bash".into()),
                    call_summary: Some("git status".into()),
                    status: ToolResultStatus::Error,
                    content: "fatal: not a git repository".into(),
                    truncated: false,
                }],
            ),
        ]
        .iter()
        .flat_map(transcript_entries)
        .collect();
        let rendered = lines(entries, DisplayMode::Rendered, false);
        assert!(
            rendered
                .iter()
                .any(|line| line == "Narzędzie · Bash git status · zakończone · błąd"),
            "{rendered:?}"
        );
    }

    #[test]
    fn contiguous_hooks_form_one_technical_card() {
        let hook = |summary: &str| ChatElement::Technical {
            category: TechnicalCategory::Hook,
            summary: summary.into(),
            data: None,
        };
        let entries = (1..=3)
            .flat_map(|number| transcript_entries(&entry(number, "{}", vec![hook("SessionStart")])))
            .collect();
        let rendered = lines(entries, DisplayMode::Rendered, false);
        assert_eq!(
            rendered
                .iter()
                .filter(|line| line.contains(" · hook"))
                .count(),
            1,
            "{rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|line| line == "Techniczne · hook · 3 wpisy"),
            "{rendered:?}"
        );
    }
}
