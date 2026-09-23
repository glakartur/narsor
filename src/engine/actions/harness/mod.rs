//! Harness adapters: one implementation per AI coding CLI (D17).
//!
//! An [`AgentActionConfig`] says *what* the agent should do; a
//! [`HarnessAdapter`] says *how* a particular command-line harness is asked to
//! do it. Two things are harness-specific and nothing else is:
//!
//! * the **argv** — which flags carry the model, the extra directories, the
//!   permission mode, the session to resume, and where the prompt goes;
//! * the **output** — what the harness prints, and where inside it the agent's
//!   final text and the session id are.
//!
//! Everything above that — the output-label contract, the session bookkeeping,
//! the process handling, the timeout — lives once in
//! [`super::agent`] and never has to be written again for a third harness.
//!
//! # Why the program name is part of the adapter
//!
//! Every adapter carries the program it runs ([`HarnessAdapter::program`]) and
//! can be built with a different one ([`ClaudeCodeAdapter::with_program`]).
//! That is not a testing hook bolted on afterwards: an operator whose `claude`
//! lives outside `PATH`, or who wraps it in a script, needs exactly the same
//! seam. The test suite uses it to point the adapters at a fake harness, so the
//! suite never spends money and never needs credentials.

pub mod claude_code;
pub mod opencode;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use crate::domain::{ActionError, AgentActionConfig};

pub use claude_code::{ClaudeCodeAdapter, CLAUDE_CODE};
pub use opencode::{OpenCodeAdapter, OPENCODE};

/// Everything an adapter needs to build one invocation.
#[derive(Debug, Clone, Copy)]
pub struct HarnessRequest<'a> {
    /// The action configuration, templates already expanded.
    pub config: &'a AgentActionConfig,

    /// The prompt as the harness will receive it: the state's prompt with the
    /// output-label contract appended.
    pub prompt: &'a str,

    /// The directory the harness runs in — the execution's workspace.
    pub workspace: &'a Path,

    /// A harness session to continue instead of starting a fresh one (D18).
    pub resume_session: Option<&'a str>,
}

/// The command line an adapter decided on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessInvocation {
    /// The binary to run; looked up on `PATH` unless it is a path.
    pub program: String,

    /// Its arguments, each one separately — never a shell string.
    pub args: Vec<String>,
}

/// What an adapter read out of a finished harness run.
#[derive(Debug, Clone, Default)]
pub struct HarnessRun {
    /// The agent's final text, which the output label is extracted from.
    pub text: String,

    /// The harness's own session id, when it reported one (D18/D19).
    pub session_id: Option<String>,

    /// Why the run failed, when the harness says it did. `None` means the
    /// harness is content — which says nothing yet about the output label.
    pub failure: Option<String>,
}

/// Runs an [`AgentActionConfig`] on one particular harness CLI.
///
/// Implementors are stateless value types and are shared behind an [`Arc`].
pub trait HarnessAdapter: Send + Sync + std::fmt::Debug {
    /// The name used in `harness:` in the YAML — e.g. `claude-code`.
    fn name(&self) -> &str;

    /// The binary this adapter runs.
    fn program(&self) -> &str;

    /// Builds the command line for one run.
    fn invocation(&self, request: &HarnessRequest<'_>) -> Result<HarnessInvocation, ActionError>;

    /// Reads the harness's output into the agent's final text, its session id
    /// and whether the harness considers the run a failure.
    ///
    /// A non-zero exit code is a failure for every harness, so it is passed in
    /// rather than re-derived; an adapter may add detail to it.
    fn parse(&self, stdout: &str, stderr: &str, exit_code: i32) -> HarnessRun;
}

/// The harnesses an [`AgentExecutor`](super::agent::AgentExecutor) knows.
///
/// A registry rather than a `match` so that a third harness — or a stand-in
/// pointed at a different binary — is added without touching the executor.
#[derive(Debug, Clone)]
pub struct HarnessRegistry {
    adapters: Vec<Arc<dyn HarnessAdapter>>,
}

impl HarnessRegistry {
    /// The harnesses of this version: `claude-code` and `opencode`, each on its
    /// default binary.
    pub fn defaults() -> Self {
        Self {
            adapters: vec![
                Arc::new(ClaudeCodeAdapter::new()),
                Arc::new(OpenCodeAdapter::new()),
            ],
        }
    }

    /// A registry with no adapters at all.
    pub fn empty() -> Self {
        Self {
            adapters: Vec::new(),
        }
    }

    /// Adds an adapter, replacing any adapter registered under the same name.
    pub fn with_adapter(mut self, adapter: Arc<dyn HarnessAdapter>) -> Self {
        self.adapters.retain(|known| known.name() != adapter.name());
        self.adapters.push(adapter);
        self
    }

    /// The adapter for `name`.
    ///
    /// An unknown harness names the ones that do exist: the author misspelled a
    /// value of a closed set, and the set is short enough to print.
    pub fn get(&self, name: &str) -> Result<&Arc<dyn HarnessAdapter>, ActionError> {
        self.adapters
            .iter()
            .find(|adapter| adapter.name() == name)
            .ok_or_else(|| ActionError {
                message: format!(
                    "unknown agent harness '{name}'; supported harnesses are: {}",
                    self.supported().join(", ")
                ),
                error_type: Some("unknown_harness".to_string()),
                details: Some(serde_json::json!({
                    "harness": name,
                    "supported": self.supported(),
                })),
            })
    }

    /// The registered harness names, in registration order.
    pub fn supported(&self) -> Vec<&str> {
        self.adapters.iter().map(|adapter| adapter.name()).collect()
    }
}

impl Default for HarnessRegistry {
    fn default() -> Self {
        Self::defaults()
    }
}

// ---------------------------------------------------------------------------
// Shared helpers for the adapters
// ---------------------------------------------------------------------------

/// Reads a string `parameters` entry.
///
/// A parameter of the wrong JSON type is an error rather than a silent default:
/// `permission_mode: true` is a mistake the author wants to be told about.
pub(crate) fn string_parameter<'a>(
    config: &'a AgentActionConfig,
    key: &str,
    harness: &str,
) -> Result<Option<&'a str>, ActionError> {
    match config.parameters.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.as_str())),
        Some(other) => Err(parameter_type_error(key, harness, "a string", other)),
    }
}

/// Reads a boolean `parameters` entry.
pub(crate) fn bool_parameter(
    config: &AgentActionConfig,
    key: &str,
    harness: &str,
) -> Result<Option<bool>, ActionError> {
    match config.parameters.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(other) => Err(parameter_type_error(key, harness, "a boolean", other)),
    }
}

/// Reads an integer `parameters` entry.
pub(crate) fn integer_parameter(
    config: &AgentActionConfig,
    key: &str,
    harness: &str,
) -> Result<Option<i64>, ActionError> {
    match config.parameters.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) if number.is_i64() => Ok(number.as_i64()),
        Some(other) => Err(parameter_type_error(key, harness, "an integer", other)),
    }
}

/// A `parameters` entry of the wrong type.
fn parameter_type_error(key: &str, harness: &str, expected: &str, got: &Value) -> ActionError {
    ActionError {
        message: format!("parameter '{key}' of harness '{harness}' must be {expected}, got {got}"),
        error_type: Some("invalid_parameter".to_string()),
        details: Some(serde_json::json!({ "parameter": key, "harness": harness })),
    }
}

/// Resolves a `read_only_paths` entry against the workspace.
///
/// The schema calls the paths workspace-relative; an absolute one is taken as
/// given, because a secondary repository provisioned beside the workspace is
/// naturally named absolutely.
pub(crate) fn resolve_read_only_path(workspace: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        workspace.join(candidate)
    }
}

/// The last `max` bytes of `text`, on a character boundary — for error details.
pub(crate) fn tail(text: &str, max: usize) -> String {
    let text = text.trim_end();
    if text.len() <= max {
        return text.to_string();
    }

    let mut start = text.len() - max;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }

    format!("…{}", &text[start..])
}
