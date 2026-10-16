//! Reusable, terminal-independent presentation state for chronological text.
//!
//! Source adapters own their records and actions. This module only keeps the
//! reading position stable while a conversation grows or changes presentation.

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

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
    /// The semantic conversation projection.
    Rendered,
    /// Every retained source event, with its observed time.
    Chronology,
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

/// One expanded transcript line and its semantic presentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptDetailLine {
    pub text: String,
    pub presentation: TranscriptPresentation,
}

impl TranscriptDetailLine {
    pub fn new(text: impl Into<String>, presentation: TranscriptPresentation) -> Self {
        Self {
            text: text.into(),
            presentation,
        }
    }
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
    /// Additional lines included only when expanded in rendered mode.
    pub detail_lines: Vec<TranscriptDetailLine>,
    /// Normalized adapter fields, formatted only while this entry is expanded.
    pub details: Option<Value>,
    /// Exact retained source bytes. This is never replaced by decoded text.
    /// Shared, not copied, by the entries of one source line and by every
    /// projection of the transcript.
    pub raw: Arc<[u8]>,
    /// Whether [`Self::raw`] is the same source line as the entry before
    /// it: one source line can yield several entries, and the global raw
    /// view shows that line once.
    shares_previous_raw: bool,
    /// Source-neutral lifecycle metadata used to recognize a safe, contiguous
    /// summary card. Adapters decide which harness events qualify.
    grouping: Option<TranscriptGrouping>,
    /// Source identities represented by this item. A normal card has one;
    /// a Conversation summary card has every source event it collapsed.
    source_ids: Vec<TranscriptEntryId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptGrouping {
    session_id: String,
    family: String,
    name: String,
    phase: Option<String>,
    correlation_id: Option<String>,
    /// Unlike hook families, an operation is one start/result pair. Its
    /// result may arrive after unrelated entries, so it is projected by its
    /// correlation id rather than by contiguous source position.
    operation: bool,
}

impl TranscriptEntry {
    pub fn new(
        id: TranscriptEntryId,
        rendered_lines: Vec<String>,
        raw: impl Into<Arc<[u8]>>,
    ) -> Self {
        let source_ids = vec![id.clone()];
        Self {
            id,
            header: String::new(),
            rendered_lines,
            presentation: TranscriptPresentation::Neutral,
            detail_lines: Vec::new(),
            details: None,
            raw: raw.into(),
            shares_previous_raw: false,
            grouping: None,
            source_ids,
        }
    }

    /// Marks [`Self::raw`] as the source line of the entry before this one,
    /// so the global raw view folds this entry into it.
    pub fn sharing_previous_raw(mut self) -> Self {
        self.shares_previous_raw = true;
        self
    }

    pub fn with_header(mut self, header: impl Into<String>) -> Self {
        self.header = header.into();
        self
    }

    pub fn with_detail_lines(self, detail_lines: Vec<String>) -> Self {
        self.with_details(
            detail_lines
                .into_iter()
                .map(|text| TranscriptDetailLine::new(text, TranscriptPresentation::Technical))
                .collect(),
        )
    }

    pub fn with_details(mut self, detail_lines: Vec<TranscriptDetailLine>) -> Self {
        self.detail_lines = detail_lines;
        self
    }

    pub fn with_presentation(mut self, presentation: TranscriptPresentation) -> Self {
        self.presentation = presentation;
        self
    }

    /// Attaches the complete adapter-normalized operation tuple used by the
    /// Conversation projection.  The tuple deliberately contains no source
    /// JSON: presentation never reparses raw records to infer correlation.
    pub fn with_operation_grouping(
        mut self,
        session_id: Option<&str>,
        name: Option<&str>,
        phase: Option<&str>,
        correlation_id: Option<&str>,
    ) -> Self {
        let Some(session_id) = session_id else {
            return self;
        };
        let Some(name) = name.filter(|name| !name.is_empty()) else {
            return self;
        };
        self.grouping = Some(TranscriptGrouping {
            session_id: session_id.to_owned(),
            family: "Hooki".to_owned(),
            name: name.to_owned(),
            phase: phase.map(str::to_owned),
            correlation_id: correlation_id.map(str::to_owned),
            operation: false,
        });
        self
    }

    /// Names the technical family shown by a contiguous summary card.
    pub fn with_group_family(mut self, family: impl Into<String>) -> Self {
        if let Some(grouping) = &mut self.grouping {
            grouping.family = family.into();
        }
        self
    }

    /// Attaches one correlated tool or skill operation.  This is deliberately
    /// distinct from a hook family: multiple operations can interleave and
    /// must still remain independently expandable and diagnosable.
    pub fn with_correlated_operation(
        mut self,
        session_id: Option<&str>,
        name: Option<&str>,
        phase: Option<&str>,
        correlation_id: Option<&str>,
    ) -> Self {
        let Some(session_id) = session_id else {
            return self;
        };
        let Some(name) = name.filter(|name| !name.is_empty()) else {
            return self;
        };
        let Some(correlation_id) = correlation_id.filter(|id| !id.is_empty()) else {
            return self;
        };
        self.grouping = Some(TranscriptGrouping {
            session_id: session_id.to_owned(),
            family: "Narzędzie".to_owned(),
            name: name.to_owned(),
            phase: phase.map(str::to_owned),
            correlation_id: Some(correlation_id.to_owned()),
            operation: true,
        });
        self
    }

    /// Prefixes all stable source identities when composing several attempts.
    pub fn prefix_source_ids(&mut self, prefix: &str) {
        self.id = TranscriptEntryId::from(format!("{prefix}/{}", self.id.as_str()));
        self.source_ids = self
            .source_ids
            .iter()
            .map(|id| TranscriptEntryId::from(format!("{prefix}/{}", id.as_str())))
            .collect();
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
    pub indented: bool,
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
        let wrapped = wrap_line(&line.text, wrapped_width(line.indented, width));
        for (fragment, text) in wrapped.into_iter().enumerate() {
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
                indented: line.indented,
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
    /// A group is a view over an append-only history. Once an operator opens
    /// it (or asks for its source), retain that exact member set: later events
    /// must not appear inside an already-open card.
    revealed_members: HashMap<ExpansionIdentity, HashSet<TranscriptEntryId>>,
    selected: Option<TranscriptEntryId>,
    selected_source: Option<TranscriptEntryId>,
}

fn grouped_detail_lines<'a>(
    members: impl IntoIterator<Item = &'a TranscriptEntry>,
    omitted_line: Option<&str>,
) -> Vec<TranscriptDetailLine> {
    members
        .into_iter()
        .flat_map(|member| {
            let time = member
                .header
                .split(" · ")
                .nth(2)
                .and_then(|stamp| stamp.split_whitespace().nth(1))
                .unwrap_or("");
            member.rendered_lines.iter().filter_map(move |line| {
                let line = line.trim();
                let keep = !line.is_empty() && omitted_line != Some(line);
                keep.then(|| {
                    let attribution = if is_error_entry(member) {
                        "BŁĄD · "
                    } else {
                        ""
                    };
                    if time.is_empty() {
                        TranscriptDetailLine::new(
                            format!("{attribution}{line}"),
                            TranscriptPresentation::Technical,
                        )
                    } else {
                        TranscriptDetailLine::new(
                            format!("{time} · {attribution}{line}"),
                            TranscriptPresentation::Technical,
                        )
                    }
                })
            })
        })
        .collect()
}

fn is_error_entry(entry: &TranscriptEntry) -> bool {
    entry.presentation == TranscriptPresentation::Error
        || entry.details.as_ref().is_some_and(|details| {
            details
                .get("error.message")
                .is_some_and(|error| !error.is_null() && error != "")
        })
}

fn wrapped_width(indented: bool, width: usize) -> usize {
    if indented {
        width.saturating_sub(2).max(1)
    } else {
        width.max(1)
    }
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
                    selected: false,
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
        let mut projected = Vec::new();
        let mut cursor = 0;
        while cursor < self.entries.len() {
            let entry = &self.entries[cursor];
            let Some(grouping) = entry.grouping.as_ref() else {
                projected.push(entry.clone());
                cursor += 1;
                continue;
            };
            // A lifecycle card is only safe when the adapter supplied an
            // operation identity. Similar-looking records without one stay
            // individually reachable rather than being guessed into a group.
            if grouping.correlation_id.is_none() {
                projected.push(entry.clone());
                cursor += 1;
                continue;
            }
            if grouping.operation {
                let correlation_id = grouping.correlation_id.as_deref().expect("checked above");
                let same_operation = |candidate: &TranscriptEntry| {
                    candidate
                        .grouping
                        .as_ref()
                        .is_some_and(|candidate_grouping| {
                            candidate_grouping.operation
                                && candidate_grouping.session_id == grouping.session_id
                                && candidate_grouping.correlation_id.as_deref()
                                    == Some(correlation_id)
                        })
                };
                if grouping.phase.as_deref() != Some("started") {
                    let started_before = self.entries[..cursor].iter().any(|candidate| {
                        same_operation(candidate)
                            && candidate.grouping.as_ref().is_some_and(|metadata| {
                                metadata.phase.as_deref() == Some("started")
                            })
                    });
                    if started_before {
                        let start = self.entries[..cursor]
                            .iter()
                            .rposition(|candidate| {
                                same_operation(candidate)
                                    && candidate.grouping.as_ref().is_some_and(|metadata| {
                                        metadata.phase.as_deref() == Some("started")
                                    })
                            })
                            .expect("started operation exists");
                        let group_identity = self.expansion_identity(&self.entries[start].id);
                        if self
                            .revealed_members
                            .get(&group_identity)
                            .is_none_or(|snapshot| snapshot.contains(&entry.id))
                        {
                            cursor += 1;
                            continue;
                        }
                    }
                    projected.push(entry.clone());
                    cursor += 1;
                    continue;
                }
                let mut members = vec![entry];
                if let Some(completed) = self.entries[cursor + 1..].iter().find(|candidate| {
                    same_operation(candidate)
                        && candidate
                            .grouping
                            .as_ref()
                            .is_some_and(|metadata| metadata.phase.as_deref() == Some("completed"))
                }) {
                    members.push(completed);
                }
                let group_identity = self.expansion_identity(&entry.id);
                let included: Vec<_> = self
                    .revealed_members
                    .get(&group_identity)
                    .map(|snapshot| {
                        members
                            .iter()
                            .copied()
                            .filter(|member| snapshot.contains(&member.id))
                            .collect()
                    })
                    .unwrap_or(members);
                let mut operation = included[0].clone();
                let completed = included.iter().any(|member| {
                    member
                        .grouping
                        .as_ref()
                        .is_some_and(|metadata| metadata.phase.as_deref() == Some("completed"))
                });
                let failed = included.iter().any(|member| is_error_entry(member));
                let mut summary = format!(
                    "Narzędzie · {} · {}",
                    grouping.name,
                    if completed { "zakończone" } else { "w toku" }
                );
                if failed {
                    summary.push_str(" · błąd");
                }
                operation.rendered_lines = vec![summary];
                operation.detail_lines = grouped_detail_lines(included.iter().copied(), None);
                operation.raw = included
                    .iter()
                    .enumerate()
                    .flat_map(|(index, member)| {
                        (index > 0)
                            .then_some(b'\n')
                            .into_iter()
                            .chain(member.raw.iter().copied())
                    })
                    .collect();
                operation.source_ids = included
                    .iter()
                    .flat_map(|member| member.source_ids.clone())
                    .collect();
                operation.grouping = None;
                projected.push(operation);
                cursor += 1;
                continue;
            }
            let mut end = cursor;
            while end < self.entries.len() {
                let candidate = &self.entries[end];
                let Some(candidate_grouping) = candidate.grouping.as_ref() else {
                    break;
                };
                if candidate_grouping.session_id != grouping.session_id
                    || candidate_grouping.name != grouping.name
                {
                    break;
                }
                end += 1;
            }
            if end - cursor < 2 {
                projected.push(entry.clone());
                cursor += 1;
                continue;
            }
            let members = &self.entries[cursor..end];
            let group_identity = self.expansion_identity(&entry.id);
            let included: Vec<_> = self
                .revealed_members
                .get(&group_identity)
                .map(|snapshot| {
                    members
                        .iter()
                        .filter(|member| snapshot.contains(&member.id))
                        .collect()
                })
                .unwrap_or_else(|| members.iter().collect());
            if included.is_empty() {
                cursor = end;
                continue;
            }
            let mut group = (*included[0]).clone();
            let has_error = included.iter().any(|member| is_error_entry(member));
            let mut operations: std::collections::BTreeMap<&str, bool> =
                std::collections::BTreeMap::new();
            for member in &included {
                let Some(metadata) = member.grouping.as_ref() else {
                    continue;
                };
                let Some(id) = metadata.correlation_id.as_deref() else {
                    continue;
                };
                match metadata.phase.as_deref() {
                    Some("started") => {
                        operations.entry(id).or_insert(false);
                    }
                    Some("completed") => {
                        if let Some(done) = operations.get_mut(id) {
                            *done = true;
                        }
                    }
                    _ => {}
                }
            }
            let completed = operations.values().filter(|done| **done).count();
            let in_progress = operations.values().filter(|done| !**done).count();
            let mut summary = format!("{} · {}", grouping.family, grouping.name);
            if !operations.is_empty() {
                if completed > 0 {
                    summary.push_str(&format!(" · {completed} zakończone"));
                }
                if in_progress > 0 {
                    summary.push_str(&format!(" · {in_progress} w toku"));
                }
            } else {
                summary.push_str(&format!(" · {} wpisy", included.len()));
            }
            if has_error {
                summary.push_str(" · błąd");
            }
            group.rendered_lines = vec![summary.clone()];
            group.detail_lines = grouped_detail_lines(included.iter().copied(), Some(&summary));
            group.raw = included
                .iter()
                .enumerate()
                .flat_map(|(index, member)| {
                    (index > 0)
                        .then_some(b'\n')
                        .into_iter()
                        .chain(member.raw.iter().copied())
                })
                .collect();
            group.source_ids = included
                .iter()
                .flat_map(|member| member.source_ids.clone())
                .collect();
            group.grouping = None;
            projected.push(group);
            // A frozen reveal must not absorb later records, but it must not
            // make them disappear either. Keep newly appended members as
            // independent cards until the original group is collapsed.
            if let Some(snapshot) = self.revealed_members.get(&group_identity) {
                projected.extend(
                    members
                        .iter()
                        .filter(|member| !snapshot.contains(&member.id))
                        .cloned(),
                );
            }
            cursor = end;
        }
        projected
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

    fn visual_line_count_for(&self, entry: &TranscriptEntry) -> usize {
        self.lines_for(entry)
            .iter()
            .map(|line| wrap_line(&line.text, wrapped_width(line.indented, self.width)).len())
            .sum()
    }

    fn lines_for(&self, entry: &TranscriptEntry) -> Vec<TranscriptLine> {
        let lines = match self.mode {
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
                    if expanded {
                        if has_details {
                            lines.extend(entry.detail_lines.iter().map(|detail| {
                                (detail.text.clone(), detail.presentation, false, true)
                            }));
                        }
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

    fn remember_revealed_members(
        &mut self,
        entry: &TranscriptEntryId,
        _identity: &ExpansionIdentity,
    ) {
        let Some(group_identity) = self.group_identity_for(entry) else {
            return;
        };
        let members = self.group_members_for(&group_identity.entry);
        if members.len() > 1
            || self
                .entries
                .iter()
                .find(|source| source.id == group_identity.entry)
                .and_then(|source| source.grouping.as_ref())
                .is_some_and(|grouping| grouping.operation)
        {
            self.revealed_members
                .insert(group_identity, members.into_iter().collect());
        }
    }

    fn forget_revealed_members_if_hidden(&mut self, identity: &ExpansionIdentity) {
        let Some(group_identity) = self.group_identity_for(&identity.entry) else {
            return;
        };
        let still_revealed = self
            .group_members_for(&group_identity.entry)
            .iter()
            .any(|member| {
                let member_identity = self.expansion_identity(member);
                self.expanded.contains(&member_identity)
                    || self.raw_entries.contains(&member_identity)
            });
        if !still_revealed {
            self.revealed_members.remove(&group_identity);
        }
    }

    /// The stable source which owns a rendered group.  A hook family owns a
    /// contiguous run, while an operation owns the source that started its
    /// correlation id even if its completion is later in the transcript.
    fn group_identity_for(&self, entry: &TranscriptEntryId) -> Option<ExpansionIdentity> {
        let index = self.entries.iter().position(|source| source.id == *entry)?;
        let grouping = self.entries[index].grouping.as_ref()?;
        grouping.correlation_id.as_ref()?;
        let primary = if grouping.operation {
            self.entries
                .iter()
                .position(|candidate| {
                    candidate.grouping.as_ref().is_some_and(|metadata| {
                        metadata.operation
                            && metadata.session_id == grouping.session_id
                            && metadata.correlation_id == grouping.correlation_id
                            && metadata.phase.as_deref() == Some("started")
                    })
                })
                .unwrap_or(index)
        } else {
            (0..=index)
                .rev()
                .take_while(|&candidate| {
                    self.entries[candidate]
                        .grouping
                        .as_ref()
                        .is_some_and(|metadata| {
                            !metadata.operation
                                && metadata.session_id == grouping.session_id
                                && metadata.name == grouping.name
                        })
                })
                .last()
                .unwrap_or(index)
        };
        Some(self.expansion_identity(&self.entries[primary].id))
    }

    fn group_members_for(&self, primary: &TranscriptEntryId) -> Vec<TranscriptEntryId> {
        let Some(index) = self.entries.iter().position(|source| source.id == *primary) else {
            return Vec::new();
        };
        let Some(grouping) = self.entries[index].grouping.as_ref() else {
            return vec![primary.clone()];
        };
        if grouping.operation {
            return self
                .entries
                .iter()
                .filter(|candidate| {
                    candidate.grouping.as_ref().is_some_and(|metadata| {
                        metadata.operation
                            && metadata.session_id == grouping.session_id
                            && metadata.correlation_id == grouping.correlation_id
                    })
                })
                .map(|candidate| candidate.id.clone())
                .collect();
        }
        self.entries[index..]
            .iter()
            .take_while(|candidate| {
                candidate.grouping.as_ref().is_some_and(|metadata| {
                    !metadata.operation
                        && metadata.session_id == grouping.session_id
                        && metadata.name == grouping.name
                })
            })
            .map(|candidate| candidate.id.clone())
            .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn hook_for_group(id: &str, event: &str, group_name: &str) -> TranscriptEntry {
        let phase = match event {
            "hook_started" => "started",
            "hook_response" => "completed",
            _ => event,
        };
        TranscriptEntry::new(
            TranscriptEntryId::from(id),
            vec![event.to_owned()],
            format!("{{\"hook_id\":\"{id}\"}}").into_bytes(),
        )
        .with_header("SYSTEM · Harness · 2026-10-11 12:00:00".to_string())
        .with_presentation(TranscriptPresentation::System)
        .with_operation_grouping(Some("session"), Some(group_name), Some(phase), Some(id))
    }

    fn hook(id: &str, event: &str) -> TranscriptEntry {
        hook_for_group(id, event, "SessionStart")
    }

    fn tool(id: &str, name: &str, phase: &str) -> TranscriptEntry {
        TranscriptEntry::new(
            TranscriptEntryId::from(format!("tool-{id}-{phase}")),
            vec![format!("{phase}: {name}")],
            format!("{{\"id\":\"{id}\",\"phase\":\"{phase}\"}}").into_bytes(),
        )
        .with_header("TECHNICZNE · Harness · 2026-10-11 12:00:00")
        .with_presentation(TranscriptPresentation::Technical)
        .with_correlated_operation(Some("session"), Some(name), Some(phase), Some(id))
    }

    #[test]
    fn conversation_groups_all_contiguous_hook_events() {
        let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
        transcript.load(vec![
            hook("one", "hook_started"),
            hook("two", "hook_started"),
            hook("two", "hook_response"),
            hook("one", "hook_response"),
        ]);

        let rendered = transcript.logical_lines();
        assert!(rendered
            .iter()
            .any(|line| line.text.starts_with("› SYSTEM · Harness")));
        assert!(rendered
            .iter()
            .any(|line| line.text == "Hooki · SessionStart · 2 zakończone"));
        assert_eq!(rendered.iter().filter(|line| line.header).count(), 1);

        let group = transcript.selected_entry().cloned().unwrap();
        transcript.toggle_expanded(&group);
        let expanded = transcript.logical_lines();
        assert!(expanded
            .iter()
            .any(|line| line.text.starts_with("⌄ SYSTEM · Harness")));
        assert!(expanded
            .iter()
            .any(|line| line.text == "Hooki · SessionStart · 2 zakończone"));
        assert!(expanded
            .iter()
            .any(|line| line.text.starts_with("12:00:00 · hook_started")));
        assert_eq!(
            expanded
                .iter()
                .filter(|line| line.text.contains("SYSTEM · Harness"))
                .count(),
            1
        );

        transcript.next_conversation_view();
        let chronology = transcript.logical_lines();
        assert_eq!(chronology.iter().filter(|line| line.header).count(), 4);
        assert_eq!(
            transcript.selected_entry(),
            Some(&TranscriptEntryId::from("one"))
        );
    }

    #[test]
    fn grouped_error_member_keeps_a_compact_error_attribution() {
        let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
        transcript.load(vec![
            hook("one", "hook_started"),
            hook("two", "hook_response").with_presentation(TranscriptPresentation::Error),
        ]);
        let group = transcript.selected_entry().cloned().unwrap();
        transcript.toggle_expanded(&group);
        assert!(transcript
            .logical_lines()
            .iter()
            .any(|line| line.text.contains("BŁĄD · hook_response")));
    }

    #[test]
    fn conversation_pairs_interleaved_operations_by_id() {
        let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
        transcript.load(vec![
            tool("one", "bash", "started"),
            tool("two", "search", "started"),
            tool("two", "search", "completed"),
            tool("one", "bash", "completed"),
        ]);

        let rendered = transcript.logical_lines();
        assert!(rendered
            .iter()
            .any(|line| line.text.starts_with("› TECHNICZNE · Harness")));
        assert!(rendered
            .iter()
            .any(|line| line.text == "Narzędzie · bash · zakończone"));
        assert!(rendered
            .iter()
            .any(|line| line.text == "Narzędzie · search · zakończone"));
        assert_eq!(rendered.iter().filter(|line| line.header).count(), 2);
    }

    #[test]
    fn operation_summary_exposes_a_result_error_without_expansion() {
        let mut result = tool("one", "bash", "completed");
        result.details = Some(serde_json::json!({"error.message": "permission denied"}));
        let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
        transcript.load(vec![tool("one", "bash", "started"), result]);

        assert!(transcript
            .logical_lines()
            .iter()
            .any(|line| line.text == "Narzędzie · bash · zakończone · błąd"));
    }

    #[test]
    fn local_raw_from_chronology_freezes_future_group_members() {
        let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
        transcript.load(vec![
            hook("one", "hook_started"),
            hook("two", "hook_started"),
        ]);
        transcript.next_conversation_view();
        transcript.line_up();
        transcript.toggle_selected_raw();
        let mut response = hook("two", "hook_response");
        response.id = TranscriptEntryId::from("two-response");
        response.source_ids = vec![TranscriptEntryId::from("two-response")];
        transcript.append(vec![response]);
        transcript.set_mode(DisplayMode::Rendered);

        let rendered = transcript.logical_lines();
        assert_eq!(
            rendered
                .iter()
                .map(|line| &line.entry_id)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            2
        );
    }

    #[test]
    fn chronology_keeps_the_same_second_precision_header_as_conversation() {
        let at = Utc.with_ymd_and_hms(2026, 10, 11, 12, 34, 56).unwrap();
        let entry = TranscriptEntry::new(
            TranscriptEntryId::from("one"),
            vec!["message".into()],
            Vec::new(),
        )
        .with_header(utterance_header(TranscriptPresentation::Agent, "Agent", at))
        .with_presentation(TranscriptPresentation::Agent);
        let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
        transcript.load(vec![entry]);
        let conversation_header = transcript.logical_lines()[0].text.clone();
        transcript.next_conversation_view();
        assert_eq!(transcript.logical_lines()[0].text, conversation_header);
    }

    #[test]
    fn disclosure_marker_only_appears_when_expansion_adds_content() {
        let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
        transcript.load(vec![
            TranscriptEntry::new(
                TranscriptEntryId::from("same"),
                vec!["already visible".into()],
                Vec::new(),
            )
            .with_detail_lines(vec!["already visible".into()]),
            TranscriptEntry::new(
                TranscriptEntryId::from("extra"),
                vec!["summary".into()],
                Vec::new(),
            )
            .with_detail_lines(vec!["additional detail".into()]),
        ]);
        transcript.line_up();
        assert!(transcript
            .logical_lines()
            .iter()
            .filter(|line| line.entry_id == TranscriptEntryId::from("same"))
            .filter(|line| line.header)
            .all(|line| line.text.starts_with("· ")));
        transcript.line_down();
        assert!(transcript
            .logical_lines()
            .iter()
            .any(|line| line.text == "› "));
        let id = transcript.selected_entry().cloned().unwrap();
        transcript.toggle_expanded(&id);
        assert_eq!(
            transcript
                .logical_lines()
                .iter()
                .filter(|line| line.text == "additional detail")
                .count(),
            1
        );
    }

    #[test]
    fn disclosure_state_is_in_header_and_every_body_line_is_indented() {
        let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
        transcript.load(vec![
            TranscriptEntry::new(
                TranscriptEntryId::from("expandable"),
                vec!["first".into(), "second".into()],
                Vec::new(),
            )
            .with_header("AGENT · Agent")
            .with_detail_lines(vec!["extra".into()]),
            TranscriptEntry::new(
                TranscriptEntryId::from("plain"),
                vec!["plain body".into()],
                Vec::new(),
            )
            .with_header("SYSTEM · Harness"),
        ]);

        let collapsed = transcript.logical_lines();
        assert_eq!(collapsed[0].text, "› AGENT · Agent");
        assert_eq!(collapsed[1].text, "first");
        assert_eq!(collapsed[2].text, "second");
        assert!(collapsed[1].indented && collapsed[2].indented);
        assert!(!collapsed[0].indented);
        assert_eq!(collapsed[4].text, "· SYSTEM · Harness");
        assert_eq!(collapsed[5].text, "plain body");
        assert!(collapsed[5].indented);

        transcript.toggle_expanded(&TranscriptEntryId::from("expandable"));
        let expanded = transcript.logical_lines();
        assert_eq!(expanded[0].text, "⌄ AGENT · Agent");
        assert_eq!(expanded[3].text, "extra");
        assert!(expanded[3].indented);
    }

    #[test]
    fn duplicate_tail_lines_still_show_a_disclosure_marker_and_expand() {
        let body = vec![
            "Status:".to_owned(),
            "ok".to_owned(),
            "ok".to_owned(),
            "ok".to_owned(),
            "ok".to_owned(),
        ];
        let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
        transcript.load(vec![TranscriptEntry::new(
            TranscriptEntryId::from("repeated"),
            body,
            Vec::new(),
        )
        .with_detail_lines(vec!["ok".into(), "ok".into()])]);
        let id = transcript.selected_entry().cloned().unwrap();
        assert!(transcript
            .logical_lines()
            .iter()
            .any(|line| line.text == "› "));

        transcript.toggle_expanded(&id);
        assert_eq!(
            transcript
                .logical_lines()
                .iter()
                .filter(|line| line.text == "ok")
                .count(),
            6
        );
    }

    #[test]
    fn wrapped_body_continuations_keep_the_two_space_indent() {
        let lines = [TranscriptLine {
            entry_id: TranscriptEntryId::from("wrapped"),
            logical_line: 0,
            text: "abcdefghijk".into(),
            presentation: TranscriptPresentation::Agent,
            header: false,
            indented: true,
            selected: true,
        }];
        let wrapped = wrap_visible_lines(&lines, 8, 0, 3);
        assert_eq!(
            wrapped
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["abcdef", "ghijk"]
        );
    }

    #[test]
    fn bottom_scroll_reaches_the_last_wrapped_indented_line() {
        let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
        transcript.set_width(10);
        transcript.set_viewport(3);
        transcript.load(vec![TranscriptEntry::new(
            TranscriptEntryId::from("long"),
            vec![
                "abcdefghij".into(),
                "klmnopqrst".into(),
                "uvwxyzABCD".into(),
            ],
            Vec::new(),
        )]);

        let logical = transcript.logical_lines();
        let visible = wrap_visible_lines(&logical, 10, transcript.scroll_offset(), 3);
        assert_eq!(visible.len(), 3);
        assert_eq!(visible.last().unwrap().text, "CD");
        assert!(visible.last().unwrap().indented);
    }

    #[test]
    fn conversation_keeps_different_hook_families_separate() {
        let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
        transcript.load(vec![
            hook_for_group("one", "hook_started", "SessionStart"),
            hook_for_group("two", "hook_response", "OtherLifecycle"),
            hook_for_group("one", "hook_response", "SessionStart"),
        ]);

        let rendered = transcript.logical_lines();
        assert_eq!(rendered.iter().filter(|line| line.header).count(), 3);
        assert!(rendered
            .iter()
            .any(|line| line.text.ends_with("hook_started")));
    }

    #[test]
    fn conversation_groups_adjacent_hooks_even_when_one_is_unpaired() {
        let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
        transcript.load(vec![
            hook("unpaired", "hook_started"),
            hook("paired", "hook_started"),
            hook("paired", "hook_response"),
        ]);

        let rendered = transcript.logical_lines();
        assert_eq!(rendered.iter().filter(|line| line.header).count(), 1);
        assert!(rendered
            .iter()
            .any(|line| line.text.starts_with("› SYSTEM · Harness")));
        assert!(rendered
            .iter()
            .any(|line| line.text == "Hooki · SessionStart · 1 zakończone · 1 w toku"));
    }

    #[test]
    fn revealed_group_does_not_absorb_later_members() {
        let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
        transcript.load(vec![
            hook("one", "hook_started"),
            hook("two", "hook_started"),
        ]);
        let group = transcript.selected_entry().cloned().unwrap();
        transcript.toggle_expanded(&group);
        transcript.append(vec![hook("three", "hook_started")]);

        let lines: Vec<_> = transcript
            .logical_lines()
            .into_iter()
            .map(|line| line.text)
            .collect();
        assert!(lines.iter().any(|line| line.ends_with("hook_started")));
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.ends_with("hook_started"))
                .count(),
            3
        );
        assert!(lines
            .iter()
            .any(|line| line.starts_with("⌄ SYSTEM · Harness")));
    }

    #[test]
    fn closing_local_raw_keeps_an_expanded_group_frozen() {
        let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
        transcript.load(vec![
            hook("one", "hook_started"),
            hook("two", "hook_started"),
        ]);
        let group = transcript.selected_entry().cloned().unwrap();
        transcript.toggle_expanded(&group);
        transcript.toggle_selected_raw();
        transcript.toggle_selected_raw();
        transcript.append(vec![hook("three", "hook_started")]);

        assert_eq!(
            transcript
                .logical_lines()
                .iter()
                .filter(|line| line.text.ends_with("hook_started"))
                .count(),
            3
        );
    }

    #[test]
    fn hooks_without_correlation_remain_separate() {
        let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
        let uncorrelated = |id: &str| {
            TranscriptEntry::new(TranscriptEntryId::from(id), vec![id.into()], Vec::new())
                .with_operation_grouping(
                    Some("session"),
                    Some("SessionStart"),
                    Some("started"),
                    None,
                )
        };
        transcript.load(vec![uncorrelated("one"), uncorrelated("two")]);
        assert_eq!(
            transcript
                .logical_lines()
                .iter()
                .filter(|line| line.header)
                .count(),
            2
        );
        assert_eq!(
            transcript
                .logical_lines()
                .iter()
                .filter(|line| line.text == "one" || line.text == "two")
                .count(),
            2
        );
    }

    #[test]
    fn refresh_preserves_the_source_selected_inside_a_group() {
        let entries = vec![hook("one", "hook_started"), hook("two", "hook_started")];
        let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
        transcript.load(entries.clone());
        transcript.set_mode(DisplayMode::Chronology);
        transcript.line_down();
        assert_eq!(
            transcript.selected_entry(),
            Some(&TranscriptEntryId::from("two"))
        );

        transcript.set_mode(DisplayMode::Rendered);
        transcript.replace_entries(entries);
        transcript.set_mode(DisplayMode::Chronology);
        assert_eq!(
            transcript.selected_entry(),
            Some(&TranscriptEntryId::from("two"))
        );
    }
}
