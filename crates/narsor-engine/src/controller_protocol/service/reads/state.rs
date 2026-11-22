//! The complete state of a task's execution for owners — every section of
//! its context, the step locals, the conductor's state — from the runner
//! through the relay: the task's own while it runs, a read-only runner's
//! while it is stopped (D63).
//!
//! The same shape as the execution details (see [`super::details`]); only
//! the payload differs. Nothing is kept: every request asks the runner.

use narsor_protocol::display::{without_unsafe_chars_json, without_unsafe_chars_multiline};

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
    /// Every value the task's workflow sees now, read from its storage.
    ///
    /// Answers like [`Self::execution_details`]: a live runner through the
    /// relay (`503` when it cannot in time), a stopped task through a
    /// read-only runner (`reader_starting` until it is connected), and an
    /// ordinary answer saying why for every other state. Reading it counts
    /// as a details read for the slot of a stopped task.
    pub async fn execution_state(
        &self,
        owner: &OwnerRef,
        task_id: &str,
    ) -> ProtocolResult<ExecutionStateResponse> {
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
            None => self.relayed_state(&record.task_id).await,
            Some(ReadBlocker::Unavailable(ConversationUnavailableReason::Archived)) => {
                Ok(unavailable(
                    ConversationUnavailableReason::Archived,
                    "the task is archived: its storage was deleted",
                ))
            }
            Some(ReadBlocker::Unavailable(reason)) => Ok(unavailable(reason, not_running(reason))),
            Some(ReadBlocker::Reader(reason)) => match self.reader_ready(&record).await? {
                ReaderReady::Ready => self.relayed_state(&record.task_id).await,
                ReaderReady::Starting => Ok(unavailable(
                    ConversationUnavailableReason::ReaderStarting,
                    not_running(ConversationUnavailableReason::ReaderStarting),
                )),
                ReaderReady::NotReadable => Ok(unavailable(reason, not_running(reason))),
            },
        }
    }

    /// Asks the live runner and waits for its upload.
    async fn relayed_state(&self, task_id: &TaskId) -> ProtocolResult<ExecutionStateResponse> {
        let (request_id, answer) =
            self.lock_state_reads()
                .open("state", task_id.clone(), |request_id| {
                    ExecutionStateRequest { request_id }
                });
        // Wakes the runner's long-poll, which then sees the request.
        let guard = RelayGuard::new(&self.state_reads, request_id);
        self.notify(task_id);
        let state = tokio::time::timeout(self.config.conversation_page_timeout, answer).await;
        drop(guard);
        match state {
            Ok(Ok(state)) => Ok(ExecutionStateResponse::Available {
                state: Box::new(state),
                source: ExecutionEventsSource::Runner,
                read_at: self.control.now(),
            }),
            Ok(Err(_)) => Err(ProtocolError::unavailable(
                "runner_read_failed",
                "the runner could not read the execution's state",
            )),
            Err(_) => Err(ProtocolError::unavailable(
                "runner_timeout",
                "the runner did not answer the state request in time",
            )),
        }
    }

    /// The state the runner uploads for a waiting owner. Only a request of
    /// the runner's own task can be answered. What a runner says is not
    /// trusted with a terminal: the texts are cleaned here, on the way in.
    pub fn runner_execution_state(
        &self,
        identity: &RunnerIdentity,
        request_id: &str,
        state: ExecutionState,
    ) -> ProtocolResult<ConversationPageAccepted> {
        self.credentials
            .touch_runner(identity, None)?
            .ok_or(ProtocolError::Unauthorized)?;
        let waiting = self.lock_state_reads().take(request_id, &identity.task_id);
        let delivered = waiting.is_some_and(|(_, reply)| reply.send(cleaned(state)).is_ok());
        Ok(ConversationPageAccepted { delivered })
    }
}

/// `state` with nothing a terminal would act on in its names and texts.
/// Line breaks stay: a multi-line value is shown on its lines.
fn cleaned(state: ExecutionState) -> ExecutionState {
    ExecutionState {
        context: cleaned_section(state.context),
        locals: cleaned_section(state.locals),
        conductor: cleaned_section(state.conductor),
    }
}

fn cleaned_section(section: StateSection) -> StateSection {
    match section {
        StateSection::Available { mut value } => {
            without_unsafe_chars_json(&mut value);
            StateSection::Available { value }
        }
        StateSection::Absent { message } => StateSection::Absent {
            message: without_unsafe_chars_multiline(&message),
        },
        StateSection::Unreadable { reason } => StateSection::Unreadable {
            reason: without_unsafe_chars_multiline(&reason),
        },
    }
}

fn unavailable(
    reason: ConversationUnavailableReason,
    message: impl Into<String>,
) -> ExecutionStateResponse {
    ExecutionStateResponse::Unavailable {
        reason,
        message: message.into(),
    }
}
