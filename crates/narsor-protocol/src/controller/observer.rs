//! The effect routes an observer's script calls through `narsor-cli`, with
//! the token of its run (`/observer/v1/runs/{run}/…`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `POST /observer/v1/runs/{run}/tasks`: create a task from the effect data.
///
/// Only the key and the data: the workflow, repositories, backend and
/// profile come from the observer's record, and only the fields its mapping
/// names become workflow inputs. Any other field is refused.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ObserverCreateTaskRequest {
    /// The effect key: one task per key and observer, ever (R13).
    pub effect_key: String,
    /// The effect data, a JSON object whose fields the mapping names.
    #[serde(default)]
    pub data: Value,
}

/// `POST /observer/v1/runs/{run}/tasks/{task_id}/events`: deliver an event
/// to the task of one of the observer's watch entries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ObserverEventRequest {
    /// The effect key: one event per key, observer and task, ever (R13).
    pub effect_key: String,
    /// The event type (`review.approved`, say).
    pub event_type: String,
    /// The event's data, any JSON value of bounded size.
    #[serde(default)]
    pub data: Value,
}

/// What an effect route did.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObserverEffectOutcome {
    /// A task was created.
    Created,
    /// The key was used before; nothing was done again (PO3: not an error).
    Duplicate,
    /// A dry run: the task would have been created (R10).
    WouldCreate,
    /// The event matched the task's waiting `wait` step and was queued for
    /// it; it is in the task's history too.
    Delivered,
    /// The task does not wait for this event: it went to the history only.
    Recorded,
    /// A dry run: the event would have been delivered (R10).
    WouldDeliver,
    /// An outcome from a newer server.
    #[serde(other)]
    Unrecognized,
}

/// The answer of an effect route.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverEffectResponse {
    /// What was done.
    pub outcome: ObserverEffectOutcome,
    /// The effect key, as given.
    pub effect_key: String,
    /// The task created, found or addressed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// The input queued for the waiting step, for a delivered event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_id: Option<String>,
    /// One line for the script's log ("skipped: duplicate", say).
    pub message: String,
}
