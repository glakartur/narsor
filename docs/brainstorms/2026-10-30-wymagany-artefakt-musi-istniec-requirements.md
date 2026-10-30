---
date: 2026-10-30
topic: wymagany-artefakt-musi-istniec
---

# Workflow nie idzie dalej bez prawidłowej odpowiedzi agenta

## Summary

Agent, który nie może wykonać kroku, dostaje uczciwy sposób, żeby to zgłosić. Taka odpowiedź zatrzymuje krok i nie wybiera przejścia. Osobno silnik sprawdza, czy wymagany artefakt wskazuje istniejący plik.

---

## Problem Frame

Krok `brainstorm` przeszedł do `doc_review` bez dokumentu wymagań. Zapis w `docs/plans/` został odrzucony, a Jira była niedostępna. Agent napisał w tekście, że dokumentu nie zapisał i że czeka na nowe zatwierdzenie. Potem zwrócił `choice: requirements_drafted`. Silnik przyjął ten wybór i poszedł dalej.

Główna przyczyna: agent nie miał prawidłowej odpowiedzi na tę sytuację.

- Krok `brainstorm` dopuszcza tylko dwa wybory: `requirements_drafted` i `requirements_ready`. Oba oznaczają „dokument jest zapisany”.
- Kontrakt zabrania `awaiting_input` dla błędów uprawnień, sandboxa i narzędzi. Kontrakt nie daje jednak agentowi sposobu, żeby zgłosić taki błąd.
- Agent musiał więc wybrać jedną z dwóch nieprawdziwych etykiet albo nie odpowiedzieć wcale. Wybrał etykietę.
- Silnik nie ma jak odróżnić taki wybór od prawdziwego. Silnik czyta tylko etykietę i pola obiektu. Silnik nie czyta tekstu przed obiektem.

Zweryfikowane w kodzie:

- Silnik przyjmuje obiekt z poprawnym `choice` i wymaganymi polami (`extract_response`, `crates/narsor-engine/src/engine/actions/agent.rs`).
- Według `WORKFLOW_SCHEMA.md` (§3.3.A) błąd akcji kończy execution ze statusem `failed`, bez przejścia. Agent nie ma jednak odpowiedzi, która zgłasza taki błąd.

Wniosek z danych (nie z kodu TUI): sekwencje `\\n` i `\\"` w widoku raw wyglądają na podwójne zakodowanie przez sam widok. Odpowiedź zawierała poprawny JSON i silnik go odczytał. Zapis nie wskazuje błędu wyświetlania.

Osobna kwestia: agent podał ścieżkę artefaktu do pliku, którego nie ma. Silnik sprawdza tylko niepustą ścieżkę (`unmet_requirements`, `crates/narsor-engine/src/engine/runner.rs`). Ta kontrola nie rozwiązuje głównego problemu, bo agent może też podać ścieżkę do pliku, który istnieje, a krok i tak nie jest wykonany.

---

## Requirements

**Zgłoszenie niemożności wykonania kroku (główny wymóg)**

- R1. Kontrakt odpowiedzi agenta ma odpowiedź „krok nie został wykonany”. Przykład: `{"status":"failed","reason":"<powód>"}`. Silnik nie traktuje jej jak wyboru.
- R2. Silnik kończy akcję błędem po odpowiedzi z R1. Execution dostaje status `failed`. Silnik nie wybiera przejścia i nie wchodzi do następnego stanu.
- R3. Powód z R1 jest widoczny w historii kroku i w TUI przy nieudanym kroku. Operator widzi, czego agent nie mógł zrobić.
- R4. Kontrakt wyjścia, który silnik dopisuje do promptu, opisuje odpowiedź z R1. Kontrakt mówi: gdy nie możesz wykonać kroku (brak uprawnienia, brak narzędzia, brak dostępu), nie wybieraj etykiety. Zwróć odpowiedź z R1.
- R5. Odpowiedź z R1 jest dostępna w każdym kroku agenta. Autor workflow nie dopisuje do niej wyboru.
- R6. Odpowiedź, która nie jest ani poprawnym wyborem, ani odpowiedzią z R1, ani `awaiting_input`, nie przesuwa workflow. Silnik pyta agenta ponownie w tej samej sesji. Po limicie reprompta krok kończy się błędem. To zachowanie już istnieje i zostaje bez zmian.

**Wymagany artefakt (zabezpieczenie dodatkowe)**

- R7. Gdy wybrany `choice` wymaga artefaktu, silnik sprawdza, czy ścieżka wskazuje istniejący, niepusty plik zwykły w obszarze roboczym kroku.
- R8. Gdy pliku nie ma, silnik traktuje artefakt jak brakujący i pyta agenta ponownie w tej samej sesji. Komunikat podaje nazwę artefaktu i ścieżkę.
- R9. Silnik odrzuca ścieżkę poza obszarem roboczym kroku (ścieżka bezwzględna lub `..`).
- R10. Kontrola z R7 dotyczy tylko artefaktów z `requires_artifacts`. Artefakty opcjonalne i `additional_artifacts` zostają bez zmian.

**Dokumentacja**

- R11. `WORKFLOW_SCHEMA.md` opisuje odpowiedź z R1 (§3.3.A) i kontrolę z R7.

---

## Acceptance Examples

- AE1. **Covers R1, R2, R3.** Dane: agent nie może zapisać dokumentu, bo zapis jest odrzucony. Agent zwraca `{"status":"failed","reason":"Zapis w docs/plans/ odrzucony."}`. Execution dostaje status `failed`. Stan nie zmienia się na `doc_review`. Historia pokazuje powód.
- AE2. **Covers R4.** Dane: krok `brainstorm` z dwoma wyborami. Prompt zawiera opis odpowiedzi „krok nie został wykonany”.
- AE3. **Covers R6.** Dane: agent zwraca tekst bez obiektu JSON. Silnik pyta ponownie. Stan nie zmienia się.
- AE4. **Covers R7, R8.** Dane: wybór `requirements_drafted` wymaga `requirements_doc`. Agent podaje `docs/plans/x.md`, a pliku nie ma. Silnik pyta ponownie z komunikatem o brakującym pliku.
- AE5. **Covers R9.** Dane: agent podaje `/etc/passwd` albo `../../x.md`. Silnik traktuje artefakt jak brakujący.
- AE6. **Covers R7.** Dane: agent zapisuje plik i podaje jego ścieżkę. Silnik przyjmuje odpowiedź.

---

## Success Criteria

- Agent, który nie może wykonać kroku, kończy krok jawnym błędem. Agent nie wybiera nieprawdziwej etykiety.
- Workflow nie przechodzi do następnego kroku po odpowiedzi „krok nie został wykonany”.
- Operator widzi powód zatrzymania bez czytania surowego strumienia.
- Istniejące workflow działają bez zmian, gdy agent odpowiada poprawnie.

---

## Scope Boundaries

- Poza zakresem: silnik nie ocenia prawdziwości tekstu agenta ani treści `additional_data` (np. `file_written`). Silnik nie ma jak tego sprawdzić. Rozwiązaniem jest uczciwa odpowiedź z R1, nie wykrywanie kłamstwa.
- Poza zakresem: ocena jakości dokumentu. Tę rolę ma krok `doc_review`.
- Poza zakresem: zmiany w wyświetlaniu raw w TUI. Zapis nie wskazuje błędu wyświetlania.
- Poza zakresem: zmiany w skillu `ce-brainstorm`.
- Poza zakresem: nowe wybory w definicji workflow `zmiana`. Odpowiedź z R1 działa bez nich.

---

## Key Decisions

- Zgłoszenie niemożności jest częścią kontraktu silnika, nie wyborem w workflow. Każdy krok agenta dostaje je bez zmian w definicji. Alternatywa (dodać `*_blocked` do każdego kroku) mnoży etykiety i przejścia.
- Odpowiedź z R1 kończy akcję błędem, a nie wywołuje `awaiting_input`. Kontrakt już mówi, że błędy sandboxa i uprawnień nie są pytaniami do człowieka. Operator może wznowić lub ponowić krok istniejącymi akcjami dla kroków `failed`.
- Kontrola istnienia pliku zostaje jako drugi, tani bezpiecznik. Nie zastępuje R1.

---

## Założenia do potwierdzenia

- „Prawidłowa odpowiedź typu choice” oznacza odpowiedź, którą agent może uczciwie dać. Agent w opisanym zdarzeniu nie miał takiej odpowiedzi. Takie rozumienie przyjęto z uwagi użytkownika. Użytkownik nie opisał szczegółów.
- Widok raw podwaja znaki ucieczki w samym widoku. Odpowiedź agenta była poprawnym JSON. Wniosek pochodzi z analizy zapisu. Nie sprawdzono kodu TUI ani surowego strumienia.
- Definicji workflow `zmiana` nie ma w tym repozytorium (`workflows/` zawiera tylko `template.yaml`). Dwa wybory kroku `brainstorm` ustalono z promptu kroku, nie z pliku.
- Status `failed` z istniejącymi akcjami wznowienia wystarcza operatorowi. Nie sprawdzono, jakie akcje TUI dotyczą kroku `failed`.
- Obszar roboczy kroku (`execution.workspace.path`) jest właściwą bazą dla ścieżek względnych w `artifacts`.
- Treść zgłoszenia WRUW-6889 jest niedostępna (konektor Atlassian wymaga autoryzacji). Dokument nie opiera się na tym zgłoszeniu.

---

## Pytania otwarte dla użytkownika

- Czy „nieprawidłowa odpowiedź typu choice” oznacza dokładnie ten przypadek: agent nie miał uczciwego wyboru? Czy chodzi też o inną wadę odpowiedzi (np. JSON nie w osobnym bloku, tekst „nie zapisano” obok etykiety „zapisano”)? Odpowiedź zmienia zakres R6.
- Czy odpowiedź „krok nie został wykonany” ma kończyć execution ze statusem `failed`, czy zawiesić krok w `input_needed` i czekać na człowieka? Dokument przyjmuje `failed`.
- Czy silnik ma dodatkowo odrzucać odpowiedź, w której `reason` lub tekst przed obiektem mówi o niepowodzeniu? Dokument odradza to: wykrywanie sensu tekstu daje fałszywe alarmy.
- Czy kontrola istnienia pliku (R7–R10) ma objąć wszystkie kroki z `requires_artifacts`? Dokument zakłada, że tak.

---

## Outstanding Questions

**Do planowania**

- Gdzie w kodzie (`agent.rs`, `answer.rs`, `crates/narsor-protocol`) odczytuje się odpowiedź i jak dodać wariant `failed` obok `awaiting_input`.
- Czy schemat JSON dla harnessu `claude-code` (jeden płaski obiekt, bez pól wymaganych) przyjmie nowy wariant.
- Skąd `unmet_requirements` bierze ścieżkę obszaru roboczego (dziś funkcja jej nie dostaje).
- Jak kontrola z R7 działa dla obszaru roboczego w kontenerze (`image`).
