//! One queue: its observers and its entries (R23), and the approval screen
//! that shows a script's text before the owner approves it (R32).
//!
//! ```text
//! ╭▸ Queue mr-review · on_episodes · 2 observers · 1 entry ─────────────╮
//! │╭ Akcje ───────────────────────────╮ ╭ Opis ─────────────────────────╮│
//! ││ │ 1 ✱ gitlab-mr · active         │ │ approved version: 2 · newest 3 ││
//! ││   2 github-pr · active           │ │ ! version 3 not approved       ││
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
    ObserverQueueView, ObserverScriptTextView, ObserverView, QueueEntryObserverView, QueueEntryView,
};

use super::super::chooser::{self, Chooser, ChooserItem};
use super::super::scroll::{one_line, sanitize};
use super::super::style::{Role, Styles};
use super::list::entries_text;
use super::CLI_HINT;

/// The mark of an observer whose newest script version is not the
/// approved one (R32, AE10).
pub const UNAPPROVED_MARK: &str = "✱";

/// One queue as the view shows it.
#[derive(Debug)]
pub struct QueueDetail {
    view: ObserverQueueView,
    observers: Chooser,
    entries_scroll: usize,
}

impl QueueDetail {
    /// The queue of `view`, on its first observer.
    pub fn new(view: ObserverQueueView) -> Self {
        let observers = Chooser::new(view.observers.iter().map(observer_item).collect());
        Self {
            view,
            observers,
            entries_scroll: 0,
        }
    }

    /// Takes a new read of the same queue in; the highlighted observer
    /// stays.
    pub fn set_view(&mut self, view: ObserverQueueView) {
        let kept = self
            .selected_observer()
            .map(|observer| observer.name.clone());
        let index = kept
            .and_then(|name| view.observers.iter().position(|o| o.name == name))
            .unwrap_or(0);
        self.observers =
            Chooser::new(view.observers.iter().map(observer_item).collect()).selecting(index);
        self.view = view;
    }

    /// What the server said of the queue.
    pub fn view(&self) -> &ObserverQueueView {
        &self.view
    }

    /// The list of observers.
    pub fn observers(&self) -> &Chooser {
        &self.observers
    }

    /// The list of observers, to move on it.
    pub fn observers_mut(&mut self) -> &mut Chooser {
        &mut self.observers
    }

    /// The highlighted observer.
    pub fn selected_observer(&self) -> Option<&ObserverView> {
        self.view.observers.get(self.observers.selected())
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

/// The schedule of an observer in words.
pub fn schedule_text(view: &ObserverView) -> String {
    match (&view.cron, view.interval_secs) {
        (Some(cron), _) => format!("cron {} (UTC)", one_line(cron)),
        (None, Some(seconds)) => format!("every {seconds} s"),
        (None, None) => "none".to_string(),
    }
}

/// The schedule as the edit form takes it: the seconds or the cron
/// expression.
pub fn schedule_value(view: &ObserverView) -> String {
    match (&view.cron, view.interval_secs) {
        (Some(cron), _) => cron.clone(),
        (None, Some(seconds)) => seconds.to_string(),
        (None, None) => String::new(),
    }
}

/// The approved and the newest version in words.
pub fn versions_text(view: &ObserverView) -> String {
    let approved = view
        .approved_version
        .map_or_else(|| "none".to_string(), |version| version.to_string());
    let latest = view
        .latest_version
        .map_or_else(|| "none".to_string(), |version| version.to_string());
    format!("approved version: {approved} · newest version: {latest}")
}

/// The run in progress or the last one, in words.
pub fn last_run_text(view: &ObserverView) -> String {
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

/// How an observer reads on the list: its name and state, marked when its
/// newest version is not approved; the rest in the description.
pub fn observer_item(view: &ObserverView) -> ChooserItem {
    let unapproved = view.unapproved_version();
    let mark = if unapproved.is_some() {
        format!("{UNAPPROVED_MARK} ")
    } else {
        String::new()
    };
    let label = format!("{mark}{} · {}", one_line(&view.name), one_line(&view.state));
    ChooserItem::new(label, observer_description(view, unapproved).join("\n"))
}

fn observer_description(view: &ObserverView, unapproved: Option<u32>) -> Vec<String> {
    let mut lines = vec![
        format!(
            "state: {} · mode: {} · source: {}",
            one_line(&view.state),
            one_line(&view.mode),
            one_line(&view.source)
        ),
        versions_text(view),
    ];
    if let Some(version) = unapproved {
        lines.push(format!(
            "{UNAPPROVED_MARK} version {version} not approved: the schedule does not run it; \
             v shows it and approves it"
        ));
    }
    lines.push(format!("schedule: {}", schedule_text(view)));
    if let Some(next) = view.next_run_at {
        lines.push(format!("next run: {}", time_text(next)));
    }
    lines.push(last_run_text(view));
    if let Some(error) = view.last_run.as_ref().and_then(|run| run.error.as_deref()) {
        lines.push(format!("error: {}", one_line(error)));
    }
    if view.consecutive_failures > 0 {
        lines.push(format!(
            "failed runs in a row: {}",
            view.consecutive_failures
        ));
    }
    if view.worker_missing {
        lines.push("no worker claims its runs".to_string());
    }
    if let Some(note) = view.note.as_deref().filter(|note| !note.trim().is_empty()) {
        lines.push(format!("note: {}", one_line(note)));
    }
    lines.push(String::new());
    lines.push("read only (set in the CLI):".to_string());
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
    lines.push(format!("task limit per run: {}", view.task_limit_per_run));
    let mapping = if view.mapping.is_empty() {
        "none".to_string()
    } else {
        view.mapping
            .iter()
            .map(|(input, field)| format!("{} ← {}", one_line(input), one_line(field)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match view.workflow_id.as_deref() {
        Some(workflow) => lines.push(format!(
            "workflow: {} · mapping: {mapping}",
            one_line(workflow)
        )),
        None => lines.push(format!("workflow mapping: {mapping}")),
    }
    lines.push(CLI_HINT.to_string());
    lines
}

/// The lines of one entry: the task, the step, the episode and the start;
/// its params; and per observer the processing state (R23).
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
    for observer in &entry.observers {
        let role = if observer.last_error.is_some() {
            Role::Problem
        } else {
            Role::Done
        };
        lines.push(Line::styled(
            format!("  {}", entry_observer_text(observer)),
            styles.of(role),
        ));
    }
    lines
}

/// How one observer processed one entry, in words.
pub fn entry_observer_text(observer: &QueueEntryObserverView) -> String {
    let mut parts = Vec::new();
    if observer.running {
        parts.push("running".to_string());
    }
    parts.push(match observer.last_run_at {
        Some(at) => format!("last run {}", time_text(at)),
        None => "no run yet".to_string(),
    });
    if let Some(error) = &observer.last_error {
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
    format!("{}: {}", one_line(&observer.observer), parts.join(" · "))
}

/// The title of a queue's frame: its name, mode and counts.
fn queue_title(view: &ObserverQueueView) -> String {
    let mode = view.mode.as_deref().map(one_line);
    let observers = match view.observers.len() {
        1 => "1 observer".to_string(),
        n => format!("{n} observers"),
    };
    let entries = entries_text(u32::try_from(view.entries.len()).unwrap_or(u32::MAX));
    match mode {
        Some(mode) => format!(
            " Queue {} · {mode} · {observers} · {entries} ",
            one_line(&view.queue)
        ),
        None => format!(
            " Queue {} · {observers} · {entries} ",
            one_line(&view.queue)
        ),
    }
}

/// Draws a queue into `area`: the observers with their description above,
/// the entries under them.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    detail: &mut QueueDetail,
    styles: &Styles,
    focused: bool,
) {
    let inner = styles.render_panel(frame, area, queue_title(&detail.view), focused);
    let body: Vec<Line> = detail
        .view
        .entries
        .iter()
        .flat_map(|entry| entry_lines(entry, styles))
        .collect();
    // The entries take what they need, up to a little under half; the
    // observers' description gets the rest.
    let wanted = u16::try_from(body.len().max(1) + 1).unwrap_or(u16::MAX);
    let entries_rows = wanted.min((inner.height * 9 / 20).max(4));
    let [top, bottom] =
        Layout::vertical([Constraint::Min(4), Constraint::Length(entries_rows)]).areas(inner);
    if detail.view.observers.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled("No observer serves this queue.", styles.of(Role::Calm)),
                Line::styled(
                    "a creates one; until then its entries wait for a person.",
                    styles.of(Role::Done),
                ),
            ])
            .wrap(Wrap { trim: false }),
            top,
        );
    } else {
        chooser::render(frame, top, &mut detail.observers, styles, focused);
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

/// The approval screen: the text of the version the owner is about to
/// approve, read before `y` (R32).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApproveScript {
    /// The observer, by name.
    pub observer: String,
    /// The version it runs now, if any.
    pub approved: Option<u32>,
    /// The version read.
    pub script: ObserverScriptTextView,
    scroll: usize,
}

impl ApproveScript {
    /// The screen for `script` of `observer`, which runs `approved` now.
    pub fn new(
        observer: impl Into<String>,
        approved: Option<u32>,
        script: ObserverScriptTextView,
    ) -> Self {
        Self {
            observer: observer.into(),
            approved,
            script,
            scroll: 0,
        }
    }

    /// The version `y` approves.
    pub fn version(&self) -> u32 {
        self.script.version
    }

    /// Scrolls the text by `delta` lines.
    pub fn scroll(&mut self, delta: isize) {
        self.scroll = self.scroll.saturating_add_signed(delta);
    }
}

/// Draws the approval screen into `area`.
pub fn render_approve(
    frame: &mut Frame,
    area: Rect,
    approve: &mut ApproveScript,
    styles: &Styles,
    focused: bool,
) {
    let title = format!(
        " Approve {} version {} ",
        one_line(&approve.observer),
        approve.script.version
    );
    let inner = styles.render_panel(frame, area, title, focused);
    let now = approve.approved.map_or_else(
        || "none".to_string(),
        |version| format!("version {version}"),
    );
    let head = vec![
        Line::styled(
            format!(
                "The schedule runs only the approved version (now: {now}). Read the script; \
                 y approves version {}, Esc goes back without approving.",
                approve.script.version
            ),
            styles.of(Role::Attention),
        ),
        Line::styled(
            format!(
                "saved {} · {}",
                time_text(approve.script.created_at),
                one_line(&approve.script.sha256)
            ),
            styles.of(Role::Done),
        ),
    ];
    let head_rows = u16::try_from(head.len() + 1).unwrap_or(3);
    let [top, body] = Layout::vertical([
        Constraint::Length(head_rows.min(inner.height)),
        Constraint::Min(1),
    ])
    .areas(inner);
    frame.render_widget(Paragraph::new(head).wrap(Wrap { trim: false }), top);
    let text = sanitize(&approve.script.script);
    let lines: Vec<&str> = text.lines().collect();
    let room = usize::from(body.height);
    approve.scroll = approve.scroll.min(lines.len().saturating_sub(room));
    let shown: Vec<Line> = lines
        .iter()
        .skip(approve.scroll)
        .map(|line| Line::styled(line.to_string(), styles.of(Role::Calm)))
        .collect();
    frame.render_widget(Paragraph::new(shown), body);
}
