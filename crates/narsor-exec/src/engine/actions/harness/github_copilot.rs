//! The `github-copilot` harness — GitHub Copilot SDK.
//!
//! This adapter runs a step through the GitHub Copilot SDK, which starts the
//! operator's GitHub Copilot CLI (`copilot` on `PATH`, or `COPILOT_CLI_PATH`)
//! in server mode and talks JSON-RPC to it. The CLI is a runtime prerequisite,
//! like `claude`; it is not bundled. The SDK runtime is in
//! [`super::copilot_sdk`], behind the `copilot-sdk` cargo feature that only
//! `narsor-runner` enables.
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

use futures::future::BoxFuture;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::copilot_sdk::{
    run_copilot_sdk, CopilotSdkBackend, CopilotSdkRequest, GitHubCopilotSdkRuntime,
};
use super::request::RunStarted;
use super::{
    model_for_tier, reject_option_like, resolve_read_only_path, string_parameter, tail,
    AgentRequest, AgentRunEnding, HarnessAdapter, HarnessRun, HarnessStreamParser, RunObservers,
};
use crate::domain::{ActionError, WorkspaceAccess};
use crate::engine::actions::answer::AnswerShape;
use narsor_protocol::api::SubagentStats;
use narsor_protocol::chat::{
    summary_line, ChatElement, TechnicalCategory, ToolResultStatus, UnclassifiedReason,
};
use narsor_protocol::controller::UsageLimit;

/// The `harness:` value this adapter answers to.
pub use narsor_protocol::harness::GITHUB_COPILOT;

/// The CLI the SDK starts, which must be on `PATH` (or named by
/// `COPILOT_CLI_PATH`).
pub const DEFAULT_PROGRAM: &str = "copilot";

const FAILURE_DETAIL_BYTES: usize = 2 * 1024;
const TECHNICAL_DATA_MAX_BYTES: usize = 1024;

/// Adapter for the GitHub Copilot SDK, run in-process.
///
/// # Workspace access is not enforced
///
/// The SDK session approves every permission request, and nothing narrows
/// it to the workflow's `workspace_access`: a read-only step can write.
/// The prompt names the read-only paths; enforcing them is a separate
/// change with its own security review
/// (`docs/follow-ups/2026-11-15-narsor-engine-split-follow-ups.md`).
#[derive(Clone)]
pub struct GitHubCopilotAdapter {
    backend: Arc<dyn CopilotSdkBackend>,
}

impl std::fmt::Debug for GitHubCopilotAdapter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GitHubCopilotAdapter")
            .finish_non_exhaustive()
    }
}

impl GitHubCopilotAdapter {
    /// An adapter running the Copilot SDK runtime of this build.
    pub fn new() -> Self {
        // A unit struct when the `copilot-sdk` feature is off.
        #[allow(clippy::default_constructed_unit_structs)]
        let runtime = GitHubCopilotSdkRuntime::default();
        Self::with_backend(Arc::new(runtime))
    }

    /// An adapter running `backend` instead of the SDK.
    pub(crate) fn with_backend(backend: Arc<dyn CopilotSdkBackend>) -> Self {
        Self { backend }
    }

    /// What the SDK is asked for, and the model it runs with.
    fn sdk_request(
        &self,
        request: &AgentRequest,
    ) -> Result<(CopilotSdkRequest, RunStarted), ActionError> {
        validate_parameters(request)?;
        reject_sandbox_parameter(request)?;
        let mut prompt = message(request);
        if let Some(system_prompt) =
            string_parameter(&request.parameters, "append_system_prompt", GITHUB_COPILOT)?
        {
            reject_option_like(system_prompt, "append_system_prompt", GITHUB_COPILOT)?;
            prompt = format!("{system_prompt}\n\n{prompt}");
        }

        let model = match &request.model {
            Some(tier) => {
                let model = model_for_tier(self, tier)?;
                reject_option_like(&model, "model", GITHUB_COPILOT)?;
                Some(model)
            }
            None => None,
        };
        if let Some(session) = &request.resume_session {
            reject_option_like(session, "resume_session", GITHUB_COPILOT)?;
        }

        let started = RunStarted {
            model: model.clone(),
            effort: None,
        };
        let sdk_request = CopilotSdkRequest {
            prompt,
            model,
            session_id: request.resume_session.clone(),
            working_dir: request.working_dir.clone(),
            action_execution_id: request.action_execution_id.clone(),
            limit: request.timeout,
        };
        Ok((sdk_request, started))
    }
}

impl Default for GitHubCopilotAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl HarnessAdapter for GitHubCopilotAdapter {
    fn model_tiers(&self) -> &'static [(&'static str, &'static str)] {
        &[
            ("small", "claude-haiku-4.5"),
            ("medium", "claude-sonnet-4.5"),
            ("large", "claude-opus-4.5"),
            ("max", "claude-opus-4.5"),
        ]
    }

    fn name(&self) -> &str {
        GITHUB_COPILOT
    }

    fn program(&self) -> &str {
        DEFAULT_PROGRAM
    }

    fn run<'a>(
        &'a self,
        request: &'a AgentRequest,
        observers: RunObservers<'a>,
    ) -> BoxFuture<'a, Result<AgentRunEnding, ActionError>> {
        Box::pin(async move {
            self.backend.check_available()?;
            let (mut sdk_request, started) = self.sdk_request(request)?;
            // The session approves every permission, so the agent reads the
            // context directory wherever it is.
            sdk_request.prompt = request.prompt_with_context(sdk_request.prompt)?;
            if request.workspace_access == Some(WorkspaceAccess::ReadOnly) {
                observers.conversation.append_outcome(
                    "github-copilot does not enforce read-only workspace access; \
                     the agent is only told not to write",
                    serde_json::json!({ "workspace_access": "read_only", "enforced": false }),
                );
            }
            // The SDK starts the CLI inside the backend's run, so this is the
            // last moment before it.
            observers.notify_started(started);
            let parser = self.stream_parser(request.answer_shape);
            run_copilot_sdk(self.backend.as_ref(), sdk_request, parser, &observers).await
        })
    }

    fn stream_parser(&self, _shape: AnswerShape) -> Box<dyn HarnessStreamParser> {
        Box::new(GitHubCopilotStreamParser::default())
    }
}

fn validate_parameters(request: &AgentRequest) -> Result<(), ActionError> {
    for key in request.parameters.keys() {
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
    if let Some(policy) = string_parameter(&request.parameters, "approval_policy", GITHUB_COPILOT)?
    {
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

/// The old `sandbox` parameter is refused: `workspace_access` replaced it.
/// The SDK has no sandbox to translate `workspace_access` into (see
/// [`GitHubCopilotAdapter`]).
fn reject_sandbox_parameter(request: &AgentRequest) -> Result<(), ActionError> {
    if request.parameters.contains_key("sandbox") {
        return Err(ActionError {
            message: "parameter 'sandbox' is no longer supported for harness 'github-copilot'; use workflow-level 'workspace_access'".to_string(),
            error_type: Some("invalid_parameter".to_string()),
            details: Some(serde_json::json!({
                "harness": GITHUB_COPILOT,
                "parameter": "sandbox"
            })),
        });
    }
    Ok(())
}

/// The prompt, naming the read-only paths the agent must not modify.
fn message(request: &AgentRequest) -> String {
    if request.read_only_paths.is_empty() {
        return request.prompt.to_string();
    }
    let paths = request
        .read_only_paths
        .iter()
        .map(|path| {
            format!(
                "- {}",
                resolve_read_only_path(&request.working_dir, path).display()
            )
        })
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
    subagents: SubagentStats,
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
                    subagent_stats: (self.subagents.spawned > 0).then_some(self.subagents),
                    turns: None,
                    duration_ms: None,
                }]
            }
            "error" => {
                self.terminal_failure = Some(error_text(value));
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
            "subagent.started" => {
                self.subagents.spawned = self.subagents.spawned.saturating_add(1);
                vec![technical_event(kind, data)]
            }
            "subagent.completed" | "subagent.failed" => {
                let stats = &mut self.subagents;
                if kind == "subagent.completed" {
                    stats.completed = stats.completed.saturating_add(1);
                } else {
                    stats.failed = stats.failed.saturating_add(1);
                }
                let tokens = data.get("totalTokens").and_then(Value::as_u64);
                stats.tokens = stats.tokens.saturating_add(tokens.unwrap_or(0));
                vec![technical_event(kind, data)]
            }
            kind if kind.starts_with("session.") || kind.starts_with("assistant.") => {
                vec![technical_event(kind, data)]
            }
            _ => vec![technical_event(kind, data)],
        }
    }
}

impl HarnessStreamParser for GitHubCopilotStreamParser {
    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

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
mod run_tests {
    use std::sync::Mutex;

    use super::super::copilot_sdk::{CopilotSdkOutcome, CopilotSdkResponse};
    use super::super::RunStarted;
    use super::*;
    use crate::engine::actions::session::conversation::tests::{tapped, RecordedSessions};

    /// Stands in for the SDK: keeps the request, plays a short session, and
    /// finishes — or times out.
    struct MockCopilotSdk {
        request: Mutex<Option<CopilotSdkRequest>>,
        times_out: bool,
    }

    impl MockCopilotSdk {
        fn new(times_out: bool) -> Arc<Self> {
            Arc::new(Self {
                request: Mutex::new(None),
                times_out,
            })
        }
    }

    impl CopilotSdkBackend for MockCopilotSdk {
        fn run<'a>(
            &'a self,
            request: CopilotSdkRequest,
            on_event: &'a mut (dyn FnMut(Value) + Send),
        ) -> BoxFuture<'a, Result<CopilotSdkOutcome, ActionError>> {
            *self.request.lock().unwrap() = Some(request);
            on_event(serde_json::json!({ "type": "session", "session_id": "sdk-session" }));
            on_event(serde_json::json!({
                "type": "assistant.message",
                "data": {"messageId": "m-1", "originatingMessageId": "r-1", "content": "hello"}
            }));
            let outcome = if self.times_out {
                CopilotSdkOutcome::TimedOut
            } else {
                CopilotSdkOutcome::Finished(CopilotSdkResponse {
                    session_id: "sdk-session".to_string(),
                    text: Some("hello".to_string()),
                    usage_limits: Vec::new(),
                })
            };
            Box::pin(async move { Ok(outcome) })
        }
    }

    /// AE5 for copilot: the request alone drives the SDK — the resolved
    /// model, the session to resume, the real read-only paths in the
    /// prompt — and the events come back as a parsed run.
    #[tokio::test]
    async fn a_request_runs_on_the_sdk_and_comes_back_parsed() {
        let sdk = MockCopilotSdk::new(false);
        let adapter = GitHubCopilotAdapter::with_backend(sdk.clone());
        let request = AgentRequest {
            model: Some("medium".into()),
            resume_session: Some("resume-me".into()),
            read_only_paths: vec!["vendor".into()],
            action_execution_id: Some("action-123".into()),
            ..AgentRequest::new("Reply with a short greeting", "/work")
        };
        let sessions = Arc::new(RecordedSessions::default());
        let conversation = tapped(&sessions, GITHUB_COPILOT);
        let heard = Mutex::new(None);
        let on_started = |started: &RunStarted| *heard.lock().unwrap() = Some(started.clone());

        let ending = adapter
            .run(
                &request,
                RunObservers::conversation(&conversation).on_started(&on_started),
            )
            .await
            .expect("the SDK ran");

        let AgentRunEnding::Finished(finished) = ending else {
            panic!("the SDK run timed out");
        };
        assert_eq!(finished.run.failure, None);
        assert_eq!(finished.run.text, "hello");
        assert_eq!(finished.run.session_id.as_deref(), Some("sdk-session"));
        assert_eq!(sessions.0.lock().unwrap().len(), 1);
        assert_eq!(
            heard
                .into_inner()
                .unwrap()
                .and_then(|started| started.model),
            Some("claude-sonnet-4.5".to_string())
        );
        let sent = sdk.request.lock().unwrap().clone().unwrap();
        assert_eq!(sent.model.as_deref(), Some("claude-sonnet-4.5"));
        assert_eq!(sent.session_id.as_deref(), Some("resume-me"));
        assert_eq!(sent.action_execution_id.as_deref(), Some("action-123"));
        assert!(sent.prompt.ends_with("- /work/vendor"), "{}", sent.prompt);
    }

    #[tokio::test]
    async fn an_sdk_run_past_its_limit_times_out() {
        let adapter = GitHubCopilotAdapter::with_backend(MockCopilotSdk::new(true));
        let ending = adapter
            .run(&AgentRequest::new("go", "/work"), RunObservers::none())
            .await
            .unwrap();
        assert!(matches!(ending, AgentRunEnding::TimedOut));
    }

    /// Runs only where the `copilot-sdk` feature is off, e.g.
    /// `cargo test -p narsor-exec` (a workspace build turns it on).
    #[cfg(not(feature = "copilot-sdk"))]
    #[tokio::test]
    async fn without_the_sdk_feature_a_run_fails_and_names_narsor_runner() {
        let error = GitHubCopilotAdapter::new()
            .run(&AgentRequest::new("go", "/work"), RunObservers::none())
            .await
            .expect_err("a build without the SDK must refuse a Copilot run");
        assert_eq!(error.error_type.as_deref(), Some("harness_unavailable"));
        assert!(error.message.contains("narsor-runner"), "{}", error.message);
    }
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
    fn the_subagents_of_a_run_are_counted_on_its_result() {
        let mut parser = GitHubCopilotStreamParser::default();
        let event = |kind: &str, data: serde_json::Value| {
            serde_json::json!({"type": kind, "data": data}).to_string()
        };
        for line in [
            event("subagent.started", serde_json::json!({"toolCallId": "a"})),
            event("subagent.started", serde_json::json!({"toolCallId": "b"})),
            event(
                "subagent.completed",
                serde_json::json!({"toolCallId": "a", "totalTokens": 7978}),
            ),
            event(
                "subagent.failed",
                serde_json::json!({"toolCallId": "b", "totalTokens": 22, "error": "x"}),
            ),
        ] {
            parser.on_stdout_line(line.as_bytes());
        }
        let elements = parser.on_stdout_line(
            serde_json::json!({"type": "result", "success": true, "text": "done"})
                .to_string()
                .as_bytes(),
        );
        let Some(ChatElement::RunResult { subagent_stats, .. }) = elements.last() else {
            panic!("expected a run result, got {elements:?}");
        };
        assert_eq!(
            *subagent_stats,
            Some(SubagentStats {
                spawned: 2,
                completed: 1,
                failed: 1,
                tokens: 8000,
                ..SubagentStats::default()
            })
        );
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
        assert_eq!(adapter.program(), "copilot");
    }
}
