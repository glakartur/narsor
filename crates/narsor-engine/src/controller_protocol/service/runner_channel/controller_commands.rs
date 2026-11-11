//! The commands a controller long-polls for and acknowledges.

use chrono::{DateTime, Utc};

use crate::controller_protocol::credentials::ControllerIdentity;
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::ProtocolService;
use crate::controller_protocol::wire::*;
use crate::domain::{BackendId, TaskId};
use crate::orchestration::{RunnerState, TaskRecord};

impl ProtocolService {
    /// The backend's desired commands this controller has not acknowledged
    /// (or all of them), read under its unexpired lease.
    pub fn commands(
        &self,
        identity: &ControllerIdentity,
        all: bool,
    ) -> ProtocolResult<CommandsResponse> {
        self.commands_and_readers(identity, all)
            .map(|(response, _)| response)
    }

    /// [`Self::commands`], and the tasks a read-only runner is wanted for,
    /// from the same read of the records.
    fn commands_and_readers(
        &self,
        identity: &ControllerIdentity,
        all: bool,
    ) -> ProtocolResult<(CommandsResponse, std::collections::BTreeSet<TaskId>)> {
        let lease = self.require_lease(identity)?;
        // Decide admission before answering: the controller asking is the
        // proof its backend is online, and its limit is read fresh.
        self.reschedule(&identity.backend_id);
        let now = self.control.now();
        let acknowledged = self
            .credentials
            .controller(&identity.controller_id)?
            .ok_or(ProtocolError::Unauthorized)?
            .acknowledged;
        let tasks = self.control.list_tasks()?;
        let ours = || {
            tasks
                .iter()
                .filter(|record| record.assignment.backend_id == identity.backend_id)
        };
        let readers = ours()
            .filter(|record| record.reader_lease_active(now))
            .map(|record| record.task_id.clone())
            .collect();
        let commands = ours()
            .filter(|record| {
                all || record.generation
                    > acknowledged
                        .get(record.task_id.as_str())
                        .copied()
                        .unwrap_or(0)
            })
            // A queued task is offered only once the scheduler admitted it.
            .filter(|record| {
                record.runner_state != RunnerState::Queued || record.reservation.is_some()
            })
            .map(|record| desired_command(record, now))
            .collect::<ProtocolResult<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        let response = CommandsResponse {
            lease_epoch: lease.epoch,
            commands,
        };
        Ok((response, readers))
    }

    /// [`Self::commands`] as a long-poll: answers as soon as there is at
    /// least one command, or empty after `query.wait_secs`.
    pub async fn wait_for_commands(
        &self,
        identity: &ControllerIdentity,
        query: CommandsQuery,
    ) -> ProtocolResult<CommandsResponse> {
        let wait = self.long_poll(query.wait_secs);
        // A reader the control plane starts or stops asking for changes no
        // generation, so no acknowledged-generation filter would ever show
        // it: the controller is woken by the set of wanted readers changing.
        let readers_at_start = self.readers_wanted(&identity.backend_id)?;
        let found = self
            .wait_for(wait, None, || {
                let (response, readers) = self.commands_and_readers(identity, query.all)?;
                let readers_changed = readers != readers_at_start;
                Ok((!response.commands.is_empty() || readers_changed).then_some(response))
            })
            .await?;
        match found {
            Some(response) => Ok(response),
            None => self.commands(identity, query.all),
        }
    }

    /// The tasks of `backend_id` a read-only runner is wanted for now.
    fn readers_wanted(
        &self,
        backend_id: &BackendId,
    ) -> ProtocolResult<std::collections::BTreeSet<TaskId>> {
        let now = self.control.now();
        Ok(self
            .control
            .list_tasks()?
            .into_iter()
            .filter(|record| {
                record.assignment.backend_id == *backend_id && record.reader_lease_active(now)
            })
            .map(|record| record.task_id)
            .collect())
    }

    /// Records acknowledgements; duplicates and reorderings are harmless.
    pub fn acknowledge(
        &self,
        identity: &ControllerIdentity,
        request: AckRequest,
    ) -> ProtocolResult<AckResponse> {
        let mut acks = Vec::with_capacity(request.acks.len());
        for ack in &request.acks {
            let record = self.backend_task(identity, &ack.task_id)?;
            if ack.generation > record.generation {
                return Err(ProtocolError::bad_request(
                    "invalid_ack",
                    format!(
                        "task '{}' is at generation {}, cannot acknowledge {}",
                        record.task_id, record.generation, ack.generation
                    ),
                ));
            }
            acks.push((record.task_id, ack.generation));
        }
        let controller = self
            .credentials
            .acknowledge(&identity.controller_id, &acks)?
            .ok_or(ProtocolError::Unauthorized)?;
        Ok(AckResponse {
            acknowledged: acks
                .iter()
                .map(|(task_id, _)| CommandAck {
                    task_id: task_id.to_string(),
                    generation: controller
                        .acknowledged
                        .get(task_id.as_str())
                        .copied()
                        .unwrap_or(0),
                })
                .collect(),
        })
    }

    /// A task of the controller's backend, or `NotFound` (R37 scope).
    pub(super) fn backend_task(
        &self,
        identity: &ControllerIdentity,
        task_id: &str,
    ) -> ProtocolResult<TaskRecord> {
        let task_id = TaskId::parse(task_id).map_err(|_| ProtocolError::NotFound)?;
        let record = self.control.task(&task_id)?;
        if record.assignment.backend_id != identity.backend_id {
            return Err(ProtocolError::NotFound);
        }
        Ok(record)
    }
}

/// The desired state of one task as of `now`. `reader_needed` is the reader
/// lease as it stands now, so a lease that ran out stops being asked for
/// without waiting for the periodic pass to remove it.
fn desired_command(
    record: &TaskRecord,
    now: DateTime<Utc>,
) -> ProtocolResult<Option<DesiredCommand>> {
    let action = match record.runner_state {
        RunnerState::Queued => DesiredAction::Start,
        RunnerState::Provisioning => DesiredAction::Provision,
        RunnerState::Running => DesiredAction::Run,
        RunnerState::PauseRequested => DesiredAction::PauseAtSafePoint,
        RunnerState::Stopped => DesiredAction::KeepPaused,
        RunnerState::Interrupted | RunnerState::Unknown => DesiredAction::Hold,
        RunnerState::Archiving => DesiredAction::Archive,
        RunnerState::Archived => return Ok(None),
    };
    let launch_spec = serde_json::to_value(&record.launch_spec)
        .map_err(|error| ProtocolError::Internal(error.to_string()))?;
    Ok(Some(DesiredCommand {
        task_id: record.task_id.clone(),
        generation: record.generation,
        runner_state: record.runner_state,
        action,
        resource_key: record.assignment.resource_key.clone(),
        launch_spec,
        runner: record.runner.as_ref().map(|runner| RunnerProgress {
            incarnation: runner.incarnation.clone(),
            launch_generation: runner.launch_generation,
            execution_status: runner.snapshot.execution_status,
            event_cursor: runner.snapshot.event_cursor,
            safe_point_acknowledged: runner.safe_point_generation == Some(record.generation),
            received_at: runner.received_at,
        }),
        reader_needed: record.reader_lease_active(now),
    }))
}
