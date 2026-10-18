//! Hierarchical retry policies (D21).
//!
//! A state may carry its own `retry_policy`; when it does not, the workflow's
//! `globals.retry_policy` applies; when neither is set, the default policy runs
//! the action exactly once.
//!
//! # `max_attempts` counts attempts, not retries
//!
//! [`RetryPolicy::max_attempts`] is the **total** number of attempts including
//! the first one, exactly as `WORKFLOW_SCHEMA.md` spells it. `max_attempts: 1`
//! therefore means "no retry" and `max_attempts: 3` means at most two
//! [`crate::domain::EventData::ActionRetrying`] events. A `max_attempts: 0`
//! written by hand is read as one attempt rather than none: refusing to run an
//! action at all is never what the author meant.
//!
//! # The delay between attempts
//!
//! The first retry waits `backoff_secs`, and every further retry multiplies the
//! previous wait by `backoff_multiplier`. The growth is bounded by
//! [`MAX_BACKOFF`], because a long-running workflow with a generous policy can
//! otherwise compute a delay longer than the execution it belongs to.

use std::time::Duration;

use crate::domain::{GlobalsConfig, RetryPolicy, State};

/// Ceiling on the wait between two attempts, whatever the multiplier says.
pub const MAX_BACKOFF: Duration = Duration::from_secs(60 * 60);

/// The retry policy in force for one state, with the arithmetic that goes with it.
///
/// Resolving and applying a policy are separated from the engine loop on
/// purpose: the loop then only asks "may I try again, and how long do I wait",
/// and every off-by-one lives in one place that is unit-testable without
/// running a workflow.
#[derive(Debug, Clone)]
pub struct RetrySchedule {
    policy: RetryPolicy,
}

impl RetrySchedule {
    /// Builds a schedule from an explicit policy.
    pub fn new(policy: RetryPolicy) -> Self {
        Self { policy }
    }

    /// Resolves the policy of a state: its own, else the workflow default, else
    /// the built-in "one attempt" policy.
    pub fn resolve(state: &State, globals: &GlobalsConfig) -> Self {
        let policy = state
            .retry_policy
            .clone()
            .or_else(|| globals.retry_policy.clone())
            .unwrap_or_default();
        Self::new(policy)
    }

    /// The policy this schedule applies.
    pub fn policy(&self) -> &RetryPolicy {
        &self.policy
    }

    /// Total number of attempts allowed, never below one.
    pub fn max_attempts(&self) -> u32 {
        self.policy.max_attempts.max(1)
    }

    /// Whether another attempt may follow the `attempt`-th one (1-based).
    pub fn permits_another(&self, attempt: u32) -> bool {
        attempt < self.max_attempts()
    }

    /// How long to wait before the `attempt`-th attempt (1-based).
    ///
    /// The first attempt never waits. The second waits `backoff_secs`, and each
    /// further one multiplies the previous wait by `backoff_multiplier`.
    pub fn delay_before(&self, attempt: u32) -> Duration {
        if attempt <= 1 || self.policy.backoff_secs == 0 {
            return Duration::ZERO;
        }

        let multiplier =
            if self.policy.backoff_multiplier.is_finite() && self.policy.backoff_multiplier > 0.0 {
                self.policy.backoff_multiplier
            } else {
                // A missing or nonsensical multiplier degrades to a constant delay
                // rather than to no delay at all: the author asked for a pause.
                1.0
            };

        let seconds = self.policy.backoff_secs as f64 * multiplier.powi(attempt as i32 - 2);
        if !seconds.is_finite() || seconds >= MAX_BACKOFF.as_secs_f64() {
            return MAX_BACKOFF;
        }
        Duration::from_secs_f64(seconds)
    }

    /// Number of retries already spent once the `attempt`-th attempt starts.
    ///
    /// This is what [`crate::domain::EventData::ActionRetrying`] reports, so
    /// the first retry is announced as `retry_count: 1`.
    pub fn retry_count(attempt: u32) -> u32 {
        attempt.saturating_sub(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(max_attempts: u32, backoff_secs: u64, backoff_multiplier: f64) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            backoff_secs,
            backoff_multiplier,
        }
    }

    #[test]
    fn max_attempts_counts_the_first_attempt() {
        let schedule = RetrySchedule::new(policy(1, 0, 2.0));
        assert!(!schedule.permits_another(1));

        let schedule = RetrySchedule::new(policy(3, 0, 2.0));
        assert!(schedule.permits_another(1));
        assert!(schedule.permits_another(2));
        assert!(!schedule.permits_another(3));
    }

    #[test]
    fn zero_max_attempts_still_runs_once() {
        let schedule = RetrySchedule::new(policy(0, 0, 2.0));
        assert_eq!(schedule.max_attempts(), 1);
        assert!(!schedule.permits_another(1));
    }

    #[test]
    fn backoff_grows_by_the_multiplier() {
        let schedule = RetrySchedule::new(policy(4, 2, 3.0));
        assert_eq!(schedule.delay_before(1), Duration::ZERO);
        assert_eq!(schedule.delay_before(2), Duration::from_secs(2));
        assert_eq!(schedule.delay_before(3), Duration::from_secs(6));
        assert_eq!(schedule.delay_before(4), Duration::from_secs(18));
    }

    #[test]
    fn an_unusable_multiplier_keeps_the_delay_constant() {
        let schedule = RetrySchedule::new(policy(4, 5, 0.0));
        assert_eq!(schedule.delay_before(2), Duration::from_secs(5));
        assert_eq!(schedule.delay_before(4), Duration::from_secs(5));
    }

    #[test]
    fn the_delay_is_capped() {
        let schedule = RetrySchedule::new(policy(50, 60, 10.0));
        assert_eq!(schedule.delay_before(20), MAX_BACKOFF);
    }

    #[test]
    fn the_state_policy_overrides_the_global_one() {
        let globals = GlobalsConfig {
            timeout_secs: None,
            retry_policy: Some(policy(2, 1, 1.0)),
            image: None,
        };

        let mut state = State {
            id: "s".to_string(),
            max_entries: None,
            description: None,
            action: None,
            outputs: Vec::new(),
            transitions: Default::default(),
            decision: None,
            timeout_secs: None,
            retry_policy: None,
            terminal: false,
            terminal_status: None,
            allow_manual_override: false,
            image: None,
        };

        assert_eq!(RetrySchedule::resolve(&state, &globals).max_attempts(), 2);

        state.retry_policy = Some(policy(5, 0, 1.0));
        assert_eq!(RetrySchedule::resolve(&state, &globals).max_attempts(), 5);

        state.retry_policy = None;
        let empty = GlobalsConfig::default();
        assert_eq!(RetrySchedule::resolve(&state, &empty).max_attempts(), 1);
    }
}
