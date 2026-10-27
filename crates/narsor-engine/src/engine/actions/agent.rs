//! `AgentAction` executor (`WORKFLOW_SCHEMA.md` §3.3.A, decisions D17–D20).
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
//! 2. **Read robustly, guess never.** A harness that enforces
//!    [`output_schema`] (Claude Code) reports the answer as a structured
//!    output, which is read first and checked by the same rules. Otherwise —
//!    or when it is unusable — the text is read. Agents wrap JSON in prose,
//!    in fences, or in both. [`extract_output`] takes the *last* JSON object anywhere in the
//!    answer that has a `choice` key, read by the parser scripts share
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
use narsor_protocol::chat::{
    summary_line, ChatElement, PromptPurpose, QuestionSource, TechnicalCategory,
    UnclassifiedReason, SUMMARY_MAX_CHARS,
};
use serde_json::{Map, Value};
use std::process::ExitStatus;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::broadcast;
use uuid::Uuid;

use super::answer::{self, ChoiceAnswer};
use super::harness::{
    FinalAnswer, HarnessAdapter, HarnessRegistry, HarnessRequest, HarnessRun, HarnessStreamParser,
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
    ConversationSource, ConversationStore, ConversationWriter, DefaultHarnessSelector,
    ExecutionStore,
};

/// Builds the output contract appended to every prompt, so the agent is told
/// the contract it has to meet.
///
/// Kept deliberately short and imperative: it competes for attention with the
/// state's own prompt, and a long preamble is the first thing a model drops.
///
/// Everything listed comes from the state itself (KTD1): its variables and
/// artifacts, each described once, then its choices in declaration order,
/// each with its description and with the variables and the artifacts it
/// requires (R10). The engine is therefore the single source of the list: a
/// prompt no longer has to repeat the labels for the agent to know them.
/// With no choices to list (a direct call with nothing wired up) the agent
/// is pointed at the label the task asks for instead.
///
/// `harness_note` is how the harness wants the answer delivered
/// ([`HarnessAdapter::contract_note`]); it closes the contract.
pub fn output_contract(contract: &StepContract<'_>, harness_note: Option<&str>) -> String {
    format!(
        "\n\n\
---\n\
When you are done, end your reply with a single JSON object on its own, and \
nothing after it:\n\
{{\"choice\": \"<label>\", \"reason\": \"<one line>\", \"variables\": {{}}, \
\"artifacts\": {{}}, \"additional_data\": {{}}, \"additional_artifacts\": []}}\n\
{definitions}\
{choices}\
`reason` is required: one line saying why you made this choice.\n\
`additional_data` (any object) and `additional_artifacts` (a list of paths) hold \
anything else worth keeping; the workflow does not interpret them. The other \
fields are optional unless your choice requires them. If, and only if, you need a \
person to answer a question before continuing this same task, return instead:\n\
{{\"status\":\"awaiting_input\",\"question\":\"<question>\",\"options\":[\"<optional choice>\"]}}\n\
Do not use `awaiting_input` for a harness, sandbox, permission, or tool error; \
those are action failures. Send the answer with `choice` exactly once, when all \
your subagents have returned their results. A decision you cannot make yourself (a trade-off, a \
change of behaviour, or a question a skill would put to the user) is a question \
for the person: ask it with `awaiting_input`, and do not use a question tool. Do \
not omit the JSON object: without it the workflow cannot continue.\
{note}",
        definitions = step_definitions(contract),
        choices = choice_list(contract.choices),
        note = harness_note
            .map(|note| format!("\n{note}"))
            .unwrap_or_default(),
    )
}

/// The step's variables and artifacts, each with what it is.
fn step_definitions(contract: &StepContract<'_>) -> String {
    let mut text = String::new();
    if !contract.variables.is_empty() {
        text.push_str("The step's variables (in `variables`, name → value):\n");
        for variable in contract.variables {
            text.push_str(&described_item(&variable.name, Some(&variable.description)));
        }
    }
    if !contract.artifacts.is_empty() {
        text.push_str("The step's artifacts (in `artifacts`, name → path):\n");
        for artifact in contract.artifacts {
            text.push_str(&described_item(&artifact.name, Some(&artifact.description)));
        }
    }
    text
}

/// The labels `choice` may take, each with what it requires.
fn choice_list(choices: &[ChoiceDefinition]) -> String {
    if choices.is_empty() {
        return "`choice` is required and must be the label the task asks for.\n".to_string();
    }
    let mut text = "`choice` is required and must be exactly one of these labels:\n".to_string();
    for choice in choices {
        text.push_str(&described_item(
            &choice.label,
            choice.description.as_deref(),
        ));
        text.push_str(&required_names("variables", &choice.requires_variables));
        text.push_str(&required_names("artifacts", &choice.requires_artifacts));
    }
    text
}

/// `- `name` — description`, or the bare name when the description is blank.
fn described_item(name: &str, description: Option<&str>) -> String {
    match description.map(str::trim) {
        Some(description) if !description.is_empty() => {
            format!("- `{name}` — {description}\n")
        }
        _ => format!("- `{name}`\n"),
    }
}

/// The line naming what a choice requires of one kind, empty for none.
fn required_names(kind: &str, names: &[String]) -> String {
    if names.is_empty() {
        return String::new();
    }
    let names: Vec<String> = names.iter().map(|name| format!("`{name}`")).collect();
    format!("  - Required {kind}: {}\n", names.join(", "))
}

/// Builds the JSON Schema of the answer [`output_contract`] describes, for a
/// harness that can enforce one (`claude-code` passes it as `--json-schema`).
///
/// One flat object with the optional fields of both answers — a `choice`
/// with its `reason`, `variables`, `artifacts`, `additional_data` and
/// `additional_artifacts`, or `status: awaiting_input` with a `question` and
/// `options` — and nothing required (KTD2). The two answers are not two
/// `anyOf` branches because the API refuses `anyOf`, `oneOf` and `allOf` at
/// the top level of a schema (measured on `claude` 2.1.302, see
/// `tests/fixtures/claude-code/README.md`). So the schema only narrows the
/// answer: which of the two it is, and whether it is complete — `reason`
/// and what a choice requires included — is checked by narsor itself, the
/// same way as an answer read from text.
///
/// `choice` is limited to the declared labels; with none declared, any
/// string is allowed, as the text contract then asks for the label the task
/// names.
pub fn output_schema(contract: &StepContract<'_>) -> Value {
    let mut choice = serde_json::json!({ "type": "string" });
    if !contract.choices.is_empty() {
        let labels: Vec<&str> = contract
            .choices
            .iter()
            .map(|choice| choice.label.as_str())
            .collect();
        choice["enum"] = serde_json::json!(labels);
    }

    serde_json::json!({
        "type": "object",
        "properties": {
            "choice": choice,
            "reason": { "type": "string" },
            "variables": { "type": "object" },
            "artifacts": { "type": "object", "additionalProperties": { "type": "string" } },
            "additional_data": { "type": "object" },
            "additional_artifacts": { "type": "array", "items": { "type": "string" } },
            "status": { "type": "string", "enum": [AWAITING_INPUT, PROGRESS] },
            "question": { "type": "string" },
            "message": { "type": "string" },
            "options": { "type": "array", "items": { "type": "string" } },
        },
        "additionalProperties": false,
    })
}

/// The `status` of an answer that asks a person before the task continues.
const AWAITING_INPUT: &str = "awaiting_input";

/// The `status` of a note that is not an answer: the step goes on after it.
const PROGRESS: &str = "progress";

/// The `error_type` of an answer no output label could be extracted from — a
/// format error the engine answers with a reprompt, not with `retry_policy`
/// (KTD7).
pub const MISSING_OUTPUT_LABEL: &str = "missing_output_label";

/// What the engine asks an agent whose answer carried no usable output label
/// (KTD7), in the same harness session, before the output contract.
///
/// Generic on purpose: the reprompt only repairs the format of work already
/// done, so it tells the agent not to carry on working. The reason the
/// previous answer was rejected is quoted after it.
pub const OUTPUT_REPROMPT: &str = "Your previous reply could not be used by the workflow, \
because it did not end with a valid output object. Do not continue the task and do not \
change any files: the work is already done. Reply only with the JSON object described \
below, choosing the label that describes the work you already did.";

/// The key of an [`MISSING_OUTPUT_LABEL`] error's `details` that says the run
/// ended on a `progress` note: the agent has not finished the step. The
/// executor resumes it at once (see [`AgentExecutor::run_until_answered`]);
/// the error never leaves it while the session can be resumed.
const PROGRESS_ONLY_DETAIL: &str = "progress_only";

/// What the engine says to an agent whose turn the harness closed on a
/// `progress` note (a harness may end the turn on any answer it is given).
pub const PROGRESS_CONTINUATION: &str = "Your previous turn ended on a progress note, so \
the harness closed it before the step was done. A progress note is not an answer. Continue \
the step from where you stopped. Send another progress note only if you must stop again. \
When the step is done, reply with the JSON object described below.";

/// The configuration of the engine's reprompt of an agent (KTD7).
///
/// Resumes `harness_session` on `harness` — the harness the rejected attempt
/// ran on, never the current default, which may have changed since — with
/// read-only workspace access, whatever the workflow declares: fixing the
/// format of an answer is not further work. Any `resume_session` of the
/// workflow's own is replaced by the session being repaired. `reason` says
/// why the previous answer was rejected.
pub fn reprompt_config(
    original: &AgentActionConfig,
    harness: &str,
    harness_session: &str,
    reason: &str,
) -> AgentActionConfig {
    let mut config = original.clone();
    config.harness = Some(harness.to_string());
    config.workspace_access = Some(WorkspaceAccess::ReadOnly);
    config.prompt = format!("{OUTPUT_REPROMPT}\n\nWhy it was rejected: {reason}");
    config.parameters.insert(
        RESUME_SESSION_PARAMETER.to_string(),
        Value::String(harness_session.to_string()),
    );
    config
}

/// The configuration of the engine's continuation of an agent whose turn ended
/// on a `progress` note.
///
/// Unlike [`reprompt_config`] it keeps the action's workspace access: the
/// agent has work left to do, and is not asked to repair the form of work
/// already done.
fn progress_continuation_config(
    original: &AgentActionConfig,
    harness: &str,
    harness_session: &str,
) -> AgentActionConfig {
    continuation_config(original, harness, harness_session, PROGRESS_CONTINUATION)
}

/// What follows the conductor's own words when it asks the agent again
/// (F1): the same boundaries as the engine's reprompt.
pub const CONDUCTOR_REPROMPT_GUARD: &str = "Do not continue the task and do not change any \
files. Reply only with the JSON object described below.";

/// The configuration of the conductor's reprompt of an agent (F1, R17):
/// [`reprompt_config`] with the conductor's `prompt` in place of the
/// engine's generic one.
pub fn conductor_reprompt_config(
    original: &AgentActionConfig,
    harness: &str,
    harness_session: &str,
    prompt: &str,
) -> AgentActionConfig {
    let mut config = reprompt_config(original, harness, harness_session, "");
    config.prompt = format!("{}\n\n{CONDUCTOR_REPROMPT_GUARD}", prompt.trim());
    config
}

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

/// Configures the next normal attempt as a continuation of an operator-facing
/// conversation. Unlike [`reprompt_config`], this preserves the action's
/// workspace permission: the operator is explicitly asking the agent to keep
/// working, not merely to repair an output envelope.
pub fn continuation_config(
    original: &AgentActionConfig,
    harness: &str,
    harness_session: &str,
    message: &str,
) -> AgentActionConfig {
    let mut config = original.clone();
    config.harness = Some(harness.to_string());
    config.parameters.insert(
        RESUME_SESSION_PARAMETER.to_string(),
        Value::String(harness_session.to_string()),
    );
    config.prompt = message.to_string();
    config
}

/// The variable the harness session id is published as, when there is one.
pub const SESSION_ID_VARIABLE: &str = "agent_session_id";

/// The `data` key carrying the session record of the run.
pub const SESSION_DATA_KEY: &str = "agent_session";

/// The most a single stdout line is handed to the harness's stream parser.
///
/// Every line reaches the conversation file in full regardless (R14) — this
/// only bounds what is fed to [`HarnessStreamParser::on_stdout_line`]. Tens of
/// megabytes is well past anything a harness's own JSON envelope needs (a
/// multi-megabyte `tool_result` line is ordinary; the final `result`/`text`
/// line is not), so a line past this limit is bulky tool output, not the
/// answer, and is safely skipped for parsing.
pub const PARSE_LINE_LIMIT: usize = 32 * 1024 * 1024;

/// Receives the agent-session events of a run.
///
/// The executor is shared between executions, so the execution the events
/// belong to is passed in per call rather than held.
pub trait AgentEventSink: Send + Sync + std::fmt::Debug {
    /// Records one event of `execution_id`.
    ///
    /// Recording must never fail a run: an event that cannot be written is
    /// logged and dropped, the same way the engine's live channel ignores a
    /// receiver that went away.
    fn emit(&self, execution_id: &str, data: EventData);
}

/// The sink the engine installs: append to the execution's log, then publish.
#[derive(Debug, Clone)]
pub struct LoggingEventSink {
    store: ExecutionStore,
    events: Option<broadcast::Sender<Event>>,
}

impl LoggingEventSink {
    /// A sink writing to `store` and publishing on `events`.
    pub fn new(store: ExecutionStore, events: Option<broadcast::Sender<Event>>) -> Self {
        Self { store, events }
    }
}

impl AgentEventSink for LoggingEventSink {
    fn emit(&self, execution_id: &str, data: EventData) {
        match self
            .store
            .event_log(execution_id)
            .append_next(&execution_id.to_string(), data)
        {
            Ok(event) => {
                if let Some(events) = &self.events {
                    let _ = events.send(event);
                }
            }
            Err(error) => {
                tracing::warn!(
                    execution_id,
                    %error,
                    "an agent session event could not be appended to the log"
                );
            }
        }
    }
}

/// Best-effort access to one attempt's conversation file (U3).
///
/// Wraps the writer in `Arc<Mutex<_>>` because stdout and stderr are drained
/// concurrently (see [`stream_lines`]) and both append to the same file; the
/// lock is held only for the duration of one `write_all`, never across an
/// `.await`. `None` stands for "there is nowhere to write" — no store was
/// given, or opening the file failed — and every method is then a no-op,
/// following the same "recording must never fail a run" rule as
/// [`AgentEventSink`].
#[derive(Clone)]
struct ConversationSink(Option<Arc<Mutex<ConversationWriter>>>);

impl ConversationSink {
    /// No file to write to.
    fn none() -> Self {
        Self(None)
    }

    /// Opens `attempt_id`'s conversation file in `store`, or answers `none()`
    /// and logs why when that fails.
    fn open(store: &ConversationStore, attempt_id: &str) -> Self {
        match store.writer(attempt_id) {
            Ok(writer) => Self(Some(Arc::new(Mutex::new(writer)))),
            Err(error) => {
                tracing::warn!(
                    attempt_id,
                    %error,
                    "could not open this attempt's conversation file; the run \
                     continues without one"
                );
                Self::none()
            }
        }
    }

    /// Appends one entry with its elements, logging (and otherwise
    /// ignoring) a failure to.
    fn append(&self, source: ConversationSource, raw: &[u8], elements: &[ChatElement]) {
        let Some(writer) = &self.0 else { return };
        let mut writer = writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Err(error) = writer.append(source, raw, elements) {
            tracing::warn!(%error, "could not append a conversation entry");
        }
    }

    /// Convenience for a narsor-authored entry (the prompt, the outcome).
    fn append_narsor(&self, text: &str, elements: &[ChatElement]) {
        self.append(ConversationSource::Narsor, text.as_bytes(), elements);
    }

    /// A narsor-authored entry about the harness process — its exit code,
    /// timeout, or failure to run — whose one element is a
    /// [`TechnicalCategory::Process`] event summarized by the same `text`.
    fn append_outcome(&self, text: &str, data: Value) {
        let element = ChatElement::Technical {
            category: TechnicalCategory::Process,
            summary: text.to_string(),
            data: Some(data),
        };
        self.append_narsor(text, &[element]);
    }

    /// Flushes the writer to disk, once, at the very end of the attempt.
    ///
    /// Consumes the sink: nothing appends after an attempt has finished.
    /// Silently does nothing if another clone of this sink is still alive
    /// when this is called — which never happens in practice, since every
    /// reader task has already joined by the time [`AgentExecutor::run`]
    /// calls this.
    fn finish(self) {
        let Some(writer) = self.0 else { return };
        match Arc::try_unwrap(writer) {
            Ok(mutex) => {
                let writer = mutex
                    .into_inner()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Err(error) = writer.finish() {
                    tracing::warn!(%error, "could not sync this attempt's conversation file");
                }
            }
            Err(_still_shared) => {
                tracing::warn!(
                    "the conversation writer still had outstanding references at the \
                     end of the attempt; it was not synced explicitly"
                );
            }
        }
    }
}

/// Executes `AgentAction` states.
///
/// Holds the harness registry, so pointing `claude-code` at a different binary
/// — a wrapper, an absolute path, the test suite's fake harness — is a matter
/// of building the executor with another registry.
#[derive(Debug, Clone)]
pub struct AgentExecutor {
    harnesses: HarnessRegistry,
    events: Option<Arc<dyn AgentEventSink>>,
    /// How long a harness may go on after its final answer.
    final_answer_grace: Duration,
}

/// Where the harness of an agent action that names none comes from.
///
/// Shared by every engine: the daemon's reads its live
/// [`DefaultHarnessSelector`], the runner's a name of its own (U7: the one
/// the control plane last reported). Read once per action — the engine pins
/// the answer for every attempt of that action (R16a) — so an implementation
/// only ever answers "right now".
pub trait DefaultHarnessSource: Send + Sync + std::fmt::Debug {
    /// The default harness at this moment.
    fn current_harness(&self) -> String;
}

/// A fixed default.
impl DefaultHarnessSource for String {
    fn current_harness(&self) -> String {
        self.clone()
    }
}

/// The daemon's live default: whatever the operator selected last.
impl DefaultHarnessSource for DefaultHarnessSelector {
    fn current_harness(&self) -> String {
        self.current().effective
    }
}

impl AgentExecutor {
    /// An executor with the stock harnesses and no event sink.
    pub fn new() -> Self {
        Self {
            harnesses: HarnessRegistry::defaults(),
            events: None,
            final_answer_grace: FINAL_ANSWER_GRACE,
        }
    }

    /// The same executor, waiting `grace` after a final answer before it ends
    /// the harness's process group.
    pub fn with_final_answer_grace(mut self, grace: Duration) -> Self {
        self.final_answer_grace = grace;
        self
    }

    /// An executor with a registry of its own.
    pub fn with_harnesses(harnesses: HarnessRegistry) -> Self {
        Self {
            harnesses,
            events: None,
            final_answer_grace: FINAL_ANSWER_GRACE,
        }
    }

    /// Returns the executor with the session events routed to `sink`.
    pub fn with_event_sink(mut self, sink: Arc<dyn AgentEventSink>) -> Self {
        self.events = Some(sink);
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
        Box::pin(async move { self.run_until_answered(request).await })
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

    /// The agent work of one step: a run, and a resumed run for every
    /// `progress` note that ended one.
    ///
    /// A harness may close its turn on a `progress` note — Claude Code does
    /// when nothing runs in the background — but the note is not an answer
    /// and not an error: the step is not done. The session is resumed with
    /// the step's own workspace access, inside this one action, so the log
    /// shows no failed session, no rejected output and no reprompt. There is
    /// no limit on notes; the timeout of the step, counted from its first
    /// run, ends a series that never gives a label.
    async fn run_until_answered(
        &self,
        request: ActionRequest<'_>,
    ) -> Result<ActionOutcome, ActionError> {
        let started = std::time::Instant::now();
        let mut action = request.action.clone();
        let mut purpose = request.purpose;
        loop {
            let timeout = match request.timeout {
                None => None,
                Some(limit) => match limit
                    .checked_sub(started.elapsed())
                    .filter(|left| !left.is_zero())
                {
                    Some(left) => Some(left),
                    None => return Ok(ActionOutcome::TimedOut(timed_out(&action, limit))),
                },
            };
            let attempt = ActionRequest {
                action: &action,
                timeout,
                purpose,
                ..request
            };
            let error = match self.run(attempt).await {
                // The limit reported is the step's, not what was left of it.
                Ok(ActionOutcome::TimedOut(mut timed_out)) => {
                    if let Some(limit) = request.timeout {
                        timed_out.timeout = limit;
                    }
                    return Ok(ActionOutcome::TimedOut(timed_out));
                }
                Err(error) => error,
                done => return done,
            };
            let Some((harness, session)) = progress_session(&error) else {
                return Err(error);
            };
            action = ActionConfig::Agent(progress_continuation_config(
                agent_config(&action)?,
                &harness,
                &session,
            ));
            purpose = PromptPurpose::Continuation;
        }
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
        let schema = output_schema(&request.contract);
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
            output_schema: Some(&schema),
        }) {
            Ok(invocation) => invocation,
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

        let ending = spawn_and_wait(
            &invocation.program,
            &invocation.args,
            workspace,
            Some(request.action_execution_id),
            request.timeout,
            self.final_answer_grace,
            adapter.stream_parser(),
            &conversation,
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
                    instruction:
                        "Claude Code needs a permission decision before this task can continue."
                            .to_string(),
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
                if progress_session(&error).is_some() {
                    // A note, not a failure: the executor resumes the session.
                    self.complete_session(
                        request.execution_id,
                        request.action_execution_id,
                        &mut session,
                        run.session_id.clone(),
                    );
                } else {
                    self.fail_session(
                        request.execution_id,
                        request.action_execution_id,
                        &mut session,
                        run.session_id.clone(),
                        error.message.clone(),
                    );
                }
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

    /// Marks the session completed and says so in the log.
    fn complete_session(
        &self,
        execution_id: Option<&str>,
        action_execution_id: &str,
        session: &mut AgentSession,
        harness_session_id: Option<String>,
    ) {
        session.status = AgentSessionStatus::Completed;
        session.ended_at = Some(Utc::now());
        self.emit(
            execution_id,
            EventData::AgentSessionCompleted {
                session_id: session.id.clone(),
                action_execution_id: Some(action_execution_id.to_string()),
                harness_session_id,
            },
        );
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

/// The session record of a run, before the harness has said anything.
fn new_session(
    config: &AgentActionConfig,
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
        // The harness is recorded here as well as on the session itself, so
        // the session config alone says where the session can be resumed
        // (KTD7), even when the action named no harness.
        config: serde_json::json!({
            "harness": harness,
            "read_only_paths": config.read_only_paths,
            "parameters": config.parameters,
        }),
    }
}

/// The element of a line the harness wrote to stderr: a
/// [`TechnicalCategory::Stderr`] event summarized by the line itself, as a
/// [`summary_line`]; the raw line keeps it in full.
///
/// Only a prefix of the line is decoded — a few bytes per summary
/// character — so a huge line is never converted, or stored, twice.
pub(crate) fn stderr_element(line: &[u8]) -> ChatElement {
    let prefix = &line[..line.len().min(STDERR_SUMMARY_SOURCE_BYTES)];
    ChatElement::Technical {
        category: TechnicalCategory::Stderr,
        summary: summary_line(&String::from_utf8_lossy(prefix)),
        data: None,
    }
}

/// How much of a stderr line [`stderr_element`] decodes: enough for a full
/// summary of four-byte characters, with one to spare to mark it as cut.
const STDERR_SUMMARY_SOURCE_BYTES: usize = (SUMMARY_MAX_CHARS + 1) * 4;

/// The harness session this run continues, if `parameters` names one (D18).
///
/// `parameters` is **not** template-expanded (only `prompt` is, see
/// `TemplateScope::render_action`), so this is a literal session id — enough to
/// pin a run to a known session, not enough to chain one state's session into
/// the next. The captured id is published as [`SESSION_ID_VARIABLE`] so that
/// chaining becomes a matter of expanding `parameters` one day, and nothing
/// else.
fn resume_session(config: &AgentActionConfig) -> Result<Option<&str>, ActionError> {
    match config.parameters.get(RESUME_SESSION_PARAMETER) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(session)) if session.is_empty() => Ok(None),
        Some(Value::String(session)) => Ok(Some(session.as_str())),
        Some(other) => Err(ActionError {
            message: format!(
                "parameter '{RESUME_SESSION_PARAMETER}' must be a session id string, got {other}"
            ),
            error_type: Some("invalid_parameter".to_string()),
            details: None,
        }),
    }
}

/// A harness process that ran to completion, already parsed.
pub(crate) struct FinishedRun {
    /// What the adapter's stream parser distilled from the stdout stream.
    pub(crate) run: HarnessRun,
    /// The process's exit code.
    pub(crate) exit_code: i32,
    /// A bounded capture of stderr, for error messages only — the full text,
    /// unbounded, is what the conversation file holds (R14).
    pub(crate) stderr: String,
    /// The process group that was still alive after the final answer and was
    /// ended by narsor, when there was one.
    pub(crate) terminated_group: Option<i32>,
}

/// How the harness process ended, from [`spawn_and_wait`]'s point of view.
pub(crate) enum SpawnEnding {
    /// It ran to completion (successfully or not — that is `run.failure`'s
    /// business, not this type's).
    Finished(FinishedRun),
    /// The time limit ran out and the process was killed.
    TimedOut,
}

/// Runs one harness process outside any action attempt — no conversation
/// file, no `ORCHESTRATOR_ACTION_ID` — for the conductor (U9), which reuses
/// the spawning (through [`spawn_retrying_text_busy`]), the pipe draining and
/// the stream parsing of an agent step without being one. The elements the
/// parser returns are discarded with the lines: there is nowhere to record
/// them.
pub(crate) async fn run_harness_process(
    program: &str,
    args: &[String],
    working_dir: &Path,
    limit: Option<Duration>,
    parser: Box<dyn HarnessStreamParser>,
) -> Result<SpawnEnding, ActionError> {
    spawn_and_wait(
        program,
        args,
        working_dir,
        None,
        limit,
        FINAL_ANSWER_GRACE,
        parser,
        &ConversationSink::none(),
    )
    .await
}

/// How long the harness may go on after its final answer before its process
/// group is ended. Long enough for it to write its session to disk, so the
/// session can be resumed.
const FINAL_ANSWER_GRACE: Duration = Duration::from_secs(15);

/// How long the group has to leave on `SIGTERM` before it is killed.
const TERM_TO_KILL: Duration = Duration::from_secs(3);

/// How long the pipes are read for after the group was killed.
const DRAIN_AFTER_KILL: Duration = Duration::from_secs(2);

/// Whether a question, like a choice, ends the step shortly after it.
///
/// Off until a run against the real `claude` shows that `--resume` works on
/// a session whose process was killed right after the question; the answer
/// to the question is that resume.
const KILL_AFTER_QUESTION: bool = false;

/// What [`spawn_and_wait`]'s driving loop found out about the group.
struct Driven {
    /// What the pipes and the wait ended with; `None` when the pipes did not
    /// close even after the group was killed.
    joined: Option<Joined>,
    /// The harness was still running when the grace period ran out.
    forced: bool,
    /// The group still had members when it was signalled.
    terminated: bool,
}

type Joined = (io::Result<()>, io::Result<()>, io::Result<ExitStatus>);

/// Runs the harness, draining both pipes while it goes and feeding every
/// stdout line to `parser` and then, with the elements it returned, to the
/// conversation store.
///
/// Reading is a line loop ([`stream_lines`]) rather than `read_capped` (used
/// by the script executor, which has no per-line structure to preserve): a
/// chatty agent — and every agent is chatty — would otherwise block on a full
/// pipe and look exactly like a hang, so both pipes are still drained
/// concurrently with the wait, only now line by line instead of as one
/// unbounded blob.
///
/// The harness leads a process group of its own. Once the parser has seen the
/// final answer the step does not wait for the process to exit: after `grace`
/// the group is sent `SIGTERM` and then `SIGKILL`. The group is cleaned up
/// even when the harness exits by itself — a background task it left behind
/// keeps the stdout pipe open, so the pipe would never reach end of file.
#[allow(clippy::too_many_arguments)]
async fn spawn_and_wait(
    program: &str,
    args: &[String],
    working_dir: &Path,
    action_execution_id: Option<&str>,
    limit: Option<Duration>,
    grace: Duration,
    parser: Box<dyn HarnessStreamParser>,
    conversation: &ConversationSink,
) -> Result<SpawnEnding, ActionError> {
    let mut command = Command::new(program);
    command.args(args).current_dir(working_dir);
    if let Some(action_execution_id) = action_execution_id {
        command.env(ACTION_ID_ENV_VAR, action_execution_id);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);

    let mut child = spawn_retrying_text_busy(&mut command)
        .await
        .map_err(|error| spawn_error(program, working_dir, &error))?;
    let mut group = GroupGuard::new(child.id());

    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");

    let mut parser = parser;
    let mut stderr_capture: Vec<u8> = Vec::new();
    // Kept here, not in the stdout task: the sender must outlive the task, or
    // the driving loop would see a closed channel where it expects an answer.
    let (answered_tx, mut answered_rx) = tokio::sync::watch::channel(false);

    let stdout_task = stream_lines(&mut stdout_pipe, |line| {
        let elements = if line.len() <= PARSE_LINE_LIMIT {
            parser.on_stdout_line(line)
        } else {
            vec![ChatElement::Unclassified {
                reason: UnclassifiedReason::TooLarge,
            }]
        };
        conversation.append(ConversationSource::Stdout, line, &elements);
        let ends_step = match parser.final_answer() {
            Some(FinalAnswer::Choice) => true,
            Some(FinalAnswer::AwaitingInput) => KILL_AFTER_QUESTION,
            None => false,
        };
        if ends_step {
            answered_tx.send_replace(true);
        }
    });

    let stderr_task = stream_lines(&mut stderr_pipe, |line| {
        conversation.append(ConversationSource::Stderr, line, &[stderr_element(line)]);
        accumulate_capped(&mut stderr_capture, line, MAX_BUFFERED_OUTPUT_BYTES);
        accumulate_capped(&mut stderr_capture, b"\n", MAX_BUFFERED_OUTPUT_BYTES);
    });

    let pgid = group.pgid();
    let drive = async {
        let wait = async { tokio::join!(stdout_task, stderr_task, child.wait()) };
        tokio::pin!(wait);

        let mut joined: Option<Joined> = None;
        tokio::select! {
            finished = &mut wait => joined = Some(finished),
            _ = answered_rx.wait_for(|answered| *answered) => {}
        }
        let answered = *answered_rx.borrow();
        if joined.is_none() {
            joined = tokio::time::timeout(grace, &mut wait).await.ok();
        }
        let forced = joined.is_none();

        let mut terminated = false;
        if let Some(pgid) = pgid.filter(|_| answered) {
            if forced {
                terminated = process_group::signal(pgid, process_group::TERMINATE);
                if terminated {
                    joined = tokio::time::timeout(TERM_TO_KILL, &mut wait).await.ok();
                }
            }
            terminated |= process_group::signal(pgid, process_group::KILL);
            if joined.is_none() {
                joined = tokio::time::timeout(DRAIN_AFTER_KILL, &mut wait).await.ok();
            }
        }
        Driven {
            joined,
            forced,
            terminated,
        }
    };

    let driven = match limit {
        Some(limit) => tokio::time::timeout(limit, drive).await,
        None => Ok(drive.await),
    };

    let driven = match driven {
        Ok(driven) => driven,
        Err(_elapsed) => {
            // The waiting future is gone, so the child can be reaped for
            // good. The still armed guard kills the rest of the group.
            let _ = child.kill().await;
            return Ok(SpawnEnding::TimedOut);
        }
    };
    group.disarm();

    let Driven {
        joined,
        forced,
        terminated,
    } = driven;
    let (stdout_result, stderr_result, status) = match joined {
        Some(joined) => joined,
        // The pipes stayed open after the kill: a process outside the group
        // holds them. What was read so far is all there will be.
        None => (Ok(()), Ok(()), child.wait().await),
    };

    stdout_result.map_err(|error| io_error(program, "stdout", &error))?;
    stderr_result.map_err(|error| io_error(program, "stderr", &error))?;
    let status = status.map_err(|error| io_error(program, "exit status", &error))?;

    let stderr = String::from_utf8_lossy(&stderr_capture).into_owned();

    // A harness ended after its final answer was not a failure, whatever
    // status the signal left.
    let exit_code = match status.code() {
        _ if forced => 0,
        Some(code) => code,
        None => {
            return Err(ActionError {
                message: format!("the agent harness '{program}' was killed and left no exit code"),
                error_type: Some("killed_by_signal".to_string()),
                details: Some(
                    serde_json::json!({ "program": program, "stderr": truncate(&stderr) }),
                ),
            });
        }
    };

    let run = parser.finish(exit_code, &stderr);

    Ok(SpawnEnding::Finished(FinishedRun {
        run,
        exit_code,
        stderr,
        terminated_group: terminated.then_some(pgid).flatten(),
    }))
}

/// The hard ceiling on one unterminated line's accumulated size, independent
/// of [`PARSE_LINE_LIMIT`] (which only bounds what the *parser* sees, not
/// what [`stream_lines`] holds while waiting for the next `\n`).
///
/// A real tool-result line runs to single-digit megabytes at most (R14's own
/// justification cites "wielomegabajtowe linie wyników narzędzi" — a few
/// tens of MiB); a harness that streams for an entire timeout window with no
/// newline at all is not a line at any size R14 was written for, and letting
/// this accumulator grow without bound turns a misbehaving subprocess into
/// an OOM instead of the ordinary "this action failed" outcome every other
/// harness failure produces. 256 MiB is generous headroom above any
/// realistic single line while still being a bound.
const MAX_LINE_BYTES: usize = 256 * 1024 * 1024;

/// Reads `pipe` line by line, calling `on_line` with the raw bytes of each
/// complete line — the trailing `\n` stripped and nothing else touched, not
/// even a trailing `\r`, not even invalid UTF-8. The final, unterminated
/// chunk left at EOF (the trace of a harness that never printed a last
/// newline) is delivered the same way as any other line.
///
/// Deliberately not `tokio::io::AsyncBufReadExt::lines()` (requires valid
/// UTF-8, and has no size limit of its own) and not
/// `tokio_util::codec::LinesCodec` (silently drops anything past its
/// configured length): this is the `fill_buf`/`consume` loop the two would
/// otherwise hide, kept explicit so that a line of any size still reaches
/// `on_line` in full (R14) — only *parsing* it is ever capped, by the caller,
/// never this function's own reading. The one exception is
/// [`MAX_LINE_BYTES`]: past that, this stops treating the stream as
/// well-formed and reports an error instead of accumulating forever.
async fn stream_lines<R>(pipe: &mut R, mut on_line: impl FnMut(&[u8])) -> io::Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::with_capacity(64 * 1024, pipe);
    let mut line: Vec<u8> = Vec::new();

    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            break;
        }

        match available.iter().position(|&byte| byte == b'\n') {
            Some(pos) => {
                line.extend_from_slice(&available[..pos]);
                reader.consume(pos + 1);
                on_line(&line);
                line.clear();
            }
            None => {
                let consumed = available.len();
                line.extend_from_slice(available);
                reader.consume(consumed);
                if line.len() > MAX_LINE_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("a single line exceeded {MAX_LINE_BYTES} bytes with no newline"),
                    ));
                }
            }
        }
    }

    if !line.is_empty() {
        on_line(&line);
    }
    Ok(())
}

/// Appends as much of `chunk` as fits within `cap` total bytes, silently
/// dropping the rest once the cap is reached — a smaller, line-oriented
/// cousin of `read_capped`'s "keep draining, stop retaining" rule, used
/// here only for the bounded stderr text quoted in error messages. The
/// conversation file holds the unbounded original regardless.
fn accumulate_capped(buffer: &mut Vec<u8>, chunk: &[u8], cap: usize) {
    if buffer.len() >= cap {
        return;
    }
    let room = cap - buffer.len();
    buffer.extend_from_slice(&chunk[..chunk.len().min(room)]);
}

/// How many times a fresh `ETXTBSY` is retried before it is given up on as a
/// real error.
const SPAWN_TEXT_BUSY_RETRIES: u32 = 20;

/// Spawns `command`, retrying briefly on `ExecutableFileBusy`.
///
/// Linux (and other Unixes) refuse to `execve` a file that some other open
/// file description still has open for writing — ordinarily this means a
/// build is genuinely in flight, but it is also a real, if brief, race for a
/// binary that was just written and closed a moment ago and is executed
/// again immediately after (a harness deployed by a rolling update; the test
/// suite's own fixture, copied and renamed into place right before this same
/// process runs it — see `tests/actions_agent.rs`'s `FakeHarness::install`).
/// The window is a matter of microseconds, so a handful of immediate retries
/// clears it without ever masking a binary that is actually, persistently
/// busy — those keep failing past the retry budget exactly as before.
async fn spawn_retrying_text_busy(command: &mut Command) -> std::io::Result<tokio::process::Child> {
    const RETRY_DELAY_MS: u64 = 5;

    for attempt in 0..SPAWN_TEXT_BUSY_RETRIES {
        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(error)
                if attempt + 1 < SPAWN_TEXT_BUSY_RETRIES
                    && error.kind() == io::ErrorKind::ExecutableFileBusy =>
            {
                tokio::time::sleep(Duration::from_millis(RETRY_DELAY_MS)).await;
            }
            // On the last attempt, an `ExecutableFileBusy` gets a message
            // naming the retry budget already spent, so this is
            // distinguishable from a first-attempt failure with the same
            // raw OS error — otherwise both look identical to an operator,
            // even though one means "genuinely, persistently busy" and the
            // other never should have reached this branch at all.
            Err(error) if error.kind() == io::ErrorKind::ExecutableFileBusy => {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "still busy after {SPAWN_TEXT_BUSY_RETRIES} attempts over ~{}ms: {error}",
                        u64::from(SPAWN_TEXT_BUSY_RETRIES - 1) * RETRY_DELAY_MS
                    ),
                ));
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("the loop above always returns before exhausting its retries")
}

/// The harness could not be started.
///
/// A missing binary gets its own message: `No such file or directory` on its
/// own tells an operator nothing about *which* file, and the answer is always
/// the same — install the harness, or put it on `PATH`.
fn spawn_error(program: &str, working_dir: &Path, error: &std::io::Error) -> ActionError {
    let message = if error.kind() == std::io::ErrorKind::NotFound {
        format!(
            "the agent harness binary '{program}' was not found; it is a runtime \
             prerequisite and must be installed and on PATH (or named by an \
             absolute path)"
        )
    } else {
        format!(
            "failed to start the agent harness '{program}' in {}: {error}",
            working_dir.display()
        )
    };

    ActionError {
        message,
        error_type: Some(if error.kind() == std::io::ErrorKind::NotFound {
            "harness_not_found".to_string()
        } else {
            "spawn_failed".to_string()
        }),
        details: Some(serde_json::json!({
            "program": program,
            "working_dir": working_dir.display().to_string(),
            "os_error": error.raw_os_error(),
        })),
    }
}

/// Reading a pipe or reaping the harness failed.
fn io_error(program: &str, what: &str, error: &std::io::Error) -> ActionError {
    ActionError {
        message: format!("failed to read {what} of the agent harness '{program}': {error}"),
        error_type: Some("io_failed".to_string()),
        details: Some(serde_json::json!({ "program": program })),
    }
}

/// The harness itself reported a failure.
fn harness_error(
    adapter: &dyn HarnessAdapter,
    failure: &str,
    finished: &FinishedRun,
    run: &HarnessRun,
    resumed_session: Option<&str>,
) -> ActionError {
    ActionError {
        message: format!("the agent harness '{}' failed: {failure}", adapter.name()),
        error_type: Some(
            if resumed_session.is_some() {
                RESUME_SESSION_REJECTED
            } else {
                "harness_failed"
            }
            .to_string(),
        ),
        details: Some(serde_json::json!({
            "harness": adapter.name(),
            "program": adapter.program(),
            "exit_code": finished.exit_code,
            "session_id": run.session_id,
            "stderr": truncate(&finished.stderr),
            "resumed_session": resumed_session,
        })),
    }
}

#[derive(Debug, Clone)]
enum AgentResponse {
    Completed(ChoiceAnswer),
    AwaitingInput {
        question: String,
        options: Vec<String>,
    },
}

/// Turns an agent's optional menu into a response form. Without a menu the
/// reply is required free text; with one, the operator can either type a free
/// answer or pick an option, optionally adding a comment.
fn agent_input_fields(options: Vec<String>) -> Vec<crate::domain::HumanInputField> {
    use crate::domain::{HumanInputField, HumanInputType};

    if options.is_empty() {
        return vec![HumanInputField {
            name: "answer".to_string(),
            field_type: HumanInputType::Text,
            required: true,
            description: Some("Answer for the agent.".to_string()),
            options,
        }];
    }

    vec![
        HumanInputField {
            name: "option".to_string(),
            field_type: HumanInputType::Select,
            required: false,
            description: Some("Optional choice proposed by the agent.".to_string()),
            options,
        },
        HumanInputField {
            name: "comment".to_string(),
            field_type: HumanInputType::Text,
            required: false,
            description: Some("Optional comment attached to the choice.".to_string()),
            options: Vec::new(),
        },
        HumanInputField {
            name: "answer".to_string(),
            field_type: HumanInputType::Text,
            required: false,
            description: Some("Free-form answer instead of a choice.".to_string()),
            options: Vec::new(),
        },
    ]
}

/// Extracts the output payload from whatever the agent said (D19/D20).
///
/// Accepts a bare JSON object, a fenced `json` code block, and a JSON object
/// trailing any amount of prose — the last object with a `choice` key wins,
/// because an agent that restates its answer restates it at the end.
///
/// Both failures are deliberate errors and not a default label:
///
/// * nothing in the answer parses as a JSON object;
/// * objects are there, but none carries a string `choice`.
pub fn extract_output(text: &str) -> Result<ChoiceAnswer, ActionError> {
    match extract_response(text)? {
        AgentResponse::Completed(output) => Ok(output),
        AgentResponse::AwaitingInput { .. } => Err(no_label_error(
            "the agent requested human input instead of producing an output label",
            text,
        )),
    }
}

/// Reads the agent's answer from a finished run: its `structured_output`
/// first, when the harness reported one that makes a usable answer, then the
/// text, as [`extract_response`] always did.
///
/// A `structured_output` that is unusable — no `choice` label and no
/// complete `awaiting_input` question — is not an error of its own: the text
/// is read instead, and only its failure is reported. A label the state does
/// not declare is not unusable here; like a label read from text, it is the
/// engine's to reject.
fn read_response(run: &HarnessRun) -> Result<AgentResponse, ActionError> {
    if let Some(structured) = &run.structured_output {
        match structured_response(structured) {
            Ok(response) => return Ok(response),
            Err(error) => tracing::warn!(
                reason = %error.message,
                "the harness's structured output is not a usable answer; reading the text instead"
            ),
        }
    }
    extract_response(&run.text)
}

/// Whether `value` is a `progress` note: a status report that is not an
/// answer and must not stand in for one.
pub(crate) fn is_progress_note(value: &Value) -> bool {
    value.get("status").and_then(Value::as_str) == Some(PROGRESS)
}

/// What kind of final answer `value` is, by the same rule [`read_response`]
/// uses: `None` for a `progress` note and for anything else that is not an
/// answer. A label the state does not declare is still a final answer; the
/// engine rejects it later.
pub(crate) fn final_answer_kind(value: &Value) -> Option<FinalAnswer> {
    match structured_response(value) {
        Ok(AgentResponse::Completed(_)) => Some(FinalAnswer::Choice),
        Ok(AgentResponse::AwaitingInput { .. }) => Some(FinalAnswer::AwaitingInput),
        Err(_) => None,
    }
}

/// Reads an answer from a harness's `structured_output` value, by the same
/// rules as an object found in the text.
fn structured_response(value: &Value) -> Result<AgentResponse, ActionError> {
    let object = value
        .as_object()
        .filter(|object| is_answer_object(object))
        .ok_or_else(|| {
            no_label_error(
                "the structured output has neither a string 'choice' nor an 'awaiting_input' status",
                &value.to_string(),
            )
        })?;
    response_from_object(object).map_err(|reason| no_label_error(reason, &value.to_string()))
}

/// Writes the agent's question to the conversation as a narsor entry, when
/// the accepted answer asks one.
///
/// The executor is the only producer of an output-contract question: it
/// knows which answer it accepted — the structured output or the text — so
/// the conversation shows exactly that question, once.
fn record_question(conversation: &ConversationSink, response: &AgentResponse) {
    let AgentResponse::AwaitingInput { question, options } = response else {
        return;
    };
    let entry = serde_json::json!({
        "event": "agent_question",
        "question": question,
        "options": options,
    })
    .to_string();
    conversation.append_narsor(
        &entry,
        &[ChatElement::AgentQuestion {
            question: question.clone(),
            options: options.clone(),
            source: QuestionSource::OutputContract,
            call_id: None,
        }],
    );
}

/// Extracts either a normal label or a request that pauses the same agent
/// action. The last matching object wins, following the normal output rule.
fn extract_response(text: &str) -> Result<AgentResponse, ActionError> {
    let objects = json_objects(text);

    if objects.is_empty() {
        return Err(no_label_error(
            "the agent's answer contains no JSON object at all",
            text,
        ));
    }

    let Some(object) = objects
        .iter()
        .rev()
        .find_map(|value| value.as_object().filter(|object| is_answer_object(object)))
    else {
        let progress = objects.iter().rev().any(is_progress_note);
        let mut error = no_label_error(
            if progress {
                "the agent's last answer was a progress note, not a final answer"
            } else {
                "the agent's answer contains JSON, but no object with a string 'choice' key"
            },
            text,
        );
        if progress {
            mark_progress_only(&mut error);
        }
        return Err(error);
    };

    response_from_object(object).map_err(|reason| no_label_error(reason, text))
}

/// Whether `object` is an answer at all: a question, or a string `choice`.
fn is_answer_object(object: &Map<String, Value>) -> bool {
    is_question(object) || answer::has_choice(object)
}

/// Whether `object` asks a person before the task continues.
fn is_question(object: &Map<String, Value>) -> bool {
    object.get("status").and_then(Value::as_str) == Some(AWAITING_INPUT)
}

/// Turns an answer object into the response it stands for. An error is the
/// reason the object is unusable; the caller quotes the answer it came
/// from (the agent's text, or the structured output) with it.
fn response_from_object(object: &Map<String, Value>) -> Result<AgentResponse, &'static str> {
    if is_question(object) {
        return question_from_object(object);
    }
    answer::read_choice_answer(object)
        .expect("the object was selected for having a choice")
        .map(AgentResponse::Completed)
}

/// Reads an `awaiting_input` object: its question and its optional menu.
fn question_from_object(object: &Map<String, Value>) -> Result<AgentResponse, &'static str> {
    let Some(question) = object.get("question").and_then(Value::as_str) else {
        return Err("the agent requested human input without a string 'question'");
    };
    if question.trim().is_empty() {
        return Err("the agent requested human input with an empty question");
    }
    let options = match object.get("options") {
        None => Vec::new(),
        Some(Value::Array(options)) => options
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()
            .filter(|options| {
                !options.is_empty() && options.iter().all(|option| !option.trim().is_empty())
            })
            .map(|options| options.into_iter().map(ToString::to_string).collect())
            .ok_or("the agent's input options must be a non-empty array of non-empty strings")?,
        Some(_) => return Err("the agent's input options must be an array of strings"),
    };
    Ok(AgentResponse::AwaitingInput {
        question: question.to_string(),
        options,
    })
}

/// How much of the agent's answer an extraction failure quotes back.
const FAILED_EXTRACTION_QUOTE_BYTES: usize = 2 * 1024;

/// No label could be extracted — and none is invented.
fn no_label_error(reason: &str, text: &str) -> ActionError {
    ActionError {
        message: format!(
            "no output label could be extracted from the agent's answer: {reason}. \
             The agent must end with a JSON object carrying a 'choice' key \
             (WORKFLOW_SCHEMA.md §3.3.A)"
        ),
        error_type: Some(MISSING_OUTPUT_LABEL.to_string()),
        details: Some(serde_json::json!({
            "answer_tail": super::harness::tail(text, FAILED_EXTRACTION_QUOTE_BYTES),
        })),
    }
}

/// The harness and the session to resume when `error` says the run ended on
/// a `progress` note and the harness reported a session — `None` for any
/// other failure.
fn progress_session(error: &ActionError) -> Option<(String, String)> {
    if error.error_type.as_deref() != Some(MISSING_OUTPUT_LABEL) {
        return None;
    }
    let details = error.details.as_ref()?;
    if details.get(PROGRESS_ONLY_DETAIL).and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let text = |key: &str| details.get(key).and_then(Value::as_str).map(str::to_string);
    Some((text("harness")?, text("harness_session_id")?))
}

/// The timeout of a step that ran out of time between two runs, before the
/// next one started.
fn timed_out(action: &ActionConfig, limit: Duration) -> TimedOutAction {
    let harness = match action {
        ActionConfig::Agent(config) => config.harness.as_deref().unwrap_or(BUILT_IN_HARNESS),
        _ => BUILT_IN_HARNESS,
    };
    TimedOutAction {
        timeout: limit,
        subject: format!("agent harness '{harness}'"),
    }
}

/// Says in the details of an extraction failure that the run ended on a
/// `progress` note ([`PROGRESS_ONLY_DETAIL`]).
fn mark_progress_only(error: &mut ActionError) {
    if let Some(Value::Object(details)) = error.details.as_mut() {
        details.insert(PROGRESS_ONLY_DETAIL.to_string(), Value::Bool(true));
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

/// How many `{` that never close a scan of an agent's text skips, and how
/// many candidates it tries from the right, before it gives up.
///
/// Each one costs a scan to the end of the text, so without a bound a text
/// full of stray braces takes quadratic time. Prose has a few; the contract
/// is near the end.
const MAX_BRACE_CANDIDATES: usize = 64;

/// Every top-level JSON object in `text`, in the order they appear.
///
/// Scanning for balanced braces — string- and escape-aware, so a `}` inside a
/// string never ends an object — rather than looking for fences: it finds the
/// fenced case too, since the fence markers are outside the braces. A `{`
/// that never closes (a brace in the prose) is skipped, and the scan goes on
/// from the next one, so it cannot hide an object after it — up to
/// [`MAX_BRACE_CANDIDATES`] of them.
pub(crate) fn json_objects(text: &str) -> Vec<Value> {
    let mut found = Vec::new();
    for (start, end) in top_level_objects(text) {
        if let Ok(value) = serde_json::from_str::<Value>(&text[start..end]) {
            if value.is_object() {
                found.push(value);
            }
        }
    }
    found
}

/// The spans of the top-level balanced objects of `text`, skipping at most
/// [`MAX_BRACE_CANDIDATES`] braces that never close.
fn top_level_objects(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let (mut from, mut unclosed) = (0, 0);
    while let Some(offset) = text[from..].find('{') {
        let start = from + offset;
        match object_end(text, start) {
            Some(end) => {
                spans.push((start, end));
                from = end;
            }
            None if unclosed < MAX_BRACE_CANDIDATES => {
                unclosed += 1;
                from = start + 1;
            }
            None => break,
        }
    }
    spans
}

/// Builds the action result of a successful run.
fn build_result(
    answer: ChoiceAnswer,
    session: &AgentSession,
    run: &HarnessRun,
    finished: &FinishedRun,
) -> ActionResult {
    let reply = run_reply(run).or_else(|| contract_reply(&answer.reason, &answer.additional_data));
    let session_record = serde_json::to_value(session).unwrap_or(Value::Null);

    let mut result = answer.into_result();
    result.data = serde_json::json!({ SESSION_DATA_KEY: session_record });
    if let Some(session_id) = &run.session_id {
        // Never over an agent's own variable of that name: the agent's context
        // is the agent's to write (D10 keeps the engine the only writer, and
        // the engine writes what it is given).
        result
            .variables
            .entry(SESSION_ID_VARIABLE.to_string())
            .or_insert_with(|| Value::String(session_id.clone()));
    }
    result.exit_code = Some(finished.exit_code);
    result.stdout = truncate(&run.text);
    result.stderr = truncate(&finished.stderr);
    result.reply = reply;
    result
}

/// The reply of an agent that answered with the output-contract object
/// alone (KTD17): its `reason`, and below it its `additional_data` as
/// pretty-printed JSON. Without it, a step whose findings live only in the
/// object hands nothing to `{{ steps.NAME.reply }}`, and the step reading
/// it fails. `None` when both are empty.
fn contract_reply(reason: &str, additional_data: &Map<String, Value>) -> Option<String> {
    let mut parts = Vec::new();
    if !reason.is_empty() {
        parts.push(reason.to_string());
    }
    if !additional_data.is_empty() {
        if let Ok(json) = serde_json::to_string_pretty(additional_data) {
            parts.push(json);
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(truncate(&parts.join("\n\n")))
}

/// Adds the attempt's reply to the details of an extraction failure, so a
/// format reprompt can keep it instead of its own text (R3).
fn record_reply_in_details(error: &mut ActionError, run: &HarnessRun) {
    let Some(reply) = run_reply(run) else {
        return;
    };
    if let Some(Value::Object(details)) = error.details.as_mut() {
        details.insert("reply".to_string(), Value::String(reply));
    }
}

/// The step's reply of `run`: the reply its harness read, or else its final
/// text, without the output-contract object (R3).
pub(crate) fn run_reply(run: &HarnessRun) -> Option<String> {
    clean_reply(run.reply.as_deref().unwrap_or(&run.text))
}

/// The agent's reply: its text without the output-contract object, and
/// without the code fence around that object (R3). `None` when nothing else
/// is left.
fn clean_reply(text: &str) -> Option<String> {
    let reply = match final_contract_start(text) {
        Some(cut) => text[..cut].to_string(),
        None => {
            let mut reply = text.to_string();
            if let Some((begin, end)) = last_contract_span(text) {
                reply.replace_range(begin..end, "");
            }
            reply
        }
    };
    let trimmed = reply.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(truncate(trimmed))
    }
}

/// Where the text's closing output-contract object starts — or the code
/// fence wrapping it, when one does.
///
/// The object is the one that ends where the text ends (but for whitespace
/// and a closing fence), found from the right, so a brace in the prose
/// before it never hides it. At most [`MAX_BRACE_CANDIDATES`] braces are
/// tried.
fn final_contract_start(text: &str) -> Option<usize> {
    let trimmed = text.trim_end();
    let (body, fenced) = match trimmed.strip_suffix("```") {
        Some(rest) => (rest.trim_end(), true),
        None => (trimmed, false),
    };
    let mut candidates = body;
    let mut tried = 0;
    while let Some(start) = candidates.rfind('{') {
        tried += 1;
        if tried > MAX_BRACE_CANDIDATES {
            return None;
        }
        candidates = &body[..start];
        if object_end(body, start) != Some(body.len()) || !is_contract(&body[start..]) {
            continue;
        }
        if !fenced {
            return Some(start);
        }
        let before = body[..start].trim_end();
        let opening = before.strip_suffix("```").or_else(|| {
            let split = before.len().checked_sub("```json".len())?;
            before
                .get(split..)
                .filter(|tail| tail.eq_ignore_ascii_case("```json"))
                .map(|_| &before[..split])
        });
        // A closing fence that does not wrap the object belongs to
        // something else.
        return opening.map(str::len);
    }
    None
}

/// The end (exclusive) of the JSON object opening at `start`: string- and
/// escape-aware brace matching. `None` when it never closes.
fn object_end(text: &str, start: usize) -> Option<usize> {
    let (mut depth, mut quoted, mut escaped) = (0usize, false, false);
    for (index, ch) in text[start..].char_indices() {
        if quoted {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                quoted = false;
            }
            continue;
        }
        match ch {
            '"' => quoted = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(start + index + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Whether `object` is an output-contract object: an answer or a question.
fn is_contract(object: &str) -> bool {
    serde_json::from_str::<Value>(object)
        .ok()
        .and_then(|value| value.as_object().map(is_answer_object))
        .unwrap_or(false)
}

/// The last output-contract object among the text's top-level objects, for
/// a text that goes on after it.
fn last_contract_span(text: &str) -> Option<(usize, usize)> {
    top_level_objects(text)
        .into_iter()
        .rev()
        .find(|&(start, end)| is_contract(&text[start..end]))
}

#[cfg(test)]
mod json_object_tests {
    use super::{extract_response, json_objects, AgentResponse};
    use serde_json::json;

    fn label_of(text: &str) -> String {
        match extract_response(text).expect("a response") {
            AgentResponse::Completed(answer) => answer.choice,
            AgentResponse::AwaitingInput { .. } => panic!("not an output"),
        }
    }

    #[test]
    fn an_unbalanced_brace_in_the_prose_does_not_hide_the_label() {
        assert_eq!(
            label_of("The parser misses a `{` on line 3.\n{\"choice\":\"findings\"}"),
            "findings"
        );
        assert_eq!(
            label_of("Open { and a \"quote.\n```json\n{\"choice\":\"clean\"}\n```"),
            "clean"
        );
    }

    #[test]
    fn braces_in_strings_stay_inside_their_object() {
        let objects = json_objects(
            "Report.\n{\"choice\":\"findings\",\"additional_data\":{\"note\":\"a } and a {\",\
             \"nested\":{\"choice\":\"inner\"}}}",
        );
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0]["choice"], json!("findings"));
        assert_eq!(objects[0]["additional_data"]["note"], json!("a } and a {"));
    }

    #[test]
    fn a_text_full_of_unclosed_braces_is_read_in_linear_time() {
        let text = format!("{}\n{{\"choice\":\"findings\"}}", "{".repeat(200_000));
        let started = std::time::Instant::now();
        let _ = json_objects(&text);
        let _ = super::clean_reply(&text);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "stray braces must not make the scan quadratic: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn the_last_object_still_wins() {
        assert_eq!(
            label_of("{\"choice\":\"first\"} then { and {\"choice\":\"second\"}"),
            "second"
        );
    }

    fn progress_only_of(text: &str) -> Option<bool> {
        let error = extract_response(text).expect_err("no answer");
        error
            .details
            .as_ref()
            .and_then(|details| details.get(super::PROGRESS_ONLY_DETAIL))
            .and_then(serde_json::Value::as_bool)
    }

    #[test]
    fn a_run_that_ends_on_a_progress_note_is_marked_progress_only() {
        assert_eq!(
            progress_only_of("{\"status\":\"progress\",\"message\":\"I read the code\"}"),
            Some(true)
        );
    }

    #[test]
    fn a_run_without_a_label_or_a_progress_note_is_not_marked() {
        assert_eq!(progress_only_of("{\"reason\":\"no label\"}"), None);
    }
}

#[cfg(test)]
mod reply_tests {
    use super::{clean_reply, contract_reply};
    use crate::engine::actions::script::{MAX_CAPTURED_OUTPUT_BYTES, TRUNCATION_MARKER};

    #[test]
    fn a_long_reply_keeps_its_beginning_and_is_marked_cut() {
        let text = format!("{}\n{{\"choice\":\"findings\"}}", "a".repeat(70 * 1024));
        let reply = clean_reply(&text).expect("a reply");
        assert!(reply.ends_with(TRUNCATION_MARKER));
        assert_eq!(
            reply.len(),
            MAX_CAPTURED_OUTPUT_BYTES + TRUNCATION_MARKER.len()
        );
        assert!(reply.starts_with("aaa"));
    }

    #[test]
    fn reply_removes_the_contract_object_and_json_fence() {
        assert_eq!(
            clean_reply("Report here.\n```json\n{\"choice\":\"findings\"}\n```"),
            Some("Report here.".to_string())
        );
    }

    #[test]
    fn contract_only_text_has_no_reply() {
        assert_eq!(clean_reply("{\"choice\":\"clean\"}"), None);
    }

    #[test]
    fn an_unbalanced_brace_in_the_prose_does_not_hide_the_contract() {
        assert_eq!(
            clean_reply("The parser misses a `{` on line 3.\n{\"choice\":\"findings\"}"),
            Some("The parser misses a `{` on line 3.".to_string())
        );
    }

    #[test]
    fn braces_inside_the_contract_are_part_of_it() {
        assert_eq!(
            clean_reply(
                "Report.\n{\"choice\":\"findings\",\"additional_data\":{\"note\":\"a } and a {\",\
                 \"nested\":{\"choice\":\"inner\"}}}"
            ),
            Some("Report.".to_string())
        );
    }

    #[test]
    fn a_fenced_json_block_goes_with_the_contract() {
        assert_eq!(
            clean_reply("Report.\n\n```JSON\n{\"choice\":\"clean\"}\n```\n"),
            Some("Report.".to_string())
        );
        assert_eq!(
            clean_reply("Report.\n```\n{\"choice\":\"clean\"}\n```"),
            Some("Report.".to_string())
        );
    }

    #[test]
    fn an_earlier_code_block_stays_in_the_reply() {
        let text = "Fix it like this:\n```rust\nfn main() { println!(\"{}\", 1); }\n```\n\
                    Done.\n{\"choice\":\"findings\"}";
        assert_eq!(
            clean_reply(text),
            Some(
                "Fix it like this:\n```rust\nfn main() { println!(\"{}\", 1); }\n```\nDone."
                    .to_string()
            )
        );
    }

    #[test]
    fn a_reply_that_is_a_code_block_keeps_its_fences() {
        let text = "```text\nreport\n```\n{\"choice\":\"findings\"}";
        assert_eq!(clean_reply(text), Some("```text\nreport\n```".to_string()));
    }

    #[test]
    fn a_contract_followed_by_prose_is_still_removed() {
        assert_eq!(
            clean_reply("Report.\n{\"choice\":\"findings\"}\nThanks."),
            Some("Report.\n\nThanks.".to_string())
        );
    }

    #[test]
    fn the_reason_and_the_additional_data_stand_in_for_the_reply_of_a_contract_alone() {
        let data = serde_json::json!({ "findings": [{ "title": "Whitespace aborts startup" }] });
        let reply = contract_reply("one finding", data.as_object().expect("an object"))
            .expect("a reply from the contract");
        assert!(reply.starts_with("one finding\n\n"), "{reply}");
        assert!(reply.contains("Whitespace aborts startup"), "{reply}");
    }

    #[test]
    fn the_reason_alone_is_the_reply() {
        assert_eq!(
            contract_reply("all checks", &serde_json::Map::new()),
            Some("all checks".to_string())
        );
    }

    #[test]
    fn an_empty_contract_gives_no_reply() {
        assert_eq!(contract_reply("", &serde_json::Map::new()), None);
    }
}
