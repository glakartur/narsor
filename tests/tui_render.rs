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
        "failure_form.rs",
        "input_form.rs",
        "keymap.rs",
        "task_app/actions.rs",
        "task_app/detail.rs",
        "task_app/layout.rs",
        "task_app/list.rs",
        "task_app/mod.rs",
        "task_app/notify.rs",
        "task_app/tabs.rs",
        "task_app/tabs/conversation.rs",
        "task_app/tabs/events.rs",
        "task_app/tabs/info.rs",
        "task_app/tabs/progress.rs",
        "task_app/waiting.rs",
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
        assert!(screen.contains("Przebieg") && screen.contains("Zdarzenia"), "the details have their tabs:\n{screen}");
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
        assert!(!screen(&list).contains("Przebieg"));
        assert!(screen(&list).contains("Zadania"));

        press(&mut app, KeyCode::Enter);
        let details = render(90, 30, &mut app, &Styles::new(false));
        assert!(screen(&details).contains("Przebieg"));
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
        assert!(rows[1].contains("NAR-12 Zmiana API zamówień"), "{}", rows[1]);
        assert!(rows[1].contains("implement-feature v3"), "{}", rows[1]);
        assert!(rows[2].contains("Jira NAR-12 ↗"), "{}", rows[2]);
        assert!(rows[2].contains("PR: —"), "a link without a value: {}", rows[2]);
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
        for expected in ["✓ plan #1", "✓ edit #1", "● review #1", "▸● review #1", "w toku"] {
            assert!(screen.contains(expected), "{expected:?} in\n{screen}");
        }
        // The axis is the left part of the details: the selected step's own heading is on the right.
        let row = |needle: &str| {
            rows(&buffer)
                .iter()
                .position(|row| row.chars().skip(30).take(30).collect::<String>().contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} is not on the axis"))
        };
        assert!(row("plan #1") < row("edit #1") && row("edit #1") < row("review #1") && row("review #1") < row("test #1"));
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
        assert_eq!(screen.matches("\u{1b}]8;;https").count(), 1, "only the link with a target");
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
            app.load(listing(vec![displayed(running, "NAR-12", &[("Jira", "NAR-12", Some(url))])], "c"));
            app.detail_mut().set_hyperlinks(true);
            let screen = screen(&render(&mut app, &Styles::new(false)));
            assert!(!screen.contains("\u{1b}]8"), "{url:?} must not become a hyperlink");
        }
    }

    #[test]
    fn a_queued_task_says_it_has_not_started_and_where_it_waits() {
        let mut app = app();
        let waiting = narsor_waiting(queued("b", 2), 2, 2, 3);
        app.load(listing(vec![queued("a", 1), displayed(waiting, "NAR-17", &[])], "c"));
        app.list_mut().select("b");
        press(&mut app, KeyCode::Enter);

        let buffer = render(&mut app, &Styles::new(false));
        let rows = rows(&buffer);
        assert!(rows[2].contains("#2 · czeka na slot (2/3)"), "{}", rows[2]);
        let screen = screen(&buffer);
        assert!(screen.contains("jeszcze nie wystartowało, #2 w kolejce"), "{screen}");

        // The other execution tabs say the same; Info shows what is there.
        press(&mut app, KeyCode::Right);
        assert!(super::screen(&render(&mut app, &Styles::new(false))).contains("jeszcze nie wystartowało"));
        press(&mut app, KeyCode::Right);
        assert!(super::screen(&render(&mut app, &Styles::new(false))).contains("jeszcze nie wystartowało"));
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Right);
        let info = super::screen(&render(&mut app, &Styles::new(false)));
        assert!(info.contains("Zapieczętowane wejścia") && info.contains("launch digest"), "{info}");
    }

    fn narsor_waiting(
        mut task: narsor::controller_protocol::TaskSummary,
        position: u32,
        running: u32,
        limit: u32,
    ) -> narsor::controller_protocol::TaskSummary {
        let reason = QueueReason::WaitingForCapacity { position, running, limit };
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
        assert!(rows(&buffer)[2].contains("kontroler offline 4m"), "{}", rows(&buffer)[2]);
    }

    #[test]
    fn an_archived_task_says_its_storage_is_gone() {
        let mut app = app();
        app.load(listing(vec![task("a", "local", "archived", 1)], "c"));
        press(&mut app, KeyCode::Enter);
        let screen = screen(&render(&mut app, &Styles::new(false)));
        assert!(screen.contains("storage usunięty przy archiwizacji"), "{screen}");
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
        assert!(screen.contains("zdarzenia niedostępne: the runner has not been heard from lately"), "{screen}");
        // The rest of the screen is intact.
        assert!(screen.contains("NAR-12 Zmiana API zamówień"));
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
        assert!(list.contains("state_entered") && list.contains("state-4"), "{list}");
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
                &[(&hostile("La@bel"), &hostile("Te@xt"), Some("https://x.example/"))],
            )],
            "c",
        ));
        let mut step = position("st@ep", PositionStatus::Failed);
        step.label = hostile("st@ep #1");
        step.error = Some(hostile("bo@om"));
        app.on_detail_read("a", 1, DetailRead::Timeline(Ok(timeline("a", vec![step], None))));
        press(&mut app, KeyCode::Enter);
        for _ in 0..5 {
            let screen = screen(&render(&mut app, &Styles::new(false)));
            assert!(!screen.contains('\u{1b}') && !screen.contains('\u{202e}'), "{screen}");
            press(&mut app, KeyCode::Right);
        }
    }

    #[tokio::test]
    async fn the_wide_screen_with_a_remote_task_shows_the_header_the_tabs_and_the_axis() {
        let harness = RemoteHarness::start().await;
        let mut recorded = position("plan", PositionStatus::Succeeded);
        recorded.conversation = ConversationState::NotCreated { reason: "script".into() };
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
        assert!(rows[1].contains("task-1") && rows[1].contains("implement-feature v1"), "{}", rows[1]);
        assert!(rows[2].contains("● running") && rows[2].contains("krok 2/2"), "{}", rows[2]);
        assert!(rows[3].contains("Przebieg") && rows[3].contains("Info"), "{}", rows[3]);
        assert!(screen.contains("✓ plan #1") && screen.contains("▸● review #1"), "{screen}");
        assert!(screen.contains("brak rozmowy: script") || screen.contains("review #1 · w toku"), "{screen}");
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
        let mut app = TaskApp::new(ApiClient::new("127.0.0.1:9".parse().unwrap(), "token"), "local", None);
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
            assert!(screen.contains(expected), "{expected:?} is missing:\n{screen}");
        }
        let list_row = screen.lines().position(|row| row.contains("▸ 2 revise")).unwrap();
        let description_row = screen.lines().position(|row| row.contains("↩ wraca do agenta: plan")).unwrap();
        assert!(description_row <= list_row + 1, "the description sits beside the list:\n{screen}");
    }

    #[test]
    fn narrow_the_description_goes_under_the_actions() {
        let mut app = opened(with_pending(running(), gate()));
        let screen = draw_app(&mut app, 64, 40, false);
        let rows: Vec<&str> = screen.lines().collect();
        let last_action = rows.iter().position(|row| row.contains("reject")).expect("the list");
        let description = rows.iter().position(|row| row.contains("Opis")).expect("the description");
        assert!(description > last_action, "{screen}");
        assert!(screen.contains("↩ wraca do agenta: plan"), "{screen}");
    }

    #[test]
    fn without_colour_the_markers_survive() {
        let mut app = opened(with_pending(running(), gate()));
        let question = draw_app(&mut app, 120, 40, true);
        assert!(question.contains('?') && question.contains('↩'), "{question}");

        let mut app = opened(with_pending(running(), problem()));
        let problem = draw_app(&mut app, 120, 40, true);
        for marker in ["! Problem", "◆", "◆ dyrygent: Testy przechodzą."] {
            assert!(problem.contains(marker), "{marker:?} is missing:\n{problem}");
        }
        assert!(problem.contains("variables.summary ◆"), "the proposed value is marked:\n{problem}");
    }

    #[test]
    fn the_panel_gives_its_rows_back_to_the_tab_when_folded() {
        let mut app = opened(with_pending(running(), gate()));
        let open = draw_app(&mut app, 120, 40, false);
        press(&mut app, KeyCode::Esc);
        let folded = draw_app(&mut app, 120, 40, false);
        assert!(open.contains("Akcje") && !folded.contains("Akcje"), "{folded}");
        assert!(folded.contains("? Pytanie · review_scope") && folded.contains("Tab rozwiń"), "{folded}");
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
        assert!(screen.contains("Uruchom od nowa") && screen.contains("Archiwizuj"), "{screen}");
        assert!(screen.contains('!'), "the irreversible action is marked");

        let mut hostile = gate();
        hostile["instruction"] = json!("pytanie\u{1b}[31m\u{202e}ewil\u{7}");
        let mut app = opened(with_pending(running(), hostile));
        let screen = draw_app(&mut app, 120, 40, false);
        assert!(!screen.contains('\u{1b}') && !screen.contains('\u{202e}') && !screen.contains('\u{7}'), "{screen}");
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
        assert!(menu.contains("wykonaj") && menu.contains("zamknij"), "{menu}");
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
        assert!(menu.contains("niedostępne: zadanie nie jest w kolejce"), "{menu}");
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('x'));
        let confirm = draw_app(&mut app, 120, 40, true);
        assert!(confirm.contains("!") && confirm.contains("y potwierdź"), "{confirm}");
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
            &[("Ji\u{1b}[31mra\u{202e}", "NAR", Some("https://jira.example/x"))],
        );
        let mut app = opened(task, "o");
        let screen = draw_app(&mut app, 120, 40, false);
        assert!(
            !screen.contains('\u{1b}') && !screen.contains('\u{202e}'),
            "{screen}"
        );
    }
}
