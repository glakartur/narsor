//! Rendering of the remote tasks mode ([`super::remote`]).
//!
//! Same shape as the local interface: bordered panels, a shortcut bar at
//! the bottom whose title carries the last notice, modals as centred
//! overlays. Every text comes from [`super::remote`]; this file only lays it
//! out and colours it.
//!
//! ```text
//! ┌ Remote tasks · default backend ──┬ Task ─────────────────────────┐
//! │ (task, backend, status badge)    │ (detail lines)                │
//! │                                  ├ Conversation ─────────────────┤
//! │                                  │ (relayed page, Detail screen) │
//! ├──────────────────────────────────┴───────────────────────────────┤
//! │ (shortcuts)                                                      │
//! └──────────────────────────────────────────────────────────────────┘
//! ```

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
};
use ratatui::Frame;

use super::remote::{
    default_backend_line, status_badge, PageView, RemoteApp, RemoteModal, RemoteScreen,
    RemoteStream, Tone, ToneLine,
};
use super::scroll::sanitize;
use super::views::centered;
use super::views::transcript_line;

/// Height of the shortcut bar, borders included.
const SHORTCUT_BAR_HEIGHT: u16 = 3;

fn tone_style(tone: Tone) -> Style {
    match tone {
        Tone::Normal => Style::default(),
        Tone::Muted => Style::default().fg(Color::DarkGray),
        Tone::Good => Style::default().fg(Color::Green),
        Tone::Warning => Style::default().fg(Color::LightYellow),
        Tone::Error => Style::default().fg(Color::LightRed),
    }
}

fn tone_line(line: &ToneLine) -> Line<'static> {
    Line::from(Span::styled(sanitize(&line.text), tone_style(line.tone)))
}

fn panel(title: String) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        ))
}

/// Draws the whole remote mode.
pub fn render(frame: &mut Frame, app: &mut RemoteApp) {
    let [body, shortcuts] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(SHORTCUT_BAR_HEIGHT)])
            .areas(frame.area());

    match app.screen() {
        RemoteScreen::List => {
            let [list, detail] =
                Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
                    .areas(body);
            render_list(frame, app, list);
            render_detail(frame, app, detail);
        }
        RemoteScreen::Detail => {
            let [detail, conversation] =
                Layout::vertical([Constraint::Percentage(45), Constraint::Percentage(55)])
                    .areas(body);
            app.set_conversation_viewport(
                conversation.height.saturating_sub(2).max(1) as usize,
                conversation.width.saturating_sub(2).max(1) as usize,
            );
            render_detail(frame, app, detail);
            render_conversation(frame, app, conversation);
        }
    }
    render_shortcuts(frame, app, shortcuts);
    render_modal(frame, app);
}

fn stream_suffix(app: &RemoteApp) -> Span<'static> {
    match app.stream() {
        RemoteStream::Live => Span::styled(" ● live ", Style::default().fg(Color::Green)),
        RemoteStream::Connecting => {
            Span::styled(" ○ connecting ", Style::default().fg(Color::DarkGray))
        }
        RemoteStream::Reconnecting(_) => {
            Span::styled(" ◌ reconnecting ", Style::default().fg(Color::LightYellow))
        }
        RemoteStream::Ended(_) => {
            Span::styled(" ✖ stream ended ", Style::default().fg(Color::LightRed))
        }
    }
}

fn render_list(frame: &mut Frame, app: &RemoteApp, area: Rect) {
    let backend = app
        .default_backend()
        .map(default_backend_line)
        .unwrap_or_default();
    let block = panel(format!(" Remote tasks · {backend} ")).title(stream_suffix(app));
    let items: Vec<ListItem> = app
        .tasks()
        .iter()
        .map(|task| {
            let badge = status_badge(task);
            ListItem::new(Line::from(vec![
                Span::raw(format!("{:<20} ", truncate(task.task_id.as_str(), 20))),
                Span::styled(
                    format!("{:<12} ", truncate(task.backend_id.as_str(), 12)),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(sanitize(&badge.text), tone_style(badge.tone)),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    if !app.tasks().is_empty() {
        state.select(Some(app.selected_index()));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_detail(frame: &mut Frame, app: &RemoteApp, area: Rect) {
    let lines: Vec<Line> = app.selected_lines().iter().map(tone_line).collect();
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(" Task ".to_string()))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_conversation(frame: &mut Frame, app: &RemoteApp, area: Rect) {
    let page = app.detail().and_then(|detail| detail.page.as_ref());
    let title = match (page, app.current_attempt()) {
        (Some(page), _) => format!(" Conversation · {} ", page.attempt_id),
        (None, Some(attempt)) => format!(" Conversation · {attempt} "),
        (None, None) => " Conversation ".to_string(),
    };
    let lines = match page {
        Some(page) => page_lines(page, area.width.saturating_sub(2) as usize),
        None => vec![Line::from(Span::styled(
            "c: read the latest attempt through the control plane · [ ]: other attempts",
            Style::default().fg(Color::DarkGray),
        ))],
    };
    let height = area.height.saturating_sub(2) as usize;
    let offset = page.map_or(0, |page| page.transcript.scroll_offset());
    frame.render_widget(
        Paragraph::new(
            lines
                .into_iter()
                .skip(offset)
                .take(height)
                .collect::<Vec<_>>(),
        )
        .block(panel(title))
        .wrap(Wrap { trim: false }),
        area,
    );
}

fn page_lines(page: &PageView, width: usize) -> Vec<Line<'static>> {
    if page.no_record {
        return vec![Line::from(Span::styled(
            "The runner holds no conversation record for this attempt.",
            Style::default().fg(Color::DarkGray),
        ))];
    }
    let mut lines: Vec<Line> = page
        .transcript
        .logical_lines()
        .into_iter()
        .map(|line| transcript_line(line, width))
        .collect();
    lines.push(Line::from(Span::styled(
        if page.truncated_tail {
            "— incomplete trailing record; retained entries shown —".to_string()
        } else if !page.complete {
            "— loading retained history… —".to_string()
        } else if page.closed {
            "— end of the attempt —".to_string()
        } else {
            "— current end of this open attempt —".to_string()
        },
        Style::default().fg(Color::DarkGray),
    )));
    lines
}

fn render_shortcuts(frame: &mut Frame, app: &RemoteApp, area: Rect) {
    let mut spans = Vec::new();
    for (key, description) in app.shortcuts() {
        spans.push(Span::styled(
            format!(" {key} "),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(format!(" {description}  ")));
    }
    let title = match app.notice() {
        Some(notice) => Span::styled(
            format!(" {} ", sanitize(&notice.text)),
            tone_style(notice.tone),
        ),
        None => Span::styled(" Narsor · remote ", Style::default().fg(Color::DarkGray)),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(title);
    frame.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

fn render_modal(frame: &mut Frame, app: &RemoteApp) {
    let (title, lines, width, height): (String, Vec<Line>, u16, u16) = match app.modal() {
        RemoteModal::None => return,
        RemoteModal::Help => (
            " Keys ".into(),
            [
                "j/k, g/G   move through the tasks",
                "Enter / Esc  open the task / back to the list",
                "r          re-read everything",
                "p          pause a running task at its next safe point",
                "u          resume a paused or interrupted task",
                "s          start an interrupted task afresh",
                "a          archive: delete the workload and its storage",
                "P          change the priority of a queued task",
                "i          answer the input the runner waits for",
                "c, n, [ ]  read the conversation, next page, other attempt",
                "b          default backend of new tasks",
                "q          quit",
            ]
            .iter()
            .map(|text| Line::from(*text))
            .collect(),
            60,
            60,
        ),
        RemoteModal::ConfirmArchive { task_id } => (
            " Archive ".into(),
            vec![
                Line::from(format!("Archive task {task_id}?")),
                Line::from(Span::styled(
                    "This deletes its workload and its task storage on the backend.",
                    Style::default().fg(Color::LightRed),
                )),
                Line::from(""),
                Line::from("y: archive · any other key: cancel"),
            ],
            50,
            30,
        ),
        RemoteModal::Priority { task_id, value, .. } => (
            " Priority ".into(),
            vec![
                Line::from(format!("Priority of queued task {task_id} (higher first):")),
                Line::from(Span::styled(
                    format!("> {value}_"),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from("Enter: apply · Esc: cancel"),
            ],
            50,
            30,
        ),
        RemoteModal::DefaultBackend(picker) => {
            let mut lines = vec![
                Line::from("Backend of tasks created from now on; existing tasks never move."),
                Line::from(""),
            ];
            let mut rows = vec![format!("reset to configured ({})", picker.view.configured)];
            rows.extend(picker.choices.iter().cloned());
            for (index, row) in rows.iter().enumerate() {
                let marker = if picker.view.effective == *row {
                    " (current)"
                } else {
                    ""
                };
                let style = if index == picker.index && picker.typed.is_empty() {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                lines.push(Line::from(Span::styled(format!("  {row}{marker}"), style)));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(format!("other: {}_", picker.typed)));
            lines.push(Line::from(
                "↑/↓ choose · type an id · Enter: apply · Esc: cancel",
            ));
            (" Default backend ".into(), lines, 60, 60)
        }
        RemoteModal::Input(input) => {
            let form = &input.form;
            let mut lines = vec![
                Line::from(Span::styled(
                    sanitize(&form.instruction),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    format!(
                        "answers request {} of task {}",
                        input.pending_input_id, input.task_id
                    ),
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
            ];
            for (index, field) in form.fields.iter().enumerate() {
                let value = form.values.get(index).cloned().unwrap_or_default();
                lines.push(Line::from(format!("{}: {}", field.name, sanitize(&value))));
            }
            if let Some(label) = form.chosen_label() {
                lines.push(Line::from(format!("outcome: {label}")));
            }
            if let Some(error) = &form.error {
                lines.push(Line::from(Span::styled(
                    sanitize(error),
                    Style::default().fg(Color::LightRed),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(
                "Tab: next row · ←/→: choose · Enter: send · Esc: cancel",
            ));
            (" Answer ".into(), lines, 80, 70)
        }
    };
    let area = centered(frame.area(), width, height);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(title))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.to_string()
    } else {
        let mut out: String = text.chars().take(limit.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
