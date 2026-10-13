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
    #[serde(default)]
    pub data: Map<String, Value>,
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
    #[serde(default)]
    pub question: Option<String>,
    #[serde(default)]
    pub answer_tail: Option<String>,
    #[serde(default)]
    pub agent_harness: Option<String>,
    #[serde(default)]
    pub agent_session_id: Option<String>,
    #[serde(default)]
    pub prefilled_answer: Option<String>,
    #[serde(default, deserialize_with = "deserialize_output_views")]
    pub outputs: Vec<OutputDefinitionView>,
    #[serde(default)]
    pub continuation: InputContinuationView,
    #[serde(default)]
    pub episode: String,
    #[serde(default)]
    pub problem: Option<OutputProblemView>,
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

/// Input keys a label requires before its transition can complete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputRequirementView {
    pub label: String,
    #[serde(default)]
    pub data: Vec<String>,
    #[serde(default)]
    pub variables: Vec<String>,
}

/// The conductor's proposed answer values, advisory until accepted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConductorAnalysisView {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub data: Map<String, Value>,
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
    #[serde(default)]
    pub data: Value,
    #[serde(default)]
    pub variables: Map<String, Value>,
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
