//! Runner state, observations and resume policy values.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::api::ExecutionStatusDto;
use crate::controller::{Freshness, PendingInputRefView};

/// State of a task's runner and its resources (slot, workload, storage) as
/// stored and exposed by the control plane. Older records and events spell
/// `Stopped` as `paused`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RunnerState {
    Queued,
    Provisioning,
    Running,
    PauseRequested,
    #[serde(alias = "paused")]
    Stopped,
    Interrupted,
    Unknown,
    Archiving,
    Archived,
}

impl RunnerState {
    /// True for `Archiving` and `Archived`.
    pub fn is_archival(self) -> bool {
        matches!(self, Self::Archiving | Self::Archived)
    }
}

/// Lifecycle-relevant fact a controller reported to the control plane.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Observation {
    StartAccepted,
    RunnerReady,
    PausedAtSafePoint,
    WorkloadLost {
        state_known: bool,
    },
    LossConfirmed,
    ResourcesDeleted,
    Status,
    #[serde(other)]
    Unrecognized,
}

/// Identity of the backend resource an observation is about.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceIdentity {
    pub uid: String,
    pub resource_generation: u64,
}

/// Normalized summary of the last controller observation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ObservedSummary {
    pub workload_present: bool,
    pub storage_present: bool,
    pub runner_status: Option<ExecutionStatusDto>,
    pub detail: Option<String>,
}

/// Archive tombstone recording the accepted request and resource cleanup.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArchiveTombstone {
    pub idempotency_key: String,
    pub generation: u64,
    pub requested_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub resources_never_created: bool,
}

/// Resume-policy state for a task after workload loss.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ResumeState {
    pub consecutive_losses: u32,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub last_loss: Option<WorkloadLoss>,
    pub exhausted: Option<ResumeExhaustion>,
    pub uncounted_losses: u32,
    pub last_loss_uncounted: bool,
}

impl ResumeState {
    /// True when no workload loss is remembered.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Where and when a task lost its workload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkloadLoss {
    pub from: RunnerState,
    pub at: DateTime<Utc>,
    pub lease_epoch: u64,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Why the resume policy gave up.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResumeExhaustion {
    pub reason: String,
    pub at: DateTime<Utc>,
    #[serde(default)]
    pub announced: bool,
}

/// Engine cursor reported by a runner, without execution internals.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionCursor {
    pub execution_id: Option<String>,
    pub current_state: Option<String>,
    pub action_execution_id: Option<String>,
    /// The action type of the step the cursor is in, as the timeline says.
    #[serde(default)]
    pub current_action_type: Option<String>,
    /// Who decides the step when it is a `decision`.
    #[serde(default)]
    pub current_decision_by: Option<String>,
}

impl ExecutionCursor {
    /// The icon of the kind of the step the cursor is in, when it has one.
    pub fn icon(&self) -> Option<&'static str> {
        crate::api::step_icon(
            self.current_action_type.as_deref(),
            self.current_decision_by.as_deref(),
        )
    }
}

/// A default harness a task cannot follow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarnessUnavailable {
    pub harness: String,
    pub kept: String,
    pub reason: String,
}

/// A condition that keeps a runner from doing its work.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerProblem {
    pub code: String,
    pub message: String,
}

/// Last controller observation exposed in an owner task summary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObservationView {
    pub observation: Observation,
    pub summary: ObservedSummary,
    pub resource: Option<ResourceIdentity>,
    pub observed_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    pub freshness: Freshness,
}

/// Last runner snapshot exposed in an owner task summary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunnerView {
    pub incarnation: String,
    pub execution_status: ExecutionStatusDto,
    pub execution_cursor: ExecutionCursor,
    pub event_cursor: u64,
    pub pending_input: Option<PendingInputRefView>,
    pub safe_point: bool,
    #[serde(default)]
    pub problem: Option<RunnerProblem>,
    #[serde(default)]
    pub conductor_available: bool,
    #[serde(default)]
    pub conductor_unavailable_reason: Option<String>,
    #[serde(default)]
    pub default_harness_unavailable: Option<HarnessUnavailable>,
    pub observed_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    pub last_contact_at: Option<DateTime<Utc>>,
    pub freshness: Freshness,
}
