//! The action panel and the operator's actions (U11, origin §3).
//!
//! `Ctrl+.` (and `.`, for a terminal that does not pass it) opens one panel of
//! everything the operator can do to the selected task. The server says what
//! that is: [`TaskSummary::actions`] holds only the actions that can be done
//! now, each described whole — its name, what it does, whether it cannot be
//! undone, whether it opens a submenu, the fields it needs and the command to
//! send. The panel draws that on the multi-step form ([`SteppedForm`]):
//! choose, fill in, confirm. It has no list of its own and no `match` on the
//! id of an action: an action it does not know is shown, confirmed and sent
//! like any other (R15, R16).
//!
//! The panel takes the place of the conversation's form at the bottom of the
//! details ([`super::detail::render`]); where the details are not drawn it
//! sits in the middle of the screen.
//!
//! Two actions are the client's own: moving a queued task (`Ctrl+↑` /
//! `Ctrl+↓`, `m`) is not in the panel, and opening a link of the task is its
//! last entry, done without a confirmation. A comment is the conversation's.
//!
//! # Confirmation
//!
//! Every action of the server ends in the confirmation step of the panel:
//! what will happen, what was typed, `!` for an action that cannot be undone,
//! then `Enter` to do it and `Esc` to go back. The `y` / `n` window
//! ([`Confirmation`], keys of [`Context::Confirm`]) stays for the answer panel
//! (U10) and the repositories.
//!
//! # What goes where
//!
//! The server names the command: an owner command (`POST …/commands`, the
//! answer is the new runner state), a command for the runner
//! (`POST …/operator-commands`, the answer only says "queued" and the verdict
//! comes as `OperatorCommandResolved`, [`CommandResolved`]) or a comment.
//! What was typed goes into it where the server put the field's placeholder
//! ([`TaskActionSend::resolve`]).
//!
//! Every send passes [`TaskApp::mutation_allowed`] and carries an
//! [`idempotency_key`] that a retry of the same decision reuses. A refusal of
//! the server (`action_unavailable`) says why on the bar and reads the tasks
//! again.

use crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::api::ClientError;
use crate::controller_protocol::{
    OwnerCommandRequest, SubmitCommentRequest, SubmitOperatorCommandRequest, TaskSummary,
};
use crate::domain::display::safe_link_url;
use narsor_protocol::controller::{
    OperatorCommand, OperatorCommandStatus, ResolvedActionSend, RunnerState, TaskActionFieldKind,
    TaskActionId, TaskActionKind, TaskActionOffer, TaskActionSend, TaskActionSurface, TaskState,
};

use super::super::chooser::{self, Chooser, ChooserAction, ChooserField, ChooserItem, Submission};
use super::super::keymap::{self, Action, Context};
use super::super::scroll::one_line;
use super::super::stepped::{self, Stage, StepEntry, SteppedForm, SteppedOutcome};
use super::super::style::{Role, Styles};
use super::harness::HarnessList;
use super::limit::LimitList;
use super::list::{title_of, CommandResolved};
use super::notify::Severity;
use super::panel::AnswerRequest;
use super::{failure_reason, idempotency_key, is_transport, Request, TaskApp};

// ---------------------------------------------------------------------
// What can be done, and when
// ---------------------------------------------------------------------

/// An action of the client's own, or the conversation's comment: the actions
/// of the panel are the server's ([`ActionPanel`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskAction {
    /// `Ctrl+↑` / `Ctrl+↓` / `m`: another place in the queue.
    Move,
    /// `o`: open a link of the task.
    OpenLink,
    /// `c`: a comment to the agent; the server picks what it does by the
    /// way the task is stopped. Not in the panel: the conversation owns it.
    Comment,
}

impl TaskAction {
    /// The action a key of the map asks for.
    pub fn of_key(action: Action) -> Option<Self> {
        match action {
            Action::MoveUp | Action::MoveDown | Action::MoveMode => Some(Self::Move),
            Action::OpenLink => Some(Self::OpenLink),
            Action::Comment => Some(Self::Comment),
            _ => None,
        }
    }

    /// Whether it changes anything on the server (and so is blocked without a
    /// connection). Opening a link does not.
    pub fn changes_state(self) -> bool {
        self != Self::OpenLink
    }
}

/// Whether an action can be done to a task now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// It can.
    Available,
    /// It cannot, and this is why — in words for the bar.
    Unavailable(String),
}

impl Availability {
    /// Whether it can be done.
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }

    /// Why it cannot be done.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Available => None,
            Self::Unavailable(reason) => Some(reason),
        }
    }

    fn because(reason: impl Into<String>) -> Self {
        Self::Unavailable(reason.into())
    }
}

/// Said when the server gave no list of actions (a server from before the
/// offer, or no data).
pub const NO_OFFER: &str = "niedostępne: brak danych z serwera";
/// Said of an action the server does not offer now: it offers only what can
/// be done.
pub const NOT_OFFERED: &str = "niedostępne teraz";
/// What the panel says when the server gave no list of actions (R6).
pub const NO_DATA_TEXT: &str = "Brak danych z serwera: serwer nie podał listy akcji.";
/// What the panel says when the list of actions is empty.
pub const NOTHING_TEXT: &str = "Brak dostępnych akcji dla tego zadania.";

/// The offer of the server for `id`, if it made one.
fn offer_of(task: &TaskSummary, id: TaskActionId) -> Option<&TaskActionOffer> {
    task.actions.as_ref()?.iter().find(|offer| offer.id == id)
}

/// What the server says of `id` for `task`: it is in the offer, or it is
/// not. An older server still lists what is not available, with its reason.
fn offered(task: &TaskSummary, id: TaskActionId) -> Availability {
    if task.actions.is_none() {
        return Availability::because(NO_OFFER);
    }
    match offer_of(task, id) {
        None => Availability::because(NOT_OFFERED),
        Some(offer) if offer.available => Availability::Available,
        Some(offer) => Availability::because(offer.reason.as_deref().unwrap_or("niedostępne")),
    }
}

/// Whether `action` can be done to `task` now, and if not, why.
///
/// A comment is the server's to allow (it offers it, or it does not);
/// moving and opening a link are the client's own. A pure function of the
/// summary, so the shortcuts and the notifications cannot disagree. It knows
/// nothing of the connection — that gate is [`TaskApp::mutation_allowed`].
pub fn availability(task: &TaskSummary, action: TaskAction) -> Availability {
    match action {
        // Moving is an operation on the queue itself: only a task that waits
        // there (not one admitted and about to start) can be moved.
        TaskAction::Move => match task.runner_state {
            RunnerState::Queued => Availability::Available,
            _ => Availability::because("zadanie nie jest w kolejce"),
        },
        TaskAction::OpenLink => {
            if openable_links(task).next().is_some() {
                Availability::Available
            } else {
                Availability::because("zadanie nie ma jeszcze linku do PR")
            }
        }
        TaskAction::Comment => offered(task, TaskActionId::Comment),
    }
}

/// The pull request links a browser can be pointed at: the workspace's name,
/// what the header shows for the pull request and its safe address.
fn openable_links(task: &TaskSummary) -> impl Iterator<Item = (&str, String, String)> {
    task.workspaces.iter().filter_map(|workspace| {
        let label = workspace.pr_label();
        let text = label.text();
        Some((workspace.name.as_str(), text, label.url?))
    })
}

/// The word that opens a notification about `action`
/// ("przesuń: zadanie nie jest w kolejce").
pub fn verb(action: TaskAction) -> &'static str {
    match action {
        TaskAction::Move => "przesuń",
        TaskAction::OpenLink => "link",
        TaskAction::Comment => "komentarz",
    }
}

// ---------------------------------------------------------------------
// What is sent
// ---------------------------------------------------------------------

/// What an action sends.
#[derive(Debug, Clone, PartialEq)]
pub enum ActionSend {
    /// A runner state command of the owner: pause, resume, start, archive.
    Owner(narsor_protocol::controller::OwnerCommand),
    /// A command for the runner: interrupt, a transition.
    Operator(OperatorCommand),
    /// A comment to the agent: what it does, the server decides.
    Comment(String),
}

impl From<ResolvedActionSend> for ActionSend {
    fn from(send: ResolvedActionSend) -> Self {
        match send {
            ResolvedActionSend::Owner(command) => Self::Owner(command),
            ResolvedActionSend::Operator(command) => Self::Operator(command),
            ResolvedActionSend::Comment(text) => Self::Comment(text),
        }
    }
}

/// An action chosen and confirmed, ready to be sent: [`TaskApp::perform`]
/// carries it out.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionRequest {
    /// The task.
    pub task_id: String,
    /// What its row shows, for the notification.
    pub title: String,
    /// The generation the screen shows; the command is refused if the task
    /// has moved on.
    pub generation: u64,
    /// The key a retry of this decision reuses.
    pub key: String,
    /// What the action is called, for a refusal ("pauza").
    pub verb: String,
    /// What the notification says when it was accepted.
    pub summary: String,
    /// What to send.
    pub send: ActionSend,
}

/// A link chosen, ready to be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkRequest {
    /// What the link is called.
    pub label: String,
    /// Where it leads: an `http(s)` address.
    pub url: String,
}

/// What a confirmed window sends.
#[derive(Debug, Clone, PartialEq)]
pub enum Confirmed {
    /// An action of the client.
    Act(Box<ActionRequest>),
    /// A chosen action of the answer panel.
    Answer(Box<AnswerRequest>),
    /// Removing the repository with this id (the repositories, U13).
    RemoveRepository(String),
}

// ---------------------------------------------------------------------
// What sits over the screen
// ---------------------------------------------------------------------

/// The `y` / `n` window of an action `!`: what it will do, in words.
#[derive(Debug, Clone, PartialEq)]
pub struct Confirmation {
    task_id: String,
    heading: String,
    lines: Vec<String>,
    confirmed: Confirmed,
}

impl Confirmation {
    /// A window for `confirmed` over the task `task_id`, titled `heading` and
    /// saying what it does in `lines`.
    pub fn new(
        task_id: impl Into<String>,
        heading: impl Into<String>,
        lines: Vec<String>,
        confirmed: Confirmed,
    ) -> Self {
        Self {
            task_id: task_id.into(),
            heading: heading.into(),
            lines,
            confirmed,
        }
    }

    /// What is being confirmed.
    pub fn heading(&self) -> &str {
        &self.heading
    }

    /// What it will do.
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// What `y` sends.
    pub fn confirmed(&self) -> &Confirmed {
        &self.confirmed
    }
}

/// What the server gave the panel to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelContent {
    /// Actions, or the client's own link.
    Entries,
    /// An empty list: nothing can be done now.
    Nothing,
    /// No list at all (R6): nothing is drawn in place of it.
    NoData,
}

/// The action panel of one task: the server's actions on the multi-step
/// form, and the client's link at the end.
#[derive(Debug, Clone)]
pub struct ActionPanel {
    task_id: String,
    /// The actions of the panel as the server offered them, in their order:
    /// what the form's entries stand for, at the same indices.
    offers: Vec<TaskActionOffer>,
    /// Whether the last entry is the client's link.
    link: bool,
    content: PanelContent,
    form: SteppedForm,
    /// The generation of the task the operator decides on: the one of the
    /// refresh before the operator chose an action. A later refresh does
    /// not change it, so an action confirmed on a task that changed since
    /// is refused as stale.
    generation: u64,
    /// Whether the last frame drew the panel in the details.
    pub(super) in_details: bool,
}

impl ActionPanel {
    /// The panel of `task`. `blocked` is why nothing that changes state can
    /// be done right now (no connection), if that is so.
    fn new(task: &TaskSummary, blocked: Option<&str>) -> Self {
        let (offers, content) = panel_offers(task);
        let link = content != PanelContent::NoData && openable_links(task).next().is_some();
        let entries = panel_entries(&offers, link, blocked);
        Self {
            task_id: task.task_id.to_string(),
            offers,
            link,
            content: if link { PanelContent::Entries } else { content },
            form: SteppedForm::new("Akcje", entries),
            generation: task.generation,
            in_details: false,
        }
    }

    /// The task the panel is for.
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    /// What the server gave the panel to draw.
    pub fn content(&self) -> PanelContent {
        self.content
    }

    /// The form of the panel.
    pub fn form(&self) -> &SteppedForm {
        &self.form
    }

    /// The labels of the entries of the level open now, as drawn.
    pub fn labels(&self) -> Vec<String> {
        self.form
            .chooser()
            .items()
            .iter()
            .map(|item| item.label.clone())
            .collect()
    }

    /// The keys that apply now: the menu's on a choice.
    fn context(&self) -> Context {
        match self.form.stage() {
            Stage::Choice => Context::Menu,
            _ => self.form.context(),
        }
    }

    /// The same panel over what `task` offers now, keeping the level, the
    /// step and what was typed. The label of a chosen entry that is gone
    /// comes back, for the bar.
    fn rebuilt(self, task: &TaskSummary, blocked: Option<&str>) -> (Self, Option<String>) {
        let fresh = Self::new(task, blocked);
        let entries = panel_entries(&fresh.offers, fresh.link, blocked);
        let (form, lost) = self.form.rebuilt(entries);
        let generation = if form.stage() == Stage::Choice {
            fresh.generation
        } else {
            self.generation
        };
        (
            Self {
                form,
                generation,
                in_details: self.in_details,
                ..fresh
            },
            lost,
        )
    }

    /// The offer the entry at `path` stands for; `None` for the link.
    fn offer_at(&self, path: &[usize]) -> Option<&TaskActionOffer> {
        let (first, rest) = path.split_first()?;
        let mut offer = self.offers.get(*first)?;
        for index in rest {
            offer = drawn(&offer.children).nth(*index)?;
        }
        Some(offer)
    }

    /// Whether `path` is the client's link.
    fn is_link(&self, path: &[usize]) -> bool {
        self.link && path == [self.offers.len()]
    }
}

/// The offers a client can draw, in their order: described, of a kind it
/// knows. An older server describes none.
fn drawn(offers: &[TaskActionOffer]) -> impl Iterator<Item = &TaskActionOffer> {
    offers
        .iter()
        .filter(|offer| offer.is_described() && offer.kind != TaskActionKind::Unrecognized)
}

/// The actions of the panel in `task`'s offer, and what the panel shows.
fn panel_offers(task: &TaskSummary) -> (Vec<TaskActionOffer>, PanelContent) {
    let Some(actions) = task.actions.as_ref() else {
        return (Vec::new(), PanelContent::NoData);
    };
    // A server from before the descriptions lists actions it does not
    // describe: the panel has nothing to draw them with.
    if !actions.is_empty() && !actions.iter().any(TaskActionOffer::is_described) {
        return (Vec::new(), PanelContent::NoData);
    }
    let mut offers: Vec<TaskActionOffer> = drawn(actions)
        .filter(|offer| offer.surface == TaskActionSurface::Panel)
        .cloned()
        .collect();
    offers.sort_by_key(|offer| offer.order);
    let content = if offers.is_empty() {
        PanelContent::Nothing
    } else {
        PanelContent::Entries
    };
    (offers, content)
}

/// The entries of the form for `offers`, and the link at the end.
fn panel_entries(offers: &[TaskActionOffer], link: bool, blocked: Option<&str>) -> Vec<StepEntry> {
    let mut entries: Vec<StepEntry> = offers
        .iter()
        .map(|offer| entry_of(offer, blocked))
        .collect();
    if link {
        entries.push(StepEntry::immediate(ChooserItem::new(
            "Otwórz link",
            "Otwiera w przeglądarce link do PR zadania.",
        )));
    }
    entries
}

/// One entry of the form, as the server described it.
fn entry_of(offer: &TaskActionOffer, blocked: Option<&str>) -> StepEntry {
    let mut item = ChooserItem::new(
        offer.label.clone().unwrap_or_default(),
        offer.description.clone().unwrap_or_default(),
    );
    if offer.irreversible {
        item = item.irreversible();
    }
    for field in &offer.fields {
        item = item.with_field(field_of(field));
    }
    match offer.kind {
        TaskActionKind::Submenu => StepEntry::submenu(
            item,
            drawn(&offer.children)
                .map(|child| entry_of(child, blocked))
                .collect(),
        ),
        _ => {
            if let Some(reason) = blocked {
                item = item.unavailable(reason);
            }
            StepEntry::leaf(item)
        }
    }
}

/// A field of the form, as the server described it.
fn field_of(field: &narsor_protocol::controller::TaskActionField) -> ChooserField {
    let made = match field.kind {
        TaskActionFieldKind::Multiline => ChooserField::multiline(field.name.clone()),
        TaskActionFieldKind::Select => {
            ChooserField::select(field.name.clone(), field.options.clone())
        }
        TaskActionFieldKind::Text | TaskActionFieldKind::Unrecognized => {
            ChooserField::text(field.name.clone())
        }
    };
    let made = match &field.hint {
        Some(hint) => made.described(hint.clone()),
        None => made,
    };
    if field.required {
        made.required()
    } else {
        made
    }
}

/// The links of the task that have an address.
#[derive(Debug, Clone)]
pub struct LinkList {
    task_id: String,
    links: Vec<LinkRequest>,
    chooser: Chooser,
}

/// The field a comment is typed into.
#[derive(Debug, Clone)]
pub struct CommentForm {
    task_id: String,
    chooser: Chooser,
}

/// What sits over the screen, taking the keys.
#[derive(Debug, Clone)]
pub enum Overlay {
    /// The action panel (`Ctrl+.`).
    Actions(Box<ActionPanel>),
    /// The links of the task (`o`).
    Links(LinkList),
    /// The installation's default harness (`h`); about no task.
    Harness(HarnessList),
    /// The limit of tasks processed at once (`L`); about no task.
    Limit(LimitList),
    /// The text of a comment (`c`).
    Comment(CommentForm),
    /// The `y` / `n` window.
    Confirm(Confirmation),
}

impl Overlay {
    /// Whether it is about a task of the list: one that left the list takes it
    /// away. The window that removes a repository is about none.
    pub fn is_task_bound(&self) -> bool {
        match self {
            Self::Harness(_) | Self::Limit(_) => false,
            Self::Confirm(confirmation) => {
                !matches!(confirmation.confirmed, Confirmed::RemoveRepository(_))
            }
            _ => true,
        }
    }

    /// The task it is about; the harness list is about none.
    pub fn task_id(&self) -> Option<&str> {
        match self {
            Self::Actions(panel) => Some(&panel.task_id),
            Self::Links(list) => Some(&list.task_id),
            Self::Comment(form) => Some(&form.task_id),
            Self::Harness(_) | Self::Limit(_) => None,
            Self::Confirm(confirmation) => Some(&confirmation.task_id),
        }
    }

    /// The list it shows, when it is one; the action panel's open level.
    pub fn chooser(&self) -> Option<&Chooser> {
        match self {
            Self::Actions(panel) => Some(panel.form.chooser()),
            Self::Links(list) => Some(&list.chooser),
            Self::Comment(form) => Some(&form.chooser),
            Self::Harness(list) => Some(list.chooser()),
            Self::Limit(list) => Some(list.chooser()),
            Self::Confirm(_) => None,
        }
    }

    /// The action panel, when it is open.
    pub fn actions(&self) -> Option<&ActionPanel> {
        match self {
            Self::Actions(panel) => Some(panel),
            _ => None,
        }
    }

    /// The window, when it is one.
    pub fn confirmation(&self) -> Option<&Confirmation> {
        match self {
            Self::Confirm(confirmation) => Some(confirmation),
            _ => None,
        }
    }

    /// The keys that apply while it is open.
    pub fn context(&self) -> Context {
        match self {
            Self::Actions(panel) => panel.context(),
            Self::Confirm(_) => Context::Confirm,
            Self::Links(list) => chooser_context(&list.chooser),
            Self::Comment(form) => chooser_context(&form.chooser),
            Self::Harness(list) => chooser_context(list.chooser()),
            Self::Limit(list) => chooser_context(list.chooser()),
        }
    }
}

/// A list typed into is a form (`q` and `?` are text); otherwise it is the
/// panel's keys.
pub(super) fn chooser_context(chooser: &Chooser) -> Context {
    if chooser.is_confirming() {
        Context::FormConfirm
    } else if chooser.is_typing() {
        Context::Form
    } else {
        chooser.context()
    }
}

// ---------------------------------------------------------------------
// Building the lists
// ---------------------------------------------------------------------

/// What a comment does to `task`, in words: the server says, by the way the
/// task is stopped.
fn comment_effect(task: &TaskSummary) -> String {
    offer_of(task, TaskActionId::Comment)
        .and_then(|offer| offer.effect.clone())
        .unwrap_or_else(|| "Wysyła tekst do agenta.".to_string())
}

/// The label of action 0: the way to write to the agent from any wait.
pub(super) const WRITE_TO_AGENT: &str = "Napisz do agenta";

/// Action 0, as an entry of a list that stands alone of any panel (KTD3).
///
/// The server says whether it is available and what it does. With `field`,
/// the entry holds the field the text is typed into; without it, the caller
/// has a field of its own for the text.
pub(super) fn write_to_agent_item(task: &TaskSummary, field: bool) -> ChooserItem {
    let item = ChooserItem::new(WRITE_TO_AGENT, comment_effect(task));
    let item = match availability(task, TaskAction::Comment) {
        Availability::Unavailable(reason) => item.unavailable(reason),
        Availability::Available => item,
    };
    if field {
        item.with_field(ChooserField::multiline(COMMENT_FIELD).required())
    } else {
        item
    }
}

/// The text a submission of [`write_to_agent_item`] carries.
pub(super) fn written_text(submission: &Submission) -> String {
    submission
        .values
        .iter()
        .find(|(field, _)| field == COMMENT_FIELD)
        .map(|(_, value)| value.trim().to_string())
        .unwrap_or_default()
}

/// The field of a comment.
fn comment_form(task: &TaskSummary, unsent: Option<&str>) -> CommentForm {
    let mut item = write_to_agent_item(task, true);
    if let (Some(text), Some(field)) = (unsent, item.fields.first_mut()) {
        *field = std::mem::replace(field, ChooserField::text("")).with_value(text);
    }
    CommentForm {
        task_id: task.task_id.to_string(),
        chooser: Chooser::new(vec![item]),
    }
}

/// The name of the field a comment is typed into: the one of the panel's
/// action form, so the two chooser forms agree.
pub(super) use super::panel::input::COMMENT_FIELD;

fn link_list(task: &TaskSummary) -> LinkList {
    let mut links = Vec::new();
    let mut items = Vec::new();
    for (label, text, url) in openable_links(task) {
        items.push(ChooserItem::new(
            format!("{}: {}", one_line(label), one_line(&text)),
            format!("Otwiera w przeglądarce:\n{url}"),
        ));
        links.push(LinkRequest {
            label: one_line(label),
            url,
        });
    }
    LinkList {
        task_id: task.task_id.to_string(),
        links,
        chooser: Chooser::new(items),
    }
}

/// What the bar says when the server accepted `send` for the action
/// `label`.
fn accepted_summary(label: &str, send: &ActionSend) -> String {
    let label = one_line(label);
    match send {
        ActionSend::Owner(_) => format!("„{label}” zlecone"),
        ActionSend::Operator(_) => format!("„{label}” wysłane do runnera"),
        ActionSend::Comment(_) => "komentarz wysłany".to_string(),
    }
}

// ---------------------------------------------------------------------
// The application's part
// ---------------------------------------------------------------------

impl TaskApp {
    /// What sits over the screen, if anything.
    pub fn overlay(&self) -> Option<&Overlay> {
        self.overlay.as_ref()
    }

    /// Why nothing that changes state can be done now, for the panel.
    fn mutation_block(&self) -> Option<String> {
        self.connection
            .mutation_gate(std::time::Instant::now())
            .err()
            .map(|blocked| blocked.reason)
    }

    /// Takes the action panel of the selected task out from over the screen,
    /// for the details to draw it in the place of the conversation's form.
    pub(super) fn take_action_panel(&mut self) -> Option<Box<ActionPanel>> {
        let selected = self.list.selected_id()?.to_string();
        match self.overlay.take() {
            Some(Overlay::Actions(mut panel)) if panel.task_id == selected => {
                panel.in_details = false;
                Some(panel)
            }
            other => {
                self.overlay = other;
                None
            }
        }
    }

    /// Puts the panel [`Self::take_action_panel`] took back over the screen.
    pub(super) fn restore_action_panel(&mut self, panel: Option<Box<ActionPanel>>) {
        if let Some(panel) = panel {
            self.overlay = Some(Overlay::Actions(panel));
        }
    }

    /// Marks the action panel as not drawn yet in this frame.
    pub(super) fn unplace_action_panel(&mut self) {
        if let Some(Overlay::Actions(panel)) = self.overlay.as_mut() {
            panel.in_details = false;
        }
    }

    /// `Ctrl+.`: the action panel of the selected task.
    pub(super) fn open_menu(&mut self) {
        let Some(task) = self.list.selected() else {
            return;
        };
        let blocked = self.mutation_block();
        self.overlay = Some(Overlay::Actions(Box::new(ActionPanel::new(
            task,
            blocked.as_deref(),
        ))));
    }

    /// The action `id` of the server, chosen outside the panel (the answer
    /// panel's form of the task's own actions): the panel opens on it, at
    /// its submenu, its fields or its confirmation.
    pub(super) fn open_action(&mut self, id: TaskActionId) -> Request {
        self.open_menu();
        let Some(Overlay::Actions(panel)) = self.overlay.as_mut() else {
            return Request::None;
        };
        let Some(index) = panel.offers.iter().position(|offer| offer.id == id) else {
            self.overlay = None;
            self.notify(Severity::Warning, format!("akcja: {NOT_OFFERED}"));
            return Request::None;
        };
        let outcome = panel.form.open(index);
        self.on_panel_outcome(outcome)
    }

    /// Keeps what sits over the screen true to the list: a task that left it
    /// takes its window with it; the action panel takes the task's new offer
    /// and keeps where the operator is (R14).
    pub(super) fn sync_overlay(&mut self) {
        let Some(overlay) = &self.overlay else {
            return;
        };
        if !overlay.is_task_bound() {
            return;
        }
        let Some(task) = self
            .list
            .tasks()
            .iter()
            .find(|task| Some(task.task_id.as_str()) == overlay.task_id())
            .cloned()
        else {
            self.overlay = None;
            return;
        };
        let blocked = self.mutation_block();
        if let Some(Overlay::Actions(panel)) = self.overlay.take() {
            let (panel, lost) = panel.rebuilt(&task, blocked.as_deref());
            self.overlay = Some(Overlay::Actions(Box::new(panel)));
            if let Some(label) = lost {
                self.notify(
                    Severity::Warning,
                    format!("„{}”: akcja nie jest już dostępna", one_line(&label)),
                );
            }
        }
    }

    /// A key while something sits over the screen.
    pub(super) fn on_overlay_key(&mut self, key: KeyEvent) -> Request {
        let Some(overlay) = self.overlay.as_ref() else {
            return Request::None;
        };
        let context = overlay.context();
        let typing = context == Context::Form;
        if !typing {
            match keymap::lookup(context, &key) {
                Some(Action::Help) => {
                    self.help_open = true;
                    return Request::None;
                }
                Some(Action::Quit) => {
                    self.on_quit();
                    return Request::None;
                }
                _ => {}
            }
        }
        match overlay {
            Overlay::Actions(_) => self.on_actions_key(key),
            Overlay::Confirm(_) => self.on_confirm_key(&key),
            Overlay::Links(_) | Overlay::Comment(_) => self.on_list_key(key),
            Overlay::Harness(_) => self.on_harness_key(key),
            Overlay::Limit(_) => self.on_limit_key(key),
        }
    }

    /// A key in the action panel: on a choice, the menu's own shortcuts
    /// first (moving in the queue, the link); otherwise the form's.
    fn on_actions_key(&mut self, key: KeyEvent) -> Request {
        let choosing = matches!(
            self.overlay.as_ref().and_then(Overlay::actions),
            Some(panel) if panel.form.stage() == Stage::Choice
        );
        if choosing {
            match keymap::lookup(Context::Menu, &key) {
                Some(Action::MoveUp) => return self.on_move_shortcut(true),
                Some(Action::MoveDown) => return self.on_move_shortcut(false),
                Some(other @ (Action::MoveMode | Action::OpenLink)) => {
                    if let Some(action) = TaskAction::of_key(other) {
                        return self.invoke(action);
                    }
                }
                _ => {}
            }
        }
        let Some(Overlay::Actions(panel)) = self.overlay.as_mut() else {
            return Request::None;
        };
        let outcome = panel.form.on_key(key);
        self.on_panel_outcome(outcome)
    }

    /// What the panel's form said after a key.
    fn on_panel_outcome(&mut self, outcome: SteppedOutcome) -> Request {
        match outcome {
            SteppedOutcome::None => Request::None,
            SteppedOutcome::Blocked(reason) => {
                self.notify(Severity::Warning, reason);
                Request::None
            }
            SteppedOutcome::Cancelled => {
                self.overlay = None;
                Request::None
            }
            SteppedOutcome::Submitted(submission) => {
                let Some(Overlay::Actions(panel)) = self.overlay.as_ref() else {
                    return Request::None;
                };
                if panel.is_link(&submission.path) {
                    return self.invoke(TaskAction::OpenLink);
                }
                let Some(offer) = panel.offer_at(&submission.path).cloned() else {
                    return Request::None;
                };
                self.chosen_action(&offer, &submission.values)
            }
        }
    }

    /// The operator confirmed `offer` with `values`: the command the server
    /// described, sent. The panel closes; a refusal says why on the bar.
    fn chosen_action(&mut self, offer: &TaskActionOffer, values: &[(String, String)]) -> Request {
        let label = offer.label.clone().unwrap_or_default();
        let send = offer
            .send
            .as_ref()
            .unwrap_or(&TaskActionSend::Unrecognized)
            .resolve(values);
        let send = match send {
            Ok(send) => ActionSend::from(send),
            Err(reason) => {
                self.notify(Severity::Warning, format!("{}: {reason}", one_line(&label)));
                return Request::None;
            }
        };
        if !self.mutation_allowed() {
            return Request::None;
        }
        let Some((task_id, generation)) = self
            .overlay
            .as_ref()
            .and_then(Overlay::actions)
            .map(|panel| (panel.task_id.clone(), panel.generation))
        else {
            return Request::None;
        };
        let Some(task) = self
            .list
            .tasks()
            .iter()
            .find(|task| task.task_id.as_str() == task_id)
            .cloned()
        else {
            return Request::None;
        };
        self.overlay = None;
        let summary = accepted_summary(&label, &send);
        Request::Act(Box::new(self.action_request(
            &task,
            generation,
            &label.to_lowercase(),
            summary,
            send,
        )))
    }

    /// `Ctrl+↑` / `Ctrl+↓` inside the panel: moves one place, and closes the
    /// panel once there is a move to make.
    fn on_move_shortcut(&mut self, up: bool) -> Request {
        let request = self.step_move(up);
        if request != Request::None {
            self.overlay = None;
        }
        request
    }

    fn on_confirm_key(&mut self, key: &KeyEvent) -> Request {
        match keymap::lookup(Context::Confirm, key) {
            Some(Action::Accept) => {
                let Some(Overlay::Confirm(confirmation)) = self.overlay.take() else {
                    return Request::None;
                };
                if !self.mutation_allowed() {
                    return Request::None;
                }
                match confirmation.confirmed {
                    Confirmed::Act(request) => Request::Act(request),
                    Confirmed::Answer(request) => Request::Answer(request),
                    Confirmed::RemoveRepository(id) => Request::RemoveRepository(id),
                }
            }
            Some(Action::Cancel) => {
                self.overlay = None;
                Request::None
            }
            _ => Request::None,
        }
    }

    /// A paste while something sits over the screen: only a text field (of
    /// the panel's fields step, of a list) takes it. A choice, a harness list
    /// or a `y` / `n` window ignores it.
    pub(super) fn on_overlay_paste(&mut self, pasted: &str) {
        match &mut self.overlay {
            Some(Overlay::Actions(panel)) => {
                panel.form.paste(pasted);
            }
            Some(Overlay::Links(list)) => {
                list.chooser.paste(pasted);
            }
            Some(Overlay::Comment(form)) => {
                form.chooser.paste(pasted);
            }
            _ => {}
        }
    }

    /// A key in the list of links or the field of a comment: the list's own
    /// keys.
    fn on_list_key(&mut self, key: KeyEvent) -> Request {
        let chooser = match &mut self.overlay {
            Some(Overlay::Links(list)) => &mut list.chooser,
            Some(Overlay::Comment(form)) => &mut form.chooser,
            _ => return Request::None,
        };
        match chooser.on_key(key) {
            ChooserAction::None => Request::None,
            ChooserAction::Cancel => {
                self.overlay = None;
                Request::None
            }
            ChooserAction::Blocked(reason) => {
                self.notify(Severity::Warning, reason);
                Request::None
            }
            ChooserAction::Submit(submission) => match self.overlay.take() {
                Some(Overlay::Links(list)) => match list.links.get(submission.index) {
                    Some(link) => Request::OpenLink(link.clone()),
                    None => Request::None,
                },
                Some(Overlay::Comment(form)) => self.chosen_comment(form, &submission),
                _ => Request::None,
            },
        }
    }

    /// The operator wrote a comment: one step, sent at once. What it does is
    /// the server's rule, told on the bar in the words of the stop.
    fn chosen_comment(&mut self, form: CommentForm, submission: &Submission) -> Request {
        if !self.mutation_allowed() {
            return Request::None;
        }
        let Some(task) = self
            .list
            .tasks()
            .iter()
            .find(|task| task.task_id.as_str() == form.task_id)
            .cloned()
        else {
            return Request::None;
        };
        let text = written_text(submission);
        if text.is_empty() {
            self.notify(
                Severity::Warning,
                "napisz do agenta: wpisz, co agent ma zobaczyć",
            );
            return Request::None;
        }
        self.unsent_messages
            .insert(form.task_id.clone(), text.clone());
        Request::Act(Box::new(self.action_request(
            &task,
            task.generation,
            "komentarz",
            "komentarz wysłany".to_string(),
            ActionSend::Comment(text),
        )))
    }

    /// An action of the client's own or the comment, from a key. Says why on
    /// the bar when it cannot be done; otherwise opens what it needs.
    pub(super) fn invoke(&mut self, action: TaskAction) -> Request {
        let Some(task) = self.list.selected().cloned() else {
            return Request::None;
        };
        if let Availability::Unavailable(reason) = availability(&task, action) {
            self.notify(Severity::Warning, format!("{}: {reason}", verb(action)));
            return Request::None;
        }
        if action.changes_state() && !self.mutation_allowed() {
            return Request::None;
        }
        self.overlay = None;
        match action {
            TaskAction::Move => {
                self.begin_move();
            }
            TaskAction::OpenLink => {
                self.overlay = Some(Overlay::Links(link_list(&task)));
            }
            TaskAction::Comment => {
                let unsent = self.unsent_messages.get(task.task_id.as_str());
                self.overlay = Some(Overlay::Comment(comment_form(
                    &task,
                    unsent.map(String::as_str),
                )));
            }
        }
        Request::None
    }

    /// Opens the window for a chosen action of the answer panel that cannot
    /// be undone, saying what it does.
    pub(super) fn confirm_answer(
        &mut self,
        request: Box<AnswerRequest>,
        heading: String,
        lines: Vec<String>,
    ) {
        self.overlay = Some(Overlay::Confirm(Confirmation::new(
            request.task_id.clone(),
            heading,
            lines,
            Confirmed::Answer(request),
        )));
    }

    /// The request for `send` to `task`, as the operator saw it at
    /// `generation`. A retry of the same decision — the same command to the
    /// same task, after one that got no answer — reuses its key; an answer,
    /// or a refusal, forgets it.
    pub(super) fn action_request(
        &mut self,
        task: &TaskSummary,
        generation: u64,
        verb: &str,
        summary: String,
        send: ActionSend,
    ) -> ActionRequest {
        let key = self
            .action_keys
            .entry(key_signature(task.task_id.as_str(), &send))
            .or_insert_with(idempotency_key)
            .clone();
        ActionRequest {
            task_id: task.task_id.to_string(),
            title: title_of(task),
            generation,
            key,
            verb: verb.to_string(),
            summary,
            send,
        }
    }

    // -----------------------------------------------------------------
    // Sending
    // -----------------------------------------------------------------

    /// Sends an action. An owner command is answered with the task's new
    /// runner state, which the stream puts in the row; an operator command is
    /// answered with "queued", and the runner's verdict follows as an event.
    /// A refusal says why on the bar and reads the tasks again.
    pub(super) async fn act(&mut self, request: ActionRequest) {
        let ActionRequest {
            task_id,
            title,
            generation,
            key,
            verb,
            summary,
            send,
        } = request;
        let signature = key_signature(&task_id, &send);
        let conductor = conductor_command(&send);
        let is_message = matches!(send, ActionSend::Comment(_));
        let had_slot = self.holds_slot(&task_id);
        let result = match send {
            ActionSend::Owner(command) => self
                .client
                .control_command(
                    &task_id,
                    &OwnerCommandRequest {
                        command,
                        idempotency_key: key,
                        expected_generation: Some(generation),
                    },
                )
                .await
                .map(|response| {
                    (
                        None,
                        response.queued_for_slot || response.task.state == TaskState::Queued,
                    )
                }),
            ActionSend::Operator(command) => self
                .client
                .control_operator_command(
                    &task_id,
                    &SubmitOperatorCommandRequest {
                        command,
                        idempotency_key: key,
                        expected_generation: Some(generation),
                    },
                )
                .await
                .map(|response| (Some(response.command), false)),
            ActionSend::Comment(text) => self
                .client
                .control_comment(
                    &task_id,
                    &SubmitCommentRequest {
                        text,
                        idempotency_key: key,
                        expected_generation: Some(generation),
                    },
                )
                .await
                .map(|response| {
                    // Work for a task without a slot resumes it, and the
                    // resume waits when the backend is full.
                    (None, response.task.state == TaskState::Queued)
                }),
        };
        match result {
            Ok((None, queued)) => {
                self.action_keys.remove(&signature);
                if is_message {
                    self.unsent_messages.remove(&task_id);
                }
                let note = if queued { QUEUED_FOR_SLOT } else { "" };
                self.notify(Severity::Success, format!("{title}: {summary}{note}"));
            }
            Ok((Some(command), _)) => {
                self.action_keys.remove(&signature);
                // Work for a task without a slot resumes it, and the resume
                // waits when the backend is full.
                if let Some(sent) = conductor {
                    self.conductor_command_sent(&task_id, sent, command.status);
                }
                let (severity, text) = match command.status {
                    OperatorCommandStatus::Queued => (Severity::Success, summary),
                    OperatorCommandStatus::Applied => {
                        (Severity::Success, format!("{verb} wykonane"))
                    }
                    OperatorCommandStatus::Rejected => (
                        Severity::Error,
                        rejection_text(&verb, command.detail.as_deref()),
                    ),
                };
                let work = command.status != OperatorCommandStatus::Rejected;
                self.list.note_operator_command(&task_id, command);
                self.after_work_check(
                    &task_id,
                    had_slot || !work,
                    severity,
                    format!("{title}: {text}"),
                );
            }
            Err(error) => {
                let retryable = is_transport(&error);
                if !retryable {
                    self.action_keys.remove(&signature);
                }
                self.note_transport(&error);
                let (severity, text) = refusal_notice(&title, &verb, &error);
                if matches!(
                    error,
                    ClientError::Refused { .. } | ClientError::Conflict | ClientError::NotFound
                ) {
                    // The read changes the list; the reason is the last word
                    // on the bar once it is in.
                    self.notify_after_refresh(severity, text);
                } else {
                    self.notify(severity, text);
                }
            }
        }
    }

    /// Whether the task holds a slot of its backend (`true` for a task the
    /// list does not know: nothing to wait for).
    pub(super) fn holds_slot(&self, task_id: &str) -> bool {
        self.list
            .tasks()
            .iter()
            .find(|task| task.task_id.as_str() == task_id)
            .is_none_or(|task| task.holds_slot)
    }

    /// Points the browser at a link.
    pub(super) fn open_link(&mut self, link: LinkRequest) {
        let Some(url) = safe_link_url(&link.url) else {
            self.notify(
                Severity::Error,
                format!("{}: adres linku jest niebezpieczny", link.label),
            );
            return;
        };
        match launch_browser(&url) {
            Ok(()) => self.notify(Severity::Success, format!("otwarto link {}", link.label)),
            Err(error) => self.notify(
                Severity::Error,
                format!(
                    "link {}: nie udało się uruchomić przeglądarki ({error})",
                    link.label
                ),
            ),
        }
    }

    /// Tells the verdict of an operator command on the bar.
    pub(super) fn notify_resolved(&mut self, resolved: &CommandResolved) {
        if resolved.command.as_ref().and_then(conductor_kind).is_some() {
            self.conductor_command_settled(&resolved.task_id);
        }
        let (severity, text) = resolved_notice(resolved);
        self.notify(severity, text);
    }
}

/// What the bar adds when the task waits for a slot after the command.
pub(super) const QUEUED_FOR_SLOT: &str = " — w kolejce, czeka na slot";

/// What the *Dyrygent* tab sent through the shared path of operator commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConductorSent {
    /// A message to the conductor.
    Chat,
    /// An undone decision.
    Undo,
}

/// Which command of the conductor's `command` is, if it is one of the two
/// the tab sends.
fn conductor_kind(command: &OperatorCommand) -> Option<ConductorSent> {
    match command {
        OperatorCommand::ConductorChat { .. } => Some(ConductorSent::Chat),
        OperatorCommand::UndoDecision { .. } => Some(ConductorSent::Undo),
        _ => None,
    }
}

fn conductor_command(send: &ActionSend) -> Option<ConductorSent> {
    match send {
        ActionSend::Operator(command) => conductor_kind(command),
        ActionSend::Owner(_) | ActionSend::Comment(_) => None,
    }
}

/// What a command is called in a verdict: "przerwanie kroku".
fn command_name(command: Option<&OperatorCommand>) -> String {
    match command {
        Some(OperatorCommand::Interrupt) => "przerwanie kroku".to_string(),
        Some(OperatorCommand::StopStep) => "zatrzymanie kroku".to_string(),
        Some(OperatorCommand::SelectStep { target, .. }) => format!("wybór kroku „{target}”"),
        Some(OperatorCommand::Transition {
            target,
            forced: false,
            ..
        }) => format!("przejście do „{}”", one_line(target)),
        Some(OperatorCommand::Transition {
            target,
            forced: true,
            ..
        }) => format!("override do „{}”", one_line(target)),
        Some(OperatorCommand::RestartStep { .. }) => "restart kroku".to_string(),
        Some(OperatorCommand::ConductorChat { .. }) => "wiadomość do dyrygenta".to_string(),
        Some(OperatorCommand::ConfirmProposal { .. } | OperatorCommand::RejectProposal { .. }) => {
            "decyzja o propozycji dyrygenta".to_string()
        }
        Some(OperatorCommand::UndoDecision { .. }) => "cofnięcie decyzji dyrygenta".to_string(),
        // Historical: no longer sent, but still in old event logs.
        Some(OperatorCommand::SetConductorHarness { .. }) => {
            "zmiana harnessu dyrygenta".to_string()
        }
        None => "polecenie operatora".to_string(),
    }
}

/// The verdict of the runner on a command, for the bar.
pub fn resolved_notice(resolved: &CommandResolved) -> (Severity, String) {
    let name = command_name(resolved.command.as_ref());
    match resolved.status {
        OperatorCommandStatus::Applied => {
            let tail = match resolved.command {
                Some(OperatorCommand::Transition { .. }) => " — runner kontynuuje",
                _ => "",
            };
            (
                Severity::Success,
                format!("{}: {name} wykonane{tail}", resolved.title),
            )
        }
        _ => (
            Severity::Error,
            format!(
                "{}: {}",
                resolved.title,
                rejection_text(&name, resolved.detail.as_deref())
            ),
        ),
    }
}

fn rejection_text(name: &str, detail: Option<&str>) -> String {
    let reason = detail
        .map(one_line)
        .filter(|reason| !reason.trim().is_empty())
        .unwrap_or_else(|| "bez podania przyczyny".to_string());
    format!("{name} odrzucone przez runnera — {reason}")
}

/// What the bar says about a command the server refused or could not carry.
fn refusal_notice(title: &str, verb: &str, error: &ClientError) -> (Severity, String) {
    if let ClientError::Refused { code, message } = error {
        if code == "backend_at_capacity" {
            return (
                Severity::Warning,
                format!(
                    "{title}: backend jest pełny — {verb} nie wykonane, zadanie zachowuje stan \
                     ({}); spróbuj, gdy zwolni się slot",
                    one_line(message)
                ),
            );
        }
    }
    if is_transport(error) {
        return (
            Severity::Error,
            format!("{title}: nie wysłano ({verb}) — {}", failure_reason(error)),
        );
    }
    (
        Severity::Error,
        format!("{title}: {verb} odrzucone — {}", refusal_reason(error)),
    )
}

/// Why the server refused an operator command, in words: the early refusals
/// of the control plane by their code, the rest as the server said it.
pub fn refusal_reason(error: &ClientError) -> String {
    let (ClientError::Refused { code, message } | ClientError::Request { code, message }) = error
    else {
        return failure_reason(error);
    };
    let said = one_line(message);
    let known = match code.as_str() {
        "operator_command_refused" => return said,
        "override_reason_required" => "override wymaga powodu",
        "override_not_allowed" => {
            "ten stan nie dopuszcza override (w starym kształcie workflow: allow_manual_override)"
        }
        "illegal_transition" => "to przejście nie jest dozwolone z bieżącego stanu",
        "missing_target" => "brak stanu docelowego przejścia",
        "stale_generation" => "zadanie zmieniło się w międzyczasie",
        "idempotency_key_reused" => "ten klucz został już użyty dla innego polecenia",
        _ => return said,
    };
    if said.trim().is_empty() {
        known.to_string()
    } else {
        format!("{known} ({said})")
    }
}

fn key_signature(task_id: &str, send: &ActionSend) -> String {
    format!("{task_id}:{send:?}")
}

/// Points the system's browser at `url`, without waiting for it.
fn launch_browser(url: &str) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let mut command = if cfg!(target_os = "macos") {
        Command::new("open")
    } else if cfg!(target_os = "windows") {
        let mut command = Command::new("rundll32");
        command.arg("url.dll,FileProtocolHandler");
        command
    } else {
        Command::new("xdg-open")
    };
    let mut child = command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Reaped in the background, so the opener never lingers as a zombie.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

// ---------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------

/// Draws what sits over the screen, if anything, centred in `area`.
pub fn render(frame: &mut Frame, area: Rect, app: &mut TaskApp, styles: &Styles) {
    if app.quit_confirm {
        render_quit_confirmation(frame, area, styles, app.detail().has_drafts());
        return;
    }
    let title = app
        .overlay
        .as_ref()
        .and_then(|overlay| {
            app.list
                .tasks()
                .iter()
                .find(|task| Some(task.task_id.as_str()) == overlay.task_id())
        })
        .map(title_of)
        .unwrap_or_default();
    if let Some(overlay) = app.overlay.as_mut() {
        match overlay {
            Overlay::Confirm(confirmation) => {
                render_confirmation(frame, area, confirmation, styles)
            }
            Overlay::Actions(panel) => {
                // Drawn in the details, in the place of the conversation's
                // form, when the details were drawn; here otherwise.
                if !panel.in_details {
                    render_panel_popup(frame, area, &title, panel, styles);
                }
            }
            Overlay::Links(list) => {
                render_list(
                    frame,
                    area,
                    &format!("Linki · {title}"),
                    &mut list.chooser,
                    styles,
                );
            }
            Overlay::Comment(form) => {
                render_list(
                    frame,
                    area,
                    &format!("Komentarz · {title}"),
                    &mut form.chooser,
                    styles,
                );
            }
            Overlay::Harness(list) => {
                render_list(frame, area, "Domyślny harness", list.chooser_mut(), styles);
            }
            Overlay::Limit(list) => {
                render_list(frame, area, "Limit zadań", list.chooser_mut(), styles);
            }
        }
    }
}

pub fn render_quit_confirmation(frame: &mut Frame, area: Rect, styles: &Styles, has_drafts: bool) {
    let width = area.width.saturating_sub(4).min(52);
    let height = 7.min(area.height.saturating_sub(2));
    let place = popup(area, width, height);
    let inner = framed(frame, place, Role::Problem, "Wyjść z TUI?", styles);
    let mut lines = vec![Line::raw("Naciśnij ponownie q, aby wyjść.")];
    if has_drafts {
        lines.push(Line::raw("Niewysłany szkic zostanie utracony."));
    }
    lines.extend([
        Line::raw(""),
        Line::from(vec![
            Span::styled(" q ", styles.key_cap()),
            Span::raw("wyjdź  "),
            Span::styled("Esc ", styles.key_cap()),
            Span::raw("anuluj"),
        ]),
    ]);
    let padded = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), padded);
}

fn popup(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

/// Clears `place`, draws a frame in `role` with `title` on it and returns
/// what is inside.
fn framed(frame: &mut Frame, place: Rect, role: Role, title: &str, styles: &Styles) -> Rect {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(Styles::FRAME)
        .border_style(styles.of(role))
        .title(format!(" {} ", one_line(title)));
    let inner = block.inner(place);
    frame.render_widget(Clear, place);
    frame.render_widget(block, place);
    inner
}

/// The rows of the action panel's frame and its gap: the panel's own rows
/// come on top.
const PANEL_FRAME_ROWS: u16 = 2;

/// The fewest rows the action panel takes: its heading and two entries, or
/// the message that it has none (R7). Below it, the list scrolls.
pub const PANEL_MIN_ROWS: u16 = 6;

/// The rows the action panel would like for an area `width` columns wide,
/// inside its frame.
pub fn panel_height(panel: &ActionPanel, width: u16) -> u16 {
    let inner = match panel.content {
        PanelContent::Entries => panel.form.preferred_height(width.saturating_sub(4)),
        PanelContent::Nothing | PanelContent::NoData => 3,
    };
    inner.max(PANEL_MIN_ROWS) + PANEL_FRAME_ROWS
}

/// Draws the action panel into `place`: a frame titled with the task, and
/// the form, or the message that there is nothing to choose (R6).
pub fn render_panel(
    frame: &mut Frame,
    place: Rect,
    title: &str,
    panel: &mut ActionPanel,
    styles: &Styles,
) {
    let heading = if title.is_empty() {
        "Akcje".to_string()
    } else {
        format!("Akcje · {title}")
    };
    let inner = framed(frame, place, Role::Focus, &heading, styles);
    let padded = Rect::new(
        inner.x.saturating_add(1),
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );
    let message = match panel.content {
        PanelContent::Entries => None,
        PanelContent::Nothing => Some(NOTHING_TEXT),
        PanelContent::NoData => Some(NO_DATA_TEXT),
    };
    match message {
        Some(text) => {
            let mut keys = Vec::new();
            for (key, label) in keymap::hints(Context::Menu)
                .into_iter()
                .filter(|(_, label)| *label == "zamknij")
            {
                keys.push(Span::styled(format!(" {key} "), styles.key_cap()));
                keys.push(Span::raw(format!("{label} ")));
            }
            let lines = vec![
                Line::styled(text, styles.of(Role::Attention)),
                Line::raw(""),
                Line::from(keys),
            ];
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), padded);
        }
        None => stepped::render(frame, padded, &mut panel.form, styles, true),
    }
}

/// The action panel in the middle of `area`, where the details are not drawn.
fn render_panel_popup(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    panel: &mut ActionPanel,
    styles: &Styles,
) {
    let width = area.width.saturating_sub(4).min(100);
    let height = panel_height(panel, width).min(area.height.saturating_sub(2));
    let place = popup(area, width, height);
    render_panel(frame, place, title, panel, styles);
}

fn render_list(frame: &mut Frame, area: Rect, title: &str, chooser: &mut Chooser, styles: &Styles) {
    let width = area.width.saturating_sub(4).min(100);
    let height = chooser.preferred_height(width.saturating_sub(2)) + 2;
    let place = popup(area, width, height.min(area.height.saturating_sub(2)));
    let inner = framed(frame, place, Role::Focus, title, styles);
    // An overlay has the keys for as long as it is open.
    chooser::render(frame, inner, chooser, styles, true);
}

fn render_confirmation(
    frame: &mut Frame,
    area: Rect,
    confirmation: &Confirmation,
    styles: &Styles,
) {
    let width = area.width.saturating_sub(4).min(72);
    let text_width = usize::from(width.saturating_sub(4)).max(1);
    let rows: usize = confirmation
        .lines
        .iter()
        .map(|line| one_line(line).chars().count().div_ceil(text_width).max(1))
        .sum();
    let height = u16::try_from(rows).unwrap_or(u16::MAX).saturating_add(5);
    let place = popup(area, width, height.min(area.height.saturating_sub(2)));
    let inner = framed(frame, place, Role::Problem, &confirmation.heading, styles);
    let mut lines: Vec<Line> = confirmation
        .lines
        .iter()
        .map(|line| {
            let role = if line.starts_with('!') {
                Role::Problem
            } else {
                Role::Calm
            };
            Line::styled(one_line(line), styles.of(role))
        })
        .collect();
    lines.push(Line::raw(""));
    // Only the window's own keys: what else the bar offers is not the point.
    let mut keys = Vec::new();
    for (key, label) in keymap::hints(Context::Confirm).into_iter().take(2) {
        keys.push(Span::styled(format!(" {key} "), styles.key_cap()));
        keys.push(Span::raw(format!("{label} ")));
    }
    lines.push(Line::from(keys));
    let padded = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), padded);
}
