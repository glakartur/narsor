//! Wire types of the control-plane protocol, version [`PROTOCOL_VERSION`].
//!
//! Three audiences, three path prefixes:
//!
//! * `/control/v1/…` — owners (TUI, CLI, Web): normalized task summaries,
//!   owner commands, input, the durable event stream (KTD4);
//! * `/controller/v1/…` — environment controllers: enrollment, heartbeat,
//!   lease, desired commands, acknowledgements, reports (KTD3);
//! * `/runner/v1/…` — task runners: snapshots, the input long-poll,
//!   credential renewal (KTD3).
//!
//! Records the control plane already persists ([`TaskEvent`],
//! [`CommandReceipt`], [`RunnerSnapshot`]…) travel as they are stored — the
//! transport changes, never the shape. Identifiers in *requests* are plain
//! strings validated by the handler, so a malformed one is a typed `400`
//! rather than a body-parsing rejection.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

pub use narsor_protocol::api::{LegacyExecutionListResponse, LegacyExecutionView};
pub use narsor_protocol::controller::ConductorReadResponse;
pub use narsor_protocol::controller::{
    AckRequest, AckResponse, AcquireLeaseRequest, ArchiveTombstone, BackendLimitSource,
    BackendLimitView, CapabilitiesView, CommandAck, CommandActor, CommandReceipt, CommandsQuery,
    CommandsResponse, ConductorReadQuery, ConductorReadRequest, ControlEventEnvelope,
    ControllerListResponse, ControllerPresence, ControllerView, ConversationAttemptSummary,
    ConversationPageAccepted, ConversationPageQuery, ConversationPageRequest, ConversationResponse,
    ConversationUnavailableReason, CreateEnrollmentRequest, CreateRepositoryRequest,
    CreateTaskRequest, DefaultBackendView, DeliveredInput, DeliveredOperatorCommand, DesiredAction,
    DesiredCommand, EnrollRequest, EnrollResponse, EnrollmentResponse, ExecutionCursor,
    ExecutionEventsPage, ExecutionEventsQuery, ExecutionEventsRequest, ExecutionEventsResponse,
    ExecutionEventsSource, Freshness, HarnessUnavailable, HeartbeatResponse, InputAckRequest,
    InputAckResponse, InputUpdate, InputView, LaunchRequest, LeaseEpochRequest, LeaseView,
    MaterialChange, MoveTaskRequest, MoveTaskTarget, ObservationView, ObservedSummary,
    OperatorCommand, OperatorCommandAckRequest, OperatorCommandAckResponse, OperatorCommandView,
    OwnerCommand, OwnerCommandRequest, OwnerCommandResponse, OwnerRef, PanelChoice, PanelForm,
    PanelFormKind, PanelOffer, PendingInputRefView, QueueView, RelayFailure, ReportRequest,
    ReportResponse, RepositoryListResponse, RepositoryView, ResourceIdentity,
    ResourceProfileRequest, ResumeExhaustion, ResumeState, RotateKeyRequest, RotateKeyResponse,
    RunnerCredentialRequest, RunnerCredentialResponse, RunnerDirective, RunnerLaunchView,
    RunnerPollQuery, RunnerPollResponse, RunnerProblem, RunnerProgress, RunnerRenewResponse,
    RunnerSnapshotRequest, RunnerSnapshotResponse, RunnerState, RunnerView, SetBackendLimitRequest,
    SetDefaultBackendRequest, SubmitCommentRequest, SubmitCommentResponse, SubmitInputRequest,
    SubmitInputResponse, SubmitOperatorCommandRequest, SubmitOperatorCommandResponse, TaskActionId,
    TaskActionOffer, TaskActionOption, TaskEvent, TaskEventKind, TaskListResponse, TaskState,
    TaskSummary, TimelineResponse, TransitionExit, TransitionOffer, ValidateWorkflowRequest,
    ValidateWorkflowResponse, WorkflowInputType, WorkflowInputView, WorkflowListResponse,
    WorkflowSource, WorkflowSourceRequest, WorkflowSourceResponse, WorkflowView, WorkspaceRequest,
    CONVERSATION_READER_STARTING, CONVERSATION_RECORD_UNREADABLE,
    CONVERSATION_RECORD_UNREADABLE_PREFIX, CONVERSATION_UNAVAILABLE,
};

use crate::domain::TaskId;
use crate::orchestration::{
    MoveTarget, PendingInputRef, QueueStatus, QueuedInput, QueuedOperatorCommand, RunnerSnapshot,
};
pub use narsor_protocol::controller::{
    usage_window_length, HarnessUsageReport, HarnessUsageResponse, HarnessUsageView, UsageLimit,
    UsageStatus, MAX_USAGE_LIMITS, USAGE_STATE_LIFETIME,
};
pub use narsor_protocol::controller::{
    TaskDefaultHarness, WORKSTATION_RISK, WORKSTATION_RISK_NOT_ACCEPTED,
};

pub(super) fn runner_snapshot_request(
    snapshot: &RunnerSnapshot,
) -> Result<RunnerSnapshotRequest, serde_json::Error> {
    Ok(RunnerSnapshotRequest {
        snapshot_sequence: snapshot.snapshot_sequence,
        execution_status: snapshot.execution_status,
        execution_cursor: snapshot.execution_cursor.clone(),
        event_cursor: snapshot.event_cursor,
        pending_input: snapshot
            .pending_input
            .as_ref()
            .map(|pending| {
                Ok(PendingInputRefView {
                    id: pending.id.clone(),
                    attempt_id: pending.attempt_id.clone(),
                    form: serde_json::to_value(&pending.form)?,
                })
            })
            .transpose()?,
        conversation: snapshot.conversation.clone(),
        timeline: snapshot.timeline.clone(),
        safe_point: snapshot.safe_point,
        problem: snapshot.problem.clone(),
        attention: snapshot.attention.clone(),
        conductor_available: snapshot.conductor_available,
        conductor_unavailable_reason: snapshot.conductor_unavailable_reason.clone(),
        default_harness_unavailable: snapshot.default_harness_unavailable.clone(),
        display: snapshot.display.clone(),
        generated_title: snapshot.generated_title.clone(),
        observed_at: snapshot.observed_at,
    })
}

pub(super) fn runner_snapshot_from_request(
    request: RunnerSnapshotRequest,
) -> Result<RunnerSnapshot, serde_json::Error> {
    Ok(RunnerSnapshot {
        snapshot_sequence: request.snapshot_sequence,
        execution_status: request.execution_status,
        execution_cursor: request.execution_cursor,
        event_cursor: request.event_cursor,
        pending_input: request
            .pending_input
            .map(|pending| {
                Ok(PendingInputRef {
                    id: pending.id,
                    attempt_id: pending.attempt_id,
                    form: serde_json::from_value(pending.form)?,
                })
            })
            .transpose()?,
        conversation: request.conversation,
        timeline: request.timeline,
        safe_point: request.safe_point,
        problem: request.problem,
        attention: request.attention,
        conductor_available: request.conductor_available,
        conductor_unavailable_reason: request.conductor_unavailable_reason,
        default_harness_unavailable: request.default_harness_unavailable,
        display: request.display,
        generated_title: request.generated_title,
        observed_at: request.observed_at,
    })
}

impl From<MoveTarget> for MoveTaskTarget {
    fn from(target: MoveTarget) -> Self {
        match target {
            MoveTarget::Before(task_id) => Self::Before(task_id.to_string()),
            MoveTarget::After(task_id) => Self::After(task_id.to_string()),
            MoveTarget::Top => Self::Top,
            MoveTarget::Bottom => Self::Bottom,
        }
    }
}

/// Version of this protocol; part of every path (`…/v1/…`).
pub const PROTOCOL_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Owner side (KTD4)
// ---------------------------------------------------------------------------

impl From<&QueuedInput> for InputView {
    fn from(input: &QueuedInput) -> Self {
        Self {
            input_id: input.input_id.clone(),
            pending_input_id: input.pending_input_id.clone(),
            status: input.status,
            submitted_at: input.submitted_at,
            updated_at: input.updated_at,
            detail: input.detail.clone(),
        }
    }
}

impl From<&QueuedOperatorCommand> for OperatorCommandView {
    fn from(queued: &QueuedOperatorCommand) -> Self {
        Self {
            command_id: queued.command_id.clone(),
            command: queued.command.clone(),
            status: queued.status,
            submitted_at: queued.submitted_at,
            updated_at: queued.updated_at,
            detail: queued.detail.clone(),
        }
    }
}

impl From<&QueueStatus> for QueueView {
    fn from(status: &QueueStatus) -> Self {
        Self {
            message: status.reason.describe(),
            reason: status.reason.clone(),
            since: status.since,
        }
    }
}

// ---------------------------------------------------------------------------
// Controller side (KTD3)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Runner side (KTD3)
// ---------------------------------------------------------------------------

/// One page of an attempt's raw conversation — the shape the local API
/// serves, so a client renders a remote page exactly like a local one.
pub type ConversationPage = narsor_protocol::api::ConversationResponse;

// ---------------------------------------------------------------------------
// Stream cursor
// ---------------------------------------------------------------------------

/// Position in the control-plane event stream: the last sequence seen of
/// every task.
///
/// Each task's log is gapless and ordered, so "everything after this
/// cursor" is well defined per task and exactly-once across reconnects —
/// no global sequence (and no extra file every mutation would have to
/// update under a global lock) is needed for the few tasks one installation
/// runs. Text form: `task-a:5,task-b:3`; task ids never contain `:` or `,`.
/// The empty string is the start of the stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamCursor(BTreeMap<TaskId, u64>);

impl StreamCursor {
    /// The position of `task_id` (0 = nothing seen).
    pub fn position(&self, task_id: &TaskId) -> u64 {
        self.0.get(task_id).copied().unwrap_or(0)
    }

    /// Moves `task_id` forward to `sequence`; never backwards.
    pub fn advance(&mut self, task_id: &TaskId, sequence: u64) {
        let entry = self.0.entry(task_id.clone()).or_insert(0);
        *entry = (*entry).max(sequence);
    }
}

impl fmt::Display for StreamCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for (task_id, sequence) in &self.0 {
            if !first {
                f.write_str(",")?;
            }
            first = false;
            write!(f, "{task_id}:{sequence}")?;
        }
        Ok(())
    }
}

/// A cursor that does not parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid stream cursor '{0}'")]
pub struct InvalidCursor(pub String);

impl FromStr for StreamCursor {
    type Err = InvalidCursor;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut cursor = Self::default();
        for part in text.split(',').filter(|part| !part.is_empty()) {
            let invalid = || InvalidCursor(text.to_string());
            let (task, sequence) = part.rsplit_once(':').ok_or_else(invalid)?;
            let task_id = TaskId::parse(task).map_err(|_| invalid())?;
            let sequence = sequence.parse::<u64>().map_err(|_| invalid())?;
            cursor.advance(&task_id, sequence);
        }
        Ok(cursor)
    }
}

/// `GET /control/v1/events/stream` query.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StreamQuery {
    /// Resume after this cursor; overrides `Last-Event-ID`.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Only this task's events.
    #[serde(default)]
    pub task_id: Option<String>,
}

/// `GET /control/v1/tasks/:task_id/events` query.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventsAfterQuery {
    /// Only events with a greater sequence.
    #[serde(default)]
    pub after: u64,
}

// ---------------------------------------------------------------------------
// Catalogue, repositories, history and capabilities (U6)
// ---------------------------------------------------------------------------

/// `GET /control/v1/tasks` query.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskListQuery {
    /// `true`: only archived tasks (archiving or archived), newest archive
    /// first — the Historia. `false`: only the others, in queue order.
    /// Absent: every task, as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
}
