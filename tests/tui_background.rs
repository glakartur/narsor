//! Reads that run apart from the event loop: the background runner, the
//! loading marker and the views that wait for their read.
//!
//! The server is a stub that answers a path only once the test opens its
//! gate, so a test can look at the application while a read is under way.
//! (The client gives up on a request after a short time: a gate is opened
//! within it, never left closed while the test waits for the answer.)

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

use narsor::api::ApiClient;
use narsor::tui::spinner::Spinner;
use narsor::tui::style::Styles;
use narsor::tui::task_app::background::OpeningKind;
use narsor::tui::task_app::{layout, Request, TaskApp, ViewKind};

mod task_app_support;
use task_app_support::{archived, listing, task, timeline};

/// What the stub says to a path.
struct Route {
    status: u16,
    body: String,
    /// Opened by the test; a route without a gate answers at once.
    gate: Option<Arc<Semaphore>>,
    hits: AtomicUsize,
}

/// A server whose answers wait for the test.
struct Stub {
    addr: SocketAddr,
    routes: Arc<Mutex<Vec<(String, Arc<Route>)>>>,
}

impl Stub {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("address");
        let routes: Arc<Mutex<Vec<(String, Arc<Route>)>>> = Arc::default();
        let served = Arc::clone(&routes);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let routes = Arc::clone(&served);
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => request.extend_from_slice(&chunk[..read]),
                        }
                    }
                    let text = String::from_utf8_lossy(&request).into_owned();
                    let path = text
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_string();
                    let route = {
                        let routes = routes.lock().expect("routes");
                        routes
                            .iter()
                            .filter(|(prefix, _)| path.starts_with(prefix.as_str()))
                            .max_by_key(|(prefix, _)| prefix.len())
                            .map(|(_, route)| Arc::clone(route))
                    };
                    let (status, body) = match route {
                        Some(route) => {
                            route.hits.fetch_add(1, Ordering::SeqCst);
                            if let Some(gate) = &route.gate {
                                match gate.acquire().await {
                                    Ok(permit) => permit.forget(),
                                    Err(_) => return,
                                }
                            }
                            (route.status, route.body.clone())
                        }
                        None => (404, "{}".to_string()),
                    };
                    let response = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Self { addr, routes }
    }

    fn client(&self) -> ApiClient {
        ApiClient::new(self.addr, "token")
    }

    /// Answers `prefix` with `body` at once.
    fn open(&self, prefix: &str, body: impl serde::Serialize) {
        self.add(prefix, 200, body, None);
    }

    /// Answers `prefix` with `body` once the returned gate has a permit.
    fn gated(&self, prefix: &str, body: impl serde::Serialize) -> Arc<Semaphore> {
        let gate = Arc::new(Semaphore::new(0));
        self.add(prefix, 200, body, Some(Arc::clone(&gate)));
        gate
    }

    /// Refuses `prefix` with a server error, once the gate has a permit.
    fn failing(&self, prefix: &str) -> Arc<Semaphore> {
        let gate = Arc::new(Semaphore::new(0));
        self.add(
            prefix,
            500,
            serde_json::json!({"code": "boom", "message": "boom"}),
            Some(Arc::clone(&gate)),
        );
        gate
    }

    fn add(
        &self,
        prefix: &str,
        status: u16,
        body: impl serde::Serialize,
        gate: Option<Arc<Semaphore>>,
    ) {
        let route = Arc::new(Route {
            status,
            body: serde_json::to_string(&body).expect("a body"),
            gate,
            hits: AtomicUsize::new(0),
        });
        let mut routes = self.routes.lock().expect("routes");
        routes.retain(|(existing, _)| existing != prefix);
        routes.push((prefix.to_string(), route));
    }

    /// How many requests reached `prefix`.
    fn hits(&self, prefix: &str) -> usize {
        self.routes
            .lock()
            .expect("routes")
            .iter()
            .find(|(existing, _)| existing == prefix)
            .map_or(0, |(_, route)| route.hits.load(Ordering::SeqCst))
    }

    /// Waits until `prefix` has been asked `count` times.
    async fn reached(&self, prefix: &str, count: usize) {
        for _ in 0..200 {
            if self.hits(prefix) >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "{prefix} was asked {} times, not {count}",
            self.hits(prefix)
        );
    }
}

const LIST: &str = "/control/v1/tasks?archived=false";
const ARCHIVED: &str = "/control/v1/tasks?archived=true";
const HARNESS: &str = "/settings/agent-harness";

fn screen_of(app: &mut TaskApp, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("a test terminal");
    terminal
        .draw(|frame| layout::render(frame, app, &Styles::new(true), Instant::now()))
        .expect("draw");
    let buffer: &Buffer = terminal.backend().buffer();
    buffer
        .content()
        .chunks(usize::from(width))
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn press(app: &mut TaskApp, code: KeyCode) -> Request {
    app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn app_over(stub: &Stub) -> TaskApp {
    let mut app = TaskApp::new(stub.client(), "local", None);
    app.set_spinner(Spinner::braille());
    app
}

fn two_tasks() -> serde_json::Value {
    serde_json::to_value(listing(
        vec![
            task("t1", "local", "running", 1),
            task("t2", "local", "running", 2),
        ],
        "c1",
    ))
    .expect("a listing")
}

#[tokio::test]
async fn the_list_shows_the_marker_while_it_is_read_and_keys_work_meanwhile() {
    let stub = Stub::start().await;
    let gate = stub.gated(LIST, two_tasks());
    let mut app = app_over(&stub);

    app.start_refresh();
    stub.reached(LIST, 1).await;
    assert!(app.is_refreshing() && app.has_running());

    // AE1: the marker is in the place of the data, and it moves.
    app.set_animation_frame(0);
    let first = screen_of(&mut app, 100, 30);
    assert!(first.contains("⠋ wczytuję zadania…"), "{first}");
    app.set_animation_frame(1);
    let second = screen_of(&mut app, 100, 30);
    assert!(second.contains("⠙ wczytuję zadania…"), "{second}");

    // A key is handled before the answer (R1, R11).
    press(&mut app, KeyCode::Char('q'));
    press(&mut app, KeyCode::Char('q'));
    assert!(app.should_quit());

    gate.add_permits(1);
    app.drain_jobs().await;
    assert!(!app.is_refreshing());
    assert_eq!(app.list().tasks().len(), 2);
    let after = screen_of(&mut app, 100, 30);
    assert!(!after.contains("wczytuję"), "{after}");
}

#[tokio::test]
async fn a_second_start_while_a_read_runs_sends_no_second_request() {
    let stub = Stub::start().await;
    let gate = stub.gated(LIST, two_tasks());
    let mut app = app_over(&stub);

    app.start_refresh();
    app.start_refresh();
    assert!(!app.start_refresh_if_due(Instant::now() + Duration::from_secs(3600)));
    stub.reached(LIST, 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(stub.hits(LIST), 1);

    gate.add_permits(1);
    app.drain_jobs().await;
    // Done: the key is free again.
    app.start_refresh();
    gate.add_permits(1);
    app.drain_jobs().await;
    assert_eq!(stub.hits(LIST), 2);
}

#[tokio::test]
async fn a_failed_read_takes_the_marker_away_and_may_be_started_again() {
    let stub = Stub::start().await;
    let gate = stub.failing(LIST);
    let mut app = app_over(&stub);

    app.start_refresh();
    gate.add_permits(1);
    app.drain_jobs().await;

    // AE3: no marker, the text the interface had, and `r` may start a read.
    assert!(!app.is_refreshing());
    let screen = screen_of(&mut app, 100, 30);
    assert!(!screen.contains("wczytuję"), "{screen}");
    assert!(screen.contains("brak danych"), "{screen}");
    app.start_refresh();
    gate.add_permits(1);
    app.drain_jobs().await;
    assert_eq!(stub.hits(LIST), 2);
}

#[tokio::test]
async fn data_that_is_shown_stays_and_only_the_title_gets_the_mark() {
    let stub = Stub::start().await;
    let gate = stub.gated(LIST, two_tasks());
    let mut app = app_over(&stub);
    app.load(serde_json::from_value(two_tasks()).expect("a listing"));
    app.set_animation_frame(2);

    app.start_refresh();
    stub.reached(LIST, 1).await;

    // AE4: the rows are the same, the mark is in the title only.
    let screen = screen_of(&mut app, 100, 30);
    assert!(screen.contains("Zadania ⠹"), "{screen}");
    assert!(!screen.contains("wczytuję zadania"), "{screen}");
    assert!(screen.contains("t1") && screen.contains("t2"), "{screen}");

    gate.add_permits(1);
    app.drain_jobs().await;
    let screen = screen_of(&mut app, 100, 30);
    assert!(!screen.contains("Zadania ⠹"), "{screen}");
}

#[tokio::test]
async fn the_ascii_marker_is_used_when_the_spinner_says_so() {
    let stub = Stub::start().await;
    let gate = stub.gated(LIST, two_tasks());
    let mut app = app_over(&stub);
    app.set_spinner(Spinner::ascii());
    app.set_animation_frame(1);

    app.start_refresh();
    stub.reached(LIST, 1).await;
    let screen = screen_of(&mut app, 100, 30);
    assert!(screen.contains("/ wczytuję zadania…"), "{screen}");

    gate.add_permits(1);
    app.drain_jobs().await;
}

#[tokio::test]
async fn the_animation_moves_only_while_a_read_runs() {
    let stub = Stub::start().await;
    let gate = stub.gated(LIST, two_tasks());
    let mut app = app_over(&stub);

    app.advance_animation();
    assert_eq!(app.animation_frame(), 0, "nothing runs");

    app.start_refresh();
    app.advance_animation();
    assert_eq!(app.animation_frame(), 1);

    gate.add_permits(1);
    app.drain_jobs().await;
    app.advance_animation();
    assert_eq!(app.animation_frame(), 1);
}

#[tokio::test]
async fn waiting_for_an_outcome_with_no_job_does_not_wait() {
    let stub = Stub::start().await;
    let mut app = app_over(&stub);
    let outcome = tokio::time::timeout(Duration::from_millis(200), app.next_outcome())
        .await
        .expect("no job: no waiting");
    assert!(outcome.is_none());
}

#[tokio::test]
async fn a_read_that_never_answers_does_not_keep_the_application_from_leaving() {
    let stub = Stub::start().await;
    let _closed = stub.gated(LIST, two_tasks());
    let mut app = app_over(&stub);

    // AE6: quit works, and dropping the application does not wait.
    app.start_refresh();
    stub.reached(LIST, 1).await;
    press(&mut app, KeyCode::Char('q'));
    press(&mut app, KeyCode::Char('q'));
    assert!(app.should_quit());
    tokio::time::timeout(Duration::from_millis(200), async { drop(app) })
        .await
        .expect("dropping the application is immediate");
}

fn timeline_body(task_id: &str) -> serde_json::Value {
    serde_json::to_value(timeline(task_id, Vec::new(), None)).expect("a timeline")
}

#[tokio::test]
async fn the_reads_of_a_task_that_was_left_are_aborted_and_never_shown_for_another() {
    let stub = Stub::start().await;
    stub.open(LIST, two_tasks());
    stub.open(HARNESS, serde_json::json!({}));
    let slow = stub.gated("/control/v1/tasks/t1/timeline", timeline_body("t1"));
    stub.open("/control/v1/tasks/t2/timeline", timeline_body("t2"));
    let mut app = app_over(&stub);
    app.refresh().await;
    assert_eq!(app.list().selected_id(), Some("t1"));

    // AE5: the read of t1 is under way when the selection moves to t2.
    app.start_detail_reads();
    stub.reached("/control/v1/tasks/t1/timeline", 1).await;
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.list().selected_id(), Some("t2"));
    app.start_detail_reads();
    slow.add_permits(1);
    app.drain_jobs().await;

    let first = app
        .detail()
        .view("t1")
        .and_then(|view| view.progress().timeline());
    assert!(first.is_none(), "the aborted read brought nothing");
    let second = app
        .detail()
        .view("t2")
        .and_then(|view| view.progress().timeline());
    assert!(second.is_some(), "the read of t2 was taken in");
    let screen = screen_of(&mut app, 100, 30);
    assert!(!screen.contains("wczytuję przebieg"), "{screen}");
}

#[tokio::test]
async fn a_timeline_that_is_read_shows_the_marker_in_its_tab() {
    let stub = Stub::start().await;
    stub.open(LIST, two_tasks());
    stub.open(HARNESS, serde_json::json!({}));
    let gate = stub.gated("/control/v1/tasks/t1/timeline", timeline_body("t1"));
    let mut app = app_over(&stub);
    app.refresh().await;
    // *Przebieg*, the first tab of the details.
    app.detail_mut()
        .set_tab(narsor::tui::task_app::DetailTab::Progress);

    app.start_detail_reads();
    stub.reached("/control/v1/tasks/t1/timeline", 1).await;
    assert!(app.detail_loading());
    let screen = screen_of(&mut app, 120, 40);
    assert!(screen.contains("⠋ wczytuję przebieg…"), "{screen}");

    gate.add_permits(1);
    app.drain_jobs().await;
    assert!(!app.detail_loading());
    let screen = screen_of(&mut app, 120, 40);
    assert!(!screen.contains("wczytuję przebieg"), "{screen}");
}

fn history_bodies(stub: &Stub) -> Arc<Semaphore> {
    let gate = stub.gated(
        ARCHIVED,
        serde_json::to_value(listing(vec![archived("old-1", "stary", 1)], "c1"))
            .expect("a listing"),
    );
    stub.open(
        "/control/v1/executions",
        serde_json::json!({"executions": []}),
    );
    gate
}

async fn app_with_list(stub: &Stub) -> TaskApp {
    stub.open(LIST, two_tasks());
    stub.open(HARNESS, serde_json::json!({}));
    let mut app = app_over(stub);
    app.refresh().await;
    app
}

#[tokio::test]
async fn a_view_that_opens_shows_the_marker_and_takes_its_keys() {
    let stub = Stub::start().await;
    let gate = history_bodies(&stub);
    let mut app = app_with_list(&stub).await;

    let request = press(&mut app, KeyCode::Char('H'));
    assert_eq!(request, Request::OpenHistory);
    app.dispatch(request).await;
    assert_eq!(app.view_kind(), ViewKind::Opening(OpeningKind::History));

    let screen = screen_of(&mut app, 100, 30);
    assert!(
        screen.contains("Historia") && screen.contains("⠋ wczytuję…"),
        "{screen}"
    );
    assert!(
        screen.contains("Esc") && screen.contains("wyjdź"),
        "{screen}"
    );

    // Another key of a view does nothing while it waits; the same key neither.
    let again = press(&mut app, KeyCode::Char('H'));
    app.dispatch(again).await;
    let other = press(&mut app, KeyCode::Char('R'));
    app.dispatch(other).await;
    assert_eq!(app.view_kind(), ViewKind::Opening(OpeningKind::History));

    gate.add_permits(1);
    app.drain_jobs().await;
    assert_eq!(app.view_kind(), ViewKind::History);
    assert_eq!(stub.hits(ARCHIVED), 1);
}

#[tokio::test]
async fn esc_leaves_a_view_that_waits_and_its_answer_opens_nothing() {
    let stub = Stub::start().await;
    let gate = history_bodies(&stub);
    let mut app = app_with_list(&stub).await;

    app.dispatch(Request::OpenHistory).await;
    stub.reached(ARCHIVED, 1).await;
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.view_kind(), ViewKind::Tasks);

    gate.add_permits(1);
    app.drain_jobs().await;
    assert_eq!(
        app.view_kind(),
        ViewKind::Tasks,
        "the stale answer is dropped"
    );

    // H, Esc, R: only the repositories open.
    stub.open(
        "/control/v1/repositories",
        serde_json::json!({"repositories": [], "unreadable": []}),
    );
    stub.open(
        "/control/v1/capabilities",
        serde_json::to_value(task_app_support::capabilities(false, None)).expect("caps"),
    );
    app.dispatch(Request::OpenHistory).await;
    press(&mut app, KeyCode::Esc);
    app.dispatch(Request::OpenRepositories).await;
    gate.add_permits(1);
    app.drain_jobs().await;
    assert_eq!(app.view_kind(), ViewKind::Repositories);
}

#[tokio::test]
async fn a_view_whose_read_fails_gives_the_list_back_and_says_why() {
    let stub = Stub::start().await;
    let gate = stub.failing(ARCHIVED);
    stub.open(
        "/control/v1/executions",
        serde_json::json!({"executions": []}),
    );
    let mut app = app_with_list(&stub).await;

    app.dispatch(Request::OpenHistory).await;
    gate.add_permits(1);
    app.drain_jobs().await;

    assert_eq!(app.view_kind(), ViewKind::Tasks);
    let note = app.notifier().latest().expect("a notice").text.clone();
    assert!(
        note.starts_with("historia: nie udało się wczytać"),
        "{note}"
    );
    let screen = screen_of(&mut app, 100, 30);
    assert!(!screen.contains("⠋ wczytuję…"), "{screen}");
}

#[tokio::test]
async fn the_opening_view_is_drawn_on_a_terminal_that_is_too_small_and_still_takes_keys() {
    let stub = Stub::start().await;
    let _gate = history_bodies(&stub);
    let mut app = app_with_list(&stub).await;

    app.dispatch(Request::OpenHistory).await;
    let screen = screen_of(&mut app, 20, 5);
    assert!(screen.contains("za mały"), "{screen}");
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.view_kind(), ViewKind::Tasks);
}

#[tokio::test]
async fn an_opening_harness_list_waits_for_its_read_and_then_opens_over_it() {
    let stub = Stub::start().await;
    let mut app = app_with_list(&stub).await;
    let gate = stub.gated(
        HARNESS,
        serde_json::to_value(task_app_support::harness_selection(
            "claude-code",
            None,
            &["claude-code", "opencode"],
        ))
        .expect("a snapshot"),
    );

    app.dispatch(Request::OpenHarness).await;
    assert_eq!(app.view_kind(), ViewKind::Opening(OpeningKind::Harness));
    gate.add_permits(1);
    app.drain_jobs().await;
    assert_eq!(app.view_kind(), ViewKind::Tasks);
}

// The usage limits of the harnesses (U7): read at start and every 15 s, on a
// clock of their own.

const USAGE: &str = "/control/v1/usage";

/// Fifteen seconds and a little: the period of the usage limits has passed.
fn past_the_period() -> Instant {
    Instant::now() + Duration::from_secs(15) + Duration::from_millis(100)
}

fn claude_usage() -> serde_json::Value {
    serde_json::json!({
        "harnesses": [{
            "harness": "claude-code",
            "observed_at": "2026-10-31T12:00:00Z",
            "received_at": "2026-10-31T12:00:01Z",
            "limits": [
                {"window": "5h", "utilization": 0.4, "resets_at": "2026-10-31T16:00:00Z"},
                {"window": "7d", "utilization": 0.9, "resets_at": "2026-11-04T09:00:00Z",
                 "status": "allowed_warning"}
            ]
        }]
    })
}

#[tokio::test]
async fn the_usage_limits_are_read_at_start_and_again_after_fifteen_seconds() {
    let stub = Stub::start().await;
    stub.open(USAGE, claude_usage());
    let mut app = app_over(&stub);

    assert!(app.start_usage_if_due(Instant::now()));
    app.drain_jobs().await;
    assert_eq!(stub.hits(USAGE), 1);
    let usage = app.harness_usage().expect("the limits that were read");
    assert_eq!(usage.harnesses[0].harness, "claude-code");
    assert_eq!(usage.harnesses[0].limits.len(), 2);

    // Not before the period has passed since the read finished.
    assert!(!app.start_usage_if_due(Instant::now()));
    let due = app.next_usage_due().expect("a next read");
    assert!(due > Instant::now() + Duration::from_secs(14), "{due:?}");

    assert!(app.start_usage_if_due(past_the_period()));
    app.drain_jobs().await;
    assert_eq!(stub.hits(USAGE), 2);
}

#[tokio::test]
async fn the_usage_limits_are_read_when_the_list_is_never_read_again() {
    let stub = Stub::start().await;
    stub.open(USAGE, claude_usage());
    // `NARSOR_TUI_REFRESH_INTERVAL=none`: no period for the list.
    let mut app = TaskApp::new(stub.client(), "local", None);
    assert_eq!(app.next_refresh_due(), None);

    assert!(app.start_usage_if_due(Instant::now()));
    app.drain_jobs().await;
    assert!(app.start_usage_if_due(past_the_period()));
    app.drain_jobs().await;
    assert_eq!(stub.hits(USAGE), 2);
}

#[tokio::test]
async fn a_usage_read_that_hangs_keeps_no_key_waiting_and_starts_no_second_read() {
    let stub = Stub::start().await;
    let gate = stub.gated(USAGE, claude_usage());
    let mut app = app_over(&stub);

    assert!(app.start_usage_if_due(Instant::now()));
    stub.reached(USAGE, 1).await;
    assert!(app.is_reading_usage() && app.has_running());
    // Nothing on the screen waits for it: no marker moves.
    assert!(!app.is_loading());
    assert_eq!(app.next_usage_due(), None);

    assert!(!app.start_usage_if_due(past_the_period()));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(stub.hits(USAGE), 1);

    // A key is handled before the answer.
    press(&mut app, KeyCode::Char('?'));
    assert!(app.is_help_open());

    gate.add_permits(1);
    app.drain_jobs().await;
    assert!(!app.is_reading_usage());
    assert!(app.harness_usage().is_some());
}

#[tokio::test]
async fn a_failed_usage_read_clears_the_limits_and_leaves_the_connection_alone() {
    let stub = Stub::start().await;
    let mut app = app_with_list(&stub).await;
    stub.open(USAGE, claude_usage());
    assert!(app.start_usage_if_due(Instant::now()));
    app.drain_jobs().await;
    assert!(app.harness_usage().is_some());
    let link = app.connection().state();
    let notice = app.notifier().latest().map(|notice| notice.text.clone());

    let gate = stub.failing(USAGE);
    gate.add_permits(1);
    assert!(app.start_usage_if_due(past_the_period()));
    app.drain_jobs().await;

    assert_eq!(app.harness_usage(), None);
    assert_eq!(app.connection().state(), link);
    assert_eq!(
        app.notifier().latest().map(|notice| notice.text.clone()),
        notice
    );
    // The next read waits the period, as after an answer.
    assert!(!app.start_usage_if_due(Instant::now()));
}

#[tokio::test]
async fn an_empty_usage_answer_forgets_the_harness_shown_before() {
    let stub = Stub::start().await;
    stub.open(USAGE, claude_usage());
    let mut app = app_over(&stub);
    assert!(app.start_usage_if_due(Instant::now()));
    app.drain_jobs().await;
    assert!(app.harness_usage().is_some());

    // AE8: the report expired on the server; the interface keeps nothing.
    stub.open(USAGE, serde_json::json!({"harnesses": []}));
    assert!(app.start_usage_if_due(past_the_period()));
    app.drain_jobs().await;
    assert_eq!(app.harness_usage(), None);
}

#[tokio::test]
async fn a_usage_answer_closes_no_dialog_and_moves_no_focus() {
    let stub = Stub::start().await;
    let mut app = app_with_list(&stub).await;
    let gate = stub.gated(USAGE, claude_usage());

    assert!(app.start_usage_if_due(Instant::now()));
    stub.reached(USAGE, 1).await;
    press(&mut app, KeyCode::Char('.'));
    assert!(app.overlay().is_some(), "the action menu is open");
    let focus = app.focus();

    gate.add_permits(1);
    app.drain_jobs().await;
    assert!(app.harness_usage().is_some());
    assert!(app.overlay().is_some(), "the action menu is still open");
    assert_eq!(app.focus(), focus);
}
