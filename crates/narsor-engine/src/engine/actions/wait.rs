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
//! step reads `{{ context.variables.event.data.mr }}`. A wait has no time
//! limit: nothing else ends it.
//!
//! # Crash safety
//!
//! As for a human gate: [`suspend`] writes the pending file before the
//! status, and the status is the authority, so a leftover file is inert.
//! [`resume`] writes the log, then the status, and removes the file last.

use std::future::Future;
use std::pin::Pin;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

use super::{ActionExecutor, ActionOutcome, ActionRequest, SuspendedAction};
use crate::domain::{
    ActionConfig, ActionError, ActionExecutionId, ActionResult, EventData, ExecutionId,
    ExecutionStatus, OutputLabel, StateId, WaitActionConfig, WaitWatchConfig, WorkflowExecution,
};
use crate::orchestration::{EventWait, EventWatch};
use crate::persistence::{ExecutionStore, PersistenceError};
use narsor_protocol::controller::filter_matches;

/// The label a matching event ends a step with when the step declares more
/// than one way out (§3.3.D).
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
                    watch: config.watch.clone(),
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

/// What [`WaitExecutor`] observed: the event the step waits for and its
/// filter with templates rendered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExternalEventRequest {
    /// Id of the [`crate::domain::ActionExecution`] that suspended.
    pub action_execution_id: ActionExecutionId,
    /// The event type (`event_type` in YAML).
    pub event_type: String,
    /// The rendered filter; `null` when the step has none.
    #[serde(default)]
    pub filter: Value,
    /// The step's rendered `watch` block, if it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<WaitWatchConfig>,
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
}

impl PendingExternalEvent {
    /// The record of a wait that begins now.
    pub fn new(
        execution_id: impl Into<ExecutionId>,
        state_id: impl Into<StateId>,
        request: ExternalEventRequest,
    ) -> Self {
        Self {
            execution_id: execution_id.into(),
            state_id: state_id.into(),
            request,
            episode: uuid::Uuid::new_v4().to_string(),
            since: Utc::now(),
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
            watch: self.request.watch.as_ref().map(|watch| EventWatch {
                step: self.state_id.to_string(),
                observer: watch.observer.clone(),
                object: watch.object.clone(),
                event_types: watch.event_types.clone(),
            }),
        }
    }
}

/// What ends a wait, as the runner hands it to the engine.
#[derive(Debug, Clone, PartialEq)]
pub struct ExternalEventDelivery {
    /// The episode it is for ([`PendingExternalEvent::episode`]).
    pub episode: String,
    /// The event.
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
/// episode and an event the wait does not match (the control plane matched
/// it already; this is the second line) — all on the read the writes below
/// follow. A refused delivery changes nothing. On success the event is in
/// the log as `ExternalEventReceived`, the execution is `Running` on disk
/// and the pending file is gone.
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
    let WaitEnding::Event {
        event_type,
        source,
        data,
    } = &delivery.ending;
    if *event_type != pending.request.event_type || !filter_matches(&pending.request.filter, data) {
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

    execution.status = ExecutionStatus::Running;
    store.save_execution(&execution)?;
    // Last, so that a crash before this point leaves the wait answerable.
    store.clear_pending_event(execution_id)?;
    Ok((execution, pending))
}

/// The step's result for how its wait ended, given the labels the state
/// declares: the event under [`received_label`].
pub fn wait_result(ending: &WaitEnding, declared: &[OutputLabel]) -> ActionResult {
    let WaitEnding::Event {
        event_type,
        source,
        data,
    } = ending;
    let mut variables = serde_json::Map::new();
    variables.insert(
        EVENT_VARIABLE.to_string(),
        json!({"type": event_type, "source": source, "data": data}),
    );
    ActionResult {
        reason: format!("external event '{event_type}' from '{source}'"),
        data: data.clone(),
        variables,
        ..ActionResult::new(received_label(declared))
    }
}

/// The label a matching event ends a step with: the only label it declares
/// (an `exit` step's `exit`, say), otherwise [`RECEIVED_LABEL`]. A step
/// declaring neither fails on the label like any step whose action answers
/// with an undeclared one.
pub fn received_label(declared: &[OutputLabel]) -> String {
    match declared {
        [only] => only.clone(),
        _ => RECEIVED_LABEL.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending() -> PendingExternalEvent {
        PendingExternalEvent::new(
            "exec",
            "await_review",
            ExternalEventRequest {
                action_execution_id: "a1".into(),
                event_type: "review.approved".into(),
                filter: json!({"mr": "17"}),
                watch: None,
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
        let result = wait_result(&ending, &labels(&["received", "rejected"]));
        assert_eq!(result.output_label, "received");
        assert_eq!(result.data, json!({"mr": 17}));
        assert_eq!(
            result.variables[EVENT_VARIABLE],
            json!({"type": "review.approved", "source": "gitlab", "data": {"mr": 17}})
        );
    }

    #[test]
    fn the_only_way_out_is_the_label_of_an_event() {
        assert_eq!(received_label(&labels(&["exit"])), "exit");
        assert_eq!(received_label(&labels(&["approved"])), "approved");
        assert_eq!(received_label(&labels(&["a", "b"])), RECEIVED_LABEL);
        assert_eq!(received_label(&[]), RECEIVED_LABEL);
    }

    #[test]
    fn the_wait_reports_its_episode_and_matches_its_event() {
        let pending = pending();
        let wait = pending.event_wait();
        assert_eq!(wait.episode_id, pending.episode);
        assert_eq!(wait.since, pending.since);
        assert!(wait.matches("review.approved", &json!({"mr": 17})));
    }
}
