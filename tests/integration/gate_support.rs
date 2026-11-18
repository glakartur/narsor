//! Tasks at `decision` gates with a `watch` block, as their runners report
//! them, for the observer tests.
//!
//! A runner's snapshot goes through `ProtocolService::record_runner_snapshot`:
//! the snapshot route without its credential check (the control plane, the
//! catalogue's entry scripts and the queue entries), so no runner session
//! and no server are needed.

use chrono::Utc;
use narsor_engine::controller_protocol::ProtocolService;
use narsor_engine::domain::{
    BackendId, ExecutionStatus, GitIdentity, ResolvedEnvironment, ResourceProfile, TaskId,
    TaskLaunchSpec, TaskRepository, TaskRepositorySource, TaskWorkspace,
};
use narsor_engine::orchestration::{
    BackendAvailability, BackendLease, ControllerReport, ExecutionCursor, NewTask, Observation,
    ObservedSummary, OwnerRef, PendingInputRef, RunnerReportOutcome, RunnerSnapshot,
    SchedulingPolicy, TaskRecord,
};
use narsor_protocol::controller::{GateWatch, GateWatchChoice, GateWatchVariable, RunnerProblem};
use serde_json::json;

/// The incarnation of a task's first runner.
pub const FIRST_RUNNER: &str = "pod-1";

/// Creates the queued task `id` of `owner`.
pub fn queued_task(service: &ProtocolService, id: &str, owner: &OwnerRef) -> TaskId {
    let workspace = TaskWorkspace::single(TaskRepository::new(
        "narsor",
        TaskRepositorySource::Git(Box::new(GitIdentity {
            repository: Some("narsor".to_string()),
            remote_url: "https://example.com/narsor.git".to_string(),
            commit: "614c3a8".repeat(5) + "abcde",
            git_ref: None,
            ref_origin: None,
            credential: None,
        })),
    ));
    let spec = TaskLaunchSpec::new(
        "steps",
        1,
        json!({}),
        workspace,
        ResolvedEnvironment::harness_only("claude-code"),
        ResourceProfile::new(1000, 2048),
    );
    let task_id = TaskId::parse(id).unwrap();
    service
        .control_plane()
        .create_task(NewTask::new(
            task_id.clone(),
            owner.clone(),
            BackendId::parse("k8s").unwrap(),
            spec,
        ))
        .unwrap();
    task_id
}

/// Takes the `k8s` backend's lease and brings the queued `task` to
/// `Running` through its controller: its first runner may report.
pub fn running(service: &ProtocolService, task: &TaskId) -> BackendLease {
    let backend = BackendId::parse("k8s").unwrap();
    let control = service.control_plane();
    let lease = control
        .acquire_lease(&backend, "controller-a", chrono::Duration::days(1))
        .unwrap();
    control
        .schedule(
            &backend,
            &SchedulingPolicy {
                backend: BackendAvailability::Available,
                max_concurrent_tasks: Some(16),
            },
        )
        .unwrap();
    observe(service, task, &lease, Observation::StartAccepted);
    observe(service, task, &lease, Observation::RunnerReady);
    lease
}

/// The controller of `lease` reports `observation` of `task`.
pub fn observe(
    service: &ProtocolService,
    task: &TaskId,
    lease: &BackendLease,
    observation: Observation,
) -> TaskRecord {
    let control = service.control_plane();
    let generation = control.task(task).unwrap().generation;
    control
        .report(ControllerReport {
            task_id: task.clone(),
            lease: lease.token(),
            desired_generation: generation,
            observation,
            resource: None,
            summary: ObservedSummary {
                workload_present: true,
                storage_present: true,
                runner_status: None,
                detail: None,
            },
            observed_at: Utc::now(),
        })
        .expect("controller report")
        .task
}

/// The `watch` of the gate `review_work` on `queue`, as a runner renders
/// it: two params, two choices (one needs a variable) and that variable.
pub fn review_gate(queue: &str) -> GateWatch {
    GateWatch {
        step: "review_work".into(),
        queue: queue.into(),
        params: [
            (
                "mr_url".to_string(),
                "https://git.example.com/a/-/merge_requests/17".to_string(),
            ),
            ("label".to_string(), "w CR".to_string()),
        ]
        .into(),
        choices: vec![
            GateWatchChoice {
                name: "check_feedback".into(),
                description: Some("Read the reviewer's feedback".into()),
                requires_variables: Vec::new(),
            },
            GateWatchChoice {
                name: "revise_plan".into(),
                description: None,
                requires_variables: vec!["note".into()],
            },
        ],
        variables: vec![GateWatchVariable {
            name: "note".into(),
            variable_type: Some("text".into()),
            description: Some("What to change".into()),
            options: Vec::new(),
        }],
    }
}

/// The task's first runner reports that it waits at `gate` in `episode`.
pub fn wait_at(
    service: &ProtocolService,
    task: &TaskId,
    episode: &str,
    gate: &GateWatch,
) -> RunnerReportOutcome {
    report(
        service,
        task,
        FIRST_RUNNER,
        true,
        Some((episode, Some(gate))),
    )
}

/// The task's first runner reports that it waits at no gate.
pub fn past_gates(service: &ProtocolService, task: &TaskId) -> RunnerReportOutcome {
    report(service, task, FIRST_RUNNER, true, None)
}

/// A runner snapshot of `incarnation` (launched at generation 1), handed
/// to the control plane and then to the queue entries, as the runner
/// channel does.
///
/// Without `execution` the runner is still starting and reports no wait.
/// `waiting` is the episode the runner waits in and the `watch` of its gate
/// (`None` for a gate without one); `None` when it waits for nothing.
pub fn report(
    service: &ProtocolService,
    task: &TaskId,
    incarnation: &str,
    execution: bool,
    waiting: Option<(&str, Option<&GateWatch>)>,
) -> RunnerReportOutcome {
    let snapshot = snapshot(service, task, incarnation, execution, waiting);
    service
        .record_runner_snapshot(task, incarnation, 1, snapshot, grace())
        .expect("runner snapshot")
}

/// The task's first runner reports a problem: its last execution, no
/// pending input (R17).
pub fn runner_problem(service: &ProtocolService, task: &TaskId) -> RunnerReportOutcome {
    let mut broken = snapshot(service, task, FIRST_RUNNER, true, None);
    broken.problem = Some(RunnerProblem {
        code: "workspace_unavailable".into(),
        message: "the workspace cannot be read".into(),
    });
    service
        .record_runner_snapshot(task, FIRST_RUNNER, 1, broken, grace())
        .expect("runner snapshot")
}

/// The wait a task at a gate gets before its slot is freed: long enough
/// that no test sees it.
fn grace() -> chrono::Duration {
    chrono::Duration::hours(1)
}

/// The snapshot [`report`] hands over: the next one of `incarnation`.
pub fn snapshot(
    service: &ProtocolService,
    task: &TaskId,
    incarnation: &str,
    execution: bool,
    waiting: Option<(&str, Option<&GateWatch>)>,
) -> RunnerSnapshot {
    let previous = service.control_plane().task(task).unwrap().runner;
    let sequence = previous
        .as_ref()
        .filter(|runner| runner.incarnation == incarnation)
        .map_or(1, |runner| runner.snapshot.snapshot_sequence + 1);
    let event_cursor = previous
        .as_ref()
        .map_or(1, |runner| runner.snapshot.event_cursor + 1);
    let (pending_input, gate_watch) = match waiting {
        Some((episode, gate)) => (Some(pending_input(episode, gate)), gate.cloned()),
        None => (None, None),
    };
    RunnerSnapshot {
        snapshot_sequence: sequence,
        execution_status: if pending_input.is_some() {
            ExecutionStatus::InputNeeded
        } else {
            ExecutionStatus::Running
        },
        execution_cursor: ExecutionCursor {
            execution_id: execution.then(|| "exec-1".to_string()),
            ..Default::default()
        },
        event_cursor,
        pending_input,
        gate_watch,
        conversation: Default::default(),
        timeline: None,
        safe_point: false,
        problem: None,
        attention: Vec::new(),
        conductor_available: false,
        conductor_unavailable_reason: None,
        default_harness_unavailable: None,
        workspaces: Vec::new(),
        generated_title: None,
        observed_at: Utc::now(),
    }
}

/// The pending input request of a gate in `episode`: the form a `decision`
/// gate raises, its choices and variables those of `gate` (none without
/// one).
fn pending_input(episode: &str, gate: Option<&GateWatch>) -> PendingInputRef {
    let outputs: Vec<_> = gate
        .map(|gate| gate.choices.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|choice| {
            json!({
                "label": choice.name,
                "description": choice.description,
                "requires_variables": choice.requires_variables,
            })
        })
        .collect();
    let fields: Vec<_> = gate
        .map(|gate| gate.variables.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|variable| {
            json!({
                "name": variable.name,
                "type": if variable.options.is_empty() { "text" } else { "select" },
                "options": variable.options,
                "required": false,
                "description": variable.description,
            })
        })
        .collect();
    let form = serde_json::from_value(json!({
        "execution_id": "exec-1",
        "state_id": "review_work",
        "action_execution_id": "act-1",
        "instruction": "Review the merge request",
        "fields": fields,
        "outputs": outputs,
        "continuation": "complete_step",
        "label_from_menu": true,
        "episode": episode,
        "requested_at": "2026-11-17T12:00:00Z",
    }))
    .expect("pending input form");
    PendingInputRef {
        id: episode.to_string(),
        attempt_id: None,
        form,
    }
}
