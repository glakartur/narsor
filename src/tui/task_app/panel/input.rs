//! What the answer panel offers for a pending human input — one panel for
//! every kind: a workflow gate, an agent's own question, an approval, a
//! problem, a permission and an entry limit.
//!
//! This is the part the forms share: the kind of wait, the lines of its
//! context, and [`InputPanel`], which turns a [`PendingHumanInputView`] into
//! the entries of a [`crate::tui::chooser::Chooser`] and the chosen entry back
//! into the delivery the server receives. What each kind builds, and how its
//! entries are delivered, lives in the directory of that kind of form:
//! [`super::action`], [`super::answer`], [`super::entry_limit`] and
//! [`super::permission`].
//!
//! The panel checks what the engine checks — required fields, `select`
//! values, a label where nothing can be inferred, the keys a problem's label
//! needs — so the operator hears about a mistake next to the field that caused
//! it. The server validates again, and a refusal it sends back keeps the panel
//! open with the typed content.

use serde_json::{Map, Value};

use crate::conductor::Annotation;
use crate::domain::display::{without_unsafe_chars, without_unsafe_chars_multiline};
use crate::tui::chooser::{ChooserField, ChooserItem, FieldKind, Submission};
use narsor_protocol::api::{
    HumanInputActionDto, HumanInputDeliveryDto, HumanInputFieldView, HumanInputTypeView,
    InputContinuationView, OutputDefinitionView, PendingHumanInputView, PermissionDecisionDto,
    PermissionDecisionKindDto, PermissionGrantScopeDto, ENTRY_LIMIT_DECISION_FIELD,
    ENTRY_LIMIT_RESET_COUNTER,
};

/// What kind of wait the form answers, as the operator reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    /// A `human` state of the workflow: the answer completes the step.
    Gate,
    /// An agent asked in the middle of its step; the answer resumes it.
    AgentQuestion,
    /// An agent step ended without a usable output label (KTD3): the answer
    /// is the output it did not give.
    Problem,
    Approval,
    Permission,
    EntryLimit,
    Unsupported,
}

impl InputKind {
    /// The kind a pending request's continuation names.
    pub fn of(continuation: InputContinuationView) -> Self {
        match continuation {
            InputContinuationView::CompleteStep => Self::Gate,
            InputContinuationView::ResumeAgent => Self::AgentQuestion,
            InputContinuationView::ResolveOutput => Self::Problem,
            InputContinuationView::ApproveOutput => Self::Approval,
            InputContinuationView::ResolvePermission => Self::Permission,
            InputContinuationView::EntryLimit => Self::EntryLimit,
            InputContinuationView::Unrecognized => Self::Unsupported,
        }
    }

    /// The modal's title.
    pub fn title(self) -> &'static str {
        match self {
            Self::Gate => " Human input ",
            Self::AgentQuestion => " Reply to the agent ",
            Self::Problem => " Resolve the problem ",
            Self::Approval => " Approve agent output ",
            Self::Permission => " Permission decision ",
            Self::EntryLimit => " Entry limit ",
            Self::Unsupported => " Unsupported wait ",
        }
    }
}

// ---------------------------------------------------------------------------
// The answer panel
// ---------------------------------------------------------------------------

/// How a line of an entry's context is drawn: by role, never by colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Ordinary text.
    Plain,
    /// Background: the agent's answer, an age.
    Dim,
    /// The conductor speaking: `◆`.
    Conductor,
    /// Something wrong.
    Problem,
}

/// One line (or paragraph) of what an entry of the panel is about. The text
/// comes from the agent, the conductor or the runner and is made safe when it
/// is drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextLine {
    /// How it is drawn.
    pub tone: Tone,
    /// What it says.
    pub text: String,
}

impl ContextLine {
    /// Ordinary text.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            tone: Tone::Plain,
            text: text.into(),
        }
    }

    /// Background text.
    pub fn dim(text: impl Into<String>) -> Self {
        Self {
            tone: Tone::Dim,
            text: text.into(),
        }
    }

    /// What the conductor said.
    pub fn conductor(text: impl Into<String>) -> Self {
        Self {
            tone: Tone::Conductor,
            text: text.into(),
        }
    }

    /// Something wrong.
    pub fn problem(text: impl Into<String>) -> Self {
        Self {
            tone: Tone::Problem,
            text: text.into(),
        }
    }
}

/// What choosing an entry of the panel does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tui::task_app::panel) enum Plan {
    /// Completes a gate with this label (`None`: the engine infers it from
    /// the fields).
    Complete(Option<String>),
    /// Answers the agent's own question; the step resumes.
    Answer,
    /// Settles a problem with this output.
    Output(String),
    /// Writes to the agent; the step runs again.
    Message,
    /// Accepts the agent's result.
    Approve,
    /// Returns the result to the agent with a comment.
    Return,
    Permission(PermissionDecisionDto),
    /// Settles the entry limit with this decision (`ENTRY_LIMIT_*`).
    EntryLimit(&'static str),
}

/// A pending human input as the panel offers it: what it is about, the
/// entries to choose from and how the chosen one is delivered.
#[derive(Debug, Clone)]
pub struct InputPanel {
    pub(in crate::tui::task_app::panel) kind: InputKind,
    pub(in crate::tui::task_app::panel) episode: String,
    pub(in crate::tui::task_app::panel) context: Vec<ContextLine>,
    pub(in crate::tui::task_app::panel) items: Vec<ChooserItem>,
    pub(in crate::tui::task_app::panel) plans: Vec<Plan>,
    pub(in crate::tui::task_app::panel) selected: usize,
    /// The select field that stands for the label, when one does (KTD17).
    pub(in crate::tui::task_app::panel) label_field: Option<String>,
    /// The values the conductor proposed, by entry and field name, so a number
    /// the operator did not touch is sent as a number.
    pub(in crate::tui::task_app::panel) originals: Vec<(usize, String, Value)>,
    /// The label of the result held for approval.
    pub(in crate::tui::task_app::panel) approval_label: Option<String>,
}

impl InputPanel {
    /// The panel for `pending`. `resumed_state` says which workflow state an
    /// agent attempt belongs to, for the `↩ wraca do agenta <stan>` of an
    /// output that resumes one.
    pub fn new(
        pending: &PendingHumanInputView,
        resumed_state: &dyn Fn(&str) -> Option<String>,
    ) -> Self {
        let kind = InputKind::of(pending.continuation);
        let mut panel = Self {
            kind,
            episode: pending.episode_key().to_string(),
            context: Vec::new(),
            items: Vec::new(),
            plans: Vec::new(),
            selected: 0,
            label_field: None,
            originals: Vec::new(),
            approval_label: None,
        };
        panel
            .context
            .push(ContextLine::plain(pending.instruction()));
        if let Some(reason) = &pending.reason {
            panel.push_reason(reason);
        }
        for required in &pending.required_values {
            panel.push_named_value(&required.name, &required.value);
        }
        if let Some(tail) = &pending.answer_tail {
            panel
                .context
                .push(ContextLine::dim(format!("agent: …{tail}")));
        }
        match kind {
            InputKind::Gate => panel.build_gate(pending, resumed_state),
            InputKind::AgentQuestion => panel.build_question(pending),
            InputKind::Problem => panel.build_problem(pending),
            InputKind::EntryLimit => panel.build_entry_limit(pending),
            InputKind::Approval => panel.build_approval(pending),
            InputKind::Permission => panel.build_permission_info(pending),
            InputKind::Unsupported => {}
        }
        panel
    }

    /// What kind of wait it is.
    pub fn kind(&self) -> InputKind {
        self.kind
    }

    /// The key of the request, which an answer names.
    pub fn episode(&self) -> &str {
        &self.episode
    }

    /// What the request is about.
    pub fn context(&self) -> &[ContextLine] {
        &self.context
    }

    /// The entries to choose from.
    pub fn items(&self) -> &[ChooserItem] {
        &self.items
    }

    /// A single ordinary answer can sit in its own composer below the action
    /// list. Multi-field answers remain attached to their action.
    pub(crate) fn composer_spec(&self) -> Option<(usize, &'static str)> {
        let index = match self.kind {
            InputKind::AgentQuestion => self
                .plans
                .iter()
                .position(|plan| matches!(plan, Plan::Answer))?,
            InputKind::Problem => self
                .plans
                .iter()
                .position(|plan| matches!(plan, Plan::Message))?,
            _ => return None,
        };
        let item = self.items.get(index)?;
        let label = match self.plans.get(index)? {
            Plan::Answer
                if item.fields.len() == 1
                    && item.fields[0].name == "answer"
                    && matches!(&item.fields[0].kind, FieldKind::Text { multiline: true }) =>
            {
                "Odpowiedź agentowi"
            }
            Plan::Message if item.fields.len() == 1 && item.fields[0].name == MESSAGE_FIELD => {
                "Odpowiedź do rozmowy"
            }
            _ => return None,
        };
        Some((index, label))
    }

    pub(crate) fn composer_submission(&self, value: String) -> Option<Submission> {
        let (index, _) = self.composer_spec()?;
        let field = self.items.get(index)?.fields.first()?.name.clone();
        Some(Submission {
            index,
            label: self.items.get(index)?.label.clone(),
            values: vec![(field, value)],
        })
    }

    /// The entry the panel opens on: the conductor's recommendation, or the
    /// safest one.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Takes the conductor's summary of the gate: it joins the context, and
    /// the entry it recommends is marked `◆` and opened on. One about another
    /// request is ignored.
    pub fn annotate(&mut self, annotation: &Annotation) {
        if annotation.key != self.episode {
            return;
        }
        let recommended = annotation
            .recommendation
            .as_deref()
            .and_then(|answer| self.items.iter().position(|item| item.label == answer));
        let mut line = format!("◆ dyrygent: {}", annotation.summary);
        if let Some(answer) = annotation.recommendation.as_deref() {
            line.push_str(&format!(" — raczej {answer}"));
        }
        self.context.push(ContextLine::conductor(line));
        if let Some(index) = recommended {
            self.items[index].recommended = true;
            self.selected = index;
        }
    }

    /// The delivery that choosing `submission` sends.
    pub fn delivery(&self, submission: &Submission) -> Result<HumanInputDeliveryDto, String> {
        let plan = self
            .plans
            .get(submission.index)
            .ok_or_else(|| "Ta akcja nie istnieje.".to_string())?;
        let delivery = match plan {
            Plan::Complete(label) => self.complete(label.as_deref(), submission),
            Plan::Answer => self.answer(submission)?,
            Plan::Output(label) => self.output(label, submission)?,
            Plan::Message => HumanInputDeliveryDto {
                agent_message: Some(
                    Self::value_of(submission, MESSAGE_FIELD)
                        .unwrap_or_default()
                        .to_string(),
                ),
                ..HumanInputDeliveryDto::default()
            },
            Plan::Approve => HumanInputDeliveryDto {
                action: Some(HumanInputActionDto::Approve),
                ..HumanInputDeliveryDto::default()
            },
            Plan::Return => HumanInputDeliveryDto {
                comment: Some(
                    Self::value_of(submission, COMMENT_TO_AGENT)
                        .unwrap_or_default()
                        .to_string(),
                ),
                action: Some(HumanInputActionDto::Resume),
                ..HumanInputDeliveryDto::default()
            },
            Plan::Permission(permission_decision) => HumanInputDeliveryDto {
                permission_decision: Some(PermissionDecisionDto {
                    pattern: Self::permission_pattern(permission_decision, submission)?,
                    message: Self::value_of(submission, MESSAGE_FIELD)
                        .map(str::trim)
                        .filter(|message| !message.is_empty())
                        .map(str::to_string),
                    ..permission_decision.clone()
                }),
                ..HumanInputDeliveryDto::default()
            },
            Plan::EntryLimit(decision) => HumanInputDeliveryDto {
                values: Map::from_iter([(
                    ENTRY_LIMIT_DECISION_FIELD.to_string(),
                    Value::String((*decision).to_string()),
                )]),
                ..HumanInputDeliveryDto::default()
            },
        };
        Ok(HumanInputDeliveryDto {
            episode: Some(self.episode.clone()),
            ..delivery
        })
    }

    /// What the notification says after `submission` was sent, without the
    /// task's name.
    pub fn summary(&self, submission: &Submission) -> String {
        let resumes = self
            .items
            .get(submission.index)
            .is_some_and(|item| item.resumes_agent.is_some());
        match self.plans.get(submission.index) {
            Some(Plan::Complete(Some(label))) if resumes => {
                format!("odpowiedziano {label} ↩ wraca do agenta")
            }
            Some(Plan::Complete(Some(label))) => format!("odpowiedziano {label}"),
            Some(Plan::Complete(None)) | None => "odpowiedziano".to_string(),
            Some(Plan::Answer) => "odpowiedziano agentowi".to_string(),
            Some(Plan::Output(label)) => format!("rozwiązano problem: {label}"),
            Some(Plan::Message) => "wysłano wiadomość do agenta".to_string(),
            Some(Plan::Approve) => match &self.approval_label {
                Some(label) => format!("zaakceptowano {label}"),
                None => "zaakceptowano".to_string(),
            },
            Some(Plan::Return) => "zwrócono agentowi".to_string(),
            Some(Plan::Permission(decision)) => match decision.decision {
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
            Some(Plan::EntryLimit(decision)) if *decision == ENTRY_LIMIT_RESET_COUNTER => {
                "wyzerowano licznik i kontynuowano".to_string()
            }
            Some(Plan::EntryLimit(_)) => "kontynuowano po limicie".to_string(),
        }
    }

    pub(in crate::tui::task_app::panel) fn value_of<'a>(
        submission: &'a Submission,
        name: &str,
    ) -> Option<&'a str> {
        submission
            .values
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.as_str())
    }

    /// Why the agent made its choice, `uzasadnienie: …`.
    pub(in crate::tui::task_app::panel) fn push_reason(&mut self, reason: &str) {
        self.context.push(ContextLine::plain(format!(
            "uzasadnienie: {}",
            without_unsafe_chars(reason)
        )));
    }

    /// A value the agent gave, `name: value`.
    pub(in crate::tui::task_app::panel) fn push_named_value(&mut self, name: &str, value: &str) {
        self.context.push(ContextLine::plain(format!(
            "{}: {}",
            without_unsafe_chars(name),
            without_unsafe_chars_multiline(value)
        )));
    }

    pub(in crate::tui::task_app::panel) fn push(&mut self, item: ChooserItem, plan: Plan) {
        self.items.push(item);
        self.plans.push(plan);
    }
}

/// The field a message to the agent is typed into.
pub const MESSAGE_FIELD: &str = "Wiadomość dla agenta";

/// The field a return to the agent is explained in.
pub const COMMENT_TO_AGENT: &str = "Komentarz dla agenta";

/// `item` with a field for each of `fields`. A field is required when the
/// workflow says so — except a resume comment, which only the output that
/// resumes an agent needs (3f3dab2), and that output always needs, even when
/// the field itself is optional (KTD12).
pub(in crate::tui::task_app::panel) fn declared_fields(
    mut item: ChooserItem,
    fields: &[&HumanInputFieldView],
    outputs: &[OutputDefinitionView],
    label: Option<&str>,
) -> ChooserItem {
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
            HumanInputTypeView::Text => ChooserField::multiline(field.name.clone()),
            HumanInputTypeView::Select => {
                let mut options = field.options.clone();
                if !required {
                    options.insert(0, String::new());
                }
                ChooserField::select(field.name.clone(), options)
            }
        };
        if required {
            built = built.required();
        }
        if let Some(description) = &field.description {
            built = built.described(description.clone());
        }
        item = item.with_field(built);
    }
    item
}
