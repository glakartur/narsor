//! The observers' schedule (R6, R7, R14, R16, R17, R25, R29, R30): which
//! observer is due, the worker's claim of a run, and what the run's result
//! changes.
//!
//! ```text
//! observer_pass   a tick (next_run_at <= now), active,  → next_run_at = the next tick
//!                 approved, idle                          after this one (KTD-3)
//!                   always                              → run_due_since = now: one run,
//!                                                         no entry
//!                   on_episodes without entries         → no run, no error
//!                   on_episodes with entries            → run_due_since = now,
//!                                                         entry_round = the entries
//!                   a task of the queue that waits in   → its entry added back
//!                   its episode with no entry
//!                   an entry whose task no longer waits → removed (R18, R19)
//!                   in its episode, ended or is gone
//!                 a tick while a run is active or a     → skipped, not queued:
//!                 round goes on                           next_run_at = the next tick
//!                 run past its deadline                 → lost: error file, token spent
//! claim           a run past its deadline               → lost (as in the pass)
//!                 the oldest due                        → active_run + token (once);
//!                                                         an on_episodes observer's run
//!                                                         takes the round's next entry
//!                                                         whose task still waits in its
//!                                                         episode; the others are removed,
//!                                                         never run (R18)
//! finish (tasks)  exit 0 + valid output                 → state saved, failures = 0, no file
//!                 anything else                         → error file, state kept,
//!                                                         failures + 1, backoff
//! finish (entry)  exit 0                                → the entry stays on its queue
//!                 anything else                         → error file (it replaces the
//!                                                         entry's older error), the entry
//!                                                         stays; no backoff
//!                 either way                            → the entry's last run time
//!                                                         (the queues view, R23)
//! ```
//!
//! The next due time counts from the tick, never from the end of a run
//! (KTD-3): the pass sets it when the tick comes. A tick that finds the
//! observer's run still active, or its round still going, is skipped, so an
//! observer owes no run for it (R29). The backoff after failed runs moves
//! `next_run_at` past the ticks it covers, so a tick in the backoff never
//! comes; a success or a lost run leaves the next tick as the tick set it,
//! past any tick the run outlasted.
//!
//! An `on_episodes` observer checks one entry of its queue per run (R30).
//! A round holds the entries that were there at the tick; while entries are
//! left in it, the observer stays due, so the next entry is claimed as soon
//! as the run before has its result. The next round comes at the first tick
//! after the round ends. An entry stays on its queue until its gate exits,
//! so it is checked again in every round until then. The entries are the
//! queue's, in the owner's namespace (R7): every observer of the queue
//! checks them on its own schedule (R8). An `always` observer runs once per
//! tick, with no entry (R25).
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

use chrono::{DateTime, Utc};
use narsor_protocol::controller::{ObserverSecretRef, ObserverWorkerRun};
use serde::Serialize;

use crate::controller_protocol::credentials::{
    observer_run_token_id, token_matches_digest, BearerToken,
};
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::ProtocolService;
use crate::observers::{
    parse_script_output, secs_duration, ActiveRun, LastRun, ObserverError, ObserverId,
    ObserverLock, ObserverMode, ObserverRecord, ObserverState, QueueEntry, RunEntry, RunFailure,
    RunId, RunStatus, ScriptChoice, ScriptEntry, ScriptInput, ScriptObserver, ScriptOutput,
    ScriptVariable, SecretRef, ENTRY_DONE_EXIT_CODE, MAX_SCRIPT_OUTPUT_BYTES, RUN_FAILURE_VERSION,
    SCRIPT_CONTRACT_VERSION,
};
use crate::orchestration::OwnerRef;

use super::entries::EntryWait;

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
    /// `on_episodes` observers that were due with no queue entry: no run,
    /// the next deadline moved to the schedule's next tick (R30).
    pub skipped: Vec<ObserverId>,
    /// Observers whose tick came while their run was active or their round
    /// went on: no run for that tick, the next deadline moved to the
    /// schedule's next tick (R29).
    pub busy: Vec<ObserverId>,
    /// Runs that passed their deadline without a result.
    pub lost: Vec<RunId>,
    /// Queue entries removed because their task no longer waits in their
    /// episode: the gate exited, or the task ended, was archived or is gone
    /// (R18, R19).
    pub entries_removed: usize,
    /// Queue entries added back for tasks that wait in a watched episode
    /// with none: the control plane stopped between the task's record and
    /// the entry.
    pub entries_restored: usize,
}

/// A run handed to a worker. The token is in no record: this is the only
/// time anyone sees it.
#[derive(Debug, Clone, Serialize)]
pub struct ClaimedObserverRun {
    /// The run.
    pub run_id: RunId,
    /// The observer's owner: the worker reads the run's secrets from the
    /// owner's part of its secrets directory.
    pub owner: OwnerRef,
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
            owner: self.owner.0,
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

/// Whether the observer's tick came while its run is active or its round
/// goes on: the tick is skipped, not queued (R29).
fn tick_while_busy(record: &ObserverRecord, now: DateTime<Utc>) -> bool {
    record.state == ObserverState::Active
        && (record.active_run.is_some() || record.run_due_since.is_some())
        && record.next_run_at.is_some_and(|at| at <= now)
}

/// Moves a tick that passed by `now` to the schedule's next tick after it
/// (KTD-3); a tick still to come stays.
fn skip_passed_tick(record: &mut ObserverRecord, now: DateTime<Utc>) {
    if let Some(at) = record.next_run_at.filter(|at| *at <= now) {
        record.next_run_at = Some(record.config.schedule.next_tick_after(at, now));
    }
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

/// How a run ended, as [`judge_tasks_run`] and [`judge_entry_run`] read
/// the worker's report.
struct Verdict<T> {
    status: RunStatus,
    exit_code: Option<i32>,
    /// Why it failed; `None` for a success.
    error: Option<String>,
    /// What a success yields; `None` for a failure.
    success: Option<T>,
}

impl<T> Verdict<T> {
    fn failure(status: RunStatus, exit_code: Option<i32>, error: String) -> Self {
        Self {
            status,
            exit_code,
            error: Some(error),
            success: None,
        }
    }

    fn success(exit_code: i32, value: T) -> Self {
        Self {
            status: RunStatus::Succeeded,
            exit_code: Some(exit_code),
            error: None,
            success: Some(value),
        }
    }
}

/// The script's exit code, or the verdict of a report that is a failure
/// whatever the contract: no plain exit.
fn exit_code_or_failure<T>(outcome: &RunOutcome) -> Result<i32, Verdict<T>> {
    let verdict = match outcome {
        RunOutcome::Exited { exit_code } => return Ok(*exit_code),
        RunOutcome::OutputTooLarge { exit_code } => Verdict::failure(
            RunStatus::InvalidOutput,
            Some(*exit_code),
            format!("the script printed more than {MAX_SCRIPT_OUTPUT_BYTES} bytes on stdout"),
        ),
        RunOutcome::TimedOut => Verdict::failure(
            RunStatus::TimedOut,
            None,
            format!("the script ran past its time limit of {SCRIPT_TIMEOUT_SECS} seconds"),
        ),
        RunOutcome::WorkerError { message } => {
            Verdict::failure(RunStatus::WorkerError, None, bounded(message))
        }
        RunOutcome::Unrecognized => Verdict::failure(
            RunStatus::WorkerError,
            None,
            "the worker reported an outcome this server does not know".to_string(),
        ),
    };
    Err(verdict)
}

/// A script's exit with a code the contract does not accept.
fn exited_with<T>(exit_code: i32) -> Verdict<T> {
    Verdict::failure(
        RunStatus::Failed,
        Some(exit_code),
        format!("the script exited with code {exit_code}"),
    )
}

/// The run of an observer that creates tasks: exit 0 with a valid output
/// document is the only success (R14).
fn judge_tasks_run(report: &RunReport) -> Verdict<ScriptOutput> {
    match exit_code_or_failure(&report.outcome) {
        Ok(0) => match parse_script_output(report.stdout.as_bytes()) {
            Ok(output) => Verdict::success(0, output),
            Err(error) => Verdict::failure(RunStatus::InvalidOutput, Some(0), error.to_string()),
        },
        Ok(exit_code) => exited_with(exit_code),
        Err(verdict) => verdict,
    }
}

/// A run for a queue entry: exit 0 is the only success, whether the script
/// answered the gate or had nothing to report (R14). Stdout is not read,
/// and no exit code removes the entry.
fn judge_entry_run(report: &RunReport) -> Verdict<()> {
    match exit_code_or_failure(&report.outcome) {
        Ok(ENTRY_DONE_EXIT_CODE) => Verdict::success(ENTRY_DONE_EXIT_CODE, ()),
        Ok(exit_code) => exited_with(exit_code),
        Err(verdict) => verdict,
    }
}

/// Counts a failed run of an `always` observer: the next run waits
/// `period × 2^failures` (the interval, or the gap to the next cron fire;
/// a cron schedule then waits for its first fire from there on), and after
/// [`MAX_CONSECUTIVE_FAILURES`] the observer goes to `error` (R16).
fn count_failure(record: &mut ObserverRecord, now: DateTime<Utc>) {
    record.consecutive_failures = record.consecutive_failures.saturating_add(1);
    let schedule = &record.config.schedule;
    let delay = retry_delay_secs(schedule.period_secs(now), record.consecutive_failures);
    record.next_run_at = Some(schedule.first_fire_from(now, delay));
    if record.consecutive_failures >= MAX_CONSECUTIVE_FAILURES
        && record.state == ObserverState::Active
    {
        record.state = ObserverState::Error;
        tracing::info!(observer = %record.observer_id, failures = record.consecutive_failures, "observers: too many failed runs; waiting for the owner");
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
            let lost = record
                .active_run
                .as_ref()
                .is_some_and(|active| active.is_past_deadline(now));
            let result = if lost {
                self.expire_run_if_lost(&record.observer_id, now)
                    .map(|lost| pass.lost.extend(lost))
            } else if tick_while_busy(record, now) {
                self.skip_busy_tick(&record.observer_id, now, &mut pass)
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
        let now = self.control.now();
        let records = self.observers.list_readable()?;
        let mut candidates: Vec<(DateTime<Utc>, ObserverId)> = Vec::new();
        for record in records {
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
    /// For an observer that creates tasks, exit code 0 with a valid output
    /// document saves the new state, marks the first run done and clears
    /// the failure count; it writes no file. Any other result writes the
    /// run's error file, keeps the state, counts a failure and waits
    /// `interval × 2^failures` (at most 1 h); after
    /// [`MAX_CONSECUTIVE_FAILURES`] the observer goes to `error` (R14, R16).
    ///
    /// For a run for a queue entry, exit code 0 is the only success. Any
    /// other result writes the run's error file, which replaces the older
    /// error of the same entry (R23), and neither counts a failure nor moves
    /// the observer to `error` (R14). The entry stays on its queue either
    /// way.
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
        // The next tick was set when this run's tick came; one the run
        // outlasted is skipped, not owed (KTD-3, R29).
        skip_passed_tick(&mut record, now);
        let (status, exit_code, error) = match &active.entry {
            Some(_) => {
                let verdict = judge_entry_run(&report);
                if verdict.success.is_some() {
                    record.first_run_done = true;
                }
                // A failed entry is checked again in the next round: no
                // backoff, no count towards the error state (R14).
                // The round's next entry waits for a claim from now, not
                // from this run's claim.
                if record.run_due_since.is_some() {
                    record.run_due_since = Some(now);
                }
                (verdict.status, verdict.exit_code, verdict.error)
            }
            None => {
                let verdict = judge_tasks_run(&report);
                match verdict.success {
                    Some(output) => {
                        record.script_state = Some(output.state);
                        record.first_run_done = true;
                        record.consecutive_failures = 0;
                    }
                    None => count_failure(&mut record, now),
                }
                (verdict.status, verdict.exit_code, verdict.error)
            }
        };
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
                    entry_id: active.entry.as_ref().map(|entry| entry.entry_id.clone()),
                },
            )?;
        }
        record.active_run = None;
        if let Some(entry) = &active.entry {
            record.entry_last_runs.insert(entry.entry_id.clone(), now);
        }
        let last_run = LastRun {
            sequence: active.sequence,
            status,
            finished_at: now,
            error,
        };
        record.last_run = Some(last_run.clone());
        self.observers.save(&lock, &mut record, now)?;
        Ok(last_run)
    }

    /// A new key for the server's observer worker, shown this once (D5).
    /// The key issued before stops working: the server calls this at every
    /// start of the worker, so a key outlives neither its worker nor the
    /// server.
    pub fn issue_observer_worker_key(&self) -> BearerToken {
        self.observer_worker_key.issue()
    }

    /// Whether `bearer` is the observer worker's current key: it may claim
    /// runs of every owner and report their results, nothing else.
    pub fn authenticate_observer_worker(&self, bearer: &str) -> bool {
        self.observer_worker_key.matches(bearer)
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

    /// Takes the observer's tick: the next tick counts from this one
    /// (KTD-3). Flags the scheduled run due, unless the observer is
    /// `on_episodes` and its queue is empty once the entries of ended tasks
    /// are gone and missing ones are back (R19, R30). An `on_episodes`
    /// observer's round is the entries left; an `always` observer's run has
    /// no entry (R25).
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
        let tick = record.next_run_at.unwrap_or(now);
        record.next_run_at = Some(record.config.schedule.next_tick_after(tick, now));
        if record.config.mode == ObserverMode::OnEpisodes {
            pass.entries_restored +=
                self.restore_queue_entries(&record.owner, &record.config.queue);
            let (remaining, removed) = self.prune_entries(&record)?;
            pass.entries_removed += removed;
            // The last runs of entries that left the queue are of no use.
            record
                .entry_last_runs
                .retain(|entry_id, _| remaining.iter().any(|entry| &entry.entry_id == entry_id));
            if remaining.is_empty() {
                self.observers.save(&lock, &mut record, now)?;
                pass.skipped.push(observer_id.clone());
                return Ok(());
            }
            record.entry_round = remaining.into_iter().map(|entry| entry.entry_id).collect();
        }
        record.run_due_since = Some(now);
        self.observers.save(&lock, &mut record, now)?;
        pass.due.push(observer_id.clone());
        Ok(())
    }

    /// Skips the tick of an observer whose run is active or whose round goes
    /// on (R29): no run for it, the next tick counts from it.
    fn skip_busy_tick(
        &self,
        observer_id: &ObserverId,
        now: DateTime<Utc>,
        pass: &mut ObserverPass,
    ) -> Result<(), ObserverError> {
        let lock = self.observers.lock(observer_id)?;
        let mut record = self.observers.load(observer_id)?;
        if !tick_while_busy(&record, now) {
            return Ok(());
        }
        skip_passed_tick(&mut record, now);
        self.observers.save(&lock, &mut record, now)?;
        tracing::debug!(observer = %observer_id, "observers: a tick came while a run was active; skipped");
        pass.busy.push(observer_id.clone());
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
    /// scheduled run is next due at the next tick the run did not outlast.
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
                entry_id: active.entry.as_ref().map(|entry| entry.entry_id.clone()),
            },
        )?;
        record.active_run = None;
        if let Some(entry) = &active.entry {
            record.entry_last_runs.insert(entry.entry_id.clone(), now);
        }
        record.last_run = Some(LastRun {
            sequence: active.sequence,
            status: RunStatus::Lost,
            finished_at: now,
            error: Some(LOST_MESSAGE.to_string()),
        });
        skip_passed_tick(record, now);
        // The round's next entry waits for a claim from now (R30).
        if record.run_due_since.is_some() {
            record.run_due_since = Some(now);
        }
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
        let entry = match record.config.mode {
            ObserverMode::OnEpisodes => match self.next_round_entry(&mut record)? {
                Some(entry) => Some(entry),
                None => {
                    // The round's entries went between the pass and this
                    // claim (R30); the next round comes at the next tick.
                    record.run_due_since = None;
                    skip_passed_tick(&mut record, now);
                    self.observers.save(&lock, &mut record, now)?;
                    return Ok(None);
                }
            },
            ObserverMode::Always => {
                record.entry_round.clear();
                None
            }
        };
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
            entry: entry.as_ref().map(script_entry),
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
            entry: entry.map(|entry| RunEntry {
                entry_id: entry.entry_id,
                task_id: entry.task_id,
                episode: entry.episode,
            }),
        });
        record.last_run_sequence = sequence;
        record.last_claim_at = Some(now);
        // Entries left in the round keep the observer due: the next one is
        // claimed once this run has its result.
        record.run_due_since = (!record.entry_round.is_empty()).then_some(now);
        self.observers.save(&lock, &mut record, now)?;
        Ok(Some(ClaimedObserverRun {
            run_id,
            owner: record.owner.clone(),
            token,
            script_version: version,
            script,
            input,
            secrets: record.config.secrets.clone(),
            timeout_secs: SCRIPT_TIMEOUT_SECS,
            deadline,
        }))
    }

    /// Takes the next entry of the round that is still on the observer's
    /// queue and whose task still waits in its episode, `None` when the
    /// round has none left.
    ///
    /// The guard before a run (R18): the task is read again, and an entry
    /// whose gate exited, or whose task ended, was archived or is gone, is
    /// removed and not run. The runner's snapshot removes it first as a
    /// rule (R16); this catches a snapshot the control plane did not see
    /// through. An entry whose task cannot be read is kept, but not run;
    /// so is an entry whose own file cannot be read.
    fn next_round_entry(
        &self,
        record: &mut ObserverRecord,
    ) -> Result<Option<QueueEntry>, ObserverError> {
        while !record.entry_round.is_empty() {
            let entry_id = record.entry_round.remove(0);
            let entry = match self.observers.queue_entry(&entry_id) {
                Ok(Some(entry)) => entry,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(entry = %entry_id, %error, "observers: cannot read a queue entry of the round; it is not run");
                    continue;
                }
            };
            if !entry.is_in(&record.owner, &record.config.queue) {
                continue;
            }
            match self.entry_wait(&entry) {
                EntryWait::Waits => return Ok(Some(entry)),
                EntryWait::Gone => {
                    let lock = self.observers.lock_queue_entries()?;
                    if self.observers.remove_queue_entry(&lock, &entry.entry_id)? {
                        tracing::info!(task = %entry.task_id, episode = %entry.episode, "observers: the task no longer waits in the episode of a queue entry; the entry is removed, not run");
                    }
                }
                EntryWait::Unknown => {}
            }
        }
        Ok(None)
    }

    /// Removes the entries of the observer's queue whose task no longer
    /// waits in their episode, ended, was archived or no longer exists
    /// (R18, R19), and the unreadable entry files of episodes no task waits
    /// in. Returns the entries left and how many went.
    fn prune_entries(
        &self,
        record: &ObserverRecord,
    ) -> Result<(Vec<QueueEntry>, usize), ObserverError> {
        let scan = self.observers.scan_queue_entries()?;
        let mut removed = self.prune_unreadable_entries(&scan.unreadable);
        let (gone, remaining): (Vec<QueueEntry>, Vec<QueueEntry>) = scan
            .entries
            .into_iter()
            .filter(|entry| entry.is_in(&record.owner, &record.config.queue))
            .partition(|entry| self.entry_wait(entry) == EntryWait::Gone);
        if !gone.is_empty() {
            let lock = self.observers.lock_queue_entries()?;
            for entry in &gone {
                if self.observers.remove_queue_entry(&lock, &entry.entry_id)? {
                    removed += 1;
                }
            }
        }
        Ok((remaining, removed))
    }
}

/// A queue entry as its script sees it (contract version 2, R11).
fn script_entry(entry: &QueueEntry) -> ScriptEntry {
    ScriptEntry {
        task_id: entry.task_id.clone(),
        episode: entry.episode.clone(),
        episode_started_at: entry.created_at,
        step: entry.step.clone(),
        params: entry.params.clone(),
        choices: entry
            .choices
            .iter()
            .map(|choice| ScriptChoice {
                name: choice.name.clone(),
                description: choice.description.clone(),
                requires_variables: choice.requires_variables.clone(),
            })
            .collect(),
        variables: entry
            .variables
            .iter()
            .map(|variable| ScriptVariable {
                name: variable.name.clone(),
                variable_type: variable.variable_type.clone(),
                description: variable.description.clone(),
                options: variable.options.clone(),
            })
            .collect(),
    }
}
