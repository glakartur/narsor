//! The mouse in the TUI: the key that switches mouse reporting, and how a
//! wheel notch or a click reaches the panel under the pointer.
//!
//! A frame is drawn on a `TestBackend` with `layout::render`, then mouse
//! events go to `TaskApp::on_mouse` at cells of that frame, as the terminal
//! would send them. What is asserted is the state the event left: the
//! selection, the focus, the scroll positions.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;

use narsor::api::ApiClient;
use narsor::tui::style::Styles;
use narsor::tui::task_app::{layout, TaskApp};

use narsor_engine::domain::ExecutionStatus;

use crate::task_app_support::{
    listing, task, titled, with_controller, with_panel, with_pending, with_runner,
};

fn app() -> TaskApp {
    TaskApp::new(
        ApiClient::new("127.0.0.1:9".parse().unwrap(), "token"),
        "local",
        None,
    )
}

fn key(app: &mut TaskApp, code: KeyCode) {
    app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
}

/// Draws one frame of `app` on a `width` × `height` terminal.
fn draw(app: &mut TaskApp, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| layout::render(frame, app, &Styles::new(false), Instant::now()))
        .unwrap();
    terminal.backend().buffer().clone()
}

/// Row `y` of the screen, as text.
fn row(buffer: &Buffer, y: u16) -> String {
    (0..buffer.area.width)
        .map(|x| buffer[(x, y)].symbol())
        .collect()
}

/// The whole screen, as text.
fn screen(buffer: &Buffer) -> String {
    (0..buffer.area.height)
        .map(|y| row(buffer, y))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The last row of the screen: the shortcut bar.
fn bar(buffer: &Buffer) -> String {
    row(buffer, buffer.area.height - 1)
}

fn listed(count: usize) -> TaskApp {
    let mut app = app();
    let tasks = (1..=count)
        .map(|n| {
            titled(
                task(&format!("t{n}"), "local", "queued", n as u32),
                &format!("NAR-{n}"),
            )
        })
        .collect();
    app.load(listing(tasks, "c"));
    app
}

/// Task `a` waits on an agent's question, its details are open and the keys
/// are in the answer panel's text field.
fn asking_app() -> TaskApp {
    let form = serde_json::json!({
        "execution_id": "exec-1",
        "state_id": "review_scope",
        "action_execution_id": "act-7",
        "instruction": "Która wersja API?",
        "fields": [{ "name": "answer", "type": "text", "required": true }],
        "continuation": "resume_agent",
        "episode": "ep-q",
        "requested_at": "2026-10-09T11:54:00Z"
    });
    let running = with_runner(
        with_controller(task("a", "local", "running", 1)),
        ExecutionStatus::InputNeeded,
        "review_scope",
    );
    let mut app = app();
    app.load(listing(
        vec![titled(with_panel(with_pending(running, form)), "NAR-12")],
        "c",
    ));
    key(&mut app, KeyCode::Char(' '));
    key(&mut app, KeyCode::Enter);
    assert!(app.panel_focused());
    app
}

mod toggle {
    use super::*;

    #[test]
    fn p_switches_mouse_reporting_off_and_on_and_the_bar_says_when_it_is_off() {
        let mut app = listed(2);
        assert!(app.mouse_captured(), "the mouse is on at start");
        assert!(!bar(&draw(&mut app, 120, 30)).contains("mysz wył."));

        key(&mut app, KeyCode::Char('P'));
        assert!(!app.mouse_captured());
        assert!(bar(&draw(&mut app, 120, 30)).contains("mysz wył."));
        key(&mut app, KeyCode::Char('?'));
        assert!(screen(&draw(&mut app, 120, 30)).contains("mysz wyłączona"));
        key(&mut app, KeyCode::Esc);

        key(&mut app, KeyCode::Char('P'));
        assert!(app.mouse_captured());
        let shown = bar(&draw(&mut app, 120, 30));
        assert!(!shown.contains("mysz wył."), "{shown}");
    }

    #[test]
    fn p_typed_into_a_field_is_text_and_leaves_the_mouse_as_it_was() {
        // The answer panel's text field.
        let mut app = asking_app();
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Char('P'));
        assert!(app.mouse_captured());
        let draft = app.detail().view("a").unwrap().draft_text();
        assert!(draft.contains('P'), "{draft:?}");

        // The list filter.
        let mut app = listed(1);
        key(&mut app, KeyCode::Char('/'));
        key(&mut app, KeyCode::Char('P'));
        assert!(app.mouse_captured());
        assert_eq!(app.list().filter(), "P");
    }
}
