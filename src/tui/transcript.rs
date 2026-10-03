//! Reusable, terminal-independent presentation state for chronological text.
//!
//! Source adapters own their records and actions. This module only keeps the
//! reading position stable while a conversation grows or changes presentation.

use std::collections::HashSet;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

use super::scroll::ScrollState;

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

/// One source-neutral chronological entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptEntry {
    pub id: TranscriptEntryId,
    /// Semantic lines supplied by the adapter. They are logical, not wrapped
    /// terminal rows.
    pub rendered_lines: Vec<String>,
    /// Optional technical detail included only when expanded in rendered mode.
    pub technical_lines: Vec<String>,
    /// Exact retained source bytes. This is never replaced by decoded text.
    pub raw: Vec<u8>,
}

impl TranscriptEntry {
    pub fn new(id: TranscriptEntryId, rendered_lines: Vec<String>, raw: Vec<u8>) -> Self {
        Self {
            id,
            rendered_lines,
            technical_lines: Vec::new(),
            raw,
        }
    }

    pub fn with_technical_lines(mut self, technical_lines: Vec<String>) -> Self {
        self.technical_lines = technical_lines;
        self
    }
}

/// A logical display line and the stable entry-local location it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptLine {
    pub entry_id: TranscriptEntryId,
    pub logical_line: usize,
    pub text: String,
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
    following_end: bool,
    loaded: bool,
    expanded: HashSet<ExpansionIdentity>,
}

impl TranscriptState {
    pub fn new(conversation: TranscriptId) -> Self {
        Self {
            conversation,
            entries: Vec::new(),
            mode: DisplayMode::Rendered,
            scroll: ScrollState::new(),
            following_end: true,
            loaded: false,
            expanded: HashSet::new(),
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

    /// First successful load starts at the newest end, including an empty
    /// transcript which remains ready to follow its first appended entry.
    pub fn load(&mut self, entries: Vec<TranscriptEntry>) {
        self.entries = entries;
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
        let following = self.following_end;
        self.entries = entries;
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
        self.restore_after_change(anchor, following);
    }

    pub fn set_viewport(&mut self, height: usize) {
        let anchor = self.anchor();
        let following = self.following_end;
        self.scroll.set_viewport(height);
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
        self.scroll.line_down(self.total_lines());
        self.update_following();
    }

    pub fn line_up(&mut self) {
        self.scroll.line_up(self.total_lines());
        self.update_following();
    }

    pub fn page_down(&mut self) {
        self.scroll.page_down(self.total_lines());
        self.update_following();
    }

    pub fn page_up(&mut self) {
        self.scroll.page_up(self.total_lines());
        self.update_following();
    }

    pub fn to_top(&mut self) {
        self.scroll.to_top();
        self.update_following();
    }

    pub fn to_bottom(&mut self) {
        self.scroll.to_bottom(self.total_lines());
        self.following_end = true;
    }

    pub fn logical_lines(&self) -> Vec<TranscriptLine> {
        self.entries
            .iter()
            .flat_map(|entry| self.lines_for(entry))
            .collect()
    }

    pub fn anchor(&self) -> Option<TranscriptAnchor> {
        let lines = self.logical_lines();
        let line = lines.get(self.scroll.offset())?;
        let entry_index = self
            .entries
            .iter()
            .position(|entry| entry.id == line.entry_id)?;
        Some(TranscriptAnchor {
            entry_id: line.entry_id.clone(),
            logical_line: line.logical_line,
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
            .map(|item| self.lines_for(item).len())
            .sum::<usize>();
        self.scroll
            .set_offset(preceding + local_line, self.total_lines());
    }

    fn total_lines(&self) -> usize {
        self.entries
            .iter()
            .map(|entry| self.lines_for(entry).len())
            .sum()
    }

    fn lines_for(&self, entry: &TranscriptEntry) -> Vec<TranscriptLine> {
        let text = match self.mode {
            DisplayMode::Raw => vec![raw_display(&entry.raw)],
            DisplayMode::Rendered => {
                let mut text = entry.rendered_lines.clone();
                if text.is_empty() {
                    text.push(String::new());
                }
                if self.is_expanded(&entry.id) {
                    text.extend(entry.technical_lines.clone());
                }
                text
            }
        };
        text.into_iter()
            .enumerate()
            .map(|(logical_line, text)| TranscriptLine {
                entry_id: entry.id.clone(),
                logical_line,
                text,
            })
            .collect()
    }

    fn expansion_identity(&self, entry: &TranscriptEntryId) -> ExpansionIdentity {
        ExpansionIdentity {
            conversation: self.conversation.clone(),
            entry: entry.clone(),
        }
    }

    fn update_following(&mut self) {
        self.following_end = self.scroll.is_at_bottom(self.total_lines());
    }
}

/// Encodes arbitrary retained bytes as printable terminal-safe text.
///
/// Base64 is reversible and contains no control characters or terminal escape
/// introducers, unlike rendering a lossy decoded string.
pub fn raw_display(raw: &[u8]) -> String {
    format!("base64:{}", BASE64.encode(raw))
}

/// Decodes a value produced by [`raw_display`].
pub fn decode_raw_display(display: &str) -> Result<Vec<u8>, base64::DecodeError> {
    BASE64.decode(display.strip_prefix("base64:").unwrap_or_default())
}
