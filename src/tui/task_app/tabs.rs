//! The content of the details tabs (U9).
//!
//! Each tab owns what it remembers per task — a cursor, a scroll position, the
//! last thing read — and knows three things: how a read lands in it
//! (`apply`), what a key does to it (`on_action`) and how it draws (`render`).
//! None of them touches the network; [`super::detail`] reads and hands the
//! answers over, which is what lets `tests/tui_state.rs` feed a tab a failure
//! the relay would have produced without starting a server.
//!
//! * [`progress`] — the step timeline and the selected step;
//! * [`conversation`] — the whole conversation, and the per-attempt cache the
//!   progress tab shares;
//! * [`events`] — the raw event log with the detail of one event;
//! * [`conductor`] — the chat with the conductor, its decisions and harness;
//! * [`info`] — what the control plane knows about the task.

/// How long the interface waits before it asks again for a stopped task whose
/// read-only runner is still starting (D63, R12). The server waits a moment
/// for the reader as well, so a read of this kind is never repeated without
/// a pause.
pub const READER_RETRY: std::time::Duration = std::time::Duration::from_secs(2);

pub mod conductor;
pub mod conversation;
pub mod events;
pub mod info;
pub mod progress;
