//! Wire payloads for the mutating endpoints (U3).
//!
//! Kept apart from `routes.rs` for the same reason `routes.rs` itself keeps
//! `ConversationEntryDto`/`ConversationResponse` next to the handler that
//! builds them would have been reasonable too — but the plan asks for a
//! dedicated module, and having every request/response shape in one place
//! makes the wire contract easy to review without wading through handler
//! bodies.

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

use crate::conductor::{Annotation, ChatAuthor, ChatEntry, ChatEntryKind};

/// `POST /executions/:id/transition` response body, and one row of
/// `GET /executions`: enough of an execution's state for a caller to decide
/// what to do next, without handing over the whole [`WorkflowExecution`](crate::domain::WorkflowExecution)
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
