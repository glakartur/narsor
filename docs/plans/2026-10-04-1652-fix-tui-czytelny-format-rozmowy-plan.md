---
artifact_contract: ce-unified-plan/v1
artifact_readiness: implementation-ready
product_contract_source: legacy-requirements
execution: code
created_at: 2026-10-04T16:52:00+02:00
type: fix
topic: tui-czytelny-format-rozmowy
origin: docs/brainstorms/2026-09-26-tui-podglad-zdarzen-i-rozmow-requirements.md
---

# fix: Czytelny format rozmowy agenta w TUI

## Goal Capsule

**Objective:** Operator odczytuje przebieg rozmowy agenta bez rozpoznawania powtarzalnych, technicznych placeholderów, a pełny materiał diagnostyczny może otworzyć, kiedy jest potrzebny.

**Means:** Zastąpić domyślne zwijanie wszystkich wpisów technicznych semantyczną prezentacją ich treści, pozostawiając rozwinięcie i tryb raw jako ścieżki do szczegółów (KTD1).

**Authority:** Potwierdzony kierunek użytkownika — czytelność domyślna, szczegóły na żądanie — ma pierwszeństwo przed obecnym skracaniem diagnostyki.

**Stop conditions:** Widok rozmowy nie pokazuje już wyłącznie powtarzanego „<rodzaj> diagnostic” dla wpisów, które mają czytelną treść; żaden utrwalony bajt nie przestaje być dostępny w raw mode.

---

## Product Contract

### Summary

Plan poprawia lokalny i zdalny widok rozmowy agenta, aby wpisy systemowe, narzędziowe, thinking i wyniki miały użyteczną domyślną treść, zaś szczegóły techniczne pozostały rozwijalne.

### Problem Frame

Obecny adapter każdemu wpisowi technicznemu przydziela tę samą zwiniętą linijkę, np. „system diagnostic — e/Enter to expand”. W krótkiej rozmowie dominuje więc lista placeholderów zamiast informacji, które operator chce szybko ocenić. Jest to sprzeczne z intencją istniejącego widoku rozmowy: rozróżniać rodzaje wpisów wizualnie i udostępniać pełną treść bez utraty danych.

### Requirements

- R9. Domyślny widok pokazuje zrozumiałą projekcję każdego obsługiwanego wpisu harnessu w kolejności zapisu; zachowuje kompletne, surowe bajty do diagnostyki.
- R10. Wpisy techniczne są odróżnialne wizualnie, ale nie są bezwarunkowo zastępowane wspólnym placeholderem. Długie albo strukturalne dane nadal można rozwinąć, a przełączenie na raw mode nadal pokazuje bezpiecznie zdekodowane źródło.
- R16. **Nowe, potwierdzone w tej rozmowie rozszerzenie R9–R10:** zwięzłe zdarzenia życia sesji, wywołania narzędzi i ich wyniki mają własne czytelne podsumowania; brakujące lub nieznane pola degradują się do zachowanego tekstu, nie do utraty wpisu.

### Acceptance Examples

- AE1. Gdy rozmowa Codex zawiera rozpoczęcie wątku, zdarzenie narzędzia i jego ukończenie, operator widzi ich rodzaj oraz zwięzłą, różną treść bez rozwijania każdego wiersza.
- AE2. Gdy wpis ma długi wynik narzędzia albo niestandardową strukturę, widok domyślny zachowuje orientację, a `Enter`/`e` i raw mode udostępniają pełny zapis.
- AE3. Gdy harness emituje nieobsługiwany albo uszkodzony wpis, rozmowa nadal pokazuje go w kolejności jako czytelną reprezentację fallbackową.

### Scope Boundaries

- Nie zmieniać formatu zapisu rozmowy, endpointów API, kursora odświeżania ani nawigacji po tasku.
- Nie usuwać trybu raw, rozwijania wpisów ani rozróżnienia kolorystycznego ról.
- Nie projektować nowego parsera dla każdego możliwego przyszłego typu harnessu; nieznane zdarzenia pozostają bezpiecznym fallbackiem.

---

## Planning Contract

### Key Technical Decisions

- KTD1. **Semantyczna projekcja jest widoczna od razu, a szczegóły są progresywnym ujawnieniem.** Adapter ma dostarczać krótki, ludzki opis wpisu zamiast wspólnego „diagnostic”; rozwinięcie zachowuje pełniejszy opis, a raw mode pozostaje ostatecznym, bezstratnym źródłem. To realizuje potwierdzony kierunek użytkownika i R9–R10.
- KTD2. **Normalizować znane koperty na granicy klasyfikacji, nie w rendererze.** Reguły dla Claude Code, opencode i Codex pozostają w `conversation.rs`, dzięki czemu lokalny task view i zdalny widok — które współdzielą `agent_transcript_entry` — renderują tę samą semantykę.
- KTD3. **Fallback jest informacyjny i zachowawczy.** Jeśli znana koperta nie ma pola potrzebnego do opisu albo typ jest nieznany, użyć obecnej zdekodowanej treści zamiast wymyślać znaczenie; surowe bajty pozostają niezmienione.

### High-Level Technical Design

```text
utrwalony wpis (raw bytes)
          |
          v
  klasyfikacja + semantyczna projekcja
          |                         \
          v                          v
domyślny opis w transcriptcie   raw mode / rozwinięcie
  (krótki i czytelny)          (pełne dane diagnostyczne)
          |
          v
lokalny task view i zdalny widok
```

### Sources / Research

- `src/tui/conversation.rs` już klasyfikuje koperty trzech harnessów, ale dla systemu i wyniku narzędzia tworzy zbyt ubogie projekcje.
- `src/tui/app.rs` zawsze ukrywa treść wpisów technicznych pod wspólnym placeholderem; ta sama funkcja zasila lokalny i zdalny transcript.
- `src/tui/transcript.rs` utrzymuje tożsamość wpisu, rozwinięcie, anchoring oraz raw mode, więc zmiana może zachować istniejącą interakcję.
- `f6d415c` wprowadził role i odstępy transcriptu; poprawka ma rozszerzyć tę czytelność, a nie znieść rozróżnienie wizualne.
- R16 jest wymaganiem dodanym po legacy origin przez potwierdzony kierunek użytkownika: czytelność domyślna, detale na żądanie.

---

## Implementation Units

### U1. Wzbogacić semantyczną projekcję wpisów harnessu

**Goal:** Dostarczyć czytelny opis dla znanych zdarzeń systemowych, wywołań i wyników narzędzi, szczególnie kopert Codex widocznych jako powtarzalne wpisy diagnostyczne.

**Requirements:** R9, R10, R16.

**Dependencies:** None.

**Files:** `src/tui/conversation.rs`, `tests/tui_conversation.rs`.

**Approach:**

1. Rozszerzyć klasyfikację/projekcję o pola opisujące etap sesji, typ narzędzia, jego komendę lub dostępne podsumowanie wyniku, z jednoznacznym fallbackiem do zachowanego tekstu.
2. Zachować aktualną klasyfikację ról oraz surowe bajty `ClassifiedEntry`; nie przenosić parsingowych reguł do renderera.
3. Ustalić jedną zwięzłą reprezentację dla pustych lub wyłącznie technicznych zdarzeń, aby była rozróżnialna, lecz nie udawała danych, których koperta nie zawiera.

**Patterns to follow:** `project_stdout_line`, `classify_codex_item` i testy tabelaryczne klasyfikacji w `tests/tui_conversation.rs`.

**Test scenarios:**

- Codexowe zdarzenia rozpoczęcia/końca sesji otrzymują odmienne, czytelne projekcje zachowując kategorię systemową albo wynikową.
- Rozpoczęte i ukończone wywołanie narzędzia pokazuje nazwę/istotne dostępne podsumowanie, a klasyfikacja rozróżnia call od result.
- Brak oczekiwanego pola i nieznany typ nie powodują paniki ani utraty tekstu; fallback pozostaje dostępny.
- Dla każdego przypadku `raw` pozostaje bajtowo równy wejściu.

**Verification:** Testy klasyfikacji wykazują treść, rodzaj oraz zachowanie fallbacku dla Claude Code, opencode i Codex.

### U2. Wyświetlać domyślną treść techniczną z zachowaniem progresywnego ujawniania

**Goal:** Zastąpić w transcriptcie identyczne wiersze „diagnostic” treścią z U1 bez naruszenia raw mode, rozwijania, scrollowania i wspólnej prezentacji lokalnej oraz zdalnej.

**Requirements:** R9, R10, R16.

**Dependencies:** U1.

**Files:** `src/tui/app.rs`, `src/tui/transcript.rs`, `src/tui/views.rs`, `src/tui/remote.rs`, `src/tui/remote_views.rs`, `tests/tui_state.rs`.

**Approach:**

1. Ukształtować wpis transcriptu jako kompaktowy nagłówek roli/czasu plus dostępna semantyczna treść, zamiast bezwarunkowo zwijać całą klasę techniczną.
2. Zachować mechanizm identyfikatora wpisu, pustego odstępu, anchorów, `Enter`/`e` i `v`; dla długiej/strukturalnej treści podpowiedź rozwinięcia ma dotyczyć rzeczywistych dodatkowych danych.
3. Zweryfikować przepływ przez `agent_transcript_entry`, bo jest on wspólnym adapterem dla task view i remote view; dostosować renderer tylko tam, gdzie jest to potrzebne do czytelnej hierarchii, nie do ponownego parsowania kopert.

**Patterns to follow:** `TranscriptState::lines_for`, `wrap_visible_lines`, `transcript_style` oraz mechanizm odtwarzania anchorów po odświeżeniu.

**Test scenarios:**

- Krótki wpis systemowy i wynik narzędzia są widoczne w rendered mode bez rozwijania i mają odrębne, zachowane style techniczne.
- Wpis z dodatkowymi danymi można rozwinąć i ponownie zwinąć bez zmiany celu akcji po odświeżeniu lub zmianie szerokości pane.
- Raw mode nadal pokazuje terminal-safe reprezentację dokładnego źródła, niezależnie od skróconej projekcji.
- Lokalne oraz zdalne ładowanie tego samego zestawu wpisów korzysta ze wspólnej prezentacji i zachowuje kolejność oraz granice wpisów.

**Verification:** Testy stanu transcriptu potwierdzają domyślną widoczność, rozwijanie, raw mode i stabilny anchor; testy widoku potwierdzają, że role zachowują rozróżnienie kolorystyczne.

---

## Verification Contract

- `cargo fmt --check`
- `cargo test --test tui_conversation`
- `cargo test --test tui_state`
- `cargo clippy --all-targets -- -D warnings`

## Definition of Done

- U1 i U2 spełniają swoje scenariusze testowe oraz przechodzą wskazane bramki jakości.
- Rozmowa z przykładowymi zdarzeniami Codex nie jest listą powtarzających się placeholderów; operator może od razu odczytać sens zdarzeń.
- Pełny, terminal-safe zapis pozostaje dostępny w raw mode, a interakcje rozwijania, przewijania i odświeżania nie regresują.
- W diffie nie zostaje eksperymentalny parser ani martwy renderer, który nie uczestniczy w wspólnej ścieżce transcriptu.
