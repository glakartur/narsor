//! The state of the panel of one task: the entries that hold what the
//! operator typed, the forms the server offered and the one shown, the chat
//! and the drafts that outlived their waiting.
//!
//! The row of the list says *that* a task waits ([`AttentionItem`]); the
//! panel is built from it and from what the details know:
//!
//! * a question, an approval, a problem the step must be settled with — the
//!   form the runner reported with the task ([`InputPanel`]);
//! * a failed step and a proposal of the conductor's — the conductor's own
//!   state, read on demand like the other tabs ([`OperatorPanel`]);
//! * a failed provisioning and a problem of the runner's — the owner commands
//!   `start` and `archive` ([`OwnerPanel`]).
//!
//! Each waiting is an [`Entry`]: its context, a [`Chooser`] holding what the
//! operator typed, and how a chosen action is sent. Entries live as long as
//! the waiting does, so a refresh, a new read of the conductor or a move to
//! another tab never costs the operator a draft; an entry whose waiting went
//! away takes its draft with it into a box the operator can still copy from.
//!
//! When the server offers forms (`TaskSummary::panel`), only the waits a form
//! answers have an entry, and [`super::switcher::Switcher`] says which form is
//! shown.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::conductor::ConductorView;
use crate::controller_protocol::{
    ConductorReadResponse, ConversationUnavailableReason, TaskSummary,
};
use narsor_protocol::api::{Position, PositionStatus};
use narsor_protocol::controller::{
    AttentionItem, AttentionKind, PanelForm, PanelFormKind, PanelOffer, TaskActionId,
};

use super::chat::{Chat, ChatEffect};
use super::header::position_hint;
use super::switcher::{FormKey, Switcher};
use crate::tui::chooser::{
    self, Chooser, ChooserAction, ChooserField, ChooserFocus, ChooserItem, Submission,
};
use crate::tui::failure_form::OperatorPanel;
use crate::tui::input_form::{ContextLine, InputPanel, Tone};
use crate::tui::keymap::{self, Action, Context};
use crate::tui::scroll::one_line;
use crate::tui::scroll::{wrap_line, ScrollState};
use crate::tui::style::{Role, Styles};
use crate::tui::task_app::actions::{write_to_agent_item, written_text, TaskAction, COMMENT_FIELD};
use crate::tui::task_app::detail::short_age;
use crate::tui::task_app::idempotency_key;
use crate::tui::task_app::list::title_of;

use super::legacy::OwnerPanel;
use super::request::{AnswerRequest, PanelEffect, Submit, SyncEffect};

/// The most rows of the context the panel gives itself before it scrolls.
const MAX_CONTEXT_ROWS: usize = 4;

/// The fewest rows of detail content the panel leaves below the header and
/// two-row tab bar.
const KEEP_FOR_CONTENT: u16 = 2;

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
            Self::Input(panel) => {
                let composer_index = panel.composer_spec().map(|composer| composer.entry);
                panel
                    .items()
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| Some(*index) != composer_index)
                    .map(|(_, item)| item.clone())
                    .collect()
            }
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
            Self::Operator(panel) => panel.selected(),
            Self::Input(_) | Self::Owner(_) | Self::Missing => 0,
        }
    }

    fn has_composer(&self) -> bool {
        matches!(self, Self::Input(panel) if panel.composer_spec().is_some())
    }

    fn composer_item(&self) -> Option<ChooserItem> {
        let Self::Input(panel) = self else {
            return None;
        };
        let composer = panel.composer_spec()?;
        let field = panel.items().get(composer.entry)?.fields.first()?.clone();
        Some(ChooserItem::new(composer.label.clone(), composer.help.clone()).with_field(field))
    }

    fn composer_submission(&self, composer: &Chooser) -> Option<Submission> {
        let Self::Input(panel) = self else {
            return None;
        };
        let value = composer.fields_of(0).first()?.value().to_string();
        panel.composer_submission(value)
    }
}

/// One thing a task waits on, with what the operator typed for it.
#[derive(Debug, Clone)]
struct Entry {
    item: AttentionItem,
    model: Model,
    chooser: Chooser,
    composer: Option<Chooser>,
    context_scroll: ScrollState,
    /// What the last send found wrong, or the server refused with.
    error: Option<String>,
    /// The key of a send that did not get an answer; a retry of the same
    /// decision reuses it.
    key: Option<String>,
    /// The send in flight is action 0: its text goes when it is accepted.
    wrote: bool,
    /// The answer was sent: the typed text is not a draft any more.
    sent: bool,
    /// What is being sent: the same decision is not sent a second time
    /// before the first is settled.
    sending: Option<Submit>,
    /// The entry is a form of the server's panel: the message to the agent
    /// is the chat form's, so the list has no action 0.
    forms: bool,
}

impl Entry {
    /// An entry for `item` of `task`; `positions` are the steps read so far
    /// and `conductor` what the conductor said, once read.
    fn build(
        item: &AttentionItem,
        task: &TaskSummary,
        positions: &[Position],
        conductor: Option<&ConductorView>,
        forms: bool,
    ) -> Self {
        let model = Self::model(item, task, positions, conductor);
        let chooser = Chooser::new(Self::items_of(&model, task, forms))
            .enter_fields_on_activate()
            .confirming_submit()
            .selecting(model.selected() + usize::from(!forms));
        let composer = model.composer_item().map(|item| {
            Chooser::new(vec![item])
                .with_inputs_layout()
                .confirming_submit()
        });
        Self {
            item: item.clone(),
            model,
            chooser,
            composer,
            context_scroll: ScrollState::new(),
            error: None,
            key: None,
            wrote: false,
            sent: false,
            sending: None,
            forms,
        }
    }

    /// Where the model's entries start in the chooser: after action 0, when
    /// there is one.
    fn offset(&self) -> usize {
        usize::from(!self.forms)
    }

    /// The entries of the chooser: action 0, then what the waiting offers.
    /// Action 0 is the field of the composer when the waiting has one (Z3),
    /// and has its own field when it has none.
    fn items_of(model: &Model, task: &TaskSummary, forms: bool) -> Vec<ChooserItem> {
        if forms {
            return model.items();
        }
        let mut items = vec![write_to_agent_item(task, !model.has_composer())];
        items.extend(model.items());
        items
    }

    fn model(
        item: &AttentionItem,
        task: &TaskSummary,
        positions: &[Position],
        conductor: Option<&ConductorView>,
    ) -> Model {
        match item.kind {
            AttentionKind::Question | AttentionKind::Approval => {
                input_panel(item, task, positions).map_or(Model::Missing, Model::Input)
            }
            AttentionKind::Problem | AttentionKind::EntryLimit => {
                match input_panel(item, task, positions) {
                    Some(panel) => Model::Input(panel),
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
            AttentionKind::Unrecognized => Model::Missing,
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
        let was_confirming = self.is_confirming();
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
        let rebuilt = chooser.rebuilt(Self::items_of(&model, task, self.forms));
        self.chooser = if untouched {
            rebuilt.selecting(model.selected() + self.offset())
        } else {
            rebuilt
        };
        if let Some(composer) = &mut self.composer {
            if model.has_composer() {
                if let Some(item) = model.composer_item() {
                    *composer =
                        std::mem::replace(composer, Chooser::new(Vec::new())).rebuilt(vec![item]);
                }
            } else {
                self.composer = None;
            }
        } else if model.has_composer() {
            let mut composer = Chooser::new(vec![model.composer_item().expect("composer spec")])
                .with_inputs_layout()
                .confirming_submit();
            composer.leave_fields();
            self.composer = Some(composer);
        }
        if was_confirming && !self.is_confirming() {
            self.error =
                Some("Lista opcji się zmieniła — sprawdź i zatwierdź ponownie.".to_string());
        }
        self.model = model;
    }

    /// Whether a send waits for the confirmation of the operator.
    fn is_confirming(&self) -> bool {
        self.chooser.is_confirming() || self.composer.as_ref().is_some_and(Chooser::is_confirming)
    }

    /// The field of action 0 is empty again: its text was sent.
    fn empty_written_field(&mut self) {
        let mut items = self.chooser.items().to_vec();
        let Some(field) = items
            .first_mut()
            .and_then(|item| item.fields.iter_mut().find(|f| f.name == COMMENT_FIELD))
        else {
            return;
        };
        *field = ChooserField::multiline(COMMENT_FIELD).required();
        let selected = self.chooser.selected();
        self.chooser = Chooser::new(items)
            .enter_fields_on_activate()
            .confirming_submit()
            .selecting(selected);
    }

    /// Whether what this entry shows comes from the conductor's state.
    fn wants_conductor(&self) -> bool {
        matches!(
            self.item.kind,
            AttentionKind::FailedStep | AttentionKind::ConductorProposal
        )
    }

    fn is_drafted(&self) -> bool {
        !self.sent
            && (!self.chooser.draft_text().is_empty()
                || self
                    .composer
                    .as_ref()
                    .is_some_and(|composer| !composer.draft_text().is_empty()))
    }

    fn submission_for_message(&self) -> Option<Submission> {
        self.model.composer_submission(self.composer.as_ref()?)
    }

    fn draft_text(&self) -> String {
        [
            self.chooser.draft_text(),
            self.composer
                .as_ref()
                .map_or_else(String::new, Chooser::draft_text),
        ]
        .into_iter()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
    }

    fn kind_word(&self) -> &'static str {
        match self.item.kind {
            AttentionKind::Question => "Pytanie",
            AttentionKind::Approval => "Approval",
            AttentionKind::Problem => "Problem",
            AttentionKind::EntryLimit => "Limit wejść",
            AttentionKind::FailedStep => "Nieudany krok",
            AttentionKind::ConductorProposal => "Propozycja dyrygenta",
            AttentionKind::ProvisioningFailed => "Nieudany provisioning",
            AttentionKind::Unrecognized => "Nieobsługiwane oczekiwanie",
        }
    }

    /// `?` for what asks, `!` for what is wrong.
    fn glyph(&self) -> char {
        match self.item.kind {
            AttentionKind::Question
            | AttentionKind::Approval
            | AttentionKind::EntryLimit
            | AttentionKind::ConductorProposal => '?',
            AttentionKind::Problem
            | AttentionKind::FailedStep
            | AttentionKind::ProvisioningFailed => '!',
            AttentionKind::Unrecognized => '?',
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

/// The action of the menu a choice of the task's own actions does.
pub(super) fn panel_action(id: TaskActionId) -> Option<TaskAction> {
    match id {
        TaskActionId::Transition => Some(TaskAction::Transition),
        TaskActionId::Resume | TaskActionId::Pause => Some(TaskAction::PauseResume),
        TaskActionId::Restart => Some(TaskAction::Restart),
        _ => None,
    }
}

/// The panel of the wait `item` stands for: the definition the server put in
/// its form. `None` while the server offers no form for the wait.
fn input_panel(
    item: &AttentionItem,
    task: &TaskSummary,
    positions: &[Position],
) -> Option<InputPanel> {
    let state_of = |attempt: &str| {
        positions
            .iter()
            .find(|position| position.action_execution_id.as_deref() == Some(attempt))
            .map(|position| position.state_id.clone())
    };
    let definition = task
        .panel
        .as_ref()?
        .forms
        .iter()
        .filter(|form| form.waiting_id.as_deref() == Some(item.waiting_id.as_str()))
        .find_map(|form| form.definition.as_ref())?;
    Some(InputPanel::from_definition(definition, &state_of))
}

/// The entries of the form of the task's own actions.
fn manual_items(form: &PanelForm) -> Vec<ChooserItem> {
    form.choices
        .iter()
        .map(|choice| {
            ChooserItem::new(
                choice.label.clone(),
                choice.description.clone().unwrap_or_default(),
            )
        })
        .collect()
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

/// The panel of one task: every waiting, the one shown, and the drafts.
#[derive(Debug, Clone, Default)]
pub struct WaitingPanel {
    entries: Vec<Entry>,
    current: usize,
    pub(super) collapsed: bool,
    released: Vec<Released>,
    conductor: Option<ConductorView>,
    conductor_revision: Option<u64>,
    /// When a read told "the reader is starting" is asked again (D63).
    conductor_retry_at: Option<std::time::Instant>,
    /// The forms of the server's panel and the one shown; `None` for a task
    /// whose server did not say (a server from before the field) or whose
    /// wait is one of the owner's own panels.
    forms: Option<Forms>,
    /// The free message to the agent: one draft per task.
    chat: Chat,
    /// The height of the details the panel was last asked about: the chat
    /// box grows to half of it (R16).
    details_height: std::cell::Cell<u16>,
}

/// The forms the server offered, and which is shown.
#[derive(Debug, Clone)]
struct Forms {
    offer: PanelOffer,
    switcher: Switcher,
    /// The choices of the form of the task's own actions.
    manual: Option<Chooser>,
}

/// Where the form shown keeps its state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// An entry of the waiting panel (an answer, a permission, an entry
    /// limit, the output of a step), by its index.
    Entry(usize),
    /// The free message to the agent.
    Chat,
    /// The choices of the task's own actions.
    Manual,
    /// Nothing is shown.
    Nothing,
}

impl WaitingPanel {
    /// Whether there is anything to show: a waiting, a form of the server's
    /// panel, or a draft left behind.
    pub fn is_present(&self) -> bool {
        match &self.forms {
            Some(forms) => {
                !forms.switcher.is_empty()
                    || !self.released.is_empty()
                    || !self.chat.draft_text().is_empty()
            }
            None => !self.entries.is_empty() || !self.released.is_empty(),
        }
    }

    /// Where the form shown keeps its state.
    fn slot(&self) -> Slot {
        let Some(forms) = &self.forms else {
            return if self.current < self.entries.len() {
                Slot::Entry(self.current)
            } else {
                Slot::Nothing
            };
        };
        match forms.switcher.active() {
            None => Slot::Nothing,
            Some(key) if key.kind == PanelFormKind::Chat => Slot::Chat,
            Some(FormKey {
                kind:
                    PanelFormKind::Action
                    | PanelFormKind::Answer
                    | PanelFormKind::EntryLimit
                    | PanelFormKind::Permission,
                waiting_id: Some(id),
            }) => self
                .entries
                .iter()
                .position(|entry| entry.item.waiting_id == *id)
                .map_or(Slot::Nothing, Slot::Entry),
            Some(key) if key.is_manual_action() => Slot::Manual,
            // A form of a kind this client does not know has no state.
            Some(_) => Slot::Nothing,
        }
    }

    fn current_entry(&self) -> Option<&Entry> {
        match self.slot() {
            Slot::Entry(index) => self.entries.get(index),
            _ => None,
        }
    }

    /// Where the entries of the chooser of the shown form start in its
    /// list: after action 0, when there is one.
    pub fn chooser_offset(&self) -> usize {
        self.current_entry().map_or(1, Entry::offset)
    }

    /// Says on the form shown what a try to send it found wrong.
    pub fn fail(&mut self, message: String) {
        match self.slot() {
            Slot::Entry(index) => self.entries[index].error = Some(message),
            Slot::Chat => self.chat.settle(Err((message, true))),
            Slot::Manual | Slot::Nothing => {}
        }
    }

    /// The kind of the form shown, in the server's panel.
    pub fn active_form(&self) -> Option<PanelFormKind> {
        self.forms.as_ref()?.switcher.active().map(|key| key.kind)
    }

    /// How many forms the server's panel has.
    pub fn form_count(&self) -> usize {
        self.forms.as_ref().map_or(0, |forms| forms.switcher.len())
    }

    /// What the operator wrote in the chat form and has not sent.
    pub fn chat_draft(&self) -> String {
        self.chat.draft_text()
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
        self.current_entry().map(|entry| &entry.chooser)
    }

    /// The separate text composer for an ordinary answer or conversation reply.
    pub fn composer(&self) -> Option<&Chooser> {
        self.current_entry()?.composer.as_ref()
    }

    /// What the last send of the waiting shown found wrong.
    pub fn error(&self) -> Option<&str> {
        match self.slot() {
            Slot::Chat => self.chat.error(),
            _ => self
                .current_entry()
                .and_then(|entry| entry.error.as_deref()),
        }
    }

    /// The drafts of waitings that went away.
    pub fn released(&self) -> &[Released] {
        &self.released
    }

    /// Whether the form of the current entry waits for the confirmation of
    /// its send: only the popup's keys act then.
    pub fn is_confirming(&self) -> bool {
        match self.slot() {
            Slot::Chat => self.chat.is_confirming(),
            Slot::Manual => false,
            _ => self.current_entry().is_some_and(|entry| {
                entry.chooser.is_confirming()
                    || entry.composer.as_ref().is_some_and(Chooser::is_confirming)
            }),
        }
    }

    /// Whether keys are typed into a field: `q` and `?` are text then.
    pub fn is_typing(&self) -> bool {
        match self.slot() {
            Slot::Chat => self.chat.is_typing(),
            Slot::Manual => false,
            _ => self.current_entry().is_some_and(|entry| {
                entry.chooser.is_typing() || entry.composer.as_ref().is_some_and(Chooser::is_typing)
            }),
        }
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
            .map(Entry::draft_text)
            .collect();
        parts.extend(self.released.iter().map(|released| released.text.clone()));
        if self.forms.is_some() {
            let chat = self.chat.draft_text();
            if !chat.is_empty() {
                parts.push(chat);
            }
        }
        parts.join("\n")
    }

    /// Whether the conductor's state has to be read for `revision` of the
    /// row: a failed step and a proposal are made of it.
    pub fn needs_conductor(&self, revision: u64) -> bool {
        self.entries.iter().any(Entry::wants_conductor)
            && (self.conductor_revision != Some(revision)
                || self
                    .conductor_retry_at
                    .is_some_and(|at| std::time::Instant::now() >= at))
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
        self.conductor_retry_at = None;
        match result {
            Ok(ConductorReadResponse::Available { conductor, .. }) => {
                self.conductor = Some(*conductor);
            }
            // The reader of a stopped task is starting: keep what the panel
            // has and ask again after a pause.
            Ok(ConductorReadResponse::Unavailable {
                reason: ConversationUnavailableReason::ReaderStarting,
                ..
            }) => {
                self.conductor_retry_at =
                    Some(std::time::Instant::now() + crate::tui::task_app::tabs::READER_RETRY);
                return;
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
        let before = self.active_key();
        let offer = task.panel.as_ref();
        let forms_mode = offer.is_some();
        let mut old = std::mem::take(&mut self.entries);
        let mut entries = Vec::with_capacity(task.attention.len());
        // With the server's forms, only the waits a form answers have an entry.
        let wanted = task.attention.iter().filter(|item| {
            offer.is_none_or(|offer| {
                offer.forms.iter().any(|form| {
                    form.kind != PanelFormKind::Unrecognized
                        && form.waiting_id.as_deref() == Some(item.waiting_id.as_str())
                })
            })
        });
        for item in wanted {
            match old.iter().position(|entry| {
                entry.item.waiting_id == item.waiting_id && entry.forms == forms_mode
            }) {
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
                        .position(|released| {
                            released.waiting_id == item.waiting_id
                                && released.entry.forms == forms_mode
                        })
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
                        None => {
                            Entry::build(item, task, positions, self.conductor.as_ref(), forms_mode)
                        }
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
                    text: gone.draft_text(),
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
        match offer {
            Some(offer) => {
                self.sync_forms(offer);
                if let Slot::Entry(index) = self.slot() {
                    self.current = index;
                }
                if self.active_key() != before {
                    effect.switched = self.is_typing() || self.is_confirming();
                    self.leave_all();
                    self.arrive();
                }
            }
            None => self.forms = None,
        }
        effect
    }

    /// Takes the server's offer in: which forms there are, which is shown,
    /// and the choices of the form of the task's own actions.
    fn sync_forms(&mut self, offer: &PanelOffer) {
        let mut forms = self.forms.take().unwrap_or_else(|| Forms {
            offer: PanelOffer::default(),
            switcher: Switcher::default(),
            manual: None,
        });
        forms.switcher.sync(offer);
        forms.manual = offer
            .forms
            .iter()
            .find(|form| form.is_manual_action())
            .map(|form| {
                let items = manual_items(form);
                match forms.manual.take() {
                    Some(chooser) => chooser.rebuilt(items),
                    None => Chooser::new(items),
                }
            });
        forms.offer = offer.clone();
        self.forms = Some(forms);
    }

    /// The key of the form shown, in the server's panel.
    fn active_key(&self) -> Option<FormKey> {
        self.forms
            .as_ref()
            .and_then(|forms| forms.switcher.active().cloned())
    }

    /// Takes the keys out of every form: what was typed stays.
    fn leave_all(&mut self) {
        self.chat.leave();
        for entry in &mut self.entries {
            entry.chooser.leave_fields();
            if let Some(composer) = entry.composer.as_mut() {
                composer.leave_fields();
            }
        }
    }

    /// Gives the keys to the box of the form that came into view, when it is
    /// a text box.
    fn arrive(&mut self) {
        if self.slot() == Slot::Chat {
            self.chat.enter();
        }
    }

    /// Folds a key into the panel. Keys that are the application's — `q`,
    /// `?` — are not here: the caller handles them first when nothing is
    /// being typed.
    pub fn on_key(&mut self, key: KeyEvent) -> PanelEffect {
        if let Some(effect) = self.on_forms_key(key) {
            return effect;
        }
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
                Some(Action::PrevPending) if self.forms.is_none() => return self.step(-1),
                Some(Action::NextPending) if self.forms.is_none() => return self.step(1),
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
        if let Some(composer) = entry.composer.as_mut() {
            if composer.is_typing() {
                let action = composer.on_key(key);
                return match action {
                    ChooserAction::Submit(_) => entry
                        .submission_for_message()
                        .map_or(PanelEffect::None, PanelEffect::Submit),
                    ChooserAction::Cancel => PanelEffect::Collapse,
                    ChooserAction::Blocked(reason) => PanelEffect::Notice(reason),
                    ChooserAction::None => {
                        entry.sent = false;
                        entry.key = None;
                        entry.error = None;
                        PanelEffect::None
                    }
                };
            }
        }
        if entry.composer.is_some()
            && !entry.chooser.is_confirming()
            && entry.chooser.focus() == ChooserFocus::Actions
            && (entry.chooser.items().is_empty()
                || (entry.chooser.selected() + 1 == entry.chooser.items().len()
                    && key.code == KeyCode::Down))
            && (key.code == KeyCode::Down
                || (entry.chooser.items().is_empty() && key.code == KeyCode::Enter))
        {
            if let Some(composer) = entry.composer.as_mut() {
                composer.enter_fields();
                return PanelEffect::None;
            }
        }
        if key.code == KeyCode::Esc
            && entry.chooser.focus() == ChooserFocus::Fields
            && !entry.chooser.is_confirming()
        {
            entry.chooser.leave_fields();
            return PanelEffect::None;
        }
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
            ChooserAction::Submit(mut submission) => {
                if submission.index == 0 && !entry.forms {
                    // Action 0: the field of the composer takes the focus, or
                    // the text of its own field goes to the agent.
                    if let Some(composer) = entry.composer.as_mut() {
                        composer.enter_fields();
                        return PanelEffect::None;
                    }
                    return PanelEffect::Comment(written_text(&submission));
                }
                let irreversible = entry
                    .chooser
                    .items()
                    .get(submission.index)
                    .is_some_and(|item| item.irreversible);
                // The entries of the model start after action 0, when there is one.
                submission.index -= entry.offset();
                if irreversible {
                    PanelEffect::Confirm(submission)
                } else {
                    PanelEffect::Submit(submission)
                }
            }
        }
    }

    /// Folds pasted text into the field being typed in — the text box or a
    /// field of the chosen action — as a key that edits it would: the last
    /// send's verdict goes. Never sends. Returns whether a field took it.
    pub fn paste(&mut self, pasted: &str) -> bool {
        match self.slot() {
            Slot::Chat => return self.chat.paste(pasted),
            Slot::Manual | Slot::Nothing => return false,
            Slot::Entry(_) => {}
        }
        let Some(entry) = self.entries.get_mut(self.current) else {
            return false;
        };
        let used = match entry.composer.as_mut() {
            Some(composer) if composer.is_typing() => composer.paste(pasted),
            _ => entry.chooser.paste(pasted),
        };
        if used {
            entry.sent = false;
            entry.key = None;
            entry.error = None;
        }
        used
    }

    /// A key in the server's panel: `Esc` steps through the forms, the chat
    /// and the task's own actions take their keys; the entries take theirs
    /// in [`Self::on_key`]. `None` leaves the key to it.
    fn on_forms_key(&mut self, key: KeyEvent) -> Option<PanelEffect> {
        if self
            .forms
            .as_ref()
            .is_none_or(|forms| forms.switcher.is_empty())
        {
            return None;
        }
        if key.code == KeyCode::Esc && !self.is_confirming() {
            self.leave_all();
            if let Some(forms) = self.forms.as_mut() {
                forms.switcher.next();
            }
            if let Slot::Entry(index) = self.slot() {
                self.current = index;
            }
            self.arrive();
            return Some(PanelEffect::None);
        }
        match self.slot() {
            Slot::Chat => Some(match self.chat.on_key(key) {
                ChatEffect::None => PanelEffect::None,
                ChatEffect::Notice(text) => PanelEffect::Notice(text),
                ChatEffect::Send(text) => PanelEffect::Comment(text),
            }),
            Slot::Manual => {
                // A form of a kind this client does not know has no chooser:
                // the key is spent, not left to an entry that is not shown.
                let Some(chooser) = self.forms.as_mut().and_then(|forms| forms.manual.as_mut())
                else {
                    return Some(PanelEffect::None);
                };
                let action = chooser.on_key(key);
                Some(match action {
                    ChooserAction::Submit(submission) => self
                        .manual_action(submission.index)
                        .map_or(PanelEffect::None, PanelEffect::Action),
                    ChooserAction::Blocked(reason) => PanelEffect::Notice(reason),
                    ChooserAction::Cancel | ChooserAction::None => PanelEffect::None,
                })
            }
            Slot::Nothing => Some(PanelEffect::None),
            Slot::Entry(_) => None,
        }
    }

    /// The action the `index`th choice of the task's own actions does.
    fn manual_action(&self, index: usize) -> Option<TaskActionId> {
        self.forms
            .as_ref()?
            .offer
            .forms
            .iter()
            .find(|form| form.is_manual_action())?
            .choices
            .get(index)?
            .action
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
        // Only the form shown may send: the entry kept in `current` can be
        // one the owner has stepped away from.
        let Slot::Entry(index) = self.slot() else {
            return Err("Nie ma czego wysłać.".to_string());
        };
        let entry = self
            .entries
            .get_mut(index)
            .ok_or_else(|| "Nie ma czego wysłać.".to_string())?;
        let (submit, summary) = match &entry.model {
            Model::Input(panel) => (
                Submit::Input {
                    pending_input_id: entry.item.waiting_id.clone(),
                    delivery: Box::new(panel.delivery(submission)?),
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
        if entry.sending.as_ref() == Some(&submit) {
            return Err("Zapis trwa.".to_string());
        }
        entry.sending = Some(submit.clone());
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

    /// The request for the text of action 0 of the waiting shown.
    pub fn prepare_comment(
        &mut self,
        task: &TaskSummary,
        text: String,
    ) -> Result<AnswerRequest, String> {
        if text.is_empty() {
            return Err("Napisz, co agent ma zobaczyć.".to_string());
        }
        if self.forms.is_some() {
            // The chat form: the message is about the task, not about a wait.
            let key = self.chat.send_key(idempotency_key);
            return Ok(AnswerRequest {
                task_id: task.task_id.to_string(),
                waiting_id: String::new(),
                title: title_of(task),
                generation: task.generation,
                key,
                summary: "wiadomość wysłana do agenta".to_string(),
                submit: Submit::Comment(text),
            });
        }
        let entry = self
            .entries
            .get_mut(self.current)
            .ok_or_else(|| "Nie ma czego wysłać.".to_string())?;
        entry.wrote = true;
        let key = entry.key.get_or_insert_with(idempotency_key).clone();
        Ok(AnswerRequest {
            task_id: task.task_id.to_string(),
            waiting_id: entry.item.waiting_id.clone(),
            title: title_of(task),
            generation: task.generation,
            key,
            summary: "wiadomość wysłana do agenta".to_string(),
            submit: Submit::Comment(text),
        })
    }

    /// A request made by [`Self::prepare`] was not sent (it waits behind the
    /// `y` / `n` window): the same decision may be prepared again.
    pub(super) fn unprepare(&mut self, waiting_id: &str) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.item.waiting_id == waiting_id)
        {
            entry.sending = None;
        }
    }

    /// Says what a send of `waiting_id` came to. Success forgets the draft; a
    /// refusal keeps the panel as it is and shows why; a lost connection keeps
    /// the key for the retry.
    pub(super) fn settle(&mut self, waiting_id: &str, outcome: Result<(), (String, bool)>) {
        if waiting_id.is_empty() && self.forms.is_some() {
            self.chat.settle(outcome);
            return;
        }
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.item.waiting_id == waiting_id)
        else {
            return;
        };
        entry.sending = None;
        match outcome {
            Ok(()) => {
                entry.sent = true;
                entry.key = None;
                entry.error = None;
                if std::mem::take(&mut entry.wrote) {
                    entry.empty_written_field();
                }
            }
            Err((reason, retryable)) => {
                entry.wrote = false;
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
    pub fn preferred_height(&self, width: u16, area_height: u16, header_height: u16) -> u16 {
        let reserved = header_height + 2 + KEEP_FOR_CONTENT;
        let cap = area_height.saturating_sub(reserved);
        self.details_height.set(area_height);
        let other = matches!(self.slot(), Slot::Chat | Slot::Manual);
        let wanted = if other {
            // The padding, the heading and the gap, and the form.
            5 + match (
                self.slot(),
                self.forms.as_ref().and_then(|f| f.manual.as_ref()),
            ) {
                (Slot::Manual, Some(chooser)) => chooser.preferred_height(width),
                _ => self.chat.preferred_height(width, area_height),
            }
        } else if let Some(entry) = self
            .current_entry()
            .filter(|_| self.forms.is_some() || self.slot() != Slot::Nothing)
        {
            if self.collapsed && self.forms.is_none() {
                2
            } else {
                let context = context_rows(&entry.model.context(), usize::from(width)).len();
                7 + context.min(MAX_CONTEXT_ROWS) as u16
                    + entry.notices().len() as u16
                    + entry.chooser.preferred_height(width)
                    + if entry.composer.is_some() { 3 } else { 0 }
            }
        } else {
            let leftovers = self.leftovers();
            if leftovers.is_empty() {
                0
            } else {
                let lines: usize = leftovers
                    .iter()
                    .map(|(_, text)| 1 + text.lines().count().max(1))
                    .sum();
                5 + lines.min(5) as u16
            }
        };
        wanted.min(cap)
    }

    /// Draws the panel into `area`. `as_of` is the server's now, which the age of
    /// the shown waiting is counted to.
    ///
    /// The dark panel background remains visible even when it has no focus;
    /// the left marker shows focus, and the action number's pipes show the
    /// active choice. The actions scroll inside the remaining space (R17).
    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        styles: &Styles,
        stale: bool,
        focused: bool,
        as_of: chrono::DateTime<chrono::Utc>,
    ) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        if matches!(self.slot(), Slot::Chat | Slot::Manual) {
            self.render_other_form(frame, area, styles, stale, focused);
            return;
        }
        if self.current_entry().is_none() {
            self.render_released(frame, area, styles);
            paint_waiting_panel(frame, area, styles, focused);
            return;
        }
        let count = self.entries.len();
        let position = self
            .forms
            .as_ref()
            .map(|forms| position_hint(&forms.switcher));
        let current = self.current;
        let collapsed = self.collapsed && position.is_none();
        let entry = &mut self.entries[current];
        let vertical_padding = u16::from(area.height >= 16);
        let inner = Rect::new(
            area.x.saturating_add(2),
            area.y.saturating_add(vertical_padding),
            area.width.saturating_sub(3),
            area.height
                .saturating_sub(vertical_padding.saturating_mul(2)),
        );
        if inner.width == 0 || inner.height == 0 {
            paint_waiting_panel(frame, area, styles, focused);
            return;
        }
        let age = short_age((as_of - entry.item.since).num_seconds());
        let state = entry
            .item
            .state_id
            .as_deref()
            .map_or_else(String::new, |state| format!(" · {}", one_line(state)));
        let role = if stale { Role::Done } else { entry.role() };
        let mut heading = format!("{} {}{state} · {age}", entry.glyph(), entry.kind_word());
        let separator_role = if stale { Role::Done } else { role };
        if inner.height < 1 {
            paint_waiting_panel(frame, area, styles, focused);
            return;
        }
        if collapsed {
            if count > 1 {
                heading.push_str(&format!(" · {}/{count}", current + 1));
            }
            let line = Line::from(vec![
                Span::styled(format!("  ▸ {heading}"), styles.strong(role)),
                Span::styled("  Tab rozwiń", styles.of(separator_role)),
            ]);
            frame.render_widget(
                Paragraph::new(line),
                Rect::new(inner.x, inner.y, inner.width, 1),
            );
            paint_waiting_panel(frame, area, styles, focused);
            return;
        }

        let count_suffix = match &position {
            Some(hint) => format!("  {hint}"),
            None if count > 1 => format!("  {}/{count}", current + 1),
            None => String::new(),
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(""),
                Span::styled(heading, styles.strong(role)),
                Span::styled(count_suffix, styles.of(separator_role)),
            ])),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
        let title_gap = if inner.height >= 10 { 2 } else { 1 };
        let body = Rect::new(
            inner.x,
            inner.y.saturating_add(title_gap),
            inner.width,
            inner.height.saturating_sub(title_gap),
        );
        if body.height == 0 {
            paint_waiting_panel(frame, area, styles, focused);
            return;
        }

        let rows = context_rows(&entry.model.context(), usize::from(body.width));
        let notices = entry.notices();
        let notice_height = (notices.len() as u16).min(body.height.saturating_sub(8));
        // The actions come first: the context takes what they leave.
        let context_height = rows
            .len()
            .min(MAX_CONTEXT_ROWS)
            .min(usize::from(body.height.saturating_sub(notice_height + 8)));
        let section_gap =
            u16::from((context_height > 0 || notice_height > 0) && inner.height >= 13);
        let [context_area, notice_area, _gap_area, chooser_area] = Layout::vertical([
            Constraint::Length(context_height as u16),
            Constraint::Length(notice_height),
            Constraint::Length(section_gap),
            Constraint::Min(0),
        ])
        .areas(body);

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
        if let Some(composer) = entry.composer.as_mut() {
            let composer_height = chooser_area.height.saturating_sub(1).min(3);
            let action_height = chooser_area.height.saturating_sub(composer_height);
            let [action_area, composer_area] = Layout::vertical([
                Constraint::Length(action_height),
                Constraint::Length(composer_height),
            ])
            .areas(chooser_area);
            chooser::render_footer(
                frame,
                action_area,
                &mut entry.chooser,
                styles,
                focused && !composer.is_typing(),
            );
            chooser::render_fields(frame, composer_area, composer, styles, focused);
        } else {
            chooser::render_footer(frame, chooser_area, &mut entry.chooser, styles, focused);
        }
        paint_waiting_panel(frame, area, styles, focused);
    }

    fn render_released(&self, frame: &mut Frame, area: Rect, styles: &Styles) {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "Szkic bez oczekiwania — skopiuj; Esc go usuwa",
                styles.strong(Role::Attention),
            )),
            Rect::new(
                area.x.saturating_add(2),
                area.y.saturating_add(1),
                area.width.saturating_sub(3),
                1,
            ),
        );
        let inner = Rect::new(
            area.x.saturating_add(2),
            area.y.saturating_add(3),
            area.width.saturating_sub(3),
            area.height.saturating_sub(4),
        );
        let mut lines = Vec::new();
        for (title, text) in self.leftovers() {
            lines.push(Line::styled(one_line(&title), styles.of(Role::Done)));
            for line in text.lines() {
                lines.push(Line::raw(one_line(line)));
            }
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }

    /// The drafts that have no waiting to go with: the ones of waitings that
    /// went away, and the chat's when no form is on offer.
    fn leftovers(&self) -> Vec<(String, String)> {
        let mut drafts: Vec<(String, String)> = self
            .released
            .iter()
            .map(|released| (released.title.clone(), released.text.clone()))
            .collect();
        if self
            .forms
            .as_ref()
            .is_some_and(|forms| forms.switcher.is_empty())
        {
            let chat = self.chat.draft_text();
            if !chat.is_empty() {
                drafts.push(("Wiadomość do agenta".to_string(), chat));
            }
        }
        drafts
    }

    /// Draws the form that is not an entry: the free message to the agent or
    /// the choices of the task's own actions, under the heading with the
    /// place among the forms.
    fn render_other_form(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        styles: &Styles,
        stale: bool,
        focused: bool,
    ) {
        let vertical_padding = u16::from(area.height >= 16);
        let inner = Rect::new(
            area.x.saturating_add(2),
            area.y.saturating_add(vertical_padding),
            area.width.saturating_sub(3),
            area.height
                .saturating_sub(vertical_padding.saturating_mul(2)),
        );
        let slot = self.slot();
        let Some(hint) = self
            .forms
            .as_ref()
            .map(|forms| position_hint(&forms.switcher))
        else {
            return;
        };
        if inner.width == 0 || inner.height < 2 {
            paint_waiting_panel(frame, area, styles, focused);
            return;
        }
        let role = if stale { Role::Done } else { Role::Attention };
        let title = if slot == Slot::Chat {
            "› Rozmowa z agentem"
        } else {
            "› Akcje zadania"
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(title, styles.strong(role)),
                Span::styled(format!("  {hint}"), styles.of(role)),
            ])),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
        let gap = if inner.height >= 10 { 2 } else { 1 };
        let body = Rect::new(
            inner.x,
            inner.y.saturating_add(gap),
            inner.width,
            inner.height.saturating_sub(gap),
        );
        match slot {
            Slot::Chat => self
                .chat
                .render(frame, body, self.details_height.get(), styles, focused),
            _ => {
                if let Some(chooser) = self.forms.as_mut().and_then(|f| f.manual.as_mut()) {
                    chooser::render_footer(frame, body, chooser, styles, focused);
                }
            }
        }
        paint_waiting_panel(frame, area, styles, focused);
    }
}

fn paint_waiting_panel(frame: &mut Frame, area: Rect, styles: &Styles, focused: bool) {
    let background = styles.answer_panel_background();
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = frame.buffer_mut().cell_mut((x, y)) {
                if let Some(color) = background.bg {
                    cell.bg = color;
                }
            }
        }
    }
    if area.width > 0 && area.height > 0 {
        let marker = styles.answer_panel_marker(focused);
        for y in area.y..area.y.saturating_add(area.height) {
            if let Some(cell) = frame.buffer_mut().cell_mut((area.x, y)) {
                cell.set_symbol("│");
                if let Some(color) = marker.fg {
                    cell.fg = color;
                }
                cell.modifier |= marker.add_modifier;
            }
        }
    }
}

/// The context wrapped to `width` columns, one row per line, each in its
/// tone. Everything in it is somebody else's text, so it is made safe here.
/// A description as lines, each safe for a terminal.
pub(super) fn one_line_per_row(text: &str) -> Vec<String> {
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
