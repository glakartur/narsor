---
date: 2026-11-11
topic: akcje-taska-pauza-stop-i-panel-akcji
---

# Akcje taska: pauza, stop i jeden panel akcji w TUI

## Summary

Serwer podaje pełną listę akcji dostępnych na tasku, zależną od jego stanu: działa, zatrzymany (pauza lub stop), workflow zakończony. TUI pokazuje tę listę w jednym panelu (Ctrl+.) i traci pozostałe skróty do zarządzania taskiem. Działający task ma dwie akcje: pauza (zatrzymanie w pierwszym możliwym momencie) i stop (natychmiastowe zerwanie). Zatrzymany i zakończony task ma wyraźnie określony zestaw akcji, w tym ręczny wybór kroku, powrót do agenta, restart skryptu, zwolnienie slotu i archiwizację.

---

## Problem Frame

Dokument `docs/brainstorms/2026-10-22-akcje-taska-z-backendu-i-komentarz-w-pauzie-requirements.md` przeniósł decyzję „co wolno" na serwer. Lista akcji dziś ma 12 pozycji (`TaskActionId` w `crates/narsor-protocol/src/controller.rs`): komentarz, odpowiedź, zatwierdź, odrzuć, zezwól, odmów, pauza, wznów, przerwij, przejście, restart, archiwizacja. Klient nadal ma wiele osobnych skrótów klawiszowych do tych akcji (`Action::PauseResume`, `Interrupt`, `Transition`, `Restart`, `Archive` w `src/tui/keymap.rs`). Operator musi pamiętać, który skrót działa w którym stanie.

Obecny model stanów zatrzymania jest niejasny z punktu widzenia operatora:

- „Pauza" i „przerwanie" nie mają ostrego rozróżnienia: kiedy czekamy na koniec bieżącej pracy, a kiedy zrywamy ją od razu.
- Zatrzymany task trzyma slot do upływu czasu oczekiwania. Operator nie może zwolnić slotu od razu, gdy wie, że nic nie zrobi.
- Po zakończeniu workflow operator ma ograniczone możliwości: nie ma jednej, jawnej drogi do ponownego wejścia w dowolny krok z komentarzem do agenta.

---

## Actors

- A1. Operator: osoba obsługująca task w TUI.
- A2. Serwer (control plane): zna stan taska i podaje listę dozwolonych akcji. Odrzuca akcję, która nie jest już dozwolona.
- A3. Klient (TUI): pokazuje akcje z serwera i wysyła wybór operatora. Nie ocenia dostępności.
- A4. Agent: sesja harnessu (Claude Code, Codex i inne), którą prowadzi runner.
- A5. Skrypt: krok workflow wykonujący skrypt, a nie agenta.

---

## Key Flows

- F1. Wstrzymanie działającego taska
  - **Trigger:** Operator otwiera panel akcji (Ctrl+.) na działającym tasku i wybiera „Pauza".
  - **Actors:** A1, A2, A4, A5
  - **Steps:** Serwer przyjmuje pauzę. Task pokazuje stan „wstrzymywany". Krok agentowy kończy się razem z sesją harnessu (harness się wyłącza). Krok skryptowy kończy się, gdy skrypt przestanie się przetwarzać. Wtedy task przechodzi w stan „zatrzymany".
  - **Outcome:** Task stoi. Żadna praca nie została zerwana w połowie.
  - **Covered by:** R4, R5, R7

- F2. Natychmiastowe zatrzymanie (stop)
  - **Trigger:** Operator wybiera „Stop" w panelu akcji działającego taska.
  - **Actors:** A1, A2, A4, A5
  - **Steps:** Dla kroku agentowego runner zrywa sesję od razu. Dla kroku skryptowego runner zrywa skrypt i zabija jego proces. Task przechodzi w stan „zatrzymany".
  - **Outcome:** Task stoi. Bieżąca praca kroku jest przerwana.
  - **Covered by:** R4, R7, R9

- F3. Dalsze kroki po zatrzymaniu
  - **Trigger:** Task jest zatrzymany (pauza lub stop).
  - **Actors:** A1, A2, A3, A4, A5
  - **Steps:** Operator w panelu rozmowy wywołuje akcję na kroku albo wraca do agenta z dodatkowym promptem. W panelu akcji restartuje skrypt, wybiera dowolny krok workflow, zwalnia slot albo archiwizuje task.
  - **Outcome:** Task wraca do pracy, zmienia krok, oddaje slot albo trafia do archiwum.
  - **Covered by:** R11–R16

- F4. Po zakończeniu workflow
  - **Trigger:** Workflow taska jest zakończony.
  - **Actors:** A1, A2, A3, A4
  - **Steps:** Operator wybiera dowolny krok workflow. Jeśli to krok agentowy, może dopisać komentarz. Serwer dokleja komentarz do prompta tego kroku. Albo operator archiwizuje task.
  - **Outcome:** Task wraca do pracy od wybranego kroku albo jest zarchiwizowany.
  - **Covered by:** R17–R19

---

## Requirements

**Lista akcji z serwera**

- R1. Serwer podaje dla każdego taska pełną listę akcji z informacją „dostępna / niedostępna" i powodem niedostępności. Lista zależy od stanu taska: działa, wstrzymywany, zatrzymany, workflow zakończony, w kolejce, w przygotowaniu (provisioning), czeka na człowieka. (Rozszerza R1–R4 dokumentu z 2026-10-22.)
- R2. Serwer odrzuca akcję, która w chwili wysłania nie jest dozwolona, i zwraca powód. Klient pokazuje powód i odświeża listę.
- R3. Klient nie ocenia dostępności akcji. Gdy serwer nie podał listy, klient pokazuje akcje jako niedostępne z powodem „brak danych".

**Panel akcji w TUI**

- R4. W TUI lista akcji taska jest dostępna tylko przez panel akcji, otwierany skrótem Ctrl+.. Panel pokazuje akcje z serwera, także niedostępne, z powodem.
- R5. TUI usuwa pozostałe skróty klawiszowe do zarządzania taskiem (pauza/wznów, przerwij, przejście, restart, archiwizacja i podobne). Skróty nawigacji i edycji (zakładki, lista, edycja tekstu) zostają. Zakres usuwanych skrótów opisuje „Założenia do potwierdzenia" (Z1).
- R6. Akcje, które z natury należą do rozmowy, nie są w panelu akcji. Dotyczy to ręcznego wywołania akcji na kroku i powrotu do agenta z dodatkowym promptem. Operator robi je w panelu rozmowy.

**Task działający**

- R7. Działający task ma dwie akcje zatrzymania: „Pauza" i „Stop". Obie są dostępne dla kroku agentowego i skryptowego.
- R8. Pauza zatrzymuje task w pierwszym możliwym momencie. Dla agenta to chwila, gdy sesja z harnessem się zakończy (harness się wyłączy). Dla skryptu to chwila, gdy skrypt przestanie się przetwarzać. Pauza niczego nie zrywa.
- R9. Stop zatrzymuje task natychmiast. Dla agenta runner od razu zrywa sesję. Dla skryptu runner zrywa skrypt i zabija jego proces.
- R10. Między wywołaniem pauzy a zatrzymaniem task ma widoczny stan „wstrzymywany". W tym stanie „Stop" nadal jest dostępny, żeby operator mógł przyspieszyć zatrzymanie. „Pauza" nie jest dostępna ponownie.

**Task zatrzymany (pauza lub stop)**

- R11. Zatrzymany task, w którym stoi krok agentowy, pozwala wrócić do rozmowy z agentem na dotychczasowej sesji. Operator może wysłać dodatkowy prompt. Dzieje się to w panelu rozmowy.
- R12. Zatrzymany task pozwala ręcznie wywołać akcję workflow na bieżącym kroku. Dzieje się to w panelu rozmowy.
- R13. Zatrzymany task, w którym stoi krok skryptowy, pozwala wykonać restart skryptu.
- R14. Zatrzymany task pozwala ręcznie wybrać dowolny krok workflow.
- R15. Zatrzymany task pozwala zwolnić slot od razu. Po zwolnieniu task zachowuje się tak samo jak task czekający na reakcję operatora, który nie odezwał się w wyznaczonym czasie: nie trzyma slotu i da się go wznowić. Akcja zastępuje czekanie na koniec czasu oczekiwania.
- R16. Zatrzymany task pozwala go zarchiwizować.

**Task po zakończeniu workflow**

- R17. Task po zakończeniu workflow pozwala ręcznie przejść do dowolnego kroku workflow.
- R18. Gdy operator wybierze krok agentowy, może dopisać komentarz. Serwer dokleja go automatycznie do prompta tego kroku. Komentarz jest opcjonalny.
- R19. Task po zakończeniu workflow pozwala go zarchiwizować.

---

## Acceptance Examples

- AE1. **Covers R7, R8.** Dane: task działa i krok agentowy trwa. Gdy operator wybiera „Pauza", task pokazuje „wstrzymywany", agent kończy bieżącą sesję i task przechodzi w „zatrzymany". Sesja nie została zerwana.
- AE2. **Covers R7, R9.** Dane: task działa i krok skryptowy trwa. Gdy operator wybiera „Stop", skrypt jest zerwany, jego proces zabity, a task jest „zatrzymany" od razu.
- AE3. **Covers R10.** Dane: task jest „wstrzymywany". Panel akcji pokazuje „Pauza" jako niedostępną z powodem „zadanie jest już wstrzymywane" i „Stop" jako dostępny.
- AE4. **Covers R11, R6.** Dane: task jest zatrzymany po pauzie, stoi krok agentowy. Gdy operator pisze w panelu rozmowy, agent dostaje prompt na dotychczasowej sesji. Panel akcji nie zawiera tej czynności.
- AE5. **Covers R15.** Dane: task jest zatrzymany i trzyma slot. Gdy operator wybiera „Zwolnij slot", slot jest wolny od razu, a następny task z kolejki może wystartować. Task da się potem wznowić.
- AE6. **Covers R17, R18.** Dane: workflow jest zakończony. Operator wybiera krok agentowy i wpisuje komentarz. Krok rusza z promptem, do którego serwer dokleja komentarz. Bez komentarza krok rusza ze zwykłym promptem.
- AE7. **Covers R5.** Dane: stary skrót „przerwij" nie jest już przypisany. Gdy operator go naciska w widoku taska, nic się nie dzieje, a panel akcji (Ctrl+.) dalej oferuje „Stop".
- AE8. **Covers R2.** Dane: operator wybrał „Archiwizuj", a w międzyczasie stan taska zmienił się tak, że akcja jest niedozwolona. Serwer odrzuca ją z powodem. Klient pokazuje powód i odświeża panel.

---

## Success Criteria

- Operator zarządza taskiem jednym skrótem (Ctrl+.) i jednym panelem. Nie musi pamiętać skrótów zależnych od stanu.
- Operator rozumie różnicę: pauza czeka na koniec bieżącej pracy, stop zrywa ją od razu.
- Operator zwalnia slot zatrzymanego taska bez czekania na koniec czasu oczekiwania.
- Po zakończeniu workflow operator wraca do dowolnego kroku z komentarzem bez obejść.
- Planista nie musi wymyślać, które akcje należą do którego stanu. Podział R7–R19 na grupy stanów (task działający, task zatrzymany, task po zakończeniu workflow) to rozstrzyga.

---

## Scope Boundaries

- Brak nowych wariantów workflow ani zmiany znaczenia przejść (`allow_manual_override`).
- Brak zmiany czasu oczekiwania na reakcję (jego wartość i konfiguracja). Zmienia się tylko możliwość wcześniejszego zwolnienia slotu.
- Brak nowych ról i uprawnień. Kto może wykonać akcję, określa dotychczasowa autoryzacja.
- Brak zmian w panelu rozmowy poza dodaniem czynności z R6, R11, R12 i komentarza z R18, o ile już tam nie są.
- Brak zmiany skrótów nawigacji, zakładek i edycji tekstu.
- Brak akcji grupowych na wielu taskach.

---

## Key Decisions

- Jeden panel akcji zamiast wielu skrótów. Dlaczego: operator ma jedno miejsce, a serwer jest jedynym źródłem prawdy o dostępności.
- Pauza i stop to dwie osobne akcje, a nie jedna z potwierdzeniem. Dlaczego: różnią się skutkiem (praca nie jest zerwana albo jest zerwana) i operator wybiera świadomie.
- Czynności związane z rozmową zostają w panelu rozmowy. Dlaczego: wymagają pola tekstowego i kontekstu rozmowy. Panel akcji nie jest do tego dobrym miejscem.
- „Zwolnij slot" zastępuje czekanie na koniec czasu oczekiwania. Dlaczego: korzysta z istniejącego zachowania zwolnienia slotu (`docs/brainstorms/2026-10-18-zwalnianie-slotow-wg-stanu-wykonania-requirements.md`), a daje operatorowi kontrolę.

---

## Dependencies / Assumptions

- Zweryfikowane w kodzie: `TaskActionId` ma dziś dwanaście akcji, w tym `Pause`, `Resume`, `Interrupt`, `Transition`, `Restart`, `Archive`. `RunnerState` ma już stany `PauseRequested` i `Stopped`. `SlotReleaseScheduled` i `SlotReleased` istnieją w protokole (`crates/narsor-protocol/src/controller.rs`). Reguły dostępności są w `crates/narsor-engine/src/controller_protocol/service/actions.rs`.
- Zweryfikowane w kodzie: `src/tui/keymap.rs` ma osobne akcje klawiszowe `PauseResume`, `Interrupt`, `Transition`, `Restart`, `Archive`.
- Niezweryfikowane: czy dziś „przerwij" (`Interrupt`) zachowuje się dokładnie jak „stop" z R9 dla agenta i dla skryptu (zabicie procesu). Do sprawdzenia w planowaniu.
- Niezweryfikowane: czy terminal operatora przekazuje Ctrl+. do aplikacji. Część terminali tego nie robi. Do sprawdzenia w planowaniu (patrz Z2).

---

## Założenia do potwierdzenia

- Z1. Usuwamy skróty do: pauza/wznów, przerwij, ręczne przejście, restart, archiwizacja, przenoszenie w kolejce i podobne czynności zarządzania taskiem. Zostają skróty nawigacji, zakładek, edycji tekstu i wyjścia. Wybrano szeroką interpretację zdania „rezygnujemy z pozostałych skrótów dotyczących zarządzania taskiem".
- Z2. Jeśli terminal nie przekaże Ctrl+., TUI nadal musi dać dostęp do panelu. Zakładamy jeden dodatkowy, zapasowy skrót otwierający ten sam panel. Zapasowy skrót nie daje dostępu do pojedynczych akcji. To jedyny wyjątek od R5.
- Z3. Akcja „Wznów" z dokumentu 2026-10-22 zostaje w panelu akcji zatrzymanego taska jako powrót do pracy bez komentarza. Powrót z promptem idzie przez panel rozmowy (R11). Stop i pauza po tym samym wznowieniu wracają do pracy tak samo.
- Z4. Po „Stop" kroku agentowego sesja agenta zostaje zachowana. Powrót (R11) używa tej sesji. Gdy harness nie potrafi jej wznowić, operator widzi błąd z powodem i może wybrać inny krok (R14).
- Z5. Po „Stop" kroku skryptowego task stoi na tym kroku. Restart skryptu (R13) startuje skrypt od początku.
- Z6. Pauza dotyczy tylko taska w stanie „działa”. Stop dotyczy tasków w stanie „działa” i „wstrzymywany” (R10). Task w kolejce, w trakcie przygotowania (provisioning) lub czekający na człowieka nie ma tych akcji. Czeka na człowieka — wtedy działają dotychczasowe akcje z dokumentu 2026-10-22.
- Z7. „Zwolnij slot" jest dostępne tylko dla taska zatrzymanego, który trzyma slot. Task bez slotu nie ma tej akcji.
- Z8. Archiwizacja wciąż działa w każdym stanie zgodnie z regułą „archiwizacja wygrywa" (obecne zachowanie serwera). Wymagania R16 i R19 tylko potwierdzają jej obecność w tych stanach i nie zawężają jej.
- Z9. Po zakończeniu workflow wybór kroku nie ogranicza się do kroków wcześniej odwiedzonych. Dowolny krok workflow jest dozwolony.
- Z10. Komentarz z R18 jest zwykłym tekstem. Dotyczy tylko kroku, który operator wybrał. Nie jest zapisywany jako trwały element workflow.

## Pytania otwarte dla użytkownika

- P1. Zapasowy skrót do panelu akcji (Z2): czy jest dopuszczalny, jeśli Ctrl+. nie działa w części terminali? Zalecane: tak, jeden zapasowy skrót. Alternatywa: tylko Ctrl+., bez zapasowego.
- P2. Wznowienie po pauzie bez promptu (Z3): czy akcja „Wznów" ma zostać w panelu akcji? Zalecane: tak. Alternatywa: wznowienie tylko przez panel rozmowy.
- P3. Zakres usuwanych skrótów (Z1): czy przenoszenie taska w kolejce też przechodzi do panelu akcji? Zalecane: tak. Alternatywa: ten skrót zostaje, bo jest operacją na liście, a nie na tasku.
- P4. Stan po „Stop" agenta (Z4): czy przy stopie sesja ma zawsze zostać możliwa do wznowienia? Zalecane: tak, z błędem i wyborem innego kroku, gdy się nie da. Alternatywa: stop kończy sesję bezpowrotnie i powrót zaczyna nową sesję.

## Outstanding Questions

### Resolve Before Planning

Brak. P1–P4 zapisano jako założenia Z1–Z4. Zmiana któregoś z nich zmienia tylko wskazane wymagania.

### Deferred to Planning

- [Affects R8, R9][Needs research] Jak runner rozróżnia pauzę i stop dla agenta i dla skryptu oraz czy obecne „przerwij" już odpowiada stopowi.
- [Affects R1, R15][Technical] Jak opisać nowe akcje (stop, zwolnij slot, restart skryptu, wybór kroku) w protokole z zachowaniem zgodności ze starszymi klientami.
- [Affects R4, R5][Needs research] Które terminale przekazują Ctrl+. i jak wygląda zapasowy skrót.
- [Affects R18][Technical] Jak serwer dokleja komentarz do prompta kroku, aby użyć istniejącego przekazu kontekstu między krokami (`docs/brainstorms/2026-10-18-przekaz-kontekstu-miedzy-krokami-requirements.md`).
