//! *Przebieg*: the step timeline (one row per attempt of an action, like
//! `narsor steps`) and the content of the selected step.
//!
//! The selected step follows the one that is running ([`Progress::follows`]):
//! a read that brings a new current step moves the selection there, until the
//! operator moves it by hand or presses `f` — and `f` again picks the running
//! step back up. What a refresh must not touch — the selected step, the mode —
//! is the state of [`Progress`]; what it replaces is the timeline.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::controller_protocol::TimelineResponse;
use crate::engine::{ConversationState, Position, PositionStatus};
use crate::tui::style::{Role, Styles};

use super::super::super::scroll::one_line;
use super::super::layout::first_row;
use super::super::list::fit;
use super::conversation::{self, Attempt, Attempts};

/// Widest the axis grows, frame included.
const AXIS_MAX_WIDTH: u16 = 34;

/// What the timeline remembers per task.
#[derive(Debug, Clone)]
pub struct Progress {
    timeline: Option<TimelineResponse>,
    failure: Option<String>,
    revision: Option<u64>,
    selected: Option<String>,
    follow: bool,
    top: usize,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            timeline: None,
            failure: None,
            revision: None,
            selected: None,
            follow: true,
            top: 0,
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
    /// The selection, the cursor and the mode stay; with the follow on, the
    /// selection moves to the running step.
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
                if self.follow || lost {
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

/// One row of the axis.
fn axis_line(
    position: &Position,
    selected: bool,
    focused: bool,
    width: usize,
    styles: &Styles,
    stale: bool,
) -> Line<'static> {
    let (glyph, role, word) = status_look(position.status);
    let grey = |role: Role| if stale { Role::Done } else { role };
    let label = fit(
        &one_line(&position.label),
        width.saturating_sub(4 + word.chars().count()),
    );
    let mut line = Line::from(vec![
        Span::styled(
            if selected { "▸" } else { " " },
            styles.of(grey(Role::Focus)),
        ),
        Span::styled(glyph, styles.of(grey(role))),
        Span::raw(" "),
        Span::styled(label, styles.of(grey(Role::Calm))),
        Span::styled(format!(" {word}"), styles.of(Role::Done)),
    ]);
    if selected && focused {
        line = line.style(styles.of(Role::Selected));
    }
    line
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
pub fn render(
    frame: &mut Frame,
    area: Rect,
    styles: &Styles,
    progress: &mut Progress,
    attempts: &mut Attempts,
    stale: bool,
    focused: bool,
) {
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
        let text = if progress.timeline().is_none() && progress.failure().is_none() {
            "wczytuję przebieg…"
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

    let axis_width = (area.width / 3).clamp(18, AXIS_MAX_WIDTH).min(area.width);
    let axis = Rect::new(area.x, area.y, axis_width, area.height);
    let content = Rect::new(
        area.x + axis_width + 1,
        area.y,
        area.width.saturating_sub(axis_width + 1),
        area.height,
    );

    // The axis keeps its top while the cursor stays in view.
    let height = usize::from(axis.height).max(1);
    let cursor = progress.selected_index().unwrap_or(0);
    if cursor < progress.top {
        progress.top = cursor;
    } else if cursor >= progress.top + height {
        progress.top = cursor + 1 - height;
    }
    let lines: Vec<Line> = progress
        .positions()
        .iter()
        .enumerate()
        .skip(progress.top)
        .take(height)
        .map(|(index, position)| {
            axis_line(
                position,
                index == cursor,
                focused,
                usize::from(axis.width),
                styles,
                stale,
            )
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), axis);
    if content.width < 8 {
        return;
    }
    render_step(frame, content, styles, progress, attempts, stale);
}

/// The right-hand side: the facts of the selected step, then its content.
fn render_step(
    frame: &mut Frame,
    area: Rect,
    styles: &Styles,
    progress: &Progress,
    attempts: &mut Attempts,
    stale: bool,
) {
    let Some(position) = progress.selected_position() else {
        return;
    };
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
        (ConversationState::Recorded, Some(attempt_id)) => match attempts.get_mut(attempt_id) {
            Some(attempt) => render_attempt(frame, body, styles, attempt, stale),
            None => message(frame, "wczytuję rozmowę kroku…".to_string()),
        },
        (ConversationState::Recorded, None) | (ConversationState::NoRecord, _) => {
            message(frame, "brak zapisu rozmowy tego kroku".to_string());
        }
        (ConversationState::NotCreated { reason }, _) => {
            message(frame, format!("brak rozmowy: {}", one_line(reason)));
        }
    }
}

fn render_attempt(
    frame: &mut Frame,
    area: Rect,
    styles: &Styles,
    attempt: &mut Attempt,
    stale: bool,
) {
    if let Some(failure) = attempt.failure() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!("✗ nie udało się wczytać rozmowy: {}", one_line(failure)),
                styles.of(Role::Problem),
            )),
            first_row(area),
        );
        if attempt.is_empty() {
            return;
        }
    }
    if attempt.no_record() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "runner nie ma zapisu rozmowy tego kroku",
                styles.of(Role::Done),
            )),
            area,
        );
        return;
    }
    conversation::render_transcript(frame, area, styles, attempt.transcript_mut(), stale);
}
