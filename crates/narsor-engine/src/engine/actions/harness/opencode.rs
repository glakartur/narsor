//! The `opencode` harness — the `opencode run` batch entry point.
//!
//! Measured against `opencode` v2.0.15:
//!
//! ```text
//! opencode run [--model <provider/model#variant>] [--agent <agent>] \
//!              [--session <session-id>] [--auto] \
//!              --format json \
//!              <message>
//! ```
//!
//! # Why that shape
//!
//! * `run` is the non-interactive entry point; the bare `opencode` opens a TUI.
//! * `--format json` gives NDJSON emitted incrementally as the agent works,
//!   with the session id on every line's envelope (`sessionID`) and the
//!   answer's text on the last `text`-typed line — no argv change was needed
//!   for U3's streaming capture, since this harness already streamed (R13's
//!   "opencode: yes" open question). It is placed **last** before the message
//!   because it takes exactly one value, so the message after it cannot be
//!   read as a flag argument.
//! * `--session <id>` continues a session (D18); `--fork` is exposed through
//!   `parameters.fork` for the case where the continuation must not disturb the
//!   original.
//!
//! # Permissions
//!
//! `--auto` auto-approves every permission that is not explicitly denied. The
//! workflow-level `workspace_access` policy controls it: read-write actions
//! receive `--auto`, while read-only actions do not. OpenCode has no narrower
//! workspace-only flag, so the provisioner's filesystem permissions remain the
//! enforcement boundary.
//!
//! # `read_only_paths`
//!
//! `opencode run` has no `--add-dir` equivalent. Rather than drop the paths
//! silently, they are named in the message itself, so the agent knows what it
//! may look at. That is visibility only — read-only *enforcement* is D15's
//! permission bits on the provisioned directories, for both harnesses.

use super::{
    bool_parameter, not_classified, reject_option_like, resolve_read_only_path, string_parameter,
    tail, HarnessAdapter, HarnessInvocation, HarnessRequest, HarnessRun, HarnessStreamParser,
};
use crate::domain::ActionError;
use crate::domain::WorkspaceAccess;
use narsor_protocol::chat::ChatElement;
use serde_json::Value;

/// The `harness:` value this adapter answers to.
pub const OPENCODE: &str = "opencode";

/// The binary the adapter runs unless it is built with another one.
pub const DEFAULT_PROGRAM: &str = "opencode";

/// How much of stderr is quoted in a failure message.
const FAILURE_DETAIL_BYTES: usize = 2 * 1024;

/// Adapter for the `opencode` CLI.
#[derive(Debug, Clone)]
pub struct OpenCodeAdapter {
    program: String,
}

impl OpenCodeAdapter {
    /// An adapter running `opencode` from `PATH`.
    pub fn new() -> Self {
        Self {
            program: DEFAULT_PROGRAM.to_string(),
        }
    }

    /// An adapter running `program` instead.
    pub fn with_program(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
        }
    }
}

impl Default for OpenCodeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl HarnessAdapter for OpenCodeAdapter {
    fn name(&self) -> &str {
        OPENCODE
    }

    fn program(&self) -> &str {
        &self.program
    }

    fn invocation(&self, request: &HarnessRequest<'_>) -> Result<HarnessInvocation, ActionError> {
        let config = request.config;
        let mut args = vec!["run".to_string()];

        if let Some(model) = &config.model {
            reject_option_like(model, "model", OPENCODE)?;
            args.push("--model".to_string());
            args.push(model.clone());
        }

        if let Some(agent) = string_parameter(config, "agent", OPENCODE)? {
            reject_option_like(agent, "agent", OPENCODE)?;
            args.push("--agent".to_string());
            args.push(agent.to_string());
        }

        if let Some(session) = request.resume_session {
            reject_option_like(session, "resume_session", OPENCODE)?;
            args.push("--session".to_string());
            args.push(session.to_string());

            if bool_parameter(config, "fork", OPENCODE)?.unwrap_or(false) {
                args.push("--fork".to_string());
            }
        }

        if config.parameters.contains_key("auto_approve") {
            return Err(ActionError {
                message: "parameter 'auto_approve' is no longer supported for harness 'opencode'; use workflow-level 'workspace_access'".to_string(),
                error_type: Some("invalid_parameter".to_string()),
                details: Some(serde_json::json!({ "harness": OPENCODE, "parameter": "auto_approve" })),
            });
        }

        match request.workspace_access {
            Some(WorkspaceAccess::ReadWrite) => args.push("--auto".to_string()),
            Some(WorkspaceAccess::ReadOnly) | None => {}
        }

        // Takes exactly one value, so the message cannot be swallowed.
        args.push("--format".to_string());
        args.push("json".to_string());

        args.push(message(request));

        Ok(HarnessInvocation {
            program: self.program.clone(),
            args,
            stdin: None,
        })
    }

    fn stream_parser(&self) -> Box<dyn HarnessStreamParser> {
        Box::new(OpenCodeStreamParser::default())
    }
}

/// Accumulates the `--format json` NDJSON lines of one run into a
/// [`HarnessRun`].
///
/// opencode has no envelope that marks the end of the run — the process
/// exiting *is* the end — so unlike Claude Code's `result` line there is
/// nothing to wait for here: the last `text`-typed line seen wins, whichever
/// line that turns out to be.
#[derive(Debug, Default)]
struct OpenCodeStreamParser {
    /// The text of the last `text`-typed line seen.
    text: Option<String>,
    /// The most recent session id seen, from any line's envelope.
    session_id: Option<String>,
    /// The most recent `error`-typed line seen, as readable text.
    error: Option<String>,
}

impl OpenCodeStreamParser {
    /// Folds one stdout line into what [`HarnessStreamParser::finish`]
    /// needs; lines that carry none of it are ignored.
    fn accumulate(&mut self, line: &[u8]) {
        let Ok(text) = std::str::from_utf8(line) else {
            return;
        };
        let Ok(value) = serde_json::from_str::<Value>(text.trim()) else {
            return;
        };

        if let Some(session_id) = extract_session_id(&value) {
            self.session_id = Some(session_id);
        }

        let line_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        match line_type {
            // A line with no `type` at all, or one tagged `result`, is the
            // pre-streaming single-object shape some fixtures (and possibly
            // an older CLI) still produce; treated the same as an explicit
            // `text` line, since it carries the answer the same way.
            "" | "text" | "result" => {
                if let Some(text) = extract_text(&value) {
                    self.text = Some(text);
                }
            }
            "error" => {
                self.error = Some(
                    extract_text(&value)
                        .or_else(|| {
                            value
                                .get("error")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .unwrap_or_else(|| value.to_string()),
                );
            }
            _ => {}
        }
    }
}

impl HarnessStreamParser for OpenCodeStreamParser {
    /// opencode output is not classified yet (R15): every line is one
    /// unclassified element.
    fn on_stdout_line(&mut self, line: &[u8]) -> Vec<ChatElement> {
        self.accumulate(line);
        not_classified()
    }

    fn finish(self: Box<Self>, exit_code: i32, stderr_tail: &str) -> HarnessRun {
        let failure = if exit_code != 0 {
            Some(format!(
                "opencode exited with code {exit_code}: {}",
                tail(stderr_tail, FAILURE_DETAIL_BYTES)
            ))
        } else {
            self.error.as_ref().map(|error| {
                format!(
                    "opencode reported an error: {}",
                    tail(error, FAILURE_DETAIL_BYTES)
                )
            })
        };

        HarnessRun {
            text: self.text.unwrap_or_default(),
            reply: None,
            session_id: self.session_id,
            failure,
            structured_output: None,
            permission_denials: Vec::new(),
        }
    }
}

/// The message the agent receives: the prompt, plus the read-only paths this
/// harness cannot pass as a flag.
fn message(request: &HarnessRequest<'_>) -> String {
    let paths = &request.config.read_only_paths;
    if paths.is_empty() {
        return request.prompt.to_string();
    }

    let listed = paths
        .iter()
        .map(|path| {
            format!(
                "- {}",
                resolve_read_only_path(request.workspace, path).display()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "{}\n\nThese directories are readable but must not be modified:\n{listed}",
        request.prompt
    )
}

/// The assistant text of an `opencode run --format json` result.
///
/// The exact shape has moved between versions, so every known spelling is
/// tried and the raw output is the fallback: a harness that renames a field
/// must not turn into an unexplained failure.
fn extract_text(value: &Value) -> Option<String> {
    if let Some(parts) = value.get("parts").and_then(Value::as_array) {
        let text = parts
            .iter()
            .filter(|part| {
                part.get("type")
                    .and_then(Value::as_str)
                    .is_none_or(|kind| kind == "text")
            })
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");

        if !text.is_empty() {
            return Some(text);
        }
    }

    for key in ["text", "result", "message", "content"] {
        if let Some(text) = value.get(key).and_then(Value::as_str) {
            return Some(text.to_string());
        }
    }

    None
}

/// The session id of an `opencode run --format json` result.
fn extract_session_id(value: &Value) -> Option<String> {
    for key in ["sessionID", "session_id", "sessionId"] {
        if let Some(id) = value.get(key).and_then(Value::as_str) {
            return Some(id.to_string());
        }
    }

    for container in ["session", "info"] {
        if let Some(id) = value
            .get(container)
            .and_then(|nested| nested.get("id"))
            .and_then(Value::as_str)
        {
            return Some(id.to_string());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::actions::agent::run_reply;

    #[test]
    fn the_reply_is_the_final_text_without_the_contract() {
        let mut parser: Box<dyn HarnessStreamParser> = Box::new(OpenCodeStreamParser::default());
        for line in [
            serde_json::json!({ "type": "text", "sessionID": "ses-1", "text": "Looking." }),
            serde_json::json!({
                "type": "text",
                "sessionID": "ses-1",
                "text": "P1: validate the request body.\n```json\n{\"choice\":\"findings\"}\n```",
            }),
        ] {
            parser.on_stdout_line(line.to_string().as_bytes());
        }
        let run = parser.finish(0, "");

        assert_eq!(run.failure, None);
        assert_eq!(
            run_reply(&run).as_deref(),
            Some("P1: validate the request body.")
        );
    }
}
