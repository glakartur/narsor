//! The effects an observer's script has in Narsor, through the routes its
//! run token opens (R12, R13, R15, R19):
//!
//! ```text
//! create task   no create_task effect → refused
//!               over limit            → refused; observer needs_attention
//!               otherwise             → create_task (random id) → created
//! decide        run with no entry (always), or another task or episode
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
//!                                       observer and its run; a paused task
//!                                       resumed → delivered
//! ```
//!
//! The server does not deduplicate tasks: every call creates one. The
//! script keeps track in its state of what it already reported. An answer
//! is one per run and episode: a retry of the same answer returns the
//! input it queued.
//!
//! Every effect runs under the observer's lock, so it is serialized with
//! the other effects of the run and with the run's result: an effect never
//! lands after the run ended.

use chrono::{DateTime, Utc};

use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::ProtocolService;
use crate::controller_protocol::wire::*;
use crate::domain::TaskId;
use crate::engine::actions::check_gate_answer;
use crate::engine::HumanInputDelivery;
use crate::observers::{
    ActiveRun, ObserverEffect, ObserverError, ObserverLock, ObserverRecord, ObserverState, RunId,
};
use crate::orchestration::{
    observer_decide_key, InputRequest, OrchestrationError, TaskProvenance, TaskRecord,
};

use super::entries::waits_in;

/// Most bytes of a "create a task" effect's data, as JSON.
pub const MAX_EFFECT_DATA_BYTES: usize = 64 * 1024;

/// The run an effect belongs to, read under the observer's lock: the
/// observer's record and its run in progress.
struct EffectScope {
    lock: ObserverLock,
    record: ObserverRecord,
    run_id: RunId,
    now: DateTime<Utc>,
}

impl EffectScope {
    /// The provenance of what this run causes (R19).
    fn provenance(&self) -> TaskProvenance {
        TaskProvenance {
            observer_id: self.record.observer_id.to_string(),
            observer_name: self.record.config.name.to_string(),
            run_id: self.run_id.to_string(),
        }
    }

    /// The run in progress; [`ProtocolService::effect_scope`] checked that
    /// there is one.
    fn active_run(&mut self) -> ProtocolResult<&mut ActiveRun> {
        self.record
            .active_run
            .as_mut()
            .ok_or(ProtocolError::Unauthorized)
    }
}

impl ProtocolService {
    /// Creates a task from the effect data for the run `run_id`
    /// authenticated (R3, R15, R19).
    ///
    /// The task belongs to the observer's owner, gets a random id and runs
    /// what the observer's record says; the script supplies only the data.
    /// Past the run's task limit the call is refused with a conflict and an
    /// active observer goes to `needs_attention`.
    pub fn observer_create_task(
        &self,
        run_id: &RunId,
        request: ObserverCreateTaskRequest,
    ) -> ProtocolResult<ObserverEffectResponse> {
        let mut scope = self.effect_scope(run_id)?;
        let ObserverEffect::CreateTask(effect) = &scope.record.config.effect else {
            return Err(effect_not_allowed(&scope.record, "create tasks"));
        };
        check_size("effect data", &request.data, MAX_EFFECT_DATA_BYTES)?;
        let task_request = effect.task_request(&request.data).map_err(invalid_effect)?;
        self.within_task_limit(&mut scope)?;
        let summary =
            self.create_task_from(&scope.record.owner, task_request, Some(scope.provenance()))?;
        let created = TaskId::parse(summary.task_id.to_string())
            .map_err(|error| ProtocolError::Internal(error.to_string()))?;
        let active = scope.active_run()?;
        active.tasks_created = active.tasks_created.saturating_add(1);
        self.observers
            .save(&scope.lock, &mut scope.record, scope.now)?;
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
    /// event name the observer and its run (R15). The reply says the answer
    /// was queued, not that the runner applied it.
    pub fn observer_decide(
        &self,
        run_id: &RunId,
        request: ObserverDecideRequest,
    ) -> ProtocolResult<ObserverEffectResponse> {
        let scope = self.effect_scope(run_id)?;
        let Some(entry) = scope
            .record
            .active_run
            .as_ref()
            .and_then(|active| active.entry.clone())
        else {
            return Err(ProtocolError::bad_request(
                "no_queue_entry",
                format!(
                    "run {run_id} of observer '{}' runs for no queue entry: only a run of an \
                     on_episodes observer for an entry answers a gate",
                    scope.record.config.name
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
        if !waits_in(&task, &scope.record.owner, &entry.episode) {
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
            tracing::info!(run = %run_id, task = %task.task_id, episode = %entry.episode, choice = %request.choice, "observers: a script answered a gate");
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

    /// Takes the observer's lock and reads the observer's record under it,
    /// refusing a run that is no longer the observer's run in progress
    /// before its deadline: its token stopped working between the gate and
    /// now.
    fn effect_scope(&self, run_id: &RunId) -> ProtocolResult<EffectScope> {
        let lock = self.observers.lock(&run_id.observer_id).map_err(run_gone)?;
        let record = self.observers.load(&run_id.observer_id).map_err(run_gone)?;
        let now = self.control.now();
        let in_progress = record.active_run.as_ref().is_some_and(|active| {
            active.sequence == run_id.sequence && !active.is_past_deadline(now)
        });
        if !in_progress {
            return Err(ProtocolError::Unauthorized);
        }
        Ok(EffectScope {
            lock,
            record,
            run_id: run_id.clone(),
            now,
        })
    }

    /// Refuses a new task past the run's limit (R15); an active observer
    /// then waits for its owner (`needs_attention`).
    fn within_task_limit(&self, scope: &mut EffectScope) -> ProtocolResult<()> {
        let limit = scope.record.config.task_limit_per_run;
        if scope.active_run()?.tasks_created < limit {
            return Ok(());
        }
        if scope.record.state == ObserverState::Active {
            scope.record.state = ObserverState::NeedsAttention;
            self.observers
                .save(&scope.lock, &mut scope.record, scope.now)?;
            tracing::info!(observer = %scope.record.observer_id, limit, "observers: a run reached its task limit; waiting for the owner");
        }
        Err(ProtocolError::conflict(
            "observer_task_limit",
            format!(
                "run {} reached the limit of {limit} new tasks per run; observer '{}' waits for its owner",
                scope.run_id,
                scope.record.config.name
            ),
        ))
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

/// A run that went away under the caller (its observer deleted): its token
/// no longer proves anything.
fn run_gone(error: ObserverError) -> ProtocolError {
    match error {
        ObserverError::NotFound(_) => ProtocolError::Unauthorized,
        other => other.into(),
    }
}

/// Effect data the script got wrong.
fn invalid_effect(error: ObserverError) -> ProtocolError {
    match error {
        ObserverError::Invalid { .. } => {
            ProtocolError::bad_request("invalid_effect_data", error.to_string())
        }
        other => other.into(),
    }
}

/// The refusal of an effect the observer's record does not allow.
fn effect_not_allowed(record: &ObserverRecord, what: &str) -> ProtocolError {
    ProtocolError::bad_request(
        "effect_not_allowed",
        format!("observer '{}' may not {what}", record.config.name),
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
