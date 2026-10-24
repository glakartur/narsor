---
date: 2026-10-24
topic: task-ticket-i-tytul
---

# Ticket i tytuł zadania

## Summary

Zadanie dostaje dwa opcjonalne pola opisowe: numer ticketu (np. numer Jiry) i tytuł. Operator wpisuje je w formularzu nowego zadania. Wartości są parametrami zadania, więc prompty mogą się do nich odwoływać. Widok szczegółów zadania dostaje nowy nagłówek, który pokazuje je w ramce razem z krótkim numerem wewnętrznym zadania.

---

## Problem Frame

Zadanie ma dziś tylko identyfikator wewnętrzny i wejścia workflow. Numer ticketu i tytuł istnieją tylko wtedy, gdy autor workflow zadeklaruje na nie osobne wejście (np. `ticket_id`) i użyje go w szablonie `display`. Każdy workflow robi to inaczej, a wiele workflow nie robi tego wcale. Operator nie widzi w szczegółach zadania, do którego ticketu zadanie należy. Prompt nie może się odwołać do ticketu bez własnego wejścia.

---

## Requirements

**Wprowadzanie**
- R1. Formularz nowego zadania ma dwa pola: „Ticket" i „Tytuł". Oba są opcjonalne. Zadanie bez nich powstaje tak jak dziś.
- R2. Pola ticketu i tytułu są jednoliniowe. Wartość jest przycinana z białych znaków na początku i na końcu. Pusta wartość znaczy „brak".
- R3. Pola ticketu i tytułu pojawiają się w formularzu dla każdego workflow, niezależnie od jego wejść. Stosują się do nich zwykłe zasady pól formularza (edycja, wklejanie, fokus).
- R4. Ticket i tytuł są ustalane przy tworzeniu zadania. Ten dokument nie dodaje ich późniejszej edycji przez operatora. Wyjątek: tytuł generowany przez dyrygenta (R15).

**Parametry zadania**
- R5. Ticket i tytuł są zapisane razem z zadaniem. Wznowienie, odtworzenie po restarcie i inny runner widzą te same wartości.
- R6. Prompty i inne szablony workflow mogą odczytać ticket i tytuł przez `{{ ... }}`. Ten sam zapis działa w każdym kroku zadania.
- R7. Gdy pole jest puste, szablon zwraca pusty tekst. Szablon nie kończy się błędem.
- R8. Nazwy tych parametrów nie kolidują z wejściami zadeklarowanymi przez workflow. Istniejące workflow działają bez zmian.

**Tytuł generowany przez dyrygenta**
- R15. Gdy zadanie powstaje bez tytułu, dyrygent przygotowuje tytuł na podstawie opisu zadania i zapisuje go jako tytuł zadania. Dyrygent nigdy nie zmienia tytułu podanego przez operatora.
- R16. Wygenerowany tytuł jest jednoliniowy, ma najwyżej 50 znaków i jest w języku użytym przez użytkownika (języku opisu zadania). Dyrygent powstaje razem z uruchomieniem runnera, czyli po utworzeniu zadania. Dlatego wygenerowany tytuł pojawia się z opóźnieniem. Do tego czasu zadanie ma pusty tytuł i działa według R7 i R10.
- R17. Gdy dyrygent zapisze tytuł, nagłówek i zakładka Info pokazują go bez ponownego tworzenia zadania. Zapis wygenerowanego tytułu jest widoczny po wznowieniu i dla innego runnera (jak R5).

**Wejścia zadania**
- R18. Backend przyjmuje ticket i tytuł na wejściu przy tworzeniu zadania, tak jak inne parametry zadania. Cała logika (przycinanie, limity, wartość pusta) jest w backendzie. TUI jest cienkim klientem i tylko przekazuje wartości.

**Nagłówek szczegółów zadania**
- R9. Widok szczegółów zadania ma nowy nagłówek nad zakładkami.
- R10. Po lewej stronie nagłówka są: ticket i tytuł. Gdy brak jednego z nich, widać drugie. Gdy brak obu, lewa strona pokazuje to, co nagłówek pokazuje dziś. Gdy tytuł jest ustawiony, zastępuje on tytuł z `display` workflow w nagłówku (linki z `display` zostają).
- R11. Po prawej stronie nagłówka jest numer wewnętrzny zadania, obcięty do pierwszych 8 znaków identyfikatora (GUID).
- R12. Ticket, tytuł i numer wewnętrzny są wyróżnione graficznie. Każda z tych grup ma własną ramkę lub inne wyraźne obramowanie w stylu istniejących paneli TUI.
- R13. Zbyt długi tytuł jest zawijany do kolejnych wierszy i widać go w całości. Nie jest skracany. Numer wewnętrzny po prawej jest zawsze widoczny w całości.

**Zakładka Info**
- R14. Zakładka Info pokazuje ticket i tytuł, gdy są ustawione.

---

## Acceptance Examples

- AE1. **Covers R1, R5.** Given formularz nowego zadania, when operator zostawia ticket i tytuł puste i zatwierdza, then zadanie powstaje, a nagłówek nie pokazuje pustych ramek.
- AE2. **Covers R6.** Given zadanie z ticketem `NAR-12`, when prompt kroku zawiera szablon ticketu, then agent dostaje w prompcie `NAR-12`.
- AE3. **Covers R7.** Given zadanie bez ticketu, when prompt zawiera szablon ticketu, then szablon daje pusty tekst, a krok startuje.
- AE4. **Covers R10, R11.** Given zadanie z ticketem `NAR-12`, tytułem „Zmiana API zamówień" i identyfikatorem zaczynającym się od `6a7ceacf`, when operator otwiera szczegóły, then po lewej widzi `NAR-12` i tytuł, a po prawej `6a7ceacf`.
- AE5. **Covers R13.** Given bardzo długi tytuł i wąski terminal, when nagłówek się rysuje, then tytuł jest zawinięty do kolejnych wierszy i widoczny w całości, a numer wewnętrzny jest cały.
- AE7. **Covers R15, R16, R17.** Given zadanie utworzone bez tytułu i z opisem, when runner startuje i dyrygent powstaje, then dyrygent zapisuje tytuł z opisu, a nagłówek pokazuje go po zapisie. Przed zapisem zadanie działa z pustym tytułem.
- AE8. **Covers R15.** Given zadanie z tytułem podanym przez operatora, when dyrygent powstaje, then tytuł się nie zmienia.
- AE6. **Covers R8.** Given workflow z własnym wejściem `ticket_id`, when operator podaje ticket w nowym polu i w tym wejściu, then oba mają osobne wartości.

---

## Success Criteria

- Operator rozpoznaje zadanie po tickecie i tytule bez otwierania zakładki Info.
- Autor workflow odwołuje się do ticketu w prompcie bez deklarowania własnego wejścia.
- Istniejące workflow i zapisane zadania działają bez zmian.
- Planowanie nie musi zgadywać, jakie pola są opcjonalne, gdzie są widoczne i co robi pusta wartość.

---

## Scope Boundaries

- Brak walidacji formatu ticketu (np. wzorca Jiry).
- Brak połączenia z Jirą: bez pobierania tytułu, statusu i bez linku.
- Brak edycji ticketu i tytułu przez operatora po utworzeniu zadania. Wyjątek: tytuł generowany przez dyrygenta (R15).
- Brak filtrowania i wyszukiwania listy zadań po tickecie lub tytule.
- Brak zmian w liście zadań (poza tym, co wynika z innych wymagań).

---

## Key Decisions

- Ticket i tytuł to wbudowane pola zadania, a nie wejścia workflow: działają w każdym workflow bez pracy autora.
- Brak tytułu nie blokuje tworzenia zadania. Dyrygent uzupełnia go później z opisu zadania (R15).
- Backend przejmuje całą logikę pól. TUI jest tylko cienkim klientem (R18).
- Ticket jest czystym tekstem. Link do Jiry nie jest potrzebny.
- Numer wewnętrzny stoi po prawej, bo jest stały, a po lewej treść zależy od operatora.

---

## Dependencies / Assumptions

- Dotychczasowy nagłówek szczegółów (`src/tui/task_app/detail.rs`) pokazuje `display` workflow i stan. Gdy zadanie ma tytuł, tytuł zastępuje w nagłówku tytuł z `display` (decyzja użytkownika). Lista zadań nadal pokazuje tytuł z `display`.
- Zadania zapisane przed zmianą nie mają tych pól. Traktujemy je jako puste. Weryfikacja zgodności zapisu (`TaskLaunchSpec` jest wersjonowany i odrzuca stare wersje) należy do planowania.

---

## Założenia do potwierdzenia

- Numer wewnętrzny to identyfikator zadania (`TaskId`). Pokazujemy jego pierwsze 8 znaków.
- „Po lewej task, tytuł" znaczy: po lewej ticket (numer Jiry) i tytuł zadania.
- Parametry nazywają się `ticket` i `title` w przestrzeni zadania, np. `{{ task.ticket }}` i `{{ task.title }}`, a nie w `inputs.*`. Powód: brak kolizji z wejściami workflow.
- Limit długości: ticket i tytuł mają rozsądny limit (propozycja: ticket 64, tytuł 200 znaków). Wartości nie mają znaków nowej linii.
- Tytuł z `display` workflow to początek treści zadania widoczny w nagłówku szczegółów. Pełny tytuł zadania zastępuje go (potwierdzone przez użytkownika). Gdy tytułu brak, nagłówek pokazuje `display` jak dziś.
- „Otaczamy graficznie" znaczy ramki w istniejącym stylu paneli (zaokrąglone, zob. `docs/brainstorms/2026-10-20-tui-zaokraglone-ramki-requirements.md`).
- Ticket i tytuł to zwykły tekst od operatora. Nie wolno w nich wpisywać danych osobowych klientów ani tajemnic. Pola nie maskują wartości.

---

## Outstanding Questions

### Deferred to Planning

- [Affects R15, R16, R17][Technical] Gdzie dyrygent zapisuje tytuł i jak zmiana dociera do widoku.
- [Affects R5][Technical] Jak zapisać pola razem z zadaniem i jak zachować zgodność ze starszymi zapisanymi zadaniami.
- [Affects R6][Technical] Gdzie w kontekście szablonów dodać przestrzeń `task`.
- [Affects R9, R12][Technical] Jak zmieścić nagłówek w układzie `src/tui/task_app/layout.rs` i `src/tui/task_app/detail.rs` w małych terminalach.
