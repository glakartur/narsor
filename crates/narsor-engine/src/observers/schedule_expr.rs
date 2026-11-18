//! Five-field cron expressions of observer schedules (R28), evaluated in
//! UTC.
//!
//! The only place that knows the cron crate: the rest of the engine sees
//! [`CronSchedule::parse`] (with its refusals) and
//! [`CronSchedule::next_after`].
//!
//! ```text
//! accepted   minute hour day-of-month month day-of-week   ("*/10 * * * *")
//! refused    a seconds field (six or more fields)          ("*/30 * * * * *")
//!            an alias                                      ("@daily")
//!            anything that can fire twice within MIN_INTERVAL_SECS
//!            anything that never fires                     ("0 0 31 2 *")
//! ```

use super::model::MIN_INTERVAL_SECS;
use super::{ObserverError, Result};
use chrono::{DateTime, Duration, TimeZone, Utc};
use croner::parser::{CronParser, Seconds, Year};
use croner::Cron;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Longest cron expression accepted.
pub const MAX_CRON_LEN: usize = 128;

/// Fire times of an expression checked against [`MIN_INTERVAL_SECS`]. A
/// five-field expression fires on whole minutes, so two fires are never
/// less than 60 s apart and the check cannot fail while the minimum is 60 s;
/// it keeps the rule true should the minimum ever grow.
const GAP_SAMPLE: usize = 256;

/// One validated cron expression of five fields; on the wire and on disk its
/// text, as the owner wrote it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CronSchedule {
    text: String,
    cron: Cron,
}

impl CronSchedule {
    /// Parses and checks `text`; see the module documentation.
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let refusal = |reason: String| ObserverError::invalid("cron", reason);
        if text.is_empty() || text.len() > MAX_CRON_LEN {
            return Err(refusal(format!(
                "a cron expression must have 1-{MAX_CRON_LEN} characters"
            )));
        }
        if text.contains('@') {
            return Err(refusal(format!(
                "'{text}' is an alias: write the five fields (minute hour day-of-month month day-of-week)"
            )));
        }
        let fields = text.split_whitespace().count();
        if fields != 5 {
            let hint = if fields > 5 {
                " (a seconds field is not allowed: the schedule has minute precision)"
            } else {
                ""
            };
            return Err(refusal(format!(
                "'{text}' has {fields} fields; a cron expression has five: minute hour day-of-month month day-of-week{hint}"
            )));
        }
        let cron = CronParser::builder()
            .seconds(Seconds::Disallowed)
            .year(Year::Disallowed)
            .build()
            .parse(text)
            .map_err(|error| {
                refusal(format!("'{text}' is not a valid cron expression: {error}"))
            })?;
        let schedule = Self {
            text: text.to_string(),
            cron,
        };
        schedule.check_gap()?;
        Ok(schedule)
    }

    /// The expression as the owner wrote it (trimmed).
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The first fire time strictly after `now`, in UTC; `None` when the
    /// expression never fires again (impossible for an accepted one within
    /// the years a timestamp holds).
    pub fn next_after(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.cron.find_next_occurrence(&now, false).ok()
    }

    /// Refuses an expression that never fires, or fires twice within
    /// [`MIN_INTERVAL_SECS`].
    fn check_gap(&self) -> Result<()> {
        // A fixed start: the check must not depend on the day it runs.
        let start = Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).unwrap();
        let Some(mut previous) = self.next_after(start) else {
            return Err(ObserverError::invalid(
                "cron",
                format!("'{}' never fires", self.text),
            ));
        };
        let min_gap = Duration::seconds(MIN_INTERVAL_SECS as i64);
        for _ in 0..GAP_SAMPLE {
            let Some(next) = self.next_after(previous) else {
                break;
            };
            if next - previous < min_gap {
                return Err(ObserverError::invalid(
                    "cron",
                    format!(
                        "'{}' fires more often than every {MIN_INTERVAL_SECS} seconds",
                        self.text
                    ),
                ));
            }
            previous = next;
        }
        Ok(())
    }
}

impl PartialEq for CronSchedule {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
    }
}

impl Eq for CronSchedule {}

impl TryFrom<String> for CronSchedule {
    type Error = ObserverError;

    fn try_from(value: String) -> Result<Self> {
        Self::parse(&value)
    }
}

impl From<CronSchedule> for String {
    fn from(value: CronSchedule) -> Self {
        value.text
    }
}

impl fmt::Display for CronSchedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        // November 2026: the 20th is a Friday.
        Utc.with_ymd_and_hms(2026, 11, day, hour, minute, 0)
            .unwrap()
    }

    fn refusal(text: &str) -> String {
        CronSchedule::parse(text).unwrap_err().to_string()
    }

    #[test]
    fn every_minute_is_the_densest_schedule_accepted() {
        let every_minute = CronSchedule::parse("* * * * *").unwrap();
        assert_eq!(every_minute.next_after(at(20, 10, 0)), Some(at(20, 10, 1)));
    }

    #[test]
    fn a_seconds_field_is_refused_with_the_reason() {
        for text in ["* * * * * *", "*/30 * * * * *", "0 0 * * * * 2030"] {
            let reason = refusal(text);
            assert!(
                reason.contains("seconds field is not allowed"),
                "{text}: {reason}"
            );
        }
    }

    #[test]
    fn aliases_and_short_or_broken_expressions_are_refused() {
        assert!(refusal("@daily").contains("alias"));
        assert!(refusal("@every 5m").contains("alias"));
        assert!(refusal("* * * *").contains("has 4 fields"));
        assert!(refusal("").contains("1-128 characters"));
        assert!(refusal(&"1,".repeat(70)).contains("1-128 characters"));
        assert!(refusal("61 * * * *").contains("not a valid cron expression"));
        assert!(refusal("*/0 * * * *").contains("not a valid cron expression"));
        assert!(refusal("0 0 31 2 *").contains("never fires"));
    }

    #[test]
    fn every_ten_minutes_fires_on_the_tens_from_a_fixed_clock() {
        let schedule = CronSchedule::parse("*/10 * * * *").unwrap();
        let mut now = at(20, 10, 0);
        let mut fires = Vec::new();
        for _ in 0..3 {
            now = schedule.next_after(now).unwrap();
            fires.push(now);
        }
        assert_eq!(fires, vec![at(20, 10, 10), at(20, 10, 20), at(20, 10, 30)]);
        // Strictly after: a clock on a fire time gets the next one.
        assert_eq!(schedule.next_after(at(20, 10, 0)), Some(at(20, 10, 10)));
        // Mid-minute counts as after the minute's fire.
        let mid = at(20, 10, 9) + Duration::seconds(30);
        assert_eq!(schedule.next_after(mid), Some(at(20, 10, 10)));
    }

    #[test]
    fn working_days_at_eight_skip_the_weekend() {
        let schedule = CronSchedule::parse("0 8 * * 1-5").unwrap();
        // Friday 20 November, after 08:00: the next fire is Monday 23rd.
        assert_eq!(schedule.next_after(at(20, 9, 0)), Some(at(23, 8, 0)));
        assert_eq!(schedule.next_after(at(20, 7, 59)), Some(at(20, 8, 0)));
    }

    #[test]
    fn the_text_round_trips_through_serde_and_is_checked_on_read() {
        let schedule = CronSchedule::parse("  0 8 * * MON-FRI ").unwrap();
        assert_eq!(schedule.as_str(), "0 8 * * MON-FRI");
        let json = serde_json::to_value(&schedule).unwrap();
        assert_eq!(json, "0 8 * * MON-FRI");
        assert_eq!(
            serde_json::from_value::<CronSchedule>(json).unwrap(),
            schedule
        );
        assert!(serde_json::from_str::<CronSchedule>("\"* * * * * *\"").is_err());
    }
}
