//! What a runner reports about its work (KTD3, KTD4): its snapshot, the
//! input request it waits on, and constants both the runner and the control
//! plane apply to operator commands.
//!
//! Produced by the runner and stored by the control plane, so it lives in
//! the model both build on.

use chrono::{DateTime, Utc};
use narsor_protocol::api::Timeline;
use narsor_protocol::controller::{
    AttentionItem, ConversationAvailability, ExecutionCursor, GateWatch, HarnessUnavailable,
    PendingInputRefView, RunnerProblem, RunnerSnapshotRequest, UsageLimitWait,
};
use serde::{Deserialize, Serialize};

use super::human_input::PendingHumanInput;
use super::{ExecutionStatus, WorkspaceHeader};

/// The input request a runner is waiting on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingInputRef {
    /// Identity of the request: [`PendingHumanInput::episode_key`]. An owner
    /// answer names it, and is refused for any other.
    pub id: String,
    /// The agent attempt that asked, when an agent asked.
    pub attempt_id: Option<String>,
    /// The form, exactly as the runner's engine recorded it, so a client
    /// renders it without the workflow definition.
    pub form: PendingHumanInput,
}

/// Everything a runner reports about its work (KTD3, KTD4).
///
/// Cursors and summaries only: workspace files, secret values and raw
/// conversation lines never leave task storage through this type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunnerSnapshot {
    /// Runner-assigned, strictly increasing within one incarnation. Orders
    /// reports that arrive out of order; restarts at any value with a new
    /// incarnation.
    pub snapshot_sequence: u64,
    /// The runner's execution status — the work-state detail.
    pub execution_status: ExecutionStatus,
    /// Where its engine stands.
    #[serde(default)]
    pub execution_cursor: ExecutionCursor,
    /// Sequence of the last event in the runner's own event log. Never goes
    /// backwards, not even across incarnations: the log lives on task
    /// storage, which outlives every Pod.
    pub event_cursor: u64,
    /// The input request it waits on.
    #[serde(default)]
    pub pending_input: Option<PendingInputRef>,
    /// The rendered `watch` block of the `decision` gate the pending input
    /// belongs to, when the gate has one. Its episode is the id of the
    /// pending input ([`PendingInputRef::id`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_watch: Option<GateWatch>,
    /// Its conversation's availability and summary.
    #[serde(default)]
    pub conversation: ConversationAvailability,
    /// The step timeline (positions and links, no content).
    #[serde(default)]
    pub timeline: Option<Timeline>,
    /// True when the runner stands at a durable safe point and may lose its
    /// compute without losing work (the acknowledgement a pause waits for).
    #[serde(default)]
    pub safe_point: bool,
    /// Why the runner cannot do its work, when it cannot — for instance a
    /// base image that breaks the runner contract (R22), or a storage it
    /// will not touch before a newer launch generation authorizes it (the
    /// owner's or the resume policy's command, D58). Diagnosable text, never
    /// a secret.
    #[serde(default)]
    pub problem: Option<RunnerProblem>,
    /// What waits on the owner, oldest first (U4). Computed by the runner
    /// from its pending input and problem; the conductor's items (U5) join
    /// it. Provisioning failures are not here: only the control plane knows
    /// them.
    #[serde(default)]
    pub attention: Vec<AttentionItem>,
    /// Whether the runner hosts a conductor (U5): its harness is usable in
    /// this task's environment. Without one a failed step goes to the
    /// owner as it is, with no diagnosis and no proposals.
    #[serde(default)]
    pub conductor_available: bool,
    /// Why there is no conductor, when there is none.
    #[serde(default)]
    pub conductor_unavailable_reason: Option<String>,
    /// Whether the current step has a harness session the owner's message
    /// would continue (rather than start a new session with). `false` also
    /// from a runner that does not say.
    #[serde(default)]
    pub agent_session_resumable: bool,
    /// The installation's default harness this task cannot switch to, while
    /// it cannot (U7, R16c): its backend does not run it, or its binary is
    /// not in the task's environment. The task keeps its harness meanwhile;
    /// this is information for the owner, never a [`RunnerProblem`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_harness_unavailable: Option<HarnessUnavailable>,
    /// The header values of the task's workspaces, in the workflow's order.
    /// Empty when the runner has none to report, which leaves what the
    /// control plane knows from the launch in place; the last report that
    /// has some wins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<WorkspaceHeader>,
    /// Title the conductor generated for a task whose operator gave none;
    /// the last report that has one wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_title: Option<String>,
    /// What the parked execution waits for when it waits for the harness's
    /// usage limit to reset. Read from the execution's own record, so a
    /// restarted runner reports the same wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_wait: Option<UsageLimitWait>,
    /// When the runner took this snapshot.
    pub observed_at: DateTime<Utc>,
}

impl RunnerSnapshot {
    /// Whether the snapshot reports an execution and no problem.
    pub fn speaks_of_execution(&self) -> bool {
        self.execution_cursor.execution_id.is_some() && self.problem.is_none()
    }

    /// The wire form a runner reports (`POST /runner/v1/snapshots`).
    pub fn to_request(&self) -> Result<RunnerSnapshotRequest, serde_json::Error> {
        let snapshot = self;
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
            gate_watch: snapshot.gate_watch.clone(),
            conversation: snapshot.conversation.clone(),
            timeline: snapshot.timeline.clone(),
            safe_point: snapshot.safe_point,
            problem: snapshot.problem.clone(),
            attention: snapshot.attention.clone(),
            conductor_available: snapshot.conductor_available,
            conductor_unavailable_reason: snapshot.conductor_unavailable_reason.clone(),
            agent_session_resumable: snapshot.agent_session_resumable,
            default_harness_unavailable: snapshot.default_harness_unavailable.clone(),
            workspaces: snapshot.workspaces.clone(),
            generated_title: snapshot.generated_title.clone(),
            usage_wait: snapshot.usage_wait.clone(),
            observed_at: snapshot.observed_at,
        })
    }

    /// The snapshot a runner reported, read back from its wire form.
    pub fn from_request(request: RunnerSnapshotRequest) -> Result<Self, serde_json::Error> {
        Ok(Self {
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
            gate_watch: request.gate_watch,
            conversation: request.conversation,
            timeline: request.timeline,
            safe_point: request.safe_point,
            problem: request.problem,
            attention: request.attention,
            conductor_available: request.conductor_available,
            conductor_unavailable_reason: request.conductor_unavailable_reason,
            agent_session_resumable: request.agent_session_resumable,
            default_harness_unavailable: request.default_harness_unavailable,
            workspaces: request.workspaces,
            generated_title: request.generated_title,
            usage_wait: request.usage_wait,
            observed_at: request.observed_at,
        })
    }
}

/// Why a [`OperatorCommand::SetConductorHarness`](narsor_protocol::controller::OperatorCommand::SetConductorHarness) is refused or settled
/// unapplied: the harness is no longer chosen per task.
pub const RETIRED_CONDUCTOR_HARNESS: &str =
    "the conductor's harness is no longer chosen per task; the conductor follows the \
     installation's default harness";
