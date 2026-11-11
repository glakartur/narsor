use super::entry::{TranscriptEntryId, TranscriptPresentation};
use crate::tui::scroll::wrap_line;

/// A logical display line and the stable entry-local location it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptLine {
    pub entry_id: TranscriptEntryId,
    pub logical_line: usize,
    pub text: String,
    pub presentation: TranscriptPresentation,
    pub header: bool,
    pub indented: bool,
    /// Inside a subagent group: drawn two columns further in.
    pub nested: bool,
    pub selected: bool,
    /// Part of a workflow choice card; every wrapped fragment keeps it.
    pub workflow_choice: bool,
    /// One line by nature: clipped with `…` to the width, not wrapped.
    pub clipped: bool,
}

/// The fragments of `line` at `width`: one clipped fragment for a line that
/// is one line by nature.
pub(super) fn fragments(line: &TranscriptLine, width: usize) -> Vec<String> {
    let width = wrapped_width(line.indented, width).saturating_sub(if line.nested {
        NESTED_INDENT
    } else {
        0
    });
    let width = width.max(1);
    if !line.clipped {
        return wrap_line(&line.text, width);
    }
    let text = line.text.replace('\n', " ");
    let length = text.chars().count();
    vec![if length <= width {
        text
    } else {
        text.chars()
            .take(width.saturating_sub(1))
            .chain(std::iter::once('…'))
            .collect()
    }]
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
        let wrapped = fragments(line, width);
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
                nested: line.nested,
                // The cursor is a vertical rail for the whole entry. Headers
                // only own their first wrapped fragment, but body fragments
                // all remain visibly selected.
                selected: line.selected && (!line.header || fragment == 0),
                workflow_choice: line.workflow_choice,
                clipped: line.clipped,
            });
        }
    }
    window
}

/// Columns a subagent group's entries are drawn further in.
const NESTED_INDENT: usize = 2;

fn wrapped_width(indented: bool, width: usize) -> usize {
    if indented {
        width.saturating_sub(2).max(1)
    } else {
        width.max(1)
    }
}

/// Shows retained bytes as readable, terminal-safe text.
///
/// The original bytes remain in [`TranscriptEntry::raw`], while this view
/// decodes them lossily and removes terminal controls for human inspection.
///
/// [`TranscriptEntry::raw`]: super::TranscriptEntry::raw
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
