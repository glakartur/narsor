//! Workflow Definition model
//! 
//! Represents the immutable definition of a workflow (the blueprint).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// Unique identifier for a workflow definition
pub type WorkflowDefinitionId = String;

/// Version number for workflow definitions
pub type Version = u32;

/// The main workflow definition structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowDefinition {
    /// Unique identifier for this workflow
    pub id: WorkflowDefinitionId,
    
    /// Human-readable name
    pub name: String,
    
    /// Version of this workflow definition
    pub version: Version,
    
    /// Optional description
    #[serde(default)]
    pub description: Option<String>,
    
    /// Input parameters required to start the workflow
    #[serde(default)]
    pub inputs: Vec<InputParameter>,
    
    /// States in the workflow graph
    pub states: HashMap<StateId, State>,
    
    /// The entry point state
    pub initial_state: StateId,
}

/// Input parameter definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputParameter {
    pub name: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<serde_json::Value>,
    #[serde(default)]
    pub description: Option<String>,
}

/// Identifier for a state within a workflow
pub type StateId = String;

/// A state in the workflow graph
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    /// Unique identifier within the workflow
    pub id: StateId,
    
    /// Optional human-readable description
    #[serde(default)]
    pub description: Option<String>,
    
    /// The action to execute when entering this state
    pub action: ActionConfig,
    
    /// Possible outputs from this state's action
    #[serde(default)]
    pub outputs: Vec<OutputDefinition>,
    
    /// Transitions from this state based on decision results
    #[serde(default)]
    pub transitions: HashMap<OutputLabel, StateId>,
    
    /// Decision provider configuration (optional, defaults to rules)
    #[serde(default)]
    pub decision: Option<DecisionConfig>,
    
    /// Timeout in seconds (optional)
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    
    /// Retry policy (optional)
    #[serde(default)]
    pub retry_policy: Option<RetryPolicy>,
    
    /// Whether this is a terminal state
    #[serde(default)]
    pub terminal: bool,
    
    /// Whether manual transition override is allowed
    #[serde(default)]
    pub allow_manual_override: bool,
}

/// Configuration for an action
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActionConfig {
    Agent(AgentActionConfig),
    Script(ScriptActionConfig),
    Human(HumanActionConfig),
    Wait(WaitActionConfig),
}

/// Agent action configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentActionConfig {
    /// Which harness to use (e.g., "claude-code", "opencode")
    pub harness: String,
    
    /// Model specification (optional)
    #[serde(default)]
    pub model: Option<String>,
    
    /// Prompt to send to the agent
    pub prompt: String,
    
    /// Workspace configuration (optional, defaults to current)
    #[serde(default)]
    pub workspace: Option<WorkspaceRef>,
    
    /// Additional parameters
    #[serde(default)]
    pub parameters: HashMap<String, serde_json::Value>,
}

/// Script action configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptActionConfig {
    /// Command to execute
    pub command: String,
    
    /// Working directory (optional)
    #[serde(default)]
    pub working_dir: Option<String>,
    
    /// Environment variables
    #[serde(default)]
    pub env: HashMap<String, String>,
    
    /// Timeout in seconds
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// Human action configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HumanActionConfig {
    /// Description of what the human needs to do
    pub instruction: String,
    
    /// Expected input schema (optional)
    #[serde(default)]
    pub input_schema: Option<serde_json::Value>,
}

/// Wait action configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitActionConfig {
    /// Event type to wait for
    pub event: String,
    
    /// Filter criteria (optional)
    #[serde(default)]
    pub filter: Option<serde_json::Value>,
    
    /// Timeout in seconds
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// Reference to a workspace
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceRef {
    Current,
    Repository(String),
    Path(String),
}

/// Definition of a possible output from an action
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputDefinition {
    pub label: OutputLabel,
    #[serde(default)]
    pub description: Option<String>,
}

/// Label identifying an output/transition
pub type OutputLabel = String;

/// Decision provider configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DecisionConfig {
    Rules,
    Agent(AgentDecisionConfig),
    Human,
}

/// Agent-based decision configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDecisionConfig {
    #[serde(default)]
    pub harness: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
}

/// Retry policy configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Maximum number of retries
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    
    /// Delay between retries in seconds
    #[serde(default = "default_retry_delay")]
    pub delay_secs: u64,
    
    /// Backoff multiplier
    #[serde(default = "default_backoff_multiplier")]
    pub backoff_multiplier: f64,
}

fn default_max_retries() -> u32 { 0 }
fn default_retry_delay() -> u64 { 1 }
fn default_backoff_multiplier() -> f64 { 2.0 }

impl WorkflowDefinition {
    /// Validates the workflow definition
    pub fn validate(&self) -> Result<(), ValidationError> {
        // Check that initial state exists
        if !self.states.contains_key(&self.initial_state) {
            return Err(ValidationError::InitialStateNotFound(self.initial_state.clone()));
        }
        
        // Check all transitions point to valid states
        for (state_id, state) in &self.states {
            for (output_label, target_state_id) in &state.transitions {
                if !self.states.contains_key(target_state_id) {
                    return Err(ValidationError::TransitionToUnknownState(
                        state_id.clone(),
                        output_label.clone(),
                        target_state_id.clone(),
                    ));
                }
            }
            
            // Check terminal states have no transitions
            if state.terminal && !state.transitions.is_empty() {
                return Err(ValidationError::TerminalStateWithTransitions(state_id.clone()));
            }
        }
        
        // Check for at least one terminal state
        let has_terminal = self.states.values().any(|s| s.terminal);
        if !has_terminal {
            return Err(ValidationError::NoTerminalState);
        }
        
        Ok(())
    }
}

/// Errors that can occur during workflow validation
#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("Initial state '{0}' not found")]
    InitialStateNotFound(StateId),
    
    #[error("State '{0}' has transition '{1}' to unknown state '{2}'")]
    TransitionToUnknownState(StateId, OutputLabel, StateId),
    
    #[error("Terminal state '{0}' cannot have transitions")]
    TerminalStateWithTransitions(StateId),
    
    #[error("Workflow must have at least one terminal state")]
    NoTerminalState,
}
