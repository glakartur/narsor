//! Tests for the TUI's state layer.
//!
//! Test the state, not the rendering: what is asserted here is what the
//! interface *decides* — which tasks it lists and in what order, what a key
//! press does to it, what it refuses to send, which request it makes of the
//! API. The application is [`narsor::tui::task_app::TaskApp`], driven over a
//! client that reaches nobody; no test starts an HTTP server.
//!
//! Besides the application: the key map's consistency, the colour roles, the
//! chooser and the connection state, the transcript presentation state, the
//! text of an old execution's events and the terminal restoration.
//!
//! The tests of the interface that U14 removed (the local `App` over
//! `/executions` and the separate `RemoteApp`) were either carried over to the
//! `task_app_*` modules below earlier (U8–U13) or exercise what the one
//! interface no longer does; the commit that removed them lists which.

use std::panic;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use serde_json::{json, Value};

use narsor::tui::event_text::{event_detail_lines, event_line};
use narsor::tui::transcript::{
    raw_display, DisplayMode, TranscriptEntry, TranscriptEntryId, TranscriptId, TranscriptState,
};
use narsor_engine::domain::{Event, EventData};
use narsor_engine::engine::actions::script::TRUNCATION_MARKER;

use crate::task_app_support;

/// A key press, as the event loop would deliver it.
fn key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

/// The terminal is restored on the way out and from the panic hook.
///
/// Entering the alternate screen is not possible here — the test harness has no
/// tty — so what is proved is that the restore path *runs*: once when called
/// directly, and once from inside a panic with the hook installed. That the
/// escape sequences then reach a real terminal is the part only a person at one
/// can confirm.
#[test]
fn the_terminal_is_restored_on_exit_and_on_panic() {
    let before = narsor::tui::restore_count();
    narsor::tui::restore_terminal();
    assert_eq!(
        narsor::tui::restore_count(),
        before + 1,
        "the ordinary exit path restores the terminal"
    );

    narsor::tui::install_panic_hook();
    let before_panic = narsor::tui::restore_count();

    let result = panic::catch_unwind(|| panic!("a crash inside the interface"));
    assert!(result.is_err(), "the panic really happened");

    assert!(
        narsor::tui::restore_count() > before_panic,
        "the panic hook restored the terminal before unwinding"
    );
}

// U6: readable events and the event detail window
// ---------------------------------------------------------------------------

/// One instance of every [`EventData`] variant, with plausible field values.
///
/// Used to prove `event_line` has no leftover catch-all: a variant added here
/// without a matching hand-written arm would fail to compile, and this list
/// is the thing that would need updating to prove it — see
/// `event_line_has_no_debug_style_dump_for_any_variant`.
fn every_event_data_variant() -> Vec<EventData> {
    vec![
        EventData::WorkflowStarted {
            workflow_id: "wf".to_string(),
            workflow_version: 1,
            inputs: json!({"ticket": "ORC-7"}),
        },
        EventData::WorkflowPaused {
            reason: "waiting on a human".to_string(),
        },
        EventData::WorkflowResumed,
        EventData::WorkflowCompleted { result: json!({}) },
        EventData::WorkflowFailed {
            error: "boom".to_string(),
        },
        EventData::WorkflowCancelled {
            reason: "operator".to_string(),
        },
        EventData::StateEntered {
            state_id: "prepare".to_string(),
        },
        EventData::StateExited {
            state_id: "prepare".to_string(),
            output_label: "success".to_string(),
        },
        EventData::ActionStarted {
            action_execution_id: "a1".to_string(),
            state_id: "prepare".to_string(),
            action_type: "agent".to_string(),
            decision_by: None,
        },
        EventData::ActionCompleted {
            action_execution_id: "a1".to_string(),
            output_label: "success".to_string(),
            reason: None,
            additional_data: Default::default(),
            warnings: Vec::new(),
        },
        EventData::ActionFailed {
            action_execution_id: "a1".to_string(),
            error: "boom".to_string(),
            error_type: None,
        },
        EventData::ActionAttemptFailed {
            action_execution_id: "a1".to_string(),
            state_id: "prepare".to_string(),
            error: "boom".to_string(),
            retry_in_ms: 500,
        },
        EventData::ActionRetrying {
            action_execution_id: "a2".to_string(),
            retry_count: 1,
            state_id: Some("prepare".to_string()),
        },
        EventData::AgentSessionStarted {
            session_id: "sess".to_string(),
            harness: "claude_code".to_string(),
            action_execution_id: Some("a1".to_string()),
            resumed_session: None,
            model: None,
            effort: None,
        },
        EventData::AgentSessionCompleted {
            session_id: "sess".to_string(),
            action_execution_id: Some("a1".to_string()),
            harness_session_id: Some("hs1".to_string()),
        },
        EventData::AgentSessionFailed {
            session_id: "sess".to_string(),
            error: "boom".to_string(),
            action_execution_id: Some("a1".to_string()),
            harness_session_id: None,
        },
        EventData::AgentMessage {
            session_id: "sess".to_string(),
            content: "hello there".to_string(),
            role: "assistant".to_string(),
            action_execution_id: Some("a1".to_string()),
        },
        ignored_denial("Bash", "ls /root", Some("not allowed"), None),
        EventData::DecisionRequested {
            provider: "claude".to_string(),
        },
        EventData::DecisionMade {
            provider: "claude".to_string(),
            output_label: "success".to_string(),
            reasoning: None,
        },
        EventData::HumanInputRequested {
            instruction: "approve?".to_string(),
            reason: None,
            required_values: Vec::new(),
            answer_tail: None,
            episode: None,
        },
        EventData::human_input_received(json!({"approval_status": "approved"})),
        EventData::HumanTransitionSelected {
            from_state: "review".to_string(),
            to_state: "done".to_string(),
        },
        EventData::ExternalEventReceived {
            event_type: "hook".to_string(),
            source: "github".to_string(),
            payload: json!({}),
        },
        EventData::TransitionTaken {
            from_state: "review".to_string(),
            to_state: "done".to_string(),
            output_label: "success".to_string(),
        },
        EventData::ArtifactCreated {
            artifact_id: "art1".to_string(),
            artifact_type: "file".to_string(),
            location: "artifacts/art1".to_string(),
        },
        EventData::Error {
            message: "boom".to_string(),
            context: json!({}),
        },
    ]
}

/// AE1: a `workflow_started` event with a dozen-plus inputs stays a single
/// line in the stream, and each input gets its own line of indented JSON in
/// the detail window.
#[test]
fn workflow_started_line_is_single_line_and_detail_expands_every_input() {
    let mut inputs = serde_json::Map::new();
    for index in 0..14 {
        inputs.insert(format!("field_{index}"), json!(format!("value-{index}")));
    }
    let event = Event::new(
        "exec-1".to_string(),
        EventData::WorkflowStarted {
            workflow_id: "wf".to_string(),
            workflow_version: 1,
            inputs: Value::Object(inputs),
        },
        1,
    );

    let line = event_line(&event);
    assert_eq!(line.lines().count(), 1, "the stream line stays one line");

    let detail = event_detail_lines(&event);
    for index in 0..14 {
        let needle = format!("field_{index}");
        assert!(
            detail.iter().any(|line| line.contains(&needle)),
            "input {needle} is present in the detail window"
        );
    }
}

/// Edge case: `event_line` never leaks a Rust `Debug` dump, for any variant —
/// every hand-written arm in the function stays free of `{`, which a derived
/// `Debug` for a struct-like variant always contains (e.g. `AgentMessage {`).
#[test]
fn event_line_has_no_debug_style_dump_for_any_variant() {
    for (index, data) in every_event_data_variant().into_iter().enumerate() {
        let event = Event::new("exec-1".to_string(), data, index as u64);
        let line = event_line(&event);
        assert!(
            !line.contains('{'),
            "event_line leaked a Debug-style dump: {line:?}"
        );
    }
}

/// Edge case: content with ANSI escapes and control characters reaches the
/// detail window with no raw control bytes left in it.
#[test]
fn event_detail_lines_strip_ansi_and_control_characters() {
    let event = Event::new(
        "exec-1".to_string(),
        EventData::Error {
            message: "boom\u{1b}[31mRED\u{1b}[0m\u{7}".to_string(),
            context: json!({"raw": "col1\tcol2\u{7}tail"}),
        },
        1,
    );

    let detail = event_detail_lines(&event);
    for line in &detail {
        assert!(
            !line.chars().any(|c| c.is_control() && c != '\n'),
            "a raw control byte survived sanitization: {line:?}"
        );
    }
    assert!(
        detail.iter().any(|line| line.contains("RED")),
        "the visible text around the escape codes is still there"
    );
}

/// Edge case: an `AgentMessage` whose content the executor truncated is
/// flagged in the detail window.
#[test]
fn truncated_agent_message_is_flagged_in_the_detail_window() {
    let content = format!("partial output{TRUNCATION_MARKER}");
    let event = Event::new(
        "exec-1".to_string(),
        EventData::AgentMessage {
            session_id: "sess".to_string(),
            content,
            role: "assistant".to_string(),
            action_execution_id: Some("a1".to_string()),
        },
        1,
    );

    let detail = event_detail_lines(&event);
    assert!(
        detail
            .iter()
            .any(|line| line.to_lowercase().contains("truncated")),
        "the truncation is flagged: {detail:?}"
    );
    assert!(
        detail.iter().any(|line| line.contains("conversation tab")),
        "the window points at the conversation tab: {detail:?}"
    );
}

// ---------------------------------------------------------------------------
// U1: shared transcript presentation state
// ---------------------------------------------------------------------------

fn transcript_entry(id: &str, lines: &[&str]) -> TranscriptEntry {
    TranscriptEntry::new(
        TranscriptEntryId::from(id),
        lines.iter().map(|line| (*line).to_string()).collect(),
        id.as_bytes().to_vec(),
    )
}

#[test]
fn transcript_cursor_moves_between_entries_not_wrapped_lines() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt-a"));
    transcript.load(vec![
        transcript_entry("one", &["line 0", "line 1", "line 2", "line 3"]),
        transcript_entry("two", &["next entry"]),
    ]);
    transcript.set_viewport(3);

    transcript.to_top();
    assert_eq!(
        transcript.selected_entry(),
        Some(&TranscriptEntryId::from("one"))
    );
    transcript.line_down();
    assert_eq!(
        transcript.selected_entry(),
        Some(&TranscriptEntryId::from("two"))
    );
    transcript.line_up();
    assert_eq!(
        transcript.selected_entry(),
        Some(&TranscriptEntryId::from("one"))
    );
}

#[test]
fn transcript_opens_at_end_and_preserves_an_older_anchor_on_append() {
    let mut empty = TranscriptState::new(TranscriptId::from("empty-attempt"));
    empty.load(Vec::new());
    assert!(empty.is_following_end());
    assert_eq!(empty.scroll_offset(), 0);
    empty.append(vec![transcript_entry("first", &["first line"])]);
    assert!(empty.is_following_end());

    let mut transcript = TranscriptState::new(TranscriptId::from("attempt-a"));
    transcript.set_viewport(2);
    transcript.load(vec![
        transcript_entry("one", &["one-a", "one-b"]),
        transcript_entry("two", &["two-a", "two-b"]),
    ]);
    // Each entry is a header and its lines, with a blank line between
    // entries: 3 + 1 + 3 lines, the last two in view.
    assert_eq!(transcript.scroll_offset(), 5);
    assert!(transcript.is_following_end());

    transcript.append(vec![transcript_entry("three", &["three-a"])]);
    assert_eq!(transcript.scroll_offset(), 8);
    assert!(transcript.is_following_end());

    transcript.to_top();
    let anchor = transcript.anchor().expect("anchor while reading");
    assert_eq!(anchor.entry_id, TranscriptEntryId::from("one"));
    assert_eq!(anchor.logical_line, 0);
    assert!(!transcript.is_following_end());

    transcript.append(vec![transcript_entry("four", &["four-a"])]);
    assert_eq!(transcript.anchor(), Some(anchor));
    assert_eq!(transcript.scroll_offset(), 0);
}

#[test]
fn transcript_reanchors_across_detail_mode_resize_and_missing_entry() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt-a"));
    transcript.set_viewport(2);
    transcript.load(vec![
        transcript_entry("one", &["one-a", "one-b"]),
        transcript_entry("two", &["two-a", "two-b"])
            .with_detail_lines(vec!["detail-a".into(), "detail-b".into()]),
    ]);
    transcript.to_top();
    transcript.line_down();
    let anchor = transcript.anchor().unwrap();

    assert_eq!(anchor.entry_id, TranscriptEntryId::from("two"));

    transcript.toggle_expanded(&TranscriptEntryId::from("two"));
    assert_eq!(transcript.anchor(), Some(anchor));
    // The anchor is the top of the viewport, not the cursor. Raw mode has
    // one line per entry, so the viewport of two lines cannot start on `two`
    // and stops on the blank line after `one`; the cursor stays on `two`.
    transcript.set_mode(DisplayMode::Raw);
    assert_eq!(
        transcript.anchor().unwrap().entry_id,
        TranscriptEntryId::from("one")
    );
    assert_eq!(
        transcript.selected_entry(),
        Some(&TranscriptEntryId::from("two"))
    );
    transcript.set_viewport(1);
    assert_eq!(
        transcript.anchor().unwrap().entry_id,
        TranscriptEntryId::from("one")
    );
    assert_eq!(
        transcript.selected_entry(),
        Some(&TranscriptEntryId::from("two"))
    );

    transcript.replace_entries(vec![transcript_entry("two", &["replacement"])]);
    assert_eq!(
        transcript.anchor().unwrap().entry_id,
        TranscriptEntryId::from("two")
    );
    assert_eq!(transcript.scroll_offset(), 0);
}

#[test]
fn transcript_expansion_is_scoped_to_its_conversation() {
    let entry = TranscriptEntryId::from("1");
    let mut first = TranscriptState::new(TranscriptId::from("attempt-a"));
    first.load(vec![
        transcript_entry("1", &["body"]).with_detail_lines(vec!["detail".into()])
    ]);
    first.toggle_expanded(&entry);
    assert!(first.is_expanded(&entry));

    first.set_conversation(TranscriptId::from("attempt-b"));
    assert!(!first.is_expanded(&entry));
    first.set_conversation(TranscriptId::from("attempt-a"));
    assert!(first.is_expanded(&entry));
}

#[test]
fn transcript_raw_display_is_terminal_safe_and_readable() {
    let raw = b"before\x1b[31m\0\r\tafter\xff";
    let shown = raw_display(raw);
    assert!(shown.contains("\\x1b"));
    assert!(shown.contains("\\x00"));
    assert!(shown.contains("\\r"));
    assert!(shown.contains("\\t"));
    assert!(!shown.contains('\r'));
    assert!(!shown.contains('\t'));
    assert!(shown.contains("before"));
    assert!(shown.contains("after"));
}

#[test]
fn transcript_keeps_entry_raw_separate_from_global_raw() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt-a"));
    transcript.load(vec![transcript_entry("one", &["readable"])]);
    transcript.toggle_selected_raw();
    assert!(transcript.is_raw(&TranscriptEntryId::from("one")));
    assert_eq!(transcript.mode(), DisplayMode::Rendered);
    transcript.toggle_mode();
    assert_eq!(transcript.mode(), DisplayMode::Raw);
    transcript.toggle_selected_raw();
    assert!(
        transcript.is_raw(&TranscriptEntryId::from("one")),
        "global raw must not mutate hidden entry state"
    );
}

#[test]
fn transcript_cycles_agent_views_without_losing_the_selected_source() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt-a"));
    transcript.load(vec![
        transcript_entry("one", &["first"]),
        transcript_entry("two", &["second"]),
    ]);
    transcript.to_top();
    transcript.next_conversation_view();
    assert_eq!(transcript.mode(), DisplayMode::Chronology);
    assert_eq!(
        transcript.selected_entry(),
        Some(&TranscriptEntryId::from("one"))
    );
    transcript.next_conversation_view();
    assert_eq!(transcript.mode(), DisplayMode::Raw);
    transcript.next_conversation_view();
    assert_eq!(transcript.mode(), DisplayMode::Rendered);
    assert_eq!(
        transcript.selected_entry(),
        Some(&TranscriptEntryId::from("one"))
    );
}

#[test]
fn transcript_separates_adjacent_entries() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt-a"));
    transcript.load(vec![
        transcript_entry("one", &["first"]),
        transcript_entry("two", &["second"]),
    ]);

    let lines: Vec<String> = transcript
        .logical_lines()
        .into_iter()
        .map(|line| line.text)
        .collect();
    // Each entry opens with its header line: here a bare marker.
    assert_eq!(lines, vec!["· ", "first", "", "· ", "second"]);
}

/// U7: the building blocks of the redesigned interface — key map, colour
/// roles, the chooser and the connection state. None of it needs a daemon.
mod foundations {
    use super::*;

    use narsor::tui::keymap::{
        self, bind, check_consistency, help_entries, hints, lookup, Action, Binding, Context, Key,
        Stack,
    };

    fn press(code: KeyCode) -> KeyEvent {
        key(code)
    }

    mod key_map {
        use super::*;

        /// The keys origin §3 gives the list context, plus the global ones.
        const LIST_KEYS: [&str; 13] = [
            "?", "q", "j", "k", "g", "G", "Space", "Enter", ".", "n", "z", "H", "R",
        ];

        fn help_keys(context: Context) -> Vec<String> {
            help_entries(context)
                .iter()
                .flat_map(|entry| {
                    if entry.keys == "/" {
                        vec!["/".to_string()]
                    } else {
                        entry.keys.split('/').map(str::to_string).collect()
                    }
                })
                .collect()
        }

        #[test]
        fn the_table_is_consistent() {
            assert_eq!(check_consistency(keymap::BINDINGS), Ok(()));
        }

        #[test]
        fn a_second_n_in_the_list_context_breaks_the_consistency_check() {
            let mut table: Vec<Binding> = keymap::BINDINGS.to_vec();
            table.push(bind(
                Context::List,
                Key::char('n'),
                Action::Quit,
                "",
                "a second meaning",
            ));
            let problems = check_consistency(&table).expect_err("a doubled key is refused");
            assert!(
                problems
                    .iter()
                    .any(|problem| problem.contains("(lista, n)")),
                "{problems:?}"
            );
        }

        #[test]
        fn a_context_key_that_shadows_a_global_one_is_refused() {
            let mut table: Vec<Binding> = keymap::BINDINGS.to_vec();
            table.push(bind(
                Context::List,
                Key::char('q'),
                Action::Back,
                "",
                "shadow",
            ));
            assert!(check_consistency(&table).is_err());
        }

        #[test]
        fn list_help_names_every_key_of_the_origin_table() {
            let keys = help_keys(Context::List);
            for expected in LIST_KEYS {
                assert!(
                    keys.iter().any(|key| key == expected),
                    "{expected} is missing from the list help: {keys:?}"
                );
            }
            assert!(help_entries(Context::List)
                .iter()
                .all(|entry| !entry.description.is_empty()));
        }

        #[test]
        fn the_panel_bar_offers_tab_enter_ctrl_j_and_esc() {
            let bar: Vec<String> = hints(Context::Panel)
                .into_iter()
                .map(|(keys, _)| keys)
                .collect();
            for expected in ["Tab/Shift+Tab", "Enter", "Ctrl+J", "Esc"] {
                assert!(
                    bar.iter().any(|keys| keys == expected),
                    "{expected}: {bar:?}"
                );
            }
        }

        #[test]
        fn help_agrees_with_the_handling_for_every_row() {
            for binding in keymap::BINDINGS {
                let event = binding.key.to_event();
                assert_eq!(
                    lookup(binding.context, &event),
                    Some(binding.action),
                    "({}, {}) must do what the table says",
                    binding.context.name(),
                    binding.key.display()
                );
                assert!(
                    help_keys(binding.context).contains(&binding.key.display()),
                    "({}, {}) must be in the help",
                    binding.context.name(),
                    binding.key.display()
                );
            }
        }

        #[test]
        fn the_details_name_brackets_as_the_tabs() {
            let entries = help_entries(Context::Details);
            let tabs = entries
                .iter()
                .find(|entry| entry.keys == "[")
                .expect("[ is in the help of the details");
            assert_eq!(tabs.label, "zakładki");
            assert!(entries.iter().any(|entry| entry.keys == "]"));
            for context in [Context::Details, Context::HistoryDetails, Context::History] {
                assert_eq!(
                    lookup(context, &press(KeyCode::Char(']'))),
                    Some(Action::NextTab)
                );
                assert_eq!(lookup(context, &press(KeyCode::Right)), None, "{context:?}");
            }
        }

        fn step_conversation() -> Stack {
            Stack::layered(Context::StepConversation, Context::Details)
        }

        fn stack_keys(stack: Stack) -> Vec<String> {
            help_entries(stack)
                .iter()
                .flat_map(|entry| entry.keys.split('/').map(str::to_string))
                .collect()
        }

        #[test]
        fn every_stack_has_its_layer_first_then_its_base_then_the_global_keys() {
            for stack in Stack::LAYERED {
                assert!(stack.layer.is_some_and(Context::is_layer), "{stack:?}");
                let expected = if stack.layer == Some(Context::AgentConversation) {
                    vec![
                        Context::AgentConversation,
                        Context::Reading,
                        stack.base,
                        Context::Global,
                    ]
                } else {
                    vec![stack.layer.unwrap(), stack.base, Context::Global]
                };
                assert_eq!(stack.scopes().collect::<Vec<_>>(), expected);
                assert_eq!(
                    lookup(stack, &press(KeyCode::Char('?'))),
                    Some(Action::Help)
                );
                assert_eq!(
                    lookup(stack, &press(KeyCode::Char(']'))),
                    Some(Action::NextTab),
                    "{stack:?}: the base's keys work under every layer"
                );
            }
            assert!(Stack::all().all(|stack| !stack.base.is_layer()));
            assert_eq!(
                Stack::from(Context::Form).scopes().collect::<Vec<_>>(),
                [Context::Form]
            );
        }

        #[test]
        fn the_axis_bar_enters_the_step_and_does_not_expand() {
            let axis = Stack::layered(Context::Axis, Context::Details);
            let bar = hints(axis);
            assert_eq!(bar[0], ("Esc".to_string(), "lista"), "{bar:?}");
            assert!(
                bar.iter()
                    .any(|(keys, label)| keys == "Enter/→" && *label == "rozmowa kroku"),
                "{bar:?}"
            );
            assert!(bar.iter().all(|(_, label)| *label != "rozwiń"), "{bar:?}");
            assert_eq!(
                lookup(axis, &press(KeyCode::Right)),
                Some(Action::EnterStep)
            );
            assert_eq!(
                lookup(axis, &press(KeyCode::Char('f'))),
                Some(Action::Follow)
            );
            for code in [
                KeyCode::PageDown,
                KeyCode::PageUp,
                KeyCode::Char(' '),
                KeyCode::Char('v'),
            ] {
                assert_eq!(lookup(axis, &press(code)), None, "{code:?} reads a step");
            }
        }

        #[test]
        fn the_step_conversation_reads_and_goes_back_to_the_axis() {
            let bar = hints(step_conversation());
            assert_eq!(bar[0], ("Esc/←".to_string(), "oś"), "{bar:?}");
            let keys = stack_keys(step_conversation());
            for expected in ["PgUp", "PgDn", "v", "r", "Enter", "Space", "j", "g", "G"] {
                assert!(
                    keys.iter().any(|key| key == expected),
                    "{expected}: {keys:?}"
                );
            }
            assert!(!keys.iter().any(|key| key == "f"), "{keys:?}");
            assert_eq!(
                lookup(step_conversation(), &press(KeyCode::Left)),
                Some(Action::LeaveStep)
            );
            assert_eq!(
                lookup(step_conversation(), &press(KeyCode::Esc)),
                Some(Action::LeaveStep),
                "the layer's Esc is the only one in the stack"
            );
            assert_eq!(
                lookup(step_conversation(), &press(KeyCode::Char('f'))),
                None
            );
        }

        #[test]
        fn raw_conversation_shortcuts_keep_navigation_and_hide_unavailable_actions() {
            for stack in [
                step_conversation(),
                Stack::layered(Context::AgentConversation, Context::Details),
            ] {
                let hints = keymap::raw_conversation_hints(stack);
                assert!(
                    hints.iter().any(|(_, label)| *label == "widok"),
                    "{hints:?}"
                );
                assert!(
                    hints.iter().all(|(_, label)| {
                        !matches!(*label, "rozwiń" | "źródło" | "surowy całości")
                    }),
                    "{hints:?}"
                );
                let entries = keymap::raw_conversation_help_entries(stack);
                assert!(
                    entries.iter().all(|entry| {
                        !matches!(
                            entry.description,
                            "rozwiń albo zwiń wpis"
                                | "pokaż albo ukryj źródło wpisu"
                                | "widok surowy całości"
                        )
                    }),
                    "{entries:?}"
                );
            }
        }

        #[test]
        fn the_history_has_the_same_layers_without_the_tasks_actions_or_following() {
            for layer in [
                Context::HistoryAxis,
                Context::StepConversation,
                Context::Reading,
                Context::AgentConversation,
            ] {
                let stack = Stack::layered(layer, Context::HistoryDetails);
                let keys = stack_keys(stack);
                for task_only in ["p", "x", "t", "s", "a", ".", "m", "c"] {
                    assert!(
                        !keys.iter().any(|key| key == task_only),
                        "{task_only} in {stack:?}: {keys:?}"
                    );
                }
                assert!(keys.iter().any(|key| key == "w"), "{stack:?}");
            }
            let axis = Stack::layered(Context::HistoryAxis, Context::HistoryDetails);
            assert!(!stack_keys(axis).iter().any(|key| key == "f"));
            assert_eq!(
                lookup(axis, &press(KeyCode::Enter)),
                Some(Action::EnterStep)
            );
        }

        #[test]
        fn a_key_in_a_layer_and_in_its_base_breaks_the_consistency_check() {
            let mut table: Vec<Binding> = keymap::BINDINGS.to_vec();
            table.push(bind(
                Context::Details,
                Key::char('v'),
                Action::Comment,
                "",
                "a second meaning",
            ));
            let problems = check_consistency(&table).expect_err("a shadowed key is refused");
            assert!(
                problems
                    .iter()
                    .any(|problem| problem == "(czytanie, v) shadows (szczegóły, v)"),
                "{problems:?}"
            );
        }

        #[test]
        fn global_keys_work_where_context_inherits_them_and_not_in_forms() {
            let help = press(KeyCode::Char('?'));
            assert_eq!(lookup(Context::List, &help), Some(Action::Help));
            assert_eq!(lookup(Context::Details, &help), Some(Action::Help));
            assert_eq!(lookup(Context::Form, &help), None, "a form types a ?");
            assert_eq!(lookup(Context::Form, &press(KeyCode::Char('q'))), None);
        }

        #[test]
        fn digits_pick_and_shift_is_ignored_for_letters() {
            assert_eq!(
                lookup(Context::Panel, &press(KeyCode::Char('3'))),
                Some(Action::Pick)
            );
            assert_eq!(keymap::digit_of(&press(KeyCode::Char('3'))), Some(3));
            assert_eq!(keymap::digit_of(&press(KeyCode::Char('0'))), None);
            let shifted_g = KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT);
            assert_eq!(lookup(Context::List, &shifted_g), Some(Action::Bottom));
        }

        #[test]
        fn every_context_has_keys_and_the_origin_move_and_menu_shortcuts_exist() {
            for context in Context::ALL {
                assert!(
                    keymap::BINDINGS
                        .iter()
                        .any(|binding| binding.context == context),
                    "{} has no keys",
                    context.name()
                );
            }
            // The queue move and the panel have keys; the five actions of
            // the panel (pause, stop, transition, restart, archive) have none.
            for context in [Context::List, Context::Details, Context::Menu] {
                assert_eq!(
                    lookup(context, &KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL)),
                    Some(Action::MoveUp),
                    "Ctrl+Up in {}",
                    context.name()
                );
                assert_eq!(
                    lookup(
                        context,
                        &KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL)
                    ),
                    Some(Action::MoveDown),
                    "Ctrl+Down in {}",
                    context.name()
                );
                assert_eq!(
                    lookup(context, &press(KeyCode::Char('m'))),
                    Some(Action::MoveMode),
                    "m in {}",
                    context.name()
                );
                assert_eq!(
                    lookup(context, &press(KeyCode::Char('o'))),
                    Some(Action::OpenLink),
                    "o in {}",
                    context.name()
                );
            }
            for context in [Context::List, Context::Details] {
                for key in [
                    KeyEvent::new(KeyCode::Char('.'), KeyModifiers::CONTROL),
                    press(KeyCode::Char('.')),
                ] {
                    assert_eq!(
                        lookup(context, &key),
                        Some(Action::ActionMenu),
                        "the panel in {}",
                        context.name()
                    );
                }
                for removed in ['p', 'x', 't', 's', 'a', 'K', 'J'] {
                    assert_eq!(
                        lookup(context, &press(KeyCode::Char(removed))),
                        None,
                        "{removed} has no meaning in {}",
                        context.name()
                    );
                }
            }
        }
    }

    mod chooser_state {
        use super::*;

        use narsor::tui::chooser::{
            Chooser, ChooserAction, ChooserField, ChooserFocus, ChooserItem,
        };

        fn alt_enter() -> KeyEvent {
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)
        }

        fn type_text(chooser: &mut Chooser, text: &str) {
            for character in text.chars() {
                chooser.on_key(press(KeyCode::Char(character)));
            }
        }

        fn review_chooser() -> Chooser {
            Chooser::new(vec![
                ChooserItem::new("approved", "Akceptuje wynik."),
                ChooserItem::new("revise", "Zwraca do agenta.")
                    .resuming("plan")
                    .recommended()
                    .with_field(ChooserField::multiline("comment").required()),
            ])
        }

        fn ctrl_j() -> KeyEvent {
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)
        }

        fn focused_comment(chooser: &mut Chooser) {
            chooser.on_key(press(KeyCode::Enter));
            assert_eq!(chooser.focus(), ChooserFocus::Fields);
        }

        /// One entry with `field`, the keys in it.
        fn typing_into(field: ChooserField) -> Chooser {
            let mut chooser =
                Chooser::new(vec![ChooserItem::new("go", "Start.").with_field(field)]);
            chooser.enter_fields();
            assert!(chooser.is_typing());
            chooser
        }

        fn pasted(field: ChooserField, text: &str) -> ChooserField {
            let mut chooser = typing_into(field);
            chooser.paste(text);
            chooser.fields_of(0)[0].clone()
        }

        #[test]
        fn a_paste_of_lines_stays_lines_with_the_cursor_after_it_and_sends_nothing() {
            let mut chooser = typing_into(ChooserField::multiline("prompt").required());
            assert!(chooser.paste("a\nb\nc"));
            let field = &chooser.fields_of(0)[0];
            assert_eq!(field.value(), "a\nb\nc");
            assert_eq!(field.cursor(), 5);
            assert_eq!(chooser.focus(), ChooserFocus::Fields);
            assert!(
                matches!(
                    chooser.on_key(press(KeyCode::Enter)),
                    ChooserAction::Submit(_)
                ),
                "Enter after a paste still sends"
            );
        }

        #[test]
        fn carriage_returns_in_a_paste_are_one_new_line_each() {
            let multiline = || ChooserField::multiline("prompt");
            assert_eq!(pasted(multiline(), "a\r\nb\r\nc").value(), "a\nb\nc");
            assert_eq!(pasted(multiline(), "a\rb").value(), "a\nb");
        }

        #[test]
        fn a_paste_into_a_one_line_field_has_spaces_for_line_breaks() {
            assert_eq!(
                pasted(ChooserField::text("ticket"), "ABC-123\n").value(),
                "ABC-123 "
            );
            assert_eq!(
                pasted(ChooserField::text("ticket"), "x\r\ny").value(),
                "x y"
            );
        }

        #[test]
        fn a_paste_goes_in_at_the_cursor_counted_in_characters() {
            let mut chooser = typing_into(ChooserField::multiline("prompt").with_value("ab"));
            chooser.on_key(press(KeyCode::Left));
            chooser.paste("XY");
            assert_eq!(chooser.fields_of(0)[0].value(), "aXYb");
            assert_eq!(chooser.fields_of(0)[0].cursor(), 3);

            let mut chooser = typing_into(ChooserField::text("name").with_value("zażółć"));
            for _ in 0..4 {
                chooser.on_key(press(KeyCode::Left));
            }
            chooser.paste("🦀ę");
            assert_eq!(chooser.fields_of(0)[0].value(), "za🦀ężółć");
            assert_eq!(chooser.fields_of(0)[0].cursor(), 4);
        }

        #[test]
        fn a_paste_keeps_tabs_and_drops_escapes_and_control_characters() {
            assert_eq!(pasted(ChooserField::text("t"), "a\tb").value(), "a\tb");
            assert_eq!(
                pasted(ChooserField::multiline("t"), "\x1b[31mred\x1b[0m\x07!\x00").value(),
                "red!"
            );
        }

        #[test]
        fn a_broken_escape_in_a_paste_takes_no_pasted_text_with_it() {
            let multiline = || ChooserField::multiline("t");
            assert_eq!(
                pasted(multiline(), "line1\x1b\nline2").value(),
                "line1\nline2",
                "a lone ESC keeps the line break after it"
            );
            assert_eq!(pasted(multiline(), "a\x1bż").value(), "aż");
            assert_eq!(
                pasted(multiline(), "log\x1b]0;tytuł\nreszta logu").value(),
                "log\nreszta logu",
                "a cut-off OSC ends at the line break"
            );
            assert_eq!(
                pasted(multiline(), "a\x1b]8;;http://x\x1b\\b").value(),
                "ab",
                "an OSC ended by ST leaves no backslash"
            );
            assert_eq!(
                pasted(multiline(), "\x1b[12, 34\nfoo").value(),
                "\nfoo",
                "a cut-off CSI stops at the line break; the next line stays whole"
            );
            assert_eq!(pasted(multiline(), "\x1bcok").value(), "cok");
            assert_eq!(pasted(multiline(), "\x1bMok").value(), "ok");
        }

        #[test]
        fn a_paste_drops_what_the_field_would_not_show() {
            assert_eq!(
                pasted(
                    ChooserField::multiline("t"),
                    "a\u{200b}b\u{202e}c\u{feff}d\u{e0041}e"
                )
                .value(),
                "abcde"
            );
            assert_eq!(
                pasted(ChooserField::multiline("t"), "a\u{2028}b\u{2029}c\u{85}d").value(),
                "a\nb\nc\nd",
                "the Unicode line breaks are new lines"
            );
            assert_eq!(
                pasted(ChooserField::text("t"), "word\u{2028}word").value(),
                "word word"
            );
        }

        #[test]
        fn a_number_field_takes_only_digits_and_a_choice_takes_no_paste() {
            assert_eq!(
                pasted(ChooserField::integer("count"), "4a2\n").value(),
                "42"
            );
            let mut chooser = Chooser::new(vec![ChooserItem::new("go", "Start.")
                .with_field(ChooserField::select("mode", vec!["a".into(), "b".into()]))]);
            chooser.enter_fields();
            assert_eq!(chooser.focus(), ChooserFocus::Fields);
            assert!(!chooser.paste("b"));
            assert_eq!(chooser.fields_of(0)[0].value(), "a");
        }

        #[test]
        fn an_empty_paste_or_one_of_control_characters_changes_nothing() {
            let field = ChooserField::text("x").proposed("v");
            let mut chooser = typing_into(field);
            assert!(!chooser.paste(""));
            assert!(!chooser.paste("\x1b[0m\x07"));
            let field = &chooser.fields_of(0)[0];
            assert_eq!(field.value(), "v");
            assert!(field.is_proposed());
        }

        #[test]
        fn a_paste_clears_the_proposal_mark_and_the_complaint() {
            let mut chooser = typing_into(ChooserField::text("x").proposed("v"));
            assert!(chooser.set_field_error(0, "x", "zła wartość"));
            assert!(chooser.paste("w"));
            let field = &chooser.fields_of(0)[0];
            assert_eq!(field.value(), "vw");
            assert!(!field.is_proposed());
            assert_eq!(field.error(), None);
        }

        #[test]
        fn a_paste_with_the_keys_on_the_list_changes_nothing() {
            let mut chooser = review_chooser().selecting(1);
            assert_eq!(chooser.focus(), ChooserFocus::Actions);
            assert!(!chooser.paste("q\n"));
            assert_eq!(chooser.fields_of(1)[0].value(), "");
            assert_eq!(chooser.selected(), 1);
        }

        #[test]
        fn enter_walks_action_fields_and_submits_from_the_last_one() {
            let mut chooser = Chooser::new(vec![ChooserItem::new("change", "Zmienia zadanie.")
                .with_field(ChooserField::text("title").required())
                .with_field(ChooserField::multiline("reason").required())]);
            focused_comment(&mut chooser);
            type_text(&mut chooser, "Tytuł");
            assert_eq!(chooser.on_key(press(KeyCode::Enter)), ChooserAction::None);
            type_text(&mut chooser, "Powód");
            match chooser.on_key(press(KeyCode::Enter)) {
                ChooserAction::Submit(submission) => assert_eq!(
                    submission.values,
                    vec![
                        ("title".to_string(), "Tytuł".to_string()),
                        ("reason".to_string(), "Powód".to_string())
                    ]
                ),
                other => panic!("expected the last field to submit, got {other:?}"),
            }
        }

        #[test]
        fn vertical_arrows_move_the_cursor_between_multiline_rows() {
            let mut chooser = Chooser::new(vec![ChooserItem::new("change", "Zmienia zadanie.")
                .with_field(ChooserField::text("title"))
                .with_field(ChooserField::multiline("reason").required())
                .with_field(ChooserField::multiline("follow_up"))])
            .with_inputs_layout();
            focused_comment(&mut chooser);
            // The focus starts on the first field, `title`.
            chooser.on_key(press(KeyCode::Down));
            type_text(&mut chooser, "abc");
            chooser.on_key(ctrl_j());
            type_text(&mut chooser, "def");
            chooser.on_key(press(KeyCode::Up));
            type_text(&mut chooser, "X");
            assert_eq!(chooser.fields_of(0)[1].value(), "abcX\ndef");
            chooser.on_key(press(KeyCode::Down));
            type_text(&mut chooser, "Y");
            assert_eq!(chooser.fields_of(0)[1].value(), "abcX\ndefY");
            // On the last row the first press only goes to the end; the
            // second leaves for the next field.
            chooser.on_key(press(KeyCode::Down));
            chooser.on_key(press(KeyCode::Down));
            type_text(&mut chooser, "Z");
            assert_eq!(chooser.fields_of(0)[2].value(), "Z");
            assert_eq!(chooser.fields_of(0)[0].value(), "");
        }

        #[test]
        fn up_from_the_second_short_field_moves_to_the_first_field() {
            let mut chooser = Chooser::new(vec![ChooserItem::new("change", "Zmienia zadanie.")
                .with_field(ChooserField::text("title"))
                .with_field(ChooserField::text("ticket"))])
            .with_inputs_layout();
            focused_comment(&mut chooser);
            chooser.on_key(press(KeyCode::Down));
            chooser.on_key(press(KeyCode::Up));
            type_text(&mut chooser, "tytuł");
            assert_eq!(chooser.fields_of(0)[0].value(), "tytuł");
            assert_eq!(chooser.fields_of(0)[1].value(), "");
        }

        #[test]
        fn ctrl_j_is_a_new_line_in_every_terminal_and_does_not_send() {
            keymap::override_modifiers_distinguished(Some(false));
            let mut chooser = review_chooser().selecting(1);
            focused_comment(&mut chooser);
            type_text(&mut chooser, "a");
            assert_eq!(chooser.on_key(ctrl_j()), ChooserAction::None);
            type_text(&mut chooser, "b");
            assert_eq!(chooser.fields_of(1)[0].value(), "a\nb");
        }

        #[test]
        fn shift_enter_and_alt_enter_are_a_new_line_never_a_send() {
            let mut chooser = review_chooser().selecting(1);
            focused_comment(&mut chooser);
            type_text(&mut chooser, "a");
            let shift = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
            assert_eq!(chooser.on_key(shift), ChooserAction::None);
            assert_eq!(chooser.on_key(alt_enter()), ChooserAction::None);
            assert_eq!(chooser.fields_of(1)[0].value(), "a\n\n");
            assert!(
                matches!(
                    chooser.on_key(press(KeyCode::Enter)),
                    ChooserAction::Submit(_)
                ),
                "a plain Enter still sends"
            );
        }

        #[test]
        fn a_form_of_inputs_starts_on_the_first_field_and_esc_goes_back_to_the_list() {
            let mut chooser = Chooser::new(vec![ChooserItem::new("ship", "Wysyła.")
                .with_field(ChooserField::text("ticket"))
                .with_field(ChooserField::multiline("notes"))
                .with_field(ChooserField::multiline("prompt").required())])
            .with_inputs_layout();
            assert_eq!(chooser.on_key(press(KeyCode::Enter)), ChooserAction::None);
            // The focus starts on the first field from the top (R9).
            type_text(&mut chooser, "t");
            assert_eq!(chooser.fields_of(0)[0].value(), "t");
            // Short fields come first in the walk; a multi-line field lets
            // go on the second arrow in a row.
            chooser.on_key(press(KeyCode::Down));
            chooser.on_key(press(KeyCode::Down));
            chooser.on_key(press(KeyCode::Down));
            type_text(&mut chooser, "p");
            assert_eq!(chooser.fields_of(0)[2].value(), "p");
            chooser.on_key(press(KeyCode::Backspace));
            assert_eq!(chooser.on_key(press(KeyCode::Esc)), ChooserAction::None);
            assert_eq!(chooser.focus(), ChooserFocus::Actions);
            assert_eq!(chooser.on_key(press(KeyCode::Esc)), ChooserAction::Cancel);
        }

        #[test]
        fn ctrl_j_in_a_one_line_or_choice_field_changes_nothing() {
            let mut chooser = Chooser::new(vec![ChooserItem::new("go", "Start.")
                .with_field(ChooserField::text("title").required())
                .with_field(ChooserField::integer("count"))
                .with_field(ChooserField::select("mode", vec!["a".into(), "b".into()]))]);
            focused_comment(&mut chooser);
            type_text(&mut chooser, "x");
            for _ in 0..3 {
                assert_eq!(chooser.on_key(ctrl_j()), ChooserAction::None);
                chooser.on_key(press(KeyCode::Down));
            }
            let fields = chooser.fields_of(0);
            assert_eq!(fields[0].value(), "x", "no `j`, no new line");
            assert_eq!(fields[1].value(), "");
            assert_eq!(fields[2].value(), fields[2].value().trim());
        }

        #[test]
        fn the_bar_promises_only_the_new_line_keys_the_terminal_delivers() {
            let bar = |distinguished| {
                keymap::override_modifiers_distinguished(Some(distinguished));
                hints(Context::Panel)
                    .into_iter()
                    .map(|(keys, _)| keys)
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            let plain = bar(false);
            assert!(plain.contains("Ctrl+J"), "{plain}");
            assert!(
                !plain.contains("Shift+Enter") && !plain.contains("Alt+Enter"),
                "{plain}"
            );
            let extended = bar(true);
            assert!(
                extended.contains("Ctrl+J/Shift+Enter/Alt+Enter"),
                "{extended}"
            );
            keymap::override_modifiers_distinguished(None);
        }

        #[test]
        fn fields_keep_their_values_when_the_action_changes_and_comes_back() {
            let mut chooser = review_chooser().selecting(1);
            assert_eq!(chooser.on_key(press(KeyCode::Enter)), ChooserAction::None);
            assert_eq!(
                chooser.focus(),
                ChooserFocus::Fields,
                "required field: Enter goes to it"
            );
            type_text(&mut chooser, "migracja");
            chooser.on_key(alt_enter());
            type_text(&mut chooser, "orders");

            for _ in 0..15 {
                chooser.on_key(press(KeyCode::Left));
            }
            // At the start of the text the first press only arms the edge.
            chooser.on_key(press(KeyCode::Up));
            assert_eq!(chooser.focus(), ChooserFocus::Fields);
            chooser.on_key(press(KeyCode::Up));
            assert_eq!(chooser.focus(), ChooserFocus::Actions);
            chooser.on_key(press(KeyCode::Char('k')));
            assert_eq!(chooser.selected(), 0);
            chooser.on_key(press(KeyCode::Char('2')));
            assert_eq!(chooser.selected(), 1);

            assert_eq!(chooser.fields_of(1)[0].value(), "migracja\norders");
        }

        #[test]
        fn digit_beyond_the_list_changes_nothing() {
            let mut chooser = review_chooser();
            chooser.on_key(press(KeyCode::Char('3')));
            assert_eq!(chooser.selected(), 0);
            chooser.on_key(press(KeyCode::Char('2')));
            assert_eq!(chooser.selected(), 1);
            chooser.on_key(press(KeyCode::Char('3')));
            assert_eq!(
                chooser.selected(),
                1,
                "3 with two actions leaves the choice"
            );
            chooser.on_key(press(KeyCode::Char('9')));
            assert_eq!(chooser.selected(), 1);
        }

        #[test]
        fn enter_sends_an_action_without_required_fields() {
            let mut chooser = review_chooser();
            match chooser.on_key(press(KeyCode::Enter)) {
                ChooserAction::Submit(submission) => {
                    assert_eq!(submission.index, 0);
                    assert_eq!(submission.label, "approved");
                    assert!(submission.values.is_empty());
                }
                other => panic!("expected a submission, got {other:?}"),
            }
        }

        #[test]
        fn enter_sends_an_action_with_only_optional_fields() {
            let mut chooser =
                Chooser::new(vec![ChooserItem::new("accept", "Akceptuje.")
                    .with_field(ChooserField::text("comment"))]);
            assert!(matches!(
                chooser.on_key(press(KeyCode::Enter)),
                ChooserAction::Submit(_)
            ));
        }

        #[test]
        fn a_required_field_blocks_the_send_with_a_complaint_on_the_field() {
            let mut chooser = review_chooser().selecting(1);
            chooser.on_key(press(KeyCode::Enter));
            assert_eq!(chooser.on_key(press(KeyCode::Enter)), ChooserAction::None);
            assert!(chooser.fields_of(1)[0]
                .error()
                .is_some_and(|error| error.contains("comment")));

            type_text(&mut chooser, "poprawić");
            assert_eq!(chooser.fields_of(1)[0].error(), None, "typing clears it");
            match chooser.on_key(press(KeyCode::Enter)) {
                ChooserAction::Submit(submission) => {
                    assert_eq!(
                        submission.values,
                        vec![("comment".to_string(), "poprawić".to_string())]
                    );
                }
                other => panic!("expected a submission, got {other:?}"),
            }
        }

        #[test]
        fn typing_in_a_field_is_not_a_command_and_q_is_text() {
            let mut chooser = review_chooser().selecting(1);
            assert!(!chooser.is_typing());
            chooser.on_key(press(KeyCode::Enter));
            assert!(chooser.is_typing());
            type_text(&mut chooser, "q?j1");
            assert_eq!(chooser.fields_of(1)[0].value(), "q?j1");
            assert_eq!(chooser.selected(), 1, "j and 1 were text, not navigation");
        }

        #[test]
        fn an_unavailable_action_reports_its_reason() {
            let mut chooser = Chooser::new(vec![
                ChooserItem::new("pauza", "Wstrzymuje.").unavailable("zadanie jest w kolejce")
            ]);
            assert_eq!(
                chooser.on_key(press(KeyCode::Enter)),
                ChooserAction::Blocked("zadanie jest w kolejce".to_string())
            );
        }

        #[test]
        fn esc_collapses_and_leaves_the_draft() {
            let mut chooser = review_chooser().selecting(1);
            chooser.on_key(press(KeyCode::Enter));
            type_text(&mut chooser, "szkic");
            assert_eq!(chooser.on_key(press(KeyCode::Esc)), ChooserAction::Cancel);
            assert_eq!(chooser.fields_of(1)[0].value(), "szkic");
        }

        #[test]
        fn a_proposed_value_is_marked_until_it_is_edited() {
            let mut chooser = Chooser::new(vec![ChooserItem::new("restart", "Od nowa.")
                .with_field(ChooserField::text("note").proposed("sprawdź testy"))]);
            assert!(chooser.fields_of(0)[0].is_proposed());
            chooser.on_key(press(KeyCode::Tab));
            chooser.on_key(press(KeyCode::Backspace));
            assert!(!chooser.fields_of(0)[0].is_proposed());
            assert_eq!(chooser.fields_of(0)[0].value(), "sprawdź test");
        }

        #[test]
        fn select_and_integer_fields_follow_the_form_rules() {
            let mut chooser = Chooser::new(vec![ChooserItem::new("nowe", "Tworzy zadanie.")
                .with_field(ChooserField::select(
                    "repo",
                    vec!["narsor".to_string(), "other".to_string()],
                ))
                .with_field(ChooserField::integer("count").required())]);
            chooser.on_key(press(KeyCode::Tab));
            chooser.on_key(press(KeyCode::Right));
            assert_eq!(chooser.fields_of(0)[0].value(), "other");
            chooser.on_key(press(KeyCode::Right));
            assert_eq!(
                chooser.fields_of(0)[0].value(),
                "narsor",
                "wraps like the form's select"
            );
            chooser.on_key(press(KeyCode::Down));
            type_text(&mut chooser, "1a2-");
            assert_eq!(
                chooser.fields_of(0)[1].value(),
                "12-",
                "letters are not numbers"
            );
            assert_eq!(chooser.on_key(press(KeyCode::Enter)), ChooserAction::None);
            assert!(
                chooser.fields_of(0)[1].error().is_some(),
                "12- is not an integer"
            );
        }
    }

    mod connection_state {
        use narsor::tui::connection::{Connection, LinkState};
        use narsor::tui::style::Role;
        use std::time::{Duration, Instant};

        #[test]
        fn live_to_retrying_to_down_to_live() {
            let start = Instant::now();
            let mut connection = Connection::new("local");
            assert_eq!(connection.state(), LinkState::Connecting);
            assert!(connection.status(start).text.contains("łączę z local"));

            connection.on_data(start);
            let status = connection.status(start);
            assert_eq!(
                (status.icon, status.text.as_str(), status.role),
                ("●", "live", Role::Live)
            );
            assert_eq!(connection.mutation_gate(start), Ok(()));
            assert_eq!(connection.age_label(start), None);

            let dropped = start + Duration::from_secs(10);
            connection.on_failure(dropped, Some(Duration::from_secs(12)));
            let status = connection.status(dropped);
            assert_eq!(
                (status.icon, status.text.as_str(), status.role),
                ("◌", "ponawiam (12s)", Role::Attention)
            );
            assert_eq!(
                connection
                    .status(dropped + Duration::from_millis(2500))
                    .text,
                "ponawiam (10s)",
                "the countdown is real and rounds up"
            );
            let blocked = connection
                .mutation_gate(dropped + Duration::from_secs(30))
                .expect_err("not live: mutations are refused");
            assert!(blocked.reason.contains("ponawiam"), "{}", blocked.reason);
            assert!(
                blocked.reason.contains("stan sprzed 40s"),
                "{}",
                blocked.reason
            );

            let given_up = dropped + Duration::from_secs(40);
            connection.on_failure(given_up, None);
            let status = connection.status(given_up);
            assert_eq!(
                (status.icon, status.text.as_str(), status.role),
                ("✖", "brak połączenia", Role::Problem)
            );
            assert_eq!(
                connection.age_label(given_up),
                Some("stan sprzed 50s".to_string())
            );
            assert!(connection.is_stale());
            assert!(connection.mutation_gate(given_up).is_err());

            let back = given_up + Duration::from_secs(5);
            connection.on_data(back);
            assert!(connection.is_live() && !connection.is_stale());
            assert_eq!(connection.status(back).text, "live");
            assert_eq!(connection.mutation_gate(back), Ok(()));
            assert_eq!(connection.age_label(back), None);
        }

        #[test]
        fn ages_use_the_largest_short_unit() {
            let start = Instant::now();
            let mut connection = Connection::new("local");
            connection.on_data(start);
            connection.on_failure(start, None);
            for (seconds, expected) in [
                (5, "stan sprzed 5s"),
                (180, "stan sprzed 3m"),
                (7300, "stan sprzed 2h"),
            ] {
                assert_eq!(
                    connection.age_label(start + Duration::from_secs(seconds)),
                    Some(expected.to_string())
                );
            }
        }

        #[test]
        fn without_any_data_there_is_nothing_to_grey_or_age() {
            let now = Instant::now();
            let mut connection = Connection::new("remote");
            connection.on_failure(now, None);
            assert!(!connection.is_stale());
            assert_eq!(connection.age_label(now), None);
            assert!(connection.mutation_gate(now).is_err());
        }
    }

    mod style_roles {
        use narsor::tui::style::{no_color_requested, Role, Styles};
        use narsor::tui::transcript::TranscriptPresentation;
        use ratatui::style::{Color, Modifier, Style};

        const KINDS: [TranscriptPresentation; 7] = [
            TranscriptPresentation::Neutral,
            TranscriptPresentation::Operator,
            TranscriptPresentation::Agent,
            TranscriptPresentation::Conductor,
            TranscriptPresentation::System,
            TranscriptPresentation::Technical,
            TranscriptPresentation::Error,
        ];

        fn all_styles(styles: &Styles) -> Vec<Style> {
            let mut all: Vec<Style> = Role::ALL.iter().map(|role| styles.of(*role)).collect();
            for kind in KINDS {
                all.push(styles.chat_header(kind));
                all.push(styles.chat_body(kind));
            }
            all.push(styles.key_cap());
            all.push(styles.selection_marks(true).style);
            all.push(styles.selection_marks(false).style);
            all
        }

        #[test]
        fn no_color_is_set_and_not_empty() {
            assert!(!no_color_requested(None));
            assert!(!no_color_requested(Some("".into())));
            assert!(no_color_requested(Some("1".into())));
        }

        #[test]
        fn colour_mode_uses_only_the_sixteen_ansi_colours() {
            for style in all_styles(&Styles::new(false)) {
                for colour in [style.fg, style.bg].into_iter().flatten() {
                    // The conductor's brown has no ANSI equivalent.
                    if colour == Color::Indexed(130) {
                        continue;
                    }
                    assert!(
                        !matches!(colour, Color::Rgb(..) | Color::Indexed(_) | Color::Reset),
                        "{colour:?} is not one of the 16 ANSI colours"
                    );
                }
            }
        }

        #[test]
        fn no_color_mode_leaves_only_bold_dim_underline_and_reverse() {
            let allowed =
                Modifier::BOLD | Modifier::DIM | Modifier::UNDERLINED | Modifier::REVERSED;
            for style in all_styles(&Styles::new(true)) {
                assert_eq!((style.fg, style.bg), (None, None), "{style:?}");
                assert!(allowed.contains(style.add_modifier), "{style:?}");
            }
        }

        #[test]
        fn no_role_is_reverse_video_in_either_mode() {
            for no_color in [false, true] {
                let styles = Styles::new(no_color);
                for role in Role::ALL {
                    assert!(
                        !styles.of(role).add_modifier.contains(Modifier::REVERSED),
                        "{role:?} with no_color={no_color}"
                    );
                }
            }
        }

        #[test]
        fn selection_marks_are_thin_in_the_focus_or_the_background_role() {
            let colour = Styles::new(false);
            let focused = colour.selection_marks(true);
            assert_eq!((focused.rule, focused.bar), ("─", "│"));
            assert_eq!(focused.style, colour.of(Role::Focus));
            assert_eq!(focused.style.fg, Some(Color::Cyan));
            let unfocused = colour.selection_marks(false);
            assert_eq!((unfocused.rule, unfocused.bar), ("─", "│"));
            assert_eq!(unfocused.style, colour.of(Role::Done));

            let plain = Styles::new(true);
            let focused = plain.selection_marks(true);
            assert_eq!((focused.rule, focused.bar), ("─", "│"));
            assert!(focused.style.add_modifier.contains(Modifier::BOLD));
            let unfocused = plain.selection_marks(false);
            assert_eq!((unfocused.rule, unfocused.bar), ("─", "│"));
            assert!(unfocused.style.add_modifier.contains(Modifier::DIM));
            for marks in [focused, unfocused] {
                assert!(!marks.style.add_modifier.contains(Modifier::REVERSED));
            }
        }

        #[test]
        fn a_workflow_choice_is_a_background_of_its_own_or_reversed() {
            let colour = Styles::new(false);
            let choice = colour.workflow_choice_background();
            assert!(choice.bg.is_some());
            assert_eq!(choice.fg, None);
            assert!(!choice.add_modifier.contains(Modifier::REVERSED));
            for other in [
                colour.selection_fill(),
                colour.answer_panel_background(),
                colour.field_editing(),
            ] {
                assert_ne!(choice.bg, other.bg, "{other:?}");
            }

            let plain = Styles::new(true).workflow_choice_background();
            assert_eq!((plain.fg, plain.bg), (None, None));
            assert!(plain.add_modifier.contains(Modifier::REVERSED));
        }

        #[test]
        fn roles_follow_the_origin_table() {
            let styles = Styles::new(false);
            assert_eq!(styles.of(Role::Attention).fg, Some(Color::Yellow));
            assert!(styles
                .of(Role::Attention)
                .add_modifier
                .contains(Modifier::BOLD));
            assert_eq!(styles.of(Role::Problem).fg, Some(Color::Red));
            assert_eq!(styles.of(Role::Live).fg, Some(Color::Green));
            assert_eq!(styles.of(Role::Calm), Style::default());
            assert_eq!(styles.of(Role::Done).fg, Some(Color::DarkGray));
            assert_eq!(styles.of(Role::Focus).fg, Some(Color::Cyan));
            assert_eq!(styles.of(Role::Conductor).fg, Some(Color::Indexed(130)));
            assert_eq!(styles.of(Role::Link).fg, Some(Color::Blue));
            assert!(styles
                .of(Role::Link)
                .add_modifier
                .contains(Modifier::UNDERLINED));
        }

        #[test]
        fn the_conversation_kinds_stay_distinguishable_in_colour() {
            let styles = Styles::new(false);
            let headers: Vec<Style> = [
                TranscriptPresentation::Operator,
                TranscriptPresentation::Agent,
                TranscriptPresentation::Conductor,
                TranscriptPresentation::Error,
            ]
            .into_iter()
            .map(|kind| styles.chat_header(kind))
            .collect();
            for (index, header) in headers.iter().enumerate() {
                assert!(headers[index + 1..]
                    .iter()
                    .all(|other| other.fg != header.fg));
            }
        }
    }
}

/// U8: the task application's list — stable order, stream patching, header
/// counters, the finished-tasks filter, moving queued tasks and the
/// connection. Most of it needs no server (a listing and events are handed
/// in).
mod task_app_list {
    use std::time::Instant;

    use crossterm::event::KeyCode;

    use narsor::api::{ApiClient, ClientError};
    use narsor::tui::connection::LinkState;
    use narsor::tui::task_app::list::StateIcon;
    use narsor::tui::task_app::notify::Severity;
    use narsor::tui::task_app::{layout, DetailTab, EventFeed, Focus, Request, TaskApp};

    use narsor_engine::domain::ExecutionStatus;
    use narsor_engine::orchestration::{
        AttentionKind, Observation, QueueReason, RunnerState, TaskEventKind,
    };
    use narsor_protocol::controller::MoveTaskTarget;

    use super::key;

    use super::task_app_support::{
        at, event, ids, listing, queue_view, queued, task, titled, waiting, waiting_for_capacity,
        with_controller, with_runner,
    };

    fn offline_client() -> ApiClient {
        ApiClient::new("127.0.0.1:9".parse().unwrap(), "token")
    }

    fn app() -> TaskApp {
        TaskApp::new(offline_client(), "local", None)
    }

    fn press(app: &mut TaskApp, code: KeyCode) -> Request {
        app.on_key(key(code))
    }

    fn ch(app: &mut TaskApp, character: char) -> Request {
        press(app, KeyCode::Char(character))
    }

    fn titles(app: &TaskApp) -> Vec<String> {
        app.list().rows().into_iter().map(|row| row.title).collect()
    }

    #[test]
    fn a_moved_event_at_the_revision_a_read_already_showed_still_reorders_the_neighbours() {
        let mut app = app();
        // A read that landed between the renumbering of the neighbours and
        // the commit: the moved row is at the event's revision already, the
        // others are still in the old order.
        let mut moved = queued("c", 3);
        moved.revision = 2;
        app.load(listing(vec![queued("a", 1), queued("b", 2), moved], "c0"));
        assert_eq!(titles(&app), ["a", "b", "c"]);

        app.on_stream(event(
            "c1",
            "c",
            2,
            RunnerState::Queued,
            TaskEventKind::Moved {
                rank: 1,
                order: ids(&["c", "a", "b"]),
            },
        ));
        assert_eq!(titles(&app), ["c", "a", "b"]);

        // The same event again, or an older one, changes nothing.
        app.on_stream(event(
            "c1",
            "c",
            1,
            RunnerState::Queued,
            TaskEventKind::Moved {
                rank: 3,
                order: ids(&["a", "b", "c"]),
            },
        ));
        assert_eq!(titles(&app), ["c", "a", "b"]);
    }

    fn row(app: &TaskApp, title: &str) -> (StateIcon, String) {
        let row = app
            .list()
            .rows()
            .into_iter()
            .find(|row| row.title == title)
            .unwrap_or_else(|| panic!("no row {title}"));
        (row.icon, row.label)
    }

    fn note(app: &TaskApp) -> String {
        app.notifier()
            .latest()
            .map(|note| note.text.clone())
            .unwrap_or_default()
    }

    fn controlled(
        id: &str,
        backend: &str,
        runner_state: &str,
        rank: u32,
    ) -> narsor_engine::controller_protocol::TaskSummary {
        with_controller(task(id, backend, runner_state, rank))
    }

    /// The read the application does after an event: the task as the control
    /// plane sees it now, `change` applied and its state derived again.
    fn reread(
        app: &mut TaskApp,
        id: &str,
        revision: u64,
        change: impl FnOnce(&mut narsor_engine::controller_protocol::TaskSummary),
    ) {
        let mut tasks = app.list().tasks().to_vec();
        let task = tasks
            .iter_mut()
            .find(|task| task.task_id.as_str() == id)
            .unwrap();
        change(task);
        task.state = super::task_app_support::served_state(task);
        task.revision = revision;
        app.load(listing(tasks, "read"));
    }

    mod order {
        use super::*;

        /// Three tasks go queued → provisioning → running → done one after
        /// the other; the rows never move, icons and labels do.
        #[test]
        fn a_state_stream_never_reorders_the_rows() {
            let mut app = app();
            let queued_all = || {
                listing(
                    vec![
                        controlled("a", "local", "queued", 1),
                        controlled("b", "local", "queued", 2),
                        controlled("c", "local", "queued", 3),
                    ],
                    "c0",
                )
            };
            app.load(queued_all());
            let order = vec!["a", "b", "c"];
            assert_eq!(titles(&app), order);
            assert_eq!(row(&app, "b").1, "#2");

            let mut sequence = 1;
            for (index, id) in ["a", "b", "c"].into_iter().enumerate() {
                sequence += 1;
                app.on_stream(event(
                    &format!("p{id}"),
                    id,
                    sequence,
                    RunnerState::Provisioning,
                    TaskEventKind::ControllerReported {
                        observation: Observation::StartAccepted,
                        lease_epoch: 1,
                    },
                ));
                assert_eq!(titles(&app), order, "{id} provisioning");
                assert!(app.refresh_wanted(), "the event carries no state to show");
                reread(&mut app, id, sequence, |task| task.holds_slot = true);
                assert_eq!(row(&app, id).0, StateIcon::Starting);
                // Whoever waits behind moves up in the queue, not on screen.
                let waiting_behind = ["a", "b", "c"].iter().skip(index + 1).count();
                if waiting_behind > 0 {
                    let next = ["a", "b", "c"][index + 1];
                    assert_eq!(row(&app, next).1, "#1");
                }

                sequence += 1;
                app.on_stream(event(
                    &format!("r{id}"),
                    id,
                    sequence,
                    RunnerState::Running,
                    TaskEventKind::ControllerReported {
                        observation: Observation::RunnerReady,
                        lease_epoch: 1,
                    },
                ));
                assert_eq!(titles(&app), order, "{id} running");
                reread(&mut app, id, sequence, |task| task.holds_slot = true);
                assert_eq!(row(&app, id).0, StateIcon::Running);

                // The debounced read brings the runner's step.
                let mut tasks = app.list().tasks().to_vec();
                tasks[index] =
                    with_runner(tasks[index].clone(), ExecutionStatus::Running, "implement");
                tasks[index].revision = sequence;
                app.load(listing(tasks, "read"));
                assert_eq!(row(&app, id).1, "implement");

                sequence += 1;
                app.on_stream(event(
                    &format!("d{id}"),
                    id,
                    sequence,
                    RunnerState::Running,
                    TaskEventKind::RunnerReported {
                        incarnation: "pod-1".into(),
                        event_cursor: 9,
                        execution_status: ExecutionStatus::Completed,
                        pending_input_id: None,
                        attention: Vec::new(),
                    },
                ));
                assert_eq!(titles(&app), order, "{id} done");
                reread(&mut app, id, sequence, |task| {
                    task.runner.as_mut().unwrap().execution_status = ExecutionStatus::Completed;
                });
                assert_eq!(row(&app, id), (StateIcon::Done, String::new()));
            }
        }

        #[test]
        fn a_stale_event_does_not_undo_a_newer_read() {
            let mut app = app();
            let mut running = controlled("a", "local", "running", 1);
            running.revision = 7;
            app.load(listing(vec![running], "c7"));
            app.on_stream(event(
                "old",
                "a",
                5,
                RunnerState::Queued,
                TaskEventKind::Created,
            ));
            assert_eq!(row(&app, "a").0, StateIcon::Running);
            assert_eq!(
                app.cursor(),
                Some("c7"),
                "a dropped event does not move the cursor"
            );
            assert!(!app.refresh_wanted());
        }

        #[test]
        fn the_list_is_never_sorted_by_state() {
            let mut app = app();
            app.load(listing(
                vec![
                    controlled("done", "local", "running", 1),
                    controlled("queue", "local", "queued", 2),
                    controlled("fail", "local", "interrupted", 3),
                    controlled("run", "local", "running", 4),
                ],
                "c",
            ));
            assert_eq!(titles(&app), ["done", "queue", "fail", "run"]);
        }

        #[test]
        fn slash_filters_by_ticket_title_and_full_id_without_reordering() {
            let mut app = app();
            let alpha = titled(queued("task-a", 1), "Alpha");
            let mut beta = titled(queued("task-b0123456", 2), "Beta");
            beta.ticket = Some("NAR-42".into());
            let gamma = titled(queued("task-c", 3), "Parser cleanup");
            app.load(listing(
                vec![alpha.clone(), beta.clone(), gamma.clone()],
                "c",
            ));
            app.list_mut().select("task-b0123456");

            ch(&mut app, '/');
            for character in "nar-42".chars() {
                ch(&mut app, character);
            }
            assert_eq!(app.list().visible_ids(), ["task-b0123456"]);
            assert_eq!(app.list().selected_id(), Some("task-b0123456"));
            press(&mut app, KeyCode::Enter);

            ch(&mut app, '/');
            for _ in "nar-42".chars() {
                press(&mut app, KeyCode::Backspace);
            }
            for character in "PARSER".chars() {
                ch(&mut app, character);
            }
            assert_eq!(app.list().visible_ids(), ["task-c"]);
            press(&mut app, KeyCode::Enter);

            ch(&mut app, '/');
            for _ in "PARSER".chars() {
                press(&mut app, KeyCode::Backspace);
            }
            for character in "b012".chars() {
                ch(&mut app, character);
            }
            assert_eq!(app.list().visible_ids(), ["task-b0123456"]);
            press(&mut app, KeyCode::Enter);

            ch(&mut app, '/');
            for character in "x".chars() {
                ch(&mut app, character);
            }
            press(&mut app, KeyCode::Esc);
            assert_eq!(app.list().visible_ids(), ["task-b0123456"]);

            ch(&mut app, '/');
            for _ in "b012".chars() {
                press(&mut app, KeyCode::Backspace);
            }
            press(&mut app, KeyCode::Enter);
            assert_eq!(
                app.list().visible_ids(),
                ["task-a", "task-b0123456", "task-c"]
            );
            assert_eq!(
                app.list()
                    .tasks()
                    .iter()
                    .map(|task| task.task_id.as_str())
                    .collect::<Vec<_>>(),
                ["task-a", "task-b0123456", "task-c"]
            );
        }

        #[test]
        fn activity_timestamp_uses_the_state_specific_source_and_skips_final_states() {
            use narsor::tui::task_app::list::TaskList;
            use narsor_engine::orchestration::TaskState;

            let now = at();
            let mut queued = queued("queued", 1);
            queued.state_since = Some(now - chrono::Duration::hours(2));
            queued.queue = Some(queue_view(QueueReason::Admitted));
            let queue_since = queued.queue.as_ref().unwrap().since;

            let mut waiting = task("waiting", "local", "running", 2);
            waiting.state = TaskState::HumanNeeded;
            let attention_since = now - chrono::Duration::minutes(12);
            waiting
                .attention
                .push(narsor_protocol::controller::AttentionItem {
                    kind: AttentionKind::Approval,
                    state_id: Some("review".into()),
                    since: attention_since,
                    waiting_id: "approval-1".into(),
                });

            let mut provisioning = task("provisioning", "local", "provisioning", 3);
            provisioning.state_since = Some(now - chrono::Duration::seconds(30));
            provisioning.queue = Some(queue_view(QueueReason::Admitted));
            let provisioning_since = provisioning.state_since;

            let mut running = task("running", "local", "running", 4);
            running.state_since = Some(now - chrono::Duration::minutes(5));
            let running_since = running.state_since;

            let mut finished = task("finished", "local", "stopped", 5);
            finished.state = TaskState::Finished;
            finished.state_since = Some(now - chrono::Duration::minutes(9));

            let mut list = TaskList::new();
            list.replace(vec![queued, waiting, provisioning, running, finished]);
            let rows = list.rows();
            assert_eq!(rows[0].since, Some(queue_since));
            assert_eq!(rows[1].since, Some(attention_since));
            assert_eq!(rows[2].since, provisioning_since);
            assert_eq!(rows[3].since, running_since);
            assert_eq!(rows[4].since, None);
        }

        #[test]
        fn an_event_for_an_unknown_task_asks_for_a_debounced_read() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1)], "c"));
            app.on_stream(event(
                "n",
                "new",
                1,
                RunnerState::Queued,
                TaskEventKind::Created,
            ));
            assert!(app.refresh_wanted());
            let due = app.next_refresh_due().expect("a refresh is due");
            assert!(due > Instant::now(), "debounced, not immediate");
        }

        #[test]
        fn queue_changes_and_answers_patch_the_row_without_a_read() {
            let mut app = app();
            let mut waiting_task = waiting(
                controlled("a", "local", "running", 1),
                AttentionKind::Question,
            );
            waiting_task.runner = Some(narsor_runner());
            app.load(listing(vec![waiting_task, queued("b", 2)], "c"));
            assert_eq!(row(&app, "a").0, StateIcon::NeedsReply);

            let waiting_id = app.list().tasks()[0].attention[0].waiting_id.clone();
            app.on_stream(event(
                "i",
                "a",
                2,
                RunnerState::Running,
                TaskEventKind::InputQueued {
                    input_id: "in-1".into(),
                    pending_input_id: waiting_id,
                },
            ));
            assert!(!row(&app, "a").0.needs_operator());
            assert!(!app.refresh_wanted(), "an answer shows without a read");

            app.on_stream(event(
                "q",
                "b",
                2,
                RunnerState::Queued,
                TaskEventKind::QueueChanged {
                    reason: QueueReason::ControllerOffline,
                },
            ));
            assert_eq!(row(&app, "b").0, StateIcon::Blocked);
            assert!(!app.refresh_wanted());
        }

        fn report(
            id: &str,
            sequence: u64,
            event_cursor: u64,
            status: ExecutionStatus,
            pending_input_id: Option<String>,
            attention: Vec<narsor_protocol::controller::AttentionItem>,
        ) -> Option<Result<narsor_protocol::controller::ControlEventEnvelope, ClientError>>
        {
            event(
                id,
                "a",
                sequence,
                RunnerState::Running,
                TaskEventKind::RunnerReported {
                    incarnation: "pod-1".into(),
                    event_cursor,
                    execution_status: status,
                    pending_input_id,
                    attention,
                },
            )
        }

        fn running_app(with_runner: bool) -> TaskApp {
            let mut app = app();
            let mut task = controlled("a", "local", "running", 1);
            if with_runner {
                task.runner = Some(narsor_runner());
            }
            app.load(listing(vec![task], "c"));
            app
        }

        #[test]
        fn a_report_that_moves_only_the_event_cursor_asks_for_no_read() {
            let mut app = running_app(true);
            let cursor = app.list().tasks()[0].runner.as_ref().unwrap().event_cursor;
            for step in 1..=10 {
                app.on_stream(report(
                    &format!("r{step}"),
                    step + 1,
                    cursor + step,
                    ExecutionStatus::Running,
                    None,
                    Vec::new(),
                ));
                assert!(!app.refresh_wanted(), "report {step} changes no row field");
            }
            let runner = app.list().tasks()[0].runner.as_ref().unwrap();
            assert_eq!(runner.event_cursor, cursor + 10);
        }

        #[test]
        fn a_report_that_changes_state_question_or_attention_asks_for_a_read() {
            let mut app = running_app(true);
            app.on_stream(report(
                "s",
                2,
                50,
                ExecutionStatus::Completed,
                None,
                Vec::new(),
            ));
            assert!(app.refresh_wanted(), "status changed");

            let mut app = running_app(true);
            app.on_stream(report(
                "p",
                2,
                50,
                ExecutionStatus::Running,
                Some("wait-1".into()),
                Vec::new(),
            ));
            assert!(app.refresh_wanted(), "a question appeared");

            let item = waiting(
                controlled("a", "local", "running", 1),
                AttentionKind::Question,
            )
            .attention[0]
                .clone();
            let mut app = running_app(true);
            app.on_stream(report(
                "t",
                2,
                50,
                ExecutionStatus::Running,
                None,
                vec![item],
            ));
            assert!(app.refresh_wanted(), "attention changed");
        }

        #[test]
        fn a_report_for_a_row_without_a_runner_asks_for_a_read() {
            let mut app = running_app(false);
            app.on_stream(report(
                "n",
                2,
                50,
                ExecutionStatus::Running,
                None,
                Vec::new(),
            ));
            assert!(app.refresh_wanted());
        }

        fn narsor_runner() -> narsor_engine::controller_protocol::RunnerView {
            super::super::task_app_support::runner(ExecutionStatus::Running, "review")
        }

        #[test]
        fn a_problem_wins_over_a_question_in_the_attention_column() {
            let mut app = app();
            let both = waiting(
                waiting(
                    controlled("a", "local", "running", 1),
                    AttentionKind::Question,
                ),
                AttentionKind::FailedStep,
            );
            app.load(listing(vec![both], "c"));
            assert_eq!(row(&app, "a").0, StateIcon::Problem);
            let counters = app.list().counters();
            assert_eq!((counters.questions, counters.problems), (0, 1));
        }
    }

    mod waiting_and_header {
        use super::*;

        #[test]
        fn space_walks_the_waiting_tasks_in_a_circle_and_the_rows_stand_still() {
            let mut app = app();
            app.load(listing(
                vec![
                    controlled("a", "local", "running", 1),
                    waiting(
                        controlled("b", "local", "running", 2),
                        AttentionKind::Question,
                    ),
                    controlled("c", "local", "running", 3),
                    waiting(
                        controlled("d", "local", "running", 4),
                        AttentionKind::Problem,
                    ),
                ],
                "c",
            ));
            let order = titles(&app);
            let mut visited = Vec::new();
            for _ in 0..5 {
                ch(&mut app, ' ');
                visited.push(app.list().selected_id().unwrap().to_string());
                assert_eq!(titles(&app), order, "the rows do not move");
            }
            assert_eq!(visited, ["b", "d", "b", "d", "b"]);
        }

        #[test]
        fn space_with_nobody_waiting_says_so() {
            let mut app = app();
            app.load(listing(vec![controlled("a", "local", "running", 1)], "c"));
            ch(&mut app, ' ');
            assert_eq!(app.list().selected_id(), Some("a"));
            assert!(note(&app).contains("nie czeka"));
        }

        #[test]
        fn the_header_counts_questions_and_problems_and_slots() {
            let mut app = app();
            app.load(listing(
                vec![
                    waiting(
                        controlled("a", "local", "running", 1),
                        AttentionKind::Approval,
                    ),
                    waiting(
                        controlled("b", "local", "running", 2),
                        AttentionKind::ProvisioningFailed,
                    ),
                    controlled("c", "local", "running", 3),
                    waiting_for_capacity(queued("d", 4), 1, 2, 3),
                ],
                "c",
            ));
            let counters = app.list().counters();
            assert_eq!((counters.questions, counters.problems), (1, 1));
            let slots = app.list().slots().expect("slots");
            assert_eq!((slots.used, slots.limit), (2, Some(3)));
        }

        #[test]
        fn slots_without_a_waiting_task_count_what_holds_compute() {
            let mut app = app();
            app.load(listing(
                vec![
                    controlled("a", "local", "running", 1),
                    controlled("b", "local", "paused", 2),
                    controlled("c", "local", "provisioning", 3),
                ],
                "c",
            ));
            let slots = app.list().slots().expect("slots");
            assert_eq!((slots.used, slots.limit), (2, None));
        }

        #[test]
        fn a_title_comes_from_display_and_falls_back_to_the_id() {
            let mut app = app();
            app.load(listing(
                vec![
                    titled(queued("a", 1), "Zmiana API\u{1b}[31m"),
                    queued("plain", 2),
                ],
                "c",
            ));
            let shown = titles(&app);
            assert_eq!(shown[1], "plain");
            assert!(!shown[0].contains('\u{1b}'), "{:?}", shown[0]);
            assert!(shown[0].starts_with("Zmiana API"));
        }
    }

    mod row_title {
        use super::*;

        fn with_title(
            id: &str,
            title: Option<&str>,
        ) -> narsor_engine::controller_protocol::TaskSummary {
            let mut task = queued(id, 1);
            task.title = title.map(str::to_string);
            task
        }

        fn row_title_after_load(task: narsor_engine::controller_protocol::TaskSummary) -> String {
            let mut app = app();
            app.load(listing(vec![task], "c"));
            titles(&app).remove(0)
        }

        #[test]
        fn the_row_shows_the_task_title() {
            let task = with_title("a", Some("Zmiana API zamówień"));
            assert_eq!(row_title_after_load(task), "Zmiana API zamówień");
        }

        #[test]
        fn without_a_title_the_row_shows_the_task_id_until_a_refresh_brings_one() {
            let mut app = app();
            app.load(listing(vec![with_title("a", None)], "c1"));
            assert_eq!(titles(&app), ["a"]);

            let mut retitled = with_title("a", Some("Tytuł dyrygenta"));
            retitled.revision = 2;
            app.load(listing(vec![retitled], "c2"));
            assert_eq!(titles(&app), ["Tytuł dyrygenta"]);
        }

        #[test]
        fn a_blank_title_shows_the_task_id() {
            let task = with_title("task-a", Some("   "));
            assert_eq!(row_title_after_load(task), "task-a");
        }

        #[test]
        fn a_title_with_a_new_line_is_shown_on_one_line() {
            let task = with_title("a", Some("Linia1\nLinia2"));
            let title = row_title_after_load(task);
            assert!(!title.contains('\n'), "{title:?}");
            assert!(
                title.starts_with("Linia1") && title.ends_with("Linia2"),
                "{title:?}"
            );
        }

        #[test]
        fn the_filter_matches_the_task_title_and_the_id() {
            let mut app = app();
            app.load(listing(
                vec![
                    with_title("task-a", Some("Zmiana API zamówień")),
                    with_title("task-b", None),
                ],
                "c",
            ));

            ch(&mut app, '/');
            for character in "api zam".chars() {
                ch(&mut app, character);
            }
            assert_eq!(app.list().visible_ids(), ["task-a"]);
            for _ in "api zam".chars() {
                press(&mut app, KeyCode::Backspace);
            }
            for character in "task-b".chars() {
                ch(&mut app, character);
            }
            assert_eq!(app.list().visible_ids(), ["task-b"]);
        }
    }

    mod refresh_and_filter {
        use super::*;

        #[test]
        fn a_read_changes_neither_the_selection_nor_the_focus_nor_the_tab_nor_the_draft() {
            let mut app = app();
            app.load(listing(
                vec![queued("a", 1), queued("b", 2), queued("c", 3)],
                "c1",
            ));
            ch(&mut app, 'j');
            press(&mut app, KeyCode::Enter);
            press(&mut app, KeyCode::Char(']'));
            app.detail_mut().set_draft("b", "Uwzględnij migrację");
            assert_eq!(app.list().selected_id(), Some("b"));
            assert_eq!(app.focus(), Focus::Details);
            assert_eq!(app.detail().tab(), DetailTab::Progress);

            let mut moved = vec![
                controlled("a", "local", "running", 1),
                controlled("b", "local", "queued", 2),
                controlled("c", "local", "queued", 3),
            ];
            moved[1].revision = 4;
            app.load(listing(moved, "c2"));

            assert_eq!(app.list().selected_id(), Some("b"));
            assert_eq!(app.focus(), Focus::Details);
            assert_eq!(app.detail().tab(), DetailTab::Progress);
            assert_eq!(app.detail().draft("b"), Some("Uwzględnij migrację"));
            assert_eq!(app.cursor(), Some("c2"));
        }

        #[test]
        fn z_hides_finished_tasks_keeps_the_order_and_a_second_z_brings_them_back() {
            let mut app = app();
            let done = |id: &str, rank| {
                with_runner(
                    controlled(id, "local", "running", rank),
                    ExecutionStatus::Completed,
                    "done",
                )
            };
            app.load(listing(
                vec![
                    queued("a", 1),
                    done("x", 2),
                    queued("b", 3),
                    done("y", 4),
                    queued("c", 5),
                ],
                "c",
            ));
            assert_eq!(titles(&app), ["a", "x", "b", "y", "c"]);
            ch(&mut app, 'z');
            assert_eq!(titles(&app), ["a", "b", "c"]);
            assert_eq!(app.list().hidden_count(), 2);
            assert_eq!(app.list().tasks().len(), 5, "hidden, not removed");
            ch(&mut app, 'z');
            assert_eq!(titles(&app), ["a", "x", "b", "y", "c"]);
        }

        #[test]
        fn hiding_the_selected_finished_task_moves_the_selection_to_the_row_below() {
            let mut app = app();
            let done = with_runner(
                controlled("x", "local", "running", 2),
                ExecutionStatus::Completed,
                "done",
            );
            app.load(listing(vec![queued("a", 1), done, queued("b", 3)], "c"));
            ch(&mut app, 'j');
            assert_eq!(app.list().selected_id(), Some("x"));
            ch(&mut app, 'z');
            assert_eq!(app.list().selected_id(), Some("b"));
        }

        #[test]
        fn a_selected_task_archived_from_the_cli_hands_the_selection_to_the_row_below() {
            let mut app = app();
            app.load(listing(
                vec![queued("a", 1), queued("b", 2), queued("c", 3)],
                "c0",
            ));
            ch(&mut app, 'j');
            assert_eq!(app.list().selected_id(), Some("b"));

            app.on_stream(event(
                "arch",
                "b",
                2,
                RunnerState::Archived,
                TaskEventKind::Created,
            ));
            assert_eq!(titles(&app), ["a", "c"]);
            assert_eq!(app.list().selected_id(), Some("c"));
            let text = note(&app);
            assert!(
                text.contains("b") && text.contains("zarchiwizowane") && text.contains("c"),
                "{text}"
            );
        }

        #[test]
        fn archiving_the_last_row_selects_the_one_above() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1), queued("b", 2)], "c0"));
            ch(&mut app, 'G');
            app.load(listing(vec![queued("a", 1)], "c1"));
            assert_eq!(app.list().selected_id(), Some("a"));
            assert!(note(&app).contains("zaznaczono a"), "{}", note(&app));
        }
    }

    mod layouts {
        use ratatui::layout::Rect;

        use super::*;
        use layout::Screen;

        #[test]
        fn a_wide_terminal_splits_into_a_narrow_list_and_the_details() {
            let areas = layout::areas(Rect::new(0, 0, 120, 30), Focus::List);
            assert_eq!(areas.screen, Screen::Split);
            assert_eq!(areas.list.unwrap().width, layout::LIST_WIDTH);
            assert_eq!(areas.detail.unwrap().width, 120 - layout::LIST_WIDTH);
            // No frame around the screen: the header is row 0, the bar the
            // last row, and the panels fill what is between them.
            assert_eq!(areas.header, Rect::new(0, 0, 120, 1));
            assert_eq!(areas.bar, Rect::new(0, 29, 120, 1));
            assert_eq!(areas.list.unwrap(), Rect::new(0, 1, layout::LIST_WIDTH, 28));
        }

        #[test]
        fn under_100_columns_the_list_and_the_details_take_turns_full_screen() {
            let on_list = layout::areas(Rect::new(0, 0, 90, 30), Focus::List);
            assert_eq!(on_list.screen, Screen::Full);
            assert!(on_list.list.is_some() && on_list.detail.is_none());
            let on_details = layout::areas(Rect::new(0, 0, 90, 30), Focus::Details);
            assert!(on_details.list.is_none() && on_details.detail.is_some());
            assert_eq!(
                layout::areas(Rect::new(0, 0, 100, 30), Focus::List).screen,
                Screen::Split
            );
            assert_eq!(
                layout::areas(Rect::new(0, 0, 99, 30), Focus::List).screen,
                Screen::Full
            );
        }

        #[test]
        fn under_60_by_20_there_is_only_a_request_for_room() {
            for (width, height) in [(50, 15), (59, 30), (100, 19)] {
                assert_eq!(
                    layout::areas(Rect::new(0, 0, width, height), Focus::List).screen,
                    Screen::TooSmall,
                    "{width}x{height}"
                );
            }
            assert_eq!(
                layout::areas(Rect::new(0, 0, 60, 20), Focus::List).screen,
                Screen::Full
            );
        }

        #[test]
        fn enter_goes_to_the_details_and_esc_comes_back_with_the_selection() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1), queued("b", 2)], "c"));
            ch(&mut app, 'j');
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.focus(), Focus::Details);
            press(&mut app, KeyCode::Esc);
            assert_eq!(app.focus(), Focus::List);
            assert_eq!(app.list().selected_id(), Some("b"));
        }
    }

    mod moving {
        use super::*;

        fn ctrl_key(app: &mut TaskApp, code: KeyCode) -> Request {
            app.on_key(crossterm::event::KeyEvent::new(
                code,
                crossterm::event::KeyModifiers::CONTROL,
            ))
        }

        fn expect_move(request: Request) -> narsor::tui::task_app::list::MoveRequest {
            match request {
                Request::Move(request) => request,
                other => panic!("a move was expected, got {other:?}"),
            }
        }

        #[test]
        fn ctrl_up_and_down_ask_to_go_before_the_row_above_and_after_the_row_below() {
            let mut app = app();
            app.load(listing(
                vec![queued("a", 1), queued("b", 2), queued("c", 3)],
                "c",
            ));
            ch(&mut app, 'j');
            let up = expect_move(ctrl_key(&mut app, KeyCode::Up));
            assert_eq!(up.target, MoveTaskTarget::Before("a".into()));
            let down = expect_move(ctrl_key(&mut app, KeyCode::Down));
            assert_eq!(down.target, MoveTaskTarget::After("c".into()));
            assert_eq!(down.generation, 1);
        }

        #[test]
        fn ctrl_up_on_the_first_row_and_ctrl_down_on_the_last_say_there_is_nowhere_to_go() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1), queued("b", 2)], "c"));
            assert_eq!(ctrl_key(&mut app, KeyCode::Up), Request::None);
            assert!(note(&app).contains("pierwsze"), "{}", note(&app));
            ch(&mut app, 'G');
            assert_eq!(ctrl_key(&mut app, KeyCode::Down), Request::None);
            assert!(note(&app).contains("ostatnie"), "{}", note(&app));
        }

        #[test]
        fn ctrl_up_on_a_running_task_says_so_and_sends_nothing() {
            let mut app = app();
            app.load(listing(
                vec![queued("a", 1), controlled("b", "local", "running", 2)],
                "c",
            ));
            ch(&mut app, 'j');
            assert_eq!(ctrl_key(&mut app, KeyCode::Up), Request::None);
            assert_eq!(note(&app), "przesuń: zadanie nie jest w kolejce");
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Warning);
            assert_eq!(ch(&mut app, 'm'), Request::None);
            assert!(
                app.list().moving().is_none(),
                "no move mode for a running task"
            );
        }

        #[test]
        fn m_previews_the_place_and_enter_sends_after_for_a_task_moved_down() {
            let mut app = app();
            app.load(listing(
                vec![
                    queued("a", 1),
                    queued("b", 2),
                    queued("c", 3),
                    queued("d", 4),
                ],
                "c",
            ));
            ch(&mut app, 'm');
            ch(&mut app, 'j');
            ch(&mut app, 'j');
            assert_eq!(
                titles(&app),
                ["b", "c", "a", "d"],
                "the preview moves the row"
            );
            assert_eq!(
                app.list().tasks()[0].task_id.as_str(),
                "a",
                "the data does not"
            );
            assert_eq!(app.list().selected_id(), Some("a"));
            let request = expect_move(press(&mut app, KeyCode::Enter));
            assert_eq!(request.task_id, "a");
            assert_eq!(request.target, MoveTaskTarget::After("c".into()));
            assert!(app.list().moving().is_none());
            assert_eq!(
                titles(&app),
                ["a", "b", "c", "d"],
                "nothing moves before the server says so"
            );
        }

        #[test]
        fn moving_up_goes_before_the_row_it_takes_the_place_of() {
            let mut app = app();
            app.load(listing(
                vec![queued("a", 1), queued("b", 2), queued("c", 3)],
                "c",
            ));
            ch(&mut app, 'G');
            ch(&mut app, 'm');
            ch(&mut app, 'k');
            ch(&mut app, 'k');
            assert_eq!(titles(&app), ["c", "a", "b"]);
            let request = expect_move(press(&mut app, KeyCode::Enter));
            assert_eq!(request.target, MoveTaskTarget::Before("a".into()));
        }

        #[test]
        fn esc_and_an_unchanged_place_send_nothing() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1), queued("b", 2)], "c"));
            ch(&mut app, 'm');
            ch(&mut app, 'j');
            press(&mut app, KeyCode::Esc);
            assert!(app.list().moving().is_none());
            assert_eq!(titles(&app), ["a", "b"]);

            ch(&mut app, 'm');
            ch(&mut app, 'j');
            ch(&mut app, 'k');
            assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
        }

        #[test]
        fn the_preview_stops_at_the_ends_of_the_queue() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1), queued("b", 2)], "c"));
            ch(&mut app, 'm');
            for _ in 0..5 {
                ch(&mut app, 'j');
            }
            assert_eq!(titles(&app), ["b", "a"]);
            for _ in 0..5 {
                ch(&mut app, 'k');
            }
            assert_eq!(titles(&app), ["a", "b"]);
        }

        #[test]
        fn a_move_is_refused_with_a_reason_while_the_connection_is_down() {
            let mut app = app();
            // Never heard from the server: connecting.
            app.on_stream(Some(Err(ClientError::Unavailable("refused".into()))));
            assert_eq!(app.connection().state(), LinkState::Connecting);
            assert!(!app.mutation_allowed());
            assert!(note(&app).contains("łączę"), "{}", note(&app));
        }
    }

    mod connection {
        use super::*;

        fn status(app: &TaskApp) -> String {
            let status = app.connection().status(Instant::now());
            format!("{} {}", status.icon, status.text)
        }

        #[test]
        fn the_stream_dropping_and_coming_back_changes_the_header_not_the_rows() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1), queued("b", 2)], "c"));
            assert_eq!(status(&app), "● live");

            app.on_stream(Some(Err(ClientError::Unavailable("reset".into()))));
            assert!(status(&app).starts_with("◌ ponawiam ("), "{}", status(&app));
            assert!(app.connection().is_stale());
            assert_eq!(titles(&app), ["a", "b"], "the data stays, grey");
            assert!(!app.mutation_allowed());
            assert!(note(&app).contains("ponawiam"), "{}", note(&app));

            app.on_stream(event(
                "e",
                "a",
                2,
                RunnerState::Provisioning,
                TaskEventKind::Created,
            ));
            assert_eq!(status(&app), "● live");
            assert!(app.mutation_allowed());
        }

        #[test]
        fn a_stream_that_ends_for_good_is_no_connection_and_is_not_reopened() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1)], "c"));
            app.on_stream(Some(Err(ClientError::Unauthorized)));
            assert_eq!(status(&app), "✖ brak połączenia");
            assert!(app.stream_ended());
            let mut feed = EventFeed::new(offline_client());
            feed.ensure_open(&app);
            assert!(!feed.is_open());
        }

        #[test]
        fn a_read_that_worked_follows_an_ended_stream_again() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1)], "c"));
            app.on_stream(Some(Err(ClientError::Unauthorized)));
            let mut feed = EventFeed::new(offline_client());
            feed.ensure_open(&app);
            assert!(!feed.is_open());

            app.load(listing(vec![queued("a", 1)], "c2"));
            assert!(!app.stream_ended());
            feed.ensure_open(&app);
            assert!(feed.is_open(), "the feed resumes from the new cursor");

            // And ending again closes what is left of it.
            app.on_stream(None);
            feed.ensure_open(&app);
            assert!(!feed.is_open());
        }
    }
}

mod task_app_detail {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use narsor::api::{ApiClient, ClientError};
    use narsor::tui::style::Styles;
    use narsor::tui::task_app::detail::{
        self, hyperlinks_supported, queue_text, readiness, short_age, unstarted_message,
        DetailRead, Readiness,
    };
    use narsor::tui::task_app::tabs::conversation::{
        AttemptRead, Attempts, Conversation, ReadFailure,
    };
    use narsor::tui::task_app::tabs::events::Availability;
    use narsor::tui::task_app::tabs::info;
    use narsor::tui::task_app::{DetailTab, Focus, TaskApp};
    use narsor::tui::transcript::{TranscriptEntry, TranscriptEntryId};
    use narsor_engine::controller_protocol::{
        ConversationUnavailableReason, ExecutionEventsResponse, ExecutionEventsSource,
    };
    use narsor_engine::domain::{Event, ExecutionStatus};
    use narsor_engine::engine::{ConversationState, PositionStatus};
    use narsor_engine::orchestration::{QueueReason, RunnerState, TaskEventKind};

    use super::task_app_support::{
        event, execution_events, listing, position, queued, task, timeline, titled,
        with_controller, with_runner,
    };

    fn offline_client() -> ApiClient {
        ApiClient::new("127.0.0.1:9".parse().unwrap(), "token")
    }

    fn app() -> TaskApp {
        TaskApp::new(offline_client(), "local", None)
    }

    fn press(app: &mut TaskApp, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn ch(app: &mut TaskApp, character: char) {
        press(app, KeyCode::Char(character));
    }

    fn running(id: &str, rank: u32) -> narsor_engine::controller_protocol::TaskSummary {
        with_runner(
            with_controller(task(id, "local", "running", rank)),
            ExecutionStatus::Running,
            "review",
        )
    }

    /// An app over running `a` and `b`, with the details of `a` open on the
    /// tab they open on: *Rozmowa*.
    fn details_app() -> TaskApp {
        let mut app = app();
        app.load(listing(
            vec![
                titled(running("a", 1), "NAR-1"),
                titled(running("b", 2), "NAR-2"),
            ],
            "c",
        ));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.focus(), Focus::Details);
        assert_eq!(app.detail().tab(), DetailTab::Conversation);
        app
    }

    /// [`details_app`] on *Przebieg*, the timeline.
    fn progress_app() -> TaskApp {
        let mut app = details_app();
        press(&mut app, KeyCode::Char(']'));
        assert_eq!(app.detail().tab(), DetailTab::Progress);
        app
    }

    fn read_timeline(app: &mut TaskApp, id: &str, revision: u64, steps: &[&str], current: &str) {
        let positions = steps
            .iter()
            .map(|state| {
                let status = if format!("{state} #1") == current {
                    PositionStatus::InProgress
                } else {
                    PositionStatus::Succeeded
                };
                position(state, status)
            })
            .collect();
        app.on_detail_read(
            id,
            revision,
            DetailRead::Timeline(Ok(timeline(id, positions, Some(current)))),
        );
    }

    fn selected_step(app: &TaskApp, id: &str) -> Option<String> {
        app.detail()
            .view(id)
            .and_then(|view| view.progress().selected_label().map(str::to_string))
    }

    mod progress {
        use super::*;

        #[test]
        fn follows_a_new_step_after_an_event() {
            let mut app = progress_app();
            read_timeline(&mut app, "a", 1, &["plan", "edit", "review"], "review #1");
            assert_eq!(selected_step(&app, "a").as_deref(), Some("review #1"));
            assert_eq!(
                app.detail().view("a").unwrap().progress().step_count(),
                Some((3, 3))
            );

            // The runner moves on: the event bumps the row's revision, which makes the
            // details due, and the new timeline takes the selection with it.
            app.on_stream(event(
                "e",
                "a",
                2,
                RunnerState::Running,
                TaskEventKind::Created,
            ));
            assert!(app.detail().view("a").unwrap().progress().needs_read(2));
            read_timeline(
                &mut app,
                "a",
                2,
                &["plan", "edit", "review", "test"],
                "test #1",
            );
            assert_eq!(selected_step(&app, "a").as_deref(), Some("test #1"));
            assert_eq!(
                app.detail().view("a").unwrap().progress().step_count(),
                Some((4, 4))
            );
        }

        #[test]
        fn f_stops_following_and_the_cursor_stays_on_its_step() {
            let mut app = progress_app();
            read_timeline(&mut app, "a", 1, &["plan", "edit", "review"], "review #1");
            assert!(app.detail().view("a").unwrap().progress().follows());

            ch(&mut app, 'f');
            assert!(!app.detail().view("a").unwrap().progress().follows());
            read_timeline(
                &mut app,
                "a",
                2,
                &["plan", "edit", "review", "test"],
                "test #1",
            );
            assert_eq!(
                selected_step(&app, "a").as_deref(),
                Some("review #1"),
                "the cursor stays where it is"
            );

            ch(&mut app, 'f');
            assert_eq!(
                selected_step(&app, "a").as_deref(),
                Some("test #1"),
                "following again picks the running step up"
            );
        }

        #[test]
        fn moving_by_hand_ends_the_following_and_a_refresh_changes_neither() {
            let mut app = progress_app();
            read_timeline(&mut app, "a", 1, &["plan", "edit", "review"], "review #1");
            ch(&mut app, 'k');
            assert_eq!(selected_step(&app, "a").as_deref(), Some("edit #1"));
            assert!(!app.detail().view("a").unwrap().progress().follows());

            let tasks = vec![
                titled(running("a", 1), "NAR-1"),
                titled(running("b", 2), "NAR-2"),
            ];
            app.load(listing(tasks, "d"));
            read_timeline(&mut app, "a", 1, &["plan", "edit", "review"], "review #1");
            assert_eq!(selected_step(&app, "a").as_deref(), Some("edit #1"));
            assert!(!app.detail().view("a").unwrap().progress().follows());
            assert_eq!(app.detail().tab(), DetailTab::Progress);
            assert_eq!(app.focus(), Focus::Details);

            ch(&mut app, 'g');
            assert_eq!(selected_step(&app, "a").as_deref(), Some("plan #1"));
            ch(&mut app, 'G');
            assert_eq!(selected_step(&app, "a").as_deref(), Some("review #1"));
        }

        #[test]
        fn coming_back_to_a_task_finds_its_tab_and_cursor() {
            let mut app = progress_app();
            read_timeline(&mut app, "a", 1, &["plan", "edit", "review"], "review #1");
            ch(&mut app, 'k');
            ch(&mut app, 'k');
            assert_eq!(selected_step(&app, "a").as_deref(), Some("plan #1"));
            press(&mut app, KeyCode::Char(']'));
            assert_eq!(app.detail().tab(), DetailTab::Events);
            app.on_detail_read(
                "a",
                1,
                DetailRead::Events {
                    after: 0,
                    result: Ok(ExecutionEventsResponse::Available {
                        events: execution_events(5),
                        has_more: false,
                        source: ExecutionEventsSource::Runner,
                    }),
                },
            );
            ch(&mut app, 'k');
            ch(&mut app, 'k');
            assert_eq!(app.detail().view("a").unwrap().events().cursor(), 2);

            // Away to b and back.
            press(&mut app, KeyCode::Esc);
            ch(&mut app, 'j');
            assert_eq!(app.list().selected_id(), Some("b"));
            press(&mut app, KeyCode::Enter);
            assert_eq!(
                app.detail().tab(),
                DetailTab::Events,
                "the tab is the operator's, not the task's"
            );
            assert!(app
                .detail()
                .view("b")
                .is_none_or(|view| view.events().events().is_empty()));
            press(&mut app, KeyCode::Esc);
            ch(&mut app, 'k');
            press(&mut app, KeyCode::Enter);

            assert_eq!(app.list().selected_id(), Some("a"));
            assert_eq!(app.detail().view("a").unwrap().events().cursor(), 2);
            assert_eq!(selected_step(&app, "a").as_deref(), Some("plan #1"));
            assert!(!app.detail().view("a").unwrap().progress().follows());
        }

        #[test]
        fn a_failed_timeline_read_is_a_message_in_the_tab_and_the_rest_works() {
            let mut app = progress_app();
            app.on_detail_read(
                "a",
                1,
                DetailRead::Timeline(Err(ClientError::Degraded {
                    code: "runner_timeout".into(),
                    message: "the runner did not answer".into(),
                })),
            );
            let view = app.detail().view("a").unwrap();
            assert_eq!(view.progress().failure(), Some("the runner did not answer"));
            assert!(
                app.connection().is_live(),
                "a 503 with a reason is not a lost connection"
            );

            // The other tabs still work.
            press(&mut app, KeyCode::Char(']'));
            press(&mut app, KeyCode::Char(']'));
            press(&mut app, KeyCode::Char(']'));
            assert_eq!(app.detail().tab(), DetailTab::Info);
            let lines = info::lines(app.list().selected().unwrap(), &Styles::new(false), false);
            assert!(!lines.is_empty());

            // The next good read clears the message.
            read_timeline(&mut app, "a", 1, &["plan"], "plan #1");
            assert_eq!(app.detail().view("a").unwrap().progress().failure(), None);
        }
    }

    /// *Przebieg* has two levels of focus: the timeline and the selected
    /// step's conversation (R1–R5).
    mod step_conversation {
        use super::*;
        use narsor::tui::keymap::Context;
        use narsor::tui::task_app::StepFocus;
        use narsor_engine::engine::Position;

        fn recorded(state: &str, status: PositionStatus) -> Position {
            let mut step = position(state, status);
            step.conversation = ConversationState::Recorded;
            step
        }

        /// A timeline of `a` whose every step has a recorded conversation.
        fn read_recorded(app: &mut TaskApp, revision: u64, steps: &[&str], current: &str) {
            let positions = steps
                .iter()
                .map(|state| {
                    let status = if format!("{state} #1") == current {
                        PositionStatus::InProgress
                    } else {
                        PositionStatus::Succeeded
                    };
                    recorded(state, status)
                })
                .collect();
            read_positions(app, revision, positions, current);
        }

        fn read_positions(
            app: &mut TaskApp,
            revision: u64,
            positions: Vec<Position>,
            current: &str,
        ) {
            app.on_detail_read(
                "a",
                revision,
                DetailRead::Timeline(Ok(timeline("a", positions, Some(current)))),
            );
        }

        fn entry(id: &str) -> TranscriptEntry {
            TranscriptEntry::new(
                TranscriptEntryId::from(id),
                vec![format!("line {id}")],
                id.as_bytes().to_vec(),
            )
            .with_header(format!("AGENT · {id}"))
        }

        fn attempt_read(entries: &[&str], next_entry: u64, no_record: bool) -> AttemptRead {
            AttemptRead {
                entries: entries.iter().map(|id| entry(id)).collect(),
                next_entry,
                closed: false,
                truncated_tail: false,
                complete: true,
                no_record,
                source: narsor_engine::controller_protocol::ExecutionEventsSource::Runner,
                recorded_at: None,
            }
        }

        /// The conversation of `state`'s attempt, read `from` an entry.
        fn read_step(app: &mut TaskApp, revision: u64, state: &str, from: u64, read: AttemptRead) {
            app.on_detail_read(
                "a",
                revision,
                DetailRead::Attempt {
                    attempt_id: format!("act-{state}"),
                    from,
                    result: Ok(read),
                },
            );
        }

        /// The details of `a` on *Przebieg*: `plan` done, `implement` running,
        /// both with three entries read.
        fn on_the_axis() -> TaskApp {
            let mut app = progress_app();
            read_recorded(&mut app, 1, &["plan", "implement"], "implement #1");
            for state in ["plan", "implement"] {
                read_step(
                    &mut app,
                    1,
                    state,
                    0,
                    attempt_read(&["1", "2", "3"], 3, false),
                );
            }
            assert_eq!(app.step_focus(), StepFocus::Axis);
            app
        }

        fn layer(app: &TaskApp) -> Option<Context> {
            app.key_context().layer
        }

        fn draw(app: &mut TaskApp, width: u16, height: u16) {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    narsor::tui::task_app::layout::render(
                        frame,
                        app,
                        &Styles::new(false),
                        std::time::Instant::now(),
                    )
                })
                .unwrap();
        }

        fn selected_entry(app: &TaskApp, state: &str) -> Option<String> {
            let view = app.detail().view("a")?;
            assert_eq!(
                view.progress().selected_label(),
                Some(format!("{state} #1").as_str())
            );
            view.step_conversation()?
                .transcript()
                .selected_entry()
                .map(|id| id.as_str().rsplit('/').next().unwrap().to_string())
        }

        fn note(app: &TaskApp) -> Option<String> {
            app.notifier().latest().map(|note| note.text.clone())
        }

        #[test]
        fn enter_reads_the_steps_conversation_and_esc_goes_back_to_the_same_step() {
            let mut app = on_the_axis();
            assert_eq!(layer(&app), Some(Context::Axis));

            press(&mut app, KeyCode::Enter);
            assert_eq!(app.step_focus(), StepFocus::Conversation);
            assert_eq!(layer(&app), Some(Context::StepConversation));
            assert_eq!(
                selected_entry(&app, "implement").as_deref(),
                Some("3"),
                "the conversation opens where it was: on the last entry"
            );

            ch(&mut app, 'g');
            ch(&mut app, 'j');
            assert_eq!(selected_entry(&app, "implement").as_deref(), Some("2"));
            assert_eq!(
                selected_step(&app, "a").as_deref(),
                Some("implement #1"),
                "j picks an entry, not a step"
            );

            press(&mut app, KeyCode::Esc);
            assert_eq!(app.step_focus(), StepFocus::Axis);
            assert_eq!(app.focus(), Focus::Details);
            assert_eq!(selected_step(&app, "a").as_deref(), Some("implement #1"));
            assert_eq!(
                selected_entry(&app, "implement").as_deref(),
                Some("2"),
                "the place in the conversation is kept for the way back"
            );

            press(&mut app, KeyCode::Esc);
            assert_eq!(app.focus(), Focus::List);
        }

        #[test]
        fn the_arrows_enter_and_leave_like_enter_and_esc() {
            let mut app = on_the_axis();
            press(&mut app, KeyCode::Right);
            assert_eq!(app.step_focus(), StepFocus::Conversation);
            press(&mut app, KeyCode::Up);
            assert_eq!(selected_entry(&app, "implement").as_deref(), Some("2"));
            press(&mut app, KeyCode::Left);
            assert_eq!(app.step_focus(), StepFocus::Axis);
            assert_eq!(app.focus(), Focus::Details);
            assert_eq!(selected_step(&app, "a").as_deref(), Some("implement #1"));
        }

        #[test]
        fn a_step_without_a_record_keeps_the_focus_on_the_axis_and_says_nothing() {
            let mut app = progress_app();
            read_timeline(&mut app, "a", 1, &["analyze"], "analyze #1");
            let before = note(&app);
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.step_focus(), StepFocus::Axis);
            assert_eq!(layer(&app), Some(Context::Axis));
            assert_eq!(app.focus(), Focus::Details);
            assert_eq!(note(&app), before, "no notification");
        }

        #[test]
        fn a_step_with_nothing_to_show_cannot_be_entered() {
            // Not read yet.
            let mut app = progress_app();
            read_recorded(&mut app, 1, &["plan"], "plan #1");
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.step_focus(), StepFocus::Axis, "not read yet");

            // Recorded, but without the attempt's id.
            let mut app = progress_app();
            let mut step = recorded("plan", PositionStatus::Succeeded);
            step.action_execution_id = None;
            read_positions(&mut app, 1, vec![step], "plan #1");
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.step_focus(), StepFocus::Axis, "no attempt");

            // Never created.
            let mut app = progress_app();
            let mut step = position("plan", PositionStatus::Failed);
            step.conversation = ConversationState::NotCreated {
                reason: "the harness did not start".into(),
            };
            read_positions(&mut app, 1, vec![step], "plan #1");
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.step_focus(), StepFocus::Axis, "never created");

            // The runner holds no record of it.
            let mut app = progress_app();
            read_recorded(&mut app, 1, &["plan"], "plan #1");
            read_step(&mut app, 1, "plan", 0, attempt_read(&[], 0, true));
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.step_focus(), StepFocus::Axis, "no record");

            // Running, nothing said yet.
            let mut app = progress_app();
            read_recorded(&mut app, 1, &["plan"], "plan #1");
            read_step(&mut app, 1, "plan", 0, attempt_read(&[], 0, false));
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.step_focus(), StepFocus::Axis, "no entry yet");

            // The first entry opens the way.
            read_step(&mut app, 2, "plan", 0, attempt_read(&["1"], 1, false));
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.step_focus(), StepFocus::Conversation);
        }

        #[test]
        fn a_new_step_does_not_move_the_axis_while_its_conversation_is_read() {
            let mut app = progress_app();
            read_recorded(&mut app, 1, &["plan"], "plan #1");
            read_step(&mut app, 1, "plan", 0, attempt_read(&["1"], 1, false));
            assert!(app.detail().view("a").unwrap().progress().follows());
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.step_focus(), StepFocus::Conversation);

            read_recorded(&mut app, 2, &["plan", "implement"], "implement #1");
            assert_eq!(selected_step(&app, "a").as_deref(), Some("plan #1"));
            assert_eq!(app.step_focus(), StepFocus::Conversation);
            assert!(app.detail().view("a").unwrap().progress().follows());

            press(&mut app, KeyCode::Esc);
            assert_eq!(
                selected_step(&app, "a").as_deref(),
                Some("implement #1"),
                "back on the axis, the following catches up"
            );
        }

        #[test]
        fn without_following_the_way_back_keeps_the_step() {
            let mut app = on_the_axis();
            ch(&mut app, 'k');
            assert!(!app.detail().view("a").unwrap().progress().follows());
            press(&mut app, KeyCode::Enter);
            read_recorded(&mut app, 2, &["plan", "implement", "review"], "review #1");
            press(&mut app, KeyCode::Esc);
            assert_eq!(selected_step(&app, "a").as_deref(), Some("plan #1"));
        }

        #[test]
        fn f_does_nothing_in_the_conversation() {
            let mut app = on_the_axis();
            press(&mut app, KeyCode::Enter);
            ch(&mut app, 'g');
            ch(&mut app, 'f');
            assert!(app.detail().view("a").unwrap().progress().follows());
            assert_eq!(
                selected_entry(&app, "implement").as_deref(),
                Some("1"),
                "f is not the end of the conversation either"
            );
            assert_eq!(app.step_focus(), StepFocus::Conversation);
        }

        #[test]
        fn a_step_that_leaves_the_run_drops_the_focus_to_the_axis_and_says_so() {
            let mut app = on_the_axis();
            ch(&mut app, 'k');
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.step_focus(), StepFocus::Conversation);
            read_recorded(&mut app, 2, &["implement"], "implement #1");
            assert_eq!(app.step_focus(), StepFocus::Axis);
            assert_eq!(layer(&app), Some(Context::Axis));
            assert_eq!(selected_step(&app, "a").as_deref(), Some("implement #1"));
            let said = note(&app).unwrap_or_default();
            assert!(said.contains("plan #1"), "{said}");
            assert_eq!(
                app.notifier().latest().unwrap().severity,
                narsor::tui::task_app::notify::Severity::Warning
            );
        }

        #[test]
        fn a_conversation_that_stops_being_shown_drops_the_focus_quietly() {
            let mut app = on_the_axis();
            press(&mut app, KeyCode::Enter);
            let before = note(&app);
            read_step(&mut app, 2, "implement", 0, attempt_read(&[], 0, true));
            assert_eq!(app.step_focus(), StepFocus::Axis);
            assert_eq!(note(&app), before, "no notification");
        }

        #[test]
        fn a_failed_read_keeps_the_conversation_and_what_was_read() {
            let mut app = on_the_axis();
            press(&mut app, KeyCode::Enter);
            app.on_detail_read(
                "a",
                2,
                DetailRead::Attempt {
                    attempt_id: "act-implement".to_string(),
                    from: 3,
                    result: Err(ClientError::Degraded {
                        code: "runner_timeout".into(),
                        message: "the runner did not answer".into(),
                    }),
                },
            );
            assert_eq!(app.step_focus(), StepFocus::Conversation);
        }

        #[test]
        fn switching_the_tab_leaves_the_conversation_for_the_axis() {
            let mut app = on_the_axis();
            press(&mut app, KeyCode::Enter);
            press(&mut app, KeyCode::Char('['));
            assert_eq!(app.detail().tab(), DetailTab::Conversation);
            assert_eq!(layer(&app), Some(Context::AgentConversation));
            press(&mut app, KeyCode::Char(']'));
            assert_eq!(app.detail().tab(), DetailTab::Progress);
            assert_eq!(app.step_focus(), StepFocus::Axis);
            assert_eq!(layer(&app), Some(Context::Axis));
        }

        #[test]
        fn another_selected_task_starts_on_the_axis() {
            let mut app = on_the_axis();
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.step_focus(), StepFocus::Conversation);
            // `a` was archived elsewhere: the selection moves to `b`.
            app.load(listing(vec![titled(running("b", 2), "NAR-2")], "d"));
            assert_eq!(app.list().selected_id(), Some("b"));
            assert_eq!(app.focus(), Focus::Details);
            assert_eq!(app.step_focus(), StepFocus::Axis);
            assert_eq!(layer(&app), Some(Context::Axis));
        }

        #[test]
        fn opening_the_details_again_starts_on_the_axis() {
            let mut app = on_the_axis();
            press(&mut app, KeyCode::Enter);
            // Help and back: the level stays.
            ch(&mut app, '?');
            assert!(app.is_help_open());
            ch(&mut app, 'x');
            assert_eq!(app.step_focus(), StepFocus::Conversation);
            // Away to b and back to a.
            press(&mut app, KeyCode::Esc);
            press(&mut app, KeyCode::Esc);
            assert_eq!(app.focus(), Focus::List);
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.step_focus(), StepFocus::Axis);
            assert_eq!(
                selected_step(&app, "a").as_deref(),
                Some("implement #1"),
                "and the timeline follows again"
            );
        }

        fn read_events(app: &mut TaskApp, events: Vec<narsor_engine::domain::Event>) {
            use narsor_engine::controller_protocol::{
                ExecutionEventsResponse, ExecutionEventsSource,
            };
            app.on_detail_read(
                "a",
                1,
                DetailRead::Events {
                    after: 0,
                    result: Ok(ExecutionEventsResponse::Available {
                        events,
                        has_more: false,
                        source: ExecutionEventsSource::Runner,
                    }),
                },
            );
        }

        fn choice_ids(app: &TaskApp) -> Vec<String> {
            app.detail()
                .view("a")
                .unwrap()
                .progress()
                .step_view()
                .map(|view| {
                    view.transcript()
                        .entries()
                        .iter()
                        .filter(|entry| entry.workflow_choice)
                        .map(|entry| entry.id.as_str().to_string())
                        .collect()
                })
                .unwrap_or_default()
        }

        /// R1, R13: *Przebieg* shows the choice made in the step, and the
        /// step still running has none until an event tells it.
        #[test]
        fn the_step_shows_its_choice_and_a_running_step_has_none() {
            use crate::task_app_support::{agent_chose, entered};
            let mut app = on_the_axis();
            read_events(
                &mut app,
                vec![
                    entered(1, "plan"),
                    agent_chose(2, "ready", None),
                    entered(3, "implement"),
                ],
            );
            assert!(choice_ids(&app).is_empty(), "implement is still running");
            ch(&mut app, 'k');
            draw(&mut app, 120, 30);
            assert_eq!(choice_ids(&app), ["workflow-choice/2"]);
            ch(&mut app, 'j');
            draw(&mut app, 120, 30);
            assert!(choice_ids(&app).is_empty());
        }

        /// R11, AE4: the cursor of a step survives moving the selection
        /// away and back, and a resize of the terminal.
        #[test]
        fn a_steps_cursor_survives_the_selection_and_a_resize() {
            let mut app = on_the_axis();
            press(&mut app, KeyCode::Enter);
            ch(&mut app, 'k');
            assert_eq!(selected_entry(&app, "implement").as_deref(), Some("2"));
            press(&mut app, KeyCode::Esc);
            ch(&mut app, 'k');
            assert_eq!(selected_step(&app, "a").as_deref(), Some("plan #1"));
            ch(&mut app, 'j');
            assert_eq!(selected_entry(&app, "implement").as_deref(), Some("2"));
            for (width, height) in [(120, 30), (60, 16), (120, 30)] {
                draw(&mut app, width, height);
                assert_eq!(selected_entry(&app, "implement").as_deref(), Some("2"));
            }
        }

        #[test]
        fn new_entries_keep_the_picked_entry_and_the_conversation() {
            let mut app = on_the_axis();
            press(&mut app, KeyCode::Enter);
            ch(&mut app, 'k');
            assert_eq!(selected_entry(&app, "implement").as_deref(), Some("2"));
            read_step(&mut app, 2, "implement", 3, attempt_read(&["4"], 4, false));
            assert_eq!(selected_entry(&app, "implement").as_deref(), Some("2"));
            assert_eq!(app.step_focus(), StepFocus::Conversation);
        }
    }

    mod not_started {
        use super::*;

        fn flatten(lines: &[ratatui::text::Line<'_>]) -> String {
            lines
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }

        #[test]
        fn a_queued_task_says_it_has_not_started_and_info_shows_what_it_was_sealed_with() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1), queued("b", 2)], "c"));
            app.list_mut().select("b");
            let task = app.list().selected().unwrap().clone();
            let place = app.list().queue_position("b");
            assert_eq!(place, Some(2));
            assert_eq!(readiness(&task), Readiness::NotStarted);
            assert_eq!(
                unstarted_message(&task, place).as_deref(),
                Some("jeszcze nie wystartowało, #2 w kolejce")
            );

            let text = flatten(&info::lines(&task, &Styles::new(false), false));
            for expected in [
                "Zapieczętowane wejścia",
                "launch digest",
                "digest",
                "środowisko",
                "implement-feature v3",
            ] {
                assert!(text.contains(expected), "{expected:?} in\n{text}");
            }
        }

        #[tokio::test]
        async fn nothing_is_read_for_a_task_without_an_execution() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1)], "c"));
            app.sync_detail().await;
            assert!(
                app.connection().is_live(),
                "no request was made, so none failed"
            );
            assert!(app
                .detail()
                .view("a")
                .is_none_or(|view| view.progress().timeline().is_none()));
        }

        #[test]
        fn an_archived_task_says_its_storage_went_with_the_archiving() {
            let archived = task("a", "local", "archived", 1);
            assert_eq!(readiness(&archived), Readiness::Archived);
            assert_eq!(
                unstarted_message(&archived, None).as_deref(),
                Some("storage usunięty przy archiwizacji")
            );
            let provisioning = task("b", "local", "provisioning", 2);
            assert_eq!(
                unstarted_message(&provisioning, None).as_deref(),
                Some("jeszcze nie wystartowało, w kolejce")
            );
            assert_eq!(unstarted_message(&running("c", 3), None), None);
        }

        #[test]
        fn the_queue_line_says_why_a_task_waits() {
            let waiting =
                super::super::task_app_support::waiting_for_capacity(queued("a", 1), 2, 2, 3);
            assert_eq!(queue_text(&waiting, Some(2)), "#2 · czeka na slot (2/3)");
            let mut offline = queued("b", 2);
            offline.queue = Some(narsor_engine::controller_protocol::QueueView {
                reason: QueueReason::ControllerOffline,
                message: String::new(),
                since: offline.as_of - chrono::Duration::minutes(4),
            });
            assert_eq!(queue_text(&offline, Some(1)), "kontroler offline 4m");
            assert_eq!(queue_text(&queued("c", 3), Some(3)), "#3 · w kolejce");
        }
    }

    /// *Rozmowa* puts a workflow choice after the conversation of the step
    /// entry it followed, before the heading of the next step.
    mod choice_order {
        use super::*;

        use narsor_engine::domain::EventData;
        use narsor_protocol::api::Position;

        use crate::task_app_support::{
            agent_chose, conductor_chose, decided, entered, ev, human_chose, operator_answered,
        };

        fn read(ids: &[&str]) -> AttemptRead {
            AttemptRead {
                entries: ids
                    .iter()
                    .map(|id| {
                        TranscriptEntry::new(
                            TranscriptEntryId::from(*id),
                            vec![format!("line {id}")],
                            id.as_bytes().to_vec(),
                        )
                        .with_header(format!("AGENT · {id}"))
                    })
                    .collect(),
                next_entry: 1,
                closed: true,
                truncated_tail: false,
                complete: true,
                no_record: false,
                source: narsor_engine::controller_protocol::ExecutionEventsSource::Runner,
                recorded_at: None,
            }
        }

        /// A recorded attempt `attempt` of `state`'s `visit`.
        fn recorded(state: &str, visit: u32, attempt: &str) -> Position {
            let mut step = position(state, PositionStatus::Succeeded);
            step.visit = visit;
            step.action_execution_id = Some(attempt.to_string());
            step.conversation = ConversationState::Recorded;
            step
        }

        fn attempts(ids: &[&str]) -> Attempts {
            let mut attempts = Attempts::default();
            for id in ids {
                attempts.apply(id, 1, 0, Ok(read(&["m"])));
            }
            attempts
        }

        /// The transcript as a list of tags: a heading is `step:<attempt>`, a
        /// choice `choice:<sequence>`, an entry its own id.
        fn order(positions: &[Position], attempts: &Attempts, events: &[Event]) -> Vec<String> {
            use narsor::tui::task_app::tabs::conversation::{build_conversation, Scope};
            tags(&build_conversation(Scope::Whole, positions, attempts, events).entries)
        }

        fn tags(entries: &[TranscriptEntry]) -> Vec<String> {
            entries
                .iter()
                .map(|entry| {
                    let id = entry.id.as_str();
                    if let Some(attempt) = id.strip_suffix("/step") {
                        format!("step:{attempt}")
                    } else if let Some(sequence) = id.strip_prefix("workflow-choice/") {
                        format!("choice:{sequence}")
                    } else {
                        id.to_string()
                    }
                })
                .collect()
        }

        fn step_order(
            state: &str,
            visit: u32,
            positions: &[Position],
            attempts: &Attempts,
            events: &[Event],
        ) -> Vec<String> {
            use narsor::tui::task_app::tabs::conversation::{build_conversation, Scope};
            let scope = Scope::Step {
                state_id: state,
                visit,
            };
            build_conversation(scope, positions, attempts, events)
                .entries
                .iter()
                .map(|entry| {
                    let id = entry.id.as_str();
                    match id.strip_prefix("workflow-choice/") {
                        Some(sequence) => format!("choice:{sequence}"),
                        None => id.to_string(),
                    }
                })
                .collect()
        }

        /// AE1: the choice stands after the conversation of `code_review` and
        /// before the heading of the next step in the whole conversation, and
        /// at the end of the step's own.
        #[test]
        fn ae1_a_step_scope_ends_with_the_choice_and_has_no_heading() {
            let positions = vec![recorded("code_review", 1, "c"), recorded("fix", 1, "f")];
            let events = vec![
                entered(1, "code_review"),
                agent_chose(2, "needs_fix", None),
                entered(3, "fix"),
            ];
            let attempts = attempts(&["c", "f"]);
            assert_eq!(
                order(&positions, &attempts, &events),
                ["step:c", "c/m", "choice:2", "step:f", "f/m"]
            );
            assert_eq!(
                step_order("code_review", 1, &positions, &attempts, &events),
                ["c/m", "choice:2"]
            );
            assert_eq!(
                step_order("fix", 1, &positions, &attempts, &events),
                ["f/m"]
            );
        }

        /// AE2: in a loop each entry into the step has only its own choices.
        #[test]
        fn ae2_a_loop_gives_each_entry_its_own_choices() {
            let positions = vec![
                recorded("code_review", 1, "c1"),
                recorded("fix", 1, "f"),
                recorded("code_review", 2, "c2"),
            ];
            let events = vec![
                entered(1, "code_review"),
                agent_chose(2, "needs_fix", None),
                entered(3, "fix"),
                agent_chose(4, "fixed", None),
                entered(5, "code_review"),
                agent_chose(6, "approved", None),
            ];
            let attempts = attempts(&["c1", "f", "c2"]);
            assert_eq!(
                step_order("code_review", 1, &positions, &attempts, &events),
                ["c1/m", "choice:2"]
            );
            assert_eq!(
                step_order("code_review", 2, &positions, &attempts, &events),
                ["c2/m", "choice:6"]
            );
            assert_eq!(
                step_order("fix", 1, &positions, &attempts, &events),
                ["f/m", "choice:4"]
            );
        }

        #[test]
        fn an_entry_the_events_do_not_tell_has_no_choices_in_its_scope() {
            let positions = vec![recorded("plan", 1, "p")];
            let events = vec![agent_chose(1, "a", None)];
            assert_eq!(
                step_order("plan", 1, &positions, &attempts(&["p"]), &events),
                ["p/m"]
            );
        }

        #[test]
        fn every_attempt_of_an_entry_is_in_its_scope_and_a_shared_one_once() {
            let positions = vec![
                recorded("plan", 1, "p1"),
                recorded("plan", 1, "p2"),
                recorded("plan", 1, "p2"),
                recorded("fix", 1, "f"),
            ];
            let events = vec![entered(1, "plan"), entered(2, "fix")];
            assert_eq!(
                step_order(
                    "plan",
                    1,
                    &positions,
                    &attempts(&["p1", "p2", "f"]),
                    &events
                ),
                ["p1/m", "p2/m"]
            );
        }

        #[test]
        fn a_choice_follows_the_conversation_of_its_step_and_precedes_the_next_heading() {
            let positions = vec![
                recorded("plan", 1, "p"),
                recorded("code_review", 1, "r"),
                recorded("fix", 1, "f"),
            ];
            let events = vec![
                entered(1, "plan"),
                entered(2, "code_review"),
                agent_chose(3, "revise", None),
                conductor_chose(4, "fix"),
                entered(5, "fix"),
            ];
            assert_eq!(
                order(&positions, &attempts(&["p", "r", "f"]), &events),
                ["step:p", "p/m", "step:r", "r/m", "choice:3", "choice:4", "step:f", "f/m"]
            );
        }

        #[test]
        fn a_choice_of_a_step_without_a_conversation_stands_between_its_neighbours() {
            let events = vec![
                entered(1, "plan"),
                entered(2, "gate"),
                human_chose(3, "gate", "implement"),
                entered(4, "implement"),
            ];
            let expected = [
                "step:p",
                "p/m",
                "step:step-visit/gate/1",
                "choice:3",
                "step:i",
                "i/m",
            ];
            let attempts = attempts(&["p", "i"]);
            // The gate has no position at all.
            let positions = vec![recorded("plan", 1, "p"), recorded("implement", 1, "i")];
            assert_eq!(order(&positions, &attempts, &events), expected);
            // The gate has a position without a conversation.
            let positions = vec![
                recorded("plan", 1, "p"),
                position("gate", PositionStatus::Succeeded),
                recorded("implement", 1, "i"),
            ];
            assert_eq!(order(&positions, &attempts, &events), expected);
        }

        fn choice_lines(events: &[Event]) -> Vec<String> {
            use narsor::tui::task_app::tabs::conversation::{build_conversation, Scope};
            build_conversation(Scope::Whole, &[], &Attempts::default(), events)
                .entries
                .iter()
                .filter(|entry| entry.id.as_str().starts_with("workflow-choice/"))
                .map(|entry| entry.rendered_lines.join(" "))
                .collect()
        }

        #[test]
        fn an_answer_of_the_conductor_to_the_agent_shows_its_rationale() {
            use narsor::tui::task_app::tabs::conversation::{build_conversation, Scope};
            let events = vec![
                entered(1, "work"),
                ev(
                    2,
                    EventData::ConductorDecision {
                        decision_id: "d2".to_string(),
                        kind: narsor_engine::domain::ConductorDecisionKind::AnswerQuestion,
                        state_id: Some("work".to_string()),
                        output_label: None,
                        episode: None,
                        confidence: Default::default(),
                        rationale: Some("the task names the version".to_string()),
                    },
                ),
            ];
            let conversation = build_conversation(Scope::Whole, &[], &Attempts::default(), &events);
            let answers: Vec<String> = conversation
                .entries
                .iter()
                .filter(|entry| entry.id.as_str().starts_with("conductor-answer/"))
                .map(|entry| entry.rendered_lines.join(" "))
                .collect();
            assert_eq!(answers.len(), 1);
            assert!(
                answers[0].contains("the task names the version"),
                "{answers:?}"
            );
        }

        #[test]
        fn a_users_answer_and_the_decision_it_produced_are_one_user_choice() {
            let events = vec![
                entered(1, "gate"),
                operator_answered(2, Some("check_feedback")),
                decided(3, "rules", "check_feedback"),
            ];
            assert_eq!(
                choice_lines(&events),
                ["użytkownik wybrał opcję: check_feedback"]
            );
        }

        #[test]
        fn a_manual_transition_is_one_user_choice() {
            for provider in ["human", "human_override"] {
                let events = vec![
                    entered(1, "gate"),
                    decided(2, provider, "implement"),
                    human_chose(3, "gate", "implement"),
                ];
                assert_eq!(
                    choice_lines(&events),
                    ["użytkownik wybrał opcję: implement"]
                );
            }
        }

        #[test]
        fn a_decision_of_another_provider_stays_the_agents() {
            let events = vec![entered(1, "gate"), decided(2, "rules", "published")];
            assert_eq!(
                choice_lines(&events),
                ["agent · rules wybrał opcję: published"]
            );
        }

        #[test]
        fn the_same_label_chosen_by_the_user_in_two_entries_gives_two_user_choices() {
            let events = vec![
                entered(1, "gate"),
                operator_answered(2, Some("again")),
                decided(3, "rules", "again"),
                entered(4, "gate"),
                operator_answered(5, Some("again")),
                decided(6, "rules", "again"),
            ];
            assert_eq!(
                choice_lines(&events),
                [
                    "użytkownik wybrał opcję: again",
                    "użytkownik wybrał opcję: again"
                ]
            );
        }

        #[test]
        fn an_answer_without_a_label_shows_nothing_and_its_human_decision_is_the_users() {
            let events = vec![entered(1, "gate"), operator_answered(2, None)];
            assert!(choice_lines(&events).is_empty());
            let events = vec![
                entered(1, "gate"),
                operator_answered(2, None),
                decided(3, "human", "approved"),
            ];
            assert_eq!(choice_lines(&events), ["użytkownik wybrał opcję: approved"]);
        }

        #[test]
        fn a_decision_with_another_label_than_the_answer_is_shown_too() {
            let events = vec![
                entered(1, "gate"),
                operator_answered(2, Some("a")),
                decided(3, "rules", "b"),
            ];
            assert_eq!(
                choice_lines(&events),
                [
                    "użytkownik wybrał opcję: a",
                    "agent · rules wybrał opcję: b"
                ]
            );
        }

        #[test]
        fn an_answer_does_not_hide_the_decision_of_a_later_entry() {
            let events = vec![
                entered(1, "gate"),
                operator_answered(2, Some("a")),
                entered(3, "review"),
                decided(4, "rules", "a"),
            ];
            assert_eq!(
                choice_lines(&events),
                [
                    "użytkownik wybrał opcję: a",
                    "agent · rules wybrał opcję: a"
                ]
            );
        }

        #[test]
        fn a_step_scope_shows_the_user_choice_once_and_no_headings() {
            let positions = vec![recorded("plan", 1, "p")];
            let events = vec![
                entered(1, "plan"),
                operator_answered(2, Some("go")),
                decided(3, "rules", "go"),
            ];
            assert_eq!(
                step_order("plan", 1, &positions, &attempts(&["p"]), &events),
                ["p/m", "choice:2"]
            );
        }

        #[test]
        fn a_gate_between_two_steps_gets_a_heading_before_its_choice() {
            let positions = vec![
                recorded("publish", 1, "p"),
                position("gate", PositionStatus::Succeeded),
                recorded("check", 1, "c"),
            ];
            let events = vec![
                entered(1, "publish"),
                decided(2, "rules", "published"),
                entered(3, "gate"),
                operator_answered(4, Some("check")),
                decided(5, "rules", "check"),
                entered(6, "check"),
            ];
            assert_eq!(
                order(&positions, &attempts(&["p", "c"]), &events),
                [
                    "step:p",
                    "p/m",
                    "choice:2",
                    "step:step-visit/gate/1",
                    "choice:4",
                    "step:c",
                    "c/m"
                ]
            );
            // Rebuilding on the same data gives the same entries.
            let mut conversation = Conversation::new("a");
            conversation.rebuild_with_events(&positions, &attempts(&["p", "c"]), &events);
            let first = tags(conversation.transcript().entries());
            conversation.rebuild_with_events(&positions, &attempts(&["p", "c"]), &events);
            assert_eq!(tags(conversation.transcript().entries()), first);
        }

        #[test]
        fn a_step_with_a_conversation_has_one_heading_and_a_loading_one_none() {
            let positions = vec![recorded("plan", 1, "p")];
            let events = vec![entered(1, "plan")];
            assert_eq!(
                order(&positions, &attempts(&["p"]), &events),
                ["step:p", "p/m"]
            );
            assert!(order(&positions, &Attempts::default(), &events).is_empty());
        }

        #[test]
        fn a_shared_chat_gets_one_heading_and_a_loop_visit_its_number() {
            let positions = vec![
                recorded("plan", 1, "p"),
                recorded("plan", 2, "p"),
                position("gate", PositionStatus::Succeeded),
            ];
            let events = vec![
                entered(1, "plan"),
                entered(2, "plan"),
                entered(3, "gate"),
                entered(4, "gate"),
            ];
            assert_eq!(
                order(&positions, &attempts(&["p"]), &events),
                [
                    "step:p",
                    "p/m",
                    "step:step-visit/gate/1",
                    "step:step-visit/gate/2"
                ]
            );
        }

        #[test]
        fn a_step_heading_starts_with_the_icon_of_its_kind() {
            use narsor::tui::task_app::tabs::conversation::{build_conversation, Scope};
            let positions = vec![crate::task_app_support::of_kind(
                recorded("implement", 1, "i"),
                "agent",
                None,
            )];
            let built = build_conversation(
                Scope::Whole,
                &positions,
                &attempts(&["i"]),
                &[entered(1, "implement")],
            );
            assert_eq!(built.entries[0].header, "── ✨ implement #1 ──");
        }

        #[test]
        fn a_step_heading_without_an_attempt_entry_keeps_the_icon_of_its_kind() {
            use narsor::tui::task_app::tabs::conversation::{build_conversation, Scope};
            let positions = vec![crate::task_app_support::of_kind(
                position("test", PositionStatus::Succeeded),
                "script",
                None,
            )];
            let built = build_conversation(
                Scope::Whole,
                &positions,
                &Attempts::default(),
                &[entered(1, "test")],
            );
            assert_eq!(built.entries[0].header, "── 📜 test #1 ──");
        }

        #[test]
        fn a_gate_heading_reads_the_step_and_its_visit() {
            use narsor::tui::task_app::tabs::conversation::{build_conversation, Scope};
            let events = vec![entered(1, "gate")];
            let built = build_conversation(Scope::Whole, &[], &Attempts::default(), &events);
            assert_eq!(built.entries[0].header, "── gate #1 ──");
        }

        #[test]
        fn each_visit_of_a_loop_keeps_its_own_choice() {
            let positions = vec![
                recorded("code_review", 1, "r1"),
                recorded("fix", 1, "f"),
                recorded("code_review", 2, "r2"),
            ];
            let events = vec![
                entered(1, "code_review"),
                agent_chose(2, "revise", None),
                entered(3, "fix"),
                agent_chose(4, "done", None),
                entered(5, "code_review"),
                agent_chose(6, "approved", None),
            ];
            assert_eq!(
                order(&positions, &attempts(&["r1", "f", "r2"]), &events),
                [
                    "step:r1", "r1/m", "choice:2", "step:f", "f/m", "choice:4", "step:r2", "r2/m",
                    "choice:6"
                ]
            );
        }

        #[test]
        fn the_attempts_of_one_entry_stay_together_and_the_choice_follows_the_last() {
            let positions = vec![
                recorded("implement", 1, "i1"),
                recorded("implement", 1, "i2"),
                recorded("implement", 1, "i3"),
                recorded("code_review", 1, "r"),
            ];
            let events = vec![
                entered(1, "implement"),
                agent_chose(5, "done", None),
                entered(6, "code_review"),
            ];
            assert_eq!(
                order(&positions, &attempts(&["i1", "i2", "i3", "r"]), &events),
                [
                    "step:i1", "i1/m", "step:i2", "i2/m", "step:i3", "i3/m", "choice:5", "step:r",
                    "r/m"
                ]
            );
        }

        #[test]
        fn the_last_choice_ends_the_transcript_and_a_reader_at_the_end_stays_there() {
            let positions = vec![recorded("plan", 1, "p")];
            let attempts = attempts(&["p"]);
            let mut events = vec![entered(1, "plan"), agent_chose(2, "done", None)];
            let mut conversation = Conversation::new("a");
            conversation.transcript_mut().set_viewport(2);
            conversation.rebuild_with_events(&positions, &attempts, &events);
            assert_eq!(
                tags(conversation.transcript().entries()).last().unwrap(),
                "choice:2"
            );
            assert!(conversation.transcript().is_following_end());

            events.push(conductor_chose(3, "again"));
            conversation.rebuild_with_events(&positions, &attempts, &events);
            assert_eq!(
                tags(conversation.transcript().entries()).last().unwrap(),
                "choice:3"
            );
            assert!(conversation.transcript().is_following_end());
        }

        #[test]
        fn a_choice_before_every_entry_stands_before_the_first_heading() {
            let positions = vec![recorded("plan", 1, "p")];
            let events = vec![
                agent_chose(1, "early", None),
                entered(2, "plan"),
                agent_chose(3, "done", None),
            ];
            assert_eq!(
                order(&positions, &attempts(&["p"]), &events),
                ["choice:1", "step:p", "p/m", "choice:3"]
            );
        }

        #[test]
        fn choices_of_one_entry_keep_the_order_of_their_sequence() {
            let positions = vec![recorded("plan", 1, "p")];
            let events = vec![
                conductor_chose(4, "b"),
                agent_chose(3, "a", None),
                entered(1, "plan"),
            ];
            assert_eq!(
                order(&positions, &attempts(&["p"]), &events),
                ["step:p", "p/m", "choice:3", "choice:4"]
            );
        }

        #[test]
        fn without_step_entries_the_choices_stay_at_the_end() {
            let positions = vec![recorded("plan", 1, "p"), recorded("fix", 1, "f")];
            let events = vec![agent_chose(1, "a", None), conductor_chose(2, "b")];
            assert_eq!(
                order(&positions, &attempts(&["p", "f"]), &events),
                ["step:p", "p/m", "step:f", "f/m", "choice:1", "choice:2"]
            );
            assert_eq!(
                order(&positions, &attempts(&["p", "f"]), &[]),
                ["step:p", "p/m", "step:f", "f/m"]
            );
        }

        #[test]
        fn a_choice_keeps_its_text_header_and_id() {
            let positions = vec![recorded("plan", 1, "p")];
            let events = vec![entered(1, "plan"), agent_chose(2, "done", None)];
            let mut conversation = Conversation::new("a");
            conversation.rebuild_with_events(&positions, &attempts(&["p"]), &events);
            let choice = conversation.transcript().entries().last().unwrap();
            assert_eq!(choice.id.as_str(), "workflow-choice/2");
            assert!(choice.header.starts_with("WYBÓR WORKFLOW · "), "{choice:?}");
            assert_eq!(
                choice.rendered_lines,
                ["agent · claude wybrał opcję: done".to_string()]
            );
        }

        #[test]
        fn only_a_choice_is_marked_as_a_workflow_choice() {
            let positions = vec![recorded("plan", 1, "p")];
            let events = vec![
                entered(1, "plan"),
                agent_chose(2, "done", None),
                conductor_chose(3, "next"),
                human_chose(4, "plan", "fix"),
            ];
            let mut conversation = Conversation::new("a");
            conversation.rebuild_with_events(&positions, &attempts(&["p"]), &events);
            for entry in conversation.transcript().entries() {
                assert_eq!(
                    entry.workflow_choice,
                    entry.id.as_str().starts_with("workflow-choice/"),
                    "{entry:?}"
                );
            }
            assert_eq!(
                conversation
                    .transcript()
                    .entries()
                    .iter()
                    .filter(|entry| entry.workflow_choice)
                    .count(),
                3
            );
        }

        #[test]
        fn an_agents_choice_shows_its_reason_without_control_characters() {
            let positions = vec![recorded("plan", 1, "p")];
            let events = vec![
                entered(1, "plan"),
                ev(
                    2,
                    EventData::DecisionMade {
                        provider: "claude".to_string(),
                        output_label: "plan_ready".to_string(),
                        reasoning: Some("Plan gotowy\u{1b}[31m, testy opisane.".to_string()),
                    },
                ),
            ];
            let mut conversation = Conversation::new("a");
            conversation.rebuild_with_events(&positions, &attempts(&["p"]), &events);
            let choice = conversation.transcript().entries().last().unwrap();
            assert_eq!(
                choice.rendered_lines,
                [
                    "agent · claude wybrał opcję: plan_ready".to_string(),
                    "uzasadnienie: Plan gotowy[31m, testy opisane.".to_string()
                ]
            );
        }

        #[test]
        fn an_unread_attempt_is_loading_and_its_choices_are_kept() {
            let positions = vec![recorded("plan", 1, "p"), recorded("fix", 1, "f")];
            let events = vec![
                entered(1, "plan"),
                agent_chose(2, "revise", None),
                entered(3, "fix"),
            ];
            use narsor::tui::task_app::tabs::conversation::{build_conversation, Scope};
            let built = build_conversation(Scope::Whole, &positions, &attempts(&["p"]), &events);
            assert!(built.loading);
            assert_eq!(tags(&built.entries), ["step:p", "p/m", "choice:2"]);
        }
    }

    mod events_and_conversation {
        use super::*;

        fn open_tab(app: &mut TaskApp, tab: DetailTab) {
            while app.detail().tab() != tab {
                press(app, KeyCode::Char(']'));
            }
        }

        #[test]
        fn a_failed_events_read_is_a_message_in_the_tab_and_not_a_lost_connection() {
            let mut app = details_app();
            open_tab(&mut app, DetailTab::Events);
            app.on_detail_read(
                "a",
                1,
                DetailRead::Events {
                    after: 0,
                    result: Err(ClientError::Degraded {
                        code: "runner_timeout".into(),
                        message: "the runner did not answer in time".into(),
                    }),
                },
            );
            let events = app.detail().view("a").unwrap().events();
            assert_eq!(events.failure(), Some("the runner did not answer in time"));
            assert!(app.connection().is_live());
            assert!(app.mutation_allowed());

            // An unreachable server is another matter.
            app.on_detail_read(
                "a",
                1,
                DetailRead::Events {
                    after: 0,
                    result: Err(ClientError::Unavailable("refused".into())),
                },
            );
            assert!(!app.connection().is_live());
        }

        #[test]
        fn unavailable_events_keep_the_reason_and_what_was_read_before() {
            let mut app = details_app();
            let available = |events| {
                Ok(ExecutionEventsResponse::Available {
                    events,
                    has_more: false,
                    source: ExecutionEventsSource::Runner,
                })
            };
            app.on_detail_read(
                "a",
                1,
                DetailRead::Events {
                    after: 0,
                    result: available(execution_events(3)),
                },
            );
            app.on_detail_read(
                "a",
                2,
                DetailRead::Events {
                    after: 3,
                    result: Ok(ExecutionEventsResponse::Unavailable {
                        reason: ConversationUnavailableReason::RunnerUnreachable,
                        message: "the runner has not been heard from lately".into(),
                    }),
                },
            );
            let events = app.detail().view("a").unwrap().events();
            assert_eq!(events.events().len(), 3);
            assert!(matches!(
                events.availability(),
                Availability::Unavailable {
                    reason: ConversationUnavailableReason::RunnerUnreachable,
                    ..
                }
            ));
        }

        #[test]
        fn events_are_appended_after_the_last_sequence_and_the_cursor_follows_the_end() {
            let mut app = details_app();
            let page = |events: Vec<Event>, has_more| {
                Ok(ExecutionEventsResponse::Available {
                    events,
                    has_more,
                    source: ExecutionEventsSource::Storage,
                })
            };
            app.on_detail_read(
                "a",
                1,
                DetailRead::Events {
                    after: 0,
                    result: page(execution_events(3), true),
                },
            );
            let events = app.detail().view("a").unwrap().events();
            assert!(events.has_more());
            assert_eq!(events.after(), 3);
            assert_eq!(events.cursor(), 2);
            app.on_detail_read(
                "a",
                1,
                DetailRead::Events {
                    after: 3,
                    result: page(execution_events(6)[3..].to_vec(), false),
                },
            );
            let events = app.detail().view("a").unwrap().events();
            assert_eq!(events.events().len(), 6);
            assert_eq!(
                events.cursor(),
                5,
                "a cursor on the last event stays on the last"
            );
        }

        #[test]
        fn v_opens_the_detail_of_the_event_and_ctrl_v_the_raw_rows() {
            let mut app = details_app();
            open_tab(&mut app, DetailTab::Events);
            app.on_detail_read(
                "a",
                1,
                DetailRead::Events {
                    after: 0,
                    result: Ok(ExecutionEventsResponse::Available {
                        events: execution_events(3),
                        has_more: false,
                        source: ExecutionEventsSource::Runner,
                    }),
                },
            );
            ch(&mut app, 'v');
            assert!(app.detail().view("a").unwrap().events().is_detail_open());
            ch(&mut app, 'v');
            assert!(!app.detail().view("a").unwrap().events().is_detail_open());
            app.on_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
            assert!(app.detail().view("a").unwrap().events().is_raw());
            ch(&mut app, 'g');
            assert_eq!(app.detail().view("a").unwrap().events().cursor(), 0);
        }

        fn entry(id: &str, text: &str) -> TranscriptEntry {
            TranscriptEntry::new(
                TranscriptEntryId::from(id),
                vec![text.to_string()],
                text.as_bytes().to_vec(),
            )
            .with_header(format!("AGENT · {id}"))
        }

        fn read(entries: Vec<TranscriptEntry>) -> AttemptRead {
            AttemptRead {
                entries,
                next_entry: 3,
                closed: true,
                truncated_tail: false,
                complete: true,
                no_record: false,
                source: narsor_engine::controller_protocol::ExecutionEventsSource::Runner,
                recorded_at: None,
            }
        }

        fn three_steps() -> Vec<narsor_protocol::api::Position> {
            let recorded = |state: &str| {
                let mut step = position(state, PositionStatus::Succeeded);
                step.conversation = ConversationState::Recorded;
                step
            };
            vec![
                recorded("plan"),
                position("script", PositionStatus::Succeeded),
                recorded("edit"),
            ]
        }

        fn texts(conversation: &Conversation) -> Vec<String> {
            conversation
                .transcript()
                .logical_lines()
                .into_iter()
                .map(|line| line.text)
                .collect()
        }

        #[test]
        fn the_conversation_is_the_newest_step_and_the_strip_lists_the_steps_newest_first() {
            let positions = three_steps();
            let mut attempts = Attempts::default();
            attempts.apply(
                "act-plan",
                1,
                0,
                Ok(read(vec![entry("1", "plan one"), entry("2", "plan two")])),
            );
            let mut conversation = Conversation::new("a");
            conversation.rebuild(&positions, &attempts);
            let labels: Vec<&str> = conversation
                .steps()
                .iter()
                .map(|step| step.label.as_str())
                .collect();
            assert_eq!(labels, ["edit #1", "script #1", "plan #1"]);
            assert_eq!(conversation.selected_step(), Some("edit #1"));
            assert!(conversation.is_loading(), "act-edit was not read yet");
            assert!(conversation.transcript().entries().is_empty());

            attempts.apply("act-edit", 1, 0, Ok(read(vec![entry("1", "edit one")])));
            conversation.rebuild(&positions, &attempts);
            assert!(!conversation.is_loading());
            let lines = texts(&conversation);
            assert!(
                lines.iter().any(|line| line.contains("edit one")),
                "{lines:?}"
            );
            assert!(
                !lines
                    .iter()
                    .any(|line| line.contains("plan one") || line.contains("#1")),
                "only the step, with no heading: {lines:?}"
            );
        }

        #[test]
        fn left_and_right_walk_the_strip_and_show_only_the_selected_step() {
            let positions = three_steps();
            let mut attempts = Attempts::default();
            attempts.apply("act-plan", 1, 0, Ok(read(vec![entry("1", "plan one")])));
            attempts.apply("act-edit", 1, 0, Ok(read(vec![entry("1", "edit one")])));
            let mut conversation = Conversation::new("a");
            conversation.rebuild(&positions, &attempts);

            assert!(!conversation.select_newer(), "the newest is selected");
            assert!(conversation.select_older());
            assert!(conversation.select_older());
            assert!(!conversation.select_older(), "the oldest is selected");
            assert_eq!(conversation.selected_step(), Some("plan #1"));
            assert!(!conversation.follows());
            conversation.rebuild(&positions, &attempts);
            let lines = texts(&conversation);
            assert!(
                lines.iter().any(|line| line.contains("plan one")),
                "{lines:?}"
            );
            assert!(
                !lines.iter().any(|line| line.contains("edit one")),
                "{lines:?}"
            );

            attempts.apply(
                "act-plan",
                2,
                1,
                Err(ReadFailure::Failed("the runner did not answer".into())),
            );
            conversation.rebuild(&positions, &attempts);
            assert_eq!(
                conversation.failure().expect("the failure").detail(),
                "the runner did not answer"
            );
            assert_eq!(
                conversation.transcript().entries().len(),
                1,
                "what was read stays"
            );
        }

        #[test]
        fn each_step_keeps_its_own_reader_across_switches() {
            let positions = three_steps();
            let mut attempts = Attempts::default();
            attempts.apply(
                "act-plan",
                1,
                0,
                Ok(read(vec![entry("1", "plan one"), entry("2", "plan two")])),
            );
            attempts.apply("act-edit", 1, 0, Ok(read(vec![entry("1", "edit one")])));
            let mut conversation = Conversation::new("a");
            conversation.rebuild(&positions, &attempts);
            conversation.select_older();
            conversation.select_older();
            conversation.rebuild(&positions, &attempts);
            narsor::tui::task_app::tabs::conversation::transcript_action(
                conversation.transcript_mut(),
                narsor::tui::keymap::Action::Top,
            );
            let picked = conversation.transcript().selected_entry().cloned();
            conversation
                .transcript_mut()
                .toggle_expanded(picked.as_ref().unwrap());

            conversation.select_newer();
            conversation.select_newer();
            conversation.rebuild(&positions, &attempts);
            assert_eq!(conversation.selected_step(), Some("edit #1"));
            conversation.select_older();
            conversation.select_older();
            conversation.rebuild(&positions, &attempts);
            assert_eq!(conversation.transcript().selected_entry().cloned(), picked);
            assert!(conversation
                .transcript()
                .is_expanded(picked.as_ref().unwrap()));
        }

        #[test]
        fn a_selected_step_that_is_gone_gives_way_to_the_newest() {
            let mut positions = three_steps();
            let attempts = Attempts::default();
            let mut conversation = Conversation::new("a");
            conversation.rebuild(&positions, &attempts);
            conversation.select_older();
            conversation.select_older();
            assert_eq!(conversation.selected_step(), Some("plan #1"));
            positions.remove(0);
            conversation.rebuild(&positions, &attempts);
            assert_eq!(conversation.selected_step(), Some("edit #1"));
            assert!(!conversation.follows(), "following stays off");

            conversation.rebuild(&[], &attempts);
            assert_eq!(conversation.selected_step(), None);
            assert!(conversation.transcript().entries().is_empty());
        }

        #[test]
        fn following_keeps_the_newest_step_until_a_step_is_picked_by_hand() {
            let mut positions = three_steps();
            let attempts = Attempts::default();
            let mut conversation = Conversation::new("a");
            conversation.rebuild(&positions, &attempts);
            assert!(conversation.follows(), "a new tab follows");
            positions.push(position("review", PositionStatus::InProgress));
            conversation.rebuild(&positions, &attempts);
            assert_eq!(conversation.selected_step(), Some("review #1"));

            conversation.select_older();
            conversation.select_newer();
            assert!(
                !conversation.follows(),
                "back on the newest by hand is not following"
            );
            positions.push(position("merge", PositionStatus::InProgress));
            conversation.rebuild(&positions, &attempts);
            assert_eq!(
                conversation.selected_step(),
                Some("review #1"),
                "a picked step stays"
            );

            assert!(conversation.toggle_follow());
            conversation.rebuild(&positions, &attempts);
            assert_eq!(conversation.selected_step(), Some("merge #1"));
            assert!(conversation.transcript().is_following_end());
            positions.push(position("deploy", PositionStatus::InProgress));
            conversation.rebuild(&positions, &attempts);
            assert_eq!(conversation.selected_step(), Some("deploy #1"));

            assert!(!conversation.toggle_follow());
            assert!(!conversation.follows());
        }

        #[test]
        fn scrolling_up_in_the_conversation_ends_the_following() {
            let positions = three_steps();
            let mut attempts = Attempts::default();
            attempts.apply(
                "act-edit",
                1,
                0,
                Ok(read(vec![entry("1", "edit one"), entry("2", "edit two")])),
            );
            let mut conversation = Conversation::new("a");
            conversation.rebuild(&positions, &attempts);
            assert!(conversation.follows());
            conversation.read(narsor::tui::keymap::Action::Top);
            assert!(!conversation.follows());

            conversation.toggle_follow();
            conversation.rebuild(&positions, &attempts);
            assert!(conversation.follows());
            assert!(
                conversation.transcript().is_following_end(),
                "back at the newest entry"
            );
        }

        #[test]
        fn the_arrows_in_the_conversation_tab_change_the_step() {
            let mut app = details_app();
            open_tab(&mut app, DetailTab::Conversation);
            let selected = |app: &TaskApp| {
                app.detail()
                    .view("a")
                    .unwrap()
                    .conversation()
                    .selected_step()
                    .map(str::to_string)
            };
            let positions = ["plan", "edit"]
                .iter()
                .map(|state| position(state, PositionStatus::Succeeded))
                .collect();
            app.on_detail_reads(
                "a",
                1,
                vec![DetailRead::Timeline(Ok(timeline(
                    "a",
                    positions,
                    Some("edit #1"),
                )))],
            );
            assert_eq!(selected(&app).as_deref(), Some("edit #1"));
            press(&mut app, KeyCode::Right);
            assert_eq!(selected(&app).as_deref(), Some("plan #1"));
            press(&mut app, KeyCode::Left);
            assert_eq!(selected(&app).as_deref(), Some("edit #1"));
            assert_eq!(app.detail().tab(), DetailTab::Conversation);
        }

        #[test]
        fn keys_scroll_expand_and_switch_to_the_raw_view_of_the_conversation() {
            let mut app = details_app();
            open_tab(&mut app, DetailTab::Conversation);
            // No transcript yet: keys are harmless.
            ch(&mut app, 'j');
            press(&mut app, KeyCode::PageDown);
            ch(&mut app, 'G');
            ch(&mut app, 'v');
            assert_eq!(app.detail().tab(), DetailTab::Conversation);
        }
    }

    mod relay {
        use super::*;

        /// Any other reason is shown with what was read last, and is not asked
        /// again until the task changes.
        #[test]
        fn an_unavailable_answer_with_another_reason_keeps_the_last_events_and_does_not_retry() {
            let now = std::time::Instant::now();
            let later = now + std::time::Duration::from_secs(3600);
            let mut events = narsor::tui::task_app::tabs::events::Events::default();
            events.apply_at(
                1,
                0,
                Ok(ExecutionEventsResponse::Available {
                    events: execution_events(2),
                    has_more: false,
                    source: ExecutionEventsSource::Runner,
                }),
                now,
            );
            events.apply_at(
                1,
                2,
                Ok(ExecutionEventsResponse::Unavailable {
                    reason: ConversationUnavailableReason::Archived,
                    message: "the task is archived".to_string(),
                }),
                now,
            );
            assert_eq!(events.events().len(), 2, "the last snapshot stays");
            assert!(!events.is_starting());
            assert!(!events.needs_read_at(1, later), "no loop");
            assert!(events.needs_read_at(2, now), "a new revision reads again");
            match events.availability() {
                Availability::Unavailable { reason, message } => {
                    assert_eq!(*reason, ConversationUnavailableReason::Archived);
                    assert_eq!(message, "the task is archived");
                }
                other => panic!("expected the reason, got {other:?}"),
            }

            let mut attempts = narsor::tui::task_app::tabs::conversation::Attempts::default();
            attempts.apply_at(
                "act-1",
                1,
                0,
                Err(ReadFailure::Unavailable("it cannot be read".to_string())),
                now,
            );
            assert!(!attempts.needs_read_at("act-1", 1, later), "no loop");
        }

        #[test]
        fn the_conductor_of_a_task_whose_reader_is_starting_loads_and_is_asked_again() {
            use narsor::tui::task_app::tabs::conductor::{Conductor, Standing};
            let now = std::time::Instant::now();
            let later = now + std::time::Duration::from_secs(60);
            let mut conductor = Conductor::new("task-1");
            conductor.apply(
                1,
                now,
                Ok(
                    narsor::controller_protocol::ConductorReadResponse::Unavailable {
                        reason: ConversationUnavailableReason::ReaderStarting,
                        message: "starting".to_string(),
                    },
                ),
            );
            assert!(conductor.is_starting());
            assert!(conductor.note().is_none());
            assert!(!conductor.needs_read(1, now), "no read without a pause");
            assert!(conductor.needs_read(1, later));
            let mut task = running("task-1", 1);
            task.runner = None;
            assert!(matches!(conductor.standing(&task), Standing::Loading));

            conductor.apply(
                1,
                later,
                Ok(
                    narsor::controller_protocol::ConductorReadResponse::Unavailable {
                        reason: ConversationUnavailableReason::Paused,
                        message: "paused".to_string(),
                    },
                ),
            );
            assert!(!conductor.is_starting());
            assert_eq!(conductor.note(), Some("paused"));
            assert!(!conductor.needs_read(1, later + std::time::Duration::from_secs(3600)));
        }
    }

    mod odds_and_ends {
        use super::*;

        #[test]
        fn ages_use_the_largest_whole_unit() {
            assert_eq!(short_age(-5), "0s");
            assert_eq!(short_age(45), "45s");
            assert_eq!(short_age(240), "4m");
            assert_eq!(short_age(3 * 3600 + 59), "3h");
            assert_eq!(short_age(2 * 86400), "2d");
        }

        #[test]
        fn hyperlinks_are_only_for_terminals_known_to_draw_them() {
            let env = |pairs: &'static [(&'static str, &'static str)]| {
                move |name: &str| {
                    pairs
                        .iter()
                        .find(|(key, _)| *key == name)
                        .map(|(_, value)| (*value).to_string())
                }
            };
            assert!(hyperlinks_supported(env(&[("TERM_PROGRAM", "iTerm.app")])));
            assert!(hyperlinks_supported(env(&[("WT_SESSION", "x")])));
            assert!(hyperlinks_supported(env(&[("VTE_VERSION", "6800")])));
            assert!(hyperlinks_supported(env(&[("TERM", "xterm-kitty")])));
            assert!(!hyperlinks_supported(env(&[("VTE_VERSION", "4000")])));
            assert!(!hyperlinks_supported(env(&[("TERM", "xterm-256color")])));
            assert!(!hyperlinks_supported(env(&[
                ("TERM", "dumb"),
                ("WT_SESSION", "x")
            ])));
        }

        #[test]
        fn the_areas_leave_room_for_a_panel_at_the_bottom() {
            let area = ratatui::layout::Rect::new(10, 2, 80, 20);
            let plain = detail::areas(area, 4, 0);
            assert_eq!(plain.header.height, 4);
            assert_eq!(plain.tabs.y, area.y + 4);
            assert_eq!(plain.content.y, area.y + 6);
            assert_eq!(plain.content.height, 14);
            assert_eq!(plain.panel, None);

            let with_panel = detail::areas(area, 4, 6);
            assert_eq!(with_panel.content.height, 7);
            let panel = with_panel.panel.expect("a panel");
            assert_eq!((panel.y, panel.height), (area.y + 14, 6));
            assert_eq!(
                panel.y - (with_panel.content.y + with_panel.content.height),
                1
            );
            assert_eq!(
                detail::areas(area, 4, 40).content.height,
                1,
                "the content keeps a row"
            );

            let minimum = ratatui::layout::Rect::new(0, 0, 60, 16);
            let with_notice = detail::areas(minimum, 5, 8);
            assert_eq!(with_notice.header.height, 5);
            assert_eq!(with_notice.tabs.height, 2);
            assert_eq!(with_notice.panel.expect("panel").height, 7);
            assert_eq!(with_notice.content.height, 1);

            let without_notice = detail::areas(minimum, 4, 9);
            assert_eq!(without_notice.panel.expect("panel").height, 8);
            assert_eq!(without_notice.content.height, 1);
        }

        #[test]
        fn the_details_of_a_task_that_left_the_list_are_forgotten() {
            let mut app = details_app();
            read_timeline_for(&mut app, "b");
            assert!(app.detail().view("b").is_some());
            app.load(listing(vec![titled(running("a", 1), "NAR-1")], "d"));
            assert!(app.detail().view("b").is_none());
        }

        fn read_timeline_for(app: &mut TaskApp, id: &str) {
            app.on_detail_read(
                id,
                1,
                DetailRead::Timeline(Ok(timeline(
                    id,
                    vec![position("plan", PositionStatus::Succeeded)],
                    None,
                ))),
            );
        }
    }

    mod tab_keys {
        use super::*;

        use narsor::tui::keymap::{self, Action, Context, Stack};

        fn screen(app: &mut TaskApp, width: u16, height: u16) -> String {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    narsor::tui::task_app::layout::render(
                        frame,
                        app,
                        &Styles::new(false),
                        std::time::Instant::now(),
                    )
                })
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            buffer
                .content()
                .chunks(usize::from(width))
                .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        }

        #[test]
        fn brackets_switch_the_tabs_and_wrap_round() {
            let mut app = details_app();
            assert_eq!(app.detail().tab(), DetailTab::Conversation);
            ch(&mut app, ']');
            assert_eq!(app.detail().tab(), DetailTab::Progress);
            ch(&mut app, ']');
            assert_eq!(app.detail().tab(), DetailTab::Events);
            ch(&mut app, '[');
            ch(&mut app, '[');
            assert_eq!(app.detail().tab(), DetailTab::Conversation);
            ch(&mut app, '[');
            assert_eq!(app.detail().tab(), DetailTab::Info, "wraps round");
            ch(&mut app, ']');
            assert_eq!(app.detail().tab(), DetailTab::Conversation, "wraps round");
        }

        #[test]
        fn przebieg_has_the_axis_layer_and_every_other_tab_the_reading_one() {
            let mut app = progress_app();
            assert_eq!(
                app.key_context(),
                Stack::layered(Context::Axis, Context::Details)
            );
            for tab in [DetailTab::Events, DetailTab::Conductor, DetailTab::Info] {
                app.detail_mut().set_tab(tab);
                assert_eq!(
                    app.key_context(),
                    Stack::layered(Context::Reading, Context::Details),
                    "{tab:?}"
                );
            }
            app.detail_mut().set_tab(DetailTab::Conversation);
            assert_eq!(
                app.key_context(),
                Stack::layered(Context::AgentConversation, Context::Details)
            );
            // Reading the conversation: the keys of the tab, as before.
            app.detail_mut().set_tab(DetailTab::Conversation);
            for (key, action) in [
                (KeyCode::Char('j'), Action::Down),
                (KeyCode::Enter, Action::Expand),
                (KeyCode::Char('v'), Action::NextConversationView),
                (KeyCode::Char('r'), Action::RawEntry),
                (KeyCode::Esc, Action::Back),
            ] {
                assert_eq!(
                    keymap::lookup(app.key_context(), &KeyEvent::from(key)),
                    Some(action)
                );
            }
            press(&mut app, KeyCode::Esc);
            assert_eq!(app.focus(), Focus::List);
        }

        #[test]
        fn on_the_axis_j_picks_the_step_and_enter_stays_on_it() {
            let mut app = progress_app();
            read_timeline(&mut app, "a", 1, &["plan", "edit", "review"], "review #1");
            ch(&mut app, 'k');
            assert_eq!(selected_step(&app, "a").as_deref(), Some("edit #1"));
            press(&mut app, KeyCode::Enter);
            press(&mut app, KeyCode::Right);
            press(&mut app, KeyCode::PageDown);
            assert_eq!(selected_step(&app, "a").as_deref(), Some("edit #1"));
            assert_eq!(app.detail().tab(), DetailTab::Progress);
            assert_eq!(app.focus(), Focus::Details);
            press(&mut app, KeyCode::Esc);
            assert_eq!(app.focus(), Focus::List);
        }

        #[test]
        fn the_bar_of_the_axis_offers_the_step_conversation() {
            let mut app = progress_app();
            let screen = screen(&mut app, 120, 30);
            assert!(screen.contains("Enter/→ rozmowa kroku"), "{screen}");
            assert!(!screen.contains("rozwiń"), "{screen}");
        }

        #[test]
        fn the_arrows_do_not_switch_the_tabs() {
            let mut app = details_app();
            app.detail_mut().set_tab(DetailTab::Info);
            press(&mut app, KeyCode::Right);
            assert_eq!(app.detail().tab(), DetailTab::Info);
            press(&mut app, KeyCode::Left);
            assert_eq!(app.detail().tab(), DetailTab::Info);
        }
    }
}

/// U10: the panel "Czeka na Ciebie". The first group characterises the form
/// semantics of the old `InputForm`, `ProblemForm` and `FailureForm`
/// (`3f3dab2`, `4ca1e73`, `2a9c6f2`, `9c6af7f`, `c7f3f0d`) on the new panel;
/// the rest is what the panel adds. Nothing here starts a daemon.
mod task_app_waiting {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::{json, Value};

    use narsor::api::{ApiClient, ClientError};
    use narsor::tui::chooser::ChooserFocus;
    use narsor::tui::task_app::detail::DetailRead;
    use narsor::tui::task_app::notify::Severity;
    use narsor::tui::task_app::panel::{AnswerRequest, Submit};
    use narsor::tui::task_app::{Focus, Request, TaskApp};
    use narsor_engine::engine::actions::human::HumanInputAction;
    use narsor_engine::engine::PositionStatus;
    use narsor_engine::orchestration::{
        AttentionKind, OperatorCommand, OwnerCommand, RunnerState, TaskEventKind,
    };
    use narsor_protocol::controller::ConductorView;
    use narsor_protocol::controller::{ConductorReadResponse, ExecutionEventsSource};

    use super::task_app_support::{
        event, listing, position, task, timeline, titled, waiting_on, with_controller,
        with_pending, with_runner,
    };
    use narsor_engine::domain::ExecutionStatus;

    fn app() -> TaskApp {
        TaskApp::new(
            ApiClient::new("127.0.0.1:9".parse().unwrap(), "token"),
            "local",
            None,
        )
    }

    /// One key. An `Enter` that asks for the confirmation of a send is
    /// answered with the `Enter` of the window, so a test that says "Enter
    /// sends" still reads so; the window itself is tested with `on_key`.
    fn press(app: &mut TaskApp, code: KeyCode) -> Request {
        let request = app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
        if code == KeyCode::Enter && request == Request::None && app.form_confirming() {
            return app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        }
        request
    }

    fn ch(app: &mut TaskApp, character: char) -> Request {
        press(app, KeyCode::Char(character))
    }

    fn typed(app: &mut TaskApp, text: &str) {
        for character in text.chars() {
            ch(app, character);
        }
    }

    fn sent(request: Request) -> AnswerRequest {
        match request {
            Request::Answer(answer) => *answer,
            other => panic!("an answer was expected, got {other:?}"),
        }
    }

    fn delivery(request: Request) -> narsor_protocol::api::HumanInputDeliveryDto {
        match sent(request).submit {
            Submit::Input { delivery, .. } => *delivery,
            other => panic!("an input was expected, got {other:?}"),
        }
    }

    fn running(id: &str, rank: u32) -> narsor_engine::controller_protocol::TaskSummary {
        with_runner(
            with_controller(task(id, "local", "running", rank)),
            ExecutionStatus::InputNeeded,
            "review_scope",
        )
    }

    /// An app whose only task `a` waits; the panel is open on its actions.
    fn opened(task: narsor_engine::controller_protocol::TaskSummary) -> TaskApp {
        let mut app = app();
        app.load(listing(vec![titled(task, "NAR-12")], "c"));
        ch(&mut app, ' ');
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.focus(), Focus::Details);
        assert!(
            app.panel_focused(),
            "Enter on a waiting task opens its panel"
        );
        app
    }

    #[test]
    fn tab_and_shift_tab_leave_the_waiting_panel_visible() {
        for key in [KeyCode::Tab, KeyCode::BackTab] {
            let mut app = opened(waiting_on(
                running("a", 1),
                AttentionKind::Problem,
                "review",
                None,
            ));
            press(&mut app, key);
            assert!(!app.panel_focused(), "{key:?} returns focus to the details");
            assert!(
                !app.detail().view("a").unwrap().waiting().is_collapsed(),
                "changing focus leaves the waiting panel visible"
            );
        }
    }

    #[test]
    fn tab_from_task_details_reenters_the_waiting_panel() {
        let mut app = opened(waiting_on(
            running("a", 1),
            AttentionKind::Problem,
            "review",
            None,
        ));
        press(&mut app, KeyCode::Tab);
        assert!(!app.panel_focused());
        press(&mut app, KeyCode::Tab);
        assert!(app.panel_focused());
        assert!(!app.detail().view("a").unwrap().waiting().is_collapsed());
    }

    fn chooser_of(app: &TaskApp) -> &narsor::tui::chooser::Chooser {
        app.detail()
            .view("a")
            .expect("details")
            .waiting()
            .chooser()
            .expect("a panel")
    }

    fn composer_of(app: &TaskApp) -> &narsor::tui::chooser::Chooser {
        app.detail()
            .view("a")
            .expect("details")
            .waiting()
            .composer()
            .expect("a text composer")
    }

    fn selected_label(app: &TaskApp) -> String {
        chooser_of(app)
            .selected_item()
            .expect("an entry")
            .label
            .clone()
    }

    fn enter_until_answer(app: &mut TaskApp) -> Request {
        let task_id = app.list().selected_id().expect("selected task");
        let fields = app
            .detail()
            .view(task_id)
            .and_then(|view| view.waiting().chooser())
            .and_then(|chooser| chooser.selected_item())
            .map_or(0, |item| item.fields.len());
        let mut request = Request::None;
        for _ in 0..=fields {
            request = press(app, KeyCode::Enter);
            if request != Request::None {
                break;
            }
        }
        request
    }

    fn enter_until_error(app: &mut TaskApp) -> bool {
        for _ in 0..8 {
            let request = press(app, KeyCode::Enter);
            let task_id = app.list().selected_id().expect("selected task");
            let waiting = app.detail().view(task_id).unwrap().waiting();
            let field_error = waiting.chooser().is_some_and(|chooser| {
                chooser
                    .items()
                    .iter()
                    .any(|item| item.fields.iter().any(|field| field.error().is_some()))
            });
            if waiting.error().is_some() || field_error {
                return true;
            }
            if request != Request::None {
                return false;
            }
        }
        false
    }

    fn screen(app: &mut TaskApp, width: u16, height: u16) -> String {
        use narsor::tui::style::Styles;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                narsor::tui::task_app::layout::render(
                    frame,
                    app,
                    &Styles::new(false),
                    std::time::Instant::now(),
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer
            .content()
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    // -- forms -------------------------------------------------------------

    /// A gate with a resume comment: only `revise` resumes the agent.
    fn gate(episode: &str) -> Value {
        json!({
            "execution_id": "exec-1",
            "state_id": "review_scope",
            "action_execution_id": "act-7",
            "instruction": "Czy zakres zmian obejmuje też migrację danych?",
            "question": "Czy migracja wchodzi w zakres?",
            "fields": [{ "name": "comment", "type": "text", "required": true }],
            "outputs": [
                { "label": "approved" },
                { "label": "revise", "description": "Zakres wymaga poprawy.", "resume_comment_field": "comment" },
                { "label": "reject" }
            ],
            "resume_targets": { "revise": { "action_execution_id": "act-plan", "harness": "claude", "session_id": "s1" } },
            "episode": episode,
            "requested_at": "2026-10-09T11:54:00Z"
        })
    }

    fn question(prefilled: Option<&str>) -> Value {
        json!({
            "execution_id": "exec-1",
            "state_id": "review_scope",
            "action_execution_id": "act-7",
            "instruction": "Która wersja API?",
            "fields": [{ "name": "answer", "type": "text", "required": true }],
            "continuation": "resume_agent",
            "prefilled_answer": prefilled,
            "episode": "ep-q",
            "requested_at": "2026-10-09T11:54:00Z"
        })
    }

    fn approval(resumable: bool) -> Value {
        json!({
            "execution_id": "exec-1",
            "state_id": "review_scope",
            "action_execution_id": "act-7",
            "instruction": "Zatwierdź wynik",
            "outputs": [{ "label": "reviewed", "description": "Wynik przeglądu." }],
            "continuation": "approve_output",
            "approval": {
                "action_execution_id": "act-7",
                "result": { "output_label": "reviewed", "data": { "files": 3 }, "variables": {}, "exit_code": null, "stdout": "", "stderr": "" },
                "agent_session_id": if resumable { json!("sess-1") } else { Value::Null }
            },
            "episode": "ep-a",
            "requested_at": "2026-10-09T11:54:00Z"
        })
    }

    fn problem(proposed: bool) -> Value {
        json!({
            "execution_id": "exec-1",
            "state_id": "implement",
            "action_execution_id": "act-9",
            "instruction": "Krok nie dał etykiety",
            "outputs": [
                { "label": "implemented", "description": "Funkcja gotowa." },
                { "label": "needs_human_decision", "description": "Najpierw decyzja człowieka." }
            ],
            "continuation": "resolve_output",
            "problem": {
                "reason": "Odpowiedź agenta nie zawiera etykiety.",
                "answers": [{ "action_execution_id": "act-9", "reason": "brak etykiety", "answer_tail": "Done, all tests pass." }],
                "requirements": [
                    { "label": "implemented", "variables": ["summary"], "artifacts": [] },
                    { "label": "needs_human_decision", "variables": [], "artifacts": ["decision_doc"] }
                ],
                "analysis": if proposed { json!({
                    "label": "implemented",
                    "artifacts": {},
                    "variables": { "summary": "all units done", "files": 3 },
                    "rationale": "Odpowiedź mówi, że testy przechodzą."
                }) } else { Value::Null }
            },
            "episode": "ep-p",
            "requested_at": "2026-10-09T11:54:00Z"
        })
    }

    fn entry_limit() -> Value {
        json!({
            "execution_id": "exec-1",
            "state_id": "code_review",
            "action_execution_id": "act-limit",
            "instruction": "Krok osiągnął limit wejść.",
            "fields": [{
                "name": "entry_limit_decision",
                "type": "select",
                "options": ["continue", "reset_counter"],
                "required": false
            }],
            "outputs": [
                { "label": "clean", "description": "Brak ustaleń." },
                { "label": "findings", "description": "Są ustalenia." }
            ],
            "continuation": "entry_limit",
            "problem": {
                "reason": "entry limit reached (5/5)",
                "requirements": [
                    { "label": "clean", "variables": [] },
                    { "label": "findings", "variables": ["manual_note"] }
                ]
            },
            "entry_limit": { "entries": 5, "max_entries": 5 },
            "episode": "ep-limit",
            "requested_at": "2026-10-18T12:00:00Z"
        })
    }

    /// The gate, with the cursor on `revise`: the panel opens on the first entry.
    fn gate_app() -> TaskApp {
        let mut app = opened(with_pending(running("a", 1), gate("ep-1")));
        ch(&mut app, '2');
        app
    }

    // -- characterisation of the form semantics --------------------------------

    #[test]
    fn a_resume_comment_is_required_only_for_the_output_that_resumes_the_agent() {
        // 3f3dab2: `approved` has no need of the comment, `revise ↩` does.
        let mut app = opened(with_pending(running("a", 1), gate("ep-1")));
        // The state the agent returns to is known from the steps read.
        app.on_detail_read(
            "a",
            1,
            DetailRead::Timeline(Ok(timeline(
                "a",
                vec![position("plan", PositionStatus::Succeeded)],
                None,
            ))),
        );
        let items = chooser_of(&app).items();
        assert!(
            !items[0].fields[0].required,
            "approved: the comment is optional"
        );
        assert!(
            items[1].fields[0].required,
            "revise: the comment is required"
        );
        assert!(!items[2].fields[0].required);
        assert_eq!(
            items[1].resumes_agent.as_deref(),
            Some("plan"),
            "↩ wraca do agenta: plan"
        );
        assert!(items[0].resumes_agent.is_none());

        // The panel opens on the first output, the one that does not resume.
        assert_eq!(selected_label(&app), "approved");
        ch(&mut app, '2');
        assert_eq!(selected_label(&app), "revise");
        // Enter without the comment takes the operator to the field, sends nothing.
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
        assert_eq!(chooser_of(&app).focus(), ChooserFocus::Fields);
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            Request::None,
            "still refused: the comment is empty"
        );
        assert!(chooser_of(&app).fields_of(1)[0].error().is_some());

        typed(&mut app, "Uwzględnij migrację");
        let delivery = delivery(press(&mut app, KeyCode::Enter));
        assert_eq!(delivery.chosen_label.as_deref(), Some("revise"));
        assert_eq!(delivery.values["comment"], "Uwzględnij migrację");
        assert_eq!(
            delivery.episode.as_deref(),
            Some("ep-1"),
            "the answer names its request"
        );
    }

    /// The `revise` gate of a workflow whose comment field is optional
    /// (KTD12): only the choice that resumes the agent needs it.
    fn optional_comment_gate() -> Value {
        json!({
            "execution_id": "exec-1",
            "state_id": "review_scope",
            "action_execution_id": "act-7",
            "instruction": "Przejrzyj pracę.",
            "fields": [{ "name": "comment", "type": "text", "required": false }],
            "outputs": [
                { "label": "approve" },
                { "label": "revise", "resume_comment_field": "comment" }
            ],
            "episode": "ep-r",
            "requested_at": "2026-10-09T11:54:00Z"
        })
    }

    #[test]
    fn revise_with_an_empty_optional_comment_is_refused_at_the_field() {
        let mut app = opened(with_pending(running("a", 1), optional_comment_gate()));
        ch(&mut app, '2');
        assert_eq!(selected_label(&app), "revise");
        assert!(chooser_of(&app).items()[1].fields[0].required);

        assert!(enter_until_error(&mut app), "the empty comment is refused");
        let field = &chooser_of(&app).fields_of(1)[0];
        assert_eq!(field.name, "comment");
        assert!(
            field.error().is_some_and(|error| error.contains("comment")),
            "{:?}",
            field.error()
        );
    }

    #[test]
    fn approve_with_an_empty_optional_comment_is_sent() {
        let mut app = opened(with_pending(running("a", 1), optional_comment_gate()));
        ch(&mut app, '1');
        assert_eq!(selected_label(&app), "approve");
        assert!(!chooser_of(&app).items()[0].fields[0].required);
        let delivery = delivery(enter_until_answer(&mut app));
        assert_eq!(delivery.chosen_label.as_deref(), Some("approve"));
        assert!(delivery.values.is_empty());
    }

    #[test]
    fn an_output_without_a_need_for_the_comment_is_sent_at_once() {
        let mut app = gate_app();
        ch(&mut app, '1');
        assert_eq!(selected_label(&app), "approved");
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
        let request = sent(press(&mut app, KeyCode::Enter));
        assert_eq!(request.summary, "odpowiedziano approved");
        let Submit::Input {
            pending_input_id,
            delivery,
        } = request.submit
        else {
            panic!("an input");
        };
        assert_eq!(pending_input_id, "ep-1");
        assert_eq!(delivery.chosen_label.as_deref(), Some("approved"));
        assert!(
            delivery.values.is_empty(),
            "a blank optional field is left out"
        );
    }

    #[test]
    fn a_select_that_offers_exactly_the_outputs_is_the_label_and_not_a_field() {
        // KTD17: the picked value is both the field and the chosen label.
        let form = json!({
            "execution_id": "exec-1",
            "state_id": "resolve_ambiguity",
            "action_execution_id": "act-1",
            "instruction": "Agent zatrzymał się na niejednoznaczności.",
            "fields": [
                { "name": "ambiguity_decision", "type": "select", "required": true, "options": ["answered", "replan"] },
                { "name": "ambiguity_answer", "type": "text" }
            ],
            "outputs": [{ "label": "answered" }, { "label": "replan" }],
            "episode": "ep-g",
            "requested_at": "2026-10-09T11:54:00Z"
        });
        let mut app = opened(with_pending(running("a", 1), form));
        let items = chooser_of(&app).items();
        assert_eq!(items.len(), 2, "one entry per output, no label row");
        assert!(items
            .iter()
            .all(|item| item.fields.len() == 1 && item.fields[0].name == "ambiguity_answer"));
        ch(&mut app, '2');
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
        let delivery = delivery(press(&mut app, KeyCode::Enter));
        assert_eq!(delivery.chosen_label.as_deref(), Some("replan"));
        assert_eq!(delivery.values["ambiguity_decision"], "replan");
    }

    fn note(app: &TaskApp) -> String {
        app.notifier()
            .latest()
            .map(|note| note.text.clone())
            .unwrap_or_default()
    }

    #[test]
    fn a_rejected_resume_gives_the_reply_back_in_the_field() {
        // 2a9c6f2: the text of an answer that could not be delivered returns.
        let mut app = opened(with_pending(running("a", 1), question(Some("wersja 2"))));
        assert_eq!(composer_of(&app).fields_of(0)[0].value(), "wersja 2");
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
        let delivery = delivery(press(&mut app, KeyCode::Enter));
        assert_eq!(delivery.values["answer"], "wersja 2");
        assert!(delivery.chosen_label.is_none(), "a question has no label");
    }

    #[test]
    fn a_question_is_answered_with_the_text_alone() {
        let mut app = opened(with_pending(running("a", 1), question(None)));
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            Request::None,
            "the answer is required"
        );
        assert_eq!(composer_of(&app).focus(), ChooserFocus::Fields);
        typed(&mut app, "ab");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        typed(&mut app, "cd");
        press(&mut app, KeyCode::Left);
        press(&mut app, KeyCode::Left);
        typed(&mut app, "X");
        assert_eq!(
            composer_of(&app).fields_of(0)[0].value(),
            "ab\nXcd",
            "an editor, not an append-only line"
        );
        let delivery = delivery(press(&mut app, KeyCode::Enter));
        assert_eq!(
            delivery.values["answer"], "ab\nXcd",
            "typed text goes out unchanged"
        );
    }

    /// AE4: an empty comment is not sent, and the field says why.
    #[test]
    fn an_empty_comment_returning_the_work_to_the_agent_is_not_sent() {
        let mut app = opened(with_pending(running("a", 1), approval(true)));
        ch(&mut app, '2');
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
        let field = &chooser_of(&app).fields_of(1)[0];
        assert!(field.error().is_some(), "{:?}", field.error());
    }

    /// AE3: a wait without a session of the agent shows action 0 greyed with
    /// the reason of the server, and choosing it opens nothing.
    #[test]
    fn write_to_the_agent_is_greyed_with_the_reason_where_no_session_waits() {
        let failed = waiting_on(
            running("a", 1),
            AttentionKind::FailedStep,
            "fk1",
            Some("test"),
        );
        let mut app = opened(failed);
        let first = chooser_of(&app).items()[0].clone();
        assert_eq!(first.label, "Napisz do agenta");
        let reason = first.unavailable.expect("a reason");
        assert!(!reason.is_empty());
        ch(&mut app, '1');
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
        assert_eq!(chooser_of(&app).focus(), ChooserFocus::Actions);
        assert!(note(&app).contains(&reason), "{}", note(&app));
    }

    /// R8: a send that fails keeps the text in the field, and the panel says
    /// why.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_message_to_the_agent_keeps_its_text() {
        let mut app = opened(with_pending(running("a", 1), approval(true)));
        ch(&mut app, '2');
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "pierwsza\nlinia");
        let request = enter_until_answer(&mut app);
        app.perform(request).await;

        let waiting = app.detail().view("a").unwrap().waiting();
        assert!(waiting.error().is_some(), "the panel says why");
        assert_eq!(
            chooser_of(&app).fields_of(1)[0].value(),
            "pierwsza\nlinia",
            "the text stays in the field"
        );
        // The connection is down: the retry waits for it, the text stays.
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
        assert_eq!(chooser_of(&app).fields_of(1)[0].value(), "pierwsza\nlinia");
    }

    #[test]
    fn lines_pasted_into_an_answer_stay_in_the_field_until_enter_sends_them() {
        let mut app = opened(with_pending(running("a", 1), question(None)));
        press(&mut app, KeyCode::Enter);
        assert_eq!(composer_of(&app).focus(), ChooserFocus::Fields);
        app.on_paste("wersja 2\r\nbez zmian\r\n");
        assert_eq!(
            composer_of(&app).fields_of(0)[0].value(),
            "wersja 2\nbez zmian\n"
        );
        assert!(app.panel_focused(), "the keys stay in the panel");
        assert!(app
            .detail()
            .draft("a")
            .is_some_and(|draft| draft.contains("wersja 2\nbez zmian")));
        let delivery = delivery(press(&mut app, KeyCode::Enter));
        assert_eq!(delivery.values["answer"], "wersja 2\nbez zmian\n");
    }

    #[test]
    fn a_paste_into_the_note_of_a_failed_step_is_sent_only_by_enter() {
        let failed = waiting_on(
            running("a", 1),
            AttentionKind::FailedStep,
            "fk1",
            Some("test"),
        );
        let mut app = opened(failed);
        read_conductor(&mut app, conductor(true));
        ch(&mut app, '4');
        press(&mut app, KeyCode::Enter);
        app.on_paste("użyj\tnowszego toolchaina");
        assert_eq!(
            sent(press(&mut app, KeyCode::Enter)).submit,
            Submit::Operator(OperatorCommand::RestartStep {
                note: Some("użyj\tnowszego toolchaina".to_string()),
                failure_key: Some("fk1".to_string()),
            })
        );
    }

    #[test]
    fn a_paste_into_a_refused_answer_clears_the_refusal_as_typing_does() {
        let mut app = opened(with_pending(running("a", 1), problem(false)));
        ch(&mut app, '1');
        press(&mut app, KeyCode::Enter);
        // A multi-line field lets go on the second arrow in a row.
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        typed(&mut app, "bez znaku");
        assert!(enter_until_error(&mut app), "a key without '=' is refused");
        let error = |app: &TaskApp| {
            app.detail()
                .view("a")
                .unwrap()
                .waiting()
                .error()
                .map(str::to_string)
        };
        assert!(error(&app).is_some());

        app.on_paste("=1");
        assert_eq!(
            error(&app),
            None,
            "the refusal was about the text before the paste"
        );
    }

    #[test]
    fn a_paste_on_the_actions_of_the_panel_is_ignored() {
        let mut app = opened(with_pending(running("a", 1), gate("ep-1")));
        let before = selected_label(&app);
        app.on_paste("2\n");
        assert_eq!(selected_label(&app), before);
        assert_eq!(chooser_of(&app).focus(), ChooserFocus::Actions);
        assert!(chooser_of(&app)
            .items()
            .iter()
            .all(|item| item.fields.iter().all(|field| field.value().is_empty())));
        assert_eq!(app.detail().draft("a").unwrap_or_default(), "");
    }

    #[test]
    fn a_paste_on_the_task_list_runs_no_key_it_holds() {
        let mut app = app();
        app.load(listing(
            vec![
                titled(running("a", 1), "NAR-12"),
                titled(running("b", 2), "NAR-13"),
            ],
            "c",
        ));
        assert_eq!(app.focus(), Focus::List);
        let selected = app.list().selected_id().map(str::to_string);
        app.on_paste("j\nq\nq\n");
        assert_eq!(app.focus(), Focus::List, "no details opened");
        assert_eq!(app.list().selected_id(), selected.as_deref());
        assert!(!app.should_quit());
        ch(&mut app, 'q');
        assert!(
            !app.should_quit(),
            "the paste did not open the exit question"
        );
    }

    #[test]
    fn a_paste_leaves_the_exit_question_and_the_help_open() {
        let loaded = || {
            let mut loaded = app();
            loaded.load(listing(vec![titled(running("a", 1), "NAR-12")], "c"));
            loaded
        };
        let mut quitting = loaded();
        ch(&mut quitting, 'q');
        quitting.on_paste("q");
        assert!(!quitting.should_quit());
        ch(&mut quitting, 'q');
        assert!(quitting.should_quit(), "the exit question was still open");

        let mut helped = loaded();
        ch(&mut helped, '?');
        assert!(helped.is_help_open());
        helped.on_paste("tekst");
        assert!(helped.is_help_open());
    }

    #[test]
    fn a_paste_does_not_answer_a_confirmation_window() {
        let failed = waiting_on(
            task("a", "local", "interrupted", 1),
            AttentionKind::ProvisioningFailed,
            "provisioning-1",
            None,
        );
        let mut app = opened(failed);
        ch(&mut app, '3');
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
        assert!(app
            .overlay()
            .and_then(|overlay| overlay.confirmation())
            .is_some());
        app.on_paste("y\n");
        assert!(
            app.overlay()
                .and_then(|overlay| overlay.confirmation())
                .is_some(),
            "the window waits for its own y / n"
        );
        ch(&mut app, 'n');
        assert!(app.overlay().is_none());
    }

    #[test]
    fn an_answer_composer_keeps_its_draft_across_refresh_and_waiting_return() {
        let waiting = || with_pending(running("a", 1), question(None));
        let mut app = opened(waiting());
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "pierwsza linia");
        app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        typed(&mut app, "druga linia");
        let answer = "pierwsza linia\ndruga linia";

        app.load(listing(vec![titled(waiting(), "NAR-12")], "refresh"));
        assert_eq!(composer_of(&app).fields_of(0)[0].value(), answer);

        app.load(listing(vec![titled(running("a", 1), "NAR-12")], "gone"));
        let panel = app.detail().view("a").unwrap().waiting();
        assert!(panel
            .released()
            .iter()
            .any(|draft| draft.text.contains(answer)));

        app.load(listing(vec![titled(waiting(), "NAR-12")], "return"));
        assert_eq!(composer_of(&app).fields_of(0)[0].value(), answer);
        let draft = app.detail().draft("a").expect("the answer draft");
        assert_eq!(draft.matches(answer).count(), 1, "{draft}");
    }

    #[test]
    fn an_approval_opens_on_accept_and_returns_to_the_agent_on_request() {
        // 9c6af7f / 4ca1e73: accepting is explicit, returning needs a comment.
        let mut app = opened(with_pending(running("a", 1), approval(true)));
        let labels: Vec<_> = chooser_of(&app)
            .items()
            .iter()
            .map(|item| item.label.clone())
            .collect();
        assert_eq!(labels, ["Akceptuj", "Zwróć agentowi"]);
        assert_eq!(
            selected_label(&app),
            "Akceptuj",
            "the first entry is selected"
        );
        ch(&mut app, '2');
        assert_eq!(selected_label(&app), "Zwróć agentowi");

        assert_eq!(
            press(&mut app, KeyCode::Enter),
            Request::None,
            "the comment comes first"
        );
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            Request::None,
            "and is required"
        );
        typed(&mut app, "popraw testy");
        let delivery = delivery(press(&mut app, KeyCode::Enter));
        assert_eq!(delivery.action, Some(HumanInputAction::Resume));
        assert_eq!(delivery.comment.as_deref(), Some("popraw testy"));

        let mut app = opened(with_pending(running("a", 1), approval(true)));
        ch(&mut app, '1');
        let request = sent(press(&mut app, KeyCode::Enter));
        assert_eq!(request.summary, "zaakceptowano reviewed");
        let Submit::Input { delivery, .. } = request.submit else {
            panic!("an input")
        };
        assert_eq!(delivery.action, Some(HumanInputAction::Approve));
        assert_eq!(delivery.episode.as_deref(), Some("ep-a"));
    }

    #[test]
    fn an_approval_whose_session_cannot_be_resumed_offers_a_new_one() {
        let app = opened(with_pending(running("a", 1), approval(false)));
        let item = &chooser_of(&app).items()[1];
        assert_eq!(item.label, "Nowa sesja");
        assert!(item.description.contains("nie da się wznowić"));
    }

    #[test]
    fn a_problem_keeps_the_reply_composer_separate_from_the_actions() {
        let mut app = opened(with_pending(running("a", 1), problem(false)));
        // The reply has no entry among the actions: Down from the last one
        // leads to the composer (Z3).
        let labels: Vec<_> = chooser_of(&app)
            .items()
            .iter()
            .map(|item| item.label.clone())
            .collect();
        assert!(
            !labels.iter().any(|label| label == "Napisz do agenta"),
            "{labels:?}"
        );
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        assert_eq!(composer_of(&app).focus(), ChooserFocus::Fields);
        typed(&mut app, "spróbuj jeszcze raz");
        press(&mut app, KeyCode::Tab);
        assert!(!app.panel_focused());
        assert!(app
            .detail()
            .view("a")
            .unwrap()
            .waiting()
            .draft_text()
            .contains("spróbuj jeszcze raz"));
        press(&mut app, KeyCode::BackTab);
        assert!(app.panel_focused());
        let delivery = delivery(press(&mut app, KeyCode::Enter));
        assert_eq!(
            delivery.agent_message.as_deref(),
            Some("spróbuj jeszcze raz")
        );
        assert!(delivery.chosen_label.is_none());
    }

    #[test]
    fn an_entry_limit_panel_shows_counts_and_offers_continue_reset_and_outputs() {
        let pending: narsor_protocol::api::PendingHumanInputView =
            serde_json::from_value(entry_limit()).expect("entry limit view");
        let panel = narsor::tui::input_form::InputPanel::from_definition(
            &narsor_protocol::panel::define(&pending, &|_| None),
            &|_| None,
        );
        assert_eq!(panel.kind(), narsor::tui::input_form::InputKind::EntryLimit);
        assert!(panel.context().iter().any(|line| line.text.contains("5/5")));
        let labels: Vec<_> = panel
            .items()
            .iter()
            .map(|item| item.label.as_str())
            .collect();
        assert_eq!(
            labels,
            ["clean", "findings", "Kontynuuj", "Wyzeruj licznik"]
        );
        assert!(
            panel.items()[3]
                .description
                .contains("Limit przestaje chronić przed pętlą, aż licznik znów dojdzie do 5."),
            "{}",
            panel.items()[3].description
        );
        let continue_delivery = panel
            .delivery(&narsor::tui::chooser::Submission {
                index: 2,
                label: "Kontynuuj".to_string(),
                values: Vec::new(),
            })
            .unwrap();
        assert_eq!(continue_delivery.values["entry_limit_decision"], "continue");
        assert_eq!(continue_delivery.episode.as_deref(), Some("ep-limit"));
        let output_delivery = panel
            .delivery(&narsor::tui::chooser::Submission {
                index: 1,
                label: "findings".to_string(),
                values: vec![("variables.manual_note".to_string(), "triage".to_string())],
            })
            .unwrap();
        assert_eq!(output_delivery.chosen_label.as_deref(), Some("findings"));
        assert_eq!(output_delivery.variables["manual_note"], "triage");
    }

    #[test]
    fn a_problem_offers_the_conductors_label_and_values_marked_until_edited() {
        let mut app = opened(with_pending(running("a", 1), problem(true)));
        assert_eq!(
            selected_label(&app),
            "implemented",
            "the recommendation is selected"
        );
        let item = chooser_of(&app).selected_item().unwrap();
        assert!(item.recommended);
        let summary = &item.fields[0];
        assert_eq!(summary.name, "variables.summary");
        assert!(summary.is_proposed(), "◆ until the operator changes it");
        assert_eq!(summary.value(), "all units done");

        // Untouched, a proposal goes out as it was: the number stays a number.
        let delivery = delivery(enter_until_answer(&mut app));
        assert_eq!(delivery.chosen_label.as_deref(), Some("implemented"));
        assert_eq!(delivery.variables["summary"], "all units done");
        assert_eq!(delivery.variables["files"], json!(3));

        // Edited, the value is the operator's and the mark is gone.
        let mut app = opened(with_pending(running("a", 1), problem(true)));
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "!");
        let field = &chooser_of(&app).selected_item().unwrap().fields[0];
        assert!(!field.is_proposed());
        assert_eq!(field.value(), "all units done!");
    }

    #[test]
    fn a_problem_label_needing_a_key_is_refused_until_it_is_given() {
        let mut app = opened(with_pending(running("a", 1), problem(false)));
        ch(&mut app, '2');
        assert_eq!(selected_label(&app), "needs_human_decision");
        assert!(
            enter_until_error(&mut app),
            "artifacts.decision_doc is required"
        );
        assert_eq!(chooser_of(&app).focus(), ChooserFocus::Fields);
        let field = &chooser_of(&app).fields_of(1)[0];
        assert_eq!(field.name, "artifacts.decision_doc");
        assert!(field
            .error()
            .is_some_and(|error| error.contains("artifacts.decision_doc")));

        // The required artifact, a field for more variables and the comment:
        // no field for artifacts the step does not require.
        let names: Vec<&str> = chooser_of(&app)
            .fields_of(1)
            .iter()
            .map(|field| field.name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "artifacts.decision_doc",
                "variables (klucz=wartość)",
                "komentarz"
            ]
        );

        typed(&mut app, "docs/decision.md");
        // A key of the operator's own, beside the ones the label needs.
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        typed(&mut app, "scope=account");
        let delivery = delivery(enter_until_answer(&mut app));
        assert_eq!(
            delivery.chosen_label.as_deref(),
            Some("needs_human_decision")
        );
        assert_eq!(delivery.artifacts["decision_doc"], "docs/decision.md");
        assert_eq!(delivery.variables["scope"], "account");
    }

    #[test]
    fn a_key_given_twice_or_without_a_value_is_refused_at_the_panel() {
        let mut app = opened(with_pending(running("a", 1), problem(false)));
        ch(&mut app, '1');
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "x");
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        typed(&mut app, "summary=again");
        assert!(enter_until_error(&mut app), "duplicate keys are refused");
        let error = app
            .detail()
            .view("a")
            .unwrap()
            .waiting()
            .error()
            .expect("refused")
            .to_string();
        assert!(error.contains("variables.summary"), "{error}");

        // A line without `=` is refused too.
        let mut app = opened(with_pending(running("a", 1), problem(false)));
        ch(&mut app, '1');
        press(&mut app, KeyCode::Enter);
        // A multi-line field lets go on the second arrow in a row.
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        typed(&mut app, "bez znaku");
        assert!(enter_until_error(&mut app), "a key without '=' is refused");
        assert!(app.detail().view("a").unwrap().waiting().error().is_some());
    }

    #[test]
    fn the_conductors_summary_of_a_gate_recommends_an_output_with_the_diamond() {
        let pending: narsor_protocol::api::PendingHumanInputView =
            serde_json::from_value(gate("ep-1")).unwrap();
        let mut panel = narsor::tui::input_form::InputPanel::from_definition(
            &narsor_protocol::panel::define(&pending, &|_| None),
            &|_| None,
        );
        let annotation: narsor::api::AnnotationDto = serde_json::from_value(json!({
            "key": "ep-1", "summary": "brak planu migracji", "recommendation": "reject",
            "created_at": "2026-10-09T12:00:00Z"
        }))
        .unwrap();
        panel.annotate(&annotation);
        assert!(panel.items()[2].recommended);
        assert_eq!(panel.items()[2].label, "reject");
        assert!(panel
            .context()
            .iter()
            .any(|line| line.text.contains("◆ dyrygent: brak planu migracji")));

        // A summary of another request changes nothing.
        let mut other = narsor::tui::input_form::InputPanel::from_definition(
            &narsor_protocol::panel::define(&pending, &|_| None),
            &|_| None,
        );
        let mut stranger = annotation.clone();
        stranger.key = "ep-2".to_string();
        other.annotate(&stranger);
        assert!(other.items().iter().all(|item| !item.recommended));
    }

    #[test]
    fn permission_panel_offers_only_scoped_choices_for_resolvable_bash_denials() {
        let pending = |denials: serde_json::Value, session: Option<&str>| {
            serde_json::from_value::<narsor_protocol::api::PendingHumanInputView>(json!({
                "execution_id": "exec-1",
                "state_id": "implement",
                "action_execution_id": "act-8",
                "instruction": "Claude Code needs permission.",
                "fields": [],
                "continuation": "resolve_permission",
                "permission": {"denials": denials},
                "agent_harness": "claude-code",
                "agent_session_id": session,
                "episode": "ep-8",
                "requested_at": "2026-10-09T11:54:00Z"
            }))
            .unwrap()
        };
        let denial = |id: &str, command: &str, reason_type: &str| {
            json!({
                "tool_name": "Bash",
                "tool_use_id": id,
                "input": {"command": command},
                "cwd": "/tmp/task",
                "reason_type": reason_type,
                "reason": "This command requires approval."
            })
        };
        let labels = |panel: &narsor::tui::input_form::InputPanel| -> Vec<String> {
            panel
                .items()
                .iter()
                .map(|item| item.label.clone())
                .collect()
        };
        let decision = |panel: &narsor::tui::input_form::InputPanel, index: usize| {
            panel
                .delivery(&narsor::tui::chooser::Submission {
                    index,
                    label: panel.items()[index].label.clone(),
                    values: Vec::new(),
                })
                .unwrap()
        };

        // Compound commands are granted part by part, all denials at once,
        // and there is one refusal for all of them.
        let compound = pending(
            json!([
                denial("t1", "cargo build --release && echo OK", "asyncAgent"),
                denial("t2", "cargo test 2>&1 | sort", "asyncAgent"),
            ]),
            Some("session-8"),
        );
        let panel = narsor::tui::input_form::InputPanel::from_definition(
            &narsor_protocol::panel::define(&compound, &|_| None),
            &|_| None,
        );
        assert_eq!(
            labels(&panel),
            [
                "Odmów wszystkim",
                "Zezwól dokładnie na te polecenia",
                "Zezwól na grupę: Bash(cargo build *), Bash(echo OK *), Bash(cargo test *), Bash(sort)",
                "Zezwól według wzorca…",
            ]
        );
        assert!(panel.items()[1]
            .description
            .contains("Bash(cargo build --release), Bash(echo OK), Bash(cargo test), Bash(sort)"));
        let delivery = decision(&panel, 1);
        let granted = delivery.permission_decision.unwrap();
        assert_eq!(granted.denial_index, None, "every denial");
        assert_eq!(
            granted.scope,
            Some(narsor_protocol::api::PermissionGrantScopeDto::ExactOperation)
        );
        assert_eq!(delivery.episode.as_deref(), Some("ep-8"));
        let refused = decision(&panel, 0).permission_decision.unwrap();
        assert_eq!(refused.denial_index, None);

        // A policy block is never granted; the grantable denial still is,
        // and the panel says why the other one is not.
        let mixed = pending(
            json!([
                denial("t1", "cargo test", "asyncAgent"),
                denial("t2", "rm -rf /", "rule"),
            ]),
            Some("session-8"),
        );
        let panel = narsor::tui::input_form::InputPanel::from_definition(
            &narsor_protocol::panel::define(&mixed, &|_| None),
            &|_| None,
        );
        assert_eq!(
            labels(&panel),
            ["Odmów wszystkim", "Zezwól dokładnie: cargo test"]
        );
        assert_eq!(
            decision(&panel, 1)
                .permission_decision
                .unwrap()
                .denial_index,
            Some(0)
        );
        assert!(panel
            .context()
            .iter()
            .any(|line| line.text.contains("zgoda niedostępna: blokada polityki")));

        // Without a session to resume, a grant could not reach the agent.
        let unresumable = pending(json!([denial("t1", "cargo test", "asyncAgent")]), None);
        let panel = narsor::tui::input_form::InputPanel::from_definition(
            &narsor_protocol::panel::define(&unresumable, &|_| None),
            &|_| None,
        );
        assert_eq!(labels(&panel), ["Odmów"]);

        // A shell script no rule can pin down leaves only a refusal: it says
        // the agent will be asked for simple commands, and carries the
        // operator's message to the agent.
        let script = pending(
            json!([denial(
                "t1",
                "f=out.log; until grep -q done \"$f\"; do sleep 2; done",
                "asyncAgent"
            )]),
            Some("session-8"),
        );
        let panel = narsor::tui::input_form::InputPanel::from_definition(
            &narsor_protocol::panel::define(&script, &|_| None),
            &|_| None,
        );
        assert_eq!(labels(&panel), ["Odmów", "Zezwól według wzorca…"]);
        assert!(panel.items()[0].description.contains("proste polecenia"));
        let delivery = panel
            .delivery(&narsor::tui::chooser::Submission {
                index: 0,
                label: "Odmów".to_string(),
                values: vec![(
                    narsor::tui::input_form::MESSAGE_FIELD.to_string(),
                    "  Uruchom cargo build --workspace --bins.  ".to_string(),
                )],
            })
            .unwrap();
        let refused = delivery.permission_decision.unwrap();
        assert_eq!(
            refused.decision,
            narsor_protocol::api::PermissionDecisionKindDto::Deny
        );
        assert_eq!(
            refused.message.as_deref(),
            Some("Uruchom cargo build --workspace --bins.")
        );
        assert_eq!(
            decision(&panel, 0).permission_decision.unwrap().message,
            None
        );

        // The operator's own pattern starts as the denied command and is
        // sent as typed; `*` allows every command, and a pattern no rule
        // can carry is refused before it is sent.
        let typed = &panel.items()[1];
        assert_eq!(typed.fields[0].name, narsor::tui::input_form::PATTERN_FIELD);
        assert!(typed.fields[0].required);
        assert!(typed.fields[0]
            .description
            .as_deref()
            .unwrap()
            .contains("KAŻDE"));
        let with_pattern = |pattern: &str| {
            panel.delivery(&narsor::tui::chooser::Submission {
                index: 1,
                label: typed.label.clone(),
                values: vec![(
                    narsor::tui::input_form::PATTERN_FIELD.to_string(),
                    pattern.to_string(),
                )],
            })
        };
        let granted = with_pattern(" * ").unwrap().permission_decision.unwrap();
        assert_eq!(
            granted.scope,
            Some(narsor_protocol::api::PermissionGrantScopeDto::Pattern)
        );
        assert_eq!(granted.pattern.as_deref(), Some("*"));
        assert!(with_pattern("echo (x)").is_err());
        assert!(with_pattern("  ").is_err());

        // A policy block is never lifted, so no pattern is offered for it.
        let blocked = pending(json!([denial("t1", "rm -rf /", "rule")]), Some("session-8"));
        let panel = narsor::tui::input_form::InputPanel::from_definition(
            &narsor_protocol::panel::define(&blocked, &|_| None),
            &|_| None,
        );
        assert_eq!(labels(&panel), ["Odmów"]);
    }

    // -- the panel ------------------------------------------------------------

    #[test]
    fn a_failed_step_confirms_the_conductors_proposal() {
        let failed = waiting_on(
            running("a", 1),
            AttentionKind::FailedStep,
            "fk1",
            Some("test"),
        );
        let mut app = opened(failed);
        read_conductor(&mut app, conductor(true));
        let labels: Vec<String> = chooser_of(&app)
            .items()
            .iter()
            .map(|item| item.label.clone())
            .collect();
        assert!(
            labels[1].starts_with("Potwierdź: Uruchom krok"),
            "{labels:?}"
        );
        assert!(labels[2].starts_with("Odrzuć: Uruchom krok"));
        assert_eq!(labels[3], "Restart kroku z notatką");
        assert!(chooser_of(&app).items()[1].recommended, "◆ the conductor's");
        assert_eq!(
            chooser_of(&app).selected(),
            1,
            "the recommendation is selected"
        );

        let request = sent(press(&mut app, KeyCode::Enter));
        assert_eq!(
            request.submit,
            Submit::Operator(OperatorCommand::ConfirmProposal {
                proposal_id: "p1".to_string()
            })
        );
        assert_eq!(request.generation, 1);
        assert!(!request.key.is_empty());
    }

    #[test]
    fn an_optional_waiting_action_note_opens_with_enter() {
        let failed = waiting_on(
            running("a", 1),
            AttentionKind::FailedStep,
            "fk1",
            Some("test"),
        );
        let mut app = opened(failed);
        read_conductor(&mut app, conductor(true));
        ch(&mut app, '4');

        assert_eq!(chooser_of(&app).selected(), 3);
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
        assert_eq!(chooser_of(&app).focus(), ChooserFocus::Fields);
        typed(&mut app, "powód restartu");
        assert_eq!(chooser_of(&app).fields_of(3)[0].value(), "powód restartu");
        assert_eq!(
            sent(press(&mut app, KeyCode::Enter)).submit,
            Submit::Operator(OperatorCommand::RestartStep {
                note: Some("powód restartu".to_string()),
                failure_key: Some("fk1".to_string()),
            })
        );
    }

    #[test]
    fn a_failed_step_rejects_or_restarts_with_a_note_naming_its_failure() {
        let failed = waiting_on(
            running("a", 1),
            AttentionKind::FailedStep,
            "fk1",
            Some("test"),
        );
        let mut app = opened(failed);
        read_conductor(&mut app, conductor(true));
        ch(&mut app, '3');
        assert_eq!(
            sent(press(&mut app, KeyCode::Enter)).submit,
            Submit::Operator(OperatorCommand::RejectProposal {
                proposal_id: "p1".to_string()
            })
        );

        ch(&mut app, '4');
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "użyj nowszego toolchaina");
        assert_eq!(
            sent(press(&mut app, KeyCode::Enter)).submit,
            Submit::Operator(OperatorCommand::RestartStep {
                note: Some("użyj nowszego toolchaina".to_string()),
                failure_key: Some("fk1".to_string()),
            })
        );
    }

    #[test]
    fn a_failed_step_without_a_conductor_shows_the_raw_error_and_restarts() {
        let failed = waiting_on(
            running("a", 1),
            AttentionKind::FailedStep,
            "fk1",
            Some("test"),
        );
        let mut app = opened(failed);
        let mut failing = position("test", PositionStatus::Failed);
        failing.error = Some("cargo test: exit 101".to_string());
        app.on_detail_read(
            "a",
            1,
            DetailRead::Timeline(Ok(timeline("a", vec![failing], None))),
        );
        read_conductor_response(
            &mut app,
            Ok(narsor::controller_protocol::ConductorReadResponse::Unavailable {
                reason:
                    narsor_engine::controller_protocol::ConversationUnavailableReason::RunnerUnreachable,
                message: "brak harnessu".to_string(),
            }),
        );
        let text = screen(&mut app, 120, 40);
        assert!(
            text.contains("dyrygent niedostępny: brak harnessu"),
            "{text}"
        );
        // The raw error comes from the timeline once the entry is built again.
        assert_eq!(
            chooser_of(&app).items().len(),
            2,
            "action 0 and the restart"
        );
        let request = sent(enter_until_answer(&mut app));
        assert_eq!(
            request.submit,
            Submit::Operator(OperatorCommand::RestartStep {
                note: None,
                failure_key: Some("fk1".to_string())
            })
        );
    }

    #[test]
    fn a_proposal_from_the_chat_is_confirmed_or_rejected() {
        let waiting = waiting_on(
            running("a", 1),
            AttentionKind::ConductorProposal,
            "p1",
            Some("test"),
        );
        let mut app = opened(waiting);
        read_conductor(&mut app, conductor(false));
        let text = screen(&mut app, 120, 40);
        assert!(
            text.contains("dyrygent proponuje: Uruchom krok od nowa"),
            "{text}"
        );
        assert_eq!(
            sent(press(&mut app, KeyCode::Enter)).submit,
            Submit::Operator(OperatorCommand::ConfirmProposal {
                proposal_id: "p1".to_string()
            })
        );
        ch(&mut app, '3');
        assert_eq!(
            sent(press(&mut app, KeyCode::Enter)).submit,
            Submit::Operator(OperatorCommand::RejectProposal {
                proposal_id: "p1".to_string()
            })
        );
    }

    #[test]
    fn a_failed_provisioning_starts_again_or_archives_after_the_confirmation_window() {
        let failed = waiting_on(
            task("a", "local", "interrupted", 1),
            AttentionKind::ProvisioningFailed,
            "provisioning-1",
            None,
        );
        let mut app = opened(failed);
        assert_eq!(selected_label(&app), "Uruchom od nowa");
        assert_eq!(
            sent(press(&mut app, KeyCode::Enter)).submit,
            Submit::Owner(OwnerCommand::Start)
        );

        ch(&mut app, '3');
        assert!(chooser_of(&app).items()[2].irreversible);
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            Request::None,
            "archiving asks first, in the shared window"
        );
        let confirmation = app
            .overlay()
            .and_then(|overlay| overlay.confirmation())
            .expect("the confirmation window");
        assert!(confirmation.heading().contains("Archiwizuj"));
        let text = screen(&mut app, 120, 40);
        assert!(
            text.contains("Usuwa workload i storage") && text.contains("Tego nie da się cofnąć"),
            "{text}"
        );
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            Request::None,
            "Enter is not the answer to a y / n window"
        );
        assert!(app.overlay().is_some());
        ch(&mut app, 'n');
        assert!(app.overlay().is_none());
        assert!(
            app.panel_focused(),
            "n took the question back, it did not fold the panel"
        );
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None, "asks again");
        assert_eq!(
            sent(ch(&mut app, 'y')).submit,
            Submit::Owner(OwnerCommand::Archive)
        );
    }

    #[test]
    fn two_waitings_are_counted_and_escape_steps_between_them() {
        let both = with_pending(with_pending(running("a", 1), gate("ep-1")), question(None));
        let mut app = opened(both);
        let waiting = |app: &TaskApp| app.detail().view("a").unwrap().waiting().current_index();
        assert_eq!(app.detail().view("a").unwrap().waiting().len(), 2);
        assert_eq!(waiting(&app), 0);
        let text = screen(&mut app, 120, 40);
        assert!(text.contains("1/2"), "{text}");
        ch(&mut app, ']');
        assert_eq!(waiting(&app), 0, "the bracket keys do not step the forms");
        press(&mut app, KeyCode::Esc);
        assert_eq!(waiting(&app), 1);
        assert!(screen(&mut app, 120, 40).contains("2/2"));
        press(&mut app, KeyCode::Esc);
        assert_eq!(waiting(&app), 0, "the forms go round");
    }

    #[test]
    fn a_refresh_while_typing_keeps_the_draft_and_the_place() {
        let mut app = gate_app();
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "Uwzględnij");
        assert!(app.detail().has_drafts());
        let waiting = with_pending(running("a", 1), gate("ep-1"));
        app.load(listing(vec![titled(waiting.clone(), "NAR-12")], "d"));
        app.on_stream(event(
            "e",
            "a",
            2,
            RunnerState::Running,
            TaskEventKind::Created,
        ));
        app.load(listing(vec![titled(waiting, "NAR-12")], "e"));
        assert!(app.panel_focused());
        assert_eq!(chooser_of(&app).focus(), ChooserFocus::Fields);
        assert_eq!(chooser_of(&app).fields_of(1)[0].value(), "Uwzględnij");
        assert!(app
            .detail()
            .draft("a")
            .is_some_and(|draft| draft.contains("Uwzględnij")));
        typed(&mut app, " migrację");
        assert_eq!(
            chooser_of(&app).fields_of(1)[0].value(),
            "Uwzględnij migrację"
        );
    }

    #[test]
    fn leaving_the_panel_with_tab_and_coming_back_brings_the_draft_back() {
        let mut app = gate_app();
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "szkic");
        press(&mut app, KeyCode::Esc);
        assert!(app.panel_focused(), "the first Esc returns to the actions");
        press(&mut app, KeyCode::Esc);
        assert!(app.panel_focused(), "Esc with one form does not fold it");
        assert!(!app.detail().view("a").unwrap().waiting().is_collapsed());
        press(&mut app, KeyCode::Tab);
        assert!(!app.panel_focused());
        assert!(!app.detail().view("a").unwrap().waiting().is_collapsed());
        assert!(app.detail().has_drafts(), "the draft stays");

        press(&mut app, KeyCode::Tab);
        assert!(app.panel_focused());
        assert_eq!(chooser_of(&app).fields_of(1)[0].value(), "szkic");
    }

    #[test]
    fn a_waiting_that_goes_away_while_typing_folds_the_panel_and_keeps_the_draft() {
        let mut app = gate_app();
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "moja odpowiedź");
        // Answered from the CLI: the row no longer waits.
        app.load(listing(vec![titled(running("a", 1), "NAR-12")], "d"));
        assert!(!app.panel_focused(), "the focus left the panel");
        let panel = app.detail().view("a").unwrap().waiting();
        assert!(panel.is_empty());
        assert_eq!(panel.released().len(), 1);
        assert!(panel.released()[0].text.contains("moja odpowiedź"));
        assert!(app.detail().has_drafts());
        let text = screen(&mut app, 120, 40);
        assert!(
            text.contains("moja odpowiedź") && text.contains("skopiuj"),
            "{text}"
        );
        assert!(app
            .notifier()
            .latest()
            .unwrap()
            .text
            .contains("oczekiwanie zniknęło"));

        // The box is dismissed on purpose.
        press(&mut app, KeyCode::Tab);
        assert!(app.panel_focused());
        press(&mut app, KeyCode::Esc);
        assert!(!app.detail().has_drafts());
        assert!(!app.detail().view("a").unwrap().waiting().is_present());
    }

    /// R8: the waiting comes back under the same id — a task resumed after
    /// its runner was lost asks again — and the draft goes back to it: no
    /// copy is left behind and the focus is not taken.
    #[test]
    fn a_draft_goes_back_to_its_waiting_when_the_same_one_returns() {
        let mut app = gate_app();
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "moja odpowiedź");
        app.load(listing(vec![titled(running("a", 1), "NAR-12")], "d"));
        assert_eq!(
            app.detail().view("a").unwrap().waiting().released().len(),
            1
        );
        assert!(!app.panel_focused());

        let back = with_pending(running("a", 1), gate("ep-1"));
        app.load(listing(vec![titled(back, "NAR-12")], "e"));
        let panel = app.detail().view("a").unwrap().waiting();
        assert!(panel.released().is_empty(), "{:?}", panel.released());
        assert_eq!(panel.len(), 1);
        assert_eq!(panel.current_waiting_id(), Some("ep-1"));
        assert_eq!(chooser_of(&app).fields_of(1)[0].value(), "moja odpowiedź");
        let draft = app.detail().draft("a").expect("the draft");
        assert_eq!(draft.matches("moja odpowiedź").count(), 1, "{draft}");
        assert!(!app.panel_focused(), "the focus stays where it was");
        assert_eq!(app.focus(), Focus::Details);
    }

    /// The draft of a waiting that returns among others goes back to its
    /// own entry, in the panel's order. The server's default form is shown
    /// (R14), and it is the one that returned.
    #[test]
    fn a_returning_waiting_takes_its_place_and_gets_its_draft_back() {
        let mut app = gate_app();
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "pierwsza");
        app.load(listing(vec![titled(running("a", 1), "NAR-12")], "d"));
        let other = with_pending(running("a", 1), question(None));
        app.load(listing(vec![titled(other, "NAR-12")], "e"));
        assert_eq!(
            app.detail()
                .view("a")
                .unwrap()
                .waiting()
                .current_waiting_id(),
            Some("ep-q")
        );

        let both = with_pending(with_pending(running("a", 1), gate("ep-1")), question(None));
        app.load(listing(vec![titled(both, "NAR-12")], "f"));
        let panel = app.detail().view("a").unwrap().waiting();
        assert!(panel.released().is_empty());
        assert_eq!(panel.len(), 2);
        assert_eq!(panel.current_index(), 0, "the default form is shown");
        assert_eq!(panel.current_waiting_id(), Some("ep-1"));
        assert_eq!(chooser_of(&app).fields_of(1)[0].value(), "pierwsza");
        assert!(app
            .detail()
            .draft("a")
            .is_some_and(|draft| draft.matches("pierwsza").count() == 1));
    }

    /// Another waiting in its place does not take the draft: it stays to be
    /// copied.
    #[test]
    fn a_different_waiting_leaves_the_draft_where_it_was() {
        let mut app = gate_app();
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "moja odpowiedź");
        app.load(listing(vec![titled(running("a", 1), "NAR-12")], "d"));
        let other = with_pending(running("a", 1), gate("ep-2"));
        app.load(listing(vec![titled(other, "NAR-12")], "e"));
        let panel = app.detail().view("a").unwrap().waiting();
        assert_eq!(panel.released().len(), 1);
        assert!(panel.released()[0].text.contains("moja odpowiedź"));
        assert_eq!(panel.current_waiting_id(), Some("ep-2"));
        assert_eq!(chooser_of(&app).draft_text(), "");
    }

    #[test]
    fn sending_without_a_connection_is_blocked_with_a_reason_and_keeps_the_draft() {
        let mut app = gate_app();
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "tekst");
        app.on_stream(Some(Err(ClientError::Unavailable("down".to_string()))));
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
        let note = app.notifier().latest().expect("a reason").clone();
        assert!(note.text.contains("zablokowana"), "{}", note.text);
        assert_eq!(note.severity, Severity::Warning);
        assert_eq!(chooser_of(&app).fields_of(1)[0].value(), "tekst");
    }

    #[test]
    fn quitting_always_requires_a_second_q_and_escape_cancels() {
        let mut app = gate_app();
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "q?");
        assert_eq!(
            chooser_of(&app).fields_of(1)[0].value(),
            "q?",
            "q and ? are text while typing"
        );
        assert!(!app.should_quit());
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Esc);
        ch(&mut app, 'q');
        assert!(!app.should_quit(), "the first q opens confirmation");
        ch(&mut app, 'j');
        assert!(!app.should_quit(), "other keys leave the dialog open");
        press(&mut app, KeyCode::Esc);
        assert!(!app.should_quit(), "Esc cancels quitting");
        ch(&mut app, 'q');
        assert!(!app.should_quit(), "q must be pressed again");
        app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(!app.should_quit(), "modified q does not confirm quitting");
        ch(&mut app, 'q');
        assert!(app.should_quit());

        let mut app = gate_app();
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('?'));
        assert!(app.is_help_open());
        ch(&mut app, 'q');
        assert!(
            !app.should_quit(),
            "q from help opens the exit confirmation"
        );
        assert!(!app.is_help_open());
        press(&mut app, KeyCode::Esc);
        assert!(!app.should_quit());

        let mut app = gate_app();
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Esc);
        ch(&mut app, 'q');
        assert!(
            !app.should_quit(),
            "confirmation is required without a draft"
        );
        ch(&mut app, 'q');
        assert!(app.should_quit());
    }

    #[test]
    fn tab_in_the_details_goes_to_the_panel_and_a_task_that_waits_on_nothing_says_so() {
        let mut app = app();
        app.load(listing(vec![titled(running("a", 1), "NAR-12")], "c"));
        press(&mut app, KeyCode::Enter);
        assert!(!app.panel_focused());
        press(&mut app, KeyCode::Tab);
        assert!(!app.panel_focused());
        assert!(app
            .notifier()
            .latest()
            .unwrap()
            .text
            .contains("na nic nie czeka"));

        let mut app = gate_app();
        press(&mut app, KeyCode::Tab);
        assert!(!app.panel_focused() && app.focus() == Focus::Details);
        press(&mut app, KeyCode::Tab);
        assert!(app.panel_focused());
    }

    /// The timeline of `id` at `revision` with one running step,
    /// `review_scope`, whose conversation has two entries read.
    fn read_step_conversation(app: &mut TaskApp, id: &str, revision: u64) {
        use narsor::tui::task_app::tabs::conversation::AttemptRead;
        use narsor::tui::transcript::{TranscriptEntry, TranscriptEntryId};
        use narsor_engine::engine::ConversationState;
        let mut step = position("review_scope", PositionStatus::InProgress);
        step.conversation = ConversationState::Recorded;
        app.on_detail_read(
            id,
            revision,
            DetailRead::Timeline(Ok(timeline(id, vec![step], Some("review_scope #1")))),
        );
        let entries = ["1", "2"]
            .into_iter()
            .map(|entry| {
                TranscriptEntry::new(
                    TranscriptEntryId::from(entry),
                    vec![format!("line {entry}")],
                    Vec::new(),
                )
            })
            .collect();
        app.on_detail_read(
            id,
            revision,
            DetailRead::Attempt {
                attempt_id: "act-review_scope".to_string(),
                from: 0,
                result: Ok(AttemptRead {
                    entries,
                    next_entry: 2,
                    closed: false,
                    truncated_tail: false,
                    complete: true,
                    no_record: false,
                    source: narsor_engine::controller_protocol::ExecutionEventsSource::Runner,
                    recorded_at: None,
                }),
            },
        );
    }

    #[test]
    fn the_steps_conversation_keeps_its_focus_through_the_panel_and_the_menu() {
        use narsor::tui::keymap::Context;
        use narsor::tui::task_app::StepFocus;
        let mut app = gate_app();
        press(&mut app, KeyCode::Tab);
        assert!(!app.panel_focused() && app.focus() == Focus::Details);
        app.detail_mut()
            .set_tab(narsor::tui::task_app::DetailTab::Progress);
        read_step_conversation(&mut app, "a", 1);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.step_focus(), StepFocus::Conversation);

        press(&mut app, KeyCode::Tab);
        assert!(app.panel_focused());
        assert_eq!(app.step_focus(), StepFocus::Conversation);
        press(&mut app, KeyCode::Tab);
        assert!(!app.panel_focused());
        assert_eq!(app.key_context().layer, Some(Context::StepConversation));

        ch(&mut app, '.');
        assert_eq!(app.key_context().base, Context::Menu);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.key_context().layer, Some(Context::StepConversation));
        assert_eq!(app.step_focus(), StepFocus::Conversation);
    }

    #[test]
    fn untyped_keys_of_the_panel_are_the_keymaps() {
        let mut app = gate_app();
        assert_eq!(app.key_context().base, narsor::tui::keymap::Context::Panel);
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.key_context().base,
            narsor::tui::keymap::Context::Form,
            "typing inherits no global key"
        );
        ch(&mut app, '?');
        assert!(!app.is_help_open());
        press(&mut app, KeyCode::Tab);
        ch(&mut app, '?');
        assert!(app.is_help_open());
    }

    fn conductor(with_failure: bool) -> ConductorView {
        let proposal = json!({
            "proposal_id": "p1",
            "text": "Uruchom krok od nowa",
            "at": "2026-10-09T12:00:00Z",
            "record": {
                "episode": if with_failure { "fk1" } else { "ep-x" },
                "state_id": "test",
                "proposal": { "kind": "retry", "summary": "Uruchom krok od nowa", "text": "z nowszym toolchainem" },
                "confidence": "high",
                "rationale": "Błąd wygląda na przejściowy."
            },
            "state": "pending"
        });
        let mut view = json!({ "available": true, "harness": "claude", "proposals": [proposal] });
        if with_failure {
            view["failure"] = json!({
                "key": "fk1",
                "episode": "fk1",
                "state_id": "test",
                "reason": "cargo test nie przeszło",
                "outcome": { "kind": "diagnosed", "diagnosis": "Testy padają przez toolchain.", "proposals": [] },
                "concluded_at": "2026-10-09T12:00:00Z"
            });
        }
        serde_json::from_value(view).expect("a conductor view")
    }

    fn read_conductor(app: &mut TaskApp, view: ConductorView) {
        read_conductor_response(
            app,
            Ok(ConductorReadResponse::Available {
                conductor: Box::new(view),
                source: ExecutionEventsSource::Runner,
            }),
        );
    }

    fn read_conductor_response(
        app: &mut TaskApp,
        result: Result<ConductorReadResponse, ClientError>,
    ) {
        app.on_detail_read("a", 1, DetailRead::Conductor(result));
    }

    // -- against a served control plane -----------------------------------------
}

/// U11: the action menu, the confirmations and the commands of the operator.
mod task_app_actions {

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use narsor::api::{ApiClient, ClientError};
    use narsor::tui::keymap::{self, Action, Context};
    use narsor::tui::task_app::actions::{
        availability, refusal_reason, verb, ActionRequest, ActionSend, Availability, Confirmed,
        Overlay, TaskAction,
    };
    use narsor::tui::task_app::notify::Severity;
    use narsor::tui::task_app::{Request, TaskApp};
    use narsor_engine::controller_protocol::{OperatorCommandView, TaskSummary};
    use narsor_engine::domain::ExecutionStatus;
    use narsor_engine::orchestration::{
        AttentionKind, OperatorCommand, OperatorCommandStatus, OwnerCommand, OwnerRef, RunnerState,
        TaskEventKind,
    };

    use super::task_app_support::{
        event, header_workspace, listing, offered, queued, task, titled, waiting_on,
        with_controller, with_header_workspaces, with_pr, with_runner, with_transitions,
    };

    fn offline_client() -> ApiClient {
        ApiClient::new("127.0.0.1:9".parse().unwrap(), "token")
    }

    fn app() -> TaskApp {
        TaskApp::new(offline_client(), "local", None)
    }

    /// One key. An `Enter` that asks for the confirmation of a send is
    /// answered with the `Enter` of the window, so a test that says "Enter
    /// sends" still reads so; the window itself is tested with `on_key`.
    fn press(app: &mut TaskApp, code: KeyCode) -> Request {
        let request = app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
        if code == KeyCode::Enter && request == Request::None && app.form_confirming() {
            return app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        }
        request
    }

    fn ch(app: &mut TaskApp, character: char) -> Request {
        press(app, KeyCode::Char(character))
    }

    /// An action of the panel, the only way to it: `Ctrl+.` opens the panel,
    /// the digit of the entry picks it and `Enter` does it. A panel left open
    /// by a refusal is closed first.
    fn choose(app: &mut TaskApp, action: TaskAction) -> Request {
        if app.overlay().is_some() {
            press(app, KeyCode::Esc);
        }
        let opened = app.on_key(KeyEvent::new(KeyCode::Char('.'), KeyModifiers::CONTROL));
        assert_eq!(opened, Request::None);
        let index = TaskAction::ALL
            .iter()
            .position(|candidate| *candidate == action)
            .expect("an action of the panel");
        ch(app, char::from(b'1' + u8::try_from(index).unwrap()));
        press(app, KeyCode::Enter)
    }

    fn typed(app: &mut TaskApp, text: &str) {
        for character in text.chars() {
            ch(app, character);
        }
    }

    fn note(app: &TaskApp) -> String {
        app.notifier()
            .latest()
            .map(|note| note.text.clone())
            .unwrap_or_default()
    }

    /// A running task whose workspace `code` has a pull request.
    fn with_pull_request(task: TaskSummary) -> TaskSummary {
        with_header_workspaces(
            task,
            vec![with_pr(
                header_workspace("code", "orders-api", "feat/x"),
                "https://gitlab.example/g/api/-/merge_requests/42",
            )],
        )
    }

    fn running(id: &str, rank: u32) -> TaskSummary {
        with_runner(
            with_controller(task(id, "local", "running", rank)),
            ExecutionStatus::Running,
            "review",
        )
    }

    fn finished(id: &str, rank: u32) -> TaskSummary {
        with_runner(
            with_controller(task(id, "local", "running", rank)),
            ExecutionStatus::Completed,
            "done",
        )
    }

    fn runner_state(word: &str) -> TaskSummary {
        with_controller(task("a", "local", word, 1))
    }

    /// A title for the row, and nothing else: the actions the test put on the
    /// task stay as they are.
    fn titled_as_served(mut task: TaskSummary) -> TaskSummary {
        task.title = Some("NAR-12".to_string());
        task
    }

    fn loaded(task: TaskSummary) -> TaskApp {
        let mut app = app();
        app.load(listing(vec![titled(task, "NAR-12")], "c"));
        app
    }

    fn act(request: Request) -> ActionRequest {
        match request {
            Request::Act(request) => *request,
            other => panic!("an action was expected, got {other:?}"),
        }
    }

    fn confirmation(app: &TaskApp) -> &narsor::tui::task_app::actions::Confirmation {
        app.overlay()
            .and_then(Overlay::confirmation)
            .expect("the confirmation window")
    }

    fn unavailable(task: &TaskSummary, action: TaskAction) -> String {
        match availability(task, action) {
            Availability::Unavailable(reason) => reason,
            Availability::Available => panic!("{action:?} should not be available"),
        }
    }

    fn available(task: &TaskSummary, action: TaskAction) -> bool {
        availability(task, action).is_available()
    }

    fn screen(app: &mut TaskApp, width: u16, height: u16) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                narsor::tui::task_app::layout::render(
                    frame,
                    app,
                    &narsor::tui::style::Styles::new(false),
                    std::time::Instant::now(),
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer
            .content()
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    // -- the table of origin §3 --------------------------------------------

    mod comment {
        use super::*;
        use crate::task_app_support::with_pending;
        use serde_json::{json, Value};

        fn type_text(app: &mut TaskApp, text: &str) {
            for c in text.chars() {
                ch(app, c);
            }
        }

        #[test]
        fn a_paused_task_offers_a_comment_and_no_resume_in_the_panel() {
            let paused = runner_state("paused");
            assert!(available(&paused, TaskAction::Comment));
            assert!(!TaskAction::ALL.contains(&TaskAction::Comment));
            assert_eq!(verb(TaskAction::Comment), "komentarz");
        }

        #[test]
        fn a_running_task_says_why_it_takes_no_comment() {
            let reason = unavailable(&running("a", 1), TaskAction::Comment);
            assert!(reason.contains("zadanie działa"), "{reason}");
        }

        #[test]
        fn c_opens_the_field_and_enter_sends_one_comment_in_one_step() {
            let mut app = loaded(runner_state("paused"));
            assert_eq!(ch(&mut app, 'c'), Request::None);
            assert!(matches!(app.overlay(), Some(Overlay::Comment(_))));
            // Enter goes on to the field; the text is typed there.
            assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
            type_text(&mut app, "zacznij od testów");
            let request = act(press(&mut app, KeyCode::Enter));
            assert_eq!(
                request.send,
                ActionSend::Comment("zacznij od testów".to_string())
            );
            assert_eq!(request.verb, "komentarz");
            assert_eq!(request.summary, "komentarz wysłany");
            assert!(app.overlay().is_none(), "no window of confirmation");
        }

        fn stop_form(continuation: &str) -> Value {
            json!({
                "execution_id": "exec-1",
                "state_id": "review_scope",
                "action_execution_id": "act-7",
                "instruction": "Zatwierdź wynik",
                "outputs": [{ "label": "reviewed", "description": "Wynik przeglądu." }],
                "fields": [{ "name": "answer", "type": "text", "required": true }],
                "continuation": continuation,
                "approval": {
                    "action_execution_id": "act-7",
                    "result": { "output_label": "reviewed", "data": {}, "variables": {}, "exit_code": null, "stdout": "", "stderr": "" },
                    "agent_session_id": "sess-1"
                },
                "episode": "ep-1",
                "requested_at": "2026-10-09T11:54:00Z"
            })
        }

        /// What the server says a comment does at a stop the agent waits at.
        fn effect_at(continuation: &str) -> String {
            let task = with_pending(running("a", 1), stop_form(continuation));
            let offer = task
                .actions
                .as_ref()
                .and_then(|actions| {
                    actions.iter().find(|offer| {
                        offer.id == narsor_engine::controller_protocol::TaskActionId::Comment
                    })
                })
                .expect("a comment offer");
            assert!(offer.available);
            offer.effect.clone().expect("the effect")
        }

        #[test]
        fn the_words_of_a_comment_are_the_servers_for_each_stop() {
            assert!(effect_at("resume_agent").contains("w tej samej sesji"));
            assert!(effect_at("approve_output").contains("Nie zatwierdza"));
        }

        #[test]
        fn a_comment_on_a_question_and_on_an_approval_is_sent_as_a_comment() {
            for continuation in ["resume_agent", "approve_output"] {
                let mut app = loaded(with_pending(running("a", 1), stop_form(continuation)));
                assert_eq!(ch(&mut app, 'c'), Request::None, "{continuation}");
                assert!(matches!(app.overlay(), Some(Overlay::Comment(_))));
                press(&mut app, KeyCode::Enter);
                type_text(&mut app, "uwaga");
                let request = act(press(&mut app, KeyCode::Enter));
                assert_eq!(
                    request.send,
                    ActionSend::Comment("uwaga".to_string()),
                    "{continuation}"
                );
            }
        }

        #[test]
        fn a_blank_comment_is_not_sent() {
            let mut app = loaded(runner_state("paused"));
            ch(&mut app, 'c');
            let request = press(&mut app, KeyCode::Enter);
            assert_eq!(request, Request::None);
        }

        #[test]
        fn without_an_offer_there_is_no_comment_field() {
            let mut task = runner_state("paused");
            task.actions = None;
            let mut app = app();
            app.load(listing(vec![titled_as_served(task)], "c"));
            assert_eq!(ch(&mut app, 'c'), Request::None);
            assert!(app.overlay().is_none());
            assert!(note(&app).contains("brak danych"), "{}", note(&app));
        }

        #[test]
        fn the_panel_has_no_comment_entry_but_the_comment_field_says_its_effect() {
            let mut app = loaded(runner_state("paused"));
            ch(&mut app, '.');
            let chooser = app.overlay().and_then(Overlay::chooser).expect("the panel");
            assert!(
                chooser
                    .items()
                    .iter()
                    .all(|item| !item.label.contains("napisz do agenta")),
                "the conversation writes, not the panel"
            );
            press(&mut app, KeyCode::Esc);
            ch(&mut app, 'c');
            let form = app.overlay().and_then(Overlay::chooser).expect("the field");
            assert!(form.items()[0].description.contains("wznawia zadanie"));
        }
    }

    mod availability_table {
        use super::*;

        #[test]
        fn move_is_for_queued_tasks_only() {
            assert!(available(&queued("a", 1), TaskAction::Move));
            for word in ["running", "paused", "interrupted", "provisioning"] {
                assert_eq!(
                    unavailable(&runner_state(word), TaskAction::Move),
                    "zadanie nie jest w kolejce",
                    "{word}"
                );
            }
        }

        #[test]
        fn pause_for_a_running_task_and_nothing_else() {
            let running = running("a", 1);
            assert!(available(&running, TaskAction::Pause));
            assert_eq!(verb(TaskAction::Pause), "pauza");
            // A stopped task is not paused again; it goes on by a comment,
            // a step chosen or a restart, never by a resume of the panel.
            assert!(unavailable(&runner_state("paused"), TaskAction::Pause).contains("wstrzymane"));
            assert_eq!(
                unavailable(&queued("a", 1), TaskAction::Pause),
                "zadanie jest w kolejce"
            );
            assert_eq!(
                unavailable(&runner_state("provisioning"), TaskAction::Pause),
                "zadanie jest w kolejce"
            );
            assert!(unavailable(&runner_state("interrupted"), TaskAction::Pause)
                .contains("uruchom je od nowa"));
            assert!(unavailable(&finished("a", 1), TaskAction::Pause).contains("zakończone"));
            assert!(
                unavailable(&runner_state("pause_requested"), TaskAction::Pause)
                    .contains("wstrzymywane")
            );
        }

        #[test]
        fn stop_needs_a_running_or_pausing_task_and_a_runner_that_answers() {
            assert!(available(&running("a", 1), TaskAction::Stop));
            assert_eq!(
                unavailable(&queued("a", 1), TaskAction::Stop),
                "zadanie jest w kolejce"
            );
            assert!(unavailable(&runner_state("paused"), TaskAction::Stop)
                .contains("napisz komentarz albo wybierz krok"));
            assert!(unavailable(&finished("a", 1), TaskAction::Stop).contains("zakończone"));
            assert_eq!(
                unavailable(&runner_state("running"), TaskAction::Stop),
                "runner jeszcze się nie zgłosił"
            );
            let mut silent = running("a", 1);
            silent.runner.as_mut().unwrap().freshness =
                narsor_engine::controller_protocol::Freshness::Stale;
            let silent = offered(silent);
            assert!(unavailable(&silent, TaskAction::Stop).contains("nie odpowiada"));
        }

        #[test]
        fn a_transition_needs_exits_or_an_override() {
            let exits = [("approved", "ship", Some("Wypuszczamy."))];
            assert!(available(
                &with_transitions(running("a", 1), "review", &exits, None),
                TaskAction::Transition
            ));
            assert!(available(
                &with_transitions(running("a", 1), "review", &[], Some(&["plan"])),
                TaskAction::Transition
            ));
            assert!(unavailable(
                &with_transitions(running("a", 1), "review", &[], None),
                TaskAction::Transition
            )
            .contains("nie ma wyjść"));
            assert!(unavailable(&running("a", 1), TaskAction::Transition)
                .contains("serwer nie podał wyjść"));
            assert_eq!(
                unavailable(&queued("a", 1), TaskAction::Transition),
                "zadanie jest w kolejce"
            );
        }

        #[test]
        fn start_again_is_for_interrupted_tasks_and_a_failed_provisioning() {
            assert!(available(&runner_state("interrupted"), TaskAction::Restart));
            assert!(available(&runner_state("unknown"), TaskAction::Restart));
            let failed = waiting_on(
                runner_state("provisioning"),
                AttentionKind::ProvisioningFailed,
                "p-1",
                None,
            );
            assert!(available(&failed, TaskAction::Restart));
            assert!(unavailable(&running("a", 1), TaskAction::Restart).contains("działa"));
            assert_eq!(
                unavailable(&queued("a", 1), TaskAction::Restart),
                "zadanie jest w kolejce"
            );
            assert!(unavailable(&runner_state("paused"), TaskAction::Restart)
                .contains("nie jest skryptem"));
        }

        #[test]
        fn archive_is_offered_wherever_the_control_plane_takes_it() {
            assert!(available(&finished("a", 1), TaskAction::Archive));
            assert!(available(&runner_state("interrupted"), TaskAction::Archive));
            let failed = waiting_on(
                runner_state("provisioning"),
                AttentionKind::ProvisioningFailed,
                "p-1",
                None,
            );
            assert!(available(&failed, TaskAction::Archive));
            // The control plane archives a running, queued or paused task too
            // (it frees the slot), so the server offers it and the menu shows it.
            for taken in [running("a", 1), queued("a", 1), runner_state("paused")] {
                assert!(
                    available(&taken, TaskAction::Archive),
                    "{:?}",
                    taken.runner_state
                );
            }
        }

        #[test]
        fn the_client_draws_the_offer_it_is_given_not_a_rule_of_its_own() {
            // The server says archive is not possible now; no local rule
            // overrides it.
            let mut task = running("a", 1);
            for offer in task.actions.as_mut().unwrap() {
                if offer.id == narsor_engine::controller_protocol::TaskActionId::Archive {
                    offer.available = false;
                    offer.reason = Some("zadanie działa — poczekaj".to_string());
                }
            }
            assert_eq!(
                unavailable(&task, TaskAction::Archive),
                "zadanie działa — poczekaj"
            );
        }

        #[test]
        fn a_task_without_an_offer_has_no_action_from_the_server() {
            let mut task = running("a", 1);
            task.actions = None;
            for action in [
                TaskAction::Pause,
                TaskAction::Stop,
                TaskAction::ReleaseSlot,
                TaskAction::Transition,
                TaskAction::SelectStep,
                TaskAction::Restart,
                TaskAction::Archive,
                TaskAction::Comment,
            ] {
                assert_eq!(
                    unavailable(&task, action),
                    "niedostępne: brak danych z serwera",
                    "{action:?}"
                );
            }
            // Moving and opening a link are the client's own.
            assert_eq!(
                unavailable(&task, TaskAction::Move),
                "zadanie nie jest w kolejce"
            );
        }

        #[test]
        fn a_link_needs_a_pull_request_address() {
            assert_eq!(
                unavailable(&running("a", 1), TaskAction::OpenLink),
                "zadanie nie ma jeszcze linku do PR"
            );
            let without_pr = with_header_workspaces(
                running("a", 1),
                vec![header_workspace("code", "orders-api", "main")],
            );
            assert_eq!(
                unavailable(&without_pr, TaskAction::OpenLink),
                "zadanie nie ma jeszcze linku do PR"
            );
            let unsafe_scheme = with_header_workspaces(
                running("a", 1),
                vec![with_pr(
                    header_workspace("code", "orders-api", "main"),
                    "javascript:alert(1)",
                )],
            );
            assert!(!available(&unsafe_scheme, TaskAction::OpenLink));
            assert!(available(
                &with_pull_request(running("a", 1)),
                TaskAction::OpenLink
            ));
        }

        #[test]
        fn every_action_of_the_panel_has_a_rule() {
            for action in [
                TaskAction::Pause,
                TaskAction::Stop,
                TaskAction::ReleaseSlot,
                TaskAction::Transition,
                TaskAction::SelectStep,
                TaskAction::Restart,
                TaskAction::Archive,
                TaskAction::OpenLink,
            ] {
                assert!(
                    TaskAction::ALL.contains(&action),
                    "{action:?} is in the panel"
                );
            }
            // A comment is the conversation's and a move has its own keys.
            assert!(!TaskAction::ALL.contains(&TaskAction::Comment));
            assert!(!TaskAction::ALL.contains(&TaskAction::Move));
            assert!(TaskAction::Stop.needs_confirmation());
            assert!(TaskAction::Archive.needs_confirmation());
            assert!(!TaskAction::Pause.needs_confirmation());
            assert!(!TaskAction::OpenLink.changes_state());
        }
    }

    // -- the menu ----------------------------------------------------------

    mod menu {
        use super::*;

        #[test]
        fn dot_on_a_running_task_lists_the_actions_and_greys_out_the_move() {
            let mut app = loaded(running("a", 1));
            ch(&mut app, '.');
            let Some(Overlay::Menu(_)) = app.overlay() else {
                panic!("the menu should be open");
            };
            let chooser = app.overlay().unwrap().chooser().unwrap();
            let by_label = |needle: &str| {
                chooser
                    .items()
                    .iter()
                    .find(|item| item.label.contains(needle))
                    .unwrap_or_else(|| panic!("no entry {needle}"))
            };
            assert_eq!(by_label("pauza").unavailable, None);
            assert_eq!(by_label("stop").unavailable, None);
            assert!(by_label("stop").label.contains('!'));
            assert_eq!(
                by_label("zwolnij slot").unavailable.as_deref(),
                Some("zadanie nie czeka na operatora")
            );
            assert!(by_label("wybierz krok").unavailable.is_some());
            assert!(by_label("uruchom od nowa").unavailable.is_some());
            assert_eq!(
                by_label("przejście ręczne").unavailable.as_deref(),
                Some("serwer nie podał wyjść bieżącego stanu"),
                "no exits known, so the entry is greyed with the reason"
            );
            assert!(
                chooser
                    .items()
                    .iter()
                    .all(|item| !item.label.contains("przesuń")),
                "a move has its own keys, not an entry of the panel"
            );
            assert_eq!(by_label("archiwizuj").unavailable, None);
            assert!(by_label("otwórz link").unavailable.is_some());
            assert_eq!(app.key_context().base, Context::Menu);
        }

        #[test]
        fn the_transition_entry_is_there_when_the_state_has_exits() {
            let task = with_transitions(
                running("a", 1),
                "review",
                &[("approved", "ship", None)],
                None,
            );
            let mut app = loaded(task);
            ch(&mut app, '.');
            let chooser = app.overlay().unwrap().chooser().unwrap();
            let entry = chooser
                .items()
                .iter()
                .find(|item| item.label.contains("przejście ręczne"))
                .unwrap();
            assert_eq!(entry.unavailable, None);
        }

        #[test]
        fn the_menu_moves_picks_by_digit_and_closes_with_esc() {
            let mut app = loaded(running("a", 1));
            ch(&mut app, '.');
            assert_eq!(app.overlay().unwrap().chooser().unwrap().selected(), 0);
            ch(&mut app, 'j');
            ch(&mut app, 'j');
            assert_eq!(app.overlay().unwrap().chooser().unwrap().selected(), 2);
            ch(&mut app, 'k');
            assert_eq!(app.overlay().unwrap().chooser().unwrap().selected(), 1);
            ch(&mut app, '4');
            assert_eq!(app.overlay().unwrap().chooser().unwrap().selected(), 3);
            press(&mut app, KeyCode::Esc);
            assert!(app.overlay().is_none());
            assert_eq!(app.key_context().base, Context::List);
        }

        #[test]
        fn enter_on_a_greyed_entry_says_why_and_keeps_the_menu_open() {
            let mut app = loaded(running("a", 1));
            ch(&mut app, '.');
            // The release of the slot is the third entry; a task that works
            // has no wait to cut short.
            ch(&mut app, '3');
            assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
            assert_eq!(
                note(&app),
                "zwolnienie slotu: zadanie nie czeka na operatora"
            );
            assert!(app.overlay().is_some(), "the menu stays");
        }

        #[test]
        fn a_shortcut_in_the_menu_runs_the_action_and_closes_it() {
            let mut app = loaded(running("a", 1));
            ch(&mut app, '.');
            let request = act(choose(&mut app, TaskAction::Pause));
            assert!(app.overlay().is_none());
            assert_eq!(request.send, ActionSend::Owner(OwnerCommand::Pause));
        }

        #[test]
        fn enter_on_an_available_entry_runs_it() {
            let mut app = loaded(running("a", 1));
            ch(&mut app, '.');
            ch(&mut app, '1');
            let request = act(press(&mut app, KeyCode::Enter));
            assert_eq!(request.send, ActionSend::Owner(OwnerCommand::Pause));
        }

        #[test]
        fn a_shortcut_to_an_unavailable_action_in_the_menu_gives_the_same_reason() {
            let mut task = running("a", 1);
            for offer in task.actions.as_mut().unwrap() {
                if offer.id == narsor_engine::controller_protocol::TaskActionId::Archive {
                    offer.available = false;
                    offer.reason = Some("zadanie działa — poczekaj".to_string());
                }
            }
            let mut app = app();
            app.load(listing(vec![titled_as_served(task)], "c"));
            ch(&mut app, '.');
            assert_eq!(choose(&mut app, TaskAction::Archive), Request::None);
            assert!(
                note(&app).starts_with("archiwizacja: zadanie działa"),
                "{}",
                note(&app)
            );
            assert!(app.overlay().is_some());
        }

        #[test]
        fn the_move_keys_work_from_the_menu_of_a_queued_task() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1), queued("b", 2)], "c"));
            ch(&mut app, '.');
            let request = app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL));
            assert!(matches!(request, Request::Move(_)), "{request:?}");
            assert!(app.overlay().is_none(), "a move to make closes the menu");

            ch(&mut app, '.');
            ch(&mut app, 'm');
            assert!(app.overlay().is_none());
            assert!(app.list().moving().is_some(), "m enters the move mode");
        }

        #[test]
        fn the_menu_is_rebuilt_when_the_task_changes_and_closed_when_it_leaves() {
            let mut app = loaded(running("a", 1));
            ch(&mut app, '.');
            app.load(listing(vec![titled(runner_state("paused"), "NAR-12")], "d"));
            let chooser = app.overlay().unwrap().chooser().unwrap();
            let pause = chooser
                .items()
                .iter()
                .find(|item| item.label.contains("pauza"))
                .expect("the pause entry");
            assert!(
                pause.unavailable.is_some(),
                "the task stopped: it cannot be paused again"
            );
            app.on_stream(event(
                "e",
                "a",
                3,
                RunnerState::Archived,
                TaskEventKind::Created,
            ));
            assert!(app.overlay().is_none(), "the task left the list");
        }

        #[test]
        fn the_menu_opens_from_the_details_too() {
            let mut app = loaded(running("a", 1));
            press(&mut app, KeyCode::Enter);
            ch(&mut app, '.');
            assert!(app.overlay().is_some());
        }

        #[test]
        fn nothing_is_opened_without_a_task() {
            let mut app = app();
            ch(&mut app, '.');
            assert!(app.overlay().is_none());
            assert_eq!(choose(&mut app, TaskAction::Pause), Request::None);
        }
    }

    // -- shortcuts ---------------------------------------------------------

    mod shortcuts {
        use super::*;

        #[test]
        fn p_on_a_queued_task_says_why_it_cannot() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1)], "c"));
            assert_eq!(choose(&mut app, TaskAction::Pause), Request::None);
            assert_eq!(note(&app), "pauza: zadanie jest w kolejce");
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Warning);
        }

        #[test]
        fn pause_of_a_running_task_goes_without_asking_and_a_stopped_one_is_not_paused() {
            let mut app = loaded(running("a", 1));
            let pause = act(choose(&mut app, TaskAction::Pause));
            assert_eq!(pause.send, ActionSend::Owner(OwnerCommand::Pause));
            assert_eq!(pause.generation, 1);
            assert!(!pause.key.is_empty());
            assert!(app.overlay().is_none(), "no window for a plain action");

            // No resume in the panel: a stopped task has none to choose.
            let mut app = loaded(runner_state("paused"));
            assert_eq!(choose(&mut app, TaskAction::Pause), Request::None);
            assert!(note(&app).starts_with("pauza:"), "{}", note(&app));
        }

        #[test]
        fn a_stopped_task_starts_a_script_step_again_through_the_runner() {
            let mut task = with_runner(runner_state("paused"), ExecutionStatus::Paused, "build");
            task.runner
                .as_mut()
                .unwrap()
                .execution_cursor
                .current_action_type = Some("script".to_string());
            let mut app = loaded(offered(task));
            let restart = act(choose(&mut app, TaskAction::Restart));
            assert_eq!(
                restart.send,
                ActionSend::Operator(OperatorCommand::RestartStep {
                    note: None,
                    failure_key: None
                })
            );
        }

        #[test]
        fn freeing_a_slot_sends_the_pause_before_its_time() {
            let mut task = running("a", 1);
            task.slot_release_pending = true;
            let mut app = loaded(offered(task));
            let release = act(choose(&mut app, TaskAction::ReleaseSlot));
            assert_eq!(release.send, ActionSend::Owner(OwnerCommand::ReleaseSlot));
            assert_eq!(release.summary, "zwolnienie slotu zlecone");
        }

        #[test]
        fn s_starts_an_interrupted_task_again_and_says_why_for_a_running_one() {
            let mut app = loaded(runner_state("interrupted"));
            let start = act(choose(&mut app, TaskAction::Restart));
            assert_eq!(start.send, ActionSend::Owner(OwnerCommand::Start));

            let mut app = loaded(running("a", 1));
            assert_eq!(choose(&mut app, TaskAction::Restart), Request::None);
            assert!(
                note(&app).starts_with("uruchomienie od nowa:"),
                "{}",
                note(&app)
            );
        }

        #[test]
        fn the_shortcuts_work_in_the_details_too() {
            let mut app = loaded(running("a", 1));
            press(&mut app, KeyCode::Enter);
            let pause = act(choose(&mut app, TaskAction::Pause));
            assert_eq!(pause.send, ActionSend::Owner(OwnerCommand::Pause));
        }

        #[test]
        fn a_retry_of_a_decision_that_got_no_answer_reuses_its_key() {
            let mut app = loaded(running("a", 1));
            let first = act(choose(&mut app, TaskAction::Pause));
            let again = act(choose(&mut app, TaskAction::Pause));
            assert_eq!(first.key, again.key, "the same decision, the same key");
        }

        #[tokio::test]
        async fn a_lost_connection_keeps_the_key_for_the_retry() {
            let mut app = loaded(running("a", 1));
            let first = act(choose(&mut app, TaskAction::Pause));
            app.perform(Request::Act(Box::new(first.clone()))).await;
            assert!(
                app.notifier().latest().unwrap().severity == Severity::Error,
                "{}",
                note(&app)
            );
            // The connection came back.
            app.load(listing(vec![titled(running("a", 1), "NAR-12")], "d"));
            let again = act(choose(&mut app, TaskAction::Pause));
            assert_eq!(first.key, again.key);
        }
    }

    // -- the confirmation window -------------------------------------------

    mod confirmation {
        use super::*;

        #[test]
        fn stop_asks_with_the_effect_and_y_sends_the_stop() {
            let mut app = loaded(running("a", 1));
            assert_eq!(choose(&mut app, TaskAction::Stop), Request::None);
            let window = confirmation(&app);
            assert!(
                window.heading().contains("Zatrzymaj"),
                "{}",
                window.heading()
            );
            let text = window.lines().join(" ");
            assert!(text.contains("„review”"), "the step it cuts short: {text}");
            assert!(text.contains("sesja agenta zostaje"), "{text}");
            assert!(text.contains("NAR-12"), "{text}");
            assert_eq!(app.key_context().base, Context::Confirm);

            let request = act(ch(&mut app, 'y'));
            assert_eq!(
                request.send,
                ActionSend::Operator(OperatorCommand::StopStep)
            );
            assert_eq!(request.task_id, "a");
            assert_eq!(request.generation, 1);
            assert!(!request.key.is_empty());
            assert!(app.overlay().is_none());
            assert_eq!(app.key_context().base, Context::List);
        }

        #[test]
        fn n_and_esc_take_it_back_and_other_keys_do_nothing() {
            let mut app = loaded(running("a", 1));
            choose(&mut app, TaskAction::Stop);
            assert_eq!(ch(&mut app, 'j'), Request::None);
            assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
            assert!(app.overlay().is_some(), "only y and n answer it");
            assert_eq!(ch(&mut app, 'n'), Request::None);
            assert!(app.overlay().is_none());

            choose(&mut app, TaskAction::Stop);
            assert_eq!(press(&mut app, KeyCode::Esc), Request::None);
            assert!(app.overlay().is_none());
        }

        #[test]
        fn a_on_a_finished_task_asks_and_y_archives() {
            let mut app = loaded(finished("a", 1));
            assert_eq!(choose(&mut app, TaskAction::Archive), Request::None);
            let window = confirmation(&app);
            assert!(window.heading().contains("Archiwizuj"));
            let text = window.lines().join(" ");
            assert!(text.contains("Usuwa workload i storage"), "{text}");
            assert!(text.contains("nie da się cofnąć"), "{text}");
            let request = act(ch(&mut app, 'y'));
            assert_eq!(request.send, ActionSend::Owner(OwnerCommand::Archive));
        }

        #[test]
        fn every_action_marked_with_a_bang_asks_the_same_way() {
            for (action, task) in [
                (TaskAction::Stop, running("a", 1)),
                (TaskAction::Archive, finished("a", 1)),
            ] {
                let mut app = loaded(task);
                choose(&mut app, action);
                assert_eq!(app.key_context().base, Context::Confirm, "{action:?}");
                assert!(matches!(
                    app.overlay().unwrap().confirmation().unwrap().confirmed(),
                    Confirmed::Act(_)
                ));
            }
        }

        #[test]
        fn y_does_not_send_without_a_connection() {
            let mut app = loaded(running("a", 1));
            choose(&mut app, TaskAction::Stop);
            app.on_stream(Some(Err(ClientError::Unavailable("down".into()))));
            assert_eq!(ch(&mut app, 'y'), Request::None);
            assert!(note(&app).contains("zablokowana"), "{}", note(&app));
            assert!(app.overlay().is_none());
        }

        #[test]
        fn the_window_closes_when_its_task_leaves_the_list() {
            let mut app = loaded(finished("a", 1));
            choose(&mut app, TaskAction::Archive);
            app.on_stream(event(
                "d",
                "a",
                2,
                RunnerState::Archived,
                TaskEventKind::Created,
            ));
            assert!(app.overlay().is_none());
        }

        #[test]
        fn the_keys_of_the_window_are_in_the_key_map() {
            let press = |code| KeyEvent::new(code, KeyModifiers::NONE);
            assert_eq!(
                keymap::lookup(Context::Confirm, &press(KeyCode::Char('y'))),
                Some(Action::Accept)
            );
            assert_eq!(
                keymap::lookup(Context::Confirm, &press(KeyCode::Char('n'))),
                Some(Action::Cancel)
            );
            assert_eq!(
                keymap::lookup(Context::Confirm, &press(KeyCode::Esc)),
                Some(Action::Cancel)
            );
            assert!(
                Context::ALL.contains(&Context::Confirm),
                "the confirmation is a context of the map"
            );
            keymap::check_consistency(keymap::BINDINGS).expect("the key map stays consistent");
            let hints = keymap::hints(Context::Confirm);
            assert!(hints
                .iter()
                .any(|(keys, label)| keys == "y" && *label == "potwierdź"));
            assert!(hints
                .iter()
                .any(|(keys, label)| keys.starts_with('n') && *label == "anuluj"));
        }
    }

    // -- manual transitions --------------------------------------------------

    mod transitions {
        use super::*;

        fn offered(override_targets: Option<&[&str]>) -> TaskSummary {
            with_transitions(
                running("a", 1),
                "review",
                &[
                    ("approved", "ship", Some("Zatwierdza i wypuszcza zmianę.")),
                    ("revise", "implement", None),
                ],
                override_targets,
            )
        }

        #[test]
        fn t_lists_the_exits_with_their_descriptions_and_a_choice_sends_the_transition() {
            let mut app = loaded(offered(None));
            assert_eq!(choose(&mut app, TaskAction::Transition), Request::None);
            let Some(Overlay::Transitions(_)) = app.overlay() else {
                panic!("the list of exits");
            };
            let chooser = app.overlay().unwrap().chooser().unwrap();
            assert_eq!(chooser.items().len(), 2, "no override: only the exits");
            assert!(chooser.items()[0].label.contains("approved"));
            assert!(chooser.items()[0].label.contains("ship"));
            assert!(chooser.items()[0]
                .description
                .contains("Zatwierdza i wypuszcza"));
            assert!(
                chooser.items()[1].description.contains("nie opisuje"),
                "an exit without a description says so"
            );
            let text = screen(&mut app, 120, 40);
            assert!(text.contains("approved → ship"), "{text}");
            assert!(text.contains("Zatwierdza i wypuszcza"), "{text}");

            ch(&mut app, '2');
            let request = act(press(&mut app, KeyCode::Enter));
            assert_eq!(
                request.send,
                ActionSend::Operator(OperatorCommand::Transition {
                    target: "implement".to_string(),
                    forced: false,
                    reason: None,
                })
            );
            assert!(app.overlay().is_none(), "an exit is sent at once");
        }

        #[test]
        fn esc_leaves_the_list_of_exits() {
            let mut app = loaded(offered(None));
            choose(&mut app, TaskAction::Transition);
            press(&mut app, KeyCode::Esc);
            assert!(app.overlay().is_none());
        }

        #[test]
        fn the_override_is_only_there_when_the_state_allows_it() {
            let mut app = loaded(offered(None));
            choose(&mut app, TaskAction::Transition);
            assert!(app
                .overlay()
                .unwrap()
                .chooser()
                .unwrap()
                .items()
                .iter()
                .all(|item| !item.label.contains("override")));

            let mut app = loaded(offered(Some(&["plan", "ship"])));
            choose(&mut app, TaskAction::Transition);
            let chooser = app.overlay().unwrap().chooser().unwrap();
            assert_eq!(chooser.items().len(), 3);
            assert!(chooser.items()[2].label.contains("override"));
            assert!(chooser.items()[2].label.starts_with('!'));
        }

        #[test]
        fn the_override_needs_a_reason_and_asks_before_it_sends() {
            let mut app = loaded(offered(Some(&["plan", "ship"])));
            choose(&mut app, TaskAction::Transition);
            ch(&mut app, '3');
            // Enter goes to the fields, then the reason, then validation runs.
            assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
            assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
            assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
            assert!(app.overlay().is_some(), "no reason, nothing to confirm");
            let chooser = app.overlay().unwrap().chooser().unwrap();
            assert!(
                chooser.fields_of(2)[1].error().is_some(),
                "the reason is what is missing"
            );

            // Down to the reason, type it, send.
            press(&mut app, KeyCode::Down);
            typed(&mut app, "CI nie ruszy bez tego");
            assert_eq!(
                app.key_context().base,
                Context::Form,
                "typing: q and ? are text"
            );
            assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
            let window = confirmation(&app);
            assert!(window.heading().contains("Override"));
            let text = window.lines().join(" ");
            assert!(
                text.contains("„review”") && text.contains("„plan”"),
                "{text}"
            );
            assert!(text.contains("CI nie ruszy bez tego"), "{text}");
            assert!(text.contains("audytu"), "{text}");

            let request = act(ch(&mut app, 'y'));
            assert_eq!(
                request.send,
                ActionSend::Operator(OperatorCommand::Transition {
                    target: "plan".to_string(),
                    forced: true,
                    reason: Some("CI nie ruszy bez tego".to_string()),
                })
            );
        }

        #[test]
        fn a_paste_fills_the_override_reason_and_sends_nothing() {
            let mut app = loaded(offered(Some(&["plan", "ship"])));
            choose(&mut app, TaskAction::Transition);
            app.on_paste("z listy");
            let chooser = app.overlay().unwrap().chooser().unwrap();
            assert!(
                chooser.fields_of(2)[1].value().is_empty(),
                "with the keys on the list a paste goes nowhere"
            );

            ch(&mut app, '3');
            press(&mut app, KeyCode::Enter);
            press(&mut app, KeyCode::Down);
            app.on_paste("CI nie ruszy\r\nbez tego\n");
            let Some(Overlay::Transitions(_)) = app.overlay() else {
                panic!("a paste neither sends nor closes the list");
            };
            let chooser = app.overlay().unwrap().chooser().unwrap();
            assert_eq!(
                chooser.fields_of(2)[1].value(),
                "CI nie ruszy bez tego ",
                "the one-line reason gets the paste, its line breaks as spaces"
            );
        }

        #[test]
        fn n_in_the_override_window_sends_nothing() {
            let mut app = loaded(offered(Some(&["plan"])));
            choose(&mut app, TaskAction::Transition);
            ch(&mut app, '3');
            press(&mut app, KeyCode::Enter);
            assert_eq!(
                app.overlay().unwrap().chooser().unwrap().focus(),
                narsor::tui::chooser::ChooserFocus::Fields
            );
            press(&mut app, KeyCode::Tab);
            assert_eq!(
                app.overlay().unwrap().chooser().unwrap().focus(),
                narsor::tui::chooser::ChooserFocus::Actions
            );
            press(&mut app, KeyCode::Tab);
            assert_eq!(
                app.overlay().unwrap().chooser().unwrap().focus(),
                narsor::tui::chooser::ChooserFocus::Fields
            );
            press(&mut app, KeyCode::Down);
            typed(&mut app, "powód");
            press(&mut app, KeyCode::Enter);
            assert_eq!(ch(&mut app, 'n'), Request::None);
            assert!(app.overlay().is_none());
        }

        #[test]
        fn t_where_the_state_has_no_exits_says_why() {
            let mut app = loaded(running("a", 1));
            assert_eq!(choose(&mut app, TaskAction::Transition), Request::None);
            assert_eq!(
                note(&app),
                "przejście: serwer nie podał wyjść bieżącego stanu"
            );
            assert!(
                matches!(app.overlay(), Some(Overlay::Menu(_))),
                "the panel stays, no list of exits opens"
            );
        }

        #[test]
        fn server_text_in_the_exits_is_made_safe() {
            let task = with_transitions(
                running("a", 1),
                "review",
                &[("ok", "ship", Some("Dobrze\u{1b}[31m czerwone"))],
                None,
            );
            let mut app = loaded(task);
            choose(&mut app, TaskAction::Transition);
            let text = screen(&mut app, 120, 40);
            assert!(!text.contains('\u{1b}'));
            assert!(text.contains("Dobrze"));
        }
    }

    // -- links -----------------------------------------------------------------

    mod links {
        use super::*;

        #[test]
        fn o_without_a_link_value_says_why() {
            let mut app = loaded(running("a", 1));
            assert_eq!(ch(&mut app, 'o'), Request::None);
            assert_eq!(note(&app), "link: zadanie nie ma jeszcze linku do PR");
            let unresolved = with_header_workspaces(
                running("a", 1),
                vec![header_workspace("code", "orders-api", "main")],
            );
            let mut app = loaded(unresolved);
            assert_eq!(ch(&mut app, 'o'), Request::None);
            assert_eq!(note(&app), "link: zadanie nie ma jeszcze linku do PR");
            assert!(app.overlay().is_none());
        }

        #[test]
        fn o_lists_the_links_with_a_value_and_enter_opens_one() {
            let task = with_header_workspaces(
                running("a", 1),
                vec![
                    with_pr(
                        header_workspace("code", "orders-api", "feat/x"),
                        "https://gitlab.example/g/api/-/merge_requests/42",
                    ),
                    header_workspace("tests", "orders-tests", "main"),
                    with_pr(
                        header_workspace("docs", "orders-docs", "feat/x"),
                        "https://docs.example/x",
                    ),
                ],
            );
            let mut app = loaded(task);
            ch(&mut app, 'o');
            let chooser = app.overlay().unwrap().chooser().unwrap();
            assert_eq!(
                chooser.items().len(),
                2,
                "the unresolved one is not offered"
            );
            assert!(chooser.items()[0].label.contains("code"));
            ch(&mut app, '2');
            let request = press(&mut app, KeyCode::Enter);
            let Request::OpenLink(link) = request else {
                panic!("a link was expected");
            };
            assert_eq!(link.url, "https://docs.example/x");
            assert_eq!(link.label, "docs");
            assert!(app.overlay().is_none());
        }

        #[test]
        fn a_paste_on_the_links_opens_none_and_leaves_the_list_open() {
            let task = with_header_workspaces(
                running("a", 1),
                vec![
                    with_pr(
                        header_workspace("code", "orders-api", "feat/x"),
                        "https://gitlab.example/g/api/-/merge_requests/42",
                    ),
                    with_pr(
                        header_workspace("docs", "orders-docs", "feat/x"),
                        "https://docs.example/x",
                    ),
                ],
            );
            let mut app = loaded(task);
            ch(&mut app, 'o');
            app.on_paste("2\n");
            let Some(Overlay::Links(_)) = app.overlay() else {
                panic!("a paste neither opens a link nor closes the list");
            };
            assert_eq!(
                app.overlay().unwrap().chooser().unwrap().selected(),
                0,
                "a paste picks nothing from the list"
            );
            let Request::OpenLink(link) = press(&mut app, KeyCode::Enter) else {
                panic!("a link was expected");
            };
            assert_eq!(link.label, "code", "the list's own keys still work");
        }

        #[tokio::test]
        async fn a_link_that_is_not_http_is_never_handed_to_a_browser() {
            let mut app = loaded(running("a", 1));
            app.perform(Request::OpenLink(
                narsor::tui::task_app::actions::LinkRequest {
                    label: "Zły".to_string(),
                    url: "file:///etc/passwd".to_string(),
                },
            ))
            .await;
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Error);
            assert!(note(&app).contains("niebezpieczny"), "{}", note(&app));
        }
    }

    // -- without a connection -------------------------------------------------

    mod without_a_connection {
        use super::*;

        fn disconnected(task: TaskSummary) -> TaskApp {
            let mut app = loaded(task);
            app.on_stream(Some(Err(ClientError::Unavailable("down".into()))));
            assert!(!app.connection().is_live());
            app
        }

        #[test]
        fn every_mutation_is_blocked_with_the_reason() {
            for (action, task) in [
                (TaskAction::Pause, running("a", 1)),
                (TaskAction::Stop, running("a", 1)),
                (TaskAction::Archive, finished("a", 1)),
                (TaskAction::Restart, runner_state("interrupted")),
                (
                    TaskAction::Transition,
                    with_transitions(running("a", 1), "review", &[("ok", "ship", None)], None),
                ),
            ] {
                let mut app = disconnected(task);
                assert_eq!(choose(&mut app, action), Request::None, "{action:?}");
                assert!(
                    note(&app).contains("zablokowana"),
                    "{action:?}: {}",
                    note(&app)
                );
                assert!(
                    matches!(app.overlay(), Some(Overlay::Menu(_))),
                    "{action:?}: only the panel stays, nothing else opens"
                );
            }
        }

        #[test]
        fn the_menu_greys_out_what_changes_state_and_leaves_the_links() {
            let task = with_pull_request(running("a", 1));
            let mut app = disconnected(task);
            ch(&mut app, '.');
            let chooser = app.overlay().unwrap().chooser().unwrap();
            let pause = chooser
                .items()
                .iter()
                .find(|item| item.label.contains("pauza"))
                .unwrap();
            assert!(
                pause
                    .unavailable
                    .as_deref()
                    .is_some_and(|reason| reason.contains("zablokowana")),
                "{:?}",
                pause.unavailable
            );
            let link = chooser
                .items()
                .iter()
                .find(|item| item.label.contains("otwórz link"))
                .unwrap();
            assert_eq!(link.unavailable, None, "opening a link changes nothing");

            // And it comes back with the connection.
            app.on_stream(event(
                "d",
                "a",
                2,
                RunnerState::Running,
                TaskEventKind::Created,
            ));
            let chooser = app.overlay().unwrap().chooser().unwrap();
            assert!(chooser
                .items()
                .iter()
                .find(|item| item.label.contains("pauza"))
                .unwrap()
                .unavailable
                .is_none());
        }

        #[test]
        fn a_task_that_cannot_be_acted_on_says_so_before_the_connection_does() {
            let mut app = disconnected(queued("a", 1));
            assert_eq!(choose(&mut app, TaskAction::Pause), Request::None);
            assert_eq!(note(&app), "pauza: zadanie jest w kolejce");
        }
    }

    // -- the verdict of the runner ---------------------------------------------

    mod verdicts {
        use super::*;

        fn queued_command(id: &str, command: OperatorCommand) -> OperatorCommandView {
            OperatorCommandView {
                command_id: id.to_string(),
                command,
                status: OperatorCommandStatus::Queued,
                submitted_at: super::super::task_app_support::at(),
                updated_at: super::super::task_app_support::at(),
                detail: None,
            }
        }

        fn command(app: &TaskApp) -> OperatorCommandView {
            app.list().tasks()[0].operator_commands[0].clone()
        }

        #[test]
        fn queued_and_resolved_events_patch_the_row_and_the_bar_tells_the_verdict() {
            let mut app = loaded(running("a", 1));
            app.on_stream(event(
                "d",
                "a",
                2,
                RunnerState::Running,
                TaskEventKind::OperatorCommandQueued {
                    command_id: "command-1".to_string(),
                    command: OperatorCommand::Interrupt,
                    requested_by: OwnerRef::new("local"),
                },
            ));
            assert_eq!(command(&app).status, OperatorCommandStatus::Queued);
            assert!(!app.refresh_wanted(), "queueing is patched, not re-read");

            app.on_stream(event(
                "e",
                "a",
                3,
                RunnerState::Running,
                TaskEventKind::OperatorCommandResolved {
                    command_id: "command-1".to_string(),
                    status: OperatorCommandStatus::Applied,
                    detail: None,
                },
            ));
            assert_eq!(command(&app).status, OperatorCommandStatus::Applied);
            assert_eq!(note(&app), "NAR-12: przerwanie kroku wykonane");
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Success);
            assert_eq!(app.cursor(), Some("e"));
        }

        #[test]
        fn a_command_the_runner_rejects_is_reported_with_its_reason() {
            let mut app = loaded(running("a", 1));
            app.on_stream(event(
                "d",
                "a",
                2,
                RunnerState::Running,
                TaskEventKind::OperatorCommandQueued {
                    command_id: "command-1".to_string(),
                    command: OperatorCommand::Transition {
                        target: "ship".to_string(),
                        forced: false,
                        reason: None,
                    },
                    requested_by: OwnerRef::new("local"),
                },
            ));
            app.on_stream(event(
                "e",
                "a",
                3,
                RunnerState::Running,
                TaskEventKind::OperatorCommandResolved {
                    command_id: "command-1".to_string(),
                    status: OperatorCommandStatus::Rejected,
                    detail: Some("the execution moved on\u{1b}[31m already".to_string()),
                },
            ));
            let text = note(&app);
            assert!(
                text.contains("przejście do „ship” odrzucone przez runnera"),
                "{text}"
            );
            assert!(text.contains("the execution moved on"), "{text}");
            assert!(!text.contains('\u{1b}'), "{text:?}");
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Error);
            let command = command(&app);
            assert_eq!(command.status, OperatorCommandStatus::Rejected);
        }

        #[test]
        fn a_rejection_without_a_reason_says_so() {
            let mut app = loaded(running("a", 1));
            app.list_mut().note_operator_command(
                "a",
                queued_command("command-1", OperatorCommand::Interrupt),
            );
            app.on_stream(event(
                "d",
                "a",
                2,
                RunnerState::Running,
                TaskEventKind::OperatorCommandResolved {
                    command_id: "command-1".to_string(),
                    status: OperatorCommandStatus::Rejected,
                    detail: None,
                },
            ));
            assert!(
                note(&app).contains("bez podania przyczyny"),
                "{}",
                note(&app)
            );
        }

        #[test]
        fn a_verdict_is_told_once() {
            let mut app = loaded(running("a", 1));
            app.list_mut().note_operator_command(
                "a",
                queued_command("command-1", OperatorCommand::Interrupt),
            );
            let resolved = |cursor: &str, sequence| {
                event(
                    cursor,
                    "a",
                    sequence,
                    RunnerState::Running,
                    TaskEventKind::OperatorCommandResolved {
                        command_id: "command-1".to_string(),
                        status: OperatorCommandStatus::Applied,
                        detail: None,
                    },
                )
            };
            app.on_stream(resolved("d", 2));
            app.on_stream(event(
                "e",
                "a",
                3,
                RunnerState::Running,
                TaskEventKind::Created,
            ));
            let before = app.notifier().latest().unwrap().at;
            app.on_stream(resolved("f", 4));
            assert_eq!(
                app.notifier().latest().unwrap().at,
                before,
                "a settled command does not notify again"
            );
        }

        #[test]
        fn a_verdict_on_a_command_the_row_does_not_know_asks_for_a_read() {
            let mut app = loaded(running("a", 1));
            app.on_stream(event(
                "d",
                "a",
                2,
                RunnerState::Running,
                TaskEventKind::OperatorCommandResolved {
                    command_id: "command-9".to_string(),
                    status: OperatorCommandStatus::Applied,
                    detail: None,
                },
            ));
            assert!(app.refresh_wanted());
            assert!(
                note(&app).contains("polecenie operatora wykonane"),
                "{}",
                note(&app)
            );
        }

        #[test]
        fn an_applied_transition_says_the_runner_goes_on() {
            let mut app = loaded(running("a", 1));
            app.list_mut().note_operator_command(
                "a",
                queued_command(
                    "command-1",
                    OperatorCommand::Transition {
                        target: "ship".to_string(),
                        forced: true,
                        reason: Some("bo".to_string()),
                    },
                ),
            );
            app.on_stream(event(
                "d",
                "a",
                2,
                RunnerState::Running,
                TaskEventKind::OperatorCommandResolved {
                    command_id: "command-1".to_string(),
                    status: OperatorCommandStatus::Applied,
                    detail: None,
                },
            ));
            let text = note(&app);
            assert!(text.contains("override do „ship” wykonane"), "{text}");
            assert!(text.contains("runner kontynuuje"), "{text}");
        }
    }

    // -- the reasons of early refusals ------------------------------------------

    mod refusals {
        use super::*;

        #[test]
        fn every_early_refusal_of_the_control_plane_has_words() {
            for (code, expected) in [
                ("override_reason_required", "override wymaga powodu"),
                ("override_not_allowed", "nie dopuszcza override"),
                ("illegal_transition", "nie jest dozwolone"),
                ("missing_target", "brak stanu docelowego"),
                ("stale_generation", "zmieniło się w międzyczasie"),
                ("idempotency_key_reused", "klucz został już użyty"),
            ] {
                let text = refusal_reason(&ClientError::Request {
                    code: code.to_string(),
                    message: "detail from the server".to_string(),
                });
                assert!(text.contains(expected), "{code}: {text}");
                assert!(text.contains("detail from the server"), "{code}: {text}");
                let refused = refusal_reason(&ClientError::Refused {
                    code: code.to_string(),
                    message: String::new(),
                });
                assert!(!refused.contains("()"), "{code}: {refused}");
            }
            assert_eq!(
                refusal_reason(&ClientError::Refused {
                    code: "operator_command_refused".to_string(),
                    message: "the task is paused; resume it first".to_string(),
                }),
                "the task is paused; resume it first"
            );
        }
    }

    // -- against a served control plane ------------------------------------------
}

/// U12: the *Dyrygent* tab — the chat, the decisions with their undo, the
/// harness, and what it says when there is no conductor.
mod task_app_conductor {
    use std::time::{Duration, Instant};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::{json, Value};

    use narsor::api::{ApiClient, ClientError};
    use narsor::tui::chat::UndoState;
    use narsor::tui::keymap::{self, Action, Context};
    use narsor::tui::task_app::actions::{ActionRequest, ActionSend, Overlay};
    use narsor::tui::task_app::detail::DetailRead;
    use narsor::tui::task_app::notify::Severity;
    use narsor::tui::task_app::panel::Submit;
    use narsor::tui::task_app::{DetailTab, Focus, Request, TaskApp};
    use narsor::tui::transcript::TranscriptPresentation;
    use narsor_engine::controller_protocol::{
        ConversationUnavailableReason, ExecutionEventsSource, TaskSummary,
    };
    use narsor_engine::domain::ExecutionStatus;
    use narsor_engine::orchestration::{
        AttentionKind, OperatorCommand, OwnerRef, RunnerState, TaskEventKind,
    };
    use narsor_protocol::controller::ConductorReadResponse;
    use narsor_protocol::controller::ConductorView;

    use super::task_app_support::{
        chat_entry, conductor_view, event, listing, task, titled, waiting_on, with_conductor,
        with_controller, with_runner, without_conductor,
    };

    fn app() -> TaskApp {
        TaskApp::new(
            ApiClient::new("127.0.0.1:9".parse().unwrap(), "token"),
            "local",
            None,
        )
    }

    /// One key. An `Enter` that asks for the confirmation of a send is
    /// answered with the `Enter` of the window, so a test that says "Enter
    /// sends" still reads so; the window itself is tested with `on_key`.
    fn press(app: &mut TaskApp, code: KeyCode) -> Request {
        let request = app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
        if code == KeyCode::Enter && request == Request::None && app.form_confirming() {
            return app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        }
        request
    }

    fn ch(app: &mut TaskApp, character: char) -> Request {
        press(app, KeyCode::Char(character))
    }

    fn typed(app: &mut TaskApp, text: &str) {
        for character in text.chars() {
            ch(app, character);
        }
    }

    fn note(app: &TaskApp) -> String {
        app.notifier()
            .latest()
            .map(|note| note.text.clone())
            .unwrap_or_default()
    }

    fn act(request: Request) -> ActionRequest {
        match request {
            Request::Act(request) => *request,
            other => panic!("an action was expected, got {other:?}"),
        }
    }

    fn sent_command(request: Request) -> OperatorCommand {
        match act(request).send {
            ActionSend::Operator(command) => command,
            other => panic!("an operator command was expected, got {other:?}"),
        }
    }

    fn running(id: &str, rank: u32) -> TaskSummary {
        with_conductor(with_runner(
            with_controller(task(id, "local", "running", rank)),
            ExecutionStatus::Running,
            "review",
        ))
    }

    /// An app over the running task `a`, on the details of the *Dyrygent* tab.
    fn tab_app() -> TaskApp {
        let mut app = app();
        app.load(listing(vec![titled(running("a", 1), "NAR-12")], "c"));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.focus(), Focus::Details);
        app.detail_mut().set_tab(DetailTab::Conductor);
        app
    }

    #[test]
    fn tab_while_composing_a_conductor_message_switches_panels_and_keeps_the_draft() {
        let mut app = app();
        let task = waiting_on(running("a", 1), AttentionKind::Question, "review", None);
        app.load(listing(vec![titled(task, "NAR-12")], "c"));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.focus(), Focus::Details);
        app.detail_mut().set_tab(DetailTab::Conductor);
        let revision = app.list().selected().unwrap().revision;
        app.on_detail_read(
            "a",
            revision,
            DetailRead::Conductor(Ok(ConductorReadResponse::Available {
                conductor: Box::new(conductor_view(vec![], json!({}))),
                source: ExecutionEventsSource::Runner,
            })),
        );
        if app.panel_focused() {
            press(&mut app, KeyCode::Tab);
        }

        ch(&mut app, 'c');
        typed(&mut app, "zachowaj tę wiadomość");
        assert!(app.detail().view("a").unwrap().conductor().is_typing());
        press(&mut app, KeyCode::Tab);
        assert!(app.panel_focused(), "Tab moves from the draft to waiting");
        press(&mut app, KeyCode::Tab);
        assert!(
            app.detail().view("a").unwrap().conductor().is_typing(),
            "Tab returns to the conductor draft"
        );
        assert_eq!(
            app.detail().view("a").unwrap().conductor().draft(),
            "zachowaj tę wiadomość"
        );
    }

    fn revision(app: &TaskApp) -> u64 {
        app.list().selected().expect("a selection").revision
    }

    fn read(app: &mut TaskApp, view: ConductorView) {
        read_response(
            app,
            Ok(ConductorReadResponse::Available {
                conductor: Box::new(view),
                source: ExecutionEventsSource::Runner,
            }),
        );
    }

    fn read_response(app: &mut TaskApp, result: Result<ConductorReadResponse, ClientError>) {
        let revision = revision(app);
        app.on_detail_read("a", revision, DetailRead::Conductor(result));
    }

    fn conductor(app: &TaskApp) -> &narsor::tui::task_app::tabs::conductor::Conductor {
        app.detail().view("a").expect("details").conductor()
    }

    fn screen(app: &mut TaskApp, width: u16, height: u16) -> String {
        use narsor::tui::style::Styles;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                narsor::tui::task_app::layout::render(
                    frame,
                    app,
                    &Styles::new(false),
                    Instant::now(),
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer
            .content()
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn decision_entry(id: &str, sequence: u64, decision_id: &str) -> Value {
        chat_entry(
            id,
            sequence,
            "conductor",
            "decision",
            "Wybrano wyjście approved",
            Some(json!({
                "kind": "choose_output",
                "decision_id": decision_id,
                "output_label": "approved",
                "confidence": "high",
                "rationale": "Testy przeszły."
            })),
        )
    }

    fn decision_view(decision_id: &str, undone: bool, undoable: bool) -> Value {
        json!({
            "decision_id": decision_id,
            "kind": "choose_output",
            "text": "Wybrano wyjście approved",
            "at": "2026-10-09T12:00:00Z",
            "confidence": "high",
            "rationale": "Testy przeszły.",
            "undone": undone,
            "undoable": undoable
        })
    }

    // -- a paste -----------------------------------------------------------

    mod paste {
        use super::*;

        #[test]
        fn a_paste_into_the_message_box_is_a_draft_not_a_message() {
            let mut app = tab_app();
            read(&mut app, conductor_view(vec![], json!({})));
            ch(&mut app, 'c');
            app.on_paste("Czy revise?\nq");
            assert_eq!(conductor(&app).draft(), "Czy revise?\nq");
            assert!(conductor(&app).is_typing(), "the keys stay in the box");
            assert!(app
                .detail()
                .draft("a")
                .is_some_and(|draft| draft.contains("Czy revise?\nq")));
            let request = act(press(&mut app, KeyCode::Enter));
            assert_eq!(
                request.send,
                ActionSend::Operator(OperatorCommand::ConductorChat {
                    text: "Czy revise?\nq".to_string()
                })
            );
        }

        #[test]
        fn a_paste_outside_the_message_box_is_ignored() {
            let mut app = tab_app();
            read(&mut app, conductor_view(vec![], json!({})));
            assert!(!conductor(&app).is_typing());
            app.on_paste("c\nq\nq\n");
            assert_eq!(conductor(&app).draft(), "");
            assert!(!conductor(&app).is_typing());
            assert!(!app.should_quit());
        }

        #[test]
        fn a_paste_picks_nothing_from_the_menu() {
            let mut app = app();
            app.load(listing(vec![titled(running("a", 1), "NAR-12")], "c"));
            ch(&mut app, '.');
            let Some(Overlay::Menu(_)) = app.overlay() else {
                panic!("the menu should be open");
            };
            let before = app.overlay().unwrap().chooser().unwrap().selected();
            app.on_paste("2\n");
            let Some(Overlay::Menu(_)) = app.overlay() else {
                panic!("the menu is still open");
            };
            assert_eq!(app.overlay().unwrap().chooser().unwrap().selected(), before);
        }
    }

    // -- the chat ----------------------------------------------------------

    mod chat {
        use super::*;

        #[test]
        fn a_message_goes_out_as_a_conductor_chat_exactly_as_typed() {
            let mut app = tab_app();
            read(&mut app, conductor_view(vec![], json!({})));
            ch(&mut app, 'c');
            assert!(conductor(&app).is_typing());
            assert_eq!(
                app.key_context().base,
                Context::Form,
                "typing inherits no global key"
            );

            typed(&mut app, "  Czy revise? q");
            app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
            typed(&mut app, "Dziękuję  ");
            let request = act(press(&mut app, KeyCode::Enter));
            assert_eq!(
                request.send,
                ActionSend::Operator(OperatorCommand::ConductorChat {
                    text: "  Czy revise? q\nDziękuję  ".to_string()
                }),
                "what the operator typed is sent without a change"
            );
            assert_eq!(request.verb, "wiadomość do dyrygenta");
            assert!(
                app.detail().has_drafts(),
                "the text stays until the control plane took it"
            );

            let again = act(press(&mut app, KeyCode::Enter));
            assert_eq!(
                again.key, request.key,
                "a retry of the same message reuses its key"
            );
        }

        #[test]
        fn the_operators_entry_and_then_the_conductors_answer_have_their_roles() {
            let mut app = tab_app();
            read(
                &mut app,
                conductor_view(
                    vec![chat_entry(
                        "m1",
                        1,
                        "operator",
                        "message",
                        "Czy revise?",
                        None,
                    )],
                    json!({ "unanswered_chat": ["m1"] }),
                ),
            );
            assert!(conductor(&app).awaits_answer());
            read(
                &mut app,
                conductor_view(
                    vec![chat_entry(
                        "m2",
                        2,
                        "conductor",
                        "message",
                        "Raczej revise — brak planu.",
                        None,
                    )],
                    json!({}),
                ),
            );
            let presentations: Vec<_> = conductor(&app)
                .transcript()
                .entries()
                .iter()
                .map(|entry| entry.presentation)
                .collect();
            assert_eq!(
                presentations,
                [
                    TranscriptPresentation::Operator,
                    TranscriptPresentation::Conductor
                ]
            );
            assert!(!conductor(&app).awaits_answer());
            let text = screen(&mut app, 120, 30);
            assert!(
                text.contains("USER") && text.contains("Czy revise?"),
                "{text}"
            );
            assert!(
                text.contains("KONDUKTOR") && text.contains("Raczej revise"),
                "{text}"
            );
        }

        #[test]
        fn what_was_typed_survives_a_refresh_and_a_visit_to_another_task() {
            let mut app = tab_app();
            read(&mut app, conductor_view(vec![], json!({})));
            ch(&mut app, 'c');
            typed(&mut app, "pół zdania");
            press(&mut app, KeyCode::Esc);
            assert!(!conductor(&app).is_typing(), "Esc leaves the box");
            assert_eq!(conductor(&app).draft(), "pół zdania");
            assert_eq!(app.detail().draft("a"), Some("pół zdania"));

            app.load(listing(vec![titled(running("a", 1), "NAR-12")], "d"));
            assert_eq!(
                conductor(&app).draft(),
                "pół zdania",
                "a refresh keeps the draft"
            );
            assert!(app.detail().has_drafts());
            assert!(screen(&mut app, 120, 30).contains("szkic: pół zdania"));

            ch(&mut app, 'c');
            typed(&mut app, " i reszta");
            assert_eq!(conductor(&app).draft(), "pół zdania i reszta");
        }

        #[test]
        fn q_and_the_question_mark_are_text_while_typing_then_quit_requires_a_second_q() {
            let mut app = tab_app();
            read(&mut app, conductor_view(vec![], json!({})));
            ch(&mut app, 'c');
            typed(&mut app, "q?");
            assert!(!app.should_quit());
            assert!(!app.is_help_open());
            assert_eq!(conductor(&app).draft(), "q?");

            press(&mut app, KeyCode::Esc);
            ch(&mut app, 'q');
            assert!(!app.should_quit(), "the first q opens confirmation");
            ch(&mut app, 'q');
            assert!(app.should_quit());
        }

        #[test]
        fn a_blank_message_is_not_sent() {
            let mut app = tab_app();
            read(&mut app, conductor_view(vec![], json!({})));
            ch(&mut app, 'c');
            typed(&mut app, "   ");
            assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
            assert!(conductor(&app).is_typing());
        }

        #[test]
        fn nothing_from_the_conductor_reaches_the_screen_with_a_terminal_control() {
            let mut app = tab_app();
            read(
                &mut app,
                conductor_view(
                    vec![chat_entry(
                        "m1",
                        1,
                        "conductor",
                        "message",
                        "ok\u{1b}[31m czerwony\u{202e}tekst\u{200b}",
                        None,
                    )],
                    json!({ "harness": "cla\u{1b}]8;;x\u{7}ude" }),
                ),
            );
            let text = screen(&mut app, 120, 30);
            assert!(text.contains("czerwony"), "{text}");
            assert!(
                !text
                    .chars()
                    .any(|c| c.is_control() && c != '\n' || "\u{202e}\u{200b}".contains(c)),
                "{text:?}"
            );
        }
    }

    // -- decisions and their undo ----------------------------------------------

    mod decisions {
        use super::*;

        fn with_decision() -> TaskApp {
            let mut app = tab_app();
            read(
                &mut app,
                conductor_view(
                    vec![decision_entry("e1", 1, "d1")],
                    json!({ "decisions": [decision_view("d1", false, true)] }),
                ),
            );
            app
        }

        #[test]
        fn undoing_a_decision_asks_with_its_confidence_and_sends_after_y() {
            let mut app = with_decision();
            assert_eq!(ch(&mut app, 'u'), Request::None);
            let window = app
                .overlay()
                .and_then(Overlay::confirmation)
                .expect("the window");
            assert!(
                window.heading().contains("Cofnij decyzję"),
                "{}",
                window.heading()
            );
            let text = screen(&mut app, 120, 30);
            assert!(text.contains("Pewność dyrygenta: high"), "{text}");
            assert!(text.contains("Uzasadnienie: Testy przeszły."), "{text}");
            assert!(text.contains("skutki uboczne"), "{text}");

            ch(&mut app, 'n');
            assert!(app.overlay().is_none(), "n takes it back");
            ch(&mut app, 'u');
            let command = sent_command(ch(&mut app, 'y'));
            assert_eq!(
                command,
                OperatorCommand::UndoDecision {
                    decision_id: "d1".to_string()
                }
            );
        }

        #[test]
        fn the_undo_comes_back_as_an_entry_and_the_decision_is_marked_undone() {
            let mut app = with_decision();
            assert!(matches!(
                conductor(&app).items()[0].undo,
                Some(UndoState::Undoable { .. })
            ));
            read(
                &mut app,
                conductor_view(
                    vec![chat_entry(
                        "e2",
                        2,
                        "conductor",
                        "decision",
                        "Cofnięto decyzję d1",
                        Some(json!({ "kind": "decision_undone", "decision_id": "d1" })),
                    )],
                    json!({ "decisions": [decision_view("d1", true, false)] }),
                ),
            );
            let items = conductor(&app).items();
            assert_eq!(items.len(), 2);
            assert_eq!(items[0].undo, Some(UndoState::Undone));
            let entries = conductor(&app).transcript().entries();
            assert!(entries[0]
                .rendered_lines
                .iter()
                .any(|line| line.contains("decyzja cofnięta")));
            assert!(entries[1].rendered_lines[0].contains("Cofnięto decyzję"));

            // The selection is on the entry of the undo now: nothing to undo there.
            assert_eq!(ch(&mut app, 'u'), Request::None);
            assert!(app.overlay().is_none());
            assert!(note(&app).contains("zaznacz"), "{}", note(&app));
        }

        #[test]
        fn a_message_is_not_a_decision_and_says_so() {
            let mut app = tab_app();
            read(
                &mut app,
                conductor_view(
                    vec![chat_entry("m1", 1, "conductor", "message", "Cześć", None)],
                    json!({}),
                ),
            );
            ch(&mut app, 'u');
            assert!(app.overlay().is_none());
            assert!(note(&app).contains("którą można cofnąć"), "{}", note(&app));
        }

        #[test]
        fn a_decision_the_server_says_cannot_be_undone_is_not_offered() {
            let mut app = tab_app();
            read(
                &mut app,
                conductor_view(
                    vec![decision_entry("e1", 1, "d1")],
                    json!({ "decisions": [decision_view("d1", false, false)] }),
                ),
            );
            assert_eq!(conductor(&app).items()[0].undo, None);
            ch(&mut app, 'u');
            assert!(app.overlay().is_none());
        }

        #[test]
        fn a_refused_undo_says_why_on_the_bar() {
            let mut app = with_decision();
            for (cursor, kind) in [
                (
                    "d",
                    TaskEventKind::OperatorCommandQueued {
                        command_id: "command-1".to_string(),
                        command: OperatorCommand::UndoDecision {
                            decision_id: "d1".to_string(),
                        },
                        requested_by: OwnerRef::new("local"),
                    },
                ),
                (
                    "e",
                    TaskEventKind::OperatorCommandResolved {
                        command_id: "command-1".to_string(),
                        status: narsor_engine::orchestration::OperatorCommandStatus::Rejected,
                        detail: Some("a later step already ran".to_string()),
                    },
                ),
            ] {
                let sequence = if cursor == "d" { 2 } else { 3 };
                app.on_stream(event(cursor, "a", sequence, RunnerState::Running, kind));
            }
            let text = note(&app);
            assert!(
                text.contains("cofnięcie decyzji dyrygenta odrzucone przez runnera"),
                "{text}"
            );
            assert!(text.contains("a later step already ran"), "{text}");
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Error);
            let revision = revision(&app);
            assert!(
                conductor(&app).needs_read(revision, Instant::now()),
                "the tab is due for a read"
            );
        }
    }

    // -- proposals live in the panel --------------------------------------------

    mod proposals {
        use super::*;

        fn proposal_view() -> ConductorView {
            conductor_view(
                vec![chat_entry(
                    "p1",
                    2,
                    "conductor",
                    "proposal",
                    "Uruchom krok od nowa",
                    None,
                )],
                json!({
                    "proposals": [{
                        "proposal_id": "p1",
                        "text": "Uruchom krok od nowa",
                        "at": "2026-10-09T12:00:00Z",
                        "record": {
                            "episode": "ep-x",
                            "state_id": "test",
                            "proposal": { "kind": "retry", "summary": "Uruchom krok od nowa", "text": "z nowszym toolchainem" },
                            "confidence": "high",
                            "rationale": "Błąd wygląda na przejściowy."
                        },
                        "in_reply_to": "m1",
                        "state": "pending"
                    }]
                }),
            )
        }

        fn waiting_app() -> TaskApp {
            let mut app = app();
            app.load(listing(
                vec![titled(
                    waiting_on(
                        running("a", 1),
                        AttentionKind::ConductorProposal,
                        "p1",
                        Some("test"),
                    ),
                    "NAR-12",
                )],
                "c",
            ));
            ch(&mut app, ' ');
            press(&mut app, KeyCode::Enter);
            assert!(app.panel_focused());
            app.detail_mut().set_tab(DetailTab::Conductor);
            app
        }

        #[test]
        fn a_proposal_from_the_chat_is_answered_in_the_panel_not_in_the_tab() {
            let mut app = waiting_app();
            read(&mut app, proposal_view());
            let text = screen(&mut app, 120, 40);
            assert!(text.contains("Propozycja dyrygenta"), "the panel: {text}");
            assert!(
                text.contains("dyrygent proponuje: Uruchom krok od nowa"),
                "{text}"
            );
            assert!(
                text.contains("(propozycja: czeka na Ciebie w panelu na dole)"),
                "the tab says where it is answered: {text}"
            );
            let answer = match press(&mut app, KeyCode::Enter) {
                Request::Answer(answer) => *answer,
                other => panic!("an answer was expected, got {other:?}"),
            };
            assert_eq!(
                answer.submit,
                Submit::Operator(OperatorCommand::ConfirmProposal {
                    proposal_id: "p1".to_string()
                })
            );
            ch(&mut app, '3');
            let reject = match press(&mut app, KeyCode::Enter) {
                Request::Answer(answer) => *answer,
                other => panic!("an answer was expected, got {other:?}"),
            };
            assert_eq!(
                reject.submit,
                Submit::Operator(OperatorCommand::RejectProposal {
                    proposal_id: "p1".to_string()
                })
            );
        }

        #[test]
        fn one_read_serves_the_tab_and_the_panel() {
            let mut app = waiting_app();
            let revision = revision(&app);
            assert!(conductor(&app).needs_read(revision, Instant::now()));
            read(&mut app, proposal_view());
            assert!(!conductor(&app).needs_read(revision, Instant::now()));
            let panel = app.detail().view("a").unwrap().waiting();
            assert!(
                !panel.needs_conductor(revision),
                "the panel did not need a second read"
            );
        }

        #[test]
        fn a_settled_proposal_says_so_in_the_tab() {
            let mut app = tab_app();
            let mut view = proposal_view();
            view.proposals[0].state = narsor_protocol::controller::ProposalState::Executed;
            read(&mut app, view);
            assert!(screen(&mut app, 120, 30).contains("(propozycja: wykonana)"));
        }
    }

    // -- no conductor, nothing to read -------------------------------------------

    mod unavailable {
        use super::*;

        fn app_of(task: TaskSummary) -> TaskApp {
            let mut app = app();
            app.load(listing(vec![titled(task, "NAR-12")], "c"));
            press(&mut app, KeyCode::Enter);
            app.detail_mut().set_tab(DetailTab::Conductor);
            app
        }

        #[test]
        fn a_task_whose_runner_has_no_conductor_shows_why_and_has_no_box() {
            let mut app = app_of(without_conductor(
                running("a", 1),
                "brak harnessu dyrygenta",
            ));
            let text = screen(&mut app, 120, 30);
            assert!(
                text.contains("dyrygent niedostępny: brak harnessu dyrygenta"),
                "{text}"
            );
            assert!(!text.contains("c napisz"), "no box, no hint for it: {text}");
            ch(&mut app, 'c');
            assert!(!conductor(&app).is_typing());
            assert!(
                note(&app).contains("brak harnessu dyrygenta"),
                "{}",
                note(&app)
            );
            ch(&mut app, 'h');
            assert!(app.overlay().is_none());
            assert!(
                note(&app).contains("brak harnessu dyrygenta"),
                "{}",
                note(&app)
            );
        }

        #[test]
        fn a_view_that_says_there_is_none_wins_and_its_history_stays_readable() {
            let mut app = tab_app();
            let mut view = conductor_view(
                vec![chat_entry(
                    "m1",
                    1,
                    "conductor",
                    "message",
                    "Stara odpowiedź",
                    None,
                )],
                json!({}),
            );
            view.available = false;
            view.unavailable_reason = Some("the runner is not running".to_string());
            read(&mut app, view);
            let text = screen(&mut app, 120, 30);
            assert!(
                text.contains("dyrygent niedostępny: the runner is not running"),
                "{text}"
            );
            assert!(text.contains("Stara odpowiedź"), "{text}");
            ch(&mut app, 'c');
            assert!(!conductor(&app).is_typing());
        }

        #[test]
        fn a_remote_task_without_a_runner_says_so_with_the_servers_words() {
            let mut app = tab_app();
            read_response(
                &mut app,
                Ok(
                    narsor::controller_protocol::ConductorReadResponse::Unavailable {
                        reason: ConversationUnavailableReason::NotStarted,
                        message: "zadanie nie ma żywego runnera".to_string(),
                    },
                ),
            );
            let text = screen(&mut app, 120, 30);
            assert!(text.contains("zadanie nie ma żywego runnera"), "{text}");
            ch(&mut app, 'c');
            assert!(!conductor(&app).is_typing());
        }

        #[test]
        fn a_503_with_a_reason_is_a_message_in_the_tab_and_not_a_lost_connection() {
            let mut app = tab_app();
            read_response(
                &mut app,
                Err(ClientError::Degraded {
                    code: "runner_timeout".to_string(),
                    message: "runner nie odpowiedział na czas".to_string(),
                }),
            );
            let text = screen(&mut app, 120, 30);
            assert!(
                text.contains("nie udało się wczytać dyrygenta (runner nie odpowiedział na czas)"),
                "{text}"
            );
            assert!(
                app.connection().is_live(),
                "the relay missing a deadline says nothing against the connection"
            );
            assert_eq!(
                conductor(&app).failure(),
                Some("runner nie odpowiedział na czas")
            );
        }

        #[test]
        fn a_failed_refresh_keeps_what_was_read() {
            let mut app = tab_app();
            read(
                &mut app,
                conductor_view(
                    vec![chat_entry("m1", 1, "conductor", "message", "Zostaje", None)],
                    json!({}),
                ),
            );
            read_response(
                &mut app,
                Err(ClientError::Degraded {
                    code: "runner_timeout".to_string(),
                    message: "runner nie odpowiedział".to_string(),
                }),
            );
            let text = screen(&mut app, 120, 30);
            assert!(text.contains("Zostaje"), "{text}");
            assert!(
                text.contains("nie udało się odświeżyć (runner nie odpowiedział)"),
                "{text}"
            );
            ch(&mut app, 'c');
            assert!(
                conductor(&app).is_typing(),
                "the conductor is still there to write to"
            );
        }

        #[test]
        fn an_archived_task_says_its_storage_went_with_the_archiving() {
            let mut app = app();
            app.load(listing(
                vec![titled(task("a", "local", "archived", 1), "NAR-12")],
                "c",
            ));
            press(&mut app, KeyCode::Enter);
            app.detail_mut().set_tab(DetailTab::Conductor);
            let text = screen(&mut app, 120, 30);
            assert!(
                text.contains("storage usunięty przy archiwizacji"),
                "{text}"
            );
        }

        #[test]
        fn before_the_first_read_the_tab_says_it_is_reading() {
            let mut app = tab_app();
            assert!(screen(&mut app, 120, 30).contains("wczytuję dyrygenta"));
        }

        #[test]
        fn the_tab_keys_say_they_belong_to_the_tab_elsewhere() {
            let mut app = tab_app();
            app.detail_mut().set_tab(DetailTab::Progress);
            for key in ['c', 'u', 'M'] {
                ch(&mut app, key);
                assert!(
                    note(&app).contains("zakładce Dyrygent"),
                    "{key}: {}",
                    note(&app)
                );
                assert!(app.overlay().is_none());
                assert!(app
                    .detail()
                    .view("a")
                    .is_none_or(|view| !view.conductor().is_typing()));
            }
        }
    }

    // -- the harness ----------------------------------------------------------

    mod harness {
        use super::*;

        #[test]
        fn the_tab_names_the_harness_and_h_is_the_global_choice_not_the_tabs() {
            let mut app = tab_app();
            read(&mut app, conductor_view(vec![], json!({})));
            let text = screen(&mut app, 120, 30);
            assert!(text.contains(" · harness claude-code"), "{text}");
            assert!(text.contains("c napisz"), "{text}");
            assert!(!text.contains("h harness"), "{text}");
            assert_eq!(
                keymap::lookup(Context::Details, &KeyEvent::from(KeyCode::Char('h'))),
                Some(Action::ChooseHarness)
            );
            // Not a command to the task's conductor: the installation's default.
            assert_eq!(ch(&mut app, 'h'), Request::OpenHarness);
        }

        /// A command from before the per-task choice was retired still shows
        /// its verdict.
        #[test]
        fn a_runner_that_has_no_conductor_refuses_and_the_bar_gives_its_reason() {
            let mut app = tab_app();
            for (cursor, sequence, kind) in [
                (
                    "d",
                    2,
                    TaskEventKind::OperatorCommandQueued {
                        command_id: "command-1".to_string(),
                        command: OperatorCommand::SetConductorHarness {
                            harness: "codex".to_string(),
                        },
                        requested_by: OwnerRef::new("local"),
                    },
                ),
                (
                    "e",
                    3,
                    TaskEventKind::OperatorCommandResolved {
                        command_id: "command-1".to_string(),
                        status: narsor_engine::orchestration::OperatorCommandStatus::Rejected,
                        detail: Some("no conductor runs for this task".to_string()),
                    },
                ),
            ] {
                app.on_stream(event(cursor, "a", sequence, RunnerState::Running, kind));
            }
            let text = note(&app);
            assert!(
                text.contains("zmiana harnessu dyrygenta odrzucone przez runnera"),
                "{text}"
            );
            assert!(text.contains("no conductor runs for this task"), "{text}");
        }
    }

    // -- paging ----------------------------------------------------------------

    mod paging {
        use super::*;

        fn page(from: u64, to: u64, more: bool) -> ConductorView {
            conductor_view(
                (from..=to)
                    .map(|sequence| {
                        chat_entry(
                            &format!("m{sequence}"),
                            sequence,
                            "conductor",
                            "message",
                            &format!("wpis {sequence}"),
                            None,
                        )
                    })
                    .collect(),
                json!({ "chat_has_more": more }),
            )
        }

        #[test]
        fn a_history_that_goes_on_is_continued_after_the_last_entry_with_m() {
            let mut app = tab_app();
            read(&mut app, page(1, 2, true));
            assert!(conductor(&app).has_more());
            assert_eq!(conductor(&app).last_sequence(), 2);
            let revision = revision(&app);
            assert!(!conductor(&app).needs_read(revision, Instant::now()));
            assert!(screen(&mut app, 120, 30).contains("M dalsze wpisy"));

            ch(&mut app, 'M');
            assert!(conductor(&app).wants_more());
            assert!(
                conductor(&app).needs_read(revision, Instant::now()),
                "the same revision is read on"
            );

            // The next page overlaps the last entry: it is not shown twice.
            read(&mut app, page(2, 3, false));
            assert_eq!(conductor(&app).items().len(), 3);
            assert_eq!(conductor(&app).last_sequence(), 3);
            assert!(!conductor(&app).has_more());
            assert!(!conductor(&app).wants_more());
            assert!(!screen(&mut app, 120, 30).contains("M dalsze wpisy"));

            ch(&mut app, 'M');
            assert!(note(&app).contains("to cały czat"), "{}", note(&app));
        }

        #[test]
        fn while_the_conductor_owes_an_answer_the_tab_is_read_again_after_a_moment() {
            let mut app = tab_app();
            read(
                &mut app,
                conductor_view(
                    vec![chat_entry("m1", 1, "operator", "message", "Hej", None)],
                    json!({ "unanswered_chat": ["m1"] }),
                ),
            );
            let revision = revision(&app);
            let now = Instant::now();
            assert!(!conductor(&app).needs_read(revision, now));
            assert!(conductor(&app).needs_read(revision, now + Duration::from_secs(3)));
        }

        #[test]
        fn a_refresh_makes_the_tab_due_without_forgetting_the_history() {
            let mut app = tab_app();
            read(&mut app, page(1, 2, false));
            let revision = revision(&app);
            assert!(!conductor(&app).needs_read(revision, Instant::now()));
            app.load(listing(vec![titled(running("a", 1), "NAR-12")], "d"));
            assert!(conductor(&app).needs_read(revision, Instant::now()));
            assert_eq!(conductor(&app).items().len(), 2);
        }
    }

    // -- keys --------------------------------------------------------------------

    #[test]
    fn the_keys_of_the_tab_are_in_the_key_map_and_in_help() {
        assert_eq!(keymap::check_consistency(keymap::BINDINGS), Ok(()));
        for (action, key) in [
            (Action::Compose, "c"),
            (Action::UndoDecision, "u"),
            (Action::LoadMore, "M"),
        ] {
            assert_eq!(
                keymap::key_of(Context::Details, action).as_deref(),
                Some(key)
            );
            assert!(
                keymap::help_entries(Context::Details)
                    .iter()
                    .any(|entry| entry.keys == key && entry.description.contains("Dyrygent")),
                "{action:?} is in the help of the details"
            );
        }
    }

    // -- against a served control plane -------------------------------------------
}

/// U13: the new-task form, the history and the repositories.
mod task_app_create {

    use std::time::Instant;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::json;

    use narsor::api::{ApiClient, ClientError};
    use narsor::tui::keymap::{self, Action, Context};
    use narsor::tui::task_app::history::HistoryEntry;
    use narsor::tui::task_app::list::PendingState;
    use narsor::tui::task_app::new_task::{
        absolute_path, CreateHow, PathVerdict, DRAFT_KEY, PATH_FIELD, PATH_LABEL, RETRY_AFTER,
        TICKET_FIELD, TITLE_FIELD,
    };
    use narsor::tui::task_app::notify::Severity;
    use narsor::tui::task_app::{DetailTab, Request, TaskApp, ViewKind};
    use narsor_engine::controller_protocol::{
        RepositoryListResponse, TaskSummary, ValidateWorkflowResponse, WorkflowListResponse,
    };
    use narsor_engine::domain::ParamType;

    use narsor_protocol::api::LegacyExecutionListResponse;

    use super::task_app_support::{
        archived, capabilities, catalogue, catalogue_workflow, legacy, listing, queued, registry,
        repository, titled, with_workspaces, workflow_input, workspace,
    };
    use narsor_protocol::controller::WorkspaceRequest;

    fn offline_client() -> ApiClient {
        ApiClient::new("127.0.0.1:9".parse().unwrap(), "token")
    }

    /// An application with a live connection to nothing: what it is handed is
    /// all it knows.
    fn app_with(tasks: Vec<TaskSummary>) -> TaskApp {
        let mut app = TaskApp::new(offline_client(), "local", None);
        app.set_capabilities(capabilities(true, None));
        app.load(listing(tasks, "c"));
        app
    }

    /// One key. An `Enter` that asks for the confirmation of a send is
    /// answered with the `Enter` of the window, so a test that says "Enter
    /// sends" still reads so; the window itself is tested with `on_key`.
    fn press(app: &mut TaskApp, code: KeyCode) -> Request {
        let request = app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
        if code == KeyCode::Enter && request == Request::None && app.form_confirming() {
            return app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        }
        request
    }

    fn ch(app: &mut TaskApp, character: char) -> Request {
        press(app, KeyCode::Char(character))
    }

    fn typed(app: &mut TaskApp, text: &str) {
        for character in text.chars() {
            ch(app, character);
        }
    }

    fn note(app: &TaskApp) -> String {
        app.notifier()
            .latest()
            .map(|note| note.text.clone())
            .unwrap_or_default()
    }

    fn workflows() -> WorkflowListResponse {
        catalogue(vec![
            with_workspaces(
                catalogue_workflow(
                    "implement-feature",
                    "Pełny cykl: plan, implementacja, review, testy i PR.",
                    vec![
                        workflow_input(
                            "ticket_id",
                            ParamType::String,
                            true,
                            Some("numer zgłoszenia"),
                        ),
                        workflow_input(
                            "attempts",
                            ParamType::Integer,
                            false,
                            Some("ile razy ponawiać"),
                        ),
                        workflow_input("dry_run", ParamType::Boolean, false, None),
                    ],
                ),
                vec![workspace("target_repo", None)],
            ),
            catalogue_workflow(
                "fix-bug",
                "Poprawka błędu.",
                vec![workflow_input("ticket_id", ParamType::String, true, None)],
            ),
        ])
    }

    fn narsor_registry() -> RepositoryListResponse {
        registry(vec![
            repository("docs", "git@github.com:acme/docs.git"),
            repository("narsor", "git@github.com:acme/narsor.git"),
        ])
    }

    /// `n`, with the answers of the server given by hand.
    fn open_form(app: &mut TaskApp) {
        assert_eq!(ch(app, 'n'), Request::OpenNewTask);
        app.open_new_task(workflows(), narsor_registry());
        assert_eq!(app.view_kind(), ViewKind::NewTask);
    }

    fn chooser_item<'a>(app: &'a TaskApp, label: &str) -> &'a narsor::tui::chooser::ChooserItem {
        app.new_task_form()
            .expect("the form is open")
            .chooser()
            .items()
            .iter()
            .find(|item| item.label == label)
            .unwrap_or_else(|| panic!("{label} is not in the form"))
    }

    /// Moves the highlight of the form's list to `label`.
    fn choose(app: &mut TaskApp, label: &str) {
        for _ in 0..12 {
            let at = app.new_task_form().unwrap().chooser();
            if at.selected_item().is_some_and(|item| item.label == label) {
                return;
            }
            ch(app, 'j');
        }
        panic!("{label} was not reached");
    }

    /// Types the workflow's `ticket_id`. The form opens on its own ticket;
    /// `ticket_id` is the fifth field, after the title, the repository and its
    /// branch.
    fn fill_ticket(app: &mut TaskApp, ticket: &str) {
        press(app, KeyCode::Enter);
        for _ in 0..4 {
            press(app, KeyCode::Down);
        }
        typed(app, ticket);
    }

    fn enter_until_send(app: &mut TaskApp) -> Request {
        for _ in 0..8 {
            let request = press(app, KeyCode::Enter);
            if request != Request::None {
                return request;
            }
        }
        Request::None
    }

    fn field_value(app: &TaskApp, label: &str, field: &str) -> String {
        let chooser = app.new_task_form().unwrap().chooser();
        let index = chooser
            .items()
            .iter()
            .position(|item| item.label == label)
            .unwrap();
        chooser
            .fields_of(index)
            .iter()
            .find(|candidate| candidate.name == field)
            .unwrap_or_else(|| panic!("{field} is not a field of {label}"))
            .value()
            .to_string()
    }

    fn screen(app: &mut TaskApp, width: u16, height: u16) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                narsor::tui::task_app::layout::render(
                    frame,
                    app,
                    &narsor::tui::style::Styles::new(false),
                    Instant::now(),
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer
            .content()
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn rows(app: &TaskApp) -> Vec<(char, String, String)> {
        app.list()
            .rows()
            .into_iter()
            .map(|row| (row.icon.glyph(), row.title, row.label))
            .collect()
    }

    // -- the form -------------------------------------------------------------

    mod the_form {
        use super::*;

        #[test]
        fn n_opens_the_catalogue_with_the_path_last_and_leaves_the_list_alone() {
            let mut app = app_with(vec![
                titled(queued("a", 1), "NAR-1"),
                titled(queued("b", 2), "NAR-2"),
            ]);
            let before = rows(&app);
            open_form(&mut app);
            let labels: Vec<&str> = app
                .new_task_form()
                .unwrap()
                .chooser()
                .items()
                .iter()
                .map(|item| item.label.as_str())
                .collect();
            assert_eq!(labels, ["implement-feature", "fix-bug", PATH_LABEL]);
            assert_eq!(rows(&app), before, "the list is not touched");
            assert_eq!(app.list().selected_id(), Some("a"));
            assert!(chooser_item(&app, "implement-feature")
                .description
                .contains("Pełny cykl"));
        }

        #[test]
        fn the_fields_are_the_inputs_of_the_highlighted_workflow_with_their_descriptions() {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            let item = chooser_item(&app, "implement-feature");
            let names: Vec<&str> = item
                .fields
                .iter()
                .map(|field| field.name.as_str())
                .collect();
            assert_eq!(
                names,
                [
                    TICKET_FIELD,
                    TITLE_FIELD,
                    "target_repo · repozytorium",
                    "target_repo · gałąź",
                    "ticket_id",
                    "attempts",
                    "dry_run",
                ]
            );
            assert!(item.fields[4].required);
            assert_eq!(
                item.fields[4].description.as_deref(),
                Some("numer zgłoszenia")
            );
            assert!(!item.fields[5].required);
            assert_eq!(
                item.fields[5].description.as_deref(),
                Some("liczba całkowita · ile razy ponawiać")
            );
        }

        /// The names of the fields of a workflow with `inputs`, as the form
        /// of a new task lists them.
        fn field_names(
            inputs: Vec<narsor_engine::controller_protocol::WorkflowInputView>,
            workspaces: &[&str],
        ) -> Vec<String> {
            let mut app = app_with(vec![]);
            ch(&mut app, 'n');
            let workspaces = workspaces
                .iter()
                .map(|name| workspace(name, None))
                .collect();
            app.open_new_task(
                catalogue(vec![with_workspaces(
                    catalogue_workflow("flow", "Opis.", inputs),
                    workspaces,
                )]),
                narsor_registry(),
            );
            chooser_item(&app, "flow")
                .fields
                .iter()
                .map(|field| field.name.clone())
                .collect()
        }

        #[test]
        fn the_main_text_field_is_last_and_the_workspaces_follow_the_title_in_file_order() {
            let names = field_names(
                vec![
                    workflow_input("prompt", ParamType::Text, true, None),
                    workflow_input("dry_run", ParamType::Boolean, false, None),
                ],
                &["code", "docs"],
            );
            assert_eq!(
                names,
                [
                    "Ticket",
                    "Tytuł",
                    "code · repozytorium",
                    "code · gałąź",
                    "docs · repozytorium",
                    "docs · gałąź",
                    "dry_run",
                    "prompt"
                ]
            );
        }

        #[test]
        fn a_workflow_without_a_repository_or_a_text_field_leaves_no_gap() {
            assert_eq!(
                field_names(
                    vec![
                        workflow_input("dry_run", ParamType::Boolean, false, None),
                        workflow_input("prompt", ParamType::Text, false, None),
                    ],
                    &[],
                ),
                ["Ticket", "Tytuł", "dry_run", "prompt"]
            );
            assert_eq!(
                field_names(
                    vec![workflow_input("count", ParamType::Integer, false, None)],
                    &["repository"],
                ),
                [
                    "Ticket",
                    "Tytuł",
                    "repository · repozytorium",
                    "repository · gałąź",
                    "count"
                ]
            );
        }

        #[test]
        fn the_required_text_field_is_the_main_one_and_ticket_id_is_an_ordinary_input() {
            assert_eq!(
                field_names(
                    vec![
                        workflow_input("notes", ParamType::Text, false, None),
                        workflow_input("prompt", ParamType::Text, true, None),
                        workflow_input("ticket_id", ParamType::String, false, None),
                    ],
                    &[],
                ),
                ["Ticket", "Tytuł", "notes", "ticket_id", "prompt"]
            );
        }

        #[test]
        fn the_ticket_takes_fifteen_characters_and_the_title_fifty() {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            press(&mut app, KeyCode::Enter);
            typed(&mut app, &"ą".repeat(20));
            assert_eq!(
                field_value(&app, "implement-feature", TICKET_FIELD),
                "ą".repeat(15)
            );
            press(&mut app, KeyCode::Down);
            typed(&mut app, &"b".repeat(80));
            assert_eq!(
                field_value(&app, "implement-feature", TITLE_FIELD),
                "b".repeat(50)
            );
            assert_eq!(narsor_protocol::display::GENERATED_TITLE_MAX_CHARS, 50);
            assert_eq!(
                narsor_engine::domain::GENERATED_TITLE_MAX_CHARS,
                narsor_protocol::display::GENERATED_TITLE_MAX_CHARS
            );
        }

        #[test]
        fn a_workflow_input_named_like_the_ticket_has_no_limit() {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            press(&mut app, KeyCode::Enter);
            for _ in 0..4 {
                press(&mut app, KeyCode::Down);
            }
            typed(&mut app, &"x".repeat(40));
            assert_eq!(
                field_value(&app, "implement-feature", "ticket_id").len(),
                40
            );
        }

        #[test]
        fn a_boolean_is_a_choice_that_starts_on_false_and_a_repository_offers_the_registered_ones()
        {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            let item = chooser_item(&app, "implement-feature");
            assert_eq!(
                item.fields[6].kind,
                narsor::tui::chooser::FieldKind::Select(vec!["true".into(), "false".into()])
            );
            assert_eq!(item.fields[6].value(), "false");
            assert_eq!(
                item.fields[2].kind,
                narsor::tui::chooser::FieldKind::Select(vec!["docs".into(), "narsor".into()])
            );
            let offered: Vec<&str> = app
                .new_task_form()
                .unwrap()
                .repositories()
                .iter()
                .map(|repository| repository.id.as_str())
                .collect();
            assert_eq!(offered, ["docs", "narsor"]);
        }

        #[test]
        fn a_repository_is_a_free_line_while_none_is_registered() {
            let mut app = app_with(vec![]);
            ch(&mut app, 'n');
            app.open_new_task(workflows(), registry(vec![]));
            let item = chooser_item(&app, "implement-feature");
            assert!(matches!(
                item.fields[2].kind,
                narsor::tui::chooser::FieldKind::Text { .. }
            ));
            assert!(
                matches!(
                    item.fields[3].kind,
                    narsor::tui::chooser::FieldKind::Suggest(_)
                ),
                "a typed repository still has a branch field"
            );
        }

        #[test]
        fn a_missing_required_input_is_an_error_at_the_field_and_nothing_is_sent() {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            assert_eq!(enter_until_send(&mut app), Request::None);
            for _ in 0..4 {
                assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
            }
            let item = chooser_item(&app, "implement-feature");
            assert!(
                item.fields[4]
                    .error()
                    .is_some_and(|error| error.contains("ticket_id")),
                "{:?}",
                item.fields[4].error()
            );
            assert_eq!(app.view_kind(), ViewKind::NewTask);
            assert!(app.outgoing().is_empty());
        }

        #[test]
        fn a_wrong_integer_is_an_error_at_the_field_and_nothing_is_sent() {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            fill_ticket(&mut app, "NAR-18");
            press(&mut app, KeyCode::Down);
            typed(&mut app, "1-2");
            assert_eq!(enter_until_send(&mut app), Request::None);
            let item = chooser_item(&app, "implement-feature");
            assert!(
                item.fields[5]
                    .error()
                    .is_some_and(|error| error.contains("attempts") && error.contains("liczby")),
                "{:?}",
                item.fields[5].error()
            );
            assert!(item.fields[4].error().is_none());
            assert_eq!(app.view_kind(), ViewKind::NewTask);
            assert!(app.outgoing().is_empty());
            assert!(app.list().pending().is_empty());
        }

        #[test]
        fn going_back_to_the_list_and_into_the_workflow_again_keeps_what_was_typed() {
            let mut app = app_with(vec![titled(queued("a", 1), "NAR-1")]);
            open_form(&mut app);
            fill_ticket(&mut app, "NAR-18");
            // `Shift+Tab` returns to the adjacent panel and preserves the form.
            press(&mut app, KeyCode::BackTab);
            assert_eq!(app.view_kind(), ViewKind::NewTask);
            assert_eq!(app.focus(), narsor::tui::task_app::Focus::List);
            assert_eq!(
                field_value(&app, "implement-feature", "ticket_id"),
                "NAR-18"
            );
            press(&mut app, KeyCode::Tab);
            assert_eq!(
                app.new_task_form().unwrap().chooser().focus(),
                narsor::tui::chooser::ChooserFocus::Fields,
                "Enter on a workflow goes into its fields"
            );
            assert_eq!(
                field_value(&app, "implement-feature", "ticket_id"),
                "NAR-18"
            );
        }

        #[test]
        fn arrows_walk_fields_and_tab_switches_to_the_adjacent_panel() {
            let mut app = app_with(vec![titled(queued("a", 1), "NAR-1")]);
            open_form(&mut app);
            fill_ticket(&mut app, "NAR-18");
            press(&mut app, KeyCode::Down);
            typed(&mut app, "1-2");
            assert_eq!(field_value(&app, "implement-feature", "attempts"), "1-2");
            press(&mut app, KeyCode::Tab);
            assert_eq!(app.focus(), narsor::tui::task_app::Focus::List);
            assert_eq!(field_value(&app, "implement-feature", "attempts"), "1-2");
            press(&mut app, KeyCode::Tab);
            assert_eq!(app.focus(), narsor::tui::task_app::Focus::Details);
        }

        #[test]
        fn esc_gives_the_selected_task_back_and_the_next_n_brings_the_data_back() {
            let mut app = app_with(vec![
                titled(queued("a", 1), "NAR-1"),
                titled(queued("b", 2), "NAR-2"),
            ]);
            ch(&mut app, 'g');
            open_form(&mut app);
            fill_ticket(&mut app, "NAR-18");
            assert!(app.detail().has_drafts(), "typed data is an unsent draft");
            assert!(app
                .detail()
                .draft(DRAFT_KEY)
                .is_some_and(|text| text.contains("NAR-18")));

            assert_eq!(
                press(&mut app, KeyCode::Esc),
                Request::None,
                "from a field Esc goes back to the list of workflows"
            );
            assert_eq!(app.view_kind(), ViewKind::NewTask);
            assert_eq!(press(&mut app, KeyCode::Esc), Request::None);
            assert_eq!(app.view_kind(), ViewKind::Tasks);
            assert_eq!(
                app.list().selected_id(),
                Some("a"),
                "the task selected before is back"
            );
            assert!(app.remembered_new_task().is_some());

            // `q` asks before throwing the data away, and Esc keeps it.
            ch(&mut app, 'q');
            assert!(!app.should_quit());
            let asked = screen(&mut app, 120, 30);
            assert!(
                asked.contains("Niewysłany szkic zostanie utracony."),
                "{asked}"
            );
            press(&mut app, KeyCode::Esc);
            assert!(!app.should_quit());

            ch(&mut app, 'n');
            app.open_new_task(workflows(), narsor_registry());
            assert_eq!(
                field_value(&app, "implement-feature", "ticket_id"),
                "NAR-18"
            );
            assert!(
                app.remembered_new_task().is_none(),
                "it is the open form now"
            );
        }

        #[test]
        fn the_form_opens_with_the_focus_on_the_details_and_esc_gives_the_focus_back() {
            let mut app = app_with(vec![titled(queued("a", 1), "NAR-1")]);
            open_form(&mut app);
            assert_eq!(app.focus(), narsor::tui::task_app::Focus::Details);
            press(&mut app, KeyCode::Esc);
            assert_eq!(app.focus(), narsor::tui::task_app::Focus::List);
        }

        #[test]
        fn tab_switches_panels_without_losing_a_new_task_draft() {
            let mut app = app_with(vec![titled(queued("a", 1), "NAR-1")]);
            open_form(&mut app);
            fill_ticket(&mut app, "NAR-18");
            press(&mut app, KeyCode::Tab);
            assert_eq!(app.focus(), narsor::tui::task_app::Focus::List);
            assert_eq!(app.view_kind(), ViewKind::NewTask);
            assert_eq!(
                field_value(&app, "implement-feature", "ticket_id"),
                "NAR-18"
            );
            ch(&mut app, 'H');
            assert_eq!(app.view_kind(), ViewKind::NewTask);
            assert_eq!(
                field_value(&app, "implement-feature", "ticket_id"),
                "NAR-18",
                "a list shortcut cannot discard the form"
            );
            press(&mut app, KeyCode::Tab);
            assert_eq!(app.focus(), narsor::tui::task_app::Focus::Details);
            assert_eq!(app.key_context().base, Context::Form);
            assert_eq!(
                field_value(&app, "implement-feature", "ticket_id"),
                "NAR-18"
            );
        }

        #[test]
        fn keys_in_the_form_are_the_choosers_and_q_is_text_in_a_field() {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            assert_eq!(app.key_context().base, Context::Panel);
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.key_context().base, Context::Form);
            ch(&mut app, 'q');
            assert!(!app.should_quit());
            assert_eq!(field_value(&app, "implement-feature", TICKET_FIELD), "q");
        }

        #[test]
        fn a_closed_catalogue_with_no_answer_says_so_and_opens_nothing() {
            let mut app = app_with(vec![]);
            // Nothing listens where the client points.
            let request = ch(&mut app, 'n');
            assert_eq!(request, Request::OpenNewTask);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(app.perform(request));
            assert_eq!(app.view_kind(), ViewKind::Tasks);
            assert!(
                note(&app).contains("nie udało się wczytać katalogu"),
                "{}",
                note(&app)
            );
        }
    }

    // -- a path ----------------------------------------------------------------

    mod a_workflow_path {
        use super::*;

        #[test]
        fn a_server_that_does_not_read_files_greys_the_path_out_with_its_reason() {
            let mut app = app_with(vec![]);
            app.set_capabilities(capabilities(
                false,
                Some("serwer zdalny nie czyta plików z dysku"),
            ));
            open_form(&mut app);
            let item = chooser_item(&app, PATH_LABEL);
            assert_eq!(
                item.unavailable.as_deref(),
                Some("serwer zdalny nie czyta plików z dysku")
            );
            choose(&mut app, PATH_LABEL);
            assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
            assert!(
                note(&app).contains("serwer zdalny nie czyta plików z dysku"),
                "{}",
                note(&app)
            );
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Warning);
        }

        #[test]
        fn without_a_reason_of_its_own_the_path_still_says_why() {
            let mut app = app_with(vec![]);
            app.set_capabilities(capabilities(false, None));
            open_form(&mut app);
            assert!(chooser_item(&app, PATH_LABEL)
                .unavailable
                .as_deref()
                .is_some_and(|reason| reason.contains("nie czyta plików")));
        }

        #[test]
        fn a_local_daemon_offers_the_path() {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            assert_eq!(chooser_item(&app, PATH_LABEL).unavailable, None);
        }

        #[test]
        fn a_relative_path_is_sent_as_an_absolute_one() {
            let absolute = absolute_path("workflows/x.yaml");
            assert!(std::path::Path::new(&absolute).is_absolute(), "{absolute}");
            assert!(absolute.ends_with("workflows/x.yaml"));
            assert_eq!(absolute_path("/srv/x.yaml"), "/srv/x.yaml");
            let home = std::env::var("HOME").unwrap_or_default();
            if !home.is_empty() {
                assert_eq!(absolute_path("~/x.yaml"), format!("{home}/x.yaml"));
            }
        }

        #[test]
        fn enter_on_a_path_asks_the_server_to_check_it_first() {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            choose(&mut app, PATH_LABEL);
            press(&mut app, KeyCode::Enter);
            typed(&mut app, "/srv/workflows/wf.yaml");
            assert_eq!(
                press(&mut app, KeyCode::Enter),
                Request::ValidatePath("/srv/workflows/wf.yaml".to_string())
            );
            assert!(
                app.outgoing().is_empty(),
                "nothing is created before the check"
            );
        }

        #[test]
        fn every_violation_is_in_the_description_and_none_is_sent() {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            choose(&mut app, PATH_LABEL);
            press(&mut app, KeyCode::Enter);
            typed(&mut app, "/srv/bad.yaml");
            app.on_path_checked(
                "/srv/bad.yaml",
                Ok(ValidateWorkflowResponse {
                    valid: false,
                    name: None,
                    version: None,
                    state_count: 0,
                    initial_state: None,
                    inputs: vec![],
                    workspaces: vec![],
                    violations: vec![
                        "state 'plan' has no transition for output 'ok'".to_string(),
                        "state 'ship' is not reachable".to_string(),
                        "input 'ticket' is declared twice".to_string(),
                    ],
                }),
            );
            let description = &chooser_item(&app, PATH_LABEL).description;
            for violation in [
                "state 'plan' has no transition for output 'ok'",
                "state 'ship' is not reachable",
                "input 'ticket' is declared twice",
            ] {
                assert!(description.contains(violation), "{description}");
            }
            assert!(description.contains("naruszeń: 3"), "{description}");
            let error = app.new_task_form().unwrap().chooser().fields_of(2)[0]
                .error()
                .map(str::to_string);
            assert!(error.is_some_and(|error| error.contains('3')));
            assert!(matches!(
                app.new_task_form().unwrap().path_check().unwrap().verdict,
                PathVerdict::Invalid(_)
            ));
            assert!(app.outgoing().is_empty());
        }

        #[test]
        fn a_definition_that_loads_brings_its_inputs_and_enter_then_creates_the_task() {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            choose(&mut app, PATH_LABEL);
            press(&mut app, KeyCode::Enter);
            typed(&mut app, "/srv/ok.yaml");
            app.on_path_checked(
                "/srv/ok.yaml",
                Ok(ValidateWorkflowResponse {
                    valid: true,
                    name: Some("my-flow".to_string()),
                    version: Some(4),
                    state_count: 2,
                    initial_state: Some("start".to_string()),
                    inputs: vec![workflow_input("ticket", ParamType::String, true, None)],
                    workspaces: vec![],
                    violations: vec![],
                }),
            );
            let item = chooser_item(&app, PATH_LABEL);
            let names: Vec<&str> = item
                .fields
                .iter()
                .map(|field| field.name.as_str())
                .collect();
            assert_eq!(names, [PATH_FIELD, TICKET_FIELD, TITLE_FIELD, "ticket"]);
            assert!(item.description.contains("my-flow v4"));
            assert_eq!(
                field_value(&app, PATH_LABEL, PATH_FIELD),
                "/srv/ok.yaml",
                "what was typed stays"
            );

            // Past the form's own ticket and title fields.
            for _ in 0..3 {
                press(&mut app, KeyCode::Down);
            }
            typed(&mut app, "NAR-3");
            let Request::SendTask(id) = press(&mut app, KeyCode::Enter) else {
                panic!("the task should be sent");
            };
            let outgoing = &app.outgoing()[0];
            assert_eq!(outgoing.task_id, id);
            assert_eq!(outgoing.title, "NAR-3");
            assert_eq!(
                outgoing.send,
                CreateHow::Path {
                    workflow: "/srv/ok.yaml".to_string(),
                    inputs: json!({ "ticket": "NAR-3" }).as_object().unwrap().clone(),
                    workspaces: Default::default(),
                    ticket: None,
                    title: None,
                }
            );
        }

        #[test]
        fn a_path_that_could_not_be_checked_says_why_in_the_description() {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            choose(&mut app, PATH_LABEL);
            press(&mut app, KeyCode::Enter);
            typed(&mut app, "/nope.yaml");
            app.on_path_checked(
                "/nope.yaml",
                Err(ClientError::Request {
                    code: "workflow_not_found".to_string(),
                    message: "there is no workflow file at that path".to_string(),
                }),
            );
            assert!(chooser_item(&app, PATH_LABEL)
                .description
                .contains("there is no workflow file at that path"));
        }
    }

    // -- sending ---------------------------------------------------------------

    mod sending {
        use super::*;

        fn filled() -> TaskApp {
            let mut app = app_with(vec![
                titled(queued("a", 1), "NAR-1"),
                titled(queued("b", 2), "NAR-2"),
            ]);
            ch(&mut app, 'g');
            open_form(&mut app);
            fill_ticket(&mut app, "NAR-18");
            press(&mut app, KeyCode::Down);
            typed(&mut app, "3");
            app
        }

        fn submit_filled(app: &mut TaskApp) -> Request {
            super::enter_until_send(app)
        }

        #[test]
        fn enter_makes_the_id_first_and_the_row_goes_last_and_is_selected() {
            let mut app = filled();
            let before = rows(&app);
            let request = super::enter_until_send(&mut app);
            let Request::SendTask(id) = request else {
                panic!("a send was expected");
            };
            assert!(id.starts_with("task-"), "{id}");
            assert_eq!(app.view_kind(), ViewKind::Tasks);
            assert_eq!(app.list().selected_id(), Some(id.as_str()));
            let now = rows(&app);
            assert_eq!(now[..2], before[..], "the rows above are where they were");
            assert_eq!(now.len(), 3);
            assert_eq!(
                now[2],
                ('↻', "NAR-18".to_string(), "wysyłanie…".to_string())
            );
            assert!(
                app.detail().has_drafts(),
                "until the server has it, it is unsent"
            );
        }

        #[test]
        fn the_request_carries_the_id_the_version_and_the_typed_inputs() {
            let mut app = filled();
            ch(&mut app, 'x'); // not a field: ignored once the form closed
            let _ = app;
            let mut app = filled();
            let request = super::enter_until_send(&mut app);
            let Request::SendTask(id) = request else {
                panic!("a send was expected");
            };
            let CreateHow::Catalogue(request) = &app.outgoing()[0].send else {
                panic!("a workflow of the catalogue is created through the control plane");
            };
            assert_eq!(request.task_id.as_deref(), Some(id.as_str()));
            assert_eq!(request.launch.workflow_id, "implement-feature");
            assert_eq!(request.launch.workflow_version, 1);
            assert_eq!(
                request.launch.inputs,
                json!({
                    "ticket_id": "NAR-18",
                    "attempts": 3,
                    "dry_run": false,
                })
            );
            assert_eq!(
                request.launch.workspaces["target_repo"]
                    .repository
                    .as_deref(),
                Some("docs")
            );
        }

        #[test]
        fn the_form_has_optional_ticket_and_title_fields_for_every_workflow() {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            for workflow in ["implement-feature", "fix-bug"] {
                let item = chooser_item(&app, workflow);
                for name in [TICKET_FIELD, TITLE_FIELD] {
                    let field = item.fields.iter().find(|field| field.name == name);
                    assert!(
                        field.is_some_and(|field| !field.required),
                        "{workflow} {name}"
                    );
                }
            }
        }

        #[test]
        fn a_task_made_without_ticket_and_title_sends_neither() {
            let mut app = filled();
            let Request::SendTask(_) = super::enter_until_send(&mut app) else {
                panic!("a send was expected");
            };
            let CreateHow::Catalogue(request) = &app.outgoing()[0].send else {
                panic!("catalogue");
            };
            assert_eq!(
                (&request.launch.ticket, &request.launch.title),
                (&None, &None)
            );
        }

        #[test]
        fn the_typed_ticket_and_title_go_to_the_request_as_typed() {
            let mut app = filled();
            // From `attempts`: up past `ticket_id`, the branch, `target_repo`
            // and the title to the ticket.
            for _ in 0..5 {
                press(&mut app, KeyCode::Up);
            }
            typed(&mut app, "  NAR-12 ");
            press(&mut app, KeyCode::Down);
            typed(&mut app, "Zmiana API zamówień");
            let Request::SendTask(_) = super::enter_until_send(&mut app) else {
                panic!("a send was expected");
            };
            let CreateHow::Catalogue(request) = &app.outgoing()[0].send else {
                panic!("catalogue");
            };
            assert_eq!(request.launch.ticket.as_deref(), Some("  NAR-12 "));
            assert_eq!(request.launch.title.as_deref(), Some("Zmiana API zamówień"));
            assert_eq!(request.launch.inputs["ticket_id"], json!("NAR-18"));
        }

        #[test]
        fn a_boolean_chosen_with_the_arrow_is_sent_as_a_boolean() {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            fill_ticket(&mut app, "NAR-1");
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Right);
            assert_eq!(field_value(&app, "implement-feature", "dry_run"), "true");
            for _ in 0..5 {
                press(&mut app, KeyCode::Enter);
            }
            let CreateHow::Catalogue(request) = &app.outgoing()[0].send else {
                panic!("catalogue");
            };
            assert_eq!(request.launch.inputs["dry_run"], json!(true));
        }

        #[test]
        fn without_a_connection_nothing_is_sent_and_the_form_keeps_what_was_typed() {
            let mut app = TaskApp::new(offline_client(), "local", None);
            app.set_capabilities(capabilities(true, None));
            open_form(&mut app);
            fill_ticket(&mut app, "NAR-18");
            assert_eq!(submit_filled(&mut app), Request::None);
            assert_eq!(app.view_kind(), ViewKind::NewTask, "the form stays");
            assert_eq!(
                field_value(&app, "implement-feature", "ticket_id"),
                "NAR-18"
            );
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Warning);
            assert!(app.outgoing().is_empty());
            assert!(app.list().pending().is_empty());
        }

        #[tokio::test]
        async fn a_lost_answer_is_sent_again_with_the_same_id_and_then_stays_unsent() {
            let mut app = filled();
            let Request::SendTask(id) = submit_filled(&mut app) else {
                panic!("a send was expected");
            };
            app.perform(Request::SendTask(id.clone())).await;
            // The first attempt got nothing: the row says it is being sent
            // again, and the request is waiting to go out again.
            assert_eq!(rows(&app)[2].0, '↻');
            assert_eq!(app.list().pending()[0].state, PendingState::Sending);
            assert!(app.next_outgoing_due().is_some());
            assert!(
                note(&app).contains("ponawiam z tym samym id"),
                "{}",
                note(&app)
            );
            let first = app.outgoing()[0].clone();

            // Not yet due.
            assert!(!app.retry_outgoing_if_due(Instant::now()).await);
            assert_eq!(app.outgoing()[0].attempts(), 1);

            // Due: the same request goes out and gets nothing again.
            assert!(
                app.retry_outgoing_if_due(Instant::now() + RETRY_AFTER * 2)
                    .await
            );
            assert_eq!(app.outgoing()[0].send, first.send, "the same request");
            assert_eq!(app.outgoing()[0].task_id, id, "under the same id");
            assert_eq!(
                rows(&app)[2],
                ('!', "NAR-18".to_string(), "nie wysłano".to_string())
            );
            assert_eq!(app.list().pending()[0].state, PendingState::NotSent);
            assert!(app.next_outgoing_due().is_none(), "no more by itself");
            assert!(app.detail().has_drafts(), "the data is kept");
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Error);
        }

        #[test]
        fn r_on_an_unsent_row_sends_it_again_when_the_connection_allows_and_says_why_not_otherwise()
        {
            let mut app = filled();
            let Request::SendTask(id) = submit_filled(&mut app) else {
                panic!("a send was expected");
            };
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                app.perform(Request::SendTask(id.clone())).await;
                app.retry_outgoing_if_due(Instant::now() + RETRY_AFTER * 2)
                    .await;
            });
            assert_eq!(app.list().pending()[0].state, PendingState::NotSent);

            // The connection is down: the reason is on the bar, nothing is sent.
            assert_eq!(ch(&mut app, 'r'), Request::None);
            assert!(!note(&app).is_empty());
            assert_eq!(app.list().pending()[0].state, PendingState::NotSent);

            // It came back.
            app.load(listing(vec![titled(queued("a", 1), "NAR-1")], "d"));
            assert_eq!(
                app.list().selected_id(),
                Some(id.as_str()),
                "the row keeps the selection"
            );
            assert_eq!(ch(&mut app, 'r'), Request::SendTask(id.clone()));
            assert_eq!(app.list().pending()[0].state, PendingState::Sending);
            assert_eq!(app.outgoing()[0].task_id, id);
        }

        #[test]
        fn r_on_a_task_the_server_has_has_nothing_to_send() {
            let mut app = app_with(vec![titled(queued("a", 1), "NAR-1")]);
            assert_eq!(ch(&mut app, 'r'), Request::None);
            assert!(
                note(&app).contains("nie ma czego ponawiać"),
                "{}",
                note(&app)
            );
        }

        #[test]
        fn the_unsent_row_has_details_that_say_where_it_stands() {
            let mut app = filled();
            let Request::SendTask(id) = submit_filled(&mut app) else {
                panic!("a send was expected");
            };
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                app.perform(Request::SendTask(id.clone())).await;
                app.retry_outgoing_if_due(Instant::now() + RETRY_AFTER * 2)
                    .await;
            });
            let screen = screen(&mut app, 120, 30);
            for expected in [
                "! nie wysłano",
                &id,
                "Dane zostały zachowane",
                "implement-feature",
                "ticket_id = NAR-18",
                "attempts = 3",
            ] {
                assert!(screen.contains(expected), "{expected}:\n{screen}");
            }
        }

        #[test]
        fn what_the_server_lists_is_no_longer_waiting_to_be_confirmed() {
            let mut app = filled();
            let Request::SendTask(id) = submit_filled(&mut app) else {
                panic!("a send was expected");
            };
            // The answer was lost, but the task is there when the list is read.
            app.load(listing(
                vec![
                    titled(queued("a", 1), "NAR-1"),
                    titled(queued("b", 2), "NAR-2"),
                    titled(queued(&id, 3), "NAR-18"),
                ],
                "d",
            ));
            assert!(app.list().pending().is_empty());
            assert!(app.outgoing().is_empty(), "nothing is left to send again");
            assert!(!app.detail().has_drafts(), "and nothing is unsent");
            assert_eq!(rows(&app).len(), 3);
            assert_eq!(app.list().selected_id(), Some(id.as_str()));
            assert_eq!(rows(&app)[2].0, '◷');
        }

        #[test]
        fn the_pending_row_can_be_moved_over_and_the_selection_stays_on_it() {
            let mut app = filled();
            let Request::SendTask(id) = submit_filled(&mut app) else {
                panic!("a send was expected");
            };
            ch(&mut app, 'k');
            assert_eq!(app.list().selected_id(), Some("b"));
            ch(&mut app, 'j');
            assert_eq!(app.list().selected_id(), Some(id.as_str()));
            ch(&mut app, 'G');
            assert_eq!(app.list().selected_id(), Some(id.as_str()));
        }
    }

    // -- the history -----------------------------------------------------------

    mod history {
        use super::*;

        fn opened() -> TaskApp {
            let mut app = app_with(vec![titled(queued("a", 1), "NAR-1")]);
            assert_eq!(ch(&mut app, 'H'), Request::OpenHistory);
            app.open_history(
                listing(
                    vec![archived("old", "NAR-9", 5), archived("older", "NAR-8", 1)],
                    "c",
                ),
                Ok(LegacyExecutionListResponse {
                    executions: vec![
                        legacy("exec-failed", "fix-bug", "failed", 3, true, None),
                        legacy(
                            "exec-done",
                            "fix-bug",
                            "completed",
                            2,
                            false,
                            Some("zakończone — nie ma czego wznawiać"),
                        ),
                    ],
                }),
            );
            assert_eq!(app.view_kind(), ViewKind::History);
            app
        }

        fn titles(app: &TaskApp) -> Vec<String> {
            app.history()
                .unwrap()
                .rows()
                .into_iter()
                .map(|row| row.title)
                .collect()
        }

        #[test]
        fn it_lists_archived_tasks_and_old_executions_newest_first() {
            let app = opened();
            assert_eq!(
                titles(&app),
                ["NAR-9", "fix-bug exec-failed", "fix-bug exec-done", "NAR-8"]
            );
            assert_eq!(app.history().unwrap().selected(), 0);
        }

        #[test]
        fn an_archived_task_has_the_tabs_read_only_and_says_its_storage_is_gone() {
            let mut app = opened();
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.key_context().base, Context::HistoryDetails);
            for (tab, expected) in [
                (DetailTab::Progress, "storage usunięty przy archiwizacji"),
                (
                    DetailTab::Conversation,
                    "storage usunięty przy archiwizacji",
                ),
                (DetailTab::Events, "storage usunięty przy archiwizacji"),
                (DetailTab::Conductor, "storage usunięty przy archiwizacji"),
            ] {
                while app.history().unwrap().tab() != tab {
                    press(&mut app, KeyCode::Char(']'));
                }
                let screen = screen(&mut app, 130, 30);
                assert!(screen.contains(expected), "{tab:?}:\n{screen}");
            }
            // The details are not the ones of the task list.
            let screen = screen(&mut app, 130, 30);
            assert!(screen.contains("NAR-9"), "{screen}");
            assert!(screen.contains("Historia"), "{screen}");
        }

        #[test]
        fn esc_in_the_details_goes_to_the_history_list_and_esc_there_to_the_tasks() {
            let mut app = opened();
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.focus(), narsor::tui::task_app::Focus::Details);
            press(&mut app, KeyCode::Esc);
            assert_eq!(app.view_kind(), ViewKind::History);
            assert_eq!(app.key_context().base, Context::History);
            press(&mut app, KeyCode::Esc);
            assert_eq!(app.view_kind(), ViewKind::Tasks);
            assert_eq!(app.list().selected_id(), Some("a"));
            assert_eq!(app.focus(), narsor::tui::task_app::Focus::List);
        }

        #[test]
        fn recover_is_offered_only_for_an_execution_that_can_be_recovered() {
            let mut app = opened();
            // The archived task: nothing to recover, and why.
            assert_eq!(ch(&mut app, 'w'), Request::None);
            assert!(
                note(&app).contains("storage usunięty przy archiwizacji"),
                "{}",
                note(&app)
            );

            // The failed execution can be.
            ch(&mut app, 'j');
            assert_eq!(
                ch(&mut app, 'w'),
                Request::Recover("exec-failed".to_string())
            );

            // The completed one cannot; the server's reason is on the bar.
            ch(&mut app, 'j');
            assert_eq!(ch(&mut app, 'w'), Request::None);
            assert!(
                note(&app).contains("zakończone — nie ma czego wznawiać"),
                "{}",
                note(&app)
            );
        }

        #[test]
        fn recover_is_blocked_without_a_connection() {
            let mut app = opened();
            ch(&mut app, 'j');
            // The connection broke after the history was read.
            app.on_stream(Some(Err(ClientError::Unavailable("gone".to_string()))));
            assert_eq!(ch(&mut app, 'w'), Request::None);
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Warning);
        }

        #[test]
        fn the_only_action_of_the_history_is_recover_the_task_keys_do_nothing() {
            let mut app = opened();
            for key in ['p', 'x', 't', 's', 'a', 'm', 'o', 'n', 'z', '.', 'R', 'H'] {
                assert_eq!(ch(&mut app, key), Request::None, "{key}");
            }
            assert_eq!(app.view_kind(), ViewKind::History);
            assert!(app.overlay().is_none());
            assert_eq!(app.list().tasks().len(), 1, "the task list is untouched");
        }

        #[test]
        fn an_old_execution_has_a_header_and_tabs_of_its_own() {
            let mut app = opened();
            ch(&mut app, 'j');
            let screen = screen(&mut app, 130, 30);
            assert!(screen.contains("fix-bug exec-failed"), "{screen}");
            assert!(screen.contains("failed"), "{screen}");
            assert!(screen.contains("Przebieg"), "{screen}");
        }

        #[test]
        fn what_the_server_could_not_list_is_said_and_the_archived_tasks_are_still_there() {
            let mut app = app_with(vec![]);
            ch(&mut app, 'H');
            app.open_history(
                listing(vec![archived("old", "NAR-9", 5)], "c"),
                Err(ClientError::Unavailable("timeout".to_string())),
            );
            assert_eq!(titles(&app), ["NAR-9"]);
            assert!(app.history().unwrap().note().is_some());
            assert!(
                note(&app).contains("dawne executions niedostępne"),
                "{}",
                note(&app)
            );
        }

        #[test]
        fn an_empty_history_is_an_empty_list_not_an_error() {
            let mut app = app_with(vec![]);
            ch(&mut app, 'H');
            app.open_history(
                listing(vec![], "c"),
                Ok(LegacyExecutionListResponse { executions: vec![] }),
            );
            assert!(titles(&app).is_empty());
            let screen = screen(&mut app, 120, 30);
            assert!(screen.contains("historia jest pusta"), "{screen}");
        }

        use narsor::tui::keymap::Stack;

        #[test]
        fn brackets_switch_the_tabs_from_the_history_list_and_its_details() {
            let mut app = opened();
            ch(&mut app, ']');
            assert_eq!(app.history().unwrap().tab(), DetailTab::Events);
            press(&mut app, KeyCode::Enter);
            ch(&mut app, ']');
            assert_eq!(app.history().unwrap().tab(), DetailTab::Conductor);
            assert_eq!(
                app.key_context(),
                Stack::layered(Context::Reading, Context::HistoryDetails)
            );
            ch(&mut app, '[');
            ch(&mut app, '[');
            assert_eq!(app.history().unwrap().tab(), DetailTab::Progress);
            assert_eq!(
                app.key_context(),
                Stack::layered(Context::HistoryAxis, Context::HistoryDetails),
                "the timeline of a closed run"
            );
            press(&mut app, KeyCode::Right);
            assert_eq!(app.history().unwrap().tab(), DetailTab::Progress);
            press(&mut app, KeyCode::Esc);
            assert_eq!(app.key_context(), Stack::from(Context::History));
        }

        #[test]
        fn tab_and_shift_tab_switch_history_panels_without_enabling_task_actions() {
            let mut app = opened();
            let selected = app.history().unwrap().selected();
            press(&mut app, KeyCode::Tab);
            assert_eq!(app.focus(), narsor::tui::task_app::Focus::Details);
            assert_eq!(app.key_context().base, Context::HistoryDetails);
            assert_eq!(app.history().unwrap().selected(), selected);
            assert_eq!(
                narsor::tui::keymap::lookup(app.key_context(), &KeyEvent::from(KeyCode::Char('p'))),
                None,
                "history details expose no task mutation actions"
            );
            press(&mut app, KeyCode::BackTab);
            assert_eq!(app.focus(), narsor::tui::task_app::Focus::List);
            assert_eq!(app.key_context().base, Context::History);
            assert_eq!(app.history().unwrap().selected(), selected);
        }

        #[test]
        fn keys_and_help_of_the_history_come_from_the_map() {
            let app = opened();
            assert_eq!(app.key_context().base, Context::History);
            let keys = keymap::hints(Context::History);
            assert!(keys.iter().any(|(_, label)| *label == "wznów"));
            assert_eq!(
                keymap::lookup(
                    Context::History,
                    &KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE)
                ),
                Some(Action::Recover)
            );
            let entry = app.history().unwrap().selected_entry().unwrap();
            assert!(matches!(entry, HistoryEntry::Archived(_)));
        }
    }

    // -- over the wire ---------------------------------------------------------

    mod over_the_wire {}

    // -- the repositories ------------------------------------------------------

    // -- a paste ----------------------------------------------------------------

    mod paste {
        use super::*;

        /// The form over a workflow with a short field and a required prompt,
        /// the keys in the prompt.
        fn prompt_form(app: &mut TaskApp) {
            ch(app, 'n');
            app.open_new_task(
                catalogue(vec![catalogue_workflow(
                    "write",
                    "Pisze.",
                    vec![
                        workflow_input("ticket_id", ParamType::String, false, None),
                        workflow_input("prompt", ParamType::Text, true, None),
                    ],
                )]),
                narsor_registry(),
            );
            press(app, KeyCode::Enter);
            // The focus starts on the first field; the prompt is below it,
            // under the form's own ticket and title fields.
            for _ in 0..3 {
                press(app, KeyCode::Down);
            }
            assert_eq!(app.key_context().base, Context::Form);
        }

        #[test]
        fn lines_pasted_into_the_prompt_stay_lines_and_send_nothing() {
            let mut app = app_with(vec![]);
            prompt_form(&mut app);
            app.on_paste("pierwsza\ndruga\ntrzecia");
            assert_eq!(
                field_value(&app, "write", "prompt"),
                "pierwsza\ndruga\ntrzecia"
            );
            assert_eq!(app.view_kind(), ViewKind::NewTask);
            assert!(app.outgoing().is_empty(), "a paste sends nothing");
            typed(&mut app, "!");
            assert_eq!(
                field_value(&app, "write", "prompt"),
                "pierwsza\ndruga\ntrzecia!",
                "the cursor is after the pasted text"
            );
        }

        #[test]
        fn pasted_lines_can_be_edited_and_enter_sends_them() {
            let mut app = app_with(vec![]);
            prompt_form(&mut app);
            app.on_paste("a\nb\nc");
            press(&mut app, KeyCode::Up);
            typed(&mut app, "X");
            let Request::SendTask(_) = press(&mut app, KeyCode::Enter) else {
                panic!("Enter after a paste still sends");
            };
            let CreateHow::Catalogue(request) = &app.outgoing()[0].send else {
                panic!("a workflow of the catalogue");
            };
            assert_eq!(request.launch.inputs["prompt"], json!("a\nbX\nc"));
        }

        #[test]
        fn a_pasted_prompt_is_a_draft_that_outlives_closing_the_form() {
            let mut app = app_with(vec![]);
            prompt_form(&mut app);
            app.on_paste("długi\nlog");
            assert!(app
                .detail()
                .draft(DRAFT_KEY)
                .is_some_and(|text| text.contains("długi\nlog")));
            press(&mut app, KeyCode::Esc);
            press(&mut app, KeyCode::Esc);
            assert_eq!(app.view_kind(), ViewKind::Tasks);
            prompt_form(&mut app);
            assert_eq!(field_value(&app, "write", "prompt"), "długi\nlog");
        }

        #[test]
        fn a_paste_on_the_list_of_workflows_is_ignored() {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            let before = app.new_task_form().unwrap().chooser().selected();
            app.on_paste("q\nj\n");
            assert_eq!(app.view_kind(), ViewKind::NewTask);
            assert!(!app.should_quit());
            assert_eq!(app.new_task_form().unwrap().chooser().selected(), before);
            assert_eq!(field_value(&app, "implement-feature", "ticket_id"), "");
            assert!(!app.detail().has_drafts());
        }

        #[test]
        fn a_path_pasted_into_the_repository_form_stays_on_one_line() {
            let mut app = app_with(vec![]);
            ch(&mut app, 'R');
            app.open_repositories(narsor_registry());
            ch(&mut app, 'a');
            press(&mut app, KeyCode::Enter);
            app.on_paste("/srv/git/narsor\n");
            let form = app.repositories().unwrap().form().expect("the form");
            assert_eq!(form.fields_of(0)[0].value(), "/srv/git/narsor ");
            assert!(form.is_typing(), "the keys stay in the field");
            assert!(app.detail().has_drafts(), "the pasted text is a draft");
        }

        #[test]
        fn a_paste_on_the_list_of_repositories_or_in_the_history_is_ignored() {
            let mut app = app_with(vec![]);
            ch(&mut app, 'R');
            app.open_repositories(narsor_registry());
            app.on_paste("a\nd\n");
            assert!(app.repositories().unwrap().form().is_none());
            assert_eq!(app.view_kind(), ViewKind::Repositories);

            let mut app = app_with(vec![]);
            ch(&mut app, 'H');
            app.open_history(
                listing(vec![archived("old", "NAR-9", 5)], "c"),
                Ok(LegacyExecutionListResponse { executions: vec![] }),
            );
            app.on_paste("q\nq\n");
            assert_eq!(app.view_kind(), ViewKind::History);
            assert!(!app.should_quit());
        }
    }

    /// The repository and branch of each workspace the workflow leaves to be
    /// chosen: the branch preselected, suggested from the server's list,
    /// typed freely, and sent as it stands.
    mod branch_field {
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};

        use super::*;
        use narsor::api::{BranchRead, BranchSource};
        use narsor::tui::chooser::FieldKind;
        use narsor::tui::input_form::{branch_field_name, repository_field_name};

        /// The branches the server would list, by repository id; a repository
        /// that is not in the map fails to list.
        #[derive(Default)]
        struct Remotes {
            lists: Mutex<HashMap<String, Result<Vec<String>, String>>>,
            reads: Mutex<Vec<String>>,
        }

        impl Remotes {
            fn listing(entries: &[(&str, &[&str])]) -> Arc<Self> {
                let remotes = Arc::new(Self::default());
                for (id, branches) in entries {
                    remotes.set(id, Ok(branches.iter().map(|b| (*b).to_string()).collect()));
                }
                remotes
            }

            fn set(&self, id: &str, answer: Result<Vec<String>, String>) {
                self.lists.lock().unwrap().insert(id.to_string(), answer);
            }

            fn reads(&self) -> Vec<String> {
                self.reads.lock().unwrap().clone()
            }
        }

        impl BranchSource for Remotes {
            fn read_branches(&self, id: &str) -> BranchRead {
                self.reads.lock().unwrap().push(id.to_string());
                let answer = self.lists.lock().unwrap().get(id).cloned();
                Box::pin(async move {
                    match answer {
                        Some(Ok(branches)) => Ok(branches),
                        Some(Err(reason)) => Err(ClientError::Degraded {
                            code: "branches_unavailable".to_string(),
                            message: reason,
                        }),
                        None => Err(ClientError::NotFound),
                    }
                })
            }
        }

        fn branch_of(workspace: &str) -> String {
            branch_field_name(workspace)
        }

        fn repository_of(workspace: &str) -> String {
            repository_field_name(workspace)
        }

        fn repository_with(
            id: &str,
            default_branch: Option<&str>,
        ) -> narsor_protocol::controller::RepositoryView {
            let mut view = repository(id, &format!("https://example.com/{id}.git"));
            view.default_branch = default_branch.map(str::to_string);
            view
        }

        fn app_reading(remotes: &Arc<Remotes>) -> TaskApp {
            app_with(vec![]).with_branch_source(Arc::clone(remotes) as Arc<dyn BranchSource>)
        }

        /// The form opened over `workflows` and `repositories`, with the reads
        /// the opening asks for done.
        async fn opened(
            app: &mut TaskApp,
            workflows: WorkflowListResponse,
            repositories: Vec<narsor_protocol::controller::RepositoryView>,
        ) {
            ch(app, 'n');
            let request = app.open_new_task(workflows, registry(repositories));
            app.perform(request).await;
        }

        async fn open_default(
            app: &mut TaskApp,
            repositories: Vec<narsor_protocol::controller::RepositoryView>,
        ) {
            opened(app, workflows(), repositories).await;
        }

        fn two_workspaces() -> WorkflowListResponse {
            catalogue(vec![with_workspaces(
                catalogue_workflow(
                    "compare",
                    "Porównuje dwa repozytoria.",
                    vec![workflow_input("ticket_id", ParamType::String, true, None)],
                ),
                vec![workspace("code", None), workspace("docs_repo", None)],
            )])
        }

        /// A key, and the reads it asks for.
        async fn key(app: &mut TaskApp, code: KeyCode) {
            let request = press(app, code);
            app.perform(request).await;
        }

        /// `Enter` into the fields, then `Down` to the field `name` of
        /// `label`.
        fn go_to(app: &mut TaskApp, label: &str, name: &str) {
            press(app, KeyCode::Enter);
            let chooser = app.new_task_form().unwrap().chooser();
            let index = chooser.selected();
            assert_eq!(chooser.selected_item().unwrap().label, label);
            let target = chooser
                .fields_of(index)
                .iter()
                .position(|field| field.name == name)
                .unwrap_or_else(|| panic!("{name} is not a field of {label}"));
            for _ in 0..target {
                press(app, KeyCode::Down);
            }
        }

        fn erase(app: &mut TaskApp, characters: usize) {
            for _ in 0..characters {
                press(app, KeyCode::Backspace);
            }
        }

        fn suggestions(app: &TaskApp, label: &str, name: &str) -> Vec<String> {
            let chooser = app.new_task_form().unwrap().chooser();
            let index = chooser
                .items()
                .iter()
                .position(|item| item.label == label)
                .unwrap();
            chooser
                .fields_of(index)
                .iter()
                .find(|field| field.name == name)
                .unwrap()
                .matching_suggestions()
                .into_iter()
                .map(str::to_string)
                .collect()
        }

        /// The branches the form sends for the workflow it is on, with the
        /// ticket filled in, by workspace.
        fn sent_branches(app: &mut TaskApp) -> std::collections::BTreeMap<String, String> {
            let Request::SendTask(_) = enter_until_send(app) else {
                panic!("a send was expected");
            };
            let CreateHow::Catalogue(request) = &app.outgoing()[0].send else {
                panic!("a workflow of the catalogue goes through the control plane");
            };
            request
                .launch
                .workspaces
                .iter()
                .filter_map(|(name, choice)| Some((name.clone(), choice.branch.clone()?)))
                .collect()
        }

        /// Types the ticket into the field under the one the keys are in,
        /// which is the `ticket_id` after the last branch field.
        fn fill_ticket_below(app: &mut TaskApp) {
            press(app, KeyCode::Down);
            typed(app, "NAR-18");
        }

        #[tokio::test]
        async fn a_workspace_has_a_branch_field_right_after_its_repository_that_starts_on_the_default(
        ) {
            let remotes = Remotes::listing(&[("docs", &["develop", "main"])]);
            let mut app = app_reading(&remotes);
            open_default(
                &mut app,
                vec![
                    repository_with("docs", Some("develop")),
                    repository_with("narsor", Some("main")),
                ],
            )
            .await;
            let names: Vec<String> = chooser_item(&app, "implement-feature")
                .fields
                .iter()
                .map(|field| field.name.clone())
                .collect();
            assert_eq!(
                names,
                [
                    "Ticket".to_string(),
                    "Tytuł".to_string(),
                    repository_of("target_repo"),
                    branch_of("target_repo"),
                    "ticket_id".to_string(),
                    "attempts".to_string(),
                    "dry_run".to_string(),
                ]
            );
            assert_eq!(
                field_value(&app, "implement-feature", &branch_of("target_repo")),
                "develop"
            );
            assert_eq!(
                chooser_item(&app, "fix-bug").fields.len(),
                3,
                "a workflow without a workspace to choose has no branch field"
            );
        }

        #[tokio::test]
        async fn the_list_of_the_server_is_offered_and_narrows_to_what_is_typed() {
            let remotes = Remotes::listing(&[("docs", &["main", "develop", "feature/x"])]);
            let mut app = app_reading(&remotes);
            open_default(&mut app, vec![repository_with("docs", Some("main"))]).await;
            let name = branch_of("target_repo");
            go_to(&mut app, "implement-feature", &name);
            assert_eq!(remotes.reads(), ["docs"]);
            assert_eq!(
                suggestions(&app, "implement-feature", &name),
                ["main", "develop", "feature/x"],
                "the untouched default does not narrow the list"
            );
            erase(&mut app, 4);
            typed(&mut app, "dev");
            assert_eq!(suggestions(&app, "implement-feature", &name), ["develop"]);
            erase(&mut app, 3);
            typed(&mut app, "X");
            assert_eq!(suggestions(&app, "implement-feature", &name), ["feature/x"]);
        }

        #[tokio::test]
        async fn a_suggestion_taken_with_the_right_arrow_is_what_is_sent() {
            let remotes = Remotes::listing(&[("docs", &["main", "develop", "feature/x"])]);
            let mut app = app_reading(&remotes);
            open_default(&mut app, vec![repository_with("docs", Some("main"))]).await;
            let name = branch_of("target_repo");
            go_to(&mut app, "implement-feature", &name);
            erase(&mut app, 4);
            typed(&mut app, "dev");
            press(&mut app, KeyCode::Right);
            assert_eq!(field_value(&app, "implement-feature", &name), "develop");
            fill_ticket_below(&mut app);
            assert_eq!(
                sent_branches(&mut app),
                [("target_repo".to_string(), "develop".to_string())].into()
            );
        }

        #[tokio::test]
        async fn the_right_arrow_steps_through_the_suggestions_of_what_was_typed() {
            let remotes = Remotes::listing(&[("docs", &["main", "release/1", "release/2"])]);
            let mut app = app_reading(&remotes);
            open_default(&mut app, vec![repository_with("docs", Some("main"))]).await;
            let name = branch_of("target_repo");
            go_to(&mut app, "implement-feature", &name);
            erase(&mut app, 4);
            typed(&mut app, "rel");
            for expected in ["release/1", "release/2", "release/1"] {
                press(&mut app, KeyCode::Right);
                assert_eq!(field_value(&app, "implement-feature", &name), expected);
            }
        }

        #[tokio::test]
        async fn a_branch_the_list_does_not_have_is_accepted_and_sent() {
            let remotes = Remotes::listing(&[("docs", &["main", "develop"])]);
            let mut app = app_reading(&remotes);
            open_default(&mut app, vec![repository_with("docs", Some("main"))]).await;
            let name = branch_of("target_repo");
            go_to(&mut app, "implement-feature", &name);
            erase(&mut app, 4);
            typed(&mut app, "feature/nowy");
            assert!(suggestions(&app, "implement-feature", &name).is_empty());
            fill_ticket_below(&mut app);
            assert_eq!(
                sent_branches(&mut app),
                [("target_repo".to_string(), "feature/nowy".to_string())].into()
            );
            assert!(app.outgoing()[0].task_id.starts_with("task-"));
        }

        #[tokio::test]
        async fn a_list_that_cannot_be_read_says_why_and_leaves_a_plain_text_field() {
            let remotes = Remotes::default();
            remotes.set("docs", Err("brak dostępu do remote".to_string()));
            let remotes = Arc::new(remotes);
            let mut app = app_reading(&remotes);
            open_default(&mut app, vec![repository_with("docs", Some("main"))]).await;
            let warning = app.notifier().latest().expect("a warning").clone();
            assert_eq!(warning.severity, Severity::Warning);
            assert!(
                warning.text.contains("brak dostępu do remote"),
                "{}",
                warning.text
            );
            assert!(warning.text.contains("docs"), "{}", warning.text);
            let name = branch_of("target_repo");
            go_to(&mut app, "implement-feature", &name);
            assert!(suggestions(&app, "implement-feature", &name).is_empty());
            erase(&mut app, 4);
            typed(&mut app, "feature/x");
            fill_ticket_below(&mut app);
            assert_eq!(
                sent_branches(&mut app),
                [("target_repo".to_string(), "feature/x".to_string())].into()
            );
        }

        #[tokio::test]
        async fn changing_the_repository_puts_its_default_in_the_field_and_reads_its_list() {
            let remotes = Remotes::listing(&[
                ("docs", &["develop", "main"]),
                ("narsor", &["main", "trunk"]),
            ]);
            let mut app = app_reading(&remotes);
            open_default(
                &mut app,
                vec![
                    repository_with("docs", Some("develop")),
                    repository_with("narsor", Some("trunk")),
                ],
            )
            .await;
            let name = branch_of("target_repo");
            go_to(&mut app, "implement-feature", &name);
            erase(&mut app, 7);
            typed(&mut app, "wlasny");
            press(&mut app, KeyCode::Up);
            key(&mut app, KeyCode::Right).await;
            assert_eq!(
                field_value(&app, "implement-feature", &repository_of("target_repo")),
                "narsor"
            );
            assert_eq!(field_value(&app, "implement-feature", &name), "trunk");
            assert_eq!(
                suggestions(&app, "implement-feature", &name),
                ["main", "trunk"]
            );
            assert_eq!(remotes.reads(), ["docs", "narsor"]);
        }

        #[tokio::test]
        async fn two_workspaces_have_two_independent_branch_fields() {
            let remotes = Remotes::listing(&[("docs", &["main", "a"]), ("narsor", &["main", "b"])]);
            let mut app = app_reading(&remotes);
            opened(
                &mut app,
                two_workspaces(),
                vec![
                    repository_with("docs", Some("main")),
                    repository_with("narsor", Some("main")),
                ],
            )
            .await;
            let names: Vec<String> = chooser_item(&app, "compare")
                .fields
                .iter()
                .map(|field| field.name.clone())
                .collect();
            assert_eq!(
                names,
                [
                    "Ticket".to_string(),
                    "Tytuł".to_string(),
                    repository_of("code"),
                    branch_of("code"),
                    repository_of("docs_repo"),
                    branch_of("docs_repo"),
                    "ticket_id".to_string(),
                ]
            );
            // The second repository is another one.
            go_to(&mut app, "compare", &repository_of("docs_repo"));
            key(&mut app, KeyCode::Right).await;
            press(&mut app, KeyCode::Down);
            erase(&mut app, 4);
            typed(&mut app, "b");
            assert_eq!(field_value(&app, "compare", &branch_of("code")), "main");
            fill_ticket_below(&mut app);
            assert_eq!(
                sent_branches(&mut app),
                [
                    ("code".to_string(), "main".to_string()),
                    ("docs_repo".to_string(), "b".to_string())
                ]
                .into(),
                "each workspace sends the branch it shows"
            );
        }

        #[tokio::test]
        async fn a_repository_without_a_default_starts_empty_and_sends_no_branch_untouched() {
            let remotes = Remotes::listing(&[("docs", &["master"])]);
            let mut app = app_reading(&remotes);
            open_default(&mut app, vec![repository_with("docs", None)]).await;
            assert_eq!(field_value_or_empty(&app, &branch_of("target_repo")), "");
            fill_ticket(&mut app, "NAR-18");
            assert!(sent_branches(&mut app).is_empty());
        }

        fn field_value_or_empty(app: &TaskApp, name: &str) -> String {
            let chooser = app.new_task_form().unwrap().chooser();
            let index = chooser
                .items()
                .iter()
                .position(|item| item.label == "implement-feature")
                .unwrap();
            chooser
                .fields_of(index)
                .iter()
                .find(|field| field.name == name)
                .unwrap()
                .value()
                .to_string()
        }

        #[tokio::test]
        async fn an_untouched_form_sends_the_branch_it_shows_and_asks_nothing_when_left() {
            let remotes = Remotes::listing(&[("docs", &["main"])]);
            let mut app = app_reading(&remotes);
            open_default(&mut app, vec![repository_with("docs", Some("main"))]).await;
            assert_eq!(app.new_task_form().unwrap().draft_text(), "");
            fill_ticket(&mut app, "NAR-18");
            let Request::SendTask(_) = enter_until_send(&mut app) else {
                panic!("a send was expected");
            };
            let CreateHow::Catalogue(request) = &app.outgoing()[0].send else {
                panic!("a workflow of the catalogue goes through the control plane");
            };
            assert_eq!(
                request.launch.workspaces,
                [(
                    "target_repo".to_string(),
                    WorkspaceRequest {
                        repository: Some("docs".to_string()),
                        branch: Some("main".to_string()),
                        commit: None,
                    }
                )]
                .into()
            );
        }

        #[tokio::test]
        async fn the_base_branch_of_the_workspace_is_suggested_whatever_the_repository() {
            let remotes = Remotes::listing(&[("docs", &["main"]), ("narsor", &["trunk"])]);
            let mut app = app_reading(&remotes);
            opened(
                &mut app,
                catalogue(vec![with_workspaces(
                    catalogue_workflow("flow", "Opis.", vec![]),
                    vec![workspace("code", Some("jojo"))],
                )]),
                vec![
                    repository_with("docs", Some("main")),
                    repository_with("narsor", Some("trunk")),
                ],
            )
            .await;
            assert_eq!(field_value(&app, "flow", &branch_of("code")), "jojo");
            go_to(&mut app, "flow", &repository_of("code"));
            key(&mut app, KeyCode::Right).await;
            assert_eq!(field_value(&app, "flow", &repository_of("code")), "narsor");
            assert_eq!(field_value(&app, "flow", &branch_of("code")), "jojo");
        }

        #[tokio::test]
        async fn a_pasted_branch_goes_into_the_field_and_narrows_the_list() {
            let remotes = Remotes::listing(&[("docs", &["main", "feature/x", "feature/y"])]);
            let mut app = app_reading(&remotes);
            open_default(&mut app, vec![repository_with("docs", Some("main"))]).await;
            let name = branch_of("target_repo");
            go_to(&mut app, "implement-feature", &name);
            erase(&mut app, 4);
            app.on_paste("feature/\n");
            assert_eq!(field_value(&app, "implement-feature", &name), "feature/ ");
            assert_eq!(
                suggestions(&app, "implement-feature", &name),
                ["feature/x", "feature/y"]
            );
        }

        #[tokio::test]
        async fn a_workflow_file_asks_for_its_workspaces_too() {
            let remotes = Remotes::listing(&[("docs", &["main"])]);
            let mut app = app_reading(&remotes);
            open_default(&mut app, vec![repository_with("docs", Some("main"))]).await;
            choose(&mut app, PATH_LABEL);
            press(&mut app, KeyCode::Enter);
            typed(&mut app, "/srv/ok.yaml");
            app.on_path_checked(
                "/srv/ok.yaml",
                Ok(ValidateWorkflowResponse {
                    valid: true,
                    name: Some("my-flow".to_string()),
                    version: Some(4),
                    state_count: 2,
                    initial_state: Some("start".to_string()),
                    inputs: vec![],
                    workspaces: vec![workspace("repo", None)],
                    violations: vec![],
                }),
            );
            let item = chooser_item(&app, PATH_LABEL);
            let names: Vec<&str> = item.fields.iter().map(|f| f.name.as_str()).collect();
            assert_eq!(
                names,
                [
                    PATH_FIELD,
                    TICKET_FIELD,
                    TITLE_FIELD,
                    repository_of("repo").as_str(),
                    branch_of("repo").as_str()
                ]
            );
            assert!(matches!(item.fields[4].kind, FieldKind::Suggest(_)));
        }
    }

    mod repositories {
        use super::*;

        fn names(app: &TaskApp) -> Vec<String> {
            app.repositories()
                .unwrap()
                .repositories()
                .iter()
                .map(|repository| repository.id.clone())
                .collect()
        }

        #[test]
        fn the_list_shows_what_is_registered_with_a_description_and_a_remote_only_form_for_a_remote_server(
        ) {
            let mut app = app_with(vec![]);
            app.set_capabilities(capabilities(false, Some("remote")));
            assert_eq!(ch(&mut app, 'R'), Request::OpenRepositories);
            app.open_repositories(narsor_registry());
            assert_eq!(app.view_kind(), ViewKind::Repositories);
            assert_eq!(names(&app), ["docs", "narsor"]);
            assert_eq!(app.key_context().base, Context::Repositories);
            let screen = screen(&mut app, 130, 30);
            for expected in [
                "Repozytoria (2)",
                "docs",
                "narsor",
                "git@github.com:acme/docs.git",
                "a dodaj",
                "d usuń",
            ] {
                assert!(screen.contains(expected), "{expected}:\n{screen}");
            }

            ch(&mut app, 'a');
            let form = app.repositories().unwrap().form().expect("the form");
            let remote = &form.items()[0].fields[1];
            assert_eq!(remote.name, "remote");
            assert!(
                remote
                    .description
                    .as_deref()
                    .is_some_and(|text| text.contains("tylko zdalny URL")),
                "{:?}",
                remote.description
            );
        }

        #[test]
        fn d_asks_y_n_and_n_removes_nothing() {
            let mut app = app_with(vec![]);
            ch(&mut app, 'R');
            app.open_repositories(narsor_registry());
            ch(&mut app, 'j');
            ch(&mut app, 'd');
            let window = app
                .overlay()
                .and_then(|overlay| overlay.confirmation())
                .expect("the window");
            assert!(window.heading().contains("Usuń repozytorium"));
            assert!(window.lines().iter().any(|line| line.contains("narsor")));
            assert_eq!(app.key_context().base, Context::Confirm);
            assert_eq!(ch(&mut app, 'n'), Request::None);
            assert!(app.overlay().is_none());
            // The window survives a re-read of the task list.
            ch(&mut app, 'd');
            app.load(listing(vec![], "d"));
            assert!(
                app.overlay().is_some(),
                "it is not about a task of the list"
            );
            assert_eq!(
                ch(&mut app, 'y'),
                Request::RemoveRepository("narsor".to_string())
            );
        }

        #[test]
        fn removing_is_blocked_without_a_connection_and_says_why() {
            let mut app = app_with(vec![]);
            ch(&mut app, 'R');
            app.open_repositories(narsor_registry());
            ch(&mut app, 'd');
            app.on_stream(Some(Err(ClientError::Unavailable("gone".to_string()))));
            assert_eq!(ch(&mut app, 'y'), Request::None);
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Warning);
        }

        #[test]
        fn esc_goes_back_to_the_tasks_and_q_asks_while_a_form_holds_text() {
            let mut app = app_with(vec![titled(queued("a", 1), "NAR-1")]);
            ch(&mut app, 'R');
            app.open_repositories(narsor_registry());
            ch(&mut app, 'a');
            press(&mut app, KeyCode::Enter);
            typed(&mut app, "docs-2");
            assert_eq!(app.key_context().base, Context::Form);
            assert!(app.detail().has_drafts());
            press(&mut app, KeyCode::Esc);
            assert!(app.repositories().unwrap().form().is_none());
            assert!(!app.detail().has_drafts(), "the form was closed");
            press(&mut app, KeyCode::Esc);
            assert_eq!(app.view_kind(), ViewKind::Tasks);
            assert_eq!(app.list().selected_id(), Some("a"));
        }
    }
}

/// U5: the installation's default harness — the header names it, `h` changes
/// it from the list, the details, the history and the repositories, never
/// from a field being typed into or over another window (R12–R15, AE3, AE4).
mod task_app_harness {

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::json;

    use narsor::api::ApiClient;
    use narsor::tui::keymap::{self, Action, Context, BINDINGS};

    use narsor::tui::task_app::{DetailTab, Focus, Request, TaskApp};
    use narsor_engine::domain::ExecutionStatus;

    use super::task_app_support::{
        capabilities, catalogue, catalogue_workflow, harness_selection, listing, registry, task,
        titled, with_controller, with_pending, with_runner, HARNESSES,
    };

    fn offline_client() -> ApiClient {
        ApiClient::new("127.0.0.1:9".parse().unwrap(), "token")
    }

    /// A live application over one running task `a`, knowing that the
    /// daemon's harness is `claude-code` from the configuration, or
    /// `override_harness` over it.
    fn app(override_harness: Option<&str>) -> TaskApp {
        let mut app = TaskApp::new(offline_client(), "local", None);
        app.load(listing(vec![titled(running("a"), "NAR-12")], "c"));
        app.set_harness_selection(harness_selection(
            "claude-code",
            override_harness,
            &HARNESSES,
        ));
        app
    }

    fn running(id: &str) -> narsor_engine::controller_protocol::TaskSummary {
        with_runner(
            with_controller(task(id, "local", "running", 1)),
            ExecutionStatus::Running,
            "review",
        )
    }

    /// One key. An `Enter` that asks for the confirmation of a send is
    /// answered with the `Enter` of the window, so a test that says "Enter
    /// sends" still reads so; the window itself is tested with `on_key`.
    fn press(app: &mut TaskApp, code: KeyCode) -> Request {
        let request = app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
        if code == KeyCode::Enter && request == Request::None && app.form_confirming() {
            return app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        }
        request
    }

    fn ch(app: &mut TaskApp, character: char) -> Request {
        press(app, KeyCode::Char(character))
    }

    fn h() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)
    }

    #[test]
    fn h_opens_the_choice_from_the_list_and_from_every_tab_of_the_details() {
        let mut app = app(None);
        assert_eq!(app.focus(), Focus::List);
        assert_eq!(ch(&mut app, 'h'), Request::OpenHarness);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.focus(), Focus::Details);
        for tab in DetailTab::ALL {
            app.detail_mut().set_tab(tab);
            assert_eq!(ch(&mut app, 'h'), Request::OpenHarness, "{tab:?}");
        }
    }

    #[test]
    fn h_belongs_to_the_history_and_the_repositories_as_well() {
        for context in [
            Context::List,
            Context::Details,
            Context::History,
            Context::HistoryDetails,
            Context::Repositories,
        ] {
            assert_eq!(
                keymap::lookup(context, &h()),
                Some(Action::ChooseHarness),
                "{context:?}"
            );
        }
        let mut app = app(None);
        app.open_repositories(registry(vec![]));
        assert_eq!(ch(&mut app, 'h'), Request::OpenHarness);
    }

    #[test]
    fn ae4_h_types_a_letter_in_the_conductors_chat() {
        let mut app = app(None);
        press(&mut app, KeyCode::Enter);
        app.detail_mut().set_tab(DetailTab::Conductor);
        let revision = app.list().selected().unwrap().revision;
        app.on_detail_read(
            "a",
            revision,
            narsor::tui::task_app::detail::DetailRead::Conductor(Ok(
                narsor_protocol::controller::ConductorReadResponse::Available {
                    conductor: Box::new(super::task_app_support::conductor_view(vec![], json!({}))),
                    source: narsor_protocol::controller::ExecutionEventsSource::Runner,
                },
            )),
        );
        ch(&mut app, 'c');
        assert_eq!(app.key_context().base, Context::Form);
        assert_eq!(ch(&mut app, 'h'), Request::None);
        assert!(app.overlay().is_none());
        assert_eq!(app.detail().draft("a"), Some("h"));
    }

    #[test]
    fn ae4_h_types_a_letter_in_the_answer_panel() {
        let mut app = TaskApp::new(offline_client(), "local", None);
        let waiting = with_pending(
            running("a"),
            json!({
                "execution_id": "exec-1",
                "state_id": "review",
                "action_execution_id": "act-7",
                "instruction": "Która wersja API?",
                "fields": [{ "name": "answer", "type": "text", "required": true }],
                "continuation": "resume_agent",
                "prefilled_answer": null,
                "episode": "ep-q",
                "requested_at": "2026-10-09T11:54:00Z"
            }),
        );
        app.load(listing(vec![titled(waiting, "NAR-12")], "c"));
        app.set_harness_selection(harness_selection("claude-code", None, &HARNESSES));
        ch(&mut app, ' ');
        press(&mut app, KeyCode::Enter);
        assert!(app.panel_focused());
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.key_context().base, Context::Form);
        assert_eq!(ch(&mut app, 'h'), Request::None);
        assert!(app.overlay().is_none());
        assert!(
            app.detail()
                .draft("a")
                .is_some_and(|draft| draft.contains('h')),
            "{:?}",
            app.detail().draft("a")
        );
    }

    #[test]
    fn ae4_h_types_a_letter_in_the_new_task_form() {
        let mut app = app(None);
        app.set_capabilities(capabilities(true, None));
        assert_eq!(ch(&mut app, 'n'), Request::OpenNewTask);
        app.open_new_task(
            catalogue(vec![catalogue_workflow(
                "fix-bug",
                "Poprawka błędu.",
                vec![super::task_app_support::workflow_input(
                    "ticket_id",
                    narsor_engine::domain::ParamType::String,
                    true,
                    None,
                )],
            )]),
            registry(vec![]),
        );
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.key_context().base, Context::Form);
        assert_eq!(ch(&mut app, 'h'), Request::None);
        assert!(app.overlay().is_none());
        assert!(app.detail().has_drafts(), "the letter went into the field");
    }

    #[test]
    fn the_key_map_stays_consistent_and_the_bar_and_help_name_h() {
        assert_eq!(keymap::check_consistency(BINDINGS), Ok(()));
        assert!(
            keymap::hints(Context::List)
                .iter()
                .any(|(keys, label)| keys == "h" && *label == "harness"),
            "{:?}",
            keymap::hints(Context::List)
        );
        for context in [Context::List, Context::Details, Context::Repositories] {
            assert!(
                keymap::help_entries(context).iter().any(
                    |entry| entry.keys == "h" && entry.description.contains("domyślny harness")
                ),
                "{context:?}"
            );
        }
    }
}

/// Automatic resume (R7, R8, R10): what the stream's resume events say on
/// the bar, what an interrupted task is said to wait for, and the
/// conversation read from the task's record going back to live.
mod task_app_auto_resume {
    use chrono::Utc;

    use narsor::api::ApiClient;
    use narsor::cli::task_text::RecordAge;
    use narsor::tui::task_app::actions::{availability, TaskAction};
    use narsor::tui::task_app::detail::DetailRead;
    use narsor::tui::task_app::list::{attention_of, Attention};
    use narsor::tui::task_app::notify::Severity;
    use narsor::tui::task_app::tabs::conversation::{record_badge, AttemptRead, Attempts};
    use narsor::tui::task_app::TaskApp;
    use narsor::tui::transcript::{TranscriptEntry, TranscriptEntryId};
    use narsor_engine::controller_protocol::{ExecutionEventsSource, TaskSummary};
    use narsor_engine::engine::{ConversationState, PositionStatus};
    use narsor_engine::orchestration::{RunnerState, TaskEventKind};

    use super::task_app_support::{
        auto_resumed, event, exhausted, interrupted, listing, position, timeline, titled,
    };

    fn app() -> TaskApp {
        TaskApp::new(
            ApiClient::new("127.0.0.1:9".parse().unwrap(), "token"),
            "local",
            None,
        )
    }

    /// `count` interrupted tasks `t1`…, `NAR-1`…, each resumed automatically
    /// soon.
    fn lost_tasks(count: u32) -> Vec<TaskSummary> {
        (1..=count)
            .map(|n| {
                let mut task = titled(
                    interrupted(&format!("t{n}"), RunnerState::Running, 1, true),
                    &format!("NAR-{n}"),
                );
                task.rank = n;
                task
            })
            .collect()
    }

    fn order(app: &TaskApp) -> Vec<String> {
        app.list()
            .tasks()
            .iter()
            .map(|task| task.task_id.to_string())
            .collect()
    }

    fn note(app: &TaskApp) -> (Severity, String) {
        let note = app.notifier().latest().expect("a notification");
        (note.severity, note.text.clone())
    }

    /// AE1: a resume says so with the task's name; the row keeps its place
    /// and the selection stays.
    #[test]
    fn an_automatic_resume_is_a_notification_and_the_row_stays_put() {
        let mut app = app();
        app.load(listing(lost_tasks(3), "c"));
        let selected = app.list().selected_id().map(str::to_string);
        app.on_stream(auto_resumed("e1", "t2", 2, 1));
        assert_eq!(
            note(&app),
            (
                Severity::Warning,
                "NAR-2: wznowiono automatycznie po utracie runnera (próba 1/5)".to_string()
            )
        );
        assert_eq!(order(&app), ["t1", "t2", "t3"]);
        assert_eq!(app.list().selected_id().map(str::to_string), selected);
        let row = &app.list().tasks()[1];
        assert_eq!(row.runner_state, RunnerState::Provisioning);
        assert_eq!(attention_of(row), Attention::None);
    }

    /// A resume after a controller restart the policy did not count names
    /// no attempt.
    #[test]
    fn an_uncounted_automatic_resume_names_the_restart_not_an_attempt() {
        let mut app = app();
        app.load(listing(lost_tasks(3), "c"));
        app.on_stream(auto_resumed("e1", "t2", 2, 0));
        assert_eq!(
            note(&app),
            (
                Severity::Warning,
                "NAR-2: wznowiono automatycznie po restarcie kontrolera (poza limitem 5 prób)"
                    .to_string()
            )
        );
    }

    /// Resumes in a row — a restart — are counted in one notification.
    #[test]
    fn resumes_in_a_row_are_counted_in_one_notification() {
        let mut app = app();
        app.load(listing(lost_tasks(5), "c"));
        for (n, task) in ["t1", "t2", "t3"].iter().enumerate() {
            app.on_stream(auto_resumed(&format!("e{n}"), task, 2, 1));
        }
        assert_eq!(
            note(&app),
            (
                Severity::Warning,
                "wznowiono automatycznie 3 zadania".to_string()
            )
        );
        app.on_stream(auto_resumed("e4", "t4", 2, 1));
        app.on_stream(auto_resumed("e5", "t5", 2, 1));
        assert_eq!(note(&app).1, "wznowiono automatycznie 5 zadań");
        assert_eq!(order(&app), ["t1", "t2", "t3", "t4", "t5"]);
    }

    /// Events the list already shows — replayed when the stream connects
    /// again — say nothing.
    #[test]
    fn replayed_resume_events_raise_no_notification() {
        let mut app = app();
        let mut tasks = lost_tasks(1);
        tasks[0].revision = 5;
        app.load(listing(tasks, "c"));
        app.on_stream(auto_resumed("e1", "t1", 4, 1));
        app.on_stream(auto_resumed("e2", "t1", 5, 1));
        app.on_stream(event(
            "e3",
            "t1",
            5,
            RunnerState::Interrupted,
            TaskEventKind::AutoResumeExhausted {
                attempts: 5,
                reason: "could not resume automatically: pod evicted".to_string(),
            },
        ));
        assert!(app.notifier().latest().is_none());
    }

    /// AE3: giving up is an error with the reason, and the row is marked
    /// for its owner in its place.
    #[test]
    fn giving_up_is_an_error_and_marks_the_row() {
        let mut app = app();
        app.load(listing(lost_tasks(3), "c"));
        app.on_stream(event(
            "e1",
            "t2",
            2,
            RunnerState::Interrupted,
            TaskEventKind::AutoResumeExhausted {
                attempts: 5,
                reason: "could not resume automatically: pod evicted".to_string(),
            },
        ));
        assert_eq!(
            note(&app),
            (
                Severity::Error,
                "NAR-2: nie udało się wznowić automatycznie: pod evicted".to_string()
            )
        );
        assert_eq!(attention_of(&app.list().tasks()[1]), Attention::Problem);
        assert_eq!(order(&app), ["t1", "t2", "t3"]);

        app.on_stream(event(
            "e2",
            "t3",
            2,
            RunnerState::Interrupted,
            TaskEventKind::AutoResumeExhausted {
                attempts: 5,
                reason: "could not resume automatically: the workload was lost".to_string(),
            },
        ));
        assert_eq!(
            note(&app).1,
            "NAR-3: nie udało się wznowić automatycznie: runner utracony"
        );
    }

    /// Only the two cases that wait for the owner mark the row.
    #[test]
    fn the_row_is_marked_only_when_the_owner_has_to_act() {
        let planned = interrupted("a", RunnerState::Running, 2, true);
        assert_eq!(attention_of(&planned), Attention::None);
        let pausing = interrupted("a", RunnerState::PauseRequested, 0, false);
        assert_eq!(attention_of(&pausing), Attention::Problem);
        let given_up = exhausted(
            interrupted("a", RunnerState::Running, 6, false),
            "could not resume automatically: pod evicted",
        );
        assert_eq!(attention_of(&given_up), Attention::Problem);
    }

    /// The menu's reason for an interrupted task mentions a planned resume.
    #[test]
    fn an_action_of_a_task_resuming_on_its_own_says_so() {
        let planned = interrupted("a", RunnerState::Running, 2, true);
        let reason = availability(&planned, TaskAction::Stop);
        assert_eq!(
            reason.reason(),
            Some("zadanie jest przerwane — wznowi się samo (próba 2/5) albo uruchom je od nowa")
        );
        let pausing = interrupted("a", RunnerState::PauseRequested, 0, false);
        assert_eq!(
            availability(&pausing, TaskAction::Pause).reason(),
            Some("zadanie jest przerwane — uruchom je od nowa")
        );
        assert!(availability(&planned, TaskAction::Restart).is_available());
    }

    #[test]
    fn a_record_is_as_old_as_its_largest_whole_unit() {
        assert_eq!(RecordAge::of_seconds(-3), RecordAge::JustNow);
        assert_eq!(RecordAge::of_seconds(59), RecordAge::JustNow);
        assert_eq!(RecordAge::of_seconds(60), RecordAge::Minutes(1));
        assert_eq!(RecordAge::of_seconds(3599), RecordAge::Minutes(59));
        assert_eq!(RecordAge::of_seconds(3600), RecordAge::Hours(1));
        assert_eq!(RecordAge::of_seconds(2 * 86_400 + 5), RecordAge::Days(2));

        let now = Utc::now();
        let ago = |seconds| Some(now - chrono::Duration::seconds(seconds));
        assert_eq!(record_badge(ago(20), now, 80), "zapis sprzed chwili");
        assert_eq!(record_badge(ago(20), now, 12), "zapis · przed chwilą");
        assert_eq!(record_badge(ago(185), now, 80), "zapis sprzed 3 min");
        assert_eq!(record_badge(ago(185), now, 12), "zapis · 3 min");
        assert_eq!(record_badge(ago(7300), now, 80), "zapis sprzed 2 h");
        assert_eq!(record_badge(ago(90_000), now, 80), "zapis sprzed 1 d");
        assert_eq!(record_badge(None, now, 80), "rozmowa z zapisu");
    }

    fn entry(id: &str) -> TranscriptEntry {
        TranscriptEntry::new(
            TranscriptEntryId::from(id),
            vec![format!("line {id}")],
            id.as_bytes().to_vec(),
        )
    }

    fn read(ids: &[&str], next_entry: u64, source: ExecutionEventsSource) -> AttemptRead {
        AttemptRead {
            entries: ids.iter().map(|id| entry(id)).collect(),
            next_entry,
            closed: false,
            truncated_tail: false,
            complete: true,
            no_record: false,
            source,
            recorded_at: (source == ExecutionEventsSource::Storage).then(Utc::now),
        }
    }

    /// R10: an interrupted task's conversation is read from the record; once
    /// the task runs again it is due for a read from where the record
    /// stopped, which comes from the runner and takes the badge away.
    #[test]
    fn back_to_running_the_conversation_is_read_live_from_where_it_stopped() {
        let mut app = app();
        app.load(listing(
            vec![titled(
                interrupted("a", RunnerState::Running, 1, true),
                "NAR-1",
            )],
            "c",
        ));
        let mut plan = position("plan", PositionStatus::InProgress);
        plan.conversation = ConversationState::Recorded;
        app.on_detail_read(
            "a",
            1,
            DetailRead::Timeline(Ok(timeline("a", vec![plan], Some("plan #1")))),
        );
        app.on_detail_read(
            "a",
            1,
            DetailRead::Attempt {
                attempt_id: "act-plan".into(),
                from: 0,
                result: Ok(read(&["1", "2", "3"], 3, ExecutionEventsSource::Storage)),
            },
        );
        let attempts = app.detail().view("a").unwrap().attempts();
        assert!(attempts.get("act-plan").unwrap().from_record());
        assert!(!attempts.needs_read("act-plan", 1));

        app.on_stream(event(
            "e1",
            "a",
            2,
            RunnerState::Running,
            TaskEventKind::Created,
        ));
        let revision = app.list().selected().unwrap().revision;
        assert_eq!(revision, 2);
        let attempts = app.detail().view("a").unwrap().attempts();
        assert!(attempts.needs_read("act-plan", revision));
        assert_eq!(attempts.from("act-plan"), 3);
    }

    /// The same, as the cache sees it: the read from the runner continues the
    /// record and the reader keeps its entry — even a closed attempt is read
    /// again once, since only the runner's word ends the badge.
    #[test]
    fn a_live_read_after_the_record_keeps_the_place_and_ends_the_badge() {
        let mut attempts = Attempts::default();
        let mut stored = read(&["1", "2", "3"], 3, ExecutionEventsSource::Storage);
        stored.closed = true;
        attempts.apply("act-plan", 1, 0, Ok(stored));
        assert!(attempts.needs_read("act-plan", 2), "a record is read again");
        let transcript = attempts.get_mut("act-plan").unwrap().transcript_mut();
        transcript.set_viewport(10);
        transcript.to_bottom();
        transcript.line_up();
        assert_eq!(transcript.selected_entry().unwrap().as_str(), "2");

        attempts.apply(
            "act-plan",
            2,
            3,
            Ok(read(&["4"], 4, ExecutionEventsSource::Runner)),
        );
        let attempt = attempts.get("act-plan").unwrap();
        assert!(!attempt.from_record());
        assert_eq!(attempt.len(), 4);
        assert_eq!(attempt.transcript().selected_entry().unwrap().as_str(), "2");
    }
}

// ---------------------------------------------------------------------------
// The row of a permission denial that did not stop the step
// ---------------------------------------------------------------------------

fn ignored_denial(
    tool: &str,
    summary: &str,
    reason: Option<&str>,
    source: Option<(&str, &str)>,
) -> EventData {
    EventData::PermissionDeniedIgnored {
        session_id: "sess".to_string(),
        action_execution_id: Some("a1b2c3d4-0000".to_string()),
        tool_name: tool.to_string(),
        tool_use_id: "toolu_77".to_string(),
        summary: summary.to_string(),
        reason: reason.map(str::to_string),
        source: source.map(|(id, name)| narsor_protocol::chat::SubagentRef {
            id: id.to_string(),
            name: name.to_string(),
        }),
    }
}

fn ignored_denial_line(data: EventData) -> String {
    event_line(&Event::new("exec-1".to_string(), data, 1))
}

#[test]
fn ignored_denial_row_names_the_main_agent_or_the_subagent() {
    let main = ignored_denial_line(ignored_denial("Bash", "ls", None, None));
    let sub = ignored_denial_line(ignored_denial("Bash", "ls", None, Some(("t1", "reviewer"))));

    assert!(
        main.ends_with("odmowa pominięta · agent · Bash · ls"),
        "{main:?}"
    );
    assert!(
        sub.ends_with("odmowa pominięta · subagent reviewer · Bash · ls"),
        "{sub:?}"
    );
}

#[test]
fn ignored_denial_row_shows_the_source_before_a_cut_key_parameter() {
    let command = "x".repeat(500);
    let line = ignored_denial_line(ignored_denial(
        "Bash",
        &command,
        None,
        Some(("t1", "reviewer")),
    ));

    assert_eq!(line.lines().count(), 1);
    assert!(line.chars().count() < 200, "the key parameter is cut");
    assert!(line.ends_with('…'), "the cut is marked: {line:?}");
    let source_at = line.find("subagent reviewer").expect("source in row");
    let key_at = line.find("xxx").expect("key parameter in row");
    assert!(source_at < key_at, "source comes first: {line:?}");
}

#[test]
fn ignored_denial_row_is_one_line_without_control_characters() {
    let line = ignored_denial_line(ignored_denial(
        "Bash",
        "echo a\nb\u{1b}[31m\u{7}",
        Some("no\nway"),
        Some(("t1", "name\twith\ncontrol\u{7}")),
    ));

    assert_eq!(line.lines().count(), 1, "{line:?}");
    assert!(
        !line.chars().any(char::is_control),
        "control character in {line:?}"
    );
}

#[test]
fn ignored_denial_row_leaves_no_empty_separators_for_missing_parts() {
    let line = ignored_denial_line(ignored_denial("Bash", "", None, Some(("t1", "  "))));

    assert!(
        line.ends_with("odmowa pominięta · subagent · Bash"),
        "{line:?}"
    );
    assert!(!line.contains("()") && !line.contains("[]"), "{line:?}");
}

#[test]
fn ignored_denial_details_show_tool_ids_reason_and_the_full_input() {
    let command = format!("echo {}", "y".repeat(300));
    let event = Event::new(
        "exec-1".to_string(),
        ignored_denial("Bash", &command, Some("not allowed"), None),
        1,
    );

    let detail = event_detail_lines(&event).join("\n");

    for expected in ["Bash", "toolu_77", "a1b2c3d4-0000", "not allowed", &command] {
        assert!(detail.contains(expected), "{expected} missing in {detail}");
    }
}

/// The one icon of a row, worked out from the fields the server sends (the
/// state icons plan, U1, U3 and U4).
mod task_app_state_icons {
    use narsor::api::ApiClient;
    use narsor::tui::task_app::list::{state_icon, StateIcon};
    use narsor::tui::task_app::TaskApp;
    use narsor_engine::controller_protocol::TaskSummary;
    use narsor_engine::domain::ExecutionStatus;
    use narsor_engine::orchestration::{AttentionKind, QueueReason, RunnerState, TaskState};

    use super::task_app_support::{
        exhausted, interrupted, listing, queue_view, queued, task, titled, waiting,
        waiting_for_capacity, with_controller, with_runner,
    };

    fn app() -> TaskApp {
        TaskApp::new(
            ApiClient::new("127.0.0.1:9".parse().unwrap(), "token"),
            "local",
            None,
        )
    }

    fn running(id: &str) -> TaskSummary {
        with_controller(with_runner(
            task(id, "local", "running", 1),
            ExecutionStatus::Running,
            "edit",
        ))
    }

    fn in_state(mut task: TaskSummary, state: TaskState) -> TaskSummary {
        task.state = state;
        task
    }

    fn offline(mut task: TaskSummary) -> TaskSummary {
        task.controller = None;
        task
    }

    #[test]
    fn a_running_task_is_running_and_a_problem_wins_over_it() {
        assert_eq!(state_icon(&running("a")), StateIcon::Running);
        let failing = waiting(running("a"), AttentionKind::FailedStep);
        assert_eq!(state_icon(&failing), StateIcon::Problem);
    }

    #[test]
    fn a_question_and_a_proposal_have_the_same_icon() {
        for kind in [AttentionKind::Question, AttentionKind::ConductorProposal] {
            assert_eq!(
                state_icon(&waiting(running("a"), kind)),
                StateIcon::NeedsReply,
                "{kind:?}"
            );
        }
    }

    #[test]
    fn a_finished_and_an_archived_task_have_different_icons() {
        let finished = in_state(running("a"), TaskState::Finished);
        let archived = in_state(running("a"), TaskState::Archived);
        assert_eq!(state_icon(&finished), StateIcon::Done);
        assert_eq!(state_icon(&archived), StateIcon::Archived);
        let left_over = waiting(archived, AttentionKind::Question);
        assert_eq!(state_icon(&left_over), StateIcon::Archived);
    }

    #[test]
    fn a_blocked_queue_and_a_running_task_without_a_controller_are_blocked() {
        let mut offline_queue = queued("a", 1);
        offline_queue.queue = Some(queue_view(QueueReason::ControllerOffline));
        assert_eq!(state_icon(&offline_queue), StateIcon::Blocked);
        assert_eq!(state_icon(&offline(running("a"))), StateIcon::Blocked);
        let plain = waiting_for_capacity(queued("a", 1), 1, 2, 2);
        assert_eq!(state_icon(&plain), StateIcon::Queued);
    }

    #[test]
    fn a_problem_or_a_question_wins_over_a_blocked_queue() {
        let failing = waiting(offline(running("a")), AttentionKind::Problem);
        assert_eq!(state_icon(&failing), StateIcon::Problem);
        let asking = waiting(offline(running("a")), AttentionKind::Question);
        assert_eq!(state_icon(&asking), StateIcon::NeedsReply);
    }

    #[test]
    fn a_task_being_provisioned_is_starting_not_queued() {
        let starting = with_controller(task("a", "local", "provisioning", 1));
        assert_eq!(starting.state, TaskState::Queued);
        assert_eq!(state_icon(&starting), StateIcon::Starting);
    }

    #[test]
    fn an_automatic_resume_is_resuming_and_blocked_without_a_controller() {
        let planned = with_controller(interrupted("a", RunnerState::Running, 1, true));
        assert_eq!(state_icon(&planned), StateIcon::Resuming);
        assert_eq!(state_icon(&offline(planned)), StateIcon::Blocked);
    }

    #[test]
    fn a_resume_that_gave_up_is_a_problem() {
        let given_up = exhausted(
            with_controller(interrupted("a", RunnerState::Running, 6, false)),
            "could not resume automatically: pod evicted",
        );
        assert_eq!(state_icon(&given_up), StateIcon::Problem);
    }

    #[test]
    fn pausing_and_a_stop_by_hand_differ_and_a_policy_pause_is_running() {
        let pausing = in_state(
            with_controller(task("a", "local", "pause_requested", 1)),
            TaskState::Paused,
        );
        assert_eq!(state_icon(&pausing), StateIcon::Pausing);
        let stopped = in_state(
            with_controller(task("a", "local", "stopped", 1)),
            TaskState::Paused,
        );
        assert_eq!(state_icon(&stopped), StateIcon::Stopped);
        let by_policy = in_state(
            with_controller(task("a", "local", "pause_requested", 1)),
            TaskState::Running,
        );
        assert_eq!(state_icon(&by_policy), StateIcon::Running);
    }

    #[test]
    fn an_unknown_state_is_neutral() {
        let unknown = in_state(running("a"), TaskState::Unrecognized);
        assert_eq!(state_icon(&unknown), StateIcon::Unknown);
    }

    #[test]
    fn the_error_state_is_a_problem_without_an_attention_item() {
        let failed = in_state(running("a"), TaskState::Error);
        assert_eq!(state_icon(&failed), StateIcon::Problem);
    }

    #[test]
    fn every_icon_has_a_glyph_and_a_word_of_its_own() {
        let all = [
            StateIcon::New,
            StateIcon::Queued,
            StateIcon::Blocked,
            StateIcon::Starting,
            StateIcon::Running,
            StateIcon::Resuming,
            StateIcon::NeedsReply,
            StateIcon::Problem,
            StateIcon::Pausing,
            StateIcon::Stopped,
            StateIcon::Done,
            StateIcon::Archived,
            StateIcon::Unknown,
            StateIcon::Sending,
        ];
        for (index, icon) in all.iter().enumerate() {
            for other in &all[index + 1..] {
                assert_ne!(icon.glyph(), other.glyph(), "{icon:?} {other:?}");
                assert_ne!(icon.word(), other.word(), "{icon:?} {other:?}");
            }
        }
        assert_eq!(StateIcon::Running.word(), "running");
        assert_eq!(StateIcon::NeedsReply.word(), "awaiting input");
    }

    /// R13a: the counters count icons, and `Space` stops on the same rows.
    #[test]
    fn the_counters_and_space_follow_the_icon_of_the_row() {
        let mut app = app();
        let asking = titled(waiting(running("ask"), AttentionKind::Question), "asking");
        let failing = titled(in_state(running("err"), TaskState::Error), "failing");
        let left_over = titled(
            waiting(
                in_state(running("arc"), TaskState::Archived),
                AttentionKind::Problem,
            ),
            "archived",
        );
        let blocked_asking = titled(
            waiting(offline(running("blk")), AttentionKind::Question),
            "blocked asking",
        );
        let calm = titled(running("calm"), "calm");
        app.load(listing(
            vec![calm, left_over, asking, failing, blocked_asking],
            "c",
        ));
        let counters = app.list().counters();
        assert_eq!((counters.questions, counters.problems), (2, 1));

        let mut stops = Vec::new();
        for _ in 0..3 {
            assert!(app.list_mut().select_next_waiting());
            stops.push(app.list().selected_id().map(str::to_string).unwrap());
        }
        assert_eq!(stops, ["ask", "err", "blk"]);
    }

    /// R12b: `z` hides finished and archived tasks by state.
    #[test]
    fn z_hides_finished_and_archived_tasks() {
        let mut app = app();
        app.load(listing(
            vec![
                titled(in_state(running("a"), TaskState::Finished), "finished"),
                titled(in_state(running("b"), TaskState::Archived), "archived"),
                titled(running("c"), "running"),
            ],
            "c",
        ));
        app.list_mut().toggle_hide_done();
        assert_eq!(app.list().hidden_count(), 2);
        assert_eq!(app.list().visible_ids(), ["c"]);
    }

    /// R9–R12: the third line holds the step or the place and never a word of
    /// the state.
    #[test]
    fn the_label_is_the_step_or_the_place_and_nothing_else() {
        let mut app = app();
        app.load(listing(
            vec![
                titled(running("a"), "stepping"),
                titled(waiting_for_capacity(queued("b", 2), 1, 2, 2), "waiting"),
                titled(in_state(running("c"), TaskState::Finished), "finished"),
                titled(in_state(running("d"), TaskState::Error), "failed"),
                titled(
                    in_state(
                        with_controller(task("e", "local", "stopped", 4)),
                        TaskState::Paused,
                    ),
                    "stopped",
                ),
            ],
            "c",
        ));
        let labels: Vec<String> = app.list().rows().into_iter().map(|row| row.label).collect();
        assert_eq!(labels, ["edit", "#1", "", "", ""]);
    }
}
