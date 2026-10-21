---
date: 2026-10-21
topic: compound-ocena-w-pr
---

# Workflow compound: ocena użytkownika w PR, nie w trakcie kroków

## Summary

Workflow `compound-engineering` ma rzadziej zatrzymywać się na pytania do użytkownika przed publikacją. Agent sam decyduje, czy dokument wymaga przeglądu. Użytkownik ocenia wynik w PR (merge request), po publikacji. Zmiany dotyczą `workflows/compound-engineering.yaml` i jego opisu w `workflows/README.md`.

---

## Problem Frame

Dziś użytkownik jest pytany w trzech miejscach, zanim dokument trafi do PR (zweryfikowane w `workflows/compound-engineering.yaml`):

- Krok `brainstorm` ma w prompcie: „When the direction is unclear, ask the person". Agent może więc wstrzymać krok pytaniem.
- Krok `doc_review` (wymagania) kończy się wyborem `reviewed` z `requires_human_approval: true`. Człowiek akceptuje wynik przeglądu, zanim dokument pójdzie do `publish_requirements`.
- Krok `plan_review` (plan) działa tak samo, przed `publish_plan`.

Po publikacji workflow i tak czeka na człowieka w bramkach `approve_requirements` i `approve_plan`. Tam użytkownik czyta dokument w PR. Akceptacja przed publikacją dubluje więc akceptację po publikacji. Użytkownik ocenia ten sam dokument dwa razy, a pierwszy raz w mniej wygodnym miejscu niż PR.

Dodatkowo `doc_review` uruchamia się zawsze. Dla małej lub oczywistej zmiany to koszt bez wartości.

---

## Requirements

**Przegląd wymagań nie zawsze jest potrzebny**

- R1. Agent kroku `brainstorm` decyduje, czy dokument wymagań potrzebuje `doc_review`. Ma dwie ścieżki: z przeglądem albo od razu do publikacji.
- R2. Agent podaje powód swojej decyzji w jednym zdaniu. Powód jest zapisany w historii zadania, tak aby użytkownik zobaczył go przy ocenie.
- R3. Gdy agent pomija przegląd, dokument i tak trafia do PR i do bramki `approve_requirements`. Pominięcie przeglądu nie pomija oceny użytkownika.
- R4. Gdy agent ma wątpliwość, wybiera przegląd. Przegląd jest domyślny.

**Brak akceptacji przed publikacją**

- R5. Wynik `doc_review` idzie do `publish_requirements` bez akceptacji użytkownika. Wynik `plan_review` idzie do `publish_plan` bez akceptacji użytkownika.
- R6. Użytkownik nadal widzi podsumowanie przeglądu (`review_summary`). Widzi je w PR razem z dokumentem, nie w osobnym zatrzymaniu workflow.
- R7. Bramki `approve_requirements`, `approve_plan`, `approve_work` i `approve_learning` zostają bez zmian. Są one miejscem, w którym użytkownik decyduje po przeczytaniu PR. Ich wybory (`approve`, `revise`, `abandon` itd.) i ścieżki powrotu nie zmieniają się.

**Mniej pytań w kroku brainstorm**

- R8. Krok `brainstorm` nie wstrzymuje się na pytania do użytkownika w trakcie workflow. Gdy kierunek jest niejasny, agent zapisuje w dokumencie wymagań jawne założenia i otwarte pytania, osobno oznaczone.
- R9. Użytkownik odpowiada na te pytania w PR. Odpowiedź wraca do workflow dwiema drogami, które już istnieją: komentarz w PR (`check_requirements_feedback`) albo komentarz przy `revise` w bramce.

---

## Success Criteria

- Zadanie przechodzi od opisu do pierwszego PR z wymaganiami bez żadnego zatrzymania na pytanie lub akceptację.
- Użytkownik czyta jeden dokument w jednym miejscu (PR) i tam decyduje.
- Dla małej zmiany użytkownik widzi w historii, że przegląd pominięto, i widzi powód.

---

## Scope Boundaries

- Bramki po publikacji nie znikają. Workflow nadal nie przechodzi dalej bez decyzji użytkownika po przeczytaniu PR.
- Krok `finalize_blocked` i bramki scalania nie zmieniają się.
- Sam skill `ce-doc-review` nie zmienia się. Zmienia się tylko to, kiedy workflow go uruchamia i kto akceptuje jego wynik.

---

## Key Decisions

- Decyzja o pominięciu przeglądu należy do agenta (wskazanie użytkownika), a nie do reguły opartej na rozmiarze dokumentu.
- Ocena użytkownika przesuwa się z „przed publikacją" na „po publikacji, w PR". Użytkownik wskazał tę zasadę dla ścieżki do publikacji, np. `publish_plan`.

---

## Dependencies / Assumptions

- Założenie do potwierdzenia przez użytkownika: pominięcie przeglądu dotyczy tylko wymagań (`brainstorm`). Dla planu (`plan_review`) przegląd uruchamia się zawsze, ale bez akceptacji przed publikacją (R5). Uwagi użytkownika o pomijaniu przeglądu wymieniają tylko brainstorm.
- Założenie do potwierdzenia: R8 i R9 (brak pytań w `brainstorm`, założenia w dokumencie) wynikają z uwagi operatora „ograniczyć odpytywanie usera w samym workflow na rzecz wypychania do PR". Użytkownik nie wybrał wariantu opisanego jako „brainstorm nigdy nie pyta"; wybrał własny opis zmian. Jeśli chce zostawić pytania blokujące w `brainstorm`, R8 trzeba zawęzić.
- Założenie do potwierdzenia: pytania agenta w kroku `work` (`ce-work`) zostają bez zmian. Wymaganie ich nie obejmuje.
- Zweryfikowane: `requires_human_approval` występuje w `workflows/compound-engineering.yaml` tylko dla `doc_review` i `plan_review`. Opis w `workflows/README.md` („a person accepts that result before it is published") musi się zmienić razem z workflow.

---

## Outstanding Questions

### Deferred to Planning

- [Affects R1, R2][Technical] Jak wyrazić dwie ścieżki z kroku `brainstorm` w kontrakcie kroku (dodatkowy wybór z własnym przejściem) i gdzie zapisać powód tak, aby był widoczny w PR.
- [Affects R5][Technical] Czy usunięcie `requires_human_approval` z wyboru `reviewed` wystarcza, czy trzeba zmienić też testy w `tests/builtin_workflows.rs` i `WORKFLOW_SCHEMA.md`.
- [Affects R8][Needs research] Jak prompt kroku `brainstorm` ma zabronić pytań do użytkownika, skoro skill `ce-brainstorm` domyślnie je zadaje.
