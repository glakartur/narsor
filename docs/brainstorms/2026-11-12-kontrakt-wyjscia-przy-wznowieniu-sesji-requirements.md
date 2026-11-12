---
date: 2026-11-12
topic: kontrakt-wyjscia-przy-wznowieniu-sesji
---

# Kontrakt wyjścia tylko raz na sesję

## Summary

Gdy operator wraca do kroku z komentarzem lub odpowiedzią w tej samej sesji agenta, narsor nie dokleja do wiadomości kontraktu wyjścia. Agent dostał kontrakt w pierwszym prompcie sesji. Kontrakt zostaje przy pierwszym uruchomieniu kroku, przy ponowieniu kroku w nowej sesji i przy naprawie formatu odpowiedzi.

---

## Problem Frame

Narsor dokleja do każdego promptu agenta blok „When you are done, end your reply with a single JSON object…”. Blok ma kilkanaście linii. Zawiera kształt odpowiedzi, listę labeli, zmienne i artefakty kroku (`crates/narsor-engine/src/engine/actions/agent/contract.rs`, `output_contract`; użycie w `crates/narsor-engine/src/engine/actions/agent.rs`).

Operator wraca do kroku z komentarzem albo odpowiedzią na pytanie agenta. Silnik wznawia wtedy tę samą sesję harnessu (`continue_session` w `crates/narsor-engine/src/engine/runner/step/harness_attempt.rs`). Wiadomość operatora dostaje ten sam blok kontraktu co prompt początkowy. Sesja ma już ten kontrakt w historii.

Skutki:

- Krótka odpowiedź operatora („tak, rób dalej”) wygląda w rozmowie jak długa instrukcja.
- Agent czyta ten sam kontrakt przy każdej wiadomości. To szum i koszt tokenów.
- W widoku rozmowy operatora (TUI) każda jego wiadomość ma doklejony blok techniczny.

---

## Actors

- A1. Operator: wraca do kroku z komentarzem lub odpowiedzią.
- A2. Agent kroku: prowadzi sesję harnessu, której ciąg dalszy dostaje wiadomość operatora.
- A3. Silnik: buduje prompt i czyta odpowiedź agenta.

---

## Key Flows

- F1. Powrót z komentarzem w tej samej sesji
  - **Trigger:** Krok czeka na odpowiedź operatora. Operator ją wysyła.
  - **Actors:** A1, A2, A3
  - **Steps:** Silnik wznawia sesję agenta. Wiadomość do agenta to tekst operatora, bez bloku kontraktu. Agent odpowiada. Silnik czyta odpowiedź według kontraktu kroku.
  - **Outcome:** Krok kończy się labelem tak samo jak dotąd. Wiadomość operatora jest czysta.
  - **Covered by:** R1, R2, R4

- F2. Agent po komentarzu gubi format
  - **Trigger:** Odpowiedź po wznowieniu nie kończy się poprawnym obiektem JSON.
  - **Actors:** A2, A3
  - **Steps:** Silnik uznaje odpowiedź za błąd formatu. Uruchamia naprawę formatu w tej samej sesji. Naprawa zawiera kontrakt.
  - **Outcome:** Agent dostaje kontrakt dokładnie wtedy, gdy go potrzebuje.
  - **Covered by:** R3

---

## Requirements

**Kiedy kontrakt jest doklejany**

- R1. Gdy silnik wznawia sesję agenta z komentarzem lub odpowiedzią operatora w tym samym kroku, wiadomość do agenta nie zawiera bloku kontraktu wyjścia.
- R2. Pierwszy prompt sesji nadal zawiera pełny kontrakt. Dotyczy to także ponowienia kroku w nowej sesji.
- R3. Naprawa formatu odpowiedzi (reprompt) nadal zawiera kontrakt. Tekst naprawy odwołuje się do opisu „poniżej”.
- R4. Krok, który wznawia sesję innego kroku przez parametr workflow, nadal dostaje kontrakt. Nowy krok ma inne labele, zmienne i artefakty.

**Czytanie odpowiedzi**

- R5. Silnik czyta odpowiedź po wznowieniu według kontraktu kroku, tak samo jak bez wznowienia. Zmienia się tylko treść wysyłanego promptu.

**Widoczność**

- R6. Rozmowa kroku w TUI i API pokazuje wiadomość operatora bez bloku kontraktu. Pierwszy prompt nadal pokazuje kontrakt.

---

## Acceptance Examples

- AE1. **Covers R1, R5.** Given krok czeka na odpowiedź operatora i ma sesję harnessu, when operator odpowiada „zrób wariant B”, then agent dostaje tekst „zrób wariant B” bez bloku kontraktu. Odpowiedź z labelem kończy krok.
- AE2. **Covers R2.** Given krok startuje po raz pierwszy, when silnik buduje prompt, then prompt kończy się pełnym kontraktem.
- AE3. **Covers R3.** Given agent po komentarzu odpowiada bez obiektu JSON, when silnik uruchamia naprawę formatu, then wiadomość naprawy zawiera pełny kontrakt.
- AE4. **Covers R4.** Given workflow ustawia `resume_session` na sesję wcześniejszego kroku, when nowy krok startuje, then prompt zawiera kontrakt nowego kroku.

---

## Success Criteria

- Operator widzi w rozmowie kroku swoje słowa bez doklejonego bloku technicznego.
- Liczba kroków kończących się błędem formatu po wznowieniu nie rośnie w porównaniu z dzisiejszą. Zapas to naprawa formatu z kontraktem.
- Planista nie musi wymyślać, kiedy kontrakt jest doklejany: reguły to R1–R4.

---

## Scope Boundaries

- Nie zmieniamy treści ani długości samego kontraktu.
- Nie zmieniamy naprawy formatu poza gwarancją, że zawiera kontrakt.
- Nie zmieniamy sposobu wznawiania sesji ani zapisu sesji.
- Nie zmieniamy profilu, dyrygenta ani kroku `profile`.

---

## Key Decisions

- **Reguła „kontrakt raz na sesję” dotyczy tylko powrotu z komentarzem w tym samym kroku.** Naprawa formatu i wznowienie sesji z innego kroku mają własne potrzeby i zostają bez zmian.
- **Reguła dotyczy wszystkich harnessów.** Zapasem jest naprawa formatu z kontraktem. To założenie, nie potwierdzony fakt: patrz Outstanding Questions.

---

## Dependencies / Assumptions

- Założenie: sesja harnessu zachowuje pierwszy prompt w kontekście, więc agent zna kontrakt. Przy bardzo długiej sesji kontekst może zostać skrócony. Nie sprawdzono tego w kodzie ani w praktyce.
- Założenie: adapter Claude Code przekazuje kontrakt także jako `--json-schema` (`crates/narsor-engine/src/engine/actions/harness/claude_code.rs`). Odpowiedź jest wtedy ustrukturyzowana nawet bez tekstu kontraktu. Warunek użycia flagi trzeba sprawdzić.
- Założenie: brak pytania do operatora. Wybrano ostrożny wariant: kontrakt zostaje wszędzie, gdzie agent mógłby go nie znać (R2–R4).

---

## Outstanding Questions

### Resolve Before Planning

Brak.

### Deferred to Planning

- [Affects R1][Technical] Jak silnik rozróżnia powrót z komentarzem (`continue_session` z odpowiedzi operatora) od naprawy formatu i od `resume_session` workflow, skoro wszystkie trzy używają tego samego parametru sesji?
- [Affects R1][Needs research] Czy harnessy inne niż Claude Code (codex, opencode, copilot) trzymają pierwszy prompt na tyle pewnie, by pominąć kontrakt? Jeśli nie, planista może ograniczyć R1 do harnessów z ustrukturyzowanym wynikiem.
- [Affects R6][Technical] Czy rozmowa w TUI i API składa wiadomość z promptu wysłanego do agenta, czy z osobnego źródła? Od tego zależy, czy R6 wymaga osobnej zmiany.
