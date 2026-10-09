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
                if line.contains("Color::") || line.contains("Color::Rgb") {
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

/// U8: the task application's screens — the wide and the narrow layout, the
/// request for room, the connection states and the move preview.
mod task_app_render {
    use super::*;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use narsor::api::{ApiClient, ClientError};
    use narsor::domain::ExecutionStatus;
    use narsor::orchestration::AttentionKind;
    use narsor::tui::task_app::{layout, TaskApp};

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
        let controlled = |id: &str, lifecycle: &str, rank| {
            with_controller(crate::task_app_support::task(id, "local", lifecycle, rank))
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
                    waiting(controlled("c", "interrupted", 3), AttentionKind::Problem),
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
            .map(|row| row.chars().skip(1).take(27).collect::<String>())
            .collect();
        let find = |needle: &str| {
            list_rows
                .iter()
                .position(|row| row.contains(needle))
                .unwrap()
        };
        assert!(
            list_rows[find("NAR-14")].starts_with(" ●"),
            "{:?}",
            list_rows[find("NAR-14")]
        );
        assert!(list_rows[find("NAR-12")].starts_with("?●"));
        assert!(list_rows[find("NAR-9 ")].starts_with("!✗"));
        assert!(list_rows[find("NAR-16")].starts_with(" ◷"));
        assert!(list_rows[find("NAR-16")].contains("#1"));
        assert!(list_rows[find("NAR-17")].contains("#2"));
        assert!(list_rows[find("NAR-8 ")].starts_with(" ✓"));
        assert!(list_rows[find("NAR-12")].contains("approval"));
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
        let header_row = &rows(&buffer)[1];
        let divider: Vec<usize> = header_row
            .chars()
            .enumerate()
            .filter(|(_, c)| *c == '│')
            .map(|(index, _)| index)
            .collect();
        assert_eq!(divider.first(), Some(&0));
        assert_eq!(
            divider.get(1),
            Some(&usize::from(layout::LIST_WIDTH)),
            "{header_row:?}"
        );
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

    #[test]
    fn live_markers_keep_their_colours_and_the_selection_is_reversed() {
        let mut app = busy_app();
        press(&mut app, KeyCode::Char('j'));
        let buffer = render(120, 30, &mut app, &Styles::new(false));
        let width = usize::from(buffer.area.width);
        let row = list_row(&buffer, "NAR-12");
        let cells = &buffer.content()[row * width..(row + 1) * width];
        assert_eq!(cells[1].symbol(), "?");
        assert_eq!(
            cells[1].fg,
            Color::Yellow,
            "the marker keeps its role colour"
        );
        assert_eq!(cells[2].symbol(), "●");
        assert_eq!(cells[2].fg, Color::Green);
        assert!(cells[1].modifier.contains(Modifier::REVERSED));
        let other = list_row(&buffer, "NAR-14");
        assert!(!buffer.content()[other * width + 4]
            .modifier
            .contains(Modifier::REVERSED));
    }

    #[test]
    fn without_colour_every_marker_and_the_selection_survive() {
        let mut app = busy_app();
        press(&mut app, KeyCode::Char('j'));
        let buffer = render(120, 30, &mut app, &Styles::new(true));
        let screen = screen(&buffer);
        for marker in ["?●", "!✗", "◷", "✓", "● live", "? 1", "! 1"] {
            assert!(screen.contains(marker), "{marker:?} lost:\n{screen}");
        }
        assert!(
            buffer
                .content()
                .iter()
                .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset),
            "NO_COLOR draws no colour at all"
        );
        let width = usize::from(buffer.area.width);
        let row = list_row(&buffer, "NAR-12");
        assert!(buffer.content()[row * width + 4]
            .modifier
            .contains(Modifier::REVERSED));
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
    fn a_long_title_is_cut_with_an_ellipsis_and_the_label_stays() {
        let mut app = app();
        let long = titled(
            with_runner(
                with_controller(crate::task_app_support::task("a", "local", "running", 1)),
                ExecutionStatus::Running,
                "implement",
            ),
            "Bardzo długi tytuł zadania który nie mieści się w kolumnie",
        );
        app.load(listing(vec![long], "c"));
        let buffer = render(120, 30, &mut app, &Styles::new(true));
        let row: String = rows(&buffer)[list_row(&buffer, "Bardzo")]
            .chars()
            .take(usize::from(layout::LIST_WIDTH))
            .collect();
        assert!(row.contains('…'), "{row:?}");
        assert!(row.contains("implement"), "{row:?}");
    }
}

mod task_app_detail_render {
    use super::*;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use narsor::api::ApiClient;
    use narsor::controller_protocol::{ExecutionEventsResponse, QueueView};
    use narsor::domain::ExecutionStatus;
    use narsor::engine::{ConversationState, PositionStatus, Timeline};
    use narsor::orchestration::QueueReason;
    use narsor::tui::task_app::detail::DetailRead;
    use narsor::tui::task_app::{layout, TaskApp};

    use crate::remote_support::{snapshot, RemoteHarness};
    use crate::task_app_support::{
        displayed, execution_events, listing, position, queued, task, timeline, with_controller,
        with_runner,
    };

    fn app() -> TaskApp {
        let client = ApiClient::new("127.0.0.1:9".parse().unwrap(), "token");
        TaskApp::new(client, "local", None)
    }

    fn press(app: &mut TaskApp, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn render(app: &mut TaskApp, styles: &Styles) -> Buffer {
        draw(120, 30, |frame| {
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
        // Row 0 is the frame; the details start on row 1.
        assert!(
            rows[1].contains("NAR-12 Zmiana API zamówień"),
            "{}",
            rows[1]
        );
        assert!(rows[1].contains("implement-feature v3"), "{}", rows[1]);
        assert!(rows[2].contains("Jira NAR-12 ↗"), "{}", rows[2]);
        assert!(
            rows[2].contains("PR: —"),
            "a link without a value: {}",
            rows[2]
        );
        assert!(!rows[2].contains("PR — ↗"));
        assert!(rows[2].contains("● running"), "{}", rows[2]);
        assert!(rows[2].contains("krok 3/5"), "{}", rows[2]);
        for tab in ["Przebieg", "Rozmowa", "Zdarzenia", "Dyrygent", "Info"] {
            assert!(rows[3].contains(tab), "{tab} in {}", rows[3]);
        }
        assert!(rows[3].contains("podąża"), "{}", rows[3]);
    }

    #[test]
    fn the_axis_has_a_row_per_step_and_marks_the_selected_one() {
        let mut app = running_app();
        let buffer = render(&mut app, &Styles::new(false));
        let screen = screen(&buffer);
        for expected in [
            "✓ plan #1",
            "✓ edit #1",
            "● review #1",
            "▸● review #1",
            "w toku",
        ] {
            assert!(screen.contains(expected), "{expected:?} in\n{screen}");
        }
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
        // Without colour the same is said in words and glyphs.
        let plain = render(&mut app, &Styles::new(true));
        assert!(super::screen(&plain).contains("▸● review #1"));
    }

    #[test]
    fn hyperlinks_are_written_only_when_the_terminal_draws_them() {
        let mut app = running_app();
        let plain = screen(&render(&mut app, &Styles::new(false)));
        assert!(!plain.contains('\u{1b}'));

        app.detail_mut().set_hyperlinks(true);
        let buffer = render(&mut app, &Styles::new(false));
        let screen = screen(&buffer);
        assert!(
            screen.contains("\u{1b}]8;;https://jira.example/browse/NAR-12\u{1b}\\Jira NAR-12 ↗\u{1b}]8;;\u{1b}\\"),
            "{:?}",
            rows(&buffer)[2]
        );
        assert_eq!(
            screen.matches("\u{1b}]8;;https").count(),
            1,
            "only the link with a target"
        );
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
        assert!(rows[2].contains("#2 · czeka na slot (2/3)"), "{}", rows[2]);
        let screen = screen(&buffer);
        assert!(
            screen.contains("jeszcze nie wystartowało, #2 w kolejce"),
            "{screen}"
        );

        // The other execution tabs say the same; Info shows what is there.
        press(&mut app, KeyCode::Right);
        assert!(super::screen(&render(&mut app, &Styles::new(false)))
            .contains("jeszcze nie wystartowało"));
        press(&mut app, KeyCode::Right);
        assert!(super::screen(&render(&mut app, &Styles::new(false)))
            .contains("jeszcze nie wystartowało"));
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Right);
        let info = super::screen(&render(&mut app, &Styles::new(false)));
        assert!(
            info.contains("Zapieczętowane wejścia") && info.contains("launch digest"),
            "{info}"
        );
    }

    fn narsor_waiting(
        mut task: narsor::controller_protocol::TaskSummary,
        position: u32,
        running: u32,
        limit: u32,
    ) -> narsor::controller_protocol::TaskSummary {
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
            rows(&buffer)[2].contains("kontroler offline 4m"),
            "{}",
            rows(&buffer)[2]
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
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Right);
        app.on_detail_read(
            "a",
            1,
            DetailRead::Events {
                after: 0,
                result: Ok(ExecutionEventsResponse::Unavailable {
                    reason: narsor::controller_protocol::ConversationUnavailableReason::RunnerUnreachable,
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
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Right);
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
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Right);
        app.on_detail_read(
            "a",
            1,
            DetailRead::Events {
                after: 0,
                result: Ok(ExecutionEventsResponse::Available {
                    events: execution_events(4),
                    has_more: false,
                    source: narsor::controller_protocol::ExecutionEventsSource::Runner,
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
        assert!(detail.contains("\"sequence\": 4"), "{detail}");
    }

    #[test]
    fn the_conversation_tab_shows_a_message_for_a_task_without_one() {
        let mut app = running_app();
        press(&mut app, KeyCode::Right);
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
            press(&mut app, KeyCode::Right);
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

        let buffer = render(&mut app, &Styles::new(false));
        let screen = screen(&buffer);
        let rows = rows(&buffer);
        assert!(
            rows[1].contains("task-1") && rows[1].contains("implement-feature v1"),
            "{}",
            rows[1]
        );
        assert!(
            rows[2].contains("● running") && rows[2].contains("krok 2/2"),
            "{}",
            rows[2]
        );
        assert!(
            rows[3].contains("Przebieg") && rows[3].contains("Info"),
            "{}",
            rows[3]
        );
        assert!(
            screen.contains("✓ plan #1") && screen.contains("▸● review #1"),
            "{screen}"
        );
        assert!(
            screen.contains("brak rozmowy: script") || screen.contains("review #1 · w toku"),
            "{screen}"
        );
        assert!(screen.contains("● live"));
    }
}

/// U10: the panel "Czeka na Ciebie" drawn at the bottom of the details.
mod task_app_waiting_render {
    use super::*;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use narsor::api::ApiClient;
    use narsor::domain::ExecutionStatus;
    use narsor::orchestration::AttentionKind;
    use narsor::tui::task_app::detail::DetailRead;
    use narsor::tui::task_app::{layout, TaskApp};
    use serde_json::json;

    use crate::task_app_support::{
        listing, position, task, timeline, titled, waiting_on, with_controller, with_pending,
        with_runner,
    };

    fn press(app: &mut TaskApp, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn running() -> narsor::controller_protocol::TaskSummary {
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
                "requirements": [{ "label": "implemented", "data": [], "variables": ["summary"] }],
                "analysis": { "label": "implemented", "data": {}, "variables": { "summary": "all units done" }, "rationale": "Testy przechodzą." }
            },
            "episode": "ep-p",
            "requested_at": "2026-10-09T11:54:00Z"
        })
    }

    fn opened(task: narsor::controller_protocol::TaskSummary) -> TaskApp {
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
                vec![position("plan", narsor::engine::PositionStatus::Succeeded)],
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

    #[test]
    fn wide_the_panel_shows_context_actions_description_and_fields() {
        let mut app = opened(with_pending(running(), gate()));
        let screen = draw_app(&mut app, 120, 40, false);
        for expected in [
            "? Pytanie · review_scope",
            "Czy zakres zmian obejmuje też migrację danych?",
            "Akcje",
            "Opis",
            "▸ 2 revise",
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
            .position(|row| row.contains("▸ 2 revise"))
            .unwrap();
        let description_row = screen
            .lines()
            .position(|row| row.contains("↩ wraca do agenta: plan"))
            .unwrap();
        assert!(
            description_row <= list_row + 1,
            "the description sits beside the list:\n{screen}"
        );
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
    use narsor::controller_protocol::TaskSummary;
    use narsor::domain::ExecutionStatus;
    use narsor::tui::task_app::{layout, TaskApp};

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
        let screen = draw_app(&mut app, 120, 40, false);
        for expected in [
            "Akcje · NAR-12",
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
        press(&mut app, KeyCode::Char('2'));
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
    use narsor::controller_protocol::{ConductorReadResponse, ExecutionEventsSource};
    use narsor::domain::ExecutionStatus;
    use narsor::tui::task_app::detail::DetailRead;
    use narsor::tui::task_app::{layout, DetailTab, TaskApp};
    use serde_json::json;

    use crate::task_app_support::{
        chat_entry, conductor_view, listing, task, titled, with_conductor, with_controller,
        with_runner, without_conductor,
    };

    fn press(app: &mut TaskApp, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn running() -> narsor::controller_protocol::TaskSummary {
        with_conductor(with_runner(
            with_controller(task("a", "local", "running", 1)),
            ExecutionStatus::Running,
            "review",
        ))
    }

    /// The tab of task `a`, with a short conversation read.
    fn opened(task: narsor::controller_protocol::TaskSummary, read: bool) -> TaskApp {
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
            "h harness",
            "M dalsze wpisy",
        ] {
            assert!(text.contains(expected), "{expected:?} is missing:\n{text}");
        }
    }

    #[test]
    fn narrow_the_tab_still_shows_the_conversation_and_the_keys() {
        let mut app = opened(running(), true);
        let text = screen(&draw_app(&mut app, 60, 20, false));
        assert!(text.contains("◆ dyrygent"), "{text}");
        assert!(text.contains("Raczej revise"), "{text}");
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
        let text = screen(&draw_app(&mut app, 120, 30, false));
        assert!(text.contains("wiadomość (wymagane)"), "{text}");
        assert!(text.contains("> Dlaczego?▏"), "{text}");
        assert!(
            text.contains("Enter wyślij") && text.contains("Alt+Enter nowa linia"),
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

    #[test]
    fn the_harness_list_fits_a_narrow_terminal() {
        let mut app = opened(running(), true);
        press(&mut app, KeyCode::Char('h'));
        let text = screen(&draw_app(&mut app, 64, 24, false));
        for expected in ["Harness dyrygenta", "claude-code", "codex", "opencode"] {
            assert!(text.contains(expected), "{expected:?} is missing:\n{text}");
        }
    }

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
    use narsor::controller_protocol::LegacyExecutionListResponse;
    use narsor::domain::ParamType;
    use narsor::tui::task_app::{layout, Request, TaskApp};

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

    fn draw_app(app: &mut TaskApp, width: u16, height: u16, no_color: bool) -> String {
        screen(&draw(width, height, |frame| {
            layout::render(frame, app, &Styles::new(no_color), Instant::now())
        }))
    }

    #[test]
    fn the_wide_form_has_the_list_the_description_and_the_fields_beside_the_untouched_task_list() {
        let mut app = app();
        open_form(&mut app);
        press(&mut app, KeyCode::Tab);
        keys(&mut app, "NAR-18");
        let screen = draw_app(&mut app, 130, 34, false);
        for expected in [
            "Nowe zadanie → trafi na koniec kolejki (#3)",
            "implement-feature",
            "fix-bug",
            "ścieżka…",
            "Opis",
            "Pełny cykl",
            "wbudowany · wersja 1",
            "ticket_id",
            "(wymagane)",
            "numer zgłoszenia",
            "> NAR-18▏",
            "attempts",
            "liczba całkowita · ile razy ponawiać",
            "‹ false ›",
            "‹ narsor ›",
            // The list of tasks is where it was.
            "Zadania",
            "NAR-14",
            "NAR-12",
        ] {
            assert!(screen.contains(expected), "{expected}:\n{screen}");
        }
    }

    #[test]
    fn the_narrow_form_takes_the_whole_screen_and_stacks_the_description_under_the_list() {
        let mut app = app();
        open_form(&mut app);
        press(&mut app, KeyCode::Tab);
        keys(&mut app, "NAR-18");
        let screen = draw_app(&mut app, 72, 30, false);
        for expected in [
            "Nowe zadanie → trafi na koniec kolejki (#3)",
            "implement-feature",
            "Opis",
            "ticket_id",
            "> NAR-18▏",
        ] {
            assert!(screen.contains(expected), "{expected}:\n{screen}");
        }
        assert!(!screen.contains("Zadania"), "the list gives way:\n{screen}");
        for row in screen.lines() {
            assert!(row.chars().count() <= 72, "{row}");
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
        press(&mut app, KeyCode::Tab);
        keys(&mut app, "NAR-18");
        press(&mut app, KeyCode::Down);
        keys(&mut app, "1-2");
        press(&mut app, KeyCode::Enter);
        let screen = draw_app(&mut app, 130, 34, false);
        assert!(
            screen.contains("✗ Pole 'attempts' wymaga liczby całkowitej."),
            "{screen}"
        );
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
        press(&mut app, KeyCode::Tab);
        keys(&mut app, "/srv/bad.yaml");
        app.on_path_checked(
            "/srv/bad.yaml",
            Ok(narsor::controller_protocol::ValidateWorkflowResponse {
                valid: false,
                name: None,
                version: None,
                inputs: vec![],
                violations: vec![
                    "state 'plan' has no transition".to_string(),
                    "state 'ship' is not reachable".to_string(),
                    "input 'ticket' is declared twice".to_string(),
                ],
            }),
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
        press(&mut app, KeyCode::Tab);
        keys(&mut app, "NAR-18");
        let Request::SendTask(id) = press(&mut app, KeyCode::Enter) else {
            panic!("a send was expected");
        };
        let sending = draw_app(&mut app, 130, 30, false);
        for expected in [
            "↻ NAR-18",
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
        for expected in ["✗ NAR-18", "nie wysłano", "Dane zostały zachowane", &id] {
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
    use narsor::orchestration::{AttentionItem, AttentionKind};
    use narsor::tui::task_app::{layout, TaskApp};

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

        // The frame: the connection by name and state, the counters.
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
        assert!(
            row_of("NAR-14").starts_with("│ ● NAR-14"),
            "{}",
            row_of("NAR-14")
        );
        assert!(
            row_of("NAR-12").starts_with("│?● NAR-12"),
            "{}",
            row_of("NAR-12")
        );
        assert!(row_of("NAR-16").starts_with("│ ◷ NAR-16") && row_of("NAR-16").contains("#1"));
        assert!(row_of("NAR-17").starts_with("│ ◷ NAR-17") && row_of("NAR-17").contains("#2"));
        let order: Vec<usize> = ["NAR-14", "NAR-12", "NAR-16", "NAR-17"]
            .iter()
            .map(|title| {
                rows.iter()
                    .position(|row| list_part(row).contains(title))
                    .unwrap()
            })
            .collect();
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{order:?}");

        // The details of the selected task: header, tabs, and the panel of
        // what waits for the operator, with its own keys on the bar.
        assert!(
            rows[1].contains("NAR-12") && rows[1].contains("implement-feature v1"),
            "{}",
            rows[1]
        );
        assert!(rows[2].contains("● running"), "{}", rows[2]);
        assert!(
            rows[3].contains("Przebieg")
                && rows[3].contains("Dyrygent")
                && rows[3].contains("Info")
        );
        assert!(screen.contains("? Approval · review"), "{screen}");
        assert!(screen.contains("Approve the plan?"), "{screen}");
        let bar = &rows[rows.len() - 2];
        assert!(
            bar.contains("Enter wyślij") && bar.contains("Esc zwiń"),
            "{bar}"
        );
    }
}
