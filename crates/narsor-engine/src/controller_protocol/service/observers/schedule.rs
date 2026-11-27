//! The automations' schedule (R6, R7, R14, R16, R17, R25, R29, R30): which
//! (queue, automation) pair is due, the worker's claim of a run, and what
//! the run's result changes.
//!
//! The catalogue is read on every pass and every claim, with no cache
//! (R2): the queue gives the schedule, the mode and the task settings; the
//! automation gives the scripts and the secrets. Every automation of a
//! queue is one pair with its own runtime record, so the automations of a
//! queue tick on the same schedule but run independently (R8).
//!
//! ```text
//! observer_pass   a pair of the catalogue without a     → record created, due at once
//!                 record
//!                 a tick (next_run_at <= now), active,  → next_run_at = the next tick
//!                 idle                                    after this one (KTD-3)
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
//!                 error, and the                        → active again, due at once
//!                 queue file, automation.yaml or check
//!                 script changed
//!                 run past its deadline                 → lost: error file, token spent
//!                 a record of a pair the catalogue no   → deleted; kept while a run is
//!                 longer has                              in progress
//! claim           a run past its deadline               → lost (as in the pass)
//!                 an entry script (`in`) due            → its hook run first (`hooks`)
//!                 the oldest due                        → active_run + token (once);
//!                                                         an on_episodes pair's run
//!                                                         takes the round's next entry
//!                                                         whose task still waits in its
//!                                                         episode and whose entry scripts
//!                                                         all succeeded (R9); the others
//!                                                         are removed (gone) or kept, never
//!                                                         run (R18)
//!                 its check script cannot be read       → a failed run: error file, no
//!                                                         worker run
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
//! pair's run still active, or its round still going, is skipped, so a pair
//! owes no run for it (R29). The backoff after failed runs moves
//! `next_run_at` past the ticks it covers, so a tick in the backoff never
//! comes; a success or a lost run leaves the next tick as the tick set it,
//! past any tick the run outlasted.
//!
//! A pair of an `on_episodes` queue checks one entry of its queue per run
//! (R30). A round holds the entries that were there at the tick; while
//! entries are left in it, the pair stays due, so the next entry is claimed
//! as soon as the run before has its result. The next round comes at the
//! first tick after the round ends. An entry stays on its queue until its
//! gate exits, so it is checked again in every round until then. A pair of
//! an `always` queue runs once per tick, with no entry (R25).
//!
//! The pass only flags; the run appears at the claim, as the record's
//! `active_run`, with the digest of the script text the catalogue held then.
//! The flag is one value, not a queue, so a control plane that stood still
//! for hours owes at most one run per pair afterwards. Deadlines, counters,
//! the flag and the run in progress live in the pair's record, which
//! survives a restart. The script's input is built at the claim and handed
//! to the worker only; it is never stored (D15).
//!
//! Every end of a run (result, failure at the claim, lost run) moves
//! `run_due_since` to the end when the round goes on: the round's next
//! entry waits for a claim counted from then.
//!
//! Only an execution error leaves a file ([`RunFailure`], written before the
//! record is saved): a crash between the two leaves the run active, and it
//! is found lost at its deadline, which writes the same run's error again.
//!
//! The pass and the claim list the records leniently: a record that cannot
//! be read is logged and skipped, so it does not stop the others (D7).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, Utc};
use narsor_protocol::controller::{ObserverSecretRef, ObserverWorkerRun};
use serde::Serialize;

use crate::automations::{
    pair_definition_digest, Automation, AutomationCatalogLoad, AutomationScript, HookPhase, Queue,
};
use crate::controller_protocol::credentials::{
    observer_run_token_id, token_matches_digest, BearerToken,
};
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::ProtocolService;
use crate::observers::{
    parse_script_output, secs_duration, ActiveRun, AutomationKey, AutomationLock, AutomationRecord,
    AutomationState, LastRun, ObserverError, ObserverMode, QueueEntry, RunEntry, RunFailure, RunId,
    RunStatus, ScheduleConfig, ScriptChoice, ScriptEntry, ScriptExit, ScriptInput, ScriptOutput,
    ScriptVariable, SecretRef, ENTRY_DONE_EXIT_CODE, MAX_SCRIPT_OUTPUT_BYTES,
    SCRIPT_CONTRACT_VERSION,
};

use crate::orchestration::GATE_HOOK_TIMEOUT_SECS;

use crate::domain::TaskId;
use crate::orchestration::TaskRecord;

use super::entries::{entry_wait_of, EntryWait};

/// Failed runs in a row after which a pair goes to `error` (R16).
pub const MAX_CONSECUTIVE_FAILURES: u32 = 5;

/// Longest wait the backoff after failed runs reaches (1 h). An interval
/// longer than this is kept as it is: a failure never runs a pair more
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

/// Why a lost run ended, as its error file and the queue view say.
pub(super) const LOST_MESSAGE: &str = "lost by the worker: no result before the run's deadline";

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

/// What one pass of the schedule did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObserverPass {
    /// Pairs whose scheduled run became due.
    pub due: Vec<AutomationKey>,
    /// Pairs of `on_episodes` queues that were due with no queue entry: no
    /// run, the next deadline moved to the schedule's next tick (R30).
    pub skipped: Vec<AutomationKey>,
    /// Pairs whose tick came while their run was active or their round
    /// went on: no run for that tick, the next deadline moved to the
    /// schedule's next tick (R29).
    pub busy: Vec<AutomationKey>,
    /// Pairs that got their runtime record on this pass.
    pub created: Vec<AutomationKey>,
    /// Pairs whose record was deleted: no longer in the catalogue.
    pub removed: Vec<AutomationKey>,
    /// Stopped pairs (`error`) back on their schedule
    /// because their catalogue definition changed.
    pub restarted: Vec<AutomationKey>,
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
    /// The script phase it runs.
    pub phase: HookPhase,
    /// The run's token, for the effect routes, valid until the result or
    /// the deadline.
    pub token: BearerToken,
    /// `sha256:` digest of the script text.
    pub script_digest: String,
    /// The script itself, as the catalogue holds it now.
    pub script: String,
    /// What the script reads on stdin.
    pub input: ScriptInput,
    /// The secrets the worker hands to the script: the automation's
    /// references, and no other.
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
            phase: self.phase.as_str().to_string(),
            queue: self.run_id.key.queue.clone(),
            automation: self.run_id.key.automation.clone(),
            token: self.token.expose().to_string(),
            script_digest: self.script_digest,
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

/// The pairs of the catalogue's active queues, with their queue.
fn catalog_pairs(catalog: &AutomationCatalogLoad) -> BTreeMap<AutomationKey, &Queue> {
    let mut pairs = BTreeMap::new();
    for (name, queue) in &catalog.queues {
        for automation in &queue.automations {
            match AutomationKey::new(name.clone(), automation.clone()) {
                Ok(key) => {
                    pairs.insert(key, queue);
                }
                Err(error) => {
                    tracing::warn!(queue = %name, %automation, %error, "automations: a catalogue pair has no usable name");
                }
            }
        }
    }
    pairs
}

/// Since when the scheduled run the pass flagged waits for a claim, `None`
/// when `record` has no run to claim.
fn claim_candidate(record: &AutomationRecord) -> Option<DateTime<Utc>> {
    if record.active_run.is_some() || record.state != AutomationState::Active {
        return None;
    }
    record.run_due_since
}

/// Whether the pair's scheduled run becomes due at `now`: active, no run in
/// progress or waiting, and its deadline come (no deadline at all counts as
/// come).
fn scheduled_run_due(record: &AutomationRecord, now: DateTime<Utc>) -> bool {
    record.state == AutomationState::Active
        && record.active_run.is_none()
        && record.run_due_since.is_none()
        && record.next_run_at.is_none_or(|at| at <= now)
}

/// Whether the pair's tick came while its run is active or its round goes
/// on: the tick is skipped, not queued (R29).
fn tick_while_busy(record: &AutomationRecord, now: DateTime<Utc>) -> bool {
    record.state == AutomationState::Active
        && (record.active_run.is_some() || record.run_due_since.is_some())
        && record.next_run_at.is_some_and(|at| at <= now)
}

/// Moves a tick that passed by `now` to the schedule's next tick after it
/// (KTD-3); a tick still to come stays. Without a schedule (the queue left
/// the catalogue) nothing moves.
fn skip_passed_tick(
    record: &mut AutomationRecord,
    schedule: Option<&ScheduleConfig>,
    now: DateTime<Utc>,
) {
    let Some(schedule) = schedule else {
        return;
    };
    if let Some(at) = record.next_run_at.filter(|at| *at <= now) {
        record.next_run_at = Some(schedule.next_tick_after(at, now));
    }
}

/// `now` plus `secs` seconds, the latest instant when that is too far.
pub(super) fn after(now: DateTime<Utc>, secs: u64) -> DateTime<Utc> {
    now.checked_add_signed(secs_duration(secs))
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

/// `text` cut to at most [`MAX_STORED_LOG_BYTES`], on a character boundary.
pub(super) fn bounded(text: &str) -> String {
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
pub(super) struct Verdict<T> {
    pub(super) status: RunStatus,
    pub(super) exit_code: Option<i32>,
    /// Why it failed; `None` for a success.
    pub(super) error: Option<String>,
    /// What a success yields; `None` for a failure.
    pub(super) success: Option<T>,
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
/// whatever the contract: no plain exit. `timeout_secs` is the run's time
/// limit.
fn exit_code_or_failure<T>(outcome: &RunOutcome, timeout_secs: u64) -> Result<i32, Verdict<T>> {
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
            format!("the script ran past its time limit of {timeout_secs} seconds"),
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

/// A `check` run of an `always` queue: exit 0 with a valid output
/// document is the only success (R14).
fn judge_tasks_run(report: &RunReport) -> Verdict<ScriptOutput> {
    match exit_code_or_failure(&report.outcome, SCRIPT_TIMEOUT_SECS) {
        Ok(0) => match parse_script_output(report.stdout.as_bytes()) {
            Ok(output) => Verdict::success(0, output),
            Err(error) => Verdict::failure(RunStatus::InvalidOutput, Some(0), error.to_string()),
        },
        Ok(exit_code) => exited_with(exit_code),
        Err(verdict) => verdict,
    }
}

/// A run for a queue entry (`check`, `in`, `out`) with the time limit
/// `timeout_secs`: exit 0 is the only success, whether the script answered
/// the gate or had nothing to report (R14). Stdout is not read, and no exit
/// code removes the entry.
pub(super) fn judge_entry_run(report: &RunReport, timeout_secs: u64) -> Verdict<()> {
    match exit_code_or_failure(&report.outcome, timeout_secs) {
        Ok(ENTRY_DONE_EXIT_CODE) => Verdict::success(ENTRY_DONE_EXIT_CODE, ()),
        Ok(exit_code) => exited_with(exit_code),
        Err(verdict) => verdict,
    }
}

/// Counts a failed run of an `always` queue's pair: the next run waits
/// `period × 2^failures` (the interval, or the gap to the next cron fire;
/// a cron schedule then waits for its first fire from there on), and after
/// [`MAX_CONSECUTIVE_FAILURES`] the pair goes to `error` (R16) until its
/// catalogue definition changes from `definition_digest`.
fn count_failure(
    record: &mut AutomationRecord,
    schedule: Option<&ScheduleConfig>,
    definition_digest: &str,
    now: DateTime<Utc>,
) {
    record.consecutive_failures = record.consecutive_failures.saturating_add(1);
    if let Some(schedule) = schedule {
        let delay = retry_delay_secs(schedule.period_secs(now), record.consecutive_failures);
        record.next_run_at = Some(schedule.first_fire_from(now, delay));
    }
    if record.consecutive_failures >= MAX_CONSECUTIVE_FAILURES
        && record.state == AutomationState::Active
    {
        record.state = AutomationState::Error;
        record.stopped_definition_digest = Some(definition_digest.to_string());
        tracing::info!(pair = %record.key, failures = record.consecutive_failures, "automations: too many failed runs; stopped until its catalogue definition changes");
    }
}

/// Whether a run of `record` in progress (scheduled or hook) passed its
/// deadline at `now`.
fn has_lost_run(record: &AutomationRecord, now: DateTime<Utc>) -> bool {
    record
        .runs_in_progress()
        .any(|active| active.is_past_deadline(now))
}

/// The conflict of a result that came after its run's deadline: the run
/// was recorded as lost.
fn run_lost(run_id: &RunId) -> ProtocolError {
    ProtocolError::conflict(
        "observer_run_lost",
        format!("run {run_id} passed its deadline and was recorded as lost"),
    )
}

/// What a claim starts: the script phase, the queue entry it runs for and,
/// for `out`, how the gate was left.
pub(super) struct RunPlan<'a> {
    pub(super) phase: HookPhase,
    pub(super) entry: Option<&'a QueueEntry>,
    pub(super) exit: Option<ScriptExit>,
}

/// The round's next entry waits for a claim from `now`, not from the claim
/// of the run that just ended (R30).
fn round_waits_from(record: &mut AutomationRecord, now: DateTime<Utc>) {
    if record.run_due_since.is_some() {
        record.run_due_since = Some(now);
    }
}

impl ProtocolService {
    /// The catalogue's automations and queues, read now. A change in the
    /// list of files that cannot be used is logged, once per change.
    pub(crate) fn automation_catalog(&self) -> AutomationCatalogLoad {
        let catalog = self.launch.automation_catalog();
        let mut logged = self
            .logged_catalog_problems
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *logged != catalog.problems {
            for problem in &catalog.problems {
                tracing::warn!(file = %problem.file.display(), reason = %problem.reason, "automations: a catalogue file cannot be used");
            }
            logged.clone_from(&catalog.problems);
        }
        catalog
    }

    /// One pass of the schedule; see the module documentation.
    ///
    /// Failures are logged per pair; the next pass retries.
    pub fn observer_pass(&self) -> ObserverPass {
        let catalog = self.automation_catalog();
        let pairs = catalog_pairs(&catalog);
        let records: BTreeMap<AutomationKey, AutomationRecord> =
            match self.observers.list_readable() {
                Ok(records) => records
                    .into_iter()
                    .map(|record| (record.key.clone(), record))
                    .collect(),
                Err(error) => {
                    tracing::warn!(%error, "automations: cannot list the runtime records");
                    return ObserverPass::default();
                }
            };
        let now = self.control.now();
        let mut pass = ObserverPass::default();
        // The entries of the `on_episodes` queues whose tick came are tidied
        // once per queue, before any pair's lock.
        let ticking: BTreeSet<String> = pairs
            .iter()
            .filter(|(key, queue)| {
                queue.mode == ObserverMode::OnEpisodes
                    && records.get(*key).is_none_or(|record| {
                        !has_lost_run(record, now) && scheduled_run_due(record, now)
                    })
            })
            .map(|(key, _)| key.queue.clone())
            .collect();
        let tidied = if ticking.is_empty() {
            BTreeMap::new()
        } else {
            self.tidy_queue_entries(&ticking, &mut pass)
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, "automations: cannot tidy the queue entries");
                    BTreeMap::new()
                })
        };
        for (key, queue) in &pairs {
            let result = match records.get(key) {
                None => self.mark_run_due(key, queue, &tidied, now, &mut pass),
                Some(record) => {
                    if has_lost_run(record, now) {
                        self.expire_run_if_lost(key, Some(&queue.schedule), now)
                            .map(|lost| pass.lost.extend(lost))
                    } else if record.state.is_stopped() {
                        match catalog.automations.get(&key.automation) {
                            Some(automation) => self.restart_if_definition_changed(
                                record, queue, automation, now, &mut pass,
                            ),
                            None => Ok(()),
                        }
                    } else if tick_while_busy(record, now) {
                        self.skip_busy_tick(key, &queue.schedule, now, &mut pass)
                    } else if scheduled_run_due(record, now) {
                        self.mark_run_due(key, queue, &tidied, now, &mut pass)
                    } else {
                        Ok(())
                    }
                }
            };
            if let Err(error) = result {
                tracing::warn!(pair = %key, %error, "automations: pass failed");
            }
        }
        for (key, record) in records.iter().filter(|(key, _)| !pairs.contains_key(key)) {
            let result = if has_lost_run(record, now) {
                self.expire_run_if_lost(key, None, now)
                    .map(|lost| pass.lost.extend(lost))
            } else if record.runs_in_progress().next().is_none() {
                self.remove_record(key, &mut pass)
            } else {
                Ok(())
            };
            if let Err(error) = result {
                tracing::warn!(pair = %key, %error, "automations: pass failed");
            }
        }
        // Ticks flagged, runs lost, retry times come: a waiting claim looks.
        self.wake_observer_claims();
        pass
    }

    /// Hands the next run to a worker: the scheduled run due the longest.
    /// `None` when no run is due.
    ///
    /// The claim writes the run into the pair's record (`active_run`) with
    /// its token's digest under the pair's lock, so two workers never get
    /// the same run and a pair never has two runs at once. A run past its
    /// deadline is recorded as lost first. A pair that fails to yield its
    /// run, or whose record cannot be read, is logged and skipped, so it
    /// does not block the rest. Only pairs the catalogue has now are
    /// claimed.
    pub fn claim_observer_run(&self) -> ProtocolResult<Option<ClaimedObserverRun>> {
        let now = self.control.now();
        let catalog = self.automation_catalog();
        let pairs = catalog_pairs(&catalog);
        let records = self.observers.list_readable()?;
        let mut candidates: Vec<(DateTime<Utc>, AutomationKey)> = Vec::new();
        for record in &records {
            let schedule = pairs.get(&record.key).map(|queue| &queue.schedule);
            if has_lost_run(record, now) {
                if let Err(error) = self.expire_run_if_lost(&record.key, schedule, now) {
                    tracing::warn!(pair = %record.key, %error, "automations: cannot record a lost run");
                }
            }
            if let Some(since) = claim_candidate(record).filter(|_| schedule.is_some()) {
                candidates.push((since, record.key.clone()));
            }
        }
        // Entry scripts first: a gate waits on them (R8).
        if let Some(claimed) = self.claim_hook_run(&catalog, &pairs, &records, now)? {
            return Ok(Some(claimed));
        }
        candidates.sort();
        for (_, key) in candidates {
            let Some(queue) = pairs.get(&key) else {
                continue;
            };
            let Some(automation) = catalog.automations.get(&key.automation) else {
                continue;
            };
            match self.claim_from(&key, queue, automation, now) {
                Ok(Some(claimed)) => return Ok(Some(claimed)),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(pair = %key, %error, "automations: cannot claim a run");
                }
            }
        }
        Ok(None)
    }

    /// Records the worker's result of a run and applies it to the pair.
    ///
    /// For a run of an `always` queue, exit code 0 with a valid output
    /// document saves the new state, marks the first run done and clears
    /// the failure count; it writes no file. Any other result writes the
    /// run's error file, keeps the state, counts a failure and waits
    /// `interval × 2^failures` (at most 1 h); after
    /// [`MAX_CONSECUTIVE_FAILURES`] the pair goes to `error` (R14, R16).
    ///
    /// For a run for a queue entry, exit code 0 is the only success. Any
    /// other result writes the run's error file, which replaces the older
    /// error of the same entry (R23), and neither counts a failure nor moves
    /// the pair to `error` (R14). The entry stays on its queue either way.
    ///
    /// The run's token stops working. A run already ended, or past its
    /// deadline (it is then recorded as lost), is refused with a conflict.
    pub fn finish_observer_run(
        &self,
        run_id: &RunId,
        report: RunReport,
    ) -> ProtocolResult<LastRun> {
        let finished = self.record_observer_run_result(run_id, report);
        // The pair is free again: its round's next entry, or the task's
        // next entry or exit script, may be due now.
        self.wake_observer_claims();
        finished
    }

    fn record_observer_run_result(
        &self,
        run_id: &RunId,
        report: RunReport,
    ) -> ProtocolResult<LastRun> {
        let key = &run_id.key;
        // Read before the pair's lock: the catalogue is not the pair's.
        let schedule = self.queue_schedule(&key.queue);
        let lock = self.observers.lock(key)?;
        let ended = || {
            ProtocolError::conflict(
                "observer_run_ended",
                format!("run {run_id} has already ended"),
            )
        };
        let mut record = self.observers.load(key)?.ok_or_else(ended)?;
        let Some(active) = record.run_in_progress(run_id.sequence).cloned() else {
            return Err(ended());
        };
        let now = self.control.now();
        if active.is_past_deadline(now) {
            self.lose(&lock, &mut record, &active, schedule.as_ref(), now)?;
            return Err(run_lost(run_id));
        }
        if active.phase != HookPhase::Check {
            return Ok(self.finish_hook_run(&lock, &mut record, &active, &report, now)?);
        }
        // The next tick was set when this run's tick came; one the run
        // outlasted is skipped, not owed (KTD-3, R29).
        skip_passed_tick(&mut record, schedule.as_ref(), now);
        let (status, exit_code, error) = match &active.entry {
            Some(_) => {
                let verdict = judge_entry_run(&report, SCRIPT_TIMEOUT_SECS);
                if verdict.success.is_some() {
                    record.first_run_done = true;
                }
                // A failed entry is checked again in the next round: no
                // backoff, no count towards the error state (R14).
                round_waits_from(&mut record, now);
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
                    None => count_failure(
                        &mut record,
                        schedule.as_ref(),
                        &active.definition_digest,
                        now,
                    ),
                }
                (verdict.status, verdict.exit_code, verdict.error)
            }
        };
        if let Some(message) = &error {
            let failure =
                RunFailure::for_run(key, &active, now, status, exit_code, message.clone())
                    .with_output(bounded(&report.stdout), bounded(&report.stderr));
            self.observers.record_failure(&lock, &failure)?;
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
    /// runs of every pair and report their results, nothing else.
    pub fn authenticate_observer_worker(&self, bearer: &str) -> bool {
        self.observer_worker_key.matches(bearer)
    }

    /// The run a run token proves, `None` for anything that is not the
    /// token of the pair's run in progress before its deadline: a token of
    /// another kind, a spent token (result in, or lost), a wrong secret.
    /// The secret is compared with the digest in the pair's record in
    /// constant time.
    pub fn authenticate_observer_run(&self, bearer: &str) -> Option<RunId> {
        let run_id = RunId::parse(observer_run_token_id(bearer)?).ok()?;
        let record = self.observers.load(&run_id.key).ok()??;
        let active = record.run_in_progress(run_id.sequence)?;
        let valid = token_matches_digest(bearer, &active.token_digest)
            && !active.is_past_deadline(self.control.now());
        valid.then_some(run_id)
    }

    /// The schedule of the catalogue's queue `queue` now, `None` when the
    /// catalogue has no such queue any more.
    fn queue_schedule(&self, queue: &str) -> Option<ScheduleConfig> {
        self.automation_catalog()
            .queues
            .remove(queue)
            .map(|queue| queue.schedule)
    }

    /// Takes the pair's tick (creating its record on first need): the next
    /// tick counts from this one (KTD-3). Flags the scheduled run due,
    /// unless the queue is `on_episodes` and empty once the entries of
    /// ended tasks are gone and missing ones are back (R19, R30). An
    /// `on_episodes` pair's round is the entries left; an `always` pair's
    /// run has no entry (R25). `tidied` holds the entries the pass left of
    /// the queues it tidied ([`Self::tidy_queue_entries`]); a queue it did
    /// not is tidied here.
    fn mark_run_due(
        &self,
        key: &AutomationKey,
        queue: &Queue,
        tidied: &BTreeMap<String, Vec<QueueEntry>>,
        now: DateTime<Utc>,
        pass: &mut ObserverPass,
    ) -> Result<(), ObserverError> {
        let lock = self.observers.lock(key)?;
        let (mut record, created) = match self.observers.load(key)? {
            Some(record) => (record, false),
            None => (AutomationRecord::new(key.clone(), now), true),
        };
        if created {
            pass.created.push(key.clone());
            tracing::info!(pair = %key, "automations: a catalogue pair got its runtime record");
        }
        if !scheduled_run_due(&record, now) {
            if created {
                self.observers.save(&lock, &mut record, now)?;
            }
            return Ok(());
        }
        let tick = record.next_run_at.unwrap_or(now);
        record.next_run_at = Some(queue.schedule.next_tick_after(tick, now));
        if queue.mode == ObserverMode::OnEpisodes {
            let remaining = match tidied.get(&key.queue) {
                Some(entries) => entries.clone(),
                None => self
                    .tidy_queue_entries(&BTreeSet::from([key.queue.clone()]), pass)?
                    .remove(&key.queue)
                    .unwrap_or_default(),
            };
            // The last runs of entries that left the queue are of no use.
            record
                .entry_last_runs
                .retain(|entry_id, _| remaining.iter().any(|entry| &entry.entry_id == entry_id));
            if remaining.is_empty() {
                self.observers.save(&lock, &mut record, now)?;
                pass.skipped.push(key.clone());
                return Ok(());
            }
            record.entry_round = remaining.into_iter().map(|entry| entry.entry_id).collect();
        }
        record.run_due_since = Some(now);
        self.observers.save(&lock, &mut record, now)?;
        pass.due.push(key.clone());
        Ok(())
    }

    /// Skips the tick of a pair whose run is active or whose round goes on
    /// (R29): no run for it, the next tick counts from it.
    fn skip_busy_tick(
        &self,
        key: &AutomationKey,
        schedule: &ScheduleConfig,
        now: DateTime<Utc>,
        pass: &mut ObserverPass,
    ) -> Result<(), ObserverError> {
        let lock = self.observers.lock(key)?;
        let Some(mut record) = self.observers.load(key)? else {
            return Ok(());
        };
        if !tick_while_busy(&record, now) {
            return Ok(());
        }
        skip_passed_tick(&mut record, Some(schedule), now);
        self.observers.save(&lock, &mut record, now)?;
        tracing::debug!(pair = %key, "automations: a tick came while a run was active; skipped");
        pass.busy.push(key.clone());
        Ok(())
    }

    /// Puts a stopped pair (`error`) back on its
    /// schedule, due at once, when its catalogue definition (the queue
    /// file, `automation.yaml`, the `check` script) is no longer the one it
    /// stopped with: the catalogue author changed it.
    fn restart_if_definition_changed(
        &self,
        seen: &AutomationRecord,
        queue: &Queue,
        automation: &Automation,
        now: DateTime<Utc>,
        pass: &mut ObserverPass,
    ) -> Result<(), ObserverError> {
        let check = self
            .launch
            .automation_script(&seen.key.automation, HookPhase::Check)
            .map(|script| script.digest)
            .unwrap_or_default();
        let current = pair_definition_digest(queue, automation, &check);
        if seen.stopped_definition_digest.as_deref() == Some(current.as_str()) {
            return Ok(());
        }
        let lock = self.observers.lock(&seen.key)?;
        let Some(mut record) = self.observers.load(&seen.key)? else {
            return Ok(());
        };
        if !record.state.is_stopped()
            || record.stopped_definition_digest.as_deref() == Some(current.as_str())
        {
            return Ok(());
        }
        record.state = AutomationState::Active;
        record.stopped_definition_digest = None;
        record.consecutive_failures = 0;
        record.next_run_at = Some(now);
        record.run_due_since = None;
        self.observers.save(&lock, &mut record, now)?;
        tracing::info!(pair = %record.key, "automations: the catalogue definition of a stopped pair changed; it runs again");
        pass.restarted.push(record.key);
        Ok(())
    }

    /// Deletes the record of a pair the catalogue no longer has, unless a
    /// run of it is in progress: its result still comes (R2).
    fn remove_record(
        &self,
        key: &AutomationKey,
        pass: &mut ObserverPass,
    ) -> Result<(), ObserverError> {
        let lock = self.observers.lock(key)?;
        if self
            .observers
            .load(key)?
            .is_some_and(|record| record.runs_in_progress().next().is_some())
        {
            return Ok(());
        }
        if self.observers.delete(&lock)? {
            tracing::info!(pair = %key, "automations: the pair is no longer in the catalogue; its runtime record is deleted");
            pass.removed.push(key.clone());
        }
        Ok(())
    }

    /// Records the pair's runs in progress (scheduled and hook) as lost if
    /// they passed their deadline, under the pair's lock. Returns the runs
    /// lost.
    fn expire_run_if_lost(
        &self,
        key: &AutomationKey,
        schedule: Option<&ScheduleConfig>,
        now: DateTime<Utc>,
    ) -> Result<Vec<RunId>, ObserverError> {
        let lock = self.observers.lock(key)?;
        let Some(mut record) = self.observers.load(key)? else {
            return Ok(Vec::new());
        };
        let expired: Vec<ActiveRun> = record
            .runs_in_progress()
            .filter(|active| active.is_past_deadline(now))
            .cloned()
            .collect();
        let mut lost = Vec::new();
        for active in expired {
            self.lose(&lock, &mut record, &active, schedule, now)?;
            let run_id = active.run_id(key);
            tracing::info!(run = %run_id, "automations: run lost by the worker");
            lost.push(run_id);
        }
        Ok(lost)
    }

    /// Ends the run `active` of `record` (its lock held) as lost: a hook
    /// run as a failed attempt ([`Self::lose_hook`]), a scheduled run as
    /// [`Self::lose_run`] says.
    fn lose(
        &self,
        lock: &AutomationLock,
        record: &mut AutomationRecord,
        active: &ActiveRun,
        schedule: Option<&ScheduleConfig>,
        now: DateTime<Utc>,
    ) -> Result<(), ObserverError> {
        match active.phase {
            HookPhase::Check => self.lose_run(lock, record, active, schedule, now),
            HookPhase::In | HookPhase::Out => self.lose_hook(lock, record, active, now),
        }
    }

    /// Ends the run `active` as lost: its error file is written, its token
    /// stops working, the pair's state and failure count stay as they are
    /// (the cause is the infrastructure, not the script), and a scheduled
    /// run is next due at the next tick the run did not outlast.
    fn lose_run(
        &self,
        lock: &AutomationLock,
        record: &mut AutomationRecord,
        active: &ActiveRun,
        schedule: Option<&ScheduleConfig>,
        now: DateTime<Utc>,
    ) -> Result<(), ObserverError> {
        self.observers.record_failure(
            lock,
            &RunFailure::for_run(
                &record.key,
                active,
                now,
                RunStatus::Lost,
                None,
                LOST_MESSAGE,
            ),
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
        skip_passed_tick(record, schedule, now);
        round_waits_from(record, now);
        self.observers.save(lock, record, now)
    }

    /// Takes the run `claim_candidate` names from one pair, under its
    /// lock; `None` when another claim was first, there is nothing to run
    /// after all, or the `check` script could not be read (a failed run).
    fn claim_from(
        &self,
        key: &AutomationKey,
        queue: &Queue,
        automation: &Automation,
        now: DateTime<Utc>,
    ) -> Result<Option<ClaimedObserverRun>, ObserverError> {
        let lock = self.observers.lock(key)?;
        let Some(mut record) = self.observers.load(key)? else {
            return Ok(None);
        };
        if claim_candidate(&record).is_none() {
            return Ok(None);
        }
        let entry = match queue.mode {
            ObserverMode::OnEpisodes => match self.next_round_entry(&mut record)? {
                Some(entry) => Some(entry),
                None => {
                    // The round's entries went between the pass and this
                    // claim (R30); the next round comes at the next tick.
                    record.run_due_since = None;
                    skip_passed_tick(&mut record, Some(&queue.schedule), now);
                    self.observers.save(&lock, &mut record, now)?;
                    return Ok(None);
                }
            },
            ObserverMode::Always => {
                record.entry_round.clear();
                None
            }
        };
        record.last_claim_at = Some(now);
        // Entries left in the round keep the pair due: the next one is
        // claimed once this run has its result.
        record.run_due_since = (!record.entry_round.is_empty()).then_some(now);
        let plan = RunPlan {
            phase: HookPhase::Check,
            entry: entry.as_ref(),
            exit: None,
        };
        match self.start_run(&mut record, queue, automation, plan, now) {
            Ok((active, claimed)) => {
                record.active_run = Some(active);
                self.observers.save(&lock, &mut record, now)?;
                Ok(Some(claimed))
            }
            Err(failure) => {
                self.fail_unreadable_script(&lock, &mut record, queue, automation, failure, now)?;
                Ok(None)
            }
        }
    }

    /// Starts the run `plan` of `record`'s pair (its lock held; the record
    /// is not saved): the next sequence number, the script as the catalogue
    /// holds it now, the run's token, input and deadline. The caller puts
    /// the run in its slot of the record.
    ///
    /// When the script cannot be read, the run's error comes back, not
    /// recorded yet; its sequence number stays taken.
    pub(super) fn start_run(
        &self,
        record: &mut AutomationRecord,
        queue: &Queue,
        automation: &Automation,
        plan: RunPlan<'_>,
        now: DateTime<Utc>,
    ) -> Result<(ActiveRun, ClaimedObserverRun), Box<RunFailure>> {
        let RunPlan { phase, entry, exit } = plan;
        let key = record.key.clone();
        let sequence = record.last_run_sequence + 1;
        record.last_run_sequence = sequence;
        let run_entry = entry.map(RunEntry::from);
        let AutomationScript { text, digest } = match self
            .launch
            .automation_script(&key.automation, phase)
        {
            Ok(script) => script,
            Err(error) => {
                tracing::warn!(pair = %key, %phase, %error, "automations: a run could not start");
                return Err(Box::new(RunFailure::at_claim(
                    &key,
                    phase,
                    sequence,
                    run_entry.as_ref(),
                    now,
                    format!("the {phase} script could not be read at the claim: {error}"),
                )));
            }
        };
        let timeout_secs = match phase {
            HookPhase::Check => SCRIPT_TIMEOUT_SECS,
            HookPhase::In | HookPhase::Out => GATE_HOOK_TIMEOUT_SECS,
        };
        let run_id = RunId::new(key.clone(), sequence);
        let token = BearerToken::issue_observer_run(&run_id.to_string());
        let input = ScriptInput {
            contract_version: SCRIPT_CONTRACT_VERSION,
            phase,
            queue: key.queue.clone(),
            automation: key.automation.clone(),
            condition: queue.condition.clone(),
            state: record.script_state.clone(),
            entry: entry.map(script_entry),
            exit,
            first_run: !record.first_run_done,
        };
        let deadline = after(now, timeout_secs + RUN_DEADLINE_MARGIN_SECS);
        record.last_claim_at = Some(now);
        let active = ActiveRun {
            sequence,
            phase,
            script_digest: digest.clone(),
            definition_digest: pair_definition_digest(queue, automation, &digest),
            claimed_at: now,
            deadline,
            token_digest: token.digest(),
            entry: run_entry,
            exit_generation: None,
        };
        let claimed = ClaimedObserverRun {
            run_id,
            phase,
            token,
            script_digest: digest,
            script: text,
            input,
            secrets: automation.secrets.clone(),
            timeout_secs,
            deadline,
        };
        Ok((active, claimed))
    }

    /// Ends the `check` run that could not start because its script could
    /// not be read at the claim (`failure`): a failed run with the reason,
    /// as if the script had failed (a failure of an `always` pair counts
    /// towards its backoff; one for an entry is recorded for the entry).
    fn fail_unreadable_script(
        &self,
        lock: &AutomationLock,
        record: &mut AutomationRecord,
        queue: &Queue,
        automation: &Automation,
        failure: Box<RunFailure>,
        now: DateTime<Utc>,
    ) -> Result<(), ObserverError> {
        self.observers.record_failure(lock, &failure)?;
        match &failure.entry_id {
            Some(entry_id) => {
                record.entry_last_runs.insert(entry_id.clone(), now);
                round_waits_from(record, now);
            }
            None => count_failure(
                record,
                Some(&queue.schedule),
                &pair_definition_digest(queue, automation, ""),
                now,
            ),
        }
        record.last_run = Some(LastRun {
            sequence: failure.sequence,
            status: RunStatus::Failed,
            finished_at: now,
            error: Some(failure.error),
        });
        self.observers.save(lock, record, now)
    }

    /// Takes the next entry of the round that is still on the pair's queue
    /// and whose task still waits in its episode, `None` when the round has
    /// none left.
    ///
    /// The guard before a run (R18): the task is read again, and an entry
    /// whose gate exited, or whose task ended, was archived or is gone, is
    /// removed and not run. The runner's snapshot removes it first as a
    /// rule (R16); this catches a snapshot the control plane did not see
    /// through. An entry whose task cannot be read is kept, but not run;
    /// so is an entry whose own file cannot be read.
    fn next_round_entry(
        &self,
        record: &mut AutomationRecord,
    ) -> Result<Option<QueueEntry>, ObserverError> {
        while !record.entry_round.is_empty() {
            let entry_id = record.entry_round.remove(0);
            let entry = match self.observers.queue_entry(&entry_id) {
                Ok(Some(entry)) => entry,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(entry = %entry_id, %error, "automations: cannot read a queue entry of the round; it is not run");
                    continue;
                }
            };
            if entry.queue != record.key.queue {
                continue;
            }
            match self.entry_wait(&entry) {
                EntryWait::Waits => return Ok(Some(entry)),
                EntryWait::Gone => {
                    let lock = self.observers.lock_queue_entries()?;
                    if self.observers.remove_queue_entry(&lock, &entry.entry_id)? {
                        tracing::info!(task = %entry.task_id, episode = %entry.episode, "automations: the task no longer waits in the episode of a queue entry; the entry is removed, not run");
                    }
                }
                EntryWait::Unknown => {}
            }
        }
        Ok(None)
    }

    /// Tidies the entries of the queues `queues`: adds back the missing
    /// ones of waiting tasks, removes the ones whose task no longer waits in
    /// their episode, ended, was archived or no longer exists (R18, R19),
    /// and the unreadable entry files of episodes no task waits in. Returns
    /// the entries left, by queue; counts what changed in `pass`.
    ///
    /// The tasks are listed once. An entry whose task, as listed, no longer
    /// waits is checked again with a fresh read before it goes: the list
    /// may be older than the entry.
    fn tidy_queue_entries(
        &self,
        queues: &BTreeSet<String>,
        pass: &mut ObserverPass,
    ) -> Result<BTreeMap<String, Vec<QueueEntry>>, ObserverError> {
        let tasks = self
            .control
            .list_tasks()
            .inspect_err(|error| {
                tracing::warn!(%error, "observers: cannot list the tasks for the queue entries");
            })
            .ok();
        if let Some(tasks) = &tasks {
            pass.entries_restored += self.restore_queue_entries(tasks, queues);
        }
        let scan = self.observers.scan_queue_entries()?;
        if let Some(tasks) = &tasks {
            pass.entries_removed += self.prune_unreadable_entries(tasks, &scan.unreadable);
        }
        let listed: HashMap<&TaskId, &TaskRecord> = tasks
            .iter()
            .flatten()
            .map(|task| (&task.task_id, task))
            .collect();
        let mut left: BTreeMap<String, Vec<QueueEntry>> = queues
            .iter()
            .map(|queue| (queue.clone(), Vec::new()))
            .collect();
        let mut gone = Vec::new();
        for entry in scan.entries {
            let Some(kept) = left.get_mut(&entry.queue) else {
                continue;
            };
            let wait = listed
                .get(&entry.task_id)
                .map(|task| entry_wait_of(task, &entry))
                .filter(|wait| *wait != EntryWait::Gone)
                .unwrap_or_else(|| self.entry_wait(&entry));
            if wait == EntryWait::Gone {
                gone.push(entry);
            } else {
                kept.push(entry);
            }
        }
        if !gone.is_empty() {
            let lock = self.observers.lock_queue_entries()?;
            for entry in &gone {
                if self.observers.remove_queue_entry(&lock, &entry.entry_id)? {
                    pass.entries_removed += 1;
                }
            }
        }
        Ok(left)
    }
}

/// A queue entry as its script sees it (R11).
pub(super) fn script_entry(entry: &QueueEntry) -> ScriptEntry {
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
