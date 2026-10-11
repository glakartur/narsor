//! Stable request and response types for the local REST API.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::controller::ExecutionEventsSource;

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

/// Where one raw conversation line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationEntrySource {
    Narsor,
    Stdout,
    Stderr,
}

/// One decoded conversation entry in the API wire format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationEntryDto {
    pub entry_number: u64,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub source: ConversationEntrySource,
    pub raw_base64: String,
}

/// One raw conversation page. `NoRecord` is a successful answer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ConversationResponse {
    Recorded {
        entries: Vec<ConversationEntryDto>,
        next_entry: u64,
        closed: bool,
        truncated_tail: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<ExecutionEventsSource>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recorded_at: Option<chrono::DateTime<chrono::Utc>>,
    },
    NoRecord,
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
    use super::{
        ConversationEntryDto, ConversationEntrySource, ConversationResponse,
        HarnessSelectionSnapshot, RecoveryRequest,
    };

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

    #[test]
    fn conversation_page_keeps_its_tag_and_omits_absent_metadata() {
        let response = ConversationResponse::Recorded {
            entries: vec![ConversationEntryDto {
                entry_number: 3,
                timestamp: chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
                    .unwrap()
                    .with_timezone(&chrono::Utc),
                source: ConversationEntrySource::Stderr,
                raw_base64: "AP8=".into(),
            }],
            next_entry: 4,
            closed: true,
            truncated_tail: false,
            source: None,
            recorded_at: None,
        };

        let value = serde_json::to_value(response).unwrap();
        assert_eq!(value["status"], "recorded");
        assert_eq!(value["entries"][0]["source"], "stderr");
        assert_eq!(value["entries"][0]["raw_base64"], "AP8=");
        assert!(value.get("source").is_none());
        assert!(value.get("recorded_at").is_none());
    }
}
