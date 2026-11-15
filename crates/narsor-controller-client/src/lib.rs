//! The client side of the controller protocol: the HTTP clients a
//! controller and a runner use, the wire shapes they exchange with the
//! server, and the proof-of-possession signing their requests carry.
//!
//! Nothing here depends on the server side (`narsor_engine::controller_protocol`'s
//! service, credential store and error mapping); the server re-exports this
//! module's items so both sides share one wire contract.

pub mod client;
pub mod controller_runtime;
pub mod pop;
pub mod wire;

pub use client::{ClientOptions, ControllerClient, RunnerClient};
pub use wire::*;
