//! `WaitAction` executor and the suspend/deliver mechanism
//! (`docs/dev/workflow-schema.md` §3.3.D).
//!
//! A `wait` step is built like a `human` gate ([`super::human`]): it cannot
//! be *run*, only waited out. Entering it records what the step waits for,
//! moves the execution to [`ExecutionStatus::WaitingForEvent`] and ends the
//! pass; what ends the wait arrives later — possibly in another process,
//! after a restart — through [`resume`].
//!
//! # The two halves
//!
//! * [`WaitExecutor`] is the [`ActionExecutor`] half. It is pure: it reads
//!   the rendered configuration and reports
//!   [`SuspendedAction::AwaitingExternalEvent`], doing no I/O.
//! * [`suspend`] and [`resume`] are the durable half, called by the engine.
//!   They own the `WaitingForEvent` status, the awaited event on disk
//!   (`pending_event.json`) and the `ExternalEventReceived` entry of the
//!   execution's log.
//!
//! # Episodes
//!
//! Every wait gets a fresh key ([`PendingExternalEvent::episode`]). The
//! runner reports it to the control plane, which matches events against it
//! and hands back only what is meant for it; [`resume`] refuses anything
//! naming another episode, so an event for a wait the task has left can
//! never end a later one.
//!
//! # What ends a wait
//!
//! A matching event ends the step with the step's label for it
//! ([`received_label`]) and the event as the step's result: the context
//! variable [`EVENT_VARIABLE`] holds `{type, source, data}`, so a later
//! step reads `{{ context.variables.event.data.mr }}`. The wait's deadline
//! (`timeout` in YAML) ends it through [`TimedOutAction::resolve`]: the
//! `timeout` label when the step declares it, otherwise a failed step
//! (PO4). The control plane's periodic pass delivers the timeout, because
//! the task may have no compute while it waits.
//!
//! # Crash safety
//!
//! As for a human gate: [`suspend`] writes the pending file before the
//! status, and the status is the authority, so a leftover file is inert.
//! [`resume`] writes the log, then the status, and removes the file last.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

use super::{ActionExecutor, ActionOutcome, ActionRequest, SuspendedAction, TimedOutAction};
use crate::domain::{
    ActionConfig, ActionError, ActionExecutionId, ActionResult, EventData, ExecutionId,
    ExecutionStatus, OutputLabel, StateId, WaitActionConfig, WorkflowExecution, TIMEOUT_LABEL,
};
use crate::orchestration::EventWait;
use crate::persistence::{ExecutionStore, PersistenceError};
use narsor_protocol::controller::filter_matches;

/// The label a matching event ends a step with when the step declares more
/// than one way out besides `timeout` (§3.3.D).
pub const RECEIVED_LABEL: &str = "received";

/// The context variable the event that ended a `wait` step is written to:
/// `{"type": ..., "source": ..., "data": ...}`. The latest event wins.
pub const EVENT_VARIABLE: &str = "event";

/// Executes `WaitAction` states. Stateless.
#[derive(Debug, Clone, Copy, Default)]
pub struct WaitExecutor;

impl WaitExecutor {
    /// Creates an executor.
    pub fn new() -> Self {
        Self
    }
}

impl ActionExecutor for WaitExecutor {
    fn execute<'a>(
        &'a self,
        request: ActionRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ActionOutcome, ActionError>> + Send + 'a>> {
        Box::pin(async move {
            let config = wait_config(request.action)?;
            Ok(ActionOutcome::Suspended(
                SuspendedAction::AwaitingExternalEvent(ExternalEventRequest {
                    action_execution_id: request.action_execution_id.to_string(),
                    event_type: config.event.clone(),
                    filter: config.filter.clone().unwrap_or(Value::Null),
                    timeout_secs: config.timeout_secs,
                }),
            ))
        })
    }
}

/// Narrows the action configuration to a wait configuration.
fn wait_config(action: &ActionConfig) -> Result<&WaitActionConfig, ActionError> {
    match action {
        ActionConfig::Wait(config) => Ok(config),
        other => Err(ActionError {
            message: format!("WaitExecutor received a {} action", other.type_name()),
            error_type: Some("wrong_action_type".to_string()),
            details: None,
        }),
    }
}

/// What [`WaitExecutor`] observed: the event the step waits for, its
/// filter with templates rendered, and its time limit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExternalEventRequest {
    /// Id of the [`crate::domain::ActionExecution`] that suspended.
    pub action_execution_id: ActionExecutionId,
    /// The event type (`event_type` in YAML).
    pub event_type: String,
    /// The rendered filter; `null` when the step has none.
    #[serde(default)]
    pub filter: Value,
    /// The step's `timeout`, in seconds; `None` waits for ever.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// The durable record of a wait — `pending_event.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingExternalEvent {
    /// The execution that is waiting.
    pub execution_id: ExecutionId,
    /// The state whose action suspended.
    pub state_id: StateId,
    /// What the executor reported.
    #[serde(flatten)]
    pub request: ExternalEventRequest,
    /// The key of this wait, fresh for every wait.
    pub episode: String,
    /// When the wait began.
    pub since: DateTime<Utc>,
    /// When it times out, if it does.
    #[serde(default)]
    pub deadline: Option<DateTime<Utc>>,
}

impl PendingExternalEvent {
    /// The record of a wait that begins now.
    pub fn new(
        execution_id: impl Into<ExecutionId>,
        state_id: impl Into<StateId>,
        request: ExternalEventRequest,
    ) -> Self {
        let since = Utc::now();
        let deadline = request.timeout_secs.map(|secs| {
            since + chrono::Duration::seconds(i64::try_from(secs).unwrap_or(i64::MAX / 1000))
        });
        Self {
            execution_id: execution_id.into(),
            state_id: state_id.into(),
            request,
            episode: uuid::Uuid::new_v4().to_string(),
            since,
            deadline,
        }
    }

    /// The action execution that suspended.
    pub fn action_execution_id(&self) -> &str {
        &self.request.action_execution_id
    }

    /// The wait as the runner reports it to the control plane.
    pub fn event_wait(&self) -> EventWait {
        EventWait {
            episode_id: self.episode.clone(),
            event_type: self.request.event_type.clone(),
            filter: self.request.filter.clone(),
            since: self.since,
            deadline: self.deadline,
        }
    }
}

/// What ends a wait, as the runner hands it to the engine.
#[derive(Debug, Clone, PartialEq)]
pub struct ExternalEventDelivery {
    /// The episode it is for ([`PendingExternalEvent::episode`]).
    pub episode: String,
    /// The event, or the end of the wait's time.
    pub ending: WaitEnding,
}

/// How a wait ended.
#[derive(Debug, Clone, PartialEq)]
pub enum WaitEnding {
    /// A matching external event came.
    Event {
        /// The event type.
        event_type: String,
        /// The observer's source.
        source: String,
        /// The event's data; untrusted.
        data: Value,
    },
    /// The deadline passed without one.
    Timeout,
}

/// Why a delivery could not end a wait.
#[derive(Debug, Error)]
pub enum WaitActionError {
    /// The store could not be read or written.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),

    /// The execution does not wait for an event.
    #[error("execution '{execution_id}' is {status}, not waiting for an external event")]
    NotWaiting {
        execution_id: ExecutionId,
        status: String,
    },

    /// The execution waits, but its record of what for is gone.
    #[error("execution '{execution_id}' has no awaited event on record")]
    NoPendingWait { execution_id: ExecutionId },

    /// The delivery is for an earlier (or another) wait.
    #[error(
        "the delivery is for wait '{delivered}', but execution '{execution_id}' waits in \
         '{current}'"
    )]
    StaleEpisode {
        execution_id: ExecutionId,
        delivered: String,
        current: String,
    },

    /// The event is not the one the step waits for.
    #[error(
        "the step waits for an event '{expected}' with its filter; '{delivered}' does not match"
    )]
    EventMismatch { expected: String, delivered: String },

    /// A timeout for a wait that has no time limit.
    #[error("execution '{execution_id}' waits without a time limit and cannot time out")]
    NoDeadline { execution_id: ExecutionId },
}

impl WaitActionError {
    /// Whether the delivery came too late: the wait it names is over.
    pub fn is_stale(&self) -> bool {
        matches!(
            self,
            Self::NotWaiting { .. } | Self::NoPendingWait { .. } | Self::StaleEpisode { .. }
        )
    }
}

/// Suspends the execution on a `wait` step: records what it waits for,
/// then marks it waiting. Mutates `execution` and persists it.
pub fn suspend(
    store: &ExecutionStore,
    execution: &mut WorkflowExecution,
    pending: &PendingExternalEvent,
) -> Result<(), WaitActionError> {
    store.save_pending_event(&execution.id, pending)?;

    execution.status = ExecutionStatus::WaitingForEvent;
    execution.current_state = Some(pending.state_id.clone());
    execution.current_action_execution = Some(pending.action_execution_id().to_string());
    store.save_execution(execution)?;
    Ok(())
}

/// The wait of an execution, if it is waiting for an event; `None` both
/// when nothing is recorded and when the execution is no longer waiting.
pub fn pending_event(
    store: &ExecutionStore,
    execution_id: &str,
) -> Result<Option<PendingExternalEvent>, WaitActionError> {
    let execution = store.load_execution(execution_id)?;
    if execution.status != ExecutionStatus::WaitingForEvent {
        return Ok(None);
    }
    Ok(store.load_pending_event(execution_id)?)
}

/// Ends a wait with `delivery`, putting the execution back to `Running`.
///
/// Refuses an execution that does not wait, a delivery for another
/// episode, an event the wait does not match (the control plane matched it
/// already; this is the second line) and a timeout of a wait without a
/// limit — all on the read the writes below follow. A refused delivery
/// changes nothing. On success an event is in the log as
/// `ExternalEventReceived` (a timeout writes nothing there: the step's
/// result or failure says it), the execution is `Running` on disk and the
/// pending file is gone.
pub fn resume(
    store: &ExecutionStore,
    execution_id: &str,
    delivery: &ExternalEventDelivery,
) -> Result<(WorkflowExecution, PendingExternalEvent), WaitActionError> {
    let mut execution = store.load_execution(execution_id)?;
    if execution.status != ExecutionStatus::WaitingForEvent {
        return Err(WaitActionError::NotWaiting {
            execution_id: execution_id.to_string(),
            status: execution.status.as_str().to_string(),
        });
    }
    let pending: PendingExternalEvent =
        store
            .load_pending_event(execution_id)?
            .ok_or_else(|| WaitActionError::NoPendingWait {
                execution_id: execution_id.to_string(),
            })?;
    if pending.execution_id != execution.id {
        return Err(WaitActionError::NoPendingWait {
            execution_id: execution_id.to_string(),
        });
    }
    if delivery.episode != pending.episode {
        return Err(WaitActionError::StaleEpisode {
            execution_id: execution_id.to_string(),
            delivered: delivery.episode.clone(),
            current: pending.episode.clone(),
        });
    }
    match &delivery.ending {
        WaitEnding::Event {
            event_type,
            source,
            data,
        } => {
            if *event_type != pending.request.event_type
                || !filter_matches(&pending.request.filter, data)
            {
                return Err(WaitActionError::EventMismatch {
                    expected: pending.request.event_type.clone(),
                    delivered: event_type.clone(),
                });
            }
            store.event_log(execution_id).append_next(
                &execution.id,
                EventData::ExternalEventReceived {
                    event_type: event_type.clone(),
                    source: source.clone(),
                    payload: data.clone(),
                },
            )?;
        }
        WaitEnding::Timeout => {
            if pending.request.timeout_secs.is_none() {
                return Err(WaitActionError::NoDeadline {
                    execution_id: execution_id.to_string(),
                });
            }
        }
    }

    execution.status = ExecutionStatus::Running;
    store.save_execution(&execution)?;
    // Last, so that a crash before this point leaves the wait answerable.
    store.clear_pending_event(execution_id)?;
    Ok((execution, pending))
}

/// The step's result for how its wait ended, given the labels the state
/// declares: the event under [`received_label`], or the timeout resolved
/// by [`TimedOutAction::resolve`] (the `timeout` label when declared,
/// otherwise an action error).
pub fn wait_result(
    pending: &PendingExternalEvent,
    ending: &WaitEnding,
    declared: &[OutputLabel],
) -> Result<ActionResult, ActionError> {
    match ending {
        WaitEnding::Event {
            event_type,
            source,
            data,
        } => {
            let mut variables = serde_json::Map::new();
            variables.insert(
                EVENT_VARIABLE.to_string(),
                json!({"type": event_type, "source": source, "data": data}),
            );
            Ok(ActionResult {
                reason: format!("external event '{event_type}' from '{source}'"),
                data: data.clone(),
                variables,
                ..ActionResult::new(received_label(declared))
            })
        }
        WaitEnding::Timeout => TimedOutAction {
            timeout: Duration::from_secs(pending.request.timeout_secs.unwrap_or_default()),
            subject: format!("wait for event '{}'", pending.request.event_type),
        }
        .resolve(declared),
    }
}

/// The label a matching event ends a step with: the only label it declares
/// besides `timeout` (an `exit` step's `exit`, say), otherwise
/// [`RECEIVED_LABEL`]. A step declaring neither fails on the label like any
/// step whose action answers with an undeclared one.
pub fn received_label(declared: &[OutputLabel]) -> String {
    let mut others = declared.iter().filter(|label| *label != TIMEOUT_LABEL);
    match (others.next(), others.next()) {
        (Some(only), None) => only.clone(),
        _ => RECEIVED_LABEL.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(timeout_secs: Option<u64>) -> PendingExternalEvent {
        PendingExternalEvent::new(
            "exec",
            "await_review",
            ExternalEventRequest {
                action_execution_id: "a1".into(),
                event_type: "review.approved".into(),
                filter: json!({"mr": "17"}),
                timeout_secs,
            },
        )
    }

    fn labels(names: &[&str]) -> Vec<OutputLabel> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn a_matching_event_ends_the_step_with_its_label_and_the_event_as_context() {
        let ending = WaitEnding::Event {
            event_type: "review.approved".into(),
            source: "gitlab".into(),
            data: json!({"mr": 17}),
        };
        let result = wait_result(&pending(None), &ending, &labels(&["received", "timeout"]))
            .expect("an event completes the step");
        assert_eq!(result.output_label, "received");
        assert_eq!(result.data, json!({"mr": 17}));
        assert_eq!(
            result.variables[EVENT_VARIABLE],
            json!({"type": "review.approved", "source": "gitlab", "data": {"mr": 17}})
        );
    }

    #[test]
    fn the_only_way_out_besides_timeout_is_the_label_of_an_event() {
        assert_eq!(received_label(&labels(&["exit"])), "exit");
        assert_eq!(
            received_label(&labels(&["approved", "timeout"])),
            "approved"
        );
        assert_eq!(received_label(&labels(&["a", "b"])), RECEIVED_LABEL);
        assert_eq!(received_label(&[]), RECEIVED_LABEL);
    }

    #[test]
    fn a_timeout_takes_the_timeout_label_only_when_declared() {
        let pending = pending(Some(60));
        let labelled = wait_result(
            &pending,
            &WaitEnding::Timeout,
            &labels(&["received", "timeout"]),
        )
        .expect("a declared timeout label completes the step");
        assert_eq!(labelled.output_label, TIMEOUT_LABEL);

        let failed = wait_result(&pending, &WaitEnding::Timeout, &labels(&["received"]))
            .expect_err("without the label the step fails");
        assert_eq!(failed.error_type.as_deref(), Some("timeout"));
        assert!(
            failed.message.contains("review.approved"),
            "{}",
            failed.message
        );
    }

    #[test]
    fn the_deadline_is_the_start_plus_the_timeout() {
        let limited = pending(Some(90));
        assert_eq!(
            limited.deadline,
            Some(limited.since + chrono::Duration::seconds(90))
        );
        assert_eq!(pending(None).deadline, None);
        let wait = limited.event_wait();
        assert_eq!(wait.episode_id, limited.episode);
        assert!(wait.matches("review.approved", &json!({"mr": 17})));
    }
}
