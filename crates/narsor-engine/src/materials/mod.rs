//! Task materials on the control plane: the store of uploaded bundles and
//! their delivery records.
//!
//! The material model, manifest rules, bundles and the runner-side writing
//! live in `narsor_exec::materials`, which this module re-exports, so the
//! runner can use them without linking the control plane.

pub use crate::controller_client::wire::{MaterialAckRequest, MaterialAckResponse};
pub use narsor_exec::materials::*;

pub mod store;

pub use store::{MaterialStore, Staging};
