---
date: 2026-10-24
topic: tlo-aktywnego-pola-formularza
---

# TUI: tło aktywnego pola na całej szerokości pola

## Summary

Aktywne pole formularza TUI ma tło `field_editing` na całej szerokości pola: od znaku `>` do ostatniej komórki przed końcem formularza. Dziś tło widać tylko pod wpisanym tekstem. Gdy tekst się zawija lub operator dodaje nową linię, tło obejmuje także każdą nową linię pola.

---

## Problem Frame

Wymaganie R8 z `docs/brainstorms/2026-10-22-tui-edycja-formularzy-requirements.md` mówi, że pole w edycji ma zmienione tło (granat). Kod ma ten styl (`field_editing` w `src/tui/style.rs`). Funkcja `value_lines` w `src/tui/form/field.rs` nakłada go na każdą linię pola. Mimo to operator widzi tło tylko pod wpisanym tekstem. Pusta część linii za tekstem ma tło standardowe. Pole wygląda jak podświetlony tekst, nie jak pole.

Przyczyna jest niezweryfikowana (Z1). Wymaganie opisuje skutek, nie naprawę.

---

## Actors

- A1. Operator: edytuje pola formularzy TUI i chce od razu widzieć, które pole jest aktywne i gdzie ono się kończy.

---

## Key Flows

- F1. Wejście do pola
  - **Trigger:** Focus przechodzi na pole.
  - **Steps:** Pole dostaje tło edycji od `>` do końca szerokości pola, także gdy jest puste.
  - **Outcome:** Operator widzi pełny prostokąt pola.
  - **Covered by:** R1, R2

- F2. Wpisywanie wielu linii
  - **Trigger:** Operator dodaje nową linię (`Ctrl+J`) albo tekst zawija się na szerokości pola.
  - **Steps:** Pole zyskuje linię. Nowa linia ma tło na całej szerokości od razu, w tej samej klatce.
  - **Outcome:** Prostokąt tła rośnie razem z tekstem, bez luk.
  - **Covered by:** R3

---

## Requirements

- R1. Aktywne pole ma tło edycji na całej szerokości pola. Zakres: od kolumny znaku `>` do ostatniej komórki przed końcem formularza (spacja przed końcem pola jest w tle). Dotyczy komórek za tekstem, nie tylko pod nim.
- R2. Puste aktywne pole też ma tło na całej szerokości swojej linii.
- R3. Pole wielolinijkowe ma tło na całej szerokości w każdej linii: w liniach z nowej linii (`\n`), w liniach z zawinięcia i w linii z kursorem na końcu tekstu. Dodana linia dostaje tło od razu.
- R4. Pole, które nie jest aktywne, nie ma tła edycji. Tło znika z całego pola, gdy focus je opuści.
- R5. Reguły R1–R4 dotyczą każdego rodzaju pola w formularzu, także pól wyboru (`‹ wartość ›`) i pól liczbowych. Dotyczy to każdego formularza, który używa wspólnego silnika.
- R6. Tło pola z paska krótkich pól (`src/tui/chooser/inputs.rs`) obejmuje całe miejsce, które ma pole w pasku. Ma ten sam kolor co w polach pełnych.
- R7. Kolor tła, motywy i tryb bez koloru zostają bez zmian (`field_editing` ze wspólnego stylu). Bez koloru pole nadal odróżnia `>` w roli focus.
- R8. Kursor (odwrócona komórka) nadal jest widoczny na tle edycji. Tło nie zmienia położenia tekstu ani kursora.

---

## Acceptance Examples

- AE1. **Covers R1, R2.** Given formularz o szerokości 40 komórek i puste aktywne pole, when formularz się rysuje, then wszystkie komórki linii pola od `>` do przedostatniej (włącznie ze spacją przed końcem) mają tło edycji.
- AE2. **Covers R1.** Given pole z tekstem `abc`, when jest aktywne, then tło obejmuje także komórki za `c` do końca pola.
- AE3. **Covers R3.** Given pole z tekstem `abc`, when operator naciska `Ctrl+J`, then pole ma dwie linie i druga linia ma tło na całej szerokości.
- AE4. **Covers R3.** Given tekst dłuższy niż szerokość pola, when zawija się na trzy linie, then każda z trzech linii ma tło na całej szerokości.
- AE5. **Covers R4.** Given aktywne pole z tłem, when focus przechodzi na pole niżej, then poprzednie pole nie ma tła, a nowe ma tło na całej szerokości.
- AE6. **Covers R5.** Given aktywne pole wyboru, when jest narysowane, then tło obejmuje całą szerokość linii, nie tylko `‹ wartość ›`.

---

## Success Criteria

- Operator widzi aktywne pole jako pełny prostokąt tła, także puste i wielolinijkowe.
- Planowanie nie musi zgadywać, od której do której komórki sięga tło.

---

## Scope Boundaries

- Nie zmieniamy koloru tła, motywów ani trybu bez koloru.
- Nie zmieniamy zawijania, kursora, nawigacji ani potwierdzenia formularza.
- Nie zmieniamy tła poza aktywnym polem (ramki, nagłówki, inne pola).
- Nie obejmuje pól poza formularzami, np. pola rozmowy z agentem, chyba że używa tego samego wspólnego rysowania pól.

---

## Key Decisions

- Poprawka jest w jednym wspólnym miejscu rysowania pól, nie osobno w każdym formularzu. Zgodnie z R16 z `docs/brainstorms/2026-10-22-tui-edycja-formularzy-requirements.md`.
- Ten dokument doprecyzowuje R8 z `2026-10-22-tui-edycja-formularzy`: tło ma sięgać całej szerokości pola.

---

## Dependencies / Assumptions

- Styl `field_editing` i funkcja `value_lines` istnieją (zweryfikowano w `src/tui/style.rs` i `src/tui/form/field.rs`).

---

## Założenia do potwierdzenia

- Z1. Przyczyna to sposób, w jaki linia przenosi tło: tło dochodzi tylko do końca tekstu linii, nie do końca obszaru. Nie zweryfikowano. Planowanie potwierdza przyczynę.
- Z2. „Spacja przed końcem pola" oznacza ostatnią komórkę pola wg R7 z `2026-10-22-tui-edycja-formularzy` (pole kończy się 1 znak przed końcem formularza). Tło sięga tej komórki włącznie, nie dalej.
- Z3. Pola wyboru i liczbowe mają ten sam wygląd aktywnego pola co pola tekstowe (R5).
- Z4. Pasek krótkich pól (R6) ma tło na całym miejscu pola, nie na całym pasku.

---

## Pytania otwarte dla użytkownika

- P1. Czy tło ma obejmować także dwie komórki wcięcia (`> `), czy tylko miejsce od tekstu do końca pola? Przyjęto: od znaku `>` (zgodnie z opisem w poleceniu).
- P2. Czy pole wyboru ma mieć tło na całej szerokości, czy tylko pod `‹ wartość ›`? Przyjęto: na całej szerokości (Z3).
