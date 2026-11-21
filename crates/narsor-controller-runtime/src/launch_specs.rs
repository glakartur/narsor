//! The launch specs a controller fetched, in memory (KTD3, KTD4).
//!
//! A command names its task's launch spec by digest only. The controller
//! fetches the spec when it creates a workload or starts a reader, checks
//! that its recomputed content digest equals the command's
//! ([`verify_launch_spec`]), and keeps it here per task and generation, so a
//! retried creation does not fetch it again.
//!
//! A backend that needs the spec after creation (the local Docker mode
//! starts its container once the image is built) shares this cache with the
//! controller ([`crate::backend::ExecutionBackend::launch_specs`]). After a
//! controller restart the cache is empty: the backend asks for the spec
//! ([`LaunchSpecs::get_or_request`]) and the controller fetches it at its
//! next observation of the task. Nothing is stored on disk.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use narsor_model::domain::TaskLaunchSpec;
use narsor_protocol::identifier::TaskId;
use serde_json::Value;

use crate::backend::validate_launch_input;

/// Verified launch specs per task and generation, and the ones a backend
/// asked for. Clones share one cache.
#[derive(Debug, Clone, Default)]
pub struct LaunchSpecs {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug, Default)]
struct Inner {
    /// One spec per task: the one of its newest generation.
    specs: HashMap<TaskId, (u64, Value)>,
    /// Generations a backend needs the spec of and did not find.
    requested: HashMap<TaskId, u64>,
}

impl LaunchSpecs {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The spec of `task_id` at `generation`, when it is here.
    pub fn get(&self, task_id: &TaskId, generation: u64) -> Option<Value> {
        self.lock()
            .specs
            .get(task_id)
            .filter(|(at, _)| *at == generation)
            .map(|(_, spec)| spec.clone())
    }

    /// Keeps `spec` as the one of `task_id` at `generation`, in place of
    /// any older generation's.
    pub fn insert(&self, task_id: &TaskId, generation: u64, spec: Value) {
        let mut inner = self.lock();
        if inner
            .specs
            .get(task_id)
            .is_some_and(|(at, _)| *at > generation)
        {
            return;
        }
        inner.specs.insert(task_id.clone(), (generation, spec));
        if inner
            .requested
            .get(task_id)
            .is_some_and(|at| *at <= generation)
        {
            inner.requested.remove(task_id);
        }
    }

    /// For a backend: the spec of `task_id` at `generation`, or `None` after
    /// noting that it is wanted. The controller fetches a wanted spec at its
    /// next observation of the task.
    pub fn get_or_request(&self, task_id: &TaskId, generation: u64) -> Option<Value> {
        if let Some(spec) = self.get(task_id, generation) {
            return Some(spec);
        }
        self.lock().requested.insert(task_id.clone(), generation);
        None
    }

    /// Whether a backend asked for the spec of `task_id` at `generation`.
    pub fn is_requested(&self, task_id: &TaskId, generation: u64) -> bool {
        self.lock().requested.get(task_id) == Some(&generation)
    }

    /// Keeps only the tasks `keep` accepts.
    pub(crate) fn retain(&self, keep: impl Fn(&TaskId) -> bool) {
        let mut inner = self.lock();
        inner.specs.retain(|task, _| keep(task));
        inner.requested.retain(|task, _| keep(task));
    }
}

/// Checks a fetched launch spec: a supported, well-formed spec whose
/// content digest, recomputed here, equals `expected` (the digest the
/// command carries). The spec's own `digest` field is not trusted.
pub fn verify_launch_spec(spec: &Value, expected: &str) -> Result<(), String> {
    validate_launch_input(spec)?;
    let typed: TaskLaunchSpec = serde_json::from_value(spec.clone())
        .map_err(|error| format!("control plane returned an invalid launch spec: {error}"))?;
    let digest = typed.compute_digest();
    if digest != expected {
        return Err(format!(
            "the launch spec the control plane returned has content digest {digest}, not the \
             digest {expected} its command names"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_newer_generation_replaces_the_spec_and_answers_the_request() {
        let specs = LaunchSpecs::default();
        let task = TaskId::parse("task-1").unwrap();

        assert_eq!(specs.get_or_request(&task, 2), None);
        assert!(specs.is_requested(&task, 2));
        specs.insert(&task, 1, Value::from("old"));
        assert!(
            specs.is_requested(&task, 2),
            "an older spec answers nothing"
        );
        specs.insert(&task, 2, Value::from("new"));

        assert!(!specs.is_requested(&task, 2));
        assert_eq!(specs.get(&task, 2), Some(Value::from("new")));
        assert_eq!(specs.get(&task, 1), None);
        specs.insert(&task, 1, Value::from("late"));
        assert_eq!(specs.get(&task, 2), Some(Value::from("new")), "never older");
    }
}
