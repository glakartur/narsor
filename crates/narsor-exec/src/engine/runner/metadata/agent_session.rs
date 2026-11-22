//! The harness session the current step of an execution can be continued
//! in.
//!
//! One read answers it, for the recovery that continues the session with the
//! owner's message and for the runner that tells the owner whether it can:
//! the two can never disagree.

use super::*;
use crate::engine::actions::harness::{CLAUDE_CODE, CODEX, GITHUB_COPILOT, OPENCODE};
use crate::engine::timeline_file::{reported_sessions, ReportedSession};

/// A harness session to continue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumableSession {
    pub harness: String,
    pub harness_session_id: String,
}

impl From<ReportedSession> for ResumableSession {
    fn from(session: ReportedSession) -> Self {
        Self {
            harness: session.harness,
            harness_session_id: session.harness_session_id,
        }
    }
}

impl From<AgentSessionProvenance> for ResumableSession {
    fn from(session: AgentSessionProvenance) -> Self {
        Self {
            harness: session.harness,
            harness_session_id: session.session_id,
        }
    }
}

/// Whether `harness` can continue a session of its own. Every harness of
/// this version can; a name it does not know is taken as one that cannot.
fn resumes_sessions(harness: &str) -> bool {
    [CLAUDE_CODE, CODEX, GITHUB_COPILOT, OPENCODE].contains(&harness)
}

/// The session of attempt `attempt`, and whether it ended well: from the
/// end of its session in the log, else from what its harness reported
/// while it ran.
pub(crate) fn attempt_session(
    events: &[Event],
    reported: &BTreeMap<String, ReportedSession>,
    attempt: &str,
) -> Option<(ResumableSession, bool)> {
    let of_attempt = |id: &Option<String>| id.as_deref() == Some(attempt);
    let mut harness = None;
    let mut ended = None;
    for event in events {
        match &event.data {
            EventData::AgentSessionStarted {
                harness: started,
                action_execution_id,
                ..
            } if of_attempt(action_execution_id) => harness = Some(started.clone()),
            EventData::AgentSessionCompleted {
                action_execution_id,
                harness_session_id,
                ..
            } if of_attempt(action_execution_id) => {
                ended = Some((harness_session_id.clone(), true));
            }
            EventData::AgentSessionFailed {
                action_execution_id,
                harness_session_id,
                ..
            } if of_attempt(action_execution_id) => {
                ended = Some((harness_session_id.clone(), false));
            }
            _ => {}
        }
    }
    let reported = reported.get(attempt);
    let completed = matches!(ended, Some((_, true)));
    let session = match ended.and_then(|(id, _)| id) {
        Some(id) => ResumableSession {
            harness: harness
                .or_else(|| reported.map(|session| session.harness.clone()))
                .unwrap_or_default(),
            harness_session_id: id,
        },
        None => ResumableSession::from(reported?.clone()),
    };
    Some((session, completed))
}

/// The latest attempt of `state_id` in the log.
fn latest_attempt<'a>(events: &'a [Event], state_id: &str) -> Option<&'a str> {
    events.iter().rev().find_map(|event| match &event.data {
        EventData::ActionStarted {
            action_execution_id,
            state_id: state,
            ..
        }
        | EventData::OutputReprompted {
            action_execution_id,
            state_id: state,
            ..
        } if state == state_id => Some(action_execution_id.as_str()),
        EventData::ActionRetrying {
            action_execution_id,
            state_id: Some(state),
            ..
        } if state == state_id => Some(action_execution_id.as_str()),
        _ => None,
    })
}

/// The harness session the current step of `execution` goes on in when it
/// is continued, if there is one a harness can continue. `events` is the
/// execution's whole log.
///
/// In this order: the session of the step's latest attempt that never
/// ended well — cut by a lost workload, or failed — then the session the
/// step failure records, then the one a rejected or rescued output names,
/// then the one the state last completed. While the failure records a
/// session, no older session of the state is looked at: the step failed in
/// that one.
pub fn resumable_agent_session(
    store: &ExecutionStore,
    execution: &WorkflowExecution,
    events: &[Event],
) -> Option<ResumableSession> {
    let state_id = execution.current_state.as_deref()?;
    let reported = reported_sessions(store, &execution.id);
    let in_flight = latest_attempt(events, state_id)
        .and_then(|attempt| attempt_session(events, &reported, attempt))
        .filter(|(_, completed)| !completed)
        .map(|(session, _)| session);
    let failed = || {
        step_failure(execution)
            .ok()
            .flatten()
            .filter(|failure| failure.state_id == state_id)
            .and_then(|failure| failure.session())
    };
    let rejected = || {
        stored_output_rejection(execution, state_id)
            .ok()
            .flatten()
            .and_then(|rejection| session_of(rejection.harness, rejection.harness_session_id))
    };
    let rescued = || {
        stored_output_rescue(execution, state_id)
            .ok()
            .flatten()
            .and_then(|rescue| session_of(rescue.harness, rescue.harness_session_id))
    };
    let completed = || {
        read_engine_record::<BTreeMap<String, AgentSessionProvenance>>(
            execution,
            METADATA_AGENT_SESSIONS_KEY,
        )
        .ok()
        .flatten()
        .and_then(|mut sessions| sessions.remove(state_id))
        .map(ResumableSession::from)
    };
    in_flight
        .or_else(failed)
        .or_else(rejected)
        .or_else(rescued)
        .or_else(completed)
        .filter(|session| resumes_sessions(&session.harness))
}

/// A session from a record that names its harness and its id, when it
/// names both.
pub(crate) fn session_of(harness: Option<String>, id: Option<String>) -> Option<ResumableSession> {
    Some(ResumableSession {
        harness: harness?,
        harness_session_id: id?,
    })
}
