//! The `claude-code` harness — Anthropic's `claude` CLI.
//!
//! Measured against `claude` 2.1.302. The invocation is always the
//! non-interactive one:
//!
//! ```text
//! claude --print --output-format stream-json --verbose \
//!        [--add-dir <read_only_paths…>] [--model <model>] [--max-turns <n>] \
//!        [--append-system-prompt <text>] [--resume <session-id>] \
//!        [--json-schema <schema>] \
//!        --permission-mode <mode> \
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
//! * `--json-schema <schema>` carries the output contract as a JSON Schema,
//!   one compact argument that starts with `{`, so it can never be read as a
//!   flag. The CLI gives the model a `StructuredOutput` tool with that input
//!   schema, rejects a call that does not match it and lets the model try
//!   again, and reports the accepted object as `structured_output` on the
//!   `result` line — also after `--resume` and in `plan` mode. The API
//!   refuses `anyOf`/`oneOf`/`allOf` at the top level of the schema and
//!   requires `"type": "object"` there, so the schema is one flat object
//!   (see [`crate::engine::actions::agent::output_schema`]). The text contract stays in
//!   the prompt: the schema adds to it, and narsor checks the answer itself.
//! * `--permission-mode` is **last** before the prompt on purpose. It takes
//!   exactly one value, so the prompt after it can never be mistaken for a flag
//!   argument — unlike `--add-dir`, which is variadic and would swallow the
//!   prompt if it stood there.
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

use std::path::Path;

use serde_json::{Map, Value};

use super::{
    integer_parameter, reject_option_like, resolve_read_only_path, string_parameter, tail,
    HarnessAdapter, HarnessInvocation, HarnessRequest, HarnessRun, HarnessStreamParser,
};
use crate::domain::ActionError;
use crate::domain::WorkspaceAccess;
use narsor_protocol::chat::ChatElement;

/// The `harness:` value this adapter answers to.
pub const CLAUDE_CODE: &str = "claude-code";

/// The binary the adapter runs unless it is built with another one.
pub const DEFAULT_PROGRAM: &str = "claude";

/// The native Claude mode for a writable workspace.
pub const DEFAULT_PERMISSION_MODE: &str = "acceptEdits";

/// How much of stderr is quoted in a failure message.
const FAILURE_DETAIL_BYTES: usize = 2 * 1024;

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
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
        ];

        // Variadic, so it must never be the last flag before the prompt.
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

        args.push("--permission-mode".to_string());
        args.push(mode.to_string());

        args.push(request.prompt.to_string());

        Ok(HarnessInvocation {
            program: self.program.clone(),
            args,
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
///   `user`, `system`, `rate_limit_event` and `result` line becomes
///   conversation elements, with tool results tied to their calls.
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
    /// `is_error`, or `subtype` naming anything other than `success`.
    reported_error: bool,
    /// The `result` line's `subtype`, for the failure message.
    subtype: Option<String>,
    /// The `result` line's `structured_output`, when it carried one.
    structured_output: Option<Value>,
}

impl ClaudeCodeStreamParser {
    /// Folds one parsed stdout line into what
    /// [`HarnessStreamParser::finish`] needs; lines that carry none of it
    /// are ignored.
    fn accumulate(&mut self, object: &Map<String, Value>) {
        let line_type = object.get("type").and_then(Value::as_str).unwrap_or("");

        if line_type == "system" {
            if self.session_id.is_none() {
                self.session_id = object
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            return;
        }

        if line_type != "result" {
            return;
        }

        self.saw_result = true;
        if let Some(session_id) = object.get("session_id").and_then(Value::as_str) {
            self.session_id = Some(session_id.to_string());
        }

        self.subtype = object
            .get("subtype")
            .and_then(Value::as_str)
            .map(str::to_string);
        let is_error = object
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let non_success_subtype = self
            .subtype
            .as_deref()
            .is_some_and(|subtype| subtype != "success");
        self.reported_error = is_error || non_success_subtype;
        self.structured_output = object
            .get("structured_output")
            .filter(|value| !value.is_null())
            .cloned();

        self.result_text = object
            .get("result")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                object
                    .get("errors")
                    .and_then(Value::as_array)
                    .map(|errors| {
                        errors
                            .iter()
                            .map(error_message)
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
            })
            .unwrap_or_default();
    }
}

impl HarnessStreamParser for ClaudeCodeStreamParser {
    fn on_stdout_line(&mut self, line: &[u8]) -> Vec<ChatElement> {
        let Some(object) = parse_object(line) else {
            return classify::invalid_json();
        };
        self.accumulate(&object);
        self.classifier.classify(&object)
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

        HarnessRun {
            text: self.result_text,
            session_id: self.session_id,
            failure,
            structured_output: self.structured_output,
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

/// One element of a `result` line's `errors` array, as readable text.
/// Claude Code 2.1.302 sends plain strings; older versions sent objects
/// with a `message`.
fn error_message(error: &Value) -> String {
    error
        .as_str()
        .or_else(|| error.get("message").and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| error.to_string())
}

/// The permission mode of this run, validated against what the CLI accepts.
fn permission_mode<'a>(request: &HarnessRequest<'a>) -> Result<&'a str, ActionError> {
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
    Ok(match request.workspace_access {
        WorkspaceAccess::ReadWrite => DEFAULT_PERMISSION_MODE,
        WorkspaceAccess::ReadOnly => "plan",
    })
}

/// One `--add-dir` entry: the path as the agent will see it.
fn read_only_arg(workspace: &Path, path: &str) -> String {
    resolve_read_only_path(workspace, path)
        .to_string_lossy()
        .into_owned()
}
