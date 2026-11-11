//! `AgentAction` executor (`docs/dev/workflow-schema.md` §3.3.A, decisions D17–D20).
//!
//! Runs an AI coding agent in the execution's workspace through one of the
//! harness adapters in [`super::harness`], and turns what the agent said into
//! an output label.
//!
//! # The three jobs
//!
//! 1. **Ask properly.** The state's prompt is sent with the
//!    [`output_contract`] appended, so the agent is told — in the same
//!    breath as the task — that it has to finish with a JSON object carrying
//!    `choice`, which labels (with their descriptions) it may use, and what
//!    the step's variables and artifacts are. D19
//!    calls this "forcing structure on the agent"; forcing it starts with
//!    saying it.
//! 2. **Read robustly, guess never.** No step passes an output schema to its
//!    harness: the answer is the JSON object that closes the agent's last
//!    message, read from its text. (A harness that reports a structured
//!    output anyway has it read first and checked by the same rules.) Agents
//!    wrap JSON in prose, in fences, or in both. [`extract_output`] takes the
//!    *last* JSON object anywhere in the answer that has a `choice` key, read by the parser scripts share
//!    ([`super::answer`]). When there is none, the action fails
//!    ([`ActionError`]) — it never falls back to `success`. In an unattended
//!    pipeline a guessed label is a silently wrong transition, which is a far
//!    worse outcome than a failure an operator can see. D20's tolerance for a
//!    hallucinating agent is served by *asking again*: the failure is a
//!    [`MISSING_OUTPUT_LABEL`] error carrying the harness and its session id,
//!    and the engine resumes that session once, read-only, with
//!    [`reprompt_config`] — outside the state's `retry_policy`, which stays
//!    for real failures (KTD7).
//! 3. **Leave the run to the session.** Spawning the harness, its time
//!    limit, the conversation file, the session record and its events and
//!    the permission denials are the shared [`super::session`]'s: this
//!    executor builds the prompt before it and reads the answer after it.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use narsor_protocol::chat::ChatElement;
use serde_json::{Map, Value};

use crate::config::AgentArgs;
use crate::engine::timeline_file::ReadingSink;

use super::answer::{self, AnswerShape, ChoiceAnswer};
use super::harness::{FinalAnswer, HarnessRegistry, HarnessRun};
use super::human::HumanInputRequest;
use super::script::truncate;
use super::session::{
    resume_session, ConversationSink, FinishedRun, SessionEnd, SessionRunner, SessionStart,
};
use super::{ActionExecutor, ActionOutcome, ActionRequest, StepContract, SuspendedAction};
use crate::domain::{
    ActionConfig, ActionError, ActionResult, AgentActionConfig, AgentSession, WorkspaceAccess,
};

mod contract;
mod response;

pub use super::session::{
    AgentEventSink, LoggingEventSink, UsageReporter, BUILT_IN_HARNESS, PARSE_LINE_LIMIT,
    RESUME_SESSION_PARAMETER, RESUME_SESSION_REJECTED,
};
use contract::AWAITING_INPUT;
pub use contract::{
    conductor_reprompt_config, continue_session, output_contract, reprompt_config,
    DefaultHarnessSource, CONDUCTOR_REPROMPT_GUARD, MISSING_OUTPUT_LABEL, OUTPUT_REPROMPT,
    SESSION_DATA_KEY, SESSION_ID_VARIABLE,
};
pub use response::extract_output;
#[cfg(test)]
pub(crate) use response::run_reply;
use response::{
    agent_input_fields, build_result, read_response, record_question, record_reply_in_details,
    AgentResponse,
};
pub(crate) use response::{closing_answer, final_answer_kind};

/// Executes `AgentAction` states.
///
/// Holds the [`SessionRunner`] its harness sessions run on, so pointing
/// `claude-code` at a different binary — a wrapper, an absolute path, the
/// test suite's fake harness — is a matter of building the executor with
/// another registry.
#[derive(Debug, Clone)]
pub struct AgentExecutor {
    sessions: SessionRunner,
}

impl AgentExecutor {
    /// An executor with the stock harnesses, no event sink and no usage
    /// reporter.
    pub fn new() -> Self {
        Self::with_harnesses(HarnessRegistry::defaults())
    }

    /// The same executor, waiting `grace` after a final answer before it ends
    /// the harness's process group.
    pub fn with_final_answer_grace(mut self, grace: Duration) -> Self {
        self.sessions.set_final_answer_grace(grace);
        self
    }

    /// The same executor, adding `args` to every harness run; on a conflict
    /// they replace the arguments the adapter decided on.
    pub fn with_extra_args(mut self, args: AgentArgs) -> Self {
        self.sessions.set_extra_args(args);
        self
    }

    /// An executor with a registry of its own.
    pub fn with_harnesses(harnesses: HarnessRegistry) -> Self {
        Self {
            sessions: SessionRunner::new(harnesses),
        }
    }

    /// Returns the executor with the session events routed to `sink`.
    pub fn with_event_sink(mut self, sink: Arc<dyn AgentEventSink>) -> Self {
        self.sessions.set_event_sink(sink);
        self
    }

    /// Returns the executor with the usage limits its harnesses report
    /// routed to `reporter`.
    pub fn with_usage_reporter(mut self, reporter: Arc<dyn UsageReporter>) -> Self {
        self.sessions.set_usage_reporter(reporter);
        self
    }

    /// Returns the executor with the tokens and cost its harnesses report
    /// handed to `sink`.
    pub fn with_reading_sink(mut self, sink: Arc<dyn ReadingSink>) -> Self {
        self.sessions.set_reading_sink(sink);
        self
    }

    /// The harnesses this executor can run.
    pub fn harnesses(&self) -> &HarnessRegistry {
        self.sessions.harnesses()
    }
}

impl Default for AgentExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl ActionExecutor for AgentExecutor {
    fn execute<'a>(
        &'a self,
        request: ActionRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ActionOutcome, ActionError>> + Send + 'a>> {
        Box::pin(self.run(request))
    }
}

impl AgentExecutor {
    /// One agent run: the prompt, the harness session, and the output label
    /// read from its answer.
    async fn run(&self, request: ActionRequest<'_>) -> Result<ActionOutcome, ActionError> {
        let config = agent_config(request.action)?;
        let shape = AnswerShape::of_step(request.contract.has_exit());
        let resume = resume_session(&config.harness_step)?;
        let harness = self.sessions.harness(&config.harness_step)?;

        let mut scope = request.step_scope();
        if config.harness_step.workspace_access == Some(WorkspaceAccess::ReadOnly) {
            scope = scope.read_only();
        }
        // A resumed session was told about the repositories when it began.
        let repositories = if resume.is_none() {
            scope.prompt_block()
        } else {
            String::new()
        };
        let prompt = format!(
            "{}{}{}",
            config.prompt,
            repositories,
            output_contract(&request.contract, harness.contract_note())
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
                    shape,
                },
            )
            .await?
        {
            SessionEnd::Answered(session) => session,
            SessionEnd::Stopped(outcome) => return Ok(outcome),
        };

        let output = match read_response(session.run(), shape) {
            Ok(output) => {
                record_question(session.conversation(), &output);
                output
            }
            Err(mut error) => {
                record_reply_in_details(&mut error, session.run(), shape);
                return Err(session.reject(error));
            }
        };
        let completed = session.complete();
        let run = &completed.finished.run;

        match output {
            AgentResponse::Completed(output) => Ok(ActionOutcome::Completed(build_result(
                output,
                &completed.record,
                run,
                &completed.finished,
                shape,
            ))),
            AgentResponse::AwaitingInput { question, options } => Ok(ActionOutcome::Suspended(
                SuspendedAction::AwaitingAgentInput(HumanInputRequest {
                    action_execution_id: request.action_execution_id.to_string(),
                    instruction: question,
                    fields: agent_input_fields(options),
                    reason: None,
                    required_values: Vec::new(),
                    answer_tail: None,
                    agent_harness: Some(completed.record.harness.clone()),
                    agent_session_id: run.session_id.clone(),
                    prefilled_answer: None,
                    permission: None,
                }),
            )),
        }
    }
}

/// Narrows the action configuration to an agent configuration.
fn agent_config(action: &ActionConfig) -> Result<&AgentActionConfig, ActionError> {
    match action {
        ActionConfig::Agent(config) => Ok(config),
        other => Err(ActionError {
            message: format!(
                "AgentExecutor received a {} action",
                action_type_name(other)
            ),
            error_type: Some("wrong_action_type".to_string()),
            details: None,
        }),
    }
}

/// The schema name of an action type, for error messages.
fn action_type_name(action: &ActionConfig) -> &'static str {
    match action {
        ActionConfig::Agent(_) => "agent",
        ActionConfig::Script(_) => "script",
        ActionConfig::Human(_) => "human",
        ActionConfig::Wait(_) => "wait",
        ActionConfig::Decision(_) => "decision",
        ActionConfig::Profile(_) => "profile",
    }
}
