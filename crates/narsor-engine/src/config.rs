//! Shared process configuration plus engine-owned state paths.

pub use narsor_shared::config::*;

#[path = "persistence/paths.rs"]
pub mod paths;
