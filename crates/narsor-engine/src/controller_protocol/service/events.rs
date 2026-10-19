//! Raw execution events for owners (U4): from the runner through the relay
//! — the task's own while it runs, a read-only runner's while it is stopped
//! (D63). The control plane never reads task storage itself.

use super::reader::ReaderReady;
use super::relay::RelayGuard;
use super::{ProtocolService, ReadBlocker, DEFAULT_EVENTS_LIMIT, MAX_EVENTS_LIMIT};
use crate::controller_protocol::credentials::RunnerIdentity;
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::wire::*;
use crate::domain::TaskId;
use crate::orchestration::OwnerRef;

impl ProtocolService {
    /// The execution's events after `query.after`, oldest first, at most
    /// `query.limit` of them.
    ///
    /// A live runner answers through the relay and may fail to in time
    /// (`503`, retryable). A stopped task is read by a read-only runner the
    /// control plane has the controller start (D63): until it is connected
    /// the answer is `reader_starting`, and the client asks again. Every
    /// other state is an ordinary answer saying why nothing can be read.
    pub async fn execution_events(
        &self,
        owner: &OwnerRef,
        task_id: &str,
        query: ExecutionEventsQuery,
    ) -> ProtocolResult<ExecutionEventsResponse> {
        let record = self.owned_task(owner, task_id)?;
        self.note_details_read(&record);
        let limit = query
            .limit
            .unwrap_or(DEFAULT_EVENTS_LIMIT)
            .clamp(1, MAX_EVENTS_LIMIT);
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
                self.relayed_events(&record.task_id, query.after, limit)
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
                    self.relayed_events(&record.task_id, query.after, limit)
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
        let guard = RelayGuard::new(&self.event_reads, request_id);
        self.notify(task_id);
        let page = tokio::time::timeout(self.config.conversation_page_timeout, answer).await;
        drop(guard);
        match page {
            Ok(Ok(page)) => Ok(ExecutionEventsResponse::Available {
                events: page.events,
                has_more: page.has_more,
                source: ExecutionEventsSource::Runner,
            }),
            Ok(Err(_)) => Err(ProtocolError::unavailable(
                "runner_read_failed",
                "the runner could not read the events",
            )),
            Err(_) => Err(ProtocolError::unavailable(
                "runner_timeout",
                "the runner did not answer the events request in time",
            )),
        }
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
        ConversationUnavailableReason::StorageUnreadable => "the task's storage could not be read",
        ConversationUnavailableReason::ReaderStarting => {
            "the task is stopped and its read-only runner is starting; ask again shortly"
        }
        ConversationUnavailableReason::Unrecognized => "it cannot be read right now",
    }
}
