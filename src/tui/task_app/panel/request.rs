//! What the panel sends, and what a key or a read of it asks of the
//! application.

use narsor_protocol::api::HumanInputDeliveryDto;
use narsor_protocol::controller::{OperatorCommand, OwnerCommand, TaskActionId};

use crate::tui::chooser::Submission;

// ---------------------------------------------------------------------
// What is sent
// ---------------------------------------------------------------------

/// What a chosen action asks the API to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Submit {
    /// An answer to the pending input request `pending_input_id`.
    Input {
        /// The request it answers.
        pending_input_id: String,
        /// The answer, boxed: it is far larger than the other variants.
        delivery: Box<HumanInputDeliveryDto>,
    },
    /// A command for the runner: a restart, a confirmation of a proposal.
    Operator(OperatorCommand),
    /// A runner state command of the owner: `start`, `archive`.
    Owner(OwnerCommand),
    /// Action 0, the text to the agent: the server picks what it does.
    Comment(String),
}

/// A chosen action, ready to be sent: [`TaskApp::perform`] carries it out.
#[derive(Debug, Clone, PartialEq)]
pub struct AnswerRequest {
    /// The task.
    pub task_id: String,
    /// The waiting it answers.
    pub waiting_id: String,
    /// The task's title for the notification.
    pub title: String,
    /// The generation the screen shows; a command is refused if the task has
    /// moved on.
    pub generation: u64,
    /// The key a retry of this decision reuses.
    pub key: String,
    /// What the notification says when it worked.
    pub summary: String,
    /// What to send.
    pub submit: Submit,
}

/// What a key in the panel asks of the application.
#[derive(Debug, Clone, PartialEq)]
pub enum PanelEffect {
    /// The key was folded into the panel.
    None,
    /// Collapse the panel.
    Collapse,
    /// Say something on the bar.
    Notice(String),
    /// Send the chosen action.
    Submit(Submission),
    /// The chosen action cannot be undone: ask in the `y` / `n` window first.
    Confirm(Submission),
    /// Action 0 with its text: send it to the agent.
    Comment(String),
    /// The chosen action of the workflow form: the application does it.
    Action(TaskActionId),
}

/// What a read of the panel's entries changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncEffect {
    /// The entry the operator was in went away.
    pub lost_current: bool,
    /// A draft was left behind.
    pub released: bool,
}
