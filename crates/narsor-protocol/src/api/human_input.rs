//! Human input forms and their operator-facing outcome DTOs.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Operator action applied when delivering a pending human input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanInputActionDto {
    Approve,
    Resume,
    StartNewSession,
}

/// Wire payload for an operator's answer to a pending human input.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HumanInputDeliveryDto {
    #[serde(default)]
    pub values: Map<String, Value>,
    #[serde(default)]
    pub chosen_label: Option<String>,
    /// The step's named artifacts, name → path, for a problem.
    #[serde(default)]
    pub artifacts: BTreeMap<String, String>,
    #[serde(default)]
    pub variables: Map<String, Value>,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub agent_message: Option<String>,
    #[serde(default)]
    pub action: Option<HumanInputActionDto>,
    #[serde(default)]
    pub episode: Option<String>,
    #[serde(default)]
    pub permission_decision: Option<PermissionDecisionDto>,
}

/// Decision scoped to one denial in a pending permission request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionDecisionDto {
    /// The one denial decided on; absent decides every denial at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denial_index: Option<usize>,
    pub decision: PermissionDecisionKindDto,
    #[serde(default)]
    pub scope: Option<PermissionGrantScopeDto>,
    /// The Bash pattern the operator typed, for the `pattern` scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// What the operator tells the agent along with the decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Request body for the dedicated permission endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionDecisionRequestDto {
    pub episode: String,
    pub permission_decision: PermissionDecisionDto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecisionKindDto {
    Deny,
    Allow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionGrantScopeDto {
    ExactOperation,
    CommandGroup,
    /// One rule from the pattern the operator typed (`pattern`).
    Pattern,
}

/// A pending form as exposed to clients, independent of the engine's durable
/// input record and workflow/domain types.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingHumanInputView {
    pub execution_id: String,
    pub state_id: String,
    pub action_execution_id: String,
    pub instruction: String,
    #[serde(default)]
    pub fields: Vec<HumanInputFieldView>,
    /// Why the agent or script step before this gate made its choice.
    #[serde(default)]
    pub reason: Option<String>,
    /// The values that choice required.
    #[serde(default)]
    pub required_values: Vec<RequiredValue>,
    #[serde(default)]
    pub answer_tail: Option<String>,
    #[serde(default)]
    pub agent_harness: Option<String>,
    #[serde(default)]
    pub agent_session_id: Option<String>,
    #[serde(default)]
    pub prefilled_answer: Option<String>,
    #[serde(default)]
    pub permission: Option<PermissionRequestView>,
    #[serde(default, deserialize_with = "deserialize_output_views")]
    pub outputs: Vec<OutputDefinitionView>,
    #[serde(default)]
    pub continuation: InputContinuationView,
    #[serde(default)]
    pub episode: String,
    #[serde(default)]
    pub problem: Option<OutputProblemView>,
    #[serde(default)]
    pub entry_limit: Option<EntryLimitView>,
    #[serde(default)]
    pub approval: Option<ApprovalCandidateView>,
    #[serde(default)]
    pub resume_targets: BTreeMap<String, AgentSessionProvenanceView>,
    pub requested_at: DateTime<Utc>,
}

impl PendingHumanInputView {
    pub fn instruction(&self) -> &str {
        &self.instruction
    }

    pub fn fields(&self) -> &[HumanInputFieldView] {
        &self.fields
    }

    pub fn episode_key(&self) -> &str {
        if self.episode.is_empty() {
            &self.action_execution_id
        } else {
            &self.episode
        }
    }

    pub fn output_labels(&self) -> Vec<String> {
        self.outputs
            .iter()
            .map(|output| output.label.clone())
            .collect()
    }
}

/// Whether a required value is a step variable or a step artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequiredValueKind {
    Variable,
    Artifact,
}

/// A value the choice before a gate required, as the gate shows it: agent
/// text, already cleaned and bounded by the engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequiredValue {
    pub name: String,
    pub kind: RequiredValueKind,
    pub value: String,
}

/// A field in a pending operator form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanInputTypeView {
    Text,
    Select,
}

/// A declared field in a pending operator form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HumanInputFieldView {
    pub name: String,
    #[serde(rename = "type")]
    pub field_type: HumanInputTypeView,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub description: Option<String>,
}

/// A possible output of the suspended state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputDefinitionView {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub requires_human_approval: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_comment_field: Option<String>,
}

fn deserialize_output_views<'de, D>(deserializer: D) -> Result<Vec<OutputDefinitionView>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StoredOutput {
        Label(String),
        Defined(OutputDefinitionView),
    }

    Vec::<StoredOutput>::deserialize(deserializer).map(|outputs| {
        outputs
            .into_iter()
            .map(|output| match output {
                StoredOutput::Label(label) => OutputDefinitionView {
                    label,
                    description: None,
                    requires_human_approval: false,
                    resume_comment_field: None,
                },
                StoredOutput::Defined(output) => output,
            })
            .collect()
    })
}

/// The continuation a submitted answer performs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputContinuationView {
    #[default]
    CompleteStep,
    ResumeAgent,
    ResolveOutput,
    ApproveOutput,
    ResolvePermission,
    EntryLimit,
    #[serde(other)]
    Unrecognized,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryLimitView {
    pub entries: u32,
    pub max_entries: u32,
}

/// The field of an entry-limit wait that carries the operator's decision.
pub const ENTRY_LIMIT_DECISION_FIELD: &str = "entry_limit_decision";
/// The entry-limit decision that lets the step run once more.
pub const ENTRY_LIMIT_CONTINUE: &str = "continue";
/// The entry-limit decision that starts the step's counter again.
pub const ENTRY_LIMIT_RESET_COUNTER: &str = "reset_counter";

/// Claude Code's `decision_reason_type`s of a denial an allow rule for the
/// task can lift: the command only lacked approval and nobody could give it
/// (`asyncAgent` under `--permission-prompts none`, `subcommandResults` for a
/// part of a compound command). Any other reason — an explicit `deny` rule,
/// a hook, a safety check, and `other`, which does not say which — stays a
/// block the owner cannot override (R6).
pub const APPROVABLE_PERMISSION_REASONS: [&str; 2] = ["asyncAgent", "subcommandResults"];

/// Whether a denial with this `decision_reason_type` can be approved.
pub fn is_approvable_permission_reason(reason_type: Option<&str>) -> bool {
    reason_type.is_some_and(|reason| APPROVABLE_PERMISSION_REASONS.contains(&reason))
}

/// Claude Code permission denials tied to a durable pending request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionRequestView {
    pub denials: Vec<PermissionDenialView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionDenialView {
    pub tool_name: String,
    pub tool_use_id: String,
    pub input: Value,
    pub cwd: String,
    #[serde(default)]
    pub reason_type: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// A stopped action that asks an operator to supply its output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputProblemView {
    pub reason: String,
    #[serde(default)]
    pub answers: Vec<RejectedAnswerView>,
    #[serde(default)]
    pub requirements: Vec<OutputRequirementView>,
    #[serde(default)]
    pub analysis: Option<ConductorAnalysisView>,
}

impl OutputProblemView {
    pub fn requirement_for(&self, label: &str) -> Option<&OutputRequirementView> {
        self.requirements
            .iter()
            .find(|requirement| requirement.label == label)
    }
}

/// An agent answer rejected before the operator was asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectedAnswerView {
    pub action_execution_id: String,
    pub reason: String,
    #[serde(default)]
    pub answer_tail: Option<String>,
}

/// Values a label requires before its transition can complete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputRequirementView {
    pub label: String,
    #[serde(default)]
    pub variables: Vec<String>,
    #[serde(default)]
    pub artifacts: Vec<String>,
}

/// The conductor's proposed answer values, advisory until accepted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConductorAnalysisView {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub artifacts: BTreeMap<String, String>,
    #[serde(default)]
    pub variables: Map<String, Value>,
    #[serde(default)]
    pub rationale: Option<String>,
}

/// A completed agent output awaiting an explicit operator decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalCandidateView {
    pub action_execution_id: String,
    pub result: ActionResultView,
    #[serde(default)]
    pub artifacts: Vec<ArtifactView>,
    #[serde(default)]
    pub agent_harness: Option<String>,
    #[serde(default)]
    pub agent_session_id: Option<String>,
}

/// An action result shown while the operator decides whether to approve it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionResultView {
    pub output_label: String,
    /// Why the action made its choice; empty when it gave none.
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub data: Value,
    #[serde(default)]
    pub variables: Map<String, Value>,
    /// The step's named artifacts the action produced: name → path.
    #[serde(default)]
    pub artifacts: BTreeMap<String, String>,
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
}

/// Artifact metadata included with a pending approval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactView {
    pub id: String,
    pub artifact_type: String,
    pub location: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub metadata: Value,
    pub producer: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub size_bytes: Option<u64>,
}

/// Provenance of a native agent session that can be resumed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionProvenanceView {
    pub action_execution_id: String,
    pub harness: String,
    pub session_id: String,
}
