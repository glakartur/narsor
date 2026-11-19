//! The queues view (U12, U9; R23): the queues of the catalogue and the
//! automations that serve them. Read only: queues and automations are
//! catalogue files (`queues/NAME.yaml`, `automations/NAME/`); Narsor only
//! reads them, and nothing here changes them.
//!
//! ```text
//! │ Zadania            │ Queues (2) · 1 file not loaded                       │
//! │ ● NAR-14  edit     │╭ Akcje ──────────────────────────╮ ╭ Opis ─────────╮│
//! │                    ││ │ 1 mr-review · 1 entry         │ │ mode: on_epis…││
//! │                    ││   2 ! triage · 0 entries        │ │ schedule: …   ││
//! │                    │╰─────────────────────────────────╯ ╰───────────────╯│
//! │                    │ Catalogue files not loaded (1)                       │
//! │                    │ queues/broken.yaml: unknown field `mod`              │
//! ```
//!
//! Two levels, one at a time in the details area:
//!
//! * the **list** of queues, each with its number of entries, a mark for a
//!   queue that does not run, and under it the catalogue files that failed
//!   to load ([`list`]);
//! * a **queue**: its automations on a list with their description
//!   (scripts, state, runs, errors, secrets), its entries with, per
//!   automation, how it processed them, and the problems of its file
//!   ([`detail`]).
//!
//! This module is plain state and drawing. The application's part — the
//! keys and the reads as keyed background jobs — is `task_app::queues`.

pub mod detail;
pub mod list;

use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use narsor_protocol::controller::{
    CatalogProblemView, ObserverQueueListResponse, ObserverQueueSummary, ObserverQueueView,
};

use super::chooser::Chooser;
use super::scroll::one_line;
use super::style::{Role, Styles};

pub use detail::QueueDetail;

/// The CLI commands that show the same, for a script or a log.
pub const CLI_COMMAND: &str = "narsor-cli queue list | narsor-cli queue show NAME";

/// Where the configuration lives: the view shows it read only.
pub const CATALOGUE_HINT: &str = "Read only: a queue is the catalogue file queues/NAME.yaml, an \
                                  automation the directory automations/NAME/. Change them there.";

/// The mark of a queue, or a catalogue file, that cannot be used.
pub const PROBLEM_MARK: &str = "!";

/// The state of the queues view.
#[derive(Debug)]
pub struct Queues {
    queues: Vec<ObserverQueueSummary>,
    problems: Vec<CatalogProblemView>,
    chooser: Chooser,
    detail: Option<QueueDetail>,
    /// The queue whose read runs, shown with the loading marker.
    opening: Option<String>,
}

impl Queues {
    /// The queues of `listing`, on the first one.
    pub fn new(listing: ObserverQueueListResponse) -> Self {
        let mut queues = Self {
            queues: Vec::new(),
            problems: Vec::new(),
            chooser: Chooser::new(Vec::new()),
            detail: None,
            opening: None,
        };
        queues.set_listing(listing);
        queues
    }

    /// Takes a new listing in; the highlighted queue stays.
    pub fn set_listing(&mut self, listing: ObserverQueueListResponse) {
        let kept = self.selected_queue().map(|queue| queue.queue.clone());
        self.queues = listing.queues;
        self.problems = listing.problems;
        let index = kept
            .and_then(|name| self.queues.iter().position(|queue| queue.queue == name))
            .unwrap_or(0);
        self.chooser =
            Chooser::new(self.queues.iter().map(list::queue_item).collect()).selecting(index);
    }

    /// The queues, by name.
    pub fn queues(&self) -> &[ObserverQueueSummary] {
        &self.queues
    }

    /// The catalogue files of automations and queues that failed to load.
    pub fn problems(&self) -> &[CatalogProblemView] {
        &self.problems
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

    /// Shows `view`; the highlighted automation stays when it is the same
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
}

/// `file: reason` of a catalogue file that cannot be used, on one line.
pub fn problem_text(problem: &CatalogProblemView) -> String {
    format!("{}: {}", one_line(&problem.file), one_line(&problem.reason))
}

/// Draws the view into `area`: a queue or the list, whichever is on top.
/// `loading` is the loading marker's text.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    queues: &mut Queues,
    styles: &Styles,
    focused: bool,
    loading: &str,
) {
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
