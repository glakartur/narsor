//! The panel of the conversation: the forms the server offers, `Esc` to step
//! through them, the drafts that stay, the free message to the agent.
//!
//! Tasks are built by hand with the offer of forms the server would put on
//! them (`with_panel`), and the application takes them with `TaskApp::load`.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use serde_json::{json, Value};

use narsor::api::ApiClient;
use narsor::tui::style::Styles;
use narsor::tui::task_app::waiting::Submit;
use narsor::tui::task_app::{layout, Request, TaskApp};
use narsor_engine::controller_protocol::{PanelFormKind, TaskSummary};
use narsor_engine::domain::ExecutionStatus;

mod task_app_support;

use task_app_support::{
    listing, task, titled, with_controller, with_panel, with_pending, with_runner,
};

fn app() -> TaskApp {
    TaskApp::new(
        ApiClient::new("127.0.0.1:9".parse().unwrap(), "token"),
        "local",
        None,
    )
}

fn key(app: &mut TaskApp, code: KeyCode) -> Request {
    app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn typed(app: &mut TaskApp, text: &str) {
    for character in text.chars() {
        key(app, KeyCode::Char(character));
    }
}

fn running() -> TaskSummary {
    with_runner(
        with_controller(task("a", "local", "running", 1)),
        ExecutionStatus::InputNeeded,
        "review_scope",
    )
}

fn question() -> Value {
    json!({
        "execution_id": "exec-1",
        "state_id": "review_scope",
        "action_execution_id": "act-7",
        "instruction": "Która wersja API?",
        "fields": [{ "name": "answer", "type": "text", "required": true }],
        "continuation": "resume_agent",
        "episode": "ep-q",
        "requested_at": "2026-10-09T11:54:00Z"
    })
}

fn asking() -> TaskSummary {
    with_panel(with_pending(running(), question()))
}

/// An app whose only task is `task`, with the panel open on it.
fn opened(task: TaskSummary) -> TaskApp {
    let mut app = app();
    app.load(listing(vec![titled(task, "NAR-12")], "c"));
    key(&mut app, KeyCode::Char(' '));
    key(&mut app, KeyCode::Enter);
    app
}

fn shown(app: &TaskApp) -> Option<PanelFormKind> {
    app.detail().view("a")?.waiting().active_form()
}

fn screen(app: &mut TaskApp, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| layout::render(frame, app, &Styles::new(true), Instant::now()))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    buffer
        .content()
        .chunks(usize::from(width))
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn an_agents_question_opens_the_answer_and_esc_goes_to_the_chat_and_back() {
    let mut app = opened(asking());
    assert!(app.panel_focused());
    assert_eq!(shown(&app), Some(PanelFormKind::Answer));

    key(&mut app, KeyCode::Esc);
    assert_eq!(shown(&app), Some(PanelFormKind::Chat));
    assert!(app.panel_focused(), "Esc does not leave the panel");

    key(&mut app, KeyCode::Esc);
    assert_eq!(shown(&app), Some(PanelFormKind::Answer));
}

#[test]
fn the_header_says_which_form_of_how_many_and_the_key() {
    let mut app = opened(asking());
    let first = screen(&mut app, 120, 40);
    assert!(first.contains("1/2 · Esc przełącza"), "{first}");
    key(&mut app, KeyCode::Esc);
    let second = screen(&mut app, 120, 40);
    assert!(second.contains("2/2 · Esc przełącza"), "{second}");
    assert!(second.contains("Rozmowa z agentem"), "{second}");
}

#[test]
fn esc_in_a_text_field_switches_and_the_draft_stays() {
    let mut app = opened(asking());
    key(&mut app, KeyCode::Esc);
    assert_eq!(shown(&app), Some(PanelFormKind::Chat));
    typed(&mut app, "pisze");
    assert!(app.detail().view("a").unwrap().waiting().is_typing());

    key(&mut app, KeyCode::Esc);
    assert_eq!(shown(&app), Some(PanelFormKind::Answer));
    assert_eq!(
        app.detail().view("a").unwrap().waiting().chat_draft(),
        "pisze"
    );

    key(&mut app, KeyCode::Esc);
    assert_eq!(shown(&app), Some(PanelFormKind::Chat));
    assert_eq!(
        app.detail().view("a").unwrap().waiting().chat_draft(),
        "pisze"
    );
}

#[test]
fn a_message_in_the_chat_is_sent_as_a_comment_after_the_confirmation() {
    let mut app = opened(asking());
    key(&mut app, KeyCode::Esc);
    typed(&mut app, "sprawdź logi");
    assert_eq!(key(&mut app, KeyCode::Enter), Request::None);
    assert!(app.form_confirming(), "Enter asks before it sends");
    let Request::Answer(request) = key(&mut app, KeyCode::Enter) else {
        panic!("the confirmed message is sent");
    };
    assert_eq!(request.submit, Submit::Comment("sprawdź logi".to_string()));
}

#[test]
fn a_new_default_takes_the_view_and_the_chat_draft_stays() {
    // The operator writes in the chat of a paused task; the agent then asks.
    let paused = with_panel(with_runner(
        with_controller(task("a", "local", "stopped", 1)),
        ExecutionStatus::Running,
        "review_scope",
    ));
    let mut app = opened(paused);
    assert_eq!(shown(&app), Some(PanelFormKind::Chat));
    typed(&mut app, "szkic");

    app.load(listing(vec![titled(asking(), "NAR-12")], "d"));
    assert_eq!(shown(&app), Some(PanelFormKind::Answer));
    key(&mut app, KeyCode::Esc);
    assert_eq!(shown(&app), Some(PanelFormKind::Chat));
    assert_eq!(
        app.detail().view("a").unwrap().waiting().chat_draft(),
        "szkic"
    );
}

#[test]
fn a_working_agent_has_no_panel() {
    let working = with_panel(with_runner(
        with_controller(task("a", "local", "running", 1)),
        ExecutionStatus::Running,
        "review_scope",
    ));
    let mut app = app();
    app.load(listing(vec![titled(working, "NAR-12")], "c"));
    key(&mut app, KeyCode::Char(' '));
    key(&mut app, KeyCode::Enter);
    assert!(!app.panel_focused());
    assert!(!app
        .detail()
        .view("a")
        .is_some_and(|view| view.waiting().is_present()));
}

#[test]
fn with_one_form_esc_changes_nothing_and_the_panel_stays() {
    let paused = with_panel(with_runner(
        with_controller(task("a", "local", "stopped", 1)),
        ExecutionStatus::Running,
        "review_scope",
    ));
    let mut app = opened(paused);
    assert_eq!(app.detail().view("a").unwrap().waiting().form_count(), 1);
    key(&mut app, KeyCode::Esc);
    assert_eq!(shown(&app), Some(PanelFormKind::Chat));
    assert!(app.panel_focused());
    assert!(!app.detail().view("a").unwrap().waiting().is_collapsed());
}

#[test]
fn tab_leaves_the_panel_and_it_stays_when_there_is_a_form() {
    let mut app = opened(asking());
    key(&mut app, KeyCode::Tab);
    assert!(!app.panel_focused());
    assert!(app.detail().view("a").unwrap().waiting().is_present());
    assert!(!app.detail().view("a").unwrap().waiting().is_collapsed());
}

#[test]
fn a_server_that_sent_no_forms_keeps_the_old_panel() {
    let old = with_pending(running(), question());
    assert!(old.panel.is_none());
    let mut app = opened(old);
    assert_eq!(shown(&app), None);
    assert!(app.panel_focused());
    // `Esc` folds the panel as it did.
    key(&mut app, KeyCode::Esc);
    assert!(app.detail().view("a").unwrap().waiting().is_collapsed() || !app.panel_focused());
}

#[test]
fn the_chat_box_grows_with_the_text_up_to_half_the_details() {
    let paused = with_panel(with_runner(
        with_controller(task("a", "local", "stopped", 1)),
        ExecutionStatus::Running,
        "review_scope",
    ));
    let mut app = opened(paused);
    let one_line = screen(&mut app, 120, 40);
    typed(&mut app, "a");
    for _ in 0..3 {
        app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        typed(&mut app, "b");
    }
    let grown = screen(&mut app, 120, 40);
    let height = |screen: &str| {
        screen
            .lines()
            .position(|row| row.contains("Rozmowa z agentem"))
            .map(|top| screen.lines().count() - top)
            .expect("the chat heading")
    };
    assert!(height(&grown) > height(&one_line), "{one_line}\n{grown}");
    for _ in 0..30 {
        app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        typed(&mut app, "c");
    }
    let limited = screen(&mut app, 120, 40);
    assert!(
        height(&limited) <= 40 / 2 + 8,
        "the box stops at half the details:\n{limited}"
    );
}
