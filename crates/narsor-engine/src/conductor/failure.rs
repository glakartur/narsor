//! The conductor at a failed step (F3, U11).
//!
//! A step that failed after its `retry_policy` ran out fails the execution,
//! as it always did (KTD4), and the engine records the failure beside it
//! ([`StepFailure`]) and hands it here ([`ConductorRegistry::step_failed`]).
//! The conductor then works *beside* the failed execution: it holds no
//! claim (KTD10), so an operator may recover it, move it on or cancel it
//! meanwhile, and it never writes the execution — what it concludes lives
//! in its own store ([`FailureReport`]).
//!
//! # What it may do
//!
//! * **Diagnose** (R22): the diagnosis and the actions it proposes go to the
//!   chat — a message, then one proposal entry per action, carried out only
//!   once the operator confirms it (U12) — and `ProblemRaised { kind:
//!   failure }` announces the problem. The execution stays `Failed`.
//! * **Retry once** (R21, KTD14), and only when no attempt of the step can
//!   have had a side effect and the failure's episode has not used its
//!   retry: the retry is marked used, durably, before an ordinary recovery
//!   with the conductor's note is started under the failure's key. An
//!   operator who acted first makes the key stale, and nothing restarts.
//!   A retry the conductor may not take becomes a proposal.
//! * **Never choose a transition** (R23): a response that does is refused by
//!   the purpose's check, and a transition can only be proposed.
//!
//! A conductor that cannot answer — after R24's one repeat, which the actor
//! makes — leaves the operator the raw error as the problem, and nothing it
//! did or failed to do triggers the conductor again (R25).
//!
//! # Restart
//!
//! The failure record of a `Failed` execution with no report of the
//! conductor's for it is a diagnosis still owed. The startup hook
//! ([`ConductorRegistry::restore_failures_at_startup`]) runs it again — the
//! one repeat R24 allows when the restart cut an invocation short, counted
//! before it runs. The record goes with the status change of the recovery
//! that answers the failure, so a recovered execution owes nothing.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::{json, Map};
use uuid::Uuid;

use crate::conductor::actor::{ConductorActor, InvocationOutcome, MAX_AUTOMATIC_ATTEMPTS};
use crate::conductor::response::{ConductorResponse, Decision, ProposalKind, ProposedAction};
use crate::conductor::store::{
    ChatAuthor, ChatEntry, ChatEntryKind, ConductorState, ConductorStore, FailureOutcome,
    FailureReport, QueuedRequest,
};
use crate::conductor::ConductorRegistry;
use crate::domain::{
    ConductorDecisionKind, ConductorPurpose, ConductorTrigger, Event, EventData, ExecutionId,
    ProblemKind, WorkflowDefinition, WorkflowExecution,
};
use crate::engine::{step_failure, EngineError, Recovery, StepFailure};
use crate::persistence::{ExecutionStore, Result};

/// How long the conductor's retry waits for the pass that failed to let go
/// of the execution's claim.
const RETRY_CLAIM_WAIT: Duration = Duration::from_secs(10);

/// How often it looks again meanwhile.
const RETRY_CLAIM_POLL: Duration = Duration::from_millis(25);

/// A pass the conductor starts, driven like any other the daemon runs —
/// the conductor's one retry of a failed step (R21).
///
/// A seam of its own because the daemon owns what a pass needs: the
/// catalogue the definition is resolved against, and the registry of
/// background passes that makes it interruptible. Without one, the
/// conductor never retries by itself: a retry becomes a proposal.
pub trait PassLauncher: Send + Sync {
    /// The definition `execution` runs, read before its claim is taken, so
    /// a definition that cannot be read never leaves a claimed execution
    /// without a pass.
    fn definition(
        &self,
        execution: &WorkflowExecution,
    ) -> std::result::Result<WorkflowDefinition, String>;

    /// Drives `pass` in the background, as the pass of `execution_id`.
    fn launch(&self, execution_id: &str, pass: Pin<Box<dyn Future<Output = ()> + Send>>);
}

/// Whether the agent attempt `action_execution_id` left a trace of having
/// acted in `events` (KTD14): a captured harness session, a message, a
/// resumed session. Judged conservatively — anything that may have acted
/// counts.
pub fn agent_attempt_left_traces(events: &[Event], action_execution_id: &str) -> bool {
    let of_attempt = |id: &Option<String>| id.as_deref() == Some(action_execution_id);
    events.iter().any(|event| match &event.data {
        EventData::AgentMessage {
            action_execution_id: id,
            ..
        } => of_attempt(id),
        EventData::AgentSessionStarted {
            action_execution_id: id,
            resumed_session,
            ..
        } => of_attempt(id) && resumed_session.is_some(),
        EventData::AgentSessionCompleted {
            action_execution_id: id,
            harness_session_id,
            ..
        }
        | EventData::AgentSessionFailed {
            action_execution_id: id,
            harness_session_id,
            ..
        } => of_attempt(id) && harness_session_id.is_some(),
        _ => false,
    })
}

/// The conductor's report of the failure `execution` is still failed on,
/// when it leaves a problem for the operator (KTD4): what `GET /executions`
/// shows as the execution's problem.
pub fn active_failure_problem(
    store: &ExecutionStore,
    execution: &WorkflowExecution,
) -> Option<FailureReport> {
    let failure = step_failure(execution).ok().flatten()?;
    let conductor = ConductorStore::new(store.paths(), &execution.id);
    if !conductor.exists() {
        return None;
    }
    conductor
        .load_state()
        .ok()?
        .failure
        .filter(|report| report.key == failure.key() && report.is_problem())
}

/// The diagnoses under way, by execution and failure key.
#[derive(Debug, Default)]
pub(super) struct Diagnoses {
    running: Mutex<HashSet<(ExecutionId, String)>>,
}

impl Diagnoses {
    fn begin(&self, execution_id: &str, key: &str) -> Option<DiagnosisGuard<'_>> {
        let entry = (execution_id.to_string(), key.to_string());
        if !self.lock().insert(entry.clone()) {
            return None;
        }
        Some(DiagnosisGuard {
            diagnoses: self,
            entry,
        })
    }

    fn keys_of(&self, execution_id: &str) -> HashSet<String> {
        self.lock()
            .iter()
            .filter(|(id, _)| id == execution_id)
            .map(|(_, key)| key.clone())
            .collect()
    }

    fn lock(&self) -> MutexGuard<'_, HashSet<(ExecutionId, String)>> {
        self.running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Ends a diagnosis when it goes away.
struct DiagnosisGuard<'a> {
    diagnoses: &'a Diagnoses,
    entry: (ExecutionId, String),
}

impl Drop for DiagnosisGuard<'_> {
    fn drop(&mut self) {
        self.diagnoses.lock().remove(&self.entry);
    }
}

impl ConductorRegistry {
    /// The engine's trigger (F3): `execution_id` just failed on a step the
    /// conductor is to diagnose. Starts the diagnosis in the background and
    /// returns at once — the pass that failed is not held up by it.
    pub fn step_failed(self: &Arc<Self>, execution_id: &str) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                execution_id,
                "conductor: no runtime to diagnose a failed step on; it is left to the operator"
            );
            return;
        };
        let registry = Arc::clone(self);
        let execution_id = execution_id.to_string();
        runtime.spawn(async move {
            if let Err(error) = registry.diagnose_failure(&execution_id).await {
                tracing::warn!(
                    execution_id = %execution_id,
                    error = %error,
                    "conductor: could not diagnose a failed step"
                );
            }
        });
    }

    /// Diagnoses the failure `execution_id` is failed on and acts on the
    /// answer (see the module documentation); does nothing when the
    /// execution owes no diagnosis — it is not failed on a recorded step
    /// failure, the conductor already concluded about this one, or another
    /// diagnosis of it is under way.
    pub async fn diagnose_failure(self: &Arc<Self>, execution_id: &str) -> Result<()> {
        let execution = self.engine.store().load_execution(execution_id)?;
        let failure = match step_failure(&execution) {
            Ok(Some(failure)) => failure,
            Ok(None) => return Ok(()),
            Err(error) => {
                tracing::warn!(
                    execution_id,
                    error = %error,
                    "conductor: the step failure is unreadable; it is left to the operator"
                );
                return Ok(());
            }
        };
        let Some(_diagnosis) = self.diagnoses.begin(execution_id, failure.key()) else {
            return Ok(());
        };

        let actor = self.actor(execution_id)?;
        // What an earlier run left for this failure is superseded by this
        // one; the requests of another failure's diagnosis under way stay.
        let mut running = self.diagnoses.keys_of(execution_id);
        running.remove(failure.key());
        actor.update_state(|state| forget_parked_diagnoses(state, &running))?;
        let state = actor.state();

        if let Some(report) = state
            .failure
            .as_ref()
            .filter(|report| report.key == failure.key())
        {
            if !report.is_problem() {
                // The retry was decided and marked used, but the restart
                // never happened: the operator takes it from here.
                self.conclude(
                    &actor,
                    &failure,
                    FailureOutcome::Unanswered {
                        error: "the conductor decided to restart the step, but the daemon \
                                stopped before the restart began"
                            .to_string(),
                    },
                    None,
                )?;
            }
            return Ok(());
        }

        let retry = self.retry_refusal(&failure, &state);
        let attempts = state.attempts(failure.key());
        let outcome = if attempts >= MAX_AUTOMATIC_ATTEMPTS {
            InvocationOutcome::Failed {
                error: format!(
                    "the conductor was already invoked {attempts} times for this failure, and \
                     an invocation a restart cut short is repeated only once"
                ),
            }
        } else {
            actor
                .invoke(diagnosis_request(&failure, retry.is_none()))
                .await?
        };

        let answered = match outcome {
            InvocationOutcome::Answered(answer) => {
                ConductorResponse::from_value(answer.response, ConductorPurpose::DiagnoseFailure)
                    .map_err(|error| error.to_string())
            }
            InvocationOutcome::Failed { error } => Err(error),
            InvocationOutcome::Interrupted => {
                Err("the conductor was stopped before it answered".to_string())
            }
        };
        let response = match answered {
            Ok(response) => response,
            Err(error) => {
                // R24's repeat is spent: the raw error, and nothing more (R25).
                return self.conclude(&actor, &failure, FailureOutcome::Unanswered { error }, None);
            }
        };

        match response.decision.clone() {
            Decision::Retry { note, diagnosis } => match retry {
                None => {
                    self.retry(&actor, &failure, &response, note, diagnosis)
                        .await
                }
                Some(refusal) => {
                    // Not the conductor's to take: proposed instead.
                    let mut proposals = response.proposals.clone();
                    if !proposals
                        .iter()
                        .any(|proposal| proposal.kind == ProposalKind::Retry)
                    {
                        proposals.insert(0, retry_proposal(&note));
                    }
                    let diagnosis = diagnosis
                        .or_else(|| response.rationale.clone())
                        .unwrap_or_else(|| failure.error.clone());
                    self.conclude(
                        &actor,
                        &failure,
                        FailureOutcome::Diagnosed {
                            diagnosis: format!(
                                "{diagnosis} (The conductor would restart the step, but {refusal}.)"
                            ),
                            proposals,
                        },
                        Some(&response),
                    )
                }
            },
            Decision::Diagnose { diagnosis } => self.conclude(
                &actor,
                &failure,
                FailureOutcome::Diagnosed {
                    diagnosis,
                    proposals: response.proposals.clone(),
                },
                Some(&response),
            ),
            other => {
                // Refused by the purpose's check before it gets here (R23).
                self.conclude(
                    &actor,
                    &failure,
                    FailureOutcome::Unanswered {
                        error: format!(
                            "the conductor answered '{}', which a diagnosis does not take",
                            other.kind().name()
                        ),
                    },
                    None,
                )
            }
        }
    }

    /// Runs the diagnoses the executions on disk still owe (the daemon
    /// restart matrix: `Failed` with the F3 record and no report of the
    /// conductor's for it), and drops what a diagnosis left in the store of
    /// an execution that owes none. Returns the executions diagnosed again.
    ///
    /// Run once at startup, after `persistence::recovery::recover`. An
    /// execution that cannot be read is skipped with a warning.
    pub fn restore_failures_at_startup(self: &Arc<Self>) -> Result<Vec<ExecutionId>> {
        let mut diagnosed = Vec::new();
        for execution_id in self.engine.store().list_execution_ids()? {
            let execution = match self.engine.store().load_execution(&execution_id) {
                Ok(execution) => execution,
                Err(error) => {
                    tracing::warn!(
                        execution_id = %execution_id,
                        error = %error,
                        "conductor: could not read an execution at startup"
                    );
                    continue;
                }
            };
            let conductor = self.store(&execution_id);
            let state = if conductor.exists() {
                conductor.load_state().unwrap_or_default()
            } else {
                ConductorState::default()
            };

            match step_failure(&execution) {
                Ok(Some(failure))
                    if !state.failure.as_ref().is_some_and(|report| {
                        report.key == failure.key() && report.is_problem()
                    }) =>
                {
                    self.step_failed(&execution_id);
                    diagnosed.push(execution_id);
                }
                _ if holds_diagnosis(&state) => {
                    if let Err(error) = self.actor(&execution_id).and_then(|actor| {
                        actor.update_state(|state| forget_parked_diagnoses(state, &HashSet::new()))
                    }) {
                        tracing::warn!(
                            execution_id = %execution_id,
                            error = %error,
                            "conductor: could not drop a stale diagnosis"
                        );
                    }
                }
                _ => {}
            }
        }
        Ok(diagnosed)
    }

    /// Why the conductor may not retry `failure` itself, or `None` when it
    /// may (R21, KTD14).
    fn retry_refusal(&self, failure: &StepFailure, state: &ConductorState) -> Option<&'static str> {
        if !failure.side_effect_free {
            Some("the failed attempt may have had side effects")
        } else if failure.conductor_retry || state.retry_used(&failure.episode) {
            Some("its one retry of this failure was already used")
        } else if self.launcher.get().is_none() {
            Some("it cannot start a pass here")
        } else {
            None
        }
    }

    /// The conductor's one retry (R21): marked used, recorded, then an
    /// ordinary recovery with its note under the failure's key.
    async fn retry(
        &self,
        actor: &ConductorActor,
        failure: &StepFailure,
        response: &ConductorResponse,
        note: String,
        diagnosis: Option<String>,
    ) -> Result<()> {
        let execution_id = actor.execution_id();
        let Some(launcher) = self.launcher.get().cloned() else {
            return self.conclude(
                actor,
                failure,
                FailureOutcome::Unanswered {
                    error: "the conductor cannot start a pass here".to_string(),
                },
                None,
            );
        };
        let execution = self.engine.store().load_execution(execution_id)?;
        let definition = match launcher.definition(&execution) {
            Ok(definition) => definition,
            Err(error) => {
                return self.conclude(
                    actor,
                    failure,
                    FailureOutcome::Unanswered {
                        error: format!("the conductor's retry could not start: {error}"),
                    },
                    None,
                );
            }
        };

        // Durable before the recovery (KTD14): a restart in between finds
        // the retry used and never runs a second one.
        let report = FailureReport {
            key: failure.key().to_string(),
            episode: failure.episode.clone(),
            state_id: failure.state_id.clone(),
            reason: diagnosis.clone().unwrap_or_else(|| failure.error.clone()),
            outcome: FailureOutcome::Retried { note: note.clone() },
            concluded_at: Utc::now(),
        };
        actor.update_state(|state| {
            state.used_retries.insert(failure.episode.clone());
            state.failure = Some(report);
        })?;

        let decision_id = Uuid::new_v4().to_string();
        let text = non_empty(response.message.as_deref()).unwrap_or_else(|| {
            format!(
                "Restarting the failed step '{}' once: {note}",
                failure.state_id
            )
        });
        let mut entry = ChatEntry::new(ChatAuthor::Conductor, ChatEntryKind::Decision, text);
        entry.payload = Some(json!({
            "decision_id": decision_id,
            "kind": ConductorDecisionKind::Retry,
            "episode": failure.key(),
            "retry_episode": failure.episode,
            "state_id": failure.state_id,
            "note": note,
            "diagnosis": diagnosis,
            "confidence": response.confidence,
            "rationale": response.rationale,
        }));
        actor.append_chat(entry)?;
        self.record(
            execution_id,
            EventData::ConductorDecision {
                decision_id,
                kind: ConductorDecisionKind::Retry,
                state_id: Some(failure.state_id.clone()),
                output_label: None,
                episode: Some(failure.key().to_string()),
                confidence: response.confidence,
                rationale: response.rationale.clone(),
            },
        );

        match self.begin_retry(execution_id, failure, note).await {
            Ok(handle) => {
                launcher.launch(
                    execution_id,
                    Box::pin(async move {
                        let _ = handle.run_to_completion(&definition).await;
                    }),
                );
                Ok(())
            }
            Err(EngineError::StaleEpisode { .. } | EngineError::NotRunnable { .. }) => {
                // The operator acted first (KTD9): nothing restarts, and
                // what they did stands.
                actor.append_chat(ChatEntry::new(
                    ChatAuthor::Conductor,
                    ChatEntryKind::Message,
                    "The execution changed before my restart could begin — the operator acted \
                     on the failure first — so nothing was restarted.",
                ))?;
                Ok(())
            }
            Err(error) => self.conclude(
                actor,
                failure,
                FailureOutcome::Unanswered {
                    error: format!("the conductor's retry could not start: {error}"),
                },
                None,
            ),
        }
    }

    /// Takes the claim for the conductor's retry, under the failure's key.
    ///
    /// The pass that failed may still hold the claim for a moment after the
    /// trigger: while the execution is still failed on this key, a claim
    /// held is waited for — anyone else who takes it changes the key first.
    async fn begin_retry(
        &self,
        execution_id: &str,
        failure: &StepFailure,
        note: String,
    ) -> std::result::Result<crate::engine::RunHandle, EngineError> {
        let deadline = Instant::now() + RETRY_CLAIM_WAIT;
        loop {
            let recovery = Recovery {
                comment: Some(note.clone()),
                episode: Some(failure.key().to_string()),
                conductor_retry: true,
            };
            match self.engine.try_begin_recover_with(execution_id, recovery) {
                Err(EngineError::AlreadyRunning { .. })
                    if Instant::now() < deadline && self.still_failed_on(execution_id, failure) =>
                {
                    tokio::time::sleep(RETRY_CLAIM_POLL).await;
                }
                other => return other,
            }
        }
    }

    /// Whether `execution_id` is still failed on `failure`.
    fn still_failed_on(&self, execution_id: &str, failure: &StepFailure) -> bool {
        self.engine
            .store()
            .load_execution(execution_id)
            .ok()
            .and_then(|execution| step_failure(&execution).ok().flatten())
            .is_some_and(|current| current.key() == failure.key())
    }

    /// Concludes about `failure` with a problem for the operator (KTD4): the
    /// diagnosis and its proposals in the chat, the report in the store,
    /// then `ProblemRaised`.
    ///
    /// A diagnosis reaches the chat whatever happened meanwhile; the problem
    /// is raised only while the execution is still failed on `failure`.
    fn conclude(
        &self,
        actor: &ConductorActor,
        failure: &StepFailure,
        outcome: FailureOutcome,
        response: Option<&ConductorResponse>,
    ) -> Result<()> {
        let execution_id = actor.execution_id();
        let reason = match &outcome {
            FailureOutcome::Diagnosed { diagnosis, .. } => diagnosis.clone(),
            FailureOutcome::Unanswered { error } => format!(
                "{} (the conductor could not diagnose it: {error})",
                failure.error
            ),
            FailureOutcome::Retried { note } => note.clone(),
        };

        if let FailureOutcome::Diagnosed {
            diagnosis,
            proposals,
        } = &outcome
        {
            let text = non_empty(response.and_then(|response| response.message.as_deref()))
                .unwrap_or_else(|| diagnosis.clone());
            let mut entry = ChatEntry::new(ChatAuthor::Conductor, ChatEntryKind::Message, text);
            entry.payload = Some(json!({
                "kind": "diagnosis",
                "episode": failure.key(),
                "state_id": failure.state_id,
                "diagnosis": diagnosis,
                "confidence": response.map(|response| response.confidence),
                "rationale": response.and_then(|response| response.rationale.clone()),
            }));
            actor.append_chat(entry)?;
            for proposal in proposals {
                let text =
                    non_empty(proposal.summary.as_deref()).unwrap_or_else(|| proposal.describe());
                let mut entry =
                    ChatEntry::new(ChatAuthor::Conductor, ChatEntryKind::Proposal, text);
                entry.payload = Some(json!({
                    "episode": failure.key(),
                    "state_id": failure.state_id,
                    "proposal": proposal,
                    "confidence": response.map(|response| response.confidence),
                    "rationale": response.and_then(|response| response.rationale.clone()),
                }));
                actor.append_chat(entry)?;
            }
        }

        if !self.still_failed_on(execution_id, failure) {
            return Ok(());
        }
        let report = FailureReport {
            key: failure.key().to_string(),
            episode: failure.episode.clone(),
            state_id: failure.state_id.clone(),
            reason: reason.clone(),
            outcome,
            concluded_at: Utc::now(),
        };
        actor.update_state(|state| state.failure = Some(report))?;
        self.record(
            execution_id,
            EventData::ProblemRaised {
                state_id: failure.state_id.clone(),
                action_execution_id: failure.key().to_string(),
                kind: ProblemKind::Failure,
                reason,
                episode: Some(failure.key().to_string()),
            },
        );
        Ok(())
    }

    fn record(&self, execution_id: &str, data: EventData) {
        if let Err(error) = self.engine.record_event(execution_id, data) {
            tracing::warn!(
                execution_id,
                error = %error,
                "conductor: could not record an event"
            );
        }
    }
}

/// The request to diagnose `failure`, keyed by the failure (R24 counts
/// per failure), carrying what the snapshot does not say by itself.
fn diagnosis_request(failure: &StepFailure, retry_allowed: bool) -> QueuedRequest {
    QueuedRequest::new(
        ConductorTrigger::StepFailure,
        ConductorPurpose::DiagnoseFailure,
    )
    .with_episode(failure.key())
    .with_payload(json!({
        "state_id": failure.state_id,
        "action_execution_id": failure.action_execution_id,
        "error": failure.error,
        "side_effect_free": failure.side_effect_free,
        "retried_by_conductor": failure.conductor_retry,
        "retry_allowed": retry_allowed,
    }))
}

/// Whether `state` holds anything of a diagnosis: a queued request or a
/// marker.
fn holds_diagnosis(state: &ConductorState) -> bool {
    let is_diagnosis = |request: &QueuedRequest| request.trigger == ConductorTrigger::StepFailure;
    state.queue.iter().any(is_diagnosis)
        || state
            .pending
            .as_ref()
            .is_some_and(|pending| is_diagnosis(&pending.request))
}

/// Drops the diagnosis requests and marker `state` holds, but those of the
/// failures in `running`: whatever else is left is from a diagnosis a
/// restart cut short, which the execution's own record supersedes.
fn forget_parked_diagnoses(state: &mut ConductorState, running: &HashSet<String>) {
    let stale = |request: &QueuedRequest| {
        request.trigger == ConductorTrigger::StepFailure
            && !request
                .episode
                .as_ref()
                .is_some_and(|episode| running.contains(episode))
    };
    state.queue.retain(|request| !stale(request));
    if state
        .pending
        .as_ref()
        .is_some_and(|pending| stale(&pending.request))
    {
        state.pending = None;
    }
}

/// The proposal a retry the conductor may not take becomes.
fn retry_proposal(note: &str) -> ProposedAction {
    ProposedAction {
        kind: ProposalKind::Retry,
        summary: Some("Restart the failed step once, with the conductor's note".to_string()),
        label: None,
        values: Map::new(),
        text: Some(note.to_string()),
    }
}

/// `text`, when it says anything.
fn non_empty(text: Option<&str>) -> Option<String> {
    text.map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToString::to_string)
}
