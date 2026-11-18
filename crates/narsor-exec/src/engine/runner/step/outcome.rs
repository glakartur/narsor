//! How an attempt, an action and a step end, as the loop reads them.

use super::*;

/// Whether the loop keeps stepping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    /// Step again.
    Continue,
    /// The pass ends here: the execution finished, failed or suspended.
    Stop,
}

/// How one attempt at an action ended, as far as the retry loop is concerned.
pub(crate) enum AttemptOutcome {
    /// The action produced a result.
    Completed(ActionResult),
    /// The action is waiting for a person; the pass ends.
    AwaitingHumanInput(HumanInputRequest),
    /// An agent asked a question and will be restarted in this same state once
    /// the person supplies its answer.
    AwaitingAgentInput(HumanInputRequest),
    /// An agent answered without an output label the state accepts: a format
    /// error, answered with a reprompt rather than a retry (KTD7).
    OutputRejected(OutputRejection),
    /// A `decision` declared `by: conductor`: the conductor is asked, with
    /// the message as the step rendered it.
    AwaitingConductor(String),
}

/// How the whole action of a state ended, retries included.
pub(crate) enum ActionUpshot {
    /// A result came back and the step can go on to its decision.
    Completed {
        /// The attempt that produced it.
        action_execution_id: String,
        /// What it produced.
        result: ActionResult,
    },
    /// The execution is suspended or failed, and that is already persisted.
    Stopped,
    /// The step already took its transition without the action's own
    /// result — the conductor's rescue settled it (F1) — and the pass goes
    /// on.
    Transitioned,
}

impl ActionUpshot {
    /// The upshot of a step the conductor's rescue settled with `flow`.
    pub(crate) fn settled(flow: Flow) -> Self {
        match flow {
            Flow::Stop => Self::Stopped,
            Flow::Continue => Self::Transitioned,
        }
    }
}

/// Where a step's attempts under `retry_policy` start from.
pub(crate) struct AttemptsFrom {
    /// The attempt (1-based) that runs first, or whose failure is `failed`.
    pub(crate) attempt: u32,
    /// Its idempotency key (D24).
    pub(crate) action_execution_id: String,
    /// A failure of that attempt which already happened — a reprompt
    /// continued after a restart that was still unusable (R13): the policy
    /// judges it before anything runs.
    pub(crate) failed: Option<AttemptFailure>,
}
