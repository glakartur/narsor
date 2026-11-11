//! `ProfileAction` executor: an agent proposes the model and the effort of
//! the workflow's other steps, and the engine checks the proposal.
//!
//! The harness session is the shared [`super::session`]'s, as for `agent`
//! (R14). Only two things are this executor's own:
//!
//! 1. **The prompt.** The author's prompt says where the work is described;
//!    the engine adds the steps the agent may profile — each with its
//!    description, its defaults and its ceiling — and the model tiers and
//!    the efforts ([`contract::data_block`], R2), then the answer contract
//!    ([`contract::answer_contract`]): a flat object with a list of entries
//!    and no `choice`, because the step has one way out.
//! 2. **The answer.** The object is read with the session's bounded
//!    scanners ([`answer::read_profile_answer`]). An answer without one is a
//!    format error: the engine asks again in the same session, then
//!    `retry_policy` applies, then the operator (R13). A single bad entry is
//!    skipped with a note ([`entries::check_profile`], R4, R6, R7).
//!
//! The checked entries travel in the action result
//! ([`STEP_PROFILE_DATA_KEY`] in [`ActionResult::data`]); the engine writes
//! them to `step-profile.json` before the step takes its transition, so a
//! step completed again after a restart writes the same file again. The
//! conversation gets one Narsor entry with what was asked, stored and
//! clamped for each step ([`report::record_profile`], R11), and the step's
//! reason says how many entries were stored and skipped.

use std::future::Future;
use std::pin::Pin;

use chrono::Utc;
use serde_json::Value;

use super::agent::{record_reply_in_details, run_reply, AgentExecutor, SESSION_DATA_KEY};
use super::answer::AnswerShape;
use super::harness::HarnessRegistry;
use super::script::truncate;
use super::session::{resume_session, SessionEnd, SessionRunner, SessionStart};
use super::{ActionExecutor, ActionOutcome, ActionRequest};
use crate::domain::{
    ActionConfig, ActionError, ActionResult, ProfileActionConfig, WorkspaceAccess, EXIT_LABEL,
};
use crate::persistence::StepProfileEntry;

mod answer;
mod candidates;
mod contract;
mod entries;
mod report;

pub use answer::{read_profile_answer, ProfileAnswer};
pub use candidates::{profile_candidates, ProfileCandidate};
use contract::{answer_contract, data_block};
pub use contract::{answer_schema, PROFILE_REPROMPT};
use entries::check_profile;
use report::summary;

/// The [`ActionResult::data`] key carrying the checked entries of a
/// `profile` step, as a list of [`StepProfileEntry`].
pub(crate) const STEP_PROFILE_DATA_KEY: &str = "step_profile";

/// The entries a `profile` step's `result` carries, for the engine to
/// store. `None` for a result without them — another action's, or one the
/// operator gave by hand, which leaves the stored profile alone.
pub(crate) fn step_profile_of(
    result: &ActionResult,
) -> Option<Result<Vec<StepProfileEntry>, serde_json::Error>> {
    let entries = result.data.get(STEP_PROFILE_DATA_KEY)?;
    Some(serde_json::from_value(entries.clone()))
}

/// Executes `ProfileAction` states.
#[derive(Debug, Clone)]
pub struct ProfileExecutor {
    sessions: SessionRunner,
}

impl ProfileExecutor {
    /// An executor with a registry of its own.
    pub fn with_harnesses(harnesses: HarnessRegistry) -> Self {
        Self {
            sessions: SessionRunner::new(harnesses),
        }
    }

    /// An executor running its sessions exactly as `agent` does: the same
    /// harnesses, extra arguments, event sink, usage reporter and reading
    /// sink.
    pub fn like(agent: &AgentExecutor) -> Self {
        Self {
            sessions: agent.sessions().clone(),
        }
    }
}

impl ActionExecutor for ProfileExecutor {
    fn execute<'a>(
        &'a self,
        request: ActionRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ActionOutcome, ActionError>> + Send + 'a>> {
        Box::pin(self.run(request))
    }
}

impl ProfileExecutor {
    /// One profiling run: the prompt, the harness session, and the checked
    /// profile read from its answer.
    async fn run(&self, request: ActionRequest<'_>) -> Result<ActionOutcome, ActionError> {
        let config = profile_config(request.action)?;
        let workflow = request.workflow.ok_or_else(|| ActionError {
            message: "a profile step needs the workflow it belongs to, to list its steps"
                .to_string(),
            error_type: Some("missing_workflow".to_string()),
            details: None,
        })?;
        let candidates = profile_candidates(workflow.definition);
        let resume = resume_session(&config.harness_step)?;
        let harness = self.sessions.harness(&config.harness_step)?;

        let mut scope = request.step_scope();
        if config.harness_step.workspace_access == Some(WorkspaceAccess::ReadOnly) {
            scope = scope.read_only();
        }
        // A resumed session was told about the repositories and the steps
        // when it began.
        let data = if resume.is_none() {
            format!("{}{}", scope.prompt_block(), data_block(&candidates))
        } else {
            String::new()
        };
        let prompt = format!(
            "{}{}{}",
            config.prompt,
            data,
            answer_contract(harness.contract_note())
        );

        let session = match self
            .sessions
            .run(
                &request,
                SessionStart {
                    harness,
                    config: &config.harness_step,
                    scope: &scope,
                    resume,
                    prompt,
                    shape: AnswerShape::Profile,
                },
            )
            .await?
        {
            SessionEnd::Answered(session) => session,
            SessionEnd::Stopped(outcome) => return Ok(outcome),
        };

        let answer = match read_profile_answer(session.run()) {
            Ok(answer) => answer,
            Err(mut error) => {
                record_reply_in_details(&mut error, session.run(), AnswerShape::Profile);
                return Err(session.reject(error));
            }
        };
        let checked = check_profile(&candidates, &answer, workflow.step_id, Utc::now());
        report::record_profile(session.conversation(), &checked);

        let completed = session.complete();
        let run = &completed.finished.run;
        let mut data = serde_json::Map::new();
        data.insert(
            SESSION_DATA_KEY.to_string(),
            serde_json::to_value(&completed.record).unwrap_or(Value::Null),
        );
        data.insert(
            STEP_PROFILE_DATA_KEY.to_string(),
            serde_json::to_value(&checked.stored).unwrap_or(Value::Null),
        );
        Ok(ActionOutcome::Completed(ActionResult {
            output_label: EXIT_LABEL.to_string(),
            reason: summary(&checked),
            data: Value::Object(data),
            exit_code: Some(completed.finished.exit_code),
            stdout: truncate(&run.text),
            stderr: truncate(&completed.finished.stderr),
            reply: run_reply(run, AnswerShape::Profile),
            ..ActionResult::default()
        }))
    }
}

/// Narrows the action configuration to a profile configuration.
fn profile_config(action: &ActionConfig) -> Result<&ProfileActionConfig, ActionError> {
    match action {
        ActionConfig::Profile(config) => Ok(config),
        _ => Err(ActionError {
            message: "ProfileExecutor received an action that is not a profile action".to_string(),
            error_type: Some("wrong_action_type".to_string()),
            details: None,
        }),
    }
}
