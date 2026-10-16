//! Tests for the TUI's conversation projection (`src/tui/conversation.rs`).
//!
//! Exercised through the public API type ([`narsor::api::ConversationEntryDto`])
//! because that is what the task view projects: the chat elements the server
//! sends next to each raw line. The projection never reads the raw line.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use chrono::Utc;
use serde_json::json;

use narsor::api::{ConversationEntryDto, ConversationEntrySource as ConversationSource};
use narsor::tui::conversation::{classify, transcript_entries, EntryKind};
use narsor::tui::transcript::{
    DisplayMode, TranscriptEntry, TranscriptId, TranscriptPresentation, TranscriptState,
};
use narsor_protocol::chat::{
    ChatElement, PromptPurpose, QuestionSource, TechnicalCategory, ToolResultStatus,
    UnclassifiedReason,
};

fn entry(number: u64, raw: &str, elements: Vec<ChatElement>) -> ConversationEntryDto {
    ConversationEntryDto {
        entry_number: number,
        timestamp: Utc::now(),
        source: ConversationSource::Stdout,
        raw_base64: BASE64.encode(raw.as_bytes()),
        elements,
    }
}

fn agent_text(text: &str) -> ChatElement {
    ChatElement::AgentText {
        text: text.to_string(),
    }
}

fn tool_call(id: &str, tool: &str, summary: &str, input: serde_json::Value) -> ChatElement {
    ChatElement::ToolCall {
        call_id: Some(id.to_string()),
        tool: tool.to_string(),
        summary: summary.to_string(),
        input,
    }
}

/// The logical lines of `entries` in `mode`, every card expanded when
/// `expand`.
fn lines(entries: Vec<TranscriptEntry>, mode: DisplayMode, expand: bool) -> Vec<String> {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
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

/// An entry with an agent text and a tool call gives two events, each with
/// its own label.
#[test]
fn agent_text_and_tool_call_in_one_entry_are_two_events_with_their_labels() {
    let entry = entry(
        4,
        r#"{"anything":"the harness wrote"}"#,
        vec![
            agent_text("Let me look."),
            tool_call("c1", "Read", "notes.txt", json!({"file_path": "notes.txt"})),
        ],
    );
    let events = classify(&entry);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].kind, EntryKind::AgentText);
    assert_eq!(events[0].kind.label(), "agent");
    assert_eq!(events[0].summary, "Let me look.");
    assert_eq!(events[1].kind, EntryKind::ToolCall);
    assert_eq!(events[1].kind.label(), "tool");
    assert_eq!(events[1].summary, "Read notes.txt");
    assert!(events.iter().all(|event| event.entry_number == 4));

    let cards = transcript_entries(&entry);
    assert_eq!(cards.len(), 2);
    assert!(
        cards[0].header.contains("AGENT · Agent · agent · "),
        "{}",
        cards[0].header
    );
    assert!(
        cards[1].header.contains(" · tool · "),
        "{}",
        cards[1].header
    );
    assert_eq!(cards[1].rendered_lines, ["Read notes.txt"]);
}

/// AE1: a `Skill` call names the skill in its description.
#[test]
fn a_skill_call_names_the_skill() {
    let events = classify(&entry(
        1,
        "{}",
        vec![tool_call(
            "c1",
            "Skill",
            "ce-plan",
            json!({"skill": "ce-plan"}),
        )],
    ));
    assert!(
        events[0].summary.contains("ce-plan"),
        "{}",
        events[0].summary
    );
    let cards = transcript_entries(&entry(
        1,
        "{}",
        vec![tool_call(
            "c1",
            "Skill",
            "ce-plan",
            json!({"skill": "ce-plan"}),
        )],
    ));
    let rendered = lines(cards, DisplayMode::Rendered, false);
    assert!(
        rendered.iter().any(|line| line.contains("Skill ce-plan")),
        "{rendered:?}"
    );
}

/// A tool call's expansion shows its full input.
#[test]
fn a_tool_call_expands_to_its_full_input() {
    let cards = transcript_entries(&entry(
        1,
        "{}",
        vec![ChatElement::ToolCall {
            call_id: None,
            tool: "Bash".to_string(),
            summary: "git status".to_string(),
            input: json!({"command": "git status", "description": "Show state"}),
        }],
    ));
    let expanded = lines(cards, DisplayMode::Rendered, true);
    assert!(expanded.iter().any(|line| line == "input:"), "{expanded:?}");
    assert!(
        expanded
            .iter()
            .any(|line| line.contains("\"description\": \"Show state\"")),
        "{expanded:?}"
    );
}

/// An agent question has its own label, not the agent text one, and shows
/// its options.
#[test]
fn an_agent_question_has_its_own_label() {
    let entry = entry(
        2,
        "{}",
        vec![ChatElement::AgentQuestion {
            question: "Which database?".to_string(),
            options: vec!["Postgres".to_string(), "SQLite".to_string()],
            source: QuestionSource::OutputContract,
            call_id: None,
        }],
    );
    let events = classify(&entry);
    assert_eq!(events[0].kind, EntryKind::AgentQuestion);
    assert_eq!(events[0].kind.label(), "question");
    assert_ne!(events[0].kind.label(), EntryKind::AgentText.label());

    let card = &transcript_entries(&entry)[0];
    assert!(card.header.contains(" · question · "), "{}", card.header);
    assert_eq!(
        card.rendered_lines,
        ["Which database?", "• Postgres", "• SQLite"]
    );
}

/// A failed tool result names the tool and the call and is marked as an
/// error, also without the call on the same page.
#[test]
fn an_error_tool_result_shows_the_tool_and_the_error_status() {
    let entry = entry(
        9,
        "{}",
        vec![ChatElement::ToolResult {
            call_id: Some("c-elsewhere".to_string()),
            tool: Some("Bash".to_string()),
            call_summary: Some("cat missing.txt".to_string()),
            status: ToolResultStatus::Error,
            content: "cat: missing.txt: No such file or directory".to_string(),
            truncated: true,
        }],
    );
    let events = classify(&entry);
    assert_eq!(events[0].kind, EntryKind::ToolResult);
    assert_eq!(
        events[0].summary,
        "Bash cat missing.txt — error: cat: missing.txt: No such file or directory"
    );

    let cards = transcript_entries(&entry);
    assert_eq!(cards[0].presentation, TranscriptPresentation::Error);
    let expanded = lines(cards, DisplayMode::Rendered, true);
    assert!(
        expanded
            .iter()
            .any(|line| line.contains("Bash cat missing.txt — error")),
        "{expanded:?}"
    );
    assert!(
        expanded
            .iter()
            .any(|line| line.contains("podgląd skrócony")),
        "a cut preview says so: {expanded:?}"
    );
}

/// A successful result is not an error.
#[test]
fn an_ok_tool_result_is_technical_with_its_status() {
    let entry = entry(
        3,
        "{}",
        vec![ChatElement::ToolResult {
            call_id: None,
            tool: Some("Read".to_string()),
            call_summary: Some("notes.txt".to_string()),
            status: ToolResultStatus::Ok,
            content: "alpha".to_string(),
            truncated: false,
        }],
    );
    let card = &transcript_entries(&entry)[0];
    assert_eq!(card.presentation, TranscriptPresentation::Technical);
    assert_eq!(card.rendered_lines, ["Read notes.txt — ok"]);
}

/// An element kind from a newer version is unclassified and renders.
#[test]
fn an_unrecognized_element_is_unclassified_and_renders() {
    let wire = r#"{"entry_number":5,"timestamp":"2026-10-17T10:00:00Z","source":"stdout","raw_base64":"e30=","elements":[{"kind":"hologram","beam":1},{"kind":"agent_text","text":"after"}]}"#;
    let entry: ConversationEntryDto = serde_json::from_str(wire).expect("a newer page");
    let events = classify(&entry);
    assert_eq!(events[0].element, ChatElement::Unrecognized);
    assert_eq!(events[0].kind, EntryKind::Unclassified);
    assert_eq!(events[0].kind.label(), "unclassified");

    let rendered = lines(transcript_entries(&entry), DisplayMode::Rendered, true);
    assert!(
        rendered
            .iter()
            .any(|line| line.contains(" · unclassified · ")),
        "{rendered:?}"
    );
    assert!(rendered.iter().any(|line| line == "after"), "{rendered:?}");
}

/// An unclassified line keeps its reason and shows its text when expanded.
#[test]
fn an_unclassified_line_names_the_reason() {
    let entry = entry(
        1,
        "not json at all",
        vec![ChatElement::Unclassified {
            reason: UnclassifiedReason::InvalidJson,
        }],
    );
    let cards = transcript_entries(&entry);
    assert_eq!(cards[0].rendered_lines, ["not valid JSON"]);
    let expanded = lines(cards, DisplayMode::Rendered, true);
    assert!(
        expanded.iter().any(|line| line == "not json at all"),
        "{expanded:?}"
    );
}

/// Raw mode shows the source line of an entry with three elements once.
#[test]
fn raw_mode_shows_one_line_for_an_entry_with_three_elements() {
    let raw = r#"{"type":"assistant","three":"blocks"}"#;
    let cards: Vec<TranscriptEntry> = [
        entry(
            1,
            raw,
            vec![
                ChatElement::Thinking {
                    text: "hm".to_string(),
                    redacted: false,
                },
                agent_text("Reading."),
                tool_call("c1", "Read", "a.txt", json!({})),
            ],
        ),
        entry(2, "second line", vec![agent_text("Done.")]),
    ]
    .iter()
    .flat_map(transcript_entries)
    .collect();
    assert_eq!(cards.len(), 4);

    let shown = lines(cards.clone(), DisplayMode::Raw, false);
    let raw_lines: Vec<_> = shown.iter().filter(|line| !line.is_empty()).collect();
    assert_eq!(raw_lines, [raw, "second line"], "{shown:?}");

    let chronology = lines(cards, DisplayMode::Chronology, false);
    assert_eq!(
        chronology
            .iter()
            .filter(|line| line.contains(" · Harness · ") || line.contains(" · Agent · "))
            .count(),
        4,
        "every element keeps its card outside raw mode: {chronology:?}"
    );
}

/// Prompts, process outcomes and stderr carry the speaker Narsor names.
#[test]
fn narsor_and_stderr_entries_have_their_speakers() {
    let mut prompt = entry(
        1,
        "prompt: fix it",
        vec![ChatElement::Prompt {
            text: "Fix it".to_string(),
            resumed_session: Some("s-1".to_string()),
            purpose: PromptPurpose::Step,
        }],
    );
    prompt.source = ConversationSource::Narsor;
    let mut stderr = entry(
        2,
        "warning",
        vec![ChatElement::Technical {
            category: TechnicalCategory::Stderr,
            summary: "warning".to_string(),
            data: None,
        }],
    );
    stderr.source = ConversationSource::Stderr;
    let process = entry(
        3,
        "exit",
        vec![ChatElement::Technical {
            category: TechnicalCategory::Process,
            summary: "harness exited with code 0".to_string(),
            data: None,
        }],
    );
    let cards: Vec<_> = [prompt, stderr, process]
        .iter()
        .flat_map(transcript_entries)
        .collect();
    assert!(
        cards[0].header.starts_with("USER · Narsor · sent · "),
        "{}",
        cards[0].header
    );
    assert_eq!(cards[0].rendered_lines, ["Fix it"]);
    assert!(
        cards[1]
            .header
            .starts_with("TECHNICZNE · stderr · technical · "),
        "{}",
        cards[1].header
    );
    assert_eq!(cards[1].rendered_lines, ["stderr: warning"]);
    assert!(
        cards[2]
            .header
            .starts_with("SYSTEM · Narsor · technical · "),
        "{}",
        cards[2].header
    );

    let expanded = lines(cards, DisplayMode::Rendered, true);
    assert!(
        expanded.iter().any(|line| line == "session: s-1"),
        "{expanded:?}"
    );
}

/// The final result speaks its text; its metadata is in the expansion.
#[test]
fn a_run_result_shows_its_text_and_expands_to_its_metadata() {
    let entry = entry(
        1,
        "{}",
        vec![ChatElement::RunResult {
            text: Some("All done.".to_string()),
            structured_output: Some(json!({"output": "done"})),
            stop_reason: Some("end_turn".to_string()),
            is_error: false,
            cost_usd: Some(0.5),
            usage: None,
            turns: Some(3),
            duration_ms: Some(1500),
        }],
    );
    let cards = transcript_entries(&entry);
    assert!(
        cards[0].header.contains(" · result · "),
        "{}",
        cards[0].header
    );
    assert_eq!(cards[0].rendered_lines, ["All done."]);
    let expanded = lines(cards, DisplayMode::Rendered, true);
    assert!(
        expanded
            .iter()
            .any(|line| line == "done · end_turn · 3 turns · 1.5s · $0.5000"),
        "{expanded:?}"
    );
    assert!(
        expanded
            .iter()
            .any(|line| line.contains("\"output\": \"done\"")),
        "{expanded:?}"
    );
}

// --- the conductor's chat (U13) ----------------------------------------------

mod conductor_chat {
    use narsor::api::{
        ChatAuthorDto as ChatAuthor, ChatEntryDto as ChatEntry, ChatEntryKindDto as ChatEntryKind,
    };
    use narsor::domain::Confidence;
    use narsor::tui::chat::{chat_items, tab_entry, ProposalState, UndoState};
    use narsor::tui::transcript::TranscriptPresentation;
    use serde_json::{json, Value};

    fn entry(
        sequence: u64,
        author: ChatAuthor,
        kind: ChatEntryKind,
        text: &str,
        payload: Value,
    ) -> ChatEntry {
        ChatEntry {
            id: String::new(),
            sequence,
            at: chrono::Utc::now(),
            author,
            kind,
            text: text.to_string(),
            in_reply_to: None,
            payload: Some(payload),
        }
    }

    fn proposal(sequence: u64) -> ChatEntry {
        entry(
            sequence,
            ChatAuthor::Conductor,
            ChatEntryKind::Proposal,
            "Approve it",
            json!({
                "episode": "gate-1",
                "proposal": {"kind": "answer_input", "label": "approved"},
                "confidence": "high",
                "rationale": "It is ready.",
            }),
        )
    }

    fn choice(sequence: u64) -> ChatEntry {
        entry(
            sequence,
            ChatAuthor::Conductor,
            ChatEntryKind::Decision,
            "Chose 'implemented'",
            json!({
                "decision_id": "decision-1",
                "kind": "choose_output",
                "output_label": "implemented",
                "confidence": "low",
                "rationale": "One answer says so.",
            }),
        )
    }

    #[test]
    fn a_proposal_is_open_until_a_later_entry_settles_it() {
        let open = proposal(1);
        let items = chat_items(std::slice::from_ref(&open));
        assert_eq!(items[0].proposal, Some(ProposalState::Open));
        assert_eq!(items[0].open_proposal(), Some(open.id.as_str()));
        assert_eq!(items[0].label.as_deref(), Some("approved"));
        assert_eq!(items[0].confidence, Some(Confidence::High));
        let lines = tab_entry(&items[0], None).rendered_lines;
        assert!(lines
            .iter()
            .any(|line| line.contains("(propozycja: czeka na Ciebie")));
        assert!(lines
            .iter()
            .any(|line| line.contains("dlaczego: It is ready.")));

        let rejected = entry(
            2,
            ChatAuthor::Operator,
            ChatEntryKind::Decision,
            "Rejected",
            json!({"kind": "proposal_rejected", "proposal_id": open.id}),
        );
        let items = chat_items(&[open.clone(), rejected]);
        assert_eq!(items[0].proposal, Some(ProposalState::Rejected));
        assert_eq!(items[0].open_proposal(), None);

        let executed = entry(
            2,
            ChatAuthor::Conductor,
            ChatEntryKind::Decision,
            "Done",
            json!({"kind": "execute_proposal", "proposal_id": open.id, "decision_id": "d"}),
        );
        let items = chat_items(&[open, executed]);
        assert_eq!(items[0].proposal, Some(ProposalState::Executed));
        assert_eq!(items[1].undo, None, "a carried-out proposal is no choice");
    }

    #[test]
    fn only_a_chosen_output_not_undone_yet_can_be_undone() {
        let items = chat_items(&[choice(1)]);
        assert_eq!(items[0].undoable_decision(), Some("decision-1"));

        let undone = entry(
            2,
            ChatAuthor::Operator,
            ChatEntryKind::Decision,
            "Undid it",
            json!({"kind": "decision_undone", "decision_id": "decision-1"}),
        );
        let items = chat_items(&[choice(1), undone]);
        assert_eq!(items[0].undo, Some(UndoState::Undone));
        assert!(tab_entry(&items[0], None)
            .rendered_lines
            .iter()
            .any(|line| line.contains("(decyzja cofnięta)")));
    }

    #[test]
    fn the_tab_gives_the_operator_and_the_conductor_their_own_presentation() {
        let operator = chat_items(&[entry(
            1,
            ChatAuthor::Operator,
            ChatEntryKind::Message,
            "please check",
            json!({}),
        )]);
        let conductor = chat_items(&[entry(
            2,
            ChatAuthor::Conductor,
            ChatEntryKind::Message,
            "checking",
            json!({}),
        )]);

        assert_eq!(
            tab_entry(&operator[0], None).presentation,
            TranscriptPresentation::Operator
        );
        assert_eq!(
            tab_entry(&conductor[0], None).presentation,
            TranscriptPresentation::Conductor
        );
    }
}

/// A failed read of a conversation page, as the tab tells it (U7 of the
/// automatic resume): the record that could not be read, a conversation no
/// one can serve, anything else.
mod read_failures {
    use narsor::api::ClientError;
    use narsor::tui::task_app::tabs::conversation::ReadFailure;

    fn refused(code: &str, message: &str) -> ClientError {
        ClientError::Refused {
            code: code.to_string(),
            message: message.to_string(),
        }
    }

    #[test]
    fn an_unreadable_record_keeps_only_the_error() {
        let error = refused(
            "conversation_record_unreadable",
            "the conversation record could not be read: permission denied",
        );
        let failure = ReadFailure::of(&error, "words".to_string());
        assert_eq!(
            failure,
            ReadFailure::RecordUnreadable("permission denied".to_string())
        );
        assert_eq!(
            failure.note(None).0,
            "✗ nie udało się odczytać zapisu: permission denied"
        );
    }

    #[test]
    fn an_unavailable_conversation_says_so_with_the_reason() {
        let error = refused(
            "conversation_unavailable",
            "the task's conversation cannot be read right now (Interrupted)",
        );
        let failure = ReadFailure::of(&error, "the reason".to_string());
        assert_eq!(failure.note(None).0, "◌ rozmowa niedostępna: the reason");
    }

    #[test]
    fn any_other_failure_names_its_step_when_it_stands_for_several() {
        let failure = ReadFailure::of(&ClientError::Conflict, "conflict".to_string());
        assert_eq!(
            failure.note(Some("plan #1")).0,
            "✗ nie udało się wczytać rozmowy (plan #1: conflict)"
        );
        assert_eq!(
            failure.note(None).0,
            "✗ nie udało się wczytać rozmowy: conflict"
        );
    }
}
