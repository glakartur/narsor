---
date: 2026-10-24
topic: formularz-nowego-zadania-etykieta-nad-polem
---

# Formularz nowego zadania: etykieta nad polem wartości

## Summary

Każde pole formularza nowego zadania ma własny wiersz z etykietą i opisem, a pod nim wiersz z wartością. Wiersz wartości zaczyna się od `> `, a po jego prawej stronie zostaje 1 spacja do ramki. Pole wyboru (`‹ wartość ›`) ma ten sam układ. Gdy focus jest na liście workflow, prawy panel pokazuje tylko opis workflow, bez pól Ticket i Tytuł. Etykiety zmieniają się: `target_repo` → „Repozytorium", `feature_description` → „Opis".

---

## Problem Frame

Dziś pole krótkie to jeden wiersz `nazwa: wartość` (`short_line` w `src/tui/chooser/inputs.rs`). Opis pola krótkiego pokazuje się osobno, w linii statusu nad kolumną, tylko dla pola z focusem. Pole wieloliniowe ma już nagłówek z opisem obok etykiety (`header_line`) i wartość z prefiksem `> ` pod spodem (`prefixed`, `value_lines`). Pola krótkie i wieloliniowe wyglądają więc inaczej i operator czyta dwa układy w jednym formularzu.

Gdy focus jest na liście workflow, pola Ticket i Tytuł rysują się pod listą i opisem (`field_lines` w `src/tui/chooser.rs`). Operator wybiera wtedy workflow, a widzi pola, których jeszcze nie wypełnia.

Etykiety pochodzą z nazw wejść workflow (`target_repo`, `feature_description` w `workflows/compound-engineering.yaml`). Operator czyta nazwy techniczne w języku angielskim, a reszta formularza (Ticket, Tytuł, Opis) jest po polsku.

---

## Requirements

**Układ pola**
- R1. Każde pole formularza nowego zadania ma dwa wiersze: wiersz etykiety i pod nim wiersz wartości. Dotyczy pól tekstowych jednolinijkowych, liczb, true/false i pól wyboru.
- R2. Pole wieloliniowe (Opis) zachowuje etykietę nad wartością. Jego wiersze wartości mają ten sam układ co wiersze innych pól (R4, R5).
- R3. Pole wyboru z wartością zmienianą klawiszami `←`/`→` (dziś `‹ wartość ›`) ma etykietę w osobnym wierszu, a wartość pod nią. Znaczniki `‹` `›` zostają przy wartości.
- R4. Wiersz wartości zaczyna się od `> ` (prefiks jak przy wartości pola Opis), potem idzie pole wartości.
- R5. Pole wartości kończy się 1 spacją przed prawą ramką. Tło edycji nie zajmuje tej spacji.
- R6. Tło edycji aktywnego pola obejmuje pole wartości (od miejsca po `> ` do tej 1 spacji). Prefiks `> ` i etykieta nie mają tła edycji. Reguły tła z `docs/brainstorms/2026-10-24-formularz-nowego-zadania-pola-pod-soba-requirements.md` (R7–R11) zostają, z tą zmianą szerokości.
- R7. Błąd pola i licznik znaków (np. `7/15`) zostają przy polu, do którego należą. Planowanie wybiera miejsce w wierszu etykiety lub wartości, byle nie psuły reguł R4 i R5.

**Opis obok etykiety**
- R8. Opis każdego pola stoi w wierszu etykiety, po etykiecie, w formie ` · opis`, jak przy polu Opis (`header_line`). Dotyczy też pól Ticket, Tytuł, Repozytorium i pozostałych wejść workflow.
- R9. Znacznik „(wymagane)" stoi w wierszu etykiety, przed opisem, jak dziś przy polach wieloliniowych.
- R10. Linia statusu nad kolumną nie powtarza opisu pola, bo opis jest już przy etykiecie. Błąd pola krótkiego może zostać w linii statusu albo przejść do pola (wybiera planowanie, R7).
- R11. Za długi opis jest przycinany z `…` do szerokości wiersza etykiety. Etykieta i znacznik „(wymagane)" zawsze się mieszczą.

**Wybór workflow**
- R12. Wybór workflow zostaje listą wartości z opisem wybranego workflow w prawym panelu. Lista i panel opisu nie zmieniają układu.
- R13. Gdy focus jest na liście workflow, pod listą i panelem nie rysują się pola Ticket i Tytuł (dziś: `field_lines`). Pola pojawiają się, gdy operator przejdzie do pól formularza.
- R14. Reguła R13 dotyczy pól opisujących ticket: Ticket i Tytuł. Pozostałe pola workflow też nie rysują się pod listą w tym stanie, jeśli dziś się tam rysują (formularz pokazuje jedną rzecz naraz: listę z opisem albo pola).
- R15. Nawigacja klawiszami między listą a polami i kolejność pól (Ticket, Tytuł, Repozytorium, pozostałe wejścia, Opis) nie zmieniają się.

**Etykiety**
- R16. Etykieta pola `target_repo` brzmi „Repozytorium". Etykieta pola `feature_description` brzmi „Opis".
- R17. Zmiana dotyczy tylko tego, co operator widzi w formularzu. Nazwa wejścia w pliku workflow, w API, w CLI (`--input target_repo=...`) i w szablonach (`{{ inputs.target_repo }}`) zostaje bez zmian.
- R18. Etykiety Ticket i Tytuł zostają. Etykiety w innych formularzach (odpowiedź, komentarz) zostają, o ile nie używają wspólnego rysowania pól z R1–R11 (wtedy dostają nowy układ, patrz Założenia).

---

## Acceptance Examples

- AE1. **Covers R1, R4, R5, R8.** Given pole Ticket w formularzu o szerokości 60 kolumn, when formularz się rysuje, then wiersz 1 to `Ticket · numer zadania w Jirze, opcjonalnie`, wiersz 2 to `> `, pole wartości i 1 wolna spacja przed prawą ramką.
- AE2. **Covers R3.** Given pole Repozytorium z wartością `acme/web`, when formularz się rysuje, then wiersz 1 to `Repozytorium (wymagane) · opis`, wiersz 2 to `> ‹ acme/web ›`.
- AE3. **Covers R6.** Given aktywne pole Tytuł, when formularz się rysuje, then tło edycji zaczyna się po `> ` i kończy przed ostatnią spacją wiersza. Etykieta nie ma tła.
- AE4. **Covers R12, R13.** Given focus na liście workflow, when formularz się rysuje, then widać listę workflow i prawy panel z opisem workflow. Pól Ticket i Tytuł nie widać.
- AE5. **Covers R16, R17.** Given workflow z wejściami `feature_description` i `target_repo`, when formularz się rysuje, then etykiety to „Opis" i „Repozytorium". Wywołanie `--input target_repo=acme-api` w CLI nadal działa.
- AE6. **Covers R11.** Given opis pola dłuższy niż wolne miejsce w wierszu etykiety, when formularz się rysuje, then opis kończy się `…`, a etykieta jest cała.

---

## Success Criteria

- Wszystkie pola formularza wyglądają tak samo: etykieta z opisem, pod spodem `> ` i pole wartości.
- Operator wybiera workflow bez pól Ticket i Tytuł na ekranie.
- Operator widzi etykiety „Repozytorium" i „Opis" zamiast nazw technicznych.
- Planowanie nie musi zgadywać układu wiersza wartości ani zakresu zmiany etykiet.

---

## Scope Boundaries

- Nie zmieniamy kolejności pól ani nawigacji klawiszami.
- Nie zmieniamy kolorów, motywów ani trybu bez koloru.
- Nie zmieniamy limitów Ticket (15) i Tytuł (50) ani zasad wklejania.
- Nie zmieniamy nazw wejść w plikach workflow, API, CLI i szablonach (R17).
- Nie zmieniamy listy workflow ani treści jej opisów.

---

## Key Decisions

- Jeden układ dla wszystkich pól (etykieta, pod nią wartość z `> `) zamiast dwóch: pole krótkie dostaje to, co pole Opis ma już dziś. Kosztem jest wysokość: każde pole zajmuje dwa wiersze.
- Opis przy etykiecie zamiast w linii statusu: operator widzi opis wszystkich pól naraz, nie tylko pola z focusem.
- Zmiana etykiet jest tylko wyświetlaniem: workflow, API i CLI nie zmieniają się.

---

## Dependencies / Assumptions

- Zweryfikowano w kodzie: pole krótkie rysuje `short_line` w `src/tui/chooser/inputs.rs` (`nazwa: wartość`). Nagłówek z opisem rysuje `header_line`. Prefiks `> ` ma `prefixed`.
- Zweryfikowano w kodzie: pola pod listą rysuje `field_lines` w `src/tui/chooser.rs`, a `render_with_style` użyje układu `inputs::render` tylko przy focusie na polach.
- Zweryfikowano w pliku: `workflows/compound-engineering.yaml` ma wejścia `feature_description` (text) i `target_repo` (repository).
- Wysokość: każde pole zajmuje dwa wiersze, więc w niskim terminalu kolumna się przewija jak dziś (`render_column`).

---

## Założenia do potwierdzenia

- Z1. „Nie pokazujemy wówczas pól opisujących ticket" znaczy: gdy focus jest na liście workflow, pola Ticket i Tytuł nie rysują się pod listą (R13). Nie znaczy, że znikają z formularza.
- Z2. Zmiana etykiet dotyczy wejść o nazwach `target_repo` i `feature_description`. Nazwy z opisu zadania (`target-repo`, `feature_description`) czytam jako te dwie nazwy. Inne workflow z innymi nazwami wejść zachowują dzisiejsze etykiety. Planowanie może zastąpić mapę nazw regułą po typie (wejście typu `repository` → „Repozytorium"), jeśli to prostsze; wtedy użytkownik potwierdza.
- Z3. Układ R1–R11 dotyczy wszystkich formularzy, które używają wspólnego rysowania pól, nie tylko formularza nowego zadania (jak R11 w dokumencie `pola-pod-soba`).
- Z4. Pole Opis (wieloliniowe) ma `> ` tylko w pierwszym wierszu wartości, a w kolejnych wierszach wcięcie o tej samej szerokości (jak `prefixed` i `CONTINUATION`).
- Z5. 1 spacja przed ramką dotyczy też pola Opis.

---

## Pytania otwarte dla użytkownika

- Brak.

---

## Outstanding Questions

### Deferred to Planning

- [Affects R7, R10][Technical] Gdzie stoją błąd pola i licznik znaków: w wierszu etykiety czy pod wartością.
- [Affects R16, R17][Technical] Mapa nazw wejść czy reguła po typie (Z2).
- [Affects R13, R14][Technical] Jak `field_lines` przestaje rysować pola przy focusie na liście, nie ruszając innych choosera (np. listy repozytoriów).
