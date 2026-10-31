//! Client library surface for the TUI and CLI.
//!
//! This crate root is being narrowed from the original implementation
//! library. Client-facing wire values are re-exported from
//! `narsor-protocol`; server and engine modules live in `narsor-engine`.

pub use narsor_api_client::api;

#[path = "cli/mod.rs"]
pub mod cli;
#[path = "client/config.rs"]
pub mod config;
#[path = "tui/mod.rs"]
pub mod tui;

pub mod controller_protocol {
    pub use narsor_protocol::api::{
        ConductorChatResponse, ConductorStateResponse, LegacyExecutionListResponse,
        LegacyExecutionView, PendingInvocationView, QueuedRequestView,
    };
    pub use narsor_protocol::controller::*;
}

pub mod conductor {
    pub use narsor_protocol::api::{
        AnnotationDto as Annotation, ChatAuthorDto as ChatAuthor, ChatEntryDto as ChatEntry,
        ChatEntryKindDto as ChatEntryKind,
    };
    pub use narsor_protocol::controller::{
        ConductorView, ConductorWorkView, DecisionView, FailureOutcome, FailureReport,
        ProposalKind, ProposalState, ProposalView, ProposedAction,
    };
    pub type ConductorWork = ConductorWorkView;
}

pub mod domain {
    pub use narsor_protocol::api::ExecutionStatusDto as ExecutionStatus;
    pub use narsor_protocol::display;
    pub use narsor_protocol::display::*;
    pub use narsor_protocol::event;
    pub use narsor_protocol::event::{
        ConductorDecisionKind, ConductorPurpose, Confidence, Event, EventData, InputCloseReason,
    };
    pub use narsor_protocol::identifier::{BackendId, TaskId};
    pub type ExecutionId = String;
}

#[path = "client/materials.rs"]
pub mod materials;
#[path = "client/usage_text.rs"]
pub mod usage_text;
