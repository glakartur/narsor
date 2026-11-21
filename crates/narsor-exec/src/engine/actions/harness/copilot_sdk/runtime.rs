//! The GitHub Copilot SDK runtime (cargo feature `copilot-sdk`).
//!
//! The SDK starts the Copilot CLI as `copilot --server --stdio` and talks
//! JSON-RPC to it. The CLI is not embedded: the operator installs it, like
//! `claude` and `codex`, and Narsor finds it ([`resolve_copilot_cli`]) and
//! hands the SDK its path.

use std::ffi::OsStr;
use std::fmt::Display;
use std::future::Future;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use github_copilot_sdk::{
    CliProgram, Client, ClientOptions, MessageOptions, ResumeSessionConfig, SessionConfig,
    SessionId,
};
use narsor_protocol::controller::{UsageLimit, UsageStatus};
use serde_json::Value;

use super::{CopilotSdkBackend, CopilotSdkOutcome, CopilotSdkRequest, CopilotSdkResponse};
use crate::domain::ActionError;
use crate::engine::actions::ACTION_ID_ENV_VAR;

/// The program name of the GitHub Copilot CLI, looked up on `PATH`.
const COPILOT_CLI_PROGRAM: &str = "copilot";

/// Names the Copilot CLI explicitly; it wins over `PATH`. The SDK reads the
/// same variable, so one setting serves both.
const COPILOT_CLI_PATH_ENV: &str = "COPILOT_CLI_PATH";

/// Finds the Copilot CLI: `COPILOT_CLI_PATH` when it is set, else
/// `copilot` on `path`.
///
/// A missing CLI is a `harness_not_found` action error that says what to
/// install or set, the same as a missing `claude` or `codex` binary.
fn resolve_copilot_cli(
    cli_path: Option<&OsStr>,
    path: Option<&OsStr>,
) -> Result<PathBuf, ActionError> {
    if let Some(cli_path) = cli_path.filter(|value| !value.is_empty()) {
        let program = PathBuf::from(cli_path);
        if is_executable(&program) {
            return Ok(program);
        }
        return Err(copilot_cli_not_found(format!(
            "{COPILOT_CLI_PATH_ENV} names '{}', which is not an executable file; \
             install the GitHub Copilot CLI and point {COPILOT_CLI_PATH_ENV} at it",
            program.display()
        )));
    }

    path.and_then(|path| {
        std::env::split_paths(path)
            .map(|dir| dir.join(COPILOT_CLI_PROGRAM))
            .find(|candidate| is_executable(candidate))
    })
    .ok_or_else(|| {
        copilot_cli_not_found(format!(
            "the GitHub Copilot CLI '{COPILOT_CLI_PROGRAM}' was not found; it is a runtime \
             prerequisite and must be installed and on PATH (or named by \
             {COPILOT_CLI_PATH_ENV})"
        ))
    })
}

fn copilot_cli_not_found(message: String) -> ActionError {
    ActionError {
        message,
        error_type: Some("harness_not_found".to_string()),
        details: Some(serde_json::json!({
            "program": COPILOT_CLI_PROGRAM,
            "env": COPILOT_CLI_PATH_ENV,
        })),
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
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

/// Runs a Copilot step through the SDK.
#[derive(Default)]
pub(crate) struct GitHubCopilotSdkRuntime {
    /// The Copilot CLI to start; `None` resolves it on every run, from this
    /// process's environment ([`resolve_copilot_cli`]).
    program: Option<PathBuf>,
}

impl GitHubCopilotSdkRuntime {
    fn program(&self) -> Result<PathBuf, ActionError> {
        match &self.program {
            Some(program) => Ok(program.clone()),
            None => resolve_copilot_cli(
                std::env::var_os(COPILOT_CLI_PATH_ENV).as_deref(),
                std::env::var_os("PATH").as_deref(),
            ),
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
            let program = self.program()?;
            let mut options = ClientOptions::default();
            options.program = CliProgram::Path(program);
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
mod tests {
    use super::{
        copilot_usage_limits, resolve_copilot_cli, run_with_sdk_deadline, GitHubCopilotSdkRuntime,
    };
    use crate::engine::actions::harness::copilot_sdk::{CopilotSdkBackend, CopilotSdkRequest};
    use chrono::Utc;
    use narsor_protocol::controller::UsageStatus;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
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

    /// A scratch directory with an executable `copilot` in `with_cli` and a
    /// non-executable one in `without_cli`.
    #[cfg(unix)]
    fn cli_dirs() -> (PathBuf, PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let root =
            std::env::temp_dir().join(format!("narsor-copilot-cli-{}", uuid::Uuid::new_v4()));
        let without_cli = root.join("without");
        let with_cli = root.join("with");
        std::fs::create_dir_all(&without_cli).unwrap();
        std::fs::create_dir_all(&with_cli).unwrap();
        std::fs::write(without_cli.join("copilot"), "not a program").unwrap();
        let cli = with_cli.join("copilot");
        std::fs::write(&cli, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        (root, without_cli, with_cli)
    }

    #[cfg(unix)]
    #[test]
    fn copilot_cli_is_the_first_executable_copilot_on_path() {
        let (root, without_cli, with_cli) = cli_dirs();
        let path = std::env::join_paths([&without_cli, &with_cli]).unwrap();

        let found = resolve_copilot_cli(None, Some(&path));

        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(found.unwrap(), with_cli.join("copilot"));
    }

    #[cfg(unix)]
    #[test]
    fn copilot_cli_path_wins_over_path() {
        let (root, _, with_cli) = cli_dirs();
        let explicit = with_cli.join("copilot");
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::copy(&explicit, elsewhere.join("copilot")).unwrap();

        let found = resolve_copilot_cli(Some(explicit.as_os_str()), Some(elsewhere.as_os_str()));

        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(found.unwrap(), explicit);
    }

    #[test]
    fn missing_copilot_cli_says_what_to_install_or_set() {
        let empty = OsString::from(Path::new("/nonexistent-narsor-dir"));

        let error = resolve_copilot_cli(None, Some(&empty)).unwrap_err();

        assert_eq!(error.error_type.as_deref(), Some("harness_not_found"));
        assert!(error.message.contains("'copilot'"), "{}", error.message);
        assert!(error.message.contains("PATH"), "{}", error.message);
        assert!(
            error.message.contains("COPILOT_CLI_PATH"),
            "{}",
            error.message
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sdk_runtime_starts_the_cli_server_protocol_not_the_agent_command() {
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
            program: Some(program),
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
