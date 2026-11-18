//! The queues view of the TUI (U12; R21, R23, R31–R34, AE10): what `O`
//! opens, what the list and a queue show, the forms and the confirmations,
//! and the mark of a task whose wait episode is on a queue.
//!
//! Nothing here starts a server: a read is handed to the application as the
//! loop would hand it over (`open_queues`, `show_queue`, `open_approve`),
//! and the answer of a write through `on_queue_written`. Frames are drawn at
//! the two widths the layout switches at: split (130) and full (80).

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;
use serde_json::json;

use narsor::api::{ApiClient, ClientError};
use narsor::controller_protocol::{
    ObserverMode, ObserverQueueListResponse, ObserverQueueView, ObserverScriptTextView,
    ObserverView, TaskQueueEntryView,
};
use narsor::tui::keymap::{self, Context};
use narsor::tui::queues::forms::{FormKind, ObserverForm};
use narsor::tui::queues::{EditorRequest, QueueWrite};
use narsor::tui::style::Styles;
use narsor::tui::task_app::list::QUEUE_ENTRY_MARK;
use narsor::tui::task_app::notify::Severity;
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

fn keys(app: &mut TaskApp, text: &str) {
    for character in text.chars() {
        key(app, character);
    }
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

fn observer(name: &str, queue: &str, approved: Option<u32>, latest: Option<u32>) -> ObserverView {
    serde_json::from_value(json!({
        "observer_id": format!("obs-{name}"),
        "name": name,
        "source": "gitlab",
        "state": "active",
        "effect": "none",
        "queue": queue,
        "mode": "on_episodes",
        "cron": "*/5 * * * *",
        "task_limit_per_run": 10,
        "secrets": [{"name": "gitlab-token", "env": "GITLAB_TOKEN"}],
        "approved_version": approved,
        "latest_version": latest,
        "last_run": {
            "sequence": 12,
            "status": "succeeded",
            "finished_at": at(),
        },
        "worker_missing": false,
        "consecutive_failures": 0,
        "created_at": at(),
    }))
    .expect("an observer view")
}

fn queues(entries: &[(&str, u32)]) -> ObserverQueueListResponse {
    serde_json::from_value(json!({
        "queues": entries
            .iter()
            .map(|(queue, count)| json!({
                "queue": queue,
                "mode": "on_episodes",
                "observers": 2,
                "entries": count,
            }))
            .collect::<Vec<_>>(),
    }))
    .expect("a queue listing")
}

fn mr_review(observers: Vec<ObserverView>) -> ObserverQueueView {
    serde_json::from_value(json!({
        "queue": "mr-review",
        "mode": "on_episodes",
        "observers": observers,
        "entries": [{
            "entry_id": "entry-1",
            "task_id": "task-1",
            "step": "review_work",
            "episode": "input-1",
            "started_at": at(),
            "params": {"mr_url": "https://gitlab.example.com/mr/7", "label": "ai-review"},
            "observers": [
                {"observer": "gitlab-mr", "running": true, "last_run_at": at()},
                {
                    "observer": "github-pr",
                    "last_error": {
                        "run_id": "obs-github-pr.4",
                        "status": "failed",
                        "finished_at": at(),
                        "exit_code": 3,
                        "error": "could not resolve host",
                    },
                },
            ],
        }],
    }))
    .expect("a queue view")
}

/// The list of `app`, open, with `mr-review` shown.
fn open_mr_review(app: &mut TaskApp) {
    assert_eq!(key(app, 'O'), Request::OpenQueues);
    app.open_queues(queues(&[("jira-intake", 0), ("mr-review", 1)]));
    app.show_queue(mr_review(vec![
        observer("github-pr", "mr-review", Some(1), Some(1)),
        observer("gitlab-mr", "mr-review", Some(2), Some(3)),
    ]));
}

#[test]
fn o_opens_the_queues_and_the_keys_table_stays_consistent() {
    let mut app = app();
    assert_eq!(key(&mut app, 'O'), Request::OpenQueues);
    assert!(keymap::check_consistency(keymap::BINDINGS).is_ok());

    app.open_queues(queues(&[("mr-review", 1)]));
    assert_eq!(app.view_kind(), ViewKind::Queues);
    assert_eq!(app.key_context().base, Context::Queues);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.view_kind(), ViewKind::Tasks);
}

#[test]
fn the_queue_list_shows_every_queue_with_its_entry_count() {
    for size in [WIDE, NARROW] {
        let mut app = app();
        key(&mut app, 'O');
        app.open_queues(queues(&[
            ("jira-intake", 0),
            ("mr-review", 1),
            ("triage", 4),
        ]));
        let screen = draw_app(&mut app, size);
        assert_shows(
            &screen,
            &[
                "Queues (3)",
                "jira-intake · 0 entries",
                "mr-review · 1 entry",
                "triage · 4 entries",
                "mode: on_episodes",
            ],
        );
    }
}

#[test]
fn no_queue_shows_the_empty_state_with_the_cli_hint() {
    for size in [WIDE, NARROW] {
        let mut app = app();
        key(&mut app, 'O');
        app.open_queues(queues(&[]));
        let screen = draw_app(&mut app, size);
        assert_shows(
            &screen,
            &[
                "Queues (0)",
                "No observer queues yet.",
                "narsor-cli observer create --help",
            ],
        );
    }
}

#[test]
fn a_queue_shows_its_observers_and_its_entries() {
    for size in [WIDE, NARROW] {
        let mut app = app();
        open_mr_review(&mut app);
        assert_eq!(app.key_context().base, Context::QueueDetail);
        // The second observer, the one with the newer version.
        press(&mut app, KeyCode::Down);
        let screen = draw_app(&mut app, size);
        assert_shows(
            &screen,
            &[
                "Queue mr-review",
                "github-pr · active",
                "gitlab-mr · active",
                "approved version: 2 · newest version: 3",
                "cron */5 * * * * (UTC)",
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
fn the_settings_the_cli_sets_are_read_only_with_a_hint() {
    let mut app = app();
    open_mr_review(&mut app);
    let screen = draw_app(&mut app, (130, 40));
    assert_shows(
        &screen,
        &[
            "read only (set in the CLI):",
            "secrets: gitlab-token → GITLAB_TOKEN",
            "task limit per run: 10",
        ],
    );
}

/// AE10: version 3 is saved, version 2 runs; the view marks it.
#[test]
fn an_observer_whose_newest_version_is_not_approved_is_marked() {
    for size in [WIDE, NARROW] {
        let mut app = app();
        open_mr_review(&mut app);
        press(&mut app, KeyCode::Down);
        let screen = draw_app(&mut app, size);
        assert_shows(&screen, &["✱ gitlab-mr", "version 3 not approved"]);
        assert!(
            !screen.contains("✱ github-pr"),
            "an approved newest version has no mark:\n{screen}"
        );
    }
}

#[tokio::test]
async fn approving_shows_the_script_text_first_and_esc_goes_back() {
    let mut app = app();
    open_mr_review(&mut app);
    press(&mut app, KeyCode::Down);
    // `v` reads the newest version; its answer is handed over here.
    assert_eq!(key(&mut app, 'v'), Request::None);
    app.open_approve(
        "gitlab-mr",
        Some(2),
        script(3, "#!/bin/sh\necho checking the MR\n"),
    );
    assert_eq!(app.key_context().base, Context::QueueApprove);
    let screen = draw_app(&mut app, WIDE);
    assert_shows(
        &screen,
        &[
            "Approve gitlab-mr version 3",
            "echo checking the MR",
            "y approves version 3",
        ],
    );
    let bar: Vec<String> = keymap::hints(app.key_context())
        .into_iter()
        .map(|(_, label)| label.to_string())
        .collect();
    assert!(bar.contains(&"approve".to_string()), "{bar:?}");
    assert!(!bar.contains(&"delete".to_string()), "{bar:?}");

    // Other keys leave it open; `Esc` goes back without a request.
    assert_eq!(key(&mut app, 'd'), Request::None);
    assert_eq!(app.key_context().base, Context::QueueApprove);
    assert_eq!(press(&mut app, KeyCode::Esc), Request::None);
    assert_eq!(app.key_context().base, Context::QueueDetail);

    app.open_approve("gitlab-mr", Some(2), script(3, "echo v3\n"));
    assert_eq!(
        key(&mut app, 'y'),
        Request::QueueWrite(Box::new(QueueWrite::Approve {
            observer: "gitlab-mr".to_string(),
            version: 3,
        }))
    );
}

fn script(version: u32, text: &str) -> ObserverScriptTextView {
    ObserverScriptTextView {
        version,
        created_at: at(),
        sha256: "sha256:abc".to_string(),
        script: text.to_string(),
    }
}

/// R33: delete names the observer in the `y` / `n` window; `Esc` cancels.
#[test]
fn delete_asks_a_confirmation_that_names_the_observer_and_esc_cancels() {
    let mut app = app();
    open_mr_review(&mut app);
    press(&mut app, KeyCode::Down);
    assert_eq!(key(&mut app, 'd'), Request::None);
    let screen = draw_app(&mut app, WIDE);
    assert_shows(
        &screen,
        &[
            "Delete observer gitlab-mr !",
            "Deletes observer gitlab-mr with its scripts and run errors.",
            "cannot be undone",
        ],
    );
    assert_eq!(app.key_context().base, Context::Confirm);

    assert_eq!(press(&mut app, KeyCode::Esc), Request::None);
    assert_eq!(app.key_context().base, Context::QueueDetail);
    assert!(!draw_app(&mut app, WIDE).contains("Delete observer"));

    key(&mut app, 'd');
    assert_eq!(
        key(&mut app, 'y'),
        Request::QueueWrite(Box::new(QueueWrite::Delete("gitlab-mr".to_string())))
    );
}

fn create_request(app: &mut TaskApp) -> Request {
    key(app, 'a');
    // name, source, queue (filled from the queue), mode, schedule, script
    keys(app, "gitlab-mr");
    press(app, KeyCode::Enter);
    keys(app, "gitlab");
    press(app, KeyCode::Enter);
    press(app, KeyCode::Enter);
    press(app, KeyCode::Enter);
    keys(app, "* * * * * *");
    press(app, KeyCode::Enter);
    press(app, KeyCode::Enter);
    // The form confirms before it sends.
    press(app, KeyCode::Enter)
}

#[test]
fn a_create_form_sends_what_was_typed_with_the_queue_of_the_open_queue() {
    let mut app = app();
    open_mr_review(&mut app);
    let Request::QueueWrite(write) = create_request(&mut app) else {
        panic!("the form sends a create");
    };
    let QueueWrite::Create { request, script } = *write else {
        panic!("a create");
    };
    assert_eq!(request.name, "gitlab-mr");
    assert_eq!(request.source, "gitlab");
    assert_eq!(request.queue.as_deref(), Some("mr-review"));
    assert_eq!(request.mode, Some(ObserverMode::OnEpisodes));
    assert_eq!(request.cron.as_deref(), Some("* * * * * *"));
    assert_eq!(request.interval_secs, None);
    assert_eq!(script, None);
}

/// AE9 through the view: the server refuses the schedule; the reason is
/// shown next to it and what was typed stays.
#[test]
fn a_refused_create_shows_the_reason_and_keeps_the_values() {
    for size in [WIDE, NARROW] {
        let mut app = app();
        open_mr_review(&mut app);
        let Request::QueueWrite(write) = create_request(&mut app) else {
            panic!("the form sends a create");
        };
        app.on_queue_written(
            &write,
            Err(ClientError::Request {
                code: "invalid_observer".to_string(),
                message: "the cron expression has a seconds field".to_string(),
            }),
        );
        let form = app
            .queues()
            .and_then(|queues| queues.form())
            .expect("still open");
        assert_eq!(form.kind(), &FormKind::Create);
        let values: Vec<(String, String)> = form
            .chooser()
            .fields_of(0)
            .iter()
            .map(|field| (field.name.clone(), field.value().to_string()))
            .collect();
        assert!(values.contains(&("name".to_string(), "gitlab-mr".to_string())));
        assert!(values.contains(&("schedule".to_string(), "* * * * * *".to_string())));
        let schedule = form
            .chooser()
            .fields_of(0)
            .iter()
            .find(|field| field.name == "schedule")
            .unwrap();
        assert_eq!(
            schedule.error(),
            Some("the cron expression has a seconds field")
        );
        let screen = draw_app(&mut app, size);
        assert_shows(&screen, &["New observer", "seconds field"]);
    }
}

#[tokio::test]
async fn a_create_that_worked_closes_the_form() {
    let mut app = app();
    open_mr_review(&mut app);
    let Request::QueueWrite(write) = create_request(&mut app) else {
        panic!("the form sends a create");
    };
    app.on_queue_written(&write, Ok((Severity::Success, "created".to_string())));
    assert!(app.queues().unwrap().form().is_none());
    assert_eq!(app.key_context().base, Context::QueueDetail);
}

#[test]
fn esc_leaves_a_form_without_sending() {
    let mut app = app();
    open_mr_review(&mut app);
    key(&mut app, 'a');
    keys(&mut app, "draft");
    assert_eq!(press(&mut app, KeyCode::Esc), Request::None);
    assert!(app.queues().unwrap().form().is_none());
    assert_eq!(app.key_context().base, Context::QueueDetail);
}

#[test]
fn the_edit_form_sends_only_what_changed() {
    let mut app = app();
    open_mr_review(&mut app);
    key(&mut app, 'e');
    // queue → schedule: keep the queue and the mode, change the schedule.
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Enter);
    for _ in 0.."*/5 * * * *".len() {
        press(&mut app, KeyCode::Backspace);
    }
    keys(&mut app, "300");
    press(&mut app, KeyCode::Enter);
    let request = press(&mut app, KeyCode::Enter);
    let Request::QueueWrite(write) = request else {
        panic!("the form sends an update: {request:?}");
    };
    let QueueWrite::Update { observer, request } = *write else {
        panic!("an update");
    };
    assert_eq!(observer, "github-pr");
    assert_eq!(request.queue, None);
    assert_eq!(request.mode, None);
    assert_eq!(request.interval_secs, Some(300));
    assert_eq!(request.cron, None);
}

#[tokio::test]
async fn without_an_editor_the_script_is_pasted_and_a_save_never_approves() {
    let mut app = app();
    open_mr_review(&mut app);
    // An observer with no script opens the paste field at once.
    app.show_queue(mr_review(vec![observer("fresh", "mr-review", None, None)]));
    key(&mut app, 's');
    let form = app.queues().unwrap().form().expect("the paste field");
    assert!(matches!(form.kind(), FormKind::Script { observer } if observer == "fresh"));
    app.on_paste("#!/bin/sh\necho hi\n");
    press(&mut app, KeyCode::Enter);
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        Request::QueueWrite(Box::new(QueueWrite::SaveScript {
            observer: "fresh".to_string(),
            script: "#!/bin/sh\necho hi\n".to_string(),
        }))
    );
    let screen = draw_app(&mut app, WIDE);
    assert!(!screen.contains("approved version: 1"), "{screen}");
}

#[test]
fn with_an_editor_the_loop_is_asked_to_open_it_and_its_text_is_saved() {
    let mut app = app();
    app.set_editor(Some("vi".to_string()));
    open_mr_review(&mut app);
    app.show_queue(mr_review(vec![observer("fresh", "mr-review", None, None)]));
    key(&mut app, 's');
    let (editor, request) = app.take_editor_request().expect("an editor to open");
    assert_eq!(editor, "vi");
    assert_eq!(
        request,
        EditorRequest {
            observer: "fresh".to_string(),
            base_version: None,
            text: String::new(),
        }
    );
    assert!(app.take_editor_request().is_none(), "asked once");

    assert_eq!(
        app.on_script_edited(request.clone(), Ok(Some("echo new\n".to_string()))),
        Request::QueueWrite(Box::new(QueueWrite::SaveScript {
            observer: "fresh".to_string(),
            script: "echo new\n".to_string(),
        }))
    );
    assert_eq!(
        app.on_script_edited(request.clone(), Ok(None)),
        Request::None
    );
    // An editor that does not start falls back to the paste field.
    assert_eq!(
        app.on_script_edited(request, Err(std::io::Error::other("no such editor"))),
        Request::None
    );
    assert!(app.queues().unwrap().form().is_some());
}

#[test]
fn a_refusal_lands_on_the_field_it_is_about() {
    let form = ObserverForm::create(None, None);
    assert_eq!(
        form.field_of_refusal("observer_name_taken", "taken"),
        "name"
    );
    assert_eq!(
        form.field_of_refusal("queue_mode_mismatch", "queue mr-review has mode always"),
        "mode"
    );
    assert_eq!(
        form.field_of_refusal("invalid_observer", "the interval is below 60 s"),
        "schedule"
    );
    assert_eq!(form.field_of_refusal("other", "something else"), "name");
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
