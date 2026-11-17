//! The observers' wire types:
//!
//! * the effect routes an observer's script calls through `narsor-cli`, with
//!   the token of its run (`/observer/v1/runs/{run}/…`);
//! * the worker routes `narsor-observer-worker` calls with the key the
//!   server gave it (`/observer/v1/worker/…`): claim a run, report its
//!   result;
//! * the owner routes of the observers themselves (`/control/v1/observers`)
//!   and of their queues (`/control/v1/observer-queues`).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `POST /observer/v1/runs/{run}/tasks`: create a task from the effect data.
///
/// Only the data: the workflow, repositories, backend and
/// profile come from the observer's record, and only the fields its mapping
/// names become workflow inputs. Any other field is refused.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ObserverCreateTaskRequest {
    /// The effect data, a JSON object whose fields the mapping names.
    #[serde(default)]
    pub data: Value,
}

/// `POST /observer/v1/runs/{run}/decide`: answer the gate of the run's queue
/// entry, as a person answers it (R12, R13).
///
/// The task and the episode must be those of the entry the run was started
/// for; a run of an `always` observer has none and answers no gate. The
/// choice must be one of the gate's, every variable it requires must have a
/// value, and every variable must be one the gate declares. No artifacts in
/// this version.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ObserverDecideRequest {
    /// The task waiting at the gate.
    pub task_id: String,
    /// The wait episode the answer is for: the entry's episode.
    pub episode: String,
    /// The choice, by its name.
    pub choice: String,
    /// Why the script chose it; the task's history shows it (R22).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The values of the gate's variables, by name.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub variables: serde_json::Map<String, Value>,
}

/// What an effect route did.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObserverEffectOutcome {
    /// A task was created.
    Created,
    /// The answer was queued for the task's waiting gate. The runner
    /// applies it; the gate exits with it unless another answer for the
    /// same episode came first.
    Delivered,
    /// The task's history recorded it; nothing was queued.
    Recorded,
    /// An outcome from a newer server.
    #[serde(other)]
    Unrecognized,
}

/// The answer of an effect route.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverEffectResponse {
    /// What was done.
    pub outcome: ObserverEffectOutcome,
    /// The task created or addressed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// The input queued for the waiting step, for a delivered answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_id: Option<String>,
    /// One line for the script's log ("created task …", say).
    pub message: String,
}

// ---------------------------------------------------------------------------
// The worker routes
// ---------------------------------------------------------------------------

/// The environment variable the server hands the observer worker its key
/// in. The server makes a new key at every start of the worker and keeps
/// only its digest; a variable, never an argument, so `ps` does not show
/// it. The worker passes it on to no script.
pub const OBSERVER_WORKER_KEY_ENV: &str = "NARSOR_OBSERVER_WORKER_KEY";

/// One secret the worker hands to a run's script as an environment
/// variable. The value comes from the observer owner's subdirectory of the
/// worker's secrets directory, one file per `name`; the control plane never
/// has it.
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
    /// The observer's owner: the worker serves every owner and reads this
    /// run's secrets from the owner's subdirectory of its secrets
    /// directory only. Empty when the server did not say; the run then
    /// fails before its script starts.
    #[serde(default)]
    pub owner: String,
    /// The run's token: the script's `NARSOR_TOKEN` on the effect routes.
    pub token: String,
    /// The script version it runs.
    pub script_version: u32,
    /// The script, run with `sh`.
    pub script: String,
    /// The document the script reads on stdin.
    pub input: Value,
    /// The secrets to hand to the script: the observer's references.
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
            .field("owner", &self.owner)
            .field("token", &"<redacted>")
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

/// What every task of a "create a task" observer runs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObserverEffectRequest {
    /// Create a task per `task create` call of the script.
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
}

/// When an observer's script runs (R25). Every observer of one queue has
/// the same mode (R26).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObserverMode {
    /// Only for the entries of its queue, one entry per run: no entry, no
    /// run.
    OnEpisodes,
    /// On every tick of its schedule, with no entry.
    Always,
}

impl ObserverMode {
    /// The wire name (`on_episodes`, `always`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnEpisodes => "on_episodes",
            Self::Always => "always",
        }
    }
}

impl std::fmt::Display for ObserverMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
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
    /// The queue it serves, in the owner's namespace; the observer's name
    /// when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<String>,
    /// When its script runs; `on_episodes` when absent, `always` for an
    /// observer that creates tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<ObserverMode>,
    /// Seconds between runs. Exactly one of `interval_secs` and `cron`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_secs: Option<u64>,
    /// A five-field cron expression, evaluated in UTC. Exactly one of
    /// `interval_secs` and `cron`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    /// The tasks its script may create; none when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<ObserverEffectRequest>,
    /// Secrets handed to the script.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<ObserverSecretRef>,
    /// Most tasks one run may create; the server's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_limit_per_run: Option<u32>,
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
    /// `create_task` or `none`.
    pub effect: String,
    /// The workflow of a "create a task" observer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<String>,
    /// The queue it serves.
    pub queue: String,
    /// `on_episodes` or `always`.
    pub mode: String,
    /// Seconds between runs, for an interval schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_secs: Option<u64>,
    /// The cron expression (UTC), for a cron schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    /// Most tasks one run may create.
    pub task_limit_per_run: u32,
    /// The secret references handed to the script (never values). Read
    /// only in the queues view: the owner sets them in the CLI (R34).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<ObserverSecretRef>,
    /// Workflow input name → effect data field, for a "create a task"
    /// observer. Read only in the queues view, as `secrets` (R34).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mapping: BTreeMap<String, String>,
    /// The only version the schedule runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_version: Option<u32>,
    /// The newest script version, approved or not. The schedule never runs
    /// it before the owner approves it: a view marks the observer while it
    /// differs from `approved_version` (R32, AE10).
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
    /// The latest finished run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<ObserverLastRunView>,
    /// Failed runs in a row.
    pub consecutive_failures: u32,
    /// What the server wants the owner to know (why an update paused it,
    /// say).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
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

/// `GET /control/v1/observers/{observer}/scripts/{version}`: one script
/// version with its text. A view shows it before the owner approves it
/// (R32) and opens it to edit the next version (R31).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverScriptTextView {
    /// Version number, from 1.
    pub version: u32,
    /// When it was saved.
    pub created_at: DateTime<Utc>,
    /// `sha256:` digest of the script.
    pub sha256: String,
    /// The shell script.
    pub script: String,
}

/// The execution error of one run that failed or was lost. A successful
/// run leaves none; the server keeps the newest few of each observer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverRunErrorView {
    /// The run (`{observer}.{sequence}`).
    pub run_id: String,
    /// Its sequence number.
    pub sequence: u64,
    /// The script version it ran.
    pub script_version: u32,
    /// Why it ended so (`failed`, `timed_out`, `invalid_output`,
    /// `worker_error`, `lost`).
    pub status: String,
    /// When a worker claimed it.
    pub claimed_at: DateTime<Utc>,
    /// When it ended, or was found lost.
    pub finished_at: DateTime<Utc>,
    /// The script's exit code, when it exited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// What went wrong.
    pub error: String,
    /// The script's stdout (masked, bounded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    /// The script's stderr (masked, bounded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    /// The queue entry the run checked, for an `on_episodes` run. A newer
    /// error of the same entry replaces this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
}

/// `GET /control/v1/observers/{observer}`: one observer in detail.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObserverDetailView {
    /// What `observer list` shows, with the secret references and the
    /// mapping.
    pub observer: ObserverView,
    /// Parameters handed to the script.
    pub condition: Value,
    /// Every script version, oldest first.
    #[serde(default)]
    pub scripts: Vec<ObserverScriptView>,
    /// The execution errors of failed or lost runs, newest first.
    #[serde(default)]
    pub errors: Vec<ObserverRunErrorView>,
}

/// `POST /control/v1/observers/{observer}/scripts`: the next script
/// version. Saving never approves it (AE6).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SetObserverScriptRequest {
    /// The shell script.
    pub script: String,
}

/// `POST /control/v1/observers/{observer}/approve`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApproveObserverRequest {
    /// The script version the schedule runs from now on.
    pub version: u32,
}

/// `PATCH /control/v1/observers/{observer}`: a new queue, mode or schedule
/// of an observer (R31, R35). A field left out stays as it is; at least one
/// must be given. `interval_secs` and `cron` replace the schedule, so at
/// most one of them may be given.
///
/// The observer must keep the mode of the queue it then serves (R26): the
/// owner moves or deletes the other observers of a queue first to change
/// the queue's mode.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UpdateObserverRequest {
    /// The queue it serves from now on, in the owner's namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<String>,
    /// Its run mode from now on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<ObserverMode>,
    /// A new interval schedule, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_secs: Option<u64>,
    /// A new cron schedule: five fields, evaluated in UTC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
}

impl UpdateObserverRequest {
    /// Whether the request changes nothing.
    pub fn is_empty(&self) -> bool {
        self.queue.is_none()
            && self.mode.is_none()
            && self.interval_secs.is_none()
            && self.cron.is_none()
    }
}

// ---------------------------------------------------------------------------
// The owner's queues (`/control/v1/observer-queues`)
// ---------------------------------------------------------------------------

/// One queue of the caller, as `observer queue list` shows it. A queue is a
/// name, not an object (R6): the caller's observers that serve it and the
/// entries of the caller's gates that name it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverQueueSummary {
    /// The queue's name.
    pub queue: String,
    /// The mode of its observers (`on_episodes` or `always`); absent when
    /// no observer serves it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// How many observers serve it.
    pub observers: u32,
    /// How many gate episodes wait on it.
    pub entries: u32,
}

/// `GET /control/v1/observer-queues`: the caller's queues, by name.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverQueueListResponse {
    /// By name.
    pub queues: Vec<ObserverQueueSummary>,
}

/// The newest error of one observer's runs for one queue entry (R23). A
/// newer error of the same entry replaces it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueEntryErrorView {
    /// The run (`{observer}.{sequence}`).
    pub run_id: String,
    /// Why it ended so (`failed`, `timed_out`, `invalid_output`,
    /// `worker_error`, `lost`).
    pub status: String,
    /// When it ended, or was found lost.
    pub finished_at: DateTime<Utc>,
    /// The script's exit code, when it exited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// What went wrong.
    pub error: String,
}

/// How one observer of the queue processed one entry, as far as the
/// control plane knows it (R23).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueEntryObserverView {
    /// The observer's name.
    pub observer: String,
    /// A run of the observer checks the entry now.
    #[serde(default)]
    pub running: bool,
    /// When a run of the observer for the entry last ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<DateTime<Utc>>,
    /// The newest error of the observer's runs for the entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<QueueEntryErrorView>,
}

/// One entry of a queue: one wait episode of a `decision` gate with a
/// `watch` block.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueEntryView {
    /// The entry's id, as a run error names it.
    pub entry_id: String,
    /// The task waiting at the gate.
    pub task_id: String,
    /// The workflow state of the gate.
    pub step: String,
    /// The wait episode.
    pub episode: String,
    /// When the episode began, as the control plane saw it.
    pub started_at: DateTime<Utc>,
    /// The gate's rendered params.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,
    /// Per observer of the queue, by name: how it processed the entry.
    #[serde(default)]
    pub observers: Vec<QueueEntryObserverView>,
}

/// `GET /control/v1/observer-queues/{queue}`: one queue of the caller, with
/// its observers and its entries. Another owner's queue of the same name
/// is not found (R7).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverQueueView {
    /// The queue's name.
    pub queue: String,
    /// The mode of its observers; absent when no observer serves it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Its observers, by name.
    #[serde(default)]
    pub observers: Vec<ObserverView>,
    /// Its entries, oldest first.
    #[serde(default)]
    pub entries: Vec<QueueEntryView>,
}
