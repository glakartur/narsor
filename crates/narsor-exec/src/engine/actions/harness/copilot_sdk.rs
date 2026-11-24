//! `harness: github-copilot` steps, run through the GitHub Copilot SDK.
//!
//! The SDK itself is behind the `copilot-sdk` cargo feature ([`runtime`]):
//! only `narsor-runner` runs agent steps, so only it enables the feature.
//! Built without it, [`GitHubCopilotSdkRuntime`] fails every run with an
//! error that says so, instead of starting anything else.

use std::time::Duration;

use futures::future::BoxFuture;
use narsor_protocol::controller::UsageLimit;
use serde_json::Value;

use crate::domain::ActionError;
use crate::engine::actions::harness::request::{AgentRunEnding, FinishedRun, RunObservers};
use crate::engine::actions::harness::HarnessStreamParser;
use crate::engine::actions::session::conversation::{
    record_stdout_line, ConversationSink, UsageRelay,
};

#[cfg(feature = "copilot-sdk")]
mod runtime;
#[cfg(feature = "copilot-sdk")]
pub(crate) use runtime::GitHubCopilotSdkRuntime;

/// Runs `request` on `backend`, feeding every SDK event through `parser`
/// to the observers the way a CLI harness's stdout lines are fed.
pub(crate) async fn run_copilot_sdk(
    backend: &dyn CopilotSdkBackend,
    request: CopilotSdkRequest,
    parser: Box<dyn HarnessStreamParser>,
    observers: &RunObservers<'_>,
) -> Result<AgentRunEnding, ActionError> {
    let conversation = observers.conversation;
    let usage = observers.usage;
    let mut parser = parser;
    let mut on_event = |record| {
        feed_copilot_sdk_record(&mut parser, conversation, usage, record);
    };
    let outcome = backend.run(request, &mut on_event).await?;
    drop(on_event);

    match outcome {
        CopilotSdkOutcome::TimedOut => Ok(AgentRunEnding::TimedOut),
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
            Ok(AgentRunEnding::Finished(FinishedRun {
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

/// One run as the SDK takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CopilotSdkRequest {
    /// The whole prompt, context files block included; sent as one SDK
    /// message, so it has no size limit of the command line.
    pub(crate) prompt: String,
    /// The model name, not the tier.
    pub(crate) model: Option<String>,
    /// The session to resume.
    pub(crate) session_id: Option<String>,
    pub(crate) working_dir: std::path::PathBuf,
    pub(crate) action_execution_id: Option<String>,
    pub(crate) limit: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CopilotSdkResponse {
    pub(crate) session_id: String,
    pub(crate) text: Option<String>,
    pub(crate) usage_limits: Vec<UsageLimit>,
}

// Without the `copilot-sdk` feature only the tests' fake backend has an outcome.
#[cfg_attr(not(feature = "copilot-sdk"), allow(dead_code))]
pub(crate) enum CopilotSdkOutcome {
    Finished(CopilotSdkResponse),
    TimedOut,
}

/// Runs one request through the GitHub Copilot SDK; the tests put a fake in
/// its place.
pub(crate) trait CopilotSdkBackend: Send + Sync {
    /// Fails when this backend cannot run at all, so the harness reports
    /// the run as not started instead of started and failed.
    fn check_available(&self) -> Result<(), ActionError> {
        Ok(())
    }

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
    fn check_available(&self) -> Result<(), ActionError> {
        Err(ActionError {
            message: "the GitHub Copilot harness runs only in narsor-runner: this binary \
                      was built without the `copilot-sdk` feature of narsor-exec"
                .to_string(),
            error_type: Some("harness_unavailable".to_string()),
            details: Some(serde_json::json!({
                "harness": super::github_copilot::GITHUB_COPILOT,
            })),
        })
    }

    fn run<'a>(
        &'a self,
        _request: CopilotSdkRequest,
        _on_event: &'a mut (dyn FnMut(serde_json::Value) + Send),
    ) -> BoxFuture<'a, Result<CopilotSdkOutcome, ActionError>> {
        let error = self
            .check_available()
            .expect_err("a build without the SDK is never available");
        Box::pin(async move { Err(error) })
    }
}
