//! The observers' routes outside the owner API:
//!
//! * the effect routes an observer's script calls through `narsor-cli`, with
//!   the token of its run (`/observer/v1/runs/{run}/…`);
//! * the worker routes `narsor-cli observer worker` calls with its own
//!   credential (`/observer/v1/worker/…`): claim a run, report its result;
//! * the owner routes that create and revoke that credential
//!   (`/control/v1/observer-workers`).

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
