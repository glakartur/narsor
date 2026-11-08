//! What the runner reports about the workspaces of its task.
//!
//! The header of each workspace starts from the launch spec. After each step
//! the runner asks for a new reading of the git state. The reading runs in
//! the background, one at a time for each workspace, so a slow repository
//! stops neither the control loop nor the other workspaces. The result stays
//! in memory, so a snapshot never calls git. A read that fails or times out
//! leaves the last result in place.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use narsor_engine::domain::WorkspaceField;
use narsor_engine::workflow::WorkspaceValues;
use serde_json::Value;

use crate::domain::{TaskRepositorySource, TaskWorkspace, WorkspaceHeader, WorkspacePr};
use crate::workspace::state::{self, GitReading};

use super::storage::TaskStorage;

pub(crate) struct WorkspaceTracker {
    entries: Vec<Tracked>,
    /// Told when a reading changed a header: the snapshot is out of date.
    changed: Arc<tokio::sync::Notify>,
}

struct Tracked {
    header: Arc<Mutex<WorkspaceHeader>>,
    git_dir: Option<PathBuf>,
    /// Whether a reading of this workspace is running.
    reading: Arc<AtomicBool>,
    /// Whether the workflow's pull request for a folder without git was
    /// already reported.
    pr_ignored_reported: AtomicBool,
}

impl WorkspaceTracker {
    pub(crate) fn new(
        storage: &TaskStorage,
        workspace: &TaskWorkspace,
        changed: Arc<tokio::sync::Notify>,
    ) -> Self {
        let entries = state::launch_headers(workspace)
            .into_iter()
            .zip(&workspace.repositories)
            .map(|(header, repository)| Tracked {
                git_dir: matches!(repository.source, TaskRepositorySource::Git(_))
                    .then(|| storage.repository_dir(&repository.name)),
                header: Arc::new(Mutex::new(header)),
                reading: Arc::new(AtomicBool::new(false)),
                pr_ignored_reported: AtomicBool::new(false),
            })
            .collect();
        Self { entries, changed }
    }

    /// Starts a reading of each workspace that has none running, and returns
    /// at once. Must run inside the tokio runtime.
    pub(crate) fn refresh(&self) {
        for entry in &self.entries {
            let Some(dir) = entry.git_dir.clone() else {
                continue;
            };
            if entry.reading.swap(true, Ordering::AcqRel) {
                continue;
            }
            let base = lock(&entry.header).base_branch.clone();
            let header = Arc::clone(&entry.header);
            let reading = Arc::clone(&entry.reading);
            let changed = Arc::clone(&self.changed);
            tokio::task::spawn_blocking(move || {
                let result = state::read(&dir, base.as_deref());
                apply(&mut lock(&header), result);
                // Signal first: once `reading` is false, the signal is already
                // stored, so a caller that waits for idle can drain it.
                changed.notify_one();
                reading.store(false, Ordering::Release);
            });
        }
    }

    /// Whether a reading of some workspace is still running.
    #[cfg(test)]
    pub(crate) fn reading_running(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.reading.load(Ordering::Acquire))
    }

    pub(crate) fn headers(&self, prs: &BTreeMap<String, WorkspacePr>) -> Vec<WorkspaceHeader> {
        self.entries
            .iter()
            .map(|entry| {
                let mut header = lock(&entry.header).clone();
                if let Some(pr) = prs.get(&header.name) {
                    if entry.git_dir.is_some() {
                        header.pr = pr.pr.clone();
                        header.pr_url = pr.pr_url.clone();
                    } else if !entry.pr_ignored_reported.swap(true, Ordering::Relaxed) {
                        tracing::warn!(
                            workspace = %header.name,
                            "the workflow set a pull request on a local folder workspace; the header shows none"
                        );
                    }
                }
                header
            })
            .collect()
    }
}

impl std::fmt::Debug for WorkspaceTracker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceTracker")
            .field("workspaces", &self.entries.len())
            .finish()
    }
}

/// The steps read the last reading of git, not a fresh one.
impl WorkspaceValues for WorkspaceTracker {
    fn value(&self, workspace: &str, field: WorkspaceField) -> Option<Value> {
        self.entries.iter().find_map(|entry| {
            let header = lock(&entry.header);
            (header.name == workspace)
                .then(|| field_value(&header, field))
                .flatten()
        })
    }
}

/// The value of `field` in the reading `header` holds. The pull request
/// values are not read here: the engine keeps them in its context.
fn field_value(header: &WorkspaceHeader, field: WorkspaceField) -> Option<Value> {
    match field {
        WorkspaceField::Repo => Some(Value::String(header.repo.clone())),
        WorkspaceField::BaseBranch => header.base_branch.clone().map(Value::String),
        WorkspaceField::Branch => header.branch.clone().map(Value::String),
        WorkspaceField::Added => header.added.map(Value::from),
        WorkspaceField::Removed => header.removed.map(Value::from),
        WorkspaceField::Pr | WorkspaceField::PrUrl => None,
    }
}

fn lock(header: &Mutex<WorkspaceHeader>) -> std::sync::MutexGuard<'_, WorkspaceHeader> {
    header.lock().unwrap_or_else(PoisonError::into_inner)
}

fn apply(header: &mut WorkspaceHeader, reading: GitReading) {
    if let Some(branch) = reading.branch {
        header.branch = Some(branch);
    }
    if header.base_branch.is_none() {
        header.base_branch = reading.base_branch;
    }
    if let Some(changes) = reading.changes {
        header.added = Some(changes.added);
        header.removed = Some(changes.removed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::state::ChangedLines;

    fn header() -> WorkspaceHeader {
        WorkspaceHeader {
            name: "code".into(),
            repo: "orders-api".into(),
            branch: Some("main".into()),
            base_branch: Some("main".into()),
            ..WorkspaceHeader::default()
        }
    }

    fn tracker_of(header: WorkspaceHeader) -> WorkspaceTracker {
        WorkspaceTracker {
            entries: vec![Tracked {
                header: Arc::new(Mutex::new(header)),
                git_dir: None,
                reading: Arc::new(AtomicBool::new(false)),
                pr_ignored_reported: AtomicBool::new(false),
            }],
            changed: Arc::new(tokio::sync::Notify::new()),
        }
    }

    #[test]
    fn a_header_gives_the_values_of_its_reading() {
        let tracker = tracker_of(WorkspaceHeader {
            branch: Some("feat/x".into()),
            added: Some(4),
            removed: Some(1),
            ..header()
        });

        let read = |field| tracker.value("code", field);

        assert_eq!(read(WorkspaceField::Repo), Some(Value::from("orders-api")));
        assert_eq!(read(WorkspaceField::Branch), Some(Value::from("feat/x")));
        assert_eq!(read(WorkspaceField::BaseBranch), Some(Value::from("main")));
        assert_eq!(read(WorkspaceField::Added), Some(Value::from(4)));
        assert_eq!(read(WorkspaceField::Removed), Some(Value::from(1)));
    }

    #[test]
    fn a_value_not_read_yet_is_no_value_not_zero() {
        let tracker = tracker_of(header());

        assert_eq!(tracker.value("code", WorkspaceField::Added), None);
        assert_eq!(tracker.value("code", WorkspaceField::Removed), None);
    }

    #[test]
    fn the_pull_request_values_are_left_to_the_engine() {
        let tracker = tracker_of(WorkspaceHeader {
            pr: Some("7".into()),
            pr_url: Some("https://git.example/pr/7".into()),
            ..header()
        });

        assert_eq!(tracker.value("code", WorkspaceField::Pr), None);
        assert_eq!(tracker.value("code", WorkspaceField::PrUrl), None);
    }

    #[test]
    fn another_workspace_name_has_no_value() {
        let tracker = tracker_of(header());

        assert_eq!(tracker.value("docs", WorkspaceField::Repo), None);
    }

    #[test]
    fn a_folder_workspace_has_a_repo_and_no_branch() {
        let tracker = tracker_of(WorkspaceHeader {
            name: "notes".into(),
            repo: "/srv/notes".into(),
            ..WorkspaceHeader::default()
        });

        assert_eq!(
            tracker.value("notes", WorkspaceField::Repo),
            Some(Value::from("/srv/notes"))
        );
        assert_eq!(tracker.value("notes", WorkspaceField::Branch), None);
    }

    #[test]
    fn a_reading_updates_the_branch_and_the_changed_lines() {
        let mut header = header();

        apply(
            &mut header,
            GitReading {
                branch: Some("feat/x".into()),
                base_branch: Some("main".into()),
                changes: Some(ChangedLines {
                    added: 4,
                    removed: 1,
                }),
            },
        );

        assert_eq!(header.branch.as_deref(), Some("feat/x"));
        assert_eq!(header.base_branch.as_deref(), Some("main"));
        assert_eq!((header.added, header.removed), (Some(4), Some(1)));
    }

    #[test]
    fn a_reading_that_found_nothing_keeps_the_last_result() {
        let mut header = header();
        header.branch = Some("feat/x".into());
        header.added = Some(4);
        header.removed = Some(1);

        apply(&mut header, GitReading::default());

        assert_eq!(header.branch.as_deref(), Some("feat/x"));
        assert_eq!((header.added, header.removed), (Some(4), Some(1)));
    }

    #[test]
    fn a_base_branch_read_from_the_remote_fills_only_an_unknown_one() {
        let mut unknown = WorkspaceHeader {
            base_branch: None,
            ..header()
        };
        let from_remote = GitReading {
            base_branch: Some("trunk".into()),
            ..GitReading::default()
        };

        apply(&mut unknown, from_remote.clone());
        let mut known = header();
        apply(&mut known, from_remote);

        assert_eq!(unknown.base_branch.as_deref(), Some("trunk"));
        assert_eq!(known.base_branch.as_deref(), Some("main"));
    }
}
