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

mod environment_support;
mod remote_support;

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
    let classified = classify(&entry(
        ConversationSource::Narsor,
        r#"{"type":"assistant"}"#,
    ));
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
    let raw =
        r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"hi"}]}}"#;
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
    let raw =
        r#"{"type":"tool_use","timestamp":0,"sessionID":"s1","state":{"input":{},"output":"ok"}}"#;
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

// --- Codex JSONL shapes -----------------------------------------------------

#[test]
fn codex_messages_reasoning_tools_results_and_errors_are_distinguished() {
    let cases = [
        (
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"hi"}}"#,
            EntryKind::Assistant,
        ),
        (
            r#"{"type":"item.updated","item":{"type":"reasoning","text":"because"}}"#,
            EntryKind::Thinking,
        ),
        (
            r#"{"type":"item.started","item":{"type":"command_execution","command":"git status"}}"#,
            EntryKind::ToolCall,
        ),
        (
            r#"{"type":"item.completed","item":{"type":"mcp_tool_call","status":"completed"}}"#,
            EntryKind::ToolResult,
        ),
        (r#"{"type":"turn.completed","usage":{}}"#, EntryKind::Result),
        (
            r#"{"type":"turn.failed","error":"boom"}"#,
            EntryKind::System,
        ),
    ];

    for (raw, expected) in cases {
        assert_eq!(
            classify(&entry(ConversationSource::Stdout, raw)).kind,
            expected
        );
    }
}

// --- fallback ----------------------------------------------------------------

#[test]
fn an_unrecognized_type_is_raw_with_full_content_preserved() {
    let raw = r#"{"type":"api_retry","detail":"backing off"}"#;
    let classified = classify(&entry(ConversationSource::Stdout, raw));
    assert_eq!(classified.kind, EntryKind::Raw);
    assert_eq!(
        classified.text, raw,
        "R9: nothing is dropped, even unrecognized"
    );
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

/// AE1/AE3: rendered mode has a concise semantic body for every supported
/// harness role, while the diagnostic source remains byte-for-byte available.
#[test]
fn semantic_projection_keeps_the_complete_source_for_diagnostics() {
    let cases = [
        (
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Readable reply"}]}}"#,
            EntryKind::Assistant,
            "Readable reply",
        ),
        (
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"Check the state"}]}}"#,
            EntryKind::Thinking,
            "Check the state",
        ),
        (
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"bash","input":{"command":"git status"}}]}}"#,
            EntryKind::ToolCall,
            "calling bash",
        ),
        (
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"clean"}]}}"#,
            EntryKind::ToolResult,
            "tool result",
        ),
        (
            r#"{"type":"result","subtype":"success","result":"Finished"}"#,
            EntryKind::Result,
            "Finished",
        ),
        (
            r#"{"type":"api_retry","detail":"backing off"}"#,
            EntryKind::Raw,
            r#"{"type":"api_retry","detail":"backing off"}"#,
        ),
    ];

    for (raw, kind, rendered) in cases {
        let classified = classify(&entry(ConversationSource::Stdout, raw));
        assert_eq!(classified.kind, kind, "{raw}");
        assert_eq!(classified.text, rendered, "{raw}");
        assert_eq!(
            classified.raw,
            raw.as_bytes(),
            "raw mode must remain lossless"
        );
    }
}

// --- the conductor's chat (U13) ----------------------------------------------

mod conductor_chat {
    use narsor::conductor::{ChatAuthor, ChatEntry, ChatEntryKind};
    use narsor::domain::Confidence;
    use narsor::tui::chat::{
        chat_item_lines, chat_items, ChatActionTarget, ChatPanel, ProposalState, UndoState,
    };
    use narsor::tui::transcript::TranscriptPresentation;
    use serde_json::{json, Value};

    fn entry(
        sequence: u64,
        author: ChatAuthor,
        kind: ChatEntryKind,
        text: &str,
        payload: Value,
    ) -> ChatEntry {
        let mut entry = ChatEntry::new(author, kind, text);
        entry.sequence = sequence;
        entry.payload = Some(payload);
        entry
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
        let lines = chat_item_lines(&items[0]);
        assert!(lines.iter().any(|line| line.contains("(proposal, open)")));
        assert!(lines.iter().any(|line| line.contains("why: It is ready.")));

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
        assert!(chat_item_lines(&items[0])
            .iter()
            .any(|line| line.contains("(undone)")));
    }

    #[test]
    fn the_panel_appends_only_newer_entries_and_keeps_its_draft() {
        let mut panel = ChatPanel {
            draft: "half a message".to_string(),
            ..ChatPanel::default()
        };
        panel.append(vec![proposal(1), choice(2)]);
        assert_eq!(panel.selected, 1, "the selection follows the end");
        panel.append(vec![choice(2), proposal(3)]);
        assert_eq!(panel.entries.len(), 3, "an entry held already is skipped");
        assert_eq!(panel.last_sequence(), 3);
        assert_eq!(panel.draft, "half a message");

        panel.selected = 1;
        assert!(panel.ask_undo());
        let undo = panel.undo.as_ref().expect("the confirmation");
        assert_eq!(undo.label.as_deref(), Some("implemented"));
        assert_eq!(undo.confidence, Some(Confidence::Low));
        assert_eq!(undo.rationale.as_deref(), Some("One answer says so."));
        panel.undo = None;
        panel.selected = 0;
        assert!(!panel.ask_undo(), "a proposal is no decision to undo");
    }

    #[test]
    fn conductor_transcript_keeps_operator_and_agent_presentation() {
        let mut panel = ChatPanel::default();
        panel.append(vec![
            entry(
                1,
                ChatAuthor::Operator,
                ChatEntryKind::Message,
                "please check",
                json!({}),
            ),
            entry(
                2,
                ChatAuthor::Conductor,
                ChatEntryKind::Message,
                "checking",
                json!({}),
            ),
        ]);

        assert_eq!(
            panel.transcript.entries()[0].presentation,
            TranscriptPresentation::Operator
        );
        assert_eq!(
            panel.transcript.entries()[1].presentation,
            TranscriptPresentation::Agent
        );
    }

    #[test]
    fn transcript_reading_and_action_target_survive_new_entries_independently() {
        let mut panel = ChatPanel::default();
        let entries = (1..=30).map(proposal).collect();
        panel.append(entries);
        assert!(panel.transcript.is_following_end());
        panel.line_up();
        let anchor = panel.transcript.anchor().expect("older reading anchor");
        assert!(!panel.transcript.is_following_end());

        panel.cycle_action_target(0);
        let target = panel.action_target.clone().expect("open proposal target");
        assert!(matches!(target, ChatActionTarget::Proposal(_)));

        panel.append(vec![proposal(31)]);
        assert_eq!(panel.action_target, Some(target));
        assert_eq!(panel.transcript.anchor().expect("restored anchor"), anchor);
        assert!(!panel.transcript.is_following_end());
    }

    #[test]
    fn refresh_clears_a_settled_action_target_without_moving_older_reading() {
        let mut panel = ChatPanel::default();
        panel.append((1..=30).map(proposal).collect());
        panel.line_up();
        let anchor = panel.transcript.anchor().expect("older reading anchor");
        panel.cycle_action_target(0);
        let ChatActionTarget::Proposal(proposal_id) = panel
            .action_target
            .clone()
            .expect("an open proposal target")
        else {
            panic!("the target is a proposal");
        };

        panel.append(vec![entry(
            31,
            ChatAuthor::Operator,
            ChatEntryKind::Decision,
            "Rejected the proposal",
            json!({"kind": "proposal_rejected", "proposal_id": proposal_id}),
        )]);

        assert!(panel.action_target.is_none(), "a stale target cannot act");
        assert_eq!(panel.transcript.anchor(), Some(anchor));
        assert!(
            !panel.transcript.is_following_end(),
            "a refresh must not pull an operator out of older context"
        );
    }
}

// --- AE12: a remote conversation and its input, through the control plane --

mod remote_conversation {
    use std::time::Duration;

    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine as _;
    use chrono::Utc;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use narsor::api::{ConversationEntryDto, ConversationResponse};
    use narsor::controller_protocol::RunnerClient;
    use narsor::persistence::conversation::ConversationSource;
    use narsor::tui::conversation::EntryKind;
    use narsor::tui::remote::{RemoteApp, RemoteModal, RemoteRequest, RemoteScreen, Tone};

    use super::remote_support::{snapshot, RemoteHarness};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Plays the runner's side of the relay: answers every page request
    /// with two entries of the asked attempt, until aborted.
    fn serve_pages(runner: RunnerClient) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                let Ok(poll) = runner.poll(Some("episode-1"), 1).await else {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                };
                for request in poll.conversation_pages {
                    let entries = vec![
                        ConversationEntryDto {
                            entry_number: request.from_entry + 1,
                            timestamp: Utc::now(),
                            source: ConversationSource::Narsor,
                            raw_base64: BASE64.encode(b"the prompt"),
                        },
                        ConversationEntryDto {
                            entry_number: request.from_entry + 2,
                            timestamp: Utc::now(),
                            source: ConversationSource::Stdout,
                            raw_base64: BASE64.encode(
                                br#"{"type":"assistant","message":{"id":"m","content":[{"type":"text","text":"planned"}]}}"#,
                            ),
                        },
                    ];
                    let page = ConversationResponse::Recorded {
                        entries,
                        next_entry: request.from_entry + 3,
                        closed: true,
                        truncated_tail: false,
                    };
                    runner
                        .upload_conversation_page(&request.request_id, &page)
                        .await
                        .expect("upload the page");
                }
            }
        })
    }

    /// Covers AE12: the TUI reads a remote task's conversation and answers
    /// its pending input through the control plane — it never opens a
    /// connection to the runner or the backend, and the answer reaches the
    /// runner waiting on exactly that request.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_remote_mode_reads_a_conversation_and_answers_the_pending_input() {
        let harness = RemoteHarness::start().await;
        let running = harness
            .running("task-1", "k8s", snapshot(1, Some("episode-1")))
            .await;
        let pages = serve_pages(running.runner.clone());

        let mut app = RemoteApp::new(harness.owner.clone());
        app.refresh().await;
        assert_eq!(app.tasks().len(), 1);
        let request = app.on_key(key(KeyCode::Enter));
        assert_eq!(request, RemoteRequest::OpenDetail);
        app.perform(request).await;
        assert_eq!(app.screen(), RemoteScreen::Detail);
        let lines: Vec<String> = app.selected_lines().into_iter().map(|l| l.text).collect();
        assert!(
            lines.iter().any(|l| l.starts_with("Conversation live")),
            "{lines:#?}"
        );
        assert!(lines
            .iter()
            .any(|l| l.contains("Awaiting   input episode-1")));

        // c: the latest attempt's page, relayed from the live runner.
        let request = app.on_key(key(KeyCode::Char('c')));
        app.perform(request).await;
        let page = app
            .detail()
            .and_then(|detail| detail.page.clone())
            .expect("a page was read");
        assert_eq!(page.attempt_id, "act-6");
        let kinds: Vec<EntryKind> = page.entries.iter().map(|entry| entry.kind).collect();
        assert_eq!(kinds, vec![EntryKind::Narsor, EntryKind::Assistant]);
        assert!(page.entries[1].text.contains("planned"));
        pages.abort();

        // i: the form of the pending request; Enter sends it.
        assert_eq!(app.on_key(key(KeyCode::Char('i'))), RemoteRequest::None);
        assert!(matches!(app.modal(), RemoteModal::Input(_)));
        for c in "looks good".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        let request = app.on_key(key(KeyCode::Enter));
        assert_eq!(request, RemoteRequest::SubmitInput);
        app.perform(request).await;
        assert!(matches!(app.modal(), RemoteModal::None));
        let notice = app.notice().expect("an outcome").clone();
        assert_eq!(notice.tone, Tone::Good, "{}", notice.text);

        // The runner waiting on episode-1 receives exactly that answer.
        let poll = running
            .runner
            .poll(Some("episode-1"), 2)
            .await
            .expect("poll");
        assert_eq!(poll.inputs.len(), 1);
        let delivered = &poll.inputs[0];
        assert_eq!(delivered.pending_input_id, "episode-1");
        assert_eq!(
            delivered.delivery.values.get("reason"),
            Some(&serde_json::json!("looks good"))
        );

        // Answered: the task no longer offers the form.
        app.refresh().await;
        app.open_input();
        assert!(matches!(app.modal(), RemoteModal::None));
    }
}
