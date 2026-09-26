//! The `claude-code` harness — Anthropic's `claude` CLI.
//!
//! Measured against `claude` 2.1.283. The invocation is always the
//! non-interactive one:
//!
//! ```text
//! claude --print --output-format stream-json --verbose \
//!        [--add-dir <read_only_paths…>] [--model <model>] [--max-turns <n>] \
//!        [--resume <session-id>] [--allow-dangerously-skip-permissions] \
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
//! whether the run hangs. The default here is [`DEFAULT_PERMISSION_MODE`]
//! (`acceptEdits`): the agent edits files in its workspace without asking, and
//! anything heavier (running commands, reaching the network) is refused rather
//! than waited on. `bypassPermissions` is available through
//! `parameters.permission_mode` and is *not* the default: with Docker isolation
//! (D14) still deferred, the only thing between an agent and the rest of the
//! machine is D15's filesystem permissions, which guard against a mistake and
//! not against a determined agent.

use std::path::Path;

use serde_json::Value;

use super::{
    bool_parameter, integer_parameter, reject_option_like, resolve_read_only_path,
    string_parameter, tail, HarnessAdapter, HarnessInvocation, HarnessRequest, HarnessRun,
    HarnessStreamParser,
};
use crate::domain::ActionError;

/// The `harness:` value this adapter answers to.
pub const CLAUDE_CODE: &str = "claude-code";

/// The binary the adapter runs unless it is built with another one.
pub const DEFAULT_PROGRAM: &str = "claude";

/// The permission mode used when `parameters.permission_mode` says nothing.
///
/// Deliberately not the most permissive one — see the module documentation.
pub const DEFAULT_PERMISSION_MODE: &str = "acceptEdits";

/// The modes `claude --permission-mode` accepts.
pub const PERMISSION_MODES: [&str; 6] = [
    "acceptEdits",
    "auto",
    "bypassPermissions",
    "manual",
    "dontAsk",
    "plan",
];

/// The mode that turns every permission check off, and which therefore also
/// needs `--allow-dangerously-skip-permissions` to be accepted at all.
const BYPASS_MODE: &str = "bypassPermissions";

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

        if mode == BYPASS_MODE {
            args.push("--allow-dangerously-skip-permissions".to_string());
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

/// Accumulates the `stream-json` lines of one run into a [`HarnessRun`].
///
/// Tracks lines by their `type`: a `system`/`init` line may carry the session
/// id before anything else does, and the `result` line — the last line a
/// well-behaved run ever prints — carries the final text (or `errors`), the
/// authoritative session id, and whether the harness itself calls the run a
/// failure. Every other line (`assistant`, `user`, `rate_limit_event`, …) is
/// only ever written to the conversation file; this parser has no use for it.
#[derive(Debug, Default)]
struct ClaudeCodeStreamParser {
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
}

impl HarnessStreamParser for ClaudeCodeStreamParser {
    fn on_stdout_line(&mut self, line: &[u8]) {
        let Ok(text) = std::str::from_utf8(line) else {
            return;
        };
        let Ok(value) = serde_json::from_str::<Value>(text.trim()) else {
            return;
        };
        let Some(object) = value.as_object() else {
            return;
        };

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
        }
    }
}

/// One element of a `result` line's `errors` array, as readable text.
fn error_message(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| error.to_string())
}

/// The permission mode of this run, validated against what the CLI accepts.
fn permission_mode<'a>(request: &HarnessRequest<'a>) -> Result<&'a str, ActionError> {
    let Some(mode) = string_parameter(request.config, "permission_mode", CLAUDE_CODE)? else {
        // `parameters.dangerously_skip_permissions: true` is the short spelling
        // of the same decision; it is kept because it reads as the warning it
        // is, and because it matches the CLI's own flag name.
        let skip = bool_parameter(request.config, "dangerously_skip_permissions", CLAUDE_CODE)?
            .unwrap_or(false);
        return Ok(if skip {
            BYPASS_MODE
        } else {
            DEFAULT_PERMISSION_MODE
        });
    };

    if !PERMISSION_MODES.contains(&mode) {
        return Err(ActionError {
            message: format!(
                "unknown permission_mode '{mode}' for harness '{CLAUDE_CODE}'; \
                 supported modes are: {}",
                PERMISSION_MODES.join(", ")
            ),
            error_type: Some("invalid_parameter".to_string()),
            details: Some(serde_json::json!({
                "parameter": "permission_mode",
                "supported": PERMISSION_MODES,
            })),
        });
    }

    Ok(mode)
}

/// One `--add-dir` entry: the path as the agent will see it.
fn read_only_arg(workspace: &Path, path: &str) -> String {
    resolve_read_only_path(workspace, path)
        .to_string_lossy()
        .into_owned()
}
