//! Records of catalogue automations: the runtime record of one (queue,
//! automation) pair (with its run in progress), the execution errors of its
//! runs and the entries of observer queues.
//!
//! Everything here is plain data. What a queue or an automation does comes
//! from the catalogue ([`crate::automations`]); this module keeps only what
//! the schedule learns at run time. The durable layout and the locks live
//! in [`super::store`]; the scheduling and the effect routes that mutate
//! these records belong to the control-plane service.
//!
//! Every field added after version 1 of a record carries `#[serde(default)]`,
//! so a record written by an older build still loads. Enums that a client may
//! read end in an `Unrecognized` fallback that this build never writes.

use super::schedule_expr::CronSchedule;
use super::{ObserverError, Result};
use crate::automations::HookPhase;
use crate::domain::observer_name::{name_like_problem, queue_name_problem};
use crate::domain::{BackendId, TaskId, WorkflowDefinition};
use chrono::{DateTime, Utc};
pub use narsor_protocol::controller::ObserverMode;
use narsor_protocol::controller::{
    CreateTaskRequest, GateWatch, GateWatchChoice, GateWatchVariable, LaunchRequest, OwnerRef,
    WorkspaceRequest,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Current schema version of [`AutomationRecord`].
pub const AUTOMATION_RECORD_VERSION: u32 = 1;
/// Current schema version of [`RunFailure`].
pub const RUN_FAILURE_VERSION: u32 = 1;
/// Current schema version of [`QueueEntry`].
pub const QUEUE_ENTRY_VERSION: u32 = 1;

/// Interval between two scheduled runs when the owner names none (5 min).
pub const DEFAULT_INTERVAL_SECS: u64 = 300;
/// Shortest interval a queue may have (R17), and the shortest gap
/// between two fires of a cron schedule (R28): it protects the API limits
/// of the source.
pub const MIN_INTERVAL_SECS: u64 = 60;
/// Longest interval a queue may have (30 days). Keeps every deadline the
/// scheduler computes far inside what a timestamp can hold.
pub const MAX_INTERVAL_SECS: u64 = 30 * 24 * 60 * 60;

/// Tasks one run may create when the owner names no limit (R15).
pub const DEFAULT_TASK_LIMIT_PER_RUN: u32 = 10;
/// Highest task limit per run an owner may configure.
pub const MAX_TASK_LIMIT_PER_RUN: u32 = 1000;

/// Largest condition, serialized as JSON.
pub const MAX_CONDITION_BYTES: usize = 64 * 1024;
/// Most secret references one automation may hold.
pub const MAX_SECRET_REFS: usize = 32;
/// Largest script version.
pub const MAX_SCRIPT_BYTES: usize = 256 * 1024;
/// Longest episode id of a queue entry.
pub const MAX_EPISODE_LEN: usize = 128;
/// Most params of a queue entry.
pub const MAX_ENTRY_PARAMS: usize = 64;
/// Most bytes of a queue entry's params, names and values together.
pub const MAX_ENTRY_PARAMS_BYTES: usize = 16 * 1024;

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Validates a name-like value ([`name_like_problem`]).
fn validate_name_like(field: &'static str, value: &str, allow_dot: bool) -> Result<()> {
    match name_like_problem(value, allow_dot) {
        None => Ok(()),
        Some(reason) => Err(ObserverError::invalid(field, reason)),
    }
}

/// The separator of the parts of a [`RunId`]: no queue or automation name
/// holds it, and it needs no escaping in a URL path.
const RUN_ID_SEPARATOR: char = '~';

/// One (queue, automation) pair of the catalogue: the scheduling unit, and
/// the key of its runtime record (R2).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AutomationKey {
    /// The queue, a catalogue file stem.
    pub queue: String,
    /// The automation, a catalogue directory name.
    pub automation: String,
}

impl AutomationKey {
    /// The pair of `queue` and `automation`, both checked as the
    /// catalogue checks them: safe path components.
    pub fn new(queue: impl Into<String>, automation: impl Into<String>) -> Result<Self> {
        let key = Self {
            queue: queue.into(),
            automation: automation.into(),
        };
        key.validate()?;
        Ok(key)
    }

    /// Refuses a queue or automation name that is no safe path component.
    pub fn validate(&self) -> Result<()> {
        if let Some(reason) = queue_name_problem(&self.queue) {
            return Err(ObserverError::invalid("queue", reason));
        }
        validate_name_like("automation", &self.automation, true)
    }
}

impl fmt::Display for AutomationKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.queue, self.automation)
    }
}

/// One run of one (queue, automation) pair:
/// `{queue}~{automation}~{sequence}`, on the wire as that string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RunId {
    /// The pair.
    pub key: AutomationKey,
    /// The run's sequence number within the pair, from 1.
    pub sequence: u64,
}

impl RunId {
    /// The run `sequence` of `key`.
    pub fn new(key: AutomationKey, sequence: u64) -> Self {
        Self { key, sequence }
    }

    /// Parses `{queue}~{automation}~{sequence}`.
    pub fn parse(value: &str) -> Result<Self> {
        let refusal = || ObserverError::invalid("run id", format!("'{value}' is not a run id"));
        let mut parts = value.split(RUN_ID_SEPARATOR);
        let (Some(queue), Some(automation), Some(sequence), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(refusal());
        };
        let key = AutomationKey::new(queue, automation).map_err(|_| refusal())?;
        if sequence.is_empty() || !sequence.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(refusal());
        }
        let sequence = sequence.parse().map_err(|_| refusal())?;
        Ok(Self::new(key, sequence))
    }
}

impl TryFrom<String> for RunId {
    type Error = ObserverError;

    fn try_from(value: String) -> Result<Self> {
        Self::parse(&value)
    }
}

impl From<RunId> for String {
    fn from(value: RunId) -> Self {
        value.to_string()
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}{RUN_ID_SEPARATOR}{}{RUN_ID_SEPARATOR}{}",
            self.key.queue, self.key.automation, self.sequence
        )
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// When the automations of a queue run (R28): an interval, or a five-field cron
/// expression in UTC. Exactly one of the two.
///
/// On disk and on the wire `{"interval_secs": 300}` or
/// `{"cron": "*/10 * * * *"}`: a record written before cron existed holds
/// only `interval_secs` and reads unchanged. A stored schedule with neither
/// field reads as the default interval, as it did before; a new observer
/// must name one ([`ScheduleConfig::from_parts`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ScheduleFields", into = "ScheduleFields")]
pub enum ScheduleConfig {
    /// Seconds between two scheduled runs; at least [`MIN_INTERVAL_SECS`].
    Interval {
        /// The interval.
        secs: u64,
    },
    /// The fire times of a cron expression.
    Cron(CronSchedule),
}

/// The stored form of a [`ScheduleConfig`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ScheduleFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    interval_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cron: Option<CronSchedule>,
}

impl TryFrom<ScheduleFields> for ScheduleConfig {
    type Error = ObserverError;

    fn try_from(fields: ScheduleFields) -> Result<Self> {
        match (fields.interval_secs, fields.cron) {
            (None, None) => Ok(Self::default()),
            (interval_secs, cron) => Self::from_parts(interval_secs, cron.map(String::from)),
        }
    }
}

impl From<ScheduleConfig> for ScheduleFields {
    fn from(schedule: ScheduleConfig) -> Self {
        match schedule {
            ScheduleConfig::Interval { secs } => Self {
                interval_secs: Some(secs),
                cron: None,
            },
            ScheduleConfig::Cron(cron) => Self {
                interval_secs: None,
                cron: Some(cron),
            },
        }
    }
}

/// `secs` seconds; a count too large for a duration is the largest one.
pub fn secs_duration(secs: u64) -> chrono::Duration {
    i64::try_from(secs)
        .ok()
        .and_then(chrono::Duration::try_seconds)
        .unwrap_or(chrono::Duration::MAX)
}

/// `at` plus `secs` seconds, the latest instant when that is too far.
fn after_secs(at: DateTime<Utc>, secs: u64) -> DateTime<Utc> {
    at.checked_add_signed(secs_duration(secs))
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self::Interval {
            secs: DEFAULT_INTERVAL_SECS,
        }
    }
}

impl ScheduleConfig {
    /// The schedule an owner asks for: exactly one of an interval and a
    /// cron expression, each checked.
    pub fn from_parts(interval_secs: Option<u64>, cron: Option<String>) -> Result<Self> {
        let schedule = match (interval_secs, cron) {
            (Some(_), Some(_)) => {
                return Err(ObserverError::invalid(
                    "schedule",
                    "give either an interval or a cron expression, not both",
                ))
            }
            (None, None) => {
                return Err(ObserverError::invalid(
                    "schedule",
                    "give an interval or a cron expression",
                ))
            }
            (Some(secs), None) => Self::Interval { secs },
            (None, Some(cron)) => Self::Cron(CronSchedule::parse(&cron)?),
        };
        schedule.validate()?;
        Ok(schedule)
    }

    /// The interval in seconds, for an interval schedule.
    pub fn interval_secs(&self) -> Option<u64> {
        match self {
            Self::Interval { secs } => Some(*secs),
            Self::Cron(_) => None,
        }
    }

    /// The cron expression, for a cron schedule.
    pub fn cron(&self) -> Option<&CronSchedule> {
        match self {
            Self::Interval { .. } => None,
            Self::Cron(cron) => Some(cron),
        }
    }

    /// When the schedule next fires after `now`: one interval on, or the
    /// cron expression's next fire time (the latest instant when it has
    /// none).
    pub fn next_after(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        match self {
            Self::Interval { secs } => after_secs(now, *secs),
            Self::Cron(cron) => cron.next_after(now).unwrap_or(DateTime::<Utc>::MAX_UTC),
        }
    }

    /// The schedule's next tick after the tick `tick` that is also after
    /// `now` (KTD-3): the next due time counts from the last tick, not from
    /// the end of a run, and ticks that passed meanwhile are skipped, never
    /// owed. An interval adds whole intervals to `tick`, so a late tick does
    /// not shift the ones after it; a cron expression fires at its own
    /// times.
    pub fn next_tick_after(&self, tick: DateTime<Utc>, now: DateTime<Utc>) -> DateTime<Utc> {
        match self {
            Self::Interval { secs } => {
                let secs = (*secs).max(1);
                let behind = u64::try_from((now - tick).num_seconds()).unwrap_or(0);
                let periods = (behind / secs).saturating_add(1);
                after_secs(tick, secs.saturating_mul(periods))
            }
            Self::Cron(cron) => cron
                .next_after(tick.max(now))
                .unwrap_or(DateTime::<Utc>::MAX_UTC),
        }
    }

    /// The first fire at or after `delay_secs` from `now`, and never before
    /// [`Self::next_after`]: an interval schedule waits exactly
    /// `delay_secs`, a cron schedule waits for its first fire time from
    /// then on. The backoff after failed runs uses it.
    pub fn first_fire_from(&self, now: DateTime<Utc>, delay_secs: u64) -> DateTime<Utc> {
        let next = self.next_after(now);
        match self {
            Self::Interval { .. } => after_secs(now, delay_secs).max(next),
            Self::Cron(cron) => {
                let earliest = after_secs(now, delay_secs);
                // Fires are on whole minutes: one second before is "at or
                // after" for the strict next fire.
                let from = earliest - chrono::Duration::seconds(1);
                cron.next_after(from)
                    .unwrap_or(DateTime::<Utc>::MAX_UTC)
                    .max(next)
            }
        }
    }

    /// Seconds from `now` to the next fire: the interval itself, or the gap
    /// to the cron expression's next fire time.
    pub fn period_secs(&self, now: DateTime<Utc>) -> u64 {
        match self {
            Self::Interval { secs } => *secs,
            Self::Cron(_) => {
                u64::try_from((self.next_after(now) - now).num_seconds()).unwrap_or(u64::MAX)
            }
        }
    }

    /// Refuses an interval outside [`MIN_INTERVAL_SECS`]..=[`MAX_INTERVAL_SECS`].
    /// A cron expression was checked when it was parsed.
    pub fn validate(&self) -> Result<()> {
        let Self::Interval { secs } = self else {
            return Ok(());
        };
        if *secs < MIN_INTERVAL_SECS {
            return Err(ObserverError::invalid(
                "interval",
                format!(
                    "{secs} seconds is too short: the interval must be at least {MIN_INTERVAL_SECS} seconds"
                ),
            ));
        }
        if *secs > MAX_INTERVAL_SECS {
            return Err(ObserverError::invalid(
                "interval",
                format!(
                    "{secs} seconds is too long: the interval must be at most {MAX_INTERVAL_SECS} seconds"
                ),
            ));
        }
        Ok(())
    }
}

/// The "create a task" effect: what every task an `always` queue creates runs.
///
/// The script supplies only the effect data. The workflow,
/// repositories, backend and profile come from here, and only the fields
/// named by `mapping` become workflow inputs, so a script cannot choose
/// another workflow or backend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateTaskEffect {
    /// Workflow to run.
    pub workflow_id: String,
    /// Workflow version the queue was validated against.
    pub workflow_version: u32,
    /// Repository choices for the workspaces of the workflow, by workspace
    /// name, as in a task create request.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub workspaces: BTreeMap<String, WorkspaceRequest>,
    /// Backend the tasks are pinned to; the owner's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_id: Option<String>,
    /// Environment profile; the owner's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Workflow input name → field name in the effect data.
    #[serde(default)]
    pub mapping: BTreeMap<String, String>,
}

impl CreateTaskEffect {
    /// Checks what can be checked without the workflow: names and backend.
    pub fn validate(&self) -> Result<()> {
        if self.workflow_id.trim().is_empty() {
            return Err(ObserverError::invalid(
                "effect.workflow_id",
                "the workflow must be named",
            ));
        }
        if let Some(backend) = &self.backend_id {
            BackendId::parse(backend.clone())
                .map_err(|error| ObserverError::invalid("effect.backend_id", error.to_string()))?;
        }
        for (input, field) in &self.mapping {
            if input.trim().is_empty() || field.trim().is_empty() {
                return Err(ObserverError::invalid(
                    "effect.mapping",
                    "input names and data field names must not be empty",
                ));
            }
        }
        Ok(())
    }

    /// The create request of the task the effect `data` yields (R3): this
    /// effect's workflow, repositories, backend and profile, a random task
    /// id, and as inputs the data fields the mapping names.
    ///
    /// The data must be a JSON object (or absent) holding no field the
    /// mapping does not name: the script cannot smuggle in inputs. A mapped
    /// field missing from the data is left out of the inputs.
    pub fn task_request(&self, data: &serde_json::Value) -> Result<CreateTaskRequest> {
        let empty = serde_json::Map::new();
        let fields = match data {
            serde_json::Value::Null => &empty,
            serde_json::Value::Object(fields) => fields,
            _ => {
                return Err(ObserverError::invalid(
                    "effect data",
                    "must be a JSON object of the fields the queue's mapping names",
                ))
            }
        };
        let mapped: BTreeSet<&str> = self.mapping.values().map(String::as_str).collect();
        let unmapped: Vec<String> = fields
            .keys()
            .filter(|field| !mapped.contains(field.as_str()))
            .map(|field| format!("'{field}'"))
            .collect();
        if !unmapped.is_empty() {
            return Err(ObserverError::invalid(
                "effect data",
                format!(
                    "field {} is not in the queue's mapping",
                    unmapped.join(", ")
                ),
            ));
        }
        let inputs: serde_json::Map<String, serde_json::Value> = self
            .mapping
            .iter()
            .filter_map(|(input, field)| {
                fields
                    .get(field)
                    .map(|value| (input.clone(), value.clone()))
            })
            .collect();
        Ok(CreateTaskRequest {
            task_id: None,
            backend_id: self.backend_id.clone(),
            launch: LaunchRequest {
                workflow_id: self.workflow_id.clone(),
                workflow_version: self.workflow_version,
                inputs: serde_json::Value::Object(inputs),
                workspaces: self.workspaces.clone(),
                profile: self.profile.clone(),
                harness: None,
                resources: None,
                ticket: None,
                title: None,
            },
        })
    }

    /// Checks the effect against the workflow it names: same id and version,
    /// and every mapped input declared by the workflow.
    pub fn validate_against(&self, workflow: &WorkflowDefinition) -> Result<()> {
        if workflow.id != self.workflow_id || workflow.version != self.workflow_version {
            return Err(ObserverError::invalid(
                "effect.workflow_id",
                format!(
                    "the effect names workflow '{}' version {}, but '{}' version {} was given",
                    self.workflow_id, self.workflow_version, workflow.id, workflow.version
                ),
            ));
        }
        let declared: BTreeSet<&str> = workflow
            .inputs
            .iter()
            .map(|input| input.name.as_str())
            .collect();
        let unknown: Vec<&str> = self
            .mapping
            .keys()
            .map(String::as_str)
            .filter(|input| !declared.contains(input))
            .collect();
        if unknown.is_empty() {
            Ok(())
        } else {
            Err(ObserverError::invalid(
                "effect.mapping",
                format!(
                    "workflow '{}' has no input {}",
                    self.workflow_id,
                    unknown
                        .iter()
                        .map(|input| format!("'{input}'"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ))
        }
    }
}

/// One secret the worker hands to the script as an environment variable.
///
/// The control plane holds only the reference; the value comes from the
/// worker's secrets directory, one file per `name` (R20).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretRef {
    /// Name of the secret in the worker's secrets directory (a file name).
    pub name: String,
    /// Environment variable the script reads it from.
    pub env: String,
}

/// Environment variables the worker sets itself, which a secret may not
/// shadow. Every `NARSOR_*` variable is reserved as well.
const RESERVED_ENVS: &[&str] = &["PATH", "HOME"];

impl SecretRef {
    /// Refuses an unsafe file name or an environment variable the worker
    /// owns.
    pub fn validate(&self) -> Result<()> {
        validate_name_like("secrets.name", &self.name, true)?;
        let env = self.env.as_bytes();
        let env_valid = !env.is_empty()
            && (env[0].is_ascii_uppercase() || env[0] == b'_')
            && env
                .iter()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_');
        if !env_valid {
            return Err(ObserverError::invalid(
                "secrets.env",
                format!(
                    "'{}' is not valid: use [A-Z_][A-Z0-9_]* for an environment variable",
                    self.env
                ),
            ));
        }
        if RESERVED_ENVS.contains(&self.env.as_str()) || self.env.starts_with("NARSOR_") {
            return Err(ObserverError::invalid(
                "secrets.env",
                format!("'{}' is reserved for the worker", self.env),
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The runtime record of a (queue, automation) pair
// ---------------------------------------------------------------------------

/// Where a (queue, automation) pair stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationState {
    /// Runs on the queue's schedule.
    #[default]
    Active,
    /// A run went over the queue's task limit (R15): stopped until the
    /// automation's `check` script changes.
    NeedsAttention,
    /// Too many runs in a row failed (R16): stopped until the automation's
    /// `check` script changes.
    Error,
    /// A state from a newer build. Never written by this one.
    #[serde(other)]
    Unrecognized,
}

impl AutomationState {
    /// Whether the pair waits for a change of its `check` script.
    pub fn is_stopped(self) -> bool {
        matches!(self, Self::NeedsAttention | Self::Error)
    }
}

/// The latest finished run, as the queue view shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastRun {
    /// Its sequence number.
    pub sequence: u64,
    /// How it ended.
    pub status: RunStatus,
    /// When it ended.
    pub finished_at: DateTime<Utc>,
    /// Why it failed, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The run a worker claimed and did not report yet, as the pair's record
/// holds it. Nothing else of a run is stored unless it fails
/// ([`RunFailure`]); its input is built at the claim and handed to the
/// worker only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveRun {
    /// Sequence number within the pair, from 1.
    pub sequence: u64,
    /// The script phase it runs. Only a `check` run answers a gate.
    pub phase: HookPhase,
    /// `sha256:` digest of the script text it runs, as the catalogue held
    /// it at the claim.
    pub script_digest: String,
    /// When a worker claimed it.
    pub claimed_at: DateTime<Utc>,
    /// No result by then: the run is lost and its token stops working.
    pub deadline: DateTime<Utc>,
    /// `sha256:` digest of the run token.
    pub token_digest: String,
    /// Tasks this run created so far, checked against the limit (R15).
    #[serde(default)]
    pub tasks_created: u32,
    /// The queue entry the run checks, for an `on_episodes` queue (one
    /// entry per run, R30).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<RunEntry>,
}

/// The queue entry one run checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEntry {
    /// The entry's id, for the run's error (R23).
    pub entry_id: String,
    /// The task waiting at the gate.
    pub task_id: TaskId,
    /// The wait episode the entry is for: an answer of the run must name it.
    pub episode: String,
}

impl ActiveRun {
    /// The run's id within `key`.
    pub fn run_id(&self, key: &AutomationKey) -> RunId {
        RunId::new(key.clone(), self.sequence)
    }

    /// Whether the run passed its deadline at `now`: lost, its token spent.
    pub fn is_past_deadline(&self, now: DateTime<Utc>) -> bool {
        now >= self.deadline
    }
}

/// The durable runtime record of one (queue, automation) pair:
/// `observer-queues/automations/{queue}/{automation}/state.json`.
///
/// Created on the first pass that finds the pair in the catalogue, deleted
/// on a pass that finds it gone and no run in progress (R2). The
/// configuration is never here: the catalogue holds it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutomationRecord {
    /// Schema version, [`AUTOMATION_RECORD_VERSION`].
    pub record_version: u32,
    /// The pair.
    #[serde(flatten)]
    pub key: AutomationKey,
    /// When it was created.
    pub created_at: DateTime<Utc>,
    /// When it was last changed.
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    /// Incremented by every save.
    #[serde(default)]
    pub revision: u64,
    /// Where it stands.
    #[serde(default)]
    pub state: AutomationState,
    /// The digest of the `check` script when the pair stopped
    /// (`needs_attention`, `error`): a new script puts it back on its
    /// schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_script_digest: Option<String>,
    /// The state the last successful run returned, handed to the next one.
    #[serde(default)]
    pub script_state: Option<serde_json::Value>,
    /// Whether a run has succeeded since the record was created.
    #[serde(default)]
    pub first_run_done: bool,
    /// When the next scheduled run is due.
    #[serde(default)]
    pub next_run_at: Option<DateTime<Utc>>,
    /// Set when a scheduled run became due and no worker claimed it yet:
    /// the "run due" flag, and since when it is up.
    #[serde(default)]
    pub run_due_since: Option<DateTime<Utc>>,
    /// The ids of the queue entries still to check in the current round of
    /// an `on_episodes` queue, in order: each run takes the first (R30).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entry_round: Vec<String>,
    /// The run in progress, if any: at most one per pair.
    #[serde(default)]
    pub active_run: Option<ActiveRun>,
    /// When a worker last claimed a run.
    #[serde(default)]
    pub last_claim_at: Option<DateTime<Utc>>,
    /// Sequence number of the latest run created; the next one is one more.
    #[serde(default)]
    pub last_run_sequence: u64,
    /// The latest finished run.
    #[serde(default)]
    pub last_run: Option<LastRun>,
    /// When a run of this pair for each queue entry (by id) last ended,
    /// whatever its outcome: what the queues view shows of an entry next to
    /// its newest error (R23). Holds the entries of the current round only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub entry_last_runs: BTreeMap<String, DateTime<Utc>>,
    /// Failed runs in a row since the last success.
    #[serde(default)]
    pub consecutive_failures: u32,
}

impl AutomationRecord {
    /// A new active record, due at once.
    pub fn new(key: AutomationKey, now: DateTime<Utc>) -> Self {
        Self {
            record_version: AUTOMATION_RECORD_VERSION,
            key,
            created_at: now,
            updated_at: None,
            revision: 0,
            state: AutomationState::Active,
            stopped_script_digest: None,
            script_state: None,
            first_run_done: false,
            next_run_at: None,
            run_due_since: None,
            entry_round: Vec::new(),
            active_run: None,
            last_claim_at: None,
            last_run_sequence: 0,
            last_run: None,
            entry_last_runs: BTreeMap::new(),
            consecutive_failures: 0,
        }
    }

    /// Since when a scheduled run waits for a worker to claim it; `None`
    /// when none waits (a run in progress does not wait).
    pub fn waiting_for_claim_since(&self) -> Option<DateTime<Utc>> {
        self.run_due_since.filter(|_| self.active_run.is_none())
    }

    /// Whether a run waited for a claim past `schedule`'s next fire at
    /// `now`: no worker seems to be polling, which the queue views warn
    /// about.
    pub fn worker_missing(&self, schedule: &ScheduleConfig, now: DateTime<Utc>) -> bool {
        self.waiting_for_claim_since()
            .is_some_and(|since| now > schedule.next_after(since))
    }
}

// ---------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------

/// How a run ended, or that it did not yet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Claimed by a worker; no result yet.
    #[default]
    Running,
    /// The script ended as the contract allows: exit 0 with a valid output
    /// document (an observer that creates tasks), or exit 0 or 1 (a run
    /// for a queue entry).
    Succeeded,
    /// The script exited with another code.
    Failed,
    /// The script ran past its time limit.
    TimedOut,
    /// The script exited 0 but its output broke the contract.
    InvalidOutput,
    /// The worker could not run the script (a missing secret, say).
    WorkerError,
    /// No result came before the run's deadline: the worker was lost. Not a
    /// failure of the script, so it does not count towards the error state.
    Lost,
    /// A status from a newer build. Never written by this one.
    #[serde(other)]
    Unrecognized,
}

/// The execution error of one run:
/// `observer-queues/automations/{queue}/{automation}/errors/{sequence:08}.json`.
///
/// Written only for a run that did not succeed (a failure, an invalid
/// output, a time-out, a worker error) or was lost. A successful run leaves
/// no file. The store keeps the newest [`super::store::KEPT_RUN_FAILURES`]
/// of each pair, and only the newest of each queue entry (R23).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFailure {
    /// Schema version, [`RUN_FAILURE_VERSION`].
    pub record_version: u32,
    /// The pair.
    #[serde(flatten)]
    pub key: AutomationKey,
    /// Sequence number of the run within the pair.
    pub sequence: u64,
    /// The script phase it ran.
    pub phase: HookPhase,
    /// `sha256:` digest of the script text it ran; empty when the script
    /// could not be read at the claim.
    #[serde(default)]
    pub script_digest: String,
    /// When a worker claimed it.
    pub claimed_at: DateTime<Utc>,
    /// When it ended (the result came, or it was found lost).
    pub finished_at: DateTime<Utc>,
    /// Why it ended so: never [`RunStatus::Running`] or
    /// [`RunStatus::Succeeded`].
    pub status: RunStatus,
    /// The script's exit code, when it exited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// What went wrong.
    pub error: String,
    /// The script's stdout as the worker sent it (masked, bounded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    /// The script's stderr as the worker sent it (masked, bounded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    /// The queue entry the run checked, for an `on_episodes` run. A newer
    /// error of the same entry replaces this one (R23).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
}

impl RunFailure {
    /// The run's id.
    pub fn run_id(&self) -> RunId {
        RunId::new(self.key.clone(), self.sequence)
    }
}

/// Hex SHA-256 of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

// ---------------------------------------------------------------------------
// Queue entries
// ---------------------------------------------------------------------------

/// One wait episode of a `decision` gate on an observer queue:
/// `observer-queues/entries/{entry_id}.json`.
///
/// Keyed by task and episode (KTD-4): one episode has one entry, and a new
/// episode of the same gate (a review loop) has a new one. The entry keeps
/// the task's owner; the queue is a catalogue queue. The entry holds what the
/// gate reported when the episode began: the rendered params and the
/// choices and variables an answer may use. It is never changed, only
/// created and removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueEntry {
    /// Schema version, [`QUEUE_ENTRY_VERSION`].
    pub record_version: u32,
    /// [`queue_entry_id`] of the task and episode.
    pub entry_id: String,
    /// The task's owner: the namespace of the queue.
    pub owner: OwnerRef,
    /// The queue, as the gate's `watch` names it.
    pub queue: String,
    /// The task waiting at the gate.
    pub task_id: TaskId,
    /// The wait episode: the id of the gate's pending input request.
    pub episode: String,
    /// The workflow state of the gate.
    pub step: String,
    /// The gate's rendered params.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,
    /// The gate's choices.
    #[serde(default)]
    pub choices: Vec<GateWatchChoice>,
    /// The variables an answer may give.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variables: Vec<GateWatchVariable>,
    /// When it was added: the episode's start as the control plane saw it.
    pub created_at: DateTime<Utc>,
}

impl QueueEntry {
    /// The entry of episode `episode` of the task `task_id` of `owner`,
    /// waiting at the gate `gate` reported.
    pub fn from_gate(
        owner: OwnerRef,
        task_id: TaskId,
        episode: impl Into<String>,
        gate: &GateWatch,
        now: DateTime<Utc>,
    ) -> Self {
        let episode = episode.into();
        Self {
            record_version: QUEUE_ENTRY_VERSION,
            entry_id: queue_entry_id(&task_id, &episode),
            owner,
            queue: gate.queue.clone(),
            task_id,
            episode,
            step: gate.step.clone(),
            params: gate.params.clone(),
            choices: gate.choices.clone(),
            variables: gate.variables.clone(),
            created_at: now,
        }
    }

    /// Whether the entry is in `owner`'s queue `queue`.
    pub fn is_in(&self, owner: &OwnerRef, queue: &str) -> bool {
        &self.owner == owner && self.queue == queue
    }

    /// Refuses an id that is not the entry's own, a bad queue name, an
    /// empty or oversized episode, or oversized params.
    pub fn validate(&self) -> Result<()> {
        let refusal = |reason: String| ObserverError::invalid("queue entry", reason);
        if self.entry_id != queue_entry_id(&self.task_id, &self.episode) {
            return Err(refusal(
                "the entry id does not match its task and episode".into(),
            ));
        }
        if let Some(reason) = queue_name_problem(&self.queue) {
            return Err(ObserverError::invalid("queue", reason));
        }
        if self.episode.is_empty()
            || self.episode.len() > MAX_EPISODE_LEN
            || self.episode.chars().any(char::is_control)
        {
            return Err(refusal(format!(
                "the episode must have 1-{MAX_EPISODE_LEN} characters and no control characters"
            )));
        }
        if self.step.is_empty() {
            return Err(refusal("the step must be named".into()));
        }
        let params_bytes: usize = self
            .params
            .iter()
            .map(|(name, value)| name.len() + value.len())
            .sum();
        if self.params.len() > MAX_ENTRY_PARAMS || params_bytes > MAX_ENTRY_PARAMS_BYTES {
            return Err(refusal(format!(
                "at most {MAX_ENTRY_PARAMS} params of {MAX_ENTRY_PARAMS_BYTES} bytes together"
            )));
        }
        Ok(())
    }
}

/// The id of the queue entry of `task_id` and `episode`: stable for the
/// pair, different for a new episode of the same task (AE3).
pub fn queue_entry_id(task_id: &TaskId, episode: &str) -> String {
    // A task id never holds a newline.
    sha256_hex(format!("{task_id}\n{episode}").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_id_round_trips_with_dotted_names_and_refuses_garbage() {
        let key = AutomationKey::new("mr.review", "gitlab-mr.label").unwrap();
        let run = RunId::new(key, 42);
        assert_eq!(run.to_string(), "mr.review~gitlab-mr.label~42");
        assert_eq!(RunId::parse("mr.review~gitlab-mr.label~42").unwrap(), run);
        assert_eq!(
            serde_json::to_value(&run).unwrap(),
            "mr.review~gitlab-mr.label~42"
        );
        for value in [
            "q~a", "q~a~", "q~a~x", "~a~1", "q~~1", "q~a~+4", "q~a~1~2", "q/x~a~1", "q~.a~1",
        ] {
            assert!(RunId::parse(value).is_err(), "{value}");
        }
    }

    #[test]
    fn a_queue_entry_id_is_stable_per_episode_and_new_for_the_next() {
        let task = TaskId::parse("task-1").unwrap();
        let first = queue_entry_id(&task, "input-1");
        assert_eq!(first, queue_entry_id(&task, "input-1"));
        assert_ne!(first, queue_entry_id(&task, "input-2"));
        assert_ne!(
            first,
            queue_entry_id(&TaskId::parse("task-2").unwrap(), "input-1")
        );
        assert_eq!(first.len(), 64);
    }

    #[test]
    fn a_secret_may_not_shadow_a_worker_variable() {
        let secret = |env: &str| SecretRef {
            name: "jira-token".into(),
            env: env.into(),
        };
        assert!(secret("JIRA_TOKEN").validate().is_ok());
        for env in ["PATH", "HOME", "NARSOR_TOKEN", "lower", "1X", ""] {
            assert!(secret(env).validate().is_err(), "{env}");
        }
        let unsafe_name = SecretRef {
            name: "../token".into(),
            env: "TOKEN".into(),
        };
        assert!(unsafe_name.validate().is_err());
    }

    fn gate() -> GateWatch {
        GateWatch {
            step: "review_work".into(),
            queue: "mr-review".into(),
            params: [("mr_url".to_string(), "https://x/1".to_string())].into(),
            choices: vec![GateWatchChoice {
                name: "check_feedback".into(),
                description: None,
                requires_variables: Vec::new(),
            }],
            variables: Vec::new(),
        }
    }

    #[test]
    fn a_queue_entry_takes_the_gate_and_checks_itself() {
        let now = Utc::now();
        let task = TaskId::parse("task-1").unwrap();
        let owner = OwnerRef("alice".into());
        let entry = QueueEntry::from_gate(owner.clone(), task.clone(), "input-1", &gate(), now);
        assert!(entry.validate().is_ok());
        assert!(entry.is_in(&owner, "mr-review"));
        assert!(!entry.is_in(&OwnerRef("bob".into()), "mr-review"));
        assert_eq!(entry.params["mr_url"], "https://x/1");

        let mut forged = entry.clone();
        forged.episode = "input-2".into();
        assert!(forged.validate().is_err());
        let blank = QueueEntry::from_gate(owner.clone(), task.clone(), "", &gate(), now);
        assert!(blank.validate().is_err());
        let mut bad_queue = gate();
        bad_queue.queue = "no spaces".into();
        let entry = QueueEntry::from_gate(owner.clone(), task.clone(), "input-1", &bad_queue, now);
        assert!(entry.validate().is_err());
        let mut big = gate();
        big.params = (0..=MAX_ENTRY_PARAMS)
            .map(|index| (format!("p{index}"), String::new()))
            .collect();
        let entry = QueueEntry::from_gate(owner, task, "input-1", &big, now);
        assert!(entry.validate().is_err());
    }

    #[test]
    fn a_schedule_is_an_interval_or_a_cron_expression_never_both() {
        assert_eq!(
            ScheduleConfig::from_parts(Some(120), None).unwrap(),
            ScheduleConfig::Interval { secs: 120 }
        );
        let cron = ScheduleConfig::from_parts(None, Some("*/10 * * * *".into())).unwrap();
        assert_eq!(cron.cron().unwrap().as_str(), "*/10 * * * *");
        assert_eq!(cron.interval_secs(), None);
        for (interval, cron) in [
            (Some(120), Some("* * * * *".to_string())),
            (None, None),
            (Some(10), None),
            (Some(MAX_INTERVAL_SECS + 1), None),
            (None, Some("* * * * * *".to_string())),
        ] {
            assert!(
                ScheduleConfig::from_parts(interval, cron.clone()).is_err(),
                "{interval:?} {cron:?}"
            );
        }
    }

    #[test]
    fn a_stored_schedule_keeps_the_old_interval_field() {
        let old: ScheduleConfig = serde_json::from_str(r#"{"interval_secs": 600}"#).unwrap();
        assert_eq!(old, ScheduleConfig::Interval { secs: 600 });
        assert_eq!(
            serde_json::to_value(&old).unwrap(),
            serde_json::json!({"interval_secs": 600})
        );
        let empty: ScheduleConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, ScheduleConfig::default());
        let cron: ScheduleConfig = serde_json::from_str(r#"{"cron": "0 8 * * 1-5"}"#).unwrap();
        assert_eq!(
            serde_json::to_value(&cron).unwrap(),
            serde_json::json!({"cron": "0 8 * * 1-5"})
        );
        assert!(serde_json::from_str::<ScheduleConfig>(
            r#"{"interval_secs": 600, "cron": "* * * * *"}"#
        )
        .is_err());
    }

    #[test]
    fn the_backoff_of_a_cron_schedule_lands_on_a_fire_time() {
        use chrono::TimeZone;
        let at = |hour, minute| Utc.with_ymd_and_hms(2026, 11, 20, hour, minute, 0).unwrap();
        let cron = ScheduleConfig::from_parts(None, Some("*/10 * * * *".into())).unwrap();
        assert_eq!(cron.next_after(at(10, 0)), at(10, 10));
        assert_eq!(cron.period_secs(at(10, 5)), 300);
        // Twenty minutes of backoff from 10:05: the fire at 10:30.
        assert_eq!(cron.first_fire_from(at(10, 5), 20 * 60), at(10, 30));
        // A delay that ends on a fire time takes that fire.
        assert_eq!(cron.first_fire_from(at(10, 0), 20 * 60), at(10, 20));
        // Never before the next fire.
        assert_eq!(cron.first_fire_from(at(10, 5), 0), at(10, 10));
        let interval = ScheduleConfig::Interval { secs: 300 };
        assert_eq!(interval.first_fire_from(at(10, 0), 600), at(10, 10));
        assert_eq!(interval.period_secs(at(10, 0)), 300);
    }
}
