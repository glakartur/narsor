---
date: 2026-10-24
topic: ticket-i-tytul-w-ramce-szczegolow
---

# Ticket, tytuł i numer zadania w ramce panelu szczegółów

## Summary

Ticket, tytuł i numer wewnętrzny zadania (pierwsze 8 znaków GUID) przenosimy z bloku nagłówkowego wewnątrz panelu szczegółów do górnej krawędzi ramki panelu szczegółów zadania. Każda wartość jest otoczona znakami oddzielającymi ją od linii ramki. Blok nagłówkowy z własnymi ramkami znika. To zmiana wyglądu. Dane i logika pól ticketu i tytułu zostają bez zmian.

---

## Problem Frame

Wymagania z `docs/brainstorms/2026-10-24-task-ticket-i-tytul-requirements.md` (R9–R13) kazały pokazać ticket, tytuł i numer w nowym nagłówku nad zakładkami. Dziś robi to `label_block` w `src/tui/task_app/detail.rs`: trzy małe ramki wewnątrz panelu. Panel ma wtedy dwie warstwy ramek, a nagłówek zabiera 3 lub więcej wierszy, które zmniejszają miejsce na zakładki w małym terminalu. Ramka panelu szczegółów ma tytuł (`panel_title`), który dziś pokazuje tylko tytuł z `display` workflow. Operator chce, by właśnie ta ramka niosła ticket, tytuł i numer.

---

## Requirements

**Miejsce wartości**
- R1. Ticket, tytuł i numer wewnętrzny są pokazane w ramce panelu szczegółów zadania (na jej górnej krawędzi). Nie są pokazane w bloku nagłówkowym wewnątrz panelu.
- R2. Blok nagłówkowy `label_block` (trzy osobne ramki wewnątrz panelu) jest usunięty. Odzyskane wiersze trafiają do zakładek.
- R3. Dotychczasowe wiersze nagłówka (stan, postęp, linki z `display`, powiadomienie) zostają bez zmian.

**Układ na krawędzi**
- R4. Po lewej stronie krawędzi stoją ticket i tytuł (w tej kolejności). Po prawej stoi numer wewnętrzny: pierwsze 8 znaków GUID.
- R5. Każda wartość jest otoczona znakami oddzielającymi ją od linii ramki, np. `┤ NAR-12 ├`. Dokładny zestaw znaków jest w „Założenia do potwierdzenia".
- R6. Numer wewnętrzny jest zawsze widoczny w całości. Jeśli miejsca jest mało, najpierw skracamy tytuł, potem ticket.
- R7. Zbyt długi tytuł jest skracany znakiem `…`. Pełny tytuł widać w zakładce Info (R14 z dokumentu bazowego).

**Brak wartości**
- R8. Gdy brak ticketu, nie rysujemy jego znaków otaczających. To samo dla tytułu. Brak pustych par znaków.
- R9. Gdy brak ticketu i tytułu, lewa strona krawędzi pokazuje to, co pokazuje dziś (tytuł z `display` workflow). Numer po prawej pokazujemy zawsze.
- R10. Gdy tytuł jest ustawiony, zastępuje tytuł z `display` na krawędzi (decyzja z dokumentu bazowego). Linki z `display` zostają w nagłówku (R3).

**Bez zmian**
- R11. Pola formularza, zapis, parametry szablonów `{{ ... }}` i tytuł generowany przez dyrygenta nie zmieniają się.
- R12. Wskaźnik ładowania zostaje widoczny na krawędzi, jak dziś.
- R13. Tryb bez koloru (`no_color`) i tryb bez połączenia (`stale`) działają jak dziś. Znaki otaczające czytelne bez koloru.

---

## Acceptance Examples

- AE1. **Covers R1, R4.** Given zadanie z ticketem `NAR-12`, tytułem „Zmiana API zamówień" i GUID `6a7ceacf-…`, when operator otwiera szczegóły, then górna krawędź ramki pokazuje po lewej `NAR-12` i tytuł, a po prawej `6a7ceacf`. Wewnątrz panelu nie ma osobnych ramek.
- AE2. **Covers R6, R7.** Given bardzo długi tytuł i wąski terminal, when ramka się rysuje, then tytuł kończy się `…`, a `6a7ceacf` jest cały.
- AE3. **Covers R8, R9.** Given zadanie bez ticketu i bez tytułu, when operator otwiera szczegóły, then krawędź pokazuje tytuł z `display` i numer po prawej, bez pustych znaków.
- AE4. **Covers R8.** Given zadanie z samym ticketem, when ramka się rysuje, then widać ticket i numer, bez znaków dla tytułu.

---

## Success Criteria

- Panel szczegółów ma jedną warstwę ramek. Nagłówek jest niższy o wiersze zajmowane dziś przez blok.
- Operator rozpoznaje zadanie po krawędzi ramki bez czytania wnętrza panelu.
- Planowanie nie musi zgadywać, co się dzieje przy braku wartości i przy braku miejsca.

---

## Scope Boundaries

- Bez zmian w liście zadań.
- Bez zmian w danych, backendzie i formularzu.
- Bez zawijania tytułu do kilku wierszy (krawędź ma jeden wiersz). Pełny tytuł jest w zakładce Info.

---

## Założenia do potwierdzenia

- „Ramka panelu szczegółów" to zewnętrzna ramka panelu rysowana przez `render_panel` w `src/tui/style.rs`, nie ramka nagłówka w środku.
- Zdanie z polecenia („otoczył te wartości znakami …") jest urwane. Zakładamy znaki w stylu ramek TUI: `┤ wartość ├` na linii krawędzi. Wartość i znaki pasują do zaokrąglonych ramek (`docs/brainstorms/2026-10-20-tui-zaokraglone-ramki-requirements.md`).
- Zawijanie tytułu (R13 z dokumentu bazowego) ustępuje: na krawędzi tytuł jest skracany, bo krawędź ma jeden wiersz.
- Zwolnione wiersze nagłówka wracają do zakładek, a nie zostają puste.
- Ten dokument zastępuje R9, R12 i R13 z dokumentu bazowego w części o nagłówku. Reszta tamtego dokumentu obowiązuje.

---

## Pytania otwarte dla użytkownika

1. Jakimi znakami otoczyć wartości? Polecenie jest urwane. Propozycja: `┤ … ├`. Inne opcje: `[ … ]` lub `│ … │`.
2. Czy skracanie długiego tytułu na krawędzi jest akceptowalne, czy tytuł ma być widoczny w całości (wtedy potrzebny jest dodatkowy wiersz wewnątrz panelu)?
3. Czy numer wewnętrzny też ma stać na górnej krawędzi po prawej, czy w dolnej krawędzi?
4. Czy ticket i tytuł mają mieć oddzielne znaki (`┤ NAR-12 ├─┤ tytuł ├`), czy jedną wspólną parę?

---

## Outstanding Questions

### Deferred to Planning

- [Affects R4–R7][Technical] Jak złożyć tytuł ramki z kilku wartości i kolorów: `panel_title` zwraca dziś jeden `String` (`src/tui/task_app/detail.rs`, `src/tui/style.rs`).
- [Affects R2][Technical] Jak policzyć wysokość nagłówka i zakładek po usunięciu `label_block` (`areas`, `render_view`).
- [Affects R5][Technical] Czy ten sam panel używa historii wpisów (`render_view` jest współdzielony) i jak tam pokazać krawędź.
