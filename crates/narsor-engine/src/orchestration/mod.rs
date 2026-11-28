//! Durable, backend-neutral orchestration of remote tasks (KTD1, KTD3).
//!
//! The control plane owns *assignment*, *desired runner state*, the *fenced
//! controller leases* and the *last observed* backend/runner summary of every
//! task. It never reads or writes a `WorkflowExecution`: the runner's work
//! state stays authoritative on task storage, and nothing here can be used to
//! reconstruct it — or to recreate a workload — after a controller loss.
//!
//! * [`model`] — records and messages;
//! * `lifecycle` — the state machine, as pure transition rules;
//! * [`store`] — the file layout and its crash-safe commit protocol;
//! * [`control_plane`] — the API owners and controllers mutate tasks through;
//! * `runner_state` — runner snapshots and owner input on the same record;
//! * [`resume`] — counting workload losses for the automatic resume;
//! * `position` — the sparse keys that order a backend's queue, and the
//!   planner of a move;
//! * [`scheduler`] — admission of queued tasks under backend availability and
//!   the global task budget (U10).
//!
//! Records live under `{state}/orchestration/`, outside `executions/`, so
//! local crash recovery (`engine::recovery`) never sees them; use
//! [`ControlPlane::recover`] at startup instead.

pub mod control_plane;
pub mod error;
pub(crate) mod lifecycle;
pub mod model;
mod operator_commands;
pub(crate) mod position;
pub mod resume;
mod runner_state;
pub mod scheduler;
pub mod store;

pub use control_plane::{Clock, ControlPlane, OrchestrationRecovery, SystemClock};
pub use error::{OrchestrationError, Result};
pub use model::*;
pub use resume::{
    auto_resume_key, exhaustion_loss, limit_requeue_key, limit_stop_key, loss_state,
    observer_decide_key, slot_release_key, usage_resume_key, work_resume_key, LossState,
    ResumePolicy, AUTO_RESUME_KEY_PREFIX, EXHAUSTION_REASON_PREFIX, LIMIT_STOP_KEY_PREFIX,
    OBSERVER_KEY_PREFIX, RESERVED_KEY_PREFIXES, SLOT_RELEASE_KEY_PREFIX, UNSPECIFIED_LOSS,
    USAGE_RESUME_KEY_PREFIX, WORK_RESUME_KEY_PREFIX,
};
pub use runner_state::EntryScripts;
pub use store::OrchestrationStore;
