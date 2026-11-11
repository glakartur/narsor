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
}

/// The author of the prompt that carries an answer back to the agent.
pub(crate) fn prompt_author(answered_by: AnsweredBy) -> PromptAuthor {
    match answered_by {
        AnsweredBy::Operator => PromptAuthor::User,
        AnsweredBy::Conductor => PromptAuthor::Conductor,
    }
}

impl Engine {
    /// Stores an answer for one agent state needing input. It lives in engine metadata
    /// rather than workflow context: it is transient control data, not a
    /// variable a later workflow step should inherit.
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
        engine_metadata_mut(execution).insert(
            METADATA_AGENT_INPUT_KEY.to_string(),
            json!({
                "state_id": state_id,
                "answer": answer,
                "harness": harness,
                "session": session,
                "permission_rules": permission_rules,
                "author": author,
            }),
        );
        self.store.save_execution(execution)?;
        Ok(())
    }
}
