---
date: 2026-10-30
topic: wymagany-artefakt-musi-istniec
---

# Wymagany artefakt musi istnieć, zanim workflow przejdzie dalej

## Summary

Silnik przyjmuje wybór agenta tylko wtedy, gdy każdy artefakt wymagany przez ten wybór wskazuje istniejący plik. Gdy pliku nie ma, silnik pyta agenta ponownie w tej samej sesji. Krok `brainstorm` nie przechodzi wtedy do `doc_review`.

---

## Problem Frame

Krok `brainstorm` przeszedł do `doc_review` bez dokumentu wymagań. Agent nie zapisał pliku: zapis w `docs/plans/` został odrzucony, a Jira była niedostępna. Mimo to agent zwrócił `choice: requirements_drafted` i podał ścieżkę w `artifacts.requirements_doc`. Ta sama odpowiedź zawierała `additional_data.file_written: false`. Następny krok dostał ścieżkę do pliku, którego nie ma.

Co ustalono w kodzie (zweryfikowane):

- Silnik sprawdza wymagane artefakty tylko pod kątem niepustej ścieżki (`unmet_requirements` w `crates/narsor-engine/src/engine/runner.rs`). Nie sprawdza, czy plik istnieje.
- Silnik rejestruje artefakt z samej ścieżki, bez dostępu do pliku (`artifact_at` w `crates/narsor-engine/src/engine/runner.rs`).
- Silnik nie interpretuje `additional_data` (`WORKFLOW_SCHEMA.md` §3.3.A). Pole `file_written: false` nie zatrzymało przejścia.

Co ustalono z zapisu raw (wniosek z danych, nie z kodu TUI):

- Sekwencje `\\n` i `\\"` w widoku raw wyglądają na podwójne zakodowanie przez sam widok. Odpowiedź agenta zawierała poprawny obiekt JSON na końcu tekstu, a silnik go odczytał. Zapis nie pokazuje błędu formatu.
- Przyczyna nie leży więc w wyświetlaniu TUI. Przyczyną jest brak kontroli treści odpowiedzi agenta.

---

## Requirements

**Kontrola artefaktów wymaganych przez wybór**

- R1. Gdy wybrany `choice` wymaga artefaktu, silnik sprawdza, czy ścieżka wskazuje istniejący plik zwykły w obszarze roboczym kroku.
- R2. Gdy plik nie istnieje, silnik traktuje artefakt jak brakujący. Silnik pyta agenta ponownie w tej samej sesji (reprompt). Silnik nie przechodzi do następnego stanu.
- R3. Komunikat reprompta podaje nazwę artefaktu i ścieżkę, której nie znaleziono. Agent wie wtedy, co ma poprawić.
- R4. Silnik odrzuca ścieżkę poza obszarem roboczym kroku (ścieżka bezwzględna lub `..`). Silnik traktuje taki artefakt jak brakujący.
- R5. Silnik odrzuca pusty plik jako artefakt wymagany.
- R6. Gdy kolejne reprompty nie dają pliku, krok kończy się błędem według istniejącej zasady limitu reprompta. Silnik nie wybiera przejścia za agenta.

**Kontrakt agenta**

- R7. Kontrakt wyjścia, który silnik dopisuje do promptu kroku, mówi: wymagany artefakt musi istnieć jako plik w chwili odpowiedzi.
- R8. `WORKFLOW_SCHEMA.md` opisuje nową kontrolę w sekcji o zasadach odczytu odpowiedzi (§3.3.A) i w opisie artefaktu kroku.

**Zakres kontroli**

- R9. Kontrola dotyczy tylko artefaktów wymaganych przez wybrany `choice` (`requires_artifacts`). Artefakty opcjonalne i `additional_artifacts` zostają bez zmian.

---

## Acceptance Examples

- AE1. **Covers R1, R2, R3.** Dane: wybór `requirements_drafted` wymaga `requirements_doc`. Agent podaje `docs/plans/x.md`, a pliku nie ma. Silnik pyta agenta ponownie z komunikatem o brakującym pliku `docs/plans/x.md`. Stan nie zmienia się na `doc_review`.
- AE2. **Covers R1.** Dane: ten sam wybór. Agent zapisuje plik i podaje jego ścieżkę. Silnik przyjmuje odpowiedź i przechodzi do `doc_review`.
- AE3. **Covers R4.** Dane: agent podaje `/etc/passwd` albo `../../x.md`. Silnik traktuje artefakt jak brakujący i pyta ponownie.
- AE4. **Covers R5.** Dane: agent tworzy pusty plik. Silnik pyta ponownie.
- AE5. **Covers R6.** Dane: agent nie zapisuje pliku w żadnej próbie. Krok kończy się błędem z powodem „brak wymaganego artefaktu”. Execution nie dociera do `doc_review`.
- AE6. **Covers R9.** Dane: wybór nie wymaga artefaktów. Agent nie podaje plików. Silnik przyjmuje odpowiedź jak dotąd.

---

## Success Criteria

- Krok z wymaganym artefaktem nie przechodzi dalej, gdy plik nie istnieje.
- Operator widzi w historii kroku powód ponowienia: brakujący plik i jego ścieżkę.
- Istniejące workflow, w których agent zapisuje pliki poprawnie, działają bez zmian.

---

## Scope Boundaries

- Poza zakresem: interpretacja `additional_data` (np. `file_written`). Pole zostaje treścią bez znaczenia dla silnika.
- Poza zakresem: sprawdzanie treści lub jakości dokumentu. Tę rolę ma krok `doc_review`.
- Poza zakresem: zmiany w wyświetlaniu raw w TUI. Zapis nie wskazuje błędu wyświetlania.
- Poza zakresem: zmiany w skillu `ce-brainstorm`.

---

## Key Decisions

- Kontrola leży w silniku, nie w prompcie. Prompt nie zatrzyma agenta, który myli się co do własnego stanu. Silnik widzi plik, więc silnik rozstrzyga.
- Brak pliku powoduje reprompt, nie od razu błąd. Ten sam mechanizm już obsługuje brak zmiennej lub artefaktu. Zachowanie jest spójne.

---

## Założenia do potwierdzenia

- Widok raw podwaja znaki ucieczki w samym widoku. Odpowiedź agenta była poprawnym JSON. Wniosek pochodzi z analizy zapisu. Nie sprawdzono kodu TUI ani surowego strumienia.
- Obszar roboczy kroku (`execution.workspace.path`) jest właściwą bazą dla ścieżek względnych w `artifacts`.
- Agent w opisanym zdarzeniu nie mógł zapisać pliku z powodu odrzuconego uprawnienia. Silnik nie wie o tym. Silnik widzi tylko brak pliku.
- Wymagany artefakt jest zawsze plikiem. Katalog nie spełnia wymagania.
- Reprompt w tej samej sesji wystarczy. Agent bez zgody na zapis nie zapisze pliku także po reprompcie. Krok wtedy kończy się błędem (R6). Taki wynik jest bezpieczniejszy niż fałszywe przejście.
- Treść zgłoszenia WRUW-6889 jest niedostępna (konektor Atlassian wymaga autoryzacji). Dokument nie opiera się na tym zgłoszeniu.

---

## Pytania otwarte dla użytkownika

- Czy workflow `zmiana` ma dostać wybór `requirements_blocked` (lub podobny) dla kroku `brainstorm`? Agent, który nie może zapisać dokumentu, ma dziś tylko dwa wybory z wymaganym artefaktem. Błąd po limicie reprompta może wystarczyć. Wybór blokady daje czytelniejszą ścieżkę do człowieka.
- Czy kontrola ma objąć wszystkie kroki z `requires_artifacts`, czy tylko wskazane w workflow? Domyślnie dokument zakłada wszystkie kroki.
- Czy ścieżka artefaktu ma wskazywać plik w repozytorium kroku, czy dopuszczamy też pliki poza repozytorium, ale w katalogu roboczym zadania? Domyślnie dokument dopuszcza tylko obszar roboczy kroku.

---

## Outstanding Questions

**Do planowania**

- Skąd `unmet_requirements` bierze ścieżkę obszaru roboczego (dziś funkcja jej nie dostaje).
- Czy kontrola działa tak samo dla kroków `script` i dla odpowiedzi człowieka lub dyrygenta.
- Jak zachować się przy obszarze roboczym w kontenerze (`image`), gdy plik leży w kontenerze.
