//! The usage limits of the task's harnesses, on their way to the control
//! plane (R8).
//!
//! [`UsageForwarder`] is the runner's one component for them. The engine
//! hands it every report, marked with its harness, without waiting. The
//! forwarder keeps the latest unsent report of each harness: a new report
//! replaces only the unsent report of the same harness. The [`super::Session`]
//! loop only drives it: it waits until a send can start
//! ([`UsageForwarder::ready`]) and starts it with the client it has
//! ([`UsageForwarder::forward`]). The reports go out one at a time, in the
//! order their harnesses first waited, so the control plane never receives an
//! older report of a harness after a newer one. A report that does not get
//! through is logged and forgotten: there is no retry, the next report of the
//! harness replaces it anyway, and the snapshot backoff of the link is not
//! this report's business.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::controller_protocol::{HarnessUsageReport, RunnerClient};
use crate::engine::UsageReporter;

/// The runner's usage component: the buffer of unsent reports and the send
/// of one at a time.
#[derive(Debug, Default)]
pub(super) struct UsageForwarder {
    state: Mutex<State>,
    /// Signalled when a report waits or a send ended.
    changed: Notify,
}

#[derive(Debug, Default)]
struct State {
    /// At most one report per harness, oldest harness first.
    waiting: VecDeque<HarnessUsageReport>,
    /// Whether a send is on its way.
    sending: bool,
}

impl State {
    fn can_send(&self) -> bool {
        !self.sending && !self.waiting.is_empty()
    }
}

impl UsageForwarder {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Resolves when a send can start: a report waits and no send is on its
    /// way. Cancel-safe: it changes nothing.
    pub(super) async fn ready(&self) {
        while !self.lock().can_send() {
            // A signal sent between the check and this wait is kept by the
            // notify, so none is lost.
            self.changed.notified().await;
        }
    }

    /// Takes the oldest waiting report and, when there is a `client`, starts
    /// to send it in a task of its own; the task, when a send started.
    ///
    /// Nothing happens while a send is on its way. Without a client the
    /// report is dropped (R8): the link's backoff is not touched.
    pub(super) fn forward(
        self: &Arc<Self>,
        client: Option<RunnerClient>,
    ) -> Option<JoinHandle<()>> {
        let report = {
            let mut state = self.lock();
            if !state.can_send() {
                return None;
            }
            let report = state.waiting.pop_front()?;
            if client.is_some() {
                state.sending = true;
            }
            report
        };
        let Some(client) = client else {
            tracing::debug!(
                harness = %report.harness,
                "no client to send the usage limits with; dropped"
            );
            return None;
        };
        let forwarder = Arc::clone(self);
        Some(tokio::spawn(async move {
            if let Err(error) = client.report_usage(&report).await {
                tracing::debug!(
                    harness = %report.harness,
                    %error,
                    "the usage limits did not reach the control plane; dropped"
                );
            }
            forwarder.lock().sending = false;
            forwarder.changed.notify_one();
        }))
    }

    /// The reports waiting to be sent, oldest harness first.
    #[cfg(test)]
    pub(super) fn waiting(&self) -> Vec<HarnessUsageReport> {
        self.lock().waiting.iter().cloned().collect()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl UsageReporter for UsageForwarder {
    fn report(&self, report: HarnessUsageReport) {
        {
            let mut state = self.lock();
            match state
                .waiting
                .iter_mut()
                .find(|waiting| waiting.harness == report.harness)
            {
                Some(waiting) => *waiting = report,
                None => state.waiting.push_back(report),
            }
        }
        self.changed.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller_protocol::UsageLimit;
    use chrono::Utc;

    fn report(harness: &str, utilization: f64) -> HarnessUsageReport {
        HarnessUsageReport {
            harness: harness.to_string(),
            observed_at: Utc::now(),
            limits: vec![UsageLimit {
                window: "5h".to_string(),
                utilization,
                resets_at: Utc::now() + chrono::Duration::hours(4),
                status: None,
            }],
        }
    }

    fn waiting(forwarder: &UsageForwarder) -> Vec<(String, f64)> {
        forwarder
            .waiting()
            .into_iter()
            .map(|report| (report.harness, report.limits[0].utilization))
            .collect()
    }

    #[test]
    fn a_new_report_replaces_only_the_unsent_report_of_its_harness() {
        let forwarder = UsageForwarder::new();
        forwarder.report(report("claude-code", 0.1));
        forwarder.report(report("codex", 0.2));
        forwarder.report(report("claude-code", 0.3));
        assert_eq!(
            waiting(&forwarder),
            [("claude-code".to_string(), 0.3), ("codex".to_string(), 0.2)]
        );
    }

    #[tokio::test]
    async fn without_a_client_the_oldest_report_is_dropped_and_the_next_waits() {
        let forwarder = UsageForwarder::new();
        forwarder.report(report("claude-code", 0.1));
        forwarder.report(report("codex", 0.2));
        forwarder.ready().await;
        assert!(forwarder.forward(None).is_none());
        assert_eq!(waiting(&forwarder), [("codex".to_string(), 0.2)]);
        forwarder.ready().await;
    }
}
