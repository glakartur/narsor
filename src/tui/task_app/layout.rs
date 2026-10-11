//! The screen: geometry, the header line and the shortcut bar.
//!
//! ```text
//! narsor · local ● live      harness claude-code (konfiguracja) · sloty 2/3 · ? 1 · ! 1
//! ┏ Zadania ━━━━━━━━━━━━━━━━┓┌ NAR-14 Edycja ───────────────────────────┐
//! ┃ ● NAR-14  edit          ┃│ details                                  │
//! ┃ …                       ┃│                                          │
//! ┗━━━━━━━━━━━━━━━━━━━━━━━━━┛└──────────────────────────────────────────┘
//! keys …                                                    notification
//! ```
//!
//! There is no frame around the screen: the header is the first row, the
//! bar the last, and each panel between them draws its own frame
//! ([`Styles::panel`]) — thick in the focus role when it has the keys, thin
//! and grey when it does not (R1–R3).
//!
//! # Three layouts (origin §1)
//!
//! * `100` columns or more: the list is a narrow column ([`LIST_WIDTH`]) and
//!   the details take the rest — [`Screen::Split`];
//! * fewer than `100`: the list and the details switch full screen, `Enter`
//!   and `Esc` — [`Screen::Full`], showing whichever has the focus (so the
//!   one panel shown always has the focus frame, R5);
//! * fewer than [`MIN_WIDTH`]×[`MIN_HEIGHT`]: only a message asking for a
//!   bigger window — [`Screen::TooSmall`].
//!
//! [`areas`] is pure geometry, so `tests/tui_render.rs` checks the three
//! without drawing anything. [`render`] draws one frame of a
//! [`super::TaskApp`]: the header, the list, the details and the bar.
//!
//! The details are [`super::detail`]: they draw their frame and what is in
//! it into [`Areas::detail`].

use std::time::Instant;

use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::super::keymap::{self, Context};
use super::super::scroll::one_line;
use super::super::style::{Role, Styles};
use super::harness::HarnessHeader;
use super::list::{fit as fit_text, Counters, Slots, TaskList};
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
    /// The whole screen.
    pub outer: Rect,
    /// The first row: the header.
    pub header: Rect,
    /// The list, when it is shown.
    pub list: Option<Rect>,
    /// The details, when they are shown.
    pub detail: Option<Rect>,
    /// The shortcut bar and the notification.
    pub bar: Rect,
}

/// The first row of `area`: a single line for a notice above a body. Empty
/// when `area` has no height, so that nothing is drawn outside it.
pub fn first_row(area: Rect) -> Rect {
    Rect::new(area.x, area.y, area.width, area.height.min(1))
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
            header: Rect::default(),
            list: None,
            detail: None,
            bar: Rect::default(),
        };
    }
    let header = Rect::new(area.x, area.y, area.width, 1);
    let bar = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
    let body = Rect::new(area.x, area.y + 1, area.width, area.height - 2);
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
        header,
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
    render_header(frame, areas.header, app, styles, now);

    let list_focused = app.keys_on_list();
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
        render_view(frame, detail_area, app, styles, stale);
    }
    render_bar(frame, areas.bar, app, styles, now);
    let body = overlay_area(areas.outer);
    actions::render(frame, body, app, styles);
    if app.is_help_open() {
        if app.active_agent_transcript_mode() == Some(crate::tui::transcript::DisplayMode::Raw) {
            keymap::render_raw_conversation_help(frame, body, app.key_context(), styles);
        } else {
            keymap::render_help(frame, body, app.key_context(), styles);
        }
    }
}

/// Where the overlays go: between the header and the bar, a column in from
/// either side of the screen.
fn overlay_area(screen: Rect) -> Rect {
    Rect::new(
        screen.x + 1,
        screen.y + 1,
        screen.width.saturating_sub(2),
        screen.height.saturating_sub(2),
    )
}

/// Draws what the details area shows: the details of the selected task, or
/// the view `n`, `H` or `R` opened.
fn render_view(frame: &mut Frame, area: Rect, app: &mut TaskApp, styles: &Styles, stale: bool) {
    let place = app.list.next_queue_place();
    let focused = app.focus == Focus::Details;
    let hyperlinks = app.detail.hyperlinks();
    match &mut app.view {
        View::Tasks => detail::render(frame, area, app, styles, stale),
        View::NewTask(form) => new_task::render(frame, area, form, place, styles, focused),
        View::History(open) => {
            history::render_details(frame, area, open, styles, focused, stale, hyperlinks);
        }
        View::Repositories(open) => repositories::render(frame, area, open, styles, focused),
    }
}

fn render_too_small(frame: &mut Frame, area: Rect, styles: &Styles) {
    let lines = vec![
        Line::styled(
            "Terminal za mały — powiększ okno",
            styles.of(Role::Attention),
        ),
        Line::styled(
            format!(
                "min. {MIN_WIDTH}×{MIN_HEIGHT}, jest {}×{}",
                area.width, area.height
            ),
            styles.of(Role::Done),
        ),
    ];
    let top = area.y + area.height.saturating_sub(2) / 2;
    let centred = Rect::new(
        area.x,
        top,
        area.width,
        area.height.saturating_sub(top - area.y).min(2),
    );
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), centred);
}

/// The header row: [`header_left`] from the left edge, [`header_right`] at
/// the right one, shortened by [`header_lines`] to fit. The right part wins
/// the room it needs; the left one is cut at it.
fn render_header(frame: &mut Frame, area: Rect, app: &TaskApp, styles: &Styles, now: Instant) {
    let (left, right) = header_lines(app, styles, now, usize::from(area.width));
    let right_width = u16::try_from(right.width())
        .unwrap_or(u16::MAX)
        .min(area.width);
    let left_width = area.width - right_width;
    frame.render_widget(
        Paragraph::new(left),
        Rect::new(area.x, area.y, left_width, area.height),
    );
    frame.render_widget(
        Paragraph::new(right),
        Rect::new(area.x + left_width, area.y, right_width, area.height),
    );
}

/// How far the header gave up detail to fit, step by step (R12): first the
/// slots, then the age of the data, then `(konfiguracja)` becomes `(konf.)`,
/// then the connection's name. What is still missing comes off the harness
/// name. The counters `? n · ! n` always stay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Fit {
    Full,
    NoSlots,
    NoAge,
    ShortConfig,
    NoName,
}

impl Fit {
    const ALL: [Fit; 5] = [
        Fit::Full,
        Fit::NoSlots,
        Fit::NoAge,
        Fit::ShortConfig,
        Fit::NoName,
    ];
}

/// The two parts of the header for `width` columns, shortened in the order of
/// [`Fit`] until they fit.
fn header_lines(
    app: &TaskApp,
    styles: &Styles,
    now: Instant,
    width: usize,
) -> (Line<'static>, Line<'static>) {
    let [shorter @ .., tightest] = Fit::ALL;
    for fit in shorter {
        let left = header_left(app, styles, now, fit);
        let right = header_right(app, styles, fit, usize::MAX);
        if left.width() + right.width() <= width {
            return (left, right);
        }
    }
    let left = header_left(app, styles, now, tightest);
    let right = header_right(app, styles, tightest, usize::MAX);
    let over = (left.width() + right.width()).saturating_sub(width);
    if over == 0 {
        return (left, right);
    }
    let name = harness_name(app).chars().count();
    let room = name.saturating_sub(over).max(1);
    (left, header_right(app, styles, tightest, room))
}

/// `narsor · name ● live`, and how old the data is when it is not live.
fn header_left(app: &TaskApp, styles: &Styles, now: Instant, fit: Fit) -> Line<'static> {
    let connection = app.connection();
    let mut spans = vec![Span::styled(" narsor ", styles.of(Role::Focus))];
    if fit < Fit::NoName {
        spans.push(Span::styled(
            format!("· {} ", connection.name()),
            styles.of(Role::Calm),
        ));
    }
    spans.push(connection.status(now).span(styles));
    if fit < Fit::NoAge {
        if let Some(age) = connection.age_label(now) {
            spans.push(Span::styled(format!(" · {age}"), styles.of(Role::Done)));
        }
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

/// The name of the harness the header shows, before any cut.
fn harness_name(app: &TaskApp) -> String {
    match app.harness_header() {
        HarnessHeader::Unknown => "?".to_string(),
        HarnessHeader::Pending(name) | HarnessHeader::Known { name, .. } => one_line(name),
    }
}

/// `harness claude-code (konfiguracja)`, `harness opencode`, `harness ?`, or
/// `harness opencode…` while a change waits for its answer. The name is cut
/// to `room` columns.
fn harness_spans(app: &TaskApp, styles: &Styles, fit: Fit, room: usize) -> Vec<Span<'static>> {
    let name = fit_text(&harness_name(app), room);
    let mut spans = vec![
        Span::styled("harness ", styles.of(Role::Done)),
        Span::styled(name, styles.of(Role::Calm)),
    ];
    match app.harness_header() {
        HarnessHeader::Pending(_) => spans.push(Span::styled("…", styles.of(Role::Done))),
        HarnessHeader::Known {
            configured: true, ..
        } => {
            let note = if fit < Fit::ShortConfig {
                " (konfiguracja)"
            } else {
                " (konf.)"
            };
            spans.push(Span::styled(note, styles.of(Role::Done)));
        }
        _ => {}
    }
    spans
}

/// `harness … · sloty 2/3 · ? 1 · ! 1`; the slots go once `fit` says so.
fn header_right(app: &TaskApp, styles: &Styles, fit: Fit, room: usize) -> Line<'static> {
    let Counters {
        questions,
        problems,
    } = app.list().counters();
    let separator = || Span::styled(" · ", styles.of(Role::Done));
    let mut spans = vec![Span::raw(" ")];
    spans.extend(harness_spans(app, styles, fit, room));
    if fit < Fit::NoSlots {
        if let Some(Slots { used, limit }) = app.list().slots() {
            let text = match limit {
                Some(limit) => format!("sloty {used}/{limit}"),
                None => format!("sloty {used}"),
            };
            spans.push(separator());
            spans.push(Span::styled(text, styles.of(Role::Calm)));
        }
    }
    let count_role = |count: usize, role: Role| if count == 0 { Role::Done } else { role };
    spans.push(separator());
    spans.push(Span::styled(
        format!("? {questions}"),
        styles.of(count_role(questions, Role::Attention)),
    ));
    spans.push(separator());
    spans.push(Span::styled(
        format!("! {problems}"),
        styles.of(count_role(problems, Role::Problem)),
    ));
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
    // Inside the list's frame, on its first row.
    frame.render_widget(
        Paragraph::new(line),
        Rect::new(
            area.x + 1,
            area.y + 1,
            area.width.saturating_sub(2),
            area.height.saturating_sub(2).min(1),
        ),
    );
}

fn render_bar(frame: &mut Frame, area: Rect, app: &TaskApp, styles: &Styles, now: Instant) {
    let notification = app.notifier().span(styles, now);
    let note_width = notification
        .as_ref()
        .map_or(0, |span| span.content.chars().count() + 1)
        .min(usize::from(area.width) / 2);
    let hints_width = usize::from(area.width).saturating_sub(note_width + 1);
    let hints =
        if app.active_agent_transcript_mode() == Some(crate::tui::transcript::DisplayMode::Raw) {
            keymap::raw_conversation_hint_line(app.key_context(), styles, hints_width)
        } else {
            keymap::hint_line(app.key_context(), styles, hints_width)
        };
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
