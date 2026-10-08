---
title: Przeprojektowanie TUI — projekt
type: design
date: 2026-10-08
topic: tui-przeprojektowanie
status: do-przeglądu
---

# Przeprojektowanie TUI — projekt

## Cel

TUI narsora jest przeprojektowane od zera: jeden interfejs, jeden model zadania
(task z control plane), stabilna lista będąca jednocześnie kolejką realizacji
i szczegóły zadania, w których odbywa się cała praca.

Kolejność ważności czynności operatora, która kształtuje układ:

1. **Odpowiadam** — pytania, approvale, problemy, nieudane kroki, propozycje dyrygenta.
2. **Pilnuję** — śledzę na żywo, co robi agent.
3. **Prowadzę kolejkę** — kolejność realizacji, zajętość slotów.
4. **Analizuję po fakcie** — zakończone i nieudane zadania.

Skala: od kilku do kilkunastu zadań jednocześnie — lista mieści się bez
filtrów i grupowania.

## Decyzje

- **D-T1. Jeden backend.** TUI współpracuje z jednym backendem — tym, który
  wynika z bieżącej konfiguracji (lokalny daemon albo `--server` /
  `NARSOR_SERVER`). Nie ma kolumny backendu, zakładek backendów ani wyboru
  backendu domyślnego w TUI.
- **D-T2. Stabilna kolejność listy.** Lista nie jest grupowana ani sortowana
  według stanu. Stan to atrybut wiersza (ikona, kolor, etykieta). Wiersze
  zmieniają pozycję wyłącznie w wyniku jawnej akcji operatora.
- **D-T3. Kolejność listy = kolejność realizacji.** Lista jest ułożona według
  pozycji w kolejce schedulera. Zadania przyjęte do realizacji i zakończone
  zachowują swoje miejsce.
- **D-T4. Nie ma pojęcia priorytetu.** Operator zarządza kolejnością przez
  przesuwanie zadań. W control plane pole `priority` zastępuje jawna pozycja w
  kolejce (`rank`) i atomowa operacja `move` (przed / za zadaniem, na początek,
  na koniec). Scheduler przydziela sloty według tej pozycji.
- **D-T5. Zakończone zostają do archiwizacji.** Zakończone zadania zostają na
  liście do czasu `archive` (pojedynczo lub hurtem). Przełącznik ukrywa je jako
  filtr, bez zmiany względnej kolejności pozostałych. Zarchiwizowane trafiają
  do widoku Historia.
- **D-T6. Wąska lista, szerokie szczegóły.** Lista zajmuje ok. 28 kolumn,
  szczegóły resztę. Nie ma osobnego pełnoekranowego widoku zadania — prawy
  panel nim jest.
- **D-T7. Workflow deklaruje nagłówek.** Nowa sekcja `display` w YAML
  workflow określa tytuł zadania i linki (zgłoszenie, PR itd.).
- **D-T8. Odpowiadanie na dole.** Wszystkie rodzaje oczekiwania obsługuje jeden
  panel na dole szczegółów, spójnie z czatem, w którym pole odpowiedzi jest
  zawsze na końcu rozmowy.
- **D-T9. Wybór zawsze jako lista + opis.** Każdy wybór (akcje odpowiedzi,
  menu akcji, workflow w nowym zadaniu, pola wejść) to pionowa lista z osobnym
  miejscem na pełny opis i skutek wybranej pozycji.
- **D-T10. Jeden klawisz — jedno znaczenie.** Mapa klawiszy jest wspólna dla
  całego TUI. Znikają tryby `tui` i `tui --remote` z rozbieżnymi skrótami.
- **D-T11. Parytet CLI.** Każda akcja dostępna w TUI ma odpowiednik w CLI.

## 1. Ekran główny

```
┌ narsor · local ● live ──────────────────────────────── sloty 2/3 · ? 1 · ! 1 ┐
│ Zadania            │ NAR-12  Zmiana API zamówień             implement v3    │
│ ● NAR-14  edit     │ Jira NAR-12 ↗   PR !482 ↗   running · krok 3/5 · 14m    │
│?● NAR-12  approval │ Przebieg  Rozmowa  Zdarzenia  Dyrygent  Info           │
│!✗ NAR-9   failed   │ ✓ plan            │ ▸ Read src/api.rs                  │
│ ◷ NAR-16  #1       │ ✓ implement (2)   │ ▸ Edit src/api.rs  +12 −3          │
│ ◷ NAR-17  #2       │ ● review_api      │ assistant: Zmieniam sygnaturę…     │
│ ✓ NAR-8   done     │   test            │ ▸ cargo test  ✓ 42 passed          │
│                    │┌ ? Czeka na Ciebie: approval · 3m ────────────────────┐│
│                    ││ …panel odpowiedzi (sekcja 2)…                        ││
│                    │└──────────────────────────────────────────────────────┘│
├────────────────────┴────────────────────────────────────────────────────────┤
│ Enter szczegóły  Space następny czekający  . akcje  n nowe  ?  │ powiadom. │
└─────────────────────────────────────────────────────────────────────────────┘
```

### Nagłówek ekranu

Nazwa połączenia z konfiguracji, stan strumienia (`● live`, `◌ ponawiam (12s)`,
`✖ brak połączenia`), zajętość slotów backendu (`sloty 2/3`) oraz liczniki
`? pytania · ! problemy`.

### Lista zadań

Jeden wiersz na zadanie, w kolejności z D-T3.

| Kolumna | Zawartość |
|---|---|
| 1 — uwaga | `?` pytanie lub approval (żółty), `!` problem, nieudany krok, nieudany provisioning (czerwony), pusto |
| 2 — stan | `●` running, `◷` queued, `⟳` provisioning, `‖` paused / pausing, `✗` failed / interrupted / unknown, `✓` done, `↻` wysyłanie |
| 3 — tytuł | `display.title`, a gdy go brak — identyfikator zadania |
| 4 — etykieta | bieżący krok; `#n` dla zadania w kolejce; `offline` gdy kontroler nie ma dzierżawy |

Wiersz zaznaczony jest odwrócony. Ruch kursora na liście natychmiast zmienia
szczegóły.

### Szczegóły zadania

1. **Nagłówek (dwie linie).**
   - Linia 1: tytuł z `display.title`, opis zadania, workflow z wersją.
   - Linia 2: linki z `display.links`, lifecycle, krok `n/m`, czas trwania.
     Dla zadania w kolejce zamiast kroku: `#2 · czeka na slot (2/3)` albo
     `kontroler offline 4m`.
   - Link bez wartości jest pokazany jako `PR: —`. Linki są klikalne przez
     OSC 8 w terminalach, które to obsługują; niezależnie od tego `o` otwiera
     menu linków w przeglądarce.
2. **Zakładki.**
   - *Przebieg* (domyślna) — oś kroków (jeden wiersz na próbę akcji, jak
     `narsor steps`) i treść wybranego kroku. Dla zadania w trakcie podąża za
     bieżącym krokiem (`f` wyłącza i włącza).
   - *Rozmowa* — pełna rozmowa agenta, wpisy według rodzaju, długie zwinięte.
   - *Zdarzenia* — surowy log zdarzeń ze szczegółem zdarzenia.
   - *Dyrygent* — czat i historia decyzji, wybór harnessu dyrygenta.
   - *Info* — zapieczętowane wejścia, workflow, workspace, runner, kontroler.
3. **Panel „Czeka na Ciebie”** — na dole, widoczny tylko wtedy, gdy zadanie na
   coś czeka (sekcja 2).

### Fokus

Fokus jest na liście albo w szczegółach. `Enter` przenosi do szczegółów
(albo od razu do panelu odpowiedzi — sekcja 2), `Esc` wraca na listę z
zachowanym zaznaczeniem. W szczegółach `Tab` przełącza fokus między treścią
zakładki a panelem na dole.

### Wąski terminal

Poniżej 100 kolumn lista i szczegóły przełączają się pełnoekranowo
(`Enter` / `Esc`). Poniżej ok. 60×20 TUI pokazuje komunikat „powiększ okno”.

## 2. Panel „Czeka na Ciebie”

### Budowa

```
│┌ ? Pytanie · review_scope · 6m ──────────────────────────────── 1/1 ┐│
││ „Czy zakres zmian obejmuje też migrację danych?”                   ││
││ ◆ dyrygent: raczej revise — brak planu migracji (0.6)              ││
││ ┌ Akcje ─────────────────┐ ┌ Opis ─────────────────────────────────┐││
││ │   approved             │ │ ↩ wraca do agenta: plan               │││
││ │ ▸ revise  ↩          ◆ │ │ Zakres wymaga poprawy. Agent planu    │││
││ │   reject               │ │ wznowi swoją sesję z Twoim komenta-   │││
││ │                        │ │ rzem i przygotuje nowy plan.          │││
││ └────────────────────────┘ └───────────────────────────────────────┘││
││ Komentarz dla agenta (wymagany):                                   ││
││ > Uwzględnij migrację tabeli orders▏                               ││
││                         Tab pola · Enter wyślij · Alt+Enter ↵ · Esc││
│└────────────────────────────────────────────────────────────────────┘│
```

1. **Nagłówek panelu** — rodzaj (`?` żółty albo `!` czerwony), krok, czas
   oczekiwania. Gdy na zadanie czeka kilka rzeczy: `1/2`, a `[` / `]`
   przełącza między nimi.
2. **Kontekst** — pytanie, wynik agenta albo diagnoza; dłuższy przewija się w
   obrębie panelu.
3. **Rekomendacja dyrygenta `◆`** z pewnością — wyłącznie doradcza. Wartości
   zaproponowane przez dyrygenta są oznaczone `◆`, dopóki operator ich nie zmieni.
4. **Akcje** — pionowa lista, jedna linia na akcję: etykieta i znaczniki
   (`↩` wznawia agenta, `◆` rekomendacja, `!` nieodwracalna). Wybór `j`/`k`,
   `↑`/`↓` albo cyfrą `1`–`9`. Domyślnie zaznaczona akcja zalecana albo
   najbezpieczniejsza.
5. **Opis** — skutek wybranej akcji (dokąd prowadzi przejście, czy wznawia
   agenta, czy jest nieodwracalna) i pełny `description` z workflow, zawijany
   i przewijalny. W wąskim terminalu opis jest pod listą akcji.
6. **Pola** wybranej akcji — pod spodem; zmieniają się razem z akcją, a
   wpisane wartości zostają zachowane.

### Klawisze w panelu

- `Tab` — Akcje ↔ Pola.
- `Enter` — gdy akcja nie ma pól wymaganych: wysyła; gdy ma: przenosi do
  pierwszego pola; w polach: wysyła.
- `Alt+Enter` — nowa linia w polu tekstowym.
- `Esc` — zwija panel; szkic zostaje i przetrwa odświeżenie.

### Rodzaje oczekiwania

| Rodzaj | Kontekst | Akcje |
|---|---|---|
| **Pytanie** (bramka `human` / pytanie agenta) | treść pytania, podsumowanie dyrygenta | wyjścia stanu z opisami; wyjście oznaczone wznowieniem ma `↩ wraca do agenta <stan>` i wymagany komentarz |
| **Approval** | wynik agenta: etykieta, `data`, opis | Akceptuj · Zwróć agentowi (komentarz wymagany) · Nowa sesja (tylko gdy sesji nie da się wznowić) |
| **Problem** | odpowiedzi agenta, analiza dyrygenta | wyjścia stanu z opisami i polami `data`/`variables` · Napisz do agenta |
| **Nieudany krok** | diagnoza, pewność, uzasadnienie albo surowy błąd | każda propozycja dyrygenta (Potwierdź / Odrzuć, uzasadnienie w Opisie) · Restart kroku z notatką |
| **Propozycja dyrygenta** (z czatu) | co proponuje i dlaczego | Potwierdź · Odrzuć |
| **Nieudany provisioning** | błąd przygotowania workspace | Uruchom od nowa · Archiwizuj |

Cofnięcie decyzji dyrygenta odbywa się z historii w zakładce *Dyrygent*.

### Szybka ścieżka odpowiadania

- Na liście `Space` przenosi zaznaczenie do następnego zadania, które czeka
  (cyklicznie). Wiersze się nie przesuwają.
- Na liście `Enter` na zadaniu, które czeka, rozwija jego panel z fokusem na
  akcjach; na innym zadaniu przenosi fokus do szczegółów.
- Po wysłaniu fokus wraca na listę, powiadomienie mówi, co się stało
  (np. „NAR-12: zaakceptowano reviewed → test”).

### Błędy

- Odmowa serwera (np. 409 — zadanie przeszło dalej) jest pokazana w panelu;
  panel zostaje otwarty z tekstem operatora i odświeża kontekst.
- Gdy oczekiwanie zniknie (odpowiedź z CLI lub innego klienta), panel się
  zwija, a szkic zostaje dostępny do skopiowania.

## 3. Akcje operatora, nowe zadanie, Historia

### Menu akcji (`.`)

Ten sam schemat lista + opis co panel odpowiedzi. Każda pozycja ma też
bezpośredni skrót. Akcje niedostępne w bieżącym stanie są wyszarzone z
powodem (np. „pauza: zadanie jest w kolejce”); skrót do niedostępnej akcji
pokazuje ten sam powód w powiadomieniu.

| Klawisz | Akcja | Dostępna gdy |
|---|---|---|
| `K` / `J` | przesuń o jedno miejsce wyżej / niżej | queued |
| `m` | tryb przesuwania: `j`/`k` wybiera miejsce z podglądem, `Enter` zatwierdza (`move --before/--after`), `Esc` anuluje | queued |
| `p` | wstrzymaj / wznów (według stanu) | running, paused |
| `x` | przerwij bieżący krok `!` | running |
| `t` | przejście ręczne — lista wyjść stanu z opisami; override (przejście do dowolnego stanu) to osobna pozycja tej listy `!`, wymaga powodu i trafia do audytu | stan na to pozwala (override tylko przy `allow_manual_override`) |
| `s` | uruchom od nowa | interrupted, nieudany provisioning |
| `a` | archiwizuj `!` — usuwa workload i storage | zakończone, przerwane |
| `o` | otwórz link w przeglądarce | `display.links` ma wartość |

Akcje oznaczone `!` zawsze wymagają potwierdzenia w jednakowym oknie `y`/`n`,
które pokazuje skutek.

### Mapa klawiszy

| Kontekst | Klawisz | Działanie |
|---|---|---|
| wszędzie | `?` | pomoc kontekstowa generowana z mapy klawiszy |
| wszędzie | `q` | wyjście (z potwierdzeniem, gdy jest niewysłany szkic) |
| lista | `j`/`k`, `g`/`G` | ruch |
| lista | `Space` | następne zadanie, które czeka |
| lista | `Enter` | odpowiedz albo przejdź do szczegółów |
| lista | `.` | menu akcji |
| lista | `n` | nowe zadanie |
| lista | `z` | ukryj / pokaż zakończone |
| lista | `H` | Historia |
| lista | `R` | repozytoria |
| szczegóły | `←`/`→` | zakładki |
| szczegóły | `Tab` | treść ↔ panel na dole |
| szczegóły | `j`/`k`, `PgUp`/`PgDn`, `g`/`G` | przewijanie |
| szczegóły | `Enter`/`Space`, `v`, `Ctrl+v` | rozwiń wpis, widok surowy wpisu / całości |
| szczegóły | `f` | podążanie za bieżącym krokiem |
| szczegóły | `Esc` | powrót na listę |

Skróty akcji z tabeli menu działają zarówno na liście, jak i w szczegółach
(poza polami tekstowymi). Wybór harnessu dyrygenta jest w zakładce *Dyrygent*.

### Nowe zadanie (`n`)

Formularz zajmuje panel szczegółów; lista pozostaje nietknięta.

```
│ Zadania            │ Nowe zadanie → trafi na koniec kolejki (#3)         │
│ ● NAR-14  edit     │┌ Workflow ──────────────┐ ┌ Opis ──────────────────┐│
│?● NAR-12  approval ││ ▸ implement-feature    │ │ Pełny cykl: plan,      ││
│ ◷ NAR-16  #1       ││   fix-bug              │ │ implementacja, review, ││
│ ◷ NAR-17  #2       ││   open-mercato-sdlc    │ │ testy i PR.            ││
│                    ││   ścieżka…             │ │                        ││
│                    │└────────────────────────┘ └────────────────────────┘│
│                    │ Wejścia                                             │
│                    │  ticket_id   > NAR-18▏                              │
│                    │  repository  ‹ narsor ›                             │
```

1. **Workflow** — wbudowane, z katalogu i „ścieżka…”, z opisem wybranego.
   Ścieżka jest walidowana z pełną listą naruszeń, jak `narsor validate`.
2. **Wejścia** — pola zadeklarowane w workflow z typami jak dziś (wybór dla
   `boolean`, lista repozytoriów dla `repository`, parsowanie `integer`), opis
   bieżącego pola obok, błędy przy polach.
3. **Wysłanie** (`Enter`) — klient generuje id zadania przed pierwszą próbą.
   Odpowiedź przychodzi bez czekania na checkout. Na końcu kolejki pojawia się
   `◷ <tytuł> #n` i zostaje zaznaczone. Gdy odpowiedź się zgubi, TUI ponawia z
   tym samym id (wiersz `↻ wysyłanie…`); dopiero gdy ponowienie się nie uda,
   wiersz ma `✗ nie wysłano` z akcją „ponów”, a dane formularza zostają.
4. `Esc` przywraca szczegóły poprzednio zaznaczonego zadania; wpisane dane są
   zapamiętane do następnego `n`.

### Historia (`H`)

Ten sam układ: lista zarchiwizowanych zadań i executions sprzed schedulera
(od najnowszych) oraz szczegóły z tymi samymi zakładkami, tylko do odczytu.
Jedyna akcja: wznów / recover dla starego execution, które na to pozwala.
`Esc` wraca do listy zadań.

### Repozytoria (`R`)

Bez zmian funkcjonalnych względem dziś (lista, `a` dodaj, `d` usuń z
potwierdzeniem), w schemacie lista + opis.

### Powiadomienia

Dolny pasek: po lewej podpowiedzi klawiszy dla bieżącego fokusu, po prawej
ostatnie powiadomienie (zielone / żółte / czerwone), gasnące po kilku
sekundach. Zdarzenie na innym zadaniu zmienia znacznik w wierszu i licznik w
nagłówku oraz daje powiadomienie; lista się nie przestawia.

## 4. Połączenie, odświeżanie, przypadki brzegowe

### Połączenie

- Jeden tryb TUI, wyłącznie przez owner API `/control/v1`. `tui --remote`
  zostaje przejściowo jako ignorowany alias z ostrzeżeniem.
- Lokalny daemon jest uruchamiany automatycznie jak dziś. Gdy serwer jest
  nieosiągalny, TUI i tak się otwiera z komunikatem „łączę z … ◌”.
- Strumień zdarzeń zawsze wznawia się od kursora i zawsze ponawia połączenie.
- Bez połączenia dane są wyszarzone i oznaczone wiekiem („stan sprzed 40s”),
  akcje zmieniające stan są zablokowane z powodem; szkice i formularze zostają.

### Odświeżanie

- Zdarzenia `lifecycle`, `queue`, `move` i `input` łatają wiersz od razu.
- Pozostałe zdarzenia zlecają odświeżenie z opóźnieniem 400 ms.
- Dodatkowo okresowo co `tui_refresh_interval`.
- Odświeżenie nigdy nie zmienia zaznaczenia, fokusu, zakładki, kursorów,
  trybu podążania ani szkiców. Zaznaczenie trzyma się `task_id`.

### Przypadki brzegowe

| Sytuacja | Zachowanie |
|---|---|
| `move` odrzucony (inny klient, zadanie przyjęte do realizacji, zmieniona rewizja) | lista ładowana na nowo, powiadomienie z przyczyną |
| zaznaczone zadanie znika (archiwizacja z CLI) | zaznaczenie przechodzi na wiersz, który był pod nim; powiadomienie |
| oczekiwanie znika w trakcie pisania | panel się zwija, szkic dostępny do skopiowania |
| zadanie bez execution (queued, provisioning) | *Przebieg*, *Rozmowa*, *Zdarzenia* mówią „jeszcze nie wystartowało, #2 w kolejce”; *Info* pokazuje zapieczętowane wejścia |
| provisioning nieudany przed startem execution | `!✗` w wierszu, oczekiwanie „Nieudany provisioning” w panelu |
| mały terminal | układ pełnoekranowy poniżej 100 kolumn, komunikat poniżej ok. 60×20 |

## 5. Architektura

- Jeden `App` na modelu `TaskSummary` zastępuje lokalny `App` i `RemoteApp`.
  Bazą jest `src/tui/remote.rs` (lifecycle, kolejka, ponawianie strumienia,
  generacje); przenoszone są do niego funkcje execution z lokalnego `App`:
  oś kroków, rozmowa, czat dyrygenta, formularze, przejścia.
- Podział plików według odpowiedzialności: lista; szczegóły z zakładkami;
  komponent wyboru „lista + opis + pola” (panel odpowiedzi, menu akcji, nowe
  zadanie, repozytoria); mapa klawiszy jako jedno źródło dla obsługi,
  podpowiedzi i `?`; stan połączenia.

### Zależności spoza TUI (wymagania wstępne)

1. **Unified Task Scheduler** (`docs/plans/2026-10-07-1923-feat-unified-task-scheduler-plan.md`)
   — zadania lokalne jako taski control plane.
2. **Kolejka bez priorytetu** — `rank` i operacja `move` w control plane
   zamiast `priority`; CLI `narsor task move <task> --before <X> | --after <X> | --top | --bottom`.
   Wymaga zmiany planu schedulera: R3 (kolejność według pozycji, nie
   priorytetu + FIFO), R5 (brak priorytetu; nowe zadanie na końcu kolejki),
   KTD dotyczących priorytetu, oraz usunięcia `--priority` z `narsor task create`.
3. **Sekcja `display` w `WORKFLOW_SCHEMA.md`** z walidacją:
   ```yaml
   display:
     title: "{{ inputs.ticket_id }}"
     links:
       - label: Jira
         text: "{{ inputs.ticket_id }}"
         url: "https://jira.example/browse/{{ inputs.ticket_id }}"
       - label: PR
         text: "!{{ variables.pr_number }}"
         url: "{{ variables.pr_url }}"
   ```
   Szablony odwołują się do `inputs` i `variables`; nierozwiązana wartość daje
   pusty link (`—`), nie błąd. Schemat URL ograniczony do `http`/`https`.
4. **Parytet `/control/v1` z lokalnym API** — czat i decyzje dyrygenta,
   formularz nieudanego kroku, przejścia ręczne i override, przerwanie kroku,
   oś kroków i zdarzenia, odczyt starych executions dla Historii.

## 6. Testy

- **Stan (`tests/tui_state.rs`):**
  - stabilność kolejności przy strumieniu zdarzeń zmieniających stan;
  - zmiana kolejności wyłącznie przez `move`, w tym odrzucony `move`;
  - zachowanie zaznaczenia, fokusu, zakładki i szkicu przy odświeżeniu;
  - szybka ścieżka `Space` → `Enter` → odpowiedź → powrót na listę;
  - pełny cykl panelu odpowiedzi dla każdego rodzaju oczekiwania;
  - akcje niedostępne z powodem;
  - ponowienie tworzenia z tym samym id; `✗ nie wysłano` po nieudanym ponowieniu;
  - zachowanie bez połączenia i po jego odzyskaniu.
- **Render** (`TestBackend`, szeroki i wąski terminal): ekran główny, panel
  odpowiedzi z opisem, menu akcji, formularz nowego zadania, stan bez
  połączenia, zadanie w kolejce bez execution.
- **Spójność mapy klawiszy:** żaden klawisz nie ma dwóch znaczeń w jednym
  kontekście; pomoc `?` zgadza się z obsługą.

## Poza zakresem

- Zmiana backendu i jego konfiguracji z poziomu TUI.
- Wiele backendów lub serwerów naraz.
- Filtrowanie i wyszukiwanie na liście (przy skali kilkunastu zadań zbędne).
- Migracja zadań między backendami.
