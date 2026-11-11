//! The normalized summary of a task.

use chrono::{DateTime, Utc};

use super::view::{
    manual_transitions, task_attention, task_title, task_workspaces, workflow_steps,
};
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::ProtocolService;
use crate::controller_protocol::wire::*;
use crate::orchestration::{RunnerObservation, RunnerState, TaskRecord};

impl ProtocolService {
    /// The normalized summary of a record.
    pub fn summarize(&self, record: &TaskRecord) -> ProtocolResult<TaskSummary> {
        let now = self.control.now();
        let controller = self.controller_presence(&record.assignment.backend_id, now)?;
        self.summarize_with(record, now, controller)
    }

    /// [`Self::summarize`] with the controller of the record's backend
    /// already looked up, as of `now`.
    pub(super) fn summarize_with(
        &self,
        record: &TaskRecord,
        now: DateTime<Utc>,
        controller: Option<ControllerPresence>,
    ) -> ProtocolResult<TaskSummary> {
        let controller_freshness = controller
            .as_ref()
            .map_or(Freshness::Stale, |presence| presence.freshness);
        let session = self.credentials.runner_session(&record.task_id)?;
        let runner = match &record.runner {
            Some(runner) => {
                let last_contact_at = session
                    .as_ref()
                    .filter(|session| session.incarnation == runner.incarnation)
                    .and_then(|session| session.last_contact_at);
                Some(RunnerView {
                    incarnation: runner.incarnation.clone(),
                    execution_status: runner.snapshot.execution_status,
                    execution_cursor: runner.snapshot.execution_cursor.clone(),
                    event_cursor: runner.snapshot.event_cursor,
                    pending_input: runner
                        .snapshot
                        .pending_input
                        .as_ref()
                        .map(pending_input_view)
                        .transpose()?,
                    safe_point: runner.snapshot.safe_point,
                    problem: runner.snapshot.problem.clone(),
                    conductor_available: runner.snapshot.conductor_available,
                    conductor_unavailable_reason: runner
                        .snapshot
                        .conductor_unavailable_reason
                        .clone(),
                    default_harness_unavailable: runner
                        .snapshot
                        .default_harness_unavailable
                        .clone(),
                    observed_at: runner.snapshot.observed_at,
                    received_at: runner.received_at,
                    last_contact_at,
                    freshness: self.runner_freshness(record, runner, now)?,
                })
            }
            None => None,
        };
        let mut summary = TaskSummary {
            task_id: record.task_id.clone(),
            backend_id: record.assignment.backend_id.clone(),
            runner_state: record.runner_state,
            state: record.task_state(),
            holds_slot: record.holds_capacity(),
            generation: record.generation,
            revision: record.revision,
            rank: record.rank,
            created_at: record.created_at,
            state_since: record.state_since,
            queue_sequence: record.queue_sequence,
            workflow_id: record.launch_spec.workflow_id.clone(),
            workflow_version: record.launch_spec.workflow_version,
            launch_digest: record.launch_spec.digest.clone(),
            environment_identity: record.launch_spec.environment.identity.clone(),
            archive: record.archive.clone(),
            observation: record.observed.as_ref().map(|observed| ObservationView {
                observation: observed.observation,
                summary: observed.summary.clone(),
                resource: observed.resource.clone(),
                observed_at: observed.observed_at,
                received_at: observed.received_at,
                freshness: controller_freshness,
            }),
            runner,
            controller,
            inputs: record.inputs.iter().map(InputView::from).collect(),
            operator_commands: record
                .operator_commands
                .iter()
                .map(OperatorCommandView::from)
                .collect(),
            attention: task_attention(record),
            queue: record.queue.as_ref().map(QueueView::from),
            workspaces: task_workspaces(record),
            ticket: record.launch_spec.ticket.clone(),
            title: task_title(record),
            manual_transitions: manual_transitions(record),
            slot_release_pending: record.slot_release_pending(),
            steps: workflow_steps(record),
            actions: None,
            panel: None,
            resume: record.resume.clone(),
            as_of: now,
        };
        // Last: the offer reads the rest of the summary.
        let mut actions = super::actions::offer(&summary);
        // One comment waits on a pause at a time: another is refused until a
        // runner takes it (`comment_pending`), so the offer does not promise it.
        if record.resume_message.is_some() {
            if let Some(comment) = actions
                .iter_mut()
                .find(|offer| offer.id == TaskActionId::Comment)
            {
                comment.available = false;
                comment.options = Vec::new();
                comment.effect = None;
                comment.reason =
                    Some("poprzedni komentarz czeka na wznowienie zadania".to_string());
            }
        }
        summary.actions = Some(actions);
        // After the actions: the panel reads the same summary and the offer.
        summary.panel = super::panel::offer_forms(&summary);
        Ok(summary)
    }

    /// Live only while the task expects a runner, the runner's launch is
    /// current, and it was heard from within the staleness window.
    pub(crate) fn runner_freshness(
        &self,
        record: &TaskRecord,
        runner: &RunnerObservation,
        now: DateTime<Utc>,
    ) -> ProtocolResult<Freshness> {
        let expects_runner = matches!(
            record.runner_state,
            RunnerState::Provisioning | RunnerState::Running | RunnerState::PauseRequested
        );
        if !expects_runner || record.supersedes_launch(runner.launch_generation) {
            return Ok(Freshness::Stale);
        }
        let last_contact = self
            .credentials
            .runner_session(&record.task_id)?
            .filter(|session| session.incarnation == runner.incarnation)
            .and_then(|session| session.last_contact_at);
        let heard = last_contact.map_or(runner.received_at, |contact| {
            contact.max(runner.received_at)
        });
        Ok(if now - heard <= self.config.runner_stale_after {
            Freshness::Live
        } else {
            Freshness::Stale
        })
    }
}

fn pending_input_view(
    pending: &crate::orchestration::PendingInputRef,
) -> ProtocolResult<PendingInputRefView> {
    let form = serde_json::to_value(&pending.form)
        .map_err(|error| ProtocolError::Internal(error.to_string()))?;
    Ok(PendingInputRefView {
        id: pending.id.clone(),
        attempt_id: pending.attempt_id.clone(),
        form,
    })
}
