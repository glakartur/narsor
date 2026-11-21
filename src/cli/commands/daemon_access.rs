//! Reaching the daemon and reading an execution through it, shared by every
//! command that talks to the control plane.

use std::path::Path;
use std::time::Duration;

use crate::api::{ApiClient, ClientError, ExecutionStatusDto, ExecutionSummary};
use crate::config::Flags;
use narsor_protocol::api::ExecutionLookupResponse;

use super::{CliError, Result};

/// Discovers a running daemon for `state`'s state root, or transparently
/// launches one (AE2) — the one entry point every command above goes
/// through instead of constructing `Engine`/`ExecutionStore` itself (R4).
pub(super) async fn daemon_client(state: &Path, flags: &Flags) -> Result<ApiClient> {
    crate::api::ensure_daemon_running(state, flags)
        .await
        .map_err(|error| CliError::failure(format!("Could not reach the narsor daemon: {error}")))
}

/// Turns a [`ClientError`] into the message an operator sees, naming the
/// execution the call was about.
///
/// [`ClientError::Conflict`] (a `409`) means another client acted on this
/// execution first. Reported as a plain sentence, never the raw `409` or a
/// JSON body, exactly as every other variant below already is.
pub(super) fn client_error(execution_id: &str, error: ClientError) -> CliError {
    match error {
        ClientError::Conflict => CliError::failure(format!(
            "Execution '{execution_id}' was changed by another client right now \
             (another 'input', or the TUI) — try again once it settles."
        )),
        ClientError::NotFound => CliError::failure(format!(
            "Could not load execution '{execution_id}': not found"
        )),
        ClientError::Unauthorized => {
            CliError::failure("The narsor daemon rejected the request: bad token".to_string())
        }
        error @ (ClientError::Forbidden | ClientError::Credential(_)) => {
            CliError::failure(error.to_string())
        }
        ClientError::Unavailable(detail) => {
            CliError::failure(format!("Could not reach the narsor daemon: {detail}"))
        }
        ClientError::Request { message, .. }
        | ClientError::Refused { message, .. }
        | ClientError::Degraded { message, .. } => CliError::failure(message),
        ClientError::Server(detail) => {
            CliError::failure(format!("The narsor daemon reported an error: {detail}"))
        }
    }
}

/// `execution_id` and the task that runs it, or `None` if no task of the
/// caller reported it — one control-plane read of the task records
/// (`GET /control/v1/executions/:id`).
pub(super) async fn find_execution(
    client: &ApiClient,
    execution_id: &str,
) -> Result<Option<ExecutionLookupResponse>> {
    match client.control_execution(execution_id).await {
        Ok(found) => Ok(Some(found)),
        Err(ClientError::NotFound) => Ok(None),
        Err(error) => Err(client_error(execution_id, error)),
    }
}

/// [`find_execution`], turning "not found" into a readable message.
pub(super) async fn load_execution(
    client: &ApiClient,
    execution_id: &str,
) -> Result<ExecutionLookupResponse> {
    find_execution(client, execution_id).await?.ok_or_else(|| {
        CliError::failure(format!(
            "Could not load execution '{execution_id}': not found"
        ))
    })
}

/// The summary of [`load_execution`].
pub(super) async fn load_execution_summary(
    client: &ApiClient,
    execution_id: &str,
) -> Result<ExecutionSummary> {
    Ok(load_execution(client, execution_id).await?.execution)
}

/// How often [`wait_until_stopped`] asks the daemon whether `execution_id`
/// is still `Running`.
pub(super) const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Blocks until `execution_id` is no longer `Running` and has left the
/// status `before`, returning its final summary.
///
/// A plain poll of the execution's task record: the execution is driven by
/// its runner, and the daemon publishes no stream of that one execution's
/// events.
pub(super) async fn wait_until_stopped(
    client: &ApiClient,
    execution_id: &str,
    before: ExecutionStatusDto,
) -> Result<ExecutionSummary> {
    loop {
        if let Some(found) = find_execution(client, execution_id).await? {
            let summary = found.execution;
            if summary.status != ExecutionStatusDto::Running && summary.status != before {
                return Ok(summary);
            }
        }
        tokio::time::sleep(STATUS_POLL_INTERVAL).await;
    }
}
