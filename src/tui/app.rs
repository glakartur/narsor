//! Application state and key handling — the half of the TUI that has no
//! terminal in it.
//!
//! Everything the interface knows lives in [`App`]: the execution list, the
//! selected execution's state, context and event stream, the health of the
//! live channel, which panel has focus and which modal is open. Rendering reads
//! this and adds nothing; that split is what lets `tests/tui_state.rs` drive
//! the whole interface — including delivering human input and forcing a
//! transition — without a tty.
//!
//! # Where the data comes from
//!
//! * The execution list and a selected execution's history come from the
//!   [`ExecutionStore`](crate::persistence::ExecutionStore): a listing, plus a
//!   one-off read of the event log to backfill the stream.
//! * Everything after that comes from [`Engine::subscribe`] — see [`on_stream`]
//!   ([`App::on_stream`]). The log is never polled for live updates.
//! * The [`WorkflowDefinition`] is resolved the way the CLI resolves it, from
//!   what the execution recorded — `metadata.cli.definition_path` first, then
//!   `metadata.cli.definition_name` through [`crate::workflows`], which is the
//!   only thing an execution started from a built-in has. Without a definition
//!   there is no way to know a state's `outputs` or its
//!   `allow_manual_override`, so the panels that need those say so instead of
//!   pretending the state has none.
//!
//! # Manual transitions (D30)
//!
//! Two modes, and they are not the same operation:
//!
//! * **Choosing an output** — the label comes from the current state's declared
//!   `outputs`. When the execution is waiting on a question this goes through
//!   [`Engine::deliver_human_input`], so the engine runs the workflow on exactly
//!   as it would for any answer.
//! * **Override** — an arbitrary target state, offered only when the state
//!   declares `allow_manual_override`, and only after a separate confirmation.
//!   No engine API forces a transition, so the events are written here, and
//!   they are written to be auditable: a `DecisionMade` naming the override
//!   provider and carrying the reason, then `HumanTransitionSelected`,
//!   `StateExited` and `TransitionTaken`. The execution is parked `Paused` at
//!   the target (or finished, if the target is terminal) and the operator
//!   resumes it explicitly.
//!
//! # Registering a repository, and starting a task
//!
//! Both are modals, and both are the CLI's own operation with a form in front
//! of it — `narsor repo add`/`repo list`/`repo remove` behind `R`, and
//! `narsor run` behind `n`. Nothing here is a capability the CLI lacks.
//!
//! * The repository form builds a [`RepositoryEntry`] and calls its
//!   [`validate`](RepositoryEntry::validate) *before* `add()`, so a refused id
//!   or an `ext::` remote is reported next to the field rather than as a
//!   generic storage failure.
//! * The new-task modal opens on a **pick list** of every workflow the
//!   resolver can reach — built-ins, the user's own copies and
//!   `{catalog}/workflows`, each row naming its source and marking an override,
//!   exactly as `narsor workflow list` does. `←`/`→` (and `space`,
//!   `j`/`k`, `↑`/`↓`) change the selection; the last row, "Enter a path…",
//!   switches to a text line so that a definition being developed — which is
//!   in no search location yet — can still be run. The pick is then validated
//!   in full (every violation, the way `narsor validate` prints them)
//!   and the input form is generated from the definition's `inputs`: the
//!   declared `type` decides the JSON type the value reaches the context as,
//!   and a `repository` input is a pick list of what the registry holds.
//!
//! ## Where the workspace comes from
//!
//! From [`crate::tasks::start_task`], the same entry point `narsor run`
//! calls: a definition declaring `globals.workspace` is expanded, resolved
//! through the registry and cloned here exactly as it is there, and one that
//! declares none gets the same empty `{work}/tasks/{id}/code/workspace`. There
//! is one copy of the rules that decide what `git` is asked to clone, and this
//! interface does not hold a second.

use std::collections::HashSet;

use chrono::Utc;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Map, Value};

use crate::api::{
    ApiClient, ClientError, ConversationResponse, ExecutionSummary, HarnessSelectionSnapshot,
    TransitionRequest,
};
use crate::domain::{
    Context, Event, EventData, ExecutionStatus, HumanInputField, HumanInputType, InputParameter,
    ParamType, State, WorkflowDefinition, WorkflowExecution, Workspace,
};
use crate::engine::actions::script::TRUNCATION_MARKER;
use crate::engine::{
    permitted_labels, permitted_outputs, ConversationState as PositionConversationState, EventLink,
    HumanInputDelivery, InputContinuation, PendingHumanInput, Timeline,
};
use crate::persistence::{CatalogPaths, StatePaths, WorkPaths};
use crate::registry::{RepositoryEntry, RepositoryRegistry};
use crate::tasks;
use crate::tui::conversation::{classify, ClassifiedEntry};
use crate::tui::scroll::{sanitize, ScrollState};
use crate::workflows::{ResolvedWorkflow, WorkflowEntry, WorkflowResolver};

/// The decision provider recorded for an operator's ordinary manual choice.
pub const MANUAL_PROVIDER: &str = "human";

/// The decision provider recorded for a forced override.
///
/// Deliberately distinct from [`MANUAL_PROVIDER`]: `EventData::HumanTransitionSelected`
/// carries only the two state ids, so the fact that a transition bypassed the
/// declared graph would otherwise be invisible in the log.
pub const OVERRIDE_PROVIDER: &str = "human_override";

/// The reason recorded on a forced transition.
pub const OVERRIDE_REASON: &str =
    "Forced manual transition (override): the target state was picked by the operator.";

/// The reason recorded on an ordinary manual transition.
pub const MANUAL_REASON: &str = "Manual transition picked by the operator.";

/// The label recorded for a forced transition, which by definition has none.
pub const OVERRIDE_LABEL: &str = "override";

// ---------------------------------------------------------------------------
// Small state types
// ---------------------------------------------------------------------------

/// The panel that currently has focus.
///
/// Cycled with `Tab`; the focused panel owns `j`/`k`/`g`/`G` and decides which
/// shortcuts the bottom bar offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    /// Left: the execution list.
    Executions,
    /// Top right: current state and context variables.
    Details,
    /// Bottom right: the live event stream.
    Events,
}

impl Panel {
    /// The next panel in the cycle.
    pub fn next(self) -> Self {
        match self {
            Panel::Executions => Panel::Details,
            Panel::Details => Panel::Events,
            Panel::Events => Panel::Executions,
        }
    }

    /// The previous panel in the cycle.
    pub fn previous(self) -> Self {
        match self {
            Panel::Executions => Panel::Events,
            Panel::Details => Panel::Executions,
            Panel::Events => Panel::Details,
        }
    }

    /// The title drawn in the panel border.
    pub fn title(self) -> &'static str {
        match self {
            Panel::Executions => "Executions",
            Panel::Details => "State and context",
            Panel::Events => "Events",
        }
    }
}

/// The health of the live event channel.
///
/// Anything other than [`StreamStatus::Live`] is drawn prominently: a view that
/// has stopped updating must never be mistaken for a workflow that has stopped
/// moving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamStatus {
    /// Receiving events.
    Live,
    /// The receiver fell behind and the channel dropped events.
    Lagged {
        /// How many events were skipped by the last lag.
        skipped: u64,
    },
    /// The sender is gone — no further event will ever arrive.
    Disconnected,
}

impl StreamStatus {
    /// Whether the stream is still delivering.
    pub fn is_live(&self) -> bool {
        matches!(self, StreamStatus::Live)
    }

    /// The indicator drawn in the event panel's border.
    pub fn indicator(&self) -> String {
        match self {
            StreamStatus::Live => "● live".to_string(),
            StreamStatus::Lagged { skipped } => {
                format!("▲ {skipped} events lost — press r to reload")
            }
            StreamStatus::Disconnected => "✖ disconnected — the stream is closed".to_string(),
        }
    }
}

/// One line of the execution list.
#[derive(Debug, Clone)]
pub struct ExecutionRow {
    /// The execution id.
    pub id: String,
    /// The workflow it runs.
    pub workflow_id: String,
    /// Its status, as the schema spells it.
    pub status: ExecutionStatus,
    /// The state it sits in, if it sits in one.
    pub current_state: Option<String>,
}

/// A message shown in the bottom bar until the next operation replaces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// What the operator reads.
    pub text: String,
    /// Whether this reports a failure.
    pub is_error: bool,
}

impl Notice {
    /// A neutral confirmation.
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    /// A failure the operator has to act on.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}

/// What [`App::on_key`] asks the event loop to do afterwards.
///
/// Key handling is synchronous so it can be tested without a runtime; anything
/// that talks to the engine is named here and awaited by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Nothing further.
    None,
    /// Re-read the whole execution list, then the selection (U7: both are
    /// now `GET /executions` calls through the client).
    Refresh,
    /// Re-read just the selected execution's detail — emitted when the
    /// cursor moves onto a different row of the Executions panel (U7: this
    /// used to be a synchronous, free store read; it is a client call now,
    /// so [`App::move_cursor`]/[`App::jump_cursor`] can no longer do it
    /// inline the way [`App::after_task_cursor_move`] still does for the
    /// task view's own, purely local cursor).
    SelectExecution,
    /// Validate and deliver the open input form.
    SubmitInput,
    /// Carry out the transition the operator picked.
    ApplyTransition,
    /// Run or resume the selected execution.
    RunSelected,
    /// Recover a failed or interrupted execution, optionally with an operator
    /// note for the next agent attempt.
    RecoverSelected { comment: Option<String> },
    /// Save a one-shot instruction for the next agent attempt.
    SaveOperatorNote { note: String },
    /// Abort the active daemon task after an explicit confirmation.
    InterruptSelected,
    /// Create the execution the new-task form describes and run it.
    StartNewTask,
    /// Open the task view at the position `sequence`'s event resolves to,
    /// via the real timeline link map (R5).
    ///
    /// Emitted by `c` on an event the local, event-only check
    /// ([`conversation_attempt`]) says *might* carry a conversation, from
    /// either the Events panel or [`Modal::EventDetail`]. The event loop
    /// awaits [`App::open_conversation_for_event`], which fetches the
    /// timeline and makes the real decision — including refusing a
    /// script-step event even though it carries an `action_execution_id`
    /// (the over-linking U6 flagged).
    OpenConversation {
        /// The event whose link to resolve.
        sequence: u64,
    },
    /// Open the task view on the selected execution's default position
    /// (R6). Emitted by `Enter` on the execution list.
    OpenTaskView,
    /// Re-read the task view's timeline and continue its selected
    /// position's conversation from its cursor (R8). Emitted by `r` while
    /// the task view is open.
    TaskRefresh,
    /// Load the freshly selected position's content from scratch. Emitted
    /// whenever the axis' selection changes.
    TaskLoadSelected,
    /// Fetch the daemon's authoritative default-harness snapshot for the
    /// picker that was just opened.
    LoadHarnessSelection,
    /// Persist one picker entry as the daemon's default harness.
    SetHarnessSelection { harness: String },
    /// Remove the durable override and return to the configured default.
    ResetHarnessSelection,
}

// ---------------------------------------------------------------------------
// The human input form
// ---------------------------------------------------------------------------

/// One row of the generated form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormRow {
    /// A declared field, by index.
    Field(usize),
    /// The output label picker, shown when the state recorded a menu.
    Label,
}

/// The human-input form, generated from [`PendingHumanInput::fields`].
///
/// A `text` field is an editable line, a `select` field cycles its `options`.
/// The label row exists because §4's `human_clarification` has one text field
/// and two outputs: nothing can be inferred there, so the operator picks.
#[derive(Debug, Clone)]
pub struct InputForm {
    /// The question.
    pub instruction: String,
    /// The declared fields, in declaration order.
    pub fields: Vec<HumanInputField>,
    /// The current value of each field, by index.
    pub values: Vec<String>,
    /// The label choices: `None` means "let the engine infer it".
    pub label_choices: Vec<Option<String>>,
    /// Which label choice is selected.
    pub label_index: usize,
    /// The focused row.
    pub row: usize,
    /// The last validation failure, if any.
    pub error: Option<String>,
}

/// A free-form note sent with an explicit recovery of an agent step.
#[derive(Debug, Clone, Default)]
pub struct RecoveryCommentForm {
    /// The text to append to the next agent prompt.
    pub value: String,
}

impl InputForm {
    /// Builds the form for a pending question.
    pub fn new(pending: &PendingHumanInput) -> Self {
        let fields = pending.fields().to_vec();
        let values = fields
            .iter()
            .map(|field| match field.field_type {
                // A select starts on its first option, so the common case is
                // one keystroke: Enter.
                HumanInputType::Select if field.required => {
                    field.options.first().cloned().unwrap_or_default()
                }
                HumanInputType::Select => String::new(),
                HumanInputType::Text => String::new(),
            })
            .collect();

        let mut label_choices: Vec<Option<String>> = Vec::new();
        if !pending.outputs.is_empty() {
            if infers_label(&fields) {
                label_choices.push(None);
            }
            label_choices.extend(pending.output_labels().into_iter().map(Some));
        }

        Self {
            instruction: pending.instruction().to_string(),
            fields,
            values,
            label_choices,
            label_index: 0,
            row: 0,
            error: None,
        }
    }

    /// The rows, in the order they are drawn and navigated.
    pub fn rows(&self) -> Vec<FormRow> {
        let mut rows: Vec<FormRow> = (0..self.fields.len()).map(FormRow::Field).collect();
        if !self.label_choices.is_empty() {
            rows.push(FormRow::Label);
        }
        rows
    }

    /// The focused row, if the form has any.
    pub fn current_row(&self) -> Option<FormRow> {
        self.rows().get(self.row).copied()
    }

    /// The label the operator picked, if they picked one.
    pub fn chosen_label(&self) -> Option<String> {
        self.label_choices.get(self.label_index).cloned().flatten()
    }

    /// Validates by the same rules the engine applies (U6) and builds the
    /// delivery.
    ///
    /// Duplicating the rules here is on purpose: the engine would reject the
    /// same delivery, but only after a round trip, and the operator deserves
    /// the complaint next to the field that caused it.
    pub fn validate(&self) -> Result<HumanInputDelivery, String> {
        let mut values = Map::new();

        for (index, field) in self.fields.iter().enumerate() {
            let raw = self.values.get(index).map(String::as_str).unwrap_or("");
            if raw.trim().is_empty() {
                if field.required {
                    return Err(format!("Field '{}' is required.", field.name));
                }
                continue;
            }

            if field.field_type == HumanInputType::Select
                && !field.options.iter().any(|option| option == raw)
            {
                return Err(format!(
                    "'{raw}' is not a permitted value for field '{}'; permitted values: {}.",
                    field.name,
                    field.options.join(", ")
                ));
            }

            values.insert(field.name.clone(), Value::String(raw.to_string()));
        }

        let mut delivery = HumanInputDelivery::with_values(values);
        if let Some(label) = self.chosen_label() {
            delivery = delivery.choosing(label);
        } else if !self.label_choices.is_empty() && !infers_label(&self.fields) {
            return Err(
                "Pick an output label: this state gives nothing to infer one from.".to_string(),
            );
        }
        Ok(delivery)
    }
}

/// Whether a single `select` field lets the engine infer the output label.
fn infers_label(fields: &[HumanInputField]) -> bool {
    fields
        .iter()
        .filter(|field| field.field_type == HumanInputType::Select)
        .count()
        == 1
}

// ---------------------------------------------------------------------------
// The repository registry
// ---------------------------------------------------------------------------

/// One row of the repository form, in the order the rows are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositoryField {
    /// The id a workflow writes in `globals.workspace.repository`.
    Id,
    /// Anything `git clone` accepts.
    Remote,
    /// Display name; defaults to the id.
    Name,
    /// Where the repository is hosted; defaults to `git`.
    Provider,
    /// Branch used when a workflow declares no `ref`.
    DefaultBranch,
    /// Free-form note.
    Description,
}

impl RepositoryField {
    /// Every row, in drawing order.
    pub const ALL: [RepositoryField; 6] = [
        RepositoryField::Id,
        RepositoryField::Remote,
        RepositoryField::Name,
        RepositoryField::Provider,
        RepositoryField::DefaultBranch,
        RepositoryField::Description,
    ];

    /// The label drawn in front of the value.
    pub fn label(self) -> &'static str {
        match self {
            RepositoryField::Id => "id",
            RepositoryField::Remote => "remote",
            RepositoryField::Name => "name",
            RepositoryField::Provider => "provider",
            RepositoryField::DefaultBranch => "default branch",
            RepositoryField::Description => "description",
        }
    }

    /// Whether the entry cannot be built without it.
    pub fn required(self) -> bool {
        matches!(self, RepositoryField::Id | RepositoryField::Remote)
    }

    /// The row's index into [`RepositoryForm::values`].
    pub fn index(self) -> usize {
        RepositoryField::ALL
            .iter()
            .position(|field| *field == self)
            .unwrap_or(0)
    }
}

/// The form that registers a repository — the TUI's `narsor repo add`.
///
/// Every value is a plain text line: the registry's own
/// [`RepositoryEntry::validate`] is what judges them, so the form never invents
/// a rule the CLI does not have.
#[derive(Debug, Clone)]
pub struct RepositoryForm {
    /// The typed value of each row, by [`RepositoryField::index`].
    pub values: Vec<String>,
    /// The focused row.
    pub row: usize,
    /// The last rejection, shown in the form and keeping it open.
    pub error: Option<String>,
}

impl Default for RepositoryForm {
    fn default() -> Self {
        Self::new()
    }
}

impl RepositoryForm {
    /// An empty form.
    pub fn new() -> Self {
        Self {
            values: vec![String::new(); RepositoryField::ALL.len()],
            row: 0,
            error: None,
        }
    }

    /// The trimmed value of one row.
    pub fn value(&self, field: RepositoryField) -> &str {
        self.values
            .get(field.index())
            .map(String::as_str)
            .unwrap_or("")
            .trim()
    }

    /// Builds the entry and runs the registry's validation on it.
    ///
    /// Deliberately *before* anything is stored: `add()` would reject the same
    /// entry, but the operator would read a generic failure instead of the rule
    /// they broke, and `ext::` remotes are exactly the case where that matters.
    pub fn entry(&self) -> Result<RepositoryEntry, String> {
        for field in RepositoryField::ALL {
            if field.required() && self.value(field).is_empty() {
                return Err(format!("Field '{}' is required.", field.label()));
            }
        }

        let id = self.value(RepositoryField::Id);
        let mut entry = RepositoryEntry::new(id, self.value(RepositoryField::Remote));

        let name = self.value(RepositoryField::Name);
        if !name.is_empty() {
            entry = entry.with_name(name);
        }
        let provider = self.value(RepositoryField::Provider);
        if !provider.is_empty() {
            entry = entry.with_provider(provider);
        }
        let branch = self.value(RepositoryField::DefaultBranch);
        if !branch.is_empty() {
            entry = entry.with_default_branch(branch);
        }
        let description = self.value(RepositoryField::Description);
        if !description.is_empty() {
            entry = entry.with_description(description);
        }

        entry.validate().map_err(|error| error.to_string())?;
        Ok(entry)
    }
}

/// The listing of what is registered — the TUI's `narsor repo list`.
///
/// A modal rather than a fourth panel: the registry is consulted when a task is
/// started, not while one is watched, and a permanent panel would spend screen
/// on it every second of a session that never touches it.
#[derive(Debug, Clone)]
pub struct RepositoryList {
    /// Every registered repository, in the registry's own order (by id).
    pub entries: Vec<RepositoryEntry>,
    /// The highlighted entry.
    pub selected: usize,
}

impl RepositoryList {
    /// The highlighted entry.
    pub fn current(&self) -> Option<&RepositoryEntry> {
        self.entries.get(self.selected)
    }
}

/// The confirmation a removal has to pass, as an override does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryRemoveConfirm {
    /// The repository that would be removed.
    pub id: String,
}

// ---------------------------------------------------------------------------
// The new-task form
// ---------------------------------------------------------------------------

/// Which half of the new-task modal is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewTaskStage {
    /// Picking a workflow out of the list the resolver offers.
    Pick,
    /// Typing the path of a definition that is in no search location.
    Definition,
    /// Filling in the inputs the loaded definition declares.
    Inputs,
}

/// One row of the new-task pick list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewTaskChoice {
    /// A workflow the resolver reached, with the source it came from.
    Workflow(WorkflowEntry),
    /// The escape hatch: type a path instead.
    Path,
}

impl NewTaskChoice {
    /// The row as the modal draws it.
    pub fn label(&self) -> String {
        match self {
            NewTaskChoice::Workflow(entry) => {
                let source = if entry.is_override() {
                    format!(
                        "{}, overrides {}",
                        entry.origin.label(),
                        entry.shadow_labels()
                    )
                } else {
                    entry.origin.label().to_string()
                };
                match &entry.description {
                    Some(description) => format!("{} [{source}] — {description}", entry.name),
                    None => format!("{} [{source}]", entry.name),
                }
            }
            NewTaskChoice::Path => "Enter a path…".to_string(),
        }
    }

    /// The workflow name this row starts, when it names one.
    pub fn name(&self) -> Option<&str> {
        match self {
            NewTaskChoice::Workflow(entry) => Some(entry.name.as_str()),
            NewTaskChoice::Path => None,
        }
    }
}

/// How one declared input is edited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEditor {
    /// A free line, for `string` and `integer`.
    Text,
    /// A cycled list: `true`/`false` for a `boolean`, the registered ids for a
    /// `repository`.
    Choice(Vec<String>),
}

/// The modal that starts a workflow — the TUI's `narsor run`.
///
/// Two stages in one overlay, because they are one operation: a path is typed,
/// the definition behind it is validated in full, and the form for its `inputs`
/// is generated from what was loaded. Nothing is created until the second stage
/// is submitted.
#[derive(Debug, Clone)]
pub struct NewTaskForm {
    /// The stage currently shown.
    pub stage: NewTaskStage,
    /// Every workflow that can be picked, plus the "enter a path…" row.
    pub choices: Vec<NewTaskChoice>,
    /// The highlighted row of the pick list.
    pub choice: usize,
    /// The typed definition path.
    pub path: String,
    /// Every violation the definition failed on — all of them, as `validate`
    /// prints them, never only the first.
    pub violations: Vec<String>,
    /// The last failure that is not a violation.
    pub error: Option<String>,
    /// The loaded definition, once the pick passed.
    pub definition: Option<WorkflowDefinition>,
    /// Where that definition came from — shown to the operator (the source
    /// label in the pick list) and, since U7, kept only for display: the
    /// daemon re-resolves from [`NewTaskForm::reference`] itself
    /// (`WorkflowResolver::resolve`, exactly as `narsor run <reference>`
    /// does), so this is no longer read to decide what gets sent over the
    /// wire.
    pub resolved: Option<ResolvedWorkflow>,
    /// The exact reference string this form resolved — a workflow name (from
    /// the pick list) or a typed path — passed to `client.start_task`
    /// verbatim (U7) so the daemon repeats the identical resolution this
    /// form already validated, rather than the form guessing a name back out
    /// of [`NewTaskForm::resolved`].
    pub reference: Option<String>,
    /// The declared parameters, in declaration order.
    pub parameters: Vec<InputParameter>,
    /// How each parameter is edited.
    pub editors: Vec<InputEditor>,
    /// The current value of each parameter, as typed or picked.
    pub values: Vec<String>,
    /// The focused parameter.
    pub row: usize,
}

impl Default for NewTaskForm {
    fn default() -> Self {
        Self::new()
    }
}

impl NewTaskForm {
    /// An empty form, waiting for a definition path.
    pub fn new() -> Self {
        Self {
            stage: NewTaskStage::Definition,
            choices: Vec::new(),
            choice: 0,
            path: String::new(),
            violations: Vec::new(),
            error: None,
            definition: None,
            resolved: None,
            reference: None,
            parameters: Vec::new(),
            editors: Vec::new(),
            values: Vec::new(),
            row: 0,
        }
    }

    /// A form opened on the pick list the resolver filled.
    pub fn picking(choices: Vec<NewTaskChoice>) -> Self {
        Self {
            stage: NewTaskStage::Pick,
            choices,
            ..Self::new()
        }
    }

    /// The highlighted row of the pick list.
    pub fn selected_choice(&self) -> Option<&NewTaskChoice> {
        self.choices.get(self.choice)
    }

    /// Moves the pick list's cursor, wrapping at both ends.
    pub fn cycle_choice(&mut self, step: isize) {
        let count = self.choices.len() as isize;
        if count == 0 {
            return;
        }
        self.choice = (self.choice as isize + step).rem_euclid(count) as usize;
    }

    /// The choices a parameter offers, when it is not a free line.
    pub fn choices(&self, index: usize) -> Option<&[String]> {
        match self.editors.get(index) {
            Some(InputEditor::Choice(options)) => Some(options),
            _ => None,
        }
    }

    /// Generates the input form from a loaded definition.
    ///
    /// `repositories` are the registered ids: a `repository` parameter offers
    /// them instead of asking the operator to retype an id the registry already
    /// knows. With nothing registered it falls back to a free line, so a raw
    /// remote — which the resolver still accepts — stays typeable.
    fn adopt(
        &mut self,
        reference: String,
        definition: WorkflowDefinition,
        resolved: ResolvedWorkflow,
        repositories: &[String],
    ) {
        self.reference = Some(reference);
        self.resolved = Some(resolved);
        self.parameters = definition.inputs.clone();
        self.editors = self
            .parameters
            .iter()
            .map(|parameter| match parameter.param_type {
                ParamType::Boolean => {
                    InputEditor::Choice(vec!["true".to_string(), "false".to_string()])
                }
                ParamType::Repository if !repositories.is_empty() => {
                    InputEditor::Choice(repositories.to_vec())
                }
                _ => InputEditor::Text,
            })
            .collect();
        self.values = self
            .parameters
            .iter()
            .zip(&self.editors)
            .map(|(parameter, editor)| match (&parameter.default, editor) {
                (Some(default), _) => default_as_text(default),
                // A boolean is never "unset" on screen, so it starts on the
                // value that changes nothing.
                (None, InputEditor::Choice(options))
                    if parameter.param_type == ParamType::Boolean =>
                {
                    options.last().cloned().unwrap_or_default()
                }
                (None, InputEditor::Choice(options)) => {
                    options.first().cloned().unwrap_or_default()
                }
                (None, InputEditor::Text) => String::new(),
            })
            .collect();
        self.definition = Some(definition);
        self.stage = NewTaskStage::Inputs;
        self.row = 0;
        self.violations.clear();
        self.error = None;
    }

    /// Turns the typed values into the execution's inputs.
    ///
    /// The declared `type` decides the JSON type, exactly as the CLI's
    /// `--input` parsing does: `is_experimental=true` reaches the context as a
    /// boolean, not as the string `"true"`.
    pub fn inputs(&self) -> Result<Map<String, Value>, String> {
        let mut values = Map::new();

        for (index, parameter) in self.parameters.iter().enumerate() {
            let raw = self
                .values
                .get(index)
                .map(String::as_str)
                .unwrap_or("")
                .trim();

            if raw.is_empty() {
                if let Some(default) = &parameter.default {
                    values.insert(parameter.name.clone(), default.clone());
                    continue;
                }
                if parameter.required {
                    return Err(format!("Input '{}' is required.", parameter.name));
                }
                continue;
            }

            let value = match parameter.param_type {
                ParamType::String | ParamType::Repository => Value::String(raw.to_string()),
                ParamType::Integer => raw.parse::<i64>().map(Value::from).map_err(|_| {
                    format!(
                        "Input '{}' expects an integer, not '{raw}'.",
                        parameter.name
                    )
                })?,
                ParamType::Boolean => match raw.to_ascii_lowercase().as_str() {
                    "true" => Value::Bool(true),
                    "false" => Value::Bool(false),
                    other => {
                        return Err(format!(
                            "Input '{}' expects true or false, not '{other}'.",
                            parameter.name
                        ))
                    }
                },
            };
            values.insert(parameter.name.clone(), value);
        }

        Ok(values)
    }
}

/// A declared default, as the form shows it.
fn default_as_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// The manual transition menu
// ---------------------------------------------------------------------------

/// Which set of targets the transition menu offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionMode {
    /// The labels the current state declares — the default.
    Outputs,
    /// An arbitrary state, for a state that allows it.
    Override,
}

/// The manual transition menu (D30).
#[derive(Debug, Clone)]
pub struct TransitionMenu {
    /// The mode currently shown.
    pub mode: TransitionMode,
    /// The declared output labels of the current state.
    pub outputs: Vec<String>,
    /// The description of each entry of `outputs`, by index; `None` where
    /// the workflow gives none.
    pub output_descriptions: Vec<Option<String>>,
    /// Every other state of the workflow, for the override mode.
    pub states: Vec<String>,
    /// The selected entry of the current mode's list.
    pub selected: usize,
    /// Whether the current state declares `allow_manual_override`.
    pub override_allowed: bool,
}

impl TransitionMenu {
    /// The entries of the mode currently shown.
    pub fn entries(&self) -> &[String] {
        match self.mode {
            TransitionMode::Outputs => &self.outputs,
            TransitionMode::Override => &self.states,
        }
    }

    /// The highlighted entry.
    pub fn current(&self) -> Option<&String> {
        self.entries().get(self.selected)
    }

    /// The description of the entry at `index` of the mode currently shown.
    /// Only declared outputs carry one; an override target never does.
    pub fn description(&self, index: usize) -> Option<&str> {
        match self.mode {
            TransitionMode::Outputs => self.output_descriptions.get(index)?.as_deref(),
            TransitionMode::Override => None,
        }
    }
}

/// The confirmation step a forced transition has to pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverrideConfirm {
    /// The state the execution would be moved to.
    pub target: String,
    /// The state it is in now.
    pub from: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptConfirm;

/// The scrollable window opened by `Enter` on an event (R1).
#[derive(Debug, Clone)]
pub struct EventDetail {
    /// The event the window was opened for.
    ///
    /// A private copy rather than an index into [`App::events`]: a live event
    /// arriving on the stream while the window is open must not shift what it
    /// shows out from under the operator (R3's "stable while open" is implicit
    /// in every other modal here, and this one is no different).
    pub event: Event,
    /// The window's own scroll cursor.
    pub scroll: ScrollState,
}

/// The daemon-backed default-harness picker.
///
/// `snapshot` is deliberately absent while an open is loading: nothing in the
/// TUI guesses the configured, overridden, or effective value.  A failed
/// request leaves the modal open, so its error can be read and the operator
/// can retry by closing and reopening it.
#[derive(Debug, Clone)]
pub struct HarnessSelectionModal {
    /// The last server-confirmed selection, if a read or mutation succeeded.
    pub snapshot: Option<HarnessSelectionSnapshot>,
    /// Supported-harness row currently highlighted; `supported.len()` is the
    /// final "use configured value" reset row.
    pub selected: usize,
    /// The most recent request failure, kept visible in the modal.
    pub error: Option<String>,
}

impl HarnessSelectionModal {
    fn loading() -> Self {
        Self {
            snapshot: None,
            selected: 0,
            error: None,
        }
    }

    fn accept_snapshot(&mut self, snapshot: HarnessSelectionSnapshot) {
        if self
            .snapshot
            .as_ref()
            .is_some_and(|current| snapshot.revision < current.revision)
        {
            return;
        }
        self.selected = snapshot
            .supported
            .iter()
            .position(|harness| harness == &snapshot.effective)
            .unwrap_or(0);
        self.snapshot = Some(snapshot);
        self.error = None;
    }
}

/// The overlay drawn on top of the layout, if any.
#[derive(Debug, Clone)]
pub enum Modal {
    /// No overlay.
    None,
    /// The generated human-input form.
    Input(InputForm),
    /// Operator note for a recovered agent attempt.
    RecoveryComment(RecoveryCommentForm),
    /// The full content of one event (R1, R2, R3).
    EventDetail(EventDetail),
    /// The manual transition menu.
    Transition(TransitionMenu),
    /// The confirmation of a forced transition.
    Confirm(OverrideConfirm),
    InterruptConfirm(InterruptConfirm),
    /// The listing of registered repositories.
    Repositories(RepositoryList),
    /// The form that registers a repository.
    RepositoryForm(RepositoryForm),
    /// The confirmation of a repository removal.
    RepositoryRemove(RepositoryRemoveConfirm),
    /// The form that starts a workflow.
    NewTask(Box<NewTaskForm>),
    /// The daemon-backed default harness picker.
    HarnessSelection(HarnessSelectionModal),
    /// The key map.
    Help,
}

impl Modal {
    /// Whether an overlay is open.
    pub fn is_open(&self) -> bool {
        !matches!(self, Modal::None)
    }
}

// ---------------------------------------------------------------------------
// The full-screen task view (U7)
// ---------------------------------------------------------------------------

/// How many logical lines a collapsed conversation entry shows before it is
/// elided, until the operator expands it (R10). Deferred to implementation
/// by the plan; six lines is enough to recognize most entries at a glance
/// without letting one drown the rest of the pane.
pub const TASK_ENTRY_COLLAPSE_LINES: usize = 6;

/// The byte budget passed on every conversation read the task view makes.
/// Matches the API's own default (`src/api/routes.rs`) — a generous soft cap
/// a single very large entry always overrides (R14 applies to a read the same
/// as to a write).
const CONVERSATION_BYTE_BUDGET: usize = 1024 * 1024;

/// Which full-screen view is showing.
#[derive(Debug, Clone)]
pub enum Screen {
    /// The four-panel main interface, with whatever [`Modal`] is open.
    Main,
    /// The full-screen task view: step timeline and step content (R6).
    ///
    /// Boxed: [`TaskView`] carries a whole [`Timeline`], and [`Screen::Main`]
    /// carries nothing, so an unboxed variant would size every `Screen`
    /// (most of which are `Main`, while the task view is open) to the
    /// larger one for no benefit.
    Task(Box<TaskView>),
}

/// Where the task view was entered from.
///
/// Keyed by the event's own `sequence` rather than its index in
/// [`App::events`], so growth of that list while the task view is open (a
/// live event arriving) cannot invalidate the selection `Esc` restores (R6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskOrigin {
    /// Entered with `Enter` on the execution list. Nothing about the
    /// Executions panel's own selection changes while the task view is
    /// open, so there is nothing further to remember here.
    Executions,
    /// Entered with `c`, either from the Events panel or from
    /// [`Modal::EventDetail`] (both dispatch through the same
    /// [`App::request_conversation`]). `was_following` is the Events panel's
    /// `follow` flag at the moment of entry: it is paused for the duration
    /// of the task view (a live event arriving must not silently move the
    /// selection `Esc` is about to restore) and reinstated on the way out.
    Event { sequence: u64, was_following: bool },
}

/// Which pane of the task view has focus (`Tab` cycles between them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskFocus {
    /// The step timeline.
    Axis,
    /// The selected step's content (a conversation, or a fallback event
    /// list).
    Content,
}

/// The selected step's content, once loaded.
#[derive(Debug, Clone)]
pub enum TaskContent {
    /// Nothing loaded yet (a fresh entry into the task view, before the
    /// first read completes).
    Empty,
    /// A conversation record, decoded and classified (U7's `conversation`
    /// module).
    Conversation {
        /// Entries read so far, oldest first.
        entries: Vec<ClassifiedEntry>,
        /// The cursor to continue from on the next manual refresh (R8).
        next_entry: u64,
        /// Whether the attempt's conversation is closed (compressed).
        closed: bool,
        /// Whether the on-disk file's last line was incomplete when read.
        truncated_tail: bool,
    },
    /// No conversation to show — a script step, an agent step whose
    /// conversation never started, or an old-format log (R11) — so the
    /// step's own events are shown instead, with `message` explaining why
    /// there is no conversation.
    Events {
        /// "no conversation record" / "conversation not created: …".
        message: String,
        /// This position's own events, in order.
        events: Vec<Event>,
    },
}

/// The full-screen task view's own state (R6).
#[derive(Debug, Clone)]
pub struct TaskView {
    /// The execution this task view is over.
    pub execution_id: String,
    /// Where it was entered from, for `Esc` (R6).
    pub origin: TaskOrigin,
    /// The step timeline, as last read from the API.
    pub timeline: Timeline,
    /// The selected position's label, if any position exists.
    pub selected: Option<String>,
    /// The axis' scroll cursor: one logical line per position, in timeline
    /// order (the shared widget from U6, `src/tui/scroll.rs`).
    pub axis_scroll: ScrollState,
    /// The content pane's scroll cursor: one logical line per entry (a
    /// conversation entry, or a fallback event).
    pub content_scroll: ScrollState,
    /// Which pane has focus.
    pub focus: TaskFocus,
    /// The selected position's content.
    pub content: TaskContent,
    /// Which conversation entries (by `entry_number`) are expanded past
    /// [`TASK_ENTRY_COLLAPSE_LINES`] (R10).
    pub expanded: HashSet<u64>,
}

// ---------------------------------------------------------------------------
// The application
// ---------------------------------------------------------------------------

/// Everything the interface knows.
pub struct App {
    /// Path arithmetic only (U7): used solely to find an execution's pinned
    /// definition copy ([`tasks::resolve_definition`]). Never opened as an
    /// [`crate::persistence::ExecutionStore`] — that would put this process
    /// back in the business of reading the store's on-disk layout directly,
    /// which is exactly what this unit removes (R4).
    state: StatePaths,
    resolver: WorkflowResolver,

    executions: Vec<ExecutionRow>,
    selected: usize,

    execution: Option<ExecutionSummary>,
    definition: Option<WorkflowDefinition>,
    definition_error: Option<String>,
    pending: Option<PendingHumanInput>,
    variables: Vec<(String, Value)>,
    variable_cursor: usize,

    events: Vec<Event>,
    event_cursor: usize,
    follow: bool,

    focus: Panel,
    modal: Modal,
    screen: Screen,
    stream: StreamStatus,
    notice: Option<Notice>,
    quit: bool,
    catalog: CatalogPaths,
    /// Kept for constructor symmetry with the CLI's own "three roots" and
    /// for any future feature that needs it — nothing currently reads it:
    /// U7 moved workspace provisioning (the one thing this used to be read
    /// for, in `start_new_task`) behind `client.start_task`, entirely onto
    /// the daemon.
    #[allow(dead_code)]
    work: WorkPaths,
    client: ApiClient,
}

impl App {
    /// Builds an interface over a daemon client. Nothing is read until
    /// [`App::refresh`].
    ///
    /// The catalogue and the work tree are handed in rather than recovered
    /// from a store this process opened itself — U7 removed the last such
    /// store, so there is nothing left to recover them from; they are read
    /// directly off the configuration exactly as every CLI command reads
    /// them.
    ///
    /// `client` is the interface's only path to the daemon (U7: the *only*
    /// engine this process ever touches is the daemon's) — reads (the
    /// execution list, the step timeline, conversations), the live SSE
    /// stream, and every write (starting a task, resuming, delivering human
    /// input, a manual/forced transition) all go through it. `state` is
    /// path arithmetic only, used solely to find an execution's pinned
    /// definition copy (see [`App`]'s own field doc).
    pub fn new(
        state: StatePaths,
        catalog: CatalogPaths,
        work: WorkPaths,
        client: ApiClient,
    ) -> Self {
        let resolver = WorkflowResolver::new(catalog.root().to_path_buf());
        Self::with_resolver(state, resolver, catalog, work, client)
    }

    /// The same, with the workflow search path given explicitly.
    pub fn with_resolver(
        state: StatePaths,
        resolver: WorkflowResolver,
        catalog: CatalogPaths,
        work: WorkPaths,
        client: ApiClient,
    ) -> Self {
        Self {
            state,
            resolver,
            catalog,
            work,
            client,
            executions: Vec::new(),
            selected: 0,
            execution: None,
            definition: None,
            definition_error: None,
            pending: None,
            variables: Vec::new(),
            variable_cursor: 0,
            events: Vec::new(),
            event_cursor: 0,
            follow: true,
            focus: Panel::Executions,
            modal: Modal::None,
            screen: Screen::Main,
            stream: StreamStatus::Live,
            notice: None,
            quit: false,
        }
    }

    // -- read-only accessors, for the views and the tests ------------------

    /// The execution list, newest first.
    pub fn executions(&self) -> &[ExecutionRow] {
        &self.executions
    }

    /// The index of the highlighted execution.
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// The highlighted execution's row.
    pub fn selected(&self) -> Option<&ExecutionRow> {
        self.executions.get(self.selected)
    }

    /// The highlighted execution's summary, as the daemon last reported it
    /// (`GET /executions`, U7 — no longer a store read).
    pub fn execution(&self) -> Option<&ExecutionSummary> {
        self.execution.as_ref()
    }

    /// The definition the highlighted execution runs, when it could be found.
    pub fn definition(&self) -> Option<&WorkflowDefinition> {
        self.definition.as_ref()
    }

    /// Why the definition could not be found, if it could not.
    pub fn definition_error(&self) -> Option<&str> {
        self.definition_error.as_deref()
    }

    /// The question the highlighted execution is waiting on.
    pub fn pending(&self) -> Option<&PendingHumanInput> {
        self.pending.as_ref()
    }

    /// The context variables, sorted by name.
    pub fn variables(&self) -> &[(String, Value)] {
        &self.variables
    }

    /// The highlighted context variable.
    pub fn variable_cursor(&self) -> usize {
        self.variable_cursor
    }

    /// The event stream of the highlighted execution, oldest first.
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// The highlighted event.
    pub fn event_cursor(&self) -> usize {
        self.event_cursor
    }

    /// Whether the stream view sticks to the newest entry.
    pub fn follows_stream(&self) -> bool {
        self.follow
    }

    /// The panel that has focus.
    pub fn focus(&self) -> Panel {
        self.focus
    }

    /// The overlay currently drawn on top of the layout.
    pub fn modal(&self) -> &Modal {
        &self.modal
    }

    /// Which full-screen view is showing: the main interface, or the task
    /// view (U7).
    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    /// The health of the live channel.
    pub fn stream_status(&self) -> &StreamStatus {
        &self.stream
    }

    /// The message in the bottom bar.
    pub fn notice(&self) -> Option<&Notice> {
        self.notice.as_ref()
    }

    /// Whether the loop should stop.
    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// Asks the loop to stop.
    pub fn quit(&mut self) {
        self.quit = true;
    }

    /// The current state of the highlighted execution, as the definition
    /// declares it.
    pub fn current_state(&self) -> Option<&State> {
        let execution = self.execution.as_ref()?;
        let definition = self.definition.as_ref()?;
        definition.states.get(execution.current_state.as_deref()?)
    }

    /// The labels the current state declares — exactly its `outputs`, in
    /// declaration order.
    pub fn permitted_transitions(&self) -> Vec<String> {
        self.current_state()
            .map(permitted_labels)
            .unwrap_or_default()
    }

    /// Whether the current state allows a forced transition.
    pub fn override_allowed(&self) -> bool {
        self.current_state().is_some()
    }

    /// The states a forced transition may target: every state but the current
    /// one, and only when the current state allows an override at all.
    pub fn override_targets(&self) -> Vec<String> {
        let Some(definition) = self.definition.as_ref() else {
            return Vec::new();
        };
        let current = self
            .execution
            .as_ref()
            .and_then(|execution| execution.current_state.clone());

        let mut targets: Vec<String> = definition
            .states
            .keys()
            .filter(|id| Some(id.as_str()) != current.as_deref())
            .cloned()
            .collect();
        targets.sort();
        targets
    }

    /// The shortcuts the bottom bar shows, given the focus and the modal.
    pub fn shortcuts(&self) -> Vec<(&'static str, &'static str)> {
        if let Screen::Task(view) = &self.screen {
            let mut keys = vec![
                ("Tab", "axis/content"),
                ("j/k", "select"),
                ("PgUp/PgDn", "page"),
                ("g/G", "top/bottom"),
                ("r", "refresh"),
            ];
            if view.focus == TaskFocus::Content {
                keys.push(("Enter/space", "expand/collapse"));
            }
            keys.push(("Esc", "back"));
            return keys;
        }
        match &self.modal {
            Modal::Input(_) => vec![
                ("Tab/↑↓", "field"),
                ("←/→", "value"),
                ("letters", "type"),
                ("Enter", "submit"),
                ("Esc", "cancel"),
            ],
            Modal::RecoveryComment(_) => vec![
                ("letters", "type operator note"),
                ("Enter", "save note"),
                ("Esc", "cancel"),
            ],
            Modal::Transition(menu) => {
                let mut keys = vec![("j/k", "select"), ("Enter", "apply")];
                if menu.override_allowed {
                    keys.push(("o", "override mode"));
                }
                keys.push(("Esc", "cancel"));
                keys
            }
            Modal::EventDetail(_) => vec![
                ("j/k", "scroll"),
                ("PgUp/PgDn", "page"),
                ("g/G", "top/bottom"),
                ("c", "conversation"),
                ("Esc", "close"),
            ],
            Modal::Confirm(_) => vec![("y", "confirm"), ("n/Esc", "cancel")],
            Modal::InterruptConfirm(_) => vec![("y", "interrupt"), ("n/Esc", "cancel")],
            Modal::RepositoryRemove(_) => vec![("y", "remove"), ("n/Esc", "cancel")],
            Modal::Repositories(_) => vec![
                ("j/k", "select"),
                ("a", "register"),
                ("d", "remove"),
                ("Esc", "close"),
            ],
            Modal::RepositoryForm(_) => vec![
                ("Tab/↑↓", "field"),
                ("letters", "type"),
                ("Enter", "register"),
                ("Esc", "cancel"),
            ],
            Modal::NewTask(form) => match form.stage {
                NewTaskStage::Pick => vec![
                    ("←/→", "workflow"),
                    ("j/k", "workflow"),
                    ("Enter", "choose"),
                    ("Esc", "cancel"),
                ],
                NewTaskStage::Definition => vec![
                    ("letters", "type the path"),
                    ("Enter", "load"),
                    ("Esc", "back to the list"),
                ],
                NewTaskStage::Inputs => vec![
                    ("Tab/↑↓", "field"),
                    ("←/→", "value"),
                    ("letters", "type"),
                    ("Enter", "start"),
                    ("Esc", "cancel"),
                ],
            },
            Modal::HarnessSelection(_) => {
                vec![("j/k", "select"), ("Enter", "apply"), ("Esc", "close")]
            }
            Modal::Help => vec![("Esc/?", "close")],
            Modal::None => {
                let mut keys = vec![("Tab", "panel"), ("r", "refresh")];
                match self.focus {
                    Panel::Executions => {
                        keys.push(("j/k", "select"));
                        keys.push(("g/G", "top/bottom"));
                        keys.push(("Enter", "task view"));
                        keys.push(("n", "new task"));
                        keys.push(("R", "repositories"));
                        keys.push(("s", "run/resume"));
                        if self.pending.as_ref().is_some_and(|pending| {
                            pending.continuation == InputContinuation::ResumeAgent
                        }) {
                            keys.push(("i", "reply to agent"));
                        }
                        keys.push(("t", "transition"));
                        keys.push(("x", "interrupt"));
                    }
                    Panel::Details => {
                        keys.push(("j/k", "variables"));
                        if self.pending.as_ref().is_some_and(|pending| {
                            pending.continuation == InputContinuation::ResumeAgent
                        }) {
                            keys.push(("i", "reply to agent"));
                        }
                        keys.push(("t", "transition"));
                        keys.push(("x", "interrupt"));
                    }
                    Panel::Events => {
                        keys.push(("j/k", "scroll"));
                        keys.push(("g/G", "top/bottom"));
                        keys.push(("f", "follow"));
                        keys.push(("Enter", "details"));
                        keys.push(("c", "conversation"));
                    }
                }
                keys.push(("?", "help"));
                keys.push(("h", "harness"));
                keys.push(("q", "quit"));
                keys
            }
        }
    }

    // -- loading (U7: every read below is a client call, never a store read) --

    /// Re-reads the execution list and the selected execution.
    ///
    /// The selection follows the execution id, not the index, so a new
    /// execution appearing at the top of the list does not move the operator's
    /// cursor onto a different workflow.
    ///
    /// `GET /executions` is the one read the whole interface now needs: the
    /// list (this method) and a selection's detail
    /// ([`App::reload_selection`]) are both just filters over the same
    /// response — see [`ExecutionSummary`]'s own doc for why that response
    /// carries `variables`/`metadata`/`pending_input` on top of the shape
    /// U3/U5 originally gave it.
    pub async fn refresh(&mut self) {
        let previous = self.selected().map(|row| row.id.clone());

        let listing = match self.client.list_executions().await {
            Ok(listing) => listing,
            Err(error) => {
                self.notice = Some(Notice::error(format!(
                    "Could not read the execution list: {error}"
                )));
                return;
            }
        };

        self.executions = listing
            .executions
            .into_iter()
            .map(|summary| ExecutionRow {
                id: summary.id,
                workflow_id: summary.workflow_id,
                status: summary.status,
                current_state: summary.current_state,
            })
            .collect();

        self.selected = previous
            .and_then(|id| self.executions.iter().position(|row| row.id == id))
            .unwrap_or(0);
        if self.selected >= self.executions.len() {
            self.selected = self.executions.len().saturating_sub(1);
        }

        self.reload_selection().await;
    }

    /// Re-reads everything that depends on which execution is selected — a
    /// second `GET /executions`, filtered down to the one row this needs,
    /// rather than threading the listing [`App::refresh`] already has
    /// through every caller of this method (cursor movement, `select_execution`,
    /// a mutation's own follow-up reload): simplicity over saving one small
    /// request on a loopback connection.
    pub async fn reload_selection(&mut self) {
        self.execution = None;
        self.definition = None;
        self.definition_error = None;
        self.pending = None;
        self.variables.clear();
        self.variable_cursor = 0;
        self.events.clear();
        self.event_cursor = 0;

        let Some(id) = self.selected().map(|row| row.id.clone()) else {
            return;
        };

        let summary = match self.client.list_executions().await {
            Ok(listing) => listing.executions.into_iter().find(|row| row.id == id),
            Err(error) => {
                self.notice = Some(Notice::error(format!(
                    "Could not load execution '{id}': {error}"
                )));
                return;
            }
        };
        let Some(summary) = summary else {
            self.notice = Some(Notice::error(format!(
                "Could not load execution '{id}': not found"
            )));
            return;
        };

        let mut variables: Vec<(String, Value)> = summary.variables.clone().into_iter().collect();
        variables.sort_by(|left, right| left.0.cmp(&right.0));
        self.variables = variables;
        self.pending = summary.pending_input.clone();

        // An execution with no events is an empty stream, not a failure: it has
        // simply not got anywhere yet.
        match self.client.events(&id, 0).await {
            Ok(events) => {
                self.events = events;
                self.event_cursor = self.events.len().saturating_sub(1);
            }
            Err(error) => {
                self.notice = Some(Notice::error(format!(
                    "Could not read the event log: {error}"
                )));
            }
        }

        self.load_definition(&summary);
        self.execution = Some(summary);
    }

    /// Resolves the definition the selected execution runs.
    ///
    /// The same rule the CLI uses, through the same resolver: the recorded
    /// path while that file is there, and otherwise the recorded workflow
    /// name — which is all an execution started from a built-in has.
    /// `tasks::resolve_definition` takes a whole [`WorkflowExecution`], which
    /// this `App` no longer holds (only [`ExecutionSummary`]) — so this
    /// builds the minimal stand-in [`execution_stub`] carries the two fields
    /// that function actually reads (`id`, `metadata`).
    fn load_definition(&mut self, summary: &ExecutionSummary) {
        let stub = execution_stub(&summary.id, &summary.workflow_id, summary.metadata.clone());

        let resolved = match tasks::resolve_definition(self.catalog.root(), &self.state, &stub) {
            Ok(resolved) => resolved,
            Err(error) => {
                self.definition_error = Some(format!(
                    "{error} Manual transitions and delivering human input are unavailable."
                ));
                return;
            }
        };

        match resolved.load() {
            Ok(definition) => self.definition = Some(definition),
            Err(error) => {
                self.definition_error = Some(format!(
                    "Could not load definition '{}' ({}): {error}",
                    resolved.name, resolved.origin
                ))
            }
        }
    }

    // -- the live stream ----------------------------------------------------

    /// Folds one item off `client.stream_all()`'s SSE stream into the state
    /// (U7: this used to fold a `broadcast::Receiver::recv` result instead —
    /// see `src/tui/mod.rs`'s module doc for why the source changed but this
    /// method's job did not).
    ///
    /// `None` (the stream ended — the connection dropped, or the daemon
    /// stopped) and `Some(Err(_))` (a transport-level failure reported as one
    /// last item, per [`ApiClient::sse_stream`]) both become the same
    /// visible, permanent indicator a closed broadcast channel used to
    /// produce: the operator has to be able to tell "nothing is happening"
    /// from "I am no longer being told what happens". There is no
    /// [`StreamStatus::Lagged`] equivalent here — SSE over a kept-open HTTP
    /// connection has no bounded ring buffer to fall behind in the way an
    /// in-process `broadcast` channel did; either the connection is up and
    /// delivering everything it is asked to relay, or it is not up at all.
    pub fn on_stream(&mut self, received: Option<Result<Event, ClientError>>) {
        match received {
            Some(Ok(event)) => {
                self.stream = StreamStatus::Live;
                self.on_engine_event(event);
            }
            Some(Err(error)) => {
                self.stream = StreamStatus::Disconnected;
                self.notice = Some(Notice::error(format!(
                    "The event stream was closed: {error}. This view no longer updates live."
                )));
            }
            None => {
                self.stream = StreamStatus::Disconnected;
                self.notice = Some(Notice::error(
                    "The event stream was closed: this view no longer updates live.",
                ));
            }
        }
    }

    /// Appends one live event and keeps the panels in step with it.
    pub fn on_engine_event(&mut self, event: Event) {
        if let Some(row) = self
            .executions
            .iter_mut()
            .find(|row| row.id == event.workflow_execution_id)
        {
            apply_to_row(row, &event.data);
        }

        // A background run's outcome is set as a notice by execution id, not
        // only for whichever execution happens to be selected (R6) — this is
        // in addition to the row update above, not a replacement for it.
        if let Some(notice) = notice_for(&event) {
            self.notice = Some(notice);
        }

        let selected = self.selected().map(|row| row.id.clone());
        if selected.as_deref() != Some(event.workflow_execution_id.as_str()) {
            return;
        }

        // A duplicate can arrive when a refresh backfilled the log while the
        // same event was already in flight on the channel.
        if self
            .events
            .last()
            .is_some_and(|last| last.sequence >= event.sequence)
        {
            return;
        }

        self.events.push(event);
        if self.follow {
            self.event_cursor = self.events.len() - 1;
        }
    }

    // -- key handling -------------------------------------------------------

    /// Folds one key press into the state and names what the loop must do next.
    pub fn on_key(&mut self, key: KeyEvent) -> Request {
        if matches!(self.screen, Screen::Task(_)) {
            return self.on_key_task(key);
        }
        match &mut self.modal {
            Modal::None => self.on_key_panels(key),
            Modal::Help => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('?') | KeyCode::Enter) {
                    self.modal = Modal::None;
                }
                Request::None
            }
            Modal::Input(_) => self.on_key_form(key),
            Modal::RecoveryComment(_) => self.on_key_recovery_comment(key),
            Modal::EventDetail(_) => self.on_key_event_detail(key),
            Modal::Transition(_) => self.on_key_transition(key),
            Modal::Confirm(_) => self.on_key_confirm(key),
            Modal::InterruptConfirm(_) => self.on_key_interrupt_confirm(key),
            Modal::Repositories(_) => self.on_key_repositories(key),
            Modal::RepositoryForm(_) => self.on_key_repository_form(key),
            Modal::RepositoryRemove(_) => self.on_key_repository_remove(key),
            Modal::NewTask(_) => self.on_key_new_task(key),
            Modal::HarnessSelection(_) => self.on_key_harness_selection(key),
        }
    }

    /// Keys when no overlay is open.
    fn on_key_panels(&mut self, key: KeyEvent) -> Request {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.quit();
                Request::None
            }
            KeyCode::Tab => {
                self.focus = self.focus.next();
                Request::None
            }
            KeyCode::BackTab => {
                self.focus = self.focus.previous();
                Request::None
            }
            KeyCode::Char('?') => {
                self.modal = Modal::Help;
                Request::None
            }
            KeyCode::Char('r') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                Request::Refresh
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_cursor(-1),
            KeyCode::Char('g') | KeyCode::Home => self.jump_cursor(false),
            KeyCode::Char('G') | KeyCode::End => self.jump_cursor(true),
            KeyCode::Char('f') if self.focus == Panel::Events => {
                self.follow = !self.follow;
                if self.follow {
                    self.event_cursor = self.events.len().saturating_sub(1);
                }
                Request::None
            }
            KeyCode::Enter if self.focus == Panel::Events => {
                self.open_event_detail();
                Request::None
            }
            KeyCode::Enter if self.focus == Panel::Executions => {
                if self.selected().is_some() {
                    Request::OpenTaskView
                } else {
                    Request::None
                }
            }
            KeyCode::Char('c') if self.focus == Panel::Events => {
                let Some(event) = self.events.get(self.event_cursor).cloned() else {
                    return Request::None;
                };
                self.request_conversation(&event)
            }
            KeyCode::Char('i') => {
                self.open_input_or_recovery_comment();
                Request::None
            }
            KeyCode::Char('t') => {
                self.open_transition_menu();
                Request::None
            }
            KeyCode::Char('x') => {
                self.modal = Modal::InterruptConfirm(InterruptConfirm);
                Request::None
            }
            KeyCode::Char('n') => {
                self.open_new_task();
                Request::None
            }
            KeyCode::Char('R') => {
                self.open_repository_list();
                Request::None
            }
            KeyCode::Char('h') => {
                self.modal = Modal::HarnessSelection(HarnessSelectionModal::loading());
                Request::LoadHarnessSelection
            }
            KeyCode::Char('s') => Request::RunSelected,
            _ => Request::None,
        }
    }

    /// Moves the focused panel's cursor by `delta`.
    ///
    /// Returns [`Request::SelectExecution`] when the Executions panel's
    /// cursor actually moved (U7): unlike the other two panels, moving onto a
    /// different execution needs a fresh client read
    /// ([`App::reload_selection`]), which this synchronous method cannot
    /// await itself — the same seam [`App::after_task_cursor_move`] already
    /// uses for the task view's axis.
    fn move_cursor(&mut self, delta: isize) -> Request {
        match self.focus {
            Panel::Executions => {
                let length = self.executions.len();
                if let Some(next) = step(self.selected, delta, length) {
                    self.selected = next;
                    return Request::SelectExecution;
                }
                Request::None
            }
            Panel::Details => {
                if let Some(next) = step(self.variable_cursor, delta, self.variables.len()) {
                    self.variable_cursor = next;
                }
                Request::None
            }
            Panel::Events => {
                if let Some(next) = step(self.event_cursor, delta, self.events.len()) {
                    self.event_cursor = next;
                    // Scrolling away from the newest entry means the operator
                    // is reading, not watching.
                    self.follow = next + 1 == self.events.len();
                }
                Request::None
            }
        }
    }

    /// Jumps the focused panel's cursor to the first or last entry. See
    /// [`App::move_cursor`] for why the Executions branch returns a
    /// [`Request`] instead of reloading inline.
    fn jump_cursor(&mut self, to_end: bool) -> Request {
        match self.focus {
            Panel::Executions => {
                if self.executions.is_empty() {
                    return Request::None;
                }
                self.selected = if to_end { self.executions.len() - 1 } else { 0 };
                Request::SelectExecution
            }
            Panel::Details => {
                if self.variables.is_empty() {
                    return Request::None;
                }
                self.variable_cursor = if to_end { self.variables.len() - 1 } else { 0 };
                Request::None
            }
            Panel::Events => {
                if self.events.is_empty() {
                    return Request::None;
                }
                self.event_cursor = if to_end { self.events.len() - 1 } else { 0 };
                self.follow = to_end;
                Request::None
            }
        }
    }

    /// Opens the detail window on the highlighted event, if there is one.
    fn open_event_detail(&mut self) {
        let Some(event) = self.events.get(self.event_cursor).cloned() else {
            return;
        };
        self.modal = Modal::EventDetail(EventDetail {
            event,
            scroll: ScrollState::new(),
        });
    }

    /// The `c` key (R5): a [`Request::OpenConversation`] for an event that
    /// carries an attempt id, or a notice explaining why not (AE3).
    ///
    /// This local check only rules out the events that can *never* carry a
    /// conversation (they do not even carry an `action_execution_id` field)
    /// — the same fast, network-free path U6 had. Whatever passes it still
    /// goes through the real timeline link map in
    /// [`App::open_conversation_for_event`], which is what actually decides
    /// (and is what fixes U6's over-linking: a script step's `ActionStarted`
    /// passes this check too, but the timeline's `action_type` then refuses
    /// it).
    fn request_conversation(&mut self, event: &Event) -> Request {
        match conversation_attempt(&event.data) {
            Some(_) => Request::OpenConversation {
                sequence: event.sequence,
            },
            None => {
                self.notice = Some(Notice::error(NO_CONVERSATION_LINK_MESSAGE));
                Request::None
            }
        }
    }

    /// Keys inside the event detail window.
    fn on_key_event_detail(&mut self, key: KeyEvent) -> Request {
        let Modal::EventDetail(detail) = &self.modal else {
            return Request::None;
        };
        let total_lines = event_detail_lines(&detail.event).len();
        let event = detail.event.clone();

        match key.code {
            KeyCode::Esc => {
                self.modal = Modal::None;
                Request::None
            }
            KeyCode::Char('c') => self.request_conversation(&event),
            KeyCode::Char('j') | KeyCode::Down => {
                self.event_detail_scroll_mut().line_down(total_lines);
                Request::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.event_detail_scroll_mut().line_up(total_lines);
                Request::None
            }
            KeyCode::PageDown => {
                self.event_detail_scroll_mut().page_down(total_lines);
                Request::None
            }
            KeyCode::PageUp => {
                self.event_detail_scroll_mut().page_up(total_lines);
                Request::None
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.event_detail_scroll_mut().to_top();
                Request::None
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.event_detail_scroll_mut().to_bottom(total_lines);
                Request::None
            }
            _ => Request::None,
        }
    }

    /// The event detail window's scroll cursor.
    ///
    /// Panics if the modal is not [`Modal::EventDetail`]: every caller only
    /// reaches this from within [`App::on_key_event_detail`], which already
    /// holds that modal open.
    fn event_detail_scroll_mut(&mut self) -> &mut ScrollState {
        match &mut self.modal {
            Modal::EventDetail(detail) => &mut detail.scroll,
            _ => unreachable!("on_key_event_detail only runs while the event detail modal is open"),
        }
    }

    // -- the full-screen task view (U7) --------------------------------

    /// `Enter` on the execution list: opens the task view at the timeline's
    /// default position (R6).
    pub async fn open_task_for_execution(&mut self) {
        let Some(execution_id) = self.selected().map(|row| row.id.clone()) else {
            return;
        };

        let timeline = match self.client.timeline(&execution_id).await {
            Ok(timeline) => timeline,
            Err(error) => {
                self.notice = Some(Notice::error(format!(
                    "Could not open the task view: {error}"
                )));
                return;
            }
        };

        let selected = timeline.default_position.clone();
        self.enter_task_view(execution_id, TaskOrigin::Executions, timeline, selected);
        self.load_selected_task_position(false).await;
    }

    /// `c` on an event, resolved through the real timeline link map (R5) —
    /// this is what fixes U6's over-linking: a script step's own
    /// `ActionStarted`/`ActionCompleted`/... passes the local
    /// [`conversation_attempt`] check (it does carry an
    /// `action_execution_id`), but the timeline's `action_type` on the
    /// position it resolves to says it is not an agent step, so it is
    /// refused here exactly as an event with no link at all is.
    pub async fn open_conversation_for_event(&mut self, sequence: u64) {
        let Some(execution_id) = self.selected().map(|row| row.id.clone()) else {
            return;
        };

        let timeline = match self.client.timeline(&execution_id).await {
            Ok(timeline) => timeline,
            Err(error) => {
                self.notice = Some(Notice::error(format!(
                    "Could not open the conversation: {error}"
                )));
                return;
            }
        };

        let (selected, info) = match timeline.link(sequence) {
            Some(EventLink::Position(label)) => match timeline.position(label) {
                Some(position) if position.action_type.as_deref() == Some("agent") => {
                    (Some(label.clone()), None)
                }
                _ => {
                    self.notice = Some(Notice::error(NO_CONVERSATION_LINK_MESSAGE));
                    return;
                }
            },
            Some(EventLink::NoLink { reason }) => {
                self.notice = Some(Notice::error(reason.clone()));
                return;
            }
            None => (
                timeline.default_position.clone(),
                Some("No attempt found for that event; showing the latest.".to_string()),
            ),
        };

        let was_following = self.follow;
        self.follow = false;
        self.enter_task_view(
            execution_id,
            TaskOrigin::Event {
                sequence,
                was_following,
            },
            timeline,
            selected,
        );
        if let Some(message) = info {
            self.notice = Some(Notice::info(message));
        }
        self.load_selected_task_position(false).await;
    }

    /// Builds a fresh [`TaskView`] and switches [`Screen`] to it.
    fn enter_task_view(
        &mut self,
        execution_id: String,
        origin: TaskOrigin,
        timeline: Timeline,
        selected: Option<String>,
    ) {
        let axis_index = selected
            .as_ref()
            .and_then(|label| timeline.positions.iter().position(|p| &p.label == label))
            .unwrap_or(0);
        let mut axis_scroll = ScrollState::new();
        for _ in 0..axis_index {
            axis_scroll.select_down(timeline.positions.len());
        }

        self.screen = Screen::Task(Box::new(TaskView {
            execution_id,
            origin,
            timeline,
            selected,
            axis_scroll,
            content_scroll: ScrollState::new(),
            focus: TaskFocus::Axis,
            content: TaskContent::Empty,
            expanded: HashSet::new(),
        }));
    }

    /// `r` in the task view (R8): re-reads the timeline, then continues the
    /// selected position's conversation from its cursor. Nothing refreshes
    /// on its own; this is the only path that does.
    pub async fn refresh_task(&mut self) {
        let Screen::Task(view) = &self.screen else {
            return;
        };
        let execution_id = view.execution_id.clone();

        match self.client.timeline(&execution_id).await {
            Ok(timeline) => {
                if let Screen::Task(view) = &mut self.screen {
                    view.timeline = timeline;
                    if view
                        .selected
                        .as_deref()
                        .is_none_or(|label| view.timeline.position(label).is_none())
                    {
                        view.selected = view.timeline.default_position.clone();
                    }
                }
            }
            Err(error) => {
                self.notice = Some(Notice::error(format!(
                    "Could not refresh the task view: {error}"
                )));
                return;
            }
        }

        self.load_selected_task_position(true).await;
    }

    /// Loads the currently selected position's content from scratch —
    /// called whenever the axis' selection changes.
    pub async fn load_task_position(&mut self) {
        self.load_selected_task_position(false).await;
    }

    /// The label the task view's axis currently has selected, if any.
    fn current_task_label(&self) -> Option<String> {
        match &self.screen {
            Screen::Task(view) => view.selected.clone(),
            Screen::Main => None,
        }
    }

    /// Loads the selected position's content: a conversation (from entry 1,
    /// or continuing from the pane's own cursor when `append`), or the
    /// position's own events when it has none.
    async fn load_selected_task_position(&mut self, append: bool) {
        let Some(label) = self.current_task_label() else {
            return;
        };
        let Screen::Task(view) = &self.screen else {
            return;
        };
        let execution_id = view.execution_id.clone();
        let Some(position) = view.timeline.position(&label).cloned() else {
            return;
        };

        match &position.conversation {
            PositionConversationState::Recorded => {
                let Some(attempt_id) = position.action_execution_id.clone() else {
                    return;
                };
                let from = if append {
                    match &view.content {
                        TaskContent::Conversation { next_entry, .. } => *next_entry,
                        _ => 1,
                    }
                } else {
                    1
                };

                match self
                    .client
                    .conversation(&execution_id, &attempt_id, from, CONVERSATION_BYTE_BUDGET)
                    .await
                {
                    Ok(ConversationResponse::Recorded {
                        entries,
                        next_entry,
                        closed,
                        truncated_tail,
                    }) => {
                        let fresh: Vec<ClassifiedEntry> = entries.iter().map(classify).collect();
                        if let Screen::Task(view) = &mut self.screen {
                            let existing = if append {
                                match std::mem::replace(&mut view.content, TaskContent::Empty) {
                                    TaskContent::Conversation { entries, .. } => entries,
                                    _ => Vec::new(),
                                }
                            } else {
                                Vec::new()
                            };
                            let mut entries = existing;
                            entries.extend(fresh);
                            view.content = TaskContent::Conversation {
                                entries,
                                next_entry,
                                closed,
                                truncated_tail,
                            };
                        }
                    }
                    Ok(ConversationResponse::NoRecord) => {
                        self.set_task_events_fallback(&label, "no conversation record".to_string());
                    }
                    Err(error) => {
                        self.notice = Some(Notice::error(format!(
                            "Could not load the conversation: {error}"
                        )));
                    }
                }
            }
            PositionConversationState::NotCreated { reason } => {
                self.set_task_events_fallback(
                    &label,
                    format!("conversation not created: {reason}"),
                );
            }
            PositionConversationState::NoRecord => {
                self.set_task_events_fallback(&label, "no conversation record".to_string());
            }
        }
    }

    /// Fills the content pane with this position's own events (R11): a
    /// script step, an agent step whose conversation never started, or an
    /// old-format log.
    fn set_task_events_fallback(&mut self, label: &str, message: String) {
        let events: Vec<Event> = match &self.screen {
            Screen::Task(view) => self
                .events
                .iter()
                .filter(|event| {
                    matches!(
                        view.timeline.link(event.sequence),
                        Some(EventLink::Position(position_label)) if position_label == label
                    )
                })
                .cloned()
                .collect(),
            Screen::Main => Vec::new(),
        };

        if let Screen::Task(view) = &mut self.screen {
            view.content = TaskContent::Events { message, events };
        }
    }

    /// Keys while the task view is open.
    fn on_key_task(&mut self, key: KeyEvent) -> Request {
        match key.code {
            KeyCode::Esc => {
                self.leave_task_view();
                Request::None
            }
            KeyCode::Tab => {
                if let Screen::Task(view) = &mut self.screen {
                    view.focus = match view.focus {
                        TaskFocus::Axis => TaskFocus::Content,
                        TaskFocus::Content => TaskFocus::Axis,
                    };
                }
                Request::None
            }
            KeyCode::Char('r') => Request::TaskRefresh,
            KeyCode::Char('j') | KeyCode::Down => self.move_task_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_task_cursor(-1),
            KeyCode::PageDown => self.page_task_cursor(1),
            KeyCode::PageUp => self.page_task_cursor(-1),
            KeyCode::Char('g') | KeyCode::Home => self.jump_task_cursor(false),
            KeyCode::Char('G') | KeyCode::End => self.jump_task_cursor(true),
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.toggle_task_entry_expanded();
                Request::None
            }
            _ => Request::None,
        }
    }

    /// The number of logical lines the focused pane currently has.
    fn task_pane_len(view: &TaskView) -> usize {
        match view.focus {
            TaskFocus::Axis => view.timeline.positions.len(),
            TaskFocus::Content => match &view.content {
                TaskContent::Conversation { entries, .. } => entries.len(),
                TaskContent::Events { events, .. } => events.len(),
                TaskContent::Empty => 0,
            },
        }
    }

    /// Applies `delta` (one line, `+1`/`-1`) to the focused pane's cursor,
    /// re-syncing the selected label when the axis moved.
    fn move_task_cursor(&mut self, delta: isize) -> Request {
        let Screen::Task(view) = &mut self.screen else {
            return Request::None;
        };
        let total = Self::task_pane_len(view);
        let scroll = match view.focus {
            TaskFocus::Axis => &mut view.axis_scroll,
            TaskFocus::Content => &mut view.content_scroll,
        };
        // A selection cursor over the pane's discrete items (positions or
        // entries), not a scroll viewport — `select_down`/`select_up`, never
        // `line_down`/`line_up`, which would freeze at index 0 the moment
        // the list is shorter than the viewport (see `ScrollState`'s doc).
        if delta >= 0 {
            scroll.select_down(total);
        } else {
            scroll.select_up(total);
        }
        self.after_task_cursor_move()
    }

    /// Like [`App::move_task_cursor`], a page at a time.
    fn page_task_cursor(&mut self, direction: isize) -> Request {
        let Screen::Task(view) = &mut self.screen else {
            return Request::None;
        };
        let total = Self::task_pane_len(view);
        let scroll = match view.focus {
            TaskFocus::Axis => &mut view.axis_scroll,
            TaskFocus::Content => &mut view.content_scroll,
        };
        if direction >= 0 {
            scroll.select_page_down(total);
        } else {
            scroll.select_page_up(total);
        }
        self.after_task_cursor_move()
    }

    /// Jumps the focused pane's cursor to its first or last entry.
    fn jump_task_cursor(&mut self, to_end: bool) -> Request {
        let Screen::Task(view) = &mut self.screen else {
            return Request::None;
        };
        let total = Self::task_pane_len(view);
        let scroll = match view.focus {
            TaskFocus::Axis => &mut view.axis_scroll,
            TaskFocus::Content => &mut view.content_scroll,
        };
        if to_end {
            scroll.select_last(total);
        } else {
            scroll.select_first();
        }
        self.after_task_cursor_move()
    }

    /// After the focused pane's cursor moved: re-syncs the selected label
    /// from the axis' cursor, and asks for a fresh load when the axis is
    /// what moved (moving inside the content pane needs no new read).
    fn after_task_cursor_move(&mut self) -> Request {
        let Screen::Task(view) = &mut self.screen else {
            return Request::None;
        };
        if view.focus != TaskFocus::Axis {
            return Request::None;
        }
        let index = view.axis_scroll.offset();
        let label = view.timeline.positions.get(index).map(|p| p.label.clone());
        if label == view.selected {
            return Request::None;
        }
        view.selected = label;
        view.content = TaskContent::Empty;
        view.content_scroll = ScrollState::new();
        Request::TaskLoadSelected
    }

    /// `Enter`/space on the content pane (R10): expands or collapses the
    /// entry under the pane's cursor.
    fn toggle_task_entry_expanded(&mut self) {
        let Screen::Task(view) = &mut self.screen else {
            return;
        };
        if view.focus != TaskFocus::Content {
            return;
        }
        let TaskContent::Conversation { entries, .. } = &view.content else {
            return;
        };
        let Some(entry) = entries.get(view.content_scroll.offset()) else {
            return;
        };
        let number = entry.entry_number;
        if !view.expanded.insert(number) {
            view.expanded.remove(&number);
        }
    }

    /// `Esc` in the task view (R6): returns to the entry point, restoring
    /// the selection an event's origin remembers even if new events arrived
    /// (with `follow` paused for exactly this reason) while it was open.
    fn leave_task_view(&mut self) {
        let Screen::Task(view) = std::mem::replace(&mut self.screen, Screen::Main) else {
            return;
        };
        if let TaskOrigin::Event {
            sequence,
            was_following,
        } = view.origin
        {
            self.follow = was_following;
            if let Some(index) = self
                .events
                .iter()
                .position(|event| event.sequence == sequence)
            {
                self.event_cursor = index;
            }
        }
    }

    /// Keys inside the input form.
    fn on_key_form(&mut self, key: KeyEvent) -> Request {
        let Modal::Input(form) = &mut self.modal else {
            return Request::None;
        };
        let rows = form.rows();

        match key.code {
            KeyCode::Esc => {
                self.modal = Modal::None;
                Request::None
            }
            KeyCode::Enter => Request::SubmitInput,
            KeyCode::Tab | KeyCode::Down => {
                if !rows.is_empty() {
                    form.row = (form.row + 1) % rows.len();
                }
                Request::None
            }
            KeyCode::BackTab | KeyCode::Up => {
                if !rows.is_empty() {
                    form.row = (form.row + rows.len() - 1) % rows.len();
                }
                Request::None
            }
            KeyCode::Left => {
                cycle_choice(form, -1);
                Request::None
            }
            KeyCode::Right => {
                cycle_choice(form, 1);
                Request::None
            }
            KeyCode::Backspace => {
                if let Some(FormRow::Field(index)) = form.current_row() {
                    if form.fields[index].field_type == HumanInputType::Text {
                        form.values[index].pop();
                    }
                }
                Request::None
            }
            KeyCode::Char(character) => {
                match form.current_row() {
                    Some(FormRow::Field(index))
                        if form.fields[index].field_type == HumanInputType::Text =>
                    {
                        form.values[index].push(character);
                    }
                    // On a select or the label row there is nothing to type, so
                    // space steps to the next option.
                    _ if character == ' ' => cycle_choice(form, 1),
                    _ => {}
                }
                Request::None
            }
            _ => Request::None,
        }
    }

    /// Keys inside the free-form operator-note editor.
    fn on_key_recovery_comment(&mut self, key: KeyEvent) -> Request {
        let Modal::RecoveryComment(form) = &mut self.modal else {
            return Request::None;
        };
        match key.code {
            KeyCode::Esc => {
                self.modal = Modal::None;
                Request::None
            }
            KeyCode::Enter => Request::SaveOperatorNote {
                note: std::mem::take(&mut form.value),
            },
            KeyCode::Backspace => {
                form.value.pop();
                Request::None
            }
            KeyCode::Char(character) => {
                form.value.push(character);
                Request::None
            }
            _ => Request::None,
        }
    }

    /// Keys inside the transition menu.
    fn on_key_transition(&mut self, key: KeyEvent) -> Request {
        let Modal::Transition(menu) = &mut self.modal else {
            return Request::None;
        };

        match key.code {
            KeyCode::Esc => {
                self.modal = Modal::None;
                Request::None
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(next) = step(menu.selected, 1, menu.entries().len()) {
                    menu.selected = next;
                }
                Request::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(next) = step(menu.selected, -1, menu.entries().len()) {
                    menu.selected = next;
                }
                Request::None
            }
            KeyCode::Char('o') => {
                if menu.override_allowed {
                    menu.mode = match menu.mode {
                        TransitionMode::Outputs => TransitionMode::Override,
                        TransitionMode::Override => TransitionMode::Outputs,
                    };
                    menu.selected = 0;
                } else {
                    self.notice = Some(Notice::error(
                        "This state does not allow a forced transition (allow_manual_override).",
                    ));
                }
                Request::None
            }
            KeyCode::Enter => {
                let Some(target) = menu.current().cloned() else {
                    self.notice = Some(Notice::error("There is no transition to pick."));
                    return Request::None;
                };
                if menu.mode == TransitionMode::Override {
                    // A forced transition never happens on one keystroke.
                    let from = self
                        .execution
                        .as_ref()
                        .and_then(|execution| execution.current_state.clone())
                        .unwrap_or_else(|| "-".to_string());
                    self.modal = Modal::Confirm(OverrideConfirm { target, from });
                    return Request::None;
                }
                Request::ApplyTransition
            }
            _ => Request::None,
        }
    }

    /// Keys inside the override confirmation.
    fn on_key_confirm(&mut self, key: KeyEvent) -> Request {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Request::ApplyTransition,
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.modal = Modal::None;
                Request::None
            }
            _ => Request::None,
        }
    }

    fn on_key_interrupt_confirm(&mut self, key: KeyEvent) -> Request {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Request::InterruptSelected,
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.modal = Modal::None;
                Request::None
            }
            _ => Request::None,
        }
    }

    /// Keys inside the default harness picker.
    fn on_key_harness_selection(&mut self, key: KeyEvent) -> Request {
        let Modal::HarnessSelection(picker) = &mut self.modal else {
            return Request::None;
        };

        match key.code {
            KeyCode::Esc => {
                self.modal = Modal::None;
                Request::None
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let Some(snapshot) = &picker.snapshot else {
                    return Request::None;
                };
                picker.selected = (picker.selected + 1) % (snapshot.supported.len() + 1);
                Request::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let Some(snapshot) = &picker.snapshot else {
                    return Request::None;
                };
                let rows = snapshot.supported.len() + 1;
                picker.selected = (picker.selected + rows - 1) % rows;
                Request::None
            }
            KeyCode::Enter => {
                let Some(snapshot) = &picker.snapshot else {
                    return Request::None;
                };
                if picker.selected == snapshot.supported.len() {
                    Request::ResetHarnessSelection
                } else {
                    snapshot
                        .supported
                        .get(picker.selected)
                        .cloned()
                        .map(|harness| Request::SetHarnessSelection { harness })
                        .unwrap_or(Request::None)
                }
            }
            _ => Request::None,
        }
    }

    /// Keys inside the repository listing.
    fn on_key_repositories(&mut self, key: KeyEvent) -> Request {
        let Modal::Repositories(list) = &mut self.modal else {
            return Request::None;
        };

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.modal = Modal::None;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(next) = step(list.selected, 1, list.entries.len()) {
                    list.selected = next;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(next) = step(list.selected, -1, list.entries.len()) {
                    list.selected = next;
                }
            }
            KeyCode::Char('a') => {
                self.modal = Modal::RepositoryForm(RepositoryForm::new());
            }
            KeyCode::Char('d') => match list.current() {
                // Removal is a write, so it goes through its own confirmation,
                // the way a forced transition does.
                Some(entry) => {
                    let id = entry.id.clone();
                    self.modal = Modal::RepositoryRemove(RepositoryRemoveConfirm { id });
                }
                None => self.notice = Some(Notice::error("No repository is selected.")),
            },
            _ => {}
        }
        Request::None
    }

    /// Keys inside the repository form.
    fn on_key_repository_form(&mut self, key: KeyEvent) -> Request {
        let Modal::RepositoryForm(form) = &mut self.modal else {
            return Request::None;
        };
        let rows = RepositoryField::ALL.len();

        match key.code {
            KeyCode::Esc => self.modal = Modal::None,
            KeyCode::Enter => self.submit_repository(),
            KeyCode::Tab | KeyCode::Down => form.row = (form.row + 1) % rows,
            KeyCode::BackTab | KeyCode::Up => form.row = (form.row + rows - 1) % rows,
            KeyCode::Backspace => {
                form.values[form.row].pop();
            }
            KeyCode::Char(character) => form.values[form.row].push(character),
            _ => {}
        }
        Request::None
    }

    /// Keys inside the repository removal confirmation.
    fn on_key_repository_remove(&mut self, key: KeyEvent) -> Request {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => self.remove_repository(),
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => self.open_repository_list(),
            _ => {}
        }
        Request::None
    }

    /// Keys inside the new-task form.
    fn on_key_new_task(&mut self, key: KeyEvent) -> Request {
        let Modal::NewTask(form) = &mut self.modal else {
            return Request::None;
        };

        if form.stage == NewTaskStage::Pick {
            match key.code {
                KeyCode::Esc => self.modal = Modal::None,
                KeyCode::Enter => self.choose_new_task_workflow(),
                // The same idiom the input forms use for a select value:
                // ←/→ and space change it, j/k and ↑/↓ move like everywhere
                // else in the interface.
                KeyCode::Left | KeyCode::Up | KeyCode::Char('k') => form.cycle_choice(-1),
                KeyCode::Right
                | KeyCode::Down
                | KeyCode::Char('j')
                | KeyCode::Char(' ')
                | KeyCode::Tab => form.cycle_choice(1),
                _ => {}
            }
            return Request::None;
        }

        if form.stage == NewTaskStage::Definition {
            match key.code {
                // Back to the list rather than out of the modal: the path line
                // is a detour from it, not a modal of its own.
                KeyCode::Esc => self.back_to_new_task_list(),
                // Loading and validating reads a file and nothing else, so it
                // stays on this side of the `Request` seam.
                KeyCode::Enter => self.load_new_task_definition(),
                KeyCode::Backspace => {
                    form.path.pop();
                }
                KeyCode::Char(character) => form.path.push(character),
                _ => {}
            }
            return Request::None;
        }

        let rows = form.parameters.len();
        match key.code {
            KeyCode::Esc => {
                self.modal = Modal::None;
                Request::None
            }
            KeyCode::Enter => Request::StartNewTask,
            KeyCode::Tab | KeyCode::Down if rows > 0 => {
                form.row = (form.row + 1) % rows;
                Request::None
            }
            KeyCode::BackTab | KeyCode::Up if rows > 0 => {
                form.row = (form.row + rows - 1) % rows;
                Request::None
            }
            KeyCode::Left => {
                cycle_input(form, -1);
                Request::None
            }
            KeyCode::Right => {
                cycle_input(form, 1);
                Request::None
            }
            KeyCode::Backspace => {
                if form.choices(form.row).is_none() {
                    if let Some(value) = form.values.get_mut(form.row) {
                        value.pop();
                    }
                }
                Request::None
            }
            KeyCode::Char(character) => {
                if form.choices(form.row).is_some() {
                    if character == ' ' {
                        cycle_input(form, 1);
                    }
                } else if let Some(value) = form.values.get_mut(form.row) {
                    value.push(character);
                }
                Request::None
            }
            _ => Request::None,
        }
    }

    // -- operations ---------------------------------------------------------

    /// Opens the form for the question the selected execution is waiting on.
    pub fn open_input_form(&mut self) {
        match self.pending.as_ref() {
            Some(pending) if pending.continuation == InputContinuation::ResumeAgent => {
                self.modal = Modal::Input(InputForm::new(pending))
            }
            Some(_) => self.notice = Some(Notice::error(
                "This workflow decision is selected with t; i only replies to an agent/harness.",
            )),
            None => {
                self.notice = Some(Notice::error(
                    "The selected execution is not waiting for human input.",
                ))
            }
        }
    }

    /// `i` replies only to the harness that raised the current question.
    pub fn open_input_or_recovery_comment(&mut self) {
        if let Some(pending) = self.pending.as_ref() {
            if pending.continuation == InputContinuation::ResumeAgent {
                self.open_input_form();
            } else {
                self.notice = Some(Notice::error(
                    "This workflow decision is selected with t; i only replies to an agent/harness.",
                ));
            }
            return;
        }
        self.notice = Some(Notice::error(
            "The selected step is not awaiting a reply from an agent/harness.",
        ));
    }

    /// Opens the manual transition menu for the current state.
    pub fn open_transition_menu(&mut self) {
        if self.execution.is_none() {
            self.notice = Some(Notice::error("No execution is selected."));
            return;
        }
        if self.definition.is_none() {
            self.notice = Some(Notice::error(self.definition_error.clone().unwrap_or_else(
                || "No workflow definition for this execution.".to_string(),
            )));
            return;
        }

        let (outputs, output_descriptions): (Vec<String>, Vec<Option<String>>) = self
            .current_state()
            .map(permitted_outputs)
            .unwrap_or_default()
            .into_iter()
            .map(|output| (output.label, output.description))
            .unzip();
        let states = self.override_targets();
        let override_allowed = self.override_allowed();

        self.modal = Modal::Transition(TransitionMenu {
            mode: if outputs.is_empty() {
                TransitionMode::Override
            } else {
                TransitionMode::Outputs
            },
            outputs,
            output_descriptions,
            states,
            selected: 0,
            override_allowed,
        });
    }

    /// Validates the open form and, if it passes, delivers it — `POST
    /// /executions/:id/input` (U7: used to dispatch `Engine::deliver_human_input`
    /// in the background via [`App::spawn_background`]; the daemon now runs
    /// the actual pass in its own background task, so this call is a short,
    /// directly-awaited `202`, per the plan's Approach for this unit).
    ///
    /// A failed validation is reported in the form and nothing is sent: a
    /// rejected answer must leave the execution exactly as paused as it was.
    /// A validated delivery's outcome, once accepted, no longer reopens this
    /// form; it surfaces later as a notice from the live stream (R6).
    pub async fn submit_input(&mut self) {
        let Modal::Input(form) = &mut self.modal else {
            return;
        };

        let delivery = match form.validate() {
            Ok(delivery) => delivery,
            Err(message) => {
                form.error = Some(message.clone());
                self.notice = Some(Notice::error(message));
                return;
            }
        };

        let Some(execution_id) = self.selected().map(|row| row.id.clone()) else {
            return;
        };

        self.modal = Modal::None;
        self.deliver(&execution_id, delivery).await;
    }

    /// Carries out the transition the operator picked.
    pub async fn apply_transition(&mut self) {
        let (target, forced) = match &self.modal {
            Modal::Transition(menu) => match menu.current() {
                Some(target) => (target.clone(), false),
                None => return,
            },
            Modal::Confirm(confirm) => (confirm.target.clone(), true),
            _ => return,
        };

        if forced {
            self.force_transition(&target).await;
            return;
        }

        self.manual_transition(&target).await;
    }

    /// The shared tail of [`App::submit_input`]/[`App::deliver_label`]:
    /// `client.deliver_input`, then a notice — success or failure — in the
    /// same wording an operator already reads for every other mutating call
    /// here.
    async fn deliver(&mut self, execution_id: &str, delivery: HumanInputDelivery) {
        match self.client.deliver_input(execution_id, delivery).await {
            Ok(_) => {
                self.notice = Some(Notice::info(format!(
                    "Delivered input to execution {execution_id}; running in the background."
                )));
            }
            Err(error) => {
                self.notice = Some(Notice::error(client_error_message(execution_id, error)));
            }
        }
    }

    /// Takes a declared transition by hand — `POST /executions/:id/transition`
    /// with `label` (U7: used to write the transition's events directly to
    /// the store here, guarded only by a client-side `Engine::is_active`
    /// check; the server now does the identical legality/activity check
    /// authoritatively, U3, and this just displays whatever it answers).
    async fn manual_transition(&mut self, label: &str) {
        let Some(execution_id) = self.selected().map(|row| row.id.clone()) else {
            return;
        };
        let request = TransitionRequest {
            label: Some(label.to_string()),
            target: None,
            force: false,
        };
        self.send_transition(&execution_id, request, false).await;
    }

    /// Forces a transition to an arbitrary state (D30 override) — `POST
    /// /executions/:id/transition` with `force: true`.
    ///
    /// The `allow_manual_override` check stays here too, even though the
    /// server repeats it authoritatively (`400` if bypassed): this is the
    /// menu's own construction-time guard ([`App::open_transition_menu`]
    /// never offers override mode otherwise), kept as a fast, round-trip-free
    /// refusal for whatever narrow path might still reach this with it
    /// disallowed.
    async fn force_transition(&mut self, target: &str) {
        let Some(execution_id) = self.selected().map(|row| row.id.clone()) else {
            return;
        };
        let request = TransitionRequest {
            label: None,
            target: Some(target.to_string()),
            force: true,
        };
        self.send_transition(&execution_id, request, true).await;
    }

    /// The shared tail of [`App::manual_transition`]/[`App::force_transition`]:
    /// `client.transition`, then the same success/error wording the old
    /// client-side `write_transition` produced — including, for
    /// [`ClientError::Conflict`], the exact denial text the old client-side
    /// `Engine::is_active` guard used to produce, since the server now makes
    /// the identical check (U3) and this is the one call site that phrases
    /// it for an operator.
    async fn send_transition(
        &mut self,
        execution_id: &str,
        request: TransitionRequest,
        forced: bool,
    ) {
        let from = self
            .execution
            .as_ref()
            .and_then(|execution| execution.current_state.clone())
            .unwrap_or_else(|| "-".to_string());
        let label = request.label.clone();

        match self.client.transition(execution_id, request).await {
            Ok(summary) => {
                let target = summary.current_state.clone().unwrap_or_default();
                self.modal = Modal::None;
                self.notice = Some(Notice::info(if forced {
                    format!("Forced transition {from} → {target}.")
                } else {
                    format!(
                        "Manual transition {from} → {target} ('{}') applied.",
                        label.unwrap_or_default()
                    )
                }));
                self.refresh().await;
            }
            Err(ClientError::Conflict) => {
                self.notice = Some(Notice::error(
                    "This execution is being driven in the background right now; \
                     wait for it to settle before transitioning it by hand.",
                ));
                self.modal = Modal::None;
            }
            Err(error) => {
                self.notice = Some(Notice::error(client_error_message(execution_id, error)));
            }
        }
    }

    // -- the repository registry --------------------------------------------

    /// The registry of the catalogue this interface was given.
    fn registry(&self) -> RepositoryRegistry {
        RepositoryRegistry::new(self.catalog.root())
    }

    /// The registered ids, or an empty list when the registry cannot be read.
    pub fn repository_ids(&self) -> Vec<String> {
        self.registry()
            .list()
            .map(|listing| listing.entries.into_iter().map(|entry| entry.id).collect())
            .unwrap_or_default()
    }

    /// Opens the listing of registered repositories.
    ///
    /// An entry that could not be read is named in a notice rather than
    /// dropped. Showing the remaining repositories and saying nothing about
    /// the missing one is the failure `repo list` is built to avoid, and the
    /// interface has no more right to it.
    pub fn open_repository_list(&mut self) {
        match self.registry().list() {
            Ok(listing) => {
                let selected = match &self.modal {
                    // Coming back from a form or a confirmation keeps the
                    // operator where they were, unless the list shrank.
                    Modal::Repositories(list) => {
                        list.selected.min(listing.entries.len().saturating_sub(1))
                    }
                    _ => 0,
                };
                let unreadable: Vec<String> = listing
                    .failures
                    .iter()
                    .map(|failure| failure.path.display().to_string())
                    .collect();
                self.modal = Modal::Repositories(RepositoryList {
                    entries: listing.entries,
                    selected,
                });
                if !unreadable.is_empty() {
                    self.notice = Some(Notice::error(format!(
                        "{} registry entr(y/ies) could not be read: {}",
                        unreadable.len(),
                        unreadable.join(", ")
                    )));
                }
            }
            Err(error) => {
                self.notice = Some(Notice::error(format!(
                    "Could not read the repository registry: {error}"
                )))
            }
        }
    }

    /// Opens the form that registers a repository.
    pub fn open_repository_form(&mut self) {
        self.modal = Modal::RepositoryForm(RepositoryForm::new());
    }

    /// Validates the open repository form and, if it passes, stores the entry.
    ///
    /// A rejection — a missing field, an unusable id, a refused remote, an id
    /// that is already taken — is shown in the form and the form stays open,
    /// exactly as the human-input form behaves. Nothing reaches the registry
    /// that `validate()` has not already accepted.
    pub fn submit_repository(&mut self) {
        let Modal::RepositoryForm(form) = &self.modal else {
            return;
        };

        let entry = match form.entry() {
            Ok(entry) => entry,
            Err(message) => {
                if let Modal::RepositoryForm(form) = &mut self.modal {
                    form.error = Some(message.clone());
                }
                self.notice = Some(Notice::error(message));
                return;
            }
        };

        let id = entry.id.clone();
        match self.registry().add(entry) {
            Ok(()) => {
                self.notice = Some(Notice::info(format!("Registered repository '{id}'.")));
                self.modal = Modal::None;
                self.open_repository_list();
            }
            Err(error) => {
                let message = format!("Could not register the repository: {error}");
                if let Modal::RepositoryForm(form) = &mut self.modal {
                    form.error = Some(message.clone());
                }
                self.notice = Some(Notice::error(message));
            }
        }
    }

    /// Removes the confirmed repository and returns to the listing.
    pub fn remove_repository(&mut self) {
        let Modal::RepositoryRemove(confirm) = &self.modal else {
            return;
        };
        let id = confirm.id.clone();

        match self.registry().remove(&id) {
            Ok(entry) => {
                self.notice = Some(Notice::info(format!(
                    "Removed repository '{}' ({}).",
                    entry.id, entry.remote
                )))
            }
            Err(error) => {
                self.notice = Some(Notice::error(format!(
                    "Could not remove the repository: {error}"
                )))
            }
        }
        self.modal = Modal::None;
        self.open_repository_list();
    }

    // -- starting a new task ------------------------------------------------

    /// Loads the definition the new-task form names and validates it in full.
    ///
    /// Every violation is kept, not the first one: the operator should see the
    /// whole verdict on one screen, the way `narsor validate` prints it.
    pub fn load_new_task_definition(&mut self) {
        let repositories = self.repository_ids();
        let reference = match &self.modal {
            Modal::NewTask(form) => form.path.trim().to_string(),
            _ => return,
        };

        if reference.is_empty() {
            if let Modal::NewTask(form) = &mut self.modal {
                form.error = Some("Type the path of a workflow definition.".to_string());
            }
            return;
        }

        // A path typed here is resolved by the same resolver the list is built
        // from, so a *name* typed by hand behaves exactly as picking it does.
        let resolved = match self.resolver.resolve(&reference) {
            Ok(resolved) => resolved,
            Err(error) => {
                if let Modal::NewTask(form) = &mut self.modal {
                    form.error = Some(error.to_string());
                }
                return;
            }
        };

        self.adopt_new_task_workflow(reference, resolved, &repositories);
    }

    /// Opens the new-task modal on the list of everything that can be run.
    ///
    /// The list is the resolver's own — the same enumeration
    /// `narsor workflow list` prints, overrides marked — plus the row
    /// that switches to a typed path, because a definition being written is in
    /// no search location yet.
    pub fn open_new_task(&mut self) {
        let mut choices: Vec<NewTaskChoice> = self
            .resolver
            .list()
            .into_iter()
            .map(NewTaskChoice::Workflow)
            .collect();
        choices.push(NewTaskChoice::Path);
        self.modal = Modal::NewTask(Box::new(NewTaskForm::picking(choices)));
    }

    /// Returns from the typed path to the pick list.
    pub fn back_to_new_task_list(&mut self) {
        let Modal::NewTask(form) = &mut self.modal else {
            return;
        };
        if form.choices.is_empty() {
            self.modal = Modal::None;
            return;
        }
        form.stage = NewTaskStage::Pick;
        form.violations.clear();
        form.error = None;
    }

    /// Acts on the highlighted row of the pick list.
    ///
    /// A workflow is resolved by name — so the built-in, the user copy or the
    /// root copy that would actually run is the one loaded — and
    /// "Enter a path…" switches to the text line.
    pub fn choose_new_task_workflow(&mut self) {
        let repositories = self.repository_ids();
        let choice = match &self.modal {
            Modal::NewTask(form) => form.selected_choice().cloned(),
            _ => return,
        };

        let Some(choice) = choice else {
            if let Modal::NewTask(form) = &mut self.modal {
                form.error = Some(
                    "No workflow is reachable — press Esc and point at a definition by path."
                        .to_string(),
                );
            }
            return;
        };

        let Some(name) = choice.name().map(ToString::to_string) else {
            if let Modal::NewTask(form) = &mut self.modal {
                form.stage = NewTaskStage::Definition;
                form.violations.clear();
                form.error = None;
            }
            return;
        };

        let resolved = match self.resolver.resolve_name(&name) {
            Ok(resolved) => resolved,
            Err(error) => {
                if let Modal::NewTask(form) = &mut self.modal {
                    form.error = Some(error.to_string());
                }
                return;
            }
        };

        self.adopt_new_task_workflow(name, resolved, &repositories);
    }

    /// Validates a resolved workflow in full and builds its input form.
    ///
    /// Every violation is kept, not the first one: the operator should see the
    /// whole verdict on one screen, the way `narsor validate` prints it.
    ///
    /// `reference` is the exact string that resolved to `resolved` — a
    /// workflow name or a typed path — remembered on the form
    /// ([`NewTaskForm::reference`]) so [`App::start_new_task`] can send the
    /// identical reference to `client.start_task` (U7).
    fn adopt_new_task_workflow(
        &mut self,
        reference: String,
        resolved: ResolvedWorkflow,
        repositories: &[String],
    ) {
        let Modal::NewTask(form) = &mut self.modal else {
            return;
        };

        form.violations.clear();
        form.error = None;
        form.definition = None;
        form.resolved = None;
        form.reference = None;

        match resolved.validate() {
            Ok(violations) if !violations.is_empty() => {
                form.violations = violations.iter().map(ToString::to_string).collect();
                form.error = Some("The definition is invalid — nothing was created.".to_string());
                return;
            }
            Ok(_) => {}
            Err(error) => {
                form.error = Some(format!("Could not load the definition: {error}"));
                return;
            }
        }

        let definition = match resolved.load() {
            Ok(definition) => definition,
            Err(error) => {
                form.error = Some(format!("Could not load the definition: {error}"));
                return;
            }
        };

        form.adopt(reference, definition, resolved, repositories);
    }

    /// Creates the execution the form describes and starts it — `POST /tasks`
    /// (U7: used to call [`crate::tasks::start_task`] directly against
    /// `self.engine`, provisioning the workspace and everything else
    /// in-process; the daemon does all of that now, on the other end of this
    /// one request, exactly as `narsor run` already worked through U6).
    ///
    /// `form.reference` — the exact workflow name or typed path this form
    /// resolved locally, to validate it and build the inputs form — is sent
    /// verbatim, so the daemon repeats the identical resolution
    /// (`WorkflowResolver::resolve`) this form already ran.
    pub async fn start_new_task(&mut self) {
        let Modal::NewTask(form) = &self.modal else {
            return;
        };
        let Some(reference) = form.reference.clone() else {
            return;
        };

        let inputs = match form.inputs() {
            Ok(inputs) => inputs,
            Err(message) => {
                if let Modal::NewTask(form) = &mut self.modal {
                    form.error = Some(message.clone());
                }
                self.notice = Some(Notice::error(message));
                return;
            }
        };

        match self.client.start_task(&reference, inputs).await {
            Ok(started) => {
                self.modal = Modal::None;
                self.notice = Some(Notice::info(format!(
                    "Started execution {}; running in the background.",
                    started.execution_id
                )));
                self.refresh().await;
                self.select_execution(&started.execution_id).await;
            }
            Err(error) => {
                let message = client_error_message(&reference, error);
                if let Modal::NewTask(form) = &mut self.modal {
                    form.error = Some(message.clone());
                }
                self.notice = Some(Notice::error(message));
            }
        }
    }

    /// Loads the default-harness snapshot for an open picker.
    pub async fn load_harness_selection(&mut self) {
        match self.client.harness_selection().await {
            Ok(snapshot) => self.accept_harness_snapshot(snapshot),
            Err(error) => self.harness_selection_error(error),
        }
    }

    /// Persists the selected harness and displays only the daemon's returned
    /// snapshot — never the requested value optimistically.
    pub async fn set_harness_selection(&mut self, harness: String) {
        match self.client.set_harness_selection(&harness).await {
            Ok(snapshot) => self.accept_harness_snapshot(snapshot),
            Err(error) => self.harness_selection_error(error),
        }
    }

    /// Removes the persistent override and displays the returned fallback.
    pub async fn reset_harness_selection(&mut self) {
        match self.client.reset_harness_selection().await {
            Ok(snapshot) => self.accept_harness_snapshot(snapshot),
            Err(error) => self.harness_selection_error(error),
        }
    }

    fn accept_harness_snapshot(&mut self, snapshot: HarnessSelectionSnapshot) {
        if let Modal::HarnessSelection(picker) = &mut self.modal {
            picker.accept_snapshot(snapshot);
        }
    }

    fn harness_selection_error(&mut self, error: ClientError) {
        let message = format!("Could not update the default harness: {error}");
        if let Modal::HarnessSelection(picker) = &mut self.modal {
            picker.error = Some(message.clone());
        }
        self.notice = Some(Notice::error(message));
    }

    /// Puts the cursor on one execution, if the list holds it.
    pub async fn select_execution(&mut self, execution_id: &str) {
        if let Some(index) = self
            .executions
            .iter()
            .position(|row| row.id == execution_id)
        {
            self.selected = index;
            self.focus = Panel::Executions;
            self.reload_selection().await;
        }
    }

    /// Runs or resumes the selected execution — `POST
    /// /executions/:id/resume` (U7: used to dispatch `Engine::run`/`resume`
    /// in the background via [`App::spawn_background`], picking one of the
    /// two based on the row's own status; the server's `Engine::prepare_resume`
    /// already accepts a `Running` execution as a no-op passthrough exactly
    /// as `Engine::run` did, so `resume` alone now covers both cases the old
    /// `Running`/`Paused` match used to split between).
    pub async fn run_selected(&mut self) {
        let Some(row) = self.selected().cloned() else {
            self.notice = Some(Notice::error("No execution is selected."));
            return;
        };

        if self.pending.is_some() {
            self.notice = Some(Notice::error(
                "The execution is waiting for an answer: use i, not s.",
            ));
            return;
        }

        match row.status {
            ExecutionStatus::Running | ExecutionStatus::Paused => {
                if let Err(error) = self.client.resume(&row.id).await {
                    self.notice = Some(Notice::error(client_error_message(&row.id, error)));
                    return;
                }
            }
            ExecutionStatus::Failed | ExecutionStatus::Unknown => {
                self.recover_selected(None).await;
                return;
            }
            other => {
                self.notice = Some(Notice::error(format!(
                    "An execution that is {} cannot be run.",
                    status_name(&other)
                )));
                return;
            }
        }

        self.notice = Some(Notice::info(format!(
            "Execution {} is running in the background.",
            row.id
        )));
    }

    /// Starts a fresh, explicit recovery attempt from a failed or interrupted
    /// state. A supplied note becomes part of the new agent prompt.
    pub async fn recover_selected(&mut self, comment: Option<String>) {
        let Some(row) = self.selected().cloned() else {
            self.notice = Some(Notice::error("No execution is selected."));
            return;
        };
        self.modal = Modal::None;
        match self.client.recover(&row.id, comment).await {
            Ok(_) => {
                self.notice = Some(Notice::info(format!(
                    "Execution {} is recovering in the background.",
                    row.id
                )))
            }
            Err(error) => {
                self.notice = Some(Notice::error(client_error_message(&row.id, error)));
            }
        }
    }

    /// Persists an instruction for the next agent attempt without implicitly
    /// restarting a terminal execution.
    pub async fn save_operator_note(&mut self, note: String) {
        let Some(row) = self.selected().cloned() else {
            return;
        };
        self.modal = Modal::None;
        match self.client.operator_note(&row.id, note).await {
            Ok(_) => {
                self.notice = Some(Notice::info(format!(
                    "Saved an operator instruction for execution {}.",
                    row.id
                )))
            }
            Err(error) => self.notice = Some(Notice::error(client_error_message(&row.id, error))),
        }
    }

    pub async fn interrupt_selected(&mut self) {
        let Some(row) = self.selected().cloned() else {
            return;
        };
        self.modal = Modal::None;
        match self.client.interrupt(&row.id).await {
            Ok(_) => {
                self.notice = Some(Notice::info(format!(
                    "Interrupted execution {}. You can now add an instruction or choose a step.",
                    row.id
                )))
            }
            Err(error) => self.notice = Some(Notice::error(client_error_message(&row.id, error))),
        }
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Turns a [`ClientError`] into the notice text an operator reads — the
/// TUI's counterpart of `src/cli/commands.rs`'s `client_error`, which does
/// the same mapping for the CLI. `execution_id` (or, for `start_new_task`,
/// the workflow reference) is folded into the message so a notice never
/// reads as a bare, unattributed "not found"/"conflict".
fn client_error_message(execution_id: &str, error: ClientError) -> String {
    match error {
        ClientError::Conflict => format!(
            "Execution '{execution_id}' is already being driven by another process right now \
             (another operation, or a second TUI/CLI) — try again once it settles."
        ),
        ClientError::NotFound => format!("Could not load execution '{execution_id}': not found"),
        ClientError::Unauthorized => {
            "The narsor daemon rejected the request: bad token".to_string()
        }
        ClientError::Unavailable(detail) => {
            format!("Could not reach the narsor daemon: {detail}")
        }
        ClientError::Request { message, .. } => message,
        ClientError::Server(detail) => {
            format!("The narsor daemon reported an error: {detail}")
        }
    }
}

/// A minimal stand-in [`WorkflowExecution`] carrying only what
/// `tasks::resolve_definition` actually reads (`metadata`, for
/// `metadata.cli.*`, plus `id`/`workflow_id` for its error messages) —
/// needed because [`App`] now holds an [`ExecutionSummary`], not the whole
/// record `resolve_definition`'s signature (shared with the CLI and the
/// daemon's own routes, both of which do have the whole thing) still takes.
/// Every other field is a harmless placeholder: nothing else in
/// `resolve_definition`'s call graph reads them.
fn execution_stub(id: &str, workflow_id: &str, metadata: Value) -> WorkflowExecution {
    WorkflowExecution {
        id: id.to_string(),
        workflow_id: workflow_id.to_string(),
        workflow_version: 0,
        status: ExecutionStatus::Unknown,
        current_state: None,
        started_at: Utc::now(),
        completed_at: None,
        inputs: Value::Null,
        context: Context::default(),
        workspace: Workspace::local_folder(String::new()),
        current_action_execution: None,
        metadata,
    }
}

/// Steps a cursor within `length`, or `None` when it cannot move.
fn step(current: usize, delta: isize, length: usize) -> Option<usize> {
    if length == 0 {
        return None;
    }
    let next = current as isize + delta;
    if next < 0 || next as usize >= length {
        return None;
    }
    Some(next as usize)
}

/// Steps the focused row's choice: a `select` field's option, or the label.
fn cycle_choice(form: &mut InputForm, delta: isize) {
    match form.current_row() {
        Some(FormRow::Field(index)) => {
            let field = &form.fields[index];
            if field.field_type != HumanInputType::Select || field.options.is_empty() {
                return;
            }
            let next = match field
                .options
                .iter()
                .position(|option| option == &form.values[index])
            {
                Some(current) => wrap(current, delta, field.options.len()),
                None if delta >= 0 => 0,
                None => field.options.len() - 1,
            };
            form.values[index] = field.options[next].clone();
        }
        Some(FormRow::Label) => {
            if form.label_choices.is_empty() {
                return;
            }
            form.label_index = wrap(form.label_index, delta, form.label_choices.len());
        }
        None => {}
    }
}

/// Steps the focused input's choice, when it has one.
fn cycle_input(form: &mut NewTaskForm, delta: isize) {
    let Some(options) = form.choices(form.row) else {
        return;
    };
    if options.is_empty() {
        return;
    }
    let current = options
        .iter()
        .position(|option| Some(option) == form.values.get(form.row))
        .unwrap_or(0);
    let next = wrap(current, delta, options.len());
    let value = options[next].clone();
    if let Some(slot) = form.values.get_mut(form.row) {
        *slot = value;
    }
}

/// Steps an index within `length`, wrapping around.
fn wrap(current: usize, delta: isize, length: usize) -> usize {
    let length = length as isize;
    (((current as isize + delta) % length + length) % length) as usize
}

/// Keeps a list row in step with a live event, so the list does not go stale
/// between refreshes.
fn apply_to_row(row: &mut ExecutionRow, data: &EventData) {
    match data {
        EventData::StateEntered { state_id } => {
            row.current_state = Some(state_id.clone());
            row.status = ExecutionStatus::Running;
        }
        EventData::WorkflowResumed => row.status = ExecutionStatus::Running,
        EventData::WorkflowPaused { .. } => row.status = ExecutionStatus::Paused,
        EventData::HumanInputRequested { .. } => row.status = ExecutionStatus::InputNeeded,
        EventData::WorkflowCompleted { .. } => row.status = ExecutionStatus::Completed,
        EventData::WorkflowFailed { .. } => row.status = ExecutionStatus::Failed,
        EventData::WorkflowCancelled { .. } => row.status = ExecutionStatus::Cancelled,
        _ => {}
    }
}

/// The bottom-bar notice a background run's outcome produces, if any (R4, R6).
///
/// Only a terminal or error event names an outcome worth interrupting the
/// operator for; reaching a human checkpoint (`WorkflowPaused`,
/// `HumanInputRequested`) is an ordinary part of a run, not one, so it
/// produces no notice here.
fn notice_for(event: &Event) -> Option<Notice> {
    let execution_id = &event.workflow_execution_id;
    match &event.data {
        EventData::WorkflowCompleted { .. } => {
            Some(Notice::info(format!("Execution {execution_id} completed.")))
        }
        EventData::WorkflowFailed { error } => Some(Notice::error(format!(
            "Execution {execution_id} failed: {error}"
        ))),
        EventData::WorkflowCancelled { reason } => Some(Notice::error(format!(
            "Execution {execution_id} was cancelled: {reason}"
        ))),
        EventData::Error { message, .. } => Some(Notice::error(format!(
            "Execution {execution_id}: {message}"
        ))),
        _ => None,
    }
}

/// The schema spelling of a status — the same words the CLI prints.
pub fn status_name(status: &ExecutionStatus) -> &'static str {
    match status {
        ExecutionStatus::Running => "running",
        ExecutionStatus::Paused => "paused",
        ExecutionStatus::InputNeeded => "input_needed",
        ExecutionStatus::WaitingForEvent => "waiting_for_event",
        ExecutionStatus::Completed => "completed",
        ExecutionStatus::Failed => "failed",
        ExecutionStatus::Cancelled => "cancelled",
        ExecutionStatus::Unknown => "unknown",
    }
}

/// A one-line description of an event, for the stream panel.
pub fn event_line(event: &Event) -> String {
    let detail = match &event.data {
        EventData::WorkflowStarted { workflow_id, .. } => {
            format!("workflow_started  {workflow_id}")
        }
        EventData::WorkflowPaused { reason } => format!("workflow_paused  {reason}"),
        EventData::WorkflowResumed => "workflow_resumed".to_string(),
        EventData::WorkflowCompleted { .. } => "workflow_completed".to_string(),
        EventData::WorkflowFailed { error } => format!("workflow_failed  {error}"),
        EventData::WorkflowCancelled { reason } => format!("workflow_cancelled  {reason}"),
        EventData::StateEntered { state_id } => format!("state_entered  {state_id}"),
        EventData::StateExited {
            state_id,
            output_label,
        } => format!("state_exited  {state_id} → {output_label}"),
        EventData::ActionStarted {
            state_id,
            action_type,
            ..
        } => format!("action_started  {state_id} ({action_type})"),
        EventData::ActionCompleted { output_label, .. } => {
            format!("action_completed  {output_label}")
        }
        EventData::ActionFailed { error, .. } => format!("action_failed  {error}"),
        EventData::ActionAttemptFailed {
            error, retry_in_ms, ..
        } => {
            format!("action_attempt_failed  {error} (retrying in {retry_in_ms}ms)")
        }
        EventData::ActionRetrying { retry_count, .. } => {
            format!("action_retrying  attempt {retry_count}")
        }
        EventData::DecisionRequested { provider } => format!("decision_requested  {provider}"),
        EventData::DecisionMade {
            provider,
            output_label,
            ..
        } => format!("decision_made  {provider} → {output_label}"),
        EventData::HumanInputRequested { instruction, .. } => {
            format!("human_input_requested  {instruction}")
        }
        EventData::HumanInputReceived { .. } => "human_input_received".to_string(),
        EventData::HumanTransitionSelected {
            from_state,
            to_state,
        } => format!("human_transition_selected  {from_state} → {to_state}"),
        EventData::TransitionTaken {
            from_state,
            to_state,
            output_label,
        } => format!("transition_taken  {from_state} → {to_state} ({output_label})"),
        EventData::ArtifactCreated { artifact_id, .. } => {
            format!("artifact_created  {artifact_id}")
        }
        EventData::ExternalEventReceived { event_type, .. } => {
            format!("external_event_received  {event_type}")
        }
        EventData::Error { message, .. } => format!("error  {message}"),
        EventData::AgentSessionStarted {
            harness,
            action_execution_id,
            resumed_session,
            ..
        } => match resumed_session {
            Some(session) => format!(
                "agent_session_started  {harness} (attempt {}, resuming {})",
                short_id(action_execution_id.as_deref()),
                short_id(Some(session))
            ),
            None => format!(
                "agent_session_started  {harness} (attempt {})",
                short_id(action_execution_id.as_deref())
            ),
        },
        EventData::AgentSessionCompleted {
            action_execution_id,
            harness_session_id,
            ..
        } => format!(
            "agent_session_completed  attempt {} (session {})",
            short_id(action_execution_id.as_deref()),
            short_id(harness_session_id.as_deref())
        ),
        EventData::AgentSessionFailed {
            error,
            action_execution_id,
            ..
        } => format!(
            "agent_session_failed  {error} (attempt {})",
            short_id(action_execution_id.as_deref())
        ),
        EventData::AgentMessage {
            role,
            content,
            action_execution_id,
            ..
        } => format!(
            "agent_message  ({role}) attempt {}: {}",
            short_id(action_execution_id.as_deref()),
            first_line(content)
        ),
    };

    format!(
        "{:>4}  {}  {}",
        event.sequence,
        event.timestamp.format("%H:%M:%S"),
        detail
    )
}

/// The first line of a multi-line string, trimmed and elided, so a long
/// agent message cannot break the one-line-per-event stream panel.
fn first_line(text: &str) -> String {
    const LIMIT: usize = 80;
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() <= LIMIT {
        return line.to_string();
    }
    let cut: String = line.chars().take(LIMIT).collect();
    format!("{cut}…")
}

/// A short fragment of an id, for a one-line summary where the full UUID
/// would swamp everything else on the line. The full value is always in the
/// detail window ([`event_detail_lines`]).
fn short_id(id: Option<&str>) -> String {
    match id {
        Some(id) => id.chars().take(8).collect(),
        None => "unknown".to_string(),
    }
}

/// The message shown in the bottom bar for `c` on an event with no
/// conversation link (AE3).
///
/// The phrase "does not belong to an agent conversation" matches the reason
/// [`crate::engine::timeline`] records for the same situation, so an operator
/// sees the same wording once U7 wires the task view up to it.
const NO_CONVERSATION_LINK_MESSAGE: &str = "This event does not belong to an agent conversation.";

/// The attempt an event is linked to, if it carries one at all (R5).
///
/// This only reads the field the event itself carries. It deliberately does
/// not attempt the fuller association [`crate::engine::timeline::build_timeline`]
/// computes — a state/transition event's link to "the last attempt of the
/// current visit", or whether an `action_execution_id` actually belongs to an
/// agent action rather than a script one — because that needs the whole event
/// log, which is timeline/API territory (U4/U7), not this window's job. An
/// event outside that reach simply has no link here yet.
fn conversation_attempt(data: &EventData) -> Option<&str> {
    match data {
        EventData::ActionStarted {
            action_execution_id,
            ..
        }
        | EventData::ActionCompleted {
            action_execution_id,
            ..
        }
        | EventData::ActionFailed {
            action_execution_id,
            ..
        }
        | EventData::ActionAttemptFailed {
            action_execution_id,
            ..
        }
        | EventData::ActionRetrying {
            action_execution_id,
            ..
        } => Some(action_execution_id.as_str()),
        EventData::AgentSessionStarted {
            action_execution_id,
            ..
        }
        | EventData::AgentSessionCompleted {
            action_execution_id,
            ..
        }
        | EventData::AgentSessionFailed {
            action_execution_id,
            ..
        }
        | EventData::AgentMessage {
            action_execution_id,
            ..
        } => action_execution_id.as_deref(),
        _ => None,
    }
}

/// Every field of an event, for the detail window (R1).
///
/// Structured values are shown as indented JSON: the whole event is
/// serialized once and pretty-printed, which covers every current and future
/// [`EventData`] variant without a second exhaustive match to keep in step
/// with [`event_line`] — that one stays hand-written because its whole point
/// is a compiler-enforced one-liner per variant (R2), which a generic dump
/// cannot give.
pub fn event_detail_lines(event: &Event) -> Vec<String> {
    let mut lines = vec![sanitize(&event_line(event)), String::new()];

    if let EventData::AgentMessage { content, .. } = &event.data {
        if content.ends_with(TRUNCATION_MARKER) {
            lines.push(sanitize(
                "This message was truncated by the executor — press c to open the full conversation.",
            ));
            lines.push(String::new());
        }
    }

    let value = serde_json::to_value(event).unwrap_or(Value::Null);
    let pretty = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    lines.extend(pretty.lines().map(sanitize));

    lines
}

/// One line of the task view's axis (R7): the position's label, status and
/// what it started at, plus the markers the status precedence rule leaves
/// (`harness_session_interrupted`, `retry_pending`).
pub fn task_position_line(position: &crate::engine::Position) -> String {
    let status = match position.status {
        crate::engine::PositionStatus::Succeeded => "succeeded",
        crate::engine::PositionStatus::Failed => "failed",
        crate::engine::PositionStatus::Interrupted => "interrupted",
        crate::engine::PositionStatus::InProgress => "in progress",
        crate::engine::PositionStatus::AwaitingDecision => "awaiting decision",
    };

    let mut suffixes = Vec::new();
    if position.harness_session_interrupted {
        suffixes.push("harness session interrupted".to_string());
    }
    if position.retry_pending {
        suffixes.push("retry pending".to_string());
    }
    if let Some(session) = &position.resumed_harness_session {
        suffixes.push(format!("resumes {}", short_id(Some(session))));
    }

    let suffix = if suffixes.is_empty() {
        String::new()
    } else {
        format!("  ({})", suffixes.join(", "))
    };

    match &position.error {
        Some(error) => format!("{}  {status}  {error}{suffix}", position.label),
        None => format!("{}  {status}{suffix}", position.label),
    }
}

/// The task view content pane's logical lines: a header per entry (or
/// event) and, when expanded (or short enough already), its full sanitized
/// body — collapsed otherwise to [`TASK_ENTRY_COLLAPSE_LINES`] with a hint
/// that `Enter`/space expands it (R10).
pub fn task_conversation_lines(
    entries: &[ClassifiedEntry],
    cursor: usize,
    expanded: &HashSet<u64>,
) -> Vec<String> {
    let mut lines = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let marker = if index == cursor { "›" } else { " " };
        lines.push(format!(
            "{marker} [{}] {}  {}  ({:?})",
            entry.entry_number,
            entry.timestamp.format("%H:%M:%S"),
            entry.kind.label(),
            entry.source
        ));

        let body: Vec<String> = sanitize(&entry.text).lines().map(str::to_string).collect();
        let is_expanded = expanded.contains(&entry.entry_number);
        if is_expanded || body.len() <= TASK_ENTRY_COLLAPSE_LINES {
            for line in &body {
                lines.push(format!("    {line}"));
            }
        } else {
            for line in &body[..TASK_ENTRY_COLLAPSE_LINES] {
                lines.push(format!("    {line}"));
            }
            lines.push(format!(
                "    … {} more lines — Enter/space to expand",
                body.len() - TASK_ENTRY_COLLAPSE_LINES
            ));
        }
    }
    lines
}

/// The task view's content pane, for a step with no conversation (R11): its
/// own events, one line each, in the same style as the Events panel.
pub fn task_events_lines(message: &str, events: &[Event], cursor: usize) -> Vec<String> {
    let mut lines = vec![sanitize(message), String::new()];
    for (index, event) in events.iter().enumerate() {
        let marker = if index == cursor { "›" } else { " " };
        lines.push(format!("{marker} {}", event_line(event)));
    }
    lines
}
