//! The action panel and the operator's actions (U11, origin §3).
//!
//! `Ctrl+.` (and `.`, for a terminal that does not pass it) opens one panel of
//! everything the operator can do to the selected task. It is the only way to
//! those actions: they have no shortcut of their own. One function,
//! [`availability`], says whether an action can be done to a task *now* and,
//! when it cannot, why — and it only reads what the server offered. The panel
//! greys the entry out and shows the reason; nothing else decides.
//!
//! | action | available when the server says |
//! |---|---|
//! | pause | the task runs: it waits for the end of the step |
//! | stop `!` | the task runs or pauses: the work is cut at once |
//! | free the slot | the task waits for the operator, before the policy stops it |
//! | transition; override `!` | the state has exits (override: see the workflow) |
//! | choose a step | the task is stopped or finished: any step of the workflow |
//! | start again | a script step of a stopped task, an interrupted task, failed provisioning |
//! | archive `!` | not archived yet |
//! | open a link (`o`) | a workspace has a pull request link |
//!
//! A comment is the conversation's, and moving a queued task is `Ctrl+↑` /
//! `Ctrl+↓` (or `m`): neither is in the panel.
//!
//! # Confirmation
//!
//! An action marked `!` always asks first, in the same window: what will
//! happen, then `y` to do it and `n` (or `Esc`) to take it back
//! ([`Confirmation`], keys of [`Context::Confirm`]). The archive of the answer
//! panel (U10) uses the same window, not a second `Enter` of its own.
//!
//! # What goes where
//!
//! * pause, free the slot, start and archive are owner commands
//!   (`POST …/commands`) — the answer is the new runner state, which the stream
//!   shows in the row. Freeing the slot is the pause the wait policy sends,
//!   sent before its time;
//! * stop, the transitions, a step chosen and a script step started again are
//!   operator commands
//!   (`POST …/operator-commands`): the control plane queues them and the
//!   *runner* carries them out, so the answer only says "queued". The verdict
//!   comes as `OperatorCommandResolved` ([`CommandResolved`]) and ends on the
//!   bar, a rejection with the reason the runner gave. After a transition that
//!   does not end the execution the runner goes on by itself; nothing is sent
//!   to resume it.
//!
//! Every send passes [`TaskApp::mutation_allowed`] and carries an
//! [`idempotency_key`] that a retry of the same decision reuses.
//!
//! # For U12 and U13
//!
//! [`Overlay`] is what sits over the screen (menu, a list of exits or links,
//! the confirmation) and [`TaskApp::overlay`] says which. A new confirmation
//! is [`Confirmation::new`] plus [`Confirmed`]; a new action is a
//! [`TaskAction`], an arm of [`availability`] and one of
//! [`TaskApp::invoke`].

use crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::api::ClientError;
use crate::controller_protocol::{
    OwnerCommandRequest, SubmitCommentRequest, SubmitOperatorCommandRequest, TaskSummary,
    TransitionExit,
};
use crate::domain::display::safe_link_url;
use narsor_protocol::controller::{
    OperatorCommand, OperatorCommandStatus, OwnerCommand, RunnerState, StepView, TaskActionId,
    TaskActionOffer, TaskState,
};

use super::super::chooser::{self, Chooser, ChooserAction, ChooserField, ChooserItem, Submission};
use super::super::keymap::{self, Action, Context};
use super::super::scroll::one_line;
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

/// An action of the menu (origin §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskAction {
    /// `Ctrl+↑` / `Ctrl+↓` / `m`: another place in the queue.
    Move,
    /// Pause: the work comes to the end of its step, then the task stops.
    Pause,
    /// Stop: the work is cut at once; the task ends stopped. `!`
    Stop,
    /// Free the slot of a task that waits for the operator.
    ReleaseSlot,
    /// A manual transition, an override among them.
    Transition,
    /// Go on from any step of the workflow.
    SelectStep,
    /// Start a script step or an interrupted task again.
    Restart,
    /// Delete the workload and the storage. `!`
    Archive,
    /// `o`: open a link of the task.
    OpenLink,
    /// `c`: a comment to the agent; the server picks what it does by the
    /// way the task is stopped. Not in the panel: the conversation owns it.
    Comment,
}

impl TaskAction {
    /// Every action of the panel, in its order. A comment and a move in the
    /// queue are not in it: the conversation writes, `Ctrl+↑` / `Ctrl+↓` move.
    pub const ALL: [TaskAction; 8] = [
        TaskAction::Pause,
        TaskAction::Stop,
        TaskAction::ReleaseSlot,
        TaskAction::Transition,
        TaskAction::SelectStep,
        TaskAction::Restart,
        TaskAction::Archive,
        TaskAction::OpenLink,
    ];

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

    /// Whether it always asks `y` / `n` first.
    pub fn needs_confirmation(self) -> bool {
        matches!(self, Self::Stop | Self::Archive)
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

/// Said of an action the server's offer does not cover.
const NO_OFFER: &str = "niedostępne: brak danych z serwera";

/// The offer of the server for `id`, if it made one.
fn offer_of(task: &TaskSummary, id: TaskActionId) -> Option<&TaskActionOffer> {
    task.actions.as_ref()?.iter().find(|offer| offer.id == id)
}

/// What the server says of `id` for `task`: its offer, or that it said
/// nothing (a server from before the offer, or one that does not know the
/// action).
fn offered(task: &TaskSummary, id: TaskActionId) -> Availability {
    let Some(offer) = offer_of(task, id) else {
        return Availability::because(NO_OFFER);
    };
    if offer.available {
        Availability::Available
    } else {
        Availability::because(offer.reason.as_deref().unwrap_or("niedostępne"))
    }
}

/// Whether `action` can be done to `task` now, and if not, why.
///
/// The server decides, on the same rule it checks at the send (R3, R4): this
/// only reads what it offered. Moving and opening a link are the client's own.
/// A pure function of the summary: the menu, the shortcuts and the
/// notifications all ask it, so they cannot disagree. It knows nothing of the
/// connection — that gate is [`TaskApp::mutation_allowed`].
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
        TaskAction::Pause => offered(task, TaskActionId::Pause),
        TaskAction::Stop => offered(task, TaskActionId::Stop),
        TaskAction::ReleaseSlot => offered(task, TaskActionId::ReleaseSlot),
        TaskAction::SelectStep => offered(task, TaskActionId::SelectStep),
        TaskAction::Transition => offered(task, TaskActionId::Transition),
        TaskAction::Restart => offered(task, TaskActionId::Restart),
        TaskAction::Archive => offered(task, TaskActionId::Archive),
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
/// ("pauza: zadanie jest w kolejce").
pub fn verb(action: TaskAction) -> &'static str {
    match action {
        TaskAction::Move => "przesuń",
        TaskAction::Pause => "pauza",
        TaskAction::Stop => "zatrzymanie",
        TaskAction::ReleaseSlot => "zwolnienie slotu",
        TaskAction::SelectStep => "wybór kroku",
        TaskAction::Transition => "przejście",
        TaskAction::Restart => "uruchomienie od nowa",
        TaskAction::Archive => "archiwizacja",
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
    Owner(OwnerCommand),
    /// A command for the runner: interrupt, a transition.
    Operator(OperatorCommand),
    /// A comment to the agent: what it does, the server decides.
    Comment(String),
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
    /// What the action is called, for a refusal ("przerwanie").
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
    /// An action of the menu.
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

/// The menu: an entry per action, over the task it was opened for.
#[derive(Debug, Clone)]
pub struct Menu {
    task_id: String,
    chooser: Chooser,
}

/// The exits of the state, and the override.
#[derive(Debug, Clone)]
pub struct TransitionList {
    task_id: String,
    from_state: String,
    exits: Vec<TransitionExit>,
    chooser: Chooser,
}

/// The steps of the workflow, to go on from one of them.
#[derive(Debug, Clone)]
pub struct StepList {
    task_id: String,
    steps: Vec<StepView>,
    chooser: Chooser,
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
    /// The menu of actions (`.`).
    Menu(Menu),
    /// The exits of the current state, and the override (`t`).
    Transitions(TransitionList),
    /// The steps of the workflow, to choose one (the panel's "wybierz krok").
    Steps(StepList),
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
            Self::Menu(menu) => Some(&menu.task_id),
            Self::Transitions(list) => Some(&list.task_id),
            Self::Steps(list) => Some(&list.task_id),
            Self::Links(list) => Some(&list.task_id),
            Self::Comment(form) => Some(&form.task_id),
            Self::Harness(_) | Self::Limit(_) => None,
            Self::Confirm(confirmation) => Some(&confirmation.task_id),
        }
    }

    /// The list it shows, when it is one.
    pub fn chooser(&self) -> Option<&Chooser> {
        match self {
            Self::Menu(menu) => Some(&menu.chooser),
            Self::Transitions(list) => Some(&list.chooser),
            Self::Steps(list) => Some(&list.chooser),
            Self::Links(list) => Some(&list.chooser),
            Self::Comment(form) => Some(&form.chooser),
            Self::Harness(list) => Some(list.chooser()),
            Self::Limit(list) => Some(list.chooser()),
            Self::Confirm(_) => None,
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
            Self::Menu(_) => Context::Menu,
            Self::Confirm(_) => Context::Confirm,
            Self::Transitions(list) => chooser_context(&list.chooser),
            Self::Steps(list) => chooser_context(&list.chooser),
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

/// What the menu says an action does.
fn menu_text(task: &TaskSummary, action: TaskAction) -> (String, String) {
    let (name, effect) = match action {
        TaskAction::Move => (
            "przesuń w kolejce".to_string(),
            "Zmienia miejsce zadania w kolejce. `Ctrl+↑` / `Ctrl+↓` przesuwają o jedno miejsce wyżej / \
             niżej, `m` włącza tryb przesuwania: `j` / `k` wybiera miejsce z podglądem, `Enter` \
             zatwierdza, `Esc` anuluje."
                .to_string(),
        ),
        TaskAction::Pause => (
            "pauza".to_string(),
            "Czeka, aż trwająca praca skończy krok. Potem zadanie staje w bezpiecznym punkcie \
             i oddaje zasoby obliczeniowe; storage zostaje."
                .to_string(),
        ),
        TaskAction::Stop => (
            "stop !".to_string(),
            "Zrywa trwającą pracę od razu. Zadanie staje w bezpiecznym punkcie i oddaje zasoby \
             obliczeniowe; sesja agenta zostaje. Wymaga potwierdzenia."
                .to_string(),
        ),
        TaskAction::ReleaseSlot => (
            "zwolnij slot".to_string(),
            "Zadanie czeka na Ciebie. Ta akcja zatrzymuje je teraz, a nie po czasie \
             oczekiwania. Slot staje się wolny, a zadanie można potem wznowić."
                .to_string(),
        ),
        TaskAction::SelectStep => (
            "wybierz krok".to_string(),
            "Wznawia zadanie od dowolnego kroku workflow, także po jego zakończeniu. Dla kroku \
             agenta można dopisać komentarz."
                .to_string(),
        ),
        TaskAction::Transition => (
            "przejście ręczne".to_string(),
            "Przenosi execution do innego stanu. Lista pokazuje wyjścia bieżącego stanu z \
             opisami; override — przejście do dowolnego stanu — jest osobną pozycją `!`, wymaga \
             powodu i trafia do audytu."
                .to_string(),
        ),
        TaskAction::Restart => (
            "uruchom od nowa".to_string(),
            "Uruchamia od początku krok skryptu zatrzymanego zadania, przerwane zadanie albo \
             ponawia nieudany provisioning na tym samym storage."
                .to_string(),
        ),
        TaskAction::Archive => (
            "archiwizuj !".to_string(),
            "Usuwa workload i storage zadania; zadanie trafia do Historii. Tego nie da się \
             cofnąć. Wymaga potwierdzenia."
                .to_string(),
        ),
        TaskAction::OpenLink => (
            "otwórz link".to_string(),
            "Otwiera w przeglądarce link do PR zadania.".to_string(),
        ),
        TaskAction::Comment => (WRITE_TO_AGENT.to_lowercase(), comment_effect(task)),
    };
    (name, effect)
}

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
/// The server says whether it is available, why not, and what it does. With
/// `field`, the entry holds the field the text is typed into; without it, the
/// caller has a field of its own for the text.
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

/// The entries of the menu for `task`. `blocked` is why nothing that changes
/// state can be done right now (no connection), if that is so.
fn menu_items(task: &TaskSummary, blocked: Option<&str>) -> Vec<ChooserItem> {
    TaskAction::ALL
        .into_iter()
        .map(|action| {
            let (label, description) = menu_text(task, action);
            let item = ChooserItem::new(label, description);
            match availability(task, action) {
                Availability::Unavailable(reason) => item.unavailable(reason),
                Availability::Available => match blocked {
                    Some(reason) if action.changes_state() => item.unavailable(reason),
                    _ => item,
                },
            }
        })
        .collect()
}

fn transition_list(task: &TaskSummary) -> Option<TransitionList> {
    let offer = task.manual_transitions.as_ref()?;
    let mut items: Vec<ChooserItem> = offer
        .exits
        .iter()
        .map(|exit| {
            let description = match &exit.description {
                Some(text) if !text.trim().is_empty() => text.clone(),
                _ => "Workflow nie opisuje tego wyjścia.".to_string(),
            };
            ChooserItem::new(
                format!("{} → {}", exit.label, exit.target),
                format!("{description}\nPrzechodzi do stanu „{}”.", exit.target),
            )
        })
        .collect();
    if offer.has_override() {
        items.push(
            ChooserItem::new(
                "! override — dowolny stan",
                format!(
                    "Wymusza przejście ze stanu „{}” do dowolnego stanu workflow, także poza \
                     deklarowanymi wyjściami. Wymaga powodu; powód i autor trafiają do audytu \
                     zadania. Wymaga potwierdzenia.",
                    offer.from_state
                ),
            )
            .with_field(
                ChooserField::select("stan docelowy", offer.override_targets.clone()).required(),
            )
            .with_field(
                ChooserField::text("powód")
                    .required()
                    .described("trafia do audytu"),
            ),
        );
    }
    Some(TransitionList {
        task_id: task.task_id.to_string(),
        from_state: offer.from_state.clone(),
        exits: offer.exits.clone(),
        chooser: Chooser::new(items),
    })
}

/// The steps the server gave, each with the field of a comment when the step
/// is the agent's. The client does not read the workflow: the kind is the
/// server's word.
fn step_list(task: &TaskSummary) -> Option<StepList> {
    let steps = task.steps.clone()?;
    let items =
        steps
            .iter()
            .map(|step| {
                let item = ChooserItem::new(
                    one_line(&step.label),
                    format!("Wznawia zadanie od kroku „{}”.", one_line(&step.id)),
                );
                if step.kind.runs_agent() {
                    item.with_field(ChooserField::multiline(COMMENT_FIELD).described(
                        "komentarz dla agenta, opcjonalny; nie zostaje na dalsze przebiegi",
                    ))
                } else {
                    item
                }
            })
            .collect();
    Some(StepList {
        task_id: task.task_id.to_string(),
        steps,
        chooser: Chooser::new(items),
    })
}

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

/// What a confirmed interrupt or archive of `task` does, in words.
fn effect_lines(task: &TaskSummary, action: TaskAction) -> Vec<String> {
    let title = title_of(task);
    match action {
        TaskAction::Stop => {
            let step = task
                .runner
                .as_ref()
                .and_then(|runner| runner.execution_cursor.current_state.as_deref())
                .map(one_line)
                .filter(|step| !step.is_empty());
            vec![
                match step {
                    Some(step) => format!("Zrywa pracę w kroku „{step}” zadania {title}."),
                    None => format!("Zrywa pracę zadania {title}."),
                },
                "Zadanie staje w bezpiecznym punkcie i oddaje slot; sesja agenta zostaje."
                    .to_string(),
                "Skrypt zacznie od początku kroku. Polecenie trafia do runnera; wynik pojawi się \
                 na pasku."
                    .to_string(),
            ]
        }
        TaskAction::Archive => vec![
            format!("Usuwa workload i storage zadania {title}."),
            "Zadanie trafia do Historii.".to_string(),
            "! Tego nie da się cofnąć.".to_string(),
        ],
        _ => Vec::new(),
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

    /// Why nothing that changes state can be done now, for the menu.
    fn mutation_block(&self) -> Option<String> {
        self.connection
            .mutation_gate(std::time::Instant::now())
            .err()
            .map(|blocked| blocked.reason)
    }

    /// `.`: the menu of the selected task.
    pub(super) fn open_menu(&mut self) {
        let Some(task) = self.list.selected() else {
            return;
        };
        let blocked = self.mutation_block();
        self.overlay = Some(Overlay::Menu(Menu {
            task_id: task.task_id.to_string(),
            chooser: Chooser::new(menu_items(task, blocked.as_deref())),
        }));
    }

    /// Keeps what sits over the screen true to the list: a task that left it
    /// takes its window with it, the menu is rebuilt for the task's new state.
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
        else {
            self.overlay = None;
            return;
        };
        let blocked = self.mutation_block();
        if let Some(Overlay::Menu(menu)) = &mut self.overlay {
            let selected = menu.chooser.selected();
            menu.chooser = Chooser::new(menu_items(task, blocked.as_deref())).selecting(selected);
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
            Overlay::Menu(_) => self.on_menu_key(&key),
            Overlay::Confirm(_) => self.on_confirm_key(&key),
            Overlay::Transitions(_)
            | Overlay::Steps(_)
            | Overlay::Links(_)
            | Overlay::Comment(_) => self.on_list_key(key),
            Overlay::Harness(_) => self.on_harness_key(key),
            Overlay::Limit(_) => self.on_limit_key(key),
        }
    }

    fn on_menu_key(&mut self, key: &KeyEvent) -> Request {
        let Some(action) = keymap::lookup(Context::Menu, key) else {
            return Request::None;
        };
        let Some(Overlay::Menu(menu)) = &mut self.overlay else {
            return Request::None;
        };
        match action {
            Action::Cancel => self.overlay = None,
            Action::Down => menu.chooser.select(menu.chooser.selected() + 1),
            Action::Up => menu
                .chooser
                .select(menu.chooser.selected().saturating_sub(1)),
            Action::Pick => {
                if let Some(digit) = keymap::digit_of(key) {
                    menu.chooser.select(usize::from(digit) - 1);
                }
            }
            Action::Confirm => {
                let chosen = TaskAction::ALL.get(menu.chooser.selected()).copied();
                if let Some(action) = chosen {
                    return self.invoke(action);
                }
            }
            Action::MoveUp => return self.on_move_shortcut(true),
            Action::MoveDown => return self.on_move_shortcut(false),
            other => {
                if let Some(action) = TaskAction::of_key(other) {
                    return self.invoke(action);
                }
            }
        }
        Request::None
    }

    /// `Ctrl+↑` / `Ctrl+↓` inside the menu: moves one place, and closes the menu once
    /// there is a move to make.
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

    /// A paste while something sits over the screen: only a text field of a
    /// list (the override's reason) takes it, as [`Self::on_list_key`] gives
    /// it the keys. A menu, a harness list or a `y` / `n` window ignores it.
    pub(super) fn on_overlay_paste(&mut self, pasted: &str) {
        match &mut self.overlay {
            Some(Overlay::Transitions(list)) => {
                list.chooser.paste(pasted);
            }
            Some(Overlay::Steps(list)) => {
                list.chooser.paste(pasted);
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

    /// A key in the list of exits or the list of links: the list's own keys.
    fn on_list_key(&mut self, key: KeyEvent) -> Request {
        let chooser = match &mut self.overlay {
            Some(Overlay::Transitions(list)) => &mut list.chooser,
            Some(Overlay::Steps(list)) => &mut list.chooser,
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
                Some(Overlay::Transitions(list)) => self.chosen_transition(list, &submission),
                Some(Overlay::Steps(list)) => self.chosen_step(list, &submission),
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
            "komentarz",
            "komentarz wysłany".to_string(),
            ActionSend::Comment(text),
        )))
    }

    /// The operator chose a step: sent at once, with the comment when the
    /// step is the agent's and one was written.
    fn chosen_step(&mut self, list: StepList, submission: &Submission) -> Request {
        if !self.mutation_allowed() {
            return Request::None;
        }
        let Some(task) = self
            .list
            .tasks()
            .iter()
            .find(|task| task.task_id.as_str() == list.task_id)
            .cloned()
        else {
            return Request::None;
        };
        let Some(step) = list.steps.get(submission.index) else {
            return Request::None;
        };
        let note = step
            .kind
            .runs_agent()
            .then(|| written_text(submission))
            .filter(|text| !text.is_empty());
        let summary = format!("wybór kroku „{}” wysłany do runnera", one_line(&step.id));
        Request::Act(Box::new(self.action_request(
            &task,
            "wybór kroku",
            summary,
            ActionSend::Operator(OperatorCommand::SelectStep {
                target: step.id.clone(),
                note,
            }),
        )))
    }

    /// The operator chose an exit (sent at once) or the override (asks first).
    fn chosen_transition(&mut self, list: TransitionList, submission: &Submission) -> Request {
        if !self.mutation_allowed() {
            return Request::None;
        }
        let Some(task) = self
            .list
            .tasks()
            .iter()
            .find(|task| task.task_id.as_str() == list.task_id)
        else {
            return Request::None;
        };
        let task = task.clone();
        if let Some(exit) = list.exits.get(submission.index) {
            let send = ActionSend::Operator(OperatorCommand::Transition {
                target: exit.target.clone(),
                forced: false,
                reason: None,
            });
            let summary = format!(
                "przejście do „{}” wysłane do runnera",
                one_line(&exit.target)
            );
            return Request::Act(Box::new(self.action_request(
                &task,
                "przejście",
                summary,
                send,
            )));
        }
        // The override: a target and a reason, then the window.
        let value = |name: &str| {
            submission
                .values
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, value)| value.trim().to_string())
        };
        let (Some(target), Some(reason)) = (value("stan docelowy"), value("powód")) else {
            return Request::None;
        };
        let send = ActionSend::Operator(OperatorCommand::Transition {
            target: target.clone(),
            forced: true,
            reason: Some(reason.clone()),
        });
        let summary = format!("override do „{}” wysłany do runnera", one_line(&target));
        let request = self.action_request(&task, "override", summary, send);
        let lines = vec![
            format!(
                "Wymusza przejście zadania {} ze stanu „{}” do „{}”, poza deklarowanymi wyjściami.",
                title_of(&task),
                one_line(&list.from_state),
                one_line(&target)
            ),
            format!("Powód: {}", one_line(&reason)),
            "Powód i autor trafiają do audytu zadania.".to_string(),
        ];
        self.overlay = Some(Overlay::Confirm(Confirmation::new(
            list.task_id,
            "Override przejścia !",
            lines,
            Confirmed::Act(Box::new(request)),
        )));
        Request::None
    }

    /// An action chosen from the list, the details or the menu. Says why on
    /// the bar when it cannot be done; otherwise does it — or opens the list
    /// or the window it needs first.
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
                Request::None
            }
            TaskAction::Pause => self.owner_request(
                &task,
                verb(action),
                "wstrzymanie zlecone",
                OwnerCommand::Pause,
            ),
            // The pause the wait policy sends, sent before its time; the server
            // checks that the task still waits.
            TaskAction::ReleaseSlot => self.owner_request(
                &task,
                verb(action),
                "zwolnienie slotu zlecone",
                OwnerCommand::ReleaseSlot,
            ),
            // A stopped task starts its script step again through the runner;
            // an interrupted one starts anew through the owner's command.
            TaskAction::Restart if task.runner_state == RunnerState::Stopped => {
                Request::Act(Box::new(self.action_request(
                    &task,
                    "uruchomienie od nowa",
                    "uruchomienie kroku od nowa zlecone".to_string(),
                    ActionSend::Operator(OperatorCommand::RestartStep {
                        note: None,
                        failure_key: None,
                    }),
                )))
            }
            TaskAction::Restart => self.owner_request(
                &task,
                "uruchomienie od nowa",
                "uruchomienie od nowa zlecone",
                OwnerCommand::Start,
            ),
            TaskAction::Stop => {
                let request = self.action_request(
                    &task,
                    verb(action),
                    "zatrzymanie wysłane do runnera".to_string(),
                    ActionSend::Operator(OperatorCommand::StopStep),
                );
                self.ask(
                    &task,
                    "Zatrzymaj zadanie !",
                    action,
                    Confirmed::Act(Box::new(request)),
                );
                Request::None
            }
            TaskAction::SelectStep => {
                self.overlay = step_list(&task).map(Overlay::Steps);
                Request::None
            }
            TaskAction::Archive => {
                let request = self.action_request(
                    &task,
                    "archiwizacja",
                    "archiwizacja rozpoczęta".to_string(),
                    ActionSend::Owner(OwnerCommand::Archive),
                );
                self.ask(
                    &task,
                    "Archiwizuj zadanie !",
                    action,
                    Confirmed::Act(Box::new(request)),
                );
                Request::None
            }
            TaskAction::Transition => {
                self.overlay = transition_list(&task).map(Overlay::Transitions);
                Request::None
            }
            TaskAction::OpenLink => {
                self.overlay = Some(Overlay::Links(link_list(&task)));
                Request::None
            }
            TaskAction::Comment => {
                let unsent = self.unsent_messages.get(task.task_id.as_str());
                self.overlay = Some(Overlay::Comment(comment_form(
                    &task,
                    unsent.map(String::as_str),
                )));
                Request::None
            }
        }
    }

    fn owner_request(
        &mut self,
        task: &TaskSummary,
        verb: &str,
        summary: &str,
        command: OwnerCommand,
    ) -> Request {
        Request::Act(Box::new(self.action_request(
            task,
            verb,
            summary.to_string(),
            ActionSend::Owner(command),
        )))
    }

    /// Opens the window for `confirmed`, with what `action` does.
    fn ask(&mut self, task: &TaskSummary, heading: &str, action: TaskAction, confirmed: Confirmed) {
        self.overlay = Some(Overlay::Confirm(Confirmation::new(
            task.task_id.to_string(),
            heading,
            effect_lines(task, action),
            confirmed,
        )));
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

    /// The request for `send` to `task`. A retry of the same decision — the
    /// same command to the same task, after one that got no answer — reuses
    /// its key; an answer, or a refusal, forgets it.
    pub(super) fn action_request(
        &mut self,
        task: &TaskSummary,
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
            generation: task.generation,
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
            Overlay::Menu(menu) => render_list(
                frame,
                area,
                &format!("Akcje · {title}"),
                &mut menu.chooser,
                styles,
            ),
            Overlay::Transitions(list) => render_list(
                frame,
                area,
                &format!(
                    "Przejście ręczne · {title} · z „{}”",
                    one_line(&list.from_state)
                ),
                &mut list.chooser,
                styles,
            ),
            Overlay::Steps(list) => render_list(
                frame,
                area,
                &format!("Wybór kroku · {title}"),
                &mut list.chooser,
                styles,
            ),
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
