---
title: "refactor: Lokalny controller jako osobny proces narsor-controller"
type: refactor
status: active
date: 2026-10-25
---

# refactor: Lokalny controller jako osobny proces narsor-controller

## Summary

Controller backendu `local` przestaje być zadaniem w procesie `narsor-server`. Staje się podkomendą `narsor-controller local`, obok `narsor-controller kubernetes`. W trybie `trusted` daemon uruchamia go jako proces potomny, sam go rejestruje i nadzoruje, więc dla usera nic się nie zmienia. Każdy backend działa wtedy tak samo: osobny proces controllera, rejestracja i protokół `/controller/v1` przez HTTP. W fazie B ten sam `narsor-controller local` może działać na laptopie jako backend zdalnego serwera w trybie `network`, z jawną zgodą i ostrzeżeniem.

---

## Problem Frame

Dziś `run_daemon` (`crates/narsor-server/src/runtime.rs`) uruchamia w swoim procesie wbudowany controller (`start_local_controller`). Ten controller rejestruje się z pominięciem HTTP (`ProtocolService::create_enrollment` wywołane bezpośrednio) i łączy się z API pod adresem nasłuchu, z pominięciem bramki proxy. W trybie `network` jego rejestracja dostawała `403`, a serwer kończył pracę. Commit `83fbb68` wyłączył go w trybie `network` (`ServerSecurity::runs_local_controller`). Backend `local` jest więc wyjątkiem: inny cykl życia, inna rejestracja i niedostępny dla serwera sieciowego. Nie da się też użyć laptopa jako backendu serwera w k8s, choć protokół na to pozwala (połączenia wychodzące).

Plan powstał z rozmowy z Arturem 2026-10-25, bez dokumentu wymagań.

---

## Requirements

**Faza A: wydzielenie (zachowanie trybu `trusted` bez zmian; zmienia się składnia `narsor-controller`)**
- R1. `narsor-controller` ma podkomendy backendu: `kubernetes` i `local`. `edit-claude-profile` jest podkomendą `kubernetes`. Wspólne argumenty połączenia (serwer, adres dla runnerów, plik poświadczenia, kod rejestracji, czas lease) są w jednej strukturze dołączanej do obu podkomend.
- R2. `narsor-controller local` steruje adapterem `LocalBackend` tak samo, jak dziś wbudowany controller: ten sam układ storage, te same argumenty `HOME` i profilu harnessu dla runnera, `narsor-runner` obok binarki controllera.
- R3. W trybie `trusted` daemon uruchamia `narsor-controller local` jako proces potomny, rejestruje go automatycznie (kod jednorazowy przekazany przez środowisko), nadzoruje go i zatrzymuje przy swoim końcu. User nie wykonuje żadnego nowego kroku.
- R4. Daemon staje się dostępny (plik discovery) dopiero wtedy, gdy lokalny controller trzyma **nowy** lease backendu `local`. Dziś daemon czeka tylko na rejestrację i start pętli, a lease pojawia się chwilę później. Ten plan wzmacnia gwarancję, żeby zadanie utworzone zaraz po starcie od razu miało controller.
- R5. Istniejące instalacje zachowują tożsamość lokalnego controllera (`{state}/local-controller/credential`) i storage zadań (`{work}/local-controller/`). Nie ma ponownej rejestracji ani migracji danych.
- R6. `narsor-server` nie zawiera już kodu wbudowanego controllera ani zależności od pętli controllera.

**Faza B: laptop jako backend zdalnego serwera**
- R7. Katalog serwera może zadeklarować backend `kind: local` o innym id niż `local` (np. `workstation`). Serwer w trybie `network` go obsługuje.
- R8. Backend `kind: local` w trybie `network` musi mieć `trust_scope: workstation`. Rejestracja jego controllera wymaga jawnej zgody właściciela i pokazuje ostrzeżenie: przejęty serwer może uruchamiać kod na tym komputerze jako jego użytkownik.
- R9. `narsor-controller local` uruchomiony przez usera łączy się z serwerem po `https://` (dla adresu spoza loopback), zawsze ze sprawdzeniem certyfikatu. Runnery na laptopie dostają ten sam adres.
- R10. Gdy serwer nie jest na loopback, `narsor-controller local` traktuje polecenia serwera jako niezaufane: domyślnie `--home private`, ścieżki tylko w katalogu storage, bez przekazywania własnego środowiska `NARSOR_*` runnerom.

---

## Scope Boundaries

- Brak przeniesienia silnika `/executions` na drogę zadań (controller + runner).
- Brak izolacji runnera na laptopie (kontener, sandbox). Backend `local` nadal uruchamia runner jako proces usera.
- Brak automatycznego startu `narsor-controller local` na laptopie w fazie B (usługa systemd itp.). User uruchamia go sam.
- Brak zmian w adapterze Kubernetes poza przeniesieniem jego argumentów do podkomendy.

### Deferred to Follow-Up Work

- Jednostka systemd / autostart dla `narsor-controller local` na laptopie: po fazie B, gdy wzorzec użycia będzie jasny.
- Przeniesienie `/executions` na drogę zadań: osobny plan (największe uproszczenie serwera, ale też migracja historii).
- Dopasowanie planu dystrybucji (`docs/plans/2026-10-25-003-feat-dystrybucja-instalacja-aktualizacja-plan.md`): U1 tamtego planu (wersje protokołu) obejmuje też lokalny controller; Helm i obrazy bez zmian. Aktualizacja tamtego planu po zakończeniu fazy A.

---

## Context & Research

### Relevant Code and Patterns

- Wbudowany controller: `crates/narsor-server/src/runtime.rs` (`run_daemon`, `start_local_controller`, `ensure_local_backend`, kolejność: lease przed discovery, zatrzymanie controllera przed zamknięciem API).
- Bramka trybu: `crates/narsor-server/src/api.rs` (`ServerSecurity::runs_local_controller`, commit `83fbb68`).
- Binarka controllera: `crates/narsor-controller/src/main.rs` (ręczne rozpoznanie `edit-claude-profile`), `crates/narsor-controller/src/controller.rs` (`ControllerArgs` z `--backend` = `ControllerBackend::Kubernetes`, `ClusterArgs`, `EditProfileArgs`, `run`, `load_or_enroll` jako opakowanie mapujące kody wyjścia, `shutdown_signal`).
- Rejestracja: rdzeń `load_or_enroll_with` w `crates/narsor-controller-runtime/src/lib.rs`; wiązanie z protokołem w `crates/narsor-engine/src/controller_runtime/mod.rs` (`load_or_enroll`, `ProtocolEnrollment`). Używają go serwer i binarka controllera.
- Pętla i nadzór: `crates/narsor-controller-runtime/src/controller.rs` (`Controller`, `ControllerConfig`), `crates/narsor-controller-runtime/src/lib.rs` (`supervise`, `Supervision`).
- Adapter lokalny: `crates/narsor-engine/src/backends/local.rs` (`LocalBackendConfig::new` szuka `narsor-runner` obok `current_exe`, `runner_args`, grupy procesów, `WORKLOAD_MARK_ENV`).
- Argumenty harnessu: `crates/narsor-shared/src/config/harness_home.rs` (`HarnessHome::runner_args`), `crates/narsor-shared/src/config/resolve.rs` (`local_harness_home`).
- Szukanie binarki obok siebie, z fallbackiem dla testów: `crates/narsor-api-client/src/api/launch.rs` (`resolve_exe`).
- Katalog i backend: `crates/narsor-engine/src/environment/backend.rs` (`BackendRecord::LOCAL_ID`, `builtin_local`, `TrustScope` — zadeklarowany, nigdzie nie sprawdzany), `crates/narsor-engine/src/environment/catalog.rs`.
- Manifest controllera: `deploy/kubernetes/controller.yaml` (komenda bez podkomendy, argumenty z env).
- Testy: `tests/daemon_lifecycle.rs`, `tests/cli_smoke.rs`, `tests/api.rs`, `tests/config_startup.rs` (testy z daemonem `#[ignore]`), `tests/backend_contract.rs` (kontrakt `LocalBackend`), `tests/controller_protocol.rs`, `tests/kubernetes_adapter.rs`.

### Institutional Learnings

- Pamięć projektu: testy z daemonem są `#[ignore]` i sprzątają w `Drop`; testy wyciekały procesy daemonów. Nowy proces potomny zwiększa to ryzyko.
- Pamięć projektu: losowy `ETXTBSY` w testach agenta (istniał wcześniej). Testy, które kopiują lub budują binarki w trakcie działania procesów, mogą go wywołać.
- D53: jeden zapisujący stan. Lokalny controller nie zapisuje stanu serwera, tylko storage zadań, więc zasada zostaje.
- D63: runner do odczytu startuje controller, także lokalny. Przenosi się razem z adapterem.
- Wersja nie jest wydana: kontrakt CLI i manifestów zmienia się bez okresu przejściowego (`docs/brainstorms/2026-09-25-rozproszony-narsor-requirements.md`).

### External References

- Brak. Wzorce są w repo.

---

## Key Technical Decisions

- **KTD1. Podkomendy zamiast `--backend`.** Backendy mają różne wymagane argumenty. Przy podkomendach clap wymusza poprawny zestaw, a każdy backend ma własne `--help`. `edit-claude-profile` staje się `narsor-controller kubernetes edit-claude-profile`, co usuwa ręczne parsowanie `argv[1]` w `main.rs`. Wspólne argumenty połączenia (`--control-plane`, `--runner-control-plane`, `--credential-file`, `--enrollment-code`, `--lease-ttl-secs`) są w jednej strukturze dołączanej przez `flatten` i nadal czytają te same zmienne `NARSOR_*`. Kształt clap: `kubernetes` ma opcjonalną podkomendę `edit-claude-profile` i `subcommand_negates_reqs`, więc edycja profilu nie wymaga argumentów połączenia. `--kubernetes-config` i `ClusterArgs` są tylko na poziomie `kubernetes`, a podkomenda je dziedziczy. Komentarz w `main.rs` o „osobnym parserze” znika razem z ręcznym parsowaniem. Kod rejestracji nie jest przyjmowany z wartości w argv dla `local`: tylko `NARSOR_ENROLLMENT_CODE` albo `--enrollment-code-file`, żeby nie trafiał do `ps` i historii powłoki.
- **KTD1a. `narsor-controller` ma własne logi.** Binarka inicjuje subskrybenta `tracing` (EnvFilter, stderr), tak jak `narsor-server`. Dziś logi pętli wbudowanego controllera (np. utrata lease) idą przez subskrybenta daemona. Bez tego zniknęłyby po przeniesieniu. Daemon przekazuje dziecku `RUST_LOG` ze swojego poziomu logów.
- **KTD2. Bez okresu przejściowego dla starej komendy.** `narsor-controller` bez podkomendy kończy się błędem użycia z podpowiedzią `narsor-controller kubernetes`. `deploy/kubernetes/controller.yaml` i `deploy/kind/` dostają podkomendę w tej samej zmianie. Wersja nie jest wydana, więc kompatybilność wsteczna nie jest wymagana.
- **KTD3. Daemon przekazuje dziecku rozwiązane ustawienia, dziecko nie czyta konfiguracji.** Daemon ma już rozwiązaną konfigurację (`Resolved`). Przekazuje `narsor-controller local` katalog storage (`{work}/local-controller`), plik poświadczenia (`{state}/local-controller/credential`), adres API, argumenty `HOME`/profilu harnessu i `--lease-ttl-secs` równy dzisiejszej wartości domyślnej `ControllerConfig` jako flagi. Daemon usuwa ze środowiska dziecka wszystkie zmienne `NARSOR_*` poza tymi, które ustawia sam, bo dziecko dziedziczy środowisko powłoki usera, a argumenty clap czytają `NARSOR_*`. Jest jedno źródło rozwiązywania konfiguracji.
- **KTD4. Kod rejestracji przez zmienną środowiskową, nie argument.** Daemon tworzy kod w procesie (`create_enrollment`), gdy plik poświadczenia nie istnieje, i przekazuje go jako `NARSOR_ENROLLMENT_CODE`. Argument byłby widoczny w `ps`. Gdy poświadczenie istnieje, kod nie powstaje (R5). Controller usuwa `NARSOR_ENROLLMENT_CODE` ze swojego środowiska zaraz po odczycie, a `LocalBackend` nie przekazuje runnerom zmiennych `NARSOR_*` controllera (dziś `spawn` dziedziczy całe środowisko).
- **KTD5. Gotowość = nowy lease backendu `local`.** Lease jest zapisany na dysku i ma TTL. Po `kill -9` daemona lease poprzedniej inkarnacji może być jeszcze ważny. Dlatego przed uruchomieniem dziecka daemon zapamiętuje `epoch` obecnego lease (albo jego brak) i czeka, aż lease backendu `local` będzie miał większy `epoch`, z limitem czasu. Dopiero potem zapisuje plik discovery (R4). Gdy dziecko zakończy się przed uzyskaniem lease, daemon kończy start z błędem i ostatnimi liniami stderr dziecka. Stderr dziecka idzie przez potok, który daemon czyta cały czas (prefiks w logu i bufor ostatnich linii), żeby dziecko nie zablokowało się na pełnym potoku. Po minięciu limitu daemon też kończy start z błędem.
- **KTD6. Nadzór procesu w daemonie.** Daemon uruchamia dziecko w nowej grupie procesów z `PR_SET_PDEATHSIG = SIGTERM`, więc dziecko kończy się także po `SIGKILL` daemona. Po nieoczekiwanym końcu dziecka daemon uruchamia je ponownie z rosnącym opóźnieniem (ten sam kształt co `Supervision`). Przy zamknięciu daemon wysyła dziecku `SIGTERM` i czeka z limitem (dziecko zwalnia lease, póki API przyjmuje żądania), potem zamyka API. Runnery żyją dalej we własnych grupach, jak dziś.
- **KTD7. Binarka controllera obok serwera.** Daemon szuka `narsor-controller` tylko obok `narsor-server`, nigdy w `PATH`. Fallback na katalog profilu cargo działa tylko w buildach debug (jak `NARSOR_TEST_DAEMON_EXE` w `resolve_exe`). Brak binarki to błąd startu z czytelnym komunikatem. `LocalBackendConfig::new` szuka `narsor-runner` obok `current_exe`, czyli teraz obok controllera. Instalacja trzyma wszystkie binarki razem, więc to się nie zmienia.
- **KTD8. Serwer bez pętli controllera.** `start_local_controller`, zależność serwera od `narsor-controller-runtime` (pętla, `supervise`) i od `LocalBackend` znikają z `narsor-server`. `ensure_local_backend` zostaje, bo chroni przed backendem `local` innego rodzaju w katalogu. `ServerSecurity::runs_local_controller` zostaje jako warunek uruchomienia procesu potomnego.
- **KTD9. Faza B używa istniejącego `trust_scope: workstation` i zaczyna go egzekwować.** Nie dodajemy nowej wartości. `workstation` („własny komputer właściciela”) opisuje dokładnie ten przypadek, ale dziś nie jest sprawdzany nigdzie. Reguły i miejsca:
  - `BackendRecord::problems()` (bez znajomości trybu): `kind: local` o id innym niż `local` wymaga `trust_scope: workstation`. Katalog czyta plik przy każdym zapytaniu, więc błąd wychodzi przy pierwszym użyciu backendu, z nazwą pola.
  - `create_enrollment` (bez znajomości trybu): kod dla backendu `kind: local` z `trust_scope: workstation` o id innym niż `local` wymaga pola zgody w żądaniu. Sprawdzenie czyta `kind` i `trust_scope`, nie id backendu. `narsor-cli controller enroll-code <backend> --allow-workstation` ustawia to pole i wypisuje ostrzeżenie.
  - Instalacja ma jednego właściciela (`docs/operations/remote-backends.md`, sekcja 13), więc tylko on tworzy zadania, także na backendzie `workstation`. Osobna autoryzacja per backend nie jest potrzebna, dopóki nie ma wielu userów.
  - Unieważnienie: istniejące `narsor-cli controller revoke <id>`. Dokumentacja opisuje je jako krok po utracie laptopa.
- **KTD10. Id `local` zostaje zarezerwowane dla wbudowanego backendu trybu `trusted`.** Laptop jako backend zdalnego serwera ma inne id (np. `workstation`). W trybie `network` `run_daemon` przy starcie sprawdza katalog `backends/` i odrzuca plik `local.yaml` z podpowiedzią innego id. `EnvironmentCatalog::backend("local")` w trybie `network` nie podstawia `builtin_local` (kontekst trybu trafia do katalogu przez `CatalogLaunchResolver` przy jego tworzeniu w `run_daemon`). Zadanie na `local` w trybie `network` jest więc odrzucane przy tworzeniu z czytelnym błędem, zamiast czekać jako `controller_offline`.

---

## Open Questions

### Resolved During Planning

- Podkomendy czy flaga: podkomendy (KTD1).
- Osobne projekty czy jedna binarka: jedna binarka. `narsor-controller` już zależy od `narsor-engine` z feature `engine`, w którym jest `LocalBackend`, więc osobny projekt nic nie zmniejsza.
- Druga kopia `load_or_enroll`: nie ma drugiej kopii logiki. Rdzeń jest w `narsor-controller-runtime`, a wersja w `narsor-controller` mapuje tylko kody wyjścia. Po U3 serwer przestaje go używać.
- Nowy `trust_scope` czy istniejący: istniejący `workstation`, z egzekwowaniem (KTD9).
- Faza B w tym planie: tak, jako druga faza (decyzja Artura).

### Deferred to Implementation

- Limit czasu oczekiwania na lease (KTD5) i parametry ponownego startu dziecka: wartości domyślne do ustalenia na testach; zacząć od tych z `Supervision`.
- Czy `PR_SET_PDEATHSIG` wystarcza przy wątkach tokio (sygnał przychodzi przy końcu wątku, który utworzył dziecko, nie procesu). Jeśli nie, dziecko sprawdza okresowo `getppid()` albo daemon uruchamia je z wątku żyjącego przez cały proces.
- Czy w fazie B `narsor-controller local` ma czytać plik konfiguracji Narsora (`--config`) dla ustawień `HOME`, czy tylko flagi i zmienne. Zacząć od flag i zmiennych (KTD3), `--config` dodać, jeśli flagi okażą się niewygodne.

---

## High-Level Technical Design

> *This illustrates the intended approach and is directional guidance for review, not implementation specification. The implementing agent should treat it as context, not code to reproduce.*

```mermaid
sequenceDiagram
    participant D as narsor-server (daemon, trusted)
    participant C as narsor-controller local (dziecko)
    participant R as narsor-runner (proces na zadanie)
    D->>D: API nasłuchuje na 127.0.0.1
    alt brak {state}/local-controller/credential
        D->>D: create_enrollment(local) → kod
    end
    D->>C: spawn (flagi: --control-plane, --credential-file, storage, HOME; env: NARSOR_ENROLLMENT_CODE)
    C->>D: POST /controller/v1/enroll (gdy kod) albo podpisane żądania
    C->>D: lease backendu local
    D->>D: lease widoczny → zapis daemon.json (discoverable)
    C->>R: start runnera dla zadania (jak dziś LocalBackend)
    R->>D: /runner/v1 (raporty, wejście)
    Note over D,C: SIGTERM daemona → SIGTERM dziecka → zwolnienie lease → zamknięcie API
```

Faza B: ten sam proces `narsor-controller local`, uruchomiony przez usera na laptopie z `--control-plane https://…` i kodem z `narsor-cli controller enroll-code workstation --allow-workstation`.

---

## Implementation Units

### Faza A. Wydzielenie

### U1. Podkomendy `narsor-controller`

**Goal:** `narsor-controller kubernetes …` i `narsor-controller kubernetes edit-claude-profile …` zastępują obecną komendę z `--backend` i ręcznym parsowaniem. Wspólne argumenty połączenia są w jednej strukturze.

**Requirements:** R1

**Dependencies:** None

**Files:**
- Modify: `crates/narsor-controller/src/main.rs`
- Modify: `crates/narsor-controller/src/controller.rs` (podział `ControllerArgs` na wspólne `ConnectionArgs` i argumenty Kubernetes; usunięcie `ControllerBackend`)
- Modify: `crates/narsor-controller/Cargo.toml` (`tracing-subscriber`, KTD1a)
- Modify: `deploy/kubernetes/controller.yaml` (komenda z podkomendą `kubernetes`; instrukcja `edit-claude-profile`)
- Modify: `deploy/kind/setup.sh` i pliki w `deploy/kind/`, jeśli zawierają komendę controllera
- Modify: `docs/operations/remote-backends.md` (sekcje 5, 6, 7 i edycja profilu)
- Test: `tests/controller_cli.rs`

**Approach:**
- Wspólne argumenty i ich zmienne `NARSOR_*` bez zmian nazw, żeby manifesty zmieniały tylko komendę.
- Bez podkomendy: błąd użycia (kod 2) z podpowiedzią (KTD2).
- `--help` głównej komendy wymienia oba backendy.

**Patterns to follow:**
- `crates/narsor-controller/src/controller.rs` (`ClusterArgs` jako wspólna struktura dołączana do dwóch komend).
- Struktura podkomend CLI w `src/cli/mod.rs` (`Command`, `ControllerAdminCommand`).

**Test scenarios:**
- Happy path: `narsor-controller kubernetes --help` wypisuje `--kubernetes-config` i argumenty połączenia.
- Happy path: `narsor-controller kubernetes edit-claude-profile --help` wypisuje `--claude-command` i `--yes`.
- Happy path: argumenty połączenia z env (`NARSOR_CONTROL_PLANE`, `NARSOR_CONTROLLER_CREDENTIAL_FILE`) są przyjmowane przez `kubernetes` bez flag.
- Error path: `narsor-controller` bez podkomendy → kod 2, komunikat wskazuje `kubernetes` i `local`.
- Error path: `narsor-controller kubernetes` bez `--kubernetes-config` → kod 2 z nazwą brakującego argumentu.
- Edge case: `narsor-controller kubernetes edit-claude-profile --kubernetes-config f` bez `NARSOR_CONTROL_PLANE` i bez pliku poświadczenia → nie żąda argumentów połączenia.
- Happy path: `RUST_LOG=narsor=info narsor-controller kubernetes …` z błędnym adresem serwera → log `tracing` na stderr (np. o niedostępnym serwerze), nie tylko końcowy `eprintln!`.
- Integration: istniejące testy `tests/kubernetes_adapter.rs`, które uruchamiają komendę controllera, przechodzą z nową składnią.

**Verification:**
- `deploy/kubernetes/controller.yaml` i `deploy/kind/` używają `kubernetes`. Test na kind z `docs/operations/remote-backends.md`, sekcja 12, przechodzi z nowym manifestem.

---

### U2. `narsor-controller local`

**Goal:** Binarka controllera steruje adapterem `LocalBackend` tak samo jak dziś wbudowany controller.

**Requirements:** R2, R5

**Dependencies:** U1

**Files:**
- Create: `crates/narsor-controller/src/local.rs` (argumenty i uruchomienie backendu lokalnego)
- Modify: `crates/narsor-controller/src/main.rs`, `crates/narsor-controller/src/controller.rs`
- Test: `tests/controller_local.rs` (`#[ignore]`: uruchamia procesy)

**Approach:**
- Argumenty `local`: katalog storage adaptera (wymagany), `--home private|host`, `--codex-home`, `--claude-config-dir`, `--claude-profile-source` (te same, co dziś `HarnessHome::runner_args` daje runnerowi), plus wspólne argumenty połączenia.
- Przy starcie: `LocalBackendConfig::new(storage)`, `runner_args` z flag, rejestracja przez `load_or_enroll`, pętla `Controller` i `shutdown_signal`, jak w `run` dla Kubernetes.
- Układ katalogów i plik poświadczenia są te same co dziś (R5), bo daemon przekaże te same ścieżki (U3).
- Kod rejestracji tylko z `NARSOR_ENROLLMENT_CODE` lub `--enrollment-code-file`. Po odczycie controller usuwa zmienną ze swojego środowiska (KTD4).
- `LocalBackend` nie przekazuje runnerom zmiennych `NARSOR_*` controllera (lista do usunięcia w `LocalBackendConfig` albo w `spawn`). Zmienna `WORKLOAD_MARK_ENV` i zmienne ustawiane jawnie przez adapter zostają.

**Patterns to follow:**
- `start_local_controller` w `crates/narsor-server/src/runtime.rs` (kolejność: konfiguracja adaptera, `runner_args`, `ControllerConfig`).
- `run` w `crates/narsor-controller/src/controller.rs`.

**Test scenarios:**
- Happy path: serwer testowy w trybie `trusted` w procesie testu, kod rejestracji, `narsor-controller local` jako proces → controller się rejestruje, trzyma lease backendu `local`, plik poświadczenia ma prawa `0600`.
- Happy path: z tym procesem zadanie na backendzie `local` dochodzi do końca (fake harness z `tests/fixtures/harness/`).
- Edge case: drugi start z istniejącym plikiem poświadczenia i bez kodu → bez ponownej rejestracji, ta sama tożsamość.
- Edge case: `--home host` → runner dostaje `--home host` (sprawdzone w `workload.json` lub argumentach procesu runnera).
- Error path: brak `narsor-runner` obok binarki → zadanie zgłasza problem startu z nazwą brakującego pliku, controller działa dalej.
- Error path: brak poświadczenia i brak kodu → kod wyjścia 2 z komunikatem jak dziś.
- Integration: `SIGTERM` controllera → lease zwolniony (widoczny od razu w `ProtocolService`), runner działa dalej we własnej grupie.
- Error path: środowisko runnera (z `/proc/<pid>/environ` w teście) nie zawiera `NARSOR_ENROLLMENT_CODE` ani innych `NARSOR_*` controllera.
- Error path: `--enrollment-code <wartość>` w argv dla `local` → kod 2 z podpowiedzią zmiennej lub pliku.

**Verification:**
- Scenariusze z `tests/controller_local.rs` przechodzą: rejestracja, lease, zadanie do końca, `--home host`, czyste środowisko runnera. `tests/backend_contract.rs` przechodzi bez zmian jako test regresji adaptera.

---

### U3. Daemon uruchamia lokalny controller jako proces potomny

**Goal:** W trybie `trusted` daemon uruchamia, rejestruje i nadzoruje `narsor-controller local`. Kod wbudowanego controllera znika z serwera.

**Requirements:** R3, R4, R5, R6

**Dependencies:** U2

**Files:**
- Create: `crates/narsor-server/src/local_controller.rs` (uruchomienie, gotowość, nadzór i zatrzymanie procesu potomnego)
- Modify: `crates/narsor-server/src/runtime.rs` (usunięcie `start_local_controller`; użycie nowego modułu w `run_daemon`)
- Modify: `crates/narsor-server/Cargo.toml` (usunięcie zbędnych zależności, jeśli zostaną bez użycia)
- Modify: `crates/narsor-engine/src/controller_protocol/` (zapytanie o ważny lease backendu, jeśli nie ma publicznego)
- Test: `tests/daemon_lifecycle.rs`, `tests/config_startup.rs`

**Approach:**
- Kolejność w `run_daemon`: API → (gdy brak poświadczenia) kod rejestracji → zapamiętanie `epoch` obecnego lease → spawn dziecka → czekanie na lease z większym `epoch` (KTD5) → plik discovery. Zamknięcie: `SIGTERM` dziecka → czekanie z limitem → `stop_accepting` API → drenaż jak dziś.
- Dziecko: nowa grupa procesów, `PR_SET_PDEATHSIG` (KTD6), środowisko bez obcych `NARSOR_*` (KTD3), `RUST_LOG` z poziomu daemona (KTD1a), stderr przez potok czytany cały czas (KTD5).
- Klient HTTP controllera ma krótki limit czasu, żeby po `SIGTERM` przy martwym API (po `kill -9` daemona) próba zwolnienia lease nie trzymała procesu długo.
- Ponowny start po nieoczekiwanym końcu z rosnącym opóźnieniem. Restart nie tworzy nowego kodu, gdy poświadczenie istnieje.
- Ścieżka binarki (KTD7): obok `narsor-server`, z fallbackiem testowym. Brak binarki → błąd startu.
- Tryb `network`: dziecka nie ma, log jak po `83fbb68`.

**Execution note:** Najpierw test charakteryzujący obecne zachowanie: poświadczenie istnieje przed plikiem discovery, lease `local` pojawia się po starcie, zadanie `local` startuje, restart daemona nie rejestruje controllera ponownie. Ten test musi przejść przed i po zmianie. Asercję „lease przed discovery” (R4) dodać dopiero razem ze zmianą, bo dziś nie jest prawdziwa.

**Patterns to follow:**
- `crates/narsor-api-client/src/api/launch.rs` (`resolve_exe`, uruchomienie procesu odłączonego).
- `crates/narsor-engine/src/backends/local.rs` (grupy procesów, sprzątanie).
- `run_daemon` w `crates/narsor-server/src/runtime.rs` (kolejność zamykania).

**Test scenarios:**
- Happy path: start daemona → istnieje proces `narsor-controller local` (dziecko daemona), `daemon.json` pojawia się po lease'ie, zadanie `local` dochodzi do końca. Covers R3, R4.
- Happy path: `narsor-server stop` → dziecko kończy się, lease zwolniony przed zamknięciem API, brak procesów `narsor-controller` z tego katalogu testowego.
- Edge case: drugi start daemona na tym samym stanie → dziecko używa istniejącego poświadczenia, brak nowego kodu rejestracji. Covers R5.
- Edge case: `kill -9` dziecka → daemon uruchamia je ponownie, lease wraca, nowe zadanie startuje.
- Edge case: `kill -9` daemona → dziecko kończy się (PDEATHSIG), następny start daemona działa bez dwóch controllerów.
- Edge case: `kill -9` daemona i natychmiastowy restart (stary lease jeszcze ważny) → `daemon.json` pojawia się dopiero po lease'ie z nowym `epoch`.
- Error path: w powłoce ustawione `NARSOR_RUNNER_CONTROL_PLANE=http://example` → dziecko go nie widzi, runnery dostają adres daemona.
- Error path: brak binarki `narsor-controller` → daemon kończy start z kodem 1 i nazwą brakującego pliku, brak `daemon.json`.
- Error path: dziecko kończy się przed lease'em (np. uszkodzony plik poświadczenia) → daemon kończy start z kodem 1 i końcem stderr dziecka.
- Integration: tryb `network` → brak dziecka, test `a_network_daemon_starts_without_the_built_in_local_controller` przechodzi bez zmian.

**Verification:**
- `crates/narsor-server/src/runtime.rs` nie zawiera pętli controllera. Testy daemona przechodzą, a po testach nie zostają procesy `narsor-controller`.

---

### U4. Dokumentacja i decyzja

**Goal:** Dokumentacja opisuje lokalny controller jako osobny proces. Decyzja jest w rejestrze.

**Requirements:** R1–R6

**Dependencies:** U3

**Files:**
- Modify: `ARCHITECTURE_DECISIONS.md` (nowa decyzja z kolejnym wolnym numerem: lokalny controller jako proces potomny, podkomendy)
- Modify: `docs/operations/remote-backends.md` (sekcja 1: architektura; tryb lokalny)
- Modify: `README.md` (procesy w trybie lokalnym, jeśli są opisane)
- Modify: `run.sh`, `install.sh` (tylko jeśli opis lub lista binarek się zmienia; `run.sh` już zatrzymuje `narsor-controller`)

**Approach:**
- Opis: w trybie `trusted` daemon uruchamia `narsor-controller local`; logi controllera są w logu daemona; ręczne uruchomienie `narsor-controller local` obok daemona `trusted` nie jest potrzebne.

**Test expectation:** none — dokumentacja.

**Verification:**
- Dokumentacja i komendy w niej odpowiadają U1–U3.

---

### Faza B. Laptop jako backend zdalnego serwera

### U5. Backend `kind: local` w trybie `network` i egzekwowanie `trust_scope`

**Goal:** Serwer sieciowy obsługuje backend `kind: local` o własnym id, tylko z `trust_scope: workstation` i z jawną zgodą przy rejestracji.

**Requirements:** R7, R8

**Dependencies:** U3

**Files:**
- Modify: `crates/narsor-engine/src/environment/backend.rs` (`problems()`: `kind: local` o id innym niż `local` wymaga `trust_scope: workstation`)
- Modify: `crates/narsor-engine/src/environment/catalog.rs` (bez `builtin_local` w trybie `network`, KTD10)
- Modify: `crates/narsor-engine/src/controller_protocol/` (`create_enrollment`: zgoda dla backendu `kind: local` z `trust_scope: workstation`, sprawdzana po `kind` i `trust_scope`, nie po id)
- Modify: `crates/narsor-server/src/runtime.rs` (tryb `network`: odrzucenie `backends/local.yaml` przy starcie, tryb przekazany do katalogu)
- Modify: `crates/narsor-protocol/src/controller.rs` (pole zgody w `CreateEnrollmentRequest`, `serde(default)`)
- Modify: `src/cli/remote.rs` (`controller enroll-code … --allow-workstation`, ostrzeżenie)
- Test: `tests/controller_protocol.rs`, `tests/environment.rs`, `tests/auth.rs`

**Approach:**
- Walidacja przy wczytaniu katalogu, z nazwą pola w błędzie.
- Zgoda jest sprawdzana przez serwer, nie tylko przez CLI, żeby inny klient API jej nie ominął. Bez zgody: `400` z kodem błędu i opisem ryzyka.
- Ostrzeżenie w CLI: przejęty serwer może uruchamiać dowolny kod na tym komputerze jako jego użytkownik; brak izolacji kontenera, RBAC i admission.
- Id `local` dalej zarezerwowane dla wbudowanego backendu trybu `trusted` (KTD10). W trybie `network` wpis o id `local` w katalogu jest odrzucany z podpowiedzią innego id.

**Test scenarios:**
- Happy path: katalog z `backends/workstation.yaml` (`kind: local`, `trust_scope: workstation`) → serwer `network` startuje, backend widoczny w `capabilities`.
- Happy path: `enroll-code workstation --allow-workstation` → kod; CLI wypisuje ostrzeżenie.
- Error path: `kind: local` z `trust_scope: dedicated` → błąd wczytania z nazwą pola `trust_scope`.
- Error path: `enroll-code workstation` bez zgody → `400` z kodem błędu, CLI pokazuje, jakiej flagi brakuje.
- Error path: żądanie API bez pola zgody (starszy klient) → `400`, nie kod.
- Edge case: tryb `trusted` z wbudowanym `local` → bez zmian, bez zgody.
- Error path: tryb `network` i wpis `backends/local.yaml` → błąd startu z podpowiedzią innego id.
- Error path: tryb `network`, zadanie z `--backend local` → odrzucone przy tworzeniu z czytelnym błędem (bez `builtin_local`).
- Happy path: backend `kind: local` o id `laptop` z `trust_scope: workstation` → kod wymaga zgody tak samo jak `workstation` (sprawdzenie po `trust_scope`, nie po id).
- Integration: `controller revoke <id>` dla controllera laptopa → kolejne podpisane żądania odrzucone, nowa rejestracja możliwa.

**Verification:**
- Laptopa nie da się zarejestrować jako backendu serwera sieciowego bez jawnej zgody właściciela.

---

### U6. `narsor-controller local` z laptopa do zdalnego serwera

**Goal:** User uruchamia `narsor-controller local` na laptopie i wykonuje zadania serwera w k8s na swoim komputerze.

**Requirements:** R9, R10

**Dependencies:** U2, U5

**Files:**
- Modify: `crates/narsor-controller/src/local.rs` (wymóg `https://` dla adresu spoza loopback; ostrzeżenie przy starcie)
- Modify: `docs/operations/remote-backends.md` (nowa sekcja: komputer jako backend)
- Modify: `deploy/kind/` (opis testu z serwerem w kind, jeśli pliki zostaną w repo)
- Test: `tests/controller_local.rs`

**Approach:**
- Adres serwera spoza loopback musi być `https://` (ta sama reguła co w kliencie API). Certyfikat serwera jest zawsze sprawdzany i nie ma flagi, która to wyłącza. Wyjątek dla `http://` istnieje tylko w buildach debug (do testów), nie w wydaniu.
- Serwer spoza loopback (R10): domyślnie `--home private`; `--home host` wymaga flagi potwierdzenia i wypisuje ostrzeżenie o dostępie zadań do prawdziwego `HOME` (klucze SSH, tokeny, konfiguracja harnessów).
- Kontrole danych z serwera po stronie controllera: ścieżki workloadu i storage tylko wewnątrz katalogu storage adaptera (odrzucenie `..` i ścieżek absolutnych z launch spec), limity rozmiaru launch spec. Adapter lokalny nie wykonuje poleceń z launch spec sam; robi to runner w swoim katalogu.
- Plik poświadczenia z prawami `0600`, domyślnie w `$XDG_STATE_HOME/narsor/workstation-controller/`.
- Runnery dostają `--runner-control-plane` albo adres serwera.
- Przy starcie controller wypisuje adres serwera, id backendu i ostrzeżenie z U5.
- Dokumentacja opisuje utratę laptopa: `narsor-cli controller revoke <id>`.

**Test scenarios:**
- Happy path: serwer testowy w trybie `network` (proxy testowe z `tests/auth.rs`), backend `workstation`, `narsor-controller local` → rejestracja, lease, zadanie dochodzi do końca. Covers R7, R9.
- Error path: `--control-plane http://10.0.0.5:8443` bez flagi testowej → kod 2 z komunikatem o `https://`.
- Edge case: `--control-plane http://127.0.0.1:…` → przyjęte (loopback, jak w kliencie).
- Error path: serwer spoza loopback i `--home host` bez flagi potwierdzenia → kod 2 z opisem ryzyka.
- Error path: launch spec ze ścieżką `../poza` → zadanie odrzucone z problemem, nic nie powstaje poza katalogiem storage.
- Integration (ręczny): serwer w kind (`deploy/kind/server.yaml`) przez `port-forward`, laptop jako backend `workstation`, jedno zadanie z TUI.

**Verification:**
- Z jednego TUI widać zadania na backendach `k8s` i `workstation` tego samego serwera.

---

## System-Wide Impact

- **Interaction graph:** `run_daemon` (start, gotowość, zamknięcie), binarka controllera (CLI), manifest k8s (komenda), katalog (walidacja backendu), API rejestracji (zgoda).
- **Error propagation:** błędy dziecka trafiają do logu daemona i, przed gotowością, do błędu startu daemona. Po starcie daemon uruchamia dziecko ponownie automatycznie (KTD6). Do czasu, gdy nowe dziecko odzyska lease, zadania `local` pokazują `controller_offline`.
- **State lifecycle risks:** dwa controllery naraz (stary po `kill -9` daemona i nowy) → fencing lease'u i PDEATHSIG; przerwana rejestracja → plik poświadczenia zapisywany atomowo przed pierwszym użyciem (bez zmian).
- **API surface parity:** CLI `controller enroll-code` dostaje flagę zgody; TUI nie tworzy kodów rejestracji, więc bez zmian.
- **Integration coverage:** kolejność lease → discovery, zamknięcie bez pozostawionych procesów i ponowne użycie poświadczenia wymagają testów z prawdziwymi procesami (`#[ignore]`).
- **Unchanged invariants:** układ storage zadań lokalnych; tożsamość lokalnego controllera; kontrakt `LocalBackend`; zachowanie trybu `network` z `83fbb68` dla wbudowanego `local`; runnery w osobnych grupach procesów przeżywają controller.

---

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| Wyciek procesów `narsor-controller` w testach i po awarii daemona | PDEATHSIG, nowa grupa procesów, sprzątanie w `Drop` testów, test braku procesów po `stop`. |
| Wolniejszy start daemona (nowy proces, rejestracja przez HTTP) | Gotowość mierzona lease'em; limit czasu z czytelnym błędem; pomiar w teście startu. |
| Zmiana komendy controllera łamie istniejące manifesty | Ta sama zmiana aktualizuje `deploy/kubernetes/` i `deploy/kind/`; błąd użycia podpowiada nową składnię. |
| Faza B otwiera wykonanie kodu na laptopie dla przejętego serwera | Egzekwowany `trust_scope: workstation`, zgoda sprawdzana przez serwer, ostrzeżenie w CLI i przy starcie controllera, `https://` wymagane. Przegląd specjalisty od bezpieczeństwa przed wydaniem fazy B. |
| `PR_SET_PDEATHSIG` zależy od wątku tworzącego dziecko | Odroczone pytanie w Open Questions; alternatywa przez `getppid()`. |

---

## Sources & References

- Rozmowa z Arturem 2026-10-25.
- Commit `83fbb68` (fix(server): tryb network startuje bez wbudowanego controllera local).
- `docs/plans/2026-10-01-2329-feat-zdalne-backendy-wykonawcze-plan.md` (KTD1, KTD3, KTD6).
- `docs/plans/2026-10-25-003-feat-dystrybucja-instalacja-aktualizacja-plan.md`.
- `docs/operations/remote-backends.md`.
