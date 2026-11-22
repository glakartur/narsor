//! The details of an execution that only its task's storage holds, as a
//! runner or a read-only runner answers a relayed details read with them.

use narsor_protocol::api::{ActiveProblem, ConductorActivity};
use narsor_protocol::controller::ExecutionDetails;

use crate::conductor::{
    active_failure_problem, annotation_of, ConductorStore, FailureReport, PendingInvocation,
    QueuedRequest,
};
use crate::domain::{
    ConductorPurpose, ConductorTrigger, ExecutionStatus, ProblemKind, WorkflowExecution,
};
use crate::engine::{step_failure, PendingHumanInput, StepFailure};
use crate::persistence::{ExecutionStore, PersistenceError};

/// The context variables of `execution_id`, the conductor's summary of the
/// input it waits on, what the conductor works on and the problem a failed
/// step leaves the owner, read from `store`.
///
/// Only the execution record must load; a part that does not is left out,
/// as the owner's reads always did.
pub fn execution_details(
    store: &ExecutionStore,
    execution_id: &str,
) -> Result<ExecutionDetails, PersistenceError> {
    let execution = store.load_execution(execution_id)?;
    let variables = store
        .load_context(execution_id)
        .map(|context| context.variables)
        .unwrap_or_else(|_| execution.context.variables.clone());
    let pending = if execution.status == ExecutionStatus::InputNeeded {
        store
            .load_pending_input::<PendingHumanInput>(execution_id)
            .unwrap_or(None)
    } else {
        None
    };
    let annotation = pending
        .as_ref()
        .and_then(|pending| annotation_of(store, execution_id, pending))
        .map(Into::into);
    // A pending input's problem is an output problem, in the task record
    // already; a failed step stays `Failed` (KTD4) with the conductor's
    // report as its problem.
    let problem = match pending {
        Some(_) => None,
        None => active_failure_problem(store, &execution)
            .as_ref()
            .map(problem_of_failure),
    };
    Ok(ExecutionDetails {
        variables: variables.into_iter().collect(),
        problem,
        annotation,
        conductor: conductor_activity(store, &execution),
    })
}

/// What the conductor of `execution` is working on (R11): the invocation
/// under way, else the first request waiting its turn, else the diagnosis
/// a step failure still owes — the engine records the failure before the
/// conductor's own marker is written, so a read in between still shows that
/// the conductor takes it up. `None` when it has nothing to do.
fn conductor_activity(
    store: &ExecutionStore,
    execution: &WorkflowExecution,
) -> Option<ConductorActivity> {
    let conductor = ConductorStore::new(store.paths(), &execution.id);
    let state = if conductor.exists() {
        Some(conductor.load_state().ok()?)
    } else {
        None
    };
    if let Some(state) = &state {
        if let Some(pending) = &state.pending {
            return Some(activity_of_pending(pending));
        }
        if let Some(queued) = state.queue.first() {
            return Some(activity_of_queued(queued));
        }
    }
    let failure = step_failure(execution).ok().flatten()?;
    let concluded = state
        .as_ref()
        .and_then(|state| state.failure.as_ref())
        .is_some_and(|report| report.key == failure.key());
    (!concluded).then(|| activity_of_owed_diagnosis(&failure, execution))
}

fn activity_of_pending(pending: &PendingInvocation) -> ConductorActivity {
    ConductorActivity {
        purpose: pending.request.purpose.into(),
        trigger: pending.request.trigger.into(),
        episode: pending.request.episode.clone(),
        started_at: pending.started_at,
    }
}

fn activity_of_queued(queued: &QueuedRequest) -> ConductorActivity {
    ConductorActivity {
        purpose: queued.purpose.into(),
        trigger: queued.trigger.into(),
        episode: queued.episode.clone(),
        started_at: queued.queued_at,
    }
}

fn activity_of_owed_diagnosis(
    failure: &StepFailure,
    execution: &WorkflowExecution,
) -> ConductorActivity {
    ConductorActivity {
        purpose: ConductorPurpose::DiagnoseFailure.into(),
        trigger: ConductorTrigger::StepFailure.into(),
        episode: Some(failure.key().to_string()),
        started_at: execution.completed_at.unwrap_or_else(chrono::Utc::now),
    }
}

fn problem_of_failure(report: &FailureReport) -> ActiveProblem {
    ActiveProblem {
        kind: ProblemKind::Failure.into(),
        reason: report.reason.clone(),
        episode: Some(report.key.clone()),
    }
}
