//! Wire payloads for the mutating endpoints (U3).
//!
//! Kept apart from `routes.rs` for the same reason `routes.rs` itself keeps
//! `ConversationEntryDto`/`ConversationResponse` next to the handler that
//! builds them would have been reasonable too — but the plan asks for a
//! dedicated module, and having every request/response shape in one place
//! makes the wire contract easy to review without wading through handler
//! bodies.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::domain::{ExecutionStatus, WorkflowExecution};
use crate::engine::PendingHumanInput;

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

/// `POST /executions/:id/resume` and `POST /executions/:id/input` response
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
    /// The question this execution is waiting on, if it is `Paused` on one
    /// raised by `human::suspend` (an operator-requested `WorkflowPaused`
    /// has none, exactly as `Engine::pending_input` already returns `None`
    /// for it).
    #[serde(default)]
    pub pending_input: Option<PendingHumanInput>,
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
        }
    }
}

/// `GET /executions` response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionListResponse {
    /// Every execution in the store, in [`crate::persistence::ExecutionStore::list_execution_ids`]'s order.
    pub executions: Vec<ExecutionSummary>,
}
