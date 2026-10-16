---
date: 2026-10-16
topic: zunifikowany-model-rozmowy-claude-code
---

# Zunifikowany model rozmowy i rozbudowany adapter Claude Code

## Summary

Narsor dostaje wspólny, niezależny od harnessu model elementów rozmowy. Adapter Claude Code emituje te elementy jako zdarzenia domenowe w trakcie przebiegu. Surowe linie zostają tylko logiem diagnostycznym. Konsumenci (TUI, API, CLI) dostają gotowe, sklasyfikowane elementy z czytelnym opisem: co przesłano do agenta, co agent myślał, co napisał, o co pytał, jakich narzędzi użył i z jakim wynikiem. Dostają też metadane przebiegu. Adapter Claude przekazuje kontrakt wyjścia jako schemat JSON, dlatego wynik i pytanie agenta przychodzą jako dane strukturalne.

---

## Problem Frame

Widok rozmowy kroku w TUI ma dużo linii technicznych bez treści. Przykład: wywołanie narzędzia `Skill` jest widoczne, ale bez nazwy skilla, czyli bez jedynej ważnej informacji. Operator nie widzi też jasnego podziału na to, co przesłano do agenta, co agent myślał, co napisał, o co pytał i jakich narzędzi użył.

Przyczyna jest w architekturze. Adapter Claude Code (`crates/narsor-engine/src/engine/actions/harness/claude_code.rs`) czyta z przebiegu tylko trzy rzeczy: tekst końcowy, id sesji i błąd. Reszta systemu dostaje surowe linie (D51). Każdy konsument rozpoznaje format sam. Dziś robi to tylko TUI (`src/tui/conversation.rs`) przez heurystykę na polu `type`. TUI rozpoznaje Claude Code i opencode, a linie codex prawdopodobnie trafiają do `Raw` (niesprawdzone). API i CLI nie mają żadnej klasyfikacji. Każdy nowy konsument musiałby znowu rozpoznawać format harnessu.

Kontrakt wyjścia ma podobny problem. Narsor dokleja do promptu opis oczekiwanego JSON-a (label i `data`) i potem wycina go z końcowego tekstu agenta. Gdy agent poda zły format, uruchamia się ratowanie labela. Dyrygent (`crates/narsor-engine/src/conductor/invoke.rs`) też sam wydobywa JSON z odpowiedzi i sprawdza go ze schematem. Claude Code umie wymusić strukturę odpowiedzi (`--json-schema` → `structured_output`), ale narsor tego nie używa.

Plik `docs/sources/claude-code-json.md` był punktem wyjścia, ale w kilku miejscach nie zgadza się z prawdziwym formatem `stream-json`. Zawiera nieistniejące typy linii `progress` i samodzielne `tool_use` z polami `tool`/`parameters`. Podaje pole `cost_usd` zamiast `total_cost_usd`. Opisuje narzędzie `AskFollowupQuestion`, którego Claude Code nie ma. Prawdziwe wywołania narzędzi są blokami w `assistant.message.content[]`, a wyniki narzędzi są blokami `tool_result` w liniach `user`. Te rozbieżności pochodzą z analizy kodu i wiedzy o CLI. Trzeba je potwierdzić na aktualnej wersji `claude`.

---

## Actors

- A1. Operator: czyta rozmowę kroku i dyrygenta w TUI albo CLI.
- A2. Agent kroku i dyrygent: sesje Claude Code uruchomione przez narsor.
- A3. Adapter harnessu: tłumaczy przebieg danego CLI na zdarzenia modelu.
- A4. Konsumenci modelu: TUI, CLI, API i ich przyszli klienci.

---

## Requirements

**Model elementów rozmowy**
- R1. Narsor ma jeden model elementów rozmowy, wspólny dla wszystkich harnessów. Model nie zawiera pojęć specyficznych dla jednego CLI.
- R2. Model rozróżnia co najmniej te kategorie treści:
  - przesłane do agenta (prompt kroku z kontraktem wyjścia, kolejne wiadomości narsora w tej samej sesji, np. dopytanie o label);
  - myślenie agenta;
  - tekst agenta;
  - pytanie agenta (R5);
  - wywołanie narzędzia;
  - wynik narzędzia;
  - wynik końcowy przebiegu, z danymi strukturalnymi, gdy są dostępne.
- R3. Komunikaty techniczne bez treści rozmowy mają własne kategorie, a nie jedną wspólną. Przykłady: inicjalizacja sesji, limity zapytań, błąd harnessu, stderr procesu. Ta praca ich nie ukrywa ani nie filtruje, tylko poprawnie je klasyfikuje.
- R4. Każdy element ma krótki, czytelny opis i pełną treść do rozwinięcia. Opis wywołania narzędzia zawiera nazwę narzędzia i jego kluczowy parametr. Przykłady: nazwa skilla dla `Skill`, polecenie dla `Bash`, ścieżka dla narzędzi plikowych, wzorzec dla wyszukiwania. Narzędzie bez znanej reguły dostaje opis z samej nazwy i pełne wejście w treści.
- R5. Kategoria „pytanie agenta” obejmuje dwa źródła: pytanie z kontraktu wyjścia narsora i wywołanie narzędzia pytającego (np. `AskUserQuestion`). Element zawiera informację, z którego źródła pochodzi.
- R6. Wynik narzędzia jest powiązany ze swoim wywołaniem i ma status: sukces albo błąd.
- R7. Przebieg ma metadane, które są dostępne razem z rozmową. Gdy harness je podaje, są to: model, wersja CLI, id sesji, koszt, użycie tokenów, liczba tur, czas trwania i przyczyna zakończenia (np. sukces, limit tur, błąd).
- R8. Linia, której adapter nie rozpoznaje (nowy typ, nowy blok treści, niepoprawny JSON), staje się elementem „nieklasyfikowane” z surową treścią. Taka linia nigdy nie powoduje błędu przebiegu.

**Adapter Claude Code**
- R9. Adapter Claude Code emituje elementy modelu w trakcie przebiegu, linia po linii. Operator widzi rozmowę na żywo tak samo jak dziś.
- R10. Adapter przekazuje do Claude Code kontrakt wyjścia (label i dane) jako schemat JSON. Dotyczy to kroków workflow i dyrygenta. Label, dane i pytanie agenta pochodzą z ustrukturyzowanego wyniku, a nie z parsowania tekstu.
- R11. Gdy Claude Code nie zwróci ustrukturyzowanego wyniku zgodnego ze schematem, działa obecne ratowanie labela. To ratowanie jest zapasem, a nie natychmiastowym błędem kroku.
- R12. Klasyfikacja Claude Code opiera się na zarejestrowanych przebiegach aktualnej wersji `claude`, a nie na `docs/sources/claude-code-json.md`. Te nagrania są dowodem formatu w testach.

**Konsumenci**
- R13. API zwraca elementy modelu i metadane przebiegu. Surowa linia jest nadal dostępna jako diagnostyka.
- R14. TUI i CLI pokazują rozmowę na podstawie modelu. Nie rozpoznają już formatu harnessu.

**Pozostałe harnessy i dane**
- R15. Opencode i codex emitują każdą linię jako element „nieklasyfikowane” z surową treścią, do czasu ich pełnej klasyfikacji w osobnych pracach.
- R16. Istniejące zapisy rozmów nie są migrowane. Można je usunąć, a nowy kod nie musi ich czytać.

---

## Acceptance Examples

- AE1. **Covers R4.** Agent wywołuje `Skill` z parametrem `skill: "ce-plan"`. Element ma kategorię „wywołanie narzędzia”, a jego opis zawiera `Skill` i `ce-plan`.
- AE2. **Covers R6.** Agent wywołuje `Bash`, a polecenie kończy się błędem. Wynik narzędzia jest powiązany z tym wywołaniem i ma status „błąd”.
- AE3. **Covers R5, R10.** Agent kończy krok labelem, który oznacza potrzebę wejścia, i pytaniem w danych. Rozmowa ma element „pytanie agenta” ze źródłem „kontrakt wyjścia”, a pytanie pochodzi z ustrukturyzowanego wyniku.
- AE4. **Covers R5.** Agent wywołuje `AskUserQuestion`. Rozmowa ma element „pytanie agenta” ze źródłem „narzędzie”.
- AE5. **Covers R11.** Claude Code kończy przebieg bez ustrukturyzowanego wyniku. Narsor uruchamia obecne ratowanie labela w tej samej sesji, a krok nie kończy się od razu błędem.
- AE6. **Covers R8.** Nowa wersja `claude` wysyła linię z nieznanym `type`. Rozmowa ma element „nieklasyfikowane” z tą linią, a przebieg trwa dalej.
- AE7. **Covers R3.** Strumień zawiera `system/init` i zdarzenie limitu zapytań. Każde z nich ma własną kategorię techniczną, a nie kategorię „tekst agenta” ani „nieklasyfikowane”.
- AE8. **Covers R15.** Krok na opencode wysyła linię tekstu agenta. Rozmowa pokazuje ją jako „nieklasyfikowane” z surową treścią.

---

## Success Criteria

- Operator czyta rozmowę kroku Claude i bez trybu raw wie, co przesłano do agenta, co agent myślał, co napisał, o co pytał, jakich narzędzi użył (z kluczowym parametrem) i jak skończyło się każde narzędzie.
- Żadna linia rozmowy Claude z zarejestrowanych przebiegów nie trafia do „nieklasyfikowane”.
- Konsument (TUI, CLI, API) nie zawiera kodu, który rozpoznaje format konkretnego harnessu.
- Krok Claude z poprawnym wynikiem nie potrzebuje parsowania tekstu, aby odczytać label.
- Planowanie nie musi wymyślać kategorii, zachowania dla nieznanych linii, zachowania opencode/codex ani zapasu przy braku ustrukturyzowanego wyniku.

---

## Scope Boundaries

- Brak decyzji o widoczności kategorii (ukrywanie, filtry, domyślny widok). To osobna praca po tej.
- Brak pełnej klasyfikacji i schematu wyjścia dla opencode i codex.
- Brak wejścia `stream-json` (rozmowa w jednym procesie zamiast `--resume`).
- Brak strumieniowania tekstu agenta znak po znaku (częściowe wiadomości).
- Brak migracji i obsługi istniejących zapisów rozmów.

---

## Key Decisions

- **Zdarzenia domenowe, a nie klasyfikacja przy odczycie.** Adapter emituje elementy modelu, a konsumenci nie widzą formatu harnessu. Koszt: poprawka klasyfikacji nie działa wstecz dla już zapisanych rozmów. To zmienia zasadę „surowa linia jest źródłem prawdy” z D51 i z `src/tui/conversation.rs`. Surowe linie zostają, ale jako log diagnostyczny. Ta zmiana wymaga nowego wpisu w `ARCHITECTURE_DECISIONS.md` (D60).
- **Najpierw klasyfikacja, potem widoczność.** Kategorie techniczne są drobne i liczne, aby późniejsze decyzje o widoczności miały dobre dane.
- **Model wspólny, wypełniony najpierw przez Claude Code.** Opencode i codex dostają tylko kategorię „nieklasyfikowane”. Opencode przez pewien czas wygląda gorzej niż dziś, bo TUI teraz częściowo go rozpoznaje. To jest świadomy koszt.
- **Kontrakt wyjścia jako schemat JSON w tej pracy.** Pytanie z kontraktu wyjścia jest jednym ze źródeł kategorii „pytanie agenta”. Dane strukturalne usuwają zgadywanie z tekstu. Ratowanie labela zostaje jako zapas.
- **Bez zgodności wstecz.** System nie działa produkcyjnie, więc stare zapisy można usunąć.

---

## Dependencies / Assumptions

- Zakładamy, że `--json-schema` działa razem z `--output-format stream-json`, `--verbose` i `--resume`, a `structured_output` jest w linii `result`. Tego nie sprawdzono na aktualnej wersji CLI.
- Zakładamy, że `AskUserQuestion` w trybie `--print` pojawia się w strumieniu jako wywołanie narzędzia (prawdopodobnie z wynikiem-błędem). Tego nie sprawdzono.

---

## Outstanding Questions

### Deferred to Planning

- [Affects R10, R11][Needs research] Czy `--json-schema` działa ze `stream-json` i `--resume`, oraz co Claude Code robi, gdy model nie spełni schematu (ponowienie, błąd, brak pola)?
- [Affects R9, R13][Technical] Gdzie i w jakiej formie zapisywać zdarzenia domenowe obok surowego logu, i jak stronicować je w API?
- [Affects R6][Technical] Jak powiązać wywołanie z wynikiem, gdy są na różnych stronach API?
- [Affects R4][Technical] Lista narzędzi z regułą kluczowego parametru na start (wbudowane narzędzia Claude Code i narzędzia MCP).
- [Affects R10][Technical] Jak opisać schemat dyrygenta i kroków, gdy dane kroku są dowolne (`data`)?
- [Affects R12][Needs research] Nagrać przebiegi aktualnej wersji `claude`: z narzędziami, myśleniem, błędem narzędzia, limitem tur, `AskUserQuestion` i schematem.
