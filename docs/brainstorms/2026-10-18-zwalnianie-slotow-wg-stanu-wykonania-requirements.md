---
date: 2026-10-18
topic: zwalnianie-slotow-wg-stanu-wykonania
---

# Zwalnianie slotów według stanu wykonania i runner tylko do odczytu

## Summary

Centrala zwalnia slot taska według stanu jego wykonania, a nie tylko według życia procesu. Stany ostateczne (Completed, Cancelled) zwalniają slot od razu. Stany chwilowe (input_needed, Failed) zwalniają go po okresie oczekiwania. Gdy slot jest zwolniony, controller zatrzymuje runnera. Odczyt szczegółów zatrzymanego taska idzie na każdym backendzie tą samą drogą: centrala prosi controller o runnera tylko do odczytu, bez slotu.

---

## Problem Frame

Artur dodał nowy task na backendzie `local` z limitem 1 slotu. Drugi task był już zakończony, a mimo to nowy nie wystartował. TUI pokazywało „sloty 1/1”.

Zakończony task miał `execution_status: completed`, ale `lifecycle: running`, a jego proces `narsor-runner` dalej działał. Scheduler liczy slot według `TaskRecord::holds_capacity` (`crates/narsor-engine/src/orchestration/model.rs`). Ta funkcja patrzy tylko na lifecycle, a `Running` zawsze zajmuje slot. Po zakończeniu workflow nikt nie zmienia lifecycle, więc slot jest zajęty, dopóki właściciel nie zarchiwizuje taska. Projekt mówi, że zakończony task zostaje na liście do archiwizacji (D-T5, R34 planu zdalnych backendów). Nie mówi, że przez ten czas ma blokować kolejkę.

To samo dotyczy tasków, które czekają na człowieka (input_needed, Failed). Runner jest bezczynny, a slot zajęty bez limitu czasu.

Proste zatrzymanie runnera po zakończeniu (automatyczna pauza) ma koszt. Gdy runner nie działa, centrala czyta szczegóły tylko z ostatniego snapshotu. Wyjątkiem jest backend `local`: tam centrala sama czyta pliki taska z dysku (`ProtocolService::with_local_storage`, wynik pracy `docs/brainstorms/2026-10-10-wznawianie-utraconych-zadan-requirements.md` R9–R11). Na zdalnym backendzie rozmowa i zdarzenia zatrzymanego taska są niedostępne (KTD4). Lokalny skrót łamie też zasadę, że centrala nie zna szczegółów backendu: omija controller i czyta storage bezpośrednio.

---

## Actors

- A1. Właściciel: dodaje taski, odpowiada na pytania, czyta szczegóły, wznawia i archiwizuje.
- A2. Centrala (control plane, `narsor-server`): decyduje o slotach, kolejce i stanie tasków. Jedyne miejsce decyzji o slotach.
- A3. Controller (lokalny wbudowany w daemona albo zdalny, np. Kubernetes): wykonuje decyzje centrali. Uruchamia i zatrzymuje runnery.
- A4. Runner: wykonuje polecenia i zgłasza stan. Sam nie decyduje o slotach ani o swoim zatrzymaniu.

---

## Key Flows

- F1. Task kończy workflow
  - **Trigger:** Runner zgłasza stan ostateczny (Completed albo Cancelled).
  - **Actors:** A2, A3, A4
  - **Steps:** Centrala od razu zwalnia slot i przelicza kolejkę. Controller zatrzymuje runnera w bezpiecznym punkcie. Następny task z kolejki dostaje slot.
  - **Outcome:** Zakończony task zostaje na liście z ostatnim stanem i storage, bez slotu i bez procesu.
  - **Covered by:** R1, R2, R4, R5

- F2. Task czeka na człowieka
  - **Trigger:** Runner zgłasza stan chwilowy (input_needed albo Failed).
  - **Actors:** A1, A2, A3, A4
  - **Steps:** Centrala zostawia slot i liczy okres oczekiwania. Gdy właściciel odpowie w tym czasie, task pracuje dalej bez kolejki. Gdy właściciel otworzy szczegóły taska, okres się wydłuża. Gdy czas minie, centrala zwalnia slot, a controller zatrzymuje runnera.
  - **Outcome:** Task czeka bez slotu. Jego pytanie i stan są widoczne.
  - **Covered by:** R3, R6, R7

- F3. Właściciel czyta szczegóły zatrzymanego taska
  - **Trigger:** Właściciel otwiera rozmowę, zdarzenia albo czat dyrygenta taska bez działającego runnera.
  - **Actors:** A1, A2, A3, A4
  - **Steps:** Centrala prosi controller o runnera tylko do odczytu. Controller go uruchamia: pod na Kubernetes, proces lokalnie. Runner odpowiada na odczyty. Po okresie bezczynności controller go zatrzymuje.
  - **Outcome:** Właściciel czyta pełne szczegóły. Slot nie jest zajęty, a stan pracy jest niezmieniony.
  - **Covered by:** R8, R9, R10, R11, R12

- F4. Właściciel zleca pracę zatrzymanemu taskowi
  - **Trigger:** Właściciel odpowiada na input, pisze do dyrygenta, cofa decyzję dyrygenta albo restartuje krok.
  - **Actors:** A1, A2, A3, A4
  - **Steps:** Centrala zapisuje polecenie i zmienia je w zwykłe resume. Task czeka w kolejce na slot. Po przydziale slotu controller uruchamia runnera do pracy, a runner wykonuje polecenie.
  - **Outcome:** Polecenie jest wykonane bez przekroczenia limitu slotów.
  - **Covered by:** R13, R14

---

## Requirements

**Zwalnianie slotu**
- R1. O slotach decyduje tylko centrala. Controller zatrzymuje i uruchamia runnery na polecenie centrali. Runner nie decyduje o swoim zatrzymaniu.
- R2. Stany ostateczne wykonania (Completed, Cancelled) zwalniają slot od razu po zgłoszeniu przez runnera.
- R3. Stany chwilowe (input_needed, Failed) trzymają slot przez okres oczekiwania. Po jego upływie centrala zwalnia slot.
- R4. Gdy centrala zwalnia slot taska z działającym runnerem, controller zatrzymuje runnera w bezpiecznym punkcie. Storage taska zostaje.
- R5. Zwolnienie slotu od razu przelicza kolejkę backendu. Następny task dostaje slot bez działania właściciela.
- R6. Okres oczekiwania jest ustawieniem backendu z wartością domyślną ok. 5 minut.
- R7. Otwarcie szczegółów taska w stanie chwilowym, który ma jeszcze slot, wydłuża okres oczekiwania o kilka minut. Wydłużenie jest ustawieniem backendu.

**Runner tylko do odczytu**
- R8. Odczyt rozmowy, zdarzeń albo czatu dyrygenta taska bez działającego runnera idzie na każdym backendzie tą samą drogą: centrala prosi controller o runnera tylko do odczytu. Centrala nie czyta storage taska bezpośrednio.
- R9. Runner tylko do odczytu nie zajmuje slotu i nie liczy się do limitu backendu.
- R10. Runner tylko do odczytu nie zmienia żadnego stanu pracy. Nie wykonuje kroków, nie naprawia, nie wznawia i nie uzgadnia przerwanej akcji. Dotyczy to też storage tasków `Paused` i `Interrupted`.
- R11. Controller zatrzymuje runnera tylko do odczytu po okresie bezczynności. Ten okres jest ustawieniem backendu z wartością domyślną.
- R12. Podczas startu runnera do odczytu właściciel widzi, że szczegóły są w trakcie wczytywania. Gdy runner do odczytu nie może wystartować, właściciel widzi powód i ostatni snapshot.

**Praca na zatrzymanym tasku**
- R13. Każde polecenie, które wymaga pracy (odpowiedź na input, wiadomość do dyrygenta, undo decyzji dyrygenta, restart kroku), zmienia task bez slotu w zwykłe resume. Task czeka w kolejce na slot, a potem runner wykonuje polecenie. Właściciel nie musi osobno robić resume.
- R14. Polecenie wysłane do taska bez slotu nie przepada. Centrala je przechowuje i dostarcza runnerowi po resume.
- R15. Gdy działa runner tylko do odczytu, a centrala przydziela taskowi slot do pracy, task nie ma dwóch runnerów na jednym storage naraz.

**Widoczność**
- R16. TUI pokazuje, czy task ma slot. Task bez slotu pokazuje stan wykonania (np. zakończony, czeka na odpowiedź) i informację, że nie zajmuje slotu. Licznik slotów nie liczy tasków bez slotu ani runnerów do odczytu.
- R17. Zwolnienie slotu po okresie oczekiwania zostawia zdarzenie w historii taska.

---

## Acceptance Examples

- AE1. **Covers R2, R4, R5.** Backend `local` ma limit 1. Task A kończy workflow, a task B czeka w kolejce. Po zgłoszeniu Completed centrala zwalnia slot A, controller zatrzymuje runnera A, a task B startuje. TUI pokazuje „sloty 1/1” dla B.
- AE2. **Covers R3, R6, R17.** Task czeka na input. Właściciel nie odpowiada przez 5 minut i nie otwiera taska. Centrala zwalnia slot, controller zatrzymuje runnera, a historia taska ma zdarzenie o zwolnieniu slotu.
- AE3. **Covers R3.** Task ma stan Failed. Właściciel w 2. minucie robi restart kroku z notatką. Krok startuje od razu, bez kolejki.
- AE4. **Covers R7.** Task czeka na input od 4 minut. Właściciel otwiera jego szczegóły i czyta rozmowę. Slot zostaje dłużej niż 5 minut od wejścia w input_needed.
- AE5. **Covers R8, R9, R11.** Zakończony task na Kubernetes nie ma poda. Właściciel otwiera rozmowę kroku. Controller uruchamia poda tylko do odczytu, właściciel czyta rozmowę, a licznik slotów się nie zmienia. Po okresie bezczynności controller usuwa poda.
- AE6. **Covers R8.** Zakończony task na `local` nie ma procesu runnera. Właściciel otwiera zdarzenia. Lokalny controller uruchamia proces runnera do odczytu. Centrala nie czyta plików taska sama.
- AE7. **Covers R10.** Task `Interrupted` ma przerwaną akcję. Właściciel otwiera jego rozmowę. Runner do odczytu pokazuje rozmowę i nie oznacza akcji jako Unknown ani nie startuje jej ponownie.
- AE8. **Covers R13, R14.** Task czekał na input i stracił slot. Inne taski zajmują wszystkie sloty. Właściciel wysyła odpowiedź. Task idzie do kolejki z zachowaną odpowiedzią. Gdy dostaje slot, runner startuje i dostarcza odpowiedź agentowi.
- AE9. **Covers R15.** Właściciel czyta rozmowę zakończonego taska przez runnera do odczytu, a potem cofa decyzję dyrygenta. Runner do odczytu się zatrzymuje albo przechodzi w tryb pracy. Na storage nigdy nie działają dwa runnery naraz.

---

## Success Criteria

- Zakończony task nigdy nie blokuje kolejki. Task bezczynny w oczekiwaniu na człowieka blokuje ją najwyżej przez okres oczekiwania.
- Właściciel może przeczytać pełne szczegóły każdego niezarchiwizowanego taska na każdym backendzie, bez zajmowania slotu.
- Limit `max_concurrent_tasks` zostaje twardy dla pracy: żadne polecenie nie wykonuje pracy agenta bez slotu.
- Centrala nie ma kodu, który zależy od rodzaju backendu przy odczycie szczegółów.
- Planowanie nie musi wymyślać, które stany zwalniają slot, kiedy, ani co robi runner do odczytu.

---

## Scope Boundaries

- Kopiowanie rozmów, zdarzeń albo storage taska do centrali lub controllera. Szczegóły czyta zawsze runner ze storage taska.
- Archiwizacja się nie zmienia. Nie potrzebuje runnera: controller usuwa workload i storage.
- Wywłaszczanie tasków, które pracują (R11 planu zdalnych backendów), zostaje wykluczone. Zwolnienie slotu dotyczy tylko tasków bez pracy w toku.
- Zmiana limitu slotów albo kolejności kolejki.
- Pauza na polecenie właściciela działa jak dziś.

---

## Key Decisions

- Slot według stanu wykonania, nie tylko według życia procesu: proces bezczynnego runnera nie jest pracą, a limit ma chronić pracę.
- Stany chwilowe mają okres oczekiwania zamiast natychmiastowego zwolnienia: szybka odpowiedź właściciela nie czeka w kolejce.
- Failed to stan chwilowy: restart kroku zwykle następuje szybko po analizie błędu.
- Runner tylko do odczytu zamiast zostawiania runnera przy życiu albo replikacji danych: pełny odczyt na każdym backendzie, bez kosztu slotu i bez kopii rozmów w centrali.
- Jedna droga odczytu dla wszystkich backendów: centrala nie zna szczegółów backendu. Kosztem jest krótkie opóźnienie przy lokalnym odczycie zatrzymanego taska.
- Praca zawsze przez slot: limit pozostaje twardy (R6/R11 planu zdalnych backendów).
- Zastępuje to KTD4 w części „nie odtwarzać compute dla odczytu” oraz R9–R11 z `docs/brainstorms/2026-10-10-wznawianie-utraconych-zadan-requirements.md` (odczyt storage przez centralę i niedostępność rozmowy zdalnej).

---

## Dependencies / Assumptions

- Runner potrafi zatrzymać się w bezpiecznym punkcie w stanach Completed, Cancelled, input_needed i Failed. Dla input_needed i Failed to założenie niezweryfikowane.
- Automatyczne zatrzymanie runnera przez centralę nie jest utratą workloadu i nie uruchamia polityki wznawiania (D58).
- Zdalny controller może uruchomić runnera do odczytu poza limitem slotów, a backend ma na to zasoby.

---

## Outstanding Questions

### Deferred to Planning

- [Affects R4, R2][Technical] Jakim stanem lifecycle jest task zatrzymany przez centralę: istniejący `Paused` z powodem, czy nowy stan? Od tego zależy, jak TUI odróżnia pauzę właściciela od zwolnienia slotu.
- [Affects R10][Technical] Jak runner wie, że ma działać tylko do odczytu, i jak gwarantuje, że start nie zmienia storage (sesja, reconcile, preflight, init).
- [Affects R15][Technical] Czy runner do odczytu przechodzi w tryb pracy, czy controller go zatrzymuje i uruchamia nowego.
- [Affects R3, R7][Technical] Gdzie centrala trzyma czas okresu oczekiwania, aby restart daemona go nie zerował ani nie zwalniał slotu za wcześnie.
- [Affects R8][Technical] Usunięcie `ProtocolService::with_local_storage` i odczytu storage w centrali (zdarzenia, rozmowy, czat dyrygenta).
- [Affects R12][Needs research] Czas startu poda do odczytu na Kubernetes i czy TUI potrzebuje stanu „wczytywanie” z limitem czasu.
