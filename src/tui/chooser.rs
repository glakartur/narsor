//! The "list + description + fields" component (D-T9).
//!
//! Every choice in the interface is the same picture: a vertical list with
//! one line per entry, a separate place for the full description and effect
//! of the highlighted one, and — under them — the fields that entry needs.
//! The answer panel, the action menu, the new-task form and the repositories
//! all use it, so they look and behave alike.
//!
//! ```text
//! ╭ Akcje ─────────────────╮ ╭ Opis ─────────────────────────────────╮
//! │   1 approved           │ │ ↩ wraca do agenta: plan               │
//! │ | 2 | revise           │ │ ◆ rekomendacja dyrygenta              │
//! │   3 reject             │ │ Zakres wymaga poprawy. Agent planu    │
//! │────────────────────────│ │                                       │
//! ╰────────────────────────╯ ╰───────────────────────────────────────╯
//! Komentarz dla agenta (wymagany):
//! > Uwzględnij migrację tabeli orders
//! ```
//!
//! Beside a wide area the description sits to the right of the list; below
//! [`SIDE_BY_SIDE_MIN_WIDTH`] columns it moves under the list.
//!
//! The list stacks its entries and puts pipes around the highlighted action
//! number; its description carries the action's recommendation and risk.
//! When it does not fit its pane it scrolls inside it, by whole entries,
//! and keeps where it was across a rebuild (R17).
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
//! text exactly where it was left. The rules of the fields — editing with
//! `Delete`, `Home` and `End`, wrapping, the cursor as a style of its cell,
//! moving between fields, the confirmation before a send and the events a form
//! reports to its owner — are in [`super::form`], not here: this module puts
//! the list, the description and a form together. A form is a list of fields
//! and an area to draw them in.
//!
//! Text that comes from outside — workflow labels and descriptions, values
//! the runner proposed — goes through [`sanitize`] before it is drawn.

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use super::form::buffer::TextBuffer;
use super::form::engine::clamp_rows;
use super::form::engine::FormEngine;
use super::form::events::{FormEvent, Growth};
use super::form::field;
use super::form::layout::{WindowRow, WrapWidth};
use super::input_form::{cycle_option, pasted_text};
use super::keymap::{digit_of, lookup, Action, Context};
use super::scroll::{one_line, sanitize, wrap_visible, ScrollState};
use super::selection::{bracketed_list_rows, render_bracketed_list, PREFIX_WIDTH};
use super::style::{Role, Styles};

/// The narrowest area in which the description still sits beside the list.
pub const SIDE_BY_SIDE_MIN_WIDTH: u16 = 64;

/// What a field accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldKind {
    /// Free text; `multiline` lets the new line key start a new line.
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
    /// The text drawn above the value, when it is not the name.
    label: Option<String>,
    /// Whether an answer cannot be sent without it.
    pub required: bool,
    /// What the field is for, shown dim beside its name.
    pub description: Option<String>,
    /// The most characters the operator can type or paste, if any.
    max_chars: Option<usize>,
    buffer: TextBuffer,
    proposed: bool,
    error: Option<String>,
    /// The width the text was last wrapped to, so that `↑`/`↓` walk the rows
    /// that are drawn.
    wrap_width: WrapWidth,
    /// The first row drawn of a field taller than its window.
    window_row: WindowRow,
}

impl ChooserField {
    fn new(name: impl Into<String>, kind: FieldKind) -> Self {
        Self {
            name: name.into(),
            kind,
            label: None,
            required: false,
            description: None,
            max_chars: None,
            buffer: TextBuffer::default(),
            proposed: false,
            error: None,
            wrap_width: WrapWidth::default(),
            window_row: WindowRow::default(),
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
            field.buffer.set(first.clone());
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

    /// The same field drawn under `label`; the name stays the key of its value.
    pub fn labelled(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// The text drawn above the value: the label, or the name.
    pub fn label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.name)
    }

    /// The same field with a description.
    pub fn described(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// The same field, taking at most `max` characters from the keyboard and
    /// the clipboard. A value set in code is not cut.
    pub fn limited(mut self, max: usize) -> Self {
        self.max_chars = Some(max);
        self
    }

    /// How many characters can still be added.
    fn room(&self) -> usize {
        self.max_chars.map_or(usize::MAX, |max| {
            max.saturating_sub(self.value().chars().count())
        })
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
        self.buffer.set(value);
    }

    /// Takes over what the operator did to `from`, the same field of an
    /// earlier build of the entry: the value as it stands (and whether it is
    /// still the conductor's), the cursor and the complaint. A field whose
    /// value the operator never touched is left as the new build has it.
    pub fn carry_over(&mut self, from: &ChooserField) {
        if from.proposed {
            return;
        }
        self.buffer = from.buffer.clone();
        self.proposed = false;
        self.error = from.error.clone();
    }

    /// Whether `other` asks for the same thing: name, kind, wording and
    /// whether it is required — the typed value is not compared.
    fn same_definition(&self, other: &ChooserField) -> bool {
        self.name == other.name
            && self.kind == other.kind
            && self.label == other.label
            && self.required == other.required
            && self.description == other.description
    }

    /// Whether the operator wrote something here that is not sent yet: a
    /// text value that is not blank and not the conductor's untouched one.
    pub fn is_drafted(&self) -> bool {
        !matches!(self.kind, FieldKind::Select(_)) && !self.proposed && !self.is_blank()
    }

    /// What is in the field.
    pub fn value(&self) -> &str {
        self.buffer.value()
    }

    /// Whether the value is still the one the conductor proposed.
    pub fn is_proposed(&self) -> bool {
        self.proposed
    }

    /// The complaint about the field, if the last send found one.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Where the cursor is, in characters of the value.
    pub fn cursor(&self) -> usize {
        self.buffer.cursor()
    }

    /// Puts pasted text at the cursor, as one edit, and the cursor after it.
    /// A `select` takes no paste. Returns whether anything was inserted.
    pub fn paste(&mut self, pasted: &str) -> bool {
        let Some(text) = pasted_text(&self.kind, pasted) else {
            return false;
        };
        let text: String = text.chars().take(self.room()).collect();
        if text.is_empty() {
            return false;
        }
        self.buffer.insert_str(&text);
        self.proposed = false;
        self.error = None;
        true
    }

    /// The width the text was last wrapped to.
    pub(crate) fn wrap_width(&self) -> &WrapWidth {
        &self.wrap_width
    }

    /// The first row of the window the field was last drawn in.
    pub(crate) fn window_row(&self) -> &WindowRow {
        &self.window_row
    }

    /// Whether the field takes several lines.
    pub(crate) fn is_long(&self) -> bool {
        matches!(self.kind, FieldKind::Text { multiline: true })
    }

    fn is_blank(&self) -> bool {
        self.buffer.value().trim().is_empty()
    }

    /// Folds one key into the field. Returns whether it was used.
    fn edit(&mut self, event: &KeyEvent, newline: bool) -> bool {
        let used = match (&self.kind, event.code) {
            // The new line key is never text: in a one-line field it is ignored.
            (FieldKind::Text { multiline: true }, _) if newline => {
                self.buffer.insert('\n');
                true
            }
            _ if newline => return false,
            (FieldKind::Text { .. }, KeyCode::Char(character)) => {
                if self.room() > 0 {
                    self.buffer.insert(character);
                }
                true
            }
            (FieldKind::Integer, KeyCode::Char(character))
                if character.is_ascii_digit() || character == '-' =>
            {
                self.buffer.insert(character);
                true
            }
            (FieldKind::Text { .. } | FieldKind::Integer, KeyCode::Backspace) => {
                self.buffer.backspace();
                true
            }
            (FieldKind::Text { .. } | FieldKind::Integer, KeyCode::Delete) => {
                self.buffer.delete();
                true
            }
            (FieldKind::Text { .. } | FieldKind::Integer, KeyCode::Left) => {
                self.buffer.left();
                return true;
            }
            (FieldKind::Text { .. } | FieldKind::Integer, KeyCode::Right) => {
                self.buffer.right();
                return true;
            }
            (FieldKind::Text { .. } | FieldKind::Integer, KeyCode::Home) => {
                self.buffer.home();
                return true;
            }
            (FieldKind::Text { .. } | FieldKind::Integer, KeyCode::End) => {
                self.buffer.end();
                return true;
            }
            (FieldKind::Select(options), KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')) => {
                let delta = if event.code == KeyCode::Left { -1 } else { 1 };
                if let Some(next) = cycle_option(options, self.buffer.value(), delta) {
                    self.buffer.set(next);
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
            FieldKind::Integer if self.value().trim().parse::<i64>().is_err() => {
                Some(format!("Pole '{}' wymaga liczby całkowitej.", self.name))
            }
            FieldKind::Select(options) if !options.iter().any(|o| o == self.value()) => {
                Some(format!(
                    "'{}' nie jest dozwoloną wartością pola '{}'.",
                    self.value(),
                    self.name
                ))
            }
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
    /// The layout of a form of inputs: with the focus in the fields, the list
    /// folds into a strip and the fields take the screen.
    inputs_layout: bool,
    /// Whether Enter opens fields even when they are optional.
    enter_fields_on_activate: bool,
    /// The first entry in view: the list scrolls inside its pane, and a
    /// rebuild keeps it where it was.
    top: usize,
    /// What the form keeps between keys: the edge press, the confirmation.
    form: FormEngine,
    /// Whether sending asks the operator first (R12).
    confirms_submit: bool,
    /// How the fields may grow, as the owner last said.
    growth: Cell<Option<Growth>>,
    /// The width the fields were last drawn in.
    area_width: Cell<usize>,
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
            inputs_layout: false,
            enter_fields_on_activate: false,
            top: 0,
            form: FormEngine::new(),
            confirms_submit: false,
            growth: Cell::new(None),
            area_width: Cell::new(0),
        }
    }

    /// Tells how the fields may grow: the way and the most rows. Past the
    /// limit the field scrolls instead. The owner says it again whenever its
    /// area changes.
    pub fn set_growth(&self, growth: Growth) {
        self.growth.set(Some(growth));
    }

    /// The rows the fields want in an area `width` columns wide, within the
    /// limit of [`Chooser::set_growth`], and at least one.
    pub fn preferred_rows(&self, width: usize) -> u16 {
        let wanted = usize::from(self.field_rows_in(width));
        let limit = self
            .growth
            .get()
            .map_or(usize::MAX, |growth| growth.limit_rows);
        clamp_rows(wanted, limit)
    }

    /// What happened to the form since the owner last asked: a value edited,
    /// the rows it wants changed, a send asked for, made or given up, the
    /// focus gained or lost. The owner reacts in its own loop; nothing is
    /// called back.
    pub fn take_events(&mut self) -> Vec<FormEvent> {
        self.form.take_events()
    }

    /// Gives the keys to the form or takes them away. Without them the form
    /// takes no key and keeps what was typed and where its cursor stands.
    pub fn set_focus(&mut self, focused: bool) {
        let events = self.form.set_focus(focused);
        self.form.queue(events);
    }

    /// Whether the form takes keys.
    pub fn has_focus(&self) -> bool {
        self.form.has_focus()
    }

    /// The same chooser, asking "Zatwierdzić formularz?" before it sends.
    /// For a form that sends; a list that only picks does not use it.
    pub fn confirming_submit(mut self) -> Self {
        self.confirms_submit = true;
        self
    }

    /// Whether the confirmation popup is open. While it is, it has the keys.
    pub fn is_confirming(&self) -> bool {
        self.form.is_confirming()
    }

    /// The same chooser in the layout of a form of inputs: `Enter` on an entry
    /// with fields goes into them; there the fields stand in one column, the
    /// main multi-line one fills the rest of the screen, and `Esc` returns to
    /// the list.
    pub fn with_inputs_layout(mut self) -> Self {
        self.inputs_layout = true;
        self
    }

    /// Enters any attached fields with Enter, including optional ones.
    pub fn enter_fields_on_activate(mut self) -> Self {
        self.enter_fields_on_activate = true;
        self
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

    /// Whether the keys are in the fields of the highlighted entry.
    pub(crate) fn fields_focused(&self) -> bool {
        self.focus == ChooserFocus::Fields
    }

    /// The index of the field with the keys, in the highlighted entry.
    pub(crate) fn field_index(&self) -> usize {
        self.field
    }

    /// The highlighted entry.
    pub fn selected_item(&self) -> Option<&ChooserItem> {
        self.items.get(self.selected)
    }

    /// Where the keys go.
    pub fn focus(&self) -> ChooserFocus {
        self.focus
    }

    /// The key-map context of the chooser. Always the panel's: its hints
    /// and help are the same whichever half has the focus.
    pub fn context(&self) -> Context {
        if self.form.is_confirming() {
            Context::FormConfirm
        } else {
            Context::Panel
        }
    }

    /// Whether keys are being typed into a field, so the owner must not
    /// treat `q` or `?` as commands.
    pub fn is_typing(&self) -> bool {
        self.form.is_confirming()
            || self.focus == ChooserFocus::Fields
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

    /// Puts a complaint next to the field `name` of entry `index` — the
    /// server's verdict on what was typed there — and the keys on that field.
    /// Returns whether there is such a field.
    pub fn set_field_error(
        &mut self,
        index: usize,
        name: &str,
        message: impl Into<String>,
    ) -> bool {
        let Some(position) = self
            .items
            .get(index)
            .and_then(|item| item.fields.iter().position(|field| field.name == name))
        else {
            return false;
        };
        self.items[index].fields[position].error = Some(message.into());
        self.select(index);
        self.focus = ChooserFocus::Fields;
        self.field = position;
        true
    }

    /// The same chooser over new `items`, as when what an entry offers became
    /// known after it was first built. What the operator typed carries over to
    /// the fields that are still there (by entry label and field name), the
    /// highlighted entry is the same one when it still exists, the focus
    /// stays where it was and so does the scrolled list. An open
    /// confirmation stays open when the highlighted entry is the same and can
    /// still be sent, and closes otherwise: the operator confirms again what
    /// is now there.
    pub fn rebuilt(mut self, mut items: Vec<ChooserItem>) -> Self {
        // The popup asked about an entry that may be gone or changed.
        let asked = self.form.decline();
        let label = self.selected_item().map(|item| item.label.clone());
        let earlier = self.selected_item().cloned();
        for item in &mut items {
            let Some(old) = self.items.iter().find(|old| old.label == item.label) else {
                continue;
            };
            for field in &mut item.fields {
                if let Some(earlier) = old.fields.iter().find(|earlier| earlier.name == field.name)
                {
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
        self.focus = if self.field_count() == 0 {
            ChooserFocus::Actions
        } else {
            focus
        };
        self.field = if self.focus == ChooserFocus::Fields {
            field.min(kept)
        } else {
            0
        };
        if let Some(return_field) = asked {
            let unchanged = match (earlier, self.selected_item()) {
                (Some(earlier), Some(now)) => {
                    earlier.label == now.label
                        && earlier.description == now.description
                        && earlier.resumes_agent == now.resumes_agent
                        && earlier.recommended == now.recommended
                        && earlier.irreversible == now.irreversible
                        && earlier.unavailable == now.unavailable
                        && earlier.fields.len() == now.fields.len()
                        && earlier
                            .fields
                            .iter()
                            .zip(&now.fields)
                            .all(|(a, b)| a.same_definition(b))
                }
                _ => false,
            };
            if unchanged && self.is_sendable() {
                self.form.ask_confirmation(return_field.min(self.field));
            }
        }
        self
    }

    /// What the operator wrote in any entry and has not sent: `name: value`
    /// lines, entry by entry. Empty when nothing is drafted.
    pub fn draft_text(&self) -> String {
        let mut lines = Vec::new();
        for item in &self.items {
            for field in item.fields.iter().filter(|field| field.is_drafted()) {
                lines.push(format!(
                    "{} / {}: {}",
                    item.label,
                    field.name,
                    field.value()
                ));
            }
        }
        lines.join("\n")
    }

    /// How many rows [`render`] would like for a `width`-column area: the
    /// list and the description (beside each other when wide, stacked when
    /// narrow) and the fields of the highlighted entry under them.
    pub fn preferred_height(&self, width: u16) -> u16 {
        let list = list_rows(self.items.len(), false);
        let top = if width >= SIDE_BY_SIDE_MIN_WIDTH {
            list.max(LIST_MIN_ROWS)
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
        self.field_rows_in(usize::MAX)
    }

    /// The rows the fields of the highlighted entry take in an area `width`
    /// columns wide, their text wrapped to it.
    pub fn field_rows_in(&self, width: usize) -> u16 {
        let Some(item) = self.selected_item() else {
            return 0;
        };
        let focused = (self.focus == ChooserFocus::Fields).then_some(self.field);
        let rows: usize = item
            .fields
            .iter()
            .enumerate()
            .map(|(index, field)| {
                1 + field::value_rows(field, width, focused == Some(index))
                    + usize::from(field.error.is_some())
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
        if !self.form.has_focus() {
            return ChooserAction::None;
        }
        let before = self.edit_signature();
        let was_confirming = self.form.is_confirming();
        let action = self.route_key(event);
        self.note_changes(&before, was_confirming, &action);
        action
    }

    /// What the active field holds and where its cursor is, to tell whether
    /// a key edited it.
    fn edit_signature(&self) -> (String, usize) {
        self.selected_item()
            .and_then(|item| item.fields.get(self.field))
            .map(|field| (field.value().to_string(), field.cursor()))
            .unwrap_or_default()
    }

    /// Turns what a key did into the events of the form.
    fn note_changes(
        &mut self,
        before: &(String, usize),
        was_confirming: bool,
        action: &ChooserAction,
    ) {
        let mut events = Vec::new();
        if self.edit_signature().0 != before.0 {
            events.push(FormEvent::Edited);
        }
        if let Some(growth) = self.growth.get() {
            let wanted = usize::from(self.field_rows_in(self.area_width.get().max(1)));
            events.extend(self.form.resize(wanted, growth.limit_rows));
        }
        if !was_confirming && self.form.is_confirming() {
            events.push(FormEvent::ConfirmRequested);
        }
        match action {
            ChooserAction::Submit(_) => events.push(FormEvent::Submitted),
            ChooserAction::Cancel => events.push(FormEvent::Cancelled),
            _ => {}
        }
        self.form.queue(events);
    }

    fn route_key(&mut self, event: KeyEvent) -> ChooserAction {
        if self.form.is_confirming() {
            return self.on_confirm_key(&event);
        }
        match self.focus {
            ChooserFocus::Actions => self.on_actions_key(event),
            ChooserFocus::Fields => self.on_fields_key(event),
        }
    }

    /// Folds pasted text into the field being typed in. A paste never sends
    /// and never moves the focus; with the keys on the list, or on a
    /// `select`, it is ignored. Returns whether a field took it.
    pub fn paste(&mut self, pasted: &str) -> bool {
        if self.focus != ChooserFocus::Fields || self.form.is_confirming() || !self.form.has_focus()
        {
            return false;
        }
        self.form.clear_edge();
        let before = self.edit_signature();
        let used = self.paste_into_field(pasted);
        self.note_changes(&before, false, &ChooserAction::None);
        used
    }

    fn paste_into_field(&mut self, pasted: &str) -> bool {
        let index = self.field;
        self.items
            .get_mut(self.selected)
            .and_then(|item| item.fields.get_mut(index))
            .is_some_and(|field| field.paste(pasted))
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

    /// `Enter` on the list: a blocked entry says why, an entry with fields
    /// takes the operator to them, and a fieldless entry is sent.
    fn activate(&mut self) -> ChooserAction {
        let Some(item) = self.selected_item() else {
            return ChooserAction::None;
        };
        if let Some(reason) = &item.unavailable {
            return ChooserAction::Blocked(reason.clone());
        }
        if ((self.inputs_layout || self.enter_fields_on_activate) && !item.fields.is_empty())
            || item.fields.iter().any(|field| field.required)
        {
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
            self.field = if self.inputs_layout {
                self.focus_order().first().copied().unwrap_or(0)
            } else {
                self.field.min(self.field_count().saturating_sub(1))
            };
        }
    }

    /// The indices of the highlighted entry's fields in the order the focus
    /// walks them: as declared, which is also the order they are drawn in.
    fn focus_order(&self) -> Vec<usize> {
        let Some(item) = self.selected_item() else {
            return Vec::new();
        };
        (0..item.fields.len()).collect()
    }

    /// The multi-line field a form of inputs draws large when the focus is
    /// not on one: the first required one, else the first, else the first
    /// field. Only drawing uses it; the focus starts on the first field (R9).
    pub(crate) fn main_field(&self) -> usize {
        let Some(item) = self.selected_item() else {
            return 0;
        };
        main_field_of(&item.fields).unwrap_or(0)
    }

    fn on_fields_key(&mut self, event: KeyEvent) -> ChooserAction {
        // Only two arrows in a row arm the edge; any other key starts over.
        if !matches!(event.code, KeyCode::Up | KeyCode::Down) {
            self.form.clear_edge();
        }
        if self.inputs_layout && matches!(event.code, KeyCode::Tab | KeyCode::BackTab) {
            if event.code == KeyCode::BackTab && self.focus_order().first() == Some(&self.field) {
                self.leave_fields();
            } else {
                let order = self.focus_order();
                if event.code == KeyCode::Tab && order.last() == Some(&self.field) {
                    if let Some(first) = order.first() {
                        self.field = *first;
                    }
                } else {
                    self.move_field(if event.code == KeyCode::Tab { 1 } else { -1 });
                }
            }
            return ChooserAction::None;
        }
        if event.code == KeyCode::Esc && self.inputs_layout {
            self.leave_fields();
            return ChooserAction::None;
        }
        match lookup(Context::Panel, &event) {
            Some(Action::SwitchFocus) => {
                self.focus = ChooserFocus::Actions;
                return ChooserAction::None;
            }
            Some(Action::Submit) => return self.advance_field_or_submit(),
            Some(Action::Cancel) => return ChooserAction::Cancel,
            Some(Action::Newline) => {
                self.form.clear_edge();
                self.edit_field(&event, true);
                return ChooserAction::None;
            }
            _ => {}
        }
        match event.code {
            KeyCode::Down => self.step_field_or_cursor(1),
            KeyCode::Up => self.step_field_or_cursor(-1),
            _ => {
                self.form.clear_edge();
                self.edit_field(&event, false);
            }
        }
        ChooserAction::None
    }

    /// The keys of the confirmation popup, and no others: `Enter` sends,
    /// `Esc` returns to the form as it was. Anything else does nothing.
    fn on_confirm_key(&mut self, event: &KeyEvent) -> ChooserAction {
        // Only the plain keys: Shift+Enter, Ctrl+Enter and Alt+Enter are not
        // the `Enter` of the window.
        if !event.modifiers.is_empty() {
            return ChooserAction::None;
        }
        match lookup(Context::FormConfirm, event) {
            Some(Action::Accept) if self.form.accept() => {
                // Sent only if the entry still passes what `submit` checks.
                if self.is_sendable() {
                    self.submission()
                } else {
                    self.submit()
                }
            }
            Some(Action::Cancel) => {
                self.form.decline();
                ChooserAction::None
            }
            _ => ChooserAction::None,
        }
    }

    fn advance_field_or_submit(&mut self) -> ChooserAction {
        let order = self.focus_order();
        let Some(position) = order.iter().position(|&index| index == self.field) else {
            return self.submit();
        };
        if let Some(next) = order.get(position + 1) {
            self.field = *next;
            ChooserAction::None
        } else {
            self.submit()
        }
    }

    /// `↑`/`↓` in a field. A multi-line field first walks the rows that are
    /// drawn; on the first or last row the first press puts the cursor at the
    /// start or the end of the text and the second leaves for the next field
    /// (R10). A one-line field, a `select` and a number leave at once.
    fn step_field_or_cursor(&mut self, delta: isize) {
        let Some(field) = self
            .items
            .get_mut(self.selected)
            .and_then(|item| item.fields.get_mut(self.field))
        else {
            return;
        };
        let multiline = field.is_long();
        if !multiline {
            self.form.clear_edge();
            let at_first = self.focus_order().first() == Some(&self.field);
            if delta < 0 && at_first {
                self.focus = ChooserFocus::Actions;
                return;
            }
            self.move_field(delta);
            return;
        }
        let wrapped = field::layout_of_current(field);
        if let Some(target) = wrapped.vertical(delta) {
            self.form.clear_edge();
            field.buffer.set_cursor(target);
            return;
        }
        if self.form.take_edge(delta) {
            if delta < 0 {
                let current = self.field;
                self.move_field(-1);
                if self.field == current {
                    self.focus = ChooserFocus::Actions;
                }
            } else {
                self.move_field(1);
            }
        } else if delta < 0 {
            field.buffer.set_cursor(0);
        } else {
            let end = field.buffer.len();
            field.buffer.set_cursor(end);
        }
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
        let order = self.focus_order();
        let Some(position) = order.iter().position(|&index| index == self.field) else {
            return;
        };
        let target = (position as isize + delta).clamp(0, order.len() as isize - 1);
        self.field = order[target as usize];
    }

    fn field_count(&self) -> usize {
        self.selected_item().map_or(0, |item| item.fields.len())
    }

    /// Sends the highlighted entry: validates its fields, and on the first
    /// problem puts the complaint on the field and the focus on it. A form
    /// that confirms asks first; the send goes out on the popup's `Enter`.
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
        if let Some((index, message)) = (0..item.fields.len())
            .find_map(|index| item.fields[index].problem().map(|message| (index, message)))
        {
            item.fields[index].error = Some(message);
            self.field = index;
            self.focus = ChooserFocus::Fields;
            return ChooserAction::None;
        }
        // An irreversible entry has its own window, which the owner opens.
        if self.confirms_submit && !item.irreversible {
            self.form.ask_confirmation(self.field);
            return ChooserAction::None;
        }
        self.submission()
    }

    /// Whether the highlighted entry is available and has no field with a
    /// problem.
    fn is_sendable(&self) -> bool {
        self.selected_item().is_some_and(|item| {
            item.unavailable.is_none() && item.fields.iter().all(|field| field.problem().is_none())
        })
    }

    /// What the highlighted entry sends.
    fn submission(&self) -> ChooserAction {
        let Some(item) = self.items.get(self.selected) else {
            return ChooserAction::None;
        };
        ChooserAction::Submit(Submission {
            index: self.selected,
            label: item.label.clone(),
            values: item
                .fields
                .iter()
                .filter(|field| !field.is_blank())
                .map(|field| (field.name.clone(), field.value().to_string()))
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

/// The multi-line field that takes the room of a form: the first required
/// one, else the first one. The caller falls back to the first field.
pub(crate) fn main_field_of(fields: &[ChooserField]) -> Option<usize> {
    fields
        .iter()
        .position(|field| field.is_long() && field.required)
        .or_else(|| fields.iter().position(ChooserField::is_long))
}

/// The rows of the list pane's frame.
const LIST_FRAME_ROWS: u16 = 2;

/// The rows a list pane needs for `entries` choices and its frame.
fn list_rows(entries: usize, footer: bool) -> u16 {
    if footer {
        (entries.min(u16::MAX as usize) as u16).saturating_add(LIST_FRAME_ROWS)
    } else {
        bracketed_list_rows(entries).saturating_add(LIST_FRAME_ROWS)
    }
}

/// The rows of the smallest list pane: one choice and its frame.
const LIST_MIN_ROWS: u16 = bracketed_list_rows(1) + LIST_FRAME_ROWS;

/// How wide the list needs to be for its longest line.
fn list_width(items: &[ChooserItem], footer: bool) -> u16 {
    let widest = items
        .iter()
        .map(|item| one_line(&item.label).chars().count())
        .max()
        .unwrap_or(0);
    if footer {
        (widest + items.len().max(1).to_string().len() + 10) as u16
    } else {
        // Number, label, three marker slots, and frame.
        (widest + usize::from(PREFIX_WIDTH) + 2 + 6 + 2) as u16
    }
}

/// One line of the list: number, label, and a pipe around the selected number.
/// Recommendation and irreversibility marks lead the description instead.
fn list_line(
    item: &ChooserItem,
    index: usize,
    selected: usize,
    focused: bool,
    styles: &Styles,
) -> Line<'static> {
    let base = if item.unavailable.is_some() {
        styles.of(Role::Done)
    } else {
        Style::default()
    };
    let digits = (index + 1).to_string();
    let mut spans = Vec::new();
    if index == selected {
        let role = if focused { Role::Focus } else { Role::Done };
        spans.push(Span::styled(format!("| {digits} | "), styles.of(role)));
    } else {
        spans.push(Span::styled(
            format!("  {digits}   "),
            styles.of(Role::Done),
        ));
    }
    spans.push(Span::styled(one_line(&item.label), base));
    Line::from(spans)
}

/// The lines of the fields of the highlighted entry, wrapped to `width`
/// columns, and the line the focus is on (for scrolling).
fn field_lines(
    chooser: &Chooser,
    styles: &Styles,
    owner_focused: bool,
    width: usize,
) -> (Vec<Line<'static>>, usize) {
    let Some(item) = chooser.selected_item() else {
        return (Vec::new(), 0);
    };
    chooser.area_width.set(width);
    let focused = (owner_focused && chooser.focus == ChooserFocus::Fields).then_some(chooser.field);
    let mut lines = Vec::new();
    let mut focus_line = 0;
    for (index, field) in item.fields.iter().enumerate() {
        let is_focused = focused == Some(index);
        lines.push(field::header_line(field, width, is_focused, styles));
        let value_start = lines.len();
        let (rows, cursor_row) = field::value_lines(field, width, is_focused, styles);
        lines.extend(rows);
        if is_focused {
            focus_line = value_start + cursor_row;
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

/// Draws `chooser` into `area`: the list and the description (side by side,
/// or stacked when the area is narrow) and the fields under them.
///
/// `focused` says whether the keys are on the chooser's owner — the answer
/// panel, the form, the overlay. The bracket around the highlighted entry is
/// in the focus role while they are and the chooser has them on its list, in
/// the background role otherwise (R12).
pub fn render(
    frame: &mut Frame,
    area: Rect,
    chooser: &mut Chooser,
    styles: &Styles,
    focused: bool,
) {
    render_with_style(frame, area, chooser, styles, focused, false);
}

/// Draws the chooser as part of the details footer, without nested pane frames.
pub fn render_footer(
    frame: &mut Frame,
    area: Rect,
    chooser: &mut Chooser,
    styles: &Styles,
    focused: bool,
) {
    render_with_style(frame, area, chooser, styles, focused, true);
}

fn render_with_style(
    frame: &mut Frame,
    area: Rect,
    chooser: &mut Chooser,
    styles: &Styles,
    focused: bool,
    footer: bool,
) {
    // The form of inputs shows its fields only when they have the keys.
    let (fields, focus_line) = if chooser.inputs_layout {
        (Vec::new(), 0)
    } else {
        field_lines(chooser, styles, focused, usize::from(area.width))
    };
    let fields_height = (fields.len() as u16).min(area.height.saturating_sub(5));
    let [top, bottom] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(fields_height)]).areas(area);

    let wide = area.width >= SIDE_BY_SIDE_MIN_WIDTH;
    let (list_area, description_area) = if wide {
        let width = list_width(&chooser.items, footer).clamp(20, area.width / 2);
        if footer {
            let [list, divider, description] = Layout::horizontal([
                Constraint::Length(width),
                Constraint::Length(1),
                Constraint::Min(10),
            ])
            .areas(top);
            for y in divider.y..divider.y.saturating_add(divider.height) {
                frame.render_widget(
                    Paragraph::new(Line::styled("│", styles.of(Role::Done))),
                    Rect::new(divider.x, y, 1, 1),
                );
            }
            (list, description)
        } else {
            let [list, description] =
                Layout::horizontal([Constraint::Length(width), Constraint::Min(10)]).areas(top);
            (list, description)
        }
    } else {
        // The description gives way first while the numbered choices remain
        // visible; below that, the list uses all available rows.
        let mut height = list_rows(chooser.items.len(), footer)
            .min(top.height.saturating_sub(3).max(LIST_MIN_ROWS))
            .min(top.height);
        if top.height - height < 3 {
            height = top.height;
        }
        let list = Rect { height, ..top };
        let description = Rect {
            y: top.y + height,
            height: top.height - height,
            ..top
        };
        (list, description)
    };

    let list_focused = focused && chooser.focus == ChooserFocus::Actions;
    render_list(frame, list_area, chooser, styles, list_focused, footer);
    render_description(frame, description_area, chooser, styles, footer);
    if fields_height > 0 {
        let offset = (focus_line + 1).saturating_sub(fields_height as usize) as u16;
        frame.render_widget(Paragraph::new(fields).scroll((offset, 0)), bottom);
    }
}

/// Draws only the fields of the highlighted entry into `area`, scrolled so
/// that the focused one is in view: a text box without the list around it.
pub fn render_fields(
    frame: &mut Frame,
    area: Rect,
    chooser: &Chooser,
    styles: &Styles,
    focused: bool,
) {
    let (fields, focus_line) = field_lines(chooser, styles, focused, usize::from(area.width));
    let height = usize::from(area.height);
    let offset = (focus_line + 1).saturating_sub(height) as u16;
    frame.render_widget(Paragraph::new(fields).scroll((offset, 0)), area);
}

fn pane(title: &str, active: bool, styles: &Styles) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(Styles::FRAME)
        .border_style(styles.of(if active { Role::Focus } else { Role::Done }))
        .title(format!(" {title} "))
}

/// The list pane: the entries stacked, the highlighted one bracketed and
/// scrolled into view inside the pane.
fn render_list(
    frame: &mut Frame,
    area: Rect,
    chooser: &mut Chooser,
    styles: &Styles,
    focused: bool,
    footer: bool,
) {
    let inner = if footer {
        if area.height == 0 {
            return;
        }
        frame.render_widget(
            Paragraph::new(Line::styled("  Akcje", styles.of(Role::Done))),
            Rect::new(area.x, area.y, area.width, 1),
        );
        Rect::new(
            area.x.saturating_add(2),
            area.y + 1,
            area.width.saturating_sub(3),
            area.height - 1,
        )
    } else {
        let block = pane("Akcje", chooser.focus == ChooserFocus::Actions, styles);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        inner
    };
    if !footer {
        let items: Vec<Vec<Line>> = chooser
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| vec![legacy_list_line(item, index, styles)])
            .collect();
        render_bracketed_list(
            frame,
            inner,
            styles,
            &items,
            Some(chooser.selected),
            focused,
            &mut chooser.top,
        );
        return;
    }
    let selected = chooser.selected;
    let height = usize::from(inner.height);
    if selected < chooser.top {
        chooser.top = selected;
    } else if height > 0 && selected >= chooser.top.saturating_add(height) {
        chooser.top = selected + 1 - height;
    }
    let items: Vec<Line> = chooser
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| list_line(item, index, selected, focused, styles))
        .collect();
    frame.render_widget(
        Paragraph::new(items).scroll((chooser.top.min(u16::MAX as usize) as u16, 0)),
        inner,
    );
}

/// The general chooser keeps the established bracket selection and trailing
/// status markers. The task details footer uses the compact pipe treatment.
fn legacy_list_line(item: &ChooserItem, index: usize, styles: &Styles) -> Line<'static> {
    let base = if item.unavailable.is_some() {
        styles.of(Role::Done)
    } else {
        Style::default()
    };
    let number = if index < 9 {
        format!("{} ", index + 1)
    } else {
        "  ".to_string()
    };
    let mut spans = vec![
        Span::styled(number, styles.of(Role::Done)),
        Span::styled(one_line(&item.label), base),
    ];
    for (present, symbol, role) in [
        (item.resumes_agent.is_some(), "↩", Role::Calm),
        (item.recommended, "◆", Role::Conductor),
        (item.irreversible, "!", Role::Problem),
    ] {
        if present {
            spans.push(Span::styled(format!(" {symbol}"), styles.of(role)));
        }
    }
    Line::from(spans)
}

fn render_description(
    frame: &mut Frame,
    area: Rect,
    chooser: &mut Chooser,
    styles: &Styles,
    footer: bool,
) {
    let inner = if footer {
        if area.height == 0 {
            return;
        }
        frame.render_widget(
            Paragraph::new(Line::styled("  Opis", styles.of(Role::Done))),
            Rect::new(area.x, area.y, area.width, 1),
        );
        Rect::new(
            area.x.saturating_add(2),
            area.y + 2,
            area.width.saturating_sub(3),
            area.height.saturating_sub(2),
        )
    } else {
        let block = pane("Opis", false, styles);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        inner
    };
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
    let paragraph = Paragraph::new(lines);
    frame.render_widget(
        if dim {
            paragraph.style(styles.of(Role::Done))
        } else {
            paragraph
        },
        inner,
    );
}
