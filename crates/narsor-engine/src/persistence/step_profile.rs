//! The result of a `profile` step: `executions/{id}/step-profile.json`.
//!
//! A `profile` step proposes a model and an effort for other steps. The engine
//! validates and clamps the proposal, then stores it here; every attempt of an
//! agent step reads it back when it starts. The file belongs to the engine,
//! not to the conductor, and it survives a restart of the runner.
//!
//! Each run of a `profile` step replaces the whole file: an entry that the
//! latest run does not name is gone. See
//! [`ExecutionStore::write_step_profile`](super::ExecutionStore::write_step_profile)
//! and [`ExecutionStore::read_step_profile`](super::ExecutionStore::read_step_profile).
//!
//! ```json
//! {
//!   "version": 1,
//!   "entries": [
//!     {
//!       "step_id": "implement",
//!       "model": "strong",
//!       "effort": "high",
//!       "rationale": "Touches the scheduler and its locking.",
//!       "requested_model": "strongest",
//!       "requested_effort": "high",
//!       "profiled_by": "profile",
//!       "decided_at": "2026-11-11T10:00:00Z"
//!     }
//!   ]
//! }
//! ```

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::engine::step_profile::Setting;

/// The version of `step-profile.json` this binary writes and reads.
pub const STEP_PROFILE_VERSION: u32 = 1;

/// The largest `step-profile.json` the engine reads, in bytes. A larger file
/// is a read error, which the caller treats as "no profile".
pub const MAX_STEP_PROFILE_BYTES: u64 = 256 * 1024;

/// What a `profile` step decided for one step, with the reason.
///
/// `model` and `effort` are the values the step starts on, already clamped to
/// its ceiling. `None` means "keep the step's default from the workflow": the
/// profiling agent did not name a value, or named the default. They are the
/// same shape as the [`Setting`] that `step_profile::resolve` takes; see
/// [`Self::setting`].
///
/// `requested_model` and `requested_effort` are what the agent asked for
/// before the clamp. They are equal to `model` and `effort` when the clamp
/// changed nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepProfileEntry {
    /// The step the entry is for.
    pub step_id: String,
    /// The model tier the step starts on; `None` keeps the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The effort the step starts on; `None` keeps the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Why, in the profiling agent's words.
    #[serde(default)]
    pub rationale: String,
    /// The model tier the agent asked for, before the clamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_model: Option<String>,
    /// The effort the agent asked for, before the clamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_effort: Option<String>,
    /// The `profile` step that made the decision.
    pub profiled_by: String,
    /// When the decision was made.
    pub decided_at: DateTime<Utc>,
}

impl StepProfileEntry {
    /// The model and effort the step starts on.
    pub fn setting(&self) -> Setting {
        Setting::new(self.model.as_deref(), self.effort.as_deref())
    }

    /// The model and effort the agent asked for, before the clamp.
    pub fn requested(&self) -> Setting {
        Setting::new(
            self.requested_model.as_deref(),
            self.requested_effort.as_deref(),
        )
    }
}

/// The content of `step-profile.json`. A missing file reads as the empty
/// result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepProfileResult {
    /// The format of the file; see [`STEP_PROFILE_VERSION`].
    pub version: u32,
    /// The entries, in the order the profiling agent gave them.
    #[serde(default)]
    pub entries: Vec<StepProfileEntry>,
}

impl Default for StepProfileResult {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl StepProfileResult {
    /// A result of the current version with `entries`.
    pub fn new(entries: Vec<StepProfileEntry>) -> Self {
        Self {
            version: STEP_PROFILE_VERSION,
            entries,
        }
    }

    /// The first entry for `step_id`, if there is one.
    pub fn lookup(&self, step_id: &str) -> Option<&StepProfileEntry> {
        self.entries.iter().find(|entry| entry.step_id == step_id)
    }
}
