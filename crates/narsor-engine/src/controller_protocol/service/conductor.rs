//! The state of a task's conductor for owners (U5): from the runner through
//! the relay — the task's own while it runs, a read-only runner's while it
//! is stopped (D63).
//!
//! The same shape as the execution events (see [`super::events`]); only
//! the payload differs.

use super::events::not_running;
use super::reader::ReaderReady;
use super::relay::RelayGuard;
use super::{ProtocolService, ReadBlocker};
use crate::conductor::{ConductorView, DEFAULT_CHAT_PAGE, MAX_CHAT_PAGE};
use crate::controller_protocol::credentials::RunnerIdentity;
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::wire::*;
use crate::domain::TaskId;
use crate::orchestration::OwnerRef;

impl ProtocolService {
    /// The conductor of the task's execution: its chat after `query.after`
    /// (at most `query.limit` entries), its decisions and proposals, and
    /// the diagnosis of a failed step.
    ///
    /// A live runner answers through the relay and may fail to in time
    /// (`503`, retryable). A stopped task is read by a read-only runner the
    /// control plane has the controller start (D63): until it is connected
    /// the answer is `reader_starting`, and the client asks again. Every
    /// other state is an ordinary answer saying why nothing can be read.
    pub async fn conductor(
        &self,
        owner: &OwnerRef,
        task_id: &str,
        query: ConductorReadQuery,
    ) -> ProtocolResult<ConductorReadResponse> {
        let record = self.owned_task(owner, task_id)?;
        self.note_details_read(&record);
        let limit = query
            .limit
            .map_or(DEFAULT_CHAT_PAGE, |limit| limit as usize)
            .clamp(1, MAX_CHAT_PAGE);
        let has_execution = record
            .runner
            .as_ref()
            .is_some_and(|runner| runner.snapshot.execution_cursor.execution_id.is_some());
        if !has_execution {
            return Ok(unavailable(
                ConversationUnavailableReason::NotStarted,
                "the task has no execution yet",
            ));
        }
        match self.live_read_blocker(&record, self.control.now())? {
            None => {
                self.relayed_conductor(&record.task_id, query.after, limit)
                    .await
            }
            Some(ReadBlocker::Unavailable(ConversationUnavailableReason::Archived)) => {
                Ok(unavailable(
                    ConversationUnavailableReason::Archived,
                    "the task is archived: its storage was deleted",
                ))
            }
            Some(ReadBlocker::Unavailable(reason)) => Ok(unavailable(reason, not_running(reason))),
            Some(ReadBlocker::Reader(reason)) => match self.reader_ready(&record).await? {
                ReaderReady::Ready => {
                    self.relayed_conductor(&record.task_id, query.after, limit)
                        .await
                }
                ReaderReady::Starting => Ok(unavailable(
                    ConversationUnavailableReason::ReaderStarting,
                    not_running(ConversationUnavailableReason::ReaderStarting),
                )),
                ReaderReady::NotReadable => Ok(unavailable(reason, not_running(reason))),
            },
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
