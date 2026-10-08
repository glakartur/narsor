//! Rendering: the panel geometry and the widgets drawn into it.
//!
//! Nothing here decides anything — every value comes from [`App`], which is why
//! the tests drive that and not this. The only thing worth testing in this file
//! is [`layout`], because it is pure geometry and it is what a resize exercises.
//!
//! # The layout
//!
//! ```text
//! ┌ Executions ────────┬ State and context ─────────┐
//! │ (the execution     │ (current state, context    │
//! │  list)             │  variables)                │
//! │                    ├ Events ────────────────────┤
//! │                    │ (the live stream)          │
//! ├────────────────────┴────────────────────────────┤
//! │ (context-sensitive shortcuts)                   │
//! └─────────────────────────────────────────────────┘
//! ```
//!
//! The split is proportional, so the same arrangement survives any terminal
//! size; only the shortcut bar has a fixed height, because its content does.
//! Modals are centred overlays drawn on top of this, never a separate screen.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
};
use ratatui::Frame;

use super::app::{
    event_detail_lines, event_line, task_events_lines, task_position_line, App, Attention,
    EntryMap, FormRow, InputForm, InputKind, Modal, NewTaskStage, Panel, RepositoryField, Screen,
    TaskContent, TaskFocus, TaskView, TransitionMode,
};
use super::chat::{ChatFocus, ChatPanel};
use super::failure_form::FailureRow;
use super::scroll::wrap_visible;
use super::transcript::{wrap_visible_lines, TranscriptLine, TranscriptPresentation};
use crate::domain::{ExecutionStatus, HumanInputType};

/// The colour of an execution waiting on a question.
const QUESTION_COLOUR: Color = Color::LightYellow;

/// The colour of an execution stopped on a problem.
const PROBLEM_COLOUR: Color = Color::LightRed;

/// The colour of what the conductor does or proposes.
const CONDUCTOR_COLOUR: Color = Color::LightMagenta;

pub(crate) fn transcript_style(presentation: TranscriptPresentation) -> Style {
    match presentation {
        TranscriptPresentation::Neutral => Style::default(),
        TranscriptPresentation::Operator => Style::default()
            .fg(Color::Rgb(213, 240, 255))
            .bg(Color::Rgb(33, 71, 93)),
        TranscriptPresentation::Agent => Style::default()
            .fg(Color::Rgb(220, 245, 215))
            .bg(Color::Rgb(48, 67, 52)),
        TranscriptPresentation::Conductor => Style::default()
            .fg(Color::Rgb(255, 233, 182))
            .bg(Color::Rgb(75, 59, 34)),
        TranscriptPresentation::System => Style::default()
            .fg(Color::Rgb(150, 170, 188))
            .bg(Color::Rgb(31, 43, 56)),
        TranscriptPresentation::Technical => Style::default()
            .fg(Color::Rgb(158, 150, 171))
            .bg(Color::Rgb(43, 39, 53)),
        TranscriptPresentation::Error => Style::default()
            .fg(Color::White)
            .bg(Color::Red)
            .add_modifier(Modifier::BOLD),
    }
}

/// Gives every entry one role-coloured, full-width header while keeping the
/// body neutral. The left marker belongs to the entry, not to wrapped rows.
pub(crate) fn transcript_line(line: TranscriptLine, width: usize) -> Line<'static> {
    let marker = if line.selected { "┃ " } else { "  " };
    let text = if line.header {
        let mut text = format!("{marker}{}", line.text);
        text.truncate(width);
        format!("{text:<width$}")
    } else {
        format!("{marker}{}", line.text)
    };
    let style = if line.header {
        transcript_style(line.presentation)
    } else {
        match line.presentation {
            TranscriptPresentation::System => Style::default().fg(Color::Rgb(135, 150, 164)),
            TranscriptPresentation::Technical => Style::default().fg(Color::Rgb(142, 134, 153)),
            TranscriptPresentation::Error => Style::default().fg(Color::LightRed),
            _ => Style::default(),
        }
    };
    Line::styled(text, style)
}

/// Height of the shortcut bar, borders included.
const SHORTCUT_BAR_HEIGHT: u16 = 3;

/// Share of the width taken by the execution list.
const LIST_WIDTH_PERCENT: u16 = 34;

/// Share of the right column's height taken by the details panel.
const DETAILS_HEIGHT_PERCENT: u16 = 45;

/// Where each panel is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelAreas {
    /// Left column: the execution list.
    pub executions: Rect,
    /// Top right: current state and context variables.
    pub details: Rect,
    /// Bottom right: the live event stream.
    pub events: Rect,
    /// The bar of context-sensitive shortcuts.
    pub shortcuts: Rect,
}

/// Splits the whole screen into the four panels.
///
/// Proportional by construction: a resize re-runs this against the new area and
/// the arrangement is unchanged, which is the only thing a resize has to
/// preserve.
pub fn layout(area: Rect) -> PanelAreas {
    let [body, shortcuts] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(SHORTCUT_BAR_HEIGHT)]).areas(area);

    let [executions, right] = Layout::horizontal([
        Constraint::Percentage(LIST_WIDTH_PERCENT),
        Constraint::Percentage(100 - LIST_WIDTH_PERCENT),
    ])
    .areas(body);

    let [details, events] = Layout::vertical([
        Constraint::Percentage(DETAILS_HEIGHT_PERCENT),
        Constraint::Percentage(100 - DETAILS_HEIGHT_PERCENT),
    ])
    .areas(right);

    PanelAreas {
        executions,
        details,
        events,
        shortcuts,
    }
}

/// A rectangle centred in `area`, sized as a share of it.
pub fn centered(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let [_, row, _] = Layout::vertical([
        Constraint::Percentage((100 - height_percent) / 2),
        Constraint::Percentage(height_percent),
        Constraint::Percentage((100 - height_percent) / 2),
    ])
    .areas(area);

    let [_, cell, _] = Layout::horizontal([
        Constraint::Percentage((100 - width_percent) / 2),
        Constraint::Percentage(width_percent),
        Constraint::Percentage((100 - width_percent) / 2),
    ])
    .areas(row);

    cell
}

/// Draws the whole interface.
pub fn render(frame: &mut Frame, app: &mut App) {
    if matches!(app.screen(), Screen::Task(_)) {
        render_task_view(frame, app);
        return;
    }
    if let Some(task) = app.queued_task() {
        render_queued_task(frame, task, app.shortcuts());
        return;
    }

    let areas = layout(frame.area());

    render_executions(frame, app, areas.executions);
    render_details(frame, app, areas.details);
    render_events(frame, app, areas.events);
    render_shortcuts(frame, app, areas.shortcuts);

    match app.modal() {
        Modal::None => {}
        Modal::Input(_) => render_input_modal(frame, app),
        Modal::Failure(_) => render_failure_modal(frame, app),
        Modal::RecoveryComment(_) => render_recovery_comment_modal(frame, app),
        Modal::EventDetail(_) => render_event_detail_modal(frame, app),
        Modal::Transition(_) => render_transition_modal(frame, app),
        Modal::Confirm(_) => render_confirm_modal(frame, app),
        Modal::InterruptConfirm(_) => render_interrupt_confirm_modal(frame, app),
        Modal::Repositories(_) => render_repository_list_modal(frame, app),
        Modal::RepositoryForm(_) => render_repository_form_modal(frame, app),
        Modal::RepositoryRemove(_) => render_repository_remove_modal(frame, app),
        Modal::NewTask(_) => render_new_task_modal(frame, app),
        Modal::HarnessSelection(_) => render_harness_selection_modal(frame, app),
        Modal::Help => render_help_modal(frame),
    }
}

/// The task-first acknowledgement surface used before an execution exists.
fn render_queued_task(
    frame: &mut Frame,
    task: &crate::controller_protocol::TaskSummary,
    shortcuts: Vec<(&'static str, &'static str)>,
) {
    let [body, footer] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(SHORTCUT_BAR_HEIGHT)])
            .areas(frame.area());
    let status = super::remote::status_badge(task);
    let mut lines = vec![
        Line::from(format!("Task: {}", task.task_id)),
        Line::from(format!("Workflow: {}", task.workflow_id)),
        Line::from(format!("Backend: {}", task.backend_id)),
        Line::from(format!("Status: {}", status.text)),
        Line::from(format!("Queue position: #{}", task.rank)),
    ];
    if let Some(hint) = super::remote::lifecycle_hint(task) {
        lines.push(Line::from(hint.text));
    }
    if let Some(runner) = &task.runner {
        lines.push(Line::from(format!(
            "Execution: {}",
            runner
                .execution_cursor
                .execution_id
                .as_deref()
                .unwrap_or("initializing")
        )));
    } else {
        lines.push(Line::from("Execution: not created yet"));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Queued task "),
            )
            .wrap(Wrap { trim: false }),
        body,
    );
    let labels = shortcuts
        .into_iter()
        .map(|(key, meaning)| format!("{key} — {meaning}"))
        .collect::<Vec<_>>()
        .join("   ");
    frame.render_widget(
        Paragraph::new(labels).block(Block::default().borders(Borders::ALL)),
        footer,
    );
}

/// The full-screen task view (U7): the step timeline on the left, the
/// selected step's content on the right.
fn render_task_view(frame: &mut Frame, app: &mut App) {
    let [body, shortcuts] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(SHORTCUT_BAR_HEIGHT)])
            .areas(frame.area());
    let [axis_area, content_area] =
        Layout::horizontal([Constraint::Percentage(32), Constraint::Percentage(68)]).areas(body);

    let chat_open = matches!(app.screen(), Screen::Task(view) if view.chat.open);
    if chat_open {
        // The chat shares the right column with the step's content, so the
        // conversation it talks about stays in sight.
        let [content_area, chat_area] =
            Layout::vertical([Constraint::Percentage(40), Constraint::Percentage(60)])
                .areas(content_area);
        app.set_task_transcript_viewports(
            content_area.height.saturating_sub(2).max(1) as usize,
            content_area.width.saturating_sub(2).max(1) as usize,
            Some(chat_area.height.saturating_sub(2).max(1) as usize),
            Some(chat_area.width.saturating_sub(2).max(1) as usize),
        );
        let Screen::Task(view) = app.screen() else {
            return;
        };
        render_task_axis(frame, app, view, axis_area);
        render_task_content(frame, view, content_area);
        render_chat_panel(frame, &view.chat, chat_area);
    } else {
        app.set_task_transcript_viewports(
            content_area.height.saturating_sub(2).max(1) as usize,
            content_area.width.saturating_sub(2).max(1) as usize,
            None,
            None,
        );
        let Screen::Task(view) = app.screen() else {
            return;
        };
        render_task_axis(frame, app, view, axis_area);
        render_task_content(frame, view, content_area);
    }
    render_shortcuts(frame, app, shortcuts);

    // `i` works from the task view too (R1), and its forms are the only
    // overlays drawn over it.
    match app.modal() {
        Modal::Input(_) => render_input_modal(frame, app),
        Modal::Failure(_) => render_failure_modal(frame, app),
        _ => {}
    }
}

/// The conductor's chat in the task view (F4): the history, each proposal
/// with its state and each decision with its confidence and rationale, and
/// the message being written — or the confirmation an undo waits for.
fn render_chat_panel(frame: &mut Frame, chat: &ChatPanel, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Thick)
        .border_style(Style::default().fg(CONDUCTOR_COLOUR))
        .title(format!(" Conductor chat — {} entries ", chat.entries.len()));
    let inner_width = area.width.saturating_sub(2).max(1) as usize;
    let inner_height = area.height.saturating_sub(2).max(1) as usize;

    let mut footer: Vec<String> = vec![String::new()];
    if let Some(undo) = &chat.undo {
        footer.push(format!("Undo the conductor's decision: {}", undo.text));
        if let Some(label) = &undo.label {
            footer.push(format!("  it chose the output '{label}'"));
        }
        footer.push(format!(
            "  confidence: {}",
            undo.confidence
                .map_or("unknown", |confidence| confidence.name())
        ));
        if let Some(rationale) = &undo.rationale {
            footer.push(format!("  why: {rationale}"));
        }
        footer.push(
            "  The step becomes a problem again; a step after it that already started keeps \
             its side effects."
                .to_string(),
        );
        footer.push("  y — undo,  n/Esc — keep it".to_string());
    } else {
        let marker = if chat.focus == ChatFocus::Draft {
            "›"
        } else {
            " "
        };
        footer.push(format!("{marker} message: {}", chat.draft));
    }

    let mut lines: Vec<TranscriptLine> = Vec::new();
    if chat.transcript.entries().is_empty() {
        lines.push(TranscriptLine {
            entry_id: super::transcript::TranscriptEntryId::from("chat-status"),
            logical_line: 0,
            text: if chat.loaded {
                "No messages yet: write to the conductor below.".to_string()
            } else {
                "Loading the chat…".to_string()
            },
            presentation: TranscriptPresentation::Neutral,
            header: false,
            selected: false,
        });
    } else {
        let action_entry = chat
            .action_item()
            .map(|item| super::transcript::TranscriptEntryId::from(item.entry.id));
        let mut last_entry = None;
        for line in chat.transcript.logical_lines() {
            let starts_entry = last_entry.as_ref() != Some(&line.entry_id);
            let marked = chat.focus == ChatFocus::Action
                && starts_entry
                && action_entry.as_ref() == Some(&line.entry_id);
            last_entry = Some(line.entry_id.clone());
            lines.push(TranscriptLine {
                text: format!("{}{}", if marked { "› " } else { "  " }, line.text),
                ..line
            });
        }
    }

    let history_height = inner_height.saturating_sub(footer.len()).max(1);
    let offset = chat.transcript.scroll_offset();
    let mut visible: Vec<Line> = wrap_visible_lines(&lines, inner_width, offset, history_height)
        .into_iter()
        .map(|line| transcript_line(line, inner_width))
        .collect();
    visible.extend(footer.into_iter().map(Line::from));
    frame.render_widget(Paragraph::new(visible).block(block), area);
}

/// The task view's left pane: every position, in timeline order (R7).
fn render_task_axis(frame: &mut Frame, app: &App, view: &TaskView, area: Rect) {
    let focused = view.focus == TaskFocus::Axis;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(if focused {
            BorderType::Thick
        } else {
            BorderType::Plain
        })
        .border_style(Style::default().fg(if focused {
            Color::Cyan
        } else {
            Color::DarkGray
        }))
        .title(
            match app
                .row(&view.execution_id)
                .and_then(|row| row.conductor_line())
            {
                // R11: the task's header says what the conductor is doing.
                Some(working) => format!(" Steps — {} · {working} ", view.execution_id),
                None => format!(" Steps — {} ", view.execution_id),
            },
        );

    if view.timeline.positions.is_empty() {
        frame.render_widget(
            Paragraph::new("No positions on this timeline yet.").block(block),
            area,
        );
        return;
    }

    let cursor = view.axis_scroll.offset();
    let inner_height = area.height.saturating_sub(2).max(1) as usize;
    let lines: Vec<Line> = view
        .timeline
        .positions
        .iter()
        .enumerate()
        .skip(cursor.saturating_sub(inner_height.saturating_sub(1)))
        .map(|(index, position)| {
            let text = task_position_line(position);
            let style = if index == cursor && focused {
                Style::default().add_modifier(Modifier::REVERSED)
            } else if index == cursor {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(Span::styled(text, style))
        })
        .collect();

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// The task view's right pane: the selected position's conversation, or a
/// fallback event list (R11).
fn render_task_content(frame: &mut Frame, view: &TaskView, area: Rect) {
    let focused = view.focus == TaskFocus::Content;
    let title = match &view.content {
        TaskContent::Conversation {
            closed,
            truncated_tail,
            complete,
            ..
        } => {
            if *truncated_tail {
                " Conversation — incomplete trailing record ".to_string()
            } else if !*complete {
                " Conversation — loading history… ".to_string()
            } else if *closed {
                " Conversation ".to_string()
            } else {
                " Conversation — attempt in progress ".to_string()
            }
        }
        TaskContent::Events { .. } => " Events ".to_string(),
        TaskContent::Empty => " Loading… ".to_string(),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(if focused {
            BorderType::Thick
        } else {
            BorderType::Plain
        })
        .border_style(Style::default().fg(if focused {
            Color::Cyan
        } else {
            Color::DarkGray
        }))
        .title(title);

    let lines: Vec<TranscriptLine> = match &view.content {
        TaskContent::Conversation { transcript, .. } => transcript.logical_lines(),
        TaskContent::Events { message, events } => {
            let start = view.content_scroll.offset().min(events.len());
            task_events_lines(message, &events[start..], 0)
                .into_iter()
                .enumerate()
                .map(|(logical_line, text)| TranscriptLine {
                    entry_id: super::transcript::TranscriptEntryId::from("events"),
                    logical_line,
                    text,
                    presentation: TranscriptPresentation::Neutral,
                    header: false,
                    selected: false,
                })
                .collect()
        }
        TaskContent::Empty => vec![TranscriptLine {
            entry_id: super::transcript::TranscriptEntryId::from("loading"),
            logical_line: 0,
            text: "Loading…".to_string(),
            presentation: TranscriptPresentation::Neutral,
            header: false,
            selected: false,
        }],
    };

    let inner_width = area.width.saturating_sub(2).max(1) as usize;
    let inner_height = area.height.saturating_sub(2).max(1) as usize;
    let offset = match &view.content {
        TaskContent::Conversation { transcript, .. } => transcript.scroll_offset(),
        _ => view.content_scroll.offset(),
    };
    let visible: Vec<Line> = wrap_visible_lines(&lines, inner_width, offset, inner_height)
        .into_iter()
        .map(|line| transcript_line(line, inner_width))
        .collect();

    frame.render_widget(Paragraph::new(visible).block(block), area);
}

/// The bordered block of a panel, highlighted when it has focus.
fn panel_block(app: &App, panel: Panel, suffix: Option<String>) -> Block<'static> {
    let focused = app.focus() == panel && !app.modal().is_open();
    let title = match suffix {
        Some(suffix) => format!(" {} — {suffix} ", panel.title()),
        None => format!(" {} ", panel.title()),
    };

    Block::default()
        .borders(Borders::ALL)
        .border_type(if focused {
            BorderType::Thick
        } else {
            BorderType::Plain
        })
        .border_style(Style::default().fg(if focused {
            Color::Cyan
        } else {
            Color::DarkGray
        }))
        .title(Span::styled(
            title,
            Style::default().add_modifier(if focused {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
        ))
}

/// The colour a status is drawn in; a question and a problem each have
/// their own, whatever the status underneath (R10).
fn status_style(status: &ExecutionStatus, attention: Option<Attention>) -> Style {
    match attention {
        Some(Attention::Question) => return Style::default().fg(QUESTION_COLOUR),
        Some(Attention::Problem) => {
            return Style::default()
                .fg(PROBLEM_COLOUR)
                .add_modifier(Modifier::BOLD)
        }
        None => {}
    }
    let colour = match status {
        ExecutionStatus::Running => Color::Green,
        ExecutionStatus::Paused => Color::Yellow,
        ExecutionStatus::InputNeeded => Color::LightYellow,
        ExecutionStatus::WaitingForEvent => Color::Blue,
        ExecutionStatus::Completed => Color::Cyan,
        ExecutionStatus::Failed => Color::Red,
        ExecutionStatus::Cancelled => Color::Magenta,
        ExecutionStatus::Unknown => Color::DarkGray,
    };
    Style::default().fg(colour)
}

/// Left panel: every execution with its status and current state.
fn render_executions(frame: &mut Frame, app: &App, area: Rect) {
    // R9: how many tasks wait on the operator, in the panel's header.
    let suffix = match app.attention_counts().summary() {
        Some(waiting) => format!("{} · {waiting}", app.executions().len()),
        None => format!("{}", app.executions().len()),
    };
    let block = panel_block(app, Panel::Executions, Some(suffix));

    if app.executions().is_empty() {
        let hint = Paragraph::new(
            "No executions in the store.\n\nPress `n` to start a workflow,\nor run `narsor run <file>`.",
        )
        .wrap(Wrap { trim: true })
        .block(block);
        frame.render_widget(hint, area);
        return;
    }

    let items: Vec<ListItem> = app
        .executions()
        .iter()
        .map(|row| {
            let short: String = row.id.chars().take(8).collect();
            let attention = row.attention();
            let style = status_style(&row.status, attention);
            let mut status = vec![
                Span::styled(short, Style::default().fg(Color::White)),
                Span::raw("  "),
                Span::styled(row.status.as_str(), style),
            ];
            if let Some(attention) = attention {
                status.push(Span::styled(format!(" · {}", attention.label()), style));
            }
            let mut lines = vec![
                Line::from(status),
                Line::from(Span::styled(
                    format!(
                        "  {} · {}",
                        row.workflow_id,
                        row.current_state.as_deref().unwrap_or("-")
                    ),
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            // R11, AE12: what the conductor is doing for this task.
            if let Some(working) = row.conductor_line() {
                lines.push(Line::from(Span::styled(
                    format!("  ◆ {working}"),
                    Style::default().fg(CONDUCTOR_COLOUR),
                )));
            }
            ListItem::new(lines)
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▌");

    let mut state = ListState::default();
    state.select(Some(app.selected_index()));
    frame.render_stateful_widget(list, area, &mut state);
}

/// Top right panel: the current state, the pending question and the context.
fn render_details(frame: &mut Frame, app: &App, area: Rect) {
    let block = panel_block(app, Panel::Details, None);

    let Some(execution) = app.execution() else {
        frame.render_widget(
            Paragraph::new("No execution is selected.").block(block),
            area,
        );
        return;
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled("Execution  ", Style::default().fg(Color::DarkGray)),
            Span::raw(execution.id.clone()),
        ]),
        Line::from(vec![
            Span::styled("Workflow   ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!(
                "{} (version {})",
                execution.workflow_id, execution.workflow_version
            )),
        ]),
        Line::from(vec![
            Span::styled("Status     ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                match app.selected_attention() {
                    Some(attention) => {
                        format!("{} ({})", execution.status.as_str(), attention.label())
                    }
                    None => execution.status.as_str().to_string(),
                },
                status_style(&execution.status, app.selected_attention()),
            ),
        ]),
        Line::from(vec![
            Span::styled("State      ", Style::default().fg(Color::DarkGray)),
            Span::raw(
                execution
                    .current_state
                    .clone()
                    .unwrap_or_else(|| "-".into()),
            ),
        ]),
    ];

    if let Some(working) = app.selected().and_then(|row| row.conductor_line()) {
        lines.push(Line::from(Span::styled(
            format!("◆ {working}"),
            Style::default().fg(CONDUCTOR_COLOUR),
        )));
    }

    lines.extend(waiting_lines(app));

    if let Some(error) = app.definition_error() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            error.to_string(),
            Style::default().fg(Color::Red),
        )));
    } else {
        let transitions = app.permitted_transitions();
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("Outputs    ", Style::default().fg(Color::DarkGray)),
            Span::raw(if transitions.is_empty() {
                "-".to_string()
            } else {
                transitions.join(", ")
            }),
        ]));
        lines.push(Line::from(vec![
            Span::styled("Override   ", Style::default().fg(Color::DarkGray)),
            Span::raw(if app.override_allowed() {
                "allowed"
            } else {
                "not allowed"
            }),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Context variables",
        Style::default().add_modifier(Modifier::BOLD),
    )));

    if app.variables().is_empty() {
        lines.push(Line::from(Span::styled(
            "  (none)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (index, (name, value)) in app.variables().iter().enumerate() {
            let style = if index == app.variable_cursor() && app.focus() == Panel::Details {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            lines.push(Line::from(Span::styled(
                format!("  {name} = {value}"),
                style,
            )));
        }
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// What the selected execution waits on, described by its kind (R10): a
/// workflow's question, an agent's question, or a problem.
fn waiting_lines(app: &App) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let question = Style::default().fg(QUESTION_COLOUR);
    let problem = Style::default()
        .fg(PROBLEM_COLOUR)
        .add_modifier(Modifier::BOLD);
    let (text, style, hint) = match (app.pending(), app.selected_problem()) {
        (Some(pending), _) => match InputKind::of(pending.continuation) {
            InputKind::Gate => (
                format!("Question — the workflow asks: {}", pending.instruction()),
                question,
                Some("Press i to answer it."),
            ),
            InputKind::AgentQuestion => (
                format!("Question — the agent asks: {}", pending.instruction()),
                question,
                Some("Press i to reply to the agent."),
            ),
            InputKind::Problem => (
                format!(
                    "Problem — {}",
                    pending
                        .problem
                        .as_ref()
                        .map(|problem| problem.reason.as_str())
                        .unwrap_or_else(|| pending.instruction())
                ),
                problem,
                Some("Press i to settle it."),
            ),
            InputKind::Approval => (
                "Approval — review the agent output.".to_string(),
                question,
                Some("Press i to review it."),
            ),
        },
        (None, Some(active)) if app.selected_attention() == Some(Attention::Problem) => (
            format!("Problem — {}", active.reason),
            problem,
            Some("Press i to settle it."),
        ),
        _ => return Vec::new(),
    };
    let mut lines = vec![Line::from(""), Line::from(Span::styled(text, style))];
    if let Some(hint) = hint {
        lines.push(Line::from(Span::styled(hint, dim)));
    }
    lines
}

/// Bottom right panel: the live event stream, with the channel's health.
fn render_events(frame: &mut Frame, app: &App, area: Rect) {
    let status = app.stream_status();
    let suffix = if app.follows_stream() {
        format!("{} · following", status.indicator())
    } else {
        status.indicator()
    };
    let mut block = panel_block(app, Panel::Events, Some(suffix));
    if !status.is_live() {
        block = block.border_style(Style::default().fg(Color::Red));
    }

    if app.events().is_empty() {
        frame.render_widget(
            Paragraph::new("No events for this execution.").block(block),
            area,
        );
        return;
    }

    let items: Vec<ListItem> = app
        .events()
        .iter()
        .map(|event| ListItem::new(Line::from(event_line(event))))
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    let mut state = ListState::default();
    state.select(Some(app.event_cursor()));
    frame.render_stateful_widget(list, area, &mut state);
}

/// The bottom bar: the shortcuts of the focused panel, plus the last message.
fn render_shortcuts(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans = Vec::new();
    for (key, description) in app.shortcuts() {
        spans.push(Span::styled(
            format!(" {key} "),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(format!(" {description}   ")));
    }

    let title = match app.notice() {
        Some(notice) => Span::styled(
            format!(" {} ", notice.text),
            Style::default().fg(if notice.is_error {
                Color::Red
            } else {
                Color::Green
            }),
        ),
        None => Span::styled(" Narsor ", Style::default().fg(Color::DarkGray)),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(title);

    frame.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

/// The centred overlay a modal is drawn into.
fn modal_area(frame: &Frame, width: u16, height: u16) -> Rect {
    centered(frame.area(), width, height)
}

/// The generated human-input form.
fn render_input_modal(frame: &mut Frame, app: &App) {
    let Modal::Input(form) = app.modal() else {
        return;
    };
    let area = modal_area(frame, 80, 80);
    frame.render_widget(Clear, area);

    let mut lines = Vec::new();
    if let Some(closure) = &form.closed {
        // R5: the request is gone; what was typed stays below, unsendable.
        lines.push(Line::from(Span::styled(
            format!("✖ {}", closure.message()),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            "This form can no longer be sent. What you typed is kept; Esc closes it.",
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::from(""));
    }
    lines.extend(input_context_lines(form));
    lines.push(Line::from(""));

    let dim = Style::default().fg(Color::DarkGray);
    let rows = form.rows();
    let chosen = form.chosen_label();
    for (index, row) in rows.iter().enumerate() {
        let focused = index == form.row;
        let marker = if focused { "›" } else { " " };
        let text = match row {
            FormRow::Field(field_index) => {
                let field = &form.fields[*field_index];
                let requirement = if form.field_is_required(field) {
                    "*"
                } else {
                    " "
                };
                let value = form.values[*field_index].clone();
                let hint = match field.field_type {
                    _ if form.label_field == Some(*field_index) => {
                        "  (chooses the output)".to_string()
                    }
                    HumanInputType::Select => format!("  [{}]", field.options.join(" | ")),
                    HumanInputType::Text => field
                        .description
                        .as_ref()
                        .map(|description| format!("  — {description}"))
                        .unwrap_or_default(),
                };
                format!("{marker} {}{requirement}: {value}{hint}", field.name)
            }
            FormRow::Label => {
                let value = form
                    .label_choices
                    .get(form.label_index)
                    .cloned()
                    .flatten()
                    .unwrap_or_else(|| "(infer from the select field)".to_string());
                let proposed = if form.is_proposed_label(&value) {
                    "  ◆ proposed by the conductor"
                } else {
                    ""
                };
                format!("{marker} output label: {value}{proposed}")
            }
            FormRow::Entry(map, entry_index) => {
                let entry = &form
                    .problem
                    .as_ref()
                    .expect("entry rows exist only on a problem's form")
                    .entries(*map)[*entry_index];
                let requirement = if form.is_required(*map, &entry.key) {
                    "*"
                } else {
                    " "
                };
                // The column typing goes to is bracketed on the focused row.
                let (key, value) = match (focused, entry.editing_key) {
                    (true, true) => (format!("[{}]", entry.key), entry.value.clone()),
                    (true, false) => (entry.key.clone(), format!("[{}]", entry.value)),
                    (false, _) => (entry.key.clone(), entry.value.clone()),
                };
                // Values the conductor proposed are marked until edited.
                let proposed = if entry.is_proposed() {
                    "  ◆ conductor"
                } else {
                    ""
                };
                format!(
                    "{marker} {}.{key}{requirement} = {value}{proposed}",
                    map.name()
                )
            }
            FormRow::AddEntry(map) => {
                let noun = match map {
                    EntryMap::Data => "a data key",
                    EntryMap::Variables => "a variable",
                };
                format!("{marker} + add {noun} (type its name)")
            }
            FormRow::Comment => {
                let comment = form
                    .problem
                    .as_ref()
                    .map(|problem| problem.comment.clone())
                    .unwrap_or_default();
                format!("{marker} comment: {comment}")
            }
        };

        lines.push(Line::from(Span::styled(
            text,
            if focused {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            },
        )));

        // A choice of outputs lists every label with what it means (KTD17).
        let offers_outputs = match row {
            FormRow::Field(field_index) => form.label_field == Some(*field_index),
            FormRow::Label => true,
            _ => false,
        };
        if offers_outputs {
            for output in &form.outputs {
                let bullet = if chosen.as_deref() == Some(output.label.as_str()) {
                    "●"
                } else {
                    "○"
                };
                let proposed = if form.is_proposed_label(&output.label) {
                    "  ◆ conductor's proposal"
                } else {
                    ""
                };
                let text = match form.description_of(&output.label) {
                    Some(description) => {
                        format!("    {bullet} {} — {description}{proposed}", output.label)
                    }
                    None => format!("    {bullet} {}{proposed}", output.label),
                };
                lines.push(Line::from(Span::styled(text, dim)));
            }
        }

        if form.error_row == Some(*row) {
            if let Some(error) = &form.error {
                lines.push(Line::from(Span::styled(
                    format!("    ↳ {error}"),
                    Style::default().fg(Color::Red),
                )));
            }
        }
    }

    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "This action declares no fields: pick an output label with t.",
            dim,
        )));
    }

    if let (Some(error), None) = (&form.error, form.error_row) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(Color::Red),
        )));
    }

    let border = match (form.closed.is_some(), form.kind) {
        (true, _) => Color::DarkGray,
        (false, InputKind::Problem) => PROBLEM_COLOUR,
        (false, InputKind::Gate | InputKind::AgentQuestion) => QUESTION_COLOUR,
        (false, InputKind::Approval) => QUESTION_COLOUR,
    };
    let title = if form.closed.is_some() {
        format!("{}(closed) ", form.kind.title())
    } else {
        form.kind.title().to_string()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(border))
        .title(title);

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// How many trailing lines of an agent's answer the form shows.
const ANSWER_TAIL_LINES: usize = 8;

/// What the form says above its rows (R7, R12): the gate's message, the
/// agent's question and the end of its answer, and for a problem why it is
/// one, each rejected answer and the conductor's reasoning.
fn input_context_lines(form: &InputForm) -> Vec<Line<'static>> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);
    let mut lines = vec![Line::from(Span::styled(form.instruction.clone(), bold))];

    if let Some(label) = form.chosen_label() {
        if form
            .outputs
            .iter()
            .any(|output| output.label == label && output.resume_comment_field.is_some())
        {
            lines.push(Line::from(Span::styled(
                "This choice resumes the target agent; the marked comment is required.",
                Style::default().fg(Color::Cyan),
            )));
        }
    }

    if let Some(mode) = form.problem_mode {
        lines.push(Line::from(""));
        let action = match (form.kind, mode) {
            (InputKind::Approval, crate::tui::input_form::ProblemMode::Conversation) => {
                "Return-to-agent mode — Enter sends the required comment (Tab: accept)."
            }
            (InputKind::Approval, crate::tui::input_form::ProblemMode::ManualOutput) => {
                "Accept mode — Enter accepts this output (Tab: return to agent)."
            }
            (_, crate::tui::input_form::ProblemMode::Conversation) => {
                "Conversation mode — Enter sends this message to the agent (Tab: manual output)."
            }
            (_, crate::tui::input_form::ProblemMode::ManualOutput) => {
                "Manual-output mode — Enter chooses the selected output (Tab: conversation)."
            }
        };
        lines.push(Line::from(Span::styled(
            action,
            Style::default().fg(Color::Cyan),
        )));
        if mode == crate::tui::input_form::ProblemMode::Conversation {
            lines.extend(
                form.agent_message
                    .lines()
                    .map(|line| Line::from(format!("  {line}"))),
            );
        }
    }

    if let Some(question) = &form.question {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "The agent asks:",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        lines.extend(question.lines().map(|line| Line::from(line.to_string())));
    }

    match &form.problem {
        Some(problem) => {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("Problem: {}", problem.reason),
                Style::default().fg(Color::Red),
            )));
            for answer in &problem.answers {
                lines.push(Line::from(Span::styled(
                    format!("Rejected answer: {}", answer.reason),
                    bold,
                )));
                if let Some(tail) = &answer.answer_tail {
                    lines.extend(tail_lines(tail, dim));
                }
            }
            if problem.rationale.is_some() || problem.proposed_label.is_some() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "Conductor's analysis (◆ marks what it proposed):",
                    bold.fg(CONDUCTOR_COLOUR),
                )));
                if let Some(label) = &problem.proposed_label {
                    lines.push(Line::from(Span::styled(
                        format!("  ◆ proposed output: {label}"),
                        Style::default().fg(CONDUCTOR_COLOUR),
                    )));
                }
                if let Some(rationale) = &problem.rationale {
                    lines.extend(
                        rationale
                            .lines()
                            .map(|line| Line::from(format!("  {line}"))),
                    );
                }
            }
        }
        None => {
            if let Some(tail) = &form.answer_tail {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("End of the agent's answer:", bold)));
                lines.extend(tail_lines(tail, dim));
            }
        }
    }

    // R30, AE16: the conductor's summary of the gate, advisory only.
    if let Some(annotation) = &form.annotation {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Conductor's summary (advisory — the answer is yours):",
            bold.fg(CONDUCTOR_COLOUR),
        )));
        lines.extend(
            annotation
                .summary
                .lines()
                .map(|line| Line::from(format!("  {line}"))),
        );
        if let Some(recommendation) = &annotation.recommendation {
            lines.push(Line::from(Span::styled(
                format!("  ◆ recommendation: {recommendation}"),
                Style::default().fg(CONDUCTOR_COLOUR),
            )));
        }
    }
    lines
}

/// The failure variant of `i` (KTD4): the failed step, the conductor's
/// diagnosis and proposals — or the raw error when it could not make one
/// (R24) — and the restart with a note.
fn render_failure_modal(frame: &mut Frame, app: &App) {
    let Modal::Failure(form) = app.modal() else {
        return;
    };
    let area = modal_area(frame, 80, 80);
    frame.render_widget(Clear, area);

    let bold = Style::default().add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);
    let conductor = Style::default().fg(CONDUCTOR_COLOUR);
    let mut lines = Vec::new();
    if let Some(closed) = &form.closed {
        lines.push(Line::from(Span::styled(
            format!("✖ {closed}"),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        format!("The step '{}' failed.", form.state_id),
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    )));

    match (&form.diagnosis, &form.raw_error) {
        (Some(diagnosis), _) => {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Conductor's diagnosis:",
                bold.fg(CONDUCTOR_COLOUR),
            )));
            lines.extend(
                diagnosis
                    .lines()
                    .map(|line| Line::from(format!("  {line}"))),
            );
            if let Some(confidence) = form.confidence {
                lines.push(Line::from(Span::styled(
                    format!("  confidence: {}", confidence.name()),
                    conductor,
                )));
            }
            if let Some(rationale) = &form.rationale {
                lines.push(Line::from(Span::styled(
                    format!("  why: {rationale}"),
                    conductor,
                )));
            }
        }
        (None, Some(error)) => {
            // R24: the conductor could not answer twice; the operator gets
            // the failure as it was, and no analysis.
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("Error:", bold)));
            lines.extend(error.lines().map(|line| Line::from(format!("  {line}"))));
        }
        (None, None) => {}
    }

    lines.push(Line::from(""));
    if !form.proposals.is_empty() {
        lines.push(Line::from(Span::styled(
            "The conductor proposes (nothing happens until you confirm):",
            bold,
        )));
    }
    for (index, row) in form.rows().iter().enumerate() {
        let focused = index == form.row;
        let marker = if focused { "›" } else { " " };
        let text = match row {
            FailureRow::Proposal(proposal) => {
                let item = &form.proposals[*proposal];
                format!("{marker} ◆ {}", item.entry.text)
            }
            FailureRow::Note => {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "Restart the step with a note for its next attempt:",
                    bold,
                )));
                format!("{marker} note: {}", form.note.value)
            }
        };
        lines.push(Line::from(Span::styled(
            text,
            if focused {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            },
        )));
    }
    lines.push(Line::from(Span::styled(
        "A restart names this failure: if the execution moved on meanwhile, it is refused.",
        dim,
    )));

    if let Some(error) = &form.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(Color::Red),
        )));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(if form.closed.is_some() {
            Color::DarkGray
        } else {
            PROBLEM_COLOUR
        }))
        .title(if form.closed.is_some() {
            " Resolve the failure (closed) "
        } else {
            " Resolve the failure "
        });
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// The last [`ANSWER_TAIL_LINES`] lines of an answer, indented.
fn tail_lines(tail: &str, style: Style) -> Vec<Line<'static>> {
    let all: Vec<&str> = tail.lines().collect();
    let start = all.len().saturating_sub(ANSWER_TAIL_LINES);
    all[start..]
        .iter()
        .map(|line| Line::from(Span::styled(format!("  {line}"), style)))
        .collect()
}

/// The short free-form instruction saved for the next agent attempt.
fn render_recovery_comment_modal(frame: &mut Frame, app: &App) {
    let Modal::RecoveryComment(form) = app.modal() else {
        return;
    };
    let area = modal_area(frame, 70, 30);
    frame.render_widget(Clear, area);
    let lines = vec![
        Line::from(Span::styled(
            "Operator instruction for the next agent attempt",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from("Saving does not restart the execution; choose a step and run it when ready."),
        Line::from(""),
        Line::from(format!("› {}", form.value)),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" Save operator instruction ");
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// The scrollable window with every field of one event (R1, R2, R3).
fn render_event_detail_modal(frame: &mut Frame, app: &App) {
    let Modal::EventDetail(detail) = app.modal() else {
        return;
    };
    let area = modal_area(frame, 86, 80);
    frame.render_widget(Clear, area);

    let lines = event_detail_lines(&detail.event);
    let inner_width = area.width.saturating_sub(2).max(1) as usize;
    let inner_height = area.height.saturating_sub(2).max(1) as usize;
    let offset = detail.scroll.offset().min(lines.len());

    // Wraps only the rows this frame is about to draw (`scroll::wrap_visible`)
    // rather than the whole buffer — the point that matters once this
    // window's sibling, the conversation view (U7), can hold megabytes of raw
    // harness output.
    let visible: Vec<Line> = wrap_visible(&lines, inner_width, offset, inner_height)
        .into_iter()
        .map(Line::from)
        .collect();

    let position = if lines.is_empty() {
        "empty".to_string()
    } else {
        format!("{}/{}", offset + 1, lines.len())
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(Color::Cyan))
        .title(format!(" Event #{} — {position} ", detail.event.sequence));

    frame.render_widget(Paragraph::new(visible).block(block), area);
}

/// The manual transition menu.
fn render_transition_modal(frame: &mut Frame, app: &App) {
    let Modal::Transition(menu) = app.modal() else {
        return;
    };
    let area = modal_area(frame, 60, 55);
    frame.render_widget(Clear, area);

    let title = match menu.mode {
        TransitionMode::Outputs => " Manual transition — declared outputs ",
        TransitionMode::Override => " Manual transition — OVERRIDE (any state) ",
    };

    let mut lines = Vec::new();
    if menu.entries().is_empty() {
        lines.push(Line::from(Span::styled(
            "Nothing to pick.",
            Style::default().fg(Color::DarkGray),
        )));
    }
    for (index, entry) in menu.entries().iter().enumerate() {
        let focused = index == menu.selected;
        let mut spans = vec![Span::styled(
            format!("{} {entry}", if focused { "›" } else { " " }),
            if focused {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            },
        )];
        if let Some(description) = menu.description(index) {
            spans.push(Span::styled(
                format!(" — {description}"),
                Style::default().fg(Color::DarkGray),
            ));
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        if menu.override_allowed {
            "o — switch to override mode (needs confirmation)"
        } else {
            "This state allows no override (allow_manual_override)."
        },
        Style::default().fg(Color::DarkGray),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(match menu.mode {
            TransitionMode::Outputs => Color::Cyan,
            TransitionMode::Override => Color::Red,
        }))
        .title(title);

    // Wrapped, so a long label description stays readable in the modal.
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// The confirmation a forced transition has to pass.
fn render_confirm_modal(frame: &mut Frame, app: &App) {
    let Modal::Confirm(confirm) = app.modal() else {
        return;
    };
    let area = modal_area(frame, 60, 30);
    frame.render_widget(Clear, area);

    let lines = vec![
        Line::from(Span::styled(
            "Forced transition outside the declared graph.",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!("  {} → {}", confirm.from, confirm.target)),
        Line::from(""),
        Line::from("It is recorded in the log as a forced transition."),
        Line::from(""),
        Line::from(Span::styled(
            "y — confirm,  n/Esc — cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(Color::Red))
        .title(" Confirm override ");

    frame.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        area,
    );
}

fn render_interrupt_confirm_modal(frame: &mut Frame, app: &App) {
    if !matches!(app.modal(), Modal::InterruptConfirm(_)) {
        return;
    }
    let area = modal_area(frame, 60, 30);
    frame.render_widget(Clear, area);
    let lines = vec![
        Line::from(Span::styled(
            "Interrupt the active harness now?",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        Line::from(
            "Its current work is discarded; you may then send an instruction or choose a step.",
        ),
        Line::from(""),
        Line::from("y — interrupt,  n/Esc — cancel"),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Double)
                .title(" Interrupt execution "),
        ),
        area,
    );
}

/// The listing of registered repositories.
fn render_repository_list_modal(frame: &mut Frame, app: &App) {
    let Modal::Repositories(list) = app.modal() else {
        return;
    };
    let area = modal_area(frame, 76, 60);
    frame.render_widget(Clear, area);

    let mut lines = Vec::new();
    if list.entries.is_empty() {
        lines.push(Line::from(Span::styled(
            "Nothing is registered yet — press a to register a repository.",
            Style::default().fg(Color::DarkGray),
        )));
    }
    for (index, entry) in list.entries.iter().enumerate() {
        let focused = index == list.selected;
        lines.push(Line::from(Span::styled(
            format!(
                "{} {:<20} {:<10} {:<14} {}",
                if focused { "›" } else { " " },
                entry.id,
                entry.provider,
                entry.default_branch.as_deref().unwrap_or("-"),
                entry.remote
            ),
            if focused {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            },
        )));
    }

    if let Some(entry) = list.current() {
        if let Some(description) = &entry.description {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                description.clone(),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "a — register,  d — remove (needs confirmation),  Esc — close",
        Style::default().fg(Color::DarkGray),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(Color::Cyan))
        .title(format!(" Repositories — {} ", list.entries.len()));

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// The form that registers a repository.
fn render_repository_form_modal(frame: &mut Frame, app: &App) {
    let Modal::RepositoryForm(form) = app.modal() else {
        return;
    };
    let area = modal_area(frame, 70, 60);
    frame.render_widget(Clear, area);

    let mut lines = vec![
        Line::from(Span::styled(
            "Register a repository",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];

    for (index, field) in RepositoryField::ALL.iter().enumerate() {
        let focused = index == form.row;
        let requirement = if field.required() { "*" } else { " " };
        lines.push(Line::from(Span::styled(
            format!(
                "{} {}{requirement}: {}",
                if focused { "›" } else { " " },
                field.label(),
                form.values.get(index).cloned().unwrap_or_default()
            ),
            if focused {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            },
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "The id names the checkout directory; the remote is checked with the \
         provisioner's own rules.",
        Style::default().fg(Color::DarkGray),
    )));

    if let Some(error) = &form.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(Color::Red),
        )));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" Register a repository ");

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// The daemon-backed default harness picker.
fn render_harness_selection_modal(frame: &mut Frame, app: &App) {
    let Modal::HarnessSelection(picker) = app.modal() else {
        return;
    };
    let area = modal_area(frame, 64, 54);
    frame.render_widget(Clear, area);

    let mut lines = Vec::new();
    match &picker.snapshot {
        None if picker.error.is_none() => {
            lines.push(Line::from(Span::styled(
                "Loading the daemon's harness selection…",
                Style::default().fg(Color::DarkGray),
            )));
        }
        None => {}
        Some(snapshot) => {
            lines.push(Line::from(format!("Configured: {}", snapshot.configured)));
            lines.push(Line::from(format!(
                "Persistent override: {}",
                snapshot.override_harness.as_deref().unwrap_or("none")
            )));
            lines.push(Line::from(Span::styled(
                format!("Effective default: {}", snapshot.effective),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
            for (index, harness) in snapshot.supported.iter().enumerate() {
                let active = harness == &snapshot.effective;
                let selected = index == picker.selected;
                lines.push(Line::from(Span::styled(
                    format!(
                        "{} {}{}",
                        if selected { "›" } else { " " },
                        harness,
                        if active { " (effective)" } else { "" }
                    ),
                    if selected {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::default()
                    },
                )));
            }
            let reset_selected = picker.selected == snapshot.supported.len();
            lines.push(Line::from(Span::styled(
                format!(
                    "{} Use configured value{}",
                    if reset_selected { "›" } else { " " },
                    if snapshot.override_harness.is_none() {
                        " (already active)"
                    } else {
                        ""
                    }
                ),
                if reset_selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                },
            )));
        }
    }

    if let Some(error) = &picker.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            error,
            Style::default().fg(Color::Red),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter applies the highlighted choice; Esc closes without changing it.",
        Style::default().fg(Color::DarkGray),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Default agent harness ");
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_repository_remove_modal(frame: &mut Frame, app: &App) {
    let Modal::RepositoryRemove(confirm) = app.modal() else {
        return;
    };
    let area = modal_area(frame, 60, 30);
    frame.render_widget(Clear, area);

    let lines = vec![
        Line::from(Span::styled(
            "Remove a registered repository.",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!("  {}", confirm.id)),
        Line::from(""),
        Line::from("A workflow that names this id will fall back to treating it as a raw remote."),
        Line::from(""),
        Line::from(Span::styled(
            "y — remove,  n/Esc — cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(Color::Red))
        .title(" Confirm removal ");

    frame.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        area,
    );
}

/// The form that starts a workflow.
fn render_new_task_modal(frame: &mut Frame, app: &App) {
    let Modal::NewTask(form) = app.modal() else {
        return;
    };
    let area = modal_area(frame, 76, 66);
    frame.render_widget(Clear, area);

    let mut lines = Vec::new();

    match form.stage {
        NewTaskStage::Pick => {
            lines.push(Line::from(Span::styled(
                "Workflow to run",
                Style::default().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));

            if form.choices.is_empty() {
                lines.push(Line::from(Span::styled(
                    "No workflow is reachable — not even a built-in one.",
                    Style::default().fg(Color::Red),
                )));
            }

            for (index, choice) in form.choices.iter().enumerate() {
                let focused = index == form.choice;
                lines.push(Line::from(Span::styled(
                    format!("{} {}", if focused { "›" } else { " " }, choice.label()),
                    if focused {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::default()
                    },
                )));
            }

            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "← / → (or j / k) change the selection; Enter loads and validates the \
                 workflow. The source says whether it is a built-in or your own copy.",
                Style::default().fg(Color::DarkGray),
            )));
        }
        NewTaskStage::Definition => {
            lines.push(Line::from(Span::styled(
                "Path of the workflow definition to run",
                Style::default().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("› {}", form.path),
                Style::default().add_modifier(Modifier::REVERSED),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "A path or a name: Enter loads and validates it, and the input form is \
                 generated from what it declares. Esc returns to the list.",
                Style::default().fg(Color::DarkGray),
            )));
        }
        NewTaskStage::Inputs => {
            let name = form
                .definition
                .as_ref()
                .map(|definition| definition.id.clone())
                .unwrap_or_default();
            let origin = form
                .resolved
                .as_ref()
                .map(|resolved| format!("  [{}]", resolved.origin))
                .unwrap_or_default();
            lines.push(Line::from(Span::styled(
                format!("Inputs of '{name}'{origin}"),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));

            if form.parameters.is_empty() {
                lines.push(Line::from(Span::styled(
                    "This workflow declares no inputs — press Enter to start it.",
                    Style::default().fg(Color::DarkGray),
                )));
            }

            for (index, parameter) in form.parameters.iter().enumerate() {
                let focused = index == form.row;
                let requirement = if parameter.required { "*" } else { " " };
                let value = form.values.get(index).cloned().unwrap_or_default();
                let hint = match form.choices(index) {
                    Some(options) => format!("  [{}]", options.join(" | ")),
                    None => String::new(),
                };
                lines.push(Line::from(Span::styled(
                    format!(
                        "{} {}{requirement} ({:?}): {value}{hint}",
                        if focused { "›" } else { " " },
                        parameter.name,
                        parameter.param_type
                    ),
                    if focused {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::default()
                    },
                )));
            }
        }
    }

    if !form.violations.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("{} violations:", form.violations.len()),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
        for violation in &form.violations {
            lines.push(Line::from(Span::styled(
                format!("  - {violation}"),
                Style::default().fg(Color::Red),
            )));
        }
    }

    if let Some(error) = &form.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(Color::Red),
        )));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" New task ");

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// The key map.
fn render_help_modal(frame: &mut Frame) {
    let area = modal_area(frame, 60, 70);
    frame.render_widget(Clear, area);

    let lines = vec![
        Line::from(Span::styled(
            "Navigation",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from("  Tab / Shift+Tab   next / previous panel"),
        Line::from("  j / k, ↓ / ↑      down / up"),
        Line::from("  g / G             first / last entry"),
        Line::from("  r                 reload from the store"),
        Line::from("  q, Esc            quit"),
        Line::from(""),
        Line::from(Span::styled(
            "Operations",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from("  n                 new task (pick a workflow and start it)"),
        Line::from("  R                 repositories (a — register, d — remove)"),
        Line::from("  s                 run / resume the selected execution"),
        Line::from("  i                 answer the pending input: gate, agent question, problem;"),
        Line::from(
            "                    on a failed step: diagnosis, proposals, restart with a note",
        ),
        Line::from("  t                 manual transition (o — override mode)"),
        Line::from("  x                 interrupt active execution (confirmation required)"),
        Line::from("  f                 follow the event stream"),
        Line::from("  Enter             (Events panel) open the event's detail window"),
        Line::from("  c                 (Events panel / detail window) open its conversation"),
        Line::from(""),
        Line::from(Span::styled(
            "Task view",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from("  Content focus     j/k, PgUp/PgDn, g/G read transcript history; e/Enter expands detail; v entry RAW; Ctrl+v conversation RAW"),
        Line::from("  c                 conductor chat: Tab cycles draft / history / action target"),
        Line::from("                    draft types and Enter sends; history uses navigation, e detail, v entry RAW / Ctrl+v conversation RAW; [/] chooses actions"),
        Line::from("                    y / n confirm / reject a proposal, u undoes a decision"),
        Line::from(""),
        Line::from(Span::styled(
            "Event detail window",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from("  j / k, PgUp/PgDn  scroll,  g / G  top / bottom"),
        Line::from("  c                 open the conversation,  Esc — close"),
        Line::from(""),
        Line::from(Span::styled(
            "Form",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from("  Tab / ↓           next field"),
        Line::from("  Shift+Tab / ↑     previous field"),
        Line::from("  ← / → / space     change a select value, or the picked workflow"),
        Line::from("  letters / Backspace  edit a text field"),
        Line::from("  ← / →             (problem data/variables) edit the key / the value"),
        Line::from("  Enter             submit,  Esc — cancel"),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Key map ");

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_roles_have_distinct_backgrounds() {
        assert_eq!(
            transcript_style(TranscriptPresentation::Operator).bg,
            Some(Color::Rgb(33, 71, 93))
        );
        assert_eq!(
            transcript_style(TranscriptPresentation::Agent).bg,
            Some(Color::Rgb(48, 67, 52))
        );
        assert_eq!(
            transcript_style(TranscriptPresentation::Error).bg,
            Some(Color::Red)
        );
    }

    fn line(text: &str, header: bool, selected: bool) -> TranscriptLine {
        TranscriptLine {
            entry_id: "e".into(),
            logical_line: 0,
            text: text.to_string(),
            presentation: TranscriptPresentation::Agent,
            header,
            selected,
        }
    }

    /// AE1/AE2: the role colour covers the whole header row and nothing else;
    /// the left marker is drawn on the selected entry only.
    #[test]
    fn a_header_fills_the_width_in_its_role_colour_and_the_body_stays_neutral() {
        let header = transcript_line(line("AGENT · Agent · 2026-10-09 10:00:00", true, true), 60);
        let text: String = header
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(text.chars().count(), 60);
        assert!(text.starts_with("┃ AGENT"));
        assert_eq!(
            header.style.bg,
            transcript_style(TranscriptPresentation::Agent).bg
        );

        let body = transcript_line(line("the answer", false, true), 60);
        assert_eq!(body.style.bg, None);
        assert!(body.spans[0].content.starts_with("┃ "));

        let other = transcript_line(line("not selected", false, false), 60);
        assert!(other.spans[0].content.starts_with("  "));
    }

    #[test]
    fn the_five_roles_have_five_distinct_header_styles() {
        let styles = [
            TranscriptPresentation::Operator,
            TranscriptPresentation::Agent,
            TranscriptPresentation::Conductor,
            TranscriptPresentation::System,
            TranscriptPresentation::Technical,
        ]
        .map(|presentation| transcript_style(presentation).bg);
        for (index, style) in styles.iter().enumerate() {
            assert!(style.is_some());
            assert!(!styles[..index].contains(style), "{styles:?}");
        }
    }
}
