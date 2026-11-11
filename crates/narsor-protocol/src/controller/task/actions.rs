//! Transitions, steps, actions and panel forms the server offers for a task.

use serde::{Deserialize, Serialize};

/// A manual transition a workflow declares from its current state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransitionOffer {
    pub from_state: String,
    pub exits: Vec<TransitionExit>,
    pub override_allowed: bool,
    #[serde(default)]
    pub override_targets: Vec<String>,
}

impl TransitionOffer {
    /// Whether the state allows an override with somewhere to go.
    pub fn has_override(&self) -> bool {
        self.override_allowed && !self.override_targets.is_empty()
    }
}

/// An action of the owner on a task that the server can tell apart.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TaskActionId {
    Comment,
    Answer,
    Approve,
    Reject,
    AllowPermission,
    DenyPermission,
    Pause,
    Resume,
    Interrupt,
    Transition,
    Restart,
    Archive,
    /// Cut the work short and stop at a safe point (ends in `Stopped`).
    Stop,
    /// Free the slot of a task that waits for the owner, before the wait
    /// policy does.
    ReleaseSlot,
    /// Choose any step of the workflow to go on from.
    SelectStep,
    #[serde(other)]
    Unrecognized,
}

/// What a step of the workflow does, for a client that must not read the
/// workflow definition itself.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Agent,
    /// A step whose agent decides the model and effort of later steps. A
    /// client that predates it reads it as [`StepKind::Unrecognized`].
    Profile,
    Script,
    Human,
    Wait,
    Decision,
    Terminal,
    #[serde(other)]
    Unrecognized,
}

impl StepKind {
    /// Whether the step runs an agent: an `agent` step, and a `profile`
    /// step, whose agent profiles the others.
    pub fn runs_agent(self) -> bool {
        match self {
            Self::Agent | Self::Profile => true,
            Self::Script
            | Self::Human
            | Self::Wait
            | Self::Decision
            | Self::Terminal
            | Self::Unrecognized => false,
        }
    }
}

/// One step of the workflow as the owner can choose it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StepView {
    pub id: String,
    pub label: String,
    pub kind: StepKind,
}

/// One action of a task as the server offers it: whether it can be done now
/// and, when it cannot, why.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskActionOffer {
    pub id: TaskActionId,
    pub available: bool,
    /// Why the action cannot be done, in words for the owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// What the action does here, in words for the owner, when it depends on
    /// the stop the task is in (a comment).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<String>,
    /// The choices the action has (the exits of a transition, the outputs of
    /// a question).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<TaskActionOption>,
}

/// One choice of a task action.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskActionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The forms of the owner's panel that the server offers for a task.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PanelOffer {
    /// In the order the owner steps through them.
    #[serde(default)]
    pub forms: Vec<PanelForm>,
    /// Index in `forms` of the form that opens first; `None` when there is
    /// no form.
    #[serde(default)]
    pub default: Option<usize>,
}

/// The kind of a panel form.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PanelFormKind {
    /// A choice of what happens to the step: an exit, an output, a restart.
    Action,
    /// An answer to a question or an approval of the agent.
    Answer,
    /// A free message to the agent.
    Chat,
    /// The decision at the entry limit of a step.
    EntryLimit,
    /// The decision on a permission the agent asked for.
    Permission,
    #[serde(other)]
    Unrecognized,
}

/// One choice of a panel form (an exit of a step, an output of the agent).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PanelChoice {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The action the choice does, when it is one of the task's actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<TaskActionId>,
}

/// One form of the panel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PanelForm {
    pub kind: PanelFormKind,
    /// The wait the form answers; absent for a form that answers none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_id: Option<String>,
    /// The choices of the form of the task's own actions; empty for a form
    /// of a wait or the chat, whose content is its `definition`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<PanelChoice>,
    /// What the form of a wait asks and how its answer is delivered: the
    /// server's definition, which the client shows and does not change. Absent
    /// for the chat and the form of the task's own actions, and from a server
    /// from before the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<crate::panel::PanelDefinition>,
}

impl PanelForm {
    /// Whether this is the form of the task's own actions (a manual
    /// transition), not the output choice of a wait: both are an
    /// [`PanelFormKind::Action`], told apart by answering no wait.
    pub fn is_manual_action(&self) -> bool {
        self.kind == PanelFormKind::Action && self.waiting_id.is_none()
    }
}

/// One declared transition out of a workflow state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransitionExit {
    pub label: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}
