//! The task list: what it holds, how events patch it, how a row looks.
//!
//! The list is the queue (D-T3). The server hands the tasks over in queue
//! order — grouped by backend, in `rank` order within each — and **the client
//! never sorts** (KTD "Kolejność listy liczy serwer"): a lifecycle event
//! changes a row's icon and label in place, and only a [`TaskEventKind::Moved`]
//! event, which carries the backend's whole new order, reorders rows. State is
//! an attribute of a row, never a reason for it to move (D-T2, D-T5).
//!
//! Three things make that safe:
//!
//! * the selection is a `task_id`, not an index, so a patch, a refresh or a
//!   move never changes *which* task is selected ([`TaskList::selected_id`]);
//! * an event older than the revision the row already shows is dropped, so a
//!   refresh that overtook the stream is not undone by what the stream
//!   delivers late;
//! * hiding finished tasks (`z`) is a filter over the same order
//!   ([`TaskList::visible`]), not a regrouping.
//!
//! # A row
//!
//! ```text
//! ? ●  Refactor parser       attention · state · title
//!      approval              label, under the title
//! ```
//!
//! A row is two lines, so that the title has the whole width (R7). The first
//! column says what the operator must do (`?` answer, `!` problem — a problem
//! wins over a question), the second what is happening (`●` running, `◷`
//! queued, `⟳` provisioning, `‖` paused, `✗` failed, `✓` done), then the title
//! (`display.title`, else the task id); the second line holds a short label:
//! the current step, `#n` for a queued task, `offline` when no controller holds
//! the backend. Everything that comes from the server is passed through
//! [`one_line`] before it is drawn.
//!
//! The rows are drawn as a bracketed list ([`render_bracketed_list`]): tasks
//! stacked directly, and a bracket — full-width rules above and below, a bar
//! beside both lines — around the selected one instead of reverse video, so
//! its markers keep their colours. A task takes two rows of the column, the
//! selected one four.

use std::collections::HashMap;

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::Frame;

use crate::cli::task_text::lifecycle_name;
use crate::controller_protocol::{
    Freshness, InputView, OperatorCommandView, QueueView, TaskSummary,
};
use crate::domain::{BackendId, ExecutionStatus};
use narsor_protocol::controller::{
    AttentionItem, AttentionKind, MoveTaskTarget, OperatorCommand, OperatorCommandStatus,
    QueueReason, ResumeExhaustion, TaskEvent, TaskEventKind, TaskLifecycle,
};
use narsor_protocol::display::{exhaustion_loss, loss_state, LossState, UNSPECIFIED_LOSS};

use super::super::scroll::one_line;
use super::super::selection::{render_bracketed_list, PREFIX_WIDTH};
use super::super::style::{Role, Styles};

/// What a row asks of the operator (column 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attention {
    /// Nothing.
    None,
    /// A question, an approval or a conductor proposal: `?`.
    Question,
    /// A problem, a failed step or a failed provisioning: `!`.
    Problem,
}

impl Attention {
    /// The glyph of the first column.
    pub fn glyph(self) -> char {
        match self {
            Self::None => ' ',
            Self::Question => '?',
            Self::Problem => '!',
        }
    }

    fn role(self) -> Role {
        match self {
            Self::None => Role::Calm,
            Self::Question => Role::Attention,
            Self::Problem => Role::Problem,
        }
    }
}

/// What is happening to a row's task (column 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateIcon {
    /// `●` the runner works.
    Running,
    /// `◷` waits in the queue.
    Queued,
    /// `⟳` the backend prepares it.
    Provisioning,
    /// `‖` paused or pausing.
    Paused,
    /// `✗` failed, interrupted or unknown.
    Failed,
    /// `✓` finished.
    Done,
    /// `↻` a new task being sent (the new-task form).
    Sending,
}

impl StateIcon {
    /// The glyph of the second column.
    pub fn glyph(self) -> char {
        match self {
            Self::Running => '●',
            Self::Queued => '◷',
            Self::Provisioning => '⟳',
            Self::Paused => '‖',
            Self::Failed => '✗',
            Self::Done => '✓',
            Self::Sending => '↻',
        }
    }

    /// The role the icon is drawn in.
    pub fn role(self) -> Role {
        match self {
            Self::Running | Self::Provisioning => Role::Live,
            Self::Queued | Self::Paused | Self::Sending => Role::Calm,
            Self::Failed => Role::Problem,
            Self::Done => Role::Done,
        }
    }
}

/// A task's loss in words, for the details.
pub fn describe_loss(loss: &LossState) -> String {
    match loss {
        LossState::AutoResume { attempt, limit } => {
            format!("wznowienie automatyczne, próba {attempt}/{limit}")
        }
        LossState::Exhausted(reason) => format!(
            "nie udało się wznowić automatycznie: {}, uruchom od nowa (s)",
            exhaustion_detail(reason)
        ),
        LossState::LostWhilePausing => {
            "utracone podczas wstrzymywania, czeka na Ciebie (s)".to_string()
        }
    }
}

/// Why the resume policy gave up, from its reason for people: the loss
/// after the server's English prefix, a loss the controller did not
/// describe in words of its own.
pub fn exhaustion_detail(reason: &str) -> String {
    let loss = exhaustion_loss(reason);
    if loss == UNSPECIFIED_LOSS {
        "runner utracony".to_string()
    } else {
        one_line(loss)
    }
}

/// What the first column shows for `task`. A problem has precedence over a
/// question (origin §5).
pub fn attention_of(task: &TaskSummary) -> Attention {
    let is = |kinds: &[AttentionKind]| task.attention.iter().any(|item| kinds.contains(&item.kind));
    let runner_problem = task
        .runner
        .as_ref()
        .is_some_and(|runner| runner.problem.is_some());
    let owner_decides =
        loss_state(task.lifecycle, &task.resume).is_some_and(|loss| loss.needs_owner());
    if runner_problem
        || owner_decides
        || is(&[
            AttentionKind::Problem,
            AttentionKind::FailedStep,
            AttentionKind::ProvisioningFailed,
        ])
    {
        Attention::Problem
    } else if is(&[
        AttentionKind::Question,
        AttentionKind::Approval,
        AttentionKind::ConductorProposal,
    ]) {
        Attention::Question
    } else {
        Attention::None
    }
}

/// What the second column shows for `task`.
pub fn state_icon(task: &TaskSummary) -> StateIcon {
    match task.lifecycle {
        TaskLifecycle::Queued => StateIcon::Queued,
        TaskLifecycle::Provisioning => StateIcon::Provisioning,
        TaskLifecycle::Running => {
            match task.runner.as_ref().map(|runner| runner.execution_status) {
                Some(ExecutionStatus::Completed) => StateIcon::Done,
                Some(ExecutionStatus::Failed | ExecutionStatus::Cancelled) => StateIcon::Failed,
                _ => StateIcon::Running,
            }
        }
        TaskLifecycle::PauseRequested | TaskLifecycle::Paused => StateIcon::Paused,
        TaskLifecycle::Interrupted | TaskLifecycle::Unknown => StateIcon::Failed,
        TaskLifecycle::Archiving | TaskLifecycle::Archived => StateIcon::Done,
    }
}

/// The title of a row: `display.title`, or the task id when there is none.
pub fn title_of(task: &TaskSummary) -> String {
    let title = one_line(&task.display.title);
    if title.trim().is_empty() {
        task.task_id.to_string()
    } else {
        title
    }
}

/// Whether the task's backend has no controller holding its lease right now:
/// a task that is expected to be worked on is not being.
fn controller_offline(task: &TaskSummary) -> bool {
    task.controller
        .as_ref()
        .is_none_or(|controller| controller.freshness != Freshness::Live)
}

/// The label of a row (its second line). `queue_position` is the 1-based place among
/// the queued tasks of the backend, in list order.
pub fn label_of(task: &TaskSummary, queue_position: Option<usize>) -> String {
    let runner_status = task.runner.as_ref().map(|runner| runner.execution_status);
    match task.lifecycle {
        TaskLifecycle::Queued => match task.queue.as_ref().map(|queue| &queue.reason) {
            Some(QueueReason::ControllerOffline) => "offline".to_string(),
            _ => queue_position.map_or_else(|| "queued".to_string(), |place| format!("#{place}")),
        },
        TaskLifecycle::Provisioning | TaskLifecycle::Running => match runner_status {
            Some(ExecutionStatus::Completed) => "done".to_string(),
            Some(ExecutionStatus::Failed) => "failed".to_string(),
            Some(ExecutionStatus::Cancelled) => "cancelled".to_string(),
            _ if controller_offline(task) => "offline".to_string(),
            _ => current_step(task).unwrap_or_else(|| lifecycle_name(task.lifecycle).to_string()),
        },
        TaskLifecycle::Archiving | TaskLifecycle::Archived => "done".to_string(),
        other => lifecycle_name(other).to_string(),
    }
}

/// The workflow state the runner reports being in.
fn current_step(task: &TaskSummary) -> Option<String> {
    let step = task
        .runner
        .as_ref()?
        .execution_cursor
        .current_state
        .as_deref()?;
    let step = one_line(step);
    (!step.trim().is_empty()).then_some(step)
}

/// A task as the list shows it, for tests and the details placeholder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowText {
    /// Column 1.
    pub attention: Attention,
    /// Column 2.
    pub icon: StateIcon,
    /// The title, first line.
    pub title: String,
    /// The label, second line.
    pub label: String,
}

/// The header counters: how many tasks ask a question, how many have a
/// problem (a task counts once, under the marker its row carries).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// `? n`
    pub questions: usize,
    /// `! n`
    pub problems: usize,
}

/// Slot occupancy of a backend, for the header (`sloty 2/3`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slots {
    /// Slots in use.
    pub used: usize,
    /// The backend's limit, known only while a task waits for capacity: the
    /// scheduler's verdict is the one place the control plane tells it.
    pub limit: Option<usize>,
}

/// A place in the queue chosen by the move mode (`m`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveMode {
    task_id: String,
    backend: BackendId,
    /// Where the task sits among the other visible rows of its backend.
    origin: usize,
    /// Where the preview puts it.
    slot: usize,
}

impl MoveMode {
    /// The task being moved.
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    /// The previewed place, 0-based among the backend's visible rows.
    pub fn slot(&self) -> usize {
        self.slot
    }

    /// Whether the preview is where the task already is.
    pub fn is_unchanged(&self) -> bool {
        self.slot == self.origin
    }
}

/// A task that left the list, and where the selection went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removal {
    /// The task that left.
    pub task_id: String,
    /// What its row showed.
    pub title: String,
    /// The task now selected, if any is left.
    pub now_selected: Option<String>,
}

/// What an event did to the list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EventEffect {
    /// The list could not be patched completely: a read is wanted.
    pub refresh: bool,
    /// The event applied to the list, as opposed to being older than it.
    pub applied: bool,
    /// The selected task left the list.
    pub removal: Option<Removal>,
    /// The runner settled a command of the operator.
    pub resolved: Option<CommandResolved>,
    /// The resume policy resumed the task or gave up on it.
    pub resume: Option<ResumeNotice>,
}

/// What the resume policy did to a task, as an event said it (R7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeNotice {
    /// The task.
    pub task_id: String,
    /// What its row shows.
    pub title: String,
    /// What happened.
    pub kind: ResumeNoticeKind,
}

/// See [`ResumeNotice`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeNoticeKind {
    /// Resumed: attempt `attempt` of `limit`.
    Resumed {
        /// The consecutive attempt, from 1; 0 after a controller restart
        /// the policy did not count.
        attempt: u32,
        /// How many the policy makes.
        limit: u32,
    },
    /// Gave up: why, without the server's English prefix.
    Exhausted(String),
}

/// An operator command the runner settled (or the control plane made
/// obsolete), as the event said it, with what the row knew of the command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResolved {
    /// The task.
    pub task_id: String,
    /// What its row shows.
    pub title: String,
    /// What was asked, when the row knew the command.
    pub command: Option<OperatorCommand>,
    /// How it ended.
    pub status: OperatorCommandStatus,
    /// Why it was rejected.
    pub detail: Option<String>,
}

/// A move the operator asked for, ready to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveRequest {
    /// The task to move.
    pub task_id: String,
    /// What the row shows, for the notification.
    pub title: String,
    /// The desired generation the operator saw.
    pub generation: u64,
    /// Where to put it.
    pub target: MoveTaskTarget,
    /// What the anchor row shows, for the notification.
    pub anchor_title: String,
}

/// Where a task the operator created stands while the server has not said it
/// has it (the new-task form, origin §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingState {
    /// The request is on its way, or its answer was lost and it is being sent
    /// again with the same id: `↻ wysyłanie…`.
    Sending,
    /// The retry failed too: `✗ nie wysłano`, to be sent again by hand.
    NotSent,
}

/// A row for a task that was created here and is not on the server's list
/// yet. It sits after the server's rows and holds the id the task will have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRow {
    /// The id the task is created with (and is sent again with).
    pub task_id: String,
    /// What the row shows.
    pub title: String,
    /// Where it stands.
    pub state: PendingState,
    /// Why it was not sent, once it was not.
    pub reason: Option<String>,
}

impl PendingRow {
    /// What the row shows.
    pub fn text(&self) -> RowText {
        let (icon, label) = match self.state {
            PendingState::Sending => (StateIcon::Sending, "wysyłanie…"),
            PendingState::NotSent => (StateIcon::Failed, "nie wysłano"),
        };
        RowText {
            attention: Attention::None,
            icon,
            title: one_line(&self.title),
            label: label.to_string(),
        }
    }
}

/// The tasks, the selection and the filter.
#[derive(Debug)]
pub struct TaskList {
    tasks: Vec<TaskSummary>,
    /// Tasks created here that the server has not confirmed; shown last.
    pending: Vec<PendingRow>,
    selected: Option<String>,
    hide_done: bool,
    moving: Option<MoveMode>,
    /// The first row in view, kept between frames so that a refresh does not
    /// scroll the list.
    top: usize,
}

impl Default for TaskList {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskList {
    /// An empty list.
    pub fn new() -> Self {
        Self {
            tasks: Vec::new(),
            pending: Vec::new(),
            selected: None,
            hide_done: false,
            moving: None,
            top: 0,
        }
    }

    /// Every task, in the server's order, finished ones included.
    pub fn tasks(&self) -> &[TaskSummary] {
        &self.tasks
    }

    /// The move mode, while it is on.
    pub fn moving(&self) -> Option<&MoveMode> {
        self.moving.as_ref()
    }

    /// The selected task's id.
    pub fn selected_id(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// The selected task.
    pub fn selected(&self) -> Option<&TaskSummary> {
        let id = self.selected.as_deref()?;
        self.tasks.iter().find(|task| task.task_id.as_str() == id)
    }

    /// Selects the task with `task_id`, when it is on the list and shown.
    pub fn select(&mut self, task_id: &str) -> bool {
        let shown = self.visible_ids().iter().any(|id| id == task_id);
        if shown {
            self.selected = Some(task_id.to_string());
        }
        shown
    }

    /// Indices into [`Self::tasks`] of the rows shown, top to bottom: the
    /// server's order minus the hidden finished tasks, and — while the move
    /// mode previews a place — with the moved row at that place.
    pub fn visible(&self) -> Vec<usize> {
        let mut shown: Vec<usize> = self
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, task)| !(self.hide_done && state_icon(task) == StateIcon::Done))
            .map(|(index, _)| index)
            .collect();
        let Some(mode) = &self.moving else {
            return shown;
        };
        let group: Vec<usize> = shown
            .iter()
            .copied()
            .filter(|&index| self.tasks[index].backend_id == mode.backend)
            .collect();
        let Some(moved) = group
            .iter()
            .copied()
            .find(|&index| self.tasks[index].task_id.as_str() == mode.task_id)
        else {
            return shown;
        };
        let mut reordered: Vec<usize> = group.iter().copied().filter(|&i| i != moved).collect();
        reordered.insert(mode.slot.min(reordered.len()), moved);
        let mut next = reordered.into_iter();
        for index in &mut shown {
            if self.tasks[*index].backend_id == mode.backend {
                *index = next.next().expect("as many rows as the group had");
            }
        }
        shown
    }

    /// The ids of the rows shown, top to bottom: the tasks, then the ones
    /// created here that the server has not confirmed.
    pub fn visible_ids(&self) -> Vec<String> {
        self.visible()
            .into_iter()
            .map(|index| self.tasks[index].task_id.to_string())
            .chain(self.pending.iter().map(|row| row.task_id.clone()))
            .collect()
    }

    /// How many finished tasks the filter hides.
    pub fn hidden_count(&self) -> usize {
        if !self.hide_done {
            return 0;
        }
        self.tasks
            .iter()
            .filter(|task| state_icon(task) == StateIcon::Done)
            .count()
    }

    /// `n`th queued task of its backend, counted in list order, for each
    /// queued task. Hidden or not: only finished tasks hide, and they are not
    /// queued.
    fn queue_positions(&self) -> HashMap<usize, usize> {
        let mut seen: HashMap<&BackendId, usize> = HashMap::new();
        let mut positions = HashMap::new();
        for (index, task) in self.tasks.iter().enumerate() {
            if task.lifecycle == TaskLifecycle::Queued {
                let place = seen.entry(&task.backend_id).or_insert(0);
                *place += 1;
                positions.insert(index, *place);
            }
        }
        positions
    }

    /// The place of `task_id` in the queue of its backend (1-based, in list
    /// order), when it is queued.
    pub fn queue_position(&self, task_id: &str) -> Option<usize> {
        let index = self
            .tasks
            .iter()
            .position(|task| task.task_id.as_str() == task_id)?;
        self.queue_positions().get(&index).copied()
    }

    /// What the row of `tasks[index]` shows.
    pub fn row_text(&self, index: usize) -> RowText {
        self.row_text_with(index, &self.queue_positions())
    }

    fn row_text_with(&self, index: usize, positions: &HashMap<usize, usize>) -> RowText {
        let task = &self.tasks[index];
        RowText {
            attention: attention_of(task),
            icon: state_icon(task),
            title: title_of(task),
            label: label_of(task, positions.get(&index).copied()),
        }
    }

    /// The rows shown, as text, top to bottom.
    pub fn rows(&self) -> Vec<RowText> {
        let positions = self.queue_positions();
        self.visible()
            .into_iter()
            .map(|index| self.row_text_with(index, &positions))
            .chain(self.pending.iter().map(PendingRow::text))
            .collect()
    }

    // -----------------------------------------------------------------
    // Tasks created here
    // -----------------------------------------------------------------

    /// The tasks created here that the server has not confirmed.
    pub fn pending(&self) -> &[PendingRow] {
        &self.pending
    }

    /// The selected row, when it is one of those.
    pub fn selected_pending(&self) -> Option<&PendingRow> {
        let id = self.selected.as_deref()?;
        self.pending.iter().find(|row| row.task_id == id)
    }

    /// Adds `row` at the end and selects it.
    pub fn push_pending(&mut self, row: PendingRow) {
        let id = row.task_id.clone();
        self.pending.retain(|known| known.task_id != id);
        self.pending.push(row);
        self.selected = Some(id);
    }

    /// Changes where a pending row stands. Returns whether there is one.
    pub fn set_pending(
        &mut self,
        task_id: &str,
        state: PendingState,
        reason: Option<String>,
    ) -> bool {
        let Some(row) = self.pending.iter_mut().find(|row| row.task_id == task_id) else {
            return false;
        };
        row.state = state;
        row.reason = reason;
        true
    }

    /// Forgets a pending row. A selection on it goes to the row above.
    pub fn remove_pending(&mut self, task_id: &str) {
        let before = self.all_ids();
        self.pending.retain(|row| row.task_id != task_id);
        self.settle_selection_with(&before, None);
    }

    /// Puts a task the server confirmed at the end of the list and selects it.
    /// The pending row of the same id, if any, becomes it, so the rows above
    /// stay where they are. A task the list already holds is replaced.
    pub fn append(&mut self, task: TaskSummary) {
        let id = task.task_id.to_string();
        self.pending.retain(|row| row.task_id != id);
        match self
            .tasks
            .iter_mut()
            .find(|known| known.task_id == task.task_id)
        {
            Some(known) => *known = task,
            None => self.tasks.push(task),
        }
        self.selected = Some(id);
    }

    /// The place a task created now would take in the queue: after every task
    /// that waits and every one being sent.
    pub fn next_queue_place(&self) -> usize {
        let queued = self
            .tasks
            .iter()
            .filter(|task| task.lifecycle == TaskLifecycle::Queued)
            .count();
        queued + self.pending.len() + 1
    }

    /// The counters of the header.
    pub fn counters(&self) -> Counters {
        self.tasks
            .iter()
            .fold(Counters::default(), |mut counters, task| {
                match attention_of(task) {
                    Attention::Question => counters.questions += 1,
                    Attention::Problem => counters.problems += 1,
                    Attention::None => {}
                }
                counters
            })
    }

    /// Slot occupancy of the selected task's backend (else the first one).
    pub fn slots(&self) -> Option<Slots> {
        let backend = self
            .selected()
            .or_else(|| self.tasks.first())
            .map(|task| &task.backend_id)?;
        let mut held = 0;
        for task in self.tasks.iter().filter(|task| task.backend_id == *backend) {
            if let Some(QueueView {
                reason: QueueReason::WaitingForCapacity { running, limit, .. },
                ..
            }) = &task.queue
            {
                return Some(Slots {
                    used: *running as usize,
                    limit: Some(*limit as usize),
                });
            }
            held += usize::from(holds_a_slot(task));
        }
        Some(Slots {
            used: held,
            limit: None,
        })
    }

    // -----------------------------------------------------------------
    // Cursor
    // -----------------------------------------------------------------

    /// Moves the cursor `by` rows (clamped at the ends).
    pub fn step(&mut self, by: isize) {
        let shown = self.visible_ids();
        let Some(current) = self
            .selected
            .as_ref()
            .and_then(|id| shown.iter().position(|shown| shown == id))
        else {
            self.selected = shown.first().cloned();
            return;
        };
        let target = current
            .saturating_add_signed(by)
            .min(shown.len().saturating_sub(1));
        self.selected = shown.get(target).cloned();
    }

    /// Cursor to the first row.
    pub fn first(&mut self) {
        self.selected = self.visible_ids().first().cloned();
    }

    /// Cursor to the last row.
    pub fn last(&mut self) {
        self.selected = self.visible_ids().last().cloned();
    }

    /// Selects the next task that waits for the operator, after the selected
    /// row and wrapping round. Returns whether one was found; the rows stay
    /// where they are.
    pub fn select_next_waiting(&mut self) -> bool {
        let shown = self.visible();
        if shown.is_empty() {
            return false;
        }
        let start = self
            .selected
            .as_ref()
            .and_then(|id| {
                shown
                    .iter()
                    .position(|&index| self.tasks[index].task_id.as_str() == id)
            })
            .map_or(0, |current| current + 1);
        let found = (0..shown.len())
            .map(|offset| shown[(start + offset) % shown.len()])
            .find(|&index| attention_of(&self.tasks[index]) != Attention::None);
        match found {
            Some(index) => {
                self.selected = Some(self.tasks[index].task_id.to_string());
                true
            }
            None => false,
        }
    }

    /// Toggles the finished-tasks filter. The order of the rest is untouched;
    /// a selection that becomes hidden moves to the nearest shown row.
    pub fn toggle_hide_done(&mut self) {
        let before = self.all_ids();
        self.hide_done = !self.hide_done;
        self.settle_selection(&before);
    }

    // -----------------------------------------------------------------
    // Loading and patching
    // -----------------------------------------------------------------

    fn all_ids(&self) -> Vec<String> {
        self.tasks
            .iter()
            .map(|task| task.task_id.to_string())
            .chain(self.pending.iter().map(|row| row.task_id.clone()))
            .collect()
    }

    /// Replaces the tasks with a fresh read. The selection stays on its task;
    /// when the task is gone it goes to the row that was under it.
    pub fn replace(&mut self, tasks: Vec<TaskSummary>) -> Option<Removal> {
        let before = self.all_ids();
        let departed_title = self.selected().map(title_of);
        // What the server lists is no longer waiting to be confirmed.
        self.pending.retain(|row| {
            tasks
                .iter()
                .all(|task| task.task_id.as_str() != row.task_id)
        });
        self.tasks = tasks;
        self.cancel_move_if_gone();
        self.settle_selection_with(&before, departed_title)
    }

    fn settle_selection(&mut self, before: &[String]) -> Option<Removal> {
        let title = self.selected().map(title_of);
        self.settle_selection_with(before, title)
    }

    /// Makes the selection valid again after tasks left or were hidden.
    /// `before` is the order of every task before the change; the replacement
    /// is the first row that was *below* the selected one and is still shown,
    /// else the nearest above, else the first row.
    fn settle_selection_with(
        &mut self,
        before: &[String],
        departed_title: Option<String>,
    ) -> Option<Removal> {
        let shown = self.visible_ids();
        let Some(current) = self.selected.clone() else {
            self.selected = shown.first().cloned();
            return None;
        };
        if shown.contains(&current) {
            return None;
        }
        let gone = !self
            .tasks
            .iter()
            .any(|task| task.task_id.as_str() == current);
        let position = before.iter().position(|id| *id == current);
        let below = position
            .into_iter()
            .flat_map(|position| before[position + 1..].iter())
            .find(|id| shown.contains(id));
        let above = position
            .into_iter()
            .flat_map(|position| before[..position].iter().rev())
            .find(|id| shown.contains(id));
        self.selected = below.or(above).or_else(|| shown.first()).cloned();
        gone.then(|| Removal {
            task_id: current,
            title: departed_title.unwrap_or_default(),
            now_selected: self.selected.clone(),
        })
    }

    /// Applies one stream event to the row it names, at once and in place.
    ///
    /// * lifecycle, generation and revision are always patched — that is what
    ///   the stream is for;
    /// * [`TaskEventKind::QueueChanged`] replaces the scheduler verdict;
    /// * [`TaskEventKind::Moved`] reorders the task's backend to the order it
    ///   carries and renumbers its rows;
    /// * [`TaskEventKind::InputQueued`] and [`TaskEventKind::RunnerReported`]
    ///   patch the attention of the row (and the runner's status), so a
    ///   question or an answer shows without a read;
    /// * [`TaskEventKind::AutoResumed`] and
    ///   [`TaskEventKind::AutoResumeExhausted`] say so in
    ///   [`EventEffect::resume`] for a notification — the row keeps its
    ///   place, only its marker changes — and ask for a read;
    /// * everything else — and anything the event cannot say, such as the
    ///   runner's current step or a task this list does not know — asks for a
    ///   read, which the caller debounces.
    ///
    /// An event that is not newer than the row's revision is dropped — except
    /// a `Moved` at exactly that revision: a read that came in between the
    /// renumbering of the neighbours and the commit shows the moved row at
    /// the event's revision with the neighbours still in the old order, and
    /// only the event's order puts them right.
    pub fn apply_event(&mut self, event: &TaskEvent) -> EventEffect {
        let Some(index) = self
            .tasks
            .iter()
            .position(|task| task.task_id == event.task_id)
        else {
            return EventEffect {
                refresh: true,
                ..EventEffect::default()
            };
        };
        let revision = self.tasks[index].revision;
        if event.sequence < revision {
            return EventEffect::default();
        }
        if event.sequence == revision {
            if let TaskEventKind::Moved { order, .. } = &event.kind {
                self.apply_order(index, order);
                return EventEffect {
                    applied: true,
                    ..EventEffect::default()
                };
            }
            return EventEffect::default();
        }
        {
            let task = &mut self.tasks[index];
            task.revision = event.sequence;
            task.lifecycle = event.lifecycle;
            task.generation = event.generation;
            if task.lifecycle != TaskLifecycle::Queued {
                task.queue = None;
            }
        }
        let mut effect = EventEffect {
            applied: true,
            ..EventEffect::default()
        };

        if event.lifecycle.is_archival() {
            let before = self.all_ids();
            let departed_title = self.selected().map(title_of);
            self.tasks.remove(index);
            self.cancel_move_if_gone();
            effect.removal = self.settle_selection_with(&before, departed_title);
            return effect;
        }

        match &event.kind {
            TaskEventKind::Moved { order, .. } => self.apply_order(index, order),
            kind @ (TaskEventKind::OperatorCommandQueued { .. }
            | TaskEventKind::OperatorCommandResolved { .. }) => {
                let (refresh, resolved) =
                    patch_operator_command(&mut self.tasks[index], kind, event);
                effect.refresh = refresh;
                effect.resolved = resolved;
            }
            TaskEventKind::AutoResumed { attempt, limit, .. } => {
                let task = &mut self.tasks[index];
                task.resume.next_attempt_at = None;
                effect.refresh = true;
                effect.resume = Some(ResumeNotice {
                    task_id: task.task_id.to_string(),
                    title: title_of(task),
                    kind: ResumeNoticeKind::Resumed {
                        attempt: *attempt,
                        limit: *limit,
                    },
                });
            }
            TaskEventKind::AutoResumeExhausted { reason, .. } => {
                // The marker shows at once; the read brings the rest.
                let task = &mut self.tasks[index];
                task.resume.next_attempt_at = None;
                task.resume.exhausted = Some(ResumeExhaustion {
                    reason: reason.clone(),
                    at: event.at,
                    announced: true,
                });
                effect.refresh = true;
                effect.resume = Some(ResumeNotice {
                    task_id: task.task_id.to_string(),
                    title: title_of(task),
                    kind: ResumeNoticeKind::Exhausted(exhaustion_detail(reason)),
                });
            }
            kind => effect.refresh = patch_row(&mut self.tasks[index], kind, event),
        }
        effect
    }

    /// Records a command the server accepted in the row it belongs to, unless
    /// the row already has it (the stream's `OperatorCommandQueued` may have
    /// come first), so that its verdict can be told by what it was.
    pub fn note_operator_command(&mut self, task_id: &str, command: OperatorCommandView) {
        let Some(task) = self
            .tasks
            .iter_mut()
            .find(|task| task.task_id.as_str() == task_id)
        else {
            return;
        };
        if task
            .operator_commands
            .iter()
            .all(|known| known.command_id != command.command_id)
        {
            task.operator_commands.push(command);
        }
    }

    /// Reorders the rows of the backend of `tasks[index]` to `order` and
    /// numbers them by it. The rows keep to the places the backend's rows
    /// already occupy; a task `order` does not name (created since) goes last.
    fn apply_order(&mut self, index: usize, order: &[crate::domain::TaskId]) {
        let backend = self.tasks[index].backend_id.clone();
        let place: HashMap<&crate::domain::TaskId, usize> =
            order.iter().enumerate().map(|(at, id)| (id, at)).collect();
        let mut group = Vec::new();
        let mut layout: Vec<Option<TaskSummary>> = Vec::with_capacity(self.tasks.len());
        for task in std::mem::take(&mut self.tasks) {
            if task.backend_id == backend {
                group.push(task);
                layout.push(None);
            } else {
                layout.push(Some(task));
            }
        }
        group.sort_by_cached_key(|task| place.get(&task.task_id).copied().unwrap_or(usize::MAX));
        for task in &mut group {
            if let Some(&position) = place.get(&task.task_id) {
                task.rank = u32::try_from(position + 1).unwrap_or(u32::MAX);
            }
        }
        let mut ordered = group.into_iter();
        self.tasks = layout
            .into_iter()
            .filter_map(|kept| kept.or_else(|| ordered.next()))
            .collect();
    }

    // -----------------------------------------------------------------
    // Moving
    // -----------------------------------------------------------------

    /// Where the selected task sits among the shown rows of its backend, and
    /// those rows' indices, the task itself left out.
    fn group_without_selected(&self) -> Option<(usize, Vec<usize>)> {
        let id = self.selected.as_deref()?;
        let task = self.selected()?;
        let group: Vec<usize> = self
            .visible()
            .into_iter()
            .filter(|&index| self.tasks[index].backend_id == task.backend_id)
            .collect();
        let origin = group
            .iter()
            .position(|&index| self.tasks[index].task_id.as_str() == id)?;
        let rest = group
            .into_iter()
            .filter(|&index| self.tasks[index].task_id.as_str() != id);
        Some((origin, rest.collect()))
    }

    /// The row next to the selected one, in its backend: `up` is the one
    /// above, otherwise the one below.
    pub fn neighbour(&self, up: bool) -> Option<&TaskSummary> {
        let (origin, rest) = self.group_without_selected()?;
        let at = if up { origin.checked_sub(1)? } else { origin };
        rest.get(at).map(|&index| &self.tasks[index])
    }

    /// Enters the move mode on the selected task.
    pub fn start_move(&mut self) -> bool {
        let Some((origin, _)) = self.group_without_selected() else {
            return false;
        };
        let Some(task) = self.selected() else {
            return false;
        };
        self.moving = Some(MoveMode {
            task_id: task.task_id.to_string(),
            backend: task.backend_id.clone(),
            origin,
            slot: origin,
        });
        true
    }

    /// Previews the place `by` rows further, within the backend's rows.
    pub fn step_move(&mut self, by: isize) {
        let limit = self
            .group_without_selected()
            .map_or(0, |(_, rest)| rest.len());
        if let Some(mode) = &mut self.moving {
            mode.slot = mode.slot.saturating_add_signed(by).min(limit);
        }
    }

    /// Leaves the move mode without moving anything.
    pub fn cancel_move(&mut self) {
        self.moving = None;
    }

    /// Drops the move mode when the task it moves is gone or no longer queued.
    fn cancel_move_if_gone(&mut self) {
        let still_movable = self.moving.as_ref().is_none_or(|mode| {
            self.tasks.iter().any(|task| {
                task.task_id.as_str() == mode.task_id && task.lifecycle == TaskLifecycle::Queued
            })
        });
        if !still_movable {
            self.moving = None;
        }
    }

    /// Ends the move mode and says what to send for the previewed place, or
    /// `None` when the preview is where the task already was.
    ///
    /// A task moved down goes *after* the row above the place it lands on, one
    /// moved up goes *before* the row that place is taken from.
    pub fn finish_move(&mut self) -> Option<MoveRequest> {
        let mode = self.moving.take()?;
        if mode.is_unchanged() {
            return None;
        }
        self.selected = Some(mode.task_id.clone());
        let (origin, rest) = self.group_without_selected()?;
        let task = self.selected()?;
        let (target, anchor) = if mode.slot > origin {
            let anchor = &self.tasks[*rest.get(mode.slot - 1)?];
            (MoveTaskTarget::After(anchor.task_id.to_string()), anchor)
        } else {
            let anchor = &self.tasks[*rest.get(mode.slot)?];
            (MoveTaskTarget::Before(anchor.task_id.to_string()), anchor)
        };
        Some(MoveRequest {
            task_id: task.task_id.to_string(),
            title: title_of(task),
            generation: task.generation,
            target,
            anchor_title: title_of(anchor),
        })
    }

    // -----------------------------------------------------------------
    // Drawing
    // -----------------------------------------------------------------

    /// Draws the list into `area`: a column titled `Zadania`, one row per
    /// task, stacked, with a bracket around the selected one
    /// ([`render_bracketed_list`]). `focused` lights the frame and the
    /// bracket; `stale` greys every row (no connection: the rows are a
    /// memory, not news).
    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        styles: &Styles,
        focused: bool,
        stale: bool,
    ) {
        let title = match self.hidden_count() {
            0 => " Zadania ".to_string(),
            hidden => format!(" Zadania · ✓ ukryte {hidden} "),
        };
        let inner = styles.render_panel(frame, area, title, focused);

        let shown = self.visible();
        let positions = self.queue_positions();
        let moving = self.moving.clone();
        let group_len = moving.as_ref().map(|mode| {
            shown
                .iter()
                .filter(|&&index| self.tasks[index].backend_id == mode.backend)
                .count()
        });
        let rows: Vec<(String, RowText)> = shown
            .iter()
            .map(|&index| {
                let task = &self.tasks[index];
                let mut text = RowText {
                    attention: attention_of(task),
                    icon: state_icon(task),
                    title: title_of(task),
                    label: label_of(task, positions.get(&index).copied()),
                };
                if let (Some(mode), Some(len)) = (&moving, group_len) {
                    if mode.task_id == task.task_id.as_str() {
                        text.label = format!("↕ {}/{len}", mode.slot + 1);
                    }
                }
                (task.task_id.to_string(), text)
            })
            .collect();
        let rows: Vec<(String, RowText)> = rows
            .into_iter()
            .chain(
                self.pending
                    .iter()
                    .map(|row| (row.task_id.clone(), row.text())),
            )
            .collect();
        let width = usize::from(inner.width.saturating_sub(PREFIX_WIDTH));
        let items: Vec<Vec<Line>> = rows
            .iter()
            .map(|(_, row)| row_text(row, width, styles, stale))
            .collect();
        let selected = rows
            .iter()
            .position(|(id, _)| self.selected.as_deref() == Some(id.as_str()));
        render_bracketed_list(
            frame,
            inner,
            styles,
            &items,
            selected,
            focused,
            &mut self.top,
        );
    }
}

/// Patches the commands of a row with what an operator-command event says.
/// Returns whether a read is wanted — an applied command may have moved the
/// execution, which the event does not say where — and the verdict, when this
/// event is the one that settled the command.
fn patch_operator_command(
    task: &mut TaskSummary,
    kind: &TaskEventKind,
    event: &TaskEvent,
) -> (bool, Option<CommandResolved>) {
    match kind {
        TaskEventKind::OperatorCommandQueued {
            command_id,
            command,
            ..
        } => {
            if task
                .operator_commands
                .iter()
                .all(|known| known.command_id != *command_id)
            {
                task.operator_commands.push(OperatorCommandView {
                    command_id: command_id.clone(),
                    command: command.clone(),
                    status: OperatorCommandStatus::Queued,
                    submitted_at: event.at,
                    updated_at: event.at,
                    detail: None,
                });
            }
            (false, None)
        }
        TaskEventKind::OperatorCommandResolved {
            command_id,
            status,
            detail,
        } => {
            let known = task
                .operator_commands
                .iter_mut()
                .find(|known| known.command_id == *command_id);
            let (command, was_settled) = match known {
                Some(known) => {
                    let was_settled = known.status.is_settled();
                    known.status = *status;
                    known.detail = detail.clone();
                    known.updated_at = event.at;
                    (Some(known.command.clone()), was_settled)
                }
                None => (None, false),
            };
            let resolved = (!was_settled).then(|| CommandResolved {
                task_id: task.task_id.to_string(),
                title: title_of(task),
                command: command.clone(),
                status: *status,
                detail: detail.clone(),
            });
            let refresh = command.is_none() || *status == OperatorCommandStatus::Applied;
            (refresh, resolved)
        }
        _ => (true, None),
    }
}

/// Patches one row with what `kind` says; returns whether a read is wanted
/// because the event says less than the row shows.
fn patch_row(task: &mut TaskSummary, kind: &TaskEventKind, event: &TaskEvent) -> bool {
    match kind {
        TaskEventKind::QueueChanged { reason } => {
            if task.lifecycle == TaskLifecycle::Queued {
                task.queue = Some(QueueView {
                    message: reason.describe(),
                    reason: reason.clone(),
                    since: event.at,
                });
            }
            false
        }
        TaskEventKind::InputQueued {
            pending_input_id, ..
        } => {
            task.attention
                .retain(|item| item.waiting_id != *pending_input_id);
            false
        }
        TaskEventKind::RunnerReported {
            execution_status,
            pending_input_id,
            attention,
            event_cursor,
            ..
        } => {
            let Some(runner) = task.runner.as_mut() else {
                return true;
            };
            runner.execution_status = *execution_status;
            runner.event_cursor = *event_cursor;
            if pending_input_id.is_none() {
                runner.pending_input = None;
            }
            replace_reported_attention(&mut task.attention, attention, &task.inputs);
            // The step the runner is in is not in the event.
            true
        }
        _ => true,
    }
}

/// Whether the task occupies a slot of its backend: it holds compute, or it
/// was admitted and waits for its start, or it was lost but not released.
fn holds_a_slot(task: &TaskSummary) -> bool {
    match task.lifecycle {
        TaskLifecycle::Provisioning
        | TaskLifecycle::Running
        | TaskLifecycle::PauseRequested
        | TaskLifecycle::Interrupted
        | TaskLifecycle::Unknown => true,
        TaskLifecycle::Queued => task
            .queue
            .as_ref()
            .is_some_and(|queue| queue.reason == QueueReason::Admitted),
        TaskLifecycle::Paused | TaskLifecycle::Archiving | TaskLifecycle::Archived => false,
    }
}

/// Replaces the runner-reported part of a row's attention with a new report,
/// keeping what the control plane added (a failed provisioning) and leaving
/// out what the owner already answered.
fn replace_reported_attention(
    current: &mut Vec<AttentionItem>,
    reported: &[AttentionItem],
    inputs: &[InputView],
) {
    use narsor_protocol::controller::InputStatus;
    let answered = |waiting_id: &str| {
        inputs.iter().any(|input| {
            input.pending_input_id == waiting_id
                && matches!(
                    input.status,
                    InputStatus::Queued | InputStatus::Delivered | InputStatus::Consumed
                )
        })
    };
    let mut items: Vec<AttentionItem> = current
        .iter()
        .filter(|item| item.kind == AttentionKind::ProvisioningFailed)
        .cloned()
        .collect();
    items.extend(
        reported
            .iter()
            .filter(|item| !answered(&item.waiting_id))
            .cloned(),
    );
    items.sort_by_key(|item| item.since);
    *current = items;
}

/// Fits `text` into `width` characters, ending in `…` when it is cut.
pub fn fit(text: &str, width: usize) -> String {
    let length = text.chars().count();
    if length <= width {
        return text.to_string();
    }
    match width {
        0 => String::new(),
        1 => "…".to_string(),
        _ => text
            .chars()
            .take(width - 1)
            .chain(std::iter::once('…'))
            .collect(),
    }
}

/// The columns before the title (and under it, before the label): attention,
/// a space, state, two spaces.
const ROW_PREFIX: usize = 5;

/// One row as two styled lines laid out in `width` columns — the content
/// width, the bracket's column already taken off: the markers and the title,
/// then the label under the title. The second line is there even when the
/// label is empty, so every row is as tall as the next. The lines carry the
/// roles of their content only; the selection is drawn around them.
pub(super) fn row_text(
    row: &RowText,
    width: usize,
    styles: &Styles,
    stale: bool,
) -> Vec<Line<'static>> {
    let text_width = width.saturating_sub(ROW_PREFIX);
    let grey = |role: Role| if stale { Role::Done } else { role };
    let label_role = match row.label.as_str() {
        "offline" => Role::Attention,
        "nie wysłano" => Role::Problem,
        label if label.starts_with('#') => Role::Calm,
        _ => Role::Done,
    };

    let first = Line::from(vec![
        Span::styled(
            row.attention.glyph().to_string(),
            styles.of(grey(row.attention.role())),
        ),
        Span::raw(" "),
        Span::styled(
            row.icon.glyph().to_string(),
            styles.of(grey(row.icon.role())),
        ),
        Span::raw("  "),
        Span::styled(fit(&row.title, text_width), styles.of(grey(Role::Calm))),
    ]);
    let second = Line::from(vec![
        Span::raw(" ".repeat(ROW_PREFIX.min(width))),
        Span::styled(fit(&row.label, text_width), styles.of(grey(label_role))),
    ]);
    vec![first, second]
}
