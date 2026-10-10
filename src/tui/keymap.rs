//! The one key map (D-T10, R16).
//!
//! A key means one thing in one context, and the table below is the single
//! source of three things that used to be able to drift apart: what a key
//! *does* ([`lookup`]), the shortcut bar at the bottom of the screen
//! ([`hint_line`]) and the `?` help ([`help_entries`], [`render_help`]).
//! Handlers match on [`Action`], never on a raw key.
//!
//! # Contexts
//!
//! A [`Context`] is "where the operator is": the list, the task details, the
//! answer panel, the action menu, the confirmation window, a form, the move
//! mode, the history (its list and its read-only details) and the
//! repositories. [`Context::Global`]
//! holds what works everywhere and is inherited by every context that is not
//! taking typed text ([`Context::inherits_global`]). A form (or a panel whose
//! focus is in a field) must be able to type `q` and `?`, so it inherits
//! nothing.
//!
//! # Consistency
//!
//! [`check_consistency`] is what keeps the table honest: no key twice in one
//! context, no key that a context would shadow from [`Context::Global`], no
//! row without a description. A test runs it over [`BINDINGS`], and a second
//! one proves it fails when a row is duplicated.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use super::style::{Role, Styles};

/// Where the operator is, as far as keys are concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Context {
    /// Works everywhere a context inherits it.
    Global,
    /// The task list has the focus.
    List,
    /// The task details have the focus (the tab content).
    Details,
    /// The "waiting for you" panel has the focus, on its action list.
    Panel,
    /// The action menu is open.
    Menu,
    /// The `y` / `n` window that shows what an action `!` will do.
    Confirm,
    /// A form is taking input: new task, repositories, a panel's fields.
    Form,
    /// A queued task is being moved.
    Move,
    /// The history's list has the focus.
    History,
    /// The details of an entry of the history have the focus (read only).
    HistoryDetails,
    /// The list of repositories has the focus.
    Repositories,
}

impl Context {
    /// Every context, in the order help and tests walk them.
    pub const ALL: [Context; 11] = [
        Context::Global,
        Context::List,
        Context::Details,
        Context::Panel,
        Context::Menu,
        Context::Confirm,
        Context::Form,
        Context::Move,
        Context::History,
        Context::HistoryDetails,
        Context::Repositories,
    ];

    /// The name help uses.
    pub fn name(self) -> &'static str {
        match self {
            Self::Global => "wszędzie",
            Self::List => "lista",
            Self::Details => "szczegóły",
            Self::Panel => "panel",
            Self::Menu => "menu",
            Self::Confirm => "potwierdzenie",
            Self::Form => "formularz",
            Self::Move => "przesuwanie",
            Self::History => "historia",
            Self::HistoryDetails => "historia: szczegóły",
            Self::Repositories => "repozytoria",
        }
    }

    /// Whether the global keys also work here. False where typed text must
    /// not be intercepted.
    pub fn inherits_global(self) -> bool {
        !matches!(self, Self::Global | Self::Form)
    }
}

/// What a key does. Handlers match on this, never on the key itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// Show the contextual help.
    Help,
    /// Leave the interface.
    Quit,
    /// Cursor one row down.
    Down,
    /// Cursor one row up.
    Up,
    /// Cursor to the first row.
    Top,
    /// Cursor to the last row.
    Bottom,
    /// Scroll a page down.
    PageDown,
    /// Scroll a page up.
    PageUp,
    /// Select the next task that waits for the operator.
    NextWaiting,
    /// Answer the waiting task, or go to its details.
    Open,
    /// Open the action menu.
    ActionMenu,
    /// Open the new-task form.
    NewTask,
    /// Hide or show finished tasks.
    ToggleDone,
    /// Open the history.
    History,
    /// Open the repositories.
    Repositories,
    /// Previous details tab.
    PrevTab,
    /// Next details tab.
    NextTab,
    /// Move the focus between the content and the panel, or between a
    /// panel's actions and fields.
    SwitchFocus,
    /// Expand or collapse the selected entry.
    Expand,
    /// The raw view of the selected entry.
    RawEntry,
    /// The raw view of everything.
    RawAll,
    /// Follow the step that is running.
    Follow,
    /// Back to the list.
    Back,
    /// Move a queued task one place up.
    MoveUp,
    /// Move a queued task one place down.
    MoveDown,
    /// Enter the move mode.
    MoveMode,
    /// Pause or resume.
    PauseResume,
    /// Interrupt the running step.
    Interrupt,
    /// A manual transition.
    Transition,
    /// Start again.
    Restart,
    /// Archive.
    Archive,
    /// Open a link.
    OpenLink,
    /// Pick the n-th entry of a list; the digit comes from the key event
    /// ([`digit_of`]).
    Pick,
    /// Submit, or go on to the first field that needs filling.
    Submit,
    /// A new line in a text field.
    Newline,
    /// Collapse the panel, close the menu, leave the form.
    Cancel,
    /// The previous waiting item of the task.
    PrevPending,
    /// The next waiting item of the task.
    NextPending,
    /// The next field.
    NextField,
    /// The previous field.
    PrevField,
    /// Step an option, or the cursor of a text field.
    Step,
    /// Confirm the chosen place or entry.
    Confirm,
    /// Say yes to what the confirmation window shows.
    Accept,
    /// Write to the conductor (the *Dyrygent* tab).
    Compose,
    /// Undo the selected decision of the conductor.
    UndoDecision,
    /// Read the next page of the conductor's chat.
    LoadMore,
    /// Choose the installation's default agent harness (the header says
    /// which one is in force).
    ChooseHarness,
    /// Send a task again that did not get through (the new-task form).
    RetrySend,
    /// Recover an old execution of the history.
    Recover,
    /// Add a repository.
    AddRepository,
    /// Remove the selected repository.
    RemoveRepository,
}

/// A key a binding listens to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A key with the modifiers (Control, Alt) it needs. Shift is never
    /// listed: an upper-case letter is its own character.
    Press(KeyCode, KeyModifiers),
    /// Any of `1`–`9`.
    Digit,
}

impl Key {
    /// A plain character.
    pub const fn char(character: char) -> Self {
        Self::Press(KeyCode::Char(character), KeyModifiers::NONE)
    }

    /// A non-character key without modifiers.
    pub const fn code(code: KeyCode) -> Self {
        Self::Press(code, KeyModifiers::NONE)
    }

    /// A key with Alt held.
    pub const fn alt(code: KeyCode) -> Self {
        Self::Press(code, KeyModifiers::ALT)
    }

    /// A character with Control held.
    pub const fn ctrl(character: char) -> Self {
        Self::Press(KeyCode::Char(character), KeyModifiers::CONTROL)
    }

    /// Whether `event` is this key. Shift is ignored on both sides.
    pub fn matches(self, event: &KeyEvent) -> bool {
        let held = event.modifiers - KeyModifiers::SHIFT;
        match self {
            Self::Digit => digit_of(event).is_some(),
            Self::Press(code, modifiers) => event.code == code && held == modifiers,
        }
    }

    /// How the key is written in the bar and in help.
    pub fn display(self) -> String {
        let Self::Press(code, modifiers) = self else {
            return "1-9".to_string();
        };
        let mut text = String::new();
        if modifiers.contains(KeyModifiers::CONTROL) {
            text.push_str("Ctrl+");
        }
        if modifiers.contains(KeyModifiers::ALT) {
            text.push_str("Alt+");
        }
        match code {
            KeyCode::Char(' ') => text.push_str("Space"),
            KeyCode::Char(character) if modifiers.contains(KeyModifiers::CONTROL) => {
                text.push(character.to_ascii_uppercase());
            }
            KeyCode::Char(character) => text.push(character),
            KeyCode::Enter => text.push_str("Enter"),
            KeyCode::Esc => text.push_str("Esc"),
            KeyCode::Tab => text.push_str("Tab"),
            KeyCode::BackTab => text.push_str("Shift+Tab"),
            KeyCode::Left => text.push('←'),
            KeyCode::Right => text.push('→'),
            KeyCode::Up => text.push('↑'),
            KeyCode::Down => text.push('↓'),
            KeyCode::PageUp => text.push_str("PgUp"),
            KeyCode::PageDown => text.push_str("PgDn"),
            other => text.push_str(&format!("{other:?}")),
        }
        text
    }

    /// A key event that presses this key; for [`Key::Digit`], `1`.
    pub fn to_event(self) -> KeyEvent {
        let (code, modifiers) = match self {
            Self::Press(code, modifiers) => (code, modifiers),
            Self::Digit => (KeyCode::Char('1'), KeyModifiers::NONE),
        };
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }
}

/// The digit `1`–`9` an event carries, when it is a bare digit press.
pub fn digit_of(event: &KeyEvent) -> Option<u8> {
    let held = event.modifiers - KeyModifiers::SHIFT;
    match event.code {
        KeyCode::Char(character @ '1'..='9') if held.is_empty() => Some(character as u8 - b'0'),
        _ => None,
    }
}

/// One row of the table: `(context, key, action, label, description)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    /// Where the key works.
    pub context: Context,
    /// The key.
    pub key: Key,
    /// What it does.
    pub action: Action,
    /// The short name on the shortcut bar. Empty keeps the row off the bar
    /// and in help only.
    pub label: &'static str,
    /// The sentence help shows.
    pub description: &'static str,
}

/// Shorthand that keeps the table one row per line.
pub const fn bind(
    context: Context,
    key: Key,
    action: Action,
    label: &'static str,
    description: &'static str,
) -> Binding {
    Binding {
        context,
        key,
        action,
        label,
        description,
    }
}

/// What `h` does, in every context that has it.
const CHOOSE_HARNESS: &str =
    "domyślny harness instalacji: wybór z listy obsługiwanych albo powrót do konfiguracji";

/// The key map.
pub static BINDINGS: &[Binding] = &[
    // Everywhere a context inherits them.
    bind(
        Context::Global,
        Key::char('?'),
        Action::Help,
        "?",
        "pomoc dla bieżącego widoku",
    ),
    bind(
        Context::Global,
        Key::char('q'),
        Action::Quit,
        "wyjście",
        "wyjście z narsora (z potwierdzeniem, gdy jest niewysłany szkic)",
    ),
    // The task list.
    bind(
        Context::List,
        Key::char('j'),
        Action::Down,
        "",
        "kursor w dół",
    ),
    bind(
        Context::List,
        Key::code(KeyCode::Down),
        Action::Down,
        "",
        "kursor w dół",
    ),
    bind(
        Context::List,
        Key::char('k'),
        Action::Up,
        "",
        "kursor w górę",
    ),
    bind(
        Context::List,
        Key::code(KeyCode::Up),
        Action::Up,
        "",
        "kursor w górę",
    ),
    bind(
        Context::List,
        Key::char('g'),
        Action::Top,
        "",
        "na początek listy",
    ),
    bind(
        Context::List,
        Key::char('G'),
        Action::Bottom,
        "",
        "na koniec listy",
    ),
    bind(
        Context::List,
        Key::char(' '),
        Action::NextWaiting,
        "następny czekający",
        "zaznacz następne zadanie, które czeka na Ciebie",
    ),
    bind(
        Context::List,
        Key::code(KeyCode::Enter),
        Action::Open,
        "otwórz",
        "odpowiedz na zadaniu, które czeka, albo przejdź do szczegółów",
    ),
    bind(
        Context::List,
        Key::char('.'),
        Action::ActionMenu,
        "akcje",
        "menu akcji zadania",
    ),
    bind(
        Context::List,
        Key::char('n'),
        Action::NewTask,
        "nowe",
        "nowe zadanie",
    ),
    bind(
        Context::List,
        Key::char('z'),
        Action::ToggleDone,
        "zakończone",
        "ukryj albo pokaż zakończone zadania",
    ),
    bind(
        Context::List,
        Key::char('H'),
        Action::History,
        "historia",
        "historia: zarchiwizowane zadania i dawne executions",
    ),
    bind(
        Context::List,
        Key::char('R'),
        Action::Repositories,
        "repozytoria",
        "repozytoria",
    ),
    bind(
        Context::List,
        Key::char('h'),
        Action::ChooseHarness,
        "harness",
        CHOOSE_HARNESS,
    ),
    bind(
        Context::List,
        Key::char('K'),
        Action::MoveUp,
        "",
        "przesuń zadanie w kolejce o jedno miejsce wyżej",
    ),
    bind(
        Context::List,
        Key::char('J'),
        Action::MoveDown,
        "",
        "przesuń zadanie w kolejce o jedno miejsce niżej",
    ),
    bind(
        Context::List,
        Key::char('m'),
        Action::MoveMode,
        "",
        "tryb przesuwania: wybierz miejsce w kolejce",
    ),
    bind(
        Context::List,
        Key::char('p'),
        Action::PauseResume,
        "",
        "wstrzymaj albo wznów zadanie",
    ),
    bind(
        Context::List,
        Key::char('x'),
        Action::Interrupt,
        "",
        "przerwij bieżący krok (wymaga potwierdzenia)",
    ),
    bind(
        Context::List,
        Key::char('t'),
        Action::Transition,
        "",
        "przejście ręczne do wybranego stanu",
    ),
    bind(
        Context::List,
        Key::char('s'),
        Action::Restart,
        "",
        "uruchom zadanie od nowa",
    ),
    bind(
        Context::List,
        Key::char('a'),
        Action::Archive,
        "",
        "archiwizuj zadanie (wymaga potwierdzenia)",
    ),
    bind(
        Context::List,
        Key::char('o'),
        Action::OpenLink,
        "",
        "otwórz link zadania w przeglądarce",
    ),
    bind(
        Context::List,
        Key::char('r'),
        Action::RetrySend,
        "",
        "ponów wysłanie nowego zadania, które nie dotarło do serwera",
    ),
    // The task details.
    // `Esc` first: on a narrow screen the details are full screen and the way
    // back must survive a shortened bar, which drops hints from the end.
    bind(
        Context::Details,
        Key::code(KeyCode::Esc),
        Action::Back,
        "lista",
        "wróć na listę z zachowanym zaznaczeniem",
    ),
    bind(
        Context::Details,
        Key::code(KeyCode::Left),
        Action::PrevTab,
        "zakładki",
        "poprzednia zakładka",
    ),
    bind(
        Context::Details,
        Key::code(KeyCode::Right),
        Action::NextTab,
        "",
        "następna zakładka",
    ),
    bind(
        Context::Details,
        Key::code(KeyCode::Tab),
        Action::SwitchFocus,
        "treść ↔ panel",
        "przełącz fokus między treścią zakładki a panelem na dole",
    ),
    bind(
        Context::Details,
        Key::char('j'),
        Action::Down,
        "przewijanie",
        "przewiń w dół",
    ),
    bind(
        Context::Details,
        Key::code(KeyCode::Down),
        Action::Down,
        "",
        "przewiń w dół",
    ),
    bind(
        Context::Details,
        Key::char('k'),
        Action::Up,
        "",
        "przewiń w górę",
    ),
    bind(
        Context::Details,
        Key::code(KeyCode::Up),
        Action::Up,
        "",
        "przewiń w górę",
    ),
    bind(
        Context::Details,
        Key::code(KeyCode::PageDown),
        Action::PageDown,
        "",
        "strona w dół",
    ),
    bind(
        Context::Details,
        Key::code(KeyCode::PageUp),
        Action::PageUp,
        "",
        "strona w górę",
    ),
    bind(
        Context::Details,
        Key::char('g'),
        Action::Top,
        "",
        "na początek",
    ),
    bind(
        Context::Details,
        Key::char('G'),
        Action::Bottom,
        "",
        "na koniec",
    ),
    bind(
        Context::Details,
        Key::code(KeyCode::Enter),
        Action::Expand,
        "rozwiń",
        "rozwiń albo zwiń wpis",
    ),
    bind(
        Context::Details,
        Key::char(' '),
        Action::Expand,
        "",
        "rozwiń albo zwiń wpis",
    ),
    bind(
        Context::Details,
        Key::char('v'),
        Action::RawEntry,
        "",
        "widok surowy wpisu",
    ),
    bind(
        Context::Details,
        Key::ctrl('v'),
        Action::RawAll,
        "",
        "widok surowy całości",
    ),
    bind(
        Context::Details,
        Key::char('f'),
        Action::Follow,
        "podążaj",
        "włącz albo wyłącz podążanie za bieżącym krokiem",
    ),
    bind(
        Context::Details,
        Key::char('.'),
        Action::ActionMenu,
        "akcje",
        "menu akcji zadania",
    ),
    bind(
        Context::Details,
        Key::char('K'),
        Action::MoveUp,
        "",
        "przesuń zadanie w kolejce o jedno miejsce wyżej",
    ),
    bind(
        Context::Details,
        Key::char('J'),
        Action::MoveDown,
        "",
        "przesuń zadanie w kolejce o jedno miejsce niżej",
    ),
    bind(
        Context::Details,
        Key::char('m'),
        Action::MoveMode,
        "",
        "tryb przesuwania: wybierz miejsce w kolejce",
    ),
    bind(
        Context::Details,
        Key::char('p'),
        Action::PauseResume,
        "",
        "wstrzymaj albo wznów zadanie",
    ),
    bind(
        Context::Details,
        Key::char('x'),
        Action::Interrupt,
        "",
        "przerwij bieżący krok (wymaga potwierdzenia)",
    ),
    bind(
        Context::Details,
        Key::char('t'),
        Action::Transition,
        "",
        "przejście ręczne do wybranego stanu",
    ),
    bind(
        Context::Details,
        Key::char('s'),
        Action::Restart,
        "",
        "uruchom zadanie od nowa",
    ),
    bind(
        Context::Details,
        Key::char('a'),
        Action::Archive,
        "",
        "archiwizuj zadanie (wymaga potwierdzenia)",
    ),
    bind(
        Context::Details,
        Key::char('o'),
        Action::OpenLink,
        "",
        "otwórz link zadania w przeglądarce",
    ),
    bind(
        Context::Details,
        Key::char('r'),
        Action::RetrySend,
        "",
        "ponów wysłanie nowego zadania, które nie dotarło do serwera",
    ),
    // The *Dyrygent* tab. Help only: the tab draws its own line of them.
    bind(
        Context::Details,
        Key::char('c'),
        Action::Compose,
        "",
        "zakładka Dyrygent: napisz do dyrygenta (Enter wysyła, Ctrl+J nowa linia, Esc kończy)",
    ),
    bind(
        Context::Details,
        Key::char('u'),
        Action::UndoDecision,
        "",
        "zakładka Dyrygent: cofnij zaznaczoną decyzję dyrygenta (wymaga potwierdzenia)",
    ),
    bind(
        Context::Details,
        Key::char('M'),
        Action::LoadMore,
        "",
        "zakładka Dyrygent: wczytaj dalsze wpisy czatu",
    ),
    bind(
        Context::Details,
        Key::char('h'),
        Action::ChooseHarness,
        "",
        CHOOSE_HARNESS,
    ),
    // The "waiting for you" panel, focus on its actions. Focus in a field
    // types text; the chooser handles those keys.
    bind(
        Context::Panel,
        Key::code(KeyCode::Tab),
        Action::SwitchFocus,
        "pola",
        "przełącz fokus między akcjami a polami",
    ),
    bind(
        Context::Panel,
        Key::code(KeyCode::Enter),
        Action::Submit,
        "wyślij",
        "wyślij; gdy akcja ma wymagane pola — przejdź do pierwszego z nich",
    ),
    bind(
        Context::Panel,
        Key::ctrl('j'),
        Action::Newline,
        "↵",
        "nowa linia w polu tekstowym (także Shift+Enter i Alt+Enter, gdy terminal je rozróżnia)",
    ),
    bind(
        Context::Panel,
        Key::code(KeyCode::Esc),
        Action::Cancel,
        "zwiń",
        "zwiń panel; szkic zostaje",
    ),
    bind(
        Context::Panel,
        Key::char('j'),
        Action::Down,
        "",
        "następna akcja",
    ),
    bind(
        Context::Panel,
        Key::code(KeyCode::Down),
        Action::Down,
        "",
        "następna akcja (w polach: następne pole)",
    ),
    bind(
        Context::Panel,
        Key::char('k'),
        Action::Up,
        "",
        "poprzednia akcja",
    ),
    bind(
        Context::Panel,
        Key::code(KeyCode::Up),
        Action::Up,
        "",
        "poprzednia akcja (w polach: poprzednie pole)",
    ),
    bind(
        Context::Panel,
        Key::Digit,
        Action::Pick,
        "",
        "wybierz akcję cyfrą",
    ),
    bind(
        Context::Panel,
        Key::code(KeyCode::PageDown),
        Action::PageDown,
        "",
        "przewiń opis w dół",
    ),
    bind(
        Context::Panel,
        Key::code(KeyCode::PageUp),
        Action::PageUp,
        "",
        "przewiń opis w górę",
    ),
    bind(
        Context::Panel,
        Key::char('['),
        Action::PrevPending,
        "",
        "poprzednie oczekiwanie zadania",
    ),
    bind(
        Context::Panel,
        Key::char(']'),
        Action::NextPending,
        "",
        "następne oczekiwanie zadania",
    ),
    bind(
        Context::Panel,
        Key::char('g'),
        Action::Top,
        "",
        "przewiń kontekst oczekiwania na początek",
    ),
    bind(
        Context::Panel,
        Key::char('G'),
        Action::Bottom,
        "",
        "przewiń kontekst oczekiwania na koniec",
    ),
    // The action menu.
    bind(
        Context::Menu,
        Key::char('j'),
        Action::Down,
        "wybór",
        "następna pozycja",
    ),
    bind(
        Context::Menu,
        Key::code(KeyCode::Down),
        Action::Down,
        "",
        "następna pozycja",
    ),
    bind(
        Context::Menu,
        Key::char('k'),
        Action::Up,
        "",
        "poprzednia pozycja",
    ),
    bind(
        Context::Menu,
        Key::code(KeyCode::Up),
        Action::Up,
        "",
        "poprzednia pozycja",
    ),
    bind(
        Context::Menu,
        Key::Digit,
        Action::Pick,
        "",
        "wybierz pozycję cyfrą",
    ),
    bind(
        Context::Menu,
        Key::code(KeyCode::Enter),
        Action::Confirm,
        "wykonaj",
        "wykonaj wybraną akcję",
    ),
    bind(
        Context::Menu,
        Key::code(KeyCode::Esc),
        Action::Cancel,
        "zamknij",
        "zamknij menu",
    ),
    bind(
        Context::Menu,
        Key::char('K'),
        Action::MoveUp,
        "",
        "przesuń zadanie w kolejce o jedno miejsce wyżej",
    ),
    bind(
        Context::Menu,
        Key::char('J'),
        Action::MoveDown,
        "",
        "przesuń zadanie w kolejce o jedno miejsce niżej",
    ),
    bind(
        Context::Menu,
        Key::char('m'),
        Action::MoveMode,
        "",
        "tryb przesuwania: wybierz miejsce w kolejce",
    ),
    bind(
        Context::Menu,
        Key::char('p'),
        Action::PauseResume,
        "",
        "wstrzymaj albo wznów zadanie",
    ),
    bind(
        Context::Menu,
        Key::char('x'),
        Action::Interrupt,
        "",
        "przerwij bieżący krok (wymaga potwierdzenia)",
    ),
    bind(
        Context::Menu,
        Key::char('t'),
        Action::Transition,
        "",
        "przejście ręczne do wybranego stanu",
    ),
    bind(
        Context::Menu,
        Key::char('s'),
        Action::Restart,
        "",
        "uruchom zadanie od nowa",
    ),
    bind(
        Context::Menu,
        Key::char('a'),
        Action::Archive,
        "",
        "archiwizuj zadanie (wymaga potwierdzenia)",
    ),
    bind(
        Context::Menu,
        Key::char('o'),
        Action::OpenLink,
        "",
        "otwórz link zadania w przeglądarce",
    ),
    // The confirmation window of an action `!`: the same for all of them.
    bind(
        Context::Confirm,
        Key::char('y'),
        Action::Accept,
        "potwierdź",
        "potwierdź akcję pokazaną w oknie",
    ),
    bind(
        Context::Confirm,
        Key::char('n'),
        Action::Cancel,
        "anuluj",
        "nie rób nic i wróć",
    ),
    bind(
        Context::Confirm,
        Key::code(KeyCode::Esc),
        Action::Cancel,
        "",
        "nie rób nic i wróć",
    ),
    // A form: new task, repositories.
    bind(
        Context::Form,
        Key::code(KeyCode::Tab),
        Action::NextField,
        "pola",
        "następne pole",
    ),
    bind(
        Context::Form,
        Key::code(KeyCode::Down),
        Action::NextField,
        "",
        "następne pole",
    ),
    bind(
        Context::Form,
        Key::code(KeyCode::BackTab),
        Action::PrevField,
        "",
        "poprzednie pole",
    ),
    bind(
        Context::Form,
        Key::code(KeyCode::Up),
        Action::PrevField,
        "",
        "poprzednie pole",
    ),
    bind(
        Context::Form,
        Key::code(KeyCode::Left),
        Action::Step,
        "",
        "poprzednia opcja albo kursor w lewo",
    ),
    bind(
        Context::Form,
        Key::code(KeyCode::Right),
        Action::Step,
        "",
        "następna opcja albo kursor w prawo",
    ),
    bind(
        Context::Form,
        Key::code(KeyCode::Enter),
        Action::Submit,
        "wyślij",
        "wyślij formularz",
    ),
    bind(
        Context::Form,
        Key::ctrl('j'),
        Action::Newline,
        "↵",
        "nowa linia w polu tekstowym (także Shift+Enter i Alt+Enter, gdy terminal je rozróżnia)",
    ),
    bind(
        Context::Form,
        Key::code(KeyCode::Esc),
        Action::Cancel,
        "wstecz",
        "wróć z pola formularza nowego zadania do listy albo zamknij formularz; wpisane dane zostają",
    ),
    // Moving a queued task.
    bind(
        Context::Move,
        Key::char('j'),
        Action::Down,
        "miejsce",
        "miejsce niżej, z podglądem",
    ),
    bind(
        Context::Move,
        Key::code(KeyCode::Down),
        Action::Down,
        "",
        "miejsce niżej, z podglądem",
    ),
    bind(
        Context::Move,
        Key::char('k'),
        Action::Up,
        "",
        "miejsce wyżej, z podglądem",
    ),
    bind(
        Context::Move,
        Key::code(KeyCode::Up),
        Action::Up,
        "",
        "miejsce wyżej, z podglądem",
    ),
    bind(
        Context::Move,
        Key::code(KeyCode::Enter),
        Action::Confirm,
        "zatwierdź",
        "zatwierdź nowe miejsce (move --before/--after)",
    ),
    bind(
        Context::Move,
        Key::code(KeyCode::Esc),
        Action::Cancel,
        "anuluj",
        "anuluj przesuwanie",
    ),
    // The history: the list, then the details of an entry (read only).
    bind(
        Context::History,
        Key::code(KeyCode::Esc),
        Action::Back,
        "zadania",
        "wróć do listy zadań",
    ),
    bind(
        Context::History,
        Key::char('j'),
        Action::Down,
        "wybór",
        "kursor w dół",
    ),
    bind(
        Context::History,
        Key::code(KeyCode::Down),
        Action::Down,
        "",
        "kursor w dół",
    ),
    bind(
        Context::History,
        Key::char('k'),
        Action::Up,
        "",
        "kursor w górę",
    ),
    bind(
        Context::History,
        Key::code(KeyCode::Up),
        Action::Up,
        "",
        "kursor w górę",
    ),
    bind(
        Context::History,
        Key::char('g'),
        Action::Top,
        "",
        "na początek listy",
    ),
    bind(
        Context::History,
        Key::char('G'),
        Action::Bottom,
        "",
        "na koniec listy",
    ),
    bind(
        Context::History,
        Key::code(KeyCode::Enter),
        Action::Open,
        "szczegóły",
        "przejdź do szczegółów zaznaczonego wpisu",
    ),
    bind(
        Context::History,
        Key::char('w'),
        Action::Recover,
        "wznów",
        "wznów stare execution, które na to pozwala (recover)",
    ),
    bind(
        Context::History,
        Key::code(KeyCode::Left),
        Action::PrevTab,
        "",
        "poprzednia zakładka",
    ),
    bind(
        Context::History,
        Key::code(KeyCode::Right),
        Action::NextTab,
        "",
        "następna zakładka",
    ),
    bind(
        Context::History,
        Key::char('h'),
        Action::ChooseHarness,
        "",
        CHOOSE_HARNESS,
    ),
    bind(
        Context::HistoryDetails,
        Key::code(KeyCode::Esc),
        Action::Back,
        "lista",
        "wróć na listę historii",
    ),
    bind(
        Context::HistoryDetails,
        Key::code(KeyCode::Left),
        Action::PrevTab,
        "zakładki",
        "poprzednia zakładka",
    ),
    bind(
        Context::HistoryDetails,
        Key::code(KeyCode::Right),
        Action::NextTab,
        "",
        "następna zakładka",
    ),
    bind(
        Context::HistoryDetails,
        Key::char('j'),
        Action::Down,
        "przewijanie",
        "przewiń w dół",
    ),
    bind(
        Context::HistoryDetails,
        Key::code(KeyCode::Down),
        Action::Down,
        "",
        "przewiń w dół",
    ),
    bind(
        Context::HistoryDetails,
        Key::char('k'),
        Action::Up,
        "",
        "przewiń w górę",
    ),
    bind(
        Context::HistoryDetails,
        Key::code(KeyCode::Up),
        Action::Up,
        "",
        "przewiń w górę",
    ),
    bind(
        Context::HistoryDetails,
        Key::code(KeyCode::PageDown),
        Action::PageDown,
        "",
        "strona w dół",
    ),
    bind(
        Context::HistoryDetails,
        Key::code(KeyCode::PageUp),
        Action::PageUp,
        "",
        "strona w górę",
    ),
    bind(
        Context::HistoryDetails,
        Key::char('g'),
        Action::Top,
        "",
        "na początek",
    ),
    bind(
        Context::HistoryDetails,
        Key::char('G'),
        Action::Bottom,
        "",
        "na koniec",
    ),
    bind(
        Context::HistoryDetails,
        Key::code(KeyCode::Enter),
        Action::Expand,
        "rozwiń",
        "rozwiń albo zwiń wpis",
    ),
    bind(
        Context::HistoryDetails,
        Key::char(' '),
        Action::Expand,
        "",
        "rozwiń albo zwiń wpis",
    ),
    bind(
        Context::HistoryDetails,
        Key::char('v'),
        Action::RawEntry,
        "",
        "widok surowy wpisu",
    ),
    bind(
        Context::HistoryDetails,
        Key::ctrl('v'),
        Action::RawAll,
        "",
        "widok surowy całości",
    ),
    bind(
        Context::HistoryDetails,
        Key::char('w'),
        Action::Recover,
        "wznów",
        "wznów stare execution, które na to pozwala (recover)",
    ),
    bind(
        Context::HistoryDetails,
        Key::char('h'),
        Action::ChooseHarness,
        "",
        CHOOSE_HARNESS,
    ),
    // The repositories: a list and a description, `a` adds, `d` removes. The
    // list itself is the chooser, which reads the panel's keys for moving.
    bind(
        Context::Repositories,
        Key::code(KeyCode::Esc),
        Action::Back,
        "zadania",
        "wróć do listy zadań",
    ),
    bind(
        Context::Repositories,
        Key::char('a'),
        Action::AddRepository,
        "dodaj",
        "dodaj repozytorium",
    ),
    bind(
        Context::Repositories,
        Key::char('d'),
        Action::RemoveRepository,
        "usuń",
        "usuń zaznaczone repozytorium (wymaga potwierdzenia)",
    ),
    bind(
        Context::Repositories,
        Key::char('j'),
        Action::Down,
        "wybór",
        "następne repozytorium",
    ),
    bind(
        Context::Repositories,
        Key::code(KeyCode::Down),
        Action::Down,
        "",
        "następne repozytorium",
    ),
    bind(
        Context::Repositories,
        Key::char('k'),
        Action::Up,
        "",
        "poprzednie repozytorium",
    ),
    bind(
        Context::Repositories,
        Key::code(KeyCode::Up),
        Action::Up,
        "",
        "poprzednie repozytorium",
    ),
    bind(
        Context::Repositories,
        Key::Digit,
        Action::Pick,
        "",
        "wybierz repozytorium cyfrą",
    ),
    bind(
        Context::Repositories,
        Key::code(KeyCode::PageDown),
        Action::PageDown,
        "",
        "przewiń opis w dół",
    ),
    bind(
        Context::Repositories,
        Key::code(KeyCode::PageUp),
        Action::PageUp,
        "",
        "przewiń opis w górę",
    ),
    bind(
        Context::Repositories,
        Key::char('h'),
        Action::ChooseHarness,
        "",
        CHOOSE_HARNESS,
    ),
];

/// Whether the terminal tells `Shift+Enter` and `Alt+Enter` from `Enter`
/// (the keyboard enhancement protocol). Set once, when the terminal is taken
/// over; it only decides which keys the bar and help promise.
static MODIFIERS_DISTINGUISHED: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// A thread's own answer to [`modifiers_distinguished`], so that tests
    /// can run side by side without sharing the process flag.
    static MODIFIERS_OVERRIDE: Cell<Option<bool>> = const { Cell::new(None) };
}

/// Records whether the terminal reports modifiers on `Enter`.
pub fn set_modifiers_distinguished(distinguished: bool) {
    MODIFIERS_DISTINGUISHED.store(distinguished, Ordering::SeqCst);
}

/// Whether the terminal reports modifiers on `Enter`; what the bar and help
/// may promise.
pub fn modifiers_distinguished() -> bool {
    MODIFIERS_OVERRIDE
        .with(Cell::get)
        .unwrap_or_else(|| MODIFIERS_DISTINGUISHED.load(Ordering::SeqCst))
}

/// Overrides [`modifiers_distinguished`] on the calling thread only (`None`
/// takes the override away). For tests.
pub fn override_modifiers_distinguished(distinguished: Option<bool>) {
    MODIFIERS_OVERRIDE.with(|cell| cell.set(distinguished));
}

/// The one rule of the new line: `Ctrl+J` always; `Enter` carrying Shift or
/// Alt, which only a terminal that reports modifiers delivers (any other
/// sends a plain `Enter`, and that sends). Decided on the raw event, because
/// [`Key::matches`] does not see Shift.
pub fn is_newline(event: &KeyEvent) -> bool {
    match event.code {
        KeyCode::Char('j') => (event.modifiers - KeyModifiers::SHIFT) == KeyModifiers::CONTROL,
        KeyCode::Enter => {
            event
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
                && !event.modifiers.contains(KeyModifiers::CONTROL)
        }
        _ => false,
    }
}

/// The keys of the new line that work in this terminal, as written.
pub fn newline_keys() -> Vec<&'static str> {
    if modifiers_distinguished() {
        vec!["Ctrl+J", "Shift+Enter", "Alt+Enter"]
    } else {
        vec!["Ctrl+J"]
    }
}

/// What `event` does in `context`: the context's own keys first, then the
/// global ones where the context inherits them.
pub fn lookup(context: Context, event: &KeyEvent) -> Option<Action> {
    lookup_in(BINDINGS, context, event)
}

/// [`lookup`] over an arbitrary table.
pub fn lookup_in(bindings: &[Binding], context: Context, event: &KeyEvent) -> Option<Action> {
    if matches!(context, Context::Panel | Context::Form) && is_newline(event) {
        return Some(Action::Newline);
    }
    let own = bindings
        .iter()
        .find(|binding| binding.context == context && binding.key.matches(event));
    let inherited = || {
        context
            .inherits_global()
            .then(|| {
                bindings.iter().find(|binding| {
                    binding.context == Context::Global && binding.key.matches(event)
                })
            })
            .flatten()
    };
    own.or_else(inherited).map(|binding| binding.action)
}

/// The first key that does `action` in `context`, as it is written in the
/// bar and in help; for text that names a key.
pub fn key_of(context: Context, action: Action) -> Option<String> {
    let binding = BINDINGS
        .iter()
        .find(|binding| binding.context == context && binding.action == action)?;
    Some(if action == Action::Newline {
        newline_keys().join("/")
    } else {
        binding.key.display()
    })
}

/// Why a table is not consistent: one message per problem.
pub fn check_consistency(bindings: &[Binding]) -> Result<(), Vec<String>> {
    let mut problems = Vec::new();
    for (index, binding) in bindings.iter().enumerate() {
        let label = binding.key.display();
        if binding.description.trim().is_empty() {
            problems.push(format!(
                "({}, {label}) has no description",
                binding.context.name()
            ));
        }
        for other in &bindings[..index] {
            if other.key != binding.key {
                continue;
            }
            if other.context == binding.context {
                problems.push(format!(
                    "({}, {label}) is bound twice: {:?} and {:?}",
                    binding.context.name(),
                    other.action,
                    binding.action
                ));
            } else if [other.context, binding.context].contains(&Context::Global) {
                let shadowing = if other.context == Context::Global {
                    binding.context
                } else {
                    other.context
                };
                if shadowing.inherits_global() {
                    problems.push(format!(
                        "({}, {label}) shadows the global {label}",
                        shadowing.name()
                    ));
                }
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// One line of help: the keys that do one thing, and what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpEntry {
    /// The context the keys belong to.
    pub context: Context,
    /// The keys, as written (`j/k`, `Ctrl+J`).
    pub keys: String,
    /// The label for the bar; empty when the row is help-only.
    pub label: &'static str,
    /// What it does.
    pub description: &'static str,
}

/// The help for `context`: its own keys, then the global ones it inherits.
/// Keys that do the same thing are written on one line.
pub fn help_entries(context: Context) -> Vec<HelpEntry> {
    help_entries_in(BINDINGS, context)
}

/// [`help_entries`] over an arbitrary table.
pub fn help_entries_in(bindings: &[Binding], context: Context) -> Vec<HelpEntry> {
    let mut contexts = vec![context];
    if context.inherits_global() {
        contexts.push(Context::Global);
    }
    let mut entries: Vec<(Context, Action, Vec<String>, &Binding)> = Vec::new();
    for scope in contexts {
        for binding in bindings.iter().filter(|binding| binding.context == scope) {
            let key = binding.key.display();
            match entries.iter_mut().find(|(entry_scope, action, ..)| {
                *entry_scope == scope && *action == binding.action
            }) {
                Some((_, _, keys, _)) => keys.push(key),
                None => entries.push((scope, binding.action, vec![key], binding)),
            }
        }
    }
    entries
        .into_iter()
        .map(|(context, action, keys, binding)| HelpEntry {
            context,
            keys: if action == Action::Newline {
                newline_keys().join("/")
            } else {
                keys.join("/")
            },
            label: binding.label,
            description: binding.description,
        })
        .collect()
}

/// The shortcut bar for `context`: `key label` pairs, in table order, only
/// the rows that carry a label.
pub fn hints(context: Context) -> Vec<(String, &'static str)> {
    help_entries(context)
        .into_iter()
        .filter(|entry| !entry.label.is_empty())
        .map(|entry| (entry.keys, entry.label))
        .collect()
}

/// The shortcut bar as one line that fits `width` columns: the keys on the
/// focus accent, the labels plain. Hints that do not fit are dropped from
/// the end, so the first ones — the ones that matter — stay.
pub fn hint_line(context: Context, styles: &Styles, width: usize) -> Line<'static> {
    let mut spans = Vec::new();
    let mut used = 0;
    for (keys, label) in hints(context) {
        let needed = keys.chars().count() + label.chars().count() + 3;
        if used + needed > width {
            break;
        }
        used += needed;
        spans.push(Span::styled(format!(" {keys} "), styles.key_cap()));
        spans.push(Span::raw(format!("{label} ")));
    }
    Line::from(spans)
}

/// Draws the `?` help for `context` as a bordered overlay over `area`.
pub fn render_help(frame: &mut Frame, area: Rect, context: Context, styles: &Styles) {
    let entries = help_entries(context);
    let keys_width = entries
        .iter()
        .map(|entry| entry.keys.chars().count())
        .max()
        .unwrap_or(0);
    let mut lines = Vec::new();
    let mut scope = None;
    for entry in &entries {
        if scope != Some(entry.context) {
            scope = Some(entry.context);
            lines.push(Line::styled(
                entry.context.name().to_string(),
                styles.of(Role::Focus),
            ));
        }
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {:<keys_width$}  ", entry.keys),
                styles.of(Role::Focus),
            ),
            Span::raw(entry.description),
        ]));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(styles.of(Role::Focus))
        .title(" Pomoc ");
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}
