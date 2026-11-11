//! Answers runners upload for relayed reads, and the relays' locks.

use crate::controller_protocol::credentials::RunnerIdentity;
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::{
    ConductorRelay, EventsRelay, PageRelay, ProtocolService,
};
use crate::controller_protocol::wire::*;

impl ProtocolService {
    /// A conversation page the runner uploads for a waiting owner. Only a
    /// request of the runner's own task can be answered.
    pub fn runner_conversation_page(
        &self,
        identity: &RunnerIdentity,
        request_id: &str,
        page: ConversationPage,
    ) -> ProtocolResult<ConversationPageAccepted> {
        self.credentials
            .touch_runner(identity, None)?
            .ok_or(ProtocolError::Unauthorized)?;
        let waiting = self.lock_pages().take(request_id, &identity.task_id);
        let delivered = waiting.is_some_and(|(_, reply)| reply.send(page).is_ok());
        Ok(ConversationPageAccepted { delivered })
    }

    /// The runner could not read what a waiting owner asked for: the
    /// request is taken out of the relay — so no poll offers it again — and
    /// its owner is answered "unavailable" now instead of at the timeout.
    /// Only a request of the runner's own task can be failed.
    pub fn runner_relay_failure(
        &self,
        identity: &RunnerIdentity,
        request_id: &str,
        reason: &str,
    ) -> ProtocolResult<ConversationPageAccepted> {
        self.credentials
            .touch_runner(identity, None)?
            .ok_or(ProtocolError::Unauthorized)?;
        tracing::warn!(task_id = %identity.task_id, request_id, reason, "a runner could not answer a read");
        // Dropping the owner's channel is the answer: the waiting side
        // reports the runner as unable to read.
        let task_id = &identity.task_id;
        let failed = self.lock_pages().take(request_id, task_id).is_some()
            || self.lock_event_reads().take(request_id, task_id).is_some()
            || self
                .lock_conductor_reads()
                .take(request_id, task_id)
                .is_some();
        Ok(ConversationPageAccepted { delivered: failed })
    }

    pub(crate) fn lock_pages(&self) -> std::sync::MutexGuard<'_, PageRelay> {
        self.pages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn lock_conductor_reads(&self) -> std::sync::MutexGuard<'_, ConductorRelay> {
        self.conductor_reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn lock_event_reads(&self) -> std::sync::MutexGuard<'_, EventsRelay> {
        self.event_reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Whether `value` can name an attempt: the Engine's ids are UUIDs, and
/// nothing outside this conservative set is ever forwarded to a runner.
pub(super) fn is_attempt_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value.starts_with('.')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}
