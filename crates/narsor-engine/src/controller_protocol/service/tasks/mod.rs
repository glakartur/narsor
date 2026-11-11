//! The owner's tasks: creating, finding, commanding and summarizing them,
//! their actions, panels and materials.

mod actions;
mod commands;
mod create;
mod history;
mod lookup;
mod materials;
mod panel;
mod summary;
mod view;

pub(crate) use create::launch_refusal;
pub use view::task_workspaces;
#[cfg(test)]
pub(crate) use view::{transition_offer, workflow_steps};

use crate::controller_protocol::wire::*;

/// The panel forms the server puts on a task summary, for whoever builds a
/// summary by hand (a test of a client) and wants the server's offer.
#[doc(hidden)]
pub fn task_panel(task: &TaskSummary) -> Option<narsor_protocol::controller::PanelOffer> {
    panel::offer_forms(task)
}

/// The offer of actions the server puts on a task summary, for whoever builds
/// a summary by hand (a test of a client) and wants the server's words.
#[doc(hidden)]
pub fn task_actions(task: &TaskSummary) -> Vec<TaskActionOffer> {
    actions::offer(task)
}
