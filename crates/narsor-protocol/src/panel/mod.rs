//! What a form of the owner's panel asks, and how its answer becomes the
//! delivery the server takes.
//!
//! The server builds a [`PanelDefinition`] for every wait the panel answers
//! (`controller::PanelForm::definition`) with [`define`]; a client only shows
//! it and sends the chosen entry back through [`deliver`]. The same two
//! functions serve a client that talks to a server from before the field: it
//! builds the definition from the wait itself, so the rules have one place.
//!
//! The definition says what an entry is called and what it needs: the lines of
//! the wait's context, the entries to choose from, the fields of each entry,
//! the plan that turns the chosen entry into a delivery and the line the
//! notification shows after it was sent. Drafts, keys and drawing are the
//! client's.

mod action;
mod answer;
mod entry_limit;
mod permission;
#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::api::{
    HumanInputActionDto, HumanInputDeliveryDto, HumanInputFieldView, HumanInputTypeView,
    InputContinuationView, OutputDefinitionView, PendingHumanInputView, PermissionDecisionDto,
    PermissionDecisionKindDto, PermissionGrantScopeDto, ENTRY_LIMIT_DECISION_FIELD,
    ENTRY_LIMIT_RESET_COUNTER,
};
use crate::display::{without_unsafe_chars, without_unsafe_chars_multiline};

pub use action::{COMMENT_FIELD, EXTRA_VARIABLES_FIELD, MESSAGE_ENTRY};
pub use permission::PATTERN_FIELD;

/// The field a message to the agent is typed into.
pub const MESSAGE_FIELD: &str = "Wiadomość dla agenta";

/// The field a return to the agent is explained in.
pub const COMMENT_TO_AGENT: &str = "Komentarz dla agenta";

/// How a line of the context is drawn: by role, never by colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelTone {
    /// Ordinary text.
    Plain,
    /// Background: the agent's answer, an age.
    Dim,
    /// The conductor speaking.
    Conductor,
    /// Something wrong.
    Problem,
}

/// One line (or paragraph) of what a form is about. The text comes from the
/// agent, the conductor or the runner: the client makes it safe when it draws
/// it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelContextLine {
    pub tone: PanelTone,
    pub text: String,
}

impl PanelContextLine {
    fn new(tone: PanelTone, text: impl Into<String>) -> Self {
        Self {
            tone,
            text: text.into(),
        }
    }

    /// Ordinary text.
    pub fn plain(text: impl Into<String>) -> Self {
        Self::new(PanelTone::Plain, text)
    }

    /// Background text.
    pub fn dim(text: impl Into<String>) -> Self {
        Self::new(PanelTone::Dim, text)
    }

    /// What the conductor said.
    pub fn conductor(text: impl Into<String>) -> Self {
        Self::new(PanelTone::Conductor, text)
    }

    /// Something wrong.
    pub fn problem(text: impl Into<String>) -> Self {
        Self::new(PanelTone::Problem, text)
    }
}

/// What a field accepts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelFieldKind {
    /// Free text; `multiline` lets a new line be typed.
    Text { multiline: bool },
    /// One of the options; it starts on the first.
    Select { options: Vec<String> },
}

/// One field of an entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelField {
    /// The name shown above the value and returned with it.
    pub name: String,
    pub kind: PanelFieldKind,
    /// Whether an answer cannot be sent without it.
    #[serde(default)]
    pub required: bool,
    /// What the field is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The text the field starts with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// The value the conductor proposed; the field starts with its text and
    /// a value the operator did not touch is delivered as this value, so a
    /// number stays a number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed: Option<Value>,
}

impl PanelField {
    fn new(name: impl Into<String>, kind: PanelFieldKind) -> Self {
        Self {
            name: name.into(),
            kind,
            required: false,
            description: None,
            value: None,
            proposed: None,
        }
    }

    /// A single-line text field.
    pub fn text(name: impl Into<String>) -> Self {
        Self::new(name, PanelFieldKind::Text { multiline: false })
    }

    /// A text field that takes new lines.
    pub fn multiline(name: impl Into<String>) -> Self {
        Self::new(name, PanelFieldKind::Text { multiline: true })
    }

    /// A field that picks one of `options`.
    pub fn select(name: impl Into<String>, options: Vec<String>) -> Self {
        Self::new(name, PanelFieldKind::Select { options })
    }

    /// The same field, required.
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// The same field with a description.
    pub fn described(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// The same field starting with `value`.
    pub fn with_value(mut self, value: impl Into<String>) -> Self {
        self.value = Some(value.into());
        self
    }

    /// The same field holding a value the conductor proposed.
    pub fn proposing(mut self, value: Value) -> Self {
        self.value = Some(value_as_text(&value));
        self.proposed = Some(value);
        self
    }
}

/// What choosing an entry does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelPlan {
    /// Completes a gate with this label (`None`: the engine infers it from
    /// the fields). `label_field` is the select field that stands for the
    /// label, when one does.
    Complete {
        label: Option<String>,
        label_field: Option<String>,
    },
    /// Answers the agent's own question; the step resumes.
    Answer,
    /// Settles a problem with this output.
    Output { label: String },
    /// Writes to the agent; the step runs again.
    Message,
    /// Accepts the agent's result.
    Approve,
    /// Returns the result to the agent with a comment.
    Return,
    /// Decides on a permission.
    Permission { decision: PermissionDecisionDto },
    /// Settles the entry limit with this decision (`ENTRY_LIMIT_*`).
    EntryLimit { decision: String },
}

/// One entry of a form: a choice with the fields it needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelEntry {
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<PanelField>,
    /// The entry resumes the agent; the workflow state it returns to, empty
    /// when the definition does not know it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumes: Option<String>,
    /// The attempt the entry resumes, for a client that knows the steps and
    /// can name the state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumes_attempt: Option<String>,
    /// The entry the conductor's analysis recommends.
    #[serde(default)]
    pub recommended: bool,
    pub plan: PanelPlan,
    /// What the notification says after the entry was sent, without the
    /// task's name.
    pub summary: String,
}

impl PanelEntry {
    pub(crate) fn new(label: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            description: description.into(),
            fields: Vec::new(),
            resumes: None,
            resumes_attempt: None,
            recommended: false,
            plan: PanelPlan::Answer,
            summary: String::new(),
        }
    }

    pub(crate) fn with_field(mut self, field: PanelField) -> Self {
        self.fields.push(field);
        self
    }

    pub(crate) fn resuming(mut self, state: impl Into<String>) -> Self {
        self.resumes = Some(state.into());
        self
    }

    pub(crate) fn recommended(mut self) -> Self {
        self.recommended = true;
        self
    }
}

/// A single ordinary answer that can sit in a composer of its own below the
/// list of entries, instead of in the entry's fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelComposer {
    /// The entry whose only field the composer holds.
    pub entry: usize,
    pub label: String,
    pub help: String,
}

/// Everything a form of the panel needs to be shown and answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelDefinition {
    /// What kind of wait it answers.
    pub continuation: InputContinuationView,
    /// The key of the request, which an answer names.
    pub episode: String,
    pub context: Vec<PanelContextLine>,
    pub entries: Vec<PanelEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composer: Option<PanelComposer>,
}

/// The definition of the form that answers `pending`. `resumed_state` names
/// the workflow state an agent attempt belongs to, for an output that resumes
/// one.
pub fn define(
    pending: &PendingHumanInputView,
    resumed_state: &dyn Fn(&str) -> Option<String>,
) -> PanelDefinition {
    let mut builder = Builder::default();
    builder
        .context
        .push(PanelContextLine::plain(pending.instruction()));
    if let Some(reason) = &pending.reason {
        builder.push_reason(reason);
    }
    for required in &pending.required_values {
        builder.push_named_value(&required.name, &required.value);
    }
    if let Some(tail) = &pending.answer_tail {
        builder
            .context
            .push(PanelContextLine::dim(format!("agent: …{tail}")));
    }
    match pending.continuation {
        InputContinuationView::CompleteStep => builder.build_gate(pending, resumed_state),
        InputContinuationView::ResumeAgent => builder.build_question(pending),
        InputContinuationView::ResolveOutput => builder.build_problem(pending),
        InputContinuationView::EntryLimit => builder.build_entry_limit(pending),
        InputContinuationView::ApproveOutput => builder.build_approval(pending),
        InputContinuationView::ResolvePermission => builder.build_permission_info(pending),
        InputContinuationView::Unrecognized => {}
    }
    builder.finish(pending)
}

/// The delivery that choosing entry `index` of `definition` with `values`
/// (field name, text) sends.
pub fn deliver(
    definition: &PanelDefinition,
    index: usize,
    values: &[(String, String)],
) -> Result<HumanInputDeliveryDto, String> {
    let entry = definition
        .entries
        .get(index)
        .ok_or_else(|| "Ta akcja nie istnieje.".to_string())?;
    let delivery = match &entry.plan {
        PanelPlan::Complete { label, label_field } => {
            complete(label.as_deref(), label_field.as_deref(), values)
        }
        PanelPlan::Answer => answer(values)?,
        PanelPlan::Output { label } => output(entry, label, values)?,
        PanelPlan::Message => HumanInputDeliveryDto {
            agent_message: Some(
                value_of(values, MESSAGE_FIELD)
                    .unwrap_or_default()
                    .to_string(),
            ),
            ..HumanInputDeliveryDto::default()
        },
        PanelPlan::Approve => HumanInputDeliveryDto {
            action: Some(HumanInputActionDto::Approve),
            ..HumanInputDeliveryDto::default()
        },
        PanelPlan::Return => HumanInputDeliveryDto {
            comment: Some(
                value_of(values, COMMENT_TO_AGENT)
                    .unwrap_or_default()
                    .to_string(),
            ),
            action: Some(HumanInputActionDto::Resume),
            ..HumanInputDeliveryDto::default()
        },
        PanelPlan::Permission { decision } => HumanInputDeliveryDto {
            permission_decision: Some(PermissionDecisionDto {
                pattern: permission::pattern(decision, values)?,
                message: value_of(values, MESSAGE_FIELD)
                    .map(str::trim)
                    .filter(|message| !message.is_empty())
                    .map(str::to_string),
                ..decision.clone()
            }),
            ..HumanInputDeliveryDto::default()
        },
        PanelPlan::EntryLimit { decision } => HumanInputDeliveryDto {
            values: Map::from_iter([(
                ENTRY_LIMIT_DECISION_FIELD.to_string(),
                Value::String(decision.clone()),
            )]),
            ..HumanInputDeliveryDto::default()
        },
    };
    Ok(HumanInputDeliveryDto {
        episode: Some(definition.episode.clone()),
        ..delivery
    })
}

/// The text typed into the field `name`, if the delivery has the field.
fn value_of<'a>(values: &'a [(String, String)], name: &str) -> Option<&'a str> {
    values
        .iter()
        .find(|(field, _)| field == name)
        .map(|(_, value)| value.as_str())
}

/// A proposed value as editable text: a string as itself, anything else as
/// JSON.
pub fn value_as_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn complete(
    label: Option<&str>,
    label_field: Option<&str>,
    submitted: &[(String, String)],
) -> HumanInputDeliveryDto {
    let mut values = Map::new();
    for (name, value) in submitted {
        values.insert(name.clone(), Value::String(value.clone()));
    }
    if let (Some(field), Some(label)) = (label_field, label) {
        values.insert(field.to_string(), Value::String(label.to_string()));
    }
    HumanInputDeliveryDto {
        values,
        chosen_label: label.map(str::to_string),
        ..HumanInputDeliveryDto::default()
    }
}

fn answer(values: &[(String, String)]) -> Result<HumanInputDeliveryDto, String> {
    let filled = |name: &str| value_of(values, name).is_some();
    if !filled("answer") && !filled("option") {
        return Err("Wpisz odpowiedź albo wybierz opcję.".to_string());
    }
    Ok(HumanInputDeliveryDto {
        values: values
            .iter()
            .map(|(name, value)| (name.clone(), Value::String(value.clone())))
            .collect(),
        ..HumanInputDeliveryDto::default()
    })
}

fn output(
    entry: &PanelEntry,
    label: &str,
    values: &[(String, String)],
) -> Result<HumanInputDeliveryDto, String> {
    let mut maps = [Map::new(), Map::new()];
    for (name, text) in values {
        for (slot, prefix) in ["artifacts.", "variables."].into_iter().enumerate() {
            if let Some(key) = name.strip_prefix(prefix) {
                let value = delivered_value(entry, name, text);
                insert_once(&mut maps[slot], &prefix[..prefix.len() - 1], key, value)?;
            }
        }
    }
    let [artifacts, mut variables] = maps;
    for line in value_of(values, EXTRA_VARIABLES_FIELD)
        .unwrap_or_default()
        .lines()
    {
        if line.trim().is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!(
                "Linia '{line}' w variables nie ma postaci klucz=wartość."
            ));
        };
        if key.trim().is_empty() {
            return Err(format!("Linia '{line}' w variables nie ma klucza."));
        }
        insert_once(
            &mut variables,
            "variables",
            key.trim(),
            Value::String(value.trim().to_string()),
        )?;
    }
    Ok(HumanInputDeliveryDto {
        chosen_label: Some(label.to_string()),
        artifacts: artifacts
            .into_iter()
            .map(|(name, path)| (name, value_as_text(&path)))
            .collect(),
        variables,
        comment: value_of(values, COMMENT_FIELD)
            .map(str::trim)
            .map(str::to_string),
        ..HumanInputDeliveryDto::default()
    })
}

/// The proposal itself while its text is untouched, the typed text otherwise.
fn delivered_value(entry: &PanelEntry, name: &str, text: &str) -> Value {
    match entry
        .fields
        .iter()
        .find(|field| field.name == name)
        .and_then(|field| field.proposed.as_ref())
    {
        Some(original) if value_as_text(original) == text => original.clone(),
        _ => Value::String(text.to_string()),
    }
}

/// Puts `value` under `key` in `map`, refusing a second one.
fn insert_once(
    map: &mut Map<String, Value>,
    name: &str,
    key: &str,
    value: Value,
) -> Result<(), String> {
    if map.insert(key.to_string(), value).is_some() {
        return Err(format!("Klucz '{name}.{key}' jest podany dwa razy."));
    }
    Ok(())
}

/// What a definition is made of while it is built.
#[derive(Default)]
struct Builder {
    context: Vec<PanelContextLine>,
    entries: Vec<PanelEntry>,
    /// The label of the result held for approval.
    approval_label: Option<String>,
}

impl Builder {
    fn push(&mut self, mut entry: PanelEntry, plan: PanelPlan) {
        entry.plan = plan;
        self.entries.push(entry);
    }

    /// Why the agent made its choice, `uzasadnienie: …`.
    fn push_reason(&mut self, reason: &str) {
        self.context.push(PanelContextLine::plain(format!(
            "uzasadnienie: {}",
            without_unsafe_chars(reason)
        )));
    }

    /// A value the agent gave, `name: value`.
    fn push_named_value(&mut self, name: &str, value: &str) {
        self.context.push(PanelContextLine::plain(format!(
            "{}: {}",
            without_unsafe_chars(name),
            without_unsafe_chars_multiline(value)
        )));
    }

    fn finish(mut self, pending: &PendingHumanInputView) -> PanelDefinition {
        for entry in &mut self.entries {
            entry.summary = summary(entry, self.approval_label.as_deref());
        }
        let composer = composer(pending.continuation, &self.entries);
        PanelDefinition {
            continuation: pending.continuation,
            episode: pending.episode_key().to_string(),
            context: self.context,
            entries: self.entries,
            composer,
        }
    }
}

/// The entry whose single field is an ordinary answer, which a client may
/// draw as a composer of its own. Multi-field answers stay in their entry.
fn composer(continuation: InputContinuationView, entries: &[PanelEntry]) -> Option<PanelComposer> {
    let (index, entry) = entries.iter().enumerate().find(|(_, entry)| {
        matches!(
            (continuation, &entry.plan),
            (InputContinuationView::ResumeAgent, PanelPlan::Answer)
                | (InputContinuationView::ResolveOutput, PanelPlan::Message)
        )
    })?;
    let (label, help) = match (&entry.plan, entry.fields.as_slice()) {
        (PanelPlan::Answer, [field])
            if field.name == "answer"
                && field.kind == (PanelFieldKind::Text { multiline: true }) =>
        {
            ("Odpowiedź agentowi", "Odpowiada agentowi.")
        }
        (PanelPlan::Message, [field]) if field.name == MESSAGE_FIELD => {
            ("Odpowiedź do rozmowy", "Wysyła wiadomość do agenta.")
        }
        _ => return None,
    };
    Some(PanelComposer {
        entry: index,
        label: label.to_string(),
        help: help.to_string(),
    })
}

/// What the notification says after `entry` was sent.
fn summary(entry: &PanelEntry, approval_label: Option<&str>) -> String {
    let resumes = entry.resumes.is_some();
    match &entry.plan {
        PanelPlan::Complete {
            label: Some(label), ..
        } if resumes => format!("odpowiedziano {label} ↩ wraca do agenta"),
        PanelPlan::Complete {
            label: Some(label), ..
        } => format!("odpowiedziano {label}"),
        PanelPlan::Complete { label: None, .. } => "odpowiedziano".to_string(),
        PanelPlan::Answer => "odpowiedziano agentowi".to_string(),
        PanelPlan::Output { label } => format!("rozwiązano problem: {label}"),
        PanelPlan::Message => "wysłano wiadomość do agenta".to_string(),
        PanelPlan::Approve => match approval_label {
            Some(label) => format!("zaakceptowano {label}"),
            None => "zaakceptowano".to_string(),
        },
        PanelPlan::Return => "zwrócono agentowi".to_string(),
        PanelPlan::Permission { decision } => match decision.decision {
            PermissionDecisionKindDto::Deny => "odmówiono dostępu agentowi".to_string(),
            PermissionDecisionKindDto::Allow => match decision.scope {
                Some(PermissionGrantScopeDto::ExactOperation) => {
                    "zezwolono na tę operację".to_string()
                }
                Some(PermissionGrantScopeDto::CommandGroup) => {
                    "zezwolono na grupę poleceń w tym tasku".to_string()
                }
                Some(PermissionGrantScopeDto::Pattern) => {
                    "zezwolono na polecenia według wzorca w tym tasku".to_string()
                }
                None => "zezwolono agentowi".to_string(),
            },
        },
        PanelPlan::EntryLimit { decision } if decision == ENTRY_LIMIT_RESET_COUNTER => {
            "wyzerowano licznik i kontynuowano".to_string()
        }
        PanelPlan::EntryLimit { .. } => "kontynuowano po limicie".to_string(),
    }
}

/// `entry` with a field for each of `fields`. A field is required when the
/// workflow says so — except a resume comment, which only the output that
/// resumes an agent needs (3f3dab2), and that output always needs, even when
/// the field itself is optional (KTD12).
fn declared_fields(
    mut entry: PanelEntry,
    fields: &[&HumanInputFieldView],
    outputs: &[OutputDefinitionView],
    label: Option<&str>,
) -> PanelEntry {
    for field in fields {
        // Whether this output, or another one, takes the field as the comment
        // of its resume.
        let (mut claimed_by_own, mut claimed_by_other) = (false, false);
        for output in outputs
            .iter()
            .filter(|output| output.resume_comment_field.as_deref() == Some(field.name.as_str()))
        {
            if label == Some(output.label.as_str()) {
                claimed_by_own = true;
            } else if label.is_some() {
                claimed_by_other = true;
            }
        }
        let required = (field.required && !claimed_by_other) || claimed_by_own;
        let mut built = match field.field_type {
            HumanInputTypeView::Text => PanelField::multiline(field.name.clone()),
            HumanInputTypeView::Select => {
                let mut options = field.options.clone();
                if !required {
                    options.insert(0, String::new());
                }
                PanelField::select(field.name.clone(), options)
            }
        };
        if required {
            built = built.required();
        }
        if let Some(description) = &field.description {
            built = built.described(description.clone());
        }
        entry = entry.with_field(built);
    }
    entry
}
