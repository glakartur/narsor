//! Event model
//!
//! Represents events in the workflow execution history.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
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
    /// A failed attempt that still has retries left (R4, R7, R11).
    ///
    /// Emitted right after the failed attempt and before the backoff sleep, so
    /// an operator watching the log sees the failure the moment it happens
    /// rather than only after the delay. The attempt that exhausts the retry
    /// budget does **not** get one of these — it goes straight to
    /// [`EventData::ActionFailed`], since there is no retry to announce.
    ActionAttemptFailed {
        /// The attempt that failed.
        action_execution_id: String,
        /// The state whose action failed.
        state_id: String,
        /// The error message, same as `ActionFailed` would carry if this were
        /// the last attempt.
        error: String,
        /// How long the engine will wait before the next attempt starts.
        retry_in_ms: u64,
    },
    ActionRetrying {
        action_execution_id: String,
        retry_count: u32,
        /// The state whose action is being retried.
        ///
        /// `#[serde(default)]` so an event log written before this field
        /// existed still deserializes, with `None` standing for "unknown".
        #[serde(default)]
        state_id: Option<String>,
    },
    /// An agent attempt ended without an output label the state accepts
    /// (KTD7): no JSON object, no `output`, or a label the state does not
    /// declare.
    ///
    /// A format error, not an action failure: it closes the attempt (D23 and
    /// the conversation sweep treat it like `ActionCompleted`/`ActionFailed`)
    /// without consuming the state's `retry_policy`. What follows is a
    /// reprompt ([`EventData::OutputReprompted`]) or `ActionFailed`.
    ActionOutputRejected {
        /// The attempt whose output was rejected.
        action_execution_id: String,
        /// The state whose action ran.
        state_id: String,
        /// Why the output was rejected.
        reason: String,
        /// The end of the agent's answer.
        #[serde(default)]
        answer_tail: Option<String>,
    },
    /// A new attempt asks the agent of a rejected attempt for its output label
    /// again, by resuming its harness session (KTD7).
    ///
    /// Opens an attempt the way `ActionRetrying` does, with a fresh
    /// `action_execution_id` (D24), but it is deliberately not a retry: the
    /// state's `retry_policy` counter does not move.
    OutputReprompted {
        /// The new attempt.
        action_execution_id: String,
        /// The attempt whose output was rejected.
        previous_attempt: String,
        /// The state whose action is asked again.
        state_id: String,
        /// Who asked.
        by: RepromptedBy,
    },

    // Agent events
    AgentSessionStarted {
        session_id: String,
        harness: String,
        /// The action attempt this session belongs to.
        ///
        /// `#[serde(default)]` so an event log written before this field
        /// existed still deserializes, with `None` standing for "unknown".
        #[serde(default)]
        action_execution_id: Option<String>,
        /// The harness session this run resumed, if `resume_session` named one
        /// (D18).
        #[serde(default)]
        resumed_session: Option<String>,
    },
    AgentSessionCompleted {
        session_id: String,
        /// The action attempt this session belongs to.
        #[serde(default)]
        action_execution_id: Option<String>,
        /// The harness's own session id, captured from its output.
        #[serde(default)]
        harness_session_id: Option<String>,
    },
    AgentSessionFailed {
        session_id: String,
        error: String,
        /// The action attempt this session belongs to.
        #[serde(default)]
        action_execution_id: Option<String>,
        /// The harness's own session id, when the harness reported one before
        /// failing.
        #[serde(default)]
        harness_session_id: Option<String>,
    },
    AgentMessage {
        session_id: String,
        content: String,
        role: String,
        /// The action attempt this message belongs to.
        #[serde(default)]
        action_execution_id: Option<String>,
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
        /// The `data.question` of the agent step the request follows, if any.
        /// `#[serde(default)]` so an event log written before this field
        /// still reads.
        #[serde(default)]
        question: Option<String>,
        /// The end of that agent step's answer.
        #[serde(default)]
        answer_tail: Option<String>,
        /// The key of the pending input this request opened (KTD9): a
        /// delivery carrying another key answers a request that is gone.
        #[serde(default)]
        episode: Option<String>,
    },
    /// A pending input stopped waiting: it was answered, or something else
    /// closed it (KTD9, R5).
    ///
    /// Every field but `input` is `#[serde(default)]`, so an event log
    /// written before they existed still reads — as an answer from the
    /// operator, which is all an input could be closed by back then.
    HumanInputReceived {
        /// The form's field values; empty when the input was closed without
        /// an answer, or answered with a label alone.
        input: serde_json::Value,
        /// Who closed it.
        #[serde(default)]
        answered_by: Option<AnsweredBy>,
        /// How it was closed; `None` in older logs means answered.
        #[serde(default)]
        close_reason: Option<InputCloseReason>,
        /// The key of the pending input that was closed.
        #[serde(default)]
        episode: Option<String>,
        /// The output label the answer chose explicitly, if it chose one.
        #[serde(default)]
        output_label: Option<String>,
        /// The step's `data`, when the answer resolved a problem.
        #[serde(default)]
        data: Option<serde_json::Value>,
        /// The step's `variables`, when the answer resolved a problem.
        #[serde(default)]
        variables: Option<serde_json::Value>,
        /// A free-text remark that came with the answer.
        #[serde(default)]
        comment: Option<String>,
    },
    HumanTransitionSelected {
        from_state: String,
        to_state: String,
    },

    /// A step needs an operator to settle what it ended with (KTD3, KTD4).
    ///
    /// Announces every new problem, so a client learns of one from the
    /// stream: for [`ProblemKind::Output`] the execution is `InputNeeded`
    /// on a pending input whose key is `episode`; for
    /// [`ProblemKind::Failure`] it stays `Failed`.
    ProblemRaised {
        /// The state the problem is in.
        state_id: String,
        /// The last attempt of that state's action.
        action_execution_id: String,
        /// What kind of problem it is.
        kind: ProblemKind,
        /// Why the step could not go on by itself.
        reason: String,
        /// The key of the pending input that asks the operator, if one does.
        #[serde(default)]
        episode: Option<String>,
    },

    // Conductor events (D55)
    /// The conductor began one invocation for this execution (R11).
    ///
    /// Not an action attempt: it opens nothing D23 reconciles, and the
    /// conductor's own marker is what survives a restart (KTD3, KTD11).
    ConductorStarted {
        /// The invocation this event opens; [`EventData::ConductorFinished`]
        /// names the same one.
        invocation_id: String,
        /// What asked for the invocation.
        trigger: ConductorTrigger,
        /// What the conductor is asked to do.
        purpose: ConductorPurpose,
        /// The episode the invocation is about (KTD9), when it is about one.
        #[serde(default)]
        episode: Option<String>,
    },
    /// The conductor's invocation ended, one way or another.
    ConductorFinished {
        /// The invocation that ended.
        invocation_id: String,
        /// How it ended.
        outcome: ConductorOutcome,
        /// Why it failed, when it did.
        #[serde(default)]
        error: Option<String>,
    },
    /// The conductor decided something about the execution (R31): recorded
    /// with its confidence and rationale so it can be read back — and, for
    /// a chosen output, undone (KTD13).
    ConductorDecision {
        /// The decision, as the conductor's store keys it.
        decision_id: String,
        /// What was decided.
        kind: ConductorDecisionKind,
        /// The state the decision is about.
        #[serde(default)]
        state_id: Option<String>,
        /// The output label chosen, for a decision that chooses one.
        #[serde(default)]
        output_label: Option<String>,
        /// The episode the decision belongs to (KTD9).
        #[serde(default)]
        episode: Option<String>,
        /// How sure the conductor was; absent means `low` (KTD6).
        #[serde(default)]
        confidence: Confidence,
        /// Why.
        #[serde(default)]
        rationale: Option<String>,
    },
    /// An operator undid a chosen output (a) of the conductor's (KTD13): the
    /// context from before it is back, and the step it settled is a
    /// problem again at the same state. Its agent step is not run again.
    ConductorDecisionUndone {
        /// The decision undone, as [`EventData::ConductorDecision`] named it.
        decision_id: String,
        /// The state the decision was about, and the problem is raised in.
        state_id: String,
        /// What undoing could not take back: a step after the decision that
        /// had already started, whose side effects stay.
        #[serde(default)]
        warning: Option<String>,
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

/// Who asked an agent again for its output label (KTD7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepromptedBy {
    /// The engine's one generic reprompt.
    Engine,
    /// The conductor's reprompt with its own wording.
    Conductor,
}

impl RepromptedBy {
    /// The name as the event log spells it.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Engine => "engine",
            Self::Conductor => "conductor",
        }
    }
}

/// Who closed a pending input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnsweredBy {
    /// A person, through the API (the TUI or the CLI).
    #[default]
    Operator,
    /// The conductor, acting on its own or on a confirmed proposal.
    Conductor,
}

impl AnsweredBy {
    /// The name as the event log spells it.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::Conductor => "conductor",
        }
    }
}

/// How a pending input stopped waiting (R5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputCloseReason {
    /// Someone answered it.
    Answered,
    /// A manual or forced transition left the state it was asked in.
    Transition,
    /// An operator interrupted the execution.
    Interrupt,
    /// An operator recovery started the state's action afresh.
    Recover,
    /// The execution was cancelled.
    Cancel,
    /// An undone decision of the conductor's moved the execution back to
    /// the step it settled (KTD13).
    Undo,
}

impl InputCloseReason {
    /// The name as the event log spells it.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Answered => "answered",
            Self::Transition => "transition",
            Self::Interrupt => "interrupt",
            Self::Recover => "recover",
            Self::Cancel => "cancel",
            Self::Undo => "undo",
        }
    }
}

/// What a [`EventData::ProblemRaised`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProblemKind {
    /// The step's action finished without a usable output label; the
    /// operator supplies it (`InputNeeded`, `ResolveOutput`).
    Output,
    /// The step's action failed; the execution stays `Failed` (KTD4).
    Failure,
}

impl ProblemKind {
    /// The name as the event log spells it.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Output => "output",
            Self::Failure => "failure",
        }
    }
}

/// What asked the conductor for an invocation (KTD11).
///
/// Automatic triggers (F1, F3) are served before anything an operator asked
/// for; see [`ConductorTrigger::is_automatic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConductorTrigger {
    /// F1: an agent step still has no usable output label after the
    /// engine's reprompt.
    OutputRescue,
    /// F3: a step failed after its `retry_policy` ran out.
    StepFailure,
    /// F4: an operator wrote in the conductor's chat.
    Chat,
    /// R30: a `human` gate was entered and can be summarized.
    Annotation,
}

impl ConductorTrigger {
    /// The name as the event log spells it.
    pub fn name(&self) -> &'static str {
        match self {
            Self::OutputRescue => "output_rescue",
            Self::StepFailure => "step_failure",
            Self::Chat => "chat",
            Self::Annotation => "annotation",
        }
    }

    /// Whether the engine raised this trigger by itself (F1, F3) rather
    /// than an operator — which is what puts it first in the queue and
    /// makes a failed invocation a problem (KTD11, KTD12).
    pub fn is_automatic(&self) -> bool {
        matches!(self, Self::OutputRescue | Self::StepFailure)
    }

    /// Whether an invocation for this trigger works beside the execution,
    /// without its claim, and is stopped through the registry of advisory
    /// interrupts (KTD10): everything but the label rescue (F1), which runs
    /// inline under the claim of the pass that waits for it.
    pub fn is_advisory(&self) -> bool {
        !matches!(self, Self::OutputRescue)
    }
}

/// What the conductor is asked to do in one invocation — the "working on
/// what" of R11.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConductorPurpose {
    /// Choose the output of a step whose agent gave none (F1).
    ChooseOutput,
    /// Choose the output after the conductor's own reprompt failed too: a
    /// second reprompt is no longer allowed (R17).
    ChooseOutputAfterReprompt,
    /// Diagnose a failed step and propose what to do (F3).
    DiagnoseFailure,
    /// Answer the operator's chat message (F4).
    ReplyToChat,
    /// Summarize a `human` gate for the operator (R30).
    SummarizeGate,
}

impl ConductorPurpose {
    /// The name as the event log spells it.
    pub fn name(&self) -> &'static str {
        match self {
            Self::ChooseOutput => "choose_output",
            Self::ChooseOutputAfterReprompt => "choose_output_after_reprompt",
            Self::DiagnoseFailure => "diagnose_failure",
            Self::ReplyToChat => "reply_to_chat",
            Self::SummarizeGate => "summarize_gate",
        }
    }
}

/// How a conductor invocation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConductorOutcome {
    /// It answered.
    Completed,
    /// It could not answer: a process error, a timeout, an unusable
    /// response (R24).
    Failed,
    /// Someone stopped it before it answered.
    Interrupted,
}

impl ConductorOutcome {
    /// The name as the event log spells it.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }
}

/// What a [`EventData::ConductorDecision`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConductorDecisionKind {
    /// (a): the step completes with an output the conductor chose.
    ChooseOutput,
    /// (b): the agent is asked again, in the conductor's own words.
    Reprompt,
    /// (c): the step becomes a problem for the operator.
    Escalate,
    /// The failed step is tried once more (R21).
    Retry,
    /// A proposal the operator confirmed was carried out (R28).
    ExecuteProposal,
}

impl ConductorDecisionKind {
    /// The name as the event log spells it.
    pub fn name(&self) -> &'static str {
        match self {
            Self::ChooseOutput => "choose_output",
            Self::Reprompt => "reprompt",
            Self::Escalate => "escalate",
            Self::Retry => "retry",
            Self::ExecuteProposal => "execute_proposal",
        }
    }
}

/// How sure the conductor was of a decision (KTD6). Anything it does not
/// declare is `low`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Sure enough to act without the operator.
    High,
    /// Not sure enough; the default.
    #[default]
    Low,
}

impl Confidence {
    /// The name as the event log spells it.
    pub fn name(&self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Low => "low",
        }
    }
}

impl EventData {
    /// A [`EventData::HumanInputReceived`] carrying field values and nothing
    /// more — how every answer was recorded before the event said who gave
    /// it and how the input was closed.
    pub fn human_input_received(input: serde_json::Value) -> Self {
        Self::HumanInputReceived {
            input,
            answered_by: None,
            close_reason: None,
            episode: None,
            output_label: None,
            data: None,
            variables: None,
            comment: None,
        }
    }
}

impl Event {
    /// Creates a new event
    pub fn new(workflow_execution_id: String, data: EventData, sequence: u64) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            workflow_execution_id,
            timestamp: Utc::now(),
            data,
            sequence,
        }
    }

    /// Returns a human-readable description of the event
    /// A one-line description of the event for an operator.
    ///
    /// This is what the CLI's `events` listing and the TUI's event stream
    /// show, so every variant is spelled out and there is deliberately no
    /// catch-all: a new [`EventData`] variant must fail to compile here rather
    /// than quietly leaking a Rust `Debug` dump into the interface.
    pub fn description(&self) -> String {
        match &self.data {
            EventData::WorkflowStarted {
                workflow_id,
                workflow_version,
                ..
            } => {
                format!("Workflow '{workflow_id}' (version {workflow_version}) started")
            }
            EventData::WorkflowPaused { reason } => format!("Workflow paused: {reason}"),
            EventData::WorkflowResumed => "Workflow resumed".to_string(),
            EventData::WorkflowCompleted { .. } => "Workflow completed".to_string(),
            EventData::WorkflowFailed { error } => format!("Workflow failed: {error}"),
            EventData::WorkflowCancelled { reason } => format!("Workflow cancelled: {reason}"),

            EventData::StateEntered { state_id } => format!("Entered state '{state_id}'"),
            EventData::StateExited {
                state_id,
                output_label,
            } => {
                format!("Left state '{state_id}' with '{output_label}'")
            }

            EventData::ActionStarted {
                action_type,
                state_id,
                ..
            } => {
                format!("Action '{action_type}' started in state '{state_id}'")
            }
            EventData::ActionCompleted { output_label, .. } => {
                format!("Action completed with '{output_label}'")
            }
            EventData::ActionFailed { error, .. } => format!("Action failed: {error}"),
            EventData::ActionAttemptFailed {
                error, retry_in_ms, ..
            } => {
                format!("Attempt failed: {error} (retrying in {retry_in_ms}ms)")
            }
            EventData::ActionRetrying { retry_count, .. } => {
                format!("Retrying the action (attempt {})", retry_count + 1)
            }
            EventData::ActionOutputRejected { reason, .. } => {
                format!("Output rejected: {}", first_line(reason))
            }
            EventData::OutputReprompted { by, .. } => {
                format!("Asked the agent again for its output (by {})", by.name())
            }

            EventData::AgentSessionStarted { harness, .. } => {
                format!("Agent session started on '{harness}'")
            }
            EventData::AgentSessionCompleted { .. } => "Agent session completed".to_string(),
            EventData::AgentSessionFailed { error, .. } => {
                format!("Agent session failed: {error}")
            }
            EventData::AgentMessage { role, content, .. } => {
                format!("Agent message ({role}): {}", first_line(content))
            }

            EventData::DecisionRequested { provider } => {
                format!("Decision requested from '{provider}'")
            }
            EventData::DecisionMade {
                provider,
                output_label,
                ..
            } => {
                format!("Decision '{output_label}' made by '{provider}'")
            }

            EventData::HumanInputRequested { instruction, .. } => {
                format!("Waiting for a human: {}", first_line(instruction))
            }
            EventData::HumanInputReceived {
                input,
                answered_by,
                close_reason,
                output_label,
                ..
            } => {
                let by = answered_by.unwrap_or_default().name();
                match close_reason {
                    Some(reason) if *reason != InputCloseReason::Answered => {
                        format!("Human input closed by {} ({by})", reason.name())
                    }
                    _ => match output_label {
                        Some(label) => format!(
                            "Human input received from {by}: '{label}', {}",
                            summarize_input(input)
                        ),
                        None => {
                            format!("Human input received from {by}: {}", summarize_input(input))
                        }
                    },
                }
            }
            EventData::ProblemRaised {
                state_id,
                kind,
                reason,
                ..
            } => {
                format!(
                    "Problem ({}) in state '{state_id}': {}",
                    kind.name(),
                    first_line(reason)
                )
            }
            EventData::HumanTransitionSelected {
                from_state,
                to_state,
            } => {
                format!("Human chose the transition {from_state} -> {to_state}")
            }

            EventData::ConductorStarted { purpose, .. } => {
                format!("Conductor started: {}", purpose.name())
            }
            EventData::ConductorFinished { outcome, error, .. } => match error {
                Some(error) => format!(
                    "Conductor finished ({}): {}",
                    outcome.name(),
                    first_line(error)
                ),
                None => format!("Conductor finished ({})", outcome.name()),
            },
            EventData::ConductorDecision {
                kind,
                output_label,
                confidence,
                ..
            } => match output_label {
                Some(label) => format!(
                    "Conductor decided to {} '{label}' ({} confidence)",
                    kind.name(),
                    confidence.name()
                ),
                None => format!(
                    "Conductor decided to {} ({} confidence)",
                    kind.name(),
                    confidence.name()
                ),
            },

            EventData::ConductorDecisionUndone {
                decision_id,
                state_id,
                ..
            } => format!("Conductor decision {decision_id} undone: back to '{state_id}'"),

            EventData::ExternalEventReceived {
                event_type, source, ..
            } => {
                format!("External event '{event_type}' from '{source}'")
            }

            EventData::TransitionTaken {
                from_state,
                to_state,
                output_label,
            } => {
                format!("Transition {from_state} -> {to_state} on '{output_label}'")
            }

            EventData::ArtifactCreated {
                artifact_type,
                location,
                ..
            } => {
                format!("Artifact '{artifact_type}' created at {location}")
            }

            EventData::Error { message, .. } => format!("Error: {message}"),
        }
    }
}

/// The first line of a multi-line string, trimmed and elided, so a long agent
/// prompt cannot break a one-line-per-event listing.
fn first_line(text: &str) -> String {
    const LIMIT: usize = 80;
    let line = text.lines().next().unwrap_or("").trim();

    if line.chars().count() <= LIMIT {
        return line.to_string();
    }
    let cut: String = line.chars().take(LIMIT).collect();
    format!("{cut}…")
}

/// Names the fields a human filled in, without echoing their values — the
/// values are in the context, and a free-text answer can be arbitrarily long.
fn summarize_input(input: &serde_json::Value) -> String {
    match input.as_object() {
        Some(fields) if !fields.is_empty() => {
            let names: Vec<&str> = fields.keys().map(String::as_str).collect();
            names.join(", ")
        }
        Some(_) => "no fields".to_string(),
        None => first_line(&input.to_string()),
    }
}
