//! The task runner: the existing Engine, run against one portable task
//! storage, reporting to a control plane it does not depend on (U3, KTD5).
//!
//! A runner is what a backend starts inside a task's isolated workload
//! (`narsor-runner …`, see [`cli`]). It owns exactly one task storage
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
//! 5. **Assemble the conductor** ([`conductor`]); it is attached to the
//!    Engine once its harness is usable in this environment, and until then
//!    a failed step goes to the owner as it is.
//! 6. **Decide how to continue** (below), ask the control plane once,
//!    without waiting, which default harness to follow, then run the
//!    Engine while a control loop reports snapshots, long-polls for
//!    directives, input, page requests and materials (U9), and renews the
//!    credential.
//!
//! # The default harness follows the installation's (U7)
//!
//! The harness the task was created with is only its fallback. Every
//! control-plane answer may carry the installation's default as it applies
//! to the task ([`TaskDefaultHarness`]), and the runner applies it before
//! anything else in that answer: a harness its backend runs and whose
//! binary is on this environment's `PATH` becomes the runner's live harness
//! — what the next agent action without a `harness:` of its own and the
//! next conductor invocation run on — and is kept on storage, so a resumed
//! runner starts on it. One it cannot switch to leaves the live harness as
//! it is, and the snapshot says why ([`HarnessUnavailable`]) until an answer
//! names one it can run. An answer without the field changes nothing. An
//! action under way is never switched: the Engine reads the default once,
//! when an action starts (R16a).
//!
//! # Unexpected loss needs an explicit decision (R13)
//!
//! Ordinary local recovery ([`crate::persistence::reconcile`]) turns an
//! action that was in flight into `Unknown`, and an explicit recovery then
//! starts that action again. Neither may happen just because a workload came
//! back: the control plane decides. The runner tells the two situations
//! apart from its own storage and learns the decision from the control
//! plane:
//!
//! * every incarnation writes an open [`storage::SessionRecord`] when it
//!   starts working and closes it only when it stops at a safe point. An
//!   open record found at start-up is the storage's testimony that the
//!   previous incarnation stopped unexpectedly;
//! * the control plane tells a runner, with every poll and snapshot answer,
//!   which `start` or `resume` launched it
//!   ([`crate::controller_protocol::RunnerLaunchView::authorized_by`]) and
//!   who issued it (`authorized_by_actor`): the owner, or the control
//!   plane's resume policy after it counted the loss. A controller never
//!   relaunches a lost workload on its own (KTD2), so a new incarnation
//!   launched at a newer generation by such a command *is* the decision,
//!   recorded where only the control plane can write it.
//!
//! So after an unclean stop the runner touches nothing until it reaches the
//! control plane. Authorized, it reconciles, and when an action was in
//! flight it starts that action again through an ordinary explicit recovery
//! (D24's idempotency key changes with the new attempt), noting in the
//! history whether the owner or the resume policy decided. The actor words
//! only that note: the authorization is the command alone. Not authorized —
//! the same incarnation restarted, or nothing says anyone decided — it
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
mod commands;
mod conductor;
pub mod home;
pub mod init;
mod materials;
pub mod preflight;
pub mod profile;
mod reader;
pub mod secrets;
mod snapshot;
pub mod storage;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::api::ClientError;
use crate::conductor::LiveHarness;
use crate::controller_protocol::{
    ConversationPageRequest, DeliveredInput, ExecutionEventsRequest, RunnerClient, RunnerDirective,
    RunnerLaunchView, RunnerPollQuery, RunnerPollResponse, TaskDefaultHarness,
};
use crate::domain::{
    AnsweredBy, ExecutionStatus, LaunchDefinitionError, RepositoryInfo, TaskId, TaskLaunchSpec,
    TaskWorkspace, WorkflowDefinition, WorkflowExecution, Workspace,
};
use crate::engine::{Engine, EngineError, HumanInputDelivery, Recovery};
use crate::orchestration::{
    CommandActor, HarnessUnavailable, OwnerCommand, RunnerProblem, RunnerSnapshot,
};
use crate::persistence::{reconcile, PersistenceError, ReconciliationOutcome};
use crate::workspace::WorkspaceError;
use home::CLAUDE_CONFIG_DIR_ENV;

pub use home::{HarnessHome, HomeMode};
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
/// Problem code: a read-only runner could not open the storage to read it.
pub const PROBLEM_READER_UNAVAILABLE: &str = "reader_unavailable";
/// Problem code: the storage was left by an unexpected stop and no start or
/// resume (the owner's or the resume policy's, D58) authorizes this
/// incarnation to recover it (R13).
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
    /// The Claude Code profile could not be brought into the work directory.
    #[error("Claude profile preparation failed: {0}")]
    Profile(String),
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
    /// A read-only runner found a storage no runner ever worked on.
    #[error("task storage '{0}' was never initialized by a runner")]
    StorageNotInitialized(PathBuf),
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
    /// Which HOME and harness configuration every action sees.
    pub harness_home: HarnessHome,
    /// This incarnation's identity, the one its credential is bound to.
    pub incarnation: String,
    /// Control-channel timing.
    pub timing: RunnerTiming,
    /// The base-image contract to check.
    pub contract: BaseImageContract,
    /// Only answer reads of the storage and change nothing on it ([`reader`]).
    pub read_only: bool,
}

/// Runs one runner process to its end.
///
/// `apply_environment` is called once, after the secrets are materialized
/// and before any work starts, with the variables every action's child
/// process must inherit (the private HOME, `env`-delivered secrets) and the
/// ones it must not. It is
/// a callback because only the process's entry point may change its
/// environment; library code never does.
pub async fn run(
    config: RunnerConfig,
    apply_environment: impl FnOnce(&ProcessEnvironment),
) -> Result<RunnerExit, RunnerError> {
    if config.read_only {
        return reader::run(config).await;
    }
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
    let private_home = storage.home_dir();
    let home = (config.harness_home.mode == HomeMode::Private).then_some(private_home.as_path());
    let mut environment = match secrets.materialize(&spec.environment, home) {
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

    environment.task.extend(config.harness_home.variables());
    match profile::prepare_claude_profile(
        &config.harness_home,
        &private_home,
        &storage.claude_profile_base_dir(),
    ) {
        // A configured directory is already among the variables.
        Ok(Some(work)) if config.harness_home.claude_config_dir.is_none() => {
            environment.task.push((
                CLAUDE_CONFIG_DIR_ENV.to_string(),
                work.display().to_string(),
            ))
        }
        Ok(_) => {}
        Err(error) => {
            return Ok(report_problem(
                &storage,
                &mut link,
                &config.timing,
                PROBLEM_INITIALIZATION,
                error.to_string(),
            )
            .await)
        }
    }
    apply_environment(&environment);

    // One live harness for the Engine and the conductor (U7): the default
    // this storage last switched to, else the one the task was created with,
    // until the control plane names another.
    let starting = storage.applied_harness().unwrap_or_else(|error| {
        tracing::warn!(%error, "the harness last switched to cannot be read; using the task's");
        None
    });
    let harness = Arc::new(LiveHarness::new(
        starting.unwrap_or_else(|| spec.environment.harness.clone()),
    ));
    let engine = Arc::new(Engine::new(store).with_default_harness_source(harness.clone()));

    let conductor =
        conductor::ConductorHost::assemble(&engine, &storage, Arc::clone(&harness), &definition);
    let mut session = Session {
        storage,
        engine,
        conductor,
        harness,
        harness_unavailable: None,
        report_forced: false,
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
    let observed_at = Utc::now();
    let snapshot = RunnerSnapshot {
        snapshot_sequence: storage.next_snapshot_sequence().unwrap_or(1),
        execution_status: existing
            .as_ref()
            .map_or(ExecutionStatus::Unknown, |execution| execution.status),
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
        attention: snapshot::attention_items(None, Some(&problem), observed_at),
        problem: Some(problem.clone()),
        conductor_available: false,
        conductor_unavailable_reason: Some("the runner cannot work".to_string()),
        default_harness_unavailable: None,
        display: None,
        observed_at,
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
    /// The control plane refused the credential: renewing comes first, as
    /// it is the one call a credential that ran out may still make.
    renewal_wanted: bool,
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
            renewal_wanted: false,
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
            // again in case a newer credential was mounted, and a renewal
            // revives one that merely ran out.
            self.client = None;
            self.renewal_wanted = true;
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

    /// Renews the credential; a refusal or a failure is handled as in
    /// [`Link::failed`].
    async fn renew(&mut self, timing: &RunnerTiming) {
        let Some(client) = self.client() else {
            return;
        };
        match client.renew().await {
            Ok(renewed) => {
                self.succeeded();
                self.expires_at = Some(renewed.expires_at);
                self.renewal_wanted = false;
            }
            Err(error) => self.failed(&error, timing),
        }
    }

    fn renew_due(&self, margin: Duration) -> bool {
        self.renewal_wanted
            || self.expires_at.is_some_and(|expires| {
                let margin = chrono::Duration::from_std(margin).unwrap_or_default();
                Utc::now() + margin >= expires
            })
    }
}

/// The work phase of one runner process.
struct Session {
    storage: TaskStorage,
    engine: Arc<Engine>,
    conductor: conductor::ConductorHost,
    /// The harness agent actions without their own and the conductor run
    /// on, shared with both (U7).
    harness: Arc<LiveHarness>,
    /// The default harness the task cannot switch to, while it cannot.
    harness_unavailable: Option<HarnessUnavailable>,
    /// A snapshot must go out even if nothing else changed: what it says
    /// about the default harness did.
    report_forced: bool,
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
    /// The last incarnation stopped unexpectedly; the owner or the resume
    /// policy explicitly decided, with this command, that it may be
    /// recovered.
    Recover(Decision),
}

/// The command that authorized recovering a storage after an unclean stop,
/// and who issued it.
#[derive(Debug, Clone, Copy)]
struct Decision {
    command: OwnerCommand,
    actor: CommandActor,
}

impl Decision {
    /// What the recovered action's history says about who decided.
    fn comment(self) -> String {
        match (self.actor, self.command) {
            (CommandActor::Policy, _) => "resumed automatically after workload loss".to_string(),
            (CommandActor::Owner, OwnerCommand::Start) => {
                "owner start after the task's workload stopped unexpectedly".to_string()
            }
            (CommandActor::Owner, _) => {
                "owner resume after the task's workload stopped unexpectedly".to_string()
            }
        }
    }
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
                Some(decision) => Continuation::Recover(decision),
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
        // A queued or paused task may have missed a change of the default:
        // its first action starts on the current one, not on the fallback.
        self.ask_default_harness().await?;

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
            Continuation::Recover(decision) => self.recover(decision)?,
        }
        // After the runner's own recovery, like the daemon's startup hook.
        self.conductor.restore_at_startup();
        self.drive().await
    }

    /// Asks the control plane once, without waiting, which default harness
    /// to follow, and applies it. Unreachable, the runner starts on the one
    /// it has.
    async fn ask_default_harness(&mut self) -> Result<(), RunnerError> {
        let Some(client) = self.link.client() else {
            return Ok(());
        };
        // Naming the request the execution waits on: a poll that names
        // another (or none) turns the owner's queued answers to it obsolete.
        let pending = self.pending_input_id()?;
        match client.poll_with(&self.poll_query(pending, 0)).await {
            // What else the answer carries — answers, commands, reads — is
            // offered again by the next poll until settled: drive picks it
            // up there.
            Ok(response) => {
                self.take_answer_header(response.default_harness, response.launch)?;
                self.link.expires_at = Some(response.credential_expires_at);
            }
            Err(error) => self.link.failed(&error, &self.timing),
        }
        Ok(())
    }

    /// A poll naming the request waited on and the harness settled on:
    /// the one the runner runs on or, while it cannot switch, the default
    /// it cannot run — so the control plane answers at once only a default
    /// the runner has not applied yet, never one it already turned down.
    fn poll_query(&self, pending_input_id: Option<String>, wait_secs: u64) -> RunnerPollQuery {
        let harness = match &self.harness_unavailable {
            Some(unavailable) => unavailable.harness.clone(),
            None => self.harness.get(),
        };
        RunnerPollQuery {
            pending_input_id,
            wait_secs: Some(wait_secs),
            defer_materials: false,
            harness: Some(harness),
        }
    }

    /// Applies what every answer of the control plane carries, in one
    /// order: the link worked, then the default harness — an action the
    /// rest of the answer resumes starts on it — then the launch.
    fn take_answer_header(
        &mut self,
        default_harness: Option<TaskDefaultHarness>,
        launch: RunnerLaunchView,
    ) -> Result<(), RunnerError> {
        self.link.succeeded();
        self.follow_default_harness(default_harness);
        self.learn_launch(launch)
    }

    /// Applies what the control plane says of the installation's default
    /// harness (see the module documentation): switches to it when this
    /// environment can run it, otherwise keeps the live harness and says
    /// why. `None` changes nothing.
    fn follow_default_harness(&mut self, default: Option<TaskDefaultHarness>) {
        let Some(default) = default else {
            return;
        };
        let current = self.harness.get();
        let unavailable = match default {
            TaskDefaultHarness::Available { harness } if harness == current => None,
            TaskDefaultHarness::Available { harness } => {
                match conductor::harness_unavailable(&harness) {
                    None => {
                        self.switch_harness(&current, harness);
                        None
                    }
                    Some(reason) => Some((harness, reason)),
                }
            }
            TaskDefaultHarness::Unavailable { harness, reason } => Some((harness, reason)),
        }
        .map(|(harness, reason)| HarnessUnavailable {
            harness,
            kept: current,
            reason,
        });
        if unavailable != self.harness_unavailable {
            if let Some(unavailable) = &unavailable {
                tracing::info!(
                    harness = %unavailable.harness,
                    kept = %unavailable.kept,
                    reason = %unavailable.reason,
                    "the default harness cannot run for this task; keeping the current one"
                );
            }
            self.harness_unavailable = unavailable;
            self.report_forced = true;
        }
    }

    /// Moves the agent actions to come and the conductor from `current`
    /// to `harness`, which can run here. Work does not stop over it: only
    /// a later incarnation would start on the older harness.
    fn switch_harness(&mut self, current: &str, harness: String) {
        tracing::info!(from = %current, to = %harness, "following the default harness");
        if let Err(error) = self.storage.record_applied_harness(&harness) {
            tracing::warn!(%error, "the harness switched to cannot be recorded");
        }
        self.harness.set(harness);
        self.conductor.refresh(None);
    }

    /// Waits for the control plane to say which command launched this
    /// incarnation, and returns it when it authorizes recovering the
    /// storage `previous` left behind.
    async fn owner_decision(
        &mut self,
        previous: &SessionRecord,
    ) -> Result<Option<Decision>, RunnerError> {
        tracing::warn!(
            previous = %previous.incarnation,
            "the previous runner stopped unexpectedly; waiting for the owner's decision"
        );
        let launch = loop {
            if let Some(launch) = &self.link.launch {
                break launch.clone();
            }
            if let Some(client) = self.link.client() {
                match client.poll_with(&self.poll_query(None, 0)).await {
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
        Ok(Some(Decision {
            command,
            actor: launch.authorized_by_actor.unwrap_or_default(),
        }))
    }

    /// Reports that the storage waits for a start/resume (the owner's or the
    /// resume policy's) and stops, having changed nothing.
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

    /// Runs reconciliation the owner or the resume policy authorized and,
    /// when it finds an action that was in flight, starts that action again.
    fn recover(&mut self, decision: Decision) -> Result<(), RunnerError> {
        let execution_id = self.execution_id.clone().unwrap_or_default();
        let reconciliation = reconcile(self.engine.store(), &execution_id)?;
        tracing::info!(?decision, outcome = ?reconciliation.outcome, "recovering after an unexpected stop");
        if let ReconciliationOutcome::InterruptedAction { .. } = reconciliation.outcome {
            let handle = self.engine.try_begin_recover_with(
                &execution_id,
                Recovery::with_comment(Some(decision.comment())),
            )?;
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

    /// Whether a pass is driving the execution: the runner's own, or one
    /// the conductor started.
    fn pass_active(&self) -> bool {
        self.pass.as_ref().is_some_and(|pass| !pass.is_finished())
            || self.conductor.passes().active()
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
                definition: &self.definition,
                execution_id: self.execution_id.as_deref(),
                pass_active,
                problem,
                stopping,
                conductor_unavailable: self.conductor.unavailable_reason(),
                default_harness_unavailable: self.harness_unavailable.as_ref(),
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

            // Before the report: a refused one would otherwise hold the
            // link back each time, and the renewal it needs never be made.
            if self.link.renew_due(self.timing.renew_margin) && self.link.ready() {
                self.renew().await;
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
            if std::mem::take(&mut self.report_forced) {
                dirty = true;
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
                        defer_materials: self.materials_deferred(),
                        ..self.poll_query(pending.clone(), self.timing.poll_wait.as_secs())
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
                        Err(error) if error.is_cancelled() => tracing::info!("the engine pass was stopped"),
                        Err(error) => tracing::error!(%error, "the engine pass panicked"),
                    }
                }
                _ = self.conductor.passes().ended() => {
                    dirty = true;
                    self.outbox_due = true;
                }
                answered = wait_for_poll(&mut poll) => {
                    poll = None;
                    match answered {
                        Ok(Ok(response)) => {
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
                self.take_answer_header(response.default_harness, response.launch)?;
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
        self.link.renew(&self.timing).await;
    }

    /// Acts on a poll answer: the default harness first — an action the
    /// same answer's input resumes starts on it — then directive, input,
    /// operator commands, page requests.
    async fn absorb(&mut self, response: RunnerPollResponse) -> Result<(), RunnerError> {
        self.take_answer_header(response.default_harness, response.launch)?;
        self.link.expires_at = Some(response.credential_expires_at);
        self.directive = response.directive;
        for input in response.inputs {
            self.deliver(input).await?;
        }
        for command in response.commands {
            self.carry_out(command).await?;
        }
        for request in response.conversation_pages {
            self.serve_page(request).await;
        }
        for request in response.execution_events {
            self.serve_events(request).await;
        }
        for request in response.conductor_reads {
            self.serve_conductor_read(request).await;
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
        let permission_decision = input.delivery.permission_decision.map(|decision| {
            narsor_engine::engine::PermissionDecision {
                denial_index: decision.denial_index,
                decision: match decision.decision {
                    narsor_protocol::api::PermissionDecisionKindDto::Deny => {
                        narsor_engine::engine::PermissionDecisionKind::Deny
                    }
                    narsor_protocol::api::PermissionDecisionKindDto::Allow => {
                        narsor_engine::engine::PermissionDecisionKind::Allow
                    }
                },
                scope: decision.scope.map(|scope| match scope {
                    narsor_protocol::api::PermissionGrantScopeDto::ExactOperation => {
                        narsor_engine::engine::PermissionGrantScope::ExactOperation
                    }
                    narsor_protocol::api::PermissionGrantScopeDto::CommandGroup => {
                        narsor_engine::engine::PermissionGrantScope::CommandGroup
                    }
                    narsor_protocol::api::PermissionGrantScopeDto::Pattern => {
                        narsor_engine::engine::PermissionGrantScope::Pattern
                    }
                }),
                pattern: decision.pattern,
                message: decision.message,
            }
        });
        let episode = input.delivery.episode.clone();
        let delivery = HumanInputDelivery {
            values: input.delivery.values,
            chosen_label: input.delivery.chosen_label,
            artifacts: input.delivery.artifacts,
            variables: input.delivery.variables,
            comment: input.delivery.comment,
            agent_message: input.delivery.agent_message,
            action: input.delivery.action,
            episode: input.delivery.episode,
            permission_decision: permission_decision.clone(),
            answered_by: AnsweredBy::Operator,
        };
        let started = if let Some(decision) = permission_decision {
            episode
                .as_deref()
                .ok_or_else(|| {
                    RunnerError::LaunchSpec("permission decision has no request id".into())
                })
                .and_then(|episode| {
                    self.engine
                        .try_begin_resolve_permission(
                            &self.definition,
                            &execution_id,
                            decision,
                            episode,
                        )
                        .map_err(RunnerError::from)
                })
        } else {
            self.engine
                .try_begin_deliver_human_input(&self.definition, &execution_id, &delivery)
                .map_err(RunnerError::from)
        };
        let verdict = match started {
            Ok(handle) => {
                self.storage.record_consumed_input(&input.input_id)?;
                self.spawn_pass(handle);
                (true, None)
            }
            Err(RunnerError::Engine(EngineError::AlreadyRunning { .. })) => return Ok(()),
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
        let Some(execution_id) = self.execution_id.clone() else {
            return;
        };
        reader::serve_page(
            &mut self.link,
            &self.timing,
            self.engine.store(),
            &execution_id,
            request,
        )
        .await;
    }

    /// Uploads the execution's events an owner waits for: those after the
    /// requested sequence, at most the requested number.
    async fn serve_events(&mut self, request: ExecutionEventsRequest) {
        let Some(execution_id) = self.execution_id.clone() else {
            return;
        };
        reader::serve_events(
            &mut self.link,
            &self.timing,
            self.engine.store(),
            &execution_id,
            request,
        )
        .await;
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
                            self.take_answer_header(response.default_harness, response.launch)?;
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
        TaskWorkspace::Local { .. } => Workspace::local_folder(path),
        TaskWorkspace::Git(identity) => {
            let name = identity
                .repository
                .clone()
                .unwrap_or_else(|| crate::tasks::repository_name(&identity.remote_url));
            // A workspace pinned at admission is known only from its checkout.
            let commit = if identity.is_pinned() {
                Some(identity.commit.clone())
            } else {
                crate::workspace::checked_out_commit(&storage.workspace_dir())
            };
            let mut workspace = Workspace::git_checkout(
                path,
                RepositoryInfo {
                    id: name.clone(),
                    name,
                    provider: "git".to_string(),
                    remote: identity.remote_url.clone(),
                },
                commit.clone(),
            );
            workspace.base_commit = commit;
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
