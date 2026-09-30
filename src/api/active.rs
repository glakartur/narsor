//! The engine passes the daemon drives in the background, by execution.
//!
//! Shared between the API's handlers and the daemon's startup hook, so a pass
//! the hook spawns (a reprompt a restart cut short, U4) can be interrupted
//! through `POST /executions/:id/interrupt` exactly like one a request
//! started. Advisory conductor invocations are deliberately *not* here: they
//! have a registry of their own ([`crate::conductor::AdvisoryInterrupts`]),
//! so they never replace the handle of a pass (KTD10).

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::task::AbortHandle;

/// Abort handles of the background engine passes, one per execution.
#[derive(Debug, Clone, Default)]
pub struct ActiveTasks {
    tasks: Arc<Mutex<HashMap<String, AbortHandle>>>,
}

impl ActiveTasks {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawns `future` as the background pass of `execution_id`, replacing
    /// the handle of an earlier one.
    pub fn spawn<F>(&self, execution_id: String, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let task = tokio::spawn(future);
        self.lock().insert(execution_id, task.abort_handle());
    }

    /// The handle of the pass of `execution_id`, if one was spawned.
    pub fn get(&self, execution_id: &str) -> Option<AbortHandle> {
        self.lock().get(execution_id).cloned()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, AbortHandle>> {
        self.tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
