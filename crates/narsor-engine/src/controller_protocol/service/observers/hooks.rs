//! Entry scripts (`in`) of watched gates (R8, R9, R12, R13, R17, R19).
//!
//! The task's watched gate holds where the episode stands
//! ([`crate::orchestration::GateAutomation`]): the snapshot that begins the
//! episode writes `entering` with one pending `in` per automation of the
//! queue that has one, or `ready` at once when none has (R17). The queue
//! entry stays the index the claim walks; the task record is the truth.
//!
//! ```text
//! claim     a pending `in` of an `entering` episode whose task still
//!           waits in it, due (no attempt yet, or its retry time came),
//!           no other hook run of the task in progress, none of the pair
//!           → the pair's hook run (`active_hook`), before any `check`
//!             run; the input is the `check` input of the entry with
//!             phase `in`, the time limit GATE_HOOK_TIMEOUT_SECS
//!           its automation left the queue, lost its `in` script, or the
//!           queue left the catalogue → skipped (history), not run
//!           its `in` script cannot be read → a failed attempt
//! result    exit 0                      → succeeded
//!           anything else               → failed attempt: error file,
//!                                         retry after 10 s, then 30 s;
//!                                         after the 3rd: failed for good
//! lost      no result by the deadline   → a failed attempt (as above)
//! episode   no `in` pending any more    → `ready` (every one succeeded or
//!                                         was skipped) or `entry_failed`
//!                                         (one failed for good): no more
//!                                         automatic attempts
//! ```
//!
//! Each end of an attempt is one task event ([`TaskEventKind::GateHookAttempt`]),
//! so the attempts show in the gate step's history, never as steps. A hook
//! run never touches the pair's `check` schedule: not its round, its
//! backoff, its failure count or its state.
//!
//! The run in progress lives in the pair's record only; the task changes
//! when the attempt ends, under the pair's lock, after the pair's record is
//! saved. A stop between the two loses the attempt's result: the script
//! stays pending and runs again (R13: entry scripts are idempotent).
//!
//! [`TaskEventKind::GateHookAttempt`]: crate::orchestration::TaskEventKind::GateHookAttempt

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};

use crate::automations::{
    pair_definition_digest, Automation, AutomationCatalogLoad, AutomationScript, HookPhase, Queue,
};
use crate::controller_protocol::credentials::BearerToken;
use crate::controller_protocol::service::ProtocolService;
use crate::domain::TaskId;
use crate::observers::{
    ActiveRun, AutomationKey, AutomationLock, AutomationRecord, LastRun, ObserverError,
    ObserverMode, QueueEntry, RunEntry, RunFailure, RunId, RunStatus, ScriptInput,
    RUN_FAILURE_VERSION, SCRIPT_CONTRACT_VERSION,
};
use crate::orchestration::{
    GateAutomationPhase, GateHookOutcome, GateHookStatus, GATE_HOOK_TIMEOUT_SECS,
};

use super::entries::waits_in;
use super::schedule::{
    after, bounded, judge_entry_run, script_entry, ClaimedObserverRun, RunReport, LOST_MESSAGE,
    RUN_DEADLINE_MARGIN_SECS,
};

/// A hook run that may be claimed: since when it is due, its pair and the
/// entry it runs for.
type HookCandidate = (DateTime<Utc>, AutomationKey, QueueEntry);

impl ProtocolService {
    /// Claims the entry script run due the longest, `None` when none is
    /// due. `records` are the pairs' records as the claim listed them: their
    /// hook runs in progress keep their task and their pair busy.
    pub(super) fn claim_hook_run(
        &self,
        catalog: &AutomationCatalogLoad,
        pairs: &BTreeMap<AutomationKey, &Queue>,
        records: &[AutomationRecord],
        now: DateTime<Utc>,
    ) -> Result<Option<ClaimedObserverRun>, ObserverError> {
        let live_hooks: Vec<(&AutomationKey, &ActiveRun)> = records
            .iter()
            .filter_map(|record| {
                record
                    .active_hook
                    .as_ref()
                    .filter(|active| !active.is_past_deadline(now))
                    .map(|active| (&record.key, active))
            })
            .collect();
        let busy_pairs: HashSet<&AutomationKey> = live_hooks.iter().map(|(key, _)| *key).collect();
        let busy_tasks: HashSet<&TaskId> = live_hooks
            .iter()
            .filter_map(|(_, active)| active.entry.as_ref())
            .map(|entry| &entry.task_id)
            .collect();
        let mut candidates = Vec::new();
        for entry in self.observers.scan_queue_entries()?.entries {
            // Hook runs of one task run one at a time.
            if busy_tasks.contains(&entry.task_id) {
                continue;
            }
            if let Some(candidate) = self.hook_candidate(catalog, pairs, &busy_pairs, entry, now) {
                candidates.push(candidate);
            }
        }
        candidates.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
        for (_, key, entry) in candidates {
            let (Some(queue), Some(automation)) =
                (pairs.get(&key), catalog.automations.get(&key.automation))
            else {
                continue;
            };
            match self.claim_hook_from(&key, queue, automation, &entry, now) {
                Ok(Some(claimed)) => return Ok(Some(claimed)),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(pair = %key, %error, "automations: cannot claim an entry script run");
                }
            }
        }
        Ok(None)
    }

    /// The first due entry script of `entry`'s episode that may run now;
    /// skips (with a history event) the ones the catalogue no longer has.
    fn hook_candidate(
        &self,
        catalog: &AutomationCatalogLoad,
        pairs: &BTreeMap<AutomationKey, &Queue>,
        busy_pairs: &HashSet<&AutomationKey>,
        entry: QueueEntry,
        now: DateTime<Utc>,
    ) -> Option<HookCandidate> {
        let task = match self.control.task(&entry.task_id) {
            Ok(task) => task,
            Err(error) => {
                tracing::debug!(task = %entry.task_id, %error, "automations: cannot read the task of a queue entry for its entry scripts");
                return None;
            }
        };
        if !waits_in(&task, &entry.owner, &entry.episode) {
            return None;
        }
        let gate = task.watched_gate.as_ref()?;
        let state = gate
            .automation
            .as_ref()
            .filter(|state| state.phase == GateAutomationPhase::Entering)?;
        for script in state.hooks.iter().filter(|script| {
            script.hook == HookPhase::In && script.status == GateHookStatus::Pending
        }) {
            let Ok(key) = AutomationKey::new(entry.queue.clone(), script.automation.clone()) else {
                continue;
            };
            if let Some(reason) = no_entry_script(catalog, pairs, &key) {
                self.settle_hook(
                    &entry.task_id,
                    &entry.episode,
                    &key.automation,
                    HookPhase::In,
                    &GateHookOutcome::Skipped(reason),
                );
                continue;
            }
            let due = script.next_attempt_at.or(gate.began_at).unwrap_or(now);
            if due > now || busy_pairs.contains(&key) {
                continue;
            }
            return Some((due, key, entry));
        }
        None
    }

    /// Starts the `in` run of `entry` on the pair `key`, under the pair's
    /// lock; `None` when the pair has a hook run in progress, or the script
    /// could not be read (a failed attempt).
    fn claim_hook_from(
        &self,
        key: &AutomationKey,
        queue: &Queue,
        automation: &Automation,
        entry: &QueueEntry,
        now: DateTime<Utc>,
    ) -> Result<Option<ClaimedObserverRun>, ObserverError> {
        let lock = self.observers.lock(key)?;
        let mut record = self
            .observers
            .load(key)?
            .unwrap_or_else(|| AutomationRecord::new(key.clone(), now));
        if record.active_hook.is_some() {
            return Ok(None);
        }
        let sequence = record.last_run_sequence + 1;
        record.last_run_sequence = sequence;
        let run_entry = RunEntry {
            entry_id: entry.entry_id.clone(),
            task_id: entry.task_id.clone(),
            episode: entry.episode.clone(),
        };
        let AutomationScript { text, digest } = match self
            .launch
            .automation_script(&key.automation, HookPhase::In)
        {
            Ok(script) => script,
            Err(error) => {
                let message = format!("the in script could not be read at the claim: {error}");
                tracing::warn!(pair = %key, %error, "automations: an entry script run could not start");
                self.observers.record_failure(
                    &lock,
                    &hook_failure(
                        key,
                        sequence,
                        "",
                        &run_entry,
                        now,
                        now,
                        RunStatus::Failed,
                        None,
                        &message,
                    ),
                )?;
                self.observers.save(&lock, &mut record, now)?;
                self.settle_hook(
                    &entry.task_id,
                    &entry.episode,
                    &key.automation,
                    HookPhase::In,
                    &GateHookOutcome::Failed(message),
                );
                return Ok(None);
            }
        };
        let run_id = RunId::new(key.clone(), sequence);
        let token = BearerToken::issue_observer_run(&run_id.to_string());
        let input = ScriptInput {
            contract_version: SCRIPT_CONTRACT_VERSION,
            phase: HookPhase::In,
            queue: key.queue.clone(),
            automation: key.automation.clone(),
            condition: queue.condition.clone(),
            state: record.script_state.clone(),
            entry: Some(script_entry(entry)),
            exit: None,
            first_run: !record.first_run_done,
            import_backlog: false,
        };
        let deadline = after(now, GATE_HOOK_TIMEOUT_SECS + RUN_DEADLINE_MARGIN_SECS);
        record.last_claim_at = Some(now);
        record.active_hook = Some(ActiveRun {
            sequence,
            phase: HookPhase::In,
            script_digest: digest.clone(),
            definition_digest: pair_definition_digest(queue, automation, &digest),
            claimed_at: now,
            deadline,
            token_digest: token.digest(),
            tasks_created: 0,
            entry: Some(run_entry),
        });
        self.observers.save(&lock, &mut record, now)?;
        tracing::info!(run = %run_id, task = %entry.task_id, episode = %entry.episode, "automations: an entry script run starts");
        Ok(Some(ClaimedObserverRun {
            run_id,
            phase: HookPhase::In,
            token,
            script_digest: digest,
            script: text,
            input,
            secrets: automation.secrets.clone(),
            timeout_secs: GATE_HOOK_TIMEOUT_SECS,
            deadline,
        }))
    }

    /// Applies the worker's result of the hook run `active` of `record`
    /// (its lock held): exit 0 succeeds, anything else is a failed attempt
    /// with an error file. The pair's record is saved first, then the
    /// attempt goes to the task.
    pub(super) fn finish_hook_run(
        &self,
        lock: &AutomationLock,
        record: &mut AutomationRecord,
        active: &ActiveRun,
        report: &RunReport,
        now: DateTime<Utc>,
    ) -> Result<LastRun, ObserverError> {
        let verdict = judge_entry_run(report, GATE_HOOK_TIMEOUT_SECS);
        if let (Some(message), Some(entry)) = (&verdict.error, &active.entry) {
            let mut failure = hook_failure(
                &record.key,
                active.sequence,
                &active.script_digest,
                entry,
                active.claimed_at,
                now,
                verdict.status,
                verdict.exit_code,
                message,
            );
            failure.phase = active.phase;
            failure.stdout = Some(bounded(&report.stdout));
            failure.stderr = Some(bounded(&report.stderr));
            self.observers.record_failure(lock, &failure)?;
        }
        record.active_hook = None;
        self.observers.save(lock, record, now)?;
        if let Some(entry) = &active.entry {
            let outcome = match &verdict.error {
                None => GateHookOutcome::Succeeded,
                Some(message) => GateHookOutcome::Failed(message.clone()),
            };
            self.settle_hook(
                &entry.task_id,
                &entry.episode,
                &record.key.automation,
                active.phase,
                &outcome,
            );
        }
        Ok(LastRun {
            sequence: active.sequence,
            status: verdict.status,
            finished_at: now,
            error: verdict.error,
        })
    }

    /// Ends the hook run `active` of `record` (its lock held) as lost: an
    /// error file, the token spent, and a failed attempt for the task.
    pub(super) fn lose_hook(
        &self,
        lock: &AutomationLock,
        record: &mut AutomationRecord,
        active: &ActiveRun,
        now: DateTime<Utc>,
    ) -> Result<(), ObserverError> {
        if let Some(entry) = &active.entry {
            let mut failure = hook_failure(
                &record.key,
                active.sequence,
                &active.script_digest,
                entry,
                active.claimed_at,
                now,
                RunStatus::Lost,
                None,
                LOST_MESSAGE,
            );
            failure.phase = active.phase;
            self.observers.record_failure(lock, &failure)?;
        }
        record.active_hook = None;
        self.observers.save(lock, record, now)?;
        if let Some(entry) = &active.entry {
            self.settle_hook(
                &entry.task_id,
                &entry.episode,
                &record.key.automation,
                active.phase,
                &GateHookOutcome::Failed(LOST_MESSAGE.to_string()),
            );
        }
        Ok(())
    }

    /// Writes the end of one attempt to the task (its history, the
    /// script's status, the gate's phase). A failure is logged: the attempt
    /// is lost, the script stays pending and runs again.
    fn settle_hook(
        &self,
        task_id: &TaskId,
        episode: &str,
        automation: &str,
        hook: HookPhase,
        outcome: &GateHookOutcome,
    ) {
        match self
            .control
            .record_gate_hook_attempt(task_id, episode, automation, hook, outcome)
        {
            Ok(phase) => {
                if let Some(phase) = phase {
                    tracing::info!(task = %task_id, %episode, phase = phase.as_str(), "automations: the entry scripts of a gate ended");
                }
                self.notify(task_id);
            }
            Err(error) => {
                tracing::warn!(task = %task_id, %episode, %automation, %error, "automations: cannot record an attempt of an entry script");
            }
        }
    }
}

/// Why the pair `key` runs no entry script for its queue now, `None` when
/// it does: the queue (an `on_episodes` one) still names the automation and
/// the automation still has an `in` script.
fn no_entry_script(
    catalog: &AutomationCatalogLoad,
    pairs: &BTreeMap<AutomationKey, &Queue>,
    key: &AutomationKey,
) -> Option<String> {
    let Some(queue) = pairs.get(key) else {
        return Some(format!(
            "queue '{}' no longer runs automation '{}'",
            key.queue, key.automation
        ));
    };
    if queue.mode != ObserverMode::OnEpisodes {
        return Some(format!("queue '{}' no longer serves gates", key.queue));
    }
    if !catalog
        .automations
        .get(&key.automation)
        .is_some_and(|automation| automation.has(HookPhase::In))
    {
        return Some(format!(
            "automation '{}' no longer has an in script",
            key.automation
        ));
    }
    None
}

/// The error file of a hook run for `entry`, with no output.
#[allow(clippy::too_many_arguments)]
fn hook_failure(
    key: &AutomationKey,
    sequence: u64,
    script_digest: &str,
    entry: &RunEntry,
    claimed_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    status: RunStatus,
    exit_code: Option<i32>,
    message: &str,
) -> RunFailure {
    RunFailure {
        record_version: RUN_FAILURE_VERSION,
        key: key.clone(),
        sequence,
        phase: HookPhase::In,
        script_digest: script_digest.to_string(),
        claimed_at,
        finished_at,
        status,
        exit_code,
        error: message.to_string(),
        stdout: None,
        stderr: None,
        entry_id: Some(entry.entry_id.clone()),
    }
}
