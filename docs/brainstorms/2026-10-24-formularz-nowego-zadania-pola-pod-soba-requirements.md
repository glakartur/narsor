---
date: 2026-10-24
topic: formularz-nowego-zadania-pola-pod-soba
---

# Formularz nowego zadania: pola jedno pod drugim

## Summary

Formularz nowego zadania pokazuje pola w jednej kolumnie, w stałej kolejności: Ticket, Tytuł, Repo, pozostałe wejścia workflow, Opis. Pole Opis zajmuje całą pozostałą wysokość i ma tło edycji na całej swojej powierzchni, także w pustych wierszach. Ticket ma najwyżej 15 znaków, Tytuł najwyżej 50.

---

## Problem Frame

Po dodaniu pól Ticket i Tytuł formularz ma pasek krótkich pól (`src/tui/chooser/inputs.rs`). Pasek układa krótkie pola obok siebie, w kolumnach, w jednym wierszu. Pod paskiem są pola wieloliniowe. Kolejność nawigacji `↑`/`↓` nie zgadza się z tym, co operator widzi: pole obok jest „następne" w poziomie, a nawigacja idzie w pionie. Operator nie wie, dokąd przejdzie focus.

Pole Opis rozciąga się na całą wolną wysokość, ale tło edycji jest tylko pod wierszami z tekstem. Pole wygląda jak podświetlony tekst, nie jak pole. Istnieje już osobny dokument o tym samym problemie dla szerokości pola: `docs/brainstorms/2026-10-24-tlo-aktywnego-pola-formularza-requirements.md`.

Serwer dziś przyjmuje ticket do 64 znaków i tytuł do 200 znaków (`crates/narsor-engine/src/domain/task.rs`). To więcej niż potrzebuje formularz.

---

## Requirements

**Układ i kolejność**
- R1. Formularz nowego zadania pokazuje pola w jednej kolumnie, jedno pod drugim, w kolejności: Ticket, Tytuł, Repo, Opis.
- R2. Każde z pól Ticket, Tytuł i Repo zajmuje własny wiersz na całą szerokość formularza. Nie ma pól obok siebie.
- R3. Nawigacja `↑`/`↓` i `Tab`/`Shift+Tab` idzie w tej samej kolejności, w jakiej pola widać na ekranie.
- R4. Pozostałe wejścia workflow (liczby, true/false, krótkie teksty) idą w kolumnie po polu Repo, przed polem Opis, w kolejności deklaracji w workflow. Ten sam układ pionowy dotyczy ich tak samo.
- R5. Pole Opis jest ostatnie i zajmuje całą pozostałą wysokość formularza.
- R6. Workflow bez pola Repo albo bez pola Opis pokazuje pozostałe pola w tej samej kolejności, bez pustego miejsca po brakującym polu.

**Tło pól**
- R7. Pole Opis w edycji ma tło edycji na całej swojej powierzchni: na całej szerokości i na całej wysokości, którą ma pole, także w wierszach bez tekstu. Pole puste lub z jedną linią tekstu ma tło w całym prostokącie.
- R8. Reguła R7 dotyczy każdego pola wieloliniowego formularza, nie tylko pola Opis. Gdy pól wieloliniowych jest kilka, każde ma tło w całym prostokącie, który mu przypada, także w stanie zwiniętym.
- R9. Pola jednolinijkowe (Ticket, Tytuł, Repo i krótkie wejścia) mają tło edycji na całej szerokości swojego wiersza, jak w `2026-10-24-tlo-aktywnego-pola-formularza-requirements.md` (R1, R2, R5).
- R10. Pole bez focusu nie ma tła edycji. Kolory, motywy i tryb bez koloru zostają bez zmian.
- R11. Reguły R7–R10 dotyczą wszystkich formularzy, które używają wspólnego rysowania pól (np. formularz odpowiedzi i komentarza), a nie tylko formularza nowego zadania.

**Limity pól**
- R12. Pole Ticket jest jednolinijkowe i przyjmuje najwyżej 15 znaków.
- R13. Pole Tytuł jest jednolinijkowe i przyjmuje najwyżej 50 znaków.
- R14. Formularz nie pozwala wpisać więcej znaków, niż wynosi limit. Nowa linia (`Ctrl+J`, `Shift+Enter`, `Alt+Enter`) w tych polach nic nie robi. Wklejony tekst z nowymi liniami jest zamieniany na jedną linię, a po przekroczeniu limitu jest obcinany do limitu.
- R15. Limity liczą znaki, nie bajty. Polskie znaki liczą się za jeden.
- R16. Operator widzi, ile znaków mu zostało lub jaki jest limit (np. `7/15`) przy każdym z tych pól.
- R17. Backend sprawdza te same limity (15 i 50 znaków) i odrzuca zadanie z dłuższą wartością z czytelnym błędem. TUI nie jest jedynym miejscem, które pilnuje limitu. Dotyczy to także zadań tworzonych przez inne wejścia (np. `src/cli/remote.rs`).
- R18. Limit 50 znaków tytułu jest ten sam co limit tytułu generowanego przez dyrygenta (`GENERATED_TITLE_MAX_CHARS`). Tytuł podany przez operatora i tytuł wygenerowany mają jedną regułę.

---

## Acceptance Examples

- AE1. **Covers R1, R2, R3.** Given formularz nowego zadania dla workflow z polem Repo i polem Opis, when formularz się rysuje, then widać cztery wiersze jeden pod drugim: Ticket, Tytuł, Repo, Opis. `↓` przenosi focus kolejno z Ticketu na Tytuł, Repo i Opis.
- AE2. **Covers R5, R7.** Given puste pole Opis w edycji i formularz o wysokości 30 wierszy, when formularz się rysuje, then tło edycji wypełnia cały prostokąt pola Opis aż do dołu formularza, nie tylko pierwszy wiersz.
- AE3. **Covers R7.** Given pole Opis z dwiema liniami tekstu, when jest aktywne, then wiersze poniżej tekstu mają to samo tło co wiersze z tekstem.
- AE4. **Covers R8, R11.** Given inny formularz z polem wieloliniowym (np. odpowiedź na pauzę), when pole jest aktywne, then jego cały prostokąt ma tło edycji.
- AE5. **Covers R12, R14, R16.** Given pole Ticket z 15 znakami, when operator wpisuje kolejny znak, then znak nie pojawia się, a licznik pokazuje `15/15`.
- AE6. **Covers R14, R15.** Given operator wkleja do pola Tytuł tekst z dwiema liniami i 80 znakami, when wklejanie się kończy, then pole zawiera jedną linię o 50 znakach.
- AE7. **Covers R13, R14.** Given pole Tytuł w edycji, when operator naciska `Ctrl+J`, then pole się nie zmienia i formularz nie jest wysłany.
- AE8. **Covers R17.** Given wywołanie tworzenia zadania z ticketem o 20 znakach, when backend je przyjmuje, then odrzuca je z błędem o limicie 15 znaków, a zadanie nie powstaje.
- AE9. **Covers R6.** Given workflow bez wejścia tekstowego dla opisu, when formularz się rysuje, then widać Ticket, Tytuł i Repo jedno pod drugim, a pod nimi komunikat, że workflow nie ma pola tekstowego, jak dziś.
- AE10. **Covers R4.** Given workflow z wejściami `repository`, `dry_run` i `prompt`, when formularz się rysuje, then kolejność to Ticket, Tytuł, Repo, `dry_run`, Opis.

---

## Success Criteria

- Operator przechodzi po polach `↓` w tej samej kolejności, w jakiej je widzi. Nie szuka pola „obok".
- Pole Opis wygląda jak pole: pełny prostokąt tła na całej dostępnej wysokości.
- Ticket i Tytuł nie przyjmują tekstu dłuższego niż limit w żadnym wejściu: w formularzu ani w backendzie.
- Planowanie nie musi zgadywać kolejności pól, limitów ani zakresu tła.

---

## Scope Boundaries

- Nie zmieniamy koloru tła, motywów ani trybu bez koloru.
- Nie zmieniamy wyboru workflow, panelika z nazwą workflow ani opisu po prawej.
- Nie zmieniamy późniejszej edycji ticketu i tytułu (nadal poza zakresem, wg `docs/brainstorms/2026-10-24-task-ticket-i-tytul-requirements.md`, R4).
- Nie zmieniamy kolejności ani układu pól w innych widokach (nagłówek szczegółów, zakładka Info).
- Nie zmieniamy sposobu generowania tytułu przez dyrygenta, poza wspólną regułą limitu z R18.
- Nie dodajemy innych pól do formularza.

---

## Key Decisions

- Jedna kolumna dla wszystkich pól zamiast paska z kolumnami: kolejność wizualna i kolejność nawigacji są ta sama. Kosztem jest wysokość: każde krótkie pole zajmuje wiersz.
- Kolejność Ticket, Tytuł, Repo, Opis jest sztywna dla pól własnych formularza: daje przewidywalny układ w każdym workflow.
- Limity są pilnowane w formularzu (wygoda) i w backendzie (zasada), bo TUI jest cienkim klientem (`docs/brainstorms/2026-10-24-task-ticket-i-tytul-requirements.md`, R18).
- Tło na całej powierzchni jest regułą wspólnego rysowania pól, nie poprawką jednego pola (zgodnie z R16 z `docs/brainstorms/2026-10-22-tui-edycja-formularzy-requirements.md`).

---

## Dependencies / Assumptions

- Zweryfikowano w kodzie: pole `Ticket` i `Tytuł` są dziś zwykłymi polami tekstowymi `ChooserField::text` w `src/tui/task_app/new_task.rs`. Pasek krótkich pól rysuje `render_bar` w `src/tui/chooser/inputs.rs`. Pola wieloliniowe rysuje `render_long_fields`.
- Zweryfikowano w kodzie: limity serwera to dziś 64 znaki (ticket) i 200 znaków (tytuł).
- Dokument `2026-10-24-tlo-aktywnego-pola-formularza-requirements.md` opisuje szerokość tła. Ten dokument dodaje wysokość (R7, R8). Planowanie scala oba zakresy w jedną zmianę wspólnego rysowania pól.

---

## Założenia do potwierdzenia

- Z1. „Temat - 50" z opisu zadania znaczy pole Tytuł (50 znaków). Pola o nazwie „Temat" w formularzu nie ma.
- Z2. Limity 15 i 50 znaków obowiązują wszędzie: w formularzu i w backendzie (R17). Obniżenie limitu backendu z 64/200 nie psuje istniejących zadań: istniejące dłuższe wartości zostają bez zmian, a limit dotyczy nowych zadań. Nie zweryfikowano, czy istnieją zadania z dłuższymi wartościami.
- Z3. „Repo" to dzisiejsze pole `repository` (przełącznik `‹ wartość ›`). Zmienia się tylko jego miejsce w układzie.
- Z4. „Opis" to główne pole tekstowe (prompt) workflow, jak w `docs/brainstorms/2026-10-09-tui-formularz-nowego-zadania-uklad-requirements.md` (R4).
- Z5. Pozostałe krótkie wejścia workflow (R4) idą po Repo, przed Opisem. To najbezpieczniejsze miejsce: Opis zostaje ostatni.
- Z6. Przycinanie białych znaków (R2 w dokumencie ticketu) działa bez zmian i liczy się po przycięciu: limit dotyczy wartości po przycięciu, a formularz liczy znaki wpisane.
- Z7. Licznik znaków (R16) jest dodatkiem o niskim koszcie. Można go pominąć, jeśli planowanie uzna, że szkodzi czytelności wąskiego terminalu.
- Z8. Wklejony tekst z nowymi liniami (R14) zamieniamy na jedną linię przez zastąpienie każdej przerwy jedną spacją. Alternatywa (odrzucenie wklejenia) jest mniej wygodna.

---

## Pytania otwarte dla użytkownika

- P1. Czy limit backendu ma spaść z 64/200 do 15/50 dla wszystkich wejść (CLI, API), czy 15/50 ma pilnować tylko formularz TUI? Zakładamy pierwsze (Z2). Zmiana limitu w API może złamać istniejące skrypty, które podają dłuższy ticket.
- P2. Czy workflow, który deklaruje własne wejście `ticket_id` (AE6 w dokumencie ticketu), ma też dostać układ pionowy z limitem 15 znaków? Zakładamy: nie, limit dotyczy tylko pola własnego formularza „Ticket".

---

## Outstanding Questions

### Deferred to Planning

- [Affects R2, R3][Technical] Czy pasek krótkich pól znika całkowicie, czy zostaje dla innych formularzy, które nie mają pól własnych.
- [Affects R5, R6][Technical] Jak formularz dzieli wysokość w niskim terminalu, gdy cztery wiersze i nagłówek nie zostawiają miejsca na Opis.
- [Affects R7, R8][Needs research] Przyczyna tła tylko pod tekstem: ta sama co Z1 w `2026-10-24-tlo-aktywnego-pola-formularza-requirements.md`.
- [Affects R14][Technical] Jak silnik formularza ogranicza długość pola i obsługuje wklejanie przy limicie.
