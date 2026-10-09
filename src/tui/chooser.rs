//! The "list + description + fields" component (D-T9).
//!
//! Every choice in the interface is the same picture: a vertical list with
//! one line per entry, a separate place for the full description and effect
//! of the highlighted one, and — under them — the fields that entry needs.
//! The answer panel, the action menu, the new-task form and the repositories
//! all use it, so they look and behave alike.
//!
//! ```text
//! ┌ Akcje ─────────────────┐ ┌ Opis ─────────────────────────────────┐
//! │   approved             │ │ ↩ wraca do agenta: plan               │
//! │ ▸ revise  ↩          ◆ │ │ Zakres wymaga poprawy. Agent planu    │
//! │   reject               │ │ wznowi swoją sesję z Twoim komentarzem│
//! └────────────────────────┘ └───────────────────────────────────────┘
//! Komentarz dla agenta (wymagany):
//! > Uwzględnij migrację tabeli orders▏
//! ```
//!
//! Beside a wide area the description sits to the right of the list; below
//! [`SIDE_BY_SIDE_MIN_WIDTH`] columns it moves under the list.
//!
//! # State and drawing are apart
//!
//! [`Chooser`] is plain state: which entry is highlighted, where the focus is,
//! what has been typed. [`Chooser::on_key`] folds a key into it and says what
//! to do ([`ChooserAction`]); nothing here sends anything. [`render`] draws the
//! state. A test drives the first without a terminal and the second against a
//! `TestBackend`.
//!
//! # Fields keep what was typed
//!
//! Each entry owns its fields, so moving to another action and back finds the
//! text exactly where it was left. Editing goes through the primitives that
//! [`super::input_form`] shares with its own fields ([`text_insert`],
//! [`cycle_option`], …): one set of rules for the cursor, for `Alt+Enter` and
//! for stepping an option, whichever surface hosts the field.
//!
//! Text that comes from outside — workflow labels and descriptions, values
//! the runner proposed — goes through [`sanitize`] before it is drawn.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use super::input_form::{cycle_option, text_backspace, text_insert, text_step};
use super::keymap::{digit_of, lookup, Action, Context};
use super::scroll::{sanitize, wrap_visible, ScrollState};
use super::style::{Role, Styles};

/// The narrowest area in which the description still sits beside the list.
pub const SIDE_BY_SIDE_MIN_WIDTH: u16 = 64;

/// The mark that opens a field's text, and the cursor inside it.
const PROMPT: &str = "> ";
const CURSOR: char = '▏';

/// What a field accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldKind {
    /// Free text; `multiline` lets `Alt+Enter` start a new line.
    Text {
        /// Whether a newline may be typed.
        multiline: bool,
    },
    /// One of the options, stepped with `←`/`→` or space.
    Select(Vec<String>),
    /// A whole number.
    Integer,
}

/// One field of an entry, with what has been typed into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChooserField {
    /// The name shown above the value and returned with it.
    pub name: String,
    /// What it accepts.
    pub kind: FieldKind,
    /// Whether an answer cannot be sent without it.
    pub required: bool,
    /// What the field is for, shown dim beside its name.
    pub description: Option<String>,
    value: String,
    cursor: usize,
    proposed: bool,
    error: Option<String>,
}

impl ChooserField {
    fn new(name: impl Into<String>, kind: FieldKind) -> Self {
        Self {
            name: name.into(),
            kind,
            required: false,
            description: None,
            value: String::new(),
            cursor: 0,
            proposed: false,
            error: None,
        }
    }

    /// A single-line text field.
    pub fn text(name: impl Into<String>) -> Self {
        Self::new(name, FieldKind::Text { multiline: false })
    }

    /// A text field that takes new lines.
    pub fn multiline(name: impl Into<String>) -> Self {
        Self::new(name, FieldKind::Text { multiline: true })
    }

    /// A field that picks one of `options`; it starts on the first.
    pub fn select(name: impl Into<String>, options: Vec<String>) -> Self {
        let mut field = Self::new(name, FieldKind::Select(Vec::new()));
        if let Some(first) = options.first() {
            field.value = first.clone();
        }
        field.kind = FieldKind::Select(options);
        field
    }

    /// A whole-number field.
    pub fn integer(name: impl Into<String>) -> Self {
        Self::new(name, FieldKind::Integer)
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

    /// The same field holding a value the conductor proposed: marked `◆`
    /// until the operator changes it.
    pub fn proposed(mut self, value: impl Into<String>) -> Self {
        self.set_value(value);
        self.proposed = true;
        self
    }

    /// The same field holding `value`, not marked.
    pub fn with_value(mut self, value: impl Into<String>) -> Self {
        self.set_value(value);
        self
    }

    fn set_value(&mut self, value: impl Into<String>) {
        self.value = value.into();
        self.cursor = self.value.chars().count();
    }

    /// Takes over what the operator did to `from`, the same field of an
    /// earlier build of the entry: the value as it stands (and whether it is
    /// still the conductor's), the cursor and the complaint. A field whose
    /// value the operator never touched is left as the new build has it.
    pub fn carry_over(&mut self, from: &ChooserField) {
        if from.proposed {
            return;
        }
        self.value = from.value.clone();
        self.cursor = from.cursor;
        self.proposed = false;
        self.error = from.error.clone();
    }

    /// Whether the operator wrote something here that is not sent yet: a
    /// text value that is not blank and not the conductor's untouched one.
    pub fn is_drafted(&self) -> bool {
        !matches!(self.kind, FieldKind::Select(_)) && !self.proposed && !self.is_blank()
    }

    /// What is in the field.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Whether the value is still the one the conductor proposed.
    pub fn is_proposed(&self) -> bool {
        self.proposed
    }

    /// The complaint about the field, if the last send found one.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    fn is_blank(&self) -> bool {
        self.value.trim().is_empty()
    }

    /// Folds one key into the field. Returns whether it was used.
    fn edit(&mut self, event: &KeyEvent, newline: bool) -> bool {
        let used = match (&self.kind, event.code) {
            (FieldKind::Text { multiline: true }, _) if newline => {
                text_insert(&mut self.value, &mut self.cursor, '\n');
                true
            }
            (FieldKind::Text { .. }, KeyCode::Char(character)) => {
                text_insert(&mut self.value, &mut self.cursor, character);
                true
            }
            (FieldKind::Integer, KeyCode::Char(character))
                if character.is_ascii_digit() || character == '-' =>
            {
                text_insert(&mut self.value, &mut self.cursor, character);
                true
            }
            (FieldKind::Text { .. } | FieldKind::Integer, KeyCode::Backspace) => {
                text_backspace(&mut self.value, &mut self.cursor);
                true
            }
            (FieldKind::Text { .. } | FieldKind::Integer, KeyCode::Left) => {
                text_step(&self.value, &mut self.cursor, -1);
                return true;
            }
            (FieldKind::Text { .. } | FieldKind::Integer, KeyCode::Right) => {
                text_step(&self.value, &mut self.cursor, 1);
                return true;
            }
            (FieldKind::Select(options), KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')) => {
                let delta = if event.code == KeyCode::Left { -1 } else { 1 };
                if let Some(next) = cycle_option(options, &self.value, delta) {
                    self.value = next;
                }
                true
            }
            _ => false,
        };
        if used {
            self.proposed = false;
            self.error = None;
        }
        used
    }

    /// What is wrong with the value, if anything.
    fn problem(&self) -> Option<String> {
        if self.is_blank() {
            return self
                .required
                .then(|| format!("Pole '{}' jest wymagane.", self.name));
        }
        match &self.kind {
            FieldKind::Integer if self.value.trim().parse::<i64>().is_err() => {
                Some(format!("Pole '{}' wymaga liczby całkowitej.", self.name))
            }
            FieldKind::Select(options) if !options.contains(&self.value) => Some(format!(
                "'{}' nie jest dozwoloną wartością pola '{}'.",
                self.value, self.name
            )),
            _ => None,
        }
    }
}

/// One line of the list, with everything the description pane says about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChooserItem {
    /// The line in the list.
    pub label: String,
    /// The full description and effect, shown beside (or under) the list.
    pub description: String,
    /// `↩`: choosing it resumes the agent; names the state it returns to.
    pub resumes_agent: Option<String>,
    /// `◆`: the conductor recommends it.
    pub recommended: bool,
    /// `!`: it cannot be undone.
    pub irreversible: bool,
    /// Why the entry cannot be chosen now; `None` when it can.
    pub unavailable: Option<String>,
    /// The fields choosing it needs.
    pub fields: Vec<ChooserField>,
}

impl ChooserItem {
    /// An entry with a label and a description.
    pub fn new(label: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            description: description.into(),
            resumes_agent: None,
            recommended: false,
            irreversible: false,
            unavailable: None,
            fields: Vec::new(),
        }
    }

    /// The same entry, resuming the agent at `state` (`↩`).
    pub fn resuming(mut self, state: impl Into<String>) -> Self {
        self.resumes_agent = Some(state.into());
        self
    }

    /// The same entry, recommended by the conductor (`◆`).
    pub fn recommended(mut self) -> Self {
        self.recommended = true;
        self
    }

    /// The same entry, irreversible (`!`).
    pub fn irreversible(mut self) -> Self {
        self.irreversible = true;
        self
    }

    /// The same entry, greyed out for `reason`.
    pub fn unavailable(mut self, reason: impl Into<String>) -> Self {
        self.unavailable = Some(reason.into());
        self
    }

    /// The same entry with one more field.
    pub fn with_field(mut self, field: ChooserField) -> Self {
        self.fields.push(field);
        self
    }

    /// What the markers of this entry say, one line each, for the top of
    /// its description.
    fn effect_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(reason) = &self.unavailable {
            lines.push(format!("niedostępne: {reason}"));
        }
        if let Some(state) = &self.resumes_agent {
            lines.push(if state.is_empty() {
                "↩ wraca do agenta".to_string()
            } else {
                format!("↩ wraca do agenta: {state}")
            });
        }
        if self.recommended {
            lines.push("◆ rekomendacja dyrygenta".to_string());
        }
        if self.irreversible {
            lines.push("! nieodwracalne".to_string());
        }
        lines
    }
}

/// Where the keys go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChooserFocus {
    /// The list: moving, picking, sending.
    Actions,
    /// The fields of the highlighted entry: typing.
    Fields,
}

/// What a chosen entry carries out of the chooser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission {
    /// The index of the entry.
    pub index: usize,
    /// Its label.
    pub label: String,
    /// The fields that have a value, `(name, value)`, in order. A blank
    /// optional field is left out.
    pub values: Vec<(String, String)>,
}

/// What the owner of the chooser should do after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChooserAction {
    /// Nothing outside the chooser changed.
    None,
    /// Send this.
    Submit(Submission),
    /// Collapse; what was typed stays in the chooser.
    Cancel,
    /// The entry cannot be chosen; tell the operator why.
    Blocked(String),
}

/// The state of one chooser.
#[derive(Debug, Clone)]
pub struct Chooser {
    items: Vec<ChooserItem>,
    selected: usize,
    focus: ChooserFocus,
    field: usize,
    description_scroll: ScrollState,
}

impl Chooser {
    /// A chooser over `items`, on the first one.
    pub fn new(items: Vec<ChooserItem>) -> Self {
        Self {
            items,
            selected: 0,
            focus: ChooserFocus::Actions,
            field: 0,
            description_scroll: ScrollState::new(),
        }
    }

    /// The same chooser, on entry `index` (the recommended or the safest one).
    pub fn selecting(mut self, index: usize) -> Self {
        self.select(index);
        self
    }

    /// The entries.
    pub fn items(&self) -> &[ChooserItem] {
        &self.items
    }

    /// The index of the highlighted entry.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The highlighted entry.
    pub fn selected_item(&self) -> Option<&ChooserItem> {
        self.items.get(self.selected)
    }

    /// Where the keys go.
    pub fn focus(&self) -> ChooserFocus {
        self.focus
    }

    /// The index of the focused field of the highlighted entry.
    pub fn field_index(&self) -> usize {
        self.field
    }

    /// The key-map context of the chooser. Always the panel's: its hints
    /// and help are the same whichever half has the focus.
    pub fn context(&self) -> Context {
        Context::Panel
    }

    /// Whether keys are being typed into a field, so the owner must not
    /// treat `q` or `?` as commands.
    pub fn is_typing(&self) -> bool {
        self.focus == ChooserFocus::Fields
            && self
                .selected_item()
                .and_then(|item| item.fields.get(self.field))
                .is_some_and(|field| !matches!(field.kind, FieldKind::Select(_)))
    }

    /// The fields of entry `index`, with what was typed.
    pub fn fields_of(&self, index: usize) -> &[ChooserField] {
        self.items
            .get(index)
            .map(|item| item.fields.as_slice())
            .unwrap_or_default()
    }

    /// The same chooser over new `items`, as when what an entry offers became
    /// known after it was first built. What the operator typed carries over to
    /// the fields that are still there (by entry label and field name), the
    /// highlighted entry is the same one when it still exists, and the focus
    /// stays where it was.
    pub fn rebuilt(mut self, mut items: Vec<ChooserItem>) -> Self {
        let label = self.selected_item().map(|item| item.label.clone());
        for item in &mut items {
            let Some(old) = self.items.iter().find(|old| old.label == item.label) else {
                continue;
            };
            for field in &mut item.fields {
                if let Some(earlier) = old.fields.iter().find(|earlier| earlier.name == field.name) {
                    field.carry_over(earlier);
                }
            }
        }
        let selected = label
            .and_then(|label| items.iter().position(|item| item.label == label))
            .unwrap_or_else(|| self.selected.min(items.len().saturating_sub(1)));
        let focus = self.focus;
        let field = self.field;
        let scroll = self.description_scroll;
        self.items = items;
        self.selected = selected;
        self.description_scroll = scroll;
        let kept = self.field.min(self.field_count().saturating_sub(1));
        self.focus = if self.field_count() == 0 { ChooserFocus::Actions } else { focus };
        self.field = if self.focus == ChooserFocus::Fields { field.min(kept) } else { 0 };
        self
    }

    /// What the operator wrote in any entry and has not sent: `name: value`
    /// lines, entry by entry. Empty when nothing is drafted.
    pub fn draft_text(&self) -> String {
        let mut lines = Vec::new();
        for item in &self.items {
            for field in item.fields.iter().filter(|field| field.is_drafted()) {
                lines.push(format!("{} / {}: {}", item.label, field.name, field.value));
            }
        }
        lines.join("\n")
    }

    /// How many rows [`render`] would like for a `width`-column area: the
    /// list and the description (beside each other when wide, stacked when
    /// narrow) and the fields of the highlighted entry under them.
    pub fn preferred_height(&self, width: u16) -> u16 {
        let list = self.items.len() as u16 + 2;
        let top = if width >= SIDE_BY_SIDE_MIN_WIDTH {
            list.max(5)
        } else {
            list + 4
        };
        top + self.field_rows()
    }

    /// Puts the keys in the fields of the highlighted entry, as `Tab` does
    /// (for an owner whose only entry is a text box).
    pub fn enter_fields(&mut self) {
        self.focus_fields();
    }

    /// Takes the keys out of the fields; what was typed stays.
    pub fn leave_fields(&mut self) {
        self.focus = ChooserFocus::Actions;
    }

    /// The rows the fields of the highlighted entry take.
    pub fn field_rows(&self) -> u16 {
        let Some(item) = self.selected_item() else {
            return 0;
        };
        let rows: usize = item
            .fields
            .iter()
            .map(|field| {
                let value_rows = match field.kind {
                    FieldKind::Select(_) => 1,
                    _ => field.value.split('\n').count().max(1),
                };
                1 + value_rows + usize::from(field.error.is_some())
            })
            .sum();
        rows as u16
    }

    /// Highlights entry `index`, if there is one. The fields keep their
    /// values; the focus returns to the list.
    pub fn select(&mut self, index: usize) {
        if index < self.items.len() {
            self.selected = index;
            self.focus = ChooserFocus::Actions;
            self.field = 0;
            self.description_scroll = ScrollState::new();
        }
    }

    /// Folds one key into the chooser.
    pub fn on_key(&mut self, event: KeyEvent) -> ChooserAction {
        match self.focus {
            ChooserFocus::Actions => self.on_actions_key(event),
            ChooserFocus::Fields => self.on_fields_key(event),
        }
    }

    fn on_actions_key(&mut self, event: KeyEvent) -> ChooserAction {
        let total = self.items.len();
        let Some(action) = lookup(Context::Panel, &event) else {
            return ChooserAction::None;
        };
        match action {
            Action::Down => self.select((self.selected + 1).min(total.saturating_sub(1))),
            Action::Up => self.select(self.selected.saturating_sub(1)),
            Action::Pick => {
                if let Some(digit) = digit_of(&event) {
                    self.select(usize::from(digit) - 1);
                }
            }
            Action::PageDown => self.description_scroll.page_down(self.description_lines()),
            Action::PageUp => self.description_scroll.page_up(self.description_lines()),
            Action::SwitchFocus => self.focus_fields(),
            Action::Submit => return self.activate(),
            Action::Cancel => return ChooserAction::Cancel,
            _ => {}
        }
        ChooserAction::None
    }

    /// `Enter` on the list: a blocked entry says why, an entry with required
    /// fields takes the operator to the first field, anything else is sent.
    fn activate(&mut self) -> ChooserAction {
        let Some(item) = self.selected_item() else {
            return ChooserAction::None;
        };
        if let Some(reason) = &item.unavailable {
            return ChooserAction::Blocked(reason.clone());
        }
        if item.fields.iter().any(|field| field.required) {
            self.focus_fields();
            return ChooserAction::None;
        }
        self.submit()
    }

    fn focus_fields(&mut self) {
        if self
            .selected_item()
            .is_some_and(|item| !item.fields.is_empty())
        {
            self.focus = ChooserFocus::Fields;
            self.field = self.field.min(self.field_count().saturating_sub(1));
        }
    }

    fn on_fields_key(&mut self, event: KeyEvent) -> ChooserAction {
        match lookup(Context::Panel, &event) {
            Some(Action::SwitchFocus) => {
                self.focus = ChooserFocus::Actions;
                return ChooserAction::None;
            }
            Some(Action::Submit) => return self.submit(),
            Some(Action::Cancel) => return ChooserAction::Cancel,
            Some(Action::Newline) => {
                self.edit_field(&event, true);
                return ChooserAction::None;
            }
            _ => {}
        }
        match event.code {
            KeyCode::Down => self.move_field(1),
            KeyCode::Up => self.move_field(-1),
            _ => self.edit_field(&event, false),
        }
        ChooserAction::None
    }

    fn edit_field(&mut self, event: &KeyEvent, newline: bool) {
        let index = self.field;
        if let Some(field) = self
            .items
            .get_mut(self.selected)
            .and_then(|item| item.fields.get_mut(index))
        {
            field.edit(event, newline);
        }
    }

    fn move_field(&mut self, delta: isize) {
        let count = self.field_count();
        if count > 0 {
            self.field = (self.field as isize + delta).clamp(0, count as isize - 1) as usize;
        }
    }

    fn field_count(&self) -> usize {
        self.selected_item().map_or(0, |item| item.fields.len())
    }

    /// Sends the highlighted entry: validates its fields, and on the first
    /// problem puts the complaint on the field and the focus on it.
    fn submit(&mut self) -> ChooserAction {
        let selected = self.selected;
        let Some(item) = self.items.get_mut(selected) else {
            return ChooserAction::None;
        };
        if let Some(reason) = &item.unavailable {
            return ChooserAction::Blocked(reason.clone());
        }
        for field in &mut item.fields {
            field.error = None;
        }
        if let Some((index, message)) = item
            .fields
            .iter()
            .enumerate()
            .find_map(|(index, field)| field.problem().map(|message| (index, message)))
        {
            item.fields[index].error = Some(message);
            self.field = index;
            self.focus = ChooserFocus::Fields;
            return ChooserAction::None;
        }
        ChooserAction::Submit(Submission {
            index: selected,
            label: item.label.clone(),
            values: item
                .fields
                .iter()
                .filter(|field| !field.is_blank())
                .map(|field| (field.name.clone(), field.value.clone()))
                .collect(),
        })
    }

    /// The description pane's logical lines for the highlighted entry:
    /// what its markers mean, then its own text. External text, sanitised.
    fn description_text(&self) -> Vec<String> {
        let Some(item) = self.selected_item() else {
            return Vec::new();
        };
        let mut lines: Vec<String> = item
            .effect_lines()
            .iter()
            .flat_map(|line| {
                sanitize(line)
                    .lines()
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .collect();
        lines.extend(sanitize(&item.description).lines().map(str::to_string));
        lines
    }

    fn description_lines(&self) -> usize {
        self.description_text().len()
    }
}

/// A single-line, sanitised version of external text.
fn one_line(text: &str) -> String {
    sanitize(text).replace('\n', " ")
}

/// How wide the list needs to be for its longest line.
fn list_width(items: &[ChooserItem]) -> u16 {
    let widest = items
        .iter()
        .map(|item| one_line(&item.label).chars().count())
        .max()
        .unwrap_or(0);
    // cursor, number, label, three marker slots, border.
    (widest + 4 + 8) as u16
}

/// One line of the list: cursor, number, label and the markers.
fn list_line(
    item: &ChooserItem,
    index: usize,
    selected: bool,
    width: usize,
    styles: &Styles,
) -> Line<'static> {
    let base = if item.unavailable.is_some() {
        styles.of(Role::Done)
    } else {
        Style::default()
    };
    let cursor = if selected { "▸ " } else { "  " };
    let number = if index < 9 {
        format!("{} ", index + 1)
    } else {
        "  ".to_string()
    };
    let label = one_line(&item.label);
    let mut spans = vec![
        Span::styled(cursor, styles.of(Role::Focus)),
        Span::styled(number, styles.of(Role::Done)),
        Span::styled(label.clone(), base),
    ];
    let mut used = 4 + label.chars().count();
    let markers: [(bool, &str, Role); 3] = [
        (item.resumes_agent.is_some(), "↩", Role::Calm),
        (item.recommended, "◆", Role::Conductor),
        (item.irreversible, "!", Role::Problem),
    ];
    for (present, symbol, role) in markers {
        if present {
            spans.push(Span::styled(format!(" {symbol}"), styles.of(role)));
            used += 2;
        }
    }
    if selected {
        spans.push(Span::raw(" ".repeat(width.saturating_sub(used))));
    }
    let line = Line::from(spans);
    if selected {
        line.style(styles.selected(Style::default()))
    } else {
        line
    }
}

/// The lines of the fields of the highlighted entry, and the line the focus
/// is on (for scrolling).
fn field_lines(chooser: &Chooser, styles: &Styles) -> (Vec<Line<'static>>, usize) {
    let Some(item) = chooser.selected_item() else {
        return (Vec::new(), 0);
    };
    let focused = (chooser.focus == ChooserFocus::Fields).then_some(chooser.field);
    let mut lines = Vec::new();
    let mut focus_line = 0;
    for (index, field) in item.fields.iter().enumerate() {
        let is_focused = focused == Some(index);
        let mut header = vec![Span::styled(
            one_line(&field.name),
            if is_focused {
                styles.of(Role::Focus)
            } else {
                Style::default()
            },
        )];
        if field.required {
            header.push(Span::styled(" (wymagane)", styles.of(Role::Done)));
        }
        if field.proposed {
            header.push(Span::styled(" ◆", styles.of(Role::Conductor)));
        }
        if let Some(description) = &field.description {
            header.push(Span::styled(
                format!(" · {}", one_line(description)),
                styles.of(Role::Done),
            ));
        }
        lines.push(Line::from(header));
        let value = sanitize(&field.value);
        let rows = field_value_rows(field, &value, is_focused);
        for (row, text) in rows.into_iter().enumerate() {
            let prefix = if row == 0 { PROMPT } else { "  " };
            lines.push(Line::from(vec![
                Span::styled(prefix, styles.of(Role::Done)),
                Span::raw(text),
            ]));
        }
        if is_focused {
            focus_line = lines.len() - 1;
        }
        if let Some(error) = &field.error {
            lines.push(Line::styled(
                format!("✗ {}", one_line(error)),
                styles.of(Role::Problem),
            ));
            if is_focused {
                focus_line = lines.len() - 1;
            }
        }
    }
    (lines, focus_line)
}

/// The rows of a field's value; a focused text field has the cursor in it.
fn field_value_rows(field: &ChooserField, value: &str, focused: bool) -> Vec<String> {
    let mut text = value.to_string();
    match &field.kind {
        FieldKind::Select(_) => {
            return vec![format!("‹ {value} ›")];
        }
        FieldKind::Text { .. } | FieldKind::Integer => {
            if focused {
                let byte = text
                    .char_indices()
                    .nth(field.cursor)
                    .map_or(text.len(), |(index, _)| index);
                text.insert(byte, CURSOR);
            }
        }
    }
    text.split('\n').map(str::to_string).collect()
}

/// Draws `chooser` into `area`: the list and the description (side by side,
/// or stacked when the area is narrow) and the fields under them.
pub fn render(frame: &mut Frame, area: Rect, chooser: &mut Chooser, styles: &Styles) {
    let (fields, focus_line) = field_lines(chooser, styles);
    let fields_height = (fields.len() as u16).min(area.height.saturating_sub(5));
    let [top, bottom] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(fields_height)]).areas(area);

    let wide = area.width >= SIDE_BY_SIDE_MIN_WIDTH;
    let list_rows = chooser.items.len() as u16 + 2;
    let (list_area, description_area) = if wide {
        let width = list_width(&chooser.items).clamp(20, area.width / 2);
        let [list, description] =
            Layout::horizontal([Constraint::Length(width), Constraint::Min(10)]).areas(top);
        (list, description)
    } else {
        let height = list_rows.min(top.height.saturating_sub(3)).max(3);
        let [list, description] =
            Layout::vertical([Constraint::Length(height), Constraint::Min(3)]).areas(top);
        (list, description)
    };

    render_list(frame, list_area, chooser, styles);
    render_description(frame, description_area, chooser, styles);
    if fields_height > 0 {
        let offset = (focus_line + 1).saturating_sub(fields_height as usize) as u16;
        frame.render_widget(Paragraph::new(fields).scroll((offset, 0)), bottom);
    }
}

/// Draws only the fields of the highlighted entry into `area`, scrolled so
/// that the focused one is in view: a text box without the list around it.
pub fn render_fields(frame: &mut Frame, area: Rect, chooser: &Chooser, styles: &Styles) {
    let (fields, focus_line) = field_lines(chooser, styles);
    let height = usize::from(area.height);
    let offset = (focus_line + 1).saturating_sub(height) as u16;
    frame.render_widget(Paragraph::new(fields).scroll((offset, 0)), area);
}

fn pane(title: &str, active: bool, styles: &Styles) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(styles.of(if active { Role::Focus } else { Role::Done }))
        .title(format!(" {title} "))
}

fn render_list(frame: &mut Frame, area: Rect, chooser: &Chooser, styles: &Styles) {
    let block = pane("Akcje", chooser.focus == ChooserFocus::Actions, styles);
    let inner = block.inner(area);
    let width = usize::from(inner.width);
    let height = usize::from(inner.height);
    // Keep the highlighted line in view when the list is taller than its pane.
    let first = (chooser.selected + 1).saturating_sub(height);
    let lines: Vec<Line> = chooser
        .items
        .iter()
        .enumerate()
        .skip(first)
        .take(height)
        .map(|(index, item)| list_line(item, index, index == chooser.selected, width, styles))
        .collect();
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_description(frame: &mut Frame, area: Rect, chooser: &mut Chooser, styles: &Styles) {
    let block = pane("Opis", false, styles);
    let inner = block.inner(area);
    let text = chooser.description_text();
    chooser
        .description_scroll
        .set_viewport(usize::from(inner.height));
    chooser.description_scroll.clamp(text.len());
    let rows = wrap_visible(
        &text,
        usize::from(inner.width),
        chooser.description_scroll.offset(),
        usize::from(inner.height),
    );
    let lines: Vec<Line> = rows.into_iter().map(Line::from).collect();
    // An unavailable entry's whole description recedes; its first line is
    // the reason, in words.
    let dim = chooser
        .selected_item()
        .is_some_and(|item| item.unavailable.is_some());
    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(
        if dim {
            paragraph.style(styles.of(Role::Done))
        } else {
            paragraph
        },
        area,
    );
}
