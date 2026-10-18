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
//! {state}/agent-harness.json          -- operator-selected default harness
//! {state}/orchestration/…             -- control-plane task records, leases,
//!                                        controller identities, runner sessions
//! {state}/launch-specs/{task}.json    -- immutable launch input (task storage)
//! {state}/materials/…                 -- bounded task materials (U9): manifests
//!                                        and blobs, outside any workspace
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

/// Directory holding execution backend records, under the catalogue root.
pub const BACKENDS_DIR: &str = "backends";

/// Directory holding user profiles, under the catalogue root.
pub const PROFILES_DIR: &str = "profiles";

/// The runtime catalogue (runtime bundle and tool packages), under the
/// catalogue root.
pub const RUNTIME_FILE: &str = "runtime.yaml";

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

/// Network-mode ownership records (U7, R38): the installation owner and,
/// until somebody claims it, the one-time claim code.
pub const AUTH_DIR: &str = "auth";

/// The installation owner's durable record.
pub const OWNER_FILE: &str = "owner.json";

/// The digest of the current one-time claim code.
pub const CLAIM_FILE: &str = "claim.json";

/// The current one-time claim code itself, for the operator to read (`0600`).
pub const CLAIM_CODE_FILE: &str = "claim-code";

/// Versioned durable override for the daemon's default agent harness.
///
/// This is state owned by the daemon, rather than configuration: deployments
/// may mount the configuration catalogue read-only while still allowing an
/// operator to change the live default through the API.
pub const AGENT_HARNESS_SELECTION_FILE: &str = "agent-harness.json";

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

/// Directory of the conductor's own store inside an execution (D55, KTD2).
pub const CONDUCTOR_DIR: &str = "conductor";

/// Directory of gate annotations inside [`CONDUCTOR_DIR`] (R30).
pub const CONDUCTOR_ANNOTATIONS_DIR: &str = "annotations";

/// Durable control-plane records (KTD1), directly under the state root.
///
/// Deliberately a sibling of [`EXECUTIONS_DIR`] rather than inside it: local
/// crash recovery (`persistence::recovery`) walks `executions/` and marks an
/// interrupted action `Unknown`, and a remote task's orchestration record must
/// never be judged by that rule — only controller reconciliation knows whether
/// its workload is still alive.
pub const ORCHESTRATION_DIR: &str = "orchestration";

/// Directory of one sub-directory per orchestrated task, inside
/// [`ORCHESTRATION_DIR`].
pub const ORCHESTRATION_TASKS_DIR: &str = "tasks";

/// Directory of one sub-directory per backend (its controller lease), inside
/// [`ORCHESTRATION_DIR`].
pub const ORCHESTRATION_BACKENDS_DIR: &str = "backends";

/// The orchestration record of one task.
pub const TASK_RECORD_FILE: &str = "task.json";

/// Advisory lock serializing every mutation of one task record.
pub const TASK_LOCK_FILE: &str = "task.lock";

/// Advisory lock serializing task creation (identifier and queue sequence).
pub const TASKS_CREATE_LOCK_FILE: &str = "create.lock";

/// The fenced controller lease of one backend (KTD3).
pub const BACKEND_LEASE_FILE: &str = "lease.json";

/// Advisory lock serializing lease acquisition and renewal of one backend.
pub const BACKEND_LEASE_LOCK_FILE: &str = "lease.lock";

/// Advisory lock serializing the scheduling decisions of one backend (U10).
pub const BACKEND_SCHEDULE_LOCK_FILE: &str = "schedule.lock";

/// The owner's durable default-backend override (R10), inside
/// [`ORCHESTRATION_DIR`].
pub const DEFAULT_BACKEND_FILE: &str = "default-backend.json";

/// Advisory lock serializing changes of [`DEFAULT_BACKEND_FILE`].
pub const DEFAULT_BACKEND_LOCK_FILE: &str = "default-backend.lock";

/// Directory of one sub-directory per enrolled controller identity (R37),
/// inside [`ORCHESTRATION_DIR`].
pub const ORCHESTRATION_CONTROLLERS_DIR: &str = "controllers";

/// The record of one controller identity: backend scope, credential hash,
/// revocation, heartbeat and acknowledged generations.
pub const CONTROLLER_RECORD_FILE: &str = "controller.json";

/// Advisory lock serializing mutations of one controller record.
pub const CONTROLLER_LOCK_FILE: &str = "controller.lock";

/// Directory of one-time controller enrollment codes, inside
/// [`ORCHESTRATION_DIR`].
pub const ORCHESTRATION_ENROLLMENTS_DIR: &str = "enrollments";

/// Advisory lock serializing enrollment creation and redemption.
pub const ENROLLMENTS_LOCK_FILE: &str = "enroll.lock";

/// The current runner credential of one task (KTD3), next to its record but
/// outside it, so the credential hash never travels with the record.
pub const RUNNER_SESSION_FILE: &str = "runner-session.json";

/// Directory of immutable task launch specifications on task storage.
///
/// Not inside `executions/{id}`: a directory there without a `state.json`
/// would show up in every execution listing as a broken execution.
pub const LAUNCH_SPECS_DIR: &str = "launch-specs";

/// Bounded task materials (U9), directly under the state root.
///
/// A sibling of [`ORCHESTRATION_DIR`], not inside it: material blobs are
/// opaque user bytes, and nothing that walks task records, event logs or
/// controller identities should ever come across them. Every task id is a
/// valid identifier, which never starts with `.`, so the two dot-directories
/// below can never collide with a task.
pub const MATERIALS_DIR: &str = "materials";

/// Uploads under way, one sub-directory each, inside [`MATERIALS_DIR`]. An
/// upload becomes a material only by being renamed out of here, so whatever
/// is left here after a crash is garbage.
pub const MATERIALS_STAGING_DIR: &str = ".staging";

/// Per-task advisory locks of the material store, inside [`MATERIALS_DIR`]
/// and outside the task directories, which archive deletes.
pub const MATERIALS_LOCKS_DIR: &str = ".locks";

/// The record (manifest and delivery state) of one material.
pub const MATERIAL_RECORD_FILE: &str = "material.json";

/// Directory of one material's content-addressed blobs.
pub const MATERIAL_BLOBS_DIR: &str = "blobs";

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

    /// `{root}/auth`
    pub fn auth_dir(&self) -> PathBuf {
        self.root.join(AUTH_DIR)
    }

    /// `{root}/auth/owner.json`
    pub fn owner_file(&self) -> PathBuf {
        self.auth_dir().join(OWNER_FILE)
    }

    /// `{root}/auth/claim.json`
    pub fn claim_file(&self) -> PathBuf {
        self.auth_dir().join(CLAIM_FILE)
    }

    /// `{root}/auth/claim-code`
    pub fn claim_code_file(&self) -> PathBuf {
        self.auth_dir().join(CLAIM_CODE_FILE)
    }

    /// `{root}/agent-harness.json`
    pub fn agent_harness_selection_file(&self) -> PathBuf {
        self.root.join(AGENT_HARNESS_SELECTION_FILE)
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

    /// `{root}/executions/{execution_id}/step-replies/{attempt_id}.txt`.
    pub fn step_reply_file(&self, execution_id: &str, attempt_id: &str) -> PathBuf {
        self.execution_dir(execution_id)
            .join("step-replies")
            .join(format!("{attempt_id}.txt"))
    }

    /// `{root}/orchestration`
    pub fn orchestration_dir(&self) -> PathBuf {
        self.root.join(ORCHESTRATION_DIR)
    }

    /// `{root}/orchestration/tasks`
    pub fn orchestration_tasks_dir(&self) -> PathBuf {
        self.orchestration_dir().join(ORCHESTRATION_TASKS_DIR)
    }

    /// `{root}/orchestration/tasks/create.lock`
    pub fn orchestration_create_lock_file(&self) -> PathBuf {
        self.orchestration_tasks_dir().join(TASKS_CREATE_LOCK_FILE)
    }

    /// `{root}/orchestration/tasks/{task_id}`
    pub fn orchestration_task_dir(&self, task_id: &str) -> PathBuf {
        self.orchestration_tasks_dir().join(task_id)
    }

    /// `{root}/orchestration/tasks/{task_id}/task.json`
    pub fn orchestration_task_file(&self, task_id: &str) -> PathBuf {
        self.orchestration_task_dir(task_id).join(TASK_RECORD_FILE)
    }

    /// `{root}/orchestration/tasks/{task_id}/task.lock`
    pub fn orchestration_task_lock_file(&self, task_id: &str) -> PathBuf {
        self.orchestration_task_dir(task_id).join(TASK_LOCK_FILE)
    }

    /// `{root}/orchestration/tasks/{task_id}/events`
    pub fn orchestration_task_events_dir(&self, task_id: &str) -> PathBuf {
        self.orchestration_task_dir(task_id).join(EVENTS_DIR)
    }

    /// `{root}/orchestration/tasks/{task_id}/events/{sequence:08}.json`
    pub fn orchestration_task_event_file(&self, task_id: &str, sequence: u64) -> PathBuf {
        self.orchestration_task_events_dir(task_id)
            .join(event_file_name(sequence))
    }

    /// `{root}/orchestration/backends`
    pub fn orchestration_backends_dir(&self) -> PathBuf {
        self.orchestration_dir().join(ORCHESTRATION_BACKENDS_DIR)
    }

    /// `{root}/orchestration/backends/{backend_id}`
    pub fn orchestration_backend_dir(&self, backend_id: &str) -> PathBuf {
        self.orchestration_backends_dir().join(backend_id)
    }

    /// `{root}/orchestration/backends/{backend_id}/lease.json`
    pub fn backend_lease_file(&self, backend_id: &str) -> PathBuf {
        self.orchestration_backend_dir(backend_id)
            .join(BACKEND_LEASE_FILE)
    }

    /// `{root}/orchestration/backends/{backend_id}/lease.lock`
    pub fn backend_lease_lock_file(&self, backend_id: &str) -> PathBuf {
        self.orchestration_backend_dir(backend_id)
            .join(BACKEND_LEASE_LOCK_FILE)
    }

    /// `{root}/orchestration/backends/{backend_id}/schedule.lock`
    pub fn backend_schedule_lock_file(&self, backend_id: &str) -> PathBuf {
        self.orchestration_backend_dir(backend_id)
            .join(BACKEND_SCHEDULE_LOCK_FILE)
    }

    /// `{root}/orchestration/default-backend.json`
    pub fn default_backend_file(&self) -> PathBuf {
        self.orchestration_dir().join(DEFAULT_BACKEND_FILE)
    }

    /// `{root}/orchestration/default-backend.lock`
    pub fn default_backend_lock_file(&self) -> PathBuf {
        self.orchestration_dir().join(DEFAULT_BACKEND_LOCK_FILE)
    }

    /// `{root}/launch-specs`
    pub fn launch_specs_dir(&self) -> PathBuf {
        self.root.join(LAUNCH_SPECS_DIR)
    }

    /// `{root}/launch-specs/{task_id}.json`
    pub fn task_launch_spec_file(&self, task_id: &str) -> PathBuf {
        self.launch_specs_dir().join(format!("{task_id}.json"))
    }

    /// `{root}/orchestration/tasks/{task_id}/runner-session.json`
    pub fn runner_session_file(&self, task_id: &str) -> PathBuf {
        self.orchestration_task_dir(task_id)
            .join(RUNNER_SESSION_FILE)
    }

    /// `{root}/materials`
    pub fn materials_dir(&self) -> PathBuf {
        self.root.join(MATERIALS_DIR)
    }

    /// `{root}/materials/.staging`
    pub fn materials_staging_dir(&self) -> PathBuf {
        self.materials_dir().join(MATERIALS_STAGING_DIR)
    }

    /// `{root}/materials/.locks/{task_id}.lock`
    pub fn task_materials_lock_file(&self, task_id: &str) -> PathBuf {
        self.materials_dir()
            .join(MATERIALS_LOCKS_DIR)
            .join(format!("{task_id}.lock"))
    }

    /// `{root}/materials/{task_id}`
    pub fn task_materials_dir(&self, task_id: &str) -> PathBuf {
        self.materials_dir().join(task_id)
    }

    /// `{root}/materials/{task_id}/{material_id}`
    pub fn material_dir(&self, task_id: &str, material_id: &str) -> PathBuf {
        self.task_materials_dir(task_id).join(material_id)
    }

    /// `{root}/materials/{task_id}/{material_id}/material.json`
    pub fn material_record_file(&self, task_id: &str, material_id: &str) -> PathBuf {
        self.material_dir(task_id, material_id)
            .join(MATERIAL_RECORD_FILE)
    }

    /// `{root}/materials/{task_id}/{material_id}/blobs/{sha256}`
    pub fn material_blob_file(&self, task_id: &str, material_id: &str, sha256: &str) -> PathBuf {
        self.material_dir(task_id, material_id)
            .join(MATERIAL_BLOBS_DIR)
            .join(sha256)
    }

    /// `{root}/orchestration/controllers`
    pub fn orchestration_controllers_dir(&self) -> PathBuf {
        self.orchestration_dir().join(ORCHESTRATION_CONTROLLERS_DIR)
    }

    /// `{root}/orchestration/controllers/{controller_id}/controller.json`
    pub fn controller_record_file(&self, controller_id: &str) -> PathBuf {
        self.orchestration_controllers_dir()
            .join(controller_id)
            .join(CONTROLLER_RECORD_FILE)
    }

    /// `{root}/orchestration/controllers/{controller_id}/controller.lock`
    pub fn controller_lock_file(&self, controller_id: &str) -> PathBuf {
        self.orchestration_controllers_dir()
            .join(controller_id)
            .join(CONTROLLER_LOCK_FILE)
    }

    /// `{root}/orchestration/enrollments`
    pub fn orchestration_enrollments_dir(&self) -> PathBuf {
        self.orchestration_dir().join(ORCHESTRATION_ENROLLMENTS_DIR)
    }

    /// `{root}/orchestration/enrollments/{enrollment_id}.json`
    pub fn enrollment_file(&self, enrollment_id: &str) -> PathBuf {
        self.orchestration_enrollments_dir()
            .join(format!("{enrollment_id}.json"))
    }

    /// `{root}/orchestration/enrollments/enroll.lock`
    pub fn enrollments_lock_file(&self) -> PathBuf {
        self.orchestration_enrollments_dir()
            .join(ENROLLMENTS_LOCK_FILE)
    }

    /// `{state}/executions/{id}/checkpoint.bundle`.
    pub fn checkpoint_file(&self, execution_id: &str) -> PathBuf {
        self.execution_dir(execution_id).join(CHECKPOINT_FILE)
    }

    /// `{state}/executions/{id}/checkpoint.git`.
    pub fn checkpoint_scratch(&self, execution_id: &str) -> PathBuf {
        self.execution_dir(execution_id)
            .join(CHECKPOINT_SCRATCH_DIR)
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

    /// `{root}/executions/{execution_id}/conductor`
    ///
    /// The conductor's state, chat and annotations (KTD2): beside the
    /// execution's own files, written by nobody but the conductor.
    pub fn conductor_dir(&self, execution_id: &str) -> PathBuf {
        self.execution_dir(execution_id).join(CONDUCTOR_DIR)
    }

    /// `{root}/executions/{execution_id}/conductor/annotations`
    pub fn conductor_annotations_dir(&self, execution_id: &str) -> PathBuf {
        self.conductor_dir(execution_id)
            .join(CONDUCTOR_ANNOTATIONS_DIR)
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

impl AsRef<Path> for StatePaths {
    fn as_ref(&self) -> &Path {
        self.root()
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

    /// `{root}/backends`
    pub fn backends_dir(&self) -> PathBuf {
        self.root.join(BACKENDS_DIR)
    }

    /// `{root}/profiles`
    pub fn profiles_dir(&self) -> PathBuf {
        self.root.join(PROFILES_DIR)
    }

    /// `{root}/runtime.yaml`
    pub fn runtime_file(&self) -> PathBuf {
        self.root.join(RUNTIME_FILE)
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
