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

use crate::domain::{BackendId, ExecutionStatus, ResourceProfile, TaskId, TaskLaunchSpec};
use crate::engine::{HumanInputDelivery, Timeline};
use crate::orchestration::{
    ArchiveTombstone, CommandReceipt, ConversationAttemptSummary, ExecutionCursor, InputStatus,
    Observation, ObservedSummary, OwnerCommand, PendingInputRef, QueuedInput, ResourceIdentity,
    RunnerProblem, RunnerSnapshot, TaskEvent, TaskLifecycle,
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
    /// Harness for agent steps that name none (R42); the profile's, else the
    /// live default, when absent.
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
    /// Client-chosen id; repeating the same request with it is idempotent
    /// while the configuration it resolves against is unchanged.
    /// Generated when absent — but then a retry creates a second task.
    #[serde(default)]
    pub task_id: Option<String>,
    /// Backend to pin the task to (R10); the owner's `default_backend` when
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_id: Option<String>,
    /// Queue priority (R11), higher first.
    #[serde(default)]
    pub priority: i32,
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
    /// Queue priority.
    pub priority: i32,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// FIFO position.
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
    /// The control plane's "now" this summary was computed at, so a client
    /// can show the age of every timestamp above.
    pub as_of: DateTime<Utc>,
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
    /// Queued: take the start over. **Report `StartAccepted` and wait for
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
    /// When the runner's credential runs out.
    pub credential_expires_at: DateTime<Utc>,
    /// The calling runner's launch.
    pub launch: RunnerLaunchView,
    /// Conversation pages owners are waiting for right now; answer each
    /// with `POST /runner/v1/conversation-pages/:request_id`.
    #[serde(default)]
    pub conversation_pages: Vec<ConversationPageRequest>,
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
