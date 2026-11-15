//! Turns the `rate_limit_info` of a Claude Code `rate_limit_event` line into
//! subscription usage limits (R7).
//!
//! The line is a report, not part of the conversation (R8a): the parser keeps
//! the limits for [`HarnessStreamParser::take_usage`](super::HarnessStreamParser::take_usage)
//! and gives no chat element for it.

use chrono::{DateTime, Utc};
use narsor_protocol::controller::{UsageLimit, UsageStatus};
use serde_json::{Map, Value};

/// The line type that carries the limits.
pub(super) const RATE_LIMIT_EVENT: &str = "rate_limit_event";

/// The Claude Code windows narsor knows, with the window each one is
/// reported as, in the order they are reported. Any other window is skipped.
const WINDOWS: [(&str, &str); 2] = [("five_hour", "5h"), ("seven_day", "7d")];

/// The limits of a `rate_limit_event` line; empty when it has no known
/// window with both a `utilization` and a `resetsAt`.
///
/// The windows come from `unifiedWindows`. Without it, the line's own
/// `rateLimitType`, `utilization`, `resetsAt` and `status` are the only
/// window. A window takes its own `status`; a window without one takes the
/// line's `status` only when it is the line's `rateLimitType`.
pub(super) fn limits(line: &Map<String, Value>) -> Vec<UsageLimit> {
    let Some(info) = line.get("rate_limit_info").and_then(Value::as_object) else {
        return Vec::new();
    };
    let limit_type = info.get("rateLimitType").and_then(Value::as_str);
    let line_status = info.get("status");

    let Some(windows) = info.get("unifiedWindows").and_then(Value::as_object) else {
        return WINDOWS
            .iter()
            .filter(|(name, _)| Some(*name) == limit_type)
            .filter_map(|(_, window)| limit(window, info, line_status))
            .collect();
    };
    WINDOWS
        .iter()
        .filter_map(|(name, window)| {
            let fields = windows.get(*name)?.as_object()?;
            let fallback = (Some(*name) == limit_type).then_some(line_status).flatten();
            limit(window, fields, fallback)
        })
        .collect()
}

/// One limit from a window's fields; `None` without `utilization` or
/// `resetsAt`. `fallback` is the status to use when the window has none. A
/// finite utilization outside 0 to 1 is brought to the nearest end: the
/// control plane refuses a report with one.
fn limit(
    window: &str,
    fields: &Map<String, Value>,
    fallback: Option<&Value>,
) -> Option<UsageLimit> {
    let utilization = fields.get("utilization")?.as_f64()?;
    let utilization = if utilization.is_finite() {
        utilization.clamp(0.0, 1.0)
    } else {
        utilization
    };
    let resets_at = unix_seconds(fields.get("resetsAt")?)?;
    let status = fields
        .get("status")
        .or(fallback)
        .and_then(Value::as_str)
        .and_then(status);
    Some(UsageLimit {
        window: window.to_string(),
        utilization,
        resets_at,
        status,
    })
}

/// `resetsAt`: seconds since the Unix epoch.
fn unix_seconds(value: &Value) -> Option<DateTime<Utc>> {
    let seconds = value
        .as_i64()
        .or_else(|| value.as_f64().map(|seconds| seconds as i64))?;
    DateTime::from_timestamp(seconds, 0)
}

/// A status narsor knows; any other leaves the limit without one.
fn status(text: &str) -> Option<UsageStatus> {
    match text {
        "allowed" => Some(UsageStatus::Allowed),
        "allowed_warning" => Some(UsageStatus::AllowedWarning),
        "rejected" => Some(UsageStatus::Rejected),
        _ => None,
    }
}
