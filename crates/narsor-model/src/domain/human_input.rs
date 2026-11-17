//! The durable human-input records: a question awaiting an answer and an
//! answer on its way in.
//!
//! Plain data shared by the engine (which raises and resumes requests, in
//! `narsor_engine::engine::actions::human`) and the control plane (which stores a
//! runner's pending input). Kept here so `narsor_engine::orchestration` does not
//! depend on `narsor_engine::engine`.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use super::{
    ActionExecutionId, ActionResult, AnsweredBy, Artifact, ChoiceDefinition, ExecutionId,
    HumanInputField, OutputLabel, StateId,
};
use narsor_protocol::api::RequiredValue;
use narsor_protocol::controller::GateWatch;

pub use narsor_protocol::api::HumanInputActionDto as HumanInputAction;
pub use narsor_protocol::api::LocalValue;

/// What `HumanExecutor` observed: the question, and nothing else.
///
/// Deliberately free of everything the executor cannot know — which execution
/// and state this belongs to, and which labels the state declares. That context
/// is added by the engine in [`PendingHumanInput::new`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HumanInputRequest {
    /// Id of the [`crate::domain::ActionExecution`] that suspended.
    pub action_execution_id: ActionExecutionId,

    /// Message shown to the user (`message` in YAML).
    pub instruction: String,

    /// Form fields the user is asked to fill in. May be empty.
    #[serde(default)]
    pub fields: Vec<HumanInputField>,

    /// Why the agent or script step right before this request made its
    /// choice (KTD11). Set by the engine, never by an executor; absent in
    /// records written before the engine carried it, which may hold the
    /// agent's `question` instead (KTD16).
    #[serde(default)]
    pub reason: Option<String>,

    /// The values that choice required, cleaned and bounded.
    #[serde(default)]
    pub required_values: Vec<RequiredValue>,

    /// The end of that step's answer.
    #[serde(default)]
    pub answer_tail: Option<String>,

    /// The harness that produced an agent question. It is durable because
    /// changing a default while the operator types must not move a
    /// continuation to another harness.
    #[serde(default)]
    pub agent_harness: Option<String>,

    /// The harness-native session that can be resumed. Old pending records
    /// deserialize without one and therefore fall back to a fresh attempt.
    #[serde(default)]
    pub agent_session_id: Option<String>,

    /// Text to restore in the form after a saved session could not be
    /// resumed. Older records simply have no prefilled answer.
    #[serde(default)]
    pub prefilled_answer: Option<String>,

    /// Structured Claude Code denial details. Kept in the task's private
    /// pending record so an operator can make a later decision.
    #[serde(default)]
    pub permission: Option<PermissionRequestDetails>,

    /// The `watch` block of a `decision` gate (`by: human`), its `params`
    /// rendered when the gate started. Set by the engine, never by an
    /// executor; kept with the request, so a restarted runner reports the
    /// same values for the same episode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_watch: Option<GateWatch>,
}

/// Details attached to a permission wait. The tool input is untrusted data;
/// render it as text in every client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionRequestDetails {
    pub denials: Vec<PermissionDenialDetails>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionDenialDetails {
    pub tool_name: String,
    pub tool_use_id: String,
    pub input: Value,
    pub cwd: String,
    #[serde(default)]
    pub reason_type: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// An owner decision for one pending Claude Code permission request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionDecision {
    /// The one denial decided on; `None` decides every denial of the
    /// request at once.
    #[serde(default)]
    pub denial_index: Option<usize>,
    pub decision: PermissionDecisionKind,
    #[serde(default)]
    pub scope: Option<PermissionGrantScope>,
    /// The Bash pattern the operator typed, for the `pattern` scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// What the operator tells the agent along with the decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecisionKind {
    Deny,
    Allow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionGrantScope {
    ExactOperation,
    CommandGroup,
    /// One rule from the pattern the operator typed (`pattern`).
    Pattern,
}

/// The durable record of a question awaiting an answer — `pending_input.json`.
///
/// Self-contained on purpose: everything the TUI needs to render the form and
/// everything `resume` needs to validate an answer is here, so neither has to
/// load the workflow definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingHumanInput {
    /// The execution that is waiting.
    pub execution_id: ExecutionId,

    /// The state whose action suspended.
    pub state_id: StateId,

    /// The question, as the executor reported it.
    #[serde(flatten)]
    pub request: HumanInputRequest,

    /// The outputs the state declares, with their descriptions.
    ///
    /// This is the menu a user picks from when the action declares no fields
    /// (§3.3.C, `human_review_tests` in §4), and it is what the TUI offers
    /// alongside the form otherwise. Empty means the caller recorded no menu,
    /// and then `resume` leaves judging the label entirely to U7.
    ///
    /// Records written before labels had descriptions hold plain strings;
    /// [`deserialize_outputs`] reads those as labels without a description.
    #[serde(default, deserialize_with = "deserialize_outputs")]
    pub outputs: Vec<ChoiceDefinition>,

    /// Whether the submitted form completes a human step or resumes the
    /// agent action that asked the question. Older pending records were all
    /// human-step forms, hence the explicit default.
    #[serde(default)]
    pub continuation: InputContinuation,

    /// The key of this request (KTD9), fresh for every request raised, so a
    /// delivery meant for an earlier one is told apart from an answer to
    /// this one. Empty in records written before requests had keys; see
    /// [`PendingHumanInput::episode_key`].
    #[serde(default)]
    pub episode: String,

    /// What settling the step takes, when this request is a problem
    /// ([`InputContinuation::ResolveOutput`]).
    #[serde(default)]
    pub problem: Option<OutputProblem>,

    /// Entry count and configured limit for an entry-limit wait.
    #[serde(default)]
    pub entry_limit: Option<EntryLimitDetails>,

    /// The completed agent result held for an explicit approval decision.
    #[serde(default)]
    pub approval: Option<ApprovalCandidate>,

    #[serde(default)]
    pub resume_targets: BTreeMap<String, AgentSessionProvenance>,

    /// The step's own `scope: local` values from earlier entries, cleaned
    /// and bounded, shown beside the form of a decision.
    #[serde(default)]
    pub local_values: Vec<LocalValue>,

    /// Whether only the menu chooses the label, as in a `decision` step: no
    /// field stands for the label, and the label's `requires_variables` must
    /// be among the delivered values. Records written before decisions had
    /// forms are all `false`, which keeps the label inferred from a lone
    /// `select` field.
    #[serde(default)]
    pub label_from_menu: bool,

    /// When the request was raised.
    pub requested_at: DateTime<Utc>,
}

/// A step that ended without a usable output label, as the operator is asked
/// to settle it (KTD3, R12).
///
/// Self-contained like the rest of [`PendingHumanInput`]: the form shows the
/// agent's answers and what each label needs without loading the workflow
/// definition, and `resume` validates against the same record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputProblem {
    /// Why the step could not go on by itself.
    pub reason: String,

    /// The agent's answers that were rejected, oldest first: the attempt and
    /// the reprompt that followed it, when there was one.
    #[serde(default)]
    pub answers: Vec<RejectedAnswer>,

    /// What choosing each of the state's labels requires, in the order of
    /// [`PendingHumanInput::outputs`].
    #[serde(default)]
    pub requirements: Vec<OutputRequirement>,

    /// The conductor's reading of the answers, when it made one (U10).
    #[serde(default)]
    pub analysis: Option<ConductorAnalysis>,

    /// The step's reply from the rejected attempts, kept as the reply of
    /// the output the operator settles (R3).
    #[serde(default)]
    pub reply: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryLimitDetails {
    pub entries: u32,
    pub max_entries: u32,
}

impl OutputProblem {
    /// What choosing `label` requires, if the problem records it.
    pub fn requirement_for(&self, label: &str) -> Option<&OutputRequirement> {
        self.requirements
            .iter()
            .find(|requirement| requirement.label == label)
    }
}

/// One agent answer that gave no usable output label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectedAnswer {
    /// The attempt that gave it.
    pub action_execution_id: ActionExecutionId,

    /// Why it was rejected.
    pub reason: String,

    /// The end of the answer, when there was one.
    #[serde(default)]
    pub answer_tail: Option<String>,
}

/// The values a delivery choosing `label` must supply (KTD15, R18).
///
/// Worked out when the problem is raised: `variables` holds the variables
/// the choice requires and every `context.NAME` the templates of the state
/// it leads to use without a declaration (KTD6), less those the context or
/// the step's kept memory already holds; `artifacts` holds the artifacts the
/// choice requires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputRequirement {
    /// The label this is about.
    pub label: OutputLabel,

    /// Names the delivery's `variables` (or the context) must hold.
    #[serde(default)]
    pub variables: Vec<String>,

    /// Names the delivery's `artifacts` must hold.
    #[serde(default)]
    pub artifacts: Vec<String>,
}

/// The conductor's proposal for a problem (U10): shown on the form and
/// prefilled, never applied by itself.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConductorAnalysis {
    /// The label it would choose.
    #[serde(default)]
    pub label: Option<OutputLabel>,

    /// The artifacts it would supply, name → path.
    #[serde(default)]
    pub artifacts: BTreeMap<String, String>,

    /// The `variables` it would supply.
    #[serde(default)]
    pub variables: Map<String, Value>,

    /// Why.
    #[serde(default)]
    pub rationale: Option<String>,
}

/// What accepting a pending form does next.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputContinuation {
    /// The answer supplies a label and lets the workflow take its transition.
    #[default]
    CompleteStep,
    /// The answer is fed back to the same agent state; no transition occurs.
    ResumeAgent,
    /// A problem (KTD3): the answer is the output the agent step did not
    /// give — a label with its `artifacts` and `variables` — and completes the
    /// step as if the agent had kept to its output contract.
    ResolveOutput,
    ApproveOutput,
    /// The workflow reached a state's configured entry limit.
    EntryLimit,
    /// A Claude Code permission request; only the dedicated permission
    /// resolution operation may close it.
    ResolvePermission,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalCandidate {
    pub action_execution_id: String,
    pub result: ActionResult,
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
    #[serde(default)]
    pub agent_harness: Option<String>,
    #[serde(default)]
    pub agent_session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionProvenance {
    pub action_execution_id: String,
    pub harness: String,
    pub session_id: String,
}

impl PendingHumanInput {
    /// Builds the durable record from what the executor reported plus the state
    /// context only the engine has.
    pub fn new(
        execution_id: impl Into<ExecutionId>,
        state_id: impl Into<StateId>,
        request: HumanInputRequest,
        outputs: Vec<ChoiceDefinition>,
    ) -> Self {
        Self {
            execution_id: execution_id.into(),
            state_id: state_id.into(),
            request,
            outputs,
            continuation: InputContinuation::CompleteStep,
            episode: new_episode(),
            problem: None,
            entry_limit: None,
            approval: None,
            resume_targets: BTreeMap::new(),
            local_values: Vec::new(),
            label_from_menu: false,
            requested_at: Utc::now(),
        }
    }

    /// The same request as the form of a `decision` step: only the menu
    /// chooses the label, and `local_values` are what the step keeps.
    pub fn deciding(mut self, local_values: Vec<LocalValue>) -> Self {
        self.label_from_menu = true;
        self.local_values = local_values;
        self
    }

    /// Builds a question raised by an agent that must be resumed in-place.
    pub fn for_agent(
        execution_id: impl Into<ExecutionId>,
        state_id: impl Into<StateId>,
        request: HumanInputRequest,
    ) -> Self {
        Self {
            execution_id: execution_id.into(),
            state_id: state_id.into(),
            request,
            outputs: Vec::new(),
            continuation: InputContinuation::ResumeAgent,
            episode: new_episode(),
            problem: None,
            entry_limit: None,
            approval: None,
            resume_targets: BTreeMap::new(),
            local_values: Vec::new(),
            label_from_menu: false,
            requested_at: Utc::now(),
        }
    }

    /// Builds a permission wait that cannot be answered through ordinary
    /// human input delivery.
    pub fn for_permission(
        execution_id: impl Into<ExecutionId>,
        state_id: impl Into<StateId>,
        request: HumanInputRequest,
    ) -> Self {
        Self {
            execution_id: execution_id.into(),
            state_id: state_id.into(),
            request,
            outputs: Vec::new(),
            continuation: InputContinuation::ResolvePermission,
            episode: new_episode(),
            problem: None,
            entry_limit: None,
            approval: None,
            resume_targets: BTreeMap::new(),
            local_values: Vec::new(),
            label_from_menu: false,
            requested_at: Utc::now(),
        }
    }

    /// Builds a problem: the agent step of `state_id` ended without a usable
    /// output label, and the operator is asked for one out of `outputs`.
    pub fn for_problem(
        execution_id: impl Into<ExecutionId>,
        state_id: impl Into<StateId>,
        request: HumanInputRequest,
        outputs: Vec<ChoiceDefinition>,
        problem: OutputProblem,
    ) -> Self {
        Self {
            execution_id: execution_id.into(),
            state_id: state_id.into(),
            request,
            outputs,
            continuation: InputContinuation::ResolveOutput,
            episode: new_episode(),
            problem: Some(problem),
            entry_limit: None,
            approval: None,
            resume_targets: BTreeMap::new(),
            local_values: Vec::new(),
            label_from_menu: false,
            requested_at: Utc::now(),
        }
    }

    pub fn for_approval(
        execution_id: impl Into<ExecutionId>,
        state_id: impl Into<StateId>,
        request: HumanInputRequest,
        candidate: ApprovalCandidate,
    ) -> Self {
        Self {
            execution_id: execution_id.into(),
            state_id: state_id.into(),
            request,
            outputs: Vec::new(),
            continuation: InputContinuation::ApproveOutput,
            episode: new_episode(),
            problem: None,
            entry_limit: None,
            approval: Some(candidate),
            resume_targets: BTreeMap::new(),
            local_values: Vec::new(),
            label_from_menu: false,
            requested_at: Utc::now(),
        }
    }

    pub fn for_entry_limit(
        execution_id: impl Into<ExecutionId>,
        state_id: impl Into<StateId>,
        request: HumanInputRequest,
        outputs: Vec<ChoiceDefinition>,
        problem: OutputProblem,
        details: EntryLimitDetails,
    ) -> Self {
        let mut pending = Self::for_problem(execution_id, state_id, request, outputs, problem);
        pending.continuation = InputContinuation::EntryLimit;
        pending.entry_limit = Some(details);
        pending
    }

    /// The key a delivery names this request by (KTD9).
    ///
    /// A record written before requests had keys falls back to the attempt
    /// that suspended, which is unique per request just the same.
    pub fn episode_key(&self) -> &str {
        if self.episode.is_empty() {
            self.action_execution_id()
        } else {
            &self.episode
        }
    }

    /// The message shown to the user.
    pub fn instruction(&self) -> &str {
        &self.request.instruction
    }

    /// The fields the user is asked to fill in.
    pub fn fields(&self) -> &[HumanInputField] {
        &self.request.fields
    }

    /// The action execution that suspended.
    pub fn action_execution_id(&self) -> &str {
        &self.request.action_execution_id
    }

    /// The declared output labels, without their descriptions.
    pub fn output_labels(&self) -> Vec<OutputLabel> {
        self.outputs
            .iter()
            .map(|output| output.label.clone())
            .collect()
    }
}

/// A fresh key for a newly raised request.
fn new_episode() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Reads [`PendingHumanInput::outputs`] in either shape it was ever written
/// in: the current `{label, description}` objects, or the bare labels of
/// records written before outputs had descriptions.
fn deserialize_outputs<'de, D>(deserializer: D) -> Result<Vec<ChoiceDefinition>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StoredOutput {
        Label(OutputLabel),
        Defined(ChoiceDefinition),
    }

    let stored = Vec::<StoredOutput>::deserialize(deserializer)?;
    Ok(stored
        .into_iter()
        .map(|output| match output {
            StoredOutput::Label(label) => ChoiceDefinition::new(label),
            StoredOutput::Defined(definition) => definition,
        })
        .collect())
}

/// An answer on its way in.
///
/// Both shapes of §3.3.C are expressed here; `resolve_label` documents how a
/// single label comes out of either:
///
/// * a form with fields — [`HumanInputDelivery::with_values`], optionally
///   pinned to a label with [`HumanInputDelivery::choosing`];
/// * no fields at all — [`HumanInputDelivery::label`], which picks straight
///   from the state's `choices`.
///
/// A problem ([`InputContinuation::ResolveOutput`]) takes the third shape: a
/// label with the step's `artifacts` and `variables`, which no other request
/// accepts.
///
/// Every field is optional on the wire, so a client written before a field
/// existed still delivers — without `episode`, though, its answer is not
/// protected against landing on a newer request than the one it saw.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HumanInputDelivery {
    /// Values for the declared fields, by field name.
    #[serde(default)]
    pub values: Map<String, Value>,

    /// The output label the user picked, when they picked one explicitly.
    #[serde(default)]
    pub chosen_label: Option<OutputLabel>,

    /// The step's named artifacts, name → path, for a problem.
    #[serde(default)]
    pub artifacts: BTreeMap<String, String>,

    /// The step's `variables`, for a problem.
    #[serde(default)]
    pub variables: Map<String, Value>,

    /// A free-text remark, kept in the event log with the answer.
    #[serde(default)]
    pub comment: Option<String>,

    /// A message that restarts the same agent step instead of choosing its
    /// output. It is deliberately distinct from `comment`: comments explain a
    /// manual output decision, while this text is prompt input for an agent.
    #[serde(default)]
    pub agent_message: Option<String>,

    #[serde(default)]
    pub action: Option<HumanInputAction>,

    /// The key of the request this answers ([`PendingHumanInput::episode_key`]).
    /// When given, a delivery to any other request is refused.
    #[serde(default)]
    pub episode: Option<String>,

    /// Present only on the dedicated permission decision endpoint. Ordinary
    /// human input rejects permission continuations regardless of this field.
    #[serde(default)]
    pub permission_decision: Option<PermissionDecision>,

    /// Who is answering. Never read from the wire: whoever calls the API is
    /// the operator, and only in-process callers answer as the conductor.
    #[serde(skip)]
    pub answered_by: AnsweredBy,
}

pub const APPROVE_OUTPUT_LABEL: &str = "__approve_output";

impl HumanInputDelivery {
    /// A delivery carrying filled-in field values.
    pub fn with_values(values: Map<String, Value>) -> Self {
        Self {
            values,
            ..Self::default()
        }
    }

    /// A delivery that only picks a label, for an action without fields.
    pub fn label(label: impl Into<OutputLabel>) -> Self {
        Self {
            chosen_label: Some(label.into()),
            ..Self::default()
        }
    }

    /// The same delivery with an explicit label attached.
    pub fn choosing(mut self, label: impl Into<OutputLabel>) -> Self {
        self.chosen_label = Some(label.into());
        self
    }

    /// The same delivery carrying the step's artifacts.
    pub fn with_artifacts(mut self, artifacts: BTreeMap<String, String>) -> Self {
        self.artifacts = artifacts;
        self
    }

    /// The same delivery carrying the step's `variables`.
    pub fn with_variables(mut self, variables: Map<String, Value>) -> Self {
        self.variables = variables;
        self
    }

    /// The same delivery with a remark attached.
    pub fn with_comment(mut self, comment: impl Into<String>) -> Self {
        self.comment = Some(comment.into());
        self
    }

    /// The same delivery carrying a follow-up message for an agent step.
    pub fn with_agent_message(mut self, message: impl Into<String>) -> Self {
        self.agent_message = Some(message.into());
        self
    }

    pub fn with_action(mut self, action: HumanInputAction) -> Self {
        self.action = Some(action);
        self
    }

    /// The same delivery, bound to the request with this key.
    pub fn for_episode(mut self, episode: impl Into<String>) -> Self {
        self.episode = Some(episode.into());
        self
    }

    /// The same delivery, made by `answered_by`.
    pub fn answered_by(mut self, answered_by: AnsweredBy) -> Self {
        self.answered_by = answered_by;
        self
    }
}
