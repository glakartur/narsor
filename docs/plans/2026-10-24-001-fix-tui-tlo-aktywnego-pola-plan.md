---
title: "fix: Tło aktywnego pola formularza TUI na całej szerokości pola"
type: fix
status: active
date: 2026-10-24
origin: docs/brainstorms/2026-10-24-tlo-aktywnego-pola-formularza-requirements.md
---

# fix: Tło aktywnego pola formularza TUI na całej szerokości pola

## Summary

Tło `field_editing` ma pokrywać dokładnie komórki tekstu aktywnego pola: od pierwszej komórki za `> ` do ostatniej komórki tekstu (`text_width`). Wcięcie `> ` i margines po prawej nie mają tła. Naprawa jest w dwóch miejscach: `value_lines` w `src/tui/form/field.rs` (pola pełne, wspólny silnik) i `render_bar` w `src/tui/chooser/inputs.rs` (pasek krótkich pól). Kolor, motywy, zawijanie i kursor zostają bez zmian (see origin: `docs/brainstorms/2026-10-24-tlo-aktywnego-pola-formularza-requirements.md`).

---

## Problem Frame

Przyczyna (Z1 z origin) jest potwierdzona w źródłach ratatui 0.30.2 (`ratatui-widgets-0.3.2/src/paragraph.rs`, `render_paragraph`): `Paragraph` rysuje tylko grafemy linii. Styl ustawiony przez `Line::style` trafia więc tylko do komórek, w których stoi tekst. Pusta część linii za tekstem zostaje bez tła. Jest też skutek uboczny: `> ` jest tekstem linii, więc dziś dostaje tło, a R1 tego zabrania.

Dziś `value_lines` robi `Line::from(spans).style(fill)`. To nie wystarcza. Tło musi być w spanach: tekst dostaje styl wypełnienia, a reszta do `text_width` jest dopełniona spacjami w tym samym stylu.

Pole kończy się 1 komórkę przed końcem obszaru (`FIELD_RIGHT_MARGIN` w `src/tui/form/layout.rs`). Tło sięga ostatniej komórki tekstu włącznie (`text_width(area_width)` komórek po `> `) i nie obejmuje marginesu. Tak mówi AE1 (do „przedostatniej" komórki). To rozstrzyga Z2.

---

## Requirements Trace

| Wymaganie | Jednostka |
|---|---|
| R1, R2, R3, R4 | U1 |
| R5 (pola wyboru, liczbowe, każdy formularz na silniku) | U1 |
| R6 (pasek krótkich pól) | U2 |
| R7, R8 (kolor, kursor, bez zmian) | U1, U2 (testy regresji) |
| AE1–AE6 | U1 |

Pola liczbowe używają tej samej gałęzi co tekst w `value_lines`. Pole wyboru ma własną gałąź i dostaje to samo dopełnienie.

---

## Scope Boundaries

- Bez zmiany koloru `field_editing`, motywów i trybu bez koloru.
- Bez zmiany zawijania (`wrap`), kursora, nawigacji i potwierdzenia.
- Bez tła w polach nieaktywnych, w ramkach i nagłówkach. Pola złożone (`folded_rows`, `prefixed`) nie mają tła i nie zmieniają się.
- Pole rozmowy z agentem: nie jest objęte, chyba że wywołuje `value_lines`. Kod wywołań (`render_fields` w `src/tui/task_app/waiting.rs` i `src/tui/task_app/tabs/conductor.rs`) idzie przez `value_lines`, więc dostaje poprawkę bez osobnej zmiany.

### Deferred to Follow-Up Work

- Szerokość komórki znaku (`cell_width` zwraca dziś 1). Dopełnienie liczy komórki przez `cell_width`, więc zmiana tej funkcji obejmie je automatycznie.

---

## Key Technical Decisions

- **Tło w spanach, nie w stylu linii.** `Line::style` nie wypełnia komórek bez tekstu w `Paragraph`. Prompt dostaje tylko styl roli (`Focus` albo `Done`), tekst i dopełnienie dostają `fill`.
- **Jedna funkcja dopełnienia w `field.rs`**, używana przez gałąź tekstu i gałąź wyboru. Liczy komórki zajęte przez tekst (`cell_width`) i dodaje spację w stylu `fill` do `text_width`. Nie ma osobnej logiki w formularzach (R16 z `2026-10-22-tui-edycja-formularzy`).
- **Kursor na końcu pełnego wiersza** ma własny wiersz (`wrap`, `needs_own_row`), więc nigdy nie wypada w marginesie. Dopełnienie nie potrzebuje wyjątku dla kursora. Kursor w pustej linii to odwrócona spacja na początku tekstu, reszta jest dopełniona.
- **Pasek krótkich pól:** dopełnienie do `reserved` komórek (miejsce, które pole ma w pasku, Z4). Pole w pasku ma styl `Focus` plus `field_editing` jak dziś, dopełnienie ma sam `field_editing`.

---

## Implementation Units

### U1. Pełna szerokość tła w `value_lines`

**Goal:** Aktywne pole (tekst, liczba, wybór, puste, wielolinijkowe) ma tło od pierwszej komórki tekstu do ostatniej komórki `text_width`. Prompt nie ma tła.

**Requirements:** R1, R2, R3, R4, R5, R7, R8; AE1–AE6.

**Dependencies:** brak.

**Files:**
- Modify: `src/tui/form/field.rs`
- Test: `tests/tui_form.rs` (sekcja „the look") albo `tests/tui_render.rs`, tam gdzie jest już `draw_chooser` i dostęp do bufora

**Approach:**
- W `value_lines` usunąć `.style(fill)` z `Line`. Prompt zostaje ze stylem roli.
- Tekst wiersza i kursor dostają `fill` jako `base` (`cursor_spans` już przyjmuje `base`). Gałąź nieaktywna używa `Style::default()`, więc nic się nie zmienia.
- Po tekście dodać span dopełnienia: tyle spacji w stylu `fill`, ile brakuje do `text_width(width)` komórek. Dopełnienie tylko gdy `focused`.
- Gałąź `Select`: tekst `‹ wartość ›` dostaje `fill`, potem to samo dopełnienie. Szerokość wartości policzona przez tę samą funkcję.
- Zbyt długi tekst wyboru (więcej niż `text_width`): dopełnienie wynosi zero, `Paragraph` przycina na brzegu obszaru jak dziś.
- Wysokość pola (`value_rows`) i położenie tekstu oraz kursora nie zmieniają się. Dopełnienie dodaje tylko komórki po tekście.

**Patterns to follow:** `cursor_spans` i `layout_of` w `src/tui/form/field.rs`. `text_width`, `cell_width`, `PROMPT_WIDTH` z `src/tui/form/layout.rs`.

**Test scenarios:** (bufor z `draw_chooser`, `Styles::new(false)`, odczyt `bg` komórek wiersza pola)
- Covers AE1. Puste aktywne pole, szerokość 40: komórki 2..=38 mają `bg` z `field_editing`; komórki 0–1 (`> `) i komórka 39 nie mają.
- Covers AE2. Tekst `abc`: komórki za `c` do komórki 38 mają tło, komórki pod tekstem też.
- Covers AE3. Tekst `abc` plus `Ctrl+J`: dwa wiersze, drugi ma tło 2..=38 i nie ma tła w komórkach 0–1.
- Covers AE4. Tekst dłuższy niż `text_width`, zawija się na trzy wiersze: każdy wiersz ma tło 2..=38.
- Linia pusta w środku tekstu (`a\n\nb`): wiersz pusty ma tło w całym zakresie.
- Kursor na końcu pełnego wiersza (własny wiersz): wiersz kursora ma tło, kursor jest odwróconą komórką 2.
- Covers AE5. Dwa pola, focus przechodzi niżej: pole poprzednie nie ma żadnej komórki z tłem edycji, nowe ma tło w całym zakresie.
- Covers AE6. Aktywne pole wyboru: tło 2..=38, `> ` bez tła.
- Pole liczbowe aktywne: to samo co pole tekstowe.
- Układ „inputs" (`with_inputs_layout`) i układ zwykły: oba mają tło pola głównego (ta sama funkcja `value_lines`).
- R8: kursor nadal jest jedyną komórką z `Modifier::REVERSED`, w tym samym miejscu co przed zmianą (istniejące testy `cursor_follows` zostają zielone).
- R7: `Styles::new(true)` (bez koloru): żadna komórka pola nie ma `bg` różnego od `Reset`; `>` nadal ma styl `Focus`.
- Test w `tests/tui_render.rs` o braku `.bg(` bez `.fg(` (około linii 866) nadal przechodzi: nie dodajemy nowego `.bg(` w kodzie.

**Verification:** wszystkie wiersze aktywnego pola mają tło w tym samym zakresie komórek. Wcięcie `> ` i margines nie mają tła. Istniejące testy kursora i zawijania przechodzą bez zmian.

---

### U2. Tło pola w pasku krótkich pól

**Goal:** Aktywne pole w pasku (`name: value`) ma tło na całym miejscu, które zajmuje w pasku (`reserved`), nie tylko pod tekstem.

**Requirements:** R6, R7, R8.

**Dependencies:** U1 (wspólna funkcja dopełnienia, jeśli da się ją udostępnić; inaczej mały lokalny odpowiednik w `inputs.rs` dla stylu `field_editing`).

**Files:**
- Modify: `src/tui/chooser/inputs.rs`
- Test: `tests/tui_form.rs` albo `tests/tui_render.rs`

**Approach:**
- W `render_bar`, w gałęzi pola z focusem, po `cursor_spans(...)` policzyć komórki spanów i dopełnić do `reserved` spacjami w stylu `field_editing` (bez stylu `Focus`: tylko tło).
- Okno `window_around_cursor` zostaje bez zmian. Tekst nie przekracza `reserved`, więc dopełnienie jest zero lub dodatnie.
- Inne pola paska i separatory zostają bez tła.

**Patterns to follow:** `render_bar`, `window_around_cursor` w `src/tui/chooser/inputs.rs`.

**Test scenarios:**
- Pasek z dwoma krótkimi polami, aktywne pierwsze, szerokie okno: tło sięga od początku miejsca pola do `reserved` komórek; komórki drugiego pola i separatora nie mają tła.
- Aktywne pole krótkie z krótką wartością: dopełnienie jest widoczne za tekstem (komórki za wartością mają tło).
- Aktywne pole z wartością dłuższą niż `reserved`: tło obejmuje całe `reserved`, kursor nadal widoczny (jedna komórka `REVERSED`).
- Pole wyboru w pasku, aktywne: tło na całym miejscu pola.
- Bez koloru (`Styles::new(true)`): brak tła.

**Verification:** tło pola w pasku kończy się dokładnie na końcu miejsca pola. Kolor jest ten sam co w polach pełnych (`styles.field_editing()`).

---

## Risks

- **Podwójne liczenie szerokości.** Dopełnienie musi używać `text_width(width)`, tej samej funkcji co `wrap`. Inaczej tło i zawijanie się rozjadą. Mitigacja: jedna funkcja i test z szerokością graniczną (np. szerokość 4 lub mniejsza, gdy `text_width` zwraca `max(1)`).
- **Wąski obszar.** Gdy `area_width < 3`, `text_width` wynosi 1, a prompt może wyjść poza obszar. `Paragraph` przycina, nic nie panikuje. Test z bardzo wąskim obszarem potwierdza brak paniki.
- **Istniejące testy kolorów.** Testy w `tests/tui_render.rs` sprawdzają, że poza wypełnieniem zaznaczenia nie ma tła. Pole aktywne jest nowym miejscem z tłem. Sprawdzić, że testy nie rysują aktywnego pola w trybie z kolorem, albo ich oczekiwania uwzględniają `field_editing`.

---

## Deferred to Implementation

- Czy funkcja dopełnienia jest `pub(crate)` w `field.rs` i używana też w `inputs.rs`, czy `inputs.rs` ma własną krótką wersję. Zależy od kształtu kodu po U1.
- Dokładne położenie testów (`tests/tui_form.rs` albo `tests/tui_render.rs`). Wybrać plik, który ma już helpery do odczytu `bg` z bufora.

---

## Origin Coverage

- Z1: potwierdzone (Problem Frame). Z2: rozstrzygnięte przez AE1 (tło do `text_width`, bez marginesu). Z3: ta sama funkcja dla wszystkich rodzajów pól. Z4: `reserved` w U2.
- Scope Boundaries z origin przeniesione bez zmian. Brak pytań blokujących.
