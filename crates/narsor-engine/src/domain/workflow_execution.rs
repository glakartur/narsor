//! Workflow Execution model
//!
//! Represents a running or completed instance of a workflow.

use crate::domain::{Artifact, Context, StateId, WorkflowDefinition, Workspace};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use narsor_protocol::api::ExecutionStatusDto as ExecutionStatus;

/// Unique identifier for a workflow execution
pub type ExecutionId = String;

/// A running or completed workflow execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowExecution {
    /// Unique identifier for this execution
    pub id: ExecutionId,

    /// ID of the workflow definition
    pub workflow_id: String,

    /// Version of the workflow definition used
    pub workflow_version: u32,

    /// Current status
    pub status: ExecutionStatus,

    /// Current state (if running/paused/waiting)
    pub current_state: Option<StateId>,

    /// When the execution was started
    pub started_at: DateTime<Utc>,

    /// When the execution completed (if applicable)
    pub completed_at: Option<DateTime<Utc>>,

    /// Input parameters provided at start
    pub inputs: serde_json::Value,

    /// The workflow context (variables, artifacts references, etc.)
    pub context: Context,

    /// Workspace configuration for this execution
    pub workspace: Workspace,

    /// ID of the current/last action execution
    pub current_action_execution: Option<ActionExecutionId>,

    /// Metadata
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// Unique identifier for an action execution
pub type ActionExecutionId = String;

/// Status of an action execution
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ActionExecutionStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Retrying,
    Cancelled,
}

/// Represents a single execution of an action within a workflow
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionExecution {
    /// Unique identifier for this action execution
    pub id: ActionExecutionId,

    /// ID of the parent workflow execution
    pub workflow_execution_id: ExecutionId,

    /// ID of the state this action belongs to
    pub state_id: StateId,

    /// When the action execution started
    pub started_at: DateTime<Utc>,

    /// When the action execution completed
    pub ended_at: Option<DateTime<Utc>>,

    /// Current status
    pub status: ActionExecutionStatus,

    /// Input data provided to the action
    pub input: serde_json::Value,

    /// Result data from the action (if completed)
    pub output: Option<ActionResult>,

    /// Error information (if failed)
    pub error: Option<ActionError>,

    /// Number of retries attempted
    pub retry_count: u32,

    /// Associated agent session ID (if applicable)
    pub agent_session_id: Option<AgentSessionId>,

    /// Artifacts produced by this action
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
}

/// Result from an action execution
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionResult {
    /// The output label (determines transition)
    pub output_label: String,

    /// Data returned by the action
    #[serde(default)]
    pub data: serde_json::Value,

    /// Variables to update in the workflow context
    #[serde(default)]
    pub variables: serde_json::Map<String, serde_json::Value>,

    /// Exit code (for script actions)
    pub exit_code: Option<i32>,

    /// Standard output (for script/agent actions)
    #[serde(default)]
    pub stdout: String,

    /// Standard error (for script/agent actions)
    #[serde(default)]
    pub stderr: String,

    /// Final human-readable agent response, with the output-contract object removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<String>,
}

/// Error information for a failed action
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionError {
    /// Error message
    pub message: String,

    /// Error type/category
    #[serde(default)]
    pub error_type: Option<String>,

    /// Stack trace or additional details
    #[serde(default)]
    pub details: Option<serde_json::Value>,
}

/// Unique identifier for an agent session
pub type AgentSessionId = String;

/// Status of an agent session
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionStatus {
    Starting,
    Running,
    Paused,
    Completed,
    Failed,
    Terminated,
}

/// Represents a session with an AI agent harness
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    /// Unique identifier for this session
    pub id: AgentSessionId,

    /// ID of the parent workflow execution
    pub workflow_execution_id: ExecutionId,

    /// Which harness is being used
    pub harness: String,

    /// When the session was started
    pub started_at: DateTime<Utc>,

    /// When the session ended
    pub ended_at: Option<DateTime<Utc>>,

    /// Current status
    pub status: AgentSessionStatus,

    /// Model specification
    #[serde(default)]
    pub model: Option<String>,

    /// Session-specific configuration
    #[serde(default)]
    pub config: serde_json::Value,
}

impl WorkflowExecution {
    /// Creates a new workflow execution from a definition
    pub fn new(
        definition: &WorkflowDefinition,
        inputs: serde_json::Value,
        workspace: Workspace,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            workflow_id: definition.id.clone(),
            workflow_version: definition.version,
            status: ExecutionStatus::Running,
            current_state: Some(definition.initial_state.clone()),
            started_at: Utc::now(),
            completed_at: None,
            inputs,
            context: Context::new(),
            workspace,
            current_action_execution: None,
            metadata: serde_json::json!({}),
        }
    }

    /// Checks if the execution is in a terminal state
    pub fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }
}

impl ActionExecution {
    /// Creates a new action execution
    pub fn new(
        workflow_execution_id: ExecutionId,
        state_id: StateId,
        input: serde_json::Value,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            workflow_execution_id,
            state_id,
            started_at: Utc::now(),
            ended_at: None,
            status: ActionExecutionStatus::Pending,
            input,
            output: None,
            error: None,
            retry_count: 0,
            agent_session_id: None,
            artifacts: Vec::new(),
        }
    }
}

impl AgentSession {
    /// Creates a new agent session
    pub fn new(workflow_execution_id: ExecutionId, harness: String, model: Option<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            workflow_execution_id,
            harness,
            started_at: Utc::now(),
            ended_at: None,
            status: AgentSessionStatus::Starting,
            model,
            config: serde_json::json!({}),
        }
    }
}
