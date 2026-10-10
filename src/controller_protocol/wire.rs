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

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::conductor::ConductorView;
use crate::domain::{
    BackendId, Event, ExecutionStatus, ResourceProfile, TaskDisplay, TaskId, TaskLaunchSpec,
};
use crate::engine::{HumanInputDelivery, Timeline};
pub use crate::environment::TaskDefaultHarness;
use crate::orchestration::{
    ArchiveTombstone, AttentionItem, CommandReceipt, ConversationAttemptSummary, ExecutionCursor,
    HarnessUnavailable, InputStatus, MoveTarget, Observation, ObservedSummary, OperatorCommand,
    OperatorCommandStatus, OwnerCommand, PendingInputRef, QueueReason, QueueStatus, QueuedInput,
    QueuedOperatorCommand, ResourceIdentity, RunnerProblem, RunnerSnapshot, TaskEvent,
    TaskLifecycle,
};

/// Version of this protocol; part of every path (`…/v1/…`).
pub const PROTOCOL_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Owner side (KTD4)
// ---------------------------------------------------------------------------

/// What a new task runs. Everything here is *intent*: the control plane
/// resolves it against its configuration into a sealed
/// [`TaskLaunchSpec`] before anything is stored (U6), and refuses the
/// request — with nothing persisted — when it does not resolve.
///
/// Unknown fields are refused, so a client that tries to hand the control
/// plane a credential value under some new key is told no instead of having
/// it silently dropped or, worse, stored.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LaunchRequest {
    /// Workflow to run.
    pub workflow_id: String,
    /// The version the client expects; another version in the catalogue is
    /// a refusal, not a silent upgrade.
    pub workflow_version: u32,
    /// Workflow inputs.
    #[serde(default)]
    pub inputs: Value,
    /// The repository and pinned commit to clone (R39); the workflow's own
    /// `globals.workspace` names the repository when this does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceRequest>,
    /// Profile (R16); the owner default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Fallback harness for agent steps that name none (D56); the profile's,
    /// else the live default, when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// CPU and memory; the backend's default when absent, never above its
    /// maximum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceProfile>,
}

/// The workspace part of a [`LaunchRequest`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRequest {
    /// Registered repository id; the workflow's when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// Full commit id to clone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// `POST /control/v1/tasks`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CreateTaskRequest {
    /// Client-chosen id; repeating the identical request with it returns
    /// the task it created, even after the configuration it resolved
    /// against (the default backend, say) changed. A *different* request
    /// with the same id is a `task_conflict`. Generated when absent — but
    /// then a retry creates a second task.
    #[serde(default)]
    pub task_id: Option<String>,
    /// Backend to pin the task to (R10); the owner's `default_backend` when
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_id: Option<String>,
    /// What to run.
    pub launch: LaunchRequest,
}

/// Freshness of something the control plane only *observed*.
///
/// A summary is never upgraded into a different state because it is old: it
/// is shown as it was, with its time and this flag (R14).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    /// Its source is reporting right now.
    Live,
    /// Its source has not been heard from within the staleness window, or is
    /// not expected to report at all (paused, interrupted, archived).
    Stale,
}

/// The last controller observation of a task, as clients see it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObservationView {
    /// What was observed.
    pub observation: Observation,
    /// Normalized summary.
    pub summary: ObservedSummary,
    /// Resource incarnation it was about.
    pub resource: Option<ResourceIdentity>,
    /// When the backend observed it.
    pub observed_at: DateTime<Utc>,
    /// When the control plane accepted it.
    pub received_at: DateTime<Utc>,
    /// Whether the controller still vouches for it.
    pub freshness: Freshness,
}

/// The last runner snapshot of a task, as clients see it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunnerView {
    /// The incarnation that reported it.
    pub incarnation: String,
    /// The runner's execution status at that time — never fabricated.
    pub execution_status: ExecutionStatus,
    /// Where its engine stood.
    pub execution_cursor: ExecutionCursor,
    /// Its event cursor.
    pub event_cursor: u64,
    /// The input request it waited on.
    pub pending_input: Option<PendingInputRef>,
    /// Whether it stood at a safe point.
    pub safe_point: bool,
    /// Why it could not work, when it said so.
    #[serde(default)]
    pub problem: Option<RunnerProblem>,
    /// Whether the runner hosts a conductor (U5).
    #[serde(default)]
    pub conductor_available: bool,
    /// Why it hosts none, when it does not.
    #[serde(default)]
    pub conductor_unavailable_reason: Option<String>,
    /// The installation's default harness the task cannot switch to, while
    /// it cannot (U7, R16c): information for the owner, never a problem.
    #[serde(default)]
    pub default_harness_unavailable: Option<HarnessUnavailable>,
    /// When the runner took the snapshot.
    pub observed_at: DateTime<Utc>,
    /// When the control plane accepted it.
    pub received_at: DateTime<Utc>,
    /// When the runner was last heard from at all.
    pub last_contact_at: Option<DateTime<Utc>>,
    /// Whether it is current.
    pub freshness: Freshness,
}

/// The controller currently responsible for a task's backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControllerPresence {
    /// The lease holder.
    pub controller_id: String,
    /// Its fencing epoch.
    pub lease_epoch: u64,
    /// When its lease runs out.
    pub lease_expires_at: DateTime<Utc>,
    /// Its last heartbeat.
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    /// Stale when the lease expired or no heartbeat arrived lately.
    pub freshness: Freshness,
}

/// An owner input, without its payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputView {
    /// Control-plane id.
    pub input_id: String,
    /// The runner request it answers.
    pub pending_input_id: String,
    /// What became of it.
    pub status: InputStatus,
    /// When it was submitted.
    pub submitted_at: DateTime<Utc>,
    /// When its status last changed.
    pub updated_at: DateTime<Utc>,
    /// The runner's reason, if it rejected it.
    pub detail: Option<String>,
}

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

/// An operator command, as owners see it (U3).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OperatorCommandView {
    /// Control-plane id.
    pub command_id: String,
    /// What it asks.
    pub command: OperatorCommand,
    /// What became of it.
    pub status: OperatorCommandStatus,
    /// When it was queued.
    pub submitted_at: DateTime<Utc>,
    /// When its status last changed.
    pub updated_at: DateTime<Utc>,
    /// Why the runner rejected it, if it did.
    pub detail: Option<String>,
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

/// The normalized, backend-neutral summary of one task (KTD4).
///
/// Nothing in it names a Kubernetes object, a runner address or a secret.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskSummary {
    /// Identity.
    pub task_id: TaskId,
    /// Backend it is pinned to.
    pub backend_id: BackendId,
    /// Orchestration state.
    pub lifecycle: TaskLifecycle,
    /// Desired generation; pass it as `expected_generation` to make an
    /// owner command conditional.
    pub generation: u64,
    /// Record revision — the task's position in the event stream.
    pub revision: u64,
    /// 1-based position in the backend's queue; the list is in this order.
    pub rank: u32,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Creation order across all tasks.
    pub queue_sequence: u64,
    /// Workflow it runs.
    pub workflow_id: String,
    /// Workflow version.
    pub workflow_version: u32,
    /// Digest of its immutable launch spec.
    pub launch_digest: String,
    /// Identity of its pinned environment (R21): equal for two tasks that
    /// run on exactly the same images, bundle, tools, profile and harness.
    pub environment_identity: String,
    /// Archive tombstone, once archived.
    pub archive: Option<ArchiveTombstone>,
    /// Last controller observation.
    pub observation: Option<ObservationView>,
    /// Last runner snapshot.
    pub runner: Option<RunnerView>,
    /// The backend's current controller.
    pub controller: Option<ControllerPresence>,
    /// Owner inputs, oldest first.
    pub inputs: Vec<InputView>,
    /// Operator commands (interrupt, transition, restart), oldest first.
    #[serde(default)]
    pub operator_commands: Vec<OperatorCommandView>,
    /// What waits on the owner, oldest first (U4): the runner's report
    /// minus what the owner has answered already, plus a failed
    /// provisioning. Metadata only; the content comes with the details.
    #[serde(default)]
    pub attention: Vec<AttentionItem>,
    /// While queued: whether the task was admitted, or why it waits (U10).
    /// `None` outside `Queued`.
    #[serde(default)]
    pub queue: Option<QueueView>,
    /// Title and links the workflow declares: rendered from the sealed
    /// inputs when the task is created, replaced by the runner's latest
    /// report. Empty when the workflow declares none.
    #[serde(default)]
    pub display: TaskDisplay,
    /// Where a manual transition can lead from the state the runner reports
    /// (U11): the declared exits with their descriptions, whether the state
    /// allows an override and, for one, the states it can go to. `None` when
    /// the task is not running, the runner has reported no state yet or the
    /// workflow snapshot cannot be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_transitions: Option<TransitionOffer>,
    /// The control plane's "now" this summary was computed at, so a client
    /// can show the age of every timestamp above.
    pub as_of: DateTime<Utc>,
}

/// What a manual transition out of a task's current state can be (U11).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransitionOffer {
    /// The state the runner reports being in.
    pub from_state: String,
    /// The declared transitions, one per target (the lowest label wins when
    /// several lead to the same state, as the runner picks it).
    pub exits: Vec<TransitionExit>,
    /// Whether the state declares `allow_manual_override` (D30).
    pub override_allowed: bool,
    /// Every other state of the workflow: what an override can go to. Empty
    /// unless `override_allowed`.
    #[serde(default)]
    pub override_targets: Vec<String>,
}

/// One declared transition out of a state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransitionExit {
    /// The output label that leads there.
    pub label: String,
    /// The state it leads to: what a `Transition` command names.
    pub target: String,
    /// What choosing the label means, when the workflow says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The scheduler's verdict on a queued task, as clients see it (U10).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueView {
    /// Machine-readable verdict.
    pub reason: QueueReason,
    /// The same, for people.
    pub message: String,
    /// Since when it holds.
    pub since: DateTime<Utc>,
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

/// `POST /control/v1/tasks/:task_id/operator-commands`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubmitOperatorCommandRequest {
    /// What to ask the runner to do.
    #[serde(flatten)]
    pub command: OperatorCommand,
    /// Client-chosen key; a retry reuses it.
    pub idempotency_key: String,
    /// Refuse unless the task is still at this desired generation.
    #[serde(default)]
    pub expected_generation: Option<u64>,
}

/// Response of a submitted operator command: queued, not yet carried out;
/// its verdict follows in the task's event stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubmitOperatorCommandResponse {
    /// The queued command.
    pub command: OperatorCommandView,
    /// True when this was a retry.
    pub replayed: bool,
}

/// `POST /control/v1/tasks/:task_id/move`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MoveTaskRequest {
    /// Where the queued task goes. Repeating a move that already holds is
    /// a no-op, which makes a retry safe without a key.
    pub target: MoveTarget,
    /// Refuse unless the task is still at this desired generation.
    #[serde(default)]
    pub expected_generation: Option<u64>,
}

/// `GET`/`PUT /control/v1/default-backend`: the backend a new task without
/// an explicit one is pinned to (R10).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DefaultBackendView {
    /// The configured `default_backend`, the fallback.
    pub configured: String,
    /// The owner's durable choice, if any.
    #[serde(rename = "override")]
    pub override_backend: Option<BackendId>,
    /// What a new task gets: the override, else the configured value.
    pub effective: String,
    /// Grows with every change of the override; 0 when never set.
    pub revision: u64,
    /// When the override last changed.
    pub updated_at: Option<DateTime<Utc>>,
}

/// `PUT /control/v1/default-backend`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetDefaultBackendRequest {
    /// A registered backend; `null` removes the override, so the
    /// configured value applies again. Existing tasks never move.
    #[serde(default)]
    pub backend_id: Option<String>,
}

/// `GET /control/v1/tasks`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskListResponse {
    /// The caller's tasks, in creation order.
    pub tasks: Vec<TaskSummary>,
    /// Stream cursor at exactly these summaries: open the event stream with
    /// it to receive every later change once, and nothing earlier.
    pub cursor: String,
}

/// `POST /control/v1/tasks/:task_id/commands`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OwnerCommandRequest {
    /// The command.
    pub command: OwnerCommand,
    /// Client-chosen key; a retry reuses it.
    pub idempotency_key: String,
    /// Refuse unless the task is still at this generation.
    #[serde(default)]
    pub expected_generation: Option<u64>,
}

/// Response of an owner command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OwnerCommandResponse {
    /// The receipt — identical for every retry.
    pub receipt: CommandReceipt,
    /// True when this delivery was a retry.
    pub replayed: bool,
    /// The task now.
    pub task: TaskSummary,
}

/// `GET /control/v1/tasks/:task_id/timeline`: the last timeline the runner
/// reported, with its time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TimelineResponse {
    /// The task.
    pub task_id: TaskId,
    /// Whether the runner reporting it is current.
    pub freshness: Freshness,
    /// When the runner took the snapshot it came from.
    pub observed_at: Option<DateTime<Utc>>,
    /// The timeline; `None` before the runner reported one.
    pub timeline: Option<Timeline>,
    /// The runner's execution status at that time.
    #[serde(default)]
    pub execution_status: Option<ExecutionStatus>,
    /// The workflow state the execution was in.
    #[serde(default)]
    pub current_state: Option<String>,
    /// The label of the position (attempt) under way — in progress or
    /// awaiting a decision — when there is one: the step a client follows.
    #[serde(default)]
    pub current_position: Option<String>,
}

/// Why a task's conversation cannot be served right now.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationUnavailableReason {
    /// No runner has reported yet.
    NotStarted,
    /// The task is pausing or paused: its compute is (being) removed.
    Paused,
    /// The workload was lost (Interrupted or Unknown).
    Interrupted,
    /// The task is archived (or being archived).
    Archived,
    /// The runner should be live but has not been heard from lately.
    RunnerUnreachable,
    /// The runner itself said it cannot serve it.
    RunnerReported,
}

/// `GET /control/v1/tasks/:task_id/conversation`.
///
/// Summaries only: the raw transcript never leaves task storage (KTD4).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum ConversationResponse {
    /// The runner is live; this is what it reported.
    Available {
        /// Attempts with a conversation record.
        attempts: Vec<ConversationAttemptSummary>,
        /// When the runner reported it.
        observed_at: DateTime<Utc>,
    },
    /// Explicitly unavailable, with the last safe summary if there is one.
    Unavailable {
        /// Why.
        reason: ConversationUnavailableReason,
        /// The last summary the runner reported, if any.
        last_attempts: Vec<ConversationAttemptSummary>,
        /// When that was.
        observed_at: Option<DateTime<Utc>>,
    },
}

/// `POST /control/v1/tasks/:task_id/input`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubmitInputRequest {
    /// The request it answers ([`PendingInputRef::id`]).
    pub pending_input_id: String,
    /// Client-chosen key; a retry reuses it.
    pub idempotency_key: String,
    /// The answer.
    pub delivery: HumanInputDelivery,
}

/// Response of a submitted input.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubmitInputResponse {
    /// The queued input.
    pub input: InputView,
    /// True when this was a retry.
    pub replayed: bool,
}

/// One frame of the control-plane event stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ControlEventEnvelope {
    /// Stream cursor *after* this event (also its SSE `id`); reconnecting
    /// with it delivers every later event exactly once.
    pub cursor: String,
    /// The event as the task's log holds it.
    pub event: TaskEvent,
}

/// `POST /control/v1/backends/:backend_id/enrollments`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateEnrollmentRequest {
    /// Label for the controller it will enroll.
    #[serde(default)]
    pub name: Option<String>,
    /// How long the code may be redeemed; bounded by the server.
    #[serde(default)]
    pub ttl_secs: Option<u64>,
}

/// A one-time enrollment code, shown exactly once.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnrollmentResponse {
    /// The code to hand to the controller.
    pub enrollment_code: String,
    /// The backend the controller will be scoped to.
    pub backend_id: BackendId,
    /// After this the code is refused.
    pub expires_at: DateTime<Utc>,
}

/// A controller identity, without its credential.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControllerView {
    /// Identity.
    pub controller_id: String,
    /// Its backend.
    pub backend_id: BackendId,
    /// Label.
    pub name: Option<String>,
    /// When it enrolled.
    pub enrolled_at: DateTime<Utc>,
    /// When it was revoked.
    pub revoked_at: Option<DateTime<Utc>>,
    /// Its last heartbeat.
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    /// Whether it holds its backend's unexpired lease.
    pub holds_lease: bool,
}

/// `GET /control/v1/controllers`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControllerListResponse {
    /// Every identity, revoked ones included.
    pub controllers: Vec<ControllerView>,
}

// ---------------------------------------------------------------------------
// Controller side (KTD3)
// ---------------------------------------------------------------------------

/// `POST /controller/v1/enroll` (no credential yet: the one-time code and
/// a signature by the key being bound are the proof).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnrollRequest {
    /// The one-time code.
    pub enrollment_code: String,
    /// The controller's Ed25519 public key, base64url without padding.
    /// Missing reads as empty, so an incomplete enrollment is refused with
    /// the same bare `401` as a wrong code rather than a parse error.
    #[serde(default)]
    pub public_key: String,
    /// Signature by that key over
    /// [`crate::auth::pop::enrollment_message`]: the enrolling party holds it.
    #[serde(default)]
    pub proof: String,
}

impl EnrollRequest {
    /// An enrollment of `key` with `enrollment_code`, signed by the key.
    pub fn signed(enrollment_code: &str, key: &crate::auth::pop::ControllerKey) -> Self {
        let public_key = key.public_key();
        Self {
            enrollment_code: enrollment_code.to_string(),
            proof: key.sign(&crate::auth::pop::enrollment_message(
                enrollment_code,
                &public_key,
            )),
            public_key,
        }
    }
}

/// The new identity. The server never knew a secret of it: the controller
/// keeps its private key and signs every later request with it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnrollResponse {
    /// Identity.
    pub controller_id: String,
    /// Backend it is scoped to.
    pub backend_id: BackendId,
    /// Protocol version the server speaks.
    pub protocol_version: u32,
}

/// `POST /controller/v1/key` — replaces the identity's public key. Signed
/// with the current key like every request; `proof` is the new key's
/// signature over [`crate::auth::pop::rotation_message`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RotateKeyRequest {
    /// The new public key.
    pub public_key: String,
    /// Signature by the new key.
    pub proof: String,
}

/// The identity after a key rotation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RotateKeyResponse {
    /// Identity.
    pub controller_id: String,
    /// When the new key took effect; the old one stopped working then.
    pub rotated_at: DateTime<Utc>,
}

/// A lease as the controller sees it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeaseView {
    /// Backend.
    pub backend_id: BackendId,
    /// Holder.
    pub controller_id: String,
    /// Fencing epoch; present it with every report.
    pub epoch: u64,
    /// When this epoch was acquired.
    pub acquired_at: DateTime<Utc>,
    /// Until when it holds unless renewed.
    pub expires_at: DateTime<Utc>,
}

/// `POST /controller/v1/heartbeat` response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HeartbeatResponse {
    /// Identity.
    pub controller_id: String,
    /// Its backend.
    pub backend_id: BackendId,
    /// The control plane's clock.
    pub server_time: DateTime<Utc>,
    /// The backend's lease, if this controller holds it.
    pub lease: Option<LeaseView>,
}

/// `POST /controller/v1/lease`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcquireLeaseRequest {
    /// Requested validity; bounded by the server.
    #[serde(default)]
    pub ttl_secs: Option<u64>,
}

/// `POST /controller/v1/lease/renew` and `/lease/release`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeaseEpochRequest {
    /// The epoch the controller holds.
    pub epoch: u64,
    /// Requested validity (renew only); bounded by the server.
    #[serde(default)]
    pub ttl_secs: Option<u64>,
}

/// What the controller must make true for one task.
///
/// Derived from the task's lifecycle; generation-stamped so the controller
/// acts on — and reports against — exactly one desired state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DesiredAction {
    /// Queued and admitted by the control plane's scheduler (U10) — a
    /// queued task without a capacity reservation is not offered at all,
    /// and its `StartAccepted` would be refused as `not_admitted`.
    /// Take the start over. **Report `StartAccepted` and wait for
    /// it to be accepted before creating any resource.** An archive racing
    /// the start completes at once on a task no controller accepted, so a
    /// resource created before the report would leak; a refused report
    /// means "do not create".
    Start,
    /// Provisioning: create or reconcile storage and workload by the task's
    /// deterministic `resource_key` (never a duplicate), then report
    /// `RunnerReady` once the runner is up.
    Provision,
    /// Running: keep it running and observe it; report a loss, never
    /// recreate it.
    Run,
    /// PauseRequested: wait for the runner's safe-point acknowledgement
    /// (`runner.safe_point_acknowledged`), then remove only the compute and
    /// report `PausedAtSafePoint`.
    PauseAtSafePoint,
    /// Paused: ensure no compute exists; keep storage.
    KeepPaused,
    /// Interrupted/Unknown: do nothing until the owner decides (R13).
    Hold,
    /// Archiving: delete workload and storage, then report
    /// `ResourcesDeleted` (absent resources count as deleted).
    Archive,
}

/// What the control plane knows of the task's runner, for the controller.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunnerProgress {
    /// Incarnation that last reported.
    pub incarnation: String,
    /// Generation it was launched at.
    pub launch_generation: u64,
    /// Its execution status.
    pub execution_status: ExecutionStatus,
    /// Its event cursor.
    pub event_cursor: u64,
    /// Whether it acknowledged a safe point for the *current* generation —
    /// the only permission to remove compute during a pause.
    pub safe_point_acknowledged: bool,
    /// When the control plane accepted its last snapshot.
    pub received_at: DateTime<Utc>,
}

/// One desired command for a controller.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DesiredCommand {
    /// The task.
    pub task_id: TaskId,
    /// Desired generation; acknowledge it and report against it.
    pub generation: u64,
    /// Current orchestration state.
    pub lifecycle: TaskLifecycle,
    /// What to make true.
    pub action: DesiredAction,
    /// Deterministic key for every backend resource name (R7).
    pub resource_key: String,
    /// The immutable launch input.
    pub launch_spec: TaskLaunchSpec,
    /// The runner's last known progress.
    pub runner: Option<RunnerProgress>,
}

/// `GET /controller/v1/commands` query.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandsQuery {
    /// Long-poll: wait up to this long for something new; bounded by the
    /// server. 0 or absent answers at once.
    #[serde(default)]
    pub wait_secs: Option<u64>,
    /// Full resync: every live task of the backend, acknowledged or not —
    /// what a controller asks for once after (re)starting.
    #[serde(default)]
    pub all: bool,
}

/// `GET /controller/v1/commands` response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandsResponse {
    /// The lease epoch the commands were read under.
    pub lease_epoch: u64,
    /// Commands newer than this controller's acknowledgements (or all).
    pub commands: Vec<DesiredCommand>,
}

/// One acknowledgement: "I have taken over this generation of this task".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandAck {
    /// The task.
    pub task_id: String,
    /// The generation taken over.
    pub generation: u64,
}

/// `POST /controller/v1/acks`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AckRequest {
    /// Acknowledgements; duplicates are harmless.
    pub acks: Vec<CommandAck>,
}

/// `POST /controller/v1/acks` response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AckResponse {
    /// The acknowledged generation of each task afterwards.
    pub acknowledged: Vec<CommandAck>,
}

/// `POST /controller/v1/reports`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReportRequest {
    /// The task.
    pub task_id: String,
    /// The lease epoch the controller holds.
    pub lease_epoch: u64,
    /// The generation it acted on.
    pub desired_generation: u64,
    /// What it observed.
    pub observation: Observation,
    /// The resource incarnation, if one exists.
    #[serde(default)]
    pub resource: Option<ResourceIdentity>,
    /// Normalized summary — no backend types, no secrets.
    #[serde(default)]
    pub summary: ObservedSummary,
    /// When the backend observed it.
    pub observed_at: DateTime<Utc>,
}

/// `POST /controller/v1/reports` response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReportResponse {
    /// False for an absorbed duplicate.
    pub applied: bool,
    /// Lifecycle afterwards.
    pub lifecycle: TaskLifecycle,
    /// Desired generation afterwards.
    pub generation: u64,
}

/// `POST /controller/v1/tasks/:task_id/runner-credentials`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerCredentialRequest {
    /// The lease epoch the controller holds.
    pub lease_epoch: u64,
    /// The desired generation the incarnation is launched for.
    pub generation: u64,
    /// The new incarnation (a Pod UID).
    pub incarnation: String,
}

/// A runner credential, shown once; the controller mounts it read-only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerCredentialResponse {
    /// The task.
    pub task_id: TaskId,
    /// Launch generation it is bound to.
    pub generation: u64,
    /// Incarnation it is bound to.
    pub incarnation: String,
    /// The bearer credential.
    pub credential: String,
    /// When it runs out unless the runner renews it.
    pub expires_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Runner side (KTD3)
// ---------------------------------------------------------------------------

/// What the control plane wants of a runner.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunnerDirective {
    /// Keep working. Also the answer while the task is Interrupted or
    /// Unknown: the control plane never stops work on its own inference —
    /// the owner's next command decides.
    Continue,
    /// Reach a durable safe point, report a snapshot with `safe_point`, and
    /// stop.
    PauseAtSafePoint,
    /// Stop now; the task is paused or being archived.
    Stop,
}

/// What the control plane knows about the launch of the calling runner.
///
/// A runner learns its own launch generation here, and — through
/// `authorized_by` — whether its owner explicitly started or resumed it.
/// That is how a runner that finds its task storage left behind by an
/// unexpected stop knows it may run ordinary recovery (R13): only an owner
/// decision taken *after* the loss launches a new incarnation at a newer
/// generation, so the decision is a fact on the control plane, never an
/// inference of the runner's.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerLaunchView {
    /// The incarnation the credential is bound to.
    pub incarnation: String,
    /// The generation it was launched at.
    pub launch_generation: u64,
    /// The owner command that produced that generation (`start` or
    /// `resume`); `None` for the first launch out of the queue.
    pub authorized_by: Option<OwnerCommand>,
}

/// `POST /runner/v1/snapshots` response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerSnapshotResponse {
    /// False for an absorbed duplicate.
    pub applied: bool,
    /// What to do next.
    pub directive: RunnerDirective,
    /// Desired generation.
    pub generation: u64,
    /// Orchestration state.
    pub lifecycle: TaskLifecycle,
    /// The calling runner's launch.
    pub launch: RunnerLaunchView,
    /// The installation's default harness as it applies to this task (U7);
    /// absent when there is none to follow. Applied before anything else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_harness: Option<TaskDefaultHarness>,
}

/// `GET /runner/v1/poll` query.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerPollQuery {
    /// The request the runner waits on right now; only answers to it are
    /// delivered, queued answers to any other become obsolete.
    #[serde(default)]
    pub pending_input_id: Option<String>,
    /// Long-poll bound; capped by the server.
    #[serde(default)]
    pub wait_secs: Option<u64>,
    /// The runner is backing off materials it could not deliver: pending
    /// materials are still listed in the answer, but they alone do not end
    /// the long-poll early (U9). Without it a material that cannot be
    /// delivered right now would turn the long-poll into a busy loop.
    #[serde(default)]
    pub defer_materials: bool,
}

/// An owner answer handed to the runner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeliveredInput {
    /// Control-plane id; acknowledge with it.
    pub input_id: String,
    /// The request it answers.
    pub pending_input_id: String,
    /// The answer (`episode` set to the request it answers).
    pub delivery: HumanInputDelivery,
}

/// An operator command handed to the runner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeliveredOperatorCommand {
    /// Control-plane id; acknowledge with it.
    pub command_id: String,
    /// The owner who sent it, for the audit the runner's log keeps.
    pub requested_by: String,
    /// What to do.
    #[serde(flatten)]
    pub command: OperatorCommand,
}

/// `GET /runner/v1/poll` response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunnerPollResponse {
    /// What to do next.
    pub directive: RunnerDirective,
    /// Desired generation.
    pub generation: u64,
    /// Orchestration state.
    pub lifecycle: TaskLifecycle,
    /// Answers to the request the runner waits on; redelivered until
    /// acknowledged.
    pub inputs: Vec<DeliveredInput>,
    /// Operator commands (U3) for this runner; redelivered until
    /// acknowledged with `POST /runner/v1/commands/:command_id/ack`.
    #[serde(default)]
    pub commands: Vec<DeliveredOperatorCommand>,
    /// When the runner's credential runs out.
    pub credential_expires_at: DateTime<Utc>,
    /// The calling runner's launch.
    pub launch: RunnerLaunchView,
    /// Conversation pages owners are waiting for right now; answer each
    /// with `POST /runner/v1/conversation-pages/:request_id`.
    #[serde(default)]
    pub conversation_pages: Vec<ConversationPageRequest>,
    /// Execution-event requests owners are waiting for right now (U4);
    /// answer each with `POST /runner/v1/execution-events/:request_id`.
    #[serde(default)]
    pub execution_events: Vec<ExecutionEventsRequest>,
    /// Conductor-state requests owners are waiting for right now (U5);
    /// answer each with `POST /runner/v1/conductor-reads/:request_id`.
    #[serde(default)]
    pub conductor_reads: Vec<ConductorReadRequest>,
    /// Owner materials not yet acknowledged (U9): fetch each manifest and
    /// its blobs, write them onto task storage, then acknowledge with
    /// `POST /runner/v1/materials/:material_id/ack`. Redelivered until
    /// acknowledged.
    #[serde(default)]
    pub materials: Vec<crate::materials::MaterialAnnouncement>,
    /// The installation's default harness as it applies to this task (U7);
    /// absent when there is none to follow. The runner applies it before
    /// the inputs and commands of the same answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_harness: Option<TaskDefaultHarness>,
}

/// An owner's request for one bounded page of one attempt's raw
/// conversation, relayed to the live runner (KTD4).
///
/// The relay keeps nothing: the request lives in memory only while the
/// owner waits, and the page the runner uploads is handed straight to that
/// owner. Raw transcripts therefore never become a central replica.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationPageRequest {
    /// Relay id; upload the page under it.
    pub request_id: String,
    /// The attempt whose conversation is asked for.
    pub attempt_id: String,
    /// First entry number wanted.
    pub from_entry: u64,
    /// Soft cap on the raw bytes of the page.
    pub byte_budget: u64,
}

/// One page of an attempt's raw conversation — the shape the local API
/// serves, so a client renders a remote page exactly like a local one.
pub type ConversationPage = crate::api::ConversationResponse;

/// `GET /control/v1/tasks/:task_id/attempts/:attempt_id/conversation` query.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationPageQuery {
    /// First entry number wanted (default 0: from the start).
    #[serde(default)]
    pub from: u64,
    /// Soft cap on the raw bytes; bounded by the server.
    #[serde(default)]
    pub byte_budget: Option<u64>,
}

/// An owner's request for the execution's raw events after a sequence,
/// relayed to the live runner the way a conversation page is (KTD4): in
/// memory only, answered straight to the waiting owner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionEventsRequest {
    /// Relay id; upload the page under it.
    pub request_id: String,
    /// Events with a greater `sequence` are wanted.
    pub after: u64,
    /// Most events to return.
    pub limit: u32,
}

/// One bounded run of the execution's events, in sequence order — what the
/// runner uploads for an [`ExecutionEventsRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionEventsPage {
    /// The events, oldest first.
    pub events: Vec<Event>,
    /// True when more events follow the last one returned.
    pub has_more: bool,
}

/// `POST /runner/v1/relay-failures/:request_id` — the runner could not read
/// what a waiting owner asked for. The owner is answered "unavailable" at
/// once instead of after the relay's timeout, and the request leaves the
/// poll, which would otherwise offer it again and again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayFailure {
    /// Why, for the control plane's log; never shown to the owner.
    pub reason: String,
}

/// `GET /control/v1/tasks/:task_id/execution-events` query.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionEventsQuery {
    /// Events with a greater `sequence` (default 0: from the start).
    #[serde(default)]
    pub after: u64,
    /// Most events to return; bounded by the server.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Where an events answer was read from.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionEventsSource {
    /// The live runner, through the relay.
    Runner,
    /// The task's own storage, by the daemon that owns it (local backend
    /// without a live runner).
    Storage,
}

/// `GET /control/v1/tasks/:task_id/execution-events`.
///
/// A task whose runner is not running is a normal answer, not a failure:
/// `Unavailable` says why. Only a runner that should answer and does not is
/// an error (`503`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum ExecutionEventsResponse {
    /// The events after the requested sequence.
    Available {
        /// The events, oldest first.
        events: Vec<Event>,
        /// True when more follow; ask again `after` the last one.
        has_more: bool,
        /// Where they were read from.
        source: ExecutionEventsSource,
    },
    /// Not readable right now.
    Unavailable {
        /// Why, in the conversation's vocabulary.
        reason: ConversationUnavailableReason,
        /// The same, for people.
        message: String,
    },
}

/// An owner's request for the state of the task's conductor, relayed to the
/// live runner like an events page (KTD4): in memory only, answered
/// straight to the waiting owner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConductorReadRequest {
    /// Relay id; upload the view under it.
    pub request_id: String,
    /// Chat entries with a greater `sequence` are wanted.
    pub after: u64,
    /// Most chat entries to return.
    pub limit: u32,
}

/// `GET /control/v1/tasks/:task_id/conductor` query.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConductorReadQuery {
    /// Chat entries with a greater `sequence` (default 0: from the start).
    #[serde(default)]
    pub after: u64,
    /// Most chat entries to return; bounded by the server.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// `GET /control/v1/tasks/:task_id/conductor`.
///
/// Answered like the execution events: by the live runner through the
/// relay, or — for the daemon's local backend, while its runner is not
/// running — from the task's own storage, with the conductor marked
/// unavailable. A task whose storage this server cannot read says why.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum ConductorReadResponse {
    /// The conductor's state.
    Available {
        /// What there is to read.
        conductor: Box<ConductorView>,
        /// Where it was read from.
        source: ExecutionEventsSource,
    },
    /// Not readable right now.
    Unavailable {
        /// Why, in the conversation's vocabulary.
        reason: ConversationUnavailableReason,
        /// The same, for people.
        message: String,
    },
}

/// `POST /runner/v1/conversation-pages/:request_id` response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationPageAccepted {
    /// False when no owner waits for that request any more (it timed out
    /// or was answered already); the runner simply moves on.
    pub delivered: bool,
}

/// `POST /runner/v1/renew` response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerRenewResponse {
    /// The credential's new expiry.
    pub expires_at: DateTime<Utc>,
}

/// `POST /runner/v1/inputs/:input_id/ack`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputAckRequest {
    /// Whether the runner's engine accepted the answer.
    pub accepted: bool,
    /// Its reason when it did not.
    #[serde(default)]
    pub detail: Option<String>,
}

/// `POST /runner/v1/inputs/:input_id/ack` response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputAckResponse {
    /// The input afterwards.
    pub input: InputView,
}

/// `POST /runner/v1/commands/:command_id/ack`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperatorCommandAckRequest {
    /// Whether the runner carried the command out.
    pub applied: bool,
    /// Its reason when it did not.
    #[serde(default)]
    pub detail: Option<String>,
}

/// `POST /runner/v1/commands/:command_id/ack` response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OperatorCommandAckResponse {
    /// The command afterwards.
    pub command: OperatorCommandView,
}

/// The snapshot body of `POST /runner/v1/snapshots`.
pub type RunnerSnapshotRequest = RunnerSnapshot;

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

/// One declared input of a workflow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowInputView {
    /// The key the value is given under.
    pub name: String,
    /// Declared type (`type` on the wire).
    #[serde(rename = "type")]
    pub input_type: crate::domain::ParamType,
    /// Whether a task cannot be created without it.
    pub required: bool,
    /// What it is for, when the workflow says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The value used when none is given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
}

/// Where the copy of a workflow that wins comes from.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowSource {
    /// Compiled into the server's binary.
    BuiltIn,
    /// The catalogue's own `workflows/` directory.
    Catalog,
    /// The server user's own workflow directory.
    User,
}

/// One workflow a task can be created from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowView {
    /// The reference to create a task with (`launch.workflow_id`).
    pub name: String,
    /// Where it comes from; never a file path.
    pub source: WorkflowSource,
    /// What it does, on one line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The version to create a task with (`launch.workflow_version`);
    /// absent when the definition does not load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// Declared inputs, in declaration order; empty when it does not load.
    pub inputs: Vec<WorkflowInputView>,
    /// Whether a task can be created from it right now.
    pub valid: bool,
    /// Every violation when it is not valid.
    #[serde(default)]
    pub violations: Vec<String>,
    /// Whether it hides another copy of the same name.
    pub overrides: bool,
}

/// `GET /control/v1/workflows`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowListResponse {
    /// Built-in and catalogue workflows, ordered by name.
    pub workflows: Vec<WorkflowView>,
}

/// `POST /control/v1/workflows/validate`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValidateWorkflowRequest {
    /// A workflow file on the server's disk.
    pub path: String,
}

/// The verdict of a validation: every violation at once, like
/// `narsor validate`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ValidateWorkflowResponse {
    /// No violation and the definition loads.
    pub valid: bool,
    /// The definition's name, when it loads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Its version, when it loads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// Its declared inputs, when it loads.
    #[serde(default)]
    pub inputs: Vec<WorkflowInputView>,
    /// Every violation (or the reason the file cannot be read as a
    /// definition).
    #[serde(default)]
    pub violations: Vec<String>,
}

/// One registered repository.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepositoryView {
    /// The id a workflow names it by.
    pub id: String,
    /// Display name.
    pub name: String,
    /// `github`, `gitlab`, `local`, …
    pub provider: String,
    /// Where it is hosted.
    pub remote: String,
    /// Branch used when a workflow declares no `ref`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    /// Free-form note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// `GET /control/v1/repositories`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepositoryListResponse {
    /// Registered repositories, ordered by id.
    pub repositories: Vec<RepositoryView>,
    /// Registry files that could not be read, one line each (file name and
    /// reason): the list never hides the entries that did load.
    #[serde(default)]
    pub unreadable: Vec<String>,
}

/// `POST /control/v1/repositories`: the parts `narsor repo add` takes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CreateRepositoryRequest {
    /// The id a workflow will name it by.
    pub id: String,
    /// Anything `git clone` accepts, checked like the provisioner does.
    pub remote: String,
    /// Display name; the id when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Provider; inferred from the remote when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Branch used when a workflow declares no `ref`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    /// Free-form note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// `GET /control/v1/capabilities`: what this server offers, so a client
/// greys out what it cannot do instead of failing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilitiesView {
    /// What to call this connection in a client's header.
    pub connection_name: String,
    /// Whether `POST /control/v1/workflows/validate` (and so a workflow
    /// *path* for a new task) is accepted: only a trusted local daemon
    /// reads files from its own disk for its owner.
    pub workflow_paths: bool,
    /// Why not, when it is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_paths_reason: Option<String>,
    /// Whether executions from before the task scheduler exist here
    /// (`/control/v1/executions`).
    pub legacy_executions: bool,
    /// Whether a repository may be registered by a path on the server's
    /// disk (`/srv/x`, `file://…`). Only a trusted local daemon accepts
    /// one; any other server takes remote git URLs only (https, ssh,
    /// `user@host:path`).
    pub local_repository_paths: bool,
}

/// An execution from before the task scheduler, as the Historia shows it.
///
/// Texts are made terminal-safe, and nothing of the server's disk (the
/// definition path in the execution's metadata, its variables) is part of
/// it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LegacyExecutionView {
    /// The execution's id.
    pub id: String,
    /// The workflow it is an execution of.
    pub workflow_id: String,
    /// The version it was started with.
    pub workflow_version: u32,
    /// Its status.
    pub status: ExecutionStatus,
    /// The state it stopped at, if any.
    #[serde(default)]
    pub current_state: Option<String>,
    /// When it started.
    pub started_at: DateTime<Utc>,
    /// When it finished, if it has.
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
    /// What it is stopped on, if it is stopped on a problem.
    #[serde(default)]
    pub problem: Option<crate::api::ActiveProblem>,
    /// Whether `POST …/recover` can start it again right now.
    pub recoverable: bool,
    /// Why not, when it cannot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_recoverable_reason: Option<String>,
}

/// `GET /control/v1/executions`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LegacyExecutionListResponse {
    /// Newest first; empty when there are none (or this is not a local
    /// daemon).
    pub executions: Vec<LegacyExecutionView>,
}
