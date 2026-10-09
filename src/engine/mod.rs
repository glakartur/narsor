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
    execution_episode, owed_output_rescue, owed_reprompt, plan_operator_transition,
    record_manual_transition, step_failure, terminal_status, ActionKind, AgentDefaults, Engine,
    EngineError, LastAgentStep, ManualTransition, OperatorTransition, OutputDecisionUndo,
    OutputRejection, OutputRescue, PendingStep, Recovery, RunHandle, StepFailure, TransitionPlan,
    UndoneOutputDecision, EVENT_CHANNEL_CAPACITY, METADATA_CONDUCTOR_RETRY_KEY,
    METADATA_ENGINE_KEY, METADATA_OUTPUT_REJECTION_KEY, METADATA_OUTPUT_RESCUE_KEY,
    METADATA_STEP_FAILURE_KEY,
};
pub use timeline::{
    build_timeline, ConversationState, EventLink, Position, PositionStatus, Timeline,
};

pub use actions::{
    ActionExecutor, ActionOutcome, ActionRequest, AgentEventSink, AgentExecutor, AgentOutput,
    ClaudeCodeAdapter, CodexAdapter, ConductorAnalysis, HarnessAdapter, HarnessInvocation,
    HarnessRegistry, HarnessRequest, HarnessRun, HumanActionError, HumanExecutor,
    HumanInputDelivery, HumanInputRequest, InputContinuation, LoggingEventSink, OpenCodeAdapter,
    OutputProblem, OutputRequirement, PendingHumanInput, RejectedAnswer, ResumedHumanAction,
    ScriptExecutor, SuspendedAction, TimedOutAction, ACTION_ID_ENV_VAR,
};
