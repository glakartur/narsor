//! The queues view of the TUI (U12, U9; R21, R23): what `O` opens, what the
//! list and a queue show, that the view is read only, and the mark of a
//! task whose wait episode is on a queue.
//!
//! Nothing here starts a server: a read is handed to the application as the
//! loop would hand it over (`open_queues`, `show_queue`). Frames are drawn
//! at the two widths the layout switches at: split (130) and full (80).

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;
use serde_json::json;

use narsor::api::ApiClient;
use narsor::controller_protocol::{
    ObserverQueueListResponse, ObserverQueueView, TaskQueueEntryView,
};
use narsor::tui::keymap::{self, Action, Context};
use narsor::tui::style::Styles;
use narsor::tui::task_app::list::QUEUE_ENTRY_MARK;
use narsor::tui::task_app::{layout, Request, TaskApp, ViewKind};

use crate::task_app_support::{at, listing, queued, titled};

/// The two layouts: list and details side by side, and one at a time.
const WIDE: (u16, u16) = (130, 30);
const NARROW: (u16, u16) = (80, 24);

fn press(app: &mut TaskApp, code: KeyCode) -> Request {
    app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn key(app: &mut TaskApp, character: char) -> Request {
    press(app, KeyCode::Char(character))
}

fn app() -> TaskApp {
    let mut app = TaskApp::new(
        ApiClient::new("127.0.0.1:9".parse().unwrap(), "token"),
        "local",
        None,
    );
    app.load(listing(
        vec![
            titled(queued("a", 1), "NAR-14"),
            titled(queued("b", 2), "NAR-12"),
        ],
        "c",
    ));
    app
}

fn draw_app(app: &mut TaskApp, (width, height): (u16, u16)) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("a test terminal");
    terminal
        .draw(|frame| layout::render(frame, app, &Styles::new(true), Instant::now()))
        .expect("draw");
    screen(terminal.backend().buffer())
}

fn screen(buffer: &Buffer) -> String {
    let width = usize::from(buffer.area.width);
    buffer
        .content()
        .chunks(width)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_shows(screen: &str, expected: &[&str]) {
    for text in expected {
        assert!(screen.contains(text), "{text:?} is not shown:\n{screen}");
    }
}

fn listing_with_problems() -> ObserverQueueListResponse {
    serde_json::from_value(json!({
        "queues": [
            {
                "queue": "mr-review",
                "mode": "on_episodes",
                "cron": "*/5 * * * *",
                "automations": ["gitlab-mr", "github-pr"],
                "entries": 1,
            },
            {
                "queue": "triage",
                "automations": [],
                "entries": 4,
                "problem": "queues/triage.yaml: unknown field `mod`",
            },
        ],
        "problems": [
            {"file": "queues/triage.yaml", "reason": "unknown field `mod`"},
            {"file": "automations/broken", "reason": "no check script"},
        ],
    }))
    .expect("a queue listing")
}

fn mr_review() -> ObserverQueueView {
    serde_json::from_value(json!({
        "queue": "mr-review",
        "mode": "on_episodes",
        "cron": "*/5 * * * *",
        "automations": [
            {
                "automation": "gitlab-mr",
                "description": "Reviews GitLab merge requests",
                "scripts": ["in", "check"],
                "secrets": [{"name": "gitlab-token", "env": "GITLAB_TOKEN"}],
                "state": "active",
                "last_run": {"sequence": 12, "status": "succeeded", "finished_at": at()},
            },
            {
                "automation": "github-pr",
                "scripts": ["check"],
                "state": "error",
                "worker_missing": true,
                "consecutive_failures": 5,
                "errors": [{
                    "run_id": "mr-review~github-pr~4",
                    "sequence": 4,
                    "phase": "check",
                    "status": "failed",
                    "claimed_at": at(),
                    "finished_at": at(),
                    "exit_code": 3,
                    "error": "could not resolve host",
                }],
            },
        ],
        "entries": [{
            "entry_id": "entry-1",
            "task_id": "task-1",
            "step": "review_work",
            "episode": "input-1",
            "started_at": at(),
            "params": {"mr_url": "https://gitlab.example.com/mr/7", "label": "ai-review"},
            "automations": [
                {"automation": "gitlab-mr", "running": true, "last_run_at": at()},
                {
                    "automation": "github-pr",
                    "last_error": {
                        "run_id": "mr-review~github-pr~4",
                        "status": "failed",
                        "finished_at": at(),
                        "exit_code": 3,
                        "error": "could not resolve host",
                    },
                },
            ],
        }],
        "problems": [
            {"file": "queues/mr-review.yaml", "reason": "automation `gone` is not in the catalogue"},
        ],
    }))
    .expect("a queue view")
}

/// The list of `app`, open, with `mr-review` shown.
fn open_mr_review(app: &mut TaskApp) {
    assert_eq!(key(app, 'O'), Request::OpenQueues);
    app.open_queues(listing_with_problems());
    app.show_queue(mr_review());
}

#[test]
fn o_opens_the_queues_and_the_keys_table_stays_consistent() {
    let mut app = app();
    assert_eq!(key(&mut app, 'O'), Request::OpenQueues);
    assert!(keymap::check_consistency(keymap::BINDINGS).is_ok());

    app.open_queues(listing_with_problems());
    assert_eq!(app.view_kind(), ViewKind::Queues);
    assert_eq!(app.key_context().base, Context::Queues);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.view_kind(), ViewKind::Tasks);
}

#[test]
fn the_queue_list_shows_each_queue_its_schedule_and_the_files_that_failed_to_load() {
    for size in [WIDE, NARROW] {
        let mut app = app();
        key(&mut app, 'O');
        app.open_queues(listing_with_problems());
        let screen = draw_app(&mut app, size);
        assert_shows(
            &screen,
            &[
                "Queues (2) · 2 files not loaded",
                "mr-review · 1 entry",
                "! triage · 4 entries",
                "mode: on_episodes",
                "automations: gitlab-mr, github-pr",
                "Catalogue files not loaded (2)",
                "! automations/broken: no check script",
            ],
        );
        assert!(
            !screen.contains("! mr-review"),
            "a queue that runs has no mark:\n{screen}"
        );
    }
}

#[test]
fn a_queue_that_does_not_run_says_why_in_its_description() {
    let mut app = app();
    key(&mut app, 'O');
    app.open_queues(listing_with_problems());
    press(&mut app, KeyCode::Down);
    let screen = draw_app(&mut app, (130, 40));
    assert_shows(
        &screen,
        &["! does not run: queues/triage.yaml", "mode: none"],
    );
}

#[test]
fn no_queue_shows_the_empty_state_with_the_catalogue_and_cli_hint() {
    for size in [WIDE, NARROW] {
        let mut app = app();
        key(&mut app, 'O');
        app.open_queues(ObserverQueueListResponse::default());
        let screen = draw_app(&mut app, size);
        assert_shows(
            &screen,
            &[
                "Queues (0)",
                "No queues.",
                "queues/NAME.yaml",
                "narsor-cli queue list",
            ],
        );
    }
}

#[test]
fn a_queue_shows_its_automations_its_entries_and_its_problems() {
    for size in [WIDE, NARROW] {
        let mut app = app();
        open_mr_review(&mut app);
        assert_eq!(app.key_context().base, Context::QueueDetail);
        let screen = draw_app(&mut app, size);
        assert_shows(
            &screen,
            &[
                "Queue mr-review",
                "gitlab-mr · active",
                "! github-pr · error",
                "! queues/mr-review.yaml: automation `gone`",
                "Entries (1)",
                "task-1 · review_work · episode input-1",
                "mr_url=https://gitlab.example.com/mr/7",
                "gitlab-mr: running · last run",
                "github-pr: no run yet · newest error: failed (exit 3)",
            ],
        );
    }
}

#[test]
fn an_automation_description_shows_its_runtime_state_and_read_only_settings() {
    let mut app = app();
    open_mr_review(&mut app);
    let screen = draw_app(&mut app, (130, 50));
    assert_shows(
        &screen,
        &[
            "scripts: in, check",
            "last run: #12 succeeded",
            "secrets: gitlab-token → GITLAB_TOKEN",
        ],
    );
    // The stopped one: why it stopped, how it starts again, its errors.
    press(&mut app, KeyCode::Down);
    let screen = draw_app(&mut app, (130, 50));
    assert_shows(
        &screen,
        &[
            "stopped: it runs again when its check script changes",
            "failed runs in a row: 5 (backoff)",
            "a due run waits for a worker",
            "#4 check: failed, exit 3",
        ],
    );
}

/// U9: queues and automations are catalogue files; the view writes nothing.
#[test]
fn the_queue_view_offers_no_create_edit_approve_pause_or_delete() {
    let navigation = [
        Action::Back,
        Action::Open,
        Action::Down,
        Action::Up,
        Action::Pick,
        Action::PageDown,
        Action::PageUp,
    ];
    for binding in keymap::BINDINGS
        .iter()
        .filter(|binding| matches!(binding.context, Context::Queues | Context::QueueDetail))
    {
        assert!(
            navigation.contains(&binding.action),
            "a queue key that is not navigation: {binding:?}"
        );
    }

    let mut app = app();
    open_mr_review(&mut app);
    for character in ['a', 'e', 's', 'v', 'y', 'p', 'r', 'd'] {
        assert_eq!(key(&mut app, character), Request::None, "{character}");
        // No form, approval screen or confirmation window opened.
        assert_eq!(app.key_context().base, Context::QueueDetail, "{character}");
    }
    let shown = app.queues().and_then(|queues| queues.detail());
    assert_eq!(shown.map(|detail| detail.view().automations.len()), Some(2));
}

/// R21: a task whose current wait episode is on a queue shows one mark.
#[test]
fn a_task_with_a_queue_entry_shows_one_mark_on_its_row() {
    for size in [WIDE, NARROW] {
        let mut app = TaskApp::new(
            ApiClient::new("127.0.0.1:9".parse().unwrap(), "token"),
            "local",
            None,
        );
        let mut waiting = titled(queued("a", 1), "NAR-14");
        waiting.queue_entry = Some(TaskQueueEntryView {
            queue: "mr-review".to_string(),
            step: "review_work".to_string(),
            episode: "input-1".to_string(),
            since: at(),
        });
        app.load(listing(
            vec![waiting, titled(queued("b", 2), "NAR-12")],
            "c",
        ));
        assert_eq!(
            app.list().row_text(0).queue_entry.as_deref(),
            Some("mr-review")
        );
        assert_eq!(app.list().row_text(1).queue_entry, None);
        let screen = draw_app(&mut app, size);
        let marked: Vec<&str> = screen
            .lines()
            .filter(|line| line.contains(QUEUE_ENTRY_MARK))
            .collect();
        assert_eq!(marked.len(), 1, "{screen}");
        let id = app.list().row_text(0).id;
        assert!(marked[0].contains(&id), "{id}:\n{screen}");
    }
}
