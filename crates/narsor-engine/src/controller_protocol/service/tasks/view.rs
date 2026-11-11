//! The parts of a task summary derived from its record and workflow.

use crate::controller_protocol::wire::*;
use crate::domain::{
    effective_title, one_line_label, ActionConfig, WorkflowDefinition, WorkspaceHeader,
    GENERATED_TITLE_MAX_CHARS,
};
use crate::engine::{manual_override_allowed, PositionStatus, Timeline};
use crate::orchestration::{AttentionItem, AttentionKind, InputStatus, RunnerState, TaskRecord};
use crate::workspace::state::launch_headers;

/// What waits on the owner: the runner's last report — minus the requests
/// the owner has answered since (an answer queued, delivered or consumed)
/// — plus a provisioning that failed. Oldest first.
pub(super) fn task_attention(record: &TaskRecord) -> Vec<AttentionItem> {
    if record.runner_state.is_archival() {
        return Vec::new();
    }
    let answered = |waiting_id: &str| {
        record.inputs.iter().any(|input| {
            input.pending_input_id == waiting_id
                && matches!(
                    input.status,
                    InputStatus::Queued | InputStatus::Delivered | InputStatus::Consumed
                )
        })
    };
    // Already cleaned when the snapshot was accepted.
    let mut items: Vec<AttentionItem> = record
        .runner
        .iter()
        .flat_map(|runner| &runner.snapshot.attention)
        .filter(|item| !answered(&item.waiting_id))
        .cloned()
        .collect();
    // The backend gave up before any execution existed: the runner (if one
    // ever reported) never got as far as creating one.
    let has_execution = record
        .runner
        .as_ref()
        .is_some_and(|runner| runner.snapshot.execution_cursor.execution_id.is_some());
    if matches!(
        record.runner_state,
        RunnerState::Interrupted | RunnerState::Unknown
    ) && !has_execution
    {
        items.push(AttentionItem {
            kind: AttentionKind::ProvisioningFailed,
            state_id: None,
            since: record
                .observed
                .as_ref()
                .map_or(record.created_at, |observed| observed.received_at),
            waiting_id: format!("provisioning-{}", record.generation),
        });
    }
    items.sort_by_key(|item| item.since);
    items
}

/// The label of the attempt under way — in progress or awaiting a decision
/// — the last one if several.
pub(super) fn current_position(timeline: &Timeline) -> Option<String> {
    timeline
        .positions
        .iter()
        .rev()
        .find(|position| {
            matches!(
                position.status,
                PositionStatus::InProgress | PositionStatus::AwaitingDecision
            )
        })
        .map(|position| position.label.clone())
}

/// The transitions an owner can ask for out of the state the runner last
/// reported: read from the sealed workflow snapshot, only while the task is
/// running (nothing else can carry one out).
pub(super) fn manual_transitions(record: &TaskRecord) -> Option<TransitionOffer> {
    if record.runner_state != RunnerState::Running {
        return None;
    }
    let current = record
        .runner
        .as_ref()?
        .snapshot
        .execution_cursor
        .current_state
        .as_deref()?;
    let definition = record.launch_spec.definition().ok()?;
    transition_offer(&definition, current)
}

/// Every step of the workflow, for the stop in which the owner may choose
/// one: the task is stopped (a finished workflow is stopped too). The first is the
/// initial step, the rest follow by name. `None` elsewhere, so a list of
/// tasks does not carry it, and when the sealed workflow cannot be read.
pub(crate) fn workflow_steps(record: &TaskRecord) -> Option<Vec<StepView>> {
    if record.runner_state != RunnerState::Stopped {
        return None;
    }
    let definition = record.launch_spec.definition().ok()?;
    let initial = definition.initial_state.to_string();
    let mut ids: Vec<String> = definition.states.keys().map(ToString::to_string).collect();
    ids.sort();
    if let Some(at) = ids.iter().position(|id| *id == initial) {
        let first = ids.remove(at);
        ids.insert(0, first);
    }
    Some(
        ids.into_iter()
            .filter_map(|id| {
                let state = definition.states.get(id.as_str())?;
                let kind = match &state.action {
                    Some(ActionConfig::Agent(_)) => StepKind::Agent,
                    Some(ActionConfig::Profile(_)) => StepKind::Profile,
                    Some(ActionConfig::Script(_)) => StepKind::Script,
                    Some(ActionConfig::Human(_)) => StepKind::Human,
                    Some(ActionConfig::Wait(_)) => StepKind::Wait,
                    Some(ActionConfig::Decision(_)) => StepKind::Decision,
                    None => StepKind::Terminal,
                };
                Some(StepView {
                    label: id.clone(),
                    id,
                    kind,
                })
            })
            .collect(),
    )
}

/// What an owner can ask for out of `current` in `definition`.
pub(crate) fn transition_offer(
    definition: &WorkflowDefinition,
    current: &str,
) -> Option<TransitionOffer> {
    let state = definition.states.get(current)?;
    let mut exits: Vec<TransitionExit> = Vec::new();
    let mut labels: Vec<(&String, &String)> = state.transitions.iter().collect();
    labels.sort();
    for (label, target) in labels {
        if exits.iter().any(|exit| exit.target == *target) {
            continue;
        }
        exits.push(TransitionExit {
            label: label.clone(),
            target: target.clone(),
            description: state
                .choices
                .iter()
                .find(|output| output.label == *label)
                .and_then(|output| output.description.clone()),
        });
    }
    let override_allowed = manual_override_allowed(definition, state);
    let mut override_targets: Vec<String> = if override_allowed {
        definition
            .states
            .keys()
            .filter(|id| id.as_str() != current)
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    override_targets.sort();
    Some(TransitionOffer {
        from_state: current.to_string(),
        exits,
        override_allowed,
        override_targets,
    })
}

/// The task's title: the operator's, else the conductor's, which a runner
/// reports and the server therefore reduces to one short plain line.
pub(super) fn task_title(record: &TaskRecord) -> Option<String> {
    let generated = record
        .runner
        .as_ref()
        .and_then(|runner| runner.snapshot.generated_title.as_deref())
        .and_then(|title| one_line_label(title, GENERATED_TITLE_MAX_CHARS));
    effective_title(record.launch_spec.title.as_deref(), generated.as_deref())
}

/// The workspace rows a task shows: what its runner reported, else what the
/// launch spec knows, which is all there is while the task waits in the
/// queue. The workspaces keep the launch spec's order.
pub fn task_workspaces(record: &TaskRecord) -> Vec<WorkspaceHeader> {
    let reported = record
        .runner
        .as_ref()
        .map(|runner| runner.snapshot.workspaces.as_slice())
        .filter(|workspaces| !workspaces.is_empty());
    match reported {
        // A remote runner's word is not trusted with a terminal.
        Some(workspaces) => workspaces
            .iter()
            .cloned()
            .map(WorkspaceHeader::sanitized)
            .collect(),
        None => launch_headers(&record.launch_spec.workspace),
    }
}
