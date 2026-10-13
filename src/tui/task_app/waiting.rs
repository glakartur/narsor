//! The panel "Czeka na Ciebie" (U10, origin §2): one panel at the bottom of
//! the details that answers every kind of waiting.
//!
//! ```text
//! ┌ ? Pytanie · review_scope · 6m ───────────────────────────────── 1/1 ┐
//! │ „Czy zakres zmian obejmuje też migrację danych?”                    │
//! │ ◆ dyrygent: raczej revise — brak planu migracji                     │
//! │ ┌ Akcje ─────────────────┐ ┌ Opis ─────────────────────────────────┐ │
//! │ │   approved             │ │ ↩ wraca do agenta: plan               │ │
//! │ │ │ revise  ↩          ◆ │ │ Zakres wymaga poprawy…                │ │
//! │ └────────────────────────┘ └───────────────────────────────────────┘ │
//! │ Komentarz dla agenta (wymagane):                                    │
//! │ > Uwzględnij migrację tabeli orders▏                                │
//! └─────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # What the panel is made of
//!
//! The row of the list says *that* a task waits ([`AttentionItem`]); this
//! panel is built from it and from what the details know:
//!
//! * a question, an approval, a problem the step must be settled with — the
//!   form the runner reported with the task ([`InputPanel`]);
//! * a failed step and a proposal of the conductor's — the conductor's own
//!   state, read on demand like the other tabs ([`OperatorPanel`]);
//! * a failed provisioning and a problem of the runner's — the owner commands
//!   `start` and `archive`.
//!
//! Each waiting is an [`Entry`]: its context, a [`Chooser`] holding what the
//! operator typed, and how a chosen action is sent. Entries live as long as the
//! waiting does, so a refresh, a new read of the conductor or a move to another
//! tab never costs the operator a draft; an entry whose waiting went away takes
//! its draft with it into a box the operator can still copy from.
//!
//! # Sending
//!
//! A chosen action becomes an [`AnswerRequest`]: an answer to the pending input
//! (`POST …/input`, naming the request), an operator command (`POST
//! …/operator-commands`) or an owner command (`POST …/commands`) — each with an
//! idempotency key a retry of the same decision reuses. A refusal (a `409`: the
//! task moved on) leaves the panel open with what was typed and reads the
//! tasks again; success returns the focus to the list with a notification.
//!
//! Text from the agent, the conductor and the runner is passed through
//! [`sanitize`] before it is drawn; what the operator typed is sent as typed.

use crossterm::event::KeyEvent;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::api::ClientError;
use crate::conductor::ConductorView;
use crate::controller_protocol::{
    ConductorReadResponse, OwnerCommandRequest, SubmitInputRequest, SubmitOperatorCommandRequest,
    TaskSummary,
};
use narsor_protocol::api::{
    HumanInputDeliveryDto, PendingHumanInputView, Position, PositionStatus,
};
use narsor_protocol::controller::{AttentionItem, AttentionKind, OperatorCommand, OwnerCommand};

use super::super::chooser::{self, Chooser, ChooserAction, ChooserFocus, ChooserItem, Submission};
use super::super::failure_form::OperatorPanel;
use super::super::input_form::{ContextLine, InputPanel, Tone};
use super::super::keymap::{self, Action, Context};
use super::super::scroll::one_line;
use super::super::scroll::{wrap_line, ScrollState};
use super::super::style::{Role, Styles};
use super::detail::short_age;
use super::list::title_of;
use super::notify::Severity;
use super::tabs::conductor;
use super::{failure_reason, idempotency_key, Request, TaskApp};

/// The most rows of the context the panel gives itself before it scrolls.
const MAX_CONTEXT_ROWS: usize = 4;

/// The fewest rows of tab content the panel leaves, with the header and the
/// tab bar.
const KEEP_FOR_CONTENT: u16 = 6;

// ---------------------------------------------------------------------
// What is sent
// ---------------------------------------------------------------------

/// What a chosen action asks the API to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Submit {
    /// An answer to the pending input request `pending_input_id`.
    Input {
        /// The request it answers.
        pending_input_id: String,
        /// The answer.
        delivery: HumanInputDeliveryDto,
    },
    /// A command for the runner: a restart, a confirmation of a proposal.
    Operator(OperatorCommand),
    /// A lifecycle command of the owner: `start`, `archive`.
    Owner(OwnerCommand),
}

/// A chosen action, ready to be sent: [`TaskApp::perform`] carries it out.
#[derive(Debug, Clone, PartialEq)]
pub struct AnswerRequest {
    /// The task.
    pub task_id: String,
    /// The waiting it answers.
    pub waiting_id: String,
    /// The task's title for the notification.
    pub title: String,
    /// The generation the screen shows; a command is refused if the task has
    /// moved on.
    pub generation: u64,
    /// The key a retry of this decision reuses.
    pub key: String,
    /// What the notification says when it worked.
    pub summary: String,
    /// What to send.
    pub submit: Submit,
}

// ---------------------------------------------------------------------
// The entries
// ---------------------------------------------------------------------

/// The owner commands behind a failed provisioning or a problem of the
/// runner's.
#[derive(Debug, Clone)]
struct OwnerPanel {
    context: Vec<ContextLine>,
    items: Vec<ChooserItem>,
    commands: Vec<OwnerCommand>,
}

impl OwnerPanel {
    fn start_item(description: &str) -> ChooserItem {
        ChooserItem::new("Uruchom od nowa", description)
    }

    fn provisioning(task: &TaskSummary) -> Self {
        let mut context = vec![ContextLine::plain(
            "Backend nie przygotował workspace'u; zadanie nie wystartowało.",
        )];
        if let Some(detail) = task
            .observation
            .as_ref()
            .and_then(|observed| observed.summary.detail.as_deref())
        {
            context.push(ContextLine::problem(detail));
        }
        Self {
            context,
            items: vec![
                Self::start_item("Ponawia przygotowanie i start zadania na tym samym storage."),
                ChooserItem::new(
                    "Archiwizuj",
                    "Usuwa workload i storage zadania; zadanie trafia do Historii.",
                )
                .irreversible(),
            ],
            commands: vec![OwnerCommand::Start, OwnerCommand::Archive],
        }
    }

    fn runner_problem(task: &TaskSummary, code: &str) -> Self {
        let message = task
            .runner
            .as_ref()
            .and_then(|runner| runner.problem.as_ref())
            .filter(|problem| problem.code == code)
            .map_or(String::new(), |problem| format!(": {}", problem.message));
        Self {
            context: vec![
                ContextLine::plain("Runner nie może wykonywać pracy."),
                ContextLine::problem(format!("{code}{message}")),
            ],
            items: vec![Self::start_item(
                "Start zadania jest Twoją decyzją o kontynuacji: runner zaczyna od zapisanego stanu.",
            )],
            commands: vec![OwnerCommand::Start],
        }
    }

    fn summary(&self, submission: &Submission) -> String {
        match self.commands.get(submission.index) {
            Some(OwnerCommand::Archive) => "zarchiwizowano",
            _ => "uruchomiono od nowa",
        }
        .to_string()
    }
}

/// How a waiting is answered.
#[derive(Debug, Clone)]
enum Model {
    Input(InputPanel),
    Operator(OperatorPanel),
    Owner(OwnerPanel),
    /// The task row says it waits but the content did not arrive (yet).
    Missing,
}

impl Model {
    fn context(&self) -> Vec<ContextLine> {
        match self {
            Self::Input(panel) => panel.context().to_vec(),
            Self::Operator(panel) => panel.context().to_vec(),
            Self::Owner(panel) => panel.context.clone(),
            Self::Missing => vec![ContextLine::dim(
                "Treść oczekiwania jeszcze nie dotarła; odświeżam dane zadania.",
            )],
        }
    }

    fn items(&self) -> Vec<ChooserItem> {
        match self {
            Self::Input(panel) => panel.items().to_vec(),
            Self::Operator(panel) => panel.items().to_vec(),
            Self::Owner(panel) => panel.items.clone(),
            Self::Missing => vec![ChooserItem::new(
                "treść niedostępna",
                "Runner jeszcze nie przysłał treści tego oczekiwania.",
            )
            .unavailable("czekam na dane zadania")],
        }
    }

    fn selected(&self) -> usize {
        match self {
            Self::Input(panel) => panel.selected(),
            Self::Operator(panel) => panel.selected(),
            Self::Owner(_) | Self::Missing => 0,
        }
    }
}

/// One thing a task waits on, with what the operator typed for it.
#[derive(Debug, Clone)]
struct Entry {
    item: AttentionItem,
    model: Model,
    chooser: Chooser,
    context_scroll: ScrollState,
    /// What the last send found wrong, or the server refused with.
    error: Option<String>,
    /// The key of a send that did not get an answer; a retry of the same
    /// decision reuses it.
    key: Option<String>,
    /// The answer was sent: the typed text is not a draft any more.
    sent: bool,
}

impl Entry {
    /// An entry for `item` of `task`; `positions` are the steps read so far
    /// and `conductor` what the conductor said, once read.
    fn build(
        item: &AttentionItem,
        task: &TaskSummary,
        positions: &[Position],
        conductor: Option<&ConductorView>,
    ) -> Self {
        let model = Self::model(item, task, positions, conductor);
        let chooser = Chooser::new(model.items()).selecting(model.selected());
        Self {
            item: item.clone(),
            model,
            chooser,
            context_scroll: ScrollState::new(),
            error: None,
            key: None,
            sent: false,
        }
    }

    fn model(
        item: &AttentionItem,
        task: &TaskSummary,
        positions: &[Position],
        conductor: Option<&ConductorView>,
    ) -> Model {
        match item.kind {
            AttentionKind::Question | AttentionKind::Approval => task
                .runner
                .as_ref()
                .and_then(|runner| runner.pending_input.as_ref())
                .filter(|pending| pending.id == item.waiting_id)
                .map_or(Model::Missing, |pending| {
                    let state_of = |attempt: &str| {
                        positions
                            .iter()
                            .find(|position| {
                                position.action_execution_id.as_deref() == Some(attempt)
                            })
                            .map(|position| position.state_id.clone())
                    };
                    serde_json::from_value::<PendingHumanInputView>(pending.form.clone())
                        .map_or(Model::Missing, |form| {
                            Model::Input(InputPanel::new(&form, &state_of))
                        })
                }),
            AttentionKind::Problem => {
                let pending = task
                    .runner
                    .as_ref()
                    .and_then(|runner| runner.pending_input.as_ref())
                    .filter(|pending| pending.id == item.waiting_id);
                match pending {
                    Some(pending) => {
                        let state_of = |attempt: &str| {
                            positions
                                .iter()
                                .find(|position| {
                                    position.action_execution_id.as_deref() == Some(attempt)
                                })
                                .map(|position| position.state_id.clone())
                        };
                        serde_json::from_value::<PendingHumanInputView>(pending.form.clone())
                            .map_or(Model::Missing, |form| {
                                Model::Input(InputPanel::new(&form, &state_of))
                            })
                    }
                    None if task
                        .runner
                        .as_ref()
                        .is_some_and(|runner| runner.problem.is_some()) =>
                    {
                        Model::Owner(OwnerPanel::runner_problem(task, &item.waiting_id))
                    }
                    None => Model::Missing,
                }
            }
            AttentionKind::FailedStep => {
                let raw_error = positions
                    .iter()
                    .rev()
                    .filter(|position| Some(&position.state_id) == item.state_id.as_ref())
                    .find(|position| {
                        position.status == PositionStatus::Failed && position.error.is_some()
                    })
                    .and_then(|position| position.error.as_deref());
                Model::Operator(OperatorPanel::failed_step(
                    &item.waiting_id,
                    item.state_id.as_deref(),
                    conductor,
                    raw_error,
                ))
            }
            AttentionKind::ConductorProposal => {
                Model::Operator(OperatorPanel::proposal(&item.waiting_id, conductor))
            }
            AttentionKind::ProvisioningFailed => Model::Owner(OwnerPanel::provisioning(task)),
        }
    }

    /// Builds the model again from what is known now, keeping what was typed.
    fn rebuild(
        &mut self,
        task: &TaskSummary,
        positions: &[Position],
        conductor: Option<&ConductorView>,
    ) {
        let model = Self::model(&self.item, task, positions, conductor);
        let chooser = std::mem::replace(&mut self.chooser, Chooser::new(Vec::new()));
        // An operator who has not moved from the opening entry follows the
        // recommendation to wherever the new build puts it.
        let opening = self
            .model
            .items()
            .get(self.model.selected())
            .map(|item| item.label.clone());
        let untouched = chooser.focus() == ChooserFocus::Actions
            && chooser.selected_item().map(|item| &item.label) == opening.as_ref();
        let rebuilt = chooser.rebuilt(model.items());
        self.chooser = if untouched {
            rebuilt.selecting(model.selected())
        } else {
            rebuilt
        };
        self.model = model;
    }

    /// Whether what this entry shows comes from the conductor's state.
    fn wants_conductor(&self) -> bool {
        matches!(
            self.item.kind,
            AttentionKind::FailedStep | AttentionKind::ConductorProposal
        )
    }

    fn is_drafted(&self) -> bool {
        !self.sent && !self.chooser.draft_text().is_empty()
    }

    fn kind_word(&self) -> &'static str {
        match self.item.kind {
            AttentionKind::Question => "Pytanie",
            AttentionKind::Approval => "Approval",
            AttentionKind::Problem => "Problem",
            AttentionKind::FailedStep => "Nieudany krok",
            AttentionKind::ConductorProposal => "Propozycja dyrygenta",
            AttentionKind::ProvisioningFailed => "Nieudany provisioning",
        }
    }

    /// `?` for what asks, `!` for what is wrong.
    fn glyph(&self) -> char {
        match self.item.kind {
            AttentionKind::Question
            | AttentionKind::Approval
            | AttentionKind::ConductorProposal => '?',
            AttentionKind::Problem
            | AttentionKind::FailedStep
            | AttentionKind::ProvisioningFailed => '!',
        }
    }

    fn role(&self) -> Role {
        if self.glyph() == '?' {
            Role::Attention
        } else {
            Role::Problem
        }
    }

    /// The lines under the context: what went wrong.
    fn notices(&self) -> Vec<(Role, String)> {
        let mut notices = Vec::new();
        if let Some(error) = &self.error {
            notices.push((Role::Problem, format!("✗ {}", one_line(error))));
        }
        notices
    }
}

/// What a waiting left behind when it went away with a draft.
///
/// The entry itself is kept: should the same waiting come back — a task
/// resumed after its runner was lost asks the same question again, under
/// the same id — the draft goes back to it ([`WaitingPanel::sync`]).
#[derive(Debug, Clone)]
pub struct Released {
    /// What it was.
    pub title: String,
    /// What the operator had typed.
    pub text: String,
    /// The waiting it was typed for.
    pub waiting_id: String,
    entry: Box<Entry>,
}

/// What a key in the panel asks of the application.
#[derive(Debug, Clone, PartialEq)]
pub enum PanelEffect {
    /// The key was folded into the panel.
    None,
    /// Collapse the panel.
    Collapse,
    /// Say something on the bar.
    Notice(String),
    /// Send the chosen action.
    Submit(Submission),
    /// The chosen action cannot be undone: ask in the `y` / `n` window first.
    Confirm(Submission),
}

/// What a read of the panel's entries changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncEffect {
    /// The entry the operator was in went away.
    pub lost_current: bool,
    /// A draft was left behind.
    pub released: bool,
}

/// The panel of one task: every waiting, the one shown, and the drafts.
#[derive(Debug, Clone, Default)]
pub struct WaitingPanel {
    entries: Vec<Entry>,
    current: usize,
    collapsed: bool,
    released: Vec<Released>,
    conductor: Option<ConductorView>,
    conductor_revision: Option<u64>,
}

impl WaitingPanel {
    /// Whether there is anything to show: a waiting or a draft left behind.
    pub fn is_present(&self) -> bool {
        !self.entries.is_empty() || !self.released.is_empty()
    }

    /// How many things wait.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing waits.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The index (0-based) of the waiting shown.
    pub fn current_index(&self) -> usize {
        self.current
    }

    /// Whether the panel is folded to its one line.
    pub fn is_collapsed(&self) -> bool {
        self.collapsed
    }

    /// The identity of the waiting shown.
    pub fn current_waiting_id(&self) -> Option<&str> {
        self.entries
            .get(self.current)
            .map(|entry| entry.item.waiting_id.as_str())
    }

    /// The chooser of the waiting shown.
    pub fn chooser(&self) -> Option<&Chooser> {
        self.entries.get(self.current).map(|entry| &entry.chooser)
    }

    /// What the last send of the waiting shown found wrong.
    pub fn error(&self) -> Option<&str> {
        self.entries
            .get(self.current)
            .and_then(|entry| entry.error.as_deref())
    }

    /// The drafts of waitings that went away.
    pub fn released(&self) -> &[Released] {
        &self.released
    }

    /// Whether keys are typed into a field: `q` and `?` are text then.
    pub fn is_typing(&self) -> bool {
        self.entries
            .get(self.current)
            .is_some_and(|entry| entry.chooser.is_typing())
    }

    /// Unfolds the panel.
    pub fn expand(&mut self) {
        self.collapsed = false;
    }

    /// What the operator wrote and has not sent, for the quit question and
    /// for the copy after a waiting went away.
    pub fn draft_text(&self) -> String {
        let mut parts: Vec<String> = self
            .entries
            .iter()
            .filter(|entry| entry.is_drafted())
            .map(|entry| entry.chooser.draft_text())
            .collect();
        parts.extend(self.released.iter().map(|released| released.text.clone()));
        parts.join("\n")
    }

    /// Whether the conductor's state has to be read for `revision` of the
    /// row: a failed step and a proposal are made of it.
    pub fn needs_conductor(&self, revision: u64) -> bool {
        self.entries.iter().any(Entry::wants_conductor) && self.conductor_revision != Some(revision)
    }

    /// Makes the conductor's state due for a new read, keeping what is shown.
    pub fn invalidate(&mut self) {
        self.conductor_revision = None;
    }

    /// Takes a read of the conductor's state in. A failed read keeps what the
    /// panel has; the entries are rebuilt, keeping what was typed.
    pub fn apply_conductor(
        &mut self,
        task: &TaskSummary,
        positions: &[Position],
        revision: u64,
        result: Result<ConductorReadResponse, String>,
    ) {
        self.conductor_revision = Some(revision);
        match result {
            Ok(ConductorReadResponse::Available { conductor, .. }) => {
                self.conductor = Some(*conductor);
            }
            Ok(ConductorReadResponse::Unavailable { message, .. }) => {
                self.conductor = Some(ConductorView::unavailable(message));
            }
            Err(_) => return,
        }
        let conductor = self.conductor.as_ref();
        for entry in self
            .entries
            .iter_mut()
            .filter(|entry| entry.wants_conductor())
        {
            entry.rebuild(task, positions, conductor);
        }
    }

    /// Builds every entry again from what is known now — the steps read since
    /// change what an output returns to — keeping what was typed.
    pub fn reread(&mut self, task: &TaskSummary, positions: &[Position]) {
        let conductor = self.conductor.as_ref();
        for entry in &mut self.entries {
            entry.rebuild(task, positions, conductor);
        }
    }

    /// Brings the entries in line with what `task` waits on: new waitings get
    /// an entry, the typed text of the others stays, and a waiting that went
    /// away takes its draft to [`Self::released`] — from where it comes back
    /// to its entry, in the entry's place, when the same waiting returns
    /// (R8). Neither moves the entry shown.
    pub fn sync(&mut self, task: &TaskSummary, positions: &[Position]) -> SyncEffect {
        let mut effect = SyncEffect::default();
        let shown = self.current_waiting_id().map(str::to_string);
        let mut old = std::mem::take(&mut self.entries);
        let mut entries = Vec::with_capacity(task.attention.len());
        for item in &task.attention {
            match old
                .iter()
                .position(|entry| entry.item.waiting_id == item.waiting_id)
            {
                Some(index) => {
                    let mut entry = old.remove(index);
                    if matches!(entry.model, Model::Missing) {
                        entry.rebuild(task, positions, self.conductor.as_ref());
                    }
                    entries.push(entry);
                }
                None => {
                    let parked = self
                        .released
                        .iter()
                        .position(|released| released.waiting_id == item.waiting_id)
                        .map(|index| self.released.remove(index));
                    let entry = match parked {
                        Some(released) => {
                            let mut entry = *released.entry;
                            entry.item = item.clone();
                            entry.error = None;
                            // Until the waiting's content arrives again, the
                            // entry keeps what it was typed into.
                            let conductor = self.conductor.as_ref();
                            let model = Entry::model(item, task, positions, conductor);
                            if !matches!(model, Model::Missing) {
                                entry.rebuild(task, positions, conductor);
                            }
                            entry
                        }
                        None => Entry::build(item, task, positions, self.conductor.as_ref()),
                    };
                    entries.push(entry);
                }
            }
        }
        for gone in old {
            if shown.as_deref() == Some(gone.item.waiting_id.as_str()) {
                effect.lost_current = true;
            }
            if gone.is_drafted() {
                self.released.push(Released {
                    title: format!(
                        "{} · {}",
                        gone.kind_word(),
                        gone.item
                            .state_id
                            .as_deref()
                            .map_or_else(String::new, one_line)
                    ),
                    text: gone.chooser.draft_text(),
                    waiting_id: gone.item.waiting_id.clone(),
                    entry: Box::new(gone),
                });
                effect.released = true;
            }
        }
        self.entries = entries;
        self.current = shown
            .and_then(|id| {
                self.entries
                    .iter()
                    .position(|entry| entry.item.waiting_id == id)
            })
            .unwrap_or(0);
        effect
    }

    /// Folds a key into the panel. Keys that are the application's — `q`,
    /// `?` — are not here: the caller handles them first when nothing is
    /// being typed.
    pub fn on_key(&mut self, key: KeyEvent) -> PanelEffect {
        if self.entries.is_empty() {
            return match keymap::lookup(Context::Panel, &key) {
                Some(Action::Cancel) => {
                    self.released.clear();
                    PanelEffect::Collapse
                }
                _ => PanelEffect::None,
            };
        }
        let typing = self.is_typing();
        if !typing {
            match keymap::lookup(Context::Panel, &key) {
                Some(Action::PrevPending) => return self.step(-1),
                Some(Action::NextPending) => return self.step(1),
                Some(Action::Top) => {
                    self.entries[self.current].context_scroll.to_top();
                    return PanelEffect::None;
                }
                Some(Action::Bottom) => {
                    // The row count is only known to the drawing, which clamps.
                    self.entries[self.current]
                        .context_scroll
                        .set_offset(usize::MAX, usize::MAX);
                    return PanelEffect::None;
                }
                _ => {}
            }
        }
        let entry = &mut self.entries[self.current];
        let action = entry.chooser.on_key(key);
        match action {
            ChooserAction::None => {
                entry.sent = false;
                entry.key = None;
                entry.error = None;
                PanelEffect::None
            }
            ChooserAction::Cancel => PanelEffect::Collapse,
            ChooserAction::Blocked(reason) => PanelEffect::Notice(reason),
            ChooserAction::Submit(submission) => {
                let irreversible = entry
                    .chooser
                    .items()
                    .get(submission.index)
                    .is_some_and(|item| item.irreversible);
                if irreversible {
                    PanelEffect::Confirm(submission)
                } else {
                    PanelEffect::Submit(submission)
                }
            }
        }
    }

    fn step(&mut self, by: isize) -> PanelEffect {
        let count = self.entries.len();
        self.current = (self.current as isize + by).rem_euclid(count as isize) as usize;
        PanelEffect::None
    }

    /// The request for a chosen action of the waiting shown.
    pub fn prepare(
        &mut self,
        task: &TaskSummary,
        submission: &Submission,
    ) -> Result<AnswerRequest, String> {
        let entry = self
            .entries
            .get_mut(self.current)
            .ok_or_else(|| "Nie ma czego wysłać.".to_string())?;
        let (submit, summary) = match &entry.model {
            Model::Input(panel) => (
                Submit::Input {
                    pending_input_id: entry.item.waiting_id.clone(),
                    delivery: panel.delivery(submission)?,
                },
                panel.summary(submission),
            ),
            Model::Operator(panel) => (
                Submit::Operator(
                    panel
                        .command(submission)
                        .ok_or_else(|| "Ta akcja nie istnieje.".to_string())?,
                ),
                panel.summary(submission),
            ),
            Model::Owner(panel) => (
                Submit::Owner(
                    *panel
                        .commands
                        .get(submission.index)
                        .ok_or_else(|| "Ta akcja nie istnieje.".to_string())?,
                ),
                panel.summary(submission),
            ),
            Model::Missing => return Err("Treść oczekiwania jeszcze nie dotarła.".to_string()),
        };
        let key = entry.key.get_or_insert_with(idempotency_key).clone();
        Ok(AnswerRequest {
            task_id: task.task_id.to_string(),
            waiting_id: entry.item.waiting_id.clone(),
            title: title_of(task),
            generation: task.generation,
            key,
            summary,
            submit,
        })
    }

    /// Says what a send of `waiting_id` came to. Success forgets the draft; a
    /// refusal keeps the panel as it is and shows why; a lost connection keeps
    /// the key for the retry.
    fn settle(&mut self, waiting_id: &str, outcome: Result<(), (String, bool)>) {
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.item.waiting_id == waiting_id)
        else {
            return;
        };
        match outcome {
            Ok(()) => {
                entry.sent = true;
                entry.key = None;
                entry.error = None;
            }
            Err((reason, retryable)) => {
                if !retryable {
                    entry.key = None;
                }
                entry.error = Some(reason);
            }
        }
    }

    // -----------------------------------------------------------------
    // Drawing
    // -----------------------------------------------------------------

    /// The rows the panel would like in a details area of `area_height` rows
    /// and `width` columns; none when nothing waits.
    pub fn preferred_height(&self, width: u16, area_height: u16) -> u16 {
        let cap = area_height.saturating_sub(KEEP_FOR_CONTENT);
        let wanted = if let Some(entry) = self.entries.get(self.current) {
            if self.collapsed {
                1
            } else {
                let inner = width.saturating_sub(2);
                let context = context_rows(&entry.model.context(), usize::from(inner)).len();
                2 + context.min(MAX_CONTEXT_ROWS) as u16
                    + entry.notices().len() as u16
                    + entry.chooser.preferred_height(inner)
            }
        } else if self.released.is_empty() {
            0
        } else {
            let lines: usize = self
                .released
                .iter()
                .map(|released| 1 + released.text.lines().count().max(1))
                .sum();
            2 + lines.min(5) as u16
        };
        wanted.min(cap)
    }

    /// Draws the panel into `area`. `as_of` is the server's now, which the age of
    /// the shown waiting is counted to.
    ///
    /// The frame is always in the entry's role, focus or not (R4): whether
    /// the keys are here shows on the frame of the details around it, and on
    /// the bracket of the actions — in the focus role while `focused` says the
    /// panel has the keys and they are on the actions, in the background role
    /// otherwise (R12). The actions
    /// scroll inside what the context and the fields leave them (R17).
    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        styles: &Styles,
        stale: bool,
        focused: bool,
        as_of: chrono::DateTime<chrono::Utc>,
    ) {
        if area.height == 0 || area.width < 4 {
            return;
        }
        if self.entries.is_empty() {
            self.render_released(frame, area, styles);
            return;
        }
        let count = self.entries.len();
        let current = self.current;
        let collapsed = self.collapsed;
        let entry = &mut self.entries[current];
        let age = short_age((as_of - entry.item.since).num_seconds());
        let state = entry
            .item
            .state_id
            .as_deref()
            .map_or_else(String::new, |state| format!(" · {}", one_line(state)));
        let role = if stale { Role::Done } else { entry.role() };
        let mut heading = format!("{} {}{state} · {age}", entry.glyph(), entry.kind_word());
        if collapsed {
            if count > 1 {
                heading.push_str(&format!(" · {}/{count}", current + 1));
            }
            let line = Line::from(vec![
                Span::styled(format!("▸ {heading}"), styles.of(role)),
                Span::styled("  Tab rozwiń", styles.of(Role::Done)),
            ]);
            frame.render_widget(Paragraph::new(line), area);
            return;
        }

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(styles.of(role))
            .title(Line::styled(format!(" {heading} "), styles.of(role)))
            .title(
                Line::styled(
                    if count > 1 {
                        format!(" {}/{count} ", current + 1)
                    } else {
                        String::new()
                    },
                    styles.of(role),
                )
                .right_aligned(),
            );
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let rows = context_rows(&entry.model.context(), usize::from(inner.width));
        let notices = entry.notices();
        let notice_height = (notices.len() as u16).min(inner.height.saturating_sub(5));
        // The actions come first: the context takes what they leave.
        let context_height = rows
            .len()
            .min(MAX_CONTEXT_ROWS)
            .min(usize::from(inner.height.saturating_sub(notice_height + 5)));
        let [context_area, notice_area, chooser_area] = Layout::vertical([
            Constraint::Length(context_height as u16),
            Constraint::Length(notice_height),
            Constraint::Min(0),
        ])
        .areas(inner);

        render_context(
            frame,
            context_area,
            &rows,
            &mut entry.context_scroll,
            styles,
            stale,
        );
        let lines: Vec<Line> = notices
            .iter()
            .map(|(role, text)| Line::styled(text.clone(), styles.of(*role)))
            .collect();
        frame.render_widget(Paragraph::new(lines), notice_area);
        chooser::render(frame, chooser_area, &mut entry.chooser, styles, focused);
    }

    fn render_released(&self, frame: &mut Frame, area: Rect, styles: &Styles) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(styles.of(Role::Attention))
            .title(Line::styled(
                " Szkic bez oczekiwania — skopiuj; Esc go usuwa ",
                styles.of(Role::Attention),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let mut lines = Vec::new();
        for released in &self.released {
            lines.push(Line::styled(
                one_line(&released.title),
                styles.of(Role::Done),
            ));
            for line in released.text.lines() {
                lines.push(Line::raw(one_line(line)));
            }
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

/// The context wrapped to `width` columns, one row per line, each in its
/// tone. Everything in it is somebody else's text, so it is made safe here.
/// A description as lines, each safe for a terminal.
fn one_line_per_row(text: &str) -> Vec<String> {
    text.lines().map(one_line).collect()
}

fn context_rows(lines: &[ContextLine], width: usize) -> Vec<(Tone, String)> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for line in lines {
        for logical in line.text.split('\n') {
            for row in wrap_line(&one_line(logical), width) {
                rows.push((line.tone, row));
            }
        }
    }
    rows
}

fn render_context(
    frame: &mut Frame,
    area: Rect,
    rows: &[(Tone, String)],
    scroll: &mut ScrollState,
    styles: &Styles,
    stale: bool,
) {
    if area.height == 0 {
        return;
    }
    let height = usize::from(area.height);
    scroll.set_viewport(height);
    scroll.clamp(rows.len());
    let first = scroll.offset();
    let more_below = first + height < rows.len();
    let lines: Vec<Line> = rows
        .iter()
        .skip(first)
        .take(height)
        .enumerate()
        .map(|(index, (tone, text))| {
            let role = match tone {
                _ if stale => Role::Done,
                Tone::Plain => Role::Calm,
                Tone::Dim => Role::Done,
                Tone::Conductor => Role::Conductor,
                Tone::Problem => Role::Problem,
            };
            let mut spans = vec![Span::styled(text.clone(), styles.of(role))];
            if more_below && index + 1 == height {
                spans.push(Span::styled(" ▾ g/G przewija", styles.of(Role::Done)));
            }
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Left), area);
}

// ---------------------------------------------------------------------
// The application's part
// ---------------------------------------------------------------------

impl TaskApp {
    /// Whether the panel has the keyboard: the focus is in the details, on
    /// the panel, and there is a panel.
    pub fn panel_focused(&self) -> bool {
        matches!(self.view, super::View::Tasks)
            && self.focus == super::Focus::Details
            && self.panel_focus
            && self.selected_panel().is_some_and(WaitingPanel::is_present)
    }

    fn selected_panel(&self) -> Option<&WaitingPanel> {
        let id = self.list.selected_id()?;
        self.detail.view(id).map(|view| &view.waiting)
    }

    /// Whether a key is typed into a field of the panel.
    pub(super) fn panel_typing(&self) -> bool {
        self.panel_focused() && self.selected_panel().is_some_and(WaitingPanel::is_typing)
    }

    /// `Enter` on the list: the details of the selected task, and — when it
    /// waits — its panel unfolded with the focus on the actions.
    pub(super) fn open_selected(&mut self) {
        let Some(id) = self.list.selected_id().map(str::to_string) else {
            return;
        };
        self.focus = super::Focus::Details;
        self.panel_focus = false;
        self.back_to_axis();
        self.sync_waiting();
        if let Some(view) = self.detail.views.get_mut(&id) {
            if view.waiting.is_present() {
                view.waiting.expand();
                self.panel_focus = true;
            }
        }
    }

    /// `Tab` in the details: to the panel, unfolding it.
    pub(super) fn focus_panel(&mut self) {
        let Some(id) = self.list.selected_id().map(str::to_string) else {
            return;
        };
        match self.detail.views.get_mut(&id) {
            Some(view) if view.waiting.is_present() => {
                view.waiting.expand();
                self.panel_focus = true;
            }
            _ => self.notify(Severity::Warning, "to zadanie na nic nie czeka"),
        }
    }

    /// Brings every task's panel in line with what its row says it waits on,
    /// and the drafts with what the panels hold. Call it after a list was
    /// read, an event was applied, a key edited a draft.
    pub fn sync_waiting(&mut self) {
        let selected = self.list.selected_id().map(str::to_string);
        let mut lost = false;
        let mut released = false;
        let mut drafts = Vec::new();
        for task in self.list.tasks() {
            let id = task.task_id.as_str();
            let present = self
                .detail
                .views
                .get(id)
                .is_some_and(|view| view.waiting.is_present());
            if task.attention.is_empty() && !present {
                continue;
            }
            let view = self.detail.view_mut(id);
            let positions = view.progress().positions().to_vec();
            let effect = view.waiting.sync(task, &positions);
            if selected.as_deref() == Some(id) {
                lost = effect.lost_current;
                released = effect.released;
            }
            drafts.push((id.to_string(), view.draft_text()));
        }
        for (id, text) in drafts {
            self.detail.set_draft(&id, text);
        }
        if lost && self.panel_focus {
            self.panel_focus = false;
            let text = if released {
                "oczekiwanie zniknęło — szkic został w panelu do skopiowania"
            } else {
                "oczekiwanie zniknęło (odpowiedź z innego klienta?)"
            };
            self.notify(Severity::Warning, text);
        }
    }

    /// A key while the panel has the keyboard.
    pub(super) fn on_panel_key(&mut self, key: KeyEvent, confirming_quit: bool) -> Request {
        let Some(id) = self.list.selected_id().map(str::to_string) else {
            return Request::None;
        };
        if !self.panel_typing() {
            match keymap::lookup(Context::Panel, &key) {
                Some(Action::Help) => {
                    self.help_open = true;
                    return Request::None;
                }
                Some(Action::Quit) => {
                    self.on_quit(confirming_quit);
                    return Request::None;
                }
                _ => {}
            }
        }
        let effect = self
            .detail
            .views
            .get_mut(&id)
            .map_or(PanelEffect::None, |view| view.waiting.on_key(key));
        let request = match effect {
            PanelEffect::None => Request::None,
            PanelEffect::Collapse => {
                if let Some(view) = self.detail.views.get_mut(&id) {
                    view.waiting.collapsed = !view.waiting.is_empty();
                }
                self.panel_focus = false;
                Request::None
            }
            PanelEffect::Notice(text) => {
                self.notify(Severity::Warning, text);
                Request::None
            }
            PanelEffect::Submit(submission) => self.submit_answer(&id, &submission),
            PanelEffect::Confirm(submission) => self.ask_to_confirm(&id, &submission),
        };
        if let Some(view) = self.detail.views.get(&id) {
            let text = view.draft_text();
            self.detail.set_draft(&id, text);
        }
        request
    }

    /// An action that cannot be undone: the request is prepared as for any
    /// other, but waits in the confirmation window for `y`.
    fn ask_to_confirm(&mut self, id: &str, submission: &Submission) -> Request {
        let Request::Answer(request) = self.submit_answer(id, submission) else {
            return Request::None;
        };
        let (label, mut lines) = self
            .selected_panel()
            .and_then(WaitingPanel::chooser)
            .and_then(|chooser| chooser.items().get(submission.index))
            .map_or_else(
                || (submission.label.clone(), Vec::new()),
                |item| (item.label.clone(), one_line_per_row(&item.description)),
            );
        lines.push("! Tego nie da się cofnąć.".to_string());
        self.confirm_answer(request, format!("{} !", one_line(&label)), lines);
        Request::None
    }

    /// Turns a chosen action into a request, unless the connection or the
    /// action's own rules say no.
    fn submit_answer(&mut self, id: &str, submission: &Submission) -> Request {
        if !self.mutation_allowed() {
            return Request::None;
        }
        let Some(task) = self
            .list
            .tasks()
            .iter()
            .find(|task| task.task_id.as_str() == id)
        else {
            return Request::None;
        };
        let Some(view) = self.detail.views.get_mut(id) else {
            return Request::None;
        };
        match view.waiting.prepare(task, submission) {
            Ok(request) => Request::Answer(Box::new(request)),
            Err(message) => {
                if let Some(entry) = view.waiting.entries.get_mut(view.waiting.current) {
                    entry.error = Some(message);
                }
                Request::None
            }
        }
    }

    /// Sends a chosen action. Success returns the focus to the list and says
    /// what happened; a refusal keeps the panel with the typed text, shows why
    /// and reads the tasks again.
    pub(super) async fn answer(&mut self, request: AnswerRequest) {
        let AnswerRequest {
            task_id,
            waiting_id,
            title,
            generation,
            key,
            summary,
            submit,
        } = request;
        let result = match submit {
            Submit::Input {
                pending_input_id,
                delivery,
            } => self
                .client
                .control_input(
                    &task_id,
                    &SubmitInputRequest {
                        pending_input_id,
                        idempotency_key: key,
                        delivery,
                    },
                )
                .await
                .map(drop),
            Submit::Operator(command) => self
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
                .map(drop),
            Submit::Owner(command) => self
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
                .map(drop),
        };
        match result {
            Ok(()) => {
                self.settle_answer(&task_id, &waiting_id, Ok(()));
                self.focus = super::Focus::List;
                self.panel_focus = false;
                self.back_to_axis();
                self.notify(Severity::Success, format!("{title}: {summary}"));
            }
            Err(error) => {
                let retryable = super::is_transport(&error);
                let reason = failure_reason(&error);
                self.note_transport(&error);
                self.settle_answer(&task_id, &waiting_id, Err((reason.clone(), retryable)));
                if !retryable {
                    // First the read, so the reason is the last word on the bar.
                    self.refresh().await;
                }
                self.notify(Severity::Error, format!("{title}: nie wysłano — {reason}"));
            }
        }
    }

    fn settle_answer(
        &mut self,
        task_id: &str,
        waiting_id: &str,
        outcome: Result<(), (String, bool)>,
    ) {
        if let Some(view) = self.detail.views.get_mut(task_id) {
            view.waiting.settle(waiting_id, outcome);
            let text = view.draft_text();
            self.detail.set_draft(task_id, text);
        }
    }

    /// Reads the conductor's state when the answer panel is made of it, or
    /// the *Dyrygent* tab is shown, and the task has not got it for this
    /// revision. One read serves both. The tab's chat is read from the last
    /// entry held, a page at a time, up to [`conductor::MAX_PAGES`] pages.
    pub(super) async fn sync_conductor(&mut self, id: &str, revision: u64) {
        let shown = self.detail.tab() == super::DetailTab::Conductor;
        let client = self.client.clone();
        for page in 0..conductor::MAX_PAGES {
            let Some(view) = self.detail.views.get(id) else {
                return;
            };
            let panel = page == 0 && view.waiting.needs_conductor(revision);
            let tab = shown
                && if page == 0 {
                    view.conductor
                        .needs_read(revision, std::time::Instant::now())
                } else {
                    view.conductor.has_more()
                };
            if !panel && !tab {
                return;
            }
            let query = crate::controller_protocol::ConductorReadQuery {
                after: view.conductor.last_sequence(),
                limit: None,
            };
            let result = super::detail::bounded_read(
                super::detail::RELAYED_READ_LIMIT,
                client.control_conductor(id, &query),
            )
            .await;
            let failed = result.is_err();
            self.on_detail_read(id, revision, super::detail::DetailRead::Conductor(result));
            if failed || !shown {
                return;
            }
        }
    }

    /// Takes a read of the conductor's state in: the panel and the tab both.
    pub(super) fn apply_conductor_read(
        &mut self,
        task_id: &str,
        revision: u64,
        result: Result<ConductorReadResponse, ClientError>,
    ) {
        let result = self.settle(result);
        let Some(task) = self
            .list
            .tasks()
            .iter()
            .find(|task| task.task_id.as_str() == task_id)
        else {
            return;
        };
        let view = self.detail.view_mut(task_id);
        view.conductor
            .apply(revision, std::time::Instant::now(), result.clone());
        let positions = view.progress().positions().to_vec();
        view.waiting
            .apply_conductor(task, &positions, revision, result);
    }

    /// The steps were read: what a waiting says about them is built again.
    pub(super) fn reread_waiting(&mut self, task_id: &str) {
        let Some(task) = self
            .list
            .tasks()
            .iter()
            .find(|task| task.task_id.as_str() == task_id)
        else {
            return;
        };
        let view = self.detail.view_mut(task_id);
        let positions = view.progress().positions().to_vec();
        view.waiting.reread(task, &positions);
    }
}
