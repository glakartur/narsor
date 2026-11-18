//! A `decision` gate (`by: human`) on an observer queue, on the runner
//! channel.
//!
//! The runner reports the gate's rendered `watch` block in its snapshot, next
//! to the pending input request it belongs to ([`GateWatch`]). The episode of
//! the gate is the id of that request (`pending_input.id`): the control plane
//! keys its queue entry by task and episode.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The `watch` block of the gate a runner waits at, its `params` rendered
/// when the gate started, with the choices and variables an answer may use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateWatch {
    /// The workflow state of the gate.
    pub step: String,
    /// The name of the queue, as the workflow wrote it (never templated).
    pub queue: String,
    /// The rendered params. Narsor does not interpret any value.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,
    /// The choices of the gate, in the workflow's order.
    #[serde(default)]
    pub choices: Vec<GateWatchChoice>,
    /// The variables an answer may give, in the workflow's order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variables: Vec<GateWatchVariable>,
}

/// One choice of a watched gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateWatchChoice {
    /// The label an answer names.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The variables an answer with this choice must give.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires_variables: Vec<String>,
}

/// One variable an answer to a watched gate may give.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateWatchVariable {
    pub name: String,
    /// The declared type (`string`, `text`, `select`, `integer`,
    /// `boolean`), when the workflow declares one.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub variable_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The only values the variable may take; empty means any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}
