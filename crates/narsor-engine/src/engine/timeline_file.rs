//! `timeline.json`: the execution's timeline, kept as a ready file, with
//! what its agents used.
//!
//! The event log stays the source of the timeline's steps; what the agents
//! used (tokens and cost) is in no event. A harness reports it in its output,
//! where the agent executor reads it ([`reading_from_element`]) and hands it
//! to a [`ReadingSink`]. The file keeps those readings per attempt next to
//! the timeline built from the log, so a reading changes the file and nothing
//! else: no event is written for it.
//!
//! The file is the only place the readings are kept: the conversation
//! records hold no tokens or cost. It says which log position, execution
//! status and conversation records the timeline was built from; when any of
//! them is not the current one the timeline is built again from the log and
//! the file's readings are put on it. A lost file loses its readings.
//!
//! # Readings
//!
//! A [`ReadingBasis::Increment`] reading is what one run used, so the
//! readings of an attempt add up. A [`ReadingBasis::SessionTotal`] reading
//! is what the harness session has used altogether: what an earlier reading
//! of the same harness session said is taken off, in timeline order, so an
//! entry only counts the runs that were its own.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use narsor_protocol::api::{PositionUsage, SubagentStats, TokenCounts};
use narsor_protocol::chat::ChatElement;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::{Event, ExecutionStatus};
use crate::engine::timeline::{build_timeline, Timeline};
use crate::persistence::{read_json, write_json_atomic, ExecutionStore};

/// The file's name, in the execution's directory.
pub const TIMELINE_FILE: &str = "timeline.json";

const VERSION: u32 = 1;

/// Serializes the read-modify-write of every timeline file of the process.
static FILE_LOCK: Mutex<()> = Mutex::new(());

/// What a reading says about the use it reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadingBasis {
    /// The use of one run.
    #[default]
    Increment,
    /// The use of the harness session so far.
    SessionTotal,
}

/// What a harness said about the use of one attempt.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageReading {
    #[serde(default)]
    pub basis: ReadingBasis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// The model the harness said it ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<TokenCounts>,
    /// The part of `total` the subagents used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagents: Option<TokenCounts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_count: Option<u32>,
    /// Counts for the whole harness process, so a later reading replaces an
    /// earlier one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_stats: Option<SubagentStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// What the conductor used when it decided in the scope of this
    /// attempt. Always the use of single runs, and never part of `total`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conductor_total: Option<TokenCounts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conductor_cost_usd: Option<f64>,
}

impl UsageReading {
    /// What the conductor's own harness run reported, as the use of the
    /// conductor: tokens and cost only, kept apart from the agent's.
    pub fn of_conductor(run: UsageReading) -> UsageReading {
        UsageReading {
            conductor_total: run.total,
            conductor_cost_usd: run.cost_usd,
            ..UsageReading::default()
        }
    }

    fn has_agent_use(&self) -> bool {
        self.model.is_some()
            || self.total.is_some()
            || self.subagents.is_some()
            || self.subagent_stats.is_some()
            || self.cost_usd.is_some()
    }

    fn is_empty(&self) -> bool {
        !self.has_agent_use() && self.conductor_total.is_none() && self.conductor_cost_usd.is_none()
    }

    /// Puts a later reading of the same attempt on this one.
    fn absorb(&mut self, later: UsageReading) {
        self.conductor_total = plus(self.conductor_total, later.conductor_total);
        self.conductor_cost_usd = match (self.conductor_cost_usd, later.conductor_cost_usd) {
            (Some(a), Some(b)) => Some(a + b),
            (a, b) => a.or(b),
        };
        // A reading of the conductor alone says nothing about the agent: it
        // must not change the basis or the harness of what the agent used.
        if !later.has_agent_use() {
            return;
        }
        let counts = |a: Option<TokenCounts>, b: Option<TokenCounts>| match later.basis {
            ReadingBasis::Increment => plus(a, b),
            ReadingBasis::SessionTotal => b.or(a),
        };
        self.total = counts(self.total, later.total);
        self.subagents = counts(self.subagents, later.subagents);
        self.subagent_count = match later.basis {
            ReadingBasis::Increment => match (self.subagent_count, later.subagent_count) {
                (Some(a), Some(b)) => Some(a.saturating_add(b)),
                (a, b) => a.or(b),
            },
            ReadingBasis::SessionTotal => later.subagent_count.or(self.subagent_count),
        };
        self.subagent_stats = later.subagent_stats.or(self.subagent_stats);
        self.cost_usd = match later.basis {
            ReadingBasis::Increment => match (self.cost_usd, later.cost_usd) {
                (Some(a), Some(b)) => Some(a + b),
                (a, b) => a.or(b),
            },
            ReadingBasis::SessionTotal => later.cost_usd.or(self.cost_usd),
        };
        self.model = later.model.or_else(|| self.model.take());
        self.harness = later.harness.or_else(|| self.harness.take());
        self.basis = later.basis;
    }
}

fn plus(a: Option<TokenCounts>, b: Option<TokenCounts>) -> Option<TokenCounts> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.plus(b)),
        (a, b) => a.or(b),
    }
}

/// The contents of `timeline.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TimelineFile {
    #[serde(default)]
    pub version: u32,
    /// The sequence of the last event the timeline was built from.
    #[serde(default)]
    pub event_cursor: u64,
    #[serde(default)]
    pub execution_status: Option<ExecutionStatus>,
    /// The attempts with a conversation record the timeline was built with,
    /// sorted.
    #[serde(default)]
    pub recorded_attempts: Vec<String>,
    #[serde(default)]
    pub timeline: Option<Timeline>,
    /// What each attempt used, by `action_execution_id`.
    #[serde(default)]
    pub readings: BTreeMap<String, UsageReading>,
}

/// Where the use that harnesses report goes.
pub trait ReadingSink: Send + Sync + std::fmt::Debug {
    /// Takes one reading of `attempt_id` of `execution_id`. Recording never
    /// fails a run: what cannot be written is logged and dropped.
    fn record(&self, execution_id: &str, attempt_id: &str, reading: UsageReading);
}

/// A [`ReadingSink`] writing `timeline.json`, and telling `on_update` when it
/// has.
#[derive(Clone)]
pub struct FileReadingSink {
    store: ExecutionStore,
    on_update: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl std::fmt::Debug for FileReadingSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileReadingSink").finish_non_exhaustive()
    }
}

impl FileReadingSink {
    pub fn new(store: ExecutionStore, on_update: Option<Arc<dyn Fn() + Send + Sync>>) -> Self {
        Self { store, on_update }
    }
}

impl ReadingSink for FileReadingSink {
    fn record(&self, execution_id: &str, attempt_id: &str, reading: UsageReading) {
        match record_reading(&self.store, execution_id, attempt_id, reading) {
            Ok(()) => {
                if let Some(on_update) = &self.on_update {
                    on_update();
                }
            }
            Err(error) => tracing::warn!(
                execution_id,
                attempt_id,
                %error,
                "a usage reading could not be written to the timeline file"
            ),
        }
    }
}

fn file_path(store: &ExecutionStore, execution_id: &str) -> PathBuf {
    store.execution_dir(execution_id).join(TIMELINE_FILE)
}

fn load(path: &std::path::Path) -> Option<TimelineFile> {
    if !path.exists() {
        return None;
    }
    match read_json::<TimelineFile>(path) {
        Ok(file) => Some(file),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "the timeline file is unreadable and is made again");
            None
        }
    }
}

fn save(path: &std::path::Path, file: &TimelineFile) {
    if let Err(error) = write_json_atomic(path, file) {
        tracing::warn!(path = %path.display(), %error, "the timeline file could not be written");
    }
}

/// Adds `reading` to what `attempt_id` used, in the file.
pub fn record_reading(
    store: &ExecutionStore,
    execution_id: &str,
    attempt_id: &str,
    reading: UsageReading,
) -> Result<(), crate::persistence::PersistenceError> {
    if reading.is_empty() {
        return Ok(());
    }
    let _guard = FILE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let path = file_path(store, execution_id);
    let mut file = load(&path).unwrap_or_default();
    file.version = VERSION;
    file.readings
        .entry(attempt_id.to_string())
        .or_default()
        .absorb(reading);
    if let Some(timeline) = file.timeline.as_mut() {
        apply_usage(timeline, &file.readings);
    }
    crate::persistence::atomic::ensure_dir(&store.execution_dir(execution_id))?;
    write_json_atomic(&path, &file)
}

/// The timeline of `execution_id` as it stands, with what its agents used.
///
/// Taken from the file when it was built from this very log position,
/// status and set of conversation records; else built again from `events`
/// and written.
pub fn current_timeline(
    store: &ExecutionStore,
    execution_id: &str,
    events: &[Event],
    execution_status: ExecutionStatus,
    recorded: &HashSet<String>,
) -> Timeline {
    let _guard = FILE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let path = file_path(store, execution_id);
    let cursor = events.last().map_or(0, |event| event.sequence);
    let mut recorded_sorted: Vec<String> = recorded.iter().cloned().collect();
    recorded_sorted.sort();

    let existing = load(&path);
    if let Some(file) = &existing {
        if file.event_cursor == cursor
            && file.execution_status == Some(execution_status)
            && file.recorded_attempts == recorded_sorted
        {
            if let Some(timeline) = &file.timeline {
                return timeline.clone();
            }
        }
    }

    let mut file = existing.unwrap_or_default();
    let mut timeline = build_timeline(events, execution_status, recorded);
    apply_usage(&mut timeline, &file.readings);
    file.version = VERSION;
    file.event_cursor = cursor;
    file.execution_status = Some(execution_status);
    file.recorded_attempts = recorded_sorted;
    file.timeline = Some(timeline.clone());
    if crate::persistence::atomic::ensure_dir(&store.execution_dir(execution_id)).is_ok() {
        save(&path, &file);
    }
    timeline
}

/// Puts what `readings` say on the positions of `timeline`.
///
/// Pure: the usage of every position is made from the readings alone, so
/// calling it again with more readings gives the same as calling it once.
pub fn apply_usage(timeline: &mut Timeline, readings: &BTreeMap<String, UsageReading>) {
    type Total = (TokenCounts, TokenCounts, f64, u32);
    let mut totals: HashMap<(String, String), Total> = HashMap::new();

    for position in &mut timeline.positions {
        let attempts: Vec<String> = position
            .attempt_ids()
            .into_iter()
            .map(str::to_string)
            .collect();
        let mut total: Option<TokenCounts> = None;
        let mut subagents: Option<TokenCounts> = None;
        let mut count: Option<u32> = None;
        let mut stats: Option<SubagentStats> = None;
        let mut cost: Option<f64> = None;
        let mut conductor_total: Option<TokenCounts> = None;
        let mut conductor_cost: Option<f64> = None;
        for attempt in attempts {
            let Some(reading) = readings.get(&attempt) else {
                continue;
            };
            conductor_total = plus(conductor_total, reading.conductor_total);
            conductor_cost = match (conductor_cost, reading.conductor_cost_usd) {
                (Some(x), Some(y)) => Some(x + y),
                (x, y) => x.or(y),
            };
            if let Some(model) = &reading.model {
                position.model = Some(model.clone());
            }
            let (mut a, mut s, mut c, mut n) = (
                reading.total,
                reading.subagents,
                reading.cost_usd,
                reading.subagent_count,
            );
            if reading.basis == ReadingBasis::SessionTotal {
                let harness = reading
                    .harness
                    .clone()
                    .or_else(|| position.harness.clone())
                    .unwrap_or_default();
                if let Some(session) = &position.harness_session_id {
                    let now: Total = (
                        a.unwrap_or_default(),
                        s.unwrap_or_default(),
                        c.unwrap_or_default(),
                        n.unwrap_or_default(),
                    );
                    let before = totals
                        .insert((harness, session.clone()), now)
                        .unwrap_or_default();
                    a = a.map(|a| a.minus(before.0));
                    s = s.map(|s| s.minus(before.1));
                    c = c.map(|c| (c - before.2).max(0.0));
                    n = n.map(|n| n.saturating_sub(before.3));
                }
            }
            stats = match (stats, reading.subagent_stats) {
                (Some(x), Some(y)) => Some(x.plus(y)),
                (x, y) => x.or(y),
            };
            total = plus(total, a);
            subagents = plus(subagents, s);
            count = match (count, n) {
                (Some(x), Some(y)) => Some(x.saturating_add(y)),
                (x, y) => x.or(y),
            };
            cost = match (cost, c) {
                (Some(x), Some(y)) => Some(x + y),
                (x, y) => x.or(y),
            };
        }
        position.usage = (total.is_some()
            || subagents.is_some()
            || count.is_some()
            || stats.is_some()
            || cost.is_some()
            || conductor_total.is_some()
            || conductor_cost.is_some())
        .then_some(PositionUsage {
            total,
            subagents,
            subagent_count: count,
            subagent_stats: stats,
            cost_usd: cost,
            conductor_total,
            conductor_cost_usd: conductor_cost,
        });
    }
}

/// What a conversation element says about use, when it says any: the model
/// a session started with, or the result of a run with its tokens and cost.
pub fn reading_from_element(element: &ChatElement, harness: Option<&str>) -> Option<UsageReading> {
    let mut reading = UsageReading {
        harness: harness.map(str::to_string),
        ..UsageReading::default()
    };
    match element {
        ChatElement::SessionStarted { model, .. } => reading.model = model.clone(),
        ChatElement::RunResult {
            cost_usd,
            usage,
            subagent_stats,
            ..
        } => {
            reading.cost_usd = *cost_usd;
            reading.subagent_stats = *subagent_stats;
            // Claude's `usage` is the whole run's; which part was a
            // subagent's is not in it, so `subagents` stays unreported.
            reading.total = usage.as_ref().map(token_counts);
        }
        _ => return None,
    }
    (!reading.is_empty()).then_some(reading)
}

/// The tokens of a harness `usage` object, in the Anthropic spelling.
fn token_counts(usage: &Value) -> TokenCounts {
    let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    TokenCounts {
        input: count("input_tokens"),
        output: count("output_tokens"),
        cache_read: count("cache_read_input_tokens"),
        cache_write: count("cache_creation_input_tokens"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use narsor_protocol::api::Position;

    fn position(label: &str, attempts: &[&str], session: Option<&str>) -> Position {
        let mut position = serde_json::from_value::<Position>(serde_json::json!({
            "label": label,
            "state_id": "s",
            "visit": 1,
            "action_execution_id": attempts.last(),
            "action_type": "agent",
            "status": "succeeded",
            "started_at": null,
            "ended_at": null,
            "error": null,
            "resumed_harness_session": null,
            "harness_session_interrupted": false,
            "retry_pending": false,
            "conversation": { "state": "recorded" }
        }))
        .unwrap();
        position.attempts = attempts.iter().map(|a| a.to_string()).collect();
        position.harness_session_id = session.map(str::to_string);
        position.harness = Some("h".into());
        position
    }

    fn tokens(input: u64, output: u64) -> Option<TokenCounts> {
        Some(TokenCounts {
            input,
            output,
            ..TokenCounts::default()
        })
    }

    fn reading(basis: ReadingBasis, input: u64, output: u64, cost: f64) -> UsageReading {
        UsageReading {
            basis,
            total: tokens(input, output),
            cost_usd: Some(cost),
            ..UsageReading::default()
        }
    }

    #[test]
    fn increments_of_an_entrys_attempts_add_up() {
        let mut timeline = Timeline {
            positions: vec![position("s #1", &["a1", "a2"], Some("x"))],
            ..Timeline::default()
        };
        let readings = BTreeMap::from([
            (
                "a1".to_string(),
                reading(ReadingBasis::Increment, 10, 4, 0.5),
            ),
            (
                "a2".to_string(),
                reading(ReadingBasis::Increment, 1, 2, 0.25),
            ),
        ]);
        apply_usage(&mut timeline, &readings);
        let usage = timeline.positions[0].usage.unwrap();
        assert_eq!(usage.total, tokens(11, 6));
        assert_eq!(usage.subagents, None);
        assert_eq!(usage.cost_usd, Some(0.75));
    }

    #[test]
    fn subagent_stats_of_one_attempt_are_the_latest_and_those_of_attempts_add_up() {
        let stats = |spawned| SubagentStats {
            spawned,
            completed: spawned,
            ..SubagentStats::default()
        };
        let mut first = reading(ReadingBasis::Increment, 1, 1, 0.1);
        first.subagent_stats = Some(stats(3));
        let mut later = reading(ReadingBasis::Increment, 1, 1, 0.1);
        later.subagent_stats = Some(stats(7));
        first.absorb(later);
        assert_eq!(first.subagent_stats, Some(stats(7)));

        let mut other = reading(ReadingBasis::Increment, 1, 1, 0.1);
        other.subagent_stats = Some(stats(2));
        let mut timeline = Timeline {
            positions: vec![position("s #1", &["a1", "a2"], Some("x"))],
            ..Timeline::default()
        };
        let readings = BTreeMap::from([("a1".to_string(), first), ("a2".to_string(), other)]);
        apply_usage(&mut timeline, &readings);
        let usage = timeline.positions[0].usage.unwrap();
        assert_eq!(usage.subagent_stats, Some(stats(9)));
    }

    #[test]
    fn a_session_total_loses_what_an_earlier_entry_of_the_session_had() {
        let mut timeline = Timeline {
            positions: vec![
                position("s #1", &["a1"], Some("x")),
                position("t #1", &["b1"], Some("y")),
                position("s #1.2", &["a2"], Some("x")),
            ],
            ..Timeline::default()
        };
        let readings = BTreeMap::from([
            (
                "a1".to_string(),
                reading(ReadingBasis::SessionTotal, 10, 4, 1.0),
            ),
            (
                "b1".to_string(),
                reading(ReadingBasis::SessionTotal, 7, 7, 0.7),
            ),
            (
                "a2".to_string(),
                reading(ReadingBasis::SessionTotal, 25, 9, 1.75),
            ),
        ]);
        apply_usage(&mut timeline, &readings);
        let of = |i: usize| timeline.positions[i].usage.unwrap();
        assert_eq!(of(0).total, tokens(10, 4));
        assert_eq!(of(1).total, tokens(7, 7));
        assert_eq!(of(2).total, tokens(15, 5));
        assert_eq!(of(2).cost_usd, Some(0.75));
    }

    #[test]
    fn subagent_stats_are_the_last_of_an_attempt_and_the_sum_of_its_attempts() {
        let stats = |spawned| SubagentStats {
            spawned,
            completed: spawned,
            ..SubagentStats::default()
        };
        let mut first = UsageReading {
            subagent_stats: Some(stats(2)),
            ..UsageReading::default()
        };
        first.absorb(UsageReading {
            subagent_stats: Some(stats(2)),
            ..UsageReading::default()
        });
        let second = UsageReading {
            subagent_stats: Some(stats(3)),
            ..UsageReading::default()
        };
        let mut timeline = Timeline {
            positions: vec![position("s #1", &["a1", "a2"], None)],
            ..Timeline::default()
        };
        let readings = BTreeMap::from([("a1".to_string(), first), ("a2".to_string(), second)]);
        apply_usage(&mut timeline, &readings);
        let usage = timeline.positions[0].usage.expect("usage");
        assert_eq!(usage.subagent_stats, Some(stats(5)));
    }

    #[test]
    fn a_position_without_a_reading_has_no_usage() {
        let mut timeline = Timeline {
            positions: vec![position("s #1", &["a1"], None)],
            ..Timeline::default()
        };
        timeline.positions[0].usage = Some(PositionUsage::default());
        apply_usage(&mut timeline, &BTreeMap::new());
        assert_eq!(timeline.positions[0].usage, None);
    }

    #[test]
    fn a_claude_result_is_a_reading() {
        let element = ChatElement::RunResult {
            text: None,
            structured_output: None,
            stop_reason: None,
            is_error: false,
            cost_usd: Some(0.02),
            usage: Some(serde_json::json!({
                "input_tokens": 3,
                "output_tokens": 209,
                "cache_read_input_tokens": 100,
                "cache_creation_input_tokens": 50,
            })),
            turns: None,
            subagent_stats: None,
            duration_ms: None,
        };
        let reading = reading_from_element(&element, Some("claude-code")).unwrap();
        assert_eq!(reading.cost_usd, Some(0.02));
        assert_eq!(
            reading.total,
            Some(TokenCounts {
                input: 3,
                output: 209,
                cache_read: 100,
                cache_write: 50
            })
        );
        assert_eq!(reading.basis, ReadingBasis::Increment);
    }

    #[test]
    fn a_conductor_reading_keeps_the_basis_and_harness_of_the_agents_reading() {
        let mut agent = reading(ReadingBasis::SessionTotal, 10, 4, 1.0);
        agent.harness = Some("claude-code".into());

        agent.absorb(UsageReading::of_conductor(reading(
            ReadingBasis::Increment,
            3,
            1,
            0.1,
        )));

        assert_eq!(agent.basis, ReadingBasis::SessionTotal);
        assert_eq!(agent.harness.as_deref(), Some("claude-code"));
        assert_eq!(agent.total, tokens(10, 4));
        assert_eq!(agent.cost_usd, Some(1.0));
        assert_eq!(agent.conductor_total, tokens(3, 1));
        assert_eq!(agent.conductor_cost_usd, Some(0.1));
    }

    #[test]
    fn a_conductor_reading_leaves_out_what_is_not_its_use() {
        let mut run = reading(ReadingBasis::Increment, 3, 1, 0.1);
        run.model = Some("m".into());
        run.subagent_stats = Some(SubagentStats::default());

        let conductor = UsageReading::of_conductor(run);

        assert!(!conductor.has_agent_use());
        assert!(!conductor.is_empty());
    }
}
