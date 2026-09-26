---
date: 2026-09-26
topic: tui-podglad-zdarzen-i-rozmow
---

# TUI: pełna treść zdarzeń i widok rozmowy z agentem

## Summary

Każde zdarzenie w panelu Events da się otworzyć w przewijanym oknie z pełną treścią. Z każdego
zdarzenia powiązanego z rozmową agenta można przejść prosto do tej rozmowy. `Enter` na wykonaniu
otwiera pełnoekranowy widok tasku z osią kroków i pełną rozmową każdego kroku agenta. Narsor
zbiera tę rozmowę sam ze strumienia harnessu: przyrostowo, bez obcinania i w postaci
skompresowanej. Ręczne odświeżenie pokazuje bieżący stan także kroku, który wciąż trwa.

---

## Problem Frame

Panel Events (`src/tui/views.rs`) rysuje każde zdarzenie jako jedną linię i ucina ją na szerokości
panelu. Nie ma zawijania ani sposobu, żeby zobaczyć resztę. Najgorzej wypada `AgentMessage`: nie ma
własnego formatowania w `event_line()` (`src/tui/app.rs`), więc wpada w gałąź `Debug` i pokazuje
się jako surowy zrzut struktury, dodatkowo obcięty wcześniej przez `truncate` w executorze agenta.

Głębszy problem: narsor w ogóle nie rejestruje przebiegu rozmowy z agentem. Harness Claude Code
uruchamia się z `--print --output-format json` (`src/engine/actions/harness/claude_code.rs`), więc
zostaje tylko końcowa odpowiedź i `harness_session_id`. Opencode działa analogicznie
(`--format json`, parsowane jako jeden obiekt). Wywołania narzędzi, ich wyniki, kolejne tury i
przebieg nieudanej próby giną. Żeby zrozumieć, co agent zrobił, na przykład dlaczego nie zwrócił
JSON-a z `output`, dlaczego wybrał dany label albo czy utknął, operator dziś ręcznie uruchamia
`claude --resume <session-id>` i czyta rozmowę w harnessie albo kontynuuje ją interaktywnie. To
wychodzi poza narsor, działa tylko na maszynie, na której biegł agent, i przestaje działać przy
kierunku z `docs/brainstorms/2026-09-25-rozproszony-narsor-requirements.md`, gdzie TUI i worker
mogą być rozdzielone. Obserwacja na żywo i diagnoza po fakcie są dla operatora tak samo ważne.

---

## Actors

- A1. Operator: obsługuje TUI, obserwuje trwające taski i diagnozuje kroki, które poszły źle.
- A2. Harness agenta (Claude Code, opencode): proces prowadzący sesję z modelem. Jego strumień
  wyjścia jest źródłem zapisu rozmowy.
- A3. Silnik narsora: uruchamia kroki, emituje zdarzenia i jest odpowiedzialny za utrwalenie
  zapisu rozmowy kroku.

---

## Key Flows

- F1. Diagnoza nieudanego kroku agenta
  - **Trigger:** Operator widzi w Events `action_failed` albo `agent_session_failed`.
  - **Actors:** A1
  - **Steps:** Zaznacza zdarzenie i otwiera okno szczegółów z pełnym komunikatem błędu. Z tego
    okna (albo wprost z listy) jednym klawiszem wchodzi w rozmowę kroku. Widok tasku otwiera się
    ustawiony na tym kroku, konkretnie na tej próbie, i pokazuje całą rozmowę razem z wywołaniami
    narzędzi i ich wynikami.
  - **Outcome:** Operator rozumie przyczynę bez wychodzenia z narsora i bez `claude --resume`.
  - **Covered by:** R1, R2, R5, R6, R7, R9, R10

- F2. Podgląd trwającego kroku
  - **Trigger:** Krok agenta trwa podejrzanie długo.
  - **Actors:** A1, A2, A3
  - **Steps:** Operator wchodzi w task (`Enter` na wykonaniu) i wybiera bieżący krok. Widzi
    rozmowę zapisaną do tej chwili, a po ręcznym odświeżeniu dochodzą nowe tury.
  - **Outcome:** Operator ocenia, czy agent pracuje sensownie, bez czekania na koniec kroku.
  - **Covered by:** R5, R6, R8, R11, R12

- F3. Przegląd historii tasku po kroku
  - **Trigger:** Task ma za sobą retry i pętle (ten sam stan odwiedzony kilka razy).
  - **Actors:** A1
  - **Steps:** Operator wchodzi w task i przegląda oś kroków. Każda próba i każde ponowne wejście w
    stan to osobna pozycja z własnym statusem i własną rozmową.
  - **Outcome:** Operator widzi przebieg dokładnie taki, jaki był, bez sklejania prób.
  - **Covered by:** R5, R6, R7

---

## Requirements

**Pełna treść zdarzeń**
- R1. Każde zdarzenie w panelu Events da się otworzyć w oknie szczegółów, które pokazuje wszystkie
  jego pola w pełnej treści, z zawijaniem i przewijaniem. Wartości strukturalne (np. `inputs`,
  `payload`, `input`, `context`) są czytelnie sformatowane, a nie wypisane w jednej linii.
- R2. Każdy typ zdarzenia ma czytelną jednoliniową reprezentację na liście Events. Żaden typ, w tym
  zdarzenia agenta, nie pokazuje się jako zrzut `Debug`.
- R3. Okno szczegółów i widok rozmowy korzystają z tego samego zachowania przewijania i tych samych
  skrótów, więc operator uczy się tego raz.

**Powiązanie zdarzeń z rozmową**
- R4. Zdarzenia należące do kroku agenta (wejście w stan, start i koniec akcji, sesja, wiadomość,
  wyjście ze stanu, retry) niosą trwałe powiązanie z rozmową tego kroku, konkretnie tej próby.
- R5. Z poziomu zdarzenia, zarówno z listy Events, jak i z okna szczegółów, jednym klawiszem
  wchodzi się w widok tasku ustawiony na rozmowie powiązanej z tym zdarzeniem. Dla zdarzeń bez
  powiązania (np. start i koniec workflow, input człowieka, krok skryptu) klawisz jest nieaktywny, a
  interfejs mówi dlaczego.

**Widok tasku**
- R6. `Enter` na wykonaniu w liście otwiera pełnoekranowy widok tasku. Po lewej jest oś kroków w
  kolejności wykonania, każdy ze stanem, statusem i czasem trwania. Po prawej jest zawartość
  wybranego kroku: jego zdarzenia albo, dla kroku agenta, jego rozmowa. Przy wejściu bez
  powiązanego zdarzenia zaznaczony jest krok aktualnie trwający, a jeśli żaden nie trwa, ostatni
  krok na osi. `Esc` z widoku tasku zawsze wraca do miejsca, z którego operator wszedł (lista
  wykonań, lista Events albo okno szczegółów zdarzenia), z zachowanym zaznaczeniem. Pasek skrótów
  i mapa klawiszy pokazują skróty tego widoku.
- R7. Oś kroków pokazuje przebieg dokładnie taki, jaki był. Każda próba (retry) i każde ponowne
  wejście w ten sam stan to osobna pozycja z własną rozmową. Kroki przerwane i nieudane są
  widoczne z tym, co zdążyły zapisać. Krok kontynuujący wcześniejszą sesję harnessu pokazuje tylko
  swoje tury i wskazuje, którą sesję kontynuuje.
- R8. W widoku tasku ręczne odświeżenie wczytuje bieżący stan osi kroków i zapisu rozmowy, także
  kroku, który wciąż trwa. Widok nie odświeża się sam. Obowiązuje to niezależnie od tego, gdzie
  działa procesor wykonania, bo TUI czyta zapis rozmowy wyłącznie przez API.

**Zawartość rozmowy**
- R9. Rozmowa kroku pokazuje wszystko, co harness wyemitował, w kolejności: prompt wysłany przez
  narsor, wiadomości asystenta, bloki thinking (jeśli harness je emituje), wywołania narzędzi z
  argumentami, pełne wyniki narzędzi i końcowy wynik sesji. Nic nie jest obcinane.
- R10. Wpisy rozmowy są rozróżnialne wizualnie po rodzaju (użytkownik, asystent, thinking,
  wywołanie narzędzia, wynik narzędzia, wynik końcowy). Długie wpisy, zwłaszcza wyniki narzędzi,
  da się rozwinąć do pełnej treści bez utraty orientacji w rozmowie.
- R11. Kroki bez zapisu rozmowy, na przykład zapisane przed tą zmianą, pokazują jednoznaczną
  informację „brak zapisu rozmowy” zamiast pustego widoku.

**Przechwytywanie i zapis**
- R12. Narsor sam zbiera przebieg rozmowy ze strumieniowego wyjścia harnessu i utrwala go w
  danych wykonania, przyrostowo w trakcie sesji. Źródłem prawdy jest zapis narsora, nie pliki
  harnessu na maszynie workera.
- R13. Tryb strumieniowy jest domyślny dla każdego harnessu. Harness, który nie potrafi pracować
  strumieniowo, wymaga obejścia, które da ten sam efekt. Uboższy widok nie jest akceptowanym
  rozwiązaniem docelowym.
- R14. Przejście na tryb strumieniowy nie zmienia tego, jak krok agenta kończy się labelem:
  kontrakt `output`, identyfikator sesji harnessu i ścieżki błędów działają jak dziś. Strumień
  jest przetwarzany linia po linii, bez buforowania całego wyjścia. Label, identyfikator sesji i
  ścieżki błędów pochodzą z końcowego wyniku sesji niezależnie od rozmiaru rozmowy, a żaden limit
  bufora nie może obciąć tego wyniku.
- R15. Zapis rozmowy zakończonego kroku jest przechowywany w postaci skompresowanej. Jeśli to
  wykonalne, zapis trwającego kroku jest kompresowany już w trakcie zapisu. Podgląd obsługuje oba
  przypadki przezroczyście, także częściowo zapisany skompresowany zapis trwającego kroku.

---

## Acceptance Examples

- AE1. **Covers R1, R2.** Mając zdarzenie `workflow_started` z kilkunastoma inputami, gdy
  operator otworzy okno szczegółów, widzi wszystkie inputy w czytelnym, przewijanym formacie, a na
  liście Events zdarzenie zajmuje jedną czytelną linię.
- AE2. **Covers R4, R5.** Mając zaznaczone `action_failed` kroku agenta `plan` (druga próba), gdy
  operator naciśnie klawisz przejścia do rozmowy, otwiera się widok tasku z zaznaczonym krokiem
  `plan #2` i jego rozmową, a nie rozmową pierwszej próby.
- AE3. **Covers R5.** Mając zaznaczone `human_input_received`, gdy operator naciśnie klawisz
  przejścia do rozmowy, nic się nie otwiera, a pasek komunikatów mówi, że to zdarzenie nie należy
  do rozmowy agenta.
- AE4. **Covers R6, R8.** Mając krok agenta, który trwa od 5 minut, gdy operator wejdzie w task,
  ten krok jest zaznaczony, a operator widzi rozmowę do tej chwili. Po ręcznym odświeżeniu widzi
  też tury dopisane w międzyczasie.
- AE5. **Covers R7.** Mając krok, którego harness zginął w połowie sesji (timeout albo awaria),
  gdy operator otworzy ten krok, widzi wszystkie tury zapisane przed awarią i status nieudany, a
  nie pusty widok.
- AE6. **Covers R11.** Mając wykonanie uruchomione przed tą zmianą, gdy operator otworzy krok
  agenta, widzi „brak zapisu rozmowy”, a zdarzenia kroku nadal są dostępne.
- AE7. **Covers R7.** Mając krok agenta `plan`, który w pierwszej próbie nie zwrócił `output`, a w
  drugiej się udał, gdy operator wejdzie w task, oś pokazuje dwie pozycje: `plan #1` (nieudaną) i
  `plan #2` (udaną), każdą z własną rozmową.

---

## Success Criteria

- Operator diagnozuje nieudany albo podejrzany krok agenta w całości z poziomu TUI, bez
  `claude --resume` i bez sięgania do plików harnessu.
- Żadna treść zdarzenia ani rozmowy nie jest niedostępna przez obcięcie. Wszystko, co narsor
  zapisał, da się przeczytać w interfejsie.
- Działa tak samo dla Claude Code i opencode (albo opencode ma udokumentowane obejście spełniające
  R13).
- Planowanie nie musi wymyślać: co jest krokiem na osi (R7), co pokazuje rozmowa (R9), co się
  dzieje dla zdarzeń bez rozmowy (R5, R11) ani jak zachowuje się podgląd trwającego kroku (R8).

---

## Scope Boundaries

- Automatyczne odświeżanie widoku rozmowy na żywo: odłożone, na start wystarczają migawki na
  żądanie (R8).
- Otwieranie sesji harnessu interaktywnie z TUI (zawieszenie TUI i `claude --resume` w
  workspace): odłożone, to naturalne rozszerzenie widoku tasku.
- Limity rozmiaru zapisu rozmowy, retencja i sprzątanie starych zapisów: odłożone. Kompresja (R15)
  jest jedynym mechanizmem ograniczania rozmiaru w tej zmianie.
- Pełny, nieobcięty podgląd stdout i stderr kroków skryptowych: poza zakresem. Okno szczegółów (R1)
  pokazuje to, co jest w zdarzeniach, ale nie zmieniamy tego, co executor skryptu zapisuje.
- Wyszukiwanie i filtrowanie w rozmowie lub w Events: poza zakresem.
- Uzupełnianie zapisu rozmowy dla starych wykonań z transkryptów harnessu: poza zakresem (R11).

---

## Key Decisions

- Rozmowa jest osobno od Events: panel Events to historia całego tasku, a rozmowa należy do
  jednego kroku, czyli jednej sesji harnessu. Zalanie Events turami agenta zniszczyłoby czytelność
  osi tasku.
- Trwałe powiązanie zdarzenie ↔ rozmowa zamiast samego rozdzielenia: większość zdarzeń tasku
  dotyczy kroku agenta, więc przejście od zdarzenia do rozmowy to podstawowa ścieżka diagnozy (F1).
- Narsor zapisuje rozmowę sam, zamiast czytać transkrypt harnessu: działa przy zdalnym workerze,
  nie zależy od wewnętrznego formatu plików Claude'a, obejmuje każdy harness i spełnia obietnicę
  audytu z README. Kosztem jest miejsce na dysku, stąd kompresja.
- Tryb strumieniowy harnessu zamiast `--output-format json`: to jedyny wariant, który daje pełną
  rozmowę i jednocześnie migawki w trakcie kroku. `stream-json` to format wyjścia tego samego
  `claude --print`, więc działa na tym samym abonamencie i logowaniu. Wcześniejszy argument
  przeciw („narsor does not forward tokens anywhere yet”) przestaje obowiązywać.
- TUI czyta zdarzenia i zapis rozmowy wyłącznie przez API, niezależnie od tego, gdzie działa
  procesor wykonania. Dlatego podgląd trwającego kroku (R8) nie zależy od topologii, a zapis
  rozmowy trwającego kroku musi być dostępny przez API już w trakcie kroku.
- Zapis bez obcinania: diagnoza wymaga pełnych wyników narzędzi, a przycięty zapis zmuszałby do
  powrotu do harnessu.
- Wierność przebiegu ponad wygodę (R7): każda próba to osobny krok, kontynuacja sesji pokazuje
  tylko swoje tury. Operator ma widzieć, co się naprawdę wydarzyło, a nie wygładzony obraz.
- Widok tasku jako osobny pełnoekranowy tryb zamiast przedefiniowania trzech istniejących
  paneli: brak dwuznaczności co do poziomu, na którym jest operator, i miejsce na odłożone
  rozszerzenia.
- Okno szczegółów pod klawiszem zamiast zawijania w panelu: działa dla każdego typu zdarzenia,
  nie rozpycha listy i reużywa istniejącego wzorca modali.

---

## Dependencies / Assumptions

- `claude --print --output-format stream-json` (wymaga `--verbose`) emituje na bieżąco każdą
  wiadomość, wywołanie i wynik narzędzia, a końcowy obiekt `result` zawiera `session_id` i końcowy
  tekst. Sprawdzone w `claude --help` dla 2.1.283. Zawartość strumienia (np. bloki thinking) nie
  była weryfikowana w praktyce.
- Niezweryfikowane: czy `opencode run --format json` (v2.0.15) wypisuje zdarzenia przyrostowo, po
  jednym w linii. `--help` pokazuje tylko `default|json`, a obecny adapter zakłada jeden obiekt.
- Zależność od `docs/brainstorms/2026-09-25-rozproszony-narsor-requirements.md`: tamtejsza
  decyzja D-B replikuje stan do centrali dopiero na granicy kroku. Zapis rozmowy trwającego kroku
  jest od niej wyjątkiem: musi docierać do API w trakcie kroku (R8). Trzeba to uzgodnić w tamtym
  dokumencie.
- Zdarzenia zapisane przed zmianą nie mają powiązania z rozmową. Nie próbujemy go odtwarzać (R11).

---

## Outstanding Questions

### Deferred to Planning

- [Affects R13][Needs research] Czy opencode `--format json` strumieniuje zdarzenia, a jeśli nie,
  jakie obejście (np. inny tryb lub serwer opencode) daje równoważny zapis przyrostowy?
- [Affects R4][Technical] Jak powiązanie zdarzenia z rozmową jest reprezentowane w zdarzeniach i w
  zapisie, tak żeby przetrwało recovery i przejęcie wykonania (checkpoint/takeover)?
- [Affects R15][Technical] Który format kompresji pozwala czytać częściowo zapisany plik trwającego
  kroku, a jeśli żaden rozsądny nie pozwala, jak wygląda przejście z zapisu nieskompresowanego do
  skompresowanego po zakończeniu kroku?
- [Affects R7][Technical] Skąd widok tasku bierze oś kroków: czy da się ją zbudować z istniejących
  zdarzeń (`state_entered`, `action_started`, `action_retrying`), czy potrzebny jest jawny
  identyfikator kroku lub próby?
- [Affects R8, R12][Technical] Jak API udostępnia zapis rozmowy trwającego kroku, także gdy
  worker jest zdalny (wyjątek od replikacji na granicy kroku z D-B), bez konstrukcji do
  wyrzucenia przy migracji.
- [Affects R5, R6][Technical] Które klawisze: przejście do rozmowy, okno szczegółów w Events
  (`Enter` jest wolny w panelu Events, ale na liście wykonań otwiera widok tasku), odświeżanie w
  widoku tasku (spójnie z istniejącym `r`).
