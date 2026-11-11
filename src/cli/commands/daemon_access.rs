//! Reaching the daemon and reading an execution through it, shared by every
//! command that talks to the control plane.

use std::path::Path;
use std::time::Duration;

use futures::StreamExt as _;

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
/// [`ClientError::Conflict`] (a `409`) is the one case this unit's test
/// scenarios call out by name: another driver — another `resume`/`run`, or
/// the TUI once U7 migrates it — already holds this execution (R11/AE4).
/// Reported as a plain sentence, never the raw `409` or a JSON body, exactly
/// as every other variant below already is.
pub(super) fn client_error(execution_id: &str, error: ClientError) -> CliError {
    match error {
        ClientError::Conflict => CliError::failure(format!(
            "Execution '{execution_id}' is already being driven by another process right now \
             (another 'run'/'resume', or the TUI) — try again once it settles."
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

/// How often [`wait_until_stopped`] asks the daemon directly whether
/// `execution_id` is still `Running`, as a bound on how long it can ever be
/// waiting on an SSE event that will never come — see that function's doc
/// comment for why that bound is needed at all, not just an optimisation.
pub(super) const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Blocks until `execution_id` is no longer `Running`, returning its final
/// summary. This is the shared tail of [`run`] and [`resume`]: both start a
/// pass driving an execution over REST and then need to wait, exactly as
/// they always blocked on `Engine::run`/`Engine::resume` directly before
/// this unit.
///
/// # Why this polls `GET /executions` at all, rather than only watching SSE
///
/// The obvious shape — drain `client.stream_execution(execution_id)` until
/// it ends itself on a terminal event — has a real race this command cannot
/// let an operator hit: `POST /tasks`/`POST /executions/:id/resume` both
/// answer `202` the instant the execution is created/claimed and hand the
/// actual run to a background `tokio::spawn`, which can start — and, for a
/// trivial workflow, finish — before this call's *next* HTTP request (the
/// SSE connection) ever reaches the daemon. `Engine::subscribe()`'s
/// broadcast channel has no replay: a subscriber only ever sees events
/// published after it connects, so a stream that connects after the
/// execution already settled would wait forever for an event that was
/// already sent. Also, the two ways a workflow parks (an operator pause —
/// `WorkflowPaused` — or a question raised — `HumanInputRequested`, from
/// `human::suspend`) are not among the three events
/// [`ApiClient::stream_execution`] treats as terminal, since that contract
/// was written for the workflow-*finishes* case (AE5) alone.
///
/// So the SSE stream here is only ever a low-latency wake-up: every loop
/// iteration checks the authoritative status directly first and returns the
/// moment it is not `Running`, and only *then* waits (bounded by
/// [`STATUS_POLL_INTERVAL`]) for the stream to say something, so as not to
/// busy-poll. Once the stream ends — on its own terminal event, or because
/// the connection dropped — this falls back to a plain, bounded sleep for
/// the rest of the wait. Either way, the wait can never exceed
/// [`STATUS_POLL_INTERVAL`] past the moment the execution actually stopped.
///
/// [`run`]: super::run
/// [`resume`]: super::resume
pub(super) async fn wait_until_stopped(
    client: &ApiClient,
    execution_id: &str,
    before: ExecutionStatusDto,
) -> Result<ExecutionSummary> {
    let mut stream = Some(Box::pin(client.stream_execution(execution_id)));
    loop {
        if let Some(summary) = find_execution(client, execution_id).await? {
            if summary.status != ExecutionStatusDto::Running && summary.status != before {
                return Ok(summary);
            }
        }

        match &mut stream {
            // Wait for the stream to say *something* — any event at all, not
            // only a stopping one — or for the bound to elapse; either way,
            // loop back around to the authoritative status check above.
            // `Ok(None)` means the stream ended on its own (its terminal
            // event, or the connection dropping): stop polling it, a
            // finished stream would otherwise just return `None` forever.
            Some(active) => {
                if let Ok(None) = tokio::time::timeout(STATUS_POLL_INTERVAL, active.next()).await {
                    stream = None;
                }
            }
            None => tokio::time::sleep(STATUS_POLL_INTERVAL).await,
        }
    }
}
