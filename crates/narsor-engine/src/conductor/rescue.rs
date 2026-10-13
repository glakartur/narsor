//! The conductor's half of the label rescue (F1, U10).
//!
//! The engine drives the rescue — it holds the claim and is the only writer
//! of the execution (KTD2, KTD10) — and comes here for what is the
//! conductor's: the inline invocation, and the durable record of what the
//! conductor decided.
//!
//! # Decisions are saved before they are applied (KTD13)
//!
//! A chosen output (a) is saved in `state.json`'s `unapplied_decisions`,
//! with the execution's context from before it, before the engine applies
//! it; the engine drops it once the transition is taken. A restart in
//! between finds it there and applies it instead of asking again. Every
//! decision — (a), (b), (c) — is also written to the chat as a
//! [`ChatEntryKind::Decision`] entry whose `payload` is the whole
//! [`SavedDecision`]: that entry outlives the application and is what an
//! undo reads the context to restore from (R31, U12).
//!
//! # R24 across restarts
//!
//! The actor counts every invocation of an episode before it runs, and
//! repeats a failed one once. A restart during an invocation is that
//! repeat: the next pass invokes again as the second try, and a failure
//! then is final. An episode whose count is already used up is not
//! invoked at all — it fails straight away, and the step becomes a problem.

use serde_json::Value;

use crate::conductor::actor::{InvocationOutcome, MAX_AUTOMATIC_ATTEMPTS};
use crate::conductor::store::{
    ChatAuthor, ChatEntry, ChatEntryKind, ConductorState, QueuedRequest, SavedDecision,
};
use crate::conductor::ConductorRegistry;
use crate::domain::{ConductorDecisionKind, ConductorTrigger};
use crate::persistence::Result;

impl ConductorRegistry {
    /// Invokes the conductor inline for the rescue `request` (F1) and waits
    /// for its outcome, R24's one repeat included.
    ///
    /// Dropping the returned future — interrupting the pass that waits —
    /// stops the invocation and keeps its marker, for the startup hook.
    /// A marker or a queued request an earlier pass left for this execution
    /// is superseded by this request and dropped first.
    pub async fn choose_output(
        &self,
        execution_id: &str,
        request: QueuedRequest,
    ) -> Result<InvocationOutcome> {
        let actor = self.actor(execution_id)?;
        let episode = request.episode.clone().unwrap_or_default();
        let attempts = actor.state().attempts(&episode);
        if attempts >= MAX_AUTOMATIC_ATTEMPTS {
            return Ok(InvocationOutcome::Failed {
                error: format!(
                    "the conductor was already invoked {attempts} times for this step, and an \
                     invocation a restart cut short is repeated only once"
                ),
            });
        }
        actor.update_state(forget_parked_rescue)?;
        actor.invoke(request).await
    }

    /// The decision saved for the rescue `episode` and not yet applied, if
    /// there is one.
    pub fn unapplied_decision(
        &self,
        execution_id: &str,
        episode: &str,
    ) -> Result<Option<SavedDecision>> {
        if !self.store(execution_id).exists() {
            return Ok(None);
        }
        Ok(self
            .actor(execution_id)?
            .state()
            .unapplied_decisions
            .into_iter()
            .find(|decision| decision.episode.as_deref() == Some(episode)))
    }

    /// Saves a chosen output before the engine applies it (KTD13): among
    /// the unapplied decisions, and in the chat.
    pub fn save_decision(
        &self,
        execution_id: &str,
        decision: &SavedDecision,
        message: Option<&str>,
    ) -> Result<()> {
        let actor = self.actor(execution_id)?;
        actor.update_state(|state| {
            state
                .unapplied_decisions
                .retain(|saved| saved.decision_id != decision.decision_id);
            state.unapplied_decisions.push(decision.clone());
        })?;
        self.note_decision(execution_id, decision, message)
    }

    /// Writes `decision` to the chat, for a decision that is carried out
    /// right away ((b), (c)) or was just saved ((a)).
    pub fn note_decision(
        &self,
        execution_id: &str,
        decision: &SavedDecision,
        message: Option<&str>,
    ) -> Result<()> {
        let text = message
            .map(str::trim)
            .filter(|message| !message.is_empty())
            .map(ToString::to_string)
            .unwrap_or_else(|| describe(decision));
        let mut entry = ChatEntry::new(ChatAuthor::Conductor, ChatEntryKind::Decision, text);
        entry.payload = Some(serde_json::to_value(decision).unwrap_or(Value::Null));
        self.actor(execution_id)?.append_chat(entry)?;
        Ok(())
    }

    /// Drops the saved decision `decision_id` once the engine applied it.
    pub fn decision_applied(&self, execution_id: &str, decision_id: &str) -> Result<()> {
        self.actor(execution_id)?.update_state(|state| {
            state
                .unapplied_decisions
                .retain(|saved| saved.decision_id != decision_id);
        })
    }

    /// Drops what the rescue of `execution_id` left in the conductor's
    /// store — a marker, a queued request, unapplied decisions — for an
    /// execution that no longer owes one (the startup hook). Returns
    /// whether there was anything.
    pub fn discard_rescue(&self, execution_id: &str) -> Result<bool> {
        let actor = self.actor(execution_id)?;
        actor.update_state(|state| {
            let before = state.clone();
            forget_parked_rescue(state);
            state.unapplied_decisions.clear();
            *state != before
        })
    }
}

/// Drops the rescue requests and the rescue marker `state` holds.
///
/// Rescue requests are never served from the actor's own queue after a
/// restart (they are parked), and at most one rescue runs for an execution
/// at a time — the one of the pass holding its claim — so anything left is
/// stale.
fn forget_parked_rescue(state: &mut ConductorState) {
    let is_rescue = |request: &QueuedRequest| request.trigger == ConductorTrigger::OutputRescue;
    state.queue.retain(|request| !is_rescue(request));
    if state
        .pending
        .as_ref()
        .is_some_and(|pending| is_rescue(&pending.request))
    {
        state.pending = None;
    }
}

/// A chat line for a decision the conductor left no message about.
fn describe(decision: &SavedDecision) -> String {
    let what = match (decision.kind, &decision.output_label) {
        (ConductorDecisionKind::ChooseOutput, Some(label)) => {
            format!("Completed the step with output '{label}'")
        }
        (ConductorDecisionKind::Reprompt, _) => "Asked the agent again".to_string(),
        (ConductorDecisionKind::Escalate, _) => "Handed the step to the operator".to_string(),
        (kind, _) => format!("Decided: {}", kind.name()),
    };
    match &decision.rationale {
        Some(rationale) => format!(
            "{what} ({} confidence): {rationale}",
            decision.confidence.name()
        ),
        None => format!("{what} ({} confidence).", decision.confidence.name()),
    }
}
