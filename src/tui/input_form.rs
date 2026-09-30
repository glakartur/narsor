//! The human-input form behind `i` — one form for every kind of pending
//! input (R1): a workflow gate, an agent's own question and a problem.
//!
//! Like the rest of the TUI's state, this holds no terminal: the form is a
//! plain value that key presses fold into, and [`InputForm::validate`] turns
//! into the [`HumanInputDelivery`] the daemon receives.
//!
//! # Validation
//!
//! The form checks what the engine checks — required fields, `select`
//! values, a label where nothing can be inferred, the keys a problem's label
//! needs (KTD15) — so the operator hears about a mistake next to the row that
//! caused it. It is only the quick answer: the daemon validates again, and a
//! `400` it sends back keeps the form open with the typed content (see
//! `App::submit_input`).
//!
//! # One source of labels (KTD17)
//!
//! A gate whose `select` field offers exactly the state's outputs (like
//! `ambiguity_decision` in the bundled workflow) is drawn as a single choice
//! with the outputs' descriptions. The picked value is both the field value
//! and `chosen_label`, so the two can never disagree.

use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyEvent};
use serde_json::{Map, Value};

use crate::domain::{
    AnsweredBy, EventData, HumanInputField, HumanInputType, InputCloseReason, OutputDefinition,
};
use crate::engine::actions::human::is_blank;
use crate::engine::{
    HumanInputDelivery, InputContinuation, OutputRequirement, PendingHumanInput, RejectedAnswer,
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
}

impl InputKind {
    /// The kind a pending request's continuation names.
    pub fn of(continuation: InputContinuation) -> Self {
        match continuation {
            InputContinuation::CompleteStep => Self::Gate,
            InputContinuation::ResumeAgent => Self::AgentQuestion,
            InputContinuation::ResolveOutput => Self::Problem,
        }
    }

    /// The modal's title.
    pub fn title(self) -> &'static str {
        match self {
            Self::Gate => " Human input ",
            Self::AgentQuestion => " Reply to the agent ",
            Self::Problem => " Resolve the problem ",
        }
    }
}

/// The two maps a problem's answer supplies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryMap {
    /// The step's `data`.
    Data,
    /// The step's `variables`.
    Variables,
}

impl EntryMap {
    /// The name the delivery (and the engine's messages) use.
    pub fn name(self) -> &'static str {
        match self {
            Self::Data => "data",
            Self::Variables => "variables",
        }
    }
}

/// One row of the form, in the order rows are drawn and navigated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormRow {
    /// A declared field, by index.
    Field(usize),
    /// The output label picker, shown when the state recorded a menu and no
    /// field already chooses the label.
    Label,
    /// One key–value entry of a problem's `data` or `variables`.
    Entry(EntryMap, usize),
    /// The row that starts a new entry in `data` or `variables`.
    AddEntry(EntryMap),
    /// A problem's free-text comment, kept in the log with the answer.
    Comment,
}

/// One key–value pair of a problem's `data` or `variables`.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// The key.
    pub key: String,
    /// The value, as text.
    pub value: String,
    /// The proposed value the text was rendered from, sent as it was — a
    /// number stays a number — unless the operator edits the text.
    pub original: Option<Value>,
    /// Whether typing edits the key (`←`) rather than the value (`→`).
    pub editing_key: bool,
}

impl Entry {
    /// An empty entry for `key`, focused on its value.
    fn blank(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: String::new(),
            original: None,
            editing_key: false,
        }
    }

    /// An entry holding a proposed value.
    fn proposed(key: &str, value: &Value) -> Self {
        Self {
            key: key.to_string(),
            value: value_as_text(value),
            original: Some(value.clone()),
            editing_key: false,
        }
    }

    /// The value to deliver: the proposal itself while its text is
    /// untouched, the typed text otherwise.
    fn delivered_value(&self) -> Value {
        match &self.original {
            Some(original) if value_as_text(original) == self.value => original.clone(),
            _ => Value::String(self.value.clone()),
        }
    }
}

/// What a problem's form carries on top of a gate's.
#[derive(Debug, Clone)]
pub struct ProblemForm {
    /// Why the step could not go on by itself.
    pub reason: String,
    /// The agent's rejected answers, oldest first.
    pub answers: Vec<RejectedAnswer>,
    /// The conductor's reasoning, when it analysed the problem.
    pub rationale: Option<String>,
    /// What each label requires (KTD15).
    pub requirements: Vec<OutputRequirement>,
    /// Context variables that already hold a value: a label needing one of
    /// them is satisfied without the operator repeating it.
    pub context_variables: HashSet<String>,
    /// The `data` entries.
    pub data: Vec<Entry>,
    /// The `variables` entries.
    pub variables: Vec<Entry>,
    /// The comment.
    pub comment: String,
}

impl ProblemForm {
    /// The entries of one map.
    pub fn entries(&self, map: EntryMap) -> &[Entry] {
        match map {
            EntryMap::Data => &self.data,
            EntryMap::Variables => &self.variables,
        }
    }

    fn entries_mut(&mut self, map: EntryMap) -> &mut Vec<Entry> {
        match map {
            EntryMap::Data => &mut self.data,
            EntryMap::Variables => &mut self.variables,
        }
    }

    /// The keys of `map` that choosing `label` requires the delivery to
    /// hold — a variable the context already holds is not among them.
    pub fn required_keys(&self, label: Option<&str>, map: EntryMap) -> Vec<String> {
        let Some(requirement) = label.and_then(|label| {
            self.requirements
                .iter()
                .find(|requirement| requirement.label == label)
        }) else {
            return Vec::new();
        };
        match map {
            EntryMap::Data => requirement.data.clone(),
            EntryMap::Variables => requirement
                .variables
                .iter()
                .filter(|key| !self.context_variables.contains(key.as_str()))
                .cloned()
                .collect(),
        }
    }

    /// Adds an empty entry for every key `label` requires that has none, so
    /// the operator sees what is needed. Entries are never removed here:
    /// switching labels back and forth must not lose typed values.
    fn add_required_entries(&mut self, label: Option<&str>) {
        for map in [EntryMap::Data, EntryMap::Variables] {
            for key in self.required_keys(label, map) {
                let entries = self.entries_mut(map);
                if !entries.iter().any(|entry| entry.key == key) {
                    entries.push(Entry::blank(key));
                }
            }
        }
    }

    /// Builds one map for the delivery, checking the keys `label` needs.
    fn collect(&self, label: &str, map: EntryMap) -> Result<Map<String, Value>, FormError> {
        let mut collected = Map::new();
        for (index, entry) in self.entries(map).iter().enumerate() {
            let key = entry.key.trim();
            let blank = entry.value.trim().is_empty();
            if key.is_empty() {
                if blank {
                    continue;
                }
                return Err(FormError::at(
                    FormRow::Entry(map, index),
                    format!("An entry in {} has a value but no key.", map.name()),
                ));
            }
            if collected.contains_key(key) {
                return Err(FormError::at(
                    FormRow::Entry(map, index),
                    format!("'{}.{key}' is given twice.", map.name()),
                ));
            }
            if blank {
                continue;
            }
            collected.insert(key.to_string(), entry.delivered_value());
        }

        for key in self.required_keys(Some(label), map) {
            if collected.get(&key).is_none_or(is_blank) {
                let row = self
                    .entries(map)
                    .iter()
                    .position(|entry| entry.key.trim() == key)
                    .map(|index| FormRow::Entry(map, index));
                return Err(FormError {
                    row,
                    message: format!(
                        "Label '{label}' needs '{}.{key}': the state it leads to uses it.",
                        map.name()
                    ),
                });
            }
        }
        Ok(collected)
    }
}

/// A validation failure, with the row it is about when there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormError {
    /// The row to point at.
    pub row: Option<FormRow>,
    /// What is wrong.
    pub message: String,
}

impl FormError {
    fn at(row: FormRow, message: impl Into<String>) -> Self {
        Self {
            row: Some(row),
            message: message.into(),
        }
    }

    fn general(message: impl Into<String>) -> Self {
        Self {
            row: None,
            message: message.into(),
        }
    }
}

/// What a key press asks of whoever holds the form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormAction {
    /// The key was folded into the form.
    None,
    /// Validate and send.
    Submit,
    /// Close the form without sending anything.
    Cancel,
}

/// The human-input form, generated from a [`PendingHumanInput`].
///
/// A `text` field is an editable line, a `select` field cycles its `options`.
/// The label row exists because `human_clarification` has one text field and
/// two outputs: nothing can be inferred there, so the operator picks.
#[derive(Debug, Clone)]
pub struct InputForm {
    /// The execution the answer is for — kept here, not read from the
    /// selection, so a form opened from the task view answers the task it
    /// was opened on.
    pub execution_id: String,
    /// What kind of wait this answers.
    pub kind: InputKind,
    /// The gate's message, or the engine's description of the problem.
    pub instruction: String,
    /// The question the agent step before this request left open.
    pub question: Option<String>,
    /// The end of that agent step's answer.
    pub answer_tail: Option<String>,
    /// The declared fields, in declaration order.
    pub fields: Vec<HumanInputField>,
    /// The current value of each field, by index.
    pub values: Vec<String>,
    /// The state's outputs, with their descriptions.
    pub outputs: Vec<OutputDefinition>,
    /// The label choices: `None` means "let the engine infer it".
    pub label_choices: Vec<Option<String>>,
    /// Which label choice is selected.
    pub label_index: usize,
    /// The `select` field whose options are exactly the outputs (KTD17): its
    /// value is the chosen label, and no separate label row is shown.
    pub label_field: Option<usize>,
    /// What a problem adds: its answers, requirements and key–value maps.
    pub problem: Option<ProblemForm>,
    /// The focused row.
    pub row: usize,
    /// The last validation or delivery failure, if any.
    pub error: Option<String>,
    /// The row the last failure is about.
    pub error_row: Option<FormRow>,
    /// The key of the request the form was opened on (KTD9): the answer is
    /// refused if that request is no longer the one pending.
    pub episode: String,
    /// How the request was closed, once it no longer is the one pending
    /// (R5): the form stays open with what was typed, but cannot be sent.
    pub closed: Option<InputClosure>,
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
            data,
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
        for values in [Some(input), data.as_ref(), variables.as_ref()]
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
            Some(InputCloseReason::Interrupt) => {
                "Closed: the execution was interrupted.".to_string()
            }
            Some(InputCloseReason::Recover) => {
                "Closed: the step was recovered and starts afresh.".to_string()
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

impl InputForm {
    /// Builds the form for a pending request. `context` is the execution's
    /// context variables, which a problem's label may already be satisfied
    /// by.
    pub fn new(pending: &PendingHumanInput, context: &[(String, Value)]) -> Self {
        let kind = InputKind::of(pending.continuation);
        let fields = pending.fields().to_vec();
        let values = fields
            .iter()
            .map(|field| match field.field_type {
                // A select starts on its first option, so the common case is
                // one keystroke: Enter.
                HumanInputType::Select if field.required => {
                    field.options.first().cloned().unwrap_or_default()
                }
                HumanInputType::Select => String::new(),
                HumanInputType::Text => String::new(),
            })
            .collect();

        let label_field = fields
            .iter()
            .position(|field| offers_exactly(field, &pending.outputs));

        let mut label_choices: Vec<Option<String>> = Vec::new();
        if !pending.outputs.is_empty() && label_field.is_none() {
            if kind == InputKind::Gate && infers_label(&fields) {
                label_choices.push(None);
            }
            label_choices.extend(pending.output_labels().into_iter().map(Some));
        }

        let problem = pending.problem.as_ref().map(|problem| {
            let analysis = problem.analysis.clone().unwrap_or_default();
            let proposed = |map: &Map<String, Value>| -> Vec<Entry> {
                map.iter()
                    .map(|(key, value)| Entry::proposed(key, value))
                    .collect()
            };
            ProblemForm {
                reason: problem.reason.clone(),
                answers: problem.answers.clone(),
                rationale: analysis.rationale.clone(),
                requirements: problem.requirements.clone(),
                context_variables: context
                    .iter()
                    .filter(|(_, value)| !is_blank(value))
                    .map(|(name, _)| name.clone())
                    .collect(),
                data: proposed(&analysis.data),
                variables: proposed(&analysis.variables),
                comment: String::new(),
            }
        });

        // The conductor's proposed label is where a problem's form starts.
        let label_index = pending
            .problem
            .as_ref()
            .and_then(|problem| problem.analysis.as_ref())
            .and_then(|analysis| analysis.label.as_deref())
            .and_then(|label| {
                label_choices
                    .iter()
                    .position(|choice| choice.as_deref() == Some(label))
            })
            .unwrap_or(0);

        let mut form = Self {
            execution_id: pending.execution_id.clone(),
            kind,
            instruction: pending.instruction().to_string(),
            question: pending.request.question.clone(),
            answer_tail: pending.request.answer_tail.clone(),
            fields,
            values,
            outputs: pending.outputs.clone(),
            label_choices,
            label_index,
            label_field,
            problem,
            row: 0,
            error: None,
            error_row: None,
            episode: pending.episode_key().to_string(),
            closed: None,
        };
        form.sync_required_entries();
        form
    }

    /// The rows, in the order they are drawn and navigated.
    pub fn rows(&self) -> Vec<FormRow> {
        let mut rows: Vec<FormRow> = (0..self.fields.len()).map(FormRow::Field).collect();
        if !self.label_choices.is_empty() {
            rows.push(FormRow::Label);
        }
        if let Some(problem) = &self.problem {
            for map in [EntryMap::Data, EntryMap::Variables] {
                rows.extend(
                    (0..problem.entries(map).len()).map(|index| FormRow::Entry(map, index)),
                );
                rows.push(FormRow::AddEntry(map));
            }
            rows.push(FormRow::Comment);
        }
        rows
    }

    /// The focused row, if the form has any.
    pub fn current_row(&self) -> Option<FormRow> {
        self.rows().get(self.row).copied()
    }

    /// The label the operator picked, if they picked one — the value of the
    /// field that chooses it (KTD17), or the label row's choice.
    pub fn chosen_label(&self) -> Option<String> {
        if let Some(index) = self.label_field {
            return self
                .values
                .get(index)
                .filter(|value| !value.is_empty())
                .cloned();
        }
        self.label_choices.get(self.label_index).cloned().flatten()
    }

    /// The description of an output, when the workflow gives one.
    pub fn description_of(&self, label: &str) -> Option<&str> {
        self.outputs
            .iter()
            .find(|output| output.label == label)
            .and_then(|output| output.description.as_deref())
    }

    /// Whether `key` of `map` is required by the label picked now.
    pub fn is_required(&self, map: EntryMap, key: &str) -> bool {
        self.problem.as_ref().is_some_and(|problem| {
            problem
                .required_keys(self.chosen_label().as_deref(), map)
                .iter()
                .any(|required| required == key)
        })
    }

    /// Folds one key press into the form.
    pub fn on_key(&mut self, key: KeyEvent) -> FormAction {
        let rows = self.rows();
        match key.code {
            KeyCode::Esc => return FormAction::Cancel,
            KeyCode::Enter => return FormAction::Submit,
            KeyCode::Tab | KeyCode::Down => {
                if !rows.is_empty() {
                    self.row = (self.row + 1) % rows.len();
                }
            }
            KeyCode::BackTab | KeyCode::Up => {
                if !rows.is_empty() {
                    self.row = (self.row + rows.len() - 1) % rows.len();
                }
            }
            KeyCode::Left => self.step_left_right(-1),
            KeyCode::Right => self.step_left_right(1),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Char(character) => self.type_char(character),
            _ => {}
        }
        FormAction::None
    }

    /// Validates by the same rules the engine applies and builds the
    /// delivery.
    ///
    /// Duplicating the rules here is on purpose: the engine would reject the
    /// same delivery, but only after a round trip, and the operator deserves
    /// the complaint next to the row that caused it.
    pub fn validate(&self) -> Result<HumanInputDelivery, FormError> {
        let mut values = Map::new();

        for (index, field) in self.fields.iter().enumerate() {
            let raw = self.values.get(index).map(String::as_str).unwrap_or("");
            if raw.trim().is_empty() {
                if field.required {
                    return Err(FormError::at(
                        FormRow::Field(index),
                        format!("Field '{}' is required.", field.name),
                    ));
                }
                continue;
            }

            if field.field_type == HumanInputType::Select
                && !field.options.iter().any(|option| option == raw)
            {
                return Err(FormError::at(
                    FormRow::Field(index),
                    format!(
                        "'{raw}' is not a permitted value for field '{}'; permitted values: {}.",
                        field.name,
                        field.options.join(", ")
                    ),
                ));
            }

            values.insert(field.name.clone(), Value::String(raw.to_string()));
        }

        if self.kind == InputKind::AgentQuestion {
            // The engine needs an answer or an option, whichever fields the
            // agent's question declares as required.
            let filled = |name: &str| values.get(name).is_some_and(|value| !is_blank(value));
            if !filled("answer") && !filled("option") {
                return Err(FormError::general("Write an answer or pick an option."));
            }
        }

        let mut delivery = HumanInputDelivery::with_values(values).for_episode(&self.episode);

        if let Some(problem) = &self.problem {
            let Some(label) = self.chosen_label() else {
                return Err(FormError::general(
                    "This step declares no outputs to choose from.",
                ));
            };
            let data = problem.collect(&label, EntryMap::Data)?;
            let variables = problem.collect(&label, EntryMap::Variables)?;
            delivery = delivery
                .choosing(label)
                .with_data(data)
                .with_variables(variables);
            let comment = problem.comment.trim();
            if !comment.is_empty() {
                delivery = delivery.with_comment(comment);
            }
            return Ok(delivery);
        }

        if self.kind == InputKind::AgentQuestion {
            return Ok(delivery);
        }

        if let Some(label) = self.chosen_label() {
            delivery = delivery.choosing(label);
        } else if let Some(index) = self.label_field {
            return Err(FormError::at(
                FormRow::Field(index),
                format!(
                    "Pick a value for '{}': it chooses the output label.",
                    self.fields[index].name
                ),
            ));
        } else if !self.label_choices.is_empty() && !infers_label(&self.fields) {
            return Err(FormError::at(
                FormRow::Label,
                "Pick an output label: this state gives nothing to infer one from.",
            ));
        }
        Ok(delivery)
    }

    /// Marks the request as closed, keeping everything typed (R5). The first
    /// closure sticks: a later refresh must not replace the answer it named
    /// with a vaguer one.
    pub fn close(&mut self, closure: InputClosure) {
        if self.closed.is_none() {
            self.closed = Some(closure);
        }
    }

    /// Records a failure and moves the focus to the row it is about.
    pub fn fail(&mut self, error: FormError) {
        if let Some(index) = error
            .row
            .and_then(|row| self.rows().iter().position(|candidate| *candidate == row))
        {
            self.row = index;
        }
        self.error_row = error.row;
        self.error = Some(error.message);
    }

    /// Adds the entries the picked label needs, after the label changed.
    fn sync_required_entries(&mut self) {
        let label = self.chosen_label();
        if let Some(problem) = &mut self.problem {
            problem.add_required_entries(label.as_deref());
        }
    }

    /// `←`/`→`: a select's option, the label, or an entry's column.
    fn step_left_right(&mut self, delta: isize) {
        match self.current_row() {
            Some(FormRow::Field(index)) => {
                let field = &self.fields[index];
                if field.field_type != HumanInputType::Select || field.options.is_empty() {
                    return;
                }
                let next = match field
                    .options
                    .iter()
                    .position(|option| option == &self.values[index])
                {
                    Some(current) => wrap(current, delta, field.options.len()),
                    None if delta >= 0 => 0,
                    None => field.options.len() - 1,
                };
                self.values[index] = field.options[next].clone();
                self.sync_required_entries();
            }
            Some(FormRow::Label) => {
                if self.label_choices.is_empty() {
                    return;
                }
                self.label_index = wrap(self.label_index, delta, self.label_choices.len());
                self.sync_required_entries();
            }
            Some(FormRow::Entry(map, index)) => {
                let required = self.entry_key_locked(map, index);
                if let Some(entry) = self.entry_mut(map, index) {
                    // A required key is fixed: renaming it would only move
                    // the requirement to a fresh, empty entry.
                    entry.editing_key = delta < 0 && !required;
                }
            }
            _ => {}
        }
    }

    fn backspace(&mut self) {
        match self.current_row() {
            Some(FormRow::Field(index)) => {
                if self.fields[index].field_type == HumanInputType::Text {
                    self.values[index].pop();
                }
            }
            Some(FormRow::Entry(map, index)) => {
                let Some(entry) = self.entry_mut(map, index) else {
                    return;
                };
                if entry.editing_key {
                    if entry.key.is_empty() && entry.value.is_empty() {
                        // An emptied entry goes away; the focus stays on the
                        // row that takes its place.
                        if let Some(problem) = &mut self.problem {
                            problem.entries_mut(map).remove(index);
                        }
                    } else {
                        entry.key.pop();
                    }
                } else {
                    entry.value.pop();
                }
            }
            Some(FormRow::Comment) => {
                if let Some(problem) = &mut self.problem {
                    problem.comment.pop();
                }
            }
            _ => {}
        }
    }

    fn type_char(&mut self, character: char) {
        match self.current_row() {
            Some(FormRow::Field(index))
                if self.fields[index].field_type == HumanInputType::Text =>
            {
                self.values[index].push(character);
            }
            Some(FormRow::Entry(map, index)) => {
                if let Some(entry) = self.entry_mut(map, index) {
                    if entry.editing_key {
                        if !character.is_whitespace() {
                            entry.key.push(character);
                        }
                    } else {
                        entry.value.push(character);
                    }
                }
            }
            // Typing on the add row starts a new entry, its key first.
            Some(FormRow::AddEntry(map)) if !character.is_whitespace() => {
                if let Some(problem) = &mut self.problem {
                    let entries = problem.entries_mut(map);
                    entries.push(Entry {
                        key: character.to_string(),
                        value: String::new(),
                        original: None,
                        editing_key: true,
                    });
                    let index = entries.len() - 1;
                    if let Some(position) = self
                        .rows()
                        .iter()
                        .position(|row| *row == FormRow::Entry(map, index))
                    {
                        self.row = position;
                    }
                }
            }
            Some(FormRow::Comment) => {
                if let Some(problem) = &mut self.problem {
                    problem.comment.push(character);
                }
            }
            // On a select or the label row there is nothing to type, so
            // space steps to the next option.
            Some(FormRow::Field(_)) | Some(FormRow::Label) if character == ' ' => {
                self.step_left_right(1)
            }
            _ => {}
        }
    }

    fn entry_mut(&mut self, map: EntryMap, index: usize) -> Option<&mut Entry> {
        self.problem
            .as_mut()
            .and_then(|problem| problem.entries_mut(map).get_mut(index))
    }

    fn entry_key_locked(&self, map: EntryMap, index: usize) -> bool {
        self.problem
            .as_ref()
            .and_then(|problem| problem.entries(map).get(index))
            .is_some_and(|entry| self.is_required(map, &entry.key))
    }
}

/// Whether a single `select` field lets the engine infer the output label.
fn infers_label(fields: &[HumanInputField]) -> bool {
    fields
        .iter()
        .filter(|field| field.field_type == HumanInputType::Select)
        .count()
        == 1
}

/// Whether `field` is a `select` whose options are exactly `outputs`, in any
/// order (KTD17).
fn offers_exactly(field: &HumanInputField, outputs: &[OutputDefinition]) -> bool {
    if field.field_type != HumanInputType::Select
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
fn wrap(current: usize, delta: isize, length: usize) -> usize {
    ((current as isize + delta).rem_euclid(length as isize)) as usize
}
