---
artifact_contract: ce-unified-plan/v1
artifact_readiness: implementation-ready
product_contract_source: legacy-requirements
execution: code
created_at: 2026-10-11T17:23:54+02:00
type: feat
topic: tui-transcript-views
origin: docs/brainstorms/2026-09-26-tui-podglad-zdarzen-i-rozmow-requirements.md
---

# feat: Trzy widoki rozmowy agenta w TUI

## Goal Capsule

**Objective:** Operator może czytać rzeczywisty przebieg pracy agenta bez szumu technicznego, a równocześnie dojść do każdego zapisanego zdarzenia i jego oryginalnego JSON-a bez opuszczania rozmowy.

**Means:** Zbudować trzy projekcje jednej historii: Rozmowę z grupowaniem technicznych wpisów i cykli operacji, Chronologię bez grupowania oraz RAW. Harnessowe adaptery zwracają walidowane obiekty uniwersalnego kontraktu; zachowują dokładne rekordy źródłowe wyłącznie jako dane diagnostyczne (KTD1, KTD2, KTD5).

**Authority:** Wymagania zapisu i bezstratnej diagnostyki z dokumentu źródłowego oraz decyzje użytkownika z tej rozmowy: jednakowy układ kart w każdym widoku, trzy widoki, `v` do ich zmiany i `r` do źródła wybranego fragmentu.

**Stop conditions:** Serie technicznych wpisów nie zalewają Rozmowy kartami opisanymi wyłącznie jako `system`; cykle operacji są jedną kartą zakotwiczoną na początku; nierozstrzygnięte starty są oznaczone jako „w toku”; rozwinięcie pokazuje koniec (w tym wynik/błąd) bezpośrednio pod początkiem. Chronologia pokazuje źródłowe zdarzenia pojedynczo, bez milisekund. Każdy źródłowy JSON w RAW zaczyna się w osobnym wierszu i pozostaje dokładny.

---

## Product Contract

### Summary

Widok rozmowy zyskuje trzy sposoby czytania tych samych danych. Domyślna Rozmowa scala tylko rozpoznane, powiązane techniczne sekwencje w rozwijalne karty. Chronologia zachowuje ten sam renderer kart, role i nawigację, ale pokazuje każde zdarzenie osobno. RAW pokazuje źródłowe JSON-y w tej samej kolejności. Każda pozycja i grupa może zostać lokalnie zastąpiona swoimi JSON-ami.

### Problem Frame

Narsor zachowuje strumień harnessa bez utraty, lecz aktualna prezentacja jest płaską listą. Claude Code emituje w starcie sesji wiele wydarzeń `system`; adapter sprowadza je do podsumowania `system`, przez co techniczny, ale użyteczny kontekst zasłania rozmowę. Globalny RAW jest dziś jedyną drogą do kompletnej kolejności źródłowej, a lokalny RAW działa wyłącznie dla jednego płaskiego wpisu.

### Requirements

- R17. Zakładka rozmowy oraz rozmowa wybranego kroku oferują trzy widoki przełączane cyklicznie: **Rozmowa**, **Chronologia** i **RAW**.
- R18. Rozmowa jest domyślną, semantyczną projekcją z tym samym stylem kart, rolami, kursorem, rozwijaniem i zachowaniem przewijania co dziś. Może grupować tylko rozpoznane powiązane zdarzenia; grupa pokazuje zwięzły stan, liczbę zdarzeń oraz ostrzeżenie lub błąd, jeśli go zawiera.
- R19. Chronologia używa tego samego układu kart co Rozmowa, ale nie grupuje: każdy zapisany wpis jest osobną pozycją w kolejności zapisu, z czasem obserwacji z dokładnością do sekundy oraz czytelną projekcją typu i stanu zdarzenia.
- R20. RAW pokazuje wszystkie źródłowe wpisy chronologicznie jako terminalowo bezpieczny, bezstratny tekst ich zapisanych bajtów. Nie zastępuje układu rozmowy odrębnym ekranem logów.
- R21. `Enter` nadal rozwija lub zwija szczegóły zaznaczonej pozycji. `r` przełącza tylko zaznaczoną pozycję między projekcją bieżącego widoku a jej źródłowymi JSON-ami; dla grupy ujawnia wszystkie jej elementy w kolejności źródłowej. W RAW payload każdej karty jest już pełnym źródłowym JSON-em, więc `Enter` i `r` nie zmieniają reprezentacji i nie są pokazywane jako dostępne akcje; lokalny stan `r` zapamiętany poza RAW wraca po zmianie widoku. `v` przełącza globalny widok w kolejności Rozmowa → Chronologia → RAW → Rozmowa.
- R22. Grupowanie nie zmienia danych ani wykonania: każda pozycja grupy zachowuje własną tożsamość, czas, kolejność i raw. Sąsiadujące wpisy techniczne mogą być grupowane wg rodziny/typu (np. hooki). Start i koniec konkretnej operacji (hook, skill, tool) tworzą jedną kartę w miejscu startu; koniec jest do niego dołączony, a brak końca oznacza „w toku”. Nie wolno łączyć operacji bez jednoznacznej korelacji. Nieznane, uszkodzone lub niepowiązane wpisy pozostają samodzielne.
- R24. Wywołania narzędzi i skilli są osobnymi kartami per użycie i mają nazwę konkretnego narzędzia/skilla; koniec jest dopasowany po identyfikatorze operacji, jeśli harness go udostępnia. Podsumowanie pokazuje rezultat/błąd od razu, gdy jest dostępny; rozwinięcie pokazuje pojedynczą, płaską listę danych startu i końca bez wielopoziomowego rozwijania.
- R25. Rozwinięta grupa pokazuje rekordy RAW jako osobne, czytelne JSON-y: każdy rekord rozpoczyna się w nowej linii. Znacznik `>` oznacza widok zwinięty, `v` rozwinięty; stan dotyczy całej pozycji i nie wprowadza zagnieżdżonych poziomów.
- R26. Adapter harnessa mapuje wejście do zamkniętego, uniwersalnego kontraktu TUI, niezależnego od nazw i szczegółów danego harnessa. Adapter zwraca walidowany obiekt `Conversation`/jego typowane elementy, nie dowolne obiekty JSON. Kontrakt definiuje dozwolone typy i podtypy, wymagane pola oraz reguły korelacji; niedozwolone warianty nie mogą zostać utworzone.
- R27. Każdy typowany element ma opcjonalne pole raw zawierające dokładny rekord harnessa wyłącznie do analizy przez człowieka. Nieobsługiwany, ale poprawny składniowo rekord może być wyrażony jawnym wariantem `Unsupported/Raw`; naruszenie schematu rozpoznanego wariantu daje błąd walidacji/adaptera, nie cichy fallback do dowolnego typu.
- R28. Każdy widoczny opis ma być użyteczny: zakazane są generyczne etykiety typu `system` bez rozpoznanej nazwy/statusu. Dla nierozpoznanych zdarzeń opis ujawnia, że to nieobsługiwany typ, a RAW zachowuje szczegóły.
- R23. Te same reguły działają dla rozmowy kroku, rozmowy całego zadania oraz odczytów lokalnych i zdalnych. Odświeżenie, paginacja i zmiana widoku zachowują zaznaczenie lub kotwicę po identyfikatorze źródłowego wpisu, o ile nadal istnieje.

### Acceptance Examples

- AE8. Gdy Claude Code wypisze kilka `hook_started` i odpowiadające im `hook_response`, w Rozmowie operator widzi jedną rozwijalną kartę startu sesji z liczbą zdarzeń i widocznym błędem hooka; po Enterze widzi elementy semantyczne, a po `r` ich JSON-y w kolejności zapisu.
- AE9. Dla tego samego zapisu, w Chronologii operator widzi każdą kartę oddzielnie z jej czasem obserwacji i czytelnym opisem rozpoczęcia albo wyniku hooka, bez zmiany stylu kart względem Rozmowy.
- AE10. Po naciśnięciu `v` w Rozmowie operator przechodzi przez Rozmowę, Chronologię i RAW, a zaznaczenie pozostaje na tym samym zdarzeniu źródłowym lub najbliższym reprezentującym je elemencie.
- AE11. Gdy zapis jest odczytywany ponownie z task record albo strona rozmowy dopisuje nowe wpisy, wcześniej rozwinięta grupa i lokalny RAW nie ujawniają danych z innej próby ani nie tracą dostępu do swoich źródeł.
- AE12. Nieznany albo uszkodzony wpis harnessa pojawia się w Chronologii i RAW w swojej pozycji, a Rozmowa nie włącza go do niezweryfikowanej grupy.
- AE13. Sąsiadujące hooki są widoczne jako zwinięta karta `> Hooki · SessionStart · 1 zakończone · 1 w toku`; `v` otwiera płaską listę. Każdy element ma czytelny typ/status, a raw JSON każdego elementu zaczyna się w osobnym wierszu.
- AE14. Start toola/skilla i jego wynik są jedną kartą per wywołanie w miejscu startu. Jeśli wynik zawiera błąd, błąd widać bez rozwijania; przy braku końca karta mówi „w toku”. Rozwinięcie pokazuje szczegóły startu i wynik bez kolejnego poziomu rozwijania.
- AE15. Adapter zwracający nieznany typ poza `Unsupported/Raw`, nieznany podtyp rozpoznanego typu albo brak wymaganych pól nie może skonstruować poprawnej `Conversation`; adapter zachowuje nieobsługiwane źródło wyłącznie jako jawny raw wariant.
- AE16. Thinking-token zdarzenia z przykładowymi polami `estimated_tokens` i `estimated_tokens_delta` nie renderują jako `system`; mają sensowną techniczną etykietę i są grupowane z sąsiadującymi wpisami tej samej rodziny.

### Scope Boundaries

- Nie zmieniać formatu trwałego conversation record, API ani wykonania harnessów; zmiana dotyczy typowanego kontraktu adaptera/projekcji TUI.
- Nie dodawać filtrowania, wyszukiwania, automatycznego odświeżania ani nowych zakładek szczegółów.
- Nie rozszerzać nowej semantyki widoków na ogólny czytnik dyrygenta lub inne nie-agentowe czytniki; ich obecne skróty raw pozostają kompatybilne.

### Key Decisions

- **Trzy perspektywy korzystają z jednej historii źródłowej** (session-settled: user-directed — chosen over hiding technical records: techniczne zdarzenia mają pozostać użyteczne w Rozmowie). Governs R17–R23.
- **Chronologia zachowuje układ kart Rozmowy** (session-settled: user-directed — chosen over a separate log layout: różni się wyłącznie brakiem grupowania i precyzją czasu). Governs R19–R20.
- **`v` zmienia perspektywę, `r` odkrywa źródło tylko zaznaczonej pozycji** (session-settled: user-directed — chosen over keeping `v` as local raw: rozróżnia widok od diagnostyki pojedynczego fragmentu). Governs R21.
- **Każda pozycja ma najwyżej jeden poziom rozwinięcia** (session-settled: user-directed — grupa rozwija się do płaskiej listy, bez rozwijania elementów zagnieżdżonych). Governs R24–R25.
- **Adapter zwraca typowane obiekty zamkniętego kontraktu** (session-settled: user-directed — walidacja przy konstrukcji blokuje niepoprawne dane, a raw pozostaje polem diagnostycznym; nieobsługiwane rekordy mają jawny wariant). Governs R26–R28.

---

## Planning Contract

### Key Technical Decisions

- KTD1. **Przechować metadane zdarzenia przy projekcji, a nie odtwarzać je z tekstu kart.** `TranscriptEvent` już zawiera czas, kolejność, źródło i surowe bajty, lecz `agent_transcript_entry` zostawia tylko sformatowany nagłówek. Nowy model wejściowy transcriptu musi zachować te pola oraz stabilną tożsamość źródła, aby Chronologia miała dokładny czas, a grupa mogła wskazywać swoje elementy bez ponownego parsowania JSON-a. Governs R19, R22, R23.
- KTD2. **Warstwa widoku tworzy pozycje z jednego albo wielu źródeł.** Stan transcriptu zachowuje płaską, uporządkowaną kolekcję zdarzeń źródłowych; projekcja generuje identyfikowalne pozycje widoku dla aktualnego trybu. Grupa przechowuje wyłącznie referencje do elementów źródłowych i występuje tylko wtedy, gdy reguła korelacji jest jednoznaczna. Zaznaczenie przechowuje identyfikator źródła: wejście z Chronologii do grupy wybiera grupę zawierającą ten identyfikator, a powrót z grupy używa tego samego identyfikatora; bez wcześniejszego członka kanonicznym reprezentantem grupy jest jej pierwszy wpis. Governs R18, R21–R23.
- KTD3. **Normalizacja i projekcja są harness-neutralne; korelacja nie opiera się na widocznym tekście.** Adapter mapuje nazwy źródłowe do typowanych wariantów i wspólnych pól operacji (rodzina, nazwa, faza/status, correlation ID, wynik/błąd, timestamp); jeśli harness nie daje identyfikatora, stosuje się wyłącznie udokumentowaną, jednoznaczną regułę lokalną, w przeciwnym razie wpis pozostaje osobny/w toku. Grupowanie obejmuje różne rodziny zdarzeń technicznych, nie tylko Claude `SessionStart`. Governs R18, R22, R24, R28.
- KTD4. **Nowe skróty są ograniczone do transcriptu rozmowy agenta.** `v` i `r` obowiązują w kontekstach rozmowy kroku oraz rozmowy zadania; nie zmieniają znaczenia `v` i `Ctrl+V` w generycznych czytnikach wykorzystujących wspólny `TranscriptState`. Governs R21, R23.
- KTD5. **`Conversation` jest granicą walidacji adaptera.** Publiczne konstruktory/typy modelu dopuszczają tylko kontraktowe warianty i wymagane pola; adaptery nie mogą przekazać dowolnego `type/subtype` jako semantycznego zdarzenia. Rozpoznane, ale wadliwe dane zwracają błąd adaptacji z zachowanym źródłem diagnostycznym; poprawne, nieobsługiwane zdarzenia trafiają do jawnego `Unsupported/Raw`. Governs R26–R28.
- KTD6. **Raw nie jest kontraktem prezentacji.** Semantyczne pola są źródłem UI; raw jest opcjonalnym, niezmienianym payloadem źródłowym dla człowieka. Grupowanie UI nigdy nie syntetyzuje JSON-a ani nie nadpisuje pól adaptera. Governs R20, R22, R25, R27.
- KTD7. **Pary operacji kotwiczą się na starcie.** Koniec jest dołączany do karty początku, także gdy jego źródłowy rekord pojawił się później; komunikat końcowy/error jest widoczny pod opisem startu po rozwinięciu. W karcie można podać czas zakończenia w szczegółach, ale nagłówek chronologii używa dokładności do sekundy. Governs R19, R22, R24.

### High-Level Technical Design

```text
ConversationEntryDto (czas, źródło, surowe bajty)
                    │
                    ▼
        Harness adapter → Conversation (typowane, walidowane warianty)
                    │
                    ▼
       źródłowe elementy + raw diagnostyczny [stabilne ID]
          │                 │                  │
          ▼                 ▼                  ▼
      Rozmowa          Chronologia             RAW
   grupy/operacje       karta / wpis       rekord / linia JSON
          │                 │                  │
          └──── Enter / r działa na pozycji poza RAW ───┘
```

Tryb nie modyfikuje danych źródłowych. Zmiana widoku odbudowuje pozycje i przywraca kotwicę do identyfikatora źródła. Pojedyncze rozwinięcie ujawnia płaską listę elementów; elementy wyniku są bezpośrednio pod startem. Raw każdego rekordu rozpoczyna się w nowej linii. Nie ma zagnieżdżonych toggle.

### Research Notes

- `src/tui/transcript.rs` ma stabilne zaznaczenie, kotwice oraz stan rozwinięcia/raw, ale zakłada jeden `TranscriptEntry` na pozycję i tylko `Rendered`/`Raw`.
- `src/tui/conversation.rs` klasyfikuje wpisy Claude Code, Codex i OpenCode; jest właściwą granicą normalizacji `hook_*`, nie renderer.
- `src/tui/task_app/tabs/conversation.rs` używa wspólnego transcriptu zarówno dla rozmowy kroku, jak i rozmowy całego zadania; `src/tui/keymap.rs` wiąże obecnie `v` z lokalnym RAW i `Ctrl+V` z globalnym RAW.
- `docs/solutions/architecture-patterns/auto-resume-policy-for-lost-workloads-2026-10-11.md` wzmacnia zasadę jednej trwałej historii i wielu projekcji klienta.

#### Bieżące wejścia adapterów — stan rozpoznany w kodzie, nie pełna specyfikacja harnessów

| Harness | Rozpoznawane wejścia źródłowe | Uwagi kontraktowe |
| --- | --- | --- |
| Claude | top-level `assistant`, `user`, `system`, `result`; content `text`, `thinking`, `tool_use`, `tool_result`; system subtype m.in. `hook_started`, `hook_progress`, `hook_response`, `thinking_tokens`; obserwowany również `rate_limit_event` | Hook start/koniec ma `hook_id`; `hook_name`, `hook_event`, subtype, outcome/exit code oraz błędy trzeba jawnie mapować. `hook_progress` i `thinking_tokens` są techniczne, ale obecnie nie są poprawnie grupowane/opisywane. `rate_limit_event` musi zostać jawnie wsparty albo widoczny jako `Unsupported/Raw`, nie jako `system`. |
| Codex | `thread.started`, `turn.started`, `turn.completed`, `turn.failed`, `item.started`, `item.updated`, `item.completed`; item: `agent_message`, `reasoning`, `command_execution`, `mcp_tool_call`, `web_search`, `file_change`, `error` | Adapter powinien przenieść item/operation ID i status start/koniec do wspólnych pól, a nie polegać na tekście podsumowania. |
| OpenCode | `text`, `reasoning`, `tool_use`, `step_start`, `step_finish`, `error` | Nazwy providerowe normalizować do wspólnych wariantów; zachować ID narzędzia/cyklu, jeśli występuje w źródle. |

Wykaz jest punktem startowym do testów i jawnego kontraktu. U5 ma zweryfikować każdy faktycznie obsługiwany wariant w kodzie i dopisać wymagane pola; plan nie zakłada kompletności względem upstreamowych wersji harnessów.

---

## Implementation Units

### U1. Zachować źródłową tożsamość i czas transcriptu

**Goal:** Rozdzielić niezmienne wpisy źródłowe od ich pozycji prezentacyjnych, tak aby wszystkie widoki mogły korzystać z tej samej historii.

**Requirements:** R19, R20, R22, R23, R25.

**Dependencies:** None.

**Files:** `src/tui/transcript.rs`, `src/tui/conversation.rs`, `tests/tui_conversation.rs`, `tests/tui_state.rs`.

**Approach:**

1. Rozszerzyć model elementu źródłowego o dane potrzebne do chronologii i stabilnej korelacji: identyfikator, obserwowany czas, źródło, projekcję semantyczną oraz oryginalne bajty.
2. Zachować `TranscriptEvent` jako granicę klasyfikacji i przestać tracić jego strukturalny czas podczas przejścia do modelu TUI.
3. Przenieść zaznaczenie, rozwinięcie, lokalny RAW i przywracanie kotwicy na tożsamość źródła, z bezpiecznym zachowaniem dla separatorów prób, które nie mają danych harnessa. Rozwinięta lub lokalnie surowa grupa nie ujawnia automatycznie członków dopisanych przy późniejszym odświeżeniu.

**Patterns to follow:** Obecne `replace_entries`, `anchor`, `restore_after_change` i `raw_display` w `src/tui/transcript.rs`; bezstratne `raw` w `src/tui/conversation.rs`.

**Test scenarios:**

- Chronologiczna pozycja pokazuje czas z dokładnością do sekundy i zachowuje oryginalny raw wpisu.
- Zmiana szerokości, odświeżenie danych i dopisanie strony zachowują zaznaczenie źródłowego wpisu oraz pozycję przewijania.
- Separator kroku nadal jest czytelny i nie daje fałszywego lokalnego RAW.
- Nieprawidłowe UTF-8 albo znaki sterujące nadal są bezpiecznie prezentowane przez RAW bez modyfikacji pamiętanych bajtów.

**Verification:** Testy modelu transcriptu dowodzą, że informacje potrzebne do trzech projekcji nie są tracone podczas klasyfikacji ani odświeżania.

### U2. Normalizować cykle operacji i grupować zdarzenia techniczne

**Goal:** Zbudować czytelne karty operacji i grupy rodzin technicznych, bez gubienia lub duplikowania zdarzeń źródłowych.

**Requirements:** R18, R22, R24, R28; Covers AE8, AE12, AE13, AE14, AE16.

**Dependencies:** U1, U5.

**Files:** `src/tui/conversation.rs`, `src/tui/transcript.rs`, `tests/tui_conversation.rs`, `tests/tui_state.rs`.

**Approach:**

1. Normalizować zdarzenia hooków, thinking-token, tool i skill do opisowych typów/subtypów, nazw, faz, correlation ID, wyników/błędów oraz opcjonalnego czasu zakończenia; dodać reguły znane dla aktualnych wejść Claude, Codex i OpenCode.
2. Dopasować starty do końców per użycie po ID operacji, a gdy go brak — tylko przez bezpieczną, udokumentowaną regułę. Karta jest zakotwiczona na początku; osierocony start ma status „w toku”, a niepowiązany koniec nie jest dopasowywany zgadywaniem.
3. Grupować sąsiadujące wpisy techniczne według rodziny/typu. Hooki tworzą grupy z nazwą zdarzenia i licznikami zakończonych/w toku operacji (np. `Hooki · SessionStart · 1 zakończone · 1 w toku`); tool/skill pozostają osobnymi kartami per użycie i zawierają konkretną nazwę.
4. Pokazywać wynik/błąd bezpośrednio pod startem; wynik błędny eksponować już w skrócie karty. Szczegóły są płaskie: start, koniec/wynik i ich raw rekordy, bez zagnieżdżonych poziomów.
5. Zastąpić podsumowania `system` samodzielnymi, sensownymi etykietami wszystkich rozpoznanych zdarzeń; nierozpoznane jawnie nazywać nieobsługiwanymi.

**Patterns to follow:** `adapt_claude`, `project_claude_summary`, `semantic_detail_lines` i testowa zasada „raw remains lossless”.

**Test scenarios:**

- Sąsiadujące hooki SessionStart pokazują zwinięte liczniki `zakończone` i `w toku`; grupa innego hook-event również jest grupowana według typu.
- Starty toola/skilla mają oddzielne karty per użycie; koniec z właściwym ID trafia pod start, błąd jest widoczny od razu, a brak końca daje „w toku”.
- Końce docierające w innej kolejności łączą się z właściwym wywołaniem po ID; karty zachowują pozycję startu, a Chronologia i RAW kolejność źródłową.
- Brak bezpiecznego correlation ID, nieznany subtype lub osierocony koniec nie powoduje zgadywanego parowania; wynik pozostaje dostępny w Chronologii/RAW.
- Thinking-token rekordy mają opis zamiast `system`; kolejne sąsiadujące rekordy tej rodziny tworzą grupę z użytecznym podsumowaniem.
- Rozwinięcie pokazuje błąd/wynik bezpośrednio pod operacją i nie oferuje kolejnego poziomu rozwijania.

**Verification:** Rozmowa pokazuje rozpoznane zdarzenia techniczne jako opisowy przebieg, a każda karta grupy prowadzi do wszystkich oryginalnych źródeł.

### U5. Wprowadzić zamknięty, typowany kontrakt adapterów

**Goal:** Uniemożliwić adapterom przekazanie do TUI arbitralnych lub niekompletnych typów zdarzeń, zachowując raw dla diagnostyki człowieka.

**Requirements:** R26–R28; Covers AE15, AE16.

**Dependencies:** None.

**Files:** `src/tui/conversation.rs`, `src/tui/transcript.rs`, `tests/tui_conversation.rs`.

**Approach:**

1. Udokumentować macierz aktualnie rozpoznawanych wejść Claude, Codex i OpenCode: typ/subtyp źródłowy, wariant uniwersalny, wymagane pola, pola opcjonalne i korelacja. Nie traktować listy kształtów obserwowanych w kodzie jako pełnej specyfikacji zewnętrznych harnessów.
2. Zastąpić nieograniczone pola `type`/`subtype` i ogólne JSON `details` granicą konstrukcji typowanego `Conversation` oraz zamkniętymi enumami/strukturami dla uzgodnionych wariantów i wymaganych danych.
3. Adapter ma konstruować kontraktowe obiekty; walidacja odrzuca rozpoznane warianty bez wymaganych pól. Poprawne składniowo wejście spoza obsługiwanej macierzy zachowuje się jako jawny `Unsupported/Raw`, bez podawania losowej nazwy jako typu semantycznego.
4. Zachować dokładne bajty rekordu jako pole diagnostyczne (związane z konkretnym elementem), nie jako źródło UI ani zamiennik normalizacji.

**Patterns to follow:** Typowane modele domenowe i serde w `src/tui/conversation.rs`; granice adapterów istniejących harnessów.

**Test scenarios:**

- Każdy znany typ/subtyp Claude, Codex i OpenCode jest mapowany do właściwego wariantu uniwersalnego z wymaganymi polami.
- Nieznany typ trafia wyłącznie do `Unsupported/Raw`; nie przechodzi jako dowolna nazwa semantyczna.
- Rozpoznany typ bez wymaganego ID/nazwy/statusu lub z błędnym typem pola zwraca błąd walidacji zamiast poprawnego `Conversation`.
- Raw zachowuje oryginalne bajty niezależnie od projekcji i nie jest rekonstruowany z typowanego obiektu.
- Kontrakt nie dopuszcza niewspieranych wartości enum; dodanie nowego wariantu wymaga jawnej zmiany modelu i testów.

**Verification:** Testy kontraktu obejmują pozytywne warianty każdego adaptera, brakujące pola, wartości nieznane oraz bezstratność raw.

### U3. Wprowadzić trzy projekcje przy niezmienionym układzie kart

**Goal:** Udostępnić Rozmowę, Chronologię i RAW jako równoważne sposoby czytania jednego transcriptu.

**Requirements:** R17–R21, R23, R25; Covers AE9, AE10, AE12, AE13.

**Dependencies:** U1, U2, U5.

**Files:** `src/tui/transcript.rs`, `src/tui/task_app/tabs/conversation.rs`, `tests/tui_state.rs`, `tests/tui_render.rs`.

**Approach:**

1. Zastąpić dwustanowy `DisplayMode` trzema nazwanymi widokami i przekształcić płaskie wpisy na pozycje aktualnej projekcji.
2. Zachować wspólny renderer nagłówków, ról, zaznaczenia, zawijania i odstępów; Chronologia różni się tylko brakiem grupowania, a RAW pokazuje źródłowe rekordy.
3. Dostosować baner zakładki i podpowiedzi tak, by zawsze wskazywały aktualny widok i następne działanie, bez specjalnego ekranu logów.
4. Przy zmianie widoku mapować zaznaczenie i kotwicę po zapamiętanym identyfikatorze źródła: wejście do Rozmowy wybiera grupę zawierającą ten wpis, a wyjście z niej wraca do tego samego wpisu; nowo zaznaczona grupa kotwiczy pierwszy człon. Gdy reprezentacja zniknie po odświeżeniu, użyć istniejącej reguły najbliższej pozycji i nie rozwijać automatycznie nowych członków grupy.

**Patterns to follow:** `render_transcript`, `TranscriptPresentation`, wspólne `Conversation::rebuild` oraz testy ramek i zawijania w `tests/tui_render.rs`.

**Test scenarios:**

- `v` przechodzi przez trzy widoki i wraca do Rozmowy bez utraty zaznaczenia.
- Chronologia renderuje rozdzielne karty z tymi samymi rolami i stylem, co Rozmowa.
- RAW zachowuje kolejność i tekst wszystkich źródeł, w tym nieznanych wpisów.
- Znacznik `>` jest widoczny dla pozycji zwiniętej, a `v` dla rozwiniętej; toggle działa na jednym poziomie i nie ma interakcji zagnieżdżonej.
- W rozwiniętej grupie każdy obiekt JSON zaczyna się na oddzielnej linii, także gdy raw wejściowy zawiera znaki escapowane albo długie pola.
- Rozwinięcie grupy oraz zmiana widoku nie psują obserwowania końca rozmowy ani kotwicy starszego wpisu; dopisanie nowego członka nie otwiera go automatycznie.
- W RAW wszystkie payloady są już widoczne; `Enter` i `r` nie zmieniają karty, nie są eksponowane w podpowiedzi, a lokalny stan `r` wraca po opuszczeniu RAW.
- Rozmowa całego zadania zachowuje granice prób, a widok jednej próby nie pokazuje wpisów sąsiedniej próby.

**Verification:** Testy renderowania potwierdzają wspólny układ trzech widoków; testy stanu potwierdzają przełączanie, kotwice i zachowanie granic prób.

### U4. Ograniczyć nowe skróty do rozmów agenta

**Goal:** Zmienić sterowanie widokiem rozmowy bez regresji dla pozostałych czytników współdzielących transcript.

**Requirements:** R21, R23, R25; Covers AE10, AE11, AE13.

**Dependencies:** U3.

**Files:** `src/tui/keymap.rs`, `src/tui/task_app/mod.rs`, `src/tui/task_app/detail.rs`, `src/tui/task_app/tabs/conversation.rs`, `tests/tui_state.rs`, `tests/tui_render.rs`.

**Approach:**

1. Wprowadzić akcję przejścia do następnego widoku transcriptu rozmowy oraz odrębną akcję lokalnego źródła.
2. Przypisać `v` i `r` wyłącznie kontekstom rozmowy kroku i rozmowy zadania; zachować obecne wiązania genericznego czytnika, w tym dyrygenta.
3. Kierować Enter nadal do pojedynczego rozwinięcia aktualnej pozycji w Rozmowie i Chronologii; w RAW, gdzie pełny payload jest już widoczny, Enter i `r` są no-opami bez podpowiedzi. Rozwinięcie grupy pokazuje raw rekordy każdego elementu na osobnej linii.
4. Zaktualizować podpowiedzi skrótów i teksty pomocy razem z ich testami.

**Patterns to follow:** Deklaratywne `bind` w `src/tui/keymap.rs` oraz wspólne `transcript_action`.

**Test scenarios:**

- W obu kontekstach rozmowy `v` zmienia widok, `r` zmienia źródło zaznaczonej pozycji poza RAW, a Enter rozwija szczegóły poza RAW; w RAW oba klawisze pozostają bez działania i nie pojawiają się w podpowiedzi.
- Konteksty czytnika niebędącego rozmową zachowują wcześniejsze `v` i `Ctrl+V`.
- Podpowiedzi skrótów odpowiadają rzeczywiście obsłużonym akcjom.
- Po rozwinięciu i odświeżeniu transcriptu stan nie przecieka między próbami ani rozmowami.

**Verification:** Testy mapy klawiszy i testy przejścia przez `TaskApp` potwierdzają zakres nowych skrótów oraz brak regresji innych czytników.

---

## Verification Contract

- `cargo fmt --check`
- `cargo test --test tui_state`
- `cargo test --test tui_conversation`
- `cargo test --test tui_render`
- `cargo test --test cli_smoke` tylko jeśli zmienią się tekstowe podpowiedzi dostępne poza renderowanymi testami TUI.

Weryfikacja manualna: uruchomić zapis rozmowy z hookami, thinking-tokenami, wywołaniami skilla/narzędzia, błędnym wynikiem oraz operacją w toku; sprawdzić liczniki/statusy, jedno-poziomowe rozwinięcie, osobne wiersze JSON i wszystkie trzy widoki.

---

## Definition of Done

- Trzy widoki są dostępne w rozmowie kroku i rozmowie zadania, zachowując wspólny układ kart.
- Grupowanie jest deterministyczne, odwracalne do źródeł i nie obejmuje wpisów o niejednoznacznej korelacji.
- Adaptery zwracają wyłącznie typowane obiekty kontraktu; warianty znane są walidowane, a nieobsługiwane jawnie typowane jako `Unsupported/Raw`.
- Hooki mogą być grupowane po typie; tool i skill są widoczne per wywołanie z wynikiem/błędem lub stanem „w toku”.
- Rozwinięcie pozostaje płaskie, używa `>`/`v`, a raw JSON-y są odseparowane nowymi liniami.
- Każdy zapisany wpis pozostaje osiągalny wraz z czasem i surowymi bajtami.
- Nawigacja, rozwijanie, odświeżanie i granice prób mają ochronę regresyjną w testach.
- Testy z Verification Contract przechodzą, a w zmianie nie pozostają porzucone ścieżki poprzedniego modelu widoku.
