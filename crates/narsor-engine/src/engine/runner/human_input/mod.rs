//! What a person, or the conductor in a person's place, gives a suspended
//! execution, what ends a `wait` step's wait, and what is done to an
//! execution outside a pass.

use super::*;

mod agent_input;
mod delivery;
mod event_delivery;
pub(super) mod manual_transition;
pub(super) mod operations;
mod permission;
mod suspension;

pub(super) use agent_input::*;
pub(super) use delivery::*;
pub(super) use manual_transition::*;
