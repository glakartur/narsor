//! HTTP/SSE client transport and sibling-server discovery.

pub mod config {
    pub use narsor_shared::config::*;
}

pub mod controller_protocol {
    pub use narsor_protocol::api::{
        ConductorChatResponse, ConductorStateResponse, LegacyExecutionListResponse,
        LegacyExecutionView, PendingInvocationView, QueuedRequestView,
    };
    pub use narsor_protocol::controller::*;
}

pub mod api {
    mod client;
    pub mod connect;
    pub mod connection_profile;
    mod discovery;
    pub mod dto;
    pub mod launch;
    pub mod token;

    pub use client::{reconnect_delay, ApiClient};
    pub use connect::{connect, ServerTarget};
    pub use discovery::{ensure_daemon_running, DiscoveryError};
    pub use narsor_protocol::api::*;
    pub use narsor_shared::api_error::ClientError;
    pub use token::TokenSource;
}
