//! The usage limits of the harnesses' subscriptions, as the runners last
//! reported them.
//!
//! The state is shared by the whole installation: the key is the harness id
//! only, any working runner replaces it and any owner reads all of it. It
//! lives in memory only, so a restart of the control plane drops it. Nothing
//! ages it out in the background: every write and every read first removes
//! the limits that already reset, the harnesses whose report the server
//! received [`USAGE_STATE_LIFETIME`] ago or earlier, and the harnesses left
//! without a limit. A read never shows what the rules removed. A report has
//! no upper bound on when a limit resets, but a report with a limit that
//! reset before the server's clock is refused and removes what the server
//! held for that harness: the runner's view is stale or its clock is wrong.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};

use super::ProtocolService;
use crate::controller_protocol::credentials::RunnerIdentity;
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use narsor_protocol::controller::{
    usage_window_length, HarnessUsageReport, HarnessUsageResponse, HarnessUsageView, UsageLimit,
    UsageStatus, MAX_USAGE_LIMITS, USAGE_STATE_LIFETIME,
};
use narsor_protocol::harness::{self, HARNESSES};

/// The last usage of each harness, keyed by its id in
/// [`narsor_protocol::harness::HARNESSES`], kept in memory.
pub(super) type HarnessUsages = std::sync::Mutex<UsageMap>;

type UsageMap = HashMap<&'static str, HeldUsage>;

/// One harness's last report, as the server keeps it.
#[derive(Debug)]
pub(super) struct HeldUsage {
    observed_at: DateTime<Utc>,
    received_at: DateTime<Utc>,
    /// Shortest window first.
    limits: Vec<UsageLimit>,
}

impl ProtocolService {
    /// Replaces what the server holds for the report's harness with the
    /// report. A report that is not valid is refused and changes nothing,
    /// except a report of a known harness with a limit that reset before
    /// `now`: it is refused and removes what the server held for the
    /// harness. A limit that resets exactly at `now` is dropped; a report
    /// left without a limit removes the harness.
    pub fn runner_usage(
        &self,
        identity: &RunnerIdentity,
        report: HarnessUsageReport,
    ) -> ProtocolResult<()> {
        Self::require_worker(identity)?;
        let now = self.control.now();
        let harness = validate_usage_report(&report).map_err(refuse_usage_report)?;
        if report.limits.iter().any(|limit| limit.resets_at < now) {
            let mut usages = self.lock_harness_usages();
            prune_usages(&mut usages, now);
            usages.remove(harness);
            return Err(refuse_usage_report("a limit reset already"));
        }
        let mut limits: Vec<UsageLimit> = report
            .limits
            .into_iter()
            .filter(|limit| limit.resets_at > now)
            .collect();
        limits.sort_by_key(|limit| usage_window_length(&limit.window));
        let mut usages = self.lock_harness_usages();
        prune_usages(&mut usages, now);
        if limits.is_empty() {
            usages.remove(harness);
        } else {
            usages.insert(
                harness,
                HeldUsage {
                    observed_at: report.observed_at,
                    received_at: now,
                    limits,
                },
            );
        }
        Ok(())
    }

    /// The harnesses with current usage, in the order of
    /// [`narsor_protocol::harness::HARNESSES`], each with its shortest
    /// window first.
    pub fn harness_usage(&self) -> HarnessUsageResponse {
        let now = self.control.now();
        let mut usages = self.lock_harness_usages();
        prune_usages(&mut usages, now);
        let harnesses = HARNESSES
            .iter()
            .filter_map(|known| {
                usages.get(known.id).map(|usage| HarnessUsageView {
                    harness: known.id.to_string(),
                    observed_at: usage.observed_at,
                    received_at: usage.received_at,
                    limits: usage.limits.clone(),
                })
            })
            .collect();
        HarnessUsageResponse { harnesses }
    }

    fn lock_harness_usages(&self) -> std::sync::MutexGuard<'_, UsageMap> {
        self.harness_usages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn refuse_usage_report(message: &'static str) -> ProtocolError {
    ProtocolError::bad_request("invalid_usage_report", message.to_string())
}

/// The report's harness id, or why the report is refused (R6a). The reasons
/// never repeat what the runner sent: it is not trusted text. When a limit
/// resets is checked by the caller, against the server's clock.
fn validate_usage_report(report: &HarnessUsageReport) -> Result<&'static str, &'static str> {
    let harness = harness::find(&report.harness).ok_or("the harness is not known")?;
    if report.limits.len() > MAX_USAGE_LIMITS {
        return Err("the report has too many limits");
    }
    let mut windows = HashSet::new();
    for limit in &report.limits {
        if usage_window_length(&limit.window).is_none() {
            return Err("a limit window is not a count followed by m, h or d");
        }
        if !windows.insert(limit.window.as_str()) {
            return Err("two limits have the same window");
        }
        if !(0.0..=1.0).contains(&limit.utilization) {
            return Err("a limit utilization is not a number from 0 to 1");
        }
        if limit.status == Some(UsageStatus::Unrecognized) {
            return Err("a limit status is not known");
        }
    }
    Ok(harness.id)
}

/// Removes the limits that reset already, the harnesses whose report is too
/// old and the harnesses left without a limit.
fn prune_usages(usages: &mut UsageMap, now: DateTime<Utc>) {
    usages.retain(|_, usage| {
        usage.limits.retain(|limit| limit.resets_at > now);
        usage.received_at + USAGE_STATE_LIFETIME > now && !usage.limits.is_empty()
    });
}
