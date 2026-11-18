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
//!                                       → skipped (history)
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

use crate::automations::{
    pair_definition_digest, Automation, AutomationCatalogLoad, AutomationScript, HookPhase, Queue,
};
use crate::controller_protocol::credentials::BearerToken;
use crate::controller_protocol::service::ProtocolService;
use crate::domain::TaskId;
use crate::observers::{
    queue_entry_id, ActiveRun, AutomationKey, AutomationLock, AutomationRecord, LastRun,
    ObserverError, ObserverMode, QueueEntry, RunEntry, RunFailure, RunId, RunStatus, ScriptExit,
    ScriptInput, RUN_FAILURE_VERSION, SCRIPT_CONTRACT_VERSION,
};
use crate::orchestration::{
    GateAutomationPhase, GateExit, GateExitAuthor, GateHookOutcome, GateHookStatus,
    GATE_HOOK_TIMEOUT_SECS,
};

use super::entries::{exit_scripts_run, waits_in};
use super::schedule::{
    after, bounded, judge_entry_run, script_entry, ClaimedObserverRun, RunReport, LOST_MESSAGE,
    RUN_DEADLINE_MARGIN_SECS,
};

/// A hook run that may be claimed: since when it is due, its pair, the
/// entry it runs for, its script and, for `out`, how the gate was left.
type HookCandidate = (
    DateTime<Utc>,
    AutomationKey,
    QueueEntry,
    HookPhase,
    Option<ScriptExit>,
);

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
        for (_, key, entry, hook, exit) in candidates {
            let (Some(queue), Some(automation)) =
                (pairs.get(&key), catalog.automations.get(&key.automation))
            else {
                continue;
            };
            let script = HookScript { hook, exit };
            match self.claim_hook_from(&key, queue, automation, &entry, script, now) {
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
            let due = script.next_attempt_at.or(since).unwrap_or(now);
            if due > now || busy_pairs.contains(&key) {
                continue;
            }
            return Some((due, key, entry, hook, exit));
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
        key: &AutomationKey,
        queue: &Queue,
        automation: &Automation,
        entry: &QueueEntry,
        script: HookScript,
        now: DateTime<Utc>,
    ) -> Result<Option<ClaimedObserverRun>, ObserverError> {
        let HookScript { hook, exit } = script;
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
            .automation_script(&key.automation, hook)
        {
            Ok(script) => script,
            Err(error) => {
                let message = format!("the {hook} script could not be read at the claim: {error}");
                tracing::warn!(pair = %key, %error, "automations: an entry or exit script run could not start");
                self.observers.record_failure(
                    &lock,
                    &hook_failure(
                        key,
                        hook,
                        sequence,
                        "",
                        &run_entry,
                        (now, now),
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
                    hook,
                    &GateHookOutcome::Failed(message),
                );
                return Ok(None);
            }
        };
        let run_id = RunId::new(key.clone(), sequence);
        let token = BearerToken::issue_observer_run(&run_id.to_string());
        let input = ScriptInput {
            contract_version: SCRIPT_CONTRACT_VERSION,
            phase: hook,
            queue: key.queue.clone(),
            automation: key.automation.clone(),
            condition: queue.condition.clone(),
            state: record.script_state.clone(),
            entry: Some(script_entry(entry)),
            exit,
            first_run: !record.first_run_done,
            import_backlog: false,
        };
        let deadline = after(now, GATE_HOOK_TIMEOUT_SECS + RUN_DEADLINE_MARGIN_SECS);
        record.last_claim_at = Some(now);
        record.active_hook = Some(ActiveRun {
            sequence,
            phase: hook,
            script_digest: digest.clone(),
            definition_digest: pair_definition_digest(queue, automation, &digest),
            claimed_at: now,
            deadline,
            token_digest: token.digest(),
            tasks_created: 0,
            entry: Some(run_entry),
        });
        self.observers.save(&lock, &mut record, now)?;
        tracing::info!(run = %run_id, task = %entry.task_id, episode = %entry.episode, %hook, "automations: an entry or exit script run starts");
        Ok(Some(ClaimedObserverRun {
            run_id,
            phase: hook,
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
                active.phase,
                active.sequence,
                &active.script_digest,
                entry,
                (active.claimed_at, now),
                verdict.status,
                verdict.exit_code,
                message,
            );
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
            let failure = hook_failure(
                &record.key,
                active.phase,
                active.sequence,
                &active.script_digest,
                entry,
                (active.claimed_at, now),
                RunStatus::Lost,
                None,
                LOST_MESSAGE,
            );
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

    /// Removes the queue entry of `episode` once its exit scripts ended,
    /// when the task no longer waits there: it left the gate, or ended at
    /// it. A gate it still waits at keeps its entry until the snapshot
    /// shows it left (the held exit is on its way).
    fn drop_left_entry(&self, task_id: &TaskId, episode: &str) {
        let waits = match self.control.task(task_id) {
            Ok(task) => waits_in(&task, &task.owner, episode),
            Err(_) => false,
        };
        if waits {
            return;
        }
        let removed = self.observers.lock_queue_entries().and_then(|lock| {
            self.observers
                .remove_queue_entry(&lock, &queue_entry_id(task_id, episode))
        });
        match removed {
            Ok(true) => {
                tracing::info!(task = %task_id, %episode, "automations: the exit scripts of a left episode ended; its queue entry is removed");
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(task = %task_id, %episode, %error, "automations: cannot remove the queue entry of a left episode");
            }
        }
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
            }
            Err(error) => {
                tracing::warn!(task = %task_id, %episode, %automation, %error, "automations: cannot record an attempt of an entry or exit script");
            }
        }
    }
}

/// Which hook script a claim starts, and for `out` how the gate was left.
struct HookScript {
    hook: HookPhase,
    exit: Option<ScriptExit>,
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
/// the automation still has the script.
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

/// The error file of the `hook` run for `entry`, with no output; `times`
/// are when it was claimed and when it ended.
#[allow(clippy::too_many_arguments)]
fn hook_failure(
    key: &AutomationKey,
    hook: HookPhase,
    sequence: u64,
    script_digest: &str,
    entry: &RunEntry,
    (claimed_at, finished_at): (DateTime<Utc>, DateTime<Utc>),
    status: RunStatus,
    exit_code: Option<i32>,
    message: &str,
) -> RunFailure {
    RunFailure {
        record_version: RUN_FAILURE_VERSION,
        key: key.clone(),
        sequence,
        phase: hook,
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
