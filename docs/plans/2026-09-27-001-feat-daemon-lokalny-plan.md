---
title: Lokalny daemon jako jedyny właściciel Engine/ExecutionStore
type: feat
status: completed
date: 2026-09-27
origin: docs/brainstorms/2026-09-26-narsor-daemon-lokalny-requirements.md
---

# Lokalny daemon jako jedyny właściciel Engine/ExecutionStore

## Summary

Nowa podkomenda `narsor daemon` staje się jedynym procesem konstruującym `Engine` i otwierającym
`ExecutionStore` do zapisu. Implementacja dzieli się na cztery fazy: (1) plik odkrywania i sam
proces daemona (bind serwera, D23-recovery, detached-spawn, `daemon stop`), (2) rozszerzenie REST
o mutujące endpointy i SSE zastępujące `Engine::subscribe`, (3) migracja `narsor run`/`resume` oraz
TUI na czystych klientów REST(+SSE) z logiką „odkryj-albo-odpal", (4) dostosowanie istniejących
suit testowych i nowy test end-to-end na AE1. Detach terminala realizujemy przez re-exec własnego
binarki z `setsid()` w `pre_exec` (bez podwójnego forka — tokio nie lubi forka po starcie
runtime'u), a SSE budujemy na `axum::response::sse` (natywne w axum 0.7, bez `axum-extra`) owijając
istniejący `Engine::subscribe()` przez `tokio_stream::wrappers::BroadcastStream`.

---

## Problem Frame

Dziś `Engine`/`ExecutionStore` żyją wewnątrz procesu, który je otworzy (`tui::mod::run`,
`cli::commands::run`/`resume`) — zamknięcie tego procesu cicho zabija każdy w tle prowadzony bieg
silnika i jego dziecko-harness. Pełny problem, wymagania i decyzje produktowe są opisane w
dokumencie źródłowym (patrz Sources & References); ten plan skupia się na TYM, JAK to
zaimplementować.

---

## Requirements

- R1. `narsor daemon` prowadzi `Engine` + `ExecutionStore` + API w procesie przeżywającym dowolne
  pojedyncze wywołanie klienta.
- R2. Daemon w pełni odczepiony od terminala wywołującego (nowa sesja, stdio do pliku logu).
- R3. Daemon nigdy nie zatrzymuje się sam — kończy go wyłącznie jawne `narsor daemon stop`.
- R4. `narsor tui` i komendy CLI (włącznie z czytającymi) odkrywają/odpalają daemona i działają
  wyłącznie jako klienci REST(+SSE) — żadna nie konstruuje `Engine` ani nie otwiera
  `ExecutionStore` samodzielnie.
- R5. Plik odkrywania pod katalogiem stanu niesie adres/port i bearer token z restrykcyjnymi
  uprawnieniami; klient sprząta martwe wpisy i odpala świeżego daemona.
- R6. API dostaje mutujące endpointy pokrywające każdą dzisiejszą ścieżkę zapisu (start wykonania,
  run/resume, dostarczenie odpowiedzi człowieka, ręczna/wymuszona tranzycja, lista wykonań).
- R7. API dostaje SSE (per-wykonanie i globalne), zastępujące `Engine::subscribe` jako źródło
  live-update.
- R8. Startowa reconciliacja (`persistence::recovery::recover`, D23) przenosi się do startu
  daemona; `tui::mod::run` przestaje ją wołać.
- R9. Transport to REST po loopback HTTP + SSE, nigdy unix socket.
- R10. `Drop` dla `ApiHandle` przestaje przerywać zadanie serwera, gdy ten serwer jest własnością
  daemona.
- R11. Istniejąca straż `Engine::begin_run`/`active_runs` nadal chroni przed dwoma sterownikami
  wyścigującymi to samo wykonanie — teraz wymuszona przez istnienie jednego procesu.

**Origin acceptance examples:** AE1 (R2, R3), AE2 (R4, R5), AE3 (R5), AE4 (R6, R11), AE5 (R7),
AE6 (R8)

---

## Scope Boundaries

- M1 (checkpoint/przejęcie międzymaszynowe), M3 (dzierżawy/fencing/wywłaszczenie), M4
  (przypisanie/nadzorca), M5 (`Sandbox`/kontenery) — poza zakresem, patrz origin.
- Autoryzacja fazy 2 (poza bearer-tokenem) — bez zmian.
- Frontend w przeglądarce — poza zakresem; ten plan tylko kształtuje transport pod niego.

### Deferred to Follow-Up Work

- Rotacja/odświeżanie bearer tokena (np. przy `daemon stop`+restart w trakcie gdy klienci trzymają
  stary token) — poza zakresem tego planu; token po prostu się zmienia przy każdym nowym
  uruchomieniu daemona i stary klient dostaje `401`, po czym samodzielnie re-discoveruje.
- Konfigurowalny port/bind-adres daemona (dziś zawsze `127.0.0.1:0`, OS-przydzielony) — nie ma dziś
  potrzeby biznesowej, zostawione jako przyszłe rozszerzenie.
- Dokładny wybór biblioteki do parsowania SSE po stronie klienta (ręczny parser na
  `reqwest::Response::bytes_stream()` vs. lekka biblioteka typu `eventsource-stream`) — pytanie
  implementacyjne bez wpływu architektonicznego, patrz Open Questions.

---

## Context & Research

### Relevant Code and Patterns

- `src/api/mod.rs` — dzisiejszy `ApiHandle`: `TcpListener::bind("127.0.0.1:0")`, jednorazowy
  `Uuid::new_v4()` token w pamięci, `Drop` robiący `self.task.abort()`. To dokładnie ten kształt
  (efemeryczny port, token nietrwały, serwer żyjący tylko dla jednego klienta), który ten plan
  zastępuje trwałym, odkrywalnym serwerem daemona.
- `src/api/routes.rs` — 3 istniejące GET-y (`/executions/:id/events`, `/timeline`,
  `/attempts/:id/conversation`), ręcznie pisany `require_token` middleware z porównaniem w czasie
  stałym (`constant_time_eq`) i `is_safe_id` walidacją ID przed złączeniem ze ścieżką pliku — oba
  wzorce trzeba powielić w nowych endpointach.
- `src/api/client.rs` — `ApiClient` z `ClientError::{Unavailable, Unauthorized, NotFound, Server}`
  i `REQUEST_TIMEOUT = 800ms`. Rozróżnienie `Unavailable` (serwer nieobecny) od reszty (serwer
  obecny, ale coś nie tak) to dokładnie kształt potrzebny logice „odkryj-albo-odpal".
- `src/engine/runner.rs:353-707` — `Engine` operuje na `&self` (celowo, żeby dało się dzielić przez
  `Arc` między biegiem a interfejsem). `begin_run`/`active_runs`/`RunGuard` (359, 370-379, 1490) to
  istniejąca straż współbieżności; `run`/`resume`/`deliver_human_input` (524, 615, 660) są nią już
  opakowane. `Engine::subscribe()` (454) ma komentarz wprost nazywający siebie „seam an SSE endpoint
  will take over (D38)". `Engine::pause`/`pause_now` (569, 598) to D34 (pause vs. interrupt).
- `src/tui/app.rs` — `spawn_background` (2648, goły `tokio::spawn`, wynik gubiony poza
  event-streamem), `write_transition`/`manual_transition`/`force_transition` (2796+, bezpośredni
  zapis do `store.event_log`, chroniony wyłącznie klienckim `engine.is_active`), `start_new_task`
  (3232, wywołuje `tasks::start_task(..., &self.engine, ...)` bezpośrednio).
- `src/tui/mod.rs::run` (linie ~158-199) — `recover(&store)` (171), potem `api::start(...)` i
  `Engine::new(store)` w tym samym procesie; `App::new` dostaje jednocześnie `Arc<Engine>` i
  `client` — dualizm, który ten plan zwija do samego klienta.
- `src/cli/commands.rs` — `run` (137-197) i `resume` (~644-673) budują `ExecutionStore`/`Engine`
  wprost, w tym samym procesie.
- `src/persistence/paths.rs` — `StatePaths::log_file()` (129-132) to wzorzec do naśladowania dla
  nowego pliku odkrywania: jedna stała + jedna metoda-akcesor, pod katalogiem stanu.
- `src/persistence/atomic.rs` — istniejący helper atomowego zapisu (używany dla `state.json`),
  do ponownego użycia dla pliku odkrywania, żeby klient nigdy nie zobaczył połowicznego zapisu.
- `src/tasks/mod.rs::start_task`/`start_task_with` (234, 250) — biorą `&Engine`, dobry precedens
  dla endpointu `POST /tasks`.
- `ARCHITECTURE_DECISIONS.md` — D23 (recovery), D33 (best-effort cancellation), D34 (pause vs.
  interrupt), D36-D38 (TUI+API w MVP, REST+SSE, SSE dla live update), D52 (dzisiejsze osadzone,
  tylko-do-odczytu API — jego własny tekst już zapowiada i celowo odkłada dokładnie ten oddzielny
  proces daemona, który ten plan teraz buduje).

### Institutional Learnings

Brak katalogu `docs/solutions/` w repo — nie ma udokumentowanych wcześniejszych lekcji do
uzgodnienia; plan opiera się na dokumentach `docs/brainstorms/`/`docs/plans/` i bezpośrednim
czytaniu kodu wymienionym wyżej.

### External References

- Detach terminala: re-exec własnego binarki (`std::process::Command::new(current_exe)`) z
  `unsafe { cmd.pre_exec(|| { libc::setsid(); Ok(()) }) }`, stdio przekierowane do pliku logu,
  `drop(child)` bez `wait()`. Preferowane nad `Command::process_group(0)` (który NIE robi
  `setsid`, więc `SIGHUP` z zamykanego terminala nadal mógłby dotrzeć) i nad podwójnym forkiem
  (niepotrzebny, bo nie forkujemy w trakcie działania runtime'u tokio — tylko re-exec). Crate
  `daemonize` odradzany (robi surowy `fork()` w żywym procesie, niebezpieczne z tokio); dodanie
  samego `libc` (bez `nix`) wystarcza na jedno wywołanie `setsid()`.
- SSE: `axum::response::sse::{Sse, Event, KeepAlive}` — natywne w axum 0.7, **nie** w
  `axum-extra` (stare re-eksporty tam są nieaktualne/nieistniejące w 0.9). Potrzebna nowa zależność
  `tokio-stream` (feature `sync`) dla `BroadcastStream::new(engine.subscribe())`. Błąd `Lagged(n)`
  z `BroadcastStreamRecvError` trzeba zamienić w `filter_map` na `None` (nie propagować jako `Err`,
  bo `Sse`/`Infallible` zamknęłoby cały strumień) — opcjonalnie wyemitować syntetyczne zdarzenie
  `resync`, żeby klient wiedział, że coś pominął. `KeepAlive` ~15s wystarcza dla loopbacku.

---

## Key Technical Decisions

- **Model dyspozycji zamiast trzymania połączenia.** Mutujące endpointy dla `run`/`resume`/
  `deliver_human_input` (długo trwające przejścia silnika) odpowiadają natychmiast (`202
  Accepted` + identyfikator wykonania) po zaplanowaniu pracy wewnątrz procesu daemona (odpowiednik
  dzisiejszego `spawn_background`, ale żyjący w daemonie, nie w kliencie); klient śledzi postęp
  przez SSE. Dzięki temu istniejący `REQUEST_TIMEOUT = 800ms` w `ApiClient` zostaje bez zmian dla
  wywołań kontrolnych — tylko połączenie SSE jest z niego jawnie wyłączone (strumieniowe, bez
  limitu). Rozstrzyga to odłożone pytanie z origin o rewizję timeoutu.
- **`narsor run`/`resume` zachowują dzisiejsze blokujące zachowanie CLI**, ale blokowanie
  przenosi się na stronę klienta: po dyspozycji przez REST klient subskrybuje SSE danego wykonania
  i czeka na zdarzenie terminalne, zamiast (jak dziś) `.await`-ować bezpośrednie wywołanie
  `Engine::run`/`resume`. Zachowuje „unchanged invariant" z origin bez trzymania żądania HTTP
  otwartego przez cały czas trwania biegu.
- **Jeden, współdzielony bearer token na cały cykl życia daemona**, trzymany w pliku odkrywania z
  restrykcyjnymi uprawnieniami (`0600`). Wszyscy lokalni klienci na tej samej maszynie/koncie
  współdzielą ten sam token — ten sam model zaufania co dzisiejszy per-proces token, tylko trwały i
  odkrywalny zamiast efemerycznego. Rozstrzyga odłożone pytanie origin o pojedynczy vs. osobne
  tokeny.
- **Zatrzymanie daemona jest łagodne (decyzja użytkownika).** `narsor daemon stop` wysyła `SIGTERM`
  do procesu daemona (PID z pliku odkrywania); handler daemona woła `Engine::pause` (D34: nie
  zaczynaj kolejnej akcji) dla każdego wpisu w `active_runs`, czeka ograniczony czas (np. kilka
  sekund) aż `active_runs` opróżni się, potem robi `axum::serve(...).with_graceful_shutdown(...)` i
  kończy proces. Jeśli limit czasu minie, daemon i tak kończy działanie — przerwane wykonania
  dostają status `Unknown` przy następnym starcie przez D23-recovery, dokładnie jak przy twardym
  zabiciu (AE6). Nie potrzeba osobnego mutującego endpointu REST do zatrzymania — sygnał
  systemowy do procesu na tej samej maszynie jest prostszy i nie poszerza powierzchni
  autoryzacji.
- **Embedded, efemeryczny model `ApiHandle` (bind-per-proces, token-w-pamięci,
  abort-na-drop) zostaje w całości wycofany, nie warunkowo zachowany.** Żaden klient nie
  konstruuje już `Engine` ani nie potrzebuje własnego serwera — jedyny serwer, jaki kiedykolwiek
  istnieje, należy do daemona. `src/api/mod.rs::start` zostaje przepisany na bootstrap serwera
  daemona (bez `Drop::abort`, z jawnym, łagodnym shutdownem opisanym wyżej).
- **Odczepienie terminala przez re-exec + `setsid()` w `pre_exec`, nie podwójny fork.** Patrz
  External References — uzasadnienie i szkic kodu.
- **SSE na natywnym `axum::response::sse`, opakowującym istniejący `Engine::subscribe()`.** Patrz
  External References.
- **Wszystkie komendy CLI, także czytające (`events`, `steps`, `conversation`), przechodzą na
  `ApiClient` przez odkryj-albo-odpal**, zamiast dzisiejszego bezpośredniego czytania store'u
  (uzasadnienie D52 — „jedynym konsumentem API jest TUI" — przestaje być prawdziwe, gdy istnieje
  daemon z wieloma klientami). To szersze niż samo Success Criterion 2 (które zabrania tylko
  zapisu spoza `src/api`), ale wprost wymagane przez R4 origin.

---

## Open Questions

### Resolved During Planning

- Semantyka `narsor daemon stop`: łagodna, patrz Key Technical Decisions.
- Kształt pliku odkrywania: JSON (`pid`, `addr`, `token`, `started_at`) pod
  `{state_root}/daemon.json`, zapisywany atomowo, uprawnienia `0600`.
- Blokujące vs. nieblokujące `narsor run`/`resume`: pozostają blokujące dla operatora, ale przez
  klienckie oczekiwanie na SSE, nie przez trzymanie żądania HTTP.
- Rewizja `REQUEST_TIMEOUT`: bez zmian dla wywołań kontrolnych (model dyspozycji), SSE jawnie
  wyłączone z tego limitu.
- Współdzielenie tokena: jeden token na daemona, współdzielony przez wszystkich klientów.
- Autoryzacja health-checku użytego w odkrywaniu: brak osobnego nieautoryzowanego endpointu —
  liveness potwierdzany zwykłym uwierzytelnionym żądaniem (`require_token` obowiązuje wszędzie,
  bez wyjątków).
- Wyścig dwóch klientów odpalających daemona równocześnie: rozwiązany przez `daemon.lock`
  (`write_atomic_exclusive`) jako bramkę spawnu, patrz U1/U5.

### Deferred to Implementation

- Dokładny wybór crate'a/parsera SSE po stronie klienta (ręczny parser na `bytes_stream()` vs.
  `eventsource-stream`) — niskie ryzyko architektoniczne, decyzja na etapie implementacji U6.
- Dokładny limit czasu oczekiwania daemona na opróżnienie `active_runs` przy łagodnym stopie
  (sekundy vs. dziesiątki sekund) — do ustalenia empirycznie przy implementacji U2, nie zmienia
  kształtu architektury.
- Czy `GET /executions` (nowa lista) potrzebuje paginacji — dzisiejsza skala (jeden lokalny
  operator) raczej nie, ale implementator może to zweryfikować przy U4.
- Dokładna nazwa/kształt dwufazowego API silnika (`try_begin_resume`/`ResumeHandle` czy
  równoważne) wprowadzanego w U3 dla `resume`/`deliver_human_input` — architektoniczny wymóg
  dwufazowości jest rozstrzygnięty w U3 Approach, dokładne nazewnictwo i to, czy `RunGuard`
  wymaga zmiany kształtu życiowego (`&'a Engine` vs. życie wewnątrz `tokio::spawn`), zostają
  implementatorowi.
- Czy pojemność kanału `broadcast` za `Engine::subscribe()` wymaga zwiększenia pod kątem kilku
  równoczesnych konsumentów SSE (U4) — zmierzyć/zweryfikować przy implementacji, nie zmienia
  kształtu architektury (transport i tak jest tym samym broadcastem, tylko z większą liczbą
  odbiorców).

---

## High-Level Technical Design

> To ilustruje zamierzone podejście i jest wskazówką kierunkową do przeglądu, nie specyfikacją
> implementacyjną. Agent wdrażający powinien traktować to jako kontekst, nie kod do odtworzenia.

Sekwencja „odkryj-albo-odpal" (AE2/AE3), wspólna dla `narsor tui`/`run`/`resume`:

```
Client                    daemon.lock / daemon.json               Daemon process
  |-- read discovery file ---------->|
  |<-- (missing / stale PID) --------|
  |-- write_atomic_exclusive(daemon.lock) -->|
  |<-- won the lock (loser would poll daemon.json instead, below) --|
  |-- spawn detached child (setsid, stdio->log) ------------------->|
  |                                                                  |-- recover() (D23)
  |                                                                  |-- bind 127.0.0.1:0
  |                                                                  |-- Engine::new(store)
  |                                                                  |-- write daemon.json (atomic)
  |-- poll daemon.json until present & authenticated health GET ok ->|
  |<-- ok, addr+token ---------------|
  |-- remove daemon.lock ------------|
  |-- build ApiClient(addr, token) ----------------------------------------------------------->|
```

Model dyspozycji dla `run`/`resume`/`deliver_human_input` (odpowiada AE1, AE4):

```
Client                                  Daemon (Arc<Engine>)
  |-- POST /executions/:id/resume ------->|
  |                                       |-- begin_run guard (409 if AlreadyRunning, AE4)
  |                                       |-- tokio::spawn(engine.resume(...))  // lives in daemon
  |<-- 202 Accepted {execution_id} -------|
  |-- GET /executions/:id/stream (SSE) -->|
  |<== event, event, event, ... Terminal =|   // client blocks here until a terminal Event
```

Zatrzymanie procesu klienta (`q`, zamknięcie terminala, zabicie klienta) nie ma już żadnego wpływu
na drugą strzałkę — background task żyje w procesie daemona, nie klienta.

---

## Implementation Units

### U1. Plik odkrywania: format i pomocnicze funkcje

**Goal:** Wprowadzić trwały, atomowo zapisywany plik odkrywania daemona i funkcje do jego
odczytu/weryfikacji żywotności.

**Requirements:** R5

**Dependencies:** None

**Files:**
- Modify: `src/persistence/paths.rs` (dodać `DAEMON_FILE`/`DAEMON_LOCK_FILE` consts i
  `StatePaths::daemon_file()`/`daemon_lock_file()`, analogicznie do `LOG_FILE`/`log_file()`)
- Create: `src/persistence/daemon_info.rs` (struct `DaemonInfo { pid: u32, addr: SocketAddr,
  token: String, started_at: ... }`, `serde` (de)serializacja, `DaemonInfo::write(path)` przez
  istniejący atomowy helper z `src/persistence/atomic.rs`, `DaemonInfo::read(path)`,
  `DaemonInfo::is_alive(&self) -> bool` sprawdzające PID (np. `kill(pid, 0)` na Unixie) — bez
  próby połączenia sieciowego, to zostaje po stronie klienta w U6)
- Modify: `src/persistence/mod.rs` (wypięcie nowego modułu)
- Test: `src/persistence/daemon_info.rs` (moduł testowy inline lub `tests/daemon_info.rs`)

**Approach:**
- **Blokada wyścigu spawnu**: samo „przeczytaj `daemon.json`, brak/martwy → odpal" jest klasycznym
  check-then-act i nie wystarcza — dwaj klienci startujący niemal równocześnie bez działającego
  daemona mogliby oba odpalić własny proces, każdy z osobnym `Engine`/`ExecutionStore` na tym
  samym state roocie (dokładnie problem, który cały ten plan ma zamknąć). Dlatego przed spawnem
  klient musi zdobyć wyłączną blokadę: `{state_root}/daemon.lock`, tworzony przez istniejący
  `write_atomic_exclusive` z `src/persistence/atomic.rs` (semantyka `O_EXCL`/hard-link, już w
  kodzie). Klient, któremu `write_atomic_exclusive` się uda, jest tym, który odpala daemona i
  usuwa `daemon.lock` dopiero gdy `daemon.json` faktycznie się pojawi (albo po niepowodzeniu
  startu — patrz U5). Klient, któremu się nie uda (plik już istnieje), **nie odpala** własnego
  daemona — zamiast tego czeka/pollinguje aż `daemon.json` się pojawi (to samo pollingowanie, co
  przy zwykłym odkrywaniu w U5), tak jakby zastał już żywy proces. Blokada jest sprzątana także
  jeśli spawnujący klient wykryje własny timeout (żeby nie zostawić trwałego zakleszczenia po
  nieudanym starcie).
- Plik pod `{state_root}/daemon.json`, uprawnienia `0600` przy tworzeniu. Dzisiejszy
  `write_atomic`/`write_json_atomic` w `src/persistence/atomic.rs` **nie** ustawia uprawnień
  pliku tymczasowego (idzie za domyślnym umask) — potrzebna albo nowa wersja tego helpera
  przyjmująca tryb (`OpenOptionsExt::mode(0o600)` na pliku tymczasowym przed zapisem/rename), albo
  jawny `set_permissions` zaraz po `File::create`, zanim jakikolwiek bajt (w tym token) zostanie
  zapisany — nie po samym rename, żeby nie zostawić choćby krótkiego okna, w którym plik
  tymczasowy niosący token istnieje z uprawnieniami domyślnymi.
- Format JSON: `{ "pid": u32, "addr": "127.0.0.1:PORT", "token": "uuid-v4-string",
  "started_at": "RFC3339" }`.
- `is_alive` sprawdza wyłącznie istnienie procesu (PID) — nie odpowiada za sprawdzenie, czy
  serwer HTTP odpowiada (to robi klient w U6, bo wymaga async/sieci).

**Patterns to follow:**
- `src/persistence/paths.rs::log_file()` (stała + akcesor)
- `src/persistence/atomic.rs` (atomowy zapis)

**Test scenarios:**
- Happy path: zapisz `DaemonInfo`, odczytaj z powrotem, pola identyczne.
- Happy path: `is_alive()` zwraca `true` dla PID bieżącego procesu testowego.
- Edge case: `is_alive()` zwraca `false` dla PID, który na pewno nie istnieje (np. bardzo duża
  wartość albo PID zebranego i zakończonego procesu potomnego).
- Error path: `DaemonInfo::read` na nieistniejącym pliku zwraca błąd, nie panikuje.
- Error path: `DaemonInfo::read` na uszkodzonym/pustym JSON zwraca błąd (odróżnialny od
  "plik nie istnieje", żeby wywołujący mógł go posprzątać zamiast zawieść cicho).
- Happy path: `write_atomic_exclusive` na `daemon.lock` udaje się dokładnie raz, gdy plik nie
  istnieje; drugie wywołanie na ten sam ścieżce (symulujące drugiego klienta) zawodzi.

**Verification:**
- `DaemonInfo` round-trip i liveness-check pokryte testami jednostkowymi, zero zależności od
  sieci/axum w tym module.

---

### U2. Podkomenda `narsor daemon` i `narsor daemon stop`

**Goal:** Proces daemona: recovery, bind serwera, zapis pliku odkrywania, łagodny shutdown na
sygnał; osobna komenda do wysłania tego sygnału.

**Requirements:** R1, R2, R3, R8, R10

**Dependencies:** U1

**Files:**
- Modify: `src/cli/mod.rs` (nowy wariant `Command::Daemon { ... }` z subkomendą `stop`, patrz
  istniejący wzorzec `Command::Tui`)
- Create: `src/cli/daemon.rs` (logika komendy: `run_daemon(config)` — foreground entry point, i
  `stop_daemon(config)` — odczyt PID z `DaemonInfo`, wysłanie `SIGTERM`)
- Modify: `src/main.rs` (dispatch nowej komendy, analogicznie do istniejących)
- Modify: `src/api/mod.rs` (przepisanie `start`/`ApiHandle`: usunięcie `Drop::abort`, dodanie
  jawnego, async `shutdown()` używającego `axum::serve(...).with_graceful_shutdown(...)`)
- Create: `src/daemon/launch.rs` lub podobnie (detach-spawn: `Command::new(current_exe)` +
  `pre_exec` z `libc::setsid()`, stdio do pliku logu — używane przez U5 po stronie klienta, ale
  sam mechanizm detach/re-exec logicznie należy do tego unitu, bo to on definiuje jak wygląda
  odpalony daemon)
- Modify: `Cargo.toml` (dodać `libc` jako zależność)
- Test: `tests/daemon_lifecycle.rs` (nowy plik)

**Approach:**
- `run_daemon`: `recover(&store)` (przeniesione z `tui::mod::run`) → bind `TcpListener` →
  `Engine::new(store).with_agent_defaults(...)` → router z pełnym zestawem endpointów (U4, U5) →
  zapisz `DaemonInfo` (adres z bind, świeży `Uuid::new_v4()` token, własny PID) → zainstaluj
  handler `tokio::signal::unix::signal(SignalKind::terminate())` → `axum::serve(...)` w pętli aż
  do sygnału.
- Na `SIGTERM`: dla każdego ID w `engine.active_runs()` (dziś `active_runs` jest prywatnym polem
  `Mutex<HashSet<ExecutionId>>` w `runner.rs` — potrzebny nowy publiczny getter, np.
  `Engine::active_run_ids(&self) -> Vec<ExecutionId>`, zwracający migawkę pod blokadą) wywołaj `engine.pause(id)`
  (D34), czekaj z ograniczonym timeoutem aż zbiór aktywnych biegów opróżni się (polling z krótkim
  interwałem), potem zainicjuj `with_graceful_shutdown`, usuń plik `daemon.json`, zakończ proces.
  Timeout przekroczony → loguj ostrzeżenie i kończ i tak (D23-recovery posprząta przy następnym
  starcie, AE6).
- `stop_daemon`: odczytaj `DaemonInfo`, jeśli PID żyje wyślij `SIGTERM` (np. przez `libc::kill`
  albo crate `nix`/ręczne FFI — bez dodawania `nix`, wystarczy `libc::kill(pid, SIGTERM)`), jeśli
  PID nie żyje wypisz komunikat i posprzątaj plik.
- Detach-spawn w `src/daemon/launch.rs`: `unsafe { cmd.pre_exec(|| { libc::setsid();
  Ok(()) }) }`, `stdin(Stdio::null())`, `stdout`/`stderr` do pliku logu (nazwa analogiczna do
  istniejącej konwencji logowania), `drop(child)` bez `wait()`. Plik logu otwierany z
  uprawnieniami `0600` (tak samo jak `daemon.json` w U1) — bez tego ryzyko, że nagłówek
  `Authorization` albo treść mutującego żądania trafiające do logu przez tracing/panic-output
  wyciekną przez plik o domyślnych uprawnieniach umask; logowanie żądań musi jawnie pomijać
  nagłówek `Authorization` i surowe ciała mutujących endpointów. Klient re-execuje binarkę z
  jawnie doklejonym `--root`/`--config`, jeśli klient sam był wywołany z taką flagą (nie polegać
  wyłącznie na dziedziczeniu zmiennych środowiskowych przy `exec`) — inaczej daemon mógłby
  rozwiązać inny, domyślny katalog stanu niż ten, którego oczekuje klient.

**Technical design:** Patrz sekwencja „odkryj-albo-odpal" i model dyspozycji w High-Level
Technical Design.

**Patterns to follow:**
- `src/tui/mod.rs::run` (obecne miejsce wywołania `recover`, do przeniesienia, nie skopiowania)
- `Engine::pause`/`pause_now` (D34)
- Builder-style `Engine::new(...).with_*` już istniejący w kodzie

**Test scenarios:**
- Happy path: `narsor daemon` startuje, plik `daemon.json` pojawia się z poprawnym PID/adresem,
  serwer odpowiada na prosty GET.
- Happy path (AE1): uruchom akcję przez daemona, zabij proces, który go odpalił (symulacja
  klienta) — proces daemona i jego dziecko-harness nadal żyją (`ps`/sprawdzenie PID), plik
  rozmowy dalej rośnie.
- Happy path (AE6): zabij proces daemona w trakcie akcji (`SIGKILL`, nie `stop`), wystartuj
  ponownie — przerwane wykonanie ma status `Unknown` i zdarzenie `Error` po starcie (D23).
- Integration (łagodny stop): uruchom długą akcję, wywołaj `narsor daemon stop`, potwierdź że
  `Engine::pause` zostało wywołane dla aktywnego biegu i proces kończy się po opróżnieniu
  `active_runs` (albo po timeout, z odpowiednim logiem).
- Edge case: `narsor daemon stop` gdy żaden daemon nie działa (brak pliku albo martwy PID) —
  komunikat, zero paniki, kod wyjścia niezerowy.
- Error path: bind portu się nie udaje (rzadkie, ale test na symulowanym konflikcie) — czytelny
  błąd startu, plik odkrywania nie zostaje zapisany w stanie połowicznym.

**Verification:**
- `ps`/`pstree` po scenariuszu AE1 pokazuje daemona i harness (Success Criterion 1).
- Grep-owalna weryfikacja: `src/api` i moduł startu daemona to jedyne miejsca konstruujące
  `Engine`/otwierające `ExecutionStore` do zapisu (Success Criterion 2) — sprawdzana ponownie po
  U7/U8, ale bind/recovery/Engine-construction powinny już wtedy istnieć wyłącznie tutaj.

---

### U3. Mutujące endpointy REST

**Goal:** Pokryć każdą dzisiejszą ścieżkę zapisu z TUI/CLI mutującym endpointem na `Arc<Engine>`
daemona.

**Requirements:** R6, R11

**Dependencies:** U2

**Files:**
- Modify: `src/engine/runner.rs` (nowa dwufazowa metoda publiczna, patrz Approach — rozbicie
  `resume`/`deliver_human_input` na "spróbuj zdobyć guard" + "biegnij do końca"; dzisiejsze
  jednowywołaniowe metody zostają jako cienkie wrappery dla `cli::commands`/testów silnika)
  oraz nowy publiczny getter `active_run_ids()` (potrzebny też w U2)
- Modify: `src/api/routes.rs` (nowe handlery i wpisy w routerze)
- Create/Modify: moduł z payloadami żądań/odpowiedzi (np. `src/api/dto.rs`) — struktury `serde`
  dla request/response bodies
- Test: `tests/api.rs` (rozszerzenie istniejącej suity)

**Approach:**
- `POST /tasks` — body: definicja zadania + inputy (kształt analogiczny do parametrów
  `tasks::start_task_with`); wywołuje `tasks::start_task_with(...)` przeciwko `Arc<Engine>`
  daemona, potem `tokio::spawn(engine.run(...))` (odpowiednik dzisiejszego `spawn_background`,
  teraz żyjący w daemonie); odpowiada `202 Accepted` z `execution_id`.
- `POST /executions/:id/resume` i `POST /executions/:id/input` wymagają rozbicia dzisiejszego,
  monolitycznego `async fn resume`/`deliver_human_input` na dwie fazy, bo dziś zdobycie
  `RunGuard` i przebiegnięcie do końca są zespolone w jednym `self.observed(id, async { let _guard
  = self.begin_run(id)?; ... }).await` (`runner.rs`) — to realna zmiana publicznego API `Engine`,
  nie kosmetyczne przełożenie kolejności. Podejście: dodać na `Engine` nową, publiczną metodę
  dwufazową, np. `Engine::try_begin_resume(&self, id) -> Result<ResumeHandle<'_>, EngineError>`
  (zwraca błąd `AlreadyRunning` synchronicznie, natychmiast, bez `.await`, zdobywając
  `RunGuard` na miejscu) plus `ResumeHandle::run_to_completion(self, definition) -> impl Future`
  (albo równoważny kształt, np. `Engine` opakowane w `Arc` tak, żeby `RunGuard` mogło żyć
  wewnątrz `tokio::spawn`-owanego zadania zamiast pożyczać `&'a Engine` z lokalnego stosu).
  Endpoint handler woła fazę 1 synchronicznie (żeby zwrócić `409` od razu bez spawnowania), a przy
  sukcesie odpala fazę 2 przez `tokio::spawn`. Istniejące, jednowywołaniowe `Engine::resume`/
  `deliver_human_input` (używane dziś przez `cli::commands` i testy w `tests/tasks.rs`) pozostają
  bez zmian kontraktu — najprościej jako cienkie wrappery wywołujące obie fazy sekwencyjnie, żeby
  U8 nie musiało przepisywać testów silnika. Dokładny kształt `ResumeHandle`/nazw jest decyzją
  implementacyjną (patrz Open Questions), ale dwufazowość — synchroniczne "spróbuj zdobyć guard",
  potem asynchroniczne "biegnij do końca" — jest architektonicznym wymogiem tego endpointu, nie
  szczegółem do wynalezienia przez implementatora bez wskazówki.
- `POST /executions/:id/transition` — synchroniczny handler odtwarzający dzisiejszą logikę
  `write_transition` (app.rs:2796+): sprawdź `engine.is_active(id)` (409 jeśli aktywne), zbuduj i
  dopisz sekwencję zdarzeń przez `store.event_log(id).append_next(...)`, zaktualizuj
  `execution.current_state`/`status`; odpowiedz zaktualizowanym stanem albo błędem. **Ważne:**
  dziś ta ścieżka jest osiągalna tylko z menu TUI, które samo ogranicza możliwe stany docelowe;
  jako endpoint REST staje się osiągalna przez dowolnego posiadacza bearer tokena (dowolny lokalny
  proces/skrypt, nie tylko TUI), więc serwer musi sam autorytatywnie sprawdzić **legalność**
  żądanej tranzycji względem definicji workflow (dozwolone przejścia z bieżącego stanu), nie tylko
  `is_active` — inaczej uwierzytelniony, ale błędny/złośliwy klient mógłby zapisać nielegalny skok
  stanu do append-only logu zdarzeń. Nielegalna tranzycja → `400`/`409`, zero zapisu.
- `GET /executions` — lista podsumowań wykonań (id, status, workflow, timestamps) czytana ze
  store'u.
- Wszystkie nowe endpointy przechodzą przez istniejący `require_token` middleware; walidacja ID
  przez istniejący wzorzec `is_safe_id`.
- Mapowanie błędów: `EngineError::AlreadyRunning` → `409`, `ExecutionNotFound` → `404`,
  `Storage`/inne → `500`, zgodnie z istniejącym `ApiError`/`IntoResponse`.

**Patterns to follow:**
- `src/api/routes.rs` (istniejące 3 GET-y, `require_token`, `is_safe_id`, `ApiError`)
- `RunGuard`/`begin_run` (`runner.rs:370-379,1490`)
- `write_transition` w `app.rs` (logika do przeniesienia server-side, nie skopiowania jeden-do-
  jednego — klient przestaje mieć bezpośredni dostęp do store'u)

**Test scenarios:**
- Happy path: `POST /tasks` tworzy wykonanie, zwraca `202` + poprawny `execution_id`, wykonanie
  pojawia się w `GET /executions`.
- Happy path: `POST /executions/:id/resume` na wstrzymanym wykonaniu zwraca `202`, wykonanie
  ostatecznie osiąga stan terminalny (weryfikowalne przez odpytanie store'u w teście).
- Happy path: `POST /executions/:id/transition` na nieaktywnym wykonaniu zapisuje oczekiwaną
  sekwencję zdarzeń i aktualizuje stan/status.
- Happy path: `POST /executions/:id/input` dostarcza odpowiedź człowieka do oczekującego
  wykonania.
- Edge case (AE4): dwa równoczesne `POST /executions/:id/resume` dla tego samego ID — dokładnie
  jeden `202`, drugi `409 Conflict`.
- Error path: `POST /executions/:id/transition` na aktywnym wykonaniu (silnik aktualnie je
  prowadzi) → `409`, żaden zapis nie następuje.
- Error path: `POST /executions/:id/transition` z docelowym stanem, który nie jest legalnym
  przejściem z bieżącego stanu wg definicji workflow → `400`/`409`, żaden zapis nie następuje.
- Error path: dowolny endpoint z nieistniejącym `execution_id` → `404`.
- Error path: dowolny endpoint bez/z błędnym bearer tokenem → `401`, bez ciała odpowiedzi
  (zgodnie z istniejącą konwencją `require_token`).
- Error path: `POST /executions/:id/transition` z niebezpiecznym ID (path traversal) →
  odrzucone przez `is_safe_id`, `400`/`404`, nie dotyka systemu plików poza store'em.

**Verification:**
- `tests/api.rs` pokrywa każdy nowy endpoint w ścieżce sukcesu i co najmniej jednej ścieżce
  błędu; żaden test nie omija `require_token`.

---

### U4. SSE: strumień zdarzeń per-wykonanie i globalny

**Goal:** Zastąpić `Engine::subscribe()` jako źródło live-update transportem HTTP (SSE) dostępnym
dla wielu klientów jednocześnie.

**Requirements:** R7

**Dependencies:** U2, U3

**Files:**
- Modify: `src/api/routes.rs` (dodać `GET /executions/:id/stream`, `GET /events/stream`)
- Modify: `Cargo.toml` (dodać `tokio-stream` z feature `sync`)
- Test: `tests/api.rs` lub nowy `tests/sse.rs`

**Approach:**
- `engine.subscribe()` → `tokio_stream::wrappers::BroadcastStream::new(rx)` → `.filter_map`
  mapujące `Ok(event)` na `Some(Ok(sse::Event::default().event("engine").json_data(&event)))` i
  `Err(BroadcastStreamRecvError::Lagged(n))` na `None` (z logiem ostrzeżenia; opcjonalnie
  syntetyczne zdarzenie `resync`).
- `/executions/:id/stream` dodatkowo filtruje po `execution_id` przed mapowaniem na
  `sse::Event`.
- `KeepAlive::new().interval(Duration::from_secs(15)).text("keep-alive")`.
- Endpoint chroniony tym samym `require_token`.
- `Engine::subscribe()`'s broadcast channel była dotąd wymiarowana pod dokładnie jednego
  wewnątrzprocesowego czytelnika (TUI). Każde nowe połączenie SSE (per-wykonanie albo globalne)
  to kolejny niezależny `broadcast::Receiver` — sprawdź przy implementacji, czy dzisiejsza
  pojemność kanału jest wystarczająca dla kilku równoczesnych konsumentów HTTP, zanim `Lagged`
  stanie się częstsze niż dziś (patrz Open Questions).

**Technical design:** Patrz szkic kodu w Context & Research / External References (adaptacja
`BroadcastStream`).

**Patterns to follow:**
- `Engine::subscribe()` (`runner.rs:454`, komentarz "seam an SSE endpoint will take over")
- `require_token` middleware (`routes.rs`)

**Test scenarios:**
- Happy path (AE5): dwóch klientów SSE podłączonych do tego samego wykonania widzą identyczną
  sekwencję zdarzeń w czasie rzeczywistym.
- Happy path: klient podłączony do `/events/stream` (globalny) widzi zdarzenia z wielu różnych
  wykonań.
- Edge case: klient odłącza się w trakcie strumienia — serwer nie panikuje, `broadcast::Sender`
  ignoruje nieudany `send` zgodnie z istniejącym komentarzem w `runner.rs`.
- Edge case: wolny klient powodujący `Lagged` — strumień kontynuuje (nie zamyka się z błędem),
  klient dostaje kolejne zdarzenia po pominiętych.
- Error path: żądanie SSE bez/z błędnym tokenem → `401` przed otwarciem strumienia.

**Verification:**
- Test integracyjny z dwoma równoczesnymi klientami SSE potwierdza identyczną kolejność zdarzeń
  (AE5).

---

### U5. `ApiClient`: rozszerzenie o mutacje, SSE i logikę odkryj-albo-odpal

**Goal:** Dać klientom (CLI, TUI) jeden spójny sposób mówienia z daemonem: wywołania mutujące,
konsumpcja SSE, i automatyczne odkrycie/odpalenie daemona gdy nieobecny.

**Requirements:** R4, R5, R9

**Dependencies:** U1, U2, U3, U4

**Files:**
- Modify: `Cargo.toml` (dodać feature `stream` do zależności `reqwest` — dziś
  `default-features = false, features = ["json"]`, bez `stream`; `Response::bytes_stream()`
  wymagany do konsumpcji SSE po stronie klienta nie skompiluje się bez tej flagi)
- Modify: `src/api/client.rs` (nowe metody: `start_task`, `resume`, `deliver_input`,
  `transition`, `list_executions`, `stream_execution`, `stream_all`)
- Create: `src/api/discovery.rs` (funkcja `ensure_daemon_running(paths: &StatePaths) ->
  Result<ApiClient, DiscoveryError>`: czytaj `DaemonInfo`, sprawdź `is_alive()` + health-check
  HTTP, jeśli brak/martwy — posprzątaj plik, odpal detach-spawn z U2, czekaj (z limitem czasu) aż
  plik się pojawi i serwer odpowie, zbuduj `ApiClient`)
- Test: `tests/api_discovery.rs` (nowy) + rozszerzenie `tests/api.rs` o metody klienta

**Approach:**
- SSE po stronie klienta: strumieniowe czytanie `reqwest::Response` (`bytes_stream()`) z ręcznym
  parserem linii `data:`/`event:` rozdzielonych `\n\n`, albo lekka biblioteka
  jednoprzeznaczeniowa — dokładny wybór odłożony do implementacji (patrz Open Questions).
  Interfejs klienta zwraca `impl Stream<Item = Result<Event, ClientError>>` niezależnie od wyboru.
  Ten strumień jawnie NIE używa `REQUEST_TIMEOUT` (połączenie długotrwałe z założenia).
- `ensure_daemon_running`: `DaemonInfo::read` → jeśli błąd/`is_alive()==false` → usuń plik →
  spróbuj `write_atomic_exclusive` na `daemon.lock` (patrz U1). Sukces: ty jesteś odpalającym —
  detach-spawn (`src/daemon/launch.rs` z U2) → poll (krótki interwał, ograniczony całkowity czas)
  aż `daemon.json` się pojawi i health-check zwróci sukces → usuń `daemon.lock` → zbuduj
  `ApiClient`. Porażka (`daemon.lock` już istnieje): ktoś inny właśnie odpala — **nie spawnuj**,
  tylko pollinguj `daemon.json` tak samo jak w gałęzi "plik istnieje" poniżej. Health-check to
  zwykłe uwierzytelnione żądanie (np. `GET /executions` ze świeżo odczytanym tokenem) — żaden
  endpoint w tym planie nie jest zwolniony z `require_token` (patrz U3/U4), więc nie ma osobnego
  nieautoryzowanego `/health`; sekwencja w High-Level Technical Design upraszcza to jako "/health"
  wyłącznie ilustracyjnie. Jeśli plik `daemon.json` istnieje i proces (PID) żyje, i tak zweryfikuj
  przez health-check zanim zwrócisz klienta (żeby odróżnić "PID żyje, ale to zombie/inny proces
  przejął PID" od prawdziwie działającego daemona) — przy niepowodzeniu health-checku traktuj jak
  martwego (AE3), posprzątaj i przejdź do gałęzi spawn/lock powyżej.
- Nowe warianty `ClientError` w razie potrzeby (np. `Conflict` dla `409`).

**Patterns to follow:**
- Istniejący `ApiClient::get_json<T>` helper i `ClientError` (`client.rs`)
- `DaemonInfo` z U1

**Test scenarios:**
- Happy path (AE2): brak działającego daemona, `ensure_daemon_running` odpala go, zwraca
  działający `ApiClient` łączący się poprawnie.
- Happy path (AE3): plik odkrywania wskazuje martwy PID — funkcja sprząta wpis i odpala świeżego
  daemona zamiast zawieść.
- Happy path: plik odkrywania wskazuje żywy, odpowiadający daemon — funkcja zwraca klienta bez
  odpalania nowego procesu (brak duplikatów).
- Edge case: dwa procesy klienckie wywołują `ensure_daemon_running` niemal równocześnie, gdy
  żaden daemon nie działa — dzięki `daemon.lock` (`write_atomic_exclusive`) dokładnie jeden
  faktycznie odpala proces daemona, drugi wykrywa istniejącą blokadę i przechodzi w tryb
  pollingu zamiast spawnować własny; test dowodzi braku dwóch konkurencyjnych daemonów
  nasłuchujących na różnych portach dla tego samego state roota (np. przez uruchomienie obu
  wywołań współbieżnie w teście i sprawdzenie, że tylko jeden proces potomny został faktycznie
  utworzony).
- Error path: PID żyje, ale to inny proces (nie daemon) — health-check HTTP zawodzi, funkcja
  traktuje to jak martwego daemona i odpala nowego.
- Error path: `ApiClient` mutacja zwraca `409` — metoda klienta mapuje to na czytelny
  `ClientError::Conflict`, nie generyczny `Server`.
- Integration (AE5 z perspektywy klienta): `stream_execution` odbiera kolejne zdarzenia aż do
  zdarzenia terminalnego i kończy strumień samodzielnie po jego odebraniu (żeby CLI mogło na nim
  blokować, patrz U6).

**Verification:**
- `ensure_daemon_running` pokryte testami dla wszystkich trzech scenariuszy AE2/AE3 + brak
  duplikatów przy współbieżnym wywołaniu.

---

### U6. Migracja `narsor run`/`resume` i pozostałych komend CLI

**Goal:** CLI przestaje konstruować `Engine`/`ExecutionStore`; wszystkie komendy (mutujące i
czytające) stają się klientami REST przez `ensure_daemon_running`.

**Requirements:** R4, R6

**Dependencies:** U5

**Files:**
- Modify: `src/cli/commands.rs` (`run` L137-197, `resume` L644-673, oraz istniejące komendy
  czytające `events`/`steps`/`conversation` — przełączyć z bezpośredniego odczytu store'u na
  `ApiClient`)
- Test: `tests/cli_commands.rs` lub rozszerzenie istniejącej suity CLI (nazwa pliku do
  potwierdzenia przy implementacji, jeśli suita CLI już istnieje pod inną nazwą)

**Approach:**
- `run`: `ensure_daemon_running(&paths)` → `client.start_task(...)` (odpowiednik dzisiejszego
  `tasks::start_task_with` + `engine.run`) → `client.stream_execution(id)` zblokowane w pętli aż
  do zdarzenia terminalnego → wydrukuj wynik/kod wyjścia jak dziś.
- `resume`: analogicznie, `client.resume(id)` → `stream_execution(id)` do zdarzenia
  terminalnego.
- Komendy czytające (`events`/`steps`/`conversation`): zamiast bezpośredniego czytania
  `ExecutionStore`, wołają `ensure_daemon_running` i odpowiednie GET na `ApiClient` (istniejące
  3 endpointy z `routes.rs` już to pokrywają — zmienia się tylko strona wywołująca).

**Execution note:** Zacznij od testu integracyjnego CLI, który dziś przechodzi przez bezpośrednie
`Engine`, i przepisz go tak, żeby oczekiwał zachowania klienckiego (ten sam efekt końcowy — status
wykonania, treść logu — inna droga) — łatwiej wychwycić regresję w zachowaniu obserwowalnym niż w
wewnętrznych wywołaniach.

**Patterns to follow:**
- `ApiClient`/`ClientError` (U5)
- Istniejący kształt `cli::commands::run`/`resume` (kolejność kroków, komunikaty, kody wyjścia —
  zachować bez zmian obserwowalnych dla operatora)

**Test scenarios:**
- Happy path: `narsor run <workflow>` kończy się tym samym statusem wyjścia i tą samą treścią
  logu co dziś, mimo że wewnętrznie przechodzi przez daemona.
- Happy path: `narsor resume <id>` analogicznie.
- Happy path: `narsor events`/`steps`/`conversation` zwracają identyczną treść jak dzisiejszy
  bezpośredni odczyt store'u, teraz przez REST.
- Edge case (AE2): żaden daemon nie działa przed wywołaniem `narsor run` — komenda odpala go
  transparentnie, operator nie musi nic robić ręcznie.
- Edge case (AE4): `narsor run <workflow>` z jednego terminala i "uruchom zaznaczone" w TUI z
  drugiego, celujące w to samo wykonanie niemal równocześnie — dokładnie jedno przechodzi
  (weryfikacja end-to-end, uzupełniająca test jednostkowy z U3).
- Error path: daemon odpowiada `409` (konflikt) na `resume` — CLI wypisuje czytelny komunikat
  operatorowi zamiast surowego błędu HTTP.

**Verification:**
- Żadna funkcja w `src/cli/commands.rs` nie konstruuje `Engine`/`ExecutionStore` po tym unicie
  (grep, Success Criterion 2).

---

### U7. Migracja TUI na czystego klienta REST(+SSE)

**Goal:** `App`/`tui::mod::run` przestają posiadać `Arc<Engine>`; wszystkie ścieżki zapisu i
subskrypcja zdarzeń przechodzą przez `ApiClient`.

**Requirements:** R4, R6, R7, R8, R11

**Dependencies:** U5

**Files:**
- Modify: `src/tui/mod.rs` (usunięcie `recover()` L171, usunięcie `api::start`/`Engine::new`
  L186-199, zastąpienie `ensure_daemon_running`, `App::new` dostaje wyłącznie `client`, pętla
  `select!` czyta z SSE stream zamiast `engine.subscribe()`)
- Modify: `src/tui/app.rs` (`spawn_background` L2648 — mutujące wywołania zamienione na
  `client.start_task`/`resume`/`deliver_input` + śledzenie przez SSE zamiast bezpośredniego
  `.await` na `Engine`; `write_transition`/`manual_transition`/`force_transition` L2796+ →
  `client.transition(...)`; `start_new_task` L3232 → `client.start_task(...)`)
- Test: `tests/tui_state.rs` (dostosowanie do `App` sterowanego przez klienta — patrz Approach)

**Approach:**
- `App` przestaje trzymać `Arc<Engine>`; jedynym uchwytem do silnika jest `ApiClient`.
- Event loop: zamiast `engine.subscribe()` (broadcast w procesie), `App` otwiera globalny SSE
  stream (`client.stream_all()`) przy starcie i miesza go do istniejącego `tokio::select!` obok
  wejścia z klawiatury — kształt pętli się nie zmienia, zmienia się tylko źródło zdarzeń.
  Ewentualnie: strumień per-wykonanie dla aktualnie wybranego wykonania, jeśli wolumen zdarzeń
  globalnych okaże się zbyt duży dla widoku listy — decyzja implementacyjna, nie architektoniczna.
- `spawn_background`'s dzisiejsza rola (odpalić i zapomnieć, wynik przez event stream) upraszcza
  się: teraz to POST + `202`, a "w tle" dzieje się w procesie daemona, nie klienta — TUI już nie
  potrzebuje własnego `tokio::spawn` do tego celu, tylko zwykłego `.await` na krótkim POST
  (dyspozycja wraca szybko, patrz Key Technical Decisions), po czym polega na SSE dla dalszych
  aktualizacji UI.
- `manual_transition`/`force_transition`: zamiast bezpośredniego zapisu, `client.transition(id,
  request)`; klienckie sprawdzenie `engine.is_active` (dzisiejsze app.rs:2806) staje się zbędne —
  serwer robi tę samą kontrolę autorytatywnie (U3) i zwraca `409`, TUI po prostu wyświetla wynik.

**Execution note:** Migracja `tests/tui_state.rs` jest ryzykownym punktem — te testy dziś sterują
`App` przez bezpośrednio-współdzielony `Arc<Engine>`. Zacznij od uruchomienia realnego daemona w
tle testu (analogicznie do `tests/api.rs`'s wzorca bindowania prawdziwego serwera na temp-root),
zanim przepiszesz asercje — łatwiej zweryfikować, że nowy fixture faktycznie odtwarza dawne
zachowanie, gdy stary i nowy fixture chwilowo współistnieją w gałęzi roboczej.

**Patterns to follow:**
- `ApiClient`/SSE stream (U5)
- Istniejący kształt `tokio::select!` w `tui::mod::run` (linie ~212-237) — zmienia się źródło
  zdarzeń, nie struktura pętli

**Test scenarios:**
- Happy path: operator startuje nowe zadanie w TUI, widzi jego postęp na żywo (przez SSE), tak
  samo jak dziś przez broadcast.
- Happy path: `manual_transition`/`force_transition` z TUI działają identycznie z perspektywy
  operatora (te same komunikaty sukcesu/błędu), teraz przez REST.
- Happy path (AE2): TUI startuje bez działającego daemona, odpala go transparentnie, operator
  widzi tę samą listę zadań co dziś.
- Edge case: TUI próbuje `force_transition` na aktywnym wykonaniu — dostaje `409` z serwera,
  wyświetla ten sam komunikat odmowy co dzisiejsze klienckie sprawdzenie `is_active`.
- Integration (AE1): operator zamyka TUI (`q`) w trakcie akcji — proces TUI kończy się, `ps`
  nadal pokazuje daemona i harness, plik rozmowy dalej rośnie (weryfikowane end-to-end w U8, tu
  wystarczy potwierdzić że TUI nie trzyma żadnego uchwytu, którego zamknięcie ubiłoby daemona).
- Integration (AE5): dwa procesy TUI podłączone do tego samego daemona widzą na żywo te same
  zdarzenia tego samego wykonania.

**Verification:**
- Grep: `src/tui/` nie zawiera już żadnej konstrukcji `Engine::new`/`ExecutionStore::new`
  (Success Criterion 2, tym razem dla TUI).

---

### U8. Aktualizacja suit testowych i test end-to-end AE1

**Goal:** Doprowadzić istniejące suity do zielonego stanu po granicy klient/daemon i dodać nowy
test dowodzący AE1 od końca do końca.

**Requirements:** Success Criteria 3, 4

**Dependencies:** U2, U3, U4, U5, U6, U7

**Files:**
- Modify: `tests/api.rs`, `tests/tui_state.rs`, `tests/engine_recovery.rs`, `tests/tasks.rs`
  (dostosowane, nie usunięte)
- Create: `tests/daemon_e2e.rs` (nowy test end-to-end na AE1)

**Approach:**
- `tests/engine_recovery.rs`: potwierdzić, że logika `recover()` sama w sobie jest niezmieniona
  (testowana jak dziś, bezpośrednio na `ExecutionStore`); dodać/dostosować jeden test
  potwierdzający, że `narsor daemon` startowo woła `recover()` dokładnie raz.
- `tests/tasks.rs`: `start_task`/`start_task_with` testowane jak dziś bezpośrednio na `Engine` w
  testach jednostkowych silnika (te testy nie muszą przechodzić przez REST — testują logikę
  domenową, nie granicę transportu); dodać test integracyjny potwierdzający, że endpoint
  `POST /tasks` z U3 faktycznie woła tę samą funkcję.
- `tests/daemon_e2e.rs` (AE1): (1) odpal prawdziwego daemona (detach-spawn z U2) na temp state
  root, (2) przez `ApiClient` uruchom akcję długotrwałą (np. workflow ze sztucznym opóźnieniem w
  test-fixture executorze), (3) zasymuluj zabicie procesu klienta (w teście: po prostu nie trzymaj
  żadnego uchwytu do niczego poza `ApiClient`, ewentualnie odpal właściwy krok w osobnym procesie
  testowym i zabij go), (4) potwierdź przez `DaemonInfo`/`ps`-equivalent (np. sprawdzenie PID z
  pliku odkrywania) że proces daemona wciąż żyje, (5) potwierdź że plik rozmowy/log zdarzeń
  nadal rośnie (kolejne odczyty przez SSE lub przez `GET /executions/:id/events` pokazują nowe
  zdarzenia po symulowanym zabiciu klienta), (6) posprzątaj: `narsor daemon stop`.

**Patterns to follow:**
- `TempRoot` hand-rolled fixture (już używany w `tests/api.rs`/`tests/tui_state.rs`) — bez
  wprowadzania `tempfile` jako nowej zależności.

**Test scenarios:**
- Integration (AE1, Success Criterion 4): scenariusz opisany w Approach, od startu daemona do
  potwierdzenia przetrwania.
- Regression: cała istniejąca suita (`tests/api.rs`, `tests/tui_state.rs`,
  `tests/engine_recovery.rs`, `tests/tasks.rs`) przechodzi po migracji, bez usuniętych testów
  (Success Criterion 3) — usunięte mogą być tylko testy, które dosłownie testowały wycofany kod
  (np. `ApiHandle::Drop::abort`), z jawnym uzasadnieniem w komunikacie commita, nie ciche
  usunięcie pokrycia.

**Verification:**
- Success Criteria 1-4 z origin zweryfikowane end-to-end: `ps` po AE1, grep na konstrukcję
  `Engine`/`ExecutionStore`, zielona istniejąca suita, nowy test AE1.

---

## System-Wide Impact

- **Interaction graph:** Każdy dzisiejszy bezpośredni wywołujący `Engine`/`ExecutionStore` (TUI,
  `cli::commands::run`/`resume`, komendy czytające) zamienia się na `ApiClient`; jedyny nowy węzeł
  w grafie to sam proces daemona, przez który przechodzi cały ruch.
- **Error propagation:** Błędy silnika (`EngineError::AlreadyRunning`, itd.) muszą teraz przejść
  przez granicę HTTP (kod statusu + ewentualne ciało) zamiast być zwracane bezpośrednio jako typy
  Rust — `ClientError` w `ApiClient` jest miejscem, gdzie ta granica jest jawnie modelowana.
- **State lifecycle risks:** Plik odkrywania musi być zapisywany atomowo (U1) — połowiczny zapis
  widziany przez konkurencyjnego klienta byłby gorszy niż brak pliku. Wyścig dwóch klientów
  odpalających daemona jednocześnie przy jego braku jest wprost pokryty testem w U5.
- **API surface parity:** Trzy istniejące GET-y z `src/api/routes.rs` (D52) pozostają bez zmian
  kontraktu — tylko strona wywołująca (CLI) się zmienia (U6), nie ich zachowanie.
- **Integration coverage:** AE1 (przetrwanie po zabiciu klienta), AE4 (wyścig dwóch sterowników),
  AE5 (wielu klientów SSE) wymagają testów przekraczających granicę procesów, nie tylko
  jednostkowych — pokryte w U3-U5 i U8.
- **Unchanged invariants:** `Engine::begin_run`/`active_runs` (R11), `Engine::pause`/`pause_now`
  (D34), format zdarzeń/store'u na dysku, i blokujące zachowanie CLI z perspektywy operatora
  (mimo zmiany mechanizmu blokowania) pozostają niezmienione.

---

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| Detach terminala (`setsid` w `pre_exec`) źle przetestowany na wszystkich powłokach/terminalach (tmux, zwykły TTY) mógłby zostawić daemona wciąż podatnego na `SIGHUP` | Test AE1 explicite symuluje zamknięcie terminala/zabicie klienta; dodatkowy defensywny handler `SIGHUP` w samym daemonie (U2) jako drugi pas bezpieczeństwa |
| Migracja `tests/tui_state.rs` na fixture z realnym daemonem może okazać się znacznie wolniejsza niż dzisiejsze testy w procesie | Uruchamiać daemon raz na moduł testowy (nie per-test), analogicznie do wzorców współdzielonych fixture'ów w Rust; jeśli wolność okaże się problemem, rozważyć not-yet-decided fake-client trait jako follow-up, nie blokować tego planu |
| SSE z `BroadcastStream::Lagged` przy wolnym kliencie mógłby ukryć utracone zdarzenia przed operatorem | U4 loguje `Lagged` po stronie serwera; rozważyć syntetyczne zdarzenie `resync` jeśli w praktyce okaże się potrzebne (nie blokujące dla MVP) |
| Rozszerzenie zakresu na komendy czytające CLI (R4) jest szersze niż samo Success Criterion 2 i mogłoby zostać pominięte przy code review jako "niepotrzebne" | Jawnie nazwane w Key Technical Decisions z cytatem z R4/D52, żeby review nie cofnął tej części jako nadmiarowej |
| Rozbicie `Engine::resume`/`deliver_human_input` na dwie fazy (U3) to zmiana publicznego API silnika, nie kosmetyczna refaktoryzacja — błędny podział mógłby zepsuć istniejące, jednowywołaniowe użycia w `cli::commands`/testach silnika | U3 wymaga zachowania dzisiejszych jednowywołaniowych metod jako cienkich wrapperów nad nowym dwufazowym API, żeby `tests/tasks.rs` i `cli::commands` nie musiały się zmieniać; U8 explicite weryfikuje zieloność tej suity |
| `daemon.lock`-owa bramka spawnu (U1/U5) mogłaby zostać zapomniana i zaimplementowana jako gołe "sprawdź plik, spawnuj" — odtwarzając wyścig dwóch daemonów na tym samym state roocie | U1 Approach i test w U5 explicite wymagają `write_atomic_exclusive` jako bramki, z dedykowanym scenariuszem testowym na współbieżny spawn |

---

## Sources & References

- **Origin document:**
  [docs/brainstorms/2026-09-26-narsor-daemon-lokalny-requirements.md](../brainstorms/2026-09-26-narsor-daemon-lokalny-requirements.md)
- Dokument nadrzędny:
  [docs/brainstorms/2026-09-25-rozproszony-narsor-requirements.md](../brainstorms/2026-09-25-rozproszony-narsor-requirements.md)
- Powiązany plan:
  [docs/plans/2026-09-26-001-fix-tui-background-engine-runs-plan.md](2026-09-26-001-fix-tui-background-engine-runs-plan.md)
- Kod: `src/api/mod.rs`, `src/api/routes.rs`, `src/api/client.rs`, `src/engine/runner.rs`,
  `src/tui/app.rs`, `src/tui/mod.rs`, `src/cli/commands.rs`, `src/cli/mod.rs`,
  `src/persistence/paths.rs`, `src/persistence/atomic.rs`, `src/tasks/mod.rs`
- `ARCHITECTURE_DECISIONS.md` — D23, D33, D34, D36, D37, D38, D52
- External: axum 0.7 `response::sse` docs; `tokio-rs/axum` discussions #2260, #2264, #1670, issue
  #2594; `rust-lang/rust` PR #142391 (`setsid` w `CommandExt`)
