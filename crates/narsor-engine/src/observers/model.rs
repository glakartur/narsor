//! Records of observers: the observer itself (with its run in progress),
//! the execution errors of its runs and the entries of observer queues.
//!
//! Everything here is plain data plus the validation of what an owner may
//! configure. The durable layout and the locks live in [`super::store`]; the
//! scheduling and the effect routes that mutate these records belong to the
//! control-plane service.
//!
//! Every field added after version 1 of a record carries `#[serde(default)]`,
//! so a record written by an older build still loads. Enums that a client may
//! read end in an `Unrecognized` fallback that this build never writes.

use super::schedule_expr::CronSchedule;
use super::{ObserverError, Result};
pub use crate::domain::observer_name::MAX_OBSERVER_NAME_LEN;
use crate::domain::observer_name::{
    name_like_problem, observer_name_problem, queue_name_problem, OBSERVER_ID_PREFIX,
};
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

/// Current schema version of [`ObserverRecord`]. Version 1 had watch lists
/// instead of queues; the store migrates it when it reads it
/// ([`super::migrate`]).
pub const OBSERVER_RECORD_VERSION: u32 = 2;
/// Current schema version of [`RunFailure`].
pub const RUN_FAILURE_VERSION: u32 = 1;
/// Current schema version of [`QueueEntry`].
pub const QUEUE_ENTRY_VERSION: u32 = 1;

/// Interval between two scheduled runs when the owner names none (5 min).
pub const DEFAULT_INTERVAL_SECS: u64 = 300;
/// Shortest interval an observer may have (R17), and the shortest gap
/// between two fires of a cron schedule (R28): it protects the API limits
/// of the source.
pub const MIN_INTERVAL_SECS: u64 = 60;
/// Longest interval an observer may have (30 days). Keeps every deadline the
/// scheduler computes far inside what a timestamp can hold.
pub const MAX_INTERVAL_SECS: u64 = 30 * 24 * 60 * 60;

/// Tasks one run may create when the owner names no limit (R15).
pub const DEFAULT_TASK_LIMIT_PER_RUN: u32 = 10;
/// Highest task limit per run an owner may configure.
pub const MAX_TASK_LIMIT_PER_RUN: u32 = 1000;

/// Longest `source` text.
pub const MAX_SOURCE_LEN: usize = 256;
/// Largest condition, serialized as JSON.
pub const MAX_CONDITION_BYTES: usize = 64 * 1024;
/// Most secret references one observer may hold.
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

/// The random, never reused identifier of one observer.
///
/// Distinct from the name: deleting an observer and creating another with
/// the same name gives a new id. An id never contains `.`, which keeps `{observer}.{sequence}` run ids
/// unambiguous and the id a safe path component.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(try_from = "String", into = "String")]
pub struct ObserverId(String);

impl ObserverId {
    /// A fresh random id.
    pub fn generate() -> Self {
        Self(format!(
            "{OBSERVER_ID_PREFIX}{}",
            uuid::Uuid::new_v4().simple()
        ))
    }

    /// Validates and wraps `value`.
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_name_like("observer id", &value, false)?;
        Ok(Self(value))
    }

    /// The id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ObserverId {
    type Error = ObserverError;

    fn try_from(value: String) -> Result<Self> {
        Self::parse(value)
    }
}

impl From<ObserverId> for String {
    fn from(value: ObserverId) -> Self {
        value.0
    }
}

impl fmt::Display for ObserverId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The unique, owner-chosen name of an observer (the handle the CLI uses).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(try_from = "String", into = "String")]
pub struct ObserverName(String);

impl ObserverName {
    /// Validates and wraps `value`.
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        match observer_name_problem(&value) {
            None => Ok(Self(value)),
            Some(reason) => Err(ObserverError::invalid("name", reason)),
        }
    }

    /// Whether `value` has the form of an observer id rather than a name.
    pub fn looks_like_id(value: &str) -> bool {
        value.starts_with(OBSERVER_ID_PREFIX)
    }

    /// The name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ObserverName {
    type Error = ObserverError;

    fn try_from(value: String) -> Result<Self> {
        Self::parse(value)
    }
}

impl From<ObserverName> for String {
    fn from(value: ObserverName) -> Self {
        value.0
    }
}

impl fmt::Display for ObserverName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One run of one observer: `{observer_id}.{sequence}`, on the wire as that
/// string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RunId {
    /// The observer.
    pub observer_id: ObserverId,
    /// The run's sequence number within the observer, from 1.
    pub sequence: u64,
}

impl RunId {
    /// The run `sequence` of `observer_id`.
    pub fn new(observer_id: ObserverId, sequence: u64) -> Self {
        Self {
            observer_id,
            sequence,
        }
    }

    /// Parses `{observer_id}.{sequence}`.
    pub fn parse(value: &str) -> Result<Self> {
        let refusal = || ObserverError::invalid("run id", format!("'{value}' is not a run id"));
        let (observer, sequence) = value.split_once('.').ok_or_else(refusal)?;
        let observer_id = ObserverId::parse(observer).map_err(|_| refusal())?;
        if sequence.is_empty() || !sequence.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(refusal());
        }
        let sequence = sequence.parse().map_err(|_| refusal())?;
        Ok(Self::new(observer_id, sequence))
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
        write!(f, "{}.{}", self.observer_id, self.sequence)
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// When an observer runs (R28): an interval, or a five-field cron
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

/// Which tasks an observer's script may create in Narsor. Answering the
/// gate of a queue entry needs no effect: it follows from the mode.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObserverEffect {
    /// Create a new task from the effect data (R3).
    CreateTask(CreateTaskEffect),
    /// Create no tasks.
    #[default]
    None,
}

/// The "create a task" effect: what every task the observer creates runs.
///
/// The script supplies only the effect data. The workflow,
/// repositories, backend and profile come from here, and only the fields
/// named by `mapping` become workflow inputs, so a script cannot choose
/// another workflow or backend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateTaskEffect {
    /// Workflow to run.
    pub workflow_id: String,
    /// Workflow version the observer was validated against.
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
                    "must be a JSON object of the fields the observer's mapping names",
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
                    "field {} is not in the observer's mapping",
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

/// What the owner configures: everything about an observer except its
/// script and its run-time state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObserverConfig {
    /// Unique name.
    pub name: ObserverName,
    /// Free text naming the source (`jira`, `gitlab.example.com`, …).
    pub source: String,
    /// Parameters handed to the script as they are (a JSON object).
    #[serde(default = "empty_object")]
    pub condition: serde_json::Value,
    /// When it runs.
    #[serde(default)]
    pub schedule: ScheduleConfig,
    /// The tasks its script may create.
    #[serde(default)]
    pub effect: ObserverEffect,
    /// The queue it serves, a name in its owner's namespace (R6, R7).
    pub queue: String,
    /// Whether it runs for the entries of its queue only, or on every tick
    /// (R25). The same for every observer of the queue (R26).
    pub mode: ObserverMode,
    /// Secrets handed to the script.
    #[serde(default)]
    pub secrets: Vec<SecretRef>,
    /// Most tasks one run may create (R15).
    #[serde(default = "default_task_limit_per_run")]
    pub task_limit_per_run: u32,
    /// Whether the first run imports what already matches the condition,
    /// rather than only remembering where to start from.
    #[serde(default)]
    pub import_backlog: bool,
}

fn empty_object() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

fn default_task_limit_per_run() -> u32 {
    DEFAULT_TASK_LIMIT_PER_RUN
}

impl ObserverConfig {
    /// A configuration with every optional part at its default: the queue
    /// named after the observer, the default interval.
    pub fn new(
        name: ObserverName,
        source: impl Into<String>,
        effect: ObserverEffect,
        mode: ObserverMode,
    ) -> Self {
        Self {
            queue: name.as_str().to_string(),
            name,
            source: source.into(),
            condition: empty_object(),
            schedule: ScheduleConfig::default(),
            effect,
            mode,
            secrets: Vec::new(),
            task_limit_per_run: DEFAULT_TASK_LIMIT_PER_RUN,
            import_backlog: false,
        }
    }

    /// Checks everything that does not need the workflow catalogue.
    pub fn validate(&self) -> Result<()> {
        let source = self.source.trim();
        if source.is_empty()
            || self.source.len() > MAX_SOURCE_LEN
            || self.source.chars().any(char::is_control)
        {
            return Err(ObserverError::invalid(
                "source",
                format!("must have 1-{MAX_SOURCE_LEN} characters and no control characters"),
            ));
        }
        if !self.condition.is_object() {
            return Err(ObserverError::invalid(
                "condition",
                "the condition must be a JSON object",
            ));
        }
        let condition_len = serde_json::to_vec(&self.condition)
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX);
        if condition_len > MAX_CONDITION_BYTES {
            return Err(ObserverError::invalid(
                "condition",
                format!("the condition has more than {MAX_CONDITION_BYTES} bytes"),
            ));
        }
        self.schedule.validate()?;
        if let Some(reason) = queue_name_problem(&self.queue) {
            return Err(ObserverError::invalid("queue", reason));
        }
        if let ObserverEffect::CreateTask(effect) = &self.effect {
            effect.validate()?;
        }
        if self.secrets.len() > MAX_SECRET_REFS {
            return Err(ObserverError::invalid(
                "secrets",
                format!("at most {MAX_SECRET_REFS} secrets"),
            ));
        }
        let mut envs = BTreeSet::new();
        for secret in &self.secrets {
            secret.validate()?;
            if !envs.insert(secret.env.as_str()) {
                return Err(ObserverError::invalid(
                    "secrets.env",
                    format!("'{}' is used twice", secret.env),
                ));
            }
        }
        if self.task_limit_per_run == 0 || self.task_limit_per_run > MAX_TASK_LIMIT_PER_RUN {
            return Err(ObserverError::invalid(
                "task_limit_per_run",
                format!("must be between 1 and {MAX_TASK_LIMIT_PER_RUN}"),
            ));
        }
        Ok(())
    }

    /// Checks the "create a task" effect against its workflow, as found in
    /// the catalogue (`None`: not found). An observer that creates no tasks
    /// needs none.
    pub fn validate_workflow(&self, workflow: Option<&WorkflowDefinition>) -> Result<()> {
        let ObserverEffect::CreateTask(effect) = &self.effect else {
            return Ok(());
        };
        match workflow {
            Some(workflow) => effect.validate_against(workflow),
            None => Err(ObserverError::WorkflowNotFound(effect.workflow_id.clone())),
        }
    }
}

// ---------------------------------------------------------------------------
// The observer record
// ---------------------------------------------------------------------------

/// Where an observer stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObserverState {
    /// No script version was approved yet; nothing runs on schedule.
    #[default]
    Draft,
    /// Runs on schedule.
    Active,
    /// Stopped by the owner.
    Paused,
    /// A run went over its task limit (R15); waits for the owner.
    NeedsAttention,
    /// Too many runs in a row failed (R16); waits for the owner.
    Error,
    /// A state from a newer build. Never written by this one.
    #[serde(other)]
    Unrecognized,
}

/// One immutable script version, as the record lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptVersion {
    /// Version number, from 1.
    pub version: u32,
    /// When it was saved.
    pub created_at: DateTime<Utc>,
    /// `sha256:` digest of the script bytes, checked on every read.
    pub sha256: String,
}

/// The latest finished run, as `observer list` shows it.
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

/// The run a worker claimed and did not report yet, as the observer's
/// record holds it. Nothing else of a run is stored unless it fails
/// ([`RunFailure`]); its input is built at the claim and handed to the
/// worker only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveRun {
    /// Sequence number within the observer, from 1.
    pub sequence: u64,
    /// The script version it runs.
    pub script_version: u32,
    /// When a worker claimed it.
    pub claimed_at: DateTime<Utc>,
    /// No result by then: the run is lost and its token stops working.
    pub deadline: DateTime<Utc>,
    /// `sha256:` digest of the run token.
    pub token_digest: String,
    /// Tasks this run created so far, checked against the limit (R15).
    #[serde(default)]
    pub tasks_created: u32,
    /// The queue entry the run checks, for an `on_episodes` observer (one
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
    /// The run's id within `observer_id`.
    pub fn run_id(&self, observer_id: &ObserverId) -> RunId {
        RunId::new(observer_id.clone(), self.sequence)
    }

    /// Whether the run passed its deadline at `now`: lost, its token spent.
    pub fn is_past_deadline(&self, now: DateTime<Utc>) -> bool {
        now >= self.deadline
    }
}

/// Reads `active_run` as an [`ActiveRun`], or as nothing when an earlier
/// build of this branch left a bare sequence number there: that run has no
/// token digest any more, so it can only be forgotten.
fn active_run_or_legacy<'de, D>(deserializer: D) -> std::result::Result<Option<ActiveRun>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Stored {
        Run(ActiveRun),
        // Read only to tell it from a malformed run.
        Legacy(#[allow(dead_code)] u64),
    }
    Ok(match Option::<Stored>::deserialize(deserializer)? {
        Some(Stored::Run(run)) => Some(run),
        Some(Stored::Legacy(_)) | None => None,
    })
}

/// The durable record of one observer: `observers/{id}/observer.json`.
///
/// The record is the commit point of the observer: a script version or a
/// run exists for the observer only once the record says so.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObserverRecord {
    /// Schema version, [`OBSERVER_RECORD_VERSION`].
    pub record_version: u32,
    /// Random id, never reused.
    pub observer_id: ObserverId,
    /// Who created it; the tasks it creates belong to them.
    pub owner: OwnerRef,
    /// When it was created.
    pub created_at: DateTime<Utc>,
    /// When it was last changed.
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    /// Incremented by every save.
    #[serde(default)]
    pub revision: u64,
    /// What the owner configured.
    pub config: ObserverConfig,
    /// Where it stands.
    #[serde(default)]
    pub state: ObserverState,
    /// Every script version, oldest first.
    #[serde(default)]
    pub scripts: Vec<ScriptVersion>,
    /// The only version the schedule runs (R11).
    #[serde(default)]
    pub approved_version: Option<u32>,
    /// The state the last successful run returned, handed to the next one.
    #[serde(default)]
    pub script_state: Option<serde_json::Value>,
    /// Whether a run has succeeded since the observer was created.
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
    /// an `on_episodes` observer, in order: each run takes the first (R30).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entry_round: Vec<String>,
    /// The run in progress, if any: at most one per observer.
    #[serde(default, deserialize_with = "active_run_or_legacy")]
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
    /// Failed runs in a row since the last success or owner resume.
    #[serde(default)]
    pub consecutive_failures: u32,
    /// What the server wants the owner to know about the observer: why an
    /// update paused it, say. Cleared when the owner resumes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ObserverRecord {
    /// A new draft record.
    pub fn new(
        observer_id: ObserverId,
        owner: OwnerRef,
        config: ObserverConfig,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            record_version: OBSERVER_RECORD_VERSION,
            observer_id,
            owner,
            created_at: now,
            updated_at: None,
            revision: 0,
            config,
            state: ObserverState::Draft,
            scripts: Vec::new(),
            approved_version: None,
            script_state: None,
            first_run_done: false,
            next_run_at: None,
            run_due_since: None,
            entry_round: Vec::new(),
            active_run: None,
            last_claim_at: None,
            last_run_sequence: 0,
            last_run: None,
            consecutive_failures: 0,
            note: None,
        }
    }

    /// The newest script version number, `None` before the first.
    pub fn latest_script_version(&self) -> Option<u32> {
        self.scripts.last().map(|script| script.version)
    }

    /// The listing of script version `version`, if it exists.
    pub fn script_version(&self, version: u32) -> Option<&ScriptVersion> {
        self.scripts.iter().find(|script| script.version == version)
    }

    /// Since when a scheduled run waits for a worker to claim it; `None`
    /// when none waits (a run in progress does not wait).
    pub fn waiting_for_claim_since(&self) -> Option<DateTime<Utc>> {
        self.run_due_since.filter(|_| self.active_run.is_none())
    }

    /// Whether a run waited for a claim past the schedule's next fire at
    /// `now` (one interval, for an interval schedule): no worker seems to
    /// be polling, which `observer list` and `observer show` warn about.
    pub fn worker_missing(&self, now: DateTime<Utc>) -> bool {
        self.waiting_for_claim_since()
            .is_some_and(|since| now > self.config.schedule.next_after(since))
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

/// The execution error of one run: `observers/{id}/errors/{sequence:08}.json`.
///
/// Written only for a run that did not succeed (a failure, an invalid
/// output, a time-out, a worker error) or was lost. A successful run leaves
/// no file. The store keeps the newest [`super::store::KEPT_RUN_FAILURES`]
/// of each observer, and only the newest of each queue entry (R23).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFailure {
    /// Schema version, [`RUN_FAILURE_VERSION`].
    pub record_version: u32,
    /// The observer.
    pub observer_id: ObserverId,
    /// Sequence number of the run within the observer.
    pub sequence: u64,
    /// The script version it ran.
    pub script_version: u32,
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
        RunId::new(self.observer_id.clone(), self.sequence)
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
/// episode of the same gate (a review loop) has a new one. The queue is a
/// name in the namespace of the task's owner (R7). The entry holds what the
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
    fn a_generated_observer_id_is_valid_and_has_no_dot() {
        let id = ObserverId::generate();
        assert!(ObserverId::parse(id.as_str()).is_ok());
        assert!(!id.as_str().contains('.'));
        assert_ne!(id, ObserverId::generate());
    }

    #[test]
    fn observer_ids_and_names_refuse_unsafe_values() {
        for value in ["", ".hidden", "a/b", "a.b", &"x".repeat(65)] {
            assert!(ObserverId::parse(value).is_err(), "{value}");
        }
        assert!(ObserverName::parse("jira.abc").is_ok());
        assert!(ObserverName::parse("../etc").is_err());
        assert!(serde_json::from_str::<ObserverName>("\"a b\"").is_err());
    }

    #[test]
    fn a_name_may_not_take_the_form_of_an_observer_id() {
        // Otherwise another owner could shadow an observer reached by id.
        let id = ObserverId::generate();
        assert!(ObserverName::parse(id.as_str()).is_err());
        assert!(ObserverName::looks_like_id(id.as_str()));
        assert!(ObserverName::parse("observer-jira").is_ok());
        assert!(!ObserverName::looks_like_id("observer-jira"));
    }

    #[test]
    fn a_run_id_round_trips_and_refuses_garbage() {
        let run = RunId::new(ObserverId::parse("obs-1").unwrap(), 42);
        assert_eq!(run.to_string(), "obs-1.42");
        assert_eq!(RunId::parse("obs-1.42").unwrap(), run);
        for value in ["obs-1", "obs-1.", "obs-1.x", ".1", "obs-1.+4", "a.b.1"] {
            assert!(RunId::parse(value).is_err(), "{value}");
        }
    }

    #[test]
    fn a_run_id_serializes_as_its_string() {
        let run = RunId::new(ObserverId::parse("obs-1").unwrap(), 7);
        assert_eq!(serde_json::to_value(&run).unwrap(), "obs-1.7");
        assert_eq!(serde_json::from_str::<RunId>("\"obs-1.7\"").unwrap(), run);
        assert!(serde_json::from_str::<RunId>("\"obs-1\"").is_err());
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
