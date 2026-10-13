//! Execution timeline response DTOs.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One workflow action attempt on an execution timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub label: String,
    pub state_id: String,
    pub visit: u32,
    pub action_execution_id: Option<String>,
    pub action_type: Option<String>,
    pub status: PositionStatus,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub resumed_harness_session: Option<String>,
    pub harness_session_interrupted: bool,
    pub retry_pending: bool,
    pub conversation: ConversationState,
}

/// Outcome of one timeline position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionStatus {
    Succeeded,
    Failed,
    Interrupted,
    InProgress,
    AwaitingDecision,
}

/// Whether a position has a readable conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConversationState {
    Recorded,
    NotCreated { reason: String },
    NoRecord,
}

/// Where an event belongs on the timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "link", content = "value", rename_all = "snake_case")]
pub enum EventLink {
    Position(String),
    NoLink { reason: String },
}

/// Timeline positions, event links, and the default selection.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Timeline {
    pub positions: Vec<Position>,
    pub links: BTreeMap<u64, EventLink>,
    pub default_position: Option<String>,
}

impl Timeline {
    pub fn position(&self, label: &str) -> Option<&Position> {
        self.positions
            .iter()
            .find(|position| position.label == label)
    }

    pub fn link(&self, sequence: u64) -> Option<&EventLink> {
        self.links.get(&sequence)
    }
}
