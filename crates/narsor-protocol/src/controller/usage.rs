//! Harness usage limit reports and views.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// Most limits one harness usage report may carry.
pub const MAX_USAGE_LIMITS: usize = 8;

/// How long the server keeps a harness's usage after the report that set it,
/// counted from when the server received the report, not from `observed_at`.
pub const USAGE_STATE_LIFETIME: Duration = Duration::minutes(15);

/// The length of a usage limit window (`5h`, `7d`, `90m`), or `None` when
/// `window` is not one: a count from 1 to 9999 without a leading zero, then
/// `m`, `h` or `d`.
pub fn usage_window_length(window: &str) -> Option<Duration> {
    let (count, unit) = window.split_at_checked(window.len().checked_sub(1)?)?;
    if !(1..=4).contains(&count.len())
        || count.starts_with('0')
        || !count.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let count = count.parse().ok()?;
    match unit {
        "m" => Some(Duration::minutes(count)),
        "h" => Some(Duration::hours(count)),
        "d" => Some(Duration::days(count)),
        _ => None,
    }
}

/// What a harness said about one usage limit, when it said it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UsageStatus {
    Allowed,
    /// Still allowed, close to the limit.
    AllowedWarning,
    Rejected,
    #[serde(other)]
    Unrecognized,
}

/// One usage limit of a harness subscription, the same for every harness.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageLimit {
    /// The window the limit counts over, e.g. `5h` or `7d`
    /// ([`usage_window_length`]).
    pub window: String,
    /// The used part of the limit, from 0 to 1.
    pub utilization: f64,
    pub resets_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<UsageStatus>,
}

/// `POST /runner/v1/usage`: the latest usage limits of one harness. It
/// replaces whatever the server held for that harness.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessUsageReport {
    /// A harness id of [`crate::harness::HARNESSES`].
    pub harness: String,
    /// When the runner read the limits from the harness.
    pub observed_at: DateTime<Utc>,
    pub limits: Vec<UsageLimit>,
}

/// The usage limits the server holds for one harness, as presented to an
/// owner client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessUsageView {
    pub harness: String,
    /// When the runner read the limits from the harness.
    pub observed_at: DateTime<Utc>,
    /// When the server received the report.
    pub received_at: DateTime<Utc>,
    /// Shortest window first.
    pub limits: Vec<UsageLimit>,
}

/// `GET /control/v1/usage`: the harnesses with current usage, in the order of
/// [`crate::harness::HARNESSES`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HarnessUsageResponse {
    pub harnesses: Vec<HarnessUsageView>,
}
