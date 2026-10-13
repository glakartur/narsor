//! A step that failed for good, handed to the conductor (F3, U11).
//!
//! # The execution stays `Failed` (KTD4)
//!
//! A step whose action failed after its `retry_policy` ran out fails the
//! execution exactly as it always did. What is new is a record beside it,
//! [`StepFailure`] in `metadata.engine.step_failure`, written by the same
//! save that makes the execution `Failed`: it is the trigger the conductor
//! diagnoses (the "F3 marker" of the daemon restart matrix), and it lives
//! until an operator — or the conductor's one retry — recovers the
//! execution, whose save clears it together with the status change.
//!
//! The conductor works beside the failed execution without its claim
//! (KTD10) and never writes it: it reads the record, and keeps what it
//! concluded in its own store.
//!
//! # What is not a trigger (R25)
//!
//! A failure while the conductor rescues a step's output label (F1) — its
//! invocation, its reprompt, the application of its decision — originates
//! from the conductor and gets no record. So does a workflow that simply
//! ends in a terminal state declared as failed: nothing failed there.
//!
//! # Without a conductor
//!
//! The failure is recorded all the same, and handed to nobody: the record
//! is also the key an operator's restart names ([`Recovery::episode`]) and
//! what a task runner reports as a failed step waiting for its owner, with
//! or without a conductor to diagnose it.
//!
//! # The conductor's one retry (R21, KTD14)
//!
//! Is an ordinary recovery with a note, taken with the failure's key
//! ([`Recovery::episode`]): an operator who acted first leaves the key stale
//! and the retry is refused. It runs exactly one attempt, whatever the
//! state's `retry_policy` says, and keeps the failure's episode — the entry
//! into the state — so that a failure of the retry is known to have used it
//! ([`ConductorRetry`]).
//!
//! "Without side effects" is judged conservatively, per attempt, while the
//! attempt ends ([`AttemptFailure::side_effect_free`]): an agent attempt that
//! captured no harness session id and emitted no `AgentMessage`, a script
//! whose process never started, a template that never rendered. Anything
//! else — a timeout included — counts as having acted.

use super::*;

/// Key of the step failure the conductor diagnoses (F3), inside the engine's
/// scratch area.
pub const METADATA_STEP_FAILURE_KEY: &str = "step_failure";

/// Key of the conductor's one retry under way, inside the engine's scratch
/// area.
pub const METADATA_CONDUCTOR_RETRY_KEY: &str = "conductor_retry";

/// A step that failed for good and is the conductor's to diagnose (F3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepFailure {
    /// The state whose step failed.
    pub state_id: StateId,

    /// The attempt that failed last: the key of this failure (KTD9) — what a
    /// recovery answering it names, and what `ProblemRaised` carries.
    pub action_execution_id: String,

    /// The failure episode (KTD14): the entry into the state, named by the
    /// first failure of it. The conductor's retry keeps it, so that one
    /// retry is all an episode ever gets.
    pub episode: String,

    /// Why the step failed, as `WorkflowFailed` says.
    pub error: String,

    /// Whether no attempt of the step can have had a side effect.
    pub side_effect_free: bool,

    /// Whether this is the failure of the conductor's own retry.
    #[serde(default)]
    pub conductor_retry: bool,
}

impl StepFailure {
    /// The key of this failure (KTD9).
    pub fn key(&self) -> &str {
        &self.action_execution_id
    }
}

/// The conductor's one retry under way: the pass it started runs a single
/// attempt of `state_id`, and a failure of it belongs to `episode`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ConductorRetry {
    pub(super) state_id: StateId,
    pub(super) episode: String,
}

/// How a failed or interrupted execution is recovered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recovery {
    /// A note for the restarted agent.
    pub comment: Option<String>,
    /// The key of the step failure the recovery answers (KTD9): when the
    /// execution is no longer failed on it, the recovery is refused
    /// ([`EngineError::StaleEpisode`]). `None` recovers whatever is there.
    pub episode: Option<String>,
    /// The conductor's one retry (R21, KTD14): exactly one attempt, in the
    /// failure's episode. Requires `episode`.
    pub conductor_retry: bool,
}

impl Recovery {
    /// An operator's recovery of the step failure `episode` names (of
    /// whatever is there when `None`), with an optional note for the
    /// agent's next attempt.
    pub fn by_operator(comment: Option<String>, episode: Option<String>) -> Self {
        Self {
            comment,
            episode,
            conductor_retry: false,
        }
    }

    /// An operator's recovery with an optional note, of whatever is there.
    pub fn with_comment(comment: Option<String>) -> Self {
        Self {
            comment,
            ..Self::default()
        }
    }
}

/// The step failure `execution` is stopped on, if it is `Failed` on one
/// the conductor took up.
pub fn step_failure(execution: &WorkflowExecution) -> Result<Option<StepFailure>, EngineError> {
    if execution.status != ExecutionStatus::Failed {
        return Ok(None);
    }
    read_engine_record(execution, METADATA_STEP_FAILURE_KEY)
}

/// The conductor's retry recorded for `state_id`, if there is one.
pub(super) fn conductor_retry_of(
    execution: &WorkflowExecution,
    state_id: &str,
) -> Result<Option<ConductorRetry>, EngineError> {
    Ok(
        read_engine_record::<ConductorRetry>(execution, METADATA_CONDUCTOR_RETRY_KEY)?
            .filter(|retry| retry.state_id == state_id),
    )
}

impl Engine {
    /// Records the step failure the execution is about to be saved with,
    /// and returns the conductor to hand it to once it is saved — `None`
    /// when no conductor is attached — or clears what an earlier failure
    /// left and returns `None`, for a failure that is not a trigger (see
    /// the module documentation).
    ///
    /// Must run before the rescue record is cleared: it is what says the
    /// failure came from the conductor.
    pub(super) fn record_step_failure(
        &self,
        execution: &mut WorkflowExecution,
        failed_attempt: Option<&str>,
        error: &str,
        side_effect_free: bool,
    ) -> Result<Option<Arc<ConductorRegistry>>, EngineError> {
        let retry = read_engine_record::<ConductorRetry>(execution, METADATA_CONDUCTOR_RETRY_KEY)?;
        clear_engine_metadata(execution, METADATA_CONDUCTOR_RETRY_KEY);
        clear_engine_metadata(execution, METADATA_STEP_FAILURE_KEY);

        let conductor = self.conductor();
        let Some(state_id) = execution.current_state.clone() else {
            return Ok(None);
        };
        if stored_output_rescue(execution, &state_id)?.is_some() {
            // Failed while the conductor rescued the output (R25).
            return Ok(None);
        }

        let action_execution_id = failed_attempt
            .map(ToString::to_string)
            .or_else(|| execution.current_action_execution.clone())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let retry = retry.filter(|retry| retry.state_id == state_id);
        let failure = StepFailure {
            episode: retry
                .as_ref()
                .map(|retry| retry.episode.clone())
                .unwrap_or_else(|| action_execution_id.clone()),
            conductor_retry: retry.is_some(),
            state_id,
            action_execution_id,
            error: error.to_string(),
            side_effect_free,
        };
        write_engine_record(execution, METADATA_STEP_FAILURE_KEY, &failure)?;
        Ok(conductor)
    }

    /// The part of a recovery [`Engine::prepare_recover`] adds for a step
    /// failure: refuses a recovery whose key is stale, clears the failure
    /// record — in the same save as the status change — and records the
    /// conductor's retry.
    pub(super) fn recover_step_failure(
        &self,
        execution: &mut WorkflowExecution,
        recovery: &Recovery,
    ) -> Result<(), EngineError> {
        let failure = step_failure(execution)?;
        if let Some(expected) = &recovery.episode {
            let current = failure.as_ref().map(|failure| failure.key().to_string());
            if current.as_deref() != Some(expected.as_str()) {
                return Err(EngineError::StaleEpisode {
                    execution_id: execution.id.clone(),
                    expected: expected.clone(),
                    current,
                });
            }
        }

        clear_engine_metadata(execution, METADATA_STEP_FAILURE_KEY);
        clear_engine_metadata(execution, METADATA_CONDUCTOR_RETRY_KEY);
        if recovery.conductor_retry {
            let Some(failure) = failure.filter(|_| recovery.episode.is_some()) else {
                return Err(EngineError::StaleEpisode {
                    execution_id: execution.id.clone(),
                    expected: String::new(),
                    current: None,
                });
            };
            write_engine_record(
                execution,
                METADATA_CONDUCTOR_RETRY_KEY,
                &ConductorRetry {
                    state_id: failure.state_id,
                    episode: failure.episode,
                },
            )?;
        }
        Ok(())
    }

    /// Whether the agent or script attempt `action_execution_id`, which
    /// failed with `error`, cannot have had a side effect (KTD14).
    pub(super) fn attempt_left_no_trace(
        &self,
        kind: ActionKind,
        execution_id: &str,
        action_execution_id: &str,
        error: &ActionError,
    ) -> bool {
        match kind {
            ActionKind::Script => error.error_type.as_deref() == Some(SCRIPT_SPAWN_FAILED),
            ActionKind::Agent => match self.store.event_log(execution_id).read_all() {
                Ok(events) => {
                    !crate::conductor::agent_attempt_left_traces(&events, action_execution_id)
                }
                // Unknown is not clean.
                Err(_) => false,
            },
            ActionKind::Human | ActionKind::Wait => false,
        }
    }
}

/// The `error_type` of a script whose process could not be started.
const SCRIPT_SPAWN_FAILED: &str = "spawn_failed";
