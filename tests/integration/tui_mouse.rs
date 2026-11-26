//! The mouse in the TUI: the key that switches mouse reporting, and how a
//! wheel notch or a click reaches the panel under the pointer.
//!
//! A frame is drawn on a `TestBackend` with `layout::render`, then mouse
//! events go to `TaskApp::on_mouse` at cells of that frame, as the terminal
//! would send them. What is asserted is the state the event left: the
//! selection, the focus, the scroll positions.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;

use narsor::api::ApiClient;
use narsor::tui::style::Styles;
use narsor::tui::task_app::{layout, Focus, Request, TaskApp};

use narsor_engine::domain::ExecutionStatus;

use crate::task_app_support::{
    listing, queued, task, titled, with_controller, with_panel, with_pending, with_runner,
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

/// A mouse event of `kind` at `column`, `row`.
fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

fn click(app: &mut TaskApp, column: u16, row: u16) -> Request {
    app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), column, row))
}

fn wheel_down(app: &mut TaskApp, column: u16, row: u16) {
    app.on_mouse(mouse(MouseEventKind::ScrollDown, column, row));
}

fn wheel_up(app: &mut TaskApp, column: u16, row: u16) {
    app.on_mouse(mouse(MouseEventKind::ScrollUp, column, row));
}

/// Where `text` is on the screen: its first cell.
fn find(buffer: &Buffer, text: &str) -> (u16, u16) {
    for y in 0..buffer.area.height {
        let line = row(buffer, y);
        if let Some(at) = line.find(text) {
            let column = line[..at].chars().count();
            return (u16::try_from(column).unwrap(), y);
        }
    }
    panic!("{text:?} is not on the screen:\n{}", screen(buffer));
}

/// What a stray event must not change: the selected task, the focus, the
/// filter and the order of the list.
fn what_the_operator_sees(app: &TaskApp) -> (Option<String>, Focus, String, Vec<String>) {
    (
        app.list().selected_id().map(str::to_string),
        app.focus(),
        app.list().filter().to_string(),
        app.list().rows().into_iter().map(|row| row.title).collect(),
    )
}

/// Three queued tasks, `NAR-1` to `NAR-3`.
fn queue() -> TaskApp {
    let mut app = app();
    app.load(listing(
        (1..=3)
            .map(|n| titled(queued(&format!("t{n}"), n), &format!("NAR-{n}")))
            .collect(),
        "c",
    ));
    app
}

mod routing {
    use super::*;

    #[test]
    fn a_click_on_a_task_behind_the_action_menu_changes_nothing_behind_it() {
        // AE5
        let mut app = queue();
        key(&mut app, KeyCode::Char('.'));
        assert!(app.overlay().is_some());
        let buffer = draw(&mut app, 120, 30);
        let (column, y) = find(&buffer, "NAR-3");
        let before = what_the_operator_sees(&app);

        assert_eq!(click(&mut app, column, y), Request::None);
        wheel_down(&mut app, column, y);

        assert_eq!(what_the_operator_sees(&app), before);
        assert!(app.overlay().is_some(), "the menu stays open");
    }

    #[test]
    fn the_header_the_bar_and_a_too_small_screen_take_no_clicks() {
        let mut app = queue();
        key(&mut app, KeyCode::Enter);
        let before = what_the_operator_sees(&app);
        draw(&mut app, 120, 30);
        for (column, y) in [(5, 0), (5, 29), (60, 0), (60, 29)] {
            click(&mut app, column, y);
            wheel_down(&mut app, column, y);
        }
        assert_eq!(what_the_operator_sees(&app), before);

        draw(&mut app, 50, 10);
        for (column, y) in [(2, 3), (40, 5)] {
            click(&mut app, column, y);
            wheel_down(&mut app, column, y);
        }
        assert_eq!(what_the_operator_sees(&app), before);
    }

    #[test]
    fn in_the_narrow_layout_the_mouse_reaches_only_the_panel_on_screen() {
        let mut app = queue();
        draw(&mut app, 90, 30);
        // Where the details would be in the split layout.
        click(&mut app, 60, 10);
        assert_eq!(app.focus(), Focus::List);
    }

    #[test]
    fn moves_drags_releases_and_other_buttons_change_nothing() {
        let mut app = queue();
        let buffer = draw(&mut app, 120, 30);
        let (column, y) = find(&buffer, "NAR-3");
        let before = what_the_operator_sees(&app);
        for kind in [
            MouseEventKind::Moved,
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Down(MouseButton::Middle),
            MouseEventKind::ScrollLeft,
            MouseEventKind::ScrollRight,
        ] {
            for (x, y) in [(column, y), (60, 10)] {
                assert_eq!(app.on_mouse(mouse(kind, x, y)), Request::None);
            }
        }
        assert_eq!(what_the_operator_sees(&app), before);
    }

    #[test]
    fn while_the_filter_is_typed_or_a_task_is_moved_the_mouse_does_nothing() {
        let mut filtering = queue();
        key(&mut filtering, KeyCode::Char('/'));
        key(&mut filtering, KeyCode::Char('N'));
        let mut moving = queue();
        key(&mut moving, KeyCode::Char('m'));
        assert!(moving.list().moving().is_some());

        for mut app in [filtering, moving] {
            let buffer = draw(&mut app, 120, 30);
            let (column, y) = find(&buffer, "NAR-3");
            let before = what_the_operator_sees(&app);
            wheel_down(&mut app, column, y);
            wheel_up(&mut app, column, y);
            click(&mut app, column, y);
            click(&mut app, 60, 10);
            assert_eq!(what_the_operator_sees(&app), before);
        }
    }

    #[test]
    fn a_click_on_empty_space_in_the_details_gives_them_the_focus_and_keeps_the_task() {
        let mut app = queue();
        draw(&mut app, 120, 30);
        let selected = app.list().selected_id().map(str::to_string);
        click(&mut app, 100, 25);
        assert_eq!(app.focus(), Focus::Details);
        assert_eq!(app.list().selected_id().map(str::to_string), selected);
    }
}
