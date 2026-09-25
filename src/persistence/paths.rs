//! Directory layout of the persistence root.
//!
//! All path construction lives here so that the layout described in
//! `DOMAIN_MODEL.md` §2.7 is defined exactly once.

use crate::domain::ExecutionId;
use std::path::{Path, PathBuf};

/// Default persistence root, relative to the process working directory.
pub const DEFAULT_ROOT: &str = "./.narsor";

/// Directory holding every execution, directly under the root.
pub const EXECUTIONS_DIR: &str = "executions";

/// File holding the serialized `WorkflowExecution`.
pub const STATE_FILE: &str = "state.json";

/// File holding the serialized `Context`.
pub const CONTEXT_FILE: &str = "context.json";

/// Directory holding the append-only event log of one execution.
pub const EVENTS_DIR: &str = "events";

/// Directory holding artifact files of one execution.
pub const ARTIFACTS_DIR: &str = "artifacts";

/// Suffix of a not-yet-renamed atomic write. Never a file to read from.
pub const TEMP_SUFFIX: &str = ".tmp";

/// Number of digits in an event file name (`00000001.json`).
const SEQUENCE_WIDTH: usize = 8;

/// Resolves every path of the persistence layout below a configurable root.
///
/// The root is configurable precisely so that tests can point it at a
/// temporary directory; nothing in this layer ever reads a global location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    root: PathBuf,
}

impl Paths {
    /// Creates a layout rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The persistence root itself.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `{root}/executions`
    pub fn executions_dir(&self) -> PathBuf {
        self.root.join(EXECUTIONS_DIR)
    }

    /// `{root}/executions/{execution_id}`
    pub fn execution_dir(&self, execution_id: &str) -> PathBuf {
        self.executions_dir().join(execution_id)
    }

    /// `{root}/executions/{execution_id}/state.json`
    pub fn state_file(&self, execution_id: &str) -> PathBuf {
        self.execution_dir(execution_id).join(STATE_FILE)
    }

    /// `{root}/executions/{execution_id}/context.json`
    pub fn context_file(&self, execution_id: &str) -> PathBuf {
        self.execution_dir(execution_id).join(CONTEXT_FILE)
    }

    /// `{root}/executions/{execution_id}/events`
    pub fn events_dir(&self, execution_id: &str) -> PathBuf {
        self.execution_dir(execution_id).join(EVENTS_DIR)
    }

    /// `{root}/executions/{execution_id}/events/{sequence:08}.json`
    pub fn event_file(&self, execution_id: &str, sequence: u64) -> PathBuf {
        self.events_dir(execution_id)
            .join(event_file_name(sequence))
    }

    /// `{root}/executions/{execution_id}/artifacts`
    pub fn artifacts_dir(&self, execution_id: &str) -> PathBuf {
        self.execution_dir(execution_id).join(ARTIFACTS_DIR)
    }

    /// `{root}/executions/{execution_id}/artifacts/{artifact_id}`
    pub fn artifact_dir(&self, execution_id: &ExecutionId, artifact_id: &str) -> PathBuf {
        self.artifacts_dir(execution_id).join(artifact_id)
    }
}

impl Default for Paths {
    fn default() -> Self {
        Self::new(DEFAULT_ROOT)
    }
}

/// Renders the file name of an event: zero-padded sequence plus `.json`.
pub fn event_file_name(sequence: u64) -> String {
    format!("{sequence:0width$}.json", width = SEQUENCE_WIDTH)
}

/// Parses a sequence number back out of an event file name.
///
/// Returns `None` for anything that is not a well-formed event file, including
/// leftover `.tmp` files, so that callers can simply skip foreign entries.
pub fn parse_event_file_name(file_name: &str) -> Option<u64> {
    let stem = file_name.strip_suffix(".json")?;
    if stem.len() != SEQUENCE_WIDTH || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    stem.parse().ok()
}

/// Returns true when the path is a temporary file of an interrupted write.
pub fn is_temp_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(TEMP_SUFFIX))
}

/// Returns the temporary file used while writing `path` atomically.
///
/// The temporary file sits in the same directory as its target, which is what
/// makes the subsequent `rename` atomic.
///
/// This is the *shape* of an interrupted write, used to recognise and to
/// fabricate one. A writer picks its own name with [`unique_temp_path_for`];
/// the two agree on the suffix, which is all [`is_temp_file`] and the sweeper
/// care about.
pub fn temp_path_for(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(TEMP_SUFFIX);
    match path.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}

/// Returns a temporary file that no concurrent write of `path` will pick too.
///
/// Deriving the temporary name from the target alone — which is what
/// [`temp_path_for`] does — is safe for one writer and unsafe for several: two
/// processes writing the same target open the *same* temporary file, so one
/// overwrites the other's bytes and one renames a file the other already moved
/// away. The first splices two documents into one, the second fails a write
/// that had every right to succeed.
///
/// The random component removes both. The suffix is unchanged, so an orphan
/// left by a crash is still recognised by [`is_temp_file`] and still swept.
pub fn unique_temp_path_for(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(format!(".{}", uuid::Uuid::new_v4()));
    name.push(TEMP_SUFFIX);
    match path.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}
