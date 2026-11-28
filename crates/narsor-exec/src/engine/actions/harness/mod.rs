//! Harness adapters: one implementation per AI coding CLI or SDK (D17).
//!
//! Every caller — the agent step, the profile step, the conductor — asks for
//! a run the same way: one [`AgentRequest`], one [`HarnessAdapter::run`],
//! one [`AgentRunEnding`], with the live output on [`RunObservers`]. What is
//! harness-specific stays behind `run` and nowhere else:
//!
//! * the **translation** — which flags (or SDK fields) carry the model, the
//!   effort, the extra directories, the permission mode, the session to
//!   resume;
//! * the **prompt transport** — on the command line, on stdin, or through an
//!   SDK call, so a prompt of any size reaches the agent (R5);
//! * the **context files** — written to the request's context directory and
//!   made readable (`--add-dir` for `claude`), or inlined into the prompt for
//!   a harness that cannot reach them (R6);
//! * the **output** — what the harness prints, where inside it the agent's
//!   final text and the session id are, and which harness-independent
//!   [`ChatElement`]s each printed line stands for.
//!
//! The CLI harnesses (`claude-code`, `codex`, `opencode`) share the process
//! mechanics in [`cli`]; `github-copilot` runs its SDK in-process
//! ([`copilot_sdk`]). Everything above a run — the output-label contract, the
//! session bookkeeping, the conversation file — lives once in the callers.
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
pub(crate) mod cli;
pub mod codex;
pub(crate) mod copilot_sdk;
pub mod github_copilot;
pub mod opencode;
pub mod request;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::future::BoxFuture;

use narsor_protocol::chat::{ChatElement, SubagentRef, UnclassifiedReason};
use narsor_protocol::controller::UsageLimit;
use serde_json::Value;

use super::answer::AnswerShape;
use crate::domain::ActionError;

pub use claude_code::{ClaudeCodeAdapter, CLAUDE_CODE};
pub use codex::{CodexAdapter, CODEX};
pub use github_copilot::{GitHubCopilotAdapter, GITHUB_COPILOT};
pub use opencode::{OpenCodeAdapter, OPENCODE};
pub use request::{
    AgentRequest, AgentRunEnding, ContextContent, ContextFile, ContextFiles, ContextSource,
    FinishedRun, RunObservers, RunStarted, FINAL_ANSWER_GRACE,
};

/// What an adapter read out of a finished harness run.
#[derive(Debug, Clone, Default)]
pub struct HarnessRun {
    /// The agent's final text, which the output label is extracted from.
    pub text: String,

    /// Assistant prose collected independently of the structured result.
    pub reply: Option<String>,

    /// The harness's own session id, when it reported one (D18/D19).
    pub session_id: Option<String>,

    /// Why the run failed, when the harness says it did. `None` means the
    /// harness is content — which says nothing yet about the output label.
    pub failure: Option<String>,

    /// The final answer as the harness validated it against
    /// [`AgentRequest::answer_schema`], or the object that closes the text of
    /// an agent's final message. A later line without an answer never
    /// replaces it. The caller still checks it, and falls back to `text` when
    /// it is absent or unusable.
    pub structured_output: Option<Value>,

    /// Structured permission denials emitted by Claude Code in the final
    /// `result` record. Other harnesses leave this empty.
    pub permission_denials: Vec<PermissionDenial>,

    /// The run stopped because the subscription usage limit of the harness
    /// is exhausted. Set only together with [`Self::failure`], and only by an
    /// adapter that can tell such a stop from other failures; the others
    /// leave it `None`. The session to continue is [`Self::session_id`].
    pub usage_limit_stop: Option<UsageLimitStop>,
}

/// A run that stopped on an exhausted subscription usage limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageLimitStop {
    /// When the exhausted window resets, when the harness reported it.
    pub resets_at: Option<chrono::DateTime<chrono::Utc>>,
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
    /// The subagent that made the call; `None` for the main agent.
    pub subagent: Option<SubagentRef>,
    /// The key parameter of the call on one line.
    pub summary: String,
}

/// What kind of final answer a harness's stream has delivered so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalAnswer {
    /// An answer with an output label (`choice`).
    Choice,
    /// A complete `awaiting_input` question.
    AwaitingInput,
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

    /// The subscription usage limits the last line fed to
    /// [`Self::on_stdout_line`] reported, given once: a second call, or a call
    /// after any other line, gives `None`.
    ///
    /// * `None`: the line is no usage report, and belongs in the
    ///   conversation like any other. The default, for a harness that never
    ///   reports its limits.
    /// * `Some` of an empty list: the line is a usage report without a limit
    ///   narsor knows. It is not part of the conversation, and there is
    ///   nothing to report.
    /// * `Some` of limits: the line is a usage report. It is not part of the
    ///   conversation; the limits are.
    fn take_usage(&mut self) -> Option<Vec<UsageLimit>> {
        None
    }

    /// The subagent that wrote the last line fed to [`Self::on_stdout_line`];
    /// `None` when the main agent did, and always for a harness that does not
    /// tell subagents apart (the default).
    fn last_line_subagent(&self) -> Option<SubagentRef> {
        None
    }

    /// The final answer the stream has delivered so far, once a `result`
    /// line has carried one. The executor ends the step shortly after it,
    /// whether or not the process has exited. `None` for a harness that
    /// cannot tell (the default) or while no final answer has arrived.
    fn final_answer(&self) -> Option<FinalAnswer> {
        None
    }

    /// The harness's own session id, as far as the stream has said it so
    /// far: the id [`Self::finish`] would put in [`HarnessRun::session_id`]
    /// now. `None` while no line carried one, and for a harness that cannot
    /// tell (the default).
    fn session_id(&self) -> Option<&str> {
        None
    }

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

/// Runs an [`AgentRequest`] on one particular harness (R2).
///
/// Implementors are stateless value types and are shared behind an [`Arc`].
pub trait HarnessAdapter: Send + Sync + std::fmt::Debug {
    /// The name used in `harness:` in the YAML — e.g. `claude-code`.
    fn name(&self) -> &str;

    /// The binary this harness needs on the machine: the CLI it runs, or
    /// the CLI its SDK starts.
    fn program(&self) -> &str;

    /// Runs `request` to its end: translates it, starts the harness, hands
    /// the prompt over however the harness takes one, makes the context
    /// files readable, and parses the output as it streams, reporting to
    /// `observers` on the way.
    ///
    /// An error before [`RunObservers::notify_started`] was called means
    /// the harness never started (a bad parameter, unwritable context
    /// files); an error after it, that the run broke off.
    fn run<'a>(
        &'a self,
        request: &'a AgentRequest,
        observers: RunObservers<'a>,
    ) -> BoxFuture<'a, Result<AgentRunEnding, ActionError>>;

    /// The model names of the four model tiers (`small`, `medium`, `large`,
    /// `max`), as this harness takes them. A workflow and the conductor name
    /// a tier, never a model, so the same workflow runs on every harness. An
    /// adapter that lists no tier cannot run a step that names a model;
    /// [`HarnessRegistry::check_model_tiers`] refuses it at the daemon's start.
    fn model_tiers(&self) -> &'static [(&'static str, &'static str)] {
        &[]
    }

    /// Whether the adapter passes the step's `effort` to its CLI. One that
    /// does not ignores the field; the executor says so in an event.
    fn supports_effort(&self) -> bool {
        false
    }

    /// Creates a fresh parser for one run's stdout stream.
    ///
    /// A new one per run: the parser accumulates state (a session id seen so
    /// far, whether a `result` line has appeared) that must never leak
    /// between two attempts. `shape` is what the step's final answer looks
    /// like, for a harness that ends the step on it.
    fn stream_parser(&self, shape: AnswerShape) -> Box<dyn HarnessStreamParser>;

    /// How this harness wants the answer and any `awaiting_input` question
    /// delivered, appended to the output contract; `None` when the contract
    /// alone is enough.
    fn contract_note(&self) -> Option<&str> {
        None
    }
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
    /// The harnesses of this version: `claude-code`, `codex`, `github-copilot`, and `opencode`, each on its
    /// default binary.
    pub fn defaults() -> Self {
        Self {
            adapters: vec![
                Arc::new(ClaudeCodeAdapter::new()),
                Arc::new(CodexAdapter::new()),
                Arc::new(GitHubCopilotAdapter::new()),
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

    /// Checks that every adapter names a model for every tier. A missing entry
    /// is an error at the daemon's start, not a step that fails later.
    pub fn check_model_tiers(&self) -> Result<(), String> {
        let mut missing = Vec::new();
        for adapter in &self.adapters {
            let tiers = adapter.model_tiers();
            for tier in crate::engine::step_profile::MODEL_TIERS {
                if !tiers
                    .iter()
                    .any(|(known, model)| *known == tier && !model.trim().is_empty())
                {
                    missing.push(format!("{}: {tier}", adapter.name()));
                }
            }
        }
        if missing.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "the harness model tiers have no model name for: {}",
                missing.join(", ")
            ))
        }
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
    parameters: &'a HashMap<String, Value>,
    key: &str,
    harness: &str,
) -> Result<Option<&'a str>, ActionError> {
    match parameters.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.as_str())),
        Some(other) => Err(parameter_type_error(key, harness, "a string", other)),
    }
}

/// Reads a boolean `parameters` entry.
pub(crate) fn bool_parameter(
    parameters: &HashMap<String, Value>,
    key: &str,
    harness: &str,
) -> Result<Option<bool>, ActionError> {
    match parameters.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(other) => Err(parameter_type_error(key, harness, "a boolean", other)),
    }
}

/// Reads an integer `parameters` entry.
pub(crate) fn integer_parameter(
    parameters: &HashMap<String, Value>,
    key: &str,
    harness: &str,
) -> Result<Option<i64>, ActionError> {
    match parameters.get(key) {
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

/// The model name `adapter` takes for the tier `tier`.
///
/// A value that is not a tier is an error: the workflow validator refuses it,
/// so only a templated value reaches this point wrong.
pub(crate) fn model_for_tier(
    adapter: &dyn HarnessAdapter,
    tier: &str,
) -> Result<String, ActionError> {
    adapter
        .model_tiers()
        .iter()
        .find(|(known, _)| *known == tier)
        .map(|(_, model)| (*model).to_string())
        .ok_or_else(|| ActionError {
            message: format!(
                "{}: '{tier}' is not a model tier with a model name; the tiers are {}",
                adapter.name(),
                crate::engine::step_profile::MODEL_TIERS.join(", ")
            ),
            error_type: Some("unknown_model_tier".to_string()),
            details: Some(serde_json::json!({ "harness": adapter.name(), "model": tier })),
        })
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

#[cfg(test)]
mod model_tier_tests {
    use super::*;

    #[test]
    fn every_stock_harness_names_a_model_for_every_tier() {
        HarnessRegistry::defaults()
            .check_model_tiers()
            .expect("the stock harnesses map every tier");
    }

    #[derive(Debug)]
    struct Partial;

    impl HarnessAdapter for Partial {
        fn name(&self) -> &str {
            "partial"
        }
        fn program(&self) -> &str {
            "partial"
        }
        fn run<'a>(
            &'a self,
            _: &'a AgentRequest,
            _: RunObservers<'a>,
        ) -> BoxFuture<'a, Result<AgentRunEnding, ActionError>> {
            unreachable!("not run")
        }
        fn model_tiers(&self) -> &'static [(&'static str, &'static str)] {
            &[("small", "s"), ("medium", "m")]
        }
        fn stream_parser(&self, _: AnswerShape) -> Box<dyn HarnessStreamParser> {
            unreachable!("not run")
        }
    }

    #[test]
    fn a_harness_without_a_tier_entry_fails_the_check() {
        let error = HarnessRegistry::empty()
            .with_adapter(Arc::new(Partial))
            .check_model_tiers()
            .expect_err("large and max are missing");
        assert!(
            error.contains("partial: large") && error.contains("partial: max"),
            "{error}"
        );
    }

    #[test]
    fn a_tier_without_an_entry_is_an_error_for_a_step() {
        let error = model_for_tier(&Partial, "large").expect_err("no entry");
        assert_eq!(error.error_type.as_deref(), Some("unknown_model_tier"));
        assert_eq!(model_for_tier(&Partial, "small").unwrap(), "s");
    }
}
