//! Narsor's execution layer: the engine that runs a workflow, the
//! conductor beside it, task workspaces and the repository registry, task
//! definitions and their sources, and the API projections of an execution.
//!
//! Linked by the runner (which runs one task) and by the server. It knows
//! nothing of the control plane: orchestration, controllers, observers and
//! authentication stay in `narsor-engine`, which re-exports these modules
//! under its old paths.

pub use narsor_model::{config, domain, workflow};
pub use narsor_store as persistence;

pub mod api;
pub mod conductor;
pub mod engine;
pub mod materials;
pub mod registry;
pub mod tasks;
pub mod workflows;
pub mod workspace;
