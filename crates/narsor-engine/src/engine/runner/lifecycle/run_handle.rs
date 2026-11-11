//! The two-phase split of a resume or a delivery (U3).

use super::*;

/// What a prepared (but not yet driven to completion) resume/delivery ended
/// up at, synchronously.
pub(crate) enum HandleState {
    /// The execution still has to be stepped (`run_loop`).
    Ready(WorkflowExecution),
    /// The synchronous phase already reached a stop (e.g. a `human` action's
    /// answer completed the step but the workflow suspended again right
    /// away) — nothing left to run.
    Done(WorkflowExecution),
}

/// The claim on one execution, held between the synchronous "try to begin"
/// phase and the asynchronous "run to completion" phase (U3's two-phase
/// split of [`Engine::resume`]/[`Engine::deliver_human_input`]).
///
/// Returned by [`Engine::try_begin_resume`] and
/// [`Engine::try_begin_deliver_human_input`] — both fail synchronously, with
/// no `.await`, if another pass already holds the execution
/// ([`EngineError::AlreadyRunning`]), which is what lets an HTTP handler
/// answer `409` immediately without spawning anything. On success, the
/// handle carries everything [`RunHandle::run_to_completion`] needs to
/// finish the job; that method is the only way to consume it, and is exactly
/// the thing a caller hands to `tokio::spawn`.
///
/// The exact shape here (an owned handle over `Arc<Engine>`, rather than the
/// plan's own sketch of `ResumeHandle<'_>` borrowing `&Engine`) is this
/// unit's implementation decision (see the plan's U3 Approach and Open
/// Questions): a borrowed handle cannot outlive the stack frame that created
/// it, which is exactly the frame an HTTP handler returns from before phase
/// 2 finishes.
pub struct RunHandle {
    pub(crate) engine: Arc<Engine>,
    pub(crate) execution_id: ExecutionId,
    pub(crate) claim: ActiveRunClaim,
    pub(crate) state: HandleState,
}

impl RunHandle {
    /// The execution this handle is driving.
    pub fn execution_id(&self) -> &str {
        &self.execution_id
    }

    /// Runs the prepared execution to completion (or, if the synchronous
    /// phase already stopped it, simply returns it) — the asynchronous half
    /// of the two-phase split, safe to hand to `tokio::spawn` because it owns
    /// everything it touches.
    ///
    /// Any error is recorded as an [`EventData::Error`] against the
    /// execution before it is returned, exactly as
    /// [`Engine::resume`]/[`Engine::deliver_human_input`] already do via
    /// `observed` — a caller that spawns this and discards the `JoinHandle`
    /// still gets the failure onto the event stream.
    pub async fn run_to_completion(
        self,
        definition: &WorkflowDefinition,
    ) -> Result<WorkflowExecution, EngineError> {
        let RunHandle {
            engine,
            execution_id,
            claim,
            state,
        } = self;

        let outcome = match state {
            HandleState::Done(execution) => Ok(execution),
            HandleState::Ready(execution) => engine.run_loop(definition, execution).await,
        };

        if let Err(error) = &outcome {
            engine.note_engine_error(&execution_id, error);
        }
        drop(claim);
        outcome
    }
}
