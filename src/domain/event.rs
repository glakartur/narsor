//! Event model
//! 
//! Represents events in the workflow execution history.

use serde::{Serialize, Deserialize};
use chrono::{DateTime, Utc};
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
    pub fn new(
        workflow_execution_id: String,
        data: EventData,
        sequence: u64,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            workflow_execution_id,
            timestamp: Utc::now(),
            data,
            sequence,
        }
    }
    
    /// Returns a human-readable description of the event
    pub fn description(&self) -> String {
        match &self.data {
            EventData::WorkflowStarted { workflow_id, .. } => 
                format!("Workflow '{}' started", workflow_id),
            EventData::StateEntered { state_id } => 
                format!("Entered state '{}'", state_id),
            EventData::ActionStarted { action_type, .. } => 
                format!("Action '{}' started", action_type),
            EventData::DecisionMade { output_label, .. } => 
                format!("Decision made: '{}'", output_label),
            EventData::TransitionTaken { from_state, to_state, .. } => 
                format!("Transition: {} -> {}", from_state, to_state),
            _ => format!("{:?}", self.data),
        }
    }
}
