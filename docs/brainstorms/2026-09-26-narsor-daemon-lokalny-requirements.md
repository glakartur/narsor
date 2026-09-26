---
date: 2026-09-26
topic: narsor-daemon-lokalny
---

# Lokalny daemon: TUI i CLI jako klienci API, nie właściciele silnika

## Summary

Dziś `Engine` i `ExecutionStore` żyją wewnątrz dowolnego procesu, który je otworzy —
`src/tui/mod.rs::run` albo `src/cli/commands.rs::run`/`resume`. Skutek: zamknięcie `narsor tui`
(`q`, zamknięcie terminala, `Ctrl+C`) kończy cały proces, a razem z nim każdy w tle prowadzony bieg
silnika i jego dziecko-harness (`claude`, `kill_on_drop(true)`) — cicho, bez żadnego zdarzenia.
Ten dokument zawęża **M2** z `docs/brainstorms/2026-09-25-rozproszony-narsor-requirements.md`
(„centrala i worker jako role w jednej binarce, klienci zamiast wbudowanego silnika") do jednego
lokalnego workera na jednym dysku — bez dzierżaw, wywłaszczenia i przejęcia międzymaszynowego
(M1, M3–M5), które rozwiązują inny problem: oddawanie pracy innej osobie.

Nowa podkomenda `narsor daemon` staje się **jedynym** procesem, który kiedykolwiek konstruuje
`Engine` albo otwiera `ExecutionStore` do zapisu. `narsor tui` i `narsor run`/`resume` stają się
klientami jego REST(+SSE) API — odkrywanym i odpalanym automatycznie w razie potrzeby, nigdy
nieubijanym przy wyjściu klienta.

---

## Problem Frame

- `src/tui/app.rs:2648` (`spawn_background`) to goły `tokio::spawn` bez zachowanego `JoinHandle`
  gdziekolwiek — potwierdza diagnozę „w pełni odczepione, nic nie czeka przy wyjściu", którą
  `docs/plans/2026-09-26-001-fix-tui-background-engine-runs-plan.md` zostawił jako nierozstrzygnięte
  pytanie („Deferred to Implementation": czy `App`/`tui::mod::run` powinno śledzić `JoinHandle`,
  żeby przy `q` poczekać na biegnące w tle wykonania, czy zostawić w pełni odczepione).
- `src/api/mod.rs:125-129` — `Drop` dla `ApiHandle` **przerywa** (`abort`) zadanie serwera. To
  poprawne dla dzisiejszego „wbudowanego, tylko do odczytu" API, którego cały sens istnienia to
  czas życia jednego klienta (TUI); staje się błędne w chwili, gdy serwer ma żyć dłużej niż
  jakikolwiek pojedynczy klient.
- Poza czterema wywołaniami, które plan 2026-09-26-001 już przeniósł w tło
  (`start_new_task`/`run_selected`/`submit_input`/`apply_transition`), znaleziono w tej sesji
  **dwie dodatkowe ścieżki**, które omijają `Engine` i piszą bezpośrednio do store'u:
  - `src/tui/app.rs:2796` (`write_transition`, używane przez `manual_transition`/`force_transition`)
    pisze do store'u wprost, chronione wyłącznie klienckim sprawdzeniem `engine.is_active`
    (app.rs:2806).
  - `src/tui/app.rs:3232` (`start_new_task`) woła `tasks::start_task` bezpośrednio, nie
    `Engine::start`.

  Obie muszą dostać własny mutujący endpoint w tym samym kroku, inaczej zamknięcie TUI na
  bezpośredni zapis do dysku pozostawiłoby dziurę identyczną do tej, którą ten dokument zamyka.
- Ta sesja już wdrożyła interim fix: `persistence::recovery::recover` (D23) wywoływane z
  `src/tui/mod.rs:171` — bo do tej pory to `tui::run` było jedynym procesem, który mógł zgubić
  śledzenie akcji w locie. Ten dokument przenosi to wywołanie do daemona, bo to on odtąd jest tym
  procesem.

---

## Requirements

- R1. Nowa podkomenda `narsor daemon` prowadzi `Engine` + `ExecutionStore` + API w procesie, który
  przeżywa dowolne pojedyncze wywołanie `narsor tui`/`narsor run`/`narsor resume`.
- R2. Daemon jest w pełni odczepiony od terminala wywołującego w chwili odpalenia (nowa
  sesja/grupa procesów, stdio przekierowane do pliku logu), tak żeby sygnał terminala
  (`SIGHUP`/`Ctrl+C`) albo zakończenie procesu klienta, który go odpalił, nigdy do niego nie
  dotarły.
- R3. Daemon nigdy nie zatrzymuje się sam — ani z bezczynności, ani gdy wszystkie wykonania są
  terminalne, ani gdy ostatni klient się rozłączy. Kończy go wyłącznie jawne `narsor daemon stop`.
- R4. `narsor tui` oraz komendy CLI, które dziś otwierają `Engine`/`ExecutionStore` wprost
  (`tui::run`, `cli::commands::run`, `resume`, i komendy czytające) odkrywają działającego daemona
  albo go odpalają, po czym działają wyłącznie jako klienci REST(+SSE) — żadna z nich nie
  konstruuje `Engine` ani nie otwiera `ExecutionStore` samodzielnie.
- R5. Odkrywanie: plik pod katalogiem stanu (obok `log_file()`, `src/persistence/paths.rs:130`)
  niesie dość, żeby klient znalazł i uwierzytelnił się do działającego daemona (adres/port,
  bearer token), z restrykcyjnymi uprawnieniami. Klient, który nie znajdzie pliku, znajdzie martwy
  PID albo daemona, który nie odpowiada, sprząta wpis i odpala świeżego daemona.
- R6. API dostaje mutujące endpointy pokrywające każdą dzisiejszą ścieżkę zapisu z TUI/CLI: start
  wykonania (dziś bezpośrednie `tasks::start_task`, app.rs:3232), `run`/`resume`
  (`Engine::run`/`resume`, runner.rs), dostarczenie odpowiedzi człowieka
  (`Engine::deliver_human_input`), ręczna/wymuszona tranzycja (dziś bezpośredni zapis
  `write_transition`, app.rs:2796) oraz lista wykonań — żaden klient nie pisze do store'u ani nie
  woła `Engine` bezpośrednio dla żadnej z nich.
- R7. API dostaje żywy strumień zdarzeń (SSE), per wykonanie albo globalny, zastępujący
  wewnątrzprocesowy `Engine::subscribe` (broadcast) jako źródło live-update dla TUI — dokładnie
  seam, który jego własny komentarz w kodzie już nazywa („this is the seam an SSE endpoint will
  take over (D38)").
- R8. Startowa reconciliacja po awarii (`persistence::recovery::recover`, D23) przenosi się do
  startu daemona; `tui::mod::run` przestaje ją wołać bezpośrednio, bo może już w ogóle nie
  posiadać store'u.
- R9. Transport to REST po loopback HTTP (dzisiejsze `axum`-owe `src/api`), plus nowy endpoint
  SSE — nigdy unix socket, żeby to samo API mogło później obsłużyć frontend w przeglądarce bez
  drugiego szwu transportowego.
- R10. `Drop` dla `ApiHandle` przestaje przerywać zadanie serwera, gdy ten serwer jest własnością
  daemona (tylko krótkotrwały, jednoklientowy serwer — o ile taki kształt w ogóle przetrwa —
  zachowuje dzisiejsze przerywanie-przy-drop).
- R11. Istniejąca straż „już biegnie" (`Engine::begin_run`/`active_runs`, runner.rs:359,1490) nadal
  chroni przed dwoma sterownikami wyścigującymi to samo wykonanie — teraz wymuszona wewnątrz
  jedynego procesu, który w ogóle może wołać `Engine`, co zamyka lukę, którą osobna instancja
  `Engine` w CLI mogłaby dziś otworzyć ponownie.

---

## Acceptance Examples

- **AE1** (R2, R3). Akcja agenta jest w trakcie, gdy operator wychodzi z `narsor tui` (`q`) albo
  zamyka terminal; `ps` zaraz potem nadal pokazuje daemona i proces harnessu (`claude`), a plik
  rozmowy/log zdarzeń dalej rośnie.
- **AE2** (R4, R5). Żaden daemon nie działa. `narsor tui` odpala go automatycznie, łączy się i
  operator widzi tę samą listę zadań co dziś.
- **AE3** (R5). Plik odkrywania wskazuje martwy PID. Klient wykrywa to, sprząta wpis i odpala
  świeżego daemona zamiast zawieść.
- **AE4** (R6, R11). `narsor run <workflow>` z jednego terminala i „uruchom zaznaczone" w TUI z
  drugiego, celujące w to samo wykonanie niemal równocześnie — dokładnie jedno przechodzi, drugie
  jest odrzucone tak samo, jak dziś odrzucane są dwa sterowniki w tle TUI.
- **AE5** (R7). Dwa procesy TUI (albo TUI i przyszły klient w przeglądarce) podłączone do tego
  samego daemona widzą na żywo te same zdarzenia tego samego wykonania.
- **AE6** (R8). Daemon zostaje zabity w trakcie akcji i wystartowany ponownie; przerwane wykonanie
  dostaje status `Unknown` i zdarzenie `Error`, dokładnie jak dzisiejsze `recover()` — zmienia się
  tylko miejsce wywołania.

---

## Success Criteria

1. `ps`/`pstree` po wyjściu z `narsor tui` w trakcie akcji nadal pokazuje daemona i dziecko-harness.
2. Żadna ścieżka kodu poza `src/api` i startem daemona nie konstruuje `Engine` ani nie otwiera
   `ExecutionStore` do zapisu (weryfikacja grepem, w stylu kryterium nr 5/6 dokumentu
   nadrzędnego: „`axum` ma użycia; nie istnieje ścieżka uruchomienia silnika z pominięciem API").
3. Istniejące suity (`tests/api.rs`, `tests/tui_state.rs`, `tests/engine_recovery.rs`,
   `tests/tasks.rs`) przechodzą, dostosowane do nowej granicy klient/daemon, a nie usunięte.
4. Nowy test integracyjny dowodzi scenariusza AE1 od końca do końca: start akcji przez daemona,
   zabicie procesu klienta, potwierdzenie że daemon i harness wciąż żyją, a plik rozmowy dalej
   rośnie.

---

## Scope Boundaries

- **M1** (checkpoint/przejęcie workspace'u między maszynami) — niepotrzebne; jeden lokalny worker,
  workspace nigdy nie opuszcza swojego dysku.
- **M3** (dzierżawy, fencing `epoch`, wywłaszczenie) — rozwiązuje przekazywanie pracy między
  ludźmi, nie ten problem.
- **M4** (przypisanie/nadzorca), **M5** (`Sandbox`/kontenery) — niedotknięte, niepowiązane.
- **Autoryzacja fazy 2** (dokument nadrzędny) — bez zmian; model bearer-tokena pozostaje jedyną
  kontrolą dostępu.
- **Sam frontend w przeglądarce** — poza zakresem; ten dokument tylko kształtuje transport jako
  REST/SSE, żeby dało się go dodać później bez drugiego szwu backendu.

### Deferred to Follow-Up Work

- Dokładny kształt i lokalizacja pliku odkrywania (`daemon.json` czy podobny) pod katalogiem
  stanu — brak istniejącej konwencji poza `log_file()` (paths.rs:130), do ustalenia przy
  planowaniu.
- Dokładne nazwy/metody/kształty payloadów nowych mutujących endpointów REST — do planowania.
- Czy `narsor run`/`narsor resume` nadal blokują wywołujący terminal, czekając na zdarzenie
  terminalne przez SSE, czy wracają od razu po dispatchu — do planowania; domyślne założenie to
  dzisiejsze, blokujące, pierwszoplanowe zachowanie CLI (jawnie nazwane „unchanged invariant" w
  planie 2026-09-26-001), chyba że planowanie znajdzie powód, by to zmienić.
- Semantyka `narsor daemon stop`: łagodna (dokończ akcje w locie) kontra natychmiastowa — do
  planowania; wiąże się z D33 (best-effort cancellation) i D34 (pause vs. interrupt) już
  zapisanymi w `ARCHITECTURE_DECISIONS.md`.
- `REQUEST_TIMEOUT = 800ms` w `ApiClient` (`client.rs:49`), dziś uzasadnione tym, że wywołanie
  siedzi w `select!` obsługi klawiszy TUI — wymaga rewizji, gdy przez tego samego klienta zaczną
  iść mutujące, nie-podsekundowe wywołania. Do planowania.
- Czy wielu równoczesnych klientów dzieli jeden bearer token z pliku odkrywania, czy każdy
  dostaje własny — do planowania.

---

## Key Decisions

- **D-J. Daemon jest jedynym procesem, który kiedykolwiek konstruuje `Engine` albo otwiera
  `ExecutionStore` do zapisu.** TUI i CLI stają się klientami, zamykając każdą ścieżkę
  bezpośredniego zapisu znalezioną w tej sesji: odczepiony `tokio::spawn` w
  `tui::app::spawn_background` (app.rs:2648), bezpośredni zapis do store'u w `write_transition`
  (app.rs:2796) i bezpośrednie wywołanie `tasks::start_task` w `start_new_task` (app.rs:3232) — nie
  tylko cztery wywołania, które plan 2026-09-26-001 przeniósł już w tło.
- **D-K. REST+SSE, nigdy unix socket — nawet dla tego czysto lokalnego wycinka.** Powód: frontend
  w przeglądarce jest znanym, bliskim konsumentem tego samego API, a drugi transport dla niego
  później oznaczałby dwa szwy backendu zamiast jednego.
- **D-L. Przerywanie-przy-drop dla `ApiHandle` (mod.rs:125-129) jest poprawne wyłącznie dla
  serwera, którego cały sens istnienia to czas życia jednego klienta** (dzisiejszy, wbudowany,
  tylko-do-odczytu przypadek). Serwer daemona nie może dziedziczyć tego zachowania — odziedziczenie
  go po cichu odtworzyłoby dokładnie tego samego buga, który ten dokument zamyka.
- **D-M. Startowa reconciliacja (D23) należy do tego procesu, który jako ostatni może zgubić
  śledzenie akcji w locie** — dziś `tui::mod::run` (fix tej sesji), odtąd daemon.

---

## Dependencies / Assumptions

- Zakłada już wdrożony mechanizm biegów w tle z planu 2026-09-26-001
  (`Engine::begin_run`/`active_runs`, runner.rs:359,1490) jako straż współbieżności, na której ten
  dokument się opiera — teraz wymuszaną istnieniem jednego procesu, nie konwencją.
  Zakłada wdrożone w tej sesji podpięcie `persistence::recovery::recover` do
  `src/tui/mod.rs:171` jako stan przejściowy, który R8 tego dokumentu przenosi dalej.
- Zakłada `docs/brainstorms/2026-09-25-rozproszony-narsor-requirements.md` jako architekturę
  nadrzędną; ten dokument jest zawężeniem jej kamienia M2 do jednego lokalnego workera, nie
  konkurencyjnym projektem.

---

## Outstanding Questions

### Deferred to Planning

Patrz sekcja „Deferred to Follow-Up Work" wyżej — każdy tamtejszy punkt jest pytaniem otwartym dla
planu wdrożeniowego, nierozstrzygniętym w tym dokumencie.

---

## Sources & References

- Dokument nadrzędny:
  [docs/brainstorms/2026-09-25-rozproszony-narsor-requirements.md](2026-09-25-rozproszony-narsor-requirements.md)
  (kamień M2)
- Powiązany, już wdrożony plan:
  [docs/plans/2026-09-26-001-fix-tui-background-engine-runs-plan.md](../plans/2026-09-26-001-fix-tui-background-engine-runs-plan.md)
  — mechanizm biegów w tle, na którym ten dokument się opiera; jego własne pytanie „Deferred to
  Implementation" o śledzenie `JoinHandle` przy wyjściu jest tym, co ten dokument rozstrzyga,
  usuwając jego przesłankę — nie ma już `Engine` należącego do TUI, dla którego trzeba by trzymać
  `JoinHandle`.
- Kod: `src/api/mod.rs:125-140`, `src/api/routes.rs:78-112`, `src/api/client.rs:49,89-130`,
  `src/engine/runner.rs:353-660` (powierzchnia `Engine`), `src/tui/app.rs:2648,2678,2739,2796,3232,3319`,
  `src/cli/commands.rs:137-197,644-673`, `src/persistence/paths.rs:114-130`,
  `src/tui/mod.rs:171` (miejsce wywołania `recover()`), `tests/api.rs`.
