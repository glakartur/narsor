//! Reusable, terminal-independent presentation state for chronological text.
//!
//! Source adapters own their records and actions. This module only keeps the
//! reading position stable while a conversation grows or changes presentation.

mod entry;
mod grouping;
mod line;
mod state;
#[cfg(test)]
mod tests;

pub use entry::{
    utterance_header, DisplayMode, TranscriptDetailLine, TranscriptEntry, TranscriptEntryId,
    TranscriptGrouping, TranscriptId, TranscriptPresentation,
};
pub use line::{raw_display, wrap_visible_lines, TranscriptLine};
pub use state::{TranscriptAnchor, TranscriptState};
