//! One queue: its automations, its entries (R23) and the problems of its
//! catalogue file. Read only.
//!
//! ```text
//! ╭▸ Queue mr-review · on_episodes · 2 automations · 1 entry ───────────╮
//! │ ! queues/mr-review.yaml: automation `gone` is not in the catalogue   │
//! │╭ Akcje ───────────────────────────╮ ╭ Opis ─────────────────────────╮│
//! ││ │ 1 gitlab-mr · active           │ │ scripts: in, check            ││
//! ││   2 ! github-pr · error          │ │ state: active                 ││
//! │╰──────────────────────────────────╯ ╰───────────────────────────────╯│
//! │ Entries (1)                                                          │
//! │ task-1 · review_work · episode input-1 · since 2026-11-17 10:05 UTC  │
//! │   mr_url=https://… · label=ai-review                                 │
//! │   gitlab-mr: last run 2026-11-17 10:07 UTC · newest error: failed …  │
//! ```

use chrono::{DateTime, Utc};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use narsor_protocol::controller::{
    ObserverQueueView, ObserverRunErrorView, QueueAutomationView, QueueEntryAutomationView,
    QueueEntryView,
};

use super::super::chooser::{self, Chooser, ChooserItem};
use super::super::scroll::one_line;
use super::super::style::{Role, Styles};
use super::list::{entries_text, schedule_text};
use super::{problem_text, CATALOGUE_HINT, PROBLEM_MARK};

/// Most errors of one automation the description shows, newest first.
pub const SHOWN_ERRORS: usize = 3;

/// One queue as the view shows it.
#[derive(Debug)]
pub struct QueueDetail {
    view: ObserverQueueView,
    automations: Chooser,
    entries_scroll: usize,
}

impl QueueDetail {
    /// The queue of `view`, on its first automation.
    pub fn new(view: ObserverQueueView) -> Self {
        let automations = Chooser::new(view.automations.iter().map(automation_item).collect());
        Self {
            view,
            automations,
            entries_scroll: 0,
        }
    }

    /// Takes a new read of the same queue in; the highlighted automation
    /// stays.
    pub fn set_view(&mut self, view: ObserverQueueView) {
        let kept = self
            .selected_automation()
            .map(|automation| automation.automation.clone());
        let index = kept
            .and_then(|name| view.automations.iter().position(|a| a.automation == name))
            .unwrap_or(0);
        self.automations =
            Chooser::new(view.automations.iter().map(automation_item).collect()).selecting(index);
        self.view = view;
    }

    /// What the server said of the queue.
    pub fn view(&self) -> &ObserverQueueView {
        &self.view
    }

    /// The list of automations.
    pub fn automations(&self) -> &Chooser {
        &self.automations
    }

    /// The list of automations, to move on it.
    pub fn automations_mut(&mut self) -> &mut Chooser {
        &mut self.automations
    }

    /// The highlighted automation.
    pub fn selected_automation(&self) -> Option<&QueueAutomationView> {
        self.view.automations.get(self.automations.selected())
    }

    /// Scrolls the entries by `delta` lines.
    pub fn scroll_entries(&mut self, delta: isize) {
        self.entries_scroll = self.entries_scroll.saturating_add_signed(delta);
    }
}

/// How a time reads in the view: UTC, to the minute.
pub fn time_text(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d %H:%M UTC").to_string()
}

/// Whether the (queue, automation) pair is stopped: it runs again only
/// when its queue file, `automation.yaml` or `check` script changes in the
/// catalogue.
fn stopped(view: &QueueAutomationView) -> bool {
    view.state == "error"
}

/// The run in progress or the last one, in words.
pub fn last_run_text(view: &QueueAutomationView) -> String {
    let last = view.last_run.as_ref().map(|run| {
        format!(
            "last run: #{} {} at {}",
            run.sequence,
            one_line(&run.status),
            time_text(run.finished_at)
        )
    });
    match (view.active_run, last) {
        (Some(active), Some(last)) => format!("run #{active} in progress · {last}"),
        (Some(active), None) => format!("run #{active} in progress"),
        (None, Some(last)) => last,
        (None, None) => "last run: none yet".to_string(),
    }
}

/// How an automation reads on the list: its name and state, marked when
/// it is stopped; the rest in the description.
pub fn automation_item(view: &QueueAutomationView) -> ChooserItem {
    let mark = if stopped(view) {
        format!("{PROBLEM_MARK} ")
    } else {
        String::new()
    };
    let label = format!(
        "{mark}{} · {}",
        one_line(&view.automation),
        one_line(&view.state)
    );
    ChooserItem::new(label, automation_description(view).join("\n"))
}

fn automation_description(view: &QueueAutomationView) -> Vec<String> {
    let mut lines = Vec::new();
    if !view.description.trim().is_empty() {
        lines.push(one_line(&view.description));
    }
    let scripts = if view.scripts.is_empty() {
        "none".to_string()
    } else {
        view.scripts
            .iter()
            .map(|script| one_line(script))
            .collect::<Vec<_>>()
            .join(", ")
    };
    lines.push(format!("scripts: {scripts}"));
    lines.push(format!("state: {}", one_line(&view.state)));
    if stopped(view) {
        lines.push(format!(
            "{PROBLEM_MARK} stopped: it runs again when its queue file, automation.yaml or check script changes"
        ));
    }
    if let Some(next) = view.next_run_at {
        lines.push(format!("next run: {}", time_text(next)));
    }
    lines.push(last_run_text(view));
    if let Some(error) = view.last_run.as_ref().and_then(|run| run.error.as_deref()) {
        lines.push(format!("error: {}", one_line(error)));
    }
    if view.consecutive_failures > 0 {
        lines.push(format!(
            "failed runs in a row: {} (backoff)",
            view.consecutive_failures
        ));
    }
    if let Some(since) = view.waiting_for_claim_since {
        lines.push(format!("waits for a worker since {}", time_text(since)));
    }
    if view.worker_missing {
        lines.push(format!(
            "{PROBLEM_MARK} a due run waits for a worker: is the observer worker running?"
        ));
    }
    if let Some(at) = view.last_claim_at {
        lines.push(format!("last claim: {}", time_text(at)));
    }
    if !view.errors.is_empty() {
        lines.push(String::new());
        lines.push(format!("recent errors ({}):", view.errors.len()));
        lines.extend(view.errors.iter().take(SHOWN_ERRORS).map(error_text));
    }
    lines.push(String::new());
    let secrets = if view.secrets.is_empty() {
        "none".to_string()
    } else {
        view.secrets
            .iter()
            .map(|secret| format!("{} → {}", one_line(&secret.name), one_line(&secret.env)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    lines.push(format!("secrets: {secrets}"));
    lines.push(CATALOGUE_HINT.to_string());
    lines
}

/// One kept execution error, on one line.
pub fn error_text(error: &ObserverRunErrorView) -> String {
    let code = error
        .exit_code
        .map(|code| format!(", exit {code}"))
        .unwrap_or_default();
    format!(
        "#{} {}: {}{code} at {}: {}",
        error.sequence,
        one_line(&error.phase),
        one_line(&error.status),
        time_text(error.finished_at),
        one_line(&error.error)
    )
}

/// The lines of one entry: the task, the step, the episode and the start;
/// its params; and per automation the processing state (R23).
pub fn entry_lines(entry: &QueueEntryView, styles: &Styles) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(
        format!(
            "{} · {} · episode {} · since {}",
            one_line(&entry.task_id),
            one_line(&entry.step),
            one_line(&entry.episode),
            time_text(entry.started_at)
        ),
        styles.of(Role::Calm),
    )];
    if !entry.params.is_empty() {
        let params = entry
            .params
            .iter()
            .map(|(name, value)| format!("{}={}", one_line(name), one_line(value)))
            .collect::<Vec<_>>()
            .join(" · ");
        lines.push(Line::styled(format!("  {params}"), styles.of(Role::Done)));
    }
    for automation in &entry.automations {
        let role = if automation.last_error.is_some() {
            Role::Problem
        } else {
            Role::Done
        };
        lines.push(Line::styled(
            format!("  {}", entry_automation_text(automation)),
            styles.of(role),
        ));
    }
    lines
}

/// How one automation processed one entry, in words.
pub fn entry_automation_text(automation: &QueueEntryAutomationView) -> String {
    let mut parts = Vec::new();
    if automation.running {
        parts.push("running".to_string());
    }
    parts.push(match automation.last_run_at {
        Some(at) => format!("last run {}", time_text(at)),
        None => "no run yet".to_string(),
    });
    if let Some(error) = &automation.last_error {
        let code = error
            .exit_code
            .map(|code| format!(" (exit {code})"))
            .unwrap_or_default();
        parts.push(format!(
            "newest error: {}{code}: {}",
            one_line(&error.status),
            one_line(&error.error)
        ));
    }
    format!(
        "{}: {}",
        one_line(&automation.automation),
        parts.join(" · ")
    )
}

/// The title of a queue's frame: its name, mode, schedule and counts.
fn queue_title(view: &ObserverQueueView) -> String {
    let automations = match view.automations.len() {
        1 => "1 automation".to_string(),
        n => format!("{n} automations"),
    };
    let entries = entries_text(u32::try_from(view.entries.len()).unwrap_or(u32::MAX));
    let mut parts = vec![format!("Queue {}", one_line(&view.queue))];
    if let Some(mode) = view.mode.as_deref() {
        parts.push(one_line(mode));
    }
    if view.interval_secs.is_some() || view.cron.is_some() {
        parts.push(schedule_text(view.interval_secs, view.cron.as_deref()));
    }
    parts.push(automations);
    parts.push(entries);
    format!(" {} ", parts.join(" · "))
}

/// Draws a queue into `area`: the problems of its file on top, the
/// automations with their description, the entries under them.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    detail: &mut QueueDetail,
    styles: &Styles,
    focused: bool,
) {
    let inner = styles.render_panel(frame, area, queue_title(&detail.view), focused);
    let problems: Vec<Line> = detail
        .view
        .problems
        .iter()
        .map(|problem| {
            Line::styled(
                format!("{PROBLEM_MARK} {}", problem_text(problem)),
                styles.of(Role::Problem),
            )
        })
        .collect();
    let problem_rows = u16::try_from(problems.len())
        .unwrap_or(u16::MAX)
        .min((inner.height / 4).max(1));
    let body: Vec<Line> = detail
        .view
        .entries
        .iter()
        .flat_map(|entry| entry_lines(entry, styles))
        .collect();
    // The entries take what they need, up to a little under half; the
    // automations' description gets the rest.
    let wanted = u16::try_from(body.len().max(1) + 1).unwrap_or(u16::MAX);
    let entries_rows = wanted.min((inner.height * 9 / 20).max(4));
    let [head, top, bottom] = Layout::vertical([
        Constraint::Length(problem_rows),
        Constraint::Min(4),
        Constraint::Length(entries_rows),
    ])
    .areas(inner);
    if !problems.is_empty() {
        frame.render_widget(Paragraph::new(problems).wrap(Wrap { trim: false }), head);
    }
    if detail.view.automations.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled("No automation serves this queue.", styles.of(Role::Calm)),
                Line::styled(
                    "Its entries wait for a person. The queue file \
                     (queues/NAME.yaml) names the automations that serve it.",
                    styles.of(Role::Done),
                ),
            ])
            .wrap(Wrap { trim: false }),
            top,
        );
    } else {
        chooser::render(frame, top, &mut detail.automations, styles, focused);
    }

    let mut lines = vec![Line::styled(
        format!("Entries ({})", detail.view.entries.len()),
        styles.of(Role::Focus),
    )];
    if detail.view.entries.is_empty() {
        lines.push(Line::styled(
            "No entries: no task waits at a gate that names this queue.",
            styles.of(Role::Done),
        ));
    }
    let room = usize::from(bottom.height.saturating_sub(1));
    let most = body.len().saturating_sub(room);
    detail.entries_scroll = detail.entries_scroll.min(most);
    lines.extend(body.into_iter().skip(detail.entries_scroll));
    frame.render_widget(Paragraph::new(lines), bottom);
}
