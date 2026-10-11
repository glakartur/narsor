//! Translates retained conversation records into one transcript event model.
//!
//! [`crate::persistence::conversation`] and the API's
//! [`crate::api::ConversationEntryDto`] deliberately keep the raw harness line
//! untouched — "the raw line stays, so a harness format change never breaks
//! the record" (the plan's Key Technical Decisions). This module is where
//! that raw line becomes a semantic kind, summary, and normalized details.
//! The renderer sanitizes these fields before terminal display.
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
//! version sends that neither shape recognizes. Every event retains its exact
//! source bytes for raw mode.
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

use crate::api::{ConversationEntryDto, ConversationEntrySource};

use super::scroll::{one_line, sanitize};
use super::transcript::{
    utterance_header, TranscriptEntry, TranscriptEntryId, TranscriptPresentation,
};

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
    /// The source bytes remain available in raw mode.
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

/// One retained record translated into the shared transcript vocabulary.
#[derive(Debug, Clone)]
pub struct TranscriptEvent {
    /// Sequential entry number, starting at 1.
    pub entry_number: u64,
    /// When narsor observed (or produced) this line.
    pub timestamp: DateTime<Utc>,
    /// Which stream this line came from.
    pub source: ConversationEntrySource,
    /// This entry's kind.
    pub kind: EntryKind,
    /// Exact source bytes retained by the API.  Display code must use this
    /// for diagnostic/raw mode rather than attempting to reconstruct it from
    /// the readable projection below.
    pub raw: Vec<u8>,
    /// Short semantic presentation. Terminal sanitization happens at display.
    pub summary: String,
    /// Adapter-neutral fields, including longer content for expansion.
    pub details: Value,
}

impl TranscriptEvent {
    /// A JSON view of the event. Exact bytes remain in [`Self::raw`].
    pub fn as_json(&self) -> Value {
        serde_json::json!({
            "kind": self.kind.label(),
            "summary": self.summary,
            "details": self.details,
            "raw": self.raw,
        })
    }

    /// Complete normalized fields for rendered expansion; raw mode owns the
    /// source payload and is the only view that displays it.
    pub fn detail_lines(&self) -> Vec<String> {
        if self
            .details
            .as_object()
            .is_none_or(serde_json::Map::is_empty)
        {
            return Vec::new();
        }
        serde_json::to_string_pretty(&self.details)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

/// Decodes and classifies one entry off the wire.
///
/// Never drops content: invalid UTF-8 is decoded lossily for presentation,
/// while [`TranscriptEvent::raw`] always retains the original bytes. Unknown
/// JSON receives a generic summary and [`EntryKind::Raw`].
pub fn classify(entry: &ConversationEntryDto) -> TranscriptEvent {
    let raw = BASE64
        .decode(entry.raw_base64.as_bytes())
        .unwrap_or_default();
    let text = String::from_utf8_lossy(&raw).into_owned();

    let (kind, summary, details) = match entry.source {
        ConversationEntrySource::Narsor => adapt_narsor(&text),
        ConversationEntrySource::Stderr => (
            EntryKind::Stderr,
            text.clone(),
            serde_json::json!({"error.message": text}),
        ),
        ConversationEntrySource::Stdout => adapt_stdout(&text),
    };

    TranscriptEvent {
        entry_number: entry.entry_number,
        timestamp: entry.timestamp,
        source: entry.source,
        kind,
        raw,
        summary,
        details,
    }
}

fn adapt_narsor(text: &str) -> (EntryKind, String, Value) {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        if let Some(event) = value.get("event").and_then(Value::as_str) {
            let (normalized, summary) = match event {
                "prompt" | "agent_input" => (
                    "agent_input",
                    value
                        .get("prompt")
                        .or_else(|| value.get("message"))
                        .and_then(Value::as_str)
                        .and_then(|prompt| prompt.lines().next())
                        .filter(|prompt| !prompt.is_empty())
                        .unwrap_or("Workflow sends input to agent"),
                ),
                "step_started" | "step_start" => ("step_started", "Workflow step started"),
                "step_completed" | "step_finish" => ("step_completed", "Workflow step completed"),
                "step_failed" => ("step_failed", "Workflow step failed"),
                "retry" => ("retry", "Workflow retries agent step"),
                "waiting_for_input" => ("waiting_for_input", "Workflow waits for input"),
                _ => ("message", "Workflow message"),
            };
            let mut details = serde_json::json!({"workflow.event": normalized});
            insert_detail(&mut details, "workflow.harness", value.get("harness"));
            insert_detail(
                &mut details,
                "workflow.session",
                value.get("resumed_session"),
            );
            insert_detail(&mut details, "workflow.step", value.get("step"));
            insert_detail(&mut details, "workflow.attempt", value.get("attempt"));
            insert_detail(
                &mut details,
                "message.content",
                value.get("prompt").or_else(|| value.get("message")),
            );
            insert_detail(&mut details, "error.message", value.get("error"));
            return (EntryKind::Narsor, summary.into(), details);
        }
    }
    let lower = text.to_ascii_lowercase();
    let (event, summary) = if lower.starts_with("prompt:") {
        ("agent_input", "Workflow sends input to agent")
    } else if lower.contains("waiting") && lower.contains("input") {
        ("waiting_for_input", "Workflow waits for input")
    } else if lower.contains("retry") {
        ("retry", "Workflow retries agent step")
    } else if lower.contains("failed")
        || lower.contains("error")
        || lower.contains("timed out")
        || lower.contains("did not start")
        || lower.starts_with("harness exited with code ") && !lower.ends_with("code 0")
    {
        ("step_failed", "Workflow step failed")
    } else if lower.contains("completed")
        || lower.contains("success")
        || lower.contains("exited with code 0")
    {
        ("step_completed", "Workflow step completed")
    } else if lower.contains("started") || lower.contains("running") {
        ("step_started", "Workflow step started")
    } else {
        ("message", "Workflow message")
    };
    (
        EntryKind::Narsor,
        summary.into(),
        serde_json::json!({"workflow.event": event, "workflow.message": text}),
    )
}

fn adapt_stdout(text: &str) -> (EntryKind, String, Value) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return (
            EntryKind::Raw,
            text.to_owned(),
            serde_json::json!({"message.content": text}),
        );
    };
    if is_codex_event(&value) || is_codex_item_event(&value) || is_codex_direct_response(&value) {
        return adapt_codex(&value);
    }
    match value.get("type").and_then(Value::as_str) {
        Some("assistant" | "user" | "system" | "result") => adapt_claude(&value),
        Some("text" | "reasoning" | "tool_use" | "step_start" | "step_finish" | "error") => {
            adapt_opencode(&value)
        }
        _ => {
            let mut details = serde_json::json!({});
            insert_detail(&mut details, "event.type", value.get("type"));
            (EntryKind::Raw, generic_json_summary(&value), details)
        }
    }
}

fn adapt_claude(value: &Value) -> (EntryKind, String, Value) {
    if value.get("type").and_then(Value::as_str) == Some("system") {
        let subtype = value.get("subtype").and_then(Value::as_str);
        let known = matches!(
            subtype,
            Some(
                "init"
                    | "hook_started"
                    | "hook_progress"
                    | "hook_response"
                    | "thinking_tokens"
                    | "rate_limit_event"
            )
        );
        let hook_requires_identity = matches!(subtype, Some("hook_started" | "hook_response"));
        let hook_is_valid = value.get("session_id").and_then(Value::as_str).is_some()
            && value
                .get("hook_id")
                .or_else(|| value.get("hookId"))
                .and_then(Value::as_str)
                .is_some();
        if !known || hook_requires_identity && !hook_is_valid {
            let reason = if known {
                "malformed Claude hook event"
            } else {
                "unsupported Claude system event"
            };
            return (
                EntryKind::Raw,
                reason.to_owned(),
                serde_json::json!({"unsupported.reason": reason, "event.type": subtype}),
            );
        }
    }
    let kind = classify_stdout_value(value);
    let block = value
        .pointer("/message/content")
        .and_then(Value::as_array)
        .and_then(|blocks| {
            blocks.iter().find(|block| match kind {
                EntryKind::Thinking => block["type"] == "thinking",
                EntryKind::ToolCall => block["type"] == "tool_use",
                EntryKind::ToolResult => block["type"] == "tool_result",
                _ => block["type"] == "text",
            })
        });
    let summary = if kind == EntryKind::User {
        block
            .and_then(|b| b.get("text"))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .unwrap_or("user message")
            .to_owned()
    } else {
        project_claude_summary(value, kind)
    };
    let mut details = serde_json::json!({});
    insert_detail(
        &mut details,
        "message.content",
        block
            .and_then(|b| {
                b.get("text")
                    .or_else(|| b.get("thinking"))
                    .or_else(|| b.get("content"))
            })
            .or_else(|| value.get("result")),
    );
    insert_detail(&mut details, "tool.name", block.and_then(|b| b.get("name")));
    insert_detail(
        &mut details,
        "tool.input",
        block.and_then(|b| b.get("input")),
    );
    insert_detail(
        &mut details,
        "tool.output",
        block.and_then(|b| b.get("content")),
    );
    if let Some(blocks) = value
        .pointer("/message/content")
        .and_then(Value::as_array)
        .filter(|blocks| blocks.len() > 1)
    {
        let normalized = blocks
            .iter()
            .map(|block| {
                let mut fields = serde_json::json!({});
                let kind = match block.get("type").and_then(Value::as_str) {
                    Some("text") => "message",
                    Some("thinking") => "thinking",
                    Some("tool_use") => "tool_call",
                    Some("tool_result") => "tool_result",
                    _ => "other",
                };
                fields["kind"] = Value::String(kind.into());
                insert_detail(
                    &mut fields,
                    "message.content",
                    block
                        .get("text")
                        .or_else(|| block.get("thinking"))
                        .or_else(|| block.get("content")),
                );
                insert_detail(&mut fields, "tool.name", block.get("name"));
                insert_detail(&mut fields, "tool.input", block.get("input"));
                insert_detail(&mut fields, "tool.output", block.get("content"));
                fields
            })
            .collect();
        let detail_fields = details.as_object_mut().expect("details object");
        for key in ["message.content", "tool.name", "tool.input", "tool.output"] {
            detail_fields.remove(key);
        }
        detail_fields.insert("message.blocks".into(), Value::Array(normalized));
    }
    insert_detail(&mut details, "event.type", value.get("subtype"));
    // Normalize the harness-specific hook envelope here.  The transcript
    // projection consumes only these neutral fields and keeps `raw` solely
    // for operator inspection.
    insert_detail(&mut details, "group.session_id", value.get("session_id"));
    insert_detail(
        &mut details,
        "group.correlation_id",
        value.get("hook_id").or_else(|| value.get("hookId")),
    );
    let hook_name = value
        .get("hook_name")
        .or_else(|| value.get("hookName"))
        .or_else(|| value.get("hook_event_name"))
        .or_else(|| value.get("hookEventName"))
        .or_else(|| value.get("hook"));
    let group_name = value
        .get("hook_event")
        .or_else(|| value.get("hookEvent"))
        .cloned()
        .or_else(|| {
            hook_name
                .and_then(Value::as_str)
                .and_then(|name| name.split(':').next())
                .map(|name| Value::String(name.to_owned()))
        });
    insert_detail(&mut details, "group.name", group_name.as_ref());
    let group_phase = match value.get("subtype").and_then(Value::as_str) {
        Some("hook_started") => Some(Value::String("started".into())),
        Some("hook_response") => Some(Value::String("completed".into())),
        _ => None,
    };
    insert_detail(&mut details, "group.phase", group_phase.as_ref());
    if value.get("subtype").and_then(Value::as_str) == Some("thinking_tokens") {
        insert_detail(&mut details, "group.session_id", value.get("session_id"));
        details["group.correlation_id"] = Value::String("thinking_tokens".into());
        details["group.name"] = Value::String("tokeny myślenia".into());
        details["group.family"] = Value::String("Techniczne".into());
        details["group.phase"] = Value::String("progress".into());
    }
    // Claude tool calls keep their own opaque id. Normalize it separately
    // from hooks so later projections can correlate a result without parsing
    // the displayed summary.
    if kind == EntryKind::ToolCall {
        insert_detail(&mut details, "group.session_id", value.get("session_id"));
        insert_detail(
            &mut details,
            "group.correlation_id",
            block.and_then(|b| b.get("id")),
        );
        details["group.name"] = Value::String(
            block
                .and_then(|b| b.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_owned(),
        );
        insert_detail(
            &mut details,
            "group.phase",
            Some(&Value::String("started".into())),
        );
        details["group.operation"] = Value::Bool(true);
    } else if kind == EntryKind::ToolResult {
        insert_detail(&mut details, "group.session_id", value.get("session_id"));
        insert_detail(
            &mut details,
            "group.correlation_id",
            block.and_then(|b| b.get("tool_use_id")),
        );
        details["group.name"] = Value::String(
            block
                .and_then(|b| b.get("tool_name").or_else(|| b.get("name")))
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_owned(),
        );
        insert_detail(
            &mut details,
            "group.phase",
            Some(&Value::String("completed".into())),
        );
        details["group.operation"] = Value::Bool(true);
    }
    insert_detail(&mut details, "error.message", value.get("error"));
    (kind, summary, details)
}

fn adapt_opencode(value: &Value) -> (EntryKind, String, Value) {
    let kind = classify_stdout_value(value);
    let summary = project_opencode_summary(value, kind);
    let mut details = serde_json::json!({});
    insert_detail(&mut details, "message.content", value.get("text"));
    insert_detail(&mut details, "tool.name", value.get("name"));
    insert_detail(&mut details, "tool.input", value.pointer("/state/input"));
    insert_detail(&mut details, "tool.output", value.pointer("/state/output"));
    insert_detail(&mut details, "event.type", value.get("type"));
    insert_detail(
        &mut details,
        "error.message",
        value
            .get("message")
            .or_else(|| value.pointer("/error/message")),
    );
    if matches!(kind, EntryKind::ToolCall | EntryKind::ToolResult) {
        let session = value
            .get("sessionID")
            .or_else(|| value.get("session_id"))
            .or_else(|| value.get("sessionId"));
        let correlation = value
            .get("tool_call_id")
            .or_else(|| value.get("toolCallId"))
            .or_else(|| value.get("id"));
        insert_detail(&mut details, "group.session_id", session);
        insert_detail(&mut details, "group.correlation_id", correlation);
        details["group.name"] = Value::String(
            value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_owned(),
        );
        details["group.phase"] = Value::String(
            if kind == EntryKind::ToolResult {
                "completed"
            } else {
                "started"
            }
            .to_owned(),
        );
        details["group.operation"] = Value::Bool(true);
    }
    (kind, summary, details)
}

fn adapt_codex(value: &Value) -> (EntryKind, String, Value) {
    let kind = classify_stdout_value(value);
    let summary = if is_codex_direct_response(value) {
        value
            .get("output")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("agent reply")
            .to_owned()
    } else if is_codex_item_event(value) {
        project_codex_item(value)
    } else {
        project_codex_event(value)
    };
    let item = value.get("item");
    let mut details = serde_json::json!({});
    insert_detail(
        &mut details,
        "message.content",
        item.and_then(|i| i.get("text").or_else(|| i.get("summary")))
            .or_else(|| value.get("output")),
    );
    if matches!(kind, EntryKind::ToolCall | EntryKind::ToolResult) {
        insert_detail(
            &mut details,
            "tool.name",
            item.and_then(|i| i.get("tool_name").or_else(|| i.get("name"))),
        );
    }
    insert_detail(
        &mut details,
        "tool.command",
        item.and_then(|i| i.get("command")),
    );
    insert_detail(
        &mut details,
        "tool.exit_code",
        item.and_then(|i| i.get("exit_code")),
    );
    insert_detail(
        &mut details,
        "tool.output",
        item.and_then(|i| i.get("aggregated_output").or_else(|| i.get("result"))),
    );
    insert_detail(&mut details, "event.type", value.get("type"));
    insert_detail(
        &mut details,
        "error.message",
        item.and_then(|i| i.get("message"))
            .or_else(|| value.pointer("/error/message"))
            .or_else(|| value.get("error")),
    );
    if matches!(kind, EntryKind::ToolCall | EntryKind::ToolResult) {
        let session = value
            .get("thread_id")
            .or_else(|| item.and_then(|item| item.get("thread_id")));
        let correlation = item.and_then(|item| item.get("id").or_else(|| item.get("call_id")));
        insert_detail(&mut details, "group.session_id", session);
        insert_detail(&mut details, "group.correlation_id", correlation);
        details["group.name"] = Value::String(
            item.and_then(|item| item.get("tool_name").or_else(|| item.get("name")))
                .or_else(|| item.and_then(|item| item.get("command")))
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_owned(),
        );
        details["group.phase"] = Value::String(
            if kind == EntryKind::ToolResult {
                "completed"
            } else {
                "started"
            }
            .to_owned(),
        );
        details["group.operation"] = Value::Bool(true);
    }
    (kind, summary, details)
}

fn insert_detail(details: &mut Value, key: &str, value: Option<&Value>) {
    if let Some(value) = value.filter(|value| !value.is_null()) {
        details
            .as_object_mut()
            .expect("details object")
            .insert(key.into(), value.clone());
    }
}

fn project_claude_summary(value: &Value, kind: EntryKind) -> String {
    match kind {
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
            .unwrap_or("assistant message")
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
            .unwrap_or("thinking")
            .to_owned(),
        EntryKind::Result => value
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or("completed")
            .to_owned(),
        EntryKind::System => match value.get("subtype").and_then(Value::as_str) {
            Some("hook_started") => value
                .get("hook_name")
                .or_else(|| value.get("hook_event"))
                .and_then(Value::as_str)
                .map(|name| format!("hook started: {name}"))
                .unwrap_or_else(|| "hook started".to_owned()),
            Some("hook_response") => value
                .get("hook_name")
                .or_else(|| value.get("hook_event"))
                .and_then(Value::as_str)
                .map(|name| format!("hook completed: {name}"))
                .unwrap_or_else(|| "hook completed".to_owned()),
            Some("hook_progress") => value
                .get("hook_name")
                .and_then(Value::as_str)
                .map(|name| format!("hook progress: {name}"))
                .unwrap_or_else(|| "hook progress".to_owned()),
            Some("thinking_tokens") => value
                .get("estimated_tokens_delta")
                .and_then(Value::as_i64)
                .map(|delta| format!("thinking tokens: +{delta}"))
                .or_else(|| {
                    value
                        .get("estimated_tokens")
                        .and_then(Value::as_i64)
                        .map(|total| format!("thinking tokens: {total}"))
                })
                .unwrap_or_else(|| "thinking tokens updated".to_owned()),
            Some(subtype) => value
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("harness event: {subtype}")),
            None => value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("harness event")
                .to_owned(),
        },
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
            .map(|name| format!("calling {name}"))
            .unwrap_or_else(|| "tool call".to_owned()),
        EntryKind::ToolResult => "tool result".to_owned(),
        EntryKind::User => "user message".to_owned(),
        _ => "event".to_owned(),
    }
}

fn project_opencode_summary(value: &Value, kind: EntryKind) -> String {
    match kind {
        EntryKind::Assistant => value
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("assistant message")
            .to_owned(),
        EntryKind::Thinking => value
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("thinking")
            .to_owned(),
        EntryKind::ToolCall => value
            .get("name")
            .and_then(Value::as_str)
            .map(|name| format!("calling {name}"))
            .unwrap_or_else(|| "tool call".to_owned()),
        EntryKind::System => value
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| value.get("type").and_then(Value::as_str))
            .unwrap_or("system event")
            .to_owned(),
        _ => "event".to_owned(),
    }
}

/// True for the outer JSONL envelopes written by `codex exec --json`.
fn is_codex_event(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(Value::as_str),
        Some("thread.started" | "turn.started" | "turn.completed" | "turn.failed")
    )
}

fn is_codex_item_event(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(Value::as_str),
        Some("item.started" | "item.updated" | "item.completed")
    )
}

/// Turn a Codex lifecycle event into a small statement which is useful in a
/// transcript without making the event's JSON its primary presentation.
fn project_codex_event(value: &Value) -> String {
    match value.get("type").and_then(Value::as_str) {
        Some("thread.started") => "thread started".to_owned(),
        Some("turn.started") => "turn started".to_owned(),
        Some("turn.completed") => "turn completed".to_owned(),
        Some("turn.failed") => error_message(value)
            .map(|message| format!("turn failed: {message}"))
            .unwrap_or_else(|| "turn failed".to_owned()),
        _ => "session event".to_owned(),
    }
}

/// Projects Codex items to readable summaries, including sparse items.
fn project_codex_item(value: &Value) -> String {
    let Some(item) = value.get("item") else {
        return "tool event".to_owned();
    };
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let completed = event_type == "item.completed";

    match item.get("type").and_then(Value::as_str).unwrap_or("tool") {
        "agent_message" => item
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("agent message")
            .to_owned(),
        "reasoning" => item
            .get("summary")
            .and_then(Value::as_str)
            .or_else(|| item.get("text").and_then(Value::as_str))
            .unwrap_or("thinking")
            .to_owned(),
        "command_execution" => {
            let command = item
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("command");
            if completed {
                let exit_code = item.get("exit_code").and_then(Value::as_i64);
                match exit_code {
                    Some(code) => format!("command completed: {command} (exit {code})"),
                    None => format!("command completed: {command}"),
                }
            } else {
                format!("running command: {command}")
            }
        }
        "mcp_tool_call" => {
            let tool = item
                .get("tool_name")
                .or_else(|| item.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let name = item
                .get("server")
                .and_then(Value::as_str)
                .map(|server| format!("{server}/{tool}"))
                .unwrap_or_else(|| tool.to_owned());
            if completed {
                format!("MCP tool completed: {name}")
            } else {
                format!("calling MCP tool: {name}")
            }
        }
        "web_search" => {
            let query = item.get("query").and_then(Value::as_str).unwrap_or("query");
            if completed {
                format!("web search completed: {query}")
            } else {
                format!("searching web: {query}")
            }
        }
        "file_change" => {
            if completed {
                "file changes completed".to_owned()
            } else {
                "applying file changes".to_owned()
            }
        }
        "error" => error_message(item)
            .map(|message| format!("error: {message}"))
            .unwrap_or_else(|| "tool error".to_owned()),
        other => format!("tool event: {other}"),
    }
}

fn generic_json_summary(value: &Value) -> String {
    value
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| value.get("error").and_then(Value::as_str))
        .map(str::to_owned)
        .or_else(|| {
            value
                .get("type")
                .and_then(Value::as_str)
                .map(|kind| format!("event: {kind}"))
        })
        .unwrap_or_else(|| "unrecognized event".to_owned())
}

fn error_message(value: &Value) -> Option<&str> {
    value
        .get("error")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/error/message").and_then(Value::as_str))
        .or_else(|| value.get("message").and_then(Value::as_str))
}

fn classify_stdout_value(value: &Value) -> EntryKind {
    let Some(type_field) = value.get("type").and_then(Value::as_str) else {
        return if is_codex_direct_response(value) {
            EntryKind::Assistant
        } else {
            EntryKind::Raw
        };
    };

    match type_field {
        // Claude Code: one full message envelope per line; the message's own
        // content blocks (grouped by `message.id`) carry the finer kind.
        "assistant" => classify_message_content(value).unwrap_or(EntryKind::Assistant),
        "user" => match classify_message_content(value) {
            Some(EntryKind::ToolResult) => EntryKind::ToolResult,
            _ => EntryKind::User,
        },
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
            classify_codex_item(value, type_field).unwrap_or(if type_field == "item.completed" {
                EntryKind::ToolResult
            } else {
                EntryKind::ToolCall
            })
        }

        _ => EntryKind::Raw,
    }
}

/// Codex's final answer can be a direct workflow output object instead of an
/// `exec --json` event envelope. It has no `type`, but still belongs to the
/// assistant turn and therefore must receive the normal agent presentation.
fn is_codex_direct_response(value: &Value) -> bool {
    value.get("output").and_then(Value::as_str).is_some()
        && value.get("data").is_some()
        && value.get("variables").is_some()
        && value.get("artifacts").is_some()
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

/// Adapts one retained agent record into the presentation model shared with
/// conductor and remote conversations. The readable projection remains the
/// default while raw mode can always return to the exact retained bytes.
pub(crate) fn agent_transcript_entry(entry: &TranscriptEvent) -> TranscriptEntry {
    // Narsor's own words are its input to the agent (the user turn) or a
    // workflow event; everything else is named by what the harness sent.
    let workflow_event = entry
        .details
        .get("workflow.event")
        .and_then(serde_json::Value::as_str);
    let (presentation, speaker) = match entry.kind {
        EntryKind::Narsor if workflow_event.is_some_and(|event| event != "agent_input") => {
            (TranscriptPresentation::System, "Narsor")
        }
        EntryKind::Narsor => (TranscriptPresentation::Operator, "Narsor"),
        EntryKind::User => (TranscriptPresentation::Operator, "Operator"),
        EntryKind::Assistant | EntryKind::Result => (TranscriptPresentation::Agent, "Agent"),
        // Stderr frequently contains benign harness progress. It is technical
        // by default; explicit failures are still projected as error events.
        EntryKind::Stderr => (TranscriptPresentation::Technical, "stderr"),
        EntryKind::Thinking | EntryKind::ToolCall | EntryKind::ToolResult | EntryKind::Raw => {
            (TranscriptPresentation::Technical, "Harness")
        }
        EntryKind::System => (TranscriptPresentation::System, "Harness"),
    };
    let mut body: Vec<String> = sanitize(&entry.summary)
        .lines()
        .map(str::to_owned)
        .collect();
    if matches!(
        presentation,
        TranscriptPresentation::Technical | TranscriptPresentation::Error
    ) {
        body.truncate(1);
    }
    let mut transcript = TranscriptEntry::new(
        TranscriptEntryId::from(entry.entry_number.to_string()),
        body,
        entry.raw.clone(),
    )
    .with_header(utterance_header(presentation, speaker, entry.timestamp))
    .with_technical_lines(semantic_detail_lines(&entry.details, &entry.summary))
    .with_presentation(presentation);
    // The adapter has already converted hook envelopes into this neutral
    // correlation tuple; transcript rendering never reads harness fields.
    let group_session = entry
        .details
        .get("group.session_id")
        .and_then(Value::as_str);
    let group_name = entry.details.get("group.name").and_then(Value::as_str);
    let group_phase = entry.details.get("group.phase").and_then(Value::as_str);
    let group_correlation = entry
        .details
        .get("group.correlation_id")
        .and_then(Value::as_str);
    transcript = if entry
        .details
        .get("group.operation")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        transcript.with_correlated_operation(
            group_session,
            group_name,
            group_phase,
            group_correlation,
        )
    } else {
        transcript.with_operation_grouping(
            group_session,
            group_name,
            group_phase,
            group_correlation,
        )
    };
    if let Some(family) = entry.details.get("group.family").and_then(Value::as_str) {
        transcript = transcript.with_group_family(family);
    }
    transcript
}

/// What expanding an entry adds to its summary, as readable text: content
/// fields as their own lines and the identifying fields as `label: value`.
/// Normalized keys and the source payload never appear here — only RAW shows
/// those.
fn semantic_detail_lines(details: &serde_json::Value, summary: &str) -> Vec<String> {
    const CONTENT: [&str; 5] = [
        "message.content",
        "tool.output",
        "error.message",
        "workflow.message",
        "message.blocks",
    ];
    const LABELLED: [(&str, &str); 7] = [
        ("tool.name", "tool"),
        ("tool.command", "command"),
        ("tool.input", "input"),
        ("tool.exit_code", "exit code"),
        ("workflow.step", "step"),
        ("workflow.attempt", "attempt"),
        ("workflow.session", "session"),
    ];
    fn text(value: &serde_json::Value) -> String {
        match value {
            serde_json::Value::String(text) => text.clone(),
            serde_json::Value::Array(values) => {
                values.iter().map(text).collect::<Vec<_>>().join("\n")
            }
            serde_json::Value::Object(values) => {
                values.values().map(text).collect::<Vec<_>>().join("\n")
            }
            serde_json::Value::Null => String::new(),
            other => other.to_string(),
        }
    }
    let mut lines = Vec::new();
    let mut labelled = Vec::new();
    for (key, label) in LABELLED {
        if let Some(value) = details.get(key) {
            let value = text(value);
            if !value.is_empty() {
                labelled.push(format!("{label}: {}", one_line(&value)));
            }
        }
    }
    lines.extend(labelled);
    for key in CONTENT {
        let Some(value) = details.get(key) else {
            continue;
        };
        let sanitized = sanitize(&text(value));
        let mut content = sanitized.lines().peekable();
        // The summary already shows a content's first line.
        if content.peek() == Some(&summary) {
            content.next();
        }
        lines.extend(content.map(str::to_owned));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn entry(source: ConversationEntrySource, raw: &str) -> ConversationEntryDto {
        ConversationEntryDto {
            entry_number: 1,
            timestamp: Utc::now(),
            source,
            raw_base64: BASE64.encode(raw.as_bytes()),
        }
    }

    #[test]
    fn narsor_lines_are_always_the_narsor_kind() {
        let classified = classify(&entry(
            ConversationEntrySource::Narsor,
            "prompt: do the thing",
        ));
        assert_eq!(classified.kind, EntryKind::Narsor);
        assert_eq!(classified.summary, "Workflow sends input to agent");
        assert_eq!(classified.details["workflow.event"], "agent_input");
    }

    #[test]
    fn stderr_lines_are_always_the_stderr_kind() {
        let classified = classify(&entry(
            ConversationEntrySource::Stderr,
            "warning: something",
        ));
        assert_eq!(classified.kind, EntryKind::Stderr);
    }

    #[test]
    fn claude_assistant_text_block_is_the_assistant_kind() {
        let raw =
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"hi"}]}}"#;
        let classified = classify(&entry(ConversationEntrySource::Stdout, raw));
        assert_eq!(classified.kind, EntryKind::Assistant);
    }

    #[test]
    fn claude_assistant_thinking_block_is_the_thinking_kind() {
        let raw = r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"thinking","thinking":""}]}}"#;
        let classified = classify(&entry(ConversationEntrySource::Stdout, raw));
        assert_eq!(classified.kind, EntryKind::Thinking);
    }

    #[test]
    fn claude_assistant_tool_use_block_is_the_tool_call_kind() {
        let raw = r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","name":"bash"}]}}"#;
        let classified = classify(&entry(ConversationEntrySource::Stdout, raw));
        assert_eq!(classified.kind, EntryKind::ToolCall);
    }

    #[test]
    fn claude_user_tool_result_block_is_the_tool_result_kind() {
        let raw =
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#;
        let classified = classify(&entry(ConversationEntrySource::Stdout, raw));
        assert_eq!(classified.kind, EntryKind::ToolResult);
    }

    #[test]
    fn claude_result_line_is_the_result_kind() {
        let raw = r#"{"type":"result","subtype":"success","result":"done"}"#;
        let classified = classify(&entry(ConversationEntrySource::Stdout, raw));
        assert_eq!(classified.kind, EntryKind::Result);
    }

    #[test]
    fn claude_system_init_line_is_the_system_kind() {
        let raw = r#"{"type":"system","subtype":"init","session_id":"abc"}"#;
        let classified = classify(&entry(ConversationEntrySource::Stdout, raw));
        assert_eq!(classified.kind, EntryKind::System);
    }

    #[test]
    fn claude_technical_system_records_have_descriptive_summaries() {
        let hook = classify(&entry(
            ConversationEntrySource::Stdout,
            r#"{"type":"system","subtype":"hook_started","session_id":"s","hook_id":"hook-1","hook_name":"SessionStart:startup"}"#,
        ));
        assert_eq!(hook.summary, "hook started: SessionStart:startup");

        let tokens = classify(&entry(
            ConversationEntrySource::Stdout,
            r#"{"type":"system","subtype":"thinking_tokens","estimated_tokens_delta":42}"#,
        ));
        assert_eq!(tokens.summary, "thinking tokens: +42");
    }

    #[test]
    fn adjacent_claude_thinking_token_updates_form_a_technical_family() {
        let mut first = entry(
            ConversationEntrySource::Stdout,
            r#"{"type":"system","subtype":"thinking_tokens","session_id":"s","estimated_tokens_delta":42}"#,
        );
        first.entry_number = 1;
        let mut second = entry(
            ConversationEntrySource::Stdout,
            r#"{"type":"system","subtype":"thinking_tokens","session_id":"s","estimated_tokens_delta":7}"#,
        );
        second.entry_number = 2;
        let mut transcript = crate::tui::transcript::TranscriptState::new(
            crate::tui::transcript::TranscriptId::from("attempt"),
        );
        transcript.load(vec![
            agent_transcript_entry(&classify(&first)),
            agent_transcript_entry(&classify(&second)),
        ]);

        assert!(transcript
            .logical_lines()
            .iter()
            .any(|line| line.text == "> Techniczne · tokeny myślenia · 2 wpisy"));
    }

    #[test]
    fn malformed_or_unknown_claude_system_records_are_raw() {
        for raw in [
            r#"{"type":"system","subtype":"hook_started","session_id":"s"}"#,
            r#"{"type":"system","subtype":"future_event"}"#,
        ] {
            assert_eq!(
                classify(&entry(ConversationEntrySource::Stdout, raw)).kind,
                EntryKind::Raw,
                "{raw}"
            );
        }
    }

    #[test]
    fn claude_tool_lifecycle_exposes_a_normalized_correlation_tuple() {
        let call = classify(&entry(
            ConversationEntrySource::Stdout,
            r#"{"type":"assistant","session_id":"s","message":{"content":[{"type":"tool_use","id":"tool-1","name":"bash"}]}}"#,
        ));
        assert_eq!(call.details["group.correlation_id"], "tool-1");
        assert_eq!(call.details["group.name"], "bash");
        assert_eq!(call.details["group.phase"], "started");
        assert_eq!(call.details["group.operation"], true);

        let result = classify(&entry(
            ConversationEntrySource::Stdout,
            r#"{"type":"user","session_id":"s","message":{"content":[{"type":"tool_result","tool_use_id":"tool-1","content":"ok"}]}}"#,
        ));
        assert_eq!(result.details["group.correlation_id"], "tool-1");
        assert_eq!(result.details["group.phase"], "completed");
    }

    #[test]
    fn opencode_text_line_is_the_assistant_kind() {
        let raw = r#"{"type":"text","sessionID":"s1","text":"hello"}"#;
        let classified = classify(&entry(ConversationEntrySource::Stdout, raw));
        assert_eq!(classified.kind, EntryKind::Assistant);
    }

    #[test]
    fn opencode_reasoning_line_is_the_thinking_kind() {
        let raw = r#"{"type":"reasoning","sessionID":"s1","text":"because"}"#;
        let classified = classify(&entry(ConversationEntrySource::Stdout, raw));
        assert_eq!(classified.kind, EntryKind::Thinking);
    }

    #[test]
    fn opencode_tool_use_line_is_the_tool_call_kind() {
        let raw = r#"{"type":"tool_use","sessionID":"s1","state":{"input":{}}}"#;
        let classified = classify(&entry(ConversationEntrySource::Stdout, raw));
        assert_eq!(classified.kind, EntryKind::ToolCall);
    }

    #[test]
    fn opencode_step_finish_line_is_the_system_kind() {
        let raw = r#"{"type":"step_finish","sessionID":"s1"}"#;
        let classified = classify(&entry(ConversationEntrySource::Stdout, raw));
        assert_eq!(classified.kind, EntryKind::System);
    }

    #[test]
    fn opencode_error_line_is_the_system_kind() {
        let raw = r#"{"type":"error","sessionID":"s1","message":"boom"}"#;
        let classified = classify(&entry(ConversationEntrySource::Stdout, raw));
        assert_eq!(classified.kind, EntryKind::System);
    }

    #[test]
    fn unknown_type_is_raw_with_full_content() {
        let raw = r#"{"type":"api_retry","detail":"..."}"#;
        let classified = classify(&entry(ConversationEntrySource::Stdout, raw));
        assert_eq!(classified.kind, EntryKind::Raw);
        assert_eq!(classified.summary, "event: api_retry");
        assert_eq!(classified.raw, raw.as_bytes());
    }

    #[test]
    fn non_json_is_raw_with_full_content() {
        let raw = "not json at all";
        let classified = classify(&entry(ConversationEntrySource::Stdout, raw));
        assert_eq!(classified.kind, EntryKind::Raw);
        assert_eq!(classified.summary, raw);
    }

    #[test]
    fn assistant_projection_uses_message_text_while_retaining_exact_source_bytes() {
        let raw = br#"{"type":"assistant","message":{"content":[{"type":"text","text":"A readable reply"}]}}"#;
        let entry = ConversationEntryDto {
            entry_number: 7,
            timestamp: Utc::now(),
            source: ConversationEntrySource::Stdout,
            raw_base64: BASE64.encode(raw),
        };

        let classified = classify(&entry);

        assert_eq!(classified.kind, EntryKind::Assistant);
        assert_eq!(classified.summary, "A readable reply");
        assert_eq!(classified.raw, raw);
    }

    #[test]
    fn malformed_binary_source_is_retained_byte_for_byte() {
        let raw = b"unknown\x00\x1b[31m\xff";
        let entry = ConversationEntryDto {
            entry_number: 8,
            timestamp: Utc::now(),
            source: ConversationEntrySource::Stdout,
            raw_base64: BASE64.encode(raw),
        };

        let classified = classify(&entry);

        assert_eq!(classified.kind, EntryKind::Raw);
        assert_eq!(classified.raw, raw);
    }
}

#[cfg(test)]
mod utterance_tests {
    use super::*;
    use crate::tui::transcript::{DisplayMode, TranscriptId, TranscriptState};
    use base64::engine::general_purpose::STANDARD as BASE64;

    fn entry_from(source: ConversationEntrySource, raw: &str) -> TranscriptEntry {
        agent_transcript_entry(&classify(&ConversationEntryDto {
            entry_number: 1,
            timestamp: chrono::Utc::now(),
            source,
            raw_base64: BASE64.encode(raw.as_bytes()),
        }))
    }

    fn lines_of(entry: TranscriptEntry, expanded: bool) -> Vec<String> {
        let mut transcript = TranscriptState::new(TranscriptId::from("t"));
        let id = entry.id.clone();
        transcript.load(vec![entry]);
        if expanded {
            transcript.toggle_expanded(&id);
        }
        assert_eq!(transcript.mode(), DisplayMode::Rendered);
        transcript
            .logical_lines()
            .into_iter()
            .map(|line| line.text)
            .collect()
    }

    #[test]
    fn claude_hook_adapter_normalizes_session_start_for_grouping() {
        let entries = [
            r#"{"type":"system","subtype":"hook_started","session_id":"session","hook_id":"one","hook_name":"SessionStart:startup"}"#,
            r#"{"type":"system","subtype":"hook_started","session_id":"session","hook_id":"two","hook_name":"SessionStart:startup"}"#,
            r#"{"type":"system","subtype":"hook_response","session_id":"session","hook_id":"two","hook_name":"SessionStart:startup"}"#,
            r#"{"type":"system","subtype":"hook_response","session_id":"session","hook_id":"one","hook_name":"SessionStart:startup"}"#,
        ]
        .into_iter()
        .enumerate()
        .map(|(entry_number, raw)| {
            agent_transcript_entry(&classify(&ConversationEntryDto {
                entry_number: entry_number as u64,
                timestamp: chrono::Utc::now(),
                source: ConversationEntrySource::Stdout,
                raw_base64: BASE64.encode(raw.as_bytes()),
            }))
        })
        .collect();
        let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
        transcript.load(entries);

        assert_eq!(
            transcript
                .logical_lines()
                .iter()
                .filter(|line| line.header)
                .count(),
            1
        );
    }

    /// AE1: each role gets its own header naming role, speaker and date.
    #[test]
    fn every_role_names_itself_the_speaker_and_the_full_date() {
        let cases = [
            (
                ConversationEntrySource::Narsor,
                r#"{"event":"prompt","prompt":"Fix it"}"#,
                "USER · Narsor · ",
            ),
            (
                ConversationEntrySource::Narsor,
                "step started",
                "SYSTEM · Narsor · ",
            ),
            (
                ConversationEntrySource::Stdout,
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Done"}]}}"#,
                "AGENT · Agent · ",
            ),
            (
                ConversationEntrySource::Stdout,
                r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hm"}]}}"#,
                "TECHNICZNE · Harness · ",
            ),
        ];
        for (source, raw, prefix) in cases {
            let header = lines_of(entry_from(source, raw), false).remove(0);
            assert!(header.starts_with(prefix), "{header}");
            // `YYYY-MM-DD HH:MM:SS`
            let date = &header[prefix.len()..];
            assert_eq!(date.len(), 19, "{header}");
            assert_eq!(&date[4..5], "-", "{header}");
            assert_eq!(&date[13..14], ":", "{header}");
        }
    }

    /// AE3: a long Narsor prompt shows its beginning, and expanding shows all
    /// of it with the line breaks — as text, not as the JSON envelope.
    #[test]
    fn a_narsor_prompt_shows_its_start_and_expands_to_the_whole_text() {
        let raw = r#"{"event":"agent_input","prompt":"Implement the feature\nthen run the tests\nand report"}"#;

        let collapsed = lines_of(entry_from(ConversationEntrySource::Narsor, raw), false);
        assert_eq!(collapsed.len(), 2, "{collapsed:?}");
        assert_eq!(collapsed[1], "Implement the feature");

        let expanded = lines_of(entry_from(ConversationEntrySource::Narsor, raw), true);
        assert_eq!(
            expanded[1..],
            ["Implement the feature", "then run the tests", "and report"]
        );
        assert!(
            !expanded.iter().any(|line| line.contains('{')),
            "{expanded:?}"
        );
    }
}
