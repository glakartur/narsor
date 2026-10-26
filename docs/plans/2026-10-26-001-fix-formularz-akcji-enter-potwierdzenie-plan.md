---
title: "fix: Enter w oknie potwierdzenia formularza akcji workflow zatwierdza za pierwszym razem"
type: fix
status: active
date: 2026-10-26
origin: docs/brainstorms/2026-10-26-formularz-akcji-workflow-enter-requirements.md
---

# fix: Enter w oknie potwierdzenia formularza akcji workflow zatwierdza za pierwszym razem

## Summary

Operator otwiera formularz akcji workflow. Po kilkunastu sekundach naciska `Enter`. Okno potwierdzenia znika albo nie przyjmuje `Enter`. Plan zaczyna od testów, które odtwarzają objaw (diagnoza). Dopiero wynik testów wybiera poprawkę. Potem plan dodaje widoczne komunikaty odmowy i blokadę drugiego wysłania.

Problem Frame i hipotezy H1–H4: patrz origin (see origin: `docs/brainstorms/2026-10-26-formularz-akcji-workflow-enter-requirements.md`).

---

## Stan wiedzy z kodu (czytanie, bez uruchamiania)

To są ustalenia z czytania kodu. Jednostka U1 musi je potwierdzić testem. Nie traktuj ich jako faktów.

- `WaitingPanel::sync` (`src/tui/task_app/panel/state.rs`) **nie przebudowuje** istniejącego wpisu. Przebudowuje tylko wpis w stanie `Model::Missing`. Odświeżenie listy samo w sobie nie wywołuje `Chooser::rebuilt`.
- `Chooser::rebuilt` (`src/tui/chooser.rs`) woła `form.decline()`. Zamyka okno potwierdzenia. Fokus pola zostaje.
- `Chooser::rebuilt` jest wołane z `Entry::rebuild`. `Entry::rebuild` jest wołane z dwóch miejsc: `WaitingPanel::reread` i `WaitingPanel::apply_conductor`.
  - `reread` działa po każdym odczycie osi czasu: `TaskApp::on_detail_read` → `DetailRead::Timeline` → `reread_waiting` (`src/tui/task_app/detail.rs`). Ten odczyt wraca po każdej zmianie rewizji wiersza zadania (`invalidate_changed`). Działające zadanie zmienia rewizję co kilka sekund. To pasuje do „kilkanaście sekund".
  - `apply_conductor` przebudowuje tylko wpisy `FailedStep` i `ConductorProposal`.
- Skutek `decline()` przy fokusie w polu: następny `Enter` idzie do `advance_field_or_submit` → `submit` → `ask_confirmation`. Okno otwiera się ponownie. Dopiero drugi `Enter` zatwierdza. To pasuje do objawu „kilka `Enter`" i do „szybki `Enter` po `Enter` działa" (brak przebudowy między nimi).
- `Entry::rebuild` ustawia też listę opcji od nowa (nowe pozycje z odczytu kroków). To pasuje do „lista opcji się zmienia".
- H1: `sync` woła `leave_all`, gdy zmienia się `active_key()` (`Switcher::sync` zmienia aktywny formularz, gdy backend wskaże nowy domyślny). To jest zgodne z R6/R14 z `docs/brainstorms/2026-10-25-tui-panel-rozmowy-trzy-formularze-requirements.md`. Dotyczy tylko prawdziwej zmiany formularza. Zwykłe odświeżenie jej nie powoduje. H1 jest więc słabsza niż H2.
- H3: `TaskApp::dispatch` czeka na `answer(...).await` w gałęzi klawisza (`src/tui/task_app/mod.rs`). Klient ma limity czasu (do sprawdzenia w U1). Artur zgłosił, że ekran reaguje (Q2) i backend jest lokalny (Q5).
- H4: `submit_with` (`src/tui/task_app/panel/app.rs`) przy `!mutation_allowed()` zwraca `Request::None`. Powód idzie tylko do paska (`notify`). Panel nic nie pokazuje. Tu jest luka względem R7.
- R8: po zatwierdzeniu okno jest zamknięte (`Stage::Editing`). Drugi `Enter` otwiera je ponownie. Drugi `Enter` w oknie wysyła drugie żądanie z tym samym kluczem idempotencji (`entry.key`). Brak blokady „zapis w toku" w `Entry`.

---

## Requirements

Z origin: R1–R9, AE1–AE5. Zakres tego planu:

| ID | W planie | Jednostka |
|----|----------|-----------|
| R1 | tak | U1 |
| R2, R3 | tak | U1 (wynik zapisany w sekcji „Wynik diagnozy") |
| R4, R5 | tak | U2 |
| R6 | tak, tylko ochrona istniejącego zachowania testem | U3 |
| R7 | tak | U4 |
| R8 | tak | U5 |
| R9 | warunkowo | Odroczone (patrz Scope Boundaries) |

---

## Scope Boundaries

- Brak zmian w układzie wizualnym, skrótach klawiszy i regułach `Esc` / `Tab`.
- Brak zmian kontraktu API z backendem. Problem po stronie serwera to osobne zadanie.
- Brak przepisania pętli zdarzeń TUI.

### Deferred to Follow-Up Work

- R9 (zapis poza pętlą klawiszy). Wykonać tylko, gdy U1 potwierdzi H3 pomiarem. Dane z origin (Q2, Q5) wskazują przeciw H3. Jeśli H3 zostanie potwierdzona, zapis jest osobnym zadaniem: to zmiana `dispatch` i modelu `Outcome` z `background`.
- Zdalny backend i czas odpowiedzi serwera.

---

## Key Technical Decisions

- **Najpierw test, który zawodzi.** U1 pisze testy odtwarzające objaw na istniejącym API (`WaitingPanel` i `TaskApp`). Test, który nie zawodzi przed poprawką, odrzuca daną hipotezę. Wynik trafia do tego planu i do opisu PR.
- **Poprawka H2 w `Chooser::rebuilt`, nie w `reread`.** `rebuilt` jest wspólnym miejscem. Reguła: okno potwierdzenia zostaje otwarte, gdy zaznaczona pozycja po przebudowie jest równa (`ChooserItem: PartialEq`) pozycji sprzed przebudowy, jeśli pominąć wartości wpisane przez operatora. Gdy pozycja zniknęła albo się zmieniła, okno się zamyka (jak dziś), a panel pokazuje powód (U4). To zachowuje bezpieczeństwo: operator nie zatwierdza czegoś, czego nie widział.
  - Alternatywa odrzucona: pomijać `Entry::rebuild`, gdy model się nie zmienił. To nie pokrywa przypadku, gdy model zmienił się w innej pozycji niż zaznaczona.
  - Jeśli U1 pokaże, że przyczyną jest inna ścieżka (H1), U2 zmienia się odpowiednio. Wtedy `sync` oddaje fokus polom tylko wtedy, gdy ten sam formularz jest nadal aktywny (otwarte pytanie z origin: [Affects R5]).
- **Komunikat odmowy w panelu przez `Entry::notices()`.** Miejsce już istnieje (`entry.error`, rysowane nad listą akcji jako `✗ …`). Nie dodajemy nowego elementu układu. Pasek zostaje jako dodatkowy kanał.
- **Blokada „zapis w toku" na `Entry`.** Pole ustawiane w `WaitingPanel::prepare`, czyszczone w `WaitingPanel::settle`. `Enter` w tym czasie nie tworzy żądania. Klucz idempotencji zostaje jak dziś.

---

## Implementation Units

### U1. Odtworzenie objawu i diagnoza hipotez

**Goal:** Dla każdej z H1–H4 uzyskać wynik „potwierdzona" albo „odrzucona" z dowodem. Zapisać wynik w sekcji „Wynik diagnozy" tego planu.

**Requirements:** R1, R2, R3, AE1, AE2.

**Dependencies:** brak.

**Files:**
- `tests/tui_panel.rs` (nowe testy; używają `opened`, `asking`, `synced`, `key`)
- `src/tui/task_app/panel/state.rs` (tylko odczyt, bez zmian w tej jednostce)

**Approach:**
- Użyć istniejącego sposobu budowy zadań w `tests/tui_panel.rs` (`with_panel`, `with_pending`, `TaskApp::load`).
- H2: otworzyć formularz z pytaniem (`asking()`), wypełnić pole, nacisnąć `Enter` do otwarcia okna potwierdzenia (`panel.is_confirming()`), wywołać `WaitingPanel::reread` (publiczne API) albo `TaskApp::on_detail_read` z `DetailRead::Timeline`, sprawdzić `is_confirming()`.
- H1: po otwarciu okna wywołać `TaskApp::load` z nowym listingiem, w którym ten sam formularz jest nadal dostępny, i z wariantem, w którym backend wskazuje nowy domyślny formularz. Sprawdzić fokus pola i `is_confirming()`.
- H4: ustawić połączenie jako nie-`live` i nacisnąć `Enter` w oknie. Zapisać, co widzi operator w panelu.
- H3: odczytać limity czasu klienta HTTP (`crates/narsor-api-client/src/api/client.rs`) i zapisać najgorszy czas blokady pętli. Pomiar na żywo z lokalnym daemonem jest opcjonalny. Wynik odpowiada na pytanie: czy R9 wchodzi do zakresu.
- R3: sprawdzić, że formularz z `confirming_submit` to wpis `Slot::Entry` (a nie `Slot::Manual`, który nie ma okna potwierdzenia). Zapisać wniosek.
- Q4: dodać wariant testu dla pytania agenta i dla akcji workflow (`AttentionKind`). Zapisać, które wpisy pokazują objaw.
- Testy diagnostyczne, które zawodzą dla potwierdzonej hipotezy, zostają jako testy regresji w U2/U3 (zmienia się tylko oczekiwany wynik).

**Execution note:** Najpierw testy z oczekiwanym zachowaniem z R4/R5 (powinny zawieść na obecnym kodzie, jeśli hipoteza jest prawdziwa). Test, który przechodzi, odrzuca hipotezę.

**Patterns to follow:** `tests/tui_panel.rs` (`a_new_default_takes_the_view_and_the_chat_draft_stays`), `tests/task_app_support/mod.rs`.

**Test scenarios:**
- Covers AE2. Okno potwierdzenia otwarte → `reread` z tymi samymi danymi → `is_confirming()` pozostaje `true`. (H2: zawodzi na dziś, jeśli hipoteza jest prawdziwa.)
- Okno potwierdzenia otwarte → `reread` z odczytem kroków, który zmienia listę opcji (nowa pozycja) → zaznaczona pozycja nadal ta sama → okno otwarte.
- Okno potwierdzenia otwarte → `reread` usuwa zaznaczoną pozycję → okno zamknięte. Oczekiwane po U2/U4.
- Covers AE1. Pełna ścieżka przez `TaskApp`: pole wypełnione, `Enter` (okno), `on_detail_read(Timeline)`, `Enter` → `Request::Answer` za pierwszym razem.
- H1: `load` z tą samą ofertą formularzy → fokus pola i okno bez zmian.
- H1: `load` z nowym domyślnym formularzem → panel przełączony, szkic poprzedniego zostaje (to zachowanie R6, ma zostać).
- H4: `Enter` w oknie przy zerwanym połączeniu → `Request::None`; zapisać, co panel pokazuje.

**Verification:** Plan zawiera uzupełnioną sekcję „Wynik diagnozy" z werdyktem i dowodem dla H1–H4, odpowiedzią na R3 i Q4.

---

### U2. Okno potwierdzenia przeżywa odświeżenie

**Goal:** Pierwszy `Enter` w oknie potwierdzenia zatwierdza formularz, gdy ten sam formularz jest nadal dostępny.

**Requirements:** R4, R5, AE1, AE2.

**Dependencies:** U1 (potwierdzona H2; jeśli nie potwierdzona, zmienić jednostkę według wyniku).

**Files:**
- `src/tui/chooser.rs` (`Chooser::rebuilt`)
- `src/tui/task_app/panel/state.rs` (`Entry::rebuild`; tylko jeśli potrzebne)
- `tests/tui_panel.rs`
- testy jednostkowe w `src/tui/chooser.rs` (moduł `tests`, jeśli istnieje; inaczej `tests/tui_form.rs`)

**Approach:**
- W `rebuilt` zapamiętać, czy okno było otwarte i jaką pozycję dotyczyło (etykieta zaznaczonej pozycji, jak dziś dla `selected`).
- Po zbudowaniu nowej listy porównać starą i nową pozycję o tej samej etykiecie z pominięciem wartości pól. Wartości pól przenosi `carry_over`, więc muszą być porównywane po przeniesieniu. Jeśli pozycja jest ta sama (opis, `irreversible`, `unavailable`, nazwy i typy pól), okno zostaje otwarte. W przeciwnym razie `decline()` jak dziś.
- Nie zmieniać `decline()` w `form/engine.rs`. Reguła należy do `Chooser`.
- Ustawić wynik rozróżnienia tak, aby U4 mógł pokazać operatorowi powód zamknięcia okna (zwracana flaga albo stan odczytywany z `Chooser`; wybór formy należy do wykonawcy).

**Patterns to follow:** `Chooser::rebuilt` (przenoszenie pól i zaznaczenia), testy w `tests/tui_form.rs`.

**Test scenarios:**
- Covers AE2. `rebuilt` z identyczną listą przy otwartym oknie → okno otwarte, `Enter` zwraca `ChooserAction::Submit`.
- `rebuilt`, w którym zmieniła się inna pozycja (nie zaznaczona) → okno otwarte.
- `rebuilt`, w którym zmienił się opis lub `unavailable` zaznaczonej pozycji → okno zamknięte.
- `rebuilt`, w którym zaznaczona pozycja zniknęła → okno zamknięte, zaznaczenie na pozycji w granicach.
- `rebuilt` po zmianie listy, gdy wpisana wartość pola jest błędna według nowej definicji → okno zamknięte (zgodnie z `is_sendable`).
- Covers AE1. `TaskApp`: pole wypełnione, `Enter`, odczyt osi czasu, `Enter` → `Request::Answer` bez drugiego okna.
- Wpisany tekst zostaje po `rebuilt` (regresja istniejącego zachowania).

**Verification:** Testy z U1 dla H2 przechodzą. Żaden istniejący test w `tests/tui_panel.rs`, `tests/tui_form.rs`, `tests/tui_state.rs` nie zmienia wyniku.

---

### U3. Ochrona zachowania przy zmianie formularza (R6)

**Goal:** Potwierdzić testem, że prawdziwa zmiana formularza nadal przełącza panel i zostawia szkic. Dodać komunikat o zmianie.

**Requirements:** R5, R6, AE5.

**Dependencies:** U1.

**Files:**
- `src/tui/task_app/panel/state.rs` (`WaitingPanel::sync`, `SyncEffect`)
- `src/tui/task_app/panel/request.rs` (`SyncEffect`)
- `src/tui/task_app/panel/app.rs` (`sync_waiting`: komunikat)
- `tests/tui_panel.rs`

**Approach:**
- `sync` zwraca w `SyncEffect` informację, że widok przełączył się z powodu nowego domyślnego formularza. `sync_waiting` pokazuje powiadomienie w pasku i notkę w panelu, gdy operator miał fokus w polu lub okno otwarte. Wzorzec: `lost_current`.
- Jeśli U1 potwierdzi H1 dla tego samego formularza (fokus tracony bez zmiany formularza), poprawić `sync`: `leave_all` tylko, gdy zmienił się klucz formularza, nie przy odświeżeniu oferty. Dziś warunek `active_key() != before` już to robi. Potwierdzić testem i nie zmieniać kodu, jeśli test przechodzi.

**Test scenarios:**
- Covers AE5. Pytanie agenta przychodzi, gdy operator ogląda formularz akcji → panel pokazuje pytanie, `SyncEffect` zgłasza przełączenie, szkic formularza akcji zostaje po powrocie `Esc`.
- Ta sama oferta formularzy w nowym listingu → fokus pola i okno potwierdzenia bez zmian.
- Zadanie traci formularz (zniknął) → panel przechodzi na domyślny, szkic w `released`.

**Verification:** Testy przechodzą. Powiadomienie o zmianie jest widoczne.

---

### U4. Widoczna odmowa w panelu (R7)

**Goal:** Gdy `Enter` nie może zatwierdzić formularza, operator widzi powód w panelu. Wpisany tekst zostaje.

**Requirements:** R7, AE3.

**Dependencies:** U2.

**Files:**
- `src/tui/task_app/panel/app.rs` (`submit_with`)
- `src/tui/task_app/mod.rs` (`mutation_allowed`; zwrócić powód obok powiadomienia)
- `src/tui/connection.rs` (źródło powodu, tylko odczyt)
- `src/tui/task_app/panel/state.rs` (`WaitingPanel::fail`)
- `tests/tui_panel.rs`

**Approach:**
- Wydzielić z `mutation_allowed` wariant zwracający `Result<(), String>` z powodem. `mutation_allowed` zostaje cienką nakładką, aby nie zmieniać innych wywołań (`actions.rs`, `harness.rs`, `limit.rs`, `tabs/conductor.rs`, `repositories.rs`).
- `submit_with`: przy odmowie wywołać `view.waiting.fail(powód)` obok powiadomienia w pasku. `fail` ustawia `entry.error`, które `Entry::notices()` rysuje w panelu.
- Błąd kasowany przy następnej edycji pola (to już robi `on_key` przez `entry.error = None`).
- Okno potwierdzenia zamknięte przez zmianę pozycji (U2) zgłasza powód tym samym kanałem (np. „Lista opcji się zmieniła — sprawdź i zatwierdź ponownie.").

**Test scenarios:**
- Covers AE3. Połączenie zerwane, `Enter` na ostatnim polu i w oknie → `Request::None`, panel zawiera tekst powodu, wpisany tekst zostaje.
- Walidacja pola nie przechodzi → komunikat przy polu (istniejące zachowanie, regresja).
- Zmiana pozycji zamyka okno → panel pokazuje powód zamknięcia.
- Po naprawie połączenia pierwszy `Enter` zatwierdza (regresja).
- Ekran: `screen(...)` zawiera `✗` i powód w wysokości 24 wierszy (układ nie zmienia się poza jedną linią notki).

**Verification:** Testy przechodzą. Układ formularza bez zmian poza dodatkową linią notki.

---

### U5. Brak drugiego wysłania przy zapisie w toku (R8)

**Goal:** `Enter` w trakcie zapisu nie wysyła formularza drugi raz.

**Requirements:** R8, AE4.

**Dependencies:** U2.

**Files:**
- `src/tui/task_app/panel/state.rs` (`Entry`, `prepare`, `settle`)
- `src/tui/task_app/panel/app.rs` (komunikat „wysyłam…")
- `tests/tui_panel.rs`

**Approach:**
- Dodać do `Entry` znacznik „zapis w toku". Ustawić w `prepare`, wyczyścić w `settle` (sukces i błąd). Klucz idempotencji zostaje (`entry.key`).
- `prepare` przy znaczniku zwraca `Err("Zapis trwa.")`. Komunikat trafia do panelu (U4).
- Uwaga: pętla TUI czeka na `answer(...).await` w gałęzi klawisza (H3). W tym czasie klawisze nie są odczytywane, więc `Enter` czeka w kolejce terminala. Po powrocie przychodzi do `on_key`. Znacznik chroni przed drugim żądaniem po powrocie, gdy wynik nie wyczyścił wpisu (błąd, który zostawia wpis).
- Kolejka w terminalu: sprawdzić w U1, czy dodatkowe `Enter` z kolejki trafia do okna po sukcesie. Po sukcesie panel traci fokus (`panel_focus = false`), więc `Enter` idzie do listy. Zapisać wynik w sekcji „Wynik diagnozy".

**Test scenarios:**
- Covers AE4. `prepare` dwa razy bez `settle` → drugi zwraca `Err`, nie `Ok`.
- `settle(Ok)` → kolejne `prepare` działa (nowa decyzja, nowy klucz).
- `settle(Err retryable)` → kolejne `prepare` używa tego samego klucza (istniejące zachowanie).
- `settle(Err not retryable)` → nowy klucz.
- Przez `TaskApp`: `Enter` w oknie, `Enter` w oknie ponownie przed `answer` → jedno `Request::Answer`.

**Verification:** Testy przechodzą. Test `settle` dla klucza idempotencji nie zmienia wyniku.

---

## Wynik diagnozy

Uzupełnia wykonawca w U1. Szablon:

| Hipoteza | Werdykt | Dowód (test, log, pomiar) |
|----------|---------|----------------------------|
| H1 | do ustalenia | |
| H2 | do ustalenia | |
| H3 | do ustalenia | |
| H4 | do ustalenia | |
| R3 (który formularz) | do ustalenia | |
| Q4 (pytanie / rozmowa / akcja) | do ustalenia | |

---

## Risks

- **H2 nie potwierdzi się.** Wtedy U2 zmienia cel. Kolejna kandydatka to H1. Szukać dalej w `sync_forms` (`manual.rebuilt` zamyka okno, ale `Chooser` formularza ręcznego nie ma `confirming_submit`) i w `Switcher::sync`. Jeśli żaden test nie odtwarza objawu, **nie** pisać poprawki. Zwrócić do Artura z wynikiem i prośbą o kroki odtworzenia.
- **Okno zostaje otwarte po zmianie treści pod spodem.** Reguła porównania w U2 musi obejmować wszystko, co operator widzi w oknie. Test „zmienił się opis" to pilnuje. Operator nie zatwierdza niewidzianej treści.
- **Brak testu na prawdziwym daemonie.** Testy używają `TaskApp::load` i bezpośrednich wywołań. Przed zamknięciem zadania Artur sprawdza ręcznie z lokalnym backendem: formularz otwarty ponad 15 s, `Enter` w oknie.

## Verification (całość)

- Testy z `tests/tui_panel.rs`, `tests/tui_form.rs`, `tests/tui_state.rs` przechodzą.
- Wynik diagnozy jest w tej sekcji i w opisie PR.
- Ręczna próba Artura: formularz akcji otwarty kilkanaście sekund, pierwszy `Enter` w oknie zatwierdza.
