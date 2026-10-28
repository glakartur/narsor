---
date: 2026-10-26
topic: tui-lista-zadan-ticket-tytul-status
---

# Lista zadań w TUI: ticket, tytuł, status i czas aktywności, stała kolejność

## Summary

Wiersz listy zadań (lewy panel TUI) pokazuje ticket (albo skrócony `task_id`, gdy ticketu brak), tytuł oraz status z czasem aktywności. Kolejność zadań na liście nie zmienia się, gdy operator zatwierdza pracę w zadaniu.

---

## Problem Frame

Dziś wiersz ma dwie linie: znaczniki uwagi i stanu, tytuł oraz etykietę (krok, `#n`, `offline`). Operator nie widzi ticketu, więc trudno mu rozpoznać zadanie po numerze z Jiry. Nie widzi też, od jak dawna zadanie jest w danym stanie.

Operator zgłasza też, że po zatwierdzeniu pracy w zadaniu kolejność na liście się zmienia. Zadanie "ucieka" z miejsca, a zaznaczenie i orientacja przepadają.

Stan kodu (zweryfikowany):
- `src/tui/task_app/list.rs`: dokumentacja modułu mówi, że klient nigdy nie sortuje listy. Kolejność wyznacza serwer (grupy po backendzie, w grupie wg `rank`). Tylko zdarzenie `Moved` zmienia kolejność wierszy.
- `TaskSummary` ma już pola `ticket`, `title`, `display.title`, `created_at`, `state`, `rank`. Ticket jest `Option<String>`.
- `title_of` zwraca `display.title`, a gdy go brak, `task_id`.
- Nie ma ogólnego pola z czasem wejścia w dowolny stan; `QueueView.since` istnieje dla `Queued`, a `AttentionItem.since` dla elementów uwagi. Źródło czasu dla pozostałych stanów trzeba ustalić.
- Przyczyny zmiany kolejności po zatwierdzeniu **nie zweryfikowałem**. Kandydaci: odświeżenie listy z serwera (`TaskList::replace`) po zatwierdzeniu, zmiana `rank` po stronie serwera albo zdarzenie `Moved`. Plan musi ją najpierw ustalić odtworzeniem błędu.

---

## Requirements

**Zawartość wiersza**

- R1. Wiersz pokazuje ticket zadania. Gdy zadanie nie ma ticketu, wiersz pokazuje skróconą postać `task_id`.
- R2. Wiersz pokazuje tytuł zadania (jak dziś: `display.title`, a gdy go brak, identyfikator).
- R3. Wiersz pokazuje status zadania i czas aktywności.
- R4. Znaczniki uwagi (`?`, `!`) i ikona stanu zostają. Dodatkowe informacje nie zmniejszają ich czytelności.
- R5. Teksty z serwera (ticket, tytuł) przechodzą przez `one_line`, tak jak dziś. Zbyt długi tekst jest przycinany, a ticket nie wypiera tytułu całkowicie.
- R6. Skrócony `task_id` ma stałą, krótką długość i jest zawsze odróżnialny od ticketu (np. przez inny styl lub prefiks). Operator nie bierze go za ticket.

**Czas aktywności**

- R7. Czas aktywności to czas, jaki zadanie spędziło w bieżącym statusie. Format jest krótki (np. `5m`, `2h 10m`, `3d`).
- R8. Czas rośnie na żywo, bez odświeżania danych z serwera. Dla zadań w stanie końcowym (zakończone, błąd) czas nie rośnie.

**Stała kolejność**

- R9. Kolejność zadań na liście nie zmienia się podczas zatwierdzania prac w zadaniu (odpowiedź agentowi, zatwierdzenie akcji, zatwierdzenie kroku). Zmiana statusu zmienia tylko tekst i ikonę wiersza, nie jego miejsce.
- R10. Kolejność zmienia się tylko wtedy, gdy operator sam przenosi zadanie (tryb `m`) albo gdy w kolejce pojawia się lub znika zadanie.
- R11. Zaznaczone zadanie pozostaje zaznaczone po zatwierdzeniu pracy (zaznaczenie po `task_id`, jak dziś).

**Filtrowanie**

- R12. Naciśnięcie `/` na liście otwiera filtr zadań. Dopasowanie uwzględnia ticket, tytuł i `task_id`; filtrowanie nie zmienia kolejności pozostałych zadań.

## Acceptance Examples

- AE1. **Covers R1, R2.** Zadanie ma ticket `NAR-42` i tytuł "Refaktor parsera". Wiersz pokazuje `NAR-42` i tytuł.
- AE2. **Covers R1, R6.** Zadanie nie ma ticketu. Wiersz pokazuje skrócony `task_id` w stylu różnym od ticketu.
- AE3. **Covers R3, R7, R8.** Zadanie czeka na odpowiedź od 12 minut. Wiersz pokazuje status i `12m`, a po minucie `13m` bez odświeżania z serwera.
- AE4. **Covers R9, R11.** Na liście są zadania A, B, C. Operator odpowiada agentowi w zadaniu B. Po zatwierdzeniu kolejność nadal brzmi A, B, C, a B jest zaznaczone.
- AE5. **Covers R10.** Operator przenosi C przed A w trybie `m`. Kolejność to C, A, B.

---

## Success Criteria

- Operator rozpoznaje zadanie na liście po tickecie albo po skróconym identyfikatorze, bez wchodzenia w szczegóły.
- Operator widzi, które zadania "stoją" najdłużej w danym statusie.
- Zatwierdzenie pracy ani zmiana statusu nie zmieniają kolejności; kolejność może się zmienić po jawnym przeniesieniu przez operatora albo dodaniu/usunięciu zadania.
- Operator może otworzyć filtr klawiszem `/` i znaleźć zadanie po tickecie, tytule lub `task_id`.

## Scope Boundaries

- Poza zakresem: nowe sortowanie lub grupowanie listy po statusie, tickecie albo czasie.
- Poza zakresem: zmiana zasady, że kolejność wyznacza serwer.
- Poza zakresem: edycja ticketu z poziomu listy.
- Poza zakresem: zmiany w panelu szczegółów (poza tym, co wynika ze wspólnych funkcji).

## Założenia i decyzje operatora

- A1. Rozstrzygnięte: "czas aktywności" to czas w bieżącym statusie. `HumanNeeded` korzysta z `attention.since`, `Queued` z `queue.since`, a `Provisioning` z czasu wejścia w ten stan.
- A2. Rozstrzygnięte: skrót `task_id` ma 8 znaków, jest widoczny tylko bez ticketu i ma tekstowy prefiks `id:`.
- A3. Wiersz ma trzy linie: znaczniki i ticket (albo skrócony `task_id`), tytuł, a następnie status, czas aktywności i dotychczasową etykietę (krok, `#n`, `offline`, `bez slotu`).
- A4. Rozstrzygnięte: czas dla `Queued` liczymy od `queue.since`; dla zadań zakończonych nie pokazujemy czasu.
- A5. Przyczyną zmiany kolejności jest zachowanie listy po zatwierdzeniu (odświeżenie lub zmiana `rank`), a nie sortowanie w kliencie, bo klient nie sortuje. To nie jest zweryfikowane. Plan zaczyna się od odtworzenia problemu.
- A6. Zmiana kolejności po zatwierdzeniu jest błędem do naprawy, a nie funkcją (np. "zadania wymagające uwagi na górę").
- A7. Pole czasu wejścia w bieżący stan może wymagać zmiany w protokole serwera (`TaskSummary`). Wymaga to oceny zgodności ze starszymi serwerami, które pola nie wyślą. W takim przypadku wiersz nie pokazuje czasu.

## Decyzje operatora

- Q1. Rozstrzygnięte: czas w bieżącym statusie; `HumanNeeded` używa `attention.since`, a `Provisioning` czasu wejścia w ten stan.
- Q2. Rozstrzygnięte: po zmianie na stan końcowy zadanie pozostaje na miejscu; ukrywanie zakończonych (`z`) nadal działa jako filtr.
- Q3. Rozstrzygnięte: skrót ma 8 znaków, pokazuje się tylko bez ticketu i ma prefiks `id:`.
- Q4. Rozstrzygnięte: szerokość listy i jej minimalna szerokość pozostają bez zmian. Ticket ma własną linię i jest przycinany do dostępnej szerokości.

## Dependencies / Assumptions

- Zależność: źródło czasu wejścia w stan (zob. A7).
- Zależność: ustalenie przyczyny zmiany kolejności (zob. A5), zanim plan przyjmie rozwiązanie.

## Next Steps

- Decyzje dla Q1–Q4 zostały uwzględnione w planie.
- Po akceptacji planu można rozpocząć implementację; pierwszym krokiem jest odtworzenie zmiany kolejności po zatwierdzeniu.
