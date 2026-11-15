//! Reaching the daemon and reading an execution through it, shared by every
//! command that talks to the control plane.

use std::path::Path;
use std::time::Duration;

use crate::api::{ApiClient, ClientError, ExecutionStatusDto, ExecutionSummary};
use crate::config::Flags;

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

/// `execution_id`'s summary, or `None` if the daemon does not know it —
/// `GET /executions`, filtered client-side (there is no single-execution GET
/// endpoint; every existing caller of that list is small enough that this is
/// not worth adding one for).
pub(super) async fn find_execution(
    client: &ApiClient,
    execution_id: &str,
) -> Result<Option<ExecutionSummary>> {
    let listing = client
        .list_executions()
        .await
        .map_err(|error| client_error(execution_id, error))?;
    Ok(listing
        .executions
        .into_iter()
        .find(|execution| execution.id == execution_id))
}

/// [`find_execution`], turning "not found" into a readable message.
pub(super) async fn load_execution_summary(
    client: &ApiClient,
    execution_id: &str,
) -> Result<ExecutionSummary> {
    find_execution(client, execution_id).await?.ok_or_else(|| {
        CliError::failure(format!(
            "Could not load execution '{execution_id}': not found"
        ))
    })
}

/// How often [`wait_until_stopped`] asks the daemon whether `execution_id`
/// is still `Running`.
pub(super) const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Blocks until `execution_id` is no longer `Running` and has left the
/// status `before`, returning its final summary.
///
/// A plain poll of `GET /executions`: the execution is driven by its runner,
/// and the daemon publishes no stream of that one execution's events.
pub(super) async fn wait_until_stopped(
    client: &ApiClient,
    execution_id: &str,
    before: ExecutionStatusDto,
) -> Result<ExecutionSummary> {
    loop {
        if let Some(summary) = find_execution(client, execution_id).await? {
            if summary.status != ExecutionStatusDto::Running && summary.status != before {
                return Ok(summary);
            }
        }
        tokio::time::sleep(STATUS_POLL_INTERVAL).await;
    }
}
