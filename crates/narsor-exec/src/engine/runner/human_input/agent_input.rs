//! The one-shot answer an agent step is resumed with.

use super::*;

/// The one-shot instruction delivered by an operator to an agent step.
/// `harness` and `session` are optional for records written before
/// continuation metadata was persisted.
pub(crate) struct AgentInput {
    pub(crate) answer: String,
    pub(crate) harness: Option<String>,
    pub(crate) session: Option<String>,
    /// Claude Code allow rules a resolved permission request grants this
    /// one resumed run; empty for any other answer.
    pub(crate) permission_rules: Vec<String>,
    /// Who wrote the answer; records without one came from a person.
    pub(crate) author: PromptAuthor,
    /// Whether a session the harness refuses starts the step again at once,
    /// with the answer in the operator recovery note, instead of asking the
    /// owner: so it is for the owner's message a recovery gives.
    pub(crate) restart_if_refused: bool,
    /// The operator recovery note the attempt took with the answer.
    pub(crate) note: Option<String>,
}

/// The author of the prompt that carries an answer back to the agent.
pub(crate) fn prompt_author(answered_by: AnsweredBy) -> PromptAuthor {
    match answered_by {
        // An observer answers gates only, never an agent's question; its
        // answer reads as the owner's, whose script it runs.
        AnsweredBy::Operator | AnsweredBy::Observer => PromptAuthor::User,
        AnsweredBy::Conductor => PromptAuthor::Conductor,
    }
}

/// The operator recovery note that gives the owner's `text` after `note`,
/// the reason the step goes on.
pub(crate) fn note_with_message(note: Option<&str>, text: &str) -> String {
    match note.filter(|note| !note.trim().is_empty()) {
        Some(note) => format!("{note}\n\n{text}"),
        None => text.to_string(),
    }
}

/// Where an owner's message left for the agent went.
pub(crate) enum MessageLeft {
    /// A message of that key was left before: nothing was left again.
    Before,
    /// Into the session the agent goes on in.
    InSession,
    /// Nowhere yet: there is no session, so the text is the caller's to put
    /// in the operator recovery note.
    AsNote,
}

/// Leaves the owner's message `text`, under `key`, for the agent of
/// `state_id`: into `session` when there is one, and the key with it, so a
/// key is left once. Writes `execution` without saving it.
pub(crate) fn leave_owner_message(
    execution: &mut WorkflowExecution,
    state_id: &str,
    key: &str,
    text: &str,
    session: Option<&ResumableSession>,
    restart_if_refused: bool,
) -> MessageLeft {
    let engine = engine_metadata_mut(execution);
    if engine.get(METADATA_RESUME_MESSAGE_KEY).and_then(Value::as_str) == Some(key) {
        return MessageLeft::Before;
    }
    engine.insert(
        METADATA_RESUME_MESSAGE_KEY.to_string(),
        Value::String(key.to_string()),
    );
    let Some(session) = session else {
        return MessageLeft::AsNote;
    };
    put_agent_input(
        execution,
        state_id,
        text,
        Some(&session.harness),
        Some(&session.harness_session_id),
        Vec::new(),
        PromptAuthor::User,
        restart_if_refused,
    );
    MessageLeft::InSession
}

/// Puts an answer for the agent of `state_id` in `execution`, without
/// saving it.
#[allow(clippy::too_many_arguments)]
fn put_agent_input(
    execution: &mut WorkflowExecution,
    state_id: &str,
    answer: &str,
    harness: Option<&str>,
    session: Option<&str>,
    permission_rules: Vec<String>,
    author: PromptAuthor,
    restart_if_refused: bool,
) {
    let mut record = json!({
        "state_id": state_id,
        "answer": answer,
        "harness": harness,
        "session": session,
        "permission_rules": permission_rules,
        "author": author,
    });
    if restart_if_refused {
        record["restart_if_refused"] = Value::Bool(true);
    }
    engine_metadata_mut(execution).insert(METADATA_AGENT_INPUT_KEY.to_string(), record);
}

impl Engine {
    /// Stores an answer for one agent state needing input. It lives in engine metadata
    /// rather than workflow context: it is transient control data, not a
    /// variable a later workflow step should inherit.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn store_agent_input(
        &self,
        execution: &mut WorkflowExecution,
        state_id: &str,
        answer: &str,
        harness: Option<&str>,
        session: Option<&str>,
        permission_rules: Vec<String>,
        author: PromptAuthor,
    ) -> Result<(), EngineError> {
        put_agent_input(
            execution,
            state_id,
            answer,
            harness,
            session,
            permission_rules,
            author,
            false,
        );
        self.store.save_execution(execution)?;
        Ok(())
    }
}
