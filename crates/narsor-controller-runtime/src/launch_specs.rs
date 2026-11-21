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
//!
//! The same cache hands such a backend a fresh runner credential when it
//! finally starts the runner: an image build can outlast the credential
//! issued at creation, so the backend asks for one
//! ([`LaunchSpecs::take_credential_or_request`]) and the controller issues it
//! for the same incarnation at its next observation of the task
//! ([`LaunchSpecs::credential_request`], [`LaunchSpecs::provide_credential`]).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use narsor_model::domain::TaskLaunchSpec;
use narsor_protocol::identifier::TaskId;
use serde_json::Value;

use crate::backend::{validate_launch_input, RunnerCredential};

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
    /// Fresh runner credentials a backend asked for, per task: the
    /// generation and incarnation, and the credential once issued.
    credentials: HashMap<TaskId, CredentialSlot>,
}

#[derive(Debug)]
struct CredentialSlot {
    generation: u64,
    incarnation: String,
    issued: Option<RunnerCredential>,
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

    /// For a backend about to start a runner: the fresh credential issued
    /// for `incarnation` of `task_id` at `generation`, taken (it is handed
    /// out once), or `None` after noting that one is wanted. The controller
    /// issues a wanted credential at its next observation of the task.
    pub fn take_credential_or_request(
        &self,
        task_id: &TaskId,
        generation: u64,
        incarnation: &str,
    ) -> Option<RunnerCredential> {
        let mut inner = self.lock();
        if let Some(slot) = inner.credentials.get(task_id) {
            if slot.generation == generation
                && slot.incarnation == incarnation
                && slot.issued.is_some()
            {
                return inner
                    .credentials
                    .remove(task_id)
                    .and_then(|slot| slot.issued);
            }
        }
        inner.credentials.insert(
            task_id.clone(),
            CredentialSlot {
                generation,
                incarnation: incarnation.to_string(),
                issued: None,
            },
        );
        None
    }

    /// The incarnation a backend wants a fresh credential for, when it asked
    /// for one of `task_id` at `generation` that was not issued yet.
    pub fn credential_request(&self, task_id: &TaskId, generation: u64) -> Option<String> {
        self.lock()
            .credentials
            .get(task_id)
            .filter(|slot| slot.generation == generation && slot.issued.is_none())
            .map(|slot| slot.incarnation.clone())
    }

    /// Hands the backend the credential issued for its request of
    /// `incarnation` of `task_id` at `generation`; nothing when the request
    /// changed meanwhile.
    pub fn provide_credential(
        &self,
        task_id: &TaskId,
        generation: u64,
        incarnation: &str,
        credential: RunnerCredential,
    ) {
        if let Some(slot) = self.lock().credentials.get_mut(task_id) {
            if slot.generation == generation && slot.incarnation == incarnation {
                slot.issued = Some(credential);
            }
        }
    }

    /// Keeps only the tasks `keep` accepts.
    pub(crate) fn retain(&self, keep: impl Fn(&TaskId) -> bool) {
        let mut inner = self.lock();
        inner.specs.retain(|task, _| keep(task));
        inner.requested.retain(|task, _| keep(task));
        inner.credentials.retain(|task, _| keep(task));
    }
}

/// Checks a fetched launch spec: a supported, well-formed spec whose
/// content digest, recomputed here, equals `expected` (the digest the
/// command carries). The spec's own `digest` field is not trusted.
pub fn verify_launch_spec(spec: &Value, expected: &str) -> Result<(), String> {
    validate_launch_input(spec)?;
    let typed: TaskLaunchSpec = serde_json::from_value(spec.clone())
        .map_err(|error| format!("control plane returned an invalid launch spec: {error}"))?;
    typed.check_pinned_digest(expected).map_err(|problem| {
        format!("the control plane returned a launch spec its command does not name: {problem}")
    })
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
