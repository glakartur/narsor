//! The observers' wire types:
//!
//! * the effect routes an observer's script calls through `narsor-cli`, with
//!   the token of its run (`/observer/v1/runs/{run}/…`);
//! * the worker routes `narsor-cli observer worker` calls with its own
//!   credential (`/observer/v1/worker/…`): claim a run, report its result;
//! * the owner routes that create and revoke that credential
//!   (`/control/v1/observer-workers`);
//! * the owner routes of the observers themselves (`/control/v1/observers`).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `POST /observer/v1/runs/{run}/tasks`: create a task from the effect data.
///
/// Only the key and the data: the workflow, repositories, backend and
/// profile come from the observer's record, and only the fields its mapping
/// names become workflow inputs. Any other field is refused.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ObserverCreateTaskRequest {
    /// The effect key: one task per key and observer, ever (R13).
    pub effect_key: String,
    /// The effect data, a JSON object whose fields the mapping names.
    #[serde(default)]
    pub data: Value,
}

/// `POST /observer/v1/runs/{run}/tasks/{task_id}/events`: deliver an event
/// to the task of one of the observer's watch entries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ObserverEventRequest {
    /// The effect key: one event per key, observer and task, ever (R13).
    pub effect_key: String,
    /// The event type (`review.approved`, say).
    pub event_type: String,
    /// The event's data, any JSON value of bounded size.
    #[serde(default)]
    pub data: Value,
}

/// What an effect route did.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObserverEffectOutcome {
    /// A task was created.
    Created,
    /// The key was used before; nothing was done again (PO3: not an error).
    Duplicate,
    /// A dry run: the task would have been created (R10).
    WouldCreate,
    /// The event matched the task's waiting `wait` step and was queued for
    /// it; it is in the task's history too.
    Delivered,
    /// The task does not wait for this event: it went to the history only.
    Recorded,
    /// A dry run: the event would have been delivered (R10).
    WouldDeliver,
    /// An outcome from a newer server.
    #[serde(other)]
    Unrecognized,
}

/// The answer of an effect route.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverEffectResponse {
    /// What was done.
    pub outcome: ObserverEffectOutcome,
    /// The effect key, as given.
    pub effect_key: String,
    /// The task created, found or addressed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// The input queued for the waiting step, for a delivered event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_id: Option<String>,
    /// One line for the script's log ("skipped: duplicate", say).
    pub message: String,
}

// ---------------------------------------------------------------------------
// The worker credential (owner routes)
// ---------------------------------------------------------------------------

/// `POST /control/v1/observer-workers`: create a worker credential.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CreateObserverWorkerRequest {
    /// A label for the worker (a host name, say).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// One worker credential, without its secret.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverWorkerView {
    /// The credential's id (`wkr-…`), the middle of the credential.
    pub worker_id: String,
    /// The owner's label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// When it was created.
    pub created_at: DateTime<Utc>,
    /// When it was revoked; a revoked credential never works again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<DateTime<Utc>>,
}

/// The answer of `POST /control/v1/observer-workers`: the credential, shown
/// this once. The server keeps only its SHA-256 digest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverWorkerCreated {
    /// The credential's record.
    pub worker: ObserverWorkerView,
    /// The credential (`nsw.{worker_id}.{secret}`).
    pub credential: String,
}

/// `GET /control/v1/observer-workers`: the owner's worker credentials,
/// revoked ones included.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverWorkerListResponse {
    /// By id.
    pub workers: Vec<ObserverWorkerView>,
}

// ---------------------------------------------------------------------------
// The worker routes
// ---------------------------------------------------------------------------

/// One secret the worker hands to a run's script as an environment
/// variable. The value comes from the worker's secrets directory, one file
/// per `name`; the control plane never has it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverSecretRef {
    /// The file name in the worker's secrets directory.
    pub name: String,
    /// The environment variable the script reads it from.
    pub env: String,
}

/// A run handed to a worker by `POST /observer/v1/worker/claim`.
///
/// The token is in no record of the server: this answer is the only time
/// anyone sees it. `Debug` hides it.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct ObserverWorkerRun {
    /// The run (`{observer}.{sequence}`).
    pub run_id: String,
    /// The run's token: the script's `NARSOR_TOKEN` on the effect routes.
    pub token: String,
    /// Effects are recorded, not performed.
    pub dry_run: bool,
    /// The script version it runs.
    pub script_version: u32,
    /// The script, run with `sh`.
    pub script: String,
    /// The document the script reads on stdin.
    pub input: Value,
    /// The secrets to hand to the script; empty for a dry run of a version
    /// that is not approved, unless the owner allowed them.
    #[serde(default)]
    pub secrets: Vec<ObserverSecretRef>,
    /// How long the script may run.
    pub timeout_secs: u64,
    /// No result by then: the run is lost and its token stops working.
    pub deadline: DateTime<Utc>,
}

impl std::fmt::Debug for ObserverWorkerRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObserverWorkerRun")
            .field("run_id", &self.run_id)
            .field("token", &"<redacted>")
            .field("dry_run", &self.dry_run)
            .field("script_version", &self.script_version)
            .field("secrets", &self.secrets)
            .field("timeout_secs", &self.timeout_secs)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

/// The answer of `POST /observer/v1/worker/claim`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ObserverClaimResponse {
    /// The run to execute, `None` when no run is due.
    #[serde(default)]
    pub run: Option<ObserverWorkerRun>,
}

/// How a run ended, as the worker saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObserverRunOutcome {
    /// The script exited with this code (128 + the signal's number when a
    /// signal killed it).
    Exited {
        /// Its exit code.
        exit_code: i32,
    },
    /// The script printed more on stdout than the contract allows: a
    /// format error, whatever its exit code.
    OutputTooLarge {
        /// Its exit code.
        exit_code: i32,
    },
    /// The script ran past its time limit and was killed.
    TimedOut,
    /// The worker could not run the script (a missing secret, say).
    WorkerError {
        /// What went wrong, without secrets.
        message: String,
    },
    /// An outcome from a newer worker; counts as a worker error.
    #[serde(other)]
    Unrecognized,
}

/// `POST /observer/v1/worker/runs/{run}/result`: the worker's result of a
/// run. Secret values and the run's token are masked, control characters
/// removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObserverRunReport {
    /// How it ended.
    pub outcome: ObserverRunOutcome,
    /// The script's stdout: the contract's document.
    #[serde(default)]
    pub stdout: String,
    /// The script's stderr.
    #[serde(default)]
    pub stderr: String,
}

/// The answer of `POST /observer/v1/worker/runs/{run}/result`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverRunResultResponse {
    /// The run.
    pub run_id: String,
    /// How the server judged it (`succeeded`, `failed`, `timed_out`,
    /// `invalid_output`, `worker_error`).
    pub status: String,
    /// Why it failed, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// The owner's observer routes (`/control/v1/observers`)
// ---------------------------------------------------------------------------

/// What every task of a "create a task" observer runs, or that the
/// observer delivers events to the tasks of its watch entries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObserverEffectRequest {
    /// Create a task per effect key.
    CreateTask {
        /// Workflow of the catalogue.
        workflow_id: String,
        /// Its version; the catalogue's when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workflow_version: Option<u32>,
        /// Repository choices by workspace name, as in a task create
        /// request.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        workspaces: BTreeMap<String, super::WorkspaceRequest>,
        /// Backend of the tasks; the owner's default when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        backend_id: Option<String>,
        /// Environment profile; the owner's default when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
        /// Workflow input name → field name in the effect data.
        #[serde(default)]
        mapping: BTreeMap<String, String>,
    },
    /// Deliver events to the tasks of the observer's watch entries.
    DeliverEvent,
}

/// `POST /control/v1/observers`: a new draft observer. Nothing runs until
/// the owner saves a script and approves a version of it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CreateObserverRequest {
    /// Unique name.
    pub name: String,
    /// Free text naming the source (`jira`, `gitlab.example.com`, …).
    pub source: String,
    /// Parameters handed to the script as they are; a JSON object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<Value>,
    /// Seconds between runs; the server's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_secs: Option<u64>,
    /// What its script may do.
    pub effect: ObserverEffectRequest,
    /// Whether the script looks only at the objects of its watch entries
    /// (required for [`ObserverEffectRequest::DeliverEvent`]).
    #[serde(default)]
    pub watch_list: bool,
    /// Secrets handed to the script.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<ObserverSecretRef>,
    /// Most tasks one run may create; the server's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_limit_per_run: Option<u32>,
    /// A watch entry's object must start with one of these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_object_prefixes: Vec<String>,
    /// Longest object of a watch entry; the server's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_object_len: Option<usize>,
    /// Whether the first run imports what already matches the condition.
    #[serde(default)]
    pub import_backlog: bool,
}

/// The latest finished run of an observer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverLastRunView {
    /// Its sequence number.
    pub sequence: u64,
    /// How it ended (`succeeded`, `failed`, `timed_out`, `invalid_output`,
    /// `worker_error`, `lost`).
    pub status: String,
    /// When it ended.
    pub finished_at: DateTime<Utc>,
    /// Why it failed, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A dry run the owner asked for and no worker claimed yet.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverDryRunView {
    /// The script version it runs.
    pub version: u32,
    /// Whether the owner allowed secret values for it.
    pub with_secrets: bool,
    /// When it was asked for.
    pub requested_at: DateTime<Utc>,
}

/// One observer as `observer list` shows it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverView {
    /// Random id, never reused.
    pub observer_id: String,
    /// Unique name.
    pub name: String,
    /// The source it polls.
    pub source: String,
    /// `draft`, `active`, `paused`, `needs_attention` or `error`.
    pub state: String,
    /// `create_task` or `deliver_event`.
    pub effect: String,
    /// The workflow of a "create a task" observer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<String>,
    /// Whether it watches only the objects of its watch entries.
    pub watch_list: bool,
    /// Seconds between runs.
    pub interval_secs: u64,
    /// Most tasks one run may create.
    pub task_limit_per_run: u32,
    /// The only version the schedule runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_version: Option<u32>,
    /// The newest script version, approved or not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<u32>,
    /// When the next scheduled run is due.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<DateTime<Utc>>,
    /// Since when a run waits for a worker to claim it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_for_claim_since: Option<DateTime<Utc>>,
    /// A run waited for a claim longer than one interval: no worker seems
    /// to be polling (R18).
    pub worker_missing: bool,
    /// When a worker last claimed a run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_claim_at: Option<DateTime<Utc>>,
    /// The run in progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_run: Option<u64>,
    /// A dry run asked for and not claimed yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_dry_run: Option<ObserverDryRunView>,
    /// The latest finished run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<ObserverLastRunView>,
    /// Failed runs in a row.
    pub consecutive_failures: u32,
    /// When it was created.
    pub created_at: DateTime<Utc>,
}

/// `GET /control/v1/observers`: the caller's observers, by name.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverListResponse {
    /// By name.
    pub observers: Vec<ObserverView>,
}

/// One immutable script version.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverScriptView {
    /// Version number, from 1.
    pub version: u32,
    /// When it was saved.
    pub created_at: DateTime<Utc>,
    /// `sha256:` digest of the script.
    pub sha256: String,
}

/// An effect a dry run would have had.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObserverPlannedEffectView {
    /// `create_task` or `deliver_event`.
    pub kind: String,
    /// Its effect key.
    pub effect_key: String,
    /// The task it would have created or gone to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// What the script sent with it.
    #[serde(default)]
    pub data: Value,
}

/// One run of an observer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObserverRunView {
    /// The run (`{observer}.{sequence}`).
    pub run_id: String,
    /// Its sequence number.
    pub sequence: u64,
    /// Whether it was a dry run.
    pub dry_run: bool,
    /// The script version it ran.
    pub script_version: u32,
    /// `running`, or how it ended.
    pub status: String,
    /// When a worker claimed it.
    pub claimed_at: DateTime<Utc>,
    /// When it ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    /// The script's exit code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Why it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Tasks it created.
    pub tasks_created: u32,
    /// What a dry run would have done.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub planned_effects: Vec<ObserverPlannedEffectView>,
    /// The script's stderr (masked, bounded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
}

/// `GET /control/v1/observers/{observer}`: one observer in detail.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObserverDetailView {
    /// What `observer list` shows.
    pub observer: ObserverView,
    /// Parameters handed to the script.
    pub condition: Value,
    /// Workflow input name → effect data field, for a "create a task"
    /// observer.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mapping: BTreeMap<String, String>,
    /// The secret references (never values).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<ObserverSecretRef>,
    /// A watch entry's object must start with one of these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_object_prefixes: Vec<String>,
    /// Every script version, oldest first.
    #[serde(default)]
    pub scripts: Vec<ObserverScriptView>,
    /// The latest runs, newest first.
    #[serde(default)]
    pub runs: Vec<ObserverRunView>,
}

/// `POST /control/v1/observers/{observer}/scripts`: the next script
/// version. Saving never approves it (AE6).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SetObserverScriptRequest {
    /// The shell script.
    pub script: String,
}

/// `POST /control/v1/observers/{observer}/dry-run`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObserverDryRunRequest {
    /// Script version to run; the newest when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// Hand secret values to a version that is not the approved one (PO6).
    #[serde(default)]
    pub with_secrets: bool,
}

/// `POST /control/v1/observers/{observer}/approve`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApproveObserverRequest {
    /// The script version the schedule runs from now on.
    pub version: u32,
}

/// `POST /control/v1/observers/{observer}/watches`: watch an object for a
/// task of the caller.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AddObserverWatchRequest {
    /// The task its events go to.
    pub task_id: String,
    /// The watched object (a PR/MR URL, say).
    pub object: String,
    /// The event types to catch.
    pub event_types: Vec<String>,
}

/// One watch entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverWatchView {
    /// The entry's id.
    pub entry_id: String,
    /// The task its events go to.
    pub task_id: String,
    /// The workflow step that added it; absent for an owner's entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    /// The watched object.
    pub object: String,
    /// The event types it catches.
    pub event_types: Vec<String>,
    /// `workflow` or `operator`.
    pub origin: String,
    /// When it was added.
    pub created_at: DateTime<Utc>,
}

/// `GET /control/v1/observers/{observer}/watches`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverWatchListResponse {
    /// Oldest first.
    pub watches: Vec<ObserverWatchView>,
}
