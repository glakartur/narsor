//! What stops the engine from running a step at all.

use super::*;

/// Everything that stops the engine from running a step at all.
///
/// A *failing action* is not in here: an action that fails is an ordinary
/// outcome of a workflow, recorded as `ActionFailed` and leaving the execution
/// in [`ExecutionStatus::Failed`]. These variants are the cases where the engine
/// itself cannot proceed.
#[derive(Debug, Error)]
pub enum EngineError {
    /// The store could not be read or written.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),

    /// Delivering or recording human input failed.
    #[error(transparent)]
    HumanInput(#[from] HumanActionError),

    /// The state's decision provider cannot be used.
    #[error(transparent)]
    Decision(#[from] DecisionError),

    /// The definition does not pass static validation (§5).
    #[error("workflow definition is invalid:\n  {}", violations.join("\n  "))]
    InvalidDefinition {
        /// Every violation, so the author sees them all at once.
        violations: Vec<String>,
    },

    /// A running execution has no current state to step from.
    #[error("execution '{execution_id}' has no current state")]
    NoCurrentState {
        /// The execution that was stepped.
        execution_id: ExecutionId,
    },

    /// The execution points at a state the definition does not contain.
    #[error(
        "execution '{execution_id}' is at state '{state_id}', which workflow \
         '{workflow_id}' does not define"
    )]
    UnknownState {
        /// The execution that was stepped.
        execution_id: ExecutionId,
        /// The state it points at.
        state_id: StateId,
        /// The workflow it was started from.
        workflow_id: String,
    },

    /// The execution is in a status the requested operation does not allow.
    #[error("execution '{execution_id}' is {status} and cannot be {operation}")]
    NotRunnable {
        /// The execution that was addressed.
        execution_id: ExecutionId,
        /// Its status, as the schema spells it.
        status: String,
        /// What was attempted, e.g. `run`.
        operation: &'static str,
    },

    /// The execution is paused on a question only a person can answer.
    #[error(
        "execution '{execution_id}' is waiting for human input; deliver the input \
         instead of resuming it"
    )]
    AwaitingHumanInput {
        /// The execution that was addressed.
        execution_id: ExecutionId,
    },

    /// The engine's own bookkeeping inside `metadata` could not be read.
    #[error("engine metadata of execution '{execution_id}' is unreadable: {message}")]
    CorruptEngineMetadata {
        /// The execution whose metadata is broken.
        execution_id: ExecutionId,
        /// What went wrong.
        message: String,
    },

    /// Another pass is already driving this execution.
    #[error("execution '{execution_id}' is already being driven by another pass")]
    AlreadyRunning {
        /// The execution a second driver tried to advance.
        execution_id: ExecutionId,
    },

    /// A request named a step failure the execution is no longer stopped
    /// on (KTD9): somebody acted on it first.
    #[error(
        "execution '{execution_id}' is no longer failed on '{expected}'{}",
        failed_now(.current)
    )]
    StaleEpisode {
        /// The execution that was addressed.
        execution_id: ExecutionId,
        /// The failure the request named.
        expected: String,
        /// The failure it is stopped on now, if any.
        current: Option<String>,
    },

    /// A request was made at a moment the execution has moved on from
    /// (KTD9): a proposal of the conductor's about an input already
    /// answered, a state already left.
    #[error(
        "execution '{execution_id}' has moved on since this was asked for (it was at \
         '{expected}'{})",
        now_at(.current)
    )]
    EpisodeChanged {
        /// The execution that was addressed.
        execution_id: ExecutionId,
        /// Where the request expected it to be.
        expected: String,
        /// Where it is now, if anywhere.
        current: Option<String>,
    },

    /// A manual transition the definition does not allow.
    #[error("{message}")]
    InvalidTransition {
        /// Why.
        message: String,
    },
}

/// The tail of [`EngineError::EpisodeChanged`]'s message.
fn now_at(current: &Option<String>) -> String {
    match current {
        Some(current) => format!(", it is now at '{current}'"),
        None => String::new(),
    }
}

/// The tail of [`EngineError::StaleEpisode`]'s message.
fn failed_now(current: &Option<String>) -> String {
    match current {
        Some(current) => format!(" (it is failed on '{current}')"),
        None => String::new(),
    }
}
