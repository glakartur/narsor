//! Conductor proposal, decision, failure and read DTOs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::controller::{ConversationUnavailableReason, ExecutionEventsSource};

/// The kinds of action the conductor can propose to an owner.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    AnswerInput,
    MessageAgent,
    Retry,
    Transition,
    Abort,
}

/// A proposed action awaiting owner confirmation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProposedAction {
    pub kind: ProposalKind,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default, deserialize_with = "nullable_map")]
    pub values: serde_json::Map<String, Value>,
    #[serde(default)]
    pub text: Option<String>,
}

impl ProposedAction {
    pub fn describe(&self) -> String {
        match self.kind {
            ProposalKind::Retry => self.text.as_ref().map_or_else(
                || "Restart the failed step".to_string(),
                |note| format!("Restart the failed step with the note: {note}"),
            ),
            ProposalKind::Transition => format!(
                "Take the transition of output '{}'",
                self.label.as_deref().unwrap_or_default()
            ),
            ProposalKind::Abort => "Cancel the execution".to_string(),
            ProposalKind::MessageAgent => format!(
                "Tell the agent: {}",
                self.text.as_deref().unwrap_or_default()
            ),
            ProposalKind::AnswerInput => {
                let values = Value::Object(self.values.clone());
                match &self.label {
                    Some(label) => format!("Answer the pending input with '{label}', {values}"),
                    None => format!("Answer the pending input with {values}"),
                }
            }
        }
    }
}

fn nullable_map<'de, D>(deserializer: D) -> Result<serde_json::Map<String, Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<serde_json::Map<String, Value>>::deserialize(deserializer)?.unwrap_or_default())
}

/// The durable proposal record as shown to an owner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProposalRecord {
    #[serde(default)]
    pub episode: Option<String>,
    #[serde(default)]
    pub state_id: Option<String>,
    pub proposal: ProposedAction,
    #[serde(default)]
    pub confidence: Option<crate::event::Confidence>,
    #[serde(default)]
    pub rationale: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProposalState {
    Pending,
    Executed,
    Rejected,
    Stale,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProposalView {
    pub proposal_id: String,
    pub text: String,
    pub at: DateTime<Utc>,
    pub record: ProposalRecord,
    #[serde(default)]
    pub in_reply_to: Option<String>,
    pub state: ProposalState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DecisionView {
    pub decision_id: String,
    pub kind: String,
    pub text: String,
    pub at: DateTime<Utc>,
    #[serde(default)]
    pub state_id: Option<String>,
    #[serde(default)]
    pub confidence: Option<crate::event::Confidence>,
    #[serde(default)]
    pub rationale: Option<String>,
    pub undone: bool,
    pub undoable: bool,
}

/// Pending or queued conductor work shown in the client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConductorWorkView {
    pub purpose: crate::event::ConductorPurpose,
    pub trigger: crate::event::ConductorTrigger,
    #[serde(default)]
    pub episode: Option<String>,
    pub since: DateTime<Utc>,
    pub running: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FailureOutcome {
    Diagnosed {
        diagnosis: String,
        #[serde(default)]
        proposals: Vec<ProposedAction>,
    },
    Retried {
        note: String,
    },
    Unanswered {
        error: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FailureReport {
    pub key: String,
    pub episode: String,
    pub state_id: String,
    pub reason: String,
    pub outcome: FailureOutcome,
    pub concluded_at: DateTime<Utc>,
}

impl FailureReport {
    pub fn is_problem(&self) -> bool {
        !matches!(self.outcome, FailureOutcome::Retried { .. })
    }
}

/// The conductor view the client receives over REST or runner relay.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ConductorView {
    pub available: bool,
    #[serde(default)]
    pub unavailable_reason: Option<String>,
    #[serde(default)]
    pub harness: Option<String>,
    #[serde(default)]
    pub working: Vec<ConductorWorkView>,
    #[serde(default)]
    pub unanswered_chat: Vec<String>,
    #[serde(default)]
    pub failure: Option<FailureReport>,
    #[serde(default)]
    pub chat: Vec<crate::api::ChatEntryDto>,
    #[serde(default)]
    pub chat_has_more: bool,
    #[serde(default)]
    pub proposals: Vec<ProposalView>,
    #[serde(default)]
    pub decisions: Vec<DecisionView>,
}

impl ConductorView {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            unavailable_reason: Some(reason.into()),
            ..Self::default()
        }
    }

    pub fn available_on(mut self, harness: impl Into<String>) -> Self {
        self.available = true;
        self.unavailable_reason = None;
        self.harness = Some(harness.into());
        self
    }

    pub fn without_conductor(mut self, reason: impl Into<String>) -> Self {
        self.available = false;
        self.unavailable_reason = Some(reason.into());
        self
    }

    pub fn cleaned(self) -> Self {
        let Ok(mut value) = serde_json::to_value(&self) else {
            return self;
        };
        crate::display::without_unsafe_chars_json(&mut value);
        serde_json::from_value(value).unwrap_or(self)
    }

    pub fn has_more(&self) -> bool {
        self.chat_has_more
    }

    pub fn last_sequence(&self) -> u64 {
        self.chat.last().map_or(0, |entry| entry.sequence)
    }
}

/// Conductor state relayed from a runner or projected from local storage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum ConductorReadResponse {
    Available {
        conductor: Box<ConductorView>,
        source: ExecutionEventsSource,
    },
    Unavailable {
        reason: ConversationUnavailableReason,
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConductorReadRequest {
    pub request_id: String,
    pub after: u64,
    pub limit: u32,
}

/// `GET /control/v1/tasks/:task_id/conductor` query.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConductorReadQuery {
    /// Chat entries with a greater `sequence` (default 0: from the start).
    #[serde(default)]
    pub after: u64,
    /// Most chat entries to return; bounded by the server.
    #[serde(default)]
    pub limit: Option<u32>,
}
