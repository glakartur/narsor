//! Narsor's execution engine and server-side domain implementation.
//!
//! Client frontends live in the root `narsor` package. This crate owns
//! execution, persistence, orchestration, and the adapters that project
//! those internal models onto protocol types.

pub use narsor_exec::api;
pub mod auth;
pub mod automations;
pub use narsor_exec::conductor;
pub mod config;
pub use narsor_controller_client as controller_client;
pub mod controller_protocol;
pub use narsor_controller_client::controller_runtime;
pub use narsor_exec::engine;
pub use narsor_model::domain;
pub mod environment;
pub mod launch;
pub mod materials;
pub mod observers;
pub mod orchestration;
pub use narsor_exec::registry;
pub use narsor_store as persistence;
/// Shared runner-home settings retained for server and integration contracts.
pub mod runner;
pub use narsor_exec::tasks;
pub use narsor_exec::workflows;
pub use narsor_exec::workspace;
pub use narsor_model::workflow;
