//! Workflow engine.
//!
//! The engine is what turns a loaded [`crate::domain::WorkflowDefinition`] into
//! movement: it runs the action of the current state, decides which transition
//! its output label selects, and persists the result.
//!
//! [`actions`] holds the execution half of that job — one executor per action
//! type, all behind the [`actions::ActionExecutor`] trait.

pub mod actions;

pub use actions::{
    ActionExecutor, ActionOutcome, ActionRequest, ScriptExecutor, TimedOutAction, ACTION_ID_ENV_VAR,
};
