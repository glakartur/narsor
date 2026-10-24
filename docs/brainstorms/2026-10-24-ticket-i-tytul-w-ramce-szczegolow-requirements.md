---
date: 2026-10-24
topic: ticket-i-tytul-w-ramce-szczegolow
---

# Ticket, tytuł i numer zadania w ramce panelu szczegółów

## Summary

Ticket, tytuł i numer wewnętrzny zadania (pierwsze 8 znaków GUID) przenosimy z bloku nagłówkowego wewnątrz panelu szczegółów do górnej krawędzi ramki panelu szczegółów zadania. Każda wartość ma kształt „pigułki”: półkola po bokach, napis w środku na tle w kolorze ramki. Blok nagłówkowy z własnymi ramkami znika. To zmiana wyglądu. Dane i logika pól ticketu i tytułu zostają bez zmian.

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
- R5. Każda wartość jest osobną „pigułką” na linii krawędzi: lewe półkole, napis na tle w kolorze ramki, prawe półkole (np. glify Powerline U+E0B6 i U+E0B4). Ticket i tytuł to dwie osobne pigułki. Numer wewnętrzny też. Bez koloru (`no_color`) pigułka wraca do znaków `┤ NAR-12 ├`.
- R6. Numer wewnętrzny jest zawsze widoczny w całości. Jeśli miejsca jest mało, najpierw skracamy tytuł, potem ticket.
- R7. Zbyt długi tytuł jest skracany znakiem `…` (decyzja użytkownika: skracamy). Pełny tytuł widać w zakładce Info (R14 z dokumentu bazowego).

**Brak wartości**
- R8. Gdy brak ticketu, cała jego pigułka znika. To samo dla tytułu. Brak pustych pigułek.
- R9. Gdy brak ticketu i tytułu, lewa strona krawędzi pokazuje to, co pokazuje dziś (tytuł z `display` workflow). Numer po prawej, na górnej krawędzi, pokazujemy zawsze.
- R10. Gdy tytuł jest ustawiony, zastępuje tytuł z `display` na krawędzi (decyzja z dokumentu bazowego). Linki z `display` zostają w nagłówku (R3).

**Bez zmian**
- R11. Pola formularza, zapis, parametry szablonów `{{ ... }}` i tytuł generowany przez dyrygenta nie zmieniają się.
- R12. Wskaźnik ładowania zostaje widoczny na krawędzi, jak dziś.
- R13. Tryb bez koloru (`no_color`) i tryb bez połączenia (`stale`) działają jak dziś. Pigułki czytelne bez koloru (patrz R5).

---

## Acceptance Examples

- AE1. **Covers R1, R4.** Given zadanie z ticketem `NAR-12`, tytułem „Zmiana API zamówień" i GUID `6a7ceacf-…`, when operator otwiera szczegóły, then górna krawędź ramki pokazuje po lewej `NAR-12` i tytuł, a po prawej `6a7ceacf`. Wewnątrz panelu nie ma osobnych ramek.
- AE2. **Covers R6, R7.** Given bardzo długi tytuł i wąski terminal, when ramka się rysuje, then tytuł kończy się `…`, a `6a7ceacf` jest cały.
- AE3. **Covers R8, R9.** Given zadanie bez ticketu i bez tytułu, when operator otwiera szczegóły, then krawędź pokazuje tytuł z `display` i numer po prawej, bez pustych pigułek.
- AE4. **Covers R8.** Given zadanie z samym ticketem, when ramka się rysuje, then widać ticket i numer, bez pigułki tytułu.

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
- Kształt znaków potwierdził użytkownik: półkola, napis w środku na tle w kolorze ramki. Glify półkoli zależą od czcionki terminala (Powerline). Planowanie sprawdza, czy czcionka operatora je ma. Bez nich działa wariant `┤ … ├`.
- Zawijanie tytułu (R13 z dokumentu bazowego) ustępuje: na krawędzi tytuł jest skracany, bo krawędź ma jeden wiersz.
- Zwolnione wiersze nagłówka wracają do zakładek, a nie zostają puste.
- Ten dokument zastępuje R9, R12 i R13 z dokumentu bazowego w części o nagłówku. Reszta tamtego dokumentu obowiązuje.

---

## Decyzje użytkownika (z przeglądu PR)

1. Znaki: półkola, napis w środku na tle w kolorze ramki.
2. Długi tytuł na krawędzi jest skracany.
3. Numer wewnętrzny stoi na górnej krawędzi, po prawej.
4. Ticket i tytuł są osobnymi pigułkami. Gdy brak elementu, cały człon znika.

---

## Outstanding Questions

### Deferred to Planning

- [Affects R4–R7][Technical] Jak złożyć tytuł ramki z kilku wartości i kolorów: `panel_title` zwraca dziś jeden `String` (`src/tui/task_app/detail.rs`, `src/tui/style.rs`).
- [Affects R2][Technical] Jak policzyć wysokość nagłówka i zakładek po usunięciu `label_block` (`areas`, `render_view`).
- [Affects R5][Technical] Czy ten sam panel używa historii wpisów (`render_view` jest współdzielony) i jak tam pokazać krawędź.
