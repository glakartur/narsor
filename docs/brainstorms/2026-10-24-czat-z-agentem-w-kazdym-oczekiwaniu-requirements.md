---
date: 2026-10-24
topic: czat-z-agentem-w-kazdym-oczekiwaniu
---

# Wiadomość do agenta w każdym oczekiwaniu i w pauzie, niezależnie od workflow

## Summary

Gdy krok agentowy czeka na odpowiedź operatora albo jego praca jest wstrzymana, operator może w każdej chwili wysłać do aktywnej sesji agenta zwykłą wiadomość. Ta możliwość jest stałą cechą TUI i backendu. Definicja workflow jej nie opisuje i nie może jej wyłączyć. W TUI pojawia się stała akcja nr 0 „Napisz do agenta". Po jej wybraniu TUI pokazuje jedno pole na treść i wysyła ją do aktywnej sesji.

---

## Problem Frame

Dziś możliwość napisania do agenta zależy od tego, co zdefiniował workflow i od rodzaju oczekiwania. Pole wiadomości (`composer_spec` w `src/tui/input_form.rs`) istnieje tylko dla dwóch rodzajów oczekiwania: pytania agenta (`InputKind::AgentQuestion`, plan `Answer`) i problemu (`InputKind::Problem`, plan `Message`). Panel zatwierdzenia z gate'a, panel operatora (`OperatorPanel`), panel właściciela (`OwnerPanel`) i pauza nie mają pola wiadomości. Jeśli workflow nie przewidział etykiety dla wiadomości, operator może tylko wybrać jedną z gotowych opcji.

Operator widzi wtedy, że agent idzie w złą stronę albo że brakuje mu kontekstu. Nie ma jak napisać „zrób inaczej" bez wybrania przejścia workflow. Nie ma też wyjścia, gdy opcje nie pasują do sytuacji. Musi przerwać task albo zmienić definicję workflow.

Ten dokument jest uzupełnieniem `docs/brainstorms/2026-10-22-akcje-taska-z-backendu-i-komentarz-w-pauzie-requirements.md`. Tamten dokument opisuje komentarz jako akcję dostarczaną przez backend w stanach zatrzymania (R5–R10). Ten dokument dodaje wymagania dla warstwy TUI i gwarancję „niezależnie od workflow".

---

## Actors

- A1. Operator: osoba, która obsługuje task w TUI i chce napisać do agenta.
- A2. Agent: harness, który prowadzi aktywną sesję.
- A3. Backend: przyjmuje wiadomość i dostarcza ją do aktywnej sesji.
- A4. Klient (TUI): pokazuje akcję i pole na treść.

---

## Key Flows

- F1. Wiadomość podczas oczekiwania na odpowiedź
  - **Trigger:** Krok agentowy czeka na odpowiedź operatora (pytanie, zatwierdzenie, uprawnienie, problem).
  - **Actors:** A1, A2, A3, A4
  - **Steps:** Operator otwiera panel oczekiwania. Na liście akcji widzi na pozycji 0 „Napisz do agenta". Wybiera ją. TUI pokazuje jedno pole na treść. Operator wpisuje tekst i zatwierdza. TUI wysyła treść do aktywnej sesji agenta. Wiadomość pojawia się w rozmowie taska.
  - **Outcome:** Agent dostał tekst operatora w tej samej sesji. Operator nie wybrał żadnej opcji workflow.
  - **Covered by:** R1, R2, R3, R4, R5, R7

- F2. Wiadomość do wstrzymanego taska
  - **Trigger:** Praca agenta jest zapauzowana przez operatora.
  - **Actors:** A1, A2, A3, A4
  - **Steps:** Operator otwiera task. Akcja 0 „Napisz do agenta" jest dostępna. Operator wysyła treść jak w F1.
  - **Outcome:** Agent dostał wiadomość. Skutek dla stanu pauzy opisuje R6.
  - **Covered by:** R1, R2, R6

---

## Requirements

**Dostępność**

- R1. Gdy task ma aktywną sesję agenta i stoi w stanie oczekiwania na odpowiedź operatora albo w pauzie, akcja „Napisz do agenta" jest dostępna. Dotyczy to każdego rodzaju oczekiwania, także innego niż agentowe (np. awaria providera), o ile istnieje agent, któremu można wysłać komunikat, i także takiego, dla którego workflow nie zdefiniował żadnej etykiety ani pola wiadomości.
- R2. Dostępność akcji nie zależy od definicji workflow. Workflow nie musi jej deklarować i nie może jej ukryć ani wyłączyć. Schemat workflow (`WORKFLOW_SCHEMA.md`) nie zyskuje nowego pola dla tej akcji.
- R3. Gdy task nie ma aktywnej sesji agenta (np. zakończony, w kolejce, krok nie jest agentowy), akcja jest niedostępna. TUI pokazuje jej powód niedostępności, a nie ukrywa jej.

**Akcja w TUI**

- R4. Akcja „Napisz do agenta" ma stałą pozycję 0 na liście akcji panelu oczekiwania. Pozostałe akcje zachowują swoją kolejność za nią. Numery skrótów nie są nigdzie zapisane ani używane poza listą, więc numeracja pozostałych akcji może się przesunąć (Z6).
- R5. Wybranie akcji pokazuje jedno pole na treść. Zatwierdzenie wysyła treść do aktywnej sesji agenta. Pusta treść nie jest wysyłana i TUI mówi, dlaczego.
- R6. Wiadomość nie wybiera opcji workflow i nie zmienia przejścia. Skutek dla stanu taska zależy od stanu zatrzymania i jest zgodny z R6, R9 i R10 z dokumentu `2026-10-22-akcje-taska-z-backendu-i-komentarz-w-pauzie`: w pauzie wiadomość wznawia agenta, przy zatwierdzeniu i zgodzie na uprawnienie jest odmową z uwagą, przy pytaniu jest odpowiedzią. Wiadomość nigdy nie udziela zgody.
- R7. Wysłana wiadomość jest widoczna w rozmowie taska jako wypowiedź operatora. Po wysłaniu pole wraca do stanu pustego, a panel pokazuje, że wysłano (jak dziś robi to istniejące pole wiadomości).
- R8. Gdy wysłanie się nie uda (sesja zakończyła się, backend odrzucił), TUI pokazuje powód i zachowuje wpisany tekst. Operator nie musi pisać go od nowa.

**Zgodność z istniejącym zachowaniem**

- R9. Istniejące pola odpowiedzi (odpowiedź na pytanie agenta, wiadomość w problemie) działają jak dziś. Akcja 0 nie zastępuje ich. Dla rodzajów oczekiwania, które mają własne pole, akcja 0 nie duplikuje go w sposób mylący — zob. Założenie Z3.
- R10. Dostępność akcji jest zawsze oceniana po stronie backendu (zgodnie z dokumentem `2026-10-22-akcje-taska-z-backendu...`, R1–R4). TUI nie zgaduje jej z rodzaju oczekiwania.

---

## Acceptance Examples

- AE1. **Covers R1, R2.** Dane: krok agentowy czeka na zatwierdzenie z gate'a, a workflow nie definiuje żadnego pola wiadomości. Gdy operator otwiera panel, na pozycji 0 jest „Napisz do agenta".
- AE2. **Covers R4, R5, R7.** Dane: agent czeka na odpowiedź. Gdy operator wybiera akcję 0, wpisuje „użyj innej biblioteki" i zatwierdza, agent dostaje ten tekst w tej samej sesji, a rozmowa pokazuje wypowiedź operatora.
- AE3. **Covers R3.** Dane: task jest zakończony. Akcja 0 jest widoczna jako niedostępna z powodem „brak aktywnej sesji agenta".
- AE4. **Covers R5.** Dane: operator wybrał akcję 0 i zatwierdził puste pole. Nic nie zostaje wysłane, a TUI pokazuje powód.
- AE5. **Covers R8.** Dane: sesja agenta zakończyła się w chwili wysyłania. Operator widzi powód odrzucenia i nadal ma wpisany tekst w polu.
- AE6. **Covers R6.** Dane: operator wstrzymał task. Gdy wysyła wiadomość akcją 0, agent rusza dalej z tą wiadomością, a stan taska przestaje być „wstrzymany".

---

## Success Criteria

- Operator może w każdym oczekiwaniu i w pauzie z aktywną sesją napisać do agenta, bez zaglądania do definicji workflow i bez zmiany workflow.
- Dodanie nowego workflow lub nowego rodzaju oczekiwania nie wymaga żadnej pracy, by ta możliwość działała.
- Planista nie musi wymyślać, kiedy akcja jest dostępna ani co się dzieje po wysłaniu. Opisują to R1–R8.

---

## Scope Boundaries

- Brak rozmowy z dyrygentem. Czat dyrygenta (`narsor conductor chat`) jest osobną funkcją i nie zmienia się.
- Brak załączników, edycji wysłanej wiadomości i wątków.
- Brak wysyłania wiadomości do tasku, który nie ma aktywnej sesji agenta.
- Brak nowych uprawnień i ról. Kto może napisać do agenta, określa dotychczasowa autoryzacja.
- Opcja przełączania Esc między formularzem odpowiedzi a formularzem czatu nie wchodzi do tego zakresu (zob. Key Decisions i Z1). Decyzja użytkownika: wybrano tylko akcję 0.
- Zwykła rozmowa z agentem, która nie wynika z pytania agenta, nie jest częścią tego zakresu. Użytkownik planuje ją w przyszłości, więc formularz czatu ma być samodzielnym elementem, który da się użyć poza oczekiwaniem (Z7).

---

## Key Decisions

- Wybrano wariant „stała akcja nr 0" zamiast przełączania Esc. Dlaczego: Esc ma dziś ustalone znaczenie. Wewnątrz pól formularza wychodzi z pól (`leave_fields`), a na liście akcji zwija panel (`ChooserAction::Cancel`). Przełączanie formularzy tym samym klawiszem zmienia to znaczenie i wymaga zmiany `src/tui/keymap.rs`. Stała akcja używa tego samego wzorca co pozostałe akcje i nie koliduje z niczym. Dodatkowo jest bardziej przyjazna dla użytkownika i tańsza w realizacji. Decyzja użytkownika. Założenie Z1.
- Możliwość jest cechą platformy, nie workflow. Dlaczego: operator potrzebuje jej właśnie wtedy, gdy workflow czegoś nie przewidział.

---

## Dependencies / Assumptions

- Zweryfikowane w kodzie: pole wiadomości jest dziś tylko dla `InputKind::AgentQuestion` i `InputKind::Problem` (`composer_spec` w `src/tui/input_form.rs`). Panele `OperatorPanel` i `OwnerPanel` w `src/tui/task_app/waiting.rs` nie mają pola wiadomości.
- Zweryfikowane w kodzie: w polach formularza Esc robi `leave_fields`; poza polami Esc zwija panel (`src/tui/task_app/waiting.rs`).
- Zależność: lista akcji z backendu (dokument `2026-10-22-akcje-taska-z-backendu...`). Jeśli ta funkcja nie powstała, dostępność akcji 0 trzeba chwilowo ocenić po stronie klienta, co jest sprzeczne z R10. Do uzgodnienia w planowaniu.
- Niezweryfikowane: czy backend ma dziś jedną ścieżkę dostarczenia wiadomości do aktywnej sesji we wszystkich stanach z R1, w tym w pauzie i przy oczekiwaniu bez pola wiadomości.

---

## Założenia do potwierdzenia

- Z1. Z dwóch wariantów z opisu zadania użytkownik wybrał stałą akcję nr 0. Alternatywa (przełączanie Esc między formularzem odpowiedzi a formularzem czatu) nie jest realizowana, bo Esc ma już znaczenie „wyjdź".
- Z2. „Aktywna sesja agenta" to sesja harnessu uruchomiona dla bieżącego kroku taska. Task bez takiej sesji (zakończony, w kolejce, krok nie jest agentowy) nie ma akcji.
- Z3. Dla pytania agenta i problemu, które mają już pole wiadomości, akcja 0 otwiera to samo pole, a nie drugie. Operator widzi jedną drogę do napisania do agenta.
- Z4. Wiadomość jest zwykłym tekstem. Wielolinijkowy zapis działa jak w istniejących polach (Enter wysyła, Ctrl+J nowa linia).
- Z5. Skutek wysłania w każdym stanie jest taki, jak w dokumencie `2026-10-22-akcje-taska-z-backendu...` (R6, R9, R10). Ten dokument nie wprowadza innych znaczeń.

- Z6. Numery skrótów akcji (1, 2, 3…) nie są nigdzie zapisane ani używane poza listą akcji. Wstawienie akcji 0 może więc przesunąć numerację pozostałych akcji. Decyzja użytkownika.
- Z7. Akcja 0 jest dostępna zawsze, gdy istnieje agent, któremu można wysłać komunikat. Dotyczy to także oczekiwań innych niż agentowe (np. awaria providera). Formularz czatu ma być samodzielnym elementem, bo w przyszłości będzie potrzebna zwykła rozmowa z agentem, niezależna od pytania agenta. Decyzja użytkownika.

## Pytania otwarte dla użytkownika

Brak. Pytania P1–P3 użytkownik rozstrzygnął w przeglądzie (Z1, Z6, Z7).

## Outstanding Questions

### Resolve Before Planning

Brak. Decyzje użytkownika zapisano jako Z1, Z6 i Z7.

### Deferred to Planning

- [Affects R1, R10][Technical] Czy lista akcji z backendu już istnieje, czy akcja 0 wymaga tymczasowej oceny po stronie klienta.
- [Affects R6][Needs research] Jak backend dostarcza wiadomość do sesji agenta w stanach bez pola wiadomości i czy każdy harness (Claude Code, Codex, opencode) to obsługuje.
- [Affects R4][Technical] Jak wstawić akcję 0 w `src/tui/task_app/waiting.rs` i `src/tui/keymap.rs`. Zmiana numeracji skrótów jest dozwolona (Z6).
