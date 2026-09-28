//! Action module (re-exports from workflow_definition)
//! 
//! Action types are defined in workflow_definition.rs

pub use crate::domain::workflow_definition::{
    ActionConfig,
    AgentActionConfig,
    ScriptActionConfig,
    HumanActionConfig,
    WaitActionConfig,
};
