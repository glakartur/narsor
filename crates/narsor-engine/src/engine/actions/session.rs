//! A harness session: one run of an AI coding agent on a ready prompt,
//! through one of the harness adapters in [`super::harness`] (D17, D18).
//!
//! Shared by every action that talks to an agent through a harness. The
//! executor builds the prompt before the session and reads the answer after
//! it; everything in between happens here, and only here:
//!
//! * the [`AgentSession`] record, with `AgentSessionStarted` (the model and
//!   the effort that ran) before the run and `AgentSessionCompleted` /
//!   `AgentSessionFailed` after it, carrying the harness's own session id
//!   captured from its output;
//! * the warning about an effort the harness does not take;
//! * the attempt's conversation file ([`conversation`]);
//! * the harness process ([`process`]): spawned once, its process group
//!   ended after the final answer and on the time limit;
//! * the permission denials that stop the step ([`denials`]).
//!
//! [`response`] holds the bounded scanners an executor reads the answer
//! with: the agent's text is untrusted.
//!
//! # Why the session emits events itself
//!
//! Everything else an executor produces is described by its return value, but a
//! harness session is a thing that exists *during* the run: it starts before
//! there is a result and it can fail without one. So the session is given an
//! [`AgentEventSink`] — the engine wires in [`LoggingEventSink`], which appends
//! to the execution's log and republishes on the live channel, exactly as the
//! engine's own `emit` does. Without a sink (a direct unit test) the run is
//! unchanged and the events simply go nowhere.

use std::io;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use narsor_protocol::chat::{ChatElement, PromptAuthor, UnclassifiedReason};
use narsor_protocol::controller::{HarnessUsageReport, UsageLimit};
use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::config::{agent_args_for, AgentArgs};
use crate::engine::timeline_file::ReadingSink;

use super::answer::AnswerShape;
use super::harness::{
    apply_extra_args, FinalAnswer, HarnessAdapter, HarnessRegistry, HarnessRequest, HarnessRun,
    HarnessStreamParser,
};
use super::human::{HumanInputRequest, PermissionDenialDetails, PermissionRequestDetails};
use super::process_group::{self, GroupGuard};
use super::scope::StepScope;
use super::script::{truncate, MAX_BUFFERED_OUTPUT_BYTES};
use super::{ActionOutcome, ActionRequest, SuspendedAction, TimedOutAction, ACTION_ID_ENV_VAR};
use crate::domain::{
    ActionError, AgentSession, AgentSessionStatus, Event, EventData, HarnessStepConfig,
};
use crate::persistence::{
    ConversationSource, ConversationStore, ConversationWriter, ExecutionStore,
};

pub(crate) mod conversation;
mod denials;
pub(crate) mod process;
pub(crate) mod response;

use conversation::UsageRelay;
pub use conversation::{AgentEventSink, LoggingEventSink, UsageReporter};
pub(crate) use conversation::{ConversationSink, ReadingTap};
use denials::split_denials;
use process::{harness_error, run_harness_invocation, FINAL_ANSWER_GRACE};
pub(crate) use process::{run_harness_process, stderr_element, FinishedRun, SpawnEnding};

/// The harness used when neither the action nor the configuration names one.
///
/// The name the adapter itself registers under, not a second spelling of it:
/// a default that `HarnessRegistry::get` does not know would fail at run time
/// rather than at compile time.
pub const BUILT_IN_HARNESS: &str = super::harness::claude_code::CLAUDE_CODE;

/// The `parameters` key that continues an existing harness session (D18).
pub const RESUME_SESSION_PARAMETER: &str = "resume_session";

/// The harness refused a saved session id. The runner turns this into a new
/// operator choice instead of retrying it or discarding the drafted message.
pub const RESUME_SESSION_REJECTED: &str = "resume_session_rejected";

/// The most a single stdout line is handed to the harness's stream parser.
///
/// Every line reaches the conversation file in full regardless (R14) — this
/// only bounds what is fed to [`HarnessStreamParser::on_stdout_line`]. Tens of
/// megabytes is well past anything a harness's own JSON envelope needs (a
/// multi-megabyte `tool_result` line is ordinary; the final `result`/`text`
/// line is not), so a line past this limit is bulky tool output, not the
/// answer, and is safely skipped for parsing.
pub const PARSE_LINE_LIMIT: usize = 32 * 1024 * 1024;

/// Runs harness sessions.
///
/// Holds the harness registry, so pointing `claude-code` at a different binary
/// — a wrapper, an absolute path, the test suite's fake harness — is a matter
/// of building the runner with another registry.
#[derive(Debug, Clone)]
pub(crate) struct SessionRunner {
    harnesses: HarnessRegistry,
    events: Option<Arc<dyn AgentEventSink>>,
    usage: Option<Arc<dyn UsageReporter>>,
    readings: Option<Arc<dyn ReadingSink>>,
    /// How long a harness may go on after its final answer.
    final_answer_grace: Duration,
    /// Extra arguments of every run, winning over the adapter's own.
    extra_args: AgentArgs,
}

/// The harness a session runs on, picked before its prompt is built: the
/// prompt closes with the harness's own [`HarnessAdapter::contract_note`].
pub(crate) struct SessionHarness {
    name: String,
    adapter: Arc<dyn HarnessAdapter>,
}

impl SessionHarness {
    /// How the harness wants the answer delivered, for the prompt.
    pub(crate) fn contract_note(&self) -> Option<&str> {
        self.adapter.contract_note()
    }
}

/// What a session is started with.
pub(crate) struct SessionStart<'r> {
    pub(crate) harness: SessionHarness,
    pub(crate) config: &'r HarnessStepConfig,
    /// The directories the harness works in and may read and write.
    pub(crate) scope: &'r StepScope,
    /// The harness session this one continues, from [`resume_session`].
    pub(crate) resume: Option<&'r str>,
    /// The full prompt, output contract included.
    pub(crate) prompt: String,
    /// What makes an object the final answer, which ends the run early and
    /// decides whether a permission denial stops the step.
    pub(crate) shape: AnswerShape,
}

/// How a session ended.
pub(crate) enum SessionEnd<'s> {
    /// The harness finished; its answer is the executor's to read.
    Answered(OpenSession<'s>),
    /// The session ended without an answer to read: the time limit ran out,
    /// or a permission denial needs an operator first.
    Stopped(ActionOutcome),
}

/// A session whose harness finished, still open until the executor has read
/// its answer and either completed it or rejected the answer.
pub(crate) struct OpenSession<'s> {
    runner: &'s SessionRunner,
    execution_id: Option<String>,
    action_execution_id: String,
    record: AgentSession,
    finished: FinishedRun,
    conversation: ConversationSink,
}

/// A completed session: its record and the run that completed it.
pub(crate) struct CompletedSession {
    pub(crate) record: AgentSession,
    pub(crate) finished: FinishedRun,
}

impl SessionRunner {
    /// A runner on `harnesses`, with no event sink and no usage reporter.
    pub(crate) fn new(harnesses: HarnessRegistry) -> Self {
        Self {
            harnesses,
            events: None,
            usage: None,
            readings: None,
            final_answer_grace: FINAL_ANSWER_GRACE,
            extra_args: AgentArgs::new(),
        }
    }

    /// Waits `grace` after a final answer before it ends the harness's
    /// process group.
    pub(crate) fn set_final_answer_grace(&mut self, grace: Duration) {
        self.final_answer_grace = grace;
    }

    /// Adds `args` to every harness run; on a conflict they replace the
    /// arguments the adapter decided on.
    pub(crate) fn set_extra_args(&mut self, args: AgentArgs) {
        self.extra_args = args;
    }

    /// Routes the session events to `sink`.
    pub(crate) fn set_event_sink(&mut self, sink: Arc<dyn AgentEventSink>) {
        self.events = Some(sink);
    }

    /// Routes the usage limits the harnesses report to `reporter`.
    pub(crate) fn set_usage_reporter(&mut self, reporter: Arc<dyn UsageReporter>) {
        self.usage = Some(reporter);
    }

    /// Hands the tokens and cost the harnesses report to `sink`.
    pub(crate) fn set_reading_sink(&mut self, sink: Arc<dyn ReadingSink>) {
        self.readings = Some(sink);
    }

    /// The harnesses this runner can run.
    pub(crate) fn harnesses(&self) -> &HarnessRegistry {
        &self.harnesses
    }

    /// The harness `config` runs on.
    ///
    /// The engine makes every attempt explicit, pinning its default harness
    /// once per action (R16a); only a caller other than the engine hands
    /// over one without, and gets the built-in.
    pub(crate) fn harness(
        &self,
        config: &HarnessStepConfig,
    ) -> Result<SessionHarness, ActionError> {
        let name = config.harness.as_deref().unwrap_or(BUILT_IN_HARNESS);
        let adapter = Arc::clone(self.harnesses.get(name)?);
        Ok(SessionHarness {
            name: name.to_string(),
            adapter,
        })
    }

    /// Runs one harness session, from the session record to the finished
    /// process.
    pub(crate) async fn run(
        &self,
        request: &ActionRequest<'_>,
        start: SessionStart<'_>,
    ) -> Result<SessionEnd<'_>, ActionError> {
        let SessionStart {
            harness:
                SessionHarness {
                    name: harness,
                    adapter,
                },
            config,
            scope,
            resume,
            prompt,
            shape,
        } = start;

        if config.effort.is_some() && !adapter.supports_effort() {
            self.emit(
                request.execution_id,
                EventData::Error {
                    message: format!(
                        "Harness '{harness}' does not take an effort; the step's effort is ignored"
                    ),
                    context: serde_json::json!({ "effort_ignored": harness }),
                },
            );
        }

        let mut session = new_session(config, &harness, request.execution_id);

        let workspace = scope.start_dir.as_path();
        let writable_dirs = scope.write_dirs();
        let read_only_dirs = scope.read_dirs();

        // Opened before `invocation` (Key Technical Decisions): even an
        // invocation that never runs (a bad parameter, a missing binary)
        // leaves a conversation file behind, with the prompt and why the
        // harness never started.
        let conversation = self.open_conversation(request, &harness, resume, &prompt);

        let invocation = match adapter.invocation(&HarnessRequest {
            config,
            workspace_access: config.workspace_access,
            prompt: &prompt,
            workspace,
            writable_dirs: &writable_dirs,
            read_only_dirs: &read_only_dirs,
            resume_session: resume,
            output_schema: None,
        }) {
            Ok(mut invocation) => {
                apply_extra_args(
                    &mut invocation,
                    &prompt,
                    agent_args_for(&self.extra_args, &harness),
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
                harness: harness.clone(),
                action_execution_id: Some(request.action_execution_id.to_string()),
                resumed_session: resume.map(ToString::to_string),
                model: flag_value(&invocation.args, &["--model", "-m"])
                    .or_else(|| config.model.clone()),
                effort: flag_value(&invocation.args, &["--effort", "--reasoning-effort"]),
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
            adapter.stream_parser(shape),
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
                if finished.exit_code != 0 {
                    conversation.append_outcome(
                        &format!("harness exited with code {}", finished.exit_code),
                        serde_json::json!({ "exit_code": finished.exit_code }),
                    );
                }
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

                return Ok(SessionEnd::Stopped(ActionOutcome::TimedOut(
                    TimedOutAction {
                        timeout: limit,
                        subject: format!("agent harness '{}'", adapter.name()),
                    },
                )));
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

        let run = &finished.run;

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

        let denials = split_denials(run, shape);
        for denial in &denials.ignored {
            self.emit(
                request.execution_id,
                EventData::PermissionDeniedIgnored {
                    session_id: session.id.clone(),
                    action_execution_id: Some(request.action_execution_id.to_string()),
                    tool_name: denial.tool_name.clone(),
                    tool_use_id: denial.tool_use_id.clone(),
                    summary: denial.summary.clone(),
                    reason: denial.reason.clone(),
                    source: denial.subagent.clone(),
                },
            );
        }

        let permission = (!denials.blocking.is_empty()).then(|| {
            let cwd = scope.start_dir.to_string_lossy().into_owned();
            HumanInputRequest {
                action_execution_id: request.action_execution_id.to_string(),
                instruction: format!(
                    "{} needs a permission decision before this task can continue.",
                    narsor_protocol::harness::display_name(&harness)
                ),
                fields: Vec::new(),
                reason: None,
                required_values: Vec::new(),
                answer_tail: Some(run.text.clone()),
                agent_harness: Some(harness.clone()),
                agent_session_id: run.session_id.clone(),
                prefilled_answer: None,
                permission: Some(PermissionRequestDetails {
                    denials: denials
                        .blocking
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
            }
        });
        let failure = run.failure.as_ref().map(|failure| {
            (
                failure.clone(),
                harness_error(adapter.as_ref(), failure, &finished, run, resume),
            )
        });

        let open = OpenSession {
            runner: self,
            execution_id: request.execution_id.map(str::to_string),
            action_execution_id: request.action_execution_id.to_string(),
            record: session,
            finished,
            conversation,
        };

        if let Some(permission) = permission {
            open.complete();
            return Ok(SessionEnd::Stopped(ActionOutcome::Suspended(
                SuspendedAction::AwaitingAgentInput(permission),
            )));
        }

        if let Some((failure, error)) = failure {
            open.fail(failure);
            return Err(error);
        }

        Ok(SessionEnd::Answered(open))
    }

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
            author: Some(request.author.unwrap_or(PromptAuthor::Engine)),
        };
        sink.append_narsor(&entry, &[element]);
        match (&self.readings, request.execution_id) {
            (Some(readings), Some(execution_id)) => sink.with_readings(ReadingTap {
                sink: Arc::clone(readings),
                execution_id: execution_id.to_string(),
                attempt_id: request.action_execution_id.to_string(),
                harness: harness.to_string(),
            }),
            _ => sink,
        }
    }

    /// Emits an event for `execution_id`, when both are there to emit with.
    fn emit(&self, execution_id: Option<&str>, data: EventData) {
        if let (Some(sink), Some(execution_id)) = (&self.events, execution_id) {
            sink.emit(execution_id, data);
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

impl OpenSession<'_> {
    /// What the harness distilled from its output: the answer to read.
    pub(crate) fn run(&self) -> &HarnessRun {
        &self.finished.run
    }

    /// The attempt's conversation, for what the executor records of the
    /// answer it accepted.
    pub(crate) fn conversation(&self) -> &ConversationSink {
        &self.conversation
    }

    /// The answer was accepted: closes the conversation and marks the
    /// session completed.
    pub(crate) fn complete(self) -> CompletedSession {
        let Self {
            runner,
            execution_id,
            action_execution_id,
            mut record,
            finished,
            conversation,
        } = self;
        conversation.finish();
        record.status = AgentSessionStatus::Completed;
        record.ended_at = Some(Utc::now());
        runner.emit(
            execution_id.as_deref(),
            EventData::AgentSessionCompleted {
                session_id: record.id.clone(),
                action_execution_id: Some(action_execution_id),
                harness_session_id: finished.run.session_id.clone(),
            },
        );
        CompletedSession { record, finished }
    }

    /// The answer was rejected with `error`: closes the conversation, marks
    /// the session failed, and adds to the error's details what the engine
    /// needs to ask this same session again (KTD7): the harness it ran on and
    /// the session it left.
    pub(crate) fn reject(self, mut error: ActionError) -> ActionError {
        record_session_in_details(
            &mut error,
            &self.record.harness,
            self.finished.run.session_id.as_deref(),
        );
        self.fail(error.message.clone());
        error
    }

    /// Closes the conversation and marks the session failed with `reason`.
    fn fail(mut self, reason: String) {
        self.conversation.finish();
        self.runner.fail_session(
            self.execution_id.as_deref(),
            &self.action_execution_id,
            &mut self.record,
            self.finished.run.session_id.clone(),
            reason,
        );
    }
}

/// The harness session `config` continues, from its
/// [`RESUME_SESSION_PARAMETER`], if any. Continuing one needs an explicit
/// `harness`: a session belongs to the harness that started it.
pub(crate) fn resume_session(config: &HarnessStepConfig) -> Result<Option<&str>, ActionError> {
    let resume = match config.parameters.get(RESUME_SESSION_PARAMETER) {
        None | Some(Value::Null) => None,
        Some(Value::String(session)) if session.is_empty() => None,
        Some(Value::String(session)) => Some(session.as_str()),
        Some(other) => {
            return Err(ActionError {
                message: format!(
                "parameter '{RESUME_SESSION_PARAMETER}' must be a session id string, got {other}"
            ),
                error_type: Some("invalid_parameter".to_string()),
                details: None,
            })
        }
    };
    if resume.is_some() && config.harness.is_none() {
        return Err(ActionError {
            message: format!(
                "parameter '{RESUME_SESSION_PARAMETER}' requires an explicit 'harness'"
            ),
            error_type: Some("invalid_parameter".to_string()),
            details: None,
        });
    }
    Ok(resume)
}

fn new_session(
    config: &HarnessStepConfig,
    harness: &str,
    execution_id: Option<&str>,
) -> AgentSession {
    AgentSession {
        id: Uuid::new_v4().to_string(),
        workflow_execution_id: execution_id.unwrap_or_default().to_string(),
        harness: harness.to_string(),
        started_at: Utc::now(),
        ended_at: None,
        status: AgentSessionStatus::Starting,
        model: config.model.clone(),
        config: serde_json::json!({
            "harness": harness,
            "read_only_paths": config.read_only_paths,
            "parameters": config.parameters,
        }),
    }
}

/// Adds the harness and its session id to the details of an extraction
/// failure, so the engine can resume that session to ask for the label again.
fn record_session_in_details(error: &mut ActionError, harness: &str, session_id: Option<&str>) {
    let details = error
        .details
        .get_or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(details) = details {
        details.insert("harness".to_string(), Value::String(harness.to_string()));
        details.insert(
            "harness_session_id".to_string(),
            session_id.map_or(Value::Null, |id| Value::String(id.to_string())),
        );
    }
}

/// The value of the first of `flags` on a harness command line, written as
/// `--flag value` or `--flag=value`.
fn flag_value(args: &[String], flags: &[&str]) -> Option<String> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if let Some((name, value)) = arg.split_once('=') {
            if flags.contains(&name) && !value.is_empty() {
                return Some(value.to_string());
            }
        } else if flags.contains(&arg.as_str()) {
            return args.next().filter(|v| !v.starts_with('-')).cloned();
        }
    }
    None
}

#[cfg(test)]
mod flag_value_tests {
    use super::flag_value;

    fn args(args: &[&str]) -> Vec<String> {
        args.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_flag_gives_its_value_either_way() {
        let flags = ["--model", "-m"];
        assert_eq!(
            flag_value(&args(&["-p", "--model", "opus"]), &flags),
            Some("opus".into())
        );
        assert_eq!(
            flag_value(&args(&["--model=sonnet"]), &flags),
            Some("sonnet".into())
        );
        assert_eq!(flag_value(&args(&["--model"]), &flags), None);
        assert_eq!(flag_value(&args(&["--other", "x"]), &flags), None);
    }
}
