//! Conversation, execution-event, execution-details and execution-state
//! reads relayed through a runner.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::{ActiveProblem, AnnotationDto, ConductorActivity};
use crate::event::Event;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationPageRequest {
    pub request_id: String,
    pub attempt_id: String,
    pub from_entry: u64,
    pub byte_budget: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionEventsRequest {
    pub request_id: String,
    pub after: u64,
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionEventsPage {
    pub events: Vec<crate::event::Event>,
    pub has_more: bool,
}

/// A runner's request for the details of its execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionDetailsRequest {
    pub request_id: String,
}

/// What only a task's storage holds about its execution, beside what its
/// task record says: the context variables, the conductor's summary of the
/// question it waits on, what the conductor works on, and the problem a
/// failed step leaves the owner.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ExecutionDetails {
    #[serde(default)]
    pub variables: HashMap<String, Value>,
    /// The conductor's report on the step the execution is failed on, when
    /// it leaves a problem to the owner. An output problem is the pending
    /// input's, and in the task record already.
    #[serde(default)]
    pub problem: Option<ActiveProblem>,
    /// The conductor's summary of the pending input.
    #[serde(default)]
    pub annotation: Option<AnnotationDto>,
    /// What the conductor works on, or the diagnosis a failed step owes.
    #[serde(default)]
    pub conductor: Option<ConductorActivity>,
}

/// Result of the owner task's execution-details request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum ExecutionDetailsResponse {
    /// The details, read from the task's storage.
    Available {
        details: Box<ExecutionDetails>,
        /// Where they were read from.
        source: ExecutionEventsSource,
    },
    /// Not readable right now.
    Unavailable {
        /// Why the details cannot be read.
        reason: ConversationUnavailableReason,
        /// Human-readable explanation.
        message: String,
    },
}

/// A runner's request for the complete state of its execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionStateRequest {
    pub request_id: String,
}

/// Every value the workflow of one execution sees now: the execution
/// context, the local values of each step and the conductor's state.
///
/// Each section is read and reported on its own, so a part that cannot be
/// read never looks empty.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecutionState {
    /// The execution context as stored, every section of it.
    pub context: StateSection,
    /// The local values of each step: `state_id -> { NAME: value }`.
    pub locals: StateSection,
    /// The conductor's state, with its generated title when it has one.
    pub conductor: StateSection,
}

/// One section of an [`ExecutionState`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum StateSection {
    /// The section, as raw JSON.
    Available { value: Value },
    /// The execution has nothing for this section.
    Absent { message: String },
    /// The section could not be read.
    Unreadable { reason: String },
}

/// Result of the owner task's execution-state request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum ExecutionStateResponse {
    /// The state, read from the task's storage.
    Available {
        state: Box<ExecutionState>,
        source: ExecutionEventsSource,
        read_at: DateTime<Utc>,
    },
    /// Not readable right now.
    Unavailable {
        reason: ConversationUnavailableReason,
        /// Human-readable explanation.
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayFailure {
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationPageAccepted {
    pub delivered: bool,
}

/// One attempt's conversation, summarized without transcript content.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationAttemptSummary {
    pub attempt_id: String,
    pub state_id: Option<String>,
    pub entries: u64,
    pub closed: bool,
    pub last_entry_at: Option<DateTime<Utc>>,
}

/// Whether a runner can serve its conversation and the summaries it holds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum ConversationAvailability {
    Live {
        attempts: Vec<ConversationAttemptSummary>,
    },
    Unavailable {
        reason: String,
    },
}

impl Default for ConversationAvailability {
    fn default() -> Self {
        Self::Live {
            attempts: Vec::new(),
        }
    }
}

/// Owner response describing whether the runner can serve its conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum ConversationResponse {
    Available {
        attempts: Vec<ConversationAttemptSummary>,
        observed_at: DateTime<Utc>,
        #[serde(default)]
        source: ExecutionEventsSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recorded_at: Option<DateTime<Utc>>,
    },
    Unavailable {
        reason: ConversationUnavailableReason,
        last_attempts: Vec<ConversationAttemptSummary>,
        observed_at: Option<DateTime<Utc>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

/// `GET /control/v1/tasks/:task_id/attempts/:attempt_id/conversation` query.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationPageQuery {
    /// First entry number wanted (default 0: from the start).
    #[serde(default)]
    pub from: u64,
    /// Soft cap on the raw bytes; bounded by the server.
    #[serde(default)]
    pub byte_budget: Option<u64>,
}

/// `GET /control/v1/tasks/:task_id/execution-events` query.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionEventsQuery {
    /// Events with a greater `sequence` (default 0: from the start).
    #[serde(default)]
    pub after: u64,
    /// Most events to return; bounded by the server.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Result of the owner task's execution-events request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum ExecutionEventsResponse {
    /// The events after the requested sequence.
    Available {
        /// The events, oldest first.
        events: Vec<Event>,
        /// True when more follow; ask again `after` the last one.
        has_more: bool,
        /// Where they were read from.
        source: ExecutionEventsSource,
    },
    /// Not readable right now.
    Unavailable {
        /// Why the event stream cannot be read.
        reason: ConversationUnavailableReason,
        /// Human-readable explanation.
        message: String,
    },
}

/// Why a task conversation cannot be served at the moment.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationUnavailableReason {
    NotStarted,
    Paused,
    Interrupted,
    Archived,
    RunnerUnreachable,
    RunnerReported,
    StorageUnreadable,
    /// The task is stopped and the control plane asked for a read-only
    /// runner to read it; the runner is not connected yet. Retry shortly.
    ReaderStarting,
    #[serde(other)]
    Unrecognized,
}

/// Where task events or a conversation page was read from.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionEventsSource {
    #[default]
    Runner,
    Storage,
}

/// Largest execution state body the control plane takes from a runner. The
/// runner checks the size before it sends, so an oversized state never meets
/// a closed connection.
pub const MAX_EXECUTION_STATE_BODY: usize = 4 * 1024 * 1024;

pub const CONVERSATION_UNAVAILABLE: &str = "conversation_unavailable";

/// The code of the `409` a conversation page is refused with while the
/// read-only runner of a stopped task is not connected yet (D63): the client
/// asks again shortly. The page's own form of
/// [`ConversationUnavailableReason::ReaderStarting`].
pub const CONVERSATION_READER_STARTING: &str = "conversation_reader_starting";

pub const CONVERSATION_RECORD_UNREADABLE: &str = "conversation_record_unreadable";

pub const CONVERSATION_RECORD_UNREADABLE_PREFIX: &str =
    "the conversation record could not be read: ";

/// Why an archived task has nothing to show of what lived in its storage:
/// the conversation of a step, the events, the conductor. The server puts it
/// on every step of an archived run; clients say it on the tabs it emptied.
pub const STORAGE_DELETED_AT_ARCHIVE: &str = "storage usunięty przy archiwizacji";
