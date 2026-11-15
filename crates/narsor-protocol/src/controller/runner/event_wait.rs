//! A `wait` step waiting for an external event, on the runner channel.
//!
//! The runner reports the wait in its snapshot ([`EventWait`]); the control
//! plane matches an observer's event against it and hands a matching one
//! back on the long-poll ([`DeliveredEvent`]). The runner acknowledges each delivery once.
//!
//! These types travel only between a runner and its control plane, which
//! are deployed together: a new [`EventDelivery`] variant is a joint update
//! of both (see `docs/dev/workflow-schema.md` §3.3.D), never a fallback.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::controller::InputStatus;

/// One episode of a task waiting in a `wait` step for an external event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventWait {
    /// Identifies this episode: a step that waits again (a review loop)
    /// starts a new one.
    pub episode_id: String,
    /// The event type the step waits for.
    pub event_type: String,
    /// The step's filter, its templates rendered.
    #[serde(default)]
    pub filter: Value,
    /// When the wait began; only events from then on reach the step.
    pub since: DateTime<Utc>,
    /// The watch entry the step's `watch` block asks for, if it has one.
    /// The control plane registers it when the episode begins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<EventWatch>,
}

/// What a `wait` step's `watch` block asks an observer to watch, its
/// templates rendered (`docs/dev/workflow-schema.md` §3.3.D).
///
/// The control plane checks it against the observer before it adds the
/// watch entry; nothing here is trusted yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventWatch {
    /// The workflow state whose `wait` asks for it: with the task and the
    /// object, the key of the watch entry.
    pub step: String,
    /// The name of the observer.
    pub observer: String,
    /// The object to watch (the URL of a merge request, say).
    pub object: String,
    /// The event types the observer's script is to report for it.
    #[serde(default)]
    pub event_types: Vec<String>,
}

impl EventWait {
    /// Whether an event of `event_type` with `data` is what this wait waits
    /// for: the same type, and [`filter_matches`] of the filter.
    pub fn matches(&self, event_type: &str, data: &Value) -> bool {
        self.event_type == event_type && filter_matches(&self.filter, data)
    }
}

/// Whether the data of an event passes a `wait` step's filter (R4): every
/// field of the filter equal to the field of the same name in the data.
///
/// The comparison is flat, without expressions. Scalars (text, number,
/// boolean) compare as text on both sides, because a filter rendered from
/// a template is always text: filter `{"mr": "17"}` matches data
/// `{"mr": 17}`. Other values compare as JSON. An empty or absent filter
/// matches every event; a filter that is not an object matches none.
pub fn filter_matches(filter: &Value, data: &Value) -> bool {
    match filter {
        Value::Null => true,
        Value::Object(filter) => filter.iter().all(|(field, expected)| {
            data.get(field)
                .is_some_and(|actual| same_filter_value(expected, actual))
        }),
        _ => false,
    }
}

/// Scalars as text, so `"17"` and `17` are the same filter value.
fn filter_scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn same_filter_value(expected: &Value, actual: &Value) -> bool {
    match (filter_scalar_text(expected), filter_scalar_text(actual)) {
        (Some(expected), Some(actual)) => expected == actual,
        _ => expected == actual,
    }
}

/// What ends a `wait` step, handed to the runner of the waiting task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeliveredEvent {
    /// Control-plane id of the delivery, unique within the task
    /// (`event-<n>`); the runner acknowledges it by this id.
    pub input_id: String,
    /// The wait episode it is for; the runner's engine refuses it for any
    /// other.
    pub episode_id: String,
    /// The event.
    pub delivery: EventDelivery,
}

/// What a [`DeliveredEvent`] carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventDelivery {
    /// An external event that matched the wait.
    Event {
        /// The event type.
        event_type: String,
        /// The observer's source.
        source: String,
        /// The event's data. Untrusted: it comes from outside.
        #[serde(default)]
        data: Value,
    },
}

/// Response to `POST /runner/v1/events/{input_id}/ack`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventAckResponse {
    pub input_id: String,
    pub status: InputStatus,
}
