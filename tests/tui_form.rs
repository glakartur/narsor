//! Tests of the form engine: what the rules of editing, moving between
//! fields, the confirmation and the events do, for any form built from fields.
//!
//! The same rules hold in every form, so most checks run over two different
//! forms (AE10): nothing here is written for one owner.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use narsor::tui::chooser::{Chooser, ChooserAction, ChooserField, ChooserFocus, ChooserItem};
use narsor::tui::form::events::{Direction, FormEvent, Growth};
use narsor::tui::keymap::{self, Context};
use narsor::tui::style::Styles;
use ratatui::style::Color;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn press(chooser: &mut Chooser, code: KeyCode) -> ChooserAction {
    chooser.on_key(key(code))
}

fn type_text(chooser: &mut Chooser, text: &str) {
    for character in text.chars() {
        chooser.on_key(key(KeyCode::Char(character)));
    }
}

fn ctrl_j() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)
}

/// Two forms with nothing in common but being forms: a title, a number and a
/// note; and a remote, a mode and a long text.
fn forms() -> Vec<Chooser> {
    vec![
        Chooser::new(vec![ChooserItem::new("one", "Pierwszy.")
            .with_field(ChooserField::text("title").required())
            .with_field(ChooserField::integer("count"))
            .with_field(ChooserField::multiline("note"))])
        .confirming_submit(),
        Chooser::new(vec![ChooserItem::new("two", "Drugi.")
            .with_field(ChooserField::text("remote"))
            .with_field(ChooserField::select("mode", vec!["a".into(), "b".into()]))
            .with_field(ChooserField::multiline("body").required())])
        .confirming_submit()
        .with_inputs_layout(),
    ]
}

fn into_fields(chooser: &mut Chooser) {
    press(chooser, KeyCode::Tab);
    assert_eq!(chooser.focus(), ChooserFocus::Fields);
}

fn value(chooser: &Chooser, field: usize) -> String {
    chooser.fields_of(0)[field].value().to_string()
}

// -- editing ---------------------------------------------------------------

#[test]
fn delete_home_and_end_work_in_every_text_field() {
    for mut chooser in forms() {
        into_fields(&mut chooser);
        // Whichever field the focus starts on, it is a text one.
        type_text(&mut chooser, "abc");
        let field = (0..3)
            .find(|&index| value(&chooser, index) == "abc")
            .expect("the typed field");
        press(&mut chooser, KeyCode::Left);
        press(&mut chooser, KeyCode::Left);
        press(&mut chooser, KeyCode::Delete);
        assert_eq!(
            value(&chooser, field),
            "ac",
            "Delete removes under the cursor"
        );
        press(&mut chooser, KeyCode::End);
        press(&mut chooser, KeyCode::Delete);
        assert_eq!(
            value(&chooser, field),
            "ac",
            "Delete at the end does nothing"
        );
        press(&mut chooser, KeyCode::Home);
        type_text(&mut chooser, "x");
        assert_eq!(value(&chooser, field), "xac");
    }
}

#[test]
fn home_and_end_go_to_the_logical_line_not_the_drawn_row() {
    let mut chooser = Chooser::new(vec![
        ChooserItem::new("go", "Start.").with_field(ChooserField::multiline("prompt"))
    ]);
    into_fields(&mut chooser);
    type_text(&mut chooser, "ab");
    chooser.on_key(ctrl_j());
    type_text(&mut chooser, "cdef");
    press(&mut chooser, KeyCode::Left);
    press(&mut chooser, KeyCode::Home);
    assert_eq!(
        chooser.fields_of(0)[0].cursor(),
        3,
        "start of the second line"
    );
    press(&mut chooser, KeyCode::End);
    assert_eq!(
        chooser.fields_of(0)[0].cursor(),
        7,
        "end of the second line"
    );
    press(&mut chooser, KeyCode::Up);
    press(&mut chooser, KeyCode::End);
    assert_eq!(chooser.fields_of(0)[0].cursor(), 2, "end of the first line");
}

// -- moving between fields -------------------------------------------------

#[test]
fn the_focus_starts_on_the_first_field_from_the_top() {
    for chooser in forms() {
        let mut chooser = chooser;
        press(&mut chooser, KeyCode::Enter);
        if chooser.focus() != ChooserFocus::Fields {
            press(&mut chooser, KeyCode::Tab);
        }
        type_text(&mut chooser, "z");
        let typed: Vec<usize> = (0..3).filter(|&i| value(&chooser, i) == "z").collect();
        assert_eq!(typed, [0]);
    }
}

#[test]
fn a_multiline_field_lets_go_on_the_second_arrow_in_a_row() {
    for mut chooser in forms() {
        into_fields(&mut chooser);
        // The third field is the multi-line one in both forms; one Down moves
        // on from a one-line field and from a select.
        press(&mut chooser, KeyCode::Down);
        press(&mut chooser, KeyCode::Down);
        type_text(&mut chooser, "ab");
        press(&mut chooser, KeyCode::Left);
        // First press: the cursor goes to the start of the text, same field.
        press(&mut chooser, KeyCode::Up);
        assert_eq!(chooser.fields_of(0)[2].cursor(), 0);
        type_text(&mut chooser, "X");
        assert_eq!(value(&chooser, 2), "Xab");
        // Another key cancels the pending press: Up, Right, Up stays.
        press(&mut chooser, KeyCode::Up);
        press(&mut chooser, KeyCode::Right);
        press(&mut chooser, KeyCode::Up);
        type_text(&mut chooser, "Y");
        assert_eq!(value(&chooser, 2), "YXab", "still in the field");
        // Two in a row leave it, even with the cursor already at the start.
        press(&mut chooser, KeyCode::Up);
        press(&mut chooser, KeyCode::Up);
        type_text(&mut chooser, "m");
        assert_eq!(value(&chooser, 2), "YXab", "the focus left the field");
    }
}

#[test]
fn tab_cancels_a_pending_edge_press() {
    let mut chooser = Chooser::new(vec![ChooserItem::new("go", "Start.")
        .with_field(ChooserField::text("name"))
        .with_field(ChooserField::multiline("note"))
        .with_field(ChooserField::text("tail"))])
    .with_inputs_layout();
    into_fields(&mut chooser);
    // Walk to the multi-line field, wherever the layout starts the focus.
    for _ in 0..3 {
        type_text(&mut chooser, "|");
        if !value(&chooser, 1).is_empty() {
            break;
        }
        press(&mut chooser, KeyCode::Down);
    }
    // One Down at its last row arms the edge; a round of Tab through the
    // three fields comes back to it disarmed.
    press(&mut chooser, KeyCode::Down);
    for _ in 0..3 {
        press(&mut chooser, KeyCode::Tab);
    }
    press(&mut chooser, KeyCode::Down);
    type_text(&mut chooser, "n");
    assert_eq!(
        value(&chooser, 1),
        "|n",
        "one Down after Tab only arms again"
    );
}

#[test]
fn a_rebuilt_form_closes_the_confirmation() {
    let item = || ChooserItem::new("go", "Start.").with_field(ChooserField::text("a"));
    let mut chooser = Chooser::new(vec![item()]).confirming_submit();
    into_fields(&mut chooser);
    type_text(&mut chooser, "a");
    press(&mut chooser, KeyCode::Enter);
    let mut chooser = chooser.rebuilt(vec![item()]);
    let action = press(&mut chooser, KeyCode::Enter);
    assert_eq!(
        action,
        ChooserAction::None,
        "Enter after a rebuild asks again; it does not send"
    );
}

#[test]
fn a_select_and_a_one_line_field_move_on_one_arrow() {
    let mut chooser = Chooser::new(vec![ChooserItem::new("go", "Start.")
        .with_field(ChooserField::select("mode", vec!["a".into(), "b".into()]))
        .with_field(ChooserField::text("remote"))
        .with_field(ChooserField::text("name"))]);
    into_fields(&mut chooser);
    press(&mut chooser, KeyCode::Right);
    assert_eq!(value(&chooser, 0), "b", "← → change the value");
    press(&mut chooser, KeyCode::Down);
    type_text(&mut chooser, "r");
    assert_eq!(value(&chooser, 1), "r", "one Down from a select");
    press(&mut chooser, KeyCode::Down);
    type_text(&mut chooser, "n");
    assert_eq!(value(&chooser, 2), "n", "one Down from a one-line field");
    press(&mut chooser, KeyCode::Up);
    type_text(&mut chooser, "R");
    assert_eq!(value(&chooser, 1), "rR");
}

#[test]
fn the_list_keeps_the_arrows_and_enter_goes_on() {
    let mut chooser = Chooser::new(vec![
        ChooserItem::new("a", "A.").with_field(ChooserField::text("note")),
        ChooserItem::new("b", "B.").with_field(ChooserField::text("note")),
    ])
    .enter_fields_on_activate();
    press(&mut chooser, KeyCode::Down);
    press(&mut chooser, KeyCode::Down);
    assert_eq!(chooser.selected(), 1, "the last entry stays");
    assert_eq!(chooser.focus(), ChooserFocus::Actions);
    press(&mut chooser, KeyCode::Enter);
    assert_eq!(
        chooser.focus(),
        ChooserFocus::Fields,
        "Enter goes to the field"
    );
}

// -- the confirmation ------------------------------------------------------

#[test]
fn enter_on_the_last_field_asks_and_only_the_popups_enter_sends() {
    for mut chooser in forms() {
        into_fields(&mut chooser);
        // Fill the required field, wherever it is, then walk to the end.
        for index in 0..3 {
            if chooser.fields_of(0)[index].required {
                // Walk with Enter until the cursor is in it.
            }
        }
        let mut guard = 0;
        let action = loop {
            type_text(&mut chooser, "x");
            let action = press(&mut chooser, KeyCode::Enter);
            guard += 1;
            if chooser.is_confirming() || guard > 8 {
                break action;
            }
        };
        assert_eq!(action, ChooserAction::None, "the form did not send yet");
        assert!(chooser.is_confirming());
        assert_eq!(chooser.context(), Context::FormConfirm);
        let before: Vec<String> = (0..3).map(|i| value(&chooser, i)).collect();
        // Other keys do nothing, not even a send.
        for event in [
            key(KeyCode::Char('a')),
            key(KeyCode::Tab),
            key(KeyCode::Char('y')),
            key(KeyCode::Down),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            ctrl_j(),
        ] {
            assert_eq!(chooser.on_key(event), ChooserAction::None, "{event:?}");
            assert!(chooser.is_confirming(), "{event:?}");
        }
        let after: Vec<String> = (0..3).map(|i| value(&chooser, i)).collect();
        assert_eq!(before, after, "keys in the window change no value");
        assert!(!chooser.paste("pasted"), "a paste does not reach the field");
        // Esc: back to editing, nothing lost.
        assert_eq!(press(&mut chooser, KeyCode::Esc), ChooserAction::None);
        assert!(!chooser.is_confirming());
        assert_eq!(chooser.focus(), ChooserFocus::Fields);
        assert_eq!(chooser.context(), Context::Panel);
        // Enter again, Enter in the window: one send.
        press(&mut chooser, KeyCode::Enter);
        assert!(chooser.is_confirming());
        let sent = press(&mut chooser, KeyCode::Enter);
        assert!(matches!(sent, ChooserAction::Submit(_)), "{sent:?}");
        assert!(!chooser.is_confirming());
        // The window is closed: another Enter only asks again, it sends nothing.
        assert_eq!(press(&mut chooser, KeyCode::Enter), ChooserAction::None);
        assert!(chooser.is_confirming());
    }
}

#[test]
fn esc_returns_to_the_same_field_with_its_cursor() {
    let mut chooser = Chooser::new(vec![ChooserItem::new("go", "Start.")
        .with_field(ChooserField::text("a"))
        .with_field(ChooserField::text("b"))])
    .confirming_submit();
    into_fields(&mut chooser);
    type_text(&mut chooser, "one");
    press(&mut chooser, KeyCode::Enter);
    type_text(&mut chooser, "two");
    press(&mut chooser, KeyCode::Left);
    press(&mut chooser, KeyCode::Enter);
    assert!(chooser.is_confirming());
    press(&mut chooser, KeyCode::Esc);
    assert_eq!(
        chooser.fields_of(0)[1].cursor(),
        2,
        "the cursor is where it was"
    );
    type_text(&mut chooser, "!");
    assert_eq!(value(&chooser, 1), "tw!o");
    assert_eq!(value(&chooser, 0), "one");
}

#[test]
fn a_problem_in_a_field_opens_no_window() {
    let mut chooser = Chooser::new(vec![
        ChooserItem::new("go", "Start.").with_field(ChooserField::text("a").required())
    ])
    .confirming_submit();
    into_fields(&mut chooser);
    assert_eq!(press(&mut chooser, KeyCode::Enter), ChooserAction::None);
    assert!(!chooser.is_confirming());
    assert!(chooser.fields_of(0)[0].error().is_some());
}

#[test]
fn an_entry_without_fields_asks_too_and_a_plain_list_does_not() {
    let mut asking =
        Chooser::new(vec![ChooserItem::new("approve", "Zatwierdza.")]).confirming_submit();
    assert_eq!(press(&mut asking, KeyCode::Enter), ChooserAction::None);
    assert!(asking.is_confirming());
    assert!(matches!(
        press(&mut asking, KeyCode::Enter),
        ChooserAction::Submit(_)
    ));
    let mut plain = Chooser::new(vec![ChooserItem::new("pick", "Wybiera.")]);
    assert!(matches!(
        press(&mut plain, KeyCode::Enter),
        ChooserAction::Submit(_)
    ));
}

#[test]
fn the_window_has_its_own_keys_in_the_key_map() {
    let hints = keymap::hints(Context::FormConfirm);
    let keys: Vec<&str> = hints.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(keys, ["Enter", "Esc"], "{hints:?}");
    assert_eq!(
        keymap::lookup(Context::FormConfirm, &key(KeyCode::Char('q'))),
        None
    );
}

// -- focus, growth and events ----------------------------------------------

#[test]
fn a_form_without_the_focus_takes_no_key_and_keeps_its_text() {
    let mut chooser = Chooser::new(vec![
        ChooserItem::new("go", "Start.").with_field(ChooserField::multiline("c"))
    ]);
    into_fields(&mut chooser);
    type_text(&mut chooser, "abc");
    press(&mut chooser, KeyCode::Left);
    chooser.take_events();
    chooser.set_focus(false);
    assert_eq!(
        chooser.take_events(),
        vec![FormEvent::FocusChanged { gained: false }]
    );
    type_text(&mut chooser, "zzz");
    assert!(!chooser.paste("zzz"));
    assert_eq!(value(&chooser, 0), "abc");
    assert_eq!(chooser.fields_of(0)[0].cursor(), 2);
    chooser.set_focus(false);
    assert!(chooser.take_events().is_empty(), "no duplicate event");
    chooser.set_focus(true);
    type_text(&mut chooser, "!");
    assert_eq!(value(&chooser, 0), "ab!c", "editing goes on where it was");
}

#[test]
fn a_field_grows_to_its_limit_and_reports_each_change_once() {
    let mut chooser = Chooser::new(vec![
        ChooserItem::new("go", "Start.").with_field(ChooserField::multiline("c"))
    ]);
    chooser.set_growth(Growth {
        direction: Direction::Up,
        limit_rows: 10,
    });
    into_fields(&mut chooser);
    chooser.take_events();
    let mut resized = Vec::new();
    for _ in 0..14 {
        chooser.on_key(ctrl_j());
        for event in chooser.take_events() {
            if let FormEvent::Resized { rows } = event {
                resized.push(rows);
            }
        }
    }
    assert!(
        resized.windows(2).all(|pair| pair[0] < pair[1]),
        "{resized:?}"
    );
    assert_eq!(resized.last(), Some(&10), "the last report is the limit");
    assert!(resized.iter().all(|&rows| rows <= 10));
    // Past the limit it scrolls: no more reports, the field still takes text.
    chooser.on_key(ctrl_j());
    assert!(!chooser
        .take_events()
        .iter()
        .any(|event| matches!(event, FormEvent::Resized { .. })));
    assert_eq!(chooser.preferred_rows(40), 10);
    // Removing lines shrinks it again, and says so.
    for _ in 0..10 {
        chooser.on_key(key(KeyCode::Backspace));
    }
    assert!(chooser
        .take_events()
        .iter()
        .any(|event| matches!(event, FormEvent::Resized { rows } if *rows < 10)));
}

#[test]
fn a_limit_below_one_row_still_leaves_one() {
    let chooser = Chooser::new(vec![
        ChooserItem::new("go", "Start.").with_field(ChooserField::multiline("c"))
    ]);
    chooser.set_growth(Growth {
        direction: Direction::Down,
        limit_rows: 0,
    });
    assert_eq!(chooser.preferred_rows(40), 1);
}

#[test]
fn submit_cancel_and_edit_are_reported() {
    let mut chooser = Chooser::new(vec![
        ChooserItem::new("go", "Start.").with_field(ChooserField::text("a"))
    ])
    .confirming_submit();
    into_fields(&mut chooser);
    chooser.take_events();
    type_text(&mut chooser, "a");
    assert_eq!(chooser.take_events(), vec![FormEvent::Edited]);
    press(&mut chooser, KeyCode::Enter);
    assert_eq!(chooser.take_events(), vec![FormEvent::ConfirmRequested]);
    press(&mut chooser, KeyCode::Enter);
    assert_eq!(chooser.take_events(), vec![FormEvent::Submitted]);
    press(&mut chooser, KeyCode::Esc);
    assert_eq!(chooser.take_events(), vec![FormEvent::Cancelled]);
}

// -- the look --------------------------------------------------------------

#[test]
fn the_field_in_edit_is_a_navy_darker_than_the_task_header() {
    let styles = Styles::new(false);
    let Some(Color::Rgb(r, g, b)) = styles.field_editing().bg else {
        panic!("a field in edit has an RGB fill");
    };
    assert!(b > r && b > g, "navy: blue leads ({r},{g},{b})");
    let Some(Color::Rgb(hr, hg, hb)) = styles.task_header_background().bg else {
        panic!("the header has an RGB fill");
    };
    assert!(
        u32::from(r) + u32::from(g) + u32::from(b) < u32::from(hr) + u32::from(hg) + u32::from(hb),
        "darker than the header"
    );
    assert_eq!(
        styles.field_editing().fg,
        None,
        "the text colour is not changed"
    );
}

#[test]
fn without_colour_a_field_in_edit_has_no_fill() {
    let styles = Styles::new(true);
    assert_eq!(styles.field_editing().bg, None);
    assert_eq!(styles.field_editing().fg, None);
}
