use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;

use super::*;
use crate::tui::style::Styles;

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn type_text(chat: &mut Chat, text: &str) {
    for c in text.chars() {
        chat.on_key(press(KeyCode::Char(c)));
    }
}

fn new_line(chat: &mut Chat) {
    chat.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
}

#[test]
fn an_empty_box_is_one_row_of_text() {
    let chat = Chat::new();
    // The label and one row of text.
    assert_eq!(chat.box_rows(40, 40), 2);
}

#[test]
fn the_box_grows_with_the_lines_up_to_half_the_details() {
    let mut chat = Chat::new();
    type_text(&mut chat, "a");
    for _ in 0..4 {
        new_line(&mut chat);
        type_text(&mut chat, "b");
    }
    // Five lines and the label, with room to spare.
    assert_eq!(chat.box_rows(40, 40), 6);
    // Details of 8 rows: the box stops at 4.
    assert_eq!(chat.box_rows(40, 8), 4);
}

#[test]
fn the_limit_follows_the_height_of_the_details() {
    let mut chat = Chat::new();
    type_text(&mut chat, "a");
    for _ in 0..9 {
        new_line(&mut chat);
        type_text(&mut chat, "b");
    }
    assert_eq!(chat.box_rows(40, 20), 10);
    assert_eq!(chat.box_rows(40, 12), 6);
}

#[test]
fn a_text_past_the_limit_scrolls_and_keeps_the_cursor_in_view() {
    let mut chat = Chat::new();
    type_text(&mut chat, "first");
    for n in 0..9 {
        new_line(&mut chat);
        type_text(&mut chat, &format!("line{n}"));
    }
    let area = Rect::new(0, 0, 40, 6);
    let mut terminal = Terminal::new(TestBackend::new(40, 6)).expect("terminal");
    let styles = Styles::new(true);
    terminal
        .draw(|frame| chat.render(frame, area, 12, &styles, true))
        .expect("draw");
    let screen: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(screen.contains("line8"), "the end is in view: {screen}");
    assert!(!screen.contains("first"), "the start scrolled away: {screen}");
}

#[test]
fn enter_asks_before_it_sends() {
    let mut chat = Chat::new();
    type_text(&mut chat, "hello");
    assert_eq!(chat.on_key(press(KeyCode::Enter)), ChatEffect::None);
    assert!(chat.is_confirming());
    assert_eq!(
        chat.on_key(press(KeyCode::Enter)),
        ChatEffect::Send("hello".to_string())
    );
}

#[test]
fn ctrl_j_adds_a_line_and_enter_does_not() {
    let mut chat = Chat::new();
    type_text(&mut chat, "a");
    new_line(&mut chat);
    type_text(&mut chat, "b");
    assert_eq!(chat.draft_text(), "a\nb");
}

#[test]
fn a_paste_of_many_lines_is_a_draft_and_is_not_sent() {
    let mut chat = Chat::new();
    assert!(chat.paste("one\ntwo\nthree"));
    assert_eq!(chat.draft_text(), "one\ntwo\nthree");
    assert!(!chat.is_confirming());
}

#[test]
fn a_draft_stays_when_the_keys_leave_and_come_back() {
    let mut chat = Chat::new();
    type_text(&mut chat, "kept");
    chat.leave();
    assert!(!chat.is_typing());
    chat.enter();
    assert!(chat.is_typing());
    assert_eq!(chat.draft_text(), "kept");
}

#[test]
fn a_refused_send_keeps_the_text_and_a_sent_one_empties_the_box() {
    let mut chat = Chat::new();
    type_text(&mut chat, "text");
    chat.settle(Err(("serwer odmówił".to_string(), false)));
    assert_eq!(chat.error(), Some("serwer odmówił"));
    assert_eq!(chat.draft_text(), "text");
    chat.settle(Ok(()));
    assert_eq!(chat.draft_text(), "");
    assert_eq!(chat.error(), None);
}

#[test]
fn a_lost_connection_keeps_the_key_for_the_retry() {
    let mut chat = Chat::new();
    type_text(&mut chat, "text");
    let key = chat.send_key(|| "k1".to_string());
    chat.settle(Err(("brak połączenia".to_string(), true)));
    assert_eq!(chat.send_key(|| "k2".to_string()), key);
    type_text(&mut chat, "!");
    assert_eq!(chat.send_key(|| "k2".to_string()), "k2");
}
