//! Narsor — an AI workflow orchestrator
//!
//! Library crate exposing the domain model and the engine subsystems.
//! The `narsor` binary is a thin wrapper over this crate; integration
//! tests under `tests/` link against it.

pub mod api;
pub mod auth;
pub mod backends;
pub mod cli;
pub mod conductor;
pub mod config;
pub mod controller_protocol;
pub mod daemon;
pub mod domain;
pub mod engine;
pub mod environment;
pub mod materials;
pub mod orchestration;
pub mod persistence;
pub mod registry;
pub mod runner;
pub mod tasks;
pub mod tui;
pub mod workflow;
pub mod workflows;
pub mod workspace;
