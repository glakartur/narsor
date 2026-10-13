//! What the conductor is shown of an execution, rebuilt at every invocation
//! (R27), within a byte budget.
//!
//! # Priorities
//!
//! A long task has more history than any prompt should carry, so the
//! snapshot is built by priority (the plan's "Migawka przy długich
//! taskach"):
//!
//! * **whole, always**: the execution's status and current state, why the
//!   conductor is invoked (the request, the chat message it answers), the
//!   pending input with its problem record, its annotation, and the pinned
//!   workflow definition (D48);
//! * **shortened**: the context's variables, the last events with their
//!   payloads cut short, the tails of the agent conversations — the current
//!   state's attempts up to their limit, only the last attempt of each
//!   earlier state — the last chat entries, and, for a conductor isolated
//!   from the workspace (KTD5), the workflow's artifact files.
//!
//! Going over the budget is not an error. Every limit of the shortened
//! sections is halved until the snapshot fits, and past the last halving
//! they are left out; the whole sections are never cut, even when they
//! alone exceed the budget.
//!
//! [`SnapshotInput`] is plain data, gathered from disk once by
//! [`SnapshotInput::collect`], so the budgeting ([`render`]) is a pure
//! function that can be tested without an execution.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::conductor::store::{Annotation, ChatEntry, ConductorStore, QueuedRequest};
use crate::domain::{Event, EventData, OutputDefinition, WorkflowExecution};
use crate::engine::actions::human::PendingHumanInput;
use crate::persistence::{ConversationSource, ConversationStore, ExecutionStore};

/// The limits of one snapshot. The numbers are the starting point, not a
/// contract: they are halved as often as it takes to fit `total_bytes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotBudget {
    /// The most the whole snapshot should take.
    pub total_bytes: usize,
    /// How many of the last events are shown.
    pub events: usize,
    /// How much of one event's payload is shown.
    pub event_payload_bytes: usize,
    /// How much of the current state's conversations is shown, all its
    /// attempts together, the latest first.
    pub current_conversation_bytes: usize,
    /// How much of the last attempt of each earlier state is shown.
    pub earlier_conversation_bytes: usize,
    /// How many earlier states have their last attempt shown.
    pub earlier_states: usize,
    /// How many of the last chat entries are shown.
    pub chat_entries: usize,
    /// How much of one chat entry is shown.
    pub chat_entry_bytes: usize,
    /// How much of the context's variables is shown.
    pub variables_bytes: usize,
    /// How much of the workflow's artifact files is shown, all together
    /// (only for a conductor isolated from the workspace).
    pub artifact_bytes: usize,
}

impl Default for SnapshotBudget {
    fn default() -> Self {
        Self {
            total_bytes: 160 * 1024,
            events: 60,
            event_payload_bytes: 600,
            current_conversation_bytes: 48 * 1024,
            earlier_conversation_bytes: 6 * 1024,
            earlier_states: 6,
            chat_entries: 20,
            chat_entry_bytes: 2 * 1024,
            variables_bytes: 8 * 1024,
            artifact_bytes: 32 * 1024,
        }
    }
}

impl SnapshotBudget {
    /// The budget with every shortened limit divided by `2^level`; `None`
    /// past the last level, where the shortened sections are left out.
    fn at_level(&self, level: u32) -> Option<Self> {
        if level > MAX_LEVEL {
            return None;
        }
        let bytes = |value: usize| value >> level;
        let count = |value: usize| value >> level;
        Some(Self {
            total_bytes: self.total_bytes,
            events: count(self.events),
            event_payload_bytes: bytes(self.event_payload_bytes).max(80),
            current_conversation_bytes: bytes(self.current_conversation_bytes),
            earlier_conversation_bytes: bytes(self.earlier_conversation_bytes),
            earlier_states: count(self.earlier_states),
            chat_entries: count(self.chat_entries),
            chat_entry_bytes: bytes(self.chat_entry_bytes).max(80),
            variables_bytes: bytes(self.variables_bytes),
            artifact_bytes: bytes(self.artifact_bytes),
        })
    }
}

/// How many times the shortened limits are halved before they are left out.
const MAX_LEVEL: u32 = 8;

/// The conversation of one attempt, as far as it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationTail {
    /// The state the attempt ran in.
    pub state_id: String,
    /// The attempt.
    pub attempt_id: String,
    /// Whether it ran in the execution's current state.
    pub current: bool,
    /// Its lines, oldest first, each prefixed with its source.
    pub text: String,
}

/// An artifact file of the workflow, as far as it was read.
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
    pub state_outputs: Vec<OutputDefinition>,
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
    /// The execution's events, oldest first.
    pub events: Vec<Event>,
    /// The conversations, the current state's first, latest first within.
    pub conversations: Vec<ConversationTail>,
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
            events: Vec::new(),
            conversations: Vec::new(),
            chat: Vec::new(),
            artifacts: Vec::new(),
        }
    }
}

/// A snapshot, rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The text the conductor is shown.
    pub text: String,
    /// How often the shortened limits were halved to fit; `None` when the
    /// shortened sections had to be left out.
    pub level: Option<u32>,
}

impl SnapshotInput {
    /// Gathers what the snapshot of `execution` is made of.
    ///
    /// Reads the most it could show at the starting `budget` and no more;
    /// anything that cannot be read becomes a line saying so rather than a
    /// failed invocation — a partial snapshot is still worth showing.
    pub fn collect(
        store: &ExecutionStore,
        conductor: &ConductorStore,
        execution: &WorkflowExecution,
        definition: Result<(String, Vec<OutputDefinition>), String>,
        budget: &SnapshotBudget,
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
        let conversations = conversation_tails(
            &store.conversation_store(&execution.id),
            execution.current_state.as_deref(),
            &events,
            budget,
        );

        let mut chat = conductor.read_chat().unwrap_or_default();
        let keep_from = chat.len().saturating_sub(budget.chat_entries);
        chat.drain(..keep_from);

        let artifacts = if with_artifacts {
            artifact_files(execution, budget.artifact_bytes)
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
            events,
            conversations,
            chat,
            artifacts,
        }
    }
}

/// Renders `input` within `budget` (see the module documentation).
pub fn render(input: &SnapshotInput, budget: &SnapshotBudget) -> Snapshot {
    let whole = render_whole(input);
    let mut level = 0;
    while let Some(limits) = budget.at_level(level) {
        let text = format!("{whole}{}", render_shortened(input, &limits));
        if text.len() <= budget.total_bytes {
            return Snapshot {
                text,
                level: Some(level),
            };
        }
        level += 1;
    }
    Snapshot {
        text: format!(
            "{whole}\n## Omitted\n\nThe events, conversations, chat and artifacts did not fit \
             the snapshot and were left out.\n"
        ),
        level: None,
    }
}

/// The sections shown whole, always.
fn render_whole(input: &SnapshotInput) -> String {
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

    let _ = writeln!(text, "\n## Workflow definition\n");
    match &input.definition {
        Ok(source) => {
            let _ = writeln!(text, "```yaml\n{}\n```", source.trim_end());
        }
        Err(error) => {
            let _ = writeln!(text, "(unavailable: {error})");
        }
    }

    text
}

/// The sections shortened to `limits`.
fn render_shortened(input: &SnapshotInput, limits: &SnapshotBudget) -> String {
    let mut text = String::new();

    if limits.variables_bytes > 0 && !is_empty_json(&input.variables) {
        let _ = writeln!(text, "\n## Context variables\n");
        let _ = writeln!(
            text,
            "```json\n{}\n```",
            head(&pretty(&input.variables), limits.variables_bytes)
        );
    }

    if limits.events > 0 && !input.events.is_empty() {
        let skipped = input.events.len().saturating_sub(limits.events);
        let _ = writeln!(text, "\n## Recent events\n");
        if skipped > 0 {
            let _ = writeln!(text, "({skipped} earlier events not shown)");
        }
        for event in &input.events[skipped..] {
            let payload = serde_json::to_string(&event.data).unwrap_or_default();
            let _ = writeln!(
                text,
                "- #{} {} {}",
                event.sequence,
                event.timestamp.format("%Y-%m-%dT%H:%M:%SZ"),
                head(&payload, limits.event_payload_bytes)
            );
        }
    }

    let conversations = render_conversations(&input.conversations, limits);
    if !conversations.is_empty() {
        let _ = writeln!(text, "\n## Agent conversations\n");
        text.push_str(&conversations);
    }

    if limits.chat_entries > 0 && !input.chat.is_empty() {
        let skipped = input.chat.len().saturating_sub(limits.chat_entries);
        let _ = writeln!(text, "\n## Recent chat\n");
        for entry in &input.chat[skipped..] {
            let author = serde_json::to_value(entry.author)
                .ok()
                .and_then(|author| author.as_str().map(str::to_string))
                .unwrap_or_default();
            let _ = writeln!(
                text,
                "- #{} {author}: {}",
                entry.sequence,
                head(&entry.text, limits.chat_entry_bytes)
            );
        }
    }

    if limits.artifact_bytes > 0 && !input.artifacts.is_empty() {
        let _ = writeln!(text, "\n## Workflow artifacts\n");
        let mut room = limits.artifact_bytes;
        for artifact in &input.artifacts {
            let content = head(&artifact.content, room);
            room = room.saturating_sub(content.len());
            let _ = writeln!(
                text,
                "### {} ({})\n\n```\n{}\n```",
                artifact.name, artifact.location, content
            );
            if room == 0 {
                break;
            }
        }
    }

    text
}

fn render_conversations(conversations: &[ConversationTail], limits: &SnapshotBudget) -> String {
    let mut text = String::new();

    let mut room = limits.current_conversation_bytes;
    for conversation in conversations.iter().filter(|tail| tail.current) {
        if room == 0 {
            break;
        }
        let shown = tail(&conversation.text, room);
        room = room.saturating_sub(shown.len());
        let _ = writeln!(
            text,
            "### {} (current state), attempt {}\n\n```\n{shown}\n```",
            conversation.state_id, conversation.attempt_id
        );
    }

    if limits.earlier_conversation_bytes > 0 {
        for conversation in conversations
            .iter()
            .filter(|tail| !tail.current)
            .take(limits.earlier_states)
        {
            let shown = tail(&conversation.text, limits.earlier_conversation_bytes);
            let _ = writeln!(
                text,
                "### {}, last attempt {}\n\n```\n{shown}\n```",
                conversation.state_id, conversation.attempt_id
            );
        }
    }

    text
}

/// The attempts to show, read from their conversation files: every attempt
/// of the current state (latest first), then the last attempt of each
/// earlier state (latest state first).
fn conversation_tails(
    store: &ConversationStore,
    current_state: Option<&str>,
    events: &[Event],
    budget: &SnapshotBudget,
) -> Vec<ConversationTail> {
    let attempts: Vec<(&str, &str)> = events
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
        .collect();

    let read = |state_id: &str, attempt_id: &str, current: bool, limit: usize| {
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
        Some(ConversationTail {
            state_id: state_id.to_string(),
            attempt_id: attempt_id.to_string(),
            current,
            text: tail(&text, limit).to_string(),
        })
    };

    let mut tails = Vec::new();
    let mut room = budget.current_conversation_bytes;
    for (state_id, attempt_id) in attempts.iter().rev() {
        if Some(*state_id) != current_state || room == 0 {
            continue;
        }
        if let Some(conversation) = read(state_id, attempt_id, true, room) {
            room = room.saturating_sub(conversation.text.len());
            tails.push(conversation);
        }
    }

    let mut seen = BTreeSet::new();
    for (state_id, attempt_id) in attempts.iter().rev() {
        if Some(*state_id) == current_state || !seen.insert(*state_id) {
            continue;
        }
        if seen.len() > budget.earlier_states {
            break;
        }
        if let Some(conversation) = read(
            state_id,
            attempt_id,
            false,
            budget.earlier_conversation_bytes,
        ) {
            tails.push(conversation);
        }
    }
    tails
}

/// The workflow's artifact files, read from the start up to `limit` bytes
/// in all — a plan is read from its beginning.
fn artifact_files(execution: &WorkflowExecution, limit: usize) -> Vec<ArtifactFile> {
    let workspace = Path::new(&execution.workspace.path);
    let mut artifacts: Vec<_> = execution.context.artifacts.iter().collect();
    artifacts.sort_by(|a, b| a.0.cmp(b.0));

    let mut room = limit;
    let mut files = Vec::new();
    for (name, artifact) in artifacts {
        if room == 0 {
            break;
        }
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
            Ok(bytes) => head(&String::from_utf8_lossy(&bytes), room).to_string(),
            Err(error) => format!("(unreadable: {error})"),
        };
        room = room.saturating_sub(content.len());
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

/// The first `max` bytes of `text`, on a character boundary, marked when
/// cut.
fn head(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// The last `max` bytes of `text`, on a character boundary, marked when
/// cut.
fn tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut start = text.len() - max;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}
