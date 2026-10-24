//! The forms of the owner's panel, and which one opens first (R1, R2, R5–R9,
//! R21, R22).
//!
//! One pure function of the task's summary, [`offer_forms`], fills
//! [`TaskSummary::panel`]. The client shows what it says and decides nothing:
//! whether a message to the agent is possible is the answer of
//! `actions::comment_reason`, the same one the offer of actions gives, so the
//! two cannot disagree.
//!
//! The order of the list is the order the owner steps through it: the form of
//! the wait (an answer, a permission, an entry limit), the free chat, the
//! action. The default is the first that applies of: the entry limit, the
//! permission, the answer, the action of a step that finished, the chat.

use narsor_protocol::api::{
    HumanInputTypeView, InputContinuationView, PendingHumanInputView, ENTRY_LIMIT_CONTINUE,
    ENTRY_LIMIT_DECISION_FIELD, ENTRY_LIMIT_RESET_COUNTER,
};
use narsor_protocol::controller::{
    AttentionKind, PanelChoice, PanelField, PanelFieldKind, PanelForm, PanelFormKind, PanelOffer,
    TaskActionId, TaskState, TaskSummary,
};

use super::actions::{comment_reason, options, runner_alive, unavailable_reason, waiting};

/// The name of the field that carries the owner's note to the agent.
pub(crate) const CHAT_FIELD: &str = "message";
/// The name of the optional comment of an answer.
pub(crate) const COMMENT_FIELD: &str = "comment";

/// The forms of `task`'s panel; empty while nothing can be sent to it.
pub(super) fn offer_forms(task: &TaskSummary) -> PanelOffer {
    if !asks_the_owner(task) {
        return PanelOffer::default();
    }
    let wait = waiting(task);
    let mut forms = Vec::new();
    // The form that is the first of the list and the default: what the
    // decision order of the module comment picks.
    let mut default = None;

    match wait.as_ref().map(|form| (form, form.continuation)) {
        Some((form, InputContinuationView::EntryLimit)) => {
            default = Some(forms.len());
            forms.push(entry_limit_form(task, form));
        }
        Some((form, InputContinuationView::ResolvePermission)) => {
            default = Some(forms.len());
            forms.push(permission_form(task, form));
        }
        Some((form, InputContinuationView::ResumeAgent | InputContinuationView::ApproveOutput)) => {
            default = Some(forms.len());
            forms.push(answer_form(task, form));
        }
        _ => {}
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
    match wait.as_ref().map(|form| (form, form.continuation)) {
        Some((
            form,
            InputContinuationView::CompleteStep | InputContinuationView::ResolveOutput,
        )) => {
            if default.is_none() {
                default = Some(forms.len());
            }
            forms.push(output_form(task, form));
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
    PanelOffer { forms, default }
}

/// Whether the summary is of a task the panel of this module serves at all:
/// not queued, not archived, not working, and without a wait of the owner's
/// own panels (a proposal of the conductor, a failed step, a failed
/// provisioning), which keep their panels.
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
    let operator_wait = task.attention.iter().any(|item| {
        matches!(
            item.kind,
            AttentionKind::ConductorProposal
                | AttentionKind::FailedStep
                | AttentionKind::ProvisioningFailed
        )
    });
    if operator_wait || task.runner.as_ref().is_some_and(|r| r.problem.is_some()) {
        return false;
    }
    // The agent (or the script) is at work: nothing to send (R8).
    !(task.state == TaskState::Running && waiting(task).is_none() && runner_alive(task))
}

fn comment_field() -> PanelField {
    PanelField {
        name: COMMENT_FIELD.to_string(),
        label: "Komentarz".to_string(),
        description: None,
        kind: PanelFieldKind::Multiline,
        required: false,
        options: Vec::new(),
        initial: None,
    }
}

fn chat_form(task: &TaskSummary) -> PanelForm {
    PanelForm {
        kind: PanelFormKind::Chat,
        waiting_id: waiting_id(task),
        fields: vec![PanelField {
            name: CHAT_FIELD.to_string(),
            label: "Wiadomość do agenta".to_string(),
            description: None,
            kind: PanelFieldKind::Multiline,
            required: true,
            options: Vec::new(),
            initial: None,
        }],
        choices: Vec::new(),
    }
}

fn answer_form(task: &TaskSummary, form: &PendingHumanInputView) -> PanelForm {
    let mut fields = declared_fields(form);
    fields.push(comment_field());
    PanelForm {
        kind: PanelFormKind::Answer,
        waiting_id: waiting_id(task),
        fields,
        choices: output_choices(form),
    }
}

fn output_form(task: &TaskSummary, form: &PendingHumanInputView) -> PanelForm {
    PanelForm {
        kind: PanelFormKind::Action,
        waiting_id: waiting_id(task),
        fields: declared_fields(form),
        choices: output_choices(form),
    }
}

fn permission_form(task: &TaskSummary, _form: &PendingHumanInputView) -> PanelForm {
    PanelForm {
        kind: PanelFormKind::Permission,
        waiting_id: waiting_id(task),
        fields: vec![comment_field()],
        choices: vec![
            PanelChoice {
                label: "Zezwól".to_string(),
                description: None,
                action: Some(TaskActionId::AllowPermission),
            },
            PanelChoice {
                label: "Odmów".to_string(),
                description: None,
                action: Some(TaskActionId::DenyPermission),
            },
        ],
    }
}

fn entry_limit_form(task: &TaskSummary, form: &PendingHumanInputView) -> PanelForm {
    let choices = [ENTRY_LIMIT_CONTINUE, ENTRY_LIMIT_RESET_COUNTER]
        .into_iter()
        .map(|value| PanelChoice {
            label: value.to_string(),
            description: None,
            action: None,
        })
        .collect();
    PanelForm {
        kind: PanelFormKind::EntryLimit,
        waiting_id: waiting_id(task),
        fields: vec![PanelField {
            name: ENTRY_LIMIT_DECISION_FIELD.to_string(),
            label: form
                .entry_limit
                .as_ref()
                .map(|limit| {
                    format!(
                        "Limit wejść w krok: {}/{}",
                        limit.entries, limit.max_entries
                    )
                })
                .unwrap_or_else(|| "Limit wejść w krok".to_string()),
            description: None,
            kind: PanelFieldKind::Select,
            required: true,
            options: vec![
                ENTRY_LIMIT_CONTINUE.to_string(),
                ENTRY_LIMIT_RESET_COUNTER.to_string(),
            ],
            initial: None,
        }],
        choices,
    }
}

/// The manual actions of the workflow the task offers now: the exits of its
/// state, a restart, a resume. `None` when it offers none.
fn action_form(task: &TaskSummary) -> Option<PanelForm> {
    let exits = options(task, TaskActionId::Transition);
    let mut choices = Vec::new();
    if unavailable_reason(task, TaskActionId::Transition).is_none() {
        choices.push(PanelChoice {
            label: "Przejście ręczne".to_string(),
            description: Some(
                exits
                    .iter()
                    .map(|exit| exit.label.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            action: Some(TaskActionId::Transition),
        });
    }
    for (id, label) in [
        (TaskActionId::Resume, "Wznów zadanie"),
        (TaskActionId::Restart, "Uruchom krok od nowa"),
    ] {
        if unavailable_reason(task, id).is_none() {
            choices.push(PanelChoice {
                label: label.to_string(),
                description: None,
                action: Some(id),
            });
        }
    }
    if choices.is_empty() {
        return None;
    }
    Some(PanelForm {
        kind: PanelFormKind::Action,
        waiting_id: None,
        fields: Vec::new(),
        choices,
    })
}

fn waiting_id(task: &TaskSummary) -> Option<String> {
    task.runner
        .as_ref()
        .and_then(|runner| runner.pending_input.as_ref())
        .filter(|_| waiting(task).is_some())
        .map(|pending| pending.id.clone())
}

fn output_choices(form: &PendingHumanInputView) -> Vec<PanelChoice> {
    form.outputs
        .iter()
        .map(|output| PanelChoice {
            label: output.label.clone(),
            description: output.description.clone(),
            action: None,
        })
        .collect()
}

/// The fields the workflow declared for the request, as the panel has them.
fn declared_fields(form: &PendingHumanInputView) -> Vec<PanelField> {
    form.fields
        .iter()
        .map(|field| PanelField {
            name: field.name.clone(),
            label: field.name.clone(),
            description: field.description.clone(),
            kind: match field.field_type {
                HumanInputTypeView::Text => PanelFieldKind::Text,
                HumanInputTypeView::Select => PanelFieldKind::Select,
            },
            required: field.required,
            options: field.options.clone(),
            initial: None,
        })
        .collect()
}
