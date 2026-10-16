//! The lines `narsor conversation` and `narsor task conversation` print for a
//! conversation entry (`narsor::cli::commands::conversation_entry_lines`):
//! one line per chat element, with the same label and summary the TUI shows.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use chrono::{TimeZone, Utc};
use serde_json::json;

use narsor::cli::commands::conversation_entry_lines;
use narsor::tui::conversation::classify;
use narsor_protocol::api::{ConversationEntryDto, ConversationEntrySource};
use narsor_protocol::chat::{ChatElement, PromptPurpose, ToolResultStatus};

fn entry(
    source: ConversationEntrySource,
    raw: &[u8],
    elements: Vec<ChatElement>,
) -> ConversationEntryDto {
    ConversationEntryDto {
        entry_number: 7,
        timestamp: Utc.with_ymd_and_hms(2026, 10, 16, 12, 0, 0).unwrap(),
        source,
        raw_base64: BASE64.encode(raw),
        elements,
    }
}

fn bash_call() -> ChatElement {
    ChatElement::ToolCall {
        call_id: Some("toolu_1".to_string()),
        tool: "Bash".to_string(),
        summary: "cargo test".to_string(),
        input: json!({"command": "cargo test"}),
    }
}

#[test]
fn a_bash_call_prints_the_tool_label_and_its_command() {
    let lines = conversation_entry_lines(&entry(
        ConversationEntrySource::Stdout,
        b"{}",
        vec![bash_call()],
    ));

    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("tool"), "{}", lines[0]);
    assert!(lines[0].contains("Bash cargo test"), "{}", lines[0]);
}

#[test]
fn every_element_of_one_entry_gets_its_own_line_in_order() {
    let lines = conversation_entry_lines(&entry(
        ConversationEntrySource::Stdout,
        b"{}",
        vec![
            ChatElement::AgentText {
                text: "Running the tests.".to_string(),
            },
            bash_call(),
            ChatElement::ToolResult {
                call_id: Some("toolu_1".to_string()),
                tool: Some("Bash".to_string()),
                call_summary: Some("cargo test".to_string()),
                status: ToolResultStatus::Error,
                content: "1 failed".to_string(),
                truncated: false,
            },
        ],
    ));

    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(lines[0].contains("Running the tests."), "{}", lines[0]);
    assert!(lines[1].contains("Bash cargo test"), "{}", lines[1]);
    assert!(lines[2].contains("error"), "{}", lines[2]);
    assert!(lines.iter().all(|line| line.starts_with("    7  ")));
}

#[test]
fn an_unrecognized_element_prints_as_unclassified() {
    let element: ChatElement =
        serde_json::from_value(json!({"kind": "future_thing", "x": 1})).unwrap();
    let lines = conversation_entry_lines(&entry(
        ConversationEntrySource::Stdout,
        b"{}",
        vec![element],
    ));

    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("unclassified"), "{}", lines[0]);
}

#[test]
fn a_narsor_entry_prints_its_own_text_once() {
    let raw = br#"{"event":"prompt","prompt":"do the thing"}"#;
    let lines = conversation_entry_lines(&entry(
        ConversationEntrySource::Narsor,
        raw,
        vec![ChatElement::Prompt {
            text: "do the thing".to_string(),
            resumed_session: None,
            purpose: PromptPurpose::Step,
        }],
    ));

    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("sent"), "{}", lines[0]);
    assert!(
        lines[0].contains(std::str::from_utf8(raw).unwrap()),
        "{}",
        lines[0]
    );
}

#[test]
fn the_cli_and_the_tui_agree_on_label_and_summary() {
    let entry = entry(ConversationEntrySource::Stdout, b"{}", vec![bash_call()]);
    let line = &conversation_entry_lines(&entry)[0];
    let event = &classify(&entry)[0];

    assert!(line.contains(event.kind.label()), "{line}");
    assert!(line.contains(&event.summary), "{line}");
}
