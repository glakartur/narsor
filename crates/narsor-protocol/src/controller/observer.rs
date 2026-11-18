//! The observers' wire types:
//!
//! * the effect routes an observer's script calls through `narsor-cli`, with
//!   the token of its run (`/observer/v1/runs/{run}/…`);
//! * the worker routes `narsor-observer-worker` calls with the key the
//!   server gave it (`/observer/v1/worker/…`): claim a run, report its
//!   result;
//! * the read-only queue views of the owner (`/control/v1/observer-queues`):
//!   queues and automations come from the catalogue, entries, runs and
//!   errors from the state.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `POST /observer/v1/runs/{run}/tasks`: create a task from the effect data.
///
/// Only the data: the workflow, repositories, backend and
/// profile come from the queue file, and only the fields its mapping
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
/// Only a `check` run answers. The task and the episode must be those of
/// the entry the run was started for; a run of an `always` queue has none
/// and answers no gate. The
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
/// variable, as the automation declares it. The value comes from the
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
    /// The run (`{queue}~{automation}~{sequence}`).
    pub run_id: String,
    /// The script phase it runs (`in`, `check`, `out`).
    #[serde(default)]
    pub phase: String,
    /// The queue of the run's (queue, automation) pair.
    #[serde(default)]
    pub queue: String,
    /// The automation of the run's (queue, automation) pair.
    #[serde(default)]
    pub automation: String,
    /// The run's token: the script's `NARSOR_TOKEN` on the effect routes.
    pub token: String,
    /// `sha256:` digest of the script text, as the catalogue held it at the
    /// claim.
    #[serde(default)]
    pub script_digest: String,
    /// The script, run with `sh`.
    pub script: String,
    /// The document the script reads on stdin.
    pub input: Value,
    /// The secrets to hand to the script: the ones the automation
    /// declares, and no other.
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
            .field("phase", &self.phase)
            .field("queue", &self.queue)
            .field("automation", &self.automation)
            .field("token", &"<redacted>")
            .field("script_digest", &self.script_digest)
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
// Run modes
// ---------------------------------------------------------------------------

/// When the automations of a queue run (R25). The queue file sets it for
/// every automation of the queue.
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

// ---------------------------------------------------------------------------
// The read-only queue views (`/control/v1/observer-queues`)
// ---------------------------------------------------------------------------

/// One catalogue file that could not be used, and why. The queue views
/// list them; nothing in Narsor can change the file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogProblemView {
    /// The file or directory, relative to the catalogue root.
    pub file: String,
    /// Why it is not used.
    pub reason: String,
}

/// The latest finished run of one (queue, automation) pair.
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

/// The execution error of one run that failed or was lost. A successful
/// run leaves none; the server keeps the newest few of each pair.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverRunErrorView {
    /// The run (`{queue}~{automation}~{sequence}`).
    pub run_id: String,
    /// Its sequence number.
    pub sequence: u64,
    /// The script phase it ran (`in`, `check`, `out`).
    #[serde(default)]
    pub phase: String,
    /// `sha256:` digest of the script text it ran; empty when the script
    /// could not be read.
    #[serde(default)]
    pub script_digest: String,
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

/// One queue as `queue list` shows it: a queue of the catalogue, a queue
/// file that could not be used, or a queue name that only waiting gates
/// name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverQueueSummary {
    /// The queue's name.
    pub queue: String,
    /// `on_episodes` or `always`; absent when the catalogue has no usable
    /// queue of this name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Seconds between runs, for an interval schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_secs: Option<u64>,
    /// The cron expression (UTC), for a cron schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    /// The automations that serve it, in the queue file's order.
    #[serde(default)]
    pub automations: Vec<String>,
    /// How many gate episodes wait on it.
    pub entries: u32,
    /// Why the queue does not run: its file cannot be used, or the
    /// catalogue has no such queue. Absent for a queue that runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// `GET /control/v1/observer-queues`: the queues, by name, and every
/// catalogue file of automations and queues that could not be used.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverQueueListResponse {
    /// By name.
    pub queues: Vec<ObserverQueueSummary>,
    /// The files that failed to load, in path order.
    #[serde(default)]
    pub problems: Vec<CatalogProblemView>,
}

/// One automation of a queue: what the catalogue says of it, and the
/// runtime state of its (queue, automation) pair.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueAutomationView {
    /// The automation's name.
    pub automation: String,
    /// What it does.
    #[serde(default)]
    pub description: String,
    /// The scripts it has, in run order (`in`, `check`, `out`).
    #[serde(default)]
    pub scripts: Vec<String>,
    /// The secret references handed to its scripts (never values).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<ObserverSecretRef>,
    /// `active`, `needs_attention` (a run reached the task limit) or
    /// `error` (too many failed runs). A stopped pair runs again when its
    /// queue file, `automation.yaml` or `check` script changes.
    pub state: String,
    /// When the next scheduled run is due.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<DateTime<Utc>>,
    /// Since when a run waits for a worker to claim it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_for_claim_since: Option<DateTime<Utc>>,
    /// A run waited for a claim past the schedule's next fire: no worker
    /// seems to be polling.
    #[serde(default)]
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
    /// Failed runs in a row: the backoff of an `always` queue.
    #[serde(default)]
    pub consecutive_failures: u32,
    /// The kept execution errors of failed or lost runs, newest first.
    #[serde(default)]
    pub errors: Vec<ObserverRunErrorView>,
}

/// The newest error of one automation's runs for one queue entry (R23). A
/// newer error of the same entry replaces it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueEntryErrorView {
    /// The run (`{queue}~{automation}~{sequence}`).
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

/// How one automation of the queue processed one entry, as far as the
/// control plane knows it (R23).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueEntryAutomationView {
    /// The automation's name.
    pub automation: String,
    /// A run of the automation checks the entry now.
    #[serde(default)]
    pub running: bool,
    /// When a run of the automation for the entry last ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<DateTime<Utc>>,
    /// The newest error of the automation's runs for the entry.
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
    /// Per automation of the queue, in the queue's order: how it processed
    /// the entry.
    #[serde(default)]
    pub automations: Vec<QueueEntryAutomationView>,
}

/// `GET /control/v1/observer-queues/{queue}`: one queue with its
/// automations, its entries and the problems of its file. Read only: the
/// catalogue is the only source of its configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverQueueView {
    /// The queue's name.
    pub queue: String,
    /// `on_episodes` or `always`; absent when the catalogue has no usable
    /// queue of this name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Seconds between runs, for an interval schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_secs: Option<u64>,
    /// The cron expression (UTC), for a cron schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    /// The workflow an `always` queue creates tasks of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<String>,
    /// Its automations, in the queue file's order.
    #[serde(default)]
    pub automations: Vec<QueueAutomationView>,
    /// Its entries, oldest first.
    #[serde(default)]
    pub entries: Vec<QueueEntryView>,
    /// Why the queue's file cannot be used, or why the catalogue has no
    /// such queue. Empty for a queue that runs.
    #[serde(default)]
    pub problems: Vec<CatalogProblemView>,
}
