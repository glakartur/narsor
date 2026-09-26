//! Filesystem persistence layer.
//!
//! Execution state, context and the append-only event history live on the
//! filesystem under a configurable root (default `./.narsor`):
//!
//! ```text
//! {root}/executions/{execution_id}/
//!     state.json                  -- the serialized WorkflowExecution
//!     context.json                -- projection of the execution context
//!     pending_input.json          -- the input request a paused execution awaits
//!     events/{sequence:08}.json   -- append-only event log
//!     artifacts/{artifact_id}/    -- files produced by actions
//!     conversations/{attempt_id}.jsonl        -- open attempt, see conversation
//!     conversations/{attempt_id}.jsonl.zst    -- closed attempt, compressed
//! ```
//!
//! Every write goes through [`atomic`]: a temporary file in the *same*
//! directory, `fsync`, then `rename`. A rename within a directory is atomic on
//! POSIX, so a reader never observes a half-written file and an interrupted
//! write leaves the previous version intact.

pub mod atomic;
pub mod conversation;
pub mod event_log;
pub mod execution_store;
pub mod paths;
pub mod recovery;

pub use atomic::{
    cleanup_orphaned_temp_files, read_json, write_atomic, write_atomic_exclusive, write_json_atomic,
};
pub use conversation::{
    ConversationEntry, ConversationSnapshot, ConversationSource, ConversationStore,
    ConversationWriter,
};
pub use event_log::EventLog;
pub use execution_store::{ExecutionStore, PENDING_INPUT_FILE};
pub use paths::{
    CatalogPaths, StatePaths, WorkPaths, CONTEXT_FILE, DEFAULT_ROOT, STATE_FILE, TEMP_SUFFIX,
};
pub use recovery::{
    compress_closed_conversations, reconcile, reconcile_all, recover, sweep_orphaned_workspaces,
    OrphanReason, OrphanedWorkspace, Reconciliation, ReconciliationOutcome, RecoveryReport,
};

use std::io;
use std::path::PathBuf;
use thiserror::Error;

/// Result alias for every fallible persistence operation.
pub type Result<T> = std::result::Result<T, PersistenceError>;

/// Errors produced by the persistence layer.
///
/// Every variant names the offending path, because a persistence failure is
/// almost always diagnosed by looking at a file on disk.
#[derive(Debug, Error)]
pub enum PersistenceError {
    /// A filesystem operation failed.
    #[error("I/O error on '{path}': {source}")]
    Io {
        /// The path the operation was performed on.
        path: PathBuf,
        /// The underlying OS error.
        #[source]
        source: io::Error,
    },

    /// A file could not be parsed as the expected JSON document.
    #[error("failed to deserialize '{path}': {source}")]
    Deserialize {
        /// The file that could not be parsed.
        path: PathBuf,
        /// The underlying serde error, including line and column.
        #[source]
        source: serde_json::Error,
    },

    /// A value could not be serialized to JSON.
    #[error("failed to serialize data for '{path}': {source}")]
    Serialize {
        /// The file the value was destined for.
        path: PathBuf,
        /// The underlying serde error.
        #[source]
        source: serde_json::Error,
    },

    /// No execution directory exists for the requested identifier.
    #[error("execution '{execution_id}' not found (expected '{path}')")]
    ExecutionNotFound {
        /// The requested execution identifier.
        execution_id: String,
        /// The path that was expected to exist.
        path: PathBuf,
    },

    /// An event with this sequence number is already on disk.
    ///
    /// This is never resolved by overwriting: it means the in-memory sequence
    /// counter drifted away from the log, and the existing file is the truth.
    #[error("event with sequence {sequence} already exists at '{path}'")]
    EventAlreadyExists {
        /// The sequence number that collided.
        sequence: u64,
        /// The event file that already exists.
        path: PathBuf,
    },

    /// The event log has a hole in its numbering.
    #[error("event log '{path}' is corrupt: expected sequence {expected}, found {found}")]
    SequenceGap {
        /// The event log directory.
        path: PathBuf,
        /// The sequence number that should have come next.
        expected: u64,
        /// The sequence number actually found.
        found: u64,
    },

    /// An event file's own `sequence` field disagrees with its file name.
    #[error("event file '{path}' is named for sequence {expected} but declares {declared}")]
    SequenceMismatch {
        /// The offending event file.
        path: PathBuf,
        /// The sequence number derived from the file name.
        expected: u64,
        /// The sequence number stored inside the file.
        declared: u64,
    },
}

impl PersistenceError {
    /// Wraps an [`io::Error`] together with the path it happened on.
    pub(crate) fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
