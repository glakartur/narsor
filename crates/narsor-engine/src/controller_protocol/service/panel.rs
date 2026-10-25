//! The forms of the owner's panel, and which one opens first (R1, R2, R5–R9,
//! R21, R22).
//!
//! One pure function of the task's summary, [`offer_forms`], fills
//! [`TaskSummary::panel`]. The client shows what it says and decides nothing,
//! not even what a form of a wait asks: its entries, fields and the way its
//! answer is delivered are the definition the server builds with
//! [`narsor_protocol::panel::define`]. Whether a message to the agent is
//! possible is the answer of `actions::comment_reason`, the same one the offer
//! of actions gives, so the two cannot disagree.
//!
//! The order of the list is the order the owner steps through it: the form of
//! the wait (an answer, a permission, an entry limit), the free chat, the
//! action. The default is the first that applies of: the entry limit, the
//! permission, the answer, the action of a step that finished, the chat.

use narsor_protocol::api::{InputContinuationView, PendingHumanInputView};
use narsor_protocol::controller::{
    AttentionKind, PanelChoice, PanelForm, PanelFormKind, PanelOffer, TaskActionId, TaskState,
    TaskSummary,
};
use narsor_protocol::panel::define;

use super::actions::{comment_reason, options, runner_alive, unavailable_reason, waiting};

/// The forms of `task`'s panel; empty while nothing can be sent to it.
///
/// `None` when the task keeps its own panel (see [`keeps_own_panel`]): the
/// client then shows that one and no form of this module. This is the only
/// place of the rule.
pub(super) fn offer_forms(task: &TaskSummary) -> Option<PanelOffer> {
    if keeps_own_panel(task) {
        return None;
    }
    if !asks_the_owner(task) {
        return Some(PanelOffer::default());
    }
    let wait = waiting(task);
    let mut forms = Vec::new();
    // The form that is the first of the list and the default: what the
    // decision order of the module comment picks.
    let mut default = None;

    let wait_kind = match wait.as_ref().map(|form| form.continuation) {
        Some(InputContinuationView::EntryLimit) => Some(PanelFormKind::EntryLimit),
        Some(InputContinuationView::ResolvePermission) => Some(PanelFormKind::Permission),
        Some(InputContinuationView::ResumeAgent | InputContinuationView::ApproveOutput) => {
            Some(PanelFormKind::Answer)
        }
        _ => None,
    };
    if let Some(kind) = wait_kind {
        default = Some(forms.len());
        forms.push(wait_form(task, kind, wait.as_ref()));
    }

    let chat = comment_reason(task).is_none();
    if chat {
        if default.is_none() && wait.is_none() {
            default = Some(forms.len());
        }
        forms.push(chat_form(task));
    }

    // A wait whose answer is a choice of the step's output is an action of
    // the workflow (Z4); it is the default when nothing before it was.
    match wait.as_ref().map(|form| form.continuation) {
        Some(InputContinuationView::CompleteStep | InputContinuationView::ResolveOutput) => {
            if default.is_none() {
                default = Some(forms.len());
            }
            forms.push(wait_form(task, PanelFormKind::Action, wait.as_ref()));
        }
        _ => {
            if let Some(form) = action_form(task) {
                // An action is the default only when the step ended with
                // nobody left to talk to (R9): nothing waits and no chat.
                if default.is_none() && !chat {
                    default = Some(forms.len());
                }
                forms.push(form);
            }
        }
    }

    // A task that waits on nothing and can be talked to opens on the chat.
    if default.is_none() && !forms.is_empty() {
        default = forms
            .iter()
            .position(|form| form.kind == PanelFormKind::Chat);
    }
    Some(PanelOffer { forms, default })
}

/// Whether the task waits on one of the owner's own panels (a proposal of the
/// conductor, a failed step, a failed provisioning, a problem of the runner),
/// which keep their panels.
fn keeps_own_panel(task: &TaskSummary) -> bool {
    task.attention.iter().any(|item| {
        matches!(
            item.kind,
            AttentionKind::ConductorProposal
                | AttentionKind::FailedStep
                | AttentionKind::ProvisioningFailed
        )
    }) || task.runner.as_ref().is_some_and(|r| r.problem.is_some())
}

/// Whether the task is one a form can be sent to: not queued, not archived,
/// not working.
fn asks_the_owner(task: &TaskSummary) -> bool {
    if task.runner_state.is_archival() {
        return false;
    }
    if matches!(
        task.state,
        TaskState::Queued | TaskState::Archived | TaskState::Unrecognized
    ) {
        return false;
    }
    // The agent (or the script) is at work: nothing to send (R8).
    !(task.state == TaskState::Running && waiting(task).is_none() && runner_alive(task))
}

fn chat_form(task: &TaskSummary) -> PanelForm {
    PanelForm {
        kind: PanelFormKind::Chat,
        waiting_id: waiting_id(task),
        choices: Vec::new(),
        definition: None,
    }
}

/// The form of a wait: which wait it answers, and what it asks — the
/// entries, their fields and how the answer is delivered. The server names the
/// workflow state an output resumes only as an attempt: the client knows the
/// steps and says which state it is.
fn wait_form(
    task: &TaskSummary,
    kind: PanelFormKind,
    wait: Option<&PendingHumanInputView>,
) -> PanelForm {
    PanelForm {
        kind,
        waiting_id: waiting_id(task),
        choices: Vec::new(),
        definition: wait.map(|wait| define(wait, &|_| None)),
    }
}

/// The manual action of the workflow the task offers now: a transition out of
/// its state. A restart or a resume is a command of the task's life with a key
/// of its own, not an action of the workflow. `None` when it offers none.
fn action_form(task: &TaskSummary) -> Option<PanelForm> {
    if unavailable_reason(task, TaskActionId::Transition).is_some() {
        return None;
    }
    let exits = options(task, TaskActionId::Transition)
        .iter()
        .map(|exit| exit.label.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Some(PanelForm {
        kind: PanelFormKind::Action,
        waiting_id: None,
        choices: vec![PanelChoice {
            label: "Przejście ręczne".to_string(),
            description: Some(exits),
            action: Some(TaskActionId::Transition),
        }],
        definition: None,
    })
}

fn waiting_id(task: &TaskSummary) -> Option<String> {
    task.runner
        .as_ref()
        .and_then(|runner| runner.pending_input.as_ref())
        .filter(|_| waiting(task).is_some())
        .map(|pending| pending.id.clone())
}
