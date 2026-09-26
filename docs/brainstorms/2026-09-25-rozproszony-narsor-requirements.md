---
date: 2026-09-25
topic: rozproszony-narsor
---

# Rozproszony narsor: centrala, workery, dzierżawa i przekazywanie pracy

## Summary

Narsor przestaje być jednym procesem, który jest jednocześnie silnikiem i interfejsem, a staje
się **centralą** (autorytatywny stan wykonań, katalog deklaratywny, rejestr dzierżaw, API) oraz
**workerami** (pętla silnika wykonująca akcje poświadczeniami konkretnej osoby). CLI i TUI
stają się klientami API i przestają mieć własną ścieżkę wykonania.

Zdolnością, dla której to się robi, jest **przekazywanie pracy między osobami**: nadzorca
przydziela wykonanie do osoby i może je wywłaszczyć, żeby przekazać komuś innemu. Kluczowe
spostrzeżenie, na którym stoi cały projekt: **przekazanie pracy, wznowienie po zabiciu workera
i odtworzenie po crashu to jedna operacja** — nowy worker podejmuje wykonanie z trwałego stanu.
Narsor ma już jej trudną połowę (`src/persistence/recovery.rs`, D23).

---

## Problem Frame

Dzisiejszy narsor jest pojedynczym procesem (D2), jednoużytkownikowym (D35), w którym CLI i TUI
wołają silnik w tym samym procesie. Nie ma API — `axum` jest w `Cargo.toml` i nie ma **ani
jednego użycia**. Wykonanie jest związane z maszyną, na której wystartowało: nie da się go
podjąć gdzie indziej, bo nie jest samowystarczalne.

### Cztery „tryby pracy" to nie są cztery tryby

Pierwotny opis mówił o czterech trybach: lokalny daemon, daemon w dockerze, daemon na k8s
z taskami w kontenerach, oraz pod nadzorczy nad wieloma zestawami. To nie są cztery tryby,
tylko punkty w przestrzeni **trzech niezależnych osi**:

| oś | wartości |
|---|---|
| transport | in-process (dziś) / API |
| miejsce wykonania akcji | proces potomny / kontener / pod |
| krotność | jeden worker / wiele workerów i nadzorca |

„Lokalny daemon" i „daemon w dockerze" to ten sam punkt (API, proces potomny, jeden worker),
różniący się wyłącznie opakowaniem. Zapisanie ich jako dwóch trybów zaprosiłoby dwie ścieżki
kodu tam, gdzie jest jedna decyzja wdrożeniowa. **Budujemy jedną architekturę o wymiennych
elementach, konfigurowaną na cztery sposoby.**

### Co dziś blokuje podjęcie wykonania gdzie indziej

- **Wykonanie nie jest samowystarczalne.** `resolve_definition` (`src/tasks/mod.rs:562`) wczytuje
  definicję **ponownie** po nazwie lub ścieżce przy każdym wznowieniu; analogicznie
  `repository: <id>` jest rozwiązywane przez rejestr przy użyciu. Podjęcie wykonania na maszynie
  z innym katalogiem deklaratywnym albo padnie, albo — gorzej — wznowi je **na innej definicji
  tej samej nazwy**, co jest cichym złamaniem D31 (zakaz mutacji definicji w trakcie wykonania).
- **Brak własności wykonania.** Nic nie gwarantuje, że tylko jeden proces prowadzi dane
  wykonanie. Dziś trzyma to konwencja („jeden proces, jedno wykonanie"), a plan rozbicia katalogu
  danych świadomie odłożył dzierżawę, heartbeat i fencing.
- **Workspace jest ulotny.** Niezacommitowana praca istnieje wyłącznie na dysku tej maszyny.
  `CleanupPolicy` już dziś odmawia sprzątnięcia workspace'u z niewypchniętą pracą, czyli system
  wie, że ta praca jest cenna — ale nie potrafi jej nigdzie przenieść.
- **Sesja agenta jest lokalna i nieprzezroczysta.** `session_id` (`harness/claude_code.rs:192`)
  to string od harnessu, wskazujący na stan w jego katalogu domowym.

### Dlaczego praca ma zostawać na sprzęcie osoby

Rozważono wariant, w którym wykonanie żyje wyłącznie centralnie, a przypisanie osoby jest tylko
polem („kto odpowiada na pytania"). Odrzucony: agent ma działać **na sprzęcie i poświadczeniach
konkretnej osoby** — jej subskrypcji harnessu, jej dostępach do repozytoriów, jej `ssh-config`.
To przesądza, że compute jest rozproszone, a przypisanie wyznacza, czyj compute pracuje.

---

## Requirements

### Niezmienniki architektury

- R1. **Przypisanie wyznacza, czyj worker ma prawo wykonywać akcje wykonania.** Nie jest to
  wyłącznie wskazanie osoby odpowiadającej na `HumanAction`.
- R2. **Wykonanie jest samowystarczalne.** Niesie przypięte: źródło definicji (nie nazwę),
  rozwiązany remote, `ref` i bazowy sha. Da się je podjąć, mając wyłącznie jego rekord.
- R3. **Dokładnie jeden worker ma prawo zapisywać do wykonania**, i wynika to z mechanizmu,
  nie z konwencji.
- R4. **Granica kroku jest jedyną granicą trwałości.** Po `TransitionTaken` worker zatwierdza
  do centrali zdarzenia, stan i deltę workspace'u jako jedną operację.
- R5. **Repozytorium produktowe jest wyłącznie źródłem kodu.** Stan zadania nie trafia tam nigdy
  — ani jako gałąź, ani jako commit, ani jako tag.
- R6. **Przekazanie, wznowienie po zabiciu workera i odtworzenie po crashu to jedna ścieżka
  kodu.** Różnią się wyłącznie tym, czy zmienia się przypisanie i czy stary worker trzeba
  wcześniej uciszyć.

### Podział na centralę i workera

- R7. Centrala jest autorytatywnym magazynem wykonań, udostępnia katalog deklaratywny tylko do
  odczytu, prowadzi rejestr dzierżaw i API nadzorcy. **Nie wykonuje akcji i nie posiada
  poświadczeń do cudzych repozytoriów.**
- R8. Worker prowadzi pętlę silnika i wykonuje akcje **własnymi poświadczeniami**. Jego katalog
  stanu i katalog pracy są jednorazowym scratchem; katalog deklaratywny nie jest mu potrzebny,
  bo definicja przyjeżdża z wykonaniem (R2).
- R9. Worker zapisuje lokalnie istniejącą warstwą persystencji i replikuje na granicy kroku
  (R4). Nie ma zapisu przelotowego po sieci przy każdym zdarzeniu.
- R10. CLI i TUI rozmawiają wyłącznie z centralą. **Nie istnieje druga ścieżka uruchamiania
  silnika.** Tryb lokalny to centrala i worker w jednym procesie na gnieździe unixowym.
- R11. Uruchomienie procesu potomnego ma jedno miejsce w kodzie (`Sandbox`), z implementacjami:
  proces potomny, kontener, pod. Konsumentami są akcja agenta, akcja skryptowa **oraz
  provisioner workspace'u** (`git clone`).

### Dzierżawa i fencing

- R12. Dzierżawa to `(execution_id, worker_id, assignee, epoch, expires_at)`, gdzie `epoch` jest
  **monotonicznie rosnącym licznikiem per wykonanie**, inkrementowanym przy każdym przyznaniu.
- R13. Każde zatwierdzenie kroku i każda odpowiedź człowieka niosą `epoch`. Centrala odrzuca
  wszystko z `epoch` mniejszym niż bieżący.
- R14. Worker odnawia dzierżawę okresowo. Brak odnowienia przez ustalony czas powoduje, że
  **worker ubija własną akcję w locie i przestaje** — bez czekania na polecenie.
- R15. Wygaśnięcie dzierżawy czyni wykonanie dostępnym do przejęcia. Wygaśnięcie **nie jest
  dowodem śmierci workera**; bezpieczeństwo daje `epoch`, nie założenie o śmierci.

### Wywłaszczenie i przypisanie

- R16. Wywłaszczenie ma dwa tryby, a wybiera nadzorca: **łagodny** (D34 pause — dokończ bieżącą
  akcję, nie zaczynaj następnej) i **siłowy** (D34 interrupt + D33 best-effort — ubij akcję).
- R17. Wywłaszczenie łagodne ma własny limit czasu z **eskalacją do siłowego**, a stan pośredni
  („wywłaszczenie w toku") jest widoczny dla nadzorcy.
- R18. Wywłaszczenie wykonania zapauzowanego (`pending_input.json`) jest natychmiastowe — nic
  nie działa, więc sprowadza się do zmiany przypisania i inkrementu `epoch`.
- R19. Wywłaszczenie workera nieosiągalnego to inkrement `epoch` plus oczekiwanie na wygaśnięcie
  dzierżawy. **Jest to ta sama ścieżka co odtworzenie po crashu.**
- R20. Zatwierdzenie kroku powstałe z wywłaszczenia siłowego jest **oznaczone jako przerwane**,
  żeby następna osoba wiedziała, że dziedziczy akcję przerwaną w pół, a nie rozmyślną pracę.
- R21. Każda operacja zmieniająca stan zapisuje, **kto** ją wykonał: przypisanie, wywłaszczenie,
  odpowiedź człowieka.

### Checkpoint i przejęcie

- R22. Zatwierdzenie kroku zawiera: zdarzenia od ostatniego zatwierdzenia (deduplikacja po
  `event_id`, D25), `state.json` z kontekstem, niezmienny deskryptor workspace'u oraz deltę.
- R23. Delta to: diff plików śledzonych, pliki nieśledzone i nieignorowane, lista usuniętych.
  Dla workspace'u typu `local` i dla wykonania bez workspace'u to samo pojęcie degraduje się do
  archiwum katalogu. **Jeden mechanizm dla wszystkich rodzajów workspace'u.**
- R24. Pliki ignorowane przez gita **nie są przenoszone**; worker docelowy je odtwarza. Definicja
  workflow może zadeklarować `preserve_paths` dla wyjątków.
- R25. Centrala trzyma **wyłącznie ostatni** checkpoint. Zapis w całości, potem atomowe
  przestawienie wskaźnika, potem usunięcie poprzedniego.
- R26. Materializacja workspace'u przy przejęciu zawsze przebiega: klon z przypiętego remote na
  przypięty sha **własnymi poświadczeniami**, potem nałożenie delty. **Nigdy z ocalałego
  wolumenu** — jedna droga dla crashu, restartu i przekazania.
- R27. Przejęcie przeprowadza reconciliację istniejącym mechanizmem (`reconcile`, D23): akcja
  w locie nie jest odtwarzana, wykonanie wpada w `Unknown`, decyduje człowiek.
- R28. Sesja agenta jest przy przejęciu **porzucana jawnym zdarzeniem**, a pierwsza akcja agenta
  po przejęciu otrzymuje w prompcie podsumowanie przekazania wygenerowane z logu zdarzeń.

### Transport, tożsamość i bezpieczeństwo fazy 1

- R29. Tożsamość wołającego jest przenoszona przez **każde** wywołanie API od początku, nawet
  jako nieweryfikowany string.
- R30. Punkt decyzji autoryzacyjnej jest **jeden** i w fazie 1 zawsze zezwala.
- R31. Bez skonfigurowanej autoryzacji centrala wiąże się **wyłącznie z gniazdem unixowym lub
  loopbackiem**. Wyjście na interfejs sieciowy wymaga jawnej flagi `--insecure-no-auth`, głośno
  logowanej przy starcie.
- R32. W trybie lokalnym przez gniazdo unixowe tożsamość jest weryfikowalna na poziomie systemu
  (`SO_PEERCRED`).
- R33. Sekret jest własnością workera, **nigdy wykonania**. Nic w rekordzie wykonania ani
  w checkpoincie nie przenosi poświadczeń.

---

## Acceptance Examples

- **AE1** (R6, R26, R27). Workflow o pięciu krokach zostaje przerwany po kroku 3. Nowy proces,
  wskazany na ten sam magazyn, odtwarza workspace **z checkpointu**, nie z ocalałego katalogu,
  i dokańcza wykonanie ze stanem końcowym identycznym jak przebieg bez zakłóceń.
- **AE2** (R12, R13). Worker A traci sieć, dzierżawa wygasa, worker B przejmuje wykonanie.
  Worker A wraca i próbuje zatwierdzić krok — **zatwierdzenie zostaje odrzucone** ze względu na
  nieaktualny `epoch`, a stan w centrali pozostaje nietknięty.
- **AE3** (R14). Worker, który nie odnowił dzierżawy przez ustalony czas, **sam ubija swoją
  akcję** i przestaje pracować, bez żadnego polecenia z zewnątrz.
- **AE4** (R16). Wywłaszczenie łagodne wykonania w trakcie akcji skryptowej pozwala akcji
  dobiec do końca, zatwierdza krok i dopiero wtedy oddaje dzierżawę. Żadna praca nie ginie.
- **AE5** (R17). Wywłaszczenie łagodne akcji, która nie kończy się w limicie czasu, **eskaluje
  do siłowego**, a nadzorca przez cały ten czas widzi stan „wywłaszczenie w toku".
- **AE6** (R18, R13). Osoba pisze odpowiedź na `HumanAction` i zostaje w tym czasie wywłaszczona.
  Jej odpowiedź przychodzi ze starym `epoch` i **zostaje odrzucona**.
- **AE7** (R2). Wykonanie wystartowane z definicji `X` w wersji 1 zostaje podjęte przez workera,
  którego katalog deklaratywny zawiera **inną** definicję o nazwie `X`. Wykonanie kontynuuje na
  swojej przypiętej definicji.
- **AE8** (R23). Wykonanie **bez zadeklarowanego workspace'u** (pusty katalog) zostaje przekazane
  innemu workerowi i kontynuuje pracę — ten sam mechanizm checkpointu, bez gita.
- **AE9** (R5). Po pełnym cyklu życia wykonania z dwoma przekazaniami w repozytorium produktowym
  **nie powstała żadna gałąź, commit ani tag** pochodzący od mechanizmu przekazywania.
- **AE10** (R31). Centrala bez skonfigurowanej autoryzacji, uruchomiona z adresem nasłuchu na
  interfejsie sieciowym i **bez** `--insecure-no-auth`, odmawia startu.
- **AE11** (R28). Po przekazaniu log zdarzeń zawiera jawne zdarzenie porzucenia sesji agenta,
  a nie ciszę.

---

## Success Criteria

1. Test równoważności przerwania przechodzi dla **każdego** punktu przerwania k ∈ 1..N: przebieg
   nieprzerwany i przebieg przerwany po kroku k z przejęciem dają ten sam stan końcowy.
2. Cztery sytuacje wywłaszczenia (zapauzowane / biegnące łagodne / biegnące siłowe / worker
   nieosiągalny) mają pokrycie testowe i **dwa** mechanizmy, nie cztery.
3. Round-trip checkpointu: brudne drzewo → delta → świeży klon → nałożenie → drzewo identyczne
   z dokładnością do plików ignorowanych.
4. Istniejące suity testowe przechodzą bez zmian semantyki, w szczególności `engine_recovery`.
5. `axum` ma użycia; nie istnieje ścieżka uruchomienia silnika z pominięciem API.
6. `grep` po `Command::new` w `src/` trafia **wyłącznie** w implementacje `Sandbox` — żaden
   executor ani provisioner nie uruchamia procesu samodzielnie.

---

## Scope Boundaries

- **Autoryzacja** — faza 2, z własnym dokumentem wymagań. Faza 1 buduje wyłącznie szwy: przenoszenie
  tożsamości (R29), jeden punkt decyzji (R30) i furtkę, która nie może się otworzyć przez
  przypadek (R31). **Przypisanie nie jest granicą bezpieczeństwa** i musi to być napisane
  w dokumentacji: każdy, kto dosięgnie API, może wywłaszczyć każdego.
- **Praca offline.** Wykluczona przez wybór modelu: worker bez sieci nie odnowi dzierżawy
  i sam się zatrzyma (R14).
- **Wysoka dostępność centrali.** Jeden proces, jeden pisarz — świadomie, bo to ta sama decyzja,
  która pozwala trzymać licznik sekwencji logu zdarzeń w pamięci. Centrala pada → praca staje.
- **Migracja sesji agenta.** Niewykonalna: `session_id` jest nieprzezroczysty i lokalny dla
  harnessu. D18 zachowuje swój fallback.
- **Wersjonowanie checkpointów i podgląd historii workspace'u.** Trzymamy ostatni (R25).
- **Multi-tenancy, kwoty i budżety tokenów.**
- **Web UI** (D36 i tak odkłada) oraz **równoległość w workflow** (D32).
- **Zmiana formatu magazynu na bazę danych.** D3 zostaje: pliki.

### Deferred to Follow-Up Work

- **Model uprawnień** (kto może wywłaszczyć kogo, kto widzi czyje konteksty) — faza 2.
- **Polityka retencji zakończonych wykonań w centrali.**
- **Odporność zatwierdzenia kroku na bardzo duże delty** (limit rozmiaru, sygnalizacja).

---

## Key Decisions

- **D-A. Jedna architektura, cztery konfiguracje.** W kodzie nie ma pojęcia „trybu". Jest
  centrala, worker i to, gdzie je postawisz. Zapobiega mnożeniu ścieżek dla decyzji, które są
  wdrożeniowe, nie architektoniczne.

- **D-B. Stan lokalnie u workera, replikacja na granicy kroku.** Odrzucono zapis przelotowy po
  sieci: kosztuje round trip na zdarzenie, nie przeżywa mrugnięcia sieci i wyrzuca całą istniejącą
  warstwę persystencji. Replikacja na granicy kroku daje **to samo okno utraty dla stanu i dla
  workspace'u** — jeden niezmiennik zamiast dwóch — i zachowuje istniejący kod. Przy okazji
  licznik sekwencji w pamięci (`event_log.rs:179`) przestaje być ograniczeniem po obu stronach,
  bo po obu stronach pisze jeden proces.

- **D-C. Fencing przez monotoniczny `epoch`, nie przez identyfikator dzierżawy.** Dzierżawa
  z UUID-em nie fencuje: żeby odrzucić zapis spóźnialskiego, trzeba umieć powiedzieć „ten jest
  starszy", a UUID-y nie mają porządku. Alternatywa — lista unieważnień — wymaga trwałego stanu
  rosnącego w nieskończoność.

- **D-D. Fencing chroni stan, nie świat.** Worker odcięty siecią dalej ma poświadczenia i może
  zrobić `git push`. Granica jest nieusuwalna bez transakcji rozproszonych i D24 już ją nazywa
  (at-least-once + klucze idempotencji). Łagodzi ją **wyłącznie** samo-fencing workera (R14),
  który ogranicza szkodę do okna odnowienia.

- **D-E. Checkpoint to baza plus delta, trzymany centralnie.** Odrzucono dwa warianty:
  *gałęzie WIP na repozytorium produktowym* — bo każdy push odpala CI repo produktowego, wymaga
  praw zapisu, zderza się z branch protection, zaśmieca wspólne repo **i nie obsługuje workflow
  bez repozytorium**, które narsor wprost wspiera (`tasks/mod.rs`, D39); oraz *dedykowane repo
  stanu* — bo wiąże z dostępnością forge'a, wymaga praw do zakładania repozytoriów i również nie
  obsługuje przypadku bez gita. Baza+delta jest mała (worker docelowy klonuje z origin sam,
  własnymi kluczami — przenosi się tylko różnica) i degraduje się uczciwie do archiwum.

- **D-F. Materializacja zawsze z checkpointu, nigdy z ocalałego wolumenu.** Kuszące jest podpiąć
  PVC i po restarcie poda odzyskać drzewo. Działa to **wyłącznie przy tej samej osobie**, więc
  tworzy drugą ścieżkę, której przekazanie nie może użyć. Jedna droga, wolniejsza, zawsze ta sama.

- **D-G. Wszystko, co rozstrzyga się przy starcie, jest przypinane do wykonania.** Definicja,
  remote, `ref`, bazowy sha. Usuwa cykl bootstrapowy (żeby podjąć zadanie, trzeba było mieć
  katalog, w którym leży jego definicja), domyka ciche złamanie D31 i czyni przejęcie operacją
  startującą z samego rekordu wykonania.

- **D-H. Kontenery są ostatnim kamieniem milowym, nie trzecim.** Skoro agent działa na sprzęcie
  i poświadczeniach osoby, kontener przestaje być mechanizmem dystrybucji, a zostaje mechanizmem
  izolacji. Wartość leży w maszynerii przypisania i przejęcia; konteneryzacja jest potem
  implementacją jednego traitu, nie przebudową.

- **D-I. Furtka bezpieczeństwa domyślnie zamknięta.** Odłożenie autoryzacji nie może się zdarzyć
  przez przypadek, dlatego brak autoryzacji **odmawia** wyjścia do sieci, zamiast ostrzegać.

### Decyzje MVP, które to zastępuje

| decyzja | status po tej zmianie |
|---|---|
| **D2** Single Process | zastąpiona: centrala + workery; tryb lokalny zachowuje jeden proces jako konfigurację, nie jako architekturę |
| **D35** Single-user / brak auth | zastąpiona: tożsamość od fazy 1, autoryzacja w fazie 2 |
| **D14** Izolacja agentów przez Docker | doprecyzowana: izolacja jest jedną z implementacji `Sandbox`, a nie sposobem uruchamiania całości |

Do dopisania jako **D48+** w `ARCHITECTURE_DECISIONS.md`, z oznaczeniem powyższych jako
zastąpionych — dokument jest w README nazwany normatywnym i nie może zostać z zapisem
sprzecznym z kodem.

---

## Kamienie milowe

Dokument opisuje architekturę docelową. **Plan wdrożeniowy powstaje per kamień milowy**;
najbliższy jest M1.

- **M0 — dokończenie refaktoru katalogu danych.** U4–U9 z planu
  `2026-09-25-001-refactor-rozbicie-katalogu-danych-plan.md` do końca. Trzy korzenie są przesłanką
  tej zmiany, a na wpół wylądowany refaktor jest najgorszą bazą. Poprawka: `WorkspaceProvisioner::root()`
  musi zniknąć zgodnie z D-Q tamtego planu, bo provisioner idzie potem za szew `Sandbox`.
- **M1 — przejęcie lokalne.** R2, R22–R27. Test najbardziej ryzykownej tezy całego projektu
  (przekazanie ≡ odtworzenie) **bez sieci, bez dzierżawy, bez kontenerów**. Dowód: AE1.
- **M2 — szew API.** R7–R10, R29. Centrala i worker jako role w jednej binarce, klienci zamiast
  wbudowanego silnika, tryb lokalny na gnieździe unixowym.
- **M3 — dzierżawa, fencing, wywłaszczenie.** R12–R21, R31. Dopiero tu wolno rozdzielić centralę
  i workera na dwie maszyny.
- **M4 — przypisanie i nadzorca.** Tożsamość, `assignee`, kolejka nieprzypisanych, API i widok.
- **M5 — `Sandbox` i kontenery.** R11.
- **Faza 2 — autoryzacja.** Własny dokument wymagań.

---

## Dependencies / Assumptions

- Zakłada się ukończenie M0; trzy korzenie i rejestr jako katalog plików są przesłankami.
- Zakłada się, że granice kroku w workflow agentowym dzieli rząd wielkości minut, co czyni
  akceptowalnym okno utraty równe jednemu krokowi (R4) i koszt replikacji na tej granicy (D-B).
- Zakłada się, że delta workspace'u jest mała, bo pliki ignorowane nie są przenoszone (R24).
  Założenie do zweryfikowania w M1 na realnym repozytorium.
- Wersja nie jest wydana; kontrakt CLI i układu danych zmienia się bez okresu przejściowego,
  poza jawnymi komunikatami o wycofaniu.
- `git` pozostaje wymaganiem czasu wykonania po stronie **workera**, nie centrali.

---

## Outstanding Questions

### Deferred to Planning

- **Postać delty.** Czy diff plików śledzonych to `git diff` w formacie tekstowym, czy
  `git bundle` gałęzi tymczasowej trzymanej lokalnie u workera i nigdy nie pushowanej.
  Drugie radzi sobie z plikami binarnymi i zmianami uprawnień, pierwsze jest czytelne.
- **Czas dzierżawy i okres odnawiania.** Wprost wpływa na okno szkody przy samo-fencingu (D-D).
- **Kształt magazynu checkpointów w centrali** — katalog per wykonanie obok `executions/`,
  czy osobny korzeń. Wiąże się z tym, czy centrala dostaje czwarte pojęcie korzenia obok trzech
  z M0.
- **Czy zatwierdzenie kroku jest jednym wywołaniem API, czy dwoma** (metadane osobno od delty),
  co ma znaczenie dla wznawiania przerwanego zatwierdzenia.
- **Wyjątek od D-B: zapis rozmowy trwającej próby.** `docs/plans/2026-09-26-002-*.md` (U2/U3,
  ADR D51) wprowadził zapis rozmowy agenta jako plik JSONL dopisywany przyrostowo w trakcie
  trwania próby, w katalogu stanu (`{state}/executions/{id}/conversations/`), a nie na granicy
  kroku. To już dziś rozjeżdża się z D-B, o ile centrala ma widzieć podgląd *trwającej* próby, a
  nie tylko jej stan po zamknięciu: replikacja na granicy kroku (D-B) replikowałaby zapis dopiero
  po zamknięciu próby i jej kompresji do `.jsonl.zst`, więc worker odcięty w trakcie długiej
  próby nie miałby jak pokazać centrali, co się dzieje, dopóki próba się nie skończy. M1 tego nie
  rozstrzyga: serwer odczytu (D52) jest wbudowany w proces TUI i czyta lokalny plik zwykły w
  trakcie zapisu, co działa tylko wtedy, gdy czytelnik i pisarz są tym samym procesem albo mają
  współdzielony dostęp do tego samego systemu plików — założenie, które M2 (worker zdalny) łamie.
  Do uzgodnienia w M2: albo zapis rozmowy trwającej próby dostaje własną, częstszą granicę
  replikacji niż krok (osobny niezmiennik obok D-B, z dodatkowym kosztem sieciowym uzasadnionym
  tym, że to dane diagnostyczne, nie stan silnika), albo podgląd trwającej próby na zdalnym
  workerze idzie przez bezpośrednie połączenie do workera (nowa powierzchnia, poza REST+SSE
  D37/D38), albo M2 świadomie akceptuje, że podgląd trwającej próby jest niedostępny z centrali
  dopóki próba się nie zamknie (degradacja R8 w konfiguracji rozproszonej, nie lokalnej).
