//! The colour roles (R17, origin §5): the only module that names a colour.
//!
//! Views ask for a [`Role`] — "this needs the operator", "this is happening
//! now" — never for a colour, so the palette lives in one place and the rules
//! behind it can be kept:
//!
//! * colour means something, it does not decorate: only what needs attention
//!   or is happening now is loud, calm states keep the terminal's text colour;
//! * text in the 16 ANSI colours only, so the interface inherits the terminal
//!   theme and reads on a light background as well as a dark one; the few
//!   fixed backgrounds (key caps, the task header, the answer panel, the
//!   selected row, a workflow choice in a transcript) live here and never
//!   colour text;
//! * colour never carries information alone: every role is shown with an icon
//!   and a word, which is why `NO_COLOR` can take all colour away
//!   ([`Styles::new`]) and leave bold, dim and reverse video behind without
//!   losing anything;
//! * one accent, cyan, for focus and interaction.
//!
//! # The selection is a mark beside the content, never a fill
//!
//! A list (tasks, history, the step timeline, every chooser) brackets its
//! selected item: a full-width rule in a row directly above it, another
//! directly below it and a bar beside each of its lines. A transcript and
//! *Zdarzenia* keep the bar alone. Both come from [`Styles::selection_marks`]:
//! always thin `─`/`│`, in [`Role::Focus`] where the keys are and in
//! [`Role::Done`] elsewhere. The content keeps its own roles, and without
//! colour bold and dim still tell the two apart.
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
use ratatui::text::{Line, Span};
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

/// What stands on the top edge of a panel, already cut to fit: pills on the
/// left, one on the right, as [`Styles::render_panel_edge`] draws them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PanelEdge {
    /// Texts of the pills on the left, in order.
    pub left: Vec<String>,
    /// A plain text, not a pill, before the pills: the panel's title when
    /// there is nothing to put in a pill. Carries its own spaces.
    pub plain: Option<String>,
    /// Text of the pill on the right.
    pub right: Option<String>,
    /// The loading marker, after the left side and outside the pills.
    pub tail: Option<String>,
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

    /// Whether colour is off: a meaning a colour would carry needs a sign
    /// of its own then, e.g. the `!` before a usage limit near its end.
    pub fn no_color(&self) -> bool {
        self.no_color
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

    /// The look of `role` for the tab that is open; the separate marker row
    /// supplies the underline cue.
    pub fn selected_tab(&self, role: Role) -> Style {
        self.strong(role)
    }

    /// The frame of every panel: rounded corners, one line type for all.
    pub const FRAME: BorderType = BorderType::Rounded;

    /// The frame of a panel titled `title` (R1–R3): the panel with the focus
    /// gets the focus role, a bold title and a `▸` before the title, the
    /// others the background role. Without colour the marker and the bold
    /// title still tell which panel has the keys.
    pub fn panel(&self, title: impl Into<String>, focused: bool) -> Block<'static> {
        let (role, title) = if focused {
            (Role::Focus, format!("▸{}", title.into()))
        } else {
            (Role::Done, title.into())
        };
        let title_style = if focused {
            self.strong(role)
        } else {
            self.of(role)
        };
        Block::bordered()
            .border_type(Self::FRAME)
            .border_style(self.of(role))
            .title(Line::styled(title, title_style))
    }

    /// The marks of a selection (R8, R12, R14): the rules above and below the
    /// selected item and the bar on its left.
    ///
    /// The lines are always thin, `─`/`│`, so a selection stays lighter than
    /// a panel frame; only the role tells where the keys are: the focus role
    /// there, the background role elsewhere. Without colour the role leaves
    /// bold or dim behind.
    pub fn selection_marks(&self, focused: bool) -> SelectionMarks {
        let role = if focused { Role::Focus } else { Role::Done };
        SelectionMarks {
            rule: "─",
            bar: "│",
            style: self.of(role),
        }
    }

    /// The low-contrast fill behind the selected item of a bracketed list: it
    /// makes the row easier to track while each label keeps its own
    /// foreground role. It is drawn in both modes.
    pub fn selection_fill(&self) -> Style {
        Style::default().bg(Color::Indexed(236))
    }

    /// The block behind a workflow choice in a transcript, from the content
    /// column to the panel's right edge: a dark olive no other background
    /// and no conversation kind uses. Without colour the block is reversed.
    pub fn workflow_choice_background(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default().bg(Color::Indexed(58))
        }
    }

    /// Backgrounds that keep the task header and its answer panel distinct
    /// without drawing separator rules between them and the content.
    pub fn task_header_background(&self) -> Style {
        if self.no_color {
            Style::default()
        } else {
            Style::default().bg(Color::Rgb(34, 48, 72))
        }
    }

    /// The answer panel sits on a dark neutral background; without colour,
    /// focus remains indicated by its structural marker.
    pub fn answer_panel_background(&self) -> Style {
        if self.no_color {
            Style::default()
        } else {
            Style::default().bg(Color::Rgb(38, 40, 43))
        }
    }

    /// The fill behind the field being edited: a navy darker than the task
    /// header, so the field stands out on it. Without colour the field is
    /// told apart by its `>` in the focus role.
    pub fn field_editing(&self) -> Style {
        if self.no_color {
            Style::default()
        } else {
            Style::default().bg(Color::Rgb(24, 36, 64))
        }
    }

    /// The cursor: the cell it stands on, reversed. Nothing is inserted
    /// into the text.
    pub fn cursor(&self) -> Style {
        Style::default().add_modifier(Modifier::REVERSED)
    }

    /// The answer panel's edge shows focus across its full height.
    pub fn answer_panel_marker(&self, focused: bool) -> Style {
        self.of(if focused { Role::Focus } else { Role::Done })
    }

    /// A quiet rule that separates the task tabs from the content below.
    pub fn task_tabs_divider(&self) -> Style {
        if self.no_color {
            Style::default()
        } else {
            Style::default().fg(Color::Black)
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

    /// Cells a pill adds to its text: a cap on each side and a space inside
    /// each cap. Without colour the caps are `┤` and `├`, so the width is the
    /// same in both modes.
    pub const PILL_OVERHEAD: usize = 4;

    /// A pill on a panel's edge: `text` on a ground in `role`'s colour between
    /// two rounded caps (U+E0B6, U+E0B4). Without colour it is `┤ text ├` in
    /// the role's own style.
    pub fn edge_pill(&self, text: &str, role: Role, bold: bool) -> Vec<Span<'static>> {
        let weight = |style: Style| {
            if bold {
                style.add_modifier(Modifier::BOLD)
            } else {
                style
            }
        };
        if self.no_color {
            return vec![Span::styled(format!("┤ {text} ├"), weight(self.of(role)))];
        }
        let ground = self.of(role).fg.unwrap_or(Color::Reset);
        let cap = Style::default().fg(ground);
        vec![
            Span::styled("\u{e0b6}", cap),
            Span::styled(
                format!(" {text} "),
                weight(Style::default().fg(Color::Black).bg(ground)),
            ),
            Span::styled("\u{e0b4}", cap),
        ]
    }

    /// Draws the frame of a panel with `edge` on its top line (see
    /// [`PanelEdge`]) over `area` and returns the area inside it. The frame
    /// looks as [`Styles::panel`] has it; a `stale` edge is in the background
    /// role whoever has the keys.
    pub fn render_panel_edge(
        &self,
        frame: &mut Frame,
        area: Rect,
        edge: &PanelEdge,
        focused: bool,
        stale: bool,
    ) -> Rect {
        let frame_role = if focused { Role::Focus } else { Role::Done };
        let pill_role = if stale { Role::Done } else { frame_role };
        let mut left: Vec<Span<'static>> = Vec::new();
        if focused {
            left.push(Span::styled("▸", self.strong(frame_role)));
        }
        if let Some(plain) = &edge.plain {
            let style = if focused {
                self.strong(frame_role)
            } else {
                self.of(frame_role)
            };
            left.push(Span::styled(plain.clone(), style));
        }
        for (index, text) in edge.left.iter().enumerate() {
            if index > 0 {
                left.push(Span::styled("─", self.of(frame_role)));
            }
            left.extend(self.edge_pill(text, pill_role, focused));
        }
        if let Some(tail) = &edge.tail {
            left.push(Span::styled(format!(" {tail}"), self.strong(frame_role)));
        }
        let mut block = Block::bordered()
            .border_type(Self::FRAME)
            .border_style(self.of(frame_role))
            .title(Line::from(left).left_aligned());
        if let Some(right) = &edge.right {
            block =
                block.title(Line::from(self.edge_pill(right, pill_role, false)).right_aligned());
        }
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
