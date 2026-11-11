//! Resuming a paused execution and recovering an interrupted one.

use super::*;

impl Engine {
    /// Resumes a paused execution and runs it on.
    ///
    /// Refuses an execution that needs input: that one needs an
    /// answer, not a nudge.
    ///
    /// A thin wrapper, unchanged in contract, over the two-phase split U3
    /// introduced for the REST endpoint: [`Engine::try_begin_resume`] claims
    /// the run and does the synchronous half, then
    /// [`RunHandle::run_to_completion`] does the rest — sequentially, in one
    /// `.await`, exactly as this used to be written as a single block.
    pub async fn resume(
        &self,
        definition: &WorkflowDefinition,
        execution_id: &str,
    ) -> Result<WorkflowExecution, EngineError> {
        self.observed(execution_id, async {
            let _guard = self.begin_run(execution_id)?;
            let execution = self.prepare_resume(execution_id)?;
            self.run_loop(definition, execution).await
        })
        .await
    }

    /// The synchronous half of [`Engine::resume`]: validates the execution's
    /// status and, for a manually `Paused` one, flips it back to `Running` and emits
    /// `WorkflowResumed` — everything [`Engine::resume`] did before it
    /// reached `run_loop`. Shared, not duplicated, between the borrowed
    /// (`resume`) and owned ([`Engine::try_begin_resume`]) entry points.
    fn prepare_resume(&self, execution_id: &str) -> Result<WorkflowExecution, EngineError> {
        let mut execution = self.store.load_execution(execution_id)?;

        match execution.status {
            ExecutionStatus::Running => {}
            ExecutionStatus::Paused => {
                self.lock_pause_requests().remove(execution_id);
                execution.status = ExecutionStatus::Running;
                self.store.save_execution(&execution)?;
                self.emit(execution_id, EventData::WorkflowResumed)?;
            }
            _ => return Err(self.not_runnable(&execution, "resumed")),
        }

        Ok(execution)
    }

    /// The synchronous half of an explicit operator recovery. Recovery is
    /// deliberately limited to terminal failure or interruption states, so it
    /// cannot turn daemon startup into an automatic replay.
    ///
    /// A recovery naming a step failure's key is refused once the execution
    /// is no longer failed on it (KTD9); the failure record goes in the same
    /// save as the status change, and so does the conductor's retry
    /// ([`Recovery`]).
    pub(crate) fn prepare_recover(
        &self,
        execution_id: &str,
        recovery: Recovery,
    ) -> Result<WorkflowExecution, EngineError> {
        let mut execution = self.store.load_execution(execution_id)?;
        match execution.status {
            ExecutionStatus::Failed | ExecutionStatus::Unknown => {}
            _ if recovery.episode.is_some() => {
                return Err(EngineError::StaleEpisode {
                    execution_id: execution_id.to_string(),
                    expected: recovery.episode.unwrap_or_default(),
                    current: None,
                })
            }
            _ => return Err(self.not_runnable(&execution, "recovered")),
        }
        self.recover_step_failure(&mut execution, &recovery)?;
        let comment = recovery.comment;

        // A recovery starts the state's action afresh. A rejected output left
        // behind by an interrupted reprompt must not turn it into another one,
        // and a request left behind must say it was closed by the recovery
        // rather than linger as if it could still be answered.
        clear_engine_metadata(&mut execution, METADATA_OUTPUT_REJECTION_KEY);
        clear_engine_metadata(&mut execution, METADATA_OUTPUT_RESCUE_KEY);
        let from = self.store.event_log(execution_id).next_sequence()?;
        human::close_pending(
            &self.store,
            execution_id,
            InputCloseReason::Recover,
            AnsweredBy::Operator,
        )?;
        self.publish_from(execution_id, from)?;

        if let Some(comment) = comment.filter(|comment| !comment.trim().is_empty()) {
            engine_metadata_mut(&mut execution).insert(
                METADATA_OPERATOR_RECOVERY_NOTE_KEY.to_string(),
                Value::String(comment),
            );
        }

        self.lock_pause_requests().remove(execution_id);
        execution.status = ExecutionStatus::Running;
        execution.completed_at = None;
        self.store.save_execution(&execution)?;
        self.emit(execution_id, EventData::WorkflowResumed)?;
        Ok(execution)
    }

    /// Phase 1 of the two-phase `resume` (U3): claims the execution
    /// synchronously — returning [`EngineError::AlreadyRunning`] immediately,
    /// with no `.await`, if another pass already holds it — and does the
    /// same synchronous validation [`Engine::resume`] does. On success, hand
    /// the returned [`RunHandle`] to [`RunHandle::run_to_completion`],
    /// typically via `tokio::spawn`, to actually drive the workflow on.
    ///
    /// Takes `self: &Arc<Self>` (rather than `&self`) because the handle it
    /// returns must be able to outlive this call's stack frame — the whole
    /// point of splitting this out of [`Engine::resume`] is to let an HTTP
    /// handler return `202 Accepted` right after this succeeds, while phase
    /// 2 keeps running after the response is sent.
    pub fn try_begin_resume(
        self: &Arc<Self>,
        execution_id: &str,
    ) -> Result<RunHandle, EngineError> {
        let claim = self.claim_active_run(execution_id)?;
        match self.prepare_resume(execution_id) {
            Ok(execution) => Ok(RunHandle {
                engine: Arc::clone(self),
                execution_id: execution_id.to_string(),
                claim,
                state: HandleState::Ready(execution),
            }),
            Err(error) => {
                self.note_engine_error(execution_id, &error);
                Err(error)
            }
        }
    }

    /// Claims a failed or crash-interrupted execution and starts one new
    /// attempt. The caller must invoke this explicitly; recovery is never
    /// performed while reconciling a stopped daemon.
    pub fn try_begin_recover(
        self: &Arc<Self>,
        execution_id: &str,
        comment: Option<String>,
    ) -> Result<RunHandle, EngineError> {
        self.try_begin_recover_with(execution_id, Recovery::with_comment(comment))
    }

    /// [`Engine::try_begin_recover`] as `recovery` describes it: with the key
    /// of the step failure it answers, and for the conductor's one retry.
    ///
    /// A stale key is [`EngineError::StaleEpisode`], and — like
    /// [`EngineError::AlreadyRunning`] — is not recorded as an `Error`
    /// event: whoever acted first is what the log shows.
    pub fn try_begin_recover_with(
        self: &Arc<Self>,
        execution_id: &str,
        recovery: Recovery,
    ) -> Result<RunHandle, EngineError> {
        let claim = self.claim_active_run(execution_id)?;
        match self.prepare_recover(execution_id, recovery) {
            Ok(execution) => Ok(RunHandle {
                engine: Arc::clone(self),
                execution_id: execution_id.to_string(),
                claim,
                state: HandleState::Ready(execution),
            }),
            Err(error) => {
                if !matches!(error, EngineError::StaleEpisode { .. }) {
                    self.note_engine_error(execution_id, &error);
                }
                Err(error)
            }
        }
    }
}
