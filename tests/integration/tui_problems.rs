//! The problem log of the interface: every failed call and every error shown
//! is recorded with the screen and task it belongs to, and the visible
//! messages stay as they were.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use narsor::api::{ApiClient, TokenSource};
use narsor::tui::style::Styles;
use narsor::tui::task_app::list::MoveRequest;
use narsor::tui::task_app::notify::{Notifier, Severity, LIFETIME};
use narsor::tui::task_app::problems::{Problem, Source};
use narsor::tui::task_app::{layout, DetailTab, Request, TaskApp};
use narsor_engine::orchestration::{RunnerState, TaskEventKind};
use narsor_protocol::controller::MoveTaskTarget;

use crate::task_app_support::{event, interrupted, listing, task, titled};

/// A server on a free loopback port: the timeline of `t1` never comes,
/// moving `t1` works, and every other path is a `404`.
async fn stub() -> SocketAddr {
    let moved = serde_json::to_value(task("t1", "local", "queued", 1)).unwrap();
    let app = axum::Router::new()
        .route(
            "/control/v1/tasks/t1/timeline",
            axum::routing::get(|| std::future::pending::<&'static str>()),
        )
        .route(
            "/control/v1/tasks/t1/move",
            axum::routing::post(move || async move { axum::Json(moved) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    address
}

/// A loopback address where nothing listens.
async fn nothing_listening() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    address
}

fn app_over(address: SocketAddr) -> TaskApp {
    let mut app = TaskApp::new(ApiClient::new(address, "token"), "local", None);
    app.load(listing(vec![task("t1", "local", "running", 1)], "c1"));
    app
}

fn problems(app: &TaskApp) -> Vec<Problem> {
    app.problems().snapshot(Instant::now()).entries
}

fn of(problems: &[Problem], source: Source) -> Vec<Problem> {
    problems
        .iter()
        .filter(|problem| problem.source == source)
        .cloned()
        .collect()
}

fn move_t1() -> Request {
    Request::Move(MoveRequest {
        task_id: "t1".to_string(),
        title: "NAR-1".to_string(),
        generation: 1,
        target: MoveTaskTarget::Top,
        anchor_title: String::new(),
    })
}

#[tokio::test]
async fn a_timeline_read_that_times_out_is_recorded_with_its_tab_task_and_cause() {
    let mut app = app_over(stub().await);
    app.detail_mut().set_tab(DetailTab::Progress);

    app.sync_detail().await;

    // The tab says what it always said.
    let view = app.detail().view("t1").expect("the details of t1");
    assert_eq!(
        view.progress().failure(),
        Some("brak połączenia z serwerem")
    );

    let entries = problems(&app);
    let on_progress: Vec<_> = entries
        .iter()
        .filter(|problem| problem.label == "Przebieg — task t1")
        .cloned()
        .collect();
    let calls = of(&on_progress, Source::CallFailed);
    assert_eq!(calls.len(), 1, "{entries:#?}");
    let call = &calls[0];
    assert_eq!(call.kind, "unavailable");
    assert_eq!(
        call.call.as_deref(),
        Some("GET /control/v1/tasks/t1/timeline")
    );
    assert!(call.detail.contains("timed out"), "{}", call.detail);
    let took = call.duration.expect("a duration");
    assert!(
        took >= Duration::from_millis(800) && took < Duration::from_secs(5),
        "{took:?}"
    );
    let shown = of(&on_progress, Source::Shown);
    assert_eq!(shown.len(), 1, "{entries:#?}");
    assert_eq!(shown[0].detail, "brak połączenia z serwerem");
}

#[tokio::test]
async fn a_failed_action_is_recorded_under_the_action_and_its_notification_too() {
    let mut app = app_over(nothing_listening().await);

    app.dispatch(move_t1()).await;

    let entries = problems(&app);
    let calls = of(&entries, Source::CallFailed);
    assert_eq!(calls.len(), 1, "{entries:#?}");
    assert_eq!(calls[0].label, "move task t1");
    let shown = of(&entries, Source::Shown);
    assert_eq!(shown.len(), 1, "{entries:#?}");
    let note = app.notifier().latest().expect("a notification");
    assert_eq!(shown[0].detail, note.text);
    assert_eq!(shown[0].kind, "error");
}

#[tokio::test]
async fn an_action_that_works_records_nothing() {
    let mut app = app_over(stub().await);

    app.dispatch(move_t1()).await;

    assert!(app.notifier().latest().is_some());
    assert_eq!(problems(&app), []);
}

#[tokio::test]
async fn an_auto_resume_that_gives_up_records_its_notice() {
    let client = ApiClient::new(nothing_listening().await, "token");
    let mut app = TaskApp::new(client, "local", None);
    app.load(listing(
        vec![titled(
            interrupted("t2", RunnerState::Running, 1, true),
            "NAR-2",
        )],
        "c1",
    ));

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

    let shown = of(&problems(&app), Source::Shown);
    assert_eq!(shown.len(), 1, "{shown:#?}");
    assert_eq!(
        shown[0].detail,
        "NAR-2: nie udało się wznowić automatycznie: pod evicted"
    );
}

// ---------------------------------------------------------------------
// `D`: the dump
// ---------------------------------------------------------------------

/// A fresh directory under the system temp dir, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("narsor-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("a temp dir");
        Self(dir)
    }

    /// The files in the directory.
    fn files(&self) -> Vec<PathBuf> {
        std::fs::read_dir(&self.0)
            .expect("the temp dir")
            .map(|entry| entry.expect("an entry").path())
            .collect()
    }

    /// The one file in the directory, read.
    fn only_file(&self) -> (PathBuf, String) {
        let files = self.files();
        assert_eq!(files.len(), 1, "{files:?}");
        let text = std::fs::read_to_string(&files[0]).expect("the dump");
        (files[0].clone(), text)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn press_d(app: &mut TaskApp) -> Request {
    app.on_key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT))
}

fn bar_text(app: &TaskApp) -> String {
    app.notifier()
        .current(Instant::now())
        .expect("a notification on the bar")
        .text
        .clone()
}

fn file_name(path: &Path) -> String {
    path.file_name().unwrap().to_string_lossy().into_owned()
}

#[tokio::test]
async fn d_on_the_list_writes_a_file_with_the_header_even_when_nothing_went_wrong() {
    let dir = TempDir::new();
    let mut app = app_over(nothing_listening().await).with_dump_dir(&dir.0);

    assert_eq!(press_d(&mut app), Request::None);
    app.drain_jobs().await;

    let (path, text) = dir.only_file();
    assert!(text.starts_with("Narsor problem log\n"), "{text}");
    assert!(text.contains("timeout:      800 ms"), "{text}");
    assert!(
        text.contains("no problems recorded in the last 30 minutes"),
        "{text}"
    );
    assert!(
        bar_text(&app).ends_with(&file_name(&path)),
        "{}",
        bar_text(&app)
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[tokio::test]
async fn d_typed_into_the_list_filter_is_text() {
    let dir = TempDir::new();
    let mut app = app_over(nothing_listening().await).with_dump_dir(&dir.0);
    app.on_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));

    press_d(&mut app);
    app.drain_jobs().await;

    assert!(
        app.list().filter().ends_with('D'),
        "{}",
        app.list().filter()
    );
    assert_eq!(dir.files(), Vec::<PathBuf>::new());
}

#[tokio::test]
async fn the_dump_names_the_server_and_never_holds_the_token() {
    const TOKEN: &str = "s3cr3t-bearer-token";
    let address = stub().await;
    let client = ApiClient::remote(
        &format!("http://{address}"),
        TokenSource::Static(TOKEN.to_string()),
    )
    .expect("a loopback client");
    let dir = TempDir::new();
    let mut app = TaskApp::new(client, "remote", None).with_dump_dir(&dir.0);
    // The list is a `404` on the stub: one failed call, and what it shows.
    app.refresh().await;
    assert!(!problems(&app).is_empty());

    press_d(&mut app);
    app.drain_jobs().await;

    let (_, text) = dir.only_file();
    assert!(
        text.contains(&format!("connection:   http://{address}")),
        "{text}"
    );
    assert!(text.contains("GET /control/v1/tasks"), "{text}");
    assert!(!text.contains(TOKEN), "{text}");
}

#[tokio::test]
async fn a_dump_that_cannot_be_written_says_why_keeps_the_log_and_replaces_the_last_path() {
    let dir = TempDir::new();
    let mut app = app_over(nothing_listening().await).with_dump_dir(&dir.0);
    app.refresh().await;
    let before = problems(&app);
    assert!(!before.is_empty());
    press_d(&mut app);
    app.drain_jobs().await;
    let (written, _) = dir.only_file();
    assert!(bar_text(&app).ends_with(&file_name(&written)));

    // Within the lifetime of the path on the bar, a dump that fails.
    let missing = dir.0.join("missing");
    let mut app = app.with_dump_dir(&missing);
    press_d(&mut app);
    app.drain_jobs().await;

    let note = app
        .notifier()
        .current(Instant::now())
        .expect("a notification");
    assert_eq!(note.severity, Severity::Error);
    assert!(note
        .text
        .starts_with("nie udało się zapisać dziennika problemów: "));
    assert_eq!(problems(&app), before);

    // A later dump still has every entry.
    let mut app = app.with_dump_dir(&dir.0);
    press_d(&mut app);
    app.drain_jobs().await;
    let newest = dir
        .files()
        .into_iter()
        .find(|path| *path != written)
        .expect("a second dump");
    let text = std::fs::read_to_string(newest).unwrap();
    assert!(
        text.contains(&format!("{} entries", before.len())),
        "{text}"
    );
}

#[tokio::test]
async fn the_path_stays_on_the_bar_over_an_unrelated_error() {
    let dir = TempDir::new();
    let mut app = app_over(nothing_listening().await).with_dump_dir(&dir.0);
    press_d(&mut app);
    app.drain_jobs().await;
    let (path, _) = dir.only_file();

    // A move fails: an error notification, recorded but not on the bar.
    app.dispatch(move_t1()).await;

    assert!(
        bar_text(&app).ends_with(&file_name(&path)),
        "{}",
        bar_text(&app)
    );
    assert!(of(&problems(&app), Source::Shown)
        .iter()
        .any(|problem| problem.label == "move task t1"));
}

#[test]
fn a_held_notification_gives_way_once_its_lifetime_is_over() {
    let now = Instant::now();
    let mut notifier = Notifier::default();
    notifier.push_held(Severity::Success, "zapisany: /tmp/a.txt", now);

    notifier.push(Severity::Error, "inny błąd", now + Duration::from_secs(1));
    assert_eq!(
        notifier.current(now + Duration::from_secs(1)).unwrap().text,
        "zapisany: /tmp/a.txt"
    );

    let later = now + LIFETIME + Duration::from_secs(1);
    notifier.push(Severity::Error, "inny błąd", later);
    assert_eq!(notifier.current(later).unwrap().text, "inny błąd");
}

#[tokio::test]
async fn a_path_too_long_for_the_bar_keeps_its_file_name() {
    let dir = TempDir::new();
    let mut app = app_over(nothing_listening().await).with_dump_dir(&dir.0);
    press_d(&mut app);
    app.drain_jobs().await;
    let (path, _) = dir.only_file();
    let name = file_name(&path);

    let mut terminal = Terminal::new(TestBackend::new(60, 20)).expect("a test terminal");
    terminal
        .draw(|frame| layout::render(frame, &mut app, &Styles::new(true), Instant::now()))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let width = usize::from(buffer.area.width);
    let rows: Vec<String> = buffer
        .content()
        .chunks(width)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect())
        .collect();

    let bar = rows
        .iter()
        .find(|row| row.contains(&name))
        .unwrap_or_else(|| panic!("the file name on a row: {rows:#?}"));
    assert!(bar.trim().starts_with('…'), "{bar}");
    assert!(bar.trim_end().ends_with(&name), "{bar}");
}
