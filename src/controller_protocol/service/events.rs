//! Raw execution events for owners (U4): from the live runner through the
//! relay, or — for the daemon's own local backend, when its runner is not
//! running — straight from the task's storage.

use std::path::PathBuf;

use super::{ProtocolService, DEFAULT_EVENTS_LIMIT, MAX_EVENTS_LIMIT};
use crate::controller_protocol::credentials::RunnerIdentity;
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::wire::*;
use crate::domain::{BackendId, TaskId};
use crate::orchestration::{OwnerRef, TaskRecord};
use crate::persistence::ExecutionStore;

/// Where one backend keeps its tasks' storages on this host:
/// `<root>/<task id>/state` holds the task's execution store.
#[derive(Debug, Clone)]
pub(super) struct LocalTaskStorage {
    pub(super) backend_id: BackendId,
    pub(super) root: PathBuf,
}

impl LocalTaskStorage {
    pub(super) fn store_of(&self, task_id: &TaskId) -> ExecutionStore {
        ExecutionStore::new(self.root.join(task_id.as_str()).join("state"))
    }
}

impl ProtocolService {
    /// The execution's events after `query.after`, oldest first, at most
    /// `query.limit` of them.
    ///
    /// A live runner answers through the relay and may fail to in time
    /// (`503`, retryable). Every other state is an ordinary answer: the
    /// daemon's local backend reads the task's own storage; any other
    /// backend says why nothing can be read ("the runner is not running").
    pub async fn execution_events(
        &self,
        owner: &OwnerRef,
        task_id: &str,
        query: ExecutionEventsQuery,
    ) -> ProtocolResult<ExecutionEventsResponse> {
        let record = self.owned_task(owner, task_id)?;
        let limit = query
            .limit
            .unwrap_or(DEFAULT_EVENTS_LIMIT)
            .clamp(1, MAX_EVENTS_LIMIT);
        let execution_id = record
            .runner
            .as_ref()
            .and_then(|runner| runner.snapshot.execution_cursor.execution_id.clone());
        let Some(execution_id) = execution_id else {
            return Ok(unavailable(
                ConversationUnavailableReason::NotStarted,
                "the task has no execution yet",
            ));
        };
        let blocker = self.live_read_blocker(&record, self.control.now())?;
        match blocker {
            None => {
                self.relayed_events(&record.task_id, query.after, limit)
                    .await
            }
            Some(ConversationUnavailableReason::Archived) => Ok(unavailable(
                ConversationUnavailableReason::Archived,
                "the task is archived: its storage was deleted",
            )),
            Some(reason) => {
                self.stored_events(&record, &execution_id, reason, query.after, limit)
                    .await
            }
        }
    }

    /// Asks the live runner and waits for its upload.
    async fn relayed_events(
        &self,
        task_id: &TaskId,
        after: u64,
        limit: u32,
    ) -> ProtocolResult<ExecutionEventsResponse> {
        let (request_id, answer) =
            self.lock_event_reads()
                .open("events", task_id.clone(), |request_id| {
                    ExecutionEventsRequest {
                        request_id,
                        after,
                        limit,
                    }
                });
        // Wakes the runner's long-poll, which then sees the request.
        self.notify(task_id);
        let page = tokio::time::timeout(self.config.conversation_page_timeout, answer).await;
        self.lock_event_reads().close(&request_id);
        match page {
            Ok(Ok(page)) => Ok(ExecutionEventsResponse::Available {
                events: page.events,
                has_more: page.has_more,
                source: ExecutionEventsSource::Runner,
            }),
            _ => Err(ProtocolError::unavailable(
                "runner_timeout",
                "the runner did not answer the events request in time",
            )),
        }
    }

    /// Reads the task's storage, when this host holds it.
    async fn stored_events(
        &self,
        record: &TaskRecord,
        execution_id: &str,
        reason: ConversationUnavailableReason,
        after: u64,
        limit: u32,
    ) -> ProtocolResult<ExecutionEventsResponse> {
        let Some(local) = self
            .local_storage
            .as_ref()
            .filter(|local| local.backend_id == record.assignment.backend_id)
        else {
            return Ok(unavailable(reason, not_running(reason)));
        };
        // The id comes from a runner's report: it must never walk the path.
        if !is_safe_component(execution_id) {
            return Ok(unavailable(
                reason,
                "the execution id the runner reported is not valid",
            ));
        }
        let store = local.store_of(&record.task_id);
        let execution_id = execution_id.to_string();
        let read = tokio::task::spawn_blocking(move || {
            if !store.exists(&execution_id) {
                return Ok(None);
            }
            store
                .event_log(&execution_id)
                .read_after(after, limit as usize)
                .map(Some)
        })
        .await
        .map_err(|error| ProtocolError::Internal(error.to_string()))?
        .map_err(|error| ProtocolError::Internal(error.to_string()))?;
        let Some((events, has_more)) = read else {
            return Ok(unavailable(
                reason,
                "the task's storage holds no such execution",
            ));
        };
        Ok(ExecutionEventsResponse::Available {
            events,
            has_more,
            source: ExecutionEventsSource::Storage,
        })
    }

    /// An events page the runner uploads for a waiting owner. Only a request
    /// of the runner's own task can be answered; the page is cut to the
    /// limit that was asked for.
    pub fn runner_execution_events(
        &self,
        identity: &RunnerIdentity,
        request_id: &str,
        mut page: ExecutionEventsPage,
    ) -> ProtocolResult<ConversationPageAccepted> {
        self.credentials
            .touch_runner(identity, None)?
            .ok_or(ProtocolError::Unauthorized)?;
        let waiting = self.lock_event_reads().take(request_id, &identity.task_id);
        let delivered = waiting.is_some_and(|(request, reply)| {
            if page.events.len() > request.limit as usize {
                page.events.truncate(request.limit as usize);
                page.has_more = true;
            }
            reply.send(page).is_ok()
        });
        Ok(ConversationPageAccepted { delivered })
    }
}

fn unavailable(
    reason: ConversationUnavailableReason,
    message: impl Into<String>,
) -> ExecutionEventsResponse {
    ExecutionEventsResponse::Unavailable {
        reason,
        message: message.into(),
    }
}

/// Why nothing can be read, for people.
pub(super) fn not_running(reason: ConversationUnavailableReason) -> &'static str {
    match reason {
        ConversationUnavailableReason::NotStarted => "the task has not started yet",
        ConversationUnavailableReason::Paused => "the runner is not running: the task is paused",
        ConversationUnavailableReason::Interrupted => {
            "the runner is not running: the task was interrupted"
        }
        ConversationUnavailableReason::Archived => "the task is archived",
        ConversationUnavailableReason::RunnerUnreachable => {
            "the runner is not running: it has not been heard from lately"
        }
        ConversationUnavailableReason::RunnerReported => "the runner cannot serve it right now",
    }
}

/// A single path component: no separators, no traversal.
pub(super) fn is_safe_component(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}
