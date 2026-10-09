//! The runner's side of a task record: snapshots and owner input (KTD3,
//! KTD4).
//!
//! A runner never changes the orchestration lifecycle — only a lease-fenced
//! controller observation does. What it changes is the *last safe summary*
//! clients read ([`TaskRecord::runner`]) and the queue of owner answers
//! waiting for it ([`TaskRecord::inputs`]). Both go through the same
//! serialized mutation protocol as every other change, so each accepted
//! report or input is one durable event a client stream replays.
//!
//! Every method takes the runner's `launch_generation` and fences it against
//! the record *under the task lock*: a resume committed a moment earlier
//! supersedes the old launch for good, and no report of it slips in after.

use super::control_plane::{ControlPlane, Mutation};
use super::error::{OrchestrationError, Result};
use super::model::{
    InputOutcome, InputRequest, InputStatus, InputUpdate, QueuedInput, RunnerObservation,
    RunnerReportOutcome, RunnerSnapshot, TaskEventKind, TaskLifecycle, TaskRecord,
};
use crate::domain::{without_unsafe_chars, TaskId};
use crate::engine::{ConversationState, EventLink, Timeline};
use chrono::{DateTime, Utc};

impl ControlPlane {
    /// Accepts a runner snapshot from `incarnation`, launched at
    /// `launch_generation`.
    ///
    /// Refused when the launch was superseded or the task is archived, when
    /// the event cursor goes backwards (task storage outlives incarnations,
    /// so it never may), or when the snapshot is older than the last one of
    /// the same incarnation. An exact redelivery is absorbed with
    /// `applied: false`. Queued answers to a request the runner no longer
    /// waits on become obsolete without ever reaching it; answers already
    /// handed over are left alone — the runner settles those itself.
    pub fn record_runner_snapshot(
        &self,
        task_id: &TaskId,
        incarnation: &str,
        launch_generation: u64,
        mut snapshot: RunnerSnapshot,
    ) -> Result<RunnerReportOutcome> {
        // A remote runner's word is not trusted with a terminal: what the
        // attention carries is cleaned before it is stored or announced.
        for item in &mut snapshot.attention {
            item.state_id = item.state_id.as_deref().map(without_unsafe_chars);
            item.waiting_id = without_unsafe_chars(&item.waiting_id);
        }
        if let Some(timeline) = &mut snapshot.timeline {
            clean_timeline(timeline);
        }
        let (task, applied) = self.mutate(task_id, |record, now| {
            fence_runner(record, incarnation, launch_generation)?;
            if let Some(previous) = &record.runner {
                if previous.incarnation == incarnation
                    && previous.snapshot.snapshot_sequence == snapshot.snapshot_sequence
                {
                    if previous.snapshot == snapshot {
                        return Ok(Mutation::Unchanged(false));
                    }
                    return Err(stale_snapshot(record, previous, &snapshot));
                }
                let reordered = previous.incarnation == incarnation
                    && snapshot.snapshot_sequence < previous.snapshot.snapshot_sequence;
                if reordered || snapshot.event_cursor < previous.snapshot.event_cursor {
                    return Err(stale_snapshot(record, previous, &snapshot));
                }
            }

            let pending = snapshot.pending_input.as_ref().map(|p| p.id.clone());
            // Obsolete answers change in the same revision: the one event
            // says what the runner reported, and the input statuses that
            // follow from it are on the record that event points at.
            obsolete_undelivered(record, pending.as_deref(), now);
            let safe_point_generation = if snapshot.safe_point {
                if record.lifecycle == TaskLifecycle::PauseRequested {
                    Some(record.generation)
                } else {
                    record
                        .runner
                        .as_ref()
                        .filter(|previous| previous.incarnation == incarnation)
                        .and_then(|previous| previous.safe_point_generation)
                }
            } else {
                None
            };
            let kind = TaskEventKind::RunnerReported {
                incarnation: incarnation.to_string(),
                event_cursor: snapshot.event_cursor,
                execution_status: snapshot.execution_status.clone(),
                pending_input_id: pending,
                attention: snapshot.attention.clone(),
            };
            record.runner = Some(RunnerObservation {
                incarnation: incarnation.to_string(),
                launch_generation,
                snapshot,
                received_at: now,
                safe_point_generation,
            });
            Ok(Mutation::Changed(kind, true))
        })?;
        Ok(RunnerReportOutcome { task, applied })
    }

    /// Queues an owner answer for the request the runner waits on.
    ///
    /// Refused — without touching the record, let alone the runner — when
    /// the task cannot take input in its lifecycle, when the last runner
    /// snapshot waits on another request (or none), or when another answer
    /// to the same request is already queued. A retry with the same key and
    /// answer returns the original input.
    pub fn queue_input(&self, task_id: &TaskId, request: InputRequest) -> Result<InputOutcome> {
        // The answer is bound to the request it names, so the runner's
        // engine refuses it too should it ever land on a newer request.
        let mut delivery = request.delivery.clone();
        delivery.episode = Some(request.pending_input_id.clone());
        let (_, (input, replayed)) = self.mutate(task_id, |record, now| {
            if let Some(existing) = record
                .inputs
                .iter()
                .find(|input| input.idempotency_key == request.idempotency_key)
            {
                // A settled input no longer holds its payload; its key and
                // request still identify the retry.
                let same = existing.pending_input_id == request.pending_input_id
                    && (existing.delivery.is_none()
                        || existing.delivery.as_ref() == Some(&delivery));
                if !same {
                    return Err(OrchestrationError::InputKeyReused {
                        task_id: record.task_id.clone(),
                        key: request.idempotency_key.clone(),
                    });
                }
                return Ok(Mutation::Unchanged((existing.clone(), true)));
            }
            if !matches!(
                record.lifecycle,
                TaskLifecycle::Provisioning
                    | TaskLifecycle::Running
                    | TaskLifecycle::PauseRequested
            ) {
                return Err(OrchestrationError::TaskNotAcceptingInput {
                    task_id: record.task_id.clone(),
                    lifecycle: record.lifecycle,
                });
            }
            let pending = record.pending_input_id().map(str::to_string);
            if pending.as_deref() != Some(request.pending_input_id.as_str()) {
                return Err(OrchestrationError::ObsoleteInput {
                    task_id: record.task_id.clone(),
                    presented: request.pending_input_id.clone(),
                    pending,
                });
            }
            if let Some(open) = record.inputs.iter().find(|input| {
                input.pending_input_id == request.pending_input_id && !input.status.is_settled()
            }) {
                return Err(OrchestrationError::InputAlreadyQueued {
                    task_id: record.task_id.clone(),
                    input_id: open.input_id.clone(),
                    pending_input_id: request.pending_input_id.clone(),
                });
            }

            let input = QueuedInput {
                input_id: format!("input-{}", record.inputs.len() + 1),
                idempotency_key: request.idempotency_key.clone(),
                pending_input_id: request.pending_input_id.clone(),
                status: InputStatus::Queued,
                delivery: Some(delivery.clone()),
                submitted_at: now,
                updated_at: now,
                detail: None,
            };
            record.inputs.push(input.clone());
            Ok(Mutation::Changed(
                TaskEventKind::InputQueued {
                    input_id: input.input_id.clone(),
                    pending_input_id: input.pending_input_id.clone(),
                },
                (input, false),
            ))
        })?;
        Ok(InputOutcome { input, replayed })
    }

    /// The answers to hand a polling runner that waits on `pending_input_id`.
    ///
    /// Only answers to exactly that request are returned (queued or already
    /// delivered and not yet acknowledged — at-least-once); the runner's
    /// engine deduplicates by the request key. Queued answers to any other
    /// request are made obsolete instead: the runner says it moved on, and
    /// an answer to an old question must never reach a new one.
    ///
    /// Returns the answers and whether the record changed (so the caller
    /// knows whether there is a new event to announce).
    pub fn deliver_inputs(
        &self,
        task_id: &TaskId,
        incarnation: &str,
        launch_generation: u64,
        pending_input_id: Option<&str>,
    ) -> Result<(Vec<QueuedInput>, bool)> {
        let (_, delivered) = self.mutate(task_id, |record, now| {
            fence_runner(record, incarnation, launch_generation)?;
            let mut updates = obsolete_undelivered(record, pending_input_id, now);
            let mut deliverable = Vec::new();
            for input in record.inputs.iter_mut() {
                if Some(input.pending_input_id.as_str()) != pending_input_id {
                    continue;
                }
                match input.status {
                    InputStatus::Queued => {
                        input.status = InputStatus::Delivered;
                        input.updated_at = now;
                        updates.push(InputUpdate {
                            input_id: input.input_id.clone(),
                            status: InputStatus::Delivered,
                        });
                        deliverable.push(input.clone());
                    }
                    InputStatus::Delivered => deliverable.push(input.clone()),
                    _ => {}
                }
            }
            if updates.is_empty() {
                Ok(Mutation::Unchanged((deliverable, false)))
            } else {
                Ok(Mutation::Changed(
                    TaskEventKind::InputsUpdated { updates },
                    (deliverable, true),
                ))
            }
        })?;
        Ok(delivered)
    }

    /// The runner's verdict on a delivered answer. Repeating the same
    /// verdict is harmless (`changed: false`); contradicting a settled one
    /// is refused.
    pub fn settle_input(
        &self,
        task_id: &TaskId,
        incarnation: &str,
        launch_generation: u64,
        input_id: &str,
        accepted: bool,
        detail: Option<String>,
    ) -> Result<(QueuedInput, bool)> {
        let (_, settled) = self.mutate(task_id, |record, now| {
            fence_runner(record, incarnation, launch_generation)?;
            let target = if accepted {
                InputStatus::Consumed
            } else {
                InputStatus::Rejected
            };
            let task_id = record.task_id.clone();
            let input = record
                .inputs
                .iter_mut()
                .find(|input| input.input_id == input_id)
                .ok_or_else(|| OrchestrationError::InputNotFound {
                    task_id: task_id.clone(),
                    input_id: input_id.to_string(),
                })?;
            if input.status == target {
                return Ok(Mutation::Unchanged((input.clone(), false)));
            }
            if input.status != InputStatus::Delivered {
                return Err(OrchestrationError::InputAlreadySettled {
                    task_id,
                    input_id: input_id.to_string(),
                    status: input.status,
                });
            }
            input.status = target;
            input.updated_at = now;
            input.detail = detail.clone();
            input.delivery = None;
            let settled = input.clone();
            Ok(Mutation::Changed(
                TaskEventKind::InputsUpdated {
                    updates: vec![InputUpdate {
                        input_id: settled.input_id.clone(),
                        status: target,
                    }],
                },
                (settled, true),
            ))
        })?;
        Ok(settled)
    }
}

/// Refuses a runner whose launch is no longer current.
pub(super) fn fence_runner(
    record: &TaskRecord,
    incarnation: &str,
    launch_generation: u64,
) -> Result<()> {
    if record.lifecycle == TaskLifecycle::Archived || record.supersedes_launch(launch_generation) {
        return Err(OrchestrationError::RunnerSuperseded {
            task_id: record.task_id.clone(),
            incarnation: incarnation.to_string(),
        });
    }
    Ok(())
}

/// Marks every still-queued answer to a request other than `pending` as
/// obsolete and drops its payload. Delivered answers are left to the
/// runner's acknowledgement.
fn obsolete_undelivered(
    record: &mut TaskRecord,
    pending: Option<&str>,
    now: DateTime<Utc>,
) -> Vec<InputUpdate> {
    let mut updates = Vec::new();
    for input in record.inputs.iter_mut() {
        if input.status == InputStatus::Queued && Some(input.pending_input_id.as_str()) != pending {
            input.status = InputStatus::Obsolete;
            input.updated_at = now;
            input.delivery = None;
            updates.push(InputUpdate {
                input_id: input.input_id.clone(),
                status: InputStatus::Obsolete,
            });
        }
    }
    updates
}

fn stale_snapshot(
    record: &TaskRecord,
    previous: &RunnerObservation,
    incoming: &RunnerSnapshot,
) -> OrchestrationError {
    OrchestrationError::StaleRunnerSnapshot {
        task_id: record.task_id.clone(),
        accepted: format!(
            "event cursor {} / sequence {}",
            previous.snapshot.event_cursor, previous.snapshot.snapshot_sequence
        ),
        presented: format!(
            "event cursor {} / sequence {}",
            incoming.event_cursor, incoming.snapshot_sequence
        ),
    }
}

/// Strips terminal-unsafe characters from every text of a runner's timeline.
/// Labels and the links that name them are cleaned alike, so they still
/// match each other.
fn clean_timeline(timeline: &mut Timeline) {
    let clean = |text: &mut String| *text = without_unsafe_chars(text);
    let clean_opt = |text: &mut Option<String>| {
        if let Some(text) = text {
            *text = without_unsafe_chars(text);
        }
    };
    for position in &mut timeline.positions {
        clean(&mut position.label);
        clean(&mut position.state_id);
        clean_opt(&mut position.action_execution_id);
        clean_opt(&mut position.action_type);
        clean_opt(&mut position.error);
        clean_opt(&mut position.resumed_harness_session);
        if let ConversationState::NotCreated { reason } = &mut position.conversation {
            clean(reason);
        }
    }
    clean_opt(&mut timeline.default_position);
    for link in timeline.links.values_mut() {
        match link {
            EventLink::Position(label) => clean(label),
            EventLink::NoLink { reason } => clean(reason),
        }
    }
}
