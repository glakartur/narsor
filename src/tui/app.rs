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

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use chrono::Utc;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Map, Value};

use crate::api::{
    ActiveProblem, ApiClient, ClientError, ConductorActivity, ConversationResponse,
    ExecutionSummary, HarnessSelectionSnapshot, TransitionRequest,
};
use crate::config::DEFAULT_TUI_REFRESH_SECS;
use crate::domain::{
    ConductorDecisionKind, ConductorPurpose, Context, Event, EventData, ExecutionStatus,
    InputCloseReason, InputParameter, ParamType, ProblemKind, State, WorkflowDefinition,
    WorkflowExecution, Workspace,
};
use crate::engine::actions::script::TRUNCATION_MARKER;
use crate::engine::{
    permitted_labels, permitted_outputs, ConversationState as PositionConversationState, EventLink,
    InputContinuation, PendingHumanInput, Timeline,
};
use crate::persistence::{CatalogPaths, StatePaths, WorkPaths};
use crate::registry::{RepositoryEntry, RepositoryRegistry};
use crate::tasks;
use crate::tui::chat::{ChatActionTarget, ChatFocus, ChatPanel};
use crate::tui::conversation::{classify, EntryKind, TranscriptEvent};
use crate::tui::failure_form::{FailureAction, FailureForm};
use crate::tui::refresh::{RefreshSchedule, DEBOUNCE};
use crate::tui::scroll::{sanitize, ScrollState};
use crate::tui::transcript::{
    TranscriptEntry, TranscriptEntryId, TranscriptId, TranscriptPresentation, TranscriptState,
};
use crate::workflows::{ResolvedWorkflow, WorkflowEntry, WorkflowResolver};

pub use crate::tui::input_form::{
    Entry, EntryMap, FormAction, FormError, FormRow, InputClosure, InputForm, InputKind,
    ProblemForm,
};

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

/// Why `t` does not choose an output of a gate with required fields (R4).
pub const GATE_NEEDS_FORM_MESSAGE: &str =
    "This gate has required fields: answer it with i, so their values reach the workflow \
     (o switches to a forced override).";

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
    /// What the pending input continues, when the daemon reported one — the
    /// difference between a question and a problem (R10).
    pub input: Option<InputContinuation>,
    /// The problem it is stopped on, from the summary or from a live
    /// `ProblemRaised`.
    pub problem: Option<ActiveProblem>,
    /// What the conductor is working on for it, from the summary or from a
    /// live `ConductorStarted` (R11).
    pub conductor: Option<ConductorActivity>,
}

impl ExecutionRow {
    /// The row a daemon summary describes.
    fn of_summary(summary: &ExecutionSummary) -> Self {
        Self {
            id: summary.id.clone(),
            workflow_id: summary.workflow_id.clone(),
            status: summary.status.clone(),
            current_state: summary.current_state.clone(),
            input: summary
                .pending_input
                .as_ref()
                .map(|pending| pending.continuation),
            problem: summary.problem.clone(),
            conductor: summary.conductor.clone(),
        }
    }

    /// "conductor: diagnosing the failure" while the conductor works on it
    /// (R11, AE12); `None` otherwise.
    pub fn conductor_line(&self) -> Option<String> {
        self.conductor
            .as_ref()
            .map(|activity| conductor_working_text(activity.purpose))
    }

    /// Whether it waits for an operator, and on what (R9, R10).
    ///
    /// A question is an `InputNeeded` gate or agent question; a problem is
    /// an `InputNeeded` problem input, or a `Failed` execution with an
    /// active failure problem.
    pub fn attention(&self) -> Option<Attention> {
        match self.status {
            ExecutionStatus::InputNeeded => {
                let problem = self.input == Some(InputContinuation::ResolveOutput)
                    || (self.input.is_none()
                        && self
                            .problem
                            .as_ref()
                            .is_some_and(|problem| problem.kind == ProblemKind::Output));
                Some(if problem {
                    Attention::Problem
                } else {
                    Attention::Question
                })
            }
            ExecutionStatus::Failed => self
                .problem
                .as_ref()
                .filter(|problem| problem.kind == ProblemKind::Failure)
                .map(|_| Attention::Problem),
            _ => None,
        }
    }
}

/// What an execution waits on an operator for (R9, R10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attention {
    /// A gate or an agent's question: someone has to answer.
    Question,
    /// A step that could not go on by itself: someone has to settle it.
    Problem,
}

impl Attention {
    /// The word drawn next to the status.
    pub fn label(self) -> &'static str {
        match self {
            Self::Question => "question",
            Self::Problem => "problem",
        }
    }
}

/// How many executions wait on a question and how many on a problem.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AttentionCounts {
    /// Executions waiting on a question.
    pub questions: usize,
    /// Executions stopped on a problem.
    pub problems: usize,
}

impl AttentionCounts {
    /// The header's text, e.g. "2 questions · 1 problem"; `None` when
    /// nothing waits.
    pub fn summary(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.questions > 0 {
            parts.push(plural(self.questions, "question"));
        }
        if self.problems > 0 {
            parts.push(plural(self.problems, "problem"));
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

/// "1 problem", "2 problems".
fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
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
    /// Open the failure variant of the `i` form (KTD4): it reads the
    /// conductor's report and the proposals for it first.
    OpenFailureForm,
    /// Read the task view's conductor chat, from after the last entry held.
    LoadChat,
    /// Send the chat panel's draft to the conductor (F4).
    SendChat,
    /// Carry out a proposal of the conductor's (R28).
    ConfirmProposal {
        execution_id: String,
        proposal_id: String,
    },
    /// Reject a proposal of the conductor's.
    RejectProposal {
        execution_id: String,
        proposal_id: String,
    },
    /// Undo a chosen output of the conductor's, once confirmed (R31,
    /// KTD13).
    UndoDecision {
        execution_id: String,
        decision_id: String,
    },
    /// Restart a failed step with a note, through the recovery that names
    /// the failure's key (KTD9).
    RecoverFailure {
        execution_id: String,
        episode: String,
        comment: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// The recovery note
// ---------------------------------------------------------------------------

/// A free-form note sent with an explicit recovery of an agent step.
#[derive(Debug, Clone, Default)]
pub struct RecoveryCommentForm {
    /// The text to append to the next agent prompt.
    pub value: String,
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
    /// The generated human-input form. Boxed: a problem's form carries its
    /// answers and key–value maps, far more than any other overlay.
    Input(Box<InputForm>),
    /// Operator note for a recovered agent attempt.
    RecoveryComment(RecoveryCommentForm),
    /// The failure variant of `i` (KTD4): the conductor's diagnosis, its
    /// proposals and a restart with a note — or the raw error. Boxed for
    /// the same reason as [`Modal::Input`].
    Failure(Box<FailureForm>),
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
        entries: Vec<TranscriptEvent>,
        /// The cursor to continue from on the next manual refresh (R8).
        next_entry: u64,
        /// Whether the attempt's conversation is closed (compressed).
        closed: bool,
        /// Whether the on-disk file's last line was incomplete when read.
        truncated_tail: bool,
        /// All currently readable forward pages were fetched.  A truncated
        /// tail is deliberately not called complete: its final record is not
        /// yet safe to show as retained history.
        complete: bool,
        /// Source-neutral reader state.  Unlike `content_scroll`, this is a
        /// logical-line viewport and preserves its anchor across reloads.
        transcript: TranscriptState,
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
    /// The conductor's chat (F4): its history, and the message being
    /// written, which no refresh resets.
    pub chat: ChatPanel,
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
    /// When the next automatic refresh is due (KTD16).
    refresh: RefreshSchedule,
    /// `WorkflowFailed` notices held back while the conductor may still
    /// take up the failure, by execution, with when they arrived — the
    /// problem or the conductor's own retry is what the operator hears of
    /// instead (U13).
    held_failures: HashMap<String, (Notice, Instant)>,
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
            refresh: RefreshSchedule::new(
                Some(Duration::from_secs(DEFAULT_TUI_REFRESH_SECS)),
                Instant::now(),
            ),
            held_failures: HashMap::new(),
        }
    }

    /// The same, re-reading the daemon on its own every `interval`
    /// (`tui_refresh_interval`); `None` leaves only the refreshes the live
    /// stream asks for, and `r`.
    pub fn with_refresh_interval(mut self, interval: Option<Duration>) -> Self {
        self.refresh = RefreshSchedule::new(interval, Instant::now());
        self
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

    /// Mutable task-view access for terminal integration tests and render setup.
    pub fn task_view_mut(&mut self) -> Option<&mut TaskView> {
        match &mut self.screen {
            Screen::Task(view) => Some(view),
            Screen::Main => None,
        }
    }

    /// Updates transcript viewports from the renderer's resolved pane sizes.
    pub fn set_task_transcript_viewports(
        &mut self,
        content_height: usize,
        chat_height: Option<usize>,
    ) {
        let Screen::Task(view) = &mut self.screen else {
            return;
        };
        if let TaskContent::Conversation { transcript, .. } = &mut view.content {
            transcript.set_viewport(content_height);
        }
        if let Some(height) = chat_height {
            view.chat.transcript.set_viewport(height);
        }
    }

    /// The health of the live channel.
    pub fn stream_status(&self) -> &StreamStatus {
        &self.stream
    }

    /// How many executions wait on a question and on a problem (R9).
    pub fn attention_counts(&self) -> AttentionCounts {
        let mut counts = AttentionCounts::default();
        for row in &self.executions {
            match row.attention() {
                Some(Attention::Question) => counts.questions += 1,
                Some(Attention::Problem) => counts.problems += 1,
                None => {}
            }
        }
        counts
    }

    /// What the highlighted execution waits on, if anything.
    pub fn selected_attention(&self) -> Option<Attention> {
        self.selected().and_then(ExecutionRow::attention)
    }

    /// The problem the highlighted execution is stopped on, if any.
    pub fn selected_problem(&self) -> Option<&ActiveProblem> {
        self.selected().and_then(|row| row.problem.as_ref())
    }

    /// The list row of `execution_id`, if the list holds it.
    pub fn row(&self, execution_id: &str) -> Option<&ExecutionRow> {
        self.executions.iter().find(|row| row.id == execution_id)
    }

    /// Whether the highlighted execution is `Failed` on a problem the
    /// conductor reported (KTD4): `i` opens the failure form for it.
    fn selected_failure_problem(&self) -> Option<&ActiveProblem> {
        self.selected()
            .filter(|row| row.status == ExecutionStatus::Failed)
            .and_then(|row| row.problem.as_ref())
            .filter(|problem| problem.kind == ProblemKind::Failure)
    }

    /// Whether an automatic refresh is due at `now` (KTD16).
    pub fn refresh_due(&self, now: Instant) -> bool {
        self.refresh.is_due(now)
    }

    /// Whether a live event is waiting for the refresh it asked for.
    pub fn refresh_requested(&self) -> bool {
        self.refresh.is_requested()
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

    /// What `i` does for the pending request, if there is one: the three
    /// kinds read differently in the bar (R10).
    fn input_shortcut(&self) -> Option<&'static str> {
        if self.pending.is_none() && self.selected_failure_problem().is_some() {
            return Some("resolve failure");
        }
        self.pending
            .as_ref()
            .map(|pending| match InputKind::of(pending.continuation) {
                InputKind::Gate => "answer",
                InputKind::AgentQuestion => "reply to agent",
                InputKind::Problem => "resolve problem",
            })
    }

    /// The shortcuts the bottom bar shows, given the focus and the modal.
    pub fn shortcuts(&self) -> Vec<(&'static str, &'static str)> {
        if let Modal::Input(form) = &self.modal {
            return form_shortcuts(form);
        }
        if let Modal::Failure(form) = &self.modal {
            return failure_shortcuts(form);
        }
        if let Screen::Task(view) = &self.screen {
            if view.chat.open {
                return chat_shortcuts(&view.chat);
            }
            let mut keys = vec![
                ("Tab", "axis/content"),
                ("j/k", "select"),
                ("PgUp/PgDn", "page"),
                ("g/G", "top/bottom"),
                ("r", "refresh"),
            ];
            if view.focus == TaskFocus::Content {
                if matches!(view.content, TaskContent::Conversation { .. }) {
                    keys[1] = ("j/k", "scroll history");
                    keys.push(("Enter/space/e", "expand detail"));
                    keys.push(("v", "raw/rendered"));
                } else {
                    keys.push(("Enter/space", "expand/collapse"));
                }
            }
            if let Some(action) = self.input_shortcut() {
                keys.push(("i", action));
            }
            keys.push(("c", "conductor chat"));
            keys.push(("Esc", "back"));
            return keys;
        }
        match &self.modal {
            Modal::Input(form) => form_shortcuts(form),
            Modal::Failure(form) => failure_shortcuts(form),
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
                        if let Some(action) = self.input_shortcut() {
                            keys.push(("i", action));
                        }
                        keys.push(("t", "transition"));
                        keys.push(("x", "interrupt"));
                    }
                    Panel::Details => {
                        keys.push(("j/k", "variables"));
                        if let Some(action) = self.input_shortcut() {
                            keys.push(("i", action));
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
                self.refresh.done(Instant::now());
                return;
            }
        };

        // Rows are rebuilt from the daemon's word, except for what only the
        // stream has told this interface so far: a failure problem the
        // summary does not carry yet (see `ExecutionSummary::problem`).
        let rows = listing
            .executions
            .iter()
            .map(|summary| {
                let mut row = ExecutionRow::of_summary(summary);
                if row.problem.is_none() && row.status == ExecutionStatus::Failed {
                    row.problem = self
                        .executions
                        .iter()
                        .find(|old| old.id == row.id && old.status == ExecutionStatus::Failed)
                        .and_then(|old| old.problem.clone())
                        .filter(|problem| problem.kind == ProblemKind::Failure);
                }
                row
            })
            .collect();
        self.executions = rows;
        self.release_held_failures(Instant::now());

        self.selected = previous
            .and_then(|id| self.executions.iter().position(|row| row.id == id))
            .unwrap_or(0);
        if self.selected >= self.executions.len() {
            self.selected = self.executions.len().saturating_sub(1);
        }

        self.reload_selection().await;
        self.refresh.done(Instant::now());
    }

    /// Decides what became of each held `WorkflowFailed` notice, now that
    /// the list says where its execution stands: a problem is announced
    /// instead, an execution that moved on (recovered, retried by the
    /// conductor) needs no notice, one the conductor still works on stays
    /// held, and a failure nobody took up is announced as it was.
    fn release_held_failures(&mut self, now: Instant) {
        for (id, (notice, held_at)) in std::mem::take(&mut self.held_failures) {
            let Some(row) = self.row(&id) else {
                continue;
            };
            if row.status != ExecutionStatus::Failed {
                continue;
            }
            if let Some(problem) = &row.problem {
                self.notice = Some(Notice::error(format!(
                    "Execution {id} has a problem: {}",
                    first_line(&problem.reason)
                )));
                continue;
            }
            // Too fresh to tell: the conductor's marker may not be written
            // yet. The refresh the failure itself asked for comes later.
            if row.conductor.is_some() || now.duration_since(held_at) < DEBOUNCE {
                self.held_failures.insert(id, (notice, held_at));
                continue;
            }
            self.notice = Some(notice);
        }
    }

    /// Whether a `WorkflowFailed` notice of `execution_id` is held back
    /// while the conductor may take up the failure.
    pub fn holds_failure_notice(&self, execution_id: &str) -> bool {
        self.held_failures.contains_key(execution_id)
    }

    /// The refresh the event loop runs when [`App::refresh_due`] says so
    /// (KTD16): the list and the selection, and the task view's axis and
    /// conversation when it is showing. Nothing the operator is looking at
    /// or typing into is reset.
    pub async fn auto_refresh(&mut self) {
        self.refresh().await;
        if matches!(self.screen, Screen::Task(_)) {
            self.refresh_task().await;
        }
    }

    /// Re-reads everything that depends on which execution is selected — a
    /// second `GET /executions`, filtered down to the one row this needs,
    /// rather than threading the listing [`App::refresh`] already has
    /// through every caller of this method (cursor movement, `select_execution`,
    /// a mutation's own follow-up reload): simplicity over saving one small
    /// request on a loopback connection.
    ///
    /// Non-destructive when the selection has not changed (KTD16): the
    /// variable and event cursors and the follow mode stay as the operator
    /// left them, only events newer than the last one held are appended, and
    /// the modal is never touched — an open input form whose request is no
    /// longer the pending one is only marked closed ([`App::sync_open_form`]).
    /// Selecting a different execution loads it from scratch.
    pub async fn reload_selection(&mut self) {
        let Some(id) = self.selected().map(|row| row.id.clone()) else {
            self.clear_selection();
            return;
        };
        let same = self
            .execution
            .as_ref()
            .is_some_and(|execution| execution.id == id);
        if !same {
            self.clear_selection();
        }

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
            self.clear_selection();
            self.notice = Some(Notice::error(format!(
                "Could not load execution '{id}': not found"
            )));
            return;
        };

        let mut variables: Vec<(String, Value)> = summary.variables.clone().into_iter().collect();
        variables.sort_by(|left, right| left.0.cmp(&right.0));
        self.variables = variables;
        self.variable_cursor = self
            .variable_cursor
            .min(self.variables.len().saturating_sub(1));
        self.pending = summary.pending_input.clone();

        // An execution with no events is an empty stream, not a failure: it has
        // simply not got anywhere yet.
        let after = self.events.last().map_or(0, |event| event.sequence);
        match self.client.events(&id, after).await {
            Ok(events) => {
                let fresh = events.into_iter().filter(|event| event.sequence > after);
                self.events.extend(fresh);
                if self.follow || !same {
                    self.event_cursor = self.events.len().saturating_sub(1);
                }
            }
            Err(error) => {
                self.notice = Some(Notice::error(format!(
                    "Could not read the event log: {error}"
                )));
            }
        }

        self.definition = None;
        self.definition_error = None;
        self.load_definition(&summary);
        self.execution = Some(summary);
        self.sync_open_form();
    }

    /// Forgets everything that belongs to the previous selection.
    fn clear_selection(&mut self) {
        self.execution = None;
        self.definition = None;
        self.definition_error = None;
        self.pending = None;
        self.variables.clear();
        self.variable_cursor = 0;
        self.events.clear();
        self.event_cursor = 0;
    }

    /// Marks an open input form closed when the request it answers is no
    /// longer the pending one (R5, AE13) — never rebuilds it, so what the
    /// operator typed stays. The closing `HumanInputReceived` says who closed
    /// it and how; without one the form only says it is no longer pending.
    fn sync_open_form(&mut self) {
        if let (Modal::Failure(form), Some(execution)) = (&mut self.modal, &self.execution) {
            let still_failed = execution.status == ExecutionStatus::Failed
                && execution
                    .problem
                    .as_ref()
                    .is_some_and(|problem| problem.episode.as_deref() == Some(form.key.as_str()));
            if execution.id == form.execution_id && !still_failed && form.closed.is_none() {
                form.closed = Some(
                    "The execution is no longer failed on this failure: it was recovered or \
                     settled elsewhere. Nothing from this form can be sent; Esc closes it."
                        .to_string(),
                );
            }
            return;
        }
        let Modal::Input(form) = &mut self.modal else {
            return;
        };
        if form.closed.is_some()
            || self
                .execution
                .as_ref()
                .map(|execution| execution.id.as_str())
                != Some(form.execution_id.as_str())
        {
            return;
        }
        let still_pending = self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.episode_key() == form.episode);
        if still_pending {
            // The conductor's summary of the gate is made beside it and may
            // arrive after the form was opened (R30).
            if let Some(execution) = &self.execution {
                form.annotate(execution.annotation.as_ref());
            }
            return;
        }
        let closure = self
            .events
            .iter()
            .rev()
            .filter_map(|event| InputClosure::from_event(&event.data))
            .find(|(episode, _)| *episode == Some(form.episode.as_str()))
            .map(|(_, closure)| closure)
            .unwrap_or_else(InputClosure::unexplained);
        form.close(closure);
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
            apply_to_row(row, &event);
        }

        // A background run's outcome is set as a notice by execution id, not
        // only for whichever execution happens to be selected (R6) — this is
        // in addition to the row update above, not a replacement for it.
        // A failure the conductor may take up is not announced yet: the
        // problem it raises, or its own retry, is (U13). A refresh releases
        // a failure nobody took up (`release_held_failures`).
        match &event.data {
            EventData::ProblemRaised { .. }
            | EventData::ConductorDecision {
                kind: ConductorDecisionKind::Retry,
                ..
            } => {
                self.held_failures.remove(&event.workflow_execution_id);
            }
            _ => {}
        }
        if let Some(notice) = notice_for(&event) {
            if matches!(event.data, EventData::WorkflowFailed { .. }) {
                self.held_failures.insert(
                    event.workflow_execution_id.clone(),
                    (notice, Instant::now()),
                );
            } else {
                self.notice = Some(notice);
            }
        }

        // The stream says something changed; the daemon has the full story
        // (the pending input, the problem, the context), read a moment later
        // so a burst of events costs one read (KTD16).
        if triggers_refresh(&event.data) {
            self.refresh.request(Instant::now());
        }

        // Someone else closed the request the open form answers: the form
        // says so at once, rather than at the next refresh (R5).
        if let (Modal::Input(form), Some((Some(episode), closure))) =
            (&mut self.modal, InputClosure::from_event(&event.data))
        {
            if form.execution_id == event.workflow_execution_id && form.episode == episode {
                form.close(closure);
            }
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
            Modal::Failure(_) => self.on_key_failure(key),
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
            KeyCode::Char('i') => self.request_input_form(),
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
            execution_id: execution_id.clone(),
            origin,
            timeline,
            selected,
            axis_scroll,
            content_scroll: ScrollState::new(),
            focus: TaskFocus::Axis,
            content: TaskContent::Empty,
            expanded: HashSet::new(),
            chat: {
                let mut chat = ChatPanel::default();
                chat.set_execution(&execution_id);
                chat
            },
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
        if matches!(&self.screen, Screen::Task(view) if view.chat.loaded) {
            self.load_chat().await;
        }
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

                // Fetch through the current forward end before exposing the
                // attempt.  A byte-bounded response cannot itself say it was
                // the final page, so an empty continuation establishes that
                // boundary.  An incomplete tail stops the walk and remains
                // visibly incomplete rather than being mislabeled complete.
                let mut cursor = from;
                let mut fresh = Vec::new();
                let (next_entry, closed, truncated_tail, complete) = loop {
                    match self
                        .client
                        .conversation(&execution_id, &attempt_id, cursor, CONVERSATION_BYTE_BUDGET)
                        .await
                    {
                        Ok(ConversationResponse::Recorded {
                            entries,
                            next_entry: page_next_entry,
                            closed: page_closed,
                            truncated_tail: page_truncated_tail,
                        }) => {
                            if entries.is_empty() {
                                break (
                                    page_next_entry,
                                    page_closed,
                                    page_truncated_tail,
                                    !page_truncated_tail,
                                );
                            }
                            fresh.extend(entries.iter().map(classify));
                            if page_truncated_tail || page_next_entry <= cursor {
                                break (page_next_entry, page_closed, page_truncated_tail, false);
                            }
                            cursor = page_next_entry;
                        }
                        Ok(ConversationResponse::NoRecord) => {
                            self.set_task_events_fallback(
                                &label,
                                "no conversation record".to_string(),
                            );
                            return;
                        }
                        Err(error) => {
                            self.notice = Some(Notice::error(format!(
                                "Could not load the conversation: {error}"
                            )));
                            return;
                        }
                    }
                };
                if let Screen::Task(view) = &mut self.screen {
                    let (existing, mut transcript) = if append {
                        match std::mem::replace(&mut view.content, TaskContent::Empty) {
                            TaskContent::Conversation {
                                entries,
                                transcript,
                                ..
                            } => (entries, transcript),
                            _ => (
                                Vec::new(),
                                TranscriptState::new(TranscriptId::from(attempt_id.clone())),
                            ),
                        }
                    } else {
                        (
                            Vec::new(),
                            TranscriptState::new(TranscriptId::from(attempt_id.clone())),
                        )
                    };
                    let mut entries = existing;
                    entries.extend(fresh);
                    let projected = entries.iter().map(agent_transcript_entry).collect();
                    transcript.replace_entries(projected);
                    view.content = TaskContent::Conversation {
                        entries,
                        next_entry,
                        closed,
                        truncated_tail,
                        complete,
                        transcript,
                    };
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
        // The input form opens over the task view and takes every key while
        // it is open.
        if matches!(self.modal, Modal::Input(_)) {
            return self.on_key_form(key);
        }
        if matches!(self.modal, Modal::Failure(_)) {
            return self.on_key_failure(key);
        }
        // The chat takes every key while it is open, so a message can be
        // typed; Esc closes it and keeps the draft.
        if matches!(&self.screen, Screen::Task(view) if view.chat.open) {
            return self.on_key_chat(key);
        }
        match key.code {
            KeyCode::Esc => {
                self.leave_task_view();
                Request::None
            }
            KeyCode::Char('c') => {
                if let Screen::Task(view) = &mut self.screen {
                    view.chat.open = true;
                }
                Request::LoadChat
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
            KeyCode::Char('i') => self.open_input_form_for_task(),
            KeyCode::Char('j') | KeyCode::Down => self.move_task_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_task_cursor(-1),
            KeyCode::PageDown => self.page_task_cursor(1),
            KeyCode::PageUp => self.page_task_cursor(-1),
            KeyCode::Char('g') | KeyCode::Home => self.jump_task_cursor(false),
            KeyCode::Char('G') | KeyCode::End => self.jump_task_cursor(true),
            KeyCode::Char('v') if self.task_transcript_focused() => {
                self.toggle_task_raw();
                Request::None
            }
            KeyCode::Char('e') if self.task_transcript_focused() => {
                self.toggle_task_transcript_detail();
                Request::None
            }
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
                TaskContent::Conversation { transcript, .. } => transcript.logical_lines().len(),
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
        if view.focus == TaskFocus::Content {
            if let TaskContent::Conversation { transcript, .. } = &mut view.content {
                if delta >= 0 {
                    transcript.line_down();
                } else {
                    transcript.line_up();
                }
                return Request::None;
            }
        }
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
        if view.focus == TaskFocus::Content {
            if let TaskContent::Conversation { transcript, .. } = &mut view.content {
                if direction >= 0 {
                    transcript.page_down();
                } else {
                    transcript.page_up();
                }
                return Request::None;
            }
        }
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
        if view.focus == TaskFocus::Content {
            if let TaskContent::Conversation { transcript, .. } = &mut view.content {
                if to_end {
                    transcript.to_bottom();
                } else {
                    transcript.to_top();
                }
                return Request::None;
            }
        }
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
        let TaskContent::Conversation { transcript, .. } = &mut view.content else {
            return;
        };
        let Some(anchor) = transcript.anchor() else {
            return;
        };
        transcript.toggle_expanded(&anchor.entry_id);
    }

    fn task_transcript_focused(&self) -> bool {
        matches!(&self.screen,
            Screen::Task(view) if view.focus == TaskFocus::Content
                && matches!(view.content, TaskContent::Conversation { .. }))
    }

    fn toggle_task_raw(&mut self) {
        let Screen::Task(view) = &mut self.screen else {
            return;
        };
        let TaskContent::Conversation { transcript, .. } = &mut view.content else {
            return;
        };
        transcript.toggle_mode();
    }

    fn toggle_task_transcript_detail(&mut self) {
        let Screen::Task(view) = &mut self.screen else {
            return;
        };
        let TaskContent::Conversation { transcript, .. } = &mut view.content else {
            return;
        };
        if let Some(anchor) = transcript.anchor() {
            transcript.toggle_expanded(&anchor.entry_id);
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
        match form.on_key(key) {
            FormAction::None => Request::None,
            // A closed request cannot be answered any more (R5): the form
            // stays, with what was typed, until Esc.
            FormAction::Submit if form.closed.is_some() => {
                let message = closed_form_message(form);
                self.notice = Some(Notice::error(message));
                Request::None
            }
            FormAction::Submit => Request::SubmitInput,
            FormAction::Cancel => {
                self.modal = Modal::None;
                Request::None
            }
        }
    }

    /// Keys inside the failure form (KTD4).
    fn on_key_failure(&mut self, key: KeyEvent) -> Request {
        let Modal::Failure(form) = &mut self.modal else {
            return Request::None;
        };
        let action = form.on_key(key);
        if let (
            Some(closed),
            FailureAction::Confirm(_) | FailureAction::Reject(_) | FailureAction::Restart(_),
        ) = (&form.closed, &action)
        {
            self.notice = Some(Notice::error(closed.clone()));
            return Request::None;
        }
        let execution_id = form.execution_id.clone();
        match action {
            FailureAction::None => Request::None,
            FailureAction::Cancel => {
                self.modal = Modal::None;
                Request::None
            }
            FailureAction::Confirm(proposal_id) => Request::ConfirmProposal {
                execution_id,
                proposal_id,
            },
            FailureAction::Reject(proposal_id) => Request::RejectProposal {
                execution_id,
                proposal_id,
            },
            FailureAction::Restart(note) => Request::RecoverFailure {
                execution_id,
                episode: form.key.clone(),
                comment: Some(note.trim().to_string()).filter(|note| !note.is_empty()),
            },
        }
    }

    /// Keys inside the task view's conductor chat (F4, R28, R31).
    fn on_key_chat(&mut self, key: KeyEvent) -> Request {
        let Screen::Task(view) = &mut self.screen else {
            return Request::None;
        };
        let execution_id = view.execution_id.clone();
        let chat = &mut view.chat;

        // An undo waits for its own confirmation, which shows what the
        // decision was, how sure the conductor was and why (R31).
        if let Some(confirm) = &chat.undo {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let decision_id = confirm.decision_id.clone();
                    chat.undo = None;
                    return Request::UndoDecision {
                        execution_id,
                        decision_id,
                    };
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => chat.undo = None,
                _ => {}
            }
            return Request::None;
        }

        match (key.code, chat.focus) {
            (KeyCode::Esc, _) => chat.open = false,
            (KeyCode::Tab, _) => {
                chat.focus = match chat.focus {
                    ChatFocus::Draft => ChatFocus::Reading,
                    ChatFocus::Reading => {
                        chat.cycle_action_target(0);
                        ChatFocus::Action
                    }
                    ChatFocus::Action => ChatFocus::Draft,
                };
            }
            (KeyCode::BackTab, _) => {
                chat.focus = match chat.focus {
                    ChatFocus::Draft => ChatFocus::Action,
                    ChatFocus::Reading => ChatFocus::Draft,
                    ChatFocus::Action => ChatFocus::Reading,
                };
            }
            (KeyCode::Down, ChatFocus::Reading) | (KeyCode::Char('j'), ChatFocus::Reading) => {
                chat.line_down()
            }
            (KeyCode::Up, ChatFocus::Reading) | (KeyCode::Char('k'), ChatFocus::Reading) => {
                chat.line_up()
            }
            (KeyCode::PageDown, ChatFocus::Reading) | (KeyCode::Char('d'), ChatFocus::Reading) => {
                chat.page_down()
            }
            (KeyCode::PageUp, ChatFocus::Reading) | (KeyCode::Char('b'), ChatFocus::Reading) => {
                chat.page_up()
            }
            (KeyCode::Char('g') | KeyCode::Home, ChatFocus::Reading) => chat.to_top(),
            (KeyCode::Char('G') | KeyCode::End, ChatFocus::Reading) => chat.to_bottom(),
            (KeyCode::Char('['), ChatFocus::Action) => chat.cycle_action_target(-1),
            (KeyCode::Char(']'), ChatFocus::Action) => chat.cycle_action_target(1),
            (KeyCode::Char('v'), ChatFocus::Reading) => chat.toggle_raw(),
            (KeyCode::Char('e'), ChatFocus::Reading) => {
                if let Some(anchor) = chat.transcript.anchor() {
                    chat.transcript.toggle_expanded(&anchor.entry_id);
                }
            }
            (KeyCode::Enter, ChatFocus::Draft) => {
                if chat.draft.trim().is_empty() {
                    self.notice = Some(Notice::error("Write a message to the conductor first."));
                    return Request::None;
                }
                return Request::SendChat;
            }
            (KeyCode::Backspace, ChatFocus::Draft) => {
                chat.draft.pop();
            }
            (KeyCode::Char(character), ChatFocus::Draft) => chat.draft.push(character),
            (KeyCode::Char('y') | KeyCode::Enter, ChatFocus::Action) => {
                let proposal_id = match chat.action_target.as_ref() {
                    Some(ChatActionTarget::Proposal(id)) => Some(id.clone()),
                    // Legacy programmatic callers can still select an entry;
                    // capture its immutable ID before asking the daemon.
                    None => chat
                        .selected_item()
                        .and_then(|item| item.open_proposal().map(String::from)),
                    _ => None,
                };
                match proposal_id {
                    Some(proposal_id) => {
                        return Request::ConfirmProposal {
                            execution_id,
                            proposal_id,
                        }
                    }
                    None => {
                        self.notice = Some(Notice::error(
                            "No selected action target is a proposal waiting for confirmation.",
                        ))
                    }
                }
            }
            (KeyCode::Char('n'), ChatFocus::Action) => {
                let proposal_id = match chat.action_target.as_ref() {
                    Some(ChatActionTarget::Proposal(id)) => Some(id.clone()),
                    None => chat
                        .selected_item()
                        .and_then(|item| item.open_proposal().map(String::from)),
                    _ => None,
                };
                match proposal_id {
                    Some(proposal_id) => {
                        return Request::RejectProposal {
                            execution_id,
                            proposal_id,
                        }
                    }
                    None => {
                        self.notice = Some(Notice::error(
                            "No selected action target is a proposal waiting for confirmation.",
                        ))
                    }
                }
            }
            (KeyCode::Char('u'), ChatFocus::Action) => {
                if !chat.ask_undo() {
                    self.notice = Some(Notice::error(
                        "Only the conductor's choice of an output that was not undone yet can \
                         be undone.",
                    ));
                }
            }
            (KeyCode::Char('r'), ChatFocus::Reading | ChatFocus::Action) => {
                return Request::LoadChat
            }
            _ => {}
        }
        Request::None
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
        let gate_needs_form = self.gate_needs_form();
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
                if menu.mode == TransitionMode::Outputs && gate_needs_form {
                    self.notice = Some(Notice::error(GATE_NEEDS_FORM_MESSAGE));
                    return Request::None;
                }
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

    /// `i`: opens the form for whatever the selected execution waits on — a
    /// gate, an agent's question or a problem (R1).
    pub fn open_input_form(&mut self) {
        match self.pending.as_ref() {
            Some(pending) => {
                let annotation = self
                    .execution
                    .as_ref()
                    .and_then(|execution| execution.annotation.as_ref());
                self.modal = Modal::Input(Box::new(
                    InputForm::new(pending, &self.variables).with_annotation(annotation),
                ));
            }
            None => {
                self.notice = Some(Notice::error(
                    "The selected execution is not waiting for human input.",
                ))
            }
        }
    }

    /// `i` in the task view: the same form, for the task on screen. The
    /// task view is always entered on the selected execution, so its pending
    /// request is the one [`App::pending`] holds; the check only guards
    /// against the two ever disagreeing.
    fn open_input_form_for_task(&mut self) -> Request {
        let Screen::Task(view) = &self.screen else {
            return Request::None;
        };
        let matches_view = self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.execution_id == view.execution_id);
        let selected_view = self
            .selected()
            .is_some_and(|row| row.id == view.execution_id);
        if matches_view || (selected_view && self.pending.is_none()) {
            return self.request_input_form();
        }
        self.notice = Some(Notice::error("This task is not waiting for human input."));
        Request::None
    }

    /// `i`: the form for whatever the selected execution waits on — or, for
    /// one `Failed` on a problem the conductor reported, the failure form
    /// (KTD4), which first needs the report from the daemon.
    fn request_input_form(&mut self) -> Request {
        if self.pending.is_none() {
            if self.selected_failure_problem().is_some() {
                return Request::OpenFailureForm;
            }
            let diagnosing = self
                .selected()
                .filter(|row| row.status == ExecutionStatus::Failed)
                .and_then(ExecutionRow::conductor_line);
            if let Some(working) = diagnosing {
                self.notice = Some(Notice::info(format!(
                    "{working}. The failure can be settled with i once it concludes."
                )));
                return Request::None;
            }
        }
        self.open_input_form();
        Request::None
    }

    /// Whether the pending request is a gate with required fields: choosing
    /// one of its outputs with `t` would skip those fields, and the next
    /// steps would never see their values (R4).
    fn gate_needs_form(&self) -> bool {
        self.pending.as_ref().is_some_and(|pending| {
            pending.continuation == InputContinuation::CompleteStep
                && pending.fields().iter().any(|field| field.required)
        })
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
    /// The form closes only once the daemon accepts the answer; what happens
    /// after that surfaces as a notice from the live stream (R6).
    pub async fn submit_input(&mut self) {
        let Modal::Input(form) = &mut self.modal else {
            return;
        };
        if form.closed.is_some() {
            self.notice = Some(Notice::error(closed_form_message(form)));
            return;
        }

        let delivery = match form.validate() {
            Ok(delivery) => delivery,
            Err(error) => {
                self.notice = Some(Notice::error(error.message.clone()));
                form.fail(error);
                return;
            }
        };
        let execution_id = form.execution_id.clone();

        match self.client.deliver_input(&execution_id, delivery).await {
            Ok(_) => {
                self.modal = Modal::None;
                self.notice = Some(Notice::info(format!(
                    "Delivered input to execution {execution_id}; running in the background."
                )));
            }
            // The daemon has the last word (a `400` names what is wrong, a
            // `409` says the request is gone): the form stays open with what
            // was typed, so nothing has to be typed again.
            Err(error) => {
                let message = client_error_message(&execution_id, error);
                if let Modal::Input(form) = &mut self.modal {
                    form.error = Some(message.clone());
                    form.error_row = None;
                }
                self.notice = Some(Notice::error(message));
            }
        }
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
        if self.gate_needs_form() {
            self.notice = Some(Notice::error(GATE_NEEDS_FORM_MESSAGE));
            return;
        }

        self.manual_transition(&target).await;
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
                    other.as_str()
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

    // -- the conductor (U13) ----------------------------------------------

    /// Opens the failure form of the selected execution (KTD4): the
    /// conductor's report of the failure, and its proposals still open.
    pub async fn open_failure_form(&mut self) {
        let Some(execution_id) = self
            .selected_failure_problem()
            .and_then(|_| self.selected())
            .map(|row| row.id.clone())
        else {
            self.notice = Some(Notice::error(
                "The selected execution is not failed on a problem.",
            ));
            return;
        };
        let state = match self.client.conductor_state(&execution_id).await {
            Ok(state) => state,
            Err(error) => {
                self.notice = Some(Notice::error(client_error_message(&execution_id, error)));
                return;
            }
        };
        let chat = match self.client.conductor_chat(&execution_id, 0).await {
            Ok(chat) => chat.entries,
            Err(error) => {
                self.notice = Some(Notice::error(client_error_message(&execution_id, error)));
                return;
            }
        };
        let form = state
            .failure
            .as_ref()
            .and_then(|report| FailureForm::new(&execution_id, report, &chat));
        match form {
            Some(form) => self.modal = Modal::Failure(Box::new(form)),
            None => {
                self.notice = Some(Notice::error(format!(
                    "Execution {execution_id} is no longer failed on a problem."
                )));
                self.refresh().await;
            }
        }
    }

    /// Reads the task view's chat from after the last entry it holds (F4):
    /// the whole history the first time, which is how it survives a restart
    /// of this interface (R26).
    pub async fn load_chat(&mut self) {
        let Screen::Task(view) = &self.screen else {
            return;
        };
        let execution_id = view.execution_id.clone();
        let after = view.chat.last_sequence();
        match self.client.conductor_chat(&execution_id, after).await {
            Ok(response) => {
                let mut stale_target = false;
                if let Screen::Task(view) = &mut self.screen {
                    if view.execution_id == execution_id {
                        stale_target = view.chat.append(response.entries);
                    }
                }
                if stale_target {
                    self.notice = Some(Notice::error(
                        "The selected conductor action is no longer available after refresh.",
                    ));
                }
            }
            Err(error) => {
                self.notice = Some(Notice::error(format!(
                    "Could not read the conductor's chat: {}",
                    client_error_message(&execution_id, error)
                )));
            }
        }
    }

    /// Sends the chat's draft to the conductor (F4). The draft is kept until
    /// the daemon accepts it, so nothing has to be typed again after a
    /// failure; its answer follows in the history.
    pub async fn send_chat(&mut self) {
        let Screen::Task(view) = &self.screen else {
            return;
        };
        let execution_id = view.execution_id.clone();
        let text = view.chat.draft.trim().to_string();
        if text.is_empty() {
            return;
        }
        match self.client.post_chat(&execution_id, &text).await {
            Ok(_) => {
                if let Screen::Task(view) = &mut self.screen {
                    view.chat.draft.clear();
                }
                self.notice = Some(Notice::info(
                    "Sent to the conductor; its answer follows in the chat.",
                ));
                self.load_chat().await;
            }
            Err(error) => {
                self.notice = Some(Notice::error(client_error_message(&execution_id, error)));
            }
        }
    }

    /// Carries out a proposal of the conductor's (R28). A proposal about a
    /// moment the execution has moved on from is refused (KTD9): the
    /// operator reads why, and the view is read again.
    pub async fn confirm_proposal(&mut self, execution_id: &str, proposal_id: &str) {
        let result = self
            .client
            .confirm_proposal(execution_id, proposal_id)
            .await;
        self.settle_proposal(execution_id, result).await;
    }

    /// Rejects a proposal of the conductor's.
    pub async fn reject_proposal(&mut self, execution_id: &str, proposal_id: &str) {
        let result = self.client.reject_proposal(execution_id, proposal_id).await;
        self.settle_proposal(execution_id, result).await;
    }

    /// Reports what became of a proposal, then reads the view again — a
    /// carried-out proposal moved the execution on, a refused one says the
    /// view was stale.
    async fn settle_proposal(
        &mut self,
        execution_id: &str,
        result: Result<crate::conductor::ProposalOutcome, ClientError>,
    ) {
        match result {
            Ok(outcome) => {
                let mut text = outcome.message;
                if outcome.repeated {
                    text.push_str(" (It had already been settled; nothing was done now.)");
                }
                self.notice = Some(Notice::info(text));
                if matches!(&self.modal, Modal::Failure(form) if form.execution_id == execution_id)
                {
                    self.modal = Modal::None;
                }
            }
            Err(error) => {
                let message = match error {
                    ClientError::Refused { message, .. } => {
                        format!("The situation changed, nothing was done: {message}")
                    }
                    other => client_error_message(execution_id, other),
                };
                if let Modal::Failure(form) = &mut self.modal {
                    form.error = Some(message.clone());
                }
                self.notice = Some(Notice::error(message));
            }
        }
        self.reread_after_conductor_action().await;
    }

    /// Undoes a chosen output of the conductor's (R31, KTD13): the step is a
    /// problem again at its state. The daemon's answer says what undoing
    /// could not take back.
    pub async fn undo_decision(&mut self, execution_id: &str, decision_id: &str) {
        match self.client.undo_decision(execution_id, decision_id).await {
            Ok(outcome) => self.notice = Some(Notice::info(outcome.message)),
            Err(error) => {
                let message = match error {
                    ClientError::Refused { message, .. } => message,
                    other => client_error_message(execution_id, other),
                };
                self.notice = Some(Notice::error(message));
            }
        }
        self.reread_after_conductor_action().await;
    }

    /// Restarts the failed step `episode` names with `comment` (KTD4,
    /// KTD9): refused once the execution is no longer failed on it.
    pub async fn recover_failure(
        &mut self,
        execution_id: &str,
        episode: &str,
        comment: Option<String>,
    ) {
        let with_note = comment.is_some();
        match self
            .client
            .recover_failure(execution_id, episode, comment)
            .await
        {
            Ok(_) => {
                self.modal = Modal::None;
                self.notice = Some(Notice::info(if with_note {
                    format!(
                        "Execution {execution_id} restarts the failed step with your note, in \
                         the background."
                    )
                } else {
                    format!("Execution {execution_id} restarts the failed step in the background.")
                }));
            }
            Err(error) => {
                let message = client_error_message(execution_id, error);
                if let Modal::Failure(form) = &mut self.modal {
                    form.error = Some(message.clone());
                }
                self.notice = Some(Notice::error(message));
            }
        }
        self.reread_after_conductor_action().await;
    }

    /// Reads the list, the selection and — in the task view — its chat
    /// again, keeping the notice the action left.
    async fn reread_after_conductor_action(&mut self) {
        let notice = self.notice.clone();
        self.refresh().await;
        if matches!(self.screen, Screen::Task(_)) {
            self.refresh_task().await;
        }
        if self.notice.as_ref().is_none_or(|current| !current.is_error) {
            self.notice = notice;
        }
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// The bar's shortcuts while the input form is open: `←`/`→` means
/// something different on a key–value entry than on a choice.
fn form_shortcuts(form: &InputForm) -> Vec<(&'static str, &'static str)> {
    if form.closed.is_some() {
        return vec![("Esc", "close the form")];
    }
    let mut keys = vec![("Tab/↑↓", "row")];
    match form.current_row() {
        Some(FormRow::Entry(..)) => keys.push(("←/→", "key/value")),
        Some(FormRow::AddEntry(_)) => keys.push(("letters", "new key")),
        _ => keys.push(("←/→", "value")),
    }
    keys.push(("letters", "type"));
    keys.push(("Enter", "submit"));
    keys.push(("Esc", "cancel"));
    keys
}

/// The bar's shortcuts while the failure form is open.
fn failure_shortcuts(form: &FailureForm) -> Vec<(&'static str, &'static str)> {
    if form.closed.is_some() {
        return vec![("Esc", "close the form")];
    }
    let mut keys = vec![("Tab/↑↓", "row")];
    match form.current_row() {
        crate::tui::failure_form::FailureRow::Proposal(_) => {
            keys.push(("Enter/y", "confirm proposal"));
            keys.push(("n", "reject proposal"));
        }
        crate::tui::failure_form::FailureRow::Note => {
            keys.push(("letters", "type the note"));
            keys.push(("Enter", "restart with the note"));
        }
    }
    keys.push(("Esc", "close"));
    keys
}

/// The bar's shortcuts while the chat is open.
fn chat_shortcuts(chat: &ChatPanel) -> Vec<(&'static str, &'static str)> {
    if chat.undo.is_some() {
        return vec![("y", "undo the decision"), ("n/Esc", "keep it")];
    }
    match chat.focus {
        ChatFocus::Draft => vec![
            ("letters", "type a message"),
            ("Enter", "send"),
            ("Tab", "read history"),
            ("Esc", "close the chat"),
        ],
        ChatFocus::Reading => vec![
            ("j/k", "scroll history"),
            ("PgUp/PgDn", "page"),
            ("g/G", "top/bottom"),
            ("e", "detail"),
            ("v", "raw/rendered"),
            ("Tab", "actions"),
            ("Esc", "close the chat"),
        ],
        ChatFocus::Action => vec![
            ("[/]", "choose action"),
            ("y", "confirm"),
            ("n", "reject"),
            ("u", "undo decision"),
            ("r", "reload"),
            ("Tab", "write"),
            ("Esc", "close the chat"),
        ],
    }
}

/// What the conductor is doing, as the list and the task view say it
/// (R11, AE12).
pub fn conductor_working_text(purpose: ConductorPurpose) -> String {
    let what = match purpose {
        ConductorPurpose::ChooseOutput => "choosing the output",
        ConductorPurpose::ChooseOutputAfterReprompt => "choosing the output after a reprompt",
        ConductorPurpose::DiagnoseFailure => "diagnosing the failure",
        ConductorPurpose::ReplyToChat => "answering the chat",
        ConductorPurpose::SummarizeGate => "summarizing the gate",
    };
    format!("conductor: {what}")
}

/// Why a closed form is not sent.
fn closed_form_message(form: &InputForm) -> String {
    let closure = form.closed.as_ref().map(InputClosure::message);
    format!(
        "{} Nothing was sent; Esc closes the form.",
        closure.unwrap_or_default()
    )
}

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
        error @ (ClientError::Forbidden | ClientError::Credential(_)) => error.to_string(),
        ClientError::Unavailable(detail) => {
            format!("Could not reach the narsor daemon: {detail}")
        }
        ClientError::Request { message, .. } | ClientError::Refused { message, .. } => message,
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
///
/// A new status forgets the input and the problem the row was waiting on;
/// `ProblemRaised` sets the problem (it follows the `HumanInputRequested` of
/// a problem input, and the `WorkflowFailed` of a failure). What kind of
/// input a bare `HumanInputRequested` opened only the next refresh says, so
/// until then it counts as a question.
fn apply_to_row(row: &mut ExecutionRow, event: &Event) {
    let status = match &event.data {
        // The conductor works beside the execution (KTD3): its activity is
        // no status, and no status change ends it.
        EventData::ConductorStarted {
            trigger,
            purpose,
            episode,
            ..
        } => {
            row.conductor = Some(ConductorActivity {
                purpose: *purpose,
                trigger: *trigger,
                episode: episode.clone(),
                started_at: event.timestamp,
            });
            return;
        }
        EventData::ConductorFinished { .. } => {
            row.conductor = None;
            return;
        }
        EventData::StateEntered { state_id } => {
            row.current_state = Some(state_id.clone());
            ExecutionStatus::Running
        }
        EventData::WorkflowResumed => ExecutionStatus::Running,
        EventData::WorkflowPaused { .. } => ExecutionStatus::Paused,
        EventData::HumanInputRequested { .. } => ExecutionStatus::InputNeeded,
        EventData::WorkflowCompleted { .. } => ExecutionStatus::Completed,
        EventData::WorkflowFailed { .. } => ExecutionStatus::Failed,
        EventData::WorkflowCancelled { .. } => ExecutionStatus::Cancelled,
        EventData::HumanInputReceived { .. } => {
            row.input = None;
            if row
                .problem
                .as_ref()
                .is_some_and(|problem| problem.kind == ProblemKind::Output)
            {
                row.problem = None;
            }
            return;
        }
        EventData::ProblemRaised {
            kind,
            reason,
            episode,
            ..
        } => {
            row.problem = Some(ActiveProblem {
                kind: *kind,
                reason: reason.clone(),
                episode: episode.clone(),
            });
            if *kind == ProblemKind::Output {
                row.input = Some(InputContinuation::ResolveOutput);
            }
            return;
        }
        _ => return,
    };
    row.status = status;
    row.input = None;
    row.problem = None;
}

/// Whether a live event changes what a refresh would show (KTD16): a
/// status, a pending input, a problem — and the conductor's work: it
/// started or finished (R11), decided, or had a decision undone, which is
/// what reaches the list, the forms and the chat without `r`.
fn triggers_refresh(data: &EventData) -> bool {
    matches!(
        data,
        EventData::WorkflowResumed
            | EventData::WorkflowPaused { .. }
            | EventData::WorkflowCompleted { .. }
            | EventData::WorkflowFailed { .. }
            | EventData::WorkflowCancelled { .. }
            | EventData::HumanInputRequested { .. }
            | EventData::HumanInputReceived { .. }
            | EventData::ProblemRaised { .. }
            | EventData::ConductorStarted { .. }
            | EventData::ConductorFinished { .. }
            | EventData::ConductorDecision { .. }
            | EventData::ConductorDecisionUndone { .. }
    )
}

/// The bottom-bar notice a background run's outcome produces, if any (R4, R6).
///
/// A terminal or error event names an outcome worth interrupting the
/// operator for, and so does any task starting to wait on the operator — a
/// question or a problem (R9), for every task, not only the selected one.
/// An operator's own pause (`WorkflowPaused`) produces none.
fn notice_for(event: &Event) -> Option<Notice> {
    let execution_id = &event.workflow_execution_id;
    match &event.data {
        EventData::HumanInputRequested { instruction, .. } => Some(Notice::info(format!(
            "Execution {execution_id} waits for an answer: {} (i answers it).",
            first_line(instruction)
        ))),
        EventData::ProblemRaised {
            state_id, reason, ..
        } => Some(Notice::error(format!(
            "Execution {execution_id} has a problem in state '{state_id}': {}",
            first_line(reason)
        ))),
        EventData::WorkflowCompleted { .. } => {
            Some(Notice::info(format!("Execution {execution_id} completed.")))
        }
        // Held while the conductor may take the failure up; see
        // `App::on_engine_event`.
        EventData::WorkflowFailed { error } => Some(Notice::error(format!(
            "Execution {execution_id} failed: {error}"
        ))),
        EventData::ConductorDecision {
            kind: ConductorDecisionKind::Retry,
            state_id,
            rationale,
            ..
        } => Some(Notice::info(format!(
            "The conductor restarts the failed step '{}' of execution {execution_id} once{}",
            state_id.as_deref().unwrap_or("-"),
            rationale
                .as_deref()
                .map(|why| format!(": {}", first_line(why)))
                .unwrap_or_else(|| ".".to_string())
        ))),
        EventData::ConductorDecisionUndone {
            state_id, warning, ..
        } => Some(Notice::info(format!(
            "A decision of the conductor's on execution {execution_id} was undone: the step \
             '{state_id}' is a problem again.{}",
            warning
                .as_deref()
                .map(|warning| format!(" {warning}"))
                .unwrap_or_default()
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
        EventData::ActionOutputRejected {
            action_execution_id,
            reason,
            ..
        } => format!(
            "action_output_rejected  attempt {}: {}",
            short_id(Some(action_execution_id)),
            first_line(reason)
        ),
        EventData::OutputReprompted {
            action_execution_id,
            previous_attempt,
            by,
            ..
        } => format!(
            "output_reprompted  by {} (attempt {}, after {})",
            by.name(),
            short_id(Some(action_execution_id)),
            short_id(Some(previous_attempt))
        ),
        EventData::DecisionRequested { provider } => format!("decision_requested  {provider}"),
        EventData::DecisionMade {
            provider,
            output_label,
            ..
        } => format!("decision_made  {provider} → {output_label}"),
        EventData::HumanInputRequested { instruction, .. } => {
            format!("human_input_requested  {instruction}")
        }
        EventData::HumanInputReceived {
            answered_by,
            close_reason,
            output_label,
            ..
        } => {
            let by = answered_by.unwrap_or_default().name();
            match (close_reason, output_label) {
                (Some(reason), _) if *reason != InputCloseReason::Answered => {
                    format!("human_input_received  closed by {} ({by})", reason.name())
                }
                (_, Some(label)) => format!("human_input_received  {by} → {label}"),
                _ => format!("human_input_received  {by}"),
            }
        }
        EventData::ProblemRaised {
            state_id,
            kind,
            reason,
            ..
        } => format!(
            "problem_raised  {} in {state_id}: {}",
            kind.name(),
            first_line(reason)
        ),
        EventData::ConductorStarted {
            trigger, purpose, ..
        } => format!("conductor_started  {} ({})", purpose.name(), trigger.name()),
        EventData::ConductorFinished { outcome, error, .. } => match error {
            Some(error) => format!(
                "conductor_finished  {}: {}",
                outcome.name(),
                first_line(error)
            ),
            None => format!("conductor_finished  {}", outcome.name()),
        },
        EventData::ConductorDecision {
            kind,
            output_label,
            confidence,
            ..
        } => match output_label {
            Some(label) => format!(
                "conductor_decision  {} → {label} ({})",
                kind.name(),
                confidence.name()
            ),
            None => format!(
                "conductor_decision  {} ({})",
                kind.name(),
                confidence.name()
            ),
        },
        EventData::ConductorDecisionUndone {
            decision_id,
            state_id,
            ..
        } => format!("conductor_decision_undone  {decision_id} → {state_id}"),
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
        }
        | EventData::ActionOutputRejected {
            action_execution_id,
            ..
        }
        | EventData::OutputReprompted {
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
    entries: &[TranscriptEvent],
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

        let is_expanded = expanded.contains(&entry.entry_number);
        let technical = matches!(
            entry.kind,
            EntryKind::Thinking
                | EntryKind::ToolCall
                | EntryKind::ToolResult
                | EntryKind::System
                | EntryKind::Stderr
                | EntryKind::Raw
        );
        let sanitized = sanitize(&entry.summary);
        let body: Vec<String> = sanitized.lines().map(str::to_string).collect();
        if is_expanded || (!technical && body.len() <= TASK_ENTRY_COLLAPSE_LINES) {
            for line in &body {
                lines.push(format!("    {line}"));
            }
        } else {
            for line in body.iter().take(TASK_ENTRY_COLLAPSE_LINES) {
                lines.push(format!("    {line}"));
            }
            let hidden = body.len().saturating_sub(TASK_ENTRY_COLLAPSE_LINES);
            lines.push(if hidden == 0 {
                "    … Enter/space to expand diagnostic detail".to_string()
            } else {
                format!("    … {hidden} more lines — Enter/space to expand")
            });
        }
        if is_expanded {
            for detail in entry.detail_lines() {
                lines.push(format!("    {}", sanitize(&detail)));
            }
        }
    }
    lines
}

/// Adapts one retained agent record into the presentation model shared with
/// conductor and remote conversations. The readable projection remains the
/// default while raw mode can always return to the exact retained bytes.
pub(crate) fn agent_transcript_entry(entry: &TranscriptEvent) -> TranscriptEntry {
    let header = format!(
        "[{}] {}  {}  ({:?})",
        entry.entry_number,
        entry.timestamp.format("%H:%M:%S"),
        entry.kind.label(),
        entry.source
    );
    let body: Vec<String> = sanitize(&entry.summary)
        .lines()
        .map(str::to_owned)
        .collect();
    let mut rendered_lines = vec![header];
    rendered_lines.extend(body);
    let presentation = match entry.kind {
        EntryKind::User => TranscriptPresentation::Operator,
        EntryKind::Assistant | EntryKind::Result => TranscriptPresentation::Agent,
        EntryKind::Stderr => TranscriptPresentation::Error,
        EntryKind::Narsor
        | EntryKind::Thinking
        | EntryKind::ToolCall
        | EntryKind::ToolResult
        | EntryKind::System
        | EntryKind::Raw => TranscriptPresentation::Technical,
    };
    TranscriptEntry::new(
        TranscriptEntryId::from(entry.entry_number.to_string()),
        rendered_lines,
        entry.raw.clone(),
    )
    .with_details(entry.details.clone())
    .with_presentation(presentation)
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
