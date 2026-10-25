//! The panel of the conversation: the forms the server offers a task, one
//! shown at a time (R10), and `Esc` to step through them (R11).
//!
//! The server decides which forms there are and which opens first
//! (`TaskSummary::panel`); this module only shows them. What is common to
//! every form lives here: the switching ([`switcher`]) and the way a form
//! is announced in the header ([`header`]). Each kind of form has a
//! directory of its own: [`chat`] is the free message to the agent; the
//! forms that answer a wait (an answer, a permission, an entry limit, the
//! output of a step) are made of the entries of [`super::waiting`], which
//! keep their drafts.

pub mod action;
pub mod answer;
pub mod chat;
pub mod entry_limit;
pub mod header;
pub mod input;
pub mod permission;
pub mod switcher;
