//! A task's timeline and the owner's event stream.

use super::view::current_position;
use crate::controller_protocol::error::ProtocolResult;
use crate::controller_protocol::service::ProtocolService;
use crate::controller_protocol::wire::*;
use crate::domain::TaskId;
use crate::orchestration::{OwnerRef, TaskEvent};

impl ProtocolService {
    /// The last timeline the task's runner reported.
    pub fn timeline(&self, owner: &OwnerRef, task_id: &str) -> ProtocolResult<TimelineResponse> {
        let record = self.owned_task(owner, task_id)?;
        let now = self.control.now();
        let runner = record.runner.as_ref();
        Ok(TimelineResponse {
            freshness: match runner {
                Some(runner) => self.runner_freshness(&record, runner, now)?,
                None => Freshness::Stale,
            },
            observed_at: runner.map(|runner| runner.snapshot.observed_at),
            execution_status: runner.map(|runner| runner.snapshot.execution_status),
            current_state: runner
                .and_then(|runner| runner.snapshot.execution_cursor.current_state.clone()),
            current_position: runner
                .and_then(|runner| runner.snapshot.timeline.as_ref())
                .and_then(current_position),
            timeline: runner.and_then(|runner| runner.snapshot.timeline.clone()),
            task_id: record.task_id.to_string(),
        })
    }

    /// The task's events after `after`.
    pub fn events(
        &self,
        owner: &OwnerRef,
        task_id: &str,
        after: u64,
    ) -> ProtocolResult<Vec<TaskEvent>> {
        let record = self.owned_task(owner, task_id)?;
        Ok(self.control.events_since(&record.task_id, after)?)
    }

    /// Every event the owner has not seen yet according to `cursor`, merged
    /// across tasks in commit-time order (each task's own order kept), with
    /// `cursor` advanced past them. `only` restricts it to one task.
    pub fn catch_up(
        &self,
        owner: &OwnerRef,
        only: Option<&TaskId>,
        cursor: &mut StreamCursor,
    ) -> ProtocolResult<Vec<ControlEventEnvelope>> {
        let task_ids = match only {
            Some(task_id) => vec![task_id.clone()],
            None => self.control.store().list_task_ids()?,
        };
        let mut per_task = Vec::new();
        for task_id in task_ids {
            let record = match self.control.task(&task_id) {
                Ok(record) => record,
                Err(crate::orchestration::OrchestrationError::TaskNotFound(_)) => continue,
                Err(error) => return Err(error.into()),
            };
            if record.owner != *owner {
                continue;
            }
            let events = self
                .control
                .events_since(&task_id, cursor.position(&task_id))?;
            if !events.is_empty() {
                per_task.push(std::collections::VecDeque::from(events));
            }
        }

        let mut merged = Vec::new();
        while let Some(next) = per_task
            .iter()
            .enumerate()
            .filter_map(|(index, events)| events.front().map(|event| (index, event.at)))
            .min_by_key(|(_, at)| *at)
            .map(|(index, _)| index)
        {
            let event = per_task[next]
                .pop_front()
                .expect("chosen queue is not empty");
            cursor.advance(&event.task_id, event.sequence);
            merged.push(ControlEventEnvelope {
                cursor: cursor.to_string(),
                event,
            });
        }
        Ok(merged)
    }
}
