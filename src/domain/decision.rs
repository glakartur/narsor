//! Decision model
//!
//! Represents the decision-making process for workflow transitions.

use crate::domain::OutputLabel;
use serde::{Deserialize, Serialize};

/// A decision made by a decision provider
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    /// ID of the decision
    pub id: String,

    /// Which provider made the decision
    pub provider: DecisionProviderType,

    /// The chosen output label
    pub selected_output: OutputLabel,

    /// Reasoning behind the decision (optional)
    #[serde(default)]
    pub reasoning: Option<String>,

    /// Additional data from the decision process
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// Type of decision provider
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DecisionProviderType {
    /// Rule-based decision
    Rules,
    /// AI agent-based decision
    Agent,
    /// Human-based decision
    Human,
}

/// Result of a decision process
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionResult {
    /// Whether a decision was successfully made
    pub success: bool,

    /// The decision (if successful)
    pub decision: Option<Decision>,

    /// Error message (if failed)
    #[serde(default)]
    pub error: Option<String>,

    /// Whether to escalate to another provider
    #[serde(default)]
    pub escalate: bool,

    /// Target provider for escalation
    #[serde(default)]
    pub escalate_to: Option<DecisionProviderType>,
}

impl Decision {
    /// Creates a new decision
    pub fn new(
        provider: DecisionProviderType,
        selected_output: OutputLabel,
        reasoning: Option<String>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            provider,
            selected_output,
            reasoning,
            metadata: serde_json::json!({}),
        }
    }
}

impl DecisionResult {
    /// Creates a successful decision result
    pub fn success(decision: Decision) -> Self {
        Self {
            success: true,
            decision: Some(decision),
            error: None,
            escalate: false,
            escalate_to: None,
        }
    }

    /// Creates a failed decision result
    pub fn failure(error: String) -> Self {
        Self {
            success: false,
            decision: None,
            error: Some(error),
            escalate: false,
            escalate_to: None,
        }
    }

    /// Creates an escalation result
    pub fn escalate(to: DecisionProviderType, reason: Option<String>) -> Self {
        Self {
            success: false,
            decision: None,
            error: reason,
            escalate: true,
            escalate_to: Some(to),
        }
    }
}
