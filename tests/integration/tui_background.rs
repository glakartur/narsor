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
use tokio::sync::Semaphore;

use narsor::api::{ApiClient, ClientError, UsageRead, UsageSource};
use narsor::tui::spinner::Spinner;
use narsor::tui::task_app::{Request, TaskApp};
use narsor_protocol::controller::HarnessUsageResponse;

use crate::task_app_support;
use task_app_support::{listing, task};



fn press(app: &mut TaskApp, code: KeyCode) -> Request {
    app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
}






// The usage limits of the harnesses (U7): read at start and every 15 s, on a
// clock of their own.

/// Fifteen seconds and a little: the period of the usage limits has passed.
fn past_the_period() -> Instant {
    Instant::now() + Duration::from_secs(15) + Duration::from_millis(100)
}

fn claude_usage() -> HarnessUsageResponse {
    serde_json::from_value(serde_json::json!({
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
    }))
    .expect("a usage answer")
}

/// A source of usage limits the test drives, in place of the server: it
/// counts the reads started and answers each with what it holds when the
/// read runs — `None` is a failed read. A gated one answers only once the
/// test adds a permit to its gate.
struct UsageDouble {
    answer: Arc<Mutex<Option<HarnessUsageResponse>>>,
    gate: Option<Arc<Semaphore>>,
    reads: AtomicUsize,
}

impl UsageDouble {
    fn answering(answer: HarnessUsageResponse) -> Arc<Self> {
        Arc::new(Self {
            answer: Arc::new(Mutex::new(Some(answer))),
            gate: None,
            reads: AtomicUsize::new(0),
        })
    }

    /// A double whose reads wait for the returned gate.
    fn gated(answer: HarnessUsageResponse) -> (Arc<Self>, Arc<Semaphore>) {
        let gate = Arc::new(Semaphore::new(0));
        let double = Arc::new(Self {
            answer: Arc::new(Mutex::new(Some(answer))),
            gate: Some(Arc::clone(&gate)),
            reads: AtomicUsize::new(0),
        });
        (double, gate)
    }

    fn answer(&self, answer: Option<HarnessUsageResponse>) {
        *self.answer.lock().unwrap() = answer;
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }

    /// Waits until `reads` reads have started.
    async fn reached(&self, reads: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.reads() < reads {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the reads start");
    }
}

impl UsageSource for UsageDouble {
    fn read_usage(&self) -> UsageRead {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let (answer, gate) = (Arc::clone(&self.answer), self.gate.clone());
        Box::pin(async move {
            if let Some(gate) = gate {
                gate.acquire().await.expect("an open gate").forget();
            }
            answer
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| ClientError::Server("the read failed".to_string()))
        })
    }
}

/// An application that reads its usage limits from `usage` and has no
/// server: nothing but the usage limits is read.
fn app_reading(usage: &Arc<UsageDouble>) -> TaskApp {
    let nowhere = ApiClient::new(SocketAddr::from(([127, 0, 0, 1], 9)), "token");
    let mut app = TaskApp::new(nowhere, "local", None)
        .with_usage_source(Arc::clone(usage) as Arc<dyn UsageSource>);
    app.set_spinner(Spinner::braille());
    app
}

/// [`app_reading`] with two tasks taken in as a read of the list would
/// bring them: a live connection, with no server.
fn app_reading_with_list(usage: &Arc<UsageDouble>) -> TaskApp {
    let mut app = app_reading(usage);
    app.load(listing(
        vec![
            task("t1", "local", "running", 1),
            task("t2", "local", "running", 2),
        ],
        "c1",
    ));
    app
}

#[tokio::test]
async fn the_usage_limits_are_read_at_start_and_again_after_fifteen_seconds() {
    let usage = UsageDouble::answering(claude_usage());
    let mut app = app_reading(&usage);

    assert!(app.start_usage_if_due(Instant::now()));
    app.drain_jobs().await;
    assert_eq!(usage.reads(), 1);
    let read = app.harness_usage().expect("the limits that were read");
    assert_eq!(read.harnesses[0].harness, "claude-code");
    assert_eq!(read.harnesses[0].limits.len(), 2);

    // Not before the period has passed since the read finished.
    assert!(!app.start_usage_if_due(Instant::now()));
    let due = app.next_usage_due().expect("a next read");
    assert!(due > Instant::now() + Duration::from_secs(14), "{due:?}");

    assert!(app.start_usage_if_due(past_the_period()));
    app.drain_jobs().await;
    assert_eq!(usage.reads(), 2);
}

#[tokio::test]
async fn the_usage_limits_are_read_when_the_list_is_never_read_again() {
    let usage = UsageDouble::answering(claude_usage());
    // `NARSOR_TUI_REFRESH_INTERVAL=none`: no period for the list.
    let mut app = app_reading(&usage);
    assert_eq!(app.next_refresh_due(), None);

    assert!(app.start_usage_if_due(Instant::now()));
    app.drain_jobs().await;
    assert!(app.start_usage_if_due(past_the_period()));
    app.drain_jobs().await;
    assert_eq!(usage.reads(), 2);
}

#[tokio::test]
async fn a_usage_read_that_hangs_keeps_no_key_waiting_and_starts_no_second_read() {
    let (usage, gate) = UsageDouble::gated(claude_usage());
    let mut app = app_reading(&usage);

    assert!(app.start_usage_if_due(Instant::now()));
    usage.reached(1).await;
    assert!(app.is_reading_usage() && app.has_running());
    // Nothing on the screen waits for it: no marker moves.
    assert!(!app.is_loading());
    assert_eq!(app.next_usage_due(), None);

    assert!(!app.start_usage_if_due(past_the_period()));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(usage.reads(), 1);

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
    let usage = UsageDouble::answering(claude_usage());
    let mut app = app_reading_with_list(&usage);
    assert!(app.start_usage_if_due(Instant::now()));
    app.drain_jobs().await;
    assert!(app.harness_usage().is_some());
    let link = app.connection().state();
    let notice = app.notifier().latest().map(|notice| notice.text.clone());

    usage.answer(None);
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
    let usage = UsageDouble::answering(claude_usage());
    let mut app = app_reading(&usage);
    assert!(app.start_usage_if_due(Instant::now()));
    app.drain_jobs().await;
    assert!(app.harness_usage().is_some());

    // AE8: the report expired on the server; the interface keeps nothing.
    usage.answer(Some(HarnessUsageResponse::default()));
    assert!(app.start_usage_if_due(past_the_period()));
    app.drain_jobs().await;
    assert_eq!(app.harness_usage(), None);
}

#[tokio::test]
async fn a_usage_answer_closes_no_dialog_and_moves_no_focus() {
    let (usage, gate) = UsageDouble::gated(claude_usage());
    let mut app = app_reading_with_list(&usage);

    assert!(app.start_usage_if_due(Instant::now()));
    usage.reached(1).await;
    press(&mut app, KeyCode::Char('.'));
    assert!(app.overlay().is_some(), "the action menu is open");
    let focus = app.focus();

    gate.add_permits(1);
    app.drain_jobs().await;
    assert!(app.harness_usage().is_some());
    assert!(app.overlay().is_some(), "the action menu is still open");
    assert_eq!(app.focus(), focus);
}
