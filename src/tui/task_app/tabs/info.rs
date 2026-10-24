//! *Info*: what the control plane knows about the task — what it was sealed
//! with, the workflow, the workspace, the runner, the controller.
//!
//! Everything is read from the task summary the list already holds, so the tab
//! never waits for a read and has nothing that can fail. Ages are counted from
//! the moment the summary was made (`as_of`), not from the clock of this
//! terminal.

use chrono::{DateTime, Utc};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::controller_protocol::{Freshness, TaskSummary};
use crate::tui::keymap::Action;
use crate::tui::style::{Role, Styles};
use narsor_protocol::controller::{QueueReason, TaskState};

use super::super::super::scroll::one_line;
use super::super::detail::short_age;
use crate::cli::task_text::{task_state_name, without_slot};

/// What the tab remembers per task: how far it is scrolled.
#[derive(Debug, Clone)]
pub struct Info {
    top: usize,
    height: usize,
}

impl Default for Info {
    fn default() -> Self {
        Self { top: 0, height: 10 }
    }
}

impl Info {
    /// The first line shown.
    pub fn top(&self) -> usize {
        self.top
    }

    /// What a key does.
    pub fn on_action(&mut self, action: Action) {
        let page = self.height.max(1);
        match action {
            Action::Down => self.top += 1,
            Action::Up => self.top = self.top.saturating_sub(1),
            Action::PageDown => self.top += page,
            Action::PageUp => self.top = self.top.saturating_sub(page),
            Action::Top => self.top = 0,
            Action::Bottom => self.top = usize::MAX,
            _ => {}
        }
    }
}

/// A heading, then `label  value` rows under it.
struct Sheet<'a> {
    styles: &'a Styles,
    /// Without a connection everything is grey.
    stale: bool,
    lines: Vec<Line<'static>>,
}

impl Sheet<'_> {
    fn grey(&self, role: Role) -> Role {
        if self.stale {
            Role::Done
        } else {
            role
        }
    }

    fn heading(&mut self, text: &str) {
        if !self.lines.is_empty() {
            self.lines.push(Line::raw(""));
        }
        self.lines.push(Line::styled(
            text.to_string(),
            self.styles.of(self.grey(Role::Focus)),
        ));
    }

    fn row(&mut self, label: &str, value: impl Into<String>, role: Role) {
        self.lines.push(Line::from(vec![
            Span::styled(
                format!("  {:<14}", one_line(label)),
                self.styles.of(Role::Done),
            ),
            Span::styled(one_line(&value.into()), self.styles.of(self.grey(role))),
        ]));
    }
}

fn ago(task: &TaskSummary, at: DateTime<Utc>) -> String {
    format!("{} temu", short_age((task.as_of - at).num_seconds()))
}

fn freshness_word(freshness: Freshness) -> &'static str {
    match freshness {
        Freshness::Live => "aktualne",
        Freshness::Stale => "nieaktualne",
    }
}

fn presence(present: bool) -> &'static str {
    if present {
        "jest"
    } else {
        "brak"
    }
}

/// The lines of the tab for `task`.
pub fn lines(task: &TaskSummary, styles: &Styles, stale: bool) -> Vec<Line<'static>> {
    let mut sheet = Sheet {
        styles,
        stale,
        lines: Vec::new(),
    };

    sheet.heading("Zapieczętowane wejścia");
    sheet.row("zadanie", task.task_id.to_string(), Role::Calm);
    sheet.row("backend", task.backend_id.to_string(), Role::Calm);
    sheet.row(
        "utworzone",
        task.created_at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        Role::Calm,
    );
    sheet.row("launch digest", task.launch_digest.clone(), Role::Calm);
    sheet.row("środowisko", task.environment_identity.clone(), Role::Calm);
    if let Some(ticket) = task.ticket.as_deref().filter(|ticket| !ticket.is_empty()) {
        sheet.row("ticket", ticket, Role::Calm);
    }
    // The task's own title replaces the one the workflow's `display` renders.
    match task.title.as_deref().filter(|title| !title.is_empty()) {
        Some(title) => sheet.row("tytuł", title, Role::Calm),
        None if !task.display.title.trim().is_empty() => {
            sheet.row("tytuł", task.display.title.clone(), Role::Calm);
        }
        None => {}
    }
    for link in &task.display.links {
        let target = link.url.as_deref().unwrap_or("brak adresu");
        sheet.row(&link.label, format!("{} → {target}", link.text), Role::Calm);
    }
    if task.inputs.is_empty() {
        sheet.row("odpowiedzi", "brak", Role::Done);
    }
    for input in &task.inputs {
        let mut text = format!(
            "{} dla {}: {:?}",
            input.input_id, input.pending_input_id, input.status
        );
        if let Some(detail) = &input.detail {
            text.push_str(&format!(" ({detail})"));
        }
        sheet.row("odpowiedź", text, Role::Calm);
    }

    sheet.heading("Workflow");
    sheet.row(
        "workflow",
        format!("{} v{}", task.workflow_id, task.workflow_version),
        Role::Calm,
    );
    sheet.row("stan", task_state_name(task.state), Role::Calm);
    sheet.row("slot", slot_text(task), Role::Calm);
    sheet.row(
        "pozycja",
        format!(
            "#{} · generacja {} · rewizja {}",
            task.rank, task.generation, task.revision
        ),
        Role::Calm,
    );
    if let Some(queue) = &task.queue {
        let text = match &queue.reason {
            QueueReason::WaitingForCapacity {
                position,
                running,
                limit,
            } => {
                format!("czeka na slot: miejsce {position}, zajęte {running} z {limit}")
            }
            QueueReason::ControllerOffline => "kontroler offline — nic nie startuje".to_string(),
            QueueReason::Admitted => "przyjęte, czeka na start przez kontroler".to_string(),
            QueueReason::BackendUnknown { detail } => format!("backend nieznany: {detail}"),
        };
        sheet.row(
            "kolejka",
            format!("{text} · od {}", ago(task, queue.since)),
            Role::Calm,
        );
    }
    if let Some(archive) = &task.archive {
        let text = match archive.completed_at {
            Some(at) => format!("zarchiwizowane {}", ago(task, at)),
            None => format!("archiwizacja zlecona {}", ago(task, archive.requested_at)),
        };
        sheet.row("archiwum", text, Role::Done);
    }

    sheet.heading("Workspace");
    match &task.observation {
        Some(observation) => {
            sheet.row(
                "workload",
                presence(observation.summary.workload_present),
                Role::Calm,
            );
            sheet.row(
                "storage",
                presence(observation.summary.storage_present),
                Role::Calm,
            );
            sheet.row(
                "obserwacja",
                format!(
                    "{}, {}",
                    ago(task, observation.observed_at),
                    freshness_word(observation.freshness)
                ),
                if observation.freshness == Freshness::Stale {
                    Role::Attention
                } else {
                    Role::Calm
                },
            );
            if let Some(detail) = &observation.summary.detail {
                sheet.row("szczegół", detail.clone(), Role::Done);
            }
        }
        None => sheet.row("obserwacja", "jeszcze nic", Role::Done),
    }

    sheet.heading("Runner");
    match &task.runner {
        Some(runner) => {
            sheet.row("status", runner.execution_status.as_str(), Role::Calm);
            if let Some(state) = &runner.execution_cursor.current_state {
                sheet.row("krok", state.clone(), Role::Calm);
            }
            sheet.row("incarnacja", runner.incarnation.clone(), Role::Done);
            sheet.row(
                "zgłoszenie",
                format!(
                    "{}, {}",
                    ago(task, runner.observed_at),
                    freshness_word(runner.freshness)
                ),
                if runner.freshness == Freshness::Stale {
                    Role::Attention
                } else {
                    Role::Calm
                },
            );
            if let Some(problem) = &runner.problem {
                sheet.row(
                    "problem",
                    format!("{}: {}", problem.code, problem.message),
                    Role::Problem,
                );
            }
            if runner.conductor_available {
                sheet.row("dyrygent", "dostępny", Role::Conductor);
            } else if let Some(reason) = &runner.conductor_unavailable_reason {
                sheet.row("dyrygent", format!("niedostępny: {reason}"), Role::Done);
            }
            if let Some(unavailable) = &runner.default_harness_unavailable {
                sheet.row(
                    "harness",
                    format!(
                        "{} niedostępny: {}",
                        unavailable.harness, unavailable.reason
                    ),
                    Role::Attention,
                );
            }
        }
        None => sheet.row("status", "runner jeszcze się nie zgłosił", Role::Done),
    }

    sheet.heading("Kontroler");
    match &task.controller {
        Some(controller) => {
            let live = controller.freshness == Freshness::Live;
            sheet.row("kontroler", controller.controller_id.clone(), Role::Calm);
            sheet.row(
                "stan",
                if live { "online" } else { "offline" },
                if live { Role::Calm } else { Role::Attention },
            );
            let heartbeat = controller
                .last_heartbeat_at
                .map_or_else(|| "nigdy".to_string(), |at| ago(task, at));
            sheet.row("heartbeat", heartbeat, Role::Done);
            sheet.row(
                "epoka dzierżawy",
                controller.lease_epoch.to_string(),
                Role::Done,
            );
        }
        None => sheet.row(
            "kontroler",
            "żaden nie trzyma dzierżawy backendu",
            Role::Attention,
        ),
    }
    sheet.lines
}

/// Draws the tab.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    styles: &Styles,
    info: &mut Info,
    task: &TaskSummary,
    stale: bool,
) {
    let lines = lines(task, styles, stale);
    info.height = usize::from(area.height).max(1);
    info.top = info.top.min(lines.len().saturating_sub(info.height));
    let shown: Vec<Line> = lines.into_iter().skip(info.top).take(info.height).collect();
    frame.render_widget(Paragraph::new(shown), area);
}

/// Whether the task holds a slot of its backend, and what that means for the
/// state it is in.
fn slot_text(task: &TaskSummary) -> &'static str {
    if task.state == TaskState::Queued && task.runner.is_some() {
        "w kolejce po wznowieniu — dostanie slot jako pierwsze"
    } else if without_slot(task) {
        "bez slotu — odpowiedź lub wznowienie przywraca zadanie"
    } else if task.holds_slot {
        "zajęty"
    } else {
        "brak"
    }
}
