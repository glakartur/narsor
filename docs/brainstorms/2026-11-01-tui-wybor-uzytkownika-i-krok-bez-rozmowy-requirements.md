---
date: 2026-11-01
topic: tui-wybor-uzytkownika-i-krok-bez-rozmowy
---

# Wybór użytkownika pokazany raz, a krok dla człowieka widoczny w Rozmowie

## Summary

W zakładce *Rozmowa* wybór, który podjął użytkownik, pojawia się jeden raz, jako wybór użytkownika. Krok, w którym czeka człowiek, ma w *Rozmowie* własny nagłówek, więc widać, na którym kroku wybór padł.

---

## Problem Frame

W *Rozmowie* kroki przeznaczone dla osób nie są pokazywane. Wpisy wyboru stoją wtedy po rozmowie poprzedniego kroku. Nagłówek „── krok #n ──” pojawia się tylko dla kroku, który ma rozmowę z agentem (`push_position` w `src/tui/task_app/tabs/conversation.rs`).

Przykład z `publish_learning #1`:

```
WYBÓR WORKFLOW · 04:18:58  agent · rules wybrał opcję: published
WYBÓR WORKFLOW · 04:19:25  użytkownik wybrał opcję: check_feedback
WYBÓR WORKFLOW · 04:19:25  agent · rules wybrał opcję: check_feedback
── check_learning_feedback #1 ──
```

Są dwa błędy:

1. **Podwójny wybór.** Odpowiedź użytkownika zapisuje dwa zdarzenia: `HumanInputReceived` (od operatora, z etykietą) i `DecisionMade`. Silnik zapisuje `DecisionMade` w `apply_decision` (`crates/narsor-engine/src/engine/runner.rs`) pod nazwą dostawcy decyzji, tu `rules`. TUI pokazuje oba zdarzenia w `workflow_choice_entry`. Drugi wpis mówi „agent · rules”, co jest nieprawdą: wybrał człowiek. Ten sam skutek ma ręczne przejście: `DecisionMade` z dostawcą `human` (lub `human_override`) i `HumanTransitionSelected` dają „agent · human” i „użytkownik”.
2. **Brak kroku.** Między 04:18:58 a 04:19:25 workflow wszedł w krok dla człowieka. Ten krok nie ma próby agenta, więc nie ma nagłówka. Wybór użytkownika wygląda, jakby należał do `publish_learning`.

Wpisy wyboru powstają ze zdarzeń w `workflow_choices` / `workflow_choice_entry`. Wejścia w kroki wynikają ze zdarzeń `StateEntered` (`step_entries`).

---

## Requirements

**Jeden wpis na wybór użytkownika**

- R1. Wybór podjęty przez użytkownika daje w *Rozmowie* jeden wpis „WYBÓR WORKFLOW” z aktorem „użytkownik”.
- R2. `DecisionMade`, który silnik zapisał po odpowiedzi użytkownika (ta sama etykieta, w tym samym wejściu w krok), nie daje wpisu. Dotyczy każdego dostawcy w tym zdarzeniu, także `rules`.
- R3. `DecisionMade` zapisany przy ręcznym przejściu (dostawca `human`, `human_override`) nie daje wpisu. Wpis daje `HumanTransitionSelected`.
- R4. Wybory agenta i dyrygenta są bez zmian: każdy ma jeden wpis, także gdy decyzję podjęły reguły (`agent · rules`) bez udziału człowieka.
- R5. Odpowiedź użytkownika bez etykiety wyjścia (np. wznowienie agenta) nie zmienia niczego w pokazywaniu wyborów.

**Nagłówek kroku bez rozmowy**

- R6. W widoku całej *Rozmowy* każde wejście w krok, które nie ma własnej rozmowy (krok dla człowieka, krok bez wpisów), dostaje nagłówek „── <krok> #n ──” w tym samym stylu co nagłówki kroków z rozmową.
- R7. Nagłówek stoi po rozmowie poprzedniego kroku i przed wyborami podjętymi w tym kroku. W przykładzie między wyborem z 04:18:58 a wyborem z 04:19:25 stoi nagłówek kroku, w którym czekał użytkownik.
- R8. Krok, który ma rozmowę, ma jeden nagłówek, jak dziś. Nagłówek z R6 nie dubluje go.
- R9. Widok pojedynczego kroku (*Przebieg*) nie dostaje nagłówków. Wybory kroku wynikają z R1–R5.

**Bez zmian**

- R10. Treść, nagłówek czasu, identyfikator i wyróżnienie wpisu wyboru są bez zmian.
- R11. Kolejność wpisów jest po kolejności zdarzeń, jak dziś.
- R12. Zmiany nie dotykają silnika ani formatu zdarzeń: zmiana jest tylko w budowaniu *Rozmowy* w TUI.

---

## Acceptance Examples

- AE1. **Covers R1, R2.** Dane: krok dla człowieka; zdarzenia `HumanInputReceived` (operator, `check_feedback`) i `DecisionMade` (`rules`, `check_feedback`). Wynik: jeden wpis „użytkownik wybrał opcję: check_feedback”. Brak „agent · rules wybrał opcję: check_feedback”.
- AE2. **Covers R3.** Dane: ręczne przejście: `DecisionMade` (`human`) i `HumanTransitionSelected`. Wynik: jeden wpis „użytkownik”.
- AE3. **Covers R4.** Dane: krok agenta, `DecisionMade` (`rules`, `published`) bez odpowiedzi użytkownika. Wynik: wpis „agent · rules wybrał opcję: published”.
- AE4. **Covers R6, R7.** Dane: po `publish_learning #1` następuje `StateEntered` kroku dla człowieka, potem wybór użytkownika, potem `check_learning_feedback #1`. Wynik: kolejność to wybór „published”, nagłówek kroku dla człowieka, wybór użytkownika, nagłówek `check_learning_feedback #1`.
- AE5. **Covers R8.** Dane: krok z rozmową agenta. Wynik: jeden nagłówek tego kroku.
- AE6. **Covers R9.** Dane: ten sam krok otwarty w *Przebiegu*. Wynik: brak nagłówków, jeden wpis wyboru użytkownika.
- AE7. **Covers R2.** Dane: ta sama etykieta wybrana przez użytkownika w dwóch kolejnych wejściach w krok. Wynik: dwa wpisy użytkownika, bez wpisów agenta.

---

## Success Criteria

- Operator czyta rozmowę z fragmentu zgłoszenia i widzi: krok agenta, nagłówek kroku dla człowieka, jeden wybór użytkownika, następny krok.
- Żaden wybór człowieka nie jest przypisany agentowi.

---

## Scope Boundaries

- Poza zakresem: zmiana silnika, np. dostawcy zapisywanego w `DecisionMade` po odpowiedzi użytkownika.
- Poza zakresem: zakładki *Zdarzenia*, *Dyrygent* i *Przebieg* (poza R9), historia, lista zadań.
- Poza zakresem: pokazywanie treści odpowiedzi użytkownika (pola formularza, komentarz).
- Poza zakresem: duplikaty wyborów dyrygenta (`ConductorDecision` i `DecisionMade` z dostawcą `conductor`). Nie opisano ich w zgłoszeniu.

---

## Key Decisions

- Poprawka w TUI, nie w silniku. `DecisionMade` po odpowiedzi użytkownika jest poprawnym zapisem silnika (reguły zastosowały etykietę). Błąd jest w tym, jak TUI go opisuje.
- Duplikat rozpoznaje się po kolejności zdarzeń: odpowiedź operatora z etykietą, potem `DecisionMade` z tą samą etykietą w tym samym wejściu w krok. Sama nazwa dostawcy nie wystarcza (`rules`).
- Nagłówek kroku bez rozmowy bierze nazwę z `StateEntered`; numer to numer wejścia w krok.

---

## Założenia do potwierdzenia

- „Widok szczegółów” to zakładka *Rozmowa* szczegółów zadania. Nagłówki „── publish_learning #1 ──” w przykładzie pochodzą z tego widoku.
- Kroki bez własnych wpisów, które nie czekają na człowieka (np. skrypt), też dostają nagłówek. Nagłówek odróżnia kroki, a nie tylko kroki ludzi.
- Numer „#n” nagłówka kroku bez rozmowy to numer wejścia w krok. Dla kroków z próbami numer liczy próby (`timeline`). Dla kroków dla człowieka te liczby się nie rozchodzą, bo nie mają prób.
- `DecisionMade` z dostawcą `human` lub `human_override` zawsze pochodzi od użytkownika.
- Duplikat wyboru dyrygenta nie jest w zakresie, bo przykład go nie pokazuje.

---

## Pytania otwarte dla użytkownika

- Czy sprawdzić i usunąć podobny duplikat dla wyborów dyrygenta jako osobny temat?
