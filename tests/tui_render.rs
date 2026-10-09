//! Render tests for the TUI building blocks (U7): the chooser, the shortcut
//! bar and help, the connection indicator and the colour rules.
//!
//! Frames are drawn on a `TestBackend` and read back flattened, row by row,
//! the way `tests/tui_state.rs` reads the older views. Nothing here starts a
//! daemon.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

use narsor::tui::chooser::{self, Chooser, ChooserField, ChooserItem};
use narsor::tui::connection::Connection;
use narsor::tui::keymap::{self, Context};
use narsor::tui::style::Styles;

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

fn row_of(buffer: &Buffer, needle: &str) -> usize {
    rows(buffer)
        .iter()
        .position(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} is not on the screen:\n{}", screen(buffer)))
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
    draw(width, height, |frame| {
        chooser::render(frame, Rect::new(0, 0, width, height), chooser, styles)
    })
}

mod chooser_render {
    use super::*;

    #[test]
    fn wide_area_puts_the_description_beside_the_list() {
        let mut chooser = review_chooser();
        let buffer = draw_chooser(100, 16, &mut chooser, &Styles::new(false));
        let screen = screen(&buffer);
        assert!(screen.contains("Akcje") && screen.contains("Opis"));
        assert!(screen.contains("▸ 2 revise"), "{screen}");
        for marker in ["↩", "◆", "!"] {
            assert!(screen.contains(marker), "{marker} is missing:\n{screen}");
        }
        assert!(screen.contains("↩ wraca do agenta: plan"));
        assert!(screen.contains("◆ rekomendacja dyrygenta"));

        let selected_row = &rows(&buffer)[row_of(&buffer, "approved")];
        assert!(
            selected_row.contains("↩ wraca do agenta"),
            "list and description share rows when wide: {selected_row}"
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
        assert!(screen(&buffer).contains("> Uwzględnij migrację▏"));
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
    fn selection_is_reversed_in_colour_and_in_no_color_and_markers_survive() {
        for no_color in [false, true] {
            let mut chooser = review_chooser();
            let buffer = draw_chooser(100, 16, &mut chooser, &Styles::new(no_color));
            let screen = screen(&buffer);
            for marker in ["↩", "◆", "!", "▸"] {
                assert!(
                    screen.contains(marker),
                    "NO_COLOR={no_color}: {marker} lost"
                );
            }

            let selected = row_of(&buffer, "revise");
            let other = row_of(&buffer, "approved");
            let width = usize::from(buffer.area.width);
            let reversed = |row: usize, column: usize| {
                buffer.content()[row * width + column]
                    .modifier
                    .contains(Modifier::REVERSED)
            };
            let label_column = rows(&buffer)[selected]
                .find("revise")
                .map(|byte| rows(&buffer)[selected][..byte].chars().count());
            let label_column = label_column.expect("the label is on its row");
            assert!(
                reversed(selected, label_column),
                "NO_COLOR={no_color}: selected row"
            );
            assert!(
                !reversed(other, label_column),
                "NO_COLOR={no_color}: other row"
            );

            if no_color {
                assert!(
                    buffer
                        .content()
                        .iter()
                        .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset),
                    "NO_COLOR draws no colour at all"
                );
            }
        }
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
        for expected in ["Tab", "Enter", "Alt+Enter", "Esc"] {
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
        for description in keymap::help_entries(Context::List)
            .iter()
            .map(|entry| entry.description)
        {
            assert!(
                screen.contains(description),
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

/// R17: views name roles, not colours. The list holds the sources of the new
/// views; U14 extends the check to the whole of `src/tui` by listing the
/// directory instead.
mod colour_discipline {
    use super::*;

    /// The new views' sources, relative to `src/tui`. Everything that draws
    /// belongs here; `style.rs` is the one file allowed to name a colour.
    const NEW_VIEW_SOURCES: &[&str] = &["chooser.rs", "connection.rs", "keymap.rs"];

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
                if line.contains("Color::") || line.contains("Color::Rgb") {
                    found.push(format!("{source}:{}: {}", number + 1, line.trim()));
                }
            }
        }
        found
    }

    #[test]
    fn new_views_do_not_name_colours_outside_the_role_module() {
        let sources: Vec<String> = NEW_VIEW_SOURCES
            .iter()
            .map(|name| name.to_string())
            .collect();
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
        fs::write(directory.join("view.rs"), "let x = Color::Red;\n").expect("write");
        fs::write(directory.join("style.rs"), "let y = Color::Green;\n").expect("write");
        let sources = vec!["view.rs".to_string(), "style.rs".to_string()];
        let found = colour_references(&directory, &sources);
        let _ = fs::remove_dir_all(&directory);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].starts_with("view.rs:1:"));
    }

    #[test]
    fn the_role_module_is_where_the_colours_are() {
        let style = fs::read_to_string(tui_dir().join(ROLE_MODULE)).expect("style.rs");
        assert!(style.contains("Color::Cyan"));
        assert!(!style.contains("Color::Rgb"), "only the 16 ANSI colours");
        assert!(!style.contains("Color::Indexed"));
    }
}
