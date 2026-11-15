//! The deadline of a `wait` step (U4, PO4): a task whose `wait` step waits
//! past its `timeout` is handed the end of its wait.
//!
//! The deadline is on the task record ([`crate::orchestration::EventWait`],
//! as the runner reported it), so the periodic pass finds it also after a
//! restart of the daemon and while the task has no compute: the slot of a
//! waiting task is freed like the slot of one waiting for its owner. The
//! pass queues the timeout for the runner once per episode
//! ([`crate::orchestration::ControlPlane::queue_wait_timeout`]) and resumes
//! a stopped task to take it, as an owner's answer would; the runner's
//! engine then ends the step with its `timeout` label or fails it.

use crate::controller_protocol::service::ProtocolService;
use crate::domain::TaskId;
use crate::orchestration::InputStatus;

impl ProtocolService {
    /// Queues the timeout of every task waiting in a `wait` step past its
    /// deadline, and resumes the stopped ones. Returns the tasks this pass
    /// queued a timeout for. Failures are logged; the next pass retries
    /// (the queue is idempotent per episode, the resume per key).
    pub fn wait_deadline_pass(&self) -> Vec<TaskId> {
        let tasks = match self.control.list_tasks() {
            Ok(tasks) => tasks,
            Err(error) => {
                tracing::warn!(%error, "wait deadline: cannot list tasks");
                return Vec::new();
            }
        };
        let now = self.control.now();
        let mut queued = Vec::new();
        for task in tasks {
            let due = task
                .event_wait
                .as_ref()
                .is_some_and(|wait| wait.is_past_deadline(now));
            // A retry repairs a resume a pass queued a timeout for and
            // failed to make: the key of the work still says what to do.
            // Only work still queued for the wait the task is in counts: an
            // input of an ended episode never resumes a task.
            let stranded = task.event_wait.as_ref().and_then(|wait| {
                task.event_inputs
                    .iter()
                    .find(|input| {
                        input.status == InputStatus::Queued && input.episode_id == wait.episode_id
                    })
                    .cloned()
            });
            if !due && stranded.is_none() {
                continue;
            }
            let input = match self.control.queue_wait_timeout(&task.task_id) {
                Ok(Some(input)) => {
                    tracing::info!(task = %task.task_id, episode = %input.episode_id, "wait deadline: the wait timed out");
                    self.notify(&task.task_id);
                    queued.push(task.task_id.clone());
                    Some(input)
                }
                Ok(None) => stranded,
                Err(error) => {
                    tracing::warn!(task = %task.task_id, %error, "wait deadline: cannot queue the timeout");
                    continue;
                }
            };
            let Some(input) = input else {
                continue;
            };
            let current = match self.control.task(&task.task_id) {
                Ok(current) => current,
                Err(error) => {
                    tracing::warn!(task = %task.task_id, %error, "wait deadline: cannot read the task");
                    continue;
                }
            };
            if let Err(error) = self.resume_for_work(&current, &input.idempotency_key) {
                tracing::warn!(task = %task.task_id, %error, "wait deadline: cannot resume the task");
            }
        }
        queued
    }
}
