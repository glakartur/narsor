//! The daemon process (U2): the only process that owns `Engine` and
//! `ExecutionStore` for write, and the mechanics of launching it detached
//! from whatever client noticed it was missing.
//!
//! [`launch`] holds the detach-spawn helper, used by `narsor daemon`'s own
//! caller (U5's `ensure_daemon_running`, not built yet) and exercised
//! directly by `tests/daemon_lifecycle.rs` in this unit.

pub mod launch;
