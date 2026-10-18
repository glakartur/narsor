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
//! * [`scheduler`] — admission of queued tasks by backend capacity (U10).
//!
//! Records live under `{state}/orchestration/`, outside `executions/`, so
//! local crash recovery (`persistence::recovery`) never sees them; use
//! [`ControlPlane::recover`] at startup instead.

pub mod control_plane;
pub mod error;
pub(crate) mod lifecycle;
pub mod model;
mod operator_commands;
pub mod resume;
mod runner_state;
pub mod scheduler;
pub mod store;

pub use control_plane::{Clock, ControlPlane, OrchestrationRecovery, SystemClock};
pub use error::{OrchestrationError, Result};
pub use model::*;
pub use resume::{
    auto_resume_key, exhaustion_loss, loss_state, LossState, ResumePolicy, AUTO_RESUME_KEY_PREFIX,
    EXHAUSTION_REASON_PREFIX, UNSPECIFIED_LOSS,
};
pub use store::OrchestrationStore;
