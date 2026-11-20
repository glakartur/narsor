//! The Historia (U13, origin §3): `H` shows the tasks that were archived,
//! newest first, in the layout of the task list and its details.
//!
//! ```text
//! │ Historia            │ NAR-9  Poprawka raportów                  fix-bug v1 │
//! │ ✓ NAR-9    archiwum │ ✓ done · zarchiwizowane                              │
//! │ ✓ NAR-7    archiwum │ razem $1.20 · ✨ $1.20                                │
//! │                     │  Przebieg   Rozmowa   Zdarzenia   Dyrygent   Info    │
//! │                     │ ✓ ✨ plan #1     $0.80                                │
//! │                     │      opus  claude                                    │
//! │                     │ ✓ ✨ review #1   $0.40                                │
//! │                     │      sonnet  claude                                  │
//! ```
//!
//! # Read only
//!
//! The tabs are the ones of the details of a task, over the same state
//! ([`TaskDetail`]); nothing here sends anything but reads. An archived task
//! is its [`TaskSummary`] and the run the archive kept: selecting an entry
//! reads its timeline once ([`TaskApp::start_history_read`]) into that
//! entry's own details, so the header has the total cost and *Przebieg* the
//! header row of every step. The steps cannot be picked or opened: their
//! conversations went away with the storage, as did the events and the
//! conductor, so *Rozmowa*, *Zdarzenia* and *Dyrygent* say so. A failed read
//! says why on *Przebieg*, and the next selection of the entry reads again.
//! *Info* shows what the control plane kept. `Esc` goes back to the list of
//! tasks.

use std::collections::BTreeMap;

use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::Frame;

use crate::api::ClientError;
use crate::controller_protocol::{TaskListResponse, TaskSummary, TimelineResponse};

use super::super::keymap::{self, Action, Context, Stack};
use super::super::selection::{render_bracketed_list, PREFIX_WIDTH};
use super::super::style::{Role, Styles};
use super::background::{DetailJob, JobKey, OpeningKind, Outcome};
use super::detail::{
    edge_layout, fit_cells, render_view, title_is_cut, KeysAt, TaskDetail, ViewOptions,
};
use super::list::{history_row_text, title_of, RowText, StateIcon};
use super::notify::Severity;
use super::{detail_keys, DetailTab, DetailView, Focus, Request, StepFocus, TaskApp, View};

/// The key an archived task's details are kept under.
fn key_of(task: &TaskSummary) -> String {
    task.task_id.to_string()
}

/// What the row of an archived task shows.
fn row_of(task: &TaskSummary) -> RowText {
    RowText {
        ticket: None,
        id: String::new(),
        icon: StateIcon::Archived,
        title: title_of(task),
        since: None,
        label: "archiwum".to_string(),
        queue_entry: None,
    }
}

/// When an archived task ended or, failing that, was archived: what newest
/// first is by.
fn when(task: &TaskSummary) -> chrono::DateTime<chrono::Utc> {
    task.archive
        .as_ref()
        .map(|archive| archive.completed_at.unwrap_or(archive.requested_at))
        .unwrap_or(task.as_of)
}

/// The history: what is on it, where the cursor is, what the tabs hold.
#[derive(Debug)]
pub struct History {
    entries: Vec<TaskSummary>,
    selected: usize,
    tab: DetailTab,
    details: BTreeMap<String, TaskDetail>,
    /// The first row of the list in view, kept between frames so that a new
    /// read does not scroll the list.
    top: usize,
}

impl History {
    /// A history of `archived` tasks, newest first.
    pub fn new(archived: Vec<TaskSummary>) -> Self {
        let mut history = Self {
            entries: Vec::new(),
            selected: 0,
            tab: DetailTab::Progress,
            details: BTreeMap::new(),
            top: 0,
        };
        history.set_entries(archived);
        history
    }

    fn set_entries(&mut self, mut entries: Vec<TaskSummary>) {
        // Newest first; the sort is stable, so equal moments keep the order
        // the server gave.
        entries.sort_by_key(|task| std::cmp::Reverse(when(task)));
        let kept = self.selected_entry().map(key_of);
        self.entries = entries;
        self.selected = kept
            .as_ref()
            .and_then(|key| self.entries.iter().position(|task| key_of(task) == *key))
            .unwrap_or(0)
            .min(self.entries.len().saturating_sub(1));
        self.details
            .retain(|key, _| self.entries.iter().any(|task| key_of(task) == *key));
        for detail in self.details.values_mut() {
            detail.invalidate();
        }
    }

    /// Every entry, newest first.
    pub fn entries(&self) -> &[TaskSummary] {
        &self.entries
    }

    /// The rows, as the list shows them.
    pub fn rows(&self) -> Vec<RowText> {
        self.entries.iter().map(row_of).collect()
    }

    /// The index of the selected entry.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The selected entry.
    pub fn selected_entry(&self) -> Option<&TaskSummary> {
        self.entries.get(self.selected)
    }

    /// The tab shown.
    pub fn tab(&self) -> DetailTab {
        self.tab
    }

    /// What the details of the selected entry remember, once shown or read.
    pub fn selected_detail(&self) -> Option<&TaskDetail> {
        let key = key_of(self.selected_entry()?);
        self.details.get(&key)
    }

    fn step(&mut self, by: isize) {
        self.select(self.selected.saturating_add_signed(by));
    }

    /// Selects the entry at `index` (the last one past the end). An entry
    /// whose last read failed reads again once it is selected anew.
    fn select(&mut self, index: usize) {
        let index = index.min(self.entries.len().saturating_sub(1));
        if index == self.selected {
            return;
        }
        self.selected = index;
        if let Some(detail) = self.selected_detail_mut() {
            if detail.progress.failure().is_some() {
                detail.progress.invalidate();
            }
        }
    }

    fn on_detail_action(&mut self, action: Action) {
        let tab = self.tab;
        if let Some(detail) = self.selected_detail_mut() {
            detail.on_action(tab, StepFocus::Axis, action);
        }
    }

    /// What the details of the selected entry remember, created empty when
    /// there is nothing yet.
    fn selected_detail_mut(&mut self) -> Option<&mut TaskDetail> {
        let key = key_of(self.selected_entry()?);
        Some(
            self.details
                .entry(key.clone())
                .or_insert_with(|| TaskDetail::new(&key)),
        )
    }

    /// `[` / `]`: the tab `by` places along.
    fn shift_tab(&mut self, by: isize) {
        self.tab = self.tab.shifted(by);
    }
}

// ---------------------------------------------------------------------
// The application's part
// ---------------------------------------------------------------------

impl TaskApp {
    /// The history, while it is open.
    pub fn history(&self) -> Option<&History> {
        match &self.view {
            View::History(history) => Some(history),
            _ => None,
        }
    }

    /// The read `H` started is back: opens the history over it.
    pub(super) fn on_history_read(&mut self, archived: Result<TaskListResponse, ClientError>) {
        match archived {
            Ok(archived) => self.open_history(archived),
            Err(error) => {
                self.note_transport(&error);
                self.notify(
                    Severity::Error,
                    format!(
                        "historia: nie udało się wczytać zarchiwizowanych zadań ({})",
                        super::failure_reason(&error)
                    ),
                );
            }
        }
    }

    /// Opens the history over what was read.
    pub fn open_history(&mut self, archived: TaskListResponse) {
        if let View::History(history) = &mut self.view {
            history.set_entries(archived.tasks);
            return;
        }
        self.enter_view(
            View::History(Box::new(History::new(archived.tasks))),
            Focus::List,
        );
    }

    /// The keys that apply while the history is open.
    pub(super) fn history_context(&self) -> Option<Stack> {
        match &self.view {
            View::History(_) if self.focus == Focus::List => Some(Context::History.into()),
            View::History(history) => Some(detail_keys(
                Context::HistoryDetails,
                history.tab,
                StepFocus::Axis,
            )),
            _ => None,
        }
    }

    /// A key while the history is open.
    pub(super) fn on_history_key(&mut self, key: crossterm::event::KeyEvent) -> Request {
        let Some(keys) = self.history_context() else {
            return Request::None;
        };
        let Some(action) = keymap::lookup(keys, &key) else {
            return Request::None;
        };
        let context = keys.base;
        match action {
            Action::Help => self.help_open = true,
            Action::Quit => self.on_quit(),
            Action::Back if context == Context::History => self.close_view(),
            Action::Back => self.focus = Focus::List,
            Action::SwitchFocus => {
                self.focus = if self.focus == Focus::List {
                    Focus::Details
                } else {
                    Focus::List
                };
            }
            Action::Open => self.focus = Focus::Details,
            Action::ChooseHarness => return Request::OpenHarness,
            Action::PrevTab => self.with_history(|history| history.shift_tab(-1)),
            Action::NextTab => self.with_history(|history| history.shift_tab(1)),
            Action::Down | Action::Up | Action::Top | Action::Bottom
                if context == Context::History =>
            {
                if let View::History(history) = &mut self.view {
                    match action {
                        Action::Down => history.step(1),
                        Action::Up => history.step(-1),
                        Action::Top => history.select(0),
                        _ => history.select(usize::MAX),
                    }
                }
            }
            other => {
                if let View::History(history) = &mut self.view {
                    history.on_detail_action(other);
                }
            }
        }
        Request::None
    }

    /// Starts the read of the run the selected entry's archive kept, when
    /// that entry has not got it yet. The answer goes to the entry it was
    /// read for, wherever the selection is by then.
    pub(super) fn start_history_read(&mut self) {
        let View::History(history) = &mut self.view else {
            return;
        };
        let Some(task) = history.selected_entry() else {
            return;
        };
        let (task_id, revision) = (key_of(task), task.revision);
        let Some(detail) = history.selected_detail_mut() else {
            return;
        };
        if !detail.progress.needs_read(revision) {
            return;
        }
        let client = self.client.clone();
        self.background.start(
            JobKey::Detail(task_id.clone(), DetailJob::ArchivedTimeline),
            async move {
                let result = client.control_timeline(&task_id).await;
                Outcome::new(move |app: &mut TaskApp| app.on_history_timeline(&task_id, result))
            },
        );
    }

    /// Takes the read of an archived task's timeline into its history entry.
    /// An entry that left the history, or a history that closed, drops it.
    pub fn on_history_timeline(
        &mut self,
        task_id: &str,
        result: Result<TimelineResponse, ClientError>,
    ) {
        let result = self.settle(result);
        let View::History(history) = &mut self.view else {
            return;
        };
        let Some(revision) = history
            .entries
            .iter()
            .find(|task| key_of(task) == task_id)
            .map(|task| task.revision)
        else {
            return;
        };
        let progress = &mut history
            .details
            .entry(task_id.to_string())
            .or_insert_with(|| TaskDetail::new(task_id))
            .progress;
        progress.apply(revision, result);
        progress.drop_selection();
    }

    /// Does `change` to the history, while it is open.
    fn with_history(&mut self, change: impl FnOnce(&mut History)) {
        if let View::History(history) = &mut self.view {
            change(history);
        }
    }

    /// `Esc` from a view that has its own screen: back to the list of tasks.
    pub(super) fn close_view(&mut self) {
        self.leave_view();
    }

    /// Starts the read that brings the open history up to date.
    pub(super) fn reload_history(&mut self) {
        let client = self.client.clone();
        self.background
            .start(JobKey::Reload(OpeningKind::History), async move {
                let archived = client.control_tasks_archived(true).await;
                Outcome::new(move |app: &mut TaskApp| {
                    if let (Ok(archived), true) = (archived, app.history().is_some()) {
                        app.open_history(archived);
                    }
                })
            });
    }
}

// ---------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------

/// Draws the list of the history into `area`, bracketing the selected entry
/// ([`render_bracketed_list`]); the bracket is in the focus role while
/// `focused`.
pub fn render_list(
    frame: &mut Frame,
    area: Rect,
    history: &mut History,
    styles: &Styles,
    focused: bool,
    stale: bool,
) {
    let inner = styles.render_panel(frame, area, " Historia ", focused);
    let rows = history.rows();
    let width = usize::from(inner.width.saturating_sub(PREFIX_WIDTH));
    let items: Vec<Vec<Line>> = rows
        .iter()
        .map(|row| history_row_text(row, width, styles, stale))
        .collect();
    let selected = (!rows.is_empty()).then_some(history.selected);
    render_bracketed_list(
        frame,
        inner,
        styles,
        &items,
        selected,
        focused,
        &mut history.top,
    );
    if rows.is_empty() {
        let text = "historia jest pusta";
        frame.render_widget(
            ratatui::widgets::Paragraph::new(Line::styled(
                format!(
                    " {}",
                    fit_cells(text, usize::from(inner.width).saturating_sub(2))
                ),
                styles.of(Role::Done),
            )),
            Rect::new(inner.x, inner.y, inner.width, 1.min(inner.height)),
        );
    }
}

/// How the details of a history entry are drawn.
#[derive(Clone, Copy)]
pub struct DetailsLook {
    pub focused: bool,
    pub stale: bool,
    pub hyperlinks: bool,
    pub glyph: &'static str,
}

/// Draws the details of the selected entry into `area`: the header and the
/// tabs of a task, with nothing to answer.
pub fn render_details(
    frame: &mut Frame,
    area: Rect,
    history: &mut History,
    styles: &Styles,
    look: DetailsLook,
) {
    let DetailsLook {
        focused,
        stale,
        hyperlinks,
        glyph,
    } = look;
    let tab = history.tab;
    let keys = if focused {
        KeysAt::Content(StepFocus::Axis)
    } else {
        KeysAt::Elsewhere
    };
    let Some(entry) = history.entries.get(history.selected) else {
        let inner = styles.render_panel(frame, area, " Historia ", focused);
        frame.render_widget(
            ratatui::widgets::Paragraph::new(Line::styled(
                "brak zaznaczonego wpisu historii",
                styles.of(Role::Done),
            )),
            inner,
        );
        return;
    };
    let key = key_of(entry);
    let task = entry;
    let edge = edge_layout(task, area.width, Styles::PILL_OVERHEAD, focused, None);
    let inner = styles.render_panel_edge(frame, area, &edge, focused, stale);
    let detail = history
        .details
        .entry(key.clone())
        .or_insert_with(|| TaskDetail::new(&key));
    let view = DetailView {
        task,
        queue_place: None,
        tab,
        detail,
        deferred: false,
        details_opened: true,
    };
    let options = ViewOptions {
        frame_focused: focused,
        stale,
        keys,
        hyperlinks,
        glyph,
        deferred: false,
        details_opened: true,
        loading: false,
        title_cut: title_is_cut(task, &edge),
        closed_run: true,
    };
    render_view(frame, inner, view, styles, options, None);
}
