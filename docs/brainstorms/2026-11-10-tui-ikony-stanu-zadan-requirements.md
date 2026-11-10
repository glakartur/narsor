---
date: 2026-11-10
topic: tui-ikony-stanu-zadan
---

# Ikony stanu zadań na liście TUI

## Summary

Lista zadań pokazuje jedną ikonę stanu przy każdym zadaniu, zamiast dwóch kolumn. Ikona mówi, co się dzieje z zadaniem. Trzecia linia wiersza pokazuje tylko nazwę bieżącego kroku i czas w bieżącym stanie.

---

## Problem Frame

Wiersz zadania ma dziś dwie kolumny znaczników. Kolumna 1 to `?` (pytanie, zatwierdzenie, propozycja conductora) albo `!` (problem). Kolumna 2 to ikona stanu (w toku, w kolejce, czeka na odpowiedź, wstrzymany, błąd, koniec, nieznany). Obie kolumny często mówią to samo: `◐` idzie z `?`, a `✗` idzie z `!`. Operator czyta dwa znaki tam, gdzie wystarczy jeden.

Ikony nie rozróżniają kilku stanów, które operator widzi inaczej. Zadanie w trakcie automatycznego wznawiania wygląda jak zadanie w toku. Zadanie w kolejce z offline'owym kontrolerem wygląda jak zwykła kolejka. Zadanie zakończone i zarchiwizowane mają tę samą ikonę.

Trzecia linia wiersza powtarza informację z ikony. Pokazuje słowo stanu (`running`, `queued`, `paused`), które ikona już niesie.

---

## Requirements

**Kolumna ikony**
- R1. Wiersz zadania na liście ma jedną kolumnę z ikoną stanu. Kolumna `?`/`!` znika.
- R2. Każdy stan z poniższej listy ma własną, osobną ikonę:
  - nowy (zadanie nieskierowane do procesowania),
  - w kolejce,
  - kolejka zablokowana (kontroler offline lub backend nieznany),
  - uruchamianie,
  - w toku,
  - wznawianie automatyczne,
  - czeka na reakcję,
  - ma problem,
  - wstrzymywanie,
  - zatrzymany ręcznie (czeka na ręczne wznowienie),
  - zakończony,
  - zarchiwizowany.
- R3. Stan nieznany (z nowszego serwera) zachowuje neutralną ikonę, jak dziś.
- R4. Każdy stan różni się od innych kształtem ikony. Sam kolor nie może nieść informacji.
- R5. „Czeka na reakcję" ma jedną ikonę dla każdej przyczyny: pytania, zatwierdzenia i propozycji conductora. Przyczyna jest widoczna poza ikoną (etykieta lub szczegóły).
- R6. „Ma problem" obejmuje wszystkie przypadki, które dziś ustawiają `!`, oraz zadania ze stanem błędu. Gdy zadanie jest w toku i ma problem, ikona problemu zastępuje ikonę „w toku". Problem ma też pierwszeństwo przed „czeka na reakcję".
- R7. Stan „nowy" ma w tabeli ikon zarezerwowane miejsce. W tym zakresie żadne zadanie jeszcze go nie przyjmuje.

**Trzecia linia wiersza**
- R8. Trzecia linia nie pokazuje słowa stanu.
- R9. Dla zadania w toku trzecia linia pokazuje nazwę bieżącego kroku workflow (z ikoną kroku, jeśli ją ma) i czas w bieżącym stanie.
- R10. Czas oznacza czas od początku bieżącego stanu, jak dziś. Zegar zeruje się przy każdej zmianie stanu. Dla zadań zakończonych, z błędem i zarchiwizowanych czasu nie ma.
- R11. Zadanie w kolejce pokazuje w trzeciej linii miejsce w kolejce (`#N`) i czas oczekiwania.
- R12. Etykiety `offline` i `bez slotu` znikają z trzeciej linii.
- R12a. Zadanie, którego kontroler jest offline, ma ikonę „kolejka zablokowana". Dotyczy to zadania w kolejce i zadania w toku.
- R12b. Klawisz `z` ukrywa razem zadania zakończone i zarchiwizowane, jak dziś.

**Liczniki w nagłówku**
- R13. Liczniki w nagłówku listy nadal liczą osobno zadania czekające na reakcję i zadania z problemem. Zadanie liczy się raz, pod ikoną, którą pokazuje jego wiersz.

---

## Acceptance Examples

- AE1. **Covers R1, R6.** Given zadanie w toku, które ma problem runnera, when lista rysuje wiersz, then wiersz ma jedną ikonę „ma problem" i nie ma ikony „w toku".
- AE2. **Covers R5.** Given dwa zadania: jedno z pytaniem do operatora, drugie z propozycją conductora, when lista rysuje wiersze, then oba mają tę samą ikonę „czeka na reakcję".
- AE3. **Covers R9, R10.** Given zadanie w toku w kroku „Review", w tym stanie od 2 godzin 10 minut, when lista rysuje wiersz, then trzecia linia pokazuje „Review" i „2h 10m", bez słowa „running".
- AE4. **Covers R10.** Given zadanie wróciło z „czeka na reakcję" do „w toku", when lista rysuje wiersz, then czas liczy się od chwili powrotu.
- AE5. **Covers R2.** Given zadanie zakończone i zadanie zarchiwizowane, when lista rysuje wiersze, then ikony są różne.
- AE6. **Covers R2, R12, R12a.** Given zadanie w kolejce, którego kontroler jest offline, when lista rysuje wiersz, then ma ikonę „kolejka zablokowana" i nie ma etykiety `offline`.
- AE7. **Covers R12a.** Given zadanie w toku, którego kontroler jest offline, when lista rysuje wiersz, then ma ikonę „kolejka zablokowana", nie „w toku".
- AE8. **Covers R12b.** Given lista z zadaniem zakończonym i zarchiwizowanym, when operator naciska `z`, then oba znikają z listy.

---

## Success Criteria

- Operator odczytuje stan zadania z jednej ikony, bez czytania trzeciej linii i bez porównywania dwóch kolumn.
- Operator odróżnia zadanie w toku od zadania w automatycznym wznawianiu oraz zwykłą kolejkę od kolejki zablokowanej.
- Trzecia linia niesie tylko informacje, których ikona nie niesie.
- Planista dostaje pełną tabelę stanów i reguły pierwszeństwa. Nie musi wymyślać zachowania wiersza ani zakresu.

---

## Scope Boundaries

- Sam stan „nowy" i akcja „skieruj do procesowania". W modelu zadań dziś nie ma takiego stanu. Zakres obejmuje tylko zarezerwowaną ikonę.
- Zmiana sposobu liczenia czasu. Czas zostaje „w bieżącym stanie". Łączny czas przetwarzania i czas kroku są poza zakresem.
- Zmiana kolejności zadań i zasad filtrowania listy.
- Nowe stany po stronie serwera. Zakres dotyczy pokazywania stanów, które serwer już rozróżnia.

---

## Key Decisions

- Jedna kolumna zamiast dwóch: kolumna `?`/`!` powtarzała stan, a jej jedyna dodatkowa informacja (problem przy pracy w toku) trafia do ikony przez regułę pierwszeństwa.
- Problem wygrywa nad stanem: problem nie może zniknąć pod ikoną „w toku". Szczegóły stanu zostają w trzeciej linii i w szczegółach zadania.
- Rozróżniamy cztery stany, które dziś wyglądają jak sąsiednie: zakończony i zarchiwizowany, wznawianie automatyczne, uruchamianie i wstrzymywanie, kolejka zablokowana.
- Trzecia linia traci słowo stanu, `offline` i `bez slotu`. Etykiety te powtarzają ikonę albo nic nie dają operatorowi. `bez slotu` pojawiało się dziś przy zadaniach zakończonych, czekających na reakcję, z błędem i wstrzymanych.
- Kontroler offline oznacza ikona „kolejka zablokowana", także przy zadaniu w toku: wspólny sygnał, że nikt zadania teraz nie obsługuje. Osobny trzynasty stan byłby dokładniejszy, ale kosztuje naukę kolejnej ikony.
- Klawisz `z` zachowuje się jak dziś i ukrywa oba stany końcowe.
- Czas zostaje „w bieżącym stanie": zmiana znaczenia czasu wymagałaby nowych danych i nie jest potrzebna do celu tej zmiany.

---

## Dependencies / Assumptions

- Serwer już rozróżnia stany „w kolejce", „kolejka zablokowana", uruchamianie, wznawianie automatyczne i wstrzymywanie. Stan „wstrzymywanie" (prośba o pauzę, zadanie jeszcze trzyma slot) i stan „uruchamianie" są dziś zwijane do „wstrzymany" i „w toku" w jednym polu stanu. Do zweryfikowania w planie: czy klient ma dość danych, by je rozróżnić.
- Zasada z `src/tui/style.rs`: kolor nigdy nie niesie informacji sam. Każdy stan potrzebuje osobnego kształtu.
- Widok historii używa tych samych dwóch kolumn. Zakładam, że dostaje tę samą zmianę (jedna ikona). Do potwierdzenia w planie.

---

## Outstanding Questions

### Resolve Before Planning

Brak.

### Deferred to Planning

- [Affects R2, R4][Technical] Dobór konkretnych znaków i kolorów dla dwunastu stanów w terminalach o różnych zestawach znaków.
- [Affects R2][Needs research] Skąd klient bierze rozróżnienie uruchamiania, wstrzymywania i automatycznego wznawiania, jeśli pole stanu ich dziś nie rozdziela.
- [Affects R13][Technical] Jak liczniki w nagłówku wyprowadzają liczby z nowej, jednej kolumny.
