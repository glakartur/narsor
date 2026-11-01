use std::fmt::Display;
use std::future::Future;
use std::path::Path;
use std::time::Duration;

use base64::Engine;
use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use github_copilot_sdk::{
    CliProgram, Client, ClientOptions, MessageOptions, ResumeSessionConfig, SessionConfig,
    SessionId,
};
use narsor_protocol::controller::{UsageLimit, UsageStatus};
use serde_json::Value;

use crate::domain::ActionError;
use crate::engine::actions::agent::conversation::{
    record_stdout_line, ConversationSink, UsageRelay,
};
use crate::engine::actions::agent::process::{FinishedRun, SpawnEnding};
use crate::engine::actions::harness::HarnessStreamParser;
use crate::engine::actions::ACTION_ID_ENV_VAR;

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

/// Reads the subscription quota snapshot maintained by the Copilot SDK.
fn copilot_usage_limits(
    projection: &github_copilot_sdk::rpc::SessionQuotaProjection,
) -> Vec<UsageLimit> {
    let Some(quota) = projection.snapshots.get("premium_interactions") else {
        return Vec::new();
    };
    if quota.is_unlimited_entitlement || !quota.remaining_percentage.is_finite() {
        return Vec::new();
    }
    let Some(reset_millis) = quota.reset_date_epoch_ms else {
        return Vec::new();
    };
    if !reset_millis.is_finite() || reset_millis < i64::MIN as f64 || reset_millis > i64::MAX as f64
    {
        return Vec::new();
    }
    let Some(resets_at) = DateTime::<Utc>::from_timestamp_millis(reset_millis as i64) else {
        return Vec::new();
    };
    if resets_at <= Utc::now() {
        return Vec::new();
    }

    let status = match quota.has_quota {
        Some(true) => Some(UsageStatus::Allowed),
        Some(false) if quota.usage_allowed_with_exhausted_quota => Some(UsageStatus::Allowed),
        Some(false) => Some(UsageStatus::Rejected),
        None => None,
    };
    vec![UsageLimit {
        // Copilot reports a monthly premium-interaction allowance; the shared
        // usage contract represents its cycle with the closest supported label.
        window: "30d".to_string(),
        utilization: (100.0 - quota.remaining_percentage.clamp(0.0, 100.0)) / 100.0,
        resets_at,
        status,
    }]
}

async fn run_with_sdk_deadline<F: Future>(
    deadline: Option<tokio::time::Instant>,
    future: F,
) -> Result<F::Output, tokio::time::error::Elapsed> {
    match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, future).await,
        None => Ok(future.await),
    }
}

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

pub(crate) struct GitHubCopilotSdkRuntime {
    program: CliProgram,
}

impl Default for GitHubCopilotSdkRuntime {
    fn default() -> Self {
        Self {
            program: CliProgram::Resolve,
        }
    }
}

impl CopilotSdkBackend for GitHubCopilotSdkRuntime {
    fn run<'a>(
        &'a self,
        request: CopilotSdkRequest,
        on_event: &'a mut (dyn FnMut(serde_json::Value) + Send),
    ) -> BoxFuture<'a, Result<CopilotSdkOutcome, ActionError>> {
        Box::pin(async move {
            let mut options = ClientOptions::default();
            options.program = self.program.clone();
            options.working_directory = request.working_dir.clone();
            if let Some(action_execution_id) = &request.action_execution_id {
                options
                    .env
                    .push((ACTION_ID_ENV_VAR.into(), action_execution_id.into()));
            }

            let deadline = request
                .limit
                .and_then(|limit| tokio::time::Instant::now().checked_add(limit));
            let client = match run_with_sdk_deadline(deadline, Client::start(options)).await {
                Ok(Ok(client)) => client,
                Ok(Err(error)) => {
                    return Err(copilot_sdk_error("failed to start Copilot SDK", error));
                }
                Err(_) => return Ok(CopilotSdkOutcome::TimedOut),
            };
            let run_client = client.clone();
            let run = async move {
                let session = if let Some(session_id) = request.session_id {
                    let mut config = ResumeSessionConfig::new(SessionId::new(session_id))
                        .with_working_directory(request.working_dir.clone())
                        .approve_all_permissions();
                    if let Some(model) = request.model {
                        config = config.with_model(model);
                    }
                    run_client.resume_session(config).await
                } else {
                    let mut config = SessionConfig::default()
                        .with_working_directory(request.working_dir.clone())
                        .approve_all_permissions();
                    if let Some(model) = request.model {
                        config = config.with_model(model);
                    }
                    run_client.create_session(config).await
                }
                .map_err(|error| {
                    copilot_sdk_error("failed to create Copilot SDK session", error)
                })?;

                let session_id = session.id().to_string();
                let mut events = session.subscribe();
                on_event(serde_json::json!({
                    "type": "session",
                    "session_id": session_id.clone()
                }));
                let message_id = session
                    .send(MessageOptions::new(request.prompt))
                    .await
                    .map_err(|error| copilot_sdk_error("Copilot SDK request failed", error))?;
                let mut request_started = false;
                let mut final_message = None;
                let response = loop {
                    let event = events.recv().await.map_err(|error| {
                        copilot_sdk_error("failed to receive Copilot SDK event", error)
                    })?;
                    let event_type = event.event_type.as_str();
                    let is_root = event.agent_id.as_deref().is_none_or(str::is_empty);
                    let data = &event.data;

                    let record = serde_json::to_value(&event).map_err(|error| {
                        copilot_sdk_error("failed to serialize Copilot SDK event", error)
                    })?;
                    on_event(record);

                    if is_root && event_type == "user.message" {
                        request_started |= data.get("messageId").and_then(Value::as_str)
                            == Some(message_id.as_str());
                    }
                    if is_root
                        && event_type == "session.warning"
                        && data.get("warningType").and_then(Value::as_str) == Some("policy_blocked")
                        && data.get("originatingMessageId").and_then(Value::as_str)
                            == Some(message_id.as_str())
                    {
                        request_started = true;
                    }
                    if is_root && event_type == "assistant.message" {
                        let origin = data.get("originatingMessageId").and_then(Value::as_str);
                        if origin == Some(message_id.as_str())
                            || (origin.is_none() && request_started)
                        {
                            request_started = true;
                            final_message = if data
                                .get("toolRequests")
                                .and_then(Value::as_array)
                                .is_some_and(|tools| !tools.is_empty())
                            {
                                None
                            } else {
                                Some(data.clone())
                            };
                        }
                    }

                    if event_type == "session.quota_observation" {
                        match session.rpc().quota().get().await {
                            Ok(projection) => {
                                let limits = copilot_usage_limits(&projection);
                                if !limits.is_empty() {
                                    on_event(serde_json::json!({
                                        "type": "usage",
                                        "limits": limits
                                    }));
                                }
                            }
                            Err(error) => {
                                tracing::debug!(%error, "could not read Copilot subscription usage");
                            }
                        }
                    }

                    if !is_root {
                        continue;
                    }
                    match event_type {
                        "session.idle" if request_started => {
                            if data.get("aborted").and_then(Value::as_bool) == Some(true) {
                                break Err(copilot_sdk_error(
                                    "Copilot SDK request failed",
                                    "session aborted before the response completed",
                                ));
                            }
                            if data.get("mode").and_then(Value::as_str) != Some("autopilot") {
                                break Ok(final_message);
                            }
                        }
                        "session.autopilot_stopped"
                            if data.get("requestId").and_then(Value::as_str)
                                == Some(message_id.as_str()) =>
                        {
                            match data.get("reason").and_then(Value::as_str) {
                                Some("error") => {
                                    break Err(copilot_sdk_error(
                                        "Copilot SDK request failed",
                                        "autopilot request failed",
                                    ));
                                }
                                Some("aborted") => {
                                    break Err(copilot_sdk_error(
                                        "Copilot SDK request failed",
                                        "session aborted before the response completed",
                                    ));
                                }
                                _ => break Ok(final_message),
                            }
                        }
                        "session.error"
                            if data
                                .get("originatingMessageId")
                                .and_then(Value::as_str)
                                .filter(|id| !id.is_empty())
                                .is_none_or(|origin| origin == message_id) =>
                        {
                            let message = data
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("session error");
                            break Err(copilot_sdk_error("Copilot SDK request failed", message));
                        }
                        _ => {}
                    }
                };
                let usage_limits = match session.rpc().quota().get().await {
                    Ok(projection) => copilot_usage_limits(&projection),
                    Err(error) => {
                        tracing::debug!(%error, "could not read Copilot subscription usage");
                        Vec::new()
                    }
                };
                let disconnect = session.disconnect().await.map_err(|error| {
                    copilot_sdk_error("failed to disconnect Copilot SDK session", error)
                });
                let response = match (response, disconnect) {
                    (Ok(response), Ok(())) => response,
                    (Err(error), Ok(())) | (Ok(_), Err(error)) => return Err(error),
                    (Err(error), Err(cleanup_error)) => {
                        return Err(combine_sdk_errors(error, cleanup_error));
                    }
                };
                Ok(CopilotSdkResponse {
                    session_id,
                    text: response.and_then(|event| {
                        event
                            .get("content")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    }),
                    usage_limits,
                })
            };

            let run_and_stop = async {
                let response = run.await;
                let stop = client
                    .stop()
                    .await
                    .map_err(|error| copilot_sdk_error("failed to stop Copilot SDK", error));

                match (response, stop) {
                    (Ok(response), Ok(())) => Ok(response),
                    (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
                    (Err(error), Err(stop_error)) => Err(combine_sdk_errors(error, stop_error)),
                }
            };

            match run_with_sdk_deadline(deadline, run_and_stop).await {
                Ok(Ok(response)) => Ok(CopilotSdkOutcome::Finished(response)),
                Ok(Err(error)) => Err(error),
                Err(_) => {
                    client.force_stop();
                    Ok(CopilotSdkOutcome::TimedOut)
                }
            }
        })
    }
}

fn copilot_sdk_error(context: &str, error: impl Display) -> ActionError {
    ActionError {
        message: format!("{context}: {error}"),
        error_type: Some("sdk_runtime_error".to_string()),
        details: None,
    }
}

fn combine_sdk_errors(primary: ActionError, cleanup: ActionError) -> ActionError {
    ActionError {
        message: format!(
            "{}; cleanup also failed: {}",
            primary.message, cleanup.message
        ),
        error_type: primary.error_type,
        details: primary.details,
    }
}

#[cfg(test)]
mod copilot_sdk_dispatch_tests {
    use super::{
        copilot_usage_limits, run_with_sdk_deadline, CopilotSdkBackend, CopilotSdkOutcome,
        CopilotSdkRequest, CopilotSdkResponse,
    };
    use crate::engine::actions::agent::conversation::ConversationSink;
    use crate::engine::actions::agent::process::{
        run_harness_invocation, run_harness_invocation_with_sdk, SpawnEnding, FINAL_ANSWER_GRACE,
    };
    use crate::engine::actions::harness::{GitHubCopilotAdapter, HarnessAdapter};
    use base64::Engine;
    use chrono::Utc;
    use futures::future::BoxFuture;
    use narsor_protocol::controller::UsageStatus;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::Duration;

    #[tokio::test]
    async fn copilot_sdk_deadline_covers_all_lifecycle_phases() {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(40);
        let result = run_with_sdk_deadline(Some(deadline), async {
            for _ in 0..3 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;

        assert!(result.is_err(), "the shared deadline should expire");
    }

    #[test]
    fn copilot_subscription_quota_maps_remaining_percentage_and_reset() {
        use github_copilot_sdk::rpc::{SessionQuotaProjection, SessionQuotaSnapshot};

        let reset = Utc::now() + chrono::Duration::days(5);
        let projection = SessionQuotaProjection {
            snapshots: [(
                "premium_interactions".to_string(),
                SessionQuotaSnapshot {
                    remaining_percentage: 37.5,
                    reset_date_epoch_ms: Some(reset.timestamp_millis() as f64),
                    has_quota: Some(true),
                    ..Default::default()
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };

        let limits = copilot_usage_limits(&projection);
        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].window, "30d");
        assert_eq!(limits[0].utilization, 0.625);
        assert_eq!(
            limits[0].resets_at.timestamp_millis(),
            reset.timestamp_millis()
        );
        assert_eq!(limits[0].status, Some(UsageStatus::Allowed));
    }

    #[test]
    fn copilot_subscription_quota_omits_unlimited_and_unknown_snapshots() {
        use github_copilot_sdk::rpc::{SessionQuotaProjection, SessionQuotaSnapshot};

        let unlimited = SessionQuotaProjection {
            snapshots: [(
                "premium_interactions".to_string(),
                SessionQuotaSnapshot {
                    is_unlimited_entitlement: true,
                    ..Default::default()
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        assert!(copilot_usage_limits(&unlimited).is_empty());

        let other_bucket = SessionQuotaProjection {
            snapshots: [("chat".to_string(), SessionQuotaSnapshot::default())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        assert!(copilot_usage_limits(&other_bucket).is_empty());
    }

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
            adapter.stream_parser(),
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
            adapter.stream_parser(),
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

    #[cfg(unix)]
    #[tokio::test]
    async fn sdk_runtime_starts_the_cli_server_protocol_not_the_agent_command() {
        use super::GitHubCopilotSdkRuntime;
        use github_copilot_sdk::CliProgram;
        use std::os::unix::fs::PermissionsExt;

        let directory =
            std::env::temp_dir().join(format!("narsor-copilot-sdk-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let args_file = directory.join("args");
        let program = directory.join("copilot-stub");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexit 1\n",
                args_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();

        let runtime = GitHubCopilotSdkRuntime {
            program: CliProgram::Path(program),
        };
        let mut on_event = |_| {};
        let result = runtime
            .run(
                CopilotSdkRequest {
                    prompt: "test".to_string(),
                    model: None,
                    session_id: None,
                    working_dir: directory.clone(),
                    action_execution_id: None,
                    limit: None,
                },
                &mut on_event,
            )
            .await;

        let args = std::fs::read_to_string(&args_file).unwrap();
        let _ = std::fs::remove_dir_all(&directory);
        assert!(
            result.is_err(),
            "the test runtime should exit before connecting"
        );
        assert!(args.lines().any(|arg| arg == "--server"), "{args}");
        assert!(args.lines().any(|arg| arg == "--stdio"), "{args}");
        assert!(!args.lines().any(|arg| arg == "agent"), "{args}");
    }
}
