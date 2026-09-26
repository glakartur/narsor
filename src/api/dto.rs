//! Wire payloads for the mutating endpoints (U3).
//!
//! Kept apart from `routes.rs` for the same reason `routes.rs` itself keeps
//! `ConversationEntryDto`/`ConversationResponse` next to the handler that
//! builds them would have been reasonable too — but the plan asks for a
//! dedicated module, and having every request/response shape in one place
//! makes the wire contract easy to review without wading through handler
//! bodies.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::domain::{ExecutionStatus, WorkflowExecution};

/// `POST /tasks` request body.
///
/// `workflow` is a reference exactly as `narsor run <reference>` accepts one
/// — an existing file, `{catalog}/workflows/<name>.yaml`, the user directory,
/// or a built-in name ([`crate::workflows::WorkflowResolver`]) — not a path
/// the caller is expected to already have resolved.
#[derive(Debug, Clone, Deserialize)]
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
#[derive(Debug, Clone, Deserialize)]
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
/// (its `context`/`workspace`/`metadata` are internal engine bookkeeping, not
/// wire contract).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionSummary {
    /// The execution's id.
    pub id: String,
    /// The workflow it is an execution of.
    pub workflow_id: String,
    /// Its current status.
    pub status: ExecutionStatus,
    /// The state it is currently at, if any.
    pub current_state: Option<String>,
    /// When it started.
    pub started_at: DateTime<Utc>,
    /// When it finished, if it has.
    pub completed_at: Option<DateTime<Utc>>,
}

impl From<&WorkflowExecution> for ExecutionSummary {
    fn from(execution: &WorkflowExecution) -> Self {
        Self {
            id: execution.id.clone(),
            workflow_id: execution.workflow_id.clone(),
            status: execution.status.clone(),
            current_state: execution.current_state.clone(),
            started_at: execution.started_at,
            completed_at: execution.completed_at,
        }
    }
}

/// `GET /executions` response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionListResponse {
    /// Every execution in the store, in [`crate::persistence::ExecutionStore::list_execution_ids`]'s order.
    pub executions: Vec<ExecutionSummary>,
}
