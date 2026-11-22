//! The owner's executions, as the task records hold them (U8).
//!
//! An execution is known to the control plane through its task: the runner
//! reports the execution id, status, cursor, pending input and timeline in
//! its snapshot. Listing and finding executions read those records only; no
//! storage is read and no read-only runner is started. The content of an
//! execution (events, conversation) is read through the task's relayed
//! reads.

use narsor_protocol::api::{
    ActiveProblem, ExecutionListResponse, ExecutionLookupResponse, ExecutionSummary,
    PendingHumanInputView,
};

use super::history::served_timeline;
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::ProtocolService;
use crate::domain::ProblemKind;
use crate::engine::{InputContinuation, PendingHumanInput};
use crate::orchestration::{OwnerRef, TaskRecord};

impl ProtocolService {
    /// The execution of every task of `owner` whose runner reported one.
    ///
    /// An execution id reported by more than one task is left out: a read
    /// by that id could not tell which task is meant.
    pub fn executions(&self, owner: &OwnerRef) -> ProtocolResult<ExecutionListResponse> {
        let records = self.owned_records(owner)?;
        let mut executions = Vec::new();
        for record in &records {
            let Some(id) = execution_id_of(record) else {
                continue;
            };
            let reported = records
                .iter()
                .filter(|other| execution_id_of(other) == Some(id))
                .count();
            if reported == 1 {
                executions.extend(execution_summary(record)?);
            }
        }
        Ok(ExecutionListResponse { executions })
    }

    /// The execution `execution_id` of one of `owner`'s tasks, and that task.
    ///
    /// `NotFound` for an execution no task of the owner reported — another
    /// owner's included, as for every task read.
    pub fn execution(
        &self,
        owner: &OwnerRef,
        execution_id: &str,
    ) -> ProtocolResult<ExecutionLookupResponse> {
        let mut matching = self
            .owned_records(owner)?
            .into_iter()
            .filter(|record| execution_id_of(record) == Some(execution_id));
        let record = matching.next().ok_or(ProtocolError::NotFound)?;
        if matching.next().is_some() {
            return Err(ProtocolError::conflict(
                "ambiguous_execution",
                format!("execution '{execution_id}' is reported by more than one task"),
            ));
        }
        let execution = execution_summary(&record)?.ok_or(ProtocolError::NotFound)?;
        Ok(ExecutionLookupResponse {
            task_id: record.task_id.to_string(),
            execution,
        })
    }

    /// Every task record of `owner`, in the control plane's order.
    fn owned_records(&self, owner: &OwnerRef) -> ProtocolResult<Vec<TaskRecord>> {
        Ok(self
            .control
            .list_tasks()?
            .into_iter()
            .filter(|record| record.owner == *owner)
            .collect())
    }
}

/// The execution id the task's runner reported last.
fn execution_id_of(record: &TaskRecord) -> Option<&str> {
    record
        .runner
        .as_ref()?
        .snapshot
        .execution_cursor
        .execution_id
        .as_deref()
}

/// The execution of `record` as its runner reported it last, `None` when it
/// reported none.
///
/// The start is that of the first step, else the task's creation; the end
/// that of the last step once the execution is terminal. What only the
/// task's storage holds — the context variables, the conductor's activity
/// and summary — is not part of it.
fn execution_summary(record: &TaskRecord) -> ProtocolResult<Option<ExecutionSummary>> {
    let Some(runner) = record.runner.as_ref() else {
        return Ok(None);
    };
    let snapshot = &runner.snapshot;
    let Some(id) = snapshot.execution_cursor.execution_id.clone() else {
        return Ok(None);
    };
    let timeline = served_timeline(record);
    let positions = timeline.as_ref().map(|t| t.positions.as_slice());
    let started_at = positions
        .and_then(|positions| positions.iter().find_map(|position| position.started_at))
        .unwrap_or(record.created_at);
    let completed_at = if snapshot.execution_status.is_terminal() {
        positions.and_then(|positions| {
            positions
                .iter()
                .filter_map(|position| position.ended_at)
                .max()
        })
    } else {
        None
    };
    let pending = snapshot.pending_input.as_ref().map(|pending| &pending.form);
    Ok(Some(ExecutionSummary {
        id,
        workflow_id: record.launch_spec.workflow_id.clone(),
        workflow_version: record.launch_spec.workflow_version,
        status: snapshot.execution_status,
        current_state: snapshot.execution_cursor.current_state.clone(),
        started_at,
        completed_at,
        variables: Default::default(),
        metadata: serde_json::Value::Null,
        pending_input: pending.map(pending_input_view).transpose()?,
        problem: pending.and_then(problem_of_pending),
        annotation: None,
        conductor: None,
    }))
}

/// The runner's record of a pending input in its client wire view.
fn pending_input_view(pending: &PendingHumanInput) -> ProtocolResult<PendingHumanInputView> {
    serde_json::to_value(pending)
        .and_then(serde_json::from_value)
        .map_err(|error| ProtocolError::Internal(format!("invalid pending-input view: {error}")))
}

/// The output problem a pending input asks the owner to resolve.
fn problem_of_pending(pending: &PendingHumanInput) -> Option<ActiveProblem> {
    if pending.continuation != InputContinuation::ResolveOutput {
        return None;
    }
    Some(ActiveProblem {
        kind: ProblemKind::Output.into(),
        reason: pending
            .problem
            .as_ref()
            .map(|problem| problem.reason.clone())
            .unwrap_or_else(|| pending.instruction().to_string()),
        episode: Some(pending.episode_key().to_string()),
    })
}
