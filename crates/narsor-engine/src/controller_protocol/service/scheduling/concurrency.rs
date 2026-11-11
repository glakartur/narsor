//! Changing the limit of tasks processed at once.

use crate::controller_protocol::service::ProtocolService;

impl ProtocolService {
    /// Changes the limit of tasks processed at once. Waiting tasks start at
    /// once under a higher limit; a lower one stops new starts, takes back
    /// reservations over it, and asks running tasks to stop at a safe point.
    pub fn set_max_concurrent_tasks(&self, limit: Option<u32>) {
        *self.lock_max_concurrent_tasks() = limit;
        self.reschedule_all();
        self.limit_stop_pass();
    }

    pub(crate) fn lock_max_concurrent_tasks(&self) -> std::sync::MutexGuard<'_, Option<u32>> {
        self.max_concurrent_tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
