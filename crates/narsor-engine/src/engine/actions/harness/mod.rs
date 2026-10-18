//! Harness adapters: one implementation per AI coding CLI (D17).
//!
//! An [`AgentActionConfig`] says *what* the agent should do; a
//! [`HarnessAdapter`] says *how* a particular command-line harness is asked to
//! do it. Two things are harness-specific and nothing else is:
//!
//! * the **argv** — which flags carry the model, the extra directories, the
//!   permission mode, the session to resume, and where the prompt goes;
//! * the **output** — what the harness prints, where inside it the agent's
//!   final text and the session id are, and which harness-independent
//!   [`ChatElement`]s each printed line stands for.
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
pub mod codex;
pub mod opencode;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use narsor_protocol::chat::{ChatElement, UnclassifiedReason};
use serde_json::Value;

use crate::domain::{ActionError, AgentActionConfig, WorkspaceAccess};

pub use claude_code::{ClaudeCodeAdapter, CLAUDE_CODE};
pub use codex::{CodexAdapter, CODEX};
pub use opencode::{OpenCodeAdapter, OPENCODE};

/// Everything an adapter needs to build one invocation.
#[derive(Debug, Clone, Copy)]
pub struct HarnessRequest<'a> {
    /// The action configuration, templates already expanded.
    pub config: &'a AgentActionConfig,

    /// Workflow-level workspace access, already parsed independently of the
    /// selected harness.
    pub workspace_access: Option<WorkspaceAccess>,

    /// The prompt as the harness will receive it: the state's prompt with the
    /// output-label contract appended.
    pub prompt: &'a str,

    /// The directory the harness runs in — the execution's workspace.
    pub workspace: &'a Path,

    /// A harness session to continue instead of starting a fresh one (D18).
    pub resume_session: Option<&'a str>,

    /// The JSON Schema of the answer, for a harness that can enforce one.
    ///
    /// An addition to the text contract in `prompt`, never a replacement:
    /// `claude-code` passes it as `--json-schema`, and every other adapter
    /// ignores it.
    pub output_schema: Option<&'a Value>,
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

    /// The answer as the harness validated it against
    /// [`HarnessRequest::output_schema`], when it reported one. The caller
    /// still checks it, and falls back to `text` when it is absent or unusable.
    pub structured_output: Option<Value>,

    /// Structured permission denials emitted by Claude Code in the final
    /// `result` record. Other harnesses leave this empty.
    pub permission_denials: Vec<PermissionDenial>,
}

/// A Claude Code permission denial correlated to the attempted tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionDenial {
    pub tool_name: String,
    pub tool_use_id: String,
    pub input: Value,
    pub reason_type: Option<String>,
    pub reason: Option<String>,
    pub session_id: Option<String>,
}

/// A stateful parser fed one stdout line at a time as the harness streams,
/// which classifies each line into [`ChatElement`]s as it arrives and
/// distills a [`HarnessRun`] once the process has ended.
///
/// Streaming means no single buffer ever holds the whole run: the executor
/// hands each line to the parser and writes it to the conversation store,
/// together with the elements the parser returned for it. Beyond those
/// elements, this trait's job is to remember exactly what building a
/// [`HarnessRun`] needs (the final text, the session id, a reported failure)
/// and nothing more. A harness line is fed here only when it is at most
/// [`super::agent::PARSE_LINE_LIMIT`] bytes; a longer line is still written
/// to the conversation in full (R14), with an
/// [`UnclassifiedReason::TooLarge`] element instead of the parser's — in
/// every observed harness the final result never arrives on an oversized
/// line, only bulky tool output does.
pub trait HarnessStreamParser: Send {
    /// Feeds one stdout line, exactly as the harness printed it (no trailing
    /// newline), and returns what the line stands for in the conversation:
    /// zero, one or several elements, in order.
    ///
    /// Not necessarily valid UTF-8 or valid JSON — a line the adapter cannot
    /// make sense of becomes an [`ChatElement::Unclassified`] element, never
    /// an error.
    fn on_stdout_line(&mut self, line: &[u8]) -> Vec<ChatElement>;

    /// Distills the run once the process has exited.
    ///
    /// `exit_code` is a failure for every harness regardless of what the
    /// stream said, so it is passed in rather than re-derived; `stderr_tail`
    /// is quoted into a failure message the same way it always was.
    fn finish(self: Box<Self>, exit_code: i32, stderr_tail: &str) -> HarnessRun;
}

/// The elements of a line from a harness whose adapter does not classify
/// its output yet (R15): the raw line is all there is to show.
pub(crate) fn not_classified() -> Vec<ChatElement> {
    vec![ChatElement::Unclassified {
        reason: UnclassifiedReason::HarnessNotClassified,
    }]
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

    /// Creates a fresh parser for one run's stdout stream.
    ///
    /// A new one per run: the parser accumulates state (a session id seen so
    /// far, whether a `result` line has appeared) that must never leak
    /// between two attempts.
    fn stream_parser(&self) -> Box<dyn HarnessStreamParser>;
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
    /// The harnesses of this version: `claude-code`, `codex`, and `opencode`, each on its
    /// default binary.
    pub fn defaults() -> Self {
        Self {
            adapters: vec![
                Arc::new(ClaudeCodeAdapter::new()),
                Arc::new(CodexAdapter::new()),
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
/// `max_turns: true` is a mistake the author wants to be told about.
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

/// Rejects a value the harness CLI would read as an option rather than a value.
///
/// Every value below is pushed into argv in a flag-value position, so one
/// beginning with `-` becomes another flag. That is not merely untidy: the
/// permission mode is checked against a whitelist, which is the one field a
/// reviewer reads before trusting a workflow, while `model` and the rest were
/// passed through raw — so `model: "--allow-dangerously-skip-permissions"`
/// would smuggle an escalation straight past that review. Validating only the
/// obvious field and leaving an innocuous one open is the whole bug.
pub(crate) fn reject_option_like(
    value: &str,
    field: &str,
    harness: &str,
) -> Result<(), ActionError> {
    if !value.starts_with('-') {
        return Ok(());
    }

    Err(ActionError {
        message: format!(
            "{harness}: {field} '{value}' starts with '-', which the harness \
             would read as an option rather than a value"
        ),
        error_type: Some("unsafe_harness_argument".to_string()),
        details: Some(serde_json::json!({
            "harness": harness,
            "field": field,
            "value": value,
        })),
    })
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
