//! Directory layout of the three roots.
//!
//! All path construction lives here so that the layout described in
//! `DOMAIN_MODEL.md` §2.7 is defined exactly once — which is why splitting one
//! root into three did not scatter it into three modules.
//!
//! The three are separate *types* rather than three fields of one, because the
//! guarantee wanted is about what a consumer can reach: an [`ExecutionStore`]
//! holding [`StatePaths`] has no method that yields a workflow directory, and
//! no raw root to join one out of. Sharing would make the split a convention
//! that holds until someone writes `.join("..")`.
//!
//! ```text
//! {catalog}/repositories/{id}.yaml   -- CatalogPaths: declared, shareable,
//! {catalog}/workflows/{name}.yaml                     mountable read-only
//!
//! {state}/executions/{id}/…          -- StatePaths: owned by one process
//! {state}/narsor.log
//!
//! {work}/tasks/{id}/code/…           -- WorkPaths: local scratch, disposable
//! ```
//!
//! All three default to the same `./.narsor`, so a run that configures nothing
//! puts every file exactly where it was before the split. The layout only
//! comes apart when someone deliberately takes it apart.
//!
//! [`ExecutionStore`]: crate::persistence::ExecutionStore

use crate::domain::ExecutionId;
use std::path::{Path, PathBuf};

/// Default root, relative to the process working directory.
///
/// One value shared by all three roots: the split is meant to be invisible
/// until an operator separates them on purpose.
pub const DEFAULT_ROOT: &str = "./.narsor";

/// Directory holding workflow definitions, under the catalogue root.
pub const WORKFLOWS_DIR: &str = "workflows";

/// Directory holding registry entries, under the catalogue root.
pub const REPOSITORIES_DIR: &str = "repositories";

/// Directory holding every execution workspace, under the work root.
pub const TASKS_DIR: &str = "tasks";

/// Directory inside a task that holds the checkouts.
pub const CODE_DIR: &str = "code";

/// File the terminal interface appends its log to, under the state root.
///
/// State rather than declaration, so it belongs here and not beside the
/// workflows — and the catalogue may be mounted read-only, which a log file
/// would not survive.
pub const LOG_FILE: &str = "narsor.log";

/// File the daemon writes so other processes can discover it (`DaemonInfo`).
///
/// State rather than declaration, for the same reason as [`LOG_FILE`]: it is
/// written by exactly one running process and would not survive a read-only
/// mount of the catalogue.
pub const DAEMON_FILE: &str = "daemon.json";

/// Exclusive lock used to serialize daemon spawn against a race between
/// several clients discovering no daemon at once.
pub const DAEMON_LOCK_FILE: &str = "daemon.lock";

/// Directory holding every execution, directly under the root.
pub const EXECUTIONS_DIR: &str = "executions";

/// File holding the serialized `WorkflowExecution`.
pub const STATE_FILE: &str = "state.json";

/// File holding the serialized `Context`.
pub const CONTEXT_FILE: &str = "context.json";

/// The definition an execution was started from, pinned inside its directory.
///
/// An execution is resumable from its own record alone, so the definition
/// travels with it rather than being looked up by name in a catalogue that may
/// have moved on (D31).
pub const DEFINITION_FILE: &str = "definition.yaml";

/// Directory holding the append-only event log of one execution.
pub const EVENTS_DIR: &str = "events";

/// Directory holding artifact files of one execution.
pub const ARTIFACTS_DIR: &str = "artifacts";

/// Directory holding one conversation file per action attempt (U2).
pub const CONVERSATIONS_DIR: &str = "conversations";

/// Suffix of a conversation file still being written by a live attempt.
pub const CONVERSATION_JSONL_SUFFIX: &str = ".jsonl";

/// Suffix of a conversation file whose attempt has closed.
pub const CONVERSATION_ZST_SUFFIX: &str = ".jsonl.zst";

/// The one checkpoint kept for an execution (R25).
pub const CHECKPOINT_FILE: &str = "checkpoint.bundle";

/// Git's own scratch area for producing and applying a checkpoint.
///
/// Deliberately outside the work tree, so that a workspace which is not a git
/// repository does not acquire a `.git` directory it never asked for.
pub const CHECKPOINT_SCRATCH_DIR: &str = "checkpoint.git";

/// Suffix of a not-yet-renamed atomic write. Never a file to read from.
pub const TEMP_SUFFIX: &str = ".tmp";

/// Number of digits in an event file name (`00000001.json`).
const SEQUENCE_WIDTH: usize = 8;

/// Resolves every path of the *state* layout below a configurable root.
///
/// Execution state and the log: everything one process writes while it runs a
/// task. Splitting the roots does not make this directory safe to share — the
/// event log's sequence counter is cached in the process and pauses live in a
/// mutex, so two processes on one state root still overwrite each other's
/// events. It is separable for mounting, not for sharing.
///
/// The root is configurable precisely so that tests can point it at a
/// temporary directory; nothing in this layer ever reads a global location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatePaths {
    root: PathBuf,
}

impl StatePaths {
    /// Creates a layout rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The state root itself.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `{root}/narsor.log`
    pub fn log_file(&self) -> PathBuf {
        self.root.join(LOG_FILE)
    }

    /// `{root}/daemon.json`
    pub fn daemon_file(&self) -> PathBuf {
        self.root.join(DAEMON_FILE)
    }

    /// `{root}/daemon.lock`
    pub fn daemon_lock_file(&self) -> PathBuf {
        self.root.join(DAEMON_LOCK_FILE)
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

    /// `{state}/executions/{id}/checkpoint.bundle`.
    pub fn checkpoint_file(&self, execution_id: &str) -> PathBuf {
        self.execution_dir(execution_id).join(CHECKPOINT_FILE)
    }

    /// `{state}/executions/{id}/checkpoint.git`.
    pub fn checkpoint_scratch(&self, execution_id: &str) -> PathBuf {
        self.execution_dir(execution_id).join(CHECKPOINT_SCRATCH_DIR)
    }

    /// `{state}/executions/{id}/definition.yaml`.
    pub fn definition_file(&self, execution_id: &str) -> PathBuf {
        self.execution_dir(execution_id).join(DEFINITION_FILE)
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

    /// `{root}/executions/{execution_id}/conversations`
    ///
    /// One file per action attempt, keyed by `action_execution_id` (D24), so
    /// every attempt and every re-entry gets its own record (R4, R7).
    pub fn conversations_dir(&self, execution_id: &str) -> PathBuf {
        self.execution_dir(execution_id).join(CONVERSATIONS_DIR)
    }

    /// `{root}/executions/{execution_id}/conversations/{attempt_id}.jsonl`
    ///
    /// The plain, append-only form written while the attempt is still in
    /// flight.
    pub fn conversation_file(&self, execution_id: &str, attempt_id: &str) -> PathBuf {
        self.conversations_dir(execution_id)
            .join(conversation_jsonl_name(attempt_id))
    }

    /// `{root}/executions/{execution_id}/conversations/{attempt_id}.jsonl.zst`
    ///
    /// The compressed form a closed attempt is rewritten to (R15).
    pub fn conversation_compressed_file(&self, execution_id: &str, attempt_id: &str) -> PathBuf {
        self.conversations_dir(execution_id)
            .join(conversation_zst_name(attempt_id))
    }
}

impl Default for StatePaths {
    fn default() -> Self {
        Self::new(DEFAULT_ROOT)
    }
}

// ---------------------------------------------------------------------------
// The catalogue
// ---------------------------------------------------------------------------

/// Resolves every path of the *catalogue* below a configurable root.
///
/// What a deployment declares: the workflow definitions and the repository
/// registry. Written rarely and from outside, read by every process, and the
/// one root that is meant to be shared between them — mounted read-only if the
/// deployment wishes, because nothing here is written while a task runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogPaths {
    root: PathBuf,
}

impl CatalogPaths {
    /// Creates a layout rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The catalogue root itself.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `{root}/workflows`
    pub fn workflows_dir(&self) -> PathBuf {
        self.root.join(WORKFLOWS_DIR)
    }

    /// `{root}/repositories`
    pub fn repositories_dir(&self) -> PathBuf {
        self.root.join(REPOSITORIES_DIR)
    }
}

impl Default for CatalogPaths {
    fn default() -> Self {
        Self::new(DEFAULT_ROOT)
    }
}

// ---------------------------------------------------------------------------
// The work directory
// ---------------------------------------------------------------------------

/// Resolves every path of the per-task *work* tree below a configurable root.
///
/// Git checkouts and whatever an action writes beside them: heavy, rewritable,
/// and interesting to nobody but the process that made it. This is the root a
/// deployment gives an `emptyDir` rather than a shared volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkPaths {
    root: PathBuf,
}

impl WorkPaths {
    /// Creates a layout rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The work root itself.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `{root}/tasks`
    pub fn tasks_dir(&self) -> PathBuf {
        self.root.join(TASKS_DIR)
    }

    /// `{root}/tasks/{execution_id}` — the whole tree owned by the execution.
    pub fn task_dir(&self, execution_id: &str) -> PathBuf {
        self.tasks_dir().join(execution_id)
    }

    /// `{root}/tasks/{execution_id}/code`
    pub fn code_dir(&self, execution_id: &str) -> PathBuf {
        self.task_dir(execution_id).join(CODE_DIR)
    }

    /// `{root}/tasks/{execution_id}/code/{repository_name}`
    pub fn checkout_dir(&self, execution_id: &str, repository_name: &str) -> PathBuf {
        self.code_dir(execution_id).join(repository_name)
    }
}

impl Default for WorkPaths {
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

/// Renders the plain conversation file name for one attempt.
pub fn conversation_jsonl_name(attempt_id: &str) -> String {
    format!("{attempt_id}{CONVERSATION_JSONL_SUFFIX}")
}

/// Renders the compressed conversation file name for one attempt.
pub fn conversation_zst_name(attempt_id: &str) -> String {
    format!("{attempt_id}{CONVERSATION_ZST_SUFFIX}")
}

/// Parses the attempt id back out of a plain conversation file name.
///
/// Returns `None` for anything that is not a well-formed plain conversation
/// file, notably a `.jsonl.zst` file or a leftover `.tmp`/`.zst.tmp` one, so
/// callers can simply skip foreign entries.
pub fn parse_conversation_jsonl_name(file_name: &str) -> Option<&str> {
    file_name.strip_suffix(CONVERSATION_JSONL_SUFFIX)
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
