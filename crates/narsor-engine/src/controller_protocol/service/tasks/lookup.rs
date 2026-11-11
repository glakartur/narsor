//! Finding an owner's tasks.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::ProtocolService;
use crate::controller_protocol::wire::*;
use crate::domain::{BackendId, TaskId};
use crate::orchestration::{OwnerRef, TaskRecord};

impl ProtocolService {
    /// The owner's tasks and the stream cursor at exactly that state.
    pub fn list_tasks(&self, owner: &OwnerRef) -> ProtocolResult<TaskListResponse> {
        self.list_tasks_where(owner, None)
    }

    /// The owner's tasks, narrowed by `archived` (see [`TaskListQuery`]).
    ///
    /// The cursor is the stream's position over *all* the owner's tasks,
    /// whatever the filter: a client that opens the stream with it misses
    /// nothing, and a task that archives later simply arrives as an event.
    /// Archived tasks come newest archive first (the Historia); the others
    /// keep the control plane's own order.
    pub fn list_tasks_where(
        &self,
        owner: &OwnerRef,
        archived: Option<bool>,
    ) -> ProtocolResult<TaskListResponse> {
        let mut cursor = StreamCursor::default();
        let mut kept = Vec::new();
        for record in self.control.list_tasks()? {
            if record.owner != *owner {
                continue;
            }
            cursor.advance(&record.task_id, record.revision);
            if archived.is_none_or(|wanted| wanted == record.runner_state.is_archival()) {
                kept.push(record);
            }
        }
        if archived == Some(true) {
            kept.sort_by(|a, b| {
                archived_at(b)
                    .cmp(&archived_at(a))
                    .then_with(|| b.task_id.as_str().cmp(a.task_id.as_str()))
            });
        }
        let now = self.control.now();
        let mut presences: HashMap<&BackendId, Option<ControllerPresence>> = HashMap::new();
        let mut tasks = Vec::with_capacity(kept.len());
        for record in &kept {
            let backend_id = &record.assignment.backend_id;
            let controller = match presences.get(backend_id) {
                Some(controller) => controller.clone(),
                None => {
                    let controller = self.controller_presence(backend_id, now)?;
                    presences.insert(backend_id, controller.clone());
                    controller
                }
            };
            tasks.push(self.summarize_with(record, now, controller)?);
        }
        Ok(TaskListResponse {
            tasks,
            cursor: cursor.to_string(),
        })
    }

    /// One of the owner's tasks.
    pub fn task(&self, owner: &OwnerRef, task_id: &str) -> ProtocolResult<TaskSummary> {
        let record = self.owned_task(owner, task_id)?;
        self.summarize(&record)
    }

    /// The owner's task, re-read and summarized.
    pub(super) fn owned_summary(
        &self,
        owner: &OwnerRef,
        task_id: &TaskId,
    ) -> ProtocolResult<TaskSummary> {
        let record = self.owned_task(owner, task_id.as_str())?;
        self.summarize(&record)
    }

    /// Whether `task_id` names a task of an owner other than `owner`.
    pub(super) fn someone_else_owns(
        &self,
        owner: &OwnerRef,
        task_id: &TaskId,
    ) -> ProtocolResult<bool> {
        match self.control.task(task_id) {
            Ok(record) => Ok(record.owner != *owner),
            Err(crate::orchestration::OrchestrationError::TaskNotFound(_)) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Whether `task_id` names one of the owner's tasks.
    pub fn owns(&self, owner: &OwnerRef, task_id: &TaskId) -> bool {
        self.control
            .task(task_id)
            .is_ok_and(|record| record.owner == *owner)
    }

    /// The owner's task, or `NotFound` — for an unknown id, a malformed one
    /// and another owner's alike.
    pub(crate) fn owned_task(&self, owner: &OwnerRef, task_id: &str) -> ProtocolResult<TaskRecord> {
        let task_id = TaskId::parse(task_id).map_err(|_| ProtocolError::NotFound)?;
        let record = self.control.task(&task_id)?;
        if record.owner != *owner {
            return Err(ProtocolError::NotFound);
        }
        Ok(record)
    }
}

/// When the owner archived the task, or created it when no tombstone
/// exists yet — the order of the Historia.
fn archived_at(record: &TaskRecord) -> DateTime<Utc> {
    record
        .archive
        .as_ref()
        .map_or(record.created_at, |tombstone| tombstone.requested_at)
}
