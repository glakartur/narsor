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
//! # Layers
//!
//! The details (the task's and the history's) keep only what works on every
//! tab. The keys of what a tab shows are in a layer over them: the timeline
//! of *Przebieg* ([`Context::Axis`], [`Context::HistoryAxis`]), the
//! conversation of a step ([`Context::StepConversation`]) or reading any
//! other tab ([`Context::Reading`]). The keys in force are a [`Stack`] —
//! layer, base, then [`Context::Global`] — and [`lookup`], the bar and help
//! all read it in that order. A plain [`Context`] is a stack without a layer.
//!
//! # Consistency
//!
//! [`check_consistency`] is what keeps the table honest: over every stack the
//! interface can be in ([`Stack::all`]), no key twice in one context, no key
//! that a layer would shadow from its base or a context from
//! [`Context::Global`], no row without a description. A test runs it over
//! [`BINDINGS`], and a second one proves it fails when a row is duplicated.

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
    /// The `Enter` / `Esc` window a form shows before it sends.
    FormConfirm,
    /// A queued task is being moved.
    Move,
    /// The history's list has the focus.
    History,
    /// The details of an entry of the history have the focus (read only).
    HistoryDetails,
    /// The list of repositories has the focus.
    Repositories,
    /// A layer over [`Context::Details`]: the step timeline of *Przebieg*.
    Axis,
    /// A layer over [`Context::HistoryDetails`]: the timeline of a closed
    /// run, which has nothing to follow.
    HistoryAxis,
    /// A layer over either details: the conversation of the step picked on
    /// the timeline.
    StepConversation,
    /// A layer over either details: reading any tab but *Przebieg*.
    Reading,
    /// A layer over either details: the task-wide agent conversation.
    AgentConversation,
}

impl Context {
    /// Every context, in the order help and tests walk them.
    pub const ALL: [Context; 17] = [
        Context::Global,
        Context::List,
        Context::Details,
        Context::Panel,
        Context::Menu,
        Context::Confirm,
        Context::Form,
        Context::FormConfirm,
        Context::Move,
        Context::History,
        Context::HistoryDetails,
        Context::Repositories,
        Context::Axis,
        Context::HistoryAxis,
        Context::StepConversation,
        Context::Reading,
        Context::AgentConversation,
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
            Self::FormConfirm => "potwierdzenie formularza",
            Self::Move => "przesuwanie",
            Self::History => "historia",
            Self::HistoryDetails => "historia: szczegóły",
            Self::Repositories => "repozytoria",
            Self::Axis | Self::HistoryAxis => "oś",
            Self::StepConversation => "rozmowa kroku",
            Self::Reading => "czytanie",
            Self::AgentConversation => "rozmowa",
        }
    }

    /// Whether the context is a layer, whose keys only ever apply over a
    /// base ([`Stack`]).
    pub fn is_layer(self) -> bool {
        matches!(
            self,
            Self::Axis
                | Self::HistoryAxis
                | Self::StepConversation
                | Self::Reading
                | Self::AgentConversation
        )
    }

    /// Whether the global keys also work here. False where typed text must
    /// not be intercepted.
    pub fn inherits_global(self) -> bool {
        !matches!(self, Self::Global | Self::Form | Self::FormConfirm)
    }
}

/// The contexts whose keys apply at once: an optional layer, its base, then
/// [`Context::Global`] where the base inherits it. A key is looked up in
/// that order, and help and the bar list them in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Stack {
    /// The keys of what the details show, over the base.
    pub layer: Option<Context>,
    /// Where the operator is.
    pub base: Context,
}

impl Stack {
    /// The stacks with a layer, each layer over the bases it goes with.
    pub const LAYERED: [Stack; 8] = [
        Stack::layered(Context::Axis, Context::Details),
        Stack::layered(Context::StepConversation, Context::Details),
        Stack::layered(Context::Reading, Context::Details),
        Stack::layered(Context::HistoryAxis, Context::HistoryDetails),
        Stack::layered(Context::StepConversation, Context::HistoryDetails),
        Stack::layered(Context::Reading, Context::HistoryDetails),
        Stack::layered(Context::AgentConversation, Context::Details),
        Stack::layered(Context::AgentConversation, Context::HistoryDetails),
    ];

    /// `layer` over `base`.
    pub const fn layered(layer: Context, base: Context) -> Self {
        Self {
            layer: Some(layer),
            base,
        }
    }

    /// Every stack the interface can be in: each context that is not a
    /// layer on its own, then [`Stack::LAYERED`].
    pub fn all() -> impl Iterator<Item = Stack> {
        Context::ALL
            .into_iter()
            .filter(|context| !context.is_layer())
            .map(Stack::from)
            .chain(Self::LAYERED)
    }

    /// The contexts to look in, first to last.
    pub fn scopes(self) -> impl Iterator<Item = Context> {
        let global = self.base.inherits_global().then_some(Context::Global);
        let reading = (self.layer == Some(Context::AgentConversation)).then_some(Context::Reading);
        [self.layer, reading, Some(self.base), global]
            .into_iter()
            .flatten()
    }
}

impl From<Context> for Stack {
    fn from(base: Context) -> Self {
        Self { layer: None, base }
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
    /// Search the task list.
    Filter,
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
    /// Advance an agent conversation through Conversation, Chronology and RAW.
    NextConversationView,
    /// Follow the step that is running; in a reader, to the end and after
    /// what comes.
    Follow,
    /// Enter the conversation of the step picked on the timeline.
    EnterStep,
    /// Leave the step's conversation for the timeline, on the same step.
    LeaveStep,
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
    /// Comment to the agent.
    Comment,
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
    /// Change the limit of concurrent tasks of the selected task's backend.
    ChooseLimit,
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

/// What `L` does, in every context that has it.
const CHOOSE_LIMIT: &str =
    "limit zadań równoległych backendu zaznaczonego zadania: o 1 więcej, o 1 mniej albo wartość z pliku";

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
        "wyjście z narsora (wymaga ponownego naciśnięcia q)",
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
        Key::code(KeyCode::Tab),
        Action::SwitchFocus,
        "szczegóły",
        "przełącz fokus z listy do sąsiedniego widoku",
    ),
    bind(
        Context::List,
        Key::code(KeyCode::BackTab),
        Action::SwitchFocus,
        "szczegóły",
        "przełącz fokus z listy do sąsiedniego widoku",
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
        Key::char('/'),
        Action::Filter,
        "",
        "filtruj listę po tickecie, tytule lub identyfikatorze",
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
        Key::char('L'),
        Action::ChooseLimit,
        "limit",
        CHOOSE_LIMIT,
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
        Key::char('c'),
        Action::Comment,
        "",
        "napisz do agenta (serwer wybiera skutek według stanu zadania)",
    ),
    bind(
        Context::List,
        Key::char('r'),
        Action::RetrySend,
        "",
        "ponów wysłanie nowego zadania, które nie dotarło do serwera",
    ),
    // The task details: what works on every tab. What reads the tab
    // itself is in a layer over these (below the history's).
    bind(
        Context::Details,
        Key::char('['),
        Action::PrevTab,
        "zakładki",
        "poprzednia zakładka",
    ),
    bind(
        Context::Details,
        Key::char(']'),
        Action::NextTab,
        "",
        "następna zakładka",
    ),
    bind(
        Context::Details,
        Key::code(KeyCode::Tab),
        Action::SwitchFocus,
        "panel",
        "przełącz fokus między treścią szczegółów a panelem odpowiedzi",
    ),
    bind(
        Context::Details,
        Key::code(KeyCode::BackTab),
        Action::SwitchFocus,
        "panel",
        "przełącz fokus między treścią szczegółów a panelem odpowiedzi",
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
    bind(
        Context::Details,
        Key::char('L'),
        Action::ChooseLimit,
        "",
        CHOOSE_LIMIT,
    ),
    // The "waiting for you" panel, focus on its actions. Focus in a field
    // types text; the chooser handles those keys.
    bind(
        Context::Panel,
        Key::code(KeyCode::Tab),
        Action::SwitchFocus,
        "szczegóły",
        "wróć fokusem do treści szczegółów; panel pozostaje widoczny",
    ),
    bind(
        Context::Panel,
        Key::code(KeyCode::BackTab),
        Action::SwitchFocus,
        "szczegóły",
        "wróć fokusem do treści szczegółów; panel pozostaje widoczny",
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
        "wstecz",
        "formularze z serwera: następny formularz, także w polu; bez nich z pola wróć do akcji, z akcji zwiń panel, szkic zostaje",
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
        "poprzednie oczekiwanie zadania (panel bez formularzy z serwera)",
    ),
    bind(
        Context::Panel,
        Key::char(']'),
        Action::NextPending,
        "",
        "następne oczekiwanie zadania (panel bez formularzy z serwera)",
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
    bind(
        Context::Menu,
        Key::char('c'),
        Action::Comment,
        "",
        "napisz do agenta (serwer wybiera skutek według stanu zadania)",
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
    // The window a form shows before it sends: only these keys act.
    bind(
        Context::FormConfirm,
        Key::code(KeyCode::Enter),
        Action::Accept,
        "wyślij",
        "zatwierdź formularz i wyślij go",
    ),
    bind(
        Context::FormConfirm,
        Key::code(KeyCode::Esc),
        Action::Cancel,
        "wróć",
        "wróć do formularza bez wysyłania; wpisane wartości zostają",
    ),
    // A form: new task, repositories.
    bind(
        Context::Form,
        Key::code(KeyCode::Tab),
        Action::SwitchFocus,
        "panel",
        "przełącz fokus między formularzem a sąsiednim panelem",
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
        Action::SwitchFocus,
        "panel",
        "przełącz fokus między formularzem a sąsiednim panelem",
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
        Key::code(KeyCode::Tab),
        Action::SwitchFocus,
        "szczegóły",
        "przełącz fokus między listą historii a jej szczegółami",
    ),
    bind(
        Context::History,
        Key::code(KeyCode::BackTab),
        Action::SwitchFocus,
        "szczegóły",
        "przełącz fokus między listą historii a jej szczegółami",
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
        Key::char('['),
        Action::PrevTab,
        "",
        "poprzednia zakładka",
    ),
    bind(
        Context::History,
        Key::char(']'),
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
        Key::char('['),
        Action::PrevTab,
        "zakładki",
        "poprzednia zakładka",
    ),
    bind(
        Context::HistoryDetails,
        Key::code(KeyCode::Tab),
        Action::SwitchFocus,
        "lista",
        "przełącz fokus między listą historii a jej szczegółami",
    ),
    bind(
        Context::HistoryDetails,
        Key::code(KeyCode::BackTab),
        Action::SwitchFocus,
        "lista",
        "przełącz fokus między listą historii a jej szczegółami",
    ),
    bind(
        Context::HistoryDetails,
        Key::char(']'),
        Action::NextTab,
        "",
        "następna zakładka",
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
    // The layers over the details, the task's and the history's: the keys of
    // what the tab shows. They change nothing on the server, so nothing of
    // the task's menu leaks into the read-only history. Each has its own
    // `Esc`, first: on a narrow screen the details are full screen and the way
    // back must survive a shortened bar, which drops hints from the end.
    //
    // The timeline of *Przebieg* in the task details.
    bind(
        Context::Axis,
        Key::code(KeyCode::Esc),
        Action::Back,
        "lista",
        "wróć na listę z zachowanym zaznaczeniem",
    ),
    bind(
        Context::Axis,
        Key::char('j'),
        Action::Down,
        "kroki",
        "następny krok",
    ),
    bind(
        Context::Axis,
        Key::code(KeyCode::Down),
        Action::Down,
        "",
        "następny krok",
    ),
    bind(
        Context::Axis,
        Key::char('k'),
        Action::Up,
        "",
        "poprzedni krok",
    ),
    bind(
        Context::Axis,
        Key::code(KeyCode::Up),
        Action::Up,
        "",
        "poprzedni krok",
    ),
    bind(
        Context::Axis,
        Key::char('g'),
        Action::Top,
        "",
        "pierwszy krok",
    ),
    bind(
        Context::Axis,
        Key::char('G'),
        Action::Bottom,
        "",
        "ostatni krok",
    ),
    bind(
        Context::Axis,
        Key::code(KeyCode::Enter),
        Action::EnterStep,
        "rozmowa kroku",
        "wejdź do rozmowy wybranego kroku",
    ),
    bind(
        Context::Axis,
        Key::code(KeyCode::Right),
        Action::EnterStep,
        "",
        "wejdź do rozmowy wybranego kroku",
    ),
    bind(
        Context::Axis,
        Key::char('f'),
        Action::Follow,
        "podążaj",
        "włącz albo wyłącz podążanie za bieżącym krokiem",
    ),
    // The timeline of *Przebieg* in the history: a closed run, nothing to follow.
    bind(
        Context::HistoryAxis,
        Key::code(KeyCode::Esc),
        Action::Back,
        "lista",
        "wróć na listę historii",
    ),
    bind(
        Context::HistoryAxis,
        Key::char('j'),
        Action::Down,
        "kroki",
        "następny krok",
    ),
    bind(
        Context::HistoryAxis,
        Key::code(KeyCode::Down),
        Action::Down,
        "",
        "następny krok",
    ),
    bind(
        Context::HistoryAxis,
        Key::char('k'),
        Action::Up,
        "",
        "poprzedni krok",
    ),
    bind(
        Context::HistoryAxis,
        Key::code(KeyCode::Up),
        Action::Up,
        "",
        "poprzedni krok",
    ),
    bind(
        Context::HistoryAxis,
        Key::char('g'),
        Action::Top,
        "",
        "pierwszy krok",
    ),
    bind(
        Context::HistoryAxis,
        Key::char('G'),
        Action::Bottom,
        "",
        "ostatni krok",
    ),
    bind(
        Context::HistoryAxis,
        Key::code(KeyCode::Enter),
        Action::EnterStep,
        "rozmowa kroku",
        "wejdź do rozmowy wybranego kroku",
    ),
    bind(
        Context::HistoryAxis,
        Key::code(KeyCode::Right),
        Action::EnterStep,
        "",
        "wejdź do rozmowy wybranego kroku",
    ),
    // The conversation of the step picked on the timeline.
    bind(
        Context::StepConversation,
        Key::code(KeyCode::Esc),
        Action::LeaveStep,
        "oś",
        "wróć na oś, do tego samego kroku",
    ),
    bind(
        Context::StepConversation,
        Key::code(KeyCode::Left),
        Action::LeaveStep,
        "",
        "wróć na oś, do tego samego kroku",
    ),
    bind(
        Context::StepConversation,
        Key::char('j'),
        Action::Down,
        "pozycja",
        "następna pozycja",
    ),
    bind(
        Context::StepConversation,
        Key::code(KeyCode::Down),
        Action::Down,
        "",
        "następna pozycja",
    ),
    bind(
        Context::StepConversation,
        Key::char('k'),
        Action::Up,
        "",
        "poprzednia pozycja",
    ),
    bind(
        Context::StepConversation,
        Key::code(KeyCode::Up),
        Action::Up,
        "",
        "poprzednia pozycja",
    ),
    bind(
        Context::StepConversation,
        Key::code(KeyCode::PageDown),
        Action::PageDown,
        "",
        "strona w dół",
    ),
    bind(
        Context::StepConversation,
        Key::code(KeyCode::PageUp),
        Action::PageUp,
        "",
        "strona w górę",
    ),
    bind(
        Context::StepConversation,
        Key::char('g'),
        Action::Top,
        "",
        "na początek",
    ),
    bind(
        Context::StepConversation,
        Key::char('G'),
        Action::Bottom,
        "",
        "na koniec",
    ),
    bind(
        Context::StepConversation,
        Key::code(KeyCode::Enter),
        Action::Expand,
        "rozwiń",
        "rozwiń albo zwiń wpis",
    ),
    bind(
        Context::StepConversation,
        Key::char(' '),
        Action::Expand,
        "",
        "rozwiń albo zwiń wpis",
    ),
    bind(
        Context::StepConversation,
        Key::char('v'),
        Action::NextConversationView,
        "widok",
        "następny widok rozmowy",
    ),
    bind(
        Context::StepConversation,
        Key::char('r'),
        Action::RawEntry,
        "źródło",
        "pokaż albo ukryj źródło wpisu",
    ),
    // The complete agent conversation reuses reading navigation through its
    // stack, while its view-specific keys do not alter generic readers.
    bind(
        Context::AgentConversation,
        Key::char('v'),
        Action::NextConversationView,
        "widok",
        "następny widok rozmowy",
    ),
    bind(
        Context::AgentConversation,
        Key::char('r'),
        Action::RawEntry,
        "źródło",
        "pokaż albo ukryj źródło wpisu",
    ),
    // Every other tab: reading what it shows.
    bind(
        Context::Reading,
        Key::code(KeyCode::Esc),
        Action::Back,
        "lista",
        "wróć na listę",
    ),
    bind(
        Context::Reading,
        Key::char('j'),
        Action::Down,
        "pozycja",
        "następna pozycja",
    ),
    bind(
        Context::Reading,
        Key::code(KeyCode::Down),
        Action::Down,
        "",
        "następna pozycja",
    ),
    bind(
        Context::Reading,
        Key::char('k'),
        Action::Up,
        "",
        "poprzednia pozycja",
    ),
    bind(
        Context::Reading,
        Key::code(KeyCode::Up),
        Action::Up,
        "",
        "poprzednia pozycja",
    ),
    bind(
        Context::Reading,
        Key::code(KeyCode::PageDown),
        Action::PageDown,
        "",
        "strona w dół",
    ),
    bind(
        Context::Reading,
        Key::code(KeyCode::PageUp),
        Action::PageUp,
        "",
        "strona w górę",
    ),
    bind(
        Context::Reading,
        Key::char('g'),
        Action::Top,
        "",
        "na początek",
    ),
    bind(
        Context::Reading,
        Key::char('G'),
        Action::Bottom,
        "",
        "na koniec",
    ),
    bind(
        Context::Reading,
        Key::code(KeyCode::Enter),
        Action::Expand,
        "rozwiń",
        "rozwiń albo zwiń wpis",
    ),
    bind(
        Context::Reading,
        Key::char(' '),
        Action::Expand,
        "",
        "rozwiń albo zwiń wpis",
    ),
    bind(
        Context::Reading,
        Key::char('v'),
        Action::RawEntry,
        "",
        "widok surowy wpisu",
    ),
    bind(
        Context::Reading,
        Key::ctrl('v'),
        Action::RawAll,
        "",
        "widok surowy całości",
    ),
    bind(
        Context::Reading,
        Key::char('f'),
        Action::Follow,
        "",
        "włącz albo wyłącz podążanie za nowymi wpisami",
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

/// What `event` does in `stack`: the layer's keys first, then the base's,
/// then the global ones where the base inherits them.
pub fn lookup(stack: impl Into<Stack>, event: &KeyEvent) -> Option<Action> {
    let stack = stack.into();
    let action = lookup_in(BINDINGS, stack, event)?;
    (stack.layer != Some(Context::AgentConversation) || action != Action::RawAll).then_some(action)
}

/// [`lookup`] over an arbitrary table.
pub fn lookup_in(
    bindings: &[Binding],
    stack: impl Into<Stack>,
    event: &KeyEvent,
) -> Option<Action> {
    let stack = stack.into();
    if matches!(stack.base, Context::Panel | Context::Form) && is_newline(event) {
        return Some(Action::Newline);
    }
    stack
        .scopes()
        .find_map(|scope| {
            bindings
                .iter()
                .find(|binding| binding.context == scope && binding.key.matches(event))
        })
        .map(|binding| binding.action)
}

/// The first key that does `action` in `stack`, as it is written in the bar
/// and in help; for text that names a key.
pub fn key_of(stack: impl Into<Stack>, action: Action) -> Option<String> {
    let binding = stack.into().scopes().find_map(|scope| {
        BINDINGS
            .iter()
            .find(|binding| binding.context == scope && binding.action == action)
    })?;
    Some(if action == Action::Newline {
        newline_keys().join("/")
    } else {
        binding.key.display()
    })
}

/// Why a table is not consistent: one message per problem. Every stack the
/// interface can be in ([`Stack::all`]) is checked whole, so a key may not
/// mean two things in one context, nor in a layer and its base, nor shadow a
/// global key.
pub fn check_consistency(bindings: &[Binding]) -> Result<(), Vec<String>> {
    let mut problems = Vec::new();
    for binding in bindings {
        if binding.description.trim().is_empty() {
            problems.push(format!(
                "({}, {}) has no description",
                binding.context.name(),
                binding.key.display()
            ));
        }
    }
    for stack in Stack::all() {
        // In the order a key is looked up: an earlier row shadows a later one.
        let rows: Vec<&Binding> = stack
            .scopes()
            .flat_map(|scope| bindings.iter().filter(move |row| row.context == scope))
            .collect();
        for (index, binding) in rows.iter().enumerate() {
            let label = binding.key.display();
            for other in rows[..index]
                .iter()
                .filter(|other| other.key == binding.key)
            {
                // Agent conversations deliberately refine the generic reader:
                // `v` changes their projection while generic readers retain
                // their local raw shortcut.
                if (other.context == Context::AgentConversation
                    && binding.context == Context::Reading)
                    || (matches!(
                        other.context,
                        Context::AgentConversation | Context::StepConversation
                    ) && binding.context == Context::Details)
                {
                    continue;
                }
                let problem = if other.context == binding.context {
                    format!(
                        "({}, {label}) is bound twice: {:?} and {:?}",
                        binding.context.name(),
                        other.action,
                        binding.action
                    )
                } else {
                    format!(
                        "({}, {label}) shadows ({}, {label})",
                        other.context.name(),
                        binding.context.name()
                    )
                };
                if !problems.contains(&problem) {
                    problems.push(problem);
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

/// The help for `stack`: the layer's keys, the base's, then the global ones
/// the base inherits. Keys that do the same thing are written on one line.
pub fn help_entries(stack: impl Into<Stack>) -> Vec<HelpEntry> {
    help_entries_excluding(stack, &[])
}

/// The help for an agent conversation's RAW projection, where the payload is
/// already visible and entry expansion is unavailable.
pub fn raw_conversation_help_entries(stack: impl Into<Stack>) -> Vec<HelpEntry> {
    help_entries_excluding(stack, &[Action::Expand, Action::RawEntry, Action::RawAll])
}

fn help_entries_excluding(stack: impl Into<Stack>, excluded: &[Action]) -> Vec<HelpEntry> {
    let stack = stack.into();
    let bindings: Vec<_> = BINDINGS
        .iter()
        .copied()
        .filter(|binding| {
            !excluded.contains(&binding.action)
                && !(stack.layer == Some(Context::AgentConversation)
                    && binding.action == Action::RawAll)
        })
        .collect();
    help_entries_in(&bindings, stack)
}

/// [`help_entries`] over an arbitrary table.
pub fn help_entries_in(bindings: &[Binding], stack: impl Into<Stack>) -> Vec<HelpEntry> {
    help_entries_matching(bindings, stack, |_| true)
}

fn help_entries_matching(
    bindings: &[Binding],
    stack: impl Into<Stack>,
    include: impl Fn(&Binding) -> bool,
) -> Vec<HelpEntry> {
    let stack = stack.into();
    let mut entries: Vec<(Context, Action, Vec<String>, &Binding)> = Vec::new();
    for scope in stack.scopes() {
        for binding in bindings
            .iter()
            .filter(|binding| binding.context == scope && include(binding))
        {
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

/// The shortcut bar for `stack`: `key label` pairs, in the order of help,
/// only the rows that carry a label.
pub fn hints(stack: impl Into<Stack>) -> Vec<(String, &'static str)> {
    help_entries(stack)
        .into_iter()
        .filter(|entry| !entry.label.is_empty())
        .map(|entry| (entry.keys, entry.label))
        .collect()
}

pub fn raw_conversation_hints(stack: impl Into<Stack>) -> Vec<(String, &'static str)> {
    raw_conversation_help_entries(stack)
        .into_iter()
        .filter(|entry| !entry.label.is_empty())
        .map(|entry| (entry.keys, entry.label))
        .collect()
}

/// The shortcut bar as one line that fits `width` columns: the keys on the
/// focus accent, the labels plain. Hints that do not fit are dropped from
/// the end, so the first ones — the ones that matter — stay.
pub fn hint_line(stack: impl Into<Stack>, styles: &Styles, width: usize) -> Line<'static> {
    hint_line_from(hints(stack), styles, width)
}

pub fn raw_conversation_hint_line(
    stack: impl Into<Stack>,
    styles: &Styles,
    width: usize,
) -> Line<'static> {
    hint_line_from(raw_conversation_hints(stack), styles, width)
}

fn hint_line_from(
    hints: Vec<(String, &'static str)>,
    styles: &Styles,
    width: usize,
) -> Line<'static> {
    let mut spans = Vec::new();
    let mut used = 0;
    for (keys, label) in hints {
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

/// Draws the `?` help for `stack` as a bordered overlay over `area`.
pub fn render_help(frame: &mut Frame, area: Rect, stack: impl Into<Stack>, styles: &Styles) {
    let entries = help_entries(stack);
    render_help_entries(frame, area, entries, styles);
}

pub fn render_raw_conversation_help(
    frame: &mut Frame,
    area: Rect,
    stack: impl Into<Stack>,
    styles: &Styles,
) {
    render_help_entries(frame, area, raw_conversation_help_entries(stack), styles);
}

fn render_help_entries(frame: &mut Frame, area: Rect, entries: Vec<HelpEntry>, styles: &Styles) {
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
        .border_type(Styles::FRAME)
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
