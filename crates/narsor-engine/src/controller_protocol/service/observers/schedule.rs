//! The observers' schedule (R6, R7, R14, R16, R17): which observer is due,
//! the worker's claim of a run, and what the run's result changes.
//!
//! ```text
//! observer_pass   next_run_at <= now, active, approved  → run_due_since = now
//!                 watch_list without entries            → next_run_at += interval, no run
//!                 run past its deadline                 → lost: error file, token spent
//! claim           a run past its deadline               → lost (as in the pass)
//!                 the oldest due                        → active_run + token (once)
//! finish          exit 0 + valid output                 → state saved, failures = 0, no file
//!                 anything else                         → error file, state kept,
//!                                                         failures + 1, backoff
//! ```
//!
//! The pass only flags; the run appears at the claim, as the record's
//! `active_run`. The flag is one value, not a queue, so a control plane that
//! stood still for hours owes at most one run per observer afterwards.
//! Deadlines, counters, the flag and the run in progress live in the
//! observer's record, which survives a restart. The script's input is built
//! at the claim and handed to the worker only; it is never stored (D15).
//!
//! Only an execution error leaves a file ([`RunFailure`], written before the
//! record is saved): a crash between the two leaves the run active, and it
//! is found lost at its deadline, which writes the same run's error again.
//!
//! The pass and the claim list the observers leniently: a record that
//! cannot be read is logged and skipped, so it does not stop the others
//! (D7).
//!
//! The pass runs on the daemon's 5-second tick and must stay cheap: when
//! nothing is due it reads the observers' records and takes no lock.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use narsor_protocol::controller::{ObserverSecretRef, ObserverWorkerRun};
use serde::Serialize;

use crate::controller_protocol::credentials::{
    observer_run_token_id, token_matches_digest, BearerToken,
};
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::ProtocolService;
use crate::domain::TaskId;
use crate::observers::{
    parse_script_output, secs_duration, ActiveRun, LastRun, ObserverError, ObserverId,
    ObserverLock, ObserverRecord, ObserverState, RunFailure, RunId, RunStatus, ScriptInput,
    ScriptObserver, ScriptOutput, ScriptWatch, SecretRef, WatchEntry, WatchMode,
    MAX_SCRIPT_OUTPUT_BYTES, RUN_FAILURE_VERSION, SCRIPT_CONTRACT_VERSION,
};
use crate::orchestration::{OrchestrationError, OwnerRef, TaskState};

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

/// Most bytes of stdout, stderr or an error message an error file keeps.
/// The whole stdout (up to the contract's limit) is parsed first.
pub const MAX_STORED_LOG_BYTES: usize = 64 * 1024;

/// Why a lost run ended, as its error file and `observer show` say.
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
    /// The script version it runs.
    pub script_version: u32,
    /// The script itself.
    pub script: String,
    /// What the script reads on stdin.
    pub input: ScriptInput,
    /// The secrets the worker hands to the script: the observer's
    /// references.
    pub secrets: Vec<SecretRef>,
    /// How long the worker lets the script run.
    pub timeout_secs: u64,
    /// No result by then: the run is lost and its token stops working.
    pub deadline: DateTime<Utc>,
}

impl ClaimedObserverRun {
    /// The claim as the worker route answers it.
    pub fn into_wire(self) -> ObserverWorkerRun {
        ObserverWorkerRun {
            run_id: self.run_id.to_string(),
            token: self.token.expose().to_string(),
            script_version: self.script_version,
            script: self.script,
            // A `ScriptInput` is plain data: it always serializes.
            input: serde_json::to_value(&self.input).unwrap_or_default(),
            secrets: self
                .secrets
                .into_iter()
                .map(|secret| ObserverSecretRef {
                    name: secret.name,
                    env: secret.env,
                })
                .collect(),
            timeout_secs: self.timeout_secs,
            deadline: self.deadline,
        }
    }
}

/// How a run ended, as the worker saw it (the wire type of the worker's
/// result route).
pub use narsor_protocol::controller::ObserverRunOutcome as RunOutcome;
/// The worker's result of a run, secrets masked.
pub use narsor_protocol::controller::ObserverRunReport as RunReport;

/// Since when the scheduled run the pass flagged waits for a claim, `None`
/// when `record` has no run to claim.
fn claim_candidate(record: &ObserverRecord) -> Option<DateTime<Utc>> {
    if record.active_run.is_some()
        || record.state != ObserverState::Active
        || record.approved_version.is_none()
    {
        return None;
    }
    record.run_due_since
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
        RunOutcome::OutputTooLarge { exit_code } => (
            RunStatus::InvalidOutput,
            Some(*exit_code),
            Some(format!(
                "the script printed more than {MAX_SCRIPT_OUTPUT_BYTES} bytes on stdout"
            )),
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
        let records = match self.observers.list_readable() {
            Ok(records) => records,
            Err(error) => {
                tracing::warn!(%error, "observers: cannot list observers");
                return ObserverPass::default();
            }
        };
        let now = self.control.now();
        let mut pass = ObserverPass::default();
        for record in &records {
            let result = if let Some(active) = &record.active_run {
                if active.is_past_deadline(now) {
                    self.expire_run_if_lost(&record.observer_id, now)
                        .map(|lost| pass.lost.extend(lost))
                } else {
                    Ok(())
                }
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

    /// Hands the next run to a worker: the scheduled run due the longest. `None` when no run is due.
    ///
    /// The claim writes the run into the observer's record (`active_run`)
    /// with its token's digest under the observer's lock, so two workers
    /// never get the same run and an observer never has two runs at once.
    /// A run past its deadline is recorded as lost first. An observer that
    /// fails to yield its run, or whose record cannot be read, is logged
    /// and skipped, so it does not block the rest.
    pub fn claim_observer_run(&self) -> ProtocolResult<Option<ClaimedObserverRun>> {
        self.claim_next_run(None)
    }

    /// Like [`Self::claim_observer_run`], for a worker of `owner`: only the
    /// runs of `owner`'s observers.
    pub fn claim_observer_run_for(
        &self,
        owner: &OwnerRef,
    ) -> ProtocolResult<Option<ClaimedObserverRun>> {
        self.claim_next_run(Some(owner))
    }

    /// Like [`Self::finish_observer_run`], for a worker of `owner`: the run
    /// of an observer of anyone else is not found.
    pub fn finish_observer_run_for(
        &self,
        owner: &OwnerRef,
        run_id: &RunId,
        report: RunReport,
    ) -> ProtocolResult<LastRun> {
        match self.observers.load(&run_id.observer_id) {
            Ok(record) if &record.owner == owner => {}
            Ok(_) | Err(ObserverError::NotFound(_)) => return Err(ProtocolError::NotFound),
            Err(error) => return Err(error.into()),
        }
        self.finish_observer_run(run_id, report)
    }

    /// The claim of [`Self::claim_observer_run`], among the observers of
    /// `owner` when given.
    fn claim_next_run(
        &self,
        owner: Option<&OwnerRef>,
    ) -> ProtocolResult<Option<ClaimedObserverRun>> {
        let now = self.control.now();
        let records = self.observers.list_readable()?;
        let mut candidates: Vec<(DateTime<Utc>, ObserverId)> = Vec::new();
        for record in records
            .into_iter()
            .filter(|record| owner.is_none_or(|owner| &record.owner == owner))
        {
            if record
                .active_run
                .as_ref()
                .is_some_and(|active| active.is_past_deadline(now))
            {
                if let Err(error) = self.expire_run_if_lost(&record.observer_id, now) {
                    tracing::warn!(observer = %record.observer_id, %error, "observers: cannot record a lost run");
                }
            } else if let Some(since) = claim_candidate(&record) {
                candidates.push((since, record.observer_id));
            }
        }
        candidates.sort();
        for (_, observer_id) in candidates {
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
    /// entries the script reported closed; it writes no file. Any other
    /// result writes the run's error file, keeps the state, counts a failure
    /// and waits `interval × 2^failures` (at most 1 h); after
    /// [`MAX_CONSECUTIVE_FAILURES`] the observer goes to `error` (R14, R16).
    ///
    /// The run's token stops working. A run already ended, or past its
    /// deadline (it is then recorded as lost), is refused with a conflict.
    pub fn finish_observer_run(
        &self,
        run_id: &RunId,
        report: RunReport,
    ) -> ProtocolResult<LastRun> {
        let observer_id = &run_id.observer_id;
        let lock = self.observers.lock(observer_id)?;
        let mut record = self.observers.load(observer_id)?;
        let Some(active) = record
            .active_run
            .clone()
            .filter(|active| active.sequence == run_id.sequence)
        else {
            return Err(ProtocolError::conflict(
                "observer_run_ended",
                format!("run {run_id} has already ended"),
            ));
        };
        let now = self.control.now();
        if active.is_past_deadline(now) {
            self.lose_run(&lock, &mut record, &active, now)?;
            return Err(ProtocolError::conflict(
                "observer_run_lost",
                format!("run {run_id} passed its deadline and was recorded as lost"),
            ));
        }
        let (status, exit_code, error, output) = judge(&report);
        if let Some(message) = &error {
            self.observers.record_failure(
                &lock,
                &RunFailure {
                    record_version: RUN_FAILURE_VERSION,
                    observer_id: observer_id.clone(),
                    sequence: active.sequence,
                    script_version: active.script_version,
                    claimed_at: active.claimed_at,
                    finished_at: now,
                    status,
                    exit_code,
                    error: message.clone(),
                    stdout: Some(bounded(&report.stdout)),
                    stderr: Some(bounded(&report.stderr)),
                },
            )?;
        }
        record.active_run = None;
        let last_run = LastRun {
            sequence: active.sequence,
            status,
            finished_at: now,
            error,
        };
        record.last_run = Some(last_run.clone());
        let interval = record.config.schedule.interval_secs;
        let mut closed = BTreeSet::new();
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
        self.observers.save(&lock, &mut record, now)?;
        if !closed.is_empty() {
            for entry in self.observers.list_watches(observer_id)? {
                if closed.contains(&entry.object) {
                    self.observers.remove_watch(&lock, &entry.entry_id)?;
                }
            }
        }
        Ok(last_run)
    }

    /// The run an observer run token proves, `None` for anything that is
    /// not the token of the observer's run in progress before its deadline:
    /// a token of another kind, a spent token (result in, or lost), a wrong
    /// secret. The secret is compared with the digest in the observer's
    /// record in constant time.
    pub fn authenticate_observer_run(&self, bearer: &str) -> Option<RunId> {
        let run_id = RunId::parse(observer_run_token_id(bearer)?).ok()?;
        let record = self.observers.load(&run_id.observer_id).ok()?;
        let active = record.active_run.as_ref()?;
        let valid = active.sequence == run_id.sequence
            && token_matches_digest(bearer, &active.token_digest)
            && !active.is_past_deadline(self.control.now());
        valid.then_some(run_id)
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

    /// Records the observer's run in progress as lost if it passed its
    /// deadline, under the observer's lock. Returns the run lost, if any.
    fn expire_run_if_lost(
        &self,
        observer_id: &ObserverId,
        now: DateTime<Utc>,
    ) -> Result<Option<RunId>, ObserverError> {
        let lock = self.observers.lock(observer_id)?;
        let mut record = self.observers.load(observer_id)?;
        let Some(active) = record
            .active_run
            .clone()
            .filter(|active| active.is_past_deadline(now))
        else {
            return Ok(None);
        };
        self.lose_run(&lock, &mut record, &active, now)?;
        let run_id = active.run_id(observer_id);
        tracing::info!(run = %run_id, "observers: run lost by the worker");
        Ok(Some(run_id))
    }

    /// Ends the run `active` as lost: its error file is written, its token
    /// stops working, the observer's state and failure count stay as they
    /// are (the cause is the infrastructure, not the script), and a
    /// scheduled run is next due one interval on.
    fn lose_run(
        &self,
        lock: &ObserverLock,
        record: &mut ObserverRecord,
        active: &ActiveRun,
        now: DateTime<Utc>,
    ) -> Result<(), ObserverError> {
        self.observers.record_failure(
            lock,
            &RunFailure {
                record_version: RUN_FAILURE_VERSION,
                observer_id: record.observer_id.clone(),
                sequence: active.sequence,
                script_version: active.script_version,
                claimed_at: active.claimed_at,
                finished_at: now,
                status: RunStatus::Lost,
                exit_code: None,
                error: LOST_MESSAGE.to_string(),
                stdout: None,
                stderr: None,
            },
        )?;
        record.active_run = None;
        record.last_run = Some(LastRun {
            sequence: active.sequence,
            status: RunStatus::Lost,
            finished_at: now,
            error: Some(LOST_MESSAGE.to_string()),
        });
        record.next_run_at = Some(after(now, record.config.schedule.interval_secs));
        self.observers.save(lock, record, now)
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
        if claim_candidate(&record).is_none() {
            return Ok(None);
        }
        let Some(version) = record.approved_version else {
            return Ok(None);
        };
        let watches = match record.config.watch_mode {
            WatchMode::WatchList => self.observers.list_watches(observer_id)?,
            WatchMode::All => Vec::new(),
        };
        if record.config.watch_mode == WatchMode::WatchList && watches.is_empty() {
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
            first_run: !record.first_run_done,
            import_backlog: record.config.import_backlog,
        };
        let deadline = after(now, SCRIPT_TIMEOUT_SECS + RUN_DEADLINE_MARGIN_SECS);
        record.active_run = Some(ActiveRun {
            sequence,
            script_version: version,
            claimed_at: now,
            deadline,
            token_digest: token.digest(),
            tasks_created: 0,
        });
        record.last_run_sequence = sequence;
        record.last_claim_at = Some(now);
        record.run_due_since = None;
        self.observers.save(&lock, &mut record, now)?;
        Ok(Some(ClaimedObserverRun {
            run_id,
            token,
            script_version: version,
            script,
            input,
            secrets: record.config.secrets.clone(),
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
