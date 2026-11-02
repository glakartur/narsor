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
    let page = match events_page(store, execution_id, &request) {
        Ok(page) => page,
        Err(error) => {
            tracing::warn!(%error, "cannot read the execution events");
            fail_read(link, timing, &request.request_id, &error.to_string()).await;
            return;
        }
    };
    if let Err(error) = client
        .upload_execution_events(&request.request_id, &page)
        .await
    {
        link.failed(&error, timing);
    }
}

/// The events an events read asks for: those after the requested sequence,
/// at most the requested number.
pub(super) fn events_page(
    store: &ExecutionStore,
    execution_id: &str,
    request: &ExecutionEventsRequest,
) -> Result<ExecutionEventsPage, crate::persistence::PersistenceError> {
    let (events, has_more) = store
        .event_log(execution_id)
        .read_after(request.after, request.limit as usize)?;
    Ok(ExecutionEventsPage { events, has_more })
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
    let view = match conductor_view(store, execution_id, &request, describe) {
        Ok(view) => view,
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

/// The conductor's state a conductor read asks for, with what `describe`
/// says beside it.
pub(super) fn conductor_view(
    store: &ExecutionStore,
    execution_id: &str,
    request: &ConductorReadRequest,
    describe: impl FnOnce(crate::conductor::ConductorView) -> crate::conductor::ConductorView,
) -> Result<crate::conductor::ConductorView, crate::persistence::PersistenceError> {
    let view = crate::conductor::ConductorView::read(
        store,
        execution_id,
        request.after,
        request.limit as usize,
    )?;
    Ok(describe(view))
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::domain::{
        EventData, ResolvedEnvironment, ResourceProfile, TaskId, TaskLaunchSpec, TaskWorkspace,
    };
    use crate::runner::{initialize, HarnessHome, MountedSecrets};

    /// A directory that deletes itself.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("narsor-reader-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const WORKFLOW: &str = r#"
name: one-step
version: 1
states:
  work:
    action:
      type: agent
      prompt: Do the work.
    choices: [done]
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states: [completed]
"#;

    fn task() -> TaskId {
        TaskId::parse("task-1").unwrap()
    }

    /// A storage a working runner initialized, and the lock it holds.
    fn worked_on(root: &TempRoot) -> (TaskStorage, StorageLock) {
        let storage = TaskStorage::new(root.0.join("storage"));
        let lock = storage.lock().unwrap();
        let definition = crate::workflow::load_from_str(WORKFLOW).unwrap();
        let spec = TaskLaunchSpec::new(
            definition.id.clone(),
            definition.version,
            serde_json::json!({}),
            TaskWorkspace::None,
            ResolvedEnvironment::harness_only("claude-code"),
            ResourceProfile::new(1000, 2048),
        )
        .with_definition(&definition)
        .unwrap();
        initialize(
            &storage,
            &storage.store(),
            &task(),
            &spec,
            &MountedSecrets::new(None),
            &crate::workspace::GitAccess::User,
        )
        .unwrap();
        (storage, lock)
    }

    /// A reader's configuration; its credential file does not exist, so it
    /// never reaches a control plane.
    fn reader_config(root: &TempRoot, storage: &TaskStorage) -> RunnerConfig {
        RunnerConfig {
            task_id: task(),
            storage: storage.root().to_path_buf(),
            launch_spec: None,
            control_plane: "http://control-plane.invalid".to_string(),
            credential_file: root.0.join("no-credential"),
            secrets_dir: None,
            harness_home: HarnessHome::default(),
            agent_args: Default::default(),
            incarnation: "reader-1".to_string(),
            timing: RunnerTiming::default(),
            contract: crate::runner::BaseImageContract::default(),
            read_only: true,
            pinned_git_credential_only: false,
        }
    }

    type TreeEntry = (bool, u64, std::time::SystemTime);

    /// Kind, size and modification time of every entry under `root`, `root`
    /// itself and the directories included: a file created and removed again
    /// still moves its directory's time.
    fn tree(root: &Path) -> BTreeMap<String, TreeEntry> {
        fn walk(root: &Path, dir: &Path, tree: &mut BTreeMap<String, TreeEntry>) {
            let meta = fs::symlink_metadata(dir).unwrap();
            let key = dir.strip_prefix(root).unwrap().display().to_string();
            let size = if meta.is_dir() { 0 } else { meta.len() };
            tree.insert(key, (meta.is_dir(), size, meta.modified().unwrap()));
            if meta.is_dir() {
                for entry in fs::read_dir(dir).unwrap() {
                    walk(root, &entry.unwrap().path(), tree);
                }
            }
        }
        let mut tree = BTreeMap::new();
        walk(root, root, &mut tree);
        tree
    }

    /// Runs a reader on the storage as it is; checks that it ends with the
    /// reason at once and leaves the storage alone, and returns the reason.
    async fn unavailable_reader(root: &TempRoot, storage: &TaskStorage) -> String {
        let before = tree(storage.root());
        let exit = run(reader_config(root, storage)).await.unwrap();
        assert_eq!(tree(storage.root()), before, "nothing changed");
        let RunnerExit::Problem(problem) = exit else {
            panic!("a problem: {exit:?}");
        };
        assert_eq!(problem.code, PROBLEM_READER_UNAVAILABLE);
        problem.message
    }

    /// A storage another runner holds is not waited for: the reader says it
    /// cannot read and changes nothing; the working runner keeps its lock.
    #[tokio::test]
    async fn a_read_only_runner_on_a_busy_storage_says_so_and_exits() {
        let root = TempRoot::new();
        let (storage, _working) = worked_on(&root);

        let reason = unavailable_reader(&root, &storage).await;

        assert!(reason.contains("in use by another runner"), "{reason}");
        assert!(
            matches!(storage.lock(), Err(RunnerError::StorageBusy(_))),
            "the working runner still holds the lock"
        );
    }

    /// A storage no runner ever finished initializing is not made into one
    /// by a read, whether its marker or its lock file is what is missing.
    #[tokio::test]
    async fn a_read_only_runner_on_an_uninitialized_storage_says_so_and_creates_nothing() {
        let root = TempRoot::new();
        let (storage, working) = worked_on(&root);
        drop(working);

        fs::remove_file(storage.bookkeeping_dir().join("initialized.json")).unwrap();
        let reason = unavailable_reader(&root, &storage).await;
        assert!(reason.contains("never initialized"), "{reason}");

        fs::remove_dir_all(storage.bookkeeping_dir()).unwrap();
        let reason = unavailable_reader(&root, &storage).await;
        assert!(reason.contains("never initialized"), "{reason}");
        assert!(
            !storage.bookkeeping_dir().exists(),
            "not even the bookkeeping directory was created"
        );
    }

    /// An events read is answered with the events after the asked sequence,
    /// at most the asked number, and says whether more follow.
    #[test]
    fn an_events_read_gets_the_page_it_asked_for() {
        let root = TempRoot::new();
        let store = ExecutionStore::new(&root.0);
        let log = store.event_log("exec-1");
        let execution_id = "exec-1".to_string();
        for state in ["a", "b", "c", "d", "e"] {
            log.append_next(
                &execution_id,
                EventData::StateEntered {
                    state_id: state.to_string(),
                },
            )
            .unwrap();
        }
        let request = |after, limit| ExecutionEventsRequest {
            request_id: "read-1".to_string(),
            after,
            limit,
        };

        let page = events_page(&store, "exec-1", &request(2, 2)).unwrap();
        let sequences: Vec<u64> = page.events.iter().map(|event| event.sequence).collect();
        assert_eq!(sequences, vec![3, 4]);
        assert!(page.has_more);

        let page = events_page(&store, "exec-1", &request(4, 10)).unwrap();
        let sequences: Vec<u64> = page.events.iter().map(|event| event.sequence).collect();
        assert_eq!(sequences, vec![5]);
        assert!(!page.has_more);
    }

    /// A reader's conductor view is the stored chat, without a conductor.
    #[test]
    fn a_readers_conductor_view_says_no_conductor_runs() {
        let root = TempRoot::new();
        let store = ExecutionStore::new(&root.0);
        let request = ConductorReadRequest {
            request_id: "read-1".to_string(),
            after: 0,
            limit: 10,
        };

        let view = conductor_view(&store, "exec-1", &request, |view| {
            view.without_conductor(NO_CONDUCTOR)
        })
        .unwrap();

        assert!(!view.available);
        assert_eq!(view.unavailable_reason.as_deref(), Some(NO_CONDUCTOR));
        assert!(view.chat.is_empty());
    }
}
