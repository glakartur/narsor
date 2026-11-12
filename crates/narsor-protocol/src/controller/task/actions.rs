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

/// One action of a task as the server offers it, described whole: what it is
/// called, what it does, whether it opens a submenu, what the owner fills in
/// and what a client sends for it. A client draws it and does not need to
/// know what the action means.
///
/// The server offers only the actions that can be done now, so `available`
/// is `true` and `reason` is absent on every entry of a current server. The
/// two fields stay for older clients, which read them.
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
    /// The name of the action, in words for the owner. Absent from a server
    /// that does not describe its actions: a client then has nothing to draw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// What the action does, in words for the owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The action cannot be undone.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub irreversible: bool,
    /// Where the action stands in its list: lower first.
    #[serde(default)]
    pub order: u32,
    /// Whether the action is done (a leaf) or opens a submenu.
    #[serde(default)]
    pub kind: TaskActionKind,
    /// Where a client shows the action.
    #[serde(default)]
    pub surface: TaskActionSurface,
    /// What the owner fills in before the action is done (a leaf only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<TaskActionField>,
    /// The entries of a submenu: the same shape, the same id as their parent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<TaskActionOffer>,
    /// What a client sends for a leaf; absent for a submenu and for an
    /// action a client does its own way (an answer to a question).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send: Option<TaskActionSend>,
}

impl TaskActionOffer {
    /// An entry `id`, available and described: a leaf of the panel with no
    /// field, until the builders say otherwise.
    pub fn described(
        id: TaskActionId,
        label: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            id,
            available: true,
            reason: None,
            effect: None,
            options: Vec::new(),
            label: Some(label.into()),
            description: Some(description.into()),
            irreversible: false,
            order: 0,
            kind: TaskActionKind::Execute,
            surface: TaskActionSurface::Panel,
            fields: Vec::new(),
            children: Vec::new(),
            send: None,
        }
    }

    /// Whether the server described the entry, so a client can draw it.
    pub fn is_described(&self) -> bool {
        self.label.is_some()
    }
}

/// Whether an action is done or opens a submenu.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskActionKind {
    /// A final choice: done after its fields and a confirmation.
    #[default]
    Execute,
    /// A list of further choices, in `children`.
    Submenu,
    #[serde(other)]
    Unrecognized,
}

/// Where a client shows an action.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskActionSurface {
    /// The action panel (`Ctrl+.`).
    #[default]
    Panel,
    /// The conversation: a comment, an answer, an approval, a permission.
    Conversation,
    #[serde(other)]
    Unrecognized,
}

/// What a field of an action accepts.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskActionFieldKind {
    /// One line of text.
    Text,
    /// Text of many lines.
    Multiline,
    /// One of `options`.
    Select,
    #[serde(other)]
    Unrecognized,
}

/// One field the owner fills in before an action is done.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskActionField {
    /// The name shown above the value; the placeholder in `send` names it.
    pub name: String,
    pub kind: TaskActionFieldKind,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    /// What the field is for, in words for the owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// The values of a `select`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}

impl TaskActionField {
    /// A field `name` of `kind`, optional and without a hint.
    pub fn new(name: impl Into<String>, kind: TaskActionFieldKind) -> Self {
        Self {
            name: name.into(),
            kind,
            required: false,
            hint: None,
            options: Vec::new(),
        }
    }

    /// The same field, required.
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// The same field, with `hint`.
    pub fn hinted(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// The same field, choosing from `options`.
    pub fn with_options(mut self, options: Vec<String>) -> Self {
        self.options = options;
        self
    }
}

/// What a client sends for a leaf action: one of the commands it already
/// knows how to send. A string of the command that is exactly
/// [`placeholder`]`(name)` stands for the value of the field `name`; a blank
/// optional field makes it `null`.
///
/// The command is kept as JSON, so a command a client does not know does not
/// stop it from reading the rest of the summary; it only cannot send that
/// action ([`TaskActionSend::resolve`]). A new kind of command still needs a
/// new client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaskActionSend {
    /// An owner command (`POST …/commands`): an [`super::OwnerCommand`].
    Owner { command: serde_json::Value },
    /// A command for the runner (`POST …/operator-commands`): an
    /// [`super::OperatorCommand`].
    Operator { command: serde_json::Value },
    /// A comment (`POST …/comments`) whose text is the field `field`.
    Comment { field: String },
    #[serde(other)]
    Unrecognized,
}

/// What a leaf action sends once its fields are filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedActionSend {
    Owner(super::OwnerCommand),
    Operator(super::OperatorCommand),
    Comment(String),
}

/// The placeholder of the field `name` in the command of a [`TaskActionSend`].
pub fn placeholder(name: &str) -> String {
    format!("{{{{{name}}}}}")
}

impl TaskActionSend {
    /// An owner command.
    pub fn owner(command: super::OwnerCommand) -> Self {
        Self::Owner {
            command: serde_json::to_value(command).expect("an owner command is JSON"),
        }
    }

    /// A command for the runner; its strings may hold [`placeholder`]s.
    pub fn operator(command: &super::OperatorCommand) -> Self {
        Self::Operator {
            command: serde_json::to_value(command).expect("an operator command is JSON"),
        }
    }

    /// The command with the values of the fields put in: `values` are
    /// `(name, value)`, a blank optional field left out. `Err` says, in words
    /// for the owner, why it cannot be sent.
    pub fn resolve(&self, values: &[(String, String)]) -> Result<ResolvedActionSend, String> {
        const NEWER: &str = "ta akcja wymaga nowszej wersji klienta";
        let value_of = |name: &str| {
            values
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, value)| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        match self {
            Self::Owner { command } => serde_json::from_value(fill(command, &value_of))
                .map(ResolvedActionSend::Owner)
                .map_err(|_| NEWER.to_string()),
            Self::Operator { command } => serde_json::from_value(fill(command, &value_of))
                .map(ResolvedActionSend::Operator)
                .map_err(|_| NEWER.to_string()),
            Self::Comment { field } => value_of(field)
                .map(ResolvedActionSend::Comment)
                .ok_or_else(|| format!("pole „{field}” jest puste")),
            Self::Unrecognized => Err(NEWER.to_string()),
        }
    }
}

/// `value` with every string that is a [`placeholder`] replaced by the value
/// of its field, or by `null` when the field is blank.
fn fill(value: &serde_json::Value, value_of: &dyn Fn(&str) -> Option<String>) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::String(text) => match text
            .strip_prefix("{{")
            .and_then(|rest| rest.strip_suffix("}}"))
        {
            Some(name) => value_of(name).map_or(Value::Null, Value::String),
            None => value.clone(),
        },
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| fill(item, value_of)).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), fill(item, value_of)))
                .collect(),
        ),
        other => other.clone(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::{OperatorCommand, OwnerCommand};
    use serde_json::json;

    fn select_step() -> TaskActionOffer {
        let mut leaf = TaskActionOffer::described(TaskActionId::SelectStep, "work", "Wznawia.");
        leaf.fields = vec![
            TaskActionField::new("komentarz", TaskActionFieldKind::Multiline).hinted("dla agenta"),
        ];
        leaf.send = Some(TaskActionSend::operator(&OperatorCommand::SelectStep {
            target: "work".to_string(),
            note: Some(placeholder("komentarz")),
        }));
        let mut menu = TaskActionOffer::described(TaskActionId::SelectStep, "Wybierz krok", "…");
        menu.kind = TaskActionKind::Submenu;
        menu.order = 40;
        menu.children = vec![leaf];
        menu
    }

    #[test]
    fn a_submenu_with_a_leaf_and_a_field_goes_both_ways() {
        let offer = select_step();
        let text = serde_json::to_value(&offer).unwrap();
        let read: TaskActionOffer = serde_json::from_value(text).unwrap();
        assert_eq!(read, offer);
    }

    #[test]
    fn an_offer_of_an_older_server_reads_with_the_new_fields_empty() {
        let read: TaskActionOffer = serde_json::from_value(json!({
            "id": "pause",
            "available": false,
            "reason": "zadanie jest w kolejce"
        }))
        .unwrap();
        assert!(!read.is_described());
        assert_eq!(read.kind, TaskActionKind::Execute);
        assert_eq!(read.surface, TaskActionSurface::Panel);
        assert!(read.fields.is_empty() && read.children.is_empty() && read.send.is_none());
    }

    #[test]
    fn unknown_kinds_read_as_unrecognized() {
        let read: TaskActionOffer = serde_json::from_value(json!({
            "id": "something_new",
            "available": true,
            "label": "X",
            "kind": "wizard",
            "surface": "sidebar",
            "fields": [{"name": "a", "kind": "calendar"}],
            "send": {"type": "webhook", "url": "x"}
        }))
        .unwrap();
        assert_eq!(read.id, TaskActionId::Unrecognized);
        assert_eq!(read.kind, TaskActionKind::Unrecognized);
        assert_eq!(read.surface, TaskActionSurface::Unrecognized);
        assert_eq!(read.fields[0].kind, TaskActionFieldKind::Unrecognized);
        assert_eq!(read.send, Some(TaskActionSend::Unrecognized));
    }

    /// The shape an older client reads the offer with.
    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    struct OlderOffer {
        id: TaskActionId,
        available: bool,
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        effect: Option<String>,
        #[serde(default)]
        options: Vec<TaskActionOption>,
    }

    #[test]
    fn an_older_client_reads_the_new_offer() {
        let text = serde_json::to_value(select_step()).unwrap();
        let read: OlderOffer = serde_json::from_value(text).unwrap();
        assert_eq!(read.id, TaskActionId::SelectStep);
        assert!(read.available);
    }

    #[test]
    fn a_command_unknown_to_the_client_still_reads_but_cannot_be_sent() {
        let read: TaskActionSend =
            serde_json::from_value(json!({"type": "operator", "command": {"kind": "teleport"}}))
                .unwrap();
        assert!(read.resolve(&[]).is_err());
    }

    #[test]
    fn the_values_of_the_fields_fill_the_placeholders() {
        let send = select_step().children[0].send.clone().unwrap();
        let with_note = send
            .resolve(&[("komentarz".to_string(), " popraw testy ".to_string())])
            .unwrap();
        assert_eq!(
            with_note,
            ResolvedActionSend::Operator(OperatorCommand::SelectStep {
                target: "work".to_string(),
                note: Some("popraw testy".to_string()),
            })
        );
        let blank = send.resolve(&[]).unwrap();
        assert_eq!(
            blank,
            ResolvedActionSend::Operator(OperatorCommand::SelectStep {
                target: "work".to_string(),
                note: None,
            })
        );
        assert_eq!(
            TaskActionSend::owner(OwnerCommand::Pause).resolve(&[]),
            Ok(ResolvedActionSend::Owner(OwnerCommand::Pause))
        );
        let comment = TaskActionSend::Comment {
            field: "tekst".to_string(),
        };
        assert!(comment.resolve(&[]).is_err());
        assert_eq!(
            comment.resolve(&[("tekst".to_string(), "hej".to_string())]),
            Ok(ResolvedActionSend::Comment("hej".to_string()))
        );
    }
}
