//! Records of observers: the observer itself (with its run in progress),
//! the execution errors of its runs and its watch entries.
//!
//! Everything here is plain data plus the validation of what an owner may
//! configure. The durable layout and the locks live in [`super::store`]; the
//! scheduling and the effect routes that mutate these records belong to the
//! control-plane service.
//!
//! Every field added after version 1 of a record carries `#[serde(default)]`,
//! so a record written by an older build still loads. Enums that a client may
//! read end in an `Unrecognized` fallback that this build never writes.

use super::{ObserverError, Result};
use crate::domain::{BackendId, TaskId, WorkflowDefinition};
use chrono::{DateTime, Utc};
use narsor_protocol::controller::{CreateTaskRequest, LaunchRequest, OwnerRef, WorkspaceRequest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Current schema version of [`ObserverRecord`].
pub const OBSERVER_RECORD_VERSION: u32 = 1;
/// Current schema version of [`RunFailure`].
pub const RUN_FAILURE_VERSION: u32 = 1;
/// Current schema version of [`WatchEntry`].
pub const WATCH_ENTRY_VERSION: u32 = 1;

/// Interval between two scheduled runs when the owner names none (5 min).
pub const DEFAULT_INTERVAL_SECS: u64 = 300;
/// Shortest interval an observer may have (R17): it protects the API limits
/// of the source.
pub const MIN_INTERVAL_SECS: u64 = 60;
/// Longest interval an observer may have (30 days). Keeps every deadline the
/// scheduler computes far inside what a timestamp can hold.
pub const MAX_INTERVAL_SECS: u64 = 30 * 24 * 60 * 60;

/// Tasks one run may create when the owner names no limit (R15).
pub const DEFAULT_TASK_LIMIT_PER_RUN: u32 = 10;
/// Highest task limit per run an owner may configure.
pub const MAX_TASK_LIMIT_PER_RUN: u32 = 1000;

/// Longest watched object (a PR/MR URL, say) when the owner names no limit.
pub const DEFAULT_WATCH_OBJECT_MAX_LEN: usize = 512;
/// Highest object length limit an owner may configure.
pub const MAX_WATCH_OBJECT_MAX_LEN: usize = 4096;

/// Longest observer name and observer id.
pub const MAX_OBSERVER_NAME_LEN: usize = 64;
/// Longest `source` text.
pub const MAX_SOURCE_LEN: usize = 256;
/// Largest condition, serialized as JSON.
pub const MAX_CONDITION_BYTES: usize = 64 * 1024;
/// Most secret references one observer may hold.
pub const MAX_SECRET_REFS: usize = 32;
/// Largest script version.
pub const MAX_SCRIPT_BYTES: usize = 256 * 1024;
/// Longest event type of a watch entry.
pub const MAX_EVENT_TYPE_LEN: usize = 128;

/// Prefix of every generated observer id.
const OBSERVER_ID_PREFIX: &str = "obs-";

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Validates a name-like value: 1–[`MAX_OBSERVER_NAME_LEN`] characters of
/// `[A-Za-z0-9_-]`, plus `.` when `allow_dot`, never starting with `.`.
fn validate_name_like(field: &'static str, value: &str, allow_dot: bool) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= MAX_OBSERVER_NAME_LEN
        && !value.starts_with('.')
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'_')
                || (allow_dot && byte == b'.')
        });
    if valid {
        Ok(())
    } else {
        let allowed = if allow_dot {
            "[A-Za-z0-9._-]"
        } else {
            "[A-Za-z0-9_-]"
        };
        Err(ObserverError::invalid(
            field,
            format!(
                "'{value}' is not valid: use 1-{MAX_OBSERVER_NAME_LEN} characters of {allowed}, not starting with '.'"
            ),
        ))
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

/// The unique, owner-chosen name of an observer (the handle the CLI and a
/// workflow's `watch` block use).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(try_from = "String", into = "String")]
pub struct ObserverName(String);

impl ObserverName {
    /// Validates and wraps `value`.
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_name_like("name", &value, true)?;
        // A name that looks like an id could shadow another owner's
        // observer where a name or an id is accepted.
        if value.starts_with(OBSERVER_ID_PREFIX) {
            return Err(ObserverError::invalid(
                "name",
                format!("'{value}' is not valid: a name may not start with '{OBSERVER_ID_PREFIX}', the prefix of observer ids"),
            ));
        }
        Ok(Self(value))
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

/// When an observer runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleConfig {
    /// Seconds between two scheduled runs; at least [`MIN_INTERVAL_SECS`].
    #[serde(default = "default_interval_secs")]
    pub interval_secs: u64,
}

/// `secs` seconds; a count too large for a duration is the largest one.
pub fn secs_duration(secs: u64) -> chrono::Duration {
    i64::try_from(secs)
        .ok()
        .and_then(chrono::Duration::try_seconds)
        .unwrap_or(chrono::Duration::MAX)
}

fn default_interval_secs() -> u64 {
    DEFAULT_INTERVAL_SECS
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            interval_secs: DEFAULT_INTERVAL_SECS,
        }
    }
}

impl ScheduleConfig {
    /// The interval as a duration.
    pub fn interval(&self) -> chrono::Duration {
        secs_duration(self.interval_secs)
    }

    /// Refuses an interval outside [`MIN_INTERVAL_SECS`]..=[`MAX_INTERVAL_SECS`].
    pub fn validate(&self) -> Result<()> {
        if self.interval_secs < MIN_INTERVAL_SECS {
            return Err(ObserverError::invalid(
                "interval",
                format!(
                    "{} seconds is too short: the interval must be at least {MIN_INTERVAL_SECS} seconds",
                    self.interval_secs
                ),
            ));
        }
        if self.interval_secs > MAX_INTERVAL_SECS {
            return Err(ObserverError::invalid(
                "interval",
                format!(
                    "{} seconds is too long: the interval must be at most {MAX_INTERVAL_SECS} seconds",
                    self.interval_secs
                ),
            ));
        }
        Ok(())
    }
}

/// What an observer's script may do in Narsor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObserverEffect {
    /// Create a new task from the effect data (R3).
    CreateTask(CreateTaskEffect),
    /// Deliver an event to the task of a watch entry (R4). Has no target of
    /// its own: the target is always the task of the entry, so it requires
    /// [`WatchMode::WatchList`].
    DeliverEvent,
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

/// Which objects an observer's script looks at.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchMode {
    /// Everything its condition selects (a Jira query, say).
    #[default]
    All,
    /// Only the objects of its watch entries (R5); no entries, no run (R6).
    WatchList,
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

/// Which objects a watch entry may name (a URL prefix, say), and how long
/// one may be.
///
/// The script sends the source's token to whatever host an object names, so
/// an entry outside these prefixes is refused rather than watched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchObjectPolicy {
    /// An object must start with one of these. A URL prefix must include
    /// the `/` that ends its host, so `https://git.example.com` cannot be
    /// extended into `https://git.example.com.evil.org/`.
    #[serde(default)]
    pub allowed_prefixes: Vec<String>,
    /// Longest object accepted.
    #[serde(default = "default_watch_object_max_len")]
    pub max_len: usize,
}

fn default_watch_object_max_len() -> usize {
    DEFAULT_WATCH_OBJECT_MAX_LEN
}

impl Default for WatchObjectPolicy {
    fn default() -> Self {
        Self {
            allowed_prefixes: Vec::new(),
            max_len: DEFAULT_WATCH_OBJECT_MAX_LEN,
        }
    }
}

impl WatchObjectPolicy {
    /// Refuses a limit out of range or a prefix that does not pin a host.
    pub fn validate(&self) -> Result<()> {
        if self.max_len == 0 || self.max_len > MAX_WATCH_OBJECT_MAX_LEN {
            return Err(ObserverError::invalid(
                "watch_objects.max_len",
                format!("must be between 1 and {MAX_WATCH_OBJECT_MAX_LEN}"),
            ));
        }
        for prefix in &self.allowed_prefixes {
            let field = "watch_objects.allowed_prefixes";
            if prefix.is_empty() || prefix.len() > self.max_len {
                return Err(ObserverError::invalid(
                    field,
                    format!("a prefix must have 1-{} characters", self.max_len),
                ));
            }
            if prefix.chars().any(|c| c.is_control() || c.is_whitespace()) {
                return Err(ObserverError::invalid(
                    field,
                    format!("'{prefix}' contains whitespace or control characters"),
                ));
            }
            if let Some(reason) = url_prefix_refusal(prefix) {
                return Err(ObserverError::invalid(
                    field,
                    format!("'{prefix}' {reason}"),
                ));
            }
        }
        Ok(())
    }

    /// Whether `object` may be watched: not empty, not longer than the
    /// limit, no control characters, and under an allowed prefix. Refused
    /// with the reason.
    pub fn check(&self, object: &str) -> Result<()> {
        let refusal = |reason: String| ObserverError::invalid("watch object", reason);
        if object.is_empty() {
            return Err(refusal("the object must not be empty".into()));
        }
        if object.len() > self.max_len {
            return Err(refusal(format!(
                "the object has {} characters, more than the limit of {}",
                object.len(),
                self.max_len
            )));
        }
        if object.chars().any(char::is_control) {
            return Err(refusal("the object contains control characters".into()));
        }
        if self
            .allowed_prefixes
            .iter()
            .any(|prefix| object.starts_with(prefix.as_str()))
        {
            Ok(())
        } else {
            Err(refusal(format!(
                "'{object}' is not under an allowed prefix of the observer"
            )))
        }
    }
}

/// Why `prefix` does not pin one host, `None` when it does or when it is
/// not URL-like at all (`MR!`, `PROJ-`).
///
/// A URL-like prefix (one with `://`, or starting with `//`) must name a
/// scheme of letters, digits, `+`, `-` or `.`, and an authority without
/// userinfo that is ended by `/`: an object that starts with such a prefix
/// then has exactly the prefix's scheme, host and port, whatever follows.
fn url_prefix_refusal(prefix: &str) -> Option<&'static str> {
    let rest = if let Some(rest) = prefix.strip_prefix("//") {
        rest
    } else if let Some((scheme, rest)) = prefix.split_once("://") {
        let scheme_valid = scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
        if !scheme_valid {
            return Some("has no valid URL scheme");
        }
        rest
    } else {
        return None;
    };
    let Some((authority, _path)) = rest.split_once('/') else {
        return Some("must end its host with '/'");
    };
    if authority.is_empty() {
        return Some("names no host");
    }
    if authority.contains(['@', '\\', '?', '#']) {
        return Some("must not carry userinfo, '\\', '?' or '#' in its host");
    }
    None
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
    /// What its script may do.
    pub effect: ObserverEffect,
    /// Which objects it looks at.
    #[serde(default)]
    pub watch_mode: WatchMode,
    /// Secrets handed to the script.
    #[serde(default)]
    pub secrets: Vec<SecretRef>,
    /// Most tasks one run may create (R15).
    #[serde(default = "default_task_limit_per_run")]
    pub task_limit_per_run: u32,
    /// Which objects a watch entry may name.
    #[serde(default)]
    pub watch_objects: WatchObjectPolicy,
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
    /// A configuration with every optional part at its default.
    pub fn new(name: ObserverName, source: impl Into<String>, effect: ObserverEffect) -> Self {
        Self {
            name,
            source: source.into(),
            condition: empty_object(),
            schedule: ScheduleConfig::default(),
            effect,
            watch_mode: WatchMode::default(),
            secrets: Vec::new(),
            task_limit_per_run: DEFAULT_TASK_LIMIT_PER_RUN,
            watch_objects: WatchObjectPolicy::default(),
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
        match &self.effect {
            ObserverEffect::CreateTask(effect) => effect.validate()?,
            ObserverEffect::DeliverEvent if self.watch_mode != WatchMode::WatchList => {
                return Err(ObserverError::invalid(
                    "effect",
                    "deliver_event needs the watch_list watch mode: the event goes to the task of a watch entry",
                ));
            }
            ObserverEffect::DeliverEvent => {}
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
        self.watch_objects.validate()?;
        if self.watch_mode == WatchMode::WatchList && self.watch_objects.allowed_prefixes.is_empty()
        {
            return Err(ObserverError::invalid(
                "watch_objects.allowed_prefixes",
                "the watch_list watch mode needs at least one allowed object prefix",
            ));
        }
        Ok(())
    }

    /// Checks the "create a task" effect against its workflow, as found in
    /// the catalogue (`None`: not found). Any other effect needs none.
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
            active_run: None,
            last_claim_at: None,
            last_run_sequence: 0,
            last_run: None,
            consecutive_failures: 0,
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
    /// when none waits.
    pub fn waiting_for_claim_since(&self) -> Option<DateTime<Utc>> {
        self.run_due_since
    }

    /// Whether a run waited for a claim longer than one interval at `now`:
    /// no worker seems to be polling, which `observer list` and `observer
    /// show` warn about.
    pub fn worker_missing(&self, now: DateTime<Utc>) -> bool {
        let interval = self.config.schedule.interval();
        self.waiting_for_claim_since()
            .is_some_and(|since| now - since > interval)
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
    /// The script exited 0 with a valid output document.
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
/// of each observer.
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
// Watch entries
// ---------------------------------------------------------------------------

/// Who added a watch entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchOrigin {
    /// A workflow step's `watch` block, when its `wait` suspended.
    #[default]
    Workflow,
    /// The owner, through the CLI.
    Operator,
    /// An origin from a newer build. Never written by this one.
    #[serde(other)]
    Unrecognized,
}

/// One watched object (a PR/MR) and the task its events go to:
/// `observers/{id}/watches/{entry_id}.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchEntry {
    /// Schema version, [`WATCH_ENTRY_VERSION`].
    pub record_version: u32,
    /// [`watch_entry_id`] of the task, step and object.
    pub entry_id: String,
    /// The task events go to.
    pub task_id: TaskId,
    /// The workflow step that added it; `None` for an owner's entry.
    #[serde(default)]
    pub step: Option<String>,
    /// The watched object, as the script receives it.
    pub object: String,
    /// The event types to catch.
    #[serde(default)]
    pub event_types: Vec<String>,
    /// Who added it.
    #[serde(default)]
    pub origin: WatchOrigin,
    /// When it was added.
    pub created_at: DateTime<Utc>,
}

impl WatchEntry {
    /// A new entry, its id derived from the task, step and object.
    pub fn new(
        task_id: TaskId,
        step: Option<String>,
        object: impl Into<String>,
        event_types: Vec<String>,
        origin: WatchOrigin,
        now: DateTime<Utc>,
    ) -> Self {
        let object = object.into();
        Self {
            record_version: WATCH_ENTRY_VERSION,
            entry_id: watch_entry_id(&task_id, step.as_deref(), &object),
            task_id,
            step,
            object,
            event_types,
            origin,
            created_at: now,
        }
    }

    /// Refuses an id that is not the entry's own, or a bad event type.
    pub fn validate(&self) -> Result<()> {
        if self.entry_id != watch_entry_id(&self.task_id, self.step.as_deref(), &self.object) {
            return Err(ObserverError::invalid(
                "watch entry",
                "the entry id does not match its task, step and object",
            ));
        }
        if self.event_types.is_empty() {
            return Err(ObserverError::invalid(
                "watch entry",
                "at least one event type is needed",
            ));
        }
        for event_type in &self.event_types {
            if event_type.is_empty()
                || event_type.len() > MAX_EVENT_TYPE_LEN
                || event_type
                    .chars()
                    .any(|c| c.is_control() || c.is_whitespace())
            {
                return Err(ObserverError::invalid(
                    "watch entry",
                    format!(
                        "event type '{event_type}' must have 1-{MAX_EVENT_TYPE_LEN} characters and no whitespace"
                    ),
                ));
            }
        }
        Ok(())
    }
}

/// The id of the watch entry of `task_id`, `step` and `object`: one entry
/// per triple, however often the step suspends (a review loop).
pub fn watch_entry_id(task_id: &TaskId, step: Option<&str>, object: &str) -> String {
    // A step name never holds a newline; `-` stands for an owner's entry.
    let step = step.map_or_else(|| "-".to_string(), |step| format!("+{step}"));
    sha256_hex(format!("{task_id}\n{step}\n{object}").as_bytes())
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
    fn the_watch_entry_id_tells_an_owner_entry_from_a_step_entry() {
        let task = TaskId::parse("task-1").unwrap();
        let owner = watch_entry_id(&task, None, "https://x/1");
        let step = watch_entry_id(&task, Some("-"), "https://x/1");
        assert_ne!(owner, step);
        assert_eq!(owner, watch_entry_id(&task, None, "https://x/1"));
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

    #[test]
    fn a_watch_object_must_sit_under_an_allowed_prefix() {
        let policy = WatchObjectPolicy {
            allowed_prefixes: vec!["https://git.example.com/".into()],
            max_len: 40,
        };
        assert!(policy.validate().is_ok());
        assert!(policy.check("https://git.example.com/a/-/1").is_ok());
        assert!(policy.check("https://git.example.com.evil.org/").is_err());
        assert!(policy
            .check("https://git.example.com/a/very/long/path/x")
            .is_err());
        assert!(policy.check("https://git.example.com/\n").is_err());
        assert!(policy.check("").is_err());
    }

    #[test]
    fn a_url_prefix_must_pin_its_host() {
        let policy = WatchObjectPolicy {
            allowed_prefixes: vec!["https://git.example.com".into()],
            ..WatchObjectPolicy::default()
        };
        assert!(policy.validate().is_err());
        for (prefix, valid) in [
            ("MR!", true),
            ("https://git.example.com/", true),
            ("https://git.example.com:8443/group/", true),
            ("https://user@evil.org/", false),
            ("https://git.example.com@evil.org/", false),
            ("https:///path/", false),
            ("//evil.org", false),
            ("//git.example.com/", true),
            ("1http://git.example.com/", false),
            ("https://git.example.com\\@evil.org/", false),
        ] {
            let policy = WatchObjectPolicy {
                allowed_prefixes: vec![prefix.into()],
                ..WatchObjectPolicy::default()
            };
            assert_eq!(policy.validate().is_ok(), valid, "{prefix}");
        }
    }
}
