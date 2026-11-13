//! The multi-step form (R11, R17): choose, fill in, confirm.
//!
//! One control for every place that asks the operator to pick something,
//! give what it needs and say yes: the action panel now, the answer to a
//! question of the agent and the new-task form later. It knows nothing of
//! what it is used for: its owner gives it a tree of [`StepEntry`] (a leaf, or
//! a submenu of further entries) and gets back one [`SteppedOutcome`].
//!
//! ```text
//! Akcje › Wybierz krok · krok 1 z 3     Akcje › Wybierz krok › work · krok 2 z 3
//! | 1 | work        │ Opis              komentarz (opcjonalny)
//!   2   lint        │ Wznawia …         > popraw testy
//! ```
//!
//! # The steps
//!
//! 1. **Choice** — the entries of a level on a [`Chooser`]. A submenu opens
//!    the next level in the same place; `Esc` goes one level up, and on the
//!    first level gives the form up ([`SteppedOutcome::Cancelled`]).
//! 2. **Fields** — the fields of the chosen leaf, when it has any, on the
//!    same [`Chooser`] and so on the same form engine ([`super::form`]): the
//!    rules of editing are not repeated here.
//! 3. **Confirmation** — the name, the effect, the risk and what was typed.
//!    `Enter` gives [`SteppedOutcome::Submitted`]; `Esc` goes back a step.
//!
//! A step that is not there (no fields, a leaf that asks no confirmation) is
//! left out, and the indicator counts only the steps that are. Going back
//! never loses what was typed: the fields of a leaf are kept when the
//! operator leaves them, and found again when the leaf is chosen again.
//!
//! [`SteppedForm::rebuilt`] takes a new tree, as when what is offered was
//! read again, and keeps the level, the step and every typed value as long as
//! the chosen entries are still in it. When a chosen entry is gone, the form
//! goes back to the first entry of the last level still there, and takes no
//! key that chooses until the operator moves on the list or goes back.

use std::collections::BTreeMap;

use crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use super::chooser::{self, Chooser, ChooserAction, ChooserItem};
use super::keymap::{self, Action, Context};
use super::scroll::{one_line, sanitize};
use super::style::{Role, Styles};

/// One entry of the tree: a line of a choice step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepEntry {
    /// What the line, the description pane and the fields step show. Its
    /// fields are the fields step of a leaf.
    pub item: ChooserItem,
    /// A leaf, or the entries of a submenu.
    pub kind: EntryKind,
}

/// What choosing an entry does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    /// It is the final choice; `confirm` says whether a confirmation step
    /// follows its fields.
    Leaf {
        /// Whether the operator confirms it.
        confirm: bool,
    },
    /// It opens the next level, with these entries.
    Submenu(Vec<StepEntry>),
}

impl StepEntry {
    /// A leaf that asks for a confirmation.
    pub fn leaf(item: ChooserItem) -> Self {
        Self {
            item,
            kind: EntryKind::Leaf { confirm: true },
        }
    }

    /// A leaf done at once, without a confirmation.
    pub fn immediate(item: ChooserItem) -> Self {
        Self {
            item,
            kind: EntryKind::Leaf { confirm: false },
        }
    }

    /// A submenu of `entries`.
    pub fn submenu(item: ChooserItem, entries: Vec<StepEntry>) -> Self {
        Self {
            item,
            kind: EntryKind::Submenu(entries),
        }
    }
}

/// What the owner of the form does after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteppedOutcome {
    /// Nothing outside the form changed.
    None,
    /// The operator chose, filled in and confirmed this.
    Submitted(SteppedSubmission),
    /// The operator gave the form up on its first level.
    Cancelled,
    /// The entry cannot be chosen; tell the operator why.
    Blocked(String),
}

/// The leaf the operator confirmed, and what was typed for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteppedSubmission {
    /// The index of the entry at every level, from the first.
    pub path: Vec<usize>,
    /// The labels along `path`.
    pub labels: Vec<String>,
    /// The fields with a value, `(name, value)`; a blank optional field is
    /// left out.
    pub values: Vec<(String, String)>,
}

/// The step the form is at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Choosing an entry of a level.
    Choice,
    /// Filling in the fields of the chosen leaf.
    Fields,
    /// Reading what will be done, before it is.
    Confirm,
}

/// One level of choice, open in the form.
#[derive(Debug, Clone)]
struct Level {
    /// The label of the entry that opened it; the first level has the title.
    label: String,
    entries: Vec<StepEntry>,
    chooser: Chooser,
}

impl Level {
    fn new(label: String, entries: Vec<StepEntry>) -> Self {
        let chooser = Chooser::new(choice_items(&entries));
        Self {
            label,
            entries,
            chooser,
        }
    }
}

/// The leaf chosen: its fields with what was typed.
#[derive(Debug, Clone)]
struct ChosenLeaf {
    index: usize,
    item: ChooserItem,
    confirm: bool,
    fields: Chooser,
    /// What the fields step sent on, for the confirmation and the result.
    values: Vec<(String, String)>,
}

/// The state of one multi-step form.
#[derive(Debug, Clone)]
pub struct SteppedForm {
    levels: Vec<Level>,
    stage: Stage,
    leaf: Option<ChosenLeaf>,
    /// The fields of the leaves the operator left, by their label path.
    drafts: BTreeMap<Vec<String>, Chooser>,
    /// A refresh took away what the operator chose, and the form fell back
    /// to a choice they did not make. Until they move on it or go back, a
    /// key that chooses does nothing: the keys meant for the entry that is
    /// gone must not choose another one.
    disarmed: bool,
}

/// The entries of a level as the choice step lists them: without fields,
/// which are the next step's.
fn choice_items(entries: &[StepEntry]) -> Vec<ChooserItem> {
    entries
        .iter()
        .map(|entry| {
            let mut item = entry.item.clone();
            item.fields.clear();
            item
        })
        .collect()
}

impl SteppedForm {
    /// A form called `title` over `entries`, on the first one.
    pub fn new(title: impl Into<String>, entries: Vec<StepEntry>) -> Self {
        Self {
            levels: vec![Level::new(title.into(), entries)],
            stage: Stage::Choice,
            leaf: None,
            drafts: BTreeMap::new(),
            disarmed: false,
        }
    }

    /// The step the form is at.
    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// The labels of the open levels after the first, then the chosen leaf.
    pub fn path_labels(&self) -> Vec<String> {
        let mut labels: Vec<String> = self.levels[1..]
            .iter()
            .map(|level| level.label.clone())
            .collect();
        if let Some(leaf) = &self.leaf {
            labels.push(leaf.item.label.clone());
        }
        labels
    }

    /// The header: the title and the path, joined by `›`.
    pub fn heading(&self) -> String {
        let mut parts = vec![self.levels[0].label.clone()];
        parts.extend(self.path_labels());
        parts
            .iter()
            .map(|part| one_line(part))
            .collect::<Vec<_>>()
            .join(" › ")
    }

    /// Which step of how many the form is at: `(2, 3)` is "krok 2 z 3". The
    /// total is not known while a submenu is highlighted.
    pub fn step_position(&self) -> (usize, Option<usize>) {
        let count = |fields: bool, confirm: bool| 1 + usize::from(fields) + usize::from(confirm);
        match (self.stage, &self.leaf) {
            (Stage::Choice, _) => {
                let level = self.level();
                let total =
                    level
                        .entries
                        .get(level.chooser.selected())
                        .and_then(|entry| match entry.kind {
                            EntryKind::Leaf { confirm } => {
                                Some(count(!entry.item.fields.is_empty(), confirm))
                            }
                            EntryKind::Submenu(_) => None,
                        });
                (1, total)
            }
            (Stage::Fields, Some(leaf)) => (2, Some(count(true, leaf.confirm))),
            (Stage::Confirm, Some(leaf)) => {
                let fields = !leaf.item.fields.is_empty();
                let total = count(fields, true);
                (total, Some(total))
            }
            (_, None) => (1, None),
        }
    }

    /// The step indicator in words: "krok 2 z 3".
    pub fn step_text(&self) -> String {
        match self.step_position() {
            (at, Some(total)) => format!("krok {at} z {total}"),
            (at, None) => format!("krok {at}"),
        }
    }

    /// The chooser of the level open now.
    pub fn chooser(&self) -> &Chooser {
        &self.level().chooser
    }

    /// The chooser of the fields of the chosen leaf, while the form is past
    /// the choice.
    pub fn fields(&self) -> Option<&Chooser> {
        self.leaf.as_ref().map(|leaf| &leaf.fields)
    }

    /// What will be confirmed: the chosen entry, while the form is past the
    /// choice.
    pub fn chosen(&self) -> Option<&ChooserItem> {
        self.leaf.as_ref().map(|leaf| &leaf.item)
    }

    /// What the fields step sent on, for the confirmation.
    pub fn values(&self) -> &[(String, String)] {
        self.leaf
            .as_ref()
            .map_or(&[], |leaf| leaf.values.as_slice())
    }

    fn level(&self) -> &Level {
        self.levels.last().expect("the first level is always open")
    }

    fn level_mut(&mut self) -> &mut Level {
        self.levels
            .last_mut()
            .expect("the first level is always open")
    }

    /// The keys that apply now.
    pub fn context(&self) -> Context {
        match self.stage {
            Stage::Choice => Context::Panel,
            Stage::Fields if self.is_typing() => Context::Form,
            Stage::Fields => Context::Panel,
            Stage::Confirm => Context::FormConfirm,
        }
    }

    /// Whether a refresh took away what the operator chose and the choice
    /// takes no key that chooses yet: the owner must not take its own
    /// shortcuts on the choice either.
    pub fn is_disarmed(&self) -> bool {
        self.disarmed
    }

    /// Whether keys are typed into a field, so the owner must not take `q`
    /// or `?` as commands.
    pub fn is_typing(&self) -> bool {
        self.stage == Stage::Fields && self.fields().is_some_and(Chooser::is_typing)
    }

    /// Whether anything was typed and not sent.
    pub fn has_draft(&self) -> bool {
        self.fields()
            .is_some_and(|fields| !fields.draft_text().is_empty())
            || self
                .drafts
                .values()
                .any(|fields| !fields.draft_text().is_empty())
    }

    /// Folds one key into the form.
    pub fn on_key(&mut self, key: KeyEvent) -> SteppedOutcome {
        match self.stage {
            Stage::Choice => self.on_choice_key(key),
            Stage::Fields => self.on_fields_key(key),
            Stage::Confirm => self.on_confirm_key(&key),
        }
    }

    /// Folds pasted text into the field being typed in; anywhere else it is
    /// ignored. Returns whether a field took it.
    pub fn paste(&mut self, pasted: &str) -> bool {
        match (&mut self.leaf, self.stage) {
            (Some(leaf), Stage::Fields) => leaf.fields.paste(pasted),
            _ => false,
        }
    }

    /// Chooses entry `index` of the level open now, as `Enter` on it does:
    /// for an owner that opens the form on a known entry.
    pub fn open(&mut self, index: usize) -> SteppedOutcome {
        if index >= self.level().entries.len() {
            return SteppedOutcome::None;
        }
        self.disarmed = false;
        self.level_mut().chooser.select(index);
        if let Some(reason) = self.level().entries[index].item.unavailable.clone() {
            return SteppedOutcome::Blocked(reason);
        }
        self.choose(index)
    }

    fn on_choice_key(&mut self, key: KeyEvent) -> SteppedOutcome {
        if self.disarmed {
            match keymap::lookup(Context::Panel, &key) {
                Some(Action::Up | Action::Down | Action::Cancel) => self.disarmed = false,
                Some(Action::PageUp | Action::PageDown) => {}
                _ => return SteppedOutcome::None,
            }
        }
        match self.level_mut().chooser.on_key(key) {
            ChooserAction::None => SteppedOutcome::None,
            ChooserAction::Blocked(reason) => SteppedOutcome::Blocked(reason),
            ChooserAction::Cancel => self.back(),
            ChooserAction::Submit(submission) => self.choose(submission.index),
        }
    }

    /// The operator chose entry `index` of the level open now.
    fn choose(&mut self, index: usize) -> SteppedOutcome {
        let Some(entry) = self.level().entries.get(index).cloned() else {
            return SteppedOutcome::None;
        };
        match entry.kind {
            EntryKind::Submenu(entries) => {
                self.levels.push(Level::new(entry.item.label, entries));
                SteppedOutcome::None
            }
            EntryKind::Leaf { confirm } => {
                let key = self.label_path(&entry.item.label);
                // A kept draft takes the entry as it is now: its options,
                // whether a field is required, why it cannot be chosen.
                let mut fields = self.drafts.remove(&key).map_or_else(
                    || Chooser::new(vec![entry.item.clone()]).enter_fields_on_activate(),
                    |draft| draft.rebuilt(vec![entry.item.clone()]),
                );
                fields.enter_fields();
                let has_fields = !entry.item.fields.is_empty();
                self.leaf = Some(ChosenLeaf {
                    index,
                    item: entry.item,
                    confirm,
                    fields,
                    values: Vec::new(),
                });
                match (has_fields, confirm) {
                    (true, _) => {
                        self.stage = Stage::Fields;
                        SteppedOutcome::None
                    }
                    (false, true) => {
                        self.stage = Stage::Confirm;
                        SteppedOutcome::None
                    }
                    (false, false) => self.submitted(),
                }
            }
        }
    }

    fn on_fields_key(&mut self, key: KeyEvent) -> SteppedOutcome {
        let Some(leaf) = self.leaf.as_mut() else {
            self.stage = Stage::Choice;
            return SteppedOutcome::None;
        };
        match leaf.fields.on_key(key) {
            ChooserAction::None => SteppedOutcome::None,
            ChooserAction::Blocked(reason) => SteppedOutcome::Blocked(reason),
            ChooserAction::Cancel => {
                self.leave_leaf();
                SteppedOutcome::None
            }
            ChooserAction::Submit(submission) => {
                leaf.values = submission.values;
                if leaf.confirm {
                    self.stage = Stage::Confirm;
                    SteppedOutcome::None
                } else {
                    self.submitted()
                }
            }
        }
    }

    /// The keys of the confirmation, and no others: `Enter` confirms, `Esc`
    /// goes back a step. Only the plain keys: `Shift+Enter` is not the
    /// `Enter` of the step.
    fn on_confirm_key(&mut self, key: &KeyEvent) -> SteppedOutcome {
        if !key.modifiers.is_empty() {
            return SteppedOutcome::None;
        }
        match keymap::lookup(Context::FormConfirm, key) {
            Some(Action::Accept) => self.submitted(),
            Some(Action::Cancel) => {
                let has_fields = self
                    .leaf
                    .as_ref()
                    .is_some_and(|leaf| !leaf.item.fields.is_empty());
                if has_fields {
                    if let Some(mut leaf) = self.leaf.take() {
                        self.back_to_fields(&mut leaf);
                        self.leaf = Some(leaf);
                    }
                } else {
                    self.leave_leaf();
                }
                SteppedOutcome::None
            }
            _ => SteppedOutcome::None,
        }
    }

    /// `Esc` on a choice: one level up, or the form given up.
    fn back(&mut self) -> SteppedOutcome {
        if self.levels.len() > 1 {
            self.levels.pop();
            SteppedOutcome::None
        } else {
            SteppedOutcome::Cancelled
        }
    }

    /// Back to the fields of `leaf`, with the keys in them. A leaf without
    /// fields has no such step: it stays where it is.
    fn back_to_fields(&mut self, leaf: &mut ChosenLeaf) {
        if leaf.item.fields.is_empty() {
            return;
        }
        self.stage = Stage::Fields;
        leaf.fields.enter_fields();
    }

    /// Back to the choice; what was typed is kept for the leaf.
    fn leave_leaf(&mut self) {
        self.stage = Stage::Choice;
        if let Some(leaf) = self.leaf.take() {
            let key = self.label_path(&leaf.item.label);
            self.keep_draft(key, leaf.fields);
        }
    }

    /// Keeps what was typed in `fields` for the leaf at the label path `key`.
    fn keep_draft(&mut self, key: Vec<String>, mut fields: Chooser) {
        fields.leave_fields();
        self.drafts.insert(key, fields);
    }

    fn submitted(&mut self) -> SteppedOutcome {
        let Some(leaf) = self.leaf.as_ref() else {
            return SteppedOutcome::None;
        };
        let mut path: Vec<usize> = self
            .levels
            .iter()
            .take(self.levels.len() - 1)
            .map(|level| level.chooser.selected())
            .collect();
        path.push(leaf.index);
        let mut labels: Vec<String> = self.levels[1..]
            .iter()
            .map(|level| level.label.clone())
            .collect();
        labels.push(leaf.item.label.clone());
        SteppedOutcome::Submitted(SteppedSubmission {
            path,
            labels,
            values: leaf.values.clone(),
        })
    }

    /// The labels of the open levels after the first, then `label`.
    fn label_path(&self, label: &str) -> Vec<String> {
        let mut path: Vec<String> = self.levels[1..]
            .iter()
            .map(|level| level.label.clone())
            .collect();
        path.push(label.to_string());
        path
    }

    /// The same form over a new tree. The open levels, the step and what was
    /// typed stay as long as the chosen entries (by label) are still there.
    /// Otherwise the form goes back to the last level that is, and the label
    /// of the entry that is gone comes back, for the operator.
    pub fn rebuilt(mut self, entries: Vec<StepEntry>) -> (Self, Option<String>) {
        let mut lost = None;
        // The key of what was typed in the open leaf, from the levels before
        // the rebuild: the leaf can be gone after it.
        let leaf_key = self
            .leaf
            .as_ref()
            .map(|leaf| self.label_path(&leaf.item.label));
        let old_levels = std::mem::take(&mut self.levels);
        let mut entries = entries;
        let mut levels = Vec::with_capacity(old_levels.len());
        for (depth, old) in old_levels.into_iter().enumerate() {
            if depth > 0 {
                // This level was opened by the entry `old.label` of the one
                // before; it must still be a submenu there.
                let parent: &Level = levels.last().expect("the level before");
                match parent
                    .entries
                    .iter()
                    .find(|entry: &&StepEntry| entry.item.label == old.label)
                    .map(|entry| entry.kind.clone())
                {
                    Some(EntryKind::Submenu(children)) => entries = children,
                    _ => {
                        lost = Some(old.label);
                        break;
                    }
                }
            }
            let chooser = old.chooser.rebuilt(choice_items(&entries));
            levels.push(Level {
                label: old.label,
                entries: entries.clone(),
                chooser,
            });
        }
        self.levels = levels;
        let level_lost = lost.is_some();
        if let Some(mut leaf) = self.leaf.take() {
            let found = (!level_lost)
                .then(|| {
                    self.level()
                        .entries
                        .iter()
                        .position(|entry| entry.item.label == leaf.item.label)
                })
                .flatten();
            match found {
                Some(index) => {
                    let entry = self.level().entries[index].clone();
                    if let EntryKind::Leaf { confirm } = entry.kind {
                        // What was sent on from the fields was checked against
                        // the fields as they were; other fields ask for it again.
                        let fields_changed = !leaf.item.same_fields(&entry.item);
                        leaf.index = index;
                        leaf.confirm = confirm;
                        leaf.fields = leaf.fields.rebuilt(vec![entry.item.clone()]);
                        leaf.item = entry.item;
                        if fields_changed {
                            leaf.values.clear();
                            if self.stage == Stage::Confirm {
                                self.back_to_fields(&mut leaf);
                            }
                        }
                        self.level_mut().chooser.select(index);
                        self.leaf = Some(leaf);
                    } else {
                        lost = Some(leaf.item.label);
                        self.stage = Stage::Choice;
                        self.keep_draft(leaf_key.expect("the key of the leaf"), leaf.fields);
                    }
                }
                None => {
                    if lost.is_none() {
                        lost = Some(leaf.item.label.clone());
                    }
                    self.stage = Stage::Choice;
                    self.keep_draft(leaf_key.expect("the key of the leaf"), leaf.fields);
                }
            }
        } else {
            self.stage = Stage::Choice;
        }
        if lost.is_some() {
            // The highlight would be where the gone entry was, on another
            // entry: the operator starts again from the top.
            self.level_mut().chooser.select(0);
            self.disarmed = true;
        }
        (self, lost)
    }

    /// The rows the form would like for an area `width` columns wide.
    pub fn preferred_height(&self, width: u16) -> u16 {
        let body = match self.stage {
            Stage::Choice => self.chooser().preferred_height(width),
            Stage::Fields => {
                1 + self
                    .fields()
                    .map_or(1, |fields| fields.field_rows_in(usize::from(width)).max(1))
            }
            Stage::Confirm => {
                let lines = confirm_lines(self, &Styles::new(false), width);
                u16::try_from(lines.len()).unwrap_or(u16::MAX)
            }
        };
        body.saturating_add(HEADING_ROWS)
    }
}

/// The rows of the heading: the path and the step, then a gap.
const HEADING_ROWS: u16 = 2;

/// Draws `form` into `area`: the heading with the path and the step, then
/// the step itself. `focused` says whether its owner has the keys.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    form: &mut SteppedForm,
    styles: &Styles,
    focused: bool,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let role = if focused { Role::Focus } else { Role::Done };
    let heading = Line::from(vec![
        Span::styled(
            one_line(&form.heading()),
            styles.of(role).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" · {}", form.step_text()), styles.of(Role::Done)),
    ]);
    frame.render_widget(
        Paragraph::new(heading),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let body = Rect::new(
        area.x,
        area.y.saturating_add(HEADING_ROWS),
        area.width,
        area.height.saturating_sub(HEADING_ROWS),
    );
    if body.height == 0 {
        return;
    }
    match form.stage {
        Stage::Choice => {
            let chooser = &mut form.level_mut().chooser;
            chooser::render_footer(frame, body, chooser, styles, focused);
        }
        Stage::Fields => {
            let Some(leaf) = form.leaf.as_ref() else {
                return;
            };
            let description = leaf
                .item
                .description
                .lines()
                .next()
                .map(one_line)
                .unwrap_or_default();
            frame.render_widget(
                Paragraph::new(Line::styled(description, styles.of(Role::Done))),
                Rect::new(body.x, body.y, body.width, 1),
            );
            let fields = Rect::new(
                body.x,
                body.y.saturating_add(1),
                body.width,
                body.height.saturating_sub(1),
            );
            chooser::render_fields(frame, fields, &leaf.fields, styles, focused);
        }
        Stage::Confirm => {
            let lines = confirm_lines(form, styles, body.width);
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
        }
    }
}

/// The lines of the confirmation step: what will be done, its risk, what was
/// typed and the keys.
fn confirm_lines(form: &SteppedForm, styles: &Styles, width: u16) -> Vec<Line<'static>> {
    let Some(item) = form.chosen() else {
        return Vec::new();
    };
    let width = usize::from(width).max(1);
    let mut lines = vec![Line::styled(
        one_line(&item.label),
        styles.of(Role::Focus).add_modifier(Modifier::BOLD),
    )];
    for text in sanitize(&item.description).lines() {
        for row in textwrap_rows(text, width) {
            lines.push(Line::styled(row, styles.of(Role::Calm)));
        }
    }
    if item.irreversible {
        lines.push(Line::styled(
            "! nieodwracalne",
            styles.of(Role::Problem).add_modifier(Modifier::BOLD),
        ));
    }
    for (name, value) in form.values() {
        let text = format!("{}: {}", one_line(name), one_line(value));
        for row in textwrap_rows(&text, width) {
            lines.push(Line::raw(row));
        }
    }
    lines.push(Line::raw(""));
    let mut keys = Vec::new();
    for (key, label) in keymap::hints(Context::FormConfirm).into_iter().take(2) {
        keys.push(Span::styled(format!(" {key} "), styles.key_cap()));
        keys.push(Span::raw(format!("{label} ")));
    }
    lines.push(Line::from(keys));
    lines
}

/// `text` cut into rows of at most `width` characters.
fn textwrap_rows(text: &str, width: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars
        .chunks(width)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::chooser::ChooserField;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn typed(form: &mut SteppedForm, text: &str) {
        for character in text.chars() {
            assert_eq!(
                form.on_key(key(KeyCode::Char(character))),
                SteppedOutcome::None
            );
        }
    }

    /// "Akcje": a pause (a leaf), a choice of a step (a submenu: an agent's
    /// step with an optional comment, a script) and a link done at once.
    fn tree() -> Vec<StepEntry> {
        vec![
            StepEntry::leaf(ChooserItem::new("Pauza", "Czeka na koniec kroku.")),
            StepEntry::submenu(
                ChooserItem::new("Wybierz krok", "Wznawia od kroku."),
                vec![
                    StepEntry::leaf(
                        ChooserItem::new("work", "Krok agenta.")
                            .with_field(ChooserField::multiline("komentarz")),
                    ),
                    StepEntry::leaf(ChooserItem::new("lint", "Skrypt.")),
                ],
            ),
            StepEntry::leaf(
                ChooserItem::new("Override", "Wymusza przejście.")
                    .irreversible()
                    .with_field(ChooserField::text("powód").required()),
            ),
            StepEntry::immediate(ChooserItem::new("Otwórz link", "Otwiera PR.")),
        ]
    }

    fn form() -> SteppedForm {
        SteppedForm::new("Akcje", tree())
    }

    fn submitted(outcome: SteppedOutcome) -> SteppedSubmission {
        match outcome {
            SteppedOutcome::Submitted(submission) => submission,
            other => panic!("a submission was expected, got {other:?}"),
        }
    }

    #[test]
    fn three_steps_go_on_with_enter_and_end_in_one_submission() {
        let mut form = form();
        form.on_key(key(KeyCode::Char('2')));
        assert_eq!(form.on_key(key(KeyCode::Enter)), SteppedOutcome::None);
        assert_eq!(form.heading(), "Akcje › Wybierz krok");
        assert_eq!(form.step_text(), "krok 1 z 3");

        assert_eq!(form.on_key(key(KeyCode::Enter)), SteppedOutcome::None);
        assert_eq!(form.stage(), Stage::Fields);
        assert_eq!(form.step_text(), "krok 2 z 3");
        typed(&mut form, "popraw");
        assert_eq!(form.on_key(key(KeyCode::Enter)), SteppedOutcome::None);
        assert_eq!(form.stage(), Stage::Confirm);
        assert_eq!(form.step_text(), "krok 3 z 3");

        let submission = submitted(form.on_key(key(KeyCode::Enter)));
        assert_eq!(submission.path, [1, 0]);
        assert_eq!(submission.labels, ["Wybierz krok", "work"]);
        assert_eq!(
            submission.values,
            [("komentarz".to_string(), "popraw".to_string())]
        );
    }

    #[test]
    fn esc_from_the_confirmation_keeps_what_was_typed() {
        // Covers AE3.
        let mut form = form();
        form.on_key(key(KeyCode::Char('2')));
        form.on_key(key(KeyCode::Enter));
        form.on_key(key(KeyCode::Enter));
        typed(&mut form, "popraw");
        form.on_key(key(KeyCode::Enter));
        assert_eq!(form.stage(), Stage::Confirm);

        form.on_key(key(KeyCode::Esc));
        assert_eq!(form.stage(), Stage::Fields);
        assert_eq!(form.fields().unwrap().fields_of(0)[0].value(), "popraw");

        // Back to the choice and into the same step again: still there.
        form.on_key(key(KeyCode::Esc));
        assert_eq!(form.stage(), Stage::Choice);
        form.on_key(key(KeyCode::Enter));
        assert_eq!(form.stage(), Stage::Fields);
        assert_eq!(form.fields().unwrap().fields_of(0)[0].value(), "popraw");
        assert!(form.has_draft());
    }

    #[test]
    fn only_enter_of_the_confirmation_submits() {
        // Covers AE4.
        let mut form = form();
        assert_eq!(form.on_key(key(KeyCode::Enter)), SteppedOutcome::None);
        assert_eq!(form.stage(), Stage::Confirm);
        assert_eq!(form.step_text(), "krok 2 z 2", "no fields step");
        assert_eq!(
            form.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            SteppedOutcome::None
        );
        assert_eq!(form.on_key(key(KeyCode::Char('y'))), SteppedOutcome::None);
        assert_eq!(form.on_key(key(KeyCode::Esc)), SteppedOutcome::None);
        assert_eq!(form.stage(), Stage::Choice);

        form.on_key(key(KeyCode::Enter));
        let submission = submitted(form.on_key(key(KeyCode::Enter)));
        assert_eq!(submission.path, [0]);
        assert!(submission.values.is_empty());
    }

    #[test]
    fn a_required_field_left_blank_does_not_go_on() {
        let mut form = form();
        form.on_key(key(KeyCode::Char('3')));
        form.on_key(key(KeyCode::Enter));
        assert_eq!(form.stage(), Stage::Fields);
        assert_eq!(form.on_key(key(KeyCode::Enter)), SteppedOutcome::None);
        assert_eq!(form.stage(), Stage::Fields);
        assert!(form.fields().unwrap().fields_of(0)[0].error().is_some());
    }

    #[test]
    fn esc_goes_one_level_up_and_gives_the_form_up_on_the_first() {
        let mut form = form();
        form.on_key(key(KeyCode::Char('2')));
        form.on_key(key(KeyCode::Enter));
        assert_eq!(form.heading(), "Akcje › Wybierz krok");
        assert_eq!(form.on_key(key(KeyCode::Esc)), SteppedOutcome::None);
        assert_eq!(form.heading(), "Akcje");
        assert_eq!(
            form.chooser().selected(),
            1,
            "the submenu stays highlighted"
        );
        assert_eq!(form.on_key(key(KeyCode::Esc)), SteppedOutcome::Cancelled);
    }

    #[test]
    fn a_leaf_without_a_confirmation_is_done_at_once() {
        let mut form = form();
        form.on_key(key(KeyCode::Char('4')));
        assert_eq!(form.step_text(), "krok 1 z 1");
        let submission = submitted(form.on_key(key(KeyCode::Enter)));
        assert_eq!(submission.path, [3]);
    }

    #[test]
    fn the_confirmation_of_a_risky_entry_shows_the_mark_of_risk() {
        let mut form = form();
        form.on_key(key(KeyCode::Char('3')));
        form.on_key(key(KeyCode::Enter));
        typed(&mut form, "pilne");
        form.on_key(key(KeyCode::Enter));
        assert_eq!(form.stage(), Stage::Confirm);
        let text: Vec<String> = confirm_lines(&form, &Styles::new(false), 60)
            .iter()
            .map(|line| line.to_string())
            .collect();
        assert!(text.contains(&"! nieodwracalne".to_string()), "{text:?}");
        assert!(text.contains(&"powód: pilne".to_string()), "{text:?}");
    }

    #[test]
    fn a_paste_goes_into_the_field_with_its_lines() {
        let mut form = form();
        form.on_key(key(KeyCode::Char('2')));
        form.on_key(key(KeyCode::Enter));
        assert!(!form.paste("ignored"), "no field on a choice");
        form.on_key(key(KeyCode::Enter));
        assert!(form.paste("one\ntwo"));
        assert_eq!(form.fields().unwrap().fields_of(0)[0].value(), "one\ntwo");
    }

    #[test]
    fn a_rebuild_keeps_the_step_and_the_values() {
        // Covers AE7.
        let mut form = form();
        form.on_key(key(KeyCode::Char('2')));
        form.on_key(key(KeyCode::Enter));
        form.on_key(key(KeyCode::Enter));
        typed(&mut form, "popraw");

        let (mut form, lost) = form.rebuilt(tree());
        assert_eq!(lost, None);
        assert_eq!(form.stage(), Stage::Fields);
        assert!(form.is_typing());
        typed(&mut form, " to");
        assert_eq!(form.fields().unwrap().fields_of(0)[0].value(), "popraw to");
    }

    #[test]
    fn a_rebuild_without_the_chosen_entry_goes_back_a_level() {
        let mut form = form();
        form.on_key(key(KeyCode::Char('2')));
        form.on_key(key(KeyCode::Enter));
        form.on_key(key(KeyCode::Enter));
        typed(&mut form, "popraw");

        let mut smaller = tree();
        smaller.remove(1);
        let (form, lost) = form.rebuilt(smaller);
        assert_eq!(lost.as_deref(), Some("Wybierz krok"));
        assert_eq!(form.stage(), Stage::Choice);
        assert_eq!(form.heading(), "Akcje");
    }

    #[test]
    fn keys_meant_for_a_lost_leaf_choose_nothing_until_the_operator_moves() {
        // "Override" is open; the refresh takes it, and the highlight would
        // be on "Otwórz link", done at once.
        let mut form = form();
        form.on_key(key(KeyCode::Char('3')));
        form.on_key(key(KeyCode::Enter));
        assert_eq!(form.stage(), Stage::Fields);

        let mut smaller = tree();
        smaller.remove(2);
        let (mut form, lost) = form.rebuilt(smaller);
        assert_eq!(lost.as_deref(), Some("Override"));
        assert_eq!(form.chooser().selected(), 0, "back to the top");
        assert!(form.is_disarmed());
        for code in [
            KeyCode::Enter,
            KeyCode::Char('3'),
            KeyCode::Tab,
            KeyCode::Enter,
        ] {
            assert_eq!(form.on_key(key(code)), SteppedOutcome::None, "{code:?}");
        }
        assert_eq!(form.stage(), Stage::Choice);
        assert_eq!(form.chooser().selected(), 0);

        // A move on the list: the keys choose again.
        form.on_key(key(KeyCode::Down));
        assert!(!form.is_disarmed());
        assert_eq!(form.chooser().selected(), 1);
        form.on_key(key(KeyCode::Char('3')));
        let submission = submitted(form.on_key(key(KeyCode::Enter)));
        assert_eq!(submission.labels, ["Otwórz link"]);
    }

    #[test]
    fn esc_after_a_lost_submenu_gives_the_form_up_as_always() {
        let mut form = form();
        form.on_key(key(KeyCode::Char('2')));
        form.on_key(key(KeyCode::Enter));
        let mut smaller = tree();
        smaller.remove(1);
        let (mut form, lost) = form.rebuilt(smaller);
        assert_eq!(lost.as_deref(), Some("Wybierz krok"));
        assert!(form.is_disarmed());
        assert_eq!(form.on_key(key(KeyCode::Esc)), SteppedOutcome::Cancelled);
    }

    #[test]
    fn a_rebuild_that_loses_nothing_leaves_the_keys_as_they_are() {
        let mut form = form();
        form.on_key(key(KeyCode::Char('2')));
        let (mut form, lost) = form.rebuilt(tree());
        assert_eq!(lost, None);
        assert!(!form.is_disarmed());
        assert_eq!(form.chooser().selected(), 1);
        assert_eq!(form.on_key(key(KeyCode::Enter)), SteppedOutcome::None);
        assert_eq!(form.heading(), "Akcje › Wybierz krok");
    }

    #[test]
    fn what_was_typed_comes_back_when_a_lost_entry_comes_back() {
        let mut form = form();
        form.on_key(key(KeyCode::Char('3')));
        form.on_key(key(KeyCode::Enter));
        assert_eq!(form.stage(), Stage::Fields);
        typed(&mut form, "pilne");

        let mut smaller = tree();
        smaller.remove(2);
        let (form, lost) = form.rebuilt(smaller);
        assert_eq!(lost.as_deref(), Some("Override"));
        assert_eq!(form.stage(), Stage::Choice);

        let (mut form, lost) = form.rebuilt(tree());
        assert_eq!(lost, None);
        assert_eq!(form.open(2), SteppedOutcome::None);
        assert_eq!(form.stage(), Stage::Fields);
        assert_eq!(form.fields().unwrap().fields_of(0)[0].value(), "pilne");
    }

    /// `tree()` with the fields of "Override" put in place of its own.
    fn tree_with_override_fields(fields: Vec<ChooserField>) -> Vec<StepEntry> {
        let mut entries = tree();
        let mut item = ChooserItem::new("Override", "Wymusza przejście.").irreversible();
        for field in fields {
            item = item.with_field(field);
        }
        entries[2] = StepEntry::leaf(item);
        entries
    }

    /// The form at the confirmation of "Override", with `reason` typed.
    fn confirming_override(reason: &str) -> SteppedForm {
        let mut form = form();
        assert_eq!(form.open(2), SteppedOutcome::None);
        typed(&mut form, reason);
        form.on_key(key(KeyCode::Enter));
        assert_eq!(form.stage(), Stage::Confirm);
        form
    }

    #[test]
    fn a_rebuild_with_the_same_fields_keeps_the_confirmation() {
        let form = confirming_override("pilne");

        let (form, lost) = form.rebuilt(tree());
        assert_eq!(lost, None);
        assert_eq!(form.stage(), Stage::Confirm);
        assert_eq!(form.values(), [("powód".to_string(), "pilne".to_string())]);
    }

    #[test]
    fn a_rebuild_with_other_fields_asks_for_them_again() {
        let form = confirming_override("pilne");

        let changed = tree_with_override_fields(vec![ChooserField::select(
            "powód",
            vec!["awaria".to_string()],
        )]);
        let (mut form, lost) = form.rebuilt(changed);
        assert_eq!(lost, None);
        assert_eq!(form.stage(), Stage::Fields);
        assert!(form.values().is_empty(), "the old values are not sent");
        // Enter does not send what the new fields no longer allow.
        if let SteppedOutcome::Submitted(submission) = form.on_key(key(KeyCode::Enter)) {
            assert!(
                !submission
                    .values
                    .contains(&("powód".to_string(), "pilne".to_string())),
                "{submission:?}"
            );
        }
    }

    #[test]
    fn a_kept_draft_takes_the_entry_as_it_is_now() {
        let mut form = form();
        assert_eq!(form.open(2), SteppedOutcome::None);
        typed(&mut form, "pilne");
        form.on_key(key(KeyCode::Esc));
        assert_eq!(form.stage(), Stage::Choice);

        let changed = tree_with_override_fields(vec![
            ChooserField::text("powód"),
            ChooserField::text("uwagi"),
        ]);
        let (mut form, lost) = form.rebuilt(changed);
        assert_eq!(lost, None);
        assert_eq!(form.open(2), SteppedOutcome::None);
        let fields = form.fields().unwrap().fields_of(0);
        assert_eq!(fields.len(), 2, "the field the server added is there");
        assert_eq!(fields[0].value(), "pilne", "what was typed is kept");
        assert!(!fields[0].required, "the field is no longer required");
    }
}
