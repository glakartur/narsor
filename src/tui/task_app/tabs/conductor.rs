//! *Dyrygent*: the chat with the task's conductor, the history of its
//! decisions with the undo, and the choice of the harness it runs on (U12).
//!
//! # What is read
//!
//! One read, `GET /control/v1/tasks/:id/conductor`, serves this tab and the
//! answer panel (U10): [`super::super::detail::DetailRead::Conductor`] lands in
//! [`Conductor::apply`] and in the panel alike, so the conductor is never read
//! twice for the same revision. The chat comes a page at a time, oldest first,
//! continued `?after=<seq>` from the last entry held; `chat_has_more` says the
//! history goes on and `M` asks for the next pages. While the conductor owes an
//! answer (an unanswered message, an invocation under way) the tab is read
//! again every [`POLL`], because the answer is a chat entry and no event says
//! so.
//!
//! The answer is one of four things, each with its place in the tab:
//!
//! * the conductor's view — shown; when it says `available = false` the tab
//!   shows the reason and has no field to write in (the history stays, to read);
//! * `Unavailable { message }` (a remote task whose runner is not there) — the
//!   message, in the tab;
//! * a failed read (`503` with a reason, a refusal) — the reason, in the tab;
//!   neither touches the state of the connection;
//! * nothing yet — "wczytuję dyrygenta…".
//!
//! # What the operator can do
//!
//! * `c` writes to the conductor: a text box (the shared [`Chooser`] field, so
//!   `Alt+Enter` is a new line and `Esc` leaves it with the text kept);
//!   `Enter` sends `ConductorChat` through the operator commands. What is typed
//!   survives a refresh and a move to another task, and counts as a draft when
//!   the operator quits.
//! * `u` undoes the selected decision after a `y` / `n` window.
//! * `h` chooses the harness (`SetConductorHarness`); the control plane and the
//!   runner refuse when the task has no conductor, and say why on the bar.
//!
//! A proposal of the conductor's is *not* answered here: it waits in the panel
//! "Czeka na Ciebie" (an `AttentionKind::ConductorProposal`), and the tab only
//! says where it is.
//!
//! Every text that comes from the conductor or the runner is passed through
//! [`sanitize`] or [`one_line`] before it is drawn; the operator's own text is
//! sent exactly as typed.

use std::time::{Duration, Instant};

use crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::conductor::{ChatEntry, ConductorView, ConductorWork};
use crate::controller_protocol::{ConductorReadResponse, TaskSummary};
use crate::domain::ConductorPurpose;
use crate::engine::actions::harness::HarnessRegistry;
use crate::orchestration::{OperatorCommand, OperatorCommandStatus};
use crate::tui::chat::{tab_entry, tab_items, undo_lines, ChatItem};
use crate::tui::chooser::{self, Chooser, ChooserAction, ChooserField, ChooserItem, Submission};
use crate::tui::keymap::{key_of, Action, Context};
use crate::tui::style::{Role, Styles};
use crate::tui::transcript::{DisplayMode, TranscriptId, TranscriptState};

use super::super::layout::first_row;
use super::super::super::scroll::one_line;
use super::super::actions::{
    ActionSend, ConductorSent, Confirmation, Confirmed, HarnessList, Overlay,
};
use super::super::detail::short_age;
use super::super::list::fit;
use super::super::notify::Severity;
use super::super::{DetailTab, Focus, Request, TaskApp};
use super::conversation::{render_transcript, transcript_action};

/// Pages of the chat read in one go; a longer history is read on with `M`.
pub const MAX_PAGES: usize = 10;

/// How often the tab is read again while the conductor owes an answer.
pub const POLL: Duration = Duration::from_secs(2);

/// The name of the one field of the text box.
const MESSAGE: &str = "wiadomość";

/// What the tab can say of the conductor right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    /// Nothing was read yet.
    Loading,
    /// The first read failed, for this reason.
    Failed(String),
    /// There is no conductor to talk to, for this reason.
    Unavailable(String),
    /// A conductor runs and can be written to.
    Ready,
}

/// What the tab remembers per task.
#[derive(Debug, Clone)]
pub struct Conductor {
    entries: Vec<ChatEntry>,
    items: Vec<ChatItem>,
    view: Option<ConductorView>,
    /// The runner or the relay said "not now", in words.
    note: Option<String>,
    /// Why the last read failed.
    failure: Option<String>,
    has_more: bool,
    more_wanted: bool,
    revision: Option<u64>,
    read_at: Option<Instant>,
    transcript: TranscriptState,
    composer: Chooser,
}

impl Conductor {
    /// An empty tab for `task_id`.
    pub fn new(task_id: &str) -> Self {
        Self {
            entries: Vec::new(),
            items: Vec::new(),
            view: None,
            note: None,
            failure: None,
            has_more: false,
            more_wanted: false,
            revision: None,
            read_at: None,
            transcript: TranscriptState::new(TranscriptId::from(format!("conductor:{task_id}"))),
            composer: new_composer(""),
        }
    }

    /// The conductor's view as last read, without the chat (that is
    /// [`Self::items`]).
    pub fn view(&self) -> Option<&ConductorView> {
        self.view.as_ref()
    }

    /// The history read so far, with the state of each entry.
    pub fn items(&self) -> &[ChatItem] {
        &self.items
    }

    /// The reader of the history.
    pub fn transcript(&self) -> &TranscriptState {
        &self.transcript
    }

    /// Why the last read failed.
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// What the runner or the relay said when it could not answer.
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    /// Whether the history goes on after the last entry held.
    pub fn has_more(&self) -> bool {
        self.has_more
    }

    /// Whether the operator asked for the next page and it is not read yet.
    pub fn wants_more(&self) -> bool {
        self.more_wanted
    }

    /// The sequence of the last entry held: the next read continues after it.
    pub fn last_sequence(&self) -> u64 {
        self.entries.last().map_or(0, |entry| entry.sequence)
    }

    /// Whether the conductor owes an answer: a message unanswered, or an
    /// invocation under way.
    pub fn awaits_answer(&self) -> bool {
        self.view.as_ref().is_some_and(|view| {
            !view.unanswered_chat.is_empty() || view.working.iter().any(|work| work.running)
        })
    }

    /// What the tab can say of the conductor of `task`: the read's own word
    /// first, then the row's — a runner that reports no conductor says why
    /// before the tab was ever read.
    pub fn standing(&self, task: &TaskSummary) -> Standing {
        if let Some(note) = &self.note {
            return Standing::Unavailable(note.clone());
        }
        if let Some(view) = &self.view {
            return if view.available {
                Standing::Ready
            } else {
                Standing::Unavailable(unavailable_text(view.unavailable_reason.as_deref()))
            };
        }
        if let Some(runner) = task
            .runner
            .as_ref()
            .filter(|runner| !runner.conductor_available)
        {
            return Standing::Unavailable(unavailable_text(
                runner.conductor_unavailable_reason.as_deref(),
            ));
        }
        match &self.failure {
            Some(reason) => Standing::Failed(reason.clone()),
            None => Standing::Loading,
        }
    }

    /// Whether the tab must be read for a task at `revision`: it was not read
    /// at this revision, the operator asked for more, or the conductor owes an
    /// answer and a moment has passed.
    pub fn needs_read(&self, revision: u64, now: Instant) -> bool {
        if self.revision != Some(revision) {
            return true;
        }
        if self.more_wanted && self.has_more {
            return true;
        }
        self.awaits_answer()
            && self
                .read_at
                .is_none_or(|at| now.saturating_duration_since(at) >= POLL)
    }

    /// Makes the tab due for a new read, keeping what is shown.
    pub fn invalidate(&mut self) {
        self.revision = None;
    }

    /// Asks for the next page of the chat. Returns whether there is one.
    pub fn request_more(&mut self) -> bool {
        self.more_wanted = self.has_more;
        self.more_wanted
    }

    /// Takes a read in. A failed read keeps what is shown and says why; the
    /// chat of a good one is added after the entries held.
    pub fn apply(
        &mut self,
        revision: u64,
        now: Instant,
        result: Result<ConductorReadResponse, String>,
    ) {
        self.revision = Some(revision);
        self.read_at = Some(now);
        self.more_wanted = false;
        match result {
            Ok(ConductorReadResponse::Available { conductor, .. }) => {
                let mut view = *conductor;
                let after = self.last_sequence();
                let page = std::mem::take(&mut view.chat);
                self.entries
                    .extend(page.into_iter().filter(|entry| entry.sequence > after));
                self.has_more = view.chat_has_more;
                self.view = Some(view);
                self.note = None;
                self.failure = None;
                self.rebuild();
            }
            Ok(ConductorReadResponse::Unavailable { message, .. }) => {
                self.note = Some(message);
                self.failure = None;
                self.has_more = false;
            }
            Err(reason) => self.failure = Some(reason),
        }
    }

    /// Puts the history together again from the entries and the view.
    fn rebuild(&mut self) {
        let decisions = self
            .view
            .as_ref()
            .map_or(&[][..], |view| view.decisions.as_slice());
        self.items = tab_items(&self.entries, decisions);
        let proposals = self
            .view
            .as_ref()
            .map_or(&[][..], |view| view.proposals.as_slice());
        let entries = self
            .items
            .iter()
            .map(|item| {
                let proposal = proposals
                    .iter()
                    .find(|proposal| proposal.proposal_id == item.entry.id);
                tab_entry(item, proposal)
            })
            .collect();
        self.transcript.replace_entries(entries);
    }

    /// What a key does to the reader of the history.
    pub fn on_action(&mut self, action: Action) {
        transcript_action(&mut self.transcript, action);
    }

    /// The decision the reader is on, when it is one that can be undone.
    pub fn selected_undoable(&self) -> Option<&ChatItem> {
        let selected = self.transcript.selected_entry()?;
        self.items
            .iter()
            .find(|item| item.entry.id == selected.as_str())
            .filter(|item| item.undoable_decision().is_some())
    }

    // -- the text box --------------------------------------------------------

    /// Whether keys are typed into the text box.
    pub fn is_typing(&self) -> bool {
        self.composer.is_typing()
    }

    /// Puts the keys in the text box.
    pub fn begin_compose(&mut self) {
        self.composer.enter_fields();
    }

    /// Takes the keys out of the text box; what was typed stays.
    pub fn end_compose(&mut self) {
        self.composer.leave_fields();
    }

    /// Folds a key into the text box.
    pub fn compose_key(&mut self, key: KeyEvent) -> ChooserAction {
        self.composer.on_key(key)
    }

    /// What was typed and not sent.
    pub fn draft(&self) -> &str {
        self.composer
            .fields_of(0)
            .first()
            .map_or("", ChooserField::value)
    }

    /// Forgets what was typed.
    pub fn clear_draft(&mut self) {
        let typing = self.composer.is_typing();
        self.composer = new_composer("");
        if typing {
            self.composer.enter_fields();
        }
    }
}

/// The text box: one entry with one required, multi-line field.
fn new_composer(text: &str) -> Chooser {
    Chooser::new(vec![ChooserItem::new(
        "Wiadomość do dyrygenta",
        "Dyrygent odpowie wpisem w czacie; jego propozycje pojawią się w panelu \
         „Czeka na Ciebie”.",
    )
    .with_field(
        ChooserField::multiline(MESSAGE).required().with_value(text),
    )])
}

fn unavailable_text(reason: Option<&str>) -> String {
    match reason
        .map(one_line)
        .filter(|reason| !reason.trim().is_empty())
    {
        Some(reason) => reason,
        None => "dyrygent nie działa dla tego zadania".to_string(),
    }
}

/// What the conductor is doing, in a word.
fn purpose_word(purpose: ConductorPurpose) -> &'static str {
    match purpose {
        ConductorPurpose::ChooseOutput | ConductorPurpose::ChooseOutputAfterReprompt => {
            "wybiera wyjście kroku"
        }
        ConductorPurpose::DiagnoseFailure => "diagnozuje nieudany krok",
        ConductorPurpose::ReplyToChat => "odpowiada na wiadomość",
        ConductorPurpose::SummarizeGate => "streszcza bramkę",
    }
}

// ---------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------

/// Draws the tab into `area`: what the conductor is, the history, the text
/// box (while it is written in or holds a draft) and the keys.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    styles: &Styles,
    conductor: &mut Conductor,
    task: &TaskSummary,
    stale: bool,
) {
    let grey = |role: Role| if stale { Role::Done } else { role };
    let standing = conductor.standing(task);
    let ready = standing == Standing::Ready;

    let mut top = vec![standing_line(&standing, conductor, task, styles, stale)];
    if ready {
        if let Some(reason) = conductor.failure() {
            top.push(Line::styled(
                format!("✗ nie udało się odświeżyć ({})", one_line(reason)),
                styles.of(grey(Role::Problem)),
            ));
        }
    }
    let top_rows = rows(top.len(), area.height);
    let mut bottom: Vec<Line<'static>> = Vec::new();
    let mut box_rows = 0;
    if ready {
        if conductor.is_typing() {
            box_rows = conductor.composer.field_rows().min(area.height / 2).max(2);
        } else if !conductor.draft().is_empty() {
            bottom.push(Line::styled(
                format!(
                    "✎ szkic: {}",
                    fit(
                        &one_line(conductor.draft()),
                        usize::from(area.width).saturating_sub(10)
                    )
                ),
                styles.of(grey(Role::Attention)),
            ));
        }
    }
    bottom.push(key_line(conductor, ready, styles));
    let bottom_rows = rows(bottom.len(), area.height.saturating_sub(top_rows));

    let [header, body, text_box, footer] = split(area, top_rows, box_rows, bottom_rows);
    frame.render_widget(Paragraph::new(top), header);
    render_history(frame, body, styles, conductor, ready, stale);
    if box_rows > 0 {
        chooser::render_fields(frame, text_box, &conductor.composer, styles);
    }
    frame.render_widget(Paragraph::new(bottom), footer);
}

fn rows(wanted: usize, available: u16) -> u16 {
    u16::try_from(wanted).unwrap_or(u16::MAX).min(available)
}

/// The header, the history, the text box and the footer, top to bottom.
fn split(area: Rect, top: u16, text_box: u16, bottom: u16) -> [Rect; 4] {
    let text_box = text_box.min(area.height.saturating_sub(top + bottom));
    let body = area.height.saturating_sub(top + text_box + bottom);
    let at = |y: u16, height: u16| Rect::new(area.x, area.y + y, area.width, height);
    [
        at(0, top),
        at(top, body),
        at(top + body, text_box),
        at(top + body + text_box, bottom),
    ]
}

/// The line over the history: who the conductor is, or why there is none.
fn standing_line(
    standing: &Standing,
    conductor: &Conductor,
    task: &TaskSummary,
    styles: &Styles,
    stale: bool,
) -> Line<'static> {
    let grey = |role: Role| if stale { Role::Done } else { role };
    let mark = Span::styled("◆ ", styles.of(grey(Role::Conductor)));
    match standing {
        Standing::Loading => Line::from(vec![
            mark,
            Span::styled("wczytuję dyrygenta…", styles.of(Role::Done)),
        ]),
        Standing::Failed(reason) => Line::styled(
            format!("✗ nie udało się wczytać dyrygenta ({})", one_line(reason)),
            styles.of(grey(Role::Problem)),
        ),
        Standing::Unavailable(reason) => Line::from(vec![
            mark,
            Span::styled(
                format!("dyrygent niedostępny: {}", one_line(reason)),
                styles.of(grey(Role::Attention)),
            ),
        ]),
        Standing::Ready => {
            let view = conductor.view();
            let harness = view
                .and_then(|view| view.harness.as_deref())
                .map(one_line)
                .filter(|harness| !harness.is_empty());
            let mut spans = vec![
                mark,
                Span::styled("dyrygent", styles.of(grey(Role::Conductor))),
            ];
            if let Some(harness) = harness {
                spans.push(Span::styled(
                    format!(" · harness {harness}"),
                    styles.of(Role::Done),
                ));
            }
            if let Some(work) = view.and_then(|view| view.working.first()) {
                spans.push(Span::styled(
                    format!(" · {}", working_text(work, task)),
                    styles.of(grey(Role::Live)),
                ));
            } else if conductor.awaits_answer() {
                spans.push(Span::styled(
                    " · odpowie wkrótce",
                    styles.of(grey(Role::Live)),
                ));
            }
            Line::from(spans)
        }
    }
}

fn working_text(work: &ConductorWork, task: &TaskSummary) -> String {
    let age = short_age((task.as_of - work.since).num_seconds());
    if work.running {
        format!("{} ({age})", purpose_word(work.purpose))
    } else {
        format!("czeka w kolejce: {} ({age})", purpose_word(work.purpose))
    }
}

/// The keys of the tab, from the key map.
fn key_line(conductor: &Conductor, ready: bool, styles: &Styles) -> Line<'static> {
    let mut parts: Vec<String> = Vec::new();
    if conductor.is_typing() {
        let named =
            |action, what: &str| key_of(Context::Form, action).map(|key| format!("{key} {what}"));
        parts.extend(named(Action::Submit, "wyślij"));
        parts.extend(named(Action::Newline, "nowa linia"));
        parts.extend(named(Action::Cancel, "koniec pisania"));
    } else {
        let named = |action, what: &str| {
            key_of(Context::Details, action).map(|key| format!("{key} {what}"))
        };
        if ready {
            parts.extend(named(Action::Compose, "napisz"));
            parts.extend(named(Action::UndoDecision, "cofnij decyzję"));
        }
        parts.extend(named(Action::ChooseHarness, "harness"));
        if conductor.has_more() {
            parts.extend(named(Action::LoadMore, "dalsze wpisy"));
        }
    }
    Line::styled(parts.join(" · "), styles.of(Role::Done))
}

fn render_history(
    frame: &mut Frame,
    area: Rect,
    styles: &Styles,
    conductor: &mut Conductor,
    ready: bool,
    stale: bool,
) {
    if area.height == 0 {
        return;
    }
    if conductor.transcript.entries().is_empty() {
        let text = if ready {
            "brak wpisów — napisz do dyrygenta"
        } else {
            ""
        };
        frame.render_widget(
            Paragraph::new(Line::styled(text, styles.of(Role::Done))),
            area,
        );
        return;
    }
    let raw = conductor.transcript.mode() == DisplayMode::Raw;
    let mut body = area;
    if raw {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "widok surowy (Ctrl+v wraca)",
                styles.of(Role::Attention),
            )),
            first_row(area),
        );
        body = Rect::new(
            area.x,
            area.y + 1,
            area.width,
            area.height.saturating_sub(1),
        );
    }
    render_transcript(frame, body, styles, &mut conductor.transcript, stale);
}

// ---------------------------------------------------------------------
// The application's part
// ---------------------------------------------------------------------

impl TaskApp {
    /// Whether keys go into the text box of the *Dyrygent* tab: `q` and `?`
    /// are text then.
    pub(in crate::tui::task_app) fn conductor_typing(&self) -> bool {
        matches!(self.view, super::super::View::Tasks)
            && self.focus == Focus::Details
            && self.detail.tab == DetailTab::Conductor
            && !self.panel_focused()
            && self.overlay.is_none()
            && self
                .list
                .selected_id()
                .and_then(|id| self.detail.view(id))
                .is_some_and(|view| view.conductor().is_typing())
    }

    /// A key typed into the text box.
    pub(in crate::tui::task_app) fn on_conductor_typing_key(&mut self, key: KeyEvent) -> Request {
        let Some(id) = self.list.selected_id().map(str::to_string) else {
            return Request::None;
        };
        let action = self.detail.view_mut(&id).conductor.compose_key(key);
        let request = match action {
            ChooserAction::None => Request::None,
            ChooserAction::Cancel => {
                self.detail.view_mut(&id).conductor.end_compose();
                Request::None
            }
            ChooserAction::Blocked(reason) => {
                self.notify(Severity::Warning, reason);
                Request::None
            }
            ChooserAction::Submit(submission) => self.send_chat(&id, &submission),
        };
        self.sync_draft(&id);
        request
    }

    /// Keeps the draft the application remembers for `task_id` true to what
    /// its panel and its text box hold.
    pub(in crate::tui::task_app) fn sync_draft(&mut self, task_id: &str) {
        if let Some(view) = self.detail.view(task_id) {
            let text = view.draft_text();
            self.detail.set_draft(task_id, text);
        }
    }

    /// `Enter` in the text box: the message, as typed, goes to the conductor.
    /// The text stays in the box until the control plane has taken it.
    fn send_chat(&mut self, id: &str, submission: &Submission) -> Request {
        let Some(text) = submission
            .values
            .iter()
            .find(|(name, _)| name == MESSAGE)
            .map(|(_, value)| value.clone())
        else {
            return Request::None;
        };
        let Some(task) = self.selected_conductor_task(id, "wiadomość do dyrygenta") else {
            return Request::None;
        };
        if !self.mutation_allowed() {
            return Request::None;
        }
        let send = ActionSend::Operator(OperatorCommand::ConductorChat { text });
        Request::Act(Box::new(self.action_request(
            &task,
            "wiadomość do dyrygenta",
            "wiadomość wysłana do dyrygenta".to_string(),
            send,
        )))
    }

    /// The task `id` when its conductor can be written to, otherwise says why
    /// on the bar, under `what`.
    fn selected_conductor_task(&mut self, id: &str, what: &str) -> Option<TaskSummary> {
        let task = self
            .list
            .tasks()
            .iter()
            .find(|task| task.task_id.as_str() == id)?
            .clone();
        let standing = self.detail.view_mut(id).conductor().standing(&task);
        let reason = match standing {
            Standing::Ready => return Some(task),
            Standing::Loading => "stan dyrygenta nie został jeszcze wczytany".to_string(),
            Standing::Failed(reason) => format!("nie udało się wczytać dyrygenta ({reason})"),
            Standing::Unavailable(reason) => reason,
        };
        self.notify(Severity::Warning, format!("{what}: {}", one_line(&reason)));
        None
    }

    /// A key of the tab that is not about reading: `c`, `u`, `h`, `M`.
    pub(in crate::tui::task_app) fn on_conductor_action(&mut self, action: Action) -> Request {
        let Some(id) = self.list.selected_id().map(str::to_string) else {
            return Request::None;
        };
        if self.detail.tab != DetailTab::Conductor {
            let key = key_of(Context::Details, action).unwrap_or_default();
            self.notify(
                Severity::Warning,
                format!("{key}: to działa w zakładce Dyrygent"),
            );
            return Request::None;
        }
        match action {
            Action::Compose => {
                if self
                    .selected_conductor_task(&id, "wiadomość do dyrygenta")
                    .is_some()
                {
                    self.detail.view_mut(&id).conductor.begin_compose();
                }
            }
            Action::UndoDecision => self.ask_undo(&id),
            Action::ChooseHarness => self.open_harnesses(&id),
            Action::LoadMore => self.load_more(&id),
            _ => {}
        }
        Request::None
    }

    /// `M`: the next page of the chat is read by the next sync.
    fn load_more(&mut self, id: &str) {
        if !self.detail.view_mut(id).conductor.request_more() {
            self.notify(Severity::Warning, "to cały czat — nie ma dalszych wpisów");
        }
    }

    /// `u`: the window that asks before the selected decision is undone.
    fn ask_undo(&mut self, id: &str) {
        let Some(task) = self.selected_conductor_task(id, "cofnięcie decyzji") else {
            return;
        };
        let Some(item) = self
            .detail
            .view(id)
            .and_then(|view| view.conductor().selected_undoable().cloned())
        else {
            self.notify(
                Severity::Warning,
                "cofnięcie decyzji: zaznacz (j / k) decyzję dyrygenta, którą można cofnąć",
            );
            return;
        };
        let Some(decision_id) = item.undoable_decision().map(str::to_string) else {
            return;
        };
        if !self.mutation_allowed() {
            return;
        }
        let request = self.action_request(
            &task,
            "cofnięcie decyzji",
            "cofnięcie decyzji wysłane do runnera".to_string(),
            ActionSend::Operator(OperatorCommand::UndoDecision { decision_id }),
        );
        self.overlay = Some(Overlay::Confirm(Confirmation::new(
            id,
            "Cofnij decyzję dyrygenta !",
            undo_lines(&item),
            Confirmed::Act(Box::new(request)),
        )));
    }

    /// `h`: the list of harnesses the conductor can run on.
    fn open_harnesses(&mut self, id: &str) {
        let Some(task) = self.selected_conductor_task(id, "zmiana harnessu dyrygenta") else {
            return;
        };
        let current = self
            .detail
            .view(id)
            .and_then(|view| view.conductor().view())
            .and_then(|view| view.harness.clone());
        let harnesses: Vec<String> = HarnessRegistry::defaults()
            .supported()
            .into_iter()
            .map(str::to_string)
            .collect();
        let items = harnesses
            .iter()
            .map(|name| {
                let item = ChooserItem::new(
                    name.clone(),
                    format!(
                        "Dyrygent zadania {} pójdzie na harnessie „{name}” od następnego \
                         wywołania; wywołanie, które trwa, kończy na dotychczasowym. Polecenie \
                         trafia do runnera.",
                        super::super::list::title_of(&task)
                    ),
                );
                if current.as_deref() == Some(name.as_str()) {
                    item.unavailable("dyrygent już działa na tym harnessie")
                } else {
                    item
                }
            })
            .collect();
        let first_other = harnesses
            .iter()
            .position(|name| current.as_deref() != Some(name.as_str()))
            .unwrap_or(0);
        self.overlay = Some(Overlay::Harness(HarnessList {
            task_id: id.to_string(),
            harnesses,
            chooser: Chooser::new(items).selecting(first_other),
        }));
    }

    /// A harness was chosen from the list.
    pub(in crate::tui::task_app) fn chosen_harness(
        &mut self,
        list: &HarnessList,
        submission: &Submission,
    ) -> Request {
        let Some(harness) = list.harnesses.get(submission.index).cloned() else {
            return Request::None;
        };
        let Some(task) = self.selected_conductor_task(&list.task_id, "zmiana harnessu dyrygenta")
        else {
            return Request::None;
        };
        if !self.mutation_allowed() {
            return Request::None;
        }
        let summary = format!(
            "harness dyrygenta: {} (od następnego wywołania)",
            one_line(&harness)
        );
        Request::Act(Box::new(self.action_request(
            &task,
            "zmiana harnessu dyrygenta",
            summary,
            ActionSend::Operator(OperatorCommand::SetConductorHarness { harness }),
        )))
    }

    /// The control plane took a command of the conductor's. A message leaves
    /// the text box; whatever it was, the tab is read again.
    pub(in crate::tui::task_app) fn conductor_command_sent(
        &mut self,
        task_id: &str,
        sent: ConductorSent,
        status: OperatorCommandStatus,
    ) {
        if status == OperatorCommandStatus::Rejected {
            return;
        }
        if sent == ConductorSent::Chat {
            self.detail.view_mut(task_id).conductor.clear_draft();
            self.sync_draft(task_id);
        }
        self.detail.view_mut(task_id).conductor.invalidate();
    }

    /// The runner settled a command of the conductor's: what the tab shows
    /// is due for a new read.
    pub(in crate::tui::task_app) fn conductor_command_settled(&mut self, task_id: &str) {
        if let Some(view) = self.detail.views.get_mut(task_id) {
            view.conductor.invalidate();
        }
    }
}
