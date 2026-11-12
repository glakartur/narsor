//! *Rozmowa*: the conversation of one step of the task, picked on a strip of
//! steps (newest first, `←` / `→`), and the per-attempt cache that it and
//! *Przebieg* show a step from.
//!
//! Pages come through the control plane's relay
//! (`GET /control/v1/tasks/:id/attempts/:attempt/conversation`), which waits
//! for the live runner, so a read can fail with a reason — the failure lands
//! in the tab, not in the connection. An attempt is read once and then only
//! from the entry it stopped at ([`Attempts::from`]).
//!
//! Without a live runner, the daemon's own local backend serves the pages
//! from the task's storage: such a page says so ([`ExecutionEventsSource`])
//! and when the record was last written, and the tab and the step's
//! conversation carry a badge with that age ([`record_badge`]) — counted
//! from this terminal's clock at every draw. The entry numbers are the same
//! from either source, so the next read after the runner is back continues
//! from where the record stopped and the badge goes away by itself.
//!
//! Entries are the transcript model of [`crate::tui::transcript`]: every
//! chat element of an entry is projected by [`transcript_entries`] into its
//! own card, with the same sanitisation and the same raw view as the
//! conductor's chat. The colours come from [`Styles::chat_header`] and
//! [`Styles::chat_body`], the conversation scale of the role module.

use std::collections::BTreeMap;
use std::time::Instant;

use chrono::{DateTime, Utc};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::api::{ApiClient, ClientError, ConversationResponse as Page};
use crate::cli::task_text::RecordAge;
use crate::controller_protocol::{
    ConversationPageQuery, ExecutionEventsSource, CONVERSATION_READER_STARTING,
    CONVERSATION_RECORD_UNREADABLE, CONVERSATION_RECORD_UNREADABLE_PREFIX,
    CONVERSATION_UNAVAILABLE,
};
use crate::domain::{Event, EventData};
use narsor_protocol::api::{ConversationState, Position, PositionStatus};

use super::super::super::scroll::one_line;
use super::super::super::selection::{bar_prefix, PREFIX_WIDTH};
use super::super::detail::{cells, position_icon, rule_line, step_name};
use super::super::layout::first_row;
use super::READER_RETRY;
use crate::tui::conversation::transcript_entries;
use crate::tui::keymap::Action;
use crate::tui::style::{Role, Styles};
use crate::tui::transcript::{
    wrap_visible_lines, DisplayMode, TranscriptEntry, TranscriptEntryId, TranscriptId,
    TranscriptLine, TranscriptPresentation, TranscriptState,
};

/// Soft cap on the raw bytes of one relayed conversation page.
pub const PAGE_BYTE_BUDGET: u64 = 64 * 1024;

/// What reading an attempt's conversation brought.
#[derive(Debug, Clone)]
pub struct AttemptRead {
    /// The entries after the entry the read started from.
    pub entries: Vec<TranscriptEntry>,
    /// Where to continue from.
    pub next_entry: u64,
    /// Whether the attempt's conversation is closed.
    pub closed: bool,
    /// Whether the last line on disk was incomplete.
    pub truncated_tail: bool,
    /// Whether every readable page was read.
    pub complete: bool,
    /// Whether the runner holds no record of the attempt at all.
    pub no_record: bool,
    /// Where the last page was read from.
    pub source: ExecutionEventsSource,
    /// When the record was last written, for a page read from storage.
    pub recorded_at: Option<DateTime<Utc>>,
}

/// Reads the conversation of `attempt_id` from entry `from`, page after page
/// through the relay, up to the current end.
pub async fn read_attempt(
    client: &ApiClient,
    task_id: &str,
    attempt_id: &str,
    from: u64,
) -> Result<AttemptRead, ClientError> {
    read_pages(from, |cursor| async move {
        let query = ConversationPageQuery {
            from: cursor,
            byte_budget: Some(PAGE_BYTE_BUDGET),
        };
        // Through the relay, so the runner has to answer: not for longer
        // than the loop can wait.
        super::super::detail::bounded_read(
            super::super::detail::RELAYED_READ_LIMIT,
            client.control_conversation_page(task_id, attempt_id, &query),
        )
        .await
    })
    .await
}

/// The same for an execution from before the task scheduler (the Historia):
/// its conversation is read from the daemon's own storage.
pub async fn read_history_attempt(
    client: &ApiClient,
    execution_id: &str,
    attempt_id: &str,
    from: u64,
) -> Result<AttemptRead, ClientError> {
    read_pages(from, |cursor| async move {
        client
            .control_history_conversation(execution_id, attempt_id, cursor)
            .await
    })
    .await
}

/// Folds the pages `fetch` returns, from entry `from`, into one read.
async fn read_pages<F, Fut>(from: u64, mut fetch: F) -> Result<AttemptRead, ClientError>
where
    F: FnMut(u64) -> Fut,
    Fut: std::future::Future<Output = Result<Page, ClientError>>,
{
    let mut cursor = from;
    let mut entries = Vec::new();
    loop {
        match fetch(cursor).await? {
            Page::Recorded {
                entries: page,
                next_entry,
                closed,
                truncated_tail,
                source,
                recorded_at,
            } => {
                let empty = page.is_empty();
                entries.extend(page.iter().flat_map(transcript_entries));
                if empty || closed || truncated_tail || next_entry <= cursor {
                    return Ok(AttemptRead {
                        entries,
                        next_entry,
                        closed,
                        truncated_tail,
                        complete: !truncated_tail && (empty || closed),
                        no_record: false,
                        source: source.unwrap_or_default(),
                        recorded_at,
                    });
                }
                cursor = next_entry;
            }
            Page::NoRecord => {
                return Ok(AttemptRead {
                    entries: Vec::new(),
                    next_entry: 0,
                    closed: true,
                    truncated_tail: false,
                    complete: true,
                    no_record: true,
                    source: ExecutionEventsSource::default(),
                    recorded_at: None,
                });
            }
        }
    }
}

/// Why a read of an attempt's conversation failed, as the tab tells it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadFailure {
    /// The task is stopped and its read-only runner is not connected yet
    /// (D63): not a failure to show as one, the read is asked again shortly.
    Starting,
    /// No live runner can serve it and there is no record to read here (a
    /// remote task without its runner): the server's reason.
    Unavailable(String),
    /// The task's own record could not be read: what went wrong.
    RecordUnreadable(String),
    /// Anything else: the reason in words.
    Failed(String),
}

impl ReadFailure {
    /// The failure of `error`, whose words are `reason`.
    pub fn of(error: &ClientError, reason: String) -> Self {
        match error {
            ClientError::Refused { code, message } if code == CONVERSATION_RECORD_UNREADABLE => {
                let detail = message
                    .strip_prefix(CONVERSATION_RECORD_UNREADABLE_PREFIX)
                    .unwrap_or(message);
                Self::RecordUnreadable(one_line(detail))
            }
            ClientError::Refused { code, .. } if code == CONVERSATION_READER_STARTING => {
                Self::Starting
            }
            ClientError::Refused { code, .. } if code == CONVERSATION_UNAVAILABLE => {
                Self::Unavailable(reason)
            }
            _ => Self::Failed(reason),
        }
    }

    /// The reason alone.
    pub fn detail(&self) -> &str {
        match self {
            Self::Starting => "wczytywanie…",
            Self::Unavailable(detail) | Self::RecordUnreadable(detail) | Self::Failed(detail) => {
                detail
            }
        }
    }

    /// The note of the failure: its words and its role. `step` names the
    /// step a failed read was of, when the note stands for several.
    pub fn note(&self, step: Option<&str>) -> (String, Role) {
        match self {
            Self::Starting => (
                "◌ wczytywanie rozmowy… zadanie jest zatrzymane, czytelnik się uruchamia"
                    .to_string(),
                Role::Attention,
            ),
            Self::Unavailable(detail) => (
                format!("◌ rozmowa niedostępna: {}", one_line(detail)),
                Role::Attention,
            ),
            Self::RecordUnreadable(detail) => (
                format!("✗ nie udało się odczytać zapisu: {}", one_line(detail)),
                Role::Problem,
            ),
            Self::Failed(detail) => (
                match step {
                    Some(step) => format!(
                        "✗ nie udało się wczytać rozmowy ({}: {})",
                        one_line(step),
                        one_line(detail)
                    ),
                    None => format!("✗ nie udało się wczytać rozmowy: {}", one_line(detail)),
                },
                Role::Problem,
            ),
        }
    }
}

/// The badge of a conversation read from the task's record written at
/// `recorded_at`, seen `now`, fitted to `width` columns: "zapis sprzed 3 min",
/// or "zapis · 3 min" where that does not fit. Words, not a colour, so it
/// reads the same without colours.
pub fn record_badge(recorded_at: Option<DateTime<Utc>>, now: DateTime<Utc>, width: u16) -> String {
    let Some(at) = recorded_at else {
        return if usize::from(width) >= "rozmowa z zapisu".chars().count() {
            "rozmowa z zapisu".to_string()
        } else {
            "zapis".to_string()
        };
    };
    let (since, age) = match RecordAge::between(now, at) {
        RecordAge::JustNow => ("chwili".to_string(), "przed chwilą".to_string()),
        RecordAge::Minutes(minutes) => (format!("{minutes} min"), format!("{minutes} min")),
        RecordAge::Hours(hours) => (format!("{hours} h"), format!("{hours} h")),
        RecordAge::Days(days) => (format!("{days} d"), format!("{days} d")),
    };
    let wide = format!("zapis sprzed {since}");
    if wide.chars().count() <= usize::from(width) {
        wide
    } else {
        format!("zapis · {age}")
    }
}

/// One attempt's conversation as read so far.
#[derive(Debug, Clone)]
pub struct Attempt {
    entries: Vec<TranscriptEntry>,
    next_entry: u64,
    closed: bool,
    truncated_tail: bool,
    no_record: bool,
    source: ExecutionEventsSource,
    recorded_at: Option<DateTime<Utc>>,
    failure: Option<ReadFailure>,
    revision: Option<u64>,
    /// When a read told "the reader is starting" is asked again.
    retry_at: Option<Instant>,
    transcript: TranscriptState,
}

impl Attempt {
    fn new(attempt_id: &str) -> Self {
        Self {
            entries: Vec::new(),
            next_entry: 0,
            closed: false,
            truncated_tail: false,
            no_record: false,
            source: ExecutionEventsSource::default(),
            recorded_at: None,
            failure: None,
            revision: None,
            retry_at: None,
            transcript: TranscriptState::new(TranscriptId::from(attempt_id)),
        }
    }

    /// Why the last read failed.
    pub fn failure(&self) -> Option<&ReadFailure> {
        self.failure.as_ref()
    }

    /// Whether the last page read came from the task's record rather than
    /// the live runner.
    pub fn from_record(&self) -> bool {
        self.source == ExecutionEventsSource::Storage
    }

    /// When the record the last page came from was last written.
    pub fn recorded_at(&self) -> Option<DateTime<Utc>> {
        self.recorded_at
    }

    /// Whether the runner holds no record of it.
    pub fn no_record(&self) -> bool {
        self.no_record
    }

    /// Whether nothing was read.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many entries were read.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there is a conversation to read: the runner holds a record
    /// and at least one entry of it was read.
    pub fn has_conversation(&self) -> bool {
        !self.no_record && !self.entries.is_empty()
    }

    /// Whether the attempt's conversation is closed.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// The reader of this attempt, for keys and for drawing.
    pub fn transcript_mut(&mut self) -> &mut TranscriptState {
        &mut self.transcript
    }

    /// The reader of this attempt.
    pub fn transcript(&self) -> &TranscriptState {
        &self.transcript
    }
}

/// The attempts of a task read so far, by `action_execution_id`.
#[derive(Debug, Clone, Default)]
pub struct Attempts {
    map: BTreeMap<String, Attempt>,
}

impl Attempts {
    /// The attempt, once a read of it landed.
    pub fn get(&self, attempt_id: &str) -> Option<&Attempt> {
        self.map.get(attempt_id)
    }

    /// The attempt, to draw or scroll it.
    pub fn get_mut(&mut self, attempt_id: &str) -> Option<&mut Attempt> {
        self.map.get_mut(attempt_id)
    }

    /// Whether the attempt must be read for a task at `revision`.
    pub fn needs_read(&self, attempt_id: &str, revision: u64) -> bool {
        self.needs_read_at(attempt_id, revision, Instant::now())
    }

    /// [`Self::needs_read`] as of `now`: a read told "the reader is
    /// starting" is due again once [`READER_RETRY`] has passed, whatever the
    /// revision.
    pub fn needs_read_at(&self, attempt_id: &str, revision: u64, now: Instant) -> bool {
        self.map.get(attempt_id).is_none_or(|attempt| {
            if attempt.failure == Some(ReadFailure::Starting) {
                return attempt.retry_at.is_none_or(|at| now >= at);
            }
            // A closed conversation that was read whole never changes —
            // but one read from the record is read again once the runner
            // may be back, so that its badge goes.
            let settled = attempt.failure.is_none()
                && !attempt.from_record()
                && (attempt.no_record || (attempt.closed && !attempt.truncated_tail));
            !settled && attempt.revision != Some(revision)
        })
    }

    /// The entry a read of the attempt continues from.
    pub fn from(&self, attempt_id: &str) -> u64 {
        self.map
            .get(attempt_id)
            .map_or(0, |attempt| attempt.next_entry)
    }

    /// Makes every attempt due again, keeping what is shown.
    pub fn invalidate(&mut self) {
        for attempt in self.map.values_mut() {
            attempt.revision = None;
        }
    }

    /// Takes a read in. A read that started `from` an entry adds to what was
    /// read; one from the start replaces it. The reader keeps its place.
    pub fn apply(
        &mut self,
        attempt_id: &str,
        revision: u64,
        from: u64,
        result: Result<AttemptRead, ReadFailure>,
    ) {
        self.apply_at(attempt_id, revision, from, result, Instant::now());
    }

    /// [`Self::apply`] as of `now`.
    pub fn apply_at(
        &mut self,
        attempt_id: &str,
        revision: u64,
        from: u64,
        result: Result<AttemptRead, ReadFailure>,
        now: Instant,
    ) {
        let attempt = self
            .map
            .entry(attempt_id.to_string())
            .or_insert_with(|| Attempt::new(attempt_id));
        attempt.revision = Some(revision);
        attempt.retry_at = None;
        match result {
            Ok(read) => {
                attempt.failure = None;
                attempt.no_record = read.no_record;
                if from == 0 {
                    attempt.entries = read.entries;
                } else {
                    attempt.entries.extend(read.entries);
                }
                attempt.next_entry = attempt.next_entry.max(read.next_entry);
                attempt.closed = read.closed;
                attempt.truncated_tail = read.truncated_tail;
                attempt.source = read.source;
                attempt.recorded_at = read.recorded_at;
                attempt.transcript.replace_entries(attempt.entries.clone());
            }
            Err(failure) => {
                if failure == ReadFailure::Starting {
                    attempt.retry_at = Some(now + READER_RETRY);
                }
                attempt.failure = Some(failure);
            }
        }
    }
}

/// What a key does to a transcript reader — the same in every tab that has
/// one: the entry cursor, expanding, the raw view.
pub fn transcript_action(transcript: &mut TranscriptState, action: Action) {
    match action {
        Action::Down => transcript.line_down(),
        Action::Up => transcript.line_up(),
        Action::PageDown => transcript.page_down(),
        Action::PageUp => transcript.page_up(),
        Action::Top => transcript.to_top(),
        Action::Bottom => transcript.to_bottom(),
        Action::Follow => transcript.toggle_follow(),
        Action::Expand if transcript.mode() != DisplayMode::Raw => {
            if let Some(entry) = transcript.selected_entry().cloned() {
                transcript.toggle_expanded(&entry);
            }
        }
        Action::RawEntry => transcript.toggle_selected_raw(),
        Action::RawAll => transcript.toggle_mode(),
        Action::NextConversationView => transcript.next_conversation_view(),
        _ => {}
    }
}

/// A step entry as the strip of *Rozmowa* names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepChip {
    /// The label of the entry, unique within the timeline.
    pub label: String,
    pub status: PositionStatus,
    pub icon: Option<&'static str>,
}

/// *Rozmowa*: the conversation of one step entry at a time, picked on the
/// strip of steps above it, newest first. Following (`f`) keeps the newest
/// step and its newest entry in view; picking a step by hand or scrolling up
/// in its conversation ends it. Each step
/// keeps its own reader (cursor, expanded entries, following the end).
#[derive(Debug, Clone)]
pub struct Conversation {
    task_id: String,
    /// The step entries, newest first.
    steps: Vec<StepChip>,
    selected: Option<String>,
    follow: bool,
    readers: BTreeMap<String, TranscriptState>,
    /// The reader shown while no step is selected.
    empty: TranscriptState,
    built: bool,
    loading: bool,
    /// The first failed read, and the step it was of.
    failure: Option<(String, ReadFailure)>,
    /// Some attempt was last read from the task's record: when the newest
    /// such record was written.
    record: Option<Option<DateTime<Utc>>>,
}

impl Conversation {
    /// An empty conversation of `task_id`.
    pub fn new(task_id: &str) -> Self {
        Self {
            task_id: task_id.to_string(),
            steps: Vec::new(),
            selected: None,
            follow: true,
            readers: BTreeMap::new(),
            empty: TranscriptState::new(TranscriptId::from(task_id)),
            built: false,
            loading: false,
            failure: None,
            record: None,
        }
    }

    /// The reader of the selected step.
    pub fn transcript(&self) -> &TranscriptState {
        self.selected
            .as_ref()
            .and_then(|label| self.readers.get(label))
            .unwrap_or(&self.empty)
    }

    /// The reader of the selected step, to scroll it.
    pub fn transcript_mut(&mut self) -> &mut TranscriptState {
        match self
            .selected
            .as_ref()
            .and_then(|label| self.readers.get_mut(label))
        {
            Some(reader) => reader,
            None => &mut self.empty,
        }
    }

    /// The step entries, newest first.
    pub fn steps(&self) -> &[StepChip] {
        &self.steps
    }

    /// The label of the selected step.
    pub fn selected_step(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// Whether it follows: the newest step, at its newest entry.
    pub fn follows(&self) -> bool {
        self.follow
    }

    /// `f`: follow the newest step at its newest entry, or stop following
    /// and leave the reader where it is. Returns whether the conversation
    /// must be put together again, as the selection moves to the newest step.
    pub fn toggle_follow(&mut self) -> bool {
        self.follow = !self.follow;
        if !self.follow {
            let reader = self.transcript_mut();
            if reader.is_following_end() {
                reader.toggle_follow();
            }
        }
        self.follow
    }

    /// A reading key on the selected step's reader. Leaving the end of the
    /// conversation ends the following.
    pub fn read(&mut self, action: Action) {
        transcript_action(self.transcript_mut(), action);
        if !self.transcript().is_following_end() {
            self.follow = false;
        }
    }

    /// Whether the conversation was put together at least once.
    pub fn is_built(&self) -> bool {
        self.built
    }

    /// Whether some attempt of the selected step has not been read yet.
    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// Why an attempt could not be read, the first that failed.
    pub fn failure(&self) -> Option<&ReadFailure> {
        self.failure.as_ref().map(|(_, failure)| failure)
    }

    /// Whether what is shown was read from the task's record, and when the
    /// newest record was written: `Some(None)` when the record did not say.
    pub fn record(&self) -> Option<Option<DateTime<Utc>>> {
        self.record
    }

    /// `←`: the step after the selected one (to the left on the strip).
    /// Returns whether the selection moved.
    pub fn select_newer(&mut self) -> bool {
        self.select_by(-1)
    }

    /// `→`: the step before the selected one (to the right on the strip).
    /// Returns whether the selection moved.
    pub fn select_older(&mut self) -> bool {
        self.select_by(1)
    }

    fn select_by(&mut self, by: isize) -> bool {
        let Some(at) = self.selected_index() else {
            return false;
        };
        let to = at as isize + by;
        if to < 0 || to as usize >= self.steps.len() {
            return false;
        }
        self.selected = Some(self.steps[to as usize].label.clone());
        self.follow = false;
        true
    }

    fn selected_index(&self) -> Option<usize> {
        let label = self.selected.as_deref()?;
        self.steps.iter().position(|step| step.label == label)
    }

    /// Puts the selected step's conversation together from the attempts
    /// read. The cursor and the expanded entries stay where they were; a
    /// reader at the end stays at the end.
    pub fn rebuild(&mut self, positions: &[Position], attempts: &Attempts) {
        self.rebuild_with_events(positions, attempts, &[]);
    }

    /// Takes the step entries of `positions` in, moves the selection to the
    /// newest one while following (or when the selected one is gone), and
    /// puts the selected step's conversation together with the workflow
    /// choices made in it: [`build_conversation`] over [`Scope::Step`].
    pub fn rebuild_with_events(
        &mut self,
        positions: &[Position],
        attempts: &Attempts,
        events: &[Event],
    ) {
        self.steps = positions
            .iter()
            .rev()
            .map(|position| StepChip {
                label: position.label.clone(),
                status: position.status,
                icon: position_icon(position),
            })
            .collect();
        if self.follow || self.selected_index().is_none() {
            self.selected = self.steps.first().map(|step| step.label.clone());
        }
        let selected = self
            .selected
            .as_deref()
            .and_then(|label| positions.iter().find(|position| position.label == label));
        let built = selected.map_or_else(Built::default, |position| {
            build_conversation(
                Scope::Step {
                    state_id: &position.state_id,
                    visit: position.visit,
                },
                positions,
                attempts,
                events,
            )
        });
        if let Some(position) = selected {
            let id = TranscriptId::from(format!("{}/{}", self.task_id, position.label));
            self.readers
                .entry(position.label.clone())
                .or_insert_with(|| TranscriptState::new(id))
                .replace_entries(built.entries);
        }
        if self.follow && !self.transcript().is_following_end() {
            self.transcript_mut().toggle_follow();
        }
        self.loading = built.loading;
        self.failure = built.failure;
        self.record = built.record;
        self.built = true;
    }
}

/// What a conversation is put together for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    /// Every step, each under its heading: the order of steps and choices
    /// over the whole task, which *Rozmowa* once showed in one piece.
    Whole,
    /// One entry into a step: its attempts and the choices made in it, no
    /// heading (*Przebieg*).
    Step { state_id: &'a str, visit: u32 },
}

/// A conversation put together, and what the reads behind it say.
#[derive(Debug, Clone, Default)]
pub struct Built {
    pub entries: Vec<TranscriptEntry>,
    /// Some attempt has not been read yet.
    pub loading: bool,
    /// The first failed read, and the step it was of.
    pub failure: Option<(String, ReadFailure)>,
    /// Some attempt was last read from the task's record: when the newest
    /// such record was written.
    pub record: Option<Option<DateTime<Utc>>>,
    /// Some attempt brought entries of its own (not only choices).
    pub has_conversation: bool,
    /// Some attempt has no record at the runner.
    pub no_record: bool,
}

/// Puts the conversation of `scope` together from the attempts read, in the
/// order of the `positions`. A choice stands after the conversation of the
/// step entry it followed and before the heading of the next one; the newest
/// choice, with no step after it, ends the transcript. In a step scope the
/// choices are the ones between its entry and the next entry into any step;
/// a step the events do not tell the entry of has none.
pub fn build_conversation(
    scope: Scope<'_>,
    positions: &[Position],
    attempts: &Attempts,
    events: &[Event],
) -> Built {
    let mut builder = Builder {
        built: Built::default(),
        seen_attempts: std::collections::HashSet::new(),
        headings: scope == Scope::Whole,
    };
    let visits = step_entries(events);
    if let Scope::Step { state_id, visit } = scope {
        for position in positions
            .iter()
            .filter(|position| position.state_id == state_id && position.visit == visit)
        {
            builder.push_position(position, attempts);
        }
        if let Some(index) = visits
            .iter()
            .position(|entered| entered.state_id == state_id && entered.visit == visit)
        {
            let from = visits[index].sequence;
            let until = visits.get(index + 1).map(|next| next.sequence);
            builder.built.entries.extend(
                workflow_choices(events)
                    .into_iter()
                    .filter(|(sequence, _)| {
                        *sequence >= from && until.is_none_or(|until| *sequence < until)
                    })
                    .map(|(_, entry)| entry),
            );
        }
        return builder.built;
    }
    let mut by_visit: BTreeMap<(&str, u32), Vec<&Position>> = BTreeMap::new();
    for position in positions {
        by_visit
            .entry((position.state_id.as_str(), position.visit))
            .or_default()
            .push(position);
    }
    if visits.is_empty() {
        // No step entry to anchor a choice to (an old or partial log): the
        // conversation as it is, the choices at its end.
        for position in positions {
            builder.push_position(position, attempts);
        }
        builder
            .built
            .entries
            .extend(workflow_choices(events).into_iter().map(|(_, entry)| entry));
    } else {
        let mut choices = workflow_choices(events).into_iter().peekable();
        let mut until = |entries: &mut Vec<TranscriptEntry>, next: Option<u64>| {
            while let Some((_, entry)) =
                choices.next_if(|(sequence, _)| next.is_none_or(|next| *sequence < next))
            {
                entries.push(entry);
            }
        };
        until(&mut builder.built.entries, Some(visits[0].sequence));
        for (index, visit) in visits.iter().enumerate() {
            let at_visit = by_visit
                .get(&(visit.state_id.as_str(), visit.visit))
                .map(Vec::as_slice)
                .unwrap_or_default();
            let mut shown = false;
            for position in at_visit {
                shown |= builder.push_position(position, attempts) != Pushed::Nothing;
            }
            if !shown {
                let icon = at_visit.iter().find_map(|position| position_icon(position));
                builder.push_step_heading(&visit.state_id, visit.visit, icon);
            }
            until(
                &mut builder.built.entries,
                visits.get(index + 1).map(|next| next.sequence),
            );
        }
        // A step the events do not tell the entry of: shown, with no choice
        // of its own.
        for position in positions {
            if !visits
                .iter()
                .any(|visit| visit.state_id == position.state_id && visit.visit == position.visit)
            {
                builder.push_position(position, attempts);
            }
        }
    }
    builder.built
}

struct Builder<'a> {
    built: Built,
    seen_attempts: std::collections::HashSet<&'a str>,
    headings: bool,
}

impl<'a> Builder<'a> {
    /// Adds the heading (in the whole scope) and the entries of `position`'s
    /// attempt, and notes what its read says: still loading, failed, from
    /// the record.
    fn push_position(&mut self, position: &'a Position, attempts: &Attempts) -> Pushed {
        if position.conversation != ConversationState::Recorded {
            return Pushed::Nothing;
        }
        let mut result = Pushed::Nothing;
        let mut headed = false;
        for attempt_id in position.attempt_ids() {
            match self.push_attempt(position, attempt_id, attempts, !headed) {
                Pushed::Entries => {
                    headed = true;
                    result = Pushed::Entries;
                }
                Pushed::Covered if result == Pushed::Nothing => result = Pushed::Covered,
                _ => {}
            }
        }
        result
    }

    fn push_attempt(
        &mut self,
        position: &'a Position,
        attempt_id: &'a str,
        attempts: &Attempts,
        heading_wanted: bool,
    ) -> Pushed {
        // A workflow retry may point at the same chat as the preceding
        // position. Show that transcript once, at its first occurrence.
        if !self.seen_attempts.insert(attempt_id) {
            return Pushed::Covered;
        }
        let built = &mut self.built;
        let Some(attempt) = attempts.get(attempt_id) else {
            built.loading = true;
            return Pushed::Covered;
        };
        if built.failure.is_none() {
            built.failure = attempt
                .failure()
                .map(|failure| (position.label.clone(), failure.clone()));
        }
        if attempt.from_record() {
            let newest = built.record.flatten().max(attempt.recorded_at());
            built.record = Some(newest);
        }
        built.no_record |= attempt.no_record();
        if attempt.entries.is_empty() {
            return Pushed::Nothing;
        }
        built.has_conversation |= attempt.has_conversation();
        if self.headings && heading_wanted {
            built.entries.push(heading(
                format!("{attempt_id}/step"),
                &step_name(position_icon(position), &position.label),
            ));
        }
        built.entries.extend(attempt.entries.iter().map(|entry| {
            let mut entry = entry.clone();
            entry.prefix_source_ids(attempt_id);
            entry.with_clipped_head()
        }));
        Pushed::Entries
    }

    /// The heading of a step entry that has no conversation of its own.
    fn push_step_heading(&mut self, state_id: &str, visit: u32, icon: Option<&str>) {
        if self.headings {
            self.built.entries.push(heading(
                format!("step-visit/{state_id}/{visit}/step"),
                &step_name(icon, &format!("{state_id} #{visit}")),
            ));
        }
    }
}

/// What `push_position` did for a position: added its entries, left them to a
/// read still running or to an attempt shown elsewhere, or added nothing.
#[derive(PartialEq, Eq)]
enum Pushed {
    Entries,
    Covered,
    Nothing,
}

fn heading(id: String, label: &str) -> TranscriptEntry {
    TranscriptEntry::new(TranscriptEntryId::from(id), Vec::new(), Vec::new())
        .with_header(format!("── {} ──", one_line(label)))
        .with_presentation(TranscriptPresentation::System)
        .with_clipped_head()
}

/// One entry into a step, as the events tell it: the same `(state, visit)`
/// that a timeline position carries, and where in the event order it began.
struct StepEntry {
    state_id: String,
    visit: u32,
    sequence: u64,
}

/// The step entries of `events`, in the order they happened. The n-th entry
/// into a state is its visit n, as the timeline counts them.
fn step_entries(events: &[Event]) -> Vec<StepEntry> {
    let mut counts: BTreeMap<&str, u32> = BTreeMap::new();
    let mut entered: Vec<(u64, &str)> = events
        .iter()
        .filter_map(|event| match &event.data {
            EventData::StateEntered { state_id } => Some((event.sequence, state_id.as_str())),
            _ => None,
        })
        .collect();
    entered.sort_by_key(|(sequence, _)| *sequence);
    entered
        .into_iter()
        .map(|(sequence, state_id)| {
            let visit = counts.entry(state_id).or_default();
            *visit += 1;
            StepEntry {
                state_id: state_id.to_string(),
                visit: *visit,
                sequence,
            }
        })
        .collect()
}

/// The provider names of a choice a person made through the manual
/// transition (the engine's `MANUAL_PROVIDER` and `OVERRIDE_PROVIDER`).
const HUMAN_PROVIDERS: [&str; 2] = ["human", "human_override"];

/// The workflow choices of `events` with their sequence, in that order. A
/// person's choice is one entry: the `DecisionMade` the engine records after
/// it (same label within the same step entry, or a human provider paired with
/// a `HumanTransitionSelected`) is not shown a second time.
fn workflow_choices(events: &[Event]) -> Vec<(u64, TranscriptEntry)> {
    let mut sorted: Vec<&Event> = events.iter().collect();
    sorted.sort_by_key(|event| event.sequence);
    // Whether a manual transition follows within the same step entry.
    let mut transition_ahead = vec![false; sorted.len()];
    let mut ahead = false;
    for (index, event) in sorted.iter().enumerate().rev() {
        match &event.data {
            EventData::StateEntered { .. } => ahead = false,
            EventData::HumanTransitionSelected { .. } => ahead = true,
            _ => {}
        }
        transition_ahead[index] = ahead;
    }
    let mut answered: Vec<&str> = Vec::new();
    let mut choices = Vec::new();
    for (index, event) in sorted.into_iter().enumerate() {
        let entry = match &event.data {
            EventData::StateEntered { .. } => {
                answered.clear();
                None
            }
            EventData::HumanInputReceived {
                answered_by,
                output_label: Some(label),
                ..
            } => {
                if answered_by.unwrap_or_default() == crate::domain::event::AnsweredBy::Operator {
                    answered.push(label);
                }
                workflow_choice_entry(event, false)
            }
            EventData::DecisionMade {
                provider,
                output_label,
                ..
            } => {
                let human = HUMAN_PROVIDERS.contains(&provider.as_str());
                if let Some(at) = answered.iter().position(|label| *label == output_label) {
                    answered.remove(at);
                    None
                } else if human && transition_ahead[index] {
                    None
                } else {
                    workflow_choice_entry(event, human)
                }
            }
            _ => workflow_choice_entry(event, false).or_else(|| conductor_answer_entry(event)),
        };
        choices.extend(entry.map(|entry| (event.sequence, entry)));
    }
    choices
}

/// Projects only events that record a person or decision-maker choosing a
/// workflow output. Routine runner state events remain in the dedicated log.
/// `by_person` marks a `DecisionMade` that a person's choice produced.
fn workflow_choice_entry(event: &Event, by_person: bool) -> Option<TranscriptEntry> {
    use crate::domain::event::AnsweredBy;
    use crate::domain::ConductorDecisionKind;
    use TranscriptPresentation::{Agent, Conductor, Operator};

    let mut reason = None;
    let (actor, choice, presentation) = match &event.data {
        EventData::DecisionMade {
            provider,
            output_label,
            reasoning,
        } => {
            reason = reasoning.as_deref().filter(|text| !text.trim().is_empty());
            if by_person {
                ("użytkownik".to_string(), output_label.clone(), Operator)
            } else if provider == "conductor" {
                ("Dyrygent".to_string(), output_label.clone(), Conductor)
            } else {
                (
                    format!("agent · {}", one_line(provider)),
                    output_label.clone(),
                    Agent,
                )
            }
        }
        EventData::ConductorDecision {
            kind,
            output_label: Some(output),
            ..
        } if *kind == ConductorDecisionKind::ChooseOutput => {
            ("Dyrygent".to_string(), output.clone(), Conductor)
        }
        EventData::HumanInputReceived {
            answered_by,
            output_label: Some(output),
            ..
        } if answered_by.unwrap_or_default() == AnsweredBy::Operator => {
            ("użytkownik".to_string(), output.clone(), Operator)
        }
        EventData::HumanTransitionSelected { to_state, .. } => {
            ("użytkownik".to_string(), to_state.clone(), Operator)
        }
        _ => return None,
    };

    let mut lines = vec![format!("{actor} wybrał opcję: {}", one_line(&choice))];
    if let Some(reason) = reason {
        lines.push(format!(
            "uzasadnienie: {}",
            crate::domain::display::without_unsafe_chars(reason)
        ));
    }
    Some(
        TranscriptEntry::new(
            TranscriptEntryId::from(format!("workflow-choice/{}", event.sequence)),
            lines,
            Vec::new(),
        )
        .with_header(format!(
            "WYBÓR WORKFLOW · {}",
            event.timestamp.format("%H:%M:%S")
        ))
        .with_presentation(presentation)
        .with_clipped_head()
        .as_workflow_choice(),
    )
}

/// The entry of a question the conductor answered for the agent in
/// `autopilot`, with the rationale it gave.
fn conductor_answer_entry(event: &Event) -> Option<TranscriptEntry> {
    use crate::domain::ConductorDecisionKind;

    let EventData::ConductorDecision {
        kind: ConductorDecisionKind::AnswerQuestion,
        rationale,
        ..
    } = &event.data
    else {
        return None;
    };
    let mut lines = vec!["Dyrygent odpowiedział na pytanie agenta".to_string()];
    if let Some(rationale) = rationale.as_deref().filter(|text| !text.trim().is_empty()) {
        lines.push(format!(
            "uzasadnienie: {}",
            crate::domain::display::without_unsafe_chars(rationale)
        ));
    }
    Some(
        TranscriptEntry::new(
            TranscriptEntryId::from(format!("conductor-answer/{}", event.sequence)),
            lines,
            Vec::new(),
        )
        .with_header(format!(
            "ODPOWIEDŹ DYRYGENTA · {}",
            event.timestamp.format("%H:%M:%S")
        ))
        .with_presentation(TranscriptPresentation::Conductor)
        .with_clipped_head(),
    )
}

/// One row of a transcript: the bar of the selected entry — in the focus role
/// while the keys are on the transcript, in the background role otherwise,
/// the same rule as the lists' bracket (R16) — then the header or the body in the colour of the entry's
/// kind.
fn transcript_row(
    line: TranscriptLine,
    styles: &Styles,
    stale: bool,
    focused: bool,
) -> Line<'static> {
    let style = if stale {
        styles.of(Role::Done)
    } else if line.header {
        styles.chat_header(line.presentation)
    } else {
        styles.chat_body(line.presentation)
    };
    Line::from(vec![
        bar_prefix(styles, line.selected, focused),
        Span::raw(if line.indented { "  " } else { "" }),
        Span::styled(
            crate::domain::display::without_unsafe_chars(&line.text),
            style,
        ),
    ])
}

/// The window a conversation is drawn in: where it is (the position puts the
/// highlighted rows of the choices in the right place) and how big (the width
/// wraps and clips, the height is what scrolls).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConversationWindow {
    pub area: Rect,
}

impl ConversationWindow {
    pub fn new(area: Rect) -> Self {
        Self { area }
    }
}

/// Draws `transcript` into `area`, wrapped to it. `focused` says the keys
/// are on it. The conductor's way into [`render_conversation`].
pub fn render_transcript(
    frame: &mut Frame,
    area: Rect,
    styles: &Styles,
    transcript: &mut TranscriptState,
    stale: bool,
    focused: bool,
) {
    render_conversation(
        frame,
        ConversationWindow::new(area),
        styles,
        transcript,
        stale,
        focused,
    );
}

/// Draws a conversation — the whole one or a step's — into `window`.
pub fn render_conversation(
    frame: &mut Frame,
    window: ConversationWindow,
    styles: &Styles,
    transcript: &mut TranscriptState,
    stale: bool,
    focused: bool,
) {
    let area = window.area;
    let width = usize::from(area.width)
        .saturating_sub(usize::from(PREFIX_WIDTH))
        .max(1);
    let height = usize::from(area.height);
    transcript.set_width(width);
    transcript.set_viewport(height);
    let lines = transcript.logical_lines();
    let visible = wrap_visible_lines(&lines, width, transcript.scroll_offset(), height);
    let choice_rows: Vec<u16> = visible
        .iter()
        .zip(area.top()..area.bottom())
        .filter(|(line, _)| line.workflow_choice)
        .map(|(_, y)| y)
        .collect();
    let rows: Vec<Line> = visible
        .into_iter()
        .map(|line| transcript_row(line, styles, stale, focused))
        .collect();
    frame.render_widget(Paragraph::new(rows), area);
    // A workflow choice is a block from the content column to the right
    // edge; the bar's columns stay as they are. Patching the cells keeps
    // the text's own colour.
    let background = styles.workflow_choice_background();
    let content_x = area.x.saturating_add(PREFIX_WIDTH);
    let content_width = area.right().saturating_sub(content_x);
    for y in choice_rows {
        frame
            .buffer_mut()
            .set_style(Rect::new(content_x, y, content_width, 1), background);
    }
}

/// The notes on the fixed lines above a conversation: the badge of a record
/// (seen `now`) and a failed read, each one line fitted to `width`.
pub fn notes(
    record: Option<Option<DateTime<Utc>>>,
    failure: Option<(String, Role)>,
    now: DateTime<Utc>,
    width: u16,
    styles: &Styles,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(recorded_at) = record {
        lines.push(Line::styled(
            record_badge(recorded_at, now, width),
            styles.of(Role::Attention),
        ));
    }
    if let Some((text, role)) = failure {
        lines.push(Line::styled(text, styles.of(role)));
    }
    lines
}

/// Draws `notes` at the top of `area` and returns what is left under them.
pub fn render_notes(frame: &mut Frame, area: Rect, notes: Vec<Line<'static>>) -> Rect {
    let rows = u16::try_from(notes.len())
        .unwrap_or(u16::MAX)
        .min(area.height);
    if rows > 0 {
        frame.render_widget(
            Paragraph::new(notes),
            Rect::new(area.x, area.y, area.width, rows),
        );
    }
    Rect::new(area.x, area.y + rows, area.width, area.height - rows)
}

/// The rows the strip of steps takes: a rule, the steps, a rule.
pub const STEP_STRIP_ROWS: u16 = 3;

/// Between two steps on the strip.
const STEP_SEPARATOR: &str = " · ";

/// Where steps are cut off the strip.
const STEP_MORE: &str = "…";

/// Draws the strip of steps over the whole of `area` (its first
/// [`STEP_STRIP_ROWS`] rows): a rule across the full width, the steps newest
/// first indented like the tab's content, and a rule again. The selected step
/// is bold and underlined, in the focus role while the keys are in the tab.
/// A strip wider than the area keeps the selected step in view, `…` where
/// steps are cut.
pub fn render_step_strip(
    frame: &mut Frame,
    area: Rect,
    styles: &Styles,
    conversation: &Conversation,
    focused: bool,
    stale: bool,
) {
    let done = styles.of(Role::Done);
    let label = if conversation.follows() {
        (
            "↧ śledzi [f]",
            styles.of(if stale { Role::Done } else { Role::Live }),
        )
    } else {
        (
            "⏸ ręcznie [f]",
            styles.of(if stale { Role::Done } else { Role::Attention }),
        )
    };
    let rows = [
        rule_line(area.width, done, None, Some(label)),
        Line::default(),
        rule_line(area.width, done, None, None),
    ];
    for (offset, line) in (0..area.height.min(STEP_STRIP_ROWS)).zip(rows) {
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(area.x, area.y + offset, area.width, 1),
        );
    }
    if area.height < 2 {
        return;
    }
    let steps = Rect::new(
        area.x.saturating_add(2),
        area.y + 1,
        area.width.saturating_sub(3),
        1,
    );
    frame.render_widget(
        Paragraph::new(step_strip_line(
            conversation,
            usize::from(steps.width),
            focused,
            styles,
        )),
        steps,
    );
}

/// The steps of the strip that fit in `width`, with the selected one in view.
fn step_strip_line(
    conversation: &Conversation,
    width: usize,
    focused: bool,
    styles: &Styles,
) -> Line<'static> {
    let steps = conversation.steps();
    if steps.is_empty() {
        return Line::styled("brak kroków", styles.of(Role::Done));
    }
    let chip_widths: Vec<usize> = steps
        .iter()
        .map(|step| 2 + cells(&step_name(step.icon, &one_line(&step.label))))
        .collect();
    let separator = Span::raw(STEP_SEPARATOR).width();
    let more = Span::raw(STEP_MORE).width() + 1;
    let span_width = |from: usize, to: usize| -> usize {
        chip_widths[from..=to].iter().sum::<usize>() + separator * (to - from)
    };
    let selected = conversation
        .selected_step()
        .and_then(|label| steps.iter().position(|step| step.label == label))
        .unwrap_or(0);
    let mut first = 0;
    while first < selected && span_width(first, selected) + more * usize::from(first > 0) > width {
        first += 1;
    }
    let mut spans = Vec::new();
    let mut used = 0;
    if first > 0 {
        spans.push(Span::styled(format!("{STEP_MORE} "), styles.of(Role::Done)));
        used += more;
    }
    for (index, step) in steps.iter().enumerate().skip(first) {
        let gap = if index > first { separator } else { 0 };
        let last = index + 1 == steps.len();
        let room = width.saturating_sub(if last { 0 } else { more });
        if index > selected && used + gap + chip_widths[index] > room {
            spans.push(Span::styled(format!(" {STEP_MORE}"), styles.of(Role::Done)));
            break;
        }
        if gap > 0 {
            spans.push(Span::styled(STEP_SEPARATOR, styles.of(Role::Done)));
        }
        spans.extend(step_chip(step, index == selected, focused, styles));
        used += gap + chip_widths[index];
    }
    Line::from(spans)
}

/// A step on the strip: its status glyph and its name.
fn step_chip(
    step: &StepChip,
    selected: bool,
    focused: bool,
    styles: &Styles,
) -> [Span<'static>; 2] {
    let (glyph, role, _) = super::progress::status_look(step.status);
    let label = one_line(&step.label);
    let label_style = if selected {
        let role = if focused { Role::Focus } else { Role::Calm };
        styles.selected_step(role)
    } else {
        styles.of(Role::Done)
    };
    [
        Span::styled(format!("{glyph} "), styles.of(role)),
        Span::styled(step_name(step.icon, &label), label_style),
    ]
}

/// Draws the tab. `focused` says the keys are in it.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    styles: &Styles,
    conversation: &mut Conversation,
    stale: bool,
    focused: bool,
    glyph: &str,
) {
    let failure = conversation
        .failure
        .as_ref()
        .map(|(step, failure)| failure.note(Some(step)));
    let notes = notes(
        conversation.record(),
        failure,
        Utc::now(),
        area.width,
        styles,
    );
    let body = render_notes(frame, area, notes);
    if conversation.transcript().entries().is_empty() {
        let text = if conversation.is_loading() {
            format!("{glyph} wczytuję rozmowę kroku…")
        } else if conversation.record().is_some() {
            "brak zapisanej rozmowy".to_string()
        } else if conversation.steps().is_empty() {
            "brak rozmowy — żaden krok nie zapisał jej jeszcze".to_string()
        } else {
            "brak rozmowy w tym kroku".to_string()
        };
        frame.render_widget(
            Paragraph::new(Line::styled(text, styles.of(Role::Done))),
            body,
        );
        return;
    }
    let mode = conversation.transcript().mode();
    let (title, rest) = (
        first_row(body),
        Rect::new(
            body.x,
            body.y + 1,
            body.width,
            body.height.saturating_sub(1),
        ),
    );
    let transcript_area = if mode != DisplayMode::Rendered {
        let label = match mode {
            DisplayMode::Chronology => "chronologia · v: RAW · r: źródło",
            DisplayMode::Raw => "RAW · v: rozmowa",
            DisplayMode::Rendered => unreachable!(),
        };
        frame.render_widget(
            Paragraph::new(Line::styled(label, styles.of(Role::Attention))),
            title,
        );
        rest
    } else {
        body
    };
    render_conversation(
        frame,
        ConversationWindow::new(transcript_area),
        styles,
        conversation.transcript_mut(),
        stale,
        focused,
    );
}
