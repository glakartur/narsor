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

/// R17: views name roles, not colours. The list holds the sources of the new
/// views; U14 extends the check to the whole of `src/tui` by listing the
/// directory instead.
mod colour_discipline {
    use super::*;

    /// The new views' sources, relative to `src/tui`. Everything that draws
    /// belongs here; `style.rs` is the one file allowed to name a colour.
    const NEW_VIEW_SOURCES: &[&str] = &[
        "chooser.rs",
        "connection.rs",
        "keymap.rs",
        "task_app/layout.rs",
        "task_app/list.rs",
        "task_app/mod.rs",
        "task_app/notify.rs",
    ];

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
                    with_runner(controlled("a", "running", 1), ExecutionStatus::Running, "edit"),
                    "NAR-14",
                ),
                titled(
                    waiting(
                        with_runner(controlled("b", "running", 2), ExecutionStatus::Running, "approval"),
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
                    with_runner(controlled("f", "running", 6), ExecutionStatus::Completed, "done"),
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
        assert!(rows(&buffer)[1].contains("NAR-14"), "the details name the selected task");

        let list_rows: Vec<String> = rows(&buffer)
            .iter()
            .map(|row| row.chars().skip(1).take(27).collect::<String>())
            .collect();
        let find = |needle: &str| list_rows.iter().position(|row| row.contains(needle)).unwrap();
        assert!(list_rows[find("NAR-14")].starts_with(" ●"), "{:?}", list_rows[find("NAR-14")]);
        assert!(list_rows[find("NAR-12")].starts_with("?●"));
        assert!(list_rows[find("NAR-9 ")].starts_with("!✗"));
        assert!(list_rows[find("NAR-16")].starts_with(" ◷"));
        assert!(list_rows[find("NAR-16")].contains("#1"));
        assert!(list_rows[find("NAR-17")].contains("#2"));
        assert!(list_rows[find("NAR-8 ")].starts_with(" ✓"));
        assert!(list_rows[find("NAR-12")].contains("approval"));
        assert!(
            find("NAR-14") < find("NAR-12") && find("NAR-12") < find("NAR-9 ") && find("NAR-17") < find("NAR-8 "),
            "the rows are in the server's order"
        );

        let bar = rows(&buffer).last().cloned().unwrap();
        let _ = bar;
        assert!(screen.contains("następny czekający"), "the bar shows the list keys:\n{screen}");
        assert!(screen.contains("szczegóły zadania"));
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
        assert_eq!(divider.get(1), Some(&usize::from(layout::LIST_WIDTH)), "{header_row:?}");
    }

    #[test]
    fn at_90_columns_the_list_is_full_screen_and_enter_swaps_it_for_the_details() {
        let mut app = busy_app();
        let list = render(90, 30, &mut app, &Styles::new(false));
        assert!(screen(&list).contains("NAR-17"));
        assert!(!screen(&list).contains("szczegóły zadania"));
        assert!(screen(&list).contains("Zadania"));

        press(&mut app, KeyCode::Enter);
        let details = render(90, 30, &mut app, &Styles::new(false));
        assert!(screen(&details).contains("szczegóły zadania"));
        assert!(!screen(&details).contains("NAR-17"));
        assert!(screen(&details).contains("lista"), "the bar offers the way back");
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
        assert!(rows(&buffer)[0].contains("◌ łączę z local"), "{:?}", rows(&buffer)[0]);
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
        assert_eq!(cells[1].fg, Color::Yellow, "the marker keeps its role colour");
        assert_eq!(cells[2].symbol(), "●");
        assert_eq!(cells[2].fg, Color::Green);
        assert!(cells[1].modifier.contains(Modifier::REVERSED));
        let other = list_row(&buffer, "NAR-14");
        assert!(!buffer.content()[other * width + 4].modifier.contains(Modifier::REVERSED));
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
        assert!(buffer.content()[row * width + 4].modifier.contains(Modifier::REVERSED));
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
        assert!(list_row(&buffer, "NAR-17") < list_row(&buffer, "NAR-16"), "the preview put NAR-16 after NAR-17");
        assert!(screen.contains("zatwierdź") && screen.contains("anuluj"), "{screen}");
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
        assert!(rows(&buffer).iter().any(|row| row.contains("zadanie nie jest w kolejce")), "{}", screen(&buffer));
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
