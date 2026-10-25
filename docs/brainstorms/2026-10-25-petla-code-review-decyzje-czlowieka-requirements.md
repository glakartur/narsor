---
date: 2026-10-25
topic: petla-code-review-decyzje-czlowieka
---

# Pętla code review z decyzjami człowieka

## Summary

Krok `code_review` w trzech procesach compound kończy się także wtedy, gdy review zgłasza finding, o którym musi zdecydować człowiek. Człowiek dostaje każde pytanie jeden raz. Review nie zgłasza ponownie findingu, o którym człowiek już zdecydował. Źródłem szczegółów jest plan `docs/plans/2026-10-25-005-feat-petla-code-review-decyzje-czlowieka-plan.md`. Ten dokument zbiera z niego wymagania produktowe i granice zakresu.

---

## Problem Frame

Dziś `code_review` uruchamia skill `ce-code-review` z tokenem `mode:agent`. Skill w wersji 3.8.2 nie zna tego tokenu i działa interaktywnie. W trybie bez narzędzia do pytań model pyta zwykłym tekstem. CLI odpowiada komunikatem `[structured-output-enforce]`, a model wybiera `findings`. Stan `fix_findings` ma tylko wybór `fixed`, więc nie może zgłosić, że problem wymaga decyzji człowieka.

Następne review w nowej sesji zgłasza ten sam finding. Pętla trwa do limitu `max_entries: 10`. To daje do 9 zbędnych rund review i poprawek.

Druga luka dotyczy wszystkich kroków agenta. Kontrakt wyjścia opisuje `awaiting_input`, ale nie mówi, że w `claude-code` pytanie musi przejść przez narzędzie `StructuredOutput`. Pytanie zadane zwykłym tekstem nie dociera do człowieka.

---

## Actors

- A1. Człowiek (Artur lub osoba prowadząca zadanie): odpowiada na pytania o findingi, podejmuje decyzje `popraw` albo `zostaw`.
- A2. Agent review (`code_review`): dzieli findingi na do poprawy, do decyzji i tylko do raportu.
- A3. Agent autora implementacji (`work_revision`, w `-debug`: `fix_revision`): poprawia kod według uwag.
- A4. Silnik narsora: przechowuje decyzje i dostarcza kontrakt wyjścia agentowi.

---

## Key Flows

- F1. **Bez `-chat`, findingi i pytania**
  - **Trigger:** `code_review` zgłasza findingi do poprawy i do decyzji.
  - **Actors:** A1, A2, A3
  - **Steps:** Review wybiera `findings`. Autor poprawia kod. Review zgłasza już tylko findingi do decyzji i wybiera `questions`. Kod i pytania trafiają do PR jako komentarze inline. Człowiek odpowiada w wątku i wybiera `answered`. Odpowiedzi trafiają do pamięci decyzji. Gdy decyzja wymaga zmiany, autor poprawia kod. Review nie zgłasza ponownie findingu z decyzją i wybiera `clean`. Proces idzie do `verify_work`.
  - **Outcome:** każde pytanie padło raz, a kod przeszedł do weryfikacji.
  - **Covered by:** R3, R4, R5, R6, R8

- F2. **Bez `-chat`, wszystko odrzucone**
  - **Trigger:** człowiek odpowiada `zostaw` na wszystkie pytania.
  - **Actors:** A1, A2
  - **Steps:** `collect_answers` wybiera `all_declined`. Proces idzie prosto do `verify_work`, bez kolejnego review.
  - **Outcome:** brak zbędnej rundy review.
  - **Covered by:** R6, R7

- F3. **`-chat` i `-debug`, pytanie w sesji**
  - **Trigger:** po skillu zostają tylko findingi do decyzji.
  - **Actors:** A1, A2
  - **Steps:** Review zadaje pytania przez `awaiting_input`, po jednym na finding, z rekomendowaną opcją pierwszą. Ta sama sesja dopisuje decyzje. Review wybiera `findings` (są zmiany) albo `clean` (człowiek odrzucił wszystko).
  - **Outcome:** człowiek decyduje w oknie rozmowy.
  - **Covered by:** R8

- **Ścieżka wyjścia:** na bramce `answer_questions` wybór `abandon` kończy zadanie jako anulowane.

---

## Requirements

**Review i podział findingów**
- R1. `code_review` we wszystkich trzech workflow uruchamia `ce-code-review` z `mode:headless`.
- R2. W żadnym z trzech workflow nie ma stanu `fix_findings`.
- R3. `code_review` wybiera `findings`, gdy jest co najmniej jeden finding, który agent może poprawić bez decyzji człowieka. Findingi do poprawy zawsze idą przed pytaniami.
- R4. `code_review` nie zgłasza ponownie findingu z decyzją `zostaw`. Dla decyzji `popraw` sprawdza, czy kod ją realizuje. Gdy nie realizuje, zgłasza finding do poprawy, a nie pytanie. Dopasowanie jest po pliku i istocie problemu. Gdy nie ma pewności, finding jest nowy.
- R5. Podział findingów jest taki sam we wszystkich workflow. Tylko raport: `pre_existing`, `advisory`, `owner: release`. Pytanie: `owner: human`, `gated_auto`, każdy kompromis i finding bez pola `owner`. Do poprawy: `manual` z `owner: downstream-resolver`.

**Pytania w PR (bez `-chat`)**
- R6. Gdy zostały tylko findingi do decyzji, `code_review` wybiera `questions`. Kod i pytania trafiają do PR. Pytania są komentarzami inline przy liniach findingów. Gdy linii nie ma w diffie, pytanie jest komentarzem ogólnym. Proces czeka na bramce `answer_questions` z wyborami `answered` i `abandon`. Bramka nie ma `accept`.
- R7. `collect_answers` czyta odpowiedzi tylko z wątków utworzonych dla pytań i dopisuje decyzje do pamięci decyzji. Wybory: `changes_needed` (do autora), `all_declined` (do `verify_work`), `unanswered` (z powrotem do `answer_questions`, z powodem, której odpowiedzi brakuje). Wybór `unanswered` ma pierwszeństwo, gdy brakuje choć jednej odpowiedzi.

**Pytania w sesji (`-chat`, `-debug`)**
- R8. `code_review` pyta przez `awaiting_input`, po jednym findingu, z rekomendacją jako pierwszą opcją. Dopisuje decyzje do pamięci decyzji. Wybiera `findings` albo `clean`.

**Pamięć decyzji i autor implementacji**
- R9. Decyzje są w zmiennej `review_decisions` w narsorze, a nie w PR. Każdy wpis ma numer `D<n>`, plik, istotę problemu, decyzję i krótki powód. Krok, który dopisuje decyzje, kopiuje poprzednie wpisy bez zmian.
- R10. Każda zmiana kodu po implementacji idzie przez krok autora implementacji. Krok czyta uwagi z bramki i z review, a potem czyści obie zmienne. Krok `work` czyści `review_findings` i `review_decisions` przy powrocie po rewizji planu.

**Kontrakt wyjścia agenta**
- R11. W `claude-code` kontrakt mówi, że odpowiedź i pytanie `awaiting_input` przechodzą przez `StructuredOutput`. Inne harnessy nie dostają tej notki. Workflow nie wspomina `StructuredOutput`.
- R12. We wszystkich harnessach kontrakt mówi, że decyzja, której agent nie może podjąć sam, jest pytaniem do człowieka.

**Dokumentacja**
- R13. README workflow i diagram opisują nową pętlę. README ostrzega, że zadania w toku ze stanem `fix_findings` mogą się nie wznowić.

---

## Acceptance Examples

- AE1. **Covers R4.** Dane: człowiek odrzucił finding A. Gdy: startuje następny `code_review`. Wtedy: dostaje decyzję A i nie wybiera `questions` z powodu A.
- AE2. **Covers R3, R6.** Dane: review ma finding do poprawy i finding do decyzji. Wtedy: wybór to `findings`, nie `questions`.
- AE3. **Covers R7.** Gdy `collect_answers` wybiera `all_declined`, następny krok to `verify_work`, a nie `code_review`.
- AE4. **Covers R7.** Dane: jedna odpowiedź `popraw` i jeden brak odpowiedzi. Wtedy: wybór to `unanswered`.
- AE5. **Covers R11.** Prompt kroku w `claude-code` zawiera notkę o `StructuredOutput`. Prompt tego samego kroku w `codex` jej nie zawiera.

---

## Success Criteria

- Człowiek odpowiada na każde pytanie o finding najwyżej jeden raz. Pętla review nie dochodzi do limitu wejść z powodu tego samego findingu.
- Pytanie `awaiting_input` w `claude-code` dociera do człowieka. Krok nie kończy się etykietą po `[structured-output-enforce]`.
- `cargo test --workspace` jest zielony po każdej jednostce. Trzy workflow przechodzą walidator bez naruszeń.
- `grep` po `workflows/` nie znajduje `fix_findings` ani `mode:agent`.
- Planista nie musi wymyślać zachowania: podział findingów, wybory kroków i miejsce pytań są opisane.

---

## Scope Boundaries

- Zmiany dotyczą tylko `compound-engineering.yaml`, `compound-engineering-chat.yaml` i `compound-engineering-debug.yaml`. Inne workflow (np. `open-mercato-sdlc.yaml`) się nie zmieniają. Zmiana kontraktu wyjścia wpływa jednak na prompty wszystkich kroków agenta, także w innych workflow. To jest zamierzone.
- Silnik nie dostaje nowych mechanizmów grafu. Zmiana silnika to notka adaptera harnessu i jedno zdanie w kontrakcie.
- Dyrygent (`conductor/prompt.rs`) ma własny kontrakt odpowiedzi i się nie zmienia.
- Skille compound-engineering się nie zmieniają.
- Odroczone: sygnał diagnostyczny „pytanie poza kontraktem”, obniżenie `max_entries`, osobny wybór dla błędu skilla w trybie headless, rozwiązywanie wątków `[narsor:decyzja]`, sprawdzenie tokenu `mode:pipeline` w `ce-resolve-pr-feedback`.

---

## Key Decisions

- `mode:headless` zamiast interaktywnego review: skill nigdy nie pyta, a poprawki `safe_auto` stosuje sam.
- Findingi do poprawy przed pytaniami: człowiek decyduje raz, na kodzie bez drobnych problemów.
- Pamięć decyzji w narsorze: PR jest tylko interfejsem komunikacji.
- Wariant bez `-chat` pyta w PR, a `-chat` i `-debug` w sesji.
- Notka o `StructuredOutput` w adapterze `claude-code`, a nie w workflow.
- Autor implementacji to `work_revision` (w `-debug`: `fix_revision`), a nie `work`: `work` mierzy bazę testów od nowa i prowadzi do `simplify`, który ma działać raz.

---

## Dependencies / Assumptions

- Poniższe fakty pochodzą z planu i nie zostały ponownie sprawdzone w tej rundzie (zweryfikowano tylko, że `fix_findings` jest w każdym z trzech workflow): `ce-code-review` 3.8.2 w `mode:headless` zwraca pola `autofix_class` i `owner`. Zmienna, której krok nie definiuje, jest odkładana do `additional_data`. Czyszczenie zmiennej zależy od tego, czy model zwróci pole.
- Prawdziwy CLI `claude` przyjmuje `awaiting_input` przez `StructuredOutput` po dodaniu notki. Plan wymaga ręcznego testu przed U3 i U4. Gdy test nie przechodzi, praca się zatrzymuje.
- Po `awaiting_input` w `-chat` retry może zadać to samo pytanie drugi raz. Ryzyko jest akceptowane.

---

## Założenia do potwierdzenia

- Plan jest zatwierdzony przez Artura (Product authority), więc wybory z Key Decisions nie są ponownie otwierane. Ten dokument tylko zbiera wymagania z planu.
- Praca idzie w kolejności jednostek U1–U5 z planu, a każda kończy się zielonym `cargo test --workspace`.
- Gdy ręczny test z U1 na prawdziwym CLI `claude` nie przechodzi, implementacja zatrzymuje się po U2. U3 i U4 czekają na decyzję.
- Zmiana kontraktu wyjścia dla wszystkich harnessów i workflow (R12) jest akceptowana, także dla `open-mercato-sdlc.yaml`.
- Zadania w toku ze stanem `fix_findings` nie są migrowane. Tylko ostrzeżenie w README.

## Pytania otwarte dla użytkownika

- Brak pytań blokujących. Plan zapisuje `Open blockers: brak`.
- Czy zadania w toku ze stanem `fix_findings` mają dostać migrację, czy wystarczy ostrzeżenie w README? Domyślnie: tylko ostrzeżenie.

---

## Outstanding Questions

### Resolve Before Planning

- Brak.

### Deferred to Planning

- [Affects R6][Technical] Jak publikować komentarz inline osobno dla GitHub i GitLab, i jak rozpoznać wątek odpowiedzi.
- [Affects R11][Needs research] Czy notka o `StructuredOutput` wystarcza, aby model w `claude-code` nie pytał zwykłym tekstem.
