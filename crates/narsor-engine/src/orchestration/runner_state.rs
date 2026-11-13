//! The runner's side of a task record: snapshots and owner input (KTD3,
//! KTD4).
//!
//! A runner never changes the orchestration runner state — only a lease-fenced
//! controller observation does. What it changes is the *last safe summary*
//! clients read ([`TaskRecord::runner`]) and the queue of owner answers
//! waiting for it ([`TaskRecord::inputs`]). Both go through the same
//! serialized mutation protocol as every other change, so each accepted
//! report or input is one durable event a client stream replays.
//!
//! Every method takes the runner's `launch_generation` and fences it against
//! the record *under the task lock*: a resume committed a moment earlier
//! supersedes the old launch for good, and no report of it slips in after.

use super::control_plane::{refresh_slot_release_due, ControlPlane, Mutation};
use super::error::{OrchestrationError, Result};
use super::model::{
    EventInputKind, EventWatch, ExternalEvent, ExternalEventOutcome, InputOutcome, InputRequest,
    InputStatus, InputUpdate, QueuedEventInput, QueuedInput, RunnerObservation,
    RunnerReportOutcome, RunnerSnapshot, RunnerState, TaskEventKind, TaskRecord,
};
use super::resume::wait_timeout_key;
use crate::domain::display::bounded_detail;
use crate::domain::{one_line_label, without_unsafe_chars, TaskId, GENERATED_TITLE_MAX_CHARS};
use crate::engine::{ConversationState, EventLink, Timeline};
use crate::environment::backend::DEFAULT_SLOT_RELEASE_GRACE_SECS;
use chrono::{DateTime, Duration, Utc};
use narsor_protocol::controller::ResumeMessage;

/// How many keys of comments already taken a record remembers.
const RESUME_MESSAGES_REMEMBERED: usize = 32;

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
        snapshot: RunnerSnapshot,
    ) -> Result<RunnerReportOutcome> {
        self.record_runner_snapshot_with(
            task_id,
            incarnation,
            launch_generation,
            snapshot,
            Duration::seconds(i64::try_from(DEFAULT_SLOT_RELEASE_GRACE_SECS).unwrap_or(i64::MAX)),
        )
    }

    /// [`Self::record_runner_snapshot`] with the wait `grace` a transient
    /// execution state (input needed, failed) gets before the task's slot
    /// is freed, as the backend is configured now. The snapshot also sets
    /// or clears the task's slot-release deadline (D62, see
    /// [`refresh_slot_release_due`]).
    pub fn record_runner_snapshot_with(
        &self,
        task_id: &TaskId,
        incarnation: &str,
        launch_generation: u64,
        mut snapshot: RunnerSnapshot,
        grace: Duration,
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
        // Nor is its explanation of why it cannot work, or has no conductor.
        if let Some(problem) = &mut snapshot.problem {
            problem.code = without_unsafe_chars(&problem.code);
            problem.message = without_unsafe_chars(&problem.message);
        }
        if let Some(reason) = &mut snapshot.conductor_unavailable_reason {
            *reason = without_unsafe_chars(reason);
        }
        // Observers hand the episode to their scripts, and clients show the
        // event type.
        if let Some(wait) = &mut snapshot.event_wait {
            wait.episode_id = without_unsafe_chars(&wait.episode_id);
            wait.event_type = without_unsafe_chars(&wait.event_type);
        }
        if let Some(unavailable) = &mut snapshot.default_harness_unavailable {
            unavailable.harness = without_unsafe_chars(&unavailable.harness);
            unavailable.kept = without_unsafe_chars(&unavailable.kept);
            unavailable.reason = without_unsafe_chars(&unavailable.reason);
        }
        // The runner's title is bounded like its other free text.
        snapshot.generated_title = snapshot
            .generated_title
            .as_deref()
            .and_then(|title| one_line_label(title, GENERATED_TITLE_MAX_CHARS));
        let (task, (applied, began_wait)) = self.mutate(task_id, |record, now| {
            fence_runner(record, incarnation, launch_generation)?;
            // A report without a title does not take back one already shown.
            if snapshot.generated_title.is_none() {
                snapshot.generated_title = record
                    .runner
                    .as_ref()
                    .and_then(|previous| previous.snapshot.generated_title.clone());
            }
            if let Some(previous) = &record.runner {
                if previous.incarnation == incarnation
                    && previous.snapshot.snapshot_sequence == snapshot.snapshot_sequence
                {
                    if previous.snapshot == snapshot {
                        return Ok(Mutation::Unchanged((false, None)));
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
            // A runner without an execution yet (it is still starting) says
            // nothing of a wait; one with an execution says where it waits,
            // or that it waits for no event, and what was queued for any
            // other episode can never reach it.
            let mut began_wait = None;
            if snapshot.execution_cursor.execution_id.is_some() {
                let episode = snapshot
                    .event_wait
                    .as_ref()
                    .map(|wait| wait.episode_id.clone());
                obsolete_undelivered_events(record, episode.as_deref(), now);
                let known = record.event_wait.as_ref().map(|wait| &wait.episode_id);
                if episode.is_some() && episode.as_ref() != known {
                    began_wait = snapshot.event_wait.clone();
                }
                record.event_wait = snapshot.event_wait.clone();
            }
            let safe_point_generation = if snapshot.safe_point {
                if record.runner_state == RunnerState::PauseRequested {
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
                execution_status: snapshot.execution_status,
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
            refresh_slot_release_due(record, now, grace);
            Ok(Mutation::Changed(kind, (true, began_wait)))
        })?;
        Ok(RunnerReportOutcome {
            task,
            applied,
            began_wait,
        })
    }

    /// Queues an owner answer for the request the runner waits on.
    ///
    /// Also taken while the task is `Stopped` or `Queued` (waiting for a
    /// slot): the answer waits on the record for the next runner.
    ///
    /// Refused — without touching the record, let alone the runner — when
    /// the task cannot take input in its runner state, when the last runner
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
            // A paused task, or one resumed and waiting for a slot, keeps the
            // answer for the runner of its next start (R13, R14); the
            // service then resumes it. It still has to answer the request
            // the last snapshot shows.
            if !matches!(
                record.runner_state,
                RunnerState::Provisioning
                    | RunnerState::Running
                    | RunnerState::PauseRequested
                    | RunnerState::Stopped
                    | RunnerState::Queued
            ) {
                return Err(OrchestrationError::TaskNotAcceptingInput {
                    task_id: record.task_id.clone(),
                    runner_state: record.runner_state,
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

    /// Records an external event an observer delivers to the task (R4), once
    /// per idempotency key, under the task's lock: a retry, or a second
    /// call racing the first, finds the key and changes nothing (R13).
    ///
    /// The event always goes to the task's history, as one
    /// [`TaskEventKind::ExternalEventReceived`]. When the task waits in a
    /// `wait` step ([`TaskRecord::event_wait`]) for this type and the
    /// filter matches ([`super::EventWait::matches`]), the same mutation
    /// queues it for the runner as a [`QueuedEventInput`] of that episode;
    /// otherwise nothing else changes, so an event for a task that does
    /// not wait never changes its state, and a `wait` that starts later
    /// never sees it (AE7). An archived task takes no events.
    pub fn record_external_event(
        &self,
        task_id: &TaskId,
        event: ExternalEvent,
    ) -> Result<ExternalEventOutcome> {
        let (task, (input, replayed)) = self.mutate(task_id, |record, now| {
            if record
                .external_event_keys
                .iter()
                .any(|key| key == &event.idempotency_key)
            {
                let input = record
                    .event_inputs
                    .iter()
                    .find(|input| input.idempotency_key == event.idempotency_key)
                    .cloned();
                return Ok(Mutation::Unchanged((input, true)));
            }
            if record.runner_state.is_archival() {
                return Err(OrchestrationError::TaskNotAcceptingInput {
                    task_id: record.task_id.clone(),
                    runner_state: record.runner_state,
                });
            }
            let episode = record
                .event_wait
                .as_ref()
                .filter(|wait| wait.matches(&event.event_type, &event.data))
                .map(|wait| wait.episode_id.clone());
            let input = episode.map(|episode_id| QueuedEventInput {
                input_id: format!("event-{}", record.event_inputs.len() + 1),
                idempotency_key: event.idempotency_key.clone(),
                kind: EventInputKind::Event,
                episode_id,
                event_type: event.event_type.clone(),
                source: event.source.clone(),
                data: Some(event.data.clone()),
                provenance: Some(event.provenance.clone()),
                status: InputStatus::Queued,
                submitted_at: now,
                updated_at: now,
                detail: None,
            });
            if let Some(input) = &input {
                record.event_inputs.push(input.clone());
            }
            record
                .external_event_keys
                .push(event.idempotency_key.clone());
            Ok(Mutation::Changed(
                TaskEventKind::ExternalEventReceived {
                    event_type: event.event_type.clone(),
                    source: event.source.clone(),
                    provenance: event.provenance.clone(),
                    data: event.data.clone(),
                    input_id: input.as_ref().map(|input| input.input_id.clone()),
                },
                (input, false),
            ))
        })?;
        Ok(ExternalEventOutcome {
            task,
            input,
            replayed,
        })
    }

    /// Drops the comment on the pause from the record once the runner says it
    /// gave it to its agent (`key`), and remembers the key so a retry of the
    /// comment still reads as a replay. `true` when the record changed.
    pub fn take_resume_message(&self, task_id: &TaskId, key: &str) -> Result<bool> {
        let (_, changed) = self.mutate(task_id, |record, _| {
            if !record
                .resume_message
                .as_ref()
                .is_some_and(|message| message.key == key)
            {
                return Ok(Mutation::Unchanged(false));
            }
            record.resume_message = None;
            record.resume_message_taken.push(key.to_string());
            // A retry of a comment is a matter of minutes: only the latest
            // keys are kept, so the record does not grow with every comment.
            let excess = record
                .resume_message_taken
                .len()
                .saturating_sub(RESUME_MESSAGES_REMEMBERED);
            record.resume_message_taken.drain(..excess);
            Ok(Mutation::Changed(
                TaskEventKind::ResumeMessageTaken {
                    idempotency_key: key.to_string(),
                },
                true,
            ))
        })?;
        Ok(changed)
    }

    /// Keeps the owner's comment on a paused task for the runner of its next
    /// launch. The same key and text stored again change nothing and say so
    /// (`true`); a task that is not paused cannot take it, and one comment
    /// waits at a time, until a runner says it took it.
    pub fn store_resume_message(&self, task_id: &TaskId, key: &str, text: &str) -> Result<bool> {
        let (_, replayed) = self.mutate(task_id, |record, _| {
            if record.resume_message_taken.iter().any(|taken| taken == key) {
                return Ok(Mutation::Unchanged(true));
            }
            if let Some(message) = &record.resume_message {
                if message.key != key {
                    return Err(OrchestrationError::ResumeMessagePending {
                        task_id: record.task_id.clone(),
                    });
                }
                if message.text != text {
                    return Err(OrchestrationError::InputKeyReused {
                        task_id: record.task_id.clone(),
                        key: key.to_string(),
                    });
                }
                return Ok(Mutation::Unchanged(true));
            }
            if record.runner_state != RunnerState::Stopped {
                return Err(OrchestrationError::TaskNotAcceptingInput {
                    task_id: record.task_id.clone(),
                    runner_state: record.runner_state,
                });
            }
            record.resume_message = Some(ResumeMessage {
                key: key.to_string(),
                text: text.to_string(),
            });
            Ok(Mutation::Changed(
                TaskEventKind::ResumeMessageStored {
                    idempotency_key: key.to_string(),
                },
                false,
            ))
        })?;
        Ok(replayed)
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
        let detail = detail.map(|detail| bounded_detail(&detail));
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

impl ControlPlane {
    /// What ends the `wait` step a polling runner waits in
    /// (`event_wait_id`, its episode): the queued events and timeout for
    /// that episode (queued, or delivered and not yet acknowledged —
    /// at-least-once; the runner's engine deduplicates by the episode).
    /// Queued ones for any other episode become obsolete, as owner answers
    /// to another request do in [`Self::deliver_inputs`].
    ///
    /// Returns them and whether the record changed.
    pub fn deliver_event_inputs(
        &self,
        task_id: &TaskId,
        incarnation: &str,
        launch_generation: u64,
        event_wait_id: Option<&str>,
    ) -> Result<(Vec<QueuedEventInput>, bool)> {
        let (_, delivered) = self.mutate(task_id, |record, now| {
            fence_runner(record, incarnation, launch_generation)?;
            let mut updates = obsolete_undelivered_events(record, event_wait_id, now);
            let mut deliverable = Vec::new();
            for input in record.event_inputs.iter_mut() {
                if Some(input.episode_id.as_str()) != event_wait_id {
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

    /// The runner's verdict on a delivered event or timeout. Repeating the
    /// same verdict is harmless (`changed: false`); contradicting a settled
    /// one is refused. An accepted one ended the wait of its episode, so
    /// the task no longer waits for an event from the same mutation on:
    /// nothing more is queued for it, and no timeout is.
    pub fn settle_event_input(
        &self,
        task_id: &TaskId,
        incarnation: &str,
        launch_generation: u64,
        input_id: &str,
        accepted: bool,
        detail: Option<String>,
    ) -> Result<(QueuedEventInput, bool)> {
        let detail = detail.map(|detail| bounded_detail(&detail));
        let (_, settled) = self.mutate(task_id, |record, now| {
            fence_runner(record, incarnation, launch_generation)?;
            let target = if accepted {
                InputStatus::Consumed
            } else {
                InputStatus::Rejected
            };
            let task_id = record.task_id.clone();
            let input = record
                .event_inputs
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
            input.data = None;
            let settled = input.clone();
            if accepted
                && record
                    .event_wait
                    .as_ref()
                    .is_some_and(|wait| wait.episode_id == settled.episode_id)
            {
                record.event_wait = None;
            }
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

    /// Queues the end of the task's wait for its runner when the wait ran
    /// past its deadline (U4, PO4): once per episode, and not while an
    /// event for it is on its way or after one ended it. Returns the
    /// queued timeout — `None` when there is nothing to queue, the task
    /// being archived, waiting for no event, or not past its deadline.
    pub fn queue_wait_timeout(&self, task_id: &TaskId) -> Result<Option<QueuedEventInput>> {
        let (_, queued) = self.mutate(task_id, |record, now| {
            if record.runner_state.is_archival() {
                return Ok(Mutation::Unchanged(None));
            }
            let Some(wait) = record
                .event_wait
                .as_ref()
                .filter(|wait| wait.is_past_deadline(now))
            else {
                return Ok(Mutation::Unchanged(None));
            };
            let settled_or_due = record.event_inputs.iter().any(|input| {
                input.episode_id == wait.episode_id
                    && (input.kind == EventInputKind::Timeout
                        || !input.status.is_settled()
                        || input.status == InputStatus::Consumed)
            });
            if settled_or_due {
                return Ok(Mutation::Unchanged(None));
            }
            let input = QueuedEventInput {
                input_id: format!("event-{}", record.event_inputs.len() + 1),
                idempotency_key: wait_timeout_key(&wait.episode_id),
                kind: EventInputKind::Timeout,
                episode_id: wait.episode_id.clone(),
                event_type: wait.event_type.clone(),
                source: String::new(),
                data: None,
                provenance: None,
                status: InputStatus::Queued,
                submitted_at: now,
                updated_at: now,
                detail: None,
            };
            record.event_inputs.push(input.clone());
            Ok(Mutation::Changed(
                TaskEventKind::WaitTimedOut {
                    episode_id: input.episode_id.clone(),
                    input_id: input.input_id.clone(),
                },
                Some(input),
            ))
        })?;
        Ok(queued)
    }

    /// Writes the warning that the `watch` of the task's `wait` step got no
    /// watch entry (U5) to the task's history. The step keeps waiting; the
    /// record changes only by the event. What the runner asked for is
    /// untrusted text, so it is cleaned and bounded first.
    pub fn record_watch_refused(
        &self,
        task_id: &TaskId,
        watch: &EventWatch,
        reason: &str,
    ) -> Result<()> {
        let kind = TaskEventKind::WatchRefused {
            step: bounded_name(&watch.step),
            observer: bounded_name(&watch.observer),
            object: bounded_name(&watch.object),
            reason: bounded_detail(reason),
        };
        self.mutate(task_id, |_, _| Ok(Mutation::Changed(kind, ())))?;
        Ok(())
    }
}

/// The longest step, observer or object a [`TaskEventKind::WatchRefused`]
/// warning keeps, in characters.
const MAX_WATCH_WARNING_FIELD_CHARS: usize = 512;

/// One-line `text` without unsafe characters, cut to
/// [`MAX_WATCH_WARNING_FIELD_CHARS`] characters.
fn bounded_name(text: &str) -> String {
    without_unsafe_chars(text)
        .chars()
        .take(MAX_WATCH_WARNING_FIELD_CHARS)
        .collect()
}

/// Refuses a runner whose launch is no longer current.
pub(super) fn fence_runner(
    record: &TaskRecord,
    incarnation: &str,
    launch_generation: u64,
) -> Result<()> {
    if record.runner_state == RunnerState::Archived || record.supersedes_launch(launch_generation) {
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

/// Marks every still-queued event or timeout for another wait episode than
/// `episode` (none: the runner waits for no event) as obsolete and drops
/// its data. Delivered ones are left to the runner's acknowledgement.
fn obsolete_undelivered_events(
    record: &mut TaskRecord,
    episode: Option<&str>,
    now: DateTime<Utc>,
) -> Vec<InputUpdate> {
    let mut updates = Vec::new();
    for input in record.event_inputs.iter_mut() {
        if input.status == InputStatus::Queued && Some(input.episode_id.as_str()) != episode {
            input.status = InputStatus::Obsolete;
            input.updated_at = now;
            input.data = None;
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
        clean_opt(&mut position.harness);
        clean_opt(&mut position.model);
        clean_opt(&mut position.effort);
        clean_opt(&mut position.harness_session_id);
        position.attempts.iter_mut().for_each(clean);
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
