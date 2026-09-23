//! Event model
//!
//! Represents events in the workflow execution history.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Unique identifier for an event
pub type EventId = String;

/// An event in the workflow execution history
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Unique identifier
    pub id: EventId,

    /// ID of the workflow execution this event belongs to
    pub workflow_execution_id: String,

    /// When the event occurred
    pub timestamp: DateTime<Utc>,

    /// The event data
    pub data: EventData,

    /// Sequence number (for ordering)
    pub sequence: u64,
}

/// Types of events that can occur
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventData {
    // Workflow lifecycle events
    WorkflowStarted {
        workflow_id: String,
        workflow_version: u32,
        inputs: serde_json::Value,
    },
    WorkflowPaused {
        reason: String,
    },
    WorkflowResumed,
    WorkflowCompleted {
        result: serde_json::Value,
    },
    WorkflowFailed {
        error: String,
    },
    WorkflowCancelled {
        reason: String,
    },

    // State events
    StateEntered {
        state_id: String,
    },
    StateExited {
        state_id: String,
        output_label: String,
    },

    // Action events
    ActionStarted {
        action_execution_id: String,
        state_id: String,
        action_type: String,
    },
    ActionCompleted {
        action_execution_id: String,
        output_label: String,
    },
    ActionFailed {
        action_execution_id: String,
        error: String,
    },
    ActionRetrying {
        action_execution_id: String,
        retry_count: u32,
    },

    // Agent events
    AgentSessionStarted {
        session_id: String,
        harness: String,
    },
    AgentSessionCompleted {
        session_id: String,
    },
    AgentSessionFailed {
        session_id: String,
        error: String,
    },
    AgentMessage {
        session_id: String,
        content: String,
        role: String,
    },

    // Decision events
    DecisionRequested {
        provider: String,
    },
    DecisionMade {
        provider: String,
        output_label: String,
        reasoning: Option<String>,
    },

    // Human interaction events
    HumanInputRequested {
        instruction: String,
    },
    HumanInputReceived {
        input: serde_json::Value,
    },
    HumanTransitionSelected {
        from_state: String,
        to_state: String,
    },

    // External events
    ExternalEventReceived {
        event_type: String,
        source: String,
        payload: serde_json::Value,
    },

    // Transition events
    TransitionTaken {
        from_state: String,
        to_state: String,
        output_label: String,
    },

    // Artifact events
    ArtifactCreated {
        artifact_id: String,
        artifact_type: String,
        location: String,
    },

    // Error events
    Error {
        message: String,
        context: serde_json::Value,
    },
}

impl Event {
    /// Creates a new event
    pub fn new(workflow_execution_id: String, data: EventData, sequence: u64) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            workflow_execution_id,
            timestamp: Utc::now(),
            data,
            sequence,
        }
    }

    /// Returns a human-readable description of the event
    /// A one-line description of the event for an operator.
    ///
    /// This is what the CLI's `events` listing and the TUI's event stream
    /// show, so every variant is spelled out and there is deliberately no
    /// catch-all: a new [`EventData`] variant must fail to compile here rather
    /// than quietly leaking a Rust `Debug` dump into the interface.
    pub fn description(&self) -> String {
        match &self.data {
            EventData::WorkflowStarted {
                workflow_id,
                workflow_version,
                ..
            } => {
                format!("Workflow '{workflow_id}' (version {workflow_version}) started")
            }
            EventData::WorkflowPaused { reason } => format!("Workflow paused: {reason}"),
            EventData::WorkflowResumed => "Workflow resumed".to_string(),
            EventData::WorkflowCompleted { .. } => "Workflow completed".to_string(),
            EventData::WorkflowFailed { error } => format!("Workflow failed: {error}"),
            EventData::WorkflowCancelled { reason } => format!("Workflow cancelled: {reason}"),

            EventData::StateEntered { state_id } => format!("Entered state '{state_id}'"),
            EventData::StateExited {
                state_id,
                output_label,
            } => {
                format!("Left state '{state_id}' with '{output_label}'")
            }

            EventData::ActionStarted {
                action_type,
                state_id,
                ..
            } => {
                format!("Action '{action_type}' started in state '{state_id}'")
            }
            EventData::ActionCompleted { output_label, .. } => {
                format!("Action completed with '{output_label}'")
            }
            EventData::ActionFailed { error, .. } => format!("Action failed: {error}"),
            EventData::ActionRetrying { retry_count, .. } => {
                format!("Retrying the action (attempt {})", retry_count + 1)
            }

            EventData::AgentSessionStarted { harness, .. } => {
                format!("Agent session started on '{harness}'")
            }
            EventData::AgentSessionCompleted { .. } => "Agent session completed".to_string(),
            EventData::AgentSessionFailed { error, .. } => {
                format!("Agent session failed: {error}")
            }
            EventData::AgentMessage { role, content, .. } => {
                format!("Agent message ({role}): {}", first_line(content))
            }

            EventData::DecisionRequested { provider } => {
                format!("Decision requested from '{provider}'")
            }
            EventData::DecisionMade {
                provider,
                output_label,
                ..
            } => {
                format!("Decision '{output_label}' made by '{provider}'")
            }

            EventData::HumanInputRequested { instruction } => {
                format!("Waiting for a human: {}", first_line(instruction))
            }
            EventData::HumanInputReceived { input } => {
                format!("Human input received: {}", summarize_input(input))
            }
            EventData::HumanTransitionSelected {
                from_state,
                to_state,
            } => {
                format!("Human chose the transition {from_state} -> {to_state}")
            }

            EventData::ExternalEventReceived {
                event_type, source, ..
            } => {
                format!("External event '{event_type}' from '{source}'")
            }

            EventData::TransitionTaken {
                from_state,
                to_state,
                output_label,
            } => {
                format!("Transition {from_state} -> {to_state} on '{output_label}'")
            }

            EventData::ArtifactCreated {
                artifact_type,
                location,
                ..
            } => {
                format!("Artifact '{artifact_type}' created at {location}")
            }

            EventData::Error { message, .. } => format!("Error: {message}"),
        }
    }
}

/// The first line of a multi-line string, trimmed and elided, so a long agent
/// prompt cannot break a one-line-per-event listing.
fn first_line(text: &str) -> String {
    const LIMIT: usize = 80;
    let line = text.lines().next().unwrap_or("").trim();

    if line.chars().count() <= LIMIT {
        return line.to_string();
    }
    let cut: String = line.chars().take(LIMIT).collect();
    format!("{cut}…")
}

/// Names the fields a human filled in, without echoing their values — the
/// values are in the context, and a free-text answer can be arbitrarily long.
fn summarize_input(input: &serde_json::Value) -> String {
    match input.as_object() {
        Some(fields) if !fields.is_empty() => {
            let names: Vec<&str> = fields.keys().map(String::as_str).collect();
            names.join(", ")
        }
        Some(_) => "no fields".to_string(),
        None => first_line(&input.to_string()),
    }
}
