//! Entry (`in`) and exit (`out`) scripts of watched gates (R8–R15, R17–R19).
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
//!           a file of the queue or the automation cannot be used now
//!                                       → waits (not run, not skipped) until
//!                                         the file is fixed or removed
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
//! Exit scripts (R10, R11, R15, R18):
//!
//! ```text
//! exit      an answer (person, `task decide`) or an operator transition
//!           or chosen step at a gate whose queue has `out` scripts
//!                                       → held on the gate (`exiting`), not
//!                                         delivered; a second exit of the
//!                                         episode is refused (first wins)
//!           the task ends at the gate   → `out` once, no hold (R18)
//!           the runner leaves with no held exit
//!                                       → `out` after the fact; the gate
//!                                         moves to the task's exiting gates
//! claim     a pending `out` of an exiting episode, due, no other hook run
//!           of the task in progress; `in` of a task's new episode waits
//!           while an episode it left still runs `out` (AE4)
//!           its automation left the queue or lost its `out` script
//!                                       → skipped (history); waits while a
//!                                         file of the pair cannot be used
//! result    as for `in`; after the last attempt the failure is in the
//!           history and the exit goes on
//! episode   no `out` pending any more   → `released`: the held exit is
//!                                         delivered at the runner's next poll;
//!                                         an episode the task left (or of an
//!                                         ended task) loses its queue entry
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

use crate::automations::{Automation, AutomationCatalogLoad, HookPhase, Queue};
use crate::controller_protocol::service::ProtocolService;
use crate::domain::TaskId;
use crate::observers::{
    ActiveRun, AutomationKey, AutomationLock, AutomationRecord, LastRun, ObserverError,
    ObserverMode, QueueEntry, RunFailure, RunStatus, ScriptExit,
};
use crate::orchestration::{
    GateAutomationPhase, GateExit, GateExitAuthor, GateHookOutcome, GateHookStatus,
    GATE_HOOK_TIMEOUT_SECS,
};

use super::entries::{exit_scripts_run, waits_in};
use super::schedule::{
    bounded, judge_entry_run, ClaimedObserverRun, RunPlan, RunReport, LOST_MESSAGE,
};

/// A hook run that may be claimed.
struct HookCandidate {
    /// Since when it is due.
    due: DateTime<Utc>,
    /// Its pair.
    key: AutomationKey,
    /// The queue entry it runs for.
    entry: QueueEntry,
    /// Its script.
    hook: HookPhase,
    /// For `out`, how the gate was left.
    exit: Option<ScriptExit>,
}

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
        candidates.sort_by(|left, right| (left.due, &left.key).cmp(&(right.due, &right.key)));
        for candidate in candidates {
            let key = candidate.key.clone();
            let (Some(queue), Some(automation)) =
                (pairs.get(&key), catalog.automations.get(&key.automation))
            else {
                continue;
            };
            match self.claim_hook_from(queue, automation, candidate, now) {
                Ok(Some(claimed)) => return Ok(Some(claimed)),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(pair = %key, %error, "automations: cannot claim an entry or exit script run");
                }
            }
        }
        Ok(None)
    }

    /// The first due entry or exit script of `entry`'s episode that may
    /// run now; skips (with a history event) the ones the catalogue no
    /// longer has. Exit scripts of an episode the task left come before
    /// any entry script of its next one (AE4).
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
                tracing::debug!(task = %entry.task_id, %error, "automations: cannot read the task of a queue entry for its entry or exit scripts");
                return None;
            }
        };
        let (gate, hook) = if exit_scripts_run(&task, &entry) {
            (task.exiting_gate(&entry.episode)?, HookPhase::Out)
        } else if waits_in(&task, &entry.owner, &entry.episode) && task.exiting_gates.is_empty() {
            (task.watched_gate.as_ref()?, HookPhase::In)
        } else {
            return None;
        };
        let state = gate.automation.as_ref()?;
        if hook == HookPhase::In && state.phase != GateAutomationPhase::Entering {
            return None;
        }
        let exit = (hook == HookPhase::Out).then(|| script_exit(state.exit.as_ref()));
        let since = state.exit.as_ref().map(|exit| exit.at).or(gate.began_at);
        for script in state
            .hooks
            .iter()
            .filter(|script| script.hook == hook && script.status == GateHookStatus::Pending)
        {
            let Ok(key) = AutomationKey::new(entry.queue.clone(), script.automation.clone()) else {
                continue;
            };
            // A file of the pair cannot be used now (an editor may save it
            // half-written): the script waits for the file, neither run nor
            // skipped.
            if catalog.pair_has_problem(&key.queue, &key.automation) {
                continue;
            }
            if let Some(reason) = no_hook_script(catalog, pairs, &key, hook) {
                self.settle_hook(
                    &entry.task_id,
                    &entry.episode,
                    &key.automation,
                    hook,
                    &GateHookOutcome::Skipped(reason),
                );
                continue;
            }
            let due = script.next_attempt_at().or(since).unwrap_or(now);
            if due > now || busy_pairs.contains(&key) {
                continue;
            }
            return Some(HookCandidate {
                due,
                key,
                entry,
                hook,
                exit,
            });
        }
        None
    }

    /// Starts the `in` or `out` run of `entry` on the pair `key`, under the
    /// pair's lock; `None` when the pair has a hook run in progress, or the
    /// script could not be read (a failed attempt).
    ///
    /// A pair stopped in `error` or `needs_attention` still runs its hooks,
    /// on purpose: those states track the health of its `check` runs only,
    /// and holding hooks back would leave the gate stuck in `entering` or
    /// `exiting`.
    fn claim_hook_from(
        &self,
        queue: &Queue,
        automation: &Automation,
        candidate: HookCandidate,
        now: DateTime<Utc>,
    ) -> Result<Option<ClaimedObserverRun>, ObserverError> {
        let HookCandidate {
            key,
            entry,
            hook,
            exit,
            ..
        } = candidate;
        let lock = self.observers.lock(&key)?;
        let mut record = self.observers.load_or_new(&key, now)?;
        if record.active_hook.is_some() {
            return Ok(None);
        }
        let plan = RunPlan {
            phase: hook,
            entry: Some(&entry),
            exit,
        };
        match self.start_run(&mut record, queue, automation, plan, now) {
            Ok((active, claimed)) => {
                record.active_hook = Some(active);
                self.observers.save(&lock, &mut record, now)?;
                tracing::info!(run = %claimed.run_id, task = %entry.task_id, episode = %entry.episode, %hook, "automations: an entry or exit script run starts");
                Ok(Some(claimed))
            }
            Err(failure) => {
                self.observers.record_failure(&lock, &failure)?;
                self.observers.save(&lock, &mut record, now)?;
                self.settle_hook(
                    &entry.task_id,
                    &entry.episode,
                    &key.automation,
                    hook,
                    &GateHookOutcome::Failed(failure.error),
                );
                Ok(None)
            }
        }
    }

    /// Applies the worker's result of the hook run `active` of `record`
    /// (its lock held): exit 0 succeeds, anything else is a failed attempt
    /// with an error file.
    pub(super) fn finish_hook_run(
        &self,
        lock: &AutomationLock,
        record: &mut AutomationRecord,
        active: &ActiveRun,
        report: &RunReport,
        now: DateTime<Utc>,
    ) -> Result<LastRun, ObserverError> {
        let verdict = judge_entry_run(report, GATE_HOOK_TIMEOUT_SECS);
        let failure = verdict.error.as_ref().map(|message| {
            RunFailure::for_run(
                &record.key,
                active,
                now,
                verdict.status,
                verdict.exit_code,
                message.clone(),
            )
            .with_output(bounded(&report.stdout), bounded(&report.stderr))
        });
        self.end_hook(lock, record, active, failure, now)?;
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
        let failure = RunFailure::for_run(
            &record.key,
            active,
            now,
            RunStatus::Lost,
            None,
            LOST_MESSAGE,
        );
        self.end_hook(lock, record, active, Some(failure), now)
    }

    /// Ends the hook run `active` of `record` (its lock held): its error
    /// file when it failed, the slot cleared and the pair's record saved
    /// first, then the attempt (a success with no `failure`) goes to the
    /// task.
    fn end_hook(
        &self,
        lock: &AutomationLock,
        record: &mut AutomationRecord,
        active: &ActiveRun,
        failure: Option<RunFailure>,
        now: DateTime<Utc>,
    ) -> Result<(), ObserverError> {
        if let (Some(failure), Some(_)) = (&failure, &active.entry) {
            self.observers.record_failure(lock, failure)?;
        }
        record.active_hook = None;
        self.observers.save(lock, record, now)?;
        if let Some(entry) = &active.entry {
            let outcome = match failure {
                None => GateHookOutcome::Succeeded,
                Some(failure) => GateHookOutcome::Failed(failure.error),
            };
            self.settle_hook(
                &entry.task_id,
                &entry.episode,
                &record.key.automation,
                active.phase,
                &outcome,
            );
        }
        Ok(())
    }

    /// Removes the queue entry of `episode` once its exit scripts ended,
    /// when the task no longer waits there: it left the gate, or ended at
    /// it. A gate it still waits at keeps its entry until the snapshot
    /// shows it left (the held exit is on its way).
    fn drop_left_entry(&self, task_id: &TaskId, episode: &str) {
        let task = self.control.task(task_id).ok();
        if task
            .as_ref()
            .is_some_and(|task| waits_in(task, &task.owner, episode))
        {
            return;
        }
        self.remove_episode_entry(
            task_id,
            task.as_ref(),
            episode,
            "the exit scripts of a left episode ended",
        );
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
                    tracing::info!(task = %task_id, %episode, phase = phase.as_str(), "automations: the entry or exit scripts of a gate ended");
                }
                if phase == Some(GateAutomationPhase::Released) {
                    self.drop_left_entry(task_id, episode);
                }
                // Also wakes the runner's long-poll: a released exit is
                // delivered at once.
                self.notify(task_id);
                // The task's next entry or exit script may be due now.
                self.wake_observer_claims();
            }
            Err(error) => {
                tracing::warn!(task = %task_id, %episode, %automation, %error, "automations: cannot record an attempt of an entry or exit script");
            }
        }
    }
}

/// How the gate was left, as an `out` script reads it (R11); a gate of a
/// task that ended there has no exit recorded yet: `task_end`.
fn script_exit(exit: Option<&GateExit>) -> ScriptExit {
    match exit {
        Some(exit) => ScriptExit {
            choice: exit.choice.clone(),
            author: exit.author.as_str().to_string(),
            run_id: exit.run_id.clone(),
        },
        None => ScriptExit {
            choice: None,
            author: GateExitAuthor::TaskEnd.as_str().to_string(),
            run_id: None,
        },
    }
}

/// Why the pair `key` runs no `hook` script for its queue now, `None` when
/// it does: the queue (an `on_episodes` one) still names the automation and
/// the automation still has the script. Asked only when no file of the
/// pair has a problem: a pair absent from the catalogue is then removed for
/// good.
fn no_hook_script(
    catalog: &AutomationCatalogLoad,
    pairs: &BTreeMap<AutomationKey, &Queue>,
    key: &AutomationKey,
    hook: HookPhase,
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
        .is_some_and(|automation| automation.has(hook))
    {
        return Some(format!(
            "automation '{}' no longer has an {hook} script",
            key.automation
        ));
    }
    None
}
