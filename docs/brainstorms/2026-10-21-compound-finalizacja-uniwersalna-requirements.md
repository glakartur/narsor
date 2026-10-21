---
date: 2026-10-21
topic: compound-finalizacja-uniwersalna
---

# Workflow compound: finalizacja niezależna od hostingu repozytorium

## Summary

Krok `finalize` w `workflows/compound-engineering.yaml` ma działać co najmniej na GitLab i GitHub, a nie tylko na GitLab. Inne hostingi są opcjonalne (zgodnie z R2 i Scope Boundaries). Krok staje się krokiem agentowym. Agent rozpoznaje hosting i dostępne narzędzia, a potem wykonuje merge zgodnie z tymi samymi gwarancjami bezpieczeństwa co dziś.

---

## Problem Frame

Krok `finalize` jest skryptem bash. Zweryfikowane w `workflows/compound-engineering.yaml`:

- Skrypt akceptuje tylko adres w formie `…/-/merge_requests/N`. Inny adres kończy się wynikiem `blocked` z komunikatem „not a GitLab merge request URL".
- Skrypt wywołuje tylko `glab` (`mr view`, `mr update --ready`, `mr approve`, `mr merge --auto-merge`, `api …cancel_merge_when_pipeline_succeeds`).
- Krok `disarm_merge` ma tę samą zależność od `glab` i od formy adresu.

Użytkownik uruchomił workflow na repozytorium GitHub. Wcześniejsze etapy działają (agent otwiera PR przez `ce-commit-push-pr`), ale `finalize` zatrzymuje się na końcu z błędem o GitLab. Cała praca jest gotowa, a ostatni krok wymaga ręcznego merge.

Zależność od GitLab jest też w innych miejscach:

- Prompty publikacji (4 kroki) mówią: „This is a GitLab repository: the pull request is a merge request."
- Prompt `repair_merge` każe użyć `glab ci`.
- `workflows/README.md` i opis na początku YAML mówią, że workflow „targets GitLab" i wymaga `glab`.

---

## Actors

- A1. Użytkownik: uruchamia workflow na repozytorium na dowolnym hostingu i ocenia wynik w PR.
- A2. Agent finalizacji: rozpoznaje hosting i narzędzia, wykonuje merge albo zgłasza powód, dla którego nie może.

---

## Key Flows

- F1. Finalizacja na hostingu innym niż GitLab
  - **Trigger:** workflow dochodzi do `finalize` z `pr_url` wskazującym na GitHub (lub inny hosting).
  - **Actors:** A1, A2
  - **Steps:** Agent rozpoznaje hosting z `pr_url` i z remote repozytorium. Sprawdza, jakie narzędzie jest dostępne (np. `gh`, `glab`, `git`). Wyprowadza z PR numer, repozytorium i gałąź docelową. Wyjmuje PR z Draft, zatwierdza go, jeśli to możliwe, i ustawia automatyczny merge po zielonych sprawdzeniach. Czeka na wynik do limitu czasu.
  - **Outcome:** PR jest scalony (`merged`), albo agent zwraca jeden ze znanych wyników błędu, a PR wraca do Draft bez automatycznego merge.
  - **Covered by:** R1, R2, R3, R5, R6
- F2. Brak narzędzia lub uprawnień
  - **Trigger:** na miejscu uruchomienia nie ma właściwego narzędzia albo brak uprawnień.
  - **Actors:** A1, A2
  - **Steps:** Agent nie próbuje obejść braku. Zwraca `blocked` z konkretnym powodem (czego brakuje i jak to naprawić).
  - **Outcome:** Bramka `finalize_blocked` pokazuje użytkownikowi powód. Użytkownik naprawia środowisko i wybiera `retry`, albo kończy bez merge.
  - **Covered by:** R4, R7

---

## Requirements

**Uniwersalność kroku**

- R1. `finalize` jest krokiem agentowym. Agent sam rozpoznaje hosting repozytorium i dostępne narzędzia. Nie ma sztywnej listy hostingów w skrypcie.
- R2. Krok działa co najmniej na GitLab i GitHub. Na GitLab zachowanie jest takie jak dziś.
- R3. Krok nie zakłada formy adresu. Numer PR, repozytorium i gałąź docelowa pochodzą z `pr_url` i z samego PR, nie z nazwy gałęzi (jak dziś).
- R4. Gdy hosting jest nieznany lub brakuje narzędzia, agent zwraca `blocked` z powodem. Nie zgaduje i nie robi merge innymi środkami (np. lokalny `git merge` i `push` do gałęzi docelowej).

**Zachowane gwarancje bezpieczeństwa**

- R5. PR nigdy nie scala się bez człowieka. Każdy wynik inny niż `merged` anuluje automatyczny merge i przywraca Draft. Jeśli PR scalił się w międzyczasie, wynik to `merged`.
- R6. Zachowany jest zestaw wyników: `merged`, `pipeline_failed`, `conflict`, `blocked`, `timeout`, z tymi samymi przejściami (`repair_merge`, `finalize_blocked`, `disarm_merge`). Ponowne uruchomienie najpierw sprawdza, czy PR jest już scalony.
- R7. Powód każdego wyniku błędu jest podany wprost, tak aby bramka `finalize_blocked` i krok `repair_merge` mogły go pokazać.
- R8. Limit czasu oczekiwania na wynik pozostaje konfigurowalny. Obecny limit kroku (70 min) i oczekiwania (60 min) są punktem wyjścia.

**Spójność reszty workflow**

- R9. `disarm_merge` (sprzątanie po przerwaniu `finalize` przez limit czasu) też działa na każdym hostingu. Po przerwaniu agent nie ma szansy sprzątać, więc ten krok musi sam rozpoznać hosting.
- R10. Prompty, które dziś mówią „This is a GitLab repository" (kroki publikacji) i `glab ci` (`repair_merge`), nie zakładają GitLab. Mówią „pull request lub merge request, zależnie od hostingu".
- R11. `workflows/README.md` i opis w YAML nie twierdzą, że workflow „targets GitLab". Opisują, jakich narzędzi potrzebuje miejsce uruchomienia na każdym hostingu.

---

## Acceptance Examples

- AE1. **Covers R1, R2, R3.** Given repozytorium na GitHub i `pr_url` w formie `https://github.com/org/repo/pull/12`, when workflow dochodzi do `finalize`, then agent rozpoznaje GitHub, ustawia automatyczny merge i zwraca `merged` po zielonym pipeline, a nie błąd o GitLab.
- AE2. **Covers R2.** Given repozytorium na GitLab, when workflow dochodzi do `finalize`, then wynik i przejścia są takie same jak przed zmianą.
- AE3. **Covers R4, R7.** Given PR na GitHub i brak `gh` w miejscu uruchomienia, when `finalize` startuje, then wynik to `blocked` z powodem „brak `gh`", a nie próba merge innym sposobem.
- AE4. **Covers R5, R6.** Given PR, którego pipeline kończy się porażką, when `finalize` czeka na wynik, then agent anuluje automatyczny merge, przywraca Draft i zwraca `pipeline_failed`.
- AE5. **Covers R9.** Given `finalize` przerwany limitem czasu na GitHub, when uruchamia się `disarm_merge`, then krok anuluje automatyczny merge i przywraca Draft przez narzędzie właściwe dla GitHub.

---

## Success Criteria

- Użytkownik uruchamia workflow na repozytorium GitHub i dochodzi do `finished` bez ręcznego merge i bez błędu o GitLab.
- Na GitLab nic się nie psuje: te same wyniki i przejścia.
- Planista nie musi wymyślać zakresu: wiadomo, które kroki i teksty się zmieniają (`finalize`, `disarm_merge`, prompty publikacji i `repair_merge`, opisy) i jakie gwarancje zostają.
- Testy w `tests/workflows.rs` i `tests/builtin_workflows.rs` oraz fałszywy `glab` (`tests/fixtures/glab/fake_glab.sh`) są zaktualizowane do nowego kształtu kroku.

---

## Scope Boundaries

- Nie dodajemy obsługi hostingów, których nikt nie użył (np. Bitbucket, Gitea) jako wymaganego zakresu. Agent może je obsłużyć, jeśli narzędzie jest dostępne, ale nie jest to kryterium odbioru.
- Nie zmieniamy pozostałych etapów workflow (requirements, plan, implementacja, learning) poza tekstami z R10.
- Nie zmieniamy sposobu uwierzytelniania narzędzi (`glab`, `gh`). Pozostaje po stronie operatora miejsca uruchomienia.
- Nie zmieniamy obsługi `finalize_blocked`, `repair_merge` i terminali poza tekstami.

---

## Key Decisions

- Krok agentowy zamiast skryptu per hosting: agent dostosowuje się do hostingu i narzędzi bez utrzymywania osobnych skryptów. Koszt: mniej deterministyczne zachowanie niż skrypt.
- Gwarancje bezpieczeństwa (R5, R6) są wymaganiem produktu, nie szczegółem implementacji. Bez nich krok agentowy mógłby scalić PR bez człowieka.

---

## Założenia do potwierdzenia

- Założenie: „uniwersalny" znaczy co najmniej GitLab i GitHub. Inne hostingi są opcjonalne.
- Założenie: wybieramy krok agentowy (opcja, którą Artur sam zaproponował), a nie wykrywanie hostingu w skrypcie.
- Założenie: na GitHub „zatwierdzenie" PR może nie być możliwe, gdy ten sam użytkownik jest autorem PR. Wtedy brak zatwierdzenia nie blokuje, jeśli reguły repozytorium go nie wymagają. Do weryfikacji.
- Założenie: automatyczny merge na GitHub wymaga włączonej tej funkcji i reguł ochrony gałęzi w repozytorium. Gdy jej brak, wynik to `blocked` z powodem (nie merge od razu bez sprawdzeń).
- Założenie: obecne limity czasu (70 min / 60 min) zostają.
- Wymaga weryfikacji przez właściwego specjalistę: reguły ochrony gałęzi i uprawnienia konta w repozytorium klienta.

---

## Pytania otwarte dla użytkownika

- Czy agent finalizacji ma dostać ścisły kontrakt wyników (te same pięć etykiet), czy wystarczy `merged` / `blocked`? Zakładamy pięć etykiet (R6), bo `repair_merge` na nich polega.
- Czy na GitHub PR bez możliwości zatwierdzenia (autor = zatwierdzający) ma przechodzić do automatycznego merge, czy kończyć się `blocked`? Zakładamy: przechodzi, jeśli reguły nie wymagają zatwierdzenia.
- Czy akceptujesz mniejszą deterministyczność kroku (agent zamiast skryptu), w zamian za uniwersalność?

---

## Outstanding Questions

### Deferred to Planning

- [Affects R1][Technical] Jak ograniczyć agenta, aby R5 było pewne (np. narzędzia dozwolone w kroku, sprawdzenie stanu PR po kroku).
- [Affects R3][Needs research] Jak najpewniej rozpoznać hosting: z `pr_url`, z remote, czy z obu.
- [Affects R9][Technical] Czy `disarm_merge` też ma być agentowy, czy wystarczy mały skrypt z wykrywaniem hostingu.
- [Affects R2][Needs research] Odpowiedniki poleceń `glab` w `gh` (gotowość PR, zatwierdzenie, auto-merge, anulowanie auto-merge, odczyt stanu).
