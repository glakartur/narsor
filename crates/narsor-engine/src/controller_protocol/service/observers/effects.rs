//! The effects an observer's script has in Narsor, through the routes its
//! run token opens (R15, R19):
//!
//! ```text
//! create task   no create_task effect → refused
//!               over limit            → refused; observer needs_attention
//!               otherwise             → create_task (random id) → created
//! ```
//!
//! The server does not deduplicate effects: every call creates a task. The
//! script keeps track in its state of what it already reported.
//!
//! Every effect runs under the observer's lock, so it is serialized with
//! the other effects of the run and with the run's result: an effect never
//! lands after the run ended.

use chrono::{DateTime, Utc};

use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::ProtocolService;
use crate::controller_protocol::wire::*;
use crate::domain::TaskId;
use crate::observers::{
    ActiveRun, ObserverEffect, ObserverError, ObserverLock, ObserverRecord, ObserverState, RunId,
};
use crate::orchestration::TaskProvenance;

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
