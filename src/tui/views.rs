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
    event_detail_lines, event_line, status_name, task_conversation_lines, task_events_lines,
    task_position_line, App, FormRow, Modal, NewTaskStage, Panel, RepositoryField, Screen,
    TaskContent, TaskFocus, TaskView, TransitionMode,
};
use super::scroll::wrap_visible;
use crate::domain::{ExecutionStatus, HumanInputType};

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
pub fn render(frame: &mut Frame, app: &App) {
    if let Screen::Task(view) = app.screen() {
        render_task_view(frame, app, view);
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

/// The full-screen task view (U7): the step timeline on the left, the
/// selected step's content on the right.
fn render_task_view(frame: &mut Frame, app: &App, view: &TaskView) {
    let [body, shortcuts] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(SHORTCUT_BAR_HEIGHT)])
            .areas(frame.area());
    let [axis_area, content_area] =
        Layout::horizontal([Constraint::Percentage(32), Constraint::Percentage(68)]).areas(body);

    render_task_axis(frame, view, axis_area);
    render_task_content(frame, view, content_area);
    render_shortcuts(frame, app, shortcuts);
}

/// The task view's left pane: every position, in timeline order (R7).
fn render_task_axis(frame: &mut Frame, view: &TaskView, area: Rect) {
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
        .title(format!(" Steps — {} ", view.execution_id));

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
        TaskContent::Conversation { closed, .. } => {
            if *closed {
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

    // The content pane always renders starting at the selected entry, so a
    // variable-height entry (a collapsed vs. an expanded tool result) never
    // has to be measured in advance to keep the cursor on screen — the same
    // reasoning `render_task_axis` applies to positions, adapted for
    // entries whose rendered height is not uniform.
    let cursor = view.content_scroll.offset();
    let lines: Vec<String> = match &view.content {
        TaskContent::Conversation { entries, .. } => {
            let start = cursor.min(entries.len());
            task_conversation_lines(&entries[start..], 0, &view.expanded)
        }
        TaskContent::Events { message, events } => {
            let start = cursor.min(events.len());
            task_events_lines(message, &events[start..], 0)
        }
        TaskContent::Empty => vec!["Loading…".to_string()],
    };

    let inner_width = area.width.saturating_sub(2).max(1) as usize;
    let inner_height = area.height.saturating_sub(2).max(1) as usize;
    let visible: Vec<Line> = wrap_visible(&lines, inner_width, 0, inner_height)
        .into_iter()
        .map(Line::from)
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

/// The colour a status is drawn in.
fn status_style(status: &ExecutionStatus) -> Style {
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
    let block = panel_block(
        app,
        Panel::Executions,
        Some(format!("{}", app.executions().len())),
    );

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
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(short, Style::default().fg(Color::White)),
                    Span::raw("  "),
                    Span::styled(status_name(&row.status), status_style(&row.status)),
                ]),
                Line::from(Span::styled(
                    format!(
                        "  {} · {}",
                        row.workflow_id,
                        row.current_state.as_deref().unwrap_or("-")
                    ),
                    Style::default().fg(Color::DarkGray),
                )),
            ])
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
                status_name(&execution.status),
                status_style(&execution.status),
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

    if let Some(pending) = app.pending() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("Waiting for an answer: {}", pending.instruction()),
            Style::default().fg(Color::Yellow),
        )));
        lines.push(Line::from(Span::styled(
            "Press i to open the form.",
            Style::default().fg(Color::DarkGray),
        )));
    }

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
    let area = modal_area(frame, 70, 60);
    frame.render_widget(Clear, area);

    let mut lines = vec![
        Line::from(Span::styled(
            form.instruction.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];

    let rows = form.rows();
    for (index, row) in rows.iter().enumerate() {
        let focused = index == form.row;
        let marker = if focused { "›" } else { " " };
        let line = match row {
            FormRow::Field(field_index) => {
                let field = &form.fields[*field_index];
                let requirement = if field.required { "*" } else { " " };
                let value = form.values[*field_index].clone();
                let hint = match field.field_type {
                    HumanInputType::Select => {
                        format!("  [{}]", field.options.join(" | "))
                    }
                    HumanInputType::Text => String::new(),
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
                format!("{marker} output label: {value}")
            }
        };

        lines.push(Line::from(Span::styled(
            line,
            if focused {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            },
        )));
    }

    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "This action declares no fields: pick an output label with t.",
            Style::default().fg(Color::DarkGray),
        )));
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
        .title(" Human input ");

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
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
        Line::from("  i                 human input form"),
        Line::from("  t                 manual transition (o — override mode)"),
        Line::from("  x                 interrupt active execution (confirmation required)"),
        Line::from("  f                 follow the event stream"),
        Line::from("  Enter             (Events panel) open the event's detail window"),
        Line::from("  c                 (Events panel / detail window) open its conversation"),
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
        Line::from("  Enter             submit,  Esc — cancel"),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Key map ");

    frame.render_widget(Paragraph::new(lines).block(block), area);
}
