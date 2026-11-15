//! Reconciliation after a crash (D23).
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
//! [`ReconciliationOutcome::ConductorInterrupted`]. The
//! same marker beside an open attempt — the conductor's own reprompt was
//! running — is an interrupted action like any other, and the marker is
//! cleared with it.
//!
//! Only `Running` executions are reconciled. A `Paused` or `InputNeeded` one parked itself
//! durably (a human action suspends *without* an `ActionCompleted`, which would
//! otherwise look exactly like an interrupted action), and a finished one has
//! nothing left to reconcile.

use serde_json::json;

use crate::conductor::ConductorStore;
use crate::domain::{ConductorTrigger, EventData, ExecutionId, ExecutionStatus};
use crate::persistence::{ExecutionStore, Result};

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
