//! The effects an observer's script has in Narsor, through the routes its
//! run token opens (R4, R15, R19):
//!
//! ```text
//! create task   over limit            → refused; observer needs_attention
//!               otherwise             → create_task (random id) → created
//! deliver event task not watched      → refused
//!               task waits, matches   → history + input queued + resume
//!               otherwise             → history only
//! ```
//!
//! The server does not deduplicate effects: every call creates a task or
//! records an event. The script keeps track in its state of what it already
//! reported.
//!
//! Every effect runs under the observer's lock, so it is serialized with
//! the other effects of the run and with the run's result: an effect never
//! lands after the run ended. An event takes the task's lock inside it
//! (observer, then task: never the other way round).
//!
//! An event that matches the task's waiting `wait` step is stored on the
//! task record as a queued input, as the owner's choice of a step's way out
//! is (D1). The server makes a new idempotency key for each event
//! ([`observer_event_key`]), which the queued input and the resume it causes
//! carry. A task whose runner does not run keeps the input until its runner
//! polls again.

use chrono::{DateTime, Utc};

use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::{parse_identifier, ProtocolService};
use crate::controller_protocol::wire::*;
use crate::domain::TaskId;
use crate::observers::{
    ActiveRun, ObserverEffect, ObserverError, ObserverLock, ObserverRecord, ObserverState, RunId,
    MAX_EVENT_TYPE_LEN,
};
use crate::orchestration::{observer_event_key, ExternalEvent, TaskProvenance, TaskRecord};

/// Most bytes of an event's data, as JSON. The history keeps it, so it is
/// small: a review state, a URL, a few ids.
pub const MAX_EVENT_DATA_BYTES: usize = 16 * 1024;

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

    /// Delivers an external event to the task `task_id` of one of the
    /// observer's watch entries (R4, R19); see the module
    /// documentation for the cases.
    ///
    /// The task must be on the observer's watch list and belong to the
    /// observer's owner. The event goes to the task's history; when the
    /// task waits for it, it is also queued for the waiting step and a
    /// stopped task is resumed to take it.
    pub fn observer_deliver_event(
        &self,
        run_id: &RunId,
        task_id: &str,
        request: ObserverEventRequest,
    ) -> ProtocolResult<ObserverEffectResponse> {
        let scope = self.effect_scope(run_id)?;
        if scope.record.config.effect != ObserverEffect::DeliverEvent {
            return Err(effect_not_allowed(&scope.record, "deliver events"));
        }
        validate_event_type(&request.event_type)?;
        check_size("event data", &request.data, MAX_EVENT_DATA_BYTES)?;
        let task_id = parse_identifier(TaskId::parse(task_id))?;
        // The watch list first, so a task outside it is refused without
        // anything about it being read.
        let watched = self
            .observers
            .list_watches(&scope.record.observer_id)?
            .iter()
            .any(|entry| entry.task_id == task_id);
        if !watched {
            return Err(ProtocolError::bad_request(
                "task_not_watched",
                format!(
                    "task '{task_id}' is not on the watch list of observer '{}'",
                    scope.record.config.name
                ),
            ));
        }
        let task = self.observer_task(&scope.record, &task_id)?;
        let outcome = self.control.record_external_event(
            &task.task_id,
            ExternalEvent {
                idempotency_key: observer_event_key(&run_id.to_string()),
                event_type: request.event_type,
                source: scope.record.config.source.clone(),
                data: request.data,
                provenance: scope.provenance(),
            },
        )?;
        self.notify(&task_id);
        if let Some(input) = &outcome.input {
            self.resume_for_work(&outcome.task, &input.idempotency_key)?;
        }
        let input_id = outcome.input.map(|input| input.input_id);
        let (outcome, message) = match &input_id {
            Some(input_id) => (
                ObserverEffectOutcome::Delivered,
                format!("delivered to the waiting step of task {task_id} as {input_id}"),
            ),
            None => (
                ObserverEffectOutcome::Recorded,
                format!("recorded in the history of task {task_id}; it does not wait for it"),
            ),
        };
        Ok(ObserverEffectResponse {
            outcome,
            task_id: Some(task_id.to_string()),
            input_id,
            message,
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

    /// The task an event goes to: it must belong to the observer's owner.
    /// Another owner's task is indistinguishable from none.
    fn observer_task(
        &self,
        record: &ObserverRecord,
        task_id: &TaskId,
    ) -> ProtocolResult<TaskRecord> {
        let task = self.control.task(task_id)?;
        if task.owner != record.owner {
            return Err(ProtocolError::NotFound);
        }
        Ok(task)
    }
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

/// Refuses an empty, overlong, whitespace or control-character event type.
fn validate_event_type(event_type: &str) -> ProtocolResult<()> {
    if event_type.is_empty()
        || event_type.len() > MAX_EVENT_TYPE_LEN
        || event_type
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(ProtocolError::bad_request(
            "invalid_effect_data",
            format!(
                "an event type must have 1-{MAX_EVENT_TYPE_LEN} bytes and no whitespace or control characters"
            ),
        ));
    }
    Ok(())
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
