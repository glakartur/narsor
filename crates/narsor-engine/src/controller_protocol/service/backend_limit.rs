//! Asking running tasks to stop when the limit of concurrent tasks is
//! exceeded.
//!
//! The limit is the daemon's `max_concurrent_tasks` setting, shared by all
//! backends. It can only be exceeded when the setting was lowered and the
//! daemon restarted over tasks already holding slots, or when a backend that
//! was unusable comes back. A backend the configuration cannot supply — it
//! is unknown or unusable — admits nothing.
//!
//! See [`crate::orchestration::ControlPlane::decide_limit_stop`] for which
//! safe points are accepted.

use super::ProtocolService;
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::domain::BackendId;
use crate::orchestration::{BackendAvailability, OrchestrationError, SchedulingPolicy};
use narsor_protocol::controller::{ConcurrencyLimitView, MAX_CONCURRENT_TASKS_LIMIT};

impl ProtocolService {
    /// The limit of tasks processed at once, what they hold and what still
    /// has to stop for it.
    pub fn concurrency_limit_view(&self) -> ProtocolResult<ConcurrencyLimitView> {
        let limit = *self.lock_max_concurrent_tasks();
        let load = self.control.total_load()?;
        Ok(ConcurrencyLimitView {
            limit,
            slots_held: load.compute,
            overshoot: load.overshoot(limit.unwrap_or(u32::MAX)),
            waiting_to_stop: load.stop_requested + load.stopping,
        })
    }

    /// Changes the limit in the running control plane; it lasts until the
    /// control plane restarts, when the setting applies again.
    pub fn change_concurrency_limit(&self, limit: u32) -> ProtocolResult<ConcurrencyLimitView> {
        if !(1..=MAX_CONCURRENT_TASKS_LIMIT).contains(&limit) {
            return Err(ProtocolError::bad_request(
                "invalid_concurrency_limit",
                format!("the limit of concurrent tasks must be 1..={MAX_CONCURRENT_TASKS_LIMIT}"),
            ));
        }
        self.set_max_concurrent_tasks(Some(limit));
        self.concurrency_limit_view()
    }

    /// The limit as the safe-point decision reads it.
    pub(super) fn shared_limit(&self) -> Option<u32> {
        *self.lock_max_concurrent_tasks()
    }

    /// Asks running tasks to stop at a safe point while more tasks hold slots
    /// than the limit allows.
    ///
    /// Every task that may be asked gets the request, though the overshoot
    /// can be smaller: the first safe points to arrive are accepted, and the
    /// control plane takes the request back from the rest (see
    /// [`crate::orchestration::ControlPlane::decide_limit_stop`]). Nothing is
    /// asked while earlier requests wait for their safe point. Failures are
    /// logged; the next pass retries.
    pub fn limit_stop_pass(&self) {
        let Some(limit) = self.shared_limit() else {
            return;
        };
        let load = match self.control.total_load() {
            Ok(load) => load,
            Err(error) => {
                tracing::warn!(%error, "limit: cannot read the load");
                return;
            }
        };
        if load.overshoot(limit) <= load.stop_requested {
            return;
        }
        let candidates = match self.control.list_tasks() {
            Ok(tasks) => tasks,
            Err(error) => {
                tracing::warn!(%error, "limit: cannot list tasks");
                return;
            }
        };
        for task in candidates
            .into_iter()
            .filter(|task| task.limit_stop_candidate())
        {
            match self
                .control
                .request_limit_stop(&task.task_id, task.generation)
            {
                Ok(outcome) if outcome.replayed => {}
                Ok(_) => {
                    tracing::info!(task = %task.task_id, "limit: asked the runner to stop");
                    self.notify(&task.task_id);
                }
                // The owner acted first, or the task moved on.
                Err(
                    error @ (OrchestrationError::StaleGeneration { .. }
                    | OrchestrationError::IllegalCommand { .. }),
                ) => {
                    tracing::debug!(task = %task.task_id, %error, "limit: overtaken");
                }
                Err(error) => {
                    tracing::warn!(task = %task.task_id, %error, "limit: could not ask the task to stop");
                }
            }
        }
    }

    /// Backend availability and the global task budget are separate inputs
    /// to the scheduler; neither is derived from the other's current load.
    pub(super) fn scheduling_policy(&self, backend_id: &BackendId) -> SchedulingPolicy {
        let backend = match self.launch.check_backend(backend_id) {
            Ok(()) => BackendAvailability::Available,
            Err(error) => BackendAvailability::Unavailable(error.to_string()),
        };
        SchedulingPolicy {
            backend,
            max_concurrent_tasks: self.shared_limit(),
        }
    }
}
