//! Workflow engine.
//!
//! The engine is what turns a loaded [`crate::domain::WorkflowDefinition`] into
//! movement: it runs the action of the current state, decides which transition
//! its output label selects, and persists the result.
//!
//! [`actions`] holds the execution half of that job — one executor per action
//! type, all behind the [`actions::ActionExecutor`] trait; the agent executor
//! reaches the AI harnesses through [`actions::harness`], one adapter per CLI. [`runner`] holds the
//! loop itself, with the order of emitted events that recovery is derived from;
//! [`decision`] chooses the transition a finished action selects and is the one
//! place an output label is judged against the state; [`retry`] resolves the
//! hierarchy of retry policies and the backoff between attempts.

pub mod actions;
pub mod decision;
pub mod retry;
pub mod runner;
pub mod timeline;

pub use decision::{
    check_label, decide, permitted_labels, permitted_outputs, DecisionError, DecisionOutcome,
    DecisionRequest,
};
pub use retry::RetrySchedule;
pub use runner::{
    terminal_status, ActionKind, AgentDefaults, Engine, EngineError, PendingStep, RunHandle,
    EVENT_CHANNEL_CAPACITY,
};
pub use timeline::{
    build_timeline, ConversationState, EventLink, Position, PositionStatus, Timeline,
};

pub use actions::{
    ActionExecutor, ActionOutcome, ActionRequest, AgentEventSink, AgentExecutor, AgentOutput,
    ClaudeCodeAdapter, CodexAdapter, HarnessAdapter, HarnessInvocation, HarnessRegistry,
    HarnessRequest, HarnessRun, HumanActionError, HumanExecutor, HumanInputDelivery,
    HumanInputRequest, InputContinuation, LoggingEventSink, OpenCodeAdapter, PendingHumanInput,
    ResumedHumanAction, ScriptExecutor, SuspendedAction, TimedOutAction, ACTION_ID_ENV_VAR,
};
