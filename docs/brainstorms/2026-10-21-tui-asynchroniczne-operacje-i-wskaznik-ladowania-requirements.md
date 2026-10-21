---
date: 2026-10-21
topic: tui-asynchroniczne-operacje-i-wskaznik-ladowania
---

# TUI: operacje poza interfejsem nie blokują go + wskaźnik ładowania

## Summary

Żadna operacja poza rysowaniem i obsługą klawiszy nie blokuje TUI. Dotyczy to odczytów z serwera, zapisów i prowadzenia silnika. Interfejs rysuje się i reaguje na klawisze cały czas. Panel lub okno, które czeka na dane, pokazuje znacznik ładowania (animowany wskaźnik, np. spinner).

---

## Problem Frame

Pętla zdarzeń TUI (`src/tui/mod.rs`) wykonuje każde żądanie przez `await` wewnątrz jednej gałęzi `tokio::select!`. Gdy ta gałąź czeka na serwer, nic innego się nie dzieje: brak rysowania, brak reakcji na klawisze (także quit). Dotyczy to m.in. `app.perform(request).await`, `app.refresh_if_due(..).await`, `app.sync_detail().await` i pierwszego odczytu `app.refresh().await`. Pod spodem jest wiele wywołań klienta, np. w `src/tui/task_app/detail.rs`, `history.rs`, `repositories.rs`, `new_task.rs`, `waiting.rs`, `actions.rs` i `harness.rs`.

Wcześniejszy dokument `docs/brainstorms/2026-09-26-tui-nieblokujace-uruchomienia-requirements.md` rozwiązuje to tylko dla czterech żądań uruchamiających silnik. Ten dokument obejmuje wszystkie pozostałe operacje.

Dziś operator nie widzi, że coś trwa. Panel bez danych wygląda tak samo jak panel, który czeka na odpowiedź. Znaczniki „ładuję" istnieją tylko w pojedynczych miejscach (np. zakładka dyrygenta, `Standing::Loading` w `src/tui/task_app/tabs/conductor.rs`, i flaga `loading` w zakładce rozmowy). Nie ma wspólnego, animowanego wskaźnika.

---

## Actors

- A1. Operator: obsługuje TUI, oczekuje, że interfejs reaguje zawsze.
- A2. Serwer / silnik: odpowiada na odczyty i zapisy z opóźnieniem, czasem długim lub z błędem.

---

## Key Flows

- F1. Odczyt danych panelu trwa dłużej niż chwilę
  - **Trigger:** Operator otwiera widok lub zakładkę, która wymaga odczytu z serwera.
  - **Actors:** A1, A2
  - **Steps:** Odczyt rusza w tle. Panel od razu pokazuje wskaźnik ładowania. Operator może w tym czasie przełączyć panel, przewijać inne dane lub wyjść. Gdy dane przychodzą, wskaźnik znika, a panel pokazuje dane.
  - **Outcome:** Operator widzi, że panel czeka, a interfejs nie zamarza.
  - **Covered by:** R1, R2, R4, R5

- F2. Odczyt kończy się błędem
  - **Trigger:** Odczyt w tle nie udaje się.
  - **Actors:** A1, A2
  - **Steps:** Wskaźnik ładowania znika. Panel pokazuje błąd w sposób, który już dziś pokazuje błędy tego panelu.
  - **Outcome:** Panel nie zostaje w stanie „ładuję" bez końca.
  - **Covered by:** R6

---

## Requirements

**Brak blokowania**
- R1. Pętla zdarzeń nie czeka na żadną operację sieciową ani silnika. Rysowanie i klawisze są obsługiwane bez przerwy, także w czasie trwania operacji.
- R2. Dotyczy to wszystkich operacji poza GUI: odczytów (lista, szczegóły, historia, rozmowa, repozytoria, workflow, harness), zapisów (np. dodanie repozytorium, odpowiedź na pytanie, zmiana harnessu) i uruchomień silnika.
- R3. Wynik operacji wraca do interfejsu bez utraty. Gdy w czasie operacji zmieniło się zaznaczenie lub otwarty panel, stary wynik nie nadpisuje nowego widoku. Wynik dla zadania, które nie jest już zaznaczone, jest odrzucony albo trafia tylko do jego własnych danych.
- R4. Ta sama operacja nie jest uruchamiana drugi raz, gdy poprzednia jeszcze trwa. Wyjątek: odczyt, który operator wymusza wprost (`r`) po tym, jak poprzedni zakończył się błędem.

**Wskaźnik ładowania**
- R5. Panel lub okno, które czeka na dane z operacji w tle, pokazuje animowany znacznik ładowania. Znacznik animuje się bez udziału operatora (zmienia klatkę co stały krótki odstęp, przy już istniejącym ticku pętli).
- R6. Znacznik znika, gdy operacja się kończy, zarówno powodzeniem, jak i błędem. Błąd pokazuje się tak jak dziś.
- R7. Znacznik jest czytelny bez koloru. Jest to znak lub krótki tekst, a nie sam kolor. Gdy terminal nie wyświetla znaków animacji, znacznik ma wersję z zwykłych znaków ASCII.
- R8. Odświeżenie danych, które już są pokazane (okresowe lub po zdarzeniu), nie zastępuje zawartości panelu znacznikiem. Pokazane dane zostają, a znacznik jest małą wskazówką w tytule panelu.
- R9. Zapis w tle (np. wysłanie odpowiedzi, dodanie repozytorium) pokazuje, że trwa, w oknie lub formularzu, który go uruchomił. Operator nie może przez przypadek wysłać go drugi raz (patrz R4).

**Zgodność**
- R10. Skróty klawiszowe, układ paneli i teksty błędów nie zmieniają się poza dodanym znacznikiem.
- R11. Zamknięcie TUI (quit) działa także wtedy, gdy operacje w tle jeszcze trwają. Terminal jest przywrócony tak jak dziś.

---

## Acceptance Examples

- AE1. **Covers R1, R5.** Dane: serwer odpowiada na odczyt zakładki po 5 sekundach. Gdy operator otwiera tę zakładkę, panel od razu pokazuje animowany znacznik. W tych 5 sekundach klawisze działają, a znacznik zmienia klatki.
- AE2. **Covers R3.** Dane: odczyt historii zadania A trwa. Gdy operator zaznacza zadanie B, a odczyt A kończy się później, widok B nie pokazuje danych A.
- AE3. **Covers R6.** Dane: odczyt kończy się błędem. Gdy błąd przychodzi, znacznik znika, a panel pokazuje błąd. Znacznik nie zostaje.
- AE4. **Covers R8.** Dane: lista zadań jest już pokazana. Gdy rusza okresowe odświeżenie, lista zostaje na ekranie, a tytuł panelu pokazuje mały znacznik.
- AE5. **Covers R4, R9.** Dane: operator zatwierdził formularz dodania repozytorium i zapis trwa. Gdy operator zatwierdza go drugi raz, nie rusza drugi zapis.
- AE6. **Covers R11.** Dane: operacja w tle trwa. Gdy operator naciska quit, TUI kończy się od razu, a terminal jest przywrócony.

---

## Success Criteria

- Operator nie widzi zamrożonego interfejsu w żadnym widoku, nawet przy wolnym lub niedostępnym serwerze.
- Operator odróżnia „panel czeka" od „panel jest pusty" i od „panel ma błąd".
- Planowanie nie musi wymyślać zachowania przy nakładających się operacjach, błędach i zamknięciu: R3, R4, R6 i R11 to pokrywają.

---

## Scope Boundaries

- Brak zmiany protokołu ani zachowania serwera. Zmiana dotyczy tylko klienta TUI.
- Brak nowych skrótów klawiszowych (np. anulowania operacji w tle).
- Brak paska postępu z procentami. Znacznik mówi tylko „trwa".
- Brak zmiany kolorów ról i układu paneli.
- Brak zmian w komendach CLI poza TUI.

---

## Key Decisions

- Jeden wspólny znacznik ładowania dla wszystkich paneli. Spójny wygląd jest tańszy w utrzymaniu niż osobne rozwiązania w każdym panelu.
- Dane już pokazane nie są zastępowane znacznikiem przy odświeżaniu (R8). Inaczej lista migałaby co okres odświeżania.

---

## Dependencies / Assumptions

- Stan kodu (sprawdzony): pętla w `src/tui/mod.rs` wykonuje `perform`, `refresh_if_due`, `retry_outgoing_if_due` i `sync_detail` przez `await` w jednym wątku zadania. Wątek `read_terminal_events` czyta klawisze osobno. Pętla budzi się co `TICK` (250 ms).
- Niesprawdzone: które operacje w `task_app` są dziś najdłuższe w praktyce. Zakres zakłada wszystkie.

---

## Założenia do potwierdzenia

- Rozumiem „doktrolkę" jako „kontrolkę" (element interfejsu) z animowanym wskaźnikiem typu spinner.
- „Operacje poza GUI" to wszystkie wywołania do serwera i silnika. Rysowanie i obsługa klawiszy zostają synchroniczne.
- Wskaźnik ma być widoczny w panelu, który czeka, a nie jako jeden globalny pasek.
- Dokument nie powtarza zakresu `2026-09-26-tui-nieblokujace-uruchomienia-requirements.md`. Zakładam, że ta zmiana go uzupełnia, a jej cztery uruchomienia silnika wchodzą tu w R2.
- Animacja korzysta z istniejącego ticku 250 ms. Płynniejsza animacja nie jest potrzebna.
- Zmiana nie wymaga nowej zależności od runtime. TUI już działa na `tokio`.

---

## Pytania otwarte dla użytkownika

- Czy znacznik ma być w tytule panelu (małe `⠋` obok nazwy), w jego środku (na miejscu danych), czy w obu miejscach zależnie od tego, czy dane już są (tak zakłada R8)?
- Czy operator ma móc przerwać trwającą operację w tle? Zakładam, że nie (patrz zakres).
- Czy zapisy (odpowiedź, dodanie repozytorium) mają blokować formularz do końca, czy zamykać go od razu i pokazać wynik w powiadomieniu? Zakładam pierwsze (R9), bo to bezpieczniejsze.
- Czy wolisz jeden wspólny znacznik z kodu, czy osobny wygląd dla odczytu i zapisu?

## Outstanding Questions

### Deferred to Planning

- [Affects R1, R2][Technical] Jak wynik operacji w tle wraca do `TaskApp` (kanał wyników, wspólny typ żądań w tle, numer wersji widoku).
- [Affects R3][Needs research] Które istniejące odczyty już mają własną ochronę przed nieaktualnym wynikiem (`revision` w `src/tui/task_app/detail.rs`) i czy da się je uogólnić.
- [Affects R5][Technical] Skąd znacznik bierze klatkę animacji, żeby test mógł ją ustawić bez czekania.
- [Affects R7][Technical] Jak wykryć terminal bez znaków animacji, albo czy wystarczy zawsze ASCII.
