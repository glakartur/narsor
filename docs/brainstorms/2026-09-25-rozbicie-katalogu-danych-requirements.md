---
date: 2026-09-25
topic: rozbicie-katalogu-danych
---

# Rozbicie katalogu danych: katalog deklaratywny, stan i katalog pracy

## Summary

Jeden korzeń danych (`data_dir`) rozpada się na trzy niezależnie wskazywane katalogi:
**katalog deklaratywny** (repozytoria i workflowy), **katalog stanu** (wykonania) i **katalog
pracy** (workspace'y zadań). Przy okazji rejestr repozytoriów przestaje być jednym dokumentem,
a staje się katalogiem plików — jeden plik na repozytorium. Powód jest wdrożeniowy: katalog
deklaratywny ma być współdzielony między podami i montowany tylko do odczytu, stan ma dać się
zamontować osobno, a workspace z checkoutami gita jest zasobem lokalnym poda i nie ma czego
szukać na wspólnym woluminie.

---

## Problem Frame

`data_dir` trzyma dziś pod jednym korzeniem cztery rzeczy o zupełnie różnej semantyce
współdzielenia i różnej żywotności:

| zasób | kto współdzieli | zapis | żywotność |
|---|---|---|---|
| `repositories.json`, `workflows/` | wszystkie pody | rzadki, z deploymentu | wersja wdrożenia |
| `executions/{id}/` | proces, który je prowadzi | ciągły | zadanie i jego historia |
| `tasks/{id}/code/` | nikt — lokalne dla poda | ciągły, ciężki | czas trwania zadania |

Jeden korzeń zmusza je do jednego woluminu, co w modelu wielopodowym jest złe w obie strony.
Checkoutu gita nie chce się trzymać na `ReadWriteMany` — to scratch, któremu należy się
`emptyDir`. A katalogu repozytoriów nie da się dostarczyć razem z deploymentem, bo siedzi
sztywno pod korzeniem, który jest zapisywalnym katalogiem danych.

Workflowy mają ten problem rozwiązany już dziś: `WorkflowResolver` czyta je ze ścieżki
z jawną precedencją, w tym z katalogu konfiguracyjnego użytkownika. Rejestr repozytoriów jest
tu jedynym wyjątkiem — i to on wyznaczył kształt tej zmiany.

Drugi, niezależny problem ujawnia się dopiero przy wielu pisarzach: `add`, `update` i `remove`
w rejestrze wykonują cykl *wczytaj cały dokument → zmień → zapisz cały dokument*
(`src/registry/store.rs:338,358`). Każdy zapis z osobna jest atomowy, ale dwa procesy
dodające **różne** repozytoria tracą jeden z wpisów, bez żadnego sygnału. To klasyczna utrata
aktualizacji i jedyna rzecz w tej zmianie, która jest naprawą błędu, a nie przestawieniem
ścieżek.

---

## Requirements

**Podział korzenia**

- R1. Katalog deklaratywny, katalog stanu i katalog pracy są trzema niezależnymi ustawieniami,
  z których każde można wskazać osobno. Żadne nie jest wyprowadzane z pozostałych.
- R2. Każde z trzech ma wartość wbudowaną `./.narsor`, a nazwy podkatalogów wewnątrz nich
  zostają dokładnie takie jak dziś: `executions/`, `tasks/`, `workflows/`. Samo rozbicie
  korzenia nie przestawia ani jednego pliku — jedyną zmianą układu jest przebudowa rejestru
  (R7), która jest osobną decyzją i ma własną regułę zgodności (R12).
- R3. Flaga `--root` ustawia wszystkie trzy naraz. Pozostaje pojedynczym pokrętłem dla pracy
  lokalnej i dla testów.
- R4. Każde z trzech jest dostępne jako zmienna środowiskowa i jako klucz w pliku
  konfiguracyjnym, na tych samych zasadach co pozostałe ustawienia.
- R5. Każdy konsument dostaje w konstruktorze wyłącznie ten korzeń, którego używa, i nie ma
  dostępu do pozostałych. Podział jest egzekwowany przez typy, nie przez konwencję.

**Wycofanie `data_dir`**

- R6. `data_dir` znika jako ustawienie, ale nie znika po cichu: pozostaje w rejestrze jako wpis
  wycofany. Użyty w pliku albo w zmiennej środowiskowej zatrzymuje start komunikatem
  nazywającym trzy ustawienia, które go zastąpiły.

**Rejestr repozytoriów**

- R7. Rejestr jest katalogiem plików — jeden plik na repozytorium, w formacie YAML, pod
  katalogiem deklaratywnym. Identyfikatorem repozytorium jest nazwa pliku.
- R8. Dodanie repozytorium tworzy plik w trybie wyłącznym. Kolizja identyfikatorów jest
  zgłaszana przez system plików, a nie przez wcześniejszy odczyt katalogu.
- R9. Żadna operacja na rejestrze nie odczytuje i nie zapisuje wpisów innych niż ten, którego
  dotyczy. Dwa procesy dodające różne repozytoria nie mogą sobie nawzajem zgubić wpisu.
- R10. Identyfikator repozytorium musi być bezpieczną nazwą pliku; sprawdzane jest to przy
  zapisie i przy odczycie.
- R11. Uszkodzony pojedynczy wpis nie wywraca całego rejestru. Wypis rejestru pokazuje wpisy,
  które się wczytały, nazywa pliki, które się nie wczytały, i kończy się błędem. Odczyt
  konkretnego identyfikatora zawodzi twardo.
- R12. Stary `repositories.json` nie jest czytany, konwertowany ani zgłaszany. Wersja nie jest
  wydana i nie ma tu czego chronić.

**Katalog tylko do odczytu**

- R13. Próba zapisu do katalogu deklaratywnego zamontowanego tylko do odczytu kończy się
  komunikatem nazywającym **ustawienie**, nie tylko ścieżkę, i wskazującym brak prawa zapisu
  jako przyczynę.

**Ścieżka szukania workflowów**

- R14. Workflow jest szukany w kolejności: jawnie wskazany plik, katalog deklaratywny,
  definicja wbudowana w binarkę. Katalog konfiguracyjny użytkownika wypada ze ścieżki —
  katalog deklaratywny jest odtąd jedynym katalogiem, z którego czyta się definicje.
- R15. `workflow eject` bez wskazanego celu zapisuje do katalogu deklaratywnego, czyli
  w miejsce, z którego workflowy są faktycznie czytane.

---

## Acceptance Examples

- AE1. **Dotyczy R1, R5.** Dane trzy katalogi wskazane na trzy różne miejsca i przebieg
  workflow od początku do końca: w katalogu deklaratywnym nie powstaje nic związanego
  z wykonaniami, w katalogu stanu nie powstaje workspace, a w katalogu pracy nie powstaje nic
  poza checkoutem.
- AE2. **Dotyczy R2.** Dane uruchomienie bez jakiejkolwiek konfiguracji: `executions/`, `tasks/`
  i `workflows/` powstają pod `./.narsor` dokładnie tam, gdzie powstawały przed zmianą.
  Jedyną różnicą w układzie jest `repositories/` w miejsce `repositories.json`.
- AE3. **Dotyczy R3.** Dane uruchomienie z `--root`: podgląd konfiguracji pokazuje trzy wiersze
  z tą samą wartością i ze źródłem `--root`.
- AE4. **Dotyczy R6.** Dany plik konfiguracyjny z kluczem `data_dir`: komenda kończy się błędem
  nazywającym trzy ustawienia, które go zastąpiły. To samo dla zmiennej `NARSOR_DATA_DIR`,
  która nie może zostać zignorowana w ciszy.
- AE5. **Dotyczy R9.** Dane dwa równoległe dodania **różnych** repozytoriów: po zakończeniu
  w rejestrze są oba wpisy. Ten przykład nie przechodzi na dzisiejszym kodzie.
- AE6. **Dotyczy R8.** Dane dwa dodania tego **samego** identyfikatora: drugie kończy się
  błędem o kolizji, a pierwszy wpis pozostaje nietknięty.
- AE7. **Dotyczy R11.** Dany rejestr z jednym plikiem, którego nie da się wczytać: wypis
  pokazuje pozostałe repozytoria, nazywa uszkodzony plik i kończy się kodem błędu.
- AE8. **Dotyczy R13.** Dany katalog deklaratywny bez prawa zapisu: dodanie repozytorium kończy
  się komunikatem nazywającym ustawienie i brak prawa zapisu, a nie surowym błędem systemu
  plików.
- AE9. **Dotyczy R14.** Dana definicja workflow w katalogu konfiguracyjnym użytkownika i brak
  jej w katalogu deklaratywnym: nie zostaje znaleziona. Utrata tego zachowania jest celowa.

---

## Success Criteria

- Katalog deklaratywny da się zamontować tylko do odczytu i współdzielić między podami,
  a katalog pracy zostaje lokalnym `emptyDir` — bez żadnej zmiany w kodzie poza konfiguracją.
- Dwa procesy rejestrujące różne repozytoria w jednym katalogu deklaratywnym nie gubią wpisów.
- Uruchomienie bez konfiguracji zachowuje się jak przed zmianą, a na dysku różni się wyłącznie
  postacią rejestru.
- Żaden konsument warstwy persystencji nie ma technicznej możliwości złożyć ścieżki z cudzego
  korzenia.
- Nikt nie odczytuje tej zmiany jako gotowości na wielopodowe współdzielenie stanu — ograniczenie
  jest zapisane wprost.

---

## Scope Boundaries

- **Własność wykonania, dzierżawa, heartbeat, fencing.** Rozbicie katalogów **nie** czyni
  katalogu stanu bezpiecznym do współdzielenia. Licznik sekwencji logu zdarzeń jest cache'owany
  w procesie (`src/persistence/event_log.rs:179`), a pauzy siedzą w `Mutex` w runnerze
  (`src/engine/runner.rs:1284`); dwa pody na jednym katalogu stanu nadal nadpiszą sobie zdarzenia
  bez żadnego sygnału. Centralny pod zlecający pracę podom roboczym to osobny brainstorm.
- **Migracja starego `repositories.json`** (R12).
- **Widok konfiguracji w TUI** — pozostaje odłożony, jak w poprzednim brainstormie.
- **Zmiana wartości wbudowanej na `~/.narsor`.** Rozważona i wycofana w trakcie rozmowy: robiła
  testy mniej bezpiecznymi, bo test, który zapomni ustawić korzeń, pisałby do prawdziwego
  katalogu domowego. Ręczne testy obsługuje `~/.config/narsor/config.yaml` z jawnie ustawionymi
  katalogami.
- **Kaskada szukania pliku konfiguracyjnego** — bez zmian. Plik jest szukany niezależnie od
  tego, gdzie wskazują trzy katalogi.

---

## Key Decisions

- **D-A. Trzy ustawienia, nie dwa i nie cztery.** Trzy odpowiadają dokładnie trzem semantykom
  współdzielenia. Wariant dwóch (odcięcie samego scratchu) rozwiązywał tylko checkouty gita
  i odkładał rozdzielenie deklaracji od stanu. Wariant czterech (rejestr osobno od workflowów)
  dawał swobodę, której nikt nie potrzebuje, kosztem czwartego ustawienia.
- **D-B. Brak ustawienia-bazy.** `data_dir` mogło przetrwać jako baza, z której wyprowadzają się
  trzy pozostałe — wygoda jednego pokrętła w pliku. Odrzucone: wymagałoby nowego wariantu źródła
  („wyprowadzone z `data_dir`") i złamałoby model *jedno ustawienie = jeden klucz = jedno źródło*,
  na którym stoi cały podgląd konfiguracji. Rolę jednego pokrętła przejmuje `--root` (R3).
- **D-C. Wartość wbudowana wspólna i układ nietknięty.** Wariant z osobnym podkatalogiem na
  warstwę (`./.narsor/catalog`, `/state`, `/work`) czytelniej pokazywałby podział lokalnie, ale
  odciąłby istniejące katalogi. Zmiana ma być niewidoczna dopóki ktoś świadomie nie rozdzieli
  katalogów.
- **D-D. Rejestr jako katalog plików.** Dzisiejszy komentarz modułowy argumentuje za jednym
  dokumentem: mały, jeden atomowy rename, brak częściowo zapisanego katalogu. Argument jest
  poprawny dla jednego pisarza i przestaje być poprawny dla wielu. Gwarancja „jeden rename dla
  całości" jest potrzebna tylko wtedy, gdy wpisy zależą od siebie — a nie zależą. Katalog
  plików czyni unikalność identyfikatora własnością strukturalną, zmniejsza zasięg uszkodzenia
  do jednego wpisu i mapuje się 1:1 na ConfigMapę.
- **D-E. YAML zamiast JSON w rejestrze.** Katalog deklaratywny staje się jednorodny: dwa
  katalogi ręcznie edytowalnych plików YAML, tak jak workflowy obok. Format wpisu ma być
  czytany i pisany także przez człowieka i przez deployment, nie tylko przez narzędzie.
- **D-F. Katalog użytkownika wypada ze ścieżki workflowów.** Po wprowadzeniu katalogu
  deklaratywnego dwa poziomy ścieżki robiłyby to samo. Jedno miejsce to jedna odpowiedź na
  pytanie „skąd ten workflow". Konsekwencje: `workflow eject` zmienia domyślny cel (R15) i traci
  błąd o nieustawionym `HOME`, a `WorkflowResolver` przestaje znać jakąkolwiek globalną
  lokalizację — więc hak testowy `with_user_dir` przestaje być potrzebny.
- **D-G. `user_config_dir()` przenosi się do modułu konfiguracji.** Mieszka dziś w module
  workflowów (`src/workflows/mod.rs`), a po D-F jego jedynym konsumentem zostaje kaskada
  konfiguracji (`src/config/cascade.rs:33`).
- **D-H. Uszkodzony wpis: wypis nazywa, odczyt zawodzi.** Wypis rejestru idzie za wzorcem
  `config show` — pokazuje, co się dało, oznacza to, czego się nie dało, i kończy się błędem.
  Odczyt konkretnego identyfikatora nie ma tego luksusu: workflow odwołuje się do repozytorium
  po identyfikatorze, więc przybliżona odpowiedź jest gorsza niż żadna.

---

## Dependencies / Assumptions

- Zakłada się, że wdrożenie wielopodowe jest realnym kierunkiem, ale ta zmiana dowozi wyłącznie
  granulację montowania i naprawę rejestru. Koordynacja pracy między podami jest świadomie poza
  zakresem i nie jest przez tę zmianę ani ułatwiona, ani utrudniona.
- Zakłada się, że rejestr pozostaje mały — garść repozytoriów. Wypis jako `readdir` plus odczyt
  każdego pliku jest przy tej skali nieodróżnialny od odczytu jednego dokumentu.
- Zakłada się, że identyfikator repozytorium jest i pozostanie bezpieczną nazwą pliku; warunek
  ten jest już sprawdzany przy tworzeniu workspace'u (`is_usable_directory_name`).
- Zmiana dotyka opublikowanego kontraktu konfiguracji sprzed jednego dnia (`config show`,
  `--root`, `data_dir`). Wersja nie jest wydana, więc kontrakt zmienia się bez okresu przejściowego
  — poza jawnym komunikatem o wycofaniu `data_dir` (R6).

---

## Outstanding Questions

### Deferred to Planning

- Nazwy kluczy trzech ustawień. Robocze: `catalog_dir`, `state_dir`, `work_dir`.
- Czy plik logu TUI ląduje pod katalogiem stanu (roboczo: tak — to stan procesu, nie deklaracja).
- Kształt typów ścieżek w `src/persistence/paths.rs` (roboczo: trzy typy w tym samym module,
  żeby układ nadal był opisany dokładnie raz).
- Dokładne brzmienie komunikatu o wycofanym `data_dir` i o katalogu bez prawa zapisu.
- Kolejność prac: test dwóch równoległych dodań (AE5) jako pierwszy i czerwony, bo jest dowodem,
  że przebudowa rejestru ma sens.
