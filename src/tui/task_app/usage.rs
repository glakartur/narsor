//! The usage limits of the harnesses' subscriptions (U7, R9, R15).
//!
//! The interface reads `GET /control/v1/usage` when it starts and then every
//! [`USAGE_PERIOD`], on a clock of its own: not the schedule of the task list
//! ([`super::super::refresh::RefreshSchedule`] has a backoff and can be
//! switched off), so `NARSOR_TUI_REFRESH_INTERVAL=none` does not stop it. The
//! next read is due a period after the previous one finished, whatever it
//! brought.
//!
//! Only the last answer that came is kept. An answer replaces the state; a
//! failure clears it — a limit the interface cannot confirm is not shown. A
//! failure is not a failure of the connection: the header and the bar stay as
//! they are. The answer closes nothing and moves no focus.
//!
//! The read is a job of [`super::background`] under [`JobKey::Usage`]; it
//! does not animate the loading marker, since nothing on the screen waits for
//! it.

use std::time::{Duration, Instant};

use narsor_protocol::controller::HarnessUsageResponse;

use crate::api::ClientError;

use super::background::{JobKey, Outcome};
use super::TaskApp;

/// How long after a read finished the next one is due.
pub const USAGE_PERIOD: Duration = Duration::from_secs(15);

impl TaskApp {
    /// The usage limits as last read: `None` before the first answer, after a
    /// failed read, and when the server holds no fresh report.
    pub fn harness_usage(&self) -> Option<&HarnessUsageResponse> {
        self.harness_usage.as_ref()
    }

    /// Takes `usage` in as an answer of the server would: what a test of the
    /// drawing sets. A response without harnesses is no state.
    pub fn set_harness_usage(&mut self, usage: Option<HarnessUsageResponse>) {
        self.harness_usage = usage.filter(|usage| !usage.harnesses.is_empty());
    }

    /// When the next read of the usage limits is due; `None` while one runs.
    pub fn next_usage_due(&self) -> Option<Instant> {
        if self.is_reading_usage() {
            return None;
        }
        Some(self.next_usage_at)
    }

    /// Whether the usage limits are being read.
    pub fn is_reading_usage(&self) -> bool {
        self.background.is_running(&JobKey::Usage)
    }

    /// Starts the read of the usage limits when it is due at `now` and none
    /// runs. Returns whether one was started.
    pub fn start_usage_if_due(&mut self, now: Instant) -> bool {
        if now < self.next_usage_at {
            return false;
        }
        let client = self.client.clone();
        self.background.start(JobKey::Usage, async move {
            let read = client.control_usage().await;
            Outcome::new(move |app: &mut TaskApp| app.on_usage_read(read, Instant::now()))
        })
    }

    /// The read is back at `now`.
    fn on_usage_read(&mut self, read: Result<HarnessUsageResponse, ClientError>, now: Instant) {
        self.set_harness_usage(read.ok());
        self.next_usage_at = now + USAGE_PERIOD;
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use chrono::{TimeZone, Utc};
    use narsor_protocol::controller::{HarnessUsageView, UsageLimit};

    use super::*;
    use crate::api::ApiClient;

    fn app() -> TaskApp {
        let client = ApiClient::new(SocketAddr::from(([127, 0, 0, 1], 9)), "token");
        TaskApp::new(client, "local", None)
    }

    fn claude() -> HarnessUsageResponse {
        let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 0, 0).unwrap();
        HarnessUsageResponse {
            harnesses: vec![HarnessUsageView {
                harness: "claude-code".into(),
                observed_at: at,
                received_at: at,
                limits: vec![UsageLimit {
                    window: "5h".into(),
                    utilization: 0.4,
                    resets_at: at,
                    status: None,
                }],
            }],
        }
    }

    #[test]
    fn an_answer_replaces_the_state_and_a_failure_clears_it() {
        let mut app = app();
        let now = Instant::now();
        app.on_usage_read(Ok(claude()), now);
        assert_eq!(app.harness_usage(), Some(&claude()));
        app.on_usage_read(Err(ClientError::Server("boom".into())), now);
        assert_eq!(app.harness_usage(), None);
    }

    #[test]
    fn an_empty_answer_forgets_the_harness_shown_before() {
        let mut app = app();
        app.set_harness_usage(Some(claude()));
        app.on_usage_read(Ok(HarnessUsageResponse::default()), Instant::now());
        assert_eq!(app.harness_usage(), None);
    }

    #[test]
    fn the_next_read_is_due_a_period_after_the_last_finished() {
        let mut app = app();
        let finished = Instant::now();
        app.on_usage_read(Ok(claude()), finished);
        assert_eq!(app.next_usage_due(), Some(finished + USAGE_PERIOD));
    }

    #[tokio::test]
    async fn the_first_read_is_due_at_once_and_does_not_animate_the_marker() {
        let mut app = app();
        assert!(app.start_usage_if_due(Instant::now()));
        assert!(app.is_reading_usage());
        assert!(!app.is_loading());
        assert_eq!(app.next_usage_due(), None);
        assert!(
            !app.start_usage_if_due(Instant::now()),
            "one read at a time"
        );
    }
}
