//! What the owner can do to a task now, and why not (R1, R2, R4).
//!
//! One pure function of the task's summary, [`offer`], says for every action
//! whether it can be done and, when it cannot, why in words for the owner. It
//! fills [`TaskSummary::actions`], and the paths that send a command ask it
//! again at the moment of sending ([`ProtocolService::require_available`]),
//! so what a client was shown and what the server accepts cannot disagree.
//!
//! The reasons are the ones the TUI gave before the server took over the
//! decision; their words are kept.

use narsor_protocol::api::{
    HumanInputActionDto, HumanInputDeliveryDto, InputContinuationView, PendingHumanInputView,
    PermissionDecisionDto, PermissionDecisionKindDto,
};
use narsor_protocol::controller::{
    AttentionKind, Freshness, RunnerState, SubmitCommentRequest, SubmitCommentResponse,
    SubmitInputRequest, TaskActionId, TaskActionOffer, TaskActionOption, TaskState, TaskSummary,
};
use narsor_protocol::display::{loss_state, LossState};

use super::ProtocolService;
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::orchestration::{lifecycle, OwnerCommand, TaskRecord};

/// Every action of the offer, in the order a client lists them.
const OFFERED: [TaskActionId; 12] = [
    TaskActionId::Comment,
    TaskActionId::Answer,
    TaskActionId::Approve,
    TaskActionId::Reject,
    TaskActionId::AllowPermission,
    TaskActionId::DenyPermission,
    TaskActionId::Pause,
    TaskActionId::Resume,
    TaskActionId::Interrupt,
    TaskActionId::Transition,
    TaskActionId::Restart,
    TaskActionId::Archive,
];

/// The offer of `task`: every action, the unavailable ones with their reason.
pub(super) fn offer(task: &TaskSummary) -> Vec<TaskActionOffer> {
    OFFERED
        .into_iter()
        .map(|id| {
            let reason = unavailable_reason(task, id);
            TaskActionOffer {
                id,
                available: reason.is_none(),
                options: if reason.is_none() {
                    options(task, id)
                } else {
                    Vec::new()
                },
                reason,
            }
        })
        .collect()
}

/// Why `id` cannot be done to `task` now; `None` when it can.
///
/// [`TaskActionId::Unrecognized`] is not an action of this server.
pub(super) fn unavailable_reason(task: &TaskSummary, id: TaskActionId) -> Option<String> {
    match id {
        TaskActionId::Comment => comment_reason(task),
        TaskActionId::Answer
        | TaskActionId::Approve
        | TaskActionId::Reject
        | TaskActionId::AllowPermission
        | TaskActionId::DenyPermission => decision_reason(task, id),
        TaskActionId::Pause => match task.runner_state {
            RunnerState::Stopped => {
                Some(not_running(task).unwrap_or_else(|| state_phrase(task).to_string()))
            }
            RunnerState::PauseRequested => Some("zadanie jest już wstrzymywane".to_string()),
            _ => not_running(task),
        },
        // Resume is a command of the stopped runner, whatever the owner sees
        // the task as: a finished task without a slot resumes too.
        TaskActionId::Resume => match task.runner_state {
            RunnerState::Stopped => None,
            RunnerState::PauseRequested => Some("zadanie jest już wstrzymywane".to_string()),
            _ => Some(state_phrase(task).to_string()),
        },
        TaskActionId::Interrupt => not_running(task).or(runner_unreachable(task)),
        TaskActionId::Transition => {
            if let Some(reason) = not_running(task).or(runner_unreachable(task)) {
                return Some(reason);
            }
            match &task.manual_transitions {
                None => Some("serwer nie podał wyjść bieżącego stanu".to_string()),
                Some(offer) if offer.exits.is_empty() && !has_override(task) => Some(format!(
                    "stan „{}” nie ma wyjść i nie dopuszcza override",
                    one_line(&offer.from_state)
                )),
                Some(_) => None,
            }
        }
        TaskActionId::Restart => match task.runner_state {
            RunnerState::Interrupted | RunnerState::Unknown => None,
            _ if provisioning_failed(task) => None,
            _ if runner_alive(task) => Some(
                "zadanie działa — jest co przerwać (x), nie ma co uruchamiać od nowa".to_string(),
            ),
            RunnerState::Stopped | RunnerState::PauseRequested => {
                Some(format!("{} — wznów je (p)", state_phrase(task)))
            }
            _ => Some(state_phrase(task).to_string()),
        },
        // The control plane takes an archive in any state ("archive wins":
        // it frees the slot of a running, queued or paused task), so the
        // offer follows it. Only a task already archiving or archived has nothing to do.
        TaskActionId::Archive if task.runner_state.is_archival() => {
            Some("zadanie jest archiwizowane".to_string())
        }
        TaskActionId::Archive => None,
        TaskActionId::Unrecognized => Some("serwer nie zna tej akcji".to_string()),
    }
}

/// Why `task` cannot be moved in the queue now. Moving is an operation on the
/// queue itself: only a task that waits there (not one admitted and about to
/// start) can be moved. The queue move is not part of the offer; the client
/// keeps it local, and this is only the server's own check at the send.
fn move_reason(task: &TaskSummary) -> Option<String> {
    match task.runner_state {
        RunnerState::Queued => None,
        _ => Some("zadanie nie jest w kolejce".to_string()),
    }
}

/// What the offer says `id` has to choose from.
fn options(task: &TaskSummary, id: TaskActionId) -> Vec<TaskActionOption> {
    match id {
        TaskActionId::Answer => waiting(task)
            .map(|form| {
                form.outputs
                    .into_iter()
                    .map(|output| TaskActionOption {
                        label: output.label,
                        description: output.description,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        TaskActionId::Transition => task
            .manual_transitions
            .iter()
            .flat_map(|offer| &offer.exits)
            .map(|exit| TaskActionOption {
                label: format!("{} → {}", exit.label, exit.target),
                description: exit.description.clone(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------
// The wait on the owner
// ---------------------------------------------------------------------

/// The request the task waits on the owner to answer: the runner's pending
/// input, as long as it is still among the things the task waits for (an
/// answer queued for it takes it off the attention list).
fn waiting(task: &TaskSummary) -> Option<PendingHumanInputView> {
    let pending = task.runner.as_ref()?.pending_input.as_ref()?;
    let waits = task.attention.iter().any(|item| {
        item.waiting_id == pending.id
            && matches!(
                item.kind,
                AttentionKind::Question
                    | AttentionKind::Approval
                    | AttentionKind::Problem
                    | AttentionKind::EntryLimit
            )
    });
    if !waits {
        return None;
    }
    serde_json::from_value(pending.form.clone()).ok()
}

/// Why a decision of the pending request (an answer, an approval, a
/// permission) is not one the task waits for.
fn decision_reason(task: &TaskSummary, id: TaskActionId) -> Option<String> {
    let (accepts, wanted): (&[InputContinuationView], &str) = match id {
        TaskActionId::Approve | TaskActionId::Reject => {
            (&[InputContinuationView::ApproveOutput], "zatwierdzenie")
        }
        TaskActionId::AllowPermission | TaskActionId::DenyPermission => (
            &[InputContinuationView::ResolvePermission],
            "zgodę na uprawnienie",
        ),
        _ => (
            &[
                InputContinuationView::CompleteStep,
                InputContinuationView::ResumeAgent,
                InputContinuationView::ResolveOutput,
                InputContinuationView::EntryLimit,
            ],
            "odpowiedź",
        ),
    };
    match waiting(task) {
        Some(form) if accepts.contains(&form.continuation) => None,
        _ => Some(format!("zadanie nie czeka na {wanted}")),
    }
}

/// Why the owner cannot comment on the task now.
///
/// A comment has somewhere to go in four stops: a question of the agent, an
/// approval, a permission, and a pause of the operator. A failed step, a
/// problem of the runner and a failed provisioning are not among them.
fn comment_reason(task: &TaskSummary) -> Option<String> {
    if let Some(form) = waiting(task) {
        return match form.continuation {
            InputContinuationView::ResumeAgent
            | InputContinuationView::ResolveOutput
            | InputContinuationView::ApproveOutput
            | InputContinuationView::ResolvePermission => None,
            // No agent session waits for a message at these two.
            InputContinuationView::CompleteStep
            | InputContinuationView::EntryLimit
            | InputContinuationView::Unrecognized => Some(format!(
                "{} — ta prośba nie przyjmuje komentarza, odpowiedz na nią",
                state_phrase(task)
            )),
        };
    }
    if task.runner_state == RunnerState::Stopped && task.state == TaskState::Paused {
        return None;
    }
    if task.state == TaskState::Queued {
        return Some(format!(
            "{} — nie ma jeszcze sesji agenta",
            state_phrase(task)
        ));
    }
    Some(format!(
        "{} — komentarz można dodać przy pytaniu, zatwierdzeniu, zgodzie na uprawnienie i w pauzie",
        state_phrase(task)
    ))
}

// ---------------------------------------------------------------------
// What the task is
// ---------------------------------------------------------------------

/// What the task is, as a clause that begins a reason.
fn state_phrase(task: &TaskSummary) -> &'static str {
    if task.runner_state == RunnerState::Interrupted {
        return "zadanie jest przerwane";
    }
    match task.state {
        TaskState::Queued => "zadanie jest w kolejce",
        TaskState::Running => "zadanie działa",
        TaskState::HumanNeeded => "zadanie czeka na odpowiedź",
        TaskState::Paused if task.holds_slot => "zadanie jest wstrzymywane",
        TaskState::Paused => "zadanie jest wstrzymane",
        TaskState::Finished => "zadanie jest zakończone",
        TaskState::Error => "zadanie jest przerwane",
        TaskState::Archived => "zadanie jest archiwizowane",
        TaskState::Unrecognized => "zadanie jest w nieznanym stanie",
    }
}

/// Whether the runner of the task is alive and asked about its step: the task
/// runs, or waits for an answer with the slot still held.
fn runner_alive(task: &TaskSummary) -> bool {
    if task.runner_state == RunnerState::Interrupted {
        // Its workload is gone, whatever the owner sees while the resume
        // policy still tries.
        return false;
    }
    match task.state {
        TaskState::Running => true,
        TaskState::HumanNeeded => task.holds_slot,
        _ => false,
    }
}

/// Whether the control plane could not prepare the workspace.
fn provisioning_failed(task: &TaskSummary) -> bool {
    task.attention
        .iter()
        .any(|item| item.kind == AttentionKind::ProvisioningFailed)
}

/// Why a task is not one whose step can be acted on — `None` when its
/// runner is alive and in a step.
fn not_running(task: &TaskSummary) -> Option<String> {
    let phrase = state_phrase(task);
    if let Some(LossState::AutoResume { attempt, limit }) =
        loss_state(task.runner_state, &task.resume)
    {
        return Some(format!(
            "{phrase} — wznowi się samo (próba {attempt}/{limit}) albo uruchom je od nowa (s)"
        ));
    }
    if runner_alive(task) {
        return None;
    }
    if task.state == TaskState::HumanNeeded {
        return Some(format!("{phrase} — odpowiedz, a zadanie wznowi się"));
    }
    if task.runner_state == RunnerState::Stopped {
        return Some(format!("{phrase} — wznów je (p)"));
    }
    if task.state == TaskState::Error || task.runner_state == RunnerState::Interrupted {
        return Some(format!("{phrase} — uruchom je od nowa (s)"));
    }
    Some(phrase.to_string())
}

/// Why the runner of a running task cannot be asked for anything.
fn runner_unreachable(task: &TaskSummary) -> Option<String> {
    match task.runner.as_ref() {
        None => Some("runner jeszcze się nie zgłosił".to_string()),
        Some(runner) if runner.freshness != Freshness::Live => {
            Some("runner nie odpowiada od dłuższego czasu".to_string())
        }
        Some(_) => None,
    }
}

/// Whether the current state of `task` allows an override with somewhere to go.
fn has_override(task: &TaskSummary) -> bool {
    task.manual_transitions
        .as_ref()
        .is_some_and(|offer| offer.override_allowed && !offer.override_targets.is_empty())
}

/// `text` on one line, without control characters.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

// ---------------------------------------------------------------------
// At the send
// ---------------------------------------------------------------------

/// The action an owner command is, as the offer names it. A resume of a task
/// whose workload is gone is the offer's "start again".
pub(super) fn owner_action(command: OwnerCommand, task: &TaskRecord) -> TaskActionId {
    match command {
        OwnerCommand::Pause => TaskActionId::Pause,
        OwnerCommand::Archive => TaskActionId::Archive,
        OwnerCommand::Start => TaskActionId::Restart,
        OwnerCommand::Resume
            if matches!(
                task.runner_state,
                RunnerState::Interrupted | RunnerState::Unknown
            ) =>
        {
            TaskActionId::Restart
        }
        OwnerCommand::Resume => TaskActionId::Resume,
    }
}

impl ProtocolService {
    /// Refuses `id` with the reason of the offer when it cannot be done to the
    /// task now.
    ///
    /// Silent where the task has moved on from `expected_generation`: the
    /// control plane refuses that with `stale_generation`, the code clients
    /// know.
    pub(super) fn require_available(
        &self,
        record: &TaskRecord,
        id: TaskActionId,
        expected_generation: Option<u64>,
    ) -> ProtocolResult<()> {
        if expected_generation.is_some_and(|expected| expected != record.generation) {
            return Ok(());
        }
        let summary = self.summarize(record)?;
        match unavailable_reason(&summary, id) {
            Some(reason) => Err(ProtocolError::conflict("action_unavailable", reason)),
            None => Ok(()),
        }
    }

    /// An owner command of the offer, checked before the control plane sees
    /// it. A command the lifecycle itself refuses (`illegal_command`) or an
    /// archive of an archived task (which converges) is left to it.
    pub(super) fn require_owner_command_available(
        &self,
        record: &TaskRecord,
        command: OwnerCommand,
        expected_generation: Option<u64>,
    ) -> ProtocolResult<()> {
        // An archive of an archived task converges in the control plane, so
        // it is not refused here.
        if command == OwnerCommand::Archive
            || !matches!(
                lifecycle::command_effect(record.runner_state, command),
                lifecycle::CommandEffect::Transition(_)
            )
        {
            return Ok(());
        }
        self.require_available(record, owner_action(command, record), expected_generation)
    }

    /// A delivery to the pending input, checked as the decision it is: an
    /// answer, an approval or a rejection, a permission allowed or denied.
    /// Left to the control plane (`obsolete_input` and the like) when the
    /// request is not the one the task waits on.
    pub(super) fn require_input_available(
        &self,
        record: &TaskRecord,
        request: &SubmitInputRequest,
    ) -> ProtocolResult<()> {
        let summary = self.summarize(record)?;
        let on_this_request = summary
            .runner
            .as_ref()
            .and_then(|runner| runner.pending_input.as_ref())
            .is_some_and(|pending| pending.id == request.pending_input_id);
        let Some(form) = waiting(&summary).filter(|_| on_this_request) else {
            return Ok(());
        };
        let delivery = &request.delivery;
        let allowed = delivery
            .permission_decision
            .as_ref()
            .is_some_and(|decision| decision.decision == PermissionDecisionKindDto::Allow);
        let action = match form.continuation {
            InputContinuationView::ApproveOutput
                if delivery.action == Some(HumanInputActionDto::Approve) =>
            {
                TaskActionId::Approve
            }
            InputContinuationView::ApproveOutput => TaskActionId::Reject,
            InputContinuationView::ResolvePermission if allowed => TaskActionId::AllowPermission,
            InputContinuationView::ResolvePermission => TaskActionId::DenyPermission,
            _ => TaskActionId::Answer,
        };
        match unavailable_reason(&summary, action) {
            Some(reason) => Err(ProtocolError::conflict("action_unavailable", reason)),
            None => Ok(()),
        }
    }

    /// A move in the queue, checked before the control plane sees it.
    pub(super) fn require_movable(
        &self,
        record: &TaskRecord,
        expected_generation: Option<u64>,
    ) -> ProtocolResult<()> {
        if expected_generation.is_some_and(|expected| expected != record.generation) {
            return Ok(());
        }
        let summary = self.summarize(record)?;
        match move_reason(&summary) {
            Some(reason) => Err(ProtocolError::conflict("task_not_queued", reason)),
            None => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------
// The comment
// ---------------------------------------------------------------------

/// The delivery a comment is to the request the task waits on, and the
/// request's id (KTD4).
///
/// The owner's text never allows anything: at an approval and at a
/// permission it turns the request down with the text as the note.
fn comment_delivery(task: &TaskSummary, text: &str) -> Option<(String, HumanInputDeliveryDto)> {
    let form = waiting(task)?;
    let pending_id = task.runner.as_ref()?.pending_input.as_ref()?.id.clone();
    let text = text.to_string();
    let delivery = match form.continuation {
        // The agent asked: the text is the reply, in the same session.
        InputContinuationView::ResumeAgent => HumanInputDeliveryDto {
            values: serde_json::Map::from_iter([(
                "answer".to_string(),
                serde_json::Value::String(text),
            )]),
            ..HumanInputDeliveryDto::default()
        },
        InputContinuationView::ResolveOutput => HumanInputDeliveryDto {
            agent_message: Some(text),
            ..HumanInputDeliveryDto::default()
        },
        // The output is not approved: the agent resumes with the note.
        InputContinuationView::ApproveOutput => HumanInputDeliveryDto {
            comment: Some(text),
            action: Some(HumanInputActionDto::Resume),
            ..HumanInputDeliveryDto::default()
        },
        InputContinuationView::ResolvePermission => HumanInputDeliveryDto {
            permission_decision: Some(PermissionDecisionDto {
                denial_index: None,
                decision: PermissionDecisionKindDto::Deny,
                scope: None,
                pattern: None,
                message: Some(text),
            }),
            ..HumanInputDeliveryDto::default()
        },
        InputContinuationView::CompleteStep
        | InputContinuationView::EntryLimit
        | InputContinuationView::Unrecognized => return None,
    };
    Some((
        pending_id,
        HumanInputDeliveryDto {
            episode: Some(form.episode_key().to_string()),
            ..delivery
        },
    ))
}

impl ProtocolService {
    /// The owner's comment on a stopped task: the one entry point, whatever
    /// the stop is (KTD3). The server picks what it does by the stop (KTD4).
    pub fn submit_comment(
        &self,
        owner: &super::OwnerRef,
        task_id: &str,
        request: SubmitCommentRequest,
    ) -> ProtocolResult<SubmitCommentResponse> {
        let record = self.owned_task(owner, task_id)?;
        if request.idempotency_key.trim().is_empty() {
            return Err(ProtocolError::bad_request(
                "missing_idempotency_key",
                "a comment needs an idempotency key",
            ));
        }
        let text = request.text.trim();
        if text.is_empty() {
            return Err(ProtocolError::bad_request(
                "empty_comment",
                "a comment needs a text",
            ));
        }
        let key_taken = |record: &TaskRecord| {
            record
                .inputs
                .iter()
                .any(|queued| queued.idempotency_key == request.idempotency_key)
                || record
                    .resume_message
                    .as_ref()
                    .is_some_and(|message| message.key == request.idempotency_key)
        };
        if !key_taken(&record) {
            if request
                .expected_generation
                .is_some_and(|expected| expected != record.generation)
            {
                return Err(ProtocolError::conflict(
                    "stale_generation",
                    "the task changed since the comment was written",
                ));
            }
            self.require_available(&record, TaskActionId::Comment, None)?;
        }
        let summary = self.summarize(&record)?;
        let Some((pending_input_id, delivery)) = comment_delivery(&summary, text) else {
            // A pause: nothing waits on an answer, so the comment waits on
            // the record for the runner of the next launch, and the task is
            // resumed (a retry repairs a resume that did not happen).
            if record.runner_state == RunnerState::Stopped {
                let replayed = self.control.store_resume_message(
                    &record.task_id,
                    &request.idempotency_key,
                    text,
                )?;
                if !replayed {
                    self.notify(&record.task_id);
                }
                self.resume_for_work(&record, &request.idempotency_key)?;
                return Ok(SubmitCommentResponse {
                    task: self.summarize(&self.owned_task(owner, task_id)?)?,
                    replayed,
                });
            }
            // A retry of a comment already taken: the request it answered
            // is no longer pending.
            return if key_taken(&record) {
                Ok(SubmitCommentResponse {
                    task: summary,
                    replayed: true,
                })
            } else {
                Err(ProtocolError::conflict(
                    "action_unavailable",
                    comment_reason(&summary)
                        .unwrap_or_else(|| "komentarz nie ma dokąd trafić".to_string()),
                ))
            };
        };
        let response = self.submit_input(
            owner,
            task_id,
            SubmitInputRequest {
                pending_input_id,
                idempotency_key: request.idempotency_key,
                delivery,
            },
        )?;
        let task = self.summarize(&self.owned_task(owner, task_id)?)?;
        Ok(SubmitCommentResponse {
            task,
            replayed: response.replayed,
        })
    }
}
