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
pub mod conversation_summary;
pub mod decision;
pub mod recovery;
pub mod retry;
pub mod runner;
pub mod step_profile;
pub mod timeline;
pub mod timeline_file;

pub use decision::{
    check_label, decide, permitted_labels, permitted_outputs, DecisionError, DecisionOutcome,
    DecisionRequest,
};
pub use recovery::{reconcile, Reconciliation, ReconciliationOutcome};
pub use retry::RetrySchedule;
pub use runner::{
    execution_episode, manual_override_allowed, owed_output_rescue, owed_reprompt,
    plan_operator_transition, record_manual_transition, resumable_agent_session, step_failure,
    terminal_status, ActionKind, Engine, EngineError, LastAgentStep, ManualTransition,
    OperatorTransition, OutputDecisionUndo, OutputRejection, OutputRescue, OwnerMessage,
    PendingStep, Recovery, ResumableSession, RunHandle, StepFailure, StepSelection, TransitionPlan,
    UndoneOutputDecision, DEFAULT_ESCALATION_THRESHOLD, EVENT_CHANNEL_CAPACITY,
    METADATA_CONDUCTOR_RETRY_KEY, METADATA_ENGINE_KEY, METADATA_OUTPUT_REJECTION_KEY,
    METADATA_OUTPUT_RESCUE_KEY, METADATA_STEP_FAILURE_KEY, METADATA_STEP_LOCALS_KEY,
};
pub use timeline::{
    build_timeline, ConversationState, EventLink, Position, PositionStatus, Timeline,
};
pub use timeline_file::{
    apply_usage, current_timeline, FileReadingSink, ReadingBasis, ReadingSink, UsageReading,
};

pub use actions::{
    ActionExecutor, ActionOutcome, ActionRequest, AgentEventSink, AgentExecutor, AgentRequest,
    AgentRunEnding, ChoiceAnswer, ClaudeCodeAdapter, CodexAdapter, ConductorAnalysis,
    ContextContent, ContextFile, ContextFiles, ContextSource, DefaultHarnessSource, FinishedRun,
    HarnessAdapter, HarnessRegistry, HarnessRun, HumanActionError, HumanExecutor,
    HumanInputDelivery, HumanInputRequest, InputContinuation, LoggingEventSink, OpenCodeAdapter,
    OutputProblem, OutputRequirement, PendingHumanInput, PermissionDecision,
    PermissionDecisionKind, PermissionGrantScope, ProfileExecutor, RejectedAnswer,
    ResumedHumanAction, RunObservers, RunStarted, ScriptExecutor, StepContract, StepInWorkflow,
    StepScope, SuspendedAction, TimedOutAction, UsageReporter, ACTION_ID_ENV_VAR,
};
