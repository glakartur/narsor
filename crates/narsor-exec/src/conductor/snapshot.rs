//! What the conductor is shown of an execution, rebuilt at every invocation
//! (R27): a core in the prompt and the rest as context files.
//!
//! # Core and context files
//!
//! The prompt carries only what every decision needs (R9), whole (R10):
//! the execution's status and current state with its definition and
//! outputs, why the conductor is invoked (the request with its payload, the
//! chat message it answers), the pending input, its annotation and the
//! current step's local values — and, for a purpose that analyses the
//! agent's answer, the last answer of the current state's latest attempt.
//!
//! Everything else goes to a context file, uncut (R11): the whole workflow
//! definition, the context's variables, every event with its payload, the
//! conversation of every agent attempt (one file each, so the conductor
//! reads only the ones it needs), the chat, and — for a conductor isolated
//! from the workspace (KTD5) — the workflow's artifact files. The harness
//! writes them to the invocation's context directory and lists them at the
//! end of the prompt (R12, R13).
//!
//! [`SnapshotInput`] is plain data, gathered from disk once by
//! [`SnapshotInput::collect`], so [`render`] is a pure function that can be
//! tested without an execution. Context files are untrusted agent text: the
//! conductor only reads them, and answers with a checked JSON object.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::conductor::store::{Annotation, ChatEntry, ConductorStore, QueuedRequest};
use crate::domain::{ChoiceDefinition, ConductorPurpose, Event, EventData, WorkflowExecution};
use crate::engine::actions::harness::ContextFile;
use crate::engine::actions::human::PendingHumanInput;
use crate::engine::{METADATA_ENGINE_KEY, METADATA_STEP_LOCALS_KEY};
use crate::persistence::{ConversationSource, ConversationStore, ExecutionStore};

/// The conversation of one agent attempt, whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptConversation {
    /// The state the attempt ran in.
    pub state_id: String,
    /// The attempt.
    pub attempt_id: String,
    /// Whether it ran in the execution's current state.
    pub current: bool,
    /// Its lines, oldest first, each prefixed with its source.
    pub text: String,
}

/// An artifact file of the workflow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactFile {
    /// Its name in the context.
    pub name: String,
    /// Where it is.
    pub location: String,
    /// Its content, or why it could not be read.
    pub content: String,
}

/// Everything a snapshot is made of.
#[derive(Debug, Clone)]
pub struct SnapshotInput {
    /// The execution.
    pub execution_id: String,
    /// Its workflow.
    pub workflow_id: String,
    /// Its status, as the API spells it.
    pub status: String,
    /// Its current state.
    pub current_state: Option<String>,
    /// The outputs the current state declares.
    pub state_outputs: Vec<ChoiceDefinition>,
    /// The pinned definition's YAML, or why there is none.
    pub definition: Result<String, String>,
    /// The request the conductor serves.
    pub request: Option<QueuedRequest>,
    /// The operator message it answers, for a chat request.
    pub chat_message: Option<ChatEntry>,
    /// The pending input, as its record is written.
    pub pending_input: Option<Value>,
    /// The annotation of the pending input.
    pub annotations: Vec<Annotation>,
    /// The context's variables.
    pub variables: Value,
    /// The `scope: local` values the current step keeps, name → value.
    pub step_locals: Value,
    /// The execution's events, oldest first.
    pub events: Vec<Event>,
    /// The final answer of the current state's latest agent attempt.
    pub last_agent_answer: Option<String>,
    /// The conversation of every agent attempt, oldest first.
    pub conversations: Vec<AttemptConversation>,
    /// The chat, oldest first.
    pub chat: Vec<ChatEntry>,
    /// The workflow's artifact files; empty unless the conductor is
    /// isolated from the workspace.
    pub artifacts: Vec<ArtifactFile>,
}

impl Default for SnapshotInput {
    fn default() -> Self {
        Self {
            execution_id: String::new(),
            workflow_id: String::new(),
            status: String::new(),
            current_state: None,
            state_outputs: Vec::new(),
            definition: Err("not collected".to_string()),
            request: None,
            chat_message: None,
            pending_input: None,
            annotations: Vec::new(),
            variables: Value::Null,
            step_locals: Value::Null,
            events: Vec::new(),
            last_agent_answer: None,
            conversations: Vec::new(),
            chat: Vec::new(),
            artifacts: Vec::new(),
        }
    }
}

/// A snapshot, rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The core the prompt carries.
    pub core: String,
    /// Everything else, one file per section (and per agent attempt); a
    /// section with nothing to show has no file.
    pub files: Vec<ContextFile>,
}

impl SnapshotInput {
    /// Gathers what the snapshot of `execution` is made of.
    ///
    /// Anything that cannot be read becomes a line saying so rather than a
    /// failed invocation — a partial snapshot is still worth showing.
    pub fn collect(
        store: &ExecutionStore,
        conductor: &ConductorStore,
        execution: &WorkflowExecution,
        definition: Result<(String, Vec<ChoiceDefinition>), String>,
        with_artifacts: bool,
    ) -> Self {
        let (definition, state_outputs) = match definition {
            Ok((source, outputs)) => (Ok(source), outputs),
            Err(error) => (Err(error), Vec::new()),
        };

        let pending: Option<PendingHumanInput> = store
            .load_pending_input(&execution.id)
            .unwrap_or_else(|error| {
                tracing::warn!(execution_id = %execution.id, %error, "conductor: unreadable pending input");
                None
            });
        let annotations = pending
            .as_ref()
            .and_then(|pending| {
                conductor
                    .load_annotation(pending.episode_key())
                    .ok()
                    .flatten()
            })
            .into_iter()
            .collect();

        let events = store
            .event_log(&execution.id)
            .read_all()
            .unwrap_or_default();
        let current_state = execution.current_state.as_deref();
        let conversations = conversations(
            &store.conversation_store(&execution.id),
            current_state,
            &events,
        );
        let last_agent_answer = last_agent_answer(current_state, &events);

        let artifacts = if with_artifacts {
            artifact_files(execution)
        } else {
            Vec::new()
        };

        Self {
            execution_id: execution.id.clone(),
            workflow_id: execution.workflow_id.clone(),
            status: serde_json::to_value(execution.status)
                .ok()
                .and_then(|status| status.as_str().map(str::to_string))
                .unwrap_or_else(|| format!("{:?}", execution.status)),
            current_state: execution.current_state.clone(),
            state_outputs,
            definition,
            request: None,
            chat_message: None,
            pending_input: pending.and_then(|pending| serde_json::to_value(pending).ok()),
            annotations,
            variables: serde_json::to_value(&execution.context.variables).unwrap_or(Value::Null),
            step_locals: current_step_locals(execution),
            events,
            last_agent_answer,
            conversations,
            chat: conductor.read_chat().unwrap_or_default(),
            artifacts,
        }
    }
}

/// The `scope: local` values the current state of `execution` keeps.
fn current_step_locals(execution: &WorkflowExecution) -> Value {
    let Some(state_id) = execution.current_state.as_deref() else {
        return Value::Null;
    };
    execution
        .metadata
        .get(METADATA_ENGINE_KEY)
        .and_then(|engine| engine.get(METADATA_STEP_LOCALS_KEY))
        .and_then(|locals| locals.get(state_id))
        .cloned()
        .unwrap_or(Value::Null)
}

/// Whether the conductor serving `purpose` analyses what the agent of the
/// current state answered, so the answer belongs in the core.
fn analyses_agent_answer(purpose: ConductorPurpose) -> bool {
    matches!(
        purpose,
        ConductorPurpose::ChooseOutput
            | ConductorPurpose::ChooseOutputAfterReprompt
            | ConductorPurpose::AnswerAgentQuestion
            | ConductorPurpose::DiagnoseFailure
    )
}

/// Renders `input` into the prompt core and the context files.
pub fn render(input: &SnapshotInput) -> Snapshot {
    Snapshot {
        core: render_core(input),
        files: context_files(input),
    }
}

/// The core, whole.
fn render_core(input: &SnapshotInput) -> String {
    let mut text = String::new();

    let _ = writeln!(text, "## Execution\n");
    let _ = writeln!(text, "- execution: {}", input.execution_id);
    let _ = writeln!(text, "- workflow: {}", input.workflow_id);
    let _ = writeln!(text, "- status: {}", input.status);
    let _ = writeln!(
        text,
        "- current state: {}",
        input.current_state.as_deref().unwrap_or("(none)")
    );
    if !input.state_outputs.is_empty() {
        let _ = writeln!(text, "- outputs of the current state:");
        for output in &input.state_outputs {
            match output.description.as_deref().map(str::trim) {
                Some(description) if !description.is_empty() => {
                    let _ = writeln!(text, "  - `{}` — {description}", output.label);
                }
                _ => {
                    let _ = writeln!(text, "  - `{}`", output.label);
                }
            }
        }
    }

    if let Some(request) = &input.request {
        let _ = writeln!(text, "\n## Request\n");
        let _ = writeln!(text, "- trigger: {}", request.trigger.name());
        let _ = writeln!(text, "- purpose: {}", request.purpose.name());
        if let Some(episode) = &request.episode {
            let _ = writeln!(text, "- episode: {episode}");
        }
        if !request.payload.is_null() {
            let _ = writeln!(
                text,
                "- details:\n\n```json\n{}\n```",
                pretty(&request.payload)
            );
        }
    }
    if let Some(message) = &input.chat_message {
        let _ = writeln!(
            text,
            "\n## The operator's message you answer\n\n{}",
            message.text
        );
    }

    if !is_empty_json(&input.step_locals) {
        let _ = writeln!(text, "\n## Local values of the current step\n");
        let _ = writeln!(text, "```json\n{}\n```", pretty(&input.step_locals));
    }

    let _ = writeln!(text, "\n## Pending input\n");
    match &input.pending_input {
        Some(pending) => {
            let _ = writeln!(text, "```json\n{}\n```", pretty(pending));
        }
        None => {
            let _ = writeln!(text, "(none)");
        }
    }

    if !input.annotations.is_empty() {
        let _ = writeln!(text, "\n## Annotations\n");
        for annotation in &input.annotations {
            let _ = writeln!(text, "- summary: {}", annotation.summary);
            if let Some(recommendation) = &annotation.recommendation {
                let _ = writeln!(text, "  recommendation: {recommendation}");
            }
        }
    }

    if let Some(state) = input
        .current_state
        .as_deref()
        .and_then(|state| state_definition(input.definition.as_deref().ok()?, state))
    {
        let _ = writeln!(text, "\n## Definition of the current state\n");
        let _ = writeln!(text, "```yaml\n{}\n```", state.trim_end());
    }

    let analyses_answer = input
        .request
        .as_ref()
        .is_some_and(|request| analyses_agent_answer(request.purpose));
    if let Some(answer) = input
        .last_agent_answer
        .as_deref()
        .filter(|_| analyses_answer)
    {
        let _ = writeln!(
            text,
            "\n## The agent's last answer in the current state\n\n```\n{}\n```",
            answer.trim_end()
        );
    }

    text
}

/// The YAML of `state` in the workflow `source`; `None` when the source
/// does not parse or has no such state.
fn state_definition(source: &str, state: &str) -> Option<String> {
    let workflow: serde_yaml::Value = serde_yaml::from_str(source).ok()?;
    let definition = workflow.get("states")?.get(state)?;
    serde_yaml::to_string(definition).ok()
}

/// The context files of `input`: every section outside the core, uncut.
fn context_files(input: &SnapshotInput) -> Vec<ContextFile> {
    let mut files = Vec::new();

    match &input.definition {
        Ok(source) => files.push(ContextFile::new(
            "workflow.yaml",
            "The pinned workflow definition, every state.",
            source.clone(),
        )),
        Err(error) => files.push(ContextFile::new(
            "workflow.yaml",
            "Why the workflow definition is unavailable.",
            format!("# unavailable: {error}\n"),
        )),
    }

    if !is_empty_json(&input.variables) {
        files.push(ContextFile::new(
            "variables.json",
            "The context variables of the execution.",
            pretty(&input.variables),
        ));
    }

    if !input.events.is_empty() {
        let mut text = String::new();
        for event in &input.events {
            let payload = serde_json::to_string(&event.data).unwrap_or_default();
            let _ = writeln!(
                text,
                "- #{} {} {payload}",
                event.sequence,
                event.timestamp.format("%Y-%m-%dT%H:%M:%SZ"),
            );
        }
        files.push(ContextFile::new(
            "events.md",
            "Every event of the execution, oldest first, with its whole payload.",
            text,
        ));
    }

    for (index, conversation) in input.conversations.iter().enumerate() {
        let description = if conversation.current {
            format!(
                "The agent conversation of attempt {} in the current state `{}`.",
                conversation.attempt_id, conversation.state_id
            )
        } else {
            format!(
                "The agent conversation of attempt {} in the earlier state `{}`.",
                conversation.attempt_id, conversation.state_id
            )
        };
        files.push(ContextFile::new(
            format!(
                "conversations/{:03}-{}-{}.md",
                index + 1,
                file_safe(&conversation.state_id),
                file_safe(&conversation.attempt_id)
            ),
            description,
            conversation.text.clone(),
        ));
    }

    if !input.chat.is_empty() {
        let mut text = String::new();
        for entry in &input.chat {
            let author = serde_json::to_value(entry.author)
                .ok()
                .and_then(|author| author.as_str().map(str::to_string))
                .unwrap_or_default();
            let _ = writeln!(text, "- #{} {author}: {}", entry.sequence, entry.text);
        }
        files.push(ContextFile::new(
            "chat.md",
            "The chat between the operator and the conductor, oldest first.",
            text,
        ));
    }

    for artifact in &input.artifacts {
        files.push(ContextFile::new(
            format!("artifacts/{}", file_safe(&artifact.name)),
            format!(
                "The workflow artifact `{}` ({}).",
                artifact.name, artifact.location
            ),
            artifact.content.clone(),
        ));
    }

    files
}

/// `name` as one file-name component: anything but letters, digits, `-`,
/// `_` and `.` becomes `_`, and it never starts with `.`.
fn file_safe(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    match safe.strip_prefix('.') {
        Some(rest) => format!("_{rest}"),
        None if safe.is_empty() => "_".to_string(),
        None => safe,
    }
}

/// The agent attempts of the execution, from its events, oldest first:
/// `(state, attempt)`.
fn attempts(events: &[Event]) -> Vec<(&str, &str)> {
    events
        .iter()
        .filter_map(|event| match &event.data {
            EventData::ActionStarted {
                action_execution_id,
                state_id,
                ..
            }
            | EventData::OutputReprompted {
                action_execution_id,
                state_id,
                ..
            } => Some((state_id.as_str(), action_execution_id.as_str())),
            _ => None,
        })
        .collect()
}

/// The conversation of every attempt that has one, oldest first, read
/// whole.
fn conversations(
    store: &ConversationStore,
    current_state: Option<&str>,
    events: &[Event],
) -> Vec<AttemptConversation> {
    attempts(events)
        .into_iter()
        .filter_map(|(state_id, attempt_id)| {
            let snapshot = store.read(attempt_id, 0, usize::MAX).ok().flatten()?;
            let mut text = String::new();
            for entry in &snapshot.entries {
                let source = match entry.source {
                    ConversationSource::Narsor => "narsor",
                    ConversationSource::Stdout => "stdout",
                    ConversationSource::Stderr => "stderr",
                };
                let _ = writeln!(text, "[{source}] {}", String::from_utf8_lossy(&entry.raw));
            }
            Some(AttemptConversation {
                state_id: state_id.to_string(),
                attempt_id: attempt_id.to_string(),
                current: Some(state_id) == current_state,
                text,
            })
        })
        .collect()
}

/// The final text of the current state's latest attempt, as the step read
/// it (its `AgentMessage`).
fn last_agent_answer(current_state: Option<&str>, events: &[Event]) -> Option<String> {
    let current_state = current_state?;
    let (_, attempt) = attempts(events)
        .into_iter()
        .rev()
        .find(|(state, _)| *state == current_state)?;
    events.iter().rev().find_map(|event| match &event.data {
        EventData::AgentMessage {
            content,
            action_execution_id: Some(id),
            ..
        } if id == attempt && !content.trim().is_empty() => Some(content.clone()),
        _ => None,
    })
}

/// The workflow's artifact files, read whole.
fn artifact_files(execution: &WorkflowExecution) -> Vec<ArtifactFile> {
    // Artifacts live in the primary repository, which `path` is.
    let workspace = Path::new(&execution.workspace.path);
    let mut artifacts: Vec<_> = execution.context.artifacts.iter().collect();
    artifacts.sort_by(|a, b| a.0.cmp(b.0));

    let mut files = Vec::new();
    for (name, artifact) in artifacts {
        let location = Path::new(&artifact.location);
        let path = if location.is_absolute() {
            location.to_path_buf()
        } else {
            workspace.join(location)
        };
        if !path.is_file() {
            continue;
        }
        let content = match fs::read(&path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) => format!("(unreadable: {error})"),
        };
        files.push(ArtifactFile {
            name: name.clone(),
            location: artifact.location.clone(),
            content,
        });
    }
    files
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

fn is_empty_json(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Object(map) => map.is_empty(),
        _ => false,
    }
}
