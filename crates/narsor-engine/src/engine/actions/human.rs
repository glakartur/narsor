//! `HumanAction` executor and the suspend/resume mechanism (`WORKFLOW_SCHEMA.md` §3.3.C).
//!
//! A human action is the one action that cannot be *run*. Entering such a state
//! records what the user is being asked for, moves the execution to
//! [`ExecutionStatus::InputNeeded`] and gives control back to the caller; the answer
//! arrives later — possibly in a different process, after a restart — through
//! [`resume`].
//!
//! # The two halves
//!
//! * [`HumanExecutor`] is the [`ActionExecutor`] half. It is pure: it reads the
//!   configuration and reports [`ActionOutcome::Suspended`], doing no I/O and
//!   knowing nothing about the state it belongs to.
//! * [`suspend`] and [`resume`] are the durable half. They are what the engine
//!   loop (U7) calls around the executor, and they own the `InputNeeded` status, the
//!   pending request on disk and the two events of the human interaction.
//!
//! # Three kinds of request
//!
//! The same record serves every wait for a person, told apart by its
//! [`InputContinuation`]: a form or decision that completes a step, an
//! agent's own question that resumes it, and a *problem* — an agent step that
//! gave no usable output label, whose answer supplies that output (KTD3).
//! Each request gets a key ([`PendingHumanInput::episode_key`]); a delivery
//! naming it is refused once the request is gone (KTD9).
//!
//! # Where the pending request lives
//!
//! In its own `pending_input.json` beside `state.json`, not in a field of
//! [`WorkflowExecution`]. Two reasons decided it:
//!
//! * the TUI lists executions by scanning the store and needs to answer "is
//!   this one waiting for me, and what does it want?" — one small file read per
//!   execution, without deserializing the whole execution record and without
//!   loading the workflow definition to look the fields up;
//! * [`WorkflowExecution`] is serialized by every other unit, and a new field
//!   there would ripple through all of them.
//!
//! The usual objection to a second file is drift, and it is answered by making
//! the file *not* authoritative: `status == InputNeeded` in `state.json` is the
//! truth, and [`resume`] checks the status before it ever looks at the pending
//! file. A leftover `pending_input.json` next to an execution that does not need input is
//! therefore inert, which is exactly what protects against a second delivery
//! after a crash.
//!
//! # Crash safety
//!
//! [`suspend`] writes the pending file *before* it flips the status to
//! `InputNeeded`. A crash in between leaves a pending file that nothing reads —
//! harmless. The other order would leave an execution needing input with no record of
//! what it is waiting for, which nothing could repair.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

use super::harness::CLAUDE_CODE;
use super::{ActionExecutor, ActionOutcome, ActionRequest, SuspendedAction};
use crate::domain::{
    ActionConfig, ActionError, ActionExecutionId, ActionResult, AnsweredBy, Artifact,
    ChoiceDefinition, EventData, ExecutionId, ExecutionStatus, HumanActionConfig, HumanInputField,
    HumanInputType, InputCloseReason, OutputLabel, StateId, WorkflowExecution,
};
use crate::persistence::{ExecutionStore, PersistenceError};

pub use narsor_protocol::api::HumanInputActionDto as HumanInputAction;
pub use narsor_protocol::api::{RequiredValue, RequiredValueKind};

/// Executes `HumanAction` states.
///
/// Stateless — one instance serves the whole engine.
#[derive(Debug, Clone, Copy, Default)]
pub struct HumanExecutor;

impl HumanExecutor {
    /// Creates an executor.
    pub fn new() -> Self {
        Self
    }
}

impl ActionExecutor for HumanExecutor {
    fn execute<'a>(
        &'a self,
        request: ActionRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ActionOutcome, ActionError>> + Send + 'a>> {
        Box::pin(async move {
            let config = human_config(request.action)?;

            Ok(ActionOutcome::Suspended(
                SuspendedAction::AwaitingHumanInput(HumanInputRequest {
                    action_execution_id: request.action_execution_id.to_string(),
                    instruction: config.instruction.clone(),
                    fields: config.inputs.clone(),
                    reason: None,
                    required_values: Vec::new(),
                    answer_tail: None,
                    agent_harness: None,
                    agent_session_id: None,
                    prefilled_answer: None,
                    permission: None,
                }),
            ))
        })
    }
}

/// Narrows the action configuration to a human configuration.
fn human_config(action: &ActionConfig) -> Result<&HumanActionConfig, ActionError> {
    match action {
        ActionConfig::Human(config) => Ok(config),
        other => Err(ActionError {
            message: format!(
                "HumanExecutor received a {} action",
                action_type_name(other)
            ),
            error_type: Some("wrong_action_type".to_string()),
            details: None,
        }),
    }
}

/// The schema name of an action type, for error messages.
fn action_type_name(action: &ActionConfig) -> &'static str {
    match action {
        ActionConfig::Agent(_) => "agent",
        ActionConfig::Script(_) => "script",
        ActionConfig::Human(_) => "human",
        ActionConfig::Wait(_) => "wait",
    }
}

/// What [`HumanExecutor`] observed: the question, and nothing else.
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

/// Whether a task grant may lift `denial` at all: a Bash command Claude Code
/// held for approval, not a policy block (R6).
fn is_grantable_bash(denial: &PermissionDenialDetails) -> bool {
    denial.tool_name == "Bash"
        && narsor_protocol::api::is_approvable_permission_reason(denial.reason_type.as_deref())
}

/// The Claude Code allow rules that grant one denial at `scope`: a rule per
/// part of its Bash command ([`narsor_protocol::api::bash_grant_rules`]).
/// `None` for a denial no task grant may lift (R6) or a command a rule could
/// not pin down — never a wider rule — and for [`PermissionGrantScope::Pattern`],
/// whose rule comes from the decision, not the command ([`granted_rules`]).
pub fn claude_permission_rules(
    denial: &PermissionDenialDetails,
    scope: PermissionGrantScope,
) -> Option<Vec<String>> {
    if !is_grantable_bash(denial) {
        return None;
    }
    let command = denial.input.get("command")?.as_str()?;
    let scope = match scope {
        PermissionGrantScope::ExactOperation => narsor_protocol::api::BashGrantScope::Exact,
        PermissionGrantScope::CommandGroup => narsor_protocol::api::BashGrantScope::Group,
        PermissionGrantScope::Pattern => return None,
    };
    narsor_protocol::api::bash_grant_rules(command, scope)
}

/// The denials `decision` is about: the one it names, or all of them.
fn decided_denials<'a>(
    request: &'a HumanInputRequest,
    decision: &PermissionDecision,
) -> Result<Vec<&'a PermissionDenialDetails>, HumanActionError> {
    let denials = request
        .permission
        .as_ref()
        .map(|permission| permission.denials.as_slice())
        .unwrap_or_default();
    match decision.denial_index {
        Some(index) => denials
            .get(index)
            .map(|denial| vec![denial])
            .ok_or_else(|| HumanActionError::MissingRequiredField {
                field: "denial_index".to_string(),
            }),
        None if denials.is_empty() => Err(HumanActionError::MissingRequiredField {
            field: "denial_index".to_string(),
        }),
        None => Ok(denials.iter().collect()),
    }
}

/// The allow rules an approving `decision` grants, every decided denial's
/// rules in order and without repeats. A denial that cannot be granted at
/// the chosen scope refuses the whole decision. The `pattern` scope grants
/// the one rule the operator typed, for denials a task grant may lift.
pub fn granted_rules(
    request: &HumanInputRequest,
    decision: &PermissionDecision,
) -> Result<Vec<String>, HumanActionError> {
    let (PermissionDecisionKind::Allow, Some(scope)) = (decision.decision, decision.scope) else {
        return Ok(Vec::new());
    };
    let typed = match scope {
        PermissionGrantScope::Pattern => Some(
            decision
                .pattern
                .as_deref()
                .and_then(narsor_protocol::api::bash_pattern_rule)
                .ok_or_else(|| HumanActionError::MissingRequiredField {
                    field: "pattern".to_string(),
                })?,
        ),
        _ => None,
    };
    let mut rules: Vec<String> = Vec::new();
    for denial in decided_denials(request, decision)? {
        let denial_rules = match &typed {
            Some(rule) => is_grantable_bash(denial).then(|| vec![rule.clone()]),
            None => claude_permission_rules(denial, scope),
        };
        for rule in denial_rules.ok_or(HumanActionError::PermissionDecisionRequired)? {
            if !rules.contains(&rule) {
                rules.push(rule);
            }
        }
    }
    Ok(rules)
}

/// The durable record of a question awaiting an answer — `pending_input.json`.
///
/// Self-contained on purpose: everything the TUI needs to render the form and
/// everything [`resume`] needs to validate an answer is here, so neither has to
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
    /// and then [`resume`] leaves judging the label entirely to U7.
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

    /// When the request was raised.
    pub requested_at: DateTime<Utc>,
}

/// A step that ended without a usable output label, as the operator is asked
/// to settle it (KTD3, R12).
///
/// Self-contained like the rest of [`PendingHumanInput`]: the form shows the
/// agent's answers and what each label needs without loading the workflow
/// definition, and [`resume`] validates against the same record.
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
            requested_at: Utc::now(),
        }
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
/// Both shapes of §3.3.C are expressed here; [`resolve_label`] documents how a
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

/// What [`resume`] hands back to the engine loop.
#[derive(Debug, Clone)]
pub struct ResumedHumanAction {
    /// The execution, now back to [`ExecutionStatus::Running`] and saved.
    pub execution: WorkflowExecution,

    /// The request that was satisfied, as it was read from disk.
    pub pending: PendingHumanInput,

    /// The result of the human action.
    ///
    /// `variables` carries the field values; per D10 the engine — not this
    /// module — merges them into `context.variables`.
    pub result: ActionResult,

    /// Present only when the dedicated permission decision path was used.
    pub permission_decision: Option<PermissionDecision>,
}

/// Everything that can go wrong around a human action.
#[derive(Debug, Error)]
pub enum HumanActionError {
    #[error("invalid pending continuation: {reason}")]
    InvalidContinuation { reason: String },
    /// A permission wait must be resolved through its dedicated decision
    /// operation; ordinary human input must never grant harness access.
    #[error("this request requires a permission decision")]
    PermissionDecisionRequired,

    /// The execution is not awaiting input, so there is nothing to answer.
    ///
    /// This is the guard against a double delivery: once an answer has moved
    /// the execution back to `Running`, a second delivery of the same answer —
    /// after a restart, or from a second client — is refused here.
    #[error(
        "execution '{execution_id}' is {status} and cannot be resumed: \
         delivering human input requires status input_needed"
    )]
    NotPaused {
        /// The execution that was addressed.
        execution_id: ExecutionId,
        /// Its actual status, as the schema spells it.
        status: String,
    },

    /// The execution needs input but no pending request is stored for it.
    #[error("execution '{execution_id}' is input_needed but has no pending human input request")]
    NoPendingRequest {
        /// The execution that was addressed.
        execution_id: ExecutionId,
    },

    /// A field declared `required: true` was not delivered.
    #[error("required field '{field}' is missing")]
    MissingRequiredField {
        /// Name of the missing field.
        field: String,
    },

    /// A value was delivered for a field the action does not declare.
    #[error(
        "field '{field}' is not declared by this human action{}",
        name_list(declared)
    )]
    UndeclaredField {
        /// The unexpected field name.
        field: String,
        /// The names the action does declare.
        declared: Vec<String>,
    },

    /// A `select` field was given a value outside its `options`.
    #[error(
        "'{value}' is not a permitted value for field '{field}'; permitted values are: {}",
        options.join(", ")
    )]
    ValueNotPermitted {
        /// The field that was filled in.
        field: String,
        /// The value that was delivered.
        value: String,
        /// The values the field allows.
        options: Vec<String>,
    },

    /// A field was given a value of the wrong shape (a `select` needs a string).
    #[error("field '{field}' expects a {expected} value")]
    WrongValueType {
        /// The field that was filled in.
        field: String,
        /// What the field expects.
        expected: &'static str,
    },

    /// No label could be derived from the delivery.
    #[error(
        "the delivered input does not select an output label; \
         pick one of the state's outputs explicitly{}",
        name_list(outputs)
    )]
    NoLabelChosen {
        /// The labels the state declares, when they were recorded.
        outputs: Vec<OutputLabel>,
    },

    /// An explicitly picked label is not one the state declares.
    #[error("'{label}' is not an output this state declares{}", name_list(outputs))]
    LabelNotDeclared {
        /// The label that was picked.
        label: OutputLabel,
        /// The labels the state declares.
        outputs: Vec<OutputLabel>,
    },

    /// The delivery answers a request that is no longer pending (KTD9): it
    /// was answered or closed, and possibly replaced by another one.
    #[error(
        "the input '{delivered}' of execution '{execution_id}' is no longer pending; \
         the execution now waits on '{current}'"
    )]
    StaleEpisode {
        /// The execution that was addressed.
        execution_id: ExecutionId,
        /// The key the delivery named.
        delivered: String,
        /// The key of the request pending now.
        current: String,
    },

    /// A problem's answer chose a label without a value it requires
    /// (KTD15).
    #[error("label '{label}' needs '{key}'")]
    MissingRequiredKey {
        /// The label that was chosen.
        label: OutputLabel,
        /// The missing value, as `variables.NAME` or `artifacts.NAME`.
        key: String,
    },

    /// `artifacts` or `variables` were delivered to a request that is not a
    /// problem, where nothing would read them.
    #[error(
        "artifacts and variables can only be delivered to resolve a step's output; \
         this request takes field values and a label"
    )]
    UnexpectedStepValues,

    /// Reading or writing the store failed.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
}

impl HumanActionError {
    /// Whether the delivery itself is at fault — a form a person can correct
    /// and send again — rather than the execution's state or the store.
    pub fn is_invalid_delivery(&self) -> bool {
        matches!(
            self,
            Self::MissingRequiredField { .. }
                | Self::UndeclaredField { .. }
                | Self::ValueNotPermitted { .. }
                | Self::WrongValueType { .. }
                | Self::NoLabelChosen { .. }
                | Self::LabelNotDeclared { .. }
                | Self::MissingRequiredKey { .. }
                | Self::UnexpectedStepValues
        )
    }

    /// Whether the delivery came too late: the execution no longer waits on
    /// the request it answers.
    pub fn is_stale(&self) -> bool {
        matches!(
            self,
            Self::NotPaused { .. } | Self::NoPendingRequest { .. } | Self::StaleEpisode { .. }
        )
    }
}

/// Renders `; declared: a, b` for an error message, or nothing when empty.
fn name_list(names: &[String]) -> String {
    if names.is_empty() {
        String::new()
    } else {
        format!("; declared: {}", names.join(", "))
    }
}

/// Suspends the execution on a human action: records the request, marks input needed, logs.
///
/// Mutates `execution` in place and persists it, so the caller's copy and the
/// disk agree afterwards. The order of writes is load-bearing — see the module
/// documentation.
pub fn suspend(
    store: &ExecutionStore,
    execution: &mut WorkflowExecution,
    pending: &PendingHumanInput,
) -> Result<(), HumanActionError> {
    store.save_pending_input(&execution.id, pending)?;

    execution.status = ExecutionStatus::InputNeeded;
    execution.current_state = Some(pending.state_id.clone());
    execution.current_action_execution = Some(pending.action_execution_id().to_string());
    store.save_execution(execution)?;

    store.event_log(&execution.id).append_next(
        &execution.id,
        EventData::HumanInputRequested {
            instruction: pending.instruction().to_string(),
            reason: pending.request.reason.clone(),
            required_values: pending.request.required_values.clone(),
            answer_tail: pending.request.answer_tail.clone(),
            episode: Some(pending.episode_key().to_string()),
        },
    )?;

    Ok(())
}

/// Reads the pending request of an execution, if it has one that can still be
/// answered.
///
/// Returns `None` both when nothing is pending and when the execution is no
/// longer needs input, so a caller scanning the store never offers a form that
/// [`resume`] would refuse.
pub fn pending_input(
    store: &ExecutionStore,
    execution_id: &str,
) -> Result<Option<PendingHumanInput>, HumanActionError> {
    let execution = store.load_execution(execution_id)?;
    if execution.status != ExecutionStatus::InputNeeded {
        return Ok(None);
    }
    Ok(store.load_pending_input(execution_id)?)
}

/// Delivers an answer, resuming the execution.
///
/// Refuses any execution that is not [`ExecutionStatus::InputNeeded`], and a
/// delivery naming another request than the pending one (KTD9) — both on the
/// same read the answer is applied to. For a problem
/// ([`InputContinuation::ResolveOutput`]) the answer is the step's output:
/// the result carries the chosen label with the delivered `artifacts` and
/// `variables`. On success
/// the execution is back to `Running` on disk, `HumanInputReceived` is in the
/// log, the pending file is gone, and the caller gets the [`ActionResult`] the
/// engine loop transitions on. A rejected delivery changes nothing: the
/// execution remains input-needed and the request stays answerable.
pub fn resume(
    store: &ExecutionStore,
    execution_id: &str,
    delivery: &HumanInputDelivery,
) -> Result<ResumedHumanAction, HumanActionError> {
    resume_inner(store, execution_id, delivery, false)
}

/// Resolves a durable Claude Code permission request. This is deliberately a
/// separate entry point from [`resume`], which cannot answer that continuation.
pub fn resolve_permission(
    store: &ExecutionStore,
    execution_id: &str,
    decision: PermissionDecision,
    episode: &str,
) -> Result<ResumedHumanAction, HumanActionError> {
    let pending = store
        .load_pending_input::<PendingHumanInput>(execution_id)?
        .ok_or_else(|| HumanActionError::NoPendingRequest {
            execution_id: execution_id.to_string(),
        })?;
    if pending.continuation != InputContinuation::ResolvePermission {
        return Err(HumanActionError::PermissionDecisionRequired);
    }
    let denials = decided_denials(&pending.request, &decision)?;
    let commands: Vec<String> = denials
        .iter()
        .map(|denial| {
            denial
                .input
                .get("command")
                .and_then(Value::as_str)
                .map_or_else(|| denial.tool_name.clone(), ToString::to_string)
        })
        .collect();
    let granted = match (decision.decision, decision.scope) {
        (PermissionDecisionKind::Deny, None) => Vec::new(),
        (PermissionDecisionKind::Allow, Some(_)) => {
            if pending.request.agent_harness.as_deref() != Some(CLAUDE_CODE)
                || pending
                    .request
                    .agent_session_id
                    .as_deref()
                    .is_none_or(str::is_empty)
            {
                return Err(HumanActionError::PermissionDecisionRequired);
            }
            granted_rules(&pending.request, &decision)?
        }
        _ => {
            return Err(HumanActionError::MissingRequiredField {
                field: "scope".to_string(),
            })
        }
    };
    let listed = |commands: &[&String]| {
        commands
            .iter()
            .map(|command| format!("- {command}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut answer = match decision.decision {
        PermissionDecisionKind::Deny => {
            // A command no rule can pin down was never the operator's to
            // approve: say so, and how to ask in a form that can be.
            let (unpinnable, denied): (Vec<_>, Vec<_>) =
                denials.iter().zip(&commands).partition(|(denial, _)| {
                    is_grantable_bash(denial)
                        && claude_permission_rules(denial, PermissionGrantScope::ExactOperation)
                            .is_none()
                });
            let unpinnable: Vec<_> = unpinnable.into_iter().map(|(_, command)| command).collect();
            let denied: Vec<_> = denied.into_iter().map(|(_, command)| command).collect();
            let mut parts = Vec::new();
            if !denied.is_empty() {
                parts.push(format!(
                    "The operator denied these blocked actions:\n{}\n\
                     Choose another approach or explain what is blocked.",
                    listed(&denied)
                ));
            }
            if !unpinnable.is_empty() {
                parts.push(format!(
                    "The operator could not approve these commands, because no permission \
                     rule can describe them exactly:\n{}\n\
                     If you still need them, run what you need as separate simple commands: \
                     no shell variables, loops, $(...), globs, parentheses or redirection to \
                     files. The operator can approve such commands when they are blocked.",
                    listed(&unpinnable)
                ));
            }
            parts.join("\n\n")
        }
        PermissionDecisionKind::Allow => format!(
            "The operator approved these blocked actions for this task:\n{}\n\
             Allow rules for the rest of this task: {}\n\
             Run them again now; they are allowed in this session.",
            listed(&commands.iter().collect::<Vec<_>>()),
            granted.join(", ")
        ),
    };
    if let Some(message) = decision.message.as_deref().map(str::trim) {
        if !message.is_empty() {
            answer.push_str("\n\nMessage from the operator:\n");
            answer.push_str(message);
        }
    }
    let delivery = HumanInputDelivery {
        agent_message: Some(answer),
        episode: Some(episode.to_string()),
        permission_decision: Some(decision),
        answered_by: AnsweredBy::Operator,
        ..HumanInputDelivery::default()
    };
    resume_inner(store, execution_id, &delivery, true)
}

fn resume_inner(
    store: &ExecutionStore,
    execution_id: &str,
    delivery: &HumanInputDelivery,
    allow_permission: bool,
) -> Result<ResumedHumanAction, HumanActionError> {
    let mut execution = store.load_execution(execution_id)?;

    if execution.status != ExecutionStatus::InputNeeded {
        return Err(HumanActionError::NotPaused {
            execution_id: execution_id.to_string(),
            status: execution.status.as_str().to_string(),
        });
    }

    let pending: PendingHumanInput = store.load_pending_input(execution_id)?.ok_or_else(|| {
        HumanActionError::NoPendingRequest {
            execution_id: execution_id.to_string(),
        }
    })?;

    if pending.execution_id != execution.id {
        return Err(HumanActionError::NoPendingRequest {
            execution_id: execution_id.to_string(),
        });
    }

    if pending.continuation == InputContinuation::ResolvePermission && !allow_permission {
        return Err(HumanActionError::PermissionDecisionRequired);
    }
    if pending.continuation == InputContinuation::ResolvePermission
        && execution.current_state.as_deref() != Some(pending.state_id.as_str())
    {
        return Err(HumanActionError::PermissionDecisionRequired);
    }
    if pending.continuation == InputContinuation::ResolvePermission && delivery.episode.is_none() {
        return Err(HumanActionError::MissingRequiredField {
            field: "episode".to_string(),
        });
    }

    // Checked on the same read the answer is applied to (KTD9): nothing
    // between here and the writes below can bring the request back.
    if let Some(delivered) = &delivery.episode {
        if delivered != pending.episode_key() {
            return Err(HumanActionError::StaleEpisode {
                execution_id: execution_id.to_string(),
                delivered: delivered.clone(),
                current: pending.episode_key().to_string(),
            });
        }
    }

    // Validation first, and before any write: a rejected delivery must leave
    // the execution exactly as paused as it was.
    let optional_resume_fields: Vec<&str> = intended_label(&pending, delivery)
        .map(|label| {
            pending
                .outputs
                .iter()
                .filter(|output| output.label != label)
                .filter_map(|output| output.resume_comment_field.as_deref())
                .collect()
        })
        .unwrap_or_default();
    let mut values = validate_values(pending.fields(), &delivery.values, &optional_resume_fields)?;
    let agent_message = delivery
        .agent_message
        .as_deref()
        .map(str::trim)
        .filter(|message| !message.is_empty());
    if agent_message.is_some()
        && (pending.continuation != InputContinuation::ResolveOutput
            && !(allow_permission && pending.continuation == InputContinuation::ResolvePermission)
            || delivery.chosen_label.is_some()
            || !delivery.artifacts.is_empty()
            || !delivery.variables.is_empty()
            || !values.is_empty())
    {
        return Err(HumanActionError::UnexpectedStepValues);
    }
    if pending.continuation == InputContinuation::ResolvePermission {
        if let Some(decision) = &delivery.permission_decision {
            values.insert(
                "permission_decision".to_string(),
                serde_json::json!({
                    "denial_index": decision.denial_index,
                    "decision": decision.decision,
                    "scope": decision.scope,
                }),
            );
        }
    }
    validate_agent_answer(&pending, &values)?;
    let (label, data, variables) = match pending.continuation {
        InputContinuation::CompleteStep => {
            reject_step_values(delivery)?;
            let label = resolve_label(&pending, delivery, &values)?;
            require_resume_comment(&pending, &label, &values)?;
            let answered = with_blank_optional_text(pending.fields(), &values);
            (label, Value::Object(answered.clone()), answered)
        }
        // This label is internal only. The engine observes `continuation` and
        // resumes the agent before any decision/transition code can read it.
        InputContinuation::ResumeAgent => {
            reject_step_values(delivery)?;
            (
                RESUME_AGENT_LABEL.to_string(),
                Value::Object(values.clone()),
                values.clone(),
            )
        }
        InputContinuation::ResolvePermission if allow_permission => (
            RESUME_AGENT_LABEL.to_string(),
            Value::Object(Map::new()),
            Map::from_iter([(
                "answer".to_string(),
                Value::String(agent_message.unwrap_or_default().to_string()),
            )]),
        ),
        InputContinuation::ResolveOutput if agent_message.is_some() => (
            RESUME_AGENT_LABEL.to_string(),
            Value::Object(Map::new()),
            Map::from_iter([(
                "answer".to_string(),
                Value::String(agent_message.unwrap_or_default().to_string()),
            )]),
        ),
        InputContinuation::ResolveOutput => {
            let label = resolve_output(&pending, delivery, &execution)?;
            (label, Value::Object(Map::new()), delivery.variables.clone())
        }
        InputContinuation::EntryLimit => {
            let decision = values
                .get(ENTRY_LIMIT_DECISION_FIELD)
                .and_then(Value::as_str)
                .unwrap_or_default();
            if delivery.chosen_label.is_none() {
                let label = match decision {
                    ENTRY_LIMIT_CONTINUE => ENTRY_LIMIT_CONTINUE_LABEL,
                    ENTRY_LIMIT_RESET_COUNTER => ENTRY_LIMIT_RESET_LABEL,
                    _ => {
                        return Err(HumanActionError::NoLabelChosen {
                            outputs: vec![ENTRY_LIMIT_CONTINUE, ENTRY_LIMIT_RESET_COUNTER]
                                .into_iter()
                                .map(str::to_string)
                                .collect(),
                        })
                    }
                };
                (label.to_string(), Value::Object(Map::new()), Map::new())
            } else {
                let label = resolve_output(&pending, delivery, &execution)?;
                (label, Value::Object(Map::new()), delivery.variables.clone())
            }
        }
        InputContinuation::ApproveOutput => {
            reject_step_values(delivery)?;
            match delivery.action {
                Some(HumanInputAction::Approve) => (
                    APPROVE_OUTPUT_LABEL.to_string(),
                    Value::Object(Map::new()),
                    Map::new(),
                ),
                Some(HumanInputAction::Resume) => {
                    let comment = delivery.comment.as_deref().map(str::trim).unwrap_or("");
                    if comment.is_empty() {
                        return Err(HumanActionError::MissingRequiredField {
                            field: "comment".to_string(),
                        });
                    }
                    (
                        RESUME_AGENT_LABEL.to_string(),
                        Value::Object(Map::new()),
                        Map::from_iter([(
                            "answer".to_string(),
                            Value::String(comment.to_string()),
                        )]),
                    )
                }
                _ => {
                    return Err(HumanActionError::MissingRequiredField {
                        field: "action".to_string(),
                    })
                }
            }
        }
        InputContinuation::ResolvePermission => {
            return Err(HumanActionError::PermissionDecisionRequired)
        }
    };

    let resolves_output = pending.continuation == InputContinuation::ResolveOutput
        || (pending.continuation == InputContinuation::EntryLimit
            && delivery.chosen_label.is_some());
    let artifacts = if resolves_output {
        delivery.artifacts.clone()
    } else {
        BTreeMap::new()
    };
    store.event_log(execution_id).append_next(
        &execution.id,
        EventData::HumanInputReceived {
            input: Value::Object(values.clone()),
            answered_by: Some(delivery.answered_by),
            close_reason: Some(InputCloseReason::Answered),
            episode: Some(pending.episode_key().to_string()),
            // The entry-limit decisions are internal labels, never a
            // declared output.
            output_label: (!matches!(
                pending.continuation,
                InputContinuation::ResumeAgent | InputContinuation::ApproveOutput
            ) && label != ENTRY_LIMIT_CONTINUE_LABEL
                && label != ENTRY_LIMIT_RESET_LABEL)
                .then(|| label.clone()),
            artifacts: resolves_output.then(|| json_object(&artifacts)),
            variables: resolves_output.then(|| Value::Object(variables.clone())),
            comment: delivery
                .comment
                .as_deref()
                .map(str::trim)
                .filter(|comment| !comment.is_empty())
                .map(ToString::to_string),
        },
    )?;

    if pending.continuation == InputContinuation::EntryLimit
        && (label == ENTRY_LIMIT_CONTINUE_LABEL || label == ENTRY_LIMIT_RESET_LABEL)
    {
        let details =
            pending
                .entry_limit
                .as_ref()
                .ok_or_else(|| HumanActionError::InvalidContinuation {
                    reason: "entry-limit wait is missing its details".to_string(),
                })?;
        let reset = label == ENTRY_LIMIT_RESET_LABEL;
        let count = if reset {
            1
        } else {
            details.entries.saturating_add(1)
        };
        let engine = execution
            .metadata
            .as_object_mut()
            .expect("execution metadata is an object")
            .entry("engine".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        let entries = engine
            .as_object_mut()
            .expect("engine metadata is an object")
            .entry("state_entries".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        entries
            .as_object_mut()
            .expect("state entry counters are an object")
            .insert(pending.state_id.clone(), Value::from(count));
        if reset {
            store.event_log(execution_id).append_next(
                &execution.id,
                EventData::EntryLimitCounterReset {
                    state_id: pending.state_id.clone(),
                    previous_entries: details.entries,
                },
            )?;
        }
        store.event_log(execution_id).append_next(
            &execution.id,
            EventData::EntryLimitDecision {
                state_id: pending.state_id.clone(),
                decision: if reset {
                    ENTRY_LIMIT_RESET_COUNTER
                } else {
                    ENTRY_LIMIT_CONTINUE
                }
                .to_string(),
            },
        )?;
    }

    execution.status = ExecutionStatus::Running;
    store.save_execution(&execution)?;

    // Last, so that a crash before this point leaves the request answerable
    // rather than losing it.
    store.clear_pending_input(execution_id)?;

    let result = ActionResult {
        data,
        variables,
        artifacts,
        ..ActionResult::new(label)
    };

    Ok(ResumedHumanAction {
        execution,
        pending,
        result,
        permission_decision: delivery.permission_decision.clone(),
    })
}

/// The event that closes `pending` without an answer (R5): something else —
/// a transition, a recovery — made the request moot, and a form still open
/// on it has to be able to say so.
pub fn closing_event(
    pending: &PendingHumanInput,
    reason: InputCloseReason,
    closed_by: AnsweredBy,
) -> EventData {
    EventData::HumanInputReceived {
        input: Value::Object(Map::new()),
        answered_by: Some(closed_by),
        close_reason: Some(reason),
        episode: Some(pending.episode_key().to_string()),
        output_label: None,
        artifacts: None,
        variables: None,
        comment: None,
    }
}

/// Closes the request pending on `execution_id`, if there is one, with
/// [`closing_event`] — the log first, so a crash in between leaves a request
/// that is closed in the log and merely not yet removed, never a removed one
/// the log does not account for.
///
/// Returns the request that was closed.
pub fn close_pending(
    store: &ExecutionStore,
    execution_id: &str,
    reason: InputCloseReason,
    closed_by: AnsweredBy,
) -> Result<Option<PendingHumanInput>, HumanActionError> {
    let Some(pending) = store.load_pending_input::<PendingHumanInput>(execution_id)? else {
        return Ok(None);
    };
    store.event_log(execution_id).append_next(
        &execution_id.to_string(),
        closing_event(&pending, reason, closed_by),
    )?;
    store.clear_pending_input(execution_id)?;
    Ok(Some(pending))
}

/// The label [`resume`] reports for an agent's answer to its own question:
/// internal only, never checked against a state or taken as a transition.
pub(crate) const RESUME_AGENT_LABEL: &str = "__resume_agent__";
pub(crate) const ENTRY_LIMIT_CONTINUE_LABEL: &str = "__entry_limit_continue__";
pub(crate) const ENTRY_LIMIT_RESET_LABEL: &str = "__entry_limit_reset__";
pub use narsor_protocol::api::{
    ENTRY_LIMIT_CONTINUE, ENTRY_LIMIT_DECISION_FIELD, ENTRY_LIMIT_RESET_COUNTER,
};

/// Refuses `artifacts`/`variables` on a request that is not a problem:
/// nothing would read them, and silently dropping an operator's values is
/// worse than saying so.
fn reject_step_values(delivery: &HumanInputDelivery) -> Result<(), HumanActionError> {
    if delivery.artifacts.is_empty() && delivery.variables.is_empty() {
        Ok(())
    } else {
        Err(HumanActionError::UnexpectedStepValues)
    }
}

/// Validates the answer to a problem and returns the label it chooses.
///
/// The label must be one the state declares, and the delivery must hold
/// every value the problem recorded for it (KTD15): a variable in
/// `variables` or already in the execution's context — read from the same
/// record the answer is applied to — and an artifact in `artifacts`.
fn resolve_output(
    pending: &PendingHumanInput,
    delivery: &HumanInputDelivery,
    execution: &WorkflowExecution,
) -> Result<OutputLabel, HumanActionError> {
    let Some(label) = delivery.chosen_label.clone() else {
        return Err(HumanActionError::NoLabelChosen {
            outputs: pending.output_labels(),
        });
    };
    if !pending.outputs.iter().any(|output| output.label == label) {
        return Err(HumanActionError::LabelNotDeclared {
            label,
            outputs: pending.output_labels(),
        });
    }

    let requirement = pending
        .problem
        .as_ref()
        .and_then(|problem| problem.requirement_for(&label));
    match requirement.and_then(|requirement| missing_value(requirement, delivery, execution)) {
        Some(key) => Err(HumanActionError::MissingRequiredKey { label, key }),
        None => Ok(label),
    }
}

/// The first value `requirement` names that neither the delivery nor the
/// context holds, as `variables.NAME` or `artifacts.NAME`.
fn missing_value(
    requirement: &OutputRequirement,
    delivery: &HumanInputDelivery,
    execution: &WorkflowExecution,
) -> Option<String> {
    let variable_held = |name: &&String| {
        delivery
            .variables
            .get(name.as_str())
            .or_else(|| execution.context.variables.get(name.as_str()))
            .is_some_and(|value| !is_blank(value))
    };
    let artifact_given = |name: &&String| {
        delivery
            .artifacts
            .get(name.as_str())
            .is_some_and(|path| !path.trim().is_empty())
    };
    let missing_variable = requirement
        .variables
        .iter()
        .find(|name| !variable_held(name))
        .map(|name| format!("variables.{name}"));
    missing_variable.or_else(|| {
        requirement
            .artifacts
            .iter()
            .find(|name| !artifact_given(name))
            .map(|name| format!("artifacts.{name}"))
    })
}

/// Refuses a gate's choice that resumes an agent's session with its comment
/// when the comment is blank (KTD12): the field may be optional for the
/// gate's other choices, never for this one.
fn require_resume_comment(
    pending: &PendingHumanInput,
    label: &str,
    values: &Map<String, Value>,
) -> Result<(), HumanActionError> {
    let comment_field = pending
        .outputs
        .iter()
        .find(|output| output.label == label)
        .and_then(|output| output.resume_comment_field.as_deref());
    match comment_field {
        Some(field) if values.get(field).is_none_or(is_blank) => {
            Err(HumanActionError::MissingRequiredField {
                field: field.to_string(),
            })
        }
        _ => Ok(()),
    }
}

/// `artifacts` as a JSON object, for the event log.
fn json_object(artifacts: &BTreeMap<String, String>) -> Value {
    Value::Object(
        artifacts
            .iter()
            .map(|(name, path)| (name.clone(), Value::String(path.clone())))
            .collect(),
    )
}

/// Checks the delivered values against the declared fields.
///
/// Returns the values destined for `ActionResult.variables`, which is exactly
/// the declared fields that were filled in: an undeclared name is an error
/// rather than a silent extra context variable.
fn validate_values(
    fields: &[HumanInputField],
    delivered: &Map<String, Value>,
    optional_resume_fields: &[&str],
) -> Result<Map<String, Value>, HumanActionError> {
    let declared: Vec<String> = fields.iter().map(|field| field.name.clone()).collect();

    for name in delivered.keys() {
        if !declared.iter().any(|field| field == name) {
            return Err(HumanActionError::UndeclaredField {
                field: name.clone(),
                declared,
            });
        }
    }

    let mut values = Map::new();

    for field in fields {
        let Some(value) = delivered.get(&field.name).filter(|value| !is_blank(value)) else {
            if field.required && !optional_resume_fields.contains(&field.name.as_str()) {
                return Err(HumanActionError::MissingRequiredField {
                    field: field.name.clone(),
                });
            }
            continue;
        };

        if field.field_type == HumanInputType::Select {
            let Some(text) = value.as_str() else {
                return Err(HumanActionError::WrongValueType {
                    field: field.name.clone(),
                    expected: "string",
                });
            };

            if !field.options.iter().any(|option| option == text) {
                return Err(HumanActionError::ValueNotPermitted {
                    field: field.name.clone(),
                    value: text.to_string(),
                    options: field.options.clone(),
                });
            }
        }

        values.insert(field.name.clone(), value.clone());
    }

    Ok(values)
}

/// `values` with an empty string for every optional `text` field left blank.
///
/// A gate that closes its step was answered in full: a blank optional note is
/// an empty answer, not a missing one, so a later `{{ context.NAME }}` renders
/// as nothing instead of failing the action as an unknown variable. Writing it
/// also clears a note left by an earlier pass through the same gate.
fn with_blank_optional_text(
    fields: &[HumanInputField],
    values: &Map<String, Value>,
) -> Map<String, Value> {
    let mut answered = values.clone();
    for field in fields {
        if !field.required && field.field_type == HumanInputType::Text {
            answered
                .entry(field.name.clone())
                .or_insert_with(|| Value::String(String::new()));
        }
    }
    answered
}

/// An agent form with a menu accepts either free text, or one selected option
/// (with an optional comment). This is checked before any durable write so a
/// blank submission remains answerable.
fn validate_agent_answer(
    pending: &PendingHumanInput,
    values: &Map<String, Value>,
) -> Result<(), HumanActionError> {
    if pending.continuation != InputContinuation::ResumeAgent {
        return Ok(());
    }
    let filled = |name: &str| {
        values
            .get(name)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    };
    if !filled("answer") && !filled("option") {
        return Err(HumanActionError::MissingRequiredField {
            field: "answer or option".to_string(),
        });
    }
    Ok(())
}

/// A value that counts as "not filled in": null, or an all-whitespace string.
pub(crate) fn is_blank(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(text) => text.trim().is_empty(),
        _ => false,
    }
}

/// Works out the single output label an answer selects (§3.3.C).
///
/// The schema says only that "output labels correspond to the values chosen by
/// the user", which covers two shapes that look different in the YAML of §4:
///
/// * **With fields.** `approval_status` is a `select` over
///   `[approved, changes_requested, rejected]` and the state's outputs are the
///   same words, so the label *is* the chosen value. It is taken from the one
///   `select` field, and any other field (`comments`) is only data. That value
///   has already been checked against the field's own `options`, so it is not
///   checked against `outputs` here — matching a label to a state is U7's job.
/// * **Without fields.** `human_review_tests` declares no `inputs` at all and
///   the user simply picks `ignore` or `fix_it` from the state's `choices`.
///   There is nothing to infer from, so the label must be delivered explicitly
///   through [`HumanInputDelivery::label`], and it *is* checked against the
///   recorded menu — with no fields, that menu is the whole form.
///
/// `human_clarification` in §4 is the case that forces both to exist at once:
/// it has one *text* field and two outputs (`clarified`, `abort`), so the user
/// fills a field in **and** picks a label. An explicit label therefore always
/// wins over an inferred one.
/// The label a delivery means before its values are validated: the one chosen
/// explicitly, or else the value of the gate's only `select` field — the same
/// inference [`resolve_label`] makes. It decides which resume comments the
/// delivery may leave blank, so a gate answered through its `select` field is
/// not held to the comment of an output it did not take.
fn intended_label<'a>(
    pending: &PendingHumanInput,
    delivery: &'a HumanInputDelivery,
) -> Option<&'a str> {
    if let Some(label) = delivery.chosen_label.as_deref() {
        return Some(label);
    }
    let mut selects = pending
        .fields()
        .iter()
        .filter(|field| field.field_type == HumanInputType::Select);
    match (selects.next(), selects.next()) {
        (Some(field), None) => delivery.values.get(&field.name)?.as_str(),
        _ => None,
    }
}

fn resolve_label(
    pending: &PendingHumanInput,
    delivery: &HumanInputDelivery,
    values: &Map<String, Value>,
) -> Result<OutputLabel, HumanActionError> {
    if let Some(label) = &delivery.chosen_label {
        if !pending.outputs.is_empty()
            && !pending.outputs.iter().any(|output| &output.label == label)
        {
            return Err(HumanActionError::LabelNotDeclared {
                label: label.clone(),
                outputs: pending.output_labels(),
            });
        }
        return Ok(label.clone());
    }

    let selects: Vec<&HumanInputField> = pending
        .fields()
        .iter()
        .filter(|field| field.field_type == HumanInputType::Select)
        .collect();

    if let [field] = selects[..] {
        if let Some(Value::String(value)) = values.get(&field.name) {
            return Ok(value.clone());
        }
    }

    Err(HumanActionError::NoLabelChosen {
        outputs: pending.output_labels(),
    })
}
