---
title: "fix: TUI nie czyta listy zadań co ok. 1 s przy każdym raporcie runnera"
type: fix
status: active
date: 2026-10-22
origin: docs/brainstorms/2026-10-22-tui-interwal-odswiezania-requirements.md
---

# fix: TUI nie czyta listy zadań co ok. 1 s przy każdym raporcie runnera

## Podsumowanie

Ustawienie `tui_refresh_interval` działa. Odczyt okresowy idzie przez `RefreshSchedule` co 10 s. Znacznik odczytu co ok. 1 s ma inną przyczynę: każdy raport runnera (`TaskEventKind::RunnerReported`) prosi TUI o odczyt listy. Runner wysyła taki raport co najwyżej raz na sekundę, gdy jego stan się zmienia. Plan zmienia to jedno miejsce. Raport, który tylko przesuwa kursor zdarzeń, nie prosi już o odczyt. Zmiana stanu, pytania lub uwagi nadal prosi o odczyt.

---

## Problem Frame

Użytkownik widzi znacznik odczytu w panelu zadań mniej więcej co sekundę. Chce odczytu co `tui_refresh_interval` (10 s). (see origin: `docs/brainstorms/2026-10-22-tui-interwal-odswiezania-requirements.md`)

### Ustalenie przyczyny (pytanie otwarte P1)

Ustalone z kodu. Nie uruchamiano TUI, więc wniosek wymaga potwierdzenia testem (U1).

- Znacznik rysuje `src/tui/task_app/layout.rs` tylko wtedy, gdy działa zadanie `JobKey::Refresh` (`app.is_refreshing()`). Odczyty szczegółów, rozmowy i Conductora używają innych kluczy (`JobKey::Detail`, `JobKey::Open`). Nie rysują tego znacznika.
- Odpada więc odpytywanie zakładki Conductor co 2 s (`READER_RETRY`). To odczyt szczegółów.
- Odpada `STREAM_RECHECK_INTERVAL`. To stała po stronie serwera (`crates/narsor-server/src/auth_gate.rs`), nie odczyt listy w TUI.
- `FIRST_RETRY` (1 s) działa tylko po błędzie odczytu. W normalnej pracy nie działa (R5 wyłącza ten przypadek).
- Zostaje odczyt wywołany zdarzeniem. W `src/tui/task_app/list.rs` funkcja `patch_row` zwraca `true` dla `RunnerReported`. Komentarz w kodzie mówi, że krok runnera nie jest w zdarzeniu. `on_stream` w `src/tui/task_app/mod.rs` wywołuje wtedy `schedule.request`. Po `DEBOUNCE` (400 ms) startuje odczyt listy.
- Runner raportuje zmiany co `report_interval` (domyślnie 1000 ms, `crates/narsor-runner/src/runner/mod.rs`). Każdy odczyt kończy się `schedule.done`. Okres 10 s zaczyna się od nowa, ale następny raport przychodzi po ok. 1 s. Wynik: odczyt co ok. 1,4 s, gdy zadanie pracuje i raport się zmienia.

---

## Requirements

Z dokumentu źródłowego:

- R1–R3. Już spełnione w kodzie: okres, konfiguracja, `none`. Plan ich nie zmienia.
- R4. Odczyt ręczny (`r`), ponowienia po błędzie i odczyty po zdarzeniach, które zmieniają to, czego zdarzenie nie niesie, zostają. Odczyt po `RunnerReported` jest przyczyną odczytów co ok. 1 s, więc plan go zawęża.
- R5. W normalnej pracy TUI nie czyta listy częściej niż co `tui_refresh_interval`. Wyjątek: odczyty z R4.

---

## Scope Boundaries

- Bez zmiany nazwy klucza, formatu wartości i wartości domyślnej (10 s).
- Bez zmiany `DEBOUNCE`, `FIRST_RETRY`, `MAX_RETRY`, `READER_RETRY` i ticków animacji.
- Bez zmiany odczytów szczegółów i rozmowy.
- Bez zmiany runnera ani serwera. Częstość raportów (`report_interval`) zostaje.

### Deferred to Follow-Up Work

- Ogólny limit częstości odczytów po zdarzeniach (np. nie częściej niż co N s). Dopiero gdy po U1 inne zdarzenia z `_ => true` w `patch_row` okażą się częste w normalnej pracy.

---

## Key Technical Decisions

- **Zawężamy żądanie odczytu po `RunnerReported`, nie wyłączamy go.** Zdarzenie niesie `execution_status`, `pending_input_id`, `attention` i `event_cursor`. Wiersz listy (stan, ikona „gotowe”, uwaga) zależy od trzech pierwszych. Samo przesunięcie `event_cursor` nie zmienia wiersza. Odczyt jest potrzebny tylko wtedy, gdy zmienia się jedno z trzech pól.
- **Aktualny krok runnera pokazuje odczyt okresowy.** Krok nie jest w zdarzeniu. Po zmianie widać go z opóźnieniem do `tui_refresh_interval`. To jest skutek, o który prosi użytkownik. README opisuje ten skutek (U3).
- **Porównujemy z poprzednim stanem wiersza.** `patch_row` ma już wiersz `task` z poprzednim `runner`. Porównanie robimy przed nadpisaniem pól.
- **Wiersz bez `runner` nadal prosi o odczyt.** Zachowujemy obecne zachowanie (`let Some(runner) = ... else { return true }`).

---

## Implementation Units

### U1. Test odtwarzający przyczynę

**Goal:** Potwierdzić przyczynę (P1) testem, który dziś nie przechodzi.

**Requirements:** R5, P1.

**Dependencies:** brak.

**Files:**
- `tests/tui_state.rs` (test w module testów strumienia, obok `queue_changes_and_answers_patch_the_row_without_a_read`)

**Approach:** Wczytaj listę z zadaniem `running` z `runner`. Wyślij `RunnerReported` z tym samym `execution_status`, tym samym `pending_input_id` i `attention`, ale z większym `event_cursor`. Sprawdź `app.refresh_wanted()`.

**Execution note:** Test najpierw. Ma się nie powieść przed U2. Jeśli przejdzie przed U2, przyczyna jest inna — zatrzymaj się i wróć do listy kandydatów z sekcji „Ustalenie przyczyny”. Sprawdź wtedy zdarzenia z gałęzi `_ => true` w `patch_row`.

**Patterns to follow:** helpery `controlled`, `narsor_runner`, `with_runner`, `event`, `listing` z `tests/tui_state.rs`.

**Test scenarios:**
- Covers R5. Raport z tym samym stanem i nowym `event_cursor` nie ustawia `refresh_wanted`.
- Kursor zdarzeń wiersza (`runner.event_cursor`) nadal się przesuwa.

**Verification:** Test nie przechodzi na obecnym kodzie i przechodzi po U2.

---

### U2. Zawężenie żądania odczytu po `RunnerReported`

**Goal:** `RunnerReported` prosi o odczyt tylko przy zmianie, której zdarzenie nie pokazuje w wierszu.

**Requirements:** R4, R5.

**Dependencies:** U1.

**Files:**
- `src/tui/task_app/list.rs` (`patch_row`, komentarz przy `apply_event`)
- `tests/tui_state.rs`

**Approach:** W gałęzi `RunnerReported` porównaj nowe `execution_status`, `pending_input_id` i `attention` ze stanem wiersza przed zmianą. Zwróć `true` przy różnicy, `false` przy samym `event_cursor`. Do porównania `pending_input_id` użyj poprzedniego `runner.pending_input` (to pole jest dziś czyszczone tylko przy `None`). Do porównania `attention` użyj części zgłoszonej przez runnera, bo `replace_reported_attention` łączy ją z częścią płaszczyzny kontrolnej. Zachowaj istniejące łatanie wiersza (status, kursor, uwaga). Zaktualizuj komentarz przy `apply_event`: krok runnera pokazuje odczyt okresowy.

**Patterns to follow:** istniejące `patch_row` i `replace_reported_attention` w `src/tui/task_app/list.rs`.

**Test scenarios:**
- Covers R5. Raport tylko z nowym `event_cursor`: brak żądania odczytu (test z U1).
- Zmiana `execution_status` (Running → Completed): `refresh_wanted` jest prawdziwe. Istniejący test przejść stanów (`tests/tui_state.rs`, ok. linii 1843) nadal przechodzi.
- Raport z nowym `pending_input_id` (pytanie): odczyt żądany.
- Raport z inną listą `attention`: odczyt żądany.
- Wiersz bez `runner`: odczyt żądany (bez zmiany).
- Seria 10 raportów co 1 s z samym zmieniającym się `event_cursor`: `RefreshSchedule` nie ma żądania, `next_due` wskazuje okres 10 s od ostatniego odczytu. Test na poziomie `TaskApp` (`app.refresh_wanted()` i `app.next_refresh_due()`).
- Odczyt ręczny (`r`) i odczyt po błędzie działają jak dotąd (istniejące testy, bez zmian).

**Verification:** Cały `tests/tui_state.rs` przechodzi. Podczas pracy zadania bez zmian stanu lista jest czytana co 10 s.

---

### U3. Dokumentacja

**Goal:** Opis zgodny z nowym zachowaniem.

**Requirements:** R2, R5.

**Dependencies:** U2.

**Files:**
- `README.md` (akapit „Connection.”, ok. linii 624)

**Approach:** Dopisz jedno zdanie: raport runnera, który nie zmienia stanu, pytania ani uwagi, nie wywołuje odczytu. Krok zadania odświeża się co `tui_refresh_interval`. Nie zmieniaj opisu klucza w tabeli konfiguracji.

**Test expectation:** none — zmiana dokumentacji.

**Verification:** README i komentarz w `list.rs` opisują to samo zachowanie.

---

## Weryfikacja ręczna (po U2)

Uruchom TUI z zadaniem, które pracuje (runner raportuje co 1 s). Znacznik odczytu w panelu zadań ma się pojawiać co 10 s, nie co ok. 1 s. Zmień `tui_refresh_interval` (np. na `5s`) i sprawdź, że znacznik idzie za nową wartością. Jeśli znacznik nadal pojawia się co ok. 1 s, przyczyna jest inna: wróć do gałęzi `_ => true` w `patch_row` i sprawdź, które zdarzenia przychodzą w normalnej pracy. Opisz wynik użytkownikowi, nie zgaduj.

## Risks

- **Przyczyna potwierdzona tylko z kodu.** Łagodzi ją U1 (test przed zmianą) i weryfikacja ręczna. Ostateczne potwierdzenie wymaga człowieka z działającym TUI.
- **Wiersz pokaże stary krok do 10 s.** To zamierzony skutek. Operator, który chce szybciej, ustawia mniejszy `tui_refresh_interval` lub używa `r`.
- **Pominięta zmiana, której zdarzenie nie niesie.** Porównanie obejmuje wszystkie pola raportu, które wpływają na wiersz. Implementer sprawdza w `patch_row`, czy `RunnerObservation` nie ma pola, które wiersz pokazuje, a którego zdarzenie nie niesie.

## Deferred to Implementation

- Dokładna postać porównania `attention` (kolejność elementów, części kontrolne).
- Czy inne zdarzenia z gałęzi `_ => true` są częste w normalnej pracy. Rozstrzyga weryfikacja ręczna.
