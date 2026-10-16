//! Tests for the TUI's state layer.
//!
//! Test the state, not the rendering: what is asserted here is what the
//! interface *decides* — which tasks it lists and in what order, what a key
//! press does to it, what it refuses to send, which request it makes of the
//! API. The application is [`narsor::tui::task_app::TaskApp`], driven over a
//! real control plane (`remote_support::RemoteHarness`) where the answer of a
//! server matters and over a client that reaches nobody where it does not.
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

mod engine_support;
mod environment_support;
mod remote_support;
mod task_app_support;

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
        },
        EventData::ActionCompleted {
            action_execution_id: "a1".to_string(),
            output_label: "success".to_string(),
        },
        EventData::ActionFailed {
            action_execution_id: "a1".to_string(),
            error: "boom".to_string(),
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
            question: None,
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
    assert_eq!(transcript.scroll_offset(), 3);
    assert!(transcript.is_following_end());

    transcript.append(vec![transcript_entry("three", &["three-a"])]);
    assert_eq!(transcript.scroll_offset(), 5);
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

    transcript.toggle_expanded(&TranscriptEntryId::from("two"));
    assert_eq!(transcript.anchor(), Some(anchor));
    transcript.set_mode(DisplayMode::Raw);
    assert_eq!(
        transcript.anchor().unwrap().entry_id,
        TranscriptEntryId::from("two")
    );
    transcript.set_viewport(1);
    assert_eq!(
        transcript.anchor().unwrap().entry_id,
        TranscriptEntryId::from("two")
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
    assert_eq!(lines, vec!["first", "", "second"]);
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
                .flat_map(|entry| entry.keys.split('/').map(str::to_string))
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
                Action::PauseResume,
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
            for (character, action) in [
                ('K', Action::MoveUp),
                ('J', Action::MoveDown),
                ('m', Action::MoveMode),
                ('p', Action::PauseResume),
                ('x', Action::Interrupt),
                ('t', Action::Transition),
                ('s', Action::Restart),
                ('a', Action::Archive),
                ('o', Action::OpenLink),
            ] {
                for context in [Context::List, Context::Details, Context::Menu] {
                    assert_eq!(
                        lookup(context, &press(KeyCode::Char(character))),
                        Some(action),
                        "{character} in {}",
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
            type_text(&mut chooser, "abc");
            chooser.on_key(ctrl_j());
            type_text(&mut chooser, "def");
            chooser.on_key(press(KeyCode::Up));
            type_text(&mut chooser, "X");
            assert_eq!(chooser.fields_of(0)[1].value(), "abcX\ndef");
            chooser.on_key(press(KeyCode::Down));
            type_text(&mut chooser, "Y");
            assert_eq!(chooser.fields_of(0)[1].value(), "abcX\ndefY");
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
        fn a_form_of_inputs_starts_on_the_required_text_and_esc_goes_back_to_the_list() {
            let mut chooser = Chooser::new(vec![ChooserItem::new("ship", "Wysyła.")
                .with_field(ChooserField::text("ticket"))
                .with_field(ChooserField::multiline("notes"))
                .with_field(ChooserField::multiline("prompt").required())])
            .with_inputs_layout();
            assert_eq!(chooser.on_key(press(KeyCode::Enter)), ChooserAction::None);
            type_text(&mut chooser, "p");
            assert_eq!(chooser.fields_of(0)[2].value(), "p");
            // Short fields come first in the walk; arrows move between fields.
            chooser.on_key(press(KeyCode::Backspace));
            chooser.on_key(press(KeyCode::Up));
            chooser.on_key(press(KeyCode::Up));
            type_text(&mut chooser, "t");
            assert_eq!(chooser.fields_of(0)[0].value(), "t");
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
            assert_eq!(styles.of(Role::Conductor).fg, Some(Color::Magenta));
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
/// in); what talks to the API uses the served control plane of `RemoteHarness`.
mod task_app_list {
    use std::time::{Duration, Instant};

    use crossterm::event::KeyCode;
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::{TcpListener, TcpStream};

    use narsor::api::{ApiClient, ClientError};
    use narsor::tui::connection::LinkState;
    use narsor::tui::task_app::list::{Attention, StateIcon};
    use narsor::tui::task_app::notify::Severity;
    use narsor::tui::task_app::{layout, DetailTab, EventFeed, Focus, Request, TaskApp};
    use narsor_engine::controller_protocol::MoveTaskRequest;
    use narsor_engine::domain::ExecutionStatus;
    use narsor_engine::orchestration::{
        AttentionKind, Observation, QueueReason, TaskEventKind, TaskLifecycle,
    };
    use narsor_protocol::controller::MoveTaskTarget;

    use super::key;
    use super::remote_support::RemoteHarness;
    use super::task_app_support::{
        event, ids, listing, queued, task, titled, waiting, waiting_for_capacity, with_controller,
        with_runner,
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
            TaskLifecycle::Queued,
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
            TaskLifecycle::Queued,
            TaskEventKind::Moved {
                rank: 3,
                order: ids(&["a", "b", "c"]),
            },
        ));
        assert_eq!(titles(&app), ["c", "a", "b"]);
    }

    fn row(app: &TaskApp, title: &str) -> (Attention, StateIcon, String) {
        let row = app
            .list()
            .rows()
            .into_iter()
            .find(|row| row.title == title)
            .unwrap_or_else(|| panic!("no row {title}"));
        (row.attention, row.icon, row.label)
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
        lifecycle: &str,
        rank: u32,
    ) -> narsor_engine::controller_protocol::TaskSummary {
        with_controller(task(id, backend, lifecycle, rank))
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
            assert_eq!(row(&app, "b").2, "#2");

            let mut sequence = 1;
            for (index, id) in ["a", "b", "c"].into_iter().enumerate() {
                sequence += 1;
                app.on_stream(event(
                    &format!("p{id}"),
                    id,
                    sequence,
                    TaskLifecycle::Provisioning,
                    TaskEventKind::ControllerReported {
                        observation: Observation::StartAccepted,
                        lease_epoch: 1,
                    },
                ));
                assert_eq!(titles(&app), order, "{id} provisioning");
                assert_eq!(row(&app, id).1, StateIcon::Provisioning);
                // Whoever waits behind moves up in the queue, not on screen.
                let waiting_behind = ["a", "b", "c"].iter().skip(index + 1).count();
                if waiting_behind > 0 {
                    let next = ["a", "b", "c"][index + 1];
                    assert_eq!(row(&app, next).2, "#1");
                }

                sequence += 1;
                app.on_stream(event(
                    &format!("r{id}"),
                    id,
                    sequence,
                    TaskLifecycle::Running,
                    TaskEventKind::ControllerReported {
                        observation: Observation::RunnerReady,
                        lease_epoch: 1,
                    },
                ));
                assert_eq!(titles(&app), order, "{id} running");
                assert_eq!(row(&app, id).1, StateIcon::Running);

                // The debounced read brings the runner's step.
                let mut tasks = app.list().tasks().to_vec();
                tasks[index] =
                    with_runner(tasks[index].clone(), ExecutionStatus::Running, "implement");
                tasks[index].revision = sequence;
                app.load(listing(tasks, "read"));
                assert_eq!(row(&app, id).2, "implement");

                sequence += 1;
                app.on_stream(event(
                    &format!("d{id}"),
                    id,
                    sequence,
                    TaskLifecycle::Running,
                    TaskEventKind::RunnerReported {
                        incarnation: "pod-1".into(),
                        event_cursor: 9,
                        execution_status: ExecutionStatus::Completed,
                        pending_input_id: None,
                        attention: Vec::new(),
                    },
                ));
                assert_eq!(titles(&app), order, "{id} done");
                assert_eq!(
                    row(&app, id),
                    (Attention::None, StateIcon::Done, "done".into())
                );
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
                TaskLifecycle::Queued,
                TaskEventKind::Created,
            ));
            assert_eq!(row(&app, "a").1, StateIcon::Running);
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
        fn an_event_for_an_unknown_task_asks_for_a_debounced_read() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1)], "c"));
            app.on_stream(event(
                "n",
                "new",
                1,
                TaskLifecycle::Queued,
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
            assert_eq!(row(&app, "a").0, Attention::Question);

            let waiting_id = app.list().tasks()[0].attention[0].waiting_id.clone();
            app.on_stream(event(
                "i",
                "a",
                2,
                TaskLifecycle::Running,
                TaskEventKind::InputQueued {
                    input_id: "in-1".into(),
                    pending_input_id: waiting_id,
                },
            ));
            assert_eq!(row(&app, "a").0, Attention::None);
            assert!(!app.refresh_wanted(), "an answer shows without a read");

            app.on_stream(event(
                "q",
                "b",
                2,
                TaskLifecycle::Queued,
                TaskEventKind::QueueChanged {
                    reason: QueueReason::ControllerOffline,
                },
            ));
            assert_eq!(row(&app, "b").2, "offline");
            assert!(!app.refresh_wanted());
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
            assert_eq!(row(&app, "a").0, Attention::Problem);
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
            assert_eq!(app.detail().tab(), DetailTab::Conversation);

            let mut moved = vec![
                controlled("a", "local", "running", 1),
                controlled("b", "local", "queued", 2),
                controlled("c", "local", "queued", 3),
            ];
            moved[1].revision = 4;
            app.load(listing(moved, "c2"));

            assert_eq!(app.list().selected_id(), Some("b"));
            assert_eq!(app.focus(), Focus::Details);
            assert_eq!(app.detail().tab(), DetailTab::Conversation);
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
                TaskLifecycle::Archived,
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

        #[tokio::test]
        async fn archived_through_the_api_the_selected_task_hands_over_on_the_next_read() {
            let harness = RemoteHarness::start().await;
            for id in ["t1", "t2", "t3"] {
                harness.create(id, None).await;
            }
            let mut app = TaskApp::new(harness.owner.clone(), "local", None);
            app.refresh().await;
            assert_eq!(titles(&app), ["t1", "t2", "t3"]);
            ch(&mut app, 'j');
            assert_eq!(app.list().selected_id(), Some("t2"));

            harness
                .owner
                .control_command(
                    "t2",
                    &narsor_engine::controller_protocol::OwnerCommandRequest {
                        command: narsor_engine::orchestration::OwnerCommand::Archive,
                        idempotency_key: "archive-t2".into(),
                        expected_generation: None,
                    },
                )
                .await
                .expect("archive");
            app.refresh().await;
            assert_eq!(titles(&app), ["t1", "t3"]);
            assert_eq!(app.list().selected_id(), Some("t3"));
            assert!(note(&app).contains("t2"), "{}", note(&app));
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

        fn expect_move(request: Request) -> narsor::tui::task_app::list::MoveRequest {
            match request {
                Request::Move(request) => request,
                other => panic!("a move was expected, got {other:?}"),
            }
        }

        #[test]
        fn k_and_j_ask_to_go_before_the_row_above_and_after_the_row_below() {
            let mut app = app();
            app.load(listing(
                vec![queued("a", 1), queued("b", 2), queued("c", 3)],
                "c",
            ));
            ch(&mut app, 'j');
            let up = expect_move(ch(&mut app, 'K'));
            assert_eq!(up.target, MoveTaskTarget::Before("a".into()));
            let down = expect_move(ch(&mut app, 'J'));
            assert_eq!(down.target, MoveTaskTarget::After("c".into()));
            assert_eq!(down.generation, 1);
        }

        #[test]
        fn k_on_the_first_row_and_j_on_the_last_say_there_is_nowhere_to_go() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1), queued("b", 2)], "c"));
            assert_eq!(ch(&mut app, 'K'), Request::None);
            assert!(note(&app).contains("pierwsze"), "{}", note(&app));
            ch(&mut app, 'G');
            assert_eq!(ch(&mut app, 'J'), Request::None);
            assert!(note(&app).contains("ostatnie"), "{}", note(&app));
        }

        #[test]
        fn k_on_a_running_task_says_so_and_sends_nothing() {
            let mut app = app();
            app.load(listing(
                vec![queued("a", 1), controlled("b", "local", "running", 2)],
                "c",
            ));
            ch(&mut app, 'j');
            assert_eq!(ch(&mut app, 'K'), Request::None);
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

        #[tokio::test]
        async fn a_move_the_server_refuses_reloads_the_list_and_says_why() {
            let harness = RemoteHarness::start().await;
            for id in ["t1", "t2", "t3"] {
                harness.create(id, None).await;
            }
            let mut app = TaskApp::new(harness.owner.clone(), "local", None);
            app.refresh().await;
            assert_eq!(titles(&app), ["t1", "t2", "t3"]);

            // Another client archives t1 behind the list's back.
            harness
                .owner
                .control_command(
                    "t1",
                    &narsor_engine::controller_protocol::OwnerCommandRequest {
                        command: narsor_engine::orchestration::OwnerCommand::Archive,
                        idempotency_key: "archive-t1".into(),
                        expected_generation: None,
                    },
                )
                .await
                .expect("archive");

            let request = ch(&mut app, 'J');
            app.perform(request).await;
            let text = note(&app);
            assert!(text.contains("odrzucone") && text.contains("t1"), "{text}");
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Error);
            assert_eq!(titles(&app), ["t2", "t3"], "the list was read again");
            assert_eq!(app.list().selected_id(), Some("t2"));
        }

        /// `m` `j` `j` `Enter` over the real API: a `move --after` goes out
        /// and the `Moved` event patches the list without a read.
        #[tokio::test]
        async fn a_moved_event_patches_the_list_without_reading_it_again() {
            let harness = RemoteHarness::start().await;
            for id in ["t1", "t2", "t3"] {
                harness.create(id, None).await;
            }
            let mut app = TaskApp::new(harness.owner.clone(), "local", None);
            app.refresh().await;
            let mut feed = EventFeed::new(harness.owner.clone());
            feed.ensure_open(&app);

            ch(&mut app, 'm');
            ch(&mut app, 'j');
            ch(&mut app, 'j');
            let request = press(&mut app, KeyCode::Enter);
            let Request::Move(move_request) = &request else {
                panic!("a move was expected");
            };
            assert!(matches!(move_request.target, MoveTaskTarget::After(_)));
            app.perform(request).await;
            assert_eq!(titles(&app), ["t1", "t2", "t3"], "no read, no event yet");
            assert!(note(&app).contains("przesunięto za t3"), "{}", note(&app));

            let before_cursor = app.cursor().map(str::to_string);
            for _ in 0..5 {
                if titles(&app) == ["t2", "t3", "t1"] {
                    break;
                }
                let item = tokio::time::timeout(Duration::from_secs(5), feed.next())
                    .await
                    .expect("the Moved event arrives");
                app.on_stream(item);
            }
            assert_eq!(titles(&app), ["t2", "t3", "t1"]);
            assert_ne!(app.cursor().map(str::to_string), before_cursor);
            assert_eq!(app.list().selected_id(), Some("t1"));
            let ranks: Vec<u32> = app.list().tasks().iter().map(|task| task.rank).collect();
            assert_eq!(ranks, [1, 2, 3]);
            assert!(!app.refresh_wanted(), "a move is patched, not re-read");
        }

        /// The stream drops; whatever happens meanwhile is delivered when it
        /// resumes from the cursor of the last event applied.
        #[tokio::test]
        async fn a_dropped_stream_resumes_from_its_cursor_and_loses_no_move() {
            let harness = RemoteHarness::start().await;
            for id in ["t1", "t2", "t3"] {
                harness.create(id, None).await;
            }
            let mut app = TaskApp::new(harness.owner.clone(), "local", None);
            app.refresh().await;
            let mut feed = EventFeed::new(harness.owner.clone());
            feed.ensure_open(&app);

            let top = MoveTaskRequest {
                target: MoveTaskTarget::Top,
                expected_generation: None,
            };
            // A first move, seen live.
            harness
                .owner
                .control_move_task("t3", &top)
                .await
                .expect("move");
            for _ in 0..5 {
                if titles(&app) == ["t3", "t1", "t2"] {
                    break;
                }
                let item = tokio::time::timeout(Duration::from_secs(5), feed.next())
                    .await
                    .expect("the first Moved arrives");
                app.on_stream(item);
            }
            assert_eq!(titles(&app), ["t3", "t1", "t2"]);

            // The connection breaks; a second move happens while it is down.
            feed.close();
            assert!(!feed.is_open());
            harness
                .owner
                .control_move_task("t2", &top)
                .await
                .expect("move");
            assert_eq!(
                titles(&app),
                ["t3", "t1", "t2"],
                "the app has not heard of it"
            );

            feed.ensure_open(&app);
            for _ in 0..5 {
                if titles(&app) == ["t2", "t3", "t1"] {
                    break;
                }
                let item = tokio::time::timeout(Duration::from_secs(5), feed.next())
                    .await
                    .expect("the second Moved arrives after the resume");
                app.on_stream(item);
            }
            assert_eq!(titles(&app), ["t2", "t3", "t1"]);
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
                TaskLifecycle::Provisioning,
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

        #[tokio::test]
        async fn an_unreachable_server_opens_as_connecting_and_goes_live_without_a_restart() {
            // Reserve a port, and let nothing listen on it yet.
            let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = reserved.local_addr().unwrap();
            drop(reserved);

            let harness = RemoteHarness::start().await;
            harness.create("t1", None).await;
            let client = ApiClient::new(address, harness.handle.token());
            let mut app = TaskApp::new(client.clone(), "moj-serwer", None);

            app.refresh().await;
            assert_eq!(app.connection().state(), LinkState::Connecting);
            assert_eq!(status(&app), "◌ łączę z moj-serwer");
            assert!(app.list().tasks().is_empty());
            assert!(app.cursor().is_none());
            let mut feed = EventFeed::new(client);
            feed.ensure_open(&app);
            assert!(!feed.is_open(), "no cursor yet, so no stream");
            assert!(!app.mutation_allowed());

            // The server comes up on that address.
            let listener = TcpListener::bind(address)
                .await
                .expect("bind the reserved port");
            let target = harness.handle.addr();
            tokio::spawn(async move {
                loop {
                    let Ok((mut inbound, _)) = listener.accept().await else {
                        return;
                    };
                    tokio::spawn(async move {
                        if let Ok(mut outbound) = TcpStream::connect(target).await {
                            let _ =
                                tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                        }
                        let _ = inbound.shutdown().await;
                    });
                }
            });

            // Not due yet; due once the retry delay has passed.
            assert!(!app.refresh_if_due(Instant::now()).await);
            assert!(
                app.refresh_if_due(Instant::now() + Duration::from_secs(2))
                    .await
            );
            assert_eq!(status(&app), "● live");
            assert_eq!(titles(&app), ["t1"]);
            assert_eq!(
                app.connection().name(),
                "local",
                "the server's own name replaces the configured one"
            );
            feed.ensure_open(&app);
            assert!(feed.is_open());
            assert!(app.mutation_allowed());
        }
    }
}

mod task_app_detail {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use narsor::api::{ApiClient, ClientError, ConversationEntryDto, ConversationResponse};
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
        ConversationUnavailableReason, ExecutionEventsResponse, ExecutionEventsSource, RunnerClient,
    };
    use narsor_engine::domain::{Event, ExecutionStatus};
    use narsor_engine::engine::{ConversationState, PositionStatus, Timeline};
    use narsor_engine::orchestration::{Observation, QueueReason, TaskEventKind, TaskLifecycle};

    use super::remote_support::{report, snapshot, RemoteHarness};
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

    /// An app over running `a` and `b`, with the details of `a` open.
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
            let mut app = details_app();
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
                TaskLifecycle::Running,
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
            let mut app = details_app();
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
            let mut app = details_app();
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
            let mut app = details_app();
            read_timeline(&mut app, "a", 1, &["plan", "edit", "review"], "review #1");
            ch(&mut app, 'k');
            ch(&mut app, 'k');
            assert_eq!(selected_step(&app, "a").as_deref(), Some("plan #1"));
            press(&mut app, KeyCode::Char(']'));
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
            let mut app = details_app();
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
            let mut app = details_app();
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

        fn selected_entry(app: &TaskApp, state: &str) -> Option<String> {
            app.detail()
                .view("a")?
                .attempts()
                .get(&format!("act-{state}"))?
                .transcript()
                .selected_entry()
                .map(|id| id.as_str().to_string())
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
            let mut app = details_app();
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
            let mut app = details_app();
            read_recorded(&mut app, 1, &["plan"], "plan #1");
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.step_focus(), StepFocus::Axis, "not read yet");

            // Recorded, but without the attempt's id.
            let mut app = details_app();
            let mut step = recorded("plan", PositionStatus::Succeeded);
            step.action_execution_id = None;
            read_positions(&mut app, 1, vec![step], "plan #1");
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.step_focus(), StepFocus::Axis, "no attempt");

            // Never created.
            let mut app = details_app();
            let mut step = position("plan", PositionStatus::Failed);
            step.conversation = ConversationState::NotCreated {
                reason: "the harness did not start".into(),
            };
            read_positions(&mut app, 1, vec![step], "plan #1");
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.step_focus(), StepFocus::Axis, "never created");

            // The runner holds no record of it.
            let mut app = details_app();
            read_recorded(&mut app, 1, &["plan"], "plan #1");
            read_step(&mut app, 1, "plan", 0, attempt_read(&[], 0, true));
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.step_focus(), StepFocus::Axis, "no record");

            // Running, nothing said yet.
            let mut app = details_app();
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
            let mut app = details_app();
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
            press(&mut app, KeyCode::Char(']'));
            assert_eq!(app.detail().tab(), DetailTab::Conversation);
            assert_eq!(layer(&app), Some(Context::AgentConversation));
            press(&mut app, KeyCode::Char('['));
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
                Some("jeszcze nie wystartowało, provisioning")
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

        #[test]
        fn the_conversation_is_every_attempt_in_the_order_of_the_steps() {
            let recorded = |state: &str| {
                let mut step = position(state, PositionStatus::Succeeded);
                step.conversation = ConversationState::Recorded;
                step
            };
            let positions = vec![
                recorded("plan"),
                position("script", PositionStatus::Succeeded),
                recorded("edit"),
            ];
            let mut attempts = Attempts::default();
            attempts.apply(
                "act-plan",
                1,
                0,
                Ok(read(vec![entry("1", "plan one"), entry("2", "plan two")])),
            );
            let mut conversation = Conversation::new("a");
            conversation.rebuild(&positions, &attempts);
            assert!(conversation.is_loading(), "act-edit was not read yet");
            assert_eq!(
                conversation.transcript().entries().len(),
                3,
                "a heading and two entries"
            );

            attempts.apply("act-edit", 1, 0, Ok(read(vec![entry("1", "edit one")])));
            conversation.rebuild(&positions, &attempts);
            assert!(!conversation.is_loading());
            let lines: Vec<String> = conversation
                .transcript()
                .logical_lines()
                .into_iter()
                .map(|line| line.text)
                .collect();
            let at = |needle: &str| lines.iter().position(|line| line.contains(needle)).unwrap();
            assert!(
                at("plan #1") < at("plan one")
                    && at("plan two") < at("edit #1")
                    && at("edit #1") < at("edit one"),
                "{lines:?}"
            );

            attempts.apply(
                "act-plan",
                2,
                3,
                Err(ReadFailure::Failed("the runner did not answer".into())),
            );
            conversation.rebuild(&positions, &attempts);
            let failure = conversation.failure().expect("the failure");
            assert_eq!(failure.detail(), "the runner did not answer");
            assert_eq!(
                failure.note(Some("plan #1")).0,
                "✗ nie udało się wczytać rozmowy (plan #1: the runner did not answer)"
            );
            assert_eq!(
                conversation.transcript().entries().len(),
                5,
                "what was read stays"
            );
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

        /// The runner's side of one relayed events read.
        async fn answer_events(runner: &RunnerClient, events: Vec<Event>, has_more: bool) {
            loop {
                let poll = runner.poll(None, 5).await.expect("poll");
                if let Some(request) = poll.execution_events.first() {
                    let page = narsor_engine::controller_protocol::ExecutionEventsPage {
                        events,
                        has_more,
                    };
                    runner
                        .upload_execution_events(&request.request_id, &page)
                        .await
                        .expect("upload");
                    return;
                }
            }
        }

        /// The runner's side of one relayed conversation page.
        async fn answer_page(runner: &RunnerClient, page: ConversationResponse) {
            loop {
                let poll = runner.poll(None, 5).await.expect("poll");
                if let Some(request) = poll.conversation_pages.first() {
                    runner
                        .upload_conversation_page(&request.request_id, &page)
                        .await
                        .expect("upload");
                    return;
                }
            }
        }

        fn live_snapshot() -> narsor_engine::orchestration::RunnerSnapshot {
            let mut recorded = position("plan", PositionStatus::Succeeded);
            recorded.conversation = ConversationState::Recorded;
            let current = position("review", PositionStatus::InProgress);
            let mut first = snapshot(1, None);
            first.timeline = Some(Timeline {
                default_position: Some(current.label.clone()),
                positions: vec![recorded, current],
                ..Timeline::default()
            });
            first
        }

        async fn app_over(harness: &RemoteHarness) -> TaskApp {
            let client = harness.owner.clone();
            let mut app = TaskApp::new(client, "remote", None);
            app.refresh().await;
            assert_eq!(app.list().tasks().len(), 1);
            press(&mut app, KeyCode::Enter);
            app
        }

        #[tokio::test]
        async fn the_timeline_of_a_remote_task_is_read_and_followed() {
            let harness = RemoteHarness::start().await;
            harness.running("task-1", "k8s", live_snapshot()).await;
            let mut app = app_over(&harness).await;

            app.sync_detail().await;
            let view = app.detail().view("task-1").unwrap();
            assert_eq!(view.progress().positions().len(), 2);
            assert_eq!(view.progress().selected_label(), Some("review #1"));
            assert!(app.connection().is_live());
        }

        /// R14: a controller that stopped answering turns what it last
        /// observed stale; it does not turn the task into some other state.
        #[tokio::test]
        async fn a_disconnected_controller_shows_a_stale_last_known_state() {
            use narsor::tui::style::Styles;
            use narsor::tui::task_app::tabs::info;

            let harness = RemoteHarness::start().await;
            harness.running("task-1", "k8s", snapshot(1, None)).await;
            let mut app = TaskApp::new(harness.owner.clone(), "remote", None);
            app.refresh().await;
            let sheet = |app: &TaskApp| -> Vec<String> {
                let task = app.list().selected().expect("a selected task");
                info::lines(task, &Styles::new(false), false)
                    .iter()
                    .map(|line| line.to_string())
                    .collect()
            };
            let live = sheet(&app);
            assert!(live.iter().any(|line| line.contains("online")), "{live:#?}");
            assert!(
                !live.iter().any(|line| line.contains("nieaktualne")),
                "{live:#?}"
            );

            harness.clock.advance(chrono::Duration::minutes(10));
            app.refresh().await;
            let task = app.list().selected().expect("a selected task");
            assert_eq!(task.lifecycle, TaskLifecycle::Running, "never invented");
            let stale = sheet(&app);
            assert!(
                stale.iter().any(|line| line.contains("offline")),
                "{stale:#?}"
            );
            assert!(
                stale
                    .iter()
                    .any(|line| line.contains("obserwacja") && line.contains("nieaktualne")),
                "{stale:#?}"
            );
            assert!(
                stale.iter().any(|line| line.contains("zgłoszenie")
                    && line.contains("temu")
                    && line.contains("nieaktualne")),
                "{stale:#?}"
            );
        }

        #[tokio::test]
        async fn events_of_a_live_runner_travel_through_the_relay() {
            let harness = RemoteHarness::start().await;
            let running = harness.running("task-1", "k8s", live_snapshot()).await;
            let mut app = app_over(&harness).await;
            press(&mut app, KeyCode::Char(']'));
            press(&mut app, KeyCode::Char(']'));
            assert_eq!(app.detail().tab(), DetailTab::Events);

            let (_, ()) = tokio::join!(
                app.sync_detail(),
                answer_events(&running.runner, execution_events(4), false)
            );
            let events = app.detail().view("task-1").unwrap().events();
            assert_eq!(events.events().len(), 4);
            assert_eq!(
                *events.availability(),
                Availability::Available(ExecutionEventsSource::Runner)
            );
        }

        #[tokio::test]
        async fn a_remote_task_without_a_live_runner_says_why_in_the_events_tab() {
            let harness = RemoteHarness::start().await;
            let running = harness.running("task-1", "k8s", live_snapshot()).await;
            running
                .controller
                .report(&report(
                    "task-1",
                    running.epoch,
                    1,
                    Observation::WorkloadLost { state_known: true },
                    Some(1),
                ))
                .await
                .expect("lost");
            let mut app = app_over(&harness).await;
            assert_eq!(
                app.list().selected().unwrap().lifecycle,
                TaskLifecycle::Interrupted
            );
            press(&mut app, KeyCode::Char(']'));
            press(&mut app, KeyCode::Char(']'));

            app.sync_detail().await;
            match app.detail().view("task-1").unwrap().events().availability() {
                Availability::Unavailable { reason, message } => {
                    assert_eq!(*reason, ConversationUnavailableReason::Interrupted);
                    assert!(message.contains("runner is not running"), "{message}");
                }
                other => panic!("expected the reason, got {other:?}"),
            }
            assert!(app.connection().is_live());
        }

        #[tokio::test]
        async fn the_conversation_pages_of_a_recorded_step_come_through_the_relay() {
            let harness = RemoteHarness::start().await;
            let running = harness.running("task-1", "k8s", live_snapshot()).await;
            let mut app = app_over(&harness).await;
            press(&mut app, KeyCode::Char(']'));
            assert_eq!(app.detail().tab(), DetailTab::Conversation);

            let page = ConversationResponse::Recorded {
                entries: vec![ConversationEntryDto {
                    entry_number: 1,
                    timestamp: chrono::Utc::now(),
                    source: narsor::api::ConversationEntrySource::Narsor,
                    raw_base64: {
                        use base64::Engine as _;
                        base64::engine::general_purpose::STANDARD
                            .encode(r#"{"event":"prompt","prompt":"Fix it"}"#)
                    },
                    elements: Vec::new(),
                }],
                next_entry: 2,
                closed: true,
                truncated_tail: false,
                source: None,
                recorded_at: None,
            };
            let (_, ()) = tokio::join!(app.sync_detail(), answer_page(&running.runner, page));
            let view = app.detail().view("task-1").unwrap();
            let lines: Vec<String> = view
                .conversation()
                .transcript()
                .logical_lines()
                .into_iter()
                .map(|line| line.text)
                .collect();
            assert!(
                lines.iter().any(|line| line.contains("plan #1")),
                "{lines:?}"
            );
            assert!(
                lines.iter().any(|line| line.contains("Fix it")),
                "{lines:?}"
            );
            assert!(view.attempts().get("act-plan").is_some());
        }

        /// AE6: a remote task without its runner keeps today's answer — the
        /// conversation is unavailable, with the server's reason, and is not
        /// read from any record.
        #[tokio::test]
        async fn a_remote_task_without_a_live_runner_says_its_conversation_is_unavailable() {
            let harness = RemoteHarness::start().await;
            let running = harness.running("task-1", "k8s", live_snapshot()).await;
            running
                .controller
                .report(&report(
                    "task-1",
                    running.epoch,
                    1,
                    Observation::WorkloadLost { state_known: true },
                    Some(1),
                ))
                .await
                .expect("lost");
            let mut app = app_over(&harness).await;
            press(&mut app, KeyCode::Char(']'));
            assert_eq!(app.detail().tab(), DetailTab::Conversation);

            app.sync_detail().await;
            let conversation = app.detail().view("task-1").unwrap().conversation();
            let failure = conversation.failure().expect("a reason");
            assert!(
                matches!(failure, ReadFailure::Unavailable(_)),
                "{failure:?}"
            );
            let (text, _) = failure.note(None);
            assert!(
                text.starts_with("◌ rozmowa niedostępna: ")
                    && text.contains("cannot be read right now"),
                "{text}"
            );
            assert_eq!(conversation.record(), None);
            assert!(app.connection().is_live());
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
            assert_eq!(app.detail().tab(), DetailTab::Progress);
            ch(&mut app, ']');
            assert_eq!(app.detail().tab(), DetailTab::Conversation);
            ch(&mut app, '[');
            ch(&mut app, '[');
            assert_eq!(app.detail().tab(), DetailTab::Info, "wraps round");
        }

        #[test]
        fn przebieg_has_the_axis_layer_and_every_other_tab_the_reading_one() {
            let mut app = details_app();
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
            let mut app = details_app();
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
            let mut app = details_app();
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
    use narsor::tui::task_app::waiting::{AnswerRequest, Submit};
    use narsor::tui::task_app::{Focus, Request, TaskApp};
    use narsor_engine::engine::actions::human::HumanInputAction;
    use narsor_engine::engine::PositionStatus;
    use narsor_engine::orchestration::{
        AttentionKind, OperatorCommand, OwnerCommand, TaskEventKind, TaskLifecycle,
    };
    use narsor_protocol::controller::ConductorView;
    use narsor_protocol::controller::{ConductorReadResponse, ExecutionEventsSource};

    use super::remote_support::{snapshot, RemoteHarness};
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

    fn press(app: &mut TaskApp, code: KeyCode) -> Request {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
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
            Submit::Input { delivery, .. } => delivery,
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
                    { "label": "implemented", "data": [], "variables": ["summary"] },
                    { "label": "needs_human_decision", "data": ["question"], "variables": [] }
                ],
                "analysis": if proposed { json!({
                    "label": "implemented",
                    "data": {},
                    "variables": { "summary": "all units done", "files": 3 },
                    "rationale": "Odpowiedź mówi, że testy przechodzą."
                }) } else { Value::Null }
            },
            "episode": "ep-p",
            "requested_at": "2026-10-09T11:54:00Z"
        })
    }

    fn gate_app() -> TaskApp {
        opened(with_pending(running("a", 1), gate("ep-1")))
    }

    // -- characterisation of the form semantics --------------------------------

    #[test]
    fn a_resume_comment_is_required_only_for_the_output_that_resumes_the_agent() {
        // 3f3dab2: `approved` has no need of the comment, `revise ↩` does.
        let mut app = gate_app();
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

        // The panel opens on the output that returns the work to the agent.
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
    fn an_approval_returns_to_the_agent_by_default_and_accepts_on_request() {
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
            "Zwróć agentowi",
            "nothing moves on by a stray Enter"
        );

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
        assert!(chooser_of(&app)
            .items()
            .iter()
            .all(|item| item.label != "Napisz do agenta"));
        press(&mut app, KeyCode::Down);
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
        assert!(enter_until_error(&mut app), "data.question is required");
        assert_eq!(chooser_of(&app).focus(), ChooserFocus::Fields);
        let field = &chooser_of(&app).fields_of(1)[0];
        assert_eq!(field.name, "data.question");
        assert!(field
            .error()
            .is_some_and(|error| error.contains("data.question")));

        typed(&mut app, "Konto czy użytkownik?");
        // A key of the operator's own, beside the ones the label needs.
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        typed(&mut app, "scope=account");
        let delivery = delivery(enter_until_answer(&mut app));
        assert_eq!(
            delivery.chosen_label.as_deref(),
            Some("needs_human_decision")
        );
        assert_eq!(delivery.data["question"], "Konto czy użytkownik?");
        assert_eq!(delivery.variables["scope"], "account");
    }

    #[test]
    fn a_key_given_twice_or_without_a_value_is_refused_at_the_panel() {
        let mut app = opened(with_pending(running("a", 1), problem(false)));
        ch(&mut app, '1');
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "x");
        press(&mut app, KeyCode::Down);
        typed(&mut app, "x=1");
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
        press(&mut app, KeyCode::Down);
        typed(&mut app, "bez znaku");
        assert!(enter_until_error(&mut app), "a key without '=' is refused");
        assert!(app.detail().view("a").unwrap().waiting().error().is_some());
    }

    #[test]
    fn the_conductors_summary_of_a_gate_recommends_an_output_with_the_diamond() {
        let pending: narsor_protocol::api::PendingHumanInputView =
            serde_json::from_value(gate("ep-1")).unwrap();
        let mut panel = narsor::tui::input_form::InputPanel::new(&pending, &|_| None);
        let annotation: narsor::api::AnnotationDto = serde_json::from_value(json!({
            "key": "ep-1", "summary": "brak planu migracji", "recommendation": "reject",
            "created_at": "2026-10-09T12:00:00Z"
        }))
        .unwrap();
        panel.annotate(&annotation);
        assert_eq!(panel.items()[panel.selected()].label, "reject");
        assert!(panel.items()[2].recommended);
        assert!(panel
            .context()
            .iter()
            .any(|line| line.text.contains("◆ dyrygent: brak planu migracji")));

        // A summary of another request changes nothing.
        let mut other = narsor::tui::input_form::InputPanel::new(&pending, &|_| None);
        let mut stranger = annotation.clone();
        stranger.key = "ep-2".to_string();
        other.annotate(&stranger);
        assert!(other.items().iter().all(|item| !item.recommended));
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
            labels[0].starts_with("Potwierdź: Uruchom krok"),
            "{labels:?}"
        );
        assert!(labels[1].starts_with("Odrzuć: Uruchom krok"));
        assert_eq!(labels[2], "Restart kroku z notatką");
        assert!(chooser_of(&app).items()[0].recommended, "◆ the conductor's");
        assert_eq!(
            chooser_of(&app).selected(),
            0,
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
        ch(&mut app, '3');

        assert_eq!(chooser_of(&app).selected(), 2);
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
        assert_eq!(chooser_of(&app).focus(), ChooserFocus::Fields);
        typed(&mut app, "powód restartu");
        assert_eq!(chooser_of(&app).fields_of(2)[0].value(), "powód restartu");
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
        ch(&mut app, '2');
        assert_eq!(
            sent(press(&mut app, KeyCode::Enter)).submit,
            Submit::Operator(OperatorCommand::RejectProposal {
                proposal_id: "p1".to_string()
            })
        );

        ch(&mut app, '3');
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
            Ok(ConductorReadResponse::Unavailable {
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
        assert_eq!(chooser_of(&app).items().len(), 1, "only the restart");
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
        ch(&mut app, '2');
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

        ch(&mut app, '2');
        assert!(chooser_of(&app).items()[1].irreversible);
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
    fn two_waitings_are_counted_and_the_bracket_keys_move_between_them() {
        let both = with_pending(with_pending(running("a", 1), gate("ep-1")), question(None));
        let mut app = opened(both);
        assert_eq!(app.detail().view("a").unwrap().waiting().len(), 2);
        assert_eq!(app.detail().view("a").unwrap().waiting().current_index(), 0);
        let text = screen(&mut app, 120, 40);
        assert!(text.contains("1/2"), "{text}");
        ch(&mut app, ']');
        assert_eq!(app.detail().view("a").unwrap().waiting().current_index(), 1);
        assert!(screen(&mut app, 120, 40).contains("2/2"));
        ch(&mut app, '[');
        assert_eq!(app.detail().view("a").unwrap().waiting().current_index(), 0);
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
            TaskLifecycle::Running,
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
    fn esc_folds_the_panel_and_opening_it_again_brings_the_draft_back() {
        let mut app = gate_app();
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "szkic");
        press(&mut app, KeyCode::Esc);
        assert!(app.panel_focused(), "the first Esc returns to the actions");
        press(&mut app, KeyCode::Esc);
        assert!(!app.panel_focused());
        assert!(app.detail().view("a").unwrap().waiting().is_collapsed());
        assert!(screen(&mut app, 120, 40).contains("Tab rozwiń"));
        assert!(app.detail().has_drafts(), "the draft stays");

        press(&mut app, KeyCode::Tab);
        assert!(app.panel_focused() && !app.detail().view("a").unwrap().waiting().is_collapsed());
        assert_eq!(chooser_of(&app).fields_of(1)[0].value(), "szkic");
        // From the list, Enter does the same.
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.focus(), Focus::List);
        press(&mut app, KeyCode::Enter);
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
    /// own entry, in the panel's order, and the entry shown stays shown.
    #[test]
    fn a_returning_waiting_takes_its_place_and_leaves_the_shown_one_alone() {
        let mut app = gate_app();
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "pierwsza");
        app.load(listing(vec![titled(running("a", 1), "NAR-12")], "d"));
        let other = waiting_on(running("a", 1), AttentionKind::Problem, "other", None);
        app.load(listing(vec![titled(other, "NAR-12")], "e"));
        assert_eq!(
            app.detail()
                .view("a")
                .unwrap()
                .waiting()
                .current_waiting_id(),
            Some("other")
        );

        let both = waiting_on(
            with_pending(running("a", 1), gate("ep-1")),
            AttentionKind::Problem,
            "other",
            None,
        );
        app.load(listing(vec![titled(both, "NAR-12")], "f"));
        let panel = app.detail().view("a").unwrap().waiting();
        assert!(panel.released().is_empty());
        assert_eq!(panel.len(), 2);
        assert_eq!(panel.current_index(), 1, "the shown entry stays shown");
        assert_eq!(panel.current_waiting_id(), Some("other"));
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
        press(&mut app, KeyCode::Esc);
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
        press(&mut app, KeyCode::Esc);
        assert!(!app.panel_focused() && app.focus() == Focus::Details);
        app.detail_mut()
            .set_tab(narsor::tui::task_app::DetailTab::Progress);
        read_step_conversation(&mut app, "a", 1);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.step_focus(), StepFocus::Conversation);

        press(&mut app, KeyCode::Tab);
        assert!(app.panel_focused());
        assert_eq!(app.step_focus(), StepFocus::Conversation);
        press(&mut app, KeyCode::Esc);
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

    fn approval_snapshot(sequence: u64) -> narsor_engine::orchestration::RunnerSnapshot {
        approval_snapshot_in(sequence, "ep-a")
    }

    /// An approval of the waiting `episode`.
    fn approval_snapshot_in(
        sequence: u64,
        episode: &str,
    ) -> narsor_engine::orchestration::RunnerSnapshot {
        let mut snapshot = snapshot(sequence, None);
        let form: narsor_engine::engine::PendingHumanInput =
            serde_json::from_value(approval(true)).unwrap();
        snapshot.pending_input = Some(narsor_engine::orchestration::PendingInputRef {
            id: episode.to_string(),
            attempt_id: None,
            form,
        });
        snapshot.attention = vec![narsor_engine::orchestration::AttentionItem {
            kind: AttentionKind::Approval,
            state_id: Some("review_scope".to_string()),
            since: chrono::Utc::now(),
            waiting_id: episode.to_string(),
        }];
        snapshot
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn space_enter_accept_enter_sends_returns_to_the_list_and_says_so() {
        let harness = RemoteHarness::start().await;
        let _running = harness
            .running("NAR-12", "local", approval_snapshot(1))
            .await;
        let mut app = TaskApp::new(harness.owner.clone(), "local", None);
        app.refresh().await;

        ch(&mut app, ' ');
        let request = press(&mut app, KeyCode::Enter);
        assert_eq!(request, Request::None);
        assert!(app.panel_focused());
        ch(&mut app, '1');
        let chooser = app
            .detail()
            .view("NAR-12")
            .unwrap()
            .waiting()
            .chooser()
            .unwrap();
        assert_eq!(chooser.selected_item().unwrap().label, "Akceptuj");
        let request = press(&mut app, KeyCode::Enter);
        app.perform(request).await;

        assert_eq!(
            app.focus(),
            Focus::List,
            "after sending, the focus is on the list"
        );
        let note = app.notifier().latest().expect("a notification");
        assert_eq!(note.text, "NAR-12: zaakceptowano reviewed");
        assert_eq!(note.severity, Severity::Success);
        let summary = harness.owner.control_task("NAR-12").await.unwrap();
        assert_eq!(
            summary.inputs.len(),
            1,
            "the answer was queued for the runner"
        );
        assert_eq!(summary.inputs[0].pending_input_id, "ep-a");
        assert!(!app.detail().has_drafts());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_answer_sent_from_the_steps_conversation_opens_the_details_again_on_the_axis() {
        use narsor::tui::task_app::StepFocus;
        let harness = RemoteHarness::start().await;
        let _running = harness
            .running("NAR-12", "local", approval_snapshot(1))
            .await;
        let mut app = TaskApp::new(harness.owner.clone(), "local", None);
        app.refresh().await;

        ch(&mut app, ' ');
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Esc);
        app.detail_mut()
            .set_tab(narsor::tui::task_app::DetailTab::Progress);
        let revision = app.list().selected().unwrap().revision;
        read_step_conversation(&mut app, "NAR-12", revision);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.step_focus(), StepFocus::Conversation);

        press(&mut app, KeyCode::Tab);
        assert!(app.panel_focused());
        ch(&mut app, '1');
        let request = press(&mut app, KeyCode::Enter);
        app.perform(request).await;
        assert_eq!(app.focus(), Focus::List, "{:?}", app.notifier().latest());

        press(&mut app, KeyCode::Enter);
        assert_eq!(app.focus(), Focus::Details);
        assert_eq!(app.step_focus(), StepFocus::Axis);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refusal_keeps_the_panel_and_the_text_and_reads_the_task_again() {
        let harness = RemoteHarness::start().await;
        let running = harness
            .running("NAR-12", "local", approval_snapshot(1))
            .await;
        let mut app = TaskApp::new(harness.owner.clone(), "local", None);
        app.refresh().await;
        ch(&mut app, ' ');
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "popraw testy");

        // The runner moved on: the request the panel shows is gone.
        running
            .runner
            .snapshot(&snapshot(2, None))
            .await
            .expect("the runner moved on");
        let request = press(&mut app, KeyCode::Enter);
        app.perform(request).await;

        let note = app.notifier().latest().unwrap();
        assert_eq!(note.severity, Severity::Error);
        assert!(
            note.text.starts_with("NAR-12: nie wysłano"),
            "{}",
            note.text
        );
        // The task was read again: nothing waits any more, and the text is kept.
        let panel = app.detail().view("NAR-12").unwrap().waiting();
        assert!(panel.is_empty());
        assert!(
            panel.released()[0].text.contains("popraw testy"),
            "{:?}",
            panel.released()
        );
        assert!(app.detail().has_drafts());
    }

    /// An answer to the waiting from before the runner was lost, sent once
    /// the resumed task asks in a new episode, is refused like any answer to
    /// a waiting the runner moved on from (D55): the text stays to be copied
    /// and the bar says why.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_answer_to_a_waiting_from_before_the_loss_is_refused_and_kept() {
        use narsor_engine::controller_protocol::{
            OwnerCommandRequest, RunnerClient, RunnerCredentialRequest,
        };
        use narsor_engine::orchestration::Observation;

        use super::remote_support::report;

        let harness = RemoteHarness::start().await;
        let running = harness
            .running("NAR-12", "local", approval_snapshot(1))
            .await;
        let mut app = TaskApp::new(harness.owner.clone(), "local", None);
        app.refresh().await;
        ch(&mut app, ' ');
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "popraw testy");

        // The runner is lost, the task resumed, and the new runner's action
        // asks again in a new episode — while the screen still shows the old.
        let (controller, epoch) = (&running.controller, running.epoch);
        controller
            .report(&report(
                "NAR-12",
                epoch,
                1,
                Observation::WorkloadLost { state_known: true },
                Some(1),
            ))
            .await
            .expect("lost");
        harness
            .owner
            .control_command(
                "NAR-12",
                &OwnerCommandRequest {
                    command: OwnerCommand::Resume,
                    idempotency_key: "resume-1".to_string(),
                    expected_generation: None,
                },
            )
            .await
            .expect("resumed");
        controller
            .report(&report(
                "NAR-12",
                epoch,
                2,
                Observation::StartAccepted,
                None,
            ))
            .await
            .expect("start accepted");
        let credential = controller
            .issue_runner_credential(
                "NAR-12",
                &RunnerCredentialRequest {
                    lease_epoch: epoch,
                    generation: 2,
                    incarnation: "pod-2".to_string(),
                },
            )
            .await
            .expect("runner credential");
        let runner = RunnerClient::new(harness.base_url(), credential.credential);
        runner
            .snapshot(&approval_snapshot_in(1, "ep-b"))
            .await
            .expect("the new episode");
        controller
            .report(&report(
                "NAR-12",
                epoch,
                2,
                Observation::RunnerReady,
                Some(2),
            ))
            .await
            .expect("runner ready");

        let request = press(&mut app, KeyCode::Enter);
        app.perform(request).await;
        let note = app.notifier().latest().unwrap();
        assert_eq!(note.severity, Severity::Error);
        assert!(
            note.text.starts_with("NAR-12: nie wysłano"),
            "{}",
            note.text
        );
        let summary = harness.owner.control_task("NAR-12").await.unwrap();
        assert!(summary.inputs.is_empty(), "nothing was queued");
        let panel = app.detail().view("NAR-12").unwrap().waiting();
        assert_eq!(panel.current_waiting_id(), Some("ep-b"));
        assert!(
            panel.released()[0].text.contains("popraw testy"),
            "{:?}",
            panel.released()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_label_from_the_workflow_cannot_put_control_characters_on_the_bar() {
        let harness = RemoteHarness::start().await;
        let mut form = gate("ep-1");
        form["outputs"][0]["label"] = json!("ap\u{1b}[31mpr\u{202e}oved\u{7}");
        let mut snapshot = snapshot(1, None);
        let pending: narsor_engine::engine::PendingHumanInput =
            serde_json::from_value(form).unwrap();
        snapshot.pending_input = Some(narsor_engine::orchestration::PendingInputRef {
            id: "ep-1".to_string(),
            attempt_id: None,
            form: pending,
        });
        snapshot.attention = vec![narsor_engine::orchestration::AttentionItem {
            kind: AttentionKind::Question,
            state_id: Some("review_scope".to_string()),
            since: chrono::Utc::now(),
            waiting_id: "ep-1".to_string(),
        }];
        let _running = harness.running("NAR-12", "local", snapshot).await;
        let mut app = TaskApp::new(harness.owner.clone(), "local", None);
        app.refresh().await;
        ch(&mut app, ' ');
        press(&mut app, KeyCode::Enter);
        ch(&mut app, '1');
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "safe");
        let request = enter_until_answer(&mut app);
        app.perform(request).await;
        let text = app.notifier().latest().expect("sent").text.clone();
        assert!(text.contains("approved") || text.contains("oved"), "{text}");
        assert!(
            !text.chars().any(|c| c.is_control() || c == '\u{202e}'),
            "no control or bidi characters reach the bar: {text:?}"
        );
    }
}

/// U11: the action menu, the confirmations and the commands of the operator.
mod task_app_actions {
    use std::time::Duration;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use narsor::api::{ApiClient, ClientError};
    use narsor::tui::keymap::{self, Action, Context};
    use narsor::tui::task_app::actions::{
        availability, refusal_reason, verb, ActionRequest, ActionSend, Availability, Confirmed,
        Overlay, TaskAction,
    };
    use narsor::tui::task_app::notify::Severity;
    use narsor::tui::task_app::{EventFeed, Request, TaskApp};
    use narsor_engine::controller_protocol::{OperatorCommandView, RunnerClient, TaskSummary};
    use narsor_engine::domain::ExecutionStatus;
    use narsor_engine::orchestration::{
        AttentionKind, OperatorCommand, OperatorCommandStatus, OwnerCommand, OwnerRef,
        TaskEventKind, TaskLifecycle,
    };

    use super::remote_support::{snapshot, RemoteHarness};
    use super::task_app_support::{
        displayed, event, listing, queued, task, titled, waiting_on, with_controller, with_runner,
        with_transitions,
    };

    fn offline_client() -> ApiClient {
        ApiClient::new("127.0.0.1:9".parse().unwrap(), "token")
    }

    fn app() -> TaskApp {
        TaskApp::new(offline_client(), "local", None)
    }

    fn press(app: &mut TaskApp, code: KeyCode) -> Request {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
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

    fn lifecycle(word: &str) -> TaskSummary {
        with_controller(task("a", "local", word, 1))
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

    mod availability_table {
        use super::*;

        #[test]
        fn move_is_for_queued_tasks_only() {
            assert!(available(&queued("a", 1), TaskAction::Move));
            for word in ["running", "paused", "interrupted", "provisioning"] {
                assert_eq!(
                    unavailable(&lifecycle(word), TaskAction::Move),
                    "zadanie nie jest w kolejce",
                    "{word}"
                );
            }
        }

        #[test]
        fn pause_for_running_resume_for_paused_and_nothing_else() {
            let running = running("a", 1);
            assert!(available(&running, TaskAction::PauseResume));
            assert_eq!(verb(&running, TaskAction::PauseResume), "pauza");
            let paused = lifecycle("paused");
            assert!(available(&paused, TaskAction::PauseResume));
            assert_eq!(verb(&paused, TaskAction::PauseResume), "wznowienie");
            assert_eq!(
                unavailable(&queued("a", 1), TaskAction::PauseResume),
                "zadanie jest w kolejce"
            );
            assert_eq!(
                unavailable(&lifecycle("provisioning"), TaskAction::PauseResume),
                "zadanie jest przygotowywane"
            );
            assert!(
                unavailable(&lifecycle("interrupted"), TaskAction::PauseResume)
                    .contains("uruchom je od nowa")
            );
            assert!(unavailable(&finished("a", 1), TaskAction::PauseResume).contains("zakończone"));
            assert!(
                unavailable(&lifecycle("pause_requested"), TaskAction::PauseResume)
                    .contains("wstrzymywane")
            );
        }

        #[test]
        fn interrupt_needs_a_running_step_and_a_runner_that_answers() {
            assert!(available(&running("a", 1), TaskAction::Interrupt));
            assert_eq!(
                unavailable(&queued("a", 1), TaskAction::Interrupt),
                "zadanie jest w kolejce"
            );
            assert!(unavailable(&lifecycle("paused"), TaskAction::Interrupt).contains("wznów"));
            assert!(unavailable(&finished("a", 1), TaskAction::Interrupt).contains("zakończone"));
            assert_eq!(
                unavailable(&lifecycle("running"), TaskAction::Interrupt),
                "runner jeszcze się nie zgłosił"
            );
            let mut silent = running("a", 1);
            silent.runner.as_mut().unwrap().freshness =
                narsor_engine::controller_protocol::Freshness::Stale;
            assert!(unavailable(&silent, TaskAction::Interrupt).contains("nie odpowiada"));
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
            assert!(available(&lifecycle("interrupted"), TaskAction::Restart));
            assert!(available(&lifecycle("unknown"), TaskAction::Restart));
            let failed = waiting_on(
                lifecycle("provisioning"),
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
            assert!(unavailable(&lifecycle("paused"), TaskAction::Restart).contains("wznów"));
        }

        #[test]
        fn archive_is_for_finished_and_interrupted_tasks() {
            assert!(available(&finished("a", 1), TaskAction::Archive));
            assert!(available(&lifecycle("interrupted"), TaskAction::Archive));
            let failed = waiting_on(
                lifecycle("provisioning"),
                AttentionKind::ProvisioningFailed,
                "p-1",
                None,
            );
            assert!(available(&failed, TaskAction::Archive));
            for blocked in [running("a", 1), queued("a", 1), lifecycle("paused")] {
                assert!(
                    unavailable(&blocked, TaskAction::Archive).contains("zakończone lub przerwane"),
                    "{:?}",
                    blocked.lifecycle
                );
            }
        }

        #[test]
        fn a_link_needs_a_value() {
            assert_eq!(
                unavailable(&running("a", 1), TaskAction::OpenLink),
                "workflow nie deklaruje linków"
            );
            let unresolved = displayed(running("a", 1), "NAR-12", &[("Jira", "—", None)]);
            assert_eq!(
                unavailable(&unresolved, TaskAction::OpenLink),
                "linki zadania nie mają jeszcze wartości"
            );
            let unsafe_scheme = displayed(
                running("a", 1),
                "NAR-12",
                &[("Ewil", "x", Some("javascript:alert(1)"))],
            );
            assert!(!available(&unsafe_scheme, TaskAction::OpenLink));
            let ok = displayed(
                running("a", 1),
                "NAR-12",
                &[("Jira", "NAR-12", Some("https://jira.example/browse/NAR-12"))],
            );
            assert!(available(&ok, TaskAction::OpenLink));
        }

        #[test]
        fn every_row_of_the_origin_table_has_a_key_an_action_and_a_rule() {
            for (key, action) in [
                ('m', TaskAction::Move),
                ('p', TaskAction::PauseResume),
                ('x', TaskAction::Interrupt),
                ('t', TaskAction::Transition),
                ('s', TaskAction::Restart),
                ('a', TaskAction::Archive),
                ('o', TaskAction::OpenLink),
            ] {
                assert_eq!(action.key(), key);
                assert!(TaskAction::ALL.contains(&action));
            }
            assert!(TaskAction::Interrupt.needs_confirmation());
            assert!(TaskAction::Archive.needs_confirmation());
            assert!(!TaskAction::PauseResume.needs_confirmation());
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
            assert_eq!(by_label("wstrzymaj").unavailable, None);
            assert_eq!(by_label("przerwij krok").unavailable, None);
            assert!(by_label("przerwij krok").label.contains('!'));
            assert_eq!(
                by_label("przejście ręczne").unavailable.as_deref(),
                Some("serwer nie podał wyjść bieżącego stanu"),
                "no exits known, so the entry is greyed with the reason"
            );
            assert_eq!(
                by_label("przesuń").unavailable.as_deref(),
                Some("zadanie nie jest w kolejce")
            );
            assert!(by_label("archiwizuj").unavailable.is_some());
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
            // The first entry is the move, which a running task cannot do.
            assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
            assert_eq!(note(&app), "przesuń: zadanie nie jest w kolejce");
            assert!(app.overlay().is_some(), "the menu stays");
        }

        #[test]
        fn a_shortcut_in_the_menu_runs_the_action_and_closes_it() {
            let mut app = loaded(running("a", 1));
            ch(&mut app, '.');
            let request = act(ch(&mut app, 'p'));
            assert!(app.overlay().is_none());
            assert_eq!(request.send, ActionSend::Owner(OwnerCommand::Pause));
        }

        #[test]
        fn enter_on_an_available_entry_runs_it() {
            let mut app = loaded(running("a", 1));
            ch(&mut app, '.');
            ch(&mut app, '2');
            let request = act(press(&mut app, KeyCode::Enter));
            assert_eq!(request.send, ActionSend::Owner(OwnerCommand::Pause));
        }

        #[test]
        fn a_shortcut_to_an_unavailable_action_in_the_menu_gives_the_same_reason() {
            let mut app = loaded(running("a", 1));
            ch(&mut app, '.');
            assert_eq!(ch(&mut app, 'a'), Request::None);
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
            let request = ch(&mut app, 'J');
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
            app.on_stream(event(
                "d",
                "a",
                2,
                TaskLifecycle::Paused,
                TaskEventKind::Created,
            ));
            let chooser = app.overlay().unwrap().chooser().unwrap();
            assert!(
                chooser
                    .items()
                    .iter()
                    .any(|item| item.label.contains("wznów")),
                "the pause became a resume"
            );
            app.on_stream(event(
                "e",
                "a",
                3,
                TaskLifecycle::Archived,
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
            assert_eq!(ch(&mut app, 'p'), Request::None);
        }
    }

    // -- shortcuts ---------------------------------------------------------

    mod shortcuts {
        use super::*;

        #[test]
        fn p_on_a_queued_task_says_why_it_cannot() {
            let mut app = app();
            app.load(listing(vec![queued("a", 1)], "c"));
            assert_eq!(ch(&mut app, 'p'), Request::None);
            assert_eq!(note(&app), "pauza: zadanie jest w kolejce");
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Warning);
        }

        #[test]
        fn p_pauses_a_running_task_and_resumes_a_paused_one_without_asking() {
            let mut app = loaded(running("a", 1));
            let pause = act(ch(&mut app, 'p'));
            assert_eq!(pause.send, ActionSend::Owner(OwnerCommand::Pause));
            assert_eq!(pause.generation, 1);
            assert!(!pause.key.is_empty());
            assert!(app.overlay().is_none(), "no window for a plain action");

            let mut app = loaded(lifecycle("paused"));
            let resume = act(ch(&mut app, 'p'));
            assert_eq!(resume.send, ActionSend::Owner(OwnerCommand::Resume));
        }

        #[test]
        fn s_starts_an_interrupted_task_again_and_says_why_for_a_running_one() {
            let mut app = loaded(lifecycle("interrupted"));
            let start = act(ch(&mut app, 's'));
            assert_eq!(start.send, ActionSend::Owner(OwnerCommand::Start));

            let mut app = loaded(running("a", 1));
            assert_eq!(ch(&mut app, 's'), Request::None);
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
            let pause = act(ch(&mut app, 'p'));
            assert_eq!(pause.send, ActionSend::Owner(OwnerCommand::Pause));
        }

        #[test]
        fn a_retry_of_a_decision_that_got_no_answer_reuses_its_key() {
            let mut app = loaded(running("a", 1));
            let first = act(ch(&mut app, 'p'));
            let again = act(ch(&mut app, 'p'));
            assert_eq!(first.key, again.key, "the same decision, the same key");
        }

        #[tokio::test]
        async fn a_lost_connection_keeps_the_key_for_the_retry() {
            let mut app = loaded(running("a", 1));
            let first = act(ch(&mut app, 'p'));
            app.perform(Request::Act(Box::new(first.clone()))).await;
            assert!(
                app.notifier().latest().unwrap().severity == Severity::Error,
                "{}",
                note(&app)
            );
            // The connection came back.
            app.load(listing(vec![titled(running("a", 1), "NAR-12")], "d"));
            let again = act(ch(&mut app, 'p'));
            assert_eq!(first.key, again.key);
        }
    }

    // -- the confirmation window -------------------------------------------

    mod confirmation {
        use super::*;

        #[test]
        fn x_asks_with_the_effect_and_y_sends_the_interrupt() {
            let mut app = loaded(running("a", 1));
            assert_eq!(ch(&mut app, 'x'), Request::None);
            let window = confirmation(&app);
            assert!(
                window.heading().contains("Przerwij"),
                "{}",
                window.heading()
            );
            let text = window.lines().join(" ");
            assert!(text.contains("„review”"), "the step it cuts short: {text}");
            assert!(text.contains("NAR-12"), "{text}");
            assert_eq!(app.key_context().base, Context::Confirm);

            let request = act(ch(&mut app, 'y'));
            assert_eq!(
                request.send,
                ActionSend::Operator(OperatorCommand::Interrupt)
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
            ch(&mut app, 'x');
            assert_eq!(ch(&mut app, 'j'), Request::None);
            assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
            assert!(app.overlay().is_some(), "only y and n answer it");
            assert_eq!(ch(&mut app, 'n'), Request::None);
            assert!(app.overlay().is_none());

            ch(&mut app, 'x');
            assert_eq!(press(&mut app, KeyCode::Esc), Request::None);
            assert!(app.overlay().is_none());
        }

        #[test]
        fn a_on_a_finished_task_asks_and_y_archives() {
            let mut app = loaded(finished("a", 1));
            assert_eq!(ch(&mut app, 'a'), Request::None);
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
            for (key, task) in [('x', running("a", 1)), ('a', finished("a", 1))] {
                let mut app = loaded(task);
                ch(&mut app, key);
                assert_eq!(app.key_context().base, Context::Confirm, "{key}");
                assert!(matches!(
                    app.overlay().unwrap().confirmation().unwrap().confirmed(),
                    Confirmed::Act(_)
                ));
            }
        }

        #[test]
        fn y_does_not_send_without_a_connection() {
            let mut app = loaded(running("a", 1));
            ch(&mut app, 'x');
            app.on_stream(Some(Err(ClientError::Unavailable("down".into()))));
            assert_eq!(ch(&mut app, 'y'), Request::None);
            assert!(note(&app).contains("zablokowana"), "{}", note(&app));
            assert!(app.overlay().is_none());
        }

        #[test]
        fn the_window_closes_when_its_task_leaves_the_list() {
            let mut app = loaded(finished("a", 1));
            ch(&mut app, 'a');
            app.on_stream(event(
                "d",
                "a",
                2,
                TaskLifecycle::Archived,
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
            assert_eq!(ch(&mut app, 't'), Request::None);
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
            ch(&mut app, 't');
            press(&mut app, KeyCode::Esc);
            assert!(app.overlay().is_none());
        }

        #[test]
        fn the_override_is_only_there_when_the_state_allows_it() {
            let mut app = loaded(offered(None));
            ch(&mut app, 't');
            assert!(app
                .overlay()
                .unwrap()
                .chooser()
                .unwrap()
                .items()
                .iter()
                .all(|item| !item.label.contains("override")));

            let mut app = loaded(offered(Some(&["plan", "ship"])));
            ch(&mut app, 't');
            let chooser = app.overlay().unwrap().chooser().unwrap();
            assert_eq!(chooser.items().len(), 3);
            assert!(chooser.items()[2].label.contains("override"));
            assert!(chooser.items()[2].label.starts_with('!'));
        }

        #[test]
        fn the_override_needs_a_reason_and_asks_before_it_sends() {
            let mut app = loaded(offered(Some(&["plan", "ship"])));
            ch(&mut app, 't');
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
        fn n_in_the_override_window_sends_nothing() {
            let mut app = loaded(offered(Some(&["plan"])));
            ch(&mut app, 't');
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
            assert_eq!(ch(&mut app, 't'), Request::None);
            assert_eq!(
                note(&app),
                "przejście: serwer nie podał wyjść bieżącego stanu"
            );
            assert!(app.overlay().is_none());
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
            ch(&mut app, 't');
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
            assert_eq!(note(&app), "link: workflow nie deklaruje linków");
            let unresolved = displayed(running("a", 1), "NAR-12", &[("Jira", "—", None)]);
            let mut app = loaded(unresolved);
            assert_eq!(ch(&mut app, 'o'), Request::None);
            assert_eq!(note(&app), "link: linki zadania nie mają jeszcze wartości");
            assert!(app.overlay().is_none());
        }

        #[test]
        fn o_lists_the_links_with_a_value_and_enter_opens_one() {
            let task = displayed(
                running("a", 1),
                "NAR-12",
                &[
                    ("Jira", "NAR-12", Some("https://jira.example/browse/NAR-12")),
                    ("PR", "—", None),
                    ("Docs", "doc", Some("https://docs.example/x")),
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
            assert!(chooser.items()[0].label.contains("Jira"));
            ch(&mut app, '2');
            let request = press(&mut app, KeyCode::Enter);
            let Request::OpenLink(link) = request else {
                panic!("a link was expected");
            };
            assert_eq!(link.url, "https://docs.example/x");
            assert_eq!(link.label, "Docs");
            assert!(app.overlay().is_none());
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
            for (key, task) in [
                ('p', running("a", 1)),
                ('x', running("a", 1)),
                ('a', finished("a", 1)),
                ('s', lifecycle("interrupted")),
                (
                    't',
                    with_transitions(running("a", 1), "review", &[("ok", "ship", None)], None),
                ),
            ] {
                let mut app = disconnected(task);
                assert_eq!(ch(&mut app, key), Request::None, "{key}");
                assert!(note(&app).contains("zablokowana"), "{key}: {}", note(&app));
                assert!(app.overlay().is_none(), "{key}: nothing opens");
            }
        }

        #[test]
        fn the_menu_greys_out_what_changes_state_and_leaves_the_links() {
            let task = displayed(
                running("a", 1),
                "NAR-12",
                &[("Jira", "NAR-12", Some("https://jira.example/browse/NAR-12"))],
            );
            let mut app = disconnected(task);
            ch(&mut app, '.');
            let chooser = app.overlay().unwrap().chooser().unwrap();
            let pause = chooser
                .items()
                .iter()
                .find(|item| item.label.contains("wstrzymaj"))
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
                TaskLifecycle::Running,
                TaskEventKind::Created,
            ));
            let chooser = app.overlay().unwrap().chooser().unwrap();
            assert!(chooser
                .items()
                .iter()
                .find(|item| item.label.contains("wstrzymaj"))
                .unwrap()
                .unavailable
                .is_none());
        }

        #[test]
        fn a_task_that_cannot_be_acted_on_says_so_before_the_connection_does() {
            let mut app = disconnected(queued("a", 1));
            assert_eq!(ch(&mut app, 'p'), Request::None);
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
                TaskLifecycle::Running,
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
                TaskLifecycle::Running,
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
                TaskLifecycle::Running,
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
                TaskLifecycle::Running,
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
                TaskLifecycle::Running,
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
                    TaskLifecycle::Running,
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
                TaskLifecycle::Running,
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
                TaskLifecycle::Running,
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
                TaskLifecycle::Running,
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

    mod served {
        use super::*;

        async fn next_event(app: &mut TaskApp, feed: &mut EventFeed) {
            let item = tokio::time::timeout(Duration::from_secs(5), feed.next())
                .await
                .expect("an event arrives");
            app.on_stream(item);
        }

        /// The runner of `task` takes the commands it has been sent and
        /// settles each as `applied` (with `detail` when it refuses).
        async fn settle_all(runner: &RunnerClient, applied: bool, detail: Option<&str>) -> usize {
            let polled = runner.poll(None, 0).await.expect("poll");
            for command in &polled.commands {
                runner
                    .ack_command(&command.command_id, applied, detail.map(str::to_string))
                    .await
                    .expect("ack");
            }
            polled.commands.len()
        }

        #[tokio::test]
        async fn x_then_y_ends_with_the_runner_applying_it_and_the_row_updated() {
            let harness = RemoteHarness::start().await;
            let running = harness.running("NAR-12", "local", snapshot(1, None)).await;
            let mut app = TaskApp::new(harness.owner.clone(), "local", None);
            app.refresh().await;
            let mut feed = EventFeed::new(harness.owner.clone());
            feed.ensure_open(&app);

            assert_eq!(ch(&mut app, 'x'), Request::None);
            let request = ch(&mut app, 'y');
            assert!(matches!(request, Request::Act(_)), "{request:?}");
            app.perform(request).await;
            assert_eq!(
                note(&app),
                "NAR-12: przerwanie kroku wysłane do runnera",
                "the answer says: queued"
            );
            assert_eq!(
                app.list().tasks()[0].operator_commands[0].status,
                OperatorCommandStatus::Queued
            );

            assert_eq!(settle_all(&running.runner, true, None).await, 1);
            for _ in 0..8 {
                if app.list().tasks()[0].operator_commands[0].status
                    == OperatorCommandStatus::Applied
                {
                    break;
                }
                next_event(&mut app, &mut feed).await;
            }
            let command = &app.list().tasks()[0].operator_commands[0];
            assert_eq!(command.status, OperatorCommandStatus::Applied);
            assert_eq!(command.command, OperatorCommand::Interrupt);
            assert_eq!(note(&app), "NAR-12: przerwanie kroku wykonane");
            assert_eq!(
                app.list().tasks()[0].operator_commands.len(),
                1,
                "no duplicate"
            );
        }

        #[tokio::test]
        async fn a_command_the_runner_rejects_ends_on_the_bar_with_its_reason() {
            let harness = RemoteHarness::start().await;
            let running = harness.running("NAR-12", "local", snapshot(1, None)).await;
            let mut app = TaskApp::new(harness.owner.clone(), "local", None);
            app.refresh().await;
            let mut feed = EventFeed::new(harness.owner.clone());
            feed.ensure_open(&app);

            ch(&mut app, 'x');
            let request = ch(&mut app, 'y');
            app.perform(request).await;
            assert_eq!(
                settle_all(&running.runner, false, Some("no step is in flight")).await,
                1
            );
            for _ in 0..8 {
                if app.list().tasks()[0].operator_commands[0].status
                    == OperatorCommandStatus::Rejected
                {
                    break;
                }
                next_event(&mut app, &mut feed).await;
            }
            let text = note(&app);
            assert!(
                text.contains("przerwanie kroku odrzucone przez runnera"),
                "{text}"
            );
            assert!(text.contains("no step is in flight"), "{text}");
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Error);
        }

        #[tokio::test]
        async fn an_override_without_a_reason_is_refused_early_and_the_bar_says_why() {
            let harness = RemoteHarness::start().await;
            let _running = harness.running("NAR-12", "local", snapshot(1, None)).await;
            let mut app = TaskApp::new(harness.owner.clone(), "local", None);
            app.refresh().await;
            let task = app.list().tasks()[0].clone();
            let request = ActionRequest {
                task_id: "NAR-12".to_string(),
                title: "NAR-12".to_string(),
                generation: task.generation,
                key: "override-1".to_string(),
                verb: "override".to_string(),
                summary: "override wysłany".to_string(),
                send: ActionSend::Operator(OperatorCommand::Transition {
                    target: "plan".to_string(),
                    forced: true,
                    reason: None,
                }),
            };
            app.perform(Request::Act(Box::new(request))).await;
            let text = note(&app);
            assert!(text.contains("override odrzucone"), "{text}");
            assert!(text.contains("override wymaga powodu"), "{text}");
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Error);
        }

        #[tokio::test]
        async fn a_command_to_a_task_that_moved_on_is_refused_with_the_generation() {
            let harness = RemoteHarness::start().await;
            let _running = harness.running("NAR-12", "local", snapshot(1, None)).await;
            let mut app = TaskApp::new(harness.owner.clone(), "local", None);
            app.refresh().await;
            ch(&mut app, 'x');
            let mut request = act(ch(&mut app, 'y'));
            request.generation += 7;
            app.perform(Request::Act(Box::new(request))).await;
            let text = note(&app);
            assert!(text.contains("przerwanie odrzucone"), "{text}");
            assert!(text.contains("zmieniło się w międzyczasie"), "{text}");
        }

        #[tokio::test]
        async fn archiving_a_finished_task_removes_its_row_and_selects_the_one_below() {
            let harness = RemoteHarness::start().await;
            let mut done = snapshot(1, None);
            done.execution_status = ExecutionStatus::Completed;
            let _running = harness.running("t1", "k8s", done).await;
            harness.create("t2", Some("k8s")).await;
            harness.create("t3", Some("k8s")).await;
            let mut app = TaskApp::new(harness.owner.clone(), "local", None);
            app.refresh().await;
            let mut feed = EventFeed::new(harness.owner.clone());
            feed.ensure_open(&app);
            let ids = |app: &TaskApp| -> Vec<String> {
                app.list()
                    .tasks()
                    .iter()
                    .map(|t| t.task_id.to_string())
                    .collect()
            };
            assert_eq!(ids(&app), ["t1", "t2", "t3"]);
            assert_eq!(app.list().selected_id(), Some("t1"));

            assert_eq!(ch(&mut app, 'a'), Request::None);
            assert!(confirmation(&app).heading().contains("Archiwizuj"));
            assert_eq!(ids(&app).len(), 3, "nothing was sent yet");
            let request = ch(&mut app, 'y');
            app.perform(request).await;
            for _ in 0..8 {
                if ids(&app).len() == 2 {
                    break;
                }
                next_event(&mut app, &mut feed).await;
            }
            assert_eq!(ids(&app), ["t2", "t3"], "the row left the list");
            assert_eq!(
                app.list().selected_id(),
                Some("t2"),
                "the selection went below"
            );
            assert!(note(&app).contains("zniknęło z listy"), "{}", note(&app));
        }

        #[tokio::test]
        async fn pause_goes_through_the_owner_command_and_the_stream_shows_it() {
            let harness = RemoteHarness::start().await;
            let _running = harness.running("NAR-12", "local", snapshot(1, None)).await;
            let mut app = TaskApp::new(harness.owner.clone(), "local", None);
            app.refresh().await;
            let mut feed = EventFeed::new(harness.owner.clone());
            feed.ensure_open(&app);
            let request = ch(&mut app, 'p');
            app.perform(request).await;
            assert_eq!(note(&app), "NAR-12: wstrzymanie zlecone");
            for _ in 0..8 {
                if app.list().tasks()[0].lifecycle != TaskLifecycle::Running {
                    break;
                }
                next_event(&mut app, &mut feed).await;
            }
            assert_eq!(
                app.list().tasks()[0].lifecycle,
                TaskLifecycle::PauseRequested
            );
        }

        #[tokio::test]
        async fn the_exits_of_a_running_task_come_from_its_workflow() {
            let harness = RemoteHarness::start().await;
            let mut first = snapshot(1, None);
            first.execution_cursor.current_state = Some("work".to_string());
            let _running = harness.running("NAR-12", "local", first).await;
            let mut app = TaskApp::new(harness.owner.clone(), "local", None);
            app.refresh().await;
            let offer = app.list().tasks()[0]
                .manual_transitions
                .clone()
                .expect("the sealed workflow names the exits of `work`");
            assert_eq!(offer.from_state, "work");
            assert_eq!(offer.exits.len(), 1);
            assert_eq!(offer.exits[0].label, "success");
            assert_eq!(offer.exits[0].target, "done");
            assert!(!offer.override_allowed);
            assert!(offer.override_targets.is_empty());

            // `t` offers it, and the choice goes through the control plane
            // that checks it against the same workflow.
            ch(&mut app, 't');
            let request = press(&mut app, KeyCode::Enter);
            let request = act(request);
            assert_eq!(
                request.send,
                ActionSend::Operator(OperatorCommand::Transition {
                    target: "done".to_string(),
                    forced: false,
                    reason: None,
                })
            );
            app.perform(Request::Act(Box::new(request))).await;
            assert_eq!(note(&app), "NAR-12: przejście do „done” wysłane do runnera");

            // The state the runner reported earlier was not one of the
            // workflow's: the server offers nothing it cannot back.
            let mut other = TaskApp::new(harness.owner.clone(), "local", None);
            harness
                .running("NAR-13", "backend-a", snapshot(1, None))
                .await;
            other.refresh().await;
            let task = other
                .list()
                .tasks()
                .iter()
                .find(|task| task.task_id.as_str() == "NAR-13")
                .unwrap();
            assert!(task.manual_transitions.is_none());
            assert_eq!(
                unavailable(task, TaskAction::Transition),
                "serwer nie podał wyjść bieżącego stanu"
            );
        }

        #[tokio::test]
        async fn an_illegal_transition_is_refused_early_with_the_reason() {
            let harness = RemoteHarness::start().await;
            let mut first = snapshot(1, None);
            first.execution_cursor.current_state = Some("work".to_string());
            let _running = harness.running("NAR-12", "local", first).await;
            let mut app = TaskApp::new(harness.owner.clone(), "local", None);
            app.refresh().await;
            let request = ActionRequest {
                task_id: "NAR-12".to_string(),
                title: "NAR-12".to_string(),
                generation: app.list().tasks()[0].generation,
                key: "t-1".to_string(),
                verb: "przejście".to_string(),
                summary: "x".to_string(),
                send: ActionSend::Operator(OperatorCommand::Transition {
                    target: "work".to_string(),
                    forced: false,
                    reason: None,
                }),
            };
            app.perform(Request::Act(Box::new(request))).await;
            let text = note(&app);
            assert!(text.contains("przejście odrzucone"), "{text}");
            assert!(text.contains("nie jest dozwolone"), "{text}");
        }
    }
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
    use narsor::tui::task_app::waiting::Submit;
    use narsor::tui::task_app::{DetailTab, Focus, Request, TaskApp};
    use narsor::tui::transcript::TranscriptPresentation;
    use narsor_engine::controller_protocol::{
        ConversationUnavailableReason, ExecutionEventsSource, RunnerClient, TaskSummary,
    };
    use narsor_engine::domain::ExecutionStatus;
    use narsor_engine::orchestration::{
        AttentionKind, OperatorCommand, OwnerRef, TaskEventKind, TaskLifecycle,
    };
    use narsor_protocol::controller::ConductorReadResponse;
    use narsor_protocol::controller::ConductorView;

    use super::remote_support::{snapshot, RemoteHarness};
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

    fn press(app: &mut TaskApp, code: KeyCode) -> Request {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
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
                app.on_stream(event(cursor, "a", sequence, TaskLifecycle::Running, kind));
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
            ch(&mut app, '2');
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
                Ok(ConductorReadResponse::Unavailable {
                    reason: ConversationUnavailableReason::NotStarted,
                    message: "zadanie nie ma żywego runnera".to_string(),
                }),
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
                app.on_stream(event(cursor, "a", sequence, TaskLifecycle::Running, kind));
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

    mod served {
        use super::*;

        fn conducting() -> narsor_engine::orchestration::RunnerSnapshot {
            let mut first = snapshot(1, None);
            first.conductor_available = true;
            first
        }

        /// The tab of task `NAR-12`, nothing read yet.
        async fn open_unread(harness: &RemoteHarness) -> TaskApp {
            let mut app = TaskApp::new(harness.owner.clone(), "local", None);
            app.refresh().await;
            press(&mut app, KeyCode::Enter);
            app.detail_mut().set_tab(DetailTab::Conductor);
            app
        }

        /// The tab of task `NAR-12`, with the conductor read already.
        async fn open(harness: &RemoteHarness) -> TaskApp {
            let mut app = open_unread(harness).await;
            let revision = app.list().selected().expect("a selection").revision;
            app.on_detail_read(
                "NAR-12",
                revision,
                DetailRead::Conductor(Ok(ConductorReadResponse::Available {
                    conductor: Box::new(conductor_view(vec![], json!({}))),
                    source: ExecutionEventsSource::Runner,
                })),
            );
            app
        }

        /// The runner's side of one relayed conductor read: the `after` it
        /// was asked for, and `view` as the answer.
        async fn answer(runner: &RunnerClient, view: ConductorView) -> u64 {
            loop {
                let poll = runner.poll(None, 5).await.expect("poll");
                if let Some(request) = poll.conductor_reads.first() {
                    runner
                        .upload_conductor_read(
                            &request.request_id,
                            &serde_json::from_value(serde_json::to_value(view).unwrap()).unwrap(),
                        )
                        .await
                        .expect("upload");
                    return request.after;
                }
            }
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_message_is_taken_by_the_control_plane_and_leaves_the_box() {
            let harness = RemoteHarness::start().await;
            let _running = harness.running("NAR-12", "local", conducting()).await;
            let mut app = open(&harness).await;
            ch(&mut app, 'c');
            assert!(conductor_typing(&app));
            typed(&mut app, "Dlaczego revise?");
            let request = press(&mut app, KeyCode::Enter);
            app.perform(request).await;

            assert_eq!(note(&app), "NAR-12: wiadomość wysłana do dyrygenta");
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Success);
            assert_eq!(app.detail().view("NAR-12").unwrap().conductor().draft(), "");
            assert!(!app.detail().has_drafts(), "the text left with the message");
            let summary = harness.owner.control_task("NAR-12").await.unwrap();
            assert_eq!(
                summary.operator_commands[0].command,
                OperatorCommand::ConductorChat {
                    text: "Dlaczego revise?".to_string()
                }
            );
        }

        fn conductor_typing(app: &TaskApp) -> bool {
            app.detail().view("NAR-12").unwrap().conductor().is_typing()
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_message_that_is_too_long_is_refused_and_stays_in_the_box() {
            let harness = RemoteHarness::start().await;
            let _running = harness.running("NAR-12", "local", conducting()).await;
            let mut app = open(&harness).await;
            ch(&mut app, 'c');
            typed(&mut app, &"a".repeat(4001));
            let request = press(&mut app, KeyCode::Enter);
            app.perform(request).await;

            let text = note(&app);
            assert!(text.contains("wiadomość do dyrygenta odrzucone"), "{text}");
            assert!(text.contains("4000"), "{text}");
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Error);
            let draft = app
                .detail()
                .view("NAR-12")
                .unwrap()
                .conductor()
                .draft()
                .to_string();
            assert_eq!(
                draft.chars().count(),
                4001,
                "nothing the operator typed was lost"
            );
            assert!(app.detail().has_drafts());
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn the_chat_is_read_through_the_relay_a_page_at_a_time_after_the_last_entry() {
            let harness = RemoteHarness::start().await;
            let running = harness.running("NAR-12", "local", conducting()).await;
            let mut app = open_unread(&harness).await;

            let first = conductor_view(
                vec![
                    chat_entry("m1", 1, "operator", "message", "Hej", None),
                    chat_entry("m2", 2, "conductor", "message", "Cześć", None),
                ],
                json!({ "chat_has_more": true }),
            );
            let second = conductor_view(
                vec![chat_entry(
                    "m3",
                    3,
                    "conductor",
                    "message",
                    "Jeszcze coś",
                    None,
                )],
                json!({}),
            );
            let (_, afters) = tokio::join!(app.sync_detail(), async {
                let one = answer(&running.runner, first).await;
                let two = answer(&running.runner, second).await;
                (one, two)
            });
            assert_eq!(
                afters,
                (0, 2),
                "the second page continued after the last entry"
            );
            let tab = app.detail().view("NAR-12").unwrap().conductor();
            assert_eq!(tab.items().len(), 3);
            assert!(!tab.has_more());
            assert!(app.connection().is_live());
        }
    }
}

/// U13: the new-task form, the history and the repositories.
mod task_app_create {
    use std::net::SocketAddr;
    use std::time::Instant;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::json;

    use narsor::api::{ApiClient, ClientError};
    use narsor::tui::keymap::{self, Action, Context};
    use narsor::tui::task_app::history::HistoryEntry;
    use narsor::tui::task_app::list::PendingState;
    use narsor::tui::task_app::new_task::{
        absolute_path, CreateHow, PathVerdict, DRAFT_KEY, PATH_FIELD, PATH_LABEL, RETRY_AFTER,
    };
    use narsor::tui::task_app::notify::Severity;
    use narsor::tui::task_app::{DetailTab, Request, TaskApp, ViewKind};
    use narsor_engine::controller_protocol::{
        RepositoryListResponse, TaskSummary, ValidateWorkflowResponse, WorkflowListResponse,
    };
    use narsor_engine::domain::ParamType;
    use narsor_engine::orchestration::TaskLifecycle;
    use narsor_engine::persistence::ExecutionStore;
    use narsor_protocol::api::LegacyExecutionListResponse;

    use super::remote_support::RemoteHarness;
    use super::task_app_support::{
        archived, capabilities, catalogue, catalogue_workflow, legacy, listing, queued, registry,
        repository, titled, workflow_input,
    };

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

    fn press(app: &mut TaskApp, code: KeyCode) -> Request {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
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
                    workflow_input("target_repo", ParamType::Repository, false, None),
                ],
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

    fn fill_ticket(app: &mut TaskApp, ticket: &str) {
        press(app, KeyCode::Enter);
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

    /// A running task `id` whose details are open on *Przebieg* with the
    /// keys in its step's conversation.
    fn running_task(id: &str, rank: u32) -> TaskSummary {
        use narsor_engine::domain::ExecutionStatus;
        super::task_app_support::with_runner(
            super::task_app_support::with_controller(super::task_app_support::task(
                id, "local", "running", rank,
            )),
            ExecutionStatus::Running,
            "review",
        )
    }

    /// Selects `id` from the list and puts the keys in its step's
    /// conversation.
    fn into_step_conversation(app: &mut TaskApp, id: &str) {
        use narsor::tui::task_app::detail::DetailRead;
        use narsor::tui::task_app::tabs::conversation::AttemptRead;
        use narsor::tui::task_app::StepFocus;
        use narsor::tui::transcript::{TranscriptEntry, TranscriptEntryId};
        use narsor_engine::engine::{ConversationState, PositionStatus};

        app.list_mut().select(id);
        press(app, KeyCode::Enter);
        app.detail_mut()
            .set_tab(narsor::tui::task_app::DetailTab::Progress);
        let revision = app.list().selected().expect("a selected task").revision;
        let mut step = super::task_app_support::position("plan", PositionStatus::InProgress);
        step.conversation = ConversationState::Recorded;
        app.on_detail_read(
            id,
            revision,
            DetailRead::Timeline(Ok(super::task_app_support::timeline(
                id,
                vec![step],
                Some("plan #1"),
            ))),
        );
        app.on_detail_read(
            id,
            revision,
            DetailRead::Attempt {
                attempt_id: "act-plan".to_string(),
                from: 0,
                result: Ok(AttemptRead {
                    entries: vec![TranscriptEntry::new(
                        TranscriptEntryId::from("1"),
                        vec!["line 1".to_string()],
                        b"1".to_vec(),
                    )],
                    next_entry: 1,
                    closed: false,
                    truncated_tail: false,
                    complete: true,
                    no_record: false,
                    source: narsor_engine::controller_protocol::ExecutionEventsSource::Runner,
                    recorded_at: None,
                }),
            },
        );
        press(app, KeyCode::Enter);
        assert_eq!(app.step_focus(), StepFocus::Conversation);
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
            assert_eq!(names, ["ticket_id", "attempts", "dry_run", "target_repo"]);
            assert!(item.fields[0].required);
            assert_eq!(
                item.fields[0].description.as_deref(),
                Some("numer zgłoszenia")
            );
            assert!(!item.fields[1].required);
            assert_eq!(
                item.fields[1].description.as_deref(),
                Some("liczba całkowita · ile razy ponawiać")
            );
        }

        #[test]
        fn a_boolean_is_a_choice_that_starts_on_false_and_a_repository_offers_the_registered_ones()
        {
            let mut app = app_with(vec![]);
            open_form(&mut app);
            let item = chooser_item(&app, "implement-feature");
            assert_eq!(
                item.fields[2].kind,
                narsor::tui::chooser::FieldKind::Select(vec!["true".into(), "false".into()])
            );
            assert_eq!(item.fields[2].value(), "false");
            assert_eq!(
                item.fields[3].kind,
                narsor::tui::chooser::FieldKind::Select(vec!["docs".into(), "narsor".into()])
            );
            assert_eq!(
                app.new_task_form().unwrap().repositories(),
                ["docs", "narsor"]
            );
        }

        #[test]
        fn a_repository_is_a_free_line_while_none_is_registered() {
            let mut app = app_with(vec![]);
            ch(&mut app, 'n');
            app.open_new_task(workflows(), registry(vec![]));
            let item = chooser_item(&app, "implement-feature");
            assert!(matches!(
                item.fields[3].kind,
                narsor::tui::chooser::FieldKind::Text { .. }
            ));
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
                item.fields[0]
                    .error()
                    .is_some_and(|error| error.contains("ticket_id")),
                "{:?}",
                item.fields[0].error()
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
                item.fields[1]
                    .error()
                    .is_some_and(|error| error.contains("attempts") && error.contains("liczby")),
                "{:?}",
                item.fields[1].error()
            );
            assert!(item.fields[0].error().is_none());
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

            // `q` asks before throwing the data away.
            ch(&mut app, 'q');
            assert!(!app.should_quit());
            assert!(note(&app).contains("szkic"), "{}", note(&app));

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
            assert_eq!(field_value(&app, "implement-feature", "ticket_id"), "q");
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
                    violations: vec![],
                }),
            );
            let item = chooser_item(&app, PATH_LABEL);
            let names: Vec<&str> = item
                .fields
                .iter()
                .map(|field| field.name.as_str())
                .collect();
            assert_eq!(names, [PATH_FIELD, "ticket"]);
            assert!(item.description.contains("my-flow v4"));
            assert_eq!(
                field_value(&app, PATH_LABEL, PATH_FIELD),
                "/srv/ok.yaml",
                "what was typed stays"
            );

            press(&mut app, KeyCode::Down);
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
                    "target_repo": "docs",
                })
            );
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
            for _ in 0..3 {
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
                ('✗', "NAR-18".to_string(), "nie wysłano".to_string())
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
                "✗ nie wysłano",
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
            assert_eq!(app.history().unwrap().tab(), DetailTab::Conversation);
            press(&mut app, KeyCode::Enter);
            ch(&mut app, ']');
            assert_eq!(app.history().unwrap().tab(), DetailTab::Events);
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

        /// An execution from before the task scheduler, with a retried step
        /// and a conversation per attempt, written under `root`.
        fn seed_old_execution(root: &std::path::Path, id: &str) {
            use narsor_engine::domain::{
                Context as WorkflowContext, EventData, ExecutionStatus, RepositoryInfo,
                WorkflowExecution, Workspace,
            };
            use narsor_engine::persistence::conversation::ConversationSource;
            let store = ExecutionStore::new(root);
            store
                .save_execution(&WorkflowExecution {
                    id: id.to_string(),
                    workflow_id: "implement-feature".to_string(),
                    workflow_version: 1,
                    status: ExecutionStatus::Completed,
                    current_state: Some("plan".to_string()),
                    started_at: chrono::Utc::now(),
                    completed_at: None,
                    inputs: json!({}),
                    context: WorkflowContext::new(),
                    workspace: Workspace::git_checkout(
                        "/tmp/workspace".to_string(),
                        RepositoryInfo {
                            id: "repo-1".to_string(),
                            name: "narsor".to_string(),
                            provider: "gitlab".to_string(),
                            remote: "git@example.com:team/narsor.git".to_string(),
                        },
                        Some("main".to_string()),
                    ),
                    current_action_execution: None,
                    metadata: json!({}),
                })
                .expect("save execution");
            let log = store.event_log(id);
            let id_string = id.to_string();
            for data in [
                EventData::WorkflowStarted {
                    workflow_id: "implement-feature".to_string(),
                    workflow_version: 1,
                    inputs: json!({}),
                },
                EventData::StateEntered {
                    state_id: "plan".to_string(),
                },
                EventData::ActionStarted {
                    action_execution_id: "attempt-1".to_string(),
                    state_id: "plan".to_string(),
                    action_type: "agent".to_string(),
                },
                EventData::ActionCompleted {
                    action_execution_id: "attempt-1".to_string(),
                    output_label: "success".to_string(),
                },
            ] {
                log.append_next(&id_string, data).expect("append");
            }
            let conversations = store.conversation_store(id);
            let mut writer = conversations.writer("attempt-1").expect("writer");
            writer
                .append_narsor("prompt: zaplanuj zmianę", &[])
                .expect("prompt");
            writer
                .append(ConversationSource::Stdout, br#"{"type":"assistant"}"#, &[])
                .expect("stdout");
            writer.finish().expect("finish");
            conversations.close_attempt("attempt-1").expect("close");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn an_old_execution_is_read_through_the_history_routes_in_every_tab_and_recover_is_refused_with_the_reason(
        ) {
            let harness = RemoteHarness::start().await;
            seed_old_execution(harness.root.path(), "exec-old");
            let mut app = TaskApp::new(harness.owner.clone(), "test", None);
            app.refresh().await;

            let request = ch(&mut app, 'H');
            app.perform(request).await;
            assert_eq!(titles(&app), ["implement-feature exec-old"]);
            press(&mut app, KeyCode::Enter);

            app.sync_detail().await;
            let progress = screen(&mut app, 130, 30);
            assert!(
                progress.contains("plan #1"),
                "the timeline of the old execution:\n{progress}"
            );
            assert!(progress.contains("completed"), "{progress}");

            press(&mut app, KeyCode::Char(']'));
            app.sync_detail().await;
            let conversation = screen(&mut app, 130, 30);
            assert!(conversation.contains("zaplanuj zmianę"), "{conversation}");
            assert!(conversation.contains("assistant message"), "{conversation}");

            press(&mut app, KeyCode::Char(']'));
            app.sync_detail().await;
            let events = screen(&mut app, 130, 30);
            assert!(
                events.contains("WorkflowStarted") || events.contains("workflow"),
                "{events}"
            );
            assert!(events.contains("plan"), "{events}");

            press(&mut app, KeyCode::Char(']'));
            app.sync_detail().await;
            let conductor = screen(&mut app, 130, 30);
            assert!(
                conductor.contains("tylko do odczytu"),
                "no chat box, only history:\n{conductor}"
            );

            // Nothing of the old execution is a key of the task list: `w` is
            // refused up front, and the server's refusal is shown if it is asked.
            assert_eq!(ch(&mut app, 'w'), Request::None);
            assert!(!note(&app).is_empty());
            app.perform(Request::Recover("exec-old".to_string())).await;
            let said = note(&app);
            assert!(said.contains("wznowienie exec-old odrzucone"), "{said}");
            assert!(said.contains("completed"), "{said}");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn the_conversation_of_an_old_step_is_entered_from_its_timeline() {
            use narsor::tui::task_app::StepFocus;
            let harness = RemoteHarness::start().await;
            seed_old_execution(harness.root.path(), "exec-old");
            let mut app = TaskApp::new(harness.owner.clone(), "test", None);
            app.refresh().await;
            let request = ch(&mut app, 'H');
            app.perform(request).await;
            press(&mut app, KeyCode::Enter);
            app.sync_detail().await;
            assert_eq!(app.key_context().layer, Some(Context::HistoryAxis));

            press(&mut app, KeyCode::Enter);
            let history = app.history().unwrap();
            assert_eq!(history.step_focus(), StepFocus::Conversation);
            assert_eq!(app.key_context().layer, Some(Context::StepConversation));
            let picked = |app: &TaskApp| {
                let detail = app.history().unwrap().selected_detail().unwrap();
                let attempt = detail.attempts().get("attempt-1").unwrap();
                attempt
                    .transcript()
                    .selected_entry()
                    .map(|id| id.as_str().to_string())
            };
            ch(&mut app, 'g');
            let first = picked(&app);
            ch(&mut app, 'j');
            assert_ne!(picked(&app), first, "j picks the next entry");

            press(&mut app, KeyCode::Esc);
            assert_eq!(app.history().unwrap().step_focus(), StepFocus::Axis);
            assert_eq!(app.key_context().layer, Some(Context::HistoryAxis));
            assert_eq!(app.focus(), narsor::tui::task_app::Focus::Details);
            press(&mut app, KeyCode::Esc);
            assert_eq!(app.key_context().base, Context::History);
            assert_eq!(app.view_kind(), ViewKind::History);

            // Back into the details: on the timeline again.
            press(&mut app, KeyCode::Enter);
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.history().unwrap().step_focus(), StepFocus::Conversation);
            press(&mut app, KeyCode::Char(']'));
            press(&mut app, KeyCode::Char('['));
            assert_eq!(app.history().unwrap().step_focus(), StepFocus::Axis);
        }

        /// The history of `harness` open, with the keys in the conversation
        /// of the selected entry's `plan #1`.
        async fn in_an_old_steps_conversation(harness: &RemoteHarness) -> TaskApp {
            use narsor::tui::task_app::StepFocus;
            let mut app = TaskApp::new(harness.owner.clone(), "test", None);
            app.refresh().await;
            let request = ch(&mut app, 'H');
            app.perform(request).await;
            press(&mut app, KeyCode::Enter);
            app.sync_detail().await;
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.history().unwrap().step_focus(), StepFocus::Conversation);
            app
        }

        /// Reads the history again, then the details of its selected entry.
        async fn read_the_history_again(app: &mut TaskApp) {
            app.perform(Request::OpenHistory).await;
            app.sync_detail().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn another_selected_entry_after_a_read_of_the_history_starts_on_the_axis() {
            use narsor::tui::task_app::StepFocus;
            let harness = RemoteHarness::start().await;
            seed_old_execution(harness.root.path(), "exec-a");
            seed_old_execution(harness.root.path(), "exec-b");
            let mut app = in_an_old_steps_conversation(&harness).await;
            let held = app.history().unwrap().selected_entry().unwrap().key();

            // The entry read went away: another one is selected.
            let store = ExecutionStore::new(harness.root.path());
            let execution = held.strip_prefix("execution:").expect("an old execution");
            std::fs::remove_dir_all(store.execution_dir(execution)).expect("remove the execution");
            app.perform(Request::OpenHistory).await;
            let history = app.history().unwrap();
            assert_eq!(history.entries().len(), 1, "{:?}", history.rows());
            assert_ne!(history.selected_entry().unwrap().key(), held);
            assert_eq!(history.step_focus(), StepFocus::Axis);
            assert_eq!(app.key_context().layer, Some(Context::HistoryAxis));
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn an_old_step_that_leaves_the_run_drops_the_focus_to_the_axis_and_says_so() {
            use narsor::tui::task_app::StepFocus;
            use narsor_engine::domain::EventData;
            let harness = RemoteHarness::start().await;
            seed_old_execution(harness.root.path(), "exec-old");
            let mut app = in_an_old_steps_conversation(&harness).await;

            // The run now holds another step instead of `plan`.
            let store = ExecutionStore::new(harness.root.path());
            let log = store.event_log("exec-old");
            std::fs::remove_dir_all(log.dir()).expect("clear the events");
            let id = "exec-old".to_string();
            for data in [
                EventData::WorkflowStarted {
                    workflow_id: "implement-feature".to_string(),
                    workflow_version: 1,
                    inputs: json!({}),
                },
                EventData::StateEntered {
                    state_id: "implement".to_string(),
                },
            ] {
                store
                    .event_log("exec-old")
                    .append_next(&id, data)
                    .expect("append");
            }
            read_the_history_again(&mut app).await;
            assert_eq!(app.history().unwrap().step_focus(), StepFocus::Axis);
            assert_eq!(app.key_context().layer, Some(Context::HistoryAxis));
            let said = note(&app);
            assert!(
                said.contains("plan #1") && said.contains("krok zniknął"),
                "{said}"
            );
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Warning);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn an_old_conversation_that_stops_being_shown_drops_the_focus_quietly() {
            use narsor::tui::task_app::StepFocus;
            let harness = RemoteHarness::start().await;
            seed_old_execution(harness.root.path(), "exec-old");
            let mut app = in_an_old_steps_conversation(&harness).await;
            let before = note(&app);

            // The record of the attempt is gone.
            let store = ExecutionStore::new(harness.root.path());
            std::fs::remove_dir_all(store.execution_dir("exec-old").join("conversations"))
                .expect("remove the conversations");
            read_the_history_again(&mut app).await;
            assert_eq!(app.history().unwrap().step_focus(), StepFocus::Axis);
            assert_eq!(note(&app), before, "no notification");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn the_archived_tasks_of_a_server_are_the_history() {
            let harness = RemoteHarness::start().await;
            harness.create("NAR-9", Some("k8s")).await;
            harness.create("NAR-10", Some("k8s")).await;
            // Archiving a task that never started completes at once.
            harness
                .owner
                .control_command(
                    "NAR-9",
                    &narsor_engine::controller_protocol::OwnerCommandRequest {
                        command: narsor_engine::orchestration::OwnerCommand::Archive,
                        idempotency_key: "archive-1".to_string(),
                        expected_generation: None,
                    },
                )
                .await
                .expect("archive");

            let mut app = TaskApp::new(harness.owner.clone(), "test", None);
            app.refresh().await;
            assert_eq!(
                app.list().tasks().len(),
                1,
                "the archived task left the list"
            );

            let request = ch(&mut app, 'H');
            app.perform(request).await;
            assert_eq!(app.view_kind(), ViewKind::History);
            assert_eq!(titles(&app), ["NAR-9"]);
            let entry = app.history().unwrap().selected_entry().unwrap();
            assert_eq!(entry.summary().lifecycle, TaskLifecycle::Archived);

            // The details are read-only and read nothing: the storage is gone.
            press(&mut app, KeyCode::Enter);
            app.sync_detail().await;
            let screen = screen(&mut app, 130, 30);
            assert!(
                screen.contains("storage usunięty przy archiwizacji"),
                "{screen}"
            );

            // `Esc` twice: the task list is where it was.
            press(&mut app, KeyCode::Esc);
            press(&mut app, KeyCode::Esc);
            assert_eq!(app.view_kind(), ViewKind::Tasks);
            assert_eq!(app.list().selected_id(), Some("NAR-10"));
        }

        /// `w` on an old execution that failed asks the server to recover it
        /// (the one action of the history, R7), and the history is read again
        /// so the entry shows what the server made of it.
        #[tokio::test(flavor = "multi_thread")]
        async fn w_recovers_a_failed_old_execution_and_the_history_is_read_again() {
            let harness = RemoteHarness::start().await;
            seed_old_execution(harness.root.path(), "exec-failed");
            let store = ExecutionStore::new(harness.root.path());
            let mut failed = store.load_execution("exec-failed").expect("load");
            let definition_path = crate::engine_support::fixtures().join("engine/two_steps.yaml");
            failed.status = narsor_engine::domain::ExecutionStatus::Failed;
            failed.current_state = Some("first".to_string());
            failed.metadata = json!({
                "cli": { "definition_path": definition_path.display().to_string() },
                "engine": { "step_failure": {
                    "state_id": "first",
                    "action_execution_id": "attempt-2",
                    "episode": "attempt-2",
                    "error": "the build broke",
                    "side_effect_free": false,
                    "conductor_retry": false,
                } },
            });
            store.save_execution(&failed).expect("mark failed");

            let mut app = TaskApp::new(harness.owner.clone(), "test", None);
            app.refresh().await;
            let request = ch(&mut app, 'H');
            app.perform(request).await;
            assert_eq!(titles(&app), ["implement-feature exec-failed"]);
            let entry = app.history().unwrap().selected_entry().unwrap();
            assert_eq!(entry.recoverable(), Ok("exec-failed"));

            let request = ch(&mut app, 'w');
            assert_eq!(request, Request::Recover("exec-failed".to_string()));
            app.perform(request).await;
            let said = note(&app);
            assert!(said.contains("exec-failed wznawia się w tle"), "{said}");
            assert_eq!(
                app.view_kind(),
                ViewKind::History,
                "the history is still open"
            );
        }
    }

    // -- over the wire ---------------------------------------------------------

    mod over_the_wire {
        use super::*;

        use tokio::io::AsyncReadExt as _;
        use tokio::net::{TcpListener, TcpStream};

        use super::super::environment_support::{workflow_image, workflow_yaml};

        /// The workflow the tests create tasks from: a ticket, a number, a
        /// flag and a repository, and a title made of the ticket.
        fn implement_feature(version: u32) -> String {
            let header = format!("name: implement-feature\nversion: {version}\n");
            let declared = "description: \"Pełny cykl: plan, implementacja, review.\"\n\
                inputs:\n  - name: ticket_id\n    type: string\n    required: true\n    \
                description: numer zgłoszenia\n  - name: attempts\n    type: integer\n  \
                - name: dry_run\n    type: boolean\n  - name: target_repo\n    \
                type: repository\ndisplay:\n  title: \"{{ inputs.ticket_id }}\"\n";
            let body = workflow_yaml("implement-feature", Some(&workflow_image()));
            let rest = body.split_once("version: 1\n").expect("a version").1;
            format!("{header}{declared}{rest}")
        }

        fn write_workflow(root: &std::path::Path) {
            std::fs::write(
                root.join("workflows/implement-feature.yaml"),
                implement_feature(1),
            )
            .expect("write workflow");
        }

        /// The served control plane, with a live controller on its backend: a
        /// queued task shows its place (`#n`), not `offline`. Keep the second
        /// part alive for as long as the test runs.
        async fn harness() -> (
            RemoteHarness,
            (narsor_engine::controller_protocol::ControllerClient, u64),
        ) {
            let harness = RemoteHarness::start_with(write_workflow).await;
            let controller = harness.controller("k8s").await;
            (harness, controller)
        }

        /// A task the server already has.
        async fn existing(harness: &RemoteHarness, id: &str) {
            let mut request = super::super::remote_support::create_request(id, Some("k8s"));
            request.launch.inputs = json!({ "ticket_id": id });
            harness
                .owner
                .create_control_task(&request)
                .await
                .expect("create");
        }

        async fn open(harness: &RemoteHarness) -> TaskApp {
            let mut app = TaskApp::new(harness.owner.clone(), "test", None);
            app.refresh().await;
            app
        }

        /// An application whose client reaches the server through a proxy
        /// that loses the answer of the first `losses` connections.
        async fn app_behind_a_lossy_proxy(harness: &RemoteHarness, losses: usize) -> TaskApp {
            let proxy = lossy_proxy(harness.handle.addr(), losses).await;
            let client = ApiClient::new(proxy, harness.handle.token().to_string());
            let mut app = TaskApp::new(client, "test", None);
            // Read through the direct client: the proxy's first connection
            // must be the creation.
            app.set_capabilities(harness.owner.control_capabilities().await.unwrap());
            app.load(harness.owner.control_tasks().await.unwrap());
            app
        }

        /// Forwards connections to `upstream`; for the first `losses` of them
        /// the request goes through and the answer never comes back: the
        /// server did what it was asked and the client does not know.
        async fn lossy_proxy(upstream: SocketAddr, losses: usize) -> SocketAddr {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
            let address = listener.local_addr().expect("an address");
            tokio::spawn(async move {
                let mut seen = 0usize;
                while let Ok((client, _)) = listener.accept().await {
                    let lose = seen < losses;
                    seen += 1;
                    tokio::spawn(async move {
                        let Ok(server) = TcpStream::connect(upstream).await else {
                            return;
                        };
                        if !lose {
                            let (mut client, mut server) = (client, server);
                            let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                            return;
                        }
                        let (mut from_client, _to_client) = client.into_split();
                        let (mut from_server, mut to_server) = server.into_split();
                        let request = tokio::spawn(async move {
                            let _ = tokio::io::copy(&mut from_client, &mut to_server).await;
                        });
                        // The answer started: the server has the task. Hang up.
                        let mut first = [0u8; 1024];
                        let _ = from_server.read(&mut first).await;
                        request.abort();
                    });
                }
            });
            address
        }

        async fn open_the_form(app: &mut TaskApp, harness: &RemoteHarness) {
            let workflows = harness.owner.control_workflows().await.unwrap();
            let repositories = harness.owner.control_repositories().await.unwrap();
            ch(app, 'n');
            app.open_new_task(workflows, repositories);
            choose(app, "implement-feature");
            fill_ticket(app, "NAR-18");
        }

        async fn tasks_on_the_server(harness: &RemoteHarness) -> Vec<String> {
            harness
                .owner
                .control_tasks()
                .await
                .unwrap()
                .tasks
                .iter()
                .map(|task| task.task_id.to_string())
                .collect()
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn n_then_a_workflow_then_a_ticket_then_enter_puts_a_queued_row_last_and_selected() {
            let (harness, _controller) = harness().await;
            existing(&harness, "NAR-1").await;
            existing(&harness, "NAR-2").await;
            let mut app = open(&harness).await;
            let before = rows(&app);
            assert_eq!(before.len(), 2);

            let request = ch(&mut app, 'n');
            assert_eq!(request, Request::OpenNewTask);
            app.perform(request).await;
            assert_eq!(app.view_kind(), ViewKind::NewTask);
            choose(&mut app, "implement-feature");
            assert!(
                chooser_item(&app, "implement-feature")
                    .description
                    .contains("Pełny cykl"),
                "{}",
                chooser_item(&app, "implement-feature").description
            );
            fill_ticket(&mut app, "NAR-18");
            let request = enter_until_send(&mut app);
            assert!(matches!(request, Request::SendTask(_)), "{request:?}");
            app.perform(request).await;

            let now = rows(&app);
            assert_eq!(now.len(), 3);
            assert_eq!(now[..2], before[..], "the list below is not touched");
            assert_eq!(now[2], ('◷', "NAR-18".to_string(), "#3".to_string()));
            let id = app.list().tasks()[2].task_id.to_string();
            assert_eq!(app.list().selected_id(), Some(id.as_str()));
            assert!(app.list().pending().is_empty());
            assert!(app.outgoing().is_empty());
            assert!(
                app.remembered_new_task().is_none(),
                "what was sent is forgotten"
            );
            assert!(!app.detail().has_drafts());
            assert!(note(&app).contains("zadanie utworzone"), "{}", note(&app));
            assert_eq!(tasks_on_the_server(&harness).await.len(), 3);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_lost_answer_is_sent_again_with_the_same_id_and_makes_one_task() {
            let (harness, _controller) = harness().await;
            let mut app = app_behind_a_lossy_proxy(&harness, 1).await;
            open_the_form(&mut app, &harness).await;
            let mut request = Request::None;
            for _ in 0..8 {
                request = press(&mut app, KeyCode::Enter);
                if request != Request::None {
                    break;
                }
            }
            let Request::SendTask(id) = request else {
                panic!("a send was expected");
            };
            app.perform(Request::SendTask(id.clone())).await;

            // The server has the task, the client does not know.
            assert_eq!(
                tasks_on_the_server(&harness).await,
                std::slice::from_ref(&id)
            );
            assert_eq!(
                rows(&app),
                [('↻', "NAR-18".to_string(), "wysyłanie…".to_string())]
            );

            assert!(
                app.retry_outgoing_if_due(Instant::now() + RETRY_AFTER * 2)
                    .await
            );
            assert_eq!(rows(&app), [('◷', "NAR-18".to_string(), "#1".to_string())]);
            assert_eq!(app.list().selected_id(), Some(id.as_str()));
            assert_eq!(
                tasks_on_the_server(&harness).await,
                std::slice::from_ref(&id),
                "the same id made no second task"
            );
            assert!(app.list().pending().is_empty());
            assert!(app.outgoing().is_empty());
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_confirmed_task_selected_away_from_a_step_conversation_starts_on_the_axis() {
            use narsor::tui::task_app::StepFocus;
            let (harness, _controller) = harness().await;
            let mut app = app_behind_a_lossy_proxy(&harness, 1).await;
            open_the_form(&mut app, &harness).await;
            let Request::SendTask(id) = super::enter_until_send(&mut app) else {
                panic!("a send was expected");
            };
            app.perform(Request::SendTask(id.clone())).await;
            assert_eq!(app.list().pending().len(), 1, "the answer was lost");

            // Meanwhile the operator reads another task's step.
            app.load(listing(vec![running_task("a", 1)], "c"));
            into_step_conversation(&mut app, "a");

            assert!(
                app.retry_outgoing_if_due(Instant::now() + RETRY_AFTER * 2)
                    .await
            );
            assert_eq!(app.list().selected_id(), Some(id.as_str()));
            assert_eq!(app.step_focus(), StepFocus::Axis);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn when_the_retry_is_lost_too_the_row_is_unsent_and_a_read_that_lists_the_task_settles_it(
        ) {
            let (harness, _controller) = harness().await;
            let mut app = app_behind_a_lossy_proxy(&harness, 2).await;
            open_the_form(&mut app, &harness).await;
            let Request::SendTask(id) = super::enter_until_send(&mut app) else {
                panic!("a send was expected");
            };
            app.perform(Request::SendTask(id.clone())).await;
            assert!(
                app.retry_outgoing_if_due(Instant::now() + RETRY_AFTER * 2)
                    .await
            );
            assert_eq!(
                rows(&app),
                [('✗', "NAR-18".to_string(), "nie wysłano".to_string())]
            );
            assert_eq!(
                tasks_on_the_server(&harness).await,
                std::slice::from_ref(&id)
            );

            // The connection is back (a read succeeded), the operator sends again.
            app.load(harness.owner.control_tasks().await.unwrap());
            assert_eq!(
                rows(&app),
                [('◷', "NAR-18".to_string(), "#1".to_string())],
                "a read that lists the task is as good as an answer"
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn r_on_the_unsent_row_makes_the_same_task_when_the_list_does_not_have_it_yet() {
            let (harness, _controller) = harness().await;
            let mut app = app_behind_a_lossy_proxy(&harness, 2).await;
            open_the_form(&mut app, &harness).await;
            let Request::SendTask(id) = super::enter_until_send(&mut app) else {
                panic!("a send was expected");
            };
            app.perform(Request::SendTask(id.clone())).await;
            app.retry_outgoing_if_due(Instant::now() + RETRY_AFTER * 2)
                .await;
            assert_eq!(app.list().pending()[0].state, PendingState::NotSent);

            // A read that does not list it (an older listing) brings the
            // connection back without settling the row.
            app.load(listing(vec![], "stale"));
            assert_eq!(app.list().pending().len(), 1);
            let request = ch(&mut app, 'r');
            assert_eq!(request, Request::SendTask(id.clone()));
            app.perform(request).await;
            assert_eq!(rows(&app), [('◷', "NAR-18".to_string(), "#1".to_string())]);
            assert_eq!(
                tasks_on_the_server(&harness).await,
                [id],
                "one task, one id"
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_refusal_is_not_a_lost_answer_the_form_comes_back_with_what_was_typed() {
            let (harness, _controller) = harness().await;
            let mut app = open(&harness).await;
            open_the_form(&mut app, &harness).await;
            // The catalogue moved on after the form was opened.
            std::fs::write(
                harness.root.path().join("workflows/implement-feature.yaml"),
                implement_feature(2),
            )
            .unwrap();
            let request = super::enter_until_send(&mut app);
            app.perform(request).await;

            assert_eq!(app.view_kind(), ViewKind::NewTask, "the form is back");
            assert_eq!(
                field_value(&app, "implement-feature", "ticket_id"),
                "NAR-18"
            );
            assert!(app.list().pending().is_empty());
            assert!(app.outgoing().is_empty());
            assert!(note(&app).contains("serwer odrzucił"), "{}", note(&app));
            assert_eq!(app.notifier().latest().unwrap().severity, Severity::Error);
            assert!(tasks_on_the_server(&harness).await.is_empty());
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_path_with_violations_lists_all_of_them_and_a_good_one_creates_a_task() {
            let (harness, _controller) = harness().await;
            let broken = "name: broken\nversion: 1\nstates:\n  work:\n    action:\n      type: script\n      \
                command: \"true\"\n    outputs: [success]\n    transitions:\n      success: nowhere\n  \
                island:\n    description: nobody comes here\nterminal_states: [done]\n";
            let expected: Vec<String> = narsor_engine::workflow::validate_source(broken)
                .expect("the YAML loads")
                .iter()
                .map(ToString::to_string)
                .collect();
            assert!(expected.len() >= 2, "{expected:?}");
            let bad_path = harness.root.path().join("broken.yaml");
            std::fs::write(&bad_path, broken).unwrap();
            let good_path = harness.root.path().join("mine.yaml");
            std::fs::write(
                &good_path,
                implement_feature(1).replace("implement-feature", "mine"),
            )
            .unwrap();

            let mut app = open(&harness).await;
            let request = ch(&mut app, 'n');
            app.perform(request).await;
            assert_eq!(
                chooser_item(&app, PATH_LABEL).unavailable,
                None,
                "a local daemon reads files"
            );
            choose(&mut app, PATH_LABEL);
            press(&mut app, KeyCode::Enter);
            typed(&mut app, bad_path.to_str().unwrap());
            let request = press(&mut app, KeyCode::Enter);
            assert_eq!(
                request,
                Request::ValidatePath(bad_path.to_str().unwrap().to_string())
            );
            app.perform(request).await;
            let description = &chooser_item(&app, PATH_LABEL).description;
            for violation in &expected {
                assert!(
                    description.contains(violation.as_str()),
                    "{violation}\n{description}"
                );
            }
            assert!(
                tasks_on_the_server(&harness).await.is_empty(),
                "nothing was created"
            );

            // A good file: first checked, then created from.
            press(&mut app, KeyCode::Enter);
            for _ in 0..bad_path.to_str().unwrap().len() {
                press(&mut app, KeyCode::Backspace);
            }
            typed(&mut app, good_path.to_str().unwrap());
            let request = press(&mut app, KeyCode::Enter);
            app.perform(request).await;
            assert!(matches!(
                app.new_task_form().unwrap().path_check().unwrap().verdict,
                PathVerdict::Valid { version: 1, .. }
            ));
            press(&mut app, KeyCode::Down);
            typed(&mut app, "NAR-77");
            let request = super::enter_until_send(&mut app);
            let Request::SendTask(id) = &request else {
                panic!("{request:?}");
            };
            let id = id.clone();
            app.perform(request).await;
            assert_eq!(tasks_on_the_server(&harness).await, [id]);
            assert_eq!(rows(&app)[0].0, '◷');
            assert_eq!(rows(&app)[0].1, "NAR-77");
        }
    }

    // -- the repositories ------------------------------------------------------

    mod repositories {
        use super::*;

        use super::super::environment_support::{workflow_image, workflow_yaml};

        fn submit_repository_form(app: &mut TaskApp) -> Request {
            for _ in 0..8 {
                let request = press(app, KeyCode::Enter);
                if request != Request::None {
                    return request;
                }
            }
            Request::None
        }

        fn with_a_repository_input(root: &std::path::Path) {
            let body = workflow_yaml("implement-feature", Some(&workflow_image()));
            let rest = body.split_once("version: 1\n").unwrap().1;
            std::fs::write(
                root.join("workflows/implement-feature.yaml"),
                format!(
                    "name: implement-feature\nversion: 1\ninputs:\n  - name: target_repo\n    \
                     type: repository\n{rest}"
                ),
            )
            .unwrap();
        }

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

        #[tokio::test(flavor = "multi_thread")]
        async fn a_repository_that_is_added_is_listed_and_offered_by_the_repository_field() {
            let harness = RemoteHarness::start_with(with_a_repository_input).await;
            let mut app = TaskApp::new(harness.owner.clone(), "test", None);
            app.refresh().await;

            let request = ch(&mut app, 'R');
            app.perform(request).await;
            assert_eq!(app.view_kind(), ViewKind::Repositories);
            assert!(names(&app).is_empty());

            ch(&mut app, 'a');
            press(&mut app, KeyCode::Enter); // to the first field
            typed(&mut app, "narsor");
            press(&mut app, KeyCode::Down);
            typed(&mut app, "git@github.com:acme/narsor.git");
            let request = submit_repository_form(&mut app);
            assert!(matches!(request, Request::AddRepository(_)), "{request:?}");
            app.perform(request).await;

            assert_eq!(names(&app), ["narsor"]);
            assert!(app.repositories().unwrap().form().is_none());
            assert!(
                note(&app).contains("dodano repozytorium narsor"),
                "{}",
                note(&app)
            );
            let screen = screen(&mut app, 130, 30);
            assert!(
                screen.contains("git@github.com:acme/narsor.git"),
                "{screen}"
            );

            // It is in the form of a new task, too.
            press(&mut app, KeyCode::Esc);
            let request = ch(&mut app, 'n');
            app.perform(request).await;
            choose(&mut app, "implement-feature");
            let item = chooser_item(&app, "implement-feature");
            assert_eq!(
                item.fields[0].kind,
                narsor::tui::chooser::FieldKind::Select(vec!["narsor".to_string()])
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn what_the_server_refuses_is_shown_next_to_the_field_and_the_form_stays() {
            let harness = RemoteHarness::start().await;
            let mut app = TaskApp::new(harness.owner.clone(), "test", None);
            app.refresh().await;
            let request = ch(&mut app, 'R');
            app.perform(request).await;

            ch(&mut app, 'a');
            press(&mut app, KeyCode::Enter);
            typed(&mut app, "evil");
            press(&mut app, KeyCode::Down);
            typed(&mut app, "ext::sh -c evil");
            let request = submit_repository_form(&mut app);
            app.perform(request).await;

            let form = app.repositories().unwrap().form().expect("the form stays");
            let remote = &form.items()[0].fields[1];
            assert!(
                remote.error().is_some(),
                "the refusal is at the remote field"
            );
            assert!(form.items()[0].fields[0].error().is_none());
            assert_eq!(remote.value(), "ext::sh -c evil", "what was typed stays");
            assert!(names(&app).is_empty());
            let screen = screen(&mut app, 130, 30);
            assert!(screen.contains("✗"), "{screen}");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn d_y_removes_the_repository_and_a_removed_one_is_gone_from_the_list() {
            let harness = RemoteHarness::start().await;
            for (id, remote) in [
                ("docs", "git@github.com:acme/docs.git"),
                ("narsor", "git@github.com:acme/narsor.git"),
            ] {
                harness
                    .owner
                    .control_add_repository(
                        &narsor_engine::controller_protocol::CreateRepositoryRequest {
                            id: id.to_string(),
                            remote: remote.to_string(),
                            name: None,
                            provider: None,
                            default_branch: None,
                            description: None,
                        },
                    )
                    .await
                    .unwrap();
            }
            let mut app = TaskApp::new(harness.owner.clone(), "test", None);
            app.refresh().await;
            let request = ch(&mut app, 'R');
            app.perform(request).await;
            assert_eq!(names(&app), ["docs", "narsor"]);

            ch(&mut app, 'd');
            let request = ch(&mut app, 'y');
            assert_eq!(request, Request::RemoveRepository("docs".to_string()));
            app.perform(request).await;
            assert_eq!(names(&app), ["narsor"]);
            assert!(
                note(&app).contains("usunięto repozytorium docs"),
                "{}",
                note(&app)
            );
            assert_eq!(
                harness
                    .owner
                    .control_repositories()
                    .await
                    .unwrap()
                    .repositories
                    .len(),
                1
            );

            // Gone behind our back: said, and the list is read again.
            harness
                .owner
                .control_remove_repository("narsor")
                .await
                .unwrap();
            ch(&mut app, 'd');
            let request = ch(&mut app, 'y');
            app.perform(request).await;
            assert!(names(&app).is_empty());
            assert!(note(&app).contains("już nie istnieje"), "{}", note(&app));
        }
    }
}

/// U5: the installation's default harness — the header names it, `h` changes
/// it from the list, the details, the history and the repositories, never
/// from a field being typed into or over another window (R12–R15, AE3, AE4).
mod task_app_harness {
    use std::time::Instant;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::json;

    use narsor::api::ApiClient;
    use narsor::tui::keymap::{self, Action, Context, BINDINGS};
    use narsor::tui::task_app::actions::Overlay;
    use narsor::tui::task_app::harness::HarnessHeader;
    use narsor::tui::task_app::notify::Severity;
    use narsor::tui::task_app::{DetailTab, Focus, Request, TaskApp};
    use narsor_engine::domain::ExecutionStatus;

    use super::remote_support::RemoteHarness;
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

    fn press(app: &mut TaskApp, code: KeyCode) -> Request {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ch(app: &mut TaskApp, character: char) -> Request {
        press(app, KeyCode::Char(character))
    }

    fn h() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)
    }

    fn note(app: &TaskApp) -> (String, Option<Severity>) {
        let latest = app.notifier().latest();
        (
            latest.map(|note| note.text.clone()).unwrap_or_default(),
            latest.map(|note| note.severity),
        )
    }

    fn labels(app: &TaskApp) -> Vec<String> {
        harness_chooser(app)
            .items()
            .iter()
            .map(|item| item.label.clone())
            .collect()
    }

    fn harness_chooser(app: &TaskApp) -> &narsor::tui::chooser::Chooser {
        match app.overlay() {
            Some(overlay @ Overlay::Harness(_)) => overlay.chooser().expect("a list"),
            other => panic!("the harness list was expected, got {other:?}"),
        }
    }

    fn header(app: &mut TaskApp, width: u16) -> String {
        use narsor::tui::style::Styles;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 30)).unwrap();
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
        buffer.content()[..usize::from(width)]
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ae3_the_header_names_the_harness_h_changes_it_and_the_configuration_comes_back() {
        let harness = RemoteHarness::start().await;
        harness.create("t1", None).await;
        let mut app = TaskApp::new(harness.owner.clone(), "local", None);
        app.refresh().await;
        let top = header(&mut app, 120);
        assert!(top.contains("harness claude-code (konfiguracja)"), "{top}");

        let request = ch(&mut app, 'h');
        assert_eq!(request, Request::OpenHarness);
        app.perform(request).await;
        assert_eq!(
            labels(&app),
            [
                "● domyślny z konfiguracji (claude-code)",
                "  claude-code",
                "  codex",
                "  opencode"
            ]
        );
        assert_eq!(harness_chooser(&app).selected(), 0, "on the one in force");

        for _ in 0..3 {
            ch(&mut app, 'j');
        }
        let request = press(&mut app, KeyCode::Enter);
        assert_eq!(request, Request::SetHarness(Some("opencode".to_string())));
        assert!(app.overlay().is_none(), "the list closes at once");
        assert_eq!(app.harness_header(), HarnessHeader::Pending("opencode"));
        let top = header(&mut app, 120);
        assert!(top.contains("harness opencode…"), "{top}");

        app.perform(request).await;
        let top = header(&mut app, 120);
        assert!(top.contains("harness opencode "), "{top}");
        assert!(!top.contains("konfiguracja"), "{top}");
        let (text, severity) = note(&app);
        assert!(text.contains("opencode"), "{text}");
        assert_eq!(severity, Some(Severity::Success));
        let served = harness.owner.harness_selection().await.unwrap();
        assert_eq!(served.override_harness.as_deref(), Some("opencode"));

        let request = ch(&mut app, 'h');
        app.perform(request).await;
        assert_eq!(harness_chooser(&app).selected(), 3);
        assert_eq!(labels(&app)[3], "● opencode");
        ch(&mut app, 'g');
        for _ in 0..3 {
            ch(&mut app, 'k');
        }
        let request = press(&mut app, KeyCode::Enter);
        assert_eq!(request, Request::SetHarness(None));
        assert_eq!(app.harness_header(), HarnessHeader::Pending("claude-code"));
        app.perform(request).await;
        let top = header(&mut app, 120);
        assert!(top.contains("harness claude-code (konfiguracja)"), "{top}");
        assert!(note(&app).0.contains("claude-code (konfiguracja)"));
        let served = harness.owner.harness_selection().await.unwrap();
        assert_eq!(served.override_harness, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unsupported_harness_is_a_problem_with_its_cause_and_the_header_keeps_the_old_one() {
        let harness = RemoteHarness::start().await;
        harness.create("t1", None).await;
        let mut app = TaskApp::new(harness.owner.clone(), "local", None);
        app.refresh().await;
        app.perform(Request::SetHarness(Some("nope".to_string())))
            .await;
        let (text, severity) = note(&app);
        assert_eq!(severity, Some(Severity::Error));
        assert!(text.contains("domyślny harness nie zmieniony"), "{text}");
        assert!(text.contains("nope"), "the cause: {text}");
        assert_eq!(
            app.harness_header(),
            HarnessHeader::Known {
                name: "claude-code",
                configured: true
            }
        );
        assert!(app.connection().is_live());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_daemon_without_a_selector_shows_a_question_mark_and_keeps_what_was_known() {
        let harness = RemoteHarness::start_without_harness_selector().await;
        harness.create("t1", None).await;
        let mut app = TaskApp::new(harness.owner.clone(), "local", None);
        assert_eq!(app.harness_header(), HarnessHeader::Unknown);
        app.refresh().await;
        assert_eq!(app.harness_header(), HarnessHeader::Unknown);
        assert!(header(&mut app, 120).contains("harness ? "));
        assert!(
            app.connection().is_live(),
            "no selector is not a broken connection"
        );

        // `h` with nothing known: says why, opens nothing.
        let request = ch(&mut app, 'h');
        app.perform(request).await;
        assert!(app.overlay().is_none());
        let (text, severity) = note(&app);
        assert_eq!(severity, Some(Severity::Error));
        assert!(text.contains("nie udało się odczytać"), "{text}");

        // A value known before the read failed stays.
        app.set_harness_selection(harness_selection("claude-code", Some("codex"), &HARNESSES));
        app.refresh().await;
        assert_eq!(
            app.harness_header(),
            HarnessHeader::Known {
                name: "codex",
                configured: false
            }
        );
        assert!(app.connection().is_live());
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
    fn h_is_not_a_command_over_a_window_in_the_move_mode_or_in_a_field() {
        for context in [
            Context::Global,
            Context::Menu,
            Context::Confirm,
            Context::Move,
            Context::Panel,
            Context::Form,
        ] {
            assert_eq!(keymap::lookup(context, &h()), None, "{context:?}");
        }
        // Over the action menu: nothing, and the menu stays.
        let mut app = app(None);
        ch(&mut app, '.');
        assert!(matches!(app.overlay(), Some(Overlay::Menu(_))));
        assert_eq!(ch(&mut app, 'h'), Request::None);
        assert!(matches!(app.overlay(), Some(Overlay::Menu(_))));
        // Over the harness list itself: nothing either.
        press(&mut app, KeyCode::Esc);
        assert!(app.open_harness());
        assert_eq!(ch(&mut app, 'h'), Request::None);
        assert!(matches!(app.overlay(), Some(Overlay::Harness(_))));
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
    fn the_list_starts_on_the_one_in_force_which_cannot_be_chosen_again() {
        let mut app = app(Some("codex"));
        assert!(app.open_harness());
        assert_eq!(
            labels(&app),
            [
                "  domyślny z konfiguracji (claude-code)",
                "  claude-code",
                "● codex",
                "  opencode"
            ]
        );
        let chooser = harness_chooser(&app);
        assert_eq!(chooser.selected(), 2);
        assert!(chooser.items()[2].unavailable.is_some(), "greyed out");
        assert!(chooser.items()[0].unavailable.is_none());

        // Choosing it changes nothing and sends nothing.
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
        assert!(app.overlay().is_none());
        assert_eq!(
            app.harness_header(),
            HarnessHeader::Known {
                name: "codex",
                configured: false
            }
        );

        // `Esc` closes, the configuration entry sends a reset.
        assert!(app.open_harness());
        press(&mut app, KeyCode::Esc);
        assert!(app.overlay().is_none());
        assert!(app.open_harness());
        ch(&mut app, 'g');
        ch(&mut app, 'k');
        ch(&mut app, 'k');
        assert_eq!(harness_chooser(&app).selected(), 0);
        assert_eq!(press(&mut app, KeyCode::Enter), Request::SetHarness(None));
    }

    #[test]
    fn a_daemon_that_names_no_harness_leaves_only_the_configuration() {
        let mut app = app(None);
        app.set_harness_selection(harness_selection("claude-code", None, &[]));
        assert!(app.open_harness());
        assert_eq!(
            labels(&app),
            [
                "● domyślny z konfiguracji (claude-code)",
                "  demon nie zgłasza obsługiwanych harnessów"
            ]
        );
        ch(&mut app, 'j');
        assert_eq!(press(&mut app, KeyCode::Enter), Request::None);
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
    use narsor_engine::orchestration::{TaskEventKind, TaskLifecycle};

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
                    interrupted(&format!("t{n}"), TaskLifecycle::Running, 1, true),
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
        assert_eq!(row.lifecycle, TaskLifecycle::Provisioning);
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
            TaskLifecycle::Interrupted,
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
            TaskLifecycle::Interrupted,
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
            TaskLifecycle::Interrupted,
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
        let planned = interrupted("a", TaskLifecycle::Running, 2, true);
        assert_eq!(attention_of(&planned), Attention::None);
        let pausing = interrupted("a", TaskLifecycle::PauseRequested, 0, false);
        assert_eq!(attention_of(&pausing), Attention::Problem);
        let given_up = exhausted(
            interrupted("a", TaskLifecycle::Running, 6, false),
            "could not resume automatically: pod evicted",
        );
        assert_eq!(attention_of(&given_up), Attention::Problem);
    }

    /// The menu's reason for an interrupted task mentions a planned resume.
    #[test]
    fn an_action_of_a_task_resuming_on_its_own_says_so() {
        let planned = interrupted("a", TaskLifecycle::Running, 2, true);
        let reason = availability(&planned, TaskAction::Interrupt);
        assert_eq!(
            reason.reason(),
            Some(
                "zadanie jest przerwane — wznowi się samo (próba 2/5) albo uruchom je od nowa (s)"
            )
        );
        let pausing = interrupted("a", TaskLifecycle::PauseRequested, 0, false);
        assert_eq!(
            availability(&pausing, TaskAction::Interrupt).reason(),
            Some("zadanie jest przerwane — uruchom je od nowa (s)")
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
                interrupted("a", TaskLifecycle::Running, 1, true),
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
            TaskLifecycle::Running,
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
