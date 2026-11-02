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
//! 3. **Record the session.** Every run creates an [`AgentSession`] and emits
//!    `AgentSessionStarted` before it and `AgentSessionCompleted` /
//!    `AgentSessionFailed` after it, with the harness's own session id captured
//!    from its output (D18).
//!
//! # Why the executor emits events itself
//!
//! Everything else an executor produces is described by its return value, but a
//! harness session is a thing that exists *during* the run: it starts before
//! there is a result and it can fail without one. So the executor is given an
//! [`AgentEventSink`] — the engine wires in [`LoggingEventSink`], which appends
//! to the execution's log and republishes on the live channel, exactly as the
//! engine's own `emit` does. Without a sink (a direct unit test) the run is
//! unchanged and the events simply go nowhere.

use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use narsor_protocol::chat::{ChatElement, UnclassifiedReason};
use narsor_protocol::controller::{HarnessUsageReport, UsageLimit};
use serde_json::{Map, Value};
use std::process::ExitStatus;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::config::{agent_args_for, AgentArgs};

use super::answer::{self, ChoiceAnswer};
use super::harness::{
    apply_extra_args, FinalAnswer, HarnessAdapter, HarnessRegistry, HarnessRequest, HarnessRun,
    HarnessStreamParser,
};
use super::human::{HumanInputRequest, PermissionDenialDetails, PermissionRequestDetails};
use super::process_group::{self, GroupGuard};
use super::script::{truncate, MAX_BUFFERED_OUTPUT_BYTES};
use super::{
    ActionExecutor, ActionOutcome, ActionRequest, StepContract, SuspendedAction, TimedOutAction,
    ACTION_ID_ENV_VAR,
};
use crate::domain::{
    ActionConfig, ActionError, ActionResult, AgentActionConfig, AgentSession, AgentSessionStatus,
    ChoiceDefinition, Event, EventData, WorkspaceAccess,
};
use crate::persistence::{
    ConversationSource, ConversationStore, ConversationWriter, ExecutionStore,
};

mod contract;
pub(crate) mod conversation;
pub(crate) mod process;
mod response;

pub use contract::{
    conductor_reprompt_config, continuation_config, output_contract, reprompt_config,
    DefaultHarnessSource, BUILT_IN_HARNESS, CONDUCTOR_REPROMPT_GUARD, MISSING_OUTPUT_LABEL,
    OUTPUT_REPROMPT, PARSE_LINE_LIMIT, RESUME_SESSION_PARAMETER, RESUME_SESSION_REJECTED,
    SESSION_DATA_KEY, SESSION_ID_VARIABLE,
};
use contract::{new_session, resume_session, AWAITING_INPUT};
pub use conversation::{AgentEventSink, LoggingEventSink, UsageReporter};
use conversation::{ConversationSink, UsageRelay};
pub(crate) use process::stderr_element;
use process::{harness_error, run_harness_invocation, FINAL_ANSWER_GRACE};
pub(crate) use process::{run_harness_process, FinishedRun, SpawnEnding};
pub use response::extract_output;
#[cfg(test)]
pub(crate) use response::run_reply;
use response::{
    agent_input_fields, build_result, read_response, record_question, record_reply_in_details,
    record_session_in_details, AgentResponse,
};
pub(crate) use response::{closing_answer, final_answer_kind, json_objects};

/// Executes `AgentAction` states.
///
/// Holds the harness registry, so pointing `claude-code` at a different binary
/// — a wrapper, an absolute path, the test suite's fake harness — is a matter
/// of building the executor with another registry.
#[derive(Debug, Clone)]
pub struct AgentExecutor {
    harnesses: HarnessRegistry,
    events: Option<Arc<dyn AgentEventSink>>,
    usage: Option<Arc<dyn UsageReporter>>,
    /// How long a harness may go on after its final answer.
    final_answer_grace: Duration,
    /// Extra arguments of every run, winning over the adapter's own.
    extra_args: AgentArgs,
}

impl AgentExecutor {
    /// An executor with the stock harnesses, no event sink and no usage
    /// reporter.
    pub fn new() -> Self {
        Self {
            harnesses: HarnessRegistry::defaults(),
            events: None,
            usage: None,
            final_answer_grace: FINAL_ANSWER_GRACE,
            extra_args: AgentArgs::new(),
        }
    }

    /// The same executor, waiting `grace` after a final answer before it ends
    /// the harness's process group.
    pub fn with_final_answer_grace(mut self, grace: Duration) -> Self {
        self.final_answer_grace = grace;
        self
    }

    /// The same executor, adding `args` to every harness run; on a conflict
    /// they replace the arguments the adapter decided on.
    pub fn with_extra_args(mut self, args: AgentArgs) -> Self {
        self.extra_args = args;
        self
    }

    /// An executor with a registry of its own.
    pub fn with_harnesses(harnesses: HarnessRegistry) -> Self {
        Self {
            harnesses,
            events: None,
            usage: None,
            final_answer_grace: FINAL_ANSWER_GRACE,
            extra_args: AgentArgs::new(),
        }
    }

    /// Returns the executor with the session events routed to `sink`.
    pub fn with_event_sink(mut self, sink: Arc<dyn AgentEventSink>) -> Self {
        self.events = Some(sink);
        self
    }

    /// Returns the executor with the usage limits its harnesses report
    /// routed to `reporter`.
    pub fn with_usage_reporter(mut self, reporter: Arc<dyn UsageReporter>) -> Self {
        self.usage = Some(reporter);
        self
    }

    /// The harnesses this executor can run.
    pub fn harnesses(&self) -> &HarnessRegistry {
        &self.harnesses
    }

    /// Emits an event for `execution_id`, when both are there to emit with.
    fn emit(&self, execution_id: Option<&str>, data: EventData) {
        if let (Some(sink), Some(execution_id)) = (&self.events, execution_id) {
            sink.emit(execution_id, data);
        }
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
    /// Opens this attempt's conversation file, if there is a store to open it
    /// in, and writes the prompt entry — the first line any conversation ever
    /// has: the prompt sent, the harness, and the session it continues, if
    /// any — with a [`ChatElement::Prompt`] carrying the full prompt.
    fn open_conversation(
        &self,
        request: &ActionRequest<'_>,
        harness: &str,
        resume: Option<&str>,
        prompt: &str,
    ) -> ConversationSink {
        let Some(store) = request.conversation_store else {
            return ConversationSink::none();
        };
        let sink = ConversationSink::open(store, request.action_execution_id);
        let entry = serde_json::json!({
            "event": "prompt",
            "harness": harness,
            "resumed_session": resume,
            "prompt": prompt,
        })
        .to_string();
        let element = ChatElement::Prompt {
            text: prompt.to_string(),
            resumed_session: resume.map(str::to_string),
            purpose: request.purpose,
        };
        sink.append_narsor(&entry, &[element]);
        sink
    }

    /// One agent run, from the session record to the output label.
    async fn run(&self, request: ActionRequest<'_>) -> Result<ActionOutcome, ActionError> {
        let config = agent_config(request.action)?;
        let resume = resume_session(config)?;
        if resume.is_some() && config.harness.is_none() {
            return Err(ActionError {
                message: format!(
                    "parameter '{RESUME_SESSION_PARAMETER}' requires an explicit 'harness'"
                ),
                error_type: Some("invalid_parameter".to_string()),
                details: None,
            });
        }

        // The engine makes every attempt explicit, pinning its default
        // harness once per action (R16a); only a caller other than the
        // engine hands over one without, and gets the built-in.
        let harness = config.harness.as_deref().unwrap_or(BUILT_IN_HARNESS);
        let adapter = self.harnesses.get(harness)?;

        let mut session = new_session(config, harness, request.execution_id);

        let prompt = format!(
            "{}{}",
            config.prompt,
            output_contract(&request.contract, adapter.contract_note())
        );
        let workspace = Path::new(&request.workspace.path);

        // Opened before `invocation` (Key Technical Decisions): even an
        // invocation that never runs (a bad parameter, a missing binary)
        // leaves a conversation file behind, with the prompt and why the
        // harness never started.
        let conversation = self.open_conversation(&request, harness, resume, &prompt);

        let invocation = match adapter.invocation(&HarnessRequest {
            config,
            workspace_access: config.workspace_access,
            prompt: &prompt,
            workspace,
            resume_session: resume,
            output_schema: None,
        }) {
            Ok(mut invocation) => {
                apply_extra_args(
                    &mut invocation,
                    &prompt,
                    agent_args_for(&self.extra_args, harness),
                );
                invocation
            }
            Err(error) => {
                conversation.append_outcome(
                    &format!("harness did not start: {}", error.message),
                    serde_json::json!({ "error_type": error.error_type }),
                );
                conversation.finish();
                return Err(error);
            }
        };

        self.emit(
            request.execution_id,
            EventData::AgentSessionStarted {
                session_id: session.id.clone(),
                // The harness that actually ran, never "unspecified".
                harness: harness.to_string(),
                action_execution_id: Some(request.action_execution_id.to_string()),
                resumed_session: resume.map(ToString::to_string),
            },
        );
        session.status = AgentSessionStatus::Running;

        let usage = self.usage.as_deref().map(|reporter| UsageRelay {
            harness: adapter.name(),
            reporter,
        });
        let ending = run_harness_invocation(
            &invocation.program,
            &invocation.args,
            invocation.stdin.as_deref(),
            workspace,
            Some(request.action_execution_id),
            request.timeout,
            self.final_answer_grace,
            adapter.stream_parser(),
            &conversation,
            usage,
        )
        .await;

        let finished = match ending {
            Ok(SpawnEnding::Finished(finished)) => {
                if let Some(group) = finished.terminated_group {
                    conversation.append_outcome(
                        &format!("agent process group {group} terminated after the final answer"),
                        serde_json::json!({ "process_group": group, "reason": "final_answer" }),
                    );
                }
                conversation.append_outcome(
                    &format!("harness exited with code {}", finished.exit_code),
                    serde_json::json!({ "exit_code": finished.exit_code }),
                );
                finished
            }
            Ok(SpawnEnding::TimedOut) => {
                let limit = request.timeout.unwrap_or_default();
                conversation.append_outcome(
                    &format!(
                        "harness timed out after {}s and was killed",
                        limit.as_secs_f64()
                    ),
                    serde_json::json!({ "timed_out_after_secs": limit.as_secs_f64() }),
                );
                conversation.finish();

                self.fail_session(
                    request.execution_id,
                    request.action_execution_id,
                    &mut session,
                    None,
                    format!(
                        "the harness was still running after {}s and was killed",
                        limit.as_secs_f64()
                    ),
                );

                return Ok(ActionOutcome::TimedOut(TimedOutAction {
                    timeout: limit,
                    subject: format!("agent harness '{}'", adapter.name()),
                }));
            }
            Err(error) => {
                let started = !matches!(
                    error.error_type.as_deref(),
                    Some("harness_not_found") | Some("spawn_failed")
                );
                conversation.append_outcome(
                    &if started {
                        format!("harness run failed: {}", error.message)
                    } else {
                        format!("harness did not start: {}", error.message)
                    },
                    serde_json::json!({ "error_type": error.error_type }),
                );
                conversation.finish();

                self.fail_session(
                    request.execution_id,
                    request.action_execution_id,
                    &mut session,
                    None,
                    error.message.clone(),
                );
                return Err(error);
            }
        };

        let run = finished.run.clone();

        if let Some(session_id) = &run.session_id {
            session
                .config
                .as_object_mut()
                .expect("the session config is built as an object")
                .insert(
                    "harness_session_id".to_string(),
                    Value::String(session_id.clone()),
                );
        }

        if !run.text.trim().is_empty() {
            self.emit(
                request.execution_id,
                EventData::AgentMessage {
                    session_id: session.id.clone(),
                    content: truncate(&run.text),
                    role: "assistant".to_string(),
                    action_execution_id: Some(request.action_execution_id.to_string()),
                },
            );
        }

        if !run.permission_denials.is_empty() {
            conversation.finish();
            session.status = AgentSessionStatus::Completed;
            session.ended_at = Some(Utc::now());
            self.emit(
                request.execution_id,
                EventData::AgentSessionCompleted {
                    session_id: session.id.clone(),
                    action_execution_id: Some(request.action_execution_id.to_string()),
                    harness_session_id: run.session_id.clone(),
                },
            );
            let cwd = request.workspace.path.clone();
            return Ok(ActionOutcome::Suspended(
                SuspendedAction::AwaitingAgentInput(HumanInputRequest {
                    action_execution_id: request.action_execution_id.to_string(),
                    instruction: format!(
                        "{} needs a permission decision before this task can continue.",
                        narsor_protocol::harness::display_name(harness)
                    ),
                    fields: Vec::new(),
                    reason: None,
                    required_values: Vec::new(),
                    answer_tail: Some(run.text.clone()),
                    agent_harness: Some(harness.to_string()),
                    agent_session_id: run.session_id.clone(),
                    prefilled_answer: None,
                    permission: Some(PermissionRequestDetails {
                        denials: run
                            .permission_denials
                            .iter()
                            .map(|denial| PermissionDenialDetails {
                                tool_name: denial.tool_name.clone(),
                                tool_use_id: denial.tool_use_id.clone(),
                                input: denial.input.clone(),
                                cwd: cwd.clone(),
                                reason_type: denial.reason_type.clone(),
                                reason: denial.reason.clone(),
                            })
                            .collect(),
                    }),
                }),
            ));
        }

        if let Some(failure) = &run.failure {
            conversation.finish();
            self.fail_session(
                request.execution_id,
                request.action_execution_id,
                &mut session,
                run.session_id.clone(),
                failure.clone(),
            );
            return Err(harness_error(
                adapter.as_ref(),
                failure,
                &finished,
                &run,
                resume,
            ));
        }

        let output = match read_response(&run) {
            Ok(output) => {
                record_question(&conversation, &output);
                conversation.finish();
                output
            }
            Err(mut error) => {
                conversation.finish();
                // What the engine needs to ask this same session again
                // (KTD7): the harness it ran on and the session it left.
                record_session_in_details(&mut error, harness, run.session_id.as_deref());
                record_reply_in_details(&mut error, &run);
                self.fail_session(
                    request.execution_id,
                    request.action_execution_id,
                    &mut session,
                    run.session_id.clone(),
                    error.message.clone(),
                );
                return Err(error);
            }
        };

        session.status = AgentSessionStatus::Completed;
        session.ended_at = Some(Utc::now());
        self.emit(
            request.execution_id,
            EventData::AgentSessionCompleted {
                session_id: session.id.clone(),
                action_execution_id: Some(request.action_execution_id.to_string()),
                harness_session_id: run.session_id.clone(),
            },
        );

        match output {
            AgentResponse::Completed(output) => Ok(ActionOutcome::Completed(build_result(
                output, &session, &run, &finished,
            ))),
            AgentResponse::AwaitingInput { question, options } => Ok(ActionOutcome::Suspended(
                SuspendedAction::AwaitingAgentInput(HumanInputRequest {
                    action_execution_id: request.action_execution_id.to_string(),
                    instruction: question,
                    fields: agent_input_fields(options),
                    reason: None,
                    required_values: Vec::new(),
                    answer_tail: None,
                    agent_harness: Some(harness.to_string()),
                    agent_session_id: run.session_id.clone(),
                    prefilled_answer: None,
                    permission: None,
                }),
            )),
        }
    }

    /// Marks the session failed and says so in the log.
    fn fail_session(
        &self,
        execution_id: Option<&str>,
        action_execution_id: &str,
        session: &mut AgentSession,
        harness_session_id: Option<String>,
        error: String,
    ) {
        session.status = AgentSessionStatus::Failed;
        session.ended_at = Some(Utc::now());
        self.emit(
            execution_id,
            EventData::AgentSessionFailed {
                session_id: session.id.clone(),
                error,
                action_execution_id: Some(action_execution_id.to_string()),
                harness_session_id,
            },
        );
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
    }
}
