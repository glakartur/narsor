//! The list of the queues: one entry per queue with its number of entries
//! and a mark for a queue that does not run; the description says its
//! mode, schedule and automations, or why it does not run. Under the list,
//! the catalogue files that failed to load.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use narsor_protocol::controller::ObserverQueueSummary;

use super::super::chooser::{self, ChooserItem};
use super::super::scroll::one_line;
use super::super::style::{Role, Styles};
use super::{problem_text, Queues, CATALOGUE_HINT, CLI_COMMAND, PROBLEM_MARK};

/// What the list says when the catalogue has no queue.
pub const EMPTY: &str = "No queues.";

/// `n entries`, with the singular for one.
pub fn entries_text(count: u32) -> String {
    match count {
        1 => "1 entry".to_string(),
        n => format!("{n} entries"),
    }
}

/// A schedule in words: `every 300 s`, `cron */5 * * * * (UTC)` or `none`.
pub fn schedule_text(interval_secs: Option<u64>, cron: Option<&str>) -> String {
    match (cron, interval_secs) {
        (Some(cron), _) => format!("cron {} (UTC)", one_line(cron)),
        (None, Some(seconds)) => format!("every {seconds} s"),
        (None, None) => "none".to_string(),
    }
}

/// How a queue reads on the list: its name and its number of entries,
/// marked when it does not run; the rest in the description.
pub fn queue_item(queue: &ObserverQueueSummary) -> ChooserItem {
    let mark = if queue.problem.is_some() {
        format!("{PROBLEM_MARK} ")
    } else {
        String::new()
    };
    let label = format!(
        "{mark}{} · {}",
        one_line(&queue.queue),
        entries_text(queue.entries)
    );
    let mut lines = Vec::new();
    if let Some(problem) = &queue.problem {
        lines.push(format!(
            "{PROBLEM_MARK} does not run: {}",
            one_line(problem)
        ));
    }
    let mode = queue
        .mode
        .as_deref()
        .map_or_else(|| "none".to_string(), one_line);
    lines.push(format!("mode: {mode}"));
    lines.push(format!(
        "schedule: {}",
        schedule_text(queue.interval_secs, queue.cron.as_deref())
    ));
    let automations = if queue.automations.is_empty() {
        "none".to_string()
    } else {
        queue
            .automations
            .iter()
            .map(|automation| one_line(automation))
            .collect::<Vec<_>>()
            .join(", ")
    };
    lines.push(format!("automations: {automations}"));
    lines.push(format!("entries: {}", queue.entries));
    lines.push(String::new());
    lines.push("Enter opens the queue: its automations and its entries.".to_string());
    lines.push(CATALOGUE_HINT.to_string());
    ChooserItem::new(label, lines.join("\n"))
}

/// The lines of the catalogue files that failed to load; none when every
/// file loaded.
fn problem_lines(queues: &Queues, styles: &Styles) -> Vec<Line<'static>> {
    if queues.problems().is_empty() {
        return Vec::new();
    }
    let mut lines = vec![Line::styled(
        format!("Catalogue files not loaded ({})", queues.problems().len()),
        styles.of(Role::Problem),
    )];
    lines.extend(queues.problems().iter().map(|problem| {
        Line::styled(
            format!("{PROBLEM_MARK} {}", problem_text(problem)),
            styles.of(Role::Problem),
        )
    }));
    lines
}

/// Draws the list into `area`: a frame titled with how many queues there
/// are, and in it the list and the description (or the empty state), with
/// the catalogue files that failed to load under them.
pub fn render(frame: &mut Frame, area: Rect, queues: &mut Queues, styles: &Styles, focused: bool) {
    let mut title = format!(" Queues ({})", queues.queues().len());
    match queues.problems().len() {
        0 => {}
        1 => title.push_str(" · 1 file not loaded"),
        n => title.push_str(&format!(" · {n} files not loaded")),
    }
    title.push(' ');
    let inner = styles.render_panel(frame, area, title, focused);
    let problems = problem_lines(queues, styles);
    let problem_rows = if problems.is_empty() {
        0
    } else {
        u16::try_from(problems.len() + 1)
            .unwrap_or(u16::MAX)
            .min((inner.height / 2).max(2))
    };
    let [top, bottom] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(problem_rows)]).areas(inner);
    if queues.queues().is_empty() {
        let lines = vec![
            Line::styled(EMPTY, styles.of(Role::Calm)),
            Line::raw(""),
            Line::styled(
                "A queue is a file of the catalogue (queues/NAME.yaml); a decision gate with \
                 `watch: {queue: …}` puts its waiting tasks on it.",
                styles.of(Role::Done),
            ),
            Line::raw(""),
            Line::styled("The same in the CLI:", styles.of(Role::Done)),
            Line::styled(format!("  {CLI_COMMAND}"), styles.of(Role::Calm)),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), top);
    } else {
        chooser::render(frame, top, queues.chooser_mut(), styles, focused);
    }
    if !problems.is_empty() {
        let mut lines = vec![Line::raw("")];
        lines.extend(problems);
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), bottom);
    }
}
