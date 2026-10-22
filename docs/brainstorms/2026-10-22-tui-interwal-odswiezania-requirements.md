---
date: 2026-10-22
topic: tui-interwal-odswiezania
---

# TUI: rzadszy odczyt stanu zadań (10 s) z ustawieniem w konfiguracji

## Summary

Prośba: TUI ma odczytywać stan wszystkich zadań rzadziej, co 10 s, a odstęp ma być ustawiany w pliku konfiguracyjnym narsora. Kod już to robi: ustawienie `tui_refresh_interval` ma wartość domyślną 10 s i działa przez plik konfiguracyjny, zmienną środowiskową i `none`. Ten dokument nie dodaje nowego zachowania. Zapisuje wynik sprawdzenia i wskazuje, co zostaje do potwierdzenia.

---

## Problem Frame

Operator chce mniej odczytów z serwera w tle. Sprawdzenie kodu pokazuje, że wymaganie jest już spełnione:

- `crates/narsor-shared/src/config/settings.rs`: `DEFAULT_TUI_REFRESH_SECS = 10` i klucz `TuiRefreshInterval` (`tui_refresh_interval`).
- `crates/narsor-shared/src/config/resolve.rs`: `tui_refresh_interval()` zwraca wartość po kaskadzie konfiguracji.
- `src/cli/commands.rs` przekazuje wartość do `crate::tui::run`. `src/tui/refresh.rs` (`RefreshSchedule`) używa jej jako okresu odczytu.
- `README.md` opisuje ustawienie: `tui_refresh_interval: 10s`, zmienna `NARSOR_TUI_REFRESH_INTERVAL`, `none` wyłącza odczyt okresowy.

Możliwe, że prośba powstała ze starszego stanu kodu, albo że chodzi o inny odczyt niż okresowy odczyt listy zadań. Tego nie da się rozstrzygnąć z kodu.

---

## Requirements

- R1. TUI odczytuje stan wszystkich zadań z serwera co 10 s, gdy nic innego nie wymusza odczytu.
- R2. Odstęp ustawia się w pliku konfiguracyjnym narsora. Kolejność źródeł i podgląd źródła są takie same jak dla innych ustawień.
- R3. Wartość `none` wyłącza odczyt okresowy.
- R4. Odczyty wywołane zdarzeniem na żywo (po krótkim opóźnieniu) i ponowienia po błędzie zostają bez zmian. Dotyczy tylko odczytu okresowego.

R1–R4 są dziś spełnione. Praca nad kodem jest potrzebna tylko wtedy, gdy któraś odpowiedź w sekcji „Pytania otwarte" wskaże lukę.

---

## Scope Boundaries

- Bez zmiany wartości domyślnej, nazwy klucza i formatu wartości.
- Bez zmiany odstępów innych niż odczyt okresowy (opóźnienie zdarzeń, ponowienia po błędzie, ticki animacji).
- Bez nowego ustawienia dla odczytów szczegółów lub rozmowy.

---

## Założenia do potwierdzenia

- Z1. „Stan wszystkich zadań" oznacza okresowy odczyt listy zadań w TUI (`RefreshSchedule`), a nie odczyty szczegółów, historii lub rozmowy.
- Z2. Obecne ustawienie `tui_refresh_interval` jest tym ustawieniem, o które chodzi. Nowy klucz nie jest potrzebny.
- Z3. Domyślne 10 s jest wartością docelową. Użytkownik nie oczekuje innej wartości domyślnej.
- Z4. Odczyty wywołane zdarzeniami na żywo nie są „odczytem rzadziej" i zostają bez zmian.

---

## Pytania otwarte dla użytkownika

- P1. Czy zaobserwowano w TUI odczyty częstsze niż co 10 s? Jeśli tak: w jakiej wersji i w jakim widoku? Wtedy należy sprawdzić, który odczyt je powoduje (np. zdarzenia na żywo, ponowienia po błędzie, odczyt szczegółów zaznaczonego zadania).
- P2. Czy prośba dotyczy też odczytów szczegółów zaznaczonego zadania, a nie tylko listy?
- P3. Czy wystarczy, że ustawienie już istnieje i jest opisane w README, czy trzeba coś dodać (np. zmienić domyślny odstęp albo opis)?
