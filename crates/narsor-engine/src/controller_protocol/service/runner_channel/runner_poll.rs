//! What a runner reads: its snapshot, its long-poll and the acknowledgements
//! of what it delivered.

use chrono::{DateTime, Utc};

use crate::controller_protocol::credentials::{RunnerIdentity, RunnerRole};
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::observers::gate_scripts;
use crate::controller_protocol::service::operator::protocol_delivery;
use crate::controller_protocol::service::{chrono_duration, ProtocolService};
use crate::controller_protocol::wire::*;
use crate::orchestration::{RunnerState, TaskRecord};

impl ProtocolService {
    /// Accepts a runner snapshot.
    pub fn runner_snapshot(
        &self,
        identity: &RunnerIdentity,
        snapshot: RunnerSnapshotRequest,
    ) -> ProtocolResult<RunnerSnapshotResponse> {
        Self::require_worker(identity)?;
        let snapshot =
            crate::orchestration::RunnerSnapshot::from_request(snapshot).map_err(|error| {
                ProtocolError::bad_request("invalid_runner_snapshot", error.to_string())
            })?;
        let current = self.control.task(&identity.task_id)?;
        let grace = self
            .slot_release_timings(&current.assignment.backend_id)
            .grace;
        // A runner at a safe point that a lowered limit asked for: the
        // control plane decides first whether the stop is still wanted, so
        // the acknowledgement below never lets compute go for nothing.
        if snapshot.safe_point
            && current.pause_requested_by_limit()
            && !current.supersedes_launch(identity.launch_generation)
        {
            let decision = self.control.decide_limit_stop(&identity.task_id, || {
                self.scheduling_policy(&current.assignment.backend_id)
            })?;
            if decision != crate::orchestration::scheduler::LimitStopDecision::NotApplicable {
                self.notify(&identity.task_id);
            }
        }
        // Before the session check, so a refusal there cannot leave the
        // queue entries behind the record.
        let outcome = self.record_runner_snapshot_of(
            &current,
            &identity.incarnation,
            identity.launch_generation,
            snapshot,
            chrono_duration(grace),
        )?;
        self.credentials
            .touch_runner(identity, None)?
            .ok_or(ProtocolError::Unauthorized)?;
        let mut task = outcome.task;
        if outcome.applied {
            self.notify(&identity.task_id);
            // A final execution state is due at once: the snapshot that
            // shows it frees the slot (D62). The answer carries the result.
            if self.release_slot_if_due(&task) {
                task = self.control.task(&identity.task_id)?;
            }
        }
        Ok(RunnerSnapshotResponse {
            applied: outcome.applied,
            directive: directive(task.runner_state),
            generation: task.generation,
            runner_state: task.runner_state,
            launch: launch_view(identity, &task),
            default_harness: self.default_harness_of(&task),
        })
    }

    /// Records a runner snapshot of `task_id` (see
    /// [`crate::orchestration::ControlPlane::record_runner_snapshot_with`])
    /// and follows the watched gate episodes it ended and began: a new
    /// episode gets its entry scripts from the catalogue as it is now, in
    /// the same write (R8, R17), and the queue entries follow the record —
    /// the ended episode's entry goes before the new one's is added (R16,
    /// KTD-5).
    ///
    /// The runner channel's snapshot route without its credential checks.
    pub fn record_runner_snapshot(
        &self,
        task_id: &crate::domain::TaskId,
        incarnation: &str,
        launch_generation: u64,
        snapshot: crate::orchestration::RunnerSnapshot,
        grace: chrono::Duration,
    ) -> Result<crate::orchestration::RunnerReportOutcome, crate::orchestration::OrchestrationError>
    {
        let known = self.control.task(task_id)?;
        self.record_runner_snapshot_of(&known, incarnation, launch_generation, snapshot, grace)
    }

    /// [`Self::record_runner_snapshot`] of the task `known`, as read just
    /// before. The catalogue is read once, before the task's write, when
    /// the snapshot begins a watched episode `known` does not have; should
    /// the task change meanwhile, the write reads it itself.
    fn record_runner_snapshot_of(
        &self,
        known: &TaskRecord,
        incarnation: &str,
        launch_generation: u64,
        snapshot: crate::orchestration::RunnerSnapshot,
        grace: chrono::Duration,
    ) -> Result<crate::orchestration::RunnerReportOutcome, crate::orchestration::OrchestrationError>
    {
        let known_episode = known
            .watched_gate
            .as_ref()
            .map(|gate| gate.episode.as_str());
        let begins_episode = snapshot.gate_watch.is_some()
            && snapshot
                .pending_input
                .as_ref()
                .is_some_and(|pending| Some(pending.id.as_str()) != known_episode);
        let catalog = begins_episode.then(|| self.automation_catalog());
        let entry_scripts = |queue: &str| match &catalog {
            Some(catalog) => gate_scripts(catalog, queue),
            None => gate_scripts(&self.automation_catalog(), queue),
        };
        let mut outcome = self.control.record_runner_snapshot_with(
            &known.task_id,
            incarnation,
            launch_generation,
            snapshot,
            grace,
            Some(&entry_scripts),
        )?;
        self.follow_gate_episode(&outcome.task, &outcome.gate_change, catalog.as_ref());
        if outcome.applied {
            if let Some(task) = self.schedule_usage_wait(&outcome.task) {
                outcome.task = task;
            }
        }
        Ok(outcome)
    }

    /// Plans the resume of a task whose snapshot just reported a wait for
    /// the harness's usage limit, with the backend's periods as configured
    /// now; the task as written, or `None` when nothing was planned.
    fn schedule_usage_wait(&self, task: &TaskRecord) -> Option<TaskRecord> {
        let reported = task
            .runner
            .as_ref()
            .is_some_and(|runner| runner.snapshot.usage_wait.is_some());
        if !reported || task.usage_wait.is_some() {
            return None;
        }
        let timings = self.slot_release_timings(&task.assignment.backend_id);
        match self.control.schedule_usage_wait(
            &task.task_id,
            chrono_duration(timings.usage_wait_retry),
            chrono_duration(timings.usage_wait_margin),
        ) {
            Ok(scheduled) => scheduled,
            Err(error) => {
                tracing::warn!(task = %task.task_id, %error, "usage wait: cannot plan the resume");
                None
            }
        }
    }

    /// The installation's default harness as it applies to the task of
    /// `record`, for its runner; `None` when there is none to follow.
    fn default_harness_of(&self, record: &TaskRecord) -> Option<TaskDefaultHarness> {
        self.launch.default_harness(&record.assignment.backend_id)
    }

    /// The runner's long-poll: answers as soon as there is an answer to
    /// deliver or the directive is anything but `Continue`.
    pub async fn runner_poll(
        &self,
        identity: &RunnerIdentity,
        query: RunnerPollQuery,
    ) -> ProtocolResult<RunnerPollResponse> {
        let session = self
            .credentials
            .touch_runner(identity, None)?
            .ok_or(ProtocolError::Unauthorized)?;
        let pending = query.pending_input_id.as_deref();
        let defer_materials = query.defer_materials;
        let reader = identity.role == RunnerRole::Reader;
        // The runner that gave the comment on the pause to its agent says
        // so; only then is the record rid of it.
        if let (false, Some(key)) = (reader, query.resume_message_taken.as_deref()) {
            if self.control.take_resume_message(&identity.task_id, key)? {
                self.notify(&identity.task_id);
            }
        }
        let poll = || -> ProtocolResult<RunnerPollResponse> {
            // A reader is handed nothing it would have to act on:
            // delivering an answer or a command to it would spend them.
            let (inputs, commands, materials) = if reader {
                (Vec::new(), Vec::new(), Vec::new())
            } else {
                let (inputs, changed) = self.control.deliver_inputs(
                    &identity.task_id,
                    &identity.incarnation,
                    identity.launch_generation,
                    pending,
                )?;
                let (commands, commands_changed) = self.control.deliver_operator_commands(
                    &identity.task_id,
                    &identity.incarnation,
                    identity.launch_generation,
                )?;
                if changed || commands_changed {
                    self.notify(&identity.task_id);
                }
                (
                    inputs,
                    commands,
                    self.material_announcements(&identity.task_id),
                )
            };
            let record = self.control.task(&identity.task_id)?;
            let now = self.control.now();
            Ok(RunnerPollResponse {
                directive: directive_for(identity, &record, now),
                generation: record.generation,
                runner_state: record.runner_state,
                inputs: inputs
                    .into_iter()
                    .filter_map(|input| {
                        let delivery = input.delivery?;
                        Some(DeliveredInput {
                            delivery: protocol_delivery(&delivery),
                            input_id: input.input_id,
                            pending_input_id: input.pending_input_id,
                            observer: input.observer,
                        })
                    })
                    .collect(),
                commands: commands
                    .into_iter()
                    .map(|queued| DeliveredOperatorCommand {
                        command_id: queued.command_id,
                        requested_by: queued.requested_by.0,
                        command: queued.command,
                    })
                    .collect(),
                credential_expires_at: session.expires_at,
                launch: launch_view(identity, &record),
                conversation_pages: self.lock_pages().requests_of(&identity.task_id),
                execution_events: self.lock_event_reads().requests_of(&identity.task_id),
                conductor_reads: self.lock_conductor_reads().requests_of(&identity.task_id),
                execution_details: self.lock_detail_reads().requests_of(&identity.task_id),
                execution_states: self.lock_state_reads().requests_of(&identity.task_id),
                materials,
                default_harness: self.default_harness_of(&record),
            })
        };
        // A change of the default while the runner waits is an answer too:
        // it should not wait out the long-poll to hear of it (U7). The
        // runner says which harness it settled on; one behind a default it
        // can switch to is answered at once. Without it (older runners) the
        // first look says which default the runner is waiting with.
        let runner_harness = query.harness;
        let behind_default = |default: &Option<TaskDefaultHarness>| {
            matches!(
                (default, runner_harness.as_deref()),
                (Some(TaskDefaultHarness::Available { harness }), Some(on)) if harness != on
            )
        };
        let mut asked_with = None;
        let wait = self.long_poll(query.wait_secs);
        let found = self
            .wait_for(wait, Some(&identity.task_id), || {
                let response = poll()?;
                let asked_with = asked_with.get_or_insert_with(|| response.default_harness.clone());
                let actionable = response.default_harness != *asked_with
                    || behind_default(&response.default_harness)
                    || !response.inputs.is_empty()
                    || !response.commands.is_empty()
                    || !response.conversation_pages.is_empty()
                    || !response.execution_events.is_empty()
                    || !response.conductor_reads.is_empty()
                    || !response.execution_details.is_empty()
                    || !response.execution_states.is_empty()
                    || (!defer_materials && !response.materials.is_empty())
                    || response.directive != RunnerDirective::Continue;
                Ok(actionable.then_some(response))
            })
            .await?;
        let response = match found {
            Some(response) => response,
            None => poll()?,
        };
        self.credentials.touch_runner(identity, None)?;
        Ok(response)
    }

    /// The runner's verdict on a delivered answer.
    pub fn runner_ack_input(
        &self,
        identity: &RunnerIdentity,
        input_id: &str,
        request: InputAckRequest,
    ) -> ProtocolResult<InputAckResponse> {
        Self::require_worker(identity)?;
        let (input, changed) = self.control.settle_input(
            &identity.task_id,
            &identity.incarnation,
            identity.launch_generation,
            input_id,
            request.accepted,
            request.detail,
        )?;
        if changed {
            self.notify(&identity.task_id);
        }
        Ok(InputAckResponse {
            input: InputView::from(&input),
        })
    }

    /// The runner's verdict on a delivered operator command.
    pub fn runner_ack_command(
        &self,
        identity: &RunnerIdentity,
        command_id: &str,
        request: OperatorCommandAckRequest,
    ) -> ProtocolResult<OperatorCommandAckResponse> {
        Self::require_worker(identity)?;
        let (command, changed) = self.control.settle_operator_command(
            &identity.task_id,
            &identity.incarnation,
            identity.launch_generation,
            command_id,
            request.applied,
            request.detail,
        )?;
        if changed {
            self.notify(&identity.task_id);
        }
        Ok(OperatorCommandAckResponse {
            command: OperatorCommandView::from(&command),
        })
    }
}

/// The calling runner's launch, as the record knows it.
fn launch_view(identity: &RunnerIdentity, record: &TaskRecord) -> RunnerLaunchView {
    // A reader is no launch: nothing authorized it to run work.
    let receipt = match identity.role {
        RunnerRole::Worker => record.launch_receipt(identity.launch_generation),
        RunnerRole::Reader => None,
    };
    RunnerLaunchView {
        incarnation: identity.incarnation.clone(),
        launch_generation: identity.launch_generation,
        authorized_by: receipt.map(|receipt| receipt.command),
        authorized_by_actor: receipt.map(|receipt| receipt.actor),
        resume_message: match identity.role {
            RunnerRole::Worker => record.resume_message.clone(),
            RunnerRole::Reader => None,
        },
    }
}

/// What the runner of `identity` should do now.
///
/// A working runner follows the task's state. A reader follows its lease
/// alone: it goes on while an owner is reading (`Continue`) and stops when
/// the lease ran out or the task is no longer one it reads — which
/// `directive(Stopped)` would say at once, to a runner that has just started.
fn directive_for(
    identity: &RunnerIdentity,
    record: &TaskRecord,
    now: DateTime<Utc>,
) -> RunnerDirective {
    match identity.role {
        RunnerRole::Worker => directive(record.runner_state),
        RunnerRole::Reader if record.reader_lease_active(now) => RunnerDirective::Continue,
        RunnerRole::Reader => RunnerDirective::Stop,
    }
}

/// What a runner should do while the task is in `runner_state`.
fn directive(runner_state: RunnerState) -> RunnerDirective {
    match runner_state {
        RunnerState::PauseRequested => RunnerDirective::PauseAtSafePoint,
        RunnerState::Stopped | RunnerState::Archiving | RunnerState::Archived => {
            RunnerDirective::Stop
        }
        RunnerState::Queued
        | RunnerState::Provisioning
        | RunnerState::Running
        | RunnerState::Interrupted
        | RunnerState::Unknown => RunnerDirective::Continue,
    }
}
