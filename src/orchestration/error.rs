//! Errors of the orchestration control plane.

use super::model::{Observation, OwnerCommand, TaskLifecycle};
use crate::domain::{BackendId, InvalidIdentifier, TaskId};
use crate::persistence::PersistenceError;
use chrono::{DateTime, Utc};
use thiserror::Error;

/// Why an orchestration operation was refused or failed.
///
/// Every refusal leaves the stored record untouched: the mutation protocol
/// only writes after the operation succeeded.
#[derive(Debug, Error)]
pub enum OrchestrationError {
    /// The underlying file store failed.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),

    /// An identifier is not safe to use.
    #[error(transparent)]
    InvalidIdentifier(#[from] InvalidIdentifier),

    /// No task with this identifier exists.
    #[error("task '{0}' does not exist")]
    TaskNotFound(TaskId),

    /// A task with this identifier exists, created from a different request.
    #[error("task '{0}' already exists with a different launch specification, owner, backend or priority")]
    TaskConflict(TaskId),

    /// The launch specification does not match its own digest.
    #[error("launch specification of task '{0}' does not match its digest")]
    InvalidLaunchSpec(TaskId),

    /// A stored record has a schema version this build does not understand.
    #[error("record '{what}' has unsupported version {version}")]
    UnsupportedRecordVersion {
        /// What kind of record.
        what: String,
        /// Its version.
        version: u32,
    },

    /// An idempotency key was reused for a different command.
    #[error("idempotency key '{key}' of task '{task_id}' was already used for {original:?}, not {requested:?}")]
    IdempotencyKeyReused {
        /// The task.
        task_id: TaskId,
        /// The reused key.
        key: String,
        /// What it was first used for.
        original: OwnerCommand,
        /// What it was offered for now.
        requested: OwnerCommand,
    },

    /// The command or report targets an older or newer desired generation.
    #[error("task '{task_id}' is at desired generation {current}, not {presented}")]
    StaleGeneration {
        /// The task.
        task_id: TaskId,
        /// Its current generation.
        current: u64,
        /// The generation offered.
        presented: u64,
    },

    /// The owner command is not legal from the current lifecycle.
    #[error("task '{task_id}' cannot {command:?} while {lifecycle:?}")]
    IllegalCommand {
        /// The task.
        task_id: TaskId,
        /// The command.
        command: OwnerCommand,
        /// Its lifecycle.
        lifecycle: TaskLifecycle,
    },

    /// The observation contradicts the current lifecycle.
    #[error("task '{task_id}' cannot accept observation {observation:?} while {lifecycle:?}")]
    IllegalObservation {
        /// The task.
        task_id: TaskId,
        /// The observation.
        observation: Observation,
        /// Its lifecycle.
        lifecycle: TaskLifecycle,
    },

    /// The report's resource version is older than one already accepted.
    #[error("task '{task_id}': resource '{uid}' generation {presented} is older than accepted {accepted}")]
    StaleObservation {
        /// The task.
        task_id: TaskId,
        /// Resource incarnation.
        uid: String,
        /// Accepted resource generation.
        accepted: u64,
        /// Offered resource generation.
        presented: u64,
    },

    /// The controller reported a pause complete although the runner has not
    /// acknowledged a safe point for the current generation (KTD2).
    #[error("the runner of task '{task_id}' has not acknowledged a safe point for generation {generation}")]
    SafePointNotAcknowledged {
        /// The task.
        task_id: TaskId,
        /// The pause's desired generation.
        generation: u64,
    },

    /// The backend lease is held by another, unexpired controller.
    #[error("backend '{backend_id}' lease is held by '{holder}' until {expires_at}")]
    LeaseHeld {
        /// The backend.
        backend_id: BackendId,
        /// Current holder.
        holder: String,
        /// When its lease expires.
        expires_at: DateTime<Utc>,
    },

    /// The presented lease was superseded, never existed or names another
    /// controller.
    #[error("backend '{backend_id}' lease epoch {presented} of '{controller_id}' is fenced (current epoch {current:?})")]
    StaleLease {
        /// The backend.
        backend_id: BackendId,
        /// The controller presenting it.
        controller_id: String,
        /// The epoch presented.
        presented: u64,
        /// The current epoch, if any lease exists.
        current: Option<u64>,
    },

    /// The presented lease is current but expired; renew before reporting.
    #[error("backend '{backend_id}' lease epoch {epoch} expired at {expires_at}")]
    LeaseExpired {
        /// The backend.
        backend_id: BackendId,
        /// The epoch.
        epoch: u64,
        /// When it expired.
        expires_at: DateTime<Utc>,
    },

    /// The controller reported on a task assigned to another backend.
    #[error("task '{task_id}' is assigned to backend '{assigned}', not '{presented}'")]
    WrongBackend {
        /// The task.
        task_id: TaskId,
        /// Its backend.
        assigned: BackendId,
        /// The lease's backend.
        presented: BackendId,
    },

    /// The runner's launch was superseded by a newer start or resume, or the
    /// task is archived: its credential and reports are fenced.
    #[error("runner incarnation '{incarnation}' of task '{task_id}' is no longer current")]
    RunnerSuperseded {
        /// The task.
        task_id: TaskId,
        /// The fenced incarnation.
        incarnation: String,
    },

    /// A runner snapshot is older than one already accepted.
    #[error("task '{task_id}': runner snapshot {presented} is older than accepted {accepted}")]
    StaleRunnerSnapshot {
        /// The task.
        task_id: TaskId,
        /// What was accepted (`event cursor/sequence`).
        accepted: String,
        /// What was offered.
        presented: String,
    },

    /// The owner answered an input request the runner is not waiting on.
    #[error("task '{task_id}' is not waiting on input '{presented}' (waiting on {pending:?})")]
    ObsoleteInput {
        /// The task.
        task_id: TaskId,
        /// The request the answer named.
        presented: String,
        /// The request the runner last reported, if any.
        pending: Option<String>,
    },

    /// Another answer to the same request is already queued.
    #[error(
        "task '{task_id}' already has input '{input_id}' queued for request '{pending_input_id}'"
    )]
    InputAlreadyQueued {
        /// The task.
        task_id: TaskId,
        /// The queued input.
        input_id: String,
        /// The request both answer.
        pending_input_id: String,
    },

    /// An input idempotency key was reused for a different answer.
    #[error(
        "input idempotency key '{key}' of task '{task_id}' was already used for another answer"
    )]
    InputKeyReused {
        /// The task.
        task_id: TaskId,
        /// The reused key.
        key: String,
    },

    /// No queued input with this id exists.
    #[error("task '{task_id}' has no input '{input_id}'")]
    InputNotFound {
        /// The task.
        task_id: TaskId,
        /// The input id.
        input_id: String,
    },

    /// The input was already settled differently.
    #[error("input '{input_id}' of task '{task_id}' is already {status:?}")]
    InputAlreadySettled {
        /// The task.
        task_id: TaskId,
        /// The input id.
        input_id: String,
        /// Its settled status.
        status: super::model::InputStatus,
    },

    /// The task's lifecycle does not let a runner take input.
    #[error("task '{task_id}' does not accept input while {lifecycle:?}")]
    TaskNotAcceptingInput {
        /// The task.
        task_id: TaskId,
        /// Its lifecycle.
        lifecycle: TaskLifecycle,
    },

    /// The advisory lock could not be taken.
    #[error("cannot lock '{path}': {source}")]
    Lock {
        /// The lock file.
        path: std::path::PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
}

/// Result type of the orchestration module.
pub type Result<T> = std::result::Result<T, OrchestrationError>;
