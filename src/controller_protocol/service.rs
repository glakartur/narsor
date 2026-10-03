//! The protocol's server-side handling, independent of HTTP.
//!
//! [`ProtocolService`] is the one door owners, controllers and runners go
//! through to the [`ControlPlane`]: it scopes every request to the caller
//! (an owner sees only its tasks, a controller only its backend's, a runner
//! only its own incarnation's), turns records into the normalized wire
//! views, and announces every accepted mutation on an in-process channel —
//! which is what wakes long-polls and live event streams.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

use super::credentials::{
    ControllerAuthenticator, ControllerIdentity, ControllerRecord, CredentialStore,
    RunnerAuthenticator, RunnerIdentity,
};
use super::error::{ProtocolError, ProtocolResult};
use super::wire::*;
use crate::domain::{BackendId, TaskId, TaskLaunchSpec};
use crate::orchestration::{
    BackendLease, CommandRequest, ControlPlane, ControllerReport, ConversationAvailability,
    InputRequest, LeaseToken, NewTask, Observation, OwnerRef, RunnerObservation, TaskEvent,
    TaskLifecycle, TaskRecord,
};

/// The owner every request authenticated by the local daemon token acts
/// as. Trusted loopback mode has exactly one owner; U7's OIDC mode puts the
/// verified `(issuer, subject)` here instead.
pub const LOCAL_OWNER: &str = "local";

/// How many change notifications may queue for a slow listener before it
/// lags — a lagging listener rescans instead of losing anything.
const CHANGE_CHANNEL_CAPACITY: usize = 256;

/// Longest incarnation identifier accepted (a Pod UID is 36 characters).
const MAX_INCARNATION_LEN: usize = 256;

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
    pub fn new(control: ControlPlane) -> Self {
        Self::with_config(control, ProtocolConfig::default())
    }

    /// A service with an explicit policy.
    pub fn with_config(control: ControlPlane, config: ProtocolConfig) -> Self {
        let credentials = CredentialStore::new(control.clone());
        let (changes, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
        Self {
            controller_auth: Arc::new(credentials.clone()),
            runner_auth: Arc::new(credentials.clone()),
            control,
            credentials,
            config,
            changes,
        }
    }

    /// Replaces how controller credentials are verified (U7: mTLS, signed
    /// proof-of-possession).
    pub fn with_controller_authenticator(mut self, auth: Arc<dyn ControllerAuthenticator>) -> Self {
        self.controller_auth = auth;
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

    /// Announces that `task_id`'s record changed. Public so an in-process
    /// writer that mutates the [`ControlPlane`] directly (U4's local
    /// adapter) wakes streams and long-polls too.
    pub fn notify(&self, task_id: &TaskId) {
        // No receiver is not an error: nobody is listening right now.
        let _ = self.changes.send(task_id.clone());
    }

    /// The controller a bearer token proves, if any.
    pub fn authenticate_controller(&self, bearer: &str) -> Option<ControllerIdentity> {
        self.controller_auth.authenticate_controller(bearer)
    }

    /// The runner a bearer token proves, if any.
    pub fn authenticate_runner(&self, bearer: &str) -> Option<RunnerIdentity> {
        self.runner_auth.authenticate_runner(bearer)
    }

    // -----------------------------------------------------------------
    // Owner side
    // -----------------------------------------------------------------

    /// Creates a queued task for `owner`, idempotently by task id.
    pub fn create_task(
        &self,
        owner: &OwnerRef,
        request: CreateTaskRequest,
    ) -> ProtocolResult<TaskSummary> {
        let task_id = match request.task_id {
            Some(task_id) => parse_identifier(TaskId::parse(task_id))?,
            None => TaskId::parse(format!("task-{}", uuid::Uuid::new_v4().simple()))
                .expect("a generated task id is always valid"),
        };
        let backend_id = parse_identifier(BackendId::parse(request.backend_id))?;
        let launch = request.launch;
        let spec = TaskLaunchSpec::new(
            launch.workflow_id,
            launch.workflow_version,
            launch.inputs,
            launch.workspace,
        );
        let record = self.control.create_task(
            NewTask::new(task_id, owner.clone(), backend_id, spec).with_priority(request.priority),
        )?;
        self.notify(&record.task_id);
        self.summarize(&record)
    }

    /// The owner's tasks and the stream cursor at exactly that state.
    pub fn list_tasks(&self, owner: &OwnerRef) -> ProtocolResult<TaskListResponse> {
        let mut cursor = StreamCursor::default();
        let mut tasks = Vec::new();
        for record in self.control.list_tasks()? {
            if record.owner != *owner {
                continue;
            }
            cursor.advance(&record.task_id, record.revision);
            tasks.push(self.summarize(&record)?);
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
        let outcome = self.control.command(
            &record.task_id,
            CommandRequest {
                command: request.command,
                idempotency_key: request.idempotency_key,
                expected_generation: request.expected_generation,
            },
        )?;
        if !outcome.replayed {
            self.notify(&record.task_id);
        }
        Ok(OwnerCommandResponse {
            task: self.summarize(&outcome.task)?,
            receipt: outcome.receipt,
            replayed: outcome.replayed,
        })
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
            timeline: runner.and_then(|runner| runner.snapshot.timeline.clone()),
            task_id: record.task_id,
        })
    }

    /// The task's conversation summary, or why it is unavailable.
    ///
    /// Available only while the runner should be and is live; a paused,
    /// interrupted or archived task answers with its last safe summary and
    /// an explicit reason — never by recreating compute (KTD4).
    pub fn conversation(
        &self,
        owner: &OwnerRef,
        task_id: &str,
    ) -> ProtocolResult<ConversationResponse> {
        let record = self.owned_task(owner, task_id)?;
        let now = self.control.now();
        let runner = record.runner.as_ref();
        let last_attempts = match runner.map(|runner| &runner.snapshot.conversation) {
            Some(ConversationAvailability::Live { attempts }) => attempts.clone(),
            _ => Vec::new(),
        };
        let observed_at = runner.map(|runner| runner.snapshot.observed_at);
        let reason = match record.lifecycle {
            TaskLifecycle::Archiving | TaskLifecycle::Archived => {
                Some(ConversationUnavailableReason::Archived)
            }
            TaskLifecycle::Paused => Some(ConversationUnavailableReason::Paused),
            TaskLifecycle::Interrupted | TaskLifecycle::Unknown => {
                Some(ConversationUnavailableReason::Interrupted)
            }
            TaskLifecycle::Queued
            | TaskLifecycle::Provisioning
            | TaskLifecycle::Running
            | TaskLifecycle::PauseRequested => match runner {
                None => Some(ConversationUnavailableReason::NotStarted),
                Some(runner)
                    if self.runner_freshness(&record, runner, now)? == Freshness::Stale =>
                {
                    Some(ConversationUnavailableReason::RunnerUnreachable)
                }
                Some(runner) => match &runner.snapshot.conversation {
                    ConversationAvailability::Live { .. } => None,
                    ConversationAvailability::Unavailable { .. } => {
                        Some(ConversationUnavailableReason::RunnerReported)
                    }
                },
            },
        };
        Ok(match (reason, observed_at) {
            (None, Some(observed_at)) => ConversationResponse::Available {
                attempts: last_attempts,
                observed_at,
            },
            (reason, observed_at) => ConversationResponse::Unavailable {
                reason: reason.unwrap_or(ConversationUnavailableReason::NotStarted),
                last_attempts,
                observed_at,
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
                delivery: request.delivery,
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
            backend_id: record.backend_id,
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
        self.controller_view(&record)
    }

    // -----------------------------------------------------------------
    // Controller side
    // -----------------------------------------------------------------

    /// Redeems an enrollment code into a controller identity.
    pub fn enroll(&self, request: EnrollRequest) -> ProtocolResult<EnrollResponse> {
        let enrolled = self
            .credentials
            .redeem_enrollment(&request.enrollment_code)?
            .ok_or(ProtocolError::Unauthorized)?;
        Ok(EnrollResponse {
            controller_id: enrolled.record.controller_id,
            backend_id: enrolled.record.backend_id,
            credential: enrolled.credential.expose().to_string(),
            protocol_version: PROTOCOL_VERSION,
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
            .filter_map(desired_command)
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
            .wait_for(wait, || {
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
    pub fn report(
        &self,
        identity: &ControllerIdentity,
        request: ReportRequest,
    ) -> ProtocolResult<ReportResponse> {
        let record = self.backend_task(identity, &request.task_id)?;
        if request.observation == Observation::PausedAtSafePoint
            && !safe_point_acknowledged(&record)
        {
            return Err(ProtocolError::conflict(
                "safe_point_not_acknowledged",
                format!(
                    "the runner of task '{}' has not acknowledged a safe point for generation {}",
                    record.task_id, record.generation
                ),
            ));
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
        }
        Ok(ReportResponse {
            applied: outcome.applied,
            lifecycle: outcome.task.lifecycle,
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
            record.lifecycle,
            TaskLifecycle::Provisioning | TaskLifecycle::Running
        ) {
            return Err(ProtocolError::conflict(
                "runner_launch_not_allowed",
                format!(
                    "task '{}' cannot launch a runner while {:?}",
                    record.task_id, record.lifecycle
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
            directive: directive(outcome.task.lifecycle),
            generation: outcome.task.generation,
            lifecycle: outcome.task.lifecycle,
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
        let poll = || -> ProtocolResult<RunnerPollResponse> {
            let (inputs, changed) = self.control.deliver_inputs(
                &identity.task_id,
                &identity.incarnation,
                identity.launch_generation,
                pending,
            )?;
            if changed {
                self.notify(&identity.task_id);
            }
            let record = self.control.task(&identity.task_id)?;
            Ok(RunnerPollResponse {
                directive: directive(record.lifecycle),
                generation: record.generation,
                lifecycle: record.lifecycle,
                inputs: inputs
                    .into_iter()
                    .filter_map(|input| {
                        Some(DeliveredInput {
                            delivery: input.delivery?,
                            input_id: input.input_id,
                            pending_input_id: input.pending_input_id,
                        })
                    })
                    .collect(),
                credential_expires_at: session.expires_at,
            })
        };
        let wait = self.long_poll(query.wait_secs);
        let found = self
            .wait_for(wait, || {
                let response = poll()?;
                let actionable =
                    !response.inputs.is_empty() || response.directive != RunnerDirective::Continue;
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

    // -----------------------------------------------------------------
    // Views
    // -----------------------------------------------------------------

    /// The normalized summary of a record.
    pub fn summarize(&self, record: &TaskRecord) -> ProtocolResult<TaskSummary> {
        let now = self.control.now();
        let controller = self.controller_presence(&record.assignment.backend_id, now)?;
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
                    execution_status: runner.snapshot.execution_status.clone(),
                    execution_cursor: runner.snapshot.execution_cursor.clone(),
                    event_cursor: runner.snapshot.event_cursor,
                    pending_input: runner.snapshot.pending_input.clone(),
                    safe_point: runner.snapshot.safe_point,
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
            lifecycle: record.lifecycle,
            generation: record.generation,
            revision: record.revision,
            priority: record.priority,
            created_at: record.created_at,
            queue_sequence: record.queue_sequence,
            workflow_id: record.launch_spec.workflow_id.clone(),
            workflow_version: record.launch_spec.workflow_version,
            launch_digest: record.launch_spec.digest.clone(),
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
            record.lifecycle,
            TaskLifecycle::Provisioning | TaskLifecycle::Running | TaskLifecycle::PauseRequested
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
            backend_id: record.backend_id.clone(),
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
    /// something or `wait` elapses.
    ///
    /// Subscribes *before* the first check, so a change landing between the
    /// check and the wait is never missed.
    async fn wait_for<T>(
        &self,
        wait: std::time::Duration,
        mut check: impl FnMut() -> ProtocolResult<Option<T>>,
    ) -> ProtocolResult<Option<T>> {
        let mut changes = self.changes.subscribe();
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            if let Some(found) = check()? {
                return Ok(Some(found));
            }
            match tokio::time::timeout_at(deadline, changes.recv()).await {
                Err(_elapsed) => return Ok(None),
                Ok(Ok(_)) | Ok(Err(RecvError::Lagged(_))) => continue,
                Ok(Err(RecvError::Closed)) => return Ok(None),
            }
        }
    }
}

/// The desired command of a task, `None` once it is archived for good.
fn desired_command(record: &TaskRecord) -> Option<DesiredCommand> {
    let action = match record.lifecycle {
        TaskLifecycle::Queued => DesiredAction::Start,
        TaskLifecycle::Provisioning => DesiredAction::Provision,
        TaskLifecycle::Running => DesiredAction::Run,
        TaskLifecycle::PauseRequested => DesiredAction::PauseAtSafePoint,
        TaskLifecycle::Paused => DesiredAction::KeepPaused,
        TaskLifecycle::Interrupted | TaskLifecycle::Unknown => DesiredAction::Hold,
        TaskLifecycle::Archiving => DesiredAction::Archive,
        TaskLifecycle::Archived => return None,
    };
    Some(DesiredCommand {
        task_id: record.task_id.clone(),
        generation: record.generation,
        lifecycle: record.lifecycle,
        action,
        resource_key: record.assignment.resource_key.clone(),
        launch_spec: record.launch_spec.clone(),
        runner: record.runner.as_ref().map(|runner| RunnerProgress {
            incarnation: runner.incarnation.clone(),
            launch_generation: runner.launch_generation,
            execution_status: runner.snapshot.execution_status.clone(),
            event_cursor: runner.snapshot.event_cursor,
            safe_point_acknowledged: runner.safe_point_generation == Some(record.generation),
            received_at: runner.received_at,
        }),
    })
}

fn safe_point_acknowledged(record: &TaskRecord) -> bool {
    record
        .runner
        .as_ref()
        .is_some_and(|runner| runner.safe_point_generation == Some(record.generation))
}

/// What a runner should do while the task is in `lifecycle`.
fn directive(lifecycle: TaskLifecycle) -> RunnerDirective {
    match lifecycle {
        TaskLifecycle::PauseRequested => RunnerDirective::PauseAtSafePoint,
        TaskLifecycle::Paused | TaskLifecycle::Archiving | TaskLifecycle::Archived => {
            RunnerDirective::Stop
        }
        TaskLifecycle::Queued
        | TaskLifecycle::Provisioning
        | TaskLifecycle::Running
        | TaskLifecycle::Interrupted
        | TaskLifecycle::Unknown => RunnerDirective::Continue,
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

fn parse_identifier<T>(parsed: Result<T, crate::domain::InvalidIdentifier>) -> ProtocolResult<T> {
    parsed.map_err(|error| ProtocolError::bad_request("invalid_identifier", error.to_string()))
}
