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

use std::path::Path;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Map, Value};
use tokio::sync::broadcast::error::RecvError;

use crate::domain::{
    Event, EventData, ExecutionStatus, HumanInputField, HumanInputType, InputParameter, ParamType,
    State, WorkflowDefinition, WorkflowExecution,
};
use crate::engine::{
    permitted_labels, terminal_status, Engine, EngineError, HumanInputDelivery, PendingHumanInput,
};
use futures::FutureExt;
use crate::registry::{RepositoryEntry, RepositoryRegistry};
use crate::tasks::{self, DefinitionSource};
use crate::persistence::{CatalogPaths, WorkPaths};
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Nothing further.
    None,
    /// Re-read the store.
    Refresh,
    /// Validate and deliver the open input form.
    SubmitInput,
    /// Carry out the transition the operator picked.
    ApplyTransition,
    /// Run or resume the selected execution.
    RunSelected,
    /// Create the execution the new-task form describes and run it.
    StartNewTask,
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

impl InputForm {
    /// Builds the form for a pending question.
    pub fn new(pending: &PendingHumanInput) -> Self {
        let fields = pending.fields().to_vec();
        let values = fields
            .iter()
            .map(|field| match field.field_type {
                // A select starts on its first option, so the common case is
                // one keystroke: Enter.
                HumanInputType::Select => field.options.first().cloned().unwrap_or_default(),
                HumanInputType::Text => String::new(),
            })
            .collect();

        let mut label_choices: Vec<Option<String>> = Vec::new();
        if !pending.outputs.is_empty() {
            if infers_label(&fields) {
                label_choices.push(None);
            }
            label_choices.extend(pending.outputs.iter().cloned().map(Some));
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
    /// Where that definition came from — recorded on the execution, so an
    /// execution started from a built-in can be resumed later.
    pub resolved: Option<ResolvedWorkflow>,
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
        definition: WorkflowDefinition,
        resolved: ResolvedWorkflow,
        repositories: &[String],
    ) {
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
}

/// The confirmation step a forced transition has to pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverrideConfirm {
    /// The state the execution would be moved to.
    pub target: String,
    /// The state it is in now.
    pub from: String,
}

/// The overlay drawn on top of the layout, if any.
#[derive(Debug, Clone)]
pub enum Modal {
    /// No overlay.
    None,
    /// The generated human-input form.
    Input(InputForm),
    /// The manual transition menu.
    Transition(TransitionMenu),
    /// The confirmation of a forced transition.
    Confirm(OverrideConfirm),
    /// The listing of registered repositories.
    Repositories(RepositoryList),
    /// The form that registers a repository.
    RepositoryForm(RepositoryForm),
    /// The confirmation of a repository removal.
    RepositoryRemove(RepositoryRemoveConfirm),
    /// The form that starts a workflow.
    NewTask(Box<NewTaskForm>),
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
// The application
// ---------------------------------------------------------------------------

/// Everything the interface knows.
pub struct App {
    engine: Arc<Engine>,
    resolver: WorkflowResolver,

    executions: Vec<ExecutionRow>,
    selected: usize,

    execution: Option<WorkflowExecution>,
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
    stream: StreamStatus,
    notice: Option<Notice>,
    quit: bool,
    catalog: CatalogPaths,
    work: WorkPaths,
}

impl App {
    /// Builds an interface over an engine. Nothing is read until
    /// [`App::refresh`].
    ///
    /// The catalogue and the work tree are handed in rather than recovered
    /// from the engine's store. They used to be: the store carried the one
    /// root, and this is where the registry and the workflow search path were
    /// rebuilt out of it. That shortcut is what made the three directories one
    /// directory in practice, whatever the configuration said.
    pub fn new(engine: Arc<Engine>, catalog: CatalogPaths, work: WorkPaths) -> Self {
        let resolver = WorkflowResolver::new(catalog.root().to_path_buf());
        Self::with_resolver(engine, resolver, catalog, work)
    }

    /// The same, with the workflow search path given explicitly.
    pub fn with_resolver(
        engine: Arc<Engine>,
        resolver: WorkflowResolver,
        catalog: CatalogPaths,
        work: WorkPaths,
    ) -> Self {
        Self {
            engine,
            resolver,
            catalog,
            work,
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

    /// The highlighted execution, as loaded from the store.
    pub fn execution(&self) -> Option<&WorkflowExecution> {
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
        self.current_state()
            .map(|state| state.allow_manual_override)
            .unwrap_or(false)
    }

    /// The states a forced transition may target: every state but the current
    /// one, and only when the current state allows an override at all.
    pub fn override_targets(&self) -> Vec<String> {
        if !self.override_allowed() {
            return Vec::new();
        }
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
        match &self.modal {
            Modal::Input(_) => vec![
                ("Tab/↑↓", "field"),
                ("←/→", "value"),
                ("letters", "type"),
                ("Enter", "submit"),
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
            Modal::Confirm(_) => vec![("y", "confirm"), ("n/Esc", "cancel")],
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
            Modal::Help => vec![("Esc/?", "close")],
            Modal::None => {
                let mut keys = vec![("Tab", "panel"), ("r", "refresh")];
                match self.focus {
                    Panel::Executions => {
                        keys.push(("j/k", "select"));
                        keys.push(("g/G", "top/bottom"));
                        keys.push(("n", "new task"));
                        keys.push(("R", "repositories"));
                        keys.push(("s", "run/resume"));
                        keys.push(("i", "input"));
                        keys.push(("t", "transition"));
                    }
                    Panel::Details => {
                        keys.push(("j/k", "variables"));
                        keys.push(("i", "input"));
                        keys.push(("t", "transition"));
                    }
                    Panel::Events => {
                        keys.push(("j/k", "scroll"));
                        keys.push(("g/G", "top/bottom"));
                        keys.push(("f", "follow"));
                    }
                }
                keys.push(("?", "help"));
                keys.push(("q", "quit"));
                keys
            }
        }
    }

    // -- loading ------------------------------------------------------------

    /// Re-reads the execution list and the selected execution.
    ///
    /// The selection follows the execution id, not the index, so a new
    /// execution appearing at the top of the list does not move the operator's
    /// cursor onto a different workflow.
    pub fn refresh(&mut self) {
        let previous = self.selected().map(|row| row.id.clone());

        let ids = match self.engine.store().list_execution_ids() {
            Ok(ids) => ids,
            Err(error) => {
                self.notice = Some(Notice::error(format!(
                    "Could not read the execution list: {error}"
                )));
                return;
            }
        };

        let mut rows = Vec::with_capacity(ids.len());
        for id in ids {
            match self.engine.store().load_execution(&id) {
                Ok(execution) => rows.push(ExecutionRow {
                    id: execution.id.clone(),
                    workflow_id: execution.workflow_id.clone(),
                    status: execution.status.clone(),
                    current_state: execution.current_state.clone(),
                }),
                Err(error) => {
                    self.notice = Some(Notice::error(format!(
                        "Could not load execution '{id}': {error}"
                    )));
                }
            }
        }

        self.executions = rows;
        self.selected = previous
            .and_then(|id| self.executions.iter().position(|row| row.id == id))
            .unwrap_or(0);
        if self.selected >= self.executions.len() {
            self.selected = self.executions.len().saturating_sub(1);
        }

        self.reload_selection();
    }

    /// Re-reads everything that depends on which execution is selected.
    pub fn reload_selection(&mut self) {
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
        let store = self.engine.store();

        match store.load_execution(&id) {
            Ok(execution) => self.execution = Some(execution),
            Err(error) => {
                self.notice = Some(Notice::error(format!(
                    "Could not load execution '{id}': {error}"
                )));
                return;
            }
        }

        match store.load_context(&id) {
            Ok(context) => {
                let mut variables: Vec<(String, Value)> = context.variables.into_iter().collect();
                variables.sort_by(|left, right| left.0.cmp(&right.0));
                self.variables = variables;
            }
            Err(error) => {
                self.notice = Some(Notice::error(format!(
                    "Could not read the context: {error}"
                )));
            }
        }

        // An execution with no events is an empty stream, not a failure: it has
        // simply not got anywhere yet.
        match store.event_log(&id).read_all() {
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

        match self.engine.pending_input(&id) {
            Ok(pending) => self.pending = pending,
            Err(error) => {
                self.notice = Some(Notice::error(format!(
                    "Could not read the pending input: {error}"
                )));
            }
        }

        self.load_definition();
    }

    /// Resolves the definition the selected execution runs.
    ///
    /// The same rule the CLI uses, through the same resolver: the recorded
    /// path while that file is there, and otherwise the recorded workflow
    /// name — which is all an execution started from a built-in has.
    fn load_definition(&mut self) {
        let Some(execution) = self.execution.as_ref() else {
            return;
        };

        let resolved = match tasks::resolve_definition(
            self.catalog.root(),
            self.engine.store().paths(),
            execution,
        ) {
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

    /// Folds one result of `broadcast::Receiver::recv` into the state.
    ///
    /// A lag and a closed channel both become a visible indicator: the operator
    /// has to be able to tell "nothing is happening" from "I am no longer being
    /// told what happens".
    pub fn on_stream(&mut self, received: Result<Event, RecvError>) {
        match received {
            Ok(event) => {
                self.stream = StreamStatus::Live;
                self.on_engine_event(event);
            }
            Err(RecvError::Lagged(skipped)) => {
                self.stream = StreamStatus::Lagged { skipped };
                self.notice = Some(Notice::error(format!(
                    "The event stream fell behind: {skipped} events were lost. \
                     Press r to reload the log."
                )));
            }
            Err(RecvError::Closed) => {
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
        match &mut self.modal {
            Modal::None => self.on_key_panels(key),
            Modal::Help => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('?') | KeyCode::Enter) {
                    self.modal = Modal::None;
                }
                Request::None
            }
            Modal::Input(_) => self.on_key_form(key),
            Modal::Transition(_) => self.on_key_transition(key),
            Modal::Confirm(_) => self.on_key_confirm(key),
            Modal::Repositories(_) => self.on_key_repositories(key),
            Modal::RepositoryForm(_) => self.on_key_repository_form(key),
            Modal::RepositoryRemove(_) => self.on_key_repository_remove(key),
            Modal::NewTask(_) => self.on_key_new_task(key),
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
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_cursor(1);
                Request::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_cursor(-1);
                Request::None
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.jump_cursor(false);
                Request::None
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.jump_cursor(true);
                Request::None
            }
            KeyCode::Char('f') if self.focus == Panel::Events => {
                self.follow = !self.follow;
                if self.follow {
                    self.event_cursor = self.events.len().saturating_sub(1);
                }
                Request::None
            }
            KeyCode::Char('i') => {
                self.open_input_form();
                Request::None
            }
            KeyCode::Char('t') => {
                self.open_transition_menu();
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
            KeyCode::Char('s') => Request::RunSelected,
            _ => Request::None,
        }
    }

    /// Moves the focused panel's cursor by `delta`.
    fn move_cursor(&mut self, delta: isize) {
        match self.focus {
            Panel::Executions => {
                let length = self.executions.len();
                if let Some(next) = step(self.selected, delta, length) {
                    self.selected = next;
                    self.reload_selection();
                }
            }
            Panel::Details => {
                if let Some(next) = step(self.variable_cursor, delta, self.variables.len()) {
                    self.variable_cursor = next;
                }
            }
            Panel::Events => {
                if let Some(next) = step(self.event_cursor, delta, self.events.len()) {
                    self.event_cursor = next;
                    // Scrolling away from the newest entry means the operator
                    // is reading, not watching.
                    self.follow = next + 1 == self.events.len();
                }
            }
        }
    }

    /// Jumps the focused panel's cursor to the first or last entry.
    fn jump_cursor(&mut self, to_end: bool) {
        match self.focus {
            Panel::Executions => {
                if self.executions.is_empty() {
                    return;
                }
                self.selected = if to_end { self.executions.len() - 1 } else { 0 };
                self.reload_selection();
            }
            Panel::Details => {
                if self.variables.is_empty() {
                    return;
                }
                self.variable_cursor = if to_end { self.variables.len() - 1 } else { 0 };
            }
            Panel::Events => {
                if self.events.is_empty() {
                    return;
                }
                self.event_cursor = if to_end { self.events.len() - 1 } else { 0 };
                self.follow = to_end;
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
            Some(pending) => self.modal = Modal::Input(InputForm::new(pending)),
            None => {
                self.notice = Some(Notice::error(
                    "The selected execution is not waiting for human input.",
                ))
            }
        }
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

        let outputs = self.permitted_transitions();
        let states = self.override_targets();
        let override_allowed = self.override_allowed();

        if outputs.is_empty() && !override_allowed {
            self.notice = Some(Notice::error(
                "The current state declares no outputs and allows no override.",
            ));
            return;
        }

        self.modal = Modal::Transition(TransitionMenu {
            mode: if outputs.is_empty() {
                TransitionMode::Override
            } else {
                TransitionMode::Outputs
            },
            outputs,
            states,
            selected: 0,
            override_allowed,
        });
    }

    /// Spawns one of the four engine-driving calls in the background (R1,
    /// R2, R3): the caller returns immediately, and the outcome — success or
    /// failure — reaches the interface only through the event stream (R4),
    /// never through this call's own return value, which is why `future`'s
    /// `Result` is otherwise discarded here.
    ///
    /// A panic inside `future` (an executor bug, say) is still caught rather
    /// than left to vanish with it: `tokio::spawn` already stops it from
    /// taking down the whole process, but the discarded `JoinHandle` would
    /// otherwise mean nobody ever learns the pass panicked at all.
    fn spawn_background(
        &self,
        future: impl std::future::Future<Output = Result<WorkflowExecution, EngineError>>
            + Send
            + 'static,
    ) {
        tokio::spawn(async move {
            if let Err(payload) = std::panic::AssertUnwindSafe(future).catch_unwind().await {
                let message = payload
                    .downcast_ref::<&str>()
                    .map(|message| message.to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "non-string panic payload".to_string());
                tracing::error!(
                    %message,
                    "a backgrounded engine call panicked; its execution's outcome is unknown"
                );
            }
        });
    }

    /// Validates the open form and, if it passes, dispatches it to the engine
    /// in the background.
    ///
    /// A failed validation is reported in the form and nothing is sent: a
    /// rejected answer must leave the execution exactly as paused as it was.
    /// A validated delivery's engine-level outcome — success or failure — no
    /// longer reopens this form; it surfaces later as a notice (R6), because
    /// the engine call itself runs in the background and this handler does
    /// not wait for it (R1).
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
        let Some(definition) = self.definition.clone() else {
            self.notice = Some(Notice::error(self.definition_error.clone().unwrap_or_else(
                || "No workflow definition for this execution.".to_string(),
            )));
            return;
        };

        self.modal = Modal::None;
        let engine = Arc::clone(&self.engine);
        self.spawn_background(async move {
            engine
                .deliver_human_input(&definition, &execution_id, &delivery)
                .await
        });
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
            self.force_transition(&target);
            return;
        }

        // Choosing a declared label while the execution waits on a question is
        // an ordinary answer, so the engine runs the workflow on from there.
        if self.pending.is_some() {
            self.deliver_label(&target).await;
        } else {
            self.manual_transition(&target);
        }
    }

    /// Answers a pending question with nothing but a label, in the
    /// background — see [`App::submit_input`] for why this no longer waits
    /// on the engine call either.
    async fn deliver_label(&mut self, label: &str) {
        let Some(execution_id) = self.selected().map(|row| row.id.clone()) else {
            return;
        };
        let Some(definition) = self.definition.clone() else {
            return;
        };

        self.modal = Modal::None;
        let delivery = HumanInputDelivery::label(label.to_string());
        let engine = Arc::clone(&self.engine);
        self.spawn_background(async move {
            engine
                .deliver_human_input(&definition, &execution_id, &delivery)
                .await
        });
    }

    /// Takes a declared transition by hand, without the engine.
    ///
    /// For an execution that is not waiting on a question there is nothing to
    /// deliver an answer to, so the transition is written here and the
    /// execution is parked at the target for the operator to resume.
    fn manual_transition(&mut self, label: &str) {
        let target = self
            .current_state()
            .and_then(|state| state.transitions.get(label).cloned());

        match target {
            Some(target) => self.write_transition(&target, label, false),
            None => {
                self.notice = Some(Notice::error(format!(
                    "The state has no transition for label '{label}'."
                )))
            }
        }
    }

    /// Forces a transition to an arbitrary state (D30 override).
    fn force_transition(&mut self, target: &str) {
        if !self.override_allowed() {
            self.notice = Some(Notice::error(
                "This state does not allow a forced transition (allow_manual_override).",
            ));
            self.modal = Modal::None;
            return;
        }
        self.write_transition(target, OVERRIDE_LABEL, true);
    }

    /// Writes the events and the new state of a transition taken by hand.
    ///
    /// The order mirrors the engine's own (`DOMAIN_MODEL.md` §4.2) so that
    /// recovery reads a hand-made transition the same way it reads a generated
    /// one. `StateEntered` is deliberately *not* written for a non-terminal
    /// target: the engine writes it when the operator resumes, and two of them
    /// would be a lie about how many times the state was entered.
    fn write_transition(&mut self, target: &str, label: &str, forced: bool) {
        let Some(execution) = self.execution.clone() else {
            return;
        };
        // A background pass may be advancing this same execution right now
        // (R7's guard covers `Engine::run`/`resume`/`deliver_human_input`,
        // but a manual/forced transition writes the store directly, so it
        // needs its own check): writing here concurrently with that pass
        // would race its own appends and its own `save_execution`, exactly
        // the corruption the guard exists to prevent one layer up.
        if self.engine.is_active(&execution.id) {
            self.notice = Some(Notice::error(
                "This execution is being driven in the background right now; \
                 wait for it to settle before transitioning it by hand.",
            ));
            self.modal = Modal::None;
            return;
        }
        let Some(definition) = self.definition.clone() else {
            return;
        };
        let Some(target_state) = definition.states.get(target).cloned() else {
            self.notice = Some(Notice::error(format!(
                "The workflow has no state '{target}'."
            )));
            return;
        };

        let mut execution = execution;
        let from = execution
            .current_state
            .clone()
            .unwrap_or_else(|| definition.initial_state.clone());
        let store = self.engine.store();
        let log = store.event_log(&execution.id);

        let writes: Vec<EventData> = vec![
            EventData::DecisionMade {
                provider: if forced {
                    OVERRIDE_PROVIDER
                } else {
                    MANUAL_PROVIDER
                }
                .to_string(),
                output_label: label.to_string(),
                reasoning: Some(
                    if forced {
                        OVERRIDE_REASON
                    } else {
                        MANUAL_REASON
                    }
                    .to_string(),
                ),
            },
            EventData::HumanTransitionSelected {
                from_state: from.clone(),
                to_state: target.to_string(),
            },
            EventData::StateExited {
                state_id: from.clone(),
                output_label: label.to_string(),
            },
            EventData::TransitionTaken {
                from_state: from.clone(),
                to_state: target.to_string(),
                output_label: label.to_string(),
            },
        ];

        for data in writes {
            if let Err(error) = log.append_next(&execution.id, data) {
                self.notice = Some(Notice::error(format!(
                    "Could not append the transition event: {error}"
                )));
                return;
            }
        }

        execution.current_state = Some(target.to_string());
        execution.current_action_execution = None;

        if target_state.terminal {
            let status = terminal_status(&target_state);
            execution.status = status.clone();
            execution.completed_at = Some(chrono::Utc::now());

            let closing = match status {
                ExecutionStatus::Failed => EventData::WorkflowFailed {
                    error: format!("Forced transition into terminal state '{target}'."),
                },
                ExecutionStatus::Cancelled => EventData::WorkflowCancelled {
                    reason: format!("Forced transition into terminal state '{target}'."),
                },
                _ => EventData::WorkflowCompleted {
                    result: Value::Object(Map::new()),
                },
            };
            let entered = EventData::StateEntered {
                state_id: target.to_string(),
            };
            for data in [entered, closing] {
                if let Err(error) = log.append_next(&execution.id, data) {
                    self.notice = Some(Notice::error(format!(
                        "Could not append the closing event: {error}"
                    )));
                    return;
                }
            }
        } else {
            execution.status = ExecutionStatus::Paused;
            if let Err(error) = log.append_next(
                &execution.id,
                EventData::WorkflowPaused {
                    reason: format!(
                        "Manual transition into state '{target}'; waiting to be resumed."
                    ),
                },
            ) {
                self.notice = Some(Notice::error(format!(
                    "Could not append the pause event: {error}"
                )));
                return;
            }
        }

        // A question asked in the state we just left can no longer be answered.
        if let Err(error) = store.clear_pending_input(&execution.id) {
            self.notice = Some(Notice::error(format!(
                "Could not clear the pending input: {error}"
            )));
            return;
        }

        if let Err(error) = store.save_execution(&execution) {
            self.notice = Some(Notice::error(format!(
                "Could not save the execution: {error}"
            )));
            return;
        }

        self.modal = Modal::None;
        self.notice = Some(Notice::info(if forced {
            format!("Forced transition {from} → {target}.")
        } else {
            format!("Manual transition {from} → {target} ('{label}') applied.")
        }));
        self.refresh();
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
            .map(|listing| {
                listing
                    .entries
                    .into_iter()
                    .map(|entry| entry.id)
                    .collect()
            })
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
                    Modal::Repositories(list) => list
                        .selected
                        .min(listing.entries.len().saturating_sub(1)),
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

        self.adopt_new_task_workflow(resolved, &repositories);
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

        self.adopt_new_task_workflow(resolved, &repositories);
    }

    /// Validates a resolved workflow in full and builds its input form.
    ///
    /// Every violation is kept, not the first one: the operator should see the
    /// whole verdict on one screen, the way `narsor validate` prints it.
    fn adopt_new_task_workflow(&mut self, resolved: ResolvedWorkflow, repositories: &[String]) {
        let Modal::NewTask(form) = &mut self.modal else {
            return;
        };

        form.violations.clear();
        form.error = None;
        form.definition = None;
        form.resolved = None;

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

        form.adopt(definition, resolved, repositories);
    }

    /// Creates the execution the form describes and runs it.
    ///
    /// The form's own values are parsed first, because a rejected input must
    /// leave nothing on disk at all; everything after that is
    /// [`crate::tasks::start_task`], the operation `narsor run`
    /// performs — the workspace a declared `globals.workspace` asks for
    /// included. The run itself stays here, so the interface keeps rendering
    /// while the engine works.
    pub async fn start_new_task(&mut self) {
        let Modal::NewTask(form) = &self.modal else {
            return;
        };
        let Some(definition) = form.definition.clone() else {
            return;
        };
        // What the execution records about where its definition came from: a
        // path when there is one, and always the name — a built-in has no path
        // at all, and without the name it could never be resumed.
        let source = match &form.resolved {
            Some(resolved) => DefinitionSource::from_resolved(resolved),
            None => DefinitionSource::from_path(Path::new(form.path.trim())),
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

        let execution = match tasks::start_task(
            self.catalog.root(),
            self.work.root(),
            &self.engine,
            &definition,
            source,
            inputs,
        ) {
            Ok(execution) => execution,
            Err(error) => {
                let message = error.to_string();
                // An error that left an execution behind happened after it
                // was created: the modal has nothing left to correct, so it
                // closes and the list shows the failed execution instead.
                match error.execution_id() {
                    Some(execution_id) => {
                        let execution_id = execution_id.to_string();
                        self.modal = Modal::None;
                        self.refresh();
                        self.select_execution(&execution_id);
                    }
                    None => {
                        if let Modal::NewTask(form) = &mut self.modal {
                            form.error = Some(message.clone());
                        }
                    }
                }
                self.notice = Some(Notice::error(message));
                return;
            }
        };

        self.modal = Modal::None;
        self.notice = Some(Notice::info(format!(
            "Started execution {}; running in the background.",
            execution.id
        )));
        self.refresh();
        self.select_execution(&execution.id);

        let engine = Arc::clone(&self.engine);
        let execution_id = execution.id.clone();
        self.spawn_background(async move { engine.run(&definition, &execution_id).await });
    }

    /// Puts the cursor on one execution, if the list holds it.
    pub fn select_execution(&mut self, execution_id: &str) {
        if let Some(index) = self
            .executions
            .iter()
            .position(|row| row.id == execution_id)
        {
            self.selected = index;
            self.focus = Panel::Executions;
            self.reload_selection();
        }
    }

    /// Runs or resumes the selected execution, in the background (R1, R2):
    /// this returns as soon as the engine call is dispatched, and its outcome
    /// — success or failure — reaches the interface later as an event (R4).
    pub async fn run_selected(&mut self) {
        let Some(row) = self.selected().cloned() else {
            self.notice = Some(Notice::error("No execution is selected."));
            return;
        };
        let Some(definition) = self.definition.clone() else {
            self.notice = Some(Notice::error(self.definition_error.clone().unwrap_or_else(
                || "No workflow definition for this execution.".to_string(),
            )));
            return;
        };

        if self.pending.is_some() {
            self.notice = Some(Notice::error(
                "The execution is waiting for an answer: use i, not s.",
            ));
            return;
        }

        let engine = Arc::clone(&self.engine);
        let execution_id = row.id.clone();
        match row.status {
            ExecutionStatus::Running => {
                self.spawn_background(async move { engine.run(&definition, &execution_id).await });
            }
            ExecutionStatus::Paused => {
                self.spawn_background(
                    async move { engine.resume(&definition, &execution_id).await },
                );
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
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

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
            let current = field
                .options
                .iter()
                .position(|option| option == &form.values[index])
                .unwrap_or(0);
            let next = wrap(current, delta, field.options.len());
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
        EventData::WorkflowPaused { .. } | EventData::HumanInputRequested { .. } => {
            row.status = ExecutionStatus::Paused
        }
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
        EventData::ActionRetrying { retry_count, .. } => {
            format!("action_retrying  attempt {retry_count}")
        }
        EventData::DecisionRequested { provider } => format!("decision_requested  {provider}"),
        EventData::DecisionMade {
            provider,
            output_label,
            ..
        } => format!("decision_made  {provider} → {output_label}"),
        EventData::HumanInputRequested { instruction } => {
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
        other => format!("{other:?}"),
    };

    format!(
        "{:>4}  {}  {}",
        event.sequence,
        event.timestamp.format("%H:%M:%S"),
        detail
    )
}
