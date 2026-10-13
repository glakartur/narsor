//! Reconciliation after a crash (D23) and the startup sweep of leftovers.
//!
//! # What reconciliation can and cannot know
//!
//! `state.json` is written at the end of a step; the event log is written
//! during it. After a crash the two can therefore disagree, and the log is the
//! finer-grained record. Confronting them is possible only because the engine's
//! emission order is fixed ([`crate::engine::runner`]):
//!
//! * an `ActionStarted` (or `ActionRetrying`, or `OutputReprompted`) with no
//!   `ActionCompleted`, `ActionFailed` or `ActionOutputRejected` after it
//!   means an action was in flight when the process died;
//! * anything else means the step boundary was reached and `state.json` is
//!   trustworthy.
//!
//! An in-flight action is **not replayed**. The narsor cannot know
//! whether the child process got as far as its side effect — pushing a branch,
//! opening a pull request, deploying — and a duplicate of that is worse than a
//! stall. The execution becomes [`ExecutionStatus::Unknown`], an `Error` event
//! says why, and a person decides what happens next. D24's idempotency key
//! exists so that person can retry deliberately.
//!
//! # The conductor's marker
//!
//! The conductor (D55) runs no action attempt of its own and writes a
//! marker to its store while an invocation is under way. A `Running`
//! execution with the marker of an automatic invocation (F1) and no open
//! attempt stopped *in the conductor*, not in an action: nothing it did can
//! have had a side effect, so it stays `Running` and is reported as
//! [`ReconciliationOutcome::ConductorInterrupted`] for the startup hook. The
//! same marker beside an open attempt — the conductor's own reprompt was
//! running — is an interrupted action like any other, and the marker is
//! cleared with it.
//!
//! Only `Running` executions are reconciled. A `Paused` or `InputNeeded` one parked itself
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

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::conductor::ConductorStore;
use crate::domain::{ConductorTrigger, Event, EventData, ExecutionId, ExecutionStatus};
use crate::persistence::{ExecutionStore, PersistenceError, Result};
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

    /// The conductor's automatic invocation was under way, with no action
    /// attempt open (D55): the execution stays `Running` for the startup
    /// hook to carry on.
    ConductorInterrupted {
        /// What the interrupted invocation was for.
        trigger: ConductorTrigger,
        /// The invocation, as its `ConductorStarted` named it.
        invocation_id: String,
    },

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
    /// Attempts (`(execution_id, action_execution_id)`) whose conversation
    /// file was compressed because they were found closed (U2, R15).
    pub compressed_conversations: Vec<(ExecutionId, String)>,
}

impl RecoveryReport {
    /// The executions that were left needing a human decision.
    pub fn needing_attention(&self) -> impl Iterator<Item = &Reconciliation> {
        self.reconciled
            .iter()
            .filter(|entry| entry.outcome.needs_attention())
    }
}

/// Clears interrupted writes, reconciles every stored execution, and
/// compresses the conversation files of attempts found closed.
///
/// Call once at startup, before running anything.
pub fn recover(store: &ExecutionStore) -> Result<RecoveryReport> {
    let removed_temp_files = store.cleanup_orphaned_temp_files()?;
    let reconciled = reconcile_all(store)?;
    let compressed_conversations = compress_closed_conversations(store)?;
    Ok(RecoveryReport {
        removed_temp_files,
        reconciled,
        compressed_conversations,
    })
}

/// Compresses the plain conversation file of every attempt found closed
/// (U2, R15).
///
/// "Closed" means either of two independent things, either being enough:
/// the owning execution is no longer `Running` (nothing will ever append to
/// this attempt's file again, whatever the event log says about the attempt
/// itself — a `Paused`/`InputNeeded`/`Completed`/`Cancelled`/`Unknown` execution has no live
/// writer left), or the event log records the attempt itself as finished
/// (`ActionCompleted`, `ActionFailed`, `ActionAttemptFailed` or
/// `ActionOutputRejected` for that `action_execution_id`) while the execution
/// keeps running its next state.
///
/// An attempt that is neither is presumed still open in another process and
/// is left completely untouched — compressing out from under a live writer
/// would corrupt its next append.
pub fn compress_closed_conversations(store: &ExecutionStore) -> Result<Vec<(ExecutionId, String)>> {
    let mut compressed = Vec::new();

    for execution_id in store.list_execution_ids()? {
        if !store.exists(&execution_id) {
            continue;
        }
        let execution = store.load_execution(&execution_id)?;
        let conversations = store.conversation_store(&execution_id);
        let open_attempts = conversations.open_attempt_ids()?;
        if open_attempts.is_empty() {
            continue;
        }

        let closed_in_log = if execution.status == ExecutionStatus::Running {
            let log = store.event_log(&execution_id);
            Some(closed_attempt_ids(&log.read_all()?))
        } else {
            // Nothing will ever write to this execution's attempts again,
            // regardless of what the log says about any individual one.
            None
        };

        for attempt_id in open_attempts {
            let should_close = match &closed_in_log {
                Some(closed) => closed.contains(&attempt_id),
                None => true,
            };
            if !should_close {
                continue;
            }
            conversations.close_attempt(&attempt_id)?;
            compressed.push((execution_id.clone(), attempt_id));
        }
    }

    Ok(compressed)
}

/// The `action_execution_id`s the event log records as finished, one way or
/// another: a completed attempt, a final failure, a failed attempt that will
/// be retried under a *different* `action_execution_id` (D24), or an attempt
/// whose output was rejected and which a reprompt continues under a
/// different id too (KTD7) — either way, nothing more is ever written to
/// *this* attempt's conversation file.
fn closed_attempt_ids(events: &[Event]) -> HashSet<String> {
    let mut closed = HashSet::new();
    for event in events {
        match &event.data {
            EventData::ActionCompleted {
                action_execution_id,
                ..
            }
            | EventData::ActionFailed {
                action_execution_id,
                ..
            }
            | EventData::ActionAttemptFailed {
                action_execution_id,
                ..
            }
            | EventData::ActionOutputRejected {
                action_execution_id,
                ..
            } => {
                closed.insert(action_execution_id.clone());
            }
            _ => {}
        }
    }
    closed
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
            status: execution.status,
            outcome: ReconciliationOutcome::NotApplicable {
                status: execution.status,
            },
        });
    }

    let log = store.event_log(execution_id);
    let events = log.read_all()?;

    let conductor = ConductorStore::new(store.paths(), execution_id);
    let marker = automatic_conductor_marker(&conductor);

    let Some((state_id, action_execution_id)) = unfinished_action(&events) else {
        let outcome = match marker {
            Some((trigger, invocation_id)) => ReconciliationOutcome::ConductorInterrupted {
                trigger,
                invocation_id,
            },
            None => ReconciliationOutcome::Consistent,
        };
        return Ok(Reconciliation {
            execution_id: execution_id.to_string(),
            status: ExecutionStatus::Running,
            outcome,
        });
    };

    // The conductor's own reprompt was running (the restart matrix): the
    // attempt is what decides, and the conductor has nothing left to resume.
    if marker.is_some() {
        let mut state = conductor.load_state()?;
        state.pending = None;
        conductor.save_state(&state)?;
    }

    let message = format!(
        "action '{action_execution_id}' of state '{state_id}' was still running when the \
         narsor stopped; its effect is unknown and it will not be replayed \
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

/// The trigger and id of the automatic conductor invocation (F1) under way
/// for this execution, if its marker says one is.
///
/// A conductor state that cannot be read is logged and treated as no
/// marker: the execution is then reconciled exactly as it was before the
/// conductor existed, which errs on the side of `Unknown`, never of a
/// replay.
fn automatic_conductor_marker(conductor: &ConductorStore) -> Option<(ConductorTrigger, String)> {
    if !conductor.exists() {
        return None;
    }
    let state = match conductor.load_state() {
        Ok(state) => state,
        Err(error) => {
            tracing::warn!(
                execution_id = conductor.execution_id(),
                error = %error,
                "recovery: could not read the conductor's state; ignoring its marker"
            );
            return None;
        }
    };
    state
        .pending
        .filter(|pending| pending.request.trigger == ConductorTrigger::OutputRescue)
        .map(|pending| (pending.request.trigger, pending.invocation_id))
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
            // A reprompt opens an attempt of its own (KTD7): a crash while it
            // runs is an interrupted action like any other, since the resumed
            // session may have done anything.
            EventData::OutputReprompted {
                action_execution_id,
                state_id,
                ..
            } => {
                open = Some((state_id.clone(), action_execution_id.clone()));
            }
            // A rejected output closes its attempt: nothing is in flight until
            // the reprompt announces itself, and a pass continuing after a
            // restart picks it up from the engine's own record rather than
            // replaying the attempt.
            EventData::ActionCompleted { .. }
            | EventData::ActionFailed { .. }
            | EventData::ActionOutputRejected { .. } => {
                open = None;
            }
            // A suspended action is parked, not interrupted: the execution is
            // `InputNeeded` and never reaches this function, but a workflow that was
            // resumed keeps the request in its history, so it is cleared here
            // too.
            EventData::HumanInputReceived { .. } => {
                open = None;
            }
            // Deliberately *not* a closure: an attempt that will be retried is
            // not the end of the action, only of one attempt at it. The action
            // stays open — a crash right after this event and before the next
            // `ActionRetrying` is still an interrupted action (D23), exactly as
            // if this event did not exist.
            EventData::ActionAttemptFailed { .. } => {}
            // Neither opens nor closes an attempt: a problem about a step's
            // output follows the rejection that closed its attempt and
            // leaves the execution `InputNeeded`, which is never reconciled;
            // one about a failure follows `ActionFailed`.
            EventData::ProblemRaised { .. } => {}
            // The conductor runs no action attempt (D55): its invocation is
            // tracked by its own marker, which `reconcile` reads, and never
            // opens or closes one here.
            EventData::ConductorStarted { .. }
            | EventData::ConductorFinished { .. }
            | EventData::ConductorDecision { .. }
            | EventData::ConductorDecisionUndone { .. } => {}
            _ => {}
        }
    }

    open
}

/// Reports — and, under [`CleanupPolicy::Remove`], deletes — workspaces with no
/// live execution behind them.
///
/// A workspace is orphaned when the store holds no execution of that id, or
/// holds one that has finished — except a `Failed` one stopped on a step
/// failure the conductor took up (F3): it is being diagnosed, or waits for
/// the operator with a diagnosis whose proposals — a retry above all — need
/// the workspace. Removal goes through the provisioner, so a
/// repository with uncommitted or unpushed work is kept and reported as
/// [`CleanupOutcome::Blocked`] instead: losing an agent's work costs more than
/// the disk space.
pub fn sweep_orphaned_workspaces(
    store: &ExecutionStore,
    provisioner: &WorkspaceProvisioner,
    policy: CleanupPolicy,
) -> Result<Vec<OrphanedWorkspace>> {
    let tasks_dir = provisioner.paths().tasks_dir();
    let mut orphans = Vec::new();

    for execution_id in task_directories(&tasks_dir)? {
        let reason = match store.exists(&execution_id) {
            false => OrphanReason::NoExecution,
            true => {
                let execution = store.load_execution(&execution_id)?;
                if !execution.is_terminal() || awaits_conductor(&execution) {
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

/// Whether `execution` is failed on a step failure the conductor took up —
/// its record says so, and an unreadable one counts as a yes.
fn awaits_conductor(execution: &crate::domain::WorkflowExecution) -> bool {
    !matches!(crate::engine::step_failure(execution), Ok(None))
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
