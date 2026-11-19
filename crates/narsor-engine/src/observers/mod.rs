//! The runtime side of catalogue automations: the shell scripts of the
//! automations of a queue poll a source (Jira, GitHub, GitLab) on the
//! queue's schedule and create tasks or answer the decision gates of the
//! queue through `narsor-cli`. What a queue or an automation does is a
//! catalogue file ([`crate::automations`]); this module keeps what happens
//! at run time.
//!
//! * [`model`] — the runtime record of a (queue, automation) pair (with its
//!   run in progress), the execution errors of runs, the entries of
//!   queues, and the schedule and task-creation types the catalogue uses;
//! * [`schedule_expr`] — five-field cron expressions of schedules;
//! * [`store`] — the file layout under
//!   `{state}/observer-queues/`, and its locks;
//! * [`migrate`] — the startup warning about stored observers of older
//!   versions, which nothing reads;
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
    parse_script_output, ContractError, ScriptChoice, ScriptEntry, ScriptExit, ScriptInput,
    ScriptOutput, ScriptVariable, ENTRY_DONE_EXIT_CODE, MAX_SCRIPT_OUTPUT_BYTES,
    SCRIPT_CONTRACT_VERSION,
};
pub use migrate::stored_observers_warning;
pub use model::*;
pub use schedule_expr::{CronSchedule, MAX_CRON_LEN};
pub use store::{
    AutomationLock, EntryScan, EntryWrite, ObserverStore, QueueEntriesLock, KEPT_RUN_FAILURES,
};

use crate::persistence::PersistenceError;
use std::path::PathBuf;
use thiserror::Error;

/// Result alias of automation runtime operations.
pub type Result<T> = std::result::Result<T, ObserverError>;

/// Why an operation on the automations' runtime state was refused or failed.
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

    /// A value a catalogue file or a caller gave is not acceptable.
    #[error("invalid {field}: {reason}")]
    Invalid {
        /// What was refused.
        field: &'static str,
        /// Why.
        reason: String,
    },

    /// A stored record has a schema version this build does not understand.
    #[error("record '{what}' has unsupported version {version}")]
    UnsupportedRecordVersion {
        /// Which file.
        what: String,
        /// Its version.
        version: u32,
    },

    /// A record was handed to the store under another pair's lock, or a
    /// record names another pair than its file: a bug, or a file moved by
    /// hand.
    #[error("record of '{record}' used under the lock of '{lock}'")]
    LockMismatch {
        /// The pair the lock is for.
        lock: AutomationKey,
        /// The pair the record names.
        record: AutomationKey,
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
