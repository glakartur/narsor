---
title: "feat: Wspólny mechanizm renderujący rozmowę w zakładkach Przebieg i Rozmowa"
type: feat
status: active
date: 2026-10-31
origin: docs/brainstorms/2026-10-31-tui-wspolny-renderer-rozmowy-requirements.md
---

# feat: Wspólny mechanizm renderujący rozmowę w zakładkach Przebieg i Rozmowa

## Summary

Jedna ścieżka budowy i rysowania rozmowy z parametrem zakresu (`Scope::Whole` albo `Scope::Step { state_id, visit }`) i oknem (`Rect`). *Rozmowa* używa `Whole`, *Przebieg* `Step` wybranego wejścia w krok. Wpisy „WYBÓR WORKFLOW” pojawiają się w obu zakładkach, bo jedna funkcja budująca składa wpisy z prób i zdarzeń (see origin: `docs/brainstorms/2026-10-31-tui-wspolny-renderer-rozmowy-requirements.md`).

---

## Problem Frame

- `Conversation::rebuild_with_events` (`src/tui/task_app/tabs/conversation.rs`) składa rozmowy wszystkich kroków, nagłówki `── <krok> ──` i wybory z `step_entries` + `workflow_choices`.
- *Przebieg* (`tabs/progress.rs`: `render_step` → `render_attempt`) rysuje `attempt.transcript_mut()` jednej próby, bez zdarzeń. Kursor i rozwinięcia leżą w `Attempt` (`on_step_conversation_action` w `detail.rs`).
- Zdarzenia (`view.events`) są dziś czytane tylko dla zakładek *Zdarzenia* i *Rozmowa* (`start_reads` w `detail.rs`). Dla *Przebiegu* trzeba je dołożyć (założenie z origin potwierdzone: nie są dostępne).
- `render_transcript(frame, area, …)` jest wspólny z *Dyrygentem* (`conductor.rs`) i już bierze wymiary z `area`.

---

## Requirements

Jak w origin: R1–R3 (wybory w obu widokach), R4–R7 (zakres), R8–R10 (okno), R11–R13 (bez zmian). Przyjęte odpowiedzi na pytania otwarte: brak wpisu wyboru dla trwającego kroku do czasu zdarzenia; treść rozmowy się zawija, skracane są tylko linie jednowierszowe z natury.

---

## Scope Boundaries

- Bez zmiany treści/formatu wpisu wyboru i nagłówka kroku, bez nowych zdarzeń i zmian protokołu.
- Bez zmian osi kroków, *Zdarzeń*, *Dyrygenta*, *Info*. `render_transcript` zachowuje wynik wizualny dla *Dyrygenta*.
- Bez zmiany skrótów, kursora, „podąża”, trybów wyświetlania.

---

## Key Technical Decisions

- **Budowa:** wyodrębnić z `rebuild_with_events` wolną funkcję `build_conversation(scope, positions, attempts, events) -> Built { entries, loading, failure, record }`. `Scope::Whole` = dzisiejsze zachowanie (z nagłówkami kroków). `Scope::Step` filtruje `positions` do `(state_id, visit)` (wszystkie próby wejścia, R6), pomija nagłówki, a wybory bierze z okna `[sequence wejścia, sequence następnego wejścia w dowolny krok)`; ostatnie wejście — do końca. Brak `StateEntered` dla kroku (stary log) → bez wyborów w tym zakresie. `Conversation::rebuild_with_events` staje się cienką nakładką na `Whole`; `push_position` zostaje jedyną logiką wpisów (nagłówek dodawany tylko dla `Whole`).
- **Stan *Przebiegu*:** `Progress` dostaje mapę `step_views: BTreeMap<(String, u32), StepView>`, gdzie `StepView` = `TranscriptState` + metadane `Built` (loading/failure/record). Kursor, rozwinięcia i „podąża” są więc osobno dla każdego wejścia w krok i przeżywają zmianę zaznaczenia na osi (pytanie otwarte o stan per zakres). Stan transkryptu w `Attempt` zostaje dla zgodności z innymi użytkownikami, ale *Przebieg* przestaje go rysować; `on_step_conversation_action` i `step_conversation()` kierują się na `StepView` (zachować semantykę „ma rozmowę do pokazania”, od której zależy `EnterStep`).
- **Odbudowa:** `rebuild_step(view)` wołana tak jak `rebuild_conversation` (po odczycie timeline/zdarzeń/próby i gdy aktywna jest zakładka *Przebieg*), tylko dla zaznaczonego wejścia; `replace_entries` zachowuje kursor, rozwinięcia i koniec (jak dziś w *Rozmowie*).
- **Okno:** nowy typ `ConversationWindow { area: Rect }` (x, y, szerokość, wysokość) przekazywany do `render_conversation(frame, window, styles, &mut TranscriptState, flags)`; to rozszerzona i przemianowana treść `render_transcript` (zostaje jej alias/delegat dla `conductor.rs`, by *Dyrygent* się nie zmienił). Pozycja (x, y) służy do wierszy podświetlenia wyborów (dziś liczone z `area.top()`), szerokość do zawijania i skracania, wysokość do okna przewijania. Bez nowych obliczeń z kontekstu zakładki.
- **Skracanie (R9):** do `TranscriptLine` dodać znacznik „jednowierszowa” (nagłówki wpisów, nagłówek `── krok ──`, wpisy wyboru, plakietki). `wrap_visible_lines` (`src/tui/transcript.rs`) dla takich linii obcina do szerokości z `…` zamiast zawijać; reszta zawija jak dziś. Przy implementacji sprawdzić, które linie dziś już są skracane, żeby nie zmienić *Dyrygenta* (R12).
- **Zdarzenia dla *Przebiegu*:** w `start_reads` dla `DetailTab::Progress` dodać `start_events_read`, a próby wszystkich pozycji zaznaczonego wejścia (nie tylko zaznaczonej próby) czytać przez `start_attempt_read`. Historia (`history.rs`) ma własny plan odczytu — rozszerzyć o te same czytania dla zakładki *Przebieg* (R13).

---

## Implementation Units

### U1. Wspólna funkcja budująca z zakresem
- **Pliki:** `src/tui/task_app/tabs/conversation.rs`
- **Zmiany:** `Scope`, `Built`, `build_conversation`; `rebuild_with_events` deleguje do niej z `Scope::Whole`; `push_position` przyjmuje flagę nagłówka; helper `choices_for_visit` (okno sekwencji).
- **Testy (jednostkowe, w module):** zakres `Whole` daje wynik identyczny jak dziś (istniejące testy bez zmian); `Step` bez nagłówków; AE1 (wybór po rozmowie `code_review`, przed nagłówkiem następnego w `Whole`; na końcu w `Step`); AE2 (pętla `code_review → fix → code_review`: drugie wejście tylko z własnymi wyborami); stary log bez `StateEntered`; wiele prób jednego wejścia; ta sama próba współdzielona przez retry pokazana raz.

### U2. Okno i skracanie w rendererze
- **Pliki:** `src/tui/task_app/tabs/conversation.rs`, `src/tui/transcript.rs`, `src/tui/task_app/tabs/conductor.rs` (tylko jeśli zmienia się sygnatura)
- **Zmiany:** `ConversationWindow`, `render_conversation`, `render_transcript` jako delegat; znacznik jednowierszowej linii i obcinanie z `…` w `wrap_visible_lines`.
- **Testy:** `tests/tui_render.rs` — AE3 (40 kolumn: nagłówek kroku i wpis wyboru z długą etykietą kończą się `…`, treść zawija się); okno z `x,y ≠ 0` podświetla właściwe wiersze wyborów; snapshot *Dyrygenta* bez zmian.

### U3. Stan i rysowanie zakładki *Przebieg* przez wspólny mechanizm
- **Pliki:** `src/tui/task_app/tabs/progress.rs`, `src/tui/task_app/detail.rs`
- **Zmiany:** `StepView` w `Progress`; `render_step` woła `render_conversation` z `Scope::Step` zamiast `render_attempt` (zachować noty plakietki rekordu i błędu odczytu oraz komunikaty „brak rozmowy…”/„wczytuję…” z `Built`); `rebuild_step`; przekierowanie `on_step_conversation_action` i `step_conversation()`; zachowanie `recheck`/`step_focus` (R11).
- **Testy:** `tests/tui_state.rs` — kursor/rozwinięcia kroku zostają po przejściu na inny krok i z powrotem; `EnterStep`/`LeaveStep` jak dziś; tryby rozmowa/chronologia/RAW; AE4 (zmiana rozmiaru terminala w trakcie rozwiniętego wpisu zachowuje kursor, rozwinięcia, „podąża”, w obu zakładkach, R10).

### U4. Odczyt zdarzeń i prób dla *Przebiegu* (aktywne zadanie i Historia)
- **Pliki:** `src/tui/task_app/detail.rs` (`start_reads`), `src/tui/task_app/history.rs`
- **Zmiany:** `DetailTab::Progress` czyta zdarzenia i próby wszystkich pozycji zaznaczonego wejścia; plan odczytu Historii analogicznie; odbudowa zaznaczonego kroku po odczytach.
- **Testy:** `tests/tui_state.rs` — po odczytach zdarzeń *Przebieg* pokazuje wybór (R1) w aktywnym zadaniu i w Historii (R13); brak wpisu dla trwającego kroku bez zdarzenia wyboru.

### U5. Dokumentacja
- Zaktualizować opis zakładek *Przebieg*/*Rozmowa* w dokumentacji TUI (`README.md`, `ARCHITECTURE_DECISIONS.md`; dokumenty w `docs/brainstorms` i `docs/plans` to historia, nie zmieniamy ich), dopisać zdanie o wspólnym mechanizmie i widocznych wyborach.

---

## Risks

- **Stan podwójny** (`Attempt.transcript` vs `StepView`): po zmianie nie rysować ani nie sterować starym; test akcji klawiszowych w *Przebiegu* wyłapie pomyłkę.
- **Koszt odczytu zdarzeń** w *Przebiegu*: istniejący `needs_read(revision)` i limity stron (`MAX_PAGES`) ograniczają go; odczyt tylko przy aktywnej zakładce.
- **Skracanie** może zmienić wygląd *Dyrygenta*: znacznik jednowierszowy ustawiać tylko w budowie *Rozmowy*/*Przebiegu*, nie globalnie; snapshot *Dyrygenta* strzeże R12.
- **Wejście w krok bez `StateEntered`:** zakres *krok* bez wyborów (świadomy kompromis, udokumentowany w teście).

---

## Verification

`cargo test --test tui_render --test tui_state --test tui_conversation` i testy modułów `tabs::conversation`; `cargo clippy --all-targets`.
