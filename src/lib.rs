//! AI Workflow Orchestrator
//!
//! Library crate exposing the domain model and the engine subsystems.
//! The `orchestrator` binary is a thin wrapper over this crate; integration
//! tests under `tests/` link against it.

pub mod cli;
pub mod domain;
pub mod engine;
pub mod persistence;
pub mod registry;
pub mod tui;
pub mod workflow;
pub mod workspace;
