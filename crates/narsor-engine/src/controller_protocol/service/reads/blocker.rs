//! Whether a read of a task's content can go to its live runner.

use chrono::{DateTime, Utc};

use crate::controller_protocol::error::ProtocolResult;
use crate::controller_protocol::service::ProtocolService;
use crate::controller_protocol::wire::*;
use crate::orchestration::{ConversationAvailability, RunnerState, TaskRecord};

impl ProtocolService {
    /// Why the task's own runner cannot be asked for content right now (a
    /// conversation page, execution events), or `None` when it is live.
    ///
    /// A stopped or interrupted task has none, and its content is read by a
    /// read-only runner ([`ReadBlocker::Reader`], D63) — never by the control
    /// plane reading storage, and never by starting the task's own runner.
    /// `Unknown` is not read that way: its previous runner may still be
    /// alive and hold the storage.
    pub(super) fn live_read_blocker(
        &self,
        record: &TaskRecord,
        now: DateTime<Utc>,
    ) -> ProtocolResult<Option<ReadBlocker>> {
        use ConversationUnavailableReason as Reason;
        let runner = record.runner.as_ref();
        Ok(match record.runner_state {
            RunnerState::Archiving | RunnerState::Archived => {
                Some(ReadBlocker::Unavailable(Reason::Archived))
            }
            RunnerState::Stopped => Some(ReadBlocker::Reader(Reason::Paused)),
            RunnerState::Interrupted => Some(ReadBlocker::Reader(Reason::Interrupted)),
            RunnerState::Unknown => Some(ReadBlocker::Unavailable(Reason::Interrupted)),
            RunnerState::Queued
            | RunnerState::Provisioning
            | RunnerState::Running
            | RunnerState::PauseRequested => match runner {
                None => Some(ReadBlocker::Unavailable(Reason::NotStarted)),
                Some(runner) if self.runner_freshness(record, runner, now)? == Freshness::Stale => {
                    Some(ReadBlocker::Unavailable(Reason::RunnerUnreachable))
                }
                Some(runner) => match &runner.snapshot.conversation {
                    ConversationAvailability::Live { .. } => None,
                    ConversationAvailability::Unavailable { .. } => {
                        Some(ReadBlocker::Unavailable(Reason::RunnerReported))
                    }
                },
            },
        })
    }
}

/// Why a read of a task's content cannot go to its live runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReadBlocker {
    /// Nobody can serve the read now; the reason says why.
    Unavailable(ConversationUnavailableReason),
    /// The task has no runner of its own and is one a read-only runner may
    /// read (`Stopped`, `Interrupted`, D63). The reason is what to say if the
    /// task stops being one meanwhile.
    Reader(ConversationUnavailableReason),
}
