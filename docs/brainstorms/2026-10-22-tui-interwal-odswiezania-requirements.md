---
date: 2026-10-22
topic: tui-interwal-odswiezania
---

# TUI: rzadszy odczyt stanu zadań (10 s) z ustawieniem w konfiguracji

## Summary

Prośba: TUI ma odczytywać listę zadań rzadziej, co 10 s, a odstęp ma być ustawiany w pliku konfiguracyjnym narsora. Ustawienie `tui_refresh_interval` istnieje i ma w kodzie wartość domyślną 10 s. Użytkownik potwierdził jednak, że znacznik odczytu w panelu zadań pojawia się mniej więcej co sekundę. Te odczyty listy mają przejść pod okres `tui_refresh_interval`. Przyczyna odczytów co ok. 1 s nie jest jeszcze ustalona.

---

## Problem Frame

Operator chce mniej odczytów z serwera w tle. Sprawdzenie kodu pokazało, że ustawienie już istnieje:

- `crates/narsor-shared/src/config/settings.rs`: `DEFAULT_TUI_REFRESH_SECS = 10` i klucz `TuiRefreshInterval` (`tui_refresh_interval`).
- `crates/narsor-shared/src/config/resolve.rs`: `tui_refresh_interval()` zwraca wartość po kaskadzie konfiguracji.
- `src/cli/commands.rs` przekazuje wartość do `crate::tui::run`. `src/tui/refresh.rs` (`RefreshSchedule`) używa jej jako okresu odczytu.
- `README.md` opisuje ustawienie: `tui_refresh_interval: 10s`, zmienna `NARSOR_TUI_REFRESH_INTERVAL`, `none` wyłącza odczyt okresowy.

Mimo to użytkownik widzi w panelu zadań znacznik odczytu co ok. 1 s. Znaczy to, że jakiś odczyt listy nie idzie przez `RefreshSchedule` albo wyzwala się częściej niż okres. Kandydaci do sprawdzenia przy planowaniu:

- odpytywanie zakładki Conductor co 2 s, gdy czeka na odpowiedź (`src/tui/task_app/tabs/conductor.rs:80`);
- ponowienia po błędzie: `FIRST_RETRY` 1 s i `READER_RETRY` 2 s;
- odczyty wywołane zdarzeniami na żywo (`DEBOUNCE` 400 ms);
- sprawdzanie strumienia co 1 s (`STREAM_RECHECK_INTERVAL`).

Lista nie jest zweryfikowana. To wskazówki, nie ustalona przyczyna.

---

## Requirements

- R1. TUI odczytuje listę zadań z serwera co `tui_refresh_interval`, gdy nic innego nie wymusza odczytu. Wartość domyślna to 10 s.
- R2. Odstęp ustawia się w pliku konfiguracyjnym narsora. Kolejność źródeł i podgląd źródła są takie same jak dla innych ustawień. Zmienna środowiskowa `NARSOR_TUI_REFRESH_INTERVAL` też działa.
- R3. Wartość `none` wyłącza odczyt okresowy.
- R4. Odczyty wywołane zdarzeniem na żywo (po krótkim opóźnieniu) i ponowienia po błędzie zostają bez zmian, o ile nie są przyczyną odczytów co ok. 1 s. Odczyt ręczny (`r`) działa jak dotąd.
- R5. W trakcie normalnej pracy, bez błędów, TUI nie odczytuje listy zadań częściej niż co `tui_refresh_interval`. Znacznik odczytu w panelu zadań pojawia się wtedy co ten okres. Wyjątek: odczyty z R4.

R1–R4 są spełnione w kodzie, ale R5 nie jest, według obserwacji użytkownika. Praca nad kodem polega na znalezieniu odczytu listy co ok. 1 s i podporządkowaniu go okresowi z R1.

---

## Scope Boundaries

- Bez zmiany nazwy klucza i formatu wartości. Wartość domyślna zostaje 10 s.
- Bez zmiany odstępów innych niż odczyt listy: opóźnienie zdarzeń, ponowienia po błędzie i ticki animacji zostają, chyba że okażą się przyczyną odczytów co ok. 1 s.
- Zakres obejmuje tylko listę zadań. Odczyty szczegółów zaznaczonego zadania i rozmowy zostają bez zmian.
- Bez nowego ustawienia dla odczytów szczegółów lub rozmowy.

---

## Założenia do potwierdzenia

- Z1. „Stan wszystkich zadań" oznacza odczyt listy zadań w TUI. Szczegóły, historia i rozmowa nie wchodzą w zakres. Potwierdzone przez użytkownika.
- Z2. Obecne ustawienie `tui_refresh_interval` jest tym ustawieniem, o które chodzi. Nowy klucz nie jest potrzebny.
- Z3. Wartość domyślna to 10 s. Użytkownik prosi o 10 s, a kod już ją ma. Skutek zmiany to usunięcie odczytów co ok. 1 s, nie zmiana liczby w ustawieniach.
- Z4. Odczyty wywołane zdarzeniami na żywo zostają bez zmian, o ile nie powodują odczytów co ok. 1 s.

---

## Pytania otwarte

- P1. Który odczyt powoduje znacznik co ok. 1 s w panelu zadań? Do ustalenia przy planowaniu przez sprawdzenie kandydatów z sekcji „Problem Frame".
