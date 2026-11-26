//! Reads that run apart from the event loop.
//!
//! Every read of the server is a job: a future that owns an [`ApiClient`]
//! clone, asks, and returns an [`Outcome`] — what to do with the answer, as a
//! closure over plain data. The job never touches [`TaskApp`]; the loop hands
//! the outcome back ([`TaskApp::next_outcome`], [`TaskApp::on_outcome`]) and
//! only then is the state changed. This is the split the details and the
//! history already had ([`super::detail::DetailRead`], `fetch_first` /
//! `apply_fetched`), made the rule for every read.
//!
//! * One job per [`JobKey`] at a time: starting a key that runs does nothing
//!   (R4). A job that failed has left the map by the time its outcome is
//!   applied, so the next start is free.
//! * A job of a scope that is no longer shown is normally aborted
//!   ([`TaskApp::abort_detail_jobs`]); detail reads of an explicitly opened
//!   finished task stay in the background so the operator can return to them.
//!   What an aborted job asked is lost; nothing was changed yet.
//! * The jobs live in a [`JoinSet`], which aborts them when the application is
//!   dropped: leaving the interface never waits for a read (R11).
//! * The frame of the loading marker is a counter of the application, raised
//!   by the loop on its tick while a read runs, so a test picks a frame.

use std::collections::HashMap;
use std::future::Future;

use tokio::task::{AbortHandle, JoinSet};

use super::{problems, TaskApp};
use crate::api::{ApiClient, ClientError, HarnessSelectionSnapshot};
use crate::controller_protocol::{
    CapabilitiesView, ObserverQueueListResponse, RepositoryListResponse, TaskListResponse,
    WorkflowListResponse,
};
use crate::tui::spinner::Spinner;

/// What to do with the answer of a read, once it is back.
type Apply = Box<dyn FnOnce(&mut TaskApp) + Send>;

/// The result of a job: the change its answer makes to the application.
pub struct Outcome(Apply);

impl Outcome {
    /// An outcome that runs `apply` on the application.
    pub(super) fn new(apply: impl FnOnce(&mut TaskApp) + Send + 'static) -> Self {
        Self(Box::new(apply))
    }
}

/// Which read of a task's details a job makes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum DetailJob {
    Timeline,
    /// The run an archived task kept, read for its history entry.
    ArchivedTimeline,
    Conductor,
    Events,
    Attempt(String),
    /// The task once more, with its environment compared with the catalogue
    /// (*Info*).
    Drift,
    /// The execution state of the task (*Stan*).
    State,
}

/// What a view that opens reads for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpeningKind {
    /// `n`: the catalogue and the repositories.
    NewTask,
    /// `H`: the archived tasks and the old executions.
    History,
    /// `R`: the registry.
    Repositories,
    /// `h`: the default harness.
    Harness,
    /// `O`: the queues (read only).
    Queues,
}

impl OpeningKind {
    /// The panel's name while it opens.
    pub fn title(self) -> &'static str {
        match self {
            Self::NewTask => " Nowe zadanie ",
            Self::History => " Historia ",
            Self::Repositories => " Repozytoria ",
            Self::Harness => " Domyślny harness ",
            Self::Queues => " Queues ",
        }
    }
}

/// What the read of a view that opens brings.
pub(super) enum OpeningRead {
    History {
        archived: Result<TaskListResponse, ClientError>,
    },
    Repositories {
        capabilities: Option<Result<CapabilitiesView, ClientError>>,
        listing: Result<RepositoryListResponse, ClientError>,
    },
    NewTask {
        capabilities: Option<Result<CapabilitiesView, ClientError>>,
        workflows: Result<WorkflowListResponse, ClientError>,
        repositories: Result<RepositoryListResponse, ClientError>,
    },
    Harness(Result<HarnessSelectionSnapshot, ClientError>),
    Queues(Result<ObserverQueueListResponse, ClientError>),
}

/// Reads what the view `kind` opens over. The capabilities are read along
/// when `want_capabilities`: the views need them and the first read of the
/// list may not have brought them.
pub(super) async fn read_for_opening(
    client: &ApiClient,
    kind: OpeningKind,
    want_capabilities: bool,
) -> OpeningRead {
    let capabilities = async {
        if want_capabilities {
            Some(client.control_capabilities().await)
        } else {
            None
        }
    };
    match kind {
        OpeningKind::History => OpeningRead::History {
            archived: client.control_tasks_archived(true).await,
        },
        OpeningKind::Repositories => {
            let (capabilities, listing) = tokio::join!(capabilities, client.control_repositories());
            OpeningRead::Repositories {
                capabilities,
                listing,
            }
        }
        OpeningKind::NewTask => {
            let (capabilities, workflows, repositories) = tokio::join!(
                capabilities,
                client.control_workflows(),
                client.control_repositories()
            );
            OpeningRead::NewTask {
                capabilities,
                workflows,
                repositories,
            }
        }
        OpeningKind::Harness => OpeningRead::Harness(client.harness_selection().await),
        OpeningKind::Queues => OpeningRead::Queues(client.observer_queues().await),
    }
}

/// The name of one operation and its scope.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum JobKey {
    /// The list of tasks, the harness and the capabilities.
    Refresh,
    /// One read of the details of a task.
    Detail(String, DetailJob),
    /// The read that opens a view.
    Open(OpeningKind),
    /// The read that brings an open view up to date after a write.
    Reload(OpeningKind),
    Branches(String),
    /// The read of the limit of tasks processed at once (`L`).
    Limit,
    /// The read of one queue of the queues view.
    Queue(String),
    /// The read of the usage limits of the harnesses, in the background: no
    /// screen waits for it, so it does not animate the marker.
    Usage,
}

impl JobKey {
    /// The operation a failed call of this job is recorded under in the
    /// problem log: the screen or tab, and the task where the key has one.
    fn label(&self) -> String {
        match self {
            Self::Refresh => "task list".to_string(),
            Self::Detail(task_id, job) => {
                let place = match job {
                    DetailJob::Timeline => "Przebieg".to_string(),
                    DetailJob::ArchivedTimeline => "Historia, Przebieg".to_string(),
                    DetailJob::Conductor => "Dyrygent".to_string(),
                    DetailJob::Events => "Zdarzenia".to_string(),
                    DetailJob::Attempt(attempt) => format!("Rozmowa, attempt {attempt}"),
                    DetailJob::Drift => "Info".to_string(),
                    DetailJob::State => "Stan".to_string(),
                };
                problems::task_label(&place, task_id)
            }
            Self::Open(kind) => format!("open {}", kind.title().trim()),
            Self::Reload(kind) => format!("reload {}", kind.title().trim()),
            Self::Branches(repository) => format!("Nowe zadanie — branches of {repository}"),
            Self::Limit => "task limit".to_string(),
            Self::Queue(queue) => format!("Queues — queue {queue}"),
            Self::Usage => "harness usage".to_string(),
        }
    }

    fn task(&self) -> Option<&str> {
        match self {
            Self::Detail(task_id, _) => Some(task_id),
            _ => None,
        }
    }
}

/// The jobs that run.
#[derive(Default)]
pub(super) struct Background {
    jobs: JoinSet<(u64, Outcome)>,
    running: HashMap<JobKey, (u64, AbortHandle)>,
    next_id: u64,
}

impl Background {
    pub(super) fn is_running(&self, key: &JobKey) -> bool {
        self.running.contains_key(key)
    }

    /// Whether any job of `task_id`'s details runs.
    pub(super) fn task_running(&self, task_id: &str) -> bool {
        self.running.keys().any(|key| key.task() == Some(task_id))
    }

    /// Starts `read` under `key`. Returns false, and starts nothing, when the
    /// key runs.
    pub(super) fn start(
        &mut self,
        key: JobKey,
        read: impl Future<Output = Outcome> + Send + 'static,
    ) -> bool {
        if self.running.contains_key(&key) {
            return false;
        }
        let id = self.next_id;
        self.next_id += 1;
        let label = key.label();
        let handle = self
            .jobs
            .spawn(async move { (id, problems::labeled(label, read).await) });
        self.running.insert(key, (id, handle));
        true
    }

    /// Aborts the jobs whose key `unwanted` picks out.
    pub(super) fn abort_where(&mut self, unwanted: impl Fn(&JobKey) -> bool) {
        self.running.retain(|key, (_, handle)| {
            let abort = unwanted(key);
            if abort {
                handle.abort();
            }
            !abort
        });
    }

    fn finished(&mut self, id: u64) {
        self.running.retain(|_, (running, _)| *running != id);
    }
}

impl TaskApp {
    /// Whether a job has not been collected yet: the loop waits for
    /// [`Self::next_outcome`] only then.
    pub fn has_running(&self) -> bool {
        !self.background.jobs.is_empty()
    }

    /// Whether a read is under way whose answer the screen waits for: what
    /// animates the marker.
    pub fn is_loading(&self) -> bool {
        self.background
            .running
            .keys()
            .any(|key| *key != JobKey::Usage)
    }

    /// Whether the list of tasks is being read.
    pub fn is_refreshing(&self) -> bool {
        self.background.is_running(&JobKey::Refresh)
    }

    /// Waits for the next job to finish. The jobs that were aborted say
    /// nothing. `None` when no job is left, at once — so the loop asks only
    /// while [`Self::has_running`].
    pub async fn next_outcome(&mut self) -> Option<Outcome> {
        loop {
            match self.background.jobs.join_next().await? {
                Ok((id, outcome)) => {
                    self.background.finished(id);
                    return Some(outcome);
                }
                Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
                Err(_) => {}
            }
        }
    }

    /// Applies what a job brought.
    pub fn on_outcome(&mut self, outcome: Outcome) {
        (outcome.0)(self);
    }

    /// Applies every outcome until no job is left. What a test, and the
    /// blocking entry points that keep their old shape, wait on.
    pub async fn drain_jobs(&mut self) {
        while let Some(outcome) = self.next_outcome().await {
            self.on_outcome(outcome);
        }
    }

    /// The frame of the loading marker.
    pub fn animation_frame(&self) -> usize {
        self.animation_frame
    }

    /// Sets the frame of the loading marker.
    pub fn set_animation_frame(&mut self, frame: usize) {
        self.animation_frame = frame;
    }

    /// The loop's tick: the next frame, while a read runs.
    pub fn advance_animation(&mut self) {
        if self.is_loading() {
            self.animation_frame = self.animation_frame.wrapping_add(1);
        }
    }

    /// Chooses the frames of the marker.
    pub fn set_spinner(&mut self, spinner: Spinner) {
        self.spinner = spinner;
    }

    /// The marker at the current frame with `text` after it.
    pub fn loading_label(&self, text: &str) -> String {
        self.spinner.label(self.animation_frame, text)
    }

    /// The glyph of the marker at the current frame.
    pub fn loading_glyph(&self) -> &'static str {
        self.spinner.glyph(self.animation_frame)
    }

    /// Aborts the reads of the details of `task_id`: its selection left.
    pub(super) fn abort_detail_jobs(&mut self, task_id: &str) {
        self.background
            .abort_where(|key| key.task() == Some(task_id));
    }

    /// Aborts the read that opens a view.
    pub(super) fn abort_open_job(&mut self, kind: OpeningKind) {
        self.background
            .abort_where(|key| *key == JobKey::Open(kind));
    }
}
