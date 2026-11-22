//! The details of a task's execution for owners — its context variables,
//! the conductor's summary and activity, a failed step's problem — from the
//! runner through the relay: the task's own while it runs, a read-only
//! runner's while it is stopped (D63).
//!
//! The same shape as the execution events (see [`super::events`]); only
//! the payload differs.

use narsor_protocol::display::without_unsafe_chars;

use super::events::not_running;
use super::reader::ReaderReady;
use super::relay::RelayGuard;
use super::ReadBlocker;
use crate::controller_protocol::credentials::RunnerIdentity;
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::ProtocolService;
use crate::controller_protocol::wire::*;
use crate::domain::TaskId;
use crate::orchestration::OwnerRef;

impl ProtocolService {
    /// The details of the task's execution that only its storage holds.
    ///
    /// A live runner answers through the relay and may fail to in time
    /// (`503`, retryable). A stopped task is read by a read-only runner the
    /// control plane has the controller start (D63): until it is connected
    /// the answer is `reader_starting`, and the client asks again. Every
    /// other state is an ordinary answer saying why nothing can be read.
    pub async fn execution_details(
        &self,
        owner: &OwnerRef,
        task_id: &str,
    ) -> ProtocolResult<ExecutionDetailsResponse> {
        let record = self.owned_task(owner, task_id)?;
        self.note_details_read(&record);
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
            None => self.relayed_details(&record.task_id).await,
            Some(ReadBlocker::Unavailable(ConversationUnavailableReason::Archived)) => {
                Ok(unavailable(
                    ConversationUnavailableReason::Archived,
                    "the task is archived: its storage was deleted",
                ))
            }
            Some(ReadBlocker::Unavailable(reason)) => Ok(unavailable(reason, not_running(reason))),
            Some(ReadBlocker::Reader(reason)) => match self.reader_ready(&record).await? {
                ReaderReady::Ready => self.relayed_details(&record.task_id).await,
                ReaderReady::Starting => Ok(unavailable(
                    ConversationUnavailableReason::ReaderStarting,
                    not_running(ConversationUnavailableReason::ReaderStarting),
                )),
                ReaderReady::NotReadable => Ok(unavailable(reason, not_running(reason))),
            },
        }
    }

    /// Asks the live runner and waits for its upload.
    async fn relayed_details(&self, task_id: &TaskId) -> ProtocolResult<ExecutionDetailsResponse> {
        let (request_id, answer) =
            self.lock_detail_reads()
                .open("details", task_id.clone(), |request_id| {
                    ExecutionDetailsRequest { request_id }
                });
        // Wakes the runner's long-poll, which then sees the request.
        let guard = RelayGuard::new(&self.detail_reads, request_id);
        self.notify(task_id);
        let details = tokio::time::timeout(self.config.conversation_page_timeout, answer).await;
        drop(guard);
        match details {
            Ok(Ok(details)) => Ok(ExecutionDetailsResponse::Available {
                details: Box::new(details),
                source: ExecutionEventsSource::Runner,
            }),
            Ok(Err(_)) => Err(ProtocolError::unavailable(
                "runner_read_failed",
                "the runner could not read the execution's details",
            )),
            Err(_) => Err(ProtocolError::unavailable(
                "runner_timeout",
                "the runner did not answer the details request in time",
            )),
        }
    }

    /// The details the runner uploads for a waiting owner. Only a request
    /// of the runner's own task can be answered. What a runner says is not
    /// trusted with a terminal: the texts are cleaned here, on the way in.
    pub fn runner_execution_details(
        &self,
        identity: &RunnerIdentity,
        request_id: &str,
        details: ExecutionDetails,
    ) -> ProtocolResult<ConversationPageAccepted> {
        self.credentials
            .touch_runner(identity, None)?
            .ok_or(ProtocolError::Unauthorized)?;
        let waiting = self.lock_detail_reads().take(request_id, &identity.task_id);
        let delivered = waiting.is_some_and(|(_, reply)| reply.send(cleaned(details)).is_ok());
        Ok(ConversationPageAccepted { delivered })
    }
}

/// `details` with nothing a terminal would act on in its texts. A value is
/// printed as JSON, which escapes it; its name is not.
fn cleaned(mut details: ExecutionDetails) -> ExecutionDetails {
    details.variables = details
        .variables
        .into_iter()
        .map(|(name, mut value)| {
            narsor_protocol::display::without_unsafe_chars_json(&mut value);
            (without_unsafe_chars(&name), value)
        })
        .collect();
    if let Some(problem) = &mut details.problem {
        problem.reason = without_unsafe_chars(&problem.reason);
        problem.episode = problem.episode.as_deref().map(without_unsafe_chars);
    }
    if let Some(annotation) = &mut details.annotation {
        annotation.summary = without_unsafe_chars(&annotation.summary);
        annotation.recommendation = annotation
            .recommendation
            .as_deref()
            .map(without_unsafe_chars);
    }
    if let Some(activity) = &mut details.conductor {
        activity.episode = activity.episode.as_deref().map(without_unsafe_chars);
    }
    details
}

fn unavailable(
    reason: ConversationUnavailableReason,
    message: impl Into<String>,
) -> ExecutionDetailsResponse {
    ExecutionDetailsResponse::Unavailable {
        reason,
        message: message.into(),
    }
}
