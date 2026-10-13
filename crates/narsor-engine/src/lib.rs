//! Narsor's execution engine and server-side domain implementation.
//!
//! Client frontends live in the root `narsor` package. This crate owns
//! execution, persistence, orchestration, and the adapters that project
//! those internal models onto protocol types.

pub mod api;
pub mod auth;
pub mod backends;
pub mod conductor;
pub mod config;
pub mod controller_protocol;
pub mod controller_runtime;
pub mod domain;
pub mod engine;
pub mod environment;
pub mod materials;
pub mod orchestration;
pub mod persistence;
pub mod registry;
/// Shared runner-home settings retained for server and integration contracts.
pub mod runner;
pub mod tasks;
pub mod workflow;
pub mod workflows;
pub mod workspace;
