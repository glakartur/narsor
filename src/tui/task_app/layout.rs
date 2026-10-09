//! The screen: geometry and the frame around the list and the details.
//!
//! ```text
//! ┌ narsor · local ● live ───────────────────── sloty 2/3 · ? 1 · ! 1 ┐
//! │ Zadania            │ details                                       │
//! │ ● NAR-14  edit     │                                               │
//! │ …                  │                                               │
//! │ keys …                                          notification       │
//! └──────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Three layouts (origin §1)
//!
//! * `100` columns or more: the list is a narrow column ([`LIST_WIDTH`]) and
//!   the details take the rest — [`Screen::Split`];
//! * fewer than `100`: the list and the details switch full screen, `Enter`
//!   and `Esc` — [`Screen::Full`], showing whichever has the focus;
//! * fewer than [`MIN_WIDTH`]×[`MIN_HEIGHT`]: only a message asking for a
//!   bigger window — [`Screen::TooSmall`].
//!
//! [`areas`] is pure geometry, so `tests/tui_render.rs` checks the three
//! without drawing anything. [`render`] draws one frame of a
//! [`super::TaskApp`]: the header, the list, the details and the bar.
//!
//! The details are [`super::detail`]: they draw into [`Areas::detail`].

use std::time::Instant;

use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use super::super::keymap::{self, Context};
use super::super::style::{Role, Styles};
use super::list::{Counters, Slots, TaskList};
use super::{actions, detail, history, new_task, repositories};
use super::{Focus, TaskApp, View};

/// The narrowest terminal that shows anything but a request for more room.
pub const MIN_WIDTH: u16 = 60;

/// The lowest terminal that shows anything but a request for more room.
pub const MIN_HEIGHT: u16 = 20;

/// From this width the list and the details sit side by side.
pub const SPLIT_MIN_WIDTH: u16 = 100;

/// The width of the list column in the split layout, its frame included.
pub const LIST_WIDTH: u16 = 28;

/// Which of the three layouts a terminal gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// List and details side by side.
    Split,
    /// One of them at a time, full screen.
    Full,
    /// Too small for either.
    TooSmall,
}

/// Where everything goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Areas {
    /// Which layout it is.
    pub screen: Screen,
    /// The frame around everything.
    pub outer: Rect,
    /// The list, when it is shown.
    pub list: Option<Rect>,
    /// The details, when they are shown.
    pub detail: Option<Rect>,
    /// The shortcut bar and the notification.
    pub bar: Rect,
}

/// The layout for a terminal of `area`, with the focus on `focus`.
pub fn areas(area: Rect, focus: Focus) -> Areas {
    let screen = if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        Screen::TooSmall
    } else if area.width < SPLIT_MIN_WIDTH {
        Screen::Full
    } else {
        Screen::Split
    };
    if screen == Screen::TooSmall {
        return Areas {
            screen,
            outer: area,
            list: None,
            detail: None,
            bar: Rect::default(),
        };
    }
    let inner = Rect::new(
        area.x + 1,
        area.y + 1,
        area.width - 2,
        area.height - 2,
    );
    let bar = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
    let body = Rect::new(inner.x, inner.y, inner.width, inner.height - 1);
    let (list, detail) = match (screen, focus) {
        (Screen::Split, _) => {
            let list = Rect::new(body.x, body.y, LIST_WIDTH, body.height);
            let detail = Rect::new(
                body.x + LIST_WIDTH,
                body.y,
                body.width - LIST_WIDTH,
                body.height,
            );
            (Some(list), Some(detail))
        }
        (_, Focus::List) => (Some(body), None),
        (_, Focus::Details) => (None, Some(body)),
    };
    Areas {
        screen,
        outer: area,
        list,
        detail,
        bar,
    }
}

/// Draws one frame of `app`. `now` decides how old the data looks and
/// whether the notification is still on the bar.
pub fn render(frame: &mut Frame, app: &mut TaskApp, styles: &Styles, now: Instant) {
    let areas = areas(frame.area(), app.focus());
    if areas.screen == Screen::TooSmall {
        render_too_small(frame, areas.outer, styles);
        return;
    }

    let stale = app.connection().is_stale();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(styles.of(Role::Done))
        .title(header_left(app, styles, now))
        .title(header_right(app, styles, usize::from(areas.outer.width)).right_aligned());
    frame.render_widget(block, areas.outer);

    let list_focused = app.focus() == Focus::List;
    if let Some(list_area) = areas.list {
        if let View::History(open) = &mut app.view {
            history::render_list(frame, list_area, open, styles, list_focused, stale);
        } else {
            app.list_mut()
                .render(frame, list_area, styles, list_focused, stale);
            if app.list().tasks().is_empty() && app.list().pending().is_empty() {
                render_empty(frame, list_area, app, styles);
            }
        }
    }
    if let Some(detail_area) = areas.detail {
        let detail_area = if areas.screen == Screen::Split {
            Rect::new(
                detail_area.x + 1,
                detail_area.y,
                detail_area.width.saturating_sub(1),
                detail_area.height,
            )
        } else {
            detail_area
        };
        render_view(frame, detail_area, app, styles, stale);
    }
    render_bar(frame, areas.bar, app, styles, now);
    let body = Rect::new(
        areas.outer.x + 2,
        areas.outer.y + 1,
        areas.outer.width.saturating_sub(4),
        areas.outer.height.saturating_sub(3),
    );
    actions::render(frame, body, app, styles);
    if app.is_help_open() {
        keymap::render_help(frame, body, app.key_context(), styles);
    }
}

/// Draws what the details area shows: the details of the selected task, or
/// the view `n`, `H` or `R` opened.
fn render_view(frame: &mut Frame, area: Rect, app: &mut TaskApp, styles: &Styles, stale: bool) {
    let place = app.list.next_queue_place();
    let focused = app.focus == Focus::Details;
    let hyperlinks = app.detail.hyperlinks();
    match &mut app.view {
        View::Tasks => detail::render(frame, area, app, styles, stale),
        View::NewTask(form) => new_task::render(frame, area, form, place, styles),
        View::History(open) => {
            history::render_details(frame, area, open, styles, focused, stale, hyperlinks);
        }
        View::Repositories(open) => repositories::render(frame, area, open, styles),
    }
}

fn render_too_small(frame: &mut Frame, area: Rect, styles: &Styles) {
    let lines = vec![
        Line::styled("Terminal za mały — powiększ okno", styles.of(Role::Attention)),
        Line::styled(
            format!(
                "min. {MIN_WIDTH}×{MIN_HEIGHT}, jest {}×{}",
                area.width, area.height
            ),
            styles.of(Role::Done),
        ),
    ];
    let top = area.y + area.height.saturating_sub(2) / 2;
    let centred = Rect::new(area.x, top, area.width, area.height.saturating_sub(top - area.y).min(2));
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), centred);
}

/// `narsor · name ● live`, and how old the data is when it is not live.
fn header_left(app: &TaskApp, styles: &Styles, now: Instant) -> Line<'static> {
    let connection = app.connection();
    let mut spans = vec![
        Span::styled(" narsor ", styles.of(Role::Focus)),
        Span::styled(format!("· {} ", connection.name()), styles.of(Role::Calm)),
        connection.status(now).span(styles),
    ];
    if let Some(age) = connection.age_label(now) {
        spans.push(Span::styled(format!(" · {age}"), styles.of(Role::Done)));
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

/// `sloty 2/3 · ? 1 · ! 1`, shortened from the left when it does not fit.
fn header_right(app: &TaskApp, styles: &Styles, width: usize) -> Line<'static> {
    let Counters { questions, problems } = app.list().counters();
    let mut parts: Vec<Span<'static>> = Vec::new();
    if let Some(Slots { used, limit }) = app.list().slots() {
        let text = match limit {
            Some(limit) => format!("sloty {used}/{limit}"),
            None => format!("sloty {used}"),
        };
        parts.push(Span::styled(text, styles.of(Role::Calm)));
    }
    let count_role = |count: usize, role: Role| if count == 0 { Role::Done } else { role };
    parts.push(Span::styled(
        format!("? {questions}"),
        styles.of(count_role(questions, Role::Attention)),
    ));
    parts.push(Span::styled(
        format!("! {problems}"),
        styles.of(count_role(problems, Role::Problem)),
    ));
    // Leave room for the left title: drop `sloty` before the counters.
    let length = |parts: &[Span]| -> usize {
        parts.iter().map(|part| part.content.chars().count() + 3).sum::<usize>() + 1
    };
    while parts.len() > 2 && length(&parts) > width / 2 {
        parts.remove(0);
    }
    let mut spans = vec![Span::raw(" ")];
    for (index, part) in parts.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", styles.of(Role::Done)));
        }
        spans.push(part);
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

fn render_empty(frame: &mut Frame, area: Rect, app: &TaskApp, styles: &Styles) {
    let text = if app.connection().is_live() {
        "brak zadań"
    } else {
        "brak danych"
    };
    let line = Line::styled(format!(" {text}"), styles.of(Role::Done));
    frame.render_widget(
        Paragraph::new(line),
        Rect::new(area.x, area.y + 1, area.width.saturating_sub(1), 1),
    );
}

fn render_bar(frame: &mut Frame, area: Rect, app: &TaskApp, styles: &Styles, now: Instant) {
    let notification = app.notifier().span(styles, now);
    let note_width = notification
        .as_ref()
        .map_or(0, |span| span.content.chars().count() + 1)
        .min(usize::from(area.width) / 2);
    let hints_width = usize::from(area.width).saturating_sub(note_width + 1);
    let hints = keymap::hint_line(app.key_context(), styles, hints_width);
    frame.render_widget(Paragraph::new(hints), area);
    if let Some(span) = notification {
        frame.render_widget(
            Paragraph::new(Line::from(span)).alignment(Alignment::Right),
            Rect::new(
                area.x + area.width - u16::try_from(note_width).unwrap_or(0),
                area.y,
                u16::try_from(note_width).unwrap_or(0),
                1,
            ),
        );
    }
}

/// The context whose keys the bar and help describe.
pub fn context_of(focus: Focus, list: &TaskList) -> Context {
    if list.moving().is_some() {
        return Context::Move;
    }
    match focus {
        Focus::List => Context::List,
        Focus::Details => Context::Details,
    }
}
