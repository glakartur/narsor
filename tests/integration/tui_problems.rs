//! The problem log of the interface: every failed call and every error shown
//! is recorded with the screen and task it belongs to, and the visible
//! messages stay as they were.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use narsor::api::ApiClient;
use narsor::tui::task_app::list::MoveRequest;
use narsor::tui::task_app::problems::{Problem, Source};
use narsor::tui::task_app::{DetailTab, Request, TaskApp};
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
    assert_eq!(view.progress().failure(), Some("brak połączenia z serwerem"));

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
