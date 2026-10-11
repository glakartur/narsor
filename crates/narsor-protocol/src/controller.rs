//! Shared controller protocol values that do not depend on service internals.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Freshness of a value the control plane only observed.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    Live,
    Stale,
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
    #[serde(other)]
    Unrecognized,
}

/// Current lease holder of a task's backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControllerPresence {
    pub controller_id: String,
    pub lease_epoch: u64,
    pub lease_expires_at: DateTime<Utc>,
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    pub freshness: Freshness,
}

/// A manual transition a workflow declares from its current state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransitionOffer {
    pub from_state: String,
    pub exits: Vec<TransitionExit>,
    pub override_allowed: bool,
    #[serde(default)]
    pub override_targets: Vec<String>,
}

/// One declared transition out of a workflow state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransitionExit {
    pub label: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

pub const CONVERSATION_UNAVAILABLE: &str = "conversation_unavailable";
pub const CONVERSATION_RECORD_UNREADABLE: &str = "conversation_record_unreadable";
pub const CONVERSATION_RECORD_UNREADABLE_PREFIX: &str =
    "the conversation record could not be read: ";

#[cfg(test)]
mod tests {
    use super::{ConversationUnavailableReason, Freshness};

    #[test]
    fn unknown_conversation_reasons_keep_the_forward_compatible_fallback() {
        let reason: ConversationUnavailableReason =
            serde_json::from_str("\"new_server_reason\"").unwrap();
        assert_eq!(reason, ConversationUnavailableReason::Unrecognized);
    }

    #[test]
    fn freshness_keeps_snake_case_wire_values() {
        assert_eq!(serde_json::to_string(&Freshness::Live).unwrap(), "\"live\"");
        assert_eq!(
            serde_json::to_string(&Freshness::Stale).unwrap(),
            "\"stale\""
        );
    }
}
