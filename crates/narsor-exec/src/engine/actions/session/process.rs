//! What a session reads out of a finished harness run.

use super::*;
use crate::engine::actions::harness::FinishedRun;

/// The run stopped on an exhausted usage limit, in a session it can go on
/// in. A stop without a session id is the ordinary failure: there is
/// nothing to continue.
pub(super) fn usage_limit_stopped(
    harness: &str,
    run: &HarnessRun,
    failure: Option<&(String, ActionError)>,
) -> Option<UsageLimitStopped> {
    let stop = run.usage_limit_stop.as_ref()?;
    let session_id = run.session_id.as_ref()?;
    let (_, error) = failure?;
    Some(UsageLimitStopped {
        harness: harness.to_string(),
        harness_session_id: session_id.clone(),
        resets_at: stop.resets_at,
        error: error.clone(),
    })
}

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
