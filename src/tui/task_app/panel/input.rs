//! What the answer panel offers for a pending human input — one panel for
//! every kind: a workflow gate, an agent's own question, an approval, a
//! problem, a permission and an entry limit.
//!
//! The server decides what each form asks: the entries, their fields, how a
//! chosen entry becomes a delivery and what the notification says after it
//! (`narsor_protocol::panel`). This part only shows it: the kind of wait, the
//! lines of its context, and [`InputPanel`], which turns a
//! [`PanelDefinition`] into the entries of a [`crate::tui::chooser::Chooser`]
//! and sends the chosen entry back through [`deliver`].
//!
//! The delivery checks what the engine checks — required fields, `select`
//! values, a label where nothing can be inferred, the keys a problem's label
//! needs — so the operator hears about a mistake next to the field that caused
//! it. The server validates again, and a refusal it sends back keeps the panel
//! open with the typed content.

use crate::conductor::Annotation;
use crate::tui::chooser::{ChooserField, ChooserItem, Submission};
use narsor_protocol::api::{HumanInputDeliveryDto, InputContinuationView};
use narsor_protocol::panel::{
    deliver, PanelComposer, PanelDefinition, PanelEntry, PanelField, PanelFieldKind,
};

/// How a line of a context is drawn, and one line of it.
pub use narsor_protocol::panel::{PanelContextLine as ContextLine, PanelTone as Tone};
pub use narsor_protocol::panel::{
    COMMENT_FIELD, COMMENT_TO_AGENT, EXTRA_VARIABLES_FIELD, MESSAGE_ENTRY, MESSAGE_FIELD,
    PATTERN_FIELD,
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

/// A pending human input as the panel shows it: what it is about, the
/// entries to choose from and the server's definition that says how the chosen
/// one is delivered. Nothing is decided here: the entries, fields, plans and
/// notifications are [`PanelDefinition`], built by the server.
#[derive(Debug, Clone)]
pub struct InputPanel {
    pub(in crate::tui::task_app::panel) kind: InputKind,
    definition: PanelDefinition,
    pub(in crate::tui::task_app::panel) context: Vec<ContextLine>,
    pub(in crate::tui::task_app::panel) items: Vec<ChooserItem>,
}

impl InputPanel {
    /// The panel for the `definition` the server sent. The server knows an
    /// output that resumes the agent only by its attempt; `resumed_state`
    /// names the workflow state of it from the steps read so far.
    pub fn from_definition(
        definition: &PanelDefinition,
        resumed_state: &dyn Fn(&str) -> Option<String>,
    ) -> Self {
        let items = definition
            .entries
            .iter()
            .map(|entry| item_of(entry, resumed_state))
            .collect();
        Self {
            kind: InputKind::of(definition.continuation),
            context: definition.context.clone(),
            items,
            definition: definition.clone(),
        }
    }

    /// What kind of wait it is.
    pub fn kind(&self) -> InputKind {
        self.kind
    }

    /// The key of the request, which an answer names.
    pub fn episode(&self) -> &str {
        &self.definition.episode
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
    /// list, when the server's definition says one can. Multi-field answers
    /// remain attached to their action.
    pub(crate) fn composer_spec(&self) -> Option<&PanelComposer> {
        self.definition.composer.as_ref()
    }

    pub(crate) fn composer_submission(&self, value: String) -> Option<Submission> {
        let index = self.composer_spec()?.entry;
        let field = self.items.get(index)?.fields.first()?.name.clone();
        Some(Submission {
            index,
            label: self.items.get(index)?.label.clone(),
            values: vec![(field, value)],
        })
    }

    /// Takes the conductor's summary of the gate: it joins the context, and
    /// the entry it recommends is marked `◆`; the panel still opens on the
    /// first entry. One about another request is ignored.
    pub fn annotate(&mut self, annotation: &Annotation) {
        if annotation.key != self.definition.episode {
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
        }
    }

    /// The delivery that choosing `submission` sends.
    pub fn delivery(&self, submission: &Submission) -> Result<HumanInputDeliveryDto, String> {
        deliver(&self.definition, submission.index, &submission.values)
    }

    /// What the notification says after `submission` was sent, without the
    /// task's name.
    pub fn summary(&self, submission: &Submission) -> String {
        self.definition.entries.get(submission.index).map_or_else(
            || "odpowiedziano".to_string(),
            |entry| entry.summary.clone(),
        )
    }
}

/// The entry of the chooser for `entry` of the definition.
fn item_of(entry: &PanelEntry, resumed_state: &dyn Fn(&str) -> Option<String>) -> ChooserItem {
    let mut item = ChooserItem::new(entry.label.clone(), entry.description.clone());
    if let Some(state) = &entry.resumes {
        let named = entry.resumes_attempt.as_deref().and_then(resumed_state);
        item = item.resuming(named.unwrap_or_else(|| state.clone()));
    }
    if entry.recommended {
        item = item.recommended();
    }
    for field in &entry.fields {
        item = item.with_field(field_of(field));
    }
    item
}

/// The field of the chooser for `field` of the definition.
fn field_of(field: &PanelField) -> ChooserField {
    let mut built = match &field.kind {
        PanelFieldKind::Text { multiline: true } => ChooserField::multiline(field.name.clone()),
        PanelFieldKind::Text { multiline: false } => ChooserField::text(field.name.clone()),
        PanelFieldKind::Select { options } => {
            ChooserField::select(field.name.clone(), options.clone())
        }
    };
    if field.required {
        built = built.required();
    }
    if let Some(description) = &field.description {
        built = built.described(description.clone());
    }
    match (&field.proposed, &field.value) {
        (Some(_), Some(text)) => built.proposed(text.clone()),
        (None, Some(text)) => built.with_value(text.clone()),
        _ => built,
    }
}
