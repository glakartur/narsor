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
//!   background only for the selected row (reverse video) and the key caps on
//!   the shortcut bar;
//! * colour never carries information alone: every role is shown with an icon
//!   and a word, which is why `NO_COLOR` can take all colour away
//!   ([`Styles::new`]) and leave bold, dim and reverse video behind without
//!   losing anything;
//! * one accent, cyan, for focus and interaction.
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

use ratatui::style::{Color, Modifier, Style};

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
    /// Focus and interaction: the active frame, the active tab, the `▸`
    /// cursor.
    Focus,
    /// The conductor: `◆` recommendations, proposed values, its entries.
    Conductor,
    /// A link from `display.links`.
    Link,
    /// The selected row. Reverse video, so a marker's own colour survives
    /// inside it.
    Selected,
}

impl Role {
    /// Every role, for tests that walk them.
    pub const ALL: [Role; 9] = [
        Role::Attention,
        Role::Problem,
        Role::Live,
        Role::Calm,
        Role::Done,
        Role::Focus,
        Role::Conductor,
        Role::Link,
        Role::Selected,
    ];
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

    /// Whether colour is off.
    pub fn is_no_color(&self) -> bool {
        self.no_color
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
                Role::Selected => Style::default().add_modifier(Modifier::REVERSED),
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
            Role::Selected => Style::default().add_modifier(Modifier::REVERSED),
        }
    }

    /// `base` with the selection laid over it: reverse video, nothing else,
    /// so what `base` says is still said.
    pub fn selected(&self, base: Style) -> Style {
        base.patch(self.of(Role::Selected))
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
