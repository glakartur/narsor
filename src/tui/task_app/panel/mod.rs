//! The panel of the conversation: the forms the server offers a task, one
//! shown at a time (R10), and `Esc` to step through them (R11).
//!
//! The server decides which forms there are, which opens first and what each
//! asks (`TaskSummary::panel`, `narsor_protocol::panel`); this module only
//! shows them. What is common to every form lives here: the switching
//! ([`switcher`]) and the way a form is announced in the header ([`header`]).
//! [`chat`] is the free message to the agent; the forms that answer a wait
//! (an answer, a permission, an entry limit, the output of a step) are made
//! of the entries of [`state`], which keep their drafts, and are shown by
//! [`input`].

pub mod chat;
pub mod header;
pub mod input;
mod legacy;
pub mod request;
mod state;
pub mod switcher;

mod app;

pub use request::{AnswerRequest, PanelEffect, Submit, SyncEffect};
pub use state::{Released, WaitingPanel};
