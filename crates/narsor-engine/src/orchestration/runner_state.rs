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
    GateAutomation, GateAutomationPhase, GateChange, GateExit, GateExitAuthor, GateHookOutcome,
    GateHookStatus, GateScripts, HeldExitItem, InputOutcome, InputRequest, InputStatus,
    InputUpdate, OperatorCommandStatus, QueuedInput, RunnerObservation, RunnerReportOutcome,
    RunnerSnapshot, RunnerState, TaskEventKind, TaskProvenance, TaskRecord, WatchedGate,
    GATE_HOOK_RETRY_DELAYS_SECS,
};
use crate::automations::HookPhase;
use crate::domain::display::bounded_detail;
use crate::domain::{one_line_label, without_unsafe_chars, TaskId, GENERATED_TITLE_MAX_CHARS};
use crate::environment::backend::DEFAULT_SLOT_RELEASE_GRACE_SECS;
use chrono::{DateTime, Duration, Utc};
use narsor_protocol::api::{ConversationState, EventLink, Timeline};
use narsor_protocol::controller::ResumeMessage;

/// For a queue, the automations whose entry (`in`) and exit (`out`)
/// scripts run for an episode that enters it, as the catalogue says now.
pub type EntryScripts<'a> = dyn Fn(&str) -> GateScripts + 'a;

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
            None,
        )
    }

    /// [`Self::record_runner_snapshot`] with the wait `grace` a transient
    /// execution state (input needed, failed) gets before the task's slot
    /// is freed, as the backend is configured now. The snapshot also sets
    /// or clears the task's slot-release deadline (D62, see
    /// [`refresh_slot_release_due`]).
    ///
    /// `entry_scripts` names, for a queue, the automations whose entry
    /// (`in`) and exit (`out`) scripts run for an episode: asked only for an
    /// episode the snapshot begins, so the episode's automation phase is
    /// written with the episode itself ([`WatchedGate::automation`]).
    /// Without it the new episode has no automation phase and counts as
    /// ready.
    pub fn record_runner_snapshot_with(
        &self,
        task_id: &TaskId,
        incarnation: &str,
        launch_generation: u64,
        mut snapshot: RunnerSnapshot,
        grace: Duration,
        entry_scripts: Option<&EntryScripts<'_>>,
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
        // The gate's step and queue reach the task's history and the
        // observers' scripts.
        if let Some(watch) = &mut snapshot.gate_watch {
            watch.step = without_unsafe_chars(&watch.step);
            watch.queue = without_unsafe_chars(&watch.queue);
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
        let (task, (applied, gate_change)) = self.mutate(task_id, |record, now| {
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
                        return Ok(Mutation::Unchanged((false, GateChange::default())));
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
            // nothing of a gate, and neither does one that reports a
            // problem: it sends its last execution id with no pending input,
            // and the episode must outlive the problem (R17). Any other
            // runner with an execution says where it waits, or that it
            // waits at no watched gate.
            let gate_change =
                if snapshot.execution_cursor.execution_id.is_some() && snapshot.problem.is_none() {
                    follow_watched_gate(record, &snapshot, now, entry_scripts)
                } else {
                    GateChange::default()
                };
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
            Ok(Mutation::Changed(kind, (true, gate_change)))
        })?;
        Ok(RunnerReportOutcome {
            task,
            applied,
            gate_change,
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
    /// to the same request is already queued or was taken by the runner
    /// (the first answer of an episode wins). A retry with the same key and
    /// answer returns the original input.
    pub fn queue_input(&self, task_id: &TaskId, request: InputRequest) -> Result<InputOutcome> {
        self.queue_input_from(task_id, request, None)
    }

    /// [`Self::queue_input`] for the answer of `observer`'s run (`task
    /// decide`, R15), or of the owner with `None`. The input and its
    /// [`TaskEventKind::InputQueued`] name the observer.
    pub fn queue_input_from(
        &self,
        task_id: &TaskId,
        request: InputRequest,
        observer: Option<TaskProvenance>,
    ) -> Result<InputOutcome> {
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
            // An answer the runner took already ended the episode; its next
            // snapshot says so (R16).
            if let Some(open) = record.inputs.iter().find(|input| {
                input.pending_input_id == request.pending_input_id
                    && (!input.status.is_settled() || input.status == InputStatus::Consumed)
            }) {
                return Err(OrchestrationError::InputAlreadyQueued {
                    task_id: record.task_id.clone(),
                    input_id: open.input_id.clone(),
                    pending_input_id: request.pending_input_id.clone(),
                });
            }

            let input_id = format!("input-{}", record.inputs.len() + 1);
            // With exit scripts the answer waits for them (R10); the first
            // exit of the episode wins.
            hold_gate_exit(
                record,
                Some(&request.pending_input_id),
                GateExit {
                    choice: delivery.chosen_label.clone(),
                    author: if observer.is_some() {
                        GateExitAuthor::Automation
                    } else {
                        GateExitAuthor::Person
                    },
                    run_id: observer.as_ref().map(|observer| observer.run_id.clone()),
                    held: Some(HeldExitItem::Input {
                        input_id: input_id.clone(),
                    }),
                    at: now,
                },
            )
            .map_err(|reason| OrchestrationError::GateExitHeld {
                task_id: record.task_id.clone(),
                episode: request.pending_input_id.clone(),
                reason,
            })?;
            let input = QueuedInput {
                input_id,
                idempotency_key: request.idempotency_key.clone(),
                pending_input_id: request.pending_input_id.clone(),
                status: InputStatus::Queued,
                delivery: Some(delivery.clone()),
                submitted_at: now,
                updated_at: now,
                detail: None,
                observer: observer.clone(),
            };
            record.inputs.push(input.clone());
            Ok(Mutation::Changed(
                TaskEventKind::InputQueued {
                    input_id: input.input_id.clone(),
                    pending_input_id: input.pending_input_id.clone(),
                    observer: observer.clone(),
                },
                (input, false),
            ))
        })?;
        Ok(InputOutcome { input, replayed })
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
            // The answer a gate holds waits until its exit scripts ended
            // (R10).
            let held = match record.held_exit_item() {
                Some(HeldExitItem::Input { input_id }) => Some(input_id.clone()),
                _ => None,
            };
            let mut deliverable = Vec::new();
            for input in record.inputs.iter_mut() {
                if Some(input.pending_input_id.as_str()) != pending_input_id
                    || held.as_ref() == Some(&input.input_id)
                {
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
    /// Writes a warning about the queue entry of the watched gate episode
    /// `gate` to the task's history (R9, R27): no observer can answer it,
    /// or it could not be stored. The gate keeps waiting for a person; the
    /// record changes only by the event. The gate's text came from the
    /// runner, so it is cleaned and bounded first.
    pub fn record_gate_queue_warning(
        &self,
        task_id: &TaskId,
        gate: &WatchedGate,
        reason: &str,
    ) -> Result<()> {
        let kind = TaskEventKind::GateQueueWarning {
            step: bounded_name(&gate.watch.step),
            queue: bounded_name(&gate.watch.queue),
            episode: bounded_name(&gate.episode),
            reason: bounded_detail(reason),
        };
        self.mutate(task_id, |_, _| Ok(Mutation::Changed(kind, ())))?;
        Ok(())
    }

    /// Records the end of one attempt of the `hook` script of `automation`
    /// in the watched gate episode `episode` (R12, R15): one event in the
    /// task's history, the script's status, and the gate's phase once no
    /// script of that kind is pending: `ready`, or `entry_failed` after a
    /// last failed `in`; `released` once every `out` ended, whatever its
    /// result.
    ///
    /// A failed attempt with attempts left waits
    /// [`GATE_HOOK_RETRY_DELAYS_SECS`] for the next one; after
    /// [`GateAutomation::max_attempts`] the script fails for good: no
    /// automatic attempt follows. An `out` of a task that ended at the gate
    /// starts the exit (author `task_end`, one attempt, R18). An episode the
    /// task left ([`TaskRecord::exiting_gates`]) is forgotten once
    /// released.
    ///
    /// Returns the gate's new phase when it changed. Changes nothing (and
    /// returns `None`) when the episode is not in the phase of the script
    /// (`entering` for `in`, `exiting` for `out`), or the script is not
    /// pending there: the attempt is of no use any more.
    pub fn record_gate_hook_attempt(
        &self,
        task_id: &TaskId,
        episode: &str,
        automation: &str,
        hook: HookPhase,
        outcome: &GateHookOutcome,
    ) -> Result<Option<GateAutomationPhase>> {
        let (_, changed) = self.mutate(task_id, |record, now| {
            let Some(gate) = gate_of_hook(record, episode, hook, now) else {
                return Ok(Mutation::Unchanged(None));
            };
            let (step, queue) = (
                bounded_name(&gate.watch.step),
                bounded_name(&gate.watch.queue),
            );
            let Some(state) = gate.automation.as_mut() else {
                return Ok(Mutation::Unchanged(None));
            };
            let max_attempts = state.max_attempts(hook);
            let Some(script) = state.hooks.iter_mut().find(|script| {
                script.automation == automation
                    && script.hook == hook
                    && script.status == GateHookStatus::Pending
            }) else {
                return Ok(Mutation::Unchanged(None));
            };
            let (mut error, mut skipped, mut retry_at) = (None, None, None);
            match outcome {
                GateHookOutcome::Succeeded => {
                    script.attempts += 1;
                    script.status = GateHookStatus::Succeeded;
                    script.next_attempt_at = None;
                }
                GateHookOutcome::Failed(reason) => {
                    script.attempts += 1;
                    let reason = bounded_detail(reason);
                    script.last_error = Some(reason.clone());
                    error = Some(reason);
                    match retry_delay(script.attempts, max_attempts) {
                        Some(delay) => {
                            let at = now + delay;
                            script.next_attempt_at = Some(at);
                            retry_at = Some(at);
                        }
                        None => {
                            script.status = GateHookStatus::Failed;
                            script.next_attempt_at = None;
                        }
                    }
                }
                GateHookOutcome::Skipped(reason) => {
                    script.status = GateHookStatus::Skipped;
                    script.next_attempt_at = None;
                    skipped = Some(bounded_detail(reason));
                }
            }
            let attempt = script.attempts;
            let gate_phase = match hook {
                HookPhase::Out => state.exit_outcome(),
                _ => state.entry_outcome(),
            };
            if let Some(phase) = gate_phase {
                state.phase = phase;
            }
            record.exiting_gates.retain(|gate| {
                gate.episode != episode
                    || gate
                        .automation
                        .as_ref()
                        .is_some_and(|state| state.phase == GateAutomationPhase::Exiting)
            });
            let kind = TaskEventKind::GateHookAttempt {
                step,
                queue,
                episode: bounded_name(episode),
                automation: bounded_name(automation),
                hook: hook.as_str().to_string(),
                attempt,
                error,
                skipped,
                retry_at,
                gate_phase,
            };
            Ok(Mutation::Changed(kind, gate_phase))
        })?;
        Ok(changed)
    }
}

/// The gate of `episode` whose `hook` script may record an attempt now:
/// for `in`, the watched gate while `entering`; for `out`, an episode the
/// task left with exit scripts, or the watched gate while `exiting`. A
/// task that ended at the gate with exit scripts not run yet starts its
/// exit here (author `task_end`, R18).
fn gate_of_hook<'a>(
    record: &'a mut TaskRecord,
    episode: &str,
    hook: HookPhase,
    now: DateTime<Utc>,
) -> Option<&'a mut WatchedGate> {
    let ended = record.has_ended();
    if hook == HookPhase::Out {
        if let Some(index) = record
            .exiting_gates
            .iter()
            .position(|gate| gate.episode == episode)
        {
            return record.exiting_gates.get_mut(index);
        }
    }
    let gate = record
        .watched_gate
        .as_mut()
        .filter(|gate| gate.episode == episode)?;
    let state = gate.automation.as_mut()?;
    let current = match (hook, state.phase) {
        (HookPhase::In, GateAutomationPhase::Entering)
        | (HookPhase::Out, GateAutomationPhase::Exiting) => true,
        (
            HookPhase::Out,
            GateAutomationPhase::Entering
            | GateAutomationPhase::Ready
            | GateAutomationPhase::EntryFailed,
        ) if ended && state.exit_pending() => {
            state.begin_exit(GateExit {
                choice: None,
                author: GateExitAuthor::TaskEnd,
                run_id: None,
                held: None,
                at: now,
            });
            true
        }
        _ => false,
    };
    current.then_some(gate)
}

/// Holds `exit` of the watched gate (of `episode`, when given) while its
/// exit scripts run (R10): the gate goes to `exiting`, or to `released`
/// at once when no exit script is left. Nothing is held at a gate whose
/// queue has no exit script (R17), nor at no watched gate.
///
/// A second exit of the episode is refused (the first wins): the reason is
/// returned. An exit whose item was never delivered (refused or made
/// obsolete meanwhile) gives way to the new one, which is then held as
/// long as the exit scripts still run.
pub(super) fn hold_gate_exit(
    record: &mut TaskRecord,
    episode: Option<&str>,
    exit: GateExit,
) -> std::result::Result<(), String> {
    let Some(gate) = record
        .watched_gate
        .as_ref()
        .filter(|gate| episode.is_none_or(|episode| gate.episode == episode))
    else {
        return Ok(());
    };
    if let Some(earlier) = gate
        .automation
        .as_ref()
        .and_then(|state| state.exit.as_ref())
        .filter(|earlier| exit_item_live(record, earlier))
    {
        let by = match &earlier.choice {
            Some(choice) => format!("'{choice}'"),
            None => "an exit".to_string(),
        };
        return Err(format!(
            "gate episode {} is already left by {by} of the {}; its exit scripts run, and the \
             first exit wins",
            gate.episode,
            earlier.author.as_str()
        ));
    }
    let Some(state) = record
        .watched_gate
        .as_mut()
        .and_then(|gate| gate.automation.as_mut())
        .filter(|state| state.hooks.iter().any(|hook| hook.hook == HookPhase::Out))
    else {
        return Ok(());
    };
    match state.phase {
        GateAutomationPhase::Entering
        | GateAutomationPhase::Ready
        | GateAutomationPhase::EntryFailed => state.begin_exit(exit),
        GateAutomationPhase::Exiting | GateAutomationPhase::Released => state.exit = Some(exit),
        GateAutomationPhase::Unrecognized => {}
    }
    Ok(())
}

/// Whether the item of `exit` still leaves the gate: it waits, it is on
/// its way to the runner, or the runner took it.
fn exit_item_live(record: &TaskRecord, exit: &GateExit) -> bool {
    match &exit.held {
        None => true,
        Some(HeldExitItem::Input { input_id }) => record.input(input_id).is_some_and(|input| {
            !input.status.is_settled() || input.status == InputStatus::Consumed
        }),
        Some(HeldExitItem::Command { command_id }) => {
            record.operator_command(command_id).is_some_and(|command| {
                !command.status.is_settled() || command.status == OperatorCommandStatus::Applied
            })
        }
    }
}

/// The wait before the attempt after `attempts` failed ones, `None` when
/// none follows (after `max_attempts`).
fn retry_delay(attempts: u32, max_attempts: u32) -> Option<Duration> {
    if attempts >= max_attempts {
        return None;
    }
    let index = usize::try_from(attempts.saturating_sub(1)).unwrap_or(usize::MAX);
    let secs = GATE_HOOK_RETRY_DELAYS_SECS
        .get(index)
        .or(GATE_HOOK_RETRY_DELAYS_SECS.last())
        .copied()
        .unwrap_or_default();
    Some(Duration::seconds(i64::try_from(secs).unwrap_or(i64::MAX)))
}

/// Keeps [`TaskRecord::watched_gate`] in step with a snapshot of a runner
/// with an execution, and says which watched episode ended and which
/// began. The episode is the id of the pending input request; a gate
/// without `watch` (or no gate) is no watched episode.
fn follow_watched_gate(
    record: &mut TaskRecord,
    snapshot: &RunnerSnapshot,
    now: DateTime<Utc>,
    entry_scripts: Option<&EntryScripts<'_>>,
) -> GateChange {
    let current = match (&snapshot.pending_input, &snapshot.gate_watch) {
        (Some(pending), Some(watch)) => Some(WatchedGate {
            episode: pending.id.clone(),
            watch: watch.clone(),
            began_at: Some(now),
            automation: None,
        }),
        _ => None,
    };
    let known = record.watched_gate.as_ref().map(|gate| &gate.episode);
    if known == current.as_ref().map(|gate| &gate.episode) {
        return GateChange::default();
    }
    // Only here, where the episode begins: a snapshot of the same episode
    // returned above, so a pause or a restart never runs `in` again (R19).
    let current = current.map(|mut gate| {
        gate.automation =
            entry_scripts.map(|scripts| GateAutomation::entering(scripts(&gate.watch.queue)));
        gate
    });
    let ended = std::mem::replace(&mut record.watched_gate, current.clone());
    // The runner left with exit scripts still to run: no exit was held
    // (or a held one was overtaken). They run now, after the fact, before
    // any entry script of the task's next episode (R10).
    if let Some(left) = left_with_exit_scripts(ended.clone(), now) {
        record.exiting_gates.push(left);
    }
    GateChange {
        ended,
        began: current,
    }
}

/// `gate`, which the runner left, when its exit scripts have not all
/// ended: in phase `exiting`, by the runner when no exit was held.
fn left_with_exit_scripts(gate: Option<WatchedGate>, now: DateTime<Utc>) -> Option<WatchedGate> {
    let mut gate = gate?;
    let state = gate.automation.as_mut()?;
    match state.phase {
        GateAutomationPhase::Exiting => {}
        GateAutomationPhase::Entering
        | GateAutomationPhase::Ready
        | GateAutomationPhase::EntryFailed
            if state.exit_pending() =>
        {
            state.begin_exit(GateExit {
                choice: None,
                author: GateExitAuthor::Runner,
                run_id: None,
                held: None,
                at: now,
            });
        }
        _ => return None,
    }
    Some(gate)
}

/// The longest step, queue or episode a [`TaskEventKind::GateQueueWarning`]
/// keeps, in characters.
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
