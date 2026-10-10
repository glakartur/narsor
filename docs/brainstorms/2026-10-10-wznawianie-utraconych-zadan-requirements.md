---
date: 2026-10-10
topic: wznawianie-utraconych-zadan
---

# Wznawianie zadań po utracie runnera i odczyt rozmowy bez runnera

## Summary

Control plane sam wznawia każde zadanie, które straciło runnera, na każdym backendzie. Zadanie `Interrupted` wznawia od razu, a `Unknown` dopiero po potwierdzeniu, że runner nie żyje. Robi to z limitem prób i rosnącym odstępem. Po wyczerpaniu limitu zadanie zostaje przerwane z powodem i czeka na operatora. Rozmowę lokalnego zadania bez żywego runnera da się czytać z zapisu na dysku, tylko do odczytu, z oznaczeniem wieku zapisu.

---

## Problem Frame

Restart daemona (np. po przebudowaniu binarki) zabija runnery lokalnych zadań. Każde z nich przechodzi w `Interrupted` z obserwacją `WorkloadLost`, trzyma swój slot i czeka, aż operator sam naciśnie `u` (wznów), `s` (od nowa) albo `a` (archiwizuj). Reguła „utracony workload nie jest restartowany sam” pochodzi z planu ujednoliconego schedulera (`docs/plans/2026-10-07-1923-feat-unified-task-scheduler-plan.md`) i z komentarza w `src/orchestration/lifecycle.rs`.

Artur trafił na to na zadaniu „sprawdź możliwości aktualizacji pakietów”. Zadanie czekało na jego decyzję w kroku `doc_review`, a runner zginął przy restarcie daemona. Zadanie nie było skończone, a mimo to stało martwe, dopóki ktoś go nie wznowił ręcznie.

Bez żywego runnera nie ma też czego czytać. Serwer oddaje treść rozmowy wyłącznie od żywego runnera (KTD4 z `docs/plans/2026-10-01-2329-feat-zdalne-backendy-wykonawcze-plan.md`), więc na takim zadaniu TUI pokazuje „nie udało się wczytać rozmowy: the task's conversation cannot be read right now (Interrupted)”. Nie da się wtedy przeczytać rozmowy kroku, wejść w nią na *Przebiegu* ani przejrzeć wyniku agenta, który właśnie trzeba zatwierdzić. To samo dotyczy zadań wstrzymanych. Przy zadaniach lokalnych zapis rozmowy leży tymczasem na dysku daemona (D51).

---

## Requirements

**Automatyczne wznawianie**
- R1. Gdy zadanie przechodzi w `Interrupted` (utrata workloadu ze znanym stanem), control plane wznawia je sam, tak jak po poleceniu `u` od właściciela. Dotyczy to każdego backendu: lokalnego i zdalnego.
- R2. Wznawiane jest zadanie przerwane w dowolnym miejscu przebiegu: zarówno w punkcie oczekiwania na operatora, jak i w środku akcji agenta. Przerwana akcja może się wykonać ponownie.
- R3. Zadanie w `Unknown` jest wznawiane dopiero wtedy, gdy controller potwierdzi, że poprzedni runner nie żyje: lokalnie przez sprawdzenie procesu, zdalnie przez sprawdzenie workloadu w backendzie. Po potwierdzeniu zadanie przechodzi w `Interrupted` i wznawia się według R1. Gdy runner żyje albo nie da się tego stwierdzić, zadanie zostaje w `Unknown` i czeka na operatora.
- R4. Zadania wstrzymane przez operatora (`Paused`) nie są wznawiane automatycznie.
- R5. Wznawianie ma limit prób z rosnącym odstępem między nimi. Licznik dotyczy kolejnych utrat bez udanego startu runnera.
- R6. Po wyczerpaniu limitu zadanie zostaje w `Interrupted` z powodem w rodzaju „nie udało się wznowić automatycznie: …” i czeka na `u`/`s`/`a` jak dziś. Ręczne `u` albo `s` nadal działa i zeruje licznik.
- R7. Każde automatyczne wznowienie zostawia ślad widoczny dla operatora: zdarzenie w historii zadania oraz powiadomienie w TUI (np. „wznowiono automatycznie po utracie runnera”). Wyczerpanie limitu też zostawia zdarzenie i powiadomienie.
- R8. Wznowione zadanie wraca w to samo miejsce przebiegu. Zadanie czekające na decyzję operatora czeka dalej na tę samą decyzję, a odpowiedź, którą operator zaczął pisać, nie ginie.

**Odczyt rozmowy bez żywego runnera**
- R9. Gdy lokalne zadanie nie ma żywego runnera (wstrzymane, w trakcie wznawiania, po wyczerpaniu limitu), jego rozmowy da się czytać z zapisu na dysku. Dotyczy to TUI (zakładka *Rozmowa*, rozmowa kroku na *Przebiegu*) i CLI (`narsor task conversation`).
- R10. Rozmowa czytana z zapisu jest tylko do odczytu i ma widoczne oznaczenie, że to zapis, a nie odczyt na żywo (np. „zapis sprzed 3 min”). Po powrocie żywego runnera odczyt wraca do niego bez działania operatora.
- R11. Zadania zdalne bez żywego runnera zachowują się jak dziś: rozmowa jest niedostępna z podanym powodem (KTD4).

---

## Acceptance Examples

- AE1. **Covers R1, R7, R8.** Mamy lokalne zadanie czekające na approval w `doc_review`. Artur restartuje daemona. Po starcie daemon wznawia zadanie, TUI pokazuje powiadomienie o automatycznym wznowieniu, a zadanie znowu czeka na ten sam approval. Rozmowa kroku `doc_review` jest czytelna na *Przebiegu*.
- AE2. **Covers R2.** Runner lokalnego zadania ginie w środku akcji agenta w kroku `implement`. Zadanie wznawia się samo, a w historii zdarzeń widać utratę i automatyczne wznowienie.
- AE3. **Covers R5, R6.** Wznowione zadanie traci runnera zaraz po starcie, bo binarka harnessu zniknęła. Po wyczerpaniu limitu prób zostaje w `Interrupted` z powodem „nie udało się wznowić automatycznie: …”. Kolejne wznowienia nie następują same, a `u` nadal działa.
- AE4. **Covers R3.** Zadanie jest w `Unknown`, a jego runner wciąż żyje. Zadanie zostaje w `Unknown` i nie powstaje drugi runner na tym samym storage.
- AE5. **Covers R4, R9, R10.** Artur wstrzymuje lokalne zadanie. Zadanie nie wznawia się samo, a jego rozmowę da się przeczytać z oznaczeniem „zapis sprzed …”.
- AE6. **Covers R11.** Zdalne zadanie po utracie Poda czeka na wznowienie. Przez ten czas TUI pokazuje „rozmowa niedostępna” z powodem, jak dziś.

---

## Success Criteria

- Restart daemona nie zostawia lokalnych zadań martwych: każde, które działało albo czekało, samo wraca do życia.
- Rozmowę lokalnego zadania da się zawsze przeczytać, żywą albo z zapisu. Komunikat „cannot be read right now” pojawia się tylko przy zadaniach zdalnych bez runnera.
- Żadne automatyczne wznowienie nie przechodzi po cichu: operator widzi je w powiadomieniach i w historii zdarzeń.
- Planowanie nie musi wymyślać, które stany są wznawiane, co się dzieje po wyczerpaniu limitu ani jak oznaczać rozmowę czytaną z zapisu.

---

## Scope Boundaries

- Brak automatycznego wznawiania zadań wstrzymanych (R4).
- Brak odczytu rozmowy z zapisu dla zadań zdalnych: storage zdalnego zadania nie jest dostępny dla serwera, a KTD4 wyklucza centralną kopię surowych rozmów.
- Brak konfiguracji polityki wznawiania per workflow albo per zadanie, np. wyłączenia jej dla kroków z efektami ubocznymi.
- Brak zmian w tym, jak pojedyncza akcja agenta jest ponawiana w ramach `retry_policy` kroku.

---

## Key Decisions

- **Wznawiane jest każde przerwane zadanie, także w środku akcji agenta.** Reguła zostaje prosta, kosztem możliwego powtórzenia efektów przerwanej akcji. Ślad z R7 sprawia, że to powtórzenie nie przechodzi po cichu.
- **Wznawianie dotyczy wszystkich backendów.** Wznowienie to w control plane to samo przejście dla każdego backendu (`Resume` z `Interrupted`/`Unknown` przechodzi w `Provisioning`). KTD4 zabrania tworzenia zasobów obliczeniowych przy *odczycie*, a nie świadomej reguły wznawiania. Dla zadań zdalnych każde wznowienie to nowy Pod, dlatego limit prób jest obowiązkowy.
- **Odczyt z zapisu tylko dla zadań lokalnych.** Daemon ma ich zapis rozmów na własnym dysku (D51). Precedens: stare wykonania historii też są funkcją wyłącznie lokalnego daemona, a inne serwery jej odmawiają z podanym powodem.
- **`Unknown` najpierw sprawdzić.** Dwa runnery na jednym storage są groźniejsze niż zadanie czekające na operatora.
- **Ta decyzja zastępuje regułę z planu ujednoliconego schedulera**, według której utracony workload czeka na jawne polecenie właściciela. Wymaga nowego wpisu w `ARCHITECTURE_DECISIONS.md` (D58).

---

## Dependencies / Assumptions

- Zakładam, że wznowienie zadania czekającego na operatora (`u` dziś) przywraca runnera do tego samego punktu oczekiwania, bez ponownego wykonania zakończonych kroków. Nie zweryfikowałem tego na zadaniu zdalnym.
- Zakładam, że controller zdalny potrafi stwierdzić, czy workload zadania jeszcze istnieje, co jest potrzebne do R3.

---

## Outstanding Questions

### Deferred to Planning

- [Affects R5][Technical] Liczba prób i krzywa odstępów. Czy limit jest wspólny dla wszystkich backendów, czy konfigurowalny per backend?
- [Affects R1][Technical] Gdzie żyje polityka wznawiania: w control plane (reakcja na obserwację `WorkloadLost`) czy w controllerze backendu. Jak ma współgrać z dzierżawą (lease) controllera i z odzyskiwaniem po starcie daemona (`recover_at_startup`)?
- [Affects R3][Needs research] Jak lokalny controller sprawdza, że proces runnera nie żyje. Jak robi to controller Kubernetes (Pod usunięty, `Failed`, węzeł nieosiągalny)?
- [Affects R9, R10][Technical] Jak odczyt z zapisu przechodzi przez obecny kontrakt rozmowy (dostępność, stronicowanie, kursor po numerze wpisu) i jak TUI odróżnia zapis od odczytu na żywo.
- [Affects R8][Technical] Czy szkic odpowiedzi w panelu „Czeka na Ciebie” przeżywa zmianę lifecycle'u zadania przy wznowieniu (identyfikator oczekiwania).
