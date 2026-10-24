---
date: 2026-10-24
topic: formularz-nowego-zadania-etykieta-nad-polem
---

# Formularz nowego zadania: etykieta nad polem wartości

## Summary

Każde pole każdego formularza TUI ma własny wiersz z etykietą i opisem, a pod nim wiersz z wartością. Wiersz wartości zaczyna się od `> `, a po jego prawej stronie zostaje 1 spacja do ramki. Pole wyboru (`‹ wartość ›`) ma ten sam układ. Wszystkie formularze rysują pola jednym wspólnym kodem. Formularz nowego zadania traci własne rysowanie pól krótkich. Gdy focus jest na liście workflow, pól Ticket i Tytuł nie widać. Etykiety w formularzu: `target_repo` → „Repozytorium", `description` → „Opis". Techniczna nazwa wejścia `feature_description` zmienia się na `description` w workflow, bo pole jest współdzielone i nie musi opisywać funkcji.

---

## Problem Frame

Dziś w TUI są dwa sposoby rysowania pól, choć wszystkie formularze są oparte o jeden `Chooser` (`src/tui/chooser.rs`).

**Wspólne rysowanie (norma).** `field_lines` rysuje nagłówek z etykietą, `znacznikiem „(wymagane)"` i opisem obok, a pod nim wartość przez `value_lines` (`src/tui/form/field.rs`): prefiks `> `, tło edycji, 1 kolumna wolna po prawej (`FIELD_RIGHT_MARGIN`). Pole wyboru ma ten sam układ (`‹ wartość ›`). Używają go:
- formularze i panele odpowiedzi w panelu „Czeka na Ciebie" (`src/tui/input_form.rs`, `src/tui/failure_form.rs`) przez `chooser::render_footer`,
- pola pod listą w każdym `Chooser` bez układu wejść, np. okna w `src/tui/task_app/actions.rs` i formularze repozytoriów w `src/tui/task_app/repositories.rs`,
- kompozytory (rozmowa z agentem, odpowiedź) przez `chooser::render_fields` w `src/tui/task_app/waiting.rs` i `src/tui/task_app/tabs/conductor.rs`.

**Własne rysowanie (odstępstwo).** Tylko `src/tui/chooser/inputs.rs` rysuje pola po swojemu. Włącza je `with_inputs_layout()` razem z focusem na polach (`render_with_style`). Efektywnie to tylko formularz nowego zadania (`src/tui/task_app/new_task.rs`); kompozytory w `waiting.rs` też wołają `with_inputs_layout()`, ale rysują się przez `render_fields`, więc tego kodu nie używają. W `inputs.rs`:
- pole krótkie to jeden wiersz `nazwa: wartość` (`short_line`), bez `> ` i bez wolnej kolumny po prawej,
- opis pola krótkiego jest w linii statusu nad kolumną, nie przy etykiecie,
- nagłówek pola wieloliniowego (`header_line`) i wiersze złożone (`prefixed`) powtarzają kod z `field_lines`.

Przyczyna: układ jednej kolumny z paskiem workflow, linią statusu i polem Opis na całą wysokość (zmiana #29) potrzebował własnego podziału wysokości. Pola krótkie zrobiono tam w jednym wierszu, żeby zaoszczędzić wysokość, i nikt nie wrócił do wspólnego kodu. Różnice w wyglądzie pól (brak `> `, opis w statusie, brak wolnej kolumny) są skutkiem tego odstępstwa, nie decyzją o wyglądzie.

Gdy focus jest na liście workflow, pola Ticket i Tytuł rysują się pod listą i opisem (`field_lines` w `src/tui/chooser.rs`). Operator wybiera wtedy workflow, a widzi pola, których jeszcze nie wypełnia.

Etykiety pochodzą z nazw wejść workflow (`target_repo`, `feature_description` w `workflows/compound-engineering.yaml`). Operator czyta nazwy techniczne w języku angielskim, a reszta formularza (Ticket, Tytuł, Opis) jest po polsku. Nazwa `feature_description` zakłada, że każde zadanie to „funkcja", a pole jest wspólne dla workflow o różnym celu.

---

## Requirements

**Układ pola**
- R0. Wszystkie formularze TUI rysują pola jednym wspólnym kodem (`field_lines` i `value_lines`, albo ich następcą). Formularz nowego zadania nie ma własnego rysowania pól: usuwamy `short_line` i powtórzony `header_line` z `src/tui/chooser/inputs.rs`. Układ formularza nowego zadania (pasek workflow, linia statusu, podział wysokości, pole Opis na całą resztę) zostaje, ale każde pole w nim rysuje wspólny kod.
- R1. Każde pole każdego formularza ma dwa wiersze (formularz nowego zadania, odpowiedzi, kompozytory, formularze repozytoriów i okna): wiersz etykiety i pod nim wiersz wartości. Dotyczy pól tekstowych jednolinijkowych, liczb, true/false i pól wyboru.
- R2. Pole wieloliniowe (Opis) zachowuje etykietę nad wartością. Jego wiersze wartości mają ten sam układ co wiersze innych pól (R4, R5).
- R3. Pole wyboru z wartością zmienianą klawiszami `←`/`→` (dziś `‹ wartość ›`) ma etykietę w osobnym wierszu, a wartość pod nią. Znaczniki `‹` `›` zostają przy wartości.
- R4. Wiersz wartości zaczyna się od `> ` (prefiks jak przy wartości pola Opis), potem idzie pole wartości.
- R5. Pole wartości kończy się 1 spacją przed prawą ramką. Tło edycji nie zajmuje tej spacji.
- R6. Tło edycji aktywnego pola obejmuje pole wartości (od miejsca po `> ` do tej 1 spacji). Prefiks `> ` i etykieta nie mają tła edycji. Reguły tła z `docs/brainstorms/2026-10-24-formularz-nowego-zadania-pola-pod-soba-requirements.md` (R7–R11) zostają, z tą zmianą szerokości.
- R7. Błąd pola i licznik znaków (np. `7/15`) zostają przy polu, do którego należą. Planowanie wybiera miejsce w wierszu etykiety lub wartości, byle nie psuły reguł R4 i R5.

**Opis obok etykiety**
- R8. Opis każdego pola stoi w wierszu etykiety, po etykiecie, w formie ` · opis`, jak przy polu Opis (`header_line`). Dotyczy też pól Ticket, Tytuł, Repozytorium i pozostałych wejść workflow.
- R9. Znacznik „(wymagane)" stoi w wierszu etykiety, przed opisem, jak dziś przy polach wieloliniowych.
- R10. Linia statusu nad kolumną formularza nowego zadania nie powtarza opisu pola, bo opis jest już przy etykiecie. Błąd pola może zostać w linii statusu albo przejść do pola (wybiera planowanie, R7).
- R11. Za długi opis jest przycinany z `…` do szerokości wiersza etykiety. Etykieta i znacznik „(wymagane)" zawsze się mieszczą.

**Wybór workflow**
- R12. Wybór workflow zostaje listą wartości z opisem wybranego workflow w prawym panelu. Lista i panel opisu nie zmieniają układu.
- R13. Gdy focus jest na liście workflow, pól Ticket i Tytuł nie widać. Wystarczy, że nie są widoczne: można je nie rysować albo wyciąć z układu (planowanie wybiera). Pola pojawiają się, gdy operator przejdzie do pól formularza.
- R14. Reguła R13 dotyczy tylko formularza nowego zadania. Pozostałe pola workflow też się nie rysują pod listą w tym stanie (formularz pokazuje jedną rzecz naraz: listę z opisem albo pola). Inne okna z listą i polami pod nią (np. dodawanie repozytorium) zachowują dzisiejsze zachowanie.
- R15. Nawigacja klawiszami między listą a polami i kolejność pól (Ticket, Tytuł, Repozytorium, pozostałe wejścia, Opis) nie zmieniają się.

**Etykiety**
- R16. Etykieta wejścia `target_repo` brzmi „Repozytorium". Etykieta wejścia `description` brzmi „Opis".
- R17. Nazwa techniczna `target_repo` zostaje bez zmian (plik workflow, API, CLI `--input target_repo=...`, szablony).
- R17a. Nazwa techniczna `feature_description` zmienia się na `description` wszędzie, gdzie jest nazwą wejścia: w dołączonych workflow (`workflows/compound-engineering.yaml`, `workflows/compound-engineering-chat.yaml`), w szablonach (`{{ inputs.description }}`), w dokumentacji i przykładach CLI (`README.md`, `WORKFLOW_SCHEMA.md`, `workflows/README.md`), w fixtures i testach. Opis wejścia w workflow nie mówi, że to „funkcja".
- R17b. Workflow użytkownika, który nadal ma wejście `feature_description`, działa dalej: formularz pokazuje je z etykietą z nazwy (bez zmiany). Zmiana nie dodaje aliasu ani migracji.
- R17c. Pozostałe dołączone workflow dostają nazwy wejść z `compound-engineering`: `workflows/compound-engineering-debug.yaml` (`bug_description` → `description`) i `workflows/open-mercato-sdlc.yaml` (`task_brief` → `description`). Nazwa `target_repo` już się zgadza. Zmiana obejmuje szablony, dokumentację i testy tych workflow. Wejście `ticket_id` w `open-mercato-sdlc.yaml` zostaje bez zmian.
- R18. Etykiety Ticket i Tytuł zostają. Etykiety w innych formularzach (odpowiedź, komentarz, repozytoria) zostają.

**Pliki formularza**
- R19. Pliki odpowiedzialne za formularz nowego zadania stoją w jednym folderze (np. `src/tui/task_app/new_task/`): `new_task.rs` i kod układu z `src/tui/chooser/inputs.rs` (pasek workflow, linia statusu, podział wysokości, pole Opis). Wspólne rysowanie pól (`field_lines`, `value_lines`) zostaje poza tym folderem, bo używają go inne formularze (R0). Planowanie wybiera nazwę folderu i podział na moduły. Przeniesienie nie zmienia zachowania.

---

## Acceptance Examples

- AE1. **Covers R1, R4, R5, R8.** Given pole Ticket w formularzu o szerokości 60 kolumn, when formularz się rysuje, then wiersz 1 to `Ticket · numer zadania w Jirze, opcjonalnie`, wiersz 2 to `> `, pole wartości i 1 wolna spacja przed prawą ramką.
- AE2. **Covers R3.** Given pole Repozytorium z wartością `acme/web`, when formularz się rysuje, then wiersz 1 to `Repozytorium (wymagane) · opis`, wiersz 2 to `> ‹ acme/web ›`.
- AE3. **Covers R6.** Given aktywne pole Tytuł, when formularz się rysuje, then tło edycji zaczyna się po `> ` i kończy przed ostatnią spacją wiersza. Etykieta nie ma tła.
- AE4. **Covers R12, R13.** Given focus na liście workflow, when formularz się rysuje, then widać listę workflow i prawy panel z opisem workflow. Pól Ticket i Tytuł nie widać.
- AE5. **Covers R16, R17, R17a.** Given dołączony workflow z wejściami `description` i `target_repo`, when formularz się rysuje, then etykiety to „Opis" i „Repozytorium". Wywołanie `--input target_repo=acme-api --input description="..."` w CLI działa.
- AE7. **Covers R0, R1.** Given formularz odpowiedzi na pauzę albo kompozytor rozmowy, when się rysuje, then pola wyglądają tak samo jak w formularzu nowego zadania: etykieta z opisem, pod nią `> ` i wartość, 1 wolna kolumna po prawej.
- AE8. **Covers R17b.** Given workflow użytkownika z wejściem `feature_description`, when formularz się rysuje, then pole ma etykietę `feature_description` i działa jak dotąd.
- AE6. **Covers R11.** Given opis pola dłuższy niż wolne miejsce w wierszu etykiety, when formularz się rysuje, then opis kończy się `…`, a etykieta jest cała.

---

## Success Criteria

- Wszystkie pola formularza wyglądają tak samo: etykieta z opisem, pod spodem `> ` i pole wartości.
- Operator wybiera workflow bez pól Ticket i Tytuł na ekranie.
- Jeden kod rysuje pola we wszystkich formularzach; w `inputs.rs` nie ma już własnego rysowania pól.
- Operator widzi etykiety „Repozytorium" i „Opis" zamiast nazw technicznych.
- Pliki formularza nowego zadania są w jednym folderze (R19).
- Planowanie nie musi zgadywać układu wiersza wartości ani zakresu zmiany etykiet.

---

## Scope Boundaries

- Nie zmieniamy kolejności pól ani nawigacji klawiszami.
- Nie zmieniamy kolorów, motywów ani trybu bez koloru.
- Nie zmieniamy limitów Ticket (15) i Tytuł (50) ani zasad wklejania.
- Nie zmieniamy nazwy `target_repo` (R17). Inne dołączone workflow zmieniają tylko nazwę wejścia opisu na `description` (R17c). Nazwa `feature_description` zmienia się na `description` tylko jako nazwa wejścia (R17a); API, CLI i silnik nie dostają innych zmian.
- Nie dodajemy aliasu `feature_description` → `description` ani migracji zapisanych zadań (R17b).
- Nie zmieniamy listy workflow ani treści jej opisów.

---

## Key Decisions

- Jeden układ dla wszystkich pól (etykieta, pod nią wartość z `> `) zamiast dwóch: pole krótkie dostaje to, co pole Opis ma już dziś. Kosztem jest wysokość: każde pole zajmuje dwa wiersze.
- Opis przy etykiecie zamiast w linii statusu: operator widzi opis wszystkich pól naraz, nie tylko pola z focusem.
- Etykieta „Repozytorium" to wyświetlanie (nazwa `target_repo` zostaje). Nazwa `description` zamiast `feature_description` to zmiana nazwy wejścia, bo nazwa techniczna nie ma sugerować rodzaju zadania (decyzja użytkownika).
- Jedno wspólne rysowanie pól w całym TUI, zamiast osobnego rysowania w formularzu nowego zadania (decyzja użytkownika).

---

## Dependencies / Assumptions

- Zweryfikowano w kodzie: pole krótkie rysuje `short_line` w `src/tui/chooser/inputs.rs` (`nazwa: wartość`). Nagłówek z opisem rysuje `header_line`. Prefiks `> ` ma `prefixed`.
- Zweryfikowano w kodzie: pola pod listą rysuje `field_lines` w `src/tui/chooser.rs`, a `render_with_style` użyje układu `inputs::render` tylko przy focusie na polach.
- Zweryfikowano w pliku: `workflows/compound-engineering.yaml` i `workflows/compound-engineering-chat.yaml` mają wejście `feature_description`; `compound-engineering.yaml` ma też `target_repo` (repository). Nazwa `feature_description` występuje też w `README.md`, `WORKFLOW_SCHEMA.md`, `workflows/README.md`, `tests/` i `tests/fixtures/`.
- Zweryfikowano w kodzie: `with_inputs_layout()` jest wołane w `src/tui/task_app/new_task.rs` i dwa razy w `src/tui/task_app/waiting.rs`; kod `inputs.rs` rysuje tylko przy `chooser::render` z focusem na polach.
- Wysokość: każde pole zajmuje dwa wiersze, więc w niskim terminalu kolumna się przewija jak dziś (`render_column`).

---

## Założenia do potwierdzenia

- Z1. Potwierdzone przez użytkownika: gdy focus jest na liście workflow, pól Ticket i Tytuł nie widać; mogą być wycięte z układu (R13).
- Z2. Potwierdzone przez użytkownika: etykiety „Repozytorium" i „Opis" wynikają z nazw wejść `target_repo` i `description`. Inne dołączone workflow dostosowujemy do nazw z `compound-engineering` (R17c), a nie dodajemy dla nich osobnych etykiet. Workflow użytkownika z innymi nazwami wejść zachowuje dzisiejsze etykiety (R17b).
- Z3. Potwierdzone przez użytkownika: wszystkie formularze mają wspólne rysowanie pól (R0). Dziś odstępuje od niego tylko formularz nowego zadania (patrz Problem Frame), więc zmiana wyglądu dotyka głównie jego.
- Z6. Zmiana nazwy `feature_description` → `description` łamie istniejące skrypty CLI (`--input feature_description=...`) i workflow użytkowników, którzy kopiują dołączone pliki. Przyjmuję to bez aliasu (R17b); zadania już zapisane trzymają swoje wejścia pod starą nazwą. Do weryfikacji przez zespół, jeśli ktoś wywołuje dołączone workflow ze skryptów.
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
- [Affects R0][Technical] Jak formularz nowego zadania używa wspólnego rysowania pól w układzie z paskiem, linią statusu i polem Opis na resztę wysokości (podział wysokości zostaje w `inputs.rs`, rysowanie pola nie).
- [Affects R17a][Needs research] Czy testy i fixtures z `feature_description` zmieniają się razem, czy część zostaje jako test wejścia o dowolnej nazwie (R17b).
- [Affects R13, R14][Technical] Jak `field_lines` przestaje rysować pola przy focusie na liście, nie ruszając innych choosera (np. listy repozytoriów).
