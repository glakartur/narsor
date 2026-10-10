//! The Historia (U13, origin §3): `H` shows what is no longer in the queue —
//! tasks that were archived and executions from before the task scheduler —
//! newest first, in the layout of the task list and its details.
//!
//! ```text
//! │ Historia            │ NAR-9  Poprawka raportów                  fix-bug v1 │
//! │ ✓ NAR-9    archiwum │ ✓ done · zarchiwizowane                              │
//! │ ✗ fix-bug  failed   │  Przebieg   Rozmowa   Zdarzenia   Dyrygent   Info    │
//! │ ✓ run-2    completed│ storage usunięty przy archiwizacji                   │
//! ```
//!
//! # Read only
//!
//! The tabs are the ones of the details of a task, over the same state
//! ([`TaskDetail`]); nothing here sends anything but one thing: `w` recovers an
//! old execution that can be recovered (`recoverable`), and says why when it
//! cannot. `Esc` goes back to the list of tasks.
//!
//! * An **archived task** is its [`TaskSummary`]: its storage went away with
//!   the archiving, so *Przebieg*, *Rozmowa*, *Zdarzenia* and *Dyrygent* say
//!   so, and *Info* shows what the control plane kept.
//! * An **old execution** has a summary made for it
//!   ([`LegacyExecution`]): the same header and tabs, with the data read from
//!   `/control/v1/executions/:id/…` instead of the task's own routes. Its
//!   conductor is shown for reading only — there is no runner to ask.
//!
//! What is read for the selected entry is [`TaskApp::sync_history`], which the
//! loop calls through [`TaskApp::sync_detail`] like the details of a task.

use std::collections::BTreeMap;
use std::time::Instant;

use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{List, ListItem, ListState};
use ratatui::Frame;
use serde_json::json;

use crate::api::ClientError;
use crate::conductor::{ChatEntry, ConductorView};
use crate::controller_protocol::{
    ConductorReadResponse, ExecutionEventsResponse, ExecutionEventsSource, Freshness,
    LegacyExecutionListResponse, LegacyExecutionView, TaskListResponse, TaskSummary,
    TimelineResponse,
};
use crate::domain::{Event, ExecutionStatus, TaskId};
use crate::engine::Timeline;

use super::super::keymap::{self, Action, Context};
use super::super::scroll::one_line;
use super::super::style::{Role, Styles};
use super::detail::TaskDetail;
use super::detail::{panel_title, recorded_attempt, render_view, ViewOptions};
use super::list::{fit, row_text, title_of, Attention, RowText, StateIcon};
use super::notify::Severity;
use super::tabs::conversation::{read_history_attempt, AttemptRead};
use super::tabs::events;
use super::{DetailTab, DetailView, Focus, Request, TaskApp, View};

/// Pages of an old conductor's chat read in one go.
const CHAT_PAGES: usize = 10;

/// An execution from before the task scheduler, as an entry of the history.
#[derive(Debug, Clone)]
pub struct LegacyExecution {
    /// What the server said of it.
    pub view: LegacyExecutionView,
    /// The summary the details are drawn from: the header, *Info*.
    summary: TaskSummary,
}

/// One row of the history.
#[derive(Debug, Clone)]
pub enum HistoryEntry {
    /// A task that was archived.
    Archived(Box<TaskSummary>),
    /// An execution from before the task scheduler.
    Legacy(Box<LegacyExecution>),
}

impl HistoryEntry {
    /// The key the entry's details are kept under.
    pub fn key(&self) -> String {
        match self {
            Self::Archived(task) => task.task_id.to_string(),
            Self::Legacy(legacy) => format!("execution:{}", legacy.view.id),
        }
    }

    /// The summary the header and the tabs are drawn from.
    pub fn summary(&self) -> &TaskSummary {
        match self {
            Self::Archived(task) => task,
            Self::Legacy(legacy) => &legacy.summary,
        }
    }

    /// What the row shows.
    pub fn row(&self) -> RowText {
        match self {
            Self::Archived(task) => RowText {
                attention: Attention::None,
                icon: StateIcon::Done,
                title: title_of(task),
                label: "archiwum".to_string(),
            },
            Self::Legacy(legacy) => RowText {
                attention: Attention::None,
                icon: legacy_icon(&legacy.view.status),
                title: legacy_title(&legacy.view),
                label: legacy.view.status.as_str().to_string(),
            },
        }
    }

    /// When it ended or, failing that, began: what newest first is by.
    fn when(&self) -> chrono::DateTime<chrono::Utc> {
        match self {
            Self::Archived(task) => task
                .archive
                .as_ref()
                .map(|archive| archive.completed_at.unwrap_or(archive.requested_at))
                .unwrap_or(task.as_of),
            Self::Legacy(legacy) => legacy.view.completed_at.unwrap_or(legacy.view.started_at),
        }
    }

    /// Why the entry cannot be recovered, or the id to recover.
    pub fn recoverable(&self) -> Result<&str, String> {
        match self {
            Self::Archived(_) => Err(
                "zadanie jest zarchiwizowane: storage usunięty przy archiwizacji, nie ma czego wznawiać"
                    .to_string(),
            ),
            Self::Legacy(legacy) if legacy.view.recoverable => Ok(&legacy.view.id),
            Self::Legacy(legacy) => Err(legacy
                .view
                .not_recoverable_reason
                .as_deref()
                .map(one_line)
                .filter(|reason| !reason.trim().is_empty())
                .unwrap_or_else(|| "serwer nie podał powodu".to_string())),
        }
    }
}

fn legacy_icon(status: &ExecutionStatus) -> StateIcon {
    match status {
        ExecutionStatus::Completed => StateIcon::Done,
        ExecutionStatus::Failed | ExecutionStatus::Cancelled | ExecutionStatus::Unknown => {
            StateIcon::Failed
        }
        ExecutionStatus::Paused
        | ExecutionStatus::InputNeeded
        | ExecutionStatus::WaitingForEvent => StateIcon::Paused,
        ExecutionStatus::Running => StateIcon::Running,
    }
}

fn legacy_title(view: &LegacyExecutionView) -> String {
    format!("{} {}", one_line(&view.workflow_id), one_line(&view.id))
}

/// The summary an old execution is drawn from: a task that ran and has a
/// runner's last word, which is its own status.
fn legacy_summary(view: &LegacyExecutionView) -> TaskSummary {
    let id = TaskId::parse(view.id.clone())
        .or_else(|_| TaskId::parse("execution"))
        .expect("a plain word is an identifier");
    let at = view.completed_at.unwrap_or(view.started_at);
    let lifecycle = if matches!(
        view.status,
        ExecutionStatus::Failed | ExecutionStatus::Cancelled | ExecutionStatus::Unknown
    ) {
        "interrupted"
    } else {
        "running"
    };
    let summary = json!({
        "task_id": id,
        "backend_id": "local",
        "lifecycle": lifecycle,
        "generation": 1,
        "revision": 1,
        "rank": 1,
        "created_at": view.started_at,
        "queue_sequence": 0,
        "workflow_id": view.workflow_id,
        "workflow_version": view.workflow_version,
        "launch_digest": "",
        "environment_identity": "",
        "archive": null,
        "observation": null,
        "runner": {
            "incarnation": "execution",
            "execution_status": view.status,
            "execution_cursor": {
                "execution_id": view.id,
                "current_state": view.current_state,
                "action_execution_id": null,
            },
            "event_cursor": 0,
            "pending_input": null,
            "safe_point": false,
            "observed_at": at,
            "received_at": at,
            "last_contact_at": at,
            "freshness": "live",
        },
        // Nobody is missing: an execution that is over needs no controller,
        // and the header must not say it has none.
        "controller": {
            "controller_id": "local",
            "lease_epoch": 1,
            "lease_expires_at": at,
            "last_heartbeat_at": at,
            "freshness": "live",
        },
        "inputs": [],
        "display": { "title": legacy_title(view), "links": [] },
        "as_of": at,
    });
    serde_json::from_value(summary).expect("the summary of an old execution")
}

/// The history: what is on it, where the cursor is, what the tabs hold.
#[derive(Debug)]
pub struct History {
    entries: Vec<HistoryEntry>,
    selected: usize,
    tab: DetailTab,
    /// Counted up when the history is read again, so what the tabs hold is
    /// due for a new read.
    revision: u64,
    details: BTreeMap<String, TaskDetail>,
    /// Why the old executions could not be listed, when they could not.
    note: Option<String>,
    view: ListState,
}

impl History {
    /// A history of `archived` tasks and `legacy` executions, newest first.
    pub fn new(
        archived: Vec<TaskSummary>,
        legacy: Vec<LegacyExecutionView>,
        note: Option<String>,
    ) -> Self {
        let mut history = Self {
            entries: Vec::new(),
            selected: 0,
            tab: DetailTab::Progress,
            revision: 1,
            details: BTreeMap::new(),
            note,
            view: ListState::default(),
        };
        history.set_entries(archived, legacy);
        history
    }

    fn set_entries(&mut self, archived: Vec<TaskSummary>, legacy: Vec<LegacyExecutionView>) {
        let mut entries: Vec<HistoryEntry> = archived
            .into_iter()
            .map(|task| HistoryEntry::Archived(Box::new(task)))
            .chain(legacy.into_iter().map(|view| {
                let summary = legacy_summary(&view);
                HistoryEntry::Legacy(Box::new(LegacyExecution { view, summary }))
            }))
            .collect();
        // Newest first; the sort is stable, so equal moments keep the order
        // the server gave.
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.when()));
        let kept = self.selected_entry().map(HistoryEntry::key);
        self.entries = entries;
        self.selected = kept
            .and_then(|key| self.entries.iter().position(|entry| entry.key() == key))
            .unwrap_or(0)
            .min(self.entries.len().saturating_sub(1));
        self.revision += 1;
        self.details
            .retain(|key, _| self.entries.iter().any(|entry| entry.key() == *key));
        for detail in self.details.values_mut() {
            detail.invalidate();
        }
    }

    /// Every entry, newest first.
    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    /// The rows, as the list shows them.
    pub fn rows(&self) -> Vec<RowText> {
        self.entries.iter().map(HistoryEntry::row).collect()
    }

    /// The index of the selected entry.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The selected entry.
    pub fn selected_entry(&self) -> Option<&HistoryEntry> {
        self.entries.get(self.selected)
    }

    /// The tab shown.
    pub fn tab(&self) -> DetailTab {
        self.tab
    }

    /// Why the old executions are not listed, when they could not be.
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    fn step(&mut self, by: isize) {
        let last = self.entries.len().saturating_sub(1);
        self.selected = self.selected.saturating_add_signed(by).min(last);
    }

    fn on_detail_action(&mut self, action: Action) {
        let Some(key) = self.selected_entry().map(HistoryEntry::key) else {
            return;
        };
        let tab = self.tab;
        self.details
            .entry(key.clone())
            .or_insert_with(|| TaskDetail::new(&key))
            .on_action(tab, action);
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

    /// `H`: reads the archived tasks and the old executions, then opens the
    /// history. Without the old executions (a server that has none, or that
    /// cannot be asked) the archived tasks are shown with the reason.
    pub(super) async fn read_and_open_history(&mut self) {
        let archived = self.client.control_tasks_archived(true).await;
        let legacy = self.client.control_history().await;
        match archived {
            Ok(archived) => self.open_history(archived, legacy),
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
    pub fn open_history(
        &mut self,
        archived: TaskListResponse,
        legacy: Result<LegacyExecutionListResponse, ClientError>,
    ) {
        let (legacy, note) = match legacy {
            Ok(listing) => (listing.executions, None),
            Err(error) => (
                Vec::new(),
                Some(format!(
                    "dawne executions niedostępne: {}",
                    super::failure_reason(&error)
                )),
            ),
        };
        if let Some(note) = &note {
            self.notify(Severity::Warning, note.clone());
        }
        if let View::History(history) = &mut self.view {
            history.set_entries(archived.tasks, legacy);
            history.note = note;
            return;
        }
        self.enter_view(
            View::History(Box::new(History::new(archived.tasks, legacy, note))),
            Focus::List,
        );
    }

    /// The context whose keys apply while the history is open.
    pub(super) fn history_context(&self) -> Option<Context> {
        match &self.view {
            View::History(_) if self.focus == Focus::List => Some(Context::History),
            View::History(_) => Some(Context::HistoryDetails),
            _ => None,
        }
    }

    /// A key while the history is open.
    pub(super) fn on_history_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        confirming_quit: bool,
    ) -> Request {
        let Some(context) = self.history_context() else {
            return Request::None;
        };
        let Some(action) = keymap::lookup(context, &key) else {
            return Request::None;
        };
        match action {
            Action::Help => self.help_open = true,
            Action::Quit => self.on_quit(confirming_quit),
            Action::Back if context == Context::History => self.close_view(),
            Action::Back => self.focus = Focus::List,
            Action::Open => self.focus = Focus::Details,
            Action::Recover => return self.recover_selected_entry(),
            Action::ChooseHarness => return Request::OpenHarness,
            Action::PrevTab | Action::NextTab => {
                if let View::History(history) = &mut self.view {
                    let by = if action == Action::PrevTab { -1 } else { 1 };
                    history.tab = history.tab.shifted(by);
                }
            }
            Action::Down | Action::Up | Action::Top | Action::Bottom
                if context == Context::History =>
            {
                if let View::History(history) = &mut self.view {
                    match action {
                        Action::Down => history.step(1),
                        Action::Up => history.step(-1),
                        Action::Top => history.selected = 0,
                        _ => history.selected = history.entries.len().saturating_sub(1),
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

    /// `Esc` from a view that has its own screen: back to the list of tasks.
    pub(super) fn close_view(&mut self) {
        self.leave_view();
    }

    /// `w`: recovers the selected old execution when it can be, otherwise says
    /// why on the bar.
    fn recover_selected_entry(&mut self) -> Request {
        let View::History(history) = &self.view else {
            return Request::None;
        };
        let Some(entry) = history.selected_entry() else {
            return Request::None;
        };
        let title = one_line(&entry.row().title);
        match entry.recoverable() {
            Err(reason) => {
                self.notify(Severity::Warning, format!("wznowienie {title}: {reason}"));
                Request::None
            }
            Ok(id) => {
                let id = id.to_string();
                if self.mutation_allowed() {
                    Request::Recover(id)
                } else {
                    Request::None
                }
            }
        }
    }

    /// Recovers the old execution `execution_id`, then reads the history again
    /// so its status is the new one.
    pub(super) async fn recover_execution(&mut self, execution_id: &str) {
        match self
            .client
            .control_recover_execution(execution_id, None)
            .await
        {
            Ok(_) => {
                self.notify(
                    Severity::Success,
                    format!("execution {} wznawia się w tle", one_line(execution_id)),
                );
                self.reload_history().await;
            }
            Err(error) => {
                self.note_transport(&error);
                self.notify(
                    Severity::Error,
                    format!(
                        "wznowienie {} odrzucone — {}",
                        one_line(execution_id),
                        super::actions::refusal_reason(&error)
                    ),
                );
            }
        }
    }

    async fn reload_history(&mut self) {
        let archived = self.client.control_tasks_archived(true).await;
        let legacy = self.client.control_history().await;
        if let Ok(archived) = archived {
            self.open_history(archived, legacy);
        }
    }

    // -----------------------------------------------------------------
    // Reading
    // -----------------------------------------------------------------

    /// Reads what the visible tab of the selected old execution needs and has
    /// not got. An archived task has nothing to read: its tabs say its storage
    /// is gone.
    pub(super) async fn sync_history(&mut self) {
        let client = self.client.clone();
        if let Some(plan) = self.plan_history_reads(false) {
            let fetched = fetch_first(&client, &plan).await;
            self.apply_fetched(&plan, fetched);
        }
        if let Some(plan) = self.plan_history_reads(true) {
            let fetched = fetch_tab(&client, &plan).await;
            self.apply_fetched(&plan, fetched);
        }
    }

    /// What is due for the selected entry. The first round is the timeline and
    /// the conductor; the second, once the timeline says what there is, is what
    /// the visible tab shows.
    fn plan_history_reads(&mut self, tab_round: bool) -> Option<Plan> {
        let View::History(history) = &mut self.view else {
            return None;
        };
        let revision = history.revision;
        let tab = history.tab;
        let entry = history.entries.get(history.selected)?.clone();
        let HistoryEntry::Legacy(legacy) = &entry else {
            return None;
        };
        let key = entry.key();
        let detail = history
            .details
            .entry(key.clone())
            .or_insert_with(|| TaskDetail::new(&key));
        let mut plan = Plan {
            key,
            execution_id: legacy.view.id.clone(),
            task_id: entry.summary().task_id.clone(),
            status: legacy.view.status.clone(),
            current_state: legacy.view.current_state.clone(),
            revision,
            timeline: false,
            conductor_after: None,
            events_after: None,
            attempts: Vec::new(),
        };
        if !tab_round {
            plan.timeline = detail.progress.needs_read(revision);
            if detail.conductor.needs_read(revision, Instant::now()) {
                plan.conductor_after = Some(detail.conductor.last_sequence());
            }
            return (plan.timeline || plan.conductor_after.is_some()).then_some(plan);
        }
        match tab {
            DetailTab::Progress => {
                let attempt = detail
                    .progress
                    .selected_position()
                    .and_then(recorded_attempt);
                if let Some(attempt) = attempt {
                    if detail.attempts.needs_read(&attempt, revision) {
                        let from = detail.attempts.from(&attempt);
                        plan.attempts.push((attempt, from));
                    }
                }
            }
            DetailTab::Conversation => {
                let wanted: Vec<String> = detail
                    .progress
                    .positions()
                    .iter()
                    .filter_map(recorded_attempt)
                    .collect();
                for attempt in wanted {
                    if detail.attempts.needs_read(&attempt, revision) {
                        let from = detail.attempts.from(&attempt);
                        plan.attempts.push((attempt, from));
                    }
                }
                if plan.attempts.is_empty() && !detail.conversation.is_built() {
                    let positions = detail.progress.positions().to_vec();
                    detail.conversation.rebuild(&positions, &detail.attempts);
                }
            }
            DetailTab::Events => {
                if detail.events.needs_read(revision) {
                    plan.events_after = Some(detail.events.after());
                }
            }
            DetailTab::Conductor | DetailTab::Info => {}
        }
        let due = !plan.attempts.is_empty() || plan.events_after.is_some();
        due.then_some(plan)
    }

    /// Puts what was read into the tabs of the entry it was read for.
    fn apply_fetched(&mut self, plan: &Plan, fetched: Fetched) {
        let Fetched {
            timeline,
            conductor,
            events,
            attempts,
        } = fetched;
        let timeline = timeline.map(|result| {
            self.settle(result).map(|timeline| TimelineResponse {
                task_id: plan.task_id.clone(),
                freshness: Freshness::Live,
                observed_at: None,
                timeline: Some(timeline),
                execution_status: Some(plan.status.clone()),
                current_state: plan.current_state.clone(),
                current_position: None,
            })
        });
        let conductor = conductor.map(|result| self.settle(result));
        let events: Vec<(u64, Result<ExecutionEventsResponse, String>)> = events
            .into_iter()
            .map(|(after, result)| {
                (
                    after,
                    self.settle(result)
                        .map(|page| ExecutionEventsResponse::Available {
                            has_more: page.has_more,
                            events: page.events,
                            source: ExecutionEventsSource::Storage,
                        }),
                )
            })
            .collect();
        let attempts: Vec<(String, u64, Result<AttemptRead, String>)> = attempts
            .into_iter()
            .map(|(attempt, from, result)| (attempt, from, self.settle(result)))
            .collect();

        let View::History(history) = &mut self.view else {
            return;
        };
        let revision = plan.revision;
        let tab = history.tab;
        let Some(detail) = history.details.get_mut(&plan.key) else {
            return;
        };
        if let Some(result) = timeline {
            detail.progress.apply(revision, result);
        }
        if let Some(result) = conductor {
            detail.conductor.apply(revision, Instant::now(), result);
        }
        for (after, result) in events {
            detail.events.apply(revision, after, result);
        }
        let read_attempts = !attempts.is_empty();
        for (attempt, from, result) in attempts {
            detail.attempts.apply(&attempt, revision, from, result);
        }
        if read_attempts && tab == DetailTab::Conversation {
            let positions = detail.progress.positions().to_vec();
            detail.conversation.rebuild(&positions, &detail.attempts);
        }
    }
}

/// What a round of reads is for.
struct Plan {
    key: String,
    execution_id: String,
    task_id: TaskId,
    status: ExecutionStatus,
    current_state: Option<String>,
    revision: u64,
    timeline: bool,
    conductor_after: Option<u64>,
    events_after: Option<u64>,
    attempts: Vec<(String, u64)>,
}

/// One page of the events of an old execution.
struct EventsPage {
    events: Vec<Event>,
    has_more: bool,
}

/// The answers of a round of reads.
#[derive(Default)]
struct Fetched {
    timeline: Option<Result<Timeline, ClientError>>,
    conductor: Option<Result<ConductorReadResponse, ClientError>>,
    events: Vec<(u64, Result<EventsPage, ClientError>)>,
    attempts: Vec<(String, u64, Result<AttemptRead, ClientError>)>,
}

async fn fetch_first(client: &crate::api::ApiClient, plan: &Plan) -> Fetched {
    let mut fetched = Fetched::default();
    if plan.timeline {
        fetched.timeline = Some(client.control_history_timeline(&plan.execution_id).await);
    }
    if let Some(after) = plan.conductor_after {
        fetched.conductor = Some(read_conductor(client, &plan.execution_id, after).await);
    }
    fetched
}

async fn fetch_tab(client: &crate::api::ApiClient, plan: &Plan) -> Fetched {
    let mut fetched = Fetched::default();
    for (attempt, from) in &plan.attempts {
        let result = read_history_attempt(client, &plan.execution_id, attempt, *from).await;
        fetched.attempts.push((attempt.clone(), *from, result));
    }
    if let Some(mut after) = plan.events_after {
        for _ in 0..events::MAX_PAGES {
            let result = client
                .control_history_events(&plan.execution_id, after)
                .await
                .map(|events| EventsPage {
                    has_more: false,
                    events,
                });
            let next = match &result {
                Ok(page) => page.events.last().map(|event| event.sequence),
                Err(_) => None,
            };
            fetched.events.push((after, result));
            match next {
                Some(next) if next > after => after = next,
                _ => break,
            }
        }
    }
    fetched
}

/// What the conductor of an old execution kept, as the tab reads a conductor:
/// its chat and its failure report, for reading only.
async fn read_conductor(
    client: &crate::api::ApiClient,
    execution_id: &str,
    mut after: u64,
) -> Result<ConductorReadResponse, ClientError> {
    let state = client.control_history_conductor(execution_id).await?;
    let mut chat: Vec<ChatEntry> = Vec::new();
    for _ in 0..CHAT_PAGES {
        let page = client
            .control_history_conductor_chat(execution_id, after)
            .await?;
        match page.entries.last() {
            Some(last) if last.sequence > after => {
                after = last.sequence;
                chat.extend(page.entries);
            }
            _ => break,
        }
    }
    let mut view = ConductorView::unavailable(
        "dawne execution: dyrygent tylko do odczytu — nie ma runnera, który by odpowiedział",
    );
    view.failure = state.failure;
    view.unanswered_chat = state.unanswered_chat;
    view.chat = chat;
    Ok(ConductorReadResponse::Available {
        conductor: Box::new(view),
        source: ExecutionEventsSource::Storage,
    })
}

// ---------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------

/// Draws the list of the history into `area`.
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
    let selected = history.selected;
    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            ListItem::new(row_text(
                row,
                usize::from(inner.width),
                styles,
                stale,
                index == selected,
            ))
        })
        .collect();
    history.view.select((!rows.is_empty()).then_some(selected));
    frame.render_stateful_widget(List::new(items), inner, &mut history.view);
    if rows.is_empty() {
        let text = history.note().unwrap_or("historia jest pusta").to_string();
        frame.render_widget(
            ratatui::widgets::Paragraph::new(Line::styled(
                format!(
                    " {}",
                    fit(&one_line(&text), usize::from(inner.width).saturating_sub(2))
                ),
                styles.of(Role::Done),
            )),
            Rect::new(inner.x, inner.y, inner.width, 1.min(inner.height)),
        );
    }
}

/// Draws the details of the selected entry into `area`: the header and the
/// tabs of a task, with nothing to answer.
pub fn render_details(
    frame: &mut Frame,
    area: Rect,
    history: &mut History,
    styles: &Styles,
    focused: bool,
    stale: bool,
    hyperlinks: bool,
) {
    let tab = history.tab;
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
    let key = entry.key();
    let task = entry.summary();
    let inner = styles.render_panel(
        frame,
        area,
        panel_title(&title_of(task), area.width),
        focused,
    );
    let detail = history
        .details
        .entry(key.clone())
        .or_insert_with(|| TaskDetail::new(&key));
    let view = DetailView {
        task,
        queue_place: None,
        tab,
        detail,
    };
    let options = ViewOptions {
        stale,
        focused,
        hyperlinks,
    };
    render_view(frame, inner, view, styles, options);
}
