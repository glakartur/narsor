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
    event_line, status_name, App, FormRow, Modal, NewTaskStage, Panel, RepositoryField,
    TransitionMode,
};
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
    let areas = layout(frame.area());

    render_executions(frame, app, areas.executions);
    render_details(frame, app, areas.details);
    render_events(frame, app, areas.events);
    render_shortcuts(frame, app, areas.shortcuts);

    match app.modal() {
        Modal::None => {}
        Modal::Input(_) => render_input_modal(frame, app),
        Modal::Transition(_) => render_transition_modal(frame, app),
        Modal::Confirm(_) => render_confirm_modal(frame, app),
        Modal::Repositories(_) => render_repository_list_modal(frame, app),
        Modal::RepositoryForm(_) => render_repository_form_modal(frame, app),
        Modal::RepositoryRemove(_) => render_repository_remove_modal(frame, app),
        Modal::NewTask(_) => render_new_task_modal(frame, app),
        Modal::Help => render_help_modal(frame),
    }
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
            "No executions in the store.\n\nStart a workflow with\n`orchestrator run <file>`.",
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
        None => Span::styled(" Orchestrator ", Style::default().fg(Color::DarkGray)),
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
        lines.push(Line::from(Span::styled(
            format!("{} {entry}", if focused { "›" } else { " " }),
            if focused {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            },
        )));
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

    frame.render_widget(Paragraph::new(lines).block(block), area);
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

/// The confirmation a repository removal has to pass.
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
                "Enter loads and validates it; the input form is generated from \
                 what it declares.",
                Style::default().fg(Color::DarkGray),
            )));
        }
        NewTaskStage::Inputs => {
            let name = form
                .definition
                .as_ref()
                .map(|definition| definition.id.clone())
                .unwrap_or_default();
            lines.push(Line::from(Span::styled(
                format!("Inputs of '{name}'"),
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
        Line::from("  n                 new task (load a definition and start it)"),
        Line::from("  R                 repositories (a — register, d — remove)"),
        Line::from("  s                 run / resume the selected execution"),
        Line::from("  i                 human input form"),
        Line::from("  t                 manual transition (o — override mode)"),
        Line::from("  f                 follow the event stream"),
        Line::from(""),
        Line::from(Span::styled(
            "Form",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from("  Tab / ↓           next field"),
        Line::from("  Shift+Tab / ↑     previous field"),
        Line::from("  ← / → / space     change a select value"),
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
