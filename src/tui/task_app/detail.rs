//! The details of the selected task (U9): a two-line header made of the
//! workflow's `display`, the tabs, and what the active tab shows.
//!
//! ```text
//! NAR-12  Zmiana API zamówień                             implement v3
//! Jira NAR-12 ↗   PR: —   ● running · krok 3/5 · 14m
//!  Przebieg   Rozmowa   Zdarzenia   Dyrygent   Info          podąża
//! ▸● review_api   │ ● review_api · w toku · agent · 2m
//!  ✓ plan         │ …
//! ```
//!
//! # What it remembers
//!
//! [`TaskDetail`] is the state of one task's details — a tab's cursor, the
//! step the timeline follows, the transcript position, what was last read —
//! kept in [`super::DetailState`] under the task's id. Moving to another task
//! and back, or a refresh, finds it as it was left; that is requirement R18.
//!
//! # Reading
//!
//! [`TaskApp::sync_detail`] reads what the selected task's visible tab needs
//! and has not got: the timeline always (the header's `krok n/m` and the
//! following need it), the attempts of the conversation for *Przebieg* and
//! *Rozmowa*, the events for *Zdarzenia*. A task whose row moved to a newer
//! revision, or a list that was read again, makes the data due; nothing is
//! read twice for the same revision. Every answer goes through
//! [`TaskApp::on_detail_read`], so a test can hand over any answer — a relay
//! error included — without a server. A failed read stays in its tab; only an
//! unreachable server counts against the connection.
//!
//! # For the answer panel (U10)
//!
//! [`areas`] takes the height of the panel and returns where it goes; the tab
//! content gives up that many rows at the bottom. [`render`] calls it with no
//! panel today.

use std::num::NonZeroU16;

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::api::ClientError;
use crate::controller_protocol::{
    ExecutionEventsQuery, ExecutionEventsResponse, TaskSummary, TimelineResponse,
};
use crate::domain::display::{safe_link_url, DisplayLink, EMPTY_LINK_TEXT};
use crate::domain::ExecutionStatus;
use crate::engine::ConversationState;
use crate::orchestration::{QueueReason, TaskLifecycle};

use super::super::keymap::Action;
use super::super::style::{Role, Styles};
use super::list::{fit, lifecycle_word, one_line, state_icon, title_of};
use super::tabs::conversation::{self, transcript_action, AttemptRead, Attempts, Conversation};
use super::tabs::events::{self, Events};
use super::tabs::info::{self, Info};
use super::tabs::progress::{self, Progress};
use super::tabs::failure_text;
use super::{DetailTab, Focus, TaskApp};

/// Everything the details of one task remember.
#[derive(Debug, Clone)]
pub struct TaskDetail {
    progress: Progress,
    attempts: Attempts,
    conversation: Conversation,
    events: Events,
    info: Info,
}

impl TaskDetail {
    /// Details of `task_id` with nothing read.
    pub fn new(task_id: &str) -> Self {
        Self {
            progress: Progress::default(),
            attempts: Attempts::default(),
            conversation: Conversation::new(task_id),
            events: Events::default(),
            info: Info::default(),
        }
    }

    /// *Przebieg*.
    pub fn progress(&self) -> &Progress {
        &self.progress
    }

    /// *Rozmowa*.
    pub fn conversation(&self) -> &Conversation {
        &self.conversation
    }

    /// The attempts read so far.
    pub fn attempts(&self) -> &Attempts {
        &self.attempts
    }

    /// *Zdarzenia*.
    pub fn events(&self) -> &Events {
        &self.events
    }

    /// *Info*.
    pub fn info(&self) -> &Info {
        &self.info
    }

    /// Makes everything due for a new read, keeping what is shown.
    pub fn invalidate(&mut self) {
        self.progress.invalidate();
        self.attempts.invalidate();
        self.events.invalidate();
    }

    /// What a key does in `tab`. Cursors, scroll positions and modes live
    /// here and survive anything but the task being gone.
    pub fn on_action(&mut self, tab: DetailTab, action: Action) {
        match tab {
            DetailTab::Progress => self.on_progress_action(action),
            DetailTab::Conversation => {
                transcript_action(self.conversation.transcript_mut(), action);
            }
            DetailTab::Events => self.events.on_action(action),
            DetailTab::Info => self.info.on_action(action),
            DetailTab::Conductor => {}
        }
    }

    /// On the timeline `j` / `k` / `g` / `G` pick the step; the rest of the
    /// keys read the step's own conversation.
    fn on_progress_action(&mut self, action: Action) {
        match action {
            Action::Down => self.progress.step(1),
            Action::Up => self.progress.step(-1),
            Action::Top => self.progress.first(),
            Action::Bottom => self.progress.last(),
            Action::Follow => self.progress.toggle_follow(),
            other => {
                let attempt = self
                    .progress
                    .selected_position()
                    .and_then(|position| position.action_execution_id.clone());
                if let Some(attempt) = attempt.and_then(|id| self.attempts.get_mut(&id)) {
                    transcript_action(attempt.transcript_mut(), other);
                }
            }
        }
    }
}

/// An answer to one of the reads the details make.
#[derive(Debug)]
pub enum DetailRead {
    /// The timeline.
    Timeline(Result<TimelineResponse, ClientError>),
    /// One page of events after `after`.
    Events {
        /// The sequence the page continued after.
        after: u64,
        /// The answer.
        result: Result<ExecutionEventsResponse, ClientError>,
    },
    /// The conversation of an attempt, from entry `from`.
    Attempt {
        /// The attempt.
        attempt_id: String,
        /// The entry the read started from.
        from: u64,
        /// The answer.
        result: Result<AttemptRead, ClientError>,
    },
}

/// Where the details of a task can be read from at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// The task has (or had) an execution to read.
    Started,
    /// No execution yet: queued, or provisioning without a runner.
    NotStarted,
    /// Archived: the storage went away with the archiving.
    Archived,
}

/// Whether there is an execution to read for `task`.
pub fn readiness(task: &TaskSummary) -> Readiness {
    if task.lifecycle.is_archival() {
        Readiness::Archived
    } else if task.runner.is_none()
        && matches!(
            task.lifecycle,
            TaskLifecycle::Queued | TaskLifecycle::Provisioning
        )
    {
        Readiness::NotStarted
    } else {
        Readiness::Started
    }
}

/// What *Przebieg*, *Rozmowa* and *Zdarzenia* say when there is nothing to
/// show, or `None` when there is.
pub fn unstarted_message(task: &TaskSummary, queue_place: Option<usize>) -> Option<String> {
    match readiness(task) {
        Readiness::Started => None,
        Readiness::Archived => Some("storage usunięty przy archiwizacji".to_string()),
        Readiness::NotStarted => {
            let place = match (task.lifecycle, queue_place) {
                (TaskLifecycle::Queued, Some(place)) => format!("#{place} w kolejce"),
                (TaskLifecycle::Queued, None) => "w kolejce".to_string(),
                (other, _) => lifecycle_word(other).to_string(),
            };
            Some(format!("jeszcze nie wystartowało, {place}"))
        }
    }
}

/// `45s`, `4m`, `3h`, `2d`: the largest whole unit.
pub fn short_age(seconds: i64) -> String {
    let seconds = seconds.max(0);
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86399 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}

/// Whether the terminal named by the environment draws OSC 8 hyperlinks.
/// `var` reads one variable; passing it in keeps the environment out of this
/// module, so a test says what the terminal is.
pub fn hyperlinks_supported(var: impl Fn(&str) -> Option<String>) -> bool {
    let has = |name: &str| var(name).is_some_and(|value| !value.is_empty());
    if var("TERM").is_some_and(|term| term == "dumb" || term.starts_with("linux")) {
        return false;
    }
    if has("WT_SESSION") || has("KITTY_WINDOW_ID") || has("WEZTERM_EXECUTABLE") {
        return true;
    }
    if var("TERM_PROGRAM").is_some_and(|program| {
        matches!(program.as_str(), "iTerm.app" | "WezTerm" | "vscode" | "ghostty" | "Hyper")
    }) {
        return true;
    }
    if var("VTE_VERSION")
        .and_then(|version| version.parse::<u32>().ok())
        .is_some_and(|version| version >= 5000)
    {
        return true;
    }
    var("TERM").is_some_and(|term| {
        term.contains("kitty") || term.contains("foot") || term.contains("ghostty")
    })
}

/// Where the parts of the details go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Areas {
    /// The two header lines.
    pub header: Rect,
    /// The tab titles.
    pub tabs: Rect,
    /// What the tab shows.
    pub content: Rect,
    /// The answer panel at the bottom, when there is one.
    pub panel: Option<Rect>,
}

/// Splits the details `area`: the header, the tabs, the content and — when
/// `panel_height` is not zero — a panel at the bottom. The content never goes
/// below one row.
pub fn areas(area: Rect, panel_height: u16) -> Areas {
    let header = Rect::new(area.x, area.y, area.width, 2.min(area.height));
    let tabs = Rect::new(area.x, area.y + header.height, area.width, 1.min(area.height - header.height));
    let used = header.height + tabs.height;
    let remaining = area.height - used;
    let panel_height = panel_height.min(remaining.saturating_sub(1));
    let content = Rect::new(area.x, area.y + used, area.width, remaining - panel_height);
    let panel = (panel_height > 0).then(|| {
        Rect::new(area.x, content.y + content.height, area.width, panel_height)
    });
    Areas {
        header,
        tabs,
        content,
        panel,
    }
}

// ---------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------

impl TaskApp {
    /// Takes an answer of a read of `task_id`'s details in. `revision` is the
    /// revision of the row the read was made for. A failure stays in its tab;
    /// one that says the server is unreachable also counts against the
    /// connection, a `503` with a reason (the relay missing its deadline) does
    /// not.
    pub fn on_detail_read(&mut self, task_id: &str, revision: u64, read: DetailRead) {
        match read {
            DetailRead::Timeline(result) => {
                let result = self.settle(result);
                self.detail.view_mut(task_id).progress.apply(revision, result);
            }
            DetailRead::Events { after, result } => {
                let result = self.settle(result);
                self.detail.view_mut(task_id).events.apply(revision, after, result);
            }
            DetailRead::Attempt {
                attempt_id,
                from,
                result,
            } => {
                let result = self.settle(result);
                self.detail
                    .view_mut(task_id)
                    .attempts
                    .apply(&attempt_id, revision, from, result);
            }
        }
    }

    /// A failed read as the words of its tab. Only a server that cannot be
    /// reached counts against the connection: a `503` with a reason is the
    /// relay missing its deadline (the connection is fine), and a refusal is
    /// an answer.
    fn settle<T>(&mut self, result: Result<T, ClientError>) -> Result<T, String> {
        result.map_err(|error| {
            if matches!(error, ClientError::Unavailable(_) | ClientError::Server(_)) {
                self.note_transport(&error);
            }
            failure_text(&error)
        })
    }

    /// Reads what the selected task's visible tab needs and has not got. Cheap
    /// when nothing is due: no request is made. Call it after a key, an event
    /// and a refresh.
    pub async fn sync_detail(&mut self) {
        let Some(task) = self.list.selected() else {
            return;
        };
        if readiness(task) != Readiness::Started {
            return;
        }
        let id = task.task_id.to_string();
        let revision = task.revision;
        let client = self.client.clone();
        let tab = self.detail.tab();

        if self.detail.view_mut(&id).progress.needs_read(revision) {
            let result = client.control_timeline(&id).await;
            self.on_detail_read(&id, revision, DetailRead::Timeline(result));
        }
        match tab {
            DetailTab::Progress => {
                let attempt = self
                    .detail
                    .view_mut(&id)
                    .progress
                    .selected_position()
                    .and_then(recorded_attempt);
                if let Some(attempt) = attempt {
                    self.sync_attempt(&id, revision, &attempt).await;
                }
            }
            DetailTab::Conversation => self.sync_conversation(&id, revision).await,
            DetailTab::Events => self.sync_events(&id, revision).await,
            DetailTab::Conductor | DetailTab::Info => {}
        }
    }

    /// Reads one attempt's conversation when it is due. Returns whether a read
    /// was made.
    async fn sync_attempt(&mut self, id: &str, revision: u64, attempt_id: &str) -> bool {
        let view = self.detail.view_mut(id);
        if !view.attempts.needs_read(attempt_id, revision) {
            return false;
        }
        let from = view.attempts.from(attempt_id);
        let client = self.client.clone();
        let result = conversation::read_attempt(&client, id, attempt_id, from).await;
        self.on_detail_read(
            id,
            revision,
            DetailRead::Attempt {
                attempt_id: attempt_id.to_string(),
                from,
                result,
            },
        );
        true
    }

    async fn sync_conversation(&mut self, id: &str, revision: u64) {
        let attempts: Vec<String> = self
            .detail
            .view_mut(id)
            .progress
            .positions()
            .iter()
            .filter_map(recorded_attempt)
            .collect();
        let mut read = false;
        for attempt in &attempts {
            read |= self.sync_attempt(id, revision, attempt).await;
        }
        let view = self.detail.view_mut(id);
        if read || !view.conversation.is_built() {
            let positions = view.progress.positions().to_vec();
            view.conversation.rebuild(&positions, &view.attempts);
        }
    }

    async fn sync_events(&mut self, id: &str, revision: u64) {
        if !self.detail.view_mut(id).events.needs_read(revision) {
            return;
        }
        let client = self.client.clone();
        for _ in 0..events::MAX_PAGES {
            let after = self.detail.view_mut(id).events.after();
            let query = ExecutionEventsQuery {
                after,
                limit: Some(events::PAGE_LIMIT),
            };
            let result = client.control_execution_events(id, &query).await;
            let more = matches!(
                &result,
                Ok(ExecutionEventsResponse::Available { events, has_more: true, .. })
                    if !events.is_empty()
            );
            self.on_detail_read(id, revision, DetailRead::Events { after, result });
            if !more {
                break;
            }
        }
    }
}

/// The attempt of a position whose conversation can be read.
fn recorded_attempt(position: &crate::engine::Position) -> Option<String> {
    match (&position.conversation, &position.action_execution_id) {
        (ConversationState::Recorded, Some(id)) => Some(id.clone()),
        _ => None,
    }
}

// ---------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------

/// A link that may be made clickable: where it starts in the line, how wide it
/// is, and where it points.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LinkSpot {
    column: u16,
    width: u16,
    url: String,
}

/// Draws the details of the selected task into `area`.
pub fn render(frame: &mut Frame, area: Rect, app: &mut TaskApp, styles: &Styles, stale: bool) {
    let focused = app.focus() == Focus::Details;
    let hyperlinks = app.detail().hyperlinks();
    let Some(view) = app.detail_view() else {
        frame.render_widget(
            Paragraph::new(Line::styled("brak zaznaczonego zadania", styles.of(Role::Done))),
            area,
        );
        return;
    };
    let parts = areas(area, 0);

    let (lines, spots) = header(view.task, view.queue_place, &view.detail.progress, styles, stale, area.width);
    frame.render_widget(Paragraph::new(lines), parts.header);
    if hyperlinks {
        link_spots(frame.buffer_mut(), parts.header, &spots);
    }
    frame.render_widget(
        Paragraph::new(tab_bar(view.tab, &view.detail.progress, focused, styles)),
        parts.tabs,
    );

    let content = parts.content;
    let unstarted = unstarted_message(view.task, view.queue_place);
    match (view.tab, unstarted) {
        (DetailTab::Progress | DetailTab::Conversation | DetailTab::Events, Some(message)) => {
            frame.render_widget(
                Paragraph::new(Line::styled(message, styles.of(Role::Done))),
                content,
            );
        }
        (DetailTab::Progress, None) => progress::render(
            frame,
            content,
            styles,
            &mut view.detail.progress,
            &mut view.detail.attempts,
            stale,
            focused,
        ),
        (DetailTab::Conversation, None) => {
            conversation::render(frame, content, styles, &mut view.detail.conversation, stale);
        }
        (DetailTab::Events, None) => {
            events::render(frame, content, styles, &mut view.detail.events, stale);
        }
        (DetailTab::Conductor, _) => frame.render_widget(
            Paragraph::new(Line::styled(
                "Dyrygent: czat i decyzje pojawią się w kolejnym kroku",
                styles.of(Role::Done),
            )),
            content,
        ),
        (DetailTab::Info, _) => {
            info::render(frame, content, styles, &mut view.detail.info, view.task, stale);
        }
    }
}

/// The tab titles, the active one in the focus role while the details have
/// the keyboard.
fn tab_bar(active: DetailTab, progress: &Progress, focused: bool, styles: &Styles) -> Line<'static> {
    let mut spans = Vec::new();
    for tab in DetailTab::ALL {
        let style = if tab == active {
            let role = if focused { Role::Focus } else { Role::Calm };
            styles.of(role).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            styles.of(Role::Done)
        };
        spans.push(Span::styled(format!(" {} ", tab.title()), style));
        spans.push(Span::raw(" "));
    }
    if active == DetailTab::Progress && !progress.positions().is_empty() {
        let (text, role) = if progress.follows() {
            ("↧ podąża", Role::Live)
        } else {
            ("· ręcznie", Role::Done)
        };
        spans.push(Span::styled(text, styles.of(role)));
    }
    Line::from(spans)
}

/// The two header lines of `task`, and where its links are on the second.
fn header(
    task: &TaskSummary,
    queue_place: Option<usize>,
    progress: &Progress,
    styles: &Styles,
    stale: bool,
    width: u16,
) -> (Vec<Line<'static>>, Vec<LinkSpot>) {
    let grey = |role: Role| if stale { Role::Done } else { role };
    let width = usize::from(width);

    // Line 1: the title, what the workflow calls it, the workflow.
    let workflow = format!("{} v{}", one_line(&task.workflow_id), task.workflow_version);
    let title = title_of(task);
    let identity = task.task_id.to_string();
    let room = width.saturating_sub(workflow.chars().count() + 3);
    let mut first = vec![Span::styled(
        fit(&title, room),
        styles.of(grey(Role::Calm)).add_modifier(Modifier::BOLD),
    )];
    let mut used = first[0].width();
    if title != identity && used + 2 + identity.chars().count() <= room {
        first.push(Span::styled(format!("  {identity}"), styles.of(Role::Done)));
        used = first.iter().map(Span::width).sum();
    }
    let padding = width.saturating_sub(used + workflow.chars().count());
    first.push(Span::raw(" ".repeat(padding)));
    first.push(Span::styled(workflow, styles.of(Role::Done)));

    // Line 2: links, the state, the step or the queue, the time.
    let mut second: Vec<Span<'static>> = Vec::new();
    let mut spots = Vec::new();
    let mut column = 0usize;
    for link in &task.display.links {
        let (text, url) = link_text(link);
        let role = if url.is_some() { Role::Link } else { Role::Done };
        let span = Span::styled(text, styles.of(if stale { Role::Done } else { role }));
        if let Some(url) = url {
            if let (Ok(column), Ok(width)) = (u16::try_from(column), u16::try_from(span.width())) {
                spots.push(LinkSpot { column, width, url });
            }
        }
        column += span.width() + 3;
        second.push(span);
        second.push(Span::raw("   "));
    }
    let icon = state_icon(task);
    second.push(Span::styled(
        format!("{} {}", icon.glyph(), status_word(task)),
        styles.of(grey(icon.role())),
    ));
    for (text, role) in status_parts(task, queue_place, progress) {
        second.push(Span::styled(" · ", styles.of(Role::Done)));
        second.push(Span::styled(text, styles.of(grey(role))));
    }
    (vec![Line::from(first), Line::from(second)], spots)
}

/// The visible text of a link and, when it can be followed, its target.
/// A link without a value is `label: —`.
fn link_text(link: &DisplayLink) -> (String, Option<String>) {
    let label = one_line(&link.label);
    let text = one_line(&link.text);
    if text.trim().is_empty() || text == EMPTY_LINK_TEXT {
        return (format!("{label}: {EMPTY_LINK_TEXT}"), None);
    }
    match link.url.as_deref().and_then(safe_link_url) {
        Some(url) => (format!("{label} {text} ↗"), Some(url)),
        None => (format!("{label} {text}"), None),
    }
}

/// The lifecycle in a word, except that a runner that finished says how.
fn status_word(task: &TaskSummary) -> &'static str {
    match task.runner.as_ref().map(|runner| &runner.execution_status) {
        Some(ExecutionStatus::Completed) => "completed",
        Some(ExecutionStatus::Failed) => "failed",
        Some(ExecutionStatus::Cancelled) => "cancelled",
        _ => lifecycle_word(task.lifecycle),
    }
}

/// What follows the state on line 2: the queue, or the step, the time and a
/// controller that is not there.
fn status_parts(
    task: &TaskSummary,
    queue_place: Option<usize>,
    progress: &Progress,
) -> Vec<(String, Role)> {
    let mut parts = Vec::new();
    if task.lifecycle == TaskLifecycle::Queued {
        parts.push((queue_text(task, queue_place), Role::Calm));
        return parts;
    }
    if let Some((at, of)) = progress.step_count() {
        parts.push((format!("krok {at}/{of}"), Role::Calm));
    } else if let Some(step) = task
        .runner
        .as_ref()
        .and_then(|runner| runner.execution_cursor.current_state.as_deref())
        .map(one_line)
        .filter(|step| !step.trim().is_empty())
    {
        parts.push((format!("krok {step}"), Role::Calm));
    }
    if let Some(seconds) = elapsed(task) {
        parts.push((short_age(seconds), Role::Done));
    }
    if matches!(task.lifecycle, TaskLifecycle::Provisioning | TaskLifecycle::Running) {
        if let Some(offline) = controller_offline(task) {
            parts.push((offline, Role::Attention));
        }
    }
    parts
}

/// The whole life of a task so far, or what it took: it runs until the
/// runner's last word when the runner finished.
fn elapsed(task: &TaskSummary) -> Option<i64> {
    let finished = matches!(
        task.runner.as_ref().map(|runner| &runner.execution_status),
        Some(ExecutionStatus::Completed | ExecutionStatus::Failed | ExecutionStatus::Cancelled)
    );
    let until = match (&task.runner, finished) {
        (Some(runner), true) => runner.observed_at,
        _ => task.as_of,
    };
    Some((until - task.created_at).num_seconds())
}

/// `kontroler offline 4m` when no live controller holds the task's backend.
fn controller_offline(task: &TaskSummary) -> Option<String> {
    use crate::controller_protocol::Freshness;
    match &task.controller {
        Some(controller) if controller.freshness == Freshness::Live => None,
        Some(controller) => Some(match controller.last_heartbeat_at {
            Some(at) => format!("kontroler offline {}", short_age((task.as_of - at).num_seconds())),
            None => "kontroler offline".to_string(),
        }),
        None => Some("brak kontrolera".to_string()),
    }
}

/// What line 2 says of a queued task: its place and why it waits.
pub fn queue_text(task: &TaskSummary, queue_place: Option<usize>) -> String {
    let place = |fallback: Option<u32>| {
        queue_place
            .map(|place| format!("#{place}"))
            .or_else(|| fallback.map(|place| format!("#{place}")))
    };
    let joined = |place: Option<String>, text: String| match place {
        Some(place) => format!("{place} · {text}"),
        None => text,
    };
    match task.queue.as_ref() {
        Some(queue) => match &queue.reason {
            QueueReason::WaitingForCapacity {
                position,
                running,
                limit,
            } => joined(
                place(Some(*position)),
                format!("czeka na slot ({running}/{limit})"),
            ),
            QueueReason::ControllerOffline => format!(
                "kontroler offline {}",
                short_age((task.as_of - queue.since).num_seconds())
            ),
            QueueReason::Admitted => joined(place(None), "przyjęte, czeka na start".to_string()),
            QueueReason::BackendUnknown { .. } => "backend nieznany".to_string(),
        },
        None => joined(place(None), "w kolejce".to_string()),
    }
}

/// Makes the links at `spots` clickable by writing OSC 8 into the buffer.
///
/// Each link becomes one cell holding the escape sequences and the whole
/// visible text, told to occupy its true width ([`CellDiffOption::ForcedWidth`])
/// so the terminal backend neither counts the escape bytes as columns nor
/// draws over the cells the text spans. A link that does not fit on the line is
/// left as plain text.
fn link_spots(buffer: &mut Buffer, header: Rect, spots: &[LinkSpot]) {
    if header.height < 2 {
        return;
    }
    let y = header.y + 1;
    for spot in spots {
        let end = u32::from(spot.column) + u32::from(spot.width);
        let (Some(width), true) = (NonZeroU16::new(spot.width), end <= u32::from(header.width)) else {
            continue;
        };
        let x = header.x + spot.column;
        let text: String = (0..spot.width)
            .map(|offset| buffer[(x + offset, y)].symbol().to_string())
            .collect();
        let cell = &mut buffer[(x, y)];
        cell.set_symbol(&format!("\x1b]8;;{}\x1b\\{text}\x1b]8;;\x1b\\", spot.url));
        cell.set_diff_option(CellDiffOption::ForcedWidth(width));
    }
}
