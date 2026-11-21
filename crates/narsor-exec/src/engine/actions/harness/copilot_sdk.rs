//! `harness: github-copilot` steps, run through the GitHub Copilot SDK.
//!
//! The SDK itself is behind the `copilot-sdk` cargo feature ([`runtime`]):
//! only `narsor-runner` runs agent steps, so only it enables the feature.
//! Built without it, [`GitHubCopilotSdkRuntime`] fails every run with an
//! error that says so, instead of starting anything else.

use std::path::Path;
use std::time::Duration;

use base64::Engine;
use futures::future::BoxFuture;
use narsor_protocol::controller::UsageLimit;
use serde_json::Value;

use crate::domain::ActionError;
use crate::engine::actions::harness::HarnessStreamParser;
use crate::engine::actions::session::conversation::{
    record_stdout_line, ConversationSink, UsageRelay,
};
use crate::engine::actions::session::process::{FinishedRun, SpawnEnding};

#[cfg(feature = "copilot-sdk")]
mod runtime;
#[cfg(feature = "copilot-sdk")]
pub(crate) use runtime::GitHubCopilotSdkRuntime;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_copilot_sdk(
    args: &[String],
    working_dir: &Path,
    action_execution_id: Option<&str>,
    limit: Option<Duration>,
    parser: Box<dyn HarnessStreamParser>,
    conversation: &ConversationSink,
    usage: Option<UsageRelay<'_>>,
    sdk_runtime: &dyn CopilotSdkBackend,
) -> Result<SpawnEnding, ActionError> {
    if args.len() < 2 {
        return Err(ActionError {
            message: "copilot-sdk invocation requires encoded configuration".to_string(),
            error_type: Some("sdk_config_error".to_string()),
            details: None,
        });
    }

    let config_b64 = &args[1];
    let config_bytes = base64::engine::general_purpose::STANDARD
        .decode(config_b64)
        .map_err(|e| ActionError {
            message: format!("failed to decode SDK configuration: {}", e),
            error_type: Some("sdk_config_error".to_string()),
            details: None,
        })?;

    let config_str = String::from_utf8(config_bytes).map_err(|e| ActionError {
        message: format!("SDK configuration is not valid UTF-8: {}", e),
        error_type: Some("sdk_config_error".to_string()),
        details: None,
    })?;

    let config: Value = serde_json::from_str(&config_str).map_err(|e| ActionError {
        message: format!("failed to parse SDK configuration: {}", e),
        error_type: Some("sdk_config_error".to_string()),
        details: None,
    })?;

    let prompt = config
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ActionError {
            message: "SDK configuration missing 'prompt' field".to_string(),
            error_type: Some("sdk_config_error".to_string()),
            details: None,
        })?;
    let request = CopilotSdkRequest {
        prompt: prompt.to_string(),
        model: config
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string),
        session_id: config
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        working_dir: working_dir.to_path_buf(),
        action_execution_id: action_execution_id.map(str::to_string),
        limit,
    };

    let mut parser = parser;
    let mut on_event = |record| {
        feed_copilot_sdk_record(&mut parser, conversation, usage, record);
    };
    let outcome = sdk_runtime.run(request, &mut on_event).await?;
    drop(on_event);

    match outcome {
        CopilotSdkOutcome::TimedOut => Ok(SpawnEnding::TimedOut),
        CopilotSdkOutcome::Finished(response) => {
            feed_copilot_sdk_record(
                &mut parser,
                conversation,
                usage,
                serde_json::json!({
                    "type": "result",
                    "session_id": response.session_id,
                    "text": response.text,
                    "success": true
                }),
            );
            if !response.usage_limits.is_empty() {
                feed_copilot_sdk_record(
                    &mut parser,
                    conversation,
                    usage,
                    serde_json::json!({
                        "type": "usage",
                        "limits": response.usage_limits
                    }),
                );
            }
            Ok(SpawnEnding::Finished(FinishedRun {
                run: parser.finish(0, ""),
                exit_code: 0,
                stderr: String::new(),
                terminated_group: None,
            }))
        }
    }
}

fn feed_copilot_sdk_record(
    parser: &mut Box<dyn HarnessStreamParser>,
    conversation: &ConversationSink,
    usage: Option<UsageRelay<'_>>,
    record: Value,
) {
    let line = record.to_string();
    let elements = parser.on_stdout_line(line.as_bytes());
    record_stdout_line(
        parser.as_mut(),
        conversation,
        usage,
        line.as_bytes(),
        &elements,
        None,
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CopilotSdkRequest {
    prompt: String,
    model: Option<String>,
    session_id: Option<String>,
    working_dir: std::path::PathBuf,
    action_execution_id: Option<String>,
    limit: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CopilotSdkResponse {
    session_id: String,
    text: Option<String>,
    usage_limits: Vec<UsageLimit>,
}

// Without the `copilot-sdk` feature only the tests' fake backend has an outcome.
#[cfg_attr(not(feature = "copilot-sdk"), allow(dead_code))]
pub(crate) enum CopilotSdkOutcome {
    Finished(CopilotSdkResponse),
    TimedOut,
}

pub(crate) trait CopilotSdkBackend: Send + Sync {
    fn run<'a>(
        &'a self,
        request: CopilotSdkRequest,
        on_event: &'a mut (dyn FnMut(serde_json::Value) + Send),
    ) -> BoxFuture<'a, Result<CopilotSdkOutcome, ActionError>>;
}

/// Stands in for the SDK runtime in a build without the `copilot-sdk`
/// feature (every binary but `narsor-runner`).
#[cfg(not(feature = "copilot-sdk"))]
#[derive(Default)]
pub(crate) struct GitHubCopilotSdkRuntime;

#[cfg(not(feature = "copilot-sdk"))]
impl CopilotSdkBackend for GitHubCopilotSdkRuntime {
    fn run<'a>(
        &'a self,
        _request: CopilotSdkRequest,
        _on_event: &'a mut (dyn FnMut(serde_json::Value) + Send),
    ) -> BoxFuture<'a, Result<CopilotSdkOutcome, ActionError>> {
        Box::pin(async {
            Err(ActionError {
                message: "the GitHub Copilot harness runs only in narsor-runner: this binary \
                          was built without the `copilot-sdk` feature of narsor-exec"
                    .to_string(),
                error_type: Some("harness_unavailable".to_string()),
                details: Some(serde_json::json!({
                    "harness": super::github_copilot::GITHUB_COPILOT,
                })),
            })
        })
    }
}

#[cfg(test)]
mod copilot_sdk_dispatch_tests {
    use super::{CopilotSdkBackend, CopilotSdkOutcome, CopilotSdkRequest, CopilotSdkResponse};
    use crate::engine::actions::answer::AnswerShape;
    use crate::engine::actions::harness::{GitHubCopilotAdapter, HarnessAdapter};
    use crate::engine::actions::session::conversation::ConversationSink;
    use crate::engine::actions::session::process::{
        run_harness_invocation, run_harness_invocation_with_sdk, SpawnEnding, FINAL_ANSWER_GRACE,
    };
    use base64::Engine;
    use futures::future::BoxFuture;
    use std::path::Path;
    use std::sync::Mutex;

    struct MockCopilotSdk {
        request: Mutex<Option<CopilotSdkRequest>>,
    }

    impl CopilotSdkBackend for MockCopilotSdk {
        fn run<'a>(
            &'a self,
            request: CopilotSdkRequest,
            on_event: &'a mut (dyn FnMut(serde_json::Value) + Send),
        ) -> BoxFuture<'a, Result<CopilotSdkOutcome, crate::domain::ActionError>> {
            *self.request.lock().unwrap() = Some(request);
            on_event(serde_json::json!({
                "type": "session",
                "session_id": "sdk-session"
            }));
            on_event(serde_json::json!({
                "type": "assistant.message_delta",
                "data": {"messageId": "message-1", "deltaContent": "hello"}
            }));
            on_event(serde_json::json!({
                "type": "assistant.message",
                "data": {
                    "messageId": "message-1",
                    "originatingMessageId": "request-1",
                    "content": "hello"
                }
            }));
            Box::pin(async {
                Ok(CopilotSdkOutcome::Finished(CopilotSdkResponse {
                    session_id: "sdk-session".to_string(),
                    text: Some("hello".to_string()),
                    usage_limits: Vec::new(),
                }))
            })
        }
    }

    #[tokio::test]
    async fn agent_spawn_routes_the_copilot_sdk_marker_to_its_runtime() {
        let adapter = GitHubCopilotAdapter::new();
        let error = match run_harness_invocation(
            "copilot-sdk",
            &["agent".to_string()],
            None,
            Path::new("."),
            None,
            None,
            FINAL_ANSWER_GRACE,
            adapter.stream_parser(AnswerShape::Choice),
            &ConversationSink::none(),
            None,
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("missing SDK configuration must be rejected"),
        };

        assert_eq!(error.error_type.as_deref(), Some("sdk_config_error"));
    }

    /// Runs only where the `copilot-sdk` feature is off, e.g.
    /// `cargo test -p narsor-exec` (a workspace build turns it on).
    #[cfg(not(feature = "copilot-sdk"))]
    #[tokio::test]
    async fn copilot_step_without_the_sdk_feature_fails_and_names_narsor_runner() {
        let adapter = GitHubCopilotAdapter::new();
        let config = base64::engine::general_purpose::STANDARD
            .encode(serde_json::json!({ "prompt": "hello" }).to_string());

        let error = match run_harness_invocation(
            "copilot-sdk",
            &["agent".to_string(), config],
            None,
            Path::new("."),
            None,
            None,
            FINAL_ANSWER_GRACE,
            adapter.stream_parser(AnswerShape::Choice),
            &ConversationSink::none(),
            None,
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("a build without the SDK must refuse a Copilot step"),
        };

        assert_eq!(error.error_type.as_deref(), Some("harness_unavailable"));
        assert!(error.message.contains("narsor-runner"), "{}", error.message);
    }

    #[tokio::test]
    async fn copilot_sdk_dispatch_uses_sdk_backend_with_prompt_model_and_resume_session() {
        let adapter = GitHubCopilotAdapter::new();
        let config = base64::engine::general_purpose::STANDARD.encode(
            serde_json::json!({
                "prompt": "Reply with a short greeting",
                "model": "gpt-4",
                "session_id": "resume-me"
            })
            .to_string(),
        );
        let sdk = MockCopilotSdk {
            request: Mutex::new(None),
        };
        let result = run_harness_invocation_with_sdk(
            "copilot-sdk",
            &["agent".to_string(), config],
            None,
            Path::new("."),
            Some("action-123"),
            None,
            FINAL_ANSWER_GRACE,
            adapter.stream_parser(AnswerShape::Choice),
            &ConversationSink::none(),
            None,
            &sdk,
        )
        .await
        .expect("Copilot SDK invocation");

        match result {
            SpawnEnding::Finished(finished) => {
                assert!(
                    finished.run.failure.is_none(),
                    "the SDK response should parse successfully: {:?}",
                    finished.run.failure
                );
                assert_eq!(finished.run.session_id.as_deref(), Some("sdk-session"));
                assert_eq!(finished.run.text, "hello");
            }
            SpawnEnding::TimedOut => panic!("Copilot SDK invocation timed out"),
        }
        assert_eq!(
            sdk.request.lock().unwrap().as_ref(),
            Some(&CopilotSdkRequest {
                prompt: "Reply with a short greeting".to_string(),
                model: Some("gpt-4".to_string()),
                session_id: Some("resume-me".to_string()),
                working_dir: Path::new(".").to_path_buf(),
                action_execution_id: Some("action-123".to_string()),
                limit: None,
            })
        );
    }
}
