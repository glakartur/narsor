//! The drawing of the chat form.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::Chat;
use crate::tui::chooser;
use crate::tui::form::events::{Direction, Growth};
use crate::tui::scroll::one_line;
use crate::tui::style::{Role, Styles};

/// The rows of the form other than the box: one for the error.
const NOTICE_ROWS: u16 = 1;

impl Chat {
    /// The rows the box wants in `width` columns, when the details are
    /// `details_height` rows high: one row to half the details, then it
    /// scrolls (R16).
    pub fn box_rows(&self, width: u16, details_height: u16) -> u16 {
        self.chooser.set_growth(Growth {
            direction: Direction::Down,
            limit_rows: usize::from(details_height / 2).max(1),
        });
        self.chooser.preferred_rows(usize::from(width)).max(1)
    }

    /// The rows the whole form wants.
    pub fn preferred_height(&self, width: u16, details_height: u16) -> u16 {
        self.box_rows(width, details_height) + NOTICE_ROWS
    }

    /// Draws the form into `area`; `details_height` is the height the 50% of
    /// the box is counted from.
    pub fn render(
        &self,
        frame: &mut Frame,
        area: Rect,
        details_height: u16,
        styles: &Styles,
        focused: bool,
    ) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let rows = self.box_rows(area.width, details_height);
        let [notice, text_box] = Layout::vertical([
            Constraint::Length(NOTICE_ROWS.min(area.height)),
            Constraint::Length(rows.min(area.height.saturating_sub(NOTICE_ROWS))),
        ])
        .areas(area);
        let line = match &self.error {
            Some(error) => Line::styled(format!("✗ {}", one_line(error)), styles.of(Role::Problem)),
            None => Line::styled(
                "Ctrl+J nowa linia · Enter wysyła",
                styles.of(Role::Done),
            ),
        };
        frame.render_widget(Paragraph::new(line), notice);
        chooser::render_fields(frame, text_box, &self.chooser, styles, focused);
    }
}
