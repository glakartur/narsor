//! Integration tests for reconciliation after a crash (U7, D23).
//!
//! A crash is simulated by writing exactly what a crashed process would have
//! left behind — a `state.json` that still says `Running` and an event log that
//! stops in the middle of a step — rather than by killing a child process,
//! because the interesting variable is *where* the log stops, and that is
//! easier to place precisely than a signal.

mod engine_support;

use std::sync::Arc;

use engine_support::{workflow, Harness};
use narsor_engine::conductor::assembly::assemble;
use narsor_engine::conductor::{
    ConductorInvoker, ConductorRegistry, ConductorState, ConductorStore, Invocation,
    InvocationOutcome, PendingInvocation, QueuedRequest, SavedDecision,
};
use narsor_engine::domain::{
    ConductorDecisionKind, ConductorPurpose, ConductorTrigger, Confidence, EventData,
    ExecutionStatus, RepromptedBy, WorkflowExecution,
};
use narsor_engine::engine::{
    owed_output_rescue, step_failure, ActionKind, Engine, EngineError, PendingHumanInput,
    PendingStep, Recovery, StepFailure,
};
use narsor_engine::persistence::{
    reconcile, reconcile_all, recover, sweep_orphaned_workspaces, OrphanReason,
    ReconciliationOutcome,
};
use narsor_engine::workspace::{
    CleanupPolicy, RepositorySpec, WorkspaceProvisioner, WorkspaceRequest,
};
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
    writer.append_narsor("the prompt", &[]).expect("append");
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

// ---------------------------------------------------------------------------
// The conductor's label rescue (F1, U10) across a restart
// ---------------------------------------------------------------------------

/// A conductor that must not be needed: it fails every invocation and
/// counts them, with the attempt each one was.
#[derive(Default)]
struct CountingConductor {
    attempts: std::sync::Mutex<Vec<u32>>,
}

impl ConductorInvoker for CountingConductor {
    fn invoke<'a>(
        &'a self,
        invocation: &'a Invocation,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = InvocationOutcome> + Send + 'a>> {
        self.attempts.lock().unwrap().push(invocation.attempt);
        Box::pin(async {
            InvocationOutcome::Failed {
                error: "the counting conductor fails".to_string(),
            }
        })
    }
}

/// An engine over `harness` whose agent must not run again, with a
/// conductor attached.
fn rescuing_engine(
    harness: &Harness,
    conductor: Arc<CountingConductor>,
) -> (
    Arc<Engine>,
    Arc<ConductorRegistry>,
    Arc<engine_support::StubAgentExecutor>,
) {
    let agent = Arc::new(engine_support::StubAgentExecutor::new(&[]));
    let engine = Arc::new(
        harness
            .engine()
            .with_executor(ActionKind::Agent, agent.clone()),
    );
    let registry = assemble(&engine, conductor, None);
    (engine, registry, agent)
}

/// Writes what a daemon that stopped during the rescue of `agent_problem`'s
/// `work` state left behind: the rejected attempt closed in the log, the
/// rescue recorded in the execution, still `Running`.
fn leave_rescue(harness: &Harness, engine: &Engine) -> WorkflowExecution {
    let mut execution = engine
        .start(&workflow("agent_problem"), json!({}), harness.workspace())
        .expect("start");
    let log = harness.store().event_log(&execution.id);
    for data in [
        EventData::StateEntered {
            state_id: "work".to_string(),
        },
        EventData::ActionStarted {
            action_execution_id: "attempt-rejected".to_string(),
            state_id: "work".to_string(),
            action_type: "agent".to_string(),
        },
        EventData::ActionOutputRejected {
            action_execution_id: "attempt-rejected".to_string(),
            state_id: "work".to_string(),
            reason: "no output label".to_string(),
            answer_tail: Some("Done, all green.".to_string()),
        },
    ] {
        log.append_next(&execution.id, data).expect("append");
    }
    execution.current_state = Some("work".to_string());
    execution.current_action_execution = Some("attempt-rejected".to_string());
    execution.metadata = json!({
        "engine": {
            "output_rescue": {
                "state_id": "work",
                "answers": [{
                    "action_execution_id": "attempt-rejected",
                    "reason": "no output label",
                    "answer_tail": "Done, all green.",
                }],
                "reason": "no output label",
            }
        }
    });
    harness
        .store()
        .save_execution(&execution)
        .expect("save the execution");
    execution
}

/// The choice (a) the conductor saved before the restart.
fn saved_choice() -> SavedDecision {
    SavedDecision {
        decision_id: "decision-saved".to_string(),
        kind: ConductorDecisionKind::ChooseOutput,
        episode: Some("attempt-rejected".to_string()),
        state_id: Some("work".to_string()),
        output_label: Some("implemented".to_string()),
        data: json!({ "units_completed": 3 }),
        variables: json!({ "summary": "three units" }),
        confidence: Confidence::High,
        rationale: Some("the agent said it was done".to_string()),
        context_before: json!({}),
        answers: Vec::new(),
        reason: None,
        reply: None,
        state_entries_before: serde_json::Value::Null,
        saved_at: chrono::Utc::now(),
    }
}

fn count_events(harness: &Harness, execution_id: &str, kind: &str) -> usize {
    harness
        .event_types(execution_id)
        .iter()
        .filter(|event| event.as_str() == kind)
        .count()
}

/// The window U5 left open — the reprompt's rejection recorded, the problem
/// not yet raised — is closed by the rescue's record: a restart there finds
/// the execution consistent and still owing the rescue.
#[tokio::test]
async fn a_restart_before_the_rescue_began_owes_the_rescue() {
    let harness = Harness::new();
    let engine = harness.engine();
    let execution = leave_rescue(&harness, &engine);

    let verdict = reconcile(&harness.store(), &execution.id).expect("reconcile");

    assert_eq!(verdict.outcome, ReconciliationOutcome::Consistent);
    let stored = harness.store().load_execution(&execution.id).expect("load");
    assert!(owed_output_rescue(&stored).expect("read").is_some());
}

/// Restart matrix: a choice saved before the restart, before its result was
/// recorded, is applied by the next pass without invoking the conductor.
#[tokio::test]
async fn a_restart_after_a_saved_choice_applies_it_without_asking_again() {
    let harness = Harness::new();
    let conductor = Arc::new(CountingConductor::default());
    let (engine, registry, agent) = rescuing_engine(&harness, conductor.clone());
    let execution = leave_rescue(&harness, &engine);
    let store = ConductorStore::new(harness.store().paths(), &execution.id);
    store
        .save_state(&ConductorState {
            unapplied_decisions: vec![saved_choice()],
            ..ConductorState::default()
        })
        .expect("save the decision");

    let verdict = reconcile(&harness.store(), &execution.id).expect("reconcile");
    assert_eq!(verdict.outcome, ReconciliationOutcome::Consistent);

    let finished = engine
        .run(&workflow("agent_problem"), &execution.id)
        .await
        .expect("carry the rescue on");

    assert_eq!(finished.status, ExecutionStatus::Completed);
    assert!(conductor.attempts.lock().unwrap().is_empty(), "not invoked");
    assert!(agent.prompts().is_empty(), "the agent did not run again");
    assert_eq!(
        count_events(&harness, &execution.id, "conductor_decision"),
        1
    );
    assert_eq!(count_events(&harness, &execution.id, "action_completed"), 2);
    let provider = harness
        .events(&execution.id)
        .into_iter()
        .find_map(|event| match event.data {
            EventData::DecisionMade { provider, .. } => Some(provider),
            _ => None,
        });
    assert_eq!(provider.as_deref(), Some("conductor"));
    assert_eq!(
        finished.context.variables.get("summary"),
        Some(&json!("three units"))
    );
    assert!(store
        .load_state()
        .expect("load")
        .unapplied_decisions
        .is_empty());
    drop(registry);
}

/// The same after the result was recorded (`ActionCompleted` and the
/// mid-step state): only the decision is left, still the conductor's.
#[tokio::test]
async fn a_restart_after_the_choices_result_takes_only_the_decision() {
    let harness = Harness::new();
    let conductor = Arc::new(CountingConductor::default());
    let (engine, _registry, _agent) = rescuing_engine(&harness, conductor.clone());
    let mut execution = leave_rescue(&harness, &engine);
    let decision = saved_choice();
    harness
        .store()
        .event_log(&execution.id)
        .append_next(
            &execution.id,
            EventData::ActionCompleted {
                action_execution_id: "attempt-rejected".to_string(),
                output_label: "implemented".to_string(),
            },
        )
        .expect("append");
    let pending = PendingStep {
        state_id: "work".to_string(),
        action_execution_id: "attempt-rejected".to_string(),
        result: narsor_engine::domain::ActionResult {
            output_label: "implemented".to_string(),
            data: decision.data.clone(),
            variables: decision.variables.as_object().cloned().unwrap_or_default(),
            ..Default::default()
        },
        artifacts: Vec::new(),
    };
    execution.metadata["engine"]["pending_step"] =
        serde_json::to_value(&pending).expect("serialize");
    harness.store().save_execution(&execution).expect("save");
    ConductorStore::new(harness.store().paths(), &execution.id)
        .save_state(&ConductorState {
            unapplied_decisions: vec![decision],
            ..ConductorState::default()
        })
        .expect("save the decision");

    let finished = engine
        .run(&workflow("agent_problem"), &execution.id)
        .await
        .expect("carry the rescue on");

    assert_eq!(finished.status, ExecutionStatus::Completed);
    assert!(conductor.attempts.lock().unwrap().is_empty());
    let completed: Vec<String> = harness
        .events(&execution.id)
        .into_iter()
        .filter_map(|event| match event.data {
            EventData::ActionCompleted {
                action_execution_id,
                ..
            } => Some(action_execution_id),
            _ => None,
        })
        .collect();
    assert_eq!(
        completed
            .iter()
            .filter(|id| *id == "attempt-rejected")
            .count(),
        1,
        "the result is not recorded twice: {completed:?}"
    );
    let provider = harness
        .events(&execution.id)
        .into_iter()
        .find_map(|event| match event.data {
            EventData::DecisionMade { provider, .. } => Some(provider),
            _ => None,
        });
    assert_eq!(provider.as_deref(), Some("conductor"));
}

/// Restart matrix: a restart during the conductor's invocation — counted
/// before it ran — is followed by one more invocation, R24's repeat; when
/// that one fails too, the step is a problem, and nothing more is invoked.
#[tokio::test]
async fn a_restart_during_the_invocation_invokes_the_conductor_once_more() {
    let harness = Harness::new();
    let conductor = Arc::new(CountingConductor::default());
    let (engine, _registry, agent) = rescuing_engine(&harness, conductor.clone());
    let execution = leave_rescue(&harness, &engine);
    let store = leave_conductor_marker(&harness, &execution, ConductorTrigger::OutputRescue);
    let mut state = store.load_state().expect("load");
    state
        .invocation_attempts
        .insert("attempt-rejected".to_string(), 1);
    store
        .save_state(&state)
        .expect("count the cut-short attempt");

    let verdict = reconcile(&harness.store(), &execution.id).expect("reconcile");
    assert!(matches!(
        verdict.outcome,
        ReconciliationOutcome::ConductorInterrupted { .. }
    ));

    let stopped = engine
        .run(&workflow("agent_problem"), &execution.id)
        .await
        .expect("carry the rescue on");

    assert_eq!(*conductor.attempts.lock().unwrap(), vec![2]);
    assert!(agent.prompts().is_empty());
    assert_eq!(stopped.status, ExecutionStatus::InputNeeded);
    let pending = harness
        .store()
        .load_pending_input::<PendingHumanInput>(&execution.id)
        .expect("read")
        .expect("a problem");
    let problem = pending.problem.expect("a problem");
    assert_eq!(problem.analysis, None);
    assert!(problem.reason.contains("the counting conductor fails"));
    let left = store.load_state().expect("load");
    assert_eq!(left.pending, None, "{left:?}");
    assert!(left.queue.is_empty(), "the stale request is gone: {left:?}");
}

/// A rescue whose invocations were all spent before the restart is not
/// invoked again: it is a problem straight away.
#[tokio::test]
async fn a_rescue_whose_invocations_are_spent_is_a_problem() {
    let harness = Harness::new();
    let conductor = Arc::new(CountingConductor::default());
    let (engine, _registry, _agent) = rescuing_engine(&harness, conductor.clone());
    let execution = leave_rescue(&harness, &engine);
    let store = leave_conductor_marker(&harness, &execution, ConductorTrigger::OutputRescue);
    let mut state = store.load_state().expect("load");
    state
        .invocation_attempts
        .insert("attempt-rejected".to_string(), 2);
    store.save_state(&state).expect("count both attempts");

    let stopped = engine
        .run(&workflow("agent_problem"), &execution.id)
        .await
        .expect("carry the rescue on");

    assert!(conductor.attempts.lock().unwrap().is_empty());
    assert_eq!(stopped.status, ExecutionStatus::InputNeeded);
    assert_eq!(count_events(&harness, &execution.id, "problem_raised"), 1);
}

/// A recovery after the conductor's reprompt was cut short drops the rescue:
/// the state's action starts afresh.
#[tokio::test]
async fn a_recovery_drops_the_rescue() {
    let harness = Harness::new();
    let engine = Arc::new(harness.engine());
    let execution = leave_rescue(&harness, &engine);
    harness
        .store()
        .event_log(&execution.id)
        .append_next(
            &execution.id,
            EventData::OutputReprompted {
                action_execution_id: "attempt-conductor-reprompt".to_string(),
                previous_attempt: "attempt-rejected".to_string(),
                state_id: "work".to_string(),
                by: RepromptedBy::Conductor,
            },
        )
        .expect("append");
    let verdict = reconcile(&harness.store(), &execution.id).expect("reconcile");
    assert_eq!(verdict.status, ExecutionStatus::Unknown);

    let handle = engine
        .try_begin_recover(&execution.id, None)
        .expect("begin the recovery");
    drop(handle);
    let stored = harness.store().load_execution(&execution.id).expect("load");
    assert!(owed_output_rescue(&stored).expect("read").is_none());
}

// ---------------------------------------------------------------------------
// A failed step the conductor took up (F3, U11)
// ---------------------------------------------------------------------------

/// Leaves `execution` failed on a step failure the conductor took up, as
/// the pass that failed saves it: the status and the record in one save.
fn fail_for_the_conductor(harness: &Harness, execution: &mut WorkflowExecution, key: &str) {
    let failure = StepFailure {
        state_id: "first".to_string(),
        action_execution_id: key.to_string(),
        episode: key.to_string(),
        error: "the build broke".to_string(),
        side_effect_free: true,
        conductor_retry: false,
    };
    execution.status = ExecutionStatus::Failed;
    execution.current_state = Some("first".to_string());
    execution.metadata = json!({ "engine": { "step_failure": failure } });
    harness
        .store()
        .save_execution(execution)
        .expect("save the execution");
}

/// The sweep keeps the workspace of an execution failed on a step the
/// conductor took up — a retry, confirmed or its own, needs it — and sweeps
/// that of one failed with nothing for the conductor, as before.
#[tokio::test]
async fn the_sweep_keeps_the_workspace_of_a_failure_the_conductor_took_up() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("two_steps");
    let mut diagnosed = engine
        .start(&definition, json!({}), harness.workspace())
        .expect("start one");
    fail_for_the_conductor(&harness, &mut diagnosed, "attempt-1");
    let mut plain = engine
        .start(&definition, json!({}), harness.workspace())
        .expect("start another");
    plain.status = ExecutionStatus::Failed;
    harness.store().save_execution(&plain).expect("save");

    let provisioner = WorkspaceProvisioner::new(harness.root().join("workspaces"));
    for execution_id in [diagnosed.id.as_str(), plain.id.as_str()] {
        std::fs::create_dir_all(provisioner.task_dir(execution_id)).expect("a workspace");
    }

    let orphans = sweep_orphaned_workspaces(&harness.store(), &provisioner, CleanupPolicy::Remove)
        .expect("sweep");

    let ids: Vec<&str> = orphans
        .iter()
        .map(|orphan| orphan.execution_id.as_str())
        .collect();
    assert_eq!(ids, vec![plain.id.as_str()]);
    assert!(provisioner.task_dir(&diagnosed.id).exists());
    assert!(!provisioner.task_dir(&plain.id).exists());
}

/// KTD9: a recovery naming a failure the execution is no longer failed on
/// is refused and changes nothing; one naming the current failure clears
/// the record in the same save as the status change.
#[tokio::test]
async fn a_recovery_names_the_failure_it_answers() {
    let harness = Harness::new();
    let engine = Arc::new(harness.engine());
    let mut execution = engine
        .start(&workflow("two_steps"), json!({}), harness.workspace())
        .expect("start");
    fail_for_the_conductor(&harness, &mut execution, "attempt-2");
    let events_before = harness.events(&execution.id).len();

    let stale = engine.try_begin_recover_with(
        &execution.id,
        Recovery {
            comment: None,
            episode: Some("attempt-1".to_string()),
            conductor_retry: false,
        },
    );
    assert!(
        matches!(
            stale,
            Err(EngineError::StaleEpisode { ref expected, ref current, .. })
                if expected == "attempt-1" && current.as_deref() == Some("attempt-2")
        ),
        "{:?}",
        stale.err()
    );
    let stored = harness.store().load_execution(&execution.id).expect("load");
    assert_eq!(stored.status, ExecutionStatus::Failed);
    assert!(step_failure(&stored).expect("read").is_some());
    assert_eq!(
        harness.events(&execution.id).len(),
        events_before,
        "a stale recovery records nothing"
    );

    let handle = engine
        .try_begin_recover_with(
            &execution.id,
            Recovery {
                comment: Some("once more".to_string()),
                episode: Some("attempt-2".to_string()),
                conductor_retry: true,
            },
        )
        .expect("the current failure is recovered");
    drop(handle);
    let stored = harness.store().load_execution(&execution.id).expect("load");
    assert_eq!(stored.status, ExecutionStatus::Running);
    assert!(stored.metadata["engine"].get("step_failure").is_none());
    assert_eq!(
        stored.metadata["engine"]["conductor_retry"],
        json!({ "state_id": "first", "episode": "attempt-2" }),
        "the conductor's retry keeps the failure's episode"
    );

    // Once recovered, the same key is stale.
    let again = engine.try_begin_recover_with(
        &execution.id,
        Recovery {
            comment: None,
            episode: Some("attempt-2".to_string()),
            conductor_retry: false,
        },
    );
    assert!(matches!(again, Err(EngineError::StaleEpisode { .. })));
}

// ---------------------------------------------------------------------------
// Characterization for the portable runner (U3)
//
// The remote runner reuses exactly these rules after an unexpected loss of
// its workload, and only once the owner decided: nothing reconciles by
// itself, reconciliation alone never re-runs anything, and an explicit
// recovery starts the interrupted action as a new attempt while the event
// log simply continues.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn loading_a_crashed_execution_does_not_reconcile_it() {
    let harness = Harness::new();
    let engine = harness.engine();
    let definition = workflow("two_steps");
    let execution = engine
        .start(&definition, json!({}), harness.workspace())
        .expect("start");
    interrupt_during_action(&harness, &execution);

    // A fresh engine over the same store — what a new process sees.
    let reopened = Engine::new(harness.store());
    let loaded = reopened
        .store()
        .load_execution(&execution.id)
        .expect("load");
    assert_eq!(loaded.status, ExecutionStatus::Running);
    assert!(
        !harness
            .event_types(&execution.id)
            .iter()
            .any(|kind| kind == "error"),
        "opening the store wrote nothing"
    );
}

#[tokio::test]
async fn an_explicit_recovery_runs_the_interrupted_action_again_as_a_new_attempt() {
    let harness = Harness::new();
    let engine = Arc::new(harness.engine());
    let definition = workflow("two_steps");
    let execution = engine
        .start(&definition, json!({}), harness.workspace())
        .expect("start");
    let interrupted = interrupt_during_action(&harness, &execution);
    let before = harness.events(&execution.id).len() as u64;

    let reconciliation = reconcile(&harness.store(), &execution.id).expect("reconcile");
    assert_eq!(reconciliation.status, ExecutionStatus::Unknown);

    let finished = engine
        .try_begin_recover_with(
            &execution.id,
            Recovery::with_comment(Some("owner resumed".to_string())),
        )
        .expect("an unknown execution can be recovered explicitly")
        .run_to_completion(&definition)
        .await
        .expect("the recovered execution runs on");
    assert_eq!(finished.status, ExecutionStatus::Completed);

    let events = harness.events(&execution.id);
    let first_attempts: Vec<String> = events
        .iter()
        .filter_map(|event| match &event.data {
            EventData::ActionStarted {
                action_execution_id,
                state_id,
                ..
            } if state_id == "first" => Some(action_execution_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(first_attempts.len(), 2, "exactly one more attempt");
    assert_eq!(first_attempts[0], interrupted);
    assert_ne!(first_attempts[1], interrupted, "a new idempotency key");

    // The log only grew, without gaps: a cursor taken before the loss is
    // still a valid position after it.
    let sequences: Vec<u64> = events.iter().map(|event| event.sequence).collect();
    assert!(sequences.windows(2).all(|pair| pair[1] == pair[0] + 1));
    assert!(*sequences.last().unwrap() > before);
}
