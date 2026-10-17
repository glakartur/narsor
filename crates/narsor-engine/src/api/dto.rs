//! Wire payloads for the mutating endpoints (U3).
//!
//! Kept apart from `routes.rs` for the same reason `routes.rs` itself keeps
//! `ConversationEntryDto`/`ConversationResponse` next to the handler that
//! builds them would have been reasonable too — but the plan asks for a
//! dedicated module, and having every request/response shape in one place
//! makes the wire contract easy to review without wading through handler
//! bodies.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

pub use narsor_protocol::api::{
    AcceptedResponse, ActiveProblem, AnnotationDto, ChatAuthorDto, ChatEntryDto, ChatEntryKindDto,
    ChatMessageRequest, ConductorActivity, ConductorPurposeDto, ConductorTriggerDto,
    ConversationEntryDto, ConversationEntrySource, ConversationResponse, ExecutionListResponse,
    ExecutionStatusDto, ExecutionSummary, HarnessSelectionSnapshot, HumanInputDeliveryDto,
    OperatorNoteRequest, PendingHumanInputView, PermissionDecisionDto, PermissionDecisionKindDto,
    PermissionDecisionRequestDto, PermissionGrantScopeDto, ProblemKindDto, RecoveryRequest,
    RequestErrorResponse, SetHarnessSelectionRequest, StartTaskRequest, StartTaskResponse,
    TransitionRequest,
};

use crate::conductor::{
    Annotation, ChatAuthor, ChatEntry, ChatEntryKind, ConductorState, FailureReport,
    PendingInvocation, QueuedRequest, SavedDecision,
};
use crate::domain::WorkflowExecution;

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
/// `metadata`), or build the human-input form (`PendingHumanInputView` carries
/// the declared fields/types/options, not just the instruction text the
/// event log already exposes). Reusing this one summary type for all three
/// keeps `GET /executions` the only read the TUI's list-and-select path
/// needs, rather than adding three more round trips per execution.
impl From<ChatEntry> for ChatEntryDto {
    fn from(entry: ChatEntry) -> Self {
        Self {
            id: entry.id,
            sequence: entry.sequence,
            at: entry.at,
            author: match entry.author {
                ChatAuthor::Operator => ChatAuthorDto::Operator,
                ChatAuthor::Conductor => ChatAuthorDto::Conductor,
            },
            kind: match entry.kind {
                ChatEntryKind::Message => ChatEntryKindDto::Message,
                ChatEntryKind::Error => ChatEntryKindDto::Error,
                ChatEntryKind::Decision => ChatEntryKindDto::Decision,
                ChatEntryKind::Proposal => ChatEntryKindDto::Proposal,
            },
            text: entry.text,
            in_reply_to: entry.in_reply_to,
            payload: entry.payload,
        }
    }
}

impl From<ChatEntryDto> for ChatEntry {
    fn from(entry: ChatEntryDto) -> Self {
        Self {
            id: entry.id,
            sequence: entry.sequence,
            at: entry.at,
            author: match entry.author {
                ChatAuthorDto::Operator => ChatAuthor::Operator,
                ChatAuthorDto::Conductor => ChatAuthor::Conductor,
            },
            kind: match entry.kind {
                ChatEntryKindDto::Message => ChatEntryKind::Message,
                ChatEntryKindDto::Error => ChatEntryKind::Error,
                ChatEntryKindDto::Decision => ChatEntryKind::Decision,
                ChatEntryKindDto::Proposal => ChatEntryKind::Proposal,
            },
            text: entry.text,
            in_reply_to: entry.in_reply_to,
            payload: entry.payload,
        }
    }
}

impl From<Annotation> for AnnotationDto {
    fn from(annotation: Annotation) -> Self {
        Self {
            key: annotation.key,
            summary: annotation.summary,
            recommendation: annotation.recommendation,
            created_at: annotation.created_at,
        }
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
            status: execution.status,
            current_state: execution.current_state.clone(),
            started_at: execution.started_at,
            completed_at: execution.completed_at,
            variables: execution.context.variables.clone(),
            metadata: execution.metadata.clone(),
            pending_input: None,
            problem: None,
            annotation: None,
            conductor: None,
        }
    }
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
    pub entries: Vec<ChatEntryDto>,
}
