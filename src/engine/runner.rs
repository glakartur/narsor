//! The engine loop: walk the graph, one state at a time, until it stops.
//!
//! # The order of events is the contract
//!
//! One step emits, in this order (`DOMAIN_MODEL.md` §4.2–4.3):
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
//! guessed: `WORKFLOW_SCHEMA.md` §3.5 lets `terminal_states` be a mapping from
//! state id to the resulting status, which the loader puts on the state as
//! `terminal_status`. The engine only reads it — it never inspects the state's
//! name, because a name-based guess reports a terminal called
//! `rejected_by_reviewer` as a success. A terminal state with no declared
//! status ends the execution as [`ExecutionStatus::Completed`]. See
//! [`terminal_status`].

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use thiserror::Error;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::conductor::ConductorRegistry;
use crate::domain::{
    ActionConfig, ActionError, ActionResult, AnsweredBy, Artifact, ArtifactReference,
    DecisionProviderType, Event, EventData, ExecutionId, ExecutionStatus, InputCloseReason,
    OutputDefinition, ProblemKind, RepromptedBy, State, StateId, WorkflowDefinition,
    WorkflowExecution, Workspace, WorkspaceInfo, WorkspaceSpec,
};
use crate::engine::actions::agent::{
    self, BUILT_IN_HARNESS, MISSING_OUTPUT_LABEL, SESSION_DATA_KEY,
};
use crate::engine::actions::harness;
use crate::engine::actions::human::{
    self, ConductorAnalysis, HumanActionError, HumanInputDelivery, HumanInputRequest,
    InputContinuation, OutputProblem, OutputRequirement, PendingHumanInput, RejectedAnswer,
};
use crate::engine::actions::{
    ActionExecutor, ActionOutcome, ActionRequest, AgentExecutor, HumanExecutor, LoggingEventSink,
    ScriptExecutor, SuspendedAction,
};
use crate::engine::decision::{self, DecisionError, DecisionOutcome, DecisionRequest};
use crate::engine::retry::RetrySchedule;
use crate::persistence::{read_json, DefaultHarnessSelector, ExecutionStore, PersistenceError};
use crate::workflow::{action_context_references, TemplateScope};

mod failure;
mod operations;
mod rescue;

pub use failure::{
    step_failure, Recovery, StepFailure, METADATA_CONDUCTOR_RETRY_KEY, METADATA_STEP_FAILURE_KEY,
};
pub use operations::{
    execution_episode, ManualTransition, OutputDecisionUndo, UndoneOutputDecision,
};

/// How many events the in-process broadcast channel buffers per receiver.
///
/// A receiver that falls further behind than this loses the events in between
/// and is told so by `RecvError::Lagged`; the engine itself never blocks on it.
pub const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Key of the engine's own scratch area inside `WorkflowExecution::metadata`.
pub const METADATA_ENGINE_KEY: &str = "engine";

/// Key of the persisted mid-step state inside the engine's scratch area.
pub const METADATA_PENDING_STEP_KEY: &str = "pending_step";

/// Key of the record of the agent step behind the latest transition (KTD8).
pub const METADATA_LAST_AGENT_STEP_KEY: &str = "last_agent_step";

/// Key of an agent output that was rejected and still awaits its reprompt
/// (KTD7).
pub const METADATA_OUTPUT_REJECTION_KEY: &str = "output_rejection";

/// Key of a step whose output the conductor is rescuing (F1, U10).
pub const METADATA_OUTPUT_RESCUE_KEY: &str = "output_rescue";

/// How much of an agent's answer travels with the input request after it.
const ANSWER_TAIL_BYTES: usize = 2 * 1024;

/// A one-shot operator note to attach to the next recovered agent attempt.
const METADATA_OPERATOR_RECOVERY_NOTE_KEY: &str = "operator_recovery_note";
const METADATA_AGENT_INPUT_KEY: &str = "agent_input";

/// How an execution ends when it enters the terminal state `state`.
///
/// The answer comes from the workflow's own declaration (`WORKFLOW_SCHEMA.md`
/// §3.5) and from nowhere else; a terminal that declares nothing completes the
/// execution.
pub fn terminal_status(state: &State) -> ExecutionStatus {
    state
        .terminal_status
        .map(ExecutionStatus::from)
        .unwrap_or(ExecutionStatus::Completed)
}

/// The agent executor this engine runs, wired to its own store and channel.
///
/// One constructor rather than two, so that anything added here — a second
/// sink, a different registry — cannot be silently lost by whichever of
/// [`Engine::new`] and [`Engine::with_agent_defaults`] ran last.
fn agent_executor(
    store: &ExecutionStore,
    events: &broadcast::Sender<Event>,
    default_harness: &str,
    default_harness_selector: Option<Arc<DefaultHarnessSelector>>,
) -> AgentExecutor {
    let executor = AgentExecutor::new().with_default_harness(default_harness);
    let executor = match default_harness_selector {
        Some(selector) => executor.with_default_harness_selector(selector),
        None => executor,
    };
    executor.with_event_sink(Arc::new(LoggingEventSink::new(
        store.clone(),
        Some(events.clone()),
    )))
}

/// What the configuration contributes to an `agent` action.
///
/// A small value rather than the whole resolved configuration, so the engine
/// stays independent of how those two values were arrived at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDefaults {
    /// The harness for an action that names none.
    pub harness: String,
    /// The time limit for an action that declares none, `None` for no limit.
    pub timeout_secs: Option<u64>,
}

/// The action types an executor can be registered for.
///
/// A typed key rather than a string, so that registering an executor for a
/// misspelled action type cannot compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionKind {
    /// `AgentAction`.
    Agent,
    /// `ScriptAction`.
    Script,
    /// `HumanAction`.
    Human,
    /// `WaitAction` — no executor in this version.
    Wait,
}

impl ActionKind {
    /// The kind of an action configuration.
    pub fn of(action: &ActionConfig) -> Self {
        match action {
            ActionConfig::Agent(_) => Self::Agent,
            ActionConfig::Script(_) => Self::Script,
            ActionConfig::Human(_) => Self::Human,
            ActionConfig::Wait(_) => Self::Wait,
        }
    }

    /// The name this action type has in the YAML contract and in events.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Script => "script",
            Self::Human => "human",
            Self::Wait => "wait",
        }
    }
}

/// The result of a finished action, kept until its transition has been taken.
///
/// Persisted in `metadata.engine.pending_step` so that a step interrupted
/// between `ActionCompleted` and `TransitionTaken` resumes at the decision
/// instead of re-running the action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingStep {
    /// The state whose action produced the result.
    pub state_id: StateId,

    /// The attempt that produced it (D24 idempotency key).
    pub action_execution_id: String,

    /// What the action returned.
    pub result: ActionResult,

    /// Artifacts the action reported, already recorded as events.
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
}

/// What the agent step behind the latest transition said (KTD8).
///
/// Persisted in `metadata.engine.last_agent_step` when a transition leaves an
/// agent state, and dropped when one leaves any other state, so that a human
/// gate entered right after an agent can show the agent's question. The
/// action's own result is gone by then: `pending_step` is cleared the moment
/// the transition is taken.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastAgentStep {
    /// The agent state the transition left.
    pub state_id: StateId,

    /// The state the transition entered — the only one the record speaks to.
    pub to_state: StateId,

    /// The attempt that produced the answer.
    pub action_execution_id: String,

    /// The agent's `data.question`, when it stated one.
    #[serde(default)]
    pub question: Option<String>,

    /// The end of the agent's answer.
    #[serde(default)]
    pub answer_tail: Option<String>,
}

impl LastAgentStep {
    /// The record of `result`, produced in `state_id` by `action_execution_id`
    /// and followed by a transition into `to_state`.
    fn from_result(
        state_id: StateId,
        to_state: StateId,
        action_execution_id: String,
        result: &ActionResult,
    ) -> Self {
        let (question, answer_tail) = agent_answer(result);
        Self {
            state_id,
            to_state,
            action_execution_id,
            question,
            answer_tail,
        }
    }
}

/// An agent attempt whose output was rejected as a format error (KTD7).
///
/// Persisted in `metadata.engine.output_rejection` from just before
/// `ActionOutputRejected` until the reprompt announces itself with
/// `OutputReprompted`. A restart in between finds the attempt closed in the
/// log — reconciliation leaves the execution `Running` — and the next pass
/// picks the reprompt up from this record instead of running the state's
/// action afresh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputRejection {
    /// The state whose action ran.
    pub state_id: StateId,

    /// The attempt whose output was rejected.
    pub action_execution_id: String,

    /// Why it was rejected.
    pub reason: String,

    /// The end of the agent's answer.
    #[serde(default)]
    pub answer_tail: Option<String>,

    /// The harness the attempt ran on — where its session can be resumed.
    #[serde(default)]
    pub harness: Option<String>,

    /// The harness's own session id, when it reported one.
    #[serde(default)]
    pub harness_session_id: Option<String>,
}

impl OutputRejection {
    /// The rejection of an answer no label could be extracted from, when
    /// `error` is one — `None` for any other failure.
    fn from_error(state: &State, action_execution_id: &str, error: &ActionError) -> Option<Self> {
        if error.error_type.as_deref() != Some(MISSING_OUTPUT_LABEL) {
            return None;
        }
        let detail = |key: &str| {
            error
                .details
                .as_ref()
                .and_then(|details| details.get(key))
                .and_then(Value::as_str)
                .map(ToString::to_string)
        };
        Some(Self {
            state_id: state.id.clone(),
            action_execution_id: action_execution_id.to_string(),
            reason: error.message.clone(),
            answer_tail: detail("answer_tail").filter(|tail| !tail.is_empty()),
            harness: detail("harness"),
            harness_session_id: detail("harness_session_id"),
        })
    }

    /// The rejection of a result whose label the state does not declare,
    /// when it is one — `None` for a declared label.
    fn from_result(
        state: &State,
        action_execution_id: &str,
        result: &ActionResult,
    ) -> Option<Self> {
        let error = match decision::check_label(state, &result.output_label) {
            Err(error @ DecisionError::LabelNotDeclared { .. }) => error,
            _ => return None,
        };
        let session = result.data.get(SESSION_DATA_KEY);
        let session_field = |pointer: &str| {
            session
                .and_then(|session| session.pointer(pointer))
                .and_then(Value::as_str)
                .map(ToString::to_string)
        };
        Some(Self {
            state_id: state.id.clone(),
            action_execution_id: action_execution_id.to_string(),
            reason: error.to_string(),
            answer_tail: Some(harness::tail(&result.stdout, ANSWER_TAIL_BYTES))
                .filter(|tail| !tail.is_empty()),
            harness: session_field("/harness"),
            harness_session_id: session_field("/config/harness_session_id"),
        })
    }

    /// The rejection of an agent that asked a question instead of giving the
    /// label it was asked for again (KTD7): a failed reprompt, never a
    /// suspension that would restart the step in a fresh session.
    fn from_question(state: &State, action_execution_id: &str, question: &str) -> Self {
        Self {
            state_id: state.id.clone(),
            action_execution_id: action_execution_id.to_string(),
            reason: format!(
                "the agent asked for human input instead of giving its output label: {question}"
            ),
            answer_tail: None,
            harness: None,
            harness_session_id: None,
        }
    }
}

/// A step whose agent gave no usable output label, even when asked again,
/// handed to the conductor (F1, U10).
///
/// Persisted in `metadata.engine.output_rescue` from the moment the step is
/// handed over — before the event that closes the last attempt — until its
/// transition is taken or it becomes a problem. A pass that finds it (after
/// a restart, or an interrupt) carries the rescue on instead of entering
/// the state afresh: nothing the conductor does can have a side effect, so
/// this is the one place a pass continues rather than replays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputRescue {
    /// The state whose output is rescued.
    pub state_id: StateId,

    /// The agent's answers without a label, oldest first. The last one's
    /// attempt is the rescue's episode (KTD9) and the attempt the step
    /// completes under.
    pub answers: Vec<RejectedAnswer>,

    /// Why the step has no label yet.
    pub reason: String,

    /// The harness the agent's session ran on, when it said.
    #[serde(default)]
    pub harness: Option<String>,

    /// The agent's session, to ask it again in (the conductor's (b)).
    #[serde(default)]
    pub harness_session_id: Option<String>,

    /// Whether the conductor's one reprompt was used (R17).
    #[serde(default)]
    pub conductor_reprompted: bool,
}

impl OutputRescue {
    /// The rescue of the step whose first answer was `rejection`.
    fn after(rejection: &OutputRejection) -> Self {
        Self {
            state_id: rejection.state_id.clone(),
            answers: vec![RejectedAnswer::from(rejection)],
            reason: rejection.reason.clone(),
            harness: rejection.harness.clone(),
            harness_session_id: rejection.harness_session_id.clone(),
            conductor_reprompted: false,
        }
    }

    /// Adds the answer of a reprompt that gave no label.
    fn record(&mut self, failure: &UnlabelledAnswer) {
        self.answers.push(failure.answer.clone());
        self.reason = failure.reason.clone();
        if failure.harness_session_id.is_some() {
            self.harness = failure.harness.clone().or_else(|| self.harness.clone());
            self.harness_session_id = failure.harness_session_id.clone();
        }
    }

    /// The episode of the rescue (KTD9): the attempt of the last answer.
    pub fn episode(&self) -> &str {
        self.answers
            .last()
            .map(|answer| answer.action_execution_id.as_str())
            .unwrap_or_default()
    }

    /// Whether the conductor may still ask the agent again: once, and only
    /// in a session there is to resume (R17).
    pub fn can_reprompt(&self) -> bool {
        !self.conductor_reprompted && self.harness.is_some() && self.harness_session_id.is_some()
    }
}

impl From<&OutputRejection> for RejectedAnswer {
    fn from(rejection: &OutputRejection) -> Self {
        Self {
            action_execution_id: rejection.action_execution_id.clone(),
            reason: rejection.reason.clone(),
            answer_tail: rejection.answer_tail.clone(),
        }
    }
}

/// Everything that stops the engine from running a step at all.
///
/// A *failing action* is not in here: an action that fails is an ordinary
/// outcome of a workflow, recorded as `ActionFailed` and leaving the execution
/// in [`ExecutionStatus::Failed`]. These variants are the cases where the engine
/// itself cannot proceed.
#[derive(Debug, Error)]
pub enum EngineError {
    /// The store could not be read or written.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),

    /// Delivering or recording human input failed.
    #[error(transparent)]
    HumanInput(#[from] HumanActionError),

    /// The state's decision provider cannot be used.
    #[error(transparent)]
    Decision(#[from] DecisionError),

    /// The definition does not pass static validation (§5).
    #[error("workflow definition is invalid:\n  {}", violations.join("\n  "))]
    InvalidDefinition {
        /// Every violation, so the author sees them all at once.
        violations: Vec<String>,
    },

    /// A running execution has no current state to step from.
    #[error("execution '{execution_id}' has no current state")]
    NoCurrentState {
        /// The execution that was stepped.
        execution_id: ExecutionId,
    },

    /// The execution points at a state the definition does not contain.
    #[error(
        "execution '{execution_id}' is at state '{state_id}', which workflow \
         '{workflow_id}' does not define"
    )]
    UnknownState {
        /// The execution that was stepped.
        execution_id: ExecutionId,
        /// The state it points at.
        state_id: StateId,
        /// The workflow it was started from.
        workflow_id: String,
    },

    /// The execution is in a status the requested operation does not allow.
    #[error("execution '{execution_id}' is {status} and cannot be {operation}")]
    NotRunnable {
        /// The execution that was addressed.
        execution_id: ExecutionId,
        /// Its status, as the schema spells it.
        status: String,
        /// What was attempted, e.g. `run`.
        operation: &'static str,
    },

    /// The execution is paused on a question only a person can answer.
    #[error(
        "execution '{execution_id}' is waiting for human input; deliver the input \
         instead of resuming it"
    )]
    AwaitingHumanInput {
        /// The execution that was addressed.
        execution_id: ExecutionId,
    },

    /// The engine's own bookkeeping inside `metadata` could not be read.
    #[error("engine metadata of execution '{execution_id}' is unreadable: {message}")]
    CorruptEngineMetadata {
        /// The execution whose metadata is broken.
        execution_id: ExecutionId,
        /// What went wrong.
        message: String,
    },

    /// Another pass is already driving this execution.
    #[error("execution '{execution_id}' is already being driven by another pass")]
    AlreadyRunning {
        /// The execution a second driver tried to advance.
        execution_id: ExecutionId,
    },

    /// A request named a step failure the execution is no longer stopped
    /// on (KTD9): somebody acted on it first.
    #[error(
        "execution '{execution_id}' is no longer failed on '{expected}'{}",
        failed_now(.current)
    )]
    StaleEpisode {
        /// The execution that was addressed.
        execution_id: ExecutionId,
        /// The failure the request named.
        expected: String,
        /// The failure it is stopped on now, if any.
        current: Option<String>,
    },

    /// A request was made at a moment the execution has moved on from
    /// (KTD9): a proposal of the conductor's about an input already
    /// answered, a state already left.
    #[error(
        "execution '{execution_id}' has moved on since this was asked for (it was at \
         '{expected}'{})",
        now_at(.current)
    )]
    EpisodeChanged {
        /// The execution that was addressed.
        execution_id: ExecutionId,
        /// Where the request expected it to be.
        expected: String,
        /// Where it is now, if anywhere.
        current: Option<String>,
    },

    /// A manual transition the definition does not allow.
    #[error("{message}")]
    InvalidTransition {
        /// Why.
        message: String,
    },
}

/// The tail of [`EngineError::EpisodeChanged`]'s message.
fn now_at(current: &Option<String>) -> String {
    match current {
        Some(current) => format!(", it is now at '{current}'"),
        None => String::new(),
    }
}

/// The tail of [`EngineError::StaleEpisode`]'s message.
fn failed_now(current: &Option<String>) -> String {
    match current {
        Some(current) => format!(" (it is failed on '{current}')"),
        None => String::new(),
    }
}

/// Whether the loop keeps stepping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    /// Step again.
    Continue,
    /// The pass ends here: the execution finished, failed or suspended.
    Stop,
}

/// How one attempt at an action ended, as far as the retry loop is concerned.
enum AttemptOutcome {
    /// The action produced a result.
    Completed(ActionResult),
    /// The action is waiting for a person; the pass ends.
    AwaitingHumanInput(HumanInputRequest),
    /// An agent asked a question and will be restarted in this same state once
    /// the person supplies its answer.
    AwaitingAgentInput(HumanInputRequest),
    /// An agent answered without an output label the state accepts: a format
    /// error, answered with a reprompt rather than a retry (KTD7).
    OutputRejected(OutputRejection),
}

/// How the whole action of a state ended, retries included.
enum ActionUpshot {
    /// A result came back and the step can go on to its decision.
    Completed {
        /// The attempt that produced it.
        action_execution_id: String,
        /// What it produced.
        result: ActionResult,
    },
    /// The execution is suspended or failed, and that is already persisted.
    Stopped,
    /// The step already took its transition without the action's own
    /// result — the conductor's rescue settled it (F1) — and the pass goes
    /// on.
    Transitioned,
}

impl ActionUpshot {
    /// The upshot of a step the conductor's rescue settled with `flow`.
    fn settled(flow: Flow) -> Self {
        match flow {
            Flow::Stop => Self::Stopped,
            Flow::Continue => Self::Transitioned,
        }
    }
}

/// How one attempt asking the agent again for its label ended.
pub(super) enum RepromptEnd {
    /// The agent gave a label the state accepts.
    Labelled {
        /// The reprompt attempt.
        action_execution_id: String,
        /// Its result.
        result: ActionResult,
    },
    /// It gave none; the attempt is still to be closed.
    Unlabelled(UnlabelledAnswer),
    /// The execution failed, and that is already persisted.
    Stopped,
}

/// A reprompt attempt that gave no usable label.
pub(super) struct UnlabelledAnswer {
    /// The answer, as a problem shows it.
    answer: RejectedAnswer,
    /// Why the step still has no label.
    reason: String,
    /// The harness the attempt ran on, when it said.
    harness: Option<String>,
    /// The session it ran in, when it said.
    harness_session_id: Option<String>,
}

/// How a reprompt ended without a label the state accepts.
enum RepromptFailure {
    /// Another format error — including a question instead of the label.
    Rejected(OutputRejection),
    /// The run itself failed or timed out.
    Failed(String),
}

/// An attempt that produced no result.
struct AttemptFailure {
    /// The message that ends up in `ActionFailed`.
    message: String,
    /// Whether trying again could plausibly give a different answer.
    retriable: bool,
    /// Whether the attempt cannot have had a side effect (KTD14): nothing
    /// ran, or what ran left no trace of having acted.
    side_effect_free: bool,
}

impl AttemptFailure {
    /// A failure worth another attempt: the action could not run, or ran out of
    /// time.
    fn retriable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retriable: true,
            side_effect_free: false,
        }
    }

    /// A failure that repeating cannot fix: a broken template, an unknown
    /// action type, a label the state does not declare. Every one of them
    /// stops the attempt before its action runs.
    fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retriable: false,
            side_effect_free: true,
        }
    }

    /// The same failure, known to have had no side effect when `clean`.
    fn side_effect_free_if(mut self, clean: bool) -> Self {
        self.side_effect_free = clean;
        self
    }
}

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
    events: broadcast::Sender<Event>,
    pause_requests: Mutex<HashSet<ExecutionId>>,
    active_runs: Mutex<HashSet<ExecutionId>>,
    checkpoints: bool,
    conductor: OnceLock<Weak<ConductorRegistry>>,
}

/// Holds the "already running" claim on one execution for as long as a pass is
/// driving it.
///
/// Acquired by [`Engine::begin_run`], which does the check-and-insert under one
/// lock so two attempts can never both succeed; released on `Drop`, so every
/// exit path — a normal return, an early `?`, or a panic unwinding through it —
/// frees the execution the same way.
struct RunGuard<'a> {
    engine: &'a Engine,
    execution_id: ExecutionId,
}

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        self.engine.lock_active_runs().remove(&self.execution_id);
    }
}

/// An owned equivalent of [`RunGuard`]: holds the same "already running"
/// claim, over the same `active_runs` set, but through an [`Arc<Engine>`]
/// rather than a borrow — which is what lets it live inside a
/// `tokio::spawn`-ed task instead of a stack frame that ends when an HTTP
/// handler returns (U3). [`Engine::begin_run`] and this claim both operate on
/// the one `active_runs` set, so a plain [`Engine::run`]/[`Engine::resume`]
/// and a [`RunHandle`]-driven resume can never both succeed on the same
/// execution.
struct ActiveRunClaim {
    engine: Arc<Engine>,
    execution_id: ExecutionId,
}

impl Drop for ActiveRunClaim {
    fn drop(&mut self) {
        self.engine.lock_active_runs().remove(&self.execution_id);
    }
}

/// What a prepared (but not yet driven to completion) resume/delivery ended
/// up at, synchronously.
enum HandleState {
    /// The execution still has to be stepped (`run_loop`).
    Ready(WorkflowExecution),
    /// The synchronous phase already reached a stop (e.g. a `human` action's
    /// answer completed the step but the workflow suspended again right
    /// away) — nothing left to run.
    Done(WorkflowExecution),
}

/// The claim on one execution, held between the synchronous "try to begin"
/// phase and the asynchronous "run to completion" phase (U3's two-phase
/// split of [`Engine::resume`]/[`Engine::deliver_human_input`]).
///
/// Returned by [`Engine::try_begin_resume`] and
/// [`Engine::try_begin_deliver_human_input`] — both fail synchronously, with
/// no `.await`, if another pass already holds the execution
/// ([`EngineError::AlreadyRunning`]), which is what lets an HTTP handler
/// answer `409` immediately without spawning anything. On success, the
/// handle carries everything [`RunHandle::run_to_completion`] needs to
/// finish the job; that method is the only way to consume it, and is exactly
/// the thing a caller hands to `tokio::spawn`.
///
/// The exact shape here (an owned handle over `Arc<Engine>`, rather than the
/// plan's own sketch of `ResumeHandle<'_>` borrowing `&Engine`) is this
/// unit's implementation decision (see the plan's U3 Approach and Open
/// Questions): a borrowed handle cannot outlive the stack frame that created
/// it, which is exactly the frame an HTTP handler returns from before phase
/// 2 finishes.
pub struct RunHandle {
    engine: Arc<Engine>,
    execution_id: ExecutionId,
    claim: ActiveRunClaim,
    state: HandleState,
}

impl RunHandle {
    /// The execution this handle is driving.
    pub fn execution_id(&self) -> &str {
        &self.execution_id
    }

    /// Runs the prepared execution to completion (or, if the synchronous
    /// phase already stopped it, simply returns it) — the asynchronous half
    /// of the two-phase split, safe to hand to `tokio::spawn` because it owns
    /// everything it touches.
    ///
    /// Any error is recorded as an [`EventData::Error`] against the
    /// execution before it is returned, exactly as
    /// [`Engine::resume`]/[`Engine::deliver_human_input`] already do via
    /// `observed` — a caller that spawns this and discards the `JoinHandle`
    /// still gets the failure onto the event stream.
    pub async fn run_to_completion(
        self,
        definition: &WorkflowDefinition,
    ) -> Result<WorkflowExecution, EngineError> {
        let RunHandle {
            engine,
            execution_id,
            claim,
            state,
        } = self;

        let outcome = match state {
            HandleState::Done(execution) => Ok(execution),
            HandleState::Ready(execution) => engine.run_loop(definition, execution).await,
        };

        if let Err(error) = &outcome {
            engine.note_engine_error(&execution_id, error);
        }
        drop(claim);
        outcome
    }
}

impl Engine {
    /// Creates an engine with the executors of this version: `script`, `human`
    /// and `agent`.
    ///
    /// `wait` is deliberately unregistered — it is Phase 3 — so a workflow that
    /// uses one fails with a clear message instead of behaving like something
    /// it is not. [`Engine::with_executor`] fills it in.
    ///
    /// The agent executor is given a [`LoggingEventSink`] over this engine's
    /// own store and live channel, so the agent-session events it emits land in
    /// the same log, in the same order, as the engine's own.
    pub fn new(store: ExecutionStore) -> Self {
        let events = broadcast::channel(EVENT_CHANNEL_CAPACITY).0;

        let agent = agent_executor(&store, &events, BUILT_IN_HARNESS, None);

        let mut executors: HashMap<ActionKind, Arc<dyn ActionExecutor>> = HashMap::new();
        executors.insert(ActionKind::Script, Arc::new(ScriptExecutor::new()));
        executors.insert(ActionKind::Human, Arc::new(HumanExecutor::new()));
        executors.insert(ActionKind::Agent, Arc::new(agent));

        Self {
            store,
            executors,
            agent_timeout_secs: None,
            events,
            pause_requests: Mutex::new(HashSet::new()),
            active_runs: Mutex::new(HashSet::new()),
            checkpoints: false,
            conductor: OnceLock::new(),
        }
    }

    /// Returns the engine with the configured defaults for `agent` actions.
    ///
    /// The default harness belongs to the agent executor, so this replaces it —
    /// which means it must come **before** any
    /// [`with_executor`](Engine::with_executor) registering an agent executor
    /// of your own, or that one is overwritten.
    pub fn with_agent_defaults(self, defaults: AgentDefaults) -> Self {
        self.with_agent_defaults_inner(defaults, None)
    }

    /// Returns the engine with configured agent defaults and a shared live
    /// selector for harnessless attempts. Custom executors registered with
    /// [`with_executor`](Engine::with_executor) remain untouched.
    pub fn with_agent_defaults_and_selector(
        self,
        defaults: AgentDefaults,
        selector: Arc<DefaultHarnessSelector>,
    ) -> Self {
        self.with_agent_defaults_inner(defaults, Some(selector))
    }

    fn with_agent_defaults_inner(
        mut self,
        defaults: AgentDefaults,
        selector: Option<Arc<DefaultHarnessSelector>>,
    ) -> Self {
        self.agent_timeout_secs = defaults.timeout_secs;
        let agent = agent_executor(&self.store, &self.events, &defaults.harness, selector);
        self.executors.insert(ActionKind::Agent, Arc::new(agent));
        self
    }

    /// Writes a checkpoint of the workspace at every step boundary (R4).
    ///
    /// Opt-in while there is no hub to replicate to: turning it on for every
    /// engine would make each step of every existing test shell out to `git`
    /// for no gain. From M2 on, the hub's worker turns it on unconditionally.
    pub fn with_checkpoints(mut self) -> Self {
        self.checkpoints = true;
        self
    }

    /// Registers (or replaces) the executor for one action type.
    pub fn with_executor(mut self, kind: ActionKind, executor: Arc<dyn ActionExecutor>) -> Self {
        self.executors.insert(kind, executor);
        self
    }

    /// Hands every step whose agent gives no usable output label, even
    /// when asked again, to `registry`'s conductor (F1, U10).
    ///
    /// Held weakly: the registry holds the engine, and the engine must not
    /// keep it alive in return. Whoever attaches it keeps it — the daemon,
    /// for as long as it runs. Without one (or once it is gone) such a step
    /// becomes a problem for the operator straight away, as it did before
    /// the conductor existed. Only the first registry attached counts.
    pub fn attach_conductor(&self, registry: &Arc<ConductorRegistry>) {
        if self.conductor.set(Arc::downgrade(registry)).is_err() {
            tracing::warn!("a conductor is already attached to this engine; ignoring another");
        }
    }

    /// The conductor attached to this engine, while it is alive.
    fn conductor(&self) -> Option<Arc<ConductorRegistry>> {
        self.conductor.get().and_then(Weak::upgrade)
    }

    /// The store this engine persists to.
    pub fn store(&self) -> &ExecutionStore {
        &self.store
    }

    /// Subscribes to the live event stream.
    ///
    /// Every event the engine appends to a log is published here, for every
    /// execution it runs. This is the seam an SSE endpoint will take over
    /// (D38): the transport changes, the contract does not. A receiver that is
    /// dropped or falls behind never slows the engine down and never fails a
    /// step — the send result is deliberately ignored.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

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

    // -----------------------------------------------------------------------
    // Starting, running, pausing
    // -----------------------------------------------------------------------

    /// Creates an execution: directories, `state.json`, `context.json`,
    /// `WorkflowStarted` (`DOMAIN_MODEL.md` §4.1).
    ///
    /// The definition is validated first: an invalid workflow must leave nothing
    /// on disk. The context starts out holding the inputs, so that a template
    /// may read a starting value through either `{{ inputs.x }}` or
    /// `{{ context.variables.x }}`.
    pub fn start(
        &self,
        definition: &WorkflowDefinition,
        inputs: Value,
        workspace: Workspace,
    ) -> Result<WorkflowExecution, EngineError> {
        definition
            .validate()
            .map_err(|violations| EngineError::InvalidDefinition {
                violations: violations.iter().map(ToString::to_string).collect(),
            })?;

        let mut execution = WorkflowExecution::new(definition, inputs, workspace);

        if let Some(object) = execution.inputs.as_object() {
            execution.context.update_variables(object.clone());
        }
        execution.context.workspace = Some(WorkspaceInfo {
            id: execution.workspace.id.clone(),
            path: execution.workspace.path.clone(),
            workspace_type: format!("{:?}", execution.workspace.workspace_type).to_lowercase(),
        });

        self.store.create_execution_dirs(&execution.id)?;
        self.store.save_execution(&execution)?;

        self.emit(
            &execution.id,
            EventData::WorkflowStarted {
                workflow_id: definition.id.clone(),
                workflow_version: definition.version,
                inputs: execution.inputs.clone(),
            },
        )?;

        Ok(execution)
    }

    /// Starts an execution and runs it until it stops.
    pub async fn start_and_run(
        &self,
        definition: &WorkflowDefinition,
        inputs: Value,
        workspace: Workspace,
    ) -> Result<WorkflowExecution, EngineError> {
        let execution = self.start(definition, inputs, workspace)?;
        self.run_loop(definition, execution).await
    }

    /// Runs a stored execution until it reaches a terminal state, suspends or
    /// is paused.
    ///
    /// Only a `Running` execution can be run; a paused one goes through
    /// [`Engine::resume`] or [`Engine::deliver_human_input`], which is what
    /// keeps an answer from being delivered twice.
    pub async fn run(
        &self,
        definition: &WorkflowDefinition,
        execution_id: &str,
    ) -> Result<WorkflowExecution, EngineError> {
        self.observed(execution_id, async {
            let _guard = self.begin_run(execution_id)?;
            let execution = self.store.load_execution(execution_id)?;
            if execution.status != ExecutionStatus::Running {
                return Err(self.not_runnable(&execution, "run"));
            }
            self.run_loop(definition, execution).await
        })
        .await
    }

    /// Runs a stored execution like [`Engine::run`], but stops after `steps`
    /// step boundaries — each one a transition taken and persisted — even if
    /// the workflow could go on.
    ///
    /// The execution is left `Running` at a clean boundary, exactly as a
    /// process that died right after that step would leave it, which is what
    /// makes it the seam for proving that a takeover resumes correctly from any
    /// point.
    pub async fn run_steps(
        &self,
        definition: &WorkflowDefinition,
        execution_id: &str,
        steps: usize,
    ) -> Result<WorkflowExecution, EngineError> {
        let execution = self.store.load_execution(execution_id)?;
        if execution.status != ExecutionStatus::Running {
            return Err(self.not_runnable(&execution, "run"));
        }
        self.step_loop(definition, execution, Some(steps)).await
    }

    /// Asks the pass currently running this execution to stop before its next
    /// action (D34).
    ///
    /// Pausing never interrupts a running action: the current one finishes
    /// normally and its transition is taken, and only then does the loop stop.
    /// The request is in-process and is honoured by the next step boundary —
    /// including the first one, if no pass is running yet. Use
    /// [`Engine::pause_now`] to park an execution that nothing is stepping.
    pub fn pause(&self, execution_id: &str) {
        self.lock_pause_requests().insert(execution_id.to_string());
    }

    /// Whether a pause has been requested and not yet acted on.
    pub fn is_pause_requested(&self, execution_id: &str) -> bool {
        self.lock_pause_requests().contains(execution_id)
    }

    /// Whether a pass is currently driving this execution through
    /// [`Engine::run`], [`Engine::resume`] or [`Engine::deliver_human_input`]
    /// (R7).
    ///
    /// For a caller deciding whether it is safe to touch the execution's
    /// record by some *other* means (the TUI's manual/forced transition,
    /// which writes the store directly rather than going through one of
    /// those three) — not for retrying the call itself, since a failed
    /// acquisition already reports [`EngineError::AlreadyRunning`] atomically
    /// at the point that matters.
    pub fn is_active(&self, execution_id: &str) -> bool {
        self.lock_active_runs().contains(execution_id)
    }

    /// A snapshot of every execution a pass is currently driving.
    ///
    /// For the daemon's graceful shutdown (U2): on `SIGTERM` it asks each of
    /// these to [`Engine::pause`] and then polls this same getter until it is
    /// empty (or a timeout elapses). A snapshot under the lock, not a live
    /// view — the set can change the instant after this returns, which is
    /// fine for a poll loop that simply calls it again.
    pub fn active_run_ids(&self) -> Vec<ExecutionId> {
        self.lock_active_runs().iter().cloned().collect()
    }

    /// Parks a running execution immediately: status `Paused`, `WorkflowPaused`
    /// in the log.
    ///
    /// For an execution no pass is stepping. Calling it on one that *is* being
    /// stepped is harmless but pointless — the pass would save over it — so
    /// [`Engine::pause`] is the right call there.
    pub fn pause_now(
        &self,
        execution_id: &str,
        reason: &str,
    ) -> Result<WorkflowExecution, EngineError> {
        let mut execution = self.store.load_execution(execution_id)?;
        if execution.status != ExecutionStatus::Running {
            return Err(self.not_runnable(&execution, "paused"));
        }
        self.pause_execution(&mut execution, reason)?;
        Ok(execution)
    }

    /// Resumes a paused execution and runs it on.
    ///
    /// Refuses an execution that needs input: that one needs an
    /// answer, not a nudge.
    ///
    /// A thin wrapper, unchanged in contract, over the two-phase split U3
    /// introduced for the REST endpoint: [`Engine::try_begin_resume`] claims
    /// the run and does the synchronous half, then
    /// [`RunHandle::run_to_completion`] does the rest — sequentially, in one
    /// `.await`, exactly as this used to be written as a single block.
    pub async fn resume(
        &self,
        definition: &WorkflowDefinition,
        execution_id: &str,
    ) -> Result<WorkflowExecution, EngineError> {
        self.observed(execution_id, async {
            let _guard = self.begin_run(execution_id)?;
            let execution = self.prepare_resume(execution_id)?;
            self.run_loop(definition, execution).await
        })
        .await
    }

    /// The synchronous half of [`Engine::resume`]: validates the execution's
    /// status and, for a manually `Paused` one, flips it back to `Running` and emits
    /// `WorkflowResumed` — everything [`Engine::resume`] did before it
    /// reached `run_loop`. Shared, not duplicated, between the borrowed
    /// (`resume`) and owned ([`Engine::try_begin_resume`]) entry points.
    fn prepare_resume(&self, execution_id: &str) -> Result<WorkflowExecution, EngineError> {
        let mut execution = self.store.load_execution(execution_id)?;

        match execution.status {
            ExecutionStatus::Running => {}
            ExecutionStatus::Paused => {
                self.lock_pause_requests().remove(execution_id);
                execution.status = ExecutionStatus::Running;
                self.store.save_execution(&execution)?;
                self.emit(execution_id, EventData::WorkflowResumed)?;
            }
            _ => return Err(self.not_runnable(&execution, "resumed")),
        }

        Ok(execution)
    }

    /// The synchronous half of an explicit operator recovery. Recovery is
    /// deliberately limited to terminal failure or interruption states, so it
    /// cannot turn daemon startup into an automatic replay.
    ///
    /// A recovery naming a step failure's key is refused once the execution
    /// is no longer failed on it (KTD9); the failure record goes in the same
    /// save as the status change, and so does the conductor's retry
    /// ([`Recovery`]).
    fn prepare_recover(
        &self,
        execution_id: &str,
        recovery: Recovery,
    ) -> Result<WorkflowExecution, EngineError> {
        let mut execution = self.store.load_execution(execution_id)?;
        match execution.status {
            ExecutionStatus::Failed | ExecutionStatus::Unknown => {}
            _ if recovery.episode.is_some() => {
                return Err(EngineError::StaleEpisode {
                    execution_id: execution_id.to_string(),
                    expected: recovery.episode.unwrap_or_default(),
                    current: None,
                })
            }
            _ => return Err(self.not_runnable(&execution, "recovered")),
        }
        self.recover_step_failure(&mut execution, &recovery)?;
        let comment = recovery.comment;

        // A recovery starts the state's action afresh. A rejected output left
        // behind by an interrupted reprompt must not turn it into another one,
        // and a request left behind must say it was closed by the recovery
        // rather than linger as if it could still be answered.
        clear_engine_metadata(&mut execution, METADATA_OUTPUT_REJECTION_KEY);
        clear_engine_metadata(&mut execution, METADATA_OUTPUT_RESCUE_KEY);
        let from = self.store.event_log(execution_id).next_sequence()?;
        human::close_pending(
            &self.store,
            execution_id,
            InputCloseReason::Recover,
            AnsweredBy::Operator,
        )?;
        self.publish_from(execution_id, from)?;

        if let Some(comment) = comment.filter(|comment| !comment.trim().is_empty()) {
            engine_metadata_mut(&mut execution).insert(
                METADATA_OPERATOR_RECOVERY_NOTE_KEY.to_string(),
                Value::String(comment),
            );
        }

        self.lock_pause_requests().remove(execution_id);
        execution.status = ExecutionStatus::Running;
        execution.completed_at = None;
        self.store.save_execution(&execution)?;
        self.emit(execution_id, EventData::WorkflowResumed)?;
        Ok(execution)
    }

    /// Phase 1 of the two-phase `resume` (U3): claims the execution
    /// synchronously — returning [`EngineError::AlreadyRunning`] immediately,
    /// with no `.await`, if another pass already holds it — and does the
    /// same synchronous validation [`Engine::resume`] does. On success, hand
    /// the returned [`RunHandle`] to [`RunHandle::run_to_completion`],
    /// typically via `tokio::spawn`, to actually drive the workflow on.
    ///
    /// Takes `self: &Arc<Self>` (rather than `&self`) because the handle it
    /// returns must be able to outlive this call's stack frame — the whole
    /// point of splitting this out of [`Engine::resume`] is to let an HTTP
    /// handler return `202 Accepted` right after this succeeds, while phase
    /// 2 keeps running after the response is sent.
    pub fn try_begin_resume(
        self: &Arc<Self>,
        execution_id: &str,
    ) -> Result<RunHandle, EngineError> {
        let claim = self.claim_active_run(execution_id)?;
        match self.prepare_resume(execution_id) {
            Ok(execution) => Ok(RunHandle {
                engine: Arc::clone(self),
                execution_id: execution_id.to_string(),
                claim,
                state: HandleState::Ready(execution),
            }),
            Err(error) => {
                self.note_engine_error(execution_id, &error);
                Err(error)
            }
        }
    }

    /// Claims a failed or crash-interrupted execution and starts one new
    /// attempt. The caller must invoke this explicitly; recovery is never
    /// performed while reconciling a stopped daemon.
    pub fn try_begin_recover(
        self: &Arc<Self>,
        execution_id: &str,
        comment: Option<String>,
    ) -> Result<RunHandle, EngineError> {
        self.try_begin_recover_with(execution_id, Recovery::with_comment(comment))
    }

    /// [`Engine::try_begin_recover`] as `recovery` describes it: with the key
    /// of the step failure it answers, and for the conductor's one retry.
    ///
    /// A stale key is [`EngineError::StaleEpisode`], and — like
    /// [`EngineError::AlreadyRunning`] — is not recorded as an `Error`
    /// event: whoever acted first is what the log shows.
    pub fn try_begin_recover_with(
        self: &Arc<Self>,
        execution_id: &str,
        recovery: Recovery,
    ) -> Result<RunHandle, EngineError> {
        let claim = self.claim_active_run(execution_id)?;
        match self.prepare_recover(execution_id, recovery) {
            Ok(execution) => Ok(RunHandle {
                engine: Arc::clone(self),
                execution_id: execution_id.to_string(),
                claim,
                state: HandleState::Ready(execution),
            }),
            Err(error) => {
                if !matches!(error, EngineError::StaleEpisode { .. }) {
                    self.note_engine_error(execution_id, &error);
                }
                Err(error)
            }
        }
    }

    /// The question an execution is waiting on, if it is waiting on one.
    pub fn pending_input(
        &self,
        execution_id: &str,
    ) -> Result<Option<PendingHumanInput>, EngineError> {
        Ok(human::pending_input(&self.store, execution_id)?)
    }

    /// Delivers an answer to an execution needing input and runs it on.
    ///
    /// Serves both places an execution can wait for a person: a `human` action,
    /// where the answer *is* the action's result, and a `human` decision, where
    /// the action already finished and the answer only picks the transition.
    /// The two are told apart by the persisted mid-step state: a decision is
    /// pending exactly when the action's result is already recorded.
    pub async fn deliver_human_input(
        &self,
        definition: &WorkflowDefinition,
        execution_id: &str,
        delivery: &HumanInputDelivery,
    ) -> Result<WorkflowExecution, EngineError> {
        self.observed(execution_id, async {
            let _guard = self.begin_run(execution_id)?;
            match self.prepare_deliver_human_input(definition, execution_id, delivery)? {
                HandleState::Done(execution) => Ok(execution),
                HandleState::Ready(execution) => self.run_loop(definition, execution).await,
            }
        })
        .await
    }

    /// The synchronous half of [`Engine::deliver_human_input`]: records the
    /// answer, applies it (as a decision or as an action's own result,
    /// depending on what was pending), and reports whether the step already
    /// stopped or still needs `run_loop`. Shared between the borrowed
    /// (`deliver_human_input`) and owned
    /// ([`Engine::try_begin_deliver_human_input`]) entry points.
    fn prepare_deliver_human_input(
        &self,
        definition: &WorkflowDefinition,
        execution_id: &str,
        delivery: &HumanInputDelivery,
    ) -> Result<HandleState, EngineError> {
        let from = self.store.event_log(execution_id).next_sequence()?;
        let resumed = human::resume(&self.store, execution_id, delivery)?;
        self.publish_from(execution_id, from)?;

        let mut execution = resumed.execution;
        self.lock_pause_requests().remove(execution_id);

        let state_id = self.current_state_id(&execution)?;
        let state = self.state_of(definition, &execution, &state_id)?;

        if resumed.pending.continuation == InputContinuation::ResumeAgent {
            let answer = agent_reply(&resumed.result.variables).ok_or_else(|| {
                EngineError::HumanInput(HumanActionError::MissingRequiredField {
                    field: "answer or option".to_string(),
                })
            })?;
            self.store_agent_input(&mut execution, &state_id, &answer)?;
            return Ok(HandleState::Ready(execution));
        }

        if resumed.pending.continuation == InputContinuation::ResolveOutput {
            // The answer is the output the agent did not give: recorded as
            // the step's result, and the label it names is the decision,
            // made by whoever answered rather than by the state's provider.
            let outcome = DecisionOutcome {
                label: resumed.result.output_label.clone(),
                provider: match delivery.answered_by {
                    AnsweredBy::Operator => DecisionProviderType::Human,
                    AnsweredBy::Conductor => DecisionProviderType::Conductor,
                },
                reasoning: delivery
                    .comment
                    .as_deref()
                    .map(str::trim)
                    .filter(|comment| !comment.is_empty())
                    .map(ToString::to_string),
            };
            let mut result = resumed.result;
            // What the agent last said, for a gate the label may lead to: it
            // is the answer the operator settled, and the gate shows it
            // beside the question the operator supplied (KTD8).
            result.stdout = resumed
                .pending
                .problem
                .as_ref()
                .and_then(|problem| {
                    problem
                        .answers
                        .iter()
                        .rev()
                        .find_map(|answer| answer.answer_tail.clone())
                })
                .unwrap_or_default();
            let flow = match self.record_result(
                &mut execution,
                state,
                resumed.pending.action_execution_id().to_string(),
                result,
            )? {
                Some(pending) => {
                    self.apply_decision(definition, &mut execution, state, pending, outcome)?
                }
                None => Flow::Stop,
            };
            return Ok(match flow {
                Flow::Stop => HandleState::Done(execution),
                Flow::Continue => HandleState::Ready(execution),
            });
        }

        let flow = match self.pending_step(&execution, &state_id)? {
            Some(pending) => {
                // The action had already finished: this answer is the decision.
                let outcome = DecisionOutcome {
                    label: resumed.result.output_label.clone(),
                    provider: DecisionProviderType::Human,
                    reasoning: None,
                };
                self.apply_decision(definition, &mut execution, state, pending, outcome)?
            }
            None => {
                // The answer is the human action's own result.
                self.record_completion(
                    definition,
                    &mut execution,
                    state,
                    resumed.pending.action_execution_id().to_string(),
                    resumed.result,
                )?
            }
        };

        Ok(match flow {
            Flow::Stop => HandleState::Done(execution),
            Flow::Continue => HandleState::Ready(execution),
        })
    }

    /// Phase 1 of the two-phase `deliver_human_input` (U3): claims the
    /// execution synchronously — returning [`EngineError::AlreadyRunning`]
    /// immediately, with no `.await`, if another pass already holds it — and
    /// does the same synchronous recording/decision work
    /// [`Engine::deliver_human_input`] does. On success, hand the returned
    /// [`RunHandle`] to [`RunHandle::run_to_completion`], typically via
    /// `tokio::spawn`, to drive the workflow the rest of the way.
    pub fn try_begin_deliver_human_input(
        self: &Arc<Self>,
        definition: &WorkflowDefinition,
        execution_id: &str,
        delivery: &HumanInputDelivery,
    ) -> Result<RunHandle, EngineError> {
        let claim = self.claim_active_run(execution_id)?;
        match self.prepare_deliver_human_input(definition, execution_id, delivery) {
            Ok(state) => Ok(RunHandle {
                engine: Arc::clone(self),
                execution_id: execution_id.to_string(),
                claim,
                state,
            }),
            Err(error) => {
                self.note_engine_error(execution_id, &error);
                Err(error)
            }
        }
    }

    // -----------------------------------------------------------------------
    // The loop
    // -----------------------------------------------------------------------

    /// Steps until the execution stops being runnable.
    async fn run_loop(
        &self,
        definition: &WorkflowDefinition,
        execution: WorkflowExecution,
    ) -> Result<WorkflowExecution, EngineError> {
        self.step_loop(definition, execution, None).await
    }

    /// Steps until the execution stops being runnable or, when `limit` is
    /// given, until that many step boundaries have been crossed.
    async fn step_loop(
        &self,
        definition: &WorkflowDefinition,
        mut execution: WorkflowExecution,
        limit: Option<usize>,
    ) -> Result<WorkflowExecution, EngineError> {
        let mut crossed = 0;
        loop {
            if execution.status != ExecutionStatus::Running {
                break;
            }
            if limit.is_some_and(|limit| crossed >= limit) {
                break;
            }

            if self.take_pause_request(&execution.id) {
                self.pause_execution(&mut execution, "paused on operator request")?;
                break;
            }

            let state_id = self.current_state_id(&execution)?;
            let state = self.state_of(definition, &execution, &state_id)?;

            let flow = if let Some(rescue) = stored_output_rescue(&execution, &state_id)? {
                // A pass that ended while the conductor was rescuing the
                // step's output (a restart, an interrupt): the rescue goes on
                // from its record, never from the start of the state.
                self.continue_output_rescue(definition, &mut execution, state, rescue)
                    .await?
            } else {
                match self.pending_step(&execution, &state_id)? {
                    Some(pending) => {
                        self.complete_step(definition, &mut execution, state, pending)?
                    }
                    None => match stored_output_rejection(&execution, &state_id)? {
                        // A pass that ended between a rejected output and its
                        // reprompt: the attempt is closed, so it continues with
                        // the reprompt instead of running the action afresh.
                        Some(rejection) => {
                            self.continue_rejected_output(
                                definition,
                                &mut execution,
                                state,
                                rejection,
                            )
                            .await?
                        }
                        None => self.enter_state(definition, &mut execution, state).await?,
                    },
                }
            };

            if flow == Flow::Stop {
                break;
            }
            crossed += 1;
        }

        Ok(execution)
    }

    /// Enters a state: `StateEntered`, then either the end of the workflow or
    /// the state's action.
    async fn enter_state(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
    ) -> Result<Flow, EngineError> {
        execution.current_state = Some(state.id.clone());
        self.emit(
            &execution.id,
            EventData::StateEntered {
                state_id: state.id.clone(),
            },
        )?;

        if state.terminal {
            self.finish(execution, state)?;
            return Ok(Flow::Stop);
        }

        let Some(action) = &state.action else {
            // Static validation rejects this, so reaching it means the
            // definition was changed under a running execution.
            let id = Uuid::new_v4().to_string();
            self.emit(
                &execution.id,
                EventData::ActionStarted {
                    action_execution_id: id.clone(),
                    state_id: state.id.clone(),
                    action_type: "none".to_string(),
                },
            )?;
            self.fail_action(
                execution,
                &id,
                format!("state '{}' is not terminal but has no action", state.id),
            )?;
            return Ok(Flow::Stop);
        };

        match self
            .run_action(definition, execution, state, action)
            .await?
        {
            ActionUpshot::Stopped => Ok(Flow::Stop),
            ActionUpshot::Transitioned => Ok(Flow::Continue),
            ActionUpshot::Completed {
                action_execution_id,
                result,
            } => self.record_completion(definition, execution, state, action_execution_id, result),
        }
    }

    /// The time limit of one attempt.
    ///
    /// The state's own value wins, then the workflow's global one — which is
    /// what R13 asks for: a setting is a default, never an instruction the
    /// workflow cannot override. Only when neither declares anything does the
    /// configured default apply, and only to an `agent` action (D-C).
    fn timeout_for(
        &self,
        kind: ActionKind,
        state: &State,
        definition: &WorkflowDefinition,
    ) -> Option<u64> {
        let declared = state.timeout_secs.or(definition.globals.timeout_secs);
        match kind {
            ActionKind::Agent => declared.or(self.agent_timeout_secs),
            // Unchanged, bit for bit: a script or human state that declares no
            // limit still has none. Spelled out rather than caught by `_`, so
            // that a fifth action kind has to decide rather than inherit.
            ActionKind::Script | ActionKind::Human | ActionKind::Wait => declared,
        }
    }

    /// Runs the action of a state, retrying it as the policy allows (D21).
    ///
    /// A failure that survived every attempt is already recorded as
    /// `ActionFailed`, and a suspension is already persisted, so both come back
    /// as [`ActionUpshot::Stopped`].
    async fn run_action(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
        action: &ActionConfig,
    ) -> Result<ActionUpshot, EngineError> {
        let schedule = RetrySchedule::resolve(state, &definition.globals);
        let kind = ActionKind::of(action);
        let timeout = self
            .timeout_for(kind, state, definition)
            .map(Duration::from_secs);

        // The conductor's retry is one attempt, never a new round of the
        // policy (KTD14).
        let single_attempt = failure::conductor_retry_of(execution, &state.id)?.is_some();
        let mut side_effect_free = true;

        let mut attempt = 1_u32;
        let mut action_execution_id = Uuid::new_v4().to_string();

        execution.current_action_execution = Some(action_execution_id.clone());
        self.emit(
            &execution.id,
            EventData::ActionStarted {
                action_execution_id: action_execution_id.clone(),
                state_id: state.id.clone(),
                action_type: kind.name().to_string(),
            },
        )?;

        loop {
            let attempted = self
                .attempt_action(execution, state, action, &action_execution_id, timeout)
                .await;

            let failure = match attempted {
                Ok(AttemptOutcome::AwaitingHumanInput(mut request)) => {
                    if let Some(record) = preceding_agent_step(execution, state) {
                        request.question = record.question;
                        request.answer_tail = record.answer_tail;
                    }
                    self.suspend_for_input(
                        execution,
                        state,
                        request,
                        decision::permitted_outputs(state),
                    )?;
                    return Ok(ActionUpshot::Stopped);
                }
                Ok(AttemptOutcome::AwaitingAgentInput(request)) => {
                    self.suspend_for_agent_input(execution, state, request)?;
                    return Ok(ActionUpshot::Stopped);
                }
                Ok(AttemptOutcome::OutputRejected(rejection)) => {
                    // Outside the retry schedule on purpose (R15): a format
                    // error is repaired in the same session, not re-run.
                    self.reject_output(execution, &rejection)?;
                    return self
                        .reprompt(definition, execution, state, action, rejection, timeout)
                        .await;
                }
                Ok(AttemptOutcome::Completed(result)) => {
                    return Ok(ActionUpshot::Completed {
                        action_execution_id,
                        result,
                    })
                }
                Err(failure) => failure,
            };

            side_effect_free &= failure.side_effect_free;
            if !failure.retriable || single_attempt || !schedule.permits_another(attempt) {
                self.fail_attempts(
                    execution,
                    &action_execution_id,
                    failure.message,
                    side_effect_free,
                )?;
                return Ok(ActionUpshot::Stopped);
            }

            let next_attempt = attempt + 1;
            let delay = schedule.delay_before(next_attempt);

            // Announced before the backoff sleep (R4, R7, R11): an operator
            // watching the log sees the failure the moment it happens, not
            // only once the delay has elapsed. The last attempt never reaches
            // here — it fails through the branch above, as `ActionFailed`.
            self.emit(
                &execution.id,
                EventData::ActionAttemptFailed {
                    action_execution_id: action_execution_id.clone(),
                    state_id: state.id.clone(),
                    error: failure.message,
                    retry_in_ms: delay.as_millis() as u64,
                },
            )?;

            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }

            attempt = next_attempt;

            // A fresh idempotency key per attempt (D24), announced in the event.
            action_execution_id = Uuid::new_v4().to_string();
            execution.current_action_execution = Some(action_execution_id.clone());
            self.emit(
                &execution.id,
                EventData::ActionRetrying {
                    action_execution_id: action_execution_id.clone(),
                    retry_count: RetrySchedule::retry_count(attempt),
                    state_id: Some(state.id.clone()),
                },
            )?;
        }
    }

    /// One attempt: expand the templates, hand the action to its executor,
    /// translate what comes back.
    async fn attempt_action(
        &self,
        execution: &mut WorkflowExecution,
        state: &State,
        action: &ActionConfig,
        action_execution_id: &str,
        timeout: Option<Duration>,
    ) -> Result<AttemptOutcome, AttemptFailure> {
        let location = format!("state '{}'", state.id);

        // Lazily, on entering the state, so that `{{ context.variables.x }}`
        // picks up the value current at this moment (D31).
        let scope = TemplateScope::from_execution(&execution.inputs, &execution.context)
            .map_err(|error| AttemptFailure::permanent(error.to_string()))?;
        let mut rendered = scope
            .render_action(action, &location)
            .map_err(|error| AttemptFailure::permanent(error.to_string()))?;

        // A comment is consumed only by the first agent attempt following an
        // explicit recovery. It remains durable until this point, so a daemon
        // restart before the attempt begins cannot silently discard it.
        if let ActionConfig::Agent(config) = &mut rendered {
            if let Some(answer) = self.take_agent_input(execution, state)? {
                config
                    .prompt
                    .push_str("\n\nHuman answer to your question:\n");
                config.prompt.push_str(&answer);
            }
            if let Some(note) = self.take_operator_recovery_note(execution)? {
                config.prompt.push_str("\n\nOperator recovery note:\n");
                config.prompt.push_str(&note);
            }
        }

        let kind = ActionKind::of(action);
        let outcome = self
            .execute_rendered(execution, state, &rendered, action_execution_id, timeout)
            .await?;

        match outcome {
            Ok(ActionOutcome::Completed(result)) => {
                // A label the state does not declare is a format error of an
                // agent, answered with a reprompt (KTD7); for any other action
                // it stays the permanent failure `record_completion` reports.
                if kind == ActionKind::Agent {
                    if let Some(rejection) =
                        OutputRejection::from_result(state, action_execution_id, &result)
                    {
                        return Ok(AttemptOutcome::OutputRejected(rejection));
                    }
                }
                Ok(AttemptOutcome::Completed(result))
            }

            Ok(ActionOutcome::TimedOut(timed_out)) => {
                match timed_out.resolve(&decision::permitted_labels(state)) {
                    Ok(result) => Ok(AttemptOutcome::Completed(result)),
                    Err(error) => Err(AttemptFailure::retriable(error.message)),
                }
            }

            Ok(ActionOutcome::Suspended(SuspendedAction::AwaitingHumanInput(request))) => {
                Ok(AttemptOutcome::AwaitingHumanInput(request))
            }

            Ok(ActionOutcome::Suspended(SuspendedAction::AwaitingAgentInput(request))) => {
                Ok(AttemptOutcome::AwaitingAgentInput(request))
            }

            Err(error) => match OutputRejection::from_error(state, action_execution_id, &error) {
                Some(rejection) => Ok(AttemptOutcome::OutputRejected(rejection)),
                None => {
                    let clean = self.attempt_left_no_trace(
                        kind,
                        &execution.id,
                        action_execution_id,
                        &error,
                    );
                    Err(AttemptFailure::retriable(error.message).side_effect_free_if(clean))
                }
            },
        }
    }

    /// Hands an already rendered action to its executor and closes the
    /// attempt's conversation once it is over.
    ///
    /// The outer `Err` is a failure before anything ran (no executor for the
    /// action type); the inner result is the executor's own.
    async fn execute_rendered(
        &self,
        execution: &WorkflowExecution,
        state: &State,
        rendered: &ActionConfig,
        action_execution_id: &str,
        timeout: Option<Duration>,
    ) -> Result<Result<ActionOutcome, ActionError>, AttemptFailure> {
        let kind = ActionKind::of(rendered);
        let executor = self.executors.get(&kind).ok_or_else(|| {
            AttemptFailure::permanent(format!(
                "state '{}' uses a {} action, which is not available in this version \
                 of narsor",
                state.id,
                kind.name()
            ))
        })?;

        let conversation_store = self.store.conversation_store(&execution.id);
        let request = ActionRequest::new(rendered, &execution.workspace, action_execution_id)
            .with_timeout(timeout)
            .with_execution_id(&execution.id)
            .with_conversation_store(&conversation_store)
            .with_outputs(&state.outputs);

        let outcome = executor.execute(request).await;

        // The attempt is over, one way or another (completed, suspended,
        // timed out, or failed outright): whatever it wrote to the
        // conversation store is final, so this is where it is closed and
        // compressed (R15). A script or human action never opened a writer,
        // so this is a harmless no-op for them. Closing must never fail the
        // attempt itself — same principle as `LoggingEventSink` recording an
        // event: the durable record of what happened is already safe, and a
        // compression failure is not a reason to also fail the workflow.
        if let Err(error) = conversation_store.close_attempt(action_execution_id) {
            tracing::warn!(
                action_execution_id,
                %error,
                "could not close/compress this attempt's conversation file"
            );
        }

        Ok(outcome)
    }

    // -----------------------------------------------------------------------
    // Format errors and the engine's reprompt (KTD7)
    // -----------------------------------------------------------------------

    /// Closes an attempt whose output was rejected: the record that lets a
    /// later pass continue with the reprompt, then `ActionOutputRejected`.
    ///
    /// In this order so that a crash between the two leaves the attempt open
    /// in the log — `Unknown`, as for any interrupted action — rather than a
    /// closed attempt with nothing to continue from, which the next pass would
    /// replay from the start of the state.
    fn reject_output(
        &self,
        execution: &mut WorkflowExecution,
        rejection: &OutputRejection,
    ) -> Result<(), EngineError> {
        let value = serde_json::to_value(rejection).map_err(|error| {
            EngineError::CorruptEngineMetadata {
                execution_id: execution.id.clone(),
                message: error.to_string(),
            }
        })?;
        engine_metadata_mut(execution).insert(METADATA_OUTPUT_REJECTION_KEY.to_string(), value);
        self.store.save_execution(execution)?;

        self.emit(
            &execution.id,
            EventData::ActionOutputRejected {
                action_execution_id: rejection.action_execution_id.clone(),
                state_id: rejection.state_id.clone(),
                reason: rejection.reason.clone(),
                answer_tail: rejection.answer_tail.clone(),
            },
        )?;
        Ok(())
    }

    /// Continues a step whose pass ended after its output was rejected and
    /// before the reprompt started (a daemon restart in between).
    async fn continue_rejected_output(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
        rejection: OutputRejection,
    ) -> Result<Flow, EngineError> {
        let Some(action) = &state.action else {
            clear_engine_metadata(execution, METADATA_OUTPUT_REJECTION_KEY);
            self.fail_action(
                execution,
                &rejection.action_execution_id,
                format!("state '{}' is not terminal but has no action", state.id),
            )?;
            return Ok(Flow::Stop);
        };
        let timeout = self
            .timeout_for(ActionKind::of(action), state, definition)
            .map(Duration::from_secs);

        match self
            .reprompt(definition, execution, state, action, rejection, timeout)
            .await?
        {
            ActionUpshot::Stopped => Ok(Flow::Stop),
            ActionUpshot::Transitioned => Ok(Flow::Continue),
            ActionUpshot::Completed {
                action_execution_id,
                result,
            } => self.record_completion(definition, execution, state, action_execution_id, result),
        }
    }

    /// The engine's one reprompt (R14): a new attempt resuming the rejected
    /// attempt's harness session, read-only, on the harness it ran on, asking
    /// only for the output object.
    ///
    /// Not a `retry_policy` retry (R15): it is announced by `OutputReprompted`,
    /// never `ActionRetrying`. Whatever it ends with but a label — another
    /// format error, a question, a failure — and a rejected attempt with no
    /// session to resume, hands the step to the conductor (F1,
    /// [`Engine::rescue_output`]).
    async fn reprompt(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
        action: &ActionConfig,
        rejection: OutputRejection,
        timeout: Option<Duration>,
    ) -> Result<ActionUpshot, EngineError> {
        let mut rescue = OutputRescue::after(&rejection);
        let resumable = match (action, &rejection.harness, &rejection.harness_session_id) {
            (ActionConfig::Agent(config), Some(harness), Some(session)) => Some(
                agent::reprompt_config(config, harness, session, &rejection.reason),
            ),
            _ => None,
        };
        let Some(config) = resumable else {
            rescue.reason = format!(
                "{} The agent could not be asked again for its output label: its harness \
                 reported no session to resume.",
                rejection.reason
            );
            self.begin_rescue(execution, &rescue)?;
            let flow = self
                .rescue_output(definition, execution, state, rescue, timeout)
                .await?;
            return Ok(ActionUpshot::settled(flow));
        };

        match self
            .reprompt_attempt(
                execution,
                state,
                config,
                &rejection.action_execution_id,
                RepromptedBy::Engine,
                timeout,
            )
            .await?
        {
            RepromptEnd::Labelled {
                action_execution_id,
                result,
            } => Ok(ActionUpshot::Completed {
                action_execution_id,
                result,
            }),
            RepromptEnd::Stopped => Ok(ActionUpshot::Stopped),
            RepromptEnd::Unlabelled(failure) => {
                rescue.record(&failure);
                // The record before the event that closes the attempt: a
                // crash in between leaves the attempt open (`Unknown`), and
                // one after it leaves the rescue to continue from (F1).
                self.begin_rescue(execution, &rescue)?;
                self.close_unlabelled(execution, state, &failure.answer)?;
                let flow = self
                    .rescue_output(definition, execution, state, rescue, timeout)
                    .await?;
                Ok(ActionUpshot::settled(flow))
            }
        }
    }

    /// One attempt asking the agent of `previous_attempt` again for its
    /// output label, resuming its session with `config` (KTD7) — the engine's
    /// reprompt or the conductor's (F1).
    ///
    /// An attempt that ends without a label is returned as it is, still
    /// open: closing it with `ActionOutputRejected` is the caller's, after
    /// it recorded what comes next.
    pub(super) async fn reprompt_attempt(
        &self,
        execution: &mut WorkflowExecution,
        state: &State,
        config: crate::domain::AgentActionConfig,
        previous_attempt: &str,
        by: RepromptedBy,
        timeout: Option<Duration>,
    ) -> Result<RepromptEnd, EngineError> {
        // Announced before the record goes: a crash in between leaves this
        // attempt open in the log (`Unknown`), never a closed attempt with
        // nothing left to continue from.
        let action_execution_id = Uuid::new_v4().to_string();
        execution.current_action_execution = Some(action_execution_id.clone());
        self.emit(
            &execution.id,
            EventData::OutputReprompted {
                action_execution_id: action_execution_id.clone(),
                previous_attempt: previous_attempt.to_string(),
                state_id: state.id.clone(),
                by,
            },
        )?;
        clear_engine_metadata(execution, METADATA_OUTPUT_REJECTION_KEY);
        self.store.save_execution(execution)?;

        let rendered = ActionConfig::Agent(config);
        let outcome = match self
            .execute_rendered(execution, state, &rendered, &action_execution_id, timeout)
            .await
        {
            Ok(outcome) => outcome,
            Err(failure) => {
                self.fail_action(execution, &action_execution_id, failure.message)?;
                return Ok(RepromptEnd::Stopped);
            }
        };

        let failure = match outcome {
            Ok(ActionOutcome::Completed(result)) => {
                match OutputRejection::from_result(state, &action_execution_id, &result) {
                    None => {
                        return Ok(RepromptEnd::Labelled {
                            action_execution_id,
                            result,
                        })
                    }
                    Some(rejection) => RepromptFailure::Rejected(rejection),
                }
            }
            Ok(ActionOutcome::TimedOut(timed_out)) => {
                match timed_out.resolve(&decision::permitted_labels(state)) {
                    Ok(result) => {
                        return Ok(RepromptEnd::Labelled {
                            action_execution_id,
                            result,
                        })
                    }
                    Err(error) => RepromptFailure::Failed(error.message),
                }
            }
            Ok(ActionOutcome::Suspended(
                SuspendedAction::AwaitingAgentInput(request)
                | SuspendedAction::AwaitingHumanInput(request),
            )) => RepromptFailure::Rejected(OutputRejection::from_question(
                state,
                &action_execution_id,
                &request.instruction,
            )),
            Err(error) => match OutputRejection::from_error(state, &action_execution_id, &error) {
                Some(rejection) => RepromptFailure::Rejected(rejection),
                None => RepromptFailure::Failed(error.message),
            },
        };

        // Either way the attempt is over without a label. A reprompt that
        // failed outright ends the same way — it is a failed reprompt, not an
        // action failure, and `retry_policy` does not apply to it.
        Ok(RepromptEnd::Unlabelled(match failure {
            RepromptFailure::Rejected(rejection) => UnlabelledAnswer {
                answer: RejectedAnswer::from(&rejection),
                reason: format!(
                    "the agent did not give a usable output label even when asked again: {}",
                    rejection.reason
                ),
                harness: rejection.harness,
                harness_session_id: rejection.harness_session_id,
            },
            RepromptFailure::Failed(message) => {
                let reason =
                    format!("asking the agent again for its output label failed: {message}");
                UnlabelledAnswer {
                    answer: RejectedAnswer {
                        action_execution_id: action_execution_id.clone(),
                        reason: reason.clone(),
                        answer_tail: None,
                    },
                    reason,
                    harness: None,
                    harness_session_id: None,
                }
            }
        }))
    }

    /// Closes a reprompt attempt that gave no label: `ActionOutputRejected`.
    pub(super) fn close_unlabelled(
        &self,
        execution: &WorkflowExecution,
        state: &State,
        answer: &RejectedAnswer,
    ) -> Result<(), EngineError> {
        self.emit(
            &execution.id,
            EventData::ActionOutputRejected {
                action_execution_id: answer.action_execution_id.clone(),
                state_id: state.id.clone(),
                reason: answer.reason.clone(),
                answer_tail: answer.answer_tail.clone(),
            },
        )?;
        Ok(())
    }

    /// Records `rescue` as the step's rescue under way, replacing a rejected
    /// output still owed its reprompt, and saves the execution.
    pub(super) fn begin_rescue(
        &self,
        execution: &mut WorkflowExecution,
        rescue: &OutputRescue,
    ) -> Result<(), EngineError> {
        let value =
            serde_json::to_value(rescue).map_err(|error| EngineError::CorruptEngineMetadata {
                execution_id: execution.id.clone(),
                message: error.to_string(),
            })?;
        clear_engine_metadata(execution, METADATA_OUTPUT_REJECTION_KEY);
        engine_metadata_mut(execution).insert(METADATA_OUTPUT_RESCUE_KEY.to_string(), value);
        self.store.save_execution(execution)?;
        Ok(())
    }

    /// Suspends a step whose agent gave no usable output label as a problem
    /// (KTD3): `InputNeeded` on a [`InputContinuation::ResolveOutput`]
    /// request carrying the agent's `answers`, then `ProblemRaised`.
    ///
    /// The last answer's attempt is the one the request suspends on; an
    /// answer to the request completes the step under that attempt.
    /// `analysis` is the conductor's reading of the answers, when it made
    /// one (U10).
    fn raise_output_problem(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
        answers: Vec<RejectedAnswer>,
        reason: String,
        analysis: Option<ConductorAnalysis>,
    ) -> Result<(), EngineError> {
        clear_engine_metadata(execution, METADATA_OUTPUT_REJECTION_KEY);
        clear_engine_metadata(execution, METADATA_OUTPUT_RESCUE_KEY);

        let action_execution_id = answers
            .last()
            .map(|answer| answer.action_execution_id.clone())
            .or_else(|| execution.current_action_execution.clone())
            .unwrap_or_default();
        let answer_tail = answers
            .iter()
            .rev()
            .find_map(|answer| answer.answer_tail.clone());
        let problem = OutputProblem {
            reason: reason.clone(),
            answers,
            requirements: output_requirements(definition, execution, state),
            analysis,
        };
        let request = HumanInputRequest {
            action_execution_id: action_execution_id.clone(),
            instruction: format!(
                "The agent of state '{}' did not give a usable output label. Choose the \
                 label its step ends with, and the data and variables it would have given.",
                state.id
            ),
            fields: Vec::new(),
            question: None,
            answer_tail,
        };
        let pending = PendingHumanInput::for_problem(
            execution.id.clone(),
            state.id.clone(),
            request,
            decision::permitted_outputs(state),
            problem,
        );

        let from = self.store.event_log(&execution.id).next_sequence()?;
        human::suspend(&self.store, execution, &pending)?;
        self.publish_from(&execution.id, from)?;

        self.emit(
            &execution.id,
            EventData::ProblemRaised {
                state_id: state.id.clone(),
                action_execution_id,
                kind: ProblemKind::Output,
                reason,
                episode: Some(pending.episode_key().to_string()),
            },
        )?;
        Ok(())
    }

    /// Records a finished action: label check, `ActionCompleted`, artifacts,
    /// and the mid-step state that makes the rest of the step resumable.
    fn record_completion(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
        action_execution_id: String,
        result: ActionResult,
    ) -> Result<Flow, EngineError> {
        match self.record_result(execution, state, action_execution_id, result)? {
            Some(pending) => self.complete_step(definition, execution, state, pending),
            None => Ok(Flow::Stop),
        }
    }

    /// The first half of [`Engine::record_completion`], up to the decision:
    /// returns the persisted mid-step state, or `None` when the label failed
    /// the action.
    fn record_result(
        &self,
        execution: &mut WorkflowExecution,
        state: &State,
        action_execution_id: String,
        result: ActionResult,
    ) -> Result<Option<PendingStep>, EngineError> {
        // A label the state does not declare is the engine's call, never the
        // executor's — this is the message the operator sees.
        if let Err(error) = decision::check_label(state, &result.output_label) {
            self.fail_action(execution, &action_execution_id, error.to_string())?;
            return Ok(None);
        }

        self.emit(
            &execution.id,
            EventData::ActionCompleted {
                action_execution_id: action_execution_id.clone(),
                output_label: result.output_label.clone(),
            },
        )?;

        let artifacts = self.register_artifacts(execution, &action_execution_id, &result)?;

        let pending = PendingStep {
            state_id: state.id.clone(),
            action_execution_id,
            result,
            artifacts,
        };
        self.write_pending_step(execution, &pending)?;
        self.store.save_execution(execution)?;

        Ok(Some(pending))
    }

    /// The second half of a step: decide, write the context, transition.
    fn complete_step(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
        pending: PendingStep,
    ) -> Result<Flow, EngineError> {
        let request = match decision::decide(state, &pending.result) {
            Ok(request) => request,
            Err(error) => {
                // A provider this version cannot use is a workflow-author
                // error, not an action failure: say so and stop.
                self.record_engine_error(execution, &error.to_string())?;
                return Err(EngineError::Decision(error));
            }
        };

        let outcome = match request {
            DecisionRequest::Resolved(outcome) => outcome,
            DecisionRequest::AwaitHuman { message } => {
                // The question comes straight from the agent that just
                // answered: its result is still at hand.
                let (question, answer_tail) = if is_agent(state) {
                    agent_answer(&pending.result)
                } else {
                    (None, None)
                };
                let request = HumanInputRequest {
                    action_execution_id: pending.action_execution_id.clone(),
                    instruction: message,
                    fields: Vec::new(),
                    question,
                    answer_tail,
                };
                self.suspend_for_input(
                    execution,
                    state,
                    request,
                    decision::permitted_outputs(state),
                )?;
                return Ok(Flow::Stop);
            }
        };

        self.apply_decision(definition, execution, state, pending, outcome)
    }

    /// `DecisionMade` → context (D10) → `TransitionTaken` → `state.json`.
    fn apply_decision(
        &self,
        definition: &WorkflowDefinition,
        execution: &mut WorkflowExecution,
        state: &State,
        pending: PendingStep,
        outcome: DecisionOutcome,
    ) -> Result<Flow, EngineError> {
        let target = match decision::check_label(state, &outcome.label) {
            Ok(target) => target.clone(),
            Err(error) => {
                self.fail_action(execution, &pending.action_execution_id, error.to_string())?;
                return Ok(Flow::Stop);
            }
        };

        self.emit(
            &execution.id,
            EventData::DecisionMade {
                provider: outcome.provider_name().to_string(),
                output_label: outcome.label.clone(),
                reasoning: outcome.reasoning.clone(),
            },
        )?;

        // The one place the context is written (D10). `save_execution` writes
        // `state.json` as well, because it embeds the context and is the
        // authority; writing `context.json` alone would leave a stale copy.
        execution
            .context
            .update_variables(pending.result.variables.clone());
        for artifact in &pending.artifacts {
            let key = artifact.name.clone().unwrap_or_else(|| artifact.id.clone());
            execution.context.add_artifact(
                key,
                ArtifactReference {
                    id: artifact.id.clone(),
                    name: artifact.name.clone(),
                    location: artifact.location.clone(),
                    metadata: artifact.metadata.clone(),
                },
            );
        }
        // Before `pending_step` goes: it holds the answer the record is made
        // of. Written from the same `save_execution` as the context, so the
        // record and the transition cannot disagree after a crash.
        let record = is_agent(state).then(|| {
            LastAgentStep::from_result(
                state.id.clone(),
                target.clone(),
                pending.action_execution_id.clone(),
                &pending.result,
            )
        });
        set_last_agent_step(execution, record.as_ref())?;
        self.clear_pending_step(execution);
        // A rescued step is over once its transition is taken (F1), and so
        // is the conductor's retry of a failed one (F3).
        clear_engine_metadata(execution, METADATA_OUTPUT_RESCUE_KEY);
        clear_engine_metadata(execution, METADATA_CONDUCTOR_RETRY_KEY);
        self.store.save_execution(execution)?;

        self.emit(
            &execution.id,
            EventData::TransitionTaken {
                from_state: state.id.clone(),
                to_state: target.clone(),
                output_label: outcome.label,
            },
        )?;

        execution.current_state = Some(target);
        execution.current_action_execution = Some(pending.action_execution_id);
        self.store.save_execution(execution)?;

        self.write_checkpoint(definition, execution);

        Ok(Flow::Continue)
    }

    /// Captures the workspace at a step boundary, if checkpointing is on.
    ///
    /// A failing checkpoint does not fail the step: the execution's own record
    /// is already durable, and refusing to go on because a *copy* could not be
    /// made would turn a recoverable situation into a stopped workflow. The
    /// failure is logged, and the previous checkpoint is left intact (R25).
    ///
    /// `preserve` comes from the workflow's own workspace declaration
    /// (`globals.workspace.preserve_paths`) — the definition, not the
    /// provisioned `Workspace` record, because the latter does not carry it
    /// (it only exists to describe *where* the workspace lives, not how it was
    /// declared).
    fn write_checkpoint(&self, definition: &WorkflowDefinition, execution: &WorkflowExecution) {
        if !self.checkpoints {
            return;
        }

        let base = match &execution.workspace.base_commit {
            Some(commit) => crate::workspace::Base::Commit(commit.clone()),
            None => crate::workspace::Base::None,
        };

        let preserve: &[String] = match &definition.workspace {
            Some(WorkspaceSpec::Git { preserve_paths, .. }) => preserve_paths,
            Some(WorkspaceSpec::Local { preserve_paths, .. }) => preserve_paths,
            None => &[],
        };

        let paths = self.store.paths();
        let request = crate::workspace::CaptureRequest {
            work_tree: std::path::Path::new(&execution.workspace.path),
            base,
            scratch: &paths.checkpoint_scratch(&execution.id),
            bundle: &paths.checkpoint_file(&execution.id),
            preserve,
        };

        if let Err(error) = crate::workspace::capture(request) {
            tracing::warn!(
                execution_id = %execution.id,
                %error,
                "could not checkpoint the workspace; the previous checkpoint is kept"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Endings
    // -----------------------------------------------------------------------

    /// Ends the execution in a terminal state (`DOMAIN_MODEL.md` §4.4).
    fn finish(&self, execution: &mut WorkflowExecution, state: &State) -> Result<(), EngineError> {
        let status = terminal_status(state);

        let ending = match status {
            ExecutionStatus::Cancelled => EventData::WorkflowCancelled {
                reason: format!("workflow ended in terminal state '{}'", state.id),
            },
            ExecutionStatus::Failed => EventData::WorkflowFailed {
                error: format!("workflow ended in failure terminal state '{}'", state.id),
            },
            _ => EventData::WorkflowCompleted {
                result: json!({
                    "terminal_state": state.id,
                    "variables": execution.context.variables,
                }),
            },
        };
        self.emit(&execution.id, ending)?;

        execution.status = status;
        execution.completed_at = Some(Utc::now());
        self.clear_pending_step(execution);
        clear_engine_metadata(execution, METADATA_CONDUCTOR_RETRY_KEY);
        self.store.save_execution(execution)?;
        Ok(())
    }

    /// Records a failed action and the failed execution that follows from it.
    ///
    /// For a failure met after the action ran — a label, a decision — so it
    /// counts as one that may have had side effects.
    fn fail_action(
        &self,
        execution: &mut WorkflowExecution,
        action_execution_id: &str,
        message: impl Into<String>,
    ) -> Result<(), EngineError> {
        self.fail_attempts(execution, action_execution_id, message, false)
    }

    /// Records the failed action whose last attempt was `action_execution_id`
    /// and the failed execution that follows; `side_effect_free` says whether
    /// none of its attempts can have acted (KTD14).
    fn fail_attempts(
        &self,
        execution: &mut WorkflowExecution,
        action_execution_id: &str,
        message: impl Into<String>,
        side_effect_free: bool,
    ) -> Result<(), EngineError> {
        let message = message.into();
        self.emit(
            &execution.id,
            EventData::ActionFailed {
                action_execution_id: action_execution_id.to_string(),
                error: message.clone(),
            },
        )?;
        self.fail_step(
            execution,
            Some(action_execution_id),
            message,
            side_effect_free,
        )
    }

    /// Ends the execution as `Failed`, with no transition taken.
    fn fail_execution(
        &self,
        execution: &mut WorkflowExecution,
        error: String,
    ) -> Result<(), EngineError> {
        self.fail_step(execution, None, error, false)
    }

    /// Ends the execution as `Failed` on its step's attempt `failed_attempt`,
    /// and hands the failure to the conductor (F3) — in the same save that
    /// makes it `Failed`, the record the conductor reads, then the trigger
    /// once it is durable.
    fn fail_step(
        &self,
        execution: &mut WorkflowExecution,
        failed_attempt: Option<&str>,
        error: String,
        side_effect_free: bool,
    ) -> Result<(), EngineError> {
        self.emit(
            &execution.id,
            EventData::WorkflowFailed {
                error: error.clone(),
            },
        )?;
        execution.status = ExecutionStatus::Failed;
        execution.completed_at = Some(Utc::now());
        let conductor =
            self.record_step_failure(execution, failed_attempt, &error, side_effect_free)?;
        self.clear_pending_step(execution);
        clear_engine_metadata(execution, METADATA_OUTPUT_REJECTION_KEY);
        clear_engine_metadata(execution, METADATA_OUTPUT_RESCUE_KEY);
        self.store.save_execution(execution)?;
        if let Some(conductor) = conductor {
            conductor.step_failed(&execution.id);
        }
        Ok(())
    }

    /// Records an engine-level problem and fails the execution on it.
    fn record_engine_error(
        &self,
        execution: &mut WorkflowExecution,
        message: &str,
    ) -> Result<(), EngineError> {
        self.emit(
            &execution.id,
            EventData::Error {
                message: message.to_string(),
                context: json!({ "state": execution.current_state }),
            },
        )?;
        self.fail_execution(execution, message.to_string())
    }

    /// Parks the execution: `Paused` plus `WorkflowPaused`.
    fn pause_execution(
        &self,
        execution: &mut WorkflowExecution,
        reason: &str,
    ) -> Result<(), EngineError> {
        execution.status = ExecutionStatus::Paused;
        self.store.save_execution(execution)?;
        self.emit(
            &execution.id,
            EventData::WorkflowPaused {
                reason: reason.to_string(),
            },
        )?;
        Ok(())
    }

    /// Suspends the execution on a `human` gate — a `human` action or a
    /// `human` decision — and records it durably.
    fn suspend_for_input(
        &self,
        execution: &mut WorkflowExecution,
        state: &State,
        request: HumanInputRequest,
        outputs: Vec<OutputDefinition>,
    ) -> Result<(), EngineError> {
        let pending =
            PendingHumanInput::new(execution.id.clone(), state.id.clone(), request, outputs);
        let from = self.store.event_log(&execution.id).next_sequence()?;
        human::suspend(&self.store, execution, &pending)?;
        self.publish_from(&execution.id, from)?;
        // The conductor summarizes the gate beside it (R30): advisory, after
        // the gate is durable, and never an answer — the gate waits for the
        // operator whatever the summary says.
        if let Some(conductor) = self.conductor() {
            conductor.gate_suspended(&execution.id, &pending);
        }
        Ok(())
    }

    /// Pauses an agent state in place. No output label is produced and no
    /// workflow transition is selected; `deliver_human_input` later records
    /// the answer and runs this same state again.
    fn suspend_for_agent_input(
        &self,
        execution: &mut WorkflowExecution,
        state: &State,
        request: HumanInputRequest,
    ) -> Result<(), EngineError> {
        let pending = PendingHumanInput::for_agent(execution.id.clone(), state.id.clone(), request);
        let from = self.store.event_log(&execution.id).next_sequence()?;
        human::suspend(&self.store, execution, &pending)?;
        self.publish_from(&execution.id, from)
    }

    // -----------------------------------------------------------------------
    // Artifacts
    // -----------------------------------------------------------------------

    /// Turns the `artifacts` list a script reported into artifact records.
    ///
    /// [`ActionResult`] has no artifacts field, so `ScriptExecutor` passes the
    /// §3.3.B list through as `data.artifacts`. Each entry becomes an
    /// `ArtifactCreated` event here and an entry in `context.artifacts` when the
    /// context is written. An entry that names no location is skipped rather
    /// than failing the action: a malformed artifact note is not a reason to
    /// discard work that succeeded.
    fn register_artifacts(
        &self,
        execution: &WorkflowExecution,
        action_execution_id: &str,
        result: &ActionResult,
    ) -> Result<Vec<Artifact>, EngineError> {
        let Some(entries) = result.data.get("artifacts").and_then(Value::as_array) else {
            return Ok(Vec::new());
        };

        let mut artifacts = Vec::new();
        for entry in entries {
            let Some(artifact) = artifact_from(entry, action_execution_id) else {
                continue;
            };

            self.emit(
                &execution.id,
                EventData::ArtifactCreated {
                    artifact_id: artifact.id.clone(),
                    artifact_type: artifact.artifact_type.clone(),
                    location: artifact.location.clone(),
                },
            )?;
            artifacts.push(artifact);
        }
        Ok(artifacts)
    }

    // -----------------------------------------------------------------------
    // Events
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // Small helpers
    // -----------------------------------------------------------------------

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

    /// The mid-step state of this execution, when it belongs to `state_id`.
    fn pending_step(
        &self,
        execution: &WorkflowExecution,
        state_id: &str,
    ) -> Result<Option<PendingStep>, EngineError> {
        stored_pending_step(execution, state_id)
    }

    /// Persists the mid-step state inside the execution record.
    fn write_pending_step(
        &self,
        execution: &mut WorkflowExecution,
        pending: &PendingStep,
    ) -> Result<(), EngineError> {
        write_engine_record(execution, METADATA_PENDING_STEP_KEY, pending)
    }

    /// Drops the mid-step state.
    fn clear_pending_step(&self, execution: &mut WorkflowExecution) {
        clear_engine_metadata(execution, METADATA_PENDING_STEP_KEY);
    }

    /// Removes and persists the note destined for the next agent attempt.
    fn take_operator_recovery_note(
        &self,
        execution: &mut WorkflowExecution,
    ) -> Result<Option<String>, AttemptFailure> {
        let Some(engine) = execution
            .metadata
            .get_mut(METADATA_ENGINE_KEY)
            .and_then(Value::as_object_mut)
        else {
            return Ok(None);
        };
        let note = engine
            .remove(METADATA_OPERATOR_RECOVERY_NOTE_KEY)
            .and_then(|value| value.as_str().map(ToString::to_string));
        if note.is_some() {
            self.store
                .save_execution(execution)
                .map_err(|error| AttemptFailure::permanent(error.to_string()))?;
        }
        Ok(note)
    }

    /// Stores an answer for one agent state needing input. It lives in engine metadata
    /// rather than workflow context: it is transient control data, not a
    /// variable a later workflow step should inherit.
    fn store_agent_input(
        &self,
        execution: &mut WorkflowExecution,
        state_id: &str,
        answer: &str,
    ) -> Result<(), EngineError> {
        engine_metadata_mut(execution).insert(
            METADATA_AGENT_INPUT_KEY.to_string(),
            json!({ "state_id": state_id, "answer": answer }),
        );
        self.store.save_execution(execution)?;
        Ok(())
    }

    /// Consumes only an answer for the state currently executing. A mismatched
    /// stale record is retained instead of leaking an answer into another
    /// state after a manual transition.
    fn take_agent_input(
        &self,
        execution: &mut WorkflowExecution,
        state: &State,
    ) -> Result<Option<String>, AttemptFailure> {
        let Some(engine) = execution
            .metadata
            .get_mut("engine")
            .and_then(Value::as_object_mut)
        else {
            return Ok(None);
        };
        let Some(record) = engine
            .get(METADATA_AGENT_INPUT_KEY)
            .and_then(Value::as_object)
        else {
            return Ok(None);
        };
        if record.get("state_id").and_then(Value::as_str) != Some(state.id.as_str()) {
            return Ok(None);
        }
        let answer = record
            .get("answer")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        if answer.is_some() {
            engine.remove(METADATA_AGENT_INPUT_KEY);
            self.store
                .save_execution(execution)
                .map_err(|error| AttemptFailure::permanent(error.to_string()))?;
        }
        Ok(answer)
    }

    /// Consumes a pending pause request, if there is one.
    fn take_pause_request(&self, execution_id: &str) -> bool {
        self.lock_pause_requests().remove(execution_id)
    }

    /// Locks the pause requests, recovering from a poisoned mutex: a panic
    /// elsewhere is no reason to make pausing impossible.
    fn lock_pause_requests(&self) -> std::sync::MutexGuard<'_, HashSet<ExecutionId>> {
        self.pause_requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Claims the right to drive `execution_id`, or refuses if another pass
    /// already holds it.
    ///
    /// The check and the insert happen under the one lock this returns from
    /// holding, which is what makes two concurrent callers unable to both
    /// succeed (R7): a separate check-then-insert would leave a gap between
    /// the two a second caller could land in.
    fn begin_run(&self, execution_id: &str) -> Result<RunGuard<'_>, EngineError> {
        if !self.lock_active_runs().insert(execution_id.to_string()) {
            return Err(EngineError::AlreadyRunning {
                execution_id: execution_id.to_string(),
            });
        }
        Ok(RunGuard {
            engine: self,
            execution_id: execution_id.to_string(),
        })
    }

    /// The owned equivalent of [`Engine::begin_run`]: claims the right to
    /// drive `execution_id` through an [`ActiveRunClaim`] that lives inside
    /// an [`Arc<Engine>`] rather than borrowing `&self`, so it can be moved
    /// into a `tokio::spawn`-ed task. Operates on the exact same
    /// `active_runs` set as [`Engine::begin_run`], so the two can never both
    /// succeed for the same execution.
    fn claim_active_run(
        self: &Arc<Self>,
        execution_id: &str,
    ) -> Result<ActiveRunClaim, EngineError> {
        if !self.lock_active_runs().insert(execution_id.to_string()) {
            return Err(EngineError::AlreadyRunning {
                execution_id: execution_id.to_string(),
            });
        }
        Ok(ActiveRunClaim {
            engine: Arc::clone(self),
            execution_id: execution_id.to_string(),
        })
    }

    /// Locks the active-run set, recovering from a poisoned mutex: a panic
    /// elsewhere is no reason to make every future run impossible.
    fn lock_active_runs(&self) -> std::sync::MutexGuard<'_, HashSet<ExecutionId>> {
        self.active_runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Runs `future`, one of the three public entry points, and makes sure any
    /// `Err` it returns is also noted as an event (R5) before it reaches the
    /// caller — which is what makes the failure observable to a caller that,
    /// like the TUI's background spawns, discards the `Result` itself.
    async fn observed<F>(
        &self,
        execution_id: &str,
        future: F,
    ) -> Result<WorkflowExecution, EngineError>
    where
        F: std::future::Future<Output = Result<WorkflowExecution, EngineError>>,
    {
        let outcome = future.await;
        if let Err(error) = &outcome {
            self.note_engine_error(execution_id, error);
        }
        outcome
    }

    /// Emits an [`EventData::Error`] against `execution_id`, best-effort.
    ///
    /// Used to make an [`EngineError`] that would otherwise only reach the
    /// caller's `Result` observable through the event stream as well (R5), for
    /// callers — background TUI spawns — that discard the `Result`. Skips
    /// emitting for the one case where `execution_id` never resolved to
    /// anything on disk (there is no log to identify), rather than inventing
    /// one; otherwise, a failure to emit never masks or replaces the original
    /// error — it is only ever logged, not returned.
    fn note_engine_error(&self, execution_id: &str, error: &EngineError) {
        if matches!(
            error,
            EngineError::Persistence(PersistenceError::ExecutionNotFound { .. })
        ) {
            return;
        }
        if let Err(emit_error) = self.emit(
            execution_id,
            EventData::Error {
                message: error.to_string(),
                context: json!({}),
            },
        ) {
            tracing::warn!(
                execution_id,
                %error,
                %emit_error,
                "could not record an engine-level error as an event"
            );
        }
    }

    /// The error for an operation the execution's status does not allow.
    fn not_runnable(&self, execution: &WorkflowExecution, operation: &'static str) -> EngineError {
        if execution.status == ExecutionStatus::InputNeeded
            && self.store.has_pending_input(&execution.id)
        {
            return EngineError::AwaitingHumanInput {
                execution_id: execution.id.clone(),
            };
        }
        EngineError::NotRunnable {
            execution_id: execution.id.clone(),
            status: execution.status.as_str().to_string(),
            operation,
        }
    }
}

/// Converts the agent-response form into the text injected into the next
/// harness turn. A choice is clearly distinguished from arbitrary prose, and
/// an optional comment stays attached to that choice.
fn agent_reply(values: &Map<String, Value>) -> Option<String> {
    let text = |name: &str| {
        values
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    if let Some(answer) = text("answer") {
        return Some(answer.to_string());
    }
    let option = text("option")?;
    let mut reply = format!("Selected option: {option}");
    if let Some(comment) = text("comment") {
        reply.push_str("\nComment: ");
        reply.push_str(comment);
    }
    Some(reply)
}

/// Builds an artifact record from one entry of a `data.artifacts` list.
///
/// Accepts either a bare path or an object with `path`/`location`, and
/// optionally `name` and `type`.
fn artifact_from(entry: &Value, action_execution_id: &str) -> Option<Artifact> {
    let (location, name, artifact_type, metadata) = match entry {
        Value::String(path) => (path.clone(), None, "file".to_string(), json!({})),
        Value::Object(object) => {
            let location = object
                .get("path")
                .or_else(|| object.get("location"))
                .and_then(Value::as_str)?
                .to_string();
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .map(ToString::to_string);
            let artifact_type = object
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("file")
                .to_string();
            (location, name, artifact_type, entry.clone())
        }
        _ => return None,
    };

    if location.is_empty() {
        return None;
    }

    let mut artifact = Artifact::new(artifact_type, location, action_execution_id.to_string());
    artifact.name = name;
    artifact.metadata = metadata;
    Some(artifact)
}

/// The mid-step state of `execution`, when it belongs to `state_id`.
fn stored_pending_step(
    execution: &WorkflowExecution,
    state_id: &str,
) -> Result<Option<PendingStep>, EngineError> {
    Ok(
        read_engine_record::<PendingStep>(execution, METADATA_PENDING_STEP_KEY)?
            .filter(|pending| pending.state_id == state_id),
    )
}

/// The rejected agent output `execution` still owes a reprompt at its current
/// state (KTD7), if any.
///
/// For the daemon's startup hook: an execution left `Running` with such a
/// record stopped between the rejection and the reprompt, and a new pass
/// picks the reprompt up rather than replaying the attempt.
pub fn owed_reprompt(
    execution: &WorkflowExecution,
) -> Result<Option<OutputRejection>, EngineError> {
    match execution.current_state.as_deref() {
        Some(state_id) => stored_output_rejection(execution, state_id),
        None => Ok(None),
    }
}

/// The rescue of an output label (F1) `execution` still owes at its current
/// state, if any.
///
/// For the daemon's startup hook: an execution left `Running` with such a
/// record stopped while the conductor worked on its step — before it was
/// asked, while it was invoked, or after its decision was saved — and a new
/// pass carries the rescue on.
pub fn owed_output_rescue(
    execution: &WorkflowExecution,
) -> Result<Option<OutputRescue>, EngineError> {
    match execution.current_state.as_deref() {
        Some(state_id) => stored_output_rescue(execution, state_id),
        None => Ok(None),
    }
}

/// The rescue of `execution`'s output label under way, when it belongs to
/// `state_id`.
fn stored_output_rescue(
    execution: &WorkflowExecution,
    state_id: &str,
) -> Result<Option<OutputRescue>, EngineError> {
    Ok(
        read_engine_record::<OutputRescue>(execution, METADATA_OUTPUT_RESCUE_KEY)?
            .filter(|rescue| rescue.state_id == state_id),
    )
}

/// The rejected agent output of `execution` still owed its reprompt, when it
/// belongs to `state_id`.
fn stored_output_rejection(
    execution: &WorkflowExecution,
    state_id: &str,
) -> Result<Option<OutputRejection>, EngineError> {
    Ok(
        read_engine_record::<OutputRejection>(execution, METADATA_OUTPUT_REJECTION_KEY)?
            .filter(|rejection| rejection.state_id == state_id),
    )
}

/// Reads one record of the engine's scratch area.
pub(super) fn read_engine_record<T: serde::de::DeserializeOwned>(
    execution: &WorkflowExecution,
    key: &str,
) -> Result<Option<T>, EngineError> {
    let Some(value) = execution
        .metadata
        .get(METADATA_ENGINE_KEY)
        .and_then(|engine| engine.get(key))
    else {
        return Ok(None);
    };
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|error| EngineError::CorruptEngineMetadata {
            execution_id: execution.id.clone(),
            message: error.to_string(),
        })
}

/// Writes one record of the engine's scratch area.
pub(super) fn write_engine_record<T: Serialize>(
    execution: &mut WorkflowExecution,
    key: &str,
    record: &T,
) -> Result<(), EngineError> {
    let value =
        serde_json::to_value(record).map_err(|error| EngineError::CorruptEngineMetadata {
            execution_id: execution.id.clone(),
            message: error.to_string(),
        })?;
    engine_metadata_mut(execution).insert(key.to_string(), value);
    Ok(())
}

/// Drops one key of the engine's scratch area, if it is there.
fn clear_engine_metadata(execution: &mut WorkflowExecution, key: &str) {
    if let Some(engine) = execution
        .metadata
        .get_mut(METADATA_ENGINE_KEY)
        .and_then(Value::as_object_mut)
    {
        engine.remove(key);
    }
}

/// What choosing each label of `state` requires of an answer to a problem
/// (KTD15, R18), worked out from the state each label leads to.
///
/// A `human` gate shows the question the agent left open, so it needs
/// `data.question`. Every `context.NAME` the target's templates use needs a
/// value — unless the context already holds one, since the target then
/// renders as it is.
fn output_requirements(
    definition: &WorkflowDefinition,
    execution: &WorkflowExecution,
    state: &State,
) -> Vec<OutputRequirement> {
    state
        .outputs
        .iter()
        .map(|output| {
            let target = state
                .transitions
                .get(&output.label)
                .and_then(|target| definition.states.get(target));
            let action = target.and_then(|target| target.action.as_ref());
            let data = match action {
                Some(ActionConfig::Human(_)) => vec!["question".to_string()],
                _ => Vec::new(),
            };
            let variables = action
                .map(action_context_references)
                .unwrap_or_default()
                .into_iter()
                .filter(|name| {
                    !execution
                        .context
                        .variables
                        .get(name)
                        .is_some_and(|value| !human::is_blank(value))
                })
                .collect();
            OutputRequirement {
                label: output.label.clone(),
                data,
                variables,
            }
        })
        .collect()
}

/// Whether `state` runs an agent.
fn is_agent(state: &State) -> bool {
    matches!(state.action, Some(ActionConfig::Agent(_)))
}

/// The `data.question` an agent stated and the end of its answer.
///
/// The answer is `stdout`, which keeps the head of an answer longer than the
/// capture limit; its tail is then the end of what was kept.
fn agent_answer(result: &ActionResult) -> (Option<String>, Option<String>) {
    let question = result
        .data
        .get("question")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|question| !question.is_empty())
        .map(ToString::to_string);
    let answer_tail =
        Some(harness::tail(&result.stdout, ANSWER_TAIL_BYTES)).filter(|tail| !tail.is_empty());
    (question, answer_tail)
}

/// Replaces the record of the agent step behind the latest transition, or
/// drops it when that transition did not leave an agent.
fn set_last_agent_step(
    execution: &mut WorkflowExecution,
    record: Option<&LastAgentStep>,
) -> Result<(), EngineError> {
    match record {
        Some(record) => {
            let value = serde_json::to_value(record).map_err(|error| {
                EngineError::CorruptEngineMetadata {
                    execution_id: execution.id.clone(),
                    message: error.to_string(),
                }
            })?;
            engine_metadata_mut(execution).insert(METADATA_LAST_AGENT_STEP_KEY.to_string(), value);
        }
        None => {
            if let Some(engine) = execution
                .metadata
                .get_mut(METADATA_ENGINE_KEY)
                .and_then(Value::as_object_mut)
            {
                engine.remove(METADATA_LAST_AGENT_STEP_KEY);
            }
        }
    }
    Ok(())
}

/// The agent step whose transition led straight into `state`, if there is one.
///
/// Every transition rewrites or drops the record, so one naming `state` as
/// its destination describes the transition that entered it — never a
/// question from an earlier pass through a loop. An unreadable record is
/// only lost context, not a reason to refuse the suspension.
fn preceding_agent_step(execution: &WorkflowExecution, state: &State) -> Option<LastAgentStep> {
    let value = execution
        .metadata
        .get(METADATA_ENGINE_KEY)?
        .get(METADATA_LAST_AGENT_STEP_KEY)?;
    let record: LastAgentStep = match serde_json::from_value(value.clone()) {
        Ok(record) => record,
        Err(error) => {
            tracing::warn!(
                execution_id = %execution.id,
                %error,
                "the record of the last agent step is unreadable; the input request goes without it"
            );
            return None;
        }
    };
    (record.to_state == state.id).then_some(record)
}

/// Records the step behind a manual transition from `from` into `to`, as
/// [`Engine`] does for every transition it takes itself (KTD8).
///
/// A manual transition takes no decision, so the agent's answer is wherever
/// the state left it: in `pending_step` when the agent finished and waited
/// for a decision, or in `pending_input` when it asked a question of its own
/// (then there is a question but no answer to quote) or when its step is a
/// problem (then there is an answer but no question). Leaving any other
/// state drops the record.
pub fn record_manual_transition(
    execution: &mut WorkflowExecution,
    from: &State,
    to: &str,
    pending_input: Option<&PendingHumanInput>,
) -> Result<(), EngineError> {
    let record = if is_agent(from) {
        if let Some(pending) = stored_pending_step(execution, &from.id)? {
            Some(LastAgentStep::from_result(
                from.id.clone(),
                to.to_string(),
                pending.action_execution_id,
                &pending.result,
            ))
        } else {
            pending_input
                .filter(|input| input.state_id == from.id)
                .and_then(|input| match input.continuation {
                    InputContinuation::ResumeAgent => Some(LastAgentStep {
                        state_id: from.id.clone(),
                        to_state: to.to_string(),
                        action_execution_id: input.action_execution_id().to_string(),
                        question: Some(input.instruction().to_string()),
                        answer_tail: None,
                    }),
                    // A problem left by hand: no question, but the answer
                    // the agent did give.
                    InputContinuation::ResolveOutput => Some(LastAgentStep {
                        state_id: from.id.clone(),
                        to_state: to.to_string(),
                        action_execution_id: input.action_execution_id().to_string(),
                        question: None,
                        answer_tail: input.request.answer_tail.clone(),
                    }),
                    InputContinuation::CompleteStep => None,
                })
        }
    } else {
        None
    };
    // A manual transition abandons a rejected output still owed a reprompt,
    // a rescue the conductor had not finished, and a failure it diagnoses.
    clear_engine_metadata(execution, METADATA_OUTPUT_REJECTION_KEY);
    clear_engine_metadata(execution, METADATA_OUTPUT_RESCUE_KEY);
    clear_engine_metadata(execution, METADATA_STEP_FAILURE_KEY);
    clear_engine_metadata(execution, METADATA_CONDUCTOR_RETRY_KEY);
    set_last_agent_step(execution, record.as_ref())
}

/// Returns the engine-owned metadata object, normalizing records written
/// before `metadata` or its engine subobject were guaranteed to be objects.
fn engine_metadata_mut(execution: &mut WorkflowExecution) -> &mut serde_json::Map<String, Value> {
    if !execution.metadata.is_object() {
        execution.metadata = json!({});
    }
    let metadata = execution
        .metadata
        .as_object_mut()
        .expect("metadata was just made an object");
    let engine = metadata
        .entry(METADATA_ENGINE_KEY.to_string())
        .or_insert_with(|| json!({}));
    if !engine.is_object() {
        *engine = json!({});
    }
    engine
        .as_object_mut()
        .expect("engine metadata was just made an object")
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::domain::TerminalStatus;

    fn terminal(id: &str, declared: Option<TerminalStatus>) -> State {
        State {
            id: id.to_string(),
            description: None,
            action: None,
            outputs: Vec::new(),
            transitions: HashMap::new(),
            decision: None,
            timeout_secs: None,
            retry_policy: None,
            terminal: true,
            terminal_status: declared,
            allow_manual_override: false,
            image: None,
        }
    }

    #[test]
    fn a_terminal_state_ends_the_execution_as_it_declares() {
        assert_eq!(
            terminal_status(&terminal("completed", Some(TerminalStatus::Completed))),
            ExecutionStatus::Completed
        );
        assert_eq!(
            terminal_status(&terminal("failed", Some(TerminalStatus::Failed))),
            ExecutionStatus::Failed
        );
        assert_eq!(
            terminal_status(&terminal("gave_up", Some(TerminalStatus::Cancelled))),
            ExecutionStatus::Cancelled
        );
    }

    #[test]
    fn an_undeclared_terminal_completes_whatever_it_is_called() {
        // The id is never consulted — this is exactly the guessing that was
        // removed, and `failed` here is the deliberate consequence of it.
        assert_eq!(
            terminal_status(&terminal("done", None)),
            ExecutionStatus::Completed
        );
        assert_eq!(
            terminal_status(&terminal("failed", None)),
            ExecutionStatus::Completed
        );
        assert_eq!(
            terminal_status(&terminal("rejected_by_reviewer", None)),
            ExecutionStatus::Completed
        );
    }

    #[test]
    fn an_artifact_can_be_a_bare_path_or_an_object() {
        let bare = artifact_from(&json!("reports/coverage.html"), "action-1").expect("artifact");
        assert_eq!(bare.location, "reports/coverage.html");
        assert_eq!(bare.artifact_type, "file");
        assert_eq!(bare.producer, "action-1");

        let described = artifact_from(
            &json!({ "name": "report", "type": "text/markdown", "path": "plan.md" }),
            "action-1",
        )
        .expect("artifact");
        assert_eq!(described.name.as_deref(), Some("report"));
        assert_eq!(described.artifact_type, "text/markdown");
        assert_eq!(described.location, "plan.md");

        assert!(artifact_from(&json!({ "name": "no location" }), "action-1").is_none());
        assert!(artifact_from(&json!(42), "action-1").is_none());
    }
}
