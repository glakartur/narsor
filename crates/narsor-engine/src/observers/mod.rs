//! Observers of external sources: named automations whose shell script
//! polls a source (Jira, GitHub, GitLab) on a schedule and creates tasks or
//! answers the decision gates of their queue through `narsor-cli`.
//!
//! * [`model`] — the observer record (with its run in progress), the
//!   execution errors of runs and the entries of observer queues, and the
//!   validation of what an owner may configure;
//! * [`schedule_expr`] — five-field cron expressions of schedules;
//! * [`store`] — the file layout under `{state}/observers/` and
//!   `{state}/observer-queues/`, and its locks;
//! * [`migrate`] — reading a record of an older version;
//! * [`contract`] — what a script reads on stdin and must print on stdout.
//!
//! The control plane owns the schedule and the effect routes; the worker
//! only runs scripts. Neither lives here: this module is plain data and its
//! durable storage.

pub mod contract;
pub mod migrate;
pub mod model;
pub mod schedule_expr;
pub mod store;

pub use contract::{
    parse_script_output, ContractError, ScriptChoice, ScriptEntry, ScriptInput, ScriptObserver,
    ScriptOutput, ScriptVariable, ENTRY_DONE_EXIT_CODE, MAX_SCRIPT_OUTPUT_BYTES,
    SCRIPT_CONTRACT_VERSION,
};
pub use model::*;
pub use schedule_expr::{CronSchedule, MAX_CRON_LEN};
pub use store::{
    EntryScan, EntryWrite, ObserverLock, ObserverStore, QueueEntriesLock, KEPT_RUN_FAILURES,
};

use crate::persistence::PersistenceError;
use std::path::PathBuf;
use thiserror::Error;

/// Result alias of observer operations.
pub type Result<T> = std::result::Result<T, ObserverError>;

/// Why an observer operation was refused or failed.
///
/// Every refusal leaves the stored files untouched: the store validates
/// before it writes.
#[derive(Debug, Error)]
pub enum ObserverError {
    /// The underlying file store failed.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),

    /// An advisory lock could not be taken.
    #[error("cannot lock '{path}': {source}")]
    Lock {
        /// The lock file.
        path: PathBuf,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// A value the owner or a caller gave is not acceptable.
    #[error("invalid {field}: {reason}")]
    Invalid {
        /// What was refused.
        field: &'static str,
        /// Why.
        reason: String,
    },

    /// No observer with this id exists.
    #[error("observer '{0}' does not exist")]
    NotFound(ObserverId),

    /// Another observer already has this name.
    #[error("an observer named '{0}' already exists")]
    NameTaken(ObserverName),

    /// Another observer of the owner's queue runs in another mode: every
    /// observer of a queue has the same mode (R26).
    #[error(
        "queue '{queue}' runs its observers {mode} (observer '{observer}'): every observer of a queue has the same mode"
    )]
    QueueModeMismatch {
        /// The queue.
        queue: String,
        /// The mode of the queue's observers.
        mode: ObserverMode,
        /// One observer of the queue in that mode.
        observer: ObserverName,
    },

    /// The workflow of a "create a task" effect is not in the catalogue.
    #[error("workflow '{0}' does not exist")]
    WorkflowNotFound(String),

    /// The observer has no such script version.
    #[error("observer '{observer_id}' has no script version {version}")]
    ScriptNotFound {
        /// The observer.
        observer_id: ObserverId,
        /// The version asked for.
        version: u32,
    },

    /// A script version on disk does not match the digest its record holds.
    #[error("script version {version} of observer '{observer_id}' does not match its digest")]
    ScriptCorrupt {
        /// The observer.
        observer_id: ObserverId,
        /// The version.
        version: u32,
    },

    /// A stored record has a schema version this build does not understand.
    #[error("record '{what}' has unsupported version {version}")]
    UnsupportedRecordVersion {
        /// Which file.
        what: String,
        /// Its version.
        version: u32,
    },

    /// A record was handed to the store under another observer's lock or
    /// with an id that is not its own: a bug of the caller.
    #[error("record of observer '{record}' used under the lock of observer '{lock}'")]
    LockMismatch {
        /// The observer the lock is for.
        lock: ObserverId,
        /// The observer the record names.
        record: ObserverId,
    },
}

impl ObserverError {
    /// An [`ObserverError::Invalid`] refusal.
    pub fn invalid(field: &'static str, reason: impl Into<String>) -> Self {
        Self::Invalid {
            field,
            reason: reason.into(),
        }
    }

    /// Whether this is a file that exists but cannot be parsed: no build
    /// can read it, so rewriting it loses nothing. A file of a newer
    /// version is not corrupt.
    pub fn is_corrupt_file(&self) -> bool {
        matches!(
            self,
            Self::Persistence(PersistenceError::Deserialize { .. })
        )
    }
}
