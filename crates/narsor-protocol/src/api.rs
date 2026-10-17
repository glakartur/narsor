//! Stable request and response types for the local REST API.

mod human_input;
mod material;
mod timeline;

pub use human_input::*;
pub use material::*;
pub use timeline::*;

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::chat::ChatElement;
use crate::controller::ExecutionEventsSource;

/// Discovery record published by `narsor-server` for local clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonDiscoveryRecord {
    pub pid: u32,
    pub addr: std::net::SocketAddr,
    pub token: String,
    pub started_at: DateTime<Utc>,
}

/// `POST /tasks` response body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StartTaskResponse {
    pub task: crate::controller::TaskSummary,
}

/// Owner-facing summary of one stored execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecutionSummary {
    pub id: String,
    pub workflow_id: String,
    #[serde(default)]
    pub workflow_version: u32,
    pub status: ExecutionStatusDto,
    pub current_state: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub variables: HashMap<String, Value>,
    #[serde(default)]
    pub metadata: Value,
    #[serde(default)]
    pub pending_input: Option<PendingHumanInputView>,
    #[serde(default)]
    pub problem: Option<ActiveProblem>,
    #[serde(default)]
    pub annotation: Option<AnnotationDto>,
    #[serde(default)]
    pub conductor: Option<ConductorActivity>,
}

/// `GET /executions` response body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecutionListResponse {
    pub executions: Vec<ExecutionSummary>,
}

/// Legacy execution from before the task scheduler.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LegacyExecutionView {
    pub id: String,
    pub workflow_id: String,
    pub workflow_version: u32,
    pub status: ExecutionStatusDto,
    #[serde(default)]
    pub current_state: Option<String>,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub problem: Option<ActiveProblem>,
    pub recoverable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_recoverable_reason: Option<String>,
}

/// Legacy execution history response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LegacyExecutionListResponse {
    pub executions: Vec<LegacyExecutionView>,
}

/// The conductor's chat history after a sequence number.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConductorChatResponse {
    pub entries: Vec<ChatEntryDto>,
}

/// A request waiting for the conductor, projected for status output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueuedRequestView {
    pub request_id: String,
    pub trigger: ConductorTriggerDto,
    pub purpose: ConductorPurposeDto,
    #[serde(default)]
    pub episode: Option<String>,
    #[serde(default)]
    pub payload: Value,
    pub queued_at: DateTime<Utc>,
}

/// The invocation currently handled by the conductor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingInvocationView {
    pub invocation_id: String,
    pub request: QueuedRequestView,
    pub started_at: DateTime<Utc>,
}

/// Failure details shown by the client; nested outcome data stays opaque.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FailureReportView {
    pub key: String,
    pub episode: String,
    pub state_id: String,
    pub reason: String,
    pub outcome: Value,
    pub concluded_at: DateTime<Utc>,
}

/// Conductor status response for a stored execution.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConductorStateResponse {
    #[serde(default)]
    pub pending: Option<PendingInvocationView>,
    #[serde(default)]
    pub queue: Vec<QueuedRequestView>,
    #[serde(default)]
    pub unanswered_chat: Vec<String>,
    #[serde(default)]
    pub invocation_attempts: BTreeMap<String, u32>,
    #[serde(default)]
    pub used_retries: std::collections::BTreeSet<String>,
    #[serde(default)]
    pub unapplied_decisions: Vec<Value>,
    #[serde(default)]
    pub failure: Option<crate::controller::FailureReport>,
}

/// How an installation owner was bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerBindingSource {
    Configuration,
    Claim,
}

/// Request to claim an unowned installation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClaimRequest {
    pub claim_code: String,
}

/// The installation owner as presented to an authenticated caller.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OwnerView {
    pub issuer: String,
    pub subject: String,
    pub bound_by: OwnerBindingSource,
    pub bound_at: DateTime<Utc>,
    pub revision: u64,
}

/// Identity and ownership information for the authenticated caller.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WhoAmIResponse {
    pub mode: String,
    pub issuer: Option<String>,
    pub subject: Option<String>,
    pub owner: bool,
    pub claimed: bool,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Result of confirming or rejecting a conductor proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    Executed,
    Rejected,
}

/// Response to confirming or rejecting a conductor proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalOutcome {
    pub proposal_id: String,
    pub status: ProposalStatus,
    #[serde(default)]
    pub decision_id: Option<String>,
    pub message: String,
    #[serde(default)]
    pub repeated: bool,
}

/// Response to undoing a conductor output decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoOutcome {
    pub decision_id: String,
    pub state_id: String,
    pub episode: String,
    #[serde(default)]
    pub warning: Option<String>,
    pub message: String,
}

/// Execution state as it appears in API responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatusDto {
    Running,
    Paused,
    InputNeeded,
    WaitingForEvent,
    Completed,
    Failed,
    Cancelled,
    Unknown,
}

impl ExecutionStatusDto {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Paused => "paused",
            Self::InputNeeded => "input_needed",
            Self::WaitingForEvent => "waiting_for_event",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Unknown => "unknown",
        }
    }
}

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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConversationEntryDto {
    pub entry_number: u64,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub source: ConversationEntrySource,
    /// The raw line, byte for byte: a diagnostic record, not for display.
    pub raw_base64: String,
    /// What the entry says, harness-independent. A current server always
    /// sends at least one element per entry; the list is empty only when
    /// the page comes from a server older than the element model.
    #[serde(default)]
    pub elements: Vec<ChatElement>,
}

/// One raw conversation page. `NoRecord` is a successful answer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

/// Author of an entry in the execution conductor chat response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatAuthorDto {
    Operator,
    Conductor,
}

/// Kind of an entry in the execution conductor chat response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatEntryKindDto {
    Message,
    Error,
    Decision,
    Proposal,
}

/// One conductor chat entry exposed through the REST API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatEntryDto {
    pub id: String,
    pub sequence: u64,
    pub at: chrono::DateTime<chrono::Utc>,
    pub author: ChatAuthorDto,
    pub kind: ChatEntryKindDto,
    pub text: String,
    #[serde(default)]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub payload: Option<Value>,
}

/// Kind of problem presented for an execution in an API response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProblemKindDto {
    Output,
    Failure,
}

impl ProblemKindDto {
    pub fn name(self) -> &'static str {
        match self {
            Self::Output => "output",
            Self::Failure => "failure",
        }
    }
}

/// Conductor trigger as presented in an execution summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConductorTriggerDto {
    OutputRescue,
    StepFailure,
    Chat,
    Annotation,
}

impl ConductorTriggerDto {
    pub fn name(self) -> &'static str {
        match self {
            Self::OutputRescue => "output_rescue",
            Self::StepFailure => "step_failure",
            Self::Chat => "chat",
            Self::Annotation => "annotation",
        }
    }
}

/// Conductor purpose as presented in an execution summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConductorPurposeDto {
    ChooseOutput,
    ChooseOutputAfterReprompt,
    DiagnoseFailure,
    ReplyToChat,
    SummarizeGate,
}

impl ConductorPurposeDto {
    pub fn name(self) -> &'static str {
        match self {
            Self::ChooseOutput => "choose_output",
            Self::ChooseOutputAfterReprompt => "choose_output_after_reprompt",
            Self::DiagnoseFailure => "diagnose_failure",
            Self::ReplyToChat => "reply_to_chat",
            Self::SummarizeGate => "summarize_gate",
        }
    }
}

/// The conductor's current activity, summarized for an API client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConductorActivity {
    pub purpose: ConductorPurposeDto,
    pub trigger: ConductorTriggerDto,
    #[serde(default)]
    pub episode: Option<String>,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

/// A problem an execution is stopped on, as presented to an API client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveProblem {
    pub kind: ProblemKindDto,
    pub reason: String,
    #[serde(default)]
    pub episode: Option<String>,
}

/// Advisory summary and recommendation attached to a pending human input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnnotationDto {
    pub key: String,
    pub summary: String,
    #[serde(default)]
    pub recommendation: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[cfg(test)]
mod tests {
    use serde_json::Map;
    use std::collections::BTreeMap;

    use super::{
        ActiveProblem, AnnotationDto, ChatAuthorDto, ChatEntryDto, ChatEntryKindDto, ClaimRequest,
        ConductorActivity, ConductorPurposeDto, ConductorTriggerDto, ConversationEntryDto,
        ConversationEntrySource, ConversationResponse, DaemonDiscoveryRecord, EventLink,
        ExecutionStatusDto, HarnessSelectionSnapshot, HumanInputActionDto, HumanInputDeliveryDto,
        HumanInputTypeView, InputContinuationView, MaterialAnnouncement, MaterialFile,
        MaterialListResponse, MaterialOrigin, MaterialSummary, MaterialUploadResponse,
        MaterialView, OwnerBindingSource, OwnerView, PendingHumanInputView, Position,
        PositionStatus, ProblemKindDto, ProposalOutcome, ProposalStatus, RecoveryRequest, Timeline,
        UndoOutcome, UploadManifest, WhoAmIResponse, BUNDLE_CONTENT_TYPE,
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
                elements: vec![crate::chat::ChatElement::Technical {
                    category: crate::chat::TechnicalCategory::Stderr,
                    summary: "boom".into(),
                    data: None,
                }],
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
        assert_eq!(value["entries"][0]["elements"][0]["kind"], "technical");
        assert!(value.get("source").is_none());
        assert!(value.get("recorded_at").is_none());
    }

    #[test]
    fn conversation_entries_from_an_older_server_read_without_elements() {
        let entry: ConversationEntryDto = serde_json::from_value(serde_json::json!({
            "entry_number": 1,
            "timestamp": "2026-01-02T03:04:05Z",
            "source": "stdout",
            "raw_base64": "e30=",
        }))
        .unwrap();
        assert!(entry.elements.is_empty());
    }

    #[test]
    fn execution_status_keeps_wire_names_and_terminal_classification() {
        let statuses = [
            (ExecutionStatusDto::Running, "running", false),
            (ExecutionStatusDto::Paused, "paused", false),
            (ExecutionStatusDto::InputNeeded, "input_needed", false),
            (
                ExecutionStatusDto::WaitingForEvent,
                "waiting_for_event",
                false,
            ),
            (ExecutionStatusDto::Completed, "completed", true),
            (ExecutionStatusDto::Failed, "failed", true),
            (ExecutionStatusDto::Cancelled, "cancelled", true),
            (ExecutionStatusDto::Unknown, "unknown", false),
        ];

        for (status, wire, terminal) in statuses {
            assert_eq!(
                serde_json::to_string(&status).unwrap(),
                format!("\"{wire}\"")
            );
            assert_eq!(status.as_str(), wire);
            assert_eq!(status.is_terminal(), terminal);
        }
    }

    #[test]
    fn problem_and_conductor_summary_types_keep_their_wire_names() {
        let problem = ActiveProblem {
            kind: ProblemKindDto::Failure,
            reason: "build failed".into(),
            episode: Some("attempt-4".into()),
        };
        let activity = ConductorActivity {
            purpose: ConductorPurposeDto::DiagnoseFailure,
            trigger: ConductorTriggerDto::StepFailure,
            episode: Some("attempt-4".into()),
            started_at: chrono::DateTime::parse_from_rfc3339("2026-10-12T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        };

        let problem = serde_json::to_value(problem).unwrap();
        assert_eq!(problem["kind"], "failure");
        assert_eq!(problem["reason"], "build failed");
        assert_eq!(problem["episode"], "attempt-4");

        let activity = serde_json::to_value(activity).unwrap();
        assert_eq!(activity["purpose"], "diagnose_failure");
        assert_eq!(activity["trigger"], "step_failure");
        assert_eq!(activity["episode"], "attempt-4");
    }

    #[test]
    fn chat_entry_types_keep_their_wire_names() {
        let entry = ChatEntryDto {
            id: "chat-1".into(),
            sequence: 3,
            at: chrono::DateTime::parse_from_rfc3339("2026-10-12T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            author: ChatAuthorDto::Conductor,
            kind: ChatEntryKindDto::Proposal,
            text: "Run the check?".into(),
            in_reply_to: Some("chat-0".into()),
            payload: Some(serde_json::json!({"command": "cargo check"})),
        };

        let value = serde_json::to_value(entry).unwrap();
        assert_eq!(value["author"], "conductor");
        assert_eq!(value["kind"], "proposal");
        assert_eq!(value["sequence"], 3);
        assert_eq!(value["in_reply_to"], "chat-0");
        assert_eq!(value["payload"]["command"], "cargo check");
    }

    #[test]
    fn annotation_dto_keeps_its_wire_fields() {
        let annotation = AnnotationDto {
            key: "input-1".into(),
            summary: "Review the result".into(),
            recommendation: Some("approve".into()),
            created_at: chrono::DateTime::parse_from_rfc3339("2026-10-12T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        };
        let value = serde_json::to_value(annotation).unwrap();
        assert_eq!(value["key"], "input-1");
        assert_eq!(value["summary"], "Review the result");
        assert_eq!(value["recommendation"], "approve");
        assert!(value.get("created_at").is_some());
    }

    #[test]
    fn timeline_types_keep_their_wire_shape() {
        let timeline = Timeline {
            positions: vec![Position {
                label: "review #1".into(),
                state_id: "review".into(),
                visit: 1,
                action_execution_id: Some("attempt-1".into()),
                action_type: Some("human".into()),
                status: PositionStatus::AwaitingDecision,
                started_at: None,
                ended_at: None,
                error: None,
                resumed_harness_session: None,
                harness_session_interrupted: false,
                retry_pending: false,
                conversation: super::ConversationState::NotCreated {
                    reason: "not an agent action".into(),
                },
            }],
            links: BTreeMap::from([(7, EventLink::Position("review #1".into()))]),
            default_position: Some("review #1".into()),
        };
        let value = serde_json::to_value(timeline).unwrap();
        assert_eq!(value["positions"][0]["status"], "awaiting_decision");
        assert_eq!(
            value["positions"][0]["conversation"]["state"],
            "not_created"
        );
        assert_eq!(value["links"]["7"]["link"], "position");
        assert_eq!(value["default_position"], "review #1");
    }

    #[test]
    fn human_input_actions_keep_their_wire_names() {
        assert_eq!(
            serde_json::to_string(&HumanInputActionDto::Approve).unwrap(),
            "\"approve\""
        );
        assert_eq!(
            serde_json::to_string(&HumanInputActionDto::Resume).unwrap(),
            "\"resume\""
        );
        assert_eq!(
            serde_json::to_string(&HumanInputActionDto::StartNewSession).unwrap(),
            "\"start_new_session\""
        );
    }

    #[test]
    fn daemon_discovery_record_keeps_its_persisted_wire_shape() {
        let record = DaemonDiscoveryRecord {
            pid: 42,
            addr: "127.0.0.1:8080".parse().unwrap(),
            token: "secret-token".into(),
            started_at: chrono::DateTime::parse_from_rfc3339("2026-10-12T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        };
        let value = serde_json::to_value(record).unwrap();
        assert_eq!(value["pid"], 42);
        assert_eq!(value["addr"], "127.0.0.1:8080");
        assert_eq!(value["token"], "secret-token");
        assert_eq!(value["started_at"], "2026-10-12T12:00:00Z");
    }

    #[test]
    fn conductor_action_outcomes_keep_their_wire_shape() {
        let proposal = ProposalOutcome {
            proposal_id: "proposal-1".into(),
            status: ProposalStatus::Executed,
            decision_id: Some("decision-1".into()),
            message: "Output chosen".into(),
            repeated: true,
        };
        let undo = UndoOutcome {
            decision_id: "decision-1".into(),
            state_id: "review".into(),
            episode: "input-2".into(),
            warning: Some("A later step already ran".into()),
            message: "Decision undone".into(),
        };

        let proposal = serde_json::to_value(proposal).unwrap();
        assert_eq!(proposal["status"], "executed");
        assert_eq!(proposal["repeated"], true);
        let undo = serde_json::to_value(undo).unwrap();
        assert_eq!(undo["decision_id"], "decision-1");
        assert_eq!(undo["warning"], "A later step already ran");
    }

    #[test]
    fn human_input_delivery_keeps_optional_wire_fields_and_action_tag() {
        let delivery = HumanInputDeliveryDto {
            values: Map::from_iter([("name".into(), serde_json::json!("Ada"))]),
            chosen_label: Some("approved".into()),
            data: Map::new(),
            variables: Map::new(),
            comment: None,
            agent_message: Some("Use the revised draft".into()),
            action: Some(HumanInputActionDto::Resume),
            episode: Some("input-3".into()),
            permission_decision: None,
        };
        let value = serde_json::to_value(delivery).unwrap();
        assert_eq!(value["values"]["name"], "Ada");
        assert_eq!(value["chosen_label"], "approved");
        assert_eq!(value["agent_message"], "Use the revised draft");
        assert_eq!(value["action"], "resume");
        assert_eq!(value["episode"], "input-3");
        assert!(value.get("answered_by").is_none());
    }

    #[test]
    fn pending_input_view_preserves_flattened_wire_shape_and_old_output_labels() {
        let value = serde_json::json!({
            "execution_id": "execution-1",
            "state_id": "review",
            "action_execution_id": "attempt-1",
            "instruction": "Choose an outcome",
            "fields": [{"name": "decision", "type": "select", "options": ["approve"]}],
            "outputs": ["approve"],
            "continuation": "complete_step",
            "episode": "input-1",
            "requested_at": "2026-10-12T12:00:00Z"
        });
        let pending: PendingHumanInputView = serde_json::from_value(value).unwrap();
        assert_eq!(pending.fields()[0].field_type, HumanInputTypeView::Select);
        assert_eq!(pending.output_labels(), vec!["approve".to_string()]);
        assert_eq!(pending.episode_key(), "input-1");

        let encoded = serde_json::to_value(pending).unwrap();
        assert_eq!(encoded["action_execution_id"], "attempt-1");
        assert_eq!(encoded["outputs"][0]["label"], "approve");
        assert!(encoded.get("request").is_none());
    }

    #[test]
    fn pending_permission_details_survive_the_client_wire_view() {
        let value = serde_json::json!({
            "execution_id": "execution-1",
            "state_id": "build",
            "action_execution_id": "attempt-1",
            "instruction": "Permission decision required",
            "permission": {
                "denials": [{
                    "tool_name": "Bash",
                    "tool_use_id": "tool-1",
                    "input": {"command": "cargo build"},
                    "cwd": "/workspace",
                    "reason_type": "subcommandResults",
                    "reason": "Approval required"
                }]
            },
            "continuation": "resolve_permission",
            "episode": "permission-1",
            "requested_at": "2026-01-02T03:04:05Z"
        });
        let pending: PendingHumanInputView = serde_json::from_value(value).unwrap();

        assert_eq!(
            pending.continuation,
            InputContinuationView::ResolvePermission
        );
        let denial = &pending.permission.unwrap().denials[0];
        assert_eq!(denial.tool_use_id, "tool-1");
        assert_eq!(denial.input["command"], "cargo build");
        assert_eq!(denial.cwd, "/workspace");
    }

    #[test]
    fn auth_api_values_keep_their_wire_fields() {
        let claim = ClaimRequest {
            claim_code: "nsclaim-once".into(),
        };
        let owner = OwnerView {
            issuer: "https://issuer.example".into(),
            subject: "operator-1".into(),
            bound_by: OwnerBindingSource::Configuration,
            bound_at: chrono::DateTime::parse_from_rfc3339("2026-10-12T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            revision: 2,
        };
        let whoami = WhoAmIResponse {
            mode: "network".into(),
            issuer: Some(owner.issuer.clone()),
            subject: Some(owner.subject.clone()),
            owner: true,
            claimed: true,
            expires_at: None,
        };

        let claim = serde_json::to_value(claim).unwrap();
        let owner = serde_json::to_value(owner).unwrap();
        let whoami = serde_json::to_value(whoami).unwrap();
        assert_eq!(claim["claim_code"], "nsclaim-once");
        assert_eq!(owner["bound_by"], "configuration");
        assert_eq!(owner["revision"], 2);
        assert_eq!(whoami["mode"], "network");
        assert_eq!(whoami["owner"], true);
        assert!(whoami.get("expires_at").is_some());
    }

    #[test]
    fn material_api_values_keep_their_wire_shape() {
        let file = MaterialFile {
            path: "reports/result.txt".into(),
            size: 4,
            sha256: "abcd".into(),
        };
        let manifest = UploadManifest {
            material_id: Some("material-1".into()),
            label: None,
            ttl_secs: Some(120),
            files: vec![file.clone()],
        };
        let summary = MaterialSummary {
            material_id: "material-1".into(),
            task_id: "task-1".into(),
            origin: MaterialOrigin::Runner,
            label: None,
            created_at: chrono::DateTime::parse_from_rfc3339("2026-10-12T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            expires_at: chrono::DateTime::parse_from_rfc3339("2026-10-12T12:02:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            file_count: 1,
            total_bytes: 4,
            manifest_digest: "digest".into(),
            delivered_at: None,
        };
        let announcement = MaterialAnnouncement {
            material_id: summary.material_id.clone(),
            manifest_digest: summary.manifest_digest.clone(),
            file_count: 1,
            total_bytes: 4,
        };
        let view = MaterialView {
            summary: summary.clone(),
            files: vec![file],
        };
        let list = MaterialListResponse {
            materials: vec![summary.clone()],
        };
        let uploaded = MaterialUploadResponse {
            material: summary,
            replayed: true,
        };

        let manifest = serde_json::to_value(manifest).unwrap();
        let announcement = serde_json::to_value(announcement).unwrap();
        let view = serde_json::to_value(view).unwrap();
        let list = serde_json::to_value(list).unwrap();
        let uploaded = serde_json::to_value(uploaded).unwrap();
        assert_eq!(manifest["files"][0]["path"], "reports/result.txt");
        assert_eq!(announcement["material_id"], "material-1");
        assert_eq!(view["files"][0]["size"], 4);
        assert_eq!(list["materials"][0]["origin"], "runner");
        assert_eq!(uploaded["replayed"], true);
        assert_eq!(
            BUNDLE_CONTENT_TYPE,
            "application/vnd.narsor.material-bundle"
        );
    }
}
