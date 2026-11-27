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
    archived, archived_step, archived_timeline, listing, queued, task, titled, with_controller,
    with_panel, with_pending, with_runner,
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

mod list {
    use super::*;

    /// The right part of the screen, from `column` on: the details.
    fn right_of(buffer: &Buffer, column: u16) -> String {
        (0..buffer.area.height)
            .map(|y| {
                row(buffer, y)
                    .chars()
                    .skip(usize::from(column))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_wheel_over_the_list_moves_the_selection_and_leaves_the_focus_in_the_details() {
        let mut app = queue();
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.focus(), Focus::Details);
        let buffer = draw(&mut app, 120, 30);
        // The details show the selected NAR-1 too; NAR-2 is only in the list.
        let (column, y) = find(&buffer, "NAR-2");

        wheel_down(&mut app, column, y);
        wheel_down(&mut app, column, y);

        assert_eq!(app.list().selected_id(), Some("t3"));
        assert_eq!(app.focus(), Focus::Details);
        let buffer = draw(&mut app, 120, 30);
        // The details begin where the frame of the list ends.
        let details = right_of(&buffer, find(&buffer, "╮╭").0 + 1);
        assert!(details.contains("NAR-3"), "{details}");
    }

    #[test]
    fn the_wheel_stops_at_the_first_and_the_last_task() {
        let mut app = queue();
        let buffer = draw(&mut app, 120, 30);
        let (column, y) = find(&buffer, "NAR-2");
        assert_eq!(app.list().selected_id(), Some("t1"));

        wheel_up(&mut app, column, y);
        assert_eq!(app.list().selected_id(), Some("t1"));

        for _ in 0..4 {
            wheel_down(&mut app, column, y);
        }
        assert_eq!(app.list().selected_id(), Some("t3"));
    }

    #[test]
    fn a_click_on_any_row_of_a_task_selects_it_and_keeps_the_focus_on_the_list() {
        let mut app = queue();
        let buffer = draw(&mut app, 120, 30);
        // A task takes three rows; its title is on the second.
        let (column, y) = find(&buffer, "NAR-3");
        assert_eq!(find(&buffer, "t3").1 + 1, y, "the title is the second row");

        assert_eq!(click(&mut app, column, y), Request::None);

        assert_eq!(app.list().selected_id(), Some("t3"));
        assert_eq!(app.focus(), Focus::List);
    }

    #[test]
    fn a_click_on_a_task_from_the_details_selects_it_and_moves_the_focus_to_the_list() {
        let mut app = queue();
        key(&mut app, KeyCode::Enter);
        let buffer = draw(&mut app, 120, 30);
        let (column, y) = find(&buffer, "NAR-2");

        click(&mut app, column, y);

        assert_eq!(app.list().selected_id(), Some("t2"));
        assert_eq!(app.focus(), Focus::List);
    }

    #[test]
    fn a_click_on_an_archived_task_selects_it_and_its_kept_run_follows() {
        let mut app = queue();
        key(&mut app, KeyCode::Char('H'));
        app.open_history(listing(
            vec![archived("old", "NAR-9", 5), archived("older", "NAR-8", 1)],
            "c",
        ));
        for (id, state) in [("old", "planning"), ("older", "reviewing")] {
            app.on_history_timeline(
                id,
                Ok(archived_timeline(
                    id,
                    vec![archived_step(state, "claude", "opus", 0.4)],
                )),
            );
        }
        let buffer = draw(&mut app, 120, 30);
        assert!(screen(&buffer).contains("planning"), "{}", screen(&buffer));
        let (column, y) = find(&buffer, "NAR-8");

        click(&mut app, column, y);

        assert_eq!(app.history().unwrap().selected(), 1);
        let shown = screen(&draw(&mut app, 120, 30));
        assert!(shown.contains("reviewing"), "{shown}");
        assert!(!shown.contains("planning"), "{shown}");
    }
}

mod details {
    use super::*;

    use narsor::tui::task_app::detail::DetailRead;
    use narsor::tui::task_app::tabs::conversation::{AttemptRead, Conversation};
    use narsor::tui::task_app::DetailTab;
    use narsor::tui::transcript::{TranscriptEntry, TranscriptEntryId};
    use narsor_engine::controller_protocol::{ExecutionEventsResponse, ExecutionEventsSource};
    use narsor_engine::engine::{ConversationState, PositionStatus};

    use crate::task_app_support::{execution_events, position, timeline};

    /// Running task `a`, selected, with the keys on the list and its details
    /// on *Rozmowa*.
    fn running_app() -> TaskApp {
        let running = with_runner(
            with_controller(task("a", "local", "running", 1)),
            ExecutionStatus::Running,
            "edit",
        );
        let mut app = app();
        app.load(listing(vec![titled(running, "NAR-1")], "c"));
        assert_eq!(app.detail().tab(), DetailTab::Conversation);
        app
    }

    /// Puts the keys in the details and opens `tab` there.
    fn open_tab(app: &mut TaskApp, tab: DetailTab) {
        key(app, KeyCode::Enter);
        assert_eq!(app.focus(), Focus::Details);
        while app.detail().tab() != tab {
            key(app, KeyCode::Char(']'));
        }
    }

    /// Entries `from..=to` of the step `edit`: a header `AGENT · nn` and two
    /// lines, `line nn` and `more nn`, `nn` two digits.
    fn entries(from: u64, to: u64) -> AttemptRead {
        AttemptRead {
            entries: (from..=to)
                .map(|n| {
                    TranscriptEntry::new(
                        TranscriptEntryId::from(format!("{n:02}")),
                        vec![format!("line {n:02}"), format!("more {n:02}")],
                        Vec::new(),
                    )
                    .with_header(format!("AGENT · {n:02}"))
                })
                .collect(),
            next_entry: to,
            closed: false,
            truncated_tail: false,
            complete: true,
            no_record: false,
            source: ExecutionEventsSource::Runner,
            recorded_at: None,
        }
    }

    fn read_entries(app: &mut TaskApp, from: u64, to: u64) {
        app.on_detail_reads(
            "a",
            1,
            vec![DetailRead::Attempt {
                attempt_id: "act-edit".into(),
                from: from - 1,
                result: Ok(entries(from, to)),
            }],
        );
    }

    /// [`running_app`] whose step `edit` said 30 entries, more than its
    /// window shows; *Rozmowa* follows the newest one.
    fn talking_app() -> TaskApp {
        let mut app = running_app();
        let mut edit = position("edit", PositionStatus::InProgress);
        edit.conversation = ConversationState::Recorded;
        app.on_detail_reads(
            "a",
            1,
            vec![DetailRead::Timeline(Ok(timeline(
                "a",
                vec![edit],
                Some("edit #1"),
            )))],
        );
        read_entries(&mut app, 1, 30);
        app
    }

    fn conversation(app: &TaskApp) -> &Conversation {
        app.detail().view("a").unwrap().conversation()
    }

    #[test]
    fn a_notch_up_over_a_followed_conversation_scrolls_three_lines_and_stops_the_follow() {
        let mut app = talking_app();
        let buffer = draw(&mut app, 120, 30);
        let bottom = conversation(&app).transcript().scroll_offset();
        assert!(conversation(&app).follows());
        let (column, y) = find(&buffer, "line 29");

        wheel_up(&mut app, column, y);

        let transcript = conversation(&app).transcript();
        assert_eq!(transcript.scroll_offset(), bottom - 3);
        assert!(!transcript.is_following_end());
        assert!(!conversation(&app).follows());
        assert_eq!(app.focus(), Focus::List, "the wheel leaves the focus");

        read_entries(&mut app, 31, 32);
        draw(&mut app, 120, 30);
        assert_eq!(
            conversation(&app).transcript().scroll_offset(),
            bottom - 3,
            "new entries do not move the view"
        );
    }

    #[test]
    fn a_notch_down_at_the_bottom_of_a_conversation_stays_where_page_down_stays() {
        let mut app = talking_app();
        let buffer = draw(&mut app, 120, 30);
        let bottom = conversation(&app).transcript().scroll_offset();
        let (column, y) = find(&buffer, "line 29");

        wheel_down(&mut app, column, y);
        assert_eq!(conversation(&app).transcript().scroll_offset(), bottom);

        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::PageDown);
        assert_eq!(conversation(&app).transcript().scroll_offset(), bottom);
    }

    #[test]
    fn a_click_on_an_entry_of_the_conversation_selects_it_and_focuses_the_details() {
        let mut app = talking_app();
        let buffer = draw(&mut app, 120, 30);
        // The cursor is on the newest entry, 30; 28 is drawn above it.
        let (column, y) = find(&buffer, "line 28");

        click(&mut app, column, y);

        let transcript = conversation(&app).transcript();
        assert_eq!(
            transcript.selected_entry(),
            Some(&TranscriptEntryId::from("act-edit/28"))
        );
        assert!(!conversation(&app).follows());
        assert_eq!(app.focus(), Focus::Details);
    }

    #[test]
    fn a_notch_over_info_scrolls_it_and_leaves_the_focus_on_the_list() {
        let mut app = running_app();
        open_tab(&mut app, DetailTab::Info);
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.focus(), Focus::List);
        // A low screen, so the sheet is taller than its window.
        let buffer = draw(&mut app, 120, 20);
        let (column, y) = find(&buffer, "Info");

        wheel_down(&mut app, column, y + 3);

        assert_eq!(app.detail().view("a").unwrap().info().top(), 3);
        assert_eq!(app.focus(), Focus::List);
    }

    #[test]
    fn a_notch_over_the_events_moves_the_cursor_three_events_and_stops_at_the_last() {
        let mut app = running_app();
        open_tab(&mut app, DetailTab::Events);
        app.on_detail_read(
            "a",
            1,
            DetailRead::Events {
                after: 0,
                result: Ok(ExecutionEventsResponse::Available {
                    events: execution_events(8),
                    has_more: false,
                    source: ExecutionEventsSource::Runner,
                }),
            },
        );
        key(&mut app, KeyCode::Char('g'));
        let cursor = |app: &TaskApp| app.detail().view("a").unwrap().events().cursor();
        assert_eq!(cursor(&app), 0);
        let buffer = draw(&mut app, 120, 30);
        let (column, y) = find(&buffer, "state-2");

        wheel_down(&mut app, column, y);
        assert_eq!(cursor(&app), 3);

        for _ in 0..3 {
            draw(&mut app, 120, 30);
            wheel_down(&mut app, column, y);
        }
        assert_eq!(cursor(&app), 7);

        // Over the open detail of an event the notch scrolls the detail and
        // leaves the event selected.
        key(&mut app, KeyCode::Char(' '));
        assert!(app.detail().view("a").unwrap().events().is_detail_open());
        let buffer = draw(&mut app, 120, 30);
        let (column, y) = find(&buffer, "szczegóły zdarzenia");
        wheel_up(&mut app, column, y + 1);
        assert_eq!(cursor(&app), 7);
    }
}
