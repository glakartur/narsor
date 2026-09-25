//! AE1: taking an execution over is the same operation as recovering one.
//!
//! The proof is an equivalence: a run that is never interrupted and a run that
//! is interrupted after step k and taken over by a fresh process must reach the
//! same final state, for every k. Script actions only, so the comparison is
//! deterministic.

mod engine_support;

use engine_support::{workflow, Harness};
use narsor::domain::ExecutionStatus;

#[tokio::test]
async fn an_interrupted_run_ends_where_an_uninterrupted_one_does() {
    let definition = workflow("three_steps");

    let reference = {
        let harness = Harness::new();
        let engine = harness.engine().with_checkpoints();
        engine
            .start_and_run(&definition, serde_json::json!({}), harness.workspace())
            .await
            .expect("reference run")
    };
    assert_eq!(
        reference.status,
        ExecutionStatus::Completed,
        "reference run"
    );

    for interrupt_after in 1..=3 {
        let harness = Harness::new();
        let engine = harness.engine().with_checkpoints();

        let execution = engine
            .start(&definition, serde_json::json!({}), harness.workspace())
            .expect("start");

        let execution = engine
            .run_steps(&definition, &execution.id, interrupt_after)
            .await
            .expect("run up to the interruption");
        assert_eq!(
            execution.status,
            ExecutionStatus::Running,
            "the run must be left mid-workflow after step {interrupt_after}"
        );

        // The process dies here: a fresh store, a fresh engine, and a work tree
        // that is deliberately destroyed so nothing can be recovered from it.
        std::fs::remove_dir_all(&execution.workspace.path).expect("destroy the work tree");

        let taken_over =
            narsor::tasks::takeover(&harness.store(), &harness.provisioner(), &execution.id)
                .expect("take the execution over");

        let finished = harness
            .engine()
            .with_checkpoints()
            .run(&definition, &taken_over.id)
            .await
            .expect("finish after the takeover");

        assert_eq!(
            finished.status,
            ExecutionStatus::Completed,
            "takeover after step {interrupt_after} must finish the workflow"
        );
        assert_eq!(
            finished.context.variables, reference.context.variables,
            "takeover after step {interrupt_after} must reach the reference context"
        );
        assert_eq!(
            finished.current_state, reference.current_state,
            "takeover after step {interrupt_after} must reach the reference state"
        );

        // AE9: the handover mechanism pushes nothing to the product repository.
        assert!(
            harness.origin_refs().is_empty(),
            "takeover after step {interrupt_after} must leave the product repo untouched, found: {:?}",
            harness.origin_refs()
        );
    }
}
