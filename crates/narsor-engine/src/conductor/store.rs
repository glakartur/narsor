//! The conductor's own durable record of one execution (KTD2).
//!
//! ```text
//! {state}/executions/{execution_id}/conductor/
//!     state.json                    -- ConductorState, versioned, atomic
//!     chat.jsonl                    -- one ChatEntry per line, append-only
//!     annotations/{key}.json        -- Annotation of one human gate (R30)
//! ```
//!
//! # Why beside the execution, not inside its `state.json`
//!
//! The execution's own `state.json` is written only by the engine pass that
//! holds the claim, and that pass keeps the whole `WorkflowExecution` in
//! memory and saves it whole: anything written next to it by someone else
//! is lost at the pass's next save. The conductor works while no pass runs
//! (advisory invocations, KTD10) and while one does (F1), so it keeps its
//! state in files nobody else writes.
//!
//! It lives under the *state* root, next to the event log, rather than in
//! the task's work tree: the work tree is disposable scratch that the
//! workspace sweep deletes once an execution finishes, and the chat history
//! is meant to outlive that (R26).
//!
//! # Concurrency
//!
//! Every write goes through the execution's one actor
//! ([`crate::conductor::ConductorActor`]), which serializes them; this type
//! itself holds no lock. Reads are safe from anywhere: `state.json` is
//! replaced atomically, and a reader of `chat.jsonl` ignores a last line that
//! is not yet complete.
//!
//! # Old executions
//!
//! An execution from before the conductor existed has no `conductor/`
//! directory. That is an empty state, not an error: every read answers
//! with the default, and the directory is created by the first write.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::conductor::response::ProposedAction;
use crate::domain::{
    ConductorDecisionKind, ConductorPurpose, ConductorTrigger, Confidence, ExecutionId,
};
use crate::engine::RejectedAnswer;
use crate::persistence::atomic::ensure_dir;
use crate::persistence::{read_json, write_json_atomic, PersistenceError, Result, StatePaths};

/// The conductor's state file, inside its directory
/// ([`StatePaths::conductor_dir`]).
pub const CONDUCTOR_STATE_FILE: &str = "state.json";

/// The conductor's chat history, inside its directory.
pub const CHAT_FILE: &str = "chat.jsonl";

/// The version of `state.json` this binary writes and reads.
pub const CONDUCTOR_STATE_VERSION: u32 = 1;

/// Everything the conductor keeps about one execution between invocations.
///
/// Every field is `#[serde(default)]`, so later units can add to it without
/// a version bump; the version only moves for a change an older binary
/// would misread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConductorState {
    /// The format of this file; see [`CONDUCTOR_STATE_VERSION`].
    pub version: u32,

    /// The invocation under way, if one is: written before the conductor
    /// is invoked and cleared when it ends, so a restart knows it was
    /// interrupted (the daemon restart matrix).
    #[serde(default)]
    pub pending: Option<PendingInvocation>,

    /// Requests waiting their turn, in the order they were accepted — every
    /// kind but chat, whose requests are the unanswered messages of
    /// `chat.jsonl` (KTD11).
    #[serde(default)]
    pub queue: Vec<QueuedRequest>,

    /// How many times the conductor was invoked for an automatic trigger,
    /// per episode (R24). Counted *before* each invocation, so an attempt a
    /// restart cut short still counts.
    #[serde(default)]
    pub invocation_attempts: BTreeMap<String, u32>,

    /// The episodes whose one retry of a failed step was used (R21, KTD14).
    #[serde(default)]
    pub used_retries: BTreeSet<String>,

    /// Decisions saved before they were applied (KTD13): a restart applies
    /// them instead of asking the conductor again.
    #[serde(default)]
    pub unapplied_decisions: Vec<SavedDecision>,

    /// What the conductor concluded about the latest failed step it took up
    /// (F3). It is the operator's problem while the execution is still
    /// failed on the same attempt (KTD4).
    #[serde(default)]
    pub failure: Option<FailureReport>,
}

impl Default for ConductorState {
    fn default() -> Self {
        Self {
            version: CONDUCTOR_STATE_VERSION,
            pending: None,
            queue: Vec::new(),
            invocation_attempts: BTreeMap::new(),
            used_retries: BTreeSet::new(),
            unapplied_decisions: Vec::new(),
            failure: None,
        }
    }
}

impl ConductorState {
    /// The number of invocations already counted for `episode`.
    pub fn attempts(&self, episode: &str) -> u32 {
        self.invocation_attempts.get(episode).copied().unwrap_or(0)
    }

    /// Whether the one retry of `episode` was used.
    pub fn retry_used(&self, episode: &str) -> bool {
        self.used_retries.contains(episode)
    }
}

/// The marker of an invocation under way.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingInvocation {
    /// The invocation, as `ConductorStarted` names it.
    pub invocation_id: String,
    /// The request it serves.
    pub request: QueuedRequest,
    /// When it started.
    pub started_at: DateTime<Utc>,
}

/// One request for the conductor, as it waits in the queue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueuedRequest {
    /// The request itself; for chat, the id of the message it answers.
    pub request_id: String,
    /// What asked.
    pub trigger: ConductorTrigger,
    /// What the conductor is to do.
    pub purpose: ConductorPurpose,
    /// The episode it is about (KTD9), when it is about one.
    #[serde(default)]
    pub episode: Option<String>,
    /// Whatever the trigger passes on (the failed state, the gate's
    /// request, ...), for the invocation to read.
    #[serde(default)]
    pub payload: Value,
    /// When the request was accepted.
    pub queued_at: DateTime<Utc>,
}

impl QueuedRequest {
    /// A new request with a fresh id.
    pub fn new(trigger: ConductorTrigger, purpose: ConductorPurpose) -> Self {
        Self {
            request_id: Uuid::new_v4().to_string(),
            trigger,
            purpose,
            episode: None,
            payload: Value::Null,
            queued_at: Utc::now(),
        }
    }

    /// The same request, about `episode`.
    pub fn with_episode(mut self, episode: impl Into<String>) -> Self {
        self.episode = Some(episode.into());
        self
    }

    /// The same request, carrying `payload`.
    pub fn with_payload(mut self, payload: Value) -> Self {
        self.payload = payload;
        self
    }

    /// The request that answers the operator's chat message `entry`.
    pub fn for_chat(entry: &ChatEntry) -> Self {
        Self {
            request_id: entry.id.clone(),
            trigger: ConductorTrigger::Chat,
            purpose: ConductorPurpose::ReplyToChat,
            episode: None,
            payload: Value::Null,
            queued_at: entry.at,
        }
    }
}

/// A decision of the conductor, saved before it is applied (KTD13).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedDecision {
    /// The decision's id, as `ConductorDecision` names it.
    pub decision_id: String,
    /// What was decided.
    pub kind: ConductorDecisionKind,
    /// The episode it belongs to.
    #[serde(default)]
    pub episode: Option<String>,
    /// The state it is about.
    #[serde(default)]
    pub state_id: Option<String>,
    /// The output label chosen, if one was.
    #[serde(default)]
    pub output_label: Option<String>,
    /// The step's `data` the conductor supplied.
    #[serde(default)]
    pub data: Value,
    /// The step's `variables` the conductor supplied.
    #[serde(default)]
    pub variables: Value,
    /// How sure it was.
    #[serde(default)]
    pub confidence: Confidence,
    /// Why.
    #[serde(default)]
    pub rationale: Option<String>,
    /// The execution's context before the decision, to restore on undo.
    #[serde(default)]
    pub context_before: Value,
    /// The agent's answers without a label the decision settled, oldest
    /// first: an undo raises the problem again with them (KTD13).
    #[serde(default)]
    pub answers: Vec<RejectedAnswer>,
    /// Why the step had no label, for the problem an undo raises again.
    #[serde(default)]
    pub reason: Option<String>,
    /// The step's reply from the answers (R3), for the problem an undo
    /// raises again.
    #[serde(default)]
    pub reply: Option<String>,
    /// The execution's entry counts (R10) before the decision, to restore
    /// on undo: its transition counted an entry into the target.
    #[serde(default)]
    pub state_entries_before: Value,
    /// The steps' entries, attempts and `local.*` values (KTD8) before the
    /// decision, to restore on undo: it recorded an attempt.
    #[serde(default)]
    pub step_memory_before: Value,
    /// When it was saved.
    pub saved_at: DateTime<Utc>,
}

/// What the conductor concluded about one failed step (F3, U11).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FailureReport {
    /// The failure it is about: its last failed attempt (KTD9).
    pub key: String,
    /// The failure's episode, the entry into the state (KTD14).
    pub episode: String,
    /// The state that failed.
    pub state_id: String,
    /// What the problem says: the diagnosis, or the raw error.
    pub reason: String,
    /// How the conductor concluded.
    pub outcome: FailureOutcome,
    /// When.
    pub concluded_at: DateTime<Utc>,
}

impl FailureReport {
    /// Whether the report leaves a problem for the operator (KTD4) — every
    /// outcome but the conductor's own retry.
    pub fn is_problem(&self) -> bool {
        !matches!(self.outcome, FailureOutcome::Retried { .. })
    }
}

/// How the conductor concluded about a failed step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FailureOutcome {
    /// It diagnosed the failure, and proposed what to do (R22).
    Diagnosed {
        /// What went wrong, as it sees it.
        diagnosis: String,
        /// What it proposes, for the operator to confirm (R28).
        #[serde(default)]
        proposals: Vec<ProposedAction>,
    },
    /// It restarted the step once, with `note` (R21).
    Retried {
        /// The note the restarted step was given.
        note: String,
    },
    /// It could not diagnose it (R24): the operator gets the raw error.
    Unanswered {
        /// Why.
        error: String,
    },
}

/// Who wrote a chat entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatAuthor {
    /// A person, through the API.
    Operator,
    /// The conductor.
    Conductor,
}

/// What a chat entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatEntryKind {
    /// Something the operator or the conductor said.
    Message,
    /// The conductor could not answer (KTD12): the entry says why, and that
    /// is all that happens.
    Error,
    /// A decision the conductor took (R31).
    Decision,
    /// A proposal waiting for the operator's confirmation (R28).
    Proposal,
}

/// One line of `chat.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatEntry {
    /// The entry's own id.
    pub id: String,
    /// Its position in the history, from 1, in the order it was accepted.
    pub sequence: u64,
    /// When it was written.
    pub at: DateTime<Utc>,
    /// Who wrote it.
    pub author: ChatAuthor,
    /// What it is.
    pub kind: ChatEntryKind,
    /// Its text.
    pub text: String,
    /// The operator message this entry answers, if it answers one.
    #[serde(default)]
    pub in_reply_to: Option<String>,
    /// Structured content of a decision or a proposal.
    #[serde(default)]
    pub payload: Option<Value>,
}

impl ChatEntry {
    /// An entry not yet written: its sequence is assigned by
    /// [`ConductorStore::append_chat`].
    pub fn new(author: ChatAuthor, kind: ChatEntryKind, text: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            sequence: 0,
            at: Utc::now(),
            author,
            kind,
            text: text.into(),
            in_reply_to: None,
            payload: None,
        }
    }

    /// The same entry, as an answer to `message_id`.
    pub fn replying_to(mut self, message_id: impl Into<String>) -> Self {
        self.in_reply_to = Some(message_id.into());
        self
    }

    /// Whether this is a message from the operator the conductor owes an
    /// answer to.
    pub fn asks_for_reply(&self) -> bool {
        self.author == ChatAuthor::Operator && self.kind == ChatEntryKind::Message
    }
}

/// The conductor's summary of one `human` gate (R30).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Annotation {
    /// The pending input it annotates (its episode key).
    pub key: String,
    /// The summary.
    pub summary: String,
    /// The answer the conductor would give, if it would give one.
    #[serde(default)]
    pub recommendation: Option<String>,
    /// When it was written.
    pub created_at: DateTime<Utc>,
}

/// The conductor's files of one execution.
#[derive(Debug, Clone)]
pub struct ConductorStore {
    execution_id: ExecutionId,
    dir: PathBuf,
    annotations_dir: PathBuf,
}

impl ConductorStore {
    /// The store of `execution_id` under the state root `paths`.
    pub fn new(paths: &StatePaths, execution_id: &str) -> Self {
        Self {
            execution_id: execution_id.to_string(),
            dir: paths.conductor_dir(execution_id),
            annotations_dir: paths.conductor_annotations_dir(execution_id),
        }
    }

    /// The execution this store belongs to.
    pub fn execution_id(&self) -> &str {
        &self.execution_id
    }

    /// `{execution}/conductor`
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether anything was ever written for this execution.
    pub fn exists(&self) -> bool {
        self.dir.is_dir()
    }

    fn state_file(&self) -> PathBuf {
        self.dir.join(CONDUCTOR_STATE_FILE)
    }

    fn chat_file(&self) -> PathBuf {
        self.dir.join(CHAT_FILE)
    }

    fn annotation_file(&self, key: &str) -> PathBuf {
        self.annotations_dir.join(format!("{key}.json"))
    }

    /// The conductor's state; the default when nothing was written yet.
    ///
    /// A file of another version is an error rather than a guess: a newer
    /// binary may have written something this one would misread.
    pub fn load_state(&self) -> Result<ConductorState> {
        let path = self.state_file();
        let state: ConductorState = match read_json(&path) {
            Ok(state) => state,
            Err(PersistenceError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
                return Ok(ConductorState::default());
            }
            Err(error) => return Err(error),
        };
        if state.version != CONDUCTOR_STATE_VERSION {
            return Err(PersistenceError::Deserialize {
                path,
                source: serde::de::Error::custom(format!(
                    "unsupported conductor state version {} (expected {CONDUCTOR_STATE_VERSION})",
                    state.version
                )),
            });
        }
        Ok(state)
    }

    /// Replaces the conductor's state atomically.
    pub fn save_state(&self, state: &ConductorState) -> Result<()> {
        write_json_atomic(&self.state_file(), state)
    }

    /// Appends `entry` to the chat history as its next line, and returns it
    /// with the sequence it got.
    ///
    /// Not safe against a concurrent writer of the same file — that is what
    /// the actor is for. The line goes out in one `write` of a file opened
    /// for appending and is synced before this returns, so a crash leaves at
    /// worst an incomplete last line, which every read skips. That torn
    /// fragment is cut off before the next line goes out; appended to as it
    /// is, it would fuse with the new line into one that never parses.
    pub fn append_chat(&self, mut entry: ChatEntry) -> Result<ChatEntry> {
        let path = self.chat_file();
        entry.sequence = self.last_chat_sequence()? + 1;
        drop_torn_tail(&path)?;

        let mut line =
            serde_json::to_vec(&entry).map_err(|source| PersistenceError::Serialize {
                path: path.clone(),
                source,
            })?;
        line.push(b'\n');

        ensure_dir(&self.dir)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| PersistenceError::io(&path, source))?;
        file.write_all(&line)
            .map_err(|source| PersistenceError::io(&path, source))?;
        file.sync_data()
            .map_err(|source| PersistenceError::io(&path, source))?;
        Ok(entry)
    }

    /// Every chat entry, in order; empty when there is no history.
    pub fn read_chat(&self) -> Result<Vec<ChatEntry>> {
        let path = self.chat_file();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(PersistenceError::io(&path, source)),
        };

        let mut entries = Vec::new();
        let mut lines = bytes.split(|byte| *byte == b'\n').peekable();
        while let Some(line) = lines.next() {
            let is_last = lines.peek().is_none();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_slice::<ChatEntry>(line) {
                Ok(entry) => entries.push(entry),
                // The line a crash cut short: never a complete entry.
                Err(_) if is_last => break,
                Err(source) => return Err(PersistenceError::Deserialize { path, source }),
            }
        }
        Ok(entries)
    }

    /// The chat entries after sequence `after`.
    pub fn read_chat_after(&self, after: u64) -> Result<Vec<ChatEntry>> {
        Ok(self
            .read_chat()?
            .into_iter()
            .filter(|entry| entry.sequence > after)
            .collect())
    }

    /// The operator messages no entry answers yet, oldest first (KTD11).
    pub fn unanswered_chat(&self) -> Result<Vec<ChatEntry>> {
        Ok(unanswered(self.read_chat()?))
    }

    fn last_chat_sequence(&self) -> Result<u64> {
        Ok(self
            .read_chat()?
            .last()
            .map(|entry| entry.sequence)
            .unwrap_or(0))
    }

    /// Writes the annotation of one gate, replacing an older one.
    pub fn save_annotation(&self, annotation: &Annotation) -> Result<()> {
        write_json_atomic(&self.annotation_file(&annotation.key), annotation)
    }

    /// The annotation of the gate `key`, if there is one.
    pub fn load_annotation(&self, key: &str) -> Result<Option<Annotation>> {
        match read_json(&self.annotation_file(key)) {
            Ok(annotation) => Ok(Some(annotation)),
            Err(PersistenceError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
}

/// Truncates the file at `path` to just after its last newline, dropping the
/// incomplete line a crash may have left; a missing file or one that already
/// ends with a newline is left alone.
fn drop_torn_tail(path: &Path) -> Result<()> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(PersistenceError::io(path, source)),
    };
    if bytes.last().is_none_or(|byte| *byte == b'\n') {
        return Ok(());
    }
    let keep = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |newline| newline + 1);

    // A last line that parses is an entry every read already returns; only
    // its newline is missing. Ending it keeps the entry rather than losing it.
    if serde_json::from_slice::<ChatEntry>(&bytes[keep..]).is_ok() {
        let mut file = OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(|source| PersistenceError::io(path, source))?;
        file.write_all(b"\n")
            .map_err(|source| PersistenceError::io(path, source))?;
        return file
            .sync_data()
            .map_err(|source| PersistenceError::io(path, source));
    }

    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|source| PersistenceError::io(path, source))?;
    file.set_len(keep as u64)
        .map_err(|source| PersistenceError::io(path, source))?;
    file.sync_data()
        .map_err(|source| PersistenceError::io(path, source))
}

/// The operator messages of `entries` that no later entry answers.
fn unanswered(entries: Vec<ChatEntry>) -> Vec<ChatEntry> {
    let answered: BTreeSet<&str> = entries
        .iter()
        .filter_map(|entry| entry.in_reply_to.as_deref())
        .collect();
    let open: Vec<ChatEntry> = entries
        .iter()
        .filter(|entry| entry.asks_for_reply() && !answered.contains(entry.id.as_str()))
        .cloned()
        .collect();
    open
}
