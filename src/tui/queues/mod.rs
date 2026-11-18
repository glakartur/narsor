//! The queues view (U12; R23, R31–R34): the owner's observer queues and the
//! observers that serve them.
//!
//! ```text
//! │ Zadania            │ Queues (2) · O                                      │
//! │ ● NAR-14  edit     │╭ Akcje ─────────────────────╮ ╭ Opis ─────────────╮│
//! │                    ││ │ 1 mr-review · 1 entry    │ │ mode: on_episodes ││
//! │                    ││   2 jira-intake · 0 entries│ │ observers: 2      ││
//! ```
//!
//! Three levels, one at a time in the details area:
//!
//! * the **list** of queues, each with its number of entries ([`list`]);
//! * a **queue**: its observers on a list with their description (name,
//!   approved and newest version, state, schedule, last run, and read only
//!   the settings the CLI sets, R34), and under them its entries with, per
//!   observer, the last run and the newest error ([`detail`]);
//! * over either, a **form** (create, edit, script; [`forms`]) or the
//!   **approval** screen that shows the script text before `y` (R32).
//!
//! This module is plain state and drawing. The application's part — the
//! keys, the reads as keyed background jobs and the writes — is
//! `task_app::queues`. A save never approves a version; the view marks an
//! observer whose newest version is not the approved one (R32, AE10).
//! Script editing goes through `$VISUAL` / `$EDITOR` when one is set, with
//! the terminal suspended, and through a paste field otherwise (KTD-10,
//! [`script`]).

pub mod detail;
pub mod forms;
pub mod list;
pub mod script;

use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use narsor_protocol::controller::{
    CreateObserverRequest, ObserverQueueListResponse, ObserverQueueSummary, ObserverQueueView,
    UpdateObserverRequest,
};

use super::chooser::{self, Chooser};
use super::scroll::one_line;
use super::style::{Role, Styles};

pub use detail::{ApproveScript, QueueDetail};
pub use forms::{FormKind, ObserverForm};

/// The command that sets what the view shows read only (R34).
pub const CLI_COMMAND: &str = "narsor-cli observer create";

/// The CLI hint for the settings the view shows read only (R34).
pub const CLI_HINT: &str = "Secrets, the task limit and the workflow mapping are read only here: \
                            set them with `narsor-cli observer create` (see --help).";

/// A write of the queues view: what a key or a form asks of the owner's
/// observer routes.
#[derive(Debug, Clone, PartialEq)]
pub enum QueueWrite {
    /// Create an observer, then save its first script when one was given.
    Create {
        /// The new observer.
        request: Box<CreateObserverRequest>,
        /// Its first script; saving never approves it.
        script: Option<String>,
    },
    /// Change the queue, the mode or the schedule of an observer.
    Update {
        /// The observer, by name.
        observer: String,
        /// Only what changed.
        request: UpdateObserverRequest,
    },
    /// Save the next script version of an observer; never approves it.
    SaveScript {
        /// The observer, by name.
        observer: String,
        /// The script.
        script: String,
    },
    /// Approve a script version: the schedule runs it from now on.
    Approve {
        /// The observer, by name.
        observer: String,
        /// The version the owner read.
        version: u32,
    },
    /// Stop the schedule of an observer.
    Pause(String),
    /// Put an observer back on its schedule.
    Resume(String),
    /// Delete an observer with its scripts, run errors and, when no other
    /// observer serves its queue, the queue's entries (R33).
    Delete(String),
}

impl QueueWrite {
    /// The observer the write is about.
    pub fn observer(&self) -> &str {
        match self {
            Self::Create { request, .. } => &request.name,
            Self::Update { observer, .. }
            | Self::SaveScript { observer, .. }
            | Self::Approve { observer, .. } => observer,
            Self::Pause(observer) | Self::Resume(observer) | Self::Delete(observer) => observer,
        }
    }

    /// What the write does, for the bar when it fails.
    pub fn verb(&self) -> &'static str {
        match self {
            Self::Create { .. } => "create observer",
            Self::Update { .. } => "edit observer",
            Self::SaveScript { .. } => "save script",
            Self::Approve { .. } => "approve version",
            Self::Pause(_) => "pause observer",
            Self::Resume(_) => "resume observer",
            Self::Delete(_) => "delete observer",
        }
    }

    /// Whether the write is the send of a form, which stays open when the
    /// server refuses it.
    pub fn is_form(&self) -> bool {
        matches!(
            self,
            Self::Create { .. } | Self::Update { .. } | Self::SaveScript { .. }
        )
    }
}

/// What the script edit opens in `$EDITOR`: the observer and the text of
/// its newest version (empty when it has none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorRequest {
    /// The observer, by name.
    pub observer: String,
    /// The version the text is of.
    pub base_version: Option<u32>,
    /// The text the editor starts with.
    pub text: String,
}

/// The state of the queues view.
#[derive(Debug)]
pub struct Queues {
    queues: Vec<ObserverQueueSummary>,
    chooser: Chooser,
    detail: Option<QueueDetail>,
    /// The queue whose read runs, shown with the loading marker.
    opening: Option<String>,
    form: Option<ObserverForm>,
    approve: Option<ApproveScript>,
    editor: Option<EditorRequest>,
}

impl Queues {
    /// The queues of `listing`, on the first one.
    pub fn new(listing: ObserverQueueListResponse) -> Self {
        let mut queues = Self {
            queues: Vec::new(),
            chooser: Chooser::new(Vec::new()),
            detail: None,
            opening: None,
            form: None,
            approve: None,
            editor: None,
        };
        queues.set_listing(listing);
        queues
    }

    /// Takes a new listing in; the highlighted queue stays.
    pub fn set_listing(&mut self, listing: ObserverQueueListResponse) {
        let kept = self.selected_queue().map(|queue| queue.queue.clone());
        self.queues = listing.queues;
        let index = kept
            .and_then(|name| self.queues.iter().position(|queue| queue.queue == name))
            .unwrap_or(0);
        self.chooser =
            Chooser::new(self.queues.iter().map(list::queue_item).collect()).selecting(index);
    }

    /// The owner's queues, by name.
    pub fn queues(&self) -> &[ObserverQueueSummary] {
        &self.queues
    }

    /// The highlighted queue of the list.
    pub fn selected_queue(&self) -> Option<&ObserverQueueSummary> {
        self.queues.get(self.chooser.selected())
    }

    /// The list of queues.
    pub fn chooser(&self) -> &Chooser {
        &self.chooser
    }

    /// The list of queues, to move on it.
    pub fn chooser_mut(&mut self) -> &mut Chooser {
        &mut self.chooser
    }

    /// The queue shown, when one is.
    pub fn detail(&self) -> Option<&QueueDetail> {
        self.detail.as_ref()
    }

    /// The queue shown, to move on it.
    pub fn detail_mut(&mut self) -> Option<&mut QueueDetail> {
        self.detail.as_mut()
    }

    /// Shows `view`; the highlighted observer stays when it is the same
    /// queue.
    pub fn show_queue(&mut self, view: ObserverQueueView) {
        self.opening = None;
        match self.detail.as_mut() {
            Some(detail) if detail.view().queue == view.queue => detail.set_view(view),
            _ => self.detail = Some(QueueDetail::new(view)),
        }
    }

    /// Back to the list of queues.
    pub fn close_queue(&mut self) {
        self.detail = None;
        self.opening = None;
    }

    /// The queue whose read runs.
    pub fn opening(&self) -> Option<&str> {
        self.opening.as_deref()
    }

    /// Marks `queue` as being read.
    pub fn set_opening(&mut self, queue: Option<String>) {
        self.opening = queue;
    }

    /// The form, while one is open.
    pub fn form(&self) -> Option<&ObserverForm> {
        self.form.as_ref()
    }

    /// The form, to type in it.
    pub fn form_mut(&mut self) -> Option<&mut ObserverForm> {
        self.form.as_mut()
    }

    /// Opens `form` over the view.
    pub fn open_form(&mut self, form: ObserverForm) {
        self.approve = None;
        self.form = Some(form);
    }

    /// Closes the form; what was typed is not kept.
    pub fn close_form(&mut self) {
        self.form = None;
    }

    /// The approval screen, while it is open.
    pub fn approve(&self) -> Option<&ApproveScript> {
        self.approve.as_ref()
    }

    /// The approval screen, to scroll it.
    pub fn approve_mut(&mut self) -> Option<&mut ApproveScript> {
        self.approve.as_mut()
    }

    /// Opens the approval screen.
    pub fn open_approve(&mut self, approve: ApproveScript) {
        self.form = None;
        self.approve = Some(approve);
    }

    /// Closes the approval screen.
    pub fn close_approve(&mut self) {
        self.approve = None;
    }

    /// Asks the loop to open `request` in the editor.
    pub fn request_editor(&mut self, request: EditorRequest) {
        self.editor = Some(request);
    }

    /// The editor the loop must open, taken.
    pub fn take_editor(&mut self) -> Option<EditorRequest> {
        self.editor.take()
    }

    /// What was typed in the form and not sent.
    pub fn draft_text(&self) -> String {
        self.form
            .as_ref()
            .map(|form| form.chooser().draft_text())
            .unwrap_or_default()
    }
}

/// Draws the view into `area`: the approval screen, a form, a queue or the
/// list, whichever is on top. `loading` is the loading marker's text.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    queues: &mut Queues,
    styles: &Styles,
    focused: bool,
    loading: &str,
) {
    if let Some(approve) = queues.approve.as_mut() {
        detail::render_approve(frame, area, approve, styles, focused);
        return;
    }
    if let Some(form) = queues.form.as_mut() {
        let inner = styles.render_panel(frame, area, form.title(), focused);
        chooser::render(frame, inner, form.chooser_mut(), styles, focused);
        return;
    }
    if let Some(queue) = queues.opening.as_deref() {
        let inner =
            styles.render_panel(frame, area, format!(" Queue {} ", one_line(queue)), focused);
        frame.render_widget(
            Paragraph::new(Line::styled(loading.to_string(), styles.of(Role::Done))),
            inner,
        );
        return;
    }
    if let Some(detail) = queues.detail.as_mut() {
        detail::render(frame, area, detail, styles, focused);
        return;
    }
    list::render(frame, area, queues, styles, focused);
}
