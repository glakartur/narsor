//! Tests for the TUI's per-harness conversation classification (U7,
//! `src/tui/conversation.rs`).
//!
//! Exercised through the public API type ([`narsor::api::ConversationEntryDto`])
//! rather than the persistence layer's own envelope, because that is the
//! shape the task view actually classifies: whatever
//! [`narsor::api::ApiClient::conversation`] hands back.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use chrono::Utc;

use narsor::api::ConversationEntryDto;
use narsor::persistence::conversation::ConversationSource;
use narsor::tui::conversation::{classify, EntryKind};

fn entry(source: ConversationSource, raw: &str) -> ConversationEntryDto {
    ConversationEntryDto {
        entry_number: 1,
        timestamp: Utc::now(),
        source,
        raw_base64: BASE64.encode(raw.as_bytes()),
    }
}

// --- narsor / stderr, regardless of content --------------------------------

#[test]
fn a_narsor_line_is_the_narsor_kind_even_if_it_looks_like_json() {
    let classified = classify(&entry(ConversationSource::Narsor, r#"{"type":"assistant"}"#));
    assert_eq!(classified.kind, EntryKind::Narsor);
}

#[test]
fn a_stderr_line_is_the_stderr_kind_even_if_it_looks_like_json() {
    let classified = classify(&entry(ConversationSource::Stderr, r#"{"type":"text"}"#));
    assert_eq!(classified.kind, EntryKind::Stderr);
}

// --- Claude Code shapes ------------------------------------------------------

#[test]
fn claude_assistant_text_is_assistant() {
    let raw = r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"hi"}]}}"#;
    assert_eq!(
        classify(&entry(ConversationSource::Stdout, raw)).kind,
        EntryKind::Assistant
    );
}

#[test]
fn claude_assistant_thinking_is_thinking() {
    let raw = r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"thinking","thinking":"..."}]}}"#;
    assert_eq!(
        classify(&entry(ConversationSource::Stdout, raw)).kind,
        EntryKind::Thinking
    );
}

#[test]
fn claude_assistant_tool_use_is_tool_call() {
    let raw = r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","name":"bash","input":{}}]}}"#;
    assert_eq!(
        classify(&entry(ConversationSource::Stdout, raw)).kind,
        EntryKind::ToolCall
    );
}

#[test]
fn claude_user_tool_result_is_tool_result() {
    let raw = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#;
    assert_eq!(
        classify(&entry(ConversationSource::Stdout, raw)).kind,
        EntryKind::ToolResult
    );
}

#[test]
fn claude_result_is_result() {
    let raw = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","session_id":"s1"}"#;
    assert_eq!(
        classify(&entry(ConversationSource::Stdout, raw)).kind,
        EntryKind::Result
    );
}

#[test]
fn claude_system_init_is_system() {
    let raw = r#"{"type":"system","subtype":"init","session_id":"s1"}"#;
    assert_eq!(
        classify(&entry(ConversationSource::Stdout, raw)).kind,
        EntryKind::System
    );
}

// --- opencode shapes ---------------------------------------------------------

#[test]
fn opencode_text_is_assistant() {
    let raw = r#"{"type":"text","timestamp":0,"sessionID":"s1","text":"hi"}"#;
    assert_eq!(
        classify(&entry(ConversationSource::Stdout, raw)).kind,
        EntryKind::Assistant
    );
}

#[test]
fn opencode_reasoning_is_thinking() {
    let raw = r#"{"type":"reasoning","timestamp":0,"sessionID":"s1","text":"because"}"#;
    assert_eq!(
        classify(&entry(ConversationSource::Stdout, raw)).kind,
        EntryKind::Thinking
    );
}

#[test]
fn opencode_tool_use_is_tool_call() {
    let raw = r#"{"type":"tool_use","timestamp":0,"sessionID":"s1","state":{"input":{},"output":"ok"}}"#;
    assert_eq!(
        classify(&entry(ConversationSource::Stdout, raw)).kind,
        EntryKind::ToolCall
    );
}

#[test]
fn opencode_step_finish_is_system() {
    let raw = r#"{"type":"step_finish","timestamp":0,"sessionID":"s1"}"#;
    assert_eq!(
        classify(&entry(ConversationSource::Stdout, raw)).kind,
        EntryKind::System
    );
}

#[test]
fn opencode_error_is_system() {
    let raw = r#"{"type":"error","timestamp":0,"sessionID":"s1","message":"boom"}"#;
    assert_eq!(
        classify(&entry(ConversationSource::Stdout, raw)).kind,
        EntryKind::System
    );
}

// --- fallback ----------------------------------------------------------------

#[test]
fn an_unrecognized_type_is_raw_with_full_content_preserved() {
    let raw = r#"{"type":"api_retry","detail":"backing off"}"#;
    let classified = classify(&entry(ConversationSource::Stdout, raw));
    assert_eq!(classified.kind, EntryKind::Raw);
    assert_eq!(classified.text, raw, "R9: nothing is dropped, even unrecognized");
}

#[test]
fn non_json_content_is_raw_with_full_content_preserved() {
    let raw = "Traceback (most recent call last): boom";
    let classified = classify(&entry(ConversationSource::Stdout, raw));
    assert_eq!(classified.kind, EntryKind::Raw);
    assert_eq!(classified.text, raw);
}

#[test]
fn json_with_no_type_field_is_raw() {
    let raw = r#"{"foo":"bar"}"#;
    let classified = classify(&entry(ConversationSource::Stdout, raw));
    assert_eq!(classified.kind, EntryKind::Raw);
}
