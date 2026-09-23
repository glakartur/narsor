//! Reconciliation after a crash (D23) and the startup sweep of leftovers.
//!
//! # What reconciliation can and cannot know
//!
//! `state.json` is written at the end of a step; the event log is written
//! during it. After a crash the two can therefore disagree, and the log is the
//! finer-grained record. Confronting them is possible only because the engine's
//! emission order is fixed ([`crate::engine::runner`]):
//!
//! * an `ActionStarted` (or `ActionRetrying`) with no `ActionCompleted` or
//!   `ActionFailed` after it means an action was in flight when the process
//!   died;
//! * anything else means the step boundary was reached and `state.json` is
//!   trustworthy.
//!
//! An in-flight action is **not replayed**. The orchestrator cannot know
//! whether the child process got as far as its side effect — pushing a branch,
//! opening a pull request, deploying — and a duplicate of that is worse than a
//! stall. The execution becomes [`ExecutionStatus::Unknown`], an `Error` event
//! says why, and a person decides what happens next. D24's idempotency key
//! exists so that person can retry deliberately.
//!
//! Only `Running` executions are reconciled. A `Paused` one parked itself
//! durably (a human action suspends *without* an `ActionCompleted`, which would
//! otherwise look exactly like an interrupted action), and a finished one has
//! nothing left to reconcile.
//!
//! # The startup sweep
//!
//! Two kinds of debris survive a crash: `.tmp` files from an interrupted
//! atomic write, and workspaces of executions that no longer exist or have
//! already finished. [`recover`] clears the first and reconciles every
//! execution; [`sweep_orphaned_workspaces`] reports the second and, when asked,
//! removes it — through the provisioner, which refuses to delete a repository
//! holding uncommitted or unpushed work.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::domain::{EventData, ExecutionId, ExecutionStatus};
use crate::persistence::{ExecutionStore, PersistenceError, Result};
use crate::workspace::provisioner::TASKS_DIR;
use crate::workspace::{CleanupOutcome, CleanupPolicy, WorkspaceProvisioner};

/// What reconciling one execution concluded.
#[derive(Debug, Clone, PartialEq)]
pub enum ReconciliationOutcome {
    /// The execution was not `Running`, so nothing was examined.
    NotApplicable {
        /// The status it was left in.
        status: ExecutionStatus,
    },

    /// `state.json` and the log agree; the execution can simply be run on.
    Consistent,

    /// An action was in flight when the process died.
    InterruptedAction {
        /// The state whose action was running.
        state_id: String,
        /// The attempt that was in flight — the idempotency key of D24.
        action_execution_id: String,
    },
}

impl ReconciliationOutcome {
    /// Whether this execution now needs a person to decide.
    pub fn needs_attention(&self) -> bool {
        matches!(self, Self::InterruptedAction { .. })
    }
}

/// The verdict on one execution.
#[derive(Debug, Clone, PartialEq)]
pub struct Reconciliation {
    /// The execution that was examined.
    pub execution_id: ExecutionId,
    /// Its status after reconciliation.
    pub status: ExecutionStatus,
    /// What was found.
    pub outcome: ReconciliationOutcome,
}

/// A workspace directory with no live execution behind it.
#[derive(Debug, Clone, PartialEq)]
pub struct OrphanedWorkspace {
    /// The execution the directory is named after.
    pub execution_id: ExecutionId,
    /// The directory itself.
    pub path: PathBuf,
    /// Why it counts as orphaned.
    pub reason: OrphanReason,
    /// What was done about it.
    pub cleanup: CleanupOutcome,
}

/// Why a workspace has no live execution.
#[derive(Debug, Clone, PartialEq)]
pub enum OrphanReason {
    /// No execution with that id is stored at all.
    NoExecution,
    /// The execution exists but has finished.
    ExecutionFinished {
        /// The status it finished in.
        status: ExecutionStatus,
    },
}

/// Everything a startup recovery did.
#[derive(Debug, Clone, Default)]
pub struct RecoveryReport {
    /// Temporary files of interrupted writes that were removed.
    pub removed_temp_files: Vec<PathBuf>,
    /// One verdict per stored execution.
    pub reconciled: Vec<Reconciliation>,
}

impl RecoveryReport {
    /// The executions that were left needing a human decision.
    pub fn needing_attention(&self) -> impl Iterator<Item = &Reconciliation> {
        self.reconciled
            .iter()
            .filter(|entry| entry.outcome.needs_attention())
    }
}

/// Clears interrupted writes and reconciles every stored execution.
///
/// Call once at startup, before running anything.
pub fn recover(store: &ExecutionStore) -> Result<RecoveryReport> {
    let removed_temp_files = store.cleanup_orphaned_temp_files()?;
    let reconciled = reconcile_all(store)?;
    Ok(RecoveryReport {
        removed_temp_files,
        reconciled,
    })
}

/// Reconciles every stored execution.
pub fn reconcile_all(store: &ExecutionStore) -> Result<Vec<Reconciliation>> {
    let mut results = Vec::new();
    for execution_id in store.list_execution_ids()? {
        if !store.exists(&execution_id) {
            continue;
        }
        results.push(reconcile(store, &execution_id)?);
    }
    Ok(results)
}

/// Confronts one execution's `state.json` with the tail of its event log.
///
/// An execution left mid-action is marked [`ExecutionStatus::Unknown`] and an
/// `Error` event describing the interruption is appended. Nothing is replayed.
pub fn reconcile(store: &ExecutionStore, execution_id: &str) -> Result<Reconciliation> {
    let mut execution = store.load_execution(execution_id)?;

    if execution.status != ExecutionStatus::Running {
        return Ok(Reconciliation {
            execution_id: execution_id.to_string(),
            status: execution.status.clone(),
            outcome: ReconciliationOutcome::NotApplicable {
                status: execution.status,
            },
        });
    }

    let log = store.event_log(execution_id);
    let events = log.read_all()?;

    let Some((state_id, action_execution_id)) = unfinished_action(&events) else {
        return Ok(Reconciliation {
            execution_id: execution_id.to_string(),
            status: ExecutionStatus::Running,
            outcome: ReconciliationOutcome::Consistent,
        });
    };

    let message = format!(
        "action '{action_execution_id}' of state '{state_id}' was still running when the \
         orchestrator stopped; its effect is unknown and it will not be replayed \
         automatically"
    );

    log.append_next(
        &execution.id,
        EventData::Error {
            message: message.clone(),
            context: json!({
                "state": state_id,
                "action_execution_id": action_execution_id,
                "reconciliation": "interrupted_action",
            }),
        },
    )?;

    execution.status = ExecutionStatus::Unknown;
    store.save_execution(&execution)?;

    Ok(Reconciliation {
        execution_id: execution_id.to_string(),
        status: ExecutionStatus::Unknown,
        outcome: ReconciliationOutcome::InterruptedAction {
            state_id,
            action_execution_id,
        },
    })
}

/// Finds an action that was started (or retried) and never finished.
///
/// Walks the log forwards, so a state entered twice — a loop — is judged on its
/// last attempt only.
fn unfinished_action(events: &[crate::domain::Event]) -> Option<(String, String)> {
    let mut open: Option<(String, String)> = None;

    for event in events {
        match &event.data {
            EventData::ActionStarted {
                action_execution_id,
                state_id,
                ..
            } => {
                open = Some((state_id.clone(), action_execution_id.clone()));
            }
            EventData::ActionRetrying {
                action_execution_id,
                ..
            } => {
                if let Some((state_id, _)) = &open {
                    open = Some((state_id.clone(), action_execution_id.clone()));
                }
            }
            EventData::ActionCompleted { .. } | EventData::ActionFailed { .. } => {
                open = None;
            }
            // A suspended action is parked, not interrupted: the execution is
            // `Paused` and never reaches this function, but a workflow that was
            // resumed keeps the request in its history, so it is cleared here
            // too.
            EventData::HumanInputReceived { .. } => {
                open = None;
            }
            _ => {}
        }
    }

    open
}

/// Reports — and, under [`CleanupPolicy::Remove`], deletes — workspaces with no
/// live execution behind them.
///
/// A workspace is orphaned when the store holds no execution of that id, or
/// holds one that has finished. Removal goes through the provisioner, so a
/// repository with uncommitted or unpushed work is kept and reported as
/// [`CleanupOutcome::Blocked`] instead: losing an agent's work costs more than
/// the disk space.
pub fn sweep_orphaned_workspaces(
    store: &ExecutionStore,
    provisioner: &WorkspaceProvisioner,
    policy: CleanupPolicy,
) -> Result<Vec<OrphanedWorkspace>> {
    let tasks_dir = provisioner.root().join(TASKS_DIR);
    let mut orphans = Vec::new();

    for execution_id in task_directories(&tasks_dir)? {
        let reason = match store.exists(&execution_id) {
            false => OrphanReason::NoExecution,
            true => {
                let execution = store.load_execution(&execution_id)?;
                if !execution.is_terminal() {
                    continue;
                }
                OrphanReason::ExecutionFinished {
                    status: execution.status,
                }
            }
        };

        let cleanup = provisioner
            .cleanup(&execution_id, policy)
            .unwrap_or_else(|error| CleanupOutcome::Blocked {
                path: provisioner.task_dir(&execution_id),
                reason: error.to_string(),
            });

        orphans.push(OrphanedWorkspace {
            execution_id: execution_id.clone(),
            path: provisioner.task_dir(&execution_id),
            reason,
            cleanup,
        });
    }

    orphans.sort_by(|left, right| left.execution_id.cmp(&right.execution_id));
    Ok(orphans)
}

/// The execution ids that have a workspace directory, in no particular order.
fn task_directories(tasks_dir: &Path) -> Result<Vec<ExecutionId>> {
    let entries = match fs::read_dir(tasks_dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(PersistenceError::io(tasks_dir, source)),
    };

    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| PersistenceError::io(tasks_dir, source))?;
        if !entry.path().is_dir() {
            continue;
        }
        if let Some(id) = entry.file_name().to_str() {
            ids.push(id.to_string());
        }
    }
    Ok(ids)
}
