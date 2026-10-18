//! Internal API projections and shared wire response values.
//!
//! HTTP/SSE transport and local server discovery belong to the client package.

pub mod dto;
pub use dto::{
    AcceptedResponse, ActiveProblem, AnnotationDto, ChatAuthorDto, ChatEntryDto, ChatEntryKindDto,
    ChatMessageRequest, ConductorActivity, ConductorChatResponse, ConductorPurposeDto,
    ConductorStateResponse, ConductorTriggerDto, ConversationEntryDto, ConversationEntrySource,
    ConversationResponse, ExecutionListResponse, ExecutionStatusDto, ExecutionSummary,
    HarnessSelectionSnapshot, HumanInputDeliveryDto, OperatorNoteRequest, PermissionDecisionDto,
    PermissionDecisionKindDto, PermissionDecisionRequestDto, PermissionGrantScopeDto,
    ProblemKindDto, RequestErrorResponse, SetHarnessSelectionRequest, StartTaskRequest,
    StartTaskResponse, TransitionRequest,
};
pub use narsor_shared::api_error::{handle_json_response, ClientError};
mod projection;
pub use projection::conversation_response_from_snapshot;
