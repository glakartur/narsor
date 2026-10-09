//! Reusable, terminal-independent presentation state for chronological text.
//!
//! Source adapters own their records and actions. This module only keeps the
//! reading position stable while a conversation grows or changes presentation.

use serde_json::Value;
use std::collections::HashSet;

use super::scroll::{wrap_line, ScrollState};

/// Stable identity of a conversation, normally an attempt or chat id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TranscriptId(String);

impl From<&str> for TranscriptId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for TranscriptId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// Stable identity supplied by a conversation adapter for one entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TranscriptEntryId(String);

impl TranscriptEntryId {
    /// The id as text, for an adapter that has to derive another from it.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for TranscriptEntryId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for TranscriptEntryId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// The reading representation selected by the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayMode {
    Rendered,
    Raw,
}

/// Semantic treatment chosen by a source adapter.  This is presentation data,
/// not terminal control text, so untrusted conversation content never carries
/// its own style instructions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptPresentation {
    Neutral,
    Operator,
    Agent,
    Conductor,
    System,
    Technical,
    Error,
}

impl TranscriptPresentation {
    /// The role named in an utterance header.
    pub fn role_label(self) -> &'static str {
        match self {
            Self::Operator => "USER",
            Self::Agent => "AGENT",
            Self::Conductor => "KONDUKTOR",
            Self::System => "SYSTEM",
            Self::Neutral | Self::Technical | Self::Error => "TECHNICZNE",
        }
    }
}

/// The header of one utterance: its role, who spoke and the full local date
/// and time.
pub fn utterance_header(
    presentation: TranscriptPresentation,
    speaker: &str,
    at: chrono::DateTime<chrono::Utc>,
) -> String {
    format!(
        "{} · {speaker} · {}",
        presentation.role_label(),
        at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S")
    )
}

/// One source-neutral chronological entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptEntry {
    pub id: TranscriptEntryId,
    /// A role-bearing header, rendered separately from the neutral body.
    pub header: String,
    /// Semantic lines supplied by the adapter. They are logical, not wrapped
    /// terminal rows.
    pub rendered_lines: Vec<String>,
    /// Role/type treatment shared by every renderer in rendered mode.
    pub presentation: TranscriptPresentation,
    /// Optional technical detail included only when expanded in rendered mode.
    pub technical_lines: Vec<String>,
    /// Normalized adapter fields, formatted only while this entry is expanded.
    pub details: Option<Value>,
    /// Exact retained source bytes. This is never replaced by decoded text.
    pub raw: Vec<u8>,
}

impl TranscriptEntry {
    pub fn new(id: TranscriptEntryId, rendered_lines: Vec<String>, raw: Vec<u8>) -> Self {
        Self {
            id,
            header: String::new(),
            rendered_lines,
            presentation: TranscriptPresentation::Neutral,
            technical_lines: Vec::new(),
            details: None,
            raw,
        }
    }

    pub fn with_header(mut self, header: impl Into<String>) -> Self {
        self.header = header.into();
        self
    }

    pub fn with_technical_lines(mut self, technical_lines: Vec<String>) -> Self {
        self.technical_lines = technical_lines;
        self
    }

    pub fn with_presentation(mut self, presentation: TranscriptPresentation) -> Self {
        self.presentation = presentation;
        self
    }
}

/// A logical display line and the stable entry-local location it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptLine {
    pub entry_id: TranscriptEntryId,
    pub logical_line: usize,
    pub text: String,
    pub presentation: TranscriptPresentation,
    pub header: bool,
    pub selected: bool,
}

/// Wraps only the visible transcript slice while retaining its semantic
/// presentation.  Keeping this alongside the logical-line model means a
/// narrow pane cannot turn styled entries back into unstyled strings.
pub fn wrap_visible_lines(
    lines: &[TranscriptLine],
    width: usize,
    offset: usize,
    height: usize,
) -> Vec<TranscriptLine> {
    if height == 0 {
        return Vec::new();
    }
    let mut window = Vec::with_capacity(height);
    let mut skipped = 0;
    'lines: for line in lines {
        for (fragment, text) in wrap_line(&line.text, width.max(1)).into_iter().enumerate() {
            if skipped < offset {
                skipped += 1;
                continue;
            }
            if window.len() >= height {
                break 'lines;
            }
            window.push(TranscriptLine {
                entry_id: line.entry_id.clone(),
                logical_line: line.logical_line,
                text,
                presentation: line.presentation,
                header: line.header && fragment == 0,
                // The cursor is a vertical rail for the whole entry. Headers
                // only own their first wrapped fragment, but body fragments
                // all remain visibly selected.
                selected: line.selected && (!line.header || fragment == 0),
            });
        }
    }
    window
}

/// Restorable reading location independent of a wrapped terminal width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptAnchor {
    pub entry_id: TranscriptEntryId,
    pub logical_line: usize,
    entry_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ExpansionIdentity {
    conversation: TranscriptId,
    entry: TranscriptEntryId,
}

/// The state shared by local, remote, and conductor transcript adapters.
#[derive(Debug, Clone)]
pub struct TranscriptState {
    conversation: TranscriptId,
    entries: Vec<TranscriptEntry>,
    mode: DisplayMode,
    scroll: ScrollState,
    width: usize,
    following_end: bool,
    loaded: bool,
    expanded: HashSet<ExpansionIdentity>,
    raw_entries: HashSet<ExpansionIdentity>,
    selected: Option<TranscriptEntryId>,
}

impl TranscriptState {
    pub fn new(conversation: TranscriptId) -> Self {
        Self {
            conversation,
            entries: Vec::new(),
            mode: DisplayMode::Rendered,
            scroll: ScrollState::new(),
            width: 80,
            following_end: true,
            loaded: false,
            expanded: HashSet::new(),
            raw_entries: HashSet::new(),
            selected: None,
        }
    }

    pub fn conversation(&self) -> &TranscriptId {
        &self.conversation
    }

    /// Changes the adapter's active conversation without allowing technical
    /// expansion identities to leak across attempts with restarted numbering.
    pub fn set_conversation(&mut self, conversation: TranscriptId) {
        self.conversation = conversation;
    }

    pub fn mode(&self) -> DisplayMode {
        self.mode
    }

    pub fn scroll_offset(&self) -> usize {
        self.scroll.offset()
    }

    pub fn is_following_end(&self) -> bool {
        self.following_end
    }

    pub fn entries(&self) -> &[TranscriptEntry] {
        &self.entries
    }

    pub fn selected_entry(&self) -> Option<&TranscriptEntryId> {
        self.selected.as_ref()
    }

    /// First successful load starts at the newest end, including an empty
    /// transcript which remains ready to follow its first appended entry.
    pub fn load(&mut self, entries: Vec<TranscriptEntry>) {
        self.entries = entries;
        self.selected = self.entries.last().map(|entry| entry.id.clone());
        self.loaded = true;
        self.following_end = true;
        self.scroll.to_bottom(self.total_lines());
    }

    /// Replaces a refreshed ordered buffer, preserving either end-following or
    /// the nearest stable entry anchor.
    pub fn replace_entries(&mut self, entries: Vec<TranscriptEntry>) {
        if !self.loaded {
            self.load(entries);
            return;
        }
        let anchor = self.anchor();
        let selected = self.selected.clone();
        let following = self.following_end;
        self.entries = entries;
        self.restore_selection(selected);
        self.restore_after_change(anchor, following);
    }

    /// Appends chronological entries using the same refresh preservation rule.
    pub fn append(&mut self, entries: Vec<TranscriptEntry>) {
        if !self.loaded {
            self.load(entries);
            return;
        }
        let anchor = self.anchor();
        let following = self.following_end;
        self.entries.extend(entries);
        if following {
            self.selected = self.entries.last().map(|entry| entry.id.clone());
        }
        self.restore_after_change(anchor, following);
    }

    pub fn set_viewport(&mut self, height: usize) {
        let anchor = self.anchor();
        let following = self.following_end;
        self.scroll.set_viewport(height);
        self.restore_after_change(anchor, following);
    }

    pub fn set_width(&mut self, width: usize) {
        let anchor = self.anchor();
        let following = self.following_end;
        self.width = width.max(1);
        self.restore_after_change(anchor, following);
    }

    pub fn set_mode(&mut self, mode: DisplayMode) {
        if self.mode == mode {
            return;
        }
        let anchor = self.anchor();
        let following = self.following_end;
        self.mode = mode;
        self.restore_after_change(anchor, following);
    }

    /// Switches between the readable projection and exact diagnostic source.
    pub fn toggle_mode(&mut self) {
        self.set_mode(match self.mode {
            DisplayMode::Rendered => DisplayMode::Raw,
            DisplayMode::Raw => DisplayMode::Rendered,
        });
    }

    /// Toggles diagnostic raw data for the selected entry only. Global raw
    /// intentionally wins visually and leaves these flags untouched.
    pub fn toggle_selected_raw(&mut self) {
        if self.mode == DisplayMode::Raw {
            return;
        }
        let Some(entry) = self.selected.clone() else {
            return;
        };
        let anchor = self.anchor();
        let following = self.following_end;
        let identity = self.expansion_identity(&entry);
        if !self.raw_entries.insert(identity.clone()) {
            self.raw_entries.remove(&identity);
        }
        self.restore_after_change(anchor, following);
    }

    pub fn is_raw(&self, entry: &TranscriptEntryId) -> bool {
        self.raw_entries.contains(&self.expansion_identity(entry))
    }

    pub fn toggle_expanded(&mut self, entry: &TranscriptEntryId) {
        let anchor = self.anchor();
        let following = self.following_end;
        let identity = self.expansion_identity(entry);
        if !self.expanded.insert(identity.clone()) {
            self.expanded.remove(&identity);
        }
        self.restore_after_change(anchor, following);
    }

    pub fn is_expanded(&self, entry: &TranscriptEntryId) -> bool {
        self.expanded.contains(&self.expansion_identity(entry))
    }

    pub fn line_down(&mut self) {
        self.move_selected(1);
    }

    pub fn line_up(&mut self) {
        self.move_selected(-1);
    }

    pub fn page_down(&mut self) {
        self.move_selected(self.scroll.viewport() as isize);
    }

    pub fn page_up(&mut self) {
        self.move_selected(-(self.scroll.viewport() as isize));
    }

    pub fn to_top(&mut self) {
        if let Some(entry) = self.entries.first() {
            self.select(entry.id.clone());
        }
    }

    pub fn to_bottom(&mut self) {
        if let Some(entry) = self.entries.last() {
            self.select(entry.id.clone());
        }
        self.following_end = true;
    }

    pub fn logical_lines(&self) -> Vec<TranscriptLine> {
        let mut lines = Vec::new();
        for (index, entry) in self.entries.iter().enumerate() {
            let entry_lines = self.lines_for(entry);
            let next_logical_line = entry_lines.len();
            lines.extend(entry_lines);
            if index + 1 < self.entries.len() {
                lines.push(TranscriptLine {
                    entry_id: entry.id.clone(),
                    logical_line: next_logical_line,
                    text: String::new(),
                    presentation: TranscriptPresentation::Neutral,
                    header: false,
                    selected: false,
                });
            }
        }
        lines
    }

    pub fn anchor(&self) -> Option<TranscriptAnchor> {
        let entry_id = self.selected.as_ref()?;
        let entry_index = self
            .entries
            .iter()
            .position(|entry| entry.id == *entry_id)?;
        Some(TranscriptAnchor {
            entry_id: entry_id.clone(),
            logical_line: 0,
            entry_index,
        })
    }

    fn restore_after_change(&mut self, anchor: Option<TranscriptAnchor>, following: bool) {
        if following {
            self.scroll.to_bottom(self.total_lines());
            self.following_end = true;
        } else {
            self.restore_anchor(anchor);
            // A presentation change can make an older anchor coincide with
            // the end temporarily (for example raw mode has one line per
            // entry). That must not silently opt the reader back into follow.
            self.following_end = false;
        }
    }

    fn restore_selection(&mut self, selected: Option<TranscriptEntryId>) {
        self.selected = selected
            .and_then(|id| {
                self.entries
                    .iter()
                    .position(|entry| entry.id == id)
                    .or_else(|| (!self.entries.is_empty()).then_some(self.entries.len() - 1))
                    .map(|index| self.entries[index].id.clone())
            })
            .or_else(|| self.entries.last().map(|entry| entry.id.clone()));
    }

    fn move_selected(&mut self, delta: isize) {
        let Some(current) = self
            .selected
            .as_ref()
            .and_then(|id| self.entries.iter().position(|entry| &entry.id == id))
        else {
            return;
        };
        let next = (current as isize + delta)
            .clamp(0, self.entries.len().saturating_sub(1) as isize) as usize;
        self.select(self.entries[next].id.clone());
    }

    fn select(&mut self, entry: TranscriptEntryId) {
        self.selected = Some(entry.clone());
        if let Some(index) = self.entries.iter().position(|item| item.id == entry) {
            let preceding = self.entries[..index]
                .iter()
                .map(|item| self.visual_line_count_for(item) + 1)
                .sum();
            self.scroll.set_offset(preceding, self.total_lines());
        }
        self.following_end = self.entries.last().is_some_and(|last| last.id == entry);
    }

    fn restore_anchor(&mut self, anchor: Option<TranscriptAnchor>) {
        let Some(anchor) = anchor else {
            self.scroll.clamp(self.total_lines());
            return;
        };
        if self.entries.is_empty() {
            self.scroll.set_offset(0, 0);
            return;
        }
        let index = self
            .entries
            .iter()
            .position(|entry| entry.id == anchor.entry_id)
            .unwrap_or_else(|| anchor.entry_index.min(self.entries.len() - 1));
        let entry = &self.entries[index];
        let local_line = anchor
            .logical_line
            .min(self.lines_for(entry).len().saturating_sub(1));
        let preceding = self.entries[..index]
            .iter()
            .map(|item| self.visual_line_count_for(item) + 1)
            .sum::<usize>();
        self.scroll
            .set_offset(preceding + local_line, self.total_lines());
    }

    fn total_lines(&self) -> usize {
        self.entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                self.visual_line_count_for(entry) + usize::from(index + 1 < self.entries.len())
            })
            .sum()
    }

    fn visual_line_count_for(&self, entry: &TranscriptEntry) -> usize {
        self.lines_for(entry)
            .iter()
            .map(|line| wrap_line(&line.text, self.width).len())
            .sum()
    }

    fn lines_for(&self, entry: &TranscriptEntry) -> Vec<TranscriptLine> {
        let lines = match self.mode {
            DisplayMode::Raw => {
                vec![(
                    raw_display(&entry.raw),
                    TranscriptPresentation::Neutral,
                    false,
                )]
            }
            DisplayMode::Rendered => {
                if self.is_raw(&entry.id) {
                    vec![(
                        raw_display(&entry.raw),
                        TranscriptPresentation::Neutral,
                        false,
                    )]
                } else {
                    let mut lines = entry
                        .rendered_lines
                        .iter()
                        .cloned()
                        .map(|text| {
                            let presentation = match entry.presentation {
                                TranscriptPresentation::Technical
                                | TranscriptPresentation::System
                                | TranscriptPresentation::Error => entry.presentation,
                                _ => TranscriptPresentation::Neutral,
                            };
                            (text, presentation, false)
                        })
                        .collect::<Vec<_>>();
                    if lines.is_empty() {
                        lines.push((String::new(), TranscriptPresentation::Neutral, false));
                    }
                    if self.is_expanded(&entry.id) {
                        lines.extend(
                            entry
                                .technical_lines
                                .iter()
                                .cloned()
                                .map(|text| (text, TranscriptPresentation::Technical, false)),
                        );
                    }
                    if !entry.header.is_empty() {
                        lines.insert(0, (entry.header.clone(), entry.presentation, true));
                    }
                    lines
                }
            }
        };
        lines
            .into_iter()
            .enumerate()
            .map(
                |(logical_line, (text, presentation, header))| TranscriptLine {
                    entry_id: entry.id.clone(),
                    logical_line,
                    text,
                    presentation,
                    header,
                    selected: self.selected.as_ref() == Some(&entry.id),
                },
            )
            .collect()
    }

    fn expansion_identity(&self, entry: &TranscriptEntryId) -> ExpansionIdentity {
        ExpansionIdentity {
            conversation: self.conversation.clone(),
            entry: entry.clone(),
        }
    }
}

/// Shows retained bytes as readable, terminal-safe text.
///
/// The original bytes remain in [`TranscriptEntry::raw`], while this view
/// decodes them lossily and removes terminal controls for human inspection.
pub fn raw_display(raw: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut shown = String::with_capacity(raw.len());
    for byte in raw {
        match byte {
            b'\n' => shown.push('\n'),
            b'\r' => shown.push_str("\\r"),
            b'\t' => shown.push_str("\\t"),
            0x20..=0x7e if *byte != b'\\' => shown.push(*byte as char),
            b'\\' => shown.push_str("\\\\"),
            byte => {
                shown.push_str("\\x");
                shown.push(HEX[(byte >> 4) as usize] as char);
                shown.push(HEX[(byte & 0x0f) as usize] as char);
            }
        }
    }
    shown
}
