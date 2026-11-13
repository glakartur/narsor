//! The observers' schedule (R6, R7, R14, R16, R17): which observer is due,
//! the worker's claim of a run, and what the run's result changes.
//!
//! ```text
//! observer_pass   next_run_at <= now, active, approved  → run_due_since = now
//!                 watch_list without entries            → next_run_at += interval, no run
//!                 run past its deadline                 → Lost, token spent
//! claim           dry run first, then the oldest due    → ObserverRun + token (once)
//! finish          exit 0 + valid output                 → state saved, failures = 0
//!                 anything else                         → state kept, failures + 1, backoff
//! ```
//!
//! The pass only flags; the run record appears at the claim. The flag is
//! one value, not a queue, so a control plane that stood still for hours
//! owes at most one run per observer afterwards. Deadlines, counters and
//! the flag live in the observer's record, which survives a restart.
//!
//! Every mutation saves the observer's record before the run's record: the
//! observer's record is the commit point. A crash between the two leaves a
//! record naming a run whose file is missing or stale; the pass repairs it.
//!
//! The pass runs on the daemon's 5-second tick and must stay cheap: when
//! nothing is due it reads the observers' records and takes no lock.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::controller_protocol::credentials::{
    observer_run_token_id, token_matches_digest, BearerToken,
};
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::ProtocolService;
use crate::domain::TaskId;
use crate::observers::{
    parse_script_output, secs_duration, LastRun, ObserverError, ObserverId, ObserverLock,
    ObserverRecord, ObserverRun, ObserverState, RunId, RunStatus, ScriptInput, ScriptObserver,
    ScriptOutput, ScriptWatch, SecretRef, WatchEntry, WatchMode, OBSERVER_RUN_VERSION,
    SCRIPT_CONTRACT_VERSION,
};
use crate::orchestration::{OrchestrationError, TaskState};

/// Failed runs in a row after which an observer goes to `error` and waits
/// for its owner (R16).
pub const MAX_CONSECUTIVE_FAILURES: u32 = 5;

/// Longest wait the backoff after failed runs reaches (1 h). An interval
/// longer than this is kept as it is: a failure never runs an observer more
/// often than its schedule.
pub const MAX_RETRY_DELAY_SECS: u64 = 60 * 60;

/// How long a script may run before the worker kills it (5 min).
pub const SCRIPT_TIMEOUT_SECS: u64 = 5 * 60;

/// How long after the script's time limit the control plane still waits
/// for the worker's result (1 min): the worker needs time to kill the
/// process group and report. A run without a result by then is lost.
pub const RUN_DEADLINE_MARGIN_SECS: u64 = 60;

/// Most bytes of stdout, stderr or an error message a run record keeps.
/// The whole stdout (up to the contract's limit) is parsed first.
pub const MAX_STORED_LOG_BYTES: usize = 64 * 1024;

/// Why a lost run ended, as `observer show` prints it.
const LOST_MESSAGE: &str =
    "lost by the worker: no result before the run's deadline; the observer's state is unchanged";

/// Seconds until the next run after `failures` failed runs in a row:
/// `interval × 2^failures`, at most [`MAX_RETRY_DELAY_SECS`], never less
/// than the interval itself.
pub fn retry_delay_secs(interval_secs: u64, failures: u32) -> u64 {
    let factor = 1u64.checked_shl(failures).unwrap_or(u64::MAX);
    interval_secs
        .saturating_mul(factor)
        .min(MAX_RETRY_DELAY_SECS)
        .max(interval_secs)
}

/// What one pass of the observers' schedule did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObserverPass {
    /// Observers whose scheduled run became due.
    pub due: Vec<ObserverId>,
    /// Watch-list observers that were due with no watch entry: no run, the
    /// next deadline moved one interval on (R6).
    pub skipped: Vec<ObserverId>,
    /// Runs that passed their deadline without a result.
    pub lost: Vec<RunId>,
    /// Watch entries removed because their task ended or was archived (R7).
    pub watches_removed: usize,
}

/// A run handed to a worker. The token is in no record: this is the only
/// time anyone sees it.
#[derive(Debug, Clone, Serialize)]
pub struct ClaimedObserverRun {
    /// The run.
    pub run_id: RunId,
    /// The run's token, for the effect routes, valid until the result or
    /// the deadline.
    pub token: BearerToken,
    /// Effects are recorded, not performed (R10).
    pub dry_run: bool,
    /// The script version it runs.
    pub script_version: u32,
    /// The script itself.
    pub script: String,
    /// What the script reads on stdin.
    pub input: ScriptInput,
    /// The secrets the worker hands to the script; empty for a dry run of
    /// a version that is not approved, unless the owner allowed them.
    pub secrets: Vec<SecretRef>,
    /// How long the worker lets the script run.
    pub timeout_secs: u64,
    /// No result by then: the run is lost and its token stops working.
    pub deadline: DateTime<Utc>,
}

/// How a run ended, as the worker saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunOutcome {
    /// The script exited with this code.
    Exited {
        /// Its exit code.
        exit_code: i32,
    },
    /// The script ran past its time limit and was killed.
    TimedOut,
    /// The worker could not run the script (a missing secret, say).
    WorkerError {
        /// What went wrong, without secrets.
        message: String,
    },
    /// An outcome from a newer worker; counts as a worker error.
    #[serde(other)]
    Unrecognized,
}

/// The worker's result of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunReport {
    /// How it ended.
    pub outcome: RunOutcome,
    /// The script's stdout, secrets masked: the contract's document.
    #[serde(default)]
    pub stdout: String,
    /// The script's stderr, secrets masked.
    #[serde(default)]
    pub stderr: String,
}

/// Which kind of run a claim would take from an observer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ClaimKind {
    /// The owner's dry run; claimed before any scheduled run.
    DryRun,
    /// The scheduled run the pass flagged.
    Scheduled,
}

/// The run a claim would take from `record`, and since when it waits.
fn claim_candidate(record: &ObserverRecord) -> Option<(ClaimKind, DateTime<Utc>)> {
    if record.active_run.is_some() {
        return None;
    }
    if let Some(request) = &record.pending_dry_run {
        return Some((ClaimKind::DryRun, request.requested_at));
    }
    if record.state == ObserverState::Active && record.approved_version.is_some() {
        return record
            .run_due_since
            .map(|since| (ClaimKind::Scheduled, since));
    }
    None
}

/// Whether the observer's scheduled run becomes due at `now`: active, with
/// an approved version, no run in progress or waiting, and its deadline
/// come (no deadline at all counts as come).
fn scheduled_run_due(record: &ObserverRecord, now: DateTime<Utc>) -> bool {
    record.state == ObserverState::Active
        && record.approved_version.is_some()
        && record.active_run.is_none()
        && record.run_due_since.is_none()
        && record.next_run_at.is_none_or(|at| at <= now)
}

/// `now` plus `secs` seconds, the latest instant when that is too far.
fn after(now: DateTime<Utc>, secs: u64) -> DateTime<Utc> {
    now.checked_add_signed(secs_duration(secs))
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

/// `text` cut to at most [`MAX_STORED_LOG_BYTES`], on a character boundary.
fn bounded(text: &str) -> String {
    if text.len() <= MAX_STORED_LOG_BYTES {
        return text.to_string();
    }
    let mut end = MAX_STORED_LOG_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Turns a worker's report into the run's status, exit code, error and,
/// for a success, the parsed output (R14).
fn judge(report: &RunReport) -> (RunStatus, Option<i32>, Option<String>, Option<ScriptOutput>) {
    match &report.outcome {
        RunOutcome::Exited { exit_code: 0 } => {
            match parse_script_output(report.stdout.as_bytes()) {
                Ok(output) => (RunStatus::Succeeded, Some(0), None, Some(output)),
                Err(error) => (
                    RunStatus::InvalidOutput,
                    Some(0),
                    Some(error.to_string()),
                    None,
                ),
            }
        }
        RunOutcome::Exited { exit_code } => (
            RunStatus::Failed,
            Some(*exit_code),
            Some(format!("the script exited with code {exit_code}")),
            None,
        ),
        RunOutcome::TimedOut => (
            RunStatus::TimedOut,
            None,
            Some(format!(
                "the script ran past its time limit of {SCRIPT_TIMEOUT_SECS} seconds"
            )),
            None,
        ),
        RunOutcome::WorkerError { message } => {
            (RunStatus::WorkerError, None, Some(bounded(message)), None)
        }
        RunOutcome::Unrecognized => (
            RunStatus::WorkerError,
            None,
            Some("the worker reported an outcome this server does not know".to_string()),
            None,
        ),
    }
}

impl ProtocolService {
    /// One pass of the observers' schedule; see the module documentation.
    ///
    /// Failures are logged per observer; the next pass retries.
    pub fn observer_pass(&self) -> ObserverPass {
        let records = match self.observers.list() {
            Ok(records) => records,
            Err(error) => {
                tracing::warn!(%error, "observers: cannot list observers");
                return ObserverPass::default();
            }
        };
        let now = self.control.now();
        let mut pass = ObserverPass::default();
        for record in &records {
            let result = if let Some(sequence) = record.active_run {
                self.expire_run_if_lost(&record.observer_id, sequence, now, &mut pass)
            } else if scheduled_run_due(record, now) {
                self.mark_run_due(&record.observer_id, now, &mut pass)
            } else {
                Ok(())
            };
            match result {
                Ok(()) | Err(ObserverError::NotFound(_)) => {}
                Err(error) => {
                    tracing::warn!(observer = %record.observer_id, %error, "observers: pass failed");
                }
            }
        }
        pass
    }

    /// Hands the next run to a worker: an owner's dry run first, then the
    /// scheduled run due the longest. `None` when no run is due.
    ///
    /// The claim creates the run's record and its token under the
    /// observer's lock, so two workers never get the same run and an
    /// observer never has two runs at once. An observer that fails to
    /// yield its run is logged and skipped, so it does not block the rest.
    pub fn claim_observer_run(&self) -> ProtocolResult<Option<ClaimedObserverRun>> {
        let mut candidates: Vec<(ClaimKind, DateTime<Utc>, ObserverId)> = self
            .observers
            .list()?
            .into_iter()
            .filter_map(|record| {
                claim_candidate(&record).map(|(kind, since)| (kind, since, record.observer_id))
            })
            .collect();
        candidates.sort();
        let now = self.control.now();
        for (_, _, observer_id) in candidates {
            match self.claim_from(&observer_id, now) {
                Ok(Some(claimed)) => return Ok(Some(claimed)),
                Ok(None) | Err(ObserverError::NotFound(_)) => {}
                Err(error) => {
                    tracing::warn!(observer = %observer_id, %error, "observers: cannot claim a run");
                }
            }
        }
        Ok(None)
    }

    /// Records the worker's result of a run and applies it to the observer.
    ///
    /// Exit code 0 with a valid output document saves the new state, marks
    /// the first run done, clears the failure count and removes the watch
    /// entries the script reported closed. Any other result keeps the state,
    /// counts a failure and waits `interval × 2^failures` (at most 1 h);
    /// after [`MAX_CONSECUTIVE_FAILURES`] the observer goes to `error`
    /// (R14, R16). A dry run's result changes nothing but its own record.
    ///
    /// The run's token stops working. A run already ended, or past its
    /// deadline (it is then recorded as lost), is refused with a conflict.
    pub fn finish_observer_run(
        &self,
        run_id: &RunId,
        report: RunReport,
    ) -> ProtocolResult<ObserverRun> {
        let observer_id = &run_id.observer_id;
        let lock = self.observers.lock(observer_id)?;
        let mut record = self.observers.load(observer_id)?;
        let mut run = self.observers.load_run(run_id)?;
        if record.active_run != Some(run_id.sequence) || run.status != RunStatus::Running {
            return Err(ProtocolError::conflict(
                "observer_run_ended",
                format!("run {run_id} has already ended"),
            ));
        }
        let now = self.control.now();
        if now >= run.deadline {
            self.lose_run(&lock, &mut record, &mut run, now)?;
            return Err(ProtocolError::conflict(
                "observer_run_lost",
                format!("run {run_id} passed its deadline and was recorded as lost"),
            ));
        }
        let (status, exit_code, error, output) = judge(&report);
        run.status = status;
        run.exit_code = exit_code;
        run.error = error.clone();
        run.finished_at = Some(now);
        run.token_digest = None;
        run.stdout = Some(bounded(&report.stdout));
        run.stderr = Some(bounded(&report.stderr));
        record.active_run = None;
        record.last_run = Some(LastRun {
            sequence: run.sequence,
            status,
            finished_at: now,
            error,
        });
        let interval = record.config.schedule.interval_secs;
        let mut closed = BTreeSet::new();
        if !run.dry_run {
            match output {
                Some(output) => {
                    record.script_state = Some(output.state);
                    record.first_run_done = true;
                    record.consecutive_failures = 0;
                    record.next_run_at = Some(after(now, interval));
                    closed.extend(output.closed_watches);
                }
                None => {
                    record.consecutive_failures = record.consecutive_failures.saturating_add(1);
                    record.next_run_at = Some(after(
                        now,
                        retry_delay_secs(interval, record.consecutive_failures),
                    ));
                    if record.consecutive_failures >= MAX_CONSECUTIVE_FAILURES
                        && record.state == ObserverState::Active
                    {
                        record.state = ObserverState::Error;
                        tracing::info!(observer = %observer_id, failures = record.consecutive_failures, "observers: too many failed runs; waiting for the owner");
                    }
                }
            }
        }
        self.observers.save(&lock, &mut record, now)?;
        if !closed.is_empty() {
            for entry in self.observers.list_watches(observer_id)? {
                if closed.contains(&entry.object) {
                    self.observers.remove_watch(&lock, &entry.entry_id)?;
                }
            }
        }
        self.observers.save_run(&lock, &run)?;
        Ok(run)
    }

    /// The run an observer run token proves, `None` for anything that is
    /// not the token of a run in progress before its deadline: a token of
    /// another kind, a spent token (result in, or lost), a wrong secret.
    /// The secret is compared with the stored digest in constant time.
    pub fn authenticate_observer_run(&self, bearer: &str) -> Option<RunId> {
        let run_id = RunId::parse(observer_run_token_id(bearer)?).ok()?;
        let run = self.observers.load_run(&run_id).ok()?;
        let digest = run.token_digest.as_deref()?;
        if !token_matches_digest(bearer, digest)
            || run.status != RunStatus::Running
            || self.control.now() >= run.deadline
        {
            return None;
        }
        let record = self.observers.load(&run_id.observer_id).ok()?;
        (record.active_run == Some(run_id.sequence)).then_some(run_id)
    }

    /// Flags the observer's scheduled run due, unless it watches a list
    /// that is empty once the entries of ended tasks are gone (R6, R7).
    fn mark_run_due(
        &self,
        observer_id: &ObserverId,
        now: DateTime<Utc>,
        pass: &mut ObserverPass,
    ) -> Result<(), ObserverError> {
        let lock = self.observers.lock(observer_id)?;
        let mut record = self.observers.load(observer_id)?;
        if !scheduled_run_due(&record, now) {
            return Ok(());
        }
        if record.config.watch_mode == WatchMode::WatchList {
            let (remaining, removed) = self.prune_watches(&lock)?;
            pass.watches_removed += removed;
            if remaining.is_empty() {
                record.next_run_at = Some(after(now, record.config.schedule.interval_secs));
                self.observers.save(&lock, &mut record, now)?;
                pass.skipped.push(observer_id.clone());
                return Ok(());
            }
        }
        record.run_due_since = Some(now);
        self.observers.save(&lock, &mut record, now)?;
        pass.due.push(observer_id.clone());
        Ok(())
    }

    /// Records the observer's run `sequence` as lost if it passed its
    /// deadline, and repairs a record that names a run which is missing or
    /// already ended (a crash between the two writes of a claim or result).
    fn expire_run_if_lost(
        &self,
        observer_id: &ObserverId,
        sequence: u64,
        now: DateTime<Utc>,
        pass: &mut ObserverPass,
    ) -> Result<(), ObserverError> {
        let run_id = RunId::new(observer_id.clone(), sequence);
        // The cheap test without the lock; the mutation tests again under it.
        match self.observers.load_run(&run_id) {
            Ok(run) if run.status == RunStatus::Running && now < run.deadline => return Ok(()),
            Ok(_) | Err(ObserverError::RunNotFound(_)) => {}
            Err(error) => return Err(error),
        }
        let lock = self.observers.lock(observer_id)?;
        let mut record = self.observers.load(observer_id)?;
        if record.active_run != Some(sequence) {
            return Ok(());
        }
        match self.observers.load_run(&run_id) {
            Ok(mut run) if run.status == RunStatus::Running => {
                if now < run.deadline {
                    return Ok(());
                }
                self.lose_run(&lock, &mut record, &mut run, now)?;
                tracing::info!(run = %run_id, "observers: run lost by the worker");
                pass.lost.push(run_id);
            }
            Ok(_) | Err(ObserverError::RunNotFound(_)) => {
                record.active_run = None;
                self.observers.save(&lock, &mut record, now)?;
            }
            Err(error) => return Err(error),
        }
        Ok(())
    }

    /// Ends `run` as lost: its token stops working, the observer's state
    /// and failure count stay as they are (the cause is the infrastructure,
    /// not the script), and a scheduled run is next due one interval on.
    fn lose_run(
        &self,
        lock: &ObserverLock,
        record: &mut ObserverRecord,
        run: &mut ObserverRun,
        now: DateTime<Utc>,
    ) -> Result<(), ObserverError> {
        run.status = RunStatus::Lost;
        run.finished_at = Some(now);
        run.error = Some(LOST_MESSAGE.to_string());
        run.token_digest = None;
        record.active_run = None;
        record.last_run = Some(LastRun {
            sequence: run.sequence,
            status: RunStatus::Lost,
            finished_at: now,
            error: run.error.clone(),
        });
        if !run.dry_run {
            record.next_run_at = Some(after(now, record.config.schedule.interval_secs));
        }
        self.observers.save(lock, record, now)?;
        self.observers.save_run(lock, run)
    }

    /// Takes the run `claim_candidate` names from one observer, under its
    /// lock; `None` when another claim was first or there is nothing to
    /// run after all.
    fn claim_from(
        &self,
        observer_id: &ObserverId,
        now: DateTime<Utc>,
    ) -> Result<Option<ClaimedObserverRun>, ObserverError> {
        let lock = self.observers.lock(observer_id)?;
        let mut record = self.observers.load(observer_id)?;
        let Some((kind, _)) = claim_candidate(&record) else {
            return Ok(None);
        };
        let (dry_run, version, with_secrets) = match (kind, &record.pending_dry_run) {
            (ClaimKind::DryRun, Some(request)) => (
                true,
                request.version,
                request.with_secrets || record.approved_version == Some(request.version),
            ),
            _ => match record.approved_version {
                Some(version) => (false, version, true),
                None => return Ok(None),
            },
        };
        let watches = match record.config.watch_mode {
            WatchMode::WatchList => self.observers.list_watches(observer_id)?,
            WatchMode::All => Vec::new(),
        };
        if !dry_run && record.config.watch_mode == WatchMode::WatchList && watches.is_empty() {
            // The last entry went between the pass and this claim (R6).
            record.run_due_since = None;
            record.next_run_at = Some(after(now, record.config.schedule.interval_secs));
            self.observers.save(&lock, &mut record, now)?;
            return Ok(None);
        }
        let script = self.observers.load_script(observer_id, version)?;
        let sequence = record.last_run_sequence + 1;
        let run_id = RunId::new(observer_id.clone(), sequence);
        let token = BearerToken::issue_observer_run(&run_id.to_string());
        let input = ScriptInput {
            contract_version: SCRIPT_CONTRACT_VERSION,
            observer: ScriptObserver {
                id: observer_id.clone(),
                name: record.config.name.clone(),
                source: record.config.source.clone(),
            },
            condition: record.config.condition.clone(),
            state: record.script_state.clone(),
            watches: watches
                .iter()
                .map(|entry| self.script_watch(entry))
                .collect(),
            dry_run,
            first_run: !record.first_run_done,
            import_backlog: record.config.import_backlog,
        };
        let deadline = after(now, SCRIPT_TIMEOUT_SECS + RUN_DEADLINE_MARGIN_SECS);
        let run = ObserverRun {
            record_version: OBSERVER_RUN_VERSION,
            observer_id: observer_id.clone(),
            sequence,
            dry_run,
            with_secrets,
            script_version: version,
            input: input.clone(),
            claimed_at: now,
            deadline,
            token_digest: Some(token.digest()),
            status: RunStatus::Running,
            finished_at: None,
            exit_code: None,
            error: None,
            stdout: None,
            stderr: None,
            tasks_created: 0,
            planned_effects: Vec::new(),
        };
        record.active_run = Some(sequence);
        record.last_run_sequence = sequence;
        record.last_claim_at = Some(now);
        if dry_run {
            record.pending_dry_run = None;
        } else {
            record.run_due_since = None;
        }
        // The record first: a crash before the run file leaves a record
        // naming a missing run, which the pass clears.
        self.observers.save(&lock, &mut record, now)?;
        self.observers.create_run(&lock, &run)?;
        Ok(Some(ClaimedObserverRun {
            run_id,
            token,
            dry_run,
            script_version: version,
            script,
            input,
            secrets: if with_secrets {
                record.config.secrets.clone()
            } else {
                Vec::new()
            },
            timeout_secs: SCRIPT_TIMEOUT_SECS,
            deadline,
        }))
    }

    /// A watch entry as its script sees it, with the `wait` episode its
    /// task is in now (`None` when the task waits for no event or cannot be
    /// read).
    fn script_watch(&self, entry: &WatchEntry) -> ScriptWatch {
        ScriptWatch {
            object: entry.object.clone(),
            task_id: entry.task_id.clone(),
            event_types: entry.event_types.clone(),
            wait_episode: self.wait_episode(&entry.task_id),
        }
    }

    /// The id of the `wait` episode `task_id` is in, from its record.
    fn wait_episode(&self, task_id: &TaskId) -> Option<String> {
        self.control
            .task(task_id)
            .ok()?
            .event_wait
            .map(|wait| wait.episode_id)
    }

    /// Removes the observer's watch entries whose task ended, was archived
    /// or no longer exists (R7). Returns the entries left and how many went.
    fn prune_watches(
        &self,
        lock: &ObserverLock,
    ) -> Result<(Vec<WatchEntry>, usize), ObserverError> {
        let mut remaining = Vec::new();
        let mut removed = 0;
        for entry in self.observers.list_watches(lock.observer_id())? {
            if self.watched_task_ended(&entry.task_id) {
                if self.observers.remove_watch(lock, &entry.entry_id)? {
                    removed += 1;
                }
            } else {
                remaining.push(entry);
            }
        }
        Ok((remaining, removed))
    }

    /// Whether a watch entry's task can receive no more events: finished,
    /// archived (or being archived), or gone. A task that cannot be read
    /// for another reason keeps its entry.
    fn watched_task_ended(&self, task_id: &TaskId) -> bool {
        match self.control.task(task_id) {
            Ok(task) => {
                task.archive.is_some()
                    || task.runner_state.is_archival()
                    || task.task_state() == TaskState::Finished
            }
            Err(OrchestrationError::TaskNotFound(_)) => true,
            Err(error) => {
                tracing::warn!(task = %task_id, %error, "observers: cannot read a watched task");
                false
            }
        }
    }
}
