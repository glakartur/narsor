---
artifact_contract: ce-unified-plan/v1
artifact_readiness: implementation-ready
product_contract_source: legacy-requirements
execution: code
title: Czytelność transkryptu TUI - Plan
date: 2026-10-13
created_at: 2026-10-13T20:38:00+02:00
type: fix
topic: tui-transcript-readability
origin: docs/brainstorms/2026-09-26-tui-podglad-zdarzen-i-rozmow-requirements.md
---

# Czytelność transkryptu TUI - Plan

## Goal Capsule

**Objective:** Operator szybko odczytuje przebieg transkryptu, a rozwinięcie wpisu pokazuje dodatkowy kontekst bez powtarzania podsumowania.

**Means:** Uporządkować podsumowania grup, podgląd tekstu oraz nagłówki i wcięcia treści wszystkich typów wpisów w istniejącym transkrypcie (KTD4).

**Authority:** Wymagania pełnego dostępu do treści i rozwijania długich wpisów z dokumentu źródłowego (R9–R10), zawężone do bieżących poprawek widoku.

**Stop conditions:** Liczniki bez zer, czytelne i nieredundantne rozwinięcie grup, maksymalnie pięć logicznych linii podglądu wypowiedzi, zachowany styl semantyczny treści oraz jednoznaczny znacznik stanu w nagłówku każdego wpisu.

**Execution profile:** Jednoprzebiegowa zmiana prezentacji transkryptu TUI w Rust; bez zmian danych ani procesu wydania.

**Tail ownership:** `ce-work` odpowiada za implementację i weryfikację; zasady repozytorium i preferencje użytkownika rozstrzygają sposób lądowania zmiany.

## Product Contract

### Summary

Plan poprawia zwinięte i rozwinięte wpisy w transkrypcie TUI. Podsumowania grup i dłuższe wypowiedzi mają pokazywać więcej użytecznego kontekstu, a rozwinięcie ma ujawniać tylko dodatkową treść. Każdy wpis ma osobny znacznik stanu w nagłówku i jednolicie wciętą treść, niezależnie od rodzaju komunikatu.

### Problem Frame

Podsumowanie hooków pokazuje liczniki o wartości zero, a rozwinięte wpisy są mało czytelne przez brak wcięć i powtarzane nagłówki. Rozwinięcie bywa kopią podsumowania. Długie wypowiedzi, szczególnie początkowy prompt workflow, dają za mało kontekstu w widoku zwiniętym. Gdy kolejne linie treści nie są wcięte, znacznik rozwinięcia wygląda jak część treści; podobny problem dotyczy wszystkich rodzajów wpisów, nie tylko wypowiedzi.

### Requirements

- R1. Podsumowanie grupy pokazuje wyłącznie niezerowe liczniki stanów.
- R2. Wiersze zdarzeń rozwiniętej grupy zawierają czas oraz opis, bez powtarzania na każdym wierszu roli i nazwy harnessa.
- R3. Wpis sygnalizuje możliwość rozwinięcia tylko wtedy, gdy ujawnia ono dodatkową treść; rozwinięcie nie powiela opisu już widocznego w podsumowaniu.
- R4. Zwinięta wypowiedź pokazuje do pięciu logicznych linii tekstu, w tym treść wejściową workflow; rozwinięcie dodaje tylko pozostałe linie, tak by pełna wypowiedź była widoczna raz.
- R5. Rozwinięty tekst wypowiedzi zachowuje prezentację jej autora lub rodzaju wpisu, a szczegóły techniczne zachowują styl techniczny.
- R6. Każdy wpis w czytelnym widoku transkryptu umieszcza znacznik stanu w nagłówku i wcięcie dwóch znaków przed każdą linią treści, zarówno po zwinięciu, jak i rozwinięciu. Zawinięte kontynuacje wyrównują się z pierwszym znakiem treści.

### Acceptance Examples

- AE1. Gdy wszystkie trzy hooki są zakończone, podsumowanie pokazuje `3 zakończone`, bez `0 w toku`; gdy dwa są w toku, pokazuje `2 w toku`, bez licznika zakończonych o wartości zero.
- AE2. Po rozwinięciu grupy hooków wiersze zawierają czas i opis zdarzenia, a `SYSTEM · Harness` nie powtarza się przy każdym zdarzeniu.
- AE3. Gdy wpis nie ma dodatkowych szczegółów poza swoim podsumowaniem, nie sygnalizuje możliwości rozwinięcia. Gdy istnieją szczegóły, rozwinięcie pokazuje je bez powtarzania podsumowania jako osobnego wiersza.
- AE4. Gdy wejściowy prompt workflow ma siedem logicznych linii, widok zwinięty pokazuje pierwsze pięć, a rozwinięcie dodaje dwie pozostałe; pełny wpis zawiera wszystkie siedem linii raz.
- AE5. Gdy rozwinięta wypowiedź należy do operatora, agenta lub systemu, jej treść używa tej samej prezentacji semantycznej co nagłówek wpisu; pola techniczne nadal używają stylu technicznego.
- AE6. **Covers R6.** Przy wąskiej szerokości terminala wcięcie nie zasłania początku treści ani znacznika zaznaczenia, a zawijanie pozostaje czytelne.
- AE7. **Covers R6.** Zwinięty rozwijalny wpis pokazuje `›` przed nagłówkiem, rozwinięty `⌄`, a wpis bez dodatkowej treści `·`. Zachowanie jest takie samo dla wiadomości operatora, agenta i dyrygenta, wejść workflow Narsora, thinking, wywołań i wyników narzędzi, wyników końcowych, wpisów systemowych, technicznych, błędów, nieznanych wpisów oraz podsumowań zdarzeń; żaden znacznik nie trafia do treści.

### Scope Boundaries

- Zmiana dotyczy prezentacji istniejących transkryptów; nie zmienia przechwytywania, trwałego zapisu, raw payloadów ani korelacji zdarzeń.
- Nie zmienia nawigacji, skrótów widoków ani ogólnej palety kolorów TUI.
- Pełna treść pozostaje dostępna po rozwinięciu i w istniejącym widoku RAW.

### Sources / Research

- `docs/brainstorms/2026-09-26-tui-podglad-zdarzen-i-rozmow-requirements.md` — R9 i R10 określają zachowanie pełnej treści oraz rozwijanie długich wpisów.
- `docs/plans/2026-10-11-1723-feat-tui-transcript-views-plan.md` — obowiązujący wzorzec grupowania, stylu kart i płaskiego rozwijania transkryptu.
- `src/tui/transcript.rs` — projekcja grup, liczniki, wiersze rozwinięcia i prezentacja logicznych linii.
- `src/tui/conversation.rs` — adapter wpisów rozmowy, podgląd promptów i semantyczne szczegóły.
- `src/tui/task_app/tabs/conversation.rs` — mapowanie prezentacji linii na style nagłówka i treści.
- `tests/tui_render.rs` — istniejące przypadki wąskiego transkryptu, zawijania i paska zaznaczenia.
- `tests/tui_conversation.rs` — testy projekcji wpisów z różnych źródeł i rodzajów harnessu.

## Planning Contract

### Key Technical Decisions

- KTD1. **Rozróżniać tekst wypowiedzi od szczegółów technicznych na granicy projekcji transkryptu.** Tekst należący do wypowiedzi dziedziczy jej prezentację semantyczną; pola diagnostyczne pozostają techniczne. Governs R5.
- KTD2. **Oceniać rozwijalność na podstawie dodatkowej treści, którą wnosi rozwinięcie.** Ten sam podgląd nie powinien tworzyć pozornego poziomu szczegółów. Governs R3.
- KTD3. **Limit pięciu linii dotyczy logicznych linii tekstu, przed zawijaniem do szerokości terminala.** Zachowuje przewidywalny limit podglądu niezależnie od szerokości. Governs R4.
- KTD4. **Nagłówek rozróżnia zwinięty, rozwinięty i nierozwijalny wpis symbolami `›`, `⌄` i `·`.** Symbole w jednej kolumnie odróżniają stan bez dokładania tekstu do treści. Governs R6.

**Product Contract preservation:** unchanged in this planning update.

### Implementation Constraints

- Zachować semantyczne style nagłówków i treści z `TranscriptPresentation` oraz istniejący pasek zaznaczenia.
- Nie usuwać ani nie modyfikować pełnych linii tekstu i danych źródłowych używanych przez rozwinięcie lub RAW.

### System-Wide Impact

Lokalne, zdalne i operatorskie transkrypty korzystają ze wspólnego `TranscriptState` i `render_transcript`; wspólna projekcja ma zapewnić identyczne położenie znaczników i wcięć niezależnie od adaptera źródłowego.

## Implementation Units

### U3. Ujednolicić nagłówki i treść wpisów

**Goal:** Znacznik rozwijania jest częścią nagłówka, a wcięta treść pozostaje od niego wizualnie oddzielona dla wszystkich wpisów.

**Requirements:** R6; Covers AE6–AE7.

**Dependencies:** None.

**Files:** `src/tui/transcript.rs`, `src/tui/task_app/tabs/conversation.rs`, `tests/tui_conversation.rs`, `tests/tui_render.rs`.

**Approach:**

1. Zastosuj znaczniki nagłówka zgodnie z KTD4 i R6 zamiast dopisywać je do treści.
2. W projekcji treści podglądu i szczegółów zachowaj wymagane przez R6 wcięcie także w zawiniętych wierszach.
3. Zachowaj semantyczne style nagłówka i treści oraz istniejący pasek zaznaczenia.

**Patterns to follow:** `TranscriptState::lines_for`, `TranscriptLine.header`, `wrap_visible_lines` i `transcript_row`.

**Test Scenarios:**

- Dla każdego rodzaju wpisu nagłówek pokazuje właściwy znacznik stanu, a żadna linia treści go nie zawiera.
- Po rozwinięciu wpisu zarówno wcześniejsze linie podglądu, jak i dodatkowe szczegóły są wcięte; wpis bez dodatkowych szczegółów pokazuje `·` i nie udaje rozwijalnego.
- Przy wąskiej szerokości zawinięte linie wyrównują się do początku tekstu, a znacznik zaznaczenia pozostaje widoczny.

**Verification:** Testy projekcji i renderowania potwierdzają położenie znacznika, wcięcie wszystkich linii i zachowanie zaznaczenia.

### U1. Uczytelnić grupy i rozwijanie

**Goal:** Podsumowania i rozwinięte grupy pokazują stan oraz dodatkowe szczegóły bez szumu i duplikatów.

**Requirements:** R1, R2, R3; Covers AE1–AE3.

**Dependencies:** U3.

**Files:** `src/tui/transcript.rs`, `tests/tui_render.rs`.

**Approach:**

1. Zmień składanie liczników i tekstu podsumowania zgodnie z R1.
2. W projekcji grup dodaj zwięzły wiersz czasu z opisem, bez powtarzanego nagłówka źródła.
3. Oprzyj znacznik rozwijania na różnicy między podsumowaniem i dostępnymi szczegółami; pomiń powielone wiersze w rozwinięciu.

**Patterns to follow:** `TranscriptState::projected_entries`, `lines_for` oraz istniejące testy grupowania w `src/tui/transcript.rs`.

**Test Scenarios:**

- Grupa zawierająca wyłącznie zakończone hooki pokazuje dodatni licznik zakończeń i nie pokazuje zera dla stanu w toku; przypadek odwrotny zachowuje się analogicznie.
- Rozwinięte hooki zawierają czas i opis, a ich wiersze nie powtarzają nagłówka `SYSTEM · Harness`.
- Wpis bez treści wykraczającej poza podsumowanie nie pokazuje znacznika możliwości rozwinięcia; wpis z dodatkowymi szczegółami rozwija się bez duplikowania wiersza podsumowania.

**Verification:** Testy projekcji i renderowania potwierdzają liczniki, różnicę między stanami zwiniętym i rozwiniętym oraz czytelność wąskiego widoku.

### U2. Pokazać dłuższe wypowiedzi w ich stylu

**Goal:** Zwinięty wpis daje wystarczający kontekst, a rozwinięta treść pozostaje spójna z rolą wypowiedzi.

**Requirements:** R4, R5; Covers AE4–AE5.

**Dependencies:** U1, U3.

**Files:** `src/tui/conversation.rs`, `src/tui/transcript.rs`, `src/tui/task_app/tabs/conversation.rs`, `tests/tui_conversation.rs`, `tests/tui_render.rs`.

**Approach:**

1. Zbuduj podgląd do pięciu logicznych linii dla tekstu rozmowy, także dla wejściowego prompta workflow.
2. Po rozwinięciu dodaj tylko linie spoza podglądu, zachowując pełną treść wpisu bez duplikatów.
3. Rozdziel style tekstu wypowiedzi i szczegółów technicznych.
4. Sprawdź, że krótkie wpisy nie otrzymują pustych ani zduplikowanych linii.

**Patterns to follow:** `agent_transcript_entry`, `semantic_detail_lines`, `TranscriptLine.presentation` oraz `transcript_row`.

**Test Scenarios:**

- Prompt workflow dłuższy niż pięć linii pokazuje pięć logicznych linii po zwinięciu; rozwinięcie dodaje resztę, bez powtarzania linii podglądu.
- Wypowiedzi krótsze niż pięć linii pokazują całą treść bez pustych linii ani powtórzeń.
- Rozwinięty tekst operatora, agenta i systemu zachowuje prezentację odpowiadającego mu nagłówka; szczegóły techniczne zachowują styl techniczny.

**Verification:** Testy adaptera, projekcji i bufora terminalowego potwierdzają limit podglądu, pełną treść po rozwinięciu oraz role stylów.

## Verification Contract

- `cargo fmt --check`
- `cargo test --lib tui::transcript::tests`
- `cargo test --lib tui::conversation::tests`
- `cargo test --test tui_conversation`
- `cargo test --test tui_render`

## Definition of Done

- Podsumowania grup pomijają zerowe liczniki, a rozwinięte elementy są opisane czasem oraz treścią.
- Rozwijanie ujawnia dodatkowe informacje bez powielania podsumowania; wpisy bez dodatkowych informacji nie sugerują rozwijania.
- Wypowiedź pokazuje do pięciu logicznych linii w stanie zwiniętym, a po rozwinięciu pełną treść w jej stylu semantycznym.
- Każdy rodzaj wpisu ma znacznik stanu w nagłówku i wciętą treść w obu stanach; znaczniki nie są renderowane jako tekst treści.
- Szczegóły techniczne pozostają techniczne, a pełne dane nadal są dostępne w istniejącej ścieżce RAW.
- Wszystkie scenariusze w Implementation Units mają testy, a porzucone eksperymentalne ścieżki nie pozostają w zmianie.
