//! The `codex` harness — Codex CLI's non-interactive JSONL entry point.
//!
//! Narsor deliberately uses `exec`, never the interactive TUI.  The CLI owns
//! its operator-provided authentication and profile; this adapter only fixes
//! the non-interactive approval and sandbox boundary for an action.

use serde_json::Value;

use super::{
    not_classified, reject_option_like, resolve_read_only_path, string_parameter, tail,
    HarnessAdapter, HarnessInvocation, HarnessRequest, HarnessRun, HarnessStreamParser,
};
use crate::domain::ActionError;
use crate::domain::WorkspaceAccess;
use narsor_protocol::chat::ChatElement;

/// The `harness:` value this adapter answers to.
pub const CODEX: &str = "codex";

/// The binary the adapter runs unless configured with another program.
pub const DEFAULT_PROGRAM: &str = "codex";

const FAILURE_DETAIL_BYTES: usize = 2 * 1024;
const APPROVAL_POLICY: &str = "never";

/// Adapter for the Codex CLI.
#[derive(Debug, Clone)]
pub struct CodexAdapter {
    program: String,
}

impl CodexAdapter {
    /// An adapter running `codex` from `PATH`.
    pub fn new() -> Self {
        Self {
            program: DEFAULT_PROGRAM.to_string(),
        }
    }

    /// An adapter running `program` instead — useful for an operator wrapper
    /// and for the deterministic harness fixture in tests.
    pub fn with_program(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
        }
    }
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl HarnessAdapter for CodexAdapter {
    fn name(&self) -> &str {
        CODEX
    }

    fn program(&self) -> &str {
        &self.program
    }

    fn invocation(&self, request: &HarnessRequest<'_>) -> Result<HarnessInvocation, ActionError> {
        validate_parameters(request)?;
        let sandbox = sandbox(request)?;
        let prompt = message(request);
        let mut common = Vec::new();

        if let Some(model) = &request.config.model {
            reject_option_like(model, "model", CODEX)?;
            common.extend(["--model".to_string(), model.clone()]);
        }
        common.extend([
            "--sandbox".to_string(),
            sandbox.to_string(),
            "--config".to_string(),
            format!("approval_policy={APPROVAL_POLICY}"),
        ]);

        // These are intentionally separate grammars.  In particular `resume`
        // is a subcommand, not a fresh-run option, and `--json` belongs to the
        // exec invocation before that subcommand.
        let mut args = vec!["exec".to_string()];
        args.extend(common);
        args.push("--json".to_string());
        if let Some(session) = request.resume_session {
            reject_option_like(session, "resume_session", CODEX)?;
            args.extend(["resume".to_string(), session.to_string()]);
        }
        args.push(prompt);

        Ok(HarnessInvocation {
            program: self.program.clone(),
            args,
        })
    }

    fn stream_parser(&self) -> Box<dyn HarnessStreamParser> {
        Box::new(CodexStreamParser::default())
    }
}

fn validate_parameters(request: &HarnessRequest<'_>) -> Result<(), ActionError> {
    for key in request.config.parameters.keys() {
        if matches!(key.as_str(), "approval_policy" | "resume_session") {
            continue;
        }
        return Err(ActionError {
            message: format!("parameter '{key}' is not supported by harness '{CODEX}'"),
            error_type: Some("invalid_parameter".to_string()),
            details: Some(serde_json::json!({ "harness": CODEX, "parameter": key })),
        });
    }
    if let Some(policy) = string_parameter(request.config, "approval_policy", CODEX)? {
        if policy != APPROVAL_POLICY {
            return Err(ActionError {
                message: format!(
                    "codex approval_policy must be '{APPROVAL_POLICY}', got '{policy}'"
                ),
                error_type: Some("unsafe_harness_argument".to_string()),
                details: Some(
                    serde_json::json!({ "harness": CODEX, "parameter": "approval_policy" }),
                ),
            });
        }
    }
    Ok(())
}

fn sandbox(request: &HarnessRequest<'_>) -> Result<String, ActionError> {
    if request.config.parameters.contains_key("sandbox") {
        return Err(ActionError {
            message: "parameter 'sandbox' is no longer supported for harness 'codex'; use workflow-level 'workspace_access'".to_string(),
            error_type: Some("invalid_parameter".to_string()),
            details: Some(serde_json::json!({ "harness": CODEX, "parameter": "sandbox" })),
        });
    }
    Ok(match request.workspace_access {
        WorkspaceAccess::ReadWrite => "workspace-write",
        WorkspaceAccess::ReadOnly => "read-only",
    }
    .to_string())
}

/// Codex's `--add-dir` grants workspace access, so it must not be used for
/// Narsor `read_only_paths`.  Name those paths in the prompt instead.
fn message(request: &HarnessRequest<'_>) -> String {
    if request.config.read_only_paths.is_empty() {
        return request.prompt.to_string();
    }
    let paths = request
        .config
        .read_only_paths
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
        "{}\n\nThese directories are readable but must not be modified:\n{paths}",
        request.prompt
    )
}

/// Distills Codex JSONL while retaining the strict terminal success contract.
#[derive(Debug, Default)]
struct CodexStreamParser {
    thread_id: Option<String>,
    agent_message: Option<String>,
    completed_after_message: bool,
    terminal_failure: Option<String>,
    malformed_json: bool,
}

impl CodexStreamParser {
    /// Folds one stdout line into what [`HarnessStreamParser::finish`]
    /// needs; lines that carry none of it are ignored.
    fn accumulate(&mut self, line: &[u8]) {
        let Ok(text) = std::str::from_utf8(line) else {
            self.malformed_json = true;
            return;
        };
        let Ok(value) = serde_json::from_str::<Value>(text.trim()) else {
            self.malformed_json = true;
            return;
        };
        let Some(kind) = value.get("type").and_then(Value::as_str) else {
            return;
        };
        match kind {
            "thread.started" => {
                if let Some(id) = value.get("thread_id").and_then(Value::as_str) {
                    self.thread_id = Some(id.to_string());
                }
            }
            "item.completed" => {
                let Some(item) = value.get("item") else {
                    return;
                };
                if item.get("type").and_then(Value::as_str) == Some("agent_message") {
                    if let Some(text) = item
                        .get("text")
                        .and_then(Value::as_str)
                        .or_else(|| item.get("content").and_then(Value::as_str))
                    {
                        self.agent_message = Some(text.to_string());
                    }
                }
            }
            "turn.completed" => self.completed_after_message = self.agent_message.is_some(),
            "turn.failed" | "error" => self.terminal_failure = Some(error_text(&value)),
            _ => {}
        }
    }
}

impl HarnessStreamParser for CodexStreamParser {
    /// codex output is not classified yet (R15): every line is one
    /// unclassified element.
    fn on_stdout_line(&mut self, line: &[u8]) -> Vec<ChatElement> {
        self.accumulate(line);
        not_classified()
    }

    fn finish(self: Box<Self>, exit_code: i32, stderr_tail: &str) -> HarnessRun {
        let failure = if exit_code != 0 {
            Some(format!(
                "codex exited with code {exit_code}: {}",
                tail(stderr_tail, FAILURE_DETAIL_BYTES)
            ))
        } else if let Some(error) = self.terminal_failure {
            Some(format!(
                "codex reported a terminal error: {}",
                tail(&error, FAILURE_DETAIL_BYTES)
            ))
        } else if self.malformed_json {
            Some("codex emitted invalid JSONL".to_string())
        } else if self.thread_id.is_none() {
            Some("codex stream ended without thread.started and thread_id".to_string())
        } else if self.agent_message.is_none() {
            Some("codex stream ended without a completed agent_message".to_string())
        } else if !self.completed_after_message {
            Some("codex stream ended without turn.completed after agent_message".to_string())
        } else {
            None
        };
        HarnessRun {
            text: self.agent_message.unwrap_or_default(),
            session_id: self.thread_id,
            failure,
            structured_output: None,
        }
    }
}

fn error_text(value: &Value) -> String {
    value
        .get("error")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
        })
        .or_else(|| value.get("message").and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}
