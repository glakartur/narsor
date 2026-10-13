//! The state of a task's conductor for owners (U5): from the live runner
//! through the relay, or — for the daemon's own local backend, when its
//! runner is not running — straight from the task's storage.
//!
//! The same shape as the execution events (see [`super::events`]); only
//! the payload differs.

use super::events::{is_safe_component, not_running};
use super::relay::RelayGuard;
use super::ProtocolService;
use crate::conductor::{ConductorView, DEFAULT_CHAT_PAGE, MAX_CHAT_PAGE};
use crate::controller_protocol::credentials::RunnerIdentity;
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::wire::*;
use crate::domain::TaskId;
use crate::orchestration::{OwnerRef, TaskRecord};

impl ProtocolService {
    /// The conductor of the task's execution: its chat after `query.after`
    /// (at most `query.limit` entries), its decisions and proposals, and
    /// the diagnosis of a failed step.
    ///
    /// A live runner answers through the relay and may fail to in time
    /// (`503`, retryable). Every other state is an ordinary answer: the
    /// daemon's local backend reads the task's own storage, with the
    /// conductor marked unavailable, and any other backend says why
    /// nothing can be read.
    pub async fn conductor(
        &self,
        owner: &OwnerRef,
        task_id: &str,
        query: ConductorReadQuery,
    ) -> ProtocolResult<ConductorReadResponse> {
        let record = self.owned_task(owner, task_id)?;
        let limit = query
            .limit
            .map_or(DEFAULT_CHAT_PAGE, |limit| limit as usize)
            .clamp(1, MAX_CHAT_PAGE);
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
        match self.live_read_blocker(&record, self.control.now())? {
            None => {
                self.relayed_conductor(&record.task_id, query.after, limit)
                    .await
            }
            Some(ConversationUnavailableReason::Archived) => Ok(unavailable(
                ConversationUnavailableReason::Archived,
                "the task is archived: its storage was deleted",
            )),
            Some(reason) => {
                self.stored_conductor(&record, &execution_id, reason, query.after, limit)
                    .await
            }
        }
    }

    /// Asks the live runner and waits for its upload.
    async fn relayed_conductor(
        &self,
        task_id: &TaskId,
        after: u64,
        limit: usize,
    ) -> ProtocolResult<ConductorReadResponse> {
        let (request_id, answer) =
            self.lock_conductor_reads()
                .open("conductor", task_id.clone(), |request_id| {
                    ConductorReadRequest {
                        request_id,
                        after,
                        limit: u32::try_from(limit).unwrap_or(u32::MAX),
                    }
                });
        // Wakes the runner's long-poll, which then sees the request.
        let guard = RelayGuard::new(&self.conductor_reads, request_id);
        self.notify(task_id);
        let view = tokio::time::timeout(self.config.conversation_page_timeout, answer).await;
        drop(guard);
        match view {
            Ok(Ok(view)) => Ok(ConductorReadResponse::Available {
                conductor: Box::new(wire_conductor_view(view)?),
                source: ExecutionEventsSource::Runner,
            }),
            Ok(Err(_)) => Err(ProtocolError::unavailable(
                "runner_read_failed",
                "the runner could not read the conductor's state",
            )),
            Err(_) => Err(ProtocolError::unavailable(
                "runner_timeout",
                "the runner did not answer the conductor request in time",
            )),
        }
    }

    /// Reads the task's storage, when this host holds it.
    async fn stored_conductor(
        &self,
        record: &TaskRecord,
        execution_id: &str,
        reason: ConversationUnavailableReason,
        after: u64,
        limit: usize,
    ) -> ProtocolResult<ConductorReadResponse> {
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
            ConductorView::read(&store, &execution_id, after, limit).map(Some)
        })
        .await
        .map_err(|error| ProtocolError::Internal(error.to_string()))?
        .map_err(|error| ProtocolError::Internal(error.to_string()))?;
        let Some(view) = read else {
            return Ok(unavailable(
                reason,
                "the task's storage holds no such execution",
            ));
        };
        let view = view.without_conductor(not_running(reason)).cleaned();
        Ok(ConductorReadResponse::Available {
            conductor: Box::new(wire_conductor_view(view)?),
            source: ExecutionEventsSource::Storage,
        })
    }

    /// A conductor view the runner uploads for a waiting owner. Only a
    /// request of the runner's own task can be answered. What a runner
    /// says is not trusted with a terminal: the texts are cleaned here, on
    /// the way in.
    pub fn runner_conductor_read(
        &self,
        identity: &RunnerIdentity,
        request_id: &str,
        view: ConductorView,
    ) -> ProtocolResult<ConversationPageAccepted> {
        self.credentials
            .touch_runner(identity, None)?
            .ok_or(ProtocolError::Unauthorized)?;
        let waiting = self
            .lock_conductor_reads()
            .take(request_id, &identity.task_id);
        let delivered = waiting.is_some_and(|(_, reply)| reply.send(view.cleaned()).is_ok());
        Ok(ConversationPageAccepted { delivered })
    }
}

fn wire_conductor_view(
    view: ConductorView,
) -> ProtocolResult<narsor_protocol::controller::ConductorView> {
    let value =
        serde_json::to_value(view).map_err(|error| ProtocolError::Internal(error.to_string()))?;
    serde_json::from_value(value).map_err(|error| ProtocolError::Internal(error.to_string()))
}

fn unavailable(
    reason: ConversationUnavailableReason,
    message: impl Into<String>,
) -> ConductorReadResponse {
    ConductorReadResponse::Unavailable {
        reason,
        message: message.into(),
    }
}
