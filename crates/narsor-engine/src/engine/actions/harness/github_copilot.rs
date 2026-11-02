//! The `github-copilot` harness — GitHub Copilot SDK.
//!
//! The GitHub Copilot SDK provides a production-grade runtime for embedding AI
//! agents. This adapter uses the Copilot SDK via JSON-RPC instead of spawning
//! a separate CLI process. The SDK handles all communication with the GitHub
//! Copilot service.
//!
//! # Integration
//!
//! The adapter communicates with the Copilot SDK using its JSON-RPC API:
//! - Creates and manages agent sessions
//! - Sends prompts and receives streamed responses
//! - Handles model configuration and parameters
//! - Manages workspace access permissions
//!
//! # Skills Support
//!
//! System prompts can be included by passing them via the `append_system_prompt`
//! parameter, which will be prepended to the user's prompt.
//!
//! ```yaml
//! action:
//!   type: agent
//!   harness: github-copilot
//!   prompt: Analyze this code for issues
//!   parameters:
//!     append_system_prompt: |
//!       # Code Review Skill
//!       - Check for security vulnerabilities
//!       - Review performance implications
//!       - Validate error handling
//! ```
//!
//! The system prompt is prepended to the main prompt before passing to the SDK.

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

use super::{
    reject_option_like, string_parameter, tail, HarnessAdapter, HarnessInvocation, HarnessRequest,
    HarnessRun, HarnessStreamParser,
};
use crate::domain::ActionError;
use crate::domain::WorkspaceAccess;
use narsor_protocol::chat::{
    summary_line, ChatElement, TechnicalCategory, ToolResultStatus, UnclassifiedReason,
};
use narsor_protocol::controller::UsageLimit;

/// The `harness:` value this adapter answers to.
pub use narsor_protocol::harness::GITHUB_COPILOT;

/// Marker for SDK-based invocation (not a real binary).
pub const DEFAULT_PROGRAM: &str = "copilot-sdk";

const FAILURE_DETAIL_BYTES: usize = 2 * 1024;
const TECHNICAL_DATA_MAX_BYTES: usize = 1024;

/// Adapter for the GitHub Copilot SDK.
#[derive(Debug, Clone)]
pub struct GitHubCopilotAdapter {
    program: String,
}

impl GitHubCopilotAdapter {
    /// An adapter using the Copilot SDK.
    pub fn new() -> Self {
        Self {
            program: DEFAULT_PROGRAM.to_string(),
        }
    }

    /// An adapter with custom configuration (for testing).
    pub fn with_program(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
        }
    }
}

impl Default for GitHubCopilotAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl HarnessAdapter for GitHubCopilotAdapter {
    fn name(&self) -> &str {
        GITHUB_COPILOT
    }

    fn program(&self) -> &str {
        &self.program
    }

    fn invocation(&self, request: &HarnessRequest<'_>) -> Result<HarnessInvocation, ActionError> {
        validate_parameters(request)?;
        let _sandbox = sandbox(request)?;
        let mut prompt = message(request);

        if let Some(system_prompt) =
            string_parameter(request.config, "append_system_prompt", GITHUB_COPILOT)?
        {
            reject_option_like(system_prompt, "append_system_prompt", GITHUB_COPILOT)?;
            prompt = format!("{}\n\n{}", system_prompt, prompt);
        }

        // Build SDK configuration from request parameters
        let mut sdk_config = serde_json::json!({
            "prompt": prompt,
        });

        if let Some(model) = &request.config.model {
            reject_option_like(model, "model", GITHUB_COPILOT)?;
            sdk_config["model"] = Value::String(model.clone());
        }

        if let Some(session) = request.resume_session {
            reject_option_like(session, "resume_session", GITHUB_COPILOT)?;
            sdk_config["session_id"] = Value::String(session.to_string());
        }

        // Return SDK marker invocation with encoded configuration
        Ok(HarnessInvocation {
            program: self.program.clone(),
            args: vec!["agent".to_string(), STANDARD.encode(sdk_config.to_string())],
            stdin: None,
        })
    }

    fn stream_parser(&self) -> Box<dyn HarnessStreamParser> {
        Box::new(GitHubCopilotStreamParser::default())
    }
}

fn validate_parameters(request: &HarnessRequest<'_>) -> Result<(), ActionError> {
    for key in request.config.parameters.keys() {
        if matches!(
            key.as_str(),
            "approval_policy" | "append_system_prompt" | "resume_session"
        ) {
            continue;
        }
        return Err(ActionError {
            message: format!("parameter '{key}' is not supported by harness '{GITHUB_COPILOT}'"),
            error_type: Some("invalid_parameter".to_string()),
            details: Some(serde_json::json!({
                "harness": GITHUB_COPILOT,
                "parameter": key
            })),
        });
    }
    if let Some(policy) = string_parameter(request.config, "approval_policy", GITHUB_COPILOT)? {
        if !["auto", "ask", "deny"].contains(&policy) {
            return Err(ActionError {
                message: format!(
                    "github-copilot approval_policy must be 'auto', 'ask', or 'deny', got '{policy}'"
                ),
                error_type: Some("unsafe_harness_argument".to_string()),
                details: Some(serde_json::json!({
                    "harness": GITHUB_COPILOT,
                    "parameter": "approval_policy"
                })),
            });
        }
    }
    Ok(())
}

fn sandbox(request: &HarnessRequest<'_>) -> Result<Option<String>, ActionError> {
    if request.config.parameters.contains_key("sandbox") {
        return Err(ActionError {
            message: "parameter 'sandbox' is no longer supported for harness 'github-copilot'; use workflow-level 'workspace_access'".to_string(),
            error_type: Some("invalid_parameter".to_string()),
            details: Some(serde_json::json!({
                "harness": GITHUB_COPILOT,
                "parameter": "sandbox"
            })),
        });
    }
    Ok(request.workspace_access.map(|access| {
        match access {
            WorkspaceAccess::ReadWrite => "workspace-write",
            WorkspaceAccess::ReadOnly => "read-only",
        }
        .to_string()
    }))
}

fn message(request: &HarnessRequest<'_>) -> String {
    if request.config.read_only_paths.is_empty() {
        return request.prompt.to_string();
    }
    let paths = request
        .config
        .read_only_paths
        .iter()
        .map(|_path| "- (read-only context)")
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{}\n\nThese directories are readable but must not be modified:\n{paths}",
        request.prompt
    )
}

/// Distills GitHub Copilot SDK session events.
#[derive(Debug, Default)]
struct GitHubCopilotStreamParser {
    session_id: Option<String>,
    agent_message: Option<String>,
    completed: bool,
    terminal_failure: Option<String>,
    malformed_json: bool,
    usage: Option<Vec<UsageLimit>>,
    streamed_messages: HashSet<String>,
    reasoning_seen: bool,
    calls: HashMap<String, (String, String)>,
}

impl GitHubCopilotStreamParser {
    /// Folds one SDK event into the final run and shared conversation model.
    fn accumulate(&mut self, value: &Value) -> Vec<ChatElement> {
        self.usage = None;
        let Some(kind) = value.get("type").and_then(Value::as_str) else {
            return vec![ChatElement::Unclassified {
                reason: UnclassifiedReason::UnknownType,
            }];
        };
        let null = Value::Null;
        let data = value.get("data").unwrap_or(&null);
        match kind {
            "session" => {
                let session_id = value
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if self.session_id.is_none() {
                    self.session_id = session_id.clone();
                }
                vec![ChatElement::SessionStarted {
                    session_id,
                    model: None,
                    cli_version: None,
                    cwd: None,
                    permission_mode: None,
                }]
            }
            "assistant" => {
                if let Some(content) = value.get("content").and_then(Value::as_str) {
                    self.agent_message = Some(content.to_string());
                    return vec![ChatElement::AgentText {
                        text: content.to_string(),
                    }];
                }
                Vec::new()
            }
            "result" => {
                if let Some(id) = value.get("session_id").and_then(Value::as_str) {
                    if self.session_id.is_none() {
                        self.session_id = Some(id.to_string());
                    }
                }
                if let Some(text) = value.get("text").and_then(Value::as_str) {
                    self.agent_message = Some(text.to_string());
                }
                let success = value
                    .get("success")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if !success {
                    if let Some(error) = value.get("error").and_then(Value::as_str) {
                        self.terminal_failure = Some(error.to_string());
                    }
                }
                self.completed = true;
                vec![ChatElement::RunResult {
                    text: value
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    structured_output: None,
                    stop_reason: None,
                    is_error: !success,
                    cost_usd: None,
                    usage: None,
                    turns: None,
                    duration_ms: None,
                }]
            }
            "error" => {
                self.terminal_failure = Some(error_text(&value));
                vec![technical_event(kind, data)]
            }
            "usage" => {
                self.usage = Some(
                    value
                        .get("limits")
                        .and_then(|limits| serde_json::from_value(limits.clone()).ok())
                        .unwrap_or_default(),
                );
                Vec::new()
            }
            "user.message" => data
                .get("content")
                .and_then(Value::as_str)
                .map(|text| {
                    vec![ChatElement::UserMessage {
                        text: text.to_string(),
                    }]
                })
                .unwrap_or_else(|| vec![technical_event(kind, data)]),
            "assistant.message_delta" => {
                let message_id = data
                    .get("messageId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !message_id.is_empty() {
                    self.streamed_messages.insert(message_id.to_string());
                }
                data.get("deltaContent")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(|text| {
                        vec![ChatElement::AgentText {
                            text: text.to_string(),
                        }]
                    })
                    .unwrap_or_default()
            }
            "assistant.reasoning_delta" => {
                self.reasoning_seen = true;
                data.get("deltaContent")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(|text| {
                    vec![ChatElement::Thinking {
                        text: text.to_string(),
                        redacted: false,
                    }]
                })
                .unwrap_or_default()
            }
            "assistant.reasoning" => {
                self.reasoning_seen = true;
                data.get("content")
                .and_then(Value::as_str)
                .map(|text| {
                    vec![ChatElement::Thinking {
                        text: text.to_string(),
                        redacted: false,
                    }]
                })
                .unwrap_or_else(|| vec![technical_event(kind, data)])
            }
            "assistant.message" => {
                let content = data.get("content").and_then(Value::as_str);
                let has_tool_requests = data
                    .get("toolRequests")
                    .and_then(Value::as_array)
                    .is_some_and(|requests| !requests.is_empty());
                if !has_tool_requests {
                    if let Some(content) = content {
                        self.agent_message = Some(content.to_string());
                    }
                }
                let message_id = data
                    .get("messageId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let mut elements = Vec::new();
                if !std::mem::take(&mut self.reasoning_seen) {
                    if let Some(text) = data
                        .get("reasoningText")
                        .and_then(Value::as_str)
                        .filter(|text| !text.trim().is_empty())
                    {
                        elements.push(ChatElement::Thinking {
                            text: text.to_string(),
                            redacted: false,
                        });
                    }
                }
                if let Some(text) = content.filter(|text| !text.trim().is_empty()) {
                    if !self.streamed_messages.contains(message_id) {
                        elements.push(ChatElement::AgentText {
                            text: text.to_string(),
                        });
                    }
                }
                elements
            }
            "tool.execution_start" => {
                let call_id = data
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let tool = data
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let input = data.get("arguments").cloned().unwrap_or(Value::Null);
                let summary = tool_summary(&input);
                if !call_id.is_empty() {
                    self.calls
                        .insert(call_id.clone(), (tool.clone(), summary.clone()));
                }
                vec![ChatElement::ToolCall {
                    call_id: Some(call_id),
                    tool,
                    summary,
                    input,
                }]
            }
            "tool.execution_complete" => {
                let call_id = data
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let (tool, call_summary) = self
                    .calls
                    .remove(&call_id)
                    .unwrap_or_else(|| ("unknown".to_string(), String::new()));
                let success = data
                    .get("success")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let content = data
                    .get("result")
                    .and_then(|result| {
                        result
                            .get("detailedContent")
                            .or_else(|| result.get("content"))
                    })
                    .and_then(Value::as_str)
                    .or_else(|| {
                        data.get("error")
                            .and_then(|error| error.get("message"))
                            .and_then(Value::as_str)
                    })
                    .unwrap_or_default();
                vec![ChatElement::ToolResult {
                    call_id: Some(call_id),
                    tool: Some(tool),
                    call_summary: Some(call_summary),
                    status: if success {
                        ToolResultStatus::Ok
                    } else {
                        ToolResultStatus::Error
                    },
                    content: summary_line(content),
                    truncated: content.chars().count() > 200,
                }]
            }
            kind if kind.starts_with("session.") || kind.starts_with("assistant.") => {
                vec![technical_event(kind, data)]
            }
            _ => vec![technical_event(kind, data)],
        }
    }
}

impl HarnessStreamParser for GitHubCopilotStreamParser {
    fn on_stdout_line(&mut self, line: &[u8]) -> Vec<ChatElement> {
        let Ok(text) = std::str::from_utf8(line) else {
            self.malformed_json = true;
            return vec![ChatElement::Unclassified {
                reason: UnclassifiedReason::InvalidJson,
            }];
        };
        let Ok(value) = serde_json::from_str::<Value>(text.trim()) else {
            self.malformed_json = true;
            return vec![ChatElement::Unclassified {
                reason: UnclassifiedReason::InvalidJson,
            }];
        };
        self.accumulate(&value)
    }

    fn take_usage(&mut self) -> Option<Vec<UsageLimit>> {
        self.usage.take()
    }

    fn finish(self: Box<Self>, exit_code: i32, stderr_tail: &str) -> HarnessRun {
        let failure = if exit_code != 0 {
            Some(format!(
                "copilot exited with code {exit_code}: {}",
                tail(stderr_tail, FAILURE_DETAIL_BYTES)
            ))
        } else if let Some(error) = self.terminal_failure {
            Some(format!(
                "copilot reported a terminal error: {}",
                tail(&error, FAILURE_DETAIL_BYTES)
            ))
        } else if self.malformed_json {
            Some("copilot SDK emitted invalid event JSON".to_string())
        } else if self.session_id.is_none() {
            Some("copilot stream ended without session information".to_string())
        } else if self.agent_message.is_none() {
            Some("copilot stream ended without an assistant message".to_string())
        } else if !self.completed {
            Some("copilot stream ended without a result line".to_string())
        } else {
            None
        };
        HarnessRun {
            text: self.agent_message.unwrap_or_default(),
            reply: None,
            session_id: self.session_id,
            failure,
            structured_output: None,
            permission_denials: Vec::new(),
        }
    }
}

fn technical_event(kind: &str, data: &Value) -> ChatElement {
    let detail = match kind {
        "tool.execution_progress" => data.get("progressMessage").and_then(Value::as_str),
        "tool.shell_output" => data.get("text").and_then(Value::as_str),
        "tool.execution_partial_result" => data.get("partialOutput").and_then(Value::as_str),
        _ => None,
    };
    let summary = detail
        .map(|detail| summary_line(&format!("{kind}: {detail}")))
        .unwrap_or_else(|| summary_line(kind));
    ChatElement::Technical {
        category: TechnicalCategory::System,
        summary,
        data: if data.is_null() {
            None
        } else {
            serde_json::to_vec(data)
                .ok()
                .filter(|encoded| encoded.len() <= TECHNICAL_DATA_MAX_BYTES)
                .map(|_| data.clone())
        },
    }
}

fn tool_summary(input: &Value) -> String {
    [
        "command",
        "path",
        "filePath",
        "file_path",
        "file",
        "query",
        "url",
    ]
    .iter()
    .find_map(|key| input.get(key).and_then(Value::as_str))
    .map(summary_line)
    .unwrap_or_default()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_session_from_first_line() {
        let mut parser: Box<dyn HarnessStreamParser> =
            Box::new(GitHubCopilotStreamParser::default());
        parser.on_stdout_line(
            serde_json::json!({
                "type": "session",
                "session_id": "copilot-session-123"
            })
            .to_string()
            .as_bytes(),
        );

        let run = parser.finish(0, "");
        assert_eq!(run.session_id, Some("copilot-session-123".to_string()));
    }

    #[test]
    fn requires_result_line_for_success() {
        let mut parser: Box<dyn HarnessStreamParser> =
            Box::new(GitHubCopilotStreamParser::default());
        parser.on_stdout_line(
            serde_json::json!({
                "type": "session",
                "session_id": "copilot-session-123"
            })
            .to_string()
            .as_bytes(),
        );
        parser.on_stdout_line(
            serde_json::json!({
                "type": "assistant",
                "content": "Here is my answer"
            })
            .to_string()
            .as_bytes(),
        );

        let run = parser.finish(0, "");
        assert!(run.failure.is_some());
        assert!(run.failure.unwrap().contains("result line"));
    }

    #[test]
    fn parses_complete_successful_run() {
        let mut parser: Box<dyn HarnessStreamParser> =
            Box::new(GitHubCopilotStreamParser::default());
        for line in [
            serde_json::json!({"type": "session", "session_id": "copilot-123"}),
            serde_json::json!({"type": "assistant", "content": "My response"}),
            serde_json::json!({"type": "result", "text": "My response", "session_id": "copilot-123", "success": true}),
        ] {
            parser.on_stdout_line(line.to_string().as_bytes());
        }

        let run = parser.finish(0, "");
        assert_eq!(run.failure, None);
        assert_eq!(run.text, "My response");
        assert_eq!(run.session_id, Some("copilot-123".to_string()));
    }

    #[test]
    fn classifies_streamed_assistant_text_without_duplicate_final_text() {
        let mut parser = GitHubCopilotStreamParser::default();
        let delta = parser.on_stdout_line(
            serde_json::json!({
                "type": "assistant.message_delta",
                "data": {"messageId": "message-1", "deltaContent": "Hello"}
            })
            .to_string()
            .as_bytes(),
        );
        assert_eq!(
            delta,
            vec![ChatElement::AgentText {
                text: "Hello".to_string()
            }]
        );

        let completed = parser.on_stdout_line(
            serde_json::json!({
                "type": "assistant.message",
                "data": {
                    "messageId": "message-1",
                    "originatingMessageId": "request-1",
                    "content": "Hello"
                }
            })
            .to_string()
            .as_bytes(),
        );
        assert!(completed.is_empty());
        assert_eq!(parser.agent_message.as_deref(), Some("Hello"));
    }

    #[test]
    fn classifies_user_messages_and_correlated_tool_calls_and_results() {
        let mut parser = GitHubCopilotStreamParser::default();
        let user = parser.on_stdout_line(
            serde_json::json!({
                "type": "user.message",
                "data": {"content": "Inspect this file"}
            })
            .to_string()
            .as_bytes(),
        );
        assert_eq!(
            user,
            vec![ChatElement::UserMessage {
                text: "Inspect this file".to_string()
            }]
        );
        let call = parser.on_stdout_line(
            serde_json::json!({
                "type": "tool.execution_start",
                "data": {
                    "toolCallId": "call-1",
                    "toolName": "read_file",
                    "arguments": {"path": "src/main.rs"}
                }
            })
            .to_string()
            .as_bytes(),
        );
        assert!(matches!(
            &call[0],
            ChatElement::ToolCall {
                call_id: Some(id),
                tool,
                summary,
                ..
            } if id == "call-1" && tool == "read_file" && summary == "src/main.rs"
        ));
        let result = parser.on_stdout_line(
            serde_json::json!({
                "type": "tool.execution_complete",
                "data": {
                    "toolCallId": "call-1",
                    "success": true,
                    "result": {"content": "file contents"}
                }
            })
            .to_string()
            .as_bytes(),
        );
        assert!(matches!(
            &result[0],
            ChatElement::ToolResult {
                call_id: Some(id),
                tool: Some(tool),
                call_summary: Some(summary),
                status: ToolResultStatus::Ok,
                content,
                ..
            } if id == "call-1" && tool == "read_file" && summary == "src/main.rs" && content == "file contents"
        ));
    }

    #[test]
    fn subscription_usage_is_reported_once_and_not_part_of_chat() {
        let reset = chrono::Utc::now() + chrono::Duration::days(1);
        let mut parser = GitHubCopilotStreamParser::default();
        let elements = parser.on_stdout_line(
            serde_json::json!({
                "type": "usage",
                "limits": [{
                    "window": "30d",
                    "utilization": 0.4,
                    "resets_at": reset,
                    "status": "allowed"
                }]
            })
            .to_string()
            .as_bytes(),
        );

        assert!(elements.is_empty());
        assert_eq!(
            parser.take_usage(),
            Some(vec![UsageLimit {
                window: "30d".to_string(),
                utilization: 0.4,
                resets_at: reset,
                status: Some(narsor_protocol::controller::UsageStatus::Allowed),
            }])
        );
        assert_eq!(parser.take_usage(), None);
    }

    #[test]
    fn github_copilot_is_registered_in_defaults() {
        use super::super::{HarnessRegistry, GITHUB_COPILOT};

        let registry = HarnessRegistry::defaults();
        let result = registry.get(GITHUB_COPILOT);

        assert!(
            result.is_ok(),
            "GitHub Copilot adapter should be registered in HarnessRegistry"
        );

        let adapter = result.unwrap();
        assert_eq!(adapter.name(), GITHUB_COPILOT);
        assert_eq!(adapter.program(), "copilot-sdk");
    }
}
