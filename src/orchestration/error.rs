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
