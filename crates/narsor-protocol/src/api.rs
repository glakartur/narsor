//! Stable request and response types for the local REST API.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Authoritative default-harness selection returned by the settings API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessSelectionSnapshot {
    pub supported: Vec<String>,
    pub configured: String,
    #[serde(rename = "override")]
    pub override_harness: Option<String>,
    pub effective: String,
    pub revision: u64,
}

/// `PUT /settings/agent-harness` request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetHarnessSelectionRequest {
    pub harness: String,
}

/// Safe, user-displayable invalid-request response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestErrorResponse {
    pub code: String,
    pub message: String,
}

/// `POST /tasks` request body. The workflow field remains an unresolved
/// reference so the client does not need server-side catalogue access.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartTaskRequest {
    pub task_id: String,
    pub workflow: String,
    #[serde(default)]
    pub inputs: Map<String, Value>,
}

/// `POST /executions/:id/resume`, `/input`, and `/recover` response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptedResponse {
    pub execution_id: String,
}

/// Explicit operator recovery of a failed or interrupted execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryRequest {
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode: Option<String>,
}

/// One-shot instruction appended to the next agent attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorNoteRequest {
    pub note: String,
}

/// `POST /executions/:id/transition` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionRequest {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub force: bool,
}

/// `POST /executions/:id/conductor/chat` request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessageRequest {
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::{HarnessSelectionSnapshot, RecoveryRequest};

    #[test]
    fn harness_override_keeps_the_existing_wire_name() {
        let value = serde_json::to_value(HarnessSelectionSnapshot {
            supported: vec!["codex".into()],
            configured: "claude".into(),
            override_harness: Some("codex".into()),
            effective: "codex".into(),
            revision: 4,
        })
        .unwrap();

        assert_eq!(value["override"], "codex");
        assert!(value.get("override_harness").is_none());
    }

    #[test]
    fn absent_recovery_fields_keep_the_existing_wire_defaults() {
        let request: RecoveryRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(request.comment, None);
        assert_eq!(request.episode, None);
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["comment"], serde_json::Value::Null);
        assert!(value.get("episode").is_none());
    }
}
