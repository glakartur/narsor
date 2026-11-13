//! The effects an observer's script has in Narsor, through the routes its
//! run token opens (R4, R10, R13, R15, R19):
//!
//! ```text
//! create task   key created           → duplicate (task id), nothing else
//!               key pending           → create_task from the recorded request
//!               new key, over limit   → refused; observer needs_attention
//!               new key               → effect file pending → create_task → created
//!               create_task refused   → effect file removed (a later call may retry)
//! deliver event task not watched      → refused
//!               key seen by the task  → duplicate
//!               task waits, matches   → history + input queued + resume
//!               otherwise             → history only
//! dry run       the same checks; "would" recorded in the run, nothing else
//! ```
//!
//! Every effect runs under the observer's lock, so it is serialized with
//! the other effects of the run and with the run's result: an effect never
//! lands after the run ended. An event takes the task's lock inside it
//! (observer, then task: never the other way round).
//!
//! A task created twice is impossible by construction: its id derives from
//! the observer id and the key ([`effect_task_id`]), the effect file is
//! created exclusively and records the full request before the task exists,
//! and a retry replays that request, whose digest the control plane
//! recognizes. An event is recorded at most once per key and task under the
//! task's lock ([`crate::orchestration::ControlPlane::record_external_event`]).

use chrono::{DateTime, Utc};

use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::{parse_identifier, ProtocolService};
use crate::controller_protocol::wire::*;
use crate::domain::TaskId;
use crate::observers::{
    effect_task_id, validate_effect_key, CreateTaskEffect, EffectRecord, EffectState, EffectWrite,
    ObserverEffect, ObserverError, ObserverLock, ObserverRecord, ObserverRun, ObserverState,
    PlannedEffect, PlannedEffectKind, RunId, RunStatus, EFFECT_RECORD_VERSION, MAX_EVENT_TYPE_LEN,
};
use crate::orchestration::{
    observer_event_key, ExternalEvent, OrchestrationError, TaskProvenance, TaskRecord,
};

/// Most bytes of an event's data, as JSON. The history keeps it, so it is
/// small: a review state, a URL, a few ids.
pub const MAX_EVENT_DATA_BYTES: usize = 16 * 1024;

/// Most bytes of a "create a task" effect's data, as JSON.
pub const MAX_EFFECT_DATA_BYTES: usize = 64 * 1024;

/// What `narsor-cli` prints for a key used before (PO3: not an error).
const DUPLICATE_MESSAGE: &str = "skipped: duplicate";

/// The run an effect belongs to, read under the observer's lock.
struct EffectScope {
    lock: ObserverLock,
    record: ObserverRecord,
    run: ObserverRun,
    now: DateTime<Utc>,
}

impl EffectScope {
    /// The provenance of what `effect_key` causes in this run (R19).
    fn provenance(&self, effect_key: &str) -> TaskProvenance {
        TaskProvenance {
            observer_id: self.record.observer_id.to_string(),
            observer_name: self.record.config.name.to_string(),
            effect_key: effect_key.to_string(),
            run_id: self.run.run_id().to_string(),
        }
    }
}

impl ProtocolService {
    /// Creates the task of effect key `request.effect_key` for the run
    /// `run_id` authenticated (R3, R13, R15, R19); see the module
    /// documentation for the cases.
    ///
    /// The task belongs to the observer's owner and runs what the
    /// observer's record says; the script supplies only the key and the
    /// data. A key used before answers [`ObserverEffectOutcome::Duplicate`]
    /// with its task. A new key past the run's task limit is refused with a
    /// conflict and, outside a dry run, puts an active observer in
    /// `needs_attention`.
    pub fn observer_create_task(
        &self,
        run_id: &RunId,
        request: ObserverCreateTaskRequest,
    ) -> ProtocolResult<ObserverEffectResponse> {
        let mut scope = self.effect_scope(run_id)?;
        let ObserverEffect::CreateTask(effect) = scope.record.config.effect.clone() else {
            return Err(effect_not_allowed(&scope.record, "create tasks"));
        };
        let key = request.effect_key;
        validate_effect_key(&key).map_err(invalid_effect)?;
        check_size("effect data", &request.data, MAX_EFFECT_DATA_BYTES)?;
        let observer_id = scope.record.observer_id.clone();
        let task_id = effect_task_id(&observer_id, &key);

        // A key used before is a duplicate whatever the data says now.
        let existing = self.observers.load_effect(&observer_id, &key)?;
        if let Some(EffectRecord {
            state: EffectState::Created { task_id, .. },
            ..
        }) = &existing
        {
            return Ok(duplicate(&key, Some(task_id.to_string())));
        }
        if scope.run.dry_run {
            return self.plan_task(scope, &effect, key, request.data);
        }
        if existing.is_none() && self.owner_has_task(&scope.record, &task_id)? {
            // The task exists although no file says so (the file was lost):
            // the key is spent, and the file says so from now on.
            self.confirm_effect(&scope, &key, task_id.clone())?;
            return Ok(duplicate(&key, Some(task_id.to_string())));
        }
        self.within_task_limit(&mut scope)?;
        let pending = match existing {
            // A crash or a transient failure after the file was written:
            // the recorded request, never the new data (R13).
            Some(effect) => effect,
            None => {
                let task_request = effect
                    .task_request(&observer_id, &key, &request.data)
                    .map_err(invalid_effect)?;
                let effect =
                    EffectRecord::pending(&key, scope.run.sequence, task_request, scope.now);
                match self.observers.create_effect(&scope.lock, &effect)? {
                    EffectWrite::Created => effect,
                    // Unreachable under the lock; answered from the file.
                    EffectWrite::Exists(existing) => match existing.state {
                        EffectState::Created { task_id, .. } => {
                            return Ok(duplicate(&key, Some(task_id.to_string())));
                        }
                        EffectState::Pending { .. } => *existing,
                    },
                }
            }
        };
        let EffectState::Pending {
            request: task_request,
        } = pending.state
        else {
            return Ok(duplicate(&key, Some(task_id.to_string())));
        };
        let provenance = scope.provenance(&key);
        match self.create_task_from(&scope.record.owner, *task_request, Some(provenance)) {
            Ok(summary) => {
                let created = TaskId::parse(summary.task_id.to_string())
                    .map_err(|error| ProtocolError::Internal(error.to_string()))?;
                self.confirm_effect(&scope, &key, created.clone())?;
                scope.run.tasks_created = scope.run.tasks_created.saturating_add(1);
                self.observers.save_run(&scope.lock, &scope.run)?;
                Ok(ObserverEffectResponse {
                    outcome: ObserverEffectOutcome::Created,
                    effect_key: key,
                    task_id: Some(created.to_string()),
                    input_id: None,
                    message: format!("created task {created}"),
                })
            }
            Err(error) => {
                if is_permanent_refusal(&error) {
                    // Refused for good (an invalid input, an unknown
                    // backend): the key is free again for a later call.
                    self.observers.remove_effect(&scope.lock, &key)?;
                }
                Err(error)
            }
        }
    }

    /// Delivers an external event to the task `task_id` of one of the
    /// observer's watch entries (R4, R13, R19); see the module
    /// documentation for the cases.
    ///
    /// The task must be on the observer's watch list and belong to the
    /// observer's owner. The event goes to the task's history once per
    /// key; when the task waits for it, it is also queued for the waiting
    /// step and a paused task is resumed to take it.
    pub fn observer_deliver_event(
        &self,
        run_id: &RunId,
        task_id: &str,
        request: ObserverEventRequest,
    ) -> ProtocolResult<ObserverEffectResponse> {
        let mut scope = self.effect_scope(run_id)?;
        if scope.record.config.effect != ObserverEffect::DeliverEvent {
            return Err(effect_not_allowed(&scope.record, "deliver events"));
        }
        let key = request.effect_key;
        validate_effect_key(&key).map_err(invalid_effect)?;
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
        let idempotency_key = observer_event_key(scope.record.observer_id.as_str(), &key);

        if scope.run.dry_run {
            let planned = scope.run.planned_effects.iter().any(|effect| {
                effect.kind == PlannedEffectKind::DeliverEvent
                    && effect.effect_key == key
                    && effect.task_id.as_ref() == Some(&task_id)
            });
            if planned || task.external_event_keys.contains(&idempotency_key) {
                return Ok(duplicate(&key, Some(task_id.to_string())));
            }
            scope.run.planned_effects.push(PlannedEffect {
                kind: PlannedEffectKind::DeliverEvent,
                effect_key: key.clone(),
                task_id: Some(task_id.clone()),
                data: request.data,
            });
            self.observers.save_run(&scope.lock, &scope.run)?;
            return Ok(ObserverEffectResponse {
                outcome: ObserverEffectOutcome::WouldDeliver,
                effect_key: key,
                task_id: Some(task_id.to_string()),
                input_id: None,
                message: format!(
                    "dry run: would deliver '{}' to task {task_id}",
                    request.event_type
                ),
            });
        }

        let outcome = self.control.record_external_event(
            &task_id,
            ExternalEvent {
                idempotency_key,
                event_type: request.event_type.clone(),
                source: scope.record.config.source.clone(),
                data: request.data,
                provenance: scope.provenance(&key),
            },
        )?;
        if !outcome.replayed {
            self.notify(&task_id);
        }
        // A retry repairs a resume the first call stored the input for and
        // failed before.
        if let Some(input) = outcome.input.as_ref().filter(|i| !i.status.is_settled()) {
            self.resume_for_work(&outcome.task, &input.idempotency_key)?;
        }
        let input_id = outcome.input.map(|input| input.input_id);
        let (outcome, message) = match (outcome.replayed, &input_id) {
            (true, _) => (
                ObserverEffectOutcome::Duplicate,
                DUPLICATE_MESSAGE.to_string(),
            ),
            (false, Some(input_id)) => (
                ObserverEffectOutcome::Delivered,
                format!("delivered to the waiting step of task {task_id} as {input_id}"),
            ),
            (false, None) => (
                ObserverEffectOutcome::Recorded,
                format!("recorded in the history of task {task_id}; it does not wait for it"),
            ),
        };
        Ok(ObserverEffectResponse {
            outcome,
            effect_key: key,
            task_id: Some(task_id.to_string()),
            input_id,
            message,
        })
    }

    /// Takes the observer's lock and reads the run `run_id` under it,
    /// refusing a run that is no longer in progress before its deadline:
    /// its token stopped working between the gate and now.
    fn effect_scope(&self, run_id: &RunId) -> ProtocolResult<EffectScope> {
        let lock = self.observers.lock(&run_id.observer_id).map_err(run_gone)?;
        let record = self.observers.load(&run_id.observer_id).map_err(run_gone)?;
        let run = self.observers.load_run(run_id).map_err(run_gone)?;
        let now = self.control.now();
        if record.active_run != Some(run_id.sequence)
            || run.status != RunStatus::Running
            || now >= run.deadline
        {
            return Err(ProtocolError::Unauthorized);
        }
        Ok(EffectScope {
            lock,
            record,
            run,
            now,
        })
    }

    /// The dry run of "create a task": the same checks, the plan recorded
    /// in the run, no effect file and no task (R10).
    fn plan_task(
        &self,
        mut scope: EffectScope,
        effect: &CreateTaskEffect,
        key: String,
        data: serde_json::Value,
    ) -> ProtocolResult<ObserverEffectResponse> {
        let task_id = effect_task_id(&scope.record.observer_id, &key);
        let planned =
            scope.run.planned_effects.iter().any(|effect| {
                effect.kind == PlannedEffectKind::CreateTask && effect.effect_key == key
            });
        if planned || self.owner_has_task(&scope.record, &task_id)? {
            return Ok(duplicate(&key, Some(task_id.to_string())));
        }
        effect
            .task_request(&scope.record.observer_id, &key, &data)
            .map_err(invalid_effect)?;
        self.within_task_limit(&mut scope)?;
        scope.run.tasks_created = scope.run.tasks_created.saturating_add(1);
        scope.run.planned_effects.push(PlannedEffect {
            kind: PlannedEffectKind::CreateTask,
            effect_key: key.clone(),
            task_id: Some(task_id.clone()),
            data,
        });
        self.observers.save_run(&scope.lock, &scope.run)?;
        Ok(ObserverEffectResponse {
            outcome: ObserverEffectOutcome::WouldCreate,
            effect_key: key,
            task_id: Some(task_id.to_string()),
            input_id: None,
            message: format!("dry run: would create task {task_id}"),
        })
    }

    /// Refuses a new task past the run's limit (R15). Outside a dry run an
    /// active observer then waits for its owner (`needs_attention`); a dry
    /// run leaves the observer's state alone.
    fn within_task_limit(&self, scope: &mut EffectScope) -> ProtocolResult<()> {
        let limit = scope.record.config.task_limit_per_run;
        if scope.run.tasks_created < limit {
            return Ok(());
        }
        if !scope.run.dry_run && scope.record.state == ObserverState::Active {
            scope.record.state = ObserverState::NeedsAttention;
            self.observers
                .save(&scope.lock, &mut scope.record, scope.now)?;
            tracing::info!(observer = %scope.record.observer_id, limit, "observers: a run reached its task limit; waiting for the owner");
        }
        Err(ProtocolError::conflict(
            "observer_task_limit",
            format!(
                "run {} reached the limit of {limit} new tasks per run; observer '{}' waits for its owner",
                scope.run.run_id(),
                scope.record.config.name
            ),
        ))
    }

    /// Marks the effect of `key` created, with its task: the pending file
    /// becomes `created`, or a `created` file is written where none is.
    fn confirm_effect(
        &self,
        scope: &EffectScope,
        key: &str,
        task_id: TaskId,
    ) -> ProtocolResult<()> {
        let state = EffectState::Created {
            task_id,
            created_at: scope.now,
        };
        match self.observers.load_effect(&scope.record.observer_id, key)? {
            Some(mut effect) => {
                effect.state = state;
                self.observers.save_effect(&scope.lock, &effect)?;
            }
            None => {
                self.observers.create_effect(
                    &scope.lock,
                    &EffectRecord {
                        record_version: EFFECT_RECORD_VERSION,
                        effect_key: key.to_string(),
                        run_sequence: scope.run.sequence,
                        recorded_at: scope.now,
                        state,
                    },
                )?;
            }
        }
        Ok(())
    }

    /// Whether the observer's owner has the task `task_id`.
    fn owner_has_task(&self, record: &ObserverRecord, task_id: &TaskId) -> ProtocolResult<bool> {
        match self.control.task(task_id) {
            Ok(task) => Ok(task.owner == record.owner),
            Err(OrchestrationError::TaskNotFound(_)) => Ok(false),
            Err(error) => Err(error.into()),
        }
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

/// The answer to a key used before.
fn duplicate(key: &str, task_id: Option<String>) -> ObserverEffectResponse {
    ObserverEffectResponse {
        outcome: ObserverEffectOutcome::Duplicate,
        effect_key: key.to_string(),
        task_id,
        input_id: None,
        message: DUPLICATE_MESSAGE.to_string(),
    }
}

/// A run that went away under the caller (its observer deleted): its token
/// no longer proves anything.
fn run_gone(error: ObserverError) -> ProtocolError {
    match error {
        ObserverError::NotFound(_) | ObserverError::RunNotFound(_) => ProtocolError::Unauthorized,
        other => other.into(),
    }
}

/// An effect key or data the script got wrong.
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

/// Whether `create_task` refused the request for good, so retrying the
/// same request can never succeed: a validation or launch refusal (`400`),
/// or a task id another owner holds. A transient failure keeps the effect
/// pending, and a retry replays it.
fn is_permanent_refusal(error: &ProtocolError) -> bool {
    matches!(
        error,
        ProtocolError::BadRequest { .. } | ProtocolError::NotFound
    )
}
