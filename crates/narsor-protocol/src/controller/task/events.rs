//! Durable task events and the control-plane event stream.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::api::ExecutionStatusDto;
use crate::controller::{
    AttentionItem, InputStatus, Observation, OperatorCommand, OperatorCommandStatus, OwnerCommand,
    OwnerRef, QueueReason, RunnerState,
};

/// Change recorded in one accepted task mutation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum TaskEventKind {
    Created,
    OwnerCommandAccepted {
        command: OwnerCommand,
        idempotency_key: String,
    },
    ControllerReported {
        observation: Observation,
        lease_epoch: u64,
    },
    RunnerReported {
        incarnation: String,
        event_cursor: u64,
        execution_status: ExecutionStatusDto,
        pending_input_id: Option<String>,
        #[serde(default)]
        attention: Vec<AttentionItem>,
    },
    InputQueued {
        input_id: String,
        pending_input_id: String,
        /// The observer and its run, when an observer's script gave the
        /// answer (`task decide`, R15); `None` for the owner's.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observer: Option<TaskProvenance>,
    },
    /// The owner commented on a paused task; the comment waits for the
    /// runner of the next launch.
    ResumeMessageStored {
        idempotency_key: String,
    },
    /// The runner of a launch gave the comment on the pause to its agent.
    ResumeMessageTaken {
        idempotency_key: String,
    },
    OperatorCommandQueued {
        command_id: String,
        command: OperatorCommand,
        requested_by: OwnerRef,
    },
    OperatorCommandResolved {
        command_id: String,
        status: OperatorCommandStatus,
        detail: Option<String>,
    },
    PriorityChanged {
        priority: i32,
    },
    /// The owner moved the task within its backend's queue.
    Moved {
        /// The task's new 1-based position among the backend's tasks that
        /// are not archived.
        rank: u32,
        /// The new order of the mover's tasks on the backend, archived ones
        /// left out.
        order: Vec<crate::identifier::TaskId>,
    },
    QueueChanged {
        reason: QueueReason,
    },
    InputsUpdated {
        updates: Vec<InputUpdate>,
    },
    MaterialChanged {
        material_id: String,
        change: MaterialChange,
        files: u32,
        total_bytes: u64,
    },
    AutoResumed {
        attempt: u32,
        limit: u32,
        #[serde(default)]
        loss_reason: Option<String>,
    },
    AutoResumeExhausted {
        attempts: u32,
        reason: String,
    },
    /// The control plane planned (or moved) the time at which it frees the
    /// slot of a task that waits for its owner.
    SlotReleaseScheduled {
        due_at: DateTime<Utc>,
    },
    /// The control plane freed the task's slot: it asked the runner to stop
    /// at a safe point.
    SlotReleased {
        reason: SlotReleaseReason,
    },
    /// A lowered limit asked this task to stop, and the control plane
    /// took the request back at the safe point: the overshoot was gone.
    LimitStopWithdrawn,
    /// An owner is reading a stopped task: the control plane asked for a
    /// read-only runner until `until` (D63).
    ReaderLeased {
        until: DateTime<Utc>,
    },
    /// The read-only runner's lease ended, because it ran idle: the control
    /// plane no longer asks for it.
    ReaderLeaseEnded,
    /// An observer created the task (R19): the first event of such a task,
    /// in place of [`TaskEventKind::Created`]. A client from before
    /// observers reads it as [`TaskEventKind::Unrecognized`].
    CreatedByObserver {
        /// The observer and its run.
        provenance: TaskProvenance,
    },
    /// An observer delivered an external event to the task (R4): recorded
    /// for each delivery, whether or not the task waits for it.
    ExternalEventReceived {
        /// The event type (`review.approved`, say).
        event_type: String,
        /// Where the event came from, as its sender named it (`gitlab`,
        /// say). Observers no longer have a source field of their own.
        source: String,
        /// The observer and its run (R19).
        provenance: TaskProvenance,
        /// The event's data, as the observer's script sent it. Untrusted:
        /// a client strips control characters before printing it.
        #[serde(default)]
        data: serde_json::Value,
        /// The input queued for the task's waiting `wait` step, when the
        /// event matched it; `None` when it only went to the history.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_id: Option<String>,
    },
    /// A warning: the task's `wait` step asked an observer to watch an
    /// object (its `watch` block), and the control plane added no watch
    /// entry. The task keeps waiting; the owner may add the entry by hand.
    /// A client from before watch entries reads it as
    /// [`TaskEventKind::Unrecognized`].
    WatchRefused {
        /// The workflow state whose `wait` asked for the entry.
        step: String,
        /// The observer it named.
        observer: String,
        /// The object it named, without unsafe characters and bounded.
        object: String,
        /// Why no entry was added.
        reason: String,
    },
    /// A warning about the queue entry of a wait episode at a `decision`
    /// gate with a `watch` block (R9, R27): no active observer of the queue
    /// can answer the gate, or the entry could not be stored. Written once
    /// per episode. The gate keeps waiting for a person. A client from
    /// before queue entries reads it as [`TaskEventKind::Unrecognized`].
    GateQueueWarning {
        /// The workflow state of the gate.
        step: String,
        /// The queue its `watch` names.
        queue: String,
        /// The wait episode: the id of the gate's pending input request.
        episode: String,
        /// What is wrong.
        reason: String,
    },
    /// One attempt of an entry (`in`) or exit (`out`) script of an
    /// automation at a watched gate ended (R12, R14). Every attempt is one
    /// event of the gate's step, never a new step. A client from before
    /// these scripts reads it as [`TaskEventKind::Unrecognized`].
    GateHookAttempt {
        /// The workflow state of the gate.
        step: String,
        /// The queue its `watch` names.
        queue: String,
        /// The wait episode.
        episode: String,
        /// The automation whose script ran.
        automation: String,
        /// The script phase: `in` or `out`.
        hook: String,
        /// The attempt, from 1.
        attempt: u32,
        /// Why it failed; `None` when it succeeded or was skipped.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// Why no script ran: the automation no longer has the script in
        /// the queue. `None` when the script ran.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        skipped: Option<String>,
        /// When the next automatic attempt runs; `None` when none follows.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_at: Option<DateTime<Utc>>,
        /// The gate's new automation phase, when this attempt changed it
        /// (`ready`, `entry_failed`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gate_phase: Option<GateAutomationPhase>,
    },
    /// The owner refreshed the catalogue-derived part of the task's
    /// environment: the launch spec now carries `new_identity` under a new
    /// generation. A client from before the refresh reads it as
    /// [`TaskEventKind::Unrecognized`].
    EnvironmentRefreshed {
        /// The owner who asked for the refresh, as the server authenticated
        /// them.
        requested_by: OwnerRef,
        /// The environment identity before the refresh.
        old_identity: String,
        /// The environment identity after the refresh.
        new_identity: String,
        /// What changed, part by part. Boxed: it is the largest payload
        /// of any event.
        #[serde(default)]
        diff: Box<EnvironmentDiff>,
    },
    /// The task's step stopped on its harness's exhausted usage limit: the
    /// control plane frees the slot and resumes the task at `resume_at`,
    /// which is `resets_at` plus a safety margin, or a retry interval after
    /// the report when the harness gave no reset time. A client from before
    /// the wait reads it as [`TaskEventKind::Unrecognized`].
    UsageWaitScheduled {
        /// The harness whose usage limit is used up.
        harness: String,
        /// When the harness said the limit resets, if it did.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resets_at: Option<DateTime<Utc>>,
        /// When the control plane resumes the task.
        resume_at: DateTime<Utc>,
    },
    /// The control plane resumed the task whose usage-limit wait ran out.
    /// A client from before the wait reads it as
    /// [`TaskEventKind::Unrecognized`].
    UsageWaitResumed,
    #[serde(other)]
    Unrecognized,
}

/// What an environment refresh changed, part by part (R9). Every part is
/// left out of the serialized form when it did not change, and a part this
/// version does not know is ignored by an older reader.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnvironmentDiff {
    /// The base image of the task image, as configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_image_base: Option<ValueChange>,
    /// The components of the task image, by name.
    #[serde(default, skip_serializing_if = "NamedChanges::is_empty")]
    pub task_image_components: NamedChanges,
    /// The image-level secrets of the task image, by token store name.
    #[serde(default, skip_serializing_if = "NamedChanges::is_empty")]
    pub task_image_secrets: NamedChanges,
    /// The secret references the controller injects, by name.
    #[serde(default, skip_serializing_if = "NamedChanges::is_empty")]
    pub secrets: NamedChanges,
    /// The base image of the main container, pinned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_image: Option<ValueChange>,
    /// The per-state image overrides, by state id.
    #[serde(default, skip_serializing_if = "NamedChanges::is_empty")]
    pub step_images: NamedChanges,
    /// The Narsor runtime bundle, pinned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_bundle: Option<ValueChange>,
    /// The tool packages, by tool name.
    #[serde(default, skip_serializing_if = "NamedChanges::is_empty")]
    pub tools: NamedChanges,
    /// The user profile, as `{id} v{version}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<ValueChange>,
}

impl EnvironmentDiff {
    /// How many single changes the diff holds: one per changed value and
    /// one per added, removed or changed name.
    pub fn change_count(&self) -> usize {
        let values = [
            &self.task_image_base,
            &self.base_image,
            &self.runtime_bundle,
            &self.profile,
        ]
        .into_iter()
        .filter(|change| change.is_some())
        .count();
        let names: usize = [
            &self.task_image_components,
            &self.task_image_secrets,
            &self.secrets,
            &self.step_images,
            &self.tools,
        ]
        .into_iter()
        .map(NamedChanges::len)
        .sum();
        values + names
    }

    /// Whether nothing changed.
    pub fn is_empty(&self) -> bool {
        self.change_count() == 0
    }
}

/// Entries of a named collection that were added, removed or changed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NamedChanges {
    /// Names only the new environment has.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added: Vec<String>,
    /// Names only the old environment had.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
    /// Names both have, with different content.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed: Vec<String>,
}

impl NamedChanges {
    /// How many names changed in any way.
    pub fn len(&self) -> usize {
        self.added.len() + self.removed.len() + self.changed.len()
    }

    /// Whether no name changed.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One value before and after a change; `None` where it was or is absent.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValueChange {
    /// The value before.
    #[serde(default)]
    pub before: Option<String>,
    /// The value after.
    #[serde(default)]
    pub after: Option<String>,
}

/// Where the automations of a watched gate's queue stand in one wait
/// episode: entry scripts (`in`), `check` runs, exit scripts (`out`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateAutomationPhase {
    /// Entry scripts run or wait for an attempt; no `check` run yet.
    Entering,
    /// Every entry script succeeded (or there is none): `check` runs.
    Ready,
    /// An entry script failed its last attempt: no `check` run and no
    /// more automatic attempts; the person chooses an exit.
    EntryFailed,
    /// An exit is held while exit scripts run.
    Exiting,
    /// Every exit script ended: the held exit goes to the runner.
    Released,
    /// A phase from a newer build.
    #[serde(other)]
    Unrecognized,
}

impl GateAutomationPhase {
    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Entering => "entering",
            Self::Ready => "ready",
            Self::EntryFailed => "entry_failed",
            Self::Exiting => "exiting",
            Self::Released => "released",
            Self::Unrecognized => "unrecognized",
        }
    }

    /// Whether the episode is not left yet: `entering`, `ready` or
    /// `entry_failed`.
    pub fn before_exit(self) -> bool {
        matches!(self, Self::Entering | Self::Ready | Self::EntryFailed)
    }
}

/// Where a task, an event or an answer came from when an observer caused it
/// (R15, R19).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskProvenance {
    /// The observer's id, never reused.
    pub observer_id: String,
    /// The observer's name when it acted.
    pub observer_name: String,
    /// The observer run that acted (`{observer_id}.{sequence}`).
    pub run_id: String,
}

/// Why the control plane freed a task's slot.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SlotReleaseReason {
    /// The task's execution ended (completed or cancelled).
    Terminal,
    /// The task waited for its owner (input needed, failed) and the wait
    /// ran out.
    GraceElapsed,
    /// The backend's limit of concurrent tasks went below the number of
    /// tasks that run, and this task was the first to reach a safe point.
    LimitLowered,
    /// The task waits for its harness's usage limit to reset: the slot is
    /// freed at once, and the task resumes at the planned time.
    UsageLimit,
    #[serde(other)]
    Unrecognized,
}

/// Material runner state change recorded in a task event.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MaterialChange {
    Uploaded,
    Published,
    Delivered,
    Deleted,
    Expired,
}

/// One queued input status change in a task event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputUpdate {
    pub input_id: String,
    pub status: InputStatus,
}

/// Durable task event, also used by the owner event stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskEvent {
    pub sequence: u64,
    pub task_id: crate::identifier::TaskId,
    pub at: DateTime<Utc>,
    #[serde(alias = "lifecycle")]
    pub runner_state: RunnerState,
    pub generation: u64,
    pub kind: TaskEventKind,
}

/// One frame of the control-plane event stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ControlEventEnvelope {
    pub cursor: String,
    pub event: TaskEvent,
}
