//! The limit of concurrent tasks of a backend, as the operator changes it in
//! the running control plane.
//!
//! The effective limit is the ad hoc value when one is set, else the
//! backend file's `max_concurrent_tasks`, read fresh on every call. The ad
//! hoc value lives in memory only: a restart of the control plane drops it,
//! and the file is never written. A backend the file cannot supply — it is
//! unknown or unusable — admits nothing, whatever the ad hoc value says.
//!
//! A higher limit starts waiting tasks at once. A lower one stops new
//! starts at once, takes back reservations over the limit, and asks the
//! tasks that run to stop at a safe point; see
//! [`crate::orchestration::ControlPlane::decide_limit_stop`] for which
//! safe points are accepted.

use std::collections::{BTreeSet, HashMap};

use super::ProtocolService;
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::domain::BackendId;
use crate::environment::validate_concurrent_tasks;
use crate::orchestration::{BackendCapacity, OrchestrationError};
use narsor_protocol::controller::{BackendLimitSource, BackendLimitView};

/// The ad hoc limits of the backends, kept in memory.
pub(super) type LimitOverrides = std::sync::Mutex<HashMap<BackendId, u32>>;

impl ProtocolService {
    /// The backend's limit, where it comes from and what the tasks hold.
    pub fn backend_limit_view(&self, backend_id: &str) -> ProtocolResult<BackendLimitView> {
        let backend = self.known_backend(backend_id)?;
        self.limit_view(&backend)
    }

    /// Sets the ad hoc limit of `backend_id`; it has priority over the file
    /// until it is cleared or the control plane restarts. A value outside the
    /// range the file allows is refused and changes nothing.
    pub fn set_backend_limit(
        &self,
        backend_id: &str,
        limit: u32,
    ) -> ProtocolResult<BackendLimitView> {
        let backend = self.known_backend(backend_id)?;
        validate_concurrent_tasks(limit).map_err(|problem| {
            ProtocolError::bad_request(
                "invalid_backend_limit",
                format!("the limit of concurrent tasks {problem}"),
            )
        })?;
        self.lock_limit_overrides().insert(backend.clone(), limit);
        self.apply_limit_change(&backend);
        self.limit_view(&backend)
    }

    /// Removes the ad hoc limit: the backend file's value applies again.
    /// Clearing a limit that is not set changes nothing.
    pub fn clear_backend_limit(&self, backend_id: &str) -> ProtocolResult<BackendLimitView> {
        let backend = self.known_backend(backend_id)?;
        let removed = self.lock_limit_overrides().remove(&backend).is_some();
        if removed {
            self.apply_limit_change(&backend);
        }
        self.limit_view(&backend)
    }

    /// Asks running tasks to stop at a safe point while a backend holds more
    /// slots than its limit allows.
    ///
    /// Every task that may be asked gets the request, though the overshoot
    /// can be smaller: the first safe points to arrive are accepted, and the
    /// control plane takes the request back from the rest (see
    /// [`crate::orchestration::ControlPlane::decide_limit_stop`]). Nothing is
    /// asked while earlier requests wait for their safe point. Failures are
    /// logged; the next pass retries.
    pub fn limit_stop_pass(&self) {
        let tasks = match self.control.list_tasks() {
            Ok(tasks) => tasks,
            Err(error) => {
                tracing::warn!(%error, "limit: cannot list tasks");
                return;
            }
        };
        let backends: BTreeSet<BackendId> = tasks
            .iter()
            .map(|task| task.assignment.backend_id.clone())
            .collect();
        for backend in backends {
            self.limit_stop_backend(&backend);
        }
    }

    fn limit_stop_backend(&self, backend: &BackendId) {
        let BackendCapacity::Limit(limit) = self.capacity(backend) else {
            return;
        };
        let load = match self.control.backend_load(backend) {
            Ok(load) => load,
            Err(error) => {
                tracing::warn!(%backend, %error, "limit: cannot read the load");
                return;
            }
        };
        if load.overshoot(limit) <= load.stop_requested {
            return;
        }
        let candidates = match self.control.list_tasks() {
            Ok(tasks) => tasks,
            Err(error) => {
                tracing::warn!(%backend, %error, "limit: cannot list tasks");
                return;
            }
        };
        for task in candidates
            .into_iter()
            .filter(|task| task.assignment.backend_id == *backend && task.limit_stop_candidate())
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

    /// What follows a change of the limit, at once and not at the next tick
    /// of the periodic pass: waiting tasks start, reservations over the limit
    /// are taken back, and running tasks are asked to stop.
    fn apply_limit_change(&self, backend: &BackendId) {
        self.reschedule(backend);
        self.limit_stop_pass();
    }

    /// The backend's capacity: the ad hoc limit over the file's, and the
    /// file's failure over both.
    pub(super) fn effective_capacity(&self, backend_id: &BackendId) -> BackendCapacity {
        match self.launch.backend_limit(backend_id) {
            Ok(file) => BackendCapacity::Limit(
                self.lock_limit_overrides()
                    .get(backend_id)
                    .copied()
                    .unwrap_or(file),
            ),
            Err(error) => BackendCapacity::Unavailable(error.to_string()),
        }
    }

    /// `backend_id` as an id of a backend the control plane can use now.
    fn known_backend(&self, backend_id: &str) -> ProtocolResult<BackendId> {
        let backend = BackendId::parse(backend_id).map_err(|_| ProtocolError::NotFound)?;
        match self.launch.backend_limit(&backend) {
            Ok(_) => Ok(backend),
            Err(error) if error.is_internal() => Err(ProtocolError::Internal(error.to_string())),
            Err(_) => Err(ProtocolError::NotFound),
        }
    }

    fn limit_view(&self, backend: &BackendId) -> ProtocolResult<BackendLimitView> {
        let file_value = self
            .launch
            .backend_limit(backend)
            .map_err(|error| ProtocolError::Internal(error.to_string()))?;
        let override_value = self.lock_limit_overrides().get(backend).copied();
        let effective = override_value.unwrap_or(file_value);
        let load = self.control.backend_load(backend)?;
        Ok(BackendLimitView {
            backend_id: backend.to_string(),
            effective,
            source: if override_value.is_some() {
                BackendLimitSource::AdHoc
            } else {
                BackendLimitSource::File
            },
            file_value,
            override_value,
            slots_held: load.compute,
            overshoot: load.overshoot(effective),
            waiting_to_stop: load.stop_requested + load.stopping,
        })
    }

    fn lock_limit_overrides(&self) -> std::sync::MutexGuard<'_, HashMap<BackendId, u32>> {
        self.limit_overrides
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
