//! The attempt that waits for an exhausted usage limit to reset.

use chrono::{DateTime, Utc};

use super::*;

/// The `WorkflowPaused` reason of an execution parked on a usage limit.
pub const USAGE_LIMIT_PAUSE_REASON: &str = "usage_limit";

/// The prompt an attempt goes on with after the usage limit reset. Not the
/// step's prompt: the session already has it, and the work it did.
pub const USAGE_LIMIT_CONTINUATION_PROMPT: &str = "Your previous turn was interrupted \
because the usage limit was reached. The limit has reset. Continue the interrupted work \
from where you stopped, and finish the task as instructed before.";

/// An attempt whose harness stopped on an exhausted subscription usage limit,
/// kept in `metadata.engine.usage_wait` while the execution is parked.
///
/// Resuming the execution continues this attempt — same
/// `action_execution_id`, same attempt number — in the same harness session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageWait {
    /// The state whose action waits.
    pub state_id: StateId,
    /// The attempt that waits (D24 idempotency key).
    pub action_execution_id: String,
    /// Its number under the state's `retry_policy`, from 1.
    pub attempt: u32,
    /// The harness that stopped.
    pub harness: String,
    /// The harness's own session id, which the attempt continues.
    pub harness_session_id: String,
    /// When the exhausted window resets, when the harness said.
    #[serde(default)]
    pub resets_at: Option<DateTime<Utc>>,
}

/// The attempt `execution` waits with for a usage limit, if any.
pub fn usage_wait(execution: &WorkflowExecution) -> Result<Option<UsageWait>, EngineError> {
    read_engine_record(execution, METADATA_USAGE_WAIT_KEY)
}

/// The attempt `execution` waits with, when it belongs to `state_id`.
pub(crate) fn stored_usage_wait(
    execution: &WorkflowExecution,
    state_id: &str,
) -> Result<Option<UsageWait>, EngineError> {
    Ok(usage_wait(execution)?.filter(|wait| wait.state_id == state_id))
}
