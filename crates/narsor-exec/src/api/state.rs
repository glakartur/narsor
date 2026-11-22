//! The complete state of an execution, as a runner or a read-only runner
//! answers a relayed state read with it.

use narsor_protocol::controller::{ExecutionState, StateSection};
use serde::Serialize;
use serde_json::Value;

use crate::conductor::ConductorStore;
use crate::engine::runner::all_step_locals;
use crate::persistence::{ExecutionStore, PersistenceError};

pub const NO_CONDUCTOR_STATE: &str = "no conductor state";

/// Every value the workflow of `execution_id` sees now, read from `store`.
///
/// The context and the step locals come from one load of the execution
/// record, the authority over both; the conductor's state from its own
/// store. Only the execution record must load: a section that does not is
/// reported as unreadable, never as empty.
pub fn execution_state(
    store: &ExecutionStore,
    execution_id: &str,
) -> Result<ExecutionState, PersistenceError> {
    let execution = store.load_execution(execution_id)?;
    let context = section_of(&execution.context);
    let locals = match all_step_locals(&execution) {
        Ok(locals) => section_of(&locals),
        Err(error) => StateSection::Unreadable {
            reason: error.to_string(),
        },
    };
    Ok(ExecutionState {
        context,
        locals,
        conductor: conductor_section(&ConductorStore::new(store.paths(), execution_id)),
    })
}

fn conductor_section(conductor: &ConductorStore) -> StateSection {
    if !conductor.exists() {
        return StateSection::Absent {
            message: NO_CONDUCTOR_STATE.to_string(),
        };
    }
    let read = || -> Result<Value, String> {
        let state = conductor.load_state().map_err(|error| error.to_string())?;
        let mut value = serde_json::to_value(state).map_err(|error| error.to_string())?;
        let title = conductor
            .load_generated_title()
            .map_err(|error| error.to_string())?;
        if let (Some(title), Some(fields)) = (title, value.as_object_mut()) {
            fields.insert("title".to_string(), Value::String(title));
        }
        Ok(value)
    };
    match read() {
        Ok(value) => StateSection::Available { value },
        Err(reason) => StateSection::Unreadable { reason },
    }
}

fn section_of(value: &impl Serialize) -> StateSection {
    match serde_json::to_value(value) {
        Ok(value) => StateSection::Available { value },
        Err(error) => StateSection::Unreadable {
            reason: error.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::Utc;
    use serde_json::json;

    use super::*;
    use crate::conductor::{
        ConductorState, FailureOutcome, FailureReport, PendingInvocation, QueuedRequest,
    };
    use crate::domain::{
        ArtifactReference, ConductorPurpose, ConductorTrigger, Context, ExecutionStatus,
        WorkflowExecution, Workspace,
    };
    use crate::engine::{METADATA_ENGINE_KEY, METADATA_STEP_LOCALS_KEY};

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("narsor-state-{}", uuid::Uuid::new_v4())))
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn saved_execution(store: &ExecutionStore) -> WorkflowExecution {
        let mut context = Context::new();
        context
            .variables
            .insert("plan".to_string(), json!("line one\nline two"));
        context.artifacts.insert(
            "report".to_string(),
            ArtifactReference {
                id: "report".to_string(),
                name: None,
                location: "out/report.md".to_string(),
                metadata: json!({}),
            },
        );
        let execution = WorkflowExecution {
            id: "exec-1".to_string(),
            workflow_id: "flow".to_string(),
            workflow_version: 1,
            status: ExecutionStatus::Failed,
            current_state: Some("review".to_string()),
            started_at: Utc::now(),
            completed_at: None,
            inputs: json!({}),
            context,
            workspace: Workspace::local_folder("/tmp/none".to_string()),
            current_action_execution: None,
            metadata: json!({ METADATA_ENGINE_KEY: { METADATA_STEP_LOCALS_KEY: {
                "draft": { "TRIES": 2 },
                "review": { "NOTE": "keep it short" },
            }}}),
        };
        store.create_execution_dirs(&execution.id).unwrap();
        store.save_execution(&execution).unwrap();
        execution
    }

    fn request(purpose: ConductorPurpose) -> QueuedRequest {
        QueuedRequest {
            request_id: "req-1".to_string(),
            trigger: ConductorTrigger::StepFailure,
            purpose,
            episode: Some("review#1".to_string()),
            payload: json!({ "error": "boom" }),
            queued_at: Utc::now(),
        }
    }

    fn value(section: &StateSection) -> &Value {
        match section {
            StateSection::Available { value } => value,
            other => panic!("expected an available section, got {other:?}"),
        }
    }

    #[test]
    // AE5.
    fn reads_context_step_locals_and_conductor_waiting_on_a_failed_step() {
        let root = TempRoot::new();
        let store = ExecutionStore::new(&root.0);
        let execution = saved_execution(&store);
        let conductor = ConductorStore::new(store.paths(), &execution.id);
        conductor
            .save_state(&ConductorState {
                pending: Some(PendingInvocation {
                    invocation_id: "inv-1".to_string(),
                    request: request(ConductorPurpose::DiagnoseFailure),
                    started_at: Utc::now(),
                }),
                failure: Some(FailureReport {
                    key: "review#1".to_string(),
                    episode: "review#1".to_string(),
                    state_id: "review".to_string(),
                    reason: "tests failed".to_string(),
                    outcome: FailureOutcome::Diagnosed {
                        diagnosis: "a flaky test".to_string(),
                        proposals: Vec::new(),
                    },
                    concluded_at: Utc::now(),
                }),
                ..ConductorState::default()
            })
            .unwrap();
        conductor.save_generated_title("Fix the review").unwrap();

        let state = execution_state(&store, &execution.id).unwrap();

        let context = value(&state.context);
        assert_eq!(context["variables"]["plan"], "line one\nline two");
        assert_eq!(context["artifacts"]["report"]["location"], "out/report.md");
        assert_eq!(
            value(&state.locals),
            &json!({ "draft": { "TRIES": 2 }, "review": { "NOTE": "keep it short" } })
        );
        let conductor = value(&state.conductor);
        assert_eq!(conductor["failure"]["reason"], "tests failed");
        assert_eq!(conductor["pending"]["invocation_id"], "inv-1");
        assert_eq!(conductor["title"], "Fix the review");
    }

    #[test]
    fn reports_a_missing_or_corrupt_conductor_without_hiding_the_other_sections() {
        let root = TempRoot::new();
        let store = ExecutionStore::new(&root.0);
        let execution = saved_execution(&store);

        let state = execution_state(&store, &execution.id).unwrap();
        assert_eq!(
            state.conductor,
            StateSection::Absent {
                message: NO_CONDUCTOR_STATE.to_string()
            }
        );

        let dir = store.paths().conductor_dir(&execution.id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("state.json"), "{ not json").unwrap();

        let state = execution_state(&store, &execution.id).unwrap();
        assert!(
            matches!(&state.conductor, StateSection::Unreadable { reason } if !reason.is_empty()),
            "{:?}",
            state.conductor
        );
        assert!(matches!(state.context, StateSection::Available { .. }));
        assert!(matches!(state.locals, StateSection::Available { .. }));
    }

    #[test]
    fn fails_when_the_execution_record_does_not_load() {
        let root = TempRoot::new();
        let store = ExecutionStore::new(&root.0);

        assert!(execution_state(&store, "missing").is_err());
    }
}
