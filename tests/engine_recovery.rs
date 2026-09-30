//! Integration tests for reconciliation after a crash (U7, D23).
//!
//! A crash is simulated by writing exactly what a crashed process would have
//! left behind — a `state.json` that still says `Running` and an event log that
//! stops in the middle of a step — rather than by killing a child process,
//! because the interesting variable is *where* the log stops, and that is
//! easier to place precisely than a signal.

mod engine_support;

use engine_support::{workflow, Harness};
use narsor::conductor::{ConductorState, ConductorStore, PendingInvocation, QueuedRequest};
use narsor::domain::{
    ConductorPurpose, ConductorTrigger, EventData, ExecutionStatus, RepromptedBy, WorkflowExecution,
};
use narsor::engine::{EngineError, PendingStep};
use narsor::persistence::{
    reconcile, reconcile_all, recover, sweep_orphaned_workspaces, OrphanReason,
    ReconciliationOutcome,
};
use narsor::workspace::{CleanupPolicy, RepositorySpec, WorkspaceProvisioner, WorkspaceRequest};
use serde_json::json;

/// Writes the log of an execution that died while its first action ran.
fn interrupt_during_action(harness: &Harness, execution: &WorkflowExecution) -> String {
    let log = harness.store().event_log(&execution.id);
    log.append_next(
        &execution.id,
        EventData::StateEntered {
            state_id: "first".to_string(),
        },
    )
    .expect("append StateEntered");

    let action_execution_id = "action-under-way".to_string();
    log.append_next(
        &execution.id,
        EventData::ActionStarted {
            action_execution_id: action_execution_id.clone(),
            state_id: "first".to_string(),
            action_type: "script".to_string(),
        },
    )
    .expect("append ActionStarted");

    action_execution_id
}

#[tokio::test]
async fn an_action_that_never_finished_leaves_the_execution_unknown() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("two_steps");

    let execution = engine
        .start(&definition, json!({}), harness.workspace())
        .expect("start");
    let action_execution_id = interrupt_during_action(&harness, &execution);

    let reconciliation = reconcile(&harness.store(), &execution.id).expect("reconcile");

    assert_eq!(reconciliation.status, ExecutionStatus::Unknown);
    assert_eq!(
        reconciliation.outcome,
        ReconciliationOutcome::InterruptedAction {
            state_id: "first".to_string(),
            action_execution_id: action_execution_id.clone(),
        }
    );
    assert!(reconciliation.outcome.needs_attention());

    // The reason is in the history, naming the attempt that was in flight.
    let message = harness
        .events(&execution.id)
        .into_iter()
        .find_map(|event| match event.data {
            EventData::Error { message, .. } => Some(message),
            _ => None,
        })
        .expect("an Error event");
    assert!(message.contains(&action_execution_id), "{message}");
    assert!(message.contains("will not be replayed"), "{message}");

    // And the status on disk is Unknown.
    let stored = harness
        .store()
        .load_execution(&execution.id)
        .expect("load the execution");
    assert_eq!(stored.status, ExecutionStatus::Unknown);
}

/// Writes the log of an execution that died during the backoff sleep between
/// two attempts: the failed attempt got its `ActionAttemptFailed`, but the
/// process died before `ActionRetrying` was appended.
fn interrupt_during_backoff(harness: &Harness, execution: &WorkflowExecution) -> String {
    let log = harness.store().event_log(&execution.id);
    log.append_next(
        &execution.id,
        EventData::StateEntered {
            state_id: "first".to_string(),
        },
    )
    .expect("append StateEntered");

    let action_execution_id = "action-under-way".to_string();
    log.append_next(
        &execution.id,
        EventData::ActionStarted {
            action_execution_id: action_execution_id.clone(),
            state_id: "first".to_string(),
            action_type: "script".to_string(),
        },
    )
    .expect("append ActionStarted");

    log.append_next(
        &execution.id,
        EventData::ActionAttemptFailed {
            action_execution_id: action_execution_id.clone(),
            state_id: "first".to_string(),
            error: "the script exited nonzero".to_string(),
            retry_in_ms: 1000,
        },
    )
    .expect("append ActionAttemptFailed");

    action_execution_id
}

#[tokio::test]
async fn a_crash_during_backoff_still_leaves_the_execution_unknown() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("two_steps");

    let execution = engine
        .start(&definition, json!({}), harness.workspace())
        .expect("start");
    let action_execution_id = interrupt_during_backoff(&harness, &execution);

    let reconciliation = reconcile(&harness.store(), &execution.id).expect("reconcile");

    // `ActionAttemptFailed` is not a closure: the attempt it names is still
    // open, exactly as if it had only just started, so the outcome is
    // identical to a crash right after `ActionStarted` (D23).
    assert_eq!(reconciliation.status, ExecutionStatus::Unknown);
    assert_eq!(
        reconciliation.outcome,
        ReconciliationOutcome::InterruptedAction {
            state_id: "first".to_string(),
            action_execution_id: action_execution_id.clone(),
        }
    );
    assert!(reconciliation.outcome.needs_attention());

    let message = harness
        .events(&execution.id)
        .into_iter()
        .find_map(|event| match event.data {
            EventData::Error { message, .. } => Some(message),
            _ => None,
        })
        .expect("an Error event");
    assert!(message.contains(&action_execution_id), "{message}");
    assert!(message.contains("will not be replayed"), "{message}");

    let stored = harness
        .store()
        .load_execution(&execution.id)
        .expect("load the execution");
    assert_eq!(stored.status, ExecutionStatus::Unknown);
}

#[tokio::test]
async fn the_engine_does_not_replay_an_interrupted_action_by_itself() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("two_steps");

    let execution = engine
        .start(&definition, json!({}), harness.workspace())
        .expect("start");
    interrupt_during_action(&harness, &execution);
    reconcile(&harness.store(), &execution.id).expect("reconcile");

    let error = engine
        .run(&definition, &execution.id)
        .await
        .expect_err("an unknown execution is not runnable");
    let message = error.to_string();
    assert!(message.contains("unknown"), "{message}");
    assert!(
        matches!(error, EngineError::NotRunnable { .. }),
        "{error:?}"
    );

    // Nothing was started a second time.
    let starts = harness
        .event_types(&execution.id)
        .iter()
        .filter(|kind| kind.as_str() == "action_started")
        .count();
    assert_eq!(starts, 1);
}

#[tokio::test]
async fn a_crash_after_the_action_resumes_at_the_decision() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("two_steps");

    let mut execution = engine
        .start(&definition, json!({}), harness.workspace())
        .expect("start");

    // Exactly what the engine would have written before dying between
    // `ActionCompleted` and `TransitionTaken`.
    let log = harness.store().event_log(&execution.id);
    log.append_next(
        &execution.id,
        EventData::StateEntered {
            state_id: "first".to_string(),
        },
    )
    .expect("append");
    log.append_next(
        &execution.id,
        EventData::ActionStarted {
            action_execution_id: "action-1".to_string(),
            state_id: "first".to_string(),
            action_type: "script".to_string(),
        },
    )
    .expect("append");
    log.append_next(
        &execution.id,
        EventData::ActionCompleted {
            action_execution_id: "action-1".to_string(),
            output_label: "done".to_string(),
        },
    )
    .expect("append");

    let pending = PendingStep {
        state_id: "first".to_string(),
        action_execution_id: "action-1".to_string(),
        result: serde_json::from_value(json!({
            "output_label": "done",
            "variables": { "first": "from the crashed step" },
        }))
        .expect("an action result"),
        artifacts: Vec::new(),
    };
    execution.metadata = json!({ "engine": { "pending_step": pending } });
    harness
        .store()
        .save_execution(&execution)
        .expect("save the mid-step state");

    // Reconciliation sees a finished action and leaves the execution runnable.
    let reconciliation = reconcile(&harness.store(), &execution.id).expect("reconcile");
    assert_eq!(reconciliation.outcome, ReconciliationOutcome::Consistent);

    let execution = engine
        .run(&definition, &execution.id)
        .await
        .expect("run on from the decision");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(
        execution.context.variables.get("first"),
        Some(&json!("from the crashed step")),
        "the variables of the finished action were applied, not recomputed"
    );

    let types = harness.event_types(&execution.id);
    assert_eq!(
        types
            .iter()
            .filter(|kind| kind.as_str() == "action_started")
            .count(),
        2,
        "the first state's action was not run again; only the second state's was"
    );
    assert_eq!(
        types[4..8].to_vec(),
        vec![
            "decision_made",
            "transition_taken",
            "state_entered",
            "action_started"
        ],
        "the interrupted step carried on at its decision"
    );
}

#[tokio::test]
async fn an_execution_needing_input_is_left_alone() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("human_decision");

    let execution = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run up to the question");
    assert_eq!(execution.status, ExecutionStatus::InputNeeded);

    let reconciliation = reconcile(&harness.store(), &execution.id).expect("reconcile");
    assert_eq!(
        reconciliation.outcome,
        ReconciliationOutcome::NotApplicable {
            status: ExecutionStatus::InputNeeded
        }
    );

    // Still answerable afterwards.
    let stored = harness.store().load_execution(&execution.id).expect("load");
    assert_eq!(stored.status, ExecutionStatus::InputNeeded);
    assert!(engine
        .pending_input(&execution.id)
        .expect("pending input")
        .is_some());
}

#[tokio::test]
async fn recovery_reports_every_execution_and_clears_interrupted_writes() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("two_steps");

    let finished = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run one to the end");
    let crashed = engine
        .start(&definition, json!({}), harness.workspace())
        .expect("start another");
    interrupt_during_action(&harness, &crashed);

    // The trace of a write that never got renamed.
    let leftover = harness
        .store()
        .execution_dir(&finished.id)
        .join("state.json.tmp");
    std::fs::write(&leftover, b"half a document").expect("write a leftover");

    let report = recover(&harness.store()).expect("recover");

    assert!(report.removed_temp_files.contains(&leftover));
    assert!(!leftover.exists());
    assert_eq!(report.reconciled.len(), 2);

    let attention: Vec<&str> = report
        .needing_attention()
        .map(|entry| entry.execution_id.as_str())
        .collect();
    assert_eq!(attention, vec![crashed.id.as_str()]);

    let finished_entry = report
        .reconciled
        .iter()
        .find(|entry| entry.execution_id == finished.id)
        .expect("the finished execution was examined");
    assert_eq!(
        finished_entry.outcome,
        ReconciliationOutcome::NotApplicable {
            status: ExecutionStatus::Completed
        }
    );

    // Reconciling twice is harmless: the second pass finds nothing running.
    let again = reconcile_all(&harness.store()).expect("reconcile again");
    assert!(again.iter().all(|entry| !entry.outcome.needs_attention()));
}

/// Writes the log of an agent attempt whose output was rejected (KTD7), and
/// returns the rejected attempt's id.
fn reject_output(harness: &Harness, execution: &WorkflowExecution) -> String {
    let log = harness.store().event_log(&execution.id);
    let rejected = "attempt-rejected".to_string();
    for data in [
        EventData::StateEntered {
            state_id: "first".to_string(),
        },
        EventData::ActionStarted {
            action_execution_id: rejected.clone(),
            state_id: "first".to_string(),
            action_type: "agent".to_string(),
        },
        EventData::ActionOutputRejected {
            action_execution_id: rejected.clone(),
            state_id: "first".to_string(),
            reason: "no output label".to_string(),
            answer_tail: Some("Done.".to_string()),
        },
    ] {
        log.append_next(&execution.id, data).expect("append");
    }
    rejected
}

/// Restart matrix: a rejected output closes its attempt, so a daemon that
/// stopped before the reprompt started finds nothing in flight.
#[tokio::test]
async fn a_restart_after_a_rejected_output_is_not_an_interrupted_action() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("two_steps");
    let execution = engine
        .start(&definition, json!({}), harness.workspace())
        .expect("start");
    reject_output(&harness, &execution);

    let report = recover(&harness.store()).expect("recover");

    let verdict = report
        .reconciled
        .iter()
        .find(|entry| entry.execution_id == execution.id)
        .expect("the execution was reconciled");
    assert_eq!(verdict.outcome, ReconciliationOutcome::Consistent);
    assert_eq!(verdict.status, ExecutionStatus::Running);
    let stored = harness
        .store()
        .load_execution(&execution.id)
        .expect("load the execution");
    assert_eq!(stored.status, ExecutionStatus::Running);
}

/// Restart matrix: a daemon that stopped while the reprompt ran leaves an
/// interrupted action — the resumed session may have done anything.
#[tokio::test]
async fn a_restart_during_a_reprompt_leaves_the_execution_unknown() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("two_steps");
    let execution = engine
        .start(&definition, json!({}), harness.workspace())
        .expect("start");
    let rejected = reject_output(&harness, &execution);
    harness
        .store()
        .event_log(&execution.id)
        .append_next(
            &execution.id,
            EventData::OutputReprompted {
                action_execution_id: "attempt-reprompt".to_string(),
                previous_attempt: rejected,
                state_id: "first".to_string(),
                by: RepromptedBy::Engine,
            },
        )
        .expect("append OutputReprompted");

    let report = recover(&harness.store()).expect("recover");

    let verdict = report
        .reconciled
        .iter()
        .find(|entry| entry.execution_id == execution.id)
        .expect("the execution was reconciled");
    assert_eq!(verdict.status, ExecutionStatus::Unknown);
    assert_eq!(
        verdict.outcome,
        ReconciliationOutcome::InterruptedAction {
            state_id: "first".to_string(),
            action_execution_id: "attempt-reprompt".to_string(),
        }
    );
}

/// The same sweep while the execution keeps running: only the attempt the
/// log closed with `ActionOutputRejected` is compressed.
#[tokio::test]
async fn a_running_executions_rejected_attempt_counts_as_closed() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("two_steps");
    let execution = engine
        .start(&definition, json!({}), harness.workspace())
        .expect("start");
    let rejected = reject_output(&harness, &execution);

    let conversations = harness.store().conversation_store(&execution.id);
    let mut writer = conversations.writer(&rejected).expect("open a writer");
    writer.append_narsor("the prompt").expect("append");
    writer.finish().expect("finish the writer");

    let report = recover(&harness.store()).expect("recover");

    let stored = harness
        .store()
        .load_execution(&execution.id)
        .expect("load the execution");
    assert_eq!(stored.status, ExecutionStatus::Running);
    assert_eq!(
        report.compressed_conversations,
        vec![(execution.id.clone(), rejected.clone())]
    );
}

#[tokio::test]
async fn workspaces_without_a_live_execution_are_swept() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("two_steps");

    let provisioner = WorkspaceProvisioner::new(harness.root().join("workspaces"));
    let source = harness.root().join("source-repo");
    std::fs::create_dir_all(source.join("src")).expect("create the source repository");
    std::fs::write(source.join("src/main.rs"), b"fn main() {}").expect("write a file");
    let request = WorkspaceRequest::new(RepositorySpec::local("app", &source));

    // One workspace of an execution that finished, one of an execution that is
    // still running, and one of an execution that does not exist at all.
    let finished = engine
        .start_and_run(&definition, json!({}), harness.workspace())
        .await
        .expect("run one to the end");
    let running = engine
        .start(&definition, json!({}), harness.workspace())
        .expect("start another");

    for execution_id in [finished.id.as_str(), running.id.as_str(), "ghost-execution"] {
        provisioner
            .provision(execution_id, &request)
            .expect("provision a workspace");
    }

    let orphans = sweep_orphaned_workspaces(&harness.store(), &provisioner, CleanupPolicy::Remove)
        .expect("sweep");

    let ids: Vec<&str> = orphans
        .iter()
        .map(|orphan| orphan.execution_id.as_str())
        .collect();
    let mut expected = vec![finished.id.as_str(), "ghost-execution"];
    expected.sort_unstable();
    assert_eq!(ids, expected);

    for orphan in &orphans {
        assert!(orphan.cleanup.is_removed(), "{orphan:?}");
        assert!(!orphan.path.exists());
    }
    assert!(orphans
        .iter()
        .any(|orphan| orphan.reason == OrphanReason::NoExecution));
    assert!(orphans.iter().any(|orphan| matches!(
        orphan.reason,
        OrphanReason::ExecutionFinished {
            status: ExecutionStatus::Completed
        }
    )));

    // The running execution kept its workspace.
    assert!(provisioner.task_dir(&running.id).exists());
}

#[tokio::test]
async fn a_sweep_can_report_without_deleting() {
    let harness = Harness::new();
    let provisioner = WorkspaceProvisioner::new(harness.root().join("workspaces"));
    let source = harness.root().join("source-repo");
    std::fs::create_dir_all(&source).expect("create the source repository");
    std::fs::write(source.join("README.md"), b"hello").expect("write a file");

    provisioner
        .provision(
            "ghost-execution",
            &WorkspaceRequest::new(RepositorySpec::local("app", &source)),
        )
        .expect("provision");

    let orphans =
        sweep_orphaned_workspaces(&harness.store(), &provisioner, CleanupPolicy::Preserve)
            .expect("sweep");

    assert_eq!(orphans.len(), 1);
    assert!(!orphans[0].cleanup.is_removed());
    assert!(provisioner.task_dir("ghost-execution").exists());
}

/// Writes the marker of a conductor invocation that was under way for
/// `trigger` when the daemon stopped.
fn leave_conductor_marker(
    harness: &Harness,
    execution: &WorkflowExecution,
    trigger: ConductorTrigger,
) -> ConductorStore {
    let store = ConductorStore::new(harness.store().paths(), &execution.id);
    let request = QueuedRequest::new(trigger, ConductorPurpose::ChooseOutput)
        .with_episode("attempt-rejected");
    let mut state = ConductorState {
        pending: Some(PendingInvocation {
            invocation_id: "invocation-cut-short".to_string(),
            request: request.clone(),
            started_at: chrono::Utc::now(),
        }),
        ..ConductorState::default()
    };
    state.queue.push(request);
    store.save_state(&state).expect("save the conductor state");
    store
}

/// Restart matrix: a daemon that stopped while the conductor chose an
/// output (F1) left no attempt open — the execution stays `Running`, not
/// `Unknown`, and the marker stays for the conductor to carry on.
#[tokio::test]
async fn a_restart_during_the_conductors_rescue_is_not_an_interrupted_action() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("two_steps");
    let execution = engine
        .start(&definition, json!({}), harness.workspace())
        .expect("start");
    reject_output(&harness, &execution);
    let conductor = leave_conductor_marker(&harness, &execution, ConductorTrigger::OutputRescue);

    let report = recover(&harness.store()).expect("recover");

    let verdict = report
        .reconciled
        .iter()
        .find(|entry| entry.execution_id == execution.id)
        .expect("the execution was reconciled");
    assert_eq!(verdict.status, ExecutionStatus::Running);
    assert_eq!(
        verdict.outcome,
        ReconciliationOutcome::ConductorInterrupted {
            trigger: ConductorTrigger::OutputRescue,
            invocation_id: "invocation-cut-short".to_string(),
        }
    );
    assert!(!verdict.outcome.needs_attention());
    let stored = harness
        .store()
        .load_execution(&execution.id)
        .expect("load the execution");
    assert_eq!(stored.status, ExecutionStatus::Running);
    assert!(conductor.load_state().expect("load").pending.is_some());
}

/// Restart matrix: the conductor's own reprompt was running — that is an
/// attempt, so the execution is `Unknown`, and the marker goes with it.
#[tokio::test]
async fn a_restart_during_the_conductors_reprompt_leaves_the_execution_unknown() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("two_steps");
    let execution = engine
        .start(&definition, json!({}), harness.workspace())
        .expect("start");
    let rejected = reject_output(&harness, &execution);
    harness
        .store()
        .event_log(&execution.id)
        .append_next(
            &execution.id,
            EventData::OutputReprompted {
                action_execution_id: "attempt-conductor-reprompt".to_string(),
                previous_attempt: rejected,
                state_id: "first".to_string(),
                by: RepromptedBy::Conductor,
            },
        )
        .expect("append OutputReprompted");
    let conductor = leave_conductor_marker(&harness, &execution, ConductorTrigger::OutputRescue);

    let report = recover(&harness.store()).expect("recover");

    let verdict = report
        .reconciled
        .iter()
        .find(|entry| entry.execution_id == execution.id)
        .expect("the execution was reconciled");
    assert_eq!(verdict.status, ExecutionStatus::Unknown);
    assert!(verdict.outcome.needs_attention());
    let state = conductor.load_state().expect("load");
    assert_eq!(state.pending, None, "the marker is cleared");
}

/// A chat invocation cut short is no reason to treat the execution
/// differently: without an open attempt it is simply consistent.
#[tokio::test]
async fn a_restart_during_a_chat_answer_leaves_the_execution_consistent() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("two_steps");
    let execution = engine
        .start(&definition, json!({}), harness.workspace())
        .expect("start");
    leave_conductor_marker(&harness, &execution, ConductorTrigger::Chat);

    let verdict = reconcile(&harness.store(), &execution.id).expect("reconcile");

    assert_eq!(verdict.outcome, ReconciliationOutcome::Consistent);
    assert_eq!(verdict.status, ExecutionStatus::Running);
}
