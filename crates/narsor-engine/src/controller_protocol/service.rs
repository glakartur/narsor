//! The protocol's server-side handling, independent of HTTP.
//!
//! [`ProtocolService`] is the one door owners, controllers and runners go
//! through to the [`ControlPlane`]: it scopes every request to the caller
//! (an owner sees only its tasks, a controller only its backend's, a runner
//! only its own incarnation's), turns records into the normalized wire
//! views, and announces every accepted mutation on an in-process channel —
//! which is what wakes long-polls and live event streams.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, watch};

mod auto_resume;
mod conductor;
mod conversation;
mod events;
mod materials;
mod relay;

use super::credentials::{
    ControllerAuthenticator, ControllerIdentity, ControllerRecord, CredentialStore,
    RunnerAuthenticator, RunnerIdentity,
};
use super::error::{ProtocolError, ProtocolResult};
use super::wire::*;
use crate::domain::{
    AnsweredBy, BackendId, LaunchDefinitionError, ResourceProfile, TaskDisplay, TaskId,
};
use crate::engine::{
    plan_operator_transition, EngineError, HumanInputDelivery, PositionStatus, Timeline,
};
use crate::environment::{LaunchError, LaunchIntent, LaunchResolver};
use crate::materials::{MaterialLimits, MaterialStore};
use crate::orchestration::{
    AttentionItem, AttentionKind, BackendCapacity, BackendLease, CommandActor, CommandRequest,
    ControlPlane, ControllerReport, ConversationAvailability, InputRequest, InputStatus,
    LeaseToken, MoveTarget, NewTask, Observation, OperatorCommand, OperatorCommandRequest,
    OwnerRef, ResumePolicy, RunnerObservation, RunnerState, TaskEvent, TaskRecord,
    RETIRED_CONDUCTOR_HARNESS,
};
use crate::workflow::TemplateScope;
use events::LocalTaskStorage;
use relay::Relay;

pub use auto_resume::AutoResumePass;

fn engine_delivery(delivery: narsor_protocol::api::HumanInputDeliveryDto) -> HumanInputDelivery {
    let permission_decision =
        delivery
            .permission_decision
            .map(|decision| crate::engine::actions::PermissionDecision {
                denial_index: decision.denial_index,
                decision: match decision.decision {
                    narsor_protocol::api::PermissionDecisionKindDto::Deny => {
                        crate::engine::actions::PermissionDecisionKind::Deny
                    }
                    narsor_protocol::api::PermissionDecisionKindDto::Allow => {
                        crate::engine::actions::PermissionDecisionKind::Allow
                    }
                },
                scope: decision.scope.map(|scope| match scope {
                    narsor_protocol::api::PermissionGrantScopeDto::ExactOperation => {
                        crate::engine::actions::PermissionGrantScope::ExactOperation
                    }
                    narsor_protocol::api::PermissionGrantScopeDto::CommandGroup => {
                        crate::engine::actions::PermissionGrantScope::CommandGroup
                    }
                    narsor_protocol::api::PermissionGrantScopeDto::Pattern => {
                        crate::engine::actions::PermissionGrantScope::Pattern
                    }
                }),
                pattern: decision.pattern,
                message: decision.message,
            });
    HumanInputDelivery {
        values: delivery.values,
        chosen_label: delivery.chosen_label,
        data: delivery.data,
        variables: delivery.variables,
        comment: delivery.comment,
        agent_message: delivery.agent_message,
        action: delivery.action,
        episode: delivery.episode,
        permission_decision,
        answered_by: AnsweredBy::Operator,
    }
}

fn protocol_delivery(delivery: &HumanInputDelivery) -> narsor_protocol::api::HumanInputDeliveryDto {
    narsor_protocol::api::HumanInputDeliveryDto {
        values: delivery.values.clone(),
        chosen_label: delivery.chosen_label.clone(),
        data: delivery.data.clone(),
        variables: delivery.variables.clone(),
        comment: delivery.comment.clone(),
        agent_message: delivery.agent_message.clone(),
        action: delivery.action,
        episode: delivery.episode.clone(),
        permission_decision: delivery.permission_decision.as_ref().map(|decision| {
            narsor_protocol::api::PermissionDecisionDto {
                denial_index: decision.denial_index,
                decision: match decision.decision {
                    crate::engine::actions::PermissionDecisionKind::Deny => {
                        narsor_protocol::api::PermissionDecisionKindDto::Deny
                    }
                    crate::engine::actions::PermissionDecisionKind::Allow => {
                        narsor_protocol::api::PermissionDecisionKindDto::Allow
                    }
                },
                scope: decision.scope.map(|scope| match scope {
                    crate::engine::actions::PermissionGrantScope::ExactOperation => {
                        narsor_protocol::api::PermissionGrantScopeDto::ExactOperation
                    }
                    crate::engine::actions::PermissionGrantScope::CommandGroup => {
                        narsor_protocol::api::PermissionGrantScopeDto::CommandGroup
                    }
                    crate::engine::actions::PermissionGrantScope::Pattern => {
                        narsor_protocol::api::PermissionGrantScopeDto::Pattern
                    }
                }),
                pattern: decision.pattern.clone(),
                message: decision.message.clone(),
            }
        }),
    }
}

fn pending_input_view(
    pending: &crate::orchestration::PendingInputRef,
) -> ProtocolResult<PendingInputRefView> {
    let form = serde_json::to_value(&pending.form)
        .map_err(|error| ProtocolError::Internal(error.to_string()))?;
    Ok(PendingInputRefView {
        id: pending.id.clone(),
        attempt_id: pending.attempt_id.clone(),
        form,
    })
}

/// A launch that did not resolve, as the owner sees it: a `400` naming the
/// reason, or a bare `500` when the catalogue could not even be read.
fn launch_refusal(error: LaunchError) -> ProtocolError {
    if error.is_internal() {
        ProtocolError::Internal(error.to_string())
    } else {
        ProtocolError::bad_request(error.code(), error.to_string())
    }
}

/// When the owner archived the task, or created it when no tombstone
/// exists yet — the order of the Historia.
fn archived_at(record: &TaskRecord) -> DateTime<Utc> {
    record
        .archive
        .as_ref()
        .map_or(record.created_at, |tombstone| tombstone.requested_at)
}

/// The owner every request authenticated by the local daemon token acts
/// as. Trusted loopback mode has exactly one owner; network mode uses the
/// verified OIDC `(issuer, subject)` instead
/// ([`crate::auth::OidcIdentity::owner_ref`]).
pub const LOCAL_OWNER: &str = "local";

/// How many change notifications may queue for a slow listener before it
/// lags — a lagging listener rescans instead of losing anything.
const CHANGE_CHANNEL_CAPACITY: usize = 256;

/// Longest incarnation identifier accepted (a Pod UID is 36 characters).
const MAX_INCARNATION_LEN: usize = 256;

/// Raw bytes a relayed conversation page holds when the owner asks for no
/// particular budget, and the most it may ask for. Smaller than the local
/// API's budget: every page crosses two network hops.
const DEFAULT_PAGE_BUDGET: u64 = 256 * 1024;
const MAX_PAGE_BUDGET: u64 = 1024 * 1024;

/// Events a relayed or stored read returns when the owner asks for no
/// particular number, and the most it may ask for.
const DEFAULT_EVENTS_LIMIT: u32 = 500;
const MAX_EVENTS_LIMIT: u32 = 2000;

/// Conversation page requests owners are waiting on (KTD4's relay).
type PageRelay = Relay<ConversationPageRequest, ConversationPage>;

/// Execution event requests owners are waiting on (U4), relayed the same way.
type EventsRelay = Relay<ExecutionEventsRequest, ExecutionEventsPage>;

/// Conductor-state requests owners are waiting on (U5), relayed the same way.
type ConductorRelay = Relay<ConductorReadRequest, crate::conductor::ConductorView>;

/// Timing policy of the protocol.
#[derive(Debug, Clone)]
pub struct ProtocolConfig {
    /// Lease validity when the controller does not ask for one.
    pub default_lease_ttl: Duration,
    /// Longest lease validity granted.
    pub max_lease_ttl: Duration,
    /// Validity of a runner credential, from issue or renewal.
    pub runner_credential_ttl: Duration,
    /// Enrollment code validity when the operator does not ask for one.
    pub default_enrollment_ttl: Duration,
    /// Longest enrollment code validity granted.
    pub max_enrollment_ttl: Duration,
    /// A runner not heard from for longer is shown stale.
    pub runner_stale_after: Duration,
    /// A controller without a heartbeat for longer is shown stale.
    pub controller_stale_after: Duration,
    /// Longest long-poll a controller or runner may ask for.
    pub max_long_poll: std::time::Duration,
    /// How long an owner waits for the live runner to upload a requested
    /// conversation page or events page before the relay gives up.
    pub conversation_page_timeout: std::time::Duration,
    /// How workload losses are counted for the automatic resume: the
    /// attempts' delays (their number is the limit) and the stability
    /// window. Installed on the service's control plane.
    pub resume: ResumePolicy,
}

impl Default for ProtocolConfig {
    fn default() -> Self {
        Self {
            default_lease_ttl: Duration::seconds(60),
            max_lease_ttl: Duration::minutes(10),
            runner_credential_ttl: Duration::minutes(15),
            default_enrollment_ttl: Duration::minutes(15),
            max_enrollment_ttl: Duration::hours(24),
            runner_stale_after: Duration::seconds(90),
            controller_stale_after: Duration::seconds(90),
            max_long_poll: std::time::Duration::from_secs(30),
            conversation_page_timeout: std::time::Duration::from_secs(20),
            resume: ResumePolicy::default(),
        }
    }
}

/// Server-side protocol handling over one [`ControlPlane`].
pub struct ProtocolService {
    control: ControlPlane,
    credentials: CredentialStore,
    controller_auth: Arc<dyn ControllerAuthenticator>,
    runner_auth: Arc<dyn RunnerAuthenticator>,
    config: ProtocolConfig,
    changes: broadcast::Sender<TaskId>,
    launch: Arc<dyn LaunchResolver>,
    pages: Mutex<PageRelay>,
    event_reads: Mutex<EventsRelay>,
    conductor_reads: Mutex<ConductorRelay>,
    local_storage: Option<LocalTaskStorage>,
    /// Closed attempts' summaries of the local tasks read from storage.
    stored_conversations: Mutex<conversation::StoredSummaries>,
    materials: MaterialStore,
}

impl std::fmt::Debug for ProtocolService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProtocolService")
            .field("control", &self.control)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl ProtocolService {
    /// A service with the default policy and file-backed bearer credentials.
    ///
    /// `launch` turns every create request into a backend and a sealed
    /// launch spec (U6); there is no default, because a control plane that
    /// accepted unresolved launches would pin tasks to nothing.
    pub fn new(control: ControlPlane, launch: Arc<dyn LaunchResolver>) -> Self {
        Self::with_config(control, launch, ProtocolConfig::default())
    }

    /// A service with an explicit policy.
    ///
    /// The config's [`ProtocolConfig::resume`] replaces the control plane's
    /// own resume policy, so the service and every report it accepts count
    /// losses alike.
    pub fn with_config(
        control: ControlPlane,
        launch: Arc<dyn LaunchResolver>,
        config: ProtocolConfig,
    ) -> Self {
        let control = control.with_resume_policy(config.resume.clone());
        let credentials = CredentialStore::new(control.clone());
        let (changes, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
        let materials = MaterialStore::new(control.store().paths(), MaterialLimits::default());
        Self {
            materials,
            controller_auth: Arc::new(credentials.clone()),
            runner_auth: Arc::new(credentials.clone()),
            control,
            credentials,
            config,
            changes,
            launch,
            pages: Mutex::new(PageRelay::default()),
            event_reads: Mutex::new(EventsRelay::default()),
            conductor_reads: Mutex::new(ConductorRelay::default()),
            local_storage: None,
            stored_conversations: Mutex::default(),
        }
    }

    /// Tells the service where a backend's task storages live on this host,
    /// so a task of that backend whose runner is not running can still be
    /// read from its own storage (U4). Only the daemon that owns the local
    /// backend knows; a service without it answers such reads "unavailable".
    pub fn with_local_storage(mut self, backend_id: BackendId, root: std::path::PathBuf) -> Self {
        self.local_storage = Some(LocalTaskStorage { backend_id, root });
        self
    }

    /// Replaces how controller requests are verified (the default checks
    /// signed proof-of-possession, [`crate::auth::pop`]).
    pub fn with_controller_authenticator(mut self, auth: Arc<dyn ControllerAuthenticator>) -> Self {
        self.controller_auth = auth;
        self
    }

    /// Replaces the bounds of task materials (U9).
    pub fn with_material_limits(mut self, limits: MaterialLimits) -> Self {
        self.materials = MaterialStore::new(self.control.store().paths(), limits);
        self
    }

    /// Replaces how runner credentials are verified.
    pub fn with_runner_authenticator(mut self, auth: Arc<dyn RunnerAuthenticator>) -> Self {
        self.runner_auth = auth;
        self
    }

    /// The control plane.
    pub fn control_plane(&self) -> &ControlPlane {
        &self.control
    }

    /// The credential store.
    pub fn credentials(&self) -> &CredentialStore {
        &self.credentials
    }

    /// The policy in force.
    pub fn config(&self) -> &ProtocolConfig {
        &self.config
    }

    /// Change notifications: the id of every task whose record changed.
    ///
    /// A notification carries no event — listeners re-read the durable log
    /// from their own cursor, so a lagged or duplicated notification can
    /// never make them skip or repeat one.
    pub fn subscribe(&self) -> broadcast::Receiver<TaskId> {
        self.changes.subscribe()
    }

    /// The installation's default harness as it applies to the task of
    /// `record`, for its runner; `None` when there is none to follow.
    fn default_harness_of(&self, record: &TaskRecord) -> Option<TaskDefaultHarness> {
        self.launch.default_harness(&record.assignment.backend_id)
    }

    /// Announces that `task_id`'s record changed. Public so an in-process
    /// writer that mutates the [`ControlPlane`] directly (U4's local
    /// adapter) wakes streams and long-polls too.
    pub fn notify(&self, task_id: &TaskId) {
        // No receiver is not an error: nobody is listening right now.
        let _ = self.changes.send(task_id.clone());
    }

    /// The controller a signed request proves, if any.
    pub fn authenticate_controller(
        &self,
        request: &crate::auth::pop::SignedRequest<'_>,
    ) -> Option<ControllerIdentity> {
        self.controller_auth.authenticate_controller(request)
    }

    /// The runner a bearer token proves, if any.
    pub fn authenticate_runner(&self, bearer: &str) -> Option<RunnerIdentity> {
        self.runner_auth.authenticate_runner(bearer)
    }

    // -----------------------------------------------------------------
    // Owner side
    // -----------------------------------------------------------------

    /// Creates a queued task for `owner`, idempotently by task id.
    ///
    /// An id already taken by another owner's task is answered `NotFound`,
    /// exactly like every other request about that task: a conflict would
    /// confirm to this owner that the other task exists.
    pub fn create_task(
        &self,
        owner: &OwnerRef,
        request: CreateTaskRequest,
    ) -> ProtocolResult<TaskSummary> {
        let request_digest = request_digest(&request)?;
        let task_id = match request.task_id {
            Some(task_id) => parse_identifier(TaskId::parse(task_id))?,
            None => TaskId::parse(format!("task-{}", uuid::Uuid::new_v4().simple()))
                .expect("a generated task id is always valid"),
        };
        match self.control.task(&task_id) {
            Ok(existing) if existing.owner != *owner => return Err(ProtocolError::NotFound),
            // A retry of the identical request: the task it created, even if
            // the configuration it resolved against — the default backend, a
            // limit, an image — changed since. Resolving again could only
            // produce a spurious conflict (U10).
            Ok(existing) if existing.request_digest.as_deref() == Some(&request_digest) => {
                return self.summarize(&existing);
            }
            Ok(_) | Err(crate::orchestration::OrchestrationError::TaskNotFound(_)) => {}
            Err(error) => return Err(error.into()),
        }
        // The owner's durable default backend (R10) is read here, once, for
        // a request that names none; the configured one applies when the
        // owner never chose. The task is pinned to the result for good.
        let backend = match request.backend_id {
            Some(backend) => Some(backend),
            None => self
                .control
                .default_backend()?
                .and_then(|selection| selection.backend)
                .map(|backend| backend.to_string()),
        };
        // Resolution reads configuration only; a refusal here returns before
        // the control plane is asked to store anything, so an invalid backend
        // limit, an unregistered secret, an unsupported harness or a missing
        // image leaves no task and no assignment behind (U6).
        let launch = request.launch;
        let workspace = launch.workspace.unwrap_or_default();
        let resolved = self
            .launch
            .resolve(&LaunchIntent {
                backend,
                workflow_id: launch.workflow_id,
                workflow_version: launch.workflow_version,
                inputs: launch.inputs,
                repository: workspace.repository,
                commit: workspace.commit,
                profile: launch.profile,
                harness: launch.harness,
                resources: launch.resources.map(|resources| {
                    ResourceProfile::new(resources.cpu_millicores, resources.memory_mib)
                }),
            })
            .map_err(launch_refusal)?;
        self.create_sealed_task(owner, task_id, request_digest, resolved)
    }

    /// Persists an already-resolved task through the same idempotent
    /// admission path as [`Self::create_task`].
    ///
    /// The local API resolves workflow references before they become a
    /// generic control-plane request: local file paths are meaningful to the
    /// daemon, but must never be stored as remote launch input.  The caller
    /// supplies the digest of its stable submission envelope, while this
    /// method retains all task-id ownership, replay, queueing, and scheduler
    /// semantics in one place.
    pub fn create_sealed_task(
        &self,
        owner: &OwnerRef,
        task_id: TaskId,
        request_digest: String,
        resolved: crate::environment::ResolvedLaunch,
    ) -> ProtocolResult<TaskSummary> {
        match self.control.task(&task_id) {
            Ok(existing) if existing.owner != *owner => return Err(ProtocolError::NotFound),
            Ok(existing) if existing.request_digest.as_deref() == Some(&request_digest) => {
                return self.summarize(&existing);
            }
            Ok(_) | Err(crate::orchestration::OrchestrationError::TaskNotFound(_)) => {}
            Err(error) => return Err(error.into()),
        }
        let record = self
            .control
            .create_task(
                NewTask::new(
                    task_id.clone(),
                    owner.clone(),
                    resolved.backend_id,
                    resolved.spec,
                )
                .with_request_digest(request_digest),
            )
            .map_err(|error| match error {
                // Created by another owner between the check above and now.
                crate::orchestration::OrchestrationError::TaskConflict(_)
                    if self.someone_else_owns(owner, &task_id).unwrap_or(true) =>
                {
                    ProtocolError::NotFound
                }
                other => other.into(),
            })?;
        self.notify(&record.task_id);
        self.reschedule(&record.assignment.backend_id);
        self.owned_summary(owner, &record.task_id)
    }

    /// Returns an existing sealed submission before a local caller performs
    /// any current workflow or environment resolution.  The submission
    /// envelope, not today's catalogue, defines replay identity.
    pub fn replay_sealed_task(
        &self,
        owner: &OwnerRef,
        task_id: &TaskId,
        request_digest: &str,
    ) -> ProtocolResult<Option<TaskSummary>> {
        match self.control.task(task_id) {
            Ok(existing) if existing.owner != *owner => Err(ProtocolError::NotFound),
            Ok(existing) if existing.request_digest.as_deref() == Some(request_digest) => {
                self.summarize(&existing).map(Some)
            }
            Ok(existing) => Err(ProtocolError::conflict(
                "task_conflict",
                format!(
                    "task '{}' already exists with a different submission",
                    existing.task_id
                ),
            )),
            Err(crate::orchestration::OrchestrationError::TaskNotFound(_)) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// The owner's tasks and the stream cursor at exactly that state.
    pub fn list_tasks(&self, owner: &OwnerRef) -> ProtocolResult<TaskListResponse> {
        self.list_tasks_where(owner, None)
    }

    /// The owner's tasks, narrowed by `archived` (see [`TaskListQuery`]).
    ///
    /// The cursor is the stream's position over *all* the owner's tasks,
    /// whatever the filter: a client that opens the stream with it misses
    /// nothing, and a task that archives later simply arrives as an event.
    /// Archived tasks come newest archive first (the Historia); the others
    /// keep the control plane's own order.
    pub fn list_tasks_where(
        &self,
        owner: &OwnerRef,
        archived: Option<bool>,
    ) -> ProtocolResult<TaskListResponse> {
        let mut cursor = StreamCursor::default();
        let mut kept = Vec::new();
        for record in self.control.list_tasks()? {
            if record.owner != *owner {
                continue;
            }
            cursor.advance(&record.task_id, record.revision);
            if archived.is_none_or(|wanted| wanted == record.runner_state.is_archival()) {
                kept.push(record);
            }
        }
        if archived == Some(true) {
            kept.sort_by(|a, b| {
                archived_at(b)
                    .cmp(&archived_at(a))
                    .then_with(|| b.task_id.as_str().cmp(a.task_id.as_str()))
            });
        }
        let now = self.control.now();
        let mut presences: HashMap<&BackendId, Option<ControllerPresence>> = HashMap::new();
        let mut tasks = Vec::with_capacity(kept.len());
        for record in &kept {
            let backend_id = &record.assignment.backend_id;
            let controller = match presences.get(backend_id) {
                Some(controller) => controller.clone(),
                None => {
                    let controller = self.controller_presence(backend_id, now)?;
                    presences.insert(backend_id, controller.clone());
                    controller
                }
            };
            tasks.push(self.summarize_with(record, now, controller)?);
        }
        Ok(TaskListResponse {
            tasks,
            cursor: cursor.to_string(),
        })
    }

    /// One of the owner's tasks.
    pub fn task(&self, owner: &OwnerRef, task_id: &str) -> ProtocolResult<TaskSummary> {
        let record = self.owned_task(owner, task_id)?;
        self.summarize(&record)
    }

    /// Applies an owner command.
    pub fn command(
        &self,
        owner: &OwnerRef,
        task_id: &str,
        request: OwnerCommandRequest,
    ) -> ProtocolResult<OwnerCommandResponse> {
        let record = self.owned_task(owner, task_id)?;
        if request.idempotency_key.trim().is_empty() {
            return Err(ProtocolError::bad_request(
                "missing_idempotency_key",
                "an owner command needs an idempotency key",
            ));
        }
        if request
            .idempotency_key
            .starts_with(crate::orchestration::AUTO_RESUME_KEY_PREFIX)
        {
            return Err(ProtocolError::bad_request(
                "reserved_idempotency_key",
                format!(
                    "idempotency keys starting with `{}` are reserved for the resume policy",
                    crate::orchestration::AUTO_RESUME_KEY_PREFIX
                ),
            ));
        }
        let capacity = self.capacity(&record.assignment.backend_id);
        let command = request.command;
        let outcome = self.control.command_with_capacity(
            &record.task_id,
            CommandRequest {
                command,
                idempotency_key: request.idempotency_key,
                expected_generation: request.expected_generation,
                actor: CommandActor::Owner,
            },
            &capacity,
        )?;
        if command == crate::orchestration::OwnerCommand::Archive {
            // Archive removes the task's materials too (U9) — also on a
            // replay, which finishes a purge a crash may have cut short.
            self.purge_materials(&record.task_id);
        }
        let mut task = outcome.task;
        if !outcome.replayed {
            self.notify(&record.task_id);
            // An archive releases a slot (or a queued task's place).
            self.reschedule(&record.assignment.backend_id);
            task = self.control.task(&record.task_id)?;
        }
        Ok(OwnerCommandResponse {
            task: self.summarize(&task)?,
            receipt: outcome.receipt,
            replayed: outcome.replayed,
        })
    }

    /// Moves a queued task within its backend's queue and re-evaluates
    /// the queue (R1). Never stops or reorders anything holding compute.
    pub fn move_task(
        &self,
        owner: &OwnerRef,
        task_id: &str,
        request: MoveTaskRequest,
    ) -> ProtocolResult<TaskSummary> {
        let record = self.owned_task(owner, task_id)?;
        let target = match request.target {
            MoveTaskTarget::Before(anchor) => {
                let anchor = TaskId::parse(anchor).map_err(|error| {
                    ProtocolError::bad_request("invalid_task_id", error.to_string())
                })?;
                // An anchor of another owner is refused like one that does
                // not exist: its existence is not the caller's to learn.
                if self.owned_task(owner, anchor.as_str()).is_err() {
                    return Err(
                        crate::orchestration::OrchestrationError::InvalidMoveAnchor {
                            task_id: record.task_id.clone(),
                            anchor,
                            reason: "the anchor is not one of your tasks",
                        }
                        .into(),
                    );
                }
                MoveTarget::Before(anchor)
            }
            MoveTaskTarget::After(anchor) => {
                let anchor = TaskId::parse(anchor).map_err(|error| {
                    ProtocolError::bad_request("invalid_task_id", error.to_string())
                })?;
                // Keep the same owner-scoped indistinguishability as Before.
                if self.owned_task(owner, anchor.as_str()).is_err() {
                    return Err(
                        crate::orchestration::OrchestrationError::InvalidMoveAnchor {
                            task_id: record.task_id.clone(),
                            anchor,
                            reason: "the anchor is not one of your tasks",
                        }
                        .into(),
                    );
                }
                MoveTarget::After(anchor)
            }
            MoveTaskTarget::Top => MoveTarget::Top,
            MoveTaskTarget::Bottom => MoveTarget::Bottom,
        };
        let (_, changed) =
            self.control
                .move_task(&record.task_id, &target, request.expected_generation)?;
        if changed {
            self.notify(&record.task_id);
            self.reschedule(&record.assignment.backend_id);
        }
        self.owned_summary(owner, &record.task_id)
    }

    /// The default backend of new tasks (R10).
    pub fn default_backend(&self) -> ProtocolResult<DefaultBackendView> {
        let selection = self.control.default_backend()?;
        let configured = self.launch.configured_default_backend();
        let override_backend_id = selection
            .as_ref()
            .and_then(|selection| selection.backend.clone());
        Ok(DefaultBackendView {
            effective: override_backend_id
                .as_ref()
                .map_or_else(|| configured.clone(), ToString::to_string),
            configured,
            override_backend: override_backend_id.map(|backend| backend.to_string()),
            revision: selection.as_ref().map_or(0, |selection| selection.revision),
            updated_at: selection.and_then(|selection| selection.updated_at),
        })
    }

    /// Durably chooses the default backend of new tasks, or clears the
    /// choice (R10). The backend must be registered and usable now; tasks
    /// that exist keep their backend.
    pub fn set_default_backend(
        &self,
        request: SetDefaultBackendRequest,
    ) -> ProtocolResult<DefaultBackendView> {
        let backend = match request.backend_id {
            Some(backend) => {
                let backend = parse_identifier(BackendId::parse(backend))?;
                self.launch
                    .backend_limit(&backend)
                    .map_err(launch_refusal)?;
                Some(backend)
            }
            None => None,
        };
        self.control.set_default_backend(backend)?;
        // Moves nothing — queued tasks keep their backend — but a client
        // watching the queue gets fresh reasons all the same.
        self.reschedule_all();
        self.default_backend()
    }

    /// Re-evaluates admission on every backend that has a queued task.
    ///
    /// The daemon calls it periodically, because some inputs change without
    /// any request: a controller's lease runs out, the operator edits a
    /// backend's limit. Failures are logged; the next pass retries.
    pub fn reschedule_all(&self) {
        let backends: std::collections::BTreeSet<BackendId> = match self.control.list_tasks() {
            Ok(tasks) => tasks
                .into_iter()
                .filter(|task| task.runner_state == RunnerState::Queued)
                .map(|task| task.assignment.backend_id)
                .collect(),
            Err(error) => {
                tracing::warn!(%error, "scheduler: cannot list tasks");
                return;
            }
        };
        for backend in backends {
            self.reschedule(&backend);
        }
    }

    /// Re-evaluates admission on `backend_id` against its limit as
    /// configured right now, announcing every task that changed.
    ///
    /// Runs after the mutation that prompted it was committed, so a failure
    /// here must not fail that request: it is logged, and the next trigger
    /// (or the daemon's periodic pass) decides again.
    pub fn reschedule(&self, backend_id: &BackendId) {
        let capacity = self.capacity(backend_id);
        match self.control.schedule(backend_id, &capacity) {
            Ok(changed) => {
                for task_id in changed {
                    self.notify(&task_id);
                }
            }
            Err(error) => {
                tracing::warn!(backend = %backend_id, %error, "scheduler: admission pass failed");
            }
        }
    }

    /// The backend's capacity per configuration, read fresh.
    fn capacity(&self, backend_id: &BackendId) -> BackendCapacity {
        match self.launch.backend_limit(backend_id) {
            Ok(limit) => BackendCapacity::Limit(limit),
            Err(error) => BackendCapacity::Unavailable(error.to_string()),
        }
    }

    /// The owner's task, re-read and summarized.
    fn owned_summary(&self, owner: &OwnerRef, task_id: &TaskId) -> ProtocolResult<TaskSummary> {
        let record = self.owned_task(owner, task_id.as_str())?;
        self.summarize(&record)
    }

    /// The last timeline the task's runner reported.
    pub fn timeline(&self, owner: &OwnerRef, task_id: &str) -> ProtocolResult<TimelineResponse> {
        let record = self.owned_task(owner, task_id)?;
        let now = self.control.now();
        let runner = record.runner.as_ref();
        Ok(TimelineResponse {
            freshness: match runner {
                Some(runner) => self.runner_freshness(&record, runner, now)?,
                None => Freshness::Stale,
            },
            observed_at: runner.map(|runner| runner.snapshot.observed_at),
            execution_status: runner.map(|runner| runner.snapshot.execution_status),
            current_state: runner
                .and_then(|runner| runner.snapshot.execution_cursor.current_state.clone()),
            current_position: runner
                .and_then(|runner| runner.snapshot.timeline.as_ref())
                .and_then(current_position),
            timeline: runner.and_then(|runner| runner.snapshot.timeline.clone()),
            task_id: record.task_id.to_string(),
        })
    }

    /// Why the task's runner cannot be asked for content right now (a
    /// conversation page, execution events), or `None` when it is live.
    /// Archived, paused, interrupted, not yet started, unreachable, or
    /// saying so itself — never by recreating compute (KTD4).
    fn live_read_blocker(
        &self,
        record: &TaskRecord,
        now: DateTime<Utc>,
    ) -> ProtocolResult<Option<ConversationUnavailableReason>> {
        let runner = record.runner.as_ref();
        Ok(match record.runner_state {
            RunnerState::Archiving | RunnerState::Archived => {
                Some(ConversationUnavailableReason::Archived)
            }
            RunnerState::Stopped => Some(ConversationUnavailableReason::Paused),
            RunnerState::Interrupted | RunnerState::Unknown => {
                Some(ConversationUnavailableReason::Interrupted)
            }
            RunnerState::Queued
            | RunnerState::Provisioning
            | RunnerState::Running
            | RunnerState::PauseRequested => match runner {
                None => Some(ConversationUnavailableReason::NotStarted),
                Some(runner) if self.runner_freshness(record, runner, now)? == Freshness::Stale => {
                    Some(ConversationUnavailableReason::RunnerUnreachable)
                }
                Some(runner) => match &runner.snapshot.conversation {
                    ConversationAvailability::Live { .. } => None,
                    ConversationAvailability::Unavailable { .. } => {
                        Some(ConversationUnavailableReason::RunnerReported)
                    }
                },
            },
        })
    }

    /// Queues an owner answer for the runner's pending input request.
    pub fn submit_input(
        &self,
        owner: &OwnerRef,
        task_id: &str,
        request: SubmitInputRequest,
    ) -> ProtocolResult<SubmitInputResponse> {
        let record = self.owned_task(owner, task_id)?;
        if request.idempotency_key.trim().is_empty() {
            return Err(ProtocolError::bad_request(
                "missing_idempotency_key",
                "an input needs an idempotency key",
            ));
        }
        let outcome = self.control.queue_input(
            &record.task_id,
            InputRequest {
                pending_input_id: request.pending_input_id,
                idempotency_key: request.idempotency_key,
                delivery: engine_delivery(request.delivery),
            },
        )?;
        if !outcome.replayed {
            self.notify(&record.task_id);
        }
        Ok(SubmitInputResponse {
            input: InputView::from(&outcome.input),
            replayed: outcome.replayed,
        })
    }

    /// Queues an operator command (interrupt, transition, restart) for the
    /// runner of a running task.
    ///
    /// Refused at once, with the reason and nothing written, when the task
    /// is not running, its runner is gone or silent, the generation moved
    /// on, or the command can be seen to be illegal from the runner's last
    /// snapshot (an override without a reason or without
    /// `allow_manual_override`, a target no declared transition leads to).
    /// The runner checks the same again under the Engine's own claim, on
    /// what it then finds: this early refusal only spares the owner a
    /// round trip.
    pub fn submit_operator_command(
        &self,
        owner: &OwnerRef,
        task_id: &str,
        request: SubmitOperatorCommandRequest,
    ) -> ProtocolResult<SubmitOperatorCommandResponse> {
        let record = self.owned_task(owner, task_id)?;
        if request.idempotency_key.trim().is_empty() {
            return Err(ProtocolError::bad_request(
                "missing_idempotency_key",
                "an operator command needs an idempotency key",
            ));
        }
        let is_retry = record
            .operator_commands
            .iter()
            .any(|queued| queued.idempotency_key == request.idempotency_key);
        if !is_retry {
            self.check_operator_command(&record, &request.command)?;
        }
        let outcome = self.control.queue_operator_command(
            &record.task_id,
            OperatorCommandRequest {
                command: request.command,
                idempotency_key: request.idempotency_key,
                expected_generation: request.expected_generation,
                requested_by: owner.clone(),
            },
        )?;
        if !outcome.replayed {
            self.notify(&record.task_id);
        }
        Ok(SubmitOperatorCommandResponse {
            command: OperatorCommandView::from(&outcome.command),
            replayed: outcome.replayed,
        })
    }

    /// What the control plane can tell about `command` without the runner.
    fn check_operator_command(
        &self,
        record: &TaskRecord,
        command: &OperatorCommand,
    ) -> ProtocolResult<()> {
        if matches!(command, OperatorCommand::SetConductorHarness { .. }) {
            return Err(ProtocolError::bad_request(
                "unsupported_command",
                RETIRED_CONDUCTOR_HARNESS,
            ));
        }
        if let Some(reason) = record.operator_command_refusal() {
            return Err(ProtocolError::conflict("operator_command_refused", reason));
        }
        let runner = record.runner.as_ref().ok_or(ProtocolError::NotFound)?;
        if self.runner_freshness(record, runner, self.control.now())? == Freshness::Stale {
            return Err(ProtocolError::conflict(
                "operator_command_refused",
                "the task's runner has not been heard from lately; the command would wait for \
                 a runner that may be gone",
            ));
        }
        if let OperatorCommand::Transition {
            target,
            forced,
            reason,
        } = command
        {
            if target.trim().is_empty() {
                return Err(ProtocolError::bad_request(
                    "missing_target",
                    "a transition needs a target state",
                ));
            }
            if *forced
                && reason
                    .as_deref()
                    .is_none_or(|reason| reason.trim().is_empty())
            {
                return Err(ProtocolError::bad_request(
                    "override_reason_required",
                    "an override needs a reason; it is recorded in the task's audit trail",
                ));
            }
            self.check_transition_legal(record, target, *forced)?;
        }
        check_conductor_command(command)
    }

    /// Checks a transition against the workflow snapshot and the state the
    /// runner last reported; silent when the runner has reported no state
    /// yet — it decides then.
    fn check_transition_legal(
        &self,
        record: &TaskRecord,
        target: &str,
        forced: bool,
    ) -> ProtocolResult<()> {
        let Some(current) = record
            .runner
            .as_ref()
            .and_then(|runner| runner.snapshot.execution_cursor.current_state.as_deref())
        else {
            return Ok(());
        };
        let definition = match record.launch_spec.definition() {
            Ok(definition) => definition,
            // Bookkeeping-only spec: there is nothing to check against.
            Err(LaunchDefinitionError::Missing(_)) => return Ok(()),
            Err(error) => {
                return Err(ProtocolError::Internal(format!(
                    "the task's workflow snapshot is unusable: {error}"
                )))
            }
        };
        let Some(state) = definition.states.get(current) else {
            return Ok(());
        };
        plan_operator_transition(&definition, state, target, forced)
            .map(|_| ())
            .map_err(|error| match error {
                EngineError::InvalidTransition { message } => {
                    let code = if forced {
                        "override_not_allowed"
                    } else {
                        "illegal_transition"
                    };
                    ProtocolError::bad_request(code, message)
                }
                other => ProtocolError::Internal(other.to_string()),
            })
    }

    /// The task's events after `after`.
    pub fn events(
        &self,
        owner: &OwnerRef,
        task_id: &str,
        after: u64,
    ) -> ProtocolResult<Vec<TaskEvent>> {
        let record = self.owned_task(owner, task_id)?;
        Ok(self.control.events_since(&record.task_id, after)?)
    }

    /// Every event the owner has not seen yet according to `cursor`, merged
    /// across tasks in commit-time order (each task's own order kept), with
    /// `cursor` advanced past them. `only` restricts it to one task.
    pub fn catch_up(
        &self,
        owner: &OwnerRef,
        only: Option<&TaskId>,
        cursor: &mut StreamCursor,
    ) -> ProtocolResult<Vec<ControlEventEnvelope>> {
        let task_ids = match only {
            Some(task_id) => vec![task_id.clone()],
            None => self.control.store().list_task_ids()?,
        };
        let mut per_task = Vec::new();
        for task_id in task_ids {
            let record = match self.control.task(&task_id) {
                Ok(record) => record,
                Err(crate::orchestration::OrchestrationError::TaskNotFound(_)) => continue,
                Err(error) => return Err(error.into()),
            };
            if record.owner != *owner {
                continue;
            }
            let events = self
                .control
                .events_since(&task_id, cursor.position(&task_id))?;
            if !events.is_empty() {
                per_task.push(std::collections::VecDeque::from(events));
            }
        }

        let mut merged = Vec::new();
        while let Some(next) = per_task
            .iter()
            .enumerate()
            .filter_map(|(index, events)| events.front().map(|event| (index, event.at)))
            .min_by_key(|(_, at)| *at)
            .map(|(index, _)| index)
        {
            let event = per_task[next]
                .pop_front()
                .expect("chosen queue is not empty");
            cursor.advance(&event.task_id, event.sequence);
            merged.push(ControlEventEnvelope {
                cursor: cursor.to_string(),
                event,
            });
        }
        Ok(merged)
    }

    /// Whether `task_id` names a task of an owner other than `owner`.
    fn someone_else_owns(&self, owner: &OwnerRef, task_id: &TaskId) -> ProtocolResult<bool> {
        match self.control.task(task_id) {
            Ok(record) => Ok(record.owner != *owner),
            Err(crate::orchestration::OrchestrationError::TaskNotFound(_)) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Whether `task_id` names one of the owner's tasks.
    pub fn owns(&self, owner: &OwnerRef, task_id: &TaskId) -> bool {
        self.control
            .task(task_id)
            .is_ok_and(|record| record.owner == *owner)
    }

    /// Creates a one-time code enrolling one controller for `backend_id`.
    pub fn create_enrollment(
        &self,
        backend_id: &str,
        request: CreateEnrollmentRequest,
    ) -> ProtocolResult<EnrollmentResponse> {
        let backend_id = parse_identifier(BackendId::parse(backend_id))?;
        let ttl = bounded_ttl(
            request.ttl_secs,
            self.config.default_enrollment_ttl,
            self.config.max_enrollment_ttl,
        );
        let (record, code) = self
            .credentials
            .create_enrollment(&backend_id, request.name, ttl)?;
        Ok(EnrollmentResponse {
            enrollment_code: code.expose().to_string(),
            backend_id: record.backend_id.to_string(),
            expires_at: record.expires_at,
        })
    }

    /// Every controller identity.
    pub fn controllers(&self) -> ProtocolResult<ControllerListResponse> {
        let controllers = self
            .credentials
            .controllers()?
            .iter()
            .map(|record| self.controller_view(record))
            .collect::<ProtocolResult<_>>()?;
        Ok(ControllerListResponse { controllers })
    }

    /// Revokes a controller identity and fences its lease (R37).
    pub fn revoke_controller(&self, controller_id: &str) -> ProtocolResult<ControllerView> {
        let record = self
            .credentials
            .revoke_controller(controller_id)?
            .ok_or(ProtocolError::NotFound)?;
        // Its lease expired with it: new starts are withheld until another
        // controller holds the backend, and the waiting tasks say so.
        self.reschedule(&record.backend_id);
        self.controller_view(&record)
    }

    // -----------------------------------------------------------------
    // Controller side
    // -----------------------------------------------------------------

    /// Redeems an enrollment code into a controller identity bound to the
    /// request's public key.
    pub fn enroll(&self, request: EnrollRequest) -> ProtocolResult<EnrollResponse> {
        let record = self
            .credentials
            .redeem_enrollment(
                &request.enrollment_code,
                &request.public_key,
                &request.proof,
            )?
            .ok_or(ProtocolError::Unauthorized)?;
        Ok(EnrollResponse {
            controller_id: record.controller_id,
            backend_id: record.backend_id,
            protocol_version: PROTOCOL_VERSION,
        })
    }

    /// Replaces the calling controller's key with the request's new one,
    /// which must sign the rotation itself (proof of possession).
    pub fn rotate_controller_key(
        &self,
        identity: &ControllerIdentity,
        request: RotateKeyRequest,
    ) -> ProtocolResult<RotateKeyResponse> {
        let message =
            crate::auth::pop::rotation_message(&identity.controller_id, &request.public_key);
        if !crate::auth::pop::verify(&request.public_key, &message, &request.proof) {
            return Err(ProtocolError::bad_request(
                "invalid_key_proof",
                "the new key must sign the rotation of this identity",
            ));
        }
        let record = self
            .credentials
            .rotate_controller_key(&identity.controller_id, &request.public_key)?
            .ok_or(ProtocolError::Unauthorized)?;
        Ok(RotateKeyResponse {
            controller_id: record.controller_id,
            rotated_at: record.key_rotated_at.unwrap_or_else(|| self.control.now()),
        })
    }

    /// Records a heartbeat and answers with the server's view.
    pub fn heartbeat(&self, identity: &ControllerIdentity) -> ProtocolResult<HeartbeatResponse> {
        self.credentials
            .record_heartbeat(&identity.controller_id)?
            .ok_or(ProtocolError::Unauthorized)?;
        let lease = self
            .control
            .held_lease(&identity.backend_id, &identity.controller_id)?;
        Ok(HeartbeatResponse {
            controller_id: identity.controller_id.clone(),
            backend_id: identity.backend_id.clone(),
            server_time: self.control.now(),
            lease: lease.map(lease_view),
        })
    }

    /// Acquires the controller's backend lease (a new fencing epoch).
    pub fn acquire_lease(
        &self,
        identity: &ControllerIdentity,
        request: AcquireLeaseRequest,
    ) -> ProtocolResult<LeaseView> {
        let ttl = self.lease_ttl(request.ttl_secs);
        let lease =
            self.control
                .acquire_lease(&identity.backend_id, &identity.controller_id, ttl)?;
        self.credentials.record_heartbeat(&identity.controller_id)?;
        // A controller is back: the backend's capacity is available again.
        self.reschedule(&identity.backend_id);
        Ok(lease_view(lease))
    }

    /// Renews the lease at `request.epoch`.
    pub fn renew_lease(
        &self,
        identity: &ControllerIdentity,
        request: LeaseEpochRequest,
    ) -> ProtocolResult<LeaseView> {
        let ttl = self.lease_ttl(request.ttl_secs);
        let lease = self
            .control
            .renew_lease(&lease_token(identity, request.epoch), ttl)?;
        self.credentials.record_heartbeat(&identity.controller_id)?;
        // A renewal may revive a lease that had lapsed.
        self.reschedule(&identity.backend_id);
        Ok(lease_view(lease))
    }

    /// Releases the lease at `request.epoch`.
    pub fn release_lease(
        &self,
        identity: &ControllerIdentity,
        request: LeaseEpochRequest,
    ) -> ProtocolResult<LeaseView> {
        let lease = self
            .control
            .release_lease(&lease_token(identity, request.epoch))?;
        self.reschedule(&identity.backend_id);
        Ok(lease_view(lease))
    }

    /// The backend's desired commands this controller has not acknowledged
    /// (or all of them), read under its unexpired lease.
    pub fn commands(
        &self,
        identity: &ControllerIdentity,
        all: bool,
    ) -> ProtocolResult<CommandsResponse> {
        let lease = self.require_lease(identity)?;
        // Decide admission before answering: the controller asking is the
        // proof its backend is online, and its limit is read fresh.
        self.reschedule(&identity.backend_id);
        let acknowledged = self
            .credentials
            .controller(&identity.controller_id)?
            .ok_or(ProtocolError::Unauthorized)?
            .acknowledged;
        let commands = self
            .control
            .list_tasks()?
            .iter()
            .filter(|record| record.assignment.backend_id == identity.backend_id)
            .filter(|record| {
                all || record.generation
                    > acknowledged
                        .get(record.task_id.as_str())
                        .copied()
                        .unwrap_or(0)
            })
            // A queued task is offered only once the scheduler admitted it.
            .filter(|record| {
                record.runner_state != RunnerState::Queued || record.reservation.is_some()
            })
            .map(desired_command)
            .collect::<ProtocolResult<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        Ok(CommandsResponse {
            lease_epoch: lease.epoch,
            commands,
        })
    }

    /// [`Self::commands`] as a long-poll: answers as soon as there is at
    /// least one command, or empty after `query.wait_secs`.
    pub async fn wait_for_commands(
        &self,
        identity: &ControllerIdentity,
        query: CommandsQuery,
    ) -> ProtocolResult<CommandsResponse> {
        let wait = self.long_poll(query.wait_secs);
        let found = self
            .wait_for(wait, None, || {
                let response = self.commands(identity, query.all)?;
                Ok((!response.commands.is_empty()).then_some(response))
            })
            .await?;
        match found {
            Some(response) => Ok(response),
            None => self.commands(identity, query.all),
        }
    }

    /// Records acknowledgements; duplicates and reorderings are harmless.
    pub fn acknowledge(
        &self,
        identity: &ControllerIdentity,
        request: AckRequest,
    ) -> ProtocolResult<AckResponse> {
        let mut acks = Vec::with_capacity(request.acks.len());
        for ack in &request.acks {
            let record = self.backend_task(identity, &ack.task_id)?;
            if ack.generation > record.generation {
                return Err(ProtocolError::bad_request(
                    "invalid_ack",
                    format!(
                        "task '{}' is at generation {}, cannot acknowledge {}",
                        record.task_id, record.generation, ack.generation
                    ),
                ));
            }
            acks.push((record.task_id, ack.generation));
        }
        let controller = self
            .credentials
            .acknowledge(&identity.controller_id, &acks)?
            .ok_or(ProtocolError::Unauthorized)?;
        Ok(AckResponse {
            acknowledged: acks
                .iter()
                .map(|(task_id, _)| CommandAck {
                    task_id: task_id.to_string(),
                    generation: controller
                        .acknowledged
                        .get(task_id.as_str())
                        .copied()
                        .unwrap_or(0),
                })
                .collect(),
        })
    }

    /// Accepts a controller's observation, fenced by its lease epoch.
    ///
    /// `PausedAtSafePoint` additionally requires the runner to have
    /// acknowledged a safe point for the current generation (KTD2): compute
    /// is removed only after that, never on the controller's word alone.
    /// [`ControlPlane::report`] enforces it, so every writer is held to it.
    pub fn report(
        &self,
        identity: &ControllerIdentity,
        request: ReportRequest,
    ) -> ProtocolResult<ReportResponse> {
        let record = self.backend_task(identity, &request.task_id)?;
        if request.observation == Observation::StartAccepted
            && record.runner_state == RunnerState::Queued
            && record.reservation.is_none()
        {
            // A start may only be accepted with a reservation; give the
            // scheduler its chance first rather than refuse a controller
            // that merely reported before any pass ran.
            self.reschedule(&identity.backend_id);
        }
        let outcome = self.control.report(ControllerReport {
            task_id: record.task_id.clone(),
            lease: lease_token(identity, request.lease_epoch),
            desired_generation: request.desired_generation,
            observation: request.observation,
            resource: request.resource,
            summary: request.summary,
            observed_at: request.observed_at,
        })?;
        if outcome.applied {
            self.notify(&record.task_id);
            // A pause, a loss, a completed archive may each free a slot.
            self.reschedule(&identity.backend_id);
        }
        Ok(ReportResponse {
            applied: outcome.applied,
            runner_state: outcome.task.runner_state,
            generation: outcome.task.generation,
        })
    }

    /// Issues the credential of a new runner incarnation, replacing the
    /// task's previous one (KTD3).
    ///
    /// Only the backend's current lease holder may, only for the task's
    /// current generation, and only while a runner may be launched
    /// (Provisioning or Running).
    pub fn issue_runner_credential(
        &self,
        identity: &ControllerIdentity,
        task_id: &str,
        request: RunnerCredentialRequest,
    ) -> ProtocolResult<RunnerCredentialResponse> {
        let record = self.backend_task(identity, task_id)?;
        let lease = self.require_lease(identity)?;
        if lease.epoch != request.lease_epoch {
            return Err(ProtocolError::conflict(
                "stale_lease",
                format!(
                    "lease epoch {} is not the current epoch {}",
                    request.lease_epoch, lease.epoch
                ),
            ));
        }
        if request.generation != record.generation {
            return Err(ProtocolError::conflict(
                "stale_generation",
                format!(
                    "task '{}' is at generation {}, not {}",
                    record.task_id, record.generation, request.generation
                ),
            ));
        }
        if !matches!(
            record.runner_state,
            RunnerState::Provisioning | RunnerState::Running
        ) {
            return Err(ProtocolError::conflict(
                "runner_launch_not_allowed",
                format!(
                    "task '{}' cannot launch a runner while {:?}",
                    record.task_id, record.runner_state
                ),
            ));
        }
        let incarnation = request.incarnation.trim();
        if incarnation.is_empty() || incarnation.len() > MAX_INCARNATION_LEN {
            return Err(ProtocolError::bad_request(
                "invalid_incarnation",
                "an incarnation is a non-empty identifier of at most 256 characters",
            ));
        }
        let (session, credential) = self.credentials.issue_runner_session(
            &record.task_id,
            incarnation,
            record.generation,
            lease.epoch,
            self.config.runner_credential_ttl,
        )?;
        Ok(RunnerCredentialResponse {
            task_id: session.task_id,
            generation: session.launch_generation,
            incarnation: session.incarnation,
            credential: credential.expose().to_string(),
            expires_at: session.expires_at,
        })
    }

    // -----------------------------------------------------------------
    // Runner side
    // -----------------------------------------------------------------

    /// Accepts a runner snapshot.
    pub fn runner_snapshot(
        &self,
        identity: &RunnerIdentity,
        snapshot: RunnerSnapshotRequest,
    ) -> ProtocolResult<RunnerSnapshotResponse> {
        let snapshot = super::wire::runner_snapshot_from_request(snapshot).map_err(|error| {
            ProtocolError::bad_request("invalid_runner_snapshot", error.to_string())
        })?;
        let outcome = self.control.record_runner_snapshot(
            &identity.task_id,
            &identity.incarnation,
            identity.launch_generation,
            snapshot,
        )?;
        self.credentials
            .touch_runner(identity, None)?
            .ok_or(ProtocolError::Unauthorized)?;
        if outcome.applied {
            self.notify(&identity.task_id);
        }
        Ok(RunnerSnapshotResponse {
            applied: outcome.applied,
            directive: directive(outcome.task.runner_state),
            generation: outcome.task.generation,
            runner_state: outcome.task.runner_state,
            launch: launch_view(identity, &outcome.task),
            default_harness: self.default_harness_of(&outcome.task),
        })
    }

    /// The runner's long-poll: answers as soon as there is an answer to
    /// deliver or the directive is anything but `Continue`.
    pub async fn runner_poll(
        &self,
        identity: &RunnerIdentity,
        query: RunnerPollQuery,
    ) -> ProtocolResult<RunnerPollResponse> {
        let session = self
            .credentials
            .touch_runner(identity, None)?
            .ok_or(ProtocolError::Unauthorized)?;
        let pending = query.pending_input_id.as_deref();
        let defer_materials = query.defer_materials;
        let poll = || -> ProtocolResult<RunnerPollResponse> {
            let (inputs, changed) = self.control.deliver_inputs(
                &identity.task_id,
                &identity.incarnation,
                identity.launch_generation,
                pending,
            )?;
            let (commands, commands_changed) = self.control.deliver_operator_commands(
                &identity.task_id,
                &identity.incarnation,
                identity.launch_generation,
            )?;
            if changed || commands_changed {
                self.notify(&identity.task_id);
            }
            let record = self.control.task(&identity.task_id)?;
            Ok(RunnerPollResponse {
                directive: directive(record.runner_state),
                generation: record.generation,
                runner_state: record.runner_state,
                inputs: inputs
                    .into_iter()
                    .filter_map(|input| {
                        let delivery = input.delivery?;
                        Some(DeliveredInput {
                            delivery: protocol_delivery(&delivery),
                            input_id: input.input_id,
                            pending_input_id: input.pending_input_id,
                        })
                    })
                    .collect(),
                commands: commands
                    .into_iter()
                    .map(|queued| DeliveredOperatorCommand {
                        command_id: queued.command_id,
                        requested_by: queued.requested_by.0,
                        command: queued.command,
                    })
                    .collect(),
                credential_expires_at: session.expires_at,
                launch: launch_view(identity, &record),
                conversation_pages: self.lock_pages().requests_of(&identity.task_id),
                execution_events: self.lock_event_reads().requests_of(&identity.task_id),
                conductor_reads: self.lock_conductor_reads().requests_of(&identity.task_id),
                materials: self.material_announcements(&identity.task_id),
                default_harness: self.default_harness_of(&record),
            })
        };
        // A change of the default while the runner waits is an answer too:
        // it should not wait out the long-poll to hear of it (U7). The
        // runner says which harness it settled on; one behind a default it
        // can switch to is answered at once. Without it (older runners) the
        // first look says which default the runner is waiting with.
        let runner_harness = query.harness;
        let behind_default = |default: &Option<TaskDefaultHarness>| {
            matches!(
                (default, runner_harness.as_deref()),
                (Some(TaskDefaultHarness::Available { harness }), Some(on)) if harness != on
            )
        };
        let mut asked_with = None;
        let wait = self.long_poll(query.wait_secs);
        let found = self
            .wait_for(wait, Some(&identity.task_id), || {
                let response = poll()?;
                let asked_with = asked_with.get_or_insert_with(|| response.default_harness.clone());
                let actionable = response.default_harness != *asked_with
                    || behind_default(&response.default_harness)
                    || !response.inputs.is_empty()
                    || !response.commands.is_empty()
                    || !response.conversation_pages.is_empty()
                    || !response.execution_events.is_empty()
                    || !response.conductor_reads.is_empty()
                    || (!defer_materials && !response.materials.is_empty())
                    || response.directive != RunnerDirective::Continue;
                Ok(actionable.then_some(response))
            })
            .await?;
        let response = match found {
            Some(response) => response,
            None => poll()?,
        };
        self.credentials.touch_runner(identity, None)?;
        Ok(response)
    }

    /// A conversation page the runner uploads for a waiting owner. Only a
    /// request of the runner's own task can be answered.
    pub fn runner_conversation_page(
        &self,
        identity: &RunnerIdentity,
        request_id: &str,
        page: ConversationPage,
    ) -> ProtocolResult<ConversationPageAccepted> {
        self.credentials
            .touch_runner(identity, None)?
            .ok_or(ProtocolError::Unauthorized)?;
        let waiting = self.lock_pages().take(request_id, &identity.task_id);
        let delivered = waiting.is_some_and(|(_, reply)| reply.send(page).is_ok());
        Ok(ConversationPageAccepted { delivered })
    }

    /// The runner could not read what a waiting owner asked for: the
    /// request is taken out of the relay — so no poll offers it again — and
    /// its owner is answered "unavailable" now instead of at the timeout.
    /// Only a request of the runner's own task can be failed.
    pub fn runner_relay_failure(
        &self,
        identity: &RunnerIdentity,
        request_id: &str,
        reason: &str,
    ) -> ProtocolResult<ConversationPageAccepted> {
        self.credentials
            .touch_runner(identity, None)?
            .ok_or(ProtocolError::Unauthorized)?;
        tracing::warn!(task_id = %identity.task_id, request_id, reason, "a runner could not answer a read");
        // Dropping the owner's channel is the answer: the waiting side
        // reports the runner as unable to read.
        let task_id = &identity.task_id;
        let failed = self.lock_pages().take(request_id, task_id).is_some()
            || self.lock_event_reads().take(request_id, task_id).is_some()
            || self
                .lock_conductor_reads()
                .take(request_id, task_id)
                .is_some();
        Ok(ConversationPageAccepted { delivered: failed })
    }

    fn lock_pages(&self) -> std::sync::MutexGuard<'_, PageRelay> {
        self.pages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_conductor_reads(&self) -> std::sync::MutexGuard<'_, ConductorRelay> {
        self.conductor_reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_event_reads(&self) -> std::sync::MutexGuard<'_, EventsRelay> {
        self.event_reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Extends the runner's credential.
    pub fn runner_renew(&self, identity: &RunnerIdentity) -> ProtocolResult<RunnerRenewResponse> {
        let session = self
            .credentials
            .touch_runner(identity, Some(self.config.runner_credential_ttl))?
            .ok_or(ProtocolError::Unauthorized)?;
        Ok(RunnerRenewResponse {
            expires_at: session.expires_at,
        })
    }

    /// The runner's verdict on a delivered answer.
    pub fn runner_ack_input(
        &self,
        identity: &RunnerIdentity,
        input_id: &str,
        request: InputAckRequest,
    ) -> ProtocolResult<InputAckResponse> {
        let (input, changed) = self.control.settle_input(
            &identity.task_id,
            &identity.incarnation,
            identity.launch_generation,
            input_id,
            request.accepted,
            request.detail,
        )?;
        if changed {
            self.notify(&identity.task_id);
        }
        Ok(InputAckResponse {
            input: InputView::from(&input),
        })
    }

    /// The runner's verdict on a delivered operator command.
    pub fn runner_ack_command(
        &self,
        identity: &RunnerIdentity,
        command_id: &str,
        request: OperatorCommandAckRequest,
    ) -> ProtocolResult<OperatorCommandAckResponse> {
        let (command, changed) = self.control.settle_operator_command(
            &identity.task_id,
            &identity.incarnation,
            identity.launch_generation,
            command_id,
            request.applied,
            request.detail,
        )?;
        if changed {
            self.notify(&identity.task_id);
        }
        Ok(OperatorCommandAckResponse {
            command: OperatorCommandView::from(&command),
        })
    }

    // -----------------------------------------------------------------
    // Views
    // -----------------------------------------------------------------

    /// The normalized summary of a record.
    pub fn summarize(&self, record: &TaskRecord) -> ProtocolResult<TaskSummary> {
        let now = self.control.now();
        let controller = self.controller_presence(&record.assignment.backend_id, now)?;
        self.summarize_with(record, now, controller)
    }

    /// [`Self::summarize`] with the controller of the record's backend
    /// already looked up, as of `now`.
    fn summarize_with(
        &self,
        record: &TaskRecord,
        now: DateTime<Utc>,
        controller: Option<ControllerPresence>,
    ) -> ProtocolResult<TaskSummary> {
        let controller_freshness = controller
            .as_ref()
            .map_or(Freshness::Stale, |presence| presence.freshness);
        let session = self.credentials.runner_session(&record.task_id)?;
        let runner = match &record.runner {
            Some(runner) => {
                let last_contact_at = session
                    .as_ref()
                    .filter(|session| session.incarnation == runner.incarnation)
                    .and_then(|session| session.last_contact_at);
                Some(RunnerView {
                    incarnation: runner.incarnation.clone(),
                    execution_status: runner.snapshot.execution_status,
                    execution_cursor: runner.snapshot.execution_cursor.clone(),
                    event_cursor: runner.snapshot.event_cursor,
                    pending_input: runner
                        .snapshot
                        .pending_input
                        .as_ref()
                        .map(pending_input_view)
                        .transpose()?,
                    safe_point: runner.snapshot.safe_point,
                    problem: runner.snapshot.problem.clone(),
                    conductor_available: runner.snapshot.conductor_available,
                    conductor_unavailable_reason: runner
                        .snapshot
                        .conductor_unavailable_reason
                        .clone(),
                    default_harness_unavailable: runner
                        .snapshot
                        .default_harness_unavailable
                        .clone(),
                    observed_at: runner.snapshot.observed_at,
                    received_at: runner.received_at,
                    last_contact_at,
                    freshness: self.runner_freshness(record, runner, now)?,
                })
            }
            None => None,
        };
        Ok(TaskSummary {
            task_id: record.task_id.clone(),
            backend_id: record.assignment.backend_id.clone(),
            runner_state: record.runner_state,
            generation: record.generation,
            revision: record.revision,
            rank: record.rank,
            created_at: record.created_at,
            queue_sequence: record.queue_sequence,
            workflow_id: record.launch_spec.workflow_id.clone(),
            workflow_version: record.launch_spec.workflow_version,
            launch_digest: record.launch_spec.digest.clone(),
            environment_identity: record.launch_spec.environment.identity.clone(),
            archive: record.archive.clone(),
            observation: record.observed.as_ref().map(|observed| ObservationView {
                observation: observed.observation,
                summary: observed.summary.clone(),
                resource: observed.resource.clone(),
                observed_at: observed.observed_at,
                received_at: observed.received_at,
                freshness: controller_freshness,
            }),
            runner,
            controller,
            inputs: record.inputs.iter().map(InputView::from).collect(),
            operator_commands: record
                .operator_commands
                .iter()
                .map(OperatorCommandView::from)
                .collect(),
            attention: task_attention(record),
            queue: record.queue.as_ref().map(QueueView::from),
            display: task_display(record),
            manual_transitions: manual_transitions(record),
            resume: record.resume.clone(),
            as_of: now,
        })
    }

    /// Live only while the task expects a runner, the runner's launch is
    /// current, and it was heard from within the staleness window.
    fn runner_freshness(
        &self,
        record: &TaskRecord,
        runner: &RunnerObservation,
        now: DateTime<Utc>,
    ) -> ProtocolResult<Freshness> {
        let expects_runner = matches!(
            record.runner_state,
            RunnerState::Provisioning | RunnerState::Running | RunnerState::PauseRequested
        );
        if !expects_runner || record.supersedes_launch(runner.launch_generation) {
            return Ok(Freshness::Stale);
        }
        let last_contact = self
            .credentials
            .runner_session(&record.task_id)?
            .filter(|session| session.incarnation == runner.incarnation)
            .and_then(|session| session.last_contact_at);
        let heard = last_contact.map_or(runner.received_at, |contact| {
            contact.max(runner.received_at)
        });
        Ok(if now - heard <= self.config.runner_stale_after {
            Freshness::Live
        } else {
            Freshness::Stale
        })
    }

    fn controller_presence(
        &self,
        backend_id: &BackendId,
        now: DateTime<Utc>,
    ) -> ProtocolResult<Option<ControllerPresence>> {
        let Some(lease) = self.control.lease(backend_id)? else {
            return Ok(None);
        };
        let record = self.credentials.controller(&lease.controller_id)?;
        let last_heartbeat_at = record.as_ref().and_then(|record| record.last_heartbeat_at);
        let live = now < lease.expires_at
            && record
                .as_ref()
                .is_some_and(|record| record.revoked_at.is_none())
            && last_heartbeat_at
                .is_some_and(|heartbeat| now - heartbeat <= self.config.controller_stale_after);
        Ok(Some(ControllerPresence {
            controller_id: lease.controller_id,
            lease_epoch: lease.epoch,
            lease_expires_at: lease.expires_at,
            last_heartbeat_at,
            freshness: if live {
                Freshness::Live
            } else {
                Freshness::Stale
            },
        }))
    }

    fn controller_view(&self, record: &ControllerRecord) -> ProtocolResult<ControllerView> {
        let holds_lease = self
            .control
            .held_lease(&record.backend_id, &record.controller_id)?
            .is_some();
        Ok(ControllerView {
            controller_id: record.controller_id.clone(),
            backend_id: record.backend_id.to_string(),
            name: record.name.clone(),
            enrolled_at: record.enrolled_at,
            revoked_at: record.revoked_at,
            last_heartbeat_at: record.last_heartbeat_at,
            holds_lease: holds_lease && record.revoked_at.is_none(),
        })
    }

    // -----------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------

    /// The owner's task, or `NotFound` — for an unknown id, a malformed one
    /// and another owner's alike.
    fn owned_task(&self, owner: &OwnerRef, task_id: &str) -> ProtocolResult<TaskRecord> {
        let task_id = TaskId::parse(task_id).map_err(|_| ProtocolError::NotFound)?;
        let record = self.control.task(&task_id)?;
        if record.owner != *owner {
            return Err(ProtocolError::NotFound);
        }
        Ok(record)
    }

    /// A task of the controller's backend, or `NotFound` (R37 scope).
    fn backend_task(
        &self,
        identity: &ControllerIdentity,
        task_id: &str,
    ) -> ProtocolResult<TaskRecord> {
        let task_id = TaskId::parse(task_id).map_err(|_| ProtocolError::NotFound)?;
        let record = self.control.task(&task_id)?;
        if record.assignment.backend_id != identity.backend_id {
            return Err(ProtocolError::NotFound);
        }
        Ok(record)
    }

    fn require_lease(&self, identity: &ControllerIdentity) -> ProtocolResult<BackendLease> {
        self.control
            .held_lease(&identity.backend_id, &identity.controller_id)?
            .ok_or_else(|| {
                ProtocolError::conflict(
                    "lease_not_held",
                    format!(
                        "controller '{}' does not hold an unexpired lease of backend '{}'",
                        identity.controller_id, identity.backend_id
                    ),
                )
            })
    }

    fn lease_ttl(&self, requested: Option<u64>) -> Duration {
        bounded_ttl(
            requested,
            self.config.default_lease_ttl,
            self.config.max_lease_ttl,
        )
    }

    fn long_poll(&self, requested: Option<u64>) -> std::time::Duration {
        std::time::Duration::from_secs(requested.unwrap_or(0)).min(self.config.max_long_poll)
    }

    /// Re-runs `check` after every change notification until it finds
    /// something or `wait` elapses. With `only`, a notification of another
    /// task is ignored: the check reads that one task, so another's change
    /// cannot have given it anything to find.
    ///
    /// Subscribes *before* the first check, so a change landing between the
    /// check and the wait is never missed.
    async fn wait_for<T>(
        &self,
        wait: std::time::Duration,
        only: Option<&TaskId>,
        mut check: impl FnMut() -> ProtocolResult<Option<T>>,
    ) -> ProtocolResult<Option<T>> {
        let mut changes = self.changes.subscribe();
        // The installation's default harness, which every task sees (U7):
        // a runner hears of a change now rather than when its long-poll
        // runs out.
        let mut harness_changes = self.launch.default_harness_changes();
        let deadline = tokio::time::Instant::now() + wait;
        let mut relevant = true;
        loop {
            if relevant {
                if let Some(found) = check()? {
                    return Ok(Some(found));
                }
            }
            relevant = tokio::select! {
                received = tokio::time::timeout_at(deadline, changes.recv()) => match received {
                    Err(_elapsed) => return Ok(None),
                    Ok(Ok(changed)) => only.is_none_or(|task_id| *task_id == changed),
                    // Some notifications were missed: any of them may matter.
                    Ok(Err(RecvError::Lagged(_))) => true,
                    Ok(Err(RecvError::Closed)) => return Ok(None),
                },
                Ok(()) = harness_changed(&mut harness_changes) => true,
            };
        }
    }
}

/// Resolves when the default harness `changes` watches changes; never
/// without one.
async fn harness_changed(
    changes: &mut Option<watch::Receiver<u64>>,
) -> Result<(), watch::error::RecvError> {
    match changes {
        Some(changes) => changes.changed().await,
        None => std::future::pending().await,
    }
}

/// Most characters of one chat message to the conductor: the text is kept
/// in the task's event stream, which is not the place for a document.
const MAX_CHAT_CHARS: usize = 4_000;

/// What can be said about a conductor command without the runner: that it
/// names what it needs.
fn check_conductor_command(command: &OperatorCommand) -> ProtocolResult<()> {
    let blank = |text: &str| text.trim().is_empty();
    match command {
        OperatorCommand::ConductorChat { text } if blank(text) => Err(ProtocolError::bad_request(
            "empty_message",
            "a chat message cannot be empty",
        )),
        OperatorCommand::ConductorChat { text } if text.chars().count() > MAX_CHAT_CHARS => {
            Err(ProtocolError::bad_request(
                "message_too_long",
                format!("a chat message holds at most {MAX_CHAT_CHARS} characters"),
            ))
        }
        OperatorCommand::ConfirmProposal { proposal_id }
        | OperatorCommand::RejectProposal { proposal_id }
            if blank(proposal_id) =>
        {
            Err(ProtocolError::bad_request(
                "missing_proposal",
                "a proposal command needs the proposal's id",
            ))
        }
        OperatorCommand::UndoDecision { decision_id } if blank(decision_id) => {
            Err(ProtocolError::bad_request(
                "missing_decision",
                "undoing a decision needs the decision's id",
            ))
        }
        _ => Ok(()),
    }
}

/// The desired command of a task, `None` once it is archived for good.
/// What waits on the owner: the runner's last report — minus the requests
/// the owner has answered since (an answer queued, delivered or consumed)
/// — plus a provisioning that failed. Oldest first.
fn task_attention(record: &TaskRecord) -> Vec<AttentionItem> {
    if record.runner_state.is_archival() {
        return Vec::new();
    }
    let answered = |waiting_id: &str| {
        record.inputs.iter().any(|input| {
            input.pending_input_id == waiting_id
                && matches!(
                    input.status,
                    InputStatus::Queued | InputStatus::Delivered | InputStatus::Consumed
                )
        })
    };
    // Already cleaned when the snapshot was accepted.
    let mut items: Vec<AttentionItem> = record
        .runner
        .iter()
        .flat_map(|runner| &runner.snapshot.attention)
        .filter(|item| !answered(&item.waiting_id))
        .cloned()
        .collect();
    // The backend gave up before any execution existed: the runner (if one
    // ever reported) never got as far as creating one.
    let has_execution = record
        .runner
        .as_ref()
        .is_some_and(|runner| runner.snapshot.execution_cursor.execution_id.is_some());
    if matches!(
        record.runner_state,
        RunnerState::Interrupted | RunnerState::Unknown
    ) && !has_execution
    {
        items.push(AttentionItem {
            kind: AttentionKind::ProvisioningFailed,
            state_id: None,
            since: record
                .observed
                .as_ref()
                .map_or(record.created_at, |observed| observed.received_at),
            waiting_id: format!("provisioning-{}", record.generation),
        });
    }
    items.sort_by_key(|item| item.since);
    items
}

/// The label of the attempt under way — in progress or awaiting a decision
/// — the last one if several.
fn current_position(timeline: &Timeline) -> Option<String> {
    timeline
        .positions
        .iter()
        .rev()
        .find(|position| {
            matches!(
                position.status,
                PositionStatus::InProgress | PositionStatus::AwaitingDecision
            )
        })
        .map(|position| position.label.clone())
}

/// What a task shows as its title and links: the runner's latest rendering,
/// else the workflow's `display` rendered from the sealed inputs alone —
/// which is all there is to go on while the task still waits in the queue.
/// The transitions an owner can ask for out of the state the runner last
/// reported: read from the sealed workflow snapshot, only while the task is
/// running (nothing else can carry one out).
fn manual_transitions(record: &TaskRecord) -> Option<TransitionOffer> {
    if record.runner_state != RunnerState::Running {
        return None;
    }
    let current = record
        .runner
        .as_ref()?
        .snapshot
        .execution_cursor
        .current_state
        .as_deref()?;
    let definition = record.launch_spec.definition().ok()?;
    let state = definition.states.get(current)?;
    let mut exits: Vec<TransitionExit> = Vec::new();
    let mut labels: Vec<(&String, &String)> = state.transitions.iter().collect();
    labels.sort();
    for (label, target) in labels {
        if exits.iter().any(|exit| exit.target == *target) {
            continue;
        }
        exits.push(TransitionExit {
            label: label.clone(),
            target: target.clone(),
            description: state
                .outputs
                .iter()
                .find(|output| output.label == *label)
                .and_then(|output| output.description.clone()),
        });
    }
    let mut override_targets: Vec<String> = if state.allow_manual_override {
        definition
            .states
            .keys()
            .filter(|id| id.as_str() != current)
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    override_targets.sort();
    Some(TransitionOffer {
        from_state: current.to_string(),
        exits,
        override_allowed: state.allow_manual_override,
        override_targets,
    })
}

fn task_display(record: &TaskRecord) -> TaskDisplay {
    if let Some(reported) = record
        .runner
        .as_ref()
        .and_then(|runner| runner.snapshot.display.clone())
    {
        // A remote runner's word is not trusted with a terminal.
        return reported.sanitized();
    }
    let Some(spec) = record.launch_spec.display_spec() else {
        return TaskDisplay::default();
    };
    let no_variables = HashMap::new();
    let inputs = record
        .launch_spec
        .inputs
        .as_object()
        .cloned()
        .unwrap_or_default();
    TemplateScope::from_parts(&inputs, &no_variables).render_display(&spec)
}

fn desired_command(record: &TaskRecord) -> ProtocolResult<Option<DesiredCommand>> {
    let action = match record.runner_state {
        RunnerState::Queued => DesiredAction::Start,
        RunnerState::Provisioning => DesiredAction::Provision,
        RunnerState::Running => DesiredAction::Run,
        RunnerState::PauseRequested => DesiredAction::PauseAtSafePoint,
        RunnerState::Stopped => DesiredAction::KeepPaused,
        RunnerState::Interrupted | RunnerState::Unknown => DesiredAction::Hold,
        RunnerState::Archiving => DesiredAction::Archive,
        RunnerState::Archived => return Ok(None),
    };
    let launch_spec = serde_json::to_value(&record.launch_spec)
        .map_err(|error| ProtocolError::Internal(error.to_string()))?;
    Ok(Some(DesiredCommand {
        task_id: record.task_id.clone(),
        generation: record.generation,
        runner_state: record.runner_state,
        action,
        resource_key: record.assignment.resource_key.clone(),
        launch_spec,
        runner: record.runner.as_ref().map(|runner| RunnerProgress {
            incarnation: runner.incarnation.clone(),
            launch_generation: runner.launch_generation,
            execution_status: runner.snapshot.execution_status,
            event_cursor: runner.snapshot.event_cursor,
            safe_point_acknowledged: runner.safe_point_generation == Some(record.generation),
            received_at: runner.received_at,
        }),
    }))
}

/// Whether `value` can name an attempt: the Engine's ids are UUIDs, and
/// nothing outside this conservative set is ever forwarded to a runner.
fn is_attempt_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value.starts_with('.')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// The calling runner's launch, as the record knows it.
fn launch_view(identity: &RunnerIdentity, record: &TaskRecord) -> RunnerLaunchView {
    let receipt = record.launch_receipt(identity.launch_generation);
    RunnerLaunchView {
        incarnation: identity.incarnation.clone(),
        launch_generation: identity.launch_generation,
        authorized_by: receipt.map(|receipt| receipt.command),
        authorized_by_actor: receipt.map(|receipt| receipt.actor),
    }
}

/// What a runner should do while the task is in `runner_state`.
fn directive(runner_state: RunnerState) -> RunnerDirective {
    match runner_state {
        RunnerState::PauseRequested => RunnerDirective::PauseAtSafePoint,
        RunnerState::Stopped | RunnerState::Archiving | RunnerState::Archived => {
            RunnerDirective::Stop
        }
        RunnerState::Queued
        | RunnerState::Provisioning
        | RunnerState::Running
        | RunnerState::Interrupted
        | RunnerState::Unknown => RunnerDirective::Continue,
    }
}

fn lease_token(identity: &ControllerIdentity, epoch: u64) -> LeaseToken {
    LeaseToken {
        backend_id: identity.backend_id.clone(),
        controller_id: identity.controller_id.clone(),
        epoch,
    }
}

fn lease_view(lease: BackendLease) -> LeaseView {
    LeaseView {
        backend_id: lease.backend_id,
        controller_id: lease.controller_id,
        epoch: lease.epoch,
        acquired_at: lease.acquired_at,
        expires_at: lease.expires_at,
    }
}

fn bounded_ttl(requested: Option<u64>, default: Duration, max: Duration) -> Duration {
    match requested {
        Some(secs) => {
            let max_secs = u64::try_from(max.num_seconds()).unwrap_or(1).max(1);
            Duration::seconds(secs.clamp(1, max_secs) as i64)
        }
        None => default,
    }
}

/// Digest of a create request — what an identical retry presents again.
fn request_digest(request: &CreateTaskRequest) -> ProtocolResult<String> {
    let value = serde_json::to_value(request)
        .map_err(|error| ProtocolError::Internal(format!("cannot digest a request: {error}")))?;
    Ok(crate::domain::canonical_sha256(&value))
}

fn parse_identifier<T>(parsed: Result<T, crate::domain::InvalidIdentifier>) -> ProtocolResult<T> {
    parsed.map_err(|error| ProtocolError::bad_request("invalid_identifier", error.to_string()))
}
