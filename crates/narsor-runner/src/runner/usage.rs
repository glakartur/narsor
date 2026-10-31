//! The usage limits of the task's harnesses, on their way to the control
//! plane (R8).
//!
//! The engine hands every report to [`LatestUsage`] without waiting; the
//! [`super::Session`] loop takes the latest one when it can and sends it in
//! a task of its own ([`send`]). A report that does not get through is
//! logged and forgotten: the next one replaces it anyway, and the snapshot
//! backoff of the link is not this report's business.

use std::sync::Arc;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::controller_protocol::{HarnessUsageReport, RunnerClient};
use crate::engine::UsageReporter;

/// What the control loop reads the reports from: only the latest counts.
pub(super) type UsageReports = watch::Receiver<Option<HarnessUsageReport>>;

/// The reporter the runner's engine is given: it keeps the latest report
/// for the control loop, replacing any it has not taken yet.
#[derive(Debug)]
pub(super) struct LatestUsage(watch::Sender<Option<HarnessUsageReport>>);

impl LatestUsage {
    /// A reporter and the receiving end the control loop reads.
    pub(super) fn channel() -> (Arc<Self>, UsageReports) {
        let (sender, receiver) = watch::channel(None);
        (Arc::new(Self(sender)), receiver)
    }
}

impl UsageReporter for LatestUsage {
    fn report(&self, report: HarnessUsageReport) {
        self.0.send_replace(Some(report));
    }
}

/// Sends `report` once, in a task of its own; a failure is only logged.
pub(super) fn send(client: RunnerClient, report: HarnessUsageReport) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = client.report_usage(&report).await {
            tracing::debug!(
                harness = %report.harness,
                %error,
                "the usage limits did not reach the control plane; dropped"
            );
        }
    })
}
