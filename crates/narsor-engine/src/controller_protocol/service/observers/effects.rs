//! The effects an automation's script has in Narsor, through the routes
//! its run token opens (R12, R13, R15, R19):
//!
//! ```text
//! create task   a run whose phase is not `check`
//!                                     → refused (not_a_check_run)
//!               no installation owner (unclaimed)       → refused
//!               otherwise             → create_task with the request as the
//!                                       owner sends it, owned by the
//!                                       installation owner, provenance of
//!                                       the run → created
//! decide        a run whose phase is not `check`
//!                                     → refused (not_a_check_run)
//!               run with no entry (always), or another task or episode
//!                                     → refused
//!               task no longer waits in the episode (answered, left, ended)
//!                                     → refused (stale_episode)
//!               task waits, its runner reports a problem or is starting
//!                                     → refused (gate_unavailable)
//!               choice unknown, required variable missing, variable not
//!               declared              → refused (invalid_answer)
//!               another answer for the episode queued or taken
//!                                     → refused (first wins)
//!               otherwise             → input queued for the gate, by the
//!                                       automation and its run; a paused
//!                                       task resumed → delivered
//! ```
//!
//! The server does not deduplicate tasks: every call creates one. The
//! script keeps track in its state of what it already reported. An answer
//! is one per run and episode: a retry of the same answer returns the
//! input it queued.
//!
//! Every effect runs under the pair's lock, so it is serialized with the
//! other effects of the run and with the run's result: an effect never
//! lands after the run ended.

use crate::automations::HookPhase;
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::tasks::launch_refusal;
use crate::controller_protocol::service::ProtocolService;
use crate::controller_protocol::wire::*;
use crate::domain::TaskId;
use crate::engine::actions::check_gate_answer;
use crate::engine::HumanInputDelivery;
use crate::observers::{ActiveRun, AutomationLock, AutomationRecord, RunEntry, RunId};
use crate::orchestration::{
    observer_decide_key, InputRequest, OrchestrationError, TaskProvenance, TaskRecord,
};

use super::entries::waits_in;

/// Refuses an effect of a run that is not a `check` run: an `in` or `out`
/// script neither answers a gate nor creates a task.
fn require_check_run(active: &ActiveRun, run_id: &RunId) -> ProtocolResult<()> {
    if active.phase == HookPhase::Check {
        return Ok(());
    }
    Err(ProtocolError::bad_request(
        "not_a_check_run",
        format!(
            "run {run_id} runs the {} script of automation '{}': only a check run has effects \
             (answers a gate, creates a task)",
            active.phase, run_id.key.automation
        ),
    ))
}

/// Most bytes of an effect's data (an answer's variables), as JSON.
pub const MAX_EFFECT_DATA_BYTES: usize = 64 * 1024;

/// The run an effect belongs to, read under the pair's lock: the pair's
/// record and its run in progress, a `check` run.
struct EffectScope {
    /// Held while the effect runs: it never lands after the run ended.
    _lock: AutomationLock,
    record: AutomationRecord,
    run_id: RunId,
    /// The queue entry the run checks, as the record held it.
    entry: Option<RunEntry>,
}

impl EffectScope {
    /// The provenance of what this run causes (R19).
    fn provenance(&self) -> TaskProvenance {
        // The provenance keeps its stored shape: the queue stands where
        // the observer's id stood, the automation where its name stood.
        TaskProvenance {
            observer_id: self.record.key.queue.clone(),
            observer_name: self.record.key.automation.clone(),
            run_id: self.run_id.to_string(),
        }
    }
}

impl ProtocolService {
    /// Creates a task for the run `run_id` authenticated (R6, R19): the
    /// same request the owner sends (`narsor-cli task create`), checked the
    /// same way; version `0` is the catalogue's. The task belongs to the installation owner (A4) and records
    /// the queue, the automation and the run.
    pub fn observer_create_task(
        &self,
        run_id: &RunId,
        mut request: CreateTaskRequest,
    ) -> ProtocolResult<ObserverEffectResponse> {
        let scope = self.effect_scope(run_id)?;
        // No version given: the catalogue's, as the owner's CLI asks the
        // workflow list for it (a run token cannot list workflows).
        if request.launch.workflow_version == 0 {
            request.launch.workflow_version = self
                .launch
                .workflow_definition(&request.launch.workflow_id)
                .map_err(launch_refusal)?
                .version;
        }
        let owner = (self.installation_owner)().ok_or_else(|| {
            ProtocolError::conflict(
                "no_installation_owner",
                "the installation has no owner yet; tasks of queues belong to the owner",
            )
        })?;
        let summary = self.create_task_from(&owner, request, Some(scope.provenance()))?;
        let created = TaskId::parse(summary.task_id.to_string())
            .map_err(|error| ProtocolError::Internal(error.to_string()))?;
        Ok(ObserverEffectResponse {
            outcome: ObserverEffectOutcome::Created,
            task_id: Some(created.to_string()),
            input_id: None,
            message: format!("created task {created}"),
        })
    }

    /// Answers the gate of the queue entry the run `run_id` was started
    /// for, as a person answers it (R12, R13, KTD-6); see the module
    /// documentation for the cases.
    ///
    /// The answer goes the way the owner's goes: the same checks of the
    /// gate's form ([`check_gate_answer`]), queued for the episode it names
    /// so the runner refuses it should the episode be gone by then, and a
    /// paused task resumed to take it (AE5). The input and the answer's
    /// event name the automation and its run (R15). The reply says the
    /// answer was queued, not that the runner applied it. Only a `check`
    /// run answers: an `in` or `out` script never decides the gate.
    pub fn observer_decide(
        &self,
        run_id: &RunId,
        request: ObserverDecideRequest,
    ) -> ProtocolResult<ObserverEffectResponse> {
        let scope = self.effect_scope(run_id)?;
        let Some(entry) = scope.entry.clone() else {
            return Err(ProtocolError::bad_request(
                "no_queue_entry",
                format!(
                    "run {run_id} of automation '{}' runs for no queue entry: only a run of an \
                     on_episodes queue for an entry answers a gate",
                    run_id.key.automation
                ),
            ));
        };
        if request.task_id != entry.task_id.as_str() || request.episode != entry.episode {
            return Err(ProtocolError::bad_request(
                "not_the_runs_entry",
                format!(
                    "run {run_id} was started for task {} in episode {} and answers that entry only",
                    entry.task_id, entry.episode
                ),
            ));
        }
        check_size(
            "answer",
            &serde_json::json!({"reason": request.reason, "variables": request.variables}),
            MAX_EFFECT_DATA_BYTES,
        )?;
        let stale = || {
            ProtocolError::conflict(
                "stale_episode",
                format!(
                    "task {} no longer waits in episode {}: the gate was answered or left",
                    entry.task_id, entry.episode
                ),
            )
        };
        let task = match self.control.task(&entry.task_id) {
            Ok(task) => task,
            Err(OrchestrationError::TaskNotFound(_)) => return Err(stale()),
            Err(error) => return Err(error.into()),
        };
        if !waits_in(&task, &task.owner, &entry.episode) {
            return Err(stale());
        }
        // The gate waits, but a runner that reports a problem or is still
        // starting shows no pending input (R17): it cannot take an answer
        // now, and the gate was neither answered nor left.
        let pending = task
            .runner
            .as_ref()
            .and_then(|runner| runner.snapshot.pending_input.as_ref())
            .filter(|pending| pending.id == entry.episode)
            .ok_or_else(|| {
                ProtocolError::conflict(
                    "gate_unavailable",
                    format!(
                        "task {} still waits in episode {}, but its runner cannot take an answer \
                         now (it reports a problem or is starting); a later run can answer",
                        entry.task_id, entry.episode
                    ),
                )
            })?;
        let delivery = HumanInputDelivery {
            values: request.variables,
            chosen_label: Some(request.choice.clone()),
            comment: request
                .reason
                .as_deref()
                .map(str::trim)
                .filter(|reason| !reason.is_empty())
                .map(ToString::to_string),
            episode: Some(entry.episode.clone()),
            ..HumanInputDelivery::default()
        };
        if let Err(error) = check_gate_answer(&pending.form, &delivery) {
            return Err(if error.is_stale() {
                stale()
            } else {
                ProtocolError::bad_request("invalid_answer", error.to_string())
            });
        }
        let outcome = self.control.queue_input_from(
            &task.task_id,
            InputRequest {
                pending_input_id: entry.episode.clone(),
                idempotency_key: observer_decide_key(&run_id.to_string(), &entry.episode),
                delivery,
            },
            Some(scope.provenance()),
        )?;
        if !outcome.replayed {
            self.notify(&task.task_id);
            // An answer at a gate with exit scripts holds its exit: they
            // are due now.
            self.wake_observer_claims();
            tracing::info!(run = %run_id, task = %task.task_id, episode = %entry.episode, choice = %request.choice, "automations: a check script answered a gate");
        }
        if !outcome.input.status.is_settled() {
            self.resume_for_work(&task, &outcome.input.idempotency_key)?;
        }
        Ok(ObserverEffectResponse {
            outcome: ObserverEffectOutcome::Delivered,
            task_id: Some(task.task_id.to_string()),
            input_id: Some(outcome.input.input_id.clone()),
            message: decided_message(&task, &request.choice, &outcome.input.input_id),
        })
    }

    /// Takes the pair's lock and reads the pair's record under it,
    /// refusing a run that is no longer the pair's run in progress before
    /// its deadline (its token stopped working between the gate and now),
    /// and a run that is not a `check` run.
    fn effect_scope(&self, run_id: &RunId) -> ProtocolResult<EffectScope> {
        let lock = self.observers.lock(&run_id.key)?;
        let record = self
            .observers
            .load(&run_id.key)?
            .ok_or(ProtocolError::Unauthorized)?;
        let now = self.control.now();
        let active = record
            .run_in_progress(run_id.sequence)
            .filter(|active| !active.is_past_deadline(now))
            .ok_or(ProtocolError::Unauthorized)?;
        require_check_run(active, run_id)?;
        let entry = active.entry.clone();
        Ok(EffectScope {
            _lock: lock,
            record,
            run_id: run_id.clone(),
            entry,
        })
    }
}

/// What `task decide` prints when the answer was queued.
fn decided_message(task: &TaskRecord, choice: &str, input_id: &str) -> String {
    let step = task
        .watched_gate
        .as_ref()
        .map_or("the gate", |gate| gate.watch.step.as_str());
    format!(
        "answered {step} of task {} with {choice} (input {input_id}); the gate exits with it \
         unless another answer came first",
        task.task_id
    )
}

/// Refuses data larger than `limit` bytes as JSON.
fn check_size(what: &str, data: &serde_json::Value, limit: usize) -> ProtocolResult<()> {
    let size = serde_json::to_vec(data)
        .map_err(|error| ProtocolError::Internal(error.to_string()))?
        .len();
    if size > limit {
        return Err(ProtocolError::bad_request(
            "invalid_effect_data",
            format!("the {what} has {size} bytes; at most {limit} are allowed"),
        ));
    }
    Ok(())
}
