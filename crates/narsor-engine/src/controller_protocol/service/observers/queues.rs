//! What the owner reads of the queues through `narsor-cli queue`, the TUI
//! and the WebUI (R21): read only. The queues and automations come from the
//! catalogue, read now; the entries, runs and errors from the state.
//!
//! ```text
//! queue list   every queue of the catalogue, every queue file that cannot
//!              be used (with its problem), every queue name only the
//!              owner's waiting gates name ("not in the catalogue"), with
//!              counts of the owner's entries; and every catalogue file of
//!              automations and queues that failed to load
//! queue show   the queue's automations (scripts, state, last run, backoff,
//!              recent errors), its entries with per automation the last
//!              run time and the newest error (R23), and the problems of
//!              its file
//! ```
//!
//! No route, command or view changes an automation or a queue: the catalogue
//! author edits the files (R2, R21).

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::automations::{Automation, AutomationCatalogLoad, CatalogProblem, HookPhase, Queue};
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::ProtocolService;
use crate::controller_protocol::wire::*;
use crate::observers::{AutomationKey, AutomationRecord, AutomationState, QueueEntry, RunFailure};
use crate::orchestration::OwnerRef;

use super::entries::problem_names_queue;

/// Why a queue name that only waiting gates name does not run.
const NOT_IN_CATALOGUE: &str = "the catalogue has no such queue; its gates wait for a person";

/// What the views know of one automation of a queue: the catalogue's
/// definition (absent when it went meanwhile), the pair's runtime record
/// (absent before its first pass) and its kept errors.
struct PairState<'a> {
    name: &'a str,
    automation: Option<&'a Automation>,
    record: Option<AutomationRecord>,
    errors: Vec<RunFailure>,
}

impl ProtocolService {
    /// The queues `owner` can see, by name, and the catalogue files that
    /// failed to load.
    pub fn observer_queues_of(
        &self,
        owner: &OwnerRef,
    ) -> ProtocolResult<ObserverQueueListResponse> {
        let catalog = self.automation_catalog();
        let entries = self.owned_entries(owner)?;
        let names: BTreeSet<String> = catalog
            .queues
            .keys()
            .cloned()
            .chain(problem_queues(&catalog))
            .chain(entries.iter().map(|entry| entry.queue.clone()))
            .collect();
        let queues = names
            .into_iter()
            .map(|name| {
                let queue = catalog.queues.get(&name);
                let (interval_secs, cron) = schedule_fields(queue);
                ObserverQueueSummary {
                    entries: count(entries.iter().filter(|entry| entry.queue == name).count()),
                    mode: queue.map(|queue| queue.mode.to_string()),
                    interval_secs,
                    cron,
                    automations: queue
                        .map(|queue| queue.automations.clone())
                        .unwrap_or_default(),
                    problem: match queue {
                        Some(_) => None,
                        None => Some(queue_problems(&catalog, &name).first().map_or_else(
                            || NOT_IN_CATALOGUE.to_string(),
                            |problem| problem.reason.clone(),
                        )),
                    },
                    queue: name,
                }
            })
            .collect();
        Ok(ObserverQueueListResponse {
            queues,
            problems: catalog.problems.iter().map(problem_view).collect(),
        })
    }

    /// One queue: its automations with their runtime state, `owner`'s
    /// entries with per automation the last run time and the newest error
    /// (R23), and the problems of its file. Not found when the catalogue
    /// has neither the queue nor a problem of its file, and no gate of
    /// `owner` names it.
    pub fn observer_queue(
        &self,
        owner: &OwnerRef,
        queue: &str,
    ) -> ProtocolResult<ObserverQueueView> {
        let catalog = self.automation_catalog();
        let found = catalog.queues.get(queue);
        let mut problems: Vec<CatalogProblemView> = queue_problems(&catalog, queue)
            .into_iter()
            .map(problem_view)
            .collect();
        let entries = self.observers.queue_entries(owner, queue)?;
        if found.is_none() && problems.is_empty() {
            if entries.is_empty() {
                return Err(ProtocolError::NotFound);
            }
            problems.push(CatalogProblemView {
                file: format!("queues/{queue}.yaml"),
                reason: NOT_IN_CATALOGUE.to_string(),
            });
        }
        let pairs = match found {
            Some(found) => self.pair_states(&catalog, queue, found)?,
            None => Vec::new(),
        };
        let now = self.control.now();
        let (interval_secs, cron) = schedule_fields(found);
        Ok(ObserverQueueView {
            queue: queue.to_string(),
            mode: found.map(|found| found.mode.to_string()),
            interval_secs,
            cron,
            workflow_id: found
                .and_then(|found| found.tasks.as_ref())
                .map(|tasks| tasks.effect.workflow_id.clone()),
            automations: pairs
                .iter()
                .map(|pair| automation_view(pair, found, now))
                .collect(),
            entries: entries
                .iter()
                .map(|entry| {
                    let mut view = entry_view(entry, &pairs);
                    view.gate_phase = self.entry_gate_phase(entry);
                    view
                })
                .collect(),
            problems,
        })
    }

    /// The automation phase of the episode of `entry`, as its task holds
    /// it; `None` when the task cannot be read, waits elsewhere or keeps no
    /// phase. One task read per entry.
    fn entry_gate_phase(&self, entry: &QueueEntry) -> Option<String> {
        let task = self.control.task(&entry.task_id).ok()?;
        task.watched_gate
            .filter(|gate| gate.episode == entry.episode)?
            .automation
            .map(|automation| automation.phase.as_str().to_string())
    }

    /// The definition, record and errors of every automation of `queue`.
    fn pair_states<'a>(
        &self,
        catalog: &'a AutomationCatalogLoad,
        name: &str,
        queue: &'a Queue,
    ) -> ProtocolResult<Vec<PairState<'a>>> {
        let mut pairs = Vec::new();
        for automation in &queue.automations {
            let (record, errors) = match AutomationKey::new(name, automation.as_str()) {
                Ok(key) => (
                    self.observers.load(&key)?,
                    self.observers.list_failures(&key)?,
                ),
                Err(_) => (None, Vec::new()),
            };
            pairs.push(PairState {
                name: automation,
                automation: catalog.automations.get(automation),
                record,
                errors,
            });
        }
        Ok(pairs)
    }

    /// The queue entries of `owner`'s gates, oldest first.
    fn owned_entries(&self, owner: &OwnerRef) -> ProtocolResult<Vec<QueueEntry>> {
        Ok(self
            .observers
            .list_queue_entries()?
            .into_iter()
            .filter(|entry| &entry.owner == owner)
            .collect())
    }
}

/// The names of the queue files that failed to load.
fn problem_queues(catalog: &AutomationCatalogLoad) -> impl Iterator<Item = String> + '_ {
    catalog.problems.iter().filter_map(|problem| {
        let stem = problem.file.file_stem()?.to_str()?;
        problem_names_queue(&problem.file, stem).then(|| stem.to_string())
    })
}

/// The problems of the file of queue `queue`.
fn queue_problems<'a>(catalog: &'a AutomationCatalogLoad, queue: &str) -> Vec<&'a CatalogProblem> {
    catalog
        .problems
        .iter()
        .filter(|problem| problem_names_queue(&problem.file, queue))
        .collect()
}

fn problem_view(problem: &CatalogProblem) -> CatalogProblemView {
    CatalogProblemView {
        file: problem.file.display().to_string(),
        reason: problem.reason.clone(),
    }
}

/// The interval and the cron expression of `queue`'s schedule.
fn schedule_fields(queue: Option<&Queue>) -> (Option<u64>, Option<String>) {
    match queue {
        Some(queue) => (
            queue.schedule.interval_secs(),
            queue.schedule.cron().map(|cron| cron.as_str().to_string()),
        ),
        None => (None, None),
    }
}

/// The wire name of a `snake_case` enum value (`needs_attention`, say).
fn wire_name<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unrecognized".to_string())
}

/// A count on the wire.
fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// What the queue view shows of one automation of `queue` at `now`.
fn automation_view(
    pair: &PairState<'_>,
    queue: Option<&Queue>,
    now: DateTime<Utc>,
) -> QueueAutomationView {
    let record = pair.record.as_ref();
    QueueAutomationView {
        automation: pair.name.to_string(),
        description: pair
            .automation
            .map(|automation| automation.description.clone())
            .unwrap_or_default(),
        scripts: HookPhase::ALL
            .into_iter()
            .filter(|phase| {
                pair.automation
                    .is_some_and(|automation| automation.has(*phase))
            })
            .map(|phase| phase.as_str().to_string())
            .collect(),
        secrets: pair
            .automation
            .map(|automation| {
                automation
                    .secrets
                    .iter()
                    .map(|secret| ObserverSecretRef {
                        name: secret.name.clone(),
                        env: secret.env.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        state: wire_name(&record.map_or(AutomationState::Active, |record| record.state)),
        next_run_at: record.and_then(|record| record.next_run_at),
        waiting_for_claim_since: record.and_then(AutomationRecord::waiting_for_claim_since),
        worker_missing: match (record, queue) {
            (Some(record), Some(queue)) => record.worker_missing(&queue.schedule, now),
            _ => false,
        },
        last_claim_at: record.and_then(|record| record.last_claim_at),
        active_run: record
            .and_then(|record| record.active_run.as_ref())
            .map(|active| active.sequence),
        last_run: record
            .and_then(|record| record.last_run.as_ref())
            .map(|run| ObserverLastRunView {
                sequence: run.sequence,
                status: wire_name(&run.status),
                finished_at: run.finished_at,
                error: run.error.clone(),
            }),
        consecutive_failures: record.map_or(0, |record| record.consecutive_failures),
        errors: pair.errors.iter().map(run_error_view).collect(),
    }
}

fn run_error_view(failure: &RunFailure) -> ObserverRunErrorView {
    ObserverRunErrorView {
        run_id: failure.run_id().to_string(),
        sequence: failure.sequence,
        phase: failure.phase.as_str().to_string(),
        script_digest: failure.script_digest.clone(),
        status: wire_name(&failure.status),
        claimed_at: failure.claimed_at,
        finished_at: failure.finished_at,
        exit_code: failure.exit_code,
        error: failure.error.clone(),
        stdout: failure.stdout.clone(),
        stderr: failure.stderr.clone(),
        entry_id: failure.entry_id.clone(),
    }
}

/// What the queues view shows of `entry`: per automation of its queue, a
/// run in progress, the last run time and the newest error (R23).
fn entry_view(entry: &QueueEntry, pairs: &[PairState<'_>]) -> QueueEntryView {
    // The store keeps one error per entry (R23), newest first.
    let newest_errors: BTreeMap<&str, &RunFailure> = pairs
        .iter()
        .filter_map(|pair| {
            pair.errors
                .iter()
                .find(|failure| failure.entry_id.as_deref() == Some(entry.entry_id.as_str()))
                .map(|failure| (pair.name, failure))
        })
        .collect();
    QueueEntryView {
        entry_id: entry.entry_id.clone(),
        task_id: entry.task_id.to_string(),
        step: entry.step.clone(),
        episode: entry.episode.clone(),
        started_at: entry.created_at,
        params: entry.params.clone(),
        automations: pairs
            .iter()
            .map(|pair| {
                let record = pair.record.as_ref();
                QueueEntryAutomationView {
                    automation: pair.name.to_string(),
                    running: record.is_some_and(|record| {
                        [record.active_run.as_ref(), record.active_hook.as_ref()]
                            .into_iter()
                            .flatten()
                            .filter_map(|active| active.entry.as_ref())
                            .any(|run| run.entry_id == entry.entry_id)
                    }),
                    last_run_at: record
                        .and_then(|record| record.entry_last_runs.get(&entry.entry_id))
                        .copied(),
                    last_error: newest_errors
                        .get(pair.name)
                        .map(|failure| QueueEntryErrorView {
                            run_id: failure.run_id().to_string(),
                            status: wire_name(&failure.status),
                            finished_at: failure.finished_at,
                            exit_code: failure.exit_code,
                            error: failure.error.clone(),
                        }),
                }
            })
            .collect(),
        gate_phase: None,
    }
}
