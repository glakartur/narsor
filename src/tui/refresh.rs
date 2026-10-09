//! When the interface re-reads the daemon on its own (KTD16).
//!
//! Two triggers, independent of each other:
//!
//! * **the periodic read** — every `tui_refresh_interval`, whatever the live
//!   stream is doing. It is what covers a `Lagged` or disconnected stream and
//!   everything the stream does not carry at all;
//! * **a relevant live event** — a status change, an input requested or
//!   closed, a problem. It asks for a refresh a short [`DEBOUNCE`] later, so
//!   a burst of events (an answer, then the next state, then the next
//!   question) costs one read, not one per event.
//!
//! A third, private to failures: when a read fails the schedule backs off
//! ([`RefreshSchedule::fail`]) — 1s, 2s, 4s … up to [`MAX_RETRY`] — so an
//! unreachable server is probed soon after it returns without being hammered,
//! and the delay is what the header counts down.
//!
//! The schedule is plain data over [`Instant`]s handed in by the caller, so a
//! test can step time without sleeping.

use std::time::{Duration, Instant};

/// How long after the first relevant event the refresh it asked for runs.
pub const DEBOUNCE: Duration = Duration::from_millis(400);

/// The first delay before a failed read is tried again.
pub const FIRST_RETRY: Duration = Duration::from_secs(1);

/// The longest delay between two attempts after failures.
pub const MAX_RETRY: Duration = Duration::from_secs(10);

/// When the next automatic refresh is due.
#[derive(Debug, Clone)]
pub struct RefreshSchedule {
    /// The periodic interval; `None` turns the periodic read off.
    interval: Option<Duration>,
    /// When the last refresh (automatic or `r`) finished.
    last: Instant,
    /// When the first event still waiting for its refresh arrived.
    requested: Option<Instant>,
    /// Consecutive failed reads; sets the retry delay.
    failures: u32,
    /// When the retry after the last failure runs.
    retry_at: Option<Instant>,
}

impl RefreshSchedule {
    /// A schedule whose period starts at `now`.
    pub fn new(interval: Option<Duration>, now: Instant) -> Self {
        Self {
            interval,
            last: now,
            requested: None,
            failures: 0,
            retry_at: None,
        }
    }

    /// The periodic interval in force.
    pub fn interval(&self) -> Option<Duration> {
        self.interval
    }

    /// A relevant event arrived: a refresh is wanted soon. Later requests
    /// inside the same window fold into the first one.
    pub fn request(&mut self, now: Instant) {
        self.requested.get_or_insert(now);
    }

    /// Whether an event is waiting for its refresh.
    pub fn is_requested(&self) -> bool {
        self.requested.is_some()
    }

    /// Whether a refresh should run at `now`.
    pub fn is_due(&self, now: Instant) -> bool {
        self.next_due().is_some_and(|due| now >= due)
    }

    /// The earliest moment a refresh is due, if one ever is.
    pub fn next_due(&self) -> Option<Instant> {
        let requested = self.requested.map(|at| at + DEBOUNCE);
        let periodic = self.interval.map(|interval| self.last + interval);
        [requested, periodic, self.retry_at]
            .into_iter()
            .flatten()
            .min()
    }

    /// A refresh finished at `now`: all triggers start over.
    pub fn done(&mut self, now: Instant) {
        self.last = now;
        self.requested = None;
        self.failures = 0;
        self.retry_at = None;
    }

    /// A refresh failed at `now`: the period starts over, the events waiting
    /// are dropped (the retry reads everything anyway) and the next attempt is
    /// scheduled after a growing delay, which is returned.
    pub fn fail(&mut self, now: Instant) -> Duration {
        let factor = 2u32.saturating_pow(self.failures.min(16));
        let delay = FIRST_RETRY.saturating_mul(factor).min(MAX_RETRY);
        self.failures = self.failures.saturating_add(1);
        self.last = now;
        self.requested = None;
        self.retry_at = Some(now + delay);
        delay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_periodic_read_is_due_once_the_interval_has_passed() {
        let start = Instant::now();
        let schedule = RefreshSchedule::new(Some(Duration::from_secs(10)), start);
        assert!(!schedule.is_due(start + Duration::from_secs(9)));
        assert!(schedule.is_due(start + Duration::from_secs(10)));
    }

    #[test]
    fn a_burst_of_events_costs_one_refresh_after_the_debounce() {
        let start = Instant::now();
        let mut schedule = RefreshSchedule::new(Some(Duration::from_secs(10)), start);
        schedule.request(start);
        schedule.request(start + Duration::from_millis(100));
        assert!(!schedule.is_due(start + Duration::from_millis(100)));
        assert!(schedule.is_due(start + DEBOUNCE));

        schedule.done(start + DEBOUNCE);
        assert!(!schedule.is_requested());
        assert!(!schedule.is_due(start + DEBOUNCE + Duration::from_secs(1)));
    }

    #[test]
    fn without_an_interval_only_events_trigger_a_refresh() {
        let start = Instant::now();
        let mut schedule = RefreshSchedule::new(None, start);
        assert_eq!(schedule.next_due(), None);
        assert!(!schedule.is_due(start + Duration::from_secs(3600)));
        schedule.request(start);
        assert!(schedule.is_due(start + DEBOUNCE));
    }

    #[test]
    fn a_failed_read_backs_off_and_a_success_resets_the_delay() {
        let start = Instant::now();
        let mut schedule = RefreshSchedule::new(Some(Duration::from_secs(30)), start);
        assert_eq!(schedule.fail(start), Duration::from_secs(1));
        assert!(!schedule.is_due(start));
        assert!(schedule.is_due(start + Duration::from_secs(1)));
        assert_eq!(schedule.fail(start), Duration::from_secs(2));
        assert_eq!(schedule.fail(start), Duration::from_secs(4));
        assert_eq!(schedule.fail(start), Duration::from_secs(8));
        assert_eq!(schedule.fail(start), MAX_RETRY);
        assert_eq!(schedule.fail(start), MAX_RETRY);

        schedule.done(start);
        assert_eq!(schedule.fail(start), Duration::from_secs(1));
    }

    #[test]
    fn a_failure_swallows_the_waiting_events_and_restarts_the_period() {
        let start = Instant::now();
        let mut schedule = RefreshSchedule::new(Some(Duration::from_secs(5)), start);
        schedule.request(start);
        let later = start + Duration::from_secs(20);
        schedule.fail(later);
        assert!(!schedule.is_requested());
        assert!(!schedule.is_due(later));
        assert!(schedule.is_due(later + FIRST_RETRY));
    }

    #[test]
    fn a_manual_refresh_restarts_the_period() {
        let start = Instant::now();
        let mut schedule = RefreshSchedule::new(Some(Duration::from_secs(10)), start);
        schedule.done(start + Duration::from_secs(8));
        assert!(!schedule.is_due(start + Duration::from_secs(12)));
        assert!(schedule.is_due(start + Duration::from_secs(18)));
    }
}
