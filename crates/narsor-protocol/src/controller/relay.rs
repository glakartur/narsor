//! Conversation and execution-event reads relayed through a runner.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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

pub const CONVERSATION_UNAVAILABLE: &str = "conversation_unavailable";

/// The code of the `409` a conversation page is refused with while the
/// read-only runner of a stopped task is not connected yet (D63): the client
/// asks again shortly. The page's own form of
/// [`ConversationUnavailableReason::ReaderStarting`].
pub const CONVERSATION_READER_STARTING: &str = "conversation_reader_starting";

pub const CONVERSATION_RECORD_UNREADABLE: &str = "conversation_record_unreadable";

pub const CONVERSATION_RECORD_UNREADABLE_PREFIX: &str =
    "the conversation record could not be read: ";
