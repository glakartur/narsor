---
date: 2026-11-11
topic: krok-profile-zamiast-profilowania-przez-dyrygenta
---

# Krok `profile` zamiast profilowania przez dyrygenta

## Summary

Model i effort kroków wybiera dedykowany krok workflow typu `profile`. Autor workflow umieszcza go, gdzie chce, i wskazuje w prompcie źródło oceny złożoności. Wynik trafia do pliku w przestrzeni runnera, który runner czyta przy starcie każdego kroku. Zmiana zastępuje profilowanie przez dyrygenta po kroku planu z #92. Najpierw powstaje refaktoryzacja wspólnego kodu akcji, potem sama funkcja.

---

## Problem Frame

Obecnie profilowanie startuje po udanym kroku planu z flagą `plan: true`. Silnik wysyła żądanie `ProfileSteps` do dyrygenta (`crates/narsor-engine/src/engine/runner.rs`, `profile_after_plan`). Dyrygent wybiera tier modelu i effort dla kroków, które jeszcze nie startowały.

Trzy problemy:

- **Mechanizm jest ukryty.** Wyzwala go flaga `plan: true` i zmienna `complexity`. Autor workflow nie widzi, kiedy i na jakiej podstawie wartości są wybierane.
- **Profil powstaje raz.** Kroki, które już startowały, są pomijane (`state_entry_count > 0`). Workflow z pętlą wraca do tych kroków i nie może zmienić ich profilu.
- **Jest zależność od dyrygenta.** Bez dyrygenta profil nie powstaje. Ocena `complexity` z kroku planu jest pośrednikiem, bo dyrygent i tak czyta plan z artefaktów. Ocena jest zresztą opcjonalna.

---

## Actors

- A1. Autor workflow: dodaje krok `profile`, pisze prompt ze źródłem oceny, ustawia `model`, `effort` i `ceiling` kroków.
- A2. Agent profilujący: dostaje dane od silnika, ocenia złożoność i proponuje wartości dla kroków.
- A3. Runner: czyta wynik profilowania i przy starcie kroku wybiera model oraz effort.
- A4. Operator: widzi w „Rozmowie” i „Przebiegu”, co agent zaproponował i co zostało obcięte.

---

## Key Flows

- F1. Profilowanie kroków
  - **Trigger:** Workflow dochodzi do kroku `profile`.
  - **Actors:** A2, A3
  - **Steps:** Silnik buduje listę kroków (bez przypiętych). Dodaje do promptu autora opisy kroków, tiery, efforty i wartości domyślne. Agent zwraca wpis na krok. Silnik waliduje każdą wartość osobno i obcina do `ceiling`. Wynik trafia do pliku runnera.
  - **Outcome:** Plik zawiera ostateczne wartości dla kroków, które agent ocenił z pewnością. Rozmowa pokazuje wpisy kroku.
  - **Covered by:** R3, R4, R5, R6, R7, R8, R9, R11

- F2. Start kroku po profilowaniu
  - **Trigger:** Runner zaczyna dowolny krok agenta.
  - **Actors:** A3
  - **Steps:** Runner szuka wpisu kroku w pliku wyniku. Brak wpisu oznacza wartości z kroku, a gdy ich brak, z `globals`. Na wynik działa eskalacja po awariach.
  - **Outcome:** Krok startuje z modelem i effortem zgodnymi z tą kolejnością.
  - **Covered by:** R9, R10

- F3. Powrót do wcześniejszych kroków
  - **Trigger:** Workflow wraca przez przejście do kroku `profile` lub do kroków, które już były.
  - **Actors:** A2, A3
  - **Steps:** Krok `profile` uruchamia się ponownie i zapisuje nowy wynik. Licznik awarii kroków nie jest zerowany.
  - **Outcome:** Krok, który wcześniej zawodził, startuje na nowym profilu z eskalacją liczoną od wszystkich dotychczasowych awarii.
  - **Covered by:** R8, R12

---

## Requirements

**Krok `profile` w workflow**
- R1. Workflow może zawierać akcję typu `profile` (klucz `profile` w YAML). Autor umieszcza ją w dowolnym miejscu workflow.
- R2. Prompt autora wskazuje źródło oceny złożoności. Silnik dokłada do niego dane: listę kroków z opisami, dozwolone tiery i efforty, wartości domyślne. Autor ich nie wpisuje.
- R3. Z listy dla agenta wypadają kroki z własnym `model` i `effort` w `action`. Kroki z `ceiling` zostają na liście, nawet jeśli mają własny model i effort. Wartości z `globals.default_*` nie liczą się jako własne.
- R4. Agent zwraca wpis na krok: `model`, `effort`, `rationale`, `confident`. Wpis z `confident: false` jest pomijany.
- R5. Profil dotyczy kroków, a nie jednostek planu. Pole `unit` znika z odpowiedzi agenta, magazynu i widoku.

**Walidacja i wynik**
- R6. Silnik ocenia model i effort osobno. Wartość spoza tierów lub skali zostaje domyślna, a druga wartość z wpisu nadal działa. Wpis bez zmian względem domyślnych jest pomijany.
- R7. Wartość powyżej `ceiling` jest obcinana do `ceiling`. Informacja, o co poproszono i co obcięto, trafia do treści kroku profilującego w „Rozmowie” i „Przebiegu”. Osobny widok profilu w TUI nie jest potrzebny.
- R8. Krok `profile` zapisuje ostateczne wartości w dedykowanym pliku w przestrzeni runnera, jeden wpis na krok. Każde uruchomienie kroku zastępuje poprzedni wynik.
- R9. Runner czyta ten plik przy starcie każdego kroku. Brak wpisu: wartości z kroku, a gdy ich brak, wartości z `globals`.
- R10. Eskalacja po awariach działa na wyniku z R9, z tym samym progiem i regułą `ceiling` co w #92.
- R11. Wpisy kroku profilującego są zwykłymi wpisami „Rozmowy”. Zawierają `rationale` i informację o obcięciu.
- R12. Nowy wynik nie zeruje licznika awarii kroku.

**Zachowanie przy awarii**
- R13. Krok `profile` zawodzi jak każdy krok agenta: obowiązują `retry_policy`, reprompt i eskalacja do operatora. Nie ma osobnej reguły „zawsze idź dalej”.

**Wspólny kod z akcją `agent`**
- R14. Akcje `agent` i `profile` współdzielą harness, limit czasu, retry i reprompt. Osobne są tylko budowanie promptu i odczyt wyniku.
- R15. Wspólne zachowania trafiają do osobnych typów i modułów, pogrupowanych w foldery funkcjonalne. Ta refaktoryzacja nie zmienia zachowania, a testy #92 przechodzą bez zmian.

**Usunięcie starego mechanizmu**
- R16. Znikają: żądanie `ProfileSteps` w dyrygencie, flaga `plan: true`, zmienna `complexity` i jej parser, pole `unit`, osobny widok profilu w TUI.

---

## Acceptance Examples

- AE1. **Covers R3.** Given krok `build` z własnym `model: small` i `effort: low` bez `ceiling`, when silnik buduje listę dla agenta, then `build` nie ma na liście.
- AE2. **Covers R3.** Given krok `build` z `model: small`, `effort: low` i `ceiling: {model: large, effort: high}`, when silnik buduje listę, then `build` jest na liście.
- AE3. **Covers R6.** Given agent zwraca dla kroku `model: "large"` i `effort: "extreme"`, when silnik waliduje wpis, then model zostaje `large`, a effort zostaje domyślny.
- AE4. **Covers R7, R11.** Given `ceiling` kroku to `effort: high` i agent prosi o `max`, when silnik zapisuje wynik, then wynik ma `high`, a „Rozmowa” pokazuje prośbę o `max` i obcięcie do `high`.
- AE5. **Covers R9.** Given plik wyniku nie ma wpisu dla kroku `review`, when runner startuje `review`, then używa `model` i `effort` z kroku, a gdy ich brak, z `globals`.
- AE6. **Covers R12.** Given krok `fix` zawiódł 3 razy i dostał eskalację o poziom effortu, when krok `profile` uruchamia się ponownie i wskazuje `medium`, then `fix` startuje z `medium` podniesionym o eskalację z 3 awarii.
- AE7. **Covers R13.** Given agent profilujący przekracza limit czasu, when `retry_policy` kroku jest wyczerpana, then krok kończy się błędem jak każdy krok agenta.
- AE8. **Covers R4.** Given wpis z `confident: false`, when silnik zapisuje wynik, then wpisu nie ma w pliku wyniku.

---

## Success Criteria

- Autor workflow widzi w definicji, gdzie i na jakiej podstawie wybierane są model i effort, bez flagi `plan: true` i zmiennej `complexity`.
- Workflow z pętlą może zmienić profil kroków po powrocie.
- Operator widzi w „Rozmowie” propozycję agenta, uzasadnienie i obcięcie do `ceiling`.
- `ce-plan` nie musi wymyślać kontraktu odpowiedzi, reguł wyboru wartości ani kolejności prac.

---

## Scope Boundaries

- Profil na jednostkę planu (`unit`) jest poza zakresem. Wróci, gdy pojawi się realny przypadek.
- Specjalna reguła „awaria kroku `profile` nie zatrzymuje zadania” jest poza zakresem.
- Zerowanie licznika awarii po nowym profilu jest poza zakresem.
- Wybór modelu przez dyrygenta dla pojedynczych kroków w trakcie zadania jest poza zakresem.
- Zmiana tierów, skali effortów i mapowania tierów na modele harnessów jest poza zakresem.

---

## Key Decisions

- **Dedykowany krok zamiast wywołania po kroku planu:** autor kontroluje miejsce i źródło oceny, a profil można przeliczyć po powrocie.
- **Nowy typ akcji zamiast flagi na `agent`:** czytelniejszy zapis w workflow. Koszt: wspólny kod trzeba wydzielić, żeby nie duplikować harnessu, retry i reprompta.
- **Wynik w pliku runnera, nie w magazynie dyrygenta:** runner czyta go przy starcie kroku. Nie ma zależności od dyrygenta.
- **Przypięcie kroku przez jawny `model` i `effort`:** `ceiling` zostawia krok na liście, bo autor dał miejsce na podniesienie.
- **Bez resetu licznika awarii:** awarie są dowodem, a `resolve` nie wymaga dodatkowego kodu.
- **Refaktoryzacja najpierw, funkcja potem:** dwa etapy w jednym planie, osobne commity lub PR. Przegląd i szukanie regresji są prostsze.

---

## Dependencies / Assumptions

- Zachowujemy `step_profile::resolve` (kolejność: wartość kroku, wynik profilowania, eskalacja, `ceiling`) z #92.
- Wymóg z #92, że krok z `ceiling` ma `model` i `effort` (własne lub z `globals`), pozostaje.
- Założenie niezweryfikowane do końca: pole `unit` nie jest używane przez silnik przy starcie kroku. Wynika z przeszukania kodu (`lookup` z jednostką występuje tylko w testach magazynu). Plan ma to potwierdzić przed usunięciem.
- Dokument zastępuje część `docs/brainstorms/2026-11-10-dynamiczny-model-i-effort-kroku-requirements.md` (profilowanie przez dyrygenta). Eskalacja po awariach, tiery i `ceiling` z tamtego dokumentu pozostają.

---

## Outstanding Questions

### Resolve Before Planning

Brak.

### Deferred to Planning

- [Affects R8][Technical] Format i miejsce pliku wyniku w przestrzeni runnera oraz zachowanie przy restarcie demona.
- [Affects R14, R15][Technical] Podział kodu akcji `agent` na wspólne typy i foldery funkcjonalne. Dla każdego `match` na `ActionKind` trzeba podjąć jawną decyzję.
- [Affects R4][Technical] Kształt odpowiedzi agenta `profile` w kontrakcie wyjścia i sposób odczytu (bez `choice` czy z `choice` kroku).
- [Affects R16][Needs research] Pełna lista miejsc do usunięcia (dyrygent, TUI, magazyn, dokumentacja schematu workflow).
- [Affects R9][Technical] Co dzieje się z zadaniami, które w momencie wdrożenia mają profil w magazynie dyrygenta.
