---
date: 2026-10-20
topic: tui-zaokraglone-ramki
---

# Zaokrąglone ramki paneli w TUI

## Summary

Ramki paneli w TUI dostają zaokrąglone narożniki (`╭ ╮ ╰ ╯`). Zmiana jest czysto wizualna. Dziś panel z focusem ma gruby obrys (`┏ ┓ ┗ ┛`). Zaokrąglenie nie ma wersji grubej. Dlatego focus trzeba pokazać inaczej niż grubością linii.

---

## Problem Frame

Panele TUI mają ostre narożniki (`┌ ┐ └ ┘`). Panel z focusem ma gruby obrys (`┏ ┓ ┗ ┛`), a inne panele cienki. Artur chce łagodniejszego wyglądu z narożnikami typu `╰`.

Stan kodu (sprawdzony): ramki paneli tworzy jedna funkcja `Styles::panel` w `src/tui/style.rs`. Wybiera ona `BorderType::Thick` albo `BorderType::Plain`. Cztery miejsca omijają tę funkcję i budują ramkę ręcznie z `Borders::ALL`: `src/tui/chooser/inputs.rs`, `src/tui/task_app/actions.rs`, `src/tui/chooser.rs`, `src/tui/keymap.rs`. Biblioteka `ratatui` 0.30 ma gotowy typ `BorderType::Rounded`.

Problem projektowy: rozróżnienie „focus / brak focusa" opiera się dziś na grubości linii. Komentarz w `style.rs` mówi wprost, że bez koloru grubość i pogrubiony tytuł wskazują panel z klawiaturą. Rounded nie ma grubej odmiany. Samo podmienienie typu ramki odebrałoby ten sygnał trybowi bez koloru.

---

## Requirements

- R1. Wszystkie ramki paneli używają zaokrąglonych narożników. Dotyczy to też czterech miejsc z ręcznie budowaną ramką.
- R2. Panel z focusem nadal wyraźnie różni się od panelu bez focusa, także bez koloru. Sygnał focusa nie opiera się już na grubości linii.
- R3. Zaznaczenie elementów (poziome linie i pionowy pasek `─` / `│`) nie zmienia się. Pozostaje lżejsze niż ramka panelu.
- R4. Zmiana nie przesuwa układu. Zaokrąglone narożniki zajmują tę samą komórkę co ostre.
- R5. Układ, skróty klawiszowe i zachowanie TUI nie zmieniają się.

## Acceptance Examples

- AE1. **Covers R1.** Dane: dowolny widok z panelami (lista zadań, szczegóły, akcje). Gdy TUI się rysuje, każdy panel ma narożniki `╭ ╮ ╰ ╯`. Żaden panel nie ma `┌ ┐ └ ┘` ani `┏ ┓ ┗ ┛`.
- AE2. **Covers R2.** Dane: terminal bez kolorów. Gdy focus przechodzi z listy do szczegółów, operator widzi po samym kształcie lub stylu, który panel ma focus.
- AE3. **Covers R3, R4.** Dane: wybrany element na liście. Zaznaczenie wygląda jak dziś, a panel zajmuje ten sam obszar.

---

## Scope Boundaries

- Brak zmiany kolorów ról (focus, tło, problem).
- Brak zmiany znaków zaznaczenia (`─`, `│`).
- Brak nowych motywów ani opcji konfiguracji wyglądu ramek.
- Brak zmian w układzie paneli.

---

## Key Decisions

- Wszystkie ramki są zaokrąglone, bez wyjątków. Jedna spójna reguła jest prostsza niż mieszanie stylów. Zgodne z prośbą „zaokrąglone ramki w TUI".
- Focus pokazuje kolor roli *Focus* (cyan) plus pogrubiony tytuł. Brak koloru obsługuje dodatkowy znacznik w tytule (np. przedrostek `▸`). Wybór dokładnego znacznika należy do planowania. Zasada: focus musi być czytelny bez koloru (R2).

## Dependencies / Assumptions

- Założenie (do potwierdzenia przez Artura): Artur akceptuje utratę grubego obrysu focusa. Alternatywa: zachować obrys gruby tylko dla panelu z focusem. Wtedy panel z focusem miałby ostre narożniki `┏ ┓ ┗ ┛`, a pozostałe zaokrąglone. To miesza style i odbiega od prośby.
- Założenie: terminal operatora wyświetla znaki `╭ ╮ ╰ ╯`. Są to standardowe znaki Unicode (Box Drawing). Obsługuje je większość czcionek terminalowych. Nie sprawdzono tego w środowiskach operatorów.

## Outstanding Questions

### Resolve Before Planning

- [Affects R2] Czy Artur woli znacznik w tytule (`▸`) jako sygnał focusa bez koloru, czy akceptuje sam kolor i pogrubienie?

### Deferred to Planning

- [Affects R2][Technical] Dokładna forma znacznika focusa w tytule i jego wpływ na szerokość tytułu.
- [Affects R1][Technical] Testy w `tests/tui_render.rs` (stała `THICK`, asercje na `┏ ┓ ┌ ┐`) zakładają obecne znaki i wymagają aktualizacji.
- [Affects R1][Technical] Czy cztery ręczne ramki warto przenieść do `Styles::panel`, czy tylko zmienić ich typ.
