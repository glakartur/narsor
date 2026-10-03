//! The task runner: the existing Engine, run against one portable task
//! storage, reporting to a control plane it does not depend on (U3, KTD5).
//!
//! A runner is what a backend starts inside a task's isolated workload
//! (`narsor runner …`, see [`cli`]). It owns exactly one task storage
//! ([`storage`]) and exactly one execution on it, and it is the only thing
//! that ever writes that execution. The control plane never sees the files;
//! it sees [`RunnerSnapshot`]s — cursors and summaries — and relays owner
//! input and conversation pages through the runner protocol (KTD3, KTD4).
//!
//! # One run of a runner process
//!
//! 1. **Lock** the storage ([`storage::TaskStorage::lock`]).
//! 2. **Read the launch spec** — the mounted file the controller hands in,
//!    or the copy on storage — and its workflow definition snapshot. The
//!    definition is never looked up again anywhere (R18).
//! 3. **Check the base image** against its contract ([`preflight`], R22)
//!    and make the mounted secrets reachable ([`secrets`], R19). Either
//!    failing stops the runner before any work with a diagnosable
//!    [`RunnerProblem`] reported to the control plane.
//! 4. **Initialize** the storage once ([`init`], R39): copy the spec, clone
//!    the pinned commit into empty storage, write the marker. A resumed
//!    storage is never cloned again.
//! 5. **Decide how to continue** (below), then run the Engine with the
//!    harness pinned in the launch spec (R42) while a control loop reports
//!    snapshots, long-polls for directives, input, page requests and
//!    materials (U9), and renews the credential.
//!
//! # Unexpected loss needs an explicit owner decision (R13)
//!
//! Ordinary local recovery ([`crate::persistence::reconcile`]) turns an
//! action that was in flight into `Unknown`, and an explicit recovery then
//! starts that action again. Neither may happen just because a workload came
//! back: the owner decides. The runner tells the two situations apart from
//! its own storage and learns the decision from the control plane:
//!
//! * every incarnation writes an open [`storage::SessionRecord`] when it
//!   starts working and closes it only when it stops at a safe point. An
//!   open record found at start-up is the storage's testimony that the
//!   previous incarnation stopped unexpectedly;
//! * the control plane tells a runner, with every poll and snapshot answer,
//!   which owner command launched it
//!   ([`crate::controller_protocol::RunnerLaunchView::authorized_by`]). A
//!   controller never relaunches a lost workload on its own (KTD2), so a
//!   new incarnation launched at a newer generation by an owner `start` or
//!   `resume` *is* the owner's decision, recorded where only the owner can
//!   write it.
//!
//! So after an unclean stop the runner touches nothing until it reaches the
//! control plane. Authorized, it reconciles, and when an action was in
//! flight it starts that action again through an ordinary explicit recovery
//! (D24's idempotency key changes with the new attempt). Not authorized —
//! the same incarnation restarted, or nothing says the owner decided — it
//! reports [`RunnerProblem`] `owner_decision_required` and exits without
//! having changed a byte of work state. A clean stop (pause at a safe point)
//! needs no decision: the resumed incarnation simply continues.
//!
//! # Offline is normal (R5, R14)
//!
//! Work never waits for the control plane, except for that one decision.
//! Reports that fail are not queued for replay: the next attempt builds a
//! fresh snapshot from storage, so a reconnect reports the latest cursor
//! once, and nothing is ever re-executed to produce it.

pub mod cli;
pub mod init;
mod materials;
pub mod preflight;
pub mod secrets;
mod snapshot;
pub mod storage;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use chrono::Utc;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::api::{ClientError, ConversationEntryDto, ConversationResponse};
use crate::controller_protocol::{
    ConversationPageRequest, DeliveredInput, RunnerClient, RunnerDirective, RunnerLaunchView,
    RunnerPollQuery, RunnerPollResponse,
};
use crate::domain::{
    ExecutionStatus, LaunchDefinitionError, RepositoryInfo, TaskId, TaskLaunchSpec, TaskWorkspace,
    WorkflowDefinition, WorkflowExecution, Workspace,
};
use crate::engine::{AgentDefaults, Engine, EngineError, Recovery};
use crate::orchestration::{OwnerCommand, RunnerProblem, RunnerSnapshot};
use crate::persistence::{reconcile, PersistenceError, ReconciliationOutcome};
use crate::workspace::WorkspaceError;

pub use init::{initialize, Initialization};
pub use preflight::{BaseImageContract, ContractViolation};
pub use secrets::{MountedSecrets, ProcessEnvironment};
pub use storage::{SessionRecord, TaskStorage};

/// Variable naming the directory of delivered materials (U9).
pub const MATERIALS_ENV: &str = "NARSOR_MATERIALS_DIR";
/// Variable naming the outbox, whose folders are published to the owner.
pub const OUTBOX_ENV: &str = "NARSOR_OUTBOX_DIR";

/// Problem code: the base image breaks the runner contract (R22).
pub const PROBLEM_BASE_IMAGE: &str = "base_image_contract";
/// Problem code: the launch spec or its definition snapshot is unusable.
pub const PROBLEM_LAUNCH_SPEC: &str = "launch_spec_invalid";
/// Problem code: a referenced secret is not mounted or not deliverable.
pub const PROBLEM_SECRET: &str = "secret_unavailable";
/// Problem code: the storage could not be initialized (clone, marker).
pub const PROBLEM_INITIALIZATION: &str = "initialization_failed";
/// Problem code: the storage was left by an unexpected stop and no owner
/// decision authorizes this incarnation to recover it (R13).
pub const PROBLEM_OWNER_DECISION: &str = "owner_decision_required";

/// Everything that can stop a runner.
#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    /// A filesystem operation failed.
    #[error("I/O error on '{path}': {source}")]
    Io {
        /// Where.
        path: PathBuf,
        /// What the OS said.
        #[source]
        source: std::io::Error,
    },
    /// The Engine's store failed.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    /// The Engine refused.
    #[error(transparent)]
    Engine(#[from] EngineError),
    /// Cloning the workspace failed.
    #[error("could not clone the pinned workspace: {0}")]
    Workspace(#[from] WorkspaceError),
    /// Another runner holds the storage.
    #[error("task storage '{0}' is in use by another runner")]
    StorageBusy(PathBuf),
    /// The storage belongs to something else, or is not empty.
    #[error("task storage cannot be used: {0}")]
    ForeignStorage(String),
    /// A secret cannot be delivered.
    #[error("secret '{name}' cannot be delivered: {detail}")]
    Secret {
        /// The reference name.
        name: String,
        /// Why.
        detail: String,
    },
    /// No usable launch spec.
    #[error("no usable launch spec: {0}")]
    LaunchSpec(String),
    /// The definition snapshot is unusable.
    #[error(transparent)]
    Definition(#[from] LaunchDefinitionError),
    /// The storage holds more than the one execution a task has.
    #[error("task storage holds several executions ({0}); a task has exactly one")]
    SeveralExecutions(String),
}

impl RunnerError {
    pub(crate) fn io(path: impl AsRef<Path>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.as_ref().to_path_buf(),
            source,
        }
    }
}

/// How a runner process ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerExit {
    /// Stopped at an acknowledged safe point for a pause; compute may go.
    PausedAtSafePoint,
    /// The control plane said stop (paused or archived task).
    Stopped {
        /// Whether no work was in flight when it stopped.
        clean: bool,
    },
    /// It could not work; the problem was reported when the control plane
    /// was reachable.
    Problem(RunnerProblem),
}

/// Timing of the control channel; tests shorten it.
#[derive(Debug, Clone)]
pub struct RunnerTiming {
    /// Long-poll wait asked for (the server caps it).
    pub poll_wait: Duration,
    /// Shortest delay between attempts after a failure.
    pub retry_min: Duration,
    /// Longest delay between attempts after failures.
    pub retry_max: Duration,
    /// Least time between two change-driven snapshots.
    pub report_interval: Duration,
    /// A snapshot is sent at least this often, changed or not.
    pub heartbeat: Duration,
    /// Renew the credential this long before it expires.
    pub renew_margin: Duration,
    /// How long a runner that cannot work keeps trying to report why.
    pub problem_report_deadline: Duration,
}

impl Default for RunnerTiming {
    fn default() -> Self {
        Self {
            poll_wait: Duration::from_secs(25),
            retry_min: Duration::from_millis(500),
            retry_max: Duration::from_secs(30),
            report_interval: Duration::from_secs(1),
            heartbeat: Duration::from_secs(30),
            renew_margin: Duration::from_secs(5 * 60),
            problem_report_deadline: Duration::from_secs(120),
        }
    }
}

/// Everything a runner process is started with.
#[derive(Debug, Clone)]
pub struct RunnerConfig {
    /// The task this runner works for.
    pub task_id: TaskId,
    /// Where the task storage is mounted.
    pub storage: PathBuf,
    /// The launch spec file the controller mounted, if any; required on
    /// first start, optional once the storage holds its copy.
    pub launch_spec: Option<PathBuf>,
    /// The control plane's base URL.
    pub control_plane: String,
    /// The read-only file holding this incarnation's credential.
    pub credential_file: PathBuf,
    /// The read-only directory of mounted secrets, one file per reference.
    pub secrets_dir: Option<PathBuf>,
    /// This incarnation's identity, the one its credential is bound to.
    pub incarnation: String,
    /// Control-channel timing.
    pub timing: RunnerTiming,
    /// The base-image contract to check.
    pub contract: BaseImageContract,
}

/// Runs one runner process to its end.
///
/// `apply_environment` is called once, after the secrets are materialized
/// and before any work starts, with the variables every action's child
/// process must inherit (the private HOME, `env`-delivered secrets). It is
/// a callback because only the process's entry point may change its
/// environment; library code never does.
pub async fn run(
    config: RunnerConfig,
    apply_environment: impl FnOnce(&ProcessEnvironment),
) -> Result<RunnerExit, RunnerError> {
    let storage = TaskStorage::new(&config.storage);
    let _lock = storage.lock()?;
    let mut link = Link::new(&config);
    let store = storage.store();

    let spec = match load_spec(&config, &storage) {
        Ok(spec) => spec,
        Err(error) => {
            return Ok(report_problem(
                &storage,
                &mut link,
                &config.timing,
                PROBLEM_LAUNCH_SPEC,
                error,
            )
            .await)
        }
    };
    let definition = match spec.definition() {
        Ok(definition) => Arc::new(definition),
        Err(error) => {
            return Ok(report_problem(
                &storage,
                &mut link,
                &config.timing,
                PROBLEM_LAUNCH_SPEC,
                error,
            )
            .await)
        }
    };

    storage.prepare()?;
    let violations = config.contract.check(&storage.home_dir());
    if !violations.is_empty() {
        let message = format!(
            "the base image does not meet the runner contract: {}",
            violations
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        );
        return Ok(report_problem(
            &storage,
            &mut link,
            &config.timing,
            PROBLEM_BASE_IMAGE,
            message,
        )
        .await);
    }

    let secrets = MountedSecrets::new(config.secrets_dir.clone());
    let mut environment = match secrets.materialize(&spec.environment, &storage.home_dir()) {
        Ok(environment) => environment,
        Err(error) => {
            return Ok(
                report_problem(&storage, &mut link, &config.timing, PROBLEM_SECRET, error).await,
            )
        }
    };
    // Where the task finds its materials and leaves documents for its
    // owner (U9) — both on task storage, outside the workspace.
    environment.task.push((
        MATERIALS_ENV.to_string(),
        storage.materials_dir().display().to_string(),
    ));
    environment.task.push((
        OUTBOX_ENV.to_string(),
        storage.outbox_dir().display().to_string(),
    ));
    apply_environment(&environment);

    if let Err(error) = initialize(&storage, &store, &config.task_id, &spec, &secrets) {
        return Ok(report_problem(
            &storage,
            &mut link,
            &config.timing,
            PROBLEM_INITIALIZATION,
            error,
        )
        .await);
    }

    // The harness is the one pinned at task creation, never a live default
    // (R42): a later change of the global default reaches new tasks only.
    let engine = Arc::new(Engine::new(store).with_agent_defaults(AgentDefaults {
        harness: spec.environment.harness.clone(),
        timeout_secs: None,
    }));

    let mut session = Session {
        storage,
        engine,
        definition,
        incarnation: config.incarnation.clone(),
        timing: config.timing.clone(),
        link,
        cache: snapshot::ConversationCache::default(),
        execution_id: None,
        directive: RunnerDirective::Continue,
        pass: None,
        outbox_due: true,
        material_failures: 0,
        material_retry_at: None,
    };
    session.begin(&spec).await
}

/// Reads the mounted launch spec, or the storage's copy when none is
/// mounted.
fn load_spec(config: &RunnerConfig, storage: &TaskStorage) -> Result<TaskLaunchSpec, RunnerError> {
    match &config.launch_spec {
        Some(path) => {
            let text =
                std::fs::read_to_string(path).map_err(|source| RunnerError::io(path, source))?;
            let spec: TaskLaunchSpec = serde_json::from_str(&text)
                .map_err(|error| RunnerError::LaunchSpec(format!("{}: {error}", path.display())))?;
            if !spec.digest_is_valid() {
                return Err(RunnerError::LaunchSpec(format!(
                    "{}: the digest does not match the content",
                    path.display()
                )));
            }
            Ok(spec)
        }
        None => storage
            .store()
            .load_task_launch_spec(&config.task_id)?
            .ok_or_else(|| {
                RunnerError::LaunchSpec(
                    "none was mounted and the task storage was never initialized".to_string(),
                )
            }),
    }
}

/// Reports a problem that keeps the runner from working, for as long as
/// the deadline allows, and returns the matching exit.
///
/// The snapshot carries whatever the storage already holds — the
/// execution's real status and event cursor on a resumed storage — so the
/// report neither fabricates a state nor moves the cursor backwards.
async fn report_problem(
    storage: &TaskStorage,
    link: &mut Link,
    timing: &RunnerTiming,
    code: &str,
    detail: impl std::fmt::Display,
) -> RunnerExit {
    let problem = RunnerProblem {
        code: code.to_string(),
        message: detail.to_string(),
    };
    tracing::error!(code, message = %problem.message, "the runner cannot work");
    let store = storage.store();
    let existing = store
        .list_execution_ids()
        .ok()
        .and_then(|ids| ids.into_iter().next())
        .and_then(|id| store.load_execution(&id).ok());
    let event_cursor = existing
        .as_ref()
        .and_then(|execution| {
            store
                .event_log(&execution.id)
                .last_sequence()
                .ok()
                .flatten()
        })
        .unwrap_or(0)
        .max(storage.reported_cursor().unwrap_or(0));
    let snapshot = RunnerSnapshot {
        snapshot_sequence: storage.next_snapshot_sequence().unwrap_or(1),
        execution_status: existing
            .as_ref()
            .map_or(ExecutionStatus::Unknown, |execution| {
                execution.status.clone()
            }),
        execution_cursor: crate::orchestration::ExecutionCursor {
            execution_id: existing.as_ref().map(|execution| execution.id.clone()),
            current_state: existing.and_then(|execution| execution.current_state),
            action_execution_id: None,
        },
        event_cursor,
        pending_input: None,
        conversation: crate::orchestration::ConversationAvailability::Unavailable {
            reason: problem.message.clone(),
        },
        timeline: None,
        safe_point: true,
        problem: Some(problem.clone()),
        observed_at: Utc::now(),
    };
    send_until(link, timing, &snapshot).await;
    RunnerExit::Problem(problem)
}

/// Sends `snapshot` until it is accepted or the problem deadline passes.
async fn send_until(link: &mut Link, timing: &RunnerTiming, snapshot: &RunnerSnapshot) -> bool {
    let deadline = Instant::now() + timing.problem_report_deadline;
    loop {
        if let Some(client) = link.client() {
            match client.snapshot(snapshot).await {
                Ok(response) => {
                    link.succeeded();
                    link.launch = Some(response.launch);
                    return true;
                }
                Err(error) => link.failed(&error, timing),
            }
        } else {
            link.failed(&ClientError::Unauthorized, timing);
        }
        if link.next_attempt >= deadline {
            return false;
        }
        tokio::time::sleep_until(link.next_attempt).await;
    }
}

/// The control-plane side of one runner: credential, client and backoff.
struct Link {
    base_url: String,
    credential_file: PathBuf,
    client: Option<RunnerClient>,
    failures: u32,
    next_attempt: Instant,
    expires_at: Option<chrono::DateTime<Utc>>,
    launch: Option<RunnerLaunchView>,
}

impl Link {
    fn new(config: &RunnerConfig) -> Self {
        Self {
            base_url: config.control_plane.clone(),
            credential_file: config.credential_file.clone(),
            client: None,
            failures: 0,
            next_attempt: Instant::now(),
            expires_at: None,
            launch: None,
        }
    }

    /// The client, reading the credential file when there is none yet.
    ///
    /// The file is read again after every refusal, so a controller that
    /// remounts a reissued credential (after a long outage outlived the old
    /// one) is picked up without a restart. The value is held in memory
    /// only: never logged, never written anywhere.
    fn client(&mut self) -> Option<RunnerClient> {
        if self.client.is_none() {
            match std::fs::read_to_string(&self.credential_file) {
                Ok(credential) => {
                    self.client = Some(RunnerClient::new(&self.base_url, credential.trim()));
                }
                Err(error) => {
                    tracing::warn!(
                        path = %self.credential_file.display(),
                        %error,
                        "the runner credential cannot be read"
                    );
                }
            }
        }
        self.client.clone()
    }

    fn ready(&self) -> bool {
        Instant::now() >= self.next_attempt
    }

    fn succeeded(&mut self) {
        self.failures = 0;
        self.next_attempt = Instant::now();
    }

    fn failed(&mut self, error: &ClientError, timing: &RunnerTiming) {
        if matches!(error, ClientError::Unauthorized) {
            // Expired, superseded or revoked: the control plane does not
            // say which. Work goes on regardless (R14); the file is read
            // again in case a newer credential was mounted.
            self.client = None;
        }
        self.failures = self.failures.saturating_add(1);
        let factor = 2u32.saturating_pow(self.failures.saturating_sub(1).min(16));
        let delay = timing
            .retry_min
            .saturating_mul(factor)
            .min(timing.retry_max);
        self.next_attempt = Instant::now() + delay;
        tracing::debug!(%error, ?delay, "control plane unreachable or refusing; retrying later");
    }

    fn renew_due(&self, margin: Duration) -> bool {
        self.expires_at.is_some_and(|expires| {
            let margin = chrono::Duration::from_std(margin).unwrap_or_default();
            Utc::now() + margin >= expires
        })
    }
}

/// The work phase of one runner process.
struct Session {
    storage: TaskStorage,
    engine: Arc<Engine>,
    definition: Arc<WorkflowDefinition>,
    incarnation: String,
    timing: RunnerTiming,
    link: Link,
    cache: snapshot::ConversationCache,
    execution_id: Option<String>,
    directive: RunnerDirective,
    pass: Option<JoinHandle<Result<WorkflowExecution, EngineError>>>,
    /// Whether the outbox should be looked at: at start and after every
    /// engine pass, the only times the task can have left something there.
    outbox_due: bool,
    /// Consecutive material deliveries that failed, and when to try again:
    /// until then the poll defers materials (U9).
    material_failures: u32,
    material_retry_at: Option<Instant>,
}

/// How the work on this storage continues.
enum Continuation {
    /// No execution exists yet: start it.
    Fresh,
    /// The last incarnation stopped cleanly (or none ever ran): go on.
    Clean,
    /// The last incarnation stopped unexpectedly; the owner explicitly
    /// decided, with this command, that it may be recovered.
    Recover(OwnerCommand),
}

impl Session {
    /// Decides how to continue, starts the work and drives it.
    async fn begin(&mut self, spec: &TaskLaunchSpec) -> Result<RunnerExit, RunnerError> {
        let store = self.engine.store().clone();
        let mut executions = store.list_execution_ids()?;
        self.execution_id = match executions.len() {
            0 => None,
            1 => executions.pop(),
            _ => return Err(RunnerError::SeveralExecutions(executions.join(", "))),
        };

        let previous = self.storage.session()?;
        let continuation = match (&self.execution_id, &previous) {
            (None, _) => Continuation::Fresh,
            (Some(_), None) => Continuation::Clean,
            (Some(_), Some(previous)) if previous.closed.is_some() => Continuation::Clean,
            (Some(_), Some(previous)) => match self.owner_decision(previous).await? {
                Some(command) => Continuation::Recover(command),
                None => return self.refuse_without_decision().await,
            },
        };

        self.storage.write_session(&SessionRecord::open(
            &self.incarnation,
            self.link
                .launch
                .as_ref()
                .map(|launch| launch.launch_generation),
        ))?;

        match continuation {
            Continuation::Fresh => {
                let execution = self.engine.start(
                    &self.definition,
                    spec.inputs.clone(),
                    workspace(&self.storage, spec),
                )?;
                tracing::info!(execution_id = %execution.id, "started the task's execution");
                self.execution_id = Some(execution.id.clone());
                self.resume_pass()?;
            }
            Continuation::Clean => self.resume_pass()?,
            Continuation::Recover(command) => self.recover(command)?,
        }
        self.drive().await
    }

    /// Waits for the control plane to say which owner command launched
    /// this incarnation, and returns it when it authorizes recovering the
    /// storage `previous` left behind.
    async fn owner_decision(
        &mut self,
        previous: &SessionRecord,
    ) -> Result<Option<OwnerCommand>, RunnerError> {
        tracing::warn!(
            previous = %previous.incarnation,
            "the previous runner stopped unexpectedly; waiting for the owner's decision"
        );
        let launch = loop {
            if let Some(launch) = &self.link.launch {
                break launch.clone();
            }
            if let Some(client) = self.link.client() {
                match client.poll(None, 0).await {
                    Ok(response) => {
                        self.link.succeeded();
                        self.link.launch = Some(response.launch.clone());
                        continue;
                    }
                    Err(error) => self.link.failed(&error, &self.timing),
                }
            } else {
                self.link.failed(&ClientError::Unauthorized, &self.timing);
            }
            tokio::time::sleep_until(self.link.next_attempt).await;
        };

        if previous.incarnation == self.incarnation {
            // The same incarnation started again: nobody decided anything.
            return Ok(None);
        }
        let Some(command) = launch.authorized_by else {
            return Ok(None);
        };
        // A decision taken before the loss would be at the lost runner's
        // own generation or older; only a newer one answers this loss.
        if previous
            .launch_generation
            .is_some_and(|lost| launch.launch_generation <= lost)
        {
            return Ok(None);
        }
        Ok(Some(command))
    }

    /// Reports that the storage waits for its owner and stops, having
    /// changed nothing.
    async fn refuse_without_decision(&mut self) -> Result<RunnerExit, RunnerError> {
        let problem = RunnerProblem {
            code: PROBLEM_OWNER_DECISION.to_string(),
            message: "the previous runner of this task stopped unexpectedly; it is recovered \
                      only after the owner explicitly starts or resumes the task"
                .to_string(),
        };
        let mut snapshot = self.snapshot(Some(problem.clone()), true)?;
        snapshot.snapshot_sequence = self.storage.next_snapshot_sequence()?;
        send_until(&mut self.link, &self.timing, &snapshot).await;
        Ok(RunnerExit::Problem(problem))
    }

    /// Runs reconciliation the owner authorized and, when it finds an
    /// action that was in flight, starts that action again.
    fn recover(&mut self, command: OwnerCommand) -> Result<(), RunnerError> {
        let execution_id = self.execution_id.clone().unwrap_or_default();
        let reconciliation = reconcile(self.engine.store(), &execution_id)?;
        tracing::info!(?command, outcome = ?reconciliation.outcome, "recovering after an unexpected stop");
        if let ReconciliationOutcome::InterruptedAction { .. } = reconciliation.outcome {
            let comment = format!(
                "owner {} after the task's workload stopped unexpectedly",
                match command {
                    OwnerCommand::Start => "start",
                    _ => "resume",
                }
            );
            let handle = self
                .engine
                .try_begin_recover_with(&execution_id, Recovery::with_comment(Some(comment)))?;
            self.spawn_pass(handle);
            return Ok(());
        }
        self.resume_pass()
    }

    /// Starts a pass for a runnable execution; leaves one that waits for
    /// input or has ended alone.
    fn resume_pass(&mut self) -> Result<(), RunnerError> {
        let Some(execution_id) = self.execution_id.clone() else {
            return Ok(());
        };
        let status = self.engine.store().load_execution(&execution_id)?.status;
        if matches!(status, ExecutionStatus::Running | ExecutionStatus::Paused) {
            let handle = self.engine.try_begin_resume(&execution_id)?;
            self.spawn_pass(handle);
        }
        Ok(())
    }

    fn spawn_pass(&mut self, handle: crate::engine::RunHandle) {
        let definition = Arc::clone(&self.definition);
        self.pass = Some(tokio::spawn(async move {
            handle.run_to_completion(&definition).await
        }));
    }

    fn pass_active(&self) -> bool {
        self.pass.as_ref().is_some_and(|pass| !pass.is_finished())
    }

    fn snapshot(
        &mut self,
        problem: Option<RunnerProblem>,
        stopping: bool,
    ) -> Result<RunnerSnapshot, RunnerError> {
        let pass_active = self.pass_active();
        snapshot::build(
            &self.engine,
            &mut self.cache,
            snapshot::SnapshotContext {
                execution_id: self.execution_id.as_deref(),
                pass_active,
                problem,
                stopping,
            },
        )
    }

    /// The control loop: reports, polls, delivers, until told to stop.
    async fn drive(&mut self) -> Result<RunnerExit, RunnerError> {
        let mut events = self.engine.subscribe();
        let mut dirty = true;
        let mut last_report: Option<Instant> = None;
        let mut poll: Option<InFlightPoll> = None;
        let mut pause_asked = false;

        loop {
            // Before a pause or stop is honoured: what the last pass left
            // for the owner goes out while this workload still exists.
            if self.outbox_due && !self.pass_active() && self.link.ready() {
                self.publish_outbox().await;
            }
            match self.directive {
                RunnerDirective::PauseAtSafePoint => {
                    if self.pass_active() {
                        if !pause_asked {
                            if let Some(execution_id) = &self.execution_id {
                                tracing::info!(
                                    "pause requested; stopping at the next step boundary"
                                );
                                self.engine.pause(execution_id);
                            }
                            pause_asked = true;
                        }
                    } else {
                        abort_poll(&mut poll);
                        if let Some(exit) = self.finish_at_safe_point().await? {
                            return Ok(exit);
                        }
                    }
                }
                RunnerDirective::Stop => {
                    abort_poll(&mut poll);
                    return self.stop();
                }
                RunnerDirective::Continue => pause_asked = false,
            }

            let now = Instant::now();
            let report_due = last_report.is_none_or(|at| {
                (dirty && now >= at + self.timing.report_interval)
                    || now >= at + self.timing.heartbeat
            });
            if report_due && self.link.ready() {
                if self.report().await? {
                    dirty = false;
                }
                last_report = Some(Instant::now());
            }

            if self.link.renew_due(self.timing.renew_margin) && self.link.ready() {
                self.renew().await;
            }

            let pending = self.pending_input_id()?;
            if poll.as_ref().is_some_and(|poll| poll.asked != pending) {
                // Asked for another request than the one waited on now: an
                // answer to the new one must not wait out the old long-poll.
                abort_poll(&mut poll);
            }
            if poll.is_none() && self.link.ready() {
                if let Some(client) = self.link.client() {
                    let query = RunnerPollQuery {
                        pending_input_id: pending.clone(),
                        wait_secs: Some(self.timing.poll_wait.as_secs()),
                        defer_materials: self.materials_deferred(),
                    };
                    poll = Some(InFlightPoll {
                        asked: pending.clone(),
                        handle: tokio::spawn(async move { client.poll_with(&query).await }),
                    });
                }
            }

            let wake = self.next_wake(last_report, dirty);
            tokio::select! {
                finished = wait_for(&mut self.pass) => {
                    self.pass = None;
                    dirty = true;
                    self.outbox_due = true;
                    match finished {
                        Ok(Ok(execution)) => tracing::info!(status = ?execution.status, "the engine pass stopped"),
                        Ok(Err(error)) => tracing::warn!(%error, "the engine pass ended with an error"),
                        Err(error) => tracing::error!(%error, "the engine pass panicked"),
                    }
                }
                answered = wait_for_poll(&mut poll) => {
                    poll = None;
                    match answered {
                        Ok(Ok(response)) => {
                            self.link.succeeded();
                            self.absorb(response).await?;
                            dirty = true;
                        }
                        Ok(Err(error)) => self.link.failed(&error, &self.timing),
                        Err(_) => {}
                    }
                }
                event = events.recv() => {
                    if !matches!(event, Err(broadcast::error::RecvError::Closed)) {
                        dirty = true;
                    }
                }
                _ = tokio::time::sleep_until(wake) => {}
            }
        }
    }

    /// When the loop must look again even if nothing happens.
    fn next_wake(&self, last_report: Option<Instant>, dirty: bool) -> Instant {
        let now = Instant::now();
        let mut wake = now + self.timing.heartbeat;
        if let Some(at) = last_report {
            wake = wake.min(at + self.timing.heartbeat);
            if dirty {
                wake = wake.min(at + self.timing.report_interval);
            }
        }
        if !self.link.ready() {
            wake = wake.min(self.link.next_attempt);
        } else {
            // A poll could not be started (unreadable credential): retry.
            wake = wake.min(now + self.timing.retry_min);
        }
        wake.max(now + Duration::from_millis(10))
    }

    /// The request the execution waits on, while no pass runs.
    fn pending_input_id(&self) -> Result<Option<String>, RunnerError> {
        if self.pass_active() {
            return Ok(None);
        }
        let Some(execution_id) = &self.execution_id else {
            return Ok(None);
        };
        Ok(self
            .engine
            .pending_input(execution_id)?
            .map(|pending| pending.episode_key().to_string()))
    }

    /// Sends a fresh snapshot; true when the control plane took it.
    async fn report(&mut self) -> Result<bool, RunnerError> {
        let Some(client) = self.link.client() else {
            self.link.failed(&ClientError::Unauthorized, &self.timing);
            return Ok(false);
        };
        let mut snapshot = self.snapshot(None, false)?;
        snapshot.snapshot_sequence = self.storage.next_snapshot_sequence()?;
        match client.snapshot(&snapshot).await {
            Ok(response) => {
                self.link.succeeded();
                self.learn_launch(response.launch)?;
                self.directive = response.directive;
                self.storage.record_reported_cursor(snapshot.event_cursor)?;
                Ok(true)
            }
            Err(error) => {
                self.link.failed(&error, &self.timing);
                Ok(false)
            }
        }
    }

    /// Records the launch generation in the open session once it is known.
    fn learn_launch(&mut self, launch: RunnerLaunchView) -> Result<(), RunnerError> {
        if let Some(mut session) = self.storage.session()? {
            if session.incarnation == self.incarnation
                && session.closed.is_none()
                && session.launch_generation.is_none()
            {
                session.launch_generation = Some(launch.launch_generation);
                self.storage.write_session(&session)?;
            }
        }
        self.link.launch = Some(launch);
        Ok(())
    }

    async fn renew(&mut self) {
        let Some(client) = self.link.client() else {
            return;
        };
        match client.renew().await {
            Ok(renewed) => {
                self.link.succeeded();
                self.link.expires_at = Some(renewed.expires_at);
            }
            Err(error) => self.link.failed(&error, &self.timing),
        }
    }

    /// Acts on a poll answer: directive, input, page requests.
    async fn absorb(&mut self, response: RunnerPollResponse) -> Result<(), RunnerError> {
        self.link.expires_at = Some(response.credential_expires_at);
        self.learn_launch(response.launch)?;
        self.directive = response.directive;
        for input in response.inputs {
            self.deliver(input).await?;
        }
        for request in response.conversation_pages {
            self.serve_page(request).await;
        }
        if !response.materials.is_empty() && !self.materials_deferred() {
            self.receive_materials(response.materials).await;
        }
        Ok(())
    }

    /// Hands one owner answer to the Engine, at most once.
    async fn deliver(&mut self, input: DeliveredInput) -> Result<(), RunnerError> {
        let Some(client) = self.link.client() else {
            return Ok(());
        };
        if self.storage.input_consumed(&input.input_id)? {
            // Our acknowledgement was lost; the Engine has it already.
            let _ = client.ack_input(&input.input_id, true, None).await;
            return Ok(());
        }
        if self.pass_active() {
            // Redelivered on a later poll, once nothing runs.
            return Ok(());
        }
        let Some(execution_id) = self.execution_id.clone() else {
            return Ok(());
        };
        let verdict = match self.engine.try_begin_deliver_human_input(
            &self.definition,
            &execution_id,
            &input.delivery,
        ) {
            Ok(handle) => {
                self.storage.record_consumed_input(&input.input_id)?;
                self.spawn_pass(handle);
                (true, None)
            }
            Err(EngineError::AlreadyRunning { .. }) => return Ok(()),
            Err(error) => (false, Some(error.to_string())),
        };
        if let Err(error) = client
            .ack_input(&input.input_id, verdict.0, verdict.1)
            .await
        {
            // Redelivered later and answered from the consumed record.
            self.link.failed(&error, &self.timing);
        }
        Ok(())
    }

    /// Uploads one bounded conversation page an owner waits for.
    async fn serve_page(&mut self, request: ConversationPageRequest) {
        let (Some(client), Some(execution_id)) = (self.link.client(), self.execution_id.clone())
        else {
            return;
        };
        let read = self.engine.store().conversation_store(&execution_id).read(
            &request.attempt_id,
            request.from_entry,
            usize::try_from(request.byte_budget).unwrap_or(usize::MAX),
        );
        let page = match read {
            Ok(Some(snapshot)) => ConversationResponse::Recorded {
                entries: snapshot
                    .entries
                    .into_iter()
                    .map(|entry| ConversationEntryDto {
                        entry_number: entry.entry_number,
                        timestamp: entry.timestamp,
                        source: entry.source,
                        raw_base64: base64::engine::general_purpose::STANDARD.encode(&entry.raw),
                    })
                    .collect(),
                next_entry: snapshot.next_entry,
                closed: snapshot.closed,
                truncated_tail: snapshot.truncated_tail,
            },
            Ok(None) => ConversationResponse::NoRecord,
            Err(error) => {
                tracing::warn!(%error, attempt = %request.attempt_id, "cannot read a conversation page");
                return;
            }
        };
        if let Err(error) = client
            .upload_conversation_page(&request.request_id, &page)
            .await
        {
            self.link.failed(&error, &self.timing);
        }
    }

    /// Closes the session cleanly, then reports the safe point a pause
    /// waits for until the control plane takes it. `None` when the task
    /// left the pause meanwhile and work goes on (the session is reopened).
    ///
    /// Closing comes first: the moment the control plane accepts the safe
    /// point, the controller may remove the compute, and the next
    /// incarnation must then find a clean stop, not an unexpected one.
    async fn finish_at_safe_point(&mut self) -> Result<Option<RunnerExit>, RunnerError> {
        self.close_session("paused at a safe point")?;
        loop {
            if self.link.ready() {
                if let Some(client) = self.link.client() {
                    let mut snapshot = self.snapshot(None, true)?;
                    snapshot.snapshot_sequence = self.storage.next_snapshot_sequence()?;
                    match client.snapshot(&snapshot).await {
                        Ok(response) => {
                            self.link.succeeded();
                            self.learn_launch(response.launch)?;
                            self.storage.record_reported_cursor(snapshot.event_cursor)?;
                            self.directive = response.directive;
                            match response.directive {
                                RunnerDirective::PauseAtSafePoint | RunnerDirective::Stop => {
                                    tracing::info!("safe point acknowledged; stopping");
                                    return Ok(Some(RunnerExit::PausedAtSafePoint));
                                }
                                RunnerDirective::Continue => {
                                    self.reopen_session()?;
                                    return Ok(None);
                                }
                            }
                        }
                        Err(error) => self.link.failed(&error, &self.timing),
                    }
                } else {
                    self.link.failed(&ClientError::Unauthorized, &self.timing);
                }
            }
            tokio::time::sleep_until(self.link.next_attempt).await;
        }
    }

    /// Stops on the control plane's word.
    fn stop(&mut self) -> Result<RunnerExit, RunnerError> {
        if self.pass_active() {
            // Work is in flight and will be cut off with the workload: the
            // session stays open, so whoever comes next needs an owner
            // decision (archive deletes the storage anyway).
            tracing::warn!("told to stop while an engine pass runs");
            return Ok(RunnerExit::Stopped { clean: false });
        }
        self.close_session("stopped by the control plane")?;
        Ok(RunnerExit::Stopped { clean: true })
    }

    /// Marks the session open again after a pause that did not happen.
    fn reopen_session(&self) -> Result<(), RunnerError> {
        if let Some(mut session) = self.storage.session()? {
            session.closed = None;
            self.storage.write_session(&session)?;
        }
        Ok(())
    }

    fn close_session(&self, reason: &str) -> Result<(), RunnerError> {
        let mut session = self
            .storage
            .session()?
            .unwrap_or_else(|| SessionRecord::open(&self.incarnation, None));
        session.closed = Some(storage::SessionClose {
            at: Utc::now(),
            reason: reason.to_string(),
        });
        self.storage.write_session(&session)
    }
}

/// The Engine workspace of a task: the cloned commit, or the empty folder.
fn workspace(storage: &TaskStorage, spec: &TaskLaunchSpec) -> Workspace {
    let path = storage.workspace_dir().display().to_string();
    match &spec.workspace {
        TaskWorkspace::None => Workspace::local_folder(path),
        TaskWorkspace::Git(identity) => {
            let name = identity
                .repository
                .clone()
                .unwrap_or_else(|| crate::tasks::repository_name(&identity.remote_url));
            let mut workspace = Workspace::git_checkout(
                path,
                RepositoryInfo {
                    id: name.clone(),
                    name,
                    provider: "git".to_string(),
                    remote: identity.remote_url.clone(),
                },
                Some(identity.commit.clone()),
            );
            workspace.base_commit = Some(identity.commit.clone());
            workspace
        }
    }
}

/// A long-poll under way, and the pending request it asked about.
struct InFlightPoll {
    asked: Option<String>,
    handle: JoinHandle<Result<RunnerPollResponse, ClientError>>,
}

fn abort_poll(poll: &mut Option<InFlightPoll>) {
    if let Some(poll) = poll.take() {
        poll.handle.abort();
    }
}

async fn wait_for<T>(handle: &mut Option<JoinHandle<T>>) -> Result<T, tokio::task::JoinError> {
    match handle {
        Some(handle) => handle.await,
        None => std::future::pending().await,
    }
}

async fn wait_for_poll(
    poll: &mut Option<InFlightPoll>,
) -> Result<Result<RunnerPollResponse, ClientError>, tokio::task::JoinError> {
    match poll {
        Some(poll) => (&mut poll.handle).await,
        None => std::future::pending().await,
    }
}
