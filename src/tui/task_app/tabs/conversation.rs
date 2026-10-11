//! *Rozmowa*: the whole conversation of the task, and the per-attempt cache
//! that *Przebieg* shows a step from.
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
//! Entries are the transcript model of [`crate::tui::transcript`]: an
//! entry is projected by [`agent_transcript_entry`], with the same
//! sanitisation and the same raw view as the conductor's chat. The colours come
//! from [`Styles::chat_header`] and [`Styles::chat_body`], the conversation
//! scale of the role module.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::api::{ApiClient, ClientError, ConversationResponse as Page};
use crate::cli::task_text::RecordAge;
use crate::controller_protocol::{
    ConversationPageQuery, ExecutionEventsSource, CONVERSATION_RECORD_UNREADABLE,
    CONVERSATION_RECORD_UNREADABLE_PREFIX, CONVERSATION_UNAVAILABLE,
};
use crate::engine::{ConversationState, Position};

use super::super::super::scroll::one_line;
use super::super::super::selection::{bar_prefix, PREFIX_WIDTH};
use super::super::layout::first_row;
use crate::tui::conversation::{agent_transcript_entry, classify};
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
                entries.extend(
                    page.iter()
                        .map(|entry| agent_transcript_entry(&classify(entry))),
                );
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
            ClientError::Refused { code, .. } if code == CONVERSATION_UNAVAILABLE => {
                Self::Unavailable(reason)
            }
            _ => Self::Failed(reason),
        }
    }

    /// The reason alone.
    pub fn detail(&self) -> &str {
        match self {
            Self::Unavailable(detail) | Self::RecordUnreadable(detail) | Self::Failed(detail) => {
                detail
            }
        }
    }

    /// The note of the failure: its words and its role. `step` names the
    /// step a failed read was of, when the note stands for several.
    pub fn note(&self, step: Option<&str>) -> (String, Role) {
        match self {
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
        self.map.get(attempt_id).is_none_or(|attempt| {
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
        let attempt = self
            .map
            .entry(attempt_id.to_string())
            .or_insert_with(|| Attempt::new(attempt_id));
        attempt.revision = Some(revision);
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
            Err(failure) => attempt.failure = Some(failure),
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
        Action::Bottom | Action::Follow => transcript.to_bottom(),
        Action::Expand => {
            if let Some(entry) = transcript.selected_entry().cloned() {
                transcript.toggle_expanded(&entry);
            }
        }
        Action::RawEntry => transcript.toggle_selected_raw(),
        Action::RawAll => transcript.toggle_mode(),
        _ => {}
    }
}

/// *Rozmowa*: every attempt's entries, oldest attempt first, one transcript.
#[derive(Debug, Clone)]
pub struct Conversation {
    full: TranscriptState,
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
            full: TranscriptState::new(TranscriptId::from(task_id)),
            built: false,
            loading: false,
            failure: None,
            record: None,
        }
    }

    /// The reader of the whole conversation.
    pub fn transcript(&self) -> &TranscriptState {
        &self.full
    }

    /// The reader, to scroll it.
    pub fn transcript_mut(&mut self) -> &mut TranscriptState {
        &mut self.full
    }

    /// Whether the conversation was put together at least once.
    pub fn is_built(&self) -> bool {
        self.built
    }

    /// Whether some attempt has not been read yet.
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

    /// Puts the whole conversation together from the attempts read, in the
    /// order of the `positions`. The cursor and the expanded entries stay
    /// where they were; a reader at the end stays at the end.
    pub fn rebuild(&mut self, positions: &[Position], attempts: &Attempts) {
        let mut entries = Vec::new();
        self.loading = false;
        self.failure = None;
        self.record = None;
        for position in positions {
            let (ConversationState::Recorded, Some(attempt_id)) = (
                &position.conversation,
                position.action_execution_id.as_deref(),
            ) else {
                continue;
            };
            let Some(attempt) = attempts.get(attempt_id) else {
                self.loading = true;
                continue;
            };
            if self.failure.is_none() {
                self.failure = attempt
                    .failure()
                    .map(|failure| (position.label.clone(), failure.clone()));
            }
            if attempt.from_record() {
                let newest = self.record.flatten().max(attempt.recorded_at());
                self.record = Some(newest);
            }
            if attempt.entries.is_empty() {
                continue;
            }
            entries.push(
                TranscriptEntry::new(
                    TranscriptEntryId::from(format!("{attempt_id}/step")),
                    Vec::new(),
                    Vec::new(),
                )
                .with_header(format!("── {} ──", one_line(&position.label)))
                .with_presentation(TranscriptPresentation::System),
            );
            entries.extend(attempt.entries.iter().map(|entry| {
                let mut entry = entry.clone();
                entry.id = TranscriptEntryId::from(format!("{attempt_id}/{}", entry.id.as_str()));
                entry
            }));
        }
        self.full.replace_entries(entries);
        self.built = true;
    }
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
        Span::styled(
            crate::domain::display::without_unsafe_chars(&line.text),
            style,
        ),
    ])
}

/// Draws `transcript` into `area`, wrapped to it. `focused` says the keys
/// are on it.
pub fn render_transcript(
    frame: &mut Frame,
    area: Rect,
    styles: &Styles,
    transcript: &mut TranscriptState,
    stale: bool,
    focused: bool,
) {
    let width = usize::from(area.width)
        .saturating_sub(usize::from(PREFIX_WIDTH))
        .max(1);
    let height = usize::from(area.height);
    transcript.set_width(width);
    transcript.set_viewport(height);
    let lines = transcript.logical_lines();
    let rows: Vec<Line> = wrap_visible_lines(&lines, width, transcript.scroll_offset(), height)
        .into_iter()
        .map(|line| transcript_row(line, styles, stale, focused))
        .collect();
    frame.render_widget(Paragraph::new(rows), area);
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

/// Draws the tab. `focused` says the keys are in it.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    styles: &Styles,
    conversation: &mut Conversation,
    stale: bool,
    focused: bool,
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
            "wczytuję rozmowę…"
        } else if conversation.record().is_some() {
            "brak zapisanej rozmowy"
        } else {
            "brak rozmowy — żaden krok nie zapisał jej jeszcze"
        };
        frame.render_widget(
            Paragraph::new(Line::styled(text, styles.of(Role::Done))),
            body,
        );
        return;
    }
    let raw = conversation.transcript().mode() == DisplayMode::Raw;
    let (title, rest) = (
        first_row(body),
        Rect::new(
            body.x,
            body.y + 1,
            body.width,
            body.height.saturating_sub(1),
        ),
    );
    if raw {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "widok surowy (Ctrl+V wraca)",
                styles.of(Role::Attention),
            )),
            title,
        );
        render_transcript(
            frame,
            rest,
            styles,
            conversation.transcript_mut(),
            stale,
            focused,
        );
    } else {
        render_transcript(
            frame,
            body,
            styles,
            conversation.transcript_mut(),
            stale,
            focused,
        );
    }
}
