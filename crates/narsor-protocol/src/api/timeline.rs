//! Execution timeline response DTOs.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The icon that shows who runs a step and what it is, from the step's action
/// type and, for a `decision`, who decides.
///
/// `None` for a kind without an icon: the unsupported `human` type, an unknown
/// type and the terminal state.
pub fn step_icon(action_type: Option<&str>, decision_by: Option<&str>) -> Option<&'static str> {
    match (action_type?, decision_by) {
        ("agent", _) => Some("✨"),
        ("script", _) => Some("📜"),
        ("wait", _) => Some("✋"),
        ("decision", Some("conductor")) => Some("⚡🔀"),
        ("decision", Some("human")) => Some("🧑🔀"),
        ("decision", _) => Some("🔀"),
        _ => None,
    }
}

/// One entry of an execution timeline: a workflow action attempt, or a run
/// of one harness session on a step.
///
/// An entry holds several attempts when the agent was asked again in the
/// same session without leaving the step (an answer to its question, a
/// reprompt for its output label): they are one entry whose status turns
/// active again. A return to an earlier session of the step after the
/// workflow visited another one is an entry of its own, numbered like the
/// session it returns to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    /// Unique within the timeline: `"<state> #<session_number>"`, with
    /// `".<k>"` after the number on the `k`th entry (from the second) of one
    /// session.
    pub label: String,
    pub state_id: String,
    pub visit: u32,
    /// The latest attempt of the entry; the last of [`Self::attempts`].
    pub action_execution_id: Option<String>,
    pub action_type: Option<String>,
    /// Who decides a `decision` step (`conductor` or `human`); `None` for
    /// other kinds and for a timeline of a version that did not say.
    #[serde(default)]
    pub decision_by: Option<String>,
    pub status: PositionStatus,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub resumed_harness_session: Option<String>,
    pub harness_session_interrupted: bool,
    pub retry_pending: bool,
    pub conversation: ConversationState,
    /// The number of the step's session this entry belongs to (the number in
    /// the label). A new session or harness on a step gets the next number;
    /// an action without a session, a script, is numbered like one.
    #[serde(default)]
    pub session_number: Option<u32>,
    /// Every attempt of the entry, in order; empty in a timeline of a version
    /// that did not say, where [`Self::action_execution_id`] is the one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<String>,
    /// The harness the agent ran on.
    #[serde(default)]
    pub harness: Option<String>,
    /// The model of the run, as the harness reported it, else as asked for.
    #[serde(default)]
    pub model: Option<String>,
    /// The reasoning effort asked for.
    #[serde(default)]
    pub effort: Option<String>,
    /// The harness's own id of the session: what returning to it takes.
    #[serde(default)]
    pub harness_session_id: Option<String>,
    /// What the entry's runs used, as far as the harness said.
    #[serde(default)]
    pub usage: Option<PositionUsage>,
}

impl Position {
    /// The icon of the step's kind, when it has one.
    pub fn icon(&self) -> Option<&'static str> {
        step_icon(self.action_type.as_deref(), self.decision_by.as_deref())
    }

    /// Every attempt of the entry, oldest first.
    pub fn attempt_ids(&self) -> Vec<&str> {
        if self.attempts.is_empty() {
            self.action_execution_id.as_deref().into_iter().collect()
        } else {
            self.attempts.iter().map(String::as_str).collect()
        }
    }
}

/// Tokens of one party, as a harness counts them. A harness that does not
/// separate a kind leaves it zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCounts {
    /// Input tokens not served from a cache.
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    /// Input tokens read from a cache.
    #[serde(default)]
    pub cache_read: u64,
    /// Input tokens written to a cache.
    #[serde(default)]
    pub cache_write: u64,
}

impl TokenCounts {
    pub fn plus(self, other: TokenCounts) -> TokenCounts {
        TokenCounts {
            input: self.input.saturating_add(other.input),
            output: self.output.saturating_add(other.output),
            cache_read: self.cache_read.saturating_add(other.cache_read),
            cache_write: self.cache_write.saturating_add(other.cache_write),
        }
    }

    /// What is left of `self` after `other` was taken away.
    pub fn minus(self, other: TokenCounts) -> TokenCounts {
        TokenCounts {
            input: self.input.saturating_sub(other.input),
            output: self.output.saturating_sub(other.output),
            cache_read: self.cache_read.saturating_sub(other.cache_read),
            cache_write: self.cache_write.saturating_sub(other.cache_write),
        }
    }
}

/// What the harness said about the subagents of one entry, all of them
/// together.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentStats {
    /// How many subagents were started.
    #[serde(default)]
    pub spawned: u32,
    #[serde(default)]
    pub completed: u32,
    #[serde(default)]
    pub failed: u32,
    /// Stopped by the parent, the user or the system.
    #[serde(default)]
    pub killed: u32,
    /// Not started because of a depth, concurrency or budget limit.
    #[serde(default)]
    pub refused: u32,
    /// Tokens, in and out, the subagents used together; 0 when the harness
    /// does not say.
    #[serde(default)]
    pub tokens: u64,
}

impl SubagentStats {
    pub fn plus(self, other: SubagentStats) -> SubagentStats {
        SubagentStats {
            spawned: self.spawned.saturating_add(other.spawned),
            completed: self.completed.saturating_add(other.completed),
            failed: self.failed.saturating_add(other.failed),
            killed: self.killed.saturating_add(other.killed),
            refused: self.refused.saturating_add(other.refused),
            tokens: self.tokens.saturating_add(other.tokens),
        }
    }
}

/// What the runs of one entry used. A part the harness did not report is
/// `None`, which is not the same as none used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct PositionUsage {
    /// All tokens in and out, the subagents' included.
    #[serde(default)]
    pub total: Option<TokenCounts>,
    /// The part of `total` the subagents used, when the harness says.
    #[serde(default)]
    pub subagents: Option<TokenCounts>,
    /// How many subagents ran, when the harness says.
    #[serde(default)]
    pub subagent_count: Option<u32>,
    /// What became of the subagents, when the harness says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_stats: Option<SubagentStats>,
    /// The cost the harness reported, in US dollars.
    #[serde(default)]
    pub cost_usd: Option<f64>,
    /// What the conductor used when it decided in this entry's scope. Kept
    /// apart from the agent's use above, which it never changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conductor_total: Option<TokenCounts>,
    /// The cost of those conductor runs, in US dollars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conductor_cost_usd: Option<f64>,
}

/// Outcome of one timeline position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionStatus {
    Succeeded,
    Failed,
    Interrupted,
    InProgress,
    AwaitingDecision,
}

/// Whether a position has a readable conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConversationState {
    Recorded,
    NotCreated { reason: String },
    NoRecord,
}

/// Where an event belongs on the timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "link", content = "value", rename_all = "snake_case")]
pub enum EventLink {
    Position(String),
    NoLink { reason: String },
}

/// Timeline positions, event links, and the default selection.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Timeline {
    pub positions: Vec<Position>,
    pub links: BTreeMap<u64, EventLink>,
    pub default_position: Option<String>,
}

impl Timeline {
    pub fn position(&self, label: &str) -> Option<&Position> {
        self.positions
            .iter()
            .find(|position| position.label == label)
    }

    pub fn link(&self, sequence: u64) -> Option<&EventLink> {
        self.links.get(&sequence)
    }
}
