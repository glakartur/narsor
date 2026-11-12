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
//! The axis is a bracketed list ([`render_bracketed_list_plain`]): one row per
//! step, stacked, the selected step bracketed without a background fill — in
//! the focus role while the keys are on the axis, in the background role while
//! they read the step's conversation or are elsewhere. A step's first row is
//! its glyph, its name, when it started and how long it ran, then its usage;
//! the second row is the model, the effort and the harness. The axis does not
//! spell the status out: the glyph says it. The conversation marks its selected entry
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
use super::super::super::selection::{render_bracketed_list_plain, PREFIX_WIDTH};
use super::super::detail::{cells as width_of, fit_cells, fit_step_name, step_name};
use super::super::layout::first_row;
use super::super::StepFocus;
use super::conversation::Attempts;
use super::conversation::{
    self, build_conversation, Built, ConversationWindow, ReadFailure, Scope,
};
use crate::tui::transcript::{TranscriptId, TranscriptState};

/// Widest the axis grows, frame included, while no step reports usage.
const AXIS_MAX_WIDTH: u16 = 34;
/// Columns between the axis and the step's content.
const AXIS_GAP: u16 = 3;
/// Narrowest a step's label is cut to before its times go.
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

/// The glyph and the role of a step's status: the shape of the glyph says it
/// too, so colour is never the only thing that does.
pub(super) fn status_look(status: PositionStatus) -> (&'static str, Role) {
    match status {
        PositionStatus::Succeeded => ("✓", Role::Done),
        PositionStatus::Failed | PositionStatus::Interrupted => ("✗", Role::Problem),
        PositionStatus::InProgress => ("●", Role::Live),
        PositionStatus::AwaitingDecision => ("?", Role::Attention),
    }
}

/// The axis: its width and the lines of each step. The first row of a step is
/// a table — status, label, when it started, how long it ran, then the cost,
/// the tokens in and out and the subagents while a step reports usage — whose
/// columns are as wide as their widest value, the axis as wide as the columns
/// need within the share of the tab it may take.
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
            .map(|cell| width_of(cell))
            .max()
            .unwrap_or(0)
    }

    fn text(&self, row: usize) -> String {
        let gap = " ".repeat(self.width().saturating_sub(width_of(&self.cells[row])));
        if self.right {
            format!("{gap}{}", self.cells[row])
        } else {
            format!("{}{gap}", self.cells[row])
        }
    }
}

/// Spaces before each column of a table row.
const COLUMN_GAP: usize = 2;
/// Spaces before the detail row.
const DETAIL_INDENT: usize = 3;

/// The columns of a table row, each after the gap; empty columns take none.
fn columns_width(columns: &[&Column]) -> usize {
    columns
        .iter()
        .map(|column| column.width())
        .filter(|width| *width > 0)
        .map(|width| width + COLUMN_GAP)
        .sum()
}

/// The most columns the axis may take: the step's content keeps its 30%.
fn axis_cap(area_width: u16, today: u16) -> usize {
    let area = usize::from(area_width);
    area.saturating_sub(usize::from(AXIS_GAP) + (area * 3).div_ceil(10))
        .min(area * 7 / 10)
        .max(usize::from(today))
}

/// When a step started or ended, as the axis shows it.
fn clock(time: Option<chrono::DateTime<chrono::Utc>>) -> String {
    time.map_or_else(String::new, |time| {
        time.with_timezone(&chrono::Local)
            .format("%m-%d %H:%M:%S")
            .to_string()
    })
}

impl AxisLayout {
    fn new(
        positions: &[Position],
        area_width: u16,
        now: chrono::DateTime<chrono::Utc>,
        styles: &Styles,
        stale: bool,
    ) -> Self {
        let mut layout = Self::usage(positions, area_width, now, styles, stale);
        layout.add_details(positions, area_width, styles);
        layout
    }

    /// Under a step that has a model, an effort or a harness, one row with
    /// them, each value in a column of its own across the steps. The axis grows
    /// to fit the row within its share of the tab; a longer row is cut.
    fn add_details(&mut self, positions: &[Position], area_width: u16, styles: &Styles) {
        let text = |value: &Option<String>| value.as_deref().map(one_line).unwrap_or_default();
        let agent = [
            Column::new(false, positions.iter().map(|p| text(&p.model)).collect()),
            Column::new(false, positions.iter().map(|p| text(&p.effort)).collect()),
            Column::new(false, positions.iter().map(|p| text(&p.harness)).collect()),
        ];
        let has_row: Vec<bool> = (0..positions.len())
            .map(|index| agent.iter().any(|column| !column.cells[index].is_empty()))
            .collect();
        let joined = |index: usize| {
            agent
                .iter()
                .filter(|column| column.width() > 0)
                .map(|column| column.text(index))
                .collect::<Vec<_>>()
                .join(&" ".repeat(COLUMN_GAP))
        };
        let agent_refs: Vec<&Column> = agent.iter().collect();
        let agent_width = columns_width(&agent_refs).saturating_sub(COLUMN_GAP);
        let wanted = usize::from(PREFIX_WIDTH) + DETAIL_INDENT + agent_width;
        let today = self.width;
        let width = usize::from(self.width)
            .max(wanted.min(axis_cap(area_width, today)))
            .min(usize::from(u16::MAX));
        self.width = u16::try_from(width).unwrap_or(u16::MAX);
        let content = width.saturating_sub(usize::from(PREFIX_WIDTH));
        for (index, rows) in self.rows.iter_mut().enumerate() {
            if !has_row[index] {
                continue;
            }
            let row = format!("{:DETAIL_INDENT$}{}", "", joined(index));
            rows.push(Line::styled(
                fit_cells(row.trim_end(), content),
                styles.of(Role::Done),
            ));
        }
    }

    fn usage(
        positions: &[Position],
        area_width: u16,
        now: chrono::DateTime<chrono::Utc>,
        styles: &Styles,
        stale: bool,
    ) -> Self {
        let today = (area_width / 3).clamp(18, AXIS_MAX_WIDTH).min(area_width);
        let cells: Vec<StepCells> = positions
            .iter()
            .map(|position| step_cells(position.usage.as_ref()))
            .collect();
        Self::table(positions, &cells, area_width, today, now, styles, stale)
    }

    fn table(
        positions: &[Position],
        cells: &[StepCells],
        area_width: u16,
        today: u16,
        now: chrono::DateTime<chrono::Utc>,
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
            .map(|position| step_name(position.icon(), &one_line(&position.label)))
            .collect();
        let label = Column::new(false, labels);
        let start = Column::new(
            false,
            positions.iter().map(|p| clock(p.started_at)).collect(),
        );
        let duration = Column::new(
            true,
            positions
                .iter()
                .map(|p| duration_of(p, now).unwrap_or_default())
                .collect(),
        );
        let main = [
            column(true, |cells| &cells.cost),
            column(true, |cells| &cells.conductor_cost),
            column(true, |cells| &cells.input),
            column(true, |cells| &cells.output),
        ];
        let subs = [
            column(true, |cells| &cells.subagent_count),
            column(true, |cells| &cells.subagent_input),
            column(true, |cells| &cells.subagent_output),
            column(true, |cells| &cells.subagent_total),
        ];
        let main_refs: Vec<&Column> = main.iter().collect();
        let sub_refs: Vec<&Column> = subs.iter().collect();
        let usage_width = columns_width(&main_refs) + columns_width(&sub_refs);

        // 2: the glyph and its space; the label and the times are columns too.
        let prefix = usize::from(PREFIX_WIDTH);
        let cap = axis_cap(area_width, today);
        let label_width = label.width();
        let times_width = |width: usize| if width > 0 { width + COLUMN_GAP } else { 0 };
        let content_needed = |label: usize, start: usize, duration: usize| {
            2 + label + times_width(start) + times_width(duration) + usage_width
        };
        let target = (prefix + content_needed(label_width, start.width(), duration.width()))
            .min(cap)
            .max(usize::from(today));
        let content = target.saturating_sub(prefix);
        // The label gives way first, down to `MIN_LABEL`; then the duration
        // goes, then the start.
        let shrink = |start: usize, duration: usize| {
            let over = content_needed(label_width, start, duration).saturating_sub(content);
            label_width
                .saturating_sub(over)
                .max(MIN_LABEL.min(label_width))
        };
        let fits = |label: usize, start: usize, duration: usize| {
            content_needed(label, start, duration) <= content
        };
        let (mut shown_start, mut shown_duration) = (start.width(), duration.width());
        let mut shown_label = shrink(shown_start, shown_duration);
        if !fits(shown_label, shown_start, shown_duration) {
            shown_duration = 0;
            shown_label = shrink(shown_start, 0);
        }
        if !fits(shown_label, shown_start, shown_duration) {
            shown_start = 0;
            shown_label = shrink(0, 0);
        }

        let rows = positions
            .iter()
            .enumerate()
            .map(|(index, position)| {
                let (glyph, role) = status_look(position.status);
                let name = fit_step_name(position.icon(), &one_line(&position.label), shown_label);
                let padding = " ".repeat(shown_label.saturating_sub(width_of(&name)));
                let mut tail: Vec<String> = Vec::new();
                let mut push = |columns: &[&Column]| {
                    for column in columns.iter().filter(|column| column.width() > 0) {
                        tail.push(format!("{:COLUMN_GAP$}{}", "", column.text(index)));
                    }
                };
                if shown_start > 0 {
                    push(&[&start]);
                }
                if shown_duration > 0 {
                    push(&[&duration]);
                }
                push(&main_refs);
                push(&sub_refs);
                // A row ends with its last value: no spaces trail after it.
                let kept = tail.iter().rposition(|cell| !cell.trim().is_empty());
                let mut spans = vec![Span::styled(glyph, styles.of(grey(role))), Span::raw(" ")];
                match kept {
                    None => spans.push(Span::styled(name, styles.of(grey(Role::Calm)))),
                    Some(last) => {
                        spans.push(Span::styled(
                            format!("{name}{padding}"),
                            styles.of(grey(Role::Calm)),
                        ));
                        tail.truncate(last + 1);
                        if let Some(cell) = tail.last_mut() {
                            *cell = cell.trim_end().to_string();
                        }
                        spans.extend(
                            tail.into_iter()
                                .map(|cell| Span::styled(cell, styles.of(Role::Done))),
                        );
                    }
                }
                vec![Line::from(spans)]
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
    let (glyph, role) = status_look(position.status);
    let mut head = vec![
        Span::styled(format!("{glyph} "), styles.of(role)),
        Span::styled(
            step_name(position.icon(), &one_line(&position.label)),
            styles.of(Role::Focus),
        ),
    ];
    if let Some(action) = &position.action_type {
        head.push(Span::styled(
            format!(" · {}", one_line(action)),
            styles.of(Role::Done),
        ));
    }
    if let (Some("decision"), Some(by)) = (position.action_type.as_deref(), &position.decision_by) {
        head.push(Span::styled(
            format!(" · {}", one_line(by)),
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

    let now = progress
        .timeline()
        .and_then(|response| response.observed_at)
        .unwrap_or_else(chrono::Utc::now);
    let layout = AxisLayout::new(progress.positions(), area.width, now, styles, stale);
    let axis_width = layout.width.min(area.width);
    let axis = Rect::new(area.x, area.y, axis_width, area.height);
    let content = Rect::new(
        area.x + axis_width + AXIS_GAP,
        area.y,
        area.width.saturating_sub(axis_width + AXIS_GAP),
        area.height,
    );

    let items = layout.rows;
    let selected = progress.selected_index();
    render_bracketed_list_plain(
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
    use narsor_protocol::api::{PositionUsage, SubagentStats, TokenCounts};

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
            decision_by: None,
        }
    }

    fn used(total: (u64, u64), subagents: Option<(u64, u64)>, cost: f64) -> Option<PositionUsage> {
        Some(PositionUsage {
            total: Some(counts(total.0, total.1)),
            subagents: subagents.map(|(input, output)| counts(input, output)),
            subagent_count: subagents.map(|_| 2),
            subagent_stats: None,
            cost_usd: Some(cost),
            ..PositionUsage::default()
        })
    }

    fn text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn layout(positions: &[Position], width: u16) -> AxisLayout {
        AxisLayout::new(
            positions,
            width,
            chrono::Utc::now(),
            &Styles::new(false),
            false,
        )
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
        assert!(rows[0].contains("2×  ↑40k  ↓3k"), "{}", rows[0]);
        for (a, b) in [("$0.42", "$0.03"), ("↑120k", "↑2.1k"), ("↓8k", "↓400")] {
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
        assert_eq!(row, "✓ script", "{row}");
        assert!(!row.contains('$') && !row.contains('—'), "{row}");
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
                width - axis.width - AXIS_GAP >= (width * 3).div_ceil(10),
                "{width}: {}",
                axis.width
            );
        }
        assert!(layout(&sample(), 200).width > 34);
    }

    #[test]
    fn the_subagents_stay_in_the_first_row_even_on_a_narrow_tab() {
        let axis = layout(&sample(), 60);
        assert_eq!(axis.rows[0].len(), 1);
        let first = text(&axis.rows[0][0]);
        assert!(
            first.contains("$0.42") && first.contains("↑120k"),
            "{first}"
        );
        assert_eq!(axis.rows[1].len(), 1);
    }

    #[test]
    fn without_a_report_or_a_start_the_axis_is_the_glyph_and_the_name() {
        for usage in [None, Some(PositionUsage::default())] {
            let steps = [step("review #1", usage)];
            for width in [20, 60, 200] {
                let axis = layout(&steps, width);
                assert_eq!(axis.width, (width / 3).clamp(18, AXIS_MAX_WIDTH));
                assert_eq!(axis.rows[0].len(), 1);
                assert_eq!(text(&axis.rows[0][0]), "✓ review #1");
            }
        }
    }

    #[test]
    fn metric_columns_are_two_spaces_apart() {
        let axis = layout(&sample(), 200);
        let row = text(&axis.rows[0][0]);
        assert!(row.contains("$0.42  ↑120k   ↓8k"), "{row}");
    }

    fn timed_steps() -> Vec<Position> {
        let now = chrono::Utc::now();
        let mut steps = sample();
        steps[0].started_at = Some(now - chrono::Duration::seconds(90));
        steps[0].ended_at = Some(now);
        steps[0].harness = Some("claude-code".into());
        steps[0].model = Some("sonnet".into());
        steps[0].effort = Some("high".into());
        steps[1].harness = Some("codex".into());
        steps
    }

    #[test]
    fn the_first_row_has_the_times_and_the_second_the_agent() {
        let steps = timed_steps();
        let axis = layout(&steps, 200);
        assert_eq!(axis.rows[0].len(), 2);
        assert_eq!(axis.rows[1].len(), 2);
        let first = text(&axis.rows[0][0]);
        assert!(first.starts_with("✓ "), "{first:?}");
        assert!(first.contains("review_api"), "{first:?}");
        let start = clock(steps[0].started_at);
        assert!(first.contains(&start), "{first:?}");
        assert!(first.contains("1m"), "{first:?}");
        assert!(
            first.find(&start) < first.find("$0.42"),
            "times before usage: {first:?}"
        );
        let second = text(&axis.rows[0][1]);
        assert_eq!(second, "   sonnet  high  claude-code");
        assert_eq!(text(&axis.rows[1][1]).trim(), "codex");
    }

    #[test]
    fn a_step_without_a_start_has_empty_time_fields_and_no_trailing_separator() {
        let mut steps = [step("lint", None), step("review", None)];
        steps[1].started_at = Some(chrono::Utc::now() - chrono::Duration::seconds(5));
        let axis = layout(&steps, 200);
        assert_eq!(text(&axis.rows[0][0]), "✓ lint");
        assert!(text(&axis.rows[1][0]).starts_with("✓ review"));
    }

    #[test]
    fn the_times_stand_in_the_same_columns_in_every_row() {
        let now = chrono::Utc::now();
        let mut steps = [
            step("a", None),
            step("a_longer_name", None),
            step("mid", None),
        ];
        for (index, seconds) in [5, 700, 4_000].into_iter().enumerate() {
            steps[index].started_at = Some(now - chrono::Duration::seconds(seconds));
            steps[index].ended_at = Some(now);
        }
        let axis = layout(&steps, 200);
        let rows: Vec<String> = axis.rows.iter().map(|rows| text(&rows[0])).collect();
        let starts: Vec<usize> = steps
            .iter()
            .zip(&rows)
            .map(|(step, row)| column_of(row, &clock(step.started_at)))
            .collect();
        assert!(starts.windows(2).all(|pair| pair[0] == pair[1]), "{rows:?}");
        let ends: Vec<usize> = rows.iter().map(|row| row.chars().count()).collect();
        assert!(ends.windows(2).all(|pair| pair[0] == pair[1]), "{rows:?}");
    }

    #[test]
    fn a_running_step_counts_its_duration_up_to_now() {
        let now = chrono::Utc::now();
        let mut steps = [step("run", None)];
        steps[0].status = PositionStatus::InProgress;
        steps[0].started_at = Some(now - chrono::Duration::seconds(120));
        let axis = AxisLayout::new(&steps, 200, now, &Styles::new(false), false);
        let row = text(&axis.rows[0][0]);
        assert!(row.starts_with("● run"), "{row:?}");
        assert!(row.ends_with("2m"), "{row:?}");
    }

    #[test]
    fn no_row_of_the_axis_spells_the_status_out() {
        for status in [
            PositionStatus::Succeeded,
            PositionStatus::Failed,
            PositionStatus::Interrupted,
            PositionStatus::InProgress,
            PositionStatus::AwaitingDecision,
        ] {
            let mut steps = timed_steps();
            steps[0].status = status;
            let axis = layout(&steps, 200);
            for row in axis.rows.iter().flatten().map(text) {
                for word in ["w toku", "ok", "błąd", "przerwany", "czeka na decyzję"] {
                    assert!(
                        !row.split_whitespace().any(|part| part == word)
                            && !row.contains(" w toku"),
                        "{status:?}: {row:?}"
                    );
                }
            }
        }
        assert_eq!(status_look(PositionStatus::InProgress), ("●", Role::Live));
    }

    #[test]
    fn a_narrow_axis_cuts_the_name_first_then_the_duration_then_the_start() {
        let now = chrono::Utc::now();
        let mut steps = [step("a_really_long_step_label", None)];
        steps[0].started_at = Some(now - chrono::Duration::seconds(90));
        steps[0].ended_at = Some(now);
        let start = clock(steps[0].started_at);
        let mut seen = (false, false, false);
        for width in (0..=200u16).rev() {
            let axis = AxisLayout::new(&steps, width, now, &Styles::new(false), false);
            let row = text(&axis.rows[0][0]);
            assert!(row.contains("✓ "), "{width}: {row:?}");
            let (cut, has_start, has_duration) = (
                row.contains('…'),
                row.contains(&start),
                row.trim_end().ends_with("1m"),
            );
            if cut && has_start && has_duration {
                seen.0 = true;
            }
            if has_start && !has_duration {
                seen.1 = true;
                assert!(
                    cut,
                    "{width}: the name is cut before the duration goes: {row:?}"
                );
            }
            if !has_start {
                seen.2 = true;
                assert!(!has_duration, "{width}: {row:?}");
                assert!(row.starts_with("✓ "), "{width}: {row:?}");
            }
        }
        assert!(seen.0 && seen.1 && seen.2, "{seen:?}");
    }

    #[test]
    fn the_subagent_stats_stand_in_the_first_row() {
        let mut steps = [step("a", used((1_000, 100), None, 0.1))];
        steps[0].usage.as_mut().unwrap().subagent_stats = Some(SubagentStats {
            spawned: 3,
            completed: 2,
            failed: 1,
            tokens: 45_000,
            ..SubagentStats::default()
        });
        let axis = layout(&steps, 200);
        assert_eq!(axis.rows[0].len(), 1);
        let first = text(&axis.rows[0][0]);
        assert!(first.contains("3×") && first.contains("↕45k"), "{first}");
        assert!(!first.contains("✗") && !first.contains("sub 3"), "{first}");
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
        let mut timed = steps.clone();
        timed[0].started_at = Some(chrono::Utc::now());
        for width in 0..=60u16 {
            assert_eq!(layout(&timed, width).rows.len(), 2);
        }
    }

    fn header(position: &Position) -> Vec<String> {
        step_lines(position, chrono::Utc::now(), &Styles::new(false))
            .iter()
            .map(text)
            .collect()
    }

    #[test]
    fn the_step_header_does_not_say_the_status() {
        let mut running = step("work", None);
        running.status = PositionStatus::InProgress;
        running.action_type = Some("agent".into());
        running.started_at = Some(chrono::Utc::now() - chrono::Duration::seconds(5));
        let lines = header(&running);
        assert!(
            lines[0].starts_with("● ") && lines[0].contains("work"),
            "{lines:?}"
        );
        assert!(!lines[0].contains("w toku"), "{lines:?}");
        assert!(lines[0].contains(" · agent · "), "{lines:?}");

        let mut decision = step("approval", None);
        decision.action_type = Some("decision".into());
        decision.decision_by = Some("human".into());
        let head = header(&decision).remove(0);
        assert!(
            head.starts_with("✓ ") && head.ends_with("approval · decision · human"),
            "{head}"
        );
    }

    #[test]
    fn the_step_header_keeps_the_error_and_the_notes() {
        let mut failed = step("work", None);
        failed.status = PositionStatus::Failed;
        failed.error = Some("boom".into());
        failed.retry_pending = true;
        let lines = header(&failed);
        assert_eq!(lines[0], "✗ work");
        assert_eq!(lines[1], "✗ boom");
        assert_eq!(lines[2], "ponowienie oczekuje");
    }
}
