//! The form engine: editing, wrapping, moving between fields, the
//! confirmation before a send, and the events a form reports to its owner.
//!
//! A new form is a list of fields and an area to draw them in; the rules of
//! editing and of moving between fields are not repeated by its owner.
//!
//! - [`buffer`]: the text of a field and its cursor (`Delete`, `Home`, `End`).
//! - [`layout`]: wrapped rows, the cursor's cell and the scrolled window.
//! - [`engine`]: the state of a form, its keys, its events.

pub mod buffer;
pub mod confirm;
pub mod engine;
pub mod events;
pub mod field;
pub mod layout;
