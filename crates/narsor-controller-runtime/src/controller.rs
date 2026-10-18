//! The generic environment controller: reconciles a backend with the
//! control plane's desired task runner_state over the controller protocol
//! (R3-R5, R12-R14, R34, R37; KTD1-KTD3).
//!
//! # One step
//!
//! 1. **Hold the lease.** Acquire the backend's fenced lease, renew it at a
//!    third of its validity, heartbeat in between. Another controller's
//!    unexpired lease puts this one in standby: it does not touch the
//!    backend at all until it holds the lease itself.
//! 2. **Read the whole desired state** (`all=true`) and acknowledge every
//!    generation in it. Reading everything every step is what makes a
//!    restart, a reconnect and a lease handoff the same, boring case: the
//!    controller never relies on remembering what it did, only on what the
//!    control plane wants and what the backend shows (R14).
//! 3. **Reconcile every task**: observe its resources by deterministic
//!    identity, act, report. The rules are those of
//!    [`DesiredAction`]; in short — report `StartAccepted` before creating
//!    anything, never create a workload while one of the task's exists,
//!    never recreate a lost one on its own (a replacement needs a new
//!    desired generation), remove a live runner's compute only after its
//!    safe-point acknowledgement, and report archive complete only once
//!    workload and storage are gone.
//!
//! Between steps it sleeps briefly while something is in progress (a
//! runner starting, a deletion finishing, a safe point awaited) and
//! otherwise long-polls for new commands, which also paces the periodic
//! observation that notices a dead workload.
//!
//! # What a lost connection means
//!
//! Nothing about the workloads. A controller that cannot reach the control
//! plane stops acting and retries; it never reports a runner lost because
//! *it* lost contact, and the runners keep working on their own channel
//! (R5, R14). After reconnecting it renews its lease if nobody took over —
//! or finds itself fenced and goes to standby — and reconciles from scratch.
//!
//! # What a lost observation means
//!
//! When the backend itself cannot be observed, a workload that should be
//! running is reported `WorkloadLost { state_known: false }` (→ Unknown)
//! after [`ControllerConfig::observe_failures_before_unknown`] consecutive
//! failures. A dead or vanished workload is reported
//! `WorkloadLost { state_known: true }` (→ Interrupted) at once — whatever
//! its exit code.
//!
//! An Unknown task's workload is observed again at the idle pace, as a
//! running one is (R3). Only a workload seen gone or terminated while the
//! task's storage is still there is reported `LossConfirmed`
//! (→ Interrupted). A live workload, one still being deleted, another
//! failed observation and missing storage all leave the task Unknown: a
//! runner that may still be working, or a task whose work is gone, is not
//! something to resume.
//!
//! The controller never replaces a lost workload by itself (R13). A
//! replacement comes only with a new desired generation: an owner's
//! `start`/`resume`, or the control plane's resume policy, which acts on
//! Interrupted tasks alone.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::time::Duration;

use chrono::Utc;
use tokio::time::Instant;

use crate::backend::{
    validate_launch_input, BackendError, BackendErrorKind, ExecutionBackend, Removal,
    RunnerCredential, TaskObservation, TaskResources, WorkloadLaunch, WorkloadPhase, WorkloadState,
};
use narsor_protocol::controller::{
    AckResponse, CommandAck, CommandsResponse, DesiredAction, DesiredCommand, HeartbeatResponse,
    LeaseView, Observation, ObservedSummary, ReportRequest, ReportResponse, ResourceIdentity,
    RunnerCredentialRequest, RunnerCredentialResponse, RunnerState,
};
use narsor_protocol::identifier::TaskId;
use narsor_shared::api_error::ClientError;

/// Refusal codes that mean "this controller no longer holds the lease".
const LEASE_CODES: [&str; 4] = [
    "stale_lease",
    "lease_expired",
    "lease_not_held",
    "lease_held",
];

/// The controller's way to the control plane: the controller protocol.
///
/// [`ControllerClient`] is the production implementation (HTTP, outbound
/// from the backend's environment). The trait exists so tests can sever and
/// restore the connection ([`crate::testkit::SeverableLink`]) without
/// changing a line of the controller.
pub trait ControllerLink: Send + Sync + 'static {
    /// `POST /controller/v1/lease`.
    fn acquire_lease(
        &self,
        ttl_secs: Option<u64>,
    ) -> impl Future<Output = Result<LeaseView, ClientError>> + Send;
    /// `POST /controller/v1/lease/renew`.
    fn renew_lease(
        &self,
        epoch: u64,
        ttl_secs: Option<u64>,
    ) -> impl Future<Output = Result<LeaseView, ClientError>> + Send;
    /// `POST /controller/v1/lease/release`.
    fn release_lease(
        &self,
        epoch: u64,
    ) -> impl Future<Output = Result<LeaseView, ClientError>> + Send;
    /// `POST /controller/v1/heartbeat`.
    fn heartbeat(&self) -> impl Future<Output = Result<HeartbeatResponse, ClientError>> + Send;
    /// `GET /controller/v1/commands`.
    fn commands(
        &self,
        wait_secs: u64,
        all: bool,
    ) -> impl Future<Output = Result<CommandsResponse, ClientError>> + Send;
    /// `POST /controller/v1/acks`.
    fn acknowledge(
        &self,
        acks: Vec<CommandAck>,
    ) -> impl Future<Output = Result<AckResponse, ClientError>> + Send;
    /// `POST /controller/v1/reports`.
    fn report(
        &self,
        report: &ReportRequest,
    ) -> impl Future<Output = Result<ReportResponse, ClientError>> + Send;
    /// `POST /controller/v1/tasks/:task_id/runner-credentials`.
    fn issue_runner_credential(
        &self,
        task_id: &str,
        request: &RunnerCredentialRequest,
    ) -> impl Future<Output = Result<RunnerCredentialResponse, ClientError>> + Send;
}

/// Timing and policy of a controller.
#[derive(Debug, Clone)]
pub struct ControllerConfig {
    /// The control-plane URL handed to runners. Not necessarily the one the
    /// controller itself uses: a runner reaches the control plane from
    /// inside the workload's network.
    pub runner_control_plane_url: String,
    /// Validity asked for the lease; renewed at a third of it.
    pub lease_ttl: Duration,
    /// Longest wait for new commands while nothing is in progress — also
    /// how often a running workload is observed.
    pub poll_wait: Duration,
    /// Pause between steps while something is in progress.
    pub settle_interval: Duration,
    /// Pause before retrying an unreachable control plane or a lease held
    /// by another controller.
    pub retry_interval: Duration,
    /// Least time between heartbeats.
    pub heartbeat_interval: Duration,
    /// Consecutive failed observations of a task that should be running
    /// before it is reported Unknown (R13). At least 1.
    pub observe_failures_before_unknown: u32,
}

impl ControllerConfig {
    /// The default policy, handing runners `runner_control_plane_url`.
    pub fn new(runner_control_plane_url: impl Into<String>) -> Self {
        Self {
            runner_control_plane_url: runner_control_plane_url.into(),
            lease_ttl: Duration::from_secs(60),
            poll_wait: Duration::from_secs(10),
            settle_interval: Duration::from_millis(500),
            retry_interval: Duration::from_secs(2),
            heartbeat_interval: Duration::from_secs(15),
            observe_failures_before_unknown: 3,
        }
    }

    fn lease_ttl_secs(&self) -> u64 {
        self.lease_ttl.as_secs().max(1)
    }

    fn renew_after(&self) -> Duration {
        Duration::from_secs(self.lease_ttl_secs()) / 3
    }
}

/// What one [`Controller::step`] ended in, which decides how long to wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// Every task is where it should be; wait for new commands.
    Idle,
    /// Something is in progress; look again soon.
    Settling,
    /// Another controller holds the lease (or this identity cannot act);
    /// nothing was touched.
    Standby,
    /// The control plane could not be reached; nothing was inferred.
    Disconnected,
}

/// Where the controller stands, for operators and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerStatus {
    /// Not holding the lease.
    Standby,
    /// Holding the lease at this epoch.
    Active {
        /// Fencing epoch.
        epoch: u64,
    },
}

/// Why a step stopped early. Neither says anything about workloads.
#[derive(Debug)]
enum Interrupt {
    /// The control plane is unreachable.
    Disconnected(String),
    /// The lease is gone (taken over, expired and fenced, revoked).
    Fenced(String),
}

/// A protocol call that did not succeed.
enum CallError {
    Interrupt(Interrupt),
    /// The control plane refused: the desired state moved on, re-read it.
    Refused(String),
}

impl From<ClientError> for CallError {
    fn from(error: ClientError) -> Self {
        match error {
            ClientError::Unavailable(message) | ClientError::Server(message) => {
                Self::Interrupt(Interrupt::Disconnected(message))
            }
            // A revoked identity can no longer act; it must not touch the
            // backend any more than a fenced one.
            ClientError::Unauthorized | ClientError::Forbidden | ClientError::Credential(_) => {
                Self::Interrupt(Interrupt::Fenced("controller credential refused".into()))
            }
            ClientError::Refused { code, message } if LEASE_CODES.contains(&code.as_str()) => {
                Self::Interrupt(Interrupt::Fenced(format!("{code}: {message}")))
            }
            ClientError::Refused { code, message }
            | ClientError::Request { code, message }
            | ClientError::Degraded { code, message } => {
                Self::Refused(format!("{code}: {message}"))
            }
            ClientError::Conflict => Self::Refused("conflict".into()),
            ClientError::NotFound => Self::Refused("task not found for this backend".into()),
        }
    }
}

/// How one task's reconciliation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Progress {
    Settled,
    Settling,
}

/// The lease this controller holds.
#[derive(Debug, Clone, Copy)]
struct HeldLease {
    epoch: u64,
    renewed_at: Instant,
    heartbeat_at: Instant,
}

/// A generic controller driving one backend.
pub struct Controller<B, L> {
    backend: B,
    link: L,
    config: ControllerConfig,
    lease: Option<HeldLease>,
    /// Generations already acknowledged, to avoid resending them.
    acknowledged: HashMap<TaskId, u64>,
    /// Consecutive failed observations per task.
    observe_failures: HashMap<TaskId, u32>,
    /// The last status report per task, so an unchanged one is not resent.
    last_status: HashMap<TaskId, (u64, ObservedSummary, Option<ResourceIdentity>)>,
    /// Tasks whose runner was seen gone during a pause while the desired
    /// state did not yet show its safe point: judged again on a desired
    /// state read after that observation.
    exited_before_ack: HashSet<TaskId>,
    /// Tasks whose loss confirmation the control plane refused (one that
    /// predates `LossConfirmed`): said once at warn, then only at debug.
    loss_confirmation_refused: OncePerTask,
}

impl<B: ExecutionBackend, L: ControllerLink> Controller<B, L> {
    /// A controller for `backend` talking to the control plane over `link`.
    pub fn new(backend: B, link: L, config: ControllerConfig) -> Self {
        Self {
            backend,
            link,
            config,
            lease: None,
            acknowledged: HashMap::new(),
            observe_failures: HashMap::new(),
            last_status: HashMap::new(),
            exited_before_ack: HashSet::new(),
            loss_confirmation_refused: OncePerTask::default(),
        }
    }

    /// The backend it drives.
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Whether it holds the lease (as far as it knows).
    pub fn status(&self) -> ControllerStatus {
        match self.lease {
            Some(lease) => ControllerStatus::Active { epoch: lease.epoch },
            None => ControllerStatus::Standby,
        }
    }

    /// Runs until `shutdown` resolves, then gives the lease up so another
    /// controller can take over at once. Dropping (aborting) the future
    /// instead models a crash: the lease then lapses on its own.
    pub async fn run(mut self, shutdown: impl Future<Output = ()> + Send) {
        tokio::pin!(shutdown);
        loop {
            let outcome = tokio::select! {
                _ = &mut shutdown => break,
                outcome = self.step() => outcome,
            };
            tokio::select! {
                _ = &mut shutdown => break,
                _ = self.pause(outcome) => {}
            }
        }
        if let Some(lease) = self.lease.take() {
            if let Err(error) = self.link.release_lease(lease.epoch).await {
                tracing::debug!(%error, "controller: could not release the lease");
            }
        }
    }

    /// One reconciliation pass over every task of the backend.
    pub async fn step(&mut self) -> StepOutcome {
        match self.try_step().await {
            Ok(Progress::Settled) => StepOutcome::Idle,
            Ok(Progress::Settling) => StepOutcome::Settling,
            Err(Interrupt::Disconnected(reason)) => {
                tracing::warn!(%reason, "controller: control plane unreachable; holding still");
                StepOutcome::Disconnected
            }
            Err(Interrupt::Fenced(reason)) => {
                if self.lease.take().is_some() {
                    tracing::warn!(%reason, "controller: lease lost; standing by");
                }
                self.forget_all();
                StepOutcome::Standby
            }
        }
    }

    /// The wait after a step.
    async fn pause(&self, outcome: StepOutcome) {
        match outcome {
            StepOutcome::Settling => tokio::time::sleep(self.config.settle_interval).await,
            StepOutcome::Standby | StepOutcome::Disconnected => {
                tokio::time::sleep(self.config.retry_interval).await
            }
            StepOutcome::Idle => {
                // Never wait past the next renewal.
                let until_renewal = self.lease.map_or(Duration::ZERO, |lease| {
                    (lease.renewed_at + self.config.renew_after())
                        .saturating_duration_since(Instant::now())
                });
                let wait = self.config.poll_wait.min(until_renewal);
                let wait_secs = wait.as_secs();
                if wait_secs == 0 {
                    tokio::time::sleep(wait.max(self.config.settle_interval)).await;
                } else if let Err(error) = self.link.commands(wait_secs, false).await {
                    // Whatever it was, the next step finds out properly.
                    tracing::debug!(%error, "controller: long-poll failed");
                    tokio::time::sleep(self.config.settle_interval).await;
                }
            }
        }
    }

    async fn try_step(&mut self) -> Result<Progress, Interrupt> {
        let epoch = self.ensure_lease().await?;
        let response = self.call(self.link.commands(0, true)).await?;
        let Ok(response) = response else {
            return Ok(Progress::Settling);
        };
        if response.lease_epoch != epoch {
            return Err(Interrupt::Fenced(format!(
                "commands were read under epoch {}, not {epoch}",
                response.lease_epoch
            )));
        }

        // Archived tasks drop out of the desired state; so does what this
        // controller remembered about them.
        let live: HashSet<&TaskId> = response.commands.iter().map(|c| &c.task_id).collect();
        self.acknowledged.retain(|task, _| live.contains(task));
        self.observe_failures.retain(|task, _| live.contains(task));
        self.last_status.retain(|task, _| live.contains(task));
        self.exited_before_ack.retain(|task| live.contains(task));
        self.acknowledge(&response.commands).await?;

        let mut progress = Progress::Settled;
        for command in &response.commands {
            if self.reconcile(epoch, command).await? == Progress::Settling {
                progress = Progress::Settling;
            }
        }
        Ok(progress)
    }

    /// Holds the lease: acquires, renews or heartbeats as due.
    async fn ensure_lease(&mut self) -> Result<u64, Interrupt> {
        let now = Instant::now();
        if let Some(mut lease) = self.lease {
            if now.duration_since(lease.renewed_at) < self.config.renew_after() {
                if now.duration_since(lease.heartbeat_at) >= self.config.heartbeat_interval {
                    self.call(self.link.heartbeat()).await?.ok();
                    lease.heartbeat_at = now;
                    self.lease = Some(lease);
                }
                return Ok(lease.epoch);
            }
            let ttl = Some(self.config.lease_ttl_secs());
            match self.link.renew_lease(lease.epoch, ttl).await {
                Ok(view) => {
                    self.lease = Some(HeldLease {
                        epoch: view.epoch,
                        renewed_at: now,
                        heartbeat_at: now,
                    });
                    return Ok(view.epoch);
                }
                Err(error) => match CallError::from(error) {
                    CallError::Interrupt(Interrupt::Fenced(reason)) => {
                        tracing::warn!(%reason, "controller: lease could not be renewed");
                        self.lease = None;
                        self.forget_all();
                    }
                    CallError::Interrupt(interrupt) => return Err(interrupt),
                    CallError::Refused(reason) => return Err(Interrupt::Fenced(reason)),
                },
            }
        }

        let view = match self
            .link
            .acquire_lease(Some(self.config.lease_ttl_secs()))
            .await
        {
            Ok(view) => view,
            Err(error) => {
                return Err(match CallError::from(error) {
                    CallError::Interrupt(interrupt) => interrupt,
                    CallError::Refused(reason) => Interrupt::Fenced(reason),
                })
            }
        };
        tracing::info!(
            epoch = view.epoch,
            "controller: lease acquired; reconciling every task"
        );
        self.forget_all();
        self.lease = Some(HeldLease {
            epoch: view.epoch,
            renewed_at: now,
            heartbeat_at: now,
        });
        Ok(view.epoch)
    }

    async fn acknowledge(&mut self, commands: &[DesiredCommand]) -> Result<(), Interrupt> {
        let acks: Vec<CommandAck> = commands
            .iter()
            .filter(|command| {
                self.acknowledged
                    .get(&command.task_id)
                    .is_none_or(|acked| *acked < command.generation)
            })
            .map(|command| CommandAck {
                task_id: command.task_id.to_string(),
                generation: command.generation,
            })
            .collect();
        if acks.is_empty() {
            return Ok(());
        }
        // A refused acknowledgement (a generation that moved on meanwhile)
        // is retried with the next read; nothing depends on it but wakeups.
        if self
            .call(self.link.acknowledge(acks.clone()))
            .await?
            .is_ok()
        {
            for ack in acks {
                if let Ok(task_id) = TaskId::parse(ack.task_id) {
                    self.acknowledged.insert(task_id, ack.generation);
                }
            }
        }
        Ok(())
    }

    /// Makes one task's backend resources match its desired action.
    async fn reconcile(
        &mut self,
        epoch: u64,
        command: &DesiredCommand,
    ) -> Result<Progress, Interrupt> {
        let task = match self
            .backend
            .resources(&command.task_id, &command.resource_key)
        {
            Ok(task) => task,
            Err(error) => {
                tracing::error!(task = %command.task_id, %error, "controller: cannot name the task's resources");
                return Ok(Progress::Settled);
            }
        };
        let context = TaskContext {
            epoch,
            command,
            task,
        };
        match command.action {
            DesiredAction::Start => self.start(&context).await,
            DesiredAction::Provision => self.provision(&context).await,
            DesiredAction::Run => self.keep_running(&context).await,
            DesiredAction::PauseAtSafePoint => self.pause_at_safe_point(&context).await,
            DesiredAction::KeepPaused => self.keep_paused(&context).await,
            // Unknown: is the runner really gone? Nothing here creates or
            // removes anything; a confirmed loss only makes the task
            // Interrupted.
            DesiredAction::Hold if command.runner_state == RunnerState::Unknown => {
                self.confirm_loss(&context).await
            }
            // Interrupted: the owner or the resume policy decides (R13).
            DesiredAction::Hold => Ok(Progress::Settled),
            DesiredAction::Archive => self.archive(&context).await,
        }
    }

    /// Queued: take the start over — and create nothing before the control
    /// plane accepted that, so an archive racing the start cannot leak a
    /// resource.
    async fn start(&mut self, context: &TaskContext<'_>) -> Result<Progress, Interrupt> {
        let summary = ObservedSummary {
            detail: Some("start accepted".into()),
            ..ObservedSummary::default()
        };
        let accepted = self
            .report(context, Observation::StartAccepted, None, summary)
            .await?;
        match accepted {
            Some(response) if response.runner_state == RunnerState::Provisioning => {
                let provisioning = DesiredCommand {
                    runner_state: RunnerState::Provisioning,
                    action: DesiredAction::Provision,
                    ..context.command.clone()
                };
                let next = TaskContext {
                    command: &provisioning,
                    ..context.clone()
                };
                self.provision(&next).await
            }
            _ => Ok(Progress::Settling),
        }
    }

    /// Provisioning: reconcile the task's workload by identity, create one
    /// only when none exists, report the runner ready once it reported.
    async fn provision(&mut self, context: &TaskContext<'_>) -> Result<Progress, Interrupt> {
        let Some(observed) = self.observe(context, true).await? else {
            return Ok(Progress::Settling);
        };
        let generation = context.command.generation;
        match &observed.workload {
            Some(workload) if workload.launch_generation == generation => {
                if !workload.phase.is_alive() {
                    let detail = format!(
                        "the runner's workload {} before it was ready",
                        workload.phase
                    );
                    return self.lost(context, &observed, true, detail).await;
                }
                if runner_reported(context.command, workload) {
                    self.report(
                        context,
                        Observation::RunnerReady,
                        Some(workload),
                        summary(&observed, "runner ready"),
                    )
                    .await?;
                    return Ok(Progress::Settled);
                }
                self.report_status(context, &observed, "waiting for the runner")
                    .await?;
                Ok(Progress::Settling)
            }
            Some(workload) if workload.launch_generation < generation => {
                // A superseded launch (a dead or paused incarnation): it goes
                // first, so a task never has two workloads (R7). Removal is
                // idempotent and repeated until the workload is gone, so a
                // deletion that stalls is pushed again rather than waited on.
                self.release(context).await;
                Ok(Progress::Settling)
            }
            // Launched for a generation newer than this view: re-read.
            Some(_) => Ok(Progress::Settling),
            None => self.launch(context, &observed).await,
        }
    }

    /// Creates the workload of the current generation.
    async fn launch(
        &mut self,
        context: &TaskContext<'_>,
        observed: &TaskObservation,
    ) -> Result<Progress, Interrupt> {
        let command = context.command;
        validate_launch_input(&command.launch_spec).map_err(Interrupt::Fenced)?;
        let launch_spec = command.launch_spec.clone();
        match observed.storage {
            crate::backend::StorageState::Deleting => return Ok(Progress::Settling),
            crate::backend::StorageState::Absent
                if command.generation > 1 && command.runner.is_some() =>
            {
                // A runner worked on storage that is gone: a fresh one would
                // silently restart the task from nothing (R13).
                let detail = "task storage is missing; the task's work cannot be continued";
                return self.lost(context, observed, false, detail.into()).await;
            }
            _ => {}
        }

        let incarnation = format!(
            "{}-g{}-{}",
            context.task.workload,
            command.generation,
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );
        let issued = self
            .call(self.link.issue_runner_credential(
                command.task_id.as_str(),
                &RunnerCredentialRequest {
                    lease_epoch: context.epoch,
                    generation: command.generation,
                    incarnation: incarnation.clone(),
                },
            ))
            .await?;
        let Ok(issued) = issued else {
            return Ok(Progress::Settling);
        };
        let launch = WorkloadLaunch {
            task_id: command.task_id.clone(),
            generation: command.generation,
            incarnation,
            lease_epoch: context.epoch,
            launch_spec,
            control_plane_url: self.config.runner_control_plane_url.clone(),
            credential: RunnerCredential::new(issued.credential),
        };
        let created = if observed.storage.exists() {
            self.backend.resume(&context.task, &launch).await
        } else {
            self.backend.provision(&context.task, &launch).await
        };
        match created {
            Ok(provisioned) => {
                let observed = TaskObservation {
                    workload: Some(provisioned.workload.clone()),
                    storage: crate::backend::StorageState::Present,
                };
                self.report_status(context, &observed, "workload created")
                    .await?;
                Ok(Progress::Settling)
            }
            Err(error) => self.launch_failed(context, observed, error).await,
        }
    }

    async fn launch_failed(
        &mut self,
        context: &TaskContext<'_>,
        observed: &TaskObservation,
        error: BackendError,
    ) -> Result<Progress, Interrupt> {
        match error.kind {
            // Retried from a fresh observation: if the create did happen,
            // the next step finds the workload by its name.
            BackendErrorKind::Transient => {
                tracing::warn!(task = %context.command.task_id, %error, "controller: launch failed; retrying");
                Ok(Progress::Settling)
            }
            BackendErrorKind::Rejected => {
                let detail = format!("the backend refused the workload: {}", error.message);
                self.lost(context, observed, true, detail).await
            }
            BackendErrorKind::StorageMissing => {
                self.lost(context, observed, false, error.message).await
            }
        }
    }

    /// Running: observe; a workload that is gone or dead is reported lost.
    /// It is never replaced here (R13): a replacement needs a new desired
    /// generation.
    async fn keep_running(&mut self, context: &TaskContext<'_>) -> Result<Progress, Interrupt> {
        let Some(observed) = self.observe(context, true).await? else {
            return Ok(Progress::Settling);
        };
        match &observed.workload {
            Some(workload)
                if workload.phase.is_alive()
                    && workload.launch_generation == context.command.generation =>
            {
                self.report_status(context, &observed, "running").await?;
                Ok(Progress::Settled)
            }
            Some(workload) => {
                let detail = format!("the runner's workload {}", workload.phase);
                self.lost(context, &observed, true, detail).await
            }
            None => {
                let detail = "the runner's workload disappeared".to_string();
                self.lost(context, &observed, true, detail).await
            }
        }
    }

    /// PauseRequested: wait for the runner's safe point, then remove only
    /// the compute and report the pause once it is gone (KTD2).
    async fn pause_at_safe_point(
        &mut self,
        context: &TaskContext<'_>,
    ) -> Result<Progress, Interrupt> {
        let acknowledged = context
            .command
            .runner
            .as_ref()
            .is_some_and(|runner| runner.safe_point_acknowledged);
        let Some(observed) = self.observe(context, true).await? else {
            return Ok(Progress::Settling);
        };
        let task_id = &context.command.task_id;
        if acknowledged {
            self.exited_before_ack.remove(task_id);
            if observed.workload.is_some()
                && !self
                    .release(context)
                    .await
                    .is_some_and(Removal::is_complete)
            {
                return Ok(Progress::Settling);
            }
            let paused = TaskObservation {
                workload: None,
                storage: observed.storage,
            };
            self.report(
                context,
                Observation::PausedAtSafePoint,
                None,
                summary(&paused, "paused at a safe point"),
            )
            .await?;
            return Ok(Progress::Settled);
        }
        match &observed.workload {
            // Still working towards the safe point.
            Some(workload) if workload.phase.is_alive() => Ok(Progress::Settling),
            // A runner exits 0 during a pause only after the control plane
            // accepted its safe point: this view of it is just older.
            Some(workload) if workload.phase == (WorkloadPhase::Exited { code: Some(0) }) => {
                Ok(Progress::Settling)
            }
            // A runner acknowledges its safe point and then exits, but the
            // desired state in hand was read before this observation: a
            // runner that is not this instance's child exits with no code
            // to tell, so a newer desired state decides. Only a gone runner
            // still unacknowledged after that is lost.
            Some(workload)
                if workload.phase == (WorkloadPhase::Exited { code: None })
                    && self.exited_before_ack.insert(task_id.clone()) =>
            {
                Ok(Progress::Settling)
            }
            Some(workload) => {
                let detail = format!(
                    "the runner's workload {} before a safe point",
                    workload.phase
                );
                self.lost(context, &observed, true, detail).await
            }
            None => {
                let detail = "the runner's workload disappeared before a safe point".to_string();
                self.lost(context, &observed, true, detail).await
            }
        }
    }

    /// Unknown: observe again, at the idle pace. Only a workload that is
    /// gone or terminated, with the task's storage still present, is a
    /// confirmed loss (→ Interrupted, which the resume policy may act on).
    ///
    /// Everything else leaves the task Unknown, with nothing reported: a
    /// live workload (a runner that may still work must not get a second
    /// one on its storage), one still being deleted (its node may not have
    /// let go of it yet), a failed observation (no second `WorkloadLost`),
    /// and storage that is gone or going (a resume could only end in
    /// another storage loss).
    async fn confirm_loss(&mut self, context: &TaskContext<'_>) -> Result<Progress, Interrupt> {
        let Some(observed) = self.observe(context, false).await? else {
            return Ok(Progress::Settled);
        };
        if observed.storage != crate::backend::StorageState::Present {
            return Ok(Progress::Settled);
        }
        let detail = match &observed.workload {
            None => "the runner's workload is gone".to_string(),
            Some(workload) if matches!(workload.phase, WorkloadPhase::Exited { .. }) => {
                format!("the runner's workload {}", workload.phase)
            }
            Some(_) => return Ok(Progress::Settled),
        };
        let task_id = &context.command.task_id;
        if self.loss_confirmation_refused.seen(task_id) {
            tracing::debug!(task = %task_id, %detail, "controller: loss confirmed again");
        } else {
            tracing::warn!(task = %task_id, %detail, "controller: loss confirmed");
        }
        let accepted = self
            .report(
                context,
                Observation::LossConfirmed,
                observed.workload.as_ref(),
                summary(&observed, &detail),
            )
            .await?;
        if accepted.is_some() {
            self.loss_confirmation_refused.forget(task_id);
        } else if self.loss_confirmation_refused.first(task_id) {
            tracing::warn!(
                task = %task_id,
                "controller: the control plane refused the loss confirmation (an older \
                 control plane?); the task stays unknown until its owner acts"
            );
        }
        Ok(Progress::Settled)
    }

    /// Stopped: no compute may exist; storage stays.
    async fn keep_paused(&mut self, context: &TaskContext<'_>) -> Result<Progress, Interrupt> {
        let Some(observed) = self.observe(context, false).await? else {
            return Ok(Progress::Settling);
        };
        if observed.workload.is_none() {
            return Ok(Progress::Settled);
        }
        Ok(match self.release(context).await {
            Some(removal) if removal.is_complete() => Progress::Settled,
            _ => Progress::Settling,
        })
    }

    /// Archiving: delete workload and storage; report once both are gone.
    async fn archive(&mut self, context: &TaskContext<'_>) -> Result<Progress, Interrupt> {
        match self.backend.archive(&context.task).await {
            Ok(progress) if progress.is_complete() => {
                let gone = TaskObservation {
                    workload: None,
                    storage: crate::backend::StorageState::Absent,
                };
                self.report(
                    context,
                    Observation::ResourcesDeleted,
                    None,
                    summary(&gone, "workload and storage deleted"),
                )
                .await?;
                Ok(Progress::Settled)
            }
            Ok(_) => Ok(Progress::Settling),
            Err(error) => {
                // Visible to the owner instead of a silent retry loop: a
                // refused deletion is a cleanup failure, not a success.
                tracing::warn!(task = %context.command.task_id, %error, "controller: archive failed");
                let failed = TaskObservation {
                    workload: None,
                    storage: crate::backend::StorageState::Present,
                };
                let detail = format!("archive incomplete: {}", error.message);
                self.report_status(context, &failed, &detail).await?;
                Ok(Progress::Settling)
            }
        }
    }

    /// Observes the task; `None` after a failure. A task that should have a
    /// live workload (`expects_runner`) is reported Unknown once failures
    /// persist, and its step then counts as settled.
    async fn observe(
        &mut self,
        context: &TaskContext<'_>,
        expects_runner: bool,
    ) -> Result<Option<TaskObservation>, Interrupt> {
        let task_id = &context.command.task_id;
        match self.backend.observe(&context.task).await {
            Ok(observed) => {
                self.observe_failures.remove(task_id);
                Ok(Some(observed))
            }
            Err(error) => {
                let failures = self.observe_failures.entry(task_id.clone()).or_insert(0);
                *failures += 1;
                let failures = *failures;
                tracing::warn!(task = %task_id, %error, failures, "controller: observation failed");
                if expects_runner && failures >= self.config.observe_failures_before_unknown.max(1)
                {
                    // Nothing is known about either resource; the detail
                    // says so rather than the presence flags.
                    let summary = ObservedSummary {
                        detail: Some(format!("the backend cannot be observed: {}", error.message)),
                        ..ObservedSummary::default()
                    };
                    self.report(
                        context,
                        Observation::WorkloadLost { state_known: false },
                        None,
                        summary,
                    )
                    .await?;
                    self.observe_failures.remove(task_id);
                }
                Ok(None)
            }
        }
    }

    /// Removes the task's compute; `None` when the backend failed.
    async fn release(&self, context: &TaskContext<'_>) -> Option<Removal> {
        match self.backend.release_compute(&context.task).await {
            Ok(removal) => Some(removal),
            Err(error) => {
                tracing::warn!(task = %context.command.task_id, %error, "controller: removing compute failed");
                None
            }
        }
    }

    /// Reports an unexpected loss; the task then waits for the control
    /// plane's resume policy or its owner (D58).
    async fn lost(
        &mut self,
        context: &TaskContext<'_>,
        observed: &TaskObservation,
        state_known: bool,
        detail: String,
    ) -> Result<Progress, Interrupt> {
        tracing::warn!(task = %context.command.task_id, %detail, "controller: workload lost");
        self.report(
            context,
            Observation::WorkloadLost { state_known },
            observed.workload.as_ref(),
            summary(observed, &detail),
        )
        .await?;
        Ok(Progress::Settled)
    }

    /// Reports a status refresh, unless it says what the last one said.
    async fn report_status(
        &mut self,
        context: &TaskContext<'_>,
        observed: &TaskObservation,
        detail: &str,
    ) -> Result<(), Interrupt> {
        let summary = summary(observed, detail);
        let resource = observed.workload.as_ref().map(resource_identity);
        let key = (context.command.generation, summary.clone(), resource);
        if self.last_status.get(&context.command.task_id) == Some(&key) {
            return Ok(());
        }
        if self
            .report(
                context,
                Observation::Status,
                observed.workload.as_ref(),
                summary,
            )
            .await?
            .is_some()
        {
            self.last_status
                .insert(context.command.task_id.clone(), key);
        }
        Ok(())
    }

    /// Sends one report; `None` when the control plane refused it (the
    /// desired state moved on and the next step re-reads it).
    async fn report(
        &mut self,
        context: &TaskContext<'_>,
        observation: Observation,
        workload: Option<&WorkloadState>,
        summary: ObservedSummary,
    ) -> Result<Option<ReportResponse>, Interrupt> {
        let request = ReportRequest {
            task_id: context.command.task_id.to_string(),
            lease_epoch: context.epoch,
            desired_generation: context.command.generation,
            observation,
            resource: workload.map(resource_identity),
            summary,
            observed_at: Utc::now(),
        };
        let response = self.call(self.link.report(&request)).await?;
        if let Err(reason) = &response {
            tracing::debug!(task = %context.command.task_id, ?observation, %reason, "controller: report refused");
        }
        Ok(response.ok())
    }

    /// Runs a protocol call: interrupts propagate, refusals are returned.
    async fn call<T>(
        &self,
        call: impl Future<Output = Result<T, ClientError>>,
    ) -> Result<Result<T, String>, Interrupt> {
        match call.await {
            Ok(value) => Ok(Ok(value)),
            Err(error) => match CallError::from(error) {
                CallError::Interrupt(interrupt) => Err(interrupt),
                CallError::Refused(reason) => Ok(Err(reason)),
            },
        }
    }

    /// Forgets everything remembered between steps: after a lease change the
    /// next step starts from the control plane and the backend alone.
    fn forget_all(&mut self) {
        self.acknowledged.clear();
        self.observe_failures.clear();
        self.last_status.clear();
        self.loss_confirmation_refused.clear();
    }
}

/// Tasks something was already said about, so a log line repeated every
/// step is said loudly only once per task.
#[derive(Debug, Default)]
struct OncePerTask(HashSet<TaskId>);

impl OncePerTask {
    /// True the first time `task_id` is noted (until forgotten).
    fn first(&mut self, task_id: &TaskId) -> bool {
        self.0.insert(task_id.clone())
    }

    /// Whether `task_id` was noted.
    fn seen(&self, task_id: &TaskId) -> bool {
        self.0.contains(task_id)
    }

    /// Lets the next note of `task_id` be the first again.
    fn forget(&mut self, task_id: &TaskId) {
        self.0.remove(task_id);
    }

    fn clear(&mut self) {
        self.0.clear();
    }
}

/// One task's reconciliation inputs.
#[derive(Clone)]
struct TaskContext<'a> {
    epoch: u64,
    command: &'a DesiredCommand,
    task: TaskResources,
}

/// Whether the workload's own runner incarnation reported to the control
/// plane — "the runner is up".
fn runner_reported(command: &DesiredCommand, workload: &WorkloadState) -> bool {
    command.runner.as_ref().is_some_and(|runner| {
        runner.incarnation == workload.incarnation && runner.launch_generation == command.generation
    })
}

fn resource_identity(workload: &WorkloadState) -> ResourceIdentity {
    ResourceIdentity {
        uid: workload.uid.clone(),
        resource_generation: workload.resource_generation,
    }
}

/// The normalized, platform-free summary of an observation (KTD4).
fn summary(observed: &TaskObservation, detail: &str) -> ObservedSummary {
    ObservedSummary {
        workload_present: observed.workload.is_some(),
        storage_present: observed.storage.exists(),
        runner_status: None,
        detail: Some(detail.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_is_said_loudly_once_per_task_until_accepted() {
        let mut said = OncePerTask::default();
        let task = TaskId::parse("task-1").unwrap();
        let other = TaskId::parse("task-2").unwrap();

        assert!(!said.seen(&task));
        assert!(said.first(&task), "the first refusal warns");
        assert!(said.seen(&task));
        assert!(!said.first(&task), "a repeated one does not");
        assert!(said.first(&other), "another task warns on its own");

        said.forget(&task);
        assert!(said.first(&task), "after an accepted report it warns again");
        said.clear();
        assert!(!said.seen(&other));
    }
}
