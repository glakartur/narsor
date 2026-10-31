//! How a usage limit of a harness subscription reads (R11): its part used in
//! whole percent and the time left until it resets.
//!
//! The TUI bar ([`crate::tui::task_app::usage`]) and the CLI both write these
//! values, so both take them from here: one spelling of `4h10m`, not two.

use chrono::{DateTime, Utc};

const MINUTES_PER_HOUR: i64 = 60;
const MINUTES_PER_DAY: i64 = 24 * MINUTES_PER_HOUR;

/// The used part `utilization` (0 to 1) in whole percent, rounded half away
/// from zero: `0.005` is `1`, `0.004` is `0`. A value outside 0 to 1 is held
/// to that range; a value that is not a number is `0`.
pub fn percent(utilization: f64) -> u32 {
    // In 0..=100 after the clamp, so the cast loses nothing; NaN casts to 0.
    (utilization * 100.0).round().clamp(0.0, 100.0) as u32
}

/// The time from `now` until `resets_at`, in whole minutes rounded down:
/// `XdYh` from a day on, `XhYm` from an hour on, `Ym` below that, and `0m`
/// when the reset is not in the future any more.
pub fn time_to_reset(now: DateTime<Utc>, resets_at: DateTime<Utc>) -> String {
    let minutes = (resets_at - now).num_minutes().max(0);
    if minutes >= MINUTES_PER_DAY {
        let days = minutes / MINUTES_PER_DAY;
        let hours = minutes % MINUTES_PER_DAY / MINUTES_PER_HOUR;
        format!("{days}d{hours}h")
    } else if minutes >= MINUTES_PER_HOUR {
        let hours = minutes / MINUTES_PER_HOUR;
        let rest = minutes % MINUTES_PER_HOUR;
        format!("{hours}h{rest}m")
    } else {
        format!("{minutes}m")
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 31, 14, 2, 0).unwrap()
    }

    fn after(duration: Duration) -> String {
        time_to_reset(now(), now() + duration)
    }

    #[test]
    fn the_time_to_reset_has_the_two_largest_units() {
        assert_eq!(after(Duration::hours(4) + Duration::minutes(10)), "4h10m");
        assert_eq!(after(Duration::minutes(45)), "45m");
        assert_eq!(after(Duration::days(2) + Duration::hours(3)), "2d3h");
        assert_eq!(after(Duration::hours(24)), "1d0h");
        assert_eq!(after(Duration::hours(1)), "1h0m");
    }

    #[test]
    fn the_part_of_a_minute_is_dropped() {
        assert_eq!(after(Duration::seconds(59)), "0m");
        assert_eq!(after(Duration::minutes(45) + Duration::seconds(59)), "45m");
    }

    #[test]
    fn a_reset_in_the_past_reads_zero_minutes() {
        assert_eq!(after(Duration::zero()), "0m");
        assert_eq!(after(-Duration::hours(3)), "0m");
    }

    #[test]
    fn the_percent_is_rounded_to_a_whole_number() {
        assert_eq!(percent(0.005), 1);
        assert_eq!(percent(0.004), 0);
        assert_eq!(percent(1.0), 100);
        assert_eq!(percent(0.9), 90);
        assert_eq!(percent(0.0), 0);
    }

    #[test]
    fn a_percent_out_of_range_is_held_to_it() {
        assert_eq!(percent(1.7), 100);
        assert_eq!(percent(-0.3), 0);
        assert_eq!(percent(f64::NAN), 0);
    }
}
