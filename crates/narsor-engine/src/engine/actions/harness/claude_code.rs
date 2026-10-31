//! The `claude-code` harness — Anthropic's `claude` CLI.
//!
//! Measured against `claude` 2.1.302. The invocation is always the
//! non-interactive one:
//!
//! ```text
//! claude --print --verbose --permission-prompts none \
//!        [--add-dir <read_only_paths…>] [--allowedTools <rule>] \
//!        --disallowedTools ScheduleWakeup CronCreate Monitor \
//!        [--model <model>] [--max-turns <n>] \
//!        [--append-system-prompt <text>] [--resume <session-id>] \
//!        [--json-schema <schema>] \
//!        --output-format stream-json [--permission-mode <mode>] \
//!        <prompt>
//! ```
//!
//! # Why that shape
//!
//! * `--print` is what makes the run batch: the CLI answers and exits instead
//!   of opening a session on a terminal narsor does not have.
//! * `--output-format stream-json --verbose` gives NDJSON: one line per
//!   message as the agent works, ending in a `result` line with the final
//!   text *and* the session id (the session id may also arrive earlier, on a
//!   `system`/`init` line). This reverses an earlier decision to use plain
//!   `json`: that format gave only the final answer and left everything in
//!   between unrecorded, which is no longer good enough once R9 requires the
//!   *whole* stream captured, byte for byte, as it happens (U3). `--verbose`
//!   is required by the CLI for `stream-json` under `--print`; without it the
//!   command refuses to start.
//! * `--json-schema <schema>` is passed only when the caller gives an output
//!   schema (the conductor does; the agent step does not). The CLI then gives
//!   the model a `StructuredOutput` tool, rejects a call that does not match
//!   the schema, and reports the accepted object as `structured_output` on the
//!   `result` line. That tool ends the turn, which is why the agent step
//!   passes no schema: its answer is the closing JSON object of the last
//!   message, read from the text of the `result` line (see
//!   [`crate::engine::actions::agent::closing_answer`]). The API refuses
//!   `anyOf`/`oneOf`/`allOf` at the top level of a schema and requires
//!   `"type": "object"` there.
//! * The flag right before the prompt always takes exactly one value —
//!   `--permission-mode` when the workflow sets `workspace_access`, else
//!   `--output-format` — so the prompt can never be read as a flag argument.
//!   `--add-dir` and `--allowedTools` are variadic and come first: right
//!   before the prompt either would swallow it, and `--resume` without a
//!   prompt makes the CLI look for a deferred tool call to continue and fail.
//! * `--disallowedTools` denies the tools that schedule a later turn: a
//!   pending one keeps the `--print` process alive until it fires, so a step
//!   would wait for it (20 minutes, in the case seen) after its answer.
//!   `Monitor` is denied for the same reason. A background task started with
//!   `Bash` (a polling loop) or by a subagent cannot be denied without
//!   denying `Bash`; instead the executor ends the step shortly after the
//!   final answer and kills the process group the harness runs in.
//! * `--resume <id>` is always passed *with* its value: the flag's value is
//!   optional, and a bare `--resume` would consume the prompt.
//!
//! # The permission mode, and what it costs
//!
//! Under `--print` nothing can be prompted for, so the mode decides what the
//! agent may do on its own and what is simply refused — it never decides
//! whether the run hangs. Workflow-level `workspace_access` is translated to
//! `acceptEdits` for read-write work and `plan` for read-only work. This keeps
//! the workflow contract portable instead of exposing Claude-specific modes.

mod classify;
mod tools;
mod usage;

use std::path::Path;

use serde_json::{Map, Value};

use super::{
    integer_parameter, reject_option_like, resolve_read_only_path, string_parameter, tail,
    FinalAnswer, HarnessAdapter, HarnessInvocation, HarnessRequest, HarnessRun,
    HarnessStreamParser, PermissionDenial,
};
use crate::domain::ActionError;
use crate::domain::WorkspaceAccess;
use crate::engine::actions::agent;
use narsor_protocol::chat::ChatElement;
use narsor_protocol::controller::UsageLimit;

/// The `harness:` value this adapter answers to.
pub use narsor_protocol::harness::CLAUDE_CODE;

/// The binary the adapter runs unless it is built with another one.
pub const DEFAULT_PROGRAM: &str = "claude";

/// The native Claude mode for a writable workspace.
pub const DEFAULT_PERMISSION_MODE: &str = "acceptEdits";

/// How much of stderr is quoted in a failure message.
const FAILURE_DETAIL_BYTES: usize = 2 * 1024;

/// Tools that schedule a later turn. Under `--print` the CLI does not exit
/// while one is pending, so the step waits for it long after the answer;
/// subagent hand-backs already wake the agent without them. `Monitor` keeps
/// a background watcher alive the same way.
const DEFERRING_TOOLS: [&str; 3] = ["ScheduleWakeup", "CronCreate", "Monitor"];

/// Adapter for the `claude` CLI.
#[derive(Debug, Clone)]
pub struct ClaudeCodeAdapter {
    program: String,
}

impl ClaudeCodeAdapter {
    /// An adapter running `claude` from `PATH`.
    pub fn new() -> Self {
        Self {
            program: DEFAULT_PROGRAM.to_string(),
        }
    }

    /// An adapter running `program` instead — an absolute path, a wrapper
    /// script, or the fake harness the test suite uses.
    pub fn with_program(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
        }
    }
}

impl Default for ClaudeCodeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl HarnessAdapter for ClaudeCodeAdapter {
    fn name(&self) -> &str {
        CLAUDE_CODE
    }

    fn program(&self) -> &str {
        &self.program
    }

    fn invocation(&self, request: &HarnessRequest<'_>) -> Result<HarnessInvocation, ActionError> {
        let config = request.config;
        let mode = permission_mode(request)?;

        let mut args = vec![
            "--print".to_string(),
            "--verbose".to_string(),
            "--permission-prompts".to_string(),
            "none".to_string(),
        ];

        // The variadic flags come first: each is followed by another flag,
        // never by the prompt, which they would swallow.
        if !config.read_only_paths.is_empty() {
            args.push("--add-dir".to_string());
            // No option-like guard here: resolve_read_only_path joins a
            // relative entry onto the workspace and leaves an absolute one
            // alone, so the result always begins with '/' and can never be
            // read as a flag. A guard here would never fire, and a guard that
            // cannot fire reads like protection that is not there.
            for path in &config.read_only_paths {
                args.push(read_only_arg(request.workspace, path));
            }
        }

        if !config.permission_grants.is_empty() {
            args.push("--allowedTools".to_string());
            args.extend(config.permission_grants.iter().cloned());
        }

        args.push("--disallowedTools".to_string());
        args.extend(DEFERRING_TOOLS.iter().map(|tool| tool.to_string()));

        if let Some(model) = &config.model {
            reject_option_like(model, "model", CLAUDE_CODE)?;
            args.push("--model".to_string());
            args.push(model.clone());
        }

        if let Some(max_turns) = integer_parameter(config, "max_turns", CLAUDE_CODE)? {
            args.push("--max-turns".to_string());
            args.push(max_turns.to_string());
        }

        if let Some(system_prompt) = string_parameter(config, "append_system_prompt", CLAUDE_CODE)?
        {
            reject_option_like(system_prompt, "append_system_prompt", CLAUDE_CODE)?;
            args.push("--append-system-prompt".to_string());
            args.push(system_prompt.to_string());
        }

        if let Some(session) = request.resume_session {
            reject_option_like(session, "resume_session", CLAUDE_CODE)?;
            args.push("--resume".to_string());
            args.push(session.to_string());
        }

        if let Some(schema) = request.output_schema {
            let schema = schema.to_string();
            reject_option_like(&schema, "output_schema", CLAUDE_CODE)?;
            args.push("--json-schema".to_string());
            args.push(schema);
        }

        // Single-valued flags close the list, so whatever stands right
        // before the prompt takes exactly one value.
        args.push("--output-format".to_string());
        args.push("stream-json".to_string());

        if let Some(mode) = mode {
            args.push("--permission-mode".to_string());
            args.push(mode.to_string());
        }

        args.push(request.prompt.to_string());

        Ok(HarnessInvocation {
            program: self.program.clone(),
            args,
            stdin: None,
        })
    }

    fn stream_parser(&self) -> Box<dyn HarnessStreamParser> {
        Box::new(ClaudeCodeStreamParser::default())
    }
}

/// Reads the `stream-json` lines of one run, live, line by line.
///
/// Each line is parsed once and used twice:
///
/// * It is classified into chat elements ([`classify`]): every `assistant`,
///   `user`, `system` and `result` line becomes conversation elements, with
///   tool results tied to their calls. A `rate_limit_event` line is no part
///   of the conversation: it gives no element, and its limits ([`usage`]) wait
///   for [`HarnessStreamParser::take_usage`].
/// * It is accumulated into a [`HarnessRun`]: a `system`/`init` line may
///   carry the session id before anything else does, and the `result` line
///   — the last line a well-behaved run ever prints — carries the final text
///   (or `errors`), the `structured_output` when a schema was passed, the
///   authoritative session id, and whether the harness itself calls the run
///   a failure.
#[derive(Debug, Default)]
struct ClaudeCodeStreamParser {
    /// Turns lines into chat elements; remembers tool calls by id.
    classifier: classify::Classifier,
    /// The earliest session id seen, from `system`/`init` or `result`.
    session_id: Option<String>,
    /// Whether a `result` line was ever seen — its absence is itself a
    /// failure (see [`HarnessStreamParser::finish`]).
    saw_result: bool,
    /// `result`/`errors` of the `result` line, already turned into text.
    result_text: String,
    reply_text: String,
    /// `is_error`, or `subtype` naming anything other than `success`.
    reported_error: bool,
    /// The `result` line's `subtype`, for the failure message.
    subtype: Option<String>,
    /// The latest final answer a `result` line carried. A later line without
    /// a usable answer, or with an error, never replaces it.
    structured_output: Option<Value>,
    final_answer: Option<FinalAnswer>,
    permission_denials: Vec<PermissionDenial>,
    denial_reasons: std::collections::HashMap<String, (Option<String>, Option<String>)>,
    /// The limits of the last line, when it was a `rate_limit_event`.
    usage: Option<Vec<UsageLimit>>,
}

impl ClaudeCodeStreamParser {
    /// Folds one parsed stdout line into what
    /// [`HarnessStreamParser::finish`] needs; lines that carry none of it
    /// are ignored. `result` is what [`classify::ResultLine::take`] read
    /// from a `result` line.
    fn accumulate(&mut self, object: &Map<String, Value>, result: Option<&classify::ResultLine>) {
        let session_id = || {
            object
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        };

        let Some(result) = result else {
            if object.get("type").and_then(Value::as_str) == Some("assistant")
                && object.get("parent_tool_use_id").is_none_or(Value::is_null)
            {
                if let Some(content) = object
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_array)
                {
                    // In order: a tool call drops the prose before it, even
                    // within one message.
                    for block in content {
                        match block.get("type").and_then(Value::as_str) {
                            Some("tool_use")
                                if block.get("name").and_then(Value::as_str)
                                    != Some("StructuredOutput") =>
                            {
                                self.reply_text.clear();
                            }
                            Some("text") => {
                                if let Some(text) = block.get("text").and_then(Value::as_str) {
                                    if !self.reply_text.is_empty() {
                                        self.reply_text.push('\n');
                                    }
                                    self.reply_text.push_str(text);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            if self.session_id.is_none()
                && object.get("type").and_then(Value::as_str) == Some("system")
            {
                self.session_id = session_id();
            }
            return;
        };

        self.saw_result = true;
        if let Some(session_id) = session_id() {
            self.session_id = Some(session_id);
        }
        self.subtype = result.subtype.clone();
        self.reported_error = result.is_error;
        // The conductor's answer arrives as `structured_output`, an agent's
        // as the closing object of the text. A later `result` line without
        // an answer (a subagent hand-back, a note while a background task
        // runs) never drops the answer already kept. An answer on an error
        // line is none. Any object is kept as it comes — an answer the
        // engine will reject, or the conductor's own shape — but only a
        // usable answer is a final one that ends the step.
        let answered = if result.is_error {
            None
        } else if let Some(value) = &result.structured_output {
            Some((value.clone(), agent::final_answer_kind(value)))
        } else {
            result
                .text
                .as_deref()
                .and_then(agent::closing_answer)
                .map(|(value, kind)| (value, Some(kind)))
        };
        self.permission_denials = result.permission_denials.clone();
        let Some((value, kind)) = answered else {
            // Neither the answer nor the text it came from is replaced.
            if result.is_error || self.structured_output.is_none() {
                self.result_text = result.text.clone().unwrap_or_default();
            }
            return;
        };
        self.structured_output = Some(value);
        self.final_answer = kind;
        self.result_text = result.text.clone().unwrap_or_default();
    }
}

impl HarnessStreamParser for ClaudeCodeStreamParser {
    fn on_stdout_line(&mut self, line: &[u8]) -> Vec<ChatElement> {
        // Limits belong to the line that reported them, and to no later one.
        self.usage = None;
        let Some(mut object) = parse_object(line) else {
            return classify::invalid_json();
        };
        if object.get("type").and_then(Value::as_str) == Some(usage::RATE_LIMIT_EVENT) {
            self.usage = Some(usage::limits(&object));
            return Vec::new();
        }
        if object.get("type").and_then(Value::as_str) == Some("system")
            && object.get("subtype").and_then(Value::as_str) == Some("permission_denied")
        {
            if let Some(id) = object.get("tool_use_id").and_then(Value::as_str) {
                self.denial_reasons.insert(
                    id.to_string(),
                    (
                        object
                            .get("decision_reason_type")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        object
                            .get("message")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    ),
                );
            }
        }
        let mut result = classify::ResultLine::take(&mut object);
        if let Some(result) = result.as_mut() {
            for denial in &mut result.permission_denials {
                if let Some((reason_type, reason)) = self.denial_reasons.get(&denial.tool_use_id) {
                    denial.reason_type = reason_type.clone();
                    denial.reason = reason.clone();
                }
            }
        }
        self.accumulate(&object, result.as_ref());
        self.classifier.classify(object, result)
    }

    fn take_usage(&mut self) -> Option<Vec<UsageLimit>> {
        self.usage.take()
    }

    fn final_answer(&self) -> Option<FinalAnswer> {
        self.final_answer
    }

    fn finish(self: Box<Self>, exit_code: i32, stderr_tail: &str) -> HarnessRun {
        let failure = if exit_code != 0 {
            Some(format!(
                "claude exited with code {exit_code}: {}",
                tail(stderr_tail, FAILURE_DETAIL_BYTES)
            ))
        } else if !self.saw_result {
            Some(format!(
                "harness finished without a result (no 'result' line was seen \
                 in its stream-json output); exit code {exit_code}, stderr: {}",
                tail(stderr_tail, FAILURE_DETAIL_BYTES)
            ))
        } else if self.reported_error {
            let subtype = self.subtype.as_deref().unwrap_or("unknown");
            Some(format!(
                "claude reported an error result ({subtype}): {}",
                tail(&self.result_text, FAILURE_DETAIL_BYTES)
            ))
        } else {
            None
        };

        let reply = if self.reply_text.trim().is_empty() {
            self.result_text.clone()
        } else {
            self.reply_text
        };
        HarnessRun {
            text: self.result_text,
            reply: Some(reply),
            session_id: self.session_id,
            failure,
            structured_output: self.structured_output,
            permission_denials: self.permission_denials,
        }
    }
}

/// A stdout line as a JSON object; `None` when it is not UTF-8, not JSON,
/// or not an object.
fn parse_object(line: &[u8]) -> Option<Map<String, Value>> {
    let text = std::str::from_utf8(line).ok()?;
    match serde_json::from_str::<Value>(text.trim()).ok()? {
        Value::Object(object) => Some(object),
        _ => None,
    }
}

/// The permission mode of this run, validated against what the CLI accepts.
fn permission_mode<'a>(request: &HarnessRequest<'a>) -> Result<Option<&'a str>, ActionError> {
    if request.config.parameters.contains_key("permission_mode")
        || request
            .config
            .parameters
            .contains_key("dangerously_skip_permissions")
    {
        return Err(ActionError {
            message: format!(
                "harness permission parameters are not supported for '{CLAUDE_CODE}'; \
                 use workflow-level 'workspace_access'"
            ),
            error_type: Some("invalid_parameter".to_string()),
            details: Some(serde_json::json!({
                "harness": CLAUDE_CODE,
                "parameter": "permission_mode",
            })),
        });
    }
    Ok(request.workspace_access.map(|access| match access {
        WorkspaceAccess::ReadWrite => DEFAULT_PERMISSION_MODE,
        WorkspaceAccess::ReadOnly => "plan",
    }))
}

/// One `--add-dir` entry: the path as the agent will see it.
fn read_only_arg(workspace: &Path, path: &str) -> String {
    resolve_read_only_path(workspace, path)
        .to_string_lossy()
        .into_owned()
}
