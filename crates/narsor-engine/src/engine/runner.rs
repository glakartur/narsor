//! The engine loop: walk the graph, one state at a time, until it stops.
//!
//! # The order of events is the contract
//!
//! One step emits, in this order (`docs/dev/domain-model.md` §4.2–4.3):
//!
//! ```text
//! StateEntered -> ActionStarted -> (ActionRetrying)* -> ActionCompleted
//!              -> DecisionMade -> [context written] -> TransitionTaken -> [state.json written]
//! ```
//!
//! Recovery is *derived* from that order (see [`crate::persistence::recovery`]),
//! so it is not merely a convention to be tidy about: an `ActionStarted` with no
//! `ActionCompleted` or `ActionFailed` after it is the definition of an
//! interrupted action, and nothing else marks one.
//!
//! An agent answer without a usable output label is a format error, not a
//! failure (KTD7): its attempt ends with `ActionOutputRejected`, and one new
//! attempt, announced by `OutputReprompted` rather than `ActionRetrying`,
//! resumes the agent's session to ask for the label again.
//!
//! Two deliberate refinements of the diagram in the plan:
//!
//! * **Templates are expanded after `ActionStarted`, not before.** Expansion can
//!   fail (an unknown `context.variables.*`), and that failure has to be
//!   reported as an `ActionFailed` — which would otherwise be an `ActionFailed`
//!   with no `ActionStarted` before it, the one shape recovery cannot read.
//!   Expansion happens once per *attempt*, so a retry re-reads the context.
//! * **Every attempt gets its own `ActionExecution` id.** D24 makes that id the
//!   idempotency key handed to the child process; it must therefore be stable
//!   within an attempt and different between retries. The new id is announced in
//!   the `ActionRetrying` event, which carries one for exactly this reason.
//!
//! # Only the engine writes the context (D10)
//!
//! An action *proposes* variables in its result; they reach `context.json` here,
//! after `DecisionMade` and before `TransitionTaken`, and nowhere else. The
//! whole `WorkflowExecution` is written with them, because `state.json` embeds
//! the context and is the authority — writing `context.json` alone would leave a
//! stale copy inside `state.json`.
//!
//! # Resuming in the middle of a step
//!
//! A step can be interrupted after its action finished but before the
//! transition was taken: by a crash, or on purpose when the state's decision
//! provider is `human`. Re-running the action would be wrong (it may have had a
//! side effect) and is often impossible (a person answered a question). So the
//! result of a finished action is persisted, in `metadata.engine.pending_step`,
//! the moment `ActionCompleted` is emitted, and the loop checks for it before
//! entering a state: when one is there, the step resumes at the decision.
//!
//! When even that attempt gives no label, or there is no session to ask
//! again, the step is handed to the conductor inline, under the pass's claim
//! (F1, KTD10): it chooses the output, asks the agent once more in its own
//! words, or makes the step a problem for the operator ([`rescue`]).
//!
//! A step whose action failed for good — its `retry_policy` ran out — fails
//! the execution as it always did, and the same save records the failure
//! for the conductor, which diagnoses it beside the failed execution and may
//! restart it once through an ordinary recovery (F3, [`failure`]).
//!
//! # Terminal states say how they end
//!
//! Whether reaching a terminal means success or failure is **declared**, not
//! guessed: `docs/dev/workflow-schema.md` §3.5 lets `terminal_states` be a mapping from
//! state id to the resulting status, which the loader puts on the state as
//! `terminal_status`. The engine only reads it — it never inspects the state's
//! name, because a name-based guess reports a terminal called
//! `rejected_by_reviewer` as a success. A terminal state with no declared
//! status ends the execution as [`ExecutionStatus::Completed`]. See
//! [`terminal_status`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::Duration;

use chrono::Utc;
use narsor_protocol::chat::{PromptAuthor, PromptPurpose};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use thiserror::Error;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::conductor::ConductorRegistry;
use crate::config::DEFAULT_ESCALATION_THRESHOLD;
use crate::config::{AgentArgs, AgentDefaults};
use crate::domain::{
    effective_title, ActionConfig, ActionError, ActionResult, AnsweredBy, Artifact,
    ArtifactReference, ChoiceDefinition, DecisionMaker, DecisionProviderType, Event, EventData,
    ExecutionId, ExecutionStatus, HarnessStepConfig, HumanActionConfig, HumanInputField,
    HumanInputType, InputCloseReason, ProblemKind, RepromptedBy, State, StateId, VariableSource,
    WorkflowDefinition, WorkflowExecution, Workspace, WorkspaceInfo,
};
use crate::engine::actions::agent::{
    self, DefaultHarnessSource, BUILT_IN_HARNESS, MISSING_OUTPUT_LABEL, SESSION_DATA_KEY,
};
use crate::engine::actions::harness;
use crate::engine::actions::human::{
    self, AgentSessionProvenance, ApprovalCandidate, ConductorAnalysis, EntryLimitDetails,
    HumanActionError, HumanInputDelivery, HumanInputRequest, InputContinuation, OutputProblem,
    OutputRequirement, PendingHumanInput, RejectedAnswer, RequiredValue, APPROVE_OUTPUT_LABEL,
    ENTRY_LIMIT_CONTINUE_LABEL, ENTRY_LIMIT_RESET_LABEL, RESUME_AGENT_LABEL,
};
use crate::engine::actions::profile::{self as profile_action, ProfileExecutor};
use crate::engine::actions::{
    ActionExecutor, ActionOutcome, ActionRequest, AgentExecutor, HumanExecutor, LoggingEventSink,
    ScriptExecutor, StepContract, StepScope, SuspendedAction, UsageReporter,
};
use crate::engine::decision::{self, DecisionError, DecisionOutcome, DecisionRequest};
use crate::engine::retry::RetrySchedule;
use crate::engine::step_profile::{self, ProfileInput, Setting};
use crate::engine::timeline_file::{FileReadingSink, ReadingSink};
use crate::persistence::{read_json, ExecutionStore, PersistenceError};
use crate::workflow::template::{
    embeds_user_content, step_reply_references, step_stream_references,
};
use crate::workflow::{
    action_context_references, places_step_history, DeclaredNames, StepMemory, TemplateScope,
    WorkspaceValues,
};

mod failure;
mod human_input;
mod lifecycle;
mod metadata;
mod output;
mod setup;
mod step;
mod transition;

#[cfg(test)]
mod tests;

use failure::*;
use human_input::*;
use lifecycle::*;
use metadata::*;
use output::*;
use setup::*;
use step::*;
use transition::*;

pub use failure::engine_error::EngineError;
pub use failure::handoff::{
    step_failure, Recovery, StepFailure, METADATA_CONDUCTOR_RETRY_KEY, METADATA_STEP_FAILURE_KEY,
};
pub use human_input::manual_transition::record_manual_transition;
pub use human_input::operations::{
    execution_episode, manual_override_allowed, plan_operator_transition, ManualTransition,
    OperatorTransition, OutputDecisionUndo, TransitionPlan, UndoneOutputDecision,
};
pub use lifecycle::finish::terminal_status;
pub use lifecycle::run_handle::RunHandle;
pub use metadata::keys::{
    METADATA_ENGINE_KEY, METADATA_LAST_AGENT_STEP_KEY, METADATA_OUTPUT_REJECTION_KEY,
    METADATA_OUTPUT_RESCUE_KEY, METADATA_PENDING_STEP_KEY,
};
pub use metadata::last_agent_step::LastAgentStep;
pub use metadata::pending_step::PendingStep;
pub use output::rejection::{owed_reprompt, OutputRejection};
pub use output::rescue_record::{owed_output_rescue, OutputRescue};
pub use step::action_kind::ActionKind;
pub use step::step_memory::{
    StepAttempt, StepAttempts, METADATA_STEP_ATTEMPTS_KEY, METADATA_STEP_LOCALS_KEY,
};

/// How many events the in-process broadcast channel buffers per receiver.
///
/// A receiver that falls further behind than this loses the events in between
/// and is told so by `RecvError::Lagged`; the engine itself never blocks on it.
pub const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Runs workflow executions against a store.
///
/// One engine serves any number of executions and any number of workflow
/// definitions: the definition is passed in per call rather than held, because
/// an execution records the definition it belongs to and the caller (the CLI,
/// the TUI) is the one that loaded it.
///
/// Every method takes `&self`, so the engine can be shared behind an [`Arc`]
/// between a running pass and a user interface that pauses it.
pub struct Engine {
    store: ExecutionStore,
    executors: HashMap<ActionKind, Arc<dyn ActionExecutor>>,
    agent_timeout_secs: Option<u64>,
    agent_args: AgentArgs,
    escalation_threshold: u32,
    usage_reporter: Option<Arc<dyn UsageReporter>>,
    reading_notifier: Option<Arc<dyn Fn() + Send + Sync>>,
    default_harness: Arc<dyn DefaultHarnessSource>,
    events: broadcast::Sender<Event>,
    pause_requests: Mutex<HashSet<ExecutionId>>,
    active_runs: Mutex<HashSet<ExecutionId>>,
    checkpoints: bool,
    conductor: OnceLock<Weak<ConductorRegistry>>,
    task_labels: TaskLabels,
    workspace_values: Option<Arc<dyn WorkspaceValues>>,
}

impl Engine {
    /// Appends an event to an execution's log and publishes it to the live
    /// stream, exactly as the engine's own events are.
    ///
    /// For the daemon's other in-process writers of the log — the conductor
    /// (D55) records its `Conductor*` events here. It never touches the
    /// execution's `state.json`, which only the pass holding the claim
    /// writes (KTD2), so it is safe to call whether or not a pass runs:
    /// concurrent appends each get their own sequence number.
    pub fn record_event(&self, execution_id: &str, data: EventData) -> Result<Event, EngineError> {
        self.emit(execution_id, data)
    }

    /// Appends an event and publishes it to the live stream.
    fn emit(&self, execution_id: &str, data: EventData) -> Result<Event, EngineError> {
        let event = self
            .store
            .event_log(execution_id)
            .append_next(&execution_id.to_string(), data)?;
        let _ = self.events.send(event.clone());
        Ok(event)
    }

    /// Publishes the events appended since `from` by code that writes the log
    /// itself (the human suspend/resume pair).
    fn publish_from(&self, execution_id: &str, from: u64) -> Result<(), EngineError> {
        let log = self.store.event_log(execution_id);
        let last = log.last_sequence()?.unwrap_or(0);
        for sequence in from..=last {
            let event: Event = read_json(&log.event_path(sequence))?;
            let _ = self.events.send(event);
        }
        Ok(())
    }

    /// The state an execution is at.
    fn current_state_id(&self, execution: &WorkflowExecution) -> Result<StateId, EngineError> {
        execution
            .current_state
            .clone()
            .ok_or_else(|| EngineError::NoCurrentState {
                execution_id: execution.id.clone(),
            })
    }

    /// Looks a state up in the definition the execution runs on.
    fn state_of<'a>(
        &self,
        definition: &'a WorkflowDefinition,
        execution: &WorkflowExecution,
        state_id: &str,
    ) -> Result<&'a State, EngineError> {
        definition
            .states
            .get(state_id)
            .ok_or_else(|| EngineError::UnknownState {
                execution_id: execution.id.clone(),
                state_id: state_id.to_string(),
                workflow_id: execution.workflow_id.clone(),
            })
    }
}
