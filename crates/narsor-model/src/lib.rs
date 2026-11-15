//! Narsor's model layer: the domain types, workflow definitions (schema,
//! loading, templates, validation) and shared configuration.
//!
//! Pure data and rules with no I/O beyond reading workflow files. Every
//! other server-side crate builds on it; `narsor-engine` re-exports its
//! modules under the old paths.

pub mod config;
pub mod domain;
pub mod workflow;
