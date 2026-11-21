//! Implementation of every subcommand.
//!
//! # Where a `run` gets its workspace
//!
//! `run` owns command-line parsing and presentation. It gets the workflow
//! schema and queues the task through the control plane API; the server owns
//! workflow resolution, task creation, and execution.
//!
//! # Where a definition comes from
//!
//! A catalogue reference is resolved by the server's workflow endpoint. A
//! file reference is validated by the local server and sent as an absolute
//! path, so the client does not parse a workflow definition itself.
//!
//! # How `input` finds the definition
//!
//! The server seals workflow definitions with accepted tasks. Follow-up
//! commands such as `input` use API responses and never resolve
//! the definition or open execution storage themselves.

mod config;
mod daemon_access;
mod image;
mod open_matters;
mod secret;
mod task_control;
mod task_read;
mod tui;
mod workflow;

pub use config::*;
pub use image::*;
pub use secret::*;
pub use task_control::*;
pub use task_read::*;
pub use tui::*;
pub use workflow::*;

/// Reached from the `cli` tests only; the commands use it through
/// [`open_matters`] directly.
#[cfg(test)]
pub(super) use open_matters::pending_input_lines;

pub use narsor_shared::cli_error::{CliError, Result};
