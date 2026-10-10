//! *Rozmowa*: the whole conversation of the task, and the per-attempt cache
//! that *Przebieg* shows a step from.
//!
//! Pages come through the control plane's relay
//! (`GET /control/v1/tasks/:id/attempts/:attempt/conversation`), which waits
//! for the live runner, so a read can fail with a reason — the failure lands
//! in the tab, not in the connection. An attempt is read once and then only
//! from the entry it stopped at ([`Attempts::from`]).
//!
//! Entries are the transcript model of [`crate::tui::transcript`]: an
//! entry is projected by [`agent_transcript_entry`], with the same
//! sanitisation and the same raw view as the conductor's chat. The colours come
//! from [`Styles::chat_header`] and [`Styles::chat_body`], the conversation
//! scale of the role module.

use std::collections::BTreeMap;

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::api::{ApiClient, ClientError, ConversationResponse as Page};
use crate::controller_protocol::ConversationPageQuery;
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
                });
            }
        }
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
    failure: Option<String>,
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
            failure: None,
            revision: None,
            transcript: TranscriptState::new(TranscriptId::from(attempt_id)),
        }
    }

    /// Why the last read failed.
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
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
            // A closed conversation that was read whole never changes.
            let settled = attempt.failure.is_none()
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
        result: Result<AttemptRead, String>,
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
                attempt.transcript.replace_entries(attempt.entries.clone());
            }
            Err(message) => attempt.failure = Some(message),
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
    failure: Option<String>,
}

impl Conversation {
    /// An empty conversation of `task_id`.
    pub fn new(task_id: &str) -> Self {
        Self {
            full: TranscriptState::new(TranscriptId::from(task_id)),
            built: false,
            loading: false,
            failure: None,
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
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// Puts the whole conversation together from the attempts read, in the
    /// order of the `positions`. The cursor and the expanded entries stay
    /// where they were; a reader at the end stays at the end.
    pub fn rebuild(&mut self, positions: &[Position], attempts: &Attempts) {
        let mut entries = Vec::new();
        self.loading = false;
        self.failure = None;
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
                    .map(|reason| format!("{}: {reason}", position.label));
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

/// One row of a transcript: the bar of the selected entry — thick while the
/// keys are on the transcript, thin otherwise, the same rule as the lists'
/// bracket (R16) — then the header or the body in the colour of the entry's
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

/// Draws the tab. `focused` says the keys are in it.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    styles: &Styles,
    conversation: &mut Conversation,
    stale: bool,
    focused: bool,
) {
    let mut body = area;
    if let Some(failure) = conversation.failure() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!("✗ nie udało się wczytać rozmowy ({})", one_line(failure)),
                styles.of(Role::Problem),
            )),
            first_row(area),
        );
        body = Rect::new(
            area.x,
            area.y + 1,
            area.width,
            area.height.saturating_sub(1),
        );
    }
    if conversation.transcript().entries().is_empty() {
        let text = if conversation.is_loading() {
            "wczytuję rozmowę…"
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
