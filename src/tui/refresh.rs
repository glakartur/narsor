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
//! The schedule is plain data over [`Instant`]s handed in by the caller, so a
//! test can step time without sleeping.

use std::time::{Duration, Instant};

/// How long after the first relevant event the refresh it asked for runs.
pub const DEBOUNCE: Duration = Duration::from_millis(400);

/// When the next automatic refresh is due.
#[derive(Debug, Clone)]
pub struct RefreshSchedule {
    /// The periodic interval; `None` turns the periodic read off.
    interval: Option<Duration>,
    /// When the last refresh (automatic or `r`) finished.
    last: Instant,
    /// When the first event still waiting for its refresh arrived.
    requested: Option<Instant>,
}

impl RefreshSchedule {
    /// A schedule whose period starts at `now`.
    pub fn new(interval: Option<Duration>, now: Instant) -> Self {
        Self {
            interval,
            last: now,
            requested: None,
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
        match (requested, periodic) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left, right) => left.or(right),
        }
    }

    /// A refresh finished at `now`: both triggers start over.
    pub fn done(&mut self, now: Instant) {
        self.last = now;
        self.requested = None;
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
    fn a_manual_refresh_restarts_the_period() {
        let start = Instant::now();
        let mut schedule = RefreshSchedule::new(Some(Duration::from_secs(10)), start);
        schedule.done(start + Duration::from_secs(8));
        assert!(!schedule.is_due(start + Duration::from_secs(12)));
        assert!(schedule.is_due(start + Duration::from_secs(18)));
    }
}
