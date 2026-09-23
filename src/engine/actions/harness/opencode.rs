//! The `opencode` harness — the `opencode run` batch entry point.
//!
//! Measured against `opencode` v2.0.14:
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
//! * `--format json` gives the session id alongside the text. It is placed
//!   **last** before the message because it takes exactly one value, so the
//!   message after it cannot be read as a flag argument.
//! * `--session <id>` continues a session (D18); `--fork` is exposed through
//!   `parameters.fork` for the case where the continuation must not disturb the
//!   original.
//!
//! # Permissions
//!
//! `--auto` auto-approves every permission that is not explicitly denied. It is
//! **off** by default: with Docker isolation (D14) deferred, D15's filesystem
//! permissions are the only boundary, and turning every approval off by default
//! would make a wrong prompt a machine-wide problem instead of a workspace-wide
//! one. Set `parameters.auto_approve: true` per action to enable it. Without
//! it, a tool call opencode would have asked about is not granted, and a
//! workflow that needs one either finishes without doing the work or sits until
//! the state's timeout — which is why the action's `timeout` matters here.
//!
//! # `read_only_paths`
//!
//! `opencode run` has no `--add-dir` equivalent. Rather than drop the paths
//! silently, they are named in the message itself, so the agent knows what it
//! may look at. That is visibility only — read-only *enforcement* is D15's
//! permission bits on the provisioned directories, for both harnesses.

use super::{
    bool_parameter, reject_option_like, resolve_read_only_path, string_parameter, tail,
    HarnessAdapter, HarnessInvocation, HarnessRequest, HarnessRun,
};
use crate::domain::ActionError;
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

        if bool_parameter(config, "auto_approve", OPENCODE)?.unwrap_or(false) {
            args.push("--auto".to_string());
        }

        // Takes exactly one value, so the message cannot be swallowed.
        args.push("--format".to_string());
        args.push("json".to_string());

        args.push(message(request));

        Ok(HarnessInvocation {
            program: self.program.clone(),
            args,
        })
    }

    fn parse(&self, stdout: &str, stderr: &str, exit_code: i32) -> HarnessRun {
        let envelope: Option<Value> = serde_json::from_str(stdout.trim()).ok();

        let text = envelope
            .as_ref()
            .and_then(extract_text)
            .unwrap_or_else(|| stdout.to_string());

        let session_id = envelope.as_ref().and_then(extract_session_id);

        let failure = (exit_code != 0).then(|| {
            format!(
                "opencode exited with code {exit_code}: {}",
                tail(stderr, FAILURE_DETAIL_BYTES)
            )
        });

        HarnessRun {
            text,
            session_id,
            failure,
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
