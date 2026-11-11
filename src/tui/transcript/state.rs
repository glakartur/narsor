use std::collections::{HashMap, HashSet};

use super::entry::{
    DisplayMode, TranscriptEntry, TranscriptEntryId, TranscriptId, TranscriptPresentation,
};
use super::grouping::ExpansionIdentity;
use super::line::{fragments, raw_display, TranscriptLine};
use crate::tui::scroll::ScrollState;

/// Restorable reading location independent of a wrapped terminal width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptAnchor {
    pub entry_id: TranscriptEntryId,
    pub logical_line: usize,
    entry_index: usize,
}

/// The state shared by local, remote, and conductor transcript adapters.
#[derive(Debug, Clone)]
pub struct TranscriptState {
    pub(super) conversation: TranscriptId,
    pub(super) entries: Vec<TranscriptEntry>,
    mode: DisplayMode,
    scroll: ScrollState,
    pub(super) width: usize,
    following_end: bool,
    loaded: bool,
    pub(super) expanded: HashSet<ExpansionIdentity>,
    pub(super) raw_entries: HashSet<ExpansionIdentity>,
    /// A group is a view over an append-only history. Once an operator opens
    /// it (or asks for its source), retain that exact member set: later events
    /// must not appear inside an already-open card.
    pub(super) revealed_members: HashMap<ExpansionIdentity, HashSet<TranscriptEntryId>>,
    selected: Option<TranscriptEntryId>,
    selected_source: Option<TranscriptEntryId>,
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
            revealed_members: HashMap::new(),
            selected: None,
            selected_source: None,
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
        self.selected = self
            .projected_entries()
            .last()
            .map(|entry| entry.id.clone());
        self.selected_source = self.entries.last().map(|entry| entry.id.clone());
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
        let selected = self
            .selected_source
            .clone()
            .or_else(|| self.selected.clone());
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
            self.selected = self
                .projected_entries()
                .last()
                .map(|entry| entry.id.clone());
            self.selected_source = self
                .projected_entries()
                .last()
                .and_then(|entry| entry.source_ids.last().cloned());
        }
        self.restore_after_change(anchor, following);
    }

    pub fn set_viewport(&mut self, height: usize) {
        if self.scroll.viewport() == height {
            return;
        }
        let anchor = self.anchor();
        let following = self.following_end;
        self.scroll.set_viewport(height);
        self.restore_after_change(anchor, following);
    }

    pub fn set_width(&mut self, width: usize) {
        let width = width.max(1);
        if self.width == width {
            return;
        }
        let anchor = self.anchor();
        let following = self.following_end;
        self.width = width;
        self.restore_after_change(anchor, following);
    }

    pub fn set_mode(&mut self, mode: DisplayMode) {
        if self.mode == mode {
            return;
        }
        let anchor = self.anchor();
        let following = false;
        let source = self
            .selected_source
            .clone()
            .or_else(|| self.selected.clone());
        self.mode = mode;
        self.restore_selection(source);
        self.restore_after_change(anchor, following);
    }

    /// Switches between the readable projection and exact diagnostic source.
    pub fn toggle_mode(&mut self) {
        self.set_mode(match self.mode {
            DisplayMode::Rendered => DisplayMode::Raw,
            DisplayMode::Chronology | DisplayMode::Raw => DisplayMode::Rendered,
        });
    }

    /// Advances the three agent-conversation projections without changing
    /// generic readers, which still use [`Self::toggle_mode`].
    pub fn next_conversation_view(&mut self) {
        self.set_mode(match self.mode {
            DisplayMode::Rendered => DisplayMode::Chronology,
            DisplayMode::Chronology => DisplayMode::Raw,
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
        let following = false;
        let identity = self.expansion_identity(&entry);
        if !self.raw_entries.insert(identity.clone()) {
            self.raw_entries.remove(&identity);
            self.forget_revealed_members_if_hidden(&identity);
        } else {
            self.remember_revealed_members(&entry, &identity);
        }
        self.restore_after_change(anchor, following);
    }

    pub fn is_raw(&self, entry: &TranscriptEntryId) -> bool {
        if self.raw_entries.contains(&self.expansion_identity(entry)) {
            return true;
        }
        if let Some(group_identity) = self.group_identity_for(entry) {
            if self
                .revealed_members
                .get(&group_identity)
                .is_some_and(|snapshot| !snapshot.contains(entry))
            {
                return false;
            }
        }
        let Some(index) = self.entries.iter().position(|source| source.id == *entry) else {
            return false;
        };
        let Some(grouping) = self.entries[index].grouping.as_ref() else {
            return false;
        };
        let same_group = |source: &TranscriptEntry| {
            source.grouping.as_ref().is_some_and(|source_grouping| {
                source_grouping.session_id == grouping.session_id
                    && if grouping.operation {
                        source_grouping.operation
                            && source_grouping.correlation_id == grouping.correlation_id
                    } else {
                        !source_grouping.operation && source_grouping.name == grouping.name
                    }
            })
        };
        let start = (0..index)
            .rev()
            .take_while(|&candidate| same_group(&self.entries[candidate]))
            .last()
            .unwrap_or(index);
        (start..self.entries.len())
            .take_while(|&candidate| same_group(&self.entries[candidate]))
            .any(|candidate| {
                self.raw_entries
                    .contains(&self.expansion_identity(&self.entries[candidate].id))
            })
    }

    pub fn toggle_expanded(&mut self, entry: &TranscriptEntryId) {
        let anchor = self.anchor();
        let following = false;
        let identity = self.expansion_identity(entry);
        if !self.expanded.insert(identity.clone()) {
            self.expanded.remove(&identity);
            self.forget_revealed_members_if_hidden(&identity);
        } else {
            self.remember_revealed_members(entry, &identity);
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
        self.scroll.page_down(self.total_lines());
        self.following_end = false;
        self.select_at_scroll();
    }

    pub fn page_up(&mut self) {
        self.scroll.page_up(self.total_lines());
        self.following_end = false;
        self.select_at_scroll();
    }

    pub fn to_top(&mut self) {
        if let Some(entry) = self.projected_entries().first() {
            self.select(entry.id.clone());
        }
    }

    pub fn to_bottom(&mut self) {
        if let Some(entry) = self.projected_entries().last() {
            self.select(entry.id.clone());
        }
        self.scroll.to_bottom(self.total_lines());
        self.following_end = false;
    }

    /// `f`: turn end-following on, or pause it while leaving the reading
    /// position where it is.
    pub fn toggle_follow(&mut self) {
        if self.following_end {
            self.following_end = false;
        } else {
            self.scroll.to_bottom(self.total_lines());
            if let Some(entry) = self.projected_entries().last() {
                self.selected = Some(entry.id.clone());
                self.selected_source = entry.source_ids.last().cloned();
            }
            self.following_end = true;
        }
    }

    pub fn logical_lines(&self) -> Vec<TranscriptLine> {
        let mut lines = Vec::new();
        let entries = self.projected_entries();
        for (index, entry) in entries.iter().enumerate() {
            let entry_lines = self.lines_for(entry);
            let next_logical_line = entry_lines.len();
            lines.extend(entry_lines);
            if index + 1 < entries.len() {
                lines.push(TranscriptLine {
                    entry_id: entry.id.clone(),
                    logical_line: next_logical_line,
                    text: String::new(),
                    presentation: TranscriptPresentation::Neutral,
                    header: false,
                    indented: false,
                    nested: false,
                    selected: false,
                    workflow_choice: false,
                    clipped: false,
                });
            }
        }
        lines
    }

    pub fn anchor(&self) -> Option<TranscriptAnchor> {
        let entries = self.projected_entries();
        let mut offset = self.scroll.offset();
        for (entry_index, entry) in entries.iter().enumerate() {
            let visual_lines = self.visual_line_count_for(entry);
            if offset < visual_lines {
                return Some(TranscriptAnchor {
                    entry_id: entry.id.clone(),
                    logical_line: offset,
                    entry_index,
                });
            }
            offset = offset.saturating_sub(visual_lines);
            if entry_index + 1 < entries.len() {
                if offset == 0 {
                    return Some(TranscriptAnchor {
                        entry_id: entry.id.clone(),
                        logical_line: visual_lines.saturating_sub(1),
                        entry_index,
                    });
                }
                offset = offset.saturating_sub(1);
            }
        }
        None
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
        let entries = self.projected_entries();
        self.selected = selected
            .clone()
            .and_then(|id| Self::projected_id_for(&entries, &id))
            .or_else(|| entries.last().map(|entry| entry.id.clone()));
        self.selected_source = selected.or_else(|| self.selected.clone());
    }

    fn move_selected(&mut self, delta: isize) {
        let entries = self.projected_entries();
        let Some(current) = self
            .selected
            .as_ref()
            .and_then(|id| entries.iter().position(|entry| &entry.id == id))
        else {
            return;
        };
        let next =
            (current as isize + delta).clamp(0, entries.len().saturating_sub(1) as isize) as usize;
        self.select(entries[next].id.clone());
    }

    /// Keeps the cursor on a visible entry after paging, without moving the
    /// viewport back to the entry's first line.
    fn select_at_scroll(&mut self) {
        let mut offset = self.scroll.offset();
        for entry in self.projected_entries() {
            let lines = self.visual_line_count_for(&entry);
            if offset < lines {
                self.selected = Some(entry.id.clone());
                self.selected_source = entry.source_ids.first().cloned();
                return;
            }
            offset = offset.saturating_sub(lines.saturating_add(1));
        }
    }

    fn select(&mut self, entry: TranscriptEntryId) {
        let entries = self.projected_entries();
        let source = entries
            .iter()
            .find(|item| item.id == entry)
            .and_then(|item| {
                self.selected_source
                    .as_ref()
                    .filter(|source| item.source_ids.contains(source))
                    .cloned()
                    .or_else(|| item.source_ids.first().cloned())
            })
            .unwrap_or_else(|| entry.clone());
        self.selected = Some(entry.clone());
        self.selected_source = Some(source);
        if let Some(index) = entries.iter().position(|item| item.id == entry) {
            let preceding = entries[..index]
                .iter()
                .map(|item| self.visual_line_count_for(item) + 1)
                .sum();
            self.scroll.set_offset(preceding, self.total_lines());
        }
        self.following_end = false;
    }

    fn restore_anchor(&mut self, anchor: Option<TranscriptAnchor>) {
        let Some(anchor) = anchor else {
            self.scroll.clamp(self.total_lines());
            return;
        };
        let entries = self.projected_entries();
        if entries.is_empty() {
            self.scroll.set_offset(0, 0);
            return;
        }
        let index = entries
            .iter()
            .position(|entry| entry.id == anchor.entry_id)
            .unwrap_or_else(|| anchor.entry_index.min(entries.len() - 1));
        let entry = &entries[index];
        let local_line = anchor
            .logical_line
            .min(self.visual_line_count_for(entry).saturating_sub(1));
        let preceding = entries[..index]
            .iter()
            .map(|item| self.visual_line_count_for(item) + 1)
            .sum::<usize>();
        self.scroll
            .set_offset(preceding + local_line, self.total_lines());
    }

    fn total_lines(&self) -> usize {
        let entries = self.projected_entries();
        entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                self.visual_line_count_for(entry) + usize::from(index + 1 < entries.len())
            })
            .sum()
    }

    fn projected_id_for(
        entries: &[TranscriptEntry],
        source_or_projected: &TranscriptEntryId,
    ) -> Option<TranscriptEntryId> {
        entries
            .iter()
            .find(|entry| {
                entry.id == *source_or_projected
                    || entry.source_ids.iter().any(|id| id == source_or_projected)
            })
            .map(|entry| entry.id.clone())
    }

    fn projected_entries(&self) -> Vec<TranscriptEntry> {
        match self.mode {
            DisplayMode::Rendered => {}
            DisplayMode::Chronology => return self.entries.clone(),
            DisplayMode::Raw => return self.raw_lines(),
        }
        if self.entries.iter().all(|entry| entry.subagent.is_none()) {
            return self.fold_contiguous_groups(&self.entries);
        }
        self.fold_subagent_threads()
    }

    /// One entry per source line: an entry sharing the previous entry's raw
    /// line is folded into it, keeping its identity reachable.
    fn raw_lines(&self) -> Vec<TranscriptEntry> {
        let mut lines: Vec<TranscriptEntry> = Vec::with_capacity(self.entries.len());
        for entry in &self.entries {
            match lines.last_mut() {
                Some(line) if entry.shares_previous_raw => {
                    line.source_ids.extend(entry.source_ids.iter().cloned());
                }
                _ => lines.push(entry.clone()),
            }
        }
        lines
    }

    pub(super) fn visual_line_count_for(&self, entry: &TranscriptEntry) -> usize {
        self.lines_for(entry)
            .iter()
            .map(|line| fragments(line, self.width).len())
            .sum()
    }

    fn lines_for(&self, entry: &TranscriptEntry) -> Vec<TranscriptLine> {
        // Raw views show source bytes, not the choice card, so only the
        // rendered card carries the highlight.
        let card = self.mode != DisplayMode::Raw && !self.is_raw(&entry.id);
        let nested =
            self.mode == DisplayMode::Rendered && entry.subagent.is_some() && !entry.group_header;
        let lines =
            match self.mode {
                DisplayMode::Raw => {
                    vec![(
                        raw_display(&entry.raw),
                        TranscriptPresentation::Neutral,
                        false,
                        false,
                    )]
                }
                DisplayMode::Rendered | DisplayMode::Chronology => {
                    if self.is_raw(&entry.id) {
                        vec![(
                            raw_display(&entry.raw),
                            TranscriptPresentation::Neutral,
                            false,
                            false,
                        )]
                    } else if entry.group_header {
                        let marker = if self.is_expanded(&entry.id) {
                            "▾"
                        } else {
                            "▸"
                        };
                        vec![(
                            format!("{marker} {}", entry.header),
                            entry.presentation,
                            true,
                            false,
                        )]
                    } else {
                        let mut lines = entry
                            .rendered_lines
                            .iter()
                            .cloned()
                            .map(|text| (text, entry.presentation, false, true))
                            .collect::<Vec<_>>();
                        let expanded = self.is_expanded(&entry.id);
                        let has_details = !entry.detail_lines.is_empty()
                            && !entry
                                .detail_lines
                                .iter()
                                .map(|detail| detail.text.as_str())
                                .eq(entry.rendered_lines.iter().map(String::as_str));
                        if expanded && has_details {
                            lines.extend(entry.detail_lines.iter().map(|detail| {
                                (detail.text.clone(), detail.presentation, false, true)
                            }));
                        }
                        // Conversation and Chronology intentionally share the
                        // same card header. The source timestamp is already
                        // shown as date, hour, minute and second; switching a
                        // projection must not turn into a formatting change.
                        let marker = if has_details {
                            if expanded {
                                "⌄"
                            } else {
                                "›"
                            }
                        } else {
                            "·"
                        };
                        lines.insert(
                            0,
                            (
                                format!("{marker} {}", entry.header),
                                entry.presentation,
                                true,
                                false,
                            ),
                        );
                        lines
                    }
                }
            };
        lines
            .into_iter()
            .enumerate()
            .map(
                |(logical_line, (text, presentation, header, indented))| TranscriptLine {
                    entry_id: entry.id.clone(),
                    logical_line,
                    text,
                    presentation,
                    header,
                    indented,
                    nested,
                    selected: self.selected.as_ref() == Some(&entry.id),
                    workflow_choice: card && entry.workflow_choice,
                    clipped: card
                        && entry.clip_head
                        && (header || (entry.workflow_choice && logical_line == 1)),
                },
            )
            .collect()
    }
}
