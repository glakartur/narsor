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

use std::borrow::Cow;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use narsor_protocol::chat::{
    ChatCategory, ChatElement, PromptAuthor, SubagentRef, TechnicalCategory, ToolResultStatus,
    UnclassifiedReason,
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

/// How much of an unclassified line its expanded card shows. Such a line
/// can be many MiB; raw mode shows it in full.
const UNCLASSIFIED_PREVIEW_BYTES: usize = 4 * 1024;

/// The start of an unclassified line, decoded for display, and whether it
/// was cut: at most [`UNCLASSIFIED_PREVIEW_BYTES`] of it, since such a line
/// can be many MiB. Not sanitized; the caller does that for its medium.
pub fn unclassified_preview(raw: &[u8]) -> (Cow<'_, str>, bool) {
    let preview = &raw[..raw.len().min(UNCLASSIFIED_PREVIEW_BYTES)];
    (String::from_utf8_lossy(preview), preview.len() < raw.len())
}

/// Closes a detail preview that does not show everything.
const PREVIEW_CUT_NOTE: &str = "(podgląd skrócony, pełna treść w widoku raw)";

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
    pub raw: Arc<[u8]>,
    /// The element's one-line description ([`ChatElement::summary`]).
    pub summary: String,
    /// The subagent the entry came from; none for the main agent.
    pub subagent: Option<SubagentRef>,
}

/// The events of one entry off the wire, one per element.
///
/// An entry without elements (a page from a server older than the element
/// model) gives one unclassified event. Invalid base64 gives empty raw bytes.
pub fn classify(entry: &ConversationEntryDto) -> Vec<TranscriptEvent> {
    let raw: Arc<[u8]> = BASE64
        .decode(entry.raw_base64.as_bytes())
        .unwrap_or_default()
        .into();
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
            raw: Arc::clone(&raw),
            summary: element.summary(),
            subagent: entry.subagent.clone(),
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
    let mut transcript =
        TranscriptEntry::new(TranscriptEntryId::from(id), body, Arc::clone(&event.raw))
            .with_header(header)
            .with_details(details)
            .with_presentation(presentation)
            .with_subagent(event.subagent.clone());
    if event.element_index > 0 {
        transcript = transcript.sharing_previous_raw();
    }
    group(transcript, event)
}

/// The presentation and the speaker of `element`.
fn voice(element: &ChatElement) -> (TranscriptPresentation, &'static str) {
    use TranscriptPresentation as P;
    match element {
        ChatElement::Prompt { author, .. } => {
            (P::Operator, author.map_or("Narsor", PromptAuthor::label))
        }
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
                    details.push(PREVIEW_CUT_NOTE.to_owned());
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
                let (preview, cut) = unclassified_preview(&event.raw);
                let mut details = text_lines(&preview);
                if cut {
                    details.push(PREVIEW_CUT_NOTE.to_owned());
                }
                Self::summary(event, details)
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
fn group(transcript: TranscriptEntry, event: &TranscriptEvent) -> TranscriptEntry {
    match &event.element {
        ChatElement::ToolCall { call_id, .. } => transcript.with_correlated_operation(
            Some(TOOL_SCOPE),
            Some(&event.summary),
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
                TechnicalCategory::Hook | TechnicalCategory::Compaction | TechnicalCategory::System
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
            subagent: None,
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
            author: None,
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
                    subagent_stats: None,
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
        assert!(entries.iter().all(|card| *card.raw == *b"the line"));
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

    fn subagent(id: &str, name: &str) -> SubagentRef {
        SubagentRef {
            id: id.into(),
            name: name.into(),
        }
    }

    fn by(number: u64, who: &SubagentRef, element: ChatElement) -> ConversationEntryDto {
        ConversationEntryDto {
            subagent: Some(who.clone()),
            ..entry(number, "{}", vec![element])
        }
    }

    fn said(number: u64, text: &str) -> ConversationEntryDto {
        entry(
            number,
            "{}",
            vec![ChatElement::AgentText { text: text.into() }],
        )
    }

    fn text(who: &SubagentRef, number: u64) -> ConversationEntryDto {
        by(number, who, ChatElement::AgentText { text: "w".into() })
    }

    fn state_of(entries: &[ConversationEntryDto], width: usize) -> TranscriptState {
        let mut state = TranscriptState::new(TranscriptId::from("t"));
        state.set_width(width);
        state.load(entries.iter().flat_map(transcript_entries).collect());
        state
    }

    fn group_headers(state: &TranscriptState) -> Vec<String> {
        state
            .logical_lines()
            .into_iter()
            .filter(|line| {
                line.header && (line.text.starts_with("▸ ") || line.text.starts_with("▾ "))
            })
            .map(|line| line.text)
            .collect()
    }

    fn group_id(state: &TranscriptState, index: usize) -> TranscriptEntryId {
        state
            .logical_lines()
            .into_iter()
            .filter(|line| {
                line.header && (line.text.starts_with("▸ ") || line.text.starts_with("▾ "))
            })
            .nth(index)
            .expect("group header")
            .entry_id
    }

    fn call(number: u64, who: &SubagentRef, call_id: &str) -> ConversationEntryDto {
        by(
            number,
            who,
            ChatElement::ToolCall {
                call_id: Some(call_id.into()),
                tool: "Bash".into(),
                summary: "ls".into(),
                input: json!({}),
            },
        )
    }

    fn result(
        number: u64,
        who: &SubagentRef,
        call_id: &str,
        status: ToolResultStatus,
    ) -> ConversationEntryDto {
        by(
            number,
            who,
            ChatElement::ToolResult {
                call_id: Some(call_id.into()),
                tool: Some("Bash".into()),
                call_summary: Some("ls".into()),
                status,
                content: "out".into(),
                truncated: false,
            },
        )
    }

    #[test]
    fn the_entries_of_one_subagent_form_one_group_apart_from_the_main_agent() {
        let first = subagent("a", "Walidacja");
        let second = subagent("b", "Przegląd");
        let state = state_of(
            &[
                said(1, "main"),
                text(&first, 2),
                text(&first, 3),
                text(&first, 4),
                text(&second, 5),
                said(6, "main again"),
            ],
            80,
        );
        let headers = group_headers(&state);
        assert_eq!(headers.len(), 2, "{headers:?}");
        assert!(
            headers[0].starts_with("▸ Walidacja · 3 wpisy"),
            "{headers:?}"
        );
        assert!(headers[1].starts_with("▸ Przegląd · 1 wpis"), "{headers:?}");
        let main = state
            .logical_lines()
            .into_iter()
            .filter(|line| line.header && line.text.contains("AGENT · Agent"))
            .count();
        assert_eq!(main, 2);
    }

    #[test]
    fn entries_of_one_subagent_apart_in_time_form_one_group_at_the_first_entry() {
        let who = subagent("a", "Walidacja");
        let state = state_of(&[text(&who, 1), said(2, "main"), text(&who, 3)], 80);
        let headers: Vec<_> = state
            .logical_lines()
            .into_iter()
            .filter(|line| line.header)
            .map(|line| line.text)
            .collect();
        assert_eq!(headers.len(), 2, "{headers:?}");
        assert!(
            headers[0].starts_with("▸ Walidacja · 2 wpisy"),
            "{headers:?}"
        );
        assert!(headers[1].contains("AGENT · Agent"), "{headers:?}");
    }

    #[test]
    fn the_header_marker_follows_the_fold_state() {
        let who = subagent("a", "Walidacja");
        let mut state = state_of(&[text(&who, 1), text(&who, 2)], 80);
        let id = group_id(&state, 0);
        assert!(group_headers(&state)[0].starts_with("▸ "));
        state.toggle_expanded(&id);
        assert!(group_headers(&state)[0].starts_with("▾ "));
        assert!(state.logical_lines().iter().any(|line| line.nested));
        state.toggle_expanded(&id);
        assert!(group_headers(&state)[0].starts_with("▸ "));
    }

    #[test]
    fn the_header_stays_one_line_with_the_count_for_any_name() {
        let long =
            "Bardzo długa nazwa subagenta, która nie mieści się w wąskim terminalu".to_owned();
        let control = "Walidacja\u{1b}[31m\nczerwona\u{7}".to_owned();
        for names in [
            ["Alfa".to_owned(), "Alfa".to_owned()],
            [long.clone(), "Inny".to_owned()],
            [control.clone(), "Inny".to_owned()],
            [String::new(), "Inny".to_owned()],
        ] {
            let first = subagent("a", &names[0]);
            let second = subagent("b", &names[1]);
            let state = state_of(&[text(&first, 1), text(&first, 2), text(&second, 3)], 36);
            let lines = state.logical_lines();
            let visible = crate::tui::transcript::wrap_visible_lines(&lines, 36, 0, 100);
            for (index, count) in ["2 wpisy", "1 wpis"].into_iter().enumerate() {
                let id = group_id(&state, index);
                let rows: Vec<_> = visible
                    .iter()
                    .filter(|line| line.entry_id == id && line.header)
                    .collect();
                assert_eq!(rows.len(), 1, "{names:?} {rows:?}");
                assert!(rows[0].text.contains(count), "{names:?} {}", rows[0].text);
                assert!(!rows[0].text.contains('\u{1b}') && !rows[0].text.contains('\n'));
            }
        }
    }

    #[test]
    fn a_missing_or_repeated_name_gets_the_number_of_the_subagent() {
        let unnamed = subagent("a", "");
        let twin = subagent("b", "Alfa");
        let other_twin = subagent("c", "Alfa");
        let state = state_of(
            &[text(&unnamed, 1), text(&twin, 2), text(&other_twin, 3)],
            80,
        );
        let headers = group_headers(&state);
        assert!(headers[0].starts_with("▸ subagent 1 · "), "{headers:?}");
        assert!(headers[1].starts_with("▸ Alfa 2 · "), "{headers:?}");
        assert!(headers[2].starts_with("▸ Alfa 3 · "), "{headers:?}");
    }

    #[test]
    fn a_tool_call_and_its_result_of_a_subagent_stay_one_card_inside_its_group() {
        let who = subagent("a", "Walidacja");
        let mut state = state_of(
            &[
                call(1, &who, "c1"),
                result(2, &who, "c1", ToolResultStatus::Ok),
            ],
            80,
        );
        let id = group_id(&state, 0);
        state.toggle_expanded(&id);
        let lines = state.logical_lines();
        let headers = lines
            .iter()
            .filter(|line| line.header && line.nested)
            .count();
        let operations = lines
            .iter()
            .filter(|line| line.nested && line.text.contains("Narzędzie · Bash ls · zakończone"))
            .count();
        assert_eq!((headers, operations), (1, 1), "{lines:?}");
    }

    #[test]
    fn entries_without_a_subagent_render_as_before_next_to_a_group() {
        let who = subagent("a", "Walidacja");
        let main = [said(1, "one"), said(2, "two")];
        let alone = state_of(&main, 80);
        let mixed = state_of(&[main[0].clone(), text(&who, 3), main[1].clone()], 80);
        let rows_of = |state: &TranscriptState, id: &str| -> Vec<(String, bool, bool)> {
            state
                .logical_lines()
                .into_iter()
                .filter(|line| line.entry_id.as_str() == id)
                .filter(|line| !line.text.is_empty())
                .map(|line| (line.text, line.nested, line.indented))
                .collect()
        };
        for id in ["1", "2"] {
            assert_eq!(rows_of(&alone, id), rows_of(&mixed, id), "{id}");
        }
        assert!(group_headers(&alone).is_empty());
    }

    #[test]
    fn the_header_counts_failed_calls_and_claims_no_run_state() {
        let who = subagent("task1", "Walidacja");
        let task_call = entry(
            1,
            "{}",
            vec![ChatElement::ToolCall {
                call_id: Some("task1".into()),
                tool: "Task".into(),
                summary: "Walidacja".into(),
                input: json!({}),
            }],
        );
        let task_result = entry(
            9,
            "{}",
            vec![ChatElement::ToolResult {
                call_id: Some("task1".into()),
                tool: Some("Task".into()),
                call_summary: None,
                status: ToolResultStatus::Ok,
                content: "ok".into(),
                truncated: false,
            }],
        );
        let work = [
            call(2, &who, "c1"),
            result(3, &who, "c1", ToolResultStatus::Error),
        ];
        let running = state_of(&[task_call.clone(), work[0].clone(), work[1].clone()], 80);
        let header = &group_headers(&running)[0];
        assert!(
            !header.contains("trwa") && !header.contains("zakończony"),
            "{header}"
        );
        assert!(header.contains("błędy: 1"), "{header}");
        let done = state_of(
            &[task_call, work[0].clone(), work[1].clone(), task_result],
            80,
        );
        assert_eq!(group_headers(&done)[0], group_headers(&running)[0]);
    }

    #[test]
    fn a_new_entry_updates_the_count_and_keeps_the_fold_state_and_the_cursor() {
        let who = subagent("a", "Walidacja");
        let mut state = state_of(&[text(&who, 1), said(2, "main")], 80);
        let id = group_id(&state, 0);
        state.to_top();
        assert_eq!(state.selected_entry(), Some(&id));
        state.append(transcript_entries(&text(&who, 3)));
        let headers = group_headers(&state);
        assert!(
            headers[0].starts_with("▸ Walidacja · 2 wpisy"),
            "{headers:?}"
        );
        assert_eq!(state.selected_entry(), Some(&id));
        assert!(!state.is_following_end());
        assert!(!state.is_expanded(&id));
    }

    #[test]
    fn a_new_subagent_entry_does_not_take_the_cursor_from_the_main_thread() {
        let who = subagent("a", "Walidacja");
        let mut state = state_of(&[text(&who, 1), said(2, "main")], 80);
        let main_id = state.selected_entry().cloned().unwrap();
        state.append(transcript_entries(&text(&who, 3)));
        assert_eq!(state.selected_entry(), Some(&main_id));
        assert!(state.is_following_end());
    }
}
