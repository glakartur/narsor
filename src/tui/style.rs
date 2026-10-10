//! The colour roles (R17, origin §5): the only module that names a colour.
//!
//! Views ask for a [`Role`] — "this needs the operator", "this is happening
//! now" — never for a colour, so the palette lives in one place and the rules
//! behind it can be kept:
//!
//! * colour means something, it does not decorate: only what needs attention
//!   or is happening now is loud, calm states keep the terminal's text colour;
//! * only the 16 ANSI colours, so the interface inherits the terminal theme
//!   and reads on a light background as well as a dark one — no RGB, and a
//!   background only for the key caps on the shortcut bar;
//! * colour never carries information alone: every role is shown with an icon
//!   and a word, which is why `NO_COLOR` can take all colour away
//!   ([`Styles::new`]) and leave bold, dim and reverse video behind without
//!   losing anything;
//! * one accent, cyan, for focus and interaction.
//!
//! # The selection is a mark beside the content, never a fill
//!
//! A list (tasks, history, the step timeline, every chooser) brackets its
//! selected item: a rule in the gap row above it, a rule in the gap row below
//! it and a bar beside each of its lines. A transcript and *Zdarzenia* keep
//! the bar alone. Both come from [`Styles::selection_marks`] and follow the
//! panel frames' rule: thick `━`/`┃` in [`Role::Focus`] where the keys are,
//! thin `─`/`│` in [`Role::Done`] elsewhere. The content keeps its own roles,
//! and without colour the shape and weight still tell the two apart.
//!
//! # `NO_COLOR` is read once
//!
//! [`Styles::from_env`] looks at the variable at startup and the result is
//! handed to everything that draws. Nothing else reads the environment, so a
//! render test picks a mode by building [`Styles`] itself.
//!
//! # The conversation scale
//!
//! The transcript kinds (operator, agent, conductor, system, technical,
//! error) are a separate scale in a separate panel ([`Styles::chat_header`],
//! [`Styles::chat_body`]). They used to be RGB fills; here each kind keeps its
//! *distinction* as an ANSI foreground colour, and the role label in the
//! header (`USER`, `AGENT`, …) carries it for `NO_COLOR`.

use std::ffi::OsString;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType};
use ratatui::Frame;

use super::transcript::TranscriptPresentation;

/// What a piece of text means. Views pick roles, [`Styles`] picks looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// Needs the operator: `?`, the waiting panel's frame, `? n`, `◌ ponawiam`.
    Attention,
    /// A problem, an error, something irreversible: `!`, `✗`, `! n`,
    /// `✖ brak połączenia`, `!` actions, field errors.
    Problem,
    /// Happening now: `●` running, `⟳` provisioning, `● live`, a success
    /// notification.
    Live,
    /// Calm: `◷` queued, `‖` paused. The terminal's own text colour.
    Calm,
    /// Finished or in the background: `✓`, inactive frames, secondary
    /// descriptions, ages, stale data, unavailable actions.
    Done,
    /// Focus and interaction: the active frame, the active tab, the
    /// selection marks where the keys are.
    Focus,
    /// The conductor: `◆` recommendations, proposed values, its entries.
    Conductor,
    /// A link from `display.links`.
    Link,
}

impl Role {
    /// Every role, for tests that walk them.
    pub const ALL: [Role; 8] = [
        Role::Attention,
        Role::Problem,
        Role::Live,
        Role::Calm,
        Role::Done,
        Role::Focus,
        Role::Conductor,
        Role::Link,
    ];
}

/// How a selection is drawn: see [`Styles::selection_marks`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionMarks {
    /// The horizontal rule above and below the selected item.
    pub rule: &'static str,
    /// The vertical bar beside each line of the selected item.
    pub bar: &'static str,
    /// The look of both.
    pub style: Style,
}

/// Whether a value of `NO_COLOR` switches colour off: set and not empty, as
/// no-color.org has it.
pub fn no_color_requested(value: Option<OsString>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

/// The looks of every role, in one of two modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Styles {
    no_color: bool,
}

impl Styles {
    /// Styles in colour, or — with `no_color` — in bold, dim and reverse
    /// video only.
    pub fn new(no_color: bool) -> Self {
        Self { no_color }
    }

    /// Styles for the environment: reads `NO_COLOR`. Call once at startup.
    pub fn from_env() -> Self {
        Self::new(no_color_requested(std::env::var_os("NO_COLOR")))
    }

    /// The look of `role`.
    pub fn of(&self, role: Role) -> Style {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        if self.no_color {
            return match role {
                Role::Attention | Role::Problem | Role::Focus => bold,
                Role::Live | Role::Calm | Role::Conductor => Style::default(),
                Role::Done => Style::default().add_modifier(Modifier::DIM),
                Role::Link => Style::default().add_modifier(Modifier::UNDERLINED),
            };
        }
        match role {
            Role::Attention => bold.fg(Color::Yellow),
            Role::Problem => Style::default().fg(Color::Red),
            Role::Live => Style::default().fg(Color::Green),
            Role::Calm => Style::default(),
            Role::Done => Style::default().fg(Color::DarkGray),
            Role::Focus => Style::default().fg(Color::Cyan),
            Role::Conductor => Style::default().fg(Color::Magenta),
            Role::Link => Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::UNDERLINED),
        }
    }

    /// The look of `role`, in bold: a title.
    pub fn strong(&self, role: Role) -> Style {
        self.of(role).add_modifier(Modifier::BOLD)
    }

    /// The look of `role` for the tab that is open: bold and underlined.
    pub fn selected_tab(&self, role: Role) -> Style {
        self.strong(role).add_modifier(Modifier::UNDERLINED)
    }

    /// The frame of a panel titled `title` (R1–R3): the panel with the focus
    /// gets a thick line in the focus role and a bold title, the others a thin
    /// one in the background role. Without colour the thickness and the bold
    /// title still tell which panel has the keys.
    pub fn panel(&self, title: impl Into<String>, focused: bool) -> Block<'static> {
        let (role, border) = if focused {
            (Role::Focus, BorderType::Thick)
        } else {
            (Role::Done, BorderType::Plain)
        };
        let title_style = if focused {
            self.strong(role)
        } else {
            self.of(role)
        };
        Block::bordered()
            .border_type(border)
            .border_style(self.of(role))
            .title(Line::styled(title.into(), title_style))
    }

    /// The marks of a selection (R8, R12, R14): the rules above and below the
    /// selected item and the bar on its left.
    ///
    /// The same rule as [`Styles::panel`]: where the keys are, thick `━`/`┃`
    /// in the focus role; elsewhere thin `─`/`│` in the background role. The
    /// characters are those of the thick and the plain panel frames, so a
    /// selection reads like a piece of frame. Without colour the role leaves
    /// bold or dim behind, and the shape of the line still tells them apart.
    pub fn selection_marks(&self, focused: bool) -> SelectionMarks {
        if focused {
            SelectionMarks {
                rule: "━",
                bar: "┃",
                style: self.of(Role::Focus),
            }
        } else {
            SelectionMarks {
                rule: "─",
                bar: "│",
                style: self.of(Role::Done),
            }
        }
    }

    /// Draws the frame of a panel titled `title` (see [`Styles::panel`]) over
    /// `area` and returns the area inside it.
    pub fn render_panel(
        &self,
        frame: &mut Frame,
        area: Rect,
        title: impl Into<String>,
        focused: bool,
    ) -> Rect {
        let block = self.panel(title, focused);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        inner
    }

    /// A key on the shortcut bar: dark on the accent, or reversed.
    pub fn key_cap(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        }
    }

    /// The header of a conversation entry of this kind.
    ///
    /// Colour follows the entry's nature: the operator is cyan, the agent
    /// green, the conductor magenta (its role colour — not amber, since
    /// yellow means "needs you"), system and technical entries recede in
    /// dark grey, errors are bold red.
    pub fn chat_header(&self, kind: TranscriptPresentation) -> Style {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        if self.no_color {
            return match kind {
                TranscriptPresentation::Neutral => Style::default(),
                TranscriptPresentation::Operator
                | TranscriptPresentation::Agent
                | TranscriptPresentation::Conductor => bold,
                TranscriptPresentation::System | TranscriptPresentation::Technical => {
                    Style::default().add_modifier(Modifier::DIM)
                }
                TranscriptPresentation::Error => bold.add_modifier(Modifier::REVERSED),
            };
        }
        match kind {
            TranscriptPresentation::Neutral => Style::default(),
            TranscriptPresentation::Operator => bold.fg(Color::Cyan),
            TranscriptPresentation::Agent => bold.fg(Color::Green),
            TranscriptPresentation::Conductor => bold.fg(Color::Magenta),
            TranscriptPresentation::System => Style::default().fg(Color::DarkGray),
            TranscriptPresentation::Technical => Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
            TranscriptPresentation::Error => bold.fg(Color::Red),
        }
    }

    /// The body of a conversation entry of this kind: neutral, except for
    /// the kinds that recede and for errors.
    pub fn chat_body(&self, kind: TranscriptPresentation) -> Style {
        match kind {
            TranscriptPresentation::System | TranscriptPresentation::Technical => {
                self.of(Role::Done)
            }
            TranscriptPresentation::Error => self.of(Role::Problem),
            _ => Style::default(),
        }
    }
}
