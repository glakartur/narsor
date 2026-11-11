//! What the owner sends to a task's runner: human input and operator commands.

mod commands;
mod input;

#[cfg(test)]
pub(crate) use commands::check_step_exists;
pub(crate) use input::protocol_delivery;
