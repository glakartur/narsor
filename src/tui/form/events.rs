//! What a form tells its owner. Plain data, returned from the calls that
//! change something; nobody is called back.

/// The side of a field an arrow press stopped at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// The top: `↑`.
    Up,
    /// The bottom: `↓`.
    Down,
}

/// Which way a field grows when it needs more rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Over the content above it.
    Up,
    /// Under, over the content below it.
    Down,
}

/// How a field may grow: in which direction and to how many rows at most.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Growth {
    /// The way it grows.
    pub direction: Direction,
    /// The most rows it may take; past them it scrolls.
    pub limit_rows: usize,
}

/// One thing that happened in a form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormEvent {
    /// The form wants another number of rows than it had.
    Resized {
        /// The rows it wants now.
        rows: u16,
    },
    /// The form asks the operator to confirm the send.
    ConfirmRequested,
    /// The send was confirmed.
    Submitted,
    /// The form was left without sending.
    Cancelled,
    /// The form got or lost the focus.
    FocusChanged {
        /// Whether it has the focus now.
        gained: bool,
    },
    /// A value changed.
    Edited,
}
