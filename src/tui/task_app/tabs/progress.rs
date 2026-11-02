//! *Przebieg*: the step timeline (one row per attempt of an action, like
//! `narsor steps`) and the content of the selected step.
//!
//! The selected step follows the one that is running ([`Progress::follows`]):
//! a read that brings a new current step moves the selection there, until the
//! operator moves it by hand or presses `f` — and `f` again picks the running
//! step back up. While the operator reads the selected step's conversation
//! the selection holds still ([`Progress::pause`]), following or not, and
//! catches up on the way back. What a refresh must not touch — the selected
//! step, the mode — is the state of [`Progress`]; what it replaces is the
//! timeline.
//!
//! The axis is a bracketed list ([`render_bracketed_list`]): one row per step,
//! stacked, the selected step bracketed — in the focus role while the keys
//! are on the axis, in the background role while they read the step's
//! conversation or are elsewhere. The conversation marks its selected entry
//! with the same bar, in the focus role only while the keys are in it.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use std::collections::BTreeMap;

use crate::controller_protocol::TimelineResponse;
use crate::domain::Event;
use crate::task_usage_text::{step_cells, StepCells};
use crate::tui::style::{Role, Styles};
use narsor_protocol::api::{ConversationState, Position, PositionStatus};

use super::super::super::scroll::one_line;
use super::super::super::selection::{render_bracketed_list, PREFIX_WIDTH};
use super::super::layout::first_row;
use super::super::list::fit;
use super::super::StepFocus;
use super::conversation::Attempts;
use super::conversation::{
    self, build_conversation, Built, ConversationWindow, ReadFailure, Scope,
};
use crate::tui::transcript::{TranscriptId, TranscriptState};

/// Widest the axis grows, frame included, while no step reports usage.
const AXIS_MAX_WIDTH: u16 = 34;
/// Narrowest a step's label is cut to before the status word goes.
const MIN_LABEL: usize = 8;

/// One entry into a step as *Przebieg* reads it: the reader (cursor,
/// expanded entries, following) and what the reads behind it say. It is the
/// state of the entry, not of the selection, so it outlives moving the
/// selection away and back.
#[derive(Debug, Clone)]
pub struct StepView {
    transcript: TranscriptState,
    loading: bool,
    failure: Option<ReadFailure>,
    record: Option<Option<chrono::DateTime<chrono::Utc>>>,
    has_conversation: bool,
    no_record: bool,
}

impl StepView {
    fn new(state_id: &str, visit: u32) -> Self {
        Self {
            transcript: TranscriptState::new(TranscriptId::from(format!(
                "step/{state_id}/{visit}"
            ))),
            loading: false,
            failure: None,
            record: None,
            has_conversation: false,
            no_record: false,
        }
    }

    /// Whether there is a conversation to read: an attempt of the entry
    /// brought entries of its own. A banner of a failed read over entries
    /// read before does not take it away.
    pub fn has_conversation(&self) -> bool {
        self.has_conversation
    }

    /// The reader of the step, for keys and for drawing.
    pub fn transcript_mut(&mut self) -> &mut TranscriptState {
        &mut self.transcript
    }

    /// The reader of the step.
    pub fn transcript(&self) -> &TranscriptState {
        &self.transcript
    }

    fn apply(&mut self, built: Built) {
        self.loading = built.loading;
        self.failure = built.failure.map(|(_, failure)| failure);
        self.record = built.record;
        self.has_conversation = built.has_conversation;
        self.no_record = built.no_record;
        self.transcript.replace_entries(built.entries);
    }
}

/// What the timeline remembers per task.
#[derive(Debug, Clone)]
pub struct Progress {
    timeline: Option<TimelineResponse>,
    failure: Option<String>,
    revision: Option<u64>,
    selected: Option<String>,
    follow: bool,
    /// The operator reads the selected step's conversation: a read does not
    /// move the selection, unless the step is gone.
    paused: bool,
    /// The first step in view on the axis, kept between frames.
    top: usize,
    /// The conversation of every entry into a step that was put together,
    /// by `(state, visit)`.
    step_views: BTreeMap<(String, u32), StepView>,
    /// The entry the last put-together was of.
    built_for: Option<(String, u32)>,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            timeline: None,
            failure: None,
            revision: None,
            selected: None,
            follow: true,
            paused: false,
            top: 0,
            step_views: BTreeMap::new(),
            built_for: None,
        }
    }
}

impl Progress {
    /// The last timeline read.
    pub fn timeline(&self) -> Option<&TimelineResponse> {
        self.timeline.as_ref()
    }

    /// Why the last read failed, until one succeeds.
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// The attempts of actions, in the order they started.
    pub fn positions(&self) -> &[Position] {
        self.timeline
            .as_ref()
            .and_then(|response| response.timeline.as_ref())
            .map_or(&[], |timeline| timeline.positions.as_slice())
    }

    /// Whether the selection follows the running step.
    pub fn follows(&self) -> bool {
        self.follow
    }

    /// Whether the selection holds still for the step's conversation.
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Holds the selection where it is while its conversation is read.
    pub fn pause(&mut self) {
        self.paused = true;
    }

    /// Lets the selection go again; with the follow on it jumps to the
    /// running step it held off.
    pub fn resume(&mut self) {
        if std::mem::take(&mut self.paused) && self.follow {
            self.selected = self.current_label().map(str::to_string);
        }
    }

    /// The label of the selected step.
    pub fn selected_label(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// The selected step.
    pub fn selected_position(&self) -> Option<&Position> {
        let label = self.selected.as_deref()?;
        self.positions()
            .iter()
            .find(|position| position.label == label)
    }

    fn selected_key(&self) -> Option<(String, u32)> {
        self.selected_position()
            .map(|position| (position.state_id.clone(), position.visit))
    }

    /// The conversation of the selected step.
    pub fn step_view(&self) -> Option<&StepView> {
        self.step_views.get(&self.selected_key()?)
    }

    /// The conversation of the selected step, to scroll it.
    pub fn step_view_mut(&mut self) -> Option<&mut StepView> {
        let key = self.selected_key()?;
        self.step_views.get_mut(&key)
    }

    /// Whether the selected step's conversation was not put together since
    /// the selection came to it.
    pub fn step_unbuilt(&self) -> bool {
        self.selected_key()
            .is_some_and(|key| self.built_for.as_ref() != Some(&key))
    }

    /// Marks every step's conversation as not put together since the last
    /// read.
    pub fn invalidate_step(&mut self) {
        self.built_for = None;
    }

    /// Puts the selected step's conversation together again from what was
    /// read. The cursor and the expanded entries stay where they were; a
    /// reader at the end stays at the end.
    pub fn rebuild_step(&mut self, attempts: &Attempts, events: &[Event]) {
        let Some(key) = self.selected_key() else {
            return;
        };
        let built = build_conversation(
            Scope::Step {
                state_id: &key.0,
                visit: key.1,
            },
            self.positions(),
            attempts,
            events,
        );
        self.step_views
            .entry(key.clone())
            .or_insert_with(|| StepView::new(&key.0, key.1))
            .apply(built);
        self.built_for = Some(key);
    }

    /// The attempts of the selected step's entry that can be read.
    pub fn selected_attempts(&self) -> Vec<String> {
        let Some((state_id, visit)) = self.selected_key() else {
            return Vec::new();
        };
        self.positions()
            .iter()
            .filter(|position| position.state_id == state_id && position.visit == visit)
            .flat_map(super::super::detail::recorded_attempts)
            .collect()
    }

    /// `(n, m)`: the running step is the `n`th of the `m` steps so far.
    /// Without a running step, the last one counts as the current.
    pub fn step_count(&self) -> Option<(usize, usize)> {
        let positions = self.positions();
        if positions.is_empty() {
            return None;
        }
        let current = self.current_label();
        let at = current
            .and_then(|label| {
                positions
                    .iter()
                    .position(|position| position.label == label)
            })
            .unwrap_or(positions.len() - 1);
        Some((at + 1, positions.len()))
    }

    /// The step the runner is at, or the one the timeline defaults to.
    fn current_label(&self) -> Option<&str> {
        let response = self.timeline.as_ref()?;
        response.current_position.as_deref().or_else(|| {
            response
                .timeline
                .as_ref()
                .and_then(|timeline| timeline.default_position.as_deref())
        })
    }

    /// Whether the timeline must be read for a task at `revision`.
    pub fn needs_read(&self, revision: u64) -> bool {
        self.revision != Some(revision)
    }

    /// Makes the next [`Self::needs_read`] true, keeping what is shown.
    pub fn invalidate(&mut self) {
        self.revision = None;
    }

    /// Takes a read of the timeline in (`failure` is its reason in words).
    /// The selection, the cursor and the mode stay; with the follow on, and not
    /// paused, the selection moves to the running step. A selected step that
    /// is gone moves it anyway and ends the pause.
    pub fn apply(&mut self, revision: u64, result: Result<TimelineResponse, String>) {
        self.revision = Some(revision);
        match result {
            Ok(response) => {
                self.timeline = Some(response);
                self.failure = None;
                let lost = self
                    .selected
                    .as_deref()
                    .is_none_or(|label| !self.positions().iter().any(|step| step.label == label));
                if lost {
                    self.paused = false;
                }
                if (self.follow && !self.paused) || lost {
                    self.selected = self.current_label().map(str::to_string);
                }
            }
            Err(message) => self.failure = Some(message),
        }
    }

    /// `f`: follow the running step, or stay where the cursor is. Following
    /// again jumps to the running step at once.
    pub fn toggle_follow(&mut self) {
        self.follow = !self.follow;
        if self.follow {
            self.selected = self.current_label().map(str::to_string);
        }
    }

    /// Moves the selection `by` steps. A selection that really moves ends the
    /// following.
    pub fn step(&mut self, by: isize) {
        let count = self.positions().len();
        if count == 0 {
            return;
        }
        let at = self.selected_index().unwrap_or(0);
        self.select_index((at as isize + by).clamp(0, count as isize - 1) as usize);
    }

    /// The first step.
    pub fn first(&mut self) {
        if !self.positions().is_empty() {
            self.select_index(0);
        }
    }

    /// The last step.
    pub fn last(&mut self) {
        if let Some(last) = self.positions().len().checked_sub(1) {
            self.select_index(last);
        }
    }

    fn selected_index(&self) -> Option<usize> {
        let label = self.selected.as_deref()?;
        self.positions()
            .iter()
            .position(|position| position.label == label)
    }

    fn select_index(&mut self, index: usize) {
        let label = self.positions()[index].label.clone();
        if self.selected.as_deref() != Some(label.as_str()) {
            self.follow = false;
            self.selected = Some(label);
        }
    }
}

/// The glyph, the role and the word of a step's status: colour is never the
/// only thing that says it.
fn status_look(status: PositionStatus) -> (&'static str, Role, &'static str) {
    match status {
        PositionStatus::Succeeded => ("✓", Role::Done, "ok"),
        PositionStatus::Failed => ("✗", Role::Problem, "błąd"),
        PositionStatus::Interrupted => ("✗", Role::Problem, "przerwany"),
        PositionStatus::InProgress => ("●", Role::Live, "w toku"),
        PositionStatus::AwaitingDecision => ("?", Role::Attention, "czeka na decyzję"),
    }
}

/// One row of the axis, `width` columns wide. The selection is not drawn
/// here: the bracketed list brackets the row (R13).
fn axis_line(position: &Position, width: usize, styles: &Styles, stale: bool) -> Line<'static> {
    let (glyph, role, word) = status_look(position.status);
    let grey = |role: Role| if stale { Role::Done } else { role };
    let label = fit(
        &one_line(&position.label),
        width.saturating_sub(3 + word.chars().count()),
    );
    Line::from(vec![
        Span::styled(glyph, styles.of(grey(role))),
        Span::raw(" "),
        Span::styled(label, styles.of(grey(Role::Calm))),
        Span::styled(format!(" {word}"), styles.of(Role::Done)),
    ])
}

/// The axis: its width and the lines of each step. While a step reports
/// usage the axis is a table — status, label, cost, tokens in and out, the
/// subagents — whose columns are as wide as their widest value, the axis as
/// wide as the columns need within the share of the tab it may take.
struct AxisLayout {
    width: u16,
    rows: Vec<Vec<Line<'static>>>,
}

/// One column of the table: its text in each step and how it is aligned.
struct Column {
    cells: Vec<String>,
    right: bool,
}

impl Column {
    fn new(right: bool, cells: Vec<String>) -> Self {
        Self { cells, right }
    }

    fn width(&self) -> usize {
        self.cells
            .iter()
            .map(|cell| cell.chars().count())
            .max()
            .unwrap_or(0)
    }

    fn text(&self, row: usize) -> String {
        let width = self.width();
        if self.right {
            format!("{:>width$}", self.cells[row])
        } else {
            format!("{:<width$}", self.cells[row])
        }
    }
}

/// The columns of a table row, each after a space; empty columns take none.
fn columns_width(columns: &[&Column]) -> usize {
    columns
        .iter()
        .map(|column| column.width())
        .filter(|width| *width > 0)
        .map(|width| width + 1)
        .sum()
}

impl AxisLayout {
    fn new(positions: &[Position], area_width: u16, styles: &Styles, stale: bool) -> Self {
        let today = (area_width / 3).clamp(18, AXIS_MAX_WIDTH).min(area_width);
        let cells: Vec<StepCells> = positions
            .iter()
            .map(|position| step_cells(position.usage.as_ref()))
            .collect();
        if !cells
            .iter()
            .any(|cells| cells.has_main() || cells.has_subagents())
        {
            let line_width = usize::from(today.saturating_sub(PREFIX_WIDTH));
            return Self {
                width: today,
                rows: positions
                    .iter()
                    .map(|position| vec![axis_line(position, line_width, styles, stale)])
                    .collect(),
            };
        }
        Self::table(positions, &cells, area_width, today, styles, stale)
    }

    fn table(
        positions: &[Position],
        cells: &[StepCells],
        area_width: u16,
        today: u16,
        styles: &Styles,
        stale: bool,
    ) -> Self {
        let grey = |role: Role| if stale { Role::Done } else { role };
        let column = |right: bool, pick: fn(&StepCells) -> &String| {
            Column::new(
                right,
                cells.iter().map(|cells| pick(cells).clone()).collect(),
            )
        };
        let labels: Vec<String> = positions
            .iter()
            .map(|position| one_line(&position.label))
            .collect();
        let label = Column::new(false, labels);
        let word = Column::new(
            false,
            positions
                .iter()
                .map(|position| status_look(position.status).2.to_string())
                .collect(),
        );
        let main = [
            column(true, |cells| &cells.cost),
            column(true, |cells| &cells.input),
            column(true, |cells| &cells.output),
        ];
        let subs = [
            column(true, |cells| &cells.subagent_count),
            column(true, |cells| &cells.subagent_input),
            column(true, |cells| &cells.subagent_output),
        ];
        let main_refs: Vec<&Column> = main.iter().collect();
        let sub_refs: Vec<&Column> = subs.iter().collect();
        let main_width = columns_width(&main_refs);
        let subs_width = columns_width(&sub_refs);
        let has_subs = subs_width > 0;

        // 2: the glyph and its space; the label and the word are columns too.
        let first = |label: usize, word: usize| {
            2 + label + if word > 0 { word + 1 } else { 0 } + main_width
        };
        let prefix = usize::from(PREFIX_WIDTH);
        let area = usize::from(area_width);
        let cap = area
            .saturating_sub(1 + (area * 3).div_ceil(10))
            .min(area * 7 / 10)
            .max(usize::from(today));
        let (label_width, word_width) = (label.width(), word.width());
        let inline = prefix + first(label_width, word_width) + subs_width <= cap;
        let second_line = has_subs && !inline;
        let needed = |label: usize, word: usize| {
            let first = first(label, word) + if inline { subs_width } else { 0 };
            prefix
                + if second_line {
                    first.max(2 + subs_width - 1)
                } else {
                    first
                }
        };
        let target = needed(label_width, word_width)
            .min(cap)
            .max(usize::from(today));
        let content = target.saturating_sub(prefix);
        let content_needed = |label: usize, word: usize| needed(label, word).saturating_sub(prefix);
        let shrink = |word: usize| {
            let over = content_needed(label_width, word).saturating_sub(content);
            label_width
                .saturating_sub(over)
                .max(MIN_LABEL.min(label_width))
        };
        let mut shown_word = word_width;
        let mut shown_label = shrink(word_width);
        if content_needed(shown_label, shown_word) > content {
            shown_word = 0;
            shown_label = shrink(0);
        }

        let rows = positions
            .iter()
            .enumerate()
            .map(|(index, position)| {
                let (glyph, role, _) = status_look(position.status);
                let text = fit(&label.cells[index], shown_label);
                let mut spans = vec![
                    Span::styled(glyph, styles.of(grey(role))),
                    Span::raw(" "),
                    Span::styled(format!("{text:<shown_label$}"), styles.of(grey(Role::Calm))),
                ];
                if shown_word > 0 {
                    spans.push(Span::styled(
                        format!(" {}", word.text(index)),
                        styles.of(Role::Done),
                    ));
                }
                let mut push = |columns: &[Column]| {
                    for column in columns.iter().filter(|column| column.width() > 0) {
                        spans.push(Span::styled(
                            format!(" {}", column.text(index)),
                            styles.of(Role::Done),
                        ));
                    }
                };
                push(&main);
                if inline {
                    push(&subs);
                }
                let mut lines = vec![Line::from(spans)];
                if second_line && cells[index].has_subagents() {
                    let text = subs
                        .iter()
                        .filter(|column| column.width() > 0)
                        .map(|column| column.text(index))
                        .collect::<Vec<_>>()
                        .join(" ");
                    lines.push(Line::styled(format!("  {text}"), styles.of(Role::Done)));
                }
                lines
            })
            .collect();
        Self {
            width: u16::try_from(target).unwrap_or(u16::MAX),
            rows,
        }
    }
}

/// How long a step took, or has been taking.
fn duration_of(position: &Position, now: chrono::DateTime<chrono::Utc>) -> Option<String> {
    let started = position.started_at?;
    let ended = position.ended_at.unwrap_or(now);
    Some(super::super::detail::short_age(
        (ended - started).num_seconds(),
    ))
}

/// The facts of a step above its content.
fn step_lines(
    position: &Position,
    now: chrono::DateTime<chrono::Utc>,
    styles: &Styles,
) -> Vec<Line<'static>> {
    let (glyph, role, word) = status_look(position.status);
    let mut head = vec![
        Span::styled(format!("{glyph} "), styles.of(role)),
        Span::styled(one_line(&position.label), styles.of(Role::Focus)),
        Span::styled(format!(" · {word}"), styles.of(role)),
    ];
    if let Some(action) = &position.action_type {
        head.push(Span::styled(
            format!(" · {}", one_line(action)),
            styles.of(Role::Done),
        ));
    }
    if let Some(duration) = duration_of(position, now) {
        head.push(Span::styled(
            format!(" · {duration}"),
            styles.of(Role::Done),
        ));
    }
    let mut lines = vec![Line::from(head)];
    if let Some(error) = &position.error {
        lines.push(Line::styled(
            format!("✗ {}", one_line(error)),
            styles.of(Role::Problem),
        ));
    }
    let mut notes = Vec::new();
    if position.retry_pending {
        notes.push("ponowienie oczekuje");
    }
    if position.harness_session_interrupted {
        notes.push("sesja harnessu przerwana");
    }
    if position.resumed_harness_session.is_some() {
        notes.push("wznowiona sesja");
    }
    if !notes.is_empty() {
        lines.push(Line::styled(notes.join(" · "), styles.of(Role::Done)));
    }
    lines
}

/// Draws the tab: the axis on the left, the selected step on the right.
/// `keys` is the level the keys are on, `None` while they are outside the
/// tab's content: the axis's bracket and the step conversation's bar are in
/// the focus role only on that level (R12, R16).
pub fn render(
    frame: &mut Frame,
    area: Rect,
    styles: &Styles,
    progress: &mut Progress,
    stale: bool,
    keys: Option<StepFocus>,
    glyph: &str,
) {
    let axis_focused = keys == Some(StepFocus::Axis);
    let conversation_focused = keys == Some(StepFocus::Conversation);
    let mut top_rows = 0;
    if let Some(failure) = progress.failure() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!("✗ nie udało się wczytać przebiegu: {}", one_line(failure)),
                styles.of(Role::Problem),
            )),
            first_row(area),
        );
        top_rows = 1;
    }
    let area = Rect::new(
        area.x,
        area.y + top_rows,
        area.width,
        area.height.saturating_sub(top_rows),
    );
    if progress.positions().is_empty() {
        let loading = format!("{glyph} wczytuję przebieg…");
        let text = if progress.timeline().is_none() && progress.failure().is_none() {
            loading.as_str()
        } else if progress.timeline().is_none() {
            ""
        } else {
            "brak kroków — runner jeszcze ich nie zgłosił"
        };
        frame.render_widget(
            Paragraph::new(Line::styled(text, styles.of(Role::Done))),
            area,
        );
        return;
    }

    let layout = AxisLayout::new(progress.positions(), area.width, styles, stale);
    let axis_width = layout.width.min(area.width);
    let axis = Rect::new(area.x, area.y, axis_width, area.height);
    let content = Rect::new(
        area.x + axis_width + 1,
        area.y,
        area.width.saturating_sub(axis_width + 1),
        area.height,
    );

    let items = layout.rows;
    let selected = progress.selected_index();
    render_bracketed_list(
        frame,
        axis,
        styles,
        &items,
        selected,
        axis_focused,
        &mut progress.top,
    );
    if content.width < 8 {
        return;
    }
    render_step(
        frame,
        content,
        styles,
        progress,
        stale,
        conversation_focused,
        glyph,
    );
}

/// The right-hand side: the facts of the selected step, then its content.
fn render_step(
    frame: &mut Frame,
    area: Rect,
    styles: &Styles,
    progress: &mut Progress,
    stale: bool,
    focused: bool,
    glyph: &str,
) {
    let Some(position) = progress.selected_position().cloned() else {
        return;
    };
    let position = &position;
    let now = progress
        .timeline()
        .and_then(|response| response.observed_at)
        .unwrap_or_else(chrono::Utc::now);
    let mut lines = step_lines(position, now, styles);
    lines.push(Line::raw(""));
    let facts = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .min(area.height);
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(area.x, area.y, area.width, facts),
    );
    let body = Rect::new(
        area.x,
        area.y + facts,
        area.width,
        area.height.saturating_sub(facts),
    );
    let message = |frame: &mut Frame, text: String| {
        frame.render_widget(
            Paragraph::new(Line::styled(text, styles.of(Role::Done))),
            body,
        );
    };
    match (
        &position.conversation,
        position.action_execution_id.as_deref(),
    ) {
        (ConversationState::Recorded, Some(_)) => match progress.step_view_mut() {
            Some(view) => render_step_view(frame, body, styles, view, stale, focused, glyph),
            None => message(frame, format!("{glyph} wczytuję rozmowę kroku…")),
        },
        (ConversationState::Recorded, None) | (ConversationState::NoRecord, _) => {
            message(frame, "brak zapisu rozmowy tego kroku".to_string());
        }
        (ConversationState::NotCreated { reason }, _) => {
            message(frame, format!("brak rozmowy: {}", one_line(reason)));
        }
    }
}

/// The step's conversation: the badge of a record and a failed read on the
/// fixed lines at the top, then the conversation in the window that is left.
fn render_step_view(
    frame: &mut Frame,
    area: Rect,
    styles: &Styles,
    view: &mut StepView,
    stale: bool,
    focused: bool,
    glyph: &str,
) {
    let failure = view.failure.as_ref().map(|failure| failure.note(None));
    let notes = conversation::notes(view.record, failure, chrono::Utc::now(), area.width, styles);
    let body = conversation::render_notes(frame, area, notes);
    if view.transcript.entries().is_empty() {
        let text = if view.loading {
            format!("{glyph} wczytuję rozmowę kroku…")
        } else if view.failure.is_some() {
            return;
        } else if view.no_record {
            "brak zapisanej rozmowy tego kroku".to_string()
        } else if view.record.is_some() {
            "brak zapisanej rozmowy".to_string()
        } else {
            return;
        };
        frame.render_widget(
            Paragraph::new(Line::styled(text, styles.of(Role::Done))),
            body,
        );
        return;
    }
    conversation::render_conversation(
        frame,
        ConversationWindow::new(body),
        styles,
        &mut view.transcript,
        stale,
        focused,
    );
}

#[cfg(test)]
mod tests {
    use narsor_protocol::api::{PositionUsage, TokenCounts};

    use super::*;

    fn counts(input: u64, output: u64) -> TokenCounts {
        TokenCounts {
            input,
            output,
            ..TokenCounts::default()
        }
    }

    fn step(label: &str, usage: Option<PositionUsage>) -> Position {
        Position {
            label: label.into(),
            state_id: label.into(),
            visit: 1,
            action_execution_id: None,
            action_type: None,
            status: PositionStatus::Succeeded,
            started_at: None,
            ended_at: None,
            error: None,
            resumed_harness_session: None,
            harness_session_interrupted: false,
            retry_pending: false,
            conversation: ConversationState::Recorded,
            session_number: Some(1),
            attempts: Vec::new(),
            harness: None,
            model: None,
            effort: None,
            harness_session_id: None,
            usage,
        }
    }

    fn used(total: (u64, u64), subagents: Option<(u64, u64)>, cost: f64) -> Option<PositionUsage> {
        Some(PositionUsage {
            total: Some(counts(total.0, total.1)),
            subagents: subagents.map(|(input, output)| counts(input, output)),
            subagent_count: subagents.map(|_| 2),
            cost_usd: Some(cost),
        })
    }

    fn text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn layout(positions: &[Position], width: u16) -> AxisLayout {
        AxisLayout::new(positions, width, &Styles::new(false), false)
    }

    fn column_of(row: &str, needle: &str) -> usize {
        let at = row
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} in {row:?}"));
        row[..at].chars().count() + needle.chars().count()
    }

    fn sample() -> Vec<Position> {
        vec![
            step(
                "review_api",
                used((120_000, 8_000), Some((40_000, 3_000)), 0.42),
            ),
            step("lint", used((2_100, 400), None, 0.03)),
        ]
    }

    #[test]
    fn values_of_a_kind_stand_in_one_column() {
        let axis = layout(&sample(), 200);
        let rows: Vec<String> = axis.rows.iter().map(|rows| text(&rows[0])).collect();
        assert!(rows[0].contains("2× ↓40k ↑3k"), "{}", rows[0]);
        for (a, b) in [("$0.42", "$0.03"), ("↓120k", "↓2.1k"), ("↑8k", "↑400")] {
            assert_eq!(column_of(&rows[0], a), column_of(&rows[1], b), "{rows:?}");
        }
        assert_eq!(axis.rows[0].len(), 1, "wide: the subagents stay in the row");
    }

    #[test]
    fn a_step_without_a_report_has_empty_usage_fields() {
        let mut steps = sample();
        steps.push(step("script", None));
        let axis = layout(&steps, 200);
        let row = text(&axis.rows[2][0]);
        assert!(row.starts_with("✓ script"), "{row}");
        assert!(
            row.contains("ok") && !row.contains('$') && !row.contains('—'),
            "{row}"
        );
    }

    #[test]
    fn the_axis_stays_within_seventy_percent_and_leaves_the_rest_to_the_step() {
        for width in 60..=220u16 {
            let axis = layout(&sample(), width);
            let today = (width / 3).clamp(18, AXIS_MAX_WIDTH);
            assert!(
                axis.width <= (width * 7 / 10).max(today),
                "{width}: {}",
                axis.width
            );
            assert!(
                width - axis.width > (width * 3).div_ceil(10),
                "{width}: {}",
                axis.width
            );
        }
        assert!(layout(&sample(), 200).width > 34);
    }

    #[test]
    fn a_narrow_tab_moves_the_subagents_under_the_label_of_the_steps_that_have_them() {
        let axis = layout(&sample(), 60);
        let first = text(&axis.rows[0][0]);
        assert!(
            first.contains("$0.42") && first.contains("↓120k") && first.contains("↑8k"),
            "{first}"
        );
        assert!(!first.contains('×'), "{first}");
        assert_eq!(axis.rows[0].len(), 2);
        assert_eq!(text(&axis.rows[0][1]).trim(), "2× ↓40k ↑3k");
        assert!(text(&axis.rows[0][1]).starts_with("  "));
        assert_eq!(axis.rows[1].len(), 1, "no second line without subagents");
    }

    #[test]
    fn without_a_report_the_axis_is_as_it_was() {
        for usage in [None, Some(PositionUsage::default())] {
            let steps = [step("review #1", usage)];
            for width in [20, 60, 200] {
                let axis = layout(&steps, width);
                assert_eq!(axis.width, (width / 3).clamp(18, AXIS_MAX_WIDTH));
                let line = usize::from(axis.width - PREFIX_WIDTH);
                assert_eq!(
                    text(&axis.rows[0][0]),
                    text(&axis_line(&steps[0], line, &Styles::new(false), false))
                );
            }
        }
    }

    #[test]
    fn a_very_narrow_tab_cuts_the_label_and_never_panics() {
        let steps = [
            step(
                "a_really_long_step_label",
                used((120_000, 8_000), Some((40_000, 3_000)), 0.42),
            ),
            step("b", None),
        ];
        for width in 0..=60u16 {
            let axis = layout(&steps, width);
            assert_eq!(axis.rows.len(), 2);
        }
        let row = text(&layout(&steps, 50).rows[0][0]);
        assert!(row.contains('…'), "{row}");
    }
}
