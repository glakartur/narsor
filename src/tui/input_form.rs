//! What the answer panel offers for a pending human input — one panel for every
//! kind (R1): a workflow gate, an agent's own question, an approval and a
//! problem — and the inputs a workflow declares (the new-task form).
//!
//! Like the rest of the TUI's state, this holds no terminal. [`InputPanel`]
//! turns a [`PendingHumanInputView`] into the entries of a
//! [`crate::tui::chooser::Chooser`] and their fields (the label picker, the
//! declared fields, a problem's `artifacts` and `variables`, the comment), and the
//! chosen entry turns back into the [`HumanInputDelivery`] the server
//! receives.
//!
//! # Validation
//!
//! The panel checks what the engine checks — required fields, `select`
//! values, a label where nothing can be inferred, the keys a problem's label
//! needs (KTD15) — so the operator hears about a mistake next to the field
//! that caused it. It is only the quick answer: the server validates again,
//! and a refusal it sends back keeps the panel open with the typed content.
//!
//! Rules worth knowing: a resume comment is required only for the output that
//! resumes an agent; the text of a refused resume is given back in the field;
//! an approval is accepted by default and going back to the agent needs a
//! comment; a problem starts by talking to the agent.
//!
//! # One source of labels (KTD17)
//!
//! A gate whose `select` field offers exactly the state's outputs (like
//! `ambiguity_decision` in the bundled workflow) is drawn as a single choice
//! with the outputs' descriptions. The picked value is both the field value
//! and the chosen label, so the two can never disagree.

use std::collections::HashSet;
use std::iter::Peekable;
use std::str::Chars;

use serde_json::{Map, Value};

use crate::conductor::Annotation;
use crate::domain::display::{
    is_invisible_format, without_unsafe_chars, without_unsafe_chars_multiline,
};
use crate::tui::chooser::{ChooserField, ChooserItem, FieldKind, Submission};
use narsor_protocol::api::{
    bash_grant_rules, bash_pattern_rule, is_approvable_permission_reason, BashGrantScope,
    HumanInputActionDto, HumanInputDeliveryDto, HumanInputFieldView, HumanInputTypeView,
    InputContinuationView, OutputDefinitionView, PendingHumanInputView, PermissionDecisionDto,
    PermissionDecisionKindDto, PermissionDenialView, PermissionGrantScopeDto, ENTRY_LIMIT_CONTINUE,
    ENTRY_LIMIT_DECISION_FIELD, ENTRY_LIMIT_RESET_COUNTER,
};
use narsor_protocol::controller::{WorkflowInputType, WorkflowInputView};
use narsor_protocol::display::NO_REASON;
use narsor_protocol::event::{AnsweredBy, EventData, InputCloseReason};

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

/// How the request an open form answers stopped waiting — someone else's
/// answer, a transition, an interrupt, a recovery (R5, AE13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputClosure {
    /// Who closed it, when the log says.
    pub answered_by: Option<AnsweredBy>,
    /// How it was closed, when the log says.
    pub reason: Option<InputCloseReason>,
    /// The answer that closed it, summarised on one line.
    pub answer: Option<String>,
}

impl InputClosure {
    /// A closure nothing explains: the request is simply no longer pending.
    pub fn unexplained() -> Self {
        Self {
            answered_by: None,
            reason: None,
            answer: None,
        }
    }

    /// The closure a `HumanInputReceived` records, with the episode it
    /// closed; `None` for any other event.
    pub fn from_event(data: &EventData) -> Option<(Option<&str>, Self)> {
        let EventData::HumanInputReceived {
            input,
            answered_by,
            close_reason,
            episode,
            output_label,
            artifacts,
            variables,
            comment,
        } = data
        else {
            return None;
        };
        let mut parts = Vec::new();
        if let Some(label) = output_label {
            parts.push(format!("'{label}'"));
        }
        for values in [Some(input), artifacts.as_ref(), variables.as_ref()]
            .into_iter()
            .flatten()
        {
            if let Some(map) = values.as_object() {
                parts.extend(
                    map.iter()
                        .map(|(key, value)| format!("{key}={}", value_as_text(value))),
                );
            }
        }
        if let Some(comment) = comment.as_deref().filter(|comment| !comment.is_empty()) {
            parts.push(format!("comment: {comment}"));
        }
        let closure = Self {
            answered_by: *answered_by,
            reason: close_reason.or(Some(InputCloseReason::Answered)),
            answer: (!parts.is_empty()).then(|| parts.join(", ")),
        };
        Some((episode.as_deref(), closure))
    }

    /// What the form says about it.
    pub fn message(&self) -> String {
        let by = self.answered_by.map(|by| match by {
            AnsweredBy::Operator => "an operator",
            AnsweredBy::Conductor => "the conductor",
        });
        match self.reason {
            Some(InputCloseReason::Transition) => match by {
                Some(by) => format!("Closed by a transition ({by})."),
                None => "Closed by a transition.".to_string(),
            },
            Some(InputCloseReason::Recover) => {
                "Closed: the step was recovered and starts afresh.".to_string()
            }
            Some(InputCloseReason::Cancel) => "Closed: the execution was cancelled.".to_string(),
            Some(InputCloseReason::Undo) => {
                "Closed: a decision of the conductor's was undone.".to_string()
            }
            Some(InputCloseReason::Answered) => {
                let by = by.unwrap_or("someone else");
                match &self.answer {
                    Some(answer) => format!("Already answered by {by}: {answer}"),
                    None => format!("Already answered by {by}."),
                }
            }
            None => "This input is no longer pending: it was closed elsewhere.".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// What a field takes from a paste or a key, besides the text buffer
// ---------------------------------------------------------------------------
//
// Editing of the text itself is `super::form::buffer::TextBuffer`; here are
// the rules about what a paste may carry and how a `select` steps.

/// What a paste puts into a field of `kind`: nothing for a `select`, and for
/// the others only what could have been typed — no escape sequence, no
/// control character but `\n` and `\t`, no invisible format character the
/// field would not show, every line break one `\n`. A one-line field gets a
/// space for each line break, a number field only its digits and `-`.
pub fn pasted_text(kind: &FieldKind, pasted: &str) -> Option<String> {
    if matches!(kind, FieldKind::Select(_)) {
        return None;
    }
    let mut text = String::with_capacity(pasted.len());
    let mut chars = pasted.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\u{1b}' => skip_pasted_escape(&mut chars),
            '\r' => {
                chars.next_if_eq(&'\n');
                text.push('\n');
            }
            // NEL, the line and the paragraph separator break a line too.
            '\n' | '\u{85}' | '\u{2028}' | '\u{2029}' => text.push('\n'),
            '\t' => text.push(character),
            c if c.is_control() => {}
            // The field does not draw these (see `scroll::sanitize`), so the
            // value would differ from what the operator sees and sends.
            c if is_invisible_format(c) || is_unicode_tag(c) => {}
            c => text.push(c),
        }
    }
    Some(match kind {
        FieldKind::Text { multiline: true } => text,
        FieldKind::Text { multiline: false } => text.replace('\n', " "),
        _ => text
            .chars()
            .filter(|character| character.is_ascii_digit() || *character == '-')
            .collect(),
    })
}

/// Skips the escape sequence a pasted ESC starts, and never more: pasted
/// text is the operator's data, not display output, so a lone or cut-off
/// sequence must not eat the text after it.
///
/// A CSI ends at its first byte that is not a parameter or an intermediate
/// byte, and only a final byte is taken with it. An OSC ends at BEL, at ST
/// (`ESC \`) or before a line break. Any other ESC takes only a following
/// `@`..=`_` byte, the one-character escapes.
fn skip_pasted_escape(chars: &mut Peekable<Chars<'_>>) {
    match chars.peek() {
        Some('[') => {
            chars.next();
            while chars.next_if(|c| (' '..='?').contains(c)).is_some() {}
            chars.next_if(|c| ('@'..='~').contains(c));
        }
        Some(']') => {
            chars.next();
            while let Some(c) = chars.next_if(|c| !matches!(c, '\n' | '\r')) {
                match c {
                    '\u{7}' => break,
                    '\u{1b}' => {
                        chars.next_if_eq(&'\\');
                        break;
                    }
                    _ => {}
                }
            }
        }
        _ => {
            chars.next_if(|c| ('@'..='_').contains(c));
        }
    }
}

/// A character of the Unicode tag block: invisible, and a known way to hide
/// text inside text.
fn is_unicode_tag(character: char) -> bool {
    ('\u{E0000}'..='\u{E007F}').contains(&character)
}

/// The option `delta` steps from `current`, wrapping around. A `current` that
/// is not an option starts at the first (forward) or last (backward) one.
/// `None` when there are no options.
pub fn cycle_option(options: &[String], current: &str, delta: isize) -> Option<String> {
    if options.is_empty() {
        return None;
    }
    let next = match options.iter().position(|option| option == current) {
        Some(index) => step_index(index, delta, options.len()),
        None if delta >= 0 => 0,
        None => options.len() - 1,
    };
    Some(options[next].clone())
}

/// Whether a single `select` field lets the engine infer the output label.
fn infers_label(fields: &[HumanInputFieldView]) -> bool {
    fields
        .iter()
        .filter(|field| field.field_type == HumanInputTypeView::Select)
        .count()
        == 1
}

/// Whether `field` is a `select` whose options are exactly `outputs`, in any
/// order (KTD17).
fn offers_exactly(field: &HumanInputFieldView, outputs: &[OutputDefinitionView]) -> bool {
    if field.field_type != HumanInputTypeView::Select
        || outputs.is_empty()
        || field.options.len() != outputs.len()
    {
        return false;
    }
    let options: HashSet<&str> = field.options.iter().map(String::as_str).collect();
    options.len() == outputs.len()
        && outputs
            .iter()
            .all(|output| options.contains(output.label.as_str()))
}

/// A proposed value as editable text: a string as itself, anything else as
/// JSON.
fn value_as_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Steps `current` by `delta`, wrapping around `length`.
fn step_index(current: usize, delta: isize, length: usize) -> usize {
    ((current as isize + delta).rem_euclid(length as isize)) as usize
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

/// The name of the optional field that takes more `variables` than the label
/// needs, one `key=value` per line.
pub const EXTRA_VARIABLES_FIELD: &str = "variables (klucz=wartość)";

/// A problem's free-text comment, kept in the log with the answer.
pub const COMMENT_FIELD: &str = "komentarz";

/// The label of the entry that talks to the agent instead of settling a
/// problem.
pub const MESSAGE_ENTRY: &str = "Napisz do agenta";

/// What choosing an entry of the panel does.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Plan {
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
    EntryLimitContinue,
    EntryLimitReset,
}

/// A pending human input as the panel offers it: what it is about, the
/// entries to choose from and how the chosen one is delivered.
#[derive(Debug, Clone)]
pub struct InputPanel {
    kind: InputKind,
    episode: String,
    context: Vec<ContextLine>,
    items: Vec<ChooserItem>,
    plans: Vec<Plan>,
    selected: usize,
    /// The select field that stands for the label, when one does (KTD17).
    label_field: Option<String>,
    /// The values the conductor proposed, by entry and field name, so a number
    /// the operator did not touch is sent as a number.
    originals: Vec<(usize, String, Value)>,
    /// The label of the result held for approval.
    approval_label: Option<String>,
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
            Plan::EntryLimitContinue => HumanInputDeliveryDto {
                values: Map::from_iter([(
                    ENTRY_LIMIT_DECISION_FIELD.to_string(),
                    Value::String(ENTRY_LIMIT_CONTINUE.to_string()),
                )]),
                ..HumanInputDeliveryDto::default()
            },
            Plan::EntryLimitReset => HumanInputDeliveryDto {
                values: Map::from_iter([(
                    ENTRY_LIMIT_DECISION_FIELD.to_string(),
                    Value::String(ENTRY_LIMIT_RESET_COUNTER.to_string()),
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
            Some(Plan::EntryLimitContinue) => "kontynuowano po limicie".to_string(),
            Some(Plan::EntryLimitReset) => "wyzerowano licznik i kontynuowano".to_string(),
        }
    }

    /// The pattern typed for a `pattern` grant, refused when no rule can
    /// carry it; `None` for every other decision.
    fn permission_pattern(
        decision: &PermissionDecisionDto,
        submission: &Submission,
    ) -> Result<Option<String>, String> {
        if decision.scope != Some(PermissionGrantScopeDto::Pattern) {
            return Ok(None);
        }
        let pattern = Self::value_of(submission, PATTERN_FIELD)
            .unwrap_or_default()
            .trim();
        if bash_pattern_rule(pattern).is_none() {
            return Err(
                "Wzorzec nie może być pusty ani zawierać nawiasów lub znaków sterujących."
                    .to_string(),
            );
        }
        Ok(Some(pattern.to_string()))
    }

    fn value_of<'a>(submission: &'a Submission, name: &str) -> Option<&'a str> {
        submission
            .values
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.as_str())
    }

    /// Why the agent made its choice, `uzasadnienie: …`.
    fn push_reason(&mut self, reason: &str) {
        self.context.push(ContextLine::plain(format!(
            "uzasadnienie: {}",
            without_unsafe_chars(reason)
        )));
    }

    /// A value the agent gave, `name: value`.
    fn push_named_value(&mut self, name: &str, value: &str) {
        self.context.push(ContextLine::plain(format!(
            "{}: {}",
            without_unsafe_chars(name),
            without_unsafe_chars_multiline(value)
        )));
    }

    fn push(&mut self, item: ChooserItem, plan: Plan) {
        self.items.push(item);
        self.plans.push(plan);
    }

    // -- a gate ---------------------------------------------------------------

    fn build_gate(
        &mut self,
        pending: &PendingHumanInputView,
        resumed_state: &dyn Fn(&str) -> Option<String>,
    ) {
        let fields = pending.fields();
        let outputs = &pending.outputs;
        self.label_field = fields
            .iter()
            .find(|field| offers_exactly(field, outputs))
            .map(|field| field.name.clone());
        let others: Vec<&HumanInputFieldView> = fields
            .iter()
            .filter(|field| Some(&field.name) != self.label_field.as_ref())
            .collect();

        if outputs.is_empty() {
            let item = declared_fields(
                ChooserItem::new("Odpowiedz", pending.instruction()),
                &others,
                outputs,
                None,
            );
            self.push(item, Plan::Complete(None));
            return;
        }
        if self.label_field.is_none() && infers_label(fields) {
            let item = declared_fields(
                ChooserItem::new(
                    "wg wartości pola",
                    "Etykieta wyniku wynika z wybranej wartości pola.",
                ),
                &others,
                outputs,
                None,
            );
            self.push(item, Plan::Complete(None));
        }
        for output in outputs {
            let mut item = ChooserItem::new(
                output.label.clone(),
                output.description.clone().unwrap_or_default(),
            );
            if output.resume_comment_field.is_some() {
                let state = pending
                    .resume_targets
                    .get(&output.label)
                    .and_then(|target| resumed_state(&target.action_execution_id))
                    .unwrap_or_default();
                item = item.resuming(state);
            }
            let item = declared_fields(item, &others, outputs, Some(&output.label));
            self.push(item, Plan::Complete(Some(output.label.clone())));
        }
        // Reversible first: an entry that returns the work to the agent.
        self.selected = self
            .items
            .iter()
            .position(|item| item.resumes_agent.is_some())
            .unwrap_or(0);
    }

    fn complete(&self, label: Option<&str>, submission: &Submission) -> HumanInputDeliveryDto {
        let mut values = Map::new();
        for (name, value) in &submission.values {
            values.insert(name.clone(), Value::String(value.clone()));
        }
        if let (Some(field), Some(label)) = (&self.label_field, label) {
            values.insert(field.clone(), Value::String(label.to_string()));
        }
        HumanInputDeliveryDto {
            values,
            chosen_label: label.map(str::to_string),
            ..HumanInputDeliveryDto::default()
        }
    }

    // -- an agent's question ----------------------------------------------------

    fn build_question(&mut self, pending: &PendingHumanInputView) {
        let fields: Vec<&HumanInputFieldView> = pending.fields().iter().collect();
        let mut item = declared_fields(
            ChooserItem::new("Odpowiedz agentowi", "Odpowiedź wznawia sesję agenta.")
                .resuming(pending.state_id.to_string()),
            &fields,
            &[],
            None,
        );
        if let Some(answer) = pending.prefilled_answer.as_deref() {
            // The text of an answer a refused resume gave back (2a9c6f2).
            for field in item
                .fields
                .iter_mut()
                .filter(|field| field.name == "answer")
            {
                *field = field.clone().with_value(answer);
            }
        }
        self.push(item, Plan::Answer);
    }

    fn answer(&self, submission: &Submission) -> Result<HumanInputDeliveryDto, String> {
        let filled = |name: &str| Self::value_of(submission, name).is_some();
        if !filled("answer") && !filled("option") {
            return Err("Wpisz odpowiedź albo wybierz opcję.".to_string());
        }
        let values = submission
            .values
            .iter()
            .map(|(name, value)| (name.clone(), Value::String(value.clone())))
            .collect();
        Ok(HumanInputDeliveryDto {
            values,
            ..HumanInputDeliveryDto::default()
        })
    }

    // -- a problem --------------------------------------------------------------

    fn build_problem(&mut self, pending: &PendingHumanInputView) {
        let Some(problem) = &pending.problem else {
            return;
        };
        self.context
            .push(ContextLine::problem(problem.reason.clone()));
        for answer in &problem.answers {
            let mut text = format!("odrzucona odpowiedź agenta: {}", answer.reason);
            if let Some(tail) = &answer.answer_tail {
                text.push_str(&format!(" …{tail}"));
            }
            self.context.push(ContextLine::dim(text));
        }
        let analysis = problem.analysis.clone().unwrap_or_default();
        if let Some(rationale) = &analysis.rationale {
            self.context
                .push(ContextLine::conductor(format!("◆ dyrygent: {rationale}")));
        }
        let mut recommended = None;
        for output in &pending.outputs {
            let proposed = analysis.label.as_deref() == Some(output.label.as_str());
            let index = self.items.len();
            let mut item = ChooserItem::new(
                output.label.clone(),
                output.description.clone().unwrap_or_default(),
            );
            let requirement = problem.requirement_for(&output.label);
            let required_artifacts: &[String] =
                requirement.map_or(&[], |need| need.artifacts.as_slice());
            let required_variables: &[String] =
                requirement.map_or(&[], |need| need.variables.as_slice());
            let proposed_artifacts: Map<String, Value> = analysis
                .artifacts
                .iter()
                .map(|(name, path)| (name.clone(), Value::String(path.clone())))
                .collect();
            for (map, prefix, required) in [
                (&proposed_artifacts, "artifacts", required_artifacts),
                (&analysis.variables, "variables", required_variables),
            ] {
                let mut keys: Vec<&String> = required.iter().collect();
                if proposed {
                    keys.extend(map.keys().filter(|key| !required.contains(key)));
                }
                for key in keys {
                    let name = format!("{prefix}.{key}");
                    let mut field = ChooserField::multiline(name.clone());
                    if prefix == "artifacts" && required.contains(key) {
                        field = field
                            .required()
                            .described("wymagane przez stan, do którego prowadzi etykieta");
                    } else if required.contains(key) {
                        field = field.described("wymagane, chyba że kontekst już ma wartość");
                    }
                    if let Some(value) = proposed.then(|| map.get(key)).flatten() {
                        field = field.proposed(value_as_text(value));
                        self.originals.push((index, name, value.clone()));
                    }
                    item = item.with_field(field);
                }
            }
            item = item
                .with_field(ChooserField::multiline(EXTRA_VARIABLES_FIELD))
                .with_field(ChooserField::multiline(COMMENT_FIELD));
            if proposed {
                item = item.recommended();
                recommended = Some(index);
            }
            self.push(item, Plan::Output(output.label.clone()));
        }
        let message = ChooserItem::new(
            MESSAGE_ENTRY,
            "Wiadomość wznawia ten sam krok agenta; wynik ustawi on sam.",
        )
        .resuming(pending.state_id.to_string())
        .with_field(ChooserField::multiline(MESSAGE_FIELD).required());
        self.push(message, Plan::Message);
        // The conductor's label, or — without one — talking to the agent.
        self.selected = recommended.unwrap_or(self.items.len() - 1);
    }

    fn build_entry_limit(&mut self, pending: &PendingHumanInputView) {
        if let Some(limit) = &pending.entry_limit {
            self.context.push(ContextLine::problem(format!(
                "Wejścia: {}/{}",
                limit.entries, limit.max_entries
            )));
        }
        self.build_problem(pending);
        if self.plans.last() == Some(&Plan::Message) {
            self.plans.pop();
            self.items.pop();
        }
        let continue_index = self.items.len();
        self.push(
            ChooserItem::new(
                "Kontynuuj",
                "Uruchom krok jeszcze raz, zwiększając licznik.",
            ),
            Plan::EntryLimitContinue,
        );
        // The reset takes the loop guard away for a whole new round of
        // entries; the option says so.
        let reset = match &pending.entry_limit {
            Some(limit) => format!(
                "Wyzeruj licznik wejść i uruchom krok. Limit przestaje chronić przed pętlą, \
                 aż licznik znów dojdzie do {}.",
                limit.max_entries
            ),
            None => "Wyzeruj licznik wejść i uruchom krok. Limit przestaje chronić przed pętlą, \
                     aż licznik znów dojdzie do limitu."
                .to_string(),
        };
        self.push(
            ChooserItem::new("Wyzeruj licznik", reset),
            Plan::EntryLimitReset,
        );
        self.selected = continue_index;
    }

    fn output(
        &self,
        label: &str,
        submission: &Submission,
    ) -> Result<HumanInputDeliveryDto, String> {
        let mut maps = [Map::new(), Map::new()];
        for (name, text) in &submission.values {
            for (slot, prefix) in ["artifacts.", "variables."].into_iter().enumerate() {
                if let Some(key) = name.strip_prefix(prefix) {
                    let value = self.delivered_value(submission.index, name, text);
                    insert_once(&mut maps[slot], &prefix[..prefix.len() - 1], key, value)?;
                }
            }
        }
        let [artifacts, mut variables] = maps;
        for line in Self::value_of(submission, EXTRA_VARIABLES_FIELD)
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
            comment: Self::value_of(submission, COMMENT_FIELD)
                .map(str::trim)
                .map(str::to_string),
            ..HumanInputDeliveryDto::default()
        })
    }

    /// The proposal itself while its text is untouched, the typed text
    /// otherwise.
    fn delivered_value(&self, index: usize, name: &str, text: &str) -> Value {
        match self
            .originals
            .iter()
            .find(|(item, field, _)| *item == index && field == name)
        {
            Some((_, _, original)) if value_as_text(original) == text => original.clone(),
            _ => Value::String(text.to_string()),
        }
    }

    // -- an approval ------------------------------------------------------------

    fn build_permission_info(&mut self, pending: &PendingHumanInputView) {
        let denials = pending
            .permission
            .as_ref()
            .map(|permission| permission.denials.as_slice())
            .unwrap_or_default();
        let resumable = pending
            .agent_session_id
            .as_deref()
            .is_some_and(|id| !id.is_empty());
        let mut exact: Vec<Option<Vec<String>>> = Vec::new();
        let mut group: Vec<Option<Vec<String>>> = Vec::new();
        for denial in denials {
            let command = denial
                .input
                .get("command")
                .and_then(Value::as_str)
                .map(str::trim);
            self.context.push(ContextLine::problem(match command {
                Some(command) => format!("{}: {command}", denial.tool_name),
                None => format!("{}: {}", denial.tool_name, denial.input),
            }));
            self.context
                .push(ContextLine::dim(format!("katalog: {}", denial.cwd)));
            let rules = |scope| grant_rules(denial, scope);
            let (denial_exact, denial_group) =
                (rules(BashGrantScope::Exact), rules(BashGrantScope::Group));
            match (&denial_exact, why_not_grantable(denial, resumable)) {
                (Some(rules), None) => self.context.push(ContextLine::dim(format!(
                    "reguły zgody: {}",
                    rules.join(", ")
                ))),
                (_, Some(why)) => {
                    self.context
                        .push(ContextLine::plain(format!("zgoda niedostępna: {why}")));
                    if let Some(reason) = denial.reason.as_deref() {
                        self.context.push(ContextLine::dim(reason));
                    }
                }
                (None, None) => {}
            }
            exact.push(denial_exact.filter(|_| resumable));
            group.push(denial_group.filter(|_| resumable));
        }

        let many = denials.len() > 1;
        let all = |grants: &[Option<Vec<String>>]| -> Option<Vec<String>> {
            let mut rules: Vec<String> = Vec::new();
            for grant in grants {
                for rule in grant.as_ref()? {
                    if !rules.contains(rule) {
                        rules.push(rule.clone());
                    }
                }
            }
            (!rules.is_empty()).then_some(rules)
        };
        // A command no rule can pin down could still be approved in simpler
        // parts: the engine asks the agent for them along with the refusal.
        let unpinnable = denials.iter().any(|denial| {
            denial.tool_name == "Bash"
                && is_approvable_permission_reason(denial.reason_type.as_deref())
                && grant_rules(denial, BashGrantScope::Exact).is_none()
        });
        self.push(
            ChooserItem::new(
                if many { "Odmów wszystkim" } else { "Odmów" },
                if unpinnable {
                    "Agent wznowi tę sesję i dostanie prośbę, aby polecenia bez reguły \
                     zgody uruchomił jako proste polecenia, na które można dać zgodę."
                } else {
                    "Agent wznowi tę sesję z informacją, że te operacje są zablokowane."
                },
            )
            .with_field(ChooserField::multiline(MESSAGE_FIELD)),
            Plan::Permission(PermissionDecisionDto {
                denial_index: None,
                decision: PermissionDecisionKindDto::Deny,
                scope: None,
                pattern: None,
                message: None,
            }),
        );
        if let Some(rules) = all(&exact) {
            self.push(
                ChooserItem::new(
                    if many {
                        "Zezwól dokładnie na te polecenia"
                    } else {
                        "Zezwól dokładnie na to polecenie"
                    },
                    format!(
                        "Agent wznowi sesję i może je uruchomić. Reguły tylko dla bieżącego \
                         tasku: {}",
                        rules.join(", ")
                    ),
                ),
                Plan::Permission(PermissionDecisionDto {
                    denial_index: None,
                    decision: PermissionDecisionKindDto::Allow,
                    scope: Some(PermissionGrantScopeDto::ExactOperation),
                    pattern: None,
                    message: None,
                }),
            );
        } else if many {
            // Not every denial can be granted: offer the ones that can.
            for (index, rules) in exact.iter().enumerate() {
                let (Some(rules), Some(command)) = (
                    rules,
                    denials[index].input.get("command").and_then(Value::as_str),
                ) else {
                    continue;
                };
                self.push(
                    ChooserItem::new(
                        format!("Zezwól dokładnie: {}", command.trim()),
                        format!("Reguły tylko dla bieżącego tasku: {}", rules.join(", ")),
                    ),
                    Plan::Permission(PermissionDecisionDto {
                        denial_index: Some(index),
                        decision: PermissionDecisionKindDto::Allow,
                        scope: Some(PermissionGrantScopeDto::ExactOperation),
                        pattern: None,
                        message: None,
                    }),
                );
            }
        }
        if let Some(rules) = all(&group).filter(|rules| Some(rules) != all(&exact).as_ref()) {
            self.push(
                ChooserItem::new(
                    format!("Zezwól na grupę: {}", rules.join(", ")),
                    "Zgoda obejmie te grupy poleceń do końca bieżącego tasku.",
                ),
                Plan::Permission(PermissionDecisionDto {
                    denial_index: None,
                    decision: PermissionDecisionKindDto::Allow,
                    scope: Some(PermissionGrantScopeDto::CommandGroup),
                    pattern: None,
                    message: None,
                }),
            );
        }
        // The operator's own rule, for what the rules above do not cover:
        // a program outside the group list, or `*` for every command.
        let patternable = !denials.is_empty()
            && denials.iter().all(|denial| {
                denial.tool_name == "Bash"
                    && is_approvable_permission_reason(denial.reason_type.as_deref())
            });
        if resumable && patternable {
            let first = denials[0]
                .input
                .get("command")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default();
            self.push(
                ChooserItem::new(
                    "Zezwól według wzorca…",
                    "Wpisz całe polecenie albo jego początek z * na końcu, np. npm run *. \
                     Zgoda obejmie pasujące polecenia Bash do końca bieżącego tasku.",
                )
                .with_field(
                    ChooserField::text(PATTERN_FIELD)
                        .required()
                        .with_value(first)
                        .described(
                            "Polecenie złożone (&&, ;, |, pętla) Claude Code sprawdza \
                             po częściach: wtedy pomoże tylko *. Sam * zezwala na KAŻDE \
                             polecenie Bash.",
                        ),
                ),
                Plan::Permission(PermissionDecisionDto {
                    denial_index: None,
                    decision: PermissionDecisionKindDto::Allow,
                    scope: Some(PermissionGrantScopeDto::Pattern),
                    pattern: None,
                    message: None,
                }),
            );
        }
    }

    fn build_approval(&mut self, pending: &PendingHumanInputView) {
        let mut resumable = false;
        if let Some(approval) = &pending.approval {
            let result = &approval.result;
            let label = result.output_label.clone();
            self.context.push(ContextLine::plain(format!(
                "wybór agenta: {}",
                without_unsafe_chars(&label)
            )));
            if let Some(description) = pending
                .outputs
                .iter()
                .find(|output| output.label == label)
                .and_then(|output| output.description.clone())
            {
                self.context.push(ContextLine::dim(description));
            }
            let reason = result.reason.trim();
            self.push_reason(if reason.is_empty() { NO_REASON } else { reason });
            for (name, value) in &result.variables {
                self.push_named_value(name, &value_as_text(value));
            }
            for (name, path) in &result.artifacts {
                self.push_named_value(name, path);
            }
            resumable = approval.agent_session_id.is_some();
            self.approval_label = Some(label);
        }
        self.push(
            ChooserItem::new("Akceptuj", "Przyjmuje wynik agenta; zadanie idzie dalej."),
            Plan::Approve,
        );
        let (label, description) = if resumable {
            (
                "Zwróć agentowi",
                "Agent wznowi swoją sesję z Twoim komentarzem i przygotuje nowy wynik.",
            )
        } else {
            (
                "Nowa sesja",
                "Sesji agenta nie da się wznowić: komentarz zaczyna nową sesję i nowy wynik.",
            )
        };
        self.push(
            ChooserItem::new(label, description)
                .resuming(pending.state_id.to_string())
                .with_field(ChooserField::multiline(COMMENT_TO_AGENT).required()),
            Plan::Return,
        );
        // Going back is the safe default: accepting moves the work on.
        self.selected = 1;
    }
}

/// The field a message to the agent is typed into.
pub const MESSAGE_FIELD: &str = "Wiadomość dla agenta";

/// The field the operator types a Bash pattern into.
pub const PATTERN_FIELD: &str = "Wzorzec polecenia";

/// The field a return to the agent is explained in.
pub const COMMENT_TO_AGENT: &str = "Komentarz dla agenta";

/// The allow rules that would grant `denial` at `scope`, the same the
/// engine grants ([`narsor_protocol::api::bash_grant_rules`]).
fn grant_rules(denial: &PermissionDenialView, scope: BashGrantScope) -> Option<Vec<String>> {
    if denial.tool_name != "Bash" || !is_approvable_permission_reason(denial.reason_type.as_deref())
    {
        return None;
    }
    bash_grant_rules(denial.input.get("command")?.as_str()?, scope)
}

/// Why `denial` cannot be granted, in a few words; `None` when it can.
fn why_not_grantable(denial: &PermissionDenialView, resumable: bool) -> Option<String> {
    if !is_approvable_permission_reason(denial.reason_type.as_deref()) {
        return Some(format!(
            "blokada polityki Claude Code ({})",
            denial.reason_type.as_deref().unwrap_or("bez powodu")
        ));
    }
    if denial.tool_name != "Bash" {
        return Some(format!(
            "zgodę można dać tylko na polecenia Bash, nie na {}",
            denial.tool_name
        ));
    }
    if grant_rules(denial, BashGrantScope::Exact).is_none() {
        return Some(
            "polecenie ma konstrukcję, której reguła nie opisze dokładnie \
             ($(…), zmienna, glob, przekierowanie do pliku, nawiasy)"
                .to_string(),
        );
    }
    (!resumable).then(|| "sesji agenta nie da się wznowić".to_string())
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

/// `item` with a field for each of `fields`. A field is required when the
/// workflow says so — except a resume comment, which only the output that
/// resumes an agent needs (3f3dab2), and that output always needs, even when
/// the field itself is optional (KTD12).
fn declared_fields(
    mut item: ChooserItem,
    fields: &[&HumanInputFieldView],
    outputs: &[OutputDefinitionView],
    label: Option<&str>,
) -> ChooserItem {
    for field in fields {
        let claimed_by_other = label.is_some_and(|label| {
            outputs.iter().any(|output| {
                output.label != label
                    && output.resume_comment_field.as_deref() == Some(field.name.as_str())
            })
        });
        let claimed_by_own = label.is_some_and(|label| {
            outputs.iter().any(|output| {
                output.label == label
                    && output.resume_comment_field.as_deref() == Some(field.name.as_str())
            })
        });
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

// ---------------------------------------------------------------------------
// The inputs a workflow declares (the new-task form)
// ---------------------------------------------------------------------------
//
// A workflow's `inputs` are typed (`string`, `integer`, `boolean`,
// `repository`); the new-task form edits them as fields of the chooser, and
// the typed values become the task's JSON inputs exactly as `narsor run
// --input` parses them: `is_experimental=true` reaches the task as a boolean.

/// The two values of a `boolean` input.
const BOOLEAN_OPTIONS: [&str; 2] = ["true", "false"];

/// The labels drawn above the inputs the bundled workflows name alike. The
/// name stays the key of the value; an input not listed is drawn under its name.
const INPUT_LABELS: [(&str, &str); 2] = [("target_repo", "Repozytorium"), ("description", "Opis")];

/// One field per declared input, in declaration order.
///
/// * `boolean` is a choice between `true` and `false`; with no default it
///   starts on `false`, the value that changes nothing, so it is never unset
///   on screen;
/// * `repository` is a choice among the `repositories` the server has
///   registered, starting on the default or the first one — with none
///   registered it is a free line, so a remote the resolver still accepts can
///   be typed;
/// * `integer` is a number: the chooser refuses anything else next to the
///   field, before anything is sent;
/// * `string` is a free line.
///
/// An input is required when the workflow says so and gives no default; a
/// default is shown as the value the field starts with.
pub fn workflow_input_fields(
    inputs: &[WorkflowInputView],
    repositories: &[String],
) -> Vec<ChooserField> {
    inputs
        .iter()
        .map(|input| {
            let mut field = match input.input_type {
                WorkflowInputType::Boolean => ChooserField::select(
                    input.name.clone(),
                    BOOLEAN_OPTIONS
                        .iter()
                        .map(|option| option.to_string())
                        .collect(),
                )
                .with_value(BOOLEAN_OPTIONS[1]),
                WorkflowInputType::Repository if !repositories.is_empty() => {
                    ChooserField::select(input.name.clone(), repositories.to_vec())
                }
                WorkflowInputType::Integer => ChooserField::integer(input.name.clone()),
                WorkflowInputType::Text => ChooserField::multiline(input.name.clone()),
                WorkflowInputType::String | WorkflowInputType::Repository => {
                    ChooserField::text(input.name.clone())
                }
            };
            if let Some(default) = &input.default {
                field = field.with_value(value_as_text(default));
            }
            if input.required && input.default.is_none() {
                field = field.required();
            }
            if let Some((_, label)) = INPUT_LABELS.iter().find(|(name, _)| *name == input.name) {
                field = field.labelled(*label);
            }
            match input_description(input) {
                Some(description) => field.described(description),
                None => field,
            }
        })
        .collect()
}

/// What the current field is for: its type when that is not obvious, then
/// what the workflow says of it.
fn input_description(input: &WorkflowInputView) -> Option<String> {
    let kind = match input.input_type {
        WorkflowInputType::String | WorkflowInputType::Text => None,
        WorkflowInputType::Integer => Some("liczba całkowita"),
        WorkflowInputType::Boolean => Some("true / false"),
        WorkflowInputType::Repository => Some("repozytorium"),
    };
    let said = input
        .description
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());
    match (kind, said) {
        (Some(kind), Some(said)) => Some(format!("{kind} · {said}")),
        (Some(kind), None) => Some(kind.to_string()),
        (None, said) => said.map(str::to_string),
    }
}

/// A value of a workflow input that cannot be turned into its declared type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputProblem {
    /// The input it is about.
    pub field: String,
    /// What is wrong, in words for the field.
    pub message: String,
}

/// The task's inputs from what the form returned (`(name, value)`, a blank
/// optional field left out): every declared input that has a value or a
/// default, typed as declared.
pub fn workflow_input_values(
    inputs: &[WorkflowInputView],
    values: &[(String, String)],
) -> Result<Map<String, Value>, InputProblem> {
    let mut typed = Map::new();
    for input in inputs {
        let given = values
            .iter()
            .find(|(name, _)| *name == input.name)
            .map(|(_, value)| value.trim())
            .filter(|value| !value.is_empty());
        let Some(raw) = given else {
            if let Some(default) = &input.default {
                typed.insert(input.name.clone(), default.clone());
            }
            continue;
        };
        let value = match input.input_type {
            WorkflowInputType::String | WorkflowInputType::Text | WorkflowInputType::Repository => {
                Value::String(raw.to_string())
            }
            WorkflowInputType::Integer => {
                raw.parse::<i64>()
                    .map(Value::from)
                    .map_err(|_| InputProblem {
                        field: input.name.clone(),
                        message: format!("Pole '{}' wymaga liczby całkowitej.", input.name),
                    })?
            }
            WorkflowInputType::Boolean => match raw.to_ascii_lowercase().as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                other => {
                    return Err(InputProblem {
                        field: input.name.clone(),
                        message: format!(
                            "Pole '{}' wymaga true albo false, nie '{other}'.",
                            input.name
                        ),
                    })
                }
            },
        };
        typed.insert(input.name.clone(), value);
    }
    Ok(typed)
}
