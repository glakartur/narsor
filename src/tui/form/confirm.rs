//! The window a form shows before it sends: one text for every form, with
//! only its own keys, drawn over whatever is on the screen.
//!
//! Its keys are those of [`Context::FormConfirm`], so the bar and the help
//! read them from the key map like every other key.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::tui::keymap::{self, Context};
use crate::tui::style::{Role, Styles};

/// The question, the same in every form (Z7): no values are repeated in it.
pub const QUESTION: &str = "Zatwierdzić formularz?";

/// Draws the window in the middle of `area`; it fits any area down to the
/// smallest terminal, where the frame gives way before the text does.
pub fn render(frame: &mut Frame, area: Rect, styles: &Styles) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let width = area.width.min(44);
    let height = area.height.min(5);
    let place = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(Styles::FRAME)
        .border_style(styles.of(Role::Focus))
        .title(" Potwierdzenie ");
    let inner = block.inner(place);
    frame.render_widget(Clear, place);
    frame.render_widget(block, place);
    let mut keys = Vec::new();
    for (key, label) in keymap::hints(Context::FormConfirm) {
        keys.push(Span::styled(format!(" {key} "), styles.key_cap()));
        keys.push(Span::raw(format!("{label} ")));
    }
    let lines = vec![Line::raw(QUESTION), Line::raw(""), Line::from(keys)];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}
