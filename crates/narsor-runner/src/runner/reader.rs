//! The read-only runner (R8, R10, R11): the short path `narsor-runner
//! --read-only` takes instead of [`super::run`].
//!
//! A task without a working runner still has a storage with its
//! conversations, its events and its conductor's chat. A reader is what
//! answers the control plane's reads of it, so the control plane never opens
//! a storage itself. It is the same binary on the same storage, but it
//! cannot change what the storage says about the work:
//!
//! 1. takes the storage's runner lock ([`TaskStorage::lock_for_reading`]),
//!    so it never runs beside a runner that works there. The lock file must
//!    exist already; nothing is created or opened for writing, on a read-only
//!    mount too;
//! 2. finds the initialization marker and the launch spec on the storage;
//! 3. long-polls with `read_only` set and answers only page, event and
//!    conductor reads, until the directive is anything but `continue` or the
//!    control plane refuses its credential.
//!
//! It skips every step of a working start that writes: no directory
//! preparation, secrets, initialization, Claude profile, session record,
//! default-harness question, Engine, reconciliation, snapshot or report, and
//! no input, command, material or outbox. An action that was in flight when
//! the last runner died stays exactly as it was (R10): it is neither marked
//! `Unknown` nor started again, and the process group the dead runner left is
//! not looked at.
//!
//! A reader that cannot read — the lock is taken, the storage was never
//! initialized, its state does not load — does not wait: it asks the control
//! plane once for the reads waiting for it, fails each with the reason
//! ([`fail_read`]) and exits with a [`RunnerProblem`].

use std::time::Duration;

use crate::api::ClientError;
use crate::controller_protocol::{
    ConductorReadRequest, ConversationPageRequest, ExecutionEventsPage, ExecutionEventsRequest,
    RelayFailure, RunnerDirective, RunnerPollQuery, RunnerPollResponse,
};
use crate::orchestration::RunnerProblem;
use crate::persistence::ExecutionStore;

use super::storage::StorageLock;
use super::{
    Link, RunnerConfig, RunnerError, RunnerExit, RunnerTiming, TaskStorage,
    PROBLEM_READER_UNAVAILABLE,
};

/// Why a reader's conductor view says no conductor works: a reader never
/// runs one.
const NO_CONDUCTOR: &str = "the task is not running; this is its stored chat";

/// What a reader holds once its storage is open.
struct Opened {
    /// Held until the reader exits.
    _lock: StorageLock,
    store: ExecutionStore,
    execution_id: Option<String>,
}

/// Runs a read-only runner to its end.
pub(super) async fn run(config: RunnerConfig) -> Result<RunnerExit, RunnerError> {
    let storage = TaskStorage::new(&config.storage);
    let mut link = Link::new(&config);
    let opened = match open(&storage, &config) {
        Ok(opened) => opened,
        Err(error) => return Ok(refuse_reads(&mut link, &config.timing, error).await),
    };
    Ok(serve(opened, link, &config.timing).await)
}

/// Locks the storage and checks it is one a runner worked on.
fn open(storage: &TaskStorage, config: &RunnerConfig) -> Result<Opened, RunnerError> {
    let lock = storage.lock_for_reading()?;
    let marker = storage
        .marker()?
        .ok_or_else(|| RunnerError::StorageNotInitialized(config.storage.clone()))?;
    if marker.task_id != config.task_id.as_str() {
        return Err(RunnerError::ForeignStorage(format!(
            "it was initialized for task '{}', not '{}'",
            marker.task_id, config.task_id
        )));
    }
    let store = storage.store();
    store
        .load_task_launch_spec(&config.task_id)?
        .ok_or_else(|| RunnerError::LaunchSpec("the storage holds no launch spec".to_string()))?;
    let mut executions = store.list_execution_ids()?;
    let execution_id = match executions.len() {
        0 => None,
        1 => executions.pop(),
        _ => return Err(RunnerError::SeveralExecutions(executions.join(", "))),
    };
    Ok(Opened {
        _lock: lock,
        store,
        execution_id,
    })
}

fn exit_problem(message: String) -> RunnerExit {
    tracing::error!(%message, "the read-only runner cannot read");
    RunnerExit::Problem(RunnerProblem {
        code: PROBLEM_READER_UNAVAILABLE.to_string(),
        message,
    })
}

/// The reads waiting for a reader that cannot read are failed with the
/// reason, once; then it exits. No snapshot goes out: a reader reports
/// nothing about the task.
async fn refuse_reads(link: &mut Link, timing: &RunnerTiming, error: RunnerError) -> RunnerExit {
    let reason = error.to_string();
    if let Some(client) = link.client() {
        let query = poll_query(timing.poll_wait);
        match client.poll_with(&query).await {
            Ok(response) => {
                let ids = waiting_request_ids(response);
                for request_id in ids {
                    fail_read(link, timing, &request_id, &reason).await;
                }
            }
            Err(error) => link.failed(&error, timing),
        }
    }
    exit_problem(reason)
}

/// The ids of the reads an answer hands out, of every kind.
fn waiting_request_ids(response: RunnerPollResponse) -> Vec<String> {
    response
        .conversation_pages
        .into_iter()
        .map(|request| request.request_id)
        .chain(
            response
                .execution_events
                .into_iter()
                .map(|request| request.request_id),
        )
        .chain(
            response
                .conductor_reads
                .into_iter()
                .map(|request| request.request_id),
        )
        .collect()
}

fn poll_query(wait: Duration) -> RunnerPollQuery {
    RunnerPollQuery {
        wait_secs: Some(wait.as_secs()),
        defer_materials: true,
        ..RunnerPollQuery::default()
    }
}

/// The poll loop of a reader whose storage is open.
async fn serve(opened: Opened, mut link: Link, timing: &RunnerTiming) -> RunnerExit {
    loop {
        if !link.ready() {
            tokio::time::sleep_until(link.next_attempt).await;
        }
        if link.renew_due(timing.renew_margin) {
            link.renew(timing).await;
        }
        let Some(client) = link.client() else {
            return exit_problem("the runner credential cannot be read".into());
        };
        match client.poll_with(&poll_query(timing.poll_wait)).await {
            Ok(response) => {
                link.succeeded();
                link.expires_at = Some(response.credential_expires_at);
                if let Some(exit) = answer(&opened, &mut link, timing, response).await {
                    return exit;
                }
            }
            Err(ClientError::Unauthorized) => {
                return exit_problem("the control plane refused the credential".into());
            }
            Err(error) => link.failed(&error, timing),
        }
    }
}

/// Acts on one poll answer: the directive first, the reads after. `Some`
/// when the reader is done.
async fn answer(
    opened: &Opened,
    link: &mut Link,
    timing: &RunnerTiming,
    response: RunnerPollResponse,
) -> Option<RunnerExit> {
    if response.directive != RunnerDirective::Continue {
        // Whatever comes next may work on this storage: let go of it now.
        tracing::info!(directive = ?response.directive, "the read-only runner is told to stop");
        return Some(RunnerExit::Stopped { clean: true });
    }
    let store = &opened.store;
    let Some(execution_id) = opened.execution_id.as_deref() else {
        let ids = waiting_request_ids(response);
        for request_id in ids {
            fail_read(link, timing, &request_id, "the task has no execution yet").await;
        }
        return None;
    };
    for request in response.conversation_pages {
        serve_page(link, timing, store, execution_id, request).await;
    }
    for request in response.execution_events {
        serve_events(link, timing, store, execution_id, request).await;
    }
    for request in response.conductor_reads {
        serve_conductor_read(link, timing, store, execution_id, request, |view| {
            view.without_conductor(NO_CONDUCTOR)
        })
        .await;
    }
    None
}

// What a working runner and a reader both answer. Only reads of the store:
// the same functions serve both, so a reader's entry numbers are the live
// runner's.

/// Uploads one bounded conversation page an owner waits for.
pub(super) async fn serve_page(
    link: &mut Link,
    timing: &RunnerTiming,
    store: &ExecutionStore,
    execution_id: &str,
    request: ConversationPageRequest,
) {
    let Some(client) = link.client() else {
        return;
    };
    let read = store.conversation_store(execution_id).read(
        &request.attempt_id,
        request.from_entry,
        usize::try_from(request.byte_budget).unwrap_or(usize::MAX),
    );
    let page = match read {
        Ok(snapshot) => crate::api::conversation_response_from_snapshot(snapshot),
        Err(error) => {
            tracing::warn!(%error, attempt = %request.attempt_id, "cannot read a conversation page");
            fail_read(link, timing, &request.request_id, &error.to_string()).await;
            return;
        }
    };
    if let Err(error) = client
        .upload_conversation_page(&request.request_id, &page)
        .await
    {
        link.failed(&error, timing);
    }
}

/// Uploads the execution's events an owner waits for: those after the
/// requested sequence, at most the requested number.
pub(super) async fn serve_events(
    link: &mut Link,
    timing: &RunnerTiming,
    store: &ExecutionStore,
    execution_id: &str,
    request: ExecutionEventsRequest,
) {
    let Some(client) = link.client() else {
        return;
    };
    let (events, has_more) = match store
        .event_log(execution_id)
        .read_after(request.after, request.limit as usize)
    {
        Ok(page) => page,
        Err(error) => {
            tracing::warn!(%error, "cannot read the execution events");
            fail_read(link, timing, &request.request_id, &error.to_string()).await;
            return;
        }
    };
    let page = ExecutionEventsPage { events, has_more };
    if let Err(error) = client
        .upload_execution_events(&request.request_id, &page)
        .await
    {
        link.failed(&error, timing);
    }
}

/// Uploads the conductor's state an owner waits for; `describe` says whether
/// a conductor works (the live runner knows, a reader never has one).
pub(super) async fn serve_conductor_read(
    link: &mut Link,
    timing: &RunnerTiming,
    store: &ExecutionStore,
    execution_id: &str,
    request: ConductorReadRequest,
    describe: impl FnOnce(crate::conductor::ConductorView) -> crate::conductor::ConductorView,
) {
    let Some(client) = link.client() else {
        return;
    };
    let view = match crate::conductor::ConductorView::read(
        store,
        execution_id,
        request.after,
        request.limit as usize,
    ) {
        Ok(view) => describe(view),
        Err(error) => {
            tracing::warn!(%error, "cannot read the conductor's state");
            fail_read(link, timing, &request.request_id, &error.to_string()).await;
            return;
        }
    };
    if let Err(error) = client
        .upload_conductor_read(&request.request_id, &view)
        .await
    {
        link.failed(&error, timing);
    }
}

/// Tells the control plane that the read `request_id` cannot be answered, so
/// its owner hears so now and the request stops being offered to every poll.
pub(super) async fn fail_read(
    link: &mut Link,
    timing: &RunnerTiming,
    request_id: &str,
    reason: &str,
) {
    let Some(client) = link.client() else {
        return;
    };
    let failure = RelayFailure {
        reason: reason.to_string(),
    };
    if let Err(error) = client.upload_relay_failure(request_id, &failure).await {
        link.failed(&error, timing);
    }
}
