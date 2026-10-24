//! Render tests for the TUI building blocks (U7): the chooser, the shortcut
//! bar and help, the connection indicator and the colour rules.
//!
//! Frames are drawn on a `TestBackend` and read back flattened, row by row,
//! the way `tests/tui_state.rs` reads the older views. Nothing here starts a
//! daemon.

use std::fs;
use std::ops::Range;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::backend::TestBackend;
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

use narsor::tui::chooser::{self, Chooser, ChooserField, ChooserItem};
use narsor::tui::connection::Connection;
use narsor::tui::keymap::{self, Context};
use narsor::tui::style::{SelectionMarks, Styles};
use narsor::tui::task_app::{layout, TaskApp};

mod environment_support;
mod remote_support;
mod task_app_support;

fn draw(width: u16, height: u16, paint: impl FnOnce(&mut ratatui::Frame)) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("a test terminal");
    terminal.draw(paint).expect("draw");
    terminal.backend().buffer().clone()
}

fn rows(buffer: &Buffer) -> Vec<String> {
    let width = usize::from(buffer.area.width);
    buffer
        .content()
        .chunks(width)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect()
}

fn screen(buffer: &Buffer) -> String {
    rows(buffer).join("\n")
}

/// The cells drawn reversed: the cursor, which is a style of the cell it
/// stands on and no character of the text.
fn cursor_cells(buffer: &Buffer) -> Vec<(u16, u16)> {
    let width = buffer.area.width;
    buffer
        .content()
        .iter()
        .enumerate()
        .filter(|(_, cell)| cell.modifier.contains(Modifier::REVERSED))
        .map(|(index, _)| (index as u16 % width, index as u16 / width))
        .collect()
}

/// Whether the cursor stands in the cell right after `text` on its row.
fn cursor_follows(buffer: &Buffer, text: &str) -> bool {
    let rows = rows(buffer);
    let Some((y, row)) = rows.iter().enumerate().find(|(_, row)| row.contains(text)) else {
        return false;
    };
    let end = row
        .find(text)
        .map(|byte| row[..byte].chars().count() + text.chars().count());
    end.is_some_and(|x| cursor_cells(buffer).contains(&(x as u16, y as u16)))
}

fn row_of(buffer: &Buffer, needle: &str) -> usize {
    rows(buffer)
        .iter()
        .position(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} is not on the screen:\n{}", screen(buffer)))
}

/// The symbol and the colour of the cell at column `x`, row `y`.
fn cell_at(buffer: &Buffer, x: u16, y: u16) -> (String, Color) {
    let cell = &buffer[(x, y)];
    (cell.symbol().to_string(), cell.fg)
}

/// Whether the cell at column `x`, row `y` is bold.
fn bold_at(buffer: &Buffer, x: u16, y: u16) -> bool {
    buffer[(x, y)].modifier.contains(Modifier::BOLD)
}

/// The column of the first `symbol` in row `y` at or after column `from`.
fn column_of(buffer: &Buffer, y: u16, from: u16, symbol: &str) -> Option<u16> {
    (from..buffer.area.width).find(|&x| buffer[(x, y)].symbol() == symbol)
}

/// The column at which `needle` starts in row `y`, counted in cells.
fn column_in(buffer: &Buffer, y: usize, needle: &str) -> u16 {
    let row = &rows(buffer)[y];
    let byte = row
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} is not in row {y}:\n{}", screen(buffer)));
    u16::try_from(row[..byte].chars().count()).expect("a column")
}

/// The cells of row `y` in `columns`.
fn row_cells(buffer: &Buffer, y: u16, columns: Range<u16>) -> impl Iterator<Item = &Cell> {
    columns.map(move |x| &buffer[(x, y)])
}

/// Whether none of `cells` is in reverse video, which a selection never
/// uses (R7).
fn none_reversed<'a>(cells: impl IntoIterator<Item = &'a Cell>) -> bool {
    cells
        .into_iter()
        .all(|cell| !cell.modifier.contains(Modifier::REVERSED))
}

/// Whether `cells` are drawn without colour: every foreground is the
/// terminal's own, and the only background is the fill behind a selected
/// row ([`Styles::selection_fill`]), which `styles` draws in both modes.
fn no_colour_but_the_selection_fill<'a>(
    cells: impl IntoIterator<Item = &'a Cell>,
    styles: &Styles,
) -> bool {
    let fill = styles.selection_fill().bg.unwrap_or(Color::Reset);
    cells
        .into_iter()
        .all(|cell| cell.fg == Color::Reset && [Color::Reset, fill].contains(&cell.bg))
}

/// The selection marks: thin, in the focus role where the keys are
/// (`focused`), in colour or — with `no_color` — in weight only.
fn marks(focused: bool, no_color: bool) -> SelectionMarks {
    Styles::new(no_color).selection_marks(focused)
}

/// Whether `cell` is `symbol` in the look of `marks`: their colour, or their
/// weight without colour.
fn is_mark(cell: &Cell, symbol: &str, marks: SelectionMarks) -> bool {
    cell.symbol() == symbol
        && cell.fg == marks.style.fg.unwrap_or(Color::Reset)
        && cell.modifier.contains(marks.style.add_modifier)
}

/// Whether row `y` is a rule of `marks` over every one of `columns`.
fn is_rule(buffer: &Buffer, y: u16, columns: Range<u16>, marks: SelectionMarks) -> bool {
    row_cells(buffer, y, columns).all(|cell| is_mark(cell, marks.rule, marks))
}

/// Asserts the item whose content starts at column `x` of row `y` and is
/// `lines` rows high is bracketed with `marks` (R8, R12, R14): the bar two
/// columns left of each of its lines and a blank column after it, and a rule
/// from the bar's column — the list's first — up to `rule_end` in the row
/// directly above and the row directly below. The bar and the rules are in
/// rows of their own, so they never touch. Nothing in the bracket is
/// reversed (R7).
fn assert_bracket(
    buffer: &Buffer,
    (x, y): (u16, u16),
    lines: u16,
    rule_end: u16,
    marks: SelectionMarks,
) {
    let screen = screen(buffer);
    for row in y..y + lines {
        assert!(
            is_mark(&buffer[(x - 2, row)], marks.bar, marks),
            "bar in row {row}:\n{screen}"
        );
        assert_eq!(
            buffer[(x - 1, row)].symbol(),
            " ",
            "gap in row {row}:\n{screen}"
        );
    }
    for row in [y - 1, y + lines] {
        assert!(
            is_rule(buffer, row, x - 2..rule_end, marks),
            "rule in row {row}:\n{screen}"
        );
    }
    assert!(
        (y - 1..=y + lines).all(|row| none_reversed(row_cells(buffer, row, x - 2..rule_end))),
        "nothing in the bracket is reversed:\n{screen}"
    );
}

/// Asserts that the one-line item whose line starts with `needle` (e.g. a
/// chooser's `"2 revise"`) is the bracketed one, its rules checked from the
/// bar's column over the first four columns of the content: cyan — bold
/// without colour — when `focused`, grey — dim without colour — otherwise.
/// Nothing on its line is reversed (R7).
fn assert_bracketed(buffer: &Buffer, needle: &str, focused: bool, no_color: bool) {
    let y = row_of(buffer, needle);
    let x = column_in(buffer, y, needle);
    let y = u16::try_from(y).expect("a row");
    assert_bracket(buffer, (x, y), 1, x + 4, marks(focused, no_color));
    assert!(
        none_reversed(row_cells(buffer, y, 0..buffer.area.width)),
        "{needle:?}: nothing on the selected line is reversed:\n{}",
        screen(buffer)
    );
}

fn assert_action_number(buffer: &Buffer, needle: &str, number: usize) {
    let row = row_of(buffer, needle);
    let line = &rows(buffer)[row];
    assert!(line.contains(&format!("| {number} |")), "{line}");
}

/// The whole screen of `app`, `width`×`height`, in colour or — with
/// `no_color` — without.
fn draw_buffer(app: &mut TaskApp, width: u16, height: u16, no_color: bool) -> Buffer {
    draw(width, height, |frame| {
        layout::render(frame, app, &Styles::new(no_color), Instant::now())
    })
}

/// Asserts that the list entry whose line starts with `needle` is drawn
/// without the bar beside it.
fn assert_not_bracketed(buffer: &Buffer, needle: &str) {
    let y = row_of(buffer, needle);
    let x = column_in(buffer, y, needle);
    let y = u16::try_from(y).expect("a row");
    assert_eq!(
        buffer[(x - 2, y)].symbol(),
        " ",
        "{needle:?} has no bar:\n{}",
        screen(buffer)
    );
}

/// The corners of a rounded frame.
const ROUNDED: [&str; 4] = ["╭", "╮", "╰", "╯"];

/// The screen without its first two rows (the header and the frames' titles),
/// where a `▸` could only be a selection marker.
fn below_the_frame_title(buffer: &Buffer) -> String {
    rows(buffer)[2..].join("\n")
}

/// The corners of the frame from column `left` to `right`, rows `top` to
/// `bottom`, with their colours.
fn corners(buffer: &Buffer, left: u16, right: u16, top: u16, bottom: u16) -> Vec<(String, Color)> {
    vec![
        cell_at(buffer, left, top),
        cell_at(buffer, right, top),
        cell_at(buffer, left, bottom),
        cell_at(buffer, right, bottom),
    ]
}

/// Asserts the frame from `left` to `right` (rows 1 to `height - 2`, between
/// the header and the bar) has `shape` corners in `colour`.
fn assert_frame(buffer: &Buffer, left: u16, right: u16, shape: [&str; 4], colour: Color) {
    let bottom = buffer.area.height - 2;
    let found = corners(buffer, left, right, 1, bottom);
    let expected: Vec<(String, Color)> = shape
        .iter()
        .map(|corner| (corner.to_string(), colour))
        .collect();
    assert_eq!(
        found,
        expected,
        "frame {left}..={right}:\n{}",
        screen(buffer)
    );
}

fn review_chooser() -> Chooser {
    Chooser::new(vec![
        ChooserItem::new("approved", "Akceptuje wynik agenta."),
        ChooserItem::new(
            "revise",
            "Zakres wymaga poprawy. Agent planu wznowi swoją sesję z Twoim komentarzem.",
        )
        .resuming("plan")
        .recommended()
        .with_field(
            ChooserField::multiline("Komentarz dla agenta")
                .required()
                .with_value("Uwzględnij migrację"),
        ),
        ChooserItem::new("reject", "Odrzuca zadanie.").irreversible(),
    ])
    .selecting(1)
}

fn draw_chooser(width: u16, height: u16, chooser: &mut Chooser, styles: &Styles) -> Buffer {
    draw_chooser_focused(width, height, chooser, styles, true)
}

fn draw_chooser_focused(
    width: u16,
    height: u16,
    chooser: &mut Chooser,
    styles: &Styles,
    focused: bool,
) -> Buffer {
    draw(width, height, |frame| {
        chooser::render(
            frame,
            Rect::new(0, 0, width, height),
            chooser,
            styles,
            focused,
        )
    })
}

/// Six plain entries, `out1` … `out6`, on the first.
fn six_outputs() -> Chooser {
    Chooser::new(
        (1..=6)
            .map(|index| ChooserItem::new(format!("out{index}"), format!("Wyjście {index}.")))
            .collect(),
    )
}

fn press_chooser(chooser: &mut Chooser, code: crossterm::event::KeyCode, times: usize) {
    for _ in 0..times {
        chooser.on_key(keymap::Key::code(code).to_event());
    }
}

mod chooser_render {
    use super::*;

    #[test]
    fn wide_area_puts_the_description_beside_the_list() {
        let mut chooser = review_chooser();
        let buffer = draw_chooser(100, 16, &mut chooser, &Styles::new(false));
        let screen = screen(&buffer);
        assert!(screen.contains("Akcje") && screen.contains("Opis"));
        assert!(screen.contains("2 revise ↩ ◆"), "{screen}");
        for marker in ["↩", "◆", "!"] {
            assert!(screen.contains(marker), "{marker} is missing:\n{screen}");
        }
        assert!(screen.contains("↩ wraca do agenta: plan"));
        assert!(screen.contains("◆ rekomendacja dyrygenta"));

        assert!(
            row_of(&buffer, "↩ wraca do agenta") <= row_of(&buffer, "1 approved"),
            "list and description share rows when wide:\n{screen}"
        );
        assert!(screen.contains("Komentarz dla agenta (wymagane)"));
        assert!(screen.contains("> Uwzględnij migrację"));
    }

    #[test]
    fn narrow_area_puts_the_description_under_the_list() {
        let mut chooser = review_chooser();
        let buffer = draw_chooser(40, 20, &mut chooser, &Styles::new(false));
        let list = row_of(&buffer, "reject");
        let description = row_of(&buffer, "Opis");
        assert!(
            description > list,
            "the description pane starts below the last list row:\n{}",
            screen(&buffer)
        );
        assert!(screen(&buffer).contains("↩ wraca do agenta: plan"));
        assert!(
            !rows(&buffer)[list].contains("Opis"),
            "no pane beside the list when narrow"
        );
    }

    #[test]
    fn the_cursor_shows_in_the_focused_field() {
        let mut chooser = review_chooser();
        chooser.on_key(keymap::Key::code(crossterm::event::KeyCode::Tab).to_event());
        let buffer = draw_chooser(100, 16, &mut chooser, &Styles::new(false));
        assert!(screen(&buffer).contains("> Uwzględnij migrację"));
        assert!(
            !screen(&buffer).contains('▏'),
            "no cursor character in the text"
        );
        assert!(cursor_follows(&buffer, "> Uwzględnij migrację"));
    }

    /// One entry with `field`, the keys in it, in the list layout or in the
    /// layout of a form of inputs.
    fn typing_into(field: ChooserField, inputs_layout: bool) -> Chooser {
        let chooser = Chooser::new(vec![ChooserItem::new("go", "Start.").with_field(field)]);
        let mut chooser = if inputs_layout {
            chooser.with_inputs_layout()
        } else {
            chooser
        };
        chooser.enter_fields();
        chooser
    }

    #[test]
    fn after_a_long_paste_the_field_shows_its_last_line_and_the_cursor() {
        let text: Vec<String> = (1..=200).map(|line| format!("linia {line}")).collect();
        for inputs_layout in [false, true] {
            let mut chooser = typing_into(ChooserField::multiline("prompt"), inputs_layout);
            assert!(chooser.paste(&text.join("\n")));
            let buffer = draw_chooser(100, 20, &mut chooser, &Styles::new(false));
            let screen = screen(&buffer);
            assert!(
                screen.contains("linia 200") && cursor_follows(&buffer, "linia 200"),
                "inputs layout {inputs_layout}:\n{screen}"
            );
        }
    }

    #[test]
    fn the_cursor_after_a_pasted_tab_is_after_the_text_it_follows() {
        for inputs_layout in [false, true] {
            for field in [
                ChooserField::multiline("prompt"),
                ChooserField::text("ticket"),
            ] {
                let mut chooser = typing_into(field, inputs_layout);
                assert!(chooser.paste("a\tb"));
                let buffer = draw_chooser(100, 20, &mut chooser, &Styles::new(false));
                let screen = screen(&buffer);
                assert!(
                    screen.contains("a   b") && cursor_follows(&buffer, "a   b"),
                    "inputs layout {inputs_layout}:\n{screen}"
                );
            }
        }
    }

    #[test]
    fn lines_pasted_into_a_one_line_field_draw_as_one_line() {
        for inputs_layout in [false, true] {
            let mut chooser = typing_into(ChooserField::text("ticket"), inputs_layout);
            assert!(chooser.paste("x\ny\r\nz"));
            let buffer = draw_chooser(100, 20, &mut chooser, &Styles::new(false));
            let screen = screen(&buffer);
            assert!(
                screen.contains("x y z") && cursor_follows(&buffer, "x y z"),
                "inputs layout {inputs_layout}:\n{screen}"
            );
            assert!(
                !rows(&buffer)
                    .iter()
                    .any(|row| row.trim_start().starts_with('y')),
                "{screen}"
            );
        }
    }

    #[test]
    fn an_unavailable_action_says_why_in_words() {
        let mut chooser =
            Chooser::new(vec![ChooserItem::new("pauza", "Wstrzymuje zadanie.")
                .unavailable("zadanie jest w kolejce")]);
        let buffer = draw_chooser(100, 10, &mut chooser, &Styles::new(true));
        assert!(screen(&buffer).contains("niedostępne: zadanie jest w kolejce"));
    }

    #[test]
    fn text_from_outside_is_sanitised() {
        let mut chooser = Chooser::new(vec![ChooserItem::new(
            "ok\u{1b}[31mred",
            "line\u{7}one\u{1b}]0;title\u{7}",
        )
        .with_field(ChooserField::text("name\u{1b}[2J").with_value("v\u{1b}[0malue"))]);
        let buffer = draw_chooser(100, 12, &mut chooser, &Styles::new(false));
        assert!(
            buffer
                .content()
                .iter()
                .all(|cell| !cell.symbol().contains('\u{1b}')),
            "no escape reaches the terminal"
        );
        assert!(screen(&buffer).contains("line"));
    }

    #[test]
    fn the_selection_is_a_bracket_in_colour_and_in_no_color_and_markers_survive() {
        for no_color in [false, true] {
            for focused in [true, false] {
                let mut chooser = review_chooser();
                let styles = Styles::new(no_color);
                let buffer = draw_chooser_focused(100, 20, &mut chooser, &styles, focused);
                let screen = screen(&buffer);
                for marker in ["↩", "◆", "!"] {
                    assert!(
                        screen.contains(marker),
                        "NO_COLOR={no_color}: {marker} lost"
                    );
                }
                assert!(!screen.contains('▸'), "no cursor glyph:\n{screen}");
                assert_bracketed(&buffer, "2 revise", focused, no_color);
                assert_not_bracketed(&buffer, "1 approved");
                assert_not_bracketed(&buffer, "3 reject");
                assert!(
                    none_reversed(buffer.content()),
                    "NO_COLOR={no_color}: nothing is reversed"
                );
                if no_color {
                    assert!(
                        no_colour_but_the_selection_fill(buffer.content(), &styles),
                        "NO_COLOR draws no colour but the selection fill"
                    );
                }
            }
        }
    }

    /// The entries stack directly under the frame; only the highlighted one
    /// has a rule row above and below it.
    #[test]
    fn the_entries_stack_and_only_the_highlighted_one_has_rules() {
        let mut chooser = review_chooser();
        let buffer = draw_chooser(100, 20, &mut chooser, &Styles::new(false));
        let first = row_of(&buffer, "1 approved");
        assert_eq!(first, 1, "frame, entry:\n{}", screen(&buffer));
        assert_eq!(row_of(&buffer, "2 revise"), first + 2, "rule, entry");
        assert_eq!(row_of(&buffer, "3 reject"), first + 4, "rule, entry");
        press_chooser(&mut chooser, crossterm::event::KeyCode::Up, 1);
        let buffer = draw_chooser(100, 20, &mut chooser, &Styles::new(false));
        assert_eq!(row_of(&buffer, "1 approved"), 2, "rule, entry");
        assert_eq!(row_of(&buffer, "2 revise"), 4, "rule, entry");
        assert_eq!(row_of(&buffer, "3 reject"), 5, "adjacent to 2");
    }

    /// R12: with the keys in the fields the bracket on the list is grey.
    #[test]
    fn with_the_focus_in_the_fields_the_bracket_is_grey() {
        let mut chooser = review_chooser();
        press_chooser(&mut chooser, crossterm::event::KeyCode::Tab, 1);
        let buffer = draw_chooser(100, 20, &mut chooser, &Styles::new(false));
        assert_bracketed(&buffer, "2 revise", false, false);
    }

    /// R11, R17: a list longer than its pane scrolls inside it, and the
    /// highlighted entry is there with both of its rules.
    #[test]
    fn a_long_list_scrolls_inside_its_pane() {
        let mut chooser = six_outputs();
        // Three entries, the two rules, and the frame.
        let buffer = draw_chooser(100, 7, &mut chooser, &Styles::new(false));
        assert_bracketed(&buffer, "1 out1", true, false);
        assert!(!screen(&buffer).contains("out4"), "{}", screen(&buffer));
        press_chooser(&mut chooser, crossterm::event::KeyCode::Down, 4);
        let buffer = draw_chooser(100, 7, &mut chooser, &Styles::new(false));
        assert_bracketed(&buffer, "5 out5", true, false);
        assert!(!screen(&buffer).contains("1 out1"), "{}", screen(&buffer));
    }

    /// Stacked below 64 columns, the list keeps room for one entry with its
    /// bracket, and the description gives way first.
    #[test]
    fn stacked_the_list_keeps_one_bracketed_entry_and_the_description_shrinks() {
        let mut chooser = six_outputs();
        press_chooser(&mut chooser, crossterm::event::KeyCode::Down, 3);
        let roomy = draw_chooser(40, 24, &mut chooser, &Styles::new(false));
        assert_bracketed(&roomy, "4 out4", true, false);
        assert!(screen(&roomy).contains("Wyjście 4."), "{}", screen(&roomy));

        for height in [8, 5] {
            let buffer = draw_chooser(40, height, &mut chooser, &Styles::new(false));
            assert_bracketed(&buffer, "4 out4", true, false);
            let list_bottom = row_of(&buffer, "4 out4") + 2;
            assert!(
                rows(&buffer)[list_bottom].starts_with('╰'),
                "height {height}: the list pane ends under the bracket:\n{}",
                screen(&buffer)
            );
        }
    }

    /// Stacked, a description that would get less than three rows — a bare
    /// frame with nothing inside — gives them all to the list.
    #[test]
    fn stacked_a_description_without_room_for_a_line_is_left_out() {
        let mut chooser = six_outputs();
        press_chooser(&mut chooser, crossterm::event::KeyCode::Down, 3);
        for height in [6, 7] {
            let buffer = draw_chooser(40, height, &mut chooser, &Styles::new(false));
            let screen = screen(&buffer);
            assert_bracketed(&buffer, "4 out4", true, false);
            assert!(
                !screen.contains("Opis"),
                "height {height}: no description frame:\n{screen}"
            );
            let last = usize::from(height) - 1;
            assert!(
                rows(&buffer)[last].starts_with('╰'),
                "height {height}: the list fills the area:\n{screen}"
            );
        }
    }

    /// R13: an entry that cannot be chosen stays grey when highlighted, and
    /// its bracket follows the focus like any other.
    #[test]
    fn a_highlighted_unavailable_entry_is_grey_inside_its_bracket() {
        for focused in [true, false] {
            let mut chooser = Chooser::new(vec![
                ChooserItem::new("wznów", "Wznawia zadanie."),
                ChooserItem::new("pauza", "Wstrzymuje zadanie.")
                    .unavailable("zadanie jest w kolejce"),
            ])
            .selecting(1);
            let styles = Styles::new(false);
            let buffer = draw_chooser_focused(100, 10, &mut chooser, &styles, focused);
            assert_bracketed(&buffer, "2 pauza", focused, false);
            let y = row_of(&buffer, "2 pauza");
            let x = column_in(&buffer, y, "pauza");
            let y = u16::try_from(y).unwrap();
            assert_eq!(cell_at(&buffer, x, y).1, Color::DarkGray, "grey label");
        }
    }

    /// R17: rebuilding the chooser over the same entries keeps the highlighted
    /// entry and where the list was scrolled.
    #[test]
    fn a_rebuild_keeps_the_selection_and_the_scroll() {
        let mut chooser = six_outputs();
        press_chooser(&mut chooser, crossterm::event::KeyCode::Down, 5);
        draw_chooser(100, 9, &mut chooser, &Styles::new(false));
        press_chooser(&mut chooser, crossterm::event::KeyCode::Up, 1);
        let before = draw_chooser(100, 9, &mut chooser, &Styles::new(false));
        let items = chooser.items().to_vec();
        let mut chooser = chooser.rebuilt(items);
        let after = draw_chooser(100, 9, &mut chooser, &Styles::new(false));
        assert_eq!(screen(&before), screen(&after));
        assert_bracketed(&after, "5 out5", true, false);
        assert!(screen(&after).contains("6 out6"), "{}", screen(&after));
    }

    #[test]
    fn markers_keep_their_own_role_colour_inside_the_selected_row() {
        let mut chooser = review_chooser();
        let buffer = draw_chooser(100, 16, &mut chooser, &Styles::new(false));
        let selected = row_of(&buffer, "revise");
        let width = usize::from(buffer.area.width);
        let colour_of = |symbol: &str| {
            buffer.content()[selected * width..(selected + 1) * width]
                .iter()
                .find(|cell| cell.symbol() == symbol)
                .map(|cell| cell.fg)
        };
        assert_eq!(colour_of("◆"), Some(Color::Magenta));
        assert_eq!(colour_of("↩"), Some(Color::Reset));
        let reject = row_of(&buffer, "3 reject");
        let bang = column_in(&buffer, reject, "!");
        assert_eq!(
            cell_at(&buffer, bang, u16::try_from(reject).unwrap()).1,
            Color::Red
        );
    }
}

mod shortcut_bar_and_help {
    use super::*;

    fn line_text(line: &ratatui::text::Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn the_panel_bar_shows_its_keys_with_key_caps() {
        let styles = Styles::new(false);
        let line = keymap::hint_line(Context::Panel, &styles, 120);
        let text = line_text(&line);
        for expected in ["Tab", "Enter", "Ctrl+J", "Esc"] {
            assert!(text.contains(expected), "{expected}: {text}");
        }
        let caps = line
            .spans
            .iter()
            .filter(|span| span.style == styles.key_cap())
            .count();
        assert!(caps >= 4, "every key sits on a cap");
    }

    #[test]
    fn a_narrow_bar_drops_hints_from_the_end_not_the_start() {
        let wide = line_text(&keymap::hint_line(Context::List, &Styles::new(true), 200));
        let narrow = line_text(&keymap::hint_line(Context::List, &Styles::new(true), 30));
        assert!(narrow.chars().count() <= 30, "{narrow:?}");
        assert!(wide.starts_with(narrow.trim_end()));
    }

    #[test]
    fn help_overlay_lists_every_key_of_the_list_context() {
        let buffer = draw(100, 50, |frame| {
            keymap::render_help(
                frame,
                Rect::new(0, 0, 100, 50),
                Context::List,
                &Styles::new(false),
            )
        });
        let screen = screen(&buffer);
        assert!(screen.contains("Pomoc"));
        // A description too long for the overlay wraps onto the next row, so
        // the text is read with the frame taken off and the rows joined.
        let text = rows(&buffer)
            .iter()
            .map(|row| row.trim_matches(|c: char| c == '│' || c.is_whitespace()))
            .collect::<Vec<_>>()
            .join(" ");
        for description in keymap::help_entries(Context::List)
            .iter()
            .map(|entry| entry.description)
        {
            assert!(
                text.contains(description),
                "{description:?} is missing:\n{screen}"
            );
        }
    }
}

mod connection_render {
    use super::*;

    fn header(connection: &Connection, now: Instant, styles: &Styles) -> (String, Buffer) {
        let buffer = draw(40, 1, |frame| {
            frame.render_widget(
                Paragraph::new(ratatui::text::Line::from(
                    connection.status(now).span(styles),
                )),
                Rect::new(0, 0, 40, 1),
            )
        });
        (rows(&buffer)[0].trim_end().to_string(), buffer)
    }

    #[test]
    fn each_state_has_icon_and_words_and_its_role_colour() {
        let styles = Styles::new(false);
        let start = Instant::now();
        let mut connection = Connection::new("local");
        connection.on_data(start);
        let (text, buffer) = header(&connection, start, &styles);
        assert_eq!(text, "● live");
        assert_eq!(buffer.content()[0].fg, Color::Green);

        connection.on_failure(start, Some(Duration::from_secs(12)));
        let (text, buffer) = header(&connection, start, &styles);
        assert_eq!(text, "◌ ponawiam (12s)");
        assert_eq!(buffer.content()[0].fg, Color::Yellow);

        connection.on_failure(start, None);
        let (text, buffer) = header(&connection, start, &styles);
        assert_eq!(text, "✖ brak połączenia");
        assert_eq!(buffer.content()[0].fg, Color::Red);
    }

    #[test]
    fn without_colour_the_words_and_icons_still_tell_the_states_apart() {
        let styles = Styles::new(true);
        let start = Instant::now();
        let mut connection = Connection::new("local");
        connection.on_data(start);
        let live = header(&connection, start, &styles).0;
        connection.on_failure(start, None);
        let down = header(&connection, start, &styles).0;
        assert_ne!(live, down);
        assert!(down.starts_with('✖') && live.starts_with('●'));
    }
}

/// R17: views name roles, not colours. The whole of `src/tui` is checked —
/// the directory is listed, so a new file is covered the day it appears.
mod colour_discipline {
    use super::*;

    /// The one module that may name a colour.
    const ROLE_MODULE: &str = "style.rs";

    fn tui_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/tui")
    }

    /// Lines of `sources` that name a colour, as `file:line: text`.
    fn colour_references(directory: &std::path::Path, sources: &[String]) -> Vec<String> {
        let mut found = Vec::new();
        for source in sources
            .iter()
            .filter(|source| source.as_str() != ROLE_MODULE)
        {
            let text = fs::read_to_string(directory.join(source))
                .unwrap_or_else(|error| panic!("{source}: {error}"));
            for (number, line) in text.lines().enumerate() {
                // `Color::Reset` hands a cell back to the terminal's own
                // colours: it names no colour.
                if line.replace("Color::Reset", "").contains("Color::") {
                    found.push(format!("{source}:{}: {}", number + 1, line.trim()));
                }
            }
        }
        found
    }

    /// Every `.rs` file under `directory`, relative to it, sorted.
    fn rust_sources(directory: &std::path::Path) -> Vec<String> {
        fn walk(directory: &std::path::Path, prefix: &str, found: &mut Vec<String>) {
            for entry in fs::read_dir(directory).expect("list the directory") {
                let entry = entry.expect("a directory entry");
                let name = entry.file_name().to_string_lossy().into_owned();
                let relative = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}/{name}")
                };
                if entry.path().is_dir() {
                    walk(&entry.path(), &relative, found);
                } else if name.ends_with(".rs") {
                    found.push(relative);
                }
            }
        }
        let mut found = Vec::new();
        walk(directory, "", &mut found);
        found.sort();
        found
    }

    #[test]
    fn nothing_in_the_tui_names_a_colour_outside_the_role_module() {
        let sources = rust_sources(&tui_dir());
        for expected in [
            "transcript.rs",
            "conversation.rs",
            "scroll.rs",
            "task_app/mod.rs",
            "task_app/tabs/info.rs",
        ] {
            assert!(
                sources.iter().any(|source| source == expected),
                "the listing must reach {expected}: {sources:?}"
            );
        }
        assert_eq!(
            colour_references(&tui_dir(), &sources),
            Vec::<String>::new()
        );
    }

    #[test]
    fn the_scan_finds_a_colour_when_one_is_there_and_skips_the_role_module() {
        let directory =
            std::env::temp_dir().join(format!("narsor-colour-scan-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("a scratch directory");
        fs::write(
            directory.join("view.rs"),
            "let x = Color::Red;\ncell.bg = Color::Reset;\n",
        )
        .expect("write");
        fs::write(directory.join("style.rs"), "let y = Color::Green;\n").expect("write");
        let sources = vec!["view.rs".to_string(), "style.rs".to_string()];
        let found = colour_references(&directory, &sources);
        let _ = fs::remove_dir_all(&directory);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].starts_with("view.rs:1:"));
    }

    /// Text stays in the 16 ANSI colours, so it inherits the terminal theme.
    /// A colour outside them — RGB or one of the 256 — only ever fills a
    /// background: the task header, the answer panel, the selected row.
    #[test]
    fn the_role_module_is_where_the_colours_are() {
        let style = fs::read_to_string(tui_dir().join(ROLE_MODULE)).expect("style.rs");
        assert!(style.contains("Color::Cyan"));
        for line in style
            .lines()
            .filter(|line| line.contains("Color::Rgb") || line.contains("Color::Indexed"))
        {
            assert!(
                line.contains(".bg(") && !line.contains(".fg("),
                "only the 16 ANSI colours for text: {}",
                line.trim()
            );
        }
    }
}

/// U8: the task application's screens — the wide and the narrow layout, the
/// request for room, the connection states and the move preview.
mod task_app_render {
    use super::*;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use narsor::api::{ApiClient, ClientError};
    use narsor::tui::task_app::{layout, TaskApp};
    use narsor_engine::domain::ExecutionStatus;
    use narsor_engine::orchestration::AttentionKind;

    use crate::task_app_support::{
        listing, queued, titled, waiting, waiting_for_capacity, with_controller, with_runner,
    };

    fn app() -> TaskApp {
        let client = ApiClient::new("127.0.0.1:9".parse().unwrap(), "token");
        TaskApp::new(client, "local", None)
    }

    fn press(app: &mut TaskApp, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    /// The screen of the origin's mock-up: a running task, one that waits for
    /// an approval, a failed one, two queued, a finished one.
    fn busy_app() -> TaskApp {
        let mut app = app();
        let controlled = |id: &str, runner_state: &str, rank| {
            with_controller(crate::task_app_support::task(
                id,
                "local",
                runner_state,
                rank,
            ))
        };
        app.load(listing(
            vec![
                titled(
                    with_runner(
                        controlled("a", "running", 1),
                        ExecutionStatus::Running,
                        "edit",
                    ),
                    "NAR-14",
                ),
                titled(
                    waiting(
                        with_runner(
                            controlled("b", "running", 2),
                            ExecutionStatus::Running,
                            "approval",
                        ),
                        AttentionKind::Approval,
                    ),
                    "NAR-12",
                ),
                titled(
                    waiting(controlled("c", "unknown", 3), AttentionKind::Problem),
                    "NAR-9",
                ),
                titled(waiting_for_capacity(queued("d", 4), 1, 2, 3), "NAR-16"),
                titled(waiting_for_capacity(queued("e", 5), 2, 2, 3), "NAR-17"),
                titled(
                    with_runner(
                        controlled("f", "running", 6),
                        ExecutionStatus::Completed,
                        "done",
                    ),
                    "NAR-8",
                ),
            ],
            "c",
        ));
        app
    }

    /// The screen row whose list column (the first 28) holds `needle`.
    fn list_row(buffer: &Buffer, needle: &str) -> usize {
        rows(buffer)
            .iter()
            .position(|row| {
                row.chars()
                    .take(usize::from(layout::LIST_WIDTH))
                    .collect::<String>()
                    .contains(needle)
            })
            .unwrap_or_else(|| panic!("{needle:?} is not in the list:\n{}", screen(buffer)))
    }

    fn render(width: u16, height: u16, app: &mut TaskApp, styles: &Styles) -> Buffer {
        draw(width, height, |frame| {
            layout::render(frame, app, styles, Instant::now())
        })
    }

    #[test]
    fn the_wide_screen_has_the_header_the_list_and_the_details() {
        let mut app = busy_app();
        let buffer = render(120, 30, &mut app, &Styles::new(false));
        let screen = screen(&buffer);

        let header = &rows(&buffer)[0];
        for expected in ["narsor · local", "● live", "sloty 2/3", "? 1", "! 1"] {
            assert!(header.contains(expected), "{expected:?} in {header:?}");
        }
        assert!(rows(&buffer)[1].contains("Zadania"));
        assert!(
            rows(&buffer)[1].contains("NAR-14"),
            "the details name the selected task"
        );

        let list_rows: Vec<String> = rows(&buffer)
            .iter()
            .map(|row| row.chars().skip(3).take(25).collect::<String>())
            .collect();
        let find = |needle: &str| {
            list_rows
                .iter()
                .position(|row| row.contains(needle))
                .unwrap()
        };
        assert!(
            list_rows[find("NAR-14")].starts_with("  ●  NAR-14"),
            "{:?}",
            list_rows[find("NAR-14")]
        );
        assert!(list_rows[find("NAR-12")].starts_with("? ●  NAR-12"));
        assert!(list_rows[find("NAR-9 ")].starts_with("! ✗  NAR-9"));
        assert!(list_rows[find("NAR-16")].starts_with("  ◷  NAR-16"));
        // The label is on the second line, under the title.
        assert!(list_rows[find("NAR-16") + 1].starts_with("     #1"));
        assert!(list_rows[find("NAR-17") + 1].starts_with("     #2"));
        assert!(list_rows[find("NAR-8 ")].starts_with("  ✓  NAR-8"));
        assert!(list_rows[find("NAR-12") + 1].starts_with("     approval"));
        assert!(
            find("NAR-14") < find("NAR-12")
                && find("NAR-12") < find("NAR-9 ")
                && find("NAR-17") < find("NAR-8 "),
            "the rows are in the server's order"
        );

        let bar = rows(&buffer).last().cloned().unwrap();
        let _ = bar;
        assert!(
            screen.contains("następny czekający"),
            "the bar shows the list keys:\n{screen}"
        );
        assert!(
            screen.contains("Przebieg") && screen.contains("Zdarzenia"),
            "the details have their tabs:\n{screen}"
        );
    }

    #[test]
    fn the_list_column_is_28_wide_and_the_details_begin_right_after() {
        let mut app = busy_app();
        let buffer = render(120, 30, &mut app, &Styles::new(true));
        let top = &rows(&buffer)[1];
        let corners: Vec<(usize, char)> = top
            .chars()
            .enumerate()
            .filter(|(_, c)| "╭╮".contains(*c))
            .collect();
        let list = usize::from(layout::LIST_WIDTH);
        assert_eq!(
            corners,
            vec![(0, '╭'), (list - 1, '╮'), (list, '╭'), (119, '╮')],
            "{top:?}"
        );
    }

    /// AE1: the panel with the keys has a thick frame in the focus colour,
    /// the other a thin grey one; `Enter` hands the focus over.
    #[test]
    fn the_focused_panel_has_a_thick_cyan_frame_and_the_other_a_thin_grey_one() {
        let mut app = busy_app();
        let list_right = layout::LIST_WIDTH - 1;
        let detail_left = layout::LIST_WIDTH;
        let buffer = render(120, 30, &mut app, &Styles::new(false));
        assert_frame(&buffer, 0, list_right, ROUNDED, Color::Cyan);
        assert_frame(&buffer, detail_left, 119, ROUNDED, Color::DarkGray);
        assert_eq!(cell_at(&buffer, 0, 15), ("│".to_string(), Color::Cyan));
        assert_eq!(
            cell_at(&buffer, 119, 15),
            ("│".to_string(), Color::DarkGray)
        );

        press(&mut app, KeyCode::Enter);
        let buffer = render(120, 30, &mut app, &Styles::new(false));
        assert_frame(&buffer, 0, list_right, ROUNDED, Color::DarkGray);
        assert_frame(&buffer, detail_left, 119, ROUNDED, Color::Cyan);
        // The title has the colour of its frame and, with the focus, is bold.
        let title = column_of(&buffer, 1, detail_left, "N").unwrap();
        assert_eq!(cell_at(&buffer, title, 1).1, Color::Cyan);
        assert!(bold_at(&buffer, title, 1));
        assert_eq!(cell_at(&buffer, 2, 1), ("Z".to_string(), Color::DarkGray));
        assert!(!bold_at(&buffer, 2, 1));
    }

    /// AE2 without colour: the `▸` marker and the bold title tell the panels
    /// apart.
    #[test]
    fn without_colour_the_focused_panel_has_a_marker_and_a_bold_title() {
        let mut app = busy_app();
        let buffer = render(120, 30, &mut app, &Styles::new(true));
        let list_right = layout::LIST_WIDTH - 1;
        assert_frame(&buffer, 0, list_right, ROUNDED, Color::Reset);
        assert_frame(&buffer, layout::LIST_WIDTH, 119, ROUNDED, Color::Reset);
        assert_eq!(cell_at(&buffer, 1, 1).0, "▸", "{}", rows(&buffer)[1]);
        assert_eq!(cell_at(&buffer, 2, 1).0, " ", "{}", rows(&buffer)[1]);
        assert!(bold_at(&buffer, 1, 1), "the active title is bold");
        let title = column_of(&buffer, 1, layout::LIST_WIDTH, "N").unwrap();
        assert!(!bold_at(&buffer, title, 1), "the other title is not");
        assert!(
            !rows(&buffer)[1]
                .chars()
                .skip(layout::LIST_WIDTH as usize)
                .any(|c| c == '▸'),
            "the other panel has no marker"
        );
        assert!(buffer
            .content()
            .iter()
            .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset));
    }

    #[test]
    fn the_header_is_the_first_row_with_no_frame_around_the_screen() {
        let mut app = busy_app();
        for width in [60, 80, 120] {
            let buffer = render(width, 24, &mut app, &Styles::new(false));
            let header = &rows(&buffer)[0];
            assert!(header.contains("narsor"), "{width}: {header:?}");
            assert!(header.contains("? 1 · ! 1"), "{width}: {header:?}");
            assert!(
                !header.contains(['╭', '╮', '╰', '╯', '│', '─']),
                "{width}: {header:?}"
            );
        }
    }

    /// R5: in the narrow layout the one panel shown has the focus frame.
    #[test]
    fn at_80_columns_the_one_panel_shown_has_the_thick_frame() {
        let mut app = busy_app();
        let buffer = render(80, 24, &mut app, &Styles::new(false));
        assert!(rows(&buffer)[1].contains("Zadania"));
        assert_frame(&buffer, 0, 79, ROUNDED, Color::Cyan);

        press(&mut app, KeyCode::Enter);
        let buffer = render(80, 24, &mut app, &Styles::new(false));
        assert!(screen(&buffer).contains("Przebieg"));
        assert_frame(&buffer, 0, 79, ROUNDED, Color::Cyan);
    }

    #[test]
    fn at_60x20_the_screen_and_the_help_fit() {
        let mut app = busy_app();
        let buffer = render(60, 20, &mut app, &Styles::new(false));
        assert!(rows(&buffer)[0].contains("narsor"));
        assert_frame(&buffer, 0, 59, ROUNDED, Color::Cyan);
        press(&mut app, KeyCode::Char('?'));
        let buffer = render(60, 20, &mut app, &Styles::new(false));
        let help = row_of(&buffer, "Pomoc");
        assert!(help >= 1, "the help leaves the header alone");
        let bar = rows(&buffer).last().cloned().unwrap();
        assert!(!bar.contains("Pomoc"), "{bar}");
    }

    /// AE1: the help window and the other views draw no square or thick
    /// corner or edge any more.
    #[test]
    fn no_view_draws_a_square_or_thick_frame() {
        const OLD: [char; 9] = ['┌', '┐', '└', '┘', '┏', '┓', '┗', '┛', '┃'];
        let mut app = busy_app();
        for no_color in [false, true] {
            let buffer = render(120, 30, &mut app, &Styles::new(no_color));
            assert!(!screen(&buffer).contains(OLD), "{}", screen(&buffer));
            press(&mut app, KeyCode::Enter);
            let buffer = render(120, 30, &mut app, &Styles::new(no_color));
            assert!(!screen(&buffer).contains(OLD), "{}", screen(&buffer));
            press(&mut app, KeyCode::Esc);
        }
        press(&mut app, KeyCode::Char('?'));
        let buffer = render(120, 30, &mut app, &Styles::new(false));
        let help = row_of(&buffer, "Pomoc");
        assert!(rows(&buffer)[help].contains('╭'), "{}", screen(&buffer));
        assert!(!screen(&buffer).contains(OLD), "{}", screen(&buffer));
        let mut chooser = review_chooser();
        let buffer = draw_chooser(80, 24, &mut chooser, &Styles::new(false));
        assert!(!screen(&buffer).contains(OLD), "{}", screen(&buffer));
        assert!(screen(&buffer).contains('╭'));
    }

    #[test]
    fn at_90_columns_the_list_is_full_screen_and_enter_swaps_it_for_the_details() {
        let mut app = busy_app();
        let list = render(90, 30, &mut app, &Styles::new(false));
        assert!(screen(&list).contains("NAR-17"));
        assert!(!screen(&list).contains("Przebieg"));
        assert!(screen(&list).contains("Zadania"));

        press(&mut app, KeyCode::Enter);
        let details = render(90, 30, &mut app, &Styles::new(false));
        assert!(screen(&details).contains("Przebieg"));
        assert!(!screen(&details).contains("NAR-17"));
        assert!(
            screen(&details).contains("lista"),
            "the bar offers the way back"
        );
    }

    #[test]
    fn a_small_terminal_only_asks_for_more_room() {
        let mut app = busy_app();
        let buffer = render(50, 15, &mut app, &Styles::new(false));
        let screen = screen(&buffer);
        assert!(screen.contains("powiększ okno"), "{screen}");
        assert!(screen.contains("50×15"));
        assert!(!screen.contains("NAR-14"));
    }

    #[test]
    fn without_a_connection_the_rows_are_grey_with_their_age_and_the_header_says_so() {
        let mut app = busy_app();
        app.on_stream(Some(Err(ClientError::Unavailable("reset".into()))));
        let buffer = render(120, 30, &mut app, &Styles::new(false));
        let header = &rows(&buffer)[0];
        assert!(header.contains("◌ ponawiam ("), "{header:?}");
        assert!(header.contains("stan sprzed"), "{header:?}");

        let row = list_row(&buffer, "NAR-14");
        let width = usize::from(buffer.area.width);
        let cells = &buffer.content()[row * width..(row + 1) * width];
        let title_cell = cells.iter().find(|cell| cell.symbol() == "N").unwrap();
        assert_eq!(title_cell.fg, Color::DarkGray, "stale data is grey");
        let marker = cells.iter().find(|cell| cell.symbol() == "●").unwrap();
        assert_eq!(marker.fg, Color::DarkGray);
    }

    #[test]
    fn a_connecting_screen_says_which_server_and_has_no_rows() {
        let mut app = app();
        let buffer = render(100, 24, &mut app, &Styles::new(false));
        assert!(
            rows(&buffer)[0].contains("◌ łączę z local"),
            "{:?}",
            rows(&buffer)[0]
        );
        assert!(screen(&buffer).contains("brak danych"));
    }

    /// AE1: the selected task has a cyan bar beside both lines while the list has the focus,
    /// and its markers keep their colours (R13).
    #[test]
    fn live_markers_keep_their_colours_inside_the_barred_selection() {
        let mut app = busy_app();
        press(&mut app, KeyCode::Char('j'));
        let buffer = render(120, 30, &mut app, &Styles::new(false));
        let row = list_row(&buffer, "NAR-12");
        let y = row as u16;
        assert_eq!(
            cell_at(&buffer, 3, y),
            ("?".to_string(), Color::Yellow),
            "the marker keeps its role colour"
        );
        assert_eq!(cell_at(&buffer, 5, y), ("●".to_string(), Color::Green));
        assert_task_bar(&buffer, row, marks(true, false));
        let other = list_row(&buffer, "NAR-14");
        assert_eq!(bar(&buffer, other).0, " ");
        assert_eq!(bar(&buffer, other + 1).0, " ");
        assert!(no_reversed_in_list(&buffer), "{}", screen(&buffer));
    }

    /// AE1: a task whose work ended holds no slot: the header counts only the
    /// slot of the running one, and the row says so next to its state.
    #[test]
    fn a_finished_task_without_a_slot_is_marked_and_not_counted() {
        use crate::task_app_support::{task, without_slot};
        let mut app = app();
        let finished = without_slot(titled(
            with_runner(
                with_controller(task("a", "local", "running", 1)),
                ExecutionStatus::Completed,
                "done",
            ),
            "NAR-1",
        ));
        let running = titled(
            with_runner(
                with_controller(task("b", "local", "running", 2)),
                ExecutionStatus::Running,
                "edit",
            ),
            "NAR-2",
        );
        app.load(listing(vec![finished, running], "c"));
        let buffer = render(120, 30, &mut app, &Styles::new(false));
        let header = &rows(&buffer)[0];
        assert!(header.contains("sloty 1 "), "{header:?}");
        let list_rows: Vec<String> = rows(&buffer)
            .iter()
            .map(|row| row.chars().skip(3).take(25).collect::<String>())
            .collect();
        let at = list_rows
            .iter()
            .position(|row| row.contains("NAR-1"))
            .unwrap();
        assert!(
            list_rows[at].starts_with("  ✓  NAR-1"),
            "{:?}",
            list_rows[at]
        );
        assert!(
            list_rows[at + 1].contains("bez slotu"),
            "{:?}",
            list_rows[at + 1]
        );
        let at = list_rows
            .iter()
            .position(|row| row.contains("NAR-2"))
            .unwrap();
        assert!(
            list_rows[at].starts_with("  ●  NAR-2"),
            "{:?}",
            list_rows[at]
        );
        assert!(!list_rows[at + 1].contains("bez slotu"));
    }

    /// Every state has its icon and its label; the rows keep their order.
    #[test]
    fn every_task_state_has_an_icon_and_a_label() {
        use crate::task_app_support::task;
        use narsor_engine::orchestration::TaskState;
        let cases = [
            (TaskState::Queued, "◷", "queued"),
            (TaskState::Running, "●", "running"),
            (TaskState::HumanNeeded, "◐", "awaiting input"),
            (TaskState::Paused, "‖", "paused"),
            (TaskState::Finished, "✓", "done"),
            (TaskState::Error, "✗", "failed"),
            (TaskState::Archived, "✓", "archived"),
            (TaskState::Unrecognized, "·", "unknown"),
        ];
        let mut app = app();
        let tasks = cases
            .iter()
            .enumerate()
            .map(|(index, (state, _, _))| {
                let mut one = titled(
                    with_controller(task(
                        &format!("t{index}"),
                        "local",
                        "running",
                        index as u32 + 1,
                    )),
                    &format!("TASK-{index}"),
                );
                one.state = *state;
                one.holds_slot = true;
                one
            })
            .collect();
        app.load(listing(tasks, "c"));
        let buffer = render(120, 40, &mut app, &Styles::new(false));
        let list_rows: Vec<String> = rows(&buffer)
            .iter()
            .map(|row| row.chars().skip(3).take(25).collect::<String>())
            .collect();
        let mut last = 0;
        for (index, (state, glyph, label)) in cases.iter().enumerate() {
            let at = list_rows
                .iter()
                .position(|row| row.contains(&format!("TASK-{index}")))
                .unwrap_or_else(|| panic!("{state:?} is not shown:\n{}", screen(&buffer)));
            assert!(
                list_rows[at].contains(glyph),
                "{state:?}: {:?}",
                list_rows[at]
            );
            assert!(
                list_rows[at + 1].contains(label),
                "{state:?}: {:?}",
                list_rows[at + 1]
            );
            assert!(at >= last, "the rows keep their order");
            last = at;
        }
    }

    /// A summary from a control plane that sends no state is drawn with the
    /// neutral icon, and nothing panics.
    #[test]
    fn a_summary_without_a_state_is_drawn_neutral() {
        use crate::task_app_support::task;
        let mut value = serde_json::to_value(titled(task("a", "local", "running", 1), "NAR-1"))
            .expect("a summary");
        let object = value.as_object_mut().unwrap();
        object.remove("state");
        object.remove("holds_slot");
        let old: narsor_engine::controller_protocol::TaskSummary =
            serde_json::from_value(value).expect("a summary of an older control plane");
        let mut app = app();
        app.load(listing(vec![old], "c"));
        let buffer = render(120, 30, &mut app, &Styles::new(false));
        let text = screen(&buffer);
        assert!(text.contains("·  NAR-1"), "{text}");
        assert!(text.contains("unknown"), "{text}");
    }

    /// AE5: a selected task with a problem keeps its red markers, and no cell
    /// of the list is reversed.
    #[test]
    fn a_selected_problem_keeps_its_red_markers() {
        let mut app = busy_app();
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('j'));
        let buffer = render(120, 30, &mut app, &Styles::new(false));
        let row = list_row(&buffer, "NAR-9 ");
        let y = row as u16;
        assert_eq!(cell_at(&buffer, 3, y), ("!".to_string(), Color::Red));
        assert_eq!(cell_at(&buffer, 5, y), ("✗".to_string(), Color::Red));
        assert_task_bar(&buffer, row, marks(true, false));
        assert!(no_reversed_in_list(&buffer), "{}", screen(&buffer));
    }

    /// AE1: with the focus gone to the details the bar stays where it
    /// was, grey, and no task moves.
    #[test]
    fn enter_greys_the_bar_and_moves_no_row() {
        let mut app = busy_app();
        press(&mut app, KeyCode::Char('j'));
        let titles = ["NAR-14", "NAR-12", "NAR-9 ", "NAR-16", "NAR-17", "NAR-8 "];
        let before = render(120, 30, &mut app, &Styles::new(false));
        let rows_before: Vec<usize> = titles.iter().map(|t| list_row(&before, t)).collect();

        press(&mut app, KeyCode::Enter);
        let after = render(120, 30, &mut app, &Styles::new(false));
        let rows_after: Vec<usize> = titles.iter().map(|t| list_row(&after, t)).collect();
        assert_eq!(rows_before, rows_after, "{}", screen(&after));
        let row = list_row(&after, "NAR-12");
        assert_task_bar(&after, row, marks(false, false));
    }

    /// AE1: without colour the weight of the bar tells the
    /// focus apart.
    #[test]
    fn without_colour_every_marker_and_the_bar_survives() {
        let mut app = busy_app();
        press(&mut app, KeyCode::Char('j'));
        let buffer = render(120, 30, &mut app, &Styles::new(true));
        let screen = screen(&buffer);
        for marker in ["? ●", "! ✗", "◷", "✓", "● live", "? 1", "! 1"] {
            assert!(screen.contains(marker), "{marker:?} lost:\n{screen}");
        }
        assert!(
            buffer
                .content()
                .iter()
                .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset),
            "NO_COLOR draws no colour at all"
        );
        let row = list_row(&buffer, "NAR-12");
        assert_task_bar(&buffer, row, marks(true, true));
        let y = row as u16;
        assert!(bold_at(&buffer, 1, y) && bold_at(&buffer, 1, y + 1));
        assert!(no_reversed_in_list(&buffer));

        press(&mut app, KeyCode::Enter);
        let buffer = render(120, 30, &mut app, &Styles::new(true));
        let row = list_row(&buffer, "NAR-12");
        assert_task_bar(&buffer, row, marks(false, true));
        let y = row as u16;
        let dim = |x: u16, y: u16| buffer[(x, y)].modifier.contains(Modifier::DIM);
        assert!(dim(1, y) && dim(1, y + 1));
        assert!(!bold_at(&buffer, 1, y));
        assert!(no_reversed_in_list(&buffer));
    }

    #[test]
    fn the_move_mode_shows_the_preview_and_its_own_keys() {
        let mut app = busy_app();
        // The first queued row.
        press(&mut app, KeyCode::Char('G'));
        press(&mut app, KeyCode::Char('k'));
        press(&mut app, KeyCode::Char('k'));
        press(&mut app, KeyCode::Char('m'));
        press(&mut app, KeyCode::Char('j'));
        let buffer = render(120, 30, &mut app, &Styles::new(true));
        let screen = screen(&buffer);
        assert!(screen.contains("↕"), "the moved row is marked:\n{screen}");
        assert!(
            list_row(&buffer, "NAR-17") < list_row(&buffer, "NAR-16"),
            "the preview put NAR-16 after NAR-17"
        );
        assert!(
            screen.contains("zatwierdź") && screen.contains("anuluj"),
            "{screen}"
        );
    }

    #[test]
    fn the_help_overlay_comes_from_the_key_map() {
        let mut app = busy_app();
        press(&mut app, KeyCode::Char('?'));
        let buffer = render(120, 40, &mut app, &Styles::new(false));
        let screen = screen(&buffer);
        assert!(screen.contains("Pomoc"));
        assert!(screen.contains("następne zadanie, które czeka"));
        press(&mut app, KeyCode::Char('x'));
        assert!(!app.is_help_open(), "any key closes it");
    }

    #[test]
    fn the_notification_sits_on_the_bar() {
        let mut app = busy_app();
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('K'));
        press(&mut app, KeyCode::Char('K'));
        press(&mut app, KeyCode::Char('K'));
        press(&mut app, KeyCode::Char('G'));
        press(&mut app, KeyCode::Char('K'));
        let buffer = render(120, 30, &mut app, &Styles::new(false));
        // NAR-8 is finished, so it cannot be moved.
        assert!(
            rows(&buffer)
                .iter()
                .any(|row| row.contains("zadanie nie jest w kolejce")),
            "{}",
            screen(&buffer)
        );
    }

    #[test]
    fn a_long_title_is_cut_with_an_ellipsis_and_the_label_goes_under_it() {
        let mut app = app();
        let title = "Bardzo długi tytuł zadania który nie mieści się w kolumnie";
        let long = titled(
            with_runner(
                with_controller(crate::task_app_support::task("a", "local", "running", 1)),
                ExecutionStatus::Running,
                "a long implementation label that fills the row",
            ),
            title,
        );
        app.load(listing(vec![long], "c"));
        let buffer = render(120, 30, &mut app, &Styles::new(true));
        let row = list_row(&buffer, "Bardzo");
        // The list's inside is 26 columns: 2 for the bar prefix, 5 for the
        // markers, and 1 reserved at the right edge. The title has 18 cells.
        let kept: String = title.chars().take(17).collect();
        assert_eq!(
            content_text(&buffer, row),
            format!("  ●  {kept}… "),
            "{}",
            screen(&buffer)
        );
        assert!(
            content_text(&buffer, row + 1).starts_with("     a long"),
            "{:?}",
            content_text(&buffer, row + 1)
        );
        let right_edge = layout::LIST_WIDTH - 2;
        assert_eq!(buffer[(right_edge, row as u16)].symbol(), " ");
        assert_eq!(buffer[(right_edge, (row + 1) as u16)].symbol(), " ");
        assert!(
            !list_text(&buffer, row + 2).contains("mieści"),
            "the title does not wrap to a third line:\n{}",
            screen(&buffer)
        );
    }

    /// The cells of the list column (inside its frame) of screen row `row`.
    fn list_cells(buffer: &Buffer, row: usize) -> &[ratatui::buffer::Cell] {
        let width = usize::from(buffer.area.width);
        &buffer.content()[row * width + 1..row * width + usize::from(layout::LIST_WIDTH) - 1]
    }

    fn list_text(buffer: &Buffer, row: usize) -> String {
        list_cells(buffer, row)
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    /// What a row of the list shows after the bracket's column.
    fn content_text(buffer: &Buffer, row: usize) -> String {
        list_text(buffer, row).chars().skip(2).collect()
    }

    /// The bracket's column of screen row `row`: the cell right inside the
    /// frame of the list.
    fn bar(buffer: &Buffer, row: usize) -> (String, Color) {
        cell_at(buffer, 1, row as u16)
    }

    /// Asserts the task at `row` is marked only by the bar beside its lines.
    fn assert_task_bar(buffer: &Buffer, row: usize, marks: SelectionMarks) {
        for y in row..row + 2 {
            let (symbol, color) = bar(buffer, y);
            assert!(
                is_mark(&buffer[(1, y as u16)], "│", marks),
                "bar in row {y}: {symbol:?} with {color:?}\n{}",
                screen(buffer)
            );
        }
        let background = buffer[(1, row as u16)].bg;
        assert!(
            (row..row + 2).all(|y| {
                row_cells(buffer, y as u16, 1..layout::LIST_WIDTH - 1)
                    .all(|cell| cell.bg == background)
            }),
            "task selection does not alter its row background:\n{}",
            screen(buffer)
        );
    }

    /// Whether no cell inside the frame of the list is in reverse video (R7);
    /// the key caps of the bar below may be.
    fn no_reversed_in_list(buffer: &Buffer) -> bool {
        (2..usize::from(buffer.area.height) - 2).all(|row| none_reversed(list_cells(buffer, row)))
    }

    /// Two running tasks with labels.
    fn two_task_app() -> TaskApp {
        let mut app = app();
        let running = |id: &str, rank, step: &str, title: &str| {
            titled(
                with_runner(
                    with_controller(crate::task_app_support::task(id, "local", "running", rank)),
                    ExecutionStatus::Running,
                    step,
                ),
                title,
            )
        };
        app.load(listing(
            vec![
                running("a", 1, "implement", "Refactor parser"),
                running("b", 2, "review", "Fix the lexer"),
            ],
            "c",
        ));
        app
    }

    /// A selected task keeps its two rows and only adds the left bar; moving
    /// the selection does not move either task.
    #[test]
    fn a_task_takes_two_rows_and_selection_only_adds_the_left_bar() {
        let mut app = two_task_app();
        let buffer = render(120, 30, &mut app, &Styles::new(false));
        let first = list_row(&buffer, "Refactor parser");
        assert_eq!(first, 2, "selection adds no rows:\n{}", screen(&buffer));
        assert!(
            content_text(&buffer, first).starts_with("  ●  Refactor parser"),
            "{:?}",
            content_text(&buffer, first)
        );
        assert!(
            content_text(&buffer, first + 1).starts_with("     implement"),
            "{:?}",
            content_text(&buffer, first + 1)
        );
        let second = list_row(&buffer, "Fix the lexer");
        assert_eq!(second, first + 2, "the tasks stay adjacent");
        assert_task_bar(&buffer, first, marks(true, false));
        assert_eq!(bar(&buffer, second).0, " ");
        assert!(
            list_text(&buffer, second + 2).trim().is_empty(),
            "nothing under the last task"
        );

        press(&mut app, KeyCode::Char('j'));
        let buffer = render(120, 30, &mut app, &Styles::new(false));
        assert_eq!(list_row(&buffer, "Refactor parser"), first);
        assert_eq!(list_row(&buffer, "Fix the lexer"), second);
        assert_task_bar(&buffer, second, marks(true, false));
        assert_eq!(bar(&buffer, 2).0, " ");
        assert!(
            content_text(&buffer, 3).starts_with("     implement"),
            "the first task's lines are adjacent:\n{}",
            screen(&buffer)
        );
    }

    /// R8: the label keeps its role on the second line.
    #[test]
    fn the_labels_keep_their_roles_on_the_second_line() {
        let mut app = app();
        let offline = titled(
            with_runner(
                crate::task_app_support::task("a", "local", "running", 1),
                ExecutionStatus::Running,
                "implement",
            ),
            "Offline task",
        );
        let step = titled(
            with_runner(
                with_controller(crate::task_app_support::task("b", "local", "running", 2)),
                ExecutionStatus::Running,
                "review",
            ),
            "Step task",
        );
        let queued_tasks: Vec<_> = (3..=5)
            .map(|rank| titled(queued(&format!("q{rank}"), rank), &format!("Queued {rank}")))
            .collect();
        let mut tasks = vec![offline, step];
        tasks.extend(queued_tasks);
        app.load(listing(tasks, "c"));
        press(&mut app, KeyCode::Char('G'));
        let buffer = render(120, 30, &mut app, &Styles::new(false));
        let label = |title: &str, text: &str| {
            let row = list_row(&buffer, title) + 1;
            let line = content_text(&buffer, row);
            assert!(line.starts_with(&format!("     {text}")), "{line:?}");
            list_cells(&buffer, row)[7].fg
        };
        assert_eq!(label("Offline task", "offline"), Color::Yellow);
        assert_eq!(label("Step task", "review"), Color::DarkGray);
        assert_eq!(label("Queued 4", "#2"), Color::Reset);
    }

    fn long_list(count: u32) -> Vec<narsor_engine::controller_protocol::TaskSummary> {
        (1..=count)
            .map(|rank| {
                titled(
                    with_runner(
                        with_controller(crate::task_app_support::task(
                            &format!("t{rank}"),
                            "local",
                            "running",
                            rank,
                        )),
                        ExecutionStatus::Running,
                        "implement",
                    ),
                    &format!("T-{rank:02}"),
                )
            })
            .collect()
    }

    /// Scrolling goes by whole tasks, with the selected one in view.
    #[test]
    fn a_long_list_scrolls_by_whole_tasks() {
        let mut app = app();
        app.load(listing(long_list(20), "c"));
        press(&mut app, KeyCode::Char('G'));
        let buffer = render(120, 31, &mut app, &Styles::new(true));
        let last = list_row(&buffer, "T-20");
        assert!(
            content_text(&buffer, last + 1).starts_with("     implement"),
            "the last task shows both lines:\n{}",
            screen(&buffer)
        );
        assert_task_bar(&buffer, last, marks(true, true));
        // The first row inside the frame is a whole task.
        assert!(
            content_text(&buffer, 2).starts_with("  ●  T-"),
            "{:?}\n{}",
            content_text(&buffer, 2),
            screen(&buffer)
        );
        assert!(content_text(&buffer, 3).starts_with("     implement"));

        // Going up does not scroll while the tasks fit; neither task moves.
        let top = content_text(&buffer, 2);
        press(&mut app, KeyCode::Char('k'));
        let buffer = render(120, 31, &mut app, &Styles::new(true));
        assert_eq!(content_text(&buffer, 2), top, "{}", screen(&buffer));
        assert_task_bar(&buffer, last - 2, marks(true, true));
        assert_eq!(list_row(&buffer, "T-19"), last - 2);
        assert_eq!(list_row(&buffer, "T-20"), last);
    }

    /// A new read — a task added at the end — moves neither the view nor the
    /// selected task.
    #[test]
    fn a_refresh_keeps_the_view_and_the_selected_row_in_place() {
        let mut app = app();
        app.load(listing(long_list(20), "c"));
        press(&mut app, KeyCode::Char('G'));
        for _ in 0..3 {
            press(&mut app, KeyCode::Char('k'));
        }
        let before = render(120, 31, &mut app, &Styles::new(false));
        let selected = list_row(&before, "T-17");
        let top = content_text(&before, 2);

        app.load(listing(long_list(21), "d"));
        let after = render(120, 31, &mut app, &Styles::new(false));
        assert_eq!(content_text(&after, 2), top, "{}", screen(&after));
        assert_eq!(list_row(&after, "T-17"), selected);
        assert_task_bar(&after, selected, marks(true, false));
    }

    /// R10: the rows of tasks being sent have the same two lines and the
    /// bracket, and `nie wysłano` is a problem.
    #[test]
    fn pending_rows_take_two_lines_and_not_sent_is_a_problem() {
        use narsor::tui::task_app::list::{PendingRow, PendingState, TaskList};
        let mut list = TaskList::new();
        for (id, state) in [("p1", PendingState::Sending), ("p2", PendingState::NotSent)] {
            list.push_pending(PendingRow {
                task_id: id.to_string(),
                title: format!("Pending {id}"),
                state,
                reason: None,
            });
        }
        let buffer = draw(layout::LIST_WIDTH, 10, |frame| {
            let area = frame.area();
            list.render(frame, area, &Styles::new(false), true, false)
        });
        let width = usize::from(buffer.area.width);
        let line =
            |row: usize| -> String { rows(&buffer)[row].chars().skip(3).take(width - 4).collect() };
        assert!(line(1).starts_with("  ↻  Pending p1"), "{:?}", line(1));
        assert!(line(2).starts_with("     wysyłanie…"), "{:?}", line(2));
        assert!(line(3).starts_with("  ✗  Pending p2"), "{:?}", line(3));
        assert!(line(4).starts_with("     nie wysłano"), "{:?}", line(4));
        assert_eq!(buffer.content()[4 * width + 8].fg, Color::Red);
        // The last pushed row is selected: it has only a bar.
        let cell = |x: u16, y: u16| cell_at(&buffer, x, y);
        assert_eq!(cell(1, 3), ("│".to_string(), Color::Cyan));
        assert_eq!(cell(1, 4), ("│".to_string(), Color::Cyan));
        assert_eq!(cell(1, 1).0, " ");
        assert!(none_reversed(buffer.content()));
    }

    /// R8: the move preview takes the place of the label, inside a bracket
    /// in the focus role.
    #[test]
    fn the_move_preview_is_on_the_second_line() {
        let mut app = app();
        let tasks = (1..=5)
            .map(|rank| titled(queued(&format!("q{rank}"), rank), &format!("Queued {rank}")))
            .collect();
        app.load(listing(tasks, "c"));
        press(&mut app, KeyCode::Char('m'));
        press(&mut app, KeyCode::Char('j'));
        let buffer = render(120, 30, &mut app, &Styles::new(false));
        let moved = list_row(&buffer, "Queued 1");
        assert!(
            content_text(&buffer, moved + 1).starts_with("     ↕ 2/5"),
            "{}",
            screen(&buffer)
        );
        assert!(list_row(&buffer, "Queued 2") < moved);
        assert_task_bar(&buffer, moved, marks(true, false));
    }
}

mod task_app_detail_render {
    use super::*;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use narsor::api::ApiClient;
    use narsor::tui::conversation as conversation_projection;
    use narsor::tui::task_app::detail::DetailRead;
    use narsor::tui::task_app::tabs::conversation::{self, AttemptRead, Attempts, Conversation};
    use narsor::tui::task_app::{layout, StepFocus, TaskApp};
    use narsor::tui::transcript::{TranscriptEntry, TranscriptEntryId};
    use narsor_engine::controller_protocol::{ExecutionEventsResponse, QueueView};
    use narsor_engine::domain::ExecutionStatus;
    use narsor_engine::engine::{ConversationState, PositionStatus, Timeline};
    use narsor_engine::orchestration::QueueReason;

    use crate::remote_support::{snapshot, RemoteHarness};
    use crate::task_app_support::{
        displayed, execution_events, listing, position, queued, task, timeline, with_controller,
        with_harness_unavailable, with_runner,
    };

    fn app() -> TaskApp {
        let client = ApiClient::new("127.0.0.1:9".parse().unwrap(), "token");
        TaskApp::new(client, "local", None)
    }

    fn press(app: &mut TaskApp, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn render(app: &mut TaskApp, styles: &Styles) -> Buffer {
        render_at(app, styles, 120, 30)
    }

    // The rows of the details header under the top bar and the frame: a
    // padded blank row, the identity, the state and the metadata (links and
    // workflow); then a rule, the tabs and the marker under the open tab. A
    // notice (an unavailable harness) adds a row under the metadata.
    const IDENTITY_ROW: usize = 3;
    const STATE_ROW: usize = 4;
    const METADATA_ROW: usize = 5;
    const TABS_ROW: usize = 7;
    const TAB_MARKER_ROW: usize = 8;

    fn render_at(app: &mut TaskApp, styles: &Styles, width: u16, height: u16) -> Buffer {
        draw(width, height, |frame| {
            layout::render(frame, app, styles, Instant::now())
        })
    }

    /// `NAR-12`: running at the third of five steps, with a Jira link and a PR
    /// link that has no value.
    fn running_app() -> TaskApp {
        let mut app = app();
        let running = with_runner(
            with_controller(task("a", "local", "running", 1)),
            ExecutionStatus::Running,
            "review",
        );
        app.load(listing(
            vec![displayed(
                running,
                "NAR-12 Zmiana API zamówień",
                &[
                    ("Jira", "NAR-12", Some("https://jira.example/browse/NAR-12")),
                    ("PR", "—", None),
                ],
            )],
            "c",
        ));
        let steps = ["plan", "edit", "review", "test", "ship"];
        let positions = steps
            .iter()
            .map(|state| {
                let status = match *state {
                    "review" => PositionStatus::InProgress,
                    "plan" | "edit" => PositionStatus::Succeeded,
                    _ => PositionStatus::Succeeded,
                };
                position(state, status)
            })
            .collect();
        app.on_detail_read(
            "a",
            1,
            DetailRead::Timeline(Ok(timeline("a", positions, Some("review #1")))),
        );
        press(&mut app, KeyCode::Enter);
        app
    }

    #[test]
    fn the_header_of_a_running_task_shows_the_display_the_step_and_the_state() {
        let mut app = running_app();
        let buffer = render(&mut app, &Styles::new(false));
        let rows = rows(&buffer);
        // The details start under a padded blank row, with identity, state,
        // and metadata rows.
        assert!(
            rows[IDENTITY_ROW].contains("NAR-12 Zmiana API zamówień"),
            "{}",
            rows[IDENTITY_ROW]
        );
        let metadata = &rows[METADATA_ROW];
        assert!(metadata.contains("implement-feature v3"), "{metadata}");
        assert!(metadata.contains("Jira NAR-12 ↗"), "{metadata}");
        assert!(
            metadata.contains("PR: —"),
            "a link without a value: {metadata}"
        );
        assert!(!metadata.contains("PR — ↗"));
        let state = &rows[STATE_ROW];
        assert!(state.contains("● running"), "{state}");
        assert!(state.contains("krok 3/5"), "{state}");
        let tabs = &rows[TABS_ROW];
        for tab in ["Przebieg", "Rozmowa", "Zdarzenia", "Dyrygent", "Info"] {
            assert!(tabs.contains(tab), "{tab} in {tabs}");
        }
        assert!(tabs.contains("śledzi"), "{tabs}");
        let selected_byte = tabs.find("Rozmowa").unwrap();
        let selected_start = tabs[..selected_byte].chars().count();
        assert_eq!(
            rows[TAB_MARKER_ROW]
                .chars()
                .skip(selected_start)
                .take("Rozmowa".len())
                .collect::<String>(),
            "━━━━━━━",
            "the focused marker spans the selected title only"
        );

        let no_color = render_at(&mut app, &Styles::new(true), 120, 30);
        let no_color_rows = super::rows(&no_color);
        assert_eq!(
            no_color_rows[TAB_MARKER_ROW].chars().nth(selected_start),
            Some('━')
        );
        assert!(bold_at(&no_color, selected_start as u16, TABS_ROW as u16));

        press(&mut app, KeyCode::Esc);
        let unfocused_rows = super::rows(&render(&mut app, &Styles::new(false)));
        assert_eq!(
            unfocused_rows[TAB_MARKER_ROW].chars().nth(selected_start),
            Some('─')
        );
    }

    #[test]
    fn the_task_header_uses_a_muted_navy_background_and_rule_above_tabs() {
        let mut app = running_app();
        press(&mut app, KeyCode::Enter);
        let buffer = render_at(&mut app, &Styles::new(false), 120, 40);
        let navy = Color::Rgb(34, 48, 72);
        let tabs_y = u16::try_from(row_of(&buffer, "Przebieg")).unwrap();
        let first_line_y = tabs_y - 5;
        assert!(
            row_cells(
                &buffer,
                first_line_y,
                layout::LIST_WIDTH + 1..buffer.area.width - 1
            )
            .all(|cell| cell.symbol() == " " && cell.bg == navy),
            "the upper panel starts with a padded blank row"
        );
        assert_eq!(buffer[(layout::LIST_WIDTH + 2, tabs_y)].bg, navy);
        let underline_y = tabs_y + 1;
        let underline_cells = row_cells(
            &buffer,
            underline_y,
            layout::LIST_WIDTH + 1..buffer.area.width - 1,
        )
        .collect::<Vec<_>>();
        assert!(
            underline_cells.iter().all(|cell| cell.bg == Color::Reset),
            "the entire underline row, including both ends, uses the main panel background"
        );
        let underline = underline_cells
            .iter()
            .find(|cell| matches!(cell.symbol(), "─" | "━"))
            .expect("the active tab has an underline");
        assert_eq!(
            underline.bg,
            Color::Reset,
            "the underline is on the main panel background"
        );
        assert!(row_cells(
            &buffer,
            tabs_y - 1,
            layout::LIST_WIDTH + 1..buffer.area.width - 1
        )
        .any(|cell| cell.symbol() == "─" && cell.fg == Color::Black));
    }

    #[test]
    fn a_fitted_title_keeps_the_task_id_visible() {
        let mut app = app();
        let running = with_runner(
            with_controller(task("RUN-1", "local", "running", 1)),
            ExecutionStatus::Running,
            "review",
        );
        let title = format!("{} RUN-1", "a very long task title ".repeat(6));
        app.load(listing(vec![displayed(running, &title, &[])], "c"));
        press(&mut app, KeyCode::Enter);

        let rows = rows(&render_at(&mut app, &Styles::new(false), 60, 20));
        assert!(
            rows[IDENTITY_ROW].contains("…  RUN-1"),
            "identity survives fitting: {}",
            rows[IDENTITY_ROW]
        );
    }

    #[test]
    fn a_long_task_id_is_fitted_to_the_header_width() {
        let mut app = app();
        let id = "A".repeat(128);
        let running = with_runner(
            with_controller(task(&id, "local", "running", 1)),
            ExecutionStatus::Running,
            "review",
        );
        app.load(listing(vec![displayed(running, "short title", &[])], "c"));
        press(&mut app, KeyCode::Enter);

        let rows = rows(&render_at(&mut app, &Styles::new(false), 60, 20));
        assert_eq!(rows[IDENTITY_ROW].chars().count(), 60);
        assert!(
            rows[IDENTITY_ROW].contains('…'),
            "long identity is marked as fitted: {}",
            rows[IDENTITY_ROW]
        );
    }

    /// `running_app`, but its runner cannot switch to `harness` and keeps
    /// `claude-code`.
    fn app_without_the_default_harness(harness: &str) -> TaskApp {
        let mut app = app();
        let running = with_harness_unavailable(
            with_runner(
                with_controller(task("a", "k8s", "running", 1)),
                ExecutionStatus::Running,
                "review",
            ),
            harness,
            "claude-code",
            "backend 'k8s' runs only claude-code",
        );
        app.load(listing(
            vec![displayed(running, "NAR-12 Zmiana API zamówień", &[])],
            "c",
        ));
        press(&mut app, KeyCode::Enter);
        app
    }

    // AE6: one line under the header says it; Info has the full reason; the
    // list has no attention marker for it.
    #[test]
    fn a_default_harness_the_task_cannot_use_is_one_line_in_the_header_and_a_row_in_info() {
        let mut app = app_without_the_default_harness("opencode");
        let buffer = render(&mut app, &Styles::new(false));
        let rows = rows(&buffer);
        assert!(rows[STATE_ROW].contains("● running"), "{}", rows[STATE_ROW]);
        let notice = METADATA_ROW + 1;
        assert!(
            rows[notice].contains("opencode niedostępny tutaj — zostaje claude-code"),
            "{}",
            rows[notice]
        );
        for tab in ["Przebieg", "Info"] {
            assert!(
                rows[TABS_ROW + 1].contains(tab),
                "the tabs move down: {}",
                rows[TABS_ROW + 1]
            );
        }
        let column = rows[notice].find("opencode").unwrap();
        let column = u16::try_from(rows[notice][..column].chars().count()).unwrap();
        assert_eq!(
            buffer[(column, notice as u16)].fg,
            Color::Yellow,
            "the attention role"
        );
        // Not a problem: the counters and the list row carry no `!`.
        assert!(rows[0].contains("! 0"), "{}", rows[0]);
        let list_row = row_of(&buffer, "NAR-12");
        assert!(
            !rows[list_row].chars().take(28).any(|c| c == '!'),
            "{}",
            rows[list_row]
        );

        for _ in 0..3 {
            press(&mut app, KeyCode::Char(']'));
        }
        let info = screen(&draw(120, 50, |frame| {
            layout::render(frame, &mut app, &Styles::new(false), Instant::now())
        }));
        assert!(
            info.contains("opencode niedostępny: backend 'k8s' runs only claude-code"),
            "{info}"
        );
    }

    #[test]
    fn without_an_unavailable_harness_there_is_no_extra_line() {
        let mut app = running_app();
        let rows = rows(&render(&mut app, &Styles::new(false)));
        assert!(rows[TABS_ROW].contains("Przebieg"), "{}", rows[TABS_ROW]);
        assert!(!rows.join("\n").contains("niedostępny tutaj"));
    }

    #[test]
    fn the_harness_line_is_cut_with_an_ellipsis_when_narrow() {
        let mut app = app_without_the_default_harness("a-harness-from-a-plugin");
        let buffer = draw(60, 20, |frame| {
            layout::render(frame, &mut app, &Styles::new(true), Instant::now())
        });
        let row = row_of(&buffer, "a-harness-from-a-plugin niedostępny");
        let line = &rows(&buffer)[row];
        assert!(line.contains('…'), "{line}");
        assert!(!line.contains("zostaje claude-code"), "{line}");
    }

    #[test]
    fn the_axis_has_a_row_per_step_and_brackets_the_selected_one() {
        let mut app = running_app();
        app.detail_mut()
            .set_tab(narsor::tui::task_app::DetailTab::Progress);
        let buffer = render(&mut app, &Styles::new(false));
        let screen = screen(&buffer);
        for expected in ["✓ plan #1", "✓ edit #1", "● review #1 w toku"] {
            assert!(screen.contains(expected), "{expected:?} in\n{screen}");
        }
        assert!(!below_the_frame_title(&buffer).contains('▸'), "{screen}");
        // The axis is the left part of the details: the selected step's own heading is on the right.
        let row = |needle: &str| {
            rows(&buffer)
                .iter()
                .position(|row| {
                    row.chars()
                        .skip(30)
                        .take(30)
                        .collect::<String>()
                        .contains(needle)
                })
                .unwrap_or_else(|| panic!("{needle:?} is not on the axis"))
        };
        assert!(
            row("plan #1") < row("edit #1")
                && row("edit #1") < row("review #1")
                && row("review #1") < row("test #1")
        );
        assert_eq!(row("plan #1") + 1, row("edit #1"), "unselected steps stack");
        assert_eq!(
            row("edit #1") + 2,
            row("review #1"),
            "the rule above the selected step"
        );
        assert_eq!(
            row("review #1") + 2,
            row("test #1"),
            "the rule below the selected step"
        );
        // The keys are on the axis: a bracket in the focus role.
        assert_axis_bracket(&buffer, "● review #1 w toku", marks(true, false));
        assert!(no_reversed_on_the_axis(&buffer, "● review #1 w toku"));
        // Without colour the same is said in words, glyphs and weight.
        let plain = render(&mut app, &Styles::new(true));
        assert_axis_bracket(&plain, "● review #1 w toku", marks(true, true));
        let (y, x) = spot(&plain, "● review #1 w toku");
        assert!(bold_at(&plain, x - 2, y) && bold_at(&plain, x, y - 1));
        assert!(no_reversed_on_the_axis(&plain, "● review #1 w toku"));
    }

    #[test]
    fn hyperlinks_are_written_only_when_the_terminal_draws_them() {
        let mut app = running_app();
        let plain_buffer = render(&mut app, &Styles::new(false));
        let plain = screen(&plain_buffer);
        assert!(!plain.contains('\u{1b}'));

        app.detail_mut().set_hyperlinks(true);
        let buffer = render(&mut app, &Styles::new(false));
        let screen = screen(&buffer);
        assert!(
            screen.contains("\u{1b}]8;;https://jira.example/browse/NAR-12\u{1b}\\Jira NAR-12 ↗\u{1b}]8;;\u{1b}\\"),
            "{:?}",
            rows(&buffer)[METADATA_ROW]
        );
        assert_eq!(
            screen.matches("\u{1b}]8;;https").count(),
            1,
            "only the link with a target"
        );
        let row = &rows(&plain_buffer)[METADATA_ROW];
        let byte = row.find("Jira NAR-12 ↗").unwrap();
        let column = u16::try_from(row[..byte].chars().count()).unwrap();
        let y = METADATA_ROW as u16;
        let link_cell = (0..buffer.area.width)
            .find(|&x| buffer[(x, y)].symbol().starts_with("\u{1b}]8;;https://"));
        assert_eq!(
            link_cell,
            Some(column),
            "link hit region begins under its label"
        );
        assert!(buffer[(column, y)].symbol().contains("Jira NAR-12 ↗"));
    }

    #[test]
    fn hyperlinks_with_unsafe_targets_stay_plain_text() {
        for url in [
            "https://x.example/\u{1b}]8;;",
            "https://x.example/\u{202e}gnp",
            "https://x.example/\u{7}",
            "https://x.example/a b",
            "javascript:alert(1)",
        ] {
            let mut app = app();
            let running = with_runner(
                with_controller(task("a", "local", "running", 1)),
                ExecutionStatus::Running,
                "review",
            );
            app.load(listing(
                vec![displayed(
                    running,
                    "NAR-12",
                    &[("Jira", "NAR-12", Some(url))],
                )],
                "c",
            ));
            app.detail_mut().set_hyperlinks(true);
            let screen = screen(&render(&mut app, &Styles::new(false)));
            assert!(
                !screen.contains("\u{1b}]8"),
                "{url:?} must not become a hyperlink"
            );
        }
    }

    #[test]
    fn a_queued_task_says_it_has_not_started_and_where_it_waits() {
        let mut app = app();
        let waiting = narsor_waiting(queued("b", 2), 2, 2, 3);
        app.load(listing(
            vec![queued("a", 1), displayed(waiting, "NAR-17", &[])],
            "c",
        ));
        app.list_mut().select("b");
        press(&mut app, KeyCode::Enter);

        let buffer = render(&mut app, &Styles::new(false));
        let rows = rows(&buffer);
        assert!(
            rows[STATE_ROW].contains("#2 · czeka na slot (2/3)"),
            "{}",
            rows[STATE_ROW]
        );
        let screen = screen(&buffer);
        assert!(
            screen.contains("jeszcze nie wystartowało, #2 w kolejce"),
            "{screen}"
        );

        // The other execution tabs say the same; Info shows what is there.
        press(&mut app, KeyCode::Char(']'));
        assert!(super::screen(&render(&mut app, &Styles::new(false)))
            .contains("jeszcze nie wystartowało"));
        press(&mut app, KeyCode::Char(']'));
        assert!(super::screen(&render(&mut app, &Styles::new(false)))
            .contains("jeszcze nie wystartowało"));
        press(&mut app, KeyCode::Char(']'));
        let info = super::screen(&render(&mut app, &Styles::new(false)));
        assert!(
            info.contains("Zapieczętowane wejścia") && info.contains("launch digest"),
            "{info}"
        );
    }

    fn narsor_waiting(
        mut task: narsor_engine::controller_protocol::TaskSummary,
        position: u32,
        running: u32,
        limit: u32,
    ) -> narsor_engine::controller_protocol::TaskSummary {
        let reason = QueueReason::WaitingForCapacity {
            position,
            running,
            limit,
        };
        task.queue = Some(QueueView {
            message: reason.describe(),
            reason,
            since: task.as_of,
        });
        task
    }

    #[test]
    fn a_controller_that_is_gone_shows_in_the_queue_line() {
        let mut app = app();
        let mut waiting = queued("a", 1);
        waiting.queue = Some(QueueView {
            reason: QueueReason::ControllerOffline,
            message: String::new(),
            since: waiting.as_of - chrono::Duration::minutes(4),
        });
        app.load(listing(vec![waiting], "c"));
        let buffer = render(&mut app, &Styles::new(false));
        assert!(
            rows(&buffer)[STATE_ROW].contains("kontroler offline 4m"),
            "{}",
            rows(&buffer)[STATE_ROW]
        );
    }

    #[test]
    fn an_archived_task_says_its_storage_is_gone() {
        let mut app = app();
        app.load(listing(vec![task("a", "local", "archived", 1)], "c"));
        press(&mut app, KeyCode::Enter);
        let screen = screen(&render(&mut app, &Styles::new(false)));
        assert!(
            screen.contains("storage usunięty przy archiwizacji"),
            "{screen}"
        );
    }

    #[test]
    fn events_that_cannot_be_read_say_why_in_their_tab() {
        let mut app = running_app();
        app.detail_mut()
            .set_tab(narsor::tui::task_app::DetailTab::Events);
        app.on_detail_read(
            "a",
            1,
            DetailRead::Events {
                after: 0,
                result: Ok(ExecutionEventsResponse::Unavailable {
                    reason: narsor_engine::controller_protocol::ConversationUnavailableReason::RunnerUnreachable,
                    message: "the runner has not been heard from lately".into(),
                }),
            },
        );
        let screen = screen(&render(&mut app, &Styles::new(false)));
        assert!(
            screen.contains("zdarzenia niedostępne: the runner has not been heard from lately"),
            "{screen}"
        );
        // The rest of the screen is intact.
        assert!(screen.contains("NAR-12 Zmiana API zamówień"));
    }

    #[test]
    fn a_silent_runner_leaves_a_note_in_the_tab_and_the_connection_alone() {
        let mut app = running_app();
        app.detail_mut()
            .set_tab(narsor::tui::task_app::DetailTab::Events);
        let before = app.connection().status(std::time::Instant::now()).text;
        app.on_detail_read(
            "a",
            1,
            DetailRead::Events {
                after: 0,
                result: Err(narsor::tui::task_app::detail::runner_not_answering()),
            },
        );
        let screen = screen(&render(&mut app, &Styles::new(false)));
        assert!(screen.contains("runner nie odpowiada"), "{screen}");
        assert_eq!(
            app.connection().status(std::time::Instant::now()).text,
            before
        );
        // The rest of the screen is intact.
        assert!(screen.contains("NAR-12 Zmiana API zamówień"));
    }

    #[tokio::test]
    async fn a_read_that_the_runner_never_answers_is_cut_off_by_the_limit() {
        use narsor::tui::task_app::detail::{bounded_read, runner_not_answering};
        let started = std::time::Instant::now();
        let result: Result<(), _> =
            bounded_read(std::time::Duration::from_millis(30), std::future::pending()).await;
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        match (result, runner_not_answering()) {
            (
                Err(narsor::api::ClientError::Degraded { code, .. }),
                narsor::api::ClientError::Degraded { code: expected, .. },
            ) => assert_eq!(code, expected),
            (other, _) => panic!("expected the runner-silent failure, got {other:?}"),
        }
        let answered = bounded_read(std::time::Duration::from_secs(5), async { Ok(7) }).await;
        assert_eq!(answered.unwrap(), 7);
    }

    #[test]
    fn events_are_listed_with_the_detail_of_the_selected_one() {
        let mut app = running_app();
        app.detail_mut()
            .set_tab(narsor::tui::task_app::DetailTab::Events);
        app.on_detail_read(
            "a",
            1,
            DetailRead::Events {
                after: 0,
                result: Ok(ExecutionEventsResponse::Available {
                    events: execution_events(4),
                    has_more: false,
                    source: narsor_engine::controller_protocol::ExecutionEventsSource::Runner,
                }),
            },
        );
        let list = screen(&render(&mut app, &Styles::new(false)));
        assert!(
            list.contains("state_entered") && list.contains("state-4"),
            "{list}"
        );
        assert!(!list.contains("szczegóły zdarzenia"));
        press(&mut app, KeyCode::Char('v'));
        let detail = screen(&render(&mut app, &Styles::new(false)));
        assert!(detail.contains("szczegóły zdarzenia 4"), "{detail}");
        assert!(detail.contains("\"state_id\": \"state-4\""), "{detail}");
    }

    #[test]
    fn the_conversation_tab_shows_a_message_for_a_task_without_one() {
        let mut app = running_app();
        app.detail_mut()
            .set_tab(narsor::tui::task_app::DetailTab::Conversation);
        let screen = screen(&render(&mut app, &Styles::new(false)));
        assert!(screen.contains("brak rozmowy"), "{screen}");
    }

    #[test]
    fn text_from_the_workflow_and_the_runner_never_carries_terminal_controls() {
        let mut app = app();
        let hostile = |text: &str| text.replace('@', "\u{1b}[31m\u{202e}");
        let running = with_runner(
            with_controller(task("a", "local", "running", 1)),
            ExecutionStatus::Running,
            "review",
        );
        app.load(listing(
            vec![displayed(
                running,
                &hostile("T@itle"),
                &[(
                    &hostile("La@bel"),
                    &hostile("Te@xt"),
                    Some("https://x.example/"),
                )],
            )],
            "c",
        ));
        let mut step = position("st@ep", PositionStatus::Failed);
        step.label = hostile("st@ep #1");
        step.error = Some(hostile("bo@om"));
        app.on_detail_read(
            "a",
            1,
            DetailRead::Timeline(Ok(timeline("a", vec![step], None))),
        );
        press(&mut app, KeyCode::Enter);
        for _ in 0..5 {
            let screen = screen(&render(&mut app, &Styles::new(false)));
            assert!(
                !screen.contains('\u{1b}') && !screen.contains('\u{202e}'),
                "{screen}"
            );
            press(&mut app, KeyCode::Char(']'));
        }
    }

    #[tokio::test]
    async fn the_wide_screen_with_a_remote_task_shows_the_header_the_tabs_and_the_axis() {
        let harness = RemoteHarness::start().await;
        let mut recorded = position("plan", PositionStatus::Succeeded);
        recorded.conversation = ConversationState::NotCreated {
            reason: "script".into(),
        };
        let current = position("review", PositionStatus::InProgress);
        let mut first = snapshot(1, None);
        first.timeline = Some(Timeline {
            default_position: Some(current.label.clone()),
            positions: vec![recorded, current],
            ..Timeline::default()
        });
        harness.running("task-1", "k8s", first).await;

        let mut app = TaskApp::new(harness.owner.clone(), "remote", None);
        app.refresh().await;
        press(&mut app, KeyCode::Enter);
        app.sync_detail().await;
        app.detail_mut()
            .set_tab(narsor::tui::task_app::DetailTab::Progress);

        let buffer = render(&mut app, &Styles::new(false));
        let screen = screen(&buffer);
        let rows = rows(&buffer);
        assert!(
            rows[IDENTITY_ROW].contains("task-1"),
            "{}",
            rows[IDENTITY_ROW]
        );
        assert!(
            rows[METADATA_ROW].contains("implement-feature v1"),
            "{}",
            rows[METADATA_ROW]
        );
        assert!(
            rows[STATE_ROW].contains("● running") && rows[STATE_ROW].contains("krok 2/2"),
            "{}",
            rows[STATE_ROW]
        );
        assert!(
            rows[TABS_ROW].contains("Przebieg") && rows[TABS_ROW].contains("Info"),
            "{}",
            rows[TABS_ROW]
        );
        assert!(
            screen.contains("✓ plan #1") && screen.contains("● review #1 w toku"),
            "{screen}"
        );
        assert_axis_bracket(&buffer, "● review #1 w toku", marks(true, false));
        assert!(
            screen.contains("brak rozmowy: script") || screen.contains("review #1 · w toku"),
            "{screen}"
        );
        assert!(screen.contains("● live"));
    }

    // -----------------------------------------------------------------
    // The bracket on the axis (U6) and the bar of the transcripts and
    // *Zdarzenia* (U8)
    // -----------------------------------------------------------------

    /// The row and the column where `needle` first shows on the screen.
    fn spot(buffer: &Buffer, needle: &str) -> (u16, u16) {
        let row = row_of(buffer, needle);
        let text = &rows(buffer)[row];
        let column = text[..text.find(needle).unwrap()].chars().count();
        (row as u16, u16::try_from(column).unwrap())
    }

    /// Asserts the axis row starting with `needle` is bracketed with
    /// `marks`, the rules checked at its first column.
    fn assert_axis_bracket(buffer: &Buffer, needle: &str, marks: SelectionMarks) {
        let (y, x) = spot(buffer, needle);
        assert_bracket(buffer, (x, y), 1, x + 1, marks);
    }

    /// Whether no cell of the axis around the row of `needle` is in reverse
    /// video (R7).
    fn no_reversed_on_the_axis(buffer: &Buffer, needle: &str) -> bool {
        let (y, x) = spot(buffer, needle);
        (y - 1..=y + 1).all(|row| none_reversed(row_cells(buffer, row, x - 2..x + 16)))
    }

    /// The bar beside the transcript row holding `needle`: the cell two
    /// columns to the left of its text.
    fn bar_position(buffer: &Buffer, needle: &str) -> (u16, u16) {
        let (y, x) = spot(buffer, needle);
        (
            x.checked_sub(4).expect("body indent and rail precede text"),
            y,
        )
    }

    fn bar_of(buffer: &Buffer, needle: &str) -> (String, Color) {
        let (bar_x, y) = bar_position(buffer, needle);
        cell_at(buffer, bar_x, y)
    }

    fn entry(id: &str, lines: &[&str]) -> TranscriptEntry {
        TranscriptEntry::new(
            TranscriptEntryId::from(id),
            lines.iter().map(|line| line.to_string()).collect(),
            id.as_bytes().to_vec(),
        )
        .with_header(format!("AGENT · {id}"))
    }

    /// The details of `NAR-12` on *Przebieg*: `review` running, with a
    /// recorded conversation of `entries`, the keys on the axis.
    fn recorded_app(entries: Vec<TranscriptEntry>) -> TaskApp {
        let mut app = app();
        let running = with_runner(
            with_controller(task("a", "local", "running", 1)),
            ExecutionStatus::Running,
            "review",
        );
        app.load(listing(vec![displayed(running, "NAR-12", &[])], "c"));
        let mut done = position("plan", PositionStatus::Succeeded);
        done.conversation = ConversationState::Recorded;
        let mut review = position("review", PositionStatus::InProgress);
        review.conversation = ConversationState::Recorded;
        app.on_detail_read(
            "a",
            1,
            DetailRead::Timeline(Ok(timeline("a", vec![done, review], Some("review #1")))),
        );
        let next_entry = entries.len() as u64;
        app.on_detail_read(
            "a",
            1,
            DetailRead::Attempt {
                attempt_id: "act-review".into(),
                from: 0,
                result: Ok(AttemptRead {
                    entries,
                    next_entry,
                    closed: false,
                    truncated_tail: false,
                    complete: true,
                    no_record: false,
                    source: narsor_engine::controller_protocol::ExecutionEventsSource::Runner,
                    recorded_at: None,
                }),
            },
        );
        app.detail_mut()
            .set_tab(narsor::tui::task_app::DetailTab::Progress);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.step_focus(), StepFocus::Axis);
        app
    }

    fn three_entries() -> Vec<TranscriptEntry> {
        ["1", "2", "3"]
            .iter()
            .map(|id| entry(id, &[&format!("line {id}")]))
            .collect()
    }

    /// AE1: in the step's conversation the selected entry's bar is cyan and
    /// the axis's bracket grey; after `Esc` the same cells hold a grey bar
    /// and the axis's bracket is cyan again.
    #[test]
    fn the_bar_and_the_bracket_follow_the_level_of_the_focus() {
        let mut app = recorded_app(three_entries());
        let on_the_axis = render(&mut app, &Styles::new(false));
        assert_axis_bracket(&on_the_axis, "● review #1 w toku", marks(true, false));
        assert_eq!(
            bar_of(&on_the_axis, "line 3"),
            ("│".to_string(), Color::DarkGray),
            "the keys are on the axis: the entry's bar is grey"
        );

        press(&mut app, KeyCode::Enter);
        assert_eq!(app.step_focus(), StepFocus::Conversation);
        press(&mut app, KeyCode::Char('k'));
        let reading = render(&mut app, &Styles::new(false));
        let cell = spot(&reading, "line 2");
        assert_eq!(bar_of(&reading, "line 2"), ("│".to_string(), Color::Cyan));
        assert_eq!(bar_of(&reading, "line 3").0, " ");
        assert_axis_bracket(&reading, "● review #1 w toku", marks(false, false));
        assert!(no_reversed_on_the_axis(&reading, "● review #1 w toku"));

        press(&mut app, KeyCode::Esc);
        assert_eq!(app.step_focus(), StepFocus::Axis);
        let back = render(&mut app, &Styles::new(false));
        assert_eq!(spot(&back, "line 2"), cell, "the entry stays where it was");
        assert_eq!(bar_of(&back, "line 2"), ("│".to_string(), Color::DarkGray));
        assert_axis_bracket(&back, "● review #1 w toku", marks(true, false));
    }

    /// AE6: without colour the weight tells the two levels apart.
    #[test]
    fn without_colour_the_weight_tells_the_levels_apart() {
        let mut app = recorded_app(three_entries());
        let on_the_axis = render(&mut app, &Styles::new(true));
        let (y, x) = spot(&on_the_axis, "● review #1 w toku");
        assert_eq!(cell_at(&on_the_axis, x - 2, y).0, "│");
        assert!(bold_at(&on_the_axis, x - 2, y));
        let (entry_bar_x, entry_y) = bar_position(&on_the_axis, "line 3");
        assert_eq!(cell_at(&on_the_axis, entry_bar_x, entry_y).0, "│");
        assert!(on_the_axis[(entry_bar_x, entry_y)]
            .modifier
            .contains(Modifier::DIM));

        press(&mut app, KeyCode::Enter);
        let reading = render(&mut app, &Styles::new(true));
        assert_eq!(cell_at(&reading, x - 2, y).0, "│");
        assert!(reading[(x - 2, y)].modifier.contains(Modifier::DIM));
        assert_eq!(cell_at(&reading, entry_bar_x, entry_y).0, "│");
        assert!(bold_at(&reading, entry_bar_x, entry_y));
        assert!(no_reversed_on_the_axis(&reading, "● review #1 w toku"));
        assert!(none_reversed(row_cells(
            &reading,
            entry_y,
            entry_bar_x..entry_bar_x + 8
        )));
    }

    /// The keys on the list: the axis keeps its bracket, grey.
    #[test]
    fn with_the_keys_on_the_list_the_axis_bracket_is_grey() {
        let mut app = running_app();
        app.detail_mut()
            .set_tab(narsor::tui::task_app::DetailTab::Progress);
        press(&mut app, KeyCode::Esc);
        let buffer = render(&mut app, &Styles::new(false));
        assert_axis_bracket(&buffer, "● review #1 w toku", marks(false, false));
    }

    /// `m` from the details moves the list's row: the keys are on the list,
    /// so its bar is in the focus role and the axis's grey until the move
    /// ends.
    #[test]
    fn the_move_mode_from_the_details_lights_the_list_and_greys_the_axis() {
        let mut app = app();
        // Queued again after it ran: its timeline stays readable.
        let requeued = with_runner(queued("a", 1), ExecutionStatus::Running, "review");
        app.load(listing(
            vec![
                displayed(requeued, "Requeued one", &[]),
                displayed(queued("b", 2), "Queued two", &[]),
            ],
            "c",
        ));
        app.on_detail_read(
            "a",
            1,
            DetailRead::Timeline(Ok(timeline(
                "a",
                vec![position("review", PositionStatus::InProgress)],
                Some("review #1"),
            ))),
        );
        app.detail_mut()
            .set_tab(narsor::tui::task_app::DetailTab::Progress);
        press(&mut app, KeyCode::Enter);
        let list_row = |buffer: &Buffer| {
            (0..buffer.area.height)
                .find(|&y| {
                    row_cells(buffer, y, 0..layout::LIST_WIDTH)
                        .map(|cell| cell.symbol())
                        .collect::<String>()
                        .contains("Requeued one")
                })
                .expect("the row on the list")
        };
        let details = render(&mut app, &Styles::new(false));
        assert_axis_bracket(&details, "● review #1 w toku", marks(true, false));
        let y = list_row(&details);
        for row in y..y + 2 {
            assert!(is_mark(&details[(1, row)], "│", marks(false, false)));
        }

        press(&mut app, KeyCode::Char('m'));
        assert!(app.list().moving().is_some(), "the move mode is on");
        let moving = render(&mut app, &Styles::new(false));
        let y = list_row(&moving);
        for row in y..y + 2 {
            assert!(is_mark(&moving[(1, row)], "│", marks(true, false)));
        }
        assert_axis_bracket(&moving, "● review #1 w toku", marks(false, false));

        press(&mut app, KeyCode::Esc);
        let back = render(&mut app, &Styles::new(false));
        assert_axis_bracket(&back, "● review #1 w toku", marks(true, false));
    }

    /// R11: at 60×20, with more steps than fit, the selected step is in
    /// view with both of its rules after `G` and after `g`.
    #[test]
    fn the_selected_step_and_its_rules_stay_in_view_on_a_small_screen() {
        let mut app = app();
        let running = with_runner(
            with_controller(task("a", "local", "running", 1)),
            ExecutionStatus::Running,
            "s5",
        );
        app.load(listing(vec![displayed(running, "NAR-12", &[])], "c"));
        let steps: Vec<String> = (0..12).map(|index| format!("s{index}")).collect();
        let positions = steps
            .iter()
            .map(|state| position(state, PositionStatus::Succeeded))
            .collect();
        app.on_detail_read(
            "a",
            1,
            DetailRead::Timeline(Ok(timeline("a", positions, Some("s5 #1")))),
        );
        app.detail_mut()
            .set_tab(narsor::tui::task_app::DetailTab::Progress);
        press(&mut app, KeyCode::Enter);
        let small = |app: &mut TaskApp| {
            draw(60, 20, |frame| {
                layout::render(frame, app, &Styles::new(false), Instant::now())
            })
        };
        press(&mut app, KeyCode::Char('G'));
        let bottom = small(&mut app);
        assert_axis_bracket(&bottom, "✓ s11 #1 ok", marks(true, false));
        press(&mut app, KeyCode::Char('g'));
        let top = small(&mut app);
        assert_axis_bracket(&top, "✓ s0 #1 ok", marks(true, false));
        assert!(!screen(&top).contains("s11 #1"), "{}", screen(&top));
    }

    /// *Rozmowa* of `review`'s `entries`, drawn 60×12 straight by the tab.
    fn conversation_tab(entries: Vec<TranscriptEntry>) -> Conversation {
        let mut review = position("review", PositionStatus::InProgress);
        review.conversation = ConversationState::Recorded;
        let next_entry = entries.len() as u64;
        let mut attempts = Attempts::default();
        attempts.apply(
            "act-review",
            1,
            0,
            Ok(AttemptRead {
                entries,
                next_entry,
                closed: false,
                truncated_tail: false,
                complete: true,
                no_record: false,
                source: narsor_engine::controller_protocol::ExecutionEventsSource::Runner,
                recorded_at: None,
            }),
        );
        let mut conversation = Conversation::new("a");
        conversation.rebuild(&[review], &attempts);
        conversation
    }

    fn draw_conversation(
        conversation: &mut Conversation,
        styles: &Styles,
        stale: bool,
        focused: bool,
    ) -> Buffer {
        draw_conversation_at(60, 12, conversation, styles, stale, focused)
    }

    fn draw_conversation_at(
        width: u16,
        height: u16,
        conversation: &mut Conversation,
        styles: &Styles,
        stale: bool,
        focused: bool,
    ) -> Buffer {
        draw(width, height, |frame| {
            conversation::render(
                frame,
                frame.area(),
                styles,
                conversation,
                stale,
                focused,
                "⠋",
            );
        })
    }

    /// The entries of a real Claude Code recording, classified by the
    /// harness stream parser exactly as the executor stores them.
    fn recorded_claude_entries(name: &str) -> Vec<TranscriptEntry> {
        use narsor_engine::engine::{ClaudeCodeAdapter, HarnessAdapter};
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/claude-code")
            .join(name);
        let text = std::fs::read_to_string(&path).expect("a recording");
        let mut parser = ClaudeCodeAdapter::new().stream_parser();
        text.lines()
            .enumerate()
            .flat_map(|(index, line)| {
                use base64::Engine as _;
                conversation_projection::transcript_entries(&narsor::api::ConversationEntryDto {
                    entry_number: index as u64 + 1,
                    timestamp: chrono::Utc::now(),
                    source: narsor::api::ConversationEntrySource::Stdout,
                    raw_base64: base64::engine::general_purpose::STANDARD.encode(line),
                    elements: parser.on_stdout_line(line.as_bytes()),
                })
            })
            .collect()
    }

    /// U6: *Rozmowa* over every real recording shows model elements only —
    /// no card is raw or unclassified — and draws without a panic.
    #[test]
    fn the_conversation_tab_of_a_real_claude_recording_has_no_raw_or_unclassified_card() {
        let recordings = [
            "ask_user_question.ndjson",
            "max_turns.ndjson",
            "plan_mode.ndjson",
            "schema_awaiting.ndjson",
            "schema_done.ndjson",
            "schema_mismatch.ndjson",
            "schema_plan_fresh.ndjson",
            "schema_plan_resume.ndjson",
            "schema_resume.ndjson",
            "thinking.ndjson",
            "tool_error.ndjson",
            "tools.ndjson",
        ];
        for name in recordings {
            let mut tab = conversation_tab(recorded_claude_entries(name));
            let lines: Vec<String> = tab
                .transcript()
                .logical_lines()
                .into_iter()
                .map(|line| line.text)
                .collect();
            for label in [" · raw · ", " · unclassified · "] {
                assert!(
                    !lines.iter().any(|line| line.contains(label)),
                    "{name} has a{label}card: {lines:#?}"
                );
            }
            let drawn = screen(&draw_conversation_at(
                120,
                40,
                &mut tab,
                &Styles::new(false),
                false,
                true,
            ));
            assert!(!drawn.trim().is_empty(), "{name}");
        }

        let tools: Vec<String> = conversation_tab(recorded_claude_entries("tools.ndjson"))
            .transcript()
            .logical_lines()
            .into_iter()
            .map(|line| line.text)
            .collect();
        assert!(
            tools.iter().any(|line| line.contains("Skill hello")),
            "AE1: the skill call names the skill: {tools:#?}"
        );
    }

    /// *Rozmowa*: the bar is cyan with the keys in the tab, grey without
    /// them, in the same cells.
    #[test]
    fn the_conversation_tab_bar_follows_the_focus() {
        let mut tab = conversation_tab(three_entries());
        let focused = draw_conversation(&mut tab, &Styles::new(false), false, true);
        assert_eq!(bar_of(&focused, "line 3"), ("│".to_string(), Color::Cyan));
        assert_eq!(bar_of(&focused, "line 2").0, " ");

        let unfocused = draw_conversation(&mut tab, &Styles::new(false), false, false);
        assert_eq!(spot(&unfocused, "line 3"), spot(&focused, "line 3"));
        assert_eq!(
            bar_of(&unfocused, "line 3"),
            ("│".to_string(), Color::DarkGray)
        );
    }

    /// A wrapped entry has the bar beside every row of its text, and a
    /// stale tab greys the text but not the bar.
    #[test]
    fn a_wrapped_entry_has_the_bar_on_every_row_and_stale_keeps_its_colour() {
        let long = "word ".repeat(30);
        let mut tab = conversation_tab(vec![entry("1", &["short"]), entry("2", &[long.trim()])]);
        let buffer = draw_conversation(&mut tab, &Styles::new(false), false, true);
        let (first, x) = spot(&buffer, "word word");
        let wrapped: Vec<u16> = (first..buffer.area.height)
            .take_while(|&y| cell_at(&buffer, x, y).0 != " ")
            .collect();
        assert_eq!(wrapped.len(), 3, "{}", screen(&buffer));
        for &y in &wrapped {
            assert_eq!(
                cell_at(&buffer, bar_position(&buffer, "word word").0, y),
                ("│".to_string(), Color::Cyan),
                "{}",
                screen(&buffer)
            );
        }
        assert_eq!(bar_of(&buffer, "short").0, " ");

        let stale = draw_conversation(&mut tab, &Styles::new(false), true, true);
        assert_eq!(
            cell_at(&stale, bar_position(&stale, "word word").0, first),
            ("│".to_string(), Color::Cyan)
        );
        assert_eq!(
            cell_at(&stale, x, first).1,
            Color::DarkGray,
            "the text is grey"
        );
    }

    #[test]
    fn expanded_multiline_entry_remains_readable_at_narrow_width() {
        let narrow_entry =
            entry("1", &["first line remains visible when wrapped"]).with_details(vec![
                narsor::tui::transcript::TranscriptDetailLine::new(
                    "second line remains visible when wrapped",
                    narsor::tui::transcript::TranscriptPresentation::Operator,
                ),
            ]);
        let mut tab = conversation_tab(vec![narrow_entry]);
        let id = tab.transcript().selected_entry().cloned().unwrap();
        tab.transcript_mut().toggle_expanded(&id);

        let buffer = draw_conversation_at(20, 12, &mut tab, &Styles::new(false), false, true);
        let text = screen(&buffer);
        assert!(text.contains("first line"), "{text}");
        assert!(text.contains("second line"), "{text}");
        assert!(
            text.contains("⌄"),
            "expanded marker remains visible: {text}"
        );
        assert_eq!(
            cell_at(&buffer, 0, row_of(&buffer, "first") as u16),
            ("│".into(), Color::Cyan)
        );
    }

    #[test]
    fn expanded_group_keeps_its_selection_rail_and_event_text_at_narrow_width() {
        let hook = |id: &str, event: &str, phase: &str| {
            entry(id, &[event])
                .with_header("SYSTEM · Harness · 2026-10-13 12:00:00")
                .with_presentation(narsor::tui::transcript::TranscriptPresentation::System)
                .with_operation_grouping(
                    Some("session"),
                    Some("SessionStart"),
                    Some(phase),
                    Some(id),
                )
        };
        let mut tab = conversation_tab(vec![
            hook("1", "hook_started", "started"),
            hook("2", "hook_response", "completed"),
        ]);
        let id = tab.transcript().selected_entry().cloned().unwrap();
        tab.transcript_mut().toggle_expanded(&id);

        let buffer = draw_conversation_at(30, 12, &mut tab, &Styles::new(false), false, true);
        let text = screen(&buffer);
        assert!(text.contains("⌄ SYSTEM"), "{text}");
        assert!(text.contains("Hooki · SessionStart"), "{text}");
        assert!(text.contains("hook_started"), "{text}");
        assert_eq!(
            bar_of(&buffer, "Hooki · SessionStart"),
            ("│".into(), Color::Cyan)
        );
    }

    /// *Zdarzenia*: the selected event has the bar in the first column of
    /// the tab, no `▸` and no reverse video anywhere.
    #[test]
    fn the_selected_event_has_the_bar_and_no_reverse_video() {
        let mut app = running_app();
        press(&mut app, KeyCode::Char(']'));
        app.on_detail_read(
            "a",
            1,
            DetailRead::Events {
                after: 0,
                result: Ok(ExecutionEventsResponse::Available {
                    events: execution_events(4),
                    has_more: false,
                    source: narsor_engine::controller_protocol::ExecutionEventsSource::Runner,
                }),
            },
        );
        for (no_color, colour) in [(false, Color::Cyan), (true, Color::Reset)] {
            let buffer = render(&mut app, &Styles::new(no_color));
            let screen = screen(&buffer);
            assert!(!below_the_frame_title(&buffer).contains('▸'), "{screen}");
            let x = content_column(&buffer);
            let rows = event_rows(&buffer);
            assert_eq!(rows.len(), 4, "{screen}");
            for &y in &rows {
                assert!(none_reversed(row_cells(&buffer, y, x..x + 40)), "{screen}");
            }
            let bars: Vec<u16> = rows
                .iter()
                .copied()
                .filter(|&y| cell_at(&buffer, x, y).0 != " ")
                .collect();
            assert_eq!(bars.len(), 1, "one selected event:\n{screen}");
            assert_eq!(cell_at(&buffer, x, bars[0]), ("│".to_string(), colour));
            assert_eq!(cell_at(&buffer, x + 1, bars[0]).0, " ");
            if no_color {
                assert!(bold_at(&buffer, x, bars[0]));
            }
        }
        press(&mut app, KeyCode::Esc);
        let unfocused = render(&mut app, &Styles::new(false));
        let x = content_column(&unfocused);
        let bars: Vec<(String, Color)> = event_rows(&unfocused)
            .into_iter()
            .map(|y| cell_at(&unfocused, x, y))
            .filter(|(symbol, _)| symbol != " ")
            .collect();
        assert_eq!(bars, vec![("│".to_string(), Color::DarkGray)]);
    }

    /// The rows of the screen that list an event.
    fn event_rows(buffer: &Buffer) -> Vec<u16> {
        (0..buffer.area.height)
            .filter(|&y| rows(buffer)[usize::from(y)].contains("state_entered"))
            .collect()
    }

    /// The first column of the details' content: where the tab bar starts.
    fn content_column(buffer: &Buffer) -> u16 {
        spot(buffer, "Przebieg").1 - 1
    }

    /// A read of `review`'s conversation from `source`, written three
    /// minutes ago when it is the record.
    fn read_from(
        source: narsor_engine::controller_protocol::ExecutionEventsSource,
        entries: Vec<TranscriptEntry>,
    ) -> AttemptRead {
        let next_entry = entries.len() as u64;
        let record = source == narsor_engine::controller_protocol::ExecutionEventsSource::Storage;
        AttemptRead {
            entries,
            next_entry,
            closed: false,
            truncated_tail: false,
            complete: true,
            no_record: false,
            source,
            recorded_at: record
                .then(|| chrono::Utc::now() - chrono::Duration::seconds(3 * 60 + 10)),
        }
    }

    /// *Rozmowa* of `review`, its one attempt read as `read`.
    fn conversation_of(read: AttemptRead) -> Conversation {
        let mut review = position("review", PositionStatus::InProgress);
        review.conversation = ConversationState::Recorded;
        let mut attempts = Attempts::default();
        attempts.apply("act-review", 1, 0, Ok(read));
        let mut conversation = Conversation::new("a");
        conversation.rebuild(&[review], &attempts);
        conversation
    }

    /// AE5: read from the record, *Rozmowa* and the step's conversation say
    /// so with the record's age — in words, so without colour too — and a
    /// page of the live runner has no badge.
    #[test]
    fn a_conversation_read_from_the_record_has_a_badge_with_its_age() {
        use narsor_engine::controller_protocol::ExecutionEventsSource::{Runner, Storage};
        for no_color in [false, true] {
            let styles = Styles::new(no_color);
            let mut record = conversation_of(read_from(Storage, three_entries()));
            let text = screen(&draw_conversation(&mut record, &styles, false, true));
            assert!(text.contains("zapis sprzed 3 min"), "{text}");
            assert!(text.contains("line 3"), "{text}");
            let mut live = conversation_of(read_from(Runner, three_entries()));
            let text = screen(&draw_conversation(&mut live, &styles, false, true));
            assert!(!text.contains("zapis"), "{text}");

            let mut app = recorded_app(three_entries());
            app.on_detail_read(
                "a",
                1,
                DetailRead::Attempt {
                    attempt_id: "act-review".into(),
                    from: 3,
                    result: Ok(read_from(Storage, Vec::new())),
                },
            );
            press(&mut app, KeyCode::Enter);
            let text = screen(&render(&mut app, &styles));
            assert!(text.contains("zapis sprzed 3 min"), "{text}");
            assert!(text.contains("line 3"), "the entries stay: {text}");
        }
    }

    /// The details of an interrupted task say what it waits for; the row
    /// is marked only when its owner has to act, and stays where it was.
    #[test]
    fn an_interrupted_task_says_what_it_waits_for() {
        use crate::task_app_support::{exhausted, interrupted};
        use narsor_engine::orchestration::RunnerState;
        let cases = [
            (
                interrupted("b", RunnerState::Running, 2, true),
                "wznowienie automatyczne, próba 2/5",
                false,
            ),
            (
                exhausted(
                    interrupted("b", RunnerState::Running, 6, false),
                    "could not resume automatically: pod evicted",
                ),
                "nie udało się wznowić automatycznie: pod evicted, uruchom od nowa (s)",
                true,
            ),
            (
                interrupted("b", RunnerState::PauseRequested, 0, false),
                "utracone podczas wstrzymywania, czeka na Ciebie (s)",
                true,
            ),
        ];
        for (lost, words, marked) in cases {
            for no_color in [false, true] {
                let mut app = app();
                let first = displayed(queued("a", 1), "NAR-1", &[]);
                let third = displayed(queued("c", 3), "NAR-3", &[]);
                let mut lost = displayed(lost.clone(), "NAR-2", &[]);
                lost.rank = 2;
                app.load(listing(vec![first, lost, third], "c"));
                press(&mut app, KeyCode::Down);
                assert_eq!(app.list().selected_id(), Some("b"));
                let buffer = render(&mut app, &Styles::new(no_color));
                let text = screen(&buffer);
                assert!(text.contains(words), "{text}");
                // The list is the first 28 columns.
                let row = rows(&buffer)
                    .into_iter()
                    .map(|row| row.chars().take(28).collect::<String>())
                    .find(|row| row.contains("NAR-2"))
                    .expect("the row");
                assert_eq!(row.contains("! "), marked, "{row}");
                let order: Vec<&str> = app
                    .list()
                    .tasks()
                    .iter()
                    .map(|task| task.task_id.as_str())
                    .collect();
                assert_eq!(order, ["a", "b", "c"]);
            }
        }
    }

    /// A narrow terminal shortens the badge to the age.
    #[test]
    fn a_narrow_conversation_shortens_the_badge() {
        use narsor_engine::controller_protocol::ExecutionEventsSource::Storage;
        let mut record = conversation_of(read_from(Storage, three_entries()));
        let buffer = draw(14, 6, |frame| {
            conversation::render(
                frame,
                frame.area(),
                &Styles::new(true),
                &mut record,
                false,
                true,
                "⠋",
            );
        });
        assert!(
            screen(&buffer).contains("zapis · 3 min"),
            "{}",
            screen(&buffer)
        );
    }

    /// An empty record and an unreadable one say so in their own words.
    #[test]
    fn an_empty_or_unreadable_record_says_so() {
        use narsor::api::ClientError;
        use narsor::tui::task_app::tabs::conversation::ReadFailure;
        use narsor_engine::controller_protocol::ExecutionEventsSource::Storage;
        let mut empty = conversation_of(read_from(Storage, Vec::new()));
        let text = screen(&draw_conversation(
            &mut empty,
            &Styles::new(false),
            false,
            true,
        ));
        assert!(text.contains("brak zapisanej rozmowy"), "{text}");

        let error = ClientError::Refused {
            code: "conversation_record_unreadable".to_string(),
            message: "the conversation record could not be read: permission denied".to_string(),
        };
        let mut review = position("review", PositionStatus::InProgress);
        review.conversation = ConversationState::Recorded;
        let mut attempts = Attempts::default();
        attempts.apply(
            "act-review",
            1,
            0,
            Err(ReadFailure::of(&error, "ignored".to_string())),
        );
        let mut unreadable = Conversation::new("a");
        unreadable.rebuild(&[review], &attempts);
        let text = screen(&draw_conversation(
            &mut unreadable,
            &Styles::new(false),
            false,
            true,
        ));
        assert!(
            text.contains("nie udało się odczytać zapisu: permission denied"),
            "{text}"
        );
    }

    // -- Ticket and title in the header and in Info -----------------------

    /// A running task with `ticket` and `title`, its id starting `6a7ceacf`.
    fn labelled_app(ticket: Option<&str>, title: Option<&str>) -> TaskApp {
        let mut app = app();
        let mut running = with_runner(
            with_controller(task("a", "local", "running", 1)),
            ExecutionStatus::Running,
            "review",
        );
        running.task_id = narsor_engine::domain::TaskId::parse("6a7ceacfdc604d52").unwrap();
        running.ticket = ticket.map(str::to_string);
        running.title = title.map(str::to_string);
        app.load(listing(vec![displayed(running, "NAR-12 opis", &[])], "c"));
        app.list_mut().select("6a7ceacfdc604d52");
        press(&mut app, KeyCode::Enter);
        app
    }

    #[test]
    fn the_header_shows_ticket_and_title_on_the_left_and_the_short_id_on_the_right() {
        let mut app = labelled_app(Some("NAR-12"), Some("Zmiana API zamówień"));
        let rows = rows(&render(&mut app, &Styles::new(false)));
        let row = rows
            .iter()
            .find(|row| row.contains("NAR-12") && row.contains("Zmiana API zamówień"))
            .unwrap_or_else(|| panic!("no label row in {rows:#?}"));
        assert!(row.contains("6a7ceacf"), "{row}");
        assert!(!row.contains("6a7ceacfd"), "only 8 characters: {row}");
        assert!(row.find("NAR-12").unwrap() < row.find("6a7ceacf").unwrap());
        assert!(rows.iter().any(|row| row.contains('╭')), "framed");
    }

    #[test]
    fn a_task_with_only_a_ticket_or_only_a_title_shows_just_that() {
        let mut ticket_only = labelled_app(Some("NAR-12"), None);
        let screen_text = screen(&render(&mut ticket_only, &Styles::new(false)));
        assert!(screen_text.contains("NAR-12") && screen_text.contains("6a7ceacf"));

        let mut title_only = labelled_app(None, Some("Zmiana API"));
        let screen_text = screen(&render(&mut title_only, &Styles::new(false)));
        assert!(screen_text.contains("Zmiana API") && screen_text.contains("6a7ceacf"));
    }

    #[test]
    fn a_task_without_ticket_and_title_has_no_label_block() {
        let mut app = labelled_app(None, None);
        let rows = rows(&render(&mut app, &Styles::new(false)));
        assert!(
            !rows
                .iter()
                .any(|row| row.contains("╭─") && row.contains("6a7ceacf")),
            "no empty frames"
        );
        assert!(
            rows[IDENTITY_ROW].contains("NAR-12 opis"),
            "{}",
            rows[IDENTITY_ROW]
        );
    }

    #[test]
    fn a_long_title_wraps_and_the_internal_number_stays_whole_at_60_columns() {
        let title = "Przebudowa obsługi zamówień hurtowych wraz z migracją danych klientów";
        let mut app = labelled_app(Some("NAR-12"), Some(title));
        let buffer = render_at(&mut app, &Styles::new(false), 60, 24);
        let rows = rows(&buffer);
        let screen_text = screen(&buffer);
        assert!(screen_text.contains("6a7ceacf"), "{screen_text}");
        assert!(screen_text.contains("Przebudowa"), "{screen_text}");
        assert!(
            screen_text.contains("klientów"),
            "the whole title fits: {screen_text}"
        );
        assert!(rows.iter().filter(|row| row.contains('│')).count() >= 3);
    }

    #[test]
    fn the_label_block_leaves_the_tabs_and_content_at_60_by_20() {
        let title = "x ".repeat(80);
        let mut app = labelled_app(Some("NAR-12"), Some(title.trim()));
        let buffer = render_at(&mut app, &Styles::new(false), 60, 20);
        let screen_text = screen(&buffer);
        assert!(screen_text.contains("6a7ceacf"), "{screen_text}");
        assert!(
            screen_text.contains("Przebieg") && screen_text.contains("Info"),
            "{screen_text}"
        );
    }

    #[test]
    fn wide_characters_in_the_title_keep_the_frames_aligned() {
        let mut app = labelled_app(
            Some("NAR-12"),
            Some("変更 注文 API 🚀 の修正を行う長いタイトル"),
        );
        let rows = rows(&render_at(&mut app, &Styles::new(false), 60, 24));
        assert!(rows.iter().any(|row| row.contains("6a7ceacf")));
    }

    #[test]
    fn info_lists_the_ticket_and_title_only_when_there_are_any() {
        let mut with = labelled_app(Some("NAR-12"), Some("Zmiana API"));
        for _ in 0..3 {
            press(&mut with, KeyCode::Char(']'));
        }
        let info = screen(&render(&mut with, &Styles::new(false)));
        assert!(info.contains("ticket") && info.contains("NAR-12"), "{info}");
        assert!(info.contains("Zmiana API"), "{info}");

        let mut without = labelled_app(None, None);
        for _ in 0..3 {
            press(&mut without, KeyCode::Char(']'));
        }
        let info = screen(&render(&mut without, &Styles::new(false)));
        assert!(!info.contains("ticket"), "{info}");
    }
}

/// U10: the panel "Czeka na Ciebie" drawn at the bottom of the details.
mod task_app_waiting_render {
    use super::*;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use narsor::api::ApiClient;
    use narsor::tui::task_app::detail::DetailRead;
    use narsor::tui::task_app::{layout, TaskApp};
    use narsor_engine::domain::ExecutionStatus;
    use narsor_engine::orchestration::AttentionKind;
    use serde_json::json;

    use crate::task_app_support::{
        listing, position, task, timeline, titled, waiting_on, with_controller, with_pending,
        with_runner,
    };

    fn press(app: &mut TaskApp, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn type_text(app: &mut TaskApp, text: &str) {
        for character in text.chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
    }

    fn running() -> narsor_engine::controller_protocol::TaskSummary {
        with_runner(
            with_controller(task("a", "local", "running", 1)),
            ExecutionStatus::InputNeeded,
            "review_scope",
        )
    }

    fn gate() -> serde_json::Value {
        json!({
            "execution_id": "exec-1",
            "state_id": "review_scope",
            "action_execution_id": "act-7",
            "instruction": "Czy zakres zmian obejmuje też migrację danych?",
            "fields": [{ "name": "comment", "type": "text", "required": true }],
            "outputs": [
                { "label": "approved" },
                { "label": "revise", "description": "Zakres wymaga poprawy. Agent planu wznowi swoją sesję.", "resume_comment_field": "comment" },
                { "label": "reject" }
            ],
            "resume_targets": { "revise": { "action_execution_id": "act-plan", "harness": "claude", "session_id": "s1" } },
            "episode": "ep-1",
            "requested_at": "2026-10-09T11:54:00Z"
        })
    }

    fn problem() -> serde_json::Value {
        json!({
            "execution_id": "exec-1",
            "state_id": "implement",
            "action_execution_id": "act-9",
            "instruction": "Krok nie dał etykiety",
            "outputs": [{ "label": "implemented", "description": "Funkcja gotowa." }],
            "continuation": "resolve_output",
            "problem": {
                "reason": "Odpowiedź agenta nie zawiera etykiety.",
                "answers": [],
                "requirements": [{ "label": "implemented", "variables": ["summary"] }],
                "analysis": { "label": "implemented", "variables": { "summary": "all units done" }, "rationale": "Testy przechodzą." }
            },
            "episode": "ep-p",
            "requested_at": "2026-10-09T11:54:00Z"
        })
    }

    /// The gate with six outputs, `out1` … `out6`, and a required comment.
    fn six_actions() -> serde_json::Value {
        let mut form = gate();
        form["outputs"] = (1..=6)
            .map(|index| json!({ "label": format!("out{index}") }))
            .collect();
        form["resume_targets"] = json!({});
        form
    }

    fn opened(task: narsor_engine::controller_protocol::TaskSummary) -> TaskApp {
        let mut app = TaskApp::new(
            ApiClient::new("127.0.0.1:9".parse().unwrap(), "token"),
            "local",
            None,
        );
        app.load(listing(vec![titled(task, "NAR-12")], "c"));
        // The step the output `revise` returns to is known from the timeline.
        app.on_detail_read(
            "a",
            1,
            DetailRead::Timeline(Ok(timeline(
                "a",
                vec![position(
                    "plan",
                    narsor_engine::engine::PositionStatus::Succeeded,
                )],
                None,
            ))),
        );
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Enter);
        app
    }

    fn draw_app(app: &mut TaskApp, width: u16, height: u16, no_color: bool) -> String {
        screen(&draw(width, height, |frame| {
            layout::render(frame, app, &Styles::new(no_color), Instant::now())
        }))
    }

    /// The answer panel stays visible through its background and focus mark.
    #[test]
    fn with_focus_in_the_panel_backgrounds_and_markers_replace_separator_rules() {
        let mut app = opened(with_pending(running(), gate()));
        assert!(app.panel_focused());
        let buffer = draw(120, 40, |frame| {
            layout::render(frame, &mut app, &Styles::new(false), Instant::now())
        });
        assert_frame(&buffer, layout::LIST_WIDTH, 119, ROUNDED, Color::Cyan);
        let heading = row_of(&buffer, "? Pytanie · review_scope");
        let y = u16::try_from(heading).unwrap();
        assert_eq!(buffer[(layout::LIST_WIDTH + 1, y)].symbol(), "│");
        assert_eq!(
            buffer[(layout::LIST_WIDTH + 2, y)].bg,
            Color::Rgb(38, 40, 43)
        );
        let header_color = Color::Rgb(34, 48, 72);
        assert!(buffer.content.iter().any(|cell| cell.bg == header_color));
        assert!(
            !rows(&buffer)[usize::from(y - 1)].contains('─'),
            "the panel has no separator rule:\n{}",
            screen(&buffer)
        );
    }

    #[test]
    fn focus_changes_the_global_hint_line_and_survives_without_colour() {
        let mut app = opened(with_pending(running(), gate()));
        let panel = draw_app(&mut app, 120, 40, false);
        assert!(panel.lines().last().unwrap().contains("szczegóły"));

        press(&mut app, KeyCode::Tab);
        assert!(!app.panel_focused());
        let details = draw_app(&mut app, 120, 40, false);
        assert!(details.lines().last().unwrap().contains("panel"));

        press(&mut app, KeyCode::BackTab);
        let buffer = draw_buffer(&mut app, 120, 40, true);
        let heading = row_of(&buffer, "? Pytanie · review_scope");
        let y = u16::try_from(heading).unwrap();
        let marker = &buffer[(layout::LIST_WIDTH + 1, y)];
        assert_eq!(marker.symbol(), "│");
        assert!(marker.modifier.contains(Modifier::BOLD));
        assert_eq!(marker.bg, Color::Reset);
        for row in y..y + 5 {
            assert_eq!(buffer[(layout::LIST_WIDTH + 1, row)].symbol(), "│");
        }
    }

    #[test]
    fn wide_the_panel_shows_context_actions_description_and_fields() {
        let mut app = opened(with_pending(running(), gate()));
        let buffer = draw_buffer(&mut app, 120, 40, false);
        let screen = screen(&buffer);
        for expected in [
            "? Pytanie · review_scope",
            "Czy zakres zmian obejmuje też migrację danych?",
            "Akcje",
            "Opis",
            "| 3 | revise",
            "↩ wraca do agenta: plan",
            "Zakres wymaga poprawy.",
            "comment (wymagane)",
        ] {
            assert!(
                screen.contains(expected),
                "{expected:?} is missing:\n{screen}"
            );
        }
        let list_row = screen
            .lines()
            .position(|row| row.contains("| 3 | revise"))
            .unwrap();
        let context_title = screen
            .lines()
            .position(|row| row.contains("? Pytanie"))
            .unwrap();
        let context = screen
            .lines()
            .position(|row| row.contains("Czy zakres zmian"))
            .unwrap();
        let actions_title = screen
            .lines()
            .position(|row| row.contains("Akcje"))
            .unwrap();
        let description_row = screen
            .lines()
            .position(|row| row.contains("↩ wraca do agenta: plan"))
            .unwrap();
        assert!(
            context >= context_title + 2,
            "the context is spaced from its title:\n{screen}"
        );
        assert!(
            list_row > context + 1,
            "the actions are spaced from the context:\n{screen}"
        );
        assert!(screen.lines().nth(actions_title).unwrap().contains('│'));
        assert!(
            description_row <= list_row + 1,
            "the description sits beside the list:\n{screen}"
        );
        let tabs_row = row_of(&buffer, "Przebieg");
        let divider_y = u16::try_from(tabs_row - 1).unwrap();
        assert_eq!(buffer[(layout::LIST_WIDTH + 4, divider_y)].symbol(), "─");
        assert_eq!(buffer[(layout::LIST_WIDTH + 4, divider_y)].fg, Color::Black);
    }

    /// A gate after an agent step, with the agent's reason and the values its
    /// choice required, as the engine writes them (KTD11).
    fn gate_after_agent(reason: &str) -> serde_json::Value {
        let mut form = gate();
        form["reason"] = json!(reason);
        form["required_values"] = json!([
            { "name": "plan_blocker", "kind": "variable", "value": "Brak decyzji o migracji." },
            { "name": "plan_doc", "kind": "artifact", "value": "docs/plan.md" }
        ]);
        form
    }

    fn approval(result: serde_json::Value) -> serde_json::Value {
        json!({
            "execution_id": "exec-1",
            "state_id": "doc_review",
            "action_execution_id": "act-7",
            "instruction": "Zatwierdź przegląd dokumentu",
            "outputs": [{ "label": "approved", "description": "Dokument przeszedł przegląd." }],
            "continuation": "approve_output",
            "approval": {
                "action_execution_id": "act-7",
                "result": result,
                "agent_session_id": "sess-1"
            },
            "episode": "ep-a",
            "requested_at": "2026-10-09T11:54:00Z"
        })
    }

    #[test]
    fn a_gate_after_an_agent_shows_its_reason_and_the_required_values() {
        let mut app = opened(with_pending(
            running(),
            gate_after_agent("Plan czeka na decyzję o migracji."),
        ));
        let screen = draw_app(&mut app, 120, 40, false);
        for expected in [
            "uzasadnienie: Plan czeka na decyzję o migracji.",
            "plan_blocker: Brak decyzji o migracji.",
            "plan_doc: docs/plan.md",
        ] {
            assert!(
                screen.contains(expected),
                "{expected:?} is missing:\n{screen}"
            );
        }
    }

    #[test]
    fn a_gate_whose_agent_gave_no_reason_says_so() {
        // The engine writes the placeholder when the agent gave none (R3).
        let mut app = opened(with_pending(
            running(),
            gate_after_agent("brak uzasadnienia"),
        ));
        let screen = draw_app(&mut app, 120, 40, false);
        assert!(
            screen.contains("uzasadnienie: brak uzasadnienia"),
            "{screen}"
        );
    }

    #[test]
    fn an_escape_in_the_reason_never_reaches_the_terminal() {
        let pending = gate_after_agent("ok\u{1b}[31m czerwony\u{7}");
        let view: narsor_protocol::api::PendingHumanInputView =
            serde_json::from_value(pending.clone()).unwrap();
        let panel = narsor::tui::input_form::InputPanel::new(&view, &|_| None);
        assert!(
            panel
                .context()
                .iter()
                .all(|line| !line.text.chars().any(char::is_control)),
            "{:?}",
            panel.context()
        );

        let mut app = opened(with_pending(running(), pending));
        let screen = draw_app(&mut app, 120, 40, false);
        assert!(!screen.contains('\u{1b}'), "{screen:?}");
        assert!(screen.contains("czerwony"), "{screen}");
    }

    #[test]
    fn an_approval_shows_the_choice_reason_variables_and_artifacts_not_raw_data() {
        let pending = approval(json!({
            "output_label": "approved",
            "reason": "Dokument jest kompletny.",
            "data": { "agent_session": "sess-1" },
            "variables": { "review_summary": "Wszystkie sekcje opisane." },
            "artifacts": { "review_doc": "docs/review.md" },
            "exit_code": null, "stdout": "", "stderr": ""
        }));
        let view: narsor_protocol::api::PendingHumanInputView =
            serde_json::from_value(pending.clone()).unwrap();
        let panel = narsor::tui::input_form::InputPanel::new(&view, &|_| None);
        let context: Vec<&str> = panel
            .context()
            .iter()
            .map(|line| line.text.as_str())
            .collect();
        assert_eq!(
            context,
            [
                "Zatwierdź przegląd dokumentu",
                "wybór agenta: approved",
                "Dokument przeszedł przegląd.",
                "uzasadnienie: Dokument jest kompletny.",
                "review_summary: Wszystkie sekcje opisane.",
                "review_doc: docs/review.md",
            ]
        );

        // The panel scrolls its context; the head of it is on screen.
        let mut app = opened(with_pending(running(), pending));
        let screen = draw_app(&mut app, 120, 40, false);
        assert!(screen.contains("wybór agenta: approved"), "{screen}");
        assert!(!screen.contains("agent_session"), "no raw data:\n{screen}");
    }

    #[test]
    fn an_approval_without_a_reason_says_so() {
        let mut app = opened(with_pending(
            running(),
            approval(json!({
                "output_label": "approved",
                "variables": {},
                "exit_code": null, "stdout": "", "stderr": ""
            })),
        ));
        let screen = draw_app(&mut app, 120, 40, false);
        assert!(
            screen.contains("uzasadnienie: brak uzasadnienia"),
            "{screen}"
        );
    }

    #[test]
    fn the_agent_reply_composer_sits_below_the_action_list_as_a_separate_field() {
        let mut app = opened(with_pending(running(), problem()));
        let screen = draw_app(&mut app, 120, 40, false);
        assert!(screen.contains("implemented"), "{screen}");
        assert!(screen.contains("Wiadomość dla agenta"), "{screen}");
        // Action 0 is the entry that moves the focus to that one field.
        assert!(screen.contains("Napisz do agenta"), "{screen}");
        let action_row = screen
            .lines()
            .position(|line| line.contains("implemented"))
            .unwrap();
        let composer_row = screen
            .lines()
            .position(|line| line.contains("Wiadomość dla agenta"))
            .unwrap();
        assert!(composer_row > action_row, "{screen}");
    }

    #[test]
    fn a_blurred_composer_has_no_active_cursor_or_focus_color() {
        let mut app = opened(with_pending(running(), problem()));
        press(&mut app, KeyCode::Down);
        type_text(&mut app, "szkic");
        let focused = draw_buffer(&mut app, 120, 40, false);
        assert!(!cursor_cells(&focused).is_empty());

        press(&mut app, KeyCode::Tab);
        let blurred = draw_buffer(&mut app, 120, 40, false);
        assert!(cursor_cells(&blurred).is_empty());
        assert!(screen(&blurred).contains("szkic"));
    }

    #[test]
    fn a_three_row_composer_scrolls_its_cursor_back_to_the_first_line() {
        let mut app = opened(with_pending(running(), problem()));
        press(&mut app, KeyCode::Down);
        for (index, line) in ["line1", "line2", "line3", "line4"].into_iter().enumerate() {
            if index > 0 {
                app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
            }
            type_text(&mut app, line);
        }
        for _ in 0..3 {
            press(&mut app, KeyCode::Up);
        }
        type_text(&mut app, "X");

        let screen = draw_app(&mut app, 120, 40, false);
        assert!(
            screen.contains("line1X"),
            "the cursor's row stays in view:\n{screen}"
        );
        assert!(
            screen.contains("line2"),
            "the next composer row stays visible:\n{screen}"
        );
        assert!(
            !screen.contains("line4"),
            "the cursor row controls scrolling:\n{screen}"
        );
    }

    #[test]
    fn a_short_panel_keeps_an_action_row_when_the_composer_is_present() {
        let mut app = opened(with_pending(running(), problem()));
        let screen = draw_app(&mut app, 60, 20, false);
        assert!(
            screen.contains("implemented"),
            "the action remains visible:\n{screen}"
        );
    }

    /// R12: with the keys on the actions the bracket is in the focus role;
    /// in the fields of the panel it is grey.
    #[test]
    fn the_bracket_on_the_actions_follows_the_focus_of_the_panel() {
        let mut app = opened(with_pending(running(), gate()));
        let buffer = draw_buffer(&mut app, 120, 40, false);
        assert_action_number(&buffer, "revise", 3);
        assert_not_bracketed(&buffer, "2   approved");

        press(&mut app, KeyCode::Tab);
        let buffer = draw_buffer(&mut app, 120, 40, false);
        assert_action_number(&buffer, "revise", 3);
        let y = row_of(&buffer, "revise") as u16;
        let x = column_in(&buffer, usize::from(y), "revise");
        assert_eq!(buffer[(x - 4, y)].fg, Color::DarkGray);
    }

    /// R12: a panel that appears while the keys are in the tab's content has
    /// a grey bracket.
    #[test]
    fn with_the_keys_in_the_tab_the_bracket_on_the_actions_is_grey() {
        let mut app = opened(running());
        assert!(!app.panel_focused());
        app.load(listing(
            vec![titled(with_pending(running(), gate()), "NAR-12")],
            "d",
        ));
        let buffer = draw_buffer(&mut app, 120, 40, false);
        assert!(!app.panel_focused(), "{}", screen(&buffer));
        assert_action_number(&buffer, "revise", 3);
    }

    /// Without colour the selected number stays visible and risk markers lead
    /// the description.
    #[test]
    fn without_colour_the_bracket_is_bold_and_keeps_the_markers() {
        let mut app = opened(with_pending(running(), gate()));
        let buffer = draw_buffer(&mut app, 120, 40, true);
        assert_action_number(&buffer, "revise", 3);

        let mut app = opened(with_pending(running(), problem()));
        let buffer = draw_buffer(&mut app, 120, 40, false);
        assert_action_number(&buffer, "implemented", 2);
        assert!(screen(&buffer).contains("! Problem"));
    }

    /// AE7, R17: six actions in a panel that fits five scroll inside the
    /// list; the question and the comment field stay where they were.
    #[test]
    fn ae7_the_actions_scroll_inside_the_panel_and_the_rest_stays() {
        let mut app = opened(with_pending(running(), six_actions()));
        // Action 0 is the first entry: `out1` is the second.
        press(&mut app, KeyCode::Char('2'));
        let before = draw_buffer(&mut app, 120, 24, false);
        let question = row_of(&before, "Czy zakres zmian");
        let field = row_of(&before, "comment (wymagane)");
        let shown = (1..=6)
            .filter(|index| screen(&before).contains(&format!("out{index}")))
            .count();
        assert!(
            shown >= 4,
            "the action list remains visible:\n{}",
            screen(&before)
        );

        for _ in 0..5 {
            press(&mut app, KeyCode::Char('j'));
        }
        let after = draw_buffer(&mut app, 120, 24, false);
        assert_action_number(&after, "out6", 7);
        assert_eq!(row_of(&after, "Czy zakres zmian"), question);
        assert_eq!(row_of(&after, "comment (wymagane)"), field);
    }

    /// At 60×20 the action bracket moves with `j`.
    #[test]
    fn at_the_smallest_terminal_one_action_shows_and_j_scrolls() {
        let mut app = opened(with_pending(running(), gate()));
        let buffer = draw_buffer(&mut app, 60, 20, false);
        assert_action_number(&buffer, "revise", 3);
        assert!(screen(&buffer).contains("comment"), "{}", screen(&buffer));

        press(&mut app, KeyCode::Char('j'));
        let buffer = draw_buffer(&mut app, 60, 20, false);
        assert_action_number(&buffer, "reject", 4);
    }

    /// R17: a refresh that builds the actions again keeps the highlighted
    /// one and where the list was scrolled.
    #[test]
    fn a_refresh_keeps_the_highlighted_action_and_the_scroll() {
        let mut app = opened(with_pending(running(), six_actions()));
        press(&mut app, KeyCode::Char('7'));
        draw_buffer(&mut app, 120, 24, false);
        press(&mut app, KeyCode::Char('k'));
        let before = draw_buffer(&mut app, 120, 24, false);
        assert!(screen(&before).contains("7   out6"), "{}", screen(&before));

        app.on_detail_read(
            "a",
            1,
            DetailRead::Timeline(Ok(timeline(
                "a",
                vec![position(
                    "plan",
                    narsor_engine::engine::PositionStatus::Succeeded,
                )],
                None,
            ))),
        );
        let after = draw_buffer(&mut app, 120, 24, false);
        assert_eq!(screen(&before), screen(&after));
        assert_action_number(&after, "out5", 6);
    }

    #[test]
    fn narrow_the_description_goes_under_the_actions() {
        let mut app = opened(with_pending(running(), gate()));
        let screen = draw_app(&mut app, 64, 40, false);
        let rows: Vec<&str> = screen.lines().collect();
        let last_action = rows
            .iter()
            .position(|row| row.contains("reject"))
            .expect("the list");
        let description = rows
            .iter()
            .position(|row| row.contains("Opis"))
            .expect("the description");
        assert!(description > last_action, "{screen}");
        assert!(screen.contains("↩ wraca do agenta: plan"), "{screen}");
    }

    #[test]
    fn without_colour_the_markers_survive() {
        let mut app = opened(with_pending(running(), gate()));
        let question = draw_app(&mut app, 120, 40, true);
        assert!(
            question.contains('?') && question.contains('↩'),
            "{question}"
        );

        let mut app = opened(with_pending(running(), problem()));
        let problem = draw_app(&mut app, 120, 40, true);
        for marker in ["! Problem", "◆", "◆ dyrygent: Testy przechodzą."] {
            assert!(
                problem.contains(marker),
                "{marker:?} is missing:\n{problem}"
            );
        }
        assert!(
            problem.contains("variables.summary ◆"),
            "the proposed value is marked:\n{problem}"
        );
    }

    #[test]
    fn the_panel_gives_its_rows_back_to_the_tab_when_folded() {
        let mut app = opened(with_pending(running(), gate()));
        let open = draw_app(&mut app, 120, 40, false);
        press(&mut app, KeyCode::Esc);
        let folded = draw_app(&mut app, 120, 40, false);
        assert!(
            open.contains("Akcje") && !folded.contains("Akcje"),
            "{folded}"
        );
        assert!(
            folded.contains("? Pytanie · review_scope") && folded.contains("Tab rozwiń"),
            "{folded}"
        );
    }

    #[test]
    fn a_failed_provisioning_and_a_hostile_diagnosis_are_drawn_safely() {
        let failed = waiting_on(
            task("a", "local", "interrupted", 1),
            AttentionKind::ProvisioningFailed,
            "provisioning-1",
            None,
        );
        let mut app = opened(failed);
        let screen = draw_app(&mut app, 120, 40, false);
        assert!(screen.contains("! Nieudany provisioning"), "{screen}");
        assert!(
            screen.contains("Uruchom od nowa") && screen.contains("Archiwizuj"),
            "{screen}"
        );
        assert!(screen.contains('!'), "the irreversible action is marked");

        let mut hostile = gate();
        hostile["instruction"] = json!("pytanie\u{1b}[31m\u{202e}ewil\u{7}");
        let mut app = opened(with_pending(running(), hostile));
        let screen = draw_app(&mut app, 120, 40, false);
        assert!(
            !screen.contains('\u{1b}') && !screen.contains('\u{202e}') && !screen.contains('\u{7}'),
            "{screen}"
        );
    }
}

/// U11: the action menu, the lists of exits and links and the confirmation
/// window, drawn.
mod task_app_actions_render {
    use super::*;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use narsor::api::ApiClient;
    use narsor::tui::task_app::{layout, TaskApp};
    use narsor_engine::controller_protocol::TaskSummary;
    use narsor_engine::domain::ExecutionStatus;

    use crate::task_app_support::{
        displayed, listing, queued, task, titled, with_controller, with_runner, with_transitions,
    };

    fn press(app: &mut TaskApp, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn running() -> TaskSummary {
        with_runner(
            with_controller(task("a", "local", "running", 1)),
            ExecutionStatus::Running,
            "review",
        )
    }

    fn opened(task: TaskSummary, keys: &str) -> TaskApp {
        let mut app = TaskApp::new(
            ApiClient::new("127.0.0.1:9".parse().unwrap(), "token"),
            "local",
            None,
        );
        app.load(listing(vec![titled(task, "NAR-12")], "c"));
        for key in keys.chars() {
            press(&mut app, KeyCode::Char(key));
        }
        app
    }

    fn draw_app(app: &mut TaskApp, width: u16, height: u16, no_color: bool) -> String {
        screen(&draw(width, height, |frame| {
            layout::render(frame, app, &Styles::new(no_color), Instant::now())
        }))
    }

    #[test]
    fn the_menu_shows_the_actions_their_keys_and_why_the_move_is_grey() {
        let mut app = opened(running(), ".");
        // Action 0 opens the menu; the move is the next entry.
        press(&mut app, KeyCode::Char('j'));
        let screen = draw_app(&mut app, 120, 40, false);
        for expected in [
            "Akcje · NAR-12",
            "c  napisz do agenta",
            "p  wstrzymaj",
            "x  przerwij krok !",
            "t  przejście ręczne",
            "a  archiwizuj !",
            "o  otwórz link",
            "m  przesuń w kolejce",
            "Opis",
        ] {
            assert!(
                screen.contains(expected),
                "{expected:?} is missing:\n{screen}"
            );
        }
        // The highlighted entry is the move, which cannot be done: its
        // description starts with the reason.
        assert!(
            screen.contains("niedostępne: zadanie nie jest w kolejce"),
            "{screen}"
        );
    }

    #[test]
    fn the_menu_fits_a_narrow_terminal_with_the_description_under_it() {
        let mut app = opened(running(), ".");
        let screen = draw_app(&mut app, 64, 40, false);
        let rows: Vec<&str> = screen.lines().collect();
        let list = rows
            .iter()
            .position(|row| row.contains("o  otwórz link"))
            .expect("the list");
        let description = rows
            .iter()
            .position(|row| row.contains("Opis"))
            .expect("the description");
        assert!(description > list, "{screen}");
    }

    /// R9, R12, R13: an overlay has the keys, so its bracket is in the
    /// focus role; the highlighted entry that cannot be done stays grey inside it,
    /// and one that can keeps its own look.
    #[test]
    fn the_menu_brackets_its_entry_in_focus_and_keeps_the_look_of_the_label() {
        let mut app = opened(running(), ".");
        let label_colour = |buffer: &Buffer, needle: &str, label: &str| {
            let y = row_of(buffer, needle);
            let x = column_in(buffer, y, label);
            cell_at(buffer, x, u16::try_from(y).unwrap()).1
        };
        let buffer = draw(120, 40, |frame| {
            layout::render(frame, &mut app, &Styles::new(false), Instant::now())
        });
        assert_bracketed(&buffer, "1 c  napisz do agenta", true, false);
        assert_eq!(
            label_colour(&buffer, "1 c  napisz do agenta", "napisz"),
            Color::DarkGray
        );

        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('j'));
        let buffer = draw(120, 40, |frame| {
            layout::render(frame, &mut app, &Styles::new(false), Instant::now())
        });
        assert_bracketed(&buffer, "3 p  wstrzymaj", true, false);
        assert_not_bracketed(&buffer, "1 c  napisz do agenta");
        assert_eq!(
            label_colour(&buffer, "3 p  wstrzymaj", "wstrzymaj"),
            Color::Reset
        );
    }

    /// R14: without colour the menu's bracket is bold, and nothing in the
    /// menu is reversed.
    #[test]
    fn without_colour_the_menu_bracket_is_bold() {
        let mut app = opened(running(), ".");
        let buffer = draw(120, 40, |frame| {
            layout::render(frame, &mut app, &Styles::new(true), Instant::now())
        });
        assert_bracketed(&buffer, "1 c  napisz do agenta", true, true);
        assert!(screen(&buffer).contains("archiwizuj !"));
    }

    #[test]
    fn the_bar_shows_the_keys_of_the_menu_and_of_the_confirmation() {
        let mut app = opened(running(), ".");
        let menu = draw_app(&mut app, 120, 40, false);
        assert!(
            menu.contains("wykonaj") && menu.contains("zamknij"),
            "{menu}"
        );
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('x'));
        let asking = draw_app(&mut app, 120, 40, false);
        assert!(
            asking.contains("potwierdź") && asking.contains("anuluj"),
            "{asking}"
        );
    }

    #[test]
    fn quitting_shows_a_dialog_with_q_and_escape_choices() {
        let mut app = opened(running(), "q");
        let screen = draw_app(&mut app, 100, 30, false);
        for expected in ["Wyjść z TUI?", "ponownie q", "q wyjdź", "Esc anuluj"] {
            assert!(
                screen.contains(expected),
                "{expected:?} is missing:\n{screen}"
            );
        }
    }

    #[test]
    fn quitting_dialog_stays_visible_in_a_too_small_terminal() {
        let mut app = opened(running(), "q");
        let screen = draw_app(&mut app, 30, 10, false);
        assert!(screen.contains("Wyjść z TUI?"), "{screen}");
        assert!(screen.contains("Esc"), "{screen}");
    }

    #[test]
    fn the_confirmation_window_says_what_will_happen_and_what_y_and_n_do() {
        let mut app = opened(running(), "x");
        let screen = draw_app(&mut app, 120, 40, false);
        for expected in [
            "Przerwij bieżący krok !",
            "Przerywa trwający krok „review” zadania NAR-12.",
            "odzyskania",
            "y potwierdź",
            "n/Esc anuluj",
        ] {
            assert!(
                screen.contains(expected),
                "{expected:?} is missing:\n{screen}"
            );
        }

        let finished = with_runner(
            with_controller(task("a", "local", "running", 1)),
            ExecutionStatus::Completed,
            "done",
        );
        let mut app = opened(finished, "a");
        let screen = draw_app(&mut app, 100, 30, false);
        assert!(
            screen.contains("Archiwizuj zadanie !")
                && screen.contains("Usuwa workload i storage zadania NAR-12.")
                && screen.contains("! Tego nie da się cofnąć."),
            "{screen}"
        );
    }

    #[test]
    fn the_confirmation_window_fits_a_narrow_terminal() {
        let mut app = opened(running(), "x");
        let screen = draw_app(&mut app, 64, 24, false);
        assert!(
            screen.contains("Przerywa trwający krok") && screen.contains("y potwierdź"),
            "{screen}"
        );
    }

    #[test]
    fn without_colour_the_bangs_and_the_reasons_survive() {
        let mut app = opened(running(), ".");
        press(&mut app, KeyCode::Char('j'));
        let menu = draw_app(&mut app, 120, 40, true);
        assert!(
            menu.contains("przerwij krok !") && menu.contains("archiwizuj !"),
            "{menu}"
        );
        assert!(
            menu.contains("niedostępne: zadanie nie jest w kolejce"),
            "{menu}"
        );
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('x'));
        let confirm = draw_app(&mut app, 120, 40, true);
        assert!(
            confirm.contains("!") && confirm.contains("y potwierdź"),
            "{confirm}"
        );
    }

    #[test]
    fn a_queued_task_has_its_move_available_and_everything_else_greyed() {
        let mut app = TaskApp::new(
            ApiClient::new("127.0.0.1:9".parse().unwrap(), "token"),
            "local",
            None,
        );
        app.load(listing(vec![queued("a", 1)], "c"));
        press(&mut app, KeyCode::Char('.'));
        press(&mut app, KeyCode::Char('3'));
        let screen = draw_app(&mut app, 120, 40, false);
        assert!(
            screen.contains("niedostępne: zadanie jest w kolejce"),
            "the pause says why:\n{screen}"
        );
    }

    #[test]
    fn the_transition_list_shows_the_exits_with_their_descriptions_and_the_override() {
        let task = with_transitions(
            running(),
            "review",
            &[
                ("approved", "ship", Some("Zatwierdza i wypuszcza zmianę.")),
                ("revise", "implement", None),
            ],
            Some(&["plan", "ship"]),
        );
        let mut app = opened(task, "t");
        let screen = draw_app(&mut app, 120, 40, false);
        for expected in [
            "Przejście ręczne · NAR-12 · z „review”",
            "approved → ship",
            "revise → implement",
            "! override — dowolny stan",
            "Zatwierdza i wypuszcza zmianę.",
        ] {
            assert!(
                screen.contains(expected),
                "{expected:?} is missing:\n{screen}"
            );
        }
        // The override asks for a target and a reason.
        press(&mut app, KeyCode::Char('3'));
        press(&mut app, KeyCode::Enter);
        let fields = draw_app(&mut app, 120, 40, false);
        assert!(
            fields.contains("stan docelowy (wymagane)") && fields.contains("powód (wymagane)"),
            "{fields}"
        );
        assert!(fields.contains("trafia do audytu"), "{fields}");
    }

    #[test]
    fn the_link_list_shows_the_addresses_it_would_open() {
        let task = displayed(
            running(),
            "NAR-12",
            &[
                ("Jira", "NAR-12", Some("https://jira.example/browse/NAR-12")),
                ("PR", "—", None),
            ],
        );
        let mut app = opened(task, "o");
        let screen = draw_app(&mut app, 120, 40, false);
        assert!(
            screen.contains("Linki · NAR-12")
                && screen.contains("Jira: NAR-12")
                && screen.contains("https://jira.example/browse/NAR-12"),
            "{screen}"
        );
        assert!(
            screen.matches("PR: —").count() == 1,
            "an empty link is in the header only, not in the list:\n{screen}"
        );
    }

    #[test]
    fn a_hostile_label_in_a_link_cannot_reach_the_terminal() {
        let task = displayed(
            running(),
            "NAR-12",
            &[(
                "Ji\u{1b}[31mra\u{202e}",
                "NAR",
                Some("https://jira.example/x"),
            )],
        );
        let mut app = opened(task, "o");
        let screen = draw_app(&mut app, 120, 40, false);
        assert!(
            !screen.contains('\u{1b}') && !screen.contains('\u{202e}'),
            "{screen}"
        );
    }
}

mod task_app_conductor_render {
    use super::*;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use narsor::api::ApiClient;
    use narsor::tui::task_app::detail::DetailRead;
    use narsor::tui::task_app::{layout, DetailTab, TaskApp};
    use narsor_engine::domain::ExecutionStatus;
    use narsor_protocol::controller::{ConductorReadResponse, ExecutionEventsSource};
    use serde_json::json;

    use crate::task_app_support::{
        chat_entry, conductor_view, listing, task, titled, with_conductor, with_controller,
        with_runner, without_conductor,
    };

    fn press(app: &mut TaskApp, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn running() -> narsor_engine::controller_protocol::TaskSummary {
        with_conductor(with_runner(
            with_controller(task("a", "local", "running", 1)),
            ExecutionStatus::Running,
            "review",
        ))
    }

    /// The tab of task `a`, with a short conversation read.
    fn opened(task: narsor_engine::controller_protocol::TaskSummary, read: bool) -> TaskApp {
        let mut app = TaskApp::new(
            ApiClient::new("127.0.0.1:9".parse().unwrap(), "token"),
            "local",
            None,
        );
        app.load(listing(vec![titled(task, "NAR-12")], "c"));
        press(&mut app, KeyCode::Enter);
        app.detail_mut().set_tab(DetailTab::Conductor);
        if read {
            let view = conductor_view(
                vec![
                    chat_entry("m1", 1, "operator", "message", "Czy revise?", None),
                    chat_entry(
                        "m2",
                        2,
                        "conductor",
                        "message",
                        "Raczej revise — brak planu migracji.",
                        None,
                    ),
                    chat_entry(
                        "d1",
                        3,
                        "conductor",
                        "decision",
                        "Wybrano revise",
                        Some(
                            json!({ "kind": "choose_output", "decision_id": "d1", "confidence": "low", "rationale": "Brak planu." }),
                        ),
                    ),
                ],
                json!({
                    "chat_has_more": true,
                    "decisions": [{
                        "decision_id": "d1", "kind": "choose_output", "text": "Wybrano revise",
                        "at": "2026-10-09T12:00:00Z", "undone": false, "undoable": true
                    }]
                }),
            );
            app.on_detail_read(
                "a",
                1,
                DetailRead::Conductor(Ok(ConductorReadResponse::Available {
                    conductor: Box::new(view),
                    source: ExecutionEventsSource::Runner,
                })),
            );
        }
        app
    }

    fn draw_app(app: &mut TaskApp, width: u16, height: u16, no_color: bool) -> Buffer {
        draw(width, height, |frame| {
            layout::render(frame, app, &Styles::new(no_color), Instant::now())
        })
    }

    #[test]
    fn wide_the_tab_has_the_conductor_the_roles_the_decision_and_the_keys() {
        let mut app = opened(running(), true);
        let text = screen(&draw_app(&mut app, 120, 30, false));
        for expected in [
            "Dyrygent",
            "◆ dyrygent · harness claude-code",
            "USER · Operator",
            "KONDUKTOR · Dyrygent",
            "Raczej revise — brak planu migracji.",
            "(decyzja; u — cofnij)",
            "pewność: low",
            "c napisz",
            "u cofnij decyzję",
            "M dalsze wpisy",
        ] {
            assert!(text.contains(expected), "{expected:?} is missing:\n{text}");
        }
        assert!(!text.contains("h harness"), "no per-task choice:\n{text}");
    }

    #[test]
    fn narrow_the_tab_still_shows_the_conversation_and_the_keys() {
        let mut app = opened(running(), true);
        let text = screen(&draw_app(&mut app, 60, 20, false));
        assert!(text.contains("◆ dyrygent"), "{text}");
        // The header takes its rows first; the conversation follows its end,
        // so the latest entry — the conductor's decision — is the one in view.
        assert!(text.contains("KONDUKTOR · Dyrygent"), "{text}");
        assert!(text.contains("Wybrano revise"), "{text}");
        assert!(text.contains("c napisz"), "{text}");
    }

    #[test]
    fn the_conductors_entries_are_magenta_and_the_operators_cyan() {
        let mut app = opened(running(), true);
        let buffer = draw_app(&mut app, 120, 30, false);
        let width = usize::from(buffer.area.width);
        let fg_of = |needle: &str| {
            let row = row_of(&buffer, needle);
            let column = rows(&buffer)[row]
                .find(needle)
                .map(|byte| rows(&buffer)[row][..byte].chars().count())
                .unwrap();
            buffer.content()[row * width + column].fg
        };
        assert_eq!(
            fg_of("KONDUKTOR"),
            Color::Magenta,
            "the role colour of the conductor"
        );
        assert_eq!(fg_of("USER"), Color::Cyan);
        assert_eq!(fg_of("◆ dyrygent"), Color::Magenta);
    }

    #[test]
    fn without_colour_the_roles_are_told_apart_by_their_words() {
        let mut app = opened(running(), true);
        let buffer = draw_app(&mut app, 120, 30, true);
        let text = screen(&buffer);
        assert!(
            text.contains("USER") && text.contains("KONDUKTOR") && text.contains("◆ dyrygent"),
            "{text}"
        );
        assert!(
            buffer
                .content()
                .iter()
                .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset),
            "NO_COLOR draws no colour at all"
        );
    }

    #[test]
    fn writing_shows_the_box_with_the_text_and_the_keys_of_the_box() {
        let mut app = opened(running(), true);
        press(&mut app, KeyCode::Char('c'));
        for character in "Dlaczego?".chars() {
            press(&mut app, KeyCode::Char(character));
        }
        let buffer = draw_app(&mut app, 120, 30, false);
        let text = screen(&buffer);
        assert!(text.contains("wiadomość (wymagane)"), "{text}");
        assert!(cursor_follows(&buffer, "> Dlaczego?"), "{text}");
        assert!(
            text.contains("Enter wyślij") && text.contains("Ctrl+J nowa linia"),
            "{text}"
        );
        assert!(text.contains("Esc koniec pisania"), "{text}");

        press(&mut app, KeyCode::Esc);
        let text = screen(&draw_app(&mut app, 120, 30, false));
        assert!(
            text.contains("✎ szkic: Dlaczego?"),
            "the draft stays in view:\n{text}"
        );
    }

    #[test]
    fn a_task_without_a_conductor_shows_the_reason_and_not_the_box() {
        let mut app = opened(
            without_conductor(running(), "brak harnessu dyrygenta"),
            false,
        );
        let text = screen(&draw_app(&mut app, 120, 30, false));
        assert!(
            text.contains("◆ dyrygent niedostępny: brak harnessu dyrygenta"),
            "{text}"
        );
        assert!(!text.contains("c napisz"), "{text}");
        let buffer = draw_app(&mut app, 100, 30, true);
        assert!(
            screen(&buffer).contains("dyrygent niedostępny"),
            "no colour, still said in words"
        );
    }

    /// R16: the selected entry's bar is cyan while the keys are on the
    /// history, grey while the operator writes a message.
    #[test]
    fn the_bar_of_the_history_greys_while_the_operator_writes() {
        let mut app = opened(running(), true);
        let bar = |buffer: &Buffer| {
            let row = row_of(buffer, "Wybrano revise");
            let text = &rows(buffer)[row];
            let column = text[..text.find("Wybrano revise").unwrap()].chars().count();
            cell_at(buffer, u16::try_from(column).unwrap() - 4, row as u16)
        };
        let reading = draw_app(&mut app, 120, 30, false);
        assert_eq!(
            bar(&reading),
            ("│".to_string(), Color::Cyan),
            "{}",
            screen(&reading)
        );

        press(&mut app, KeyCode::Char('c'));
        let writing = draw_app(&mut app, 120, 30, false);
        assert_eq!(
            bar(&writing),
            ("│".to_string(), Color::DarkGray),
            "{}",
            screen(&writing)
        );
    }

    // The harness is no longer chosen per task on this tab; the list of
    // harnesses comes back as the global chooser, with its own
    // narrow-terminal test.

    #[test]
    fn a_tiny_area_does_not_panic() {
        let mut app = opened(running(), true);
        for (width, height) in [(20, 6), (30, 8), (80, 5)] {
            draw_app(&mut app, width, height, false);
        }
        press(&mut app, KeyCode::Char('c'));
        draw_app(&mut app, 24, 7, false);
    }
}

/// U13: the new-task form, the unsent rows, the history and the repositories.
mod task_app_create_render {
    use super::*;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use narsor::api::ApiClient;
    use narsor::tui::task_app::{layout, Request, TaskApp};
    use narsor_engine::domain::ParamType;
    use narsor_protocol::api::LegacyExecutionListResponse;

    use crate::task_app_support::{
        archived, capabilities, catalogue, catalogue_workflow, legacy, listing, queued, registry,
        repository, titled, workflow_input,
    };

    fn press(app: &mut TaskApp, code: KeyCode) -> Request {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn keys(app: &mut TaskApp, text: &str) {
        for character in text.chars() {
            press(app, KeyCode::Char(character));
        }
    }

    fn app() -> TaskApp {
        let mut app = TaskApp::new(
            ApiClient::new("127.0.0.1:9".parse().unwrap(), "token"),
            "local",
            None,
        );
        app.set_capabilities(capabilities(true, None));
        app.load(listing(
            vec![
                titled(queued("a", 1), "NAR-14"),
                titled(queued("b", 2), "NAR-12"),
            ],
            "c",
        ));
        app
    }

    fn open_form(app: &mut TaskApp) {
        keys(app, "n");
        app.open_new_task(
            catalogue(vec![
                catalogue_workflow(
                    "implement-feature",
                    "Pełny cykl: plan, implementacja, review, testy i PR.",
                    vec![
                        workflow_input(
                            "ticket_id",
                            ParamType::String,
                            true,
                            Some("numer zgłoszenia"),
                        ),
                        workflow_input(
                            "attempts",
                            ParamType::Integer,
                            false,
                            Some("ile razy ponawiać"),
                        ),
                        workflow_input("dry_run", ParamType::Boolean, false, None),
                        workflow_input("target_repo", ParamType::Repository, false, None),
                    ],
                ),
                catalogue_workflow("fix-bug", "Poprawka błędu.", vec![]),
            ]),
            registry(vec![repository("narsor", "git@github.com:acme/narsor.git")]),
        );
    }

    /// The form opens on the ticket; the workflow's `ticket_id` is the fourth
    /// field, after the title and the repository.
    fn to_ticket_id(app: &mut TaskApp) {
        for _ in 0..3 {
            press(app, KeyCode::Down);
        }
    }

    fn draw_app(app: &mut TaskApp, width: u16, height: u16, no_color: bool) -> String {
        screen(&draw(width, height, |frame| {
            layout::render(frame, app, &Styles::new(no_color), Instant::now())
        }))
    }

    #[test]
    fn the_wide_form_folds_the_list_into_a_strip_and_keeps_the_untouched_task_list() {
        let mut app = app();
        open_form(&mut app);
        press(&mut app, KeyCode::Enter);
        to_ticket_id(&mut app);
        keys(&mut app, "NAR-18");
        let screen = draw_app(&mut app, 130, 34, false);
        for expected in [
            "Nowe zadanie",
            "→ trafi na koniec kolejki (#3)",
            "implement-feature",
            "Pełny cykl",
            "ticket_id: NAR-18",
            "attempts: ",
            "dry_run: ‹ false ›",
            "target_repo: ‹ narsor ›",
            "ticket_id (wymagane) · numer zgłoszenia",
            "brak pola tekstowego w tym workflow",
            // The list of tasks is where it was.
            "Zadania",
            "NAR-14",
            "NAR-12",
        ] {
            assert!(screen.contains(expected), "{expected}:\n{screen}");
        }
    }

    #[test]
    fn the_narrow_form_takes_the_whole_screen_and_folds_the_list_into_a_strip() {
        let mut app = app();
        open_form(&mut app);
        press(&mut app, KeyCode::Enter);
        to_ticket_id(&mut app);
        keys(&mut app, "NAR-18");
        let screen = draw_app(&mut app, 72, 30, false);
        for expected in [
            "Nowe zadanie",
            "→ trafi na koniec kolejki (#3)",
            "implement-feature",
            "ticket_id: NAR-18",
        ] {
            assert!(screen.contains(expected), "{expected}:\n{screen}");
        }
        assert!(!screen.contains("Zadania"), "the list gives way:\n{screen}");
        for row in screen.lines() {
            assert!(row.chars().count() <= 72, "{row}");
        }
    }

    fn open_prompt_form(app: &mut TaskApp, texts: &[&str]) {
        keys(app, "n");
        let mut inputs = vec![
            workflow_input("ticket_id", ParamType::String, false, None),
            workflow_input("dry_run", ParamType::Boolean, false, None),
            workflow_input("target_repo", ParamType::Repository, false, None),
        ];
        for (position, name) in texts.iter().enumerate() {
            inputs.push(workflow_input(name, ParamType::Text, position == 0, None));
        }
        app.open_new_task(
            catalogue(vec![catalogue_workflow("ship", "Wysyła zmianę.", inputs)]),
            registry(vec![repository("narsor", "git@github.com:acme/narsor.git")]),
        );
    }

    #[test]
    fn choosing_a_workflow_folds_the_list_and_gives_the_prompt_the_rest_of_the_screen() {
        for (width, height) in [(120, 30), (60, 20)] {
            let mut app = app();
            open_prompt_form(&mut app, &["prompt"]);
            let listed = draw_app(&mut app, width, height, false);
            assert!(listed.contains("Akcje"), "{listed}");
            // Stacked at 60×20 the description has no room for a line, so
            // it gives way to the list.
            assert_eq!(listed.contains("Opis"), width >= 64, "{listed}");

            press(&mut app, KeyCode::Enter);
            // The focus starts on the first field; the prompt is the last,
            // after the form's own ticket and title fields.
            for _ in 0..5 {
                press(&mut app, KeyCode::Down);
            }
            keys(&mut app, "pierwsza");
            app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
            keys(&mut app, "druga");
            let screen = draw_app(&mut app, width, height, false);
            // Five short fields (the form's ticket and title among them) do
            // not fit whole in the bar at 60 columns; it shortens them.
            let bar: [&str; 3] = if width >= 100 {
                [
                    "ticket_id:",
                    "dry_run: ‹ false ›",
                    "target_repo: ‹ narsor ›",
                ]
            } else {
                ["ticket_id", "dry_run", "target_re"]
            };
            for expected in bar.into_iter().chain([
                "ship",
                "Wysyła zmianę.",
                "prompt (wymagane)",
                "> pierwsza",
                "  druga",
            ]) {
                assert!(
                    screen.contains(expected),
                    "{width}x{height} {expected}:\n{screen}"
                );
            }
            assert!(
                !screen.contains("Akcje") && !screen.contains("Opis"),
                "{screen}"
            );
        }
    }

    /// The form over one workflow with a long description and a prompt, the
    /// keys on its first field.
    fn open_described_form(app: &mut TaskApp, description: &str) {
        keys(app, "n");
        app.open_new_task(
            catalogue(vec![catalogue_workflow(
                "ship",
                description,
                vec![
                    workflow_input("ticket_id", ParamType::String, false, None),
                    workflow_input("prompt", ParamType::Text, true, None),
                ],
            )]),
            registry(vec![repository("narsor", "git@github.com:acme/narsor.git")]),
        );
        press(app, KeyCode::Enter);
    }

    #[test]
    fn a_long_description_wraps_whole_inside_its_frame_at_any_width() {
        let description = "Pełny cykl wysyłki: plan, implementacja, przegląd, testy, \
                           wydanie i opis zmian w dzienniku, aż do zamknięcia zgłoszenia.";
        for width in [60u16, 64, 100, 130] {
            let mut app = app();
            open_described_form(&mut app, description);
            let screen = draw_app(&mut app, width, 34, false);
            // The text is cut at the frame, not at words, so compare the
            // frame's rows joined, without the spaces the cut may fall on.
            let lines: Vec<Vec<char>> = screen.lines().map(|row| row.chars().collect()).collect();
            let top = lines
                .iter()
                .position(|row| row.iter().collect::<String>().contains("╭ ship"))
                .expect("the strip");
            let left = lines[top].iter().position(|&c| c == '╭').unwrap();
            let left = lines[top]
                .iter()
                .enumerate()
                .skip(left)
                .find(|(i, _)| lines[top][*i..].iter().take(6).collect::<String>() == "╭ ship")
                .map(|(i, _)| i)
                .unwrap();
            let right = lines[top].iter().rposition(|&c| c == '╮').unwrap();
            let inside: String = lines[top + 1..]
                .iter()
                .take_while(|row| row.get(left) != Some(&'╰'))
                .map(|row| row[left + 1..right].iter().collect::<String>())
                .collect();
            let squashed = |text: &str| {
                text.chars()
                    .filter(|c| !c.is_whitespace())
                    .collect::<String>()
            };
            assert_eq!(
                squashed(&inside),
                squashed(description),
                "{width}:\n{screen}"
            );
            for row in screen.lines() {
                assert!(row.chars().count() <= usize::from(width), "{width}: {row}");
            }
        }
    }

    #[test]
    fn the_confirmation_window_is_drawn_even_in_a_terminal_too_small_for_the_form() {
        let mut app = app();
        open_described_form(&mut app, "Wysyła zmianę.");
        keys(&mut app, "NAR-1");
        for _ in 0..4 {
            app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        keys(&mut app, "tekst");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.form_confirming());
        assert_eq!(app.key_context().base, Context::FormConfirm);
        for (width, height) in [(120, 34), (60, 20), (40, 10)] {
            let screen = draw_app(&mut app, width, height, false);
            assert!(
                screen.contains("Zatwierdzić formularz?"),
                "{width}x{height}\n{screen}"
            );
        }
        // The bar offers the window's keys and no others.
        let screen = draw_app(&mut app, 120, 34, false);
        let bar = screen.lines().last().unwrap();
        assert!(
            bar.contains("Enter wyślij") && bar.contains("Esc wróć"),
            "{bar}"
        );
        assert!(!bar.contains("wyjście"), "no global keys in the bar: {bar}");
        // `q` and `?` are not commands while the window is open, and not text.
        app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        assert!(app.form_confirming());
        assert!(!app.is_help_open());
        assert!(!app.should_quit());
        let screen = draw_app(&mut app, 120, 34, false);
        assert!(!screen.contains("Wyjść z TUI?"), "{screen}");
        // A paste does not reach the field behind it.
        app.on_paste("wklejone");
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.form_confirming());
        let screen = draw_app(&mut app, 120, 34, false);
        assert!(
            screen.contains("tekst") && !screen.contains("wklejone"),
            "{screen}"
        );
    }

    #[test]
    fn the_field_in_edit_has_the_navy_fill_and_the_others_do_not() {
        let mut app = app();
        open_described_form(&mut app, "Wysyła zmianę.");
        let buffer = draw_buffer(&mut app, 120, 34, false);
        let fill = Styles::new(false).field_editing().bg;
        let filled: Vec<usize> = rows(&buffer)
            .iter()
            .enumerate()
            .filter(|(row, _)| {
                let width = usize::from(buffer.area.width);
                buffer.content()[row * width..(row + 1) * width]
                    .iter()
                    .any(|cell| Some(cell.bg) == fill)
            })
            .map(|(row, _)| row)
            .collect();
        assert!(!filled.is_empty(), "the field in edit is filled");
        // The bar shows the first field in edit; the prompt below is not.
        let prompt_row = row_of(&buffer, "prompt (wymagane)");
        assert!(!filled.contains(&prompt_row), "{filled:?} vs {prompt_row}");
        // Without colour there is no fill at all.
        let plain = draw_buffer(&mut app, 120, 34, true);
        assert!(plain.content().iter().all(|cell| cell.bg == Color::Reset));
    }

    #[test]
    fn a_long_line_of_the_prompt_wraps_and_the_cursor_stays_in_view() {
        let mut app = app();
        open_prompt_form(&mut app, &["prompt"]);
        press(&mut app, KeyCode::Enter);
        for _ in 0..3 {
            press(&mut app, KeyCode::Down);
        }
        keys(&mut app, &"ab ".repeat(60));
        let screen = draw_app(&mut app, 60, 20, false);
        assert!(!screen.contains('▏'), "{screen}");
        assert!(
            screen.lines().all(|row| row.chars().count() <= 60),
            "{screen}"
        );
    }

    #[test]
    fn with_two_text_fields_the_focused_one_stretches_and_the_other_folds() {
        let mut app = app();
        open_prompt_form(&mut app, &["prompt", "notatki"]);
        press(&mut app, KeyCode::Enter);
        for line in ["1", "2", "3", "4", "5"] {
            keys(&mut app, line);
            app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        }
        press(&mut app, KeyCode::Enter);
        keys(&mut app, "notatki");
        let screen = draw_app(&mut app, 100, 30, false);
        assert!(
            screen.contains("notatki") && screen.contains("prompt (wymagane)"),
            "{screen}"
        );
        assert!(
            !screen.contains("  4"),
            "a folded field shows three lines at most:\n{screen}"
        );
    }

    #[test]
    fn a_form_without_a_text_field_shows_the_bar_and_says_so() {
        let mut app = app();
        open_form(&mut app);
        press(&mut app, KeyCode::Enter);
        let screen = draw_app(&mut app, 100, 24, false);
        assert!(
            screen.contains("brak pola tekstowego w tym workflow"),
            "{screen}"
        );
    }

    #[test]
    fn three_text_fields_leave_the_main_one_at_least_four_rows_at_60x24() {
        let mut app = app();
        open_prompt_form(&mut app, &["prompt", "notatki", "dodatki"]);
        press(&mut app, KeyCode::Enter);
        // The prompt is the main field, so it stands last: after the ticket,
        // the title, the repository, `ticket_id`, `dry_run` and the two others.
        // The arrow leaves a multi-line field only on the second press.
        for _ in 0..9 {
            press(&mut app, KeyCode::Down);
        }
        for _ in 0..3 {
            keys(&mut app, "x");
            app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        }
        let screen = draw_app(&mut app, 60, 24, false);
        let rows: Vec<&str> = screen.lines().collect();
        let header = rows
            .iter()
            .position(|row| row.contains("prompt (wymagane)"))
            .unwrap_or_else(|| panic!("{screen}"));
        let folded = rows.iter().position(|row| row.contains("notatki")).unwrap();
        assert!(folded < header, "{screen}");
        assert!(rows.len() > header + 4, "{screen}");
        for row in &rows[header + 1..header + 4] {
            assert!(row.contains('x'), "{screen}");
        }
    }

    #[test]
    fn the_form_reads_the_same_without_colour() {
        let mut app = app();
        open_form(&mut app);
        let coloured = draw_app(&mut app, 120, 30, false);
        let plain = draw_app(&mut app, 120, 30, true);
        assert_eq!(coloured, plain, "colour never carries information alone");
    }

    #[test]
    fn a_field_error_is_drawn_under_the_field() {
        let mut app = app();
        open_form(&mut app);
        press(&mut app, KeyCode::Enter);
        to_ticket_id(&mut app);
        keys(&mut app, "NAR-18");
        press(&mut app, KeyCode::Down);
        keys(&mut app, "1-2");
        for _ in 0..6 {
            let request = press(&mut app, KeyCode::Enter);
            if request != Request::None {
                assert!(matches!(request, Request::SendTask(_)));
                break;
            }
        }
        let screen = draw_app(&mut app, 130, 34, false);
        assert!(
            screen.contains("✗ Pole 'attempts' wymaga liczby całkowitej."),
            "{screen}"
        );
    }

    /// R12: the workflow list has a bracket in the focus role while the keys
    /// are on it;
    /// in the fields the list folds away and nothing breaks.
    #[test]
    fn tab_switches_to_the_task_list_and_enter_folds_the_workflow_list_into_fields() {
        let mut app = app();
        open_form(&mut app);
        let buffer = draw_buffer(&mut app, 130, 34, false);
        assert_bracketed(&buffer, "1 implement-feature", true, false);
        assert_not_bracketed(&buffer, "2 fix-bug");

        press(&mut app, KeyCode::Tab);
        let buffer = draw_buffer(&mut app, 130, 34, false);
        assert_frame(&buffer, 0, layout::LIST_WIDTH - 1, ROUNDED, Color::Cyan);
        assert!(screen(&buffer).contains("implement-feature"));
        press(&mut app, KeyCode::BackTab);
        press(&mut app, KeyCode::Enter);
        let buffer = draw_buffer(&mut app, 130, 34, false);
        let screen = screen(&buffer);
        assert!(screen.contains("ticket_id"), "{screen}");
        assert!(!screen.contains("│ 1 implement-feature"), "{screen}");
    }

    /// R12: the repositories have a bracket in the focus role on their list;
    /// with the keys in the fields of the form the bracket on its entry is
    /// grey.
    #[test]
    fn the_repositories_are_bracketed_by_the_focus_of_their_chooser() {
        let mut app = app();
        keys(&mut app, "R");
        app.open_repositories(registry(vec![
            repository("docs", "git@github.com:acme/docs.git"),
            repository("narsor", "git@github.com:acme/narsor.git"),
        ]));
        let buffer = draw_buffer(&mut app, 130, 30, false);
        assert_bracketed(&buffer, "1 docs", true, false);
        assert_not_bracketed(&buffer, "2 narsor");

        keys(&mut app, "a");
        let buffer = draw_buffer(&mut app, 130, 30, false);
        assert_bracketed(&buffer, "1 dodaj repozytorium", true, false);
        press(&mut app, KeyCode::Enter);
        let buffer = draw_buffer(&mut app, 130, 30, false);
        assert_bracketed(&buffer, "1 dodaj repozytorium", false, false);
    }

    #[test]
    fn a_path_that_cannot_be_used_is_grey_and_says_why() {
        let mut app = app();
        app.set_capabilities(capabilities(
            false,
            Some("serwer zdalny nie czyta plików z dysku"),
        ));
        open_form(&mut app);
        keys(&mut app, "jj");
        let screen = draw_app(&mut app, 130, 34, false);
        assert!(
            screen.contains("niedostępne: serwer zdalny nie czyta plików z dysku"),
            "{screen}"
        );
    }

    #[test]
    fn a_path_with_violations_lists_them_all_in_the_description() {
        let mut app = app();
        open_form(&mut app);
        keys(&mut app, "jj");
        press(&mut app, KeyCode::Enter);
        keys(&mut app, "/srv/bad.yaml");
        app.on_path_checked(
            "/srv/bad.yaml",
            Ok(
                narsor_engine::controller_protocol::ValidateWorkflowResponse {
                    valid: false,
                    name: None,
                    version: None,
                    state_count: 0,
                    initial_state: None,
                    inputs: vec![],
                    violations: vec![
                        "state 'plan' has no transition".to_string(),
                        "state 'ship' is not reachable".to_string(),
                        "input 'ticket' is declared twice".to_string(),
                    ],
                },
            ),
        );
        let screen = draw_app(&mut app, 130, 40, false);
        for expected in [
            "naruszeń: 3",
            "1. state 'plan' has no transition",
            "2. state 'ship' is not reachable",
            "3. input 'ticket' is declared twice",
        ] {
            assert!(screen.contains(expected), "{expected}:\n{screen}");
        }
    }

    #[test]
    fn a_task_being_sent_and_one_not_sent_are_rows_with_their_own_details() {
        let mut app = app();
        open_form(&mut app);
        press(&mut app, KeyCode::Enter);
        to_ticket_id(&mut app);
        keys(&mut app, "NAR-18");
        let request = (0..7)
            .map(|_| press(&mut app, KeyCode::Enter))
            .find(|request| *request != Request::None)
            .unwrap_or(Request::None);
        let Request::SendTask(id) = request else {
            panic!("a send was expected");
        };
        let sending = draw_app(&mut app, 130, 30, false);
        assert_two_lines(&sending, "↻  NAR-18", "     wysyłanie…");
        for expected in [
            "↻  NAR-18",
            "wysyłanie…",
            "Serwer jeszcze nie potwierdził zadania",
        ] {
            assert!(sending.contains(expected), "{expected}:\n{sending}");
        }
        assert!(
            sending.contains("NAR-14"),
            "the rows above stay:\n{sending}"
        );

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            app.perform(Request::SendTask(id.clone())).await;
            app.retry_outgoing_if_due(Instant::now() + std::time::Duration::from_secs(5))
                .await;
        });
        let unsent = draw_app(&mut app, 130, 30, false);
        assert_two_lines(&unsent, "✗  NAR-18", "     nie wysłano");
        for expected in ["✗  NAR-18", "nie wysłano", "Dane zostały zachowane", &id] {
            assert!(unsent.contains(expected), "{expected}:\n{unsent}");
        }
    }

    #[test]
    fn the_history_is_a_list_beside_read_only_details() {
        let mut app = app();
        keys(&mut app, "H");
        app.open_history(
            listing(vec![archived("old", "NAR-9", 5)], "c"),
            Ok(LegacyExecutionListResponse {
                executions: vec![legacy("exec-1", "fix-bug", "failed", 3, true, None)],
            }),
        );
        let screen = draw_app(&mut app, 130, 30, false);
        for expected in [
            "Historia",
            "NAR-9",
            "archiwum",
            "fix-bug exec-1",
            "failed",
            "Przebieg",
            "storage usunięty przy archiwizacji",
            "wznów",
        ] {
            assert!(screen.contains(expected), "{expected}:\n{screen}");
        }
        assert!(
            !screen.contains("NAR-14"),
            "the task list gives way:\n{screen}"
        );
        assert_two_lines(&screen, "✓  NAR-9", "     archiwum");
        assert_two_lines(&screen, "fix-bug exec-1", "     failed");

        // The bracket covers both lines of the selected entry, and only of
        // it: cyan while the list has the focus, the rules across the whole
        // list from its first column.
        let buffer = draw_buffer(&mut app, 130, 30, false);
        let (selected, other) = history_rows(&buffer);
        let cell = |buffer: &Buffer, x: u16, y: usize| cell_at(buffer, x, y as u16);
        let bar = ("│".to_string(), Color::Cyan);
        assert_eq!(cell(&buffer, 1, selected + 1), bar);
        for x in [1, 3] {
            assert_eq!(
                cell(&buffer, x, selected - 1),
                ("─".to_string(), Color::Cyan)
            );
            assert_eq!(
                cell(&buffer, x, selected + 2),
                ("─".to_string(), Color::Cyan)
            );
        }
        assert_eq!(cell(&buffer, 1, other).0, " ");
        assert_eq!(cell(&buffer, 1, other + 1).0, " ");
        assert!(none_reversed(buffer.content()));

        // With the focus in the details the same bracket is grey,
        // and no entry moves.
        press(&mut app, KeyCode::Enter);
        let details = draw_buffer(&mut app, 130, 30, false);
        assert_eq!(history_rows(&details), (selected, other));
        let grey = ("│".to_string(), Color::DarkGray);
        assert_eq!(cell(&details, 1, selected), grey);
        assert_eq!(cell(&details, 1, selected + 1), grey);
        assert_eq!(
            cell(&details, 3, selected - 1),
            ("─".to_string(), Color::DarkGray)
        );
        assert_eq!(
            cell(&details, 3, selected + 2),
            ("─".to_string(), Color::DarkGray)
        );
    }

    /// The first screen rows of the selected history entry and of the other
    /// one, told apart by the bar beside the selected one.
    fn history_rows(buffer: &Buffer) -> (usize, usize) {
        let all = rows(buffer);
        let at = |needle: &str| {
            all.iter()
                .position(|row| {
                    row.chars()
                        .take(usize::from(layout::LIST_WIDTH))
                        .collect::<String>()
                        .contains(needle)
                })
                .unwrap()
        };
        let barred = |row: usize| buffer[(1, row as u16)].symbol() != " ";
        let (nine, fix) = (at("NAR-9"), at("fix-bug"));
        assert_ne!(barred(nine), barred(fix), "{}", screen(buffer));
        if barred(nine) {
            (nine, fix)
        } else {
            (fix, nine)
        }
    }

    /// The list line holding `first` is followed by one that starts with
    /// `second` (inside the frame of the list column).
    fn assert_two_lines(screen: &str, first: &str, second: &str) {
        let lines: Vec<String> = screen
            .lines()
            .map(|line| {
                line.chars()
                    .skip(3)
                    .take(usize::from(layout::LIST_WIDTH) - 4)
                    .collect()
            })
            .collect();
        let at = lines
            .iter()
            .position(|line| line.contains(first))
            .unwrap_or_else(|| panic!("{first:?} is not in the list:\n{screen}"));
        assert!(
            lines[at + 1].starts_with(second),
            "{second:?} under {first:?}:\n{screen}"
        );
    }

    /// R6: the views of `n`, `H` and `R` have the same frames and the same
    /// rule of the focus as the tasks.
    #[test]
    fn the_new_task_history_and_repositories_frames_follow_the_focus() {
        let fresh = app;
        let list_right = layout::LIST_WIDTH - 1;
        let detail_left = layout::LIST_WIDTH;

        let mut app = fresh();
        open_form(&mut app);
        let buffer = draw_buffer(&mut app, 130, 34, false);
        assert_frame(&buffer, 0, list_right, ROUNDED, Color::DarkGray);
        assert_frame(&buffer, detail_left, 129, ROUNDED, Color::Cyan);
        assert!(rows(&buffer)[1].contains("Nowe zadanie"));

        let mut app = fresh();
        keys(&mut app, "H");
        app.open_history(
            listing(vec![archived("old", "NAR-9", 5)], "c"),
            Ok(LegacyExecutionListResponse { executions: vec![] }),
        );
        let buffer = draw_buffer(&mut app, 130, 30, false);
        assert!(rows(&buffer)[1].contains("Historia"));
        assert_frame(&buffer, 0, list_right, ROUNDED, Color::Cyan);
        assert_frame(&buffer, detail_left, 129, ROUNDED, Color::DarkGray);
        press(&mut app, KeyCode::Enter);
        let buffer = draw_buffer(&mut app, 130, 30, false);
        assert_frame(&buffer, 0, list_right, ROUNDED, Color::DarkGray);
        assert_frame(&buffer, detail_left, 129, ROUNDED, Color::Cyan);
        assert!(rows(&buffer)[1].contains("NAR-9"));

        let mut app = fresh();
        keys(&mut app, "R");
        app.open_repositories(registry(vec![repository(
            "docs",
            "git@github.com:acme/docs.git",
        )]));
        let buffer = draw_buffer(&mut app, 130, 30, false);
        assert!(rows(&buffer)[1].contains("Repozytoria (1)"));
        assert_frame(&buffer, 0, list_right, ROUNDED, Color::DarkGray);
        assert_frame(&buffer, detail_left, 129, ROUNDED, Color::Cyan);
    }

    #[test]
    fn the_history_in_a_narrow_terminal_switches_between_the_list_and_the_details() {
        let mut app = app();
        keys(&mut app, "H");
        app.open_history(
            listing(vec![archived("old", "NAR-9", 5)], "c"),
            Ok(LegacyExecutionListResponse { executions: vec![] }),
        );
        let list = draw_app(&mut app, 80, 24, false);
        assert!(
            list.contains("Historia") && list.contains("NAR-9"),
            "{list}"
        );
        assert!(!list.contains("Przebieg"), "{list}");
        press(&mut app, KeyCode::Enter);
        let details = draw_app(&mut app, 80, 24, false);
        assert!(details.contains("Przebieg"), "{details}");
        assert!(
            details.contains("lista"),
            "the way back is on the bar:\n{details}"
        );
    }

    #[test]
    fn the_repositories_are_a_list_and_a_description_and_the_form_shows_its_refusal() {
        let mut app = app();
        keys(&mut app, "R");
        app.open_repositories(registry(vec![
            repository("docs", "git@github.com:acme/docs.git"),
            repository("narsor", "git@github.com:acme/narsor.git"),
        ]));
        let list = draw_app(&mut app, 130, 30, false);
        for expected in [
            "Repozytoria (2)",
            "docs",
            "narsor",
            "git@github.com:acme/docs.git",
            "github",
            "gałąź main",
            "Zadania",
            "dodaj",
            "usuń",
        ] {
            assert!(list.contains(expected), "{expected}:\n{list}");
        }

        keys(&mut app, "a");
        press(&mut app, KeyCode::Enter);
        keys(&mut app, "evil");
        let form = draw_app(&mut app, 130, 30, false);
        for expected in [
            "nowe repozytorium",
            "dodaj repozytorium",
            "id",
            "remote",
            "(wymagane)",
        ] {
            assert!(form.contains(expected), "{expected}:\n{form}");
        }
    }

    #[test]
    fn removing_a_repository_asks_in_the_window_of_every_action_with_a_bang() {
        let mut app = app();
        keys(&mut app, "R");
        app.open_repositories(registry(vec![repository(
            "docs",
            "git@github.com:acme/docs.git",
        )]));
        keys(&mut app, "d");
        let screen = draw_app(&mut app, 130, 30, false);
        for expected in [
            "Usuń repozytorium !",
            "Usuwa repozytorium docs",
            "potwierdź",
            "anuluj",
        ] {
            assert!(screen.contains(expected), "{expected}:\n{screen}");
        }
    }
}

/// The main screen of the origin (§1), end to end: tasks of a real control
/// plane in the list, the header of the connection, the details of the
/// selected one and the bar.
mod task_app_main_screen {
    use super::*;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use narsor::tui::task_app::{layout, TaskApp};
    use narsor_engine::orchestration::{AttentionItem, AttentionKind};

    use crate::remote_support::{snapshot, RemoteHarness};

    fn render(app: &mut TaskApp, styles: &Styles) -> Buffer {
        draw(130, 30, |frame| {
            layout::render(frame, app, styles, Instant::now())
        })
    }

    #[tokio::test]
    async fn the_main_screen_of_the_origin_is_drawn_from_a_real_control_plane() {
        let harness = RemoteHarness::start().await;
        harness
            .running("NAR-14", "backend-b", snapshot(1, None))
            .await;
        let mut waiting = snapshot(1, Some("in-1"));
        waiting.attention = vec![AttentionItem {
            kind: AttentionKind::Approval,
            state_id: Some("review".to_string()),
            since: chrono::Utc::now(),
            waiting_id: "in-1".to_string(),
        }];
        harness.running("NAR-12", "k8s", waiting).await;
        harness.create("NAR-16", Some("k8s")).await;
        harness.create("NAR-17", Some("k8s")).await;
        let mut app = TaskApp::new(harness.owner.clone(), "local", None);
        app.refresh().await;
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.sync_detail().await;
        let buffer = render(&mut app, &Styles::new(false));
        let rows = rows(&buffer);
        let screen = screen(&buffer);

        // The header: the connection by name and state, the counters.
        assert!(rows[0].contains("narsor · local ● live"), "{}", rows[0]);
        assert!(
            rows[0].contains("? 1") && rows[0].contains("! 0"),
            "{}",
            rows[0]
        );
        assert!(rows[0].contains("sloty"), "{}", rows[0]);

        // The list, in the order of the queue, one row per task: attention,
        // state, title, label.
        let list_part = |row: &String| -> String {
            row.chars().take(usize::from(layout::LIST_WIDTH)).collect()
        };
        let row_of = |title: &str| {
            rows.iter()
                .map(list_part)
                .find(|row| row.contains(title))
                .unwrap_or_else(|| panic!("no row for {title}:\n{screen}"))
        };
        let under = |title: &str| {
            let at = rows
                .iter()
                .position(|row| list_part(row).contains(title))
                .unwrap_or_else(|| panic!("no row for {title}:\n{screen}"));
            list_part(&rows[at + 1])
        };
        // The frame, the bracket's column, then the row; the selected task
        // has the grey bar, the focus being in the details.
        assert!(
            row_of("NAR-14").starts_with("│    ●  NAR-14"),
            "{}",
            row_of("NAR-14")
        );
        assert!(
            row_of("NAR-12").starts_with("││ ? ●  NAR-12"),
            "{}",
            row_of("NAR-12")
        );
        assert!(
            row_of("NAR-16").starts_with("│    ◷  NAR-16")
                && under("NAR-16").starts_with("│       #1")
        );
        assert!(
            row_of("NAR-17").starts_with("│    ◷  NAR-17")
                && under("NAR-17").starts_with("│       #2")
        );
        let order: Vec<usize> = ["NAR-14", "NAR-12", "NAR-16", "NAR-17"]
            .iter()
            .map(|title| {
                rows.iter()
                    .position(|row| list_part(row).contains(title))
                    .unwrap()
            })
            .collect();
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{order:?}");

        // The details of the selected task: header (under a padded blank
        // row: identity, state, metadata, a rule), tabs, and the panel of
        // what waits for the operator, with its own keys on the bar.
        assert!(rows[3].contains("NAR-12"), "{}", rows[3]);
        assert!(rows[4].contains("● running"), "{}", rows[4]);
        assert!(rows[5].contains("implement-feature v1"), "{}", rows[5]);
        assert!(
            rows[7].contains("Przebieg")
                && rows[7].contains("Dyrygent")
                && rows[7].contains("Info"),
            "{}",
            rows[7]
        );
        assert!(screen.contains("? Approval · review"), "{screen}");
        assert!(screen.contains("Approve the plan?"), "{screen}");
        let bar = &rows[rows.len() - 1];
        assert!(
            bar.contains("Enter wyślij") && bar.contains("Esc wstecz"),
            "{bar}"
        );
    }
}

/// U5: the default harness in the header, shortened in its order, and the
/// list `h` opens.
mod task_app_harness_render {
    use super::*;

    use narsor::api::ApiClient;
    use narsor::tui::task_app::{layout, TaskApp};

    use crate::task_app_support::{
        harness_selection, listing, queued, titled, waiting_for_capacity, HARNESSES,
    };

    /// A live application whose queue reports 2 of 3 slots taken, and whose
    /// daemon runs on `claude-code` from the configuration, or on
    /// `override_harness`.
    fn app(override_harness: Option<&str>) -> TaskApp {
        let mut app = TaskApp::new(
            ApiClient::new("127.0.0.1:9".parse().unwrap(), "token"),
            "local",
            None,
        );
        app.load(listing(
            vec![titled(
                waiting_for_capacity(queued("a", 1), 1, 2, 3),
                "NAR-12",
            )],
            "c",
        ));
        app.set_harness_selection(harness_selection(
            "claude-code",
            override_harness,
            &HARNESSES,
        ));
        app
    }

    fn header(app: &mut TaskApp, width: u16) -> String {
        let buffer = draw(width, 24, |frame| {
            layout::render(frame, app, &Styles::new(true), Instant::now())
        });
        rows(&buffer).remove(0)
    }

    #[test]
    fn wide_the_header_has_everything() {
        let top = header(&mut app(None), 120);
        for expected in [
            "narsor · local ● live",
            "harness claude-code (konfiguracja)",
            "sloty 2/3",
            "? 0 · ! 0",
        ] {
            assert!(top.contains(expected), "{expected:?} is missing: {top:?}");
        }
    }

    #[test]
    fn at_80_columns_the_slots_go_first() {
        let top = header(&mut app(None), 80);
        assert!(!top.contains("sloty"), "{top:?}");
        for expected in [
            "narsor · local",
            "harness claude-code (konfiguracja)",
            "? 0 · ! 0",
        ] {
            assert!(top.contains(expected), "{expected:?} is missing: {top:?}");
        }
    }

    #[test]
    fn at_60_columns_the_note_shortens_and_the_name_goes_but_harness_and_counters_stay() {
        let top = header(&mut app(None), 60);
        assert!(!top.contains("sloty"), "{top:?}");
        assert!(!top.contains("local"), "{top:?}");
        for expected in ["narsor", "harness claude-code (konf.)", "? 0 · ! 0"] {
            assert!(top.contains(expected), "{expected:?} is missing: {top:?}");
        }
    }

    #[test]
    fn a_name_too_long_for_the_row_is_cut_and_the_counters_stay() {
        let long = "a-very-long-harness-name-from-a-plugin";
        let mut app = app(Some(long));
        let mut snapshot = app.harness_selection().unwrap().clone();
        snapshot.supported.push(long.to_string());
        app.set_harness_selection(snapshot);
        let top = header(&mut app, 60);
        assert_eq!(top.chars().count(), 60);
        assert!(top.contains("harness a-very-long"), "{top:?}");
        assert!(top.contains('…'), "{top:?}");
        assert!(top.contains("? 0 · ! 0"), "{top:?}");
        assert!(!top.contains(long), "{top:?}");
    }

    #[test]
    fn before_the_first_read_the_header_says_it_does_not_know() {
        let mut app = TaskApp::new(
            ApiClient::new("127.0.0.1:9".parse().unwrap(), "token"),
            "local",
            None,
        );
        app.load(listing(vec![], "c"));
        assert!(header(&mut app, 120).contains("harness ? "));
    }

    #[test]
    fn the_harness_list_fits_a_narrow_terminal() {
        let mut app = app(None);
        assert!(app.open_harness());
        let buffer = draw(64, 24, |frame| {
            layout::render(frame, &mut app, &Styles::new(false), Instant::now())
        });
        let text = screen(&buffer);
        for expected in [
            " Domyślny harness ",
            "● domyślny z konfiguracji (claude-code)",
            "claude-code",
            "codex",
            "opencode",
        ] {
            assert!(text.contains(expected), "{expected:?} is missing:\n{text}");
        }
        let y = u16::try_from(row_of(&buffer, " Domyślny harness ")).unwrap();
        let x = column_of(&buffer, y, 0, "╭").expect("the list's frame");
        assert_eq!(cell_at(&buffer, x, y).1, Color::Cyan, "the focus frame");
        assert_bracketed(&buffer, "1 ● domyślny z konfiguracji", true, false);
    }

    #[test]
    fn the_list_bar_offers_h() {
        let mut app = app(None);
        let buffer = draw(140, 24, |frame| {
            layout::render(frame, &mut app, &Styles::new(true), Instant::now())
        });
        assert!(rows(&buffer).last().unwrap().contains("h harness"));
    }
}

/// U4: the bracketed list, drawn on its own.
mod bracketed_list_render {
    use super::*;

    use narsor::tui::selection::{bar_prefix, render_bracketed_list};
    use narsor::tui::style::Role;
    use ratatui::text::{Line, Span};

    /// `count` items of two lines each: a red `!` and a name, then a detail.
    fn items(styles: &Styles, count: usize) -> Vec<Vec<Line<'static>>> {
        (0..count)
            .map(|index| {
                vec![
                    Line::from(vec![
                        Span::styled("!", styles.of(Role::Problem)),
                        Span::raw(format!(" item {index}")),
                    ]),
                    Line::raw(format!("detail {index}")),
                ]
            })
            .collect()
    }

    fn draw_list(
        width: u16,
        height: u16,
        styles: &Styles,
        items: &[Vec<Line<'static>>],
        selected: Option<usize>,
        focused: bool,
        top: &mut usize,
    ) -> Buffer {
        draw(width, height, |frame| {
            render_bracketed_list(
                frame,
                Rect::new(0, 0, width, height),
                styles,
                items,
                selected,
                focused,
                top,
            )
        })
    }

    /// Whether row `y` is a rule of `marks` across the whole width, from
    /// column 0 to the right edge.
    fn is_full_rule(buffer: &Buffer, y: u16, marks: SelectionMarks) -> bool {
        is_rule(buffer, y, 0..buffer.area.width, marks)
    }

    /// The rows of `buffer` that are rules.
    fn rule_rows(buffer: &Buffer) -> Vec<u16> {
        (0..buffer.area.height)
            .filter(|&y| buffer[(0, y)].symbol() == "─")
            .collect()
    }

    fn no_marks(buffer: &Buffer) -> bool {
        buffer
            .content()
            .iter()
            .all(|cell| !["━", "─", "┃", "│"].contains(&cell.symbol()))
    }

    #[test]
    fn a_focused_selection_is_a_thin_cyan_bracket_with_full_width_rules() {
        let styles = Styles::new(false);
        let mut top = 0;
        let buffer = draw_list(20, 10, &styles, &items(&styles, 3), Some(1), true, &mut top);
        let screen = screen(&buffer);
        assert_eq!(row_of(&buffer, "item 0"), 0, "{screen}");
        assert_eq!(row_of(&buffer, "item 1"), 3, "{screen}");
        assert_eq!(row_of(&buffer, "item 2"), 6, "{screen}");
        assert!(
            is_full_rule(&buffer, 2, styles.selection_marks(true)),
            "{screen}"
        );
        assert!(
            is_full_rule(&buffer, 5, styles.selection_marks(true)),
            "{screen}"
        );
        for y in [3, 4] {
            assert_eq!(cell_at(&buffer, 0, y), ("│".to_string(), Color::Cyan));
            assert_eq!(cell_at(&buffer, 1, y).0, " ");
        }
        assert_eq!(cell_at(&buffer, 0, 2), ("─".to_string(), Color::Cyan));
        for y in [8usize, 9] {
            assert!(rows(&buffer)[y].trim().is_empty(), "{screen}");
        }
        assert_eq!(cell_at(&buffer, 0, 0).0, " ", "an item outside the bracket");
        assert_eq!(cell_at(&buffer, 2, 0).0, "!", "content keeps its column");
        assert_eq!(cell_at(&buffer, 2, 3).0, "!", "content keeps its column");
    }

    #[test]
    fn without_focus_the_same_bracket_is_thin_and_grey() {
        let styles = Styles::new(false);
        let mut top = 0;
        let buffer = draw_list(
            20,
            10,
            &styles,
            &items(&styles, 3),
            Some(1),
            false,
            &mut top,
        );
        assert!(
            is_full_rule(&buffer, 2, styles.selection_marks(false)),
            "{}",
            screen(&buffer)
        );
        assert!(is_full_rule(&buffer, 5, styles.selection_marks(false)));
        assert_eq!(cell_at(&buffer, 0, 5), ("─".to_string(), Color::DarkGray));
        for y in [3, 4] {
            assert_eq!(cell_at(&buffer, 0, y), ("│".to_string(), Color::DarkGray));
        }
    }

    #[test]
    fn without_colour_the_bracket_speaks_through_bold_and_dim() {
        let styles = Styles::new(true);
        let list = items(&styles, 3);
        let mut top = 0;
        let focused = draw_list(20, 10, &styles, &list, Some(1), true, &mut top);
        assert!(
            is_full_rule(&focused, 2, styles.selection_marks(true))
                && is_full_rule(&focused, 5, styles.selection_marks(true))
        );
        assert_eq!(focused[(0, 3)].symbol(), "│");
        assert!(bold_at(&focused, 0, 3) && bold_at(&focused, 5, 2));
        let unfocused = draw_list(20, 10, &styles, &list, Some(1), false, &mut top);
        assert!(
            is_full_rule(&unfocused, 2, styles.selection_marks(false))
                && is_full_rule(&unfocused, 5, styles.selection_marks(false))
        );
        assert_eq!(unfocused[(0, 3)].symbol(), "│");
        for (x, y) in [(0, 3), (5, 2)] {
            assert!(unfocused[(x, y)].modifier.contains(Modifier::DIM));
            assert!(!bold_at(&unfocused, x, y));
        }
        for buffer in [&focused, &unfocused] {
            assert!(none_reversed(buffer.content()), "{}", screen(buffer));
            assert!(
                no_colour_but_the_selection_fill(buffer.content(), &styles),
                "{}",
                screen(buffer)
            );
        }
    }

    #[test]
    fn the_selected_content_keeps_its_own_colours() {
        let styles = Styles::new(false);
        let mut top = 0;
        let buffer = draw_list(20, 10, &styles, &items(&styles, 3), Some(1), true, &mut top);
        assert_eq!(cell_at(&buffer, 2, 3), ("!".to_string(), Color::Red));
        assert_eq!(cell_at(&buffer, 4, 3), ("i".to_string(), Color::Reset));
        assert!(none_reversed(buffer.content()), "{}", screen(&buffer));
    }

    #[test]
    fn rules_are_only_around_the_selected_item_and_the_others_stack() {
        let styles = Styles::new(false);
        let list = items(&styles, 3);
        let mut top = 0;
        let first = draw_list(20, 10, &styles, &list, Some(0), true, &mut top);
        let screen = super::screen(&first);
        assert_eq!(rule_rows(&first), vec![0, 3], "{screen}");
        assert_eq!(row_of(&first, "item 0"), 1, "{screen}");
        assert_eq!(row_of(&first, "item 1"), 4, "{screen}");
        assert_eq!(row_of(&first, "item 2"), 6, "unselected items are adjacent");
        assert_eq!(row_of(&first, "detail 1"), 5);
    }

    #[test]
    fn moving_the_selection_moves_the_rules_and_shifts_the_neighbours() {
        let styles = Styles::new(false);
        let list = items(&styles, 3);
        let mut top = 0;
        let first = draw_list(20, 10, &styles, &list, Some(0), true, &mut top);
        let second = draw_list(20, 10, &styles, &list, Some(1), true, &mut top);
        assert_eq!(row_of(&first, "item 1"), 4);
        assert_eq!(row_of(&second, "item 1"), 3, "the rule moved above it");
        assert_eq!(row_of(&first, "item 0"), 1);
        assert_eq!(row_of(&second, "item 0"), 0, "no rule above it any more");
        assert_eq!(rule_rows(&second), vec![2, 5]);
        assert_eq!(row_of(&second, "item 2"), 6, "below both rules, as before");
    }

    #[test]
    fn the_first_item_has_its_rule_in_the_first_row() {
        let styles = Styles::new(false);
        let list = items(&styles, 3);
        let mut top = 0;
        let first = draw_list(20, 10, &styles, &list, Some(0), true, &mut top);
        assert!(
            is_full_rule(&first, 0, styles.selection_marks(true))
                && is_full_rule(&first, 3, styles.selection_marks(true))
        );
        let last = draw_list(20, 10, &styles, &list, Some(2), true, &mut top);
        assert!(
            is_full_rule(&last, 4, styles.selection_marks(true))
                && is_full_rule(&last, 7, styles.selection_marks(true)),
            "{}",
            screen(&last)
        );
        assert_eq!(top, 0, "everything fits, nothing scrolls");
    }

    #[test]
    fn a_long_list_scrolls_just_enough_for_the_selected_item_and_its_rules() {
        let styles = Styles::new(false);
        let list = items(&styles, 10);
        let mut top = 0;
        let buffer = draw_list(20, 10, &styles, &list, Some(9), true, &mut top);
        let screen = screen(&buffer);
        assert_eq!(top, 6, "{screen}");
        assert_eq!(row_of(&buffer, "item 6"), 0, "{screen}");
        assert_eq!(row_of(&buffer, "item 9"), 7, "{screen}");
        assert!(
            is_full_rule(&buffer, 6, styles.selection_marks(true))
                && is_full_rule(&buffer, 9, styles.selection_marks(true)),
            "{screen}"
        );

        for (selected, expected_row) in [(8, 5), (7, 3), (6, 1)] {
            let buffer = draw_list(20, 10, &styles, &list, Some(selected), true, &mut top);
            assert_eq!(top, 6, "the bracket still fits: the view stays");
            assert_eq!(row_of(&buffer, &format!("item {selected}")), expected_row);
        }
        let buffer = draw_list(20, 10, &styles, &list, Some(5), true, &mut top);
        assert_eq!(top, 5, "one item up, one step of the view");
        assert!(
            is_full_rule(&buffer, 0, styles.selection_marks(true))
                && is_full_rule(&buffer, 3, styles.selection_marks(true)),
            "{}",
            super::screen(&buffer)
        );
    }

    #[test]
    fn a_refresh_keeps_the_first_item_in_view() {
        let styles = Styles::new(false);
        let list = items(&styles, 10);
        let mut top = 3;
        let before = draw_list(20, 10, &styles, &list, Some(4), true, &mut top);
        assert_eq!(top, 3);
        let after = draw_list(
            20,
            10,
            &styles,
            &items(&styles, 10),
            Some(4),
            true,
            &mut top,
        );
        assert_eq!(top, 3, "a refresh does not move the view");
        assert_eq!(screen(&before), screen(&after));
        assert_eq!(row_of(&after, "item 3"), 0);
    }

    #[test]
    fn no_selection_and_no_items_draw_no_marks() {
        let styles = Styles::new(false);
        let mut top = 0;
        let unselected = draw_list(20, 10, &styles, &items(&styles, 3), None, true, &mut top);
        assert!(no_marks(&unselected), "{}", screen(&unselected));
        assert_eq!(row_of(&unselected, "item 0"), 0);
        assert_eq!(row_of(&unselected, "item 2"), 4, "items stack directly");
        let past_the_end = draw_list(20, 10, &styles, &items(&styles, 3), Some(5), true, &mut top);
        assert!(no_marks(&past_the_end));
        let empty = draw_list(20, 10, &styles, &[], Some(0), true, &mut top);
        assert!(screen(&empty).trim().is_empty());
        assert_eq!(top, 0);
    }

    #[test]
    fn an_area_lower_than_the_bracket_keeps_the_bar_alone() {
        let styles = Styles::new(false);
        let list = items(&styles, 3);
        let mut top = 0;
        let buffer = draw_list(20, 3, &styles, &list, Some(2), true, &mut top);
        let screen = screen(&buffer);
        assert_eq!(row_of(&buffer, "item 2"), 0, "{screen}");
        assert_eq!(cell_at(&buffer, 0, 0), ("│".to_string(), Color::Cyan));
        assert_eq!(cell_at(&buffer, 0, 1).0, "│");
        assert!(!screen.contains('─'), "{screen}");
        for (width, height) in [(1, 1), (2, 2), (0, 0), (3, 1)] {
            let mut top = 0;
            draw_list(width, height, &styles, &list, Some(1), true, &mut top);
        }
    }

    #[test]
    fn the_bar_prefix_follows_the_focus_rule() {
        let styles = Styles::new(false);
        let focused = bar_prefix(&styles, true, true);
        assert_eq!(focused.content, "│ ");
        assert_eq!(focused.style.fg, Some(Color::Cyan));
        let unfocused = bar_prefix(&styles, true, false);
        assert_eq!(unfocused.content, "│ ");
        assert_eq!(unfocused.style.fg, Some(Color::DarkGray));
        assert_eq!(bar_prefix(&styles, false, true).content, "  ");
    }
}

mod active_field_fill {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    const WIDTH: u16 = 40;

    fn key(chooser: &mut Chooser, code: KeyCode, modifiers: KeyModifiers) {
        chooser.on_key(KeyEvent::new(code, modifiers));
    }

    fn type_text(chooser: &mut Chooser, text: &str) {
        for character in text.chars() {
            key(chooser, KeyCode::Char(character), KeyModifiers::NONE);
        }
    }

    fn form(field: ChooserField) -> Chooser {
        let mut chooser = Chooser::new(vec![ChooserItem::new("go", "Start.").with_field(field)]);
        key(&mut chooser, KeyCode::Tab, KeyModifiers::NONE);
        chooser
    }

    /// The rows of the buffer that start with the prompt or its blank twin
    /// and carry the editing fill somewhere: the rows of the active field.
    fn field_rows(buffer: &Buffer, styles: &Styles) -> Vec<Vec<Cell>> {
        let fill = styles.field_editing().bg;
        let width = usize::from(buffer.area.width);
        buffer
            .content()
            .chunks(width)
            .filter(|row| row.iter().any(|cell| Some(cell.bg) == fill))
            .map(<[Cell]>::to_vec)
            .collect()
    }

    /// Cells 2..=38 filled; the prompt (0, 1) and the margin (39) are not.
    fn assert_filled(row: &[Cell], styles: &Styles) {
        let fill = styles.field_editing().bg;
        for (column, cell) in row.iter().enumerate() {
            let expected = (2..=38).contains(&column);
            assert_eq!(Some(cell.bg) == fill, expected, "column {column}");
        }
    }

    fn drawn(chooser: &mut Chooser, styles: &Styles) -> Vec<Vec<Cell>> {
        let buffer = draw_chooser(WIDTH, 12, chooser, styles);
        field_rows(&buffer, styles)
    }

    #[test]
    fn an_empty_field_is_filled_after_the_prompt_to_the_last_text_cell() {
        let styles = Styles::new(false);
        let rows = drawn(&mut form(ChooserField::text("a")), &styles);
        assert_eq!(rows.len(), 1);
        assert_filled(&rows[0], &styles);
    }

    #[test]
    fn a_short_text_is_filled_behind_and_after_it() {
        let styles = Styles::new(false);
        let mut chooser = form(ChooserField::text("a"));
        type_text(&mut chooser, "abc");
        let rows = drawn(&mut chooser, &styles);
        assert_eq!(rows.len(), 1);
        assert_filled(&rows[0], &styles);
    }

    #[test]
    fn every_row_of_a_multiline_or_wrapped_field_is_filled() {
        let styles = Styles::new(false);
        let mut chooser = form(ChooserField::multiline("a"));
        type_text(&mut chooser, "a");
        key(&mut chooser, KeyCode::Char('j'), KeyModifiers::CONTROL);
        key(&mut chooser, KeyCode::Char('j'), KeyModifiers::CONTROL);
        type_text(&mut chooser, "b");
        let rows = drawn(&mut chooser, &styles);
        assert_eq!(rows.len(), 3);
        rows.iter().for_each(|row| assert_filled(row, &styles));

        let mut chooser = form(ChooserField::multiline("a"));
        type_text(&mut chooser, &"x".repeat(100));
        let rows = drawn(&mut chooser, &styles);
        assert!(rows.len() >= 3);
        rows.iter().for_each(|row| assert_filled(row, &styles));
    }

    #[test]
    fn a_select_and_a_number_are_filled_the_same_way() {
        let styles = Styles::new(false);
        let select = ChooserField::select("m", vec!["a".into(), "b".into()]);
        for field in [select, ChooserField::integer("n")] {
            let rows = drawn(&mut form(field), &styles);
            assert_eq!(rows.len(), 1);
            assert_filled(&rows[0], &styles);
        }
    }

    #[test]
    fn the_cursor_stays_the_only_reversed_cell() {
        let styles = Styles::new(false);
        let mut chooser = form(ChooserField::text("a"));
        type_text(&mut chooser, "abc");
        let buffer = draw_chooser(WIDTH, 12, &mut chooser, &styles);
        let reversed = buffer
            .content()
            .iter()
            .filter(|cell| cell.modifier.contains(Modifier::REVERSED))
            .count();
        assert_eq!(reversed, 1);
    }

    #[test]
    fn without_colour_no_cell_has_a_fill() {
        let styles = Styles::new(true);
        let mut chooser = form(ChooserField::text("a"));
        let buffer = draw_chooser(WIDTH, 12, &mut chooser, &styles);
        let editing = Styles::new(false).field_editing().bg;
        assert!(buffer.content().iter().all(|cell| Some(cell.bg) != editing));
    }

    #[test]
    fn a_very_narrow_area_does_not_panic() {
        let styles = Styles::new(false);
        let mut chooser = form(ChooserField::text("a"));
        type_text(&mut chooser, "abcdef");
        for width in 1..=4 {
            draw_chooser(width, 8, &mut chooser, &styles);
        }
    }

    #[test]
    fn the_short_field_in_the_bar_is_filled_to_its_whole_place() {
        let styles = Styles::new(false);
        let mut chooser = Chooser::new(vec![ChooserItem::new("go", "Start.")
            .with_field(ChooserField::text("ab"))
            .with_field(ChooserField::text("cd"))
            .with_field(ChooserField::multiline("long"))]);
        key(&mut chooser, KeyCode::Tab, KeyModifiers::NONE);
        let buffer = draw_chooser(80, 12, &mut chooser, &styles);
        let fill = styles.field_editing().bg;
        let filled: Vec<usize> = buffer
            .content()
            .chunks(80)
            .flat_map(|row| {
                row.iter()
                    .enumerate()
                    .filter(|(_, cell)| Some(cell.bg) == fill)
                    .map(|(column, _)| column)
                    .collect::<Vec<_>>()
            })
            .collect();
        assert!(!filled.is_empty());
        // "ab: " is 4 cells plus the cursor space: the place is 5 cells wide.
        assert!(filled.len() >= 5, "filled {filled:?}");
    }
}

/// A form of inputs draws its fields one under the other, and the field being
/// edited has the editing fill over the whole of its rectangle.
mod stacked_form {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    const WIDTH: u16 = 40;

    fn press(chooser: &mut Chooser, code: KeyCode) {
        chooser.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn entered(fields: Vec<ChooserField>) -> Chooser {
        let item = fields
            .into_iter()
            .fold(ChooserItem::new("go", "Start."), ChooserItem::with_field);
        let mut chooser = Chooser::new(vec![item]).with_inputs_layout();
        chooser.enter_fields();
        chooser
    }

    fn standard() -> Vec<ChooserField> {
        vec![
            ChooserField::text("Ticket"),
            ChooserField::text("Tytuł"),
            ChooserField::text("Repo"),
            ChooserField::multiline("Opis").required(),
        ]
    }

    fn filled(buffer: &Buffer, row: usize, styles: &Styles) -> Vec<bool> {
        let width = usize::from(buffer.area.width);
        let fill = styles.field_editing().bg;
        buffer.content()[row * width..(row + 1) * width]
            .iter()
            .map(|cell| Some(cell.bg) == fill)
            .collect()
    }

    #[test]
    fn every_field_is_in_its_own_row_in_the_order_of_declaration() {
        let mut chooser = entered(standard());
        let buffer = draw_chooser(WIDTH, 20, &mut chooser, &Styles::new(false));
        let rows = rows(&buffer);
        let at = |needle: &str| {
            let found: Vec<usize> = (0..rows.len())
                .filter(|&row| rows[row].contains(needle))
                .collect();
            assert_eq!(found.len(), 1, "{needle}:\n{}", screen(&buffer));
            found[0]
        };
        let (ticket, title, repo, text) = (at("Ticket:"), at("Tytuł:"), at("Repo:"), at("Opis"));
        assert!(ticket < title && title < repo && repo < text);
    }

    #[test]
    fn the_empty_main_field_is_filled_over_its_whole_rectangle() {
        let styles = Styles::new(false);
        let mut chooser = entered(standard());
        for _ in 0..3 {
            press(&mut chooser, KeyCode::Down);
        }
        let buffer = draw_chooser(WIDTH, 30, &mut chooser, &styles);
        let header = rows(&buffer)
            .iter()
            .position(|row| row.contains("Opis"))
            .unwrap();
        // From the row under the header to the last row of the area.
        let area_end = rows(&buffer).len() - 1;
        let filled_rows: Vec<usize> = (header + 1..area_end)
            .filter(|&row| filled(&buffer, row, &styles).iter().any(|&on| on))
            .collect();
        assert!(filled_rows.len() >= 10, "{}", screen(&buffer));
        let first = filled_rows[0];
        assert_eq!(first, header + 1);
        assert!(filled_rows.windows(2).all(|pair| pair[1] == pair[0] + 1));
        for row in filled_rows {
            let cells = filled(&buffer, row, &styles);
            assert!(
                cells[2..usize::from(WIDTH) - 2].iter().all(|&on| on),
                "row {row}"
            );
            assert!(!cells[0] && !cells[1], "the prompt has no fill");
        }
    }

    #[test]
    fn a_field_without_the_focus_has_no_fill_and_no_colour_has_none_at_all() {
        let mut chooser = entered(standard());
        let fill = Styles::new(false).field_editing().bg;
        let buffer = draw_chooser_focused(WIDTH, 20, &mut chooser, &Styles::new(false), false);
        assert!(buffer.content().iter().all(|cell| Some(cell.bg) != fill));
        let buffer = draw_chooser(WIDTH, 20, &mut chooser, &Styles::new(true));
        assert!(buffer.content().iter().all(|cell| cell.bg == Color::Reset));
    }

    #[test]
    fn a_short_field_in_edit_is_filled_to_the_last_text_cell_of_its_row() {
        let styles = Styles::new(false);
        let mut chooser = entered(standard());
        press(&mut chooser, KeyCode::Down);
        let buffer = draw_chooser(WIDTH, 20, &mut chooser, &styles);
        let row = rows(&buffer)
            .iter()
            .position(|row| row.contains("Tytuł:"))
            .unwrap();
        let cells = filled(&buffer, row, &styles);
        assert!(cells[0..usize::from(WIDTH) - 2].iter().all(|&on| on));
        assert!(!cells[usize::from(WIDTH) - 1]);
    }

    #[test]
    fn a_form_without_a_text_field_says_so_under_the_short_ones() {
        let mut chooser = entered(vec![
            ChooserField::text("Ticket"),
            ChooserField::text("Repo"),
        ]);
        let buffer = draw_chooser(WIDTH, 12, &mut chooser, &Styles::new(false));
        let rows = rows(&buffer);
        let last = rows.iter().position(|row| row.contains("Repo:")).unwrap();
        assert!(
            rows[last + 1].contains("brak pola tekstowego"),
            "{}",
            screen(&buffer)
        );
    }

    #[test]
    fn a_low_area_scrolls_to_keep_the_focused_short_field_in_view() {
        let mut chooser = entered(vec![
            ChooserField::text("pierwsze"),
            ChooserField::text("drugie"),
            ChooserField::text("trzecie"),
            ChooserField::text("czwarte"),
            ChooserField::multiline("Opis"),
        ]);
        for _ in 0..3 {
            press(&mut chooser, KeyCode::Down);
        }
        let buffer = draw_chooser(WIDTH, 8, &mut chooser, &Styles::new(false));
        assert!(screen(&buffer).contains("czwarte:"), "{}", screen(&buffer));
    }

    #[test]
    fn a_very_narrow_area_does_not_panic() {
        for width in 1..=4 {
            let mut chooser = entered(standard());
            draw_chooser(width, 12, &mut chooser, &Styles::new(false));
            press(&mut chooser, KeyCode::Down);
            draw_chooser(width, 12, &mut chooser, &Styles::new(false));
        }
    }
}
