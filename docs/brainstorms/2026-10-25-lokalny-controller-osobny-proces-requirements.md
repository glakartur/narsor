---
date: 2026-10-25
topic: lokalny-controller-osobny-proces
---

# Lokalny controller jako osobny proces `narsor-controller`

## Summary

Controller backendu `local` przestaje działać wewnątrz procesu `narsor-server`. Staje się podkomendą `narsor-controller local`, obok `narsor-controller kubernetes`. W trybie `trusted` daemon sam uruchamia, rejestruje i nadzoruje ten proces, więc dla usera nic się nie zmienia (faza A). Potem ten sam proces może działać na laptopie jako backend zdalnego serwera w trybie `network`, tylko z jawną zgodą właściciela (faza B).

Dokument opisuje wymagania do planu `docs/plans/2026-10-25-004-refactor-lokalny-controller-osobny-proces-plan.md`. Plan powstał przed tym dokumentem. Tu są zapisane zachowania i granice, które plan musi spełnić.

---

## Problem Frame

Dziś daemon (`crates/narsor-server/src/runtime.rs`) uruchamia controller backendu `local` jako zadanie we własnym procesie. Ten controller rejestruje się bez HTTP i łączy się z API z pominięciem bramki proxy. W trybie `network` rejestracja dostawała `403` i serwer kończył pracę. Commit `83fbb68` wyłączył ten controller w trybie `network` (`ServerSecurity::runs_local_controller` w `crates/narsor-server/src/api.rs`).

Skutki:

- Backend `local` jest wyjątkiem: ma inny cykl życia i inną rejestrację niż backend Kubernetes.
- Serwer w trybie `network` nie ma backendu na komputerze usera. Zadanie na `local` czeka jako `controller_offline`.
- Nie można użyć laptopa jako backendu serwera w k8s, choć protokół controllera na to pozwala (controller otwiera tylko połączenia wychodzące).
- `narsor-controller` ma flagę `--backend` z jedną wartością (`kubernetes`) i ręcznie rozpoznaje `edit-claude-profile` w `main.rs`.

---

## Actors

- A1. User w trybie `trusted`: uruchamia daemon na swoim komputerze i tworzy zadania na backendzie `local`.
- A2. Właściciel serwera w trybie `network`: deklaruje backendy w katalogu i tworzy kody rejestracji.
- A3. User laptopa w fazie B: ręcznie uruchamia `narsor-controller local` i łączy go ze zdalnym serwerem. Zwykle to ta sama osoba co A2 (instalacja ma jednego właściciela).
- A4. Operator klastra: wdraża `narsor-controller kubernetes` przez manifesty w `deploy/kubernetes/`.

---

## Key Flows

- F1. Start daemona w trybie `trusted`
  - **Trigger:** A1 uruchamia daemon (bezpośrednio albo przez CLI/TUI).
  - **Actors:** A1
  - **Steps:** Daemon uruchamia API. Gdy nie ma poświadczenia lokalnego controllera, daemon tworzy kod rejestracji. Daemon uruchamia `narsor-controller local` jako proces potomny. Proces się rejestruje (albo używa istniejącego poświadczenia) i bierze lease backendu `local`. Dopiero wtedy daemon staje się dostępny dla klientów.
  - **Outcome:** Zadanie utworzone zaraz po starcie od razu ma controller.
  - **Failure path:** Gdy proces potomny kończy się przed lease albo nie ma binarki, daemon kończy start z błędem i nie staje się dostępny.
  - **Covered by:** R3, R4, R5, R6, R7

- F2. Zatrzymanie i awaria daemona
  - **Trigger:** `narsor-server stop`, `SIGTERM` albo `kill -9` daemona; albo nieoczekiwany koniec procesu potomnego.
  - **Actors:** A1
  - **Steps:** Przy zatrzymaniu daemon zatrzymuje proces potomny, proces zwalnia lease, potem daemon zamyka API. Po `kill -9` daemona proces potomny też się kończy. Po nieoczekiwanym końcu procesu potomnego daemon uruchamia go ponownie.
  - **Outcome:** Nie zostają osierocone procesy controllera. Runnery działających zadań żyją dalej.
  - **Covered by:** R8, R9

- F3. Laptop jako backend serwera w trybie `network` (faza B)
  - **Trigger:** A2 chce wykonywać zadania serwera na swoim laptopie.
  - **Actors:** A2, A3
  - **Steps:** A2 deklaruje w katalogu backend `kind: local` o własnym id i z `trust_scope: workstation`. A2 tworzy kod rejestracji z jawną zgodą i widzi ostrzeżenie. A3 uruchamia `narsor-controller local` na laptopie z adresem `https://` serwera. Controller się rejestruje i bierze lease.
  - **Outcome:** Z jednego TUI widać zadania na backendach `k8s` i na laptopie tego samego serwera.
  - **Failure path:** Bez zgody serwer nie wydaje kodu. Bez `https://` (adres spoza loopback) controller nie startuje w buildzie wydania. Po utracie laptopa A2 unieważnia controller przez `narsor-cli controller revoke <id>`.
  - **Covered by:** R12, R13, R14, R15, R16, R17, R18

---

## Requirements

**Faza A: komenda `narsor-controller`**
- R1. `narsor-controller` ma podkomendy backendu: `kubernetes` i `local`. `edit-claude-profile` jest podkomendą `kubernetes`. Każda podkomenda ma własne `--help` z tylko swoimi argumentami.
- R2. Argumenty połączenia (adres serwera, adres dla runnerów, plik poświadczenia, kod rejestracji, czas lease) są wspólne dla obu podkomend i czytają te same zmienne `NARSOR_*` co dziś. Manifesty zmieniają tylko komendę.
- R3. `narsor-controller` bez podkomendy kończy się błędem użycia z podpowiedzią nowej składni. Nie ma okresu przejściowego dla starej składni. `deploy/kubernetes/controller.yaml` i dokumentacja dostają nową składnię w tej samej zmianie.
- R4. `narsor-controller` zapisuje własne logi na stderr (poziom z `RUST_LOG`). Logi lokalnego controllera, które dziś idą przez log daemona, nadal są widoczne w logu daemona w trybie `trusted`.

**Faza A: `narsor-controller local` uruchamiany przez daemon**
- R5. `narsor-controller local` steruje adapterem `LocalBackend` tak samo jak dziś wbudowany controller: ten sam układ storage, te same ustawienia `HOME` i profilu harnessu dla runnera, `narsor-runner` obok binarki controllera.
- R6. W trybie `trusted` daemon uruchamia `narsor-controller local` jako proces potomny, rejestruje go automatycznie i przekazuje mu rozwiązane ustawienia. Proces potomny nie czyta pliku konfiguracji Narsora ani obcych zmiennych `NARSOR_*` ze środowiska usera. User nie wykonuje nowego kroku.
- R7. Daemon staje się dostępny (plik discovery) dopiero wtedy, gdy proces potomny trzyma **nowy** lease backendu `local` (z tej inkarnacji daemona, nie z poprzedniej po awarii). Gdy proces potomny kończy się przed lease, nie ma binarki albo mija limit czasu, daemon kończy start z błędem i z ostatnimi liniami stderr procesu potomnego.
- R8. Przy zatrzymaniu daemon najpierw zatrzymuje proces potomny (ten zwalnia lease, póki API działa), potem zamyka API. Po `kill -9` daemona proces potomny też się kończy. Nigdy nie działają dwa lokalne controllery tego samego daemona naraz. Przy starcie `narsor-controller local` bierze wyłączną blokadę pliku w katalogu poświadczenia. Gdy blokada jest zajęta, proces czeka na nią do limitu czasu gotowości z R7 i zapisuje w logu PID właściciela blokady. Gdy limit mija, proces kończy się błędem z tym PID. Dzięki temu natychmiastowy restart po `kill -9` (AE4) nie kończy się błędem, gdy stary proces potomny jeszcze się zamyka.
- R9. Po nieoczekiwanym końcu procesu potomnego daemon uruchamia go ponownie z rosnącym opóźnieniem, bez nowej rejestracji. W tym czasie zadania `local` pokazują `controller_offline`. Runnery działających zadań żyją dalej.
- R10. Istniejące instalacje zachowują tożsamość lokalnego controllera i storage zadań. Nie ma ponownej rejestracji ani migracji danych.
- R11. `narsor-server` nie zawiera kodu pętli controllera. Tryb `network` nadal nie uruchamia lokalnego controllera (zachowanie z `83fbb68`).

**Ochrona sekretów (faza A i B)**
- R12. Kod rejestracji nigdy nie jest widoczny w argumentach procesu (`ps`, historia powłoki). `narsor-controller local` przyjmuje go tylko ze zmiennej środowiskowej albo z pliku. Runnery nie dostają kodu rejestracji ani innych zmiennych `NARSOR_*` controllera.

**Faza B: laptop jako backend zdalnego serwera**
- R13. Katalog serwera może zadeklarować backend `kind: local` o dowolnym id innym niż `local`. Id `workstation` w tym dokumencie to tylko przykład, nie zalecana nazwa. Serwer w trybie `network` go obsługuje. Id `local` jest zarezerwowane dla wbudowanego backendu trybu `trusted`: w trybie `network` wpis o id `local` jest odrzucany przy starcie z podpowiedzią innego id, a zadanie na `local` jest odrzucane przy tworzeniu z czytelnym błędem.
- R14. Backend `kind: local` o id innym niż `local` musi mieć `trust_scope: workstation`. Inna wartość daje błąd katalogu z nazwą pola.
- R15. Kod rejestracji dla backendu `kind: local` z `trust_scope: workstation` wymaga jawnej zgody w żądaniu. Serwer sprawdza zgodę (nie tylko CLI) po `kind` i `trust_scope`. Poza zarezerwowanym id `local` serwer nie sprawdza id. Wyjątek: wbudowany backend o id `local` w trybie `trusted`. Daemon tworzy dla niego kod w procesie, bez pola zgody (ten backend też ma `kind: local` i `trust_scope: workstation`). CLI ma flagę zgody i wypisuje ostrzeżenie: przejęty serwer może uruchamiać dowolny kod na tym komputerze jako jego user, bez izolacji kontenera.
- R16. `narsor-controller local` łączy się z serwerem spoza loopback tylko przez `https://`, zawsze ze sprawdzeniem certyfikatu. Nie ma flagi, która wyłącza sprawdzenie. Wyjątek `http://` jest tylko w buildach debug. Serwer jest na loopback tylko wtedy, gdy host w adresie to literalne IP loopback (`127.0.0.0/8`, `::1`) albo dokładnie `localhost`. Controller nie używa DNS do tej decyzji i nie przechodzi za przekierowaniem na inny host. Każdy inny host jest spoza loopback.
- R17. Gdy serwer nie jest na loopback, `narsor-controller local` traktuje polecenia serwera jako niezaufane: domyślnie `--home private`; `--home host` wymaga flagi potwierdzenia i pokazuje ostrzeżenie; ścieżki z launch spec są tylko wewnątrz katalogu storage (sprawdzenie po rozwiązaniu dowiązań i `..`); controller zawsze uruchamia stały `narsor-runner` obok swojej binarki, a z launch spec przyjmuje tylko pola z listy dozwolonych i odrzuca zmienne środowiskowe spoza listy (np. `PATH`, `LD_PRELOAD`); plik poświadczenia ma prawa `0600`. Przy starcie controller wypisuje adres serwera, id backendu i ostrzeżenie z R15.
- R18. W fazie B `narsor-controller` uruchomiony ręcznie (nie przez daemon) czyta plik konfiguracji Narsora (`--config` albo ścieżka domyślna) dla ustawień `HOME`. Kolejność: flagi, potem zmienne środowiskowe, potem plik. Reguły R17 obowiązują także dla wartości z pliku: `home: host` z pliku wymaga flagi potwierdzenia. Proces potomny daemona (R6) nadal nie czyta pliku.

---

## Acceptance Examples

- AE1. **Covers R1, R3.** Given nowa wersja, when operator uruchamia `narsor-controller` bez podkomendy, then kod wyjścia 2 i komunikat wskazuje `kubernetes` i `local`.
- AE2. **Covers R1.** Given brak adresu serwera i poświadczenia, when operator uruchamia `narsor-controller kubernetes edit-claude-profile`, then komenda nie żąda argumentów połączenia.
- AE3. **Covers R6, R7.** Given czysty stan, when user startuje daemon `trusted`, then istnieje proces `narsor-controller local` (dziecko daemona), plik discovery pojawia się po lease, a zadanie `local` dochodzi do końca.
- AE4. **Covers R7, R8.** Given `kill -9` daemona i natychmiastowy restart (stary lease jeszcze ważny), when daemon startuje, then plik discovery pojawia się dopiero po lease z nowej inkarnacji, a działa tylko jeden lokalny controller.
- AE5. **Covers R7.** Given brak binarki `narsor-controller` obok `narsor-server`, when user startuje daemon, then daemon kończy start z kodem 1 i nazwą brakującego pliku; pliku discovery nie ma.
- AE6. **Covers R9.** Given działający daemon, when proces potomny dostaje `kill -9`, then daemon uruchamia go ponownie, lease wraca, nowe zadanie startuje, a nie powstaje nowy kod rejestracji.
- AE7. **Covers R10.** Given instalacja z istniejącym poświadczeniem lokalnego controllera, when user startuje nową wersję daemona, then controller ma tę samą tożsamość, a stare zadania są w tym samym storage.
- AE8. **Covers R6, R12.** Given w powłoce usera `NARSOR_RUNNER_CONTROL_PLANE=http://example`, when daemon uruchamia proces potomny, then proces potomny go nie widzi, a runnery dostają adres daemona i nie mają w środowisku kodu rejestracji.
- AE9. **Covers R11.** Given tryb `network`, when daemon startuje, then nie ma procesu potomnego (istniejący test `a_network_daemon_starts_without_the_built_in_local_controller` przechodzi bez zmian).
- AE10. **Covers R14, R15.** Given backend `laptop` (`kind: local`, `trust_scope: workstation`), when klient API żąda kodu bez pola zgody, then `400` z kodem błędu i opisem ryzyka; z flagą zgody CLI dostaje kod i wypisuje ostrzeżenie.
- AE11. **Covers R14.** Given backend `kind: local` o id `workstation` z `trust_scope: dedicated`, when serwer używa katalogu, then błąd wskazuje pole `trust_scope`.
- AE12. **Covers R13.** Given tryb `network` i plik `backends/local.yaml`, when daemon startuje, then błąd startu z podpowiedzią innego id.
- AE13. **Covers R16, R17.** Given `--control-plane http://10.0.0.5:8443` w buildzie wydania, when user uruchamia `narsor-controller local`, then kod 2 z komunikatem o `https://`. Given serwer spoza loopback i `--home host` bez flagi potwierdzenia, then kod 2 z opisem ryzyka.
- AE14. **Covers R17, R18.** Given serwer spoza loopback i plik konfiguracji z `home: host`, when user uruchamia `narsor-controller local` bez flagi potwierdzenia, then kod 2 z opisem ryzyka. Given ten sam plik i flaga `--home private`, then controller używa `private`.

---

## Success Criteria

- User trybu `trusted` nie widzi różnicy: ten sam start, te same zadania, ta sama tożsamość controllera, a po `stop` nie zostają procesy `narsor-controller`.
- Każdy backend działa tak samo: osobny proces controllera, rejestracja i protokół `/controller/v1` przez HTTP.
- Faza B: z jednego TUI widać zadania na backendzie `k8s` i na laptopie tego samego serwera. Laptopa nie da się zarejestrować bez jawnej zgody właściciela.
- Plan i implementacja nie muszą wymyślać zachowań przy starcie, zatrzymaniu, awarii i rejestracji: AE1–AE14 je określają.

---

## Scope Boundaries

- Brak przeniesienia silnika `/executions` na drogę zadań (controller + runner). To osobny plan.
- Brak izolacji runnera na laptopie (kontener, sandbox). Backend `local` nadal uruchamia runner jako proces usera.
- Brak autostartu `narsor-controller local` na laptopie (np. usługa systemd). W fazie B user uruchamia go sam.
- Brak zmian w adapterze Kubernetes poza przeniesieniem jego argumentów do podkomendy.
- Brak osobnej autoryzacji per backend. Instalacja ma jednego właściciela (`docs/operations/remote-backends.md`, sekcja 13).
- Brak zmian w TUI. TUI nie tworzy kodów rejestracji.
- Aktualizacja planu dystrybucji (`docs/plans/2026-10-25-003-feat-dystrybucja-instalacja-aktualizacja-plan.md`) jest po fazie A, poza tym zakresem.

---

## Key Decisions

- Podkomendy zamiast flagi `--backend`: backendy mają różne wymagane argumenty; każdy ma własne `--help`.
- Jedna binarka `narsor-controller` dla obu backendów: binarka już zależy od kodu `LocalBackend`, więc osobny projekt nic nie zmniejsza.
- Bez okresu przejściowego dla starej składni: wersja nie jest wydana (`docs/brainstorms/2026-09-25-rozproszony-narsor-requirements.md`).
- Daemon przekazuje rozwiązane ustawienia, proces potomny nie czyta konfiguracji: jedno źródło rozwiązywania konfiguracji.
- Gotowość daemona = nowy lease, nie sama rejestracja: zadanie zaraz po starcie ma controller, także po awarii daemona.
- Faza B używa istniejącego `trust_scope: workstation` i zaczyna go sprawdzać: wartość opisuje dokładnie ten przypadek, a dziś nie jest sprawdzana nigdzie.
- Faza B jest w tym samym zakresie co faza A, jako druga faza (decyzja Artura, 2026-10-25).
- Ręcznie uruchomiony `narsor-controller local` czyta plik konfiguracji Narsora dla ustawień `HOME` (R18). Proces potomny daemona go nie czyta (R6) (decyzja Artura, 2026-10-25).
- Id backendu `kind: local` w fazie B jest dowolne. Dokumentacja podaje `workstation` tylko jako przykład (decyzja Artura, 2026-10-25).

---

## Dependencies / Assumptions

- Binarki `narsor-server`, `narsor-controller` i `narsor-runner` są zainstalowane w jednym katalogu. Daemon szuka `narsor-controller` tylko obok siebie, nie w `PATH`.
- Fencing lease przez `epoch` istnieje w protokole controllera i wystarcza do rozpoznania lease z nowej inkarnacji (założenie z planu, KTD5; do sprawdzenia w planowaniu). Fencing chroni raporty, ale nie wyklucza dwóch procesów z tym samym poświadczeniem. Dlatego R8 wymaga blokady pliku.

---

## Założenia do potwierdzenia

- Plan wymienia `deploy/kind/` (pliki i `deploy/kind/server.yaml`), ale w repo nie ma tego katalogu (sprawdzone: `deploy/` zawiera tylko `deploy/kubernetes/`). Zakładam, że zmiana składni dotyczy tylko `deploy/kubernetes/` i dokumentacji, bez nowych plików w `deploy/kind/`. Sekcja 12 w `docs/operations/remote-backends.md` ma tylko testy Kubernetes i nie opisuje fazy B. Dlatego zmiana fazy B dodaje do tego dokumentu test ręczny controllera na laptopie z serwerem w trybie `network`. Test opisuje też, jak laptop ufa certyfikatowi serwera (CA serwera w systemowym magazynie zaufanych certyfikatów), bo R16 nie ma flagi, która wyłącza sprawdzenie.
- Zakładam, że faza B wychodzi razem z fazą A w jednym wydaniu, ale może trafić do osobnego merge requesta. Plan nie określa podziału na merge requesty. Faza A może wyjść bez fazy B: gdy przegląd bezpieczeństwa fazy B nie jest gotowy, wydanie zawiera tylko fazę A.
- Zakładam, że przegląd specjalisty od bezpieczeństwa jest warunkiem wydania fazy B, nie warunkiem jej implementacji (plan, tabela ryzyk).
- Zakładam, że w fazie B domyślny plik poświadczenia na laptopie jest w `$XDG_STATE_HOME/narsor/workstation-controller/` niezależnie od id backendu. Przy kilku serwerach user podaje inną ścieżkę flagą.

---

## Pytania otwarte dla użytkownika

- Brak. Odpowiedzi są w Key Decisions.

---

## Outstanding Questions

### Deferred to Planning

- [Affects R7, R9][Technical] Limit czasu oczekiwania na lease i parametry ponownego startu procesu potomnego. Start od wartości z `Supervision`. Start procesu potomnego i oczekiwanie na lease muszą być wyraźnie krótsze niż `DISCOVERY_TIMEOUT` (15 s, `crates/narsor-api-client/src/api/discovery.rs`). Jeśli nie, ta sama zmiana musi zmienić ten limit. Gdy CLI/TUI uruchamia daemon, błąd z R7 (z liniami stderr procesu potomnego) trafia do `narsor.log`. Komunikat klienta o przekroczeniu limitu czasu musi wskazać userowi ten log.
- [Affects R8][Needs research] Czy `PR_SET_PDEATHSIG` działa poprawnie przy wątkach tokio (sygnał przy końcu wątku, który utworzył proces). Alternatywa: okresowe sprawdzenie `getppid()` albo uruchomienie z wątku żyjącego przez cały proces.
- [Affects R8][Technical] Krótki limit czasu klienta HTTP controllera, żeby po `SIGTERM` przy martwym API próba zwolnienia lease nie trzymała procesu długo.
