//! What a session reads out of a finished harness run.

use super::*;
use crate::engine::actions::harness::FinishedRun;

/// The harness itself reported a failure.
pub(super) fn harness_error(
    adapter: &dyn HarnessAdapter,
    failure: &str,
    finished: &FinishedRun,
    run: &HarnessRun,
    resumed_session: Option<&str>,
) -> ActionError {
    ActionError {
        message: format!("the agent harness '{}' failed: {failure}", adapter.name()),
        error_type: Some(
            if resumed_session.is_some() {
                RESUME_SESSION_REJECTED
            } else {
                "harness_failed"
            }
            .to_string(),
        ),
        details: Some(serde_json::json!({
            "harness": adapter.name(),
            "program": adapter.program(),
            "exit_code": finished.exit_code,
            "session_id": run.session_id,
            "stderr": truncate(&finished.stderr),
            "resumed_session": resumed_session,
        })),
    }
}
