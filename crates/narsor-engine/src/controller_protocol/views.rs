//! Owner-facing views of control-plane records, built by the server.

use crate::orchestration::{QueueStatus, QueuedInput, QueuedOperatorCommand};
use narsor_protocol::controller::{InputView, OperatorCommandView, QueueView};

impl From<&QueuedInput> for InputView {
    fn from(input: &QueuedInput) -> Self {
        Self {
            input_id: input.input_id.clone(),
            pending_input_id: input.pending_input_id.clone(),
            status: input.status,
            submitted_at: input.submitted_at,
            updated_at: input.updated_at,
            detail: input.detail.clone(),
        }
    }
}

impl From<&QueuedOperatorCommand> for OperatorCommandView {
    fn from(queued: &QueuedOperatorCommand) -> Self {
        Self {
            command_id: queued.command_id.clone(),
            command: queued.command.clone(),
            status: queued.status,
            submitted_at: queued.submitted_at,
            updated_at: queued.updated_at,
            detail: queued.detail.clone(),
        }
    }
}

impl From<&QueueStatus> for QueueView {
    fn from(status: &QueueStatus) -> Self {
        Self {
            message: status.reason.describe(),
            reason: status.reason.clone(),
            since: status.since,
        }
    }
}
