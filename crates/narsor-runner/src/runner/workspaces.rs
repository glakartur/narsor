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
