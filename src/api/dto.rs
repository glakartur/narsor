//! Wire payloads for the mutating endpoints (U3).
//!
//! Kept apart from `routes.rs` for the same reason `routes.rs` itself keeps
//! `ConversationEntryDto`/`ConversationResponse` next to the handler that
//! builds them would have been reasonable too — but the plan asks for a
//! dedicated module, and having every request/response shape in one place
//! makes the wire contract easy to review without wading through handler
//! bodies.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::conductor::{
    ChatEntry, ConductorState, FailureReport, PendingInvocation, QueuedRequest, SavedDecision,
};
use crate::domain::{ExecutionStatus, ProblemKind, WorkflowExecution};
use crate::engine::{InputContinuation, PendingHumanInput};

/// Authoritative default-harness selection returned by `GET`, `PUT`, and
/// `DELETE /settings/agent-harness`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessSelectionSnapshot {
    /// Harness adapters registered by this daemon, in registry order.
    pub supported: Vec<String>,
    /// The startup configuration value used when there is no override.
    pub configured: String,
    /// The durable operator override, if one is active.
    #[serde(rename = "override")]
    pub override_harness: Option<String>,
    /// The harness new agent attempts will use by default.
    pub effective: String,
    /// Monotonically increasing durable mutation revision.
    pub revision: u64,
}

/// `PUT /settings/agent-harness` request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetHarnessSelectionRequest {
    /// A name from [`HarnessSelectionSnapshot::supported`].
    pub harness: String,
}

/// A safe, user-displayable invalid-request response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestErrorResponse {
    /// Stable machine-readable reason, such as `unsupported_harness`.
    pub code: String,
    /// Human-readable explanation that contains no credentials.
    pub message: String,
}

/// `POST /tasks` request body.
///
/// `workflow` is a reference exactly as `narsor run <reference>` accepts one
/// — an existing file, `{catalog}/workflows/<name>.yaml`, the user directory,
/// or a built-in name ([`crate::workflows::WorkflowResolver`]) — not a path
/// the caller is expected to already have resolved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartTaskRequest {
    /// The workflow to start.
    pub workflow: String,
    /// Values for the workflow's declared inputs.
    #[serde(default)]
    pub inputs: Map<String, Value>,
}

/// `POST /tasks` response body: `202 Accepted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartTaskResponse {
    /// The id of the execution that was created.
    pub execution_id: String,
}

/// `POST /executions/:id/resume`, `/input`, and `/recover` response
/// body: `202 Accepted`.
///
/// Carries no execution state — by the time the response is sent, phase 2
/// (`RunHandle::run_to_completion`) has only just been handed to
/// `tokio::spawn` and has very likely not run at all yet. A caller that wants
/// to know when the execution settles polls `GET /executions` or (from U4 on)
/// subscribes to the SSE stream; inventing a body here would only invite a
/// caller to read state that is already stale the instant it is written.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptedResponse {
    /// The execution the request was accepted for.
    pub execution_id: String,
}

/// An explicit operator recovery of a failed or interrupted execution.
///
/// When present, `comment` is shown to the agent as an operator note in the
/// newly started attempt; it is never used for an automatic retry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryRequest {
    #[serde(default)]
    pub comment: Option<String>,
    /// The key of the step failure the recovery answers (KTD9), as the
    /// execution's [`ActiveProblem::episode`] names it: once the execution is
    /// no longer failed on it, the recovery is refused with `409`. Without
    /// it, whatever the execution is stopped on is recovered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode: Option<String>,
}

/// A one-shot instruction appended to the next agent attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorNoteRequest {
    pub note: String,
}

/// `POST /executions/:id/transition` request body.
///
/// Mirrors the TUI's own split between an ordinary, declared transition and a
/// forced one (`src/tui/app.rs`'s `manual_transition`/`force_transition`):
///
/// * `force: false` (the default) — `label` must name one of the current
///   state's declared `transitions`; the target is looked up from it. This is
///   the *only* shape the server allows unless the current state opts in to
///   overrides.
/// * `force: true` — `target` names the state to jump to directly, bypassing
///   the declared transition graph; the current state must declare
///   `allow_manual_override: true` (D30) or the request is rejected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionRequest {
    /// The declared output label to take. Required (and validated against
    /// the current state's transitions) unless `force` is set.
    #[serde(default)]
    pub label: Option<String>,
    /// The state to force a transition into. Required when `force` is set,
    /// ignored otherwise.
    #[serde(default)]
    pub target: Option<String>,
    /// Whether this is a forced override (D30) rather than an ordinary,
    /// declared transition.
    #[serde(default)]
    pub force: bool,
}

/// `POST /executions/:id/transition` response body, and one row of
/// `GET /executions`: enough of an execution's state for a caller to decide
/// what to do next, without handing over the whole [`WorkflowExecution`]
/// (its `workspace` is internal engine bookkeeping, not wire contract).
///
/// # `variables`/`metadata`/`pending_input` (U7)
///
/// These three were not part of the original U3/U5 wire shape — they were
/// added while migrating the TUI off `Arc<Engine>` (U7), once it became clear
/// the existing endpoints (`events`/`timeline`/`conversation`/the mutating
/// calls) leave no way for a client that holds neither an `Engine` nor an
/// `ExecutionStore` to render the "state and context" panel, resolve the
/// workflow definition an execution runs (`tasks::resolve_definition` needs
/// `metadata`), or build the human-input form (`PendingHumanInput` carries
/// the declared fields/types/options, not just the instruction text the
/// event log already exposes). Reusing this one summary type for all three
/// keeps `GET /executions` the only read the TUI's list-and-select path
/// needs, rather than adding three more round trips per execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionSummary {
    /// The execution's id.
    pub id: String,
    /// The workflow it is an execution of.
    pub workflow_id: String,
    /// The version of that workflow's definition this execution was started
    /// with.
    #[serde(default)]
    pub workflow_version: u32,
    /// Its current status.
    pub status: ExecutionStatus,
    /// The state it is currently at, if any.
    pub current_state: Option<String>,
    /// When it started.
    pub started_at: DateTime<Utc>,
    /// When it finished, if it has.
    pub completed_at: Option<DateTime<Utc>>,
    /// The context's user-defined variables — the freshest copy the store
    /// holds (`ExecutionStore::load_context`), not necessarily the one
    /// embedded in the execution record itself.
    #[serde(default)]
    pub variables: HashMap<String, Value>,
    /// The raw `metadata` an execution recorded (`metadata.cli.*`), exactly
    /// as `tasks::resolve_definition` reads it — kept opaque here so the one
    /// place that interprets it stays that function, not this DTO.
    #[serde(default)]
    pub metadata: Value,
    /// The question this execution is waiting on, if it is `InputNeeded` on one
    /// raised by `human::suspend` (an operator-requested `WorkflowPaused`
    /// has none, exactly as `Engine::pending_input` already returns `None`
    /// for it).
    #[serde(default)]
    pub pending_input: Option<PendingHumanInput>,
    /// The problem this execution is stopped on, if it is stopped on one
    /// (conductor plan U7) — what a client needs to count and colour it
    /// without reading the event log.
    ///
    /// For a problem input ([`ProblemKind::Output`]) it is derived from
    /// `pending_input`. A [`ProblemKind::Failure`] leaves the execution
    /// `Failed` with no pending input (KTD4): it is the conductor's report
    /// of the failure the execution is still failed on — its diagnosis, or
    /// the raw error when it could not make one — with the failure's key as
    /// `episode`, the key a recovery answering it names. A failure the
    /// conductor is still diagnosing, or retried by itself, is no problem.
    #[serde(default)]
    pub problem: Option<ActiveProblem>,
}

/// A problem an execution is stopped on (KTD3, KTD4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveProblem {
    /// Whether the step lacks its output or failed.
    pub kind: ProblemKind,
    /// Why the step could not go on by itself.
    pub reason: String,
    /// The key of the pending input that asks the operator, if one does.
    #[serde(default)]
    pub episode: Option<String>,
}

impl ActiveProblem {
    /// The problem the conductor's report of a failed step stands for (F3).
    pub fn of_failure(report: &FailureReport) -> Self {
        Self {
            kind: ProblemKind::Failure,
            reason: report.reason.clone(),
            episode: Some(report.key.clone()),
        }
    }

    /// The problem a pending input stands for, if it is a problem input.
    pub fn of_pending(pending: &PendingHumanInput) -> Option<Self> {
        if pending.continuation != InputContinuation::ResolveOutput {
            return None;
        }
        Some(Self {
            kind: ProblemKind::Output,
            reason: pending
                .problem
                .as_ref()
                .map(|problem| problem.reason.clone())
                .unwrap_or_else(|| pending.instruction().to_string()),
            episode: Some(pending.episode_key().to_string()),
        })
    }
}

impl From<&WorkflowExecution> for ExecutionSummary {
    /// A summary built from the execution record alone, with `variables`
    /// taken from `execution.context` (its own, possibly-stale copy — see
    /// [`ExecutionSummary`]'s doc) and `pending_input` left `None`.
    /// `src/api/routes.rs`'s `build_summary` is what fills in the fresher
    /// context and the real pending-input record; this impl exists for
    /// [`super::TransitionRequest`]'s response, where there never is a
    /// pending input by construction (a transition just took the execution
    /// past whatever question it may have had).
    fn from(execution: &WorkflowExecution) -> Self {
        Self {
            id: execution.id.clone(),
            workflow_id: execution.workflow_id.clone(),
            workflow_version: execution.workflow_version,
            status: execution.status.clone(),
            current_state: execution.current_state.clone(),
            started_at: execution.started_at,
            completed_at: execution.completed_at,
            variables: execution.context.variables.clone(),
            metadata: execution.metadata.clone(),
            pending_input: None,
            problem: None,
        }
    }
}

/// `GET /executions` response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionListResponse {
    /// Every execution in the store, in [`crate::persistence::ExecutionStore::list_execution_ids`]'s order.
    pub executions: Vec<ExecutionSummary>,
}

/// `GET /executions/:id/conductor` — what the conductor of one execution is
/// doing and has kept (R11, R26).
///
/// An execution the conductor never worked on answers with the empty
/// state, not `404`: it simply has nothing to show yet.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConductorStateResponse {
    /// The invocation under way, if one is — the "conductor is working on
    /// ..." of R11.
    #[serde(default)]
    pub pending: Option<PendingInvocation>,
    /// Requests other than chat waiting their turn.
    #[serde(default)]
    pub queue: Vec<QueuedRequest>,
    /// The operator messages still waiting for an answer, oldest first.
    #[serde(default)]
    pub unanswered_chat: Vec<String>,
    /// Invocations counted per episode for automatic triggers (R24).
    #[serde(default)]
    pub invocation_attempts: BTreeMap<String, u32>,
    /// Episodes whose one retry was used (R21).
    #[serde(default)]
    pub used_retries: BTreeSet<String>,
    /// Decisions saved but not yet applied (KTD13).
    #[serde(default)]
    pub unapplied_decisions: Vec<SavedDecision>,
    /// What the conductor concluded about the latest failed step it took up
    /// (F3) — a problem only while the execution is still failed on it; see
    /// [`ExecutionSummary::problem`].
    #[serde(default)]
    pub failure: Option<FailureReport>,
}

impl ConductorStateResponse {
    /// The response for `state` and the ids of the unanswered messages.
    pub fn new(state: ConductorState, unanswered_chat: Vec<String>) -> Self {
        Self {
            pending: state.pending,
            queue: state.queue,
            unanswered_chat,
            invocation_attempts: state.invocation_attempts,
            used_retries: state.used_retries,
            unapplied_decisions: state.unapplied_decisions,
            failure: state.failure,
        }
    }
}

/// `GET /executions/:id/conductor/chat?after=<sequence>` — the chat history
/// after a sequence number, in order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConductorChatResponse {
    /// The entries with `sequence > after`.
    pub entries: Vec<ChatEntry>,
}
