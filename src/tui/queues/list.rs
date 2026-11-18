//! The list of the owner's queues: one entry per queue with its number of
//! entries; the description says its mode and how many observers serve it.

use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use narsor_protocol::controller::ObserverQueueSummary;

use super::super::chooser::{self, ChooserItem};
use super::super::scroll::one_line;
use super::super::style::{Role, Styles};
use super::{Queues, CLI_COMMAND};

/// What the list says when the owner has no queue.
pub const EMPTY: &str = "No observer queues yet.";

/// `n entries`, with the singular for one.
pub fn entries_text(count: u32) -> String {
    match count {
        1 => "1 entry".to_string(),
        n => format!("{n} entries"),
    }
}

/// How a queue reads on the list: its name and its number of entries; the
/// rest in the description.
pub fn queue_item(queue: &ObserverQueueSummary) -> ChooserItem {
    let label = format!(
        "{} · {}",
        one_line(&queue.queue),
        entries_text(queue.entries)
    );
    let mode = queue
        .mode
        .as_deref()
        .map(one_line)
        .unwrap_or_else(|| "none (no observer serves it)".to_string());
    let lines = [
        format!("mode: {mode}"),
        format!("observers: {}", queue.observers),
        format!("entries: {}", queue.entries),
        String::new(),
        "Enter opens the queue: its observers and its entries.".to_string(),
    ];
    ChooserItem::new(label, lines.join("\n"))
}

/// Draws the list into `area`: a frame titled with how many queues there
/// are, and in it the list and the description, or the empty state with the
/// CLI hint.
pub fn render(frame: &mut Frame, area: Rect, queues: &mut Queues, styles: &Styles, focused: bool) {
    let mut title = format!(" Queues ({})", queues.queues().len());
    if queues.queues().is_empty() {
        title.push_str(" · a creates an observer");
    }
    title.push(' ');
    let inner = styles.render_panel(frame, area, title, focused);
    if queues.queues().is_empty() {
        let lines = vec![
            Line::styled(EMPTY, styles.of(Role::Calm)),
            Line::raw(""),
            Line::styled(
                "a creates an observer; the queue it serves shows here, and a decision gate \
                 with `watch: {queue: …}` puts its waiting tasks on it.",
                styles.of(Role::Done),
            ),
            Line::raw(""),
            Line::styled(
                "Secrets, the task limit and the workflow mapping are set in the CLI:",
                styles.of(Role::Done),
            ),
            Line::styled(format!("  {CLI_COMMAND} --help"), styles.of(Role::Calm)),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        return;
    }
    chooser::render(frame, inner, queues.chooser_mut(), styles, focused);
}
