//! Classifies conversation entries by kind, per harness (U7, R10).
//!
//! [`crate::persistence::conversation`] and the API's
//! [`crate::api::ConversationEntryDto`] deliberately keep the raw harness line
//! untouched — "the raw line stays, so a harness format change never breaks
//! the record" (the plan's Key Technical Decisions). This module is where
//! that raw line is finally turned into something an operator can tell apart
//! at a glance: a kind ([`EntryKind`]) the task view (U7) styles differently,
//! and a decoded, sanitized text to show.
//!
//! # Why sniffing the shape instead of asking which harness wrote it
//!
//! Claude Code's stream-json envelopes and opencode's NDJSON envelopes use
//! disjoint values of their own top-level `type` field (`"assistant"`,
//! `"user"`, `"system"`, `"result"` for Claude Code; `"text"`, `"reasoning"`,
//! `"tool_use"`, `"step_start"`, `"step_finish"`, `"error"` for opencode).
//! Classifying by that field, rather than threading a harness name through
//! every call site, keeps this module a pure function of one entry and still
//! tells the two harnesses' lines apart correctly — and degrades safely
//! (→ [`EntryKind::Raw`]) for whatever a future harness or a future harness
//! version sends that neither shape recognizes, per R9's "raw content is
//! always available".
//!
//! # Where `Narsor`/`Stderr` fit in
//!
//! Those two are decided by [`crate::persistence::conversation::ConversationSource`]
//! alone, never by sniffing content: a narsor-written line (the sent prompt,
//! the attempt's outcome) is always [`EntryKind::Narsor`], and anything the
//! harness process wrote to its standard error is always [`EntryKind::Stderr`]
//! — neither is JSON the harness intended to be interpreted as a message.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::api::ConversationEntryDto;
use crate::persistence::conversation::ConversationSource;

/// The kind of one conversation entry, per the plan's list: user, assistant,
/// thinking, tool call, tool result, final result, system, stderr, narsor —
/// plus [`EntryKind::Raw`] for anything that does not fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A narsor-written line: the sent prompt, or the attempt's outcome.
    Narsor,
    /// A line the harness wrote to standard error.
    Stderr,
    /// A user-role line that is not a tool result (rare in headless mode).
    User,
    /// An assistant text turn.
    Assistant,
    /// A thinking/reasoning block.
    Thinking,
    /// A tool invocation.
    ToolCall,
    /// A tool's result, reported back to the model.
    ToolResult,
    /// The harness's final result envelope (Claude Code's `result` line).
    Result,
    /// Harness-level bookkeeping: `system`/`init`, opencode's `step_start`,
    /// `step_finish` and `error` lines — none of them a conversational turn.
    System,
    /// Not valid JSON, or JSON this module does not recognize the shape of.
    /// The decoded text is always the full raw content regardless (R9).
    Raw,
}

impl EntryKind {
    /// A short label for the kind, for the task view's rendering.
    pub fn label(self) -> &'static str {
        match self {
            EntryKind::Narsor => "narsor",
            EntryKind::Stderr => "stderr",
            EntryKind::User => "user",
            EntryKind::Assistant => "assistant",
            EntryKind::Thinking => "thinking",
            EntryKind::ToolCall => "tool call",
            EntryKind::ToolResult => "tool result",
            EntryKind::Result => "result",
            EntryKind::System => "system",
            EntryKind::Raw => "raw",
        }
    }
}

/// One conversation entry, decoded and classified, ready for the task view.
#[derive(Debug, Clone)]
pub struct ClassifiedEntry {
    /// Sequential entry number, starting at 1.
    pub entry_number: u64,
    /// When narsor observed (or produced) this line.
    pub timestamp: DateTime<Utc>,
    /// Which stream this line came from.
    pub source: ConversationSource,
    /// This entry's kind.
    pub kind: EntryKind,
    /// Exact source bytes retained by the API.  Display code must use this
    /// for diagnostic/raw mode rather than attempting to reconstruct it from
    /// the readable projection below.
    pub raw: Vec<u8>,
    /// The decoded raw content, as text. Not yet sanitized — the task view's
    /// rendering path runs [`crate::tui::scroll::sanitize`] on it, the same
    /// as every other window (R10), right before it reaches the terminal.
    pub text: String,
}

/// Decodes and classifies one entry off the wire.
///
/// Never drops content: a payload that is not valid UTF-8 falls back to a
/// lossy decode rather than being discarded, and a payload that is not valid
/// JSON (or does not match a recognized shape) becomes [`EntryKind::Raw`]
/// with its text unchanged.
pub fn classify(entry: &ConversationEntryDto) -> ClassifiedEntry {
    let raw = BASE64
        .decode(entry.raw_base64.as_bytes())
        .unwrap_or_default();
    let text = String::from_utf8_lossy(&raw).into_owned();

    let (kind, text) = match entry.source {
        ConversationSource::Narsor => (EntryKind::Narsor, text),
        ConversationSource::Stderr => (EntryKind::Stderr, text),
        ConversationSource::Stdout => project_stdout_line(&text),
    };

    ClassifiedEntry {
        entry_number: entry.entry_number,
        timestamp: entry.timestamp,
        source: entry.source,
        kind,
        raw,
        text,
    }
}

/// Produces the primary, readable body while preserving the raw envelope in
/// [`ClassifiedEntry::raw`].  Technical forms deliberately retain a concise
/// summary here; their full arguments/results are still available verbatim in
/// raw mode.
fn project_stdout_line(text: &str) -> (EntryKind, String) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return (EntryKind::Raw, text.to_owned());
    };
    let kind = classify_stdout_value(&value);
    let projected = match kind {
        EntryKind::Assistant => value
            .pointer("/message/content")
            .and_then(Value::as_array)
            .and_then(|blocks| {
                blocks.iter().find_map(|block| {
                    (block.get("type").and_then(Value::as_str) == Some("text"))
                        .then(|| block.get("text").and_then(Value::as_str))
                        .flatten()
                })
            })
            .or_else(|| value.get("text").and_then(Value::as_str))
            .or_else(|| value.pointer("/item/text").and_then(Value::as_str))
            .unwrap_or(text)
            .to_owned(),
        EntryKind::Thinking => value
            .pointer("/message/content")
            .and_then(Value::as_array)
            .and_then(|blocks| {
                blocks.iter().find_map(|block| {
                    (block.get("type").and_then(Value::as_str) == Some("thinking"))
                        .then(|| block.get("thinking").and_then(Value::as_str))
                        .flatten()
                })
            })
            .or_else(|| value.get("text").and_then(Value::as_str))
            .or_else(|| value.pointer("/item/summary").and_then(Value::as_str))
            .unwrap_or("thinking")
            .to_owned(),
        EntryKind::Result => value
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or("completed")
            .to_owned(),
        EntryKind::System => value
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| value.get("type").and_then(Value::as_str))
            .unwrap_or("system event")
            .to_owned(),
        EntryKind::ToolCall => value
            .pointer("/message/content")
            .and_then(Value::as_array)
            .and_then(|blocks| {
                blocks.iter().find_map(|block| {
                    (block.get("type").and_then(Value::as_str) == Some("tool_use"))
                        .then(|| block.get("name").and_then(Value::as_str))
                        .flatten()
                })
            })
            .or_else(|| value.get("name").and_then(Value::as_str))
            .or_else(|| value.pointer("/item/type").and_then(Value::as_str))
            .map(|name| format!("calling {name}"))
            .unwrap_or_else(|| "tool call".to_owned()),
        EntryKind::ToolResult => "tool result".to_owned(),
        _ => text.to_owned(),
    };
    (kind, projected)
}

fn classify_stdout_value(value: &Value) -> EntryKind {
    let Some(type_field) = value.get("type").and_then(Value::as_str) else {
        return EntryKind::Raw;
    };

    match type_field {
        // Claude Code: one full message envelope per line; the message's own
        // content blocks (grouped by `message.id`) carry the finer kind.
        "assistant" => classify_message_content(value).unwrap_or(EntryKind::Assistant),
        "user" => classify_message_content(value).unwrap_or(EntryKind::User),
        "system" => EntryKind::System,
        "result" => EntryKind::Result,

        // opencode: the top-level `type` names the kind directly.
        "text" => EntryKind::Assistant,
        "reasoning" => EntryKind::Thinking,
        "tool_use" => EntryKind::ToolCall,
        "step_start" | "step_finish" | "error" => EntryKind::System,

        // Codex `exec --json`: lifecycle envelopes have dotted event names;
        // item envelopes carry their conversational kind one level down.
        "thread.started" | "turn.started" => EntryKind::System,
        "turn.completed" => EntryKind::Result,
        "turn.failed" => EntryKind::System,
        "item.started" | "item.updated" | "item.completed" => {
            classify_codex_item(value, type_field).unwrap_or(EntryKind::Raw)
        }

        _ => EntryKind::Raw,
    }
}

/// Classifies Codex's nested `item` payload without altering the raw JSON
/// shown by the conversation view. Completed tool items are results; all
/// other appearances are calls in progress.
fn classify_codex_item(value: &Value, event_type: &str) -> Option<EntryKind> {
    let item_type = value.pointer("/item/type")?.as_str()?;
    match item_type {
        "agent_message" => Some(EntryKind::Assistant),
        "reasoning" => Some(EntryKind::Thinking),
        "command_execution" | "mcp_tool_call" | "web_search" | "file_change" => {
            Some(if event_type == "item.completed" {
                EntryKind::ToolResult
            } else {
                EntryKind::ToolCall
            })
        }
        "error" => Some(EntryKind::System),
        _ => None,
    }
}

/// For a Claude Code `assistant`/`user` envelope, the kind its
/// `message.content` blocks carry, if any block names one this module
/// recognizes.
fn classify_message_content(value: &Value) -> Option<EntryKind> {
    let blocks = value.pointer("/message/content")?.as_array()?;
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("thinking") => return Some(EntryKind::Thinking),
            Some("tool_use") => return Some(EntryKind::ToolCall),
            Some("tool_result") => return Some(EntryKind::ToolResult),
            Some("text") => return Some(EntryKind::Assistant),
            _ => continue,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn entry(source: ConversationSource, raw: &str) -> ConversationEntryDto {
        ConversationEntryDto {
            entry_number: 1,
            timestamp: Utc::now(),
            source,
            raw_base64: BASE64.encode(raw.as_bytes()),
        }
    }

    #[test]
    fn narsor_lines_are_always_the_narsor_kind() {
        let classified = classify(&entry(ConversationSource::Narsor, "prompt: do the thing"));
        assert_eq!(classified.kind, EntryKind::Narsor);
        assert_eq!(classified.text, "prompt: do the thing");
    }

    #[test]
    fn stderr_lines_are_always_the_stderr_kind() {
        let classified = classify(&entry(ConversationSource::Stderr, "warning: something"));
        assert_eq!(classified.kind, EntryKind::Stderr);
    }

    #[test]
    fn claude_assistant_text_block_is_the_assistant_kind() {
        let raw =
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"hi"}]}}"#;
        let classified = classify(&entry(ConversationSource::Stdout, raw));
        assert_eq!(classified.kind, EntryKind::Assistant);
    }

    #[test]
    fn claude_assistant_thinking_block_is_the_thinking_kind() {
        let raw = r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"thinking","thinking":""}]}}"#;
        let classified = classify(&entry(ConversationSource::Stdout, raw));
        assert_eq!(classified.kind, EntryKind::Thinking);
    }

    #[test]
    fn claude_assistant_tool_use_block_is_the_tool_call_kind() {
        let raw = r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","name":"bash"}]}}"#;
        let classified = classify(&entry(ConversationSource::Stdout, raw));
        assert_eq!(classified.kind, EntryKind::ToolCall);
    }

    #[test]
    fn claude_user_tool_result_block_is_the_tool_result_kind() {
        let raw =
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#;
        let classified = classify(&entry(ConversationSource::Stdout, raw));
        assert_eq!(classified.kind, EntryKind::ToolResult);
    }

    #[test]
    fn claude_result_line_is_the_result_kind() {
        let raw = r#"{"type":"result","subtype":"success","result":"done"}"#;
        let classified = classify(&entry(ConversationSource::Stdout, raw));
        assert_eq!(classified.kind, EntryKind::Result);
    }

    #[test]
    fn claude_system_init_line_is_the_system_kind() {
        let raw = r#"{"type":"system","subtype":"init","session_id":"abc"}"#;
        let classified = classify(&entry(ConversationSource::Stdout, raw));
        assert_eq!(classified.kind, EntryKind::System);
    }

    #[test]
    fn opencode_text_line_is_the_assistant_kind() {
        let raw = r#"{"type":"text","sessionID":"s1","text":"hello"}"#;
        let classified = classify(&entry(ConversationSource::Stdout, raw));
        assert_eq!(classified.kind, EntryKind::Assistant);
    }

    #[test]
    fn opencode_reasoning_line_is_the_thinking_kind() {
        let raw = r#"{"type":"reasoning","sessionID":"s1","text":"because"}"#;
        let classified = classify(&entry(ConversationSource::Stdout, raw));
        assert_eq!(classified.kind, EntryKind::Thinking);
    }

    #[test]
    fn opencode_tool_use_line_is_the_tool_call_kind() {
        let raw = r#"{"type":"tool_use","sessionID":"s1","state":{"input":{}}}"#;
        let classified = classify(&entry(ConversationSource::Stdout, raw));
        assert_eq!(classified.kind, EntryKind::ToolCall);
    }

    #[test]
    fn opencode_step_finish_line_is_the_system_kind() {
        let raw = r#"{"type":"step_finish","sessionID":"s1"}"#;
        let classified = classify(&entry(ConversationSource::Stdout, raw));
        assert_eq!(classified.kind, EntryKind::System);
    }

    #[test]
    fn opencode_error_line_is_the_system_kind() {
        let raw = r#"{"type":"error","sessionID":"s1","message":"boom"}"#;
        let classified = classify(&entry(ConversationSource::Stdout, raw));
        assert_eq!(classified.kind, EntryKind::System);
    }

    #[test]
    fn unknown_type_is_raw_with_full_content() {
        let raw = r#"{"type":"api_retry","detail":"..."}"#;
        let classified = classify(&entry(ConversationSource::Stdout, raw));
        assert_eq!(classified.kind, EntryKind::Raw);
        assert_eq!(classified.text, raw);
    }

    #[test]
    fn non_json_is_raw_with_full_content() {
        let raw = "not json at all";
        let classified = classify(&entry(ConversationSource::Stdout, raw));
        assert_eq!(classified.kind, EntryKind::Raw);
        assert_eq!(classified.text, raw);
    }

    #[test]
    fn assistant_projection_uses_message_text_while_retaining_exact_source_bytes() {
        let raw = br#"{"type":"assistant","message":{"content":[{"type":"text","text":"A readable reply"}]}}"#;
        let entry = ConversationEntryDto {
            entry_number: 7,
            timestamp: Utc::now(),
            source: ConversationSource::Stdout,
            raw_base64: BASE64.encode(raw),
        };

        let classified = classify(&entry);

        assert_eq!(classified.kind, EntryKind::Assistant);
        assert_eq!(classified.text, "A readable reply");
        assert_eq!(classified.raw, raw);
    }

    #[test]
    fn malformed_binary_source_is_retained_byte_for_byte() {
        let raw = b"unknown\x00\x1b[31m\xff";
        let entry = ConversationEntryDto {
            entry_number: 8,
            timestamp: Utc::now(),
            source: ConversationSource::Stdout,
            raw_base64: BASE64.encode(raw),
        };

        let classified = classify(&entry);

        assert_eq!(classified.kind, EntryKind::Raw);
        assert_eq!(classified.raw, raw);
    }
}
